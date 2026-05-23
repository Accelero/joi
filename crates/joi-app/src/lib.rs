//! Host-agnostic application layer for JOI (Seam A). A host — the Tauri shell, the `joi-cli` binary,
//! or a future HTTP/WS server — builds a [`JoiApp`] from a [`Config`] and drives it via these
//! methods plus the event/audio streams. No Tauri, webview, HTTP, or CLI types appear here, so the
//! engine can be operated headlessly.
//!
//! The command set mirrors the IPC surface (SPEC §11.1): [`JoiApp::start`]/[`stop`](JoiApp::stop)/
//! [`send_text`](JoiApp::send_text)/[`set_mic_muted`](JoiApp::set_mic_muted)/
//! [`start_screenshare`](JoiApp::start_screenshare)/[`stop_screenshare`](JoiApp::stop_screenshare)/
//! [`has_api_key`](JoiApp::has_api_key). Outputs are the [`subscribe_events`](JoiApp::subscribe_events)
//! `UiEvent` stream and, for headless hosts, the [`send_audio`](JoiApp::send_audio) /
//! [`subscribe_audio`](JoiApp::subscribe_audio) raw-audio transport.

use std::sync::Arc;

use joi_core::clock::SystemClock;
use joi_core::config::{Config, UiCfg};
use joi_core::error::SessionError;
use joi_core::history::InMemoryHistory;
use joi_core::manager::{SessionFactory, SessionManager, SessionManagerHandle};
use joi_core::media::AudioFormat;
use joi_core::session::event::UiEvent;
use joi_media::{MediaConfig, MediaEngine};
use tokio::sync::broadcast;

/// Whether the engine drives local audio/screen devices itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode {
    /// Desktop: native cpal mic/playback + xcap screen, bound to the session.
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
    /// Web-frontend settings, surfaced to hosts via [`JoiApp::ui_config`] (Seam B's `get_ui_config`).
    ui: UiCfg,
}

impl JoiApp {
    /// Composition root. **Must be called inside a Tokio runtime** — the session manager and the
    /// media engine spawn tasks with `tokio::spawn`.
    #[must_use]
    pub fn build(config: Config, media_mode: MediaMode) -> Self {
        let has_key = config.live_api.gemini.api_key.is_set();
        // Keep the UI section before `config` is moved into the manager (cheap clone).
        let ui = config.ui.clone();
        match joi_providers::build_session_factory(&config) {
            Ok(factory) => {
                let factory: Arc<dyn SessionFactory> = Arc::from(factory);
                let clock = Arc::new(SystemClock);
                let history = Arc::new(InMemoryHistory::new());
                // Read what local media needs from `config` before moving it into the manager.
                let media_config = MediaConfig {
                    frame_samples: AudioFormat::INPUT
                        .samples_per_frame(config.media.audio.frame_ms),
                    screen_fps: config.media.screen.fps,
                    screen_max_width: config.media.screen.max_width,
                    screen_quality: config.media.screen.quality,
                    echo_cancellation: config.media.audio.echo_cancellation,
                    noise_suppression: config.media.audio.noise_suppression,
                    auto_gain: config.media.audio.auto_gain,
                };
                let handle = SessionManager::spawn(config, clock, history, factory);
                let media = match media_mode {
                    MediaMode::LocalDevices => Some(MediaEngine::new(handle.clone(), media_config)),
                    MediaMode::None => None,
                };
                Self {
                    handle: Some(handle),
                    media,
                    has_key,
                    ui,
                }
            }
            Err(e) => {
                tracing::warn!("session unavailable until configured: {e}");
                Self {
                    handle: None,
                    media: None,
                    has_key,
                    ui,
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

    /// Push a mic frame (16 kHz mono PCM16) — for headless hosts; the desktop's `MediaEngine` feeds
    /// audio itself.
    pub async fn send_audio(&self, pcm: Vec<i16>) -> Result<(), SessionError> {
        self.session()?.send_audio(pcm).await
    }

    /// App-level mute (no-op without local media).
    pub fn set_mic_muted(&self, muted: bool) {
        if let Some(m) = &self.media {
            m.set_mic_muted(muted);
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

    /// The web-frontend settings (the `ui` config section) for a host to hand to its UI. Engine
    /// logic never reads these — they exist only to be delivered to the presentation layer.
    #[must_use]
    pub fn ui_config(&self) -> UiCfg {
        self.ui.clone()
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
