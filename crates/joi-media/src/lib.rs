//! Native media I/O for Joi (PLAN-NATIVE-MEDIA). All audio/screen capture and playback happen in
//! Rust via [`cpal`] (and later screen-capture crates), so no media ever crosses into the webview.
//!
//! `cpal`'s [`Stream`](cpal::Stream) is `!Send`, so each engine owns its stream on a dedicated OS
//! thread and is driven over a channel; callers hold only the `Send` channel sender.

pub mod capture;
pub mod playback;
pub mod screen;

pub use capture::{spawn_capture, CaptureHandle};
pub use playback::{spawn_playback, PlaybackCmd};
pub use screen::{spawn_screen_capture, ScreenHandle};

/// Failures from a native media engine.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// No default device for the requested direction.
    #[error("no default audio output device")]
    NoOutputDevice,
    /// No default audio input (microphone) device.
    #[error("no default audio input device")]
    NoInputDevice,
    /// The device's sample format isn't one we render.
    #[error("unsupported sample format: {0}")]
    UnsupportedFormat(String),
    /// The underlying audio backend (ALSA/cpal) returned an error.
    #[error("audio backend error: {0}")]
    Backend(String),
}
