//! Rendering: turn an [`AppModel`] into ratatui widgets. Thin and logic-free — it only reads the
//! model and the wall clock. The deck frame (rounded border + brand/clock header) echoes the web
//! `.deck`; the transcript, prompt, controls, and footer fill in across M2–M5.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use joi_core::session::event::{AppState, ConnectionStatus, Reachability};

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
        .style(Style::new().bg(model.theme.background))
        .title_top(brand_line(model.theme.accent))
        .title_top(
            Line::from(format!(" {clock} "))
                .right_aligned()
                .style(Style::new().fg(theme::FG_DIM)),
        );
    let inner = deck.inner(area);
    frame.render_widget(deck, area);

    // Top→bottom: controls, divider, transcript (fills), JOI's status line, a blank breather, the
    // divider above the prompt, prompt, divider, footer. The status line sits with the transcript
    // (it's JOI's state, not the user's input); dividers fence off the prompt and the bottom rail.
    let rows = Layout::vertical([
        Constraint::Length(1), // controls
        Constraint::Length(1), // divider
        Constraint::Min(0),    // transcript
        Constraint::Length(1), // status line (JOI's)
        Constraint::Length(1), // blank line above the prompt's divider
        Constraint::Length(1), // divider
        Constraint::Length(1), // prompt
        Constraint::Length(1), // divider
        Constraint::Length(1), // footer (connection / uptime / metrics)
    ])
    .split(inner);
    frame.render_widget(Paragraph::new(controls_line(model)), rows[0]);
    frame.render_widget(Paragraph::new(divider(rows[1].width)), rows[1]);
    render_transcript(frame, rows[2], model);
    frame.render_widget(Paragraph::new(status_line(model)), rows[3]);
    // rows[4] is intentionally left blank (shows the background).
    frame.render_widget(Paragraph::new(divider(rows[5].width)), rows[5]);
    render_prompt(frame, rows[6], model);
    frame.render_widget(Paragraph::new(divider(rows[7].width)), rows[7]);
    frame.render_widget(Paragraph::new(footer_line(model)), rows[8]);

    // HUD corner brackets over the deck's rounded corners (echoes the web `.deck-corner` crop marks).
    draw_corners(frame, area, model.theme.accent);

    if model.show_help {
        render_help(frame, area, model.theme);
    }
}

/// A centered keybinding overlay (F1). Clears its area first so it floats over the deck.
fn render_help(frame: &mut Frame, area: Rect, theme: theme::Theme) {
    let keys = [
        ("F2", "start / stop session"),
        ("F3", "mute / unmute mic"),
        ("F4", "share / stop screen"),
        ("Enter", "send message"),
        ("PgUp / PgDn", "scroll (or mouse wheel)"),
        ("Home / End", "oldest / newest"),
        ("F1 / Esc", "toggle help / clear"),
        ("Ctrl+C / Ctrl+Q", "quit"),
    ];
    let body: Vec<Line> = keys
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!(" {k:<17}"), Style::new().fg(theme.accent)),
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
        .border_style(Style::new().fg(theme.accent))
        .style(Style::new().bg(theme.background))
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
fn draw_corners(frame: &mut Frame, area: Rect, accent: Color) {
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
            cell.set_symbol(sym).set_fg(accent);
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
        Span::styled("F2 ▶ start", Style::new().fg(model.theme.accent))
    };
    let mute = if model.mic_muted {
        Span::styled("F3 ⊘ muted", Style::new().fg(theme::DANGER))
    } else {
        Span::styled("F3 ● mic", Style::new().fg(theme::FG_DIM))
    };
    let share_color = if !running {
        theme::FG_FAINT
    } else if model.sharing {
        model.theme.accent
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
    let base = Style::new().bg(model.theme.background);
    let chevron = Span::styled(
        "❯ ",
        Style::new()
            .fg(model.theme.accent)
            .bg(model.theme.background),
    );

    // Scroll horizontally just enough to keep the caret in view at the right edge. An empty prompt
    // is just the chevron + the block cursor — no placeholder text.
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
    // Paragraph's internal scroll/line-count). No speaker labels — turns are distinguished by color
    // (see `kind_color`) and separated by a blank line whenever the speaker changes.
    let accent = model.theme.accent;
    let mut lines: Vec<Line> = Vec::new();
    let mut prev_kind: Option<LineKind> = None;
    for entry in model.transcript.entries() {
        if prev_kind.is_some_and(|p| p != entry.kind) {
            lines.push(Line::default()); // blank line between turns
        }
        prev_kind = Some(entry.kind);
        let style = Style::new().fg(kind_color(entry.kind, accent));
        for piece in textwrap::wrap(&entry.text, width) {
            lines.push(Line::from(piece.into_owned()).style(style));
        }
    }

    // Resolve the top visible line. Following pins to the bottom; otherwise the stored top is
    // clamped — and if it has reached the bottom, autoscroll re-engages. Keeping `transcript_top`
    // synced to `max_top` while following means a later up-scroll starts from the bottom.
    let total = lines.len();
    let max_top = total.saturating_sub(height);
    model.transcript_page = area.height;
    let top = if model.follow {
        max_top
    } else {
        let clamped = (model.transcript_top as usize).min(max_top);
        if clamped >= max_top {
            model.follow = true;
        }
        clamped
    };
    model.transcript_top = top as u16;

    let window: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
    frame.render_widget(
        Paragraph::new(window).style(Style::new().bg(model.theme.background)),
        area,
    );
}

fn kind_color(kind: LineKind, accent: Color) -> Color {
    match kind {
        LineKind::User => accent,
        LineKind::Agent => theme::FG,
        LineKind::Error => theme::DANGER,
    }
}

fn brand_line(accent: Color) -> Line<'static> {
    Line::from(Span::styled(
        " JOI ",
        Style::new().fg(accent).add_modifier(Modifier::BOLD),
    ))
}

/// The lifecycle status line (above the prompt): a glowing dot + state label, like the web
/// `tui-status`. The dot animates per state (see `theme::status_dot`); the label keeps the steady
/// state color.
fn status_line(model: &AppModel) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "●",
            Style::new().fg(theme::status_dot(
                model.state,
                model.tick,
                model.theme.accent,
            )),
        ),
        Span::raw(" "),
        Span::styled(
            state_label(model.state),
            Style::new().fg(theme::state_color(model.state, model.theme.accent)),
        ),
    ])
}

/// The bottom rail: a contextual dot (so it never just echoes the start/stop button) followed by
/// the uptime + throughput readouts, which are always shown — dashed placeholders when idle, real
/// figures once a session is live and samples arrive.
/// - no key → the setup hint;
/// - idle → provider **reachability** from the token-free probe (meaningful with no session);
/// - live session → the socket connection status.
fn footer_line(model: &AppModel) -> Line<'static> {
    if !model.has_key {
        return Line::from(Span::styled(
            "● no API key — set GEMINI_API_KEY",
            Style::new().fg(theme::DANGER),
        ));
    }

    let (dot_color, label) = if model.is_running() {
        (
            theme::connection_color(model.connection, model.theme.accent),
            connection_label(model.connection),
        )
    } else {
        reachability_display(model.reachability, model.theme.accent)
    };

    Line::from(vec![
        Span::styled("●", Style::new().fg(dot_color)),
        Span::raw(" "),
        Span::styled(label, Style::new().fg(theme::FG_FAINT)),
        Span::styled("    ↑ ", Style::new().fg(theme::FG_FAINT)),
        Span::styled(format_uptime(model), Style::new().fg(theme::FG_DIM)),
        Span::styled(
            format!("    {}", metrics_text(model)),
            Style::new().fg(theme::FG_FAINT),
        ),
    ])
}

/// Throughput readout — real figures when a sample is present, dashed placeholders otherwise.
fn metrics_text(model: &AppModel) -> String {
    match model.metrics {
        Some(m) => format!(
            "↑{:.1} ↓{:.1} kb/s · ↑{:.0} ↓{:.0} tok/s",
            m.up_kbps, m.down_kbps, m.up_tokens_per_sec, m.down_tokens_per_sec
        ),
        None => "↑--.- ↓--.- kb/s · ↑-- ↓-- tok/s".to_string(),
    }
}

/// Color + label for the idle reachability indicator.
fn reachability_display(reachability: Reachability, accent: Color) -> (Color, &'static str) {
    match reachability {
        Reachability::Online => (accent, "online"),
        Reachability::Checking => (theme::WARN, "checking…"),
        Reachability::Offline => (theme::DANGER, "offline"),
        Reachability::Unauthorized => (theme::DANGER, "key rejected"),
        Reachability::Unknown => (theme::FG_FAINT, "—"),
    }
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
        assert!(text.contains("stopped"), "lifecycle status missing: {text}");
        assert!(!text.contains("no API key"));
    }

    #[test]
    fn footer_shows_reachability_when_idle_and_connection_when_running() {
        // Idle (stopped) → the bottom dot reflects provider reachability, not "disconnected".
        let mut idle = AppModel::new(true);
        idle.reachability = Reachability::Online;
        let text = render_to_string(idle);
        assert!(
            text.contains("online"),
            "reachability missing when idle: {text}"
        );
        assert!(
            !text.contains("disconnected"),
            "should not show session conn when idle: {text}"
        );
        // Uptime + metrics readouts still show, with dashed placeholders when idle.
        assert!(
            text.contains("--:--:--"),
            "uptime placeholder missing: {text}"
        );
        assert!(
            text.contains("tok/s"),
            "metrics placeholder missing: {text}"
        );

        // Running → the bottom dot reflects the live session connection.
        let mut live = AppModel::new(true);
        live.state = AppState::Listening;
        live.connection = ConnectionStatus::Connected;
        live.reachability = Reachability::Online;
        let text = render_to_string(live);
        assert!(
            text.contains("connected"),
            "session connection missing when running: {text}"
        );
    }

    #[test]
    fn no_key_shows_banner() {
        let text = render_to_string(AppModel::new(false));
        assert!(text.contains("no API key"), "banner missing: {text}");
    }

    #[test]
    fn transcript_renders_text_without_labels() {
        let mut model = AppModel::new(true);
        model.transcript.push_transcript(
            joi_core::session::event::Speaker::Agent,
            "hello".into(),
            true,
        );
        let text = render_to_string(model);
        assert!(text.contains("hello"));
        assert!(!text.contains("JOI:"), "labels should be gone: {text}");
        assert!(!text.contains("User:"), "labels should be gone: {text}");
    }

    #[test]
    fn empty_prompt_is_just_the_chevron() {
        let text = render_to_string(AppModel::new(true));
        assert!(text.contains('❯'), "chevron missing: {text}");
        assert!(
            !text.contains("message JOI"),
            "placeholder should be gone: {text}"
        );
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
    fn blank_line_separates_turns_only() {
        use joi_core::session::event::Speaker;
        let (w, h) = (40u16, 16u16);
        let mut model = AppModel::new(true);
        model
            .transcript
            .push_transcript(Speaker::Agent, "alpha".into(), true);
        model
            .transcript
            .push_transcript(Speaker::Agent, "bravo".into(), true);
        model
            .transcript
            .push_transcript(Speaker::User, "charlie".into(), true);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, &mut model)).unwrap();
        let buf = terminal.backend().buffer();
        // Interior columns only (skip the left/right border) so a blank content row reads as empty.
        let rows: Vec<String> = (0..h)
            .map(|y| {
                (1..w - 1)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        let alpha = rows.iter().position(|r| r.contains("alpha")).unwrap();
        let bravo = rows.iter().position(|r| r.contains("bravo")).unwrap();
        let charlie = rows.iter().position(|r| r.contains("charlie")).unwrap();
        // Same speaker → adjacent, no blank between alpha and bravo.
        assert_eq!(
            bravo,
            alpha + 1,
            "same-speaker lines should be adjacent: {rows:?}"
        );
        // Speaker change → a blank line sits between bravo and charlie.
        assert!(
            charlie > bravo + 1 && rows[bravo + 1..charlie].iter().any(String::is_empty),
            "expected a blank line between turns: {rows:?}"
        );
        // No speaker labels anywhere.
        let all = rows.concat();
        assert!(
            !all.contains("JOI:") && !all.contains("User:"),
            "labels present: {all}"
        );
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
