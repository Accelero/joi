# Joi — Clean Rewrite Plan (TUI-first)

> **Deliverable.** This document is the rewrite plan. On approval it is saved verbatim to
> `doc/PLAN.md`. It is written to be executed by a *fresh* agent with no prior context: it states the
> target layout, the interfaces (first-class), the two subsystems that are hard to get right
> (audio + config), and an ordered milestone list with done-criteria.
>
> Read `doc/ARCH.md` (normative architecture) and `doc/SPEC.md` (functional requirements `FR-*`)
> first. The old implementation lives under `old/` and is the **reference to port from** — its code
> quality is uneven, but its behavior is correct and hard-won. Port behavior faithfully; restructure
> freely.

---

## 1. Context — why this rewrite

Joi grew feature-by-feature; architectural decisions were retrofitted, so the tree drifted from the
layering in `doc/ARCH.md`. We are rewriting cleanly against the normative architecture, with
**interfaces as first-class citizens** — every layer boundary is a small, explicit Rust trait/type
surface, defined before the code behind it.

The intended outcome: the same engine that a future desktop/web frontend will drive, exercised today
through a TUI, with (a) the **audio pipeline reproduced with fidelity** (it took the longest to get
right), and (b) **configuration handled systematically** (one typed schema, one precedence rule, one
path resolver).

## 2. Scope & non-goals

**In scope** (the crates we build):

- `joi-core` — domain (config, clock, history/sessions, realtime-session contracts, manager actor,
  media contracts + pure DSP, metrics, connectivity).
- `joi-providers` — `RealtimeSession` adapters: **Gemini** (real, vendored `adk-realtime`) and
  **Mock** (tests). No OpenAI stub.
- `joi-media` — native cpal audio capture/playback + xcap screen capture + the APM (sonora) chain.
- `joi-app` — composition root, exposes the `JoiApp` Seam-A API.
- `joi-tui` — the only frontend host (ratatui).
- `joi-testkit` — shared test doubles + the provider conformance suite.
- `vendor/adk-realtime`, `vendor/sonora-aec3` — two **required** patched dependencies (see §7.4).

**Out of scope / non-goals** (design must not preclude them; do not build them now):

- **No web frontend, no Tauri host, no `src-tauri`, no `src/` TS.** Boundary types are **plain native
  Rust** consumed in-process by the TUI.
- **No `ts-rs`, no `bindings/`, no TS parity gate.** (Approved deviation — see §3.)
- **No `joi-cli`, no `joi-server`.** (Approved deviation — see §3.)
- **No tools/shell/memory** (`FR-24/25` are `[LATER]`). Keep the `RealtimeSession` tool hooks present
  but unimplemented so the seam exists.
- **No session resumption handle / native screen input** beyond honest `Capabilities` flags = false.

## 3. Approved deviations from `doc/ARCH.md`

`ARCH.md` is normative but two of its invariants are explicitly overridden for this rewrite. **Update
`doc/ARCH.md` to match as the final step (M7).**

1. **Invariant #6 (generated boundary types) is dropped.** There is no TS frontend; the boundary is a
   native Rust API. `UiEvent`, `SessionSummary`, etc. are ordinary Rust enums/structs with `serde`
   derives (for on-disk history and future transport), but **no `ts-rs`**. Seam B (JSON IPC) does not
   exist.
2. **Invariant #8 (prove it headless) is satisfied without a separate binary.** Instead of `joi-cli`,
   a headless integration test in `joi-app` builds `JoiApp` with `MediaMode::None` + the Mock
   provider and drives a full command→event loop. That test *is* the headless gate.

Everything else in `ARCH.md` stands: one principle (all logic in Rust, frontend renders + dispatches),
mechanism-in-core vs policy-in-app, provider-sealed-in-`joi-providers`, one `UiEvent` surface,
no device I/O in core, realtime-session ≠ persisted-session.

## 4. Target project layout

```
joi/
├─ Cargo.toml                 # workspace: members, shared deps, profiles, lints, [patch.crates-io]
├─ .cargo/config.toml         # optional; only if a build-env tweak is needed (none required now)
├─ rust-toolchain.toml        # pin toolchain (optional but recommended)
├─ scripts/check.sh           # fmt + clippy -D warnings + cargo test --workspace + dep assertions
├─ config/joi.example.yaml    # documented example of the full config schema
├─ doc/{ARCH.md,SPEC.md,PLAN.md}
├─ vendor/
│  ├─ adk-realtime/           # patched: realtimeInput.audio (not deprecated mediaChunks[])
│  └─ sonora-aec3/            # patched: AEC filter-shrink no-op (upstream panics)
└─ crates/
   ├─ joi-core/      src/{lib, clock, error, config, metrics, connectivity,
   │                       session/{mod,event}, history/{mod,memory,session},
   │                       media, manager}.rs
   ├─ joi-providers/ src/{lib, factory, gemini, mock}.rs
   ├─ joi-media/     src/{lib, capture, playback, screen, engine}.rs
   ├─ joi-app/       src/lib.rs
   ├─ joi-testkit/   src/lib.rs
   └─ joi-tui/       src/{main, app, keys, input, ui, theme, transcript, picker}.rs
```

Notes vs. old tree: drop `joi-cli`, `joi-server`, `src-tauri`, `src/`; drop `history/file.rs` (the
`JsonlHistory` single-file store is dead — `SessionStore` + `InMemoryHistory` are the only two impls);
drop `capture.rs` from `joi-core` if it only held the old `ScreenSource` contract — fold contracts
into `media.rs`. Add `joi-tui/src/picker.rs` for the session picker (the old TUI lacked one; `FR-20`
requires it).

## 5. Dependency graph & layering invariants

```
joi-core   ← depends on nothing host/provider/device-specific
  ▲  ▲  ▲
  │  │  └── joi-media     (cpal, sonora, xcap, image)  — devices only, no session logic
  │  └───── joi-providers (adk-realtime[gemini], reqwest, secrecy) — wire protocol only
  └──────── joi-testkit   (joi-core + joi-providers doubles)
joi-app    ← joi-core + joi-providers + joi-media   (composition + policy)
joi-tui    ← joi-app + joi-core                      (presentation + input)
```

Build-breaking invariants (asserted by `scripts/check.sh`, §10):
- `joi-core` has **no** dependency on `cpal`/`xcap`/`adk-realtime`/`ratatui`/`reqwest`. It compiles
  standalone and names **no concrete provider**.
- All wire-protocol knowledge lives in `joi-providers`; all device I/O in `joi-media`.
- `joi-tui` depends on `joi-app` (Seam A) and `joi-core` (types) only — never on `joi-providers` or
  `joi-media` directly.
- No `ts-rs`, no `tauri`, no `webkit` anywhere (trivially true; assert anyway to prevent regressions).

## 6. The configuration model (build this systematically — §1 user priority)

One typed schema in `joi-core/src/config.rs`, one precedence rule, one path resolver. Port from
`old/crates/joi-core/src/config.rs` (674 lines) and `old/config/joi.example.yaml`.

### 6.1 Schema (sections → fields → defaults)

| Section.field | Type | Default | Notes |
|---|---|---|---|
| `live_api.provider` | enum `gemini\|mock` | `gemini` | drop `openai` |
| `live_api.gemini.model` | String | `gemini-live-2.5-flash-native-audio` | exact model id |
| `live_api.gemini.api_key` | `ApiKey` (redacting) | `""` | `#[serde(default)]`; never logged |
| `live_api.gemini.voice` | `Option<String>` | `Aoede` | |
| `live_api.gemini.system_instruction` | String | persona | seeded every session |
| `live_api.gemini.input_transcription` | bool | `true` | user audio → text (`FR-3`) |
| `live_api.gemini.output_transcription` | bool | `true` | agent audio → text |
| `live_api.reachability_probe_secs` | u64 | `20` | `0` disables periodic probe |
| `history.dir` | `Option<PathBuf>` | `null`→`~/.joi/sessions` | resolved at load |
| `history.token_budget` | u32 | `32000` | Live API **input** limit, not the 1M text window; min 1000 |
| `logging.level` | String | `info` | error\|warn\|info\|debug\|trace |
| `logging.file` | `Option<PathBuf>` | `null`→`~/.joi/logs` | |
| `media.audio.input_sample_rate` | u32 | `16000` | Gemini in = 16 kHz mono |
| `media.audio.output_sample_rate` | u32 | `24000` | Gemini out = 24 kHz mono |
| `media.audio.frame_ms` | u32 | `20` | validated `5..=60`; 20 ms = 320 samples @16k |
| `media.audio.input_device` | String | `default` | `"default"` or exact device name |
| `media.audio.output_device` | String | `default` | |
| `media.audio.echo_cancellation` | bool | `true` | AEC3 |
| `media.audio.noise_suppression` | bool | `true` | HPF + NS |
| `media.audio.auto_gain` | bool | `true` | AGC2 |
| `media.screen.enabled` | bool | `false` | |
| `media.screen.fps` | f32 | `1.0` | validated `(0,60]` |
| `media.screen.max_width` | u32 | `768` | Gemini tiles ~768 |
| `media.screen.quality` | u8 | `80` | JPEG, validated `1..=100` |
| `ui.terminal.theme` | String | `joi-dark` | |
| `ui.terminal.accent` | String | `#9aede4` | hex or named |
| `ui.terminal.background` | String | `transparent` | `transparent`→`Color::Reset` |
| `ui.terminal.scrollback` | u32 | `5000` | |
| `ui.terminal.font` | String | `JetBrains Mono` | informational |

(Drop `media.screen.capture_path` — it only distinguished webview vs native capture, irrelevant
without a webview. Native capture is the only path.)

### 6.2 Precedence (lowest → highest)

`Config::default()` → YAML file (deep merge) → `JOI_*` env (nested via `__`, e.g.
`JOI_MEDIA__AUDIO__FRAME_MS=30`) → conventional shortcuts `GEMINI_API_KEY`, `GEMINI_MODEL`. **Env
always wins over file.** Use `figment` with `Env::prefixed("JOI_").split("__")`. Entry point:
`Config::load(cli_path: Option<&Path>) -> Result<Config, ConfigError>`.

### 6.3 Paths, secret, validation

- `ProjectPaths::resolve()` → `~/.joi/{config, sessions, logs}` via `directories::BaseDirs`. Core is
  the single source of paths; null `history.dir`/`logging.file` resolved post-load. Write defaults to
  `~/.joi/config` if missing (best-effort).
- `ApiKey(String)`: `Debug` prints `ApiKey(unset)` / `ApiKey(<redacted>)`, never the value
  (`SEC-1`). `get() -> Option<&str>` (None when empty), `is_set() -> bool`.
- `validate()`: sample rates > 0; `frame_ms ∈ 5..=60`; `screen.fps ∈ (0,60]`; `quality ∈ 1..=100`;
  `token_budget ≥ 1000`. Fail loudly on load.
- Each subsystem reads only its slice: `joi-media` gets a `MediaConfig` derived from `media.*`;
  `joi-providers` gets `live_api.*`; `joi-tui` reads `ui.terminal.*`.

## 7. The audio subsystem (the critical path — §1 user priority)

**Port from `old/crates/joi-media/` and `old/crates/joi-core/src/media.rs` with fidelity.** Same
constants, same stage order, same render-reference discipline. Restructure the code, but do **not**
re-derive the DSP — it is correct and was painful to get right. The proven invariants below are
non-negotiable.

### 7.1 Formats & constants (exact)

- Capture/provider **input**: 16 kHz, mono, PCM16. APM runs at **16 kHz, 10 ms = 160-sample** blocks.
- Provider **output / playback**: 24 kHz, mono, PCM16.
- Mic frames emitted to the session: `frame_ms` (default 20 ms = 320 samples @ 16 kHz).
- `MAX_RENDER_BACKLOG = APM_FRAME * 20 = 3200 samples` (~200 ms) — cap the AEC far-end buffer; drop
  oldest on overflow (provider can stream faster than real-time and overflow AEC delay tracking).
- Linear resampler (no FFT crate) on both sides: device↔16 kHz (mic), 24 kHz↔device (playback).
- Channels: capture channel cap 64; frame channel cap 8; render reference = unbounded `std::sync::mpsc`.

### 7.2 Pipeline (two halves + screen)

**Mic →:** cpal input callback (realtime: downmix to mono ch0, atomic mute-gate, forward via channel,
**no DSP, no alloc**) → `joi-capture` thread: resample device→16 kHz → accumulate to 160-sample APM
blocks → **for every capture block, feed one render block to AEC** (real far-end, or silence if none)
→ `process_capture` through the APM chain → convert to i16 → accumulate to `frame_ms` frame →
`SessionManagerHandle::send_audio`.

**APM stage order (sonora):** `EchoCanceller(AEC3)` → `HighPassFilter` (with NS) → `NoiseSuppression`
→ `GainController2(AGC2)`. Gate each by config: AEC by `echo_cancellation`; HPF+NS by
`noise_suppression`; AGC by `auto_gain` (and disable AGC when NS is off, to avoid amplifying residual
echo).

**Playback →:** `subscribe_audio()` broadcast → playback pump: empty frame ⇒ `Flush` (barge-in),
else `Pcm` → `joi-playback` thread: resample 24 kHz→device → `JitterBuffer` (VecDeque). Output
callback pulls fixed blocks (silence on underrun), **and forwards the just-emitted device-rate mono
to the AEC render reference** (this is the only correct tap point), then replicates mono across device
channels.

**Screen →:** `joi-screen` thread: xcap primary monitor at `fps` → downscale to `max_width` → JPEG at
`quality` → `VideoFrame` → `SessionManagerHandle::send_frame`.

### 7.3 Non-negotiable invariants (hard-won; keep comments explaining each)

1. **Feed the AEC far-end every capture frame, including silence.** Feeding render only while the
   agent speaks drifts the AEC's render/capture alignment until it cancels the *user's* voice after a
   few turns. (Old `capture.rs:426-437`.)
2. **Tap the render reference at the playback output**, after the jitter buffer, on the realtime
   callback — forward the actually-emitted samples, never the enqueued ones.
3. **No allocation/blocking/DSP in either cpal callback.** Only downmix, atomic check, channel send.
   All DSP runs on the dedicated capture thread.
4. **Flush the jitter buffer immediately on barge-in** (`Interrupted`/empty frame) — that is `FR-2`/
   `FR-7`.
5. **Cap the render backlog** (`MAX_RENDER_BACKLOG`) and drop oldest on overflow.
6. **Lock order `capture → render_sink`** in the engine to avoid a start/stop deadlock; all lifecycle
   methods (`start_capture`/`stop_capture`/`start_screenshare`/`stop_screenshare`) are idempotent.

### 7.4 The two required vendored patches (carry over verbatim)

Copy `old/vendor/` and the `[patch.crates-io]` block from `old/Cargo.toml`. Workspace must
`exclude = ["vendor"]`.

- **`vendor/sonora-aec3`** — upstream 0.1.0 panics (`slice index starts at N but ends at N-1`) when
  the AEC adaptive filter *shrinks* (echo path shortens after a barge-in flush / stall). The patch
  no-ops `zero_filter` when `new_size <= old_size`. Without it, the capture thread crashes on
  barge-in. Marked `PATCH(joi)` in `src/adaptive_fir_filter.rs`.
- **`vendor/adk-realtime`** — send the current `realtimeInput.audio` blob instead of the deprecated
  `mediaChunks[]`, which current Gemini Live models reject. `joi-providers` depends on
  `adk-realtime = "0.8"` with only `features = ["gemini"]`.

### 7.5 MediaEngine interface (binds devices to Seam A)

`MediaEngine::new(handle: SessionManagerHandle, cfg: MediaConfig)` runs on a dedicated OS thread and
owns the cpal streams + xcap. Methods: `start_capture`, `stop_capture`, `set_mic_muted(bool)`,
`start_screenshare`, `stop_screenshare`. Internally it wires the render-sink (AEC ref) between
playback and capture, publishes the playback device rate (`AtomicU32`) to the capture resampler, and
runs three pumps: playback (provider audio out), audio drain (mic in → `send_audio`), frame drain
(screen → `send_frame`). `MediaConfig` is derived from `media.*` config (frame samples, device names,
APM toggles, screen fps/width/quality).

## 8. Interface catalog (the first-class citizens)

Define these *before* their implementations. Signatures port from the old tree (cited).

**`joi-core::clock`** — `trait Clock: Send+Sync+Debug { fn now_ms(&self) -> UnixMillis }`;
`SystemClock`, `TestClock{advance,set}`. (`type UnixMillis = u64`.)

**`joi-core::session`** — the provider seam:
```rust
#[async_trait] pub trait RealtimeSession: Send {
    async fn connect(&mut self, cfg: SessionConfig) -> Result<(), SessionError>;
    async fn close(&mut self) -> Result<(), SessionError>;
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SessionError>;
    async fn send_video_frame(&mut self, frame: &VideoFrame) -> Result<(), SessionError>;
    async fn send_text(&mut self, text: &str) -> Result<(), SessionError>;
    async fn end_audio_stream(&mut self) -> Result<(), SessionError> { Ok(()) }
    async fn send_tool_result(&mut self, _: ToolCallId, _: ToolResult) -> Result<(), SessionError> {
        Err(SessionError::Unimplemented("tool results")) }   // seam present, LATER
    fn take_events(&mut self) -> EventReceiver;               // owned, taken once after connect
    fn capabilities(&self) -> Capabilities;
    fn transport_bytes(&self) -> Option<TransportBytes> { None }
    fn token_usage(&self) -> Option<TokenUsage> { None }
}
```
Plus `SessionConfig{model, system_instruction, voice, input_audio, output_audio,
enable_input/output_transcription, initial_context: Vec<HistoryTurn>, resumption_handle, tools}`,
`Capabilities{session_resumption, native_screen_input, async_tool_calls}` (all false in MVP),
`SessionEvent{AudioOutput{pcm}, Transcript{speaker,text,final_}, TurnEvent, SessionResumptionUpdate,
Error, Closed{reason}}`, `Speaker{User,Agent}`, `TurnEvent{TurnStarted,TurnComplete,Interrupted}`,
`CloseReason{Client,Server,Error}`. `EventReceiver = tokio::sync::mpsc::Receiver<SessionEvent>`.
The owned-receiver pattern (`take_events`) is what lets the manager `select!` over sends and events.

**`joi-core::session` (UI side)** — `UiEvent{State, Connection, Reachability, Transcript, Metrics,
Error, History}`, `AppState{Stopped,Connecting,Listening,Thinking,Speaking,Reconnecting,Error}`,
`ConnectionStatus`. These are the only events a frontend folds. **Plain Rust + `serde`, no `ts-rs`.**

**`joi-core::history`** — `trait HistoryStore: Send+Sync { append; load_within_budget(TokenBudget);
clear; meta }`. Types: `Role{User,Assistant,System}`, `HistoryTurn{role,text,ts_ms}` with
`token_estimate()` = chars/4 (min 1), `TokenBudget(u32)`, `HistoryMeta`. Impls: `InMemoryHistory`
(fallback/tests) and `SessionStore`. `SessionStore` surface beyond the trait: `create_new`, `load`,
`list(dir)->Vec<SessionSummary>` (newest-`last_updated` first), `current_summary`, `current`,
`rename(Option<String>)`, `start_new`, `switch_to(id)`. On-disk: `index.json` (`uuid→SessionMeta`) +
`<uuid>.jsonl` (append-only turns); auto-name from first user turn; skip corrupt lines; index atomic
write + rebuildable. Types: `SessionMeta{name,created_at,last_opened,last_updated}`, `SessionSummary`.

**`joi-core::manager`** — `SessionFactory{fn create(&self)->Box<dyn RealtimeSession>}` (+ blanket impl
for `Fn`); `Command{Start{resume},Stop{pause},SendText,SendAudio,SendFrame,SetMicMuted,QueryState,
Shutdown}`; `SessionManager::spawn(config, clock, history, factory, probe) -> SessionManagerHandle`;
`SessionManagerHandle` (Clone) with `subscribe()/subscribe_audio()/start/stop/send_text/send_audio/
send_frame/set_mic_muted/state/check_reachability`. The actor owns the live `RealtimeSession`, maps
`SessionEvent→UiEvent`, accumulates transcript deltas per speaker and appends finalized lines to
history, seeds `initial_context` from `load_within_budget` on start, drops mic audio when muted (and
calls `end_audio_stream` on the false→true edge), samples metrics every 1 s.

**`joi-core::{metrics,connectivity}`** — `MetricsSnapshot{up_kbps,down_kbps,up_tps,down_tps}` (+`ZERO`),
`ThroughputMeter`, `TokenUsage`, `TransportBytes`; `trait ConnectivityProbe{async fn probe()->
ProbeOutcome}`, `Reachability{Unknown,Checking,Online,Unauthorized,Offline}`, `spawn_monitor(...)`.

**`joi-core::media`** — `AudioFormat::{INPUT,OUTPUT}` constants, `VideoFrame`, `trait ScreenSource`,
`resample_linear`, `JitterBuffer{enqueue,pull,flush,buffered}`, `FrameAccumulator`, i16↔f32 + level
math. Pure DSP only — no devices.

**`joi-app::JoiApp`** (Seam A) — `JoiApp::build(Config, MediaMode) -> JoiApp`;
`MediaMode{LocalDevices,None}`; command surface `start/stop/send_text/send_audio/set_mic_muted/
start_screenshare/stop_screenshare/check_reachability/has_api_key/ui_config`; session commands
`list_sessions/current_session/new_session/resume_session`; streams `subscribe_events()->Option<..>`,
`subscribe_audio()->Option<..>`. Policy lives here: directory from `config.history.dir`, **fall back
to `InMemoryHistory` when no key / dir unusable**, build `MediaEngine` only in `LocalDevices`.

## 9. Implementation milestones (ordered for a fresh agent)

Each milestone ends green (`cargo build` + its tests + `clippy -D warnings`). Build bottom-up so every
layer is testable before the next depends on it.

### M0 — Workspace skeleton & tooling
- Workspace `Cargo.toml`: members (the 6 crates), `[workspace.package]`, `[workspace.dependencies]`
  (async-trait, thiserror, serde/serde_json, serde_norway/yaml, tokio, tracing, figment, directories,
  uuid, secrecy, reqwest[rustls]), `[workspace.lints.clippy]` (`all`+`pedantic` warn;
  `unwrap_used`/`expect_used`/`panic` deny, allowed in tests), `[profile.release] strip + lto="thin"`,
  `exclude=["vendor"]`, and the `[patch.crates-io]` block.
- Copy `old/vendor/` (both patched crates) verbatim.
- `scripts/check.sh` (see §10). Empty compiling crate stubs.
- **Done:** `cargo build --workspace` + `scripts/check.sh` pass on empty crates.

### M1 — `joi-core` domain (pure, no devices, no provider)
Order within: `clock` → `error` → `config` (§6) → `metrics` → `connectivity` → `media` contracts +
DSP (§7.1 pure parts: formats, resampler, jitter buffer, accumulator, conversions) → `session`
contracts + `UiEvent`/`AppState` → `history` (`InMemoryHistory` then `SessionStore`) → `manager`
actor + handle + `SessionFactory`.
- Unit tests with `TestClock` + `InMemoryHistory`: config precedence + redaction + validation;
  budget windowing; session auto-naming/switch/corruption/index-rebuild; resample/jitter/accumulator
  math; manager state machine driven by a hand-rolled fake `RealtimeSession`.
- **Done:** `cargo test -p joi-core` green; crate has zero device/provider/host deps.

### M2 — `joi-providers` (Mock, then Gemini)
- **Mock first** (`mock.rs`): a `RealtimeSession` that emits a deterministic transcript+turn loop and
  optional audio — unblocks manager/testkit/app tests with no network.
- **Gemini** (`gemini.rs`, feature-gated `gemini`): `GeminiAdapter` over vendored `adk-realtime` —
  `connect` (audio_only + server_vad + voice + transcription), `send_audio` (PCM16 LE 16 kHz),
  `send_video_frame` (JPEG blob), `send_text`+`create_response`, `end_audio_stream`, event pump +
  `EventMapper` (ServerEvent→SessionEvent: AudioDelta→AudioOutput; Input/Transcript/Text deltas with
  open/close line discipline; SpeechStarted→Interrupted; ResponseCreated/Done→TurnStarted/Complete;
  Error/Closed), history seeding (chronological, drop System/blank), `capabilities`=all-false,
  `transport_bytes`/`token_usage` passthrough, auth-vs-connect error classification, `init_crypto`.
  `GeminiProbe` (token-free `GET /v1beta/models`).
- `factory.rs`: `build_session_factory(&Config)`, `build_connectivity_probe(&Config)`,
  `FactoryError{MissingApiKey,ProviderDisabled}`.
- **Done:** `cargo test -p joi-providers` green; Gemini compiles behind feature.

### M3 — `joi-testkit`
- `sample_session_config()` fixture; `run_conformance<S: RealtimeSession>()` asserting the ordering
  guarantee (final agent `Transcript` precedes `TurnComplete`) and lifecycle; `ConformanceOutcome`.
- Run it against Mock.
- **Done:** conformance passes on Mock; `joi-core`/`app` tests can pull doubles from here.

### M4 — `joi-media` (faithful audio port — §7)
- `screen.rs` (xcap+jpeg) → `playback.rs` (resample+jitter+flush+render tap) → `capture.rs`
  (downmix+resample+APM 10 ms blocks+render-every-frame+backlog cap+frame accumulator) → `engine.rs`
  (`MediaEngine`, lifecycle, pumps, render-sink wiring, device-rate publish, lock order).
- Keep the debug per-second level meters (raw/pre-APM/post-APM/render dBFS) — they are how you confirm
  the AEC is healthy.
- **Done:** `cargo build -p joi-media`; a manual mic→APM→frame smoke check; the six §7.3 invariants
  visibly present with explanatory comments.

### M5 — `joi-app` (composition root, Seam A)
- `JoiApp::build` wiring per §8 (factory selection, history-dir policy + in-memory fallback,
  `MediaEngine` only in `LocalDevices`, spawn manager, optional probe). Command surface + streams.
- **Headless gate (replaces `joi-cli`):** integration test builds `JoiApp(MediaMode::None, Mock)`,
  subscribes events, runs `start → send_text → observe transcript/turn events → stop`, asserts the
  full loop with no devices.
- **Done:** `cargo test -p joi-app` green incl. the headless loop.

### M6 — `joi-tui` (frontend host)
- `main.rs`: terminal setup (crossterm raw + alt screen + block cursor), **panic hook restores the
  terminal**, `tokio::select!` over (input `EventStream`, `subscribe_events()`, 80 ms animation tick),
  render each iteration, graceful `app.stop` + restore on exit.
- `app.rs`: `AppModel` + **pure reducers** `on_action(Action)->Option<Command>` and
  `on_ui_event(UiEvent)->Option<Command>` (no I/O); `transcript.rs` fold (delta append / final close /
  error line / scrollback cap); `input.rs` UTF-8 line editor; `keys.rs` `Event→Action`; `theme.rs`
  (background/accent from config, status-dot animation); `ui.rs` layout (controls, transcript, status,
  prompt, footer with connection/reachability/uptime/throughput).
- `picker.rs` + `/resume`: render `list_sessions()`, select → `resume_session(id)` then `start(true)`;
  also `/exit|/quit|/q` and `new_session`. (New vs old TUI, which had no picker — required by `FR-20`.)
- Keys: F2 start/stop, F3 mute, F4 screenshare, F1 help, Enter submit, scroll keys, Ctrl+C/Q quit.
- **Done:** TUI runs against a live Gemini key end-to-end (voice in/out, transcript, mute, share,
  resume); reducers unit-tested without a terminal.

### M7 — Verification, docs, polish
- `scripts/check.sh` green. Update `doc/ARCH.md` to reflect the §3 deviations (no ts-rs/Seam B; drop
  `joi-cli`/`joi-server`; headless gate = app test). Refresh `config/joi.example.yaml`. Update
  `CLAUDE.md`/`README.md` status. Map acceptance criteria (SPEC §7) to tests/manual checks.

## 10. Verification

`scripts/check.sh` (mirrors what CI would run):
1. `cargo fmt --all --check`
2. `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`
3. `cargo test --workspace`
4. **Dependency assertions** (the layering is honest): `cargo tree -e no-dev` must show **no**
   `cpal`/`xcap`/`sonora`/`adk-realtime`/`reqwest`/`ratatui` under `joi-core`; **no** `ratatui`/
   `crossterm` outside `joi-tui`; **no** `tauri`/`webkit`/`ts-rs` anywhere.

End-to-end (manual, needs a Gemini key in `GEMINI_API_KEY`):
- **Voice loop / barge-in** (`FR-1,2,7`): speak, get audio reply, interrupt mid-reply → playback stops
  promptly. Watch the debug dBFS meters: post-APM stays live across many turns (AEC not eating the
  user — confirms §7.3#1).
- **Mute** (`FR-6`): F3 stops upstream audio immediately; state visible.
- **Screen share** (`FR-8,9,10`): F4 shares; model describes the screen; stop is immediate.
- **Sessions** (`FR-18-22`): converse, quit, relaunch, `/resume` lists newest-first, resume re-seeds
  context; first user message auto-names the session; corrupt a `.jsonl` line → load skips it.
- **No key**: `JoiApp` falls back to in-memory; session commands return clear errors, no panic.
- **Headless** (`FR-13`, invariant #8): `cargo test -p joi-app` headless loop passes.

## 11. Risks & gotchas (carry the hard-won lessons forward)

- **AEC render alignment** — the #1 audio failure mode. Always feed the far-end every frame (silence
  included); tap at playback output. (§7.3 #1,#2.)
- **sonora-aec3 shrink panic** — without the vendored patch, barge-in crashes capture. (§7.4.)
- **adk-realtime audio blob** — without the vendored patch, current Gemini models reject the audio and
  you get a silent, broken session. (§7.4.)
- **Device sample-rate mismatch** — never assume 16 kHz at the mic; resample from the device's
  *reported* rate; keep the debug rate-mismatch log.
- **cpal `Stream` is `!Send`** — capture and playback each stay on their own dedicated thread; cross
  thread boundaries only via channels/atomics.
- **Persistence must not block audio** — history append is on the manager task, off the audio path.
- **Empty `subscribe_audio` frame = flush** — keep that convention consistent between manager and
  playback pump.
```
