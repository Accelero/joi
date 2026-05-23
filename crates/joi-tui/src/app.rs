//! The pure view-state model and its reducers. No IO and no ratatui types live here, so the
//! non-trivial state transitions are unit-testable without a terminal (PLAN-TUI §3). The event loop
//! in `main.rs` owns all IO: it folds keystrokes into [`Action`]s and the engine's stream into
//! [`AppModel::on_ui_event`], then renders the model.

use std::time::Instant;

use joi_core::metrics::MetricsSnapshot;
use joi_core::session::event::{AppState, ConnectionStatus, Speaker, UiEvent};

use crate::input::Input;
use crate::transcript::Transcript;

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
    /// Scroll the transcript toward older lines (PageUp).
    ScrollUp,
    /// Scroll the transcript toward newer lines (PageDown).
    ScrollDown,
    /// Type a character into the prompt.
    Insert(char),
    /// Delete the char before the caret.
    Backspace,
    /// Delete the char at the caret.
    Delete,
    /// Move the caret one char left / right.
    Left,
    Right,
    /// Jump to start / end of the current line.
    Home,
    End,
    /// Submit the prompt (Enter).
    Submit,
    /// Toggle the keybinding help overlay (F1).
    ToggleHelp,
    /// Close the help overlay if open, else clear the prompt.
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
    /// Whether an API key was configured at load (drives the no-key banner).
    pub has_key: bool,
    /// The streaming conversation transcript.
    pub transcript: Transcript,
    /// Lines scrolled up from the bottom; `0` means pinned to the latest (autoscroll).
    pub transcript_scroll: u16,
    /// Transcript viewport height, learned at render time and used to size a page scroll.
    pub transcript_page: u16,
    /// The prompt's line editor.
    pub input: Input,
    /// App-level mic mute (mirrors `Controls` mute button).
    pub mic_muted: bool,
    /// Whether screen-share is active.
    pub sharing: bool,
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
            has_key,
            transcript: Transcript::default(),
            transcript_scroll: 0,
            transcript_page: 0,
            input: Input::default(),
            mic_muted: false,
            sharing: false,
            metrics: None,
            started_at: None,
            tick: 0,
            show_help: false,
            should_quit: false,
        }
    }

    /// Whether a session is live (text can be sent), mirroring the web `running` flag.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !matches!(self.state, AppState::Stopped | AppState::Error)
    }

    /// Fold a decoded key intent into the model, returning any side effect for the loop to run.
    /// Scroll offsets are clamped against the real content height at render time, so over-scroll
    /// here is harmless.
    pub fn on_action(&mut self, action: Action) -> Option<Command> {
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
                // Screen-share is only meaningful with a live session (mirrors the disabled web
                // button when stopped).
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
            Action::ScrollUp => {
                self.transcript_scroll = self.transcript_scroll.saturating_add(page);
            }
            Action::ScrollDown => {
                self.transcript_scroll = self.transcript_scroll.saturating_sub(page);
            }
            Action::Insert(c) => self.input.insert(c),
            Action::Backspace => self.input.backspace(),
            Action::Delete => self.input.delete(),
            Action::Left => self.input.left(),
            Action::Right => self.input.right(),
            Action::Home => self.input.home(),
            Action::End => self.input.end(),
            Action::Submit => return self.submit(),
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::Escape => {
                if self.show_help {
                    self.show_help = false;
                } else {
                    self.input.clear();
                }
            }
            Action::Ignore => {}
        }
        None
    }

    /// Submit the prompt: echo the typed line into the transcript (the engine doesn't round-trip
    /// typed text, unlike spoken audio) and emit a [`Command::SendText`]. No-op unless a session is
    /// live and the line is non-empty — mirroring the web prompt's `canSend` gate.
    fn submit(&mut self) -> Option<Command> {
        if !self.is_running() {
            return None;
        }
        let text = self.input.value().trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.input.clear();
        self.transcript
            .push_transcript(Speaker::User, text.clone(), true);
        Some(Command::SendText(text))
    }

    /// Fold one engine `UiEvent` into the model. Owned payloads (transcript text, error strings) are
    /// moved out of the event, so it is taken by value.
    pub fn on_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::State { state } => {
                self.state = state;
                // Mirror App.tsx: a stopped/error session clears uptime, share, and metrics; a
                // running one starts the uptime clock (once).
                if matches!(state, AppState::Stopped | AppState::Error) {
                    self.started_at = None;
                    self.sharing = false;
                    self.metrics = None;
                } else {
                    self.started_at.get_or_insert_with(Instant::now);
                }
            }
            UiEvent::Connection { status, .. } => self.connection = status,
            UiEvent::Transcript {
                speaker,
                text,
                final_,
            } => self.transcript.push_transcript(speaker, text, final_),
            UiEvent::Metrics(snapshot) => self.metrics = Some(snapshot),
            UiEvent::Error { kind, message } => {
                self.transcript.push_error(format!("{kind}: {message}"));
            }
            UiEvent::History(_) => {}
        }
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
        assert!(m.input.is_empty(), "input cleared after submit");
        assert_eq!(m.transcript.entries().len(), 1, "user line echoed");
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
        assert!(m.input.is_empty());
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
        m.on_ui_event(UiEvent::State {
            state: AppState::Stopped,
        });
        assert!(m.started_at.is_none());
        assert!(!m.sharing);
        assert!(m.metrics.is_none());
    }
}
