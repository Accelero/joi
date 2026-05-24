//! Session-scoped, persisted conversation history — the unit a user resumes (Claude-Code style).
//!
//! Each session is stored under the sessions directory (`~/.joi/sessions`) as **two** things:
//!   * `<uuid>.jsonl` — the turns, one JSON [`HistoryTurn`] per line, **append-only**; the canonical
//!     conversation log.
//!   * `index.json` — a single `uuid → `[`SessionMeta`] map for the whole directory (name +
//!     timestamps). Metadata lives here, **not** in the JSONL, so refreshing `last_updated` /
//!     `last_opened` never rewrites the (potentially large) turn log. The index is rebuildable from
//!     the logs if it's ever lost ([`SessionStore::rebuild_index`] — FR-22).
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

use super::{newest_first_within, HistoryMeta, HistoryStore, HistoryTurn, Role, TokenBudget};
use crate::clock::{Clock, UnixMillis};
use crate::error::HistoryError;

/// Name of the shared session index inside the sessions directory.
const INDEX_FILE: &str = "index.json";

/// Metadata for one session — the value side of the `uuid → metadata` index. The id is the map key
/// (and the JSONL filename stem), so it isn't duplicated in here. Carried inside [`SessionSummary`],
/// it is the value the picker renders (and a future out-of-process frontend would receive as JSON).
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

/// A row for listing sessions in a resume picker — an index entry paired with its id. `list`
/// returns it newest-activity-first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session id (the `<uuid>.jsonl` stem; pass it to `resume_session`).
    pub id: String,
    /// Name + timestamps.
    pub meta: SessionMeta,
}

/// Persisted-session repository bound to the **current** session, usable as a [`HistoryStore`].
///
/// Turns are appended to `<dir>/<id>.jsonl`; the `<dir>/index.json` metadata is bumped alongside.
/// The current session's turns live on disk, so this holds only its id + metadata head behind a
/// mutex (interior mutability for the `&self` store API and the "switch session" commands).
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
        let mut meta = index
            .get(id)
            .cloned()
            .unwrap_or_else(|| SessionMeta::fresh(now));
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

    /// Rebuild the on-disk index by scanning every `<uuid>.jsonl` log in `dir` and reconstructing
    /// each session's [`SessionMeta`] from its turns (FR-22 — a lost index is recoverable). An entry
    /// already present in the index is **preserved** (so explicit renames / `last_opened` survive);
    /// only sessions whose logs exist but are missing from the index are reconstructed. Returns the
    /// rebuilt session list, newest-activity first.
    pub async fn rebuild_index(dir: &Path) -> Result<Vec<SessionSummary>, HistoryError> {
        let mut index = read_index(dir).await;
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(HistoryError::Io(e.to_string())),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| HistoryError::Io(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if index.contains_key(&id) {
                continue; // keep the authoritative entry (names, last_opened)
            }
            let turns = read_turns(&path).await?;
            index.insert(id, meta_from_turns(&turns));
        }
        write_index(dir, &index).await?;
        Ok(Self::list(dir).await)
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

    /// Read the full, chronological turn log of **any** session by id, without changing the current
    /// binding. This is the read a host uses to **repopulate a transcript view** when it loads or
    /// resumes a session — the whole conversation as persisted. It is distinct from
    /// [`load_within_budget`](HistoryStore::load_within_budget), which returns the newest-first,
    /// budget-bounded slice used to *re-seed the model*. A missing log reads as empty; corrupt lines
    /// are skipped (FR-22).
    pub async fn load_turns(&self, id: &str) -> Result<Vec<HistoryTurn>, HistoryError> {
        read_turns(&log_path(&self.dir, id)).await
    }

    /// Set (or clear) the current session's name, persisting it to the index.
    pub async fn rename(&self, name: Option<String>) -> Result<(), HistoryError> {
        let mut c = self.current.lock().await;
        c.meta.name = name;
        upsert_meta(&self.dir, &c.id, &c.meta).await
    }

    /// The sessions directory this store manages (for listing).
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Switch the store to a **brand-new** session (fresh `uuid`), at runtime, without restarting.
    /// The previous session's files are left intact on disk; only the *current* binding changes, so
    /// subsequent appends/seeds target the new session. Returns the new session's summary.
    pub async fn start_new(&self) -> Result<SessionSummary, HistoryError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| HistoryError::Io(e.to_string()))?;
        let id = uuid::Uuid::new_v4().to_string();
        let meta = SessionMeta::fresh(self.clock.now_ms());
        tokio::fs::write(log_path(&self.dir, &id), b"")
            .await
            .map_err(|e| HistoryError::Io(e.to_string()))?;
        upsert_meta(&self.dir, &id, &meta).await?;
        *self.current.lock().await = Current {
            id: id.clone(),
            meta: meta.clone(),
        };
        tracing::info!(session = %id, "switched to a new session");
        Ok(SessionSummary { id, meta })
    }

    /// Switch the store to an **existing** session by id, at runtime, refreshing its `last_opened`.
    /// The session being switched away from is untouched on disk. Tolerant of an unknown id (starts
    /// an empty log for it), so a stale picker entry can't fail the switch.
    pub async fn switch_to(&self, id: &str) -> Result<SessionSummary, HistoryError> {
        let now = self.clock.now_ms();
        let mut index = read_index(&self.dir).await;
        let mut meta = index
            .get(id)
            .cloned()
            .unwrap_or_else(|| SessionMeta::fresh(now));
        meta.last_opened = now;
        index.insert(id.to_string(), meta.clone());
        write_index(&self.dir, &index).await?;
        // Ensure a log exists so later reads/appends succeed (an unknown id resumes as empty).
        let log = log_path(&self.dir, id);
        if tokio::fs::metadata(&log).await.is_err() {
            tokio::fs::write(&log, b"")
                .await
                .map_err(|e| HistoryError::Io(e.to_string()))?;
        }
        *self.current.lock().await = Current {
            id: id.to_string(),
            meta: meta.clone(),
        };
        tracing::info!(session = %id, "resumed session");
        Ok(SessionSummary {
            id: id.to_string(),
            meta,
        })
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
        // Auto-label an unnamed session from the first user turn (Claude-Code-style); an explicit
        // `rename` is preserved because we only fill a `None` name. Rides this same index write.
        if c.meta.name.is_none() && turn.role == Role::User {
            c.meta.name = derive_name(&turn.text);
        }
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

/// Derive a short display label from a user turn: the first few words, capped in length. `None`
/// for blank/whitespace-only text (leaves the session unnamed until a real first message).
fn derive_name(text: &str) -> Option<String> {
    /// Word and character caps for an auto-derived session label.
    const MAX_WORDS: usize = 6;
    const MAX_CHARS: usize = 50;
    let mut name = text
        .split_whitespace()
        .take(MAX_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        return None;
    }
    if name.chars().count() > MAX_CHARS {
        name = name.chars().take(MAX_CHARS).collect::<String>();
        name.push('…');
    }
    Some(name)
}

/// Reconstruct a session's metadata from its turns alone (used by [`SessionStore::rebuild_index`]).
/// Timestamps come from the first/last turn; the name from the first user turn. An empty log yields
/// an all-zero meta so the session still lists.
fn meta_from_turns(turns: &[HistoryTurn]) -> SessionMeta {
    let created_at = turns.first().map_or(0, |t| t.ts_ms);
    let last_updated = turns.last().map_or(0, |t| t.ts_ms);
    let name = turns
        .iter()
        .find(|t| t.role == Role::User)
        .and_then(|t| derive_name(&t.text));
    SessionMeta {
        name,
        created_at,
        last_opened: last_updated,
        last_updated,
    }
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join(INDEX_FILE)
}

fn log_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

/// Read every turn of a session log in chronological order, skipping unparseable lines (a corrupt
/// log degrades to what it can parse rather than failing — FR-22).
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
async fn write_index(
    dir: &Path,
    index: &BTreeMap<String, SessionMeta>,
) -> Result<(), HistoryError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| HistoryError::Io(e.to_string()))?;
    let buf =
        serde_json::to_string_pretty(index).map_err(|e| HistoryError::Serde(e.to_string()))?;
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
    let buf =
        serde_json::to_string_pretty(index).map_err(|e| HistoryError::Serde(e.to_string()))?;
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
    async fn load_turns_reads_any_session_without_switching_current() {
        // A host repopulating a transcript view loads the full chronological log for a given id,
        // and doing so must not retarget the store's current session.
        let dir = tempfile::tempdir().unwrap();
        let (c, _tc) = clock(1_000);

        // Session A gets two turns; remember its id, then switch away to a fresh session B.
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        let id_a = store.current_summary().await.id;
        store
            .append(HistoryTurn::new(Role::User, "hello", 1))
            .await
            .unwrap();
        store
            .append(HistoryTurn::new(Role::Assistant, "hi there", 2))
            .await
            .unwrap();
        let id_b = store.start_new().await.unwrap().id;

        // Reading A's turns returns the whole conversation, oldest-first…
        let turns = store.load_turns(&id_a).await.unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "hello");
        assert_eq!(turns[1].text, "hi there");
        // …and the current binding is untouched (still B).
        assert_eq!(store.current_summary().await.id, id_b);

        // An unknown / empty session reads as no turns rather than erroring.
        assert!(store.load_turns("does-not-exist").await.unwrap().is_empty());
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
        assert!(
            after > created,
            "append must bump last_updated: {after} > {created}"
        );
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
        assert_eq!(
            summary.meta.last_opened, 9_000,
            "load refreshes last_opened"
        );
        // Turns survived the reopen and seed the resumed session.
        let loaded = store.load_within_budget(TokenBudget(1_000)).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text, "remember me");
    }

    #[tokio::test]
    async fn first_user_turn_auto_names_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let (c, _tc) = clock(1_000);
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        assert_eq!(store.current_summary().await.meta.name, None);

        store
            .append(HistoryTurn::new(
                Role::User,
                "what's the weather like today",
                1,
            ))
            .await
            .unwrap();
        assert_eq!(
            store.current_summary().await.meta.name.as_deref(),
            Some("what's the weather like today")
        );

        // A later user turn must not overwrite the established name.
        store
            .append(HistoryTurn::new(Role::User, "and tomorrow", 2))
            .await
            .unwrap();
        assert_eq!(
            store.current_summary().await.meta.name.as_deref(),
            Some("what's the weather like today")
        );
        // Durable in the index.
        assert_eq!(
            SessionStore::list(dir.path()).await[0].meta.name.as_deref(),
            Some("what's the weather like today")
        );
    }

    #[tokio::test]
    async fn agent_first_turn_does_not_name_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let (c, _tc) = clock(1_000);
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        // Only a *user* turn names the session.
        store
            .append(HistoryTurn::new(
                Role::Assistant,
                "Hello! How can I help?",
                1,
            ))
            .await
            .unwrap();
        assert_eq!(store.current_summary().await.meta.name, None);
    }

    #[test]
    fn derive_name_caps_words_and_length() {
        assert_eq!(derive_name("   "), None);
        assert_eq!(
            derive_name("one two three four five six seven eight").as_deref(),
            Some("one two three four five six")
        );
        let long = "x".repeat(80);
        let got = derive_name(&long).unwrap();
        assert!(got.chars().count() <= 51, "capped + ellipsis: {got}");
        assert!(got.ends_with('…'));
    }

    #[tokio::test]
    async fn rename_persists_to_index() {
        let dir = tempfile::tempdir().unwrap();
        let (c, _tc) = clock(1_000);
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        store
            .rename(Some("Morning chat".to_string()))
            .await
            .unwrap();
        let listed = SessionStore::list(dir.path()).await;
        assert_eq!(listed[0].meta.name.as_deref(), Some("Morning chat"));
    }

    #[tokio::test]
    async fn corrupt_log_line_is_skipped_not_fatal() {
        // A damaged JSONL line must be skipped, not fail the whole load (FR-22).
        let dir = tempfile::tempdir().unwrap();
        let (c, _tc) = clock(1_000);
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        let id = store.current_summary().await.id;
        store
            .append(HistoryTurn::new(Role::User, "good line", 1))
            .await
            .unwrap();
        // Manually corrupt the log: a garbage line between two valid ones.
        let path = dir.path().join(format!("{id}.jsonl"));
        let mut body = tokio::fs::read_to_string(&path).await.unwrap();
        body.push_str("{ this is not valid json\n");
        body.push_str(
            &serde_json::to_string(&HistoryTurn::new(Role::Assistant, "after corruption", 2))
                .unwrap(),
        );
        body.push('\n');
        tokio::fs::write(&path, body).await.unwrap();

        let loaded = store.load_within_budget(TokenBudget(1_000)).await.unwrap();
        assert_eq!(loaded.len(), 2, "two valid turns survive the corrupt line");
    }

    #[tokio::test]
    async fn lost_index_is_rebuilt_from_logs() {
        // FR-22: deleting index.json must not lose sessions — they rebuild from the logs.
        let dir = tempfile::tempdir().unwrap();
        let (c, _tc) = clock(1_000);
        let store = SessionStore::create_new(dir.path(), c).unwrap();
        let id = store.current_summary().await.id;
        store
            .append(HistoryTurn::new(Role::User, "rebuild me please", 5))
            .await
            .unwrap();
        store
            .append(HistoryTurn::new(Role::Assistant, "ok", 6))
            .await
            .unwrap();

        // Lose the index entirely.
        tokio::fs::remove_file(dir.path().join(INDEX_FILE))
            .await
            .unwrap();
        assert!(SessionStore::list(dir.path()).await.is_empty());

        // Rebuild reconstructs the session from its log, name derived from the first user turn.
        let rebuilt = SessionStore::rebuild_index(dir.path()).await.unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].id, id);
        assert_eq!(rebuilt[0].meta.name.as_deref(), Some("rebuild me please"));
        assert_eq!(rebuilt[0].meta.created_at, 5);
        assert_eq!(rebuilt[0].meta.last_updated, 6);
    }
}
