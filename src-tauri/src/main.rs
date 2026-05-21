#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Joi Tauri shell: the composition root that wires [`Config`], the [`SecretStore`], and the
//! [`SessionManager`] actor together, plus the IPC command surface mirrored by `src/ipc.ts`
//! (SPEC §11). All provider/session logic lives in the inner crates; this binary is the thin edge.

mod secret_store;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use joi_core::clock::SystemClock;
use joi_core::config::Config;
use joi_core::history::InMemoryHistory;
use joi_core::manager::{SessionFactory, SessionManager, SessionManagerHandle};
use joi_core::media::AudioFormat;
use joi_core::secrets::SecretStore;
use secrecy::SecretString;
use serde::Serialize;
use tauri::{async_runtime, Emitter, Manager, State};
use tokio::sync::broadcast::error::RecvError;

use secret_store::EnvWithOverlayStore;

/// Shared application context held in Tauri-managed state.
struct AppCtx {
    /// `None` until a usable provider is configured (e.g. no API key at startup) — the window still
    /// opens; session commands then return a clear error rather than crashing the app.
    handle: Option<SessionManagerHandle>,
    secrets: Arc<dyn SecretStore>,
    /// Native mic frames are pushed here and drained by a forwarder into the session.
    cap_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    /// Active mic capture (present only while a session runs); dropping it stops the stream.
    capture: Mutex<Option<joi_media::CaptureHandle>>,
    /// App-level mute: gates native capture at the source, independent of session state.
    mic_muted: Arc<AtomicBool>,
    /// Samples per input frame (e.g. 320 at 16 kHz / 20 ms), from config.
    frame_samples: usize,
}

impl AppCtx {
    fn session(&self) -> Result<&SessionManagerHandle, String> {
        self.handle
            .as_ref()
            .ok_or_else(|| "No API key set. Set GEMINI_API_KEY and restart Joi.".to_string())
    }
}

#[derive(Serialize)]
struct HasApiKeyResult {
    present: bool,
}

#[derive(Serialize)]
struct StartResult {
    session_id: String,
}

#[tauri::command]
fn ping() -> &'static str {
    "pong"
}

#[tauri::command]
async fn has_api_key(ctx: State<'_, AppCtx>) -> Result<HasApiKeyResult, String> {
    let present = ctx.secrets.has_api_key().await.map_err(|e| e.to_string())?;
    Ok(HasApiKeyResult { present })
}

#[tauri::command]
async fn set_api_key(key: String, ctx: State<'_, AppCtx>) -> Result<(), String> {
    ctx.secrets
        .set_api_key(SecretString::from(key))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn start(resume: bool, ctx: State<'_, AppCtx>) -> Result<StartResult, String> {
    // Audio I/O is native (joi-media) — nothing crosses to the webview.
    ctx.session()?.start(resume).await.map_err(|e| e.to_string())?;
    // Begin native mic capture for this session; the manager gates muted audio.
    if let Ok(mut cap) = ctx.capture.lock() {
        *cap = Some(joi_media::spawn_capture(
            ctx.cap_tx.clone(),
            ctx.frame_samples,
            Arc::clone(&ctx.mic_muted),
        ));
    }
    Ok(StartResult {
        session_id: "session".to_string(),
    })
}

#[tauri::command]
async fn stop(pause: bool, ctx: State<'_, AppCtx>) -> Result<(), String> {
    if let Ok(mut cap) = ctx.capture.lock() {
        *cap = None; // stop mic capture before tearing down the session
    }
    ctx.session()?.stop(pause).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_text(text: String, ctx: State<'_, AppCtx>) -> Result<(), String> {
    ctx.session()?
        .send_text(text)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri commands take `State` by value
fn set_mic_muted(muted: bool, ctx: State<'_, AppCtx>) {
    // App-level mute: gates native capture regardless of whether a session is running.
    ctx.mic_muted.store(muted, Ordering::Relaxed);
}

#[allow(clippy::too_many_lines)] // composition root: linear wiring reads better in one place
fn main() -> anyhow::Result<()> {
    // Debug for Joi's own crates by default (deps stay at info to avoid raw-event/tauri noise);
    // RUST_LOG overrides entirely when set.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| {
                tracing_subscriber::EnvFilter::new(
                    "info,joi=debug,joi_core=debug,joi_media=debug,joi_providers=debug",
                )
            },
        ))
        .init();

    let config = Config::load(None)?;
    let secrets: Arc<dyn SecretStore> = Arc::new(EnvWithOverlayStore::new());
    let frame_samples = AudioFormat::INPUT.samples_per_frame(config.audio.frame_ms);
    let (cap_tx, mut cap_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(64);
    let mic_muted = Arc::new(AtomicBool::new(false));

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            ping,
            has_api_key,
            set_api_key,
            start,
            stop,
            send_text,
            set_mic_muted
        ])
        .setup(move |app| {
            // Build the session manager on the async runtime so its internal `tokio::spawn` has a
            // runtime context, and so we can read the key before spawning. A missing key (or any
            // factory error) is non-fatal: the window still opens and session commands report it.
            let cfg = config.clone();
            let secrets_for_factory = Arc::clone(&secrets);
            let handle: Option<SessionManagerHandle> = async_runtime::block_on(async move {
                let api_key = secrets_for_factory.get_api_key().await.ok().flatten();
                match joi_providers::build_session_factory(&cfg, api_key) {
                    Ok(factory) => {
                        let factory: Arc<dyn SessionFactory> = Arc::from(factory);
                        let clock = Arc::new(SystemClock);
                        let history = Arc::new(InMemoryHistory::new());
                        Some(SessionManager::spawn(cfg.clone(), clock, history, factory))
                    }
                    Err(e) => {
                        tracing::warn!("session unavailable until configured: {e}");
                        None
                    }
                }
            });

            if let Some(handle) = &handle {
                // Fan out UI events to the webview (SPEC §11.3).
                let emitter = app.handle().clone();
                let mut ui_rx = handle.subscribe();
                async_runtime::spawn(async move {
                    loop {
                        match ui_rx.recv().await {
                            Ok(event) => {
                                let _ = emitter.emit("ui_event", event);
                            }
                            Err(RecvError::Closed) => break,
                            Err(RecvError::Lagged(_)) => {}
                        }
                    }
                });

                // Render provider audio natively via the cpal engine (no webview audio). An empty
                // frame is the manager's barge-in sentinel → flush the playback buffer (FR-2).
                let playback_tx = joi_media::spawn_playback();
                let mut audio_rx = handle.subscribe_audio();
                async_runtime::spawn(async move {
                    loop {
                        match audio_rx.recv().await {
                            Ok(pcm) => {
                                let cmd = if pcm.is_empty() {
                                    joi_media::PlaybackCmd::Flush
                                } else {
                                    joi_media::PlaybackCmd::Pcm(pcm)
                                };
                                if playback_tx.send(cmd).is_err() {
                                    break;
                                }
                            }
                            Err(RecvError::Closed) => break,
                            Err(RecvError::Lagged(_)) => {}
                        }
                    }
                });

                // Drain native mic frames into the session (manager gates muted audio).
                let session = handle.clone();
                async_runtime::spawn(async move {
                    while let Some(frame) = cap_rx.recv().await {
                        if session.send_audio(frame).await.is_err() {
                            break;
                        }
                    }
                });
            }

            app.manage(AppCtx {
                handle,
                secrets: Arc::clone(&secrets),
                cap_tx,
                capture: Mutex::new(None),
                mic_muted,
                frame_samples,
            });
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
