//! `[M2]` [`GeminiAdapter`] — Gemini Live native audio over adk-rust (SPEC §4.3, §4.5).
//!
//! **Stub until the M2 precondition spike.** The real connect/send/recv/interrupt/resumption shape
//! of adk-rust is unknown until its API is read; per PLAN M2 that is a go/no-go gate recorded in
//! `NOTES-adk.md`, and any adk-rust churn is adapted **here only** so it never leaks past
//! [`RealtimeSession`]. Until then this returns [`SessionError::Unimplemented`] from `connect`.
//!
//! The reported [`Capabilities`] reflect Gemini's documented intent so dependent code can be
//! exercised against them; they are not load-bearing until the adapter is implemented.

use async_trait::async_trait;
use joi_core::error::SessionError;
use joi_core::media::VideoFrame;
use joi_core::session::event::{EventReceiver, SessionEvent};
use joi_core::session::{Capabilities, RealtimeSession, SessionConfig};
use tokio::sync::mpsc;

/// Gemini Live adapter. Connect is unimplemented pending the M2 adk-rust spike.
#[derive(Debug, Default)]
pub struct GeminiAdapter;

impl GeminiAdapter {
    /// Construct the (not-yet-functional) adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RealtimeSession for GeminiAdapter {
    async fn connect(&mut self, _cfg: SessionConfig) -> Result<(), SessionError> {
        Err(SessionError::Unimplemented(
            "Gemini adapter pending M2 adk-rust API spike (see NOTES-adk.md)",
        ))
    }

    async fn send_audio(&mut self, _pcm: &[i16]) -> Result<(), SessionError> {
        Err(SessionError::NotConnected)
    }

    async fn send_video_frame(&mut self, _frame: &VideoFrame) -> Result<(), SessionError> {
        Err(SessionError::NotConnected)
    }

    async fn send_text(&mut self, _text: &str) -> Result<(), SessionError> {
        Err(SessionError::NotConnected)
    }

    fn take_events(&mut self) -> EventReceiver {
        let (_tx, rx) = mpsc::channel::<SessionEvent>(1);
        rx
    }

    async fn close(&mut self) -> Result<(), SessionError> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            session_resumption: true,
            native_screen_input: true,
            async_tool_calls: true,
        }
    }
}
