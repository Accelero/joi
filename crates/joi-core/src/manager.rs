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
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot, Notify};

use crate::clock::{Clock, UnixMillis};
use crate::config::Config;
use crate::connectivity::ConnectivityProbe;
use crate::error::SessionError;
use crate::history::{HistoryStore, HistoryTurn, Role, TokenBudget};
use crate::media::VideoFrame;
use crate::metrics::{
    bytes_to_kbps, MetricsSnapshot, ThroughputMeter, TransportBytes, SAMPLE_INTERVAL,
};
use crate::session::event::{AppState, CloseReason, ConnectionStatus, SessionEvent, Speaker};
use crate::session::{RealtimeSession, SessionConfig, UiEvent};

/// Bytes per PCM16 sample — used to weigh audio frames into the throughput meter.
const BYTES_PER_SAMPLE: u64 = 2;

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
    /// Pokes the reachability monitor to probe now (host `check_reachability`). `None` when no
    /// probe is wired (no API key / a provider without a probe). See [`crate::connectivity`].
    probe_trigger: Option<Arc<Notify>>,
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

    /// Trigger an immediate reachability probe (the result arrives as a `UiEvent::Reachability` on
    /// the event stream). No-op when no probe is wired. Non-blocking and infallible by design — it
    /// just nudges the background monitor.
    pub fn check_reachability(&self) {
        if let Some(trigger) = &self.probe_trigger {
            trigger.notify_one();
        }
    }
}

/// The actor. Owns the session, history, and config; serves commands from a single task.
pub struct SessionManager {
    config: Config,
    clock: Arc<dyn Clock>,
    history: Arc<dyn HistoryStore>,
    factory: Arc<dyn SessionFactory>,
    ui_tx: broadcast::Sender<UiEvent>,
    audio_tx: broadcast::Sender<Vec<i16>>,
    ev_tx: mpsc::Sender<SessionEvent>,
    state: AppState,
    mic_muted: bool,
    last_resumption_handle: Option<String>,
    /// Accumulates the current transcript line's incremental deltas so the full line can be
    /// appended to history when it finalizes (the provider streams deltas; SPEC §11.3).
    transcript_line: String,
    /// Rolling payload byte/token tallies, drained into a `UiEvent::Metrics` each `SAMPLE_INTERVAL`.
    /// Supplies the token rate always, and the up/down rates when the provider can't report wire bytes.
    meter: ThroughputMeter,
    /// Clock time of the last metrics sample, so the next rate covers the real elapsed window.
    last_sample_ms: UnixMillis,
    /// Provider wire-byte totals at the previous sample, differenced to get the per-window rate.
    /// `None` until the first sample of a connection (and after stop) so we don't report a spike.
    last_transport: Option<TransportBytes>,
    /// Pokes the reachability monitor to re-probe (e.g. right after a connect failure, so the dot
    /// updates without waiting for the next poll). `None` when no probe is wired.
    probe_trigger: Option<Arc<Notify>>,
}

impl SessionManager {
    /// Build the actor, spawn its task, and return a handle.
    ///
    /// `probe` is the provider's token-free reachability check (injected, like `factory`, so the
    /// engine never names a provider). When `Some`, a background [reachability
    /// monitor](crate::connectivity::spawn_monitor) is spawned that emits `UiEvent::Reachability`;
    /// when `None` (no API key, or a provider without a probe) no monitor runs.
    pub fn spawn(
        config: Config,
        clock: Arc<dyn Clock>,
        history: Arc<dyn HistoryStore>,
        factory: Arc<dyn SessionFactory>,
        probe: Option<Arc<dyn ConnectivityProbe>>,
    ) -> SessionManagerHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL);
        let (ui_tx, _) = broadcast::channel(UI_CHANNEL);
        let (audio_tx, _) = broadcast::channel(MEDIA_CHANNEL);
        let (ev_tx, ev_rx) = mpsc::channel(MEDIA_CHANNEL);

        // Wire the reachability monitor only when a probe exists. The shared `Notify` lets both a
        // host (`check_reachability`) and the actor (on connect failure) ask for an immediate probe.
        let probe_trigger = probe.map(|probe| {
            let trigger = Arc::new(Notify::new());
            crate::connectivity::spawn_monitor(
                ui_tx.clone(),
                probe,
                Duration::from_secs(config.live_api.reachability_probe_secs),
                Arc::clone(&trigger),
            );
            trigger
        });

        let last_sample_ms = clock.now_ms();
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
            transcript_line: String::new(),
            meter: ThroughputMeter::default(),
            last_sample_ms,
            last_transport: None,
            probe_trigger: probe_trigger.clone(),
        };
        tokio::spawn(actor.run(cmd_rx, ev_rx));
        SessionManagerHandle {
            cmd_tx,
            ui_tx,
            audio_tx,
            probe_trigger,
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
        let mut metrics_tick = tokio::time::interval(SAMPLE_INTERVAL);
        // Don't pile up catch-up ticks if the actor was busy; one late tick is enough.
        metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                _ = metrics_tick.tick() => {
                    self.sample_metrics(session.as_deref());
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
                    Some(s) => {
                        self.meter.add_up(text.len() as u64);
                        s.send_text(&text).await
                    }
                    None => Err(SessionError::NotConnected),
                };
                let _ = reply.send(res);
            }
            Command::SendAudio { pcm } => {
                if !self.mic_muted {
                    // Borrow ends before any teardown so we can drop the dead session below.
                    let result = match session.as_mut() {
                        Some(s) => {
                            self.meter.add_up(pcm.len() as u64 * BYTES_PER_SAMPLE);
                            Some(s.send_audio(&pcm).await)
                        }
                        None => None,
                    };
                    if let Some(Err(e)) = result {
                        // A send failure means the socket is gone; stop cleanly rather than logging
                        // an error per 20 ms frame. (M3 will distinguish transient drops → reconnect.)
                        self.on_error("audio_send", &e);
                        self.do_stop(session).await;
                        self.set_state(AppState::Error);
                    }
                }
            }
            Command::SendFrame { frame } => {
                if let Some(s) = session.as_mut() {
                    self.meter.add_up(frame.data.len() as u64);
                    if let Err(e) = s.send_video_frame(&frame).await {
                        self.on_error("frame_send", &e);
                    }
                }
            }
            Command::SetMicMuted { muted } => {
                // On the mute transition, tell the provider the audio stream paused so it finalizes
                // the current turn and stops expecting audio — cleaner than streaming silence (no
                // bandwidth/tokens, and the model's output doesn't degrade). Unmuting needs no
                // signal: the next mic frame reopens the stream server-side. A failure here is
                // non-fatal (the connection stays up), so just log it rather than surfacing an error.
                if muted && !self.mic_muted {
                    if let Some(s) = session.as_mut() {
                        if let Err(e) = s.end_audio_stream().await {
                            tracing::warn!(error = %e, "audio_stream_end failed");
                        }
                    }
                }
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
            // A connect failure is fresh evidence about reachability — re-probe now so the status
            // updates immediately rather than at the next poll (distinguishes offline vs. bad key).
            if let Some(trigger) = &self.probe_trigger {
                trigger.notify_one();
            }
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
        // Start the throughput window fresh so the first sample measures from connect, not from a
        // possibly-long idle gap since the last session.
        self.meter.reset();
        self.last_sample_ms = self.clock.now_ms();
        self.last_transport = None;
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
            // Clear the live indicator: drop any partial window and emit one zero sample.
            self.meter.reset();
            self.last_transport = None;
            self.emit(UiEvent::Metrics(MetricsSnapshot::ZERO));
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
                self.meter.add_down(pcm.len() as u64 * BYTES_PER_SAMPLE);
                self.set_state(AppState::Speaking);
                let _ = self.audio_tx.send(pcm);
            }
            SessionEvent::Transcript {
                speaker,
                text,
                final_,
            } => {
                // Transcripts arrive from the provider (down); the agent's words also feed the
                // token-rate estimate (the user's input transcription does not — it's not output).
                self.meter.add_down(text.len() as u64);
                if speaker == Speaker::Agent {
                    self.meter.add_output_text(&text);
                }
                // `text` is an incremental delta: accumulate the line for history, forward the
                // delta to the UI (which appends it). On finalize, persist the whole line.
                self.transcript_line.push_str(&text);
                self.emit(UiEvent::Transcript {
                    speaker,
                    text,
                    final_,
                });
                if final_ {
                    let line = std::mem::take(&mut self.transcript_line);
                    self.append_turn(speaker, line).await;
                }
            }
            SessionEvent::TurnEvent(turn) => {
                use crate::session::event::TurnEvent;
                match turn {
                    TurnEvent::TurnStarted => self.set_state(AppState::Thinking),
                    TurnEvent::TurnComplete => self.set_state(AppState::Listening),
                    TurnEvent::Interrupted => {
                        // Barge-in (FR-2): tell the playback sink to drop queued agent audio now,
                        // via an empty-frame sentinel on the audio channel (TurnComplete must NOT
                        // flush, or it would clip the reply's tail).
                        let _ = self.audio_tx.send(Vec::new());
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

    /// Drain the throughput window into a `UiEvent::Metrics` (called on each `SAMPLE_INTERVAL`
    /// tick). No-op while stopped — the zero sample emitted at stop already cleared the indicator,
    /// and we keep the window/clock fresh so the first post-start sample is accurate.
    ///
    /// The token rate always comes from the meter. For up/down bytes we prefer the provider's
    /// **wire** counters (actual WS frame payload — base64+JSON) when it reports them, differencing
    /// successive totals over the elapsed window; otherwise we fall back to the meter's
    /// payload-level estimate (e.g. providers/test doubles that don't measure their socket).
    fn sample_metrics(&mut self, session: Option<&dyn RealtimeSession>) {
        if self.state == AppState::Stopped {
            self.meter.reset();
            self.last_transport = None;
            self.last_sample_ms = self.clock.now_ms();
            return;
        }
        let now = self.clock.now_ms();
        let elapsed = Duration::from_millis(now.saturating_sub(self.last_sample_ms));
        let Some(mut snapshot) = self.meter.sample(elapsed) else {
            return; // no time elapsed (e.g. a frozen test clock); keep counters for the next tick
        };

        // Prefer the provider's wire-byte counters, differencing successive totals over the window.
        if let Some(current) = session.and_then(RealtimeSession::transport_bytes) {
            let secs = elapsed.as_secs_f64();
            // The first sample of a connection has no baseline; treat `prev == current` so it
            // reports zero rather than a spike from the connect/setup handshake.
            let prev = self.last_transport.unwrap_or(current);
            snapshot.up_kbps = bytes_to_kbps(current.sent.saturating_sub(prev.sent), secs);
            snapshot.down_kbps =
                bytes_to_kbps(current.received.saturating_sub(prev.received), secs);
            self.last_transport = Some(current);
        }

        self.last_sample_ms = now;
        self.emit(UiEvent::Metrics(snapshot));
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]
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
    /// `closed` counts `close()` calls so tests can prove Stop tears the session down.
    /// Externally-controlled `(sent, received)` wire-byte totals a test can drive directly.
    type WireCounters = (std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64);

    struct ScriptedSession {
        tx: Option<EventSender>,
        rx: Option<EventReceiver>,
        closed: Arc<std::sync::atomic::AtomicUsize>,
        /// Counts `end_audio_stream()` calls so a test can prove mute pauses the provider stream.
        audio_ended: Arc<std::sync::atomic::AtomicUsize>,
        /// When set, the session reports these as `transport_bytes` (the wire-metering path).
        transport: Option<Arc<WireCounters>>,
    }

    impl ScriptedSession {
        fn new() -> Self {
            Self::with_close_tracker(Arc::new(std::sync::atomic::AtomicUsize::new(0)))
        }

        fn with_close_tracker(closed: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            Self {
                tx: None,
                rx: None,
                closed,
                audio_ended: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                transport: None,
            }
        }

        fn with_audio_end_tracker(audio_ended: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            Self {
                audio_ended,
                ..Self::new()
            }
        }

        fn with_transport(transport: Arc<WireCounters>) -> Self {
            Self {
                transport: Some(transport),
                ..Self::new()
            }
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

        async fn end_audio_stream(&mut self) -> Result<(), SessionError> {
            self.audio_ended
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            // Agent reply streamed as incremental deltas, then committed with an empty-text final —
            // mirrors the real provider path (the manager accumulates the line for history).
            for delta in ["ec", "ho: ", text] {
                tx.send(SessionEvent::Transcript {
                    speaker: Speaker::Agent,
                    text: delta.to_string(),
                    final_: false,
                })
                .await
                .ok();
            }
            tx.send(SessionEvent::Transcript {
                speaker: Speaker::Agent,
                text: String::new(),
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
            self.closed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                session_resumption: false,
                native_screen_input: false,
                async_tool_calls: false,
            }
        }

        fn transport_bytes(&self) -> Option<TransportBytes> {
            use std::sync::atomic::Ordering;
            self.transport.as_ref().map(|c| TransportBytes {
                sent: c.0.load(Ordering::SeqCst),
                received: c.1.load(Ordering::SeqCst),
            })
        }
    }

    fn spawn_manager() -> (SessionManagerHandle, Arc<InMemoryHistory>) {
        let (handle, history, _clock) = spawn_manager_with_clock();
        (handle, history)
    }

    fn spawn_manager_with_clock() -> (SessionManagerHandle, Arc<InMemoryHistory>, TestClock) {
        let history = Arc::new(InMemoryHistory::new());
        let clock = TestClock::new(1_000);
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(clock.clone()),
            history.clone(),
            Arc::new(|| Box::new(ScriptedSession::new()) as Box<dyn RealtimeSession>),
            None,
        );
        (handle, history, clock)
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

    async fn next_metrics(rx: &mut broadcast::Receiver<UiEvent>) -> MetricsSnapshot {
        loop {
            if let UiEvent::Metrics(m) = next_ui(rx).await {
                return m;
            }
        }
    }

    #[tokio::test]
    async fn stop_closes_the_session_so_billing_ends() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let closed = Arc::new(AtomicUsize::new(0));
        let factory_closed = Arc::clone(&closed);
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(TestClock::new(1_000)),
            Arc::new(InMemoryHistory::new()),
            Arc::new(move || {
                Box::new(ScriptedSession::with_close_tracker(Arc::clone(
                    &factory_closed,
                ))) as Box<dyn RealtimeSession>
            }),
            None,
        );

        handle.start(false).await.unwrap();
        assert_eq!(
            closed.load(Ordering::SeqCst),
            0,
            "session closed while running"
        );

        handle.stop(false).await.unwrap();
        // Stop must close the provider session (the WebSocket) so the provider stops billing.
        assert_eq!(
            closed.load(Ordering::SeqCst),
            1,
            "Stop must close the session"
        );
        assert_eq!(handle.state().await.unwrap(), AppState::Stopped);
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
        let mut agent_text = String::new();
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
                // Agent line arrives as incremental deltas; the full text is their concatenation.
                UiEvent::Transcript {
                    speaker: Speaker::Agent,
                    text,
                    final_,
                } => {
                    agent_text.push_str(&text);
                    if final_ && agent_text == "echo: hi" {
                        agent_seen = true;
                    }
                }
                UiEvent::History(meta) if meta.turns >= 2 => history_seen = true,
                _ => {}
            }
            if user_seen && agent_seen && history_seen {
                break;
            }
        }
        assert!(user_seen, "user transcript not observed");
        assert!(agent_seen, "agent transcript not assembled from deltas");
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
    async fn metrics_sample_reports_upstream_throughput() {
        // Send mic audio, advance the clock a full window, and prove the periodic sample surfaces
        // a non-zero up rate as a `UiEvent::Metrics` the frontend can render.
        let (handle, _history, clock) = spawn_manager_with_clock();
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();
        for _ in 0..5 {
            handle.send_audio(vec![0; 320]).await.unwrap();
        }
        // One second of clock time elapses before the next real interval tick fires sample_metrics.
        clock.advance(1_000);

        let mut up = None;
        for _ in 0..50 {
            if let UiEvent::Metrics(m) = next_ui(&mut ui).await {
                if m.up_kbps > 0.0 {
                    up = Some(m);
                    break;
                }
            }
        }
        let snap = up.expect("a non-zero up-rate metrics sample");
        // 5 frames * 320 samples * 2 bytes = 3200 bytes over ~1 s.
        assert!(snap.up_kbps > 0.0, "up rate should be positive: {snap:?}");
        handle.stop(false).await.unwrap();
    }

    #[tokio::test]
    async fn metrics_prefer_provider_wire_bytes_over_payload_estimate() {
        // The provider reports growing wire totals while *no* mic audio is sent, so a positive
        // down rate can only come from the wire counters — proving they override the payload meter.
        use std::sync::atomic::{AtomicU64, Ordering};
        let wire = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
        let clock = TestClock::new(1_000);
        let factory_wire = Arc::clone(&wire);
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(clock.clone()),
            Arc::new(InMemoryHistory::new()),
            Arc::new(move || {
                Box::new(ScriptedSession::with_transport(Arc::clone(&factory_wire)))
                    as Box<dyn RealtimeSession>
            }),
            None,
        );
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();

        // Phase 1: advance a window so one sample establishes the baseline (no traffic -> zero).
        clock.advance(1_000);
        let baseline = next_metrics(&mut ui).await;
        assert_eq!(baseline.down_kbps, 0.0, "first sample is the baseline");

        // Phase 2: grow only the *received* wire total; the next sample must show a positive down
        // rate even though no SendAudio ever ran (so it can't come from the payload meter).
        wire.1.store(2_000, Ordering::SeqCst);
        clock.advance(1_000);
        let mut down = 0.0;
        for _ in 0..50 {
            let m = next_metrics(&mut ui).await;
            if m.down_kbps > 0.0 {
                down = m.down_kbps;
                break;
            }
        }
        // 2000 bytes over ~1 s = 16 kbit/s.
        assert!(down > 0.0, "wire down rate should be positive, got {down}");
        handle.stop(false).await.unwrap();
    }

    #[tokio::test]
    async fn stop_emits_zero_metrics_to_clear_the_indicator() {
        let (handle, _history, _clock) = spawn_manager_with_clock();
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();
        handle.stop(false).await.unwrap();

        let mut saw_zero = false;
        while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(100), ui.recv()).await {
            if let UiEvent::Metrics(m) = ev {
                if m == MetricsSnapshot::ZERO {
                    saw_zero = true;
                }
            }
        }
        assert!(saw_zero, "stop must emit a zero metrics sample");
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

    #[tokio::test]
    async fn mute_pauses_provider_audio_stream_once_per_transition() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let audio_ended = Arc::new(AtomicUsize::new(0));
        let factory_ended = Arc::clone(&audio_ended);
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(TestClock::new(1_000)),
            Arc::new(InMemoryHistory::new()),
            Arc::new(move || {
                Box::new(ScriptedSession::with_audio_end_tracker(Arc::clone(
                    &factory_ended,
                ))) as Box<dyn RealtimeSession>
            }),
            None,
        );
        handle.start(false).await.unwrap();

        // Muting signals the provider once (audioStreamEnd) so it finalizes the turn and pauses.
        handle.set_mic_muted(true).await.unwrap();
        // A redundant mute(true) must not re-signal — only the false→true transition fires.
        handle.set_mic_muted(true).await.unwrap();
        // Unmuting needs no signal (the next mic frame reopens the stream).
        handle.set_mic_muted(false).await.unwrap();
        handle.state().await.unwrap(); // barrier: all prior commands processed

        assert_eq!(
            audio_ended.load(Ordering::SeqCst),
            1,
            "expected exactly one end_audio_stream on the mute transition"
        );
        handle.stop(false).await.unwrap();
    }
}
