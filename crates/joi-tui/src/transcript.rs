//! The transcript buffer: folds the engine's streaming `Transcript`/`Error` events into a list of
//! speaker-labeled entries. Pure data + fold logic (no ratatui), so it's unit-tested directly.
//!
//! Fold rules (FR-3/FR-12): transcript text arrives as **incremental deltas**. A line stays "open"
//! while the same speaker streams — deltas append to it; a speaker change starts a new labeled line;
//! `final` commits (closes) the open line. An error closes any open line and appends its own line.

use joi_core::session::event::Speaker;

/// Hard cap on retained entries (oldest dropped) — the TUI's scrollback bound.
const MAX_ENTRIES: usize = 2000;

/// What a transcript line represents — drives its label and color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// The human user.
    User,
    /// The agent.
    Agent,
    /// A surfaced error.
    Error,
}

/// One transcript line: its kind and the accumulated text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: LineKind,
    pub text: String,
}

/// The streaming transcript. `open` is `Some(kind)` while the last entry is still accumulating
/// deltas for that speaker.
#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    open: Option<LineKind>,
}

impl Transcript {
    /// Fold one transcript delta. Appends to the open line, or opens a new labeled line on a speaker
    /// change; `final_` closes the line so the next delta starts fresh.
    pub fn push_transcript(&mut self, speaker: Speaker, text: String, final_: bool) {
        let kind = match speaker {
            Speaker::User => LineKind::User,
            Speaker::Agent => LineKind::Agent,
        };
        match self.entries.last_mut() {
            // Same speaker still streaming: append the delta to the open line.
            Some(last) if self.open == Some(kind) => last.text.push_str(&text),
            // Speaker changed (or first line / line was closed): open a new labeled line.
            _ => {
                self.start(Entry { kind, text });
                self.open = Some(kind);
            }
        }
        if final_ {
            self.open = None;
        }
    }

    /// Append a surfaced error as its own line, closing any open transcript line first.
    pub fn push_error(&mut self, message: String) {
        self.open = None;
        self.start(Entry {
            kind: LineKind::Error,
            text: message,
        });
    }

    fn start(&mut self, entry: Entry) {
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            let overflow = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..overflow);
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(t: &Transcript) -> Vec<(LineKind, &str)> {
        t.entries()
            .iter()
            .map(|e| (e.kind, e.text.as_str()))
            .collect()
    }

    #[test]
    fn deltas_append_into_one_open_line() {
        let mut t = Transcript::default();
        t.push_transcript(Speaker::Agent, "Hel".into(), false);
        t.push_transcript(Speaker::Agent, "lo".into(), true);
        assert_eq!(texts(&t), vec![(LineKind::Agent, "Hello")]);
    }

    #[test]
    fn speaker_change_starts_a_new_line() {
        let mut t = Transcript::default();
        t.push_transcript(Speaker::Agent, "hi".into(), false);
        t.push_transcript(Speaker::User, "yo".into(), true);
        assert_eq!(
            texts(&t),
            vec![(LineKind::Agent, "hi"), (LineKind::User, "yo")]
        );
    }

    #[test]
    fn error_closes_the_open_line_and_appends() {
        let mut t = Transcript::default();
        t.push_transcript(Speaker::Agent, "partial".into(), false);
        t.push_error("connect: boom".into());
        // A following agent delta must open a fresh line, not resume "partial".
        t.push_transcript(Speaker::Agent, "again".into(), true);
        assert_eq!(
            texts(&t),
            vec![
                (LineKind::Agent, "partial"),
                (LineKind::Error, "connect: boom"),
                (LineKind::Agent, "again"),
            ]
        );
    }

    #[test]
    fn scrollback_caps_oldest_entries() {
        let mut t = Transcript::default();
        for i in 0..(MAX_ENTRIES + 50) {
            t.push_error(format!("e{i}"));
        }
        assert_eq!(t.entries().len(), MAX_ENTRIES);
        assert_eq!(t.entries()[0].text, "e50"); // first 50 dropped
    }
}
