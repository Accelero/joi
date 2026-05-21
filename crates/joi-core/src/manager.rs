//! The [`SessionManager`] actor — the single owner of the live session (PLAN §1, §6).
//!
//! A single owning task holds the [`RealtimeSession`], the [`HistoryStore`], and the [`Config`],
//! and `select!`s over (a) inbound [`Command`]s on an `mpsc` and (b) the provider's
//! [`SessionEvent`] stream, mapping events to [`UiEvent`]s on a `broadcast` and appending finalized
//! transcripts to history. Callers hold only a cheap [`SessionManagerHandle`], so no shared
//! `&mut` to the session exists anywhere — this sidesteps the borrow problem (you cannot hold an
//! event stream *and* call `send_*` on the same `&mut` session).
//!
//! The provider's owned event receiver is **pumped** into an internal `mpsc` by a small forwarding
//! task, so the actor's `select!` reads owned values from two local receivers and its handlers
//! borrow `self` freely.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::clock::Clock;
use crate::config::Config;
use crate::error::SessionError;
use crate::history::{HistoryStore, HistoryTurn, Role, TokenBudget};
use crate::media::VideoFrame;
use crate::session::event::{AppState, CloseReason, ConnectionStatus, SessionEvent, Speaker};
use crate::session::{RealtimeSession, SessionConfig, UiEvent};

/// Capacity of the inbound command channel.
const COMMAND_CHANNEL: usize = 64;
/// Capacity of the UI-event broadcast.
const UI_CHANNEL: usize = 256;
/// Capacity of the internal provider-event and audio-output channels.
const MEDIA_CHANNEL: usize = 512;

/// Creates the [`RealtimeSession`] for a start/resume. Injected so the manager never names a
/// concrete provider (the composition root picks one by `config.provider.name`).
pub trait SessionFactory: Send + Sync {
    /// Build a fresh, unconnected session.
    fn create(&self) -> Box<dyn RealtimeSession>;
}

impl<F> SessionFactory for F
where
    F: Fn() -> Box<dyn RealtimeSession> + Send + Sync,
{
    fn create(&self) -> Box<dyn RealtimeSession> {
        (self)()
    }
}

/// A command sent to the [`SessionManager`] actor (mirrors the IPC commands in SPEC §11.1).
#[derive(Debug)]
pub enum Command {
    /// Start (`resume=false`) or resume (`resume=true`) a session.
    Start {
        /// Whether to seed restored context.
        resume: bool,
        /// Reply with the connect result.
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    /// Stop or pause the session (both fully disconnect — SPEC §5.3).
    Stop {
        /// `true` = pause (intent only; both close the socket).
        pause: bool,
        /// Reply once closed.
        reply: oneshot::Sender<()>,
    },
    /// Send typed text to the model.
    SendText {
        /// The text.
        text: String,
        /// Reply with the send result.
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    /// Stream a mic frame (16 kHz mono PCM16). Dropped at the source when muted (FR-6).
    SendAudio {
        /// The PCM frame.
        pcm: Vec<i16>,
    },
    /// Stream a screen frame.
    SendFrame {
        /// The encoded frame.
        frame: Box<VideoFrame>,
    },
    /// Mute/unmute the mic at the manager (the worklet is the primary gate — FR-6).
    SetMicMuted {
        /// Muted state.
        muted: bool,
    },
    /// Query the current [`AppState`].
    QueryState {
        /// Reply with the state.
        reply: oneshot::Sender<AppState>,
    },
    /// Stop and shut the actor down.
    Shutdown,
}

/// Cheap, cloneable handle to a running [`SessionManager`]. Held by the Tauri command layer.
#[derive(Clone)]
#[allow(clippy::struct_field_names)] // cmd_tx/ui_tx/audio_tx are distinct channels, not noise
pub struct SessionManagerHandle {
    cmd_tx: mpsc::Sender<Command>,
    ui_tx: broadcast::Sender<UiEvent>,
    audio_tx: broadcast::Sender<Vec<i16>>,
}

impl SessionManagerHandle {
    fn dead() -> SessionError {
        SessionError::Provider("session manager unavailable".to_string())
    }

    /// Subscribe to UI events (SPEC §11.3).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.ui_tx.subscribe()
    }

    /// Subscribe to audio-output frames (24 kHz mono PCM16). The single production consumer is the
    /// binary Channel forwarder (SPEC §11.2).
    #[must_use]
    pub fn subscribe_audio(&self) -> broadcast::Receiver<Vec<i16>> {
        self.audio_tx.subscribe()
    }

    /// Start or resume a session.
    pub async fn start(&self, resume: bool) -> Result<(), SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Start { resume, reply })
            .await
            .map_err(|_| Self::dead())?;
        rx.await.map_err(|_| Self::dead())?
    }

    /// Stop (`pause=false`) or pause (`pause=true`).
    pub async fn stop(&self, pause: bool) -> Result<(), SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Stop { pause, reply })
            .await
            .map_err(|_| Self::dead())?;
        rx.await.map_err(|_| Self::dead())
    }

    /// Send typed text.
    pub async fn send_text(&self, text: impl Into<String>) -> Result<(), SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::SendText {
                text: text.into(),
                reply,
            })
            .await
            .map_err(|_| Self::dead())?;
        rx.await.map_err(|_| Self::dead())?
    }

    /// Stream a mic frame.
    pub async fn send_audio(&self, pcm: Vec<i16>) -> Result<(), SessionError> {
        self.cmd_tx
            .send(Command::SendAudio { pcm })
            .await
            .map_err(|_| Self::dead())
    }

    /// Stream a screen frame.
    pub async fn send_frame(&self, frame: VideoFrame) -> Result<(), SessionError> {
        self.cmd_tx
            .send(Command::SendFrame {
                frame: Box::new(frame),
            })
            .await
            .map_err(|_| Self::dead())
    }

    /// Set the mic mute state.
    pub async fn set_mic_muted(&self, muted: bool) -> Result<(), SessionError> {
        self.cmd_tx
            .send(Command::SetMicMuted { muted })
            .await
            .map_err(|_| Self::dead())
    }

    /// Query the current state.
    pub async fn state(&self) -> Result<AppState, SessionError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::QueryState { reply })
            .await
            .map_err(|_| Self::dead())?;
        rx.await.map_err(|_| Self::dead())
    }
}

/// The actor. Owns the session, history, and config; serves commands from a single task.
pub struct SessionManager {
    config: Config,
    #[allow(dead_code)] // used once lifecycle timing/backoff lands (M3)
    clock: Arc<dyn Clock>,
    history: Arc<dyn HistoryStore>,
    factory: Arc<dyn SessionFactory>,
    ui_tx: broadcast::Sender<UiEvent>,
    audio_tx: broadcast::Sender<Vec<i16>>,
    ev_tx: mpsc::Sender<SessionEvent>,
    state: AppState,
    mic_muted: bool,
    last_resumption_handle: Option<String>,
}

impl SessionManager {
    /// Build the actor, spawn its task, and return a handle.
    pub fn spawn(
        config: Config,
        clock: Arc<dyn Clock>,
        history: Arc<dyn HistoryStore>,
        factory: Arc<dyn SessionFactory>,
    ) -> SessionManagerHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL);
        let (ui_tx, _) = broadcast::channel(UI_CHANNEL);
        let (audio_tx, _) = broadcast::channel(MEDIA_CHANNEL);
        let (ev_tx, ev_rx) = mpsc::channel(MEDIA_CHANNEL);

        let actor = SessionManager {
            config,
            clock,
            history,
            factory,
            ui_tx: ui_tx.clone(),
            audio_tx: audio_tx.clone(),
            ev_tx,
            state: AppState::Stopped,
            mic_muted: false,
            last_resumption_handle: None,
        };
        tokio::spawn(actor.run(cmd_rx, ev_rx));
        SessionManagerHandle {
            cmd_tx,
            ui_tx,
            audio_tx,
        }
    }

    /// The actor loop. `session` and the two receivers are locals, so command and event handlers
    /// borrow `self` without aliasing (the borrow-safe core of PLAN §6).
    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<Command>,
        mut ev_rx: mpsc::Receiver<SessionEvent>,
    ) {
        let mut session: Option<Box<dyn RealtimeSession>> = None;
        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        Some(cmd) => {
                            if self.handle_command(cmd, &mut session).await {
                                break;
                            }
                        }
                        None => break, // all handles dropped
                    }
                }
                Some(event) = ev_rx.recv() => {
                    self.handle_event(event).await;
                }
            }
        }
        if let Some(mut s) = session.take() {
            let _ = s.close().await;
        }
    }

    /// Returns `true` when the actor should shut down.
    async fn handle_command(
        &mut self,
        cmd: Command,
        session: &mut Option<Box<dyn RealtimeSession>>,
    ) -> bool {
        match cmd {
            Command::Start { resume, reply } => {
                let res = self.do_start(resume, session).await;
                let _ = reply.send(res);
            }
            Command::Stop { pause, reply } => {
                self.do_stop(session).await;
                tracing::info!(pause, "session stopped");
                let _ = reply.send(());
            }
            Command::SendText { text, reply } => {
                let res = match session.as_mut() {
                    Some(s) => s.send_text(&text).await,
                    None => Err(SessionError::NotConnected),
                };
                let _ = reply.send(res);
            }
            Command::SendAudio { pcm } => {
                if !self.mic_muted {
                    if let Some(s) = session.as_mut() {
                        if let Err(e) = s.send_audio(&pcm).await {
                            self.on_error("audio_send", &e);
                        }
                    }
                }
            }
            Command::SendFrame { frame } => {
                if let Some(s) = session.as_mut() {
                    if let Err(e) = s.send_video_frame(&frame).await {
                        self.on_error("frame_send", &e);
                    }
                }
            }
            Command::SetMicMuted { muted } => {
                self.mic_muted = muted;
            }
            Command::QueryState { reply } => {
                let _ = reply.send(self.state);
            }
            Command::Shutdown => {
                self.do_stop(session).await;
                return true;
            }
        }
        false
    }

    async fn do_start(
        &mut self,
        resume: bool,
        session: &mut Option<Box<dyn RealtimeSession>>,
    ) -> Result<(), SessionError> {
        self.do_stop(session).await; // idempotent: never leak a prior session

        self.set_state(AppState::Connecting);
        self.emit_connection(ConnectionStatus::Connecting, None);

        let initial_context = if resume {
            self.restore_context().await
        } else {
            Vec::new()
        };
        let cfg = SessionConfig::from_config(
            &self.config,
            initial_context,
            self.last_resumption_handle.clone(),
        );

        let mut new_session = self.factory.create();
        if let Err(e) = new_session.connect(cfg).await {
            self.on_error("connect", &e);
            self.set_state(AppState::Error);
            self.emit_connection(ConnectionStatus::Disconnected, Some(e.to_string()));
            return Err(e);
        }

        // Pump the owned provider stream into the actor's internal channel.
        let mut events = new_session.take_events();
        let ev_tx = self.ev_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if ev_tx.send(ev).await.is_err() {
                    break;
                }
            }
        });

        *session = Some(new_session);
        self.set_state(AppState::Listening);
        self.emit_connection(ConnectionStatus::Connected, None);
        Ok(())
    }

    async fn do_stop(&mut self, session: &mut Option<Box<dyn RealtimeSession>>) {
        if let Some(mut s) = session.take() {
            let _ = s.close().await;
        }
        if self.state != AppState::Stopped {
            self.set_state(AppState::Stopped);
            self.emit_connection(ConnectionStatus::Disconnected, None);
        }
    }

    /// Load persisted turns within budget, oldest-first, for re-seeding (SPEC §6.3).
    async fn restore_context(&self) -> Vec<HistoryTurn> {
        let budget = TokenBudget(self.config.history.token_budget);
        match self.history.load_within_budget(budget).await {
            Ok(mut turns) => {
                turns.reverse(); // store returns newest-first; seed chronologically
                turns
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to restore history; starting fresh");
                Vec::new()
            }
        }
    }

    async fn handle_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::AudioOutput { pcm } => {
                self.set_state(AppState::Speaking);
                let _ = self.audio_tx.send(pcm);
            }
            SessionEvent::Transcript {
                speaker,
                text,
                final_,
            } => {
                self.emit(UiEvent::Transcript {
                    speaker,
                    text: text.clone(),
                    final_,
                });
                if final_ {
                    self.append_turn(speaker, text).await;
                }
            }
            SessionEvent::TurnEvent(turn) => {
                use crate::session::event::TurnEvent;
                match turn {
                    TurnEvent::TurnStarted => self.set_state(AppState::Thinking),
                    // M2 adds barge-in playback flush on Interrupted; both return to listening.
                    TurnEvent::TurnComplete | TurnEvent::Interrupted => {
                        self.set_state(AppState::Listening);
                    }
                }
            }
            SessionEvent::SessionResumptionUpdate { handle } => {
                self.last_resumption_handle = Some(handle);
            }
            SessionEvent::Error(e) => {
                self.on_error("provider", &e);
                self.set_state(AppState::Error);
            }
            SessionEvent::Closed { reason } => {
                if reason != CloseReason::Client && self.state != AppState::Stopped {
                    // M3 will distinguish transient drops (-> Reconnecting); MVP stops cleanly.
                    self.set_state(AppState::Stopped);
                    self.emit_connection(ConnectionStatus::Disconnected, None);
                }
            }
            SessionEvent::ToolCall { .. } => {
                // [POST] No tool dispatch in the MVP (SPEC §10).
            }
        }
    }

    async fn append_turn(&self, speaker: Speaker, text: String) {
        let role = match speaker {
            Speaker::User => Role::User,
            Speaker::Agent => Role::Assistant,
        };
        let turn = HistoryTurn::new(role, text, self.clock.now_ms());
        if let Err(e) = self.history.append(turn).await {
            tracing::warn!(error = %e, "history append failed");
            return;
        }
        let budget = TokenBudget(self.config.history.token_budget);
        if let Ok(meta) = self.history.meta(budget).await {
            self.emit(UiEvent::History(meta));
        }
    }

    fn set_state(&mut self, state: AppState) {
        if self.state != state {
            self.state = state;
            self.emit(UiEvent::State { state });
        }
    }

    fn emit(&self, event: UiEvent) {
        let _ = self.ui_tx.send(event);
    }

    fn emit_connection(&self, status: ConnectionStatus, detail: Option<String>) {
        self.emit(UiEvent::Connection { status, detail });
    }

    fn on_error(&self, kind: &str, err: &SessionError) {
        tracing::warn!(kind, error = %err, "session error");
        self.emit(UiEvent::Error {
            kind: kind.to_string(),
            message: err.to_string(),
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::history::InMemoryHistory;
    use crate::session::event::{EventReceiver, EventSender, TurnEvent};
    use crate::session::Capabilities;
    use async_trait::async_trait;
    use std::time::Duration;
    use tokio::sync::broadcast::error::RecvError;

    /// A minimal scripted session: each `send_text`/`send_audio` pushes deterministic events.
    struct ScriptedSession {
        tx: Option<EventSender>,
        rx: Option<EventReceiver>,
    }

    impl ScriptedSession {
        fn new() -> Self {
            Self { tx: None, rx: None }
        }
    }

    #[async_trait]
    impl RealtimeSession for ScriptedSession {
        async fn connect(&mut self, _cfg: SessionConfig) -> Result<(), SessionError> {
            let (tx, rx) = mpsc::channel(64);
            self.tx = Some(tx);
            self.rx = Some(rx);
            Ok(())
        }

        async fn send_audio(&mut self, _pcm: &[i16]) -> Result<(), SessionError> {
            let tx = self.tx.as_ref().ok_or(SessionError::NotConnected)?;
            let _ = tx
                .send(SessionEvent::AudioOutput { pcm: vec![1, 2, 3] })
                .await;
            Ok(())
        }

        async fn send_video_frame(&mut self, _f: &VideoFrame) -> Result<(), SessionError> {
            Ok(())
        }

        async fn send_text(&mut self, text: &str) -> Result<(), SessionError> {
            let tx = self.tx.as_ref().ok_or(SessionError::NotConnected)?.clone();
            tx.send(SessionEvent::TurnEvent(TurnEvent::TurnStarted))
                .await
                .ok();
            tx.send(SessionEvent::Transcript {
                speaker: Speaker::User,
                text: text.to_string(),
                final_: true,
            })
            .await
            .ok();
            tx.send(SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text: format!("echo: {text}"),
                final_: true,
            })
            .await
            .ok();
            tx.send(SessionEvent::TurnEvent(TurnEvent::TurnComplete))
                .await
                .ok();
            Ok(())
        }

        fn take_events(&mut self) -> EventReceiver {
            self.rx
                .take()
                .expect("take_events called once after connect")
        }

        async fn close(&mut self) -> Result<(), SessionError> {
            self.tx = None; // ends the pump
            Ok(())
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                session_resumption: false,
                native_screen_input: false,
                async_tool_calls: false,
            }
        }
    }

    fn spawn_manager() -> (SessionManagerHandle, Arc<InMemoryHistory>) {
        let history = Arc::new(InMemoryHistory::new());
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(TestClock::new(1_000)),
            history.clone(),
            Arc::new(|| Box::new(ScriptedSession::new()) as Box<dyn RealtimeSession>),
        );
        (handle, history)
    }

    async fn next_ui(rx: &mut broadcast::Receiver<UiEvent>) -> UiEvent {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(ev)) => return ev,
                Ok(Err(RecvError::Lagged(_))) => {}
                other => panic!("expected a UI event, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn start_running_stop_transitions() {
        let (handle, _history) = spawn_manager();
        let mut ui = handle.subscribe();

        handle.start(false).await.unwrap();
        // Connecting -> Listening
        assert_eq!(handle.state().await.unwrap(), AppState::Listening);

        handle.stop(false).await.unwrap();
        assert_eq!(handle.state().await.unwrap(), AppState::Stopped);

        // We should have observed at least one State event.
        let mut saw_state = false;
        while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(50), ui.recv()).await {
            if matches!(ev, UiEvent::State { .. }) {
                saw_state = true;
            }
        }
        assert!(saw_state);
    }

    #[tokio::test]
    async fn text_turn_emits_transcripts_and_appends_history() {
        let (handle, history) = spawn_manager();
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();
        handle.send_text("hi").await.unwrap();

        let mut user_seen = false;
        let mut agent_seen = false;
        let mut history_seen = false;
        for _ in 0..20 {
            match next_ui(&mut ui).await {
                UiEvent::Transcript {
                    speaker: Speaker::User,
                    text,
                    final_: true,
                } if text == "hi" => {
                    user_seen = true;
                }
                UiEvent::Transcript {
                    speaker: Speaker::Agent,
                    text,
                    final_: true,
                } if text == "echo: hi" => {
                    agent_seen = true;
                }
                UiEvent::History(meta) if meta.turns >= 2 => history_seen = true,
                _ => {}
            }
            if user_seen && agent_seen && history_seen {
                break;
            }
        }
        assert!(user_seen, "user transcript not observed");
        assert!(agent_seen, "agent transcript not observed");
        assert!(history_seen, "history meta not observed");

        let stored = history
            .load_within_budget(TokenBudget(10_000))
            .await
            .unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[tokio::test]
    async fn sends_and_events_interleave_without_deadlock() {
        // Proves the actor model: we can send while events stream back, concurrently.
        let (handle, _history) = spawn_manager();
        let mut audio = handle.subscribe_audio();
        handle.start(false).await.unwrap();

        for _ in 0..5 {
            handle.send_audio(vec![0; 320]).await.unwrap();
        }
        handle.send_text("concurrent").await.unwrap();

        // Audio output frames produced by the scripted session arrive on the audio channel.
        let mut frames = 0;
        while frames < 5 {
            match tokio::time::timeout(Duration::from_secs(2), audio.recv()).await {
                Ok(Ok(_)) => frames += 1,
                Ok(Err(RecvError::Lagged(n))) => frames += n as usize,
                other => panic!("expected audio frame, got {other:?}"),
            }
        }
        assert!(frames >= 5);
        handle.stop(false).await.unwrap();
    }

    #[tokio::test]
    async fn muted_mic_drops_audio_at_manager() {
        let (handle, _history) = spawn_manager();
        let mut audio = handle.subscribe_audio();
        handle.start(false).await.unwrap();
        handle.set_mic_muted(true).await.unwrap();
        handle.send_audio(vec![0; 320]).await.unwrap();

        // No AudioOutput should be produced because the send was dropped at the manager.
        let got = tokio::time::timeout(Duration::from_millis(200), audio.recv()).await;
        assert!(got.is_err(), "expected no audio when muted, got {got:?}");
        handle.stop(false).await.unwrap();
    }
}
