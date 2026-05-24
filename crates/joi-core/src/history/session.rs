//! Session-scoped, persisted conversation history — the unit a user resumes (Claude-Code style).
//!
//! Each session is stored under the sessions directory (`~/.joi/sessions`) as **two** things:
//!   * `<uuid>.jsonl` — the turns, one JSON [`HistoryTurn`] per line, **append-only**; the canonical
//!     conversation log.
//!   * `index.json` — a single `uuid → `[`SessionMeta`] map for the whole directory (name +
//!     timestamps). Metadata lives here, **not** in the JSONL, so refreshing `last_updated` /
//!     `last_opened` never rewrites the (potentially large) turn log. The index is rebuildable from
//!     the logs if it's ever lost.
//!
//! A [`SessionStore`] owns the *current* [`Session`] and implements [`HistoryStore`], so the
//! [`crate::manager::SessionManager`] appends turns to it and seeds from it with no changes: a fresh
//! session seeds nothing; a resumed/continued one seeds its turns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use super::{newest_first_within, HistoryMeta, HistoryStore, HistoryTurn, TokenBudget};
use crate::clock::{Clock, UnixMillis};
use crate::error::HistoryError;

/// Name of the shared session index inside the sessions directory.
const INDEX_FILE: &str = "index.json";

/// Metadata for one session — the value side of the `uuid → metadata` index. The id is the map key
/// (and the JSONL filename stem), so it isn't duplicated in here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// User-assigned label, or `None` for an unnamed session.
    pub name: Option<String>,
    /// When the session was first created (ms since the Unix epoch).
    pub created_at: UnixMillis,
    /// When the session was last loaded — refreshed on [`SessionStore::load`].
    pub last_opened: UnixMillis,
    /// When the session was last written to — refreshed on every appended turn.
    pub last_updated: UnixMillis,
}

impl SessionMeta {
    fn fresh(now: UnixMillis) -> Self {
        Self {
            name: None,
            created_at: now,
            last_opened: now,
            last_updated: now,
        }
    }
}

/// One session as the user thinks of it: identity + metadata + its turns. Built from the JSONL log
/// (turns) plus the index (metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Session id (v4 UUID); the JSONL filename stem and the index key.
    pub id: String,
    /// Name + timestamps.
    pub meta: SessionMeta,
    /// Conversation turns in chronological order.
    pub turns: Vec<HistoryTurn>,
}

/// A row for listing sessions in a resume picker — an index entry flattened with its id key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session id.
    pub id: String,
    /// Name + timestamps.
    #[serde(flatten)]
    pub meta: SessionMeta,
}

/// Persisted-session repository bound to the **current** session, usable as a [`HistoryStore`].
///
/// Turns are appended to `<dir>/<id>.jsonl`; the `<dir>/index.json` metadata is bumped alongside.
/// The current session's turns live on disk, so this holds only its id + metadata head behind a
/// mutex (interior mutability for the `&self` store API and a future "switch session" command).
pub struct SessionStore {
    dir: PathBuf,
    clock: Arc<dyn Clock>,
    current: Mutex<Current>,
}

struct Current {
    id: String,
    meta: SessionMeta,
}

impl SessionStore {
    /// Start a brand-new session (a fresh `uuid`), registering it in the index. Used by the app at
    /// startup by default. Synchronous (one-off startup I/O via `std::fs`) so it composes into the
    /// non-async `JoiApp::build`.
    pub fn create_new(
        dir: impl Into<PathBuf>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, HistoryError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| HistoryError::Io(e.to_string()))?;
        let id = uuid::Uuid::new_v4().to_string();
        let meta = SessionMeta::fresh(clock.now_ms());
        // Touch an empty log so the session exists on disk before its first turn.
        let log = log_path(&dir, &id);
        std::fs::write(&log, b"").map_err(|e| HistoryError::Io(e.to_string()))?;
        let mut index = read_index_blocking(&dir);
        index.insert(id.clone(), meta.clone());
        write_index_blocking(&dir, &index)?;
        tracing::info!(session = %id, "started new session");
        Ok(Self {
            dir,
            clock,
            current: Mutex::new(Current { id, meta }),
        })
    }

    /// Resume an existing session by id, refreshing its `last_opened`. Tolerant of an id missing
    /// from the index (treats it as a fresh entry) so a stray-but-present log can still be opened.
    pub fn load(
        dir: impl Into<PathBuf>,
        id: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, HistoryError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| HistoryError::Io(e.to_string()))?;
        let now = clock.now_ms();
        let mut index = read_index_blocking(&dir);
        let mut meta = index.get(id).cloned().unwrap_or_else(|| SessionMeta::fresh(now));
        meta.last_opened = now;
        index.insert(id.to_string(), meta.clone());
        write_index_blocking(&dir, &index)?;
        tracing::info!(session = %id, "resumed session");
        Ok(Self {
            dir,
            clock,
            current: Mutex::new(Current {
                id: id.to_string(),
                meta,
            }),
        })
    }

    /// List every session in `dir`, newest-activity first — the data a resume picker renders.
    pub async fn list(dir: &Path) -> Vec<SessionSummary> {
        let mut out: Vec<SessionSummary> = read_index(dir)
            .await
            .into_iter()
            .map(|(id, meta)| SessionSummary { id, meta })
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.meta.last_updated));
        out
    }

    /// Summary of the session currently bound to this store.
    pub async fn current_summary(&self) -> SessionSummary {
        let c = self.current.lock().await;
        SessionSummary {
            id: c.id.clone(),
            meta: c.meta.clone(),
        }
    }

    /// The current session as a full [`Session`] value — identity + metadata + the turns read back
    /// from its JSONL log. This is the object a host instantiates from a session file.
    pub async fn current(&self) -> Result<Session, HistoryError> {
        let (id, meta) = {
            let c = self.current.lock().await;
            (c.id.clone(), c.meta.clone())
        };
        let turns = read_turns(&log_path(&self.dir, &id)).await?;
        Ok(Session { id, meta, turns })
    }

    /// Set (or clear) the current session's name, persisting it to the index.
    pub async fn rename(&self, name: Option<String>) -> Result<(), HistoryError> {
        let mut c = self.current.lock().await;
        c.meta.name = name;
        upsert_meta(&self.dir, &c.id, &c.meta).await
    }
}

#[async_trait]
impl HistoryStore for SessionStore {
    async fn append(&self, turn: HistoryTurn) -> Result<(), HistoryError> {
        let id = self.current.lock().await.id.clone();
        append_turn_line(&log_path(&self.dir, &id), &turn).await?;
        // Bump last_updated on every write so the picker can sort by "last in use".
        let now = self.clock.now_ms();
        let mut c = self.current.lock().await;
        c.meta.last_updated = now;
        upsert_meta(&self.dir, &id, &c.meta).await
    }

    async fn load_within_budget(
        &self,
        budget: TokenBudget,
    ) -> Result<Vec<HistoryTurn>, HistoryError> {
        let id = self.current.lock().await.id.clone();
        Ok(newest_first_within(
            &read_turns(&log_path(&self.dir, &id)).await?,
            budget,
        ))
    }

    async fn clear(&self) -> Result<(), HistoryError> {
        let id = self.current.lock().await.id.clone();
        tokio::fs::write(log_path(&self.dir, &id), b"")
            .await
            .map_err(|e| HistoryError::Io(e.to_string()))
    }

    async fn meta(&self, budget: TokenBudget) -> Result<HistoryMeta, HistoryError> {
        let id = self.current.lock().await.id.clone();
        let turns = read_turns(&log_path(&self.dir, &id)).await?;
        Ok(HistoryMeta {
            turns: turns.len(),
            token_estimate: turns.iter().map(HistoryTurn::token_estimate).sum(),
            budget: budget.0,
        })
    }
}

// ── File helpers ────────────────────────────────────────────────────────────

fn index_path(dir: &Path) -> PathBuf {
    dir.join(INDEX_FILE)
}

fn log_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

/// Read every turn of a session log in chronological order, skipping unparseable lines (a corrupt
/// log degrades to what it can parse rather than failing — SPEC §14).
async fn read_turns(path: &Path) -> Result<Vec<HistoryTurn>, HistoryError> {
    let contents = match tokio::fs::read_to_string(path).await {
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

/// Append one turn as a JSON line (true append — never rewrites the existing log).
async fn append_turn_line(path: &Path, turn: &HistoryTurn) -> Result<(), HistoryError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| HistoryError::Io(e.to_string()))?;
    }
    let mut line = serde_json::to_string(turn).map_err(|e| HistoryError::Serde(e.to_string()))?;
    line.push('\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| HistoryError::Io(e.to_string()))?;
    file.write_all(line.as_bytes())
        .await
        .map_err(|e| HistoryError::Io(e.to_string()))
}

/// Parse the index, tolerating a missing/corrupt file by starting empty.
async fn read_index(dir: &Path) -> BTreeMap<String, SessionMeta> {
    match tokio::fs::read_to_string(index_path(dir)).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    }
}

/// Atomically rewrite the index (temp sibling + rename).
async fn write_index(dir: &Path, index: &BTreeMap<String, SessionMeta>) -> Result<(), HistoryError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| HistoryError::Io(e.to_string()))?;
    let buf = serde_json::to_string_pretty(index).map_err(|e| HistoryError::Serde(e.to_string()))?;
    let tmp = index_path(dir).with_extension("json.tmp");
    tokio::fs::write(&tmp, buf.as_bytes())
        .await
        .map_err(|e| HistoryError::Io(e.to_string()))?;
    tokio::fs::rename(&tmp, index_path(dir))
        .await
        .map_err(|e| HistoryError::Io(e.to_string()))
}

/// Insert/replace one entry, leaving the rest of the index intact.
async fn upsert_meta(dir: &Path, id: &str, meta: &SessionMeta) -> Result<(), HistoryError> {
    let mut index = read_index(dir).await;
    index.insert(id.to_string(), meta.clone());
    write_index(dir, &index).await
}

/// Blocking index read for the synchronous constructors (one-off startup I/O).
fn read_index_blocking(dir: &Path) -> BTreeMap<String, SessionMeta> {
    match std::fs::read_to_string(index_path(dir)) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    }
}

/// Blocking, atomic index write for the synchronous constructors.
fn write_index_blocking(
    dir: &Path,
    index: &BTreeMap<String, SessionMeta>,
) -> Result<(), HistoryError> {
    let buf = serde_json::to_string_pretty(index).map_err(|e| HistoryError::Serde(e.to_string()))?;
    let tmp = index_path(dir).with_extension("json.tmp");
    std::fs::write(&tmp, buf.as_bytes()).map_err(|e| HistoryError::Io(e.to_string()))?;
    std::fs::rename(&tmp, index_path(dir)).map_err(|e| HistoryError::Io(e.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::history::Role;

    fn clock(ms: UnixMillis) -> (Arc<dyn Clock>, TestClock) {
        let tc = TestClock::new(ms);
        (Arc::new(tc.clone()), tc)
    }

    #[tokio::test]
    async fn new_session_persists_turns_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let (c, _tc) = clock(1_000);
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        store
            .append(HistoryTurn::new(Role::User, "hello", 1))
            .await
            .unwrap();
        store
            .append(HistoryTurn::new(Role::Assistant, "hi there", 2))
            .await
            .unwrap();

        // Turns round-trip, newest-first within budget.
        let loaded = store.load_within_budget(TokenBudget(1_000)).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text, "hi there");

        // The index records exactly this session.
        let summaries = SessionStore::list(dir.path()).await;
        assert_eq!(summaries.len(), 1);
        let cur = store.current_summary().await;
        assert_eq!(summaries[0].id, cur.id);
        // The per-session file is named <uuid>.jsonl.
        assert!(dir.path().join(format!("{}.jsonl", cur.id)).exists());

        // The full Session object reads identity + metadata + turns from the JSONL.
        let session = store.current().await.unwrap();
        assert_eq!(session.id, cur.id);
        assert_eq!(session.turns.len(), 2);
        assert_eq!(session.turns[0].text, "hello"); // chronological order
    }

    #[tokio::test]
    async fn append_refreshes_last_updated() {
        let dir = tempfile::tempdir().unwrap();
        let (c, tc) = clock(1_000);
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        let created = store.current_summary().await.meta.last_updated;

        tc.advance(5_000);
        store
            .append(HistoryTurn::new(Role::User, "later", 9))
            .await
            .unwrap();
        let after = store.current_summary().await.meta.last_updated;
        assert!(after > created, "append must bump last_updated: {after} > {created}");
        // And the bump is durable in the index.
        let listed = SessionStore::list(dir.path()).await;
        assert_eq!(listed[0].meta.last_updated, after);
    }

    #[tokio::test]
    async fn load_resumes_turns_and_refreshes_last_opened() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let (c, _tc) = clock(1_000);
            let store = SessionStore::create_new(dir.path(), c).unwrap();
            store
                .append(HistoryTurn::new(Role::User, "remember me", 1))
                .await
                .unwrap();
            store.current_summary().await.id
        };

        // Reopen the same session later.
        let (c2, _tc2) = clock(9_000);
        let store = SessionStore::load(dir.path(), &id, c2).unwrap();
        let summary = store.current_summary().await;
        assert_eq!(summary.id, id);
        assert_eq!(summary.meta.last_opened, 9_000, "load refreshes last_opened");
        // Turns survived the reopen and seed the resumed session.
        let loaded = store.load_within_budget(TokenBudget(1_000)).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text, "remember me");
    }

    #[tokio::test]
    async fn rename_persists_to_index() {
        let dir = tempfile::tempdir().unwrap();
        let (c, _tc) = clock(1_000);
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        store.rename(Some("Morning chat".to_string())).await.unwrap();
        let listed = SessionStore::list(dir.path()).await;
        assert_eq!(listed[0].meta.name.as_deref(), Some("Morning chat"));
    }
}
