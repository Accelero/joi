//! `[POST]` [`OpenAIAdapter`] — a compile-only stub (SPEC §4.4).
//!
//! It implements the trait signature, returns `Err`/unimplemented at runtime, and reports
//! `async_tool_calls = false`. Its purpose is to **keep the abstraction honest**: the workspace
//! must build with it present and the conformance suite must run against it, proving no Gemini-ism
//! leaked into app logic (SPEC §16, M5).

use async_trait::async_trait;
use joi_core::error::SessionError;
use joi_core::media::VideoFrame;
use joi_core::session::event::{EventReceiver, SessionEvent};
use joi_core::session::{Capabilities, RealtimeSession, SessionConfig};
use tokio::sync::mpsc;

/// Compile-only OpenAI Realtime adapter. Not functional in the MVP.
#[derive(Debug, Default)]
pub struct OpenAIAdapter;

impl OpenAIAdapter {
    /// Construct the stub.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RealtimeSession for OpenAIAdapter {
    async fn connect(&mut self, _cfg: SessionConfig) -> Result<(), SessionError> {
        Err(SessionError::Unimplemented(
            "OpenAI Realtime adapter is post-MVP",
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
        // An already-closed stream: the stub never connects, so it never emits.
        let (_tx, rx) = mpsc::channel::<SessionEvent>(1);
        rx
    }

    async fn close(&mut self) -> Result<(), SessionError> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            session_resumption: false,
            native_screen_input: false,
            async_tool_calls: false,
        }
    }
}
