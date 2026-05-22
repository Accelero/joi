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

/// Maps adk `ServerEvent`s to Joi [`SessionEvent`]s (NOTES-adk.md mapping table), accumulating the
/// agent's output-transcript deltas into the cumulative line the UI renders.
///
/// Transcript/text deltas arrive incrementally, but the manager forwards each `Transcript.text`
/// verbatim and the terminal rewrites the line in place — so we emit the **cumulative** text each
/// time and a final line at the turn boundary (which is also what gets appended to history).
#[derive(Default)]
struct EventMapper {
    /// The agent's transcript accumulated so far this turn.
    agent: String,
}

impl EventMapper {
    fn map(&mut self, event: ServerEvent) -> Vec<SessionEvent> {
        match event {
            ServerEvent::AudioDelta { delta, .. } => vec![SessionEvent::AudioOutput {
                pcm: media::le_bytes_to_pcm16(&delta),
            }],
            ServerEvent::TranscriptDelta { delta, .. } | ServerEvent::TextDelta { delta, .. } => {
                self.agent.push_str(&delta);
                vec![self.agent_line(false)]
            }
            // A provider that sends an explicit done (OpenAI-style) carries the full text.
            ServerEvent::TranscriptDone {
                transcript: text, ..
            }
            | ServerEvent::TextDone { text, .. } => {
                self.agent = text;
                let line = self.agent_line(true);
                self.agent.clear();
                vec![line]
            }
            // Server VAD detected the user speaking → commit any partial line and flush agent
            // playback (barge-in, FR-2).
            ServerEvent::SpeechStarted { .. } => {
                tracing::debug!("gemini barge-in: model response interrupted by user speech");
                let mut out = self.finalize_pending();
                out.push(SessionEvent::TurnEvent(TurnEvent::Interrupted));
                out
            }
            ServerEvent::ResponseCreated { .. } => {
                self.agent.clear();
                vec![SessionEvent::TurnEvent(TurnEvent::TurnStarted)]
            }
            // Gemini signals turn end with no transcript-done; commit the accumulated line here.
            ServerEvent::ResponseDone { .. } => {
                let mut out = self.finalize_pending();
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

    /// Emit a final agent line for the accumulated transcript if any, clearing the buffer.
    fn finalize_pending(&mut self) -> Vec<SessionEvent> {
        if self.agent.is_empty() {
            return vec![];
        }
        let line = self.agent_line(true);
        self.agent.clear();
        vec![line]
    }

    fn agent_line(&self, final_: bool) -> SessionEvent {
        SessionEvent::Transcript {
            speaker: Speaker::Agent,
            text: self.agent.clone(),
            final_,
        }
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
    fn transcript_delta_is_a_partial_agent_line() {
        match EventMapper::default()
            .map(transcript_delta("hello"))
            .as_slice()
        {
            [SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text,
                final_: false,
            }] => assert_eq!(text, "hello"),
            other => panic!("expected partial agent transcript, got {other:?}"),
        }
    }

    #[test]
    fn output_transcript_deltas_accumulate_and_finalize_on_turn_end() {
        let mut m = EventMapper::default();
        // Deltas are incremental; each emitted partial carries the cumulative text.
        assert!(matches!(
            m.map(transcript_delta("Hel")).as_slice(),
            [SessionEvent::Transcript { text, final_: false, .. }] if text == "Hel"
        ));
        assert!(matches!(
            m.map(transcript_delta("lo there")).as_slice(),
            [SessionEvent::Transcript { text, final_: false, .. }] if text == "Hello there"
        ));
        // Turn end commits the full line, then reports the turn complete.
        match m.map(response_done()).as_slice() {
            [SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text,
                final_: true,
            }, SessionEvent::TurnEvent(TurnEvent::TurnComplete)] => assert_eq!(text, "Hello there"),
            other => panic!("expected final line + TurnComplete, got {other:?}"),
        }
        // Buffer reset: a bare turn end now emits only TurnComplete (no empty transcript).
        assert!(matches!(
            m.map(response_done()).as_slice(),
            [SessionEvent::TurnEvent(TurnEvent::TurnComplete)]
        ));
    }

    #[test]
    fn barge_in_commits_the_partial_line() {
        let mut m = EventMapper::default();
        let _ = m.map(transcript_delta("partial reply"));
        let speech = ServerEvent::SpeechStarted {
            event_id: "e".into(),
            audio_start_ms: 0,
        };
        match m.map(speech).as_slice() {
            [SessionEvent::Transcript {
                text, final_: true, ..
            }, SessionEvent::TurnEvent(TurnEvent::Interrupted)] => {
                assert_eq!(text, "partial reply");
            }
            other => panic!("expected committed partial + Interrupted, got {other:?}"),
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
