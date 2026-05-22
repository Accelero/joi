# Joi

A local, provider-agnostic **voice + screen + terminal-UI** companion. Full-duplex speech with a
realtime model, screen sharing as video input, a colorized terminal transcript, and a
start/stop/pause/resume lifecycle with bounded, restorable history.

See [`SPEC.md`](./SPEC.md) for *what* it must do and [`PLAN.md`](./PLAN.md) for the *how* (milestone
plan M0–M5). This README covers the current build.

## What's implemented

The verifiable, fully-tested backbone (M0 foundation + the M1 core loop on a mock):

| Crate | Contents |
|---|---|
| **`joi-core`** | Pure domain + port traits. `Config` (figment: defaults → TOML → `JOI_` env, XDG paths, validation), `Clock`, `SecretStore` (redacting `SecretString`), `RealtimeSession` + `SessionEvent`/`UiEvent`, `HistoryStore` (`InMemory` + bounded `Jsonl`), `ScreenSource`, `media` framing/PCM conversions, and the **`SessionManager` actor** (owns the session, `select!`s commands vs. the provider event stream, fans out `UiEvent`s, appends finalized transcripts). The `[POST]` tools seam exists, unused. |
| **`joi-providers`** | `MockSession` (scripted, no network), the `OpenAIAdapter` compile-only stub (SPEC §4.4), and the `GeminiAdapter` stub awaiting the M2 adk-rust spike. |
| **`joi-testkit`** | `run_conformance` — one ordered scenario run against any adapter; proves provider-agnosticism (passes against the mock, verifies the OpenAI stub). |
| **frontend (`src/`)** | Vite + React + TS + Tailwind v4 scaffold. The testable media DSP (`media/dsp.ts`: downsample, PCM framing, jitter buffer, transcript throttle) and the typed IPC boundary (`ipc.ts`) mirroring the Rust contract, both unit-tested with Vitest. |

Everything above passes `cargo fmt`/`clippy -D warnings`/`test` and `bun run typecheck`/`test`/`build`.

## Not yet built (and why)

- **Tauri shell (`src-tauri/`)** — the composition root, `#[tauri::command]` handlers, the binary
  `tauri::ipc::Channel` media transport, and the keychain `SecretStore`. Compiling Tauri needs the
  system WebKitGTK libraries (`libwebkit2gtk-4.1-dev`) for the UI webview and ALSA dev headers
  (`alsa-lib`/`libasound2-dev`) for native cpal audio — see `scripts/setup-linux.sh`. Media is
  native (cpal + xcap), so **no GStreamer is needed**.
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

Copy `config/joi.example.toml` → `~/.config/joi/joi.toml`. Any field overrides via `JOI_`-prefixed
env vars (nested with `__`, e.g. `JOI_AUDIO__FRAME_MS=30`). **The API key is never in config** — it
lives in the OS keychain; for dev, `GEMINI_API_KEY` is read at runtime and never persisted.

### Display-server note (Linux)

Media is native now (cpal audio, xcap screen), so the old WebKitGTK getUserMedia/Wayland fragility
no longer applies. Audio goes through ALSA (PipeWire's ALSA compat works). If the webview UI itself
misbehaves on Wayland, the X11 fallback still works:

```bash
GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 cargo tauri dev
```
