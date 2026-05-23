//! Bounded, restorable conversation history (SPEC §6).
//!
//! History persists **text** turns (transcripts), not audio — enough to re-seed a fresh session as
//! `initial_context` on resume (SPEC §6.1, §6.3). It is bounded by a [`TokenBudget`] sized to the
//! **Live session's input limit**, *not* the underlying text model's 1M-class window (SPEC §6.2).
//!
//! Two implementations: [`memory::InMemoryHistory`] (tests) and [`file::JsonlHistory`] (prod).

pub mod file;
pub mod memory;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::clock::UnixMillis;
use crate::error::HistoryError;

pub use file::JsonlHistory;
pub use memory::InMemoryHistory;

/// Who produced a turn (SPEC §6.1).
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

    /// Approximate token cost of this turn.
    #[must_use]
    pub fn token_estimate(&self) -> u32 {
        estimate_tokens(&self.text)
    }
}

/// An approximate token budget bounding stored history (SPEC §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct TokenBudget(pub u32);

/// Summary of the current history, surfaced to the UI (SPEC §11, `history` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ts_rs::TS)]
#[ts(export)]
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
/// This is the single swap point for a smarter tokenizer later (PLAN §7 M3); the MVP uses the
/// chars/4 heuristic. Always returns at least 1 so an empty turn still counts.
#[must_use]
pub fn estimate_tokens(text: &str) -> u32 {
    ((text.chars().count() / 4) as u32).max(1)
}

/// Persisted conversation store. Append-mostly with pruning to a token budget (SPEC §6.4).
#[async_trait]
pub trait HistoryStore: Send + Sync {
    /// Append a finalized turn, pruning oldest turns if the budget is exceeded.
    async fn append(&self, turn: HistoryTurn) -> Result<(), HistoryError>;

    /// Newest-first turns whose cumulative token estimate fits `budget`.
    ///
    /// The returned set is **guaranteed re-seedable**: its total estimate is ≤ `budget`, so it can
    /// be passed straight back as `SessionConfig.initial_context` (SPEC §6.3).
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
        assert_eq!(estimate_tokens(""), 1);
        assert_eq!(estimate_tokens("12345678"), 2);
    }

    #[test]
    fn newest_first_respects_budget_and_order() {
        // each turn is ~1 token ("abcd" -> 1)
        let turns: Vec<_> = (0..10)
            .map(|i| HistoryTurn::new(Role::User, "abcd", i))
            .collect();
        let got = newest_first_within(&turns, TokenBudget(3));
        assert_eq!(got.len(), 3);
        // newest-first: ts 9, 8, 7
        assert_eq!(got[0].ts_ms, 9);
        assert_eq!(got[2].ts_ms, 7);
        let total: u32 = got.iter().map(HistoryTurn::token_estimate).sum();
        assert!(total <= 3);
    }
}
