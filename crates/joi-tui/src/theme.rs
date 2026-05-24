//! The minimal-mono / refined-HUD palette, ported from the web frontend (`src/index.css`,
//! `Terminal.tsx`) to ratatui `Color::Rgb`. Pure presentation data — colors and the per-state status
//! mapping that mirrors `Prompt.tsx`'s `STATUS` map. Animation helpers (glow/blink) arrive in M5.

use joi_core::session::event::{AppState, ConnectionStatus};
use ratatui::style::Color;

/// The fixed dark floor the status dot fades toward when glowing (independent of the configurable
/// background, which may be transparent).
const GLOW_FLOOR: Color = Color::Rgb(0x07, 0x09, 0x0c);
/// Default accent when config omits/can't-parse one (teal).
pub const DEFAULT_ACCENT: Color = Color::Rgb(0x9a, 0xed, 0xe4);

pub const LINE: Color = Color::Rgb(0x44, 0x50, 0x5f);
pub const LINE_SOFT: Color = Color::Rgb(0x2c, 0x35, 0x42);

pub const FG: Color = Color::Rgb(0xf7, 0xf9, 0xfb);
pub const FG_DIM: Color = Color::Rgb(0xcd, 0xd6, 0xe0);
pub const FG_FAINT: Color = Color::Rgb(0x9a, 0xa4, 0xb0);

pub const DANGER: Color = Color::Rgb(0xe0, 0x8c, 0x8c);
pub const WARN: Color = Color::Rgb(0xd8, 0xc0, 0x8a);
pub const SPEAK: Color = Color::Rgb(0xc3, 0xb6, 0xe6);
pub const THINK: Color = Color::Rgb(0x93, 0xb2, 0xd6);

/// The two configurable colors, resolved from config. Everything else in the palette is fixed.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// UI background; [`Color::Reset`] means "use the terminal's own background" (transparent).
    pub background: Color,
    /// Accent color (brand, prompt, active states, controls, corners…).
    pub accent: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Color::Reset,
            accent: DEFAULT_ACCENT,
        }
    }
}

impl Theme {
    /// Resolve from the `ui.terminal` config strings. `transparent`/`default`/`terminal`/empty →
    /// transparent background; an unparseable accent falls back to [`DEFAULT_ACCENT`].
    #[must_use]
    pub fn from_config(background: &str, accent: &str) -> Self {
        let background = match background.trim().to_ascii_lowercase().as_str() {
            "" | "transparent" | "default" | "terminal" | "none" => Color::Reset,
            _ => parse_hex(background).unwrap_or(Color::Reset),
        };
        Self {
            background,
            accent: parse_hex(accent).unwrap_or(DEFAULT_ACCENT),
        }
    }
}

/// Parse `#rrggbb` (or bare `rrggbb`) into an RGB color.
fn parse_hex(s: &str) -> Option<Color> {
    let hex = s.trim().strip_prefix('#').unwrap_or_else(|| s.trim());
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(red, green, blue))
}

/// Lifecycle-state accent, mirroring the web `STATUS` color map.
pub fn state_color(state: AppState, accent: Color) -> Color {
    match state {
        AppState::Stopped => FG_FAINT,
        AppState::Connecting | AppState::Reconnecting => WARN,
        AppState::Listening => accent,
        AppState::Thinking => THINK,
        AppState::Speaking => SPEAK,
        AppState::Error => DANGER,
    }
}

/// Connection-status accent (reuses the lifecycle vocabulary, like `App.tsx`'s `CONN_COLOR`).
pub fn connection_color(status: ConnectionStatus, accent: Color) -> Color {
    match status {
        ConnectionStatus::Disconnected => FG_FAINT,
        ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => WARN,
        ConnectionStatus::Connected => accent,
    }
}

/// The animated status-dot color for the current render tick, mirroring `Prompt.tsx`'s STATUS
/// animations: a calm glow while listening, a faster one while thinking, a livelier (deeper) pulse
/// while speaking, and a brightness blink for the transient connecting/reconnecting states. Steady
/// when stopped or errored. Periods are in render ticks (~80 ms each).
pub fn status_dot(state: AppState, tick: u64, accent: Color) -> Color {
    match state {
        AppState::Stopped => FG_FAINT,
        AppState::Error => DANGER,
        AppState::Connecting | AppState::Reconnecting => glow(WARN, tick, 12),
        AppState::Listening => glow(accent, tick, 26), // ~2s, calm
        AppState::Thinking => glow(THINK, tick, 14),
        AppState::Speaking => glow_active(SPEAK, tick, 11), // ~0.85s, active
    }
}

/// Pulse a color between full and ~half brightness (toward the dark floor) on a triangle wave.
fn glow(base: Color, tick: u64, period: u64) -> Color {
    lerp(base, lerp(base, GLOW_FLOOR, 0.55), triangle(tick, period))
}

/// A deeper, livelier pulse (for the speaking state).
fn glow_active(base: Color, tick: u64, period: u64) -> Color {
    lerp(base, lerp(base, GLOW_FLOOR, 0.78), triangle(tick, period))
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
            status_dot(AppState::Listening, 0, DEFAULT_ACCENT),
            status_dot(AppState::Listening, 13, DEFAULT_ACCENT)
        );
    }

    #[test]
    fn stopped_dot_is_steady() {
        assert_eq!(
            status_dot(AppState::Stopped, 0, DEFAULT_ACCENT),
            status_dot(AppState::Stopped, 999, DEFAULT_ACCENT)
        );
    }

    #[test]
    fn from_config_parses_hex_and_transparent() {
        let t = Theme::from_config("transparent", "#9aede4");
        assert_eq!(t.background, Color::Reset);
        assert_eq!(t.accent, Color::Rgb(0x9a, 0xed, 0xe4));
        // bad accent → default; explicit bg hex parses.
        let t = Theme::from_config("#101418", "not-a-color");
        assert_eq!(t.background, Color::Rgb(0x10, 0x14, 0x18));
        assert_eq!(t.accent, DEFAULT_ACCENT);
    }
}
