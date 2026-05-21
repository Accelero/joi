# Joi — MVP Implementation Plan

> Build plan for the MVP defined in [`SPEC.md`](./SPEC.md). Written so a fresh agent can execute
> it end-to-end on **Linux** with no extra context. SPEC.md is the source of truth for *what*;
> this document is the ordered *how*, with interfaces, tests, and exit criteria per milestone.
>
> **Locked decisions:** Tauri v2 shell · realtime layer on **adk-rust** (behind our own trait) ·
> frontend **React + TypeScript** (Vite + Tailwind + shadcn/ui + xterm.js). See SPEC §4.5, §9.
>
> **Revision note:** revised per [`PLAN_REVIEW.md`](./PLAN_REVIEW.md) — added an M0 Linux webview
> media spike (go/no-go gate), completed the WebKitGTK/GStreamer dependency story, made
> `SessionManager` an actor with an owned event stream, committed binary media to
> `tauri::ipc::Channel`, corrected the history token budget, made secret handling
> construction-safe, and split screen capture into two real pipelines. Contract changes are mirrored
> into SPEC §4 (events accessor), §6.2 (budget), and §11.2 (transport).
>
> **Golden rules:** every milestone ends **green** (`cargo fmt`, `cargo clippy -D warnings`,
> `cargo test`, frontend `lint`+`test`+`build`) and **demoable**. No milestone starts before the
> previous one is green. Do not pull post-MVP work (tools/bash/gate/memory/OpenAI-real) forward —
> only leave the seams (SPEC §10).

---

## 0. How to use this plan

1. Read SPEC.md §0–§11 once. Keep §4 (traits), §5 (lifecycle), §6 (history), §11 (IPC) open.
2. Follow milestones **M0 → M5 in order**. Each is: *Objective → Deliverables → Interfaces →
   Tasks → Tests → Exit criteria*.
3. Obey the architecture rules in §1 — they are what keep the codebase clean and the loop
   testable without a network or a GUI.
4. **Two spikes gate progress and must be done before the work they precede:** the **M0 Linux
   webview media spike** (does mic/playback even work in WebKitGTK?) and the **M2 adk-rust API
   spike** (what is the realtime API's actual shape?). Both are go/no-go gates, not warm-ups.
5. When adk-rust's actual API differs from sketches here (it is pre-1.0), adapt **inside
   `crates/joi-providers/src/gemini.rs` only** — never let it leak past our trait.

---

## 1. Architecture principles (non-negotiable)

These produce the clean interfaces / loose coupling the project requires.

- **Dependency inversion.** `joi-core` defines **traits** (the contracts) and pure logic. IO and
  vendor SDKs live *behind* those traits in outer crates. Core depends on nothing IO-heavy and
  has **zero** Tauri or provider-SDK dependencies. This is what makes the whole loop unit-testable.
- **One composition root.** Only `src-tauri/src/main.rs` constructs concrete implementations and
  wires them together. Everything else receives dependencies as trait objects / generics.
- **Ports & adapters (hexagonal).** Ports (traits) in core: `RealtimeSession`, `HistoryStore`,
  `SecretStore`, `ScreenSource`, `Clock`, plus the post-MVP `Tool`/`ExecEndpoint` seams. Adapters
  implement them (`joi-providers`, `src-tauri`). Tests use in-memory/mock adapters.
- **The `SessionManager` is an actor.** It **owns** the `Box<dyn RealtimeSession>`, the
  `HistoryStore`, and the `Config`, and serves commands (`start/stop/pause/resume/send_text/
  send_audio/...`) over a `tokio::mpsc` channel from a single owning task. This sidesteps the
  `&mut self` aliasing problem (you cannot hold an event stream *and* call `send_*` on the same
  `&mut` session) and gives a clean concurrency model. UI-facing events go out on a
  `tokio::broadcast`. **Provider event consumption uses an owned stream taken once at connect**
  (see §6), not a borrow off `&mut self`.
- **Media off the control plane.** Audio/screen frames stream over **`tauri::ipc::Channel`** binary
  channels (SPEC §11.2) and **never** pass through React state (SPEC §8.2) or any JSON path.
- **Errors:** libraries use `thiserror` with typed errors; the binary edge uses `anyhow`. **No
  `unwrap`/`expect`/`panic!` in library code** (lints enforce, CI-only — see §5). Fallible paths
  return `Result`.
- **Async:** `tokio` (multi-thread runtime). Cross-task comms via `tokio::sync` channels. No
  blocking IO on async tasks (`spawn_blocking` for fs/keychain).
- **Determinism in tests.** Inject `Clock` and randomness; no wall-clock sleeps in unit tests.
- **Feature flags** gate providers (`gemini`, `openai`, `mock`) so builds/tests stay lean.
- **Secrets are construction-safe.** The API key is a `Secret<String>` (the `secrecy` crate) whose
  `Debug`/`Display` are redacted — it *cannot* be formatted into a log by accident (§5, M-6 fix).
- **Public API discipline.** Each crate exposes a small, documented `lib.rs` surface; internal
  modules are `pub(crate)`. Every public item has a doc comment.

---

## 2. Repository & workspace structure

A Cargo **workspace** separates pure domain logic from IO and the GUI shell.

```
joi/
├─ Cargo.toml                      # [workspace] members + shared lints/deps
├─ rust-toolchain.toml             # pin stable toolchain
├─ .github/workflows/ci.yml        # Linux CI (fmt, clippy, test, frontend, tauri build)
├─ SPEC.md  PLAN.md  PLAN_REVIEW.md  README.md
│
├─ crates/
│  ├─ joi-core/                    # PURE domain — no Tauri, no provider SDKs
│  │  └─ src/
│  │     ├─ lib.rs                 # re-exports; crate docs
│  │     ├─ config.rs              # Config struct + layered loader (file + env) — §4
│  │     ├─ error.rs               # thiserror error enums
│  │     ├─ clock.rs               # Clock trait + SystemClock + TestClock
│  │     ├─ session/
│  │     │  ├─ mod.rs              # RealtimeSession trait, SessionConfig, Capabilities — §6
│  │     │  └─ event.rs            # SessionEvent, TurnEvent, Speaker, ids, EventReceiver
│  │     ├─ manager.rs             # SessionManager actor (lifecycle FSM + fan-out) — SPEC §5
│  │     ├─ history/
│  │     │  ├─ mod.rs              # HistoryStore trait, HistoryTurn, TokenBudget — SPEC §6
│  │     │  ├─ memory.rs           # InMemoryHistory (tests)
│  │     │  └─ file.rs             # JsonlHistory (prod, append + prune)
│  │     ├─ media.rs               # AudioFormat, VideoFrame, framing helpers (pure)
│  │     ├─ secrets.rs             # SecretStore trait (returns Secret<String>) + in-memory impl
│  │     ├─ capture.rs             # ScreenSource trait, CaptureSource, CaptureQuality
│  │     └─ tools/                 # [POST] seam only: Tool trait, Registry (unused in MVP)
│  │
│  ├─ joi-providers/               # RealtimeSession implementations
│  │  ├─ Cargo.toml                # features: gemini, openai, mock
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ mock.rs                # MockSession: scripted events (tests + M1)
│  │     ├─ gemini.rs              # GeminiAdapter wrapping adk-rust — SPEC §4.3/§4.5
│  │     └─ openai.rs              # OpenAI stub: compiles, returns Err — SPEC §4.4
│  │
│  └─ joi-testkit/                 # shared test utilities + adapter conformance suite
│     └─ src/lib.rs                # run_conformance(session), fixtures, builders
│
├─ src-tauri/                      # Tauri v2 binary `joi` — composition root + IO adapters
│  ├─ Cargo.toml
│  ├─ build.rs
│  ├─ tauri.conf.json
│  └─ src/
│     ├─ main.rs                   # parse argv flags, build Config, wire SessionManager, run Tauri
│     ├─ webview.rs                # WRY media settings + permissions-request handler — M0/B1
│     ├─ state.rs                  # AppState (mpsc handle to SessionManager, Config)
│     ├─ commands.rs               # #[tauri::command] handlers → SessionManager (SPEC §11.1)
│     ├─ events.rs                 # broadcast → webview emit (SPEC §11.3)
│     ├─ media.rs                  # tauri::ipc::Channel binary mic-in / audio-out (SPEC §11.2)
│     ├─ secrets_keychain.rs       # SecretStore impl via OS keychain (keyring/plugin)
│     └─ capture_native.rs         # ScreenSource impl via scap/xcap (M4 native path)
│
├─ src/                            # React frontend (Vite) — see SPEC §2.2
│  ├─ main.tsx  App.tsx
│  ├─ ipc.ts                       # typed invoke/listen + Channel wrappers
│  ├─ state.ts                     # UI store (Zustand) — control state only
│  ├─ media/{mic.ts,playback.ts,screen.ts,dsp.ts,worklets/*.js}   # dsp.ts = pure, testable
│  └─ ui/{Terminal.tsx,Controls.tsx,Settings.tsx,components/}
│
├─ config/joi.example.toml         # documented example config (§4)
├─ package.json  vite.config.ts  tailwind.config.ts  tsconfig.json
└─ scripts/                        # dev helpers (setup-linux.sh, check.sh, media-spike/)
```

**Dependency direction (must hold):** `joi-core` ← `joi-providers` ← `src-tauri` → frontend.
Core never imports outward. `joi-testkit` depends on core (+ providers’ `mock` feature) only.

---

## 3. Toolchain & Linux prerequisites

Target: **Linux** (x86_64). Document in `README.md` + `scripts/setup-linux.sh`.

- **Rust** stable (pin in `rust-toolchain.toml`), components `rustfmt`, `clippy`.
- **Node** ≥ 20 + **pnpm**.
- **Tauri v2 CLI:** `cargo install tauri-cli --version '^2'`.
- **Tauri/WebKitGTK build libs:** `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `librsvg2-dev`,
  `libayatana-appindicator3-dev`, `libssl-dev`, `build-essential`, `pkg-config`, `curl`, `wget`,
  `file`.
- **GStreamer runtime — REQUIRED for mic/screen capture in WebKitGTK** (this is the part most
  setups miss): `libgstreamer1.0-dev`, `gstreamer1.0-plugins-base`, `gstreamer1.0-plugins-good`,
  **`gstreamer1.0-plugins-bad`** (WebRTC/media-stream codecs), `gstreamer1.0-libav`, and
  **`gstreamer1.0-pipewire`** (PipeWire portal screencast for `getDisplayMedia`). Without these,
  `getUserMedia`/`getDisplayMedia` fail or return silence.
- **Screen capture backend:** `xdg-desktop-portal` + a backend (`xdg-desktop-portal-gtk` and/or
  `-wlr`/`-gnome`/`-kde`) for the webview `getDisplayMedia` path on Wayland.
- **Display-server reality (verified, see PLAN_REVIEW B1):** WebKitGTK media capture under Tauri is
  fragile on **Wayland** (GBM buffer errors); the known-good path forces the **X11** GDK backend.
  Provide a launch fallback documented in `setup-linux.sh` and README:
  `GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 ./joi`. The M0 spike (M0) decides the default
  per environment.
- Verify early: `cargo tauri info` reports a healthy Linux setup; the **M0 media spike** must pass
  before building anything else.

---

## 4. Configuration system (file + env) — implement in M0

A single layered `Config`, loaded once at startup, then immutable (runtime UI changes write back to
the file and re-load). **Secrets are never in config** (SPEC SEC-5) — see §4.4.

### 4.1 Precedence (lowest → highest)
1. **Built-in defaults** (`Config::default()`).
2. **Config file** (TOML) — §4.3.
3. **Environment variables** prefixed `JOI_` (nested via `__`).
4. **CLI flags** — parsed from `std::env::args()` in `main.rs` *before* the Tauri builder runs
   (only `--config <path>` and `--log <level>`; keep this tier tiny). If you prefer, drop the tier
   for MVP — but if kept, this is exactly where it is read.

Use **`figment`** (Toml + Env + Serialized providers) to merge, then `extract::<Config>()`.

### 4.2 `Config` shape (`joi-core/src/config.rs`)
```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub provider: ProviderCfg,   // name, model, voice, system_instruction, transcription flags
    pub audio:    AudioCfg,      // in/out sample rate, frame_ms, devices
    pub screen:   ScreenCfg,     // enabled, capture_path(auto|webview|native), fps, max_width, quality
    pub history:  HistoryCfg,    // dir, token_budget
    pub terminal: TerminalCfg,   // theme, font, scrollback
    pub logging:  LoggingCfg,    // level, file
}
impl Default for Config { /* sane defaults; paths via `directories` XDG */ }
impl Config {
    pub fn load(cli_path: Option<&Path>) -> Result<Self, ConfigError>; // figment merge + validate
    pub fn validate(&self) -> Result<(), ConfigError>;                 // ranges, enums, paths
}
```
- **Paths — single source of truth:** `joi-core` resolves XDG paths via the `directories` crate
  (config `~/.config/joi/joi.toml`, data/history `~/.local/share/joi/`, logs
  `~/.local/state/joi/`). `src-tauri` must **pass these resolved paths in**, not re-derive them via
  Tauri's `app_*_dir()` API, to avoid divergent locations (m-1).

### 4.3 Example file (`config/joi.example.toml`)
```toml
[provider]
name = "gemini"
model = "gemini-live-2.5-flash-native-audio"
voice = "Aoede"
system_instruction = "You are Joi, a concise local voice companion."
input_transcription  = true   # FR-3: needed to render user transcript
output_transcription = true   # FR-3: needed to render agent transcript

[audio]
input_sample_rate  = 16000   # to provider
output_sample_rate = 24000   # from provider
frame_ms = 20
input_device  = "default"
output_device = "default"

[screen]
enabled = false
capture_path = "auto"   # auto | webview | native  (auto resolves via M0 spike result)
fps = 1.0
max_width = 1920
quality = 80            # 1-100

[history]
# dir defaults to XDG data dir.
# token_budget bounds persisted history (SPEC §6.2). Unit = approx tokens (chars/4 heuristic).
# IMPORTANT: this is the *Live session* input budget, which is much smaller than the 1M text-model
# context window — do NOT copy the 1M number. Default sized to the Live model's real input limit
# with headroom; confirm against the model card during M2/M3 and set accordingly.
token_budget = 32000

[terminal]
theme = "joi-dark"
font = "JetBrains Mono"
scrollback = 5000

[logging]
level = "info"          # error|warn|info|debug|trace ; RUST_LOG overrides
```

### 4.4 Environment variables & secrets
| Var | Effect |
|---|---|
| `JOI_CONFIG` | path to config file (else XDG default) |
| `JOI_PROVIDER__MODEL`, `JOI_AUDIO__FRAME_MS`, … | override any field (nested `__`) |
| `RUST_LOG` / `JOI_LOGGING__LEVEL` | log filter (RUST_LOG wins for `tracing-subscriber`) |
| `GEMINI_API_KEY` | **dev-only** secret fallback (read at runtime, never persisted) |
| `GDK_BACKEND`, `WEBKIT_DISABLE_COMPOSITING_MODE` | Linux webview fallback (§3) |

**Secret handling:** primary path = OS keychain via `SecretStore`, returning a `Secret<String>`
(SPEC SEC-5). For developer convenience only, if the keychain has no key, fall back to
`GEMINI_API_KEY` env (logged as a dev-mode warning, never written to disk/config). The key is
**never** stored in `Config`.

---

## 5. Cross-cutting conventions (set up in M0)

- **Logging/telemetry:** `tracing` + `tracing-subscriber` (env-filter) → stderr + configured log
  file; structured spans around session lifecycle, connection, turns.
- **Secret redaction — construction-safe (M-6 fix):** the API key is a `Secret<String>` (`secrecy`)
  whose `Debug`/`Display` redact, so it cannot be formatted into a log by construction. **In
  addition**, a scan test feeds a known key through a full mock session and asserts it appears in no
  emitted event, captured log, or history file. Belt and braces; the newtype is the primary control.
- **Lints:** `clippy::all` + selected `pedantic`; `unwrap_used`/`expect_used` denied in library
  crates. `deny(warnings)` is applied **CI-only** via `RUSTFLAGS="-D warnings"` (not a source
  attribute, which would break on future compiler lint bumps) (m-6).
- **Formatting:** `rustfmt.toml`, `cargo fmt --check` in CI; Prettier + ESLint + `tsc --noEmit`.
- **Testing libs:** Rust `tokio::test`, `assert_matches`, `pretty_assertions`, `proptest` (history
  bounds, framing), `wiremock`/recorded fixtures for the Gemini adapter (no live net in CI).
- **Frontend testing — be honest about jsdom (M-4 fix):** Vitest can only test **pure helpers**
  (`media/dsp.ts`: downsample math, framing, jitter-buffer queue logic, terminal-write throttle).
  `AudioWorklet`, `getUserMedia`, `getDisplayMedia`, and the xterm WebGL renderer **do not exist in
  jsdom** — the worklet↔IPC↔playback integration is verified by the **M0 media spike** and each
  milestone's manual exit demo (and optional `tauri-driver` e2e). Extract DSP into `dsp.ts` so the
  testable part is isolated from the un-testable Web-Audio glue.
- **IPC types parity:** define payloads once in `events.rs`/`commands.rs` (serde) and mirror in
  `src/ipc.ts`. A parity test (de)serializes representative payloads to catch drift; keep field
  names consistent across the boundary (e.g. transcript `final` — note the Rust raw-keyword dodge
  `final_` must serde-rename to `final`) (m-4). `ts-rs` generation is a nice-to-have, not required.

---

## 6. Core contracts (define in M1, stub in M0)

Full detail in SPEC §4–§6. The trait surface, with the **owned-event-stream** and **actor**
decisions folded in:

```rust
// session/mod.rs
#[async_trait] pub trait RealtimeSession: Send {
    async fn connect(&mut self, cfg: SessionConfig) -> Result<(), SessionError>;
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SessionError>;
    async fn send_video_frame(&mut self, f: &VideoFrame) -> Result<(), SessionError>;
    async fn send_text(&mut self, text: &str) -> Result<(), SessionError>;
    /// Take the event stream ONCE, after connect. Returns an OWNED receiver so the manager can
    /// read events while `send_*` runs (a borrowed `&mut self` stream would alias the session).
    fn take_events(&mut self) -> EventReceiver;   // e.g. tokio::mpsc::Receiver<SessionEvent>
    async fn close(&mut self) -> Result<(), SessionError>;
    fn capabilities(&self) -> Capabilities;
}
```
> **Divergence from SPEC §4 (intentional, mirrored into SPEC):** SPEC §4 sketched
> `fn events(&mut self) -> EventStream` (a borrow). That cannot coexist with `send_*(&mut self)`.
> We take an **owned** receiver once. SPEC §4 is updated to match.

```rust
// history/mod.rs
#[async_trait] pub trait HistoryStore: Send + Sync {
    async fn append(&self, turn: HistoryTurn) -> Result<(), HistoryError>;
    /// Newest-first turns whose cumulative token estimate fits `budget`. Guaranteed re-seedable.
    async fn load_within_budget(&self, budget: TokenBudget) -> Result<Vec<HistoryTurn>, HistoryError>;
    async fn clear(&self) -> Result<(), HistoryError>;
    async fn meta(&self) -> Result<HistoryMeta, HistoryError>;
}

// secrets.rs
#[async_trait] pub trait SecretStore: Send + Sync {
    async fn get_api_key(&self) -> Result<Option<Secret<String>>, SecretError>;
    async fn set_api_key(&self, key: Secret<String>) -> Result<(), SecretError>;
}

// capture.rs
#[async_trait] pub trait ScreenSource: Send + Sync {
    async fn list(&self) -> Result<Vec<SourceInfo>, CaptureError>;        // displays (windows = [POST])
    async fn start(&self, sel: CaptureSource, q: CaptureQuality) -> Result<FrameStream, CaptureError>;
    async fn stop(&self) -> Result<(), CaptureError>;
}
```

**`SessionManager` is an actor (manager.rs).** A single owning task holds
`Box<dyn RealtimeSession>` + `HistoryStore` + `Config`, takes the session's `EventReceiver` after
connect, and runs a `select!` loop over (a) inbound commands on an `mpsc` (`start/stop/pause/
resume/send_text/send_audio/send_frame/...`) and (b) the provider's `SessionEvent` stream — mapping
events to `UiEvent` on a `broadcast` and appending finalized transcripts to history. Callers
(Tauri commands) hold only a cheap `mpsc::Sender` handle, so no shared `&mut` to the session exists
anywhere. **This resolves the borrow problem and is the documented concurrency model.**

---

## 7. Milestones

> Each: **Objective · Deliverables · Key interfaces · Tasks · Tests · Exit criteria.** Maps to
> SPEC §17. Keep PRs small.

### M0 — Foundation, scaffolding & the Linux media spike
**Objective:** A Tauri v2 + React app that builds and runs on Linux, with config, logging, CI — and
**proof that mic capture and audio playback actually work inside the WebKitGTK webview.** This spike
is the project's first go/no-go gate (PLAN_REVIEW B1/B2).

**Deliverables:** workspace + crates skeleton (§2); `Config` loader (§4); `tracing` + `secrecy`
redaction (§5); Tauri shell launching React; **WRY media configuration + `permissions-request`
handler** (`webview.rs`); the **media spike**; `ping` command; CI green; `setup-linux.sh`; README.

**Key interfaces:** `Config::load`, `SecretStore` (+ in-memory + keychain), `Clock`.

**Tasks:**
1. Init workspace, crates, `rust-toolchain.toml`, lints, `rustfmt.toml`.
2. Scaffold Vite + React + TS + Tailwind + shadcn/ui; Tauri v2 integration; confirm
   `cargo tauri dev` opens a window on Linux.
3. **WebKitGTK media enablement (`webview.rs`) — do this before the spike:** on the WRY/WebView
   builder enable media settings (`set_enable_media_stream`/`set_enable_webrtc`/`set_enable_media`
   per the current WRY API) and register a **`permissions-request` handler that grants
   audio/video/display capture** — without it every `getUserMedia`/`getDisplayMedia` is silently
   denied.
4. **Media spike (`scripts/media-spike/` + a temporary route in the app):** in the real Tauri
   window on Linux, prove `getUserMedia({audio})` yields non-silent samples and Web Audio plays a
   tone; measure round-trip frame latency over `tauri::ipc::Channel`. Record the working display
   server (X11 vs Wayland) and any env flags needed; write findings into README + set the
   `screen.capture_path=auto` resolution default accordingly. **Gate:** if neither X11 nor Wayland
   yields capture, stop and escalate before further work.
5. Implement `Config` (defaults + TOML + `JOI_` env via figment) + `validate` + `joi.example.toml`;
   wire `--config`/`--log` argv parsing in `main.rs` (§4.1).
6. `tracing` init from config/`RUST_LOG`; `Secret<String>` newtype for the key; redaction verified.
7. Keychain `SecretStore` (+ in-memory for tests); `set_api_key`/`has_api_key` commands.
8. `ping`/`get_config` command + typed `ipc.ts`; render config + a "key present?" badge.
9. CI workflow: install Linux + **GStreamer** deps (§3), `cargo fmt`, `clippy` (RUSTFLAGS -D
   warnings), `cargo test`, frontend lint/test/build.

**Tests:**
- `config`: defaults; file > defaults; `JOI_*` env > file; invalid value → `ConfigError`; XDG paths.
- `secrets`: in-memory set/get; `Secret<String>` `Debug`/`Display` redact; redaction scan on a
  string containing the key.
- IPC smoke: `ping`; `get_config` parity (serde round-trip Rust↔TS payloads).
- **Media spike acceptance (manual, recorded in README):** mic samples captured + tone played in the
  Tauri window; measured Channel latency noted.

**Exit:** `cargo tauri dev` shows a window; **the media spike passes on Linux with a documented
display-server config**; all automated tests + CI green.

---

### M1 — Core loop on a mock adapter
**Objective:** With capture proven (M0), build the abstraction and prove the full loop with **zero
network**: mic → PCM → `Channel` → `MockSession`; scripted audio out → playback; transcript →
terminal; live state. Now genuinely low-risk.

**Deliverables:** `RealtimeSession` trait + `SessionEvent` + `EventReceiver` (§6); `MockSession`;
**`SessionManager` actor**; `tauri::ipc::Channel` binary media wiring; `Terminal.tsx` (xterm.js) +
`Controls.tsx`; extracted `media/dsp.ts`; `joi-testkit` conformance suite skeleton.

**Key interfaces:** §6 traits; `UiEvent` enum; `tauri::ipc::Channel` for mic-in + audio-out.

**Tasks:**
1. Define session traits/types + `Capabilities` + errors in `joi-core`; `take_events` owned receiver.
2. `MockSession`: connect; on `send_audio`/`send_text` emit scripted `Transcript`/`AudioOutput`/
   `TurnEvent` per a deterministic, `Clock`-driven script.
3. `SessionManager` **actor**: owning task + `select!` over command mpsc and event stream; map →
   `UiEvent` broadcast; append finalized transcripts to `InMemoryHistory`.
4. Frontend `mic.ts` (getUserMedia → AudioWorklet → 16k mono PCM, 20 ms) → `Channel`; mute gates at
   worklet. `playback.ts` (24k PCM → Web Audio jitter buffer worklet, flush on interrupt). Pure math
   in `dsp.ts`.
5. `Terminal.tsx`: mount xterm.js; write transcript with ANSI speaker colors; throttle partial
   commits (SPEC §8.2). `Controls.tsx`: start/stop, mute, state indicator.
6. `joi-testkit::run_conformance(session)`: connect → take_events → send text → assert ordered
   events → close.

**Tests:**
- Core: `SessionManager` start→running→stop; event ordering (transcript-before-turn-end); history
  append on final transcript; command/event interleave (sends while events stream — proves the actor
  model compiles and works).
- `MockSession` drives a scripted turn; conformance suite green against it.
- Media (core): 20 ms framing; `AudioFormat` conversions.
- Frontend (Vitest, pure only): downsample-to-16k; jitter-buffer enqueue/flush; throttle logic;
  `ipc.ts`/Channel wrappers (mocked).

**Exit:** Type/click input → scripted spoken + terminal response locally, no network; sends and event
reception run concurrently without borrow errors; conformance suite green.

---

### M2 — Gemini voice (adk-rust)
**Objective:** Real full-duplex S2S with Gemini Live via `GeminiAdapter`, **after** pinning down
adk-rust's actual API. Turn-taking, barge-in, BYOK direct connect.

**Precondition spike (PLAN_REVIEW B3 — gate, write it down):** read the `adk-realtime` crate API and
its `examples/gemini_audio`; **record the real connect/send/recv/interrupt/session-resumption shape**
in a short `crates/joi-providers/NOTES-adk.md`. Confirm whether events arrive as a stream/channel/
callbacks and how its audio I/O loop is owned. **Decide the gemini-rs fallback trigger before
coding.** If adk-rust owns its own loop in a way that fights `take_events`, adapt the adapter (e.g.
bridge its callbacks/loop into our `mpsc`) inside `gemini.rs` only.

**Deliverables:** `GeminiAdapter` (feature `gemini`); settings UI for API key; connection status;
barge-in handling + a measured latency metric.

**Tasks:**
1. Per the spike, construct adk-rust's realtime session for `gemini-live-2.5-flash-native-audio`,
   key from `SecretStore` (dev: `GEMINI_API_KEY`).
2. Bridge adk-rust events → `SessionEvent` (into the owned `EventReceiver`); trait `send_*` ↔
   adk-rust sends; map interruption + resumption handles.
3. **Wire transcription flags** (`provider.input_transcription`/`output_transcription`) so FR-3
   transcripts actually arrive (m-2).
4. `SessionManager` selects adapter by `config.provider.name`.
5. Barge-in: on `Interrupted`, flush playback immediately (frontend) + update state; **emit a
   dev-overlay metric for detected-speech→playback-halt and assert it's < 300 ms** in a manual/
   instrumented check (FR-2, gap 5).
6. Settings UI: paste key → keychain; connection state surfaced; auth/network errors shown.

**Tests:**
- Adapter unit with **recorded fixtures / fake WS** (no live net in CI): provider frames →
  `SessionEvent`; capability flags; error frames → `SessionError`.
- Barge-in handler: `Interrupted` → playback flush (Vitest + core state).
- Latency: barge-in halt budget logged/asserted; playback jitter ≤ 80 ms measured (dev overlay).
- `#[ignore]` live smoke behind `JOI_LIVE_TESTS=1` + real key: connect, one turn.
- Conformance suite runs against `GeminiAdapter` (fixture-backed).

**Exit:** Real spoken conversation works; turn-taking + barge-in feel right and meet the latency
budgets; `NOTES-adk.md` exists; CI green (live test ignored).

---

### M3 — Lifecycle + persistence
**Objective:** start/stop/**pause/resume** for cost control + **bounded** on-disk history that
restores context across a system restart (SPEC §5, §6).

**Deliverables:** lifecycle FSM in the `SessionManager` actor; `JsonlHistory` (append + prune to
token budget); restore-to-context on launch/resume; transient-reconnect via resumption handle with
context-restore fallback; history meta to UI.

**Key interfaces:** `HistoryStore` (file impl); `SessionConfig.initial_context`,
`SessionConfig.resumption_handle`; `UiEvent::History`, `UiEvent::Connection`.

**Tasks:**
1. FSM (Stopped ⇄ Connecting ⇄ Running, Running → Reconnecting). `stop`/`pause` = `session.close()`;
   **assert zero open connections in `Stopped`**.
2. `JsonlHistory`: append; `load_within_budget` newest-first within `token_budget`; prune oldest
   beyond budget; meta. Token estimate = chars/4 behind a swappable function; **budget = the Live
   model's real input limit** (not the 1M text window) — confirm and set in config (M-2).
3. **Resume composition (gap 6):** seed a new session with the configured `system_instruction` **plus**
   the restored turns; specify dedupe rules so a persisted system turn doesn't double the configured
   instruction. Document the exact composition order.
4. Transient reconnect: on drop, retry last `resumption_handle` (mockable); on expiry, do a
   context-restoring restart.
5. Persist `resumption_handle` opportunistically; writes must not block the audio path (queue /
   `spawn_blocking`).

**Tests:**
- FSM: every transition incl. pause→resume; `Stopped` holds no session (mock asserts `close`).
- History (`proptest`): append/prune never exceeds budget; **re-seeded `initial_context` is
  guaranteed to fit `token_budget`** (not just that the file is bounded) (M-2); restore round-trip;
  empty/corrupt file → fresh start, no panic.
- Resume composition: system instruction + restored turns assembled per the documented rule.
- Reconnect: simulated drop → resume-with-handle; expired handle → context-restore path.

**Exit:** Pause (no connection/cost) then resume continues coherently; after process kill + relaunch,
prior context restored and provably fits the budget. CI green.

---

### M4 — Screen capture (two pipelines)
**Objective:** Stream a chosen screen as low-fps video input; choose source; start/stop; quality
settable. **This is two real pipelines, not one** (PLAN_REVIEW M-5): a webview path and a native
path. Pick the Linux default from the M0 spike result.

**Deliverables:**
- **(a) Webview capture path:** `screen.ts` — `getDisplayMedia` + source pick → ~configured-fps
  encoded frames → `Channel` → `send_video_frame`.
- **(b) Native capture path:** `capture_native.rs` — scap/xcap enumerate + capture on Linux; frames
  emitted backend-side (never cross IPC).
- Shared: capability probe + path resolution (`auto`), quality config, sharing indicator, backpressure
  policy.

**Key interfaces:** `ScreenSource`, `CaptureSource::{Display(id)}` (`Window` = `[POST]`),
`CaptureQuality { fps, max_width, quality }`; commands `list_screen_sources`, `start_screenshare`,
`stop_screenshare`, `screen_capability`.

**Tasks:**
1. Webview path: enumerate via `getDisplayMedia`; sample to fps; encode JPEG/WebP; send bytes.
2. Native path: scap/xcap enumerate + capture; emit frames backend-side.
3. Capability probe → resolve `auto` to webview or native **based on the M0 spike**; if Wayland
   capture was unreliable in M0, native may be the Linux **default**, not the fallback.
4. Quality clamping to ceilings; default native-resolution/max-fps policy (SPEC §7.3, FR-11).
5. Start/stop revokes in-flight frames immediately; sharing indicator reflects active path.
6. **Backpressure (gap 8):** define drop policy when encode/IPC can't keep up (drop newest, keep at
   most one queued frame); ensure stop drains/cancels so no frame escapes after stop (FR-10).

**Tests:**
- Path-resolution: capable→webview / incapable→native / explicit override respected.
- **Each pipeline tested separately:** webview frame production (mocked `getDisplayMedia`) and native
  capture mapping (mock `ScreenSource`).
- Quality clamp: out-of-range fps/width/quality clamped.
- Stop revokes: no frames emitted after `stop` (both pipelines); backpressure drops per policy.
- Native capture smoke gated on a Linux display (`#[ignore]` in CI).

**Exit:** Pick a screen, start/stop sharing on the chosen Linux default path; model discusses
on-screen content (manual); quality adjustable; no frame leaks after stop. CI green.

---

### M5 — Hardening, security checks, packaging
**Objective:** Production-readiness for the Linux MVP.

**Deliverables:** robust error/reconnect UX; `panic_stop`; logging/persistence hygiene; OpenAI stub
honesty; **Linux bundle via `cargo tauri build`**; README.

**Tasks:**
1. Error UX: auth failure → settings; connection loss → `reconnecting` → restore/fallback; corrupt
   history → fresh + warn; getDisplayMedia denied → native or clear disable.
2. `panic_stop` command: close session + mute mic + stop capture in one action; reflect in state.
3. Security: `Secret<String>` everywhere for the key; scan test asserts it never appears in
   logs/events/history (SEC-5/7/8).
4. `OpenAIAdapter` stub returns graceful `Err`/`unimplemented`, `async_tool_calls=false`; workspace
   compiles with it and **the conformance suite runs against it** (proving no Gemini-ism leaked) —
   SPEC §16.
5. **`cargo tauri build` produces a Linux bundle (AppImage/.deb) and this is a CI gate** (no "or at
   least `cargo build`" escape hatch) (m-5); document run + config locations + the X11 fallback.

**Tests:**
- Error-path tests for each scenario (mock-driven).
- SEC scan: key absent from emitted events, captured logs, and history file.
- `panic_stop`: session closed + mic muted + capture stopped.
- CI builds the bundle on Linux.

**Exit:** All SPEC §18 acceptance criteria pass on Linux; CI green incl. the bundle build; runnable
artifact exists.

---

## 8. Test strategy & CI (summary)

- **Unit (core):** pure logic — config, FSM, history bounds, framing, redaction. Fast, no IO.
- **Adapter conformance (`joi-testkit`):** one suite run against `MockSession`, `GeminiAdapter`
  (fixtures), and the `OpenAIAdapter` stub — proves provider-agnosticism (SPEC §16) and de-risks
  adk-rust.
- **Integration:** `SessionManager` actor + `MockSession` + `InMemoryHistory` exercise the full loop
  (incl. concurrent send/receive) with no network/GUI.
- **Frontend (Vitest):** **pure helpers only** (`dsp.ts`: resample, framing, jitter-buffer queue,
  throttle) — jsdom cannot run AudioWorklet/getUserMedia/getDisplayMedia/WebGL (M-4). The media
  integration is covered by the M0 spike + per-milestone manual exit demos + optional `tauri-driver`.
- **Live/e2e (non-gating):** `#[ignore]` live Gemini smoke (env-flag + key); optional `tauri-driver`.
- **CI (`.github/workflows/ci.yml`, Linux):** install system + **GStreamer** deps → `cargo fmt
  --check` → `cargo clippy --workspace --all-targets` with `RUSTFLAGS="-D warnings"` →
  `cargo test --workspace` → frontend `lint`+`test`+`build` → (M5) **`cargo tauri build`**. Live
  tests excluded.

---

## 9. Build & run (Linux)

```bash
# one-time
./scripts/setup-linux.sh           # installs webkit2gtk + GStreamer plugins + rustup comps + tauri-cli
pnpm install

# develop
cargo tauri dev                    # if mic/screen capture misbehaves on Wayland, use the fallback:
GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 cargo tauri dev

# quality gate (mirror CI before every commit)
cargo fmt --all
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
cargo test --workspace
pnpm lint && pnpm test && pnpm build

# release bundle
cargo tauri build                  # AppImage/.deb in src-tauri/target/release/bundle
```

Config: copy `config/joi.example.toml` → `~/.config/joi/joi.toml`. API key entered in-app
(keychain) or `GEMINI_API_KEY` for dev.

---

## 10. Definition of Done (MVP)

MVP is done when, on Linux, **all SPEC §18 acceptance criteria pass** and:
- workspace builds + all gates green in CI; **`cargo tauri build` produces a runnable bundle**;
- the conformance suite passes against mock + Gemini(fixtures) + OpenAI-stub (no Gemini-ism leaked);
- config works via file **and** env; secrets only in keychain via `Secret<String>` (dev env fallback
  documented);
- the M0 media spike is documented (working display-server config) and capture works in-app;
- latency budgets met (barge-in < 300 ms, playback ≤ 80 ms) per dev-overlay metrics;
- no tool/shell code path is reachable by the model, yet the registry/dispatch + `ScreenSource`/
  `SecretStore` seams exist for post-MVP (SPEC §10).

---

## 11. Risks & validation spikes (do early)

- **Linux webview media capture (PLAN_REVIEW B1/B2) — highest risk, de-risked FIRST in M0:** WRY
  media settings + `permissions-request` handler + GStreamer plugins are mandatory; Wayland is
  fragile, X11 is the known-good fallback. The M0 spike is a go/no-go gate.
- **adk-rust realtime API shape (PLAN_REVIEW B3, SPEC §4.5):** unknown until read; the **M2
  precondition spike** + `NOTES-adk.md` pins it down; gemini-rs is the contained fallback (one file).
- **Owned-event-stream / actor model (PLAN_REVIEW M-1):** proven to compile and work in M1 (concurrent
  send/receive test) before the real adapter lands.
- **Audio latency/jitter over `tauri::ipc::Channel`:** measured in the M0 spike and M2; keep jitter
  buffer ≤ 80 ms; media off React state (SPEC §8.2).
- **Live token budget vs context window (PLAN_REVIEW M-2):** use the Live model's real input limit,
  not 1M; test re-seed fits.
- **Screen-capture is two pipelines (PLAN_REVIEW M-5):** webview vs native each built/tested; Linux
  default chosen from the M0 spike, not assumed.
- **Frame backpressure (gap 8):** explicit drop policy so stop never leaks frames.
```
