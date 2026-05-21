//! Pure media value types and framing helpers (SPEC §7).
//!
//! No I/O and no Web Audio here — the webview owns capture/playback and the binary
//! `tauri::ipc::Channel` transport (SPEC §11.2). This module is the shared, testable vocabulary:
//! formats, the framed [`VideoFrame`], and the PCM16 ⟷ little-endian-bytes conversions used on the
//! Channel boundary.

use serde::{Deserialize, Serialize};

/// A linear-PCM audio format. Joi only uses signed 16-bit mono (SPEC §7.1/7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct AudioFormat {
    /// Samples per second (Hz).
    pub sample_rate: u32,
    /// Channel count. Always 1 in the MVP.
    pub channels: u16,
}

impl AudioFormat {
    /// Mic input format sent to the provider: 16 kHz mono.
    pub const INPUT: Self = Self {
        sample_rate: 16_000,
        channels: 1,
    };
    /// Audio output format received from the provider: 24 kHz mono.
    pub const OUTPUT: Self = Self {
        sample_rate: 24_000,
        channels: 1,
    };

    /// Number of samples in a `frame_ms`-millisecond frame at this rate.
    ///
    /// e.g. 16 kHz × 20 ms = 320 samples (SPEC §7.1).
    #[must_use]
    pub const fn samples_per_frame(&self, frame_ms: u32) -> usize {
        (self.sample_rate as usize * frame_ms as usize) / 1000
    }
}

/// How a [`VideoFrame`]'s bytes are encoded for the model (SPEC §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameEncoding {
    /// JPEG.
    Jpeg,
    /// WebP.
    Webp,
}

/// One encoded screen frame handed to [`crate::session::RealtimeSession::send_video_frame`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VideoFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Encoding of `data`.
    pub encoding: FrameEncoding,
    /// The encoded image bytes.
    pub data: Vec<u8>,
}

/// Split a PCM buffer into fixed-size frames of `samples_per_frame` samples each.
///
/// The final chunk may be shorter than a full frame; the caller decides whether to pad or drop it.
pub fn frames(pcm: &[i16], samples_per_frame: usize) -> std::slice::Chunks<'_, i16> {
    // A zero frame size would panic `Chunks`; treat it as "one frame" defensively.
    pcm.chunks(samples_per_frame.max(1))
}

/// Encode PCM16 samples as little-endian bytes for the binary Channel transport (SPEC §11.2).
#[must_use]
pub fn pcm16_to_le_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for &s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Decode little-endian bytes from the Channel back into PCM16 samples.
///
/// A trailing odd byte (a torn sample) is ignored rather than erroring.
#[must_use]
pub fn le_bytes_to_pcm16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn input_frame_is_320_samples_at_20ms() {
        assert_eq!(AudioFormat::INPUT.samples_per_frame(20), 320);
        assert_eq!(AudioFormat::OUTPUT.samples_per_frame(20), 480);
    }

    #[test]
    fn framing_splits_into_20ms_frames() {
        let pcm = vec![0i16; 320 * 3 + 100];
        let chunks: Vec<_> = frames(&pcm, 320).collect();
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].len(), 320);
        assert_eq!(chunks[3].len(), 100); // partial tail preserved
    }

    #[test]
    fn pcm_byte_roundtrip() {
        let pcm = vec![0i16, 1, -1, 32767, -32768, 12345];
        let bytes = pcm16_to_le_bytes(&pcm);
        assert_eq!(bytes.len(), pcm.len() * 2);
        assert_eq!(le_bytes_to_pcm16(&bytes), pcm);
    }

    #[test]
    fn torn_trailing_byte_is_ignored() {
        let bytes = vec![1u8, 0, 2]; // 1.5 samples
        assert_eq!(le_bytes_to_pcm16(&bytes), vec![1i16]);
    }
}
