#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Joi Tauri shell: a thin host adapter over the [`JoiApp`] engine (Seam A). It only translates
//! `#[tauri::command]`s into `JoiApp` calls and fans the `UiEvent` stream out to the webview as the
//! `"ui_event"` IPC channel (Seam B, SPEC §11). All composition and domain logic live in `joi-app`
//! and the inner crates; this binary holds no engine state of its own.

use joi_app::{JoiApp, MediaMode};
use joi_core::config::Config;
use serde::Serialize;
use tauri::{async_runtime, Emitter, Manager, State};
use tokio::sync::broadcast::error::RecvError;

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
#[allow(clippy::needless_pass_by_value)] // Tauri commands take `State` by value
fn has_api_key(app: State<'_, JoiApp>) -> HasApiKeyResult {
    HasApiKeyResult {
        present: app.has_api_key(),
    }
}

#[tauri::command]
async fn start(resume: bool, app: State<'_, JoiApp>) -> Result<StartResult, String> {
    app.start(resume).await.map_err(|e| e.to_string())?;
    Ok(StartResult {
        session_id: "session".to_string(),
    })
}

#[tauri::command]
async fn stop(pause: bool, app: State<'_, JoiApp>) -> Result<(), String> {
    app.stop(pause).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_text(text: String, app: State<'_, JoiApp>) -> Result<(), String> {
    app.send_text(&text).await.map_err(|e| e.to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn set_mic_muted(muted: bool, app: State<'_, JoiApp>) {
    app.set_mic_muted(muted);
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn start_screenshare(app: State<'_, JoiApp>) {
    app.start_screenshare();
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn stop_screenshare(app: State<'_, JoiApp>) {
    app.stop_screenshare();
}

fn main() -> anyhow::Result<()> {
    // (WebKitGTK's blank-window workaround lives in `.cargo/config.toml [env]` — it must be set
    // before the process starts, so it can't be done here.)

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

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            ping,
            has_api_key,
            start,
            stop,
            send_text,
            set_mic_muted,
            start_screenshare,
            stop_screenshare
        ])
        .setup(move |tauri_app| {
            // Build the engine inside the runtime (it spawns tasks). Desktop = local devices.
            let app =
                async_runtime::block_on(async { JoiApp::build(config, MediaMode::LocalDevices) });

            // The one Tauri-specific bridge: fan UiEvents out to the webview (SPEC §11.3).
            if let Some(mut ui_rx) = app.subscribe_events() {
                let emitter = tauri_app.handle().clone();
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

            tauri_app.manage(app);
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
