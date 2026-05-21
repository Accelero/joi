//! Provider adapters implementing [`joi_core::session::RealtimeSession`].
//!
//! Each adapter absorbs all provider-specific divergence behind the trait (SPEC §4). adk-rust (the
//! chosen realtime SDK, SPEC §4.5) is an implementation detail *inside* [`gemini`] — it never leaks
//! past the trait, so the founding constraint stays ours, not a dependency's.
//!
//! - [`mock`] — scripted, no network; drives the M1 loop and the conformance suite.
//! - [`gemini`] — `[M2]` Gemini Live via adk-rust. Stub until the M2 API spike (PLAN §0, M2).
//! - [`openai`] — `[POST]` compile-only stub that keeps the abstraction honest (SPEC §4.4).

pub mod factory;
#[cfg(feature = "gemini")]
pub mod gemini;
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "openai")]
pub mod openai;

pub use factory::{build_session_factory, FactoryError};
#[cfg(feature = "mock")]
pub use mock::MockSession;
