//! Provider adapters implementing [`joi_core::session::RealtimeSession`].
//!
//! Each adapter absorbs all provider-specific divergence behind the trait (SPEC §2). The realtime
//! SDK (vendored `adk-realtime`) is an implementation detail *inside* [`gemini`] — it never leaks
//! past the trait, so the founding constraint stays ours, not a dependency's (PLAN §5).
//!
//! - [`mock`] — scripted, no network; drives the headless gate and the conformance suite.
//! - [`gemini`] — Gemini Live via the vendored `adk-realtime` (feature `gemini`).

pub mod factory;
#[cfg(feature = "gemini")]
pub mod gemini;
#[cfg(feature = "mock")]
pub mod mock;

pub use factory::{build_connectivity_probe, build_session_factory, voice_catalog, FactoryError};
#[cfg(feature = "mock")]
pub use mock::MockSession;
