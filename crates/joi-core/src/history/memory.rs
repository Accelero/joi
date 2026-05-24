//! In-memory [`HistoryStore`] — the no-key/no-disk fallback and the test double.

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{newest_first_within, HistoryMeta, HistoryStore, HistoryTurn, TokenBudget};
use crate::error::HistoryError;

/// A [`HistoryStore`] backed by an in-memory `Vec`. Not persistent.
#[derive(Debug, Default)]
pub struct InMemoryHistory {
    turns: Mutex<Vec<HistoryTurn>>,
}

impl InMemoryHistory {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl HistoryStore for InMemoryHistory {
    async fn append(&self, turn: HistoryTurn) -> Result<(), HistoryError> {
        self.turns.lock().await.push(turn);
        Ok(())
    }

    async fn load_within_budget(
        &self,
        budget: TokenBudget,
    ) -> Result<Vec<HistoryTurn>, HistoryError> {
        Ok(newest_first_within(&self.turns.lock().await, budget))
    }

    async fn clear(&self) -> Result<(), HistoryError> {
        self.turns.lock().await.clear();
        Ok(())
    }

    async fn meta(&self, budget: TokenBudget) -> Result<HistoryMeta, HistoryError> {
        let turns = self.turns.lock().await;
        let token_estimate = turns.iter().map(HistoryTurn::token_estimate).sum();
        Ok(HistoryMeta {
            turns: turns.len(),
            token_estimate,
            budget: budget.0,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::history::Role;

    #[tokio::test]
    async fn append_and_load_roundtrip() {
        let store = InMemoryHistory::new();
        store
            .append(HistoryTurn::new(Role::User, "hello", 1))
            .await
            .unwrap();
        store
            .append(HistoryTurn::new(Role::Assistant, "hi there", 2))
            .await
            .unwrap();
        let loaded = store.load_within_budget(TokenBudget(1_000)).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].ts_ms, 2); // newest first
    }

    #[tokio::test]
    async fn meta_counts_turns() {
        let store = InMemoryHistory::new();
        store
            .append(HistoryTurn::new(Role::User, "abcd", 1))
            .await
            .unwrap();
        let meta = store.meta(TokenBudget(100)).await.unwrap();
        assert_eq!(meta.turns, 1);
        assert_eq!(meta.budget, 100);
    }
}
