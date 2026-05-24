//! The pure view-state model and its reducers. No IO and no ratatui types live here, so the
//! non-trivial state transitions are unit-testable without a terminal (PLAN §9 M6). The event loop
//! in `main.rs` owns all IO: it folds keystrokes into [`Action`]s and the engine's stream into
//! [`AppModel::on_ui_event`], runs any resulting [`Command`] against the engine, then renders.

use std::time::Instant;

use joi_core::metrics::MetricsSnapshot;
use joi_core::session::event::{AppState, ConnectionStatus, Reachability, Speaker, UiEvent};

use crate::input::Input;
use crate::picker::Picker;
use crate::theme::Theme;
use crate::transcript::Transcript;

/// Lines the transcript scrolls per mouse-wheel notch.
const WHEEL_LINES: u16 = 3;

/// A decoded user intent from a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Tear down and exit.
    Quit,
    /// Start or stop the session (F2).
    ToggleSession,
    /// Mute or unmute the mic (F3).
    ToggleMute,
    /// Start or stop screen-share (F4).
    ToggleShare,
    /// Scroll the transcript a page toward older lines (PageUp).
    ScrollUp,
    /// Scroll the transcript a page toward newer lines (PageDown).
    ScrollDown,
    /// Scroll the transcript a few lines toward older content (mouse wheel up).
    ScrollLineUp,
    /// Scroll the transcript a few lines toward newer content (mouse wheel down).
    ScrollLineDown,
    /// Jump to the oldest transcript line (Home).
    ScrollTop,
    /// Jump to the newest transcript line and resume autoscroll (End).
    ScrollBottom,
    /// Move the picker highlight up (↑) — ignored when no picker is open.
    Up,
    /// Move the picker highlight down (↓) — ignored when no picker is open.
    Down,
    /// Type a character into the prompt.
    Insert(char),
    /// Delete the char before the caret.
    Backspace,
    /// Delete the char at the caret.
    Delete,
    /// Move the caret one char left / right.
    Left,
    Right,
    /// Submit the prompt (Enter), or confirm the picker selection.
    Submit,
    /// Toggle the keybinding help overlay (F1).
    ToggleHelp,
    /// Close an open overlay (help / picker) if any, else clear the prompt.
    Escape,
    /// A key we don't act on.
    Ignore,
}

/// A side effect the loop runs against [`JoiApp`](joi_app::JoiApp). Keeping IO out of the model lets
/// the reducers stay pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Start (or resume) the session.
    Start,
    /// Stop (or pause) the session.
    Stop,
    /// Send a typed text turn to the model.
    SendText(String),
    /// Set app-level mic mute.
    SetMicMuted(bool),
    /// Start / stop native screen capture.
    StartScreenshare,
    StopScreenshare,
    /// Open the `/resume` picker: the loop fetches `list_sessions()` and populates it.
    OpenPicker,
    /// Resume the session with this id, then start it (the loop calls `resume_session` + `start`).
    ResumeSession(String),
    /// Switch to a brand-new session (the loop calls `new_session`).
    NewSession,
}

/// All the state the UI renders. Reducers mutate it; rendering only reads it (plus clamping the
/// transcript scroll against the live viewport, which it can only know at draw time).
// The flags (key present, mic muted, sharing, quit) are independent UI facts, not a state machine —
// folding them into enums would obscure rather than clarify.
#[allow(clippy::struct_excessive_bools)]
pub struct AppModel {
    /// Lifecycle/UI state (FR-4).
    pub state: AppState,
    /// Connection detail.
    pub connection: ConnectionStatus,
    /// Provider-API reachability, from the token-free background probe (independent of a session).
    pub reachability: Reachability,
    /// Whether an API key was configured at load (drives the no-key banner).
    pub has_key: bool,
    /// Configurable colors (background + accent), resolved from config at startup.
    pub theme: Theme,
    /// The streaming conversation transcript.
    pub transcript: Transcript,
    /// Autoscroll: when `true` the transcript stays pinned to the newest line as content streams.
    /// Scrolling up turns it off; scrolling back to the bottom turns it on again.
    pub follow: bool,
    /// Top visible transcript line (absolute), used when not following. Clamped at render time. A
    /// scrolled-up view stays put as new content arrives (it appends below, off-screen).
    pub transcript_top: u16,
    /// Transcript viewport height, learned at render time and used to size a page scroll.
    pub transcript_page: u16,
    /// The prompt's line editor.
    pub input: Input,
    /// App-level mic mute (mirrors the controls row mute button).
    pub mic_muted: bool,
    /// Whether screen-share is active.
    pub sharing: bool,
    /// The `/resume` session picker overlay, when open.
    pub picker: Option<Picker>,
    /// Latest throughput sample while a session is live (cleared on stop).
    pub metrics: Option<MetricsSnapshot>,
    /// When the session last entered a running state, for the uptime readout.
    pub started_at: Option<Instant>,
    /// Animation phase, bumped by the loop's render tick; drives the status dot.
    pub tick: u64,
    /// Whether the keybinding help overlay is shown.
    pub show_help: bool,
    /// Set by [`Action::Quit`]; the loop breaks when true.
    pub should_quit: bool,
}

impl AppModel {
    #[must_use]
    pub fn new(has_key: bool) -> Self {
        Self {
            state: AppState::Stopped,
            connection: ConnectionStatus::Disconnected,
            reachability: Reachability::Unknown,
            has_key,
            theme: Theme::default(),
            transcript: Transcript::default(),
            follow: true,
            transcript_top: 0,
            transcript_page: 0,
            input: Input::default(),
            mic_muted: false,
            sharing: false,
            picker: None,
            metrics: None,
            started_at: None,
            tick: 0,
            show_help: false,
            should_quit: false,
        }
    }

    /// Whether a session is live (text can be sent).
    #[must_use]
    pub fn is_running(&self) -> bool {
        !matches!(self.state, AppState::Stopped | AppState::Error)
    }

    /// Populate (open) the `/resume` picker with the session list the loop fetched. Called by the
    /// host after the async `list_sessions()` resolves — not by a pure reducer.
    pub fn open_picker(&mut self, sessions: Vec<joi_core::history::SessionSummary>) {
        self.picker = Some(Picker::new(sessions));
    }

    /// Reset the on-screen conversation when switching/resuming sessions: clear the transcript and
    /// re-pin autoscroll. (Resumed history re-seeds the *model*, not the terminal view.)
    pub fn reset_conversation_view(&mut self) {
        self.transcript = Transcript::default();
        self.follow = true;
        self.transcript_top = 0;
    }

    /// Fold a decoded key intent into the model, returning any side effect for the loop to run.
    /// Scroll offsets are clamped against the real content height at render time, so over-scroll
    /// here is harmless.
    pub fn on_action(&mut self, action: Action) -> Option<Command> {
        // When the picker is open it captures navigation/confirm/cancel; everything else is inert so
        // keystrokes don't leak into the prompt behind the overlay.
        if self.picker.is_some() {
            return self.on_picker_action(action);
        }
        let page = self.transcript_page.max(1);
        match action {
            Action::Quit => self.should_quit = true,
            Action::ToggleSession => {
                if self.is_running() {
                    return Some(Command::Stop);
                }
                // Starting needs a provider key — without one the engine would only error.
                return self.has_key.then_some(Command::Start);
            }
            Action::ToggleMute => {
                self.mic_muted = !self.mic_muted;
                return Some(Command::SetMicMuted(self.mic_muted));
            }
            Action::ToggleShare => {
                // Screen-share is only meaningful with a live session.
                if !self.is_running() {
                    return None;
                }
                self.sharing = !self.sharing;
                return Some(if self.sharing {
                    Command::StartScreenshare
                } else {
                    Command::StopScreenshare
                });
            }
            // Up-scrolls disengage autoscroll; down-scrolls move toward the bottom (render re-engages
            // follow once they reach it). render clamps `transcript_top` against the real height.
            Action::ScrollUp => self.scroll_up(page),
            Action::ScrollDown => self.transcript_top = self.transcript_top.saturating_add(page),
            Action::ScrollLineUp => self.scroll_up(WHEEL_LINES),
            Action::ScrollLineDown => {
                self.transcript_top = self.transcript_top.saturating_add(WHEEL_LINES);
            }
            Action::ScrollTop => self.scroll_up(u16::MAX),
            Action::ScrollBottom => self.follow = true,
            Action::Insert(c) => self.input.insert(c),
            Action::Backspace => self.input.backspace(),
            Action::Delete => self.input.delete(),
            Action::Left => self.input.left(),
            Action::Right => self.input.right(),
            Action::Submit => return self.submit(),
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::Escape => {
                if self.show_help {
                    self.show_help = false;
                } else {
                    self.input.clear();
                }
            }
            Action::Up | Action::Down | Action::Ignore => {}
        }
        None
    }

    /// Picker-mode reducer: ↑/↓ move the highlight, Enter resumes the selected session, Esc cancels.
    fn on_picker_action(&mut self, action: Action) -> Option<Command> {
        let picker = self.picker.as_mut()?;
        match action {
            Action::Up => picker.up(),
            Action::Down => picker.down(),
            Action::Submit => {
                let id = picker.selected_id().map(str::to_string);
                self.picker = None;
                if let Some(id) = id {
                    self.reset_conversation_view();
                    return Some(Command::ResumeSession(id));
                }
            }
            Action::Escape | Action::Quit => self.picker = None,
            _ => {}
        }
        None
    }

    /// Scroll the transcript up by `lines`, leaving autoscroll. `transcript_top` already tracks the
    /// bottom while following (render keeps it synced), so subtracting from it scrolls up correctly.
    fn scroll_up(&mut self, lines: u16) {
        self.follow = false;
        self.transcript_top = self.transcript_top.saturating_sub(lines);
    }

    /// Submit the prompt. A leading `/` is a local command, never sent to the model:
    /// `/exit`|`/quit`|`/q` quits; `/resume` opens the session picker; `/new` starts a fresh session.
    /// Otherwise echo the typed line into the transcript (the engine doesn't round-trip typed text,
    /// unlike spoken audio) and emit a [`Command::SendText`]; that path is a no-op unless a session
    /// is live and the line is non-empty.
    fn submit(&mut self) -> Option<Command> {
        let text = self.input.value().trim().to_string();
        match text.to_ascii_lowercase().as_str() {
            "/exit" | "/quit" | "/q" => {
                self.input.clear();
                self.should_quit = true;
                return None;
            }
            "/resume" => {
                self.input.clear();
                return Some(Command::OpenPicker);
            }
            "/new" => {
                self.input.clear();
                self.reset_conversation_view();
                return Some(Command::NewSession);
            }
            _ => {}
        }
        if !self.is_running() || text.is_empty() {
            return None;
        }
        self.input.clear();
        self.transcript
            .push_transcript(Speaker::User, text.clone(), true);
        Some(Command::SendText(text))
    }

    /// Fold one engine `UiEvent` into the model, returning any side effect for the loop to run.
    /// Owned payloads (transcript text, error strings) are moved out of the event, so it is taken by
    /// value.
    pub fn on_ui_event(&mut self, event: UiEvent) -> Option<Command> {
        match event {
            UiEvent::State { state } => {
                self.state = state;
                // A stopped/error session clears uptime and metrics, and tears down screen-share. The
                // engine's `stop` does *not* stop screenshare on its own, so we must emit
                // StopScreenshare here — otherwise a disconnect mid-share leaves the backend capturing
                // with no way to turn it off (the share toggle needs a live session).
                if matches!(state, AppState::Stopped | AppState::Error) {
                    self.started_at = None;
                    self.metrics = None;
                    if self.sharing {
                        self.sharing = false;
                        return Some(Command::StopScreenshare);
                    }
                } else {
                    self.started_at.get_or_insert_with(Instant::now);
                }
            }
            UiEvent::Connection { status, .. } => self.connection = status,
            UiEvent::Reachability { state, .. } => self.reachability = state,
            UiEvent::Transcript {
                speaker,
                text,
                final_,
            } => self.transcript.push_transcript(speaker, text, final_),
            // Only keep samples while a session is live. On stop the engine emits a trailing
            // `Metrics(ZERO)` to clear its own meter; ignoring it (the `State::Stopped` above already
            // nulled `metrics`) lets the footer fall back to the `--` N/A placeholders instead of a
            // literal `0.0`.
            UiEvent::Metrics(snapshot) => {
                if self.is_running() {
                    self.metrics = Some(snapshot);
                }
            }
            UiEvent::Error { kind, message } => {
                self.transcript.push_error(format!("{kind}: {message}"));
            }
            UiEvent::History(_) => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_sets_flag() {
        let mut m = AppModel::new(true);
        assert!(!m.should_quit);
        m.on_action(Action::Quit);
        assert!(m.should_quit);
    }

    #[test]
    fn ui_events_update_state_and_connection() {
        let mut m = AppModel::new(true);
        m.on_ui_event(UiEvent::State {
            state: AppState::Listening,
        });
        m.on_ui_event(UiEvent::Connection {
            status: ConnectionStatus::Connected,
            detail: None,
        });
        assert_eq!(m.state, AppState::Listening);
        assert_eq!(m.connection, ConnectionStatus::Connected);
    }

    #[test]
    fn submit_echoes_and_emits_command_when_running() {
        let mut m = AppModel::new(true);
        m.state = AppState::Listening; // running
        "hi".chars().for_each(|c| {
            m.on_action(Action::Insert(c));
        });
        let cmd = m.on_action(Action::Submit);
        assert_eq!(cmd, Some(Command::SendText("hi".to_string())));
        assert!(m.input.value().is_empty(), "input cleared after submit");
        assert_eq!(m.transcript.entries().len(), 1, "user line echoed");
    }

    #[test]
    fn exit_command_quits_without_sending() {
        // Works while live: quits the app and never round-trips a turn to the model.
        let mut m = AppModel::new(true);
        m.state = AppState::Listening;
        "/exit".chars().for_each(|c| {
            m.on_action(Action::Insert(c));
        });
        assert_eq!(m.on_action(Action::Submit), None, "no SendText is emitted");
        assert!(m.should_quit, "exit command quits");
        assert!(m.input.value().is_empty(), "prompt cleared");
        assert_eq!(m.transcript.entries().len(), 0, "nothing echoed");

        // Works while stopped too (no session needed), and is case-insensitive.
        let mut m = AppModel::new(true); // Stopped
        "/QUIT".chars().for_each(|c| {
            m.on_action(Action::Insert(c));
        });
        assert_eq!(m.on_action(Action::Submit), None);
        assert!(m.should_quit, "exit command quits even when stopped");
    }

    #[test]
    fn resume_command_opens_the_picker() {
        let mut m = AppModel::new(true);
        "/resume".chars().for_each(|c| {
            m.on_action(Action::Insert(c));
        });
        assert_eq!(m.on_action(Action::Submit), Some(Command::OpenPicker));
        assert!(m.input.value().is_empty(), "prompt cleared");
    }

    #[test]
    fn new_command_resets_view_and_requests_new_session() {
        let mut m = AppModel::new(true);
        m.transcript
            .push_transcript(Speaker::Agent, "old line".into(), true);
        "/new".chars().for_each(|c| {
            m.on_action(Action::Insert(c));
        });
        assert_eq!(m.on_action(Action::Submit), Some(Command::NewSession));
        assert_eq!(
            m.transcript.entries().len(),
            0,
            "view reset for new session"
        );
    }

    #[test]
    fn picker_navigates_and_resumes_selected() {
        use joi_core::history::{SessionMeta, SessionSummary};
        let summary = |id: &str| SessionSummary {
            id: id.to_string(),
            meta: SessionMeta {
                name: Some(id.to_string()),
                created_at: 0,
                last_opened: 0,
                last_updated: 0,
            },
        };
        let mut m = AppModel::new(true);
        m.open_picker(vec![summary("aaa"), summary("bbb")]);
        // While the picker is open, typing does not leak into the prompt.
        assert_eq!(m.on_action(Action::Insert('x')), None);
        assert!(m.input.value().is_empty());
        // ↓ then Enter resumes the second session and closes the picker.
        m.on_action(Action::Down);
        let cmd = m.on_action(Action::Submit);
        assert_eq!(cmd, Some(Command::ResumeSession("bbb".to_string())));
        assert!(m.picker.is_none(), "picker closed after selection");
    }

    #[test]
    fn picker_escape_cancels() {
        use joi_core::history::{SessionMeta, SessionSummary};
        let mut m = AppModel::new(true);
        m.open_picker(vec![SessionSummary {
            id: "x".into(),
            meta: SessionMeta {
                name: None,
                created_at: 0,
                last_opened: 0,
                last_updated: 0,
            },
        }]);
        assert_eq!(m.on_action(Action::Escape), None);
        assert!(m.picker.is_none(), "Esc closes the picker without resuming");
    }

    #[test]
    fn submit_is_noop_when_stopped() {
        let mut m = AppModel::new(true); // state defaults to Stopped
        "hi".chars().for_each(|c| {
            m.on_action(Action::Insert(c));
        });
        assert_eq!(m.on_action(Action::Submit), None);
        assert_eq!(m.input.value(), "hi", "text kept when not sendable");
        assert_eq!(m.transcript.entries().len(), 0);
    }

    #[test]
    fn toggle_session_maps_to_start_or_stop() {
        let mut m = AppModel::new(true);
        assert_eq!(m.on_action(Action::ToggleSession), Some(Command::Start));
        m.state = AppState::Listening;
        assert_eq!(m.on_action(Action::ToggleSession), Some(Command::Stop));
    }

    #[test]
    fn toggle_mute_flips_and_emits() {
        let mut m = AppModel::new(true);
        assert_eq!(
            m.on_action(Action::ToggleMute),
            Some(Command::SetMicMuted(true))
        );
        assert!(m.mic_muted);
        assert_eq!(
            m.on_action(Action::ToggleMute),
            Some(Command::SetMicMuted(false))
        );
        assert!(!m.mic_muted);
    }

    #[test]
    fn toggle_share_requires_a_live_session() {
        let mut m = AppModel::new(true);
        assert_eq!(m.on_action(Action::ToggleShare), None, "no-op when stopped");
        assert!(!m.sharing);
        m.state = AppState::Listening;
        assert_eq!(
            m.on_action(Action::ToggleShare),
            Some(Command::StartScreenshare)
        );
        assert!(m.sharing);
    }

    #[test]
    fn scrolling_up_leaves_follow_and_end_resumes_it() {
        let mut m = AppModel::new(true);
        assert!(m.follow, "starts pinned to the newest");
        m.on_action(Action::ScrollLineUp);
        assert!(!m.follow, "scrolling up stops autoscroll");
        m.on_action(Action::ScrollBottom); // End
        assert!(m.follow, "End resumes autoscroll");
        m.on_action(Action::ScrollTop); // Home
        assert!(
            !m.follow && m.transcript_top == 0,
            "Home jumps to the oldest line"
        );
    }

    #[test]
    fn no_key_blocks_start() {
        let mut m = AppModel::new(false);
        assert_eq!(m.on_action(Action::ToggleSession), None);
    }

    #[test]
    fn help_toggles_and_escape_closes_then_clears() {
        let mut m = AppModel::new(true);
        m.on_action(Action::ToggleHelp);
        assert!(m.show_help);
        m.on_action(Action::Escape); // closes help
        assert!(!m.show_help);
        "hi".chars().for_each(|c| {
            m.on_action(Action::Insert(c));
        });
        m.on_action(Action::Escape); // help closed → clears the prompt
        assert!(m.input.value().is_empty());
    }

    #[test]
    fn stopping_clears_uptime_share_and_metrics() {
        let mut m = AppModel::new(true);
        m.on_ui_event(UiEvent::State {
            state: AppState::Listening,
        });
        m.sharing = true;
        m.metrics = Some(MetricsSnapshot::ZERO);
        assert!(m.started_at.is_some());
        // Stopping while sharing must emit StopScreenshare so the backend capture actually ends.
        let cmd = m.on_ui_event(UiEvent::State {
            state: AppState::Stopped,
        });
        assert_eq!(cmd, Some(Command::StopScreenshare));
        assert!(m.started_at.is_none());
        assert!(!m.sharing);
        assert!(m.metrics.is_none());

        // The engine emits a trailing zero sample after stopping; it must NOT repopulate the
        // metrics — the footer should stay on its `--` N/A placeholders once idle.
        m.on_ui_event(UiEvent::Metrics(MetricsSnapshot::ZERO));
        assert!(
            m.metrics.is_none(),
            "trailing zero sample ignored when idle"
        );
    }
}
