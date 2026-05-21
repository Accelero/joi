//! `joi-core` — the pure domain layer for Joi.
//!
//! This crate defines the **ports** (traits) the rest of the system depends on and the pure
//! logic that sits behind them. It has **zero** Tauri and **zero** provider-SDK dependencies, so
//! the whole conversational loop is unit-testable without a network or a GUI (PLAN §1).
//!
//! The ports are:
//! - [`session::RealtimeSession`] — a provider-agnostic realtime voice session (SPEC §4).
//! - [`history::HistoryStore`] — bounded, restorable conversation history (SPEC §6).
//! - [`secrets::SecretStore`] — the API key, returned as a redacting [`secrecy::SecretString`].
//! - [`capture::ScreenSource`] — screen enumeration and capture (SPEC §7.3).
//! - [`clock::Clock`] — injected time, so tests are deterministic.
//!
//! The [`manager::SessionManager`] is an **actor** that owns a [`session::RealtimeSession`], a
//! [`history::HistoryStore`], and the [`config::Config`], and serves commands over a channel
//! (PLAN §1, §6). Concrete adapters for these ports live in outer crates (`joi-providers`,
//! `src-tauri`); only the composition root wires them together.

pub mod capture;
pub mod clock;
pub mod config;
pub mod error;
pub mod history;
pub mod manager;
pub mod media;
pub mod secrets;
pub mod session;
pub mod tools;

pub use clock::{Clock, SystemClock};
pub use config::Config;
pub use error::{CaptureError, ConfigError, HistoryError, SecretError, SessionError};
pub use manager::{Command, SessionManager, SessionManagerHandle};
pub use session::event::{SessionEvent, Speaker, TurnEvent, UiEvent};
pub use session::{Capabilities, RealtimeSession, SessionConfig};
