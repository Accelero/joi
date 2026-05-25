# Joi — Application Architecture

> **Status:** current architecture. This document is normative for the implemented TUI-first tree:
> the seven workspace crates in `crates/` follow these layers and contracts. Where a rule says *must*
> / *never*, treat it as a build-breaking invariant (enforced by `scripts/check.sh`). This file is
> the source of truth for structural decisions. The engine is frontend-agnostic, the only frontend
> built today is the TUI, boundary types are plain Rust + `serde` (no `ts-rs`), and Seam B / a Tauri
> host / a web UI are future work the design must not preclude. The built-in tool harness is
> implemented; MCP and richer tool UX follow `doc/TOOLS_PLAN.md`.

Joi is a local, provider-agnostic **voice + screen + terminal** companion (TUI-first; more frontends
will follow). It connects a human to a realtime multimodal model (Gemini Live today, others behind
the same trait), streams audio/video both ways, renders a live transcript, and **persists every
conversation as a resumable session** — the Claude-Code model: you can list past sessions, resume
one, start a new one, and the history seeds the model so it "remembers."

This document explains how the app is layered, what each module owns, and the interfaces that hold it
together — using **session management** as the worked example throughout.

---

## 1. The one principle everything follows

**All logic lives in Rust. The frontend is presentation and input only.**

Every substantive decision — session lifecycle, provider protocol, history persistence, audio DSP,
config, state — happens in a Rust crate. The UI (web or terminal) renders events and dispatches
commands. It never computes, buffers, transforms, orchestrates, or touches media.

> Litmus test for any new code: *"Could a headless process with no UI do this?"* If yes, it's
> backend logic and belongs in a crate. The UI only exists to **show** the result and **collect** an
> intent.

A second principle resolves almost every "which layer does this go in?" question:

**Separate _domain mechanism_ from _composition/policy_.** These are different axes from "does it
touch the disk":

- **Mechanism** = *what a thing is and how it behaves*, including its own file I/O. The jsonl session
  format, the "newest-first within token budget" rule, auto-naming a session from its first user
  turn — all mechanism, all domain, even though they read and write files. Mechanism lives in the
  **engine core**.
- **Policy / composition** = *wiring*: which directory the store uses, what to fall back to when
  there's no API key, whether to drive local microphones. This lives at the **composition root**
  (see §4, `JoiApp`). It adds no new domain knowledge; it only plugs mechanisms together.

"No I/O in core" means **no _device_ I/O** (microphone, speaker, screen) — not "never open a file."
Portable filesystem access for the domain's own data (config, session logs) is mechanism and belongs
in core.

---

## 2. Topology: three layers, two seams

```
┌─────────────────────────────────────────────────────────────────────────┐
│  FRONTEND (presentation + input only)                                     │
│                                                                           │
│   TUI render (ratatui)          [future frontends attach here too]        │
│        │                                                                  │
│        │  (in-process — no Seam B today)                                  │
└────────┼──────────────────────────────────────────────────────────────────┘
         │
         │   Seam A: the JoiApp Rust API (start/stop/send/sessions/events)
         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  ENGINE (host-agnostic; no Tauri, no webview, no ts-rs)                   │
│                                                                           │
│   joi-app  ──▶  joi-core  ◀──  joi-providers (Gemini · Mock)              │
│      (composition)  (domain)      (provider adapters)                     │
│                      ▲                                                     │
│                      ├── joi-tools  (built-in read/list/glob/grep/write/edit/bash) │
│                  joi-media  (native audio/screen I/O)                     │
└─────────────────────────────────────────────────────────────────────────┘
```

**The three layers:**

1. **Engine** — host-agnostic Rust. Knows nothing about Tauri, WebKit, HTTP, or terminals. Can run
   headless. This is `joi-core` + `joi-providers` + `joi-media` + `joi-app`.
2. **Hosts** — thin adapters that drive the engine and connect it to a frontend. Today there is one:
   the **TUI**, which calls `JoiApp` in-process. Future hosts (an out-of-process Tauri/IPC backend, a
   WebSocket server) would attach the same way. A host owns process lifecycle and transport, nothing
   domain-related.
3. **Frontends** — pure UI: render events, capture clicks/keys, dispatch commands.

**The seams** (the contracts that matter):

- **Seam A — `JoiApp` Rust API.** The boundary between the engine and any host. A single Rust object
  with a command surface (`start`, `stop`, `send_text`, `list_sessions`, `resume_session`, …) plus
  event/audio subscriptions. No host-specific type ever crosses it. This is the **only seam built
  today**, and it is what lets a new frontend become *the same engine with another face*.
- **Seam B — JSON IPC (future, not built).** It exists only when a host splits across a process
  boundary (e.g. a Tauri Rust ↔ webview frontend). It would be a thin JSON mirror of Seam A. The TUI
  has no Seam B — it calls `JoiApp` directly in-process. **Seam B is a transport detail, never a
  place for logic.**

> Why this matters for the rewrite: every feature is designed once, in the engine, behind Seam A.
> Any future frontend gets it for free. If you ever find yourself implementing a feature "for the
> TUI" (or "for the web UI"), it's in the wrong layer.

---

## 3. Vocabulary (read before the module list)

Two different things are both called "session." Keep them distinct:

| Term | What it is | Lifetime | Lives in |
|---|---|---|---|
| **Realtime session** (`RealtimeSession`) | A live, connected stream to the provider (the WebSocket and its event stream). | Ephemeral — opened on Start, closed on Stop. | provider adapter, driven by the manager |
| **Session** (persisted) | A resumable *conversation* — id, name, timestamps, and its ordered turns on disk. The Claude-Code unit you list and resume. | Durable — survives restarts. | `joi-core` history |

A persisted **Session** is the thing the user manages. A **Realtime session** is the transient
connection that *appends turns to* the current persisted Session. Starting/resuming a Session opens
a new Realtime session that is **seeded** from the persisted turns.

Other key types: `HistoryTurn` (one persisted line), `Role`/`Speaker` (user vs assistant/agent),
`UiEvent` (the serializable event the frontend renders), `SessionEvent` (the internal provider
event the manager consumes), `TokenBudget` (how much history re-seeds a session).

---

## 4. Modules and what they own

### Engine

#### `joi-core` — the domain

The pure heart. No OS device I/O, no Tauri, no networking transport. Owns:

- **`Config`** — typed configuration with sections (`live_api`, `history`, `media`, `ui`,
  `logging`). Knows how to parse and deep-merge `~/.joi/config.json` with `JOI_`/`GEMINI_*` env
  overrides, migrate a legacy YAML `~/.joi/config` once, resolve default paths, and bootstrap
  `~/.joi/prompt.md`. The provider key lives at `live_api.gemini.api_key` as a redacting secret type
  and is never written back to disk. **Mechanism:** core knows the schema; a host decides *to* load it.
- **`Clock`** — a trait (`now_ms() -> UnixMillis`) so time is injectable and tests are deterministic.
- **Realtime session abstraction** — the `RealtimeSession` trait (connect / send_audio /
  send_video_frame / send_text / take_events / close / capabilities), the ordered `SessionEvent`
  stream it emits, and the `UiEvent` enum the frontend consumes. Providers implement the trait;
  core never names a concrete provider.
- **Tool mechanism** — provider-neutral `Tool` contracts, `ToolRegistry`, `ToolRuntime`, permission
  profile evaluation, argument validation, and the manager-side dispatch state machine. Core owns
  the pipeline; concrete tool behavior is sealed in `joi-tools`.
- **History** — the `HistoryStore` trait (append / load_within_budget / clear / meta) and its
  implementations, including the **session store** (see §6). This is where persisted Sessions live.
- **Runtime settings** — a curated settings surface over `Config` (`voice`, `accent`,
  `background`) with apply timing metadata. Core validates and applies setting values in memory;
  `joi-app` persists and broadcasts the resulting snapshot.
- **`SessionManager`** — the **actor** that owns the live state machine: it holds the current
  `RealtimeSession`, processes `Command`s (Start/Stop/SendText/…), maps `SessionEvent` → `UiEvent`,
  appends finalized turns to the `HistoryStore`, tracks metrics, and fans events out to subscribers.
  Hosts never see the actor; they hold a cheap, cloneable **`SessionManagerHandle`**.
- **Media _contracts_ and pure DSP** — `ScreenSource`, `VideoFrame`, resampling/level math. The
  *algorithms*, not the devices.

> Core depends on nothing host-specific and compiles standalone. Everything else points at it.

#### `joi-providers` — realtime provider adapters

Implements `RealtimeSession` for each provider: **Gemini Live** (real, over the vendored
`adk-realtime`) and a **Mock** used by tests (deterministic transcript/turn loop, no network). It
exposes `build_session_factory(&Config) -> SessionFactory`, `build_connectivity_probe(&Config)`, and
`voice_catalog(&Config)`. A `SessionFactory` is the injection seam: the composition root picks the
provider by config, the manager just calls `factory.create()` and gets a fresh
`Box<dyn RealtimeSession>`. **All wire-protocol knowledge is confined here** — message framing,
Gemini setup details, usage-metadata parsing, token-free reachability probing, voice catalogs, and
provider-specific history seeding. If a behavior is provider-specific, it lives in this crate and
nowhere else.

#### `joi-tools` — built-in tool implementations

The sealed implementation crate for the first agent-harness tools: `read`, `list`, `glob`, `grep`,
`write`, `edit`, and `bash`. It depends on `joi-core`'s tool contracts and owns filesystem/path
checks, output caps, and non-interactive process execution. `joi-core` does not depend on
`joi-tools`; `joi-app` builds the registry from config and injects the resulting `ToolRuntime` into
the manager. Future MCP support adds another tool source behind the same `dyn Tool` interface.

#### `joi-media` — native audio/screen I/O

The only crate that touches devices. Behind a `MediaEngine` interface: cpal microphone capture (with
noise suppression / AGC / echo cancellation), cpal playback, xcap screen capture. It binds to a
`SessionManagerHandle` and pushes captured mic frames / screen frames in, and plays provider audio
out. **Media never crosses Seam B** — raw PCM and frames stay in Rust; only transcripts and control
events become JSON.

#### `joi-app` — the composition root (Seam A)

The crate that *assembles* the engine and exposes **`JoiApp`**, the single object every host drives.
It owns **composition and policy**, never domain rules:

- `JoiApp::build(Config, MediaMode)` — the composition root. It picks the provider factory, decides
  the history directory from `config.history.dir`, falls back to in-memory history when the history
  dir is unavailable, builds the `MediaEngine` only in `MediaMode::LocalDevices`, spawns the
  `SessionManager`, and wires everything together. With no usable provider credentials it still
  constructs a `JoiApp`, but there is no manager or persisted-session store; session commands return
  clear errors instead of panicking.
- The **command surface** (Seam A): `start`, `stop`, `send_text`, `send_audio`, `set_mic_muted`,
  `start_screenshare`, `stop_screenshare`, `check_reachability`, `has_api_key`, `ui_config`, and the
  **session commands** `list_sessions`, `current_session`, `new_session`, `resume_session`,
  `session_turns` (see §6), tool approval command `resolve_tool_permission`, plus runtime settings
  commands `settings_schema` and `update_setting`.
- The **output streams**: `subscribe_events()` (the `UiEvent` broadcast) and `subscribe_audio()`
  (for headless hosts that transport audio themselves).

`MediaMode` is the one knob that distinguishes a rich host from a headless one:
- `LocalDevices` — the engine drives the machine's mic/speaker/screen itself (desktop, TUI).
- `None` — no local devices; the host feeds and consumes audio via `send_audio`/`subscribe_audio`
  (the headless test, a future CLI or WebSocket server).

### Hosts

| Host | Frontend | Media | Seam B? | Purpose |
|---|---|---|---|---|
| **TUI** | ratatui terminal | `LocalDevices` | no | The only host built today. Full voice/screen/session UX, `/resume` + `/new` session commands, `/voice` runtime voice selection, no webview; calls `JoiApp` directly in-process. |
| **Headless test** | none | `None` | no | Not a binary — an integration test in `joi-app` (Mock provider) that drives a full command→event loop. The headless proof (see §8). |
| _(future)_ **Tauri/IPC backend** | Web UI | `LocalDevices` | yes | Not built. Would add a thin JSON mirror of Seam A (Seam B) for an out-of-process webview. |
| _(future)_ **WS server** | WS clients | `None` | no | Not built. Would expose the Seam-A command + `UiEvent` contract over `/ws`. |

The headless test is an **architectural fixture**: `scripts/check.sh` asserts the engine carries no
dependency it shouldn't (see §8). If those assertions pass and the headless test drives a full loop
with no devices, Seam A is honest.

### Frontends

- **TUI render** — pure ratatui view + reducers over the `UiEvent` stream. Model/reducers are
  separate from rendering and do no I/O; they fold `UiEvent`s into view state and emit `Command`s.
  This is the only frontend today.
- **(future) Web UI** — a React/TS frontend would render the same `UiEvent`s and dispatch the same
  commands through an out-of-process IPC host. The boundary types (`UiEvent`, `SessionSummary`, …)
  are plain Rust structs/enums with `serde` derives; a web frontend consumes their JSON form. The
  rewrite does **not** use `ts-rs` or a generated `src/bindings/` (see §7).

---

## 5. Configuration and settings

Configuration is owned by `joi-core` as domain mechanism. The current file is
`~/.joi/config.json`. A legacy YAML `~/.joi/config` is migrated once to JSON when `config.json` is
absent; the legacy file is left in place as a backup. The system prompt is bootstrapped into
`~/.joi/prompt.md`; when that file is present and non-blank, it overrides
`live_api.gemini.system_instruction`.

Precedence, lowest to highest:

1. Built-in defaults.
2. `~/.joi/config.json`, deep-merged over defaults.
3. `JOI_` environment variables split with `__`, for example
   `JOI_MEDIA__AUDIO__FRAME_MS=30`.
4. Conventional shortcuts `GEMINI_API_KEY` and `GEMINI_MODEL`.

Environment normally wins over the file. The prompt file is the documented exception: it is treated
as the explicit persona source and overrides the inline system instruction.

The config sections are:

- `live_api`: provider selection, Gemini model/API key/voice/transcription/context compression,
  provider token budget, and reachability-probe cadence.
- `history`: persisted-session directory.
- `logging`: log level and resolved log path. The current TUI host uses `$JOI_TUI_LOG` or its
  platform state-dir log file instead of `logging.file`.
- `media.audio`: device names, mic frame size, echo cancellation, noise suppression, and auto gain.
- `media.screen`: FPS, max width, and JPEG quality.
- `ui.terminal`: terminal background and accent color.
- `tools`: disabled-by-default agent-harness tools, built-in names, readable/writable roots,
  timeout/output/network limits, and ordered permission rules.

Secrets are redacted in memory and never written back to disk. All config writes go through
`joi-core::util::atomic_write`.

Runtime settings are a curated view over `Config`, not a separate store. `joi-core::settings`
defines `SettingId`, `SettingValue`, descriptors, validation, and apply timing. `joi-app` owns the
policy: clone config, apply and validate the setting, persist the redacted JSON atomically, send
`Command::UpdateConfig` to the manager, then broadcast `UiEvent::Settings`.

Editable settings today:

- `Voice`: provider/model-owned choice list from `joi-providers::voice_catalog`, applies on next
  session start.
- `Accent`: terminal accent color, immediate for frontends that fold the settings snapshot.
- `Background`: terminal background color, immediate for frontends that fold the settings snapshot.

The TUI currently exposes `/voice`. A generic settings panel for the full curated surface is future
frontend work.

---

## 6. Worked example: session management end-to-end

This is the feature to keep in mind for every layer. Requirements, Claude-Code style:

- Conversations persist automatically; each is a **Session** with a stable id and a human name.
- The user can **list** past sessions, **resume** one (its history re-seeds the model), or **start a
  new** one — at runtime, without restarting.
- A session is **auto-named** from its first user message; the user can rename it.
- On resume, only history that fits the model's input **token budget** is re-seeded.

### 6.1 The data — owned by `joi-core` (mechanism)

```rust
// One conversation, as the user thinks of it.
struct Session { id: String, meta: SessionMeta, turns: Vec<HistoryTurn> }

// Name + timestamps — plain serde struct; persisted in index.json, surfaced to the frontend.
struct SessionMeta {
    name: Option<String>,        // None = unnamed; auto-filled from first user turn
    created_at: UnixMillis,
    last_opened: UnixMillis,
    last_updated: UnixMillis,    // bumped on every appended turn → sort key for the picker
}

// A row the picker renders and selects from.
struct SessionSummary { id: String, meta: SessionMeta }
```

**On-disk layout** (under `config.history.dir`, default `~/.joi/sessions`):

```
~/.joi/sessions/
  index.json            # { uuid -> SessionMeta }  — one map for the whole dir
  <uuid-a>.jsonl        # append-only turns, one HistoryTurn per line
  <uuid-b>.jsonl
```

Design rules (mechanism, all in core):
- Turn logs are **append-only**; metadata lives in `index.json`, so bumping `last_updated` never
  rewrites a large turn log.
- `index.json` is **rebuildable** from the logs if lost, and tolerant of corruption (a bad line is
  skipped, not fatal).
- Listing sorts by `last_updated` **newest-first**.

### 6.2 The store — `SessionStore: HistoryStore` (core)

`SessionStore` is bound to the **current** Session and implements the generic `HistoryStore` trait,
so the `SessionManager` appends turns and re-seeds with **no knowledge that sessions exist**:

```rust
impl HistoryStore for SessionStore {
    async fn append(&self, turn: HistoryTurn) -> Result<(), HistoryError>;          // writes <current>.jsonl + bumps index, auto-names on first user turn
    async fn load_within_budget(&self, b: TokenBudget) -> Result<Vec<HistoryTurn>>; // newest-first turns that fit the budget → re-seed on resume
    async fn clear(&self) -> ...;
    async fn meta(&self, b: TokenBudget) -> ...;
}

// Session-specific surface (beyond the trait):
impl SessionStore {
    fn create_new(dir, clock) -> Result<Self>;     // fresh uuid, registered in index
    async fn list(dir: &Path) -> Vec<SessionSummary>;
    async fn current_summary(&self) -> SessionSummary;
    async fn start_new(&self) -> Result<SessionSummary>;   // switch current → brand-new, at runtime
    async fn switch_to(&self, id) -> Result<SessionSummary>; // switch current → existing, refresh last_opened
    async fn rename(&self, name: Option<String>) -> Result<()>;
}
```

**Single current session.** The store binds exactly one current Session at a time (matching Claude
Code). "List sessions" means *resumable* sessions on disk — not several concurrently-live ones.
Switching stops the live realtime connection and retargets the store; the previous log is untouched.
(Multiple *simultaneously live* sessions would mean multiple `SessionManager`s and a session map in
`JoiApp` — an explicit future extension, not a tweak.)

### 6.3 The lifecycle — `SessionManager` (core)

The manager treats history generically. The interesting session behavior is at the seams of its
state machine:

- **Start:** open a `RealtimeSession` via the factory and always ask the current `HistoryStore` for
  `load_within_budget(budget)`. A brand-new session has an empty log, so it seeds nothing; a resumed
  session seeds its prior turns as `initial_context` so the model "remembers." The budget is sized to
  the **Live API input limit**, not the giant text-model window.
- **Stop:** close the realtime session; the persisted Session and its log remain.

### 6.4 The composition — `JoiApp` (joi-app, policy)

`JoiApp` decides *where* sessions live and *what happens without a key*, then exposes the
host-facing command surface:

```rust
impl JoiApp {
    async fn list_sessions(&self) -> Vec<SessionSummary>;          // [] when not persisted
    async fn current_session(&self) -> Option<SessionSummary>;
    async fn new_session(&self) -> Result<SessionSummary>;         // stops live session first
    async fn resume_session(&self, id: &str) -> Result<SessionSummary>; // stops, retargets, next start() seeds
    async fn session_turns(&self, id: &str) -> Result<Vec<HistoryTurn>>; // full transcript for UI reload
}
```

Policy lives here: directory from `config.history.dir`; **fall back to in-memory history** (sessions
unavailable, commands return a clear error rather than panic) when the dir is unusable. When provider
credentials are missing, no `SessionManager` is spawned and session commands fail clearly.

### 6.5 The host + frontend

- **TUI:** a `/resume` command opens a picker rendered from `list_sessions()`; selecting a row calls
  `resume_session(id)` and repopulates the transcript from `session_turns(id)`. It does **not**
  auto-start the provider stream; the user opens the billable live session manually with F2. All
  in-process Rust. `/new` switches to a fresh session, and `/voice` writes the provider voice setting
  through `update_setting`.
- **(future) out-of-process frontend:** the same calls would be JSON commands mirrored 1:1 over Seam
  B; a web picker would render `SessionSummary[]` (the serde JSON form of the Rust type) and invoke
  `resume_session`. The IPC host adds nothing but the JSON hop.

### 6.6 Full flow: user resumes session "morning chat"

```
TUI: user opens the picker (/resume) ─► JoiApp::list_sessions()              [Seam A]
        │  renders SessionSummary[] (newest last_updated first)
        ▼
TUI: user selects a row ─► JoiApp::resume_session(id)                        [Seam A]
        │
JoiApp: stop() any live session ─► SessionStore::switch_to(id) (bump last_opened)
        │  returns SessionSummary
        ▼
TUI reloads the transcript ─► JoiApp::session_turns(id)                      [Seam A]
        │
User presses F2 ─► JoiApp::start(resume = true)                              [Seam A]
        │
SessionManager: factory.create() ─► connect() with
        initial_context = store.load_within_budget(budget)   (re-seed!)
        │  + MediaEngine starts mic capture (LocalDevices)
        ▼
SessionEvent stream ─► UiEvent ─► broadcast ─► TUI folds it ─► transcript renders live
        each finalized turn ─► store.append() ─► <id>.jsonl + index bump
```

Every call is in-process Rust through Seam A — no JSON hop. Note every layer's role: core *is* the
session, the manager *runs* it, `JoiApp` *wires and exposes* it, the TUI *shows and triggers* it. A
future out-of-process frontend would insert a JSON mirror (Seam B) at the `[Seam A]` boundaries
without changing anything below. Nothing leaks across.

---

## 7. Commands and events (the contract shape)

The system is **command-in, event-out** through Seam A (a future out-of-process frontend would
mirror it over Seam B):

- **Commands** (host → engine): imperative methods on `JoiApp` / `SessionManagerHandle`. A future IPC
  host would map each command **1:1** to a `JoiApp` method — no command in the IPC that isn't on the
  Rust API.
- **Events** (engine → host): a single `UiEvent` broadcast channel. The manager translates internal
  `SessionEvent`s (transcript lines, turn boundaries, audio, close reasons) and metrics into the
  serializable `UiEvent` enum. `JoiApp` also publishes settings snapshots on that same channel after
  settings changes. The frontend's job is to fold `UiEvent`s into view state — nothing more.

Today `UiEvent` covers lifecycle state, transcript deltas, connection status, token-free provider
reachability, history metadata, runtime settings snapshots, throughput metrics, and surfaced errors.

`UiEvent` and the session/payload types are **plain Rust with `serde` derives** — no `ts-rs`, no
generated `src/bindings/`, no parity gate. The TUI consumes them in-process; the serde JSON form is
what a future out-of-process frontend would receive. "The frontend is UI only" still holds
structurally: the only way a frontend can change anything is to send a `Command`, so it cannot invent
engine state the backend doesn't define.

---

## 8. Testing & verification strategy

- **Core is unit-tested in isolation** with a `TestClock` and `InMemoryHistory` — deterministic, no
  devices, no network. Session semantics (auto-naming, budget windowing, switch/resume, index
  durability) are pure-logic tests.
- **Providers** have focused unit tests around adapter mapping and configuration. The **Mock**
  provider (in `joi-providers`) drives the rest of the engine, which talks only to the
  `RealtimeSession` trait.
- **`joi-testkit`** holds the shared test doubles (clock/media) and the provider **conformance
  suite**, run against the Mock provider.
- **Seam A is exercised headlessly** by an integration test in `joi-app` that builds `JoiApp` with
  `MediaMode::None` + the Mock provider and drives a full command→event loop — no devices, no GUI.
  That test *is* the headless gate (it replaces a standalone CLI).
- **Architectural enforcement:** `scripts/check.sh` (mirrors CI) runs `cargo fmt --check`,
  `clippy -D warnings`, `cargo test --workspace`, **and** dependency assertions: no
  `cpal`/`xcap`/`sonora`/`adk-realtime`/`reqwest`/`ratatui` under `joi-core`; no `ratatui`/`crossterm`
  outside `joi-tui`; no `tauri`/`webkit`/`ts-rs` anywhere.

---

## 9. Invariants checklist

1. **No logic in the frontend.** No compute, buffering, DSP, media, or orchestration in the TUI view
   (or any future frontend).
2. **No Tauri/WebKit/`ts-rs` types in the engine, and no HTTP/provider/device/UI dependencies in
   `joi-core`.** Enforced by `check.sh`'s dependency assertions.
3. **No media across Seam B.** Audio/video stay native (`joi-media` ↔ engine); only JSON would cross IPC.
4. **Mechanism in core, policy in `joi-app`.** Domain rules (incl. their own file I/O) in `joi-core`;
   wiring/fallbacks/dir-selection in `joi-app`.
5. **Providers are sealed.** All wire-protocol knowledge in `joi-providers`, behind `RealtimeSession`.
   Core never names a concrete provider.
6. **One event channel, plain serde types.** `UiEvent` is the only event surface; boundary types are
   ordinary Rust structs/enums with `serde` derives — no `ts-rs`, no generated bindings.
7. **Commands are 1:1.** Every command is a `JoiApp` method; a future IPC host mirrors each exactly
   once — no orphans either way.
8. **The engine runs headless.** A new feature must work through the `joi-app` headless integration
   test (`MediaMode::None` + Mock) with no GUI before it's "done."
9. **Realtime session ≠ persisted Session.** Keep the two concepts (and their types) distinct.
