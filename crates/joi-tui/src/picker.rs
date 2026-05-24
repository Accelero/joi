//! The session picker: pure selection state over the list returned by
//! [`JoiApp::list_sessions`](joi_app::JoiApp::list_sessions). The host populates it (async I/O) and
//! reads the chosen id back; navigation/rendering are pure. New vs the old TUI, which had no picker
//! — `FR-20` requires listing and resuming sessions at runtime.

use joi_core::history::SessionSummary;
use joi_core::settings::{SettingDescriptor, SettingId, SettingKind, SettingValue};

/// An open `/resume` picker: the session rows (newest-activity first), the highlighted index, and
/// the id of the currently-active session (rendered with a distinct highlight, see `ui::render_picker`).
pub struct Picker {
    sessions: Vec<SessionSummary>,
    selected: usize,
    current_id: Option<String>,
}

impl Picker {
    /// Build a picker over `sessions` (already newest-first from the store). The cursor starts on the
    /// currently-active session when it's in the list, so opening `/resume` lands you where you are;
    /// otherwise it starts at the first (newest) row.
    #[must_use]
    pub fn new(sessions: Vec<SessionSummary>, current_id: Option<String>) -> Self {
        let selected = current_id
            .as_deref()
            .and_then(|id| sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        Self {
            sessions,
            selected,
            current_id,
        }
    }

    /// The rows to render, newest-activity first.
    #[must_use]
    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    /// The id of the currently-active session, if any — the row to highlight as "current".
    #[must_use]
    pub fn current_id(&self) -> Option<&str> {
        self.current_id.as_deref()
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

/// An open `/voice` picker: the voices the provider offers (`SettingKind::Choice` order), the
/// highlighted index, and the currently-active voice (rendered with a distinct highlight, see
/// `ui::render_voice_picker`). A change applies on the next session start (`ApplyTiming::NextSession`).
pub struct VoicePicker {
    voices: Vec<String>,
    selected: usize,
    current: Option<String>,
}

impl VoicePicker {
    /// Build a picker over `voices`. The cursor starts on the active voice when it's in the list, so
    /// opening `/voice` lands you on the voice in use; otherwise it starts at the first row. An empty
    /// or unlisted `current` (the model-default voice) just selects the first row.
    #[must_use]
    pub fn new(voices: Vec<String>, current: Option<String>) -> Self {
        let selected = current
            .as_deref()
            .and_then(|c| voices.iter().position(|v| v == c))
            .unwrap_or(0);
        Self {
            voices,
            selected,
            current,
        }
    }

    /// Build a voice picker from the engine's settings schema: the [`SettingId::Voice`] descriptor's
    /// `Choice` options become the rows and its current value the highlight. Returns `None` if the
    /// schema has no voice choice (e.g. a provider without selectable voices), so the host shows
    /// nothing rather than an empty box.
    #[must_use]
    pub fn from_schema(schema: &[SettingDescriptor]) -> Option<Self> {
        let desc = schema.iter().find(|d| d.id == SettingId::Voice)?;
        let SettingKind::Choice { options } = &desc.kind else {
            return None;
        };
        Some(Self::new(options.clone(), current_voice(schema)))
    }

    /// The voices to render, in the provider's offered order.
    #[must_use]
    pub fn voices(&self) -> &[String] {
        &self.voices
    }

    /// The active voice, if any — the row to highlight as "current".
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Index of the highlighted row.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Whether the picker has nothing to show.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.voices.is_empty()
    }

    /// Move the highlight up the list.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight down the list, clamped to the last row.
    pub fn down(&mut self) {
        if !self.voices.is_empty() {
            self.selected = (self.selected + 1).min(self.voices.len() - 1);
        }
    }

    /// The highlighted voice, or `None` when the list is empty.
    #[must_use]
    pub fn selected_voice(&self) -> Option<&str> {
        self.voices.get(self.selected).map(String::as_str)
    }
}

/// The active voice from a settings schema — the [`SettingId::Voice`] descriptor's value, or `None`
/// for the model default (an empty value). Seeds and refreshes the footer's voice readout and the
/// voice picker's "current" highlight, so both agree on what's in use.
#[must_use]
pub fn current_voice(schema: &[SettingDescriptor]) -> Option<String> {
    schema
        .iter()
        .find(|d| d.id == SettingId::Voice)
        .and_then(|d| match &d.value {
            SettingValue::Text(v) if !v.is_empty() => Some(v.clone()),
            _ => None,
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
        let mut p = Picker::new(vec![summary("a"), summary("b"), summary("c")], None);
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
        let mut p = Picker::new(Vec::new(), None);
        assert!(p.is_empty());
        assert_eq!(p.selected_id(), None);
        p.down(); // no panic on empty
        p.up();
        assert_eq!(p.selected_id(), None);
    }

    #[test]
    fn cursor_starts_on_the_current_session() {
        // Opening /resume lands the cursor on the session you're already in, not the newest row.
        let p = Picker::new(
            vec![summary("a"), summary("b"), summary("c")],
            Some("b".to_string()),
        );
        assert_eq!(p.selected_id(), Some("b"));
        assert_eq!(p.current_id(), Some("b"));
    }

    #[test]
    fn current_id_absent_from_list_falls_back_to_first_row() {
        // A stale/unknown current id (or none) just selects the newest row.
        let p = Picker::new(vec![summary("a"), summary("b")], Some("gone".to_string()));
        assert_eq!(p.selected_id(), Some("a"));
    }

    fn voices() -> Vec<String> {
        vec![
            "Aoede".to_string(),
            "Charon".to_string(),
            "Puck".to_string(),
        ]
    }

    #[test]
    fn voice_navigation_clamps_at_both_ends() {
        let mut p = VoicePicker::new(voices(), None);
        assert_eq!(p.selected_voice(), Some("Aoede"));
        p.up(); // clamp at top
        assert_eq!(p.selected_voice(), Some("Aoede"));
        p.down();
        p.down();
        assert_eq!(p.selected_voice(), Some("Puck"));
        p.down(); // clamp at bottom
        assert_eq!(p.selected_voice(), Some("Puck"));
    }

    #[test]
    fn voice_cursor_starts_on_the_active_voice() {
        // Opening /voice lands the cursor on the voice in use, not the first row.
        let p = VoicePicker::new(voices(), Some("Charon".to_string()));
        assert_eq!(p.selected_voice(), Some("Charon"));
        assert_eq!(p.current(), Some("Charon"));
    }

    #[test]
    fn voice_default_or_unlisted_current_falls_back_to_first_row() {
        // The model-default voice (no active value) or an unknown one selects the first row.
        assert_eq!(
            VoicePicker::new(voices(), None).selected_voice(),
            Some("Aoede")
        );
        let p = VoicePicker::new(voices(), Some("Nope".to_string()));
        assert_eq!(p.selected_voice(), Some("Aoede"));
    }

    /// A Voice `Choice` descriptor as the engine's settings schema exposes it — built by hand so the
    /// frontend's extraction is tested independently of the core schema builder's signature.
    fn voice_descriptor(options: &[&str], value: &str) -> joi_core::settings::SettingDescriptor {
        use joi_core::settings::{ApplyTiming, SettingDescriptor, SettingKind, SettingValue};
        SettingDescriptor {
            id: SettingId::Voice,
            label: "Voice".to_string(),
            value: SettingValue::Text(value.to_string()),
            kind: SettingKind::Choice {
                options: options.iter().map(|s| (*s).to_string()).collect(),
            },
            apply: ApplyTiming::NextSession,
        }
    }

    #[test]
    fn from_schema_extracts_voice_options_and_current() {
        // The Voice choice's options become the rows and its value the highlight/selection.
        let schema = vec![voice_descriptor(&["Aoede", "Charon", "Puck"], "Charon")];
        let p = VoicePicker::from_schema(&schema).expect("voice choice present in schema");
        assert_eq!(p.voices(), ["Aoede", "Charon", "Puck"]);
        assert_eq!(p.current(), Some("Charon"));
        assert_eq!(
            p.selected_voice(),
            Some("Charon"),
            "cursor starts on the active voice"
        );
    }

    #[test]
    fn current_voice_is_none_for_the_model_default() {
        // An empty value means "model default" → no active voice to highlight.
        let schema = vec![voice_descriptor(&["Aoede", "Charon"], "")];
        assert_eq!(current_voice(&schema), None);
        assert_eq!(VoicePicker::from_schema(&schema).unwrap().current(), None);
    }
}
