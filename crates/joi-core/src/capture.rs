//! The screen-capture port (SPEC §7.3).
//!
//! Two pipelines implement this later (PLAN M4): a webview `getDisplayMedia` path and a native
//! `scap`/`xcap` path. The [`CaptureSource`] enum carries `Display(id)` now and gains `Window(id)`
//! post-MVP (FR-12), so app-window capture is an added variant, not a rewrite.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::CaptureError;
use crate::media::VideoFrame;

/// Stream of captured frames from a running [`ScreenSource`].
pub type FrameStream = tokio::sync::mpsc::Receiver<VideoFrame>;

/// What to capture (SPEC §7.3). `Window` is a `[POST]` variant kept off the MVP path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    /// A whole display, by id.
    Display(String),
}

/// The kind of an enumerated source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A display/monitor.
    Display,
}

/// A selectable capture source, returned by [`ScreenSource::list`] (FR-9).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SourceInfo {
    /// Stable id used in [`CaptureSource`].
    pub id: String,
    /// Human-readable label for the picker.
    pub label: String,
    /// What kind of source this is.
    pub kind: SourceKind,
}

/// Capture quality settings (FR-11). Defaults aim for native resolution at the max accepted rate,
/// clamped by a configurable ceiling for cost/bandwidth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct CaptureQuality {
    /// Frames per second.
    pub fps: u32,
    /// Resolution ceiling (longest edge, pixels).
    pub max_width: u32,
    /// Encode quality, 1–100.
    pub quality: u8,
}

impl CaptureQuality {
    /// Clamp each field into its valid range and below the given ceilings (FR-11).
    #[must_use]
    pub fn clamped(self, fps_ceiling: u32, width_ceiling: u32) -> Self {
        Self {
            fps: self.fps.clamp(1, fps_ceiling.max(1)),
            max_width: self.max_width.clamp(1, width_ceiling.max(1)),
            quality: self.quality.clamp(1, 100),
        }
    }
}

/// Enumerate, start, and stop screen capture.
#[async_trait]
pub trait ScreenSource: Send + Sync {
    /// List available displays (windows are `[POST]`).
    async fn list(&self) -> Result<Vec<SourceInfo>, CaptureError>;

    /// Start capturing `sel` at `quality`, returning a stream of encoded frames.
    async fn start(
        &self,
        sel: CaptureSource,
        quality: CaptureQuality,
    ) -> Result<FrameStream, CaptureError>;

    /// Stop capture immediately, revoking in-flight frames (FR-10).
    async fn stop(&self) -> Result<(), CaptureError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_clamps_into_range_and_under_ceilings() {
        let q = CaptureQuality {
            fps: 999,
            max_width: 100_000,
            quality: 200,
        };
        let c = q.clamped(30, 1920);
        assert_eq!(c.fps, 30);
        assert_eq!(c.max_width, 1920);
        assert_eq!(c.quality, 100);

        let zero = CaptureQuality {
            fps: 0,
            max_width: 0,
            quality: 0,
        };
        let c = zero.clamped(30, 1920);
        assert_eq!(c.fps, 1);
        assert_eq!(c.max_width, 1);
        assert_eq!(c.quality, 1);
    }
}
