//! Composition root + Seam-A API for JOI. A host — the TUI today, a future out-of-process backend
//! or WS server — builds a [`JoiApp`] from a [`Config`] and drives it via these methods plus the
//! event/audio streams. No host, frontend, or transport types appear here, so the engine can be
//! operated headlessly (PLAN §8).
//!
//! The command surface: [`JoiApp::start`]/[`stop`](JoiApp::stop)/[`send_text`](JoiApp::send_text)/
//! [`set_mic_muted`](JoiApp::set_mic_muted)/[`start_screenshare`](JoiApp::start_screenshare)/
//! [`stop_screenshare`](JoiApp::stop_screenshare)/[`has_api_key`](JoiApp::has_api_key) plus the
//! session commands [`list_sessions`](JoiApp::list_sessions)/[`current_session`](JoiApp::current_session)/
//! [`new_session`](JoiApp::new_session)/[`resume_session`](JoiApp::resume_session). Outputs are the
//! [`subscribe_events`](JoiApp::subscribe_events) `UiEvent` stream and, for headless hosts, the
//! [`send_audio`](JoiApp::send_audio) / [`subscribe_audio`](JoiApp::subscribe_audio) raw-audio
//! transport.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use joi_core::clock::SystemClock;
use joi_core::config::{Config, ProjectPaths, UiCfg};
use joi_core::error::{SessionError, SettingsError};
use joi_core::history::{HistoryStore, HistoryTurn, InMemoryHistory, SessionStore, SessionSummary};
use joi_core::manager::{SessionFactory, SessionManager, SessionManagerHandle};
use joi_core::media::AudioFormat;
use joi_core::session::event::UiEvent;
use joi_core::settings::{
    apply_setting, settings_schema as build_schema, SettingDescriptor, SettingId, SettingValue,
    SettingsContext,
};
use joi_media::{MediaConfig, MediaEngine};
use tokio::sync::broadcast;

/// Error when a session operation is requested but no persisted-session store exists (no API key,
/// or the in-memory fallback is active).
fn sessions_unavailable() -> SessionError {
    SessionError::Provider("session history is not available".to_string())
}

/// Build the editable-settings schema for the **current** config, resolving the provider-dependent
/// option lists here (the only layer that knows both settings and provider). The voice catalog is
/// derived from `cfg` on every call — cheap, and what makes a provider/model change reflect in the
/// schema automatically (no caching to invalidate).
fn build_current_schema(cfg: &Config) -> Vec<SettingDescriptor> {
    let ctx = SettingsContext {
        voices: joi_providers::voice_catalog(cfg),
    };
    build_schema(cfg, &ctx)
}

/// Whether the engine drives local audio/screen devices itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode {
    /// Desktop/TUI: native cpal mic/playback + xcap screen, bound to the session.
    LocalDevices,
    /// Headless: no local devices. The host feeds/consumes audio via [`JoiApp::send_audio`] /
    /// [`JoiApp::subscribe_audio`] instead.
    None,
}

/// The composed JOI engine. `handle`/`media` are `None` when no provider is configured (e.g. no API
/// key); the app still constructs and session commands then return a clear error rather than panic.
pub struct JoiApp {
    handle: Option<SessionManagerHandle>,
    media: Option<MediaEngine>,
    has_key: bool,
    /// Frontend settings, surfaced to hosts via [`JoiApp::ui_config`].
    ui: UiCfg,
    /// Persisted-session repository for list/resume/new, shared with the manager (it appends turns
    /// to the same store). `None` when sessions aren't persisted (no key, or in-memory fallback).
    sessions: Option<Arc<SessionStore>>,
    /// Authoritative config copy for the runtime settings interface. The manager keeps its own copy,
    /// resynced via [`SessionManagerHandle::update_config`] on each change — no shared lock reaches
    /// into the actor. Holds the full config (incl. the in-memory API key); the key is blanked only
    /// when written to disk.
    config: RwLock<Config>,
    /// Where to persist config changes (`~/.joi/config.json`). `None` when no path resolves, in
    /// which case [`update_setting`](JoiApp::update_setting) reports an error rather than silently
    /// dropping the change.
    config_path: Option<PathBuf>,
}

impl JoiApp {
    /// Composition root. **Must be called inside a Tokio runtime** — the session manager and the
    /// media engine spawn tasks with `tokio::spawn`.
    ///
    /// Runtime settings changes are persisted to the default `~/.joi/config.json`. (A host that
    /// loaded config from a non-default path can use [`build_with_config_path`](Self::build_with_config_path).)
    #[must_use]
    pub fn build(config: Config, media_mode: MediaMode) -> Self {
        let config_path = ProjectPaths::resolve().ok().map(|p| p.config_file);
        Self::build_with_config_path(config, media_mode, config_path)
    }

    /// Composition root with an explicit path for persisting runtime settings changes (`None` to
    /// disable persistence). [`build`](Self::build) is this with the default `~/.joi/config.json`.
    #[must_use]
    pub fn build_with_config_path(
        config: Config,
        media_mode: MediaMode,
        config_path: Option<PathBuf>,
    ) -> Self {
        let has_key = config.live_api.gemini.api_key.is_set();
        // Authoritative copy for the settings surface, kept in sync with the manager via
        // `update_config`. Cloned before `config` is moved into the manager below.
        let config_for_app = config.clone();
        // Keep the UI section before `config` is moved into the manager (cheap clone).
        let ui = config.ui.clone();
        match joi_providers::build_session_factory(&config) {
            Ok(factory) => {
                let factory: Arc<dyn SessionFactory> = Arc::from(factory);
                let clock = Arc::new(SystemClock);
                // Persist this run's conversation as a new session under ~/.joi/sessions. Falls back
                // to in-memory history if the dir is unset (headless/tests) or can't be created, so
                // the app still runs. `config.history.dir` was resolved by `Config::load`.
                let (history, sessions): (Arc<dyn HistoryStore>, Option<Arc<SessionStore>>) =
                    match config.history.dir.clone() {
                        Some(dir) => match SessionStore::create_new(dir, clock.clone()) {
                            Ok(store) => {
                                let store = Arc::new(store);
                                (store.clone(), Some(store))
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "session store unavailable; using in-memory history");
                                (Arc::new(InMemoryHistory::new()), None)
                            }
                        },
                        None => (Arc::new(InMemoryHistory::new()), None),
                    };
                // Read what local media needs from `config` before moving it into the manager.
                let media_config = MediaConfig {
                    input_device: config.media.audio.input_device.clone(),
                    output_device: config.media.audio.output_device.clone(),
                    frame_samples: AudioFormat::INPUT
                        .samples_per_frame(config.media.audio.frame_ms),
                    screen_fps: config.media.screen.fps,
                    screen_max_width: config.media.screen.max_width,
                    screen_quality: config.media.screen.quality,
                    echo_cancellation: config.media.audio.echo_cancellation,
                    noise_suppression: config.media.audio.noise_suppression,
                    auto_gain: config.media.audio.auto_gain,
                };
                // Token-free reachability probe (provider-specific call, composed here so the
                // engine stays provider-agnostic). `None` when the provider has no probe / no key.
                let probe = joi_providers::build_connectivity_probe(&config);
                let handle = SessionManager::spawn(config, clock, history, factory, probe);
                let media = match media_mode {
                    MediaMode::LocalDevices => Some(MediaEngine::new(handle.clone(), media_config)),
                    MediaMode::None => None,
                };
                Self {
                    handle: Some(handle),
                    media,
                    has_key,
                    ui,
                    sessions,
                    config: RwLock::new(config_for_app),
                    config_path,
                }
            }
            Err(e) => {
                tracing::warn!("session unavailable until configured: {e}");
                Self {
                    handle: None,
                    media: None,
                    has_key,
                    ui,
                    sessions: None,
                    config: RwLock::new(config_for_app),
                    config_path,
                }
            }
        }
    }

    fn session(&self) -> Result<&SessionManagerHandle, SessionError> {
        self.handle.as_ref().ok_or_else(|| {
            SessionError::Provider(
                "no API key configured (set GEMINI_API_KEY or live_api.gemini.api_key)".to_string(),
            )
        })
    }

    /// Start (or resume) a session and begin local mic capture (when in `LocalDevices` mode).
    pub async fn start(&self, resume: bool) -> Result<(), SessionError> {
        self.session()?.start(resume).await?;
        if let Some(m) = &self.media {
            m.start_capture();
        }
        Ok(())
    }

    /// Stop the session and local capture.
    pub async fn stop(&self, pause: bool) -> Result<(), SessionError> {
        if let Some(m) = &self.media {
            m.stop_capture();
        }
        self.session()?.stop(pause).await
    }

    /// Send typed text to the model.
    pub async fn send_text(&self, text: &str) -> Result<(), SessionError> {
        self.session()?.send_text(text).await
    }

    /// Push a mic frame (16 kHz mono PCM16) — for headless hosts; the desktop/TUI's `MediaEngine`
    /// feeds audio itself.
    pub async fn send_audio(&self, pcm: Vec<i16>) -> Result<(), SessionError> {
        self.session()?.send_audio(pcm).await
    }

    /// App-level mute. Gates local capture at the source immediately (`media`), and notifies the
    /// session manager so it can pause/resume the provider's audio stream on the transition
    /// (`audioStreamEnd` rather than streaming silence). The manager notification is fire-and-forget
    /// on the current runtime — mute is best-effort and a failure is non-fatal.
    pub fn set_mic_muted(&self, muted: bool) {
        if let Some(m) = &self.media {
            m.set_mic_muted(muted);
        }
        if let Some(h) = &self.handle {
            let h = h.clone();
            tokio::spawn(async move {
                let _ = h.set_mic_muted(muted).await;
            });
        }
    }

    /// Trigger an immediate provider-reachability probe (token-free). The result arrives on the
    /// [`subscribe_events`](Self::subscribe_events) stream as `UiEvent::Reachability`; this is a
    /// non-blocking nudge to the background monitor. No-op when no probe is wired (no key / no
    /// session manager). Reachability is also probed automatically at startup and on a poll.
    pub fn check_reachability(&self) {
        if let Some(h) = &self.handle {
            h.check_reachability();
        }
    }

    /// Start native screen capture (no-op without local media; idempotent).
    pub fn start_screenshare(&self) {
        if let Some(m) = &self.media {
            m.start_screenshare();
        }
    }

    /// Stop native screen capture (no-op without local media).
    pub fn stop_screenshare(&self) {
        if let Some(m) = &self.media {
            m.stop_screenshare();
        }
    }

    /// Whether an API key was configured at load (file or env).
    #[must_use]
    pub fn has_api_key(&self) -> bool {
        self.has_key
    }

    /// List persisted sessions, **newest-activity first** — the data a `/resume` picker renders.
    /// Empty when sessions aren't persisted (headless / no key).
    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        match &self.sessions {
            Some(s) => SessionStore::list(s.dir()).await,
            None => Vec::new(),
        }
    }

    /// Summary of the session currently in use (`None` when sessions aren't persisted).
    pub async fn current_session(&self) -> Option<SessionSummary> {
        match &self.sessions {
            Some(s) => Some(s.current_summary().await),
            None => None,
        }
    }

    /// Switch to a brand-new session. **Stops** any live session first so no trailing turn lands in
    /// it; the next [`start`](Self::start) seeds the (empty) new session. Returns its summary.
    pub async fn new_session(&self) -> Result<SessionSummary, SessionError> {
        self.stop(false).await?;
        let store = self.sessions.as_ref().ok_or_else(sessions_unavailable)?;
        store
            .start_new()
            .await
            .map_err(|e| SessionError::Provider(e.to_string()))
    }

    /// Resume an existing session by id. **Stops** any live session first, then retargets the store
    /// so the next [`start`](Self::start) seeds the resumed conversation. Returns its summary.
    pub async fn resume_session(&self, id: &str) -> Result<SessionSummary, SessionError> {
        self.stop(false).await?;
        let store = self.sessions.as_ref().ok_or_else(sessions_unavailable)?;
        store
            .switch_to(id)
            .await
            .map_err(|e| SessionError::Provider(e.to_string()))
    }

    /// The full persisted transcript of a session, **chronological order** — the data a frontend
    /// renders to repopulate its message feed when it loads or resumes a session. This is the whole
    /// conversation as stored (user + assistant turns), not the budget-bounded slice that re-seeds
    /// the model. Errors when sessions aren't persisted (headless / no key), mirroring
    /// [`resume_session`](Self::resume_session), so a host can tell "no turns yet" (an empty vec)
    /// from "sessions unavailable" (an error).
    pub async fn session_turns(&self, id: &str) -> Result<Vec<HistoryTurn>, SessionError> {
        let store = self.sessions.as_ref().ok_or_else(sessions_unavailable)?;
        store
            .load_turns(id)
            .await
            .map_err(|e| SessionError::Provider(e.to_string()))
    }

    /// The frontend settings (the `ui` config section) for a host to hand to its UI. Engine logic
    /// never reads these — they exist only to be delivered to the presentation layer.
    #[must_use]
    pub fn ui_config(&self) -> UiCfg {
        self.ui.clone()
    }

    /// The curated runtime-editable settings, with current values + apply-timing — the data a host
    /// renders its settings panel from. See [`update_setting`](Self::update_setting) to change one.
    #[must_use]
    pub fn settings_schema(&self) -> Vec<SettingDescriptor> {
        let cfg = self
            .config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        build_current_schema(&cfg)
    }

    /// Change one runtime setting (Seam A). Validates the new value, persists the whole config to
    /// `config.json` **atomically** (with the API key blanked — Joi never writes the secret), resyncs
    /// the running manager (so the next connect uses it), then broadcasts the fresh settings snapshot
    /// as [`UiEvent::Settings`] for the frontend to fold.
    ///
    /// On a validation error the config is left untouched and nothing is written, so the host can
    /// surface the message and keep its current state. Errors with [`SettingsError::Io`] if no
    /// config path is available to persist to.
    pub async fn update_setting(
        &self,
        id: SettingId,
        value: SettingValue,
    ) -> Result<(), SettingsError> {
        // Work on a clone so a rejected change never mutates the live config.
        let mut next = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cfg.clone()
        };
        apply_setting(&mut next, id, &value)?; // validates; returns early on bad value

        // Durably persist before adopting the change, so the on-disk config always reflects what
        // the engine is running. `write_json` is atomic and blanks the API key.
        let path = self
            .config_path
            .as_ref()
            .ok_or_else(|| SettingsError::Io("no config path to persist to".to_string()))?;
        next.write_json(path)
            .map_err(|e| SettingsError::Io(e.to_string()))?;

        // Adopt it locally, and build the snapshot for the frontend from the *new* config (this is
        // where provider voices are resolved — only the app can, so the manager doesn't emit it).
        let snapshot = {
            let mut cfg = self
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *cfg = next.clone();
            build_current_schema(&cfg)
        };
        // Resync the manager (so the next connect uses the change) and broadcast the snapshot. No
        // manager (no key) → the on-disk change still stands; there are simply no subscribers.
        if let Some(h) = &self.handle {
            h.update_config(next)
                .await
                .map_err(|e| SettingsError::Io(e.to_string()))?;
            h.broadcast_settings(snapshot);
        }
        Ok(())
    }

    /// Subscribe to UI events (`None` if no session is configured).
    #[must_use]
    pub fn subscribe_events(&self) -> Option<broadcast::Receiver<UiEvent>> {
        self.handle.as_ref().map(SessionManagerHandle::subscribe)
    }

    /// Subscribe to provider audio-out frames (24 kHz mono PCM16) — for headless hosts.
    #[must_use]
    pub fn subscribe_audio(&self) -> Option<broadcast::Receiver<Vec<i16>>> {
        self.handle
            .as_ref()
            .map(SessionManagerHandle::subscribe_audio)
    }
}
