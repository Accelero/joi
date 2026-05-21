//! On-disk [`HistoryStore`] backed by a JSONL file with append + prune (SPEC §6.4).
//!
//! Each line is one JSON [`HistoryTurn`]. The store is constructed with the budget it enforces, so
//! [`HistoryStore::append`] prunes oldest turns whenever the total estimate would exceed it. Loads
//! are tolerant of an empty or partially corrupt file (skip unparseable lines) — a corrupt history
//! starts fresh rather than panicking (SPEC §14).

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{newest_first_within, HistoryMeta, HistoryStore, HistoryTurn, TokenBudget};
use crate::error::HistoryError;

/// A [`HistoryStore`] persisted as JSONL at `path`, bounded by `budget`.
#[derive(Debug, Clone)]
pub struct JsonlHistory {
    path: PathBuf,
    budget: TokenBudget,
}

impl JsonlHistory {
    /// Create a store writing to `path` and pruning to `budget` on append.
    #[must_use]
    pub fn new(path: PathBuf, budget: TokenBudget) -> Self {
        Self { path, budget }
    }

    /// Read all turns in chronological order, skipping any unparseable lines.
    async fn read_all(&self) -> Result<Vec<HistoryTurn>, HistoryError> {
        let contents = match tokio::fs::read_to_string(&self.path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(HistoryError::Io(e.to_string())),
        };
        Ok(contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<HistoryTurn>(l).ok())
            .collect())
    }

    /// Atomically rewrite the file with `turns` (write to a temp sibling, then rename).
    async fn write_all(&self, turns: &[HistoryTurn]) -> Result<(), HistoryError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| HistoryError::Io(e.to_string()))?;
        }
        let mut buf = String::new();
        for turn in turns {
            let line =
                serde_json::to_string(turn).map_err(|e| HistoryError::Serde(e.to_string()))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        tokio::fs::write(&tmp, buf.as_bytes())
            .await
            .map_err(|e| HistoryError::Io(e.to_string()))?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| HistoryError::Io(e.to_string()))
    }

    /// Prune oldest turns until the cumulative estimate is within `budget`.
    fn prune_to_budget(turns: &mut Vec<HistoryTurn>, budget: TokenBudget) {
        let mut total: u32 = turns.iter().map(HistoryTurn::token_estimate).sum();
        let mut drop_count = 0;
        for turn in turns.iter() {
            if total <= budget.0 {
                break;
            }
            total = total.saturating_sub(turn.token_estimate());
            drop_count += 1;
        }
        turns.drain(0..drop_count);
    }
}

#[async_trait]
impl HistoryStore for JsonlHistory {
    async fn append(&self, turn: HistoryTurn) -> Result<(), HistoryError> {
        let mut turns = self.read_all().await?;
        turns.push(turn);
        Self::prune_to_budget(&mut turns, self.budget);
        self.write_all(&turns).await
    }

    async fn load_within_budget(
        &self,
        budget: TokenBudget,
    ) -> Result<Vec<HistoryTurn>, HistoryError> {
        Ok(newest_first_within(&self.read_all().await?, budget))
    }

    async fn clear(&self) -> Result<(), HistoryError> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HistoryError::Io(e.to_string())),
        }
    }

    async fn meta(&self, budget: TokenBudget) -> Result<HistoryMeta, HistoryError> {
        let turns = self.read_all().await?;
        let token_estimate = turns.iter().map(HistoryTurn::token_estimate).sum();
        Ok(HistoryMeta {
            turns: turns.len(),
            token_estimate,
            budget: budget.0,
        })
    }
}

/// Path helper: the canonical history file inside a history directory (SPEC §6.4 — one
/// conversation in the MVP, fixed id `current`).
#[must_use]
pub fn current_conversation_path(history_dir: &Path) -> PathBuf {
    history_dir.join("current.jsonl")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::history::Role;
    use proptest::prelude::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn append_load_clear_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlHistory::new(current_conversation_path(dir.path()), TokenBudget(1_000));
        store
            .append(HistoryTurn::new(Role::User, "hello there", 1))
            .await
            .unwrap();
        store
            .append(HistoryTurn::new(Role::Assistant, "general kenobi", 2))
            .await
            .unwrap();

        let loaded = store.load_within_budget(TokenBudget(1_000)).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].ts_ms, 2); // newest-first

        store.clear().await.unwrap();
        assert!(store
            .load_within_budget(TokenBudget(1_000))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlHistory::new(dir.path().join("nope.jsonl"), TokenBudget(1_000));
        assert!(store
            .load_within_budget(TokenBudget(1_000))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn corrupt_lines_are_skipped_not_panicked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current.jsonl");
        let good = serde_json::to_string(&HistoryTurn::new(Role::User, "ok", 1)).unwrap();
        tokio::fs::write(&path, format!("{good}\nnot json at all\n{{partial\n"))
            .await
            .unwrap();
        let store = JsonlHistory::new(path, TokenBudget(1_000));
        let loaded = store.load_within_budget(TokenBudget(1_000)).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text, "ok");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn append_never_exceeds_budget(
            texts in proptest::collection::vec("[a-z ]{0,40}", 0..40),
            budget in 1u32..50,
        ) {
            let rt = rt();
            rt.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let store = JsonlHistory::new(
                    current_conversation_path(dir.path()),
                    TokenBudget(budget),
                );
                for (i, t) in texts.iter().enumerate() {
                    store.append(HistoryTurn::new(Role::User, t.clone(), i as u64)).await.unwrap();
                    let meta = store.meta(TokenBudget(budget)).await.unwrap();
                    prop_assert!(
                        meta.token_estimate <= budget,
                        "stored estimate {} exceeded budget {}", meta.token_estimate, budget,
                    );
                }
                // The re-seedable window also fits the budget.
                let loaded = store.load_within_budget(TokenBudget(budget)).await.unwrap();
                let total: u32 = loaded.iter().map(HistoryTurn::token_estimate).sum();
                prop_assert!(total <= budget);
                Ok(())
            })?;
        }
    }
}
