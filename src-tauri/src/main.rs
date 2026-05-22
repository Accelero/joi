#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Joi Tauri shell: the composition root that wires [`Config`], the [`SecretStore`], and the
//! [`SessionManager`] actor together, plus the IPC command surface mirrored by `src/ipc.ts`
//! (SPEC §11). All provider/session logic lives in the inner crates; this binary is the thin edge.

mod secret_store;

use std::sync::Arc;

use joi_core::clock::SystemClock;
use joi_core::config::Config;
use joi_core::history::InMemoryHistory;
use joi_core::manager::{SessionFactory, SessionManager, SessionManagerHandle};
use joi_core::media::AudioFormat;
use joi_core::secrets::SecretStore;
use joi_media::{MediaConfig, MediaEngine};
use secrecy::SecretString;
use serde::Serialize;
use tauri::{async_runtime, Emitter, Manager, State};
use tokio::sync::broadcast::error::RecvError;

use secret_store::EnvWithOverlayStore;

/// Shared application context held in Tauri-managed state. Domain work lives in the inner crates;
/// this struct just holds the handles the commands dispatch to. `handle`/`media` are `None` until a
/// provider is configured (e.g. no API key at startup) — the window still opens and session
/// commands return a clear error rather than crashing.
struct AppCtx {
    handle: Option<SessionManagerHandle>,
    secrets: Arc<dyn SecretStore>,
    media: Option<MediaEngine>,
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
    // Audio/screen I/O is native (joi-media's MediaEngine) — nothing crosses to the webview.
    ctx.session()?
        .start(resume)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(media) = &ctx.media {
        media.start_capture();
    }
    Ok(StartResult {
        session_id: "session".to_string(),
    })
}

#[tauri::command]
async fn stop(pause: bool, ctx: State<'_, AppCtx>) -> Result<(), String> {
    if let Some(media) = &ctx.media {
        media.stop_capture(); // stop mic capture before tearing down the session
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
    if let Some(media) = &ctx.media {
        media.set_mic_muted(muted);
    }
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn start_screenshare(ctx: State<'_, AppCtx>) {
    if let Some(media) = &ctx.media {
        media.start_screenshare();
    }
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn stop_screenshare(ctx: State<'_, AppCtx>) {
    if let Some(media) = &ctx.media {
        media.stop_screenshare();
    }
}

#[allow(clippy::too_many_lines)] // composition root: linear wiring reads better in one place
fn main() -> anyhow::Result<()> {
    // Debug for Joi's own crates by default (deps stay at info to avoid raw-event/tauri noise);
    // RUST_LOG overrides entirely when set.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "info,joi=debug,joi_core=debug,joi_media=debug,joi_providers=debug",
                )
            }),
        )
        .init();

    let config = Config::load(None)?;
    let secrets: Arc<dyn SecretStore> = Arc::new(EnvWithOverlayStore::new());
    let media_config = MediaConfig {
        frame_samples: AudioFormat::INPUT.samples_per_frame(config.audio.frame_ms),
        screen_fps: config.screen.fps,
        screen_max_width: config.screen.max_width,
        screen_quality: config.screen.quality,
    };

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            ping,
            has_api_key,
            set_api_key,
            start,
            stop,
            send_text,
            set_mic_muted,
            start_screenshare,
            stop_screenshare
        ])
        .setup(move |app| {
            let cfg = config.clone();
            let secrets_for_factory = Arc::clone(&secrets);
            // Build the manager and its MediaEngine together inside the runtime: we read the key
            // first, and both the manager and the engine spawn tasks with `tokio::spawn`, which
            // needs the runtime context this block enters. A missing key (or any factory error) is
            // non-fatal: the window still opens and session commands report it.
            let (handle, media): (Option<SessionManagerHandle>, Option<MediaEngine>) =
                async_runtime::block_on(async move {
                    let api_key = secrets_for_factory.get_api_key().await.ok().flatten();
                    match joi_providers::build_session_factory(&cfg, api_key) {
                        Ok(factory) => {
                            let factory: Arc<dyn SessionFactory> = Arc::from(factory);
                            let clock = Arc::new(SystemClock);
                            let history = Arc::new(InMemoryHistory::new());
                            let handle =
                                SessionManager::spawn(cfg.clone(), clock, history, factory);
                            let media = MediaEngine::new(handle.clone(), media_config);
                            (Some(handle), Some(media))
                        }
                        Err(e) => {
                            tracing::warn!("session unavailable until configured: {e}");
                            (None, None)
                        }
                    }
                });

            // The one media-adjacent task that genuinely belongs to the shell: fan provider/UI
            // events out to the webview (SPEC §11.3). All native media lives in the MediaEngine.
            if let Some(handle) = &handle {
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
            }

            app.manage(AppCtx {
                handle,
                secrets: Arc::clone(&secrets),
                media,
            });
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
