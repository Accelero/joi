//! The provider-agnostic realtime session port (SPEC §4) — the founding abstraction.
//!
//! App logic (lifecycle, history, terminal UI) talks **only** to [`RealtimeSession`], never to a
//! provider SDK. Adapters absorb every provider divergence (audio formats, VAD/interruption
//! semantics, resumption, tool-call schema). App code must compile and behave identically against
//! any adapter that honors this trait — that is how provider-agnosticism is *proven* (SPEC §16).

pub mod event;

use async_trait::async_trait;

use crate::config::Config;
use crate::error::SessionError;
use crate::history::HistoryTurn;
use crate::media::{AudioFormat, VideoFrame};
use crate::metrics::{TokenUsage, TransportBytes};
use crate::tools::{ToolCallId, ToolResult, ToolSchema};

pub use event::{
    AppState, CloseReason, ConnectionStatus, EventReceiver, EventSender, Reachability,
    SessionEvent, Speaker, TurnEvent, UiEvent,
};

/// Everything needed to open a session (SPEC §4).
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Model id.
    pub model: String,
    /// System instruction / persona.
    pub system_instruction: String,
    /// Optional named voice.
    pub voice: Option<String>,
    /// Mic input format (16 kHz mono PCM16).
    pub input_audio: AudioFormat,
    /// Audio output format (24 kHz mono PCM16).
    pub output_audio: AudioFormat,
    /// Request transcription of the user's audio (FR-3).
    pub enable_input_transcription: bool,
    /// Request transcription of the agent's audio (FR-3).
    pub enable_output_transcription: bool,
    /// Prior conversation turns to seed on resume (SPEC §6.3).
    pub initial_context: Vec<HistoryTurn>,
    /// Provider session-resumption handle for a transient reconnect (SPEC §5.2).
    pub resumption_handle: Option<String>,
    /// `[POST]` Tool schemas. Always empty in the MVP (SPEC §10).
    pub tools: Vec<ToolSchema>,
}

impl SessionConfig {
    /// Build a session config from [`Config`] plus the restore seed for this start/resume.
    #[must_use]
    pub fn from_config(
        cfg: &Config,
        initial_context: Vec<HistoryTurn>,
        resumption_handle: Option<String>,
    ) -> Self {
        Self {
            model: cfg.live_api.gemini.model.clone(),
            system_instruction: cfg.live_api.gemini.system_instruction.clone(),
            voice: cfg.live_api.gemini.voice.clone(),
            input_audio: AudioFormat::INPUT,
            output_audio: AudioFormat::OUTPUT,
            enable_input_transcription: cfg.live_api.gemini.input_transcription,
            enable_output_transcription: cfg.live_api.gemini.output_transcription,
            initial_context,
            resumption_handle,
            tools: Vec::new(),
        }
    }
}

/// Provider capability flags. App logic must **never assume** these (SPEC §4) — it checks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Provider supports session resumption handles (SPEC §5.2).
    pub session_resumption: bool,
    /// Provider accepts native screen/video input (SPEC §7.3).
    pub native_screen_input: bool,
    /// `[POST]` Provider supports non-blocking tool calls. Plumbed but ignored in MVP (SPEC §10).
    pub async_tool_calls: bool,
}

/// A realtime, full-duplex voice session with a provider.
///
/// The event stream is taken **once** after `connect` via [`RealtimeSession::take_events`], which
/// returns an *owned* receiver. A borrowed stream would alias `&mut self` and forbid calling
/// `send_*` while reading events; the owned receiver lets the [`crate::manager::SessionManager`]
/// actor `select!` over sends and events concurrently (SPEC §4 design note).
#[async_trait]
pub trait RealtimeSession: Send {
    /// Open the connection with the given config.
    async fn connect(&mut self, cfg: SessionConfig) -> Result<(), SessionError>;

    /// Send a frame of 16 kHz mono PCM16 mic audio to the provider.
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SessionError>;

    /// Send one encoded screen frame to the provider.
    async fn send_video_frame(&mut self, frame: &VideoFrame) -> Result<(), SessionError>;

    /// Send a text message (e.g. typed input).
    async fn send_text(&mut self, text: &str) -> Result<(), SessionError>;

    /// Signal that the mic audio stream has paused (e.g. the user muted) so the provider finalizes
    /// the current turn and stops expecting audio until the next [`send_audio`](Self::send_audio)
    /// reopens it. The default is a no-op — a provider without an explicit pause signal just stops
    /// receiving audio.
    async fn end_audio_stream(&mut self) -> Result<(), SessionError> {
        Ok(())
    }

    /// `[POST]` Return a tool result to the provider. Unused in the MVP; the default rejects
    /// (SPEC §10).
    async fn send_tool_result(
        &mut self,
        _id: ToolCallId,
        _result: ToolResult,
    ) -> Result<(), SessionError> {
        Err(SessionError::Unimplemented("tool results"))
    }

    /// Take the owned event stream. Call exactly once, after `connect`.
    fn take_events(&mut self) -> EventReceiver;

    /// Close the session (no streaming cost afterwards — SPEC §5.3).
    async fn close(&mut self) -> Result<(), SessionError>;

    /// This provider's capability flags.
    fn capabilities(&self) -> Capabilities;

    /// Cumulative wire-byte counters for the live connection, if this provider measures them.
    /// `None` (the default) means it doesn't — the [`crate::manager::SessionManager`] then reports
    /// a payload-level estimate instead. Counts are monotonic for one connection's lifetime.
    fn transport_bytes(&self) -> Option<TransportBytes> {
        None
    }

    /// Cumulative provider-reported [`TokenUsage`] for this session (input/prompt and output
    /// tokens), if the provider surfaces it. `None` (the default) means it doesn't — the
    /// [`crate::manager::SessionManager`] then falls back to a chars/4 estimate. Counts are
    /// monotonic for one connection; the manager differences them into a per-second token rate.
    fn token_usage(&self) -> Option<TokenUsage> {
        None
    }
}
