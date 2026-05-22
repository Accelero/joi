# Joi — agent guide

Joi is a local, provider-agnostic **voice + screen + terminal-UI** companion: a Tauri v2 desktop app
with a Rust backend and a thin web (React/TS) frontend.

## Architecture principle (read first)

**All logic and heavy lifting live in the Rust backend. The TypeScript frontend is UI only.**

- **Rust** owns everything substantive: session/provider logic, audio capture + playback, screen
  capture, DSP, resampling, jitter buffering, history, config, secrets, state machine. Media never
  crosses into the webview — `crates/joi-media` does all capture/playback natively via cpal/xcap.
- **TypeScript** (`src/`) is presentation + input only: render `UiEvent`s into the terminal/controls
  and dispatch commands. **No business logic, no media, no DSP in TS.** If you're tempted to compute,
  buffer, transform, or orchestrate in the frontend, it belongs in Rust behind an IPC command.
- When adding a feature, default to: new logic in a crate → exposed via a `SessionManagerHandle`/
  engine method → surfaced over IPC → thin TS that calls it and renders the result.

## Workspace layout

| Crate | Responsibility |
|---|---|
| `crates/joi-core` | Pure domain: `Config`, `Clock`, `SecretStore`, `RealtimeSession`/`SessionEvent`/`UiEvent`, `HistoryStore`, `ScreenSource`, pure `media` DSP (framing, resample, `JitterBuffer`, PCM/float), and the **`SessionManager`** actor + `SessionManagerHandle`. No OS/audio/IO deps. |
| `crates/joi-media` | Native I/O behind the **`MediaEngine`** interface: cpal capture (with NS+AGC) and playback, xcap screen capture. Bound to a `SessionManagerHandle`; keeps OS/audio deps out of `joi-core`. |
| `crates/joi-providers` | Realtime provider adapters (Gemini Live via adk-rust; OpenAI stub) + `build_session_factory`. |
| `crates/joi-testkit` | Test doubles/helpers. |
| `src-tauri` | **Thin** composition root: loads config/secrets, builds the manager + `MediaEngine`, exposes `#[tauri::command]`s, and pumps `UiEvent`s to the webview. No domain logic here. |
| `src/` | React UI: `App.tsx` (wires events↔commands), `components/Terminal.tsx` (xterm), `components/Controls.tsx`, `ipc.ts` (typed IPC boundary). |

## IPC boundary

JSON only — no media crosses IPC (SPEC §11). Two directions:
- **Commands** (`invoke`): `ipc.ts`'s `commands` object mirrors **exactly** the `generate_handler!`
  list in `src-tauri/src/main.rs`. Keep them 1:1 — don't expose a command in `ipc.ts` that isn't
  registered.
- **Events**: the backend emits the tagged `UiEvent` enum on a single `ui_event` channel. `ipc.ts`'s
  `UiEvent` union mirrors `joi-core`'s serde shape; `src/ipc.test.ts` pins JSON parity so drift fails
  the build.

## Build / test

```bash
cargo test --workspace
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
bun install && bun run test && bun run build
./scripts/check.sh                 # mirrors CI (Rust + TS)
```

## Run

- **Dev (hot reload):** `bun run tauri dev` — runs the Vite dev server; the webview loads it.
- **Standalone:** `bun run tauri build --no-bundle` → `./target/release/joi` (frontend embedded).
  Do **not** use `cargo build --release` for the standalone — Tauri stays in dev mode and points the
  webview at the dev URL. The provider key is read from `GEMINI_API_KEY` at launch.
