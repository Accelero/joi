//! Bounded, restorable conversation history (SPEC §3.6).
//!
//! History persists **text** turns (transcripts), not audio — enough to re-seed a fresh session as
//! `initial_context` on resume (FR-20/21). It is bounded by a [`TokenBudget`] sized to the
//! **Live session's input limit**, *not* the underlying text model's 1M-class window (FR-21).
//!
//! Two implementations: [`memory::InMemoryHistory`] (fallback/tests) and the persistent
//! [`session::SessionStore`] (the resumable-session unit the user manages).

pub mod memory;
pub mod session;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::clock::UnixMillis;
use crate::error::HistoryError;

pub use memory::InMemoryHistory;
pub use session::{Session, SessionMeta, SessionStore, SessionSummary};

/// Who produced a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The human user.
    User,
    /// The agent.
    Assistant,
    /// A system preamble / instruction.
    System,
}

/// One persisted dialogue turn.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HistoryTurn {
    /// Who spoke.
    pub role: Role,
    /// The finalized text content.
    pub text: String,
    /// When it was recorded (ms since Unix epoch).
    pub ts_ms: UnixMillis,
}

impl HistoryTurn {
    /// Build a turn with the given role/text/timestamp.
    #[must_use]
    pub fn new(role: Role, text: impl Into<String>, ts_ms: UnixMillis) -> Self {
        Self {
            role,
            text: text.into(),
            ts_ms,
        }
    }

    /// Approximate token cost of this turn **as it is seeded into a prompt** — its text plus the
    /// per-turn framing the provider wraps around it ([`TURN_FRAMING_TOKENS`]). Budgeting on bare
    /// text alone under-counts (the framing is never free), so this is what the budget windowing and
    /// the UI's [`HistoryMeta`] use.
    #[must_use]
    pub fn token_estimate(&self) -> u32 {
        estimate_tokens(&self.text).saturating_add(TURN_FRAMING_TOKENS)
    }
}

/// Per-turn framing overhead, in tokens, added to a turn's text estimate.
///
/// A turn is never put on the wire as bare text: the provider wraps each one in speaker/role framing
/// plus a delimiter — e.g. the Gemini adapter prefixes `"Me: "` / `"You: "` and a newline before
/// folding the turns into the seed. Counting only the text would under-budget the re-seed and let it
/// overrun the model's input window. This is a deliberately conservative, provider-agnostic estimate
/// of that per-turn framing; the exact wrapping is provider-specific, and a provider may add a small
/// one-time envelope on top (e.g. the Gemini adapter's context preamble) that is negligible next to
/// the budget.
pub const TURN_FRAMING_TOKENS: u32 = 4;

/// An approximate token budget bounding stored history (FR-21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct TokenBudget(pub u32);

/// Summary of the current history, surfaced to the UI (the `history` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct HistoryMeta {
    /// Number of stored turns.
    pub turns: usize,
    /// Sum of per-turn token estimates.
    pub token_estimate: u32,
    /// The configured budget.
    pub budget: u32,
}

/// Approximate token count of `text`.
///
/// This is the single swap point for a smarter tokenizer later; the MVP uses the chars/4 heuristic.
/// Always returns at least 1 so an empty turn still counts.
#[must_use]
pub fn estimate_tokens(text: &str) -> u32 {
    ((text.chars().count() / 4) as u32).max(1)
}

/// Persisted conversation store. Append-mostly with pruning to a token budget (FR-21/22).
#[async_trait]
pub trait HistoryStore: Send + Sync {
    /// Append a finalized turn, pruning oldest turns if the budget is exceeded.
    async fn append(&self, turn: HistoryTurn) -> Result<(), HistoryError>;

    /// Newest-first turns whose cumulative token estimate fits `budget`.
    ///
    /// The returned set is **guaranteed re-seedable**: its total estimate is ≤ `budget`, so it can
    /// be passed straight back as `SessionConfig.initial_context` (FR-20/21).
    async fn load_within_budget(
        &self,
        budget: TokenBudget,
    ) -> Result<Vec<HistoryTurn>, HistoryError>;

    /// Drop all stored turns.
    async fn clear(&self) -> Result<(), HistoryError>;

    /// Current store metadata for the given budget.
    async fn meta(&self, budget: TokenBudget) -> Result<HistoryMeta, HistoryError>;
}

/// Select newest-first turns from a chronological slice such that the cumulative estimate fits
/// `budget`. Shared by both store implementations so the bound is defined in exactly one place.
fn newest_first_within(turns: &[HistoryTurn], budget: TokenBudget) -> Vec<HistoryTurn> {
    let mut out = Vec::new();
    let mut used = 0u32;
    for turn in turns.iter().rev() {
        let cost = turn.token_estimate();
        if used.saturating_add(cost) > budget.0 {
            break;
        }
        used += cost;
        out.push(turn.clone());
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn estimate_is_at_least_one() {
        // `estimate_tokens` is the bare-text tokenizer (no per-turn framing).
        assert_eq!(estimate_tokens(""), 1);
        assert_eq!(estimate_tokens("12345678"), 2);
    }

    #[test]
    fn token_estimate_adds_turn_framing() {
        // A turn costs its text estimate PLUS the per-turn framing it's seeded with.
        let turn = HistoryTurn::new(Role::User, "12345678", 0); // 8 chars -> 2 text tokens
        assert_eq!(estimate_tokens(&turn.text), 2);
        assert_eq!(turn.token_estimate(), 2 + TURN_FRAMING_TOKENS);
    }

    #[test]
    fn newest_first_respects_budget_and_order() {
        // Each turn costs estimate_tokens("abcd")=1 + TURN_FRAMING_TOKENS, so budget for exactly 3.
        let per_turn = 1 + TURN_FRAMING_TOKENS;
        let turns: Vec<_> = (0..10)
            .map(|i| HistoryTurn::new(Role::User, "abcd", i))
            .collect();
        let got = newest_first_within(&turns, TokenBudget(per_turn * 3));
        assert_eq!(got.len(), 3);
        // newest-first: ts 9, 8, 7
        assert_eq!(got[0].ts_ms, 9);
        assert_eq!(got[2].ts_ms, 7);
        let total: u32 = got.iter().map(HistoryTurn::token_estimate).sum();
        assert!(total <= per_turn * 3);
    }
}
