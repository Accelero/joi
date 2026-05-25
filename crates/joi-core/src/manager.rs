//! The [`SessionManager`] actor — the single owner of the live session.
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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::clock::{Clock, UnixMillis};
use crate::config::Config;
use crate::connectivity::ConnectivityProbe;
use crate::error::SessionError;
use crate::history::{HistoryStore, HistoryTurn, Role, TokenBudget};
use crate::media::VideoFrame;
use crate::metrics::{
    bytes_to_kbps, MetricsSnapshot, ThroughputMeter, TokenUsage, TransportBytes, SAMPLE_INTERVAL,
};
use crate::session::event::{AppState, CloseReason, ConnectionStatus, SessionEvent, Speaker};
use crate::session::{RealtimeSession, SessionConfig, UiEvent};
use crate::tools::{
    evaluate_permission, validate_args, Permission, PermissionAction, Tool, ToolCallId, ToolResult,
    ToolRuntime,
};

/// Bytes per PCM16 sample — used to weigh audio frames into the throughput meter.
const BYTES_PER_SAMPLE: u64 = 2;

/// Capacity of the inbound command channel.
const COMMAND_CHANNEL: usize = 64;
/// Capacity of the UI-event broadcast.
const UI_CHANNEL: usize = 256;
/// Capacity of the internal provider-event and audio-output channels.
const MEDIA_CHANNEL: usize = 512;

/// Creates the [`RealtimeSession`] for a start/resume. Injected so the manager never names a
/// concrete provider (the composition root picks one by `config.live_api.provider`).
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

#[derive(Debug)]
struct ToolOutcome {
    epoch: u64,
    id: ToolCallId,
    name: String,
    result: ToolResult,
}

struct PendingTool {
    tool: Arc<dyn Tool>,
    args: Value,
    name: String,
    permission: Permission,
}

struct RunningTool {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

/// A command sent to the [`SessionManager`] actor (the command half of Seam A).
#[derive(Debug)]
pub enum Command {
    /// Start (`resume=false`) or resume (`resume=true`) a session.
    Start {
        /// Whether to seed restored context.
        resume: bool,
        /// Reply with the connect result.
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    /// Stop or pause the session (both fully disconnect — FR-14/15).
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
    /// Mute/unmute the mic at the manager (the capture gate is the primary mute — FR-6).
    SetMicMuted {
        /// Muted state.
        muted: bool,
    },
    /// Query the current [`AppState`].
    QueryState {
        /// Reply with the state.
        reply: oneshot::Sender<AppState>,
    },
    /// Resolve a pending tool permission request.
    ResolveToolPermission {
        /// Epoch captured when the permission prompt was emitted.
        epoch: u64,
        /// Provider-assigned tool-call id.
        id: ToolCallId,
        /// Whether the user approved this call.
        approve: bool,
    },
    /// Replace the actor's [`Config`] after a runtime settings change (boxed — it's large relative
    /// to the other variants). `NextSession` fields take effect on the next [`Command::Start`]. The
    /// editable-settings snapshot is broadcast separately by `JoiApp` via
    /// [`SessionManagerHandle::broadcast_settings`] (it needs provider data the actor can't compute).
    UpdateConfig {
        /// The new configuration.
        config: Box<Config>,
    },
    /// Stop and shut the actor down.
    Shutdown,
}

/// Cheap, cloneable handle to a running [`SessionManager`]. Held by `JoiApp` and the media engine.
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

    /// Subscribe to UI events (the event half of Seam A).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.ui_tx.subscribe()
    }

    /// Subscribe to audio-output frames (24 kHz mono PCM16). The production consumer is the
    /// `joi-media` playback pump; an empty frame is the flush/barge-in sentinel.
    #[must_use]
    pub fn subscribe_audio(&self) -> broadcast::Receiver<Vec<i16>> {
        self.audio_tx.subscribe()
    }

    /// Broadcast a settings snapshot on the UI event stream as [`UiEvent::Settings`]. Used by
    /// `JoiApp` after a settings change: the snapshot needs provider-supplied data (e.g. the voice
    /// catalog) the actor can't compute, so the composition root builds it and publishes it through
    /// this method — the actor still owns the channel, it just relays what it's handed.
    pub fn broadcast_settings(&self, settings: crate::settings::SettingsSnapshot) {
        let _ = self.ui_tx.send(UiEvent::Settings { settings });
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

    /// Replace the actor's config after a runtime settings change so the next connect uses it
    /// (`NextSession` fields take effect on the next [`start`](Self::start)). The editable-settings
    /// snapshot is published separately via [`broadcast_settings`](Self::broadcast_settings).
    pub async fn update_config(&self, config: Config) -> Result<(), SessionError> {
        self.cmd_tx
            .send(Command::UpdateConfig {
                config: Box::new(config),
            })
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

    /// Resolve a pending tool permission request.
    pub async fn resolve_tool_permission(
        &self,
        epoch: u64,
        id: ToolCallId,
        approve: bool,
    ) -> Result<(), SessionError> {
        self.cmd_tx
            .send(Command::ResolveToolPermission { epoch, id, approve })
            .await
            .map_err(|_| Self::dead())
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
    tool_tx: mpsc::Sender<ToolOutcome>,
    tool_runtime: ToolRuntime,
    active_epoch: u64,
    pending_tools: HashMap<(u64, ToolCallId), PendingTool>,
    running_tools: HashMap<(u64, ToolCallId), RunningTool>,
    state: AppState,
    mic_muted: bool,
    last_resumption_handle: Option<String>,
    /// Accumulates the in-flight agent transcript line's incremental deltas so the full line can be
    /// appended to history when it finalizes (the provider streams deltas).
    agent_line: String,
    /// Accumulates the in-flight user (input-transcription) line the same way. Kept separate from
    /// `agent_line` so an interleaved user/agent delta order can't splice the two together.
    user_line: String,
    /// Rolling payload byte/token tallies, drained into a `UiEvent::Metrics` each `SAMPLE_INTERVAL`.
    /// Supplies the token rate always, and the up/down rates when the provider can't report wire bytes.
    meter: ThroughputMeter,
    /// Clock time of the last metrics sample, so the next rate covers the real elapsed window.
    last_sample_ms: UnixMillis,
    /// Provider wire-byte totals at the previous sample, differenced to get the per-window rate.
    /// `None` until the first sample of a connection (and after stop) so we don't report a spike.
    last_transport: Option<TransportBytes>,
    /// Provider token-usage totals at the previous sample, differenced to get the per-window token
    /// rate. `None` until the first sample of a connection (and after stop) — same spike-guard as
    /// `last_transport`.
    last_token_usage: Option<TokenUsage>,
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
        tool_runtime: ToolRuntime,
    ) -> SessionManagerHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL);
        let (ui_tx, _) = broadcast::channel(UI_CHANNEL);
        let (audio_tx, _) = broadcast::channel(MEDIA_CHANNEL);
        let (ev_tx, ev_rx) = mpsc::channel(MEDIA_CHANNEL);
        let (tool_tx, tool_rx) = mpsc::channel(COMMAND_CHANNEL);

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
            tool_tx,
            tool_runtime,
            active_epoch: 0,
            pending_tools: HashMap::new(),
            running_tools: HashMap::new(),
            state: AppState::Stopped,
            mic_muted: false,
            last_resumption_handle: None,
            agent_line: String::new(),
            user_line: String::new(),
            meter: ThroughputMeter::default(),
            last_sample_ms,
            last_transport: None,
            last_token_usage: None,
            probe_trigger: probe_trigger.clone(),
        };
        tokio::spawn(actor.run(cmd_rx, ev_rx, tool_rx));
        SessionManagerHandle {
            cmd_tx,
            ui_tx,
            audio_tx,
            probe_trigger,
        }
    }

    /// The actor loop. `session` and the two receivers are locals, so command and event handlers
    /// borrow `self` without aliasing.
    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<Command>,
        mut ev_rx: mpsc::Receiver<SessionEvent>,
        mut tool_rx: mpsc::Receiver<ToolOutcome>,
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
                    self.handle_event(event, &mut session).await;
                }
                Some(outcome) = tool_rx.recv() => {
                    self.complete_tool(outcome, &mut session).await;
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
                        self.meter.add_input_text(&text);
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
                        // an error per 20 ms frame.
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
            Command::ResolveToolPermission { epoch, id, approve } => {
                self.resolve_tool_permission(epoch, id, approve, session)
                    .await;
            }
            Command::UpdateConfig { config } => {
                // Swap in the new config. `NextSession` fields (voice, model, transcription, budget,
                // compression) are read fresh by `do_start`, so they apply on the next connect; the
                // live session is untouched. The editable-settings snapshot is built and broadcast by
                // `JoiApp` (it needs provider-supplied data the manager can't compute — see
                // `broadcast_settings`), not here.
                self.config = *config;
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
        self.active_epoch = self.active_epoch.wrapping_add(1);

        self.set_state(AppState::Connecting);
        self.emit_connection(ConnectionStatus::Connecting, None);

        // Always seed from the persisted log so a session continues the prior conversation; the
        // first start finds an empty log and seeds nothing. (`resume` is retained for the future
        // native resumption-handle path — context seeding no longer depends on it.)
        let initial_context = self.restore_context().await;
        tracing::debug!(
            resume,
            seed_turns = initial_context.len(),
            "starting session"
        );
        let mut cfg = SessionConfig::from_config(
            &self.config,
            initial_context,
            self.last_resumption_handle.clone(),
        );
        cfg.tools = self.tool_runtime.registry.schemas();

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
        // Drop any half-finished transcript line from a prior connection so it can't bleed into the
        // new session's first turn.
        self.user_line.clear();
        self.agent_line.clear();
        // Start the throughput window fresh so the first sample measures from connect, not from a
        // possibly-long idle gap since the last session.
        self.meter.reset();
        self.last_sample_ms = self.clock.now_ms();
        self.last_transport = None;
        self.last_token_usage = None;
        self.set_state(AppState::Listening);
        self.emit_connection(ConnectionStatus::Connected, None);
        Ok(())
    }

    async fn do_stop(&mut self, session: &mut Option<Box<dyn RealtimeSession>>) {
        self.active_epoch = self.active_epoch.wrapping_add(1);
        self.cancel_all_tools();
        if let Some(mut s) = session.take() {
            let _ = s.close().await;
        }
        if self.state != AppState::Stopped {
            self.set_state(AppState::Stopped);
            self.emit_connection(ConnectionStatus::Disconnected, None);
            // Clear the live indicator: drop any partial window and emit one zero sample.
            self.meter.reset();
            self.last_transport = None;
            self.last_token_usage = None;
            self.emit(UiEvent::Metrics(MetricsSnapshot::ZERO));
        }
    }

    /// Load persisted turns within budget, oldest-first, for re-seeding (FR-20/21).
    async fn restore_context(&self) -> Vec<HistoryTurn> {
        let budget = TokenBudget(self.config.live_api.token_budget());
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

    async fn handle_event(
        &mut self,
        event: SessionEvent,
        session: &mut Option<Box<dyn RealtimeSession>>,
    ) {
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
                // `text` is an incremental delta: accumulate the line (per speaker) for history,
                // forward the delta to the UI (which appends it). On finalize, persist the whole
                // line — skipping empties so a bare commit doesn't write a blank turn.
                match speaker {
                    Speaker::User => self.user_line.push_str(&text),
                    Speaker::Agent => self.agent_line.push_str(&text),
                }
                self.emit(UiEvent::Transcript {
                    speaker,
                    text,
                    final_,
                });
                if final_ {
                    let line = match speaker {
                        Speaker::User => std::mem::take(&mut self.user_line),
                        Speaker::Agent => std::mem::take(&mut self.agent_line),
                    };
                    if !line.is_empty() {
                        self.append_turn(speaker, line).await;
                    }
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
                    // A server/error close in the MVP stops cleanly; transient-drop → reconnect is
                    // future work (FR-16).
                    self.set_state(AppState::Stopped);
                    self.emit_connection(ConnectionStatus::Disconnected, None);
                }
            }
            SessionEvent::ToolCall { id, name, args } => {
                self.handle_tool_call(id, name, args, session).await;
            }
        }
    }

    async fn handle_tool_call(
        &mut self,
        id: ToolCallId,
        name: String,
        args: Value,
        session: &mut Option<Box<dyn RealtimeSession>>,
    ) {
        let Some(tool) = self.tool_runtime.registry.get(&name) else {
            self.send_tool_result_now(
                self.active_epoch,
                id,
                name,
                ToolResult::error("unknown tool"),
                session,
            )
            .await;
            return;
        };

        let schema = tool.schema();
        if let Err(e) = validate_args(&schema.parameters, &args) {
            self.send_tool_result_now(
                self.active_epoch,
                id,
                name,
                ToolResult::error(format!("invalid tool arguments: {e}")),
                session,
            )
            .await;
            return;
        }

        let permission = tool.permission(&args, &self.tool_runtime.ctx_template);
        self.emit(UiEvent::ToolCall {
            id: id.clone(),
            name: name.clone(),
            summary: permission.summary.clone(),
        });
        match evaluate_permission(&self.tool_runtime.permission_profile, &permission) {
            PermissionAction::Allow => {
                self.spawn_tool(self.active_epoch, id, name, args, tool);
            }
            PermissionAction::Ask => {
                let epoch = self.active_epoch;
                self.emit(UiEvent::ToolPermission {
                    epoch,
                    id: id.clone(),
                    name: name.clone(),
                    summary: permission.summary.clone(),
                    detail: permission.detail.clone(),
                });
                self.pending_tools.insert(
                    (epoch, id),
                    PendingTool {
                        tool,
                        args,
                        name,
                        permission,
                    },
                );
            }
            PermissionAction::Deny => {
                self.send_tool_result_now(
                    self.active_epoch,
                    id,
                    name,
                    denied_tool_result("system", &permission.summary),
                    session,
                )
                .await;
            }
        }
    }

    async fn resolve_tool_permission(
        &mut self,
        epoch: u64,
        id: ToolCallId,
        approve: bool,
        session: &mut Option<Box<dyn RealtimeSession>>,
    ) {
        if epoch != self.active_epoch {
            return;
        }
        let Some(pending) = self.pending_tools.remove(&(epoch, id.clone())) else {
            return;
        };
        if approve {
            self.spawn_tool(epoch, id, pending.name, pending.args, pending.tool);
        } else {
            self.send_tool_result_now(
                epoch,
                id,
                pending.name,
                denied_tool_result("user", &pending.permission.summary),
                session,
            )
            .await;
        }
    }

    fn spawn_tool(
        &mut self,
        epoch: u64,
        id: ToolCallId,
        name: String,
        args: Value,
        tool: Arc<dyn Tool>,
    ) {
        let cancel = CancellationToken::new();
        let mut ctx = self.tool_runtime.ctx_template.clone();
        ctx.cancel = cancel.clone();
        let tx = self.tool_tx.clone();
        let id_for_task = id.clone();
        let name_for_task = name;
        let handle = tokio::spawn(async move {
            let run = tool.run(args, &ctx);
            let result = tokio::select! {
                () = ctx.cancel.cancelled() => ToolResult::error("tool cancelled"),
                result = tokio::time::timeout(ctx.timeout, run) => {
                    match result {
                        Ok(result) => result,
                        Err(_) => ToolResult::error("tool timed out"),
                    }
                }
            };
            let _ = tx
                .send(ToolOutcome {
                    epoch,
                    id: id_for_task,
                    name: name_for_task,
                    result,
                })
                .await;
        });
        self.running_tools
            .insert((epoch, id), RunningTool { cancel, handle });
    }

    async fn complete_tool(
        &mut self,
        outcome: ToolOutcome,
        session: &mut Option<Box<dyn RealtimeSession>>,
    ) {
        let key = (outcome.epoch, outcome.id.clone());
        self.running_tools.remove(&key);
        self.send_tool_result_now(
            outcome.epoch,
            outcome.id,
            outcome.name,
            outcome.result,
            session,
        )
        .await;
    }

    async fn send_tool_result_now(
        &mut self,
        epoch: u64,
        id: ToolCallId,
        name: String,
        result: ToolResult,
        session: &mut Option<Box<dyn RealtimeSession>>,
    ) {
        if epoch != self.active_epoch {
            return;
        }
        let summary = tool_result_summary(&result);
        let ok = result.ok;
        if let Some(s) = session.as_mut() {
            if let Err(e) = s.send_tool_result(id.clone(), result).await {
                self.on_error("tool_result_send", &e);
                return;
            }
        } else {
            self.on_error("tool_result_send", &SessionError::NotConnected);
            return;
        }
        self.emit(UiEvent::ToolResult {
            id,
            name,
            ok,
            summary,
        });
    }

    fn cancel_all_tools(&mut self) {
        self.pending_tools.clear();
        for (_, running) in self.running_tools.drain() {
            running.cancel.cancel();
            running.handle.abort();
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
        let budget = TokenBudget(self.config.live_api.token_budget());
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
            self.last_token_usage = None;
            self.last_sample_ms = self.clock.now_ms();
            return;
        }
        let now = self.clock.now_ms();
        let elapsed = Duration::from_millis(now.saturating_sub(self.last_sample_ms));
        let Some(mut snapshot) = self.meter.sample(elapsed) else {
            return; // no time elapsed (e.g. a frozen test clock); keep counters for the next tick
        };

        let secs = elapsed.as_secs_f64();

        // Prefer the provider's wire-byte counters, differencing successive totals over the window.
        if let Some(current) = session.and_then(RealtimeSession::transport_bytes) {
            // The first sample of a connection has no baseline; treat `prev == current` so it
            // reports zero rather than a spike from the connect/setup handshake.
            let prev = self.last_transport.unwrap_or(current);
            snapshot.up_kbps = bytes_to_kbps(current.sent.saturating_sub(prev.sent), secs);
            snapshot.down_kbps =
                bytes_to_kbps(current.received.saturating_sub(prev.received), secs);
            self.last_transport = Some(current);
        }

        // Prefer the provider's real token usage over the chars/4 estimate, differencing the
        // cumulative totals over the window (same first-sample spike-guard as the wire bytes).
        if let Some(current) = session.and_then(RealtimeSession::token_usage) {
            let prev = self.last_token_usage.unwrap_or(current);
            #[allow(clippy::cast_precision_loss)] // token counts stay well within f64's exact range
            {
                snapshot.up_tokens_per_sec = current.up.saturating_sub(prev.up) as f64 / secs;
                snapshot.down_tokens_per_sec = current.down.saturating_sub(prev.down) as f64 / secs;
            }
            self.last_token_usage = Some(current);
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

fn tool_result_summary(result: &ToolResult) -> String {
    if !result.ok {
        return result
            .content
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("tool failed")
            .to_string();
    }
    if let Some(summary) = result
        .content
        .get("summary")
        .and_then(serde_json::Value::as_str)
    {
        return summary.to_string();
    }
    "tool completed".to_string()
}

fn denied_tool_result(origin: &str, summary: &str) -> ToolResult {
    ToolResult {
        ok: false,
        content: serde_json::json!({
            "error": format!("tool use denied by {origin}: {summary}"),
            "denied_by": origin,
        }),
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
    use crate::tools::{PermissionProfile, ToolCtx, ToolRegistry};
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::broadcast::error::RecvError;

    /// A minimal scripted session: each `send_text`/`send_audio` pushes deterministic events.
    /// `closed` counts `close()` calls so tests can prove Stop tears the session down.
    /// Externally-controlled `(sent, received)` wire-byte totals a test can drive directly.
    type WireCounters = (std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64);
    type RecordedToolResults = Arc<std::sync::Mutex<Vec<(ToolCallId, ToolResult)>>>;

    struct ScriptedSession {
        tx: Option<EventSender>,
        rx: Option<EventReceiver>,
        closed: Arc<std::sync::atomic::AtomicUsize>,
        /// Counts `end_audio_stream()` calls so a test can prove mute pauses the provider stream.
        audio_ended: Arc<std::sync::atomic::AtomicUsize>,
        /// When set, the session reports these as `transport_bytes` (the wire-metering path).
        transport: Option<Arc<WireCounters>>,
        /// When set, the session reports these as `token_usage` `(up, down)` cumulative totals.
        tokens: Option<Arc<WireCounters>>,
        /// When set, `send_audio` emits a streamed user (input-transcription) line instead of audio,
        /// to exercise the manager's per-speaker transcript accumulation.
        user_stream: bool,
        /// When set, `connect` records how many `initial_context` turns the manager seeded, so a
        /// test can prove start replays the persisted log.
        seeded: Option<Arc<std::sync::atomic::AtomicUsize>>,
        /// When set, `send_text` emits this tool call instead of a transcript turn.
        tool_call: Option<(String, Value)>,
        /// When set, `send_tool_result` records the provider-facing result.
        tool_results: Option<RecordedToolResults>,
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
                tokens: None,
                user_stream: false,
                seeded: None,
                tool_call: None,
                tool_results: None,
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

        fn with_token_usage(tokens: Arc<WireCounters>) -> Self {
            Self {
                tokens: Some(tokens),
                ..Self::new()
            }
        }

        fn with_user_transcript() -> Self {
            Self {
                user_stream: true,
                ..Self::new()
            }
        }

        fn with_seed_recorder(seeded: Arc<std::sync::atomic::AtomicUsize>) -> Self {
            Self {
                seeded: Some(seeded),
                ..Self::new()
            }
        }

        fn with_tool_call(name: &str, args: Value) -> Self {
            Self {
                tool_call: Some((name.to_string(), args)),
                ..Self::new()
            }
        }

        fn with_tool_call_and_result_recorder(
            name: &str,
            args: Value,
            tool_results: RecordedToolResults,
        ) -> Self {
            Self {
                tool_call: Some((name.to_string(), args)),
                tool_results: Some(tool_results),
                ..Self::new()
            }
        }
    }

    #[async_trait]
    impl RealtimeSession for ScriptedSession {
        async fn connect(&mut self, cfg: SessionConfig) -> Result<(), SessionError> {
            if let Some(seeded) = &self.seeded {
                seeded.store(
                    cfg.initial_context.len(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
            let (tx, rx) = mpsc::channel(64);
            self.tx = Some(tx);
            self.rx = Some(rx);
            Ok(())
        }

        async fn send_audio(&mut self, _pcm: &[i16]) -> Result<(), SessionError> {
            let tx = self.tx.as_ref().ok_or(SessionError::NotConnected)?;
            if self.user_stream {
                // The provider's input transcription arrives as deltas, then an empty-text final
                // (the words are accumulated by the manager, mirroring the real Gemini mapper).
                for delta in ["par", "tial"] {
                    tx.send(SessionEvent::Transcript {
                        speaker: Speaker::User,
                        text: delta.to_string(),
                        final_: false,
                    })
                    .await
                    .ok();
                }
                tx.send(SessionEvent::Transcript {
                    speaker: Speaker::User,
                    text: String::new(),
                    final_: true,
                })
                .await
                .ok();
                return Ok(());
            }
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
            if let Some((name, args)) = self.tool_call.clone() {
                tx.send(SessionEvent::ToolCall {
                    id: ToolCallId("tool-1".to_string()),
                    name,
                    args,
                })
                .await
                .ok();
                return Ok(());
            }
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

        async fn send_tool_result(
            &mut self,
            id: ToolCallId,
            result: ToolResult,
        ) -> Result<(), SessionError> {
            if let Some(tool_results) = &self.tool_results {
                tool_results.lock().unwrap().push((id, result));
            }
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

        fn token_usage(&self) -> Option<TokenUsage> {
            use std::sync::atomic::Ordering;
            self.tokens.as_ref().map(|c| TokenUsage {
                up: c.0.load(Ordering::SeqCst),
                down: c.1.load(Ordering::SeqCst),
            })
        }
    }

    struct EchoTool {
        action: PermissionAction,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn schema(&self) -> crate::tools::ToolSchema {
            crate::tools::ToolSchema {
                name: "echo_tool".to_string(),
                description: "Echo test input.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"]
                }),
            }
        }

        fn permission(&self, args: &Value, _ctx: &ToolCtx) -> Permission {
            Permission {
                key: "echo".to_string(),
                subject: args
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                default_action: self.action,
                summary: "echo test".to_string(),
                detail: "echo test detail".to_string(),
            }
        }

        async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
            ToolResult::ok(serde_json::json!({
                "echo": args["text"],
                "summary": "echoed"
            }))
        }
    }

    fn runtime_with_echo(clock: TestClock, action: PermissionAction) -> ToolRuntime {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool { action }));
        ToolRuntime {
            registry: Arc::new(registry),
            ctx_template: ToolCtx {
                readable_roots: Vec::new(),
                writable_roots: Vec::new(),
                cwd: PathBuf::new(),
                timeout: Duration::from_secs(1),
                max_output_bytes: 4096,
                network: false,
                cancel: CancellationToken::new(),
                clock: Arc::new(clock),
            },
            permission_profile: PermissionProfile::default(),
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
            ToolRuntime::disabled(Arc::new(clock.clone())),
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
            ToolRuntime::disabled(Arc::new(TestClock::new(1_000))),
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
    async fn allowed_tool_call_runs_and_sends_result() {
        let clock = TestClock::new(1_000);
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(clock.clone()),
            Arc::new(InMemoryHistory::new()),
            Arc::new(|| {
                Box::new(ScriptedSession::with_tool_call(
                    "echo_tool",
                    serde_json::json!({"text": "hi"}),
                )) as Box<dyn RealtimeSession>
            }),
            None,
            runtime_with_echo(clock, PermissionAction::Allow),
        );
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();
        handle.send_text("call").await.unwrap();

        let mut saw_call = false;
        let mut saw_result = false;
        for _ in 0..20 {
            match next_ui(&mut ui).await {
                UiEvent::ToolCall { name, .. } if name == "echo_tool" => saw_call = true,
                UiEvent::ToolResult {
                    name,
                    ok: true,
                    summary,
                    ..
                } if name == "echo_tool" && summary == "echoed" => saw_result = true,
                _ => {}
            }
            if saw_call && saw_result {
                break;
            }
        }
        assert!(saw_call, "tool call event not emitted");
        assert!(saw_result, "tool result event not emitted");
    }

    #[tokio::test]
    async fn asked_tool_waits_for_permission_resolution() {
        let clock = TestClock::new(1_000);
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(clock.clone()),
            Arc::new(InMemoryHistory::new()),
            Arc::new(|| {
                Box::new(ScriptedSession::with_tool_call(
                    "echo_tool",
                    serde_json::json!({"text": "approval"}),
                )) as Box<dyn RealtimeSession>
            }),
            None,
            runtime_with_echo(clock, PermissionAction::Ask),
        );
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();
        handle.send_text("call").await.unwrap();

        let (epoch, id) = loop {
            if let UiEvent::ToolPermission {
                epoch, id, name, ..
            } = next_ui(&mut ui).await
            {
                assert_eq!(name, "echo_tool");
                break (epoch, id);
            }
        };
        handle
            .resolve_tool_permission(epoch, id, true)
            .await
            .unwrap();

        let mut saw_result = false;
        for _ in 0..20 {
            if let UiEvent::ToolResult { name, ok: true, .. } = next_ui(&mut ui).await {
                if name == "echo_tool" {
                    saw_result = true;
                    break;
                }
            }
        }
        assert!(saw_result, "approved tool did not run");
    }

    #[tokio::test]
    async fn user_denied_tool_result_identifies_user_origin() {
        let clock = TestClock::new(1_000);
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded_for_session = recorded.clone();
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(clock.clone()),
            Arc::new(InMemoryHistory::new()),
            Arc::new(move || {
                Box::new(ScriptedSession::with_tool_call_and_result_recorder(
                    "echo_tool",
                    serde_json::json!({"text": "approval"}),
                    recorded_for_session.clone(),
                )) as Box<dyn RealtimeSession>
            }),
            None,
            runtime_with_echo(clock, PermissionAction::Ask),
        );
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();
        handle.send_text("call").await.unwrap();

        let (epoch, id) = loop {
            if let UiEvent::ToolPermission { epoch, id, .. } = next_ui(&mut ui).await {
                break (epoch, id);
            }
        };
        handle
            .resolve_tool_permission(epoch, id, false)
            .await
            .unwrap();

        let summary = loop {
            if let UiEvent::ToolResult {
                name,
                ok: false,
                summary,
                ..
            } = next_ui(&mut ui).await
            {
                if name == "echo_tool" {
                    break summary;
                }
            }
        };
        assert_eq!(summary, "tool use denied by user: echo test");

        let recorded = recorded.lock().unwrap();
        let (_, result) = recorded.first().expect("tool result sent to provider");
        assert!(!result.ok);
        assert_eq!(result.content["denied_by"], "user");
        assert_eq!(
            result.content["error"],
            "tool use denied by user: echo test"
        );
    }

    #[tokio::test]
    async fn system_denied_tool_result_identifies_system_origin() {
        let clock = TestClock::new(1_000);
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded_for_session = recorded.clone();
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(clock.clone()),
            Arc::new(InMemoryHistory::new()),
            Arc::new(move || {
                Box::new(ScriptedSession::with_tool_call_and_result_recorder(
                    "echo_tool",
                    serde_json::json!({"text": "blocked"}),
                    recorded_for_session.clone(),
                )) as Box<dyn RealtimeSession>
            }),
            None,
            runtime_with_echo(clock, PermissionAction::Deny),
        );
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();
        handle.send_text("call").await.unwrap();

        let summary = loop {
            if let UiEvent::ToolResult {
                name,
                ok: false,
                summary,
                ..
            } = next_ui(&mut ui).await
            {
                if name == "echo_tool" {
                    break summary;
                }
            }
        };
        assert_eq!(summary, "tool use denied by system: echo test");

        let recorded = recorded.lock().unwrap();
        let (_, result) = recorded.first().expect("tool result sent to provider");
        assert!(!result.ok);
        assert_eq!(result.content["denied_by"], "system");
        assert_eq!(
            result.content["error"],
            "tool use denied by system: echo test"
        );
    }

    #[tokio::test]
    async fn start_seeds_context_from_the_persisted_log() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // A prior conversation already sits in the log.
        let history = Arc::new(InMemoryHistory::new());
        history
            .append(HistoryTurn::new(Role::User, "earlier question", 1))
            .await
            .unwrap();
        history
            .append(HistoryTurn::new(Role::Assistant, "earlier answer", 2))
            .await
            .unwrap();

        let seeded = Arc::new(AtomicUsize::new(0));
        let factory_seeded = Arc::clone(&seeded);
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(TestClock::new(1_000)),
            history.clone(),
            Arc::new(move || {
                Box::new(ScriptedSession::with_seed_recorder(Arc::clone(
                    &factory_seeded,
                ))) as Box<dyn RealtimeSession>
            }),
            None,
            ToolRuntime::disabled(Arc::new(TestClock::new(1_000))),
        );

        // Even with resume=false, start replays the persisted log into the new session.
        handle.start(false).await.unwrap();
        assert_eq!(
            seeded.load(Ordering::SeqCst),
            2,
            "start should seed the two persisted turns"
        );
        handle.stop(false).await.unwrap();
    }

    #[tokio::test]
    async fn streamed_user_transcript_is_stored_as_one_user_turn() {
        // The provider streams the user's input transcription as deltas + an empty-text final; the
        // manager must accumulate them into a single Speaker::User turn (and not store a blank one).
        let history = Arc::new(InMemoryHistory::new());
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(TestClock::new(1_000)),
            history.clone(),
            Arc::new(|| {
                Box::new(ScriptedSession::with_user_transcript()) as Box<dyn RealtimeSession>
            }),
            None,
            ToolRuntime::disabled(Arc::new(TestClock::new(1_000))),
        );
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();
        handle.send_audio(vec![0; 320]).await.unwrap();

        // Wait until the user turn has been appended (signalled by its History meta).
        let mut appended = false;
        for _ in 0..50 {
            if let UiEvent::History(meta) = next_ui(&mut ui).await {
                if meta.turns >= 1 {
                    appended = true;
                    break;
                }
            }
        }
        assert!(appended, "user turn was not appended to history");

        let stored = history
            .load_within_budget(TokenBudget(10_000))
            .await
            .unwrap();
        // Exactly one turn — the deltas concatenated, the empty final adding nothing.
        assert_eq!(stored.len(), 1, "expected one stored turn, got {stored:?}");
        assert_eq!(stored[0].role, Role::User);
        assert_eq!(stored[0].text, "partial");
        handle.stop(false).await.unwrap();
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
            ToolRuntime::disabled(Arc::new(clock.clone())),
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
    async fn metrics_report_provider_token_usage_as_a_rate() {
        // The provider reports growing cumulative token totals; the manager differences them into a
        // per-second up/down token rate, with the first sample as a zero baseline (no spike).
        use std::sync::atomic::{AtomicU64, Ordering};
        let tokens = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
        let clock = TestClock::new(1_000);
        let factory_tokens = Arc::clone(&tokens);
        let handle = SessionManager::spawn(
            Config::default(),
            Arc::new(clock.clone()),
            Arc::new(InMemoryHistory::new()),
            Arc::new(move || {
                Box::new(ScriptedSession::with_token_usage(Arc::clone(
                    &factory_tokens,
                ))) as Box<dyn RealtimeSession>
            }),
            None,
            ToolRuntime::disabled(Arc::new(clock.clone())),
        );
        let mut ui = handle.subscribe();
        handle.start(false).await.unwrap();

        // Phase 1: baseline window — totals still zero, so the first sample reports no rate.
        clock.advance(1_000);
        let baseline = next_metrics(&mut ui).await;
        assert_eq!(
            baseline.up_tokens_per_sec, 0.0,
            "first sample is the baseline"
        );

        // Phase 2: grow both cumulative totals; the next sample differences them over the window
        // (90 prompt + 40 response tokens over ~1 s -> 90 up tok/s, 40 down tok/s).
        tokens.0.store(90, Ordering::SeqCst);
        tokens.1.store(40, Ordering::SeqCst);
        clock.advance(1_000);
        let mut sample = None;
        for _ in 0..50 {
            let m = next_metrics(&mut ui).await;
            if m.up_tokens_per_sec > 0.0 {
                sample = Some(m);
                break;
            }
        }
        let m = sample.expect("a non-zero up-token-rate sample");
        assert!(
            m.up_tokens_per_sec > 0.0,
            "up token rate should be positive: {m:?}"
        );
        assert!(
            m.down_tokens_per_sec > 0.0,
            "down token rate should be positive: {m:?}"
        );
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
    async fn broadcast_settings_publishes_on_the_ui_stream() {
        use crate::settings::{settings_schema, SettingId, SettingsContext};
        let (handle, _history) = spawn_manager();
        let mut ui = handle.subscribe();

        // The composition root builds the snapshot (here with a provider-supplied voice list) and
        // hands it to the actor to publish.
        let cfg = Config::default();
        let snapshot = settings_schema(
            &cfg,
            &SettingsContext {
                voices: vec!["Aoede".to_string(), "Charon".to_string()],
            },
        );
        handle.broadcast_settings(snapshot);

        let mut saw_voice_options = false;
        for _ in 0..20 {
            if let UiEvent::Settings { settings } = next_ui(&mut ui).await {
                saw_voice_options = settings
                    .iter()
                    .find(|d| d.id == SettingId::Voice)
                    .is_some_and(|d| {
                        matches!(&d.kind, crate::settings::SettingKind::Choice { options }
                            if options == &vec!["Aoede".to_string(), "Charon".to_string()])
                    });
                break;
            }
        }
        assert!(
            saw_voice_options,
            "broadcast_settings should emit the snapshot verbatim"
        );
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
            ToolRuntime::disabled(Arc::new(TestClock::new(1_000))),
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
