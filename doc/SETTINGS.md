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
> **Done (engine, Phase 8):** the `Voice` choices are **provider-owned** — the voice list is
> hardcoded per provider/model in `joi-providers` (`voice_catalog`), resolved from the active config,
> with non-destructive fallback when the stored voice isn't applicable. `UiEvent::Settings` *emission*
> moved from the `SessionManager` to `JoiApp` (only the app can resolve provider voices), so live
> provider/model switching works by construction. See §8. Backend crates green; frontend (Phase 6)
> still pending.
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
- **Provider-dependent option lists are provider-sealed.** Where a setting's valid choices depend on
  the provider (today: `Voice`), the list is hardcoded *in the provider* (`joi-providers`), keyed by
  the active provider/model in config. `joi-core` never names a provider or its voices (ARCH §5); it
  takes the resolved list as injected data. The app is the only layer that knows both settings and
  provider, so it does the plumbing. See §8.
- **Config is the durable record of user intent; fallback is non-destructive.** Config is written
  **only** when the user explicitly sets a value through the settings interface. The engine never
  rewrites a stored value just because it isn't valid for the *current* provider — it resolves an
  effective value at the point of use/display instead, so switching provider/model and back restores
  the original choice. See §8.

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
- **Who emits `UiEvent::Settings`:** **`JoiApp`**, not the `SessionManager`. The settings *snapshot*
  needs provider voices (§8), and the manager (in `joi-core`) can't call `joi-providers`. So the
  manager's `UpdateConfig` only swaps its config (for the next connect); the app builds the full
  snapshot and broadcasts it. *(Phase 4/5 originally had the manager emit; §8 corrects this.)*
- **Provider voice catalog:** hardcoded per provider/model in `joi-providers`
  (`voice_catalog(&Config) -> Vec<String>`), resolved from the active config — **not** queryable from
  the Gemini API (there is no voices-list endpoint; the set is also model-dependent: ~8 half-cascade
  vs ~30 native-audio). Core takes the list as injected `SettingsContext`. See §8.

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
- **Revised by §8:** the manager originally *also* emitted `UiEvent::Settings` here. §8 removes that
  (the manager can't resolve provider voices) and adds a `broadcast_settings` handle method the app
  uses instead.

### Phase 5 — Seam A (`joi-app/src/lib.rs`)
1. `JoiApp` gains `config: RwLock<Config>` (authoritative copy).
2. `settings_schema(&self) -> Vec<SettingDescriptor>` (query for the panel).
3. `update_setting(&self, id, value) -> Result<(), SettingsError>`:
   - lock → clone → `apply_setting` (validate); on error return it (UI shows it), state unchanged;
   - persist: serialize new config with **`api_key` blanked** → `atomic_write(config.json)`;
   - store back into the `RwLock`; send `Command::UpdateConfig` to the manager;
   - emit `UiEvent::Settings(schema)`. *(§8 moves this build+emit to the app and feeds it provider
     voices; the manager stops emitting.)*
- **Done:** `cargo test -p joi-app` green incl. a headless test: set `Voice` → persists to
  `config.json`, manager picks it up on next start, schema reflects it; invalid value rejected.

### Phase 6 — Frontend contract (hand to the frontend dev)
- Read `settings_schema()` at open; render controls from `SettingKind`.
- Write `update_setting(id, value)`; on `SettingsError::InvalidValue` show the message; **don't** lock
  controls.
- Fold `UiEvent::Settings` to re-render; show "applies on reconnect" when a changed field's
  `apply == NextSession` and `AppState != Stopped`.

### Phase 7 — Migration, verify, land
- Convert `~/.joi/config` (YAML) → `~/.joi/config.json`.
- `scripts/check.sh` green; commit/push when asked.

### Phase 8 — Provider-owned voice catalog + live provider/model switching

> Implement **before** Phase 6. Revises Phases 2/4/5: the voice list becomes provider-sourced,
> `settings_schema` takes injected option data, and `UiEvent::Settings` emission moves to `JoiApp`.

**Why.** The valid `Voice` choices depend on the provider/model and are **not queryable** from the
Gemini API (no voices-list endpoint; ~8 voices on half-cascade models, ~30 on native-audio). So the
list must be hardcoded, and it belongs in `joi-providers` (provider-sealed wire knowledge — ARCH §5),
not in `joi-core`. The current `joi-core::settings::KNOWN_VOICES` is a layering smell to remove.

**Model of the data (two different things, kept apart):**
- **Available voices = capability.** A *pure function of the active config*: `voice_catalog(&Config)`
  in `joi-providers` reads `cfg.live_api.provider` (+ Gemini model family) and returns the hardcoded
  list. No live session/connection needed — the panel must show voices while `Stopped`. Config is the
  *key* that selects which hardcoded table applies; the table's *contents* live in the provider.
- **Active voice = state.** Stored in config under the provider's block (`live_api.gemini.voice`),
  persisted. Written **only** when the user picks a voice via `update_setting`.

**Non-destructive resolution.** When the stored active voice isn't in the applicable list (e.g. after
a provider/model switch), the engine does **not** rewrite config. It resolves an *effective* voice for
display/use: `active ∈ list ? Some(active) : None` (`None` = model default; the Gemini adapter already
falls back server-side at connect, so no client-side default lookup is needed). The schema's
descriptor `value` shows the resolved voice. Because config is untouched, switching the provider/model
back restores the user's original choice.

**Why emission moves to `JoiApp`.** The schema now needs `voice_catalog` (in `joi-providers`). The
`SessionManager` lives in `joi-core` and can't call it, so it can't build the snapshot. Only `JoiApp`
knows both settings and provider — so it builds and emits `UiEvent::Settings`. Computing the catalog
from *current* config on every schema build (cheap — a `match` returning a `Vec`) is what makes live
provider/model switching automatic: switch → new key → new catalog → new `Choice.options` + resolved
`value` in the next emitted snapshot → the frontend re-renders on the same event it already folds.
Nothing to invalidate; correct by construction.

**Steps:**
1. **`joi-providers`** — `pub fn voice_catalog(cfg: &Config) -> Vec<String>`: matches on
   `cfg.live_api.provider`; for Gemini, branches on model family (native-audio → the documented 30;
   else → the safe 8); Mock → `[]`. Voice constants live in the `gemini` module. Unit-test both
   branches. *(No live instance; pure over config.)*
2. **`joi-core::settings`** — add `struct SettingsContext { voices: Vec<String> }` (extensible for
   future provider-dependent option lists). Change `settings_schema(cfg: &Config, ctx: &SettingsContext)
   -> Vec<SettingDescriptor>`. The `Voice` descriptor uses `ctx.voices` for `Choice.options` and the
   **resolved** value (stored voice if in `ctx.voices`, else empty = model default). **Remove
   `KNOWN_VOICES`.** Tests: resolution shows default when stored ∉ list; stored value shown when ∈
   list; config not mutated by schema building.
3. **`joi-core::manager`** — `Command::UpdateConfig` no longer emits `UiEvent::Settings` (just swaps
   config). Add `SessionManagerHandle::broadcast_settings(snapshot: SettingsSnapshot)` that sends
   `UiEvent::Settings { settings }` on the existing `ui_tx`. Move the old rebroadcast unit test out
   (it becomes an app-level test).
4. **`joi-app`** — add a private `current_schema()` that reads the `RwLock<Config>`, builds
   `SettingsContext { voices: joi_providers::voice_catalog(&cfg) }`, and calls core `settings_schema`.
   `settings_schema()` (query) returns it. `update_setting` persists/resyncs as before, then calls
   `handle.broadcast_settings(current_schema())` (instead of the manager emitting). Headless test:
   change `Voice` → persisted + `UiEvent::Settings` carries new value/options; assert resolution +
   non-destructive fallback (set an out-of-list voice directly in config → schema shows default,
   config unchanged).
- **Done:** `cargo test -p joi-core -p joi-providers -p joi-app` green; `joi-core` still names no
  provider (dep assertion holds).

## 5. Atomicity guarantee

Every config write — bootstrap and every `update_setting` — goes through the single `atomic_write`
helper (temp in same dir → `sync_all` → atomic `rename`). No in-place writes exist, so a crash or
concurrent read can never observe a half-written `config.json`. Secrets are blanked before
serialization on every save.

## 6. Out of scope (for now)

- Exposing `RestartRequired` fields in the runtime UI (add when subsystems gain hot-reconfigure).
- Editing `api_key` / `model` from the UI (env/config-only).
- A `reconnect()` convenience command to apply `NextSession` changes immediately (optional follow-up).
- **Runtime provider/model switching itself.** It's a *maybe* later; provider/model stay file+env
  today. But the §8 design supports it for free — the voice catalog is derived from current config on
  every schema build, so when provider/model become editable, the list and resolution follow with no
  new machinery (only the `SettingId` set and apply-timing would need the new entries).
