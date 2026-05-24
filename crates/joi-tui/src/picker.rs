//! The session picker: pure selection state over the list returned by
//! [`JoiApp::list_sessions`](joi_app::JoiApp::list_sessions). The host populates it (async I/O) and
//! reads the chosen id back; navigation/rendering are pure. New vs the old TUI, which had no picker
//! — `FR-20` requires listing and resuming sessions at runtime.

use joi_core::history::SessionSummary;

/// An open `/resume` picker: the session rows (newest-activity first) and the highlighted index.
pub struct Picker {
    sessions: Vec<SessionSummary>,
    selected: usize,
}

impl Picker {
    /// Build a picker over `sessions` (already newest-first from the store), selecting the first row.
    #[must_use]
    pub fn new(sessions: Vec<SessionSummary>) -> Self {
        Self {
            sessions,
            selected: 0,
        }
    }

    /// The rows to render, newest-activity first.
    #[must_use]
    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    /// Index of the highlighted row.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Whether the picker has nothing to show.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Move the highlight toward newer rows (up the visual list).
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight toward older rows (down the visual list), clamped to the last row.
    pub fn down(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1).min(self.sessions.len() - 1);
        }
    }

    /// The id of the highlighted session, or `None` when the list is empty.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.sessions.get(self.selected).map(|s| s.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use joi_core::history::SessionMeta;

    fn summary(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            meta: SessionMeta {
                name: Some(id.to_string()),
                created_at: 0,
                last_opened: 0,
                last_updated: 0,
            },
        }
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let mut p = Picker::new(vec![summary("a"), summary("b"), summary("c")]);
        assert_eq!(p.selected_id(), Some("a"));
        p.up(); // clamp at top
        assert_eq!(p.selected_id(), Some("a"));
        p.down();
        p.down();
        assert_eq!(p.selected_id(), Some("c"));
        p.down(); // clamp at bottom
        assert_eq!(p.selected_id(), Some("c"));
        p.up();
        assert_eq!(p.selected_id(), Some("b"));
    }

    #[test]
    fn empty_picker_has_no_selection() {
        let mut p = Picker::new(Vec::new());
        assert!(p.is_empty());
        assert_eq!(p.selected_id(), None);
        p.down(); // no panic on empty
        p.up();
        assert_eq!(p.selected_id(), None);
    }
}
