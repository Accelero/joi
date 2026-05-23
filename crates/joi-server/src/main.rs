//! HTTP/WS host for the JOI engine — a third host besides the Tauri shell and `joi-cli`, proving
//! [Seam A](joi_app::JoiApp) works over the network. It serves the **same** contract as the Tauri
//! IPC (Seam B): JSON commands in, the `UiEvent` stream out — only the transport is a WebSocket
//! instead of Tauri `invoke`/`emit`.
//!
//! Built in [`MediaMode::None`](joi_app::MediaMode::None) (headless); streaming binary audio over the
//! socket is a future extension. One shared [`JoiApp`] backs all connections, so every client sees
//! the one session's events — a deliberate simplification for a reference adapter.
//!
//! - `GET /`     — health check (`joi-server ok`).
//! - `GET /ws`   — upgrade to a WebSocket. Inbound text frames are [`ClientCommand`] JSON; outbound
//!   text frames are `UiEvent` JSON.
//!
//! Bind address: `JOI_SERVER_ADDR` (default `127.0.0.1:8765`) — host runtime config, like the dev
//! server's port, so it is intentionally not part of the engine's YAML.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use joi_app::{JoiApp, MediaMode};
use joi_core::config::Config;
use serde::Deserialize;

/// A command a WebSocket client sends (mirrors the Tauri command surface, SPEC §11.1).
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
enum ClientCommand {
    /// Start (or resume) a session.
    Start {
        #[serde(default)]
        resume: bool,
    },
    /// Stop (or pause) the session.
    Stop {
        #[serde(default)]
        pause: bool,
    },
    /// Send a text turn to the model.
    Text {
        /// The message text.
        text: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,joi=info")),
        )
        .init();

    let config = Config::load(None)?;
    // Headless: clients drive the session over the socket; no local mic/speaker/screen.
    let app = Arc::new(JoiApp::build(config, MediaMode::None));
    if !app.has_api_key() {
        tracing::warn!(
            "no API key set (GEMINI_API_KEY or live_api.gemini.api_key) — start will fail"
        );
    }

    let addr: SocketAddr = std::env::var("JOI_SERVER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8765".to_string())
        .parse()?;

    let router = Router::new()
        .route("/", get(|| async { "joi-server ok" }))
        .route("/ws", get(ws_upgrade))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("joi-server listening on ws://{addr}/ws");
    axum::serve(listener, router).await?;
    Ok(())
}

/// Upgrade an HTTP request to a WebSocket bound to the shared engine.
async fn ws_upgrade(ws: WebSocketUpgrade, State(app): State<Arc<JoiApp>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

/// Pump `UiEvent`s out to the client and route inbound JSON commands into the engine until the
/// socket closes.
async fn handle_socket(socket: WebSocket, app: Arc<JoiApp>) {
    let (mut sender, mut receiver) = socket.split();

    // Outbound: forward this connection's view of the UiEvent stream as JSON text frames.
    let mut forward = app.subscribe_events().map(|mut events| {
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                match serde_json::to_string(&event) {
                    Ok(json) => {
                        if sender.send(Message::text(json)).await.is_err() {
                            break; // client went away
                        }
                    }
                    Err(e) => tracing::warn!("dropping unserializable UiEvent: {e}"),
                }
            }
        })
    });

    // Inbound: parse each text frame as a command and drive the engine.
    while let Some(Ok(msg)) = receiver.next().await {
        let Message::Text(text) = msg else {
            if matches!(msg, Message::Close(_)) {
                break;
            }
            continue;
        };
        match serde_json::from_str::<ClientCommand>(&text) {
            Ok(cmd) => dispatch(&app, cmd).await,
            Err(e) => tracing::warn!("ignoring malformed command {text:?}: {e}"),
        }
    }

    // Client disconnected: stop forwarding events to this (now-dead) socket.
    if let Some(handle) = forward.take() {
        handle.abort();
    }
}

/// Apply one client command to the engine, logging any engine-level error (the socket stays open).
async fn dispatch(app: &JoiApp, cmd: ClientCommand) {
    let result = match cmd {
        ClientCommand::Start { resume } => app.start(resume).await,
        ClientCommand::Stop { pause } => app.stop(pause).await,
        ClientCommand::Text { text } => app.send_text(&text).await,
    };
    if let Err(e) = result {
        tracing::warn!("command failed: {e}");
    }
}
