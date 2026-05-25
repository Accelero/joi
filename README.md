# Joi

A local, provider-agnostic **voice + screen** companion. Joi connects you to a realtime multimodal
model (Gemini Live today), streams audio and screen video both ways, renders a live transcript in
the terminal, and **persists every conversation as a resumable session** — list past sessions,
resume one, or start a new one, and the history re-seeds the model so it "remembers". It runs
locally and connects directly to the provider; your key, history, and conversation stay on your
machine.

It's a Rust app, **TUI-first**: all the logic lives in an engine that any frontend can drive; the
terminal UI is the only frontend built today.

## Quickstart

```sh
export GEMINI_API_KEY=...        # your Gemini Live API key
export GEMINI_MODEL=gemini-3.1-flash-live-preview
cargo run -p joi-tui             # launch the terminal UI
```

On first run Joi writes a config to `~/.joi/config.json` (see
[`config/joi.example.json`](config/joi.example.json) for every field, and
[`doc/SPEC.md`](doc/SPEC.md#5-configuration) for the annotated reference). Sessions are stored under
`~/.joi/sessions`. The TUI writes logs to `$JOI_TUI_LOG` when set, otherwise to the platform state
directory (for example `~/.local/state/joi/joi-tui.log` on Linux).

### Keys

- **F2** start / stop · **F3** mute · **F4** screen-share · **F1** help
- **Enter** send a typed message · **PgUp/PgDn** or mouse wheel scroll · **Home/End** oldest/newest
- **/resume** list & resume a past session · **/new** start a fresh one · **/voice** pick voice · **/exit** quit
  (also **Ctrl+C** / **Ctrl+Q**)

## Layout

```
crates/
  joi-core       domain: config, history/sessions, realtime-session + UiEvent contracts,
                 the SessionManager actor, media contracts + pure DSP, metrics, connectivity
  joi-tools      built-in agent-harness tools: read/list/glob/grep/write/edit/bash
  joi-providers  RealtimeSession adapters: Gemini (vendored adk-realtime) + Mock
  joi-media      native cpal audio capture/playback + sonora APM (AEC/NS/AGC) + xcap screen
  joi-app        composition root + the Seam-A `JoiApp` API
  joi-tui        the ratatui terminal frontend
  joi-testkit    shared test doubles + the provider conformance suite
vendor/          two required patched deps (adk-realtime, sonora-aec3)
```

See [`doc/SPEC.md`](doc/SPEC.md) for what Joi must do (`FR-*`) and [`doc/ARCH.md`](doc/ARCH.md) for
how it's layered. The built-in tool harness is implemented behind disabled-by-default config; the
remaining tool roadmap is in [`doc/TOOLS_PLAN.md`](doc/TOOLS_PLAN.md).

## Development

```sh
scripts/check.sh    # fmt + clippy -D warnings + workspace tests + layering dependency assertions
```

The engine is provable headless: `cargo test -p joi-app` drives a full command→event loop with the
Mock provider and no devices.
