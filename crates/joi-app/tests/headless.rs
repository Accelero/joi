//! The **headless gate**: build [`JoiApp`] with `MediaMode::None` +
//! the Mock provider and drive a full command→event loop with no devices and no GUI. If this
//! passes, Seam A is honest — a frontend is pure presentation over the same engine.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use joi_app::{JoiApp, MediaMode};
use joi_core::config::{Config, ProviderName};
use joi_core::history::Role;
use joi_core::session::event::{Speaker, UiEvent};
use joi_core::settings::{SettingId, SettingValue};
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
    // session commands return a clear error instead of panicking.
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
async fn update_setting_persists_resyncs_and_rebroadcasts() {
    // The runtime settings loop end-to-end over Seam A: change a setting → it's validated, persisted
    // to config.json atomically (key blanked), the schema reflects it, and `JoiApp` broadcasts a
    // UiEvent::Settings the frontend would fold. Uses Accent (provider-independent) for the mechanics;
    // voice-catalog specifics are covered by the joi-providers/joi-core unit tests.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    let mut config = mock_config();
    // `update_setting` runs `Config::validate()` as a safety net, which requires a model.
    config.live_api.gemini.model = "mock-model".to_string();
    let app = JoiApp::build_with_config_path(config, MediaMode::None, Some(config_path.clone()));
    let mut events = app.subscribe_events().expect("events");

    let accent_value = |app: &JoiApp| {
        app.settings_schema()
            .into_iter()
            .find(|d| d.id == SettingId::Accent)
            .map(|d| d.value)
    };
    assert_eq!(
        accent_value(&app),
        Some(SettingValue::Text("#9aede4".to_string()))
    );

    app.update_setting(SettingId::Accent, SettingValue::Text("#ff0066".to_string()))
        .await
        .expect("accent is editable");

    // The in-memory schema reflects the change…
    assert_eq!(
        accent_value(&app),
        Some(SettingValue::Text("#ff0066".to_string()))
    );

    // …it was persisted to config.json (valid JSON, new accent, no secret on disk)…
    let written = std::fs::read_to_string(&config_path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(json["ui"]["terminal"]["accent"], "#ff0066");
    assert_eq!(json["live_api"]["gemini"]["api_key"], "");

    // …and `JoiApp` broadcast the fresh snapshot for the frontend to fold. The snapshot carries the
    // new accent AND a Voice descriptor whose options come from the provider catalog (Mock → empty),
    // proving the app threads `voice_catalog` into the schema.
    let mut snapshot = None;
    for _ in 0..20 {
        if let UiEvent::Settings { settings } = next_ui(&mut events).await {
            snapshot = Some(settings);
            break;
        }
    }
    let snapshot = snapshot.expect("a UiEvent::Settings broadcast");
    let accent = snapshot.iter().find(|d| d.id == SettingId::Accent).unwrap();
    assert_eq!(accent.value, SettingValue::Text("#ff0066".to_string()));
    let voice = snapshot.iter().find(|d| d.id == SettingId::Voice).unwrap();
    assert_eq!(
        voice.kind,
        joi_core::settings::SettingKind::Choice { options: vec![] },
        "Mock provider offers no voices → empty Choice options"
    );

    // A bad value type is rejected, leaves the config untouched, and writes nothing new.
    let err = app
        .update_setting(SettingId::Accent, SettingValue::Bool(true))
        .await
        .expect_err("wrong value type is rejected");
    assert!(matches!(
        err,
        joi_core::error::SettingsError::InvalidValue { .. }
    ));
    let after_reject = std::fs::read_to_string(&config_path).unwrap();
    assert_eq!(
        after_reject, written,
        "a rejected change must not rewrite config.json"
    );
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

    // A frontend repopulating its feed on resume can read the persisted turns over Seam A. The
    // first conversation recorded a user turn (the typed text) and the mock's echoed agent reply.
    let turns = app.session_turns(&current.id).await.unwrap();
    assert!(
        turns.len() >= 2,
        "expected user + agent turns, got {turns:?}"
    );
    assert_eq!(turns[0].role, Role::User);
    assert_eq!(turns[0].text, "first conversation topic");
    assert_eq!(turns[1].role, Role::Assistant);
    assert!(
        turns[1].text.contains("echo: first conversation topic"),
        "agent turn should be the mock's echoed reply, got {:?}",
        turns[1].text
    );
}
