//! Key-event → [`Action`] mapping. Kept separate from the model so the binding scheme is one
//! readable table. The input line is always focused (like the web prompt), so session controls use
//! function keys (added in M4); only the quit chords exist for now.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::Action;

/// Decode a crossterm event into an [`Action`]. Non-key and key-release events are ignored.
pub fn map(event: &Event) -> Action {
    let Event::Key(key) = event else {
        return Action::Ignore;
    };
    // Some terminals report key releases (kitty/enhanced mode); only act on presses.
    if key.kind == KeyEventKind::Release {
        return Action::Ignore;
    }
    map_key(key)
}

fn map_key(key: &KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // Ctrl+C / Ctrl+Q quit (avoid Ctrl+M — that's Enter in a terminal).
        KeyCode::Char('c' | 'q') if ctrl => Action::Quit,
        // Function keys never collide with typed text: F1 help, F2–F4 session controls.
        KeyCode::F(1) => Action::ToggleHelp,
        KeyCode::F(2) => Action::ToggleSession,
        KeyCode::F(3) => Action::ToggleMute,
        KeyCode::F(4) => Action::ToggleShare,
        KeyCode::Esc => Action::Escape,
        // Printable input: anything else without Ctrl/Alt (Shift is allowed, for capitals).
        KeyCode::Char(c) if !ctrl && !alt => Action::Insert(c),
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::Enter => Action::Submit,
        KeyCode::PageUp => Action::ScrollUp,
        KeyCode::PageDown => Action::ScrollDown,
        _ => Action::Ignore,
    }
}
