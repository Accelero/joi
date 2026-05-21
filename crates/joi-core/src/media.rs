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

/// Convert clamped float `[-1.0, 1.0]` samples to signed 16-bit PCM (native capture gives floats).
#[must_use]
pub fn pcm16_from_f32(input: &[f32]) -> Vec<i16> {
    input
        .iter()
        .map(|&s| {
            let s = s.clamp(-1.0, 1.0);
            // Asymmetric scale so -1.0→-32768 and 1.0→32767 (the full i16 range).
            if s < 0.0 {
                (s * 32768.0) as i16
            } else {
                (s * 32767.0) as i16
            }
        })
        .collect()
}

/// Linear-interpolation resample of mono PCM16 from `in_rate` to `out_rate` (Hz).
///
/// Used both ways: mic device rate → 16 kHz (down) and provider 24 kHz → device rate (either way).
/// Equal rates, an empty input, or a zero rate return the input unchanged — never panics.
#[must_use]
pub fn resample_linear(input: &[i16], in_rate: u32, out_rate: u32) -> Vec<i16> {
    if input.is_empty() || in_rate == 0 || out_rate == 0 || in_rate == out_rate {
        return input.to_vec();
    }
    let ratio = f64::from(in_rate) / f64::from(out_rate);
    let out_len = (input.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let i0 = pos.floor() as usize;
        let i1 = (i0 + 1).min(input.len() - 1);
        let frac = pos - i0 as f64;
        let s = f64::from(input[i0]) * (1.0 - frac) + f64::from(input[i1]) * frac;
        out.push(s.round() as i16);
    }
    out
}

/// Accumulates PCM16 samples and emits fixed-size frames, buffering the remainder across pushes —
/// native audio callbacks rarely align to `frame_size` boundaries (SPEC §7.1).
#[derive(Debug)]
pub struct FrameAccumulator {
    frame_size: usize,
    remainder: Vec<i16>,
}

impl FrameAccumulator {
    /// A new accumulator emitting frames of `frame_size` samples (clamped to ≥ 1).
    #[must_use]
    pub fn new(frame_size: usize) -> Self {
        Self {
            frame_size: frame_size.max(1),
            remainder: Vec::new(),
        }
    }

    /// Push samples; return any newly completed frames (each exactly `frame_size`).
    pub fn push(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
        self.remainder.extend_from_slice(samples);
        let mut frames = Vec::new();
        while self.remainder.len() >= self.frame_size {
            frames.push(self.remainder.drain(..self.frame_size).collect());
        }
        frames
    }

    /// Samples currently held back, waiting for a full frame.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.remainder.len()
    }
}

/// Playback jitter buffer (SPEC §7.2): enqueue provider PCM, pull fixed blocks (silence on
/// underrun), and [`flush`](Self::flush) instantly for barge-in (FR-2). Pure and lock-free of I/O —
/// the native output callback pulls; session events enqueue.
#[derive(Debug, Default)]
pub struct JitterBuffer {
    queue: std::collections::VecDeque<i16>,
}

impl JitterBuffer {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk of output PCM.
    pub fn enqueue(&mut self, chunk: &[i16]) {
        self.queue.extend(chunk.iter().copied());
    }

    /// Samples currently buffered.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.queue.len()
    }

    /// Buffered duration in ms at `rate` (Hz).
    #[must_use]
    pub fn buffered_ms(&self, rate: u32) -> f64 {
        if rate == 0 {
            return 0.0;
        }
        (self.queue.len() as f64 / f64::from(rate)) * 1000.0
    }

    /// Pull exactly `n` samples; a missing tail is silence (zeros) on underrun.
    pub fn pull(&mut self, n: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.queue.pop_front().unwrap_or(0));
        }
        out
    }

    /// Drop all buffered audio immediately (barge-in / interrupt).
    pub fn flush(&mut self) {
        self.queue.clear();
    }
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

    #[test]
    fn float_to_pcm16_clamps_to_full_range() {
        assert_eq!(pcm16_from_f32(&[0.0]), vec![0]);
        assert_eq!(pcm16_from_f32(&[1.0]), vec![32767]);
        assert_eq!(pcm16_from_f32(&[-1.0]), vec![-32768]);
        assert_eq!(pcm16_from_f32(&[2.0, -2.0]), vec![32767, -32768]); // clamped
    }

    #[test]
    fn resample_equal_rate_or_empty_is_identity() {
        let pcm = vec![1i16, 2, 3, 4];
        assert_eq!(resample_linear(&pcm, 16_000, 16_000), pcm);
        assert_eq!(resample_linear(&[], 48_000, 16_000), Vec::<i16>::new());
        assert_eq!(resample_linear(&pcm, 0, 16_000), pcm); // zero rate → unchanged, no panic
    }

    #[test]
    fn resample_downsample_shortens_by_ratio() {
        let pcm = vec![0i16; 480]; // 10 ms at 48 kHz
        let out = resample_linear(&pcm, 48_000, 16_000); // → 16 kHz
        assert_eq!(out.len(), 160); // 480 / 3
    }

    #[test]
    fn frame_accumulator_emits_full_frames_and_buffers_remainder() {
        let mut acc = FrameAccumulator::new(320);
        let frames = acc.push(&vec![0i16; 320 * 3 + 100]);
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|f| f.len() == 320));
        assert_eq!(acc.buffered(), 100);
        // The remainder completes on the next push.
        let more = acc.push(&vec![0i16; 220]);
        assert_eq!(more.len(), 1);
        assert_eq!(acc.buffered(), 0);
    }

    #[test]
    fn jitter_buffer_pulls_then_pads_with_silence_and_flushes() {
        let mut jb = JitterBuffer::new();
        jb.enqueue(&[1, 2, 3]);
        assert_eq!(jb.buffered(), 3);
        assert_eq!(jb.pull(2), vec![1, 2]);
        assert_eq!(jb.pull(3), vec![3, 0, 0]); // underrun → silence
        jb.enqueue(&[9, 9]);
        jb.flush();
        assert_eq!(jb.buffered(), 0);
        assert_eq!(jb.pull(2), vec![0, 0]);
    }
}
