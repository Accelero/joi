//! Rendering: turn an [`AppModel`] into ratatui widgets. Thin and logic-free — it only reads the
//! model and the wall clock. The deck frame (rounded border + brand/clock header) wraps the
//! transcript, prompt, controls, and footer; overlays (help, the `/resume` picker) float on top.

use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use joi_core::session::event::{AppState, ConnectionStatus, Reachability};

use crate::app::AppModel;
use crate::commands;
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
    // A one-column gutter inside the border so content (messages, prompt, dividers) never touches
    // the frame. Vertical spacing is already provided by the border rows above/below.
    let inner = deck.inner(area).inner(Margin::new(1, 0));
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
    render_footer(frame, rows[8], model);

    // HUD corner brackets over the deck's rounded corners.
    draw_corners(frame, area, model.theme.accent);

    // The slash-command suggester floats just above the prompt while typing a `/` command — but not
    // behind another overlay.
    if model.picker.is_none() && model.voice_picker.is_none() && !model.show_help {
        render_suggester(frame, rows[6], model);
    }
    if model.show_help {
        render_help(frame, area, model.theme);
    }
    // A picker floats over everything else when open (only one is ever open at a time).
    if model.picker.is_some() {
        render_picker(frame, area, model);
    }
    if model.voice_picker.is_some() {
        render_voice_picker(frame, area, model);
    }
}

/// The slash-command autosuggest popup: a small list floating just above the prompt, listing the
/// commands whose body substring-matches the text typed after `/`. The highlighted row is in the
/// accent color (↑/↓ move, Tab completes, Enter runs it). Renders nothing unless the prompt is a
/// `/` command with at least one match.
fn render_suggester(frame: &mut Frame, prompt: Rect, model: &AppModel) {
    let matches = model.slash_suggestions();
    if matches.is_empty() {
        return;
    }
    let theme = model.theme;
    let selected = model.suggestion_cursor();

    let body: Vec<Line> = matches
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let on = i == selected;
            let marker = if on { "❯ " } else { "  " };
            let style = if on {
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::FG_DIM)
            };
            Line::from(vec![
                Span::styled(format!("{marker}{}", cmd.name), style),
                Span::styled(format!("  {}", cmd.help), Style::new().fg(theme::FG_FAINT)),
            ])
        })
        .collect();

    // Size to the content and float so the box's bottom sits just above the prompt row, left-aligned
    // with it. Clamp height so it never spills past the top of the deck.
    let content_w = matches
        .iter()
        .map(|c| c.name.len() + c.help.len() + 4)
        .max()
        .unwrap_or(20) as u16;
    let width = (content_w + 4).min(prompt.width.max(8));
    let height = (body.len() as u16 + 2).min(prompt.y.max(1));
    let rect = Rect {
        x: prompt.x,
        y: prompt.y.saturating_sub(height),
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .style(Style::new().bg(theme.background))
        .title_top(Line::from(" commands ").style(Style::new().fg(theme::FG_FAINT)));
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(body)
            .block(block)
            .style(Style::new().bg(theme.background)),
        rect,
    );
}

/// A centered keybinding overlay (F1). Clears its area first so it floats over the deck.
fn render_help(frame: &mut Frame, area: Rect, theme: theme::Theme) {
    let keys = [
        ("F2", "start / stop session"),
        ("F3", "mute / unmute mic"),
        ("F4", "share / stop screen"),
        ("Enter", "send message"),
        ("Tab", "complete a / command"),
        ("PgUp / PgDn", "scroll (or mouse wheel)"),
        ("Home / End", "oldest / newest"),
        ("F1 / Esc", "toggle help / clear"),
        ("Ctrl+C / Ctrl+Q", "quit"),
    ];
    // Keys first, then the slash commands from the shared catalog (one source of truth with the
    // prompt suggester).
    let mut body: Vec<Line> = keys
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!(" {k:<17}"), Style::new().fg(theme.accent)),
                Span::styled((*d).to_string(), Style::new().fg(theme::FG_DIM)),
            ])
        })
        .collect();
    for cmd in commands::SLASH_COMMANDS {
        body.push(Line::from(vec![
            Span::styled(format!(" {:<17}", cmd.name), Style::new().fg(theme.accent)),
            Span::styled(cmd.help.to_string(), Style::new().fg(theme::FG_DIM)),
        ]));
    }

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

/// The `/resume` session picker overlay: a centered list of sessions, newest-activity first. The
/// cursor row is in the accent color; the currently-active session is highlighted in `theme::CURRENT`
/// and tagged `● current` (the two can coincide). ↑/↓ move, Enter resumes, Esc cancels.
fn render_picker(frame: &mut Frame, area: Rect, model: &AppModel) {
    let Some(picker) = model.picker.as_ref() else {
        return;
    };
    let theme = model.theme;

    let body: Vec<Line> = if picker.is_empty() {
        vec![Line::from(Span::styled(
            " no saved sessions yet",
            Style::new().fg(theme::FG_FAINT),
        ))]
    } else {
        picker
            .sessions()
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let selected = i == picker.selected();
                // The session you're currently in — highlighted in its own color so it reads even
                // when the cursor is elsewhere.
                let current = picker.current_id() == Some(s.id.as_str());
                let marker = if selected { "❯ " } else { "  " };
                let name = s.meta.name.as_deref().unwrap_or("(unnamed session)");
                let when = format_when(s.meta.last_updated);
                // Cursor selection (accent) wins for the name color; an unselected current row still
                // stands out in the "current" color. The trailing tag marks the active session
                // unambiguously even when it is the selected row.
                let style = if selected {
                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
                } else if current {
                    Style::new().fg(theme::CURRENT)
                } else {
                    Style::new().fg(theme::FG_DIM)
                };
                let mut spans = vec![
                    Span::styled(format!("{marker}{name}"), style),
                    Span::styled(format!("  {when}"), Style::new().fg(theme::FG_FAINT)),
                ];
                if current {
                    spans.push(Span::styled("  ● current", Style::new().fg(theme::CURRENT)));
                }
                Line::from(spans)
            })
            .collect()
    };

    let width = 60.min(area.width);
    // Title + bottom hint + the rows, capped to the available height.
    let height = (body.len() as u16 + 3).min(area.height);
    let rect = centered_rect(area, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .style(Style::new().bg(theme.background))
        .title_top(Line::from(" resume session ").style(Style::new().fg(theme::FG_FAINT)))
        .title_bottom(
            Line::from(" ↑/↓ select · enter open · esc cancel ")
                .centered()
                .style(Style::new().fg(theme::FG_FAINT)),
        );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(body)
            .block(block)
            .style(Style::new().bg(theme.background)),
        rect,
    );
}

/// The `/voice` picker overlay: a centered list of the voices the provider offers. The cursor row is
/// in the accent color; the voice currently in use is highlighted in `theme::CURRENT` and tagged
/// `● current` (the two can coincide). ↑/↓ move, Enter applies, Esc cancels. The title notes that a
/// change takes effect on the next session start (the Voice setting's pinned `NextSession` timing).
fn render_voice_picker(frame: &mut Frame, area: Rect, model: &AppModel) {
    let Some(picker) = model.voice_picker.as_ref() else {
        return;
    };
    let theme = model.theme;

    let body: Vec<Line> = if picker.is_empty() {
        vec![Line::from(Span::styled(
            " no voices offered",
            Style::new().fg(theme::FG_FAINT),
        ))]
    } else {
        picker
            .voices()
            .iter()
            .enumerate()
            .map(|(i, voice)| {
                let selected = i == picker.selected();
                let current = picker.current() == Some(voice.as_str());
                let marker = if selected { "❯ " } else { "  " };
                // Cursor selection (accent) wins for the name color; the active voice still stands
                // out in its own color when the cursor is elsewhere.
                let style = if selected {
                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
                } else if current {
                    Style::new().fg(theme::CURRENT)
                } else {
                    Style::new().fg(theme::FG_DIM)
                };
                let mut spans = vec![Span::styled(format!("{marker}{voice}"), style)];
                if current {
                    spans.push(Span::styled("  ● current", Style::new().fg(theme::CURRENT)));
                }
                Line::from(spans)
            })
            .collect()
    };

    let width = 44.min(area.width);
    // Title + bottom hint + the rows, capped to the available height.
    let height = (body.len() as u16 + 3).min(area.height);
    let rect = centered_rect(area, width, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .style(Style::new().bg(theme.background))
        .title_top(
            Line::from(" voice · applies on next start ").style(Style::new().fg(theme::FG_FAINT)),
        )
        .title_bottom(
            Line::from(" ↑/↓ select · enter apply · esc cancel ")
                .centered()
                .style(Style::new().fg(theme::FG_FAINT)),
        );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(body)
            .block(block)
            .style(Style::new().bg(theme.background)),
        rect,
    );
}

/// Format a `last_updated` millis timestamp as a short local `MM-DD HH:MM`, or `—` if unparseable.
fn format_when(ms: u64) -> String {
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX);
    let nsec = ((ms % 1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nsec).map_or_else(
        || "—".to_string(),
        |dt| {
            chrono::DateTime::<chrono::Local>::from(dt)
                .format("%m-%d %H:%M")
                .to_string()
        },
    )
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

    // While an overlay picker is open the prompt is inert — don't steal the cursor into it.
    if model.picker.is_none() && model.voice_picker.is_none() {
        let cursor_x = area.x + CHEVRON_COLS + (caret_col - h_scroll) as u16;
        frame.set_cursor_position((cursor_x.min(area.x + area.width - 1), area.y));
    }
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
/// `transcript_top` lines. Clamps the stored scroll against the real content height — the only
/// thing render mutates on the model.
fn render_transcript(frame: &mut Frame, area: Rect, model: &mut AppModel) {
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    // Pre-wrap every entry into display rows so we can slice an exact window (no reliance on
    // Paragraph's internal scroll/line-count). No speaker labels — turns are distinguished by color
    // (see `kind_color`) and separated by a blank line between every turn (each `Entry` is one
    // turn — even back-to-back agent turns get their own breather).
    let accent = model.theme.accent;
    let mut lines: Vec<Line> = Vec::new();
    for (i, entry) in model.transcript.entries().iter().enumerate() {
        if i > 0 {
            lines.push(Line::default()); // blank line between turns
        }
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

/// The lifecycle status line (above the prompt): a glowing dot + state label. The dot animates per
/// state (see `theme::status_dot`); the label keeps the steady state color. When stopped there's no
/// live state to show, so the line is left blank — its row stays reserved by the layout so nothing
/// below it shifts.
fn status_line(model: &AppModel) -> Line<'static> {
    if model.state == AppState::Stopped {
        return Line::default();
    }
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

/// The bottom rail: [`footer_line`] (connection/reachability + uptime + throughput) fills from the
/// left, and the agent's current voice (`♪ <voice>`, the model default reading as `default`) sits
/// flush right. The voice gets exactly its own width so the left readouts keep the rest of the row
/// (and simply truncate before the voice on a narrow terminal). The trailing space keeps the voice
/// off the frame's right gutter.
fn render_footer(frame: &mut Frame, area: Rect, model: &AppModel) {
    use unicode_width::UnicodeWidthStr;

    let voice = model.voice.as_deref().unwrap_or("default");
    let voice_w = UnicodeWidthStr::width(format!("♪ {voice} ").as_str()) as u16;
    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(voice_w)]).split(area);
    frame.render_widget(Paragraph::new(footer_line(model)), cols[0]);
    let line = Line::from(vec![
        Span::styled("♪ ", Style::new().fg(theme::FG_FAINT)),
        Span::styled(format!("{voice} "), Style::new().fg(model.theme.accent)),
    ]);
    frame.render_widget(Paragraph::new(line), cols[1]);
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
    /// transcript several visible lines, and wide enough that the footer's left readouts and the
    /// right-aligned voice both fit without truncation.
    fn render_to_string(mut model: AppModel) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
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
        // Stopped: the brand shows, but the lifecycle status line is blank (no "stopped" label) —
        // its row is still reserved, just empty.
        let text = render_to_string(AppModel::new(true));
        assert!(text.contains("JOI"), "brand missing: {text}");
        assert!(
            !text.contains("stopped"),
            "stopped state should not be shown: {text}"
        );
        assert!(!text.contains("no API key"));

        // Running: the live state label appears.
        let mut live = AppModel::new(true);
        live.state = AppState::Listening;
        let text = render_to_string(live);
        assert!(text.contains("listening"), "live status missing: {text}");
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
    fn footer_shows_the_current_voice_on_the_right() {
        let mut m = AppModel::new(true);
        m.voice = Some("Charon".to_string());
        let text = render_to_string(m);
        assert!(text.contains("♪"), "voice marker missing: {text}");
        assert!(text.contains("Charon"), "voice name missing: {text}");

        // No configured voice reads as the model default.
        let text = render_to_string(AppModel::new(true));
        assert!(
            text.contains("default"),
            "default voice readout missing: {text}"
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
    fn picker_overlay_lists_sessions() {
        use joi_core::history::{SessionMeta, SessionSummary};
        let mut model = AppModel::new(true);
        model.open_picker(
            vec![SessionSummary {
                id: "abc".into(),
                meta: SessionMeta {
                    name: Some("morning chat".into()),
                    created_at: 0,
                    last_opened: 0,
                    last_updated: 0,
                },
            }],
            None,
        );
        let text = render_to_string(model);
        assert!(
            text.contains("resume session"),
            "picker title missing: {text}"
        );
        assert!(
            text.contains("morning chat"),
            "session name missing: {text}"
        );
    }

    #[test]
    fn picker_marks_the_current_session() {
        use joi_core::history::{SessionMeta, SessionSummary};
        let meta = |name: &str| SessionMeta {
            name: Some(name.into()),
            created_at: 0,
            last_opened: 0,
            last_updated: 0,
        };
        let mut model = AppModel::new(true);
        model.open_picker(
            vec![
                SessionSummary {
                    id: "new".into(),
                    meta: meta("newest"),
                },
                SessionSummary {
                    id: "active".into(),
                    meta: meta("the active one"),
                },
            ],
            Some("active".to_string()),
        );
        let text = render_to_string(model);
        // The active session carries the "current" tag; the other row does not.
        assert!(
            text.contains("current"),
            "active session not marked: {text}"
        );
    }

    #[test]
    fn voice_picker_overlay_lists_voices_and_marks_current() {
        use crate::picker::VoicePicker;
        let mut model = AppModel::new(true);
        model.open_voice_picker(VoicePicker::new(
            vec!["Aoede".into(), "Charon".into()],
            Some("Charon".into()),
        ));
        let text = render_to_string(model);
        assert!(text.contains("voice"), "picker title missing: {text}");
        assert!(
            text.contains("Aoede") && text.contains("Charon"),
            "voices missing: {text}"
        );
        assert!(text.contains("current"), "active voice not marked: {text}");
    }

    #[test]
    fn suggester_popup_lists_matching_commands() {
        // Typing a `/` floats the suggester above the prompt, listing matches from the catalog.
        let mut model = AppModel::new(true);
        "/".chars().for_each(|c| model.input.insert(c));
        let text = render_to_string(model);
        assert!(text.contains("commands"), "suggester title missing: {text}");
        assert!(text.contains("/resume"), "command missing: {text}");
    }

    #[test]
    fn suggester_hidden_without_a_slash_prompt() {
        let mut model = AppModel::new(true);
        "hi".chars().for_each(|c| model.input.insert(c));
        let text = render_to_string(model);
        assert!(
            !text.contains("commands"),
            "suggester should be closed: {text}"
        );
    }

    #[test]
    fn blank_line_separates_every_turn() {
        use joi_core::session::event::Speaker;
        let (w, h) = (40u16, 16u16);
        let mut model = AppModel::new(true);
        // Two back-to-back agent turns, then a user turn — every turn is its own `Entry`.
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
        // A blank line sits between every turn — including the two same-speaker agent turns.
        assert!(
            bravo > alpha + 1 && rows[alpha + 1..bravo].iter().any(String::is_empty),
            "expected a blank line between consecutive agent turns: {rows:?}"
        );
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
