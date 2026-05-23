# RUNBOOK — Modularize: JOI engine ↔ Tauri backend ↔ Web frontend

> **For the implementing agent.** This is a step-by-step runbook. You do **not** need prior context.
> Work the stages **in order**. After every stage, `./scripts/check.sh` must pass before continuing.
> The refactor is **behavior-preserving** — the desktop app must work identically after each stage.
> Make no change that isn't called for here; if reality differs from this doc, stop and report.

---

## 0. Orientation

**Goal:** three layers joined by two explicit interfaces, each layer compiling on its own.

```
JOI ENGINE (no Tauri)      crates/joi-core, joi-providers, joi-media, joi-app(NEW), joi-cli(NEW)
        │  Seam A: JoiApp Rust API (crates/joi-app)
TAURI BACKEND              src-tauri  (thin adapter)
        │  Seam B: JSON IPC — commands + "ui_event" (already exists; SPEC §11)
WEB FRONTEND               src/  (React; no Rust dep, only the TS contract in src/ipc.ts)
```

**Dependency rule (must stay acyclic, never reach "up"):**
`joi-core` → (depended on by) `joi-providers`, `joi-media` → `joi-app` → `src-tauri`, `joi-cli`.
The web frontend depends on **no Rust crate**.

**How to verify (run from repo root):**
- Everything: `./scripts/check.sh` (fmt + clippy `-D warnings` + `cargo test --workspace` + `bun run typecheck`/`test`/`build`).
- One crate: `cargo build -p joi-app` / `-p joi` / `-p joi-cli`.
- Frontend only: `bun run build`.
- After Rust edits, run `cargo fmt` (check.sh enforces formatting).
- **Do not run the GUI** to verify logic; the checks above are authoritative. (The app is launched
  with `bun run tauri dev`; the dev server is fixed to port 1420.)

**Workspace file:** `Cargo.toml` (root) — `members = [...]`, `exclude = ["vendor"]`. Shared deps live
in `[workspace.dependencies]`; crates opt in with `dep.workspace = true`.

---

## 1. Current-state map (where things are *today*)

| Symbol | File | Notes |
|---|---|---|
| `Config` (+ `LiveApiCfg`, `GeminiCfg`, `AudioCfg`, `ScreenCfg`, `HistoryCfg`, `TerminalCfg`, `LoggingCfg`, `ApiKey`, `ProviderName`, `ProjectPaths`) | `crates/joi-core/src/config.rs` | `Config::load(cli_path)` / `load_from(file, paths)`. Today **flat**: `config.audio`, `config.screen`, `config.terminal`. |
| `SessionManager::spawn(config: Config, clock, history, factory) -> SessionManagerHandle` | `crates/joi-core/src/manager.rs:214` | The actor. Handle methods: `start/stop/send_text/send_audio/send_frame/set_mic_muted/subscribe()→Receiver<UiEvent>/subscribe_audio()→Receiver<Vec<i16>>`. |
| `SessionConfig::from_config(cfg: &Config, …)` | `crates/joi-core/src/session/mod.rs:51` | Reads `cfg.live_api.gemini.*`. Leave as-is (S2 keeps `live_api` top-level). |
| `build_session_factory(config: &Config) -> Result<Box<dyn SessionFactory>, FactoryError>` | `crates/joi-providers/src/factory.rs:28` | Reads `config.live_api.{provider, gemini.api_key}`. |
| `MediaConfig { frame_samples, screen_fps, screen_max_width, screen_quality, echo_cancellation }`, `MediaEngine::new(handle, config)` | `crates/joi-media/src/engine.rs:26,64` | `start_capture/stop_capture/set_mic_muted/start_screenshare/stop_screenshare` (idempotent). |
| Tauri shell: `AppCtx { handle: Option<SessionManagerHandle>, has_key: bool, media: Option<MediaEngine> }`; commands `ping, has_api_key, start, stop, send_text, set_mic_muted, start_screenshare, stop_screenshare`; `main()` builds it + the `"ui_event"` emit pump | `src-tauri/src/main.rs` | This is the only Tauri-coupled code. **The composition lives in the `.setup()` closure** — that's what S1 moves out. |
| Frontend IPC contract | `src/ipc.ts` (+ `src/ipc.test.ts` parity) | `commands` object (1:1 with `generate_handler!`) and `UiEvent` union. |

---

## Stage S1 — Extract `crates/joi-app` (the `JoiApp` API). No behavior change.

**Outcome:** the composition + command surface move into a Tauri-free library; `src-tauri` becomes a
thin adapter over it. The desktop app behaves identically.

### S1.1 Create the crate

Create `crates/joi-app/Cargo.toml`:
```toml
[package]
name = "joi-app"
description = "Host-agnostic composition + API for the JOI engine (no Tauri/UI)."
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[lints]
workspace = true

[dependencies]
joi-core = { path = "../joi-core" }
joi-providers = { path = "../joi-providers" }
joi-media = { path = "../joi-media" }
tokio = { workspace = true }
tracing = { workspace = true }
```

Add `"crates/joi-app"` to `members` in the root `Cargo.toml`.

### S1.2 Write `crates/joi-app/src/lib.rs`

> Uses the **current (flat)** config field names (`config.audio`, `config.screen`); S2 updates them.
> `build` is sync but spawns Tokio tasks, so it **must be called inside a Tokio runtime context**
> (the Tauri shell wraps it in `async_runtime::block_on`; `joi-cli` runs under `#[tokio::main]`).

```rust
//! Host-agnostic application layer for JOI (Seam A). A host (Tauri shell, CLI, future HTTP server)
//! builds a [`JoiApp`] from [`Config`] and drives it via these methods + the event/audio streams.
//! No Tauri, webview, HTTP, or CLI types appear here.

use std::sync::Arc;

use joi_core::clock::SystemClock;
use joi_core::config::Config;
use joi_core::error::SessionError;
use joi_core::history::InMemoryHistory;
use joi_core::manager::{SessionFactory, SessionManager, SessionManagerHandle};
use joi_core::media::AudioFormat;
use joi_core::session::event::UiEvent;
use joi_media::{MediaConfig, MediaEngine};
use tokio::sync::broadcast;

/// Whether the engine drives local audio/screen devices itself.
pub enum MediaMode {
    /// Desktop: cpal mic/playback + xcap screen, bound to the session.
    LocalDevices,
    /// Headless: no local devices; the host feeds/consumes audio via `send_audio`/`subscribe_audio`.
    None,
}

/// The composed JOI engine. `handle`/`media` are `None` when no provider is configured (e.g. no API
/// key) — the app still constructs; session commands then return a clear error.
pub struct JoiApp {
    handle: Option<SessionManagerHandle>,
    media: Option<MediaEngine>,
    has_key: bool,
}

impl JoiApp {
    /// Composition root. **Call inside a Tokio runtime** (spawns tasks).
    #[must_use]
    pub fn build(config: Config, media_mode: MediaMode) -> Self {
        let has_key = config.live_api.gemini.api_key.is_set();
        match joi_providers::build_session_factory(&config) {
            Ok(factory) => {
                let factory: Arc<dyn SessionFactory> = Arc::from(factory);
                let clock = Arc::new(SystemClock);
                let history = Arc::new(InMemoryHistory::new());
                let handle = SessionManager::spawn(config.clone(), clock, history, factory);
                let media = match media_mode {
                    MediaMode::LocalDevices => {
                        let media_config = MediaConfig {
                            frame_samples: AudioFormat::INPUT
                                .samples_per_frame(config.audio.frame_ms),
                            screen_fps: config.screen.fps,
                            screen_max_width: config.screen.max_width,
                            screen_quality: config.screen.quality,
                            echo_cancellation: config.audio.echo_cancellation,
                        };
                        Some(MediaEngine::new(handle.clone(), media_config))
                    }
                    MediaMode::None => None,
                };
                Self { handle: Some(handle), media, has_key }
            }
            Err(e) => {
                tracing::warn!("session unavailable until configured: {e}");
                Self { handle: None, media: None, has_key }
            }
        }
    }

    fn session(&self) -> Result<&SessionManagerHandle, SessionError> {
        self.handle.as_ref().ok_or_else(|| {
            SessionError::Provider(
                "no API key configured (set GEMINI_API_KEY or live_api.gemini.api_key)".to_string(),
            )
        })
    }

    /// Start (or resume) a session and begin local mic capture (if `LocalDevices`).
    pub async fn start(&self, resume: bool) -> Result<(), SessionError> {
        self.session()?.start(resume).await?;
        if let Some(m) = &self.media {
            m.start_capture();
        }
        Ok(())
    }

    /// Stop the session and local capture.
    pub async fn stop(&self, pause: bool) -> Result<(), SessionError> {
        if let Some(m) = &self.media {
            m.stop_capture();
        }
        self.session()?.stop(pause).await
    }

    pub async fn send_text(&self, text: &str) -> Result<(), SessionError> {
        self.session()?.send_text(text).await
    }

    /// Push a mic frame (headless hosts; the desktop's `MediaEngine` does this itself).
    pub async fn send_audio(&self, pcm: Vec<i16>) -> Result<(), SessionError> {
        self.session()?.send_audio(pcm).await
    }

    pub fn set_mic_muted(&self, muted: bool) {
        if let Some(m) = &self.media {
            m.set_mic_muted(muted);
        }
    }

    pub fn start_screenshare(&self) {
        if let Some(m) = &self.media {
            m.start_screenshare();
        }
    }

    pub fn stop_screenshare(&self) {
        if let Some(m) = &self.media {
            m.stop_screenshare();
        }
    }

    #[must_use]
    pub fn has_api_key(&self) -> bool {
        self.has_key
    }

    /// Subscribe to UI events (`None` if no session is configured).
    #[must_use]
    pub fn subscribe_events(&self) -> Option<broadcast::Receiver<UiEvent>> {
        self.handle.as_ref().map(SessionManagerHandle::subscribe)
    }

    /// Subscribe to provider audio-out frames (24 kHz mono PCM16) — for headless hosts.
    #[must_use]
    pub fn subscribe_audio(&self) -> Option<broadcast::Receiver<Vec<i16>>> {
        self.handle.as_ref().map(SessionManagerHandle::subscribe_audio)
    }
}
```

> If any symbol path above doesn't resolve (e.g. `joi_core::session::event::UiEvent`), find the real
> path with `grep -rn "pub use" crates/joi-core/src/lib.rs` and adjust the `use`. Don't invent APIs.

### S1.3 Rewire `src-tauri/src/main.rs` to use `JoiApp`

Mechanical replacement of the embedded composition + `AppCtx` with `JoiApp`:

1. **Imports:** add `use joi_app::{JoiApp, MediaMode};`. Remove now-unused imports that moved into
   `joi-app`: `SessionFactory`, `SessionManager`, `MediaConfig`, `MediaEngine`, `SystemClock`,
   `InMemoryHistory`, `AudioFormat`. Keep `Config`, `tauri::*`, `broadcast::error::RecvError`, `serde`.
   (Let the compiler tell you which imports are now unused.)
2. **Add the crate dep:** in `src-tauri/Cargo.toml`, add `joi-app = { path = "../crates/joi-app" }`.
   You may then be able to drop direct `joi-providers`/`joi-media` deps if `src-tauri` no longer names
   them (check after compiling).
3. **Delete `struct AppCtx` and its `impl` (the `session()` helper).** Replace the Tauri-managed
   state type with `JoiApp` everywhere it appears: every command's `ctx: State<'_, AppCtx>` becomes
   `app: State<'_, JoiApp>`, and the body calls the `JoiApp` method. Keep the `HasApiKeyResult` /
   `StartResult` serde structs. Examples:
   ```rust
   #[tauri::command]
   async fn start(resume: bool, app: State<'_, JoiApp>) -> Result<StartResult, String> {
       app.start(resume).await.map_err(|e| e.to_string())?;
       Ok(StartResult { session_id: "session".to_string() })
   }

   #[tauri::command]
   #[allow(clippy::needless_pass_by_value)]
   fn has_api_key(app: State<'_, JoiApp>) -> HasApiKeyResult {
       HasApiKeyResult { present: app.has_api_key() }
   }

   #[tauri::command]
   #[allow(clippy::needless_pass_by_value)]
   fn set_mic_muted(muted: bool, app: State<'_, JoiApp>) { app.set_mic_muted(muted); }
   ```
   Do the same for `stop` (`app.stop(pause).await`), `send_text`, `start_screenshare`,
   `stop_screenshare`. `ping` is unchanged. The `generate_handler![...]` list is unchanged.
4. **Rewrite the `.setup()` closure** — replace the whole "build (handle, media) via block_on +
   ui_event pump + app.manage(AppCtx{…})" block with:
   ```rust
   .setup(move |app_handle_setup| {
       // Build the engine inside the runtime (spawns tasks). Config is moved in.
       let app = tauri::async_runtime::block_on(async {
           JoiApp::build(config, MediaMode::LocalDevices)
       });

       // The one Tauri-specific bridge: fan UiEvents out to the webview (SPEC §11.3).
       if let Some(mut ui_rx) = app.subscribe_events() {
           let emitter = app_handle_setup.handle().clone();
           tauri::async_runtime::spawn(async move {
               loop {
                   match ui_rx.recv().await {
                       Ok(event) => { let _ = emitter.emit("ui_event", event); }
                       Err(RecvError::Closed) => break,
                       Err(RecvError::Lagged(_)) => {}
                   }
               }
           });
       }

       app_handle_setup.manage(app);
       Ok(())
   })
   ```
   (Rename the closure param if it collides with `app`.) `main()` keeps `let config = Config::load(None)?;`
   and drops the now-unused `has_key` / `media_config` locals (they're inside `JoiApp::build` now).

### S1.4 Verify S1
- `cargo build -p joi-app` succeeds **and pulls in no Tauri** (sanity: `cargo tree -p joi-app | grep -i tauri` prints nothing).
- `cargo fmt` then `./scripts/check.sh` is green.
- (Optional manual) `bun run tauri dev` → app starts, Start/Stop/text/screenshare all still work.

**Commit:** `Extract joi-app: host-agnostic engine API; Tauri becomes a thin adapter`.

---

## Stage S2 — Config: one YAML, sections per module

**Outcome:** `media: {audio, screen}` and `ui: {terminal}` group config by module; `live_api`,
`history`, `logging` stay top-level (engine). Structs stay in `joi-core` (the shared-types crate) —
**no cross-crate moves.**

### S2.1 Edit `crates/joi-core/src/config.rs`
Add two grouping structs and re-shape `Config`:
```rust
/// Native media I/O settings (joi-media).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MediaCfg {
    pub audio: AudioCfg,
    pub screen: ScreenCfg,
}

/// Web-frontend settings (delivered to the UI by the host).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UiCfg {
    pub terminal: TerminalCfg,
}
```
Change `Config` from `{ live_api, audio, screen, history, terminal, logging }` to:
```rust
pub struct Config {
    pub live_api: LiveApiCfg,
    pub history: HistoryCfg,
    pub logging: LoggingCfg,
    pub media: MediaCfg,   // { audio, screen }
    pub ui: UiCfg,         // { terminal }
}
```
Update `Config::default()` to nest the existing `AudioCfg`/`ScreenCfg` defaults under
`media: MediaCfg { audio: …, screen: … }` and `TerminalCfg` under `ui: UiCfg { terminal: … }`.
Update `validate()`: `self.audio.*` → `self.media.audio.*`, `self.screen.*` → `self.media.screen.*`.

### S2.2 Update every reference to the moved fields
Find them: `grep -rn "config\.audio\|config\.screen\|config\.terminal\|cfg\.audio\|cfg\.screen\|\.terminal\." crates/ src-tauri/ --include=*.rs`.
Expected sites: `crates/joi-app/src/lib.rs` (`config.audio.frame_ms`→`config.media.audio.frame_ms`,
`config.screen.*`→`config.media.screen.*`, `config.audio.echo_cancellation`→
`config.media.audio.echo_cancellation`) and the config tests below. (`live_api`/`history`/`logging`
are unchanged, so `SessionConfig::from_config` and `build_session_factory` need no change.)

### S2.3 Update config tests (`crates/joi-core/src/config.rs` `mod tests`)
The Jail tests build YAML fixtures with top-level `audio:`/`provider:`. Re-nest them, e.g.
`file_overrides_defaults` / `env_overrides_file` change `audio:\n  frame_ms: …` to
`media:\n  audio:\n    frame_ms: …`, and any `cfg.audio.*` assertions to `cfg.media.audio.*`. The env
test that sets `JOI_AUDIO__FRAME_MS` becomes `JOI_MEDIA__AUDIO__FRAME_MS`.

### S2.4 Update the example + note migration
- `config/joi.example.yaml`: regroup to:
  ```yaml
  live_api: { … }            # unchanged
  history: { … }
  logging: { … }
  media:
    audio:  { input_sample_rate: 16000, …, echo_cancellation: true }
    screen: { enabled: false, fps: 1.0, max_width: 768, quality: 80 }
  ui:
    terminal: { theme: joi-dark, font: "JetBrains Mono", scrollback: 5000 }
  ```
- **Migration:** this is a breaking YAML change. The loader merges defaults, so an old config's
  top-level `audio:`/`screen:`/`terminal:` are silently ignored (defaults used). Either delete
  `~/.config/joi/joi.yaml` to regenerate, or move those keys under `media:`/`ui:`. Document this in
  the README config section (S4). Do **not** add legacy-key shims unless asked.

### S2.5 Verify S2
`cargo fmt` → `./scripts/check.sh` green. **Commit:** `Config: group YAML into per-module sections (media, ui)`.

---

## Stage S3 — `crates/joi-cli`: headless proof

**Outcome:** a binary that drives the engine with no GUI, proving Seam A and `MediaMode::None`.

Create `crates/joi-cli/Cargo.toml` (bin) depending on `joi-app`, `joi-core`, `tokio` (with `macros`,
`rt-multi-thread`), `anyhow`, `tracing-subscriber`. Add `"crates/joi-cli"` to workspace members.

`crates/joi-cli/src/main.rs` (minimal, text-only):
```rust
//! Headless JOI host: prove the engine runs with no Tauri/UI. Reads commands from stdin
//! (`start`, `stop`, `quit`, or any other line = send as text), prints transcripts.
use joi_app::{JoiApp, MediaMode};
use joi_core::config::Config;
use joi_core::session::event::UiEvent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info,joi=info").init();
    let config = Config::load(None)?;
    // LocalDevices if you want mic/speaker; None for pure text/headless.
    let app = JoiApp::build(config, MediaMode::None);

    if let Some(mut rx) = app.subscribe_events() {
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if let UiEvent::Transcript { speaker, text, .. } = ev {
                    print!("{speaker:?}: {text}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
        });
    }

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        if std::io::BufRead::read_line(&mut stdin.lock(), &mut line)? == 0 { break; }
        match line.trim() {
            "quit" => break,
            "start" => { app.start(false).await?; }
            "stop" => { app.stop(false).await?; }
            text => { app.send_text(text).await?; }
        }
    }
    app.stop(false).await.ok();
    Ok(())
}
```
(Speaker/UiEvent field shapes: confirm against `crates/joi-core/src/session/event.rs`. Adjust the
match arms if names differ.)

### Verify S3
- `cargo build -p joi-cli` (no Tauri in tree). `./scripts/check.sh` green.
- (Optional manual) `GEMINI_API_KEY=… cargo run -p joi-cli`, type `start`, type a sentence, see the
  agent transcript print. Proves the engine is GUI-free.

**Commit:** `Add joi-cli: headless host proving the engine runs without Tauri`.

---

## Stage S4 — Docs + CI guard

1. **CI guard** in `scripts/check.sh`: assert the engine has no GUI deps —
   `cargo tree -p joi-app -e no-dev | grep -qi 'tauri\|webkit' && { echo 'joi-app must not depend on Tauri'; exit 1; } || true`.
2. **SPEC.md:** update §2 layout (new crates `joi-app`, `joi-cli`; the 3-layer split), note Seam A
   (JoiApp) and Seam B (IPC, §11), and §13 config = the per-module sections.
3. **README.md / CLAUDE.md:** add the layer map + the config migration note (S2.4). In CLAUDE.md's
   crate table, add `joi-app` (engine API) and `joi-cli`.
4. **`crates/joi-app/src/lib.rs`:** crate-level doc already states Seam A; ensure it lists the method
   set and the `MediaMode` contract.

**Commit:** `Docs + CI: document the 3-layer split and guard joi-app against Tauri deps`.

---

## Stage S5 — Round out the seams (was "out of scope")

**Outcome:** the three deferred follow-ups, each its own commit, each behavior-preserving for the
desktop app and each leaving `./scripts/check.sh` green.

### S5a — Deliver the `ui` config section to the frontend (`get_ui_config`)
The `ui: {terminal}` section (S2) is loaded but the webview still hard-codes theme/font/scrollback.
Close the loop **the architecture-correct way** — config flows out of Rust; TS only renders it.
- `crates/joi-app/src/lib.rs`: store the `UiCfg` (cheap clone before `config` is moved into the
  manager) and add `pub fn ui_config(&self) -> UiCfg`.
- `src-tauri/src/main.rs`: add `get_ui_config(app) -> UiCfg` and register it in `generate_handler!`.
- `src/ipc.ts`: add the `getUiConfig` command (+ `UiConfig`/`TerminalConfig` types — superseded by
  S5b's generated types).
- `src/App.tsx`: fetch it once on mount; pass `terminal` to `<Terminal>`.
- `src/components/Terminal.tsx`: apply `font` + `scrollback`; resolve the theme **name** to concrete
  xterm colors via a small presentation map (legitimately frontend — it's styling, not logic).

### S5b — Generate the TS IPC types from Rust (single source of truth)
Replace the hand-maintained `UiEvent`/enums in `ipc.ts` with types generated from `joi-core`.
- Add `ts-rs` to `[workspace.dependencies]` and as a `joi-core` dependency (pure Rust, no OS/IO — fits
  core's purity rule). Derive `TS` (via the existing serde attrs) on `Speaker`, `AppState`,
  `ConnectionStatus`, `HistoryMeta`, `UiEvent`, `TerminalCfg`, `UiCfg`, exporting to
  `../../src/bindings/`. ts-rs honors `#[serde(tag/rename_all/rename)]` so the JSON shape is preserved.
- Generation runs in the normal `cargo test` (so `check.sh` regenerates); `ipc.ts` imports + re-exports
  from `./bindings/`. Keep `ipc.test.ts` as a runtime guard.
- `scripts/check.sh`: after the Rust tests, `git diff --exit-code src/bindings` so stale bindings fail CI.

### S5c — HTTP/WS host (`crates/joi-server`)
A third host (besides Tauri + CLI) proving Seam A over the network — the documented "later, separate
adapter."
- New bin crate depending on `joi-app`/`joi-core` + `axum` (ws) + `tokio`(net) + `serde_json`/
  `futures-util`/`anyhow`/`tracing-subscriber`. `MediaMode::None` (headless; audio-over-WS is a future
  extension). Bind addr from `JOI_SERVER_ADDR` (host runtime config, like the dev port — not engine YAML).
- `/ws`: per-connection `subscribe_events()` → forward each `UiEvent` as a JSON text frame; inbound
  JSON `{"cmd":"start|stop|text", ...}` → `JoiApp` calls.
- Add `joi-server` to the no-Tauri guard loop in `scripts/check.sh` and the member list + docs.

## Acceptance criteria (whole runbook)
- `cargo build -p joi-app` and `cargo build -p joi-cli` succeed with **no** Tauri/webkit in their
  dependency trees.
- `cargo build -p joi` (Tauri) succeeds; `src-tauri/src/main.rs` contains no domain composition —
  only command shims + the `ui_event` emit pump + `JoiApp::build`.
- `bun run build` (frontend) succeeds unchanged; `src/ipc.ts` still mirrors the command set 1:1 and
  `src/ipc.test.ts` passes.
- `./scripts/check.sh` green at every commit.
- The YAML has top-level sections `live_api`, `history`, `logging`, `media`, `ui`, each owned by one
  module; `config/joi.example.yaml` documents them.
- Desktop behavior is unchanged from before S1.

## Out of scope (do not do unless asked)
- Moving config structs out of `joi-core` into per-module crates (kept central deliberately).
- Multi-provider config, or any change to audio/transcript/AEC behavior.
- Streaming binary audio over the WS host (S5c is JSON commands + the `UiEvent` stream only).

> The HTTP/WS host, TS-types-from-Rust, and `get_ui_config` delivery moved **into scope** as Stage S5.
