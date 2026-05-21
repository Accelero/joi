//! Adapter conformance suite + shared fixtures (SPEC §16).
//!
//! [`run_conformance`] runs one ordered scenario against any [`RealtimeSession`] implementation. A
//! functional adapter (mock, later Gemini) must produce a correctly ordered turn; a compile-only
//! stub (OpenAI, SPEC §4.4) is allowed to reject `connect` with
//! [`SessionError::Unimplemented`] — and is reported as [`ConformanceOutcome::StubVerified`]. This
//! is how provider-agnosticism is *proven*: the same suite passes against every adapter.

use std::time::Duration;

use joi_core::error::SessionError;
use joi_core::media::AudioFormat;
use joi_core::session::event::{SessionEvent, Speaker, TurnEvent};
use joi_core::session::{RealtimeSession, SessionConfig};

/// Per-event read timeout — generous, since the mock is synchronous and CI is slow.
const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

/// The result of a successful conformance run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformanceOutcome {
    /// The adapter connected and produced a correctly ordered turn.
    FullLoop,
    /// The adapter is a compile-only stub: `connect` returned `Unimplemented` (SPEC §4.4).
    StubVerified,
}

/// A conformance violation.
#[derive(Debug, thiserror::Error)]
#[error("conformance failure: {0}")]
pub struct ConformanceError(pub String);

impl ConformanceError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// A representative [`SessionConfig`] for tests/fixtures.
#[must_use]
pub fn sample_session_config() -> SessionConfig {
    SessionConfig {
        model: "conformance-model".to_string(),
        system_instruction: "You are under test.".to_string(),
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

/// Run the conformance scenario against `session`.
///
/// Functional adapters must, after `send_text`, emit a final agent transcript **before**
/// `TurnComplete` (the transcript-before-turn-end ordering guarantee of SPEC §4).
pub async fn run_conformance<S: RealtimeSession>(
    mut session: S,
) -> Result<ConformanceOutcome, ConformanceError> {
    // Capabilities must be queryable before connect, and must not panic.
    let _ = session.capabilities();

    match session.connect(sample_session_config()).await {
        Ok(()) => {}
        Err(SessionError::Unimplemented(_)) => return Ok(ConformanceOutcome::StubVerified),
        Err(e) => return Err(ConformanceError::new(format!("connect failed: {e}"))),
    }

    let mut events = session.take_events();
    session
        .send_text("conformance probe")
        .await
        .map_err(|e| ConformanceError::new(format!("send_text failed: {e}")))?;

    let mut saw_user = false;
    let mut agent_final_idx: Option<usize> = None;
    let mut turn_complete_idx: Option<usize> = None;
    let mut idx = 0usize;

    while turn_complete_idx.is_none() {
        let ev = match tokio::time::timeout(EVENT_TIMEOUT, events.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                return Err(ConformanceError::new(
                    "event stream closed before TurnComplete",
                ))
            }
            Err(_) => return Err(ConformanceError::new("timed out waiting for events")),
        };
        match ev {
            SessionEvent::Transcript {
                speaker: Speaker::User,
                ..
            } => saw_user = true,
            SessionEvent::Transcript {
                speaker: Speaker::Agent,
                final_: true,
                ..
            } => {
                agent_final_idx = Some(idx);
            }
            SessionEvent::TurnEvent(TurnEvent::TurnComplete) => turn_complete_idx = Some(idx),
            _ => {}
        }
        idx += 1;
    }

    if !saw_user {
        return Err(ConformanceError::new("no user transcript observed"));
    }
    let agent = agent_final_idx
        .ok_or_else(|| ConformanceError::new("no final agent transcript observed"))?;
    let complete = turn_complete_idx.unwrap_or(usize::MAX);
    if agent >= complete {
        return Err(ConformanceError::new(
            "final transcript must precede TurnComplete",
        ));
    }

    session
        .close()
        .await
        .map_err(|e| ConformanceError::new(format!("close failed: {e}")))?;

    Ok(ConformanceOutcome::FullLoop)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use joi_providers::mock::MockSession;
    use joi_providers::openai::OpenAIAdapter;

    #[tokio::test]
    async fn mock_passes_full_loop() {
        let outcome = run_conformance(MockSession::new()).await.unwrap();
        assert_eq!(outcome, ConformanceOutcome::FullLoop);
    }

    #[tokio::test]
    async fn openai_stub_is_verified_not_failed() {
        // Proves no Gemini-ism leaked: the abstraction is honest against an unimplemented adapter.
        let outcome = run_conformance(OpenAIAdapter::new()).await.unwrap();
        assert_eq!(outcome, ConformanceOutcome::StubVerified);
    }
}
