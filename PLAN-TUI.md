# PLAN-TUI — a native ratatui/crossterm host for JOI

> Status: **proposed**. This document is written so a fresh agent can implement the TUI end-to-end
> without prior context. Read `CLAUDE.md` (architecture principle + three-layer split) and
> `PLAN-MODULARIZATION.md` (Seam A / Seam B) first; this plan assumes both.

## 1. Goal

Build `crates/joi-tui` — a standalone terminal-UI executable (`joi-tui`) that drives the JOI engine
through **Seam A** (`joi_app::JoiApp`) and renders a full-screen TUI with **feature parity** and
**visual kinship** to the Tauri/React frontend, using **ratatui** + **crossterm**.

It is a *peer* of the Tauri shell, not a replacement: a fourth host alongside `src-tauri` (GUI),
`joi-cli` (stdin), and `joi-server` (WebSocket). It compiles and runs independently of Tauri.

### Why this is clean to add

- `JoiApp` already exposes everything a host needs: `build(Config, MediaMode)`,
  `start/stop/send_text/set_mic_muted/start_screenshare/stop_screenshare/has_api_key/ui_config`,
  and `subscribe_events() -> broadcast::Receiver<UiEvent>`. The TUI is "`joi-cli` with a real UI."
- **Crucial difference from the other headless hosts:** `joi-cli`/`joi-server` build with
  `MediaMode::None`. The TUI is a native process, so it builds with **`MediaMode::LocalDevices`** and
  gets native cpal mic capture, native playback, and xcap screen capture *for free* via `joi-media` —
  the exact media path the desktop app uses. **No audio crosses the TUI**; ratatui only renders text.
- No Seam B: the TUI links the engine in-process and consumes the `UiEvent` enum directly. There is
  **no IPC, no JSON, no ts-rs** on this path. (`UiEvent`/`AppState`/`Speaker` are plain Rust enums.)

## 2. Non-goals / constraints

- **Logic stays in the engine.** Per `CLAUDE.md`, no session/provider/DSP logic in the host. The TUI
  only folds `UiEvent`s + keystrokes into view state and renders. Anything substantive belongs behind
  a `JoiApp`/`SessionManagerHandle` method, not in the TUI.
- **No Tauri/WebKit in the dependency tree.** `joi-tui` joins the `check.sh` guard (§9). ratatui and
  crossterm pull in neither.
- **Logging must not corrupt the screen.** A ratatui app owns the alternate screen; `tracing` output
  to stdout/stderr would shred the frame. Route logs to a **file** (`tracing-appender`), default
  `~/.local/state/joi/joi-tui.log` (or `$JOI_TUI_LOG`). Never write logs to the live terminal.
- **Restore the terminal on every exit path** — normal quit, error, *and panic*. A panic hook must
  leave the alternate screen and disable raw mode before the default hook prints, or the user's shell
  is left wrecked.
- **UTF-8 / truecolor target.** Assume a modern emulator (the user's is). Use box-drawing, `❯`, `●`,
  block glyphs, and 24-bit `Color::Rgb` freely. No need for a 16-color fallback in v1 (note it as a
  later nicety in §10).

## 3. Internal architecture (inside the crate)

Keep a clean **model / update / view** split so the non-trivial parts are unit-testable without a
terminal:

```
joi-tui
├── main.rs        # tokio runtime, terminal setup/teardown, panic hook, the select! loop
├── app.rs         # AppModel: pure state + reducers (no IO, no ratatui) — UNIT TESTED
├── transcript.rs  # transcript buffer: streaming-partial folding, speaker labels, scrollback
├── input.rs       # the line editor: text + caret + edit ops (insert/backspace/move) — UNIT TESTED
├── ui.rs          # render(frame, &AppModel): all ratatui widget drawing — thin, no logic
├── theme.rs       # the minimal-mono palette as ratatui Color + symbols + animation helpers
└── keys.rs        # key event → Action mapping
```

- **`AppModel`** holds: `state: AppState`, `connection: ConnectionStatus`, `mic_muted: bool`,
  `sharing: bool`, `transcript: Transcript`, `input: Input`, `metrics: Option<MetricsSnapshot>`,
  `started_at: Option<Instant>`, `has_key: bool`, `error_banner: Option<String>`, `tick: u64` (drives
  animation phase), `should_quit: bool`.
- **Reducers are pure**: `AppModel::on_ui_event(&mut self, UiEvent)` and
  `AppModel::on_action(&mut self, Action) -> Vec<Command>` where `Command` is an *intent* the loop
  executes against `JoiApp` (e.g. `Command::Start`, `Command::SendText(String)`,
  `Command::SetMicMuted(bool)`). This keeps the IO (calling `app.start().await`) in `main.rs` and the
  decision-making in testable code.
- **The loop** (`tokio::select!` in `main.rs`) multiplexes three sources:
  1. **Input** — `crossterm::event::EventStream` (needs the `event-stream` feature) → key events.
  2. **Engine** — the `broadcast::Receiver<UiEvent>` from `app.subscribe_events()`.
  3. **Tick** — a `tokio::time::interval(~80ms)` that bumps `model.tick` so the status dot animates
     and the uptime/clock refresh, then requests a redraw.
  After handling any event, run `Command`s against `JoiApp`, then `terminal.draw(|f| ui::render(f, &model))`.
  Redraw on every wake (simplest correct approach; optimize to dirty-flag later if needed).

```rust
// shape of the loop (elided)
let mut events = app.subscribe_events();      // Option<Receiver<UiEvent>>
let mut input = crossterm::event::EventStream::new();
let mut tick = tokio::time::interval(Duration::from_millis(80));
loop {
    tokio::select! {
        Some(Ok(ev)) = input.next() => { for c in model.on_action(keys::map(ev)) { run(&app, c).await; } }
        ev = recv_ui(&mut events) => { if let Some(ev) = ev { model.on_ui_event(ev); } }
        _ = tick.tick() => { model.tick = model.tick.wrapping_add(1); }
    }
    if model.should_quit { break; }
    terminal.draw(|f| ui::render(f, &model))?;
}
let _ = app.stop(false).await;   // graceful: end the session before restoring the terminal
```

## 4. Feature-parity matrix (Tauri frontend → TUI)

| Tauri frontend (`src/`)                              | TUI equivalent                                                                 |
|------------------------------------------------------|--------------------------------------------------------------------------------|
| `Terminal.tsx` streaming transcript, `JOI:`/`User:` labels, partial-rewrite, error lines | `transcript.rs` buffer + a `Paragraph`/list in `ui.rs`; identical fold semantics |
| `Prompt.tsx` chevron `❯` + block caret + Enter/Shift+Enter | `input.rs` line editor + block-cursor render; same key behavior                |
| `Prompt.tsx` status line (colored glowing/blinking dot + state) | status line widget, per-`AppState` color + tick-driven dot animation           |
| `Controls.tsx` Start/Stop, Mute, Share icon buttons  | a control bar showing the three actions + on/off state, bound to function keys |
| `App.tsx` echo typed text into transcript            | reducer for `Command::SendText` also pushes a finalized `User:` line           |
| `App.tsx` deck header: brand + wall clock            | header line: `JOI · voice · screen companion` + `HH:MM:SS`                     |
| `App.tsx` footer: connection dot + uptime            | footer line: connection + uptime (+ metrics, see below)                        |
| backend `Metrics` event (frontend not yet showing)   | footer shows `↑/↓ kb/s · tok/s` from `UiEvent::Metrics` — small parity bonus    |
| Window controls (min/max/close), custom titlebar     | **N/A** — the terminal emulator owns the window. Quit via `Ctrl+C`/`Ctrl+Q`.   |
| `getUiConfig()` → `TerminalCfg { theme, font, … }`   | read `app.ui_config()`; map `theme` name → palette; `font` is moot in a TTY    |

### Keybindings (proposed — input is always focused, like the web prompt)

- Printable keys, `Backspace`, `←/→`, `Home/End` → edit the input line.
- `Enter` → send (if non-empty & session live); `Shift+Enter`/`Alt+Enter` → newline.
- `F2` → start/stop toggle · `F3` → mute toggle · `F4` → screen-share toggle. (Function keys never
  collide with typed text; avoid `Ctrl+M`, which is `Enter` in terminals.)
- `PageUp`/`PageDown` → scroll transcript; `End` at empty input → jump to latest (autoscroll on new).
- `Ctrl+C` / `Ctrl+Q` → quit (stops the session, restores the terminal).
- `F1` → toggle a help/keys overlay (nice-to-have, M6).

## 5. Visual style (minimal-mono / refined HUD, ported to a TTY)

Reuse the exact palette from `src/index.css` / `Terminal.tsx`, as ratatui `Color::Rgb`:

| Token        | RGB        | Use                                              |
|--------------|------------|--------------------------------------------------|
| base         | `#07090c`  | app background (set via a full-area block)       |
| panel        | `#090c11`  | transcript panel background                      |
| line         | `#44505f`  | borders / corner brackets                        |
| line-soft    | `#2c3542`  | separators / dim rules                           |
| fg           | `#f7f9fb`  | agent text, primary labels                       |
| fg-dim       | `#cdd6e0`  | body text                                        |
| fg-faint     | `#9aa4b0`  | placeholders, footer secondary, `stopped` dot    |
| accent       | `#9aede4`  | user label/text, `listening`, primary action     |
| danger       | `#e08c8c`  | errors, `error` state, muted indicator           |
| warn         | `#d8c08a`  | `connecting`/`reconnecting`                       |
| speak        | `#c3b6e6`  | `speaking` state                                  |
| think        | `#93b2d6`  | `thinking` state                                  |

Per-`AppState` status mapping (mirror `Prompt.tsx`'s `STATUS` map): `stopped`→fg-faint/steady,
`connecting`→warn/blink, `listening`→accent/soft-glow, `thinking`→think/soft-glow(faster),
`speaking`→speak/active-pulse, `reconnecting`→warn/blink, `error`→danger/steady.

**Animation without CSS:** drive it off `model.tick`. A "glow" is a sine/triangle ramp of the dot's
brightness (lerp the RGB toward base and back) keyed on `tick`; a "blink" toggles the glyph
(`●`/`◌` or visible/space) on a tick period. Speaking pulses faster than listening. Keep it subtle.

**Frame / chrome:** a full-screen `Block` with rounded borders (`BorderType::Rounded`) in `line`,
plus corner-bracket accents (`⌜⌝⌞⌟` or custom border symbols) to echo the deck's `.deck-corner`
brackets. A `transcript` panel label sits on the inner top border. Use `❯` for the prompt and a
reverse-video cell or `▮` for the block caret. Layout sketch:

```
⌜ JOI · voice · screen companion ───────────────────────────── 14:53:21 ⌝
│  ▶ F2 start    ● F3 mute    ▣ F4 share                                 │
├ transcript ────────────────────────────────────────────────────────────┤
│ JOI: hey, what are you working on?                                      │
│ User: wiring up the tui                                                 │
│                                                                         │
│ ● listening                                                             │
│ ❯ message JOI▮                                                          │
├──────────────────────────────────────────────────────────────────────┤
⌞ ● connected                              ↑00:04:12   ↑12.3 ↓ 4.1 kb/s ⌟
```

## 6. Crate setup

`crates/joi-tui/Cargo.toml`:

```toml
[package]
name = "joi-tui"
description = "Native terminal-UI host for the JOI engine (ratatui/crossterm). No Tauri."
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[lints]
workspace = true

[[bin]]
name = "joi-tui"
path = "src/main.rs"

[dependencies]
joi-app  = { path = "../joi-app" }
joi-core = { path = "../joi-core" }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "time", "sync"] }
ratatui = "0.29"
crossterm = { version = "0.28", features = ["event-stream"] }
futures-util = "0.3"          # StreamExt for crossterm's EventStream
anyhow = "1"
tracing = { workspace = true }
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender = "0.2"      # non-blocking file logging (keep logs off the screen)
directories = { workspace = true }  # resolve the state/log dir

[dev-dependencies]
joi-testkit = { path = "../joi-testkit" }
```

Add `"crates/joi-tui"` to the `[workspace] members` in the root `Cargo.toml`.

> Note the workspace lints deny `unwrap`/`expect`/`panic` in non-test code — applies here too.
> Tests may re-allow via the usual `#![cfg_attr(test, allow(...))]`.

## 7. Milestones

Each milestone is independently compilable and verifiable. Verify TUI behavior with **tmux**
(`tmux new -d`, `send-keys`, `capture-pane -p`) per the `run` skill's TUI recipe — launch it, drive a
key, capture the pane, and *read* the result. Always confirm the terminal is restored after quit.

### M0 — Scaffolding & headless bring-up
**Goal:** the crate exists, builds, and can construct + drive the engine with no UI yet.
- Create the crate (§6), add to workspace members and to the `check.sh` guard loop (§9).
- `main.rs`: `Config::load(None)`, `JoiApp::build(config, MediaMode::LocalDevices)`, log
  `has_api_key()`, init `tracing-appender` to the log file. Spawn a task that drains
  `subscribe_events()` to the log. Then `app.start(false).await`, sleep briefly, `app.stop(false)`.
- **Accept:** `cargo build -p joi-tui` clean; running it connects (watch the log file show
  `connection`/`state` events) and exits cleanly; `RUSTFLAGS="-D warnings" cargo clippy -p joi-tui`
  clean. No screen output yet.

### M1 — Terminal scaffold & event loop
**Goal:** a stable full-screen app with safe teardown and the three-source loop.
- Terminal setup: `enable_raw_mode`, `EnterAlternateScreen`, `ratatui::init()` (or manual
  `Terminal::new(CrosstermBackend)`). Teardown helper restores both. Install a **panic hook** that
  runs teardown before chaining the previous hook.
- Implement the `tokio::select!` loop over `EventStream` + `UiEvent` receiver + 80ms tick. `Ctrl+C`/
  `Ctrl+Q`/`F10` sets `should_quit`. Draw an empty bordered deck frame + header with the live clock.
- **Accept (tmux):** launch in tmux; `capture-pane` shows the framed deck + ticking clock; `send-keys
  C-c` exits and the pane returns to a clean shell prompt (terminal fully restored, no raw-mode
  residue). Force a `panic!` behind a debug key and confirm the shell is still usable after.

### M2 — Transcript view
**Goal:** streaming transcript with parity semantics.
- `transcript.rs`: a buffer of finalized lines + one "open" line per current speaker. Port
  `Terminal.tsx`'s fold: on `UiEvent::Transcript`, if the speaker changed, commit the open line and
  start a new labeled one (`JOI:` / `User:`); append the text delta; on `final`, commit. `UiEvent::
  Error` commits any open line then pushes a `! message` line in `danger`. Cap to `scrollback`.
- `ui.rs`: render as a `Paragraph` (wrapping) or a `List`, with speaker-colored labels (user→accent,
  agent→fg). Autoscroll to bottom on new content; `PageUp/PageDown` scroll with a scroll offset in
  the model; new content while scrolled-up does not yank the view (show a "new ↓" hint, optional).
- **Unit test** the fold (speaker switch, partial→final, error interleave) against `transcript.rs`
  with no terminal.
- **Accept (tmux):** start a session, speak/inject; `capture-pane` shows labeled, colored, wrapping
  transcript lines that grow in place then commit.

### M3 — Input prompt + send
**Goal:** the `❯` prompt with a block caret that sends text.
- `input.rs`: a single-logical-line (multiline-capable) editor — value + caret; ops: insert char,
  backspace/delete, `←/→`, `Home/End`, `Shift/Alt+Enter` newline. Pure + unit-tested.
- `ui.rs`: render `❯ ` + the text; draw the block caret as a reverse-video cell at the caret index
  (or set the real cursor position via `frame.set_cursor_position` + a block cursor shape). Show the
  `message JOI…` placeholder in `fg-faint` when empty.
- `Enter` → `model.on_action(Send)` returns `Command::SendText(text)` **and** pushes a finalized
  `User:` line into the transcript (parity with `App.tsx`'s echo, since the backend doesn't
  round-trip typed text). The loop calls `app.send_text(&text)`.
- **Accept (tmux):** `send-keys` a message + `Enter`; pane shows the `User:` echo immediately and the
  agent's reply streams beneath it; backspace/arrows behave.

### M4 — Status line, controls & footer
**Goal:** lifecycle, actions, and connection/uptime readouts.
- Status line above the prompt: `● <state>` colored per `AppState`, dot animated per §5 off
  `model.tick`. State updates come from `UiEvent::State`.
- Control bar (below header): three items `▶/■ start/stop`, `● mute`, `▣ share`, each showing
  on/off via color/glyph. Bind `F2/F3/F4` → `Command::Start|Stop`, `SetMicMuted(!muted)`,
  `Start|StopScreenshare`. Mirror `App.tsx`: on `state==stopped|error`, force `sharing=false` and
  emit `StopScreenshare`. Disable share visually unless a session is live.
- Footer: connection (`● <status>` colored via the `ConnectionStatus` map) + uptime
  (`started_at` set when entering a running state, like `App.tsx`) + metrics from `UiEvent::Metrics`
  (`↑/↓ kb/s · tok/s`), formatted compactly.
- **Accept (tmux):** `F2` starts (dot→connecting→listening, uptime ticks); `F3` toggles the mute
  glyph and actually mutes (verify via log `set_mic_muted`); `F4` toggles share; footer shows live
  connection + metrics.

### M5 — Style pass (minimal-mono / refined HUD)
**Goal:** make it look like the deck, within TTY limits.
- `theme.rs`: the palette as `Color::Rgb`, the per-state status map, and `glow(tick, base)` /
  `blink(tick)` helpers. Apply `base`/`panel` backgrounds, `line` borders with corner brackets, the
  `transcript` panel label, brand header styling, and the `❯`/caret/dot glyphs. Tune animation
  periods to match `Prompt.tsx` (listening calm ~2s, speaking faster ~0.85s).
- Map `TerminalCfg.theme` (`app.ui_config().terminal.theme`, default `joi-dark`) to the palette;
  unknown names fall back to the dark palette. `font` is irrelevant in a TTY (document that).
- **Accept:** side-by-side screenshot/capture against the Tauri window reads as the same product —
  same palette, same status vocabulary, same chevron/dot language.

### M6 — Robustness & polish
**Goal:** production-quality edges.
- **Resize:** crossterm `Event::Resize` → redraw (ratatui handles layout; just trigger a draw).
- **No-API-key path:** `subscribe_events()` returns `None` → show a persistent banner ("no API key —
  set `GEMINI_API_KEY` or `live_api.gemini.api_key`"), keep the UI usable, disable start.
- **Graceful shutdown:** on quit, `app.stop(false).await` before terminal teardown (already in the
  loop tail — verify it runs on every exit path, including `Ctrl+C`).
- **Error states:** `UiEvent::Error` → transcript line + transient footer flash; `AppState::Error`
  status styling.
- `F1` help overlay listing keybindings. Optional: mouse scroll for the transcript.
- **Accept:** kill the network mid-session (reconnecting state renders), resize aggressively (no
  panic, layout reflows), run with no key (banner shows), quit always restores the terminal.

## 8. Testing strategy

- **Pure units (no terminal):** `transcript.rs` fold semantics, `input.rs` edit ops, `app.rs`
  reducers (`on_ui_event` state transitions; `on_action` → `Command` mapping incl. the typed-text
  echo and the stop-clears-sharing rule). These are the only places with logic — cover them.
- **Smoke (engine):** a test that builds `JoiApp` in `MediaMode::None` with a `joi-testkit` fake
  factory and asserts the loop's reducer reaches `listening` on the fake's events — proves the wiring
  without a real provider or devices. (Keep the device-driven path out of CI.)
- **Manual/agent E2E:** the tmux drive-and-capture flow in §7 per the `run` skill.
- ratatui offers `TestBackend` + buffer assertions; use it for one golden-frame test of `ui::render`
  against a fixed `AppModel` if cheap, but don't over-invest — the model/reducers are the contract.

## 9. CI / quality gate

- Add `joi-tui` to the host-agnostic guard in `scripts/check.sh` (the `for crate in joi-app joi-cli
  joi-server` loop → add `joi-tui`). It must show **no** `tauri`/`webkit` in `cargo tree`.
- `cargo fmt`, `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`, and
  `cargo test --workspace` already cover the new crate once it's a workspace member — confirm green.
- No ts-rs/bindings impact (the TUI doesn't cross Seam B), so the `src/bindings` drift check is
  unaffected.

## 10. Risks & open questions

- **`MediaMode::LocalDevices` device contention:** if a desktop instance and the TUI run at once they
  compete for the mic/speaker. Acceptable for v1 (don't run both); note it.
- **Logging discipline:** any stray `println!`/stdout `tracing` will corrupt the frame. Audit that
  the subscriber writes only to the file appender; set a non-blocking writer so logging never stalls
  the render loop.
- **Broadcast lag:** `subscribe_events()` is a `broadcast` channel; under a burst the receiver can
  see `RecvError::Lagged`. Handle it (skip + log) rather than treating it as fatal.
- **Block-cursor approach:** prefer the real terminal cursor (`set_cursor_position` + block shape) for
  correctness over a faked reverse-video cell; fall back to the fake cell only if positioning across
  wrapped input proves fiddly.
- **16-color fallback / `NO_COLOR`:** out of scope for v1 (target is truecolor). Note as a follow-up.
- **Open question:** should `Esc` clear the input or be a no-op? Proposed: clear input; `Esc` twice
  with empty input does nothing. Confirm with the user during M3 if it matters.

## 11. Definition of done

`joi-tui` launches into a minimal-mono deck, connects to Gemini Live with native voice (mic in,
audio out) and screen-share, streams labeled transcripts, accepts typed input, shows the animated
lifecycle status + connection/uptime/metrics, restores the terminal on every exit, passes
`./scripts/check.sh`, and is registered as a workspace member + in the host-agnostic guard.
