# Joi - Codex Agent Guide

This is the file to keep in context when working on Joi. It condenses the current source of truth
from `doc/ARCH.md`, `doc/SPEC.md`, and `CLAUDE.md`, plus the practical commands needed to run and
debug the project.

## Status

- The clean rewrite is implemented. The active workspace is the six crates under `crates/`.
- `doc/ARCH.md` is normative for architecture and layering.
- `doc/SPEC.md` is normative for requirements, `[NOW]` / `[LATER]` status, and config reference.
- `doc/TOOLS_PLAN.md` tracks the tool roadmap. The first built-in pipeline exists; MCP, stronger
  bash analysis, and richer permission UX are follow-up work.
- `CLAUDE.md` is older agent guidance but now points back to `ARCH.md`; follow this `AGENTS.md` and
  `ARCH.md` for current decisions.

## Product Shape

Joi is a local, provider-agnostic voice + screen companion. It connects directly to Gemini Live
today, streams mic audio and screen frames, plays model audio, renders a live transcript in the TUI,
and persists conversations as resumable sessions.

The app is TUI-first, not TUI-only. All substantive behavior belongs in the Rust engine so future
frontends can drive the same `JoiApp` API.

## Architecture Rules

- All logic lives in Rust. Frontends render events and dispatch commands.
- The engine is host-agnostic. No frontend, GUI, webview, IPC, or terminal type belongs in engine
  crates.
- Stay provider-agnostic outside `joi-providers`. `joi-core` never names Gemini, Mock, adk, or a
  concrete provider.
- Mechanism belongs in `joi-core`; composition and policy belong in `joi-app`.
- "No I/O in core" means no device or provider/network I/O. Domain filesystem I/O, such as config
  and session logs, is allowed in `joi-core`.
- One command/event surface: hosts call `JoiApp` / `SessionManagerHandle` methods and fold
  `UiEvent`s. Boundary types are plain Rust with `serde`, not generated `ts-rs` bindings.
- The engine must remain provably headless. Features should work through `joi-app` tests with
  `MediaMode::None` and the Mock provider.
- Never put provider-specific schema, voice, wire, reconnection, or endpoint behavior in `joi-core`.
- Never put device I/O outside `joi-media`.
- Never make `joi-tui` depend directly on `joi-providers` or `joi-media`.

## Crate Ownership

- `joi-core`: domain contracts and mechanisms: config, settings mechanism, clock, history/session
  store, `RealtimeSession` trait, `SessionEvent`, `UiEvent`, `SessionManager`, metrics,
  connectivity traits, media contracts, tool contracts/runtime.
- `joi-providers`: provider adapters and provider facts: Gemini Live adapter, Mock adapter,
  `build_session_factory`, `build_connectivity_probe`, provider voice catalog, Gemini event mapping,
  token/transport usage, provider-specific history seeding.
- `joi-media`: native device I/O only: cpal capture/playback, APM chain, xcap screen capture, media
  pumps bound to `SessionManagerHandle`.
- `joi-app`: composition root and Seam A API: builds provider/history/media/manager, owns runtime
  settings persistence policy, exposes session commands, event/audio subscriptions, and UI config.
- `joi-tui`: presentation and input only: ratatui views, pure reducers, key mapping, slash commands,
  session picker, voice picker.
- `joi-testkit`: shared test fixtures and provider conformance suite.
- `vendor/`: patched dependencies that are required, not incidental.

## Current Feature Reality

- Sessions persist under `~/.joi/sessions` as `<uuid>.jsonl` plus `index.json`.
- The session store auto-names from the first user turn and lists newest activity first.
- Starting a session always seeds from the current store within the configured token budget. A new
  session has an empty log, so it seeds nothing.
- `/resume` retargets the store and reloads the transcript view, but does not auto-start a billable
  provider session. The user presses F2 to start.
- `/new` switches to a fresh persisted session.
- `/voice` uses `settings_schema()` and `update_setting()` to persist the provider voice. It applies
  on the next session start.
- Runtime settings exist for `voice`, terminal `accent`, and terminal `background`. Only voice has a
  TUI picker today; a full settings panel is future work.
- Automatic transient reconnect/session resumption is still `[LATER]`. Today provider/server close
  is surfaced and the live session stops cleanly.
- Tool calls are implemented for the built-in harness when `tools.enabled=true`; tools are disabled
  by default and MCP is still follow-up work.

## Config And Local Files

- Config file: `~/.joi/config.json`.
- Legacy config: `~/.joi/config` YAML is migrated once to JSON if `config.json` is absent.
- Prompt file: `~/.joi/prompt.md`; when present and non-blank, it overrides
  `live_api.gemini.system_instruction`.
- Sessions: `~/.joi/sessions`.
- TUI logs: `$JOI_TUI_LOG` if set, otherwise the platform state dir, e.g.
  `~/.local/state/joi/joi-tui.log` on Linux.
- API key is redacted and never written back to config. Prefer `GEMINI_API_KEY`.
- Gemini model has no code default. Set `GEMINI_MODEL` or `live_api.gemini.model`.

## Running The App

```sh
export GEMINI_API_KEY=...
export GEMINI_MODEL=gemini-3.1-flash-live-preview
cargo run -p joi-tui
```

Useful TUI controls:

- `F2`: start / stop live session.
- `F3`: mute.
- `F4`: start / stop screen share.
- `F1`: help.
- `Enter`: send typed text.
- `/resume`: list and resume a previous session.
- `/new`: start a fresh session.
- `/voice`: pick the agent voice.
- `/exit`, `/quit`, `/q`, `Ctrl+C`, `Ctrl+Q`: quit.

## Development Commands

Run the full local gate before handing off code:

```sh
scripts/check.sh
```

That runs:

- `cargo fmt --all --check`
- `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`
- `cargo test --workspace`
- dependency assertions for the architecture

Focused commands:

```sh
cargo test -p joi-core
cargo test -p joi-providers
cargo test -p joi-app
cargo test -p joi-tui
cargo test -p joi-app --test headless
cargo clippy -p joi-core --all-targets
```

The headless proof is:

```sh
cargo test -p joi-app --test headless
```

It builds `JoiApp(MediaMode::None)` with the Mock provider and drives a full command to event loop
with no devices and no GUI.

## Debugging

- Set `JOI_TUI_LOG=/tmp/joi-tui.log` before running the TUI to put logs somewhere predictable.
- Use `RUST_LOG=joi=debug,joi_core=debug,joi_providers=debug,joi_media=debug` for verbose Rust logs.
- Native audio libraries can write directly to stderr; the TUI redirects stderr to its log file to
  avoid corrupting the alternate screen.
- If the app fails at startup with a config error, check `GEMINI_MODEL`; the code intentionally ships
  no default model.
- If Gemini auth/reachability is suspect, watch `UiEvent::Reachability` in tests/logs. The Gemini
  probe uses a token-free `models.list` request.
- For audio issues, remember the hard invariants: no allocation/blocking/DSP in cpal callbacks, feed
  AEC a far-end frame for every capture frame, tap render reference at playback output, flush
  playback on barge-in, cap render backlog.
- For resume/history issues, inspect `~/.joi/sessions/index.json` and the relevant `<uuid>.jsonl`.
  Corrupt JSONL lines are skipped by design.

## Change Discipline

- Read surrounding code before editing. Follow existing local patterns.
- Keep edits scoped. Do not refactor unrelated layers while implementing a feature.
- Preserve the dependency graph. If a change needs a forbidden dependency, the design is probably in
  the wrong crate.
- Prefer focused tests for the crate touched. Broaden to headless or full `scripts/check.sh` when a
  change crosses `joi-core`, `joi-app`, providers, media, or TUI contracts.
- For frontend/TUI work, keep reducers pure and I/O in `main.rs` / host command execution.
- For provider work, add mapping/unit tests in `joi-providers` and keep wire quirks sealed there.
- For config/settings work, maintain atomic writes, secret redaction, env precedence, and
  non-destructive provider-dependent resolution.
- For tool work, start from `doc/TOOLS_PLAN.md`; preserve the single core pipeline and keep concrete
  tool behavior sealed outside `joi-core`.
