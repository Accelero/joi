//! Typed error enums for the core ports.
//!
//! Library code returns these (`thiserror`); the binary edge wraps them with `anyhow`
//! (PLAN §1). No `unwrap`/`expect`/`panic` on these paths.

use std::path::PathBuf;

/// Failure loading or validating [`crate::config::Config`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The layered figment merge or deserialize failed.
    #[error("failed to load configuration: {0}")]
    Load(String),
    /// A field was present but outside its allowed range / enum.
    #[error("invalid configuration: {field}: {reason}")]
    Invalid {
        /// Dotted path of the offending field, e.g. `media.audio.frame_ms`.
        field: String,
        /// Human-readable reason the value was rejected.
        reason: String,
    },
    /// A required directory could not be resolved or created.
    #[error("could not resolve path {path}: {reason}")]
    Path {
        /// The path that could not be resolved/created.
        path: PathBuf,
        /// Why resolution failed.
        reason: String,
    },
}

/// Failure on a [`crate::session::RealtimeSession`].
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Could not establish the realtime connection.
    #[error("connection failed: {0}")]
    Connect(String),
    /// The session is not connected, so this operation is invalid.
    #[error("session not connected")]
    NotConnected,
    /// Sending audio/video/text to the provider failed.
    #[error("send failed: {0}")]
    Send(String),
    /// The provider reported an authentication/authorization failure.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The provider sent a protocol-level error.
    #[error("provider error: {0}")]
    Provider(String),
    /// A capability whose seam exists but is not wired in the MVP (e.g. tool results — FR-24
    /// is `[LATER]`; PLAN §2, §8).
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
}

/// Failure on a [`crate::history::HistoryStore`].
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// An I/O error touching the history file or directory.
    #[error("history io error: {0}")]
    Io(String),
    /// A persisted record could not be (de)serialized.
    #[error("history serialization error: {0}")]
    Serde(String),
}

/// Failure changing a runtime setting via [`crate::settings`].
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The setting exists but isn't editable at runtime (file/env only).
    #[error("setting is not editable: {0}")]
    NotEditable(String),
    /// The supplied value was the wrong type or out of range for this setting.
    #[error("invalid value for {setting}: {reason}")]
    InvalidValue {
        /// Which setting was being changed.
        setting: String,
        /// Why the value was rejected.
        reason: String,
    },
    /// Persisting the changed config to disk failed.
    #[error("could not persist settings: {0}")]
    Io(String),
}

/// Failure on a [`crate::media::ScreenSource`].
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// No capture backend is available on this platform/session.
    #[error("screen capture unavailable: {0}")]
    Unavailable(String),
    /// The requested source id does not exist.
    #[error("unknown capture source: {0}")]
    UnknownSource(String),
    /// The capture backend returned an error while running.
    #[error("capture backend error: {0}")]
    Backend(String),
}
