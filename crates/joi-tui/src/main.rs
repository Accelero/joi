//! Native terminal-UI host for the JOI engine (Seam A). The only frontend built today (PLAN §2).
//! Unlike the headless integration test (which runs [`MediaMode::None`](joi_app::MediaMode::None)),
//! the TUI is a native process, so it drives the engine in
//! [`MediaMode::LocalDevices`](joi_app::MediaMode::LocalDevices): real cpal mic/playback + xcap
//! screen via `joi-media`. No audio crosses the TUI — ratatui only renders the text `UiEvent`
//! stream and dispatches commands.

mod app;
mod commands;
mod input;
mod keys;
mod picker;
mod theme;
mod transcript;
mod ui;

use std::ffi::OsStr;
use std::io::{self, Stdout};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, EventStream};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use joi_app::{JoiApp, MediaMode};
use joi_core::config::Config;
use joi_core::session::event::UiEvent;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;
use tokio::time::MissedTickBehavior;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

type Tui = Terminal<CrosstermBackend<Stdout>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load config *first*, before the terminal and stderr are taken over, so a config error (e.g.
    // no model set — Joi ships no default) prints plainly to the real stderr instead of vanishing
    // into the redirected log.
    let config = Config::load(None)?;

    // A ratatui app owns the alternate screen; any log line written to stdout/stderr would shred the
    // frame. So logging goes to a file for the whole life of the process.
    let (_guard, log_path) = init_logging()?;
    // Native audio/screen libraries (ALSA, libjack, PipeWire, cpal) write warnings straight to the
    // process's stderr (fd 2), bypassing Rust's logging. A GUI host never notices — they go to the
    // launching console — but for the TUI, stderr *is* the screen, so they shred the frame. Point
    // fd 2 at the log file for the whole run so the deck stays clean and the warnings are still kept.
    redirect_stderr_to(&log_path);
    tracing::info!(path = %log_path.display(), "joi-tui starting");

    let app = JoiApp::build(config, MediaMode::LocalDevices);
    let mut model = app::AppModel::new(app.has_api_key());
    // Resolve the configurable colors (background + accent) from the shared `ui.terminal` config.
    let ui = app.ui_config();
    model.theme = theme::Theme::from_config(&ui.terminal.background, &ui.terminal.accent);
    let mut events = app.subscribe_events();

    // From here on the terminal is in raw/alt-screen mode — restore it on *every* exit path,
    // including a panic, or the user's shell is left wrecked.
    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, &mut model, &app, &mut events).await;
    // Graceful shutdown: end the session before handing the terminal back.
    let _ = app.stop(false).await;
    restore_terminal()?;
    result
}

/// The render/event loop: multiplex terminal input, the engine's `UiEvent` stream, and an animation
/// tick; fold each into the model, run any resulting command against the engine, and redraw.
async fn run(
    terminal: &mut Tui,
    model: &mut app::AppModel,
    app: &JoiApp,
    events: &mut Option<Receiver<UiEvent>>,
) -> anyhow::Result<()> {
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    terminal.draw(|f| ui::render(f, &mut *model))?;
    loop {
        tokio::select! {
            maybe = input.next() => match maybe {
                Some(Ok(event)) => {
                    if let Some(command) = model.on_action(keys::map(&event)) {
                        run_command(app, model, command).await;
                    }
                }
                Some(Err(e)) => { tracing::warn!("terminal input error: {e}"); model.should_quit = true; }
                None => model.should_quit = true, // stdin closed
            },
            event = next_ui_event(events) => {
                if let Some(command) = model.on_ui_event(event) {
                    run_command(app, model, command).await;
                }
            }
            _ = tick.tick() => model.tick = model.tick.wrapping_add(1),
        }
        if model.should_quit {
            break;
        }
        terminal.draw(|f| ui::render(f, &mut *model))?;
    }
    Ok(())
}

/// Apply one model-emitted [`Command`](app::Command) to the engine. Engine errors are logged (not
/// fatal) — the session stays up and a `UiEvent::Error` would surface separately. The session
/// commands (`OpenPicker`/`ResumeSession`/`NewSession`) fold their async result back into the model.
async fn run_command(app: &JoiApp, model: &mut app::AppModel, command: app::Command) {
    use app::Command;
    match command {
        Command::Start => log_command_err("start", app.start(false).await),
        Command::Stop => log_command_err("stop", app.stop(false).await),
        Command::SendText(text) => log_command_err("send_text", app.send_text(&text).await),
        Command::SetMicMuted(muted) => app.set_mic_muted(muted),
        Command::StartScreenshare => app.start_screenshare(),
        Command::StopScreenshare => app.stop_screenshare(),
        Command::OpenPicker => {
            // Fetch the list *and* which session is current, so the picker can highlight the active
            // one and land the cursor on it.
            let sessions = app.list_sessions().await;
            let current_id = app.current_session().await.map(|s| s.id);
            model.open_picker(sessions, current_id);
        }
        Command::ResumeSession(id) => {
            // Retarget the store to the chosen session but do NOT auto-start: opening the API
            // stream (and its billing) stays a manual F2 action. The next start still seeds this
            // session's history, because the manager always re-seeds from the current store.
            match app.resume_session(&id).await {
                // Repopulate the transcript from the persisted turns so the user sees where they
                // left off (the model is re-seeded separately, on the next start).
                Ok(_) => match app.session_turns(&id).await {
                    Ok(turns) => model.load_history(turns),
                    Err(e) => tracing::warn!("session_turns failed: {e}"),
                },
                Err(e) => tracing::warn!("resume_session failed: {e}"),
            }
        }
        Command::NewSession => {
            if let Err(e) = app.new_session().await {
                tracing::warn!("new_session failed: {e}");
            }
        }
    }
}

fn log_command_err(what: &str, result: Result<(), joi_core::error::SessionError>) {
    if let Err(e) = result {
        tracing::warn!("{what} failed: {e}");
    }
}

/// Await the next `UiEvent`, transparently skipping broadcast lag and parking forever once the
/// stream is gone (or never existed — no API key) so it simply never wins the `select!`.
async fn next_ui_event(events: &mut Option<Receiver<UiEvent>>) -> UiEvent {
    loop {
        match events {
            Some(rx) => match rx.recv().await {
                Ok(event) => return event,
                Err(RecvError::Lagged(n)) => tracing::warn!("ui_event stream lagged {n}"),
                Err(RecvError::Closed) => *events = None,
            },
            None => std::future::pending::<()>().await,
        }
    }
}

fn setup_terminal() -> anyhow::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // A blinking block caret. ratatui shows it only on frames that set a cursor position (the prompt
    // does), so it appears exactly at the caret. Mouse capture lets the wheel scroll the transcript
    // (hold Shift for the terminal's native text selection).
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        SetCursorStyle::BlinkingBlock
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore_terminal() -> anyhow::Result<()> {
    crossterm::execute!(
        io::stdout(),
        SetCursorStyle::DefaultUserShape,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    disable_raw_mode()?;
    Ok(())
}

/// Restore the terminal before the default panic handler prints, so a panic doesn't leave the shell
/// in raw mode on the alternate screen.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original(info);
    }));
}

/// Initialize file-only logging (non-blocking) and return the worker guard (must be held for the
/// program's lifetime) plus the resolved log path.
fn init_logging() -> anyhow::Result<(WorkerGuard, PathBuf)> {
    let path = log_path();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("joi-tui.log"));
    std::fs::create_dir_all(dir)?;

    let appender = tracing_appender::rolling::never(dir, file);
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,joi=info")),
        )
        .init();
    Ok((guard, path))
}

/// Point the process's stderr (fd 2) at the log file for the whole run, so native-library warnings
/// (ALSA/libjack/PipeWire/cpal) that write directly to fd 2 land in the log instead of corrupting
/// the alternate screen. Best-effort: a failure here just leaves stderr as-is. Stdout (fd 1) is left
/// alone — that's where ratatui draws the deck.
fn redirect_stderr_to(path: &Path) {
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    // SAFETY: `file` owns a valid fd for the duration of the call; `dup2` duplicates it onto
    // STDERR_FILENO. We then leak `file` so its fd stays open for the process lifetime.
    unsafe {
        libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
    }
    std::mem::forget(file);
}

/// Resolve the log file: `$JOI_TUI_LOG` if set, else `<state-dir>/joi/joi-tui.log` (falling back to
/// the data dir, then the temp dir).
fn log_path() -> PathBuf {
    if let Ok(p) = std::env::var("JOI_TUI_LOG") {
        return PathBuf::from(p);
    }
    let base = match directories::ProjectDirs::from("", "", "joi") {
        Some(d) => d
            .state_dir()
            .unwrap_or_else(|| d.data_local_dir())
            .to_path_buf(),
        None => std::env::temp_dir(),
    };
    base.join("joi-tui.log")
}
