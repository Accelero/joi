//! `joi-core` — the pure domain layer for Joi.
//!
//! This crate defines the **ports** (traits) the rest of the system depends on and the pure
//! logic that sits behind them. It has **zero** device I/O and **zero** provider-SDK dependencies,
//! so the whole conversational loop is unit-testable without a network or a GUI (PLAN §1).
//!
//! The ports are:
//! - [`session::RealtimeSession`] — a provider-agnostic realtime voice session (SPEC §2).
//! - [`history::HistoryStore`] — bounded, restorable conversation history (SPEC §3.6).
//! - [`media::ScreenSource`] — screen enumeration and capture (FR-8).
//! - [`clock::Clock`] — injected time, so tests are deterministic.
//! - [`connectivity::ConnectivityProbe`] — token-free provider reachability.
//!
//! The provider API key is part of [`config::Config`] (`live_api.gemini.api_key`, settable in the
//! YAML file or via the environment), held as a redacting [`config::ApiKey`].
//!
//! The [`manager::SessionManager`] is an **actor** that owns a [`session::RealtimeSession`], a
//! [`history::HistoryStore`], and the [`config::Config`], and serves commands over a channel
//! (PLAN §8). Concrete adapters for these ports live in outer crates (`joi-providers`,
//! `joi-media`); only the composition root (`joi-app`) wires them together.

pub mod clock;
pub mod config;
pub mod connectivity;
pub mod error;
pub mod history;
pub mod manager;
pub mod media;
pub mod metrics;
pub mod session;
pub mod settings;
pub mod tools;
pub mod util;

pub use clock::{Clock, SystemClock};
pub use config::Config;
pub use connectivity::{ConnectivityProbe, ProbeOutcome};
pub use error::{CaptureError, ConfigError, HistoryError, SessionError, SettingsError};
pub use manager::{Command, SessionFactory, SessionManager, SessionManagerHandle};
pub use session::event::{SessionEvent, Speaker, TurnEvent, UiEvent};
pub use session::{Capabilities, RealtimeSession, SessionConfig};
pub use settings::{
    apply_setting, settings_schema, ApplyTiming, SettingDescriptor, SettingId, SettingKind,
    SettingValue, SettingsContext,
};
