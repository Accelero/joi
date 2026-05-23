//! Rendering: turn an [`AppModel`] into ratatui widgets. Thin and logic-free — it only reads the
//! model and the wall clock. The deck frame (rounded border + brand/clock header) echoes the web
//! `.deck`; the transcript, prompt, controls, and footer fill in across M2–M5.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use joi_core::session::event::{AppState, ConnectionStatus};

use crate::app::AppModel;
use crate::theme;
use crate::transcript::LineKind;

pub fn render(frame: &mut Frame, model: &mut AppModel) {
    let area = frame.area();
    let clock = chrono::Local::now().format("%H:%M:%S").to_string();

    // The deck: a full-bleed rounded frame on the base background, brand on the top-left of the
    // border, wall clock on the top-right.
    let deck = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::LINE))
        .style(Style::new().bg(theme::BASE))
        .title_top(brand_line())
        .title_top(
            Line::from(format!(" {clock} "))
                .right_aligned()
                .style(Style::new().fg(theme::FG_DIM)),
        );
    let inner = deck.inner(area);
    frame.render_widget(deck, area);

    // Top→bottom: controls bar, divider, transcript (fills), divider, status line, prompt, footer.
    // The two dividers bracket the transcript — one under the controls, one above the input zone.
    let rows = Layout::vertical([
        Constraint::Length(1), // controls
        Constraint::Length(1), // divider
        Constraint::Min(0),    // transcript
        Constraint::Length(1), // divider
        Constraint::Length(1), // status line
        Constraint::Length(1), // prompt
        Constraint::Length(1), // footer
    ])
    .split(inner);
    frame.render_widget(Paragraph::new(controls_line(model)), rows[0]);
    frame.render_widget(Paragraph::new(divider(rows[1].width)), rows[1]);
    render_transcript(frame, rows[2], model);
    frame.render_widget(Paragraph::new(divider(rows[3].width)), rows[3]);
    frame.render_widget(Paragraph::new(status_line(model)), rows[4]);
    render_prompt(frame, rows[5], model);
    frame.render_widget(Paragraph::new(footer_line(model)), rows[6]);

    // HUD corner brackets over the deck's rounded corners (echoes the web `.deck-corner` crop marks).
    draw_corners(frame, area);

    if model.show_help {
        render_help(frame, area);
    }
}

/// A centered keybinding overlay (F1). Clears its area first so it floats over the deck.
fn render_help(frame: &mut Frame, area: Rect) {
    let keys = [
        ("F2", "start / stop session"),
        ("F3", "mute / unmute mic"),
        ("F4", "share / stop screen"),
        ("Enter", "send message"),
        ("PgUp / PgDn", "scroll transcript"),
        ("F1 / Esc", "toggle help / clear"),
        ("Ctrl+C / Ctrl+Q", "quit"),
    ];
    let body: Vec<Line> = keys
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!(" {k:<17}"), Style::new().fg(theme::ACCENT)),
                Span::styled((*d).to_string(), Style::new().fg(theme::FG_DIM)),
            ])
        })
        .collect();

    let width = 46;
    let height = body.len() as u16 + 2;
    let rect = centered_rect(area, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::ACCENT))
        .style(Style::new().bg(theme::PANEL))
        .title_top(Line::from(" keys ").style(Style::new().fg(theme::FG_FAINT)));
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(body).block(block), rect);
}

/// A `w`×`h` rectangle centered within `area` (clamped to it).
fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

/// Overwrite the four deck corners with bracket glyphs in the accent color.
fn draw_corners(frame: &mut Frame, area: Rect) {
    let buf = frame.buffer_mut();
    let corners = [
        (area.left(), area.top(), "⌜"),
        (area.right().saturating_sub(1), area.top(), "⌝"),
        (area.left(), area.bottom().saturating_sub(1), "⌞"),
        (
            area.right().saturating_sub(1),
            area.bottom().saturating_sub(1),
            "⌟",
        ),
    ];
    for (x, y, sym) in corners {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_symbol(sym).set_fg(theme::ACCENT);
        }
    }
}

/// A full-width horizontal rule.
fn divider(width: u16) -> Line<'static> {
    Line::from("─".repeat(width as usize)).style(Style::new().fg(theme::LINE_SOFT))
}

/// The session controls row: F2 start/stop · F3 mute · F4 share, each styled by its state.
fn controls_line(model: &AppModel) -> Line<'static> {
    let running = model.is_running();

    let session = if running {
        Span::styled("F2 ■ stop", Style::new().fg(theme::DANGER))
    } else {
        Span::styled("F2 ▶ start", Style::new().fg(theme::ACCENT))
    };
    let mute = if model.mic_muted {
        Span::styled("F3 ⊘ muted", Style::new().fg(theme::DANGER))
    } else {
        Span::styled("F3 ● mic", Style::new().fg(theme::FG_DIM))
    };
    let share_color = if !running {
        theme::FG_FAINT
    } else if model.sharing {
        theme::ACCENT
    } else {
        theme::FG_DIM
    };
    let share = Span::styled(
        if model.sharing {
            "F4 ◉ sharing"
        } else {
            "F4 ▣ share"
        },
        Style::new().fg(share_color),
    );

    let sep = || Span::styled("    ", Style::new());
    Line::from(vec![session, sep(), mute, sep(), share])
}

/// Render the `❯` prompt with a horizontally-scrolling single line and the real terminal block
/// cursor (set via `set_cursor_position`) so it blinks natively at the caret.
fn render_prompt(frame: &mut Frame, area: Rect, model: &AppModel) {
    const CHEVRON_COLS: u16 = 2; // "❯ "
    if area.width <= CHEVRON_COLS {
        return;
    }
    let avail = (area.width - CHEVRON_COLS) as usize;
    let base = Style::new().bg(theme::PANEL);
    let chevron = Span::styled("❯ ", Style::new().fg(theme::ACCENT).bg(theme::PANEL));

    if model.input.is_empty() {
        let line = Line::from(vec![
            chevron,
            Span::styled("message JOI…", Style::new().fg(theme::FG_FAINT)),
        ]);
        frame.render_widget(Paragraph::new(line).style(base), area);
        frame.set_cursor_position((area.x + CHEVRON_COLS, area.y));
        return;
    }

    // Scroll horizontally just enough to keep the caret in view at the right edge.
    let caret_col = model.input.caret_display_col();
    let h_scroll = caret_col.saturating_sub(avail.saturating_sub(1));
    let visible = slice_by_cols(model.input.value(), h_scroll, avail);
    let line = Line::from(vec![
        chevron,
        Span::styled(visible, Style::new().fg(theme::FG)),
    ]);
    frame.render_widget(Paragraph::new(line).style(base), area);

    let cursor_x = area.x + CHEVRON_COLS + (caret_col - h_scroll) as u16;
    frame.set_cursor_position((cursor_x.min(area.x + area.width - 1), area.y));
}

/// The substring of `s` spanning display columns `[start, start + width)`. Wide chars straddling an
/// edge are dropped — fine for a prompt's horizontal scroll.
fn slice_by_cols(s: &str, start: usize, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut col = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        let next = col + w;
        if next <= start {
            col = next;
            continue;
        }
        if col >= start + width {
            break;
        }
        out.push(ch);
        col = next;
    }
    out
}

/// Width of the speaker-label column (`JOI:`/`User:` left-padded so the text after them aligns).
const LABEL_W: usize = 6;

/// Render the transcript, wrapped to width and anchored to the bottom (newest visible), offset by
/// `transcript_scroll` lines. Clamps the stored scroll against the real content height — the only
/// thing render mutates on the model.
fn render_transcript(frame: &mut Frame, area: Rect, model: &mut AppModel) {
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    // Pre-wrap every entry into display rows so we can slice an exact window (no reliance on
    // Paragraph's internal scroll/line-count). Layout per line: a fixed-width label column, then the
    // word-wrapped text. The speaker label (`JOI:`/`User:`) is shown only when the speaker *changes*
    // — consecutive lines from the same speaker are unlabeled — and wrapped/continuation lines are
    // indented to align under the text, never under the label.
    let indent = " ".repeat(LABEL_W);
    let text_width = width.saturating_sub(LABEL_W).max(1);
    let mut lines: Vec<Line> = Vec::new();
    let mut prev_kind: Option<LineKind> = None;
    for entry in model.transcript.entries() {
        let style = Style::new().fg(kind_color(entry.kind));
        let first_prefix = if prev_kind == Some(entry.kind) {
            indent.clone() // same speaker continuing — no repeated label
        } else {
            format!("{:<w$}", kind_label(entry.kind), w = LABEL_W)
        };
        prev_kind = Some(entry.kind);

        let wrapped = textwrap::wrap(&entry.text, text_width);
        if wrapped.is_empty() {
            lines.push(Line::from(first_prefix).style(style));
        } else {
            for (i, piece) in wrapped.iter().enumerate() {
                let prefix = if i == 0 { &first_prefix } else { &indent };
                lines.push(Line::from(format!("{prefix}{piece}")).style(style));
            }
        }
    }

    let total = lines.len();
    let max_scroll = total.saturating_sub(height);
    model.transcript_page = area.height;
    let scroll = (model.transcript_scroll as usize).min(max_scroll);
    model.transcript_scroll = scroll as u16;
    let start = max_scroll - scroll;

    let window: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    frame.render_widget(
        Paragraph::new(window).style(Style::new().bg(theme::PANEL)),
        area,
    );
}

fn kind_label(kind: LineKind) -> &'static str {
    match kind {
        LineKind::User => "User:",
        LineKind::Agent => "JOI:",
        LineKind::Error => "!",
    }
}

fn kind_color(kind: LineKind) -> Color {
    match kind {
        LineKind::User => theme::ACCENT,
        LineKind::Agent => theme::FG,
        LineKind::Error => theme::DANGER,
    }
}

fn brand_line() -> Line<'static> {
    Line::from(vec![
        Span::styled(
            " JOI ",
            Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "voice · screen companion ",
            Style::new().fg(theme::FG_FAINT),
        ),
    ])
}

/// The lifecycle status line (above the prompt): a glowing dot + state label, like the web
/// `tui-status`. The dot animates per state (see `theme::status_dot`); the label keeps the steady
/// state color.
fn status_line(model: &AppModel) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "●",
            Style::new().fg(theme::status_dot(model.state, model.tick)),
        ),
        Span::raw(" "),
        Span::styled(
            state_label(model.state),
            Style::new().fg(theme::state_color(model.state)),
        ),
    ])
}

/// The bottom rail: connection + uptime + live metrics, plus the no-key hint. Mirrors the web deck
/// footer (and surfaces the `Metrics` event the web UI doesn't show yet).
fn footer_line(model: &AppModel) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            "●",
            Style::new().fg(theme::connection_color(model.connection)),
        ),
        Span::raw(" "),
        Span::styled(
            connection_label(model.connection),
            Style::new().fg(theme::FG_FAINT),
        ),
        Span::styled("    ↑ ", Style::new().fg(theme::FG_FAINT)),
        Span::styled(format_uptime(model), Style::new().fg(theme::FG_DIM)),
    ];

    if let Some(m) = model.metrics.filter(|_| model.is_running()) {
        spans.push(Span::styled(
            format!(
                "    ↑{:.1} ↓{:.1} kb/s · {:.0} tok/s",
                m.up_kbps, m.down_kbps, m.tokens_per_sec
            ),
            Style::new().fg(theme::FG_FAINT),
        ));
    }
    if !model.has_key {
        spans.push(Span::styled(
            "    no API key — set GEMINI_API_KEY",
            Style::new().fg(theme::DANGER),
        ));
    }
    Line::from(spans)
}

/// `HH:MM:SS` since the session started, or `--:--:--` when stopped.
fn format_uptime(model: &AppModel) -> String {
    match model.started_at {
        Some(start) => {
            let secs = start.elapsed().as_secs();
            format!(
                "{:02}:{:02}:{:02}",
                secs / 3600,
                (secs % 3600) / 60,
                secs % 60
            )
        }
        None => "--:--:--".to_string(),
    }
}

fn state_label(state: AppState) -> &'static str {
    match state {
        AppState::Stopped => "stopped",
        AppState::Connecting => "connecting",
        AppState::Listening => "listening",
        AppState::Thinking => "thinking",
        AppState::Speaking => "speaking",
        AppState::Reconnecting => "reconnecting",
        AppState::Error => "error",
    }
}

fn connection_label(status: ConnectionStatus) -> &'static str {
    match status {
        ConnectionStatus::Disconnected => "disconnected",
        ConnectionStatus::Connecting => "connecting",
        ConnectionStatus::Connected => "connected",
        ConnectionStatus::Reconnecting => "reconnecting",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::app::AppModel;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use ratatui::Terminal;

    /// Render a model into a fixed-size off-screen buffer and flatten it to text for assertions.
    /// Tall enough that the chrome rows (controls, two dividers, status, prompt, footer) leave the
    /// transcript several visible lines.
    fn render_to_string(mut model: AppModel) -> String {
        let mut terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();
        terminal.draw(|f| render(f, &mut model)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(Cell::symbol)
            .collect()
    }

    #[test]
    fn frame_shows_brand_and_status() {
        let text = render_to_string(AppModel::new(true));
        assert!(text.contains("JOI"), "brand missing: {text}");
        assert!(text.contains("stopped"));
        assert!(text.contains("disconnected"));
        assert!(!text.contains("no API key"));
    }

    #[test]
    fn no_key_shows_banner() {
        let text = render_to_string(AppModel::new(false));
        assert!(text.contains("no API key"), "banner missing: {text}");
    }

    #[test]
    fn transcript_lines_render_with_labels() {
        let mut model = AppModel::new(true);
        model.transcript.push_transcript(
            joi_core::session::event::Speaker::Agent,
            "hello".into(),
            true,
        );
        let text = render_to_string(model);
        assert!(text.contains("JOI:"), "agent label missing: {text}");
        assert!(text.contains("hello"));
    }

    #[test]
    fn empty_prompt_shows_placeholder() {
        let text = render_to_string(AppModel::new(true));
        assert!(text.contains('❯'), "chevron missing: {text}");
        assert!(text.contains("message JOI"), "placeholder missing: {text}");
    }

    #[test]
    fn deck_has_corner_brackets_and_dividers() {
        let text = render_to_string(AppModel::new(true));
        assert!(
            text.contains('⌜') && text.contains('⌟'),
            "corners missing: {text}"
        );
        assert!(text.contains('─'), "divider rule missing: {text}");
    }

    #[test]
    fn consecutive_same_speaker_lines_are_labeled_once() {
        use joi_core::session::event::Speaker;
        let mut model = AppModel::new(true);
        model
            .transcript
            .push_transcript(Speaker::Agent, "first".into(), true);
        model
            .transcript
            .push_transcript(Speaker::Agent, "second".into(), true);
        model
            .transcript
            .push_transcript(Speaker::User, "hi".into(), true);
        let text = render_to_string(model);
        assert_eq!(
            text.matches("JOI:").count(),
            1,
            "JOI: should label once: {text}"
        );
        assert_eq!(
            text.matches("User:").count(),
            1,
            "User: should label once: {text}"
        );
        assert!(text.contains("second"), "continuation line missing: {text}");
    }

    #[test]
    fn prompt_renders_typed_text() {
        let mut model = AppModel::new(true);
        "hello world".chars().for_each(|c| model.input.insert(c));
        let text = render_to_string(model);
        assert!(text.contains('❯'));
        assert!(text.contains("hello world"), "typed text missing: {text}");
        assert!(!text.contains("message JOI"), "placeholder shown over text");
    }
}
