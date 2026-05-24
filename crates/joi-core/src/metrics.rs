//! Throughput metering surfaced to the UI (via [`crate::session::UiEvent::Metrics`]).
//!
//! The [`crate::manager::SessionManager`] records the payload bytes crossing the provider boundary
//! in each direction and the agent's output tokens, then samples a rolling rate on a fixed cadence
//! ([`SAMPLE_INTERVAL`]). These are **payload-level** rates — the audio/text/video bytes the engine
//! sends and receives, not raw WebSocket/TLS wire bytes — which is what a "how much is moving"
//! indicator wants. Living in the manager (above any provider SDK) keeps it provider-agnostic: the
//! same numbers surface whatever adapter is plugged in.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How often the manager samples the meter and emits a [`MetricsSnapshot`] while a session is live.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Tokens are estimated at this many characters each — the same chars/4 basis as
/// [`crate::history::estimate_tokens`], the single tokenizer swap-point (kept in sync deliberately).
const CHARS_PER_TOKEN: f64 = 4.0;

/// A point-in-time throughput sample surfaced to the UI as [`crate::session::UiEvent::Metrics`].
///
/// All four figures are instantaneous rates over the most recent [`SAMPLE_INTERVAL`] window, so
/// the frontend can render a live up/down bandwidth + up/down token-rate indicator without doing
/// any math of its own (the architecture rule: logic in Rust, the UI only renders).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Upstream rate to the provider in kbit/s — actual WebSocket frame bytes (base64+JSON) when the
    /// provider reports them, otherwise a payload-level estimate (mic audio + screen frames + text).
    pub up_kbps: f64,
    /// Downstream rate from the provider in kbit/s — actual WebSocket frame bytes when reported,
    /// otherwise a payload-level estimate (synthesized audio + transcripts).
    pub down_kbps: f64,
    /// Upstream token rate in tokens/s — **input** tokens the provider is consuming (audio + video +
    /// text, all modalities). From Gemini's real `usageMetadata.promptTokenCount` differenced over
    /// the window when available; otherwise a chars/4 estimate of sent text only.
    pub up_tokens_per_sec: f64,
    /// Downstream token rate in tokens/s — **output** tokens the model is generating. From Gemini's
    /// real `usageMetadata.responseTokenCount` differenced over the window when available; otherwise
    /// a chars/4 estimate of the output transcript.
    pub down_tokens_per_sec: f64,
}

impl MetricsSnapshot {
    /// The all-zero sample, emitted once when a session stops so the UI clears its indicator.
    pub const ZERO: Self = Self {
        up_kbps: 0.0,
        down_kbps: 0.0,
        up_tokens_per_sec: 0.0,
        down_tokens_per_sec: 0.0,
    };
}

/// Cumulative provider-reported token counts for a live session, by direction. Like
/// [`TransportBytes`], these are **monotonic** for the session's lifetime; the manager differences
/// successive reads into a per-window token rate. `up` is input/prompt tokens (Gemini's
/// `promptTokenCount`, which grows with the context window); `down` is output tokens (a running sum
/// reconstructed from the per-turn `responseTokenCount`). `None` from a provider means it doesn't
/// report usage, so the manager falls back to the [`ThroughputMeter`]'s chars/4 estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Cumulative input/prompt tokens the provider has counted this session.
    pub up: u64,
    /// Cumulative output/response tokens the provider has counted this session.
    pub down: u64,
}

/// Accumulates payload byte/token counts and converts a window of them into a [`MetricsSnapshot`].
///
/// The manager calls [`add_up`](Self::add_up)/[`add_down`](Self::add_down)/
/// [`add_input_text`](Self::add_input_text)/[`add_output_text`](Self::add_output_text) as data flows,
/// then [`sample`](Self::sample) on each tick to drain the window into a rate. Byte tallies are `u64`
/// and saturate rather than overflow; the token estimate is derived from accumulated characters at
/// sample time, so streams of tiny transcript deltas don't each round up to a whole token.
///
/// The char-based token figures are only a **fallback** — when the provider reports real
/// [`TokenUsage`] the manager overrides them. They keep the meter meaningful for providers/test
/// doubles that don't report usage.
#[derive(Debug, Default)]
pub struct ThroughputMeter {
    up_bytes: u64,
    down_bytes: u64,
    in_chars: u64,
    out_chars: u64,
}

impl ThroughputMeter {
    /// Record `bytes` sent toward the provider during the current window.
    pub fn add_up(&mut self, bytes: u64) {
        self.up_bytes = self.up_bytes.saturating_add(bytes);
    }

    /// Record `bytes` received from the provider during the current window.
    pub fn add_down(&mut self, bytes: u64) {
        self.down_bytes = self.down_bytes.saturating_add(bytes);
    }

    /// Record a chunk of **sent** text; its characters feed the upstream token-rate fallback.
    pub fn add_input_text(&mut self, text: &str) {
        self.in_chars = self.in_chars.saturating_add(text.chars().count() as u64);
    }

    /// Record a chunk of agent **output** text; its characters feed the downstream token-rate
    /// fallback.
    pub fn add_output_text(&mut self, text: &str) {
        self.out_chars = self.out_chars.saturating_add(text.chars().count() as u64);
    }

    /// Reset the window without producing a sample (used when a session goes idle/stops).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Drain the window into a rate over `elapsed`, resetting the counters.
    ///
    /// Returns `None` when no time has passed (e.g. a frozen test clock) to avoid divide-by-zero;
    /// the counters are then left intact so the next tick can fold them into a real interval.
    pub fn sample(&mut self, elapsed: Duration) -> Option<MetricsSnapshot> {
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)] // payload tallies are far below f64's exact range
        let snap = MetricsSnapshot {
            up_kbps: bytes_to_kbps(self.up_bytes, secs),
            down_kbps: bytes_to_kbps(self.down_bytes, secs),
            up_tokens_per_sec: (self.in_chars as f64 / CHARS_PER_TOKEN) / secs,
            down_tokens_per_sec: (self.out_chars as f64 / CHARS_PER_TOKEN) / secs,
        };
        self.reset();
        Some(snap)
    }
}

/// Bytes over `secs` seconds as kilobits per second (1 byte = 8 bits, 1 kbit = 1000 bits).
/// Returns `0.0` for a non-positive window so callers needn't guard against divide-by-zero.
#[must_use]
#[allow(clippy::cast_precision_loss)] // tallies stay well within f64's exactly-representable range
pub fn bytes_to_kbps(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        return 0.0;
    }
    (bytes as f64) * 8.0 / 1000.0 / secs
}

/// Cumulative transport byte counters for a live connection, as reported by a provider that can
/// measure its own wire traffic (see [`crate::session::RealtimeSession::transport_bytes`]).
///
/// Counts are **monotonic** for the life of one connection; the manager differences successive
/// reads to turn them into a per-window rate. When a provider can't measure wire bytes the manager
/// falls back to the payload-level estimate accumulated in [`ThroughputMeter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransportBytes {
    /// Bytes sent toward the provider since the connection opened.
    pub sent: u64,
    /// Bytes received from the provider since the connection opened.
    pub received: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn sample_converts_bytes_to_kbps() {
        let mut m = ThroughputMeter::default();
        m.add_up(1000); // 1000 bytes = 8000 bits = 8 kbit
        m.add_down(2000); // 16 kbit
        let snap = m.sample(Duration::from_secs(1)).unwrap();
        assert_eq!(snap.up_kbps, 8.0);
        assert_eq!(snap.down_kbps, 16.0);
    }

    #[test]
    fn rate_scales_with_elapsed_window() {
        let mut m = ThroughputMeter::default();
        m.add_up(1000); // 8 kbit over half a second -> 16 kbit/s
        let snap = m.sample(Duration::from_millis(500)).unwrap();
        assert_eq!(snap.up_kbps, 16.0);
    }

    #[test]
    fn tokens_use_chars_over_four() {
        let mut m = ThroughputMeter::default();
        m.add_output_text("abcd"); // 4 chars -> 1 token
        m.add_output_text("efgh"); // 4 chars -> 1 token (8 chars total -> 2 tokens)
        m.add_input_text("wxyz"); // 4 sent chars -> 1 up token
        let snap = m.sample(Duration::from_secs(2)).unwrap(); // over 2 s
        assert_eq!(snap.down_tokens_per_sec, 1.0); // 2 output tokens / 2 s
        assert_eq!(snap.up_tokens_per_sec, 0.5); // 1 input token / 2 s
    }

    #[test]
    fn sample_drains_the_window() {
        let mut m = ThroughputMeter::default();
        m.add_up(1000);
        let _ = m.sample(Duration::from_secs(1)).unwrap();
        let next = m.sample(Duration::from_secs(1)).unwrap();
        assert_eq!(next, MetricsSnapshot::ZERO);
    }

    #[test]
    fn zero_elapsed_yields_no_sample_and_keeps_counters() {
        let mut m = ThroughputMeter::default();
        m.add_up(1000);
        assert!(m.sample(Duration::ZERO).is_none());
        // Counters survived: the next real interval folds them in.
        let snap = m.sample(Duration::from_secs(1)).unwrap();
        assert_eq!(snap.up_kbps, 8.0);
    }
}
