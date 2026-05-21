//! [`MockSession`] — a scripted, deterministic [`RealtimeSession`] with no network.
//!
//! It drives the M1 loop and backs the conformance suite (SPEC §16). Each `send_text` emits a
//! fixed, ordered turn (transcript-before-turn-end); each `send_audio` emits one output chunk, so
//! the full media + abstraction path is exercised without a provider.

use async_trait::async_trait;
use joi_core::error::SessionError;
use joi_core::media::VideoFrame;
use joi_core::session::event::{EventReceiver, EventSender, SessionEvent, Speaker, TurnEvent};
use joi_core::session::{Capabilities, RealtimeSession, SessionConfig};
use tokio::sync::mpsc;

/// Samples emitted per scripted audio chunk (10 ms at 24 kHz).
const MOCK_AUDIO_CHUNK: usize = 240;

/// A scripted in-memory session. Construct with [`MockSession::new`] or pick capabilities with
/// [`MockSession::with_capabilities`].
pub struct MockSession {
    tx: Option<EventSender>,
    rx: Option<EventReceiver>,
    capabilities: Capabilities,
}

impl MockSession {
    /// A mock that advertises resumption + native screen input, but no async tool calls.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capabilities(Capabilities {
            session_resumption: true,
            native_screen_input: true,
            async_tool_calls: false,
        })
    }

    /// A mock with explicit capability flags (for capability-handling tests).
    #[must_use]
    pub fn with_capabilities(capabilities: Capabilities) -> Self {
        Self {
            tx: None,
            rx: None,
            capabilities,
        }
    }

    fn sender(&self) -> Result<EventSender, SessionError> {
        self.tx.clone().ok_or(SessionError::NotConnected)
    }
}

impl Default for MockSession {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RealtimeSession for MockSession {
    async fn connect(&mut self, _cfg: SessionConfig) -> Result<(), SessionError> {
        let (tx, rx) = mpsc::channel(64);
        self.tx = Some(tx);
        self.rx = Some(rx);
        Ok(())
    }

    async fn send_audio(&mut self, _pcm: &[i16]) -> Result<(), SessionError> {
        let tx = self.sender()?;
        let _ = tx
            .send(SessionEvent::AudioOutput {
                pcm: vec![0; MOCK_AUDIO_CHUNK],
            })
            .await;
        Ok(())
    }

    async fn send_video_frame(&mut self, _frame: &VideoFrame) -> Result<(), SessionError> {
        // The mock has nothing to do with a frame; just accept it.
        let _ = self.sender()?;
        Ok(())
    }

    async fn send_text(&mut self, text: &str) -> Result<(), SessionError> {
        let tx = self.sender()?;
        // Deterministic, ordered turn: start -> user echo -> agent partial -> agent final ->
        // audio -> complete. Ordering (transcript before turn-end) is what conformance checks.
        let _ = tx
            .send(SessionEvent::TurnEvent(TurnEvent::TurnStarted))
            .await;
        let _ = tx
            .send(SessionEvent::Transcript {
                speaker: Speaker::User,
                text: text.to_string(),
                final_: true,
            })
            .await;
        let _ = tx
            .send(SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text: "…".to_string(),
                final_: false,
            })
            .await;
        let _ = tx
            .send(SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text: format!("echo: {text}"),
                final_: true,
            })
            .await;
        let _ = tx
            .send(SessionEvent::AudioOutput {
                pcm: vec![0; MOCK_AUDIO_CHUNK],
            })
            .await;
        let _ = tx
            .send(SessionEvent::TurnEvent(TurnEvent::TurnComplete))
            .await;
        Ok(())
    }

    fn take_events(&mut self) -> EventReceiver {
        self.rx.take().unwrap_or_else(|| {
            // Defensive: a receiver that is already closed rather than panicking on misuse.
            let (_tx, rx) = mpsc::channel(1);
            rx
        })
    }

    async fn close(&mut self) -> Result<(), SessionError> {
        self.tx = None; // closing the sender ends the event stream
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use joi_core::media::AudioFormat;

    fn cfg() -> SessionConfig {
        SessionConfig {
            model: "mock".to_string(),
            system_instruction: String::new(),
            voice: None,
            input_audio: AudioFormat::INPUT,
            output_audio: AudioFormat::OUTPUT,
            enable_input_transcription: true,
            enable_output_transcription: true,
            initial_context: Vec::new(),
            resumption_handle: None,
            tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn scripted_turn_is_ordered() {
        let mut s = MockSession::new();
        s.connect(cfg()).await.unwrap();
        let mut rx = s.take_events();
        s.send_text("hi").await.unwrap();

        let mut agent_final_at = None;
        let mut turn_complete_at = None;
        let mut idx = 0;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await
        {
            match ev {
                SessionEvent::Transcript {
                    speaker: Speaker::Agent,
                    final_: true,
                    ..
                } => {
                    agent_final_at = Some(idx);
                }
                SessionEvent::TurnEvent(TurnEvent::TurnComplete) => {
                    turn_complete_at = Some(idx);
                    break;
                }
                _ => {}
            }
            idx += 1;
        }
        let a = agent_final_at.expect("agent final transcript");
        let t = turn_complete_at.expect("turn complete");
        assert!(a < t, "final transcript must precede turn-complete");
    }
}
