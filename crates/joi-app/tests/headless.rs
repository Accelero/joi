//! The **headless gate** (PLAN §3 deviation #2, §9 M5): build [`JoiApp`] with `MediaMode::None` +
//! the Mock provider and drive a full command→event loop with no devices and no GUI. If this
//! passes, Seam A is honest — a frontend is pure presentation over the same engine.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use joi_app::{JoiApp, MediaMode};
use joi_core::config::{Config, ProviderName};
use joi_core::session::event::{Speaker, UiEvent};
use tokio::sync::broadcast::error::RecvError;

/// A config that drives the scripted Mock provider with no persisted sessions (in-memory history).
fn mock_config() -> Config {
    let mut config = Config::default();
    config.live_api.provider = ProviderName::Mock;
    config.history.dir = None; // → in-memory fallback
    config
}

/// Read the next `UiEvent`, failing the test on timeout/closure (lagged is skipped).
async fn next_ui(rx: &mut tokio::sync::broadcast::Receiver<UiEvent>) -> UiEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(ev)) => return ev,
            Ok(Err(RecvError::Lagged(_))) => {}
            other => panic!("expected a UI event, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn headless_full_command_event_loop() {
    let app = JoiApp::build(mock_config(), MediaMode::None);
    let mut events = app
        .subscribe_events()
        .expect("a configured engine exposes events");

    app.start(false).await.expect("start the mock session");
    app.send_text("hello headless").await.expect("send text");

    // Fold the UiEvent stream like a frontend would: prove the user line and the agent's assembled
    // reply both surface over Seam A.
    let mut saw_user = false;
    let mut agent_reply = String::new();
    let mut agent_done = false;
    for _ in 0..40 {
        match next_ui(&mut events).await {
            UiEvent::Transcript {
                speaker: Speaker::User,
                text,
                final_: true,
            } if text == "hello headless" => saw_user = true,
            UiEvent::Transcript {
                speaker: Speaker::Agent,
                text,
                final_,
            } => {
                agent_reply.push_str(&text);
                if final_ && agent_reply.contains("echo: hello headless") {
                    agent_done = true;
                }
            }
            _ => {}
        }
        if saw_user && agent_done {
            break;
        }
    }
    assert!(saw_user, "user transcript should surface over Seam A");
    assert!(
        agent_done,
        "agent reply should assemble from streamed deltas"
    );

    app.stop(false).await.expect("stop the session");
}

#[tokio::test]
async fn no_api_key_falls_back_without_panicking() {
    // Default config selects Gemini with an empty key → no factory, so the engine constructs but
    // session commands return a clear error instead of panicking (PLAN §8 policy).
    let mut config = Config::default();
    config.live_api.gemini.api_key = joi_core::config::ApiKey::default(); // explicit: unset
    config.history.dir = None;
    let app = JoiApp::build(config, MediaMode::None);

    assert!(!app.has_api_key());
    assert!(app.subscribe_events().is_none());
    assert!(
        app.start(false).await.is_err(),
        "start without a key errors"
    );
    assert!(app.send_text("hi").await.is_err());
    assert!(app.list_sessions().await.is_empty());
    assert!(app.current_session().await.is_none());
    assert!(app.new_session().await.is_err());
}

#[tokio::test]
async fn sessions_persist_list_and_switch() {
    // Mock provider + a real sessions dir: prove the persisted-session commands work end-to-end
    // (FR-18..20) headlessly.
    let dir = tempfile::tempdir().unwrap();
    let mut config = mock_config();
    config.history.dir = Some(dir.path().to_path_buf());
    let app = JoiApp::build(config, MediaMode::None);

    let mut events = app.subscribe_events().expect("events");
    app.start(false).await.unwrap();
    app.send_text("first conversation topic").await.unwrap();

    // Wait until the first user turn is persisted (the History meta event signals the append).
    let mut persisted = false;
    for _ in 0..50 {
        if let UiEvent::History(meta) = next_ui(&mut events).await {
            if meta.turns >= 1 {
                persisted = true;
                break;
            }
        }
    }
    assert!(
        persisted,
        "the user turn should be appended to the session log"
    );

    // The current session is auto-named from that first user message (FR-19).
    let current = app.current_session().await.expect("a current session");
    assert_eq!(
        current.meta.name.as_deref(),
        Some("first conversation topic")
    );
    assert_eq!(app.list_sessions().await.len(), 1);

    // Starting a new session leaves the old one resumable (FR-20).
    let fresh = app.new_session().await.unwrap();
    assert_ne!(fresh.id, current.id);
    let listed = app.list_sessions().await;
    assert_eq!(listed.len(), 2);

    // Resume the original by id; it becomes current again.
    let resumed = app.resume_session(&current.id).await.unwrap();
    assert_eq!(resumed.id, current.id);
    assert_eq!(app.current_session().await.unwrap().id, current.id);
}
