# Joi — Settings Interface Implementation Plan

> **Status:** engine implemented; frontend pending. The unified runtime-settings system: one
> `Config`, a curated editable surface with per-field apply-timing, edited **only** through the
> engine (Seam A), persisted **atomically** to a JSON config file. Read `doc/ARCH.md` (layering)
> first.
>
> **Done (engine, Phases 1–5 + 7 migration):** config is JSON (`~/.joi/config.json`) with one-shot
> YAML migration + atomic writes (`joi-core::util::atomic_write`); `joi-core::settings`
> (`SettingId`/`SettingValue`/`SettingKind`/`SettingDescriptor`/`ApplyTiming`, `settings_schema`,
> `apply_setting`); `UiEvent::Settings { settings }`; `Command::UpdateConfig`; Seam A
> `JoiApp::settings_schema()` + `JoiApp::update_setting(id, value)`. Backend crates build green with
> tests + `clippy -D warnings`.
>
> **Pending (frontend, Phase 6):** `joi-tui` must fold `UiEvent::Settings` and offer a settings
> panel — see §4 Phase 6. Until it handles the new variant, the `joi-tui` build (and thus the full
> `scripts/check.sh`) does not compile.

## 1. Principles (why it's built this way)

- **There is no separate "settings system."** Settings *are* config. One `Config` type, one set of
  code defaults, one `validate()`, one file. "Settings" adds only (a) a write path the UI can call
  and (b) per-field metadata (editable? apply-timing?).
- **Edited through the backend, never by the UI directly.** The engine holds the authoritative
  runtime `Config` (the `SessionManager` reads it at connect). A UI file-write would desync the
  running engine. The frontend dispatches a command; the engine validates, persists, propagates, and
  emits an event; the UI re-renders from that event. Litmus test: a headless process could do it →
  engine logic.
- **Curated editable surface, not the raw config.** Most config is consumed once at build/connect, so
  it can't apply live. The engine exposes a typed descriptor of the fields that *are* editable, each
  tagged with when a change takes effect.
- **Atomic persistence.** Every write (bootstrap and every change) goes through one `atomic_write`
  (temp in same dir → `sync_all` → `rename`). A crash or concurrent read can never see a
  half-written `config.json`. Secrets are blanked before serialization on every save.

## 2. Apply-timing taxonomy

| Timing | Meaning | Joi fields (today) |
|---|---|---|
| `Immediate` | applies live; engine persists + emits | `ui.terminal.accent`, `ui.terminal.background` |
| `NextSession` | applies on next connect (no app restart) | `voice`, `model`, `input/output_transcription`, `token_budget`, `context_window_compression` |
| `RestartRequired` | wired at `JoiApp::build`/startup | `media.*`, `history.dir`, `logging.*`, `reachability_probe_secs` |
| *(not exposed)* | file+env only, never in the UI | `live_api.provider`, `api_key` (secret) |

Initial editable set: `Voice` (NextSession), `Accent` + `Background` (Immediate). `RestartRequired`
fields are **not** exposed yet (add later as subsystems gain hot-reconfigure paths).

## 3. Decisions

- **File:** `~/.joi/config` → **`~/.joi/config.json`**. One-shot migration reads a legacy `~/.joi/config`
  (YAML) if `config.json` is absent.
- **Format:** JSON — round-trips cleanly for machine writing, no comment expectation, deterministic.
- **Docs:** `config/joi.example.yaml` → `config/joi.example.json` (plain); annotated field reference
  moves to `doc/CONFIG.md` (rustdoc on `Config` already documents each field).
- **Authoritative config:** `JoiApp` holds it behind a `RwLock<Config>`; the `SessionManager` keeps
  its own copy, resynced via a new `Command::UpdateConfig`. No shared lock into the actor.

## 4. Phases (each ends green: build + tests + `clippy -D warnings`)

### Phase 1 — Config format → JSON + atomic write
*`joi-core/src/config.rs`, new `joi-core/src/util.rs`, `config/joi.example.json`, `doc/CONFIG.md`, config tests.*
1. `atomic_write(path, bytes)` in core: temp in same dir → `flush` + `sync_all` → `rename`; create
   parent dirs. (Reuses the temp+rename idiom in `history/session.rs::write_index`.)
2. Loader → JSON: parse with `serde_json::from_str::<serde_json::Value>` → `Serialized::defaults(value)`
   (no figment feature change; `serde_json` already a workspace dep). Precedence + env layer unchanged.
3. Bootstrap → JSON: `write_default_if_missing` writes `serde_json::to_string_pretty(Config::default())`
   via `atomic_write`.
4. `ProjectPaths.config_file` → `config.json`; add `legacy_config_file` (`~/.joi/config`). In `load`:
   if `config.json` absent but legacy exists, parse legacy YAML → write `config.json` → log migration.
5. Example + docs: ship `config/joi.example.json`; move annotated reference to `doc/CONFIG.md`.
6. Tests: convert the jail-based config tests (YAML → JSON); add a round-trip test + a legacy-migration test.
- **Done:** `cargo test -p joi-core` green; bootstrap writes valid JSON atomically.

### Phase 2 — Settings core types (mechanism)
*new `joi-core/src/settings.rs`, `error.rs`.*
1. `enum ApplyTiming { Immediate, NextSession, RestartRequired }`.
2. `enum SettingId { Voice, Accent, Background, … }` (curated, extensible).
3. `enum SettingValue { Bool(bool), U32(u32), Text(String) }` + `enum SettingKind { Toggle,
   Choice{options}, Number{min,max}, Color, Text }` (drives UI control + constraints).
4. `struct SettingDescriptor { id, label, value, kind, apply }`.
5. `fn settings_schema(&Config) -> Vec<SettingDescriptor>` (current values from `Config`).
6. `fn apply_setting(&mut Config, id, value) -> Result<(), SettingsError>` — id→field map, per-field
   validation, then `Config::validate()` on the result.
7. `enum SettingsError { NotEditable, InvalidValue { reason }, Io }`.
8. Drift-coverage test: every `SettingId` round-trips through schema/apply and its `ApplyTiming` is
   asserted — guards the curated surface against diverging from `Config`.
- **Done:** `cargo test -p joi-core` green.

### Phase 3 — Event surface
- `UiEvent::Settings(SettingsSnapshot)` where `SettingsSnapshot = Vec<SettingDescriptor>`. Per-field
  `ApplyTiming` lets the frontend render "applies on reconnect" / "restart to apply" generically.

### Phase 4 — Manager resync (`joi-core/src/manager.rs`)
- `Command::UpdateConfig(Box<Config>)` replaces `self.config`. NextSession fields take effect on the
  next `do_start`; Immediate UI fields aren't read by the manager (handed to the frontend).

### Phase 5 — Seam A (`joi-app/src/lib.rs`)
1. `JoiApp` gains `config: RwLock<Config>` (authoritative copy).
2. `settings_schema(&self) -> Vec<SettingDescriptor>` (query for the panel).
3. `update_setting(&self, id, value) -> Result<(), SettingsError>`:
   - lock → clone → `apply_setting` (validate); on error return it (UI shows it), state unchanged;
   - persist: serialize new config with **`api_key` blanked** → `atomic_write(config.json)`;
   - store back into the `RwLock`; send `Command::UpdateConfig` to the manager;
   - emit `UiEvent::Settings(schema)`.
- **Done:** `cargo test -p joi-app` green incl. a headless test: set `Voice` → persists to
  `config.json`, manager picks it up on next start, schema reflects it; invalid value rejected;
  non-editable id rejected.

### Phase 6 — Frontend contract (hand to the frontend dev)
- Read `settings_schema()` at open; render controls from `SettingKind`.
- Write `update_setting(id, value)`; on `SettingsError::InvalidValue` show the message; **don't** lock
  controls.
- Fold `UiEvent::Settings` to re-render; show "applies on reconnect" when a changed field's
  `apply == NextSession` and `AppState != Stopped`.

### Phase 7 — Migration, verify, land
- Convert `~/.joi/config` (YAML) → `~/.joi/config.json`.
- `scripts/check.sh` green; commit/push when asked.

## 5. Atomicity guarantee

Every config write — bootstrap and every `update_setting` — goes through the single `atomic_write`
helper (temp in same dir → `sync_all` → atomic `rename`). No in-place writes exist, so a crash or
concurrent read can never observe a half-written `config.json`. Secrets are blanked before
serialization on every save.

## 6. Out of scope (for now)

- Exposing `RestartRequired` fields in the runtime UI (add when subsystems gain hot-reconfigure).
- Editing `api_key` / `model` from the UI (env/config-only).
- A `reconnect()` convenience command to apply `NextSession` changes immediately (optional follow-up).
