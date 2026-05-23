//! Headless host for the JOI engine. Proves the backend runs with **no Tauri / no GUI**: it builds
//! the same [`JoiApp`] the desktop shell uses (here in `MediaMode::None` — text only) and drives it
//! from stdin, printing transcripts as they stream.
//!
//! Commands (one per line): `start`, `stop`, `quit`; anything else is sent to the model as text.

use std::io::{BufRead, Write};

use joi_app::{JoiApp, MediaMode};
use joi_core::config::Config;
use joi_core::session::event::UiEvent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs to stderr so stdout carries only transcripts (clean for piping/headless use).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,joi=info")),
        )
        .init();

    let config = Config::load(None)?;
    let app = JoiApp::build(config, MediaMode::None);
    if !app.has_api_key() {
        eprintln!("warning: no API key set (GEMINI_API_KEY or live_api.gemini.api_key)");
    }

    // Stream transcripts to stdout as the engine emits them.
    if let Some(mut events) = app.subscribe_events() {
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let UiEvent::Transcript { speaker, text, .. } = event {
                    print!("[{speaker:?}] {text}");
                    let _ = std::io::stdout().flush();
                }
            }
        });
    }

    eprintln!("commands: start | stop | quit | <anything else> = send as text");
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF
        }
        match line.trim() {
            "" => {}
            "quit" => break,
            "start" => app.start(false).await?,
            "stop" => app.stop(false).await?,
            text => app.send_text(text).await?,
        }
    }

    let _ = app.stop(false).await;
    Ok(())
}
