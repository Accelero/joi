//! Native media I/O for Joi. All audio/screen capture and playback happen in Rust via
//! [`cpal`] (mic/speaker), `sonora` (the APM chain), and `xcap` (screen) — devices live only here,
//! never in `joi-core`.
//!
//! `cpal`'s [`Stream`](cpal::Stream) is `!Send`, so each engine owns its stream on a dedicated OS
//! thread and is driven over a channel; callers hold only the `Send` channel sender.

pub mod engine;

// Low-level cpal/xcap workers are crate-internal; `MediaEngine` is the public interface.
mod capture;
mod playback;
#[cfg(debug_assertions)]
mod processed_mic_recorder;
mod screen;

pub use engine::{MediaConfig, MediaEngine};

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
