# Joi

A local, provider-agnostic **voice + screen + terminal-UI** companion. Full-duplex speech with a
realtime model, screen sharing as video input, a colorized terminal transcript, and a
start/stop/pause/resume lifecycle with bounded, restorable history.

See [`SPEC.md`](./SPEC.md) for *what* it must do and [`PLAN.md`](./PLAN.md) for the *how* (milestone
plan M0–M5). This README covers the current build.

## Architecture — three layers

Joi is split into three independently-compilable layers (see
[`PLAN-MODULARIZATION.md`](./PLAN-MODULARIZATION.md)):

| Layer | Crates / dirs | Notes |
|---|---|---|
| **JOI engine** (host-agnostic, no Tauri) | `crates/joi-core`, `joi-providers`, `joi-media`, `joi-app` | Owns all logic, media (cpal/xcap), provider sessions, config, history. `joi-app` exposes **`JoiApp`** — the API a host drives (**Seam A**). |
| **Headless hosts** | `crates/joi-cli`, `crates/joi-server` | `joi-cli` is a text-only stdin host; `joi-server` exposes `JoiApp` over a WebSocket (`/ws`) with the same JSON-command + `UiEvent`-stream contract. Both prove the engine runs with no GUI. |
| **Tauri backend** | `src-tauri` | Thin adapter: `#[tauri::command]`s → `JoiApp`; pumps its `UiEvent` stream to the webview as `ui_event` (**Seam B**, JSON IPC). |
| **Web frontend** | `src/` | React UI; depends on no Rust crate, only the IPC contract in `src/ipc.ts` (whose types are generated from `joi-core` by ts-rs into `src/bindings/`). |

`./scripts/check.sh` guards that `joi-app`/`joi-cli`/`joi-server` pull in **no** Tauri/WebKit, and that `src/bindings` stays in sync with the Rust types.

## What's implemented

The verifiable, fully-tested backbone (M0 foundation + the M1 core loop on a mock):

| Crate | Contents |
|---|---|
| **`joi-core`** | Pure domain + port traits. `Config` (figment: defaults → YAML file → `JOI_`/`GEMINI_*` env, env wins; XDG paths, validation; provider key lives at `live_api.gemini.api_key` as a redacting `ApiKey`), `Clock`, `RealtimeSession` + `SessionEvent`/`UiEvent`, `HistoryStore` (`InMemory` + bounded `Jsonl`), `ScreenSource`, `media` framing/PCM conversions, and the **`SessionManager` actor** (owns the session, `select!`s commands vs. the provider event stream, fans out `UiEvent`s, appends finalized transcripts). The `[POST]` tools seam exists, unused. |
| **`joi-providers`** | `MockSession` (scripted, no network), the `OpenAIAdapter` compile-only stub (SPEC §4.4), and the `GeminiAdapter` stub awaiting the M2 adk-rust spike. |
| **`joi-testkit`** | `run_conformance` — one ordered scenario run against any adapter; proves provider-agnosticism (passes against the mock, verifies the OpenAI stub). |
| **frontend (`src/`)** | Vite + React + TS + Tailwind v4 scaffold. The testable media DSP (`media/dsp.ts`: downsample, PCM framing, jitter buffer, transcript throttle) and the typed IPC boundary (`ipc.ts`) mirroring the Rust contract, both unit-tested with Vitest. |

Everything above passes `cargo fmt`/`clippy -D warnings`/`test` and `bun run typecheck`/`test`/`build`.

## Not yet built (and why)

- **Build system deps** — compiling the Tauri shell needs the system WebKitGTK libraries
  (`libwebkit2gtk-4.1-dev`) for the UI webview and ALSA dev headers (`alsa-lib`/`libasound2-dev`)
  for native cpal audio — see `scripts/setup-linux.sh`. Media is native (cpal + xcap), so **no
  GStreamer is needed**.
- **M0 media spike, M2 (Gemini/adk-rust), M3 lifecycle persistence wiring, M4 screen capture
  pipelines, M5 packaging** — these need the running webview, a live API key, or the screen-capture
  backends. The core seams for all of them are in place; see `PLAN.md`.

## Build & verify

```bash
# Rust core (no system libraries required)
cargo test --workspace
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets

# Frontend (needs bun; pnpm works too)
bun install
bun run test && bun run build

# Everything at once (mirrors CI)
./scripts/check.sh
```

Full setup for the eventual Tauri build (system deps + tauri-cli):

```bash
./scripts/setup-linux.sh    # apt or pacman; needs sudo
```

### Config

Config is a **YAML** file at `~/.config/joi/joi.yaml`; Joi writes it with defaults on first run if
it's missing (see `config/joi.example.yaml` for the documented schema). Top-level sections map to
modules: `live_api` / `history` / `logging` (engine), `media: {audio, screen}` (joi-media),
`ui: {terminal}` (frontend). Every field can be set in the file **or** via a `JOI_`-prefixed env var
(nested with `__`, e.g. `JOI_MEDIA__AUDIO__FRAME_MS=30`), and **env takes precedence over the file**.

> **Migration:** audio/screen settings now live under `media:` and terminal under `ui:` (they used to
> be top-level). An old config's top-level `audio`/`screen`/`terminal` keys are ignored (defaults
> used); move them under `media`/`ui`, or delete the file to regenerate. `live_api` (incl. the key)
> is unchanged.

The Gemini API key lives at `live_api.gemini.api_key`. It can be set in the YAML, but **prefer the
environment** so it isn't stored in plaintext on disk:

- `GEMINI_API_KEY=…` (the convenient, conventional name), or
- `JOI_LIVE_API__GEMINI__API_KEY=…` (the uniform nested form).

Env always wins over the file. The model is `live_api.gemini.model` (or `GEMINI_MODEL`) — set the
exact id your key can access.

### Display-server note (Linux)

Media is native now (cpal audio, xcap screen), so the old WebKitGTK getUserMedia/Wayland fragility
no longer applies. Audio goes through ALSA (PipeWire's ALSA compat works). If the webview UI itself
misbehaves on Wayland, the X11 fallback still works:

```bash
GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 cargo tauri dev
```
