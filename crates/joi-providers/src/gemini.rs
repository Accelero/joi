//! `[M2]` [`GeminiAdapter`] — Gemini Live native audio over `adk-realtime` (SPEC §4.3, §4.5).
//!
//! The realtime SDK is an implementation detail confined to this module (see `NOTES-adk.md` for the
//! spike that pinned its API). We use `adk-realtime`'s **low-level** `RealtimeSession` (not the
//! callback `RealtimeRunner`): its `&self` sends + `next_event()` let us pump the provider's events
//! into Joi's owned [`EventReceiver`], so nothing about adk leaks past [`RealtimeSession`].
//!
//! The API key is injected at construction (the factory reads it from `config.live_api.gemini`) and
//! held as a [`secrecy::SecretString`] — it never travels through [`SessionConfig`].

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use adk_realtime::audio::AudioChunk;
use adk_realtime::gemini::{GeminiLiveBackend, GeminiRealtimeModel};
use adk_realtime::session::RealtimeSession as AdkSession;
use adk_realtime::{RealtimeConfig, RealtimeError, RealtimeModel, ServerEvent};

use joi_core::error::SessionError;
use joi_core::media::{self, VideoFrame};
use joi_core::metrics::TransportBytes;
use joi_core::session::event::{
    CloseReason, EventReceiver, EventSender, SessionEvent, Speaker, TurnEvent,
};
use joi_core::session::{Capabilities, RealtimeSession, SessionConfig};

/// Buffer for the pump→manager event channel. Matches the manager's internal media channel.
const EVENT_CHANNEL: usize = 512;

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
        let model = GeminiRealtimeModel::new(backend, cfg.model);

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

        let boxed = model.connect(rc).await.map_err(|e| map_connect_err(&e))?;
        // adk hands back a `Box<dyn RealtimeSession>`; share it as an `Arc` so the pump task and the
        // `send_*` calls (all `&self`) can hold it concurrently without aliasing `&mut`.
        let session: Arc<dyn AdkSession> = Arc::from(boxed);

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
        // Honest MVP flags: resumption is wired in M3, native screen in M4, tools are POST.
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
}

impl EventMapper {
    fn map(&mut self, event: ServerEvent) -> Vec<SessionEvent> {
        match event {
            ServerEvent::AudioDelta { delta, .. } => vec![SessionEvent::AudioOutput {
                pcm: media::le_bytes_to_pcm16(&delta),
            }],
            ServerEvent::TranscriptDelta { delta, .. } | ServerEvent::TextDelta { delta, .. } => {
                self.agent_open = true;
                vec![agent_delta(delta, false)]
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
                let mut out = self.close_line();
                out.push(SessionEvent::TurnEvent(TurnEvent::Interrupted));
                out
            }
            ServerEvent::ResponseCreated { .. } => {
                let mut out = self.close_line();
                out.push(SessionEvent::TurnEvent(TurnEvent::TurnStarted));
                out
            }
            // Gemini signals turn end with no transcript-done; commit the open line here.
            ServerEvent::ResponseDone { .. } => {
                let mut out = self.close_line();
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
            // SessionCreated, AudioDone, item/buffer bookkeeping, tool calls (POST), rate limits,
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
}

/// One incremental agent transcript event (the manager accumulates the line; the UI appends).
fn agent_delta(text: String, final_: bool) -> SessionEvent {
    SessionEvent::Transcript {
        speaker: Speaker::Agent,
        text,
        final_,
    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use adk_realtime::events::ErrorInfo;

    fn adapter() -> GeminiAdapter {
        GeminiAdapter::new(SecretString::from("test-key"))
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
