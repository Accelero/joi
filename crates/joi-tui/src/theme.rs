//! The minimal-mono / refined-HUD palette, ported from the web frontend (`src/index.css`,
//! `Terminal.tsx`) to ratatui `Color::Rgb`. Pure presentation data — colors and the per-state status
//! mapping that mirrors `Prompt.tsx`'s `STATUS` map. Animation helpers (glow/blink) arrive in M5.

use joi_core::session::event::{AppState, ConnectionStatus};
use ratatui::style::Color;

pub const BASE: Color = Color::Rgb(0x07, 0x09, 0x0c);
pub const LINE: Color = Color::Rgb(0x44, 0x50, 0x5f);
pub const LINE_SOFT: Color = Color::Rgb(0x2c, 0x35, 0x42);

pub const FG: Color = Color::Rgb(0xf7, 0xf9, 0xfb);
pub const FG_DIM: Color = Color::Rgb(0xcd, 0xd6, 0xe0);
pub const FG_FAINT: Color = Color::Rgb(0x9a, 0xa4, 0xb0);

pub const ACCENT: Color = Color::Rgb(0x9a, 0xed, 0xe4);
pub const DANGER: Color = Color::Rgb(0xe0, 0x8c, 0x8c);
pub const WARN: Color = Color::Rgb(0xd8, 0xc0, 0x8a);
pub const SPEAK: Color = Color::Rgb(0xc3, 0xb6, 0xe6);
pub const THINK: Color = Color::Rgb(0x93, 0xb2, 0xd6);

/// Lifecycle-state accent, mirroring the web `STATUS` color map.
pub fn state_color(state: AppState) -> Color {
    match state {
        AppState::Stopped => FG_FAINT,
        AppState::Connecting | AppState::Reconnecting => WARN,
        AppState::Listening => ACCENT,
        AppState::Thinking => THINK,
        AppState::Speaking => SPEAK,
        AppState::Error => DANGER,
    }
}

/// Connection-status accent (reuses the lifecycle vocabulary, like `App.tsx`'s `CONN_COLOR`).
pub fn connection_color(status: ConnectionStatus) -> Color {
    match status {
        ConnectionStatus::Disconnected => FG_FAINT,
        ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => WARN,
        ConnectionStatus::Connected => ACCENT,
    }
}

/// The animated status-dot color for the current render tick, mirroring `Prompt.tsx`'s STATUS
/// animations: a calm glow while listening, a faster one while thinking, a livelier (deeper) pulse
/// while speaking, and a brightness blink for the transient connecting/reconnecting states. Steady
/// when stopped or errored. Periods are in render ticks (~80 ms each).
pub fn status_dot(state: AppState, tick: u64) -> Color {
    match state {
        AppState::Stopped => FG_FAINT,
        AppState::Error => DANGER,
        AppState::Connecting | AppState::Reconnecting => glow(WARN, tick, 12),
        AppState::Listening => glow(ACCENT, tick, 26), // ~2s, calm
        AppState::Thinking => glow(THINK, tick, 14),
        AppState::Speaking => glow_active(SPEAK, tick, 11), // ~0.85s, active
    }
}

/// Pulse a color between full and ~half brightness (toward the background) on a triangle wave.
fn glow(base: Color, tick: u64, period: u64) -> Color {
    lerp(base, lerp(base, BASE, 0.55), triangle(tick, period))
}

/// A deeper, livelier pulse (for the speaking state).
fn glow_active(base: Color, tick: u64, period: u64) -> Color {
    lerp(base, lerp(base, BASE, 0.78), triangle(tick, period))
}

/// A 0→1→0 triangle wave over `period` ticks.
fn triangle(tick: u64, period: u64) -> f64 {
    let period = period.max(2);
    let p = (tick % period) as f64;
    let half = period as f64 / 2.0;
    if p < half {
        p / half
    } else {
        2.0 - p / half
    }
}

/// Linearly interpolate between two RGB colors (`t` in `[0, 1]`).
fn lerp(a: Color, b: Color, t: f64) -> Color {
    let (ar, ag, ab) = rgb(a);
    let (br, bg, bb) = rgb(b);
    let mix = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
}

fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0xff, 0xff, 0xff),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_dot_animates_over_the_period() {
        // tick 0 is full brightness, mid-period is dimmed — so they differ.
        assert_ne!(
            status_dot(AppState::Listening, 0),
            status_dot(AppState::Listening, 13)
        );
    }

    #[test]
    fn stopped_dot_is_steady() {
        assert_eq!(
            status_dot(AppState::Stopped, 0),
            status_dot(AppState::Stopped, 999)
        );
    }
}
