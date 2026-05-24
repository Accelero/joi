//! [`GeminiAdapter`] — Gemini Live native audio over the vendored `adk-realtime` (PLAN §7.4, M2).
//!
//! The realtime SDK is an implementation detail confined to this module (see `NOTES-adk.md` for the
//! spike that pinned its API). We use `adk-realtime`'s **low-level** `RealtimeSession` (not the
//! callback `RealtimeRunner`): its `&self` sends + `next_event()` let us pump the provider's events
//! into Joi's owned [`EventReceiver`], so nothing about adk leaks past [`RealtimeSession`].
//!
//! The API key is injected at construction (the factory reads it from `config.live_api.gemini`) and
//! held as a [`secrecy::SecretString`] — it never travels through [`SessionConfig`] (SEC-1).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use adk_realtime::audio::AudioChunk;
use adk_realtime::gemini::{GeminiLiveBackend, GeminiRealtimeModel};
use adk_realtime::session::RealtimeSession as AdkSession;
use adk_realtime::{RealtimeConfig, RealtimeError, RealtimeModel, ServerEvent};

use joi_core::connectivity::{ConnectivityProbe, ProbeOutcome};
use joi_core::error::SessionError;
use joi_core::history::{HistoryTurn, Role};
use joi_core::media::{self, VideoFrame};
use joi_core::metrics::{TokenUsage, TransportBytes};
use joi_core::session::event::{
    CloseReason, EventReceiver, EventSender, Reachability, SessionEvent, Speaker, TurnEvent,
};
use joi_core::session::{Capabilities, RealtimeSession, SessionConfig};

/// Buffer for the pump→manager event channel. Matches the manager's internal media channel.
const EVENT_CHANNEL: usize = 512;

/// Prebuilt voices on the **legacy half-cascade** Live models (Gemini 2.0 / non-native-audio 2.5) —
/// the 8-voice subset.
const HALF_CASCADE_VOICES: &[&str] = &[
    "Puck", "Charon", "Kore", "Fenrir", "Aoede", "Leda", "Orus", "Zephyr",
];

/// Prebuilt voices on **native-audio** / current Live models (gemini-3.x flash-live, *native-audio*)
/// — the full documented set of 30. (Not queryable from the API; sourced from Google's docs.)
const NATIVE_AUDIO_VOICES: &[&str] = &[
    "Zephyr",
    "Puck",
    "Charon",
    "Kore",
    "Fenrir",
    "Leda",
    "Orus",
    "Aoede",
    "Callirrhoe",
    "Autonoe",
    "Enceladus",
    "Iapetus",
    "Umbriel",
    "Algieba",
    "Despina",
    "Erinome",
    "Algenib",
    "Rasalgethi",
    "Laomedeia",
    "Achernar",
    "Alnilam",
    "Schedar",
    "Gacrux",
    "Pulcherrima",
    "Achird",
    "Zubenelgenubi",
    "Vindemiatrix",
    "Sadachbia",
    "Sadaltager",
    "Sulafat",
];

/// The prebuilt voices a Gemini `model` accepts (provider-sealed; **not** queryable from the API).
///
/// Native-audio and current models (gemini-3.x) expose the full 30; the legacy half-cascade Live
/// models (2.0 / non-native-audio 2.5) expose only the 8-voice subset. Unknown/newer model ids
/// default to the full set — the adapter falls back to the model default server-side for any voice a
/// model doesn't accept, so erring toward the larger set is safe.
#[must_use]
pub fn voices(model: &str) -> Vec<String> {
    let list = if is_legacy_half_cascade(model) {
        HALF_CASCADE_VOICES
    } else {
        NATIVE_AUDIO_VOICES
    };
    list.iter().map(|s| (*s).to_string()).collect()
}

/// Whether `model` is a legacy half-cascade Live model (8 voices). Native-audio models — including
/// everything with `native-audio` in the id and the gemini-3.x flash-live family — are not.
fn is_legacy_half_cascade(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    !m.contains("native-audio")
        && (m.contains("2.0-flash-live")
            || m.contains("2.5-flash-live")
            || m.contains("live-2.5-flash"))
}

/// Install the rustls crypto provider that `adk-realtime` requires. Idempotent; safe to call from
/// the composition root at startup and again here (the first install wins).
pub fn init_crypto() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Gemini Live adapter. Construct with the API key, then [`RealtimeSession::connect`].
pub struct GeminiAdapter {
    api_key: SecretString,
    session: Option<Arc<dyn AdkSession>>,
    events: Option<EventReceiver>,
    pump: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for GeminiAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the key.
        f.debug_struct("GeminiAdapter")
            .field("connected", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

impl GeminiAdapter {
    /// Build an unconnected adapter bound to `api_key`.
    #[must_use]
    pub fn new(api_key: SecretString) -> Self {
        Self {
            api_key,
            session: None,
            events: None,
            pump: None,
        }
    }
}

#[async_trait]
impl RealtimeSession for GeminiAdapter {
    async fn connect(&mut self, cfg: SessionConfig) -> Result<(), SessionError> {
        init_crypto();

        let backend = GeminiLiveBackend::studio(self.api_key.expose_secret().to_string());
        let model = GeminiRealtimeModel::new(backend, model_resource_name(&cfg.model));

        let mut rc = RealtimeConfig::default()
            .with_instruction(cfg.system_instruction)
            .with_audio_only()
            .with_server_vad();
        if let Some(voice) = cfg.voice {
            rc = rc.with_voice(voice);
        }
        if cfg.enable_input_transcription {
            rc = rc.with_transcription();
        }
        if cfg.enable_output_transcription {
            // Gemini transcribes the model's spoken reply to text → streamed to the terminal.
            rc = rc.with_output_transcription();
        }
        if cfg.context_window_compression {
            // Enable the server-side sliding-window compression so the session isn't capped at the
            // default duration limits (15 min audio / 2 min audio+video). `slidingWindow: {}` uses
            // Gemini's defaults (compress at ~80% of the model's context window). The vendored
            // adk-realtime PATCH(joi) reads this off `extra` and emits it as the top-level
            // `contextWindowCompression` setup field. We own `extra` here (nothing else sets it).
            rc.extra = Some(serde_json::json!({
                "contextWindowCompression": { "slidingWindow": {} }
            }));
        }

        let boxed = model.connect(rc).await.map_err(|e| map_connect_err(&e))?;
        // adk hands back a `Box<dyn RealtimeSession>`; share it as an `Arc` so the pump task and the
        // `send_*` calls (all `&self`) can hold it concurrently without aliasing `&mut`.
        let session: Arc<dyn AdkSession> = Arc::from(boxed);

        // Seed the prior conversation as context (turnComplete=false) so this fresh connection
        // continues the same chat without replaying old turns as new prompts. An empty log (first
        // run) seeds nothing. A seed failure is non-fatal — the session works, just without memory.
        //
        // The Live protocol rejects any `clientContent` sent before the server acknowledges setup
        // ("Request contains an invalid argument"), and adk's `connect` returns *without* awaiting
        // that ack — so wait for `setupComplete` before seeding. The first start has an empty seed,
        // which is why this only ever bit a resume.
        let seed = history_to_seed_turns(&cfg.initial_context);
        if !seed.is_empty() {
            wait_for_setup(&session).await;
            match session.send_history(&seed).await {
                Ok(()) => tracing::info!(turns = seed.len(), "seeded prior conversation"),
                Err(e) => tracing::warn!(error = %e, turns = seed.len(), "failed to seed history"),
            }
        }

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL);
        let pump = tokio::spawn(pump_events(Arc::clone(&session), tx));

        self.session = Some(session);
        self.events = Some(rx);
        self.pump = Some(pump);
        Ok(())
    }

    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SessionError> {
        let session = self.session.as_ref().ok_or(SessionError::NotConnected)?;
        let chunk = AudioChunk::pcm16_16khz(media::pcm16_to_le_bytes(pcm));
        session
            .send_audio(&chunk)
            .await
            .map_err(|e| SessionError::Send(e.to_string()))
    }

    async fn send_video_frame(&mut self, frame: &VideoFrame) -> Result<(), SessionError> {
        let session = self.session.as_ref().ok_or(SessionError::NotConnected)?;
        // Screen frames are JPEG (joi-media); Gemini takes them as a realtimeInput.video blob.
        session
            .send_video_jpeg(&frame.data)
            .await
            .map_err(|e| SessionError::Send(e.to_string()))
    }

    async fn end_audio_stream(&mut self) -> Result<(), SessionError> {
        let session = self.session.as_ref().ok_or(SessionError::NotConnected)?;
        session
            .send_audio_stream_end()
            .await
            .map_err(|e| SessionError::Send(e.to_string()))
    }

    async fn send_text(&mut self, text: &str) -> Result<(), SessionError> {
        let session = self.session.as_ref().ok_or(SessionError::NotConnected)?;
        session
            .send_text(text)
            .await
            .map_err(|e| SessionError::Send(e.to_string()))?;
        // With server VAD the model auto-responds to audio, but typed text needs an explicit trigger.
        session
            .create_response()
            .await
            .map_err(|e| SessionError::Send(e.to_string()))
    }

    fn take_events(&mut self) -> EventReceiver {
        self.events.take().unwrap_or_else(|| {
            // Called before connect or twice: hand back an already-empty receiver.
            let (_tx, rx) = mpsc::channel(1);
            rx
        })
    }

    async fn close(&mut self) -> Result<(), SessionError> {
        if let Some(session) = self.session.take() {
            let _ = session.close().await;
        }
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        self.events = None;
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        // Honest MVP flags: resumption, native screen input, and tool calls are all future work.
        Capabilities {
            session_resumption: false,
            native_screen_input: false,
            async_tool_calls: false,
        }
    }

    fn transport_bytes(&self) -> Option<TransportBytes> {
        let (sent, received) = self.session.as_ref()?.transport_bytes()?;
        Some(TransportBytes { sent, received })
    }

    fn token_usage(&self) -> Option<TokenUsage> {
        let (up, down) = self.session.as_ref()?.token_usage()?;
        Some(TokenUsage { up, down })
    }
}

/// Block until the server acknowledges setup (`setupComplete`, surfaced by adk as
/// [`ServerEvent::SessionCreated`]) or a short timeout elapses. The Live API rejects any
/// `clientContent` (e.g. the history seed) sent before setup is acknowledged, and adk's `connect`
/// does not await it. Events consumed here precede `setupComplete` and carry nothing the UI needs,
/// so discarding them is safe; the pump reads everything after.
async fn wait_for_setup(session: &Arc<dyn AdkSession>) {
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = session.next_event().await {
            if matches!(result, Ok(ServerEvent::SessionCreated { .. })) {
                break;
            }
        }
    })
    .await;
}

/// Forward adk `ServerEvent`s into Joi's owned event stream until the session closes or the manager
/// drops its receiver.
async fn pump_events(session: Arc<dyn AdkSession>, tx: EventSender) {
    let mut mapper = EventMapper::default();
    while let Some(result) = session.next_event().await {
        let session_events = match result {
            Ok(event) => mapper.map(event),
            Err(e) => vec![SessionEvent::Error(SessionError::Provider(e.to_string()))],
        };
        for ev in session_events {
            if tx.send(ev).await.is_err() {
                return; // manager gone
            }
        }
    }
    let _ = tx
        .send(SessionEvent::Closed {
            reason: CloseReason::Server,
        })
        .await;
}

/// Maps adk `ServerEvent`s to Joi [`SessionEvent`]s (NOTES-adk.md mapping table). The agent's
/// output transcript is streamed **incrementally**: each provider delta becomes one
/// `Transcript { final_: false }` carrying only that delta (the manager accumulates the line for
/// history; the terminal appends). A turn boundary (turn-complete / barge-in) emits a final,
/// empty-text `Transcript { final_: true }` to commit the line.
#[derive(Default)]
struct EventMapper {
    /// Whether an agent transcript line is open (deltas streamed, not yet committed this turn).
    agent_open: bool,
    /// Whether a user (input) transcript line is open. Committed when the model begins replying or
    /// the turn ends, so the user's spoken words land in history as one [`Speaker::User`] turn.
    user_open: bool,
}

impl EventMapper {
    fn map(&mut self, event: ServerEvent) -> Vec<SessionEvent> {
        match event {
            // Model audio means the user's turn is over: commit their transcript before playback.
            ServerEvent::AudioDelta { delta, .. } => {
                let mut out = self.close_user_line();
                out.push(SessionEvent::AudioOutput {
                    pcm: media::le_bytes_to_pcm16(&delta),
                });
                out
            }
            // The user's own spoken words (Gemini inputTranscription) — stream as an open user line.
            ServerEvent::InputTranscriptDelta { delta, .. } => {
                self.user_open = true;
                vec![user_delta(delta, false)]
            }
            ServerEvent::TranscriptDelta { delta, .. } | ServerEvent::TextDelta { delta, .. } => {
                // The model is now replying → the user's input line is complete; commit it first.
                let mut out = self.close_user_line();
                self.agent_open = true;
                out.push(agent_delta(delta, false));
                out
            }
            // A provider that sends an explicit done (OpenAI-style) carries the full text. If we
            // already streamed deltas, they cover it → just commit; otherwise emit it as the line.
            ServerEvent::TranscriptDone {
                transcript: text, ..
            }
            | ServerEvent::TextDone { text, .. } => {
                let line = agent_delta(if self.agent_open { String::new() } else { text }, true);
                self.agent_open = false;
                vec![line]
            }
            // Server VAD detected the user speaking → commit any open line and flush agent
            // playback (barge-in, FR-2).
            ServerEvent::SpeechStarted { .. } => {
                tracing::debug!("gemini barge-in: model response interrupted by user speech");
                let mut out = self.close_lines();
                out.push(SessionEvent::TurnEvent(TurnEvent::Interrupted));
                out
            }
            ServerEvent::ResponseCreated { .. } => {
                let mut out = self.close_lines();
                out.push(SessionEvent::TurnEvent(TurnEvent::TurnStarted));
                out
            }
            // Gemini signals turn end with no transcript-done; commit the open line here.
            ServerEvent::ResponseDone { .. } => {
                let mut out = self.close_lines();
                out.push(SessionEvent::TurnEvent(TurnEvent::TurnComplete));
                out
            }
            ServerEvent::Error { error, .. } => {
                let code = error.code.unwrap_or(error.error_type);
                vec![SessionEvent::Error(SessionError::Provider(format!(
                    "{code}: {}",
                    error.message
                )))]
            }
            // SessionCreated, AudioDone, item/buffer bookkeeping, tool calls (LATER), rate limits,
            // and unknown forward-compat variants carry nothing the MVP UI needs.
            _ => vec![],
        }
    }

    /// Commit the open agent line (empty-text final) if one is open; no-op otherwise.
    fn close_line(&mut self) -> Vec<SessionEvent> {
        if !self.agent_open {
            return vec![];
        }
        self.agent_open = false;
        vec![agent_delta(String::new(), true)]
    }

    /// Commit the open user (input-transcript) line if one is open; no-op otherwise.
    fn close_user_line(&mut self) -> Vec<SessionEvent> {
        if !self.user_open {
            return vec![];
        }
        self.user_open = false;
        vec![user_delta(String::new(), true)]
    }

    /// Commit both lines, user first (it precedes the model's reply in a turn).
    fn close_lines(&mut self) -> Vec<SessionEvent> {
        let mut out = self.close_user_line();
        out.append(&mut self.close_line());
        out
    }
}

/// One incremental agent transcript event (the manager accumulates the line; the UI appends).
fn agent_delta(text: String, final_: bool) -> SessionEvent {
    SessionEvent::Transcript {
        speaker: Speaker::Agent,
        text,
        final_,
    }
}

/// One incremental user transcript event (the user's own audio, transcribed by the provider).
fn user_delta(text: String, final_: bool) -> SessionEvent {
    SessionEvent::Transcript {
        speaker: Speaker::User,
        text,
        final_,
    }
}

/// Build the conversation seed sent on (re)connect as context.
///
/// Gemini's Live `clientContent` rejects a multi-turn history that carries `model`-role turns — it
/// closes the socket with "Request contains an invalid argument" (a single `user` turn is the only
/// shape it reliably accepts). So rather than replay turns natively, we fold the whole prior
/// conversation into **one `user` turn**: a labelled transcript the model reads as memory.
/// `send_history` sends it with `turnComplete=false`, so the model doesn't reply to it — it just
/// primes the next turn. `System` turns are dropped (that context lives in the setup instruction),
/// whitespace-only turns are skipped, and an empty/whitespace-only history seeds nothing.
fn history_to_seed_turns(turns: &[HistoryTurn]) -> Vec<(String, String)> {
    let mut lines = Vec::new();
    for turn in turns {
        let text = turn.text.trim();
        if text.is_empty() {
            continue;
        }
        // "Me"/"You" so the model reads the lines as the human's and its own prior words.
        let who = match turn.role {
            Role::User => "Me",
            Role::Assistant => "You",
            Role::System => continue,
        };
        lines.push(format!("{who}: {text}"));
    }
    if lines.is_empty() {
        return Vec::new();
    }
    let context = format!(
        "[Context: a transcript of our earlier conversation so you can continue it. Don't reply to \
         this note — just use it as memory.]\n\n{}",
        lines.join("\n")
    );
    vec![("user".to_string(), context)]
}

/// Build the wire resource name for the Live (`bidiGenerateContent`) endpoint, which addresses
/// models as `models/<id>`. Config carries the **bare** model name (`gemini-3.1-flash-live-preview`)
/// — `Config::validate` rejects a `models/` prefix — so this only qualifies it; it performs no
/// lenient adaptation of the input.
fn model_resource_name(model: &str) -> String {
    format!("models/{}", model.trim())
}

/// Classify a connect-time error so the UI can tell "bad key" from "network down".
fn map_connect_err(e: &RealtimeError) -> SessionError {
    let msg = e.to_string();
    let low = msg.to_lowercase();
    if [
        "unauthor",
        "api key",
        "permission",
        "401",
        "403",
        "invalid_argument key",
    ]
    .iter()
    .any(|needle| low.contains(needle))
    {
        SessionError::Auth(msg)
    } else {
        SessionError::Connect(msg)
    }
}

/// Token-free reachability probe for the Gemini API. Hits the **`models.list`** metadata endpoint
/// (`GET /v1beta/models`) — a management call that returns no generated content, so it **never
/// consumes tokens**. The result distinguishes reachable+authorized, reachable-but-bad-key, and
/// unreachable. The HTTP details live here, in the Gemini connector; the engine drives it through
/// the provider-agnostic [`ConnectivityProbe`] trait.
pub struct GeminiProbe {
    client: reqwest::Client,
    /// `{base}/v1beta/models` — the studio REST host (same host as the Live WS endpoint).
    url: String,
    api_key: SecretString,
}

impl GeminiProbe {
    /// Studio REST base; matches `GeminiLiveBackend::studio` so the probe checks the same host the
    /// session connects to.
    const BASE: &'static str = "https://generativelanguage.googleapis.com";
    /// Keep the probe snappy so the monitor's cadence stays predictable on a flaky network.
    const TIMEOUT: Duration = Duration::from_secs(4);

    /// Build a probe bound to `api_key`. The HTTP client is reused across probes.
    #[must_use]
    pub fn new(api_key: SecretString) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Self::TIMEOUT)
                .build()
                .unwrap_or_default(),
            url: format!("{}/v1beta/models", Self::BASE),
            api_key,
        }
    }
}

#[async_trait]
impl ConnectivityProbe for GeminiProbe {
    async fn probe(&self) -> ProbeOutcome {
        let resp = self
            .client
            .get(&self.url)
            .query(&[("key", self.api_key.expose_secret()), ("pageSize", "1")])
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => ProbeOutcome::new(Reachability::Online),
            // Reached Google, but the key was rejected — actionable, distinct from offline.
            Ok(r) if matches!(r.status().as_u16(), 401 | 403) => ProbeOutcome::with_detail(
                Reachability::Unauthorized,
                format!("HTTP {}", r.status()),
            ),
            // Reached an endpoint but it returned an error status (5xx / 429 / …): not usable now.
            Ok(r) => {
                ProbeOutcome::with_detail(Reachability::Offline, format!("HTTP {}", r.status()))
            }
            // DNS / TLS / connect / timeout: unreachable.
            Err(e) => ProbeOutcome::with_detail(Reachability::Offline, probe_err_detail(&e)),
        }
    }
}

/// A short, log-safe reason for a failed probe request (never includes the URL with its key query).
fn probe_err_detail(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else {
        "request error".to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use adk_realtime::events::ErrorInfo;

    fn adapter() -> GeminiAdapter {
        GeminiAdapter::new(SecretString::from("test-key"))
    }

    #[test]
    fn native_audio_and_current_models_get_the_full_voice_set() {
        // The user's model and any native-audio model expose all 30.
        assert_eq!(voices("gemini-3.1-flash-live-preview").len(), 30);
        assert_eq!(
            voices("gemini-2.5-flash-preview-native-audio-dialog").len(),
            30
        );
        // Unknown/newer ids default to the full set.
        assert_eq!(voices("some-future-live-model").len(), 30);
        assert!(voices("gemini-3.1-flash-live-preview").contains(&"Aoede".to_string()));
    }

    #[test]
    fn legacy_half_cascade_models_get_the_eight_voice_subset() {
        assert_eq!(voices("gemini-2.0-flash-live-001").len(), 8);
        assert_eq!(voices("gemini-live-2.5-flash-preview").len(), 8);
        // The subset is contained in the full set.
        for v in voices("gemini-2.0-flash-live-001") {
            assert!(
                NATIVE_AUDIO_VOICES.contains(&v.as_str()),
                "{v} not in full set"
            );
        }
    }

    #[tokio::test]
    async fn sends_before_connect_report_not_connected() {
        let mut a = adapter();
        assert!(matches!(
            a.send_audio(&[0i16; 320]).await,
            Err(SessionError::NotConnected)
        ));
        assert!(matches!(
            a.send_text("hi").await,
            Err(SessionError::NotConnected)
        ));
    }

    #[test]
    fn debug_never_renders_the_key() {
        let rendered = format!("{:?}", adapter());
        assert!(!rendered.contains("test-key"), "key leaked: {rendered}");
    }

    #[test]
    fn bare_model_name_is_qualified_for_the_wire() {
        // Config holds the bare name (a `models/` prefix is rejected by Config::validate); the
        // adapter qualifies it as the Live endpoint's `models/<id>` resource name.
        assert_eq!(
            model_resource_name("gemini-3.1-flash-live-preview"),
            "models/gemini-3.1-flash-live-preview"
        );
        assert_eq!(
            model_resource_name("  gemini-3.1-flash-live-preview  "),
            "models/gemini-3.1-flash-live-preview"
        );
    }

    fn transcript_delta(delta: &str) -> ServerEvent {
        ServerEvent::TranscriptDelta {
            event_id: "e".into(),
            response_id: "r".into(),
            item_id: "i".into(),
            output_index: 0,
            content_index: 0,
            delta: delta.into(),
        }
    }

    fn response_done() -> ServerEvent {
        ServerEvent::ResponseDone {
            event_id: "e".into(),
            response: serde_json::Value::Null,
        }
    }

    #[test]
    fn audio_delta_maps_to_pcm_output() {
        let pcm = vec![0i16, 1, -1, 12345, -32768];
        let event = ServerEvent::AudioDelta {
            event_id: "e".into(),
            response_id: "r".into(),
            item_id: "i".into(),
            output_index: 0,
            content_index: 0,
            delta: media::pcm16_to_le_bytes(&pcm),
        };
        match EventMapper::default().map(event).as_slice() {
            [SessionEvent::AudioOutput { pcm: out }] => assert_eq!(*out, pcm),
            other => panic!("expected one AudioOutput, got {other:?}"),
        }
    }

    #[test]
    fn speech_started_maps_to_barge_in() {
        let event = ServerEvent::SpeechStarted {
            event_id: "e".into(),
            audio_start_ms: 0,
        };
        assert!(matches!(
            EventMapper::default().map(event).as_slice(),
            [SessionEvent::TurnEvent(TurnEvent::Interrupted)]
        ));
    }

    #[test]
    fn transcript_delta_is_an_incremental_agent_line() {
        match EventMapper::default()
            .map(transcript_delta("hello"))
            .as_slice()
        {
            [SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text,
                final_: false,
            }] => assert_eq!(text, "hello"),
            other => panic!("expected incremental agent transcript, got {other:?}"),
        }
    }

    #[test]
    fn output_transcript_streams_deltas_and_commits_on_turn_end() {
        let mut m = EventMapper::default();
        // Each delta is forwarded verbatim — incrementally, not cumulatively.
        assert!(matches!(
            m.map(transcript_delta("Hel")).as_slice(),
            [SessionEvent::Transcript { text, final_: false, .. }] if text == "Hel"
        ));
        assert!(matches!(
            m.map(transcript_delta("lo there")).as_slice(),
            [SessionEvent::Transcript { text, final_: false, .. }] if text == "lo there"
        ));
        // Turn end commits the line (empty-text final — the manager accumulated it), then reports
        // the turn complete.
        match m.map(response_done()).as_slice() {
            [SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text,
                final_: true,
            }, SessionEvent::TurnEvent(TurnEvent::TurnComplete)] => assert!(text.is_empty()),
            other => panic!("expected empty final + TurnComplete, got {other:?}"),
        }
        // Line already committed: a bare turn end now emits only TurnComplete.
        assert!(matches!(
            m.map(response_done()).as_slice(),
            [SessionEvent::TurnEvent(TurnEvent::TurnComplete)]
        ));
    }

    #[test]
    fn barge_in_commits_the_open_line() {
        let mut m = EventMapper::default();
        let _ = m.map(transcript_delta("partial reply"));
        let speech = ServerEvent::SpeechStarted {
            event_id: "e".into(),
            audio_start_ms: 0,
        };
        // Open line is committed (empty-text final), then the barge-in is surfaced.
        match m.map(speech).as_slice() {
            [SessionEvent::Transcript {
                text, final_: true, ..
            }, SessionEvent::TurnEvent(TurnEvent::Interrupted)] => assert!(text.is_empty()),
            other => panic!("expected empty final + Interrupted, got {other:?}"),
        }
    }

    fn input_transcript_delta(delta: &str) -> ServerEvent {
        ServerEvent::InputTranscriptDelta {
            event_id: "e".into(),
            delta: delta.into(),
        }
    }

    #[test]
    fn input_transcript_is_a_user_line_committed_when_the_model_replies() {
        let mut m = EventMapper::default();
        // The user's spoken words stream in as an open user line.
        assert!(matches!(
            m.map(input_transcript_delta("hel")).as_slice(),
            [SessionEvent::Transcript { speaker: Speaker::User, text, final_: false }] if text == "hel"
        ));
        assert!(matches!(
            m.map(input_transcript_delta("lo")).as_slice(),
            [SessionEvent::Transcript { speaker: Speaker::User, text, final_: false }] if text == "lo"
        ));
        // When the model starts replying, the user line is committed (empty-text final) *before*
        // the first agent delta — so the manager files the words under the user, not the agent.
        match m.map(transcript_delta("hi there")).as_slice() {
            [SessionEvent::Transcript {
                speaker: Speaker::User,
                text: u,
                final_: true,
            }, SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text: a,
                final_: false,
            }] => {
                assert!(u.is_empty());
                assert_eq!(a, "hi there");
            }
            other => panic!("expected user-final then agent-delta, got {other:?}"),
        }
    }

    #[test]
    fn input_transcript_commits_on_turn_end_without_a_reply() {
        let mut m = EventMapper::default();
        let _ = m.map(input_transcript_delta("just me"));
        // A turn that ends with no model output still commits the open user line.
        match m.map(response_done()).as_slice() {
            [SessionEvent::Transcript {
                speaker: Speaker::User,
                text,
                final_: true,
            }, SessionEvent::TurnEvent(TurnEvent::TurnComplete)] => assert!(text.is_empty()),
            other => panic!("expected user-final then TurnComplete, got {other:?}"),
        }
    }

    #[test]
    fn model_audio_commits_the_open_user_line() {
        let mut m = EventMapper::default();
        let _ = m.map(input_transcript_delta("a question"));
        let audio = ServerEvent::AudioDelta {
            event_id: "e".into(),
            response_id: "r".into(),
            item_id: "i".into(),
            output_index: 0,
            content_index: 0,
            delta: media::pcm16_to_le_bytes(&[1, 2, 3]),
        };
        // Model audio implies the user finished: their line commits before playback.
        match m.map(audio).as_slice() {
            [SessionEvent::Transcript {
                speaker: Speaker::User,
                final_: true,
                ..
            }, SessionEvent::AudioOutput { .. }] => {}
            other => panic!("expected user-final then audio, got {other:?}"),
        }
    }

    #[test]
    fn history_folds_into_one_user_context_turn() {
        // The whole prior conversation becomes a single user turn (a labelled transcript) — the only
        // shape Gemini's clientContent reliably accepts. Back-to-back same-role turns and a trailing
        // model turn (both of which Gemini rejects as native turns) are harmless inside the text.
        let turns = vec![
            HistoryTurn::new(Role::User, "Hello, can you hear me?", 1),
            HistoryTurn::new(Role::User, "What was my first word?", 2),
            HistoryTurn::new(Role::Assistant, "Yes — it was Hello.", 3),
            HistoryTurn::new(Role::User, "   ", 4), // whitespace-only → skipped
            HistoryTurn::new(Role::System, "be nice", 5), // system → dropped (goes in setup)
        ];
        let seed = history_to_seed_turns(&turns);
        assert_eq!(seed.len(), 1, "exactly one (user) turn");
        let (role, text) = &seed[0];
        assert_eq!(role, "user");
        assert!(text.contains("Me: Hello, can you hear me?"), "{text}");
        assert!(text.contains("Me: What was my first word?"), "{text}");
        assert!(text.contains("You: Yes — it was Hello."), "{text}");
        assert!(!text.contains("be nice"), "system turn dropped: {text}");
    }

    #[test]
    fn empty_or_contentless_history_seeds_nothing() {
        assert!(history_to_seed_turns(&[]).is_empty());
        assert!(history_to_seed_turns(&[HistoryTurn::new(Role::System, "x", 1)]).is_empty());
        assert!(history_to_seed_turns(&[HistoryTurn::new(Role::User, "   ", 1)]).is_empty());
    }

    #[test]
    fn server_error_maps_to_provider_error() {
        let event = ServerEvent::Error {
            event_id: "e".into(),
            error: ErrorInfo {
                error_type: "internal".into(),
                code: Some("500".into()),
                message: "boom".into(),
                param: None,
            },
        };
        match EventMapper::default().map(event).as_slice() {
            [SessionEvent::Error(SessionError::Provider(msg))] => {
                assert!(msg.contains("500") && msg.contains("boom"), "{msg}");
            }
            other => panic!("expected provider error, got {other:?}"),
        }
    }
}
