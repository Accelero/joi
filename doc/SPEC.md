# Joi — Functional Specification

> **What this is.** This document defines **what Joi must be able to do** — its capabilities,
> requirements, and constraints — independent of how it is built. It is architecture-agnostic on
> purpose: no module layout, no frameworks, no wire protocols. For *how* these requirements are
> realized, see [`ARCH.md`](./ARCH.md).
>
> Requirement IDs (`FR-*` functional, `SEC-*` security) are stable handles for tracking and tests.
> `[NOW]` marks what exists today; `[LATER]` marks deferred capabilities that the design must not
> preclude.

---

## 1. What Joi is

Joi is a local, **provider-agnostic** voice + screen companion. It connects a person to a realtime
multimodal model, streams audio and screen video both ways, shows a live transcript, and
**remembers conversations** — each is persisted as a resumable session the user can list, reopen,
or branch from, with its history re-seeding the model so context survives across restarts.

Joi runs locally and connects **directly** to the chosen provider — no Joi-operated relay or server
sits in between.

## 2. Founding constraints (non-negotiable)

These shape every capability below.

- **Provider-agnostic.** Joi must not be tied to one model vendor. All realtime-provider behavior
  sits behind a single abstraction; the rest of the app behaves identically regardless of provider.
  Adding or swapping a provider must not require reworking conversation, history, or UI logic.
- **Local & private.** Conversation content, history, and the API key stay on the user's machine.
  The key travels only to the provider, never to logs, transcripts, or any third party.
- **Cost-controllable.** The user can fully disconnect the live model at will; a disconnected Joi
  incurs no streaming cost.
- **Permission-gated tools.** Tool execution is disabled by default. When deliberately enabled, model
  tool calls must pass through typed schemas, policy, sandbox roots, output caps, and non-voice
  approval for mutating/destructive actions.

---

## 3. Capabilities

### 3.1 Voice conversation `[NOW]`
- **FR-1** Hold a full-duplex spoken conversation: user audio in, agent audio out, with natural
  turn-taking.
- **FR-2** Support **barge-in**: when the user speaks during agent speech, the agent stops promptly
  and listens.
- **FR-3** Render a live **transcript** of both sides, labeled by speaker, updating as speech is
  recognized.
- **FR-4** Surface a clear **state** at all times (e.g. stopped, connecting, listening, thinking,
  speaking, reconnecting, error).

### 3.2 Audio I/O `[NOW]`
- **FR-5** Capture the system **microphone** and stream it to the model live, with conditioning
  (noise suppression / echo cancellation) so the model hears clean audio.
- **FR-6** A **mute** control that stops sending mic audio at the source, reachable in one action,
  with always-visible state.
- **FR-7** Play the model's **audio output** with low added latency, flushing immediately on
  barge-in.

### 3.3 Screen sharing as video input `[NOW]`
- **FR-8** Stream a screen to the model as live video input.
- **FR-9** **Start and stop** sharing at any time; stopping is immediate and ends in-flight frames;
  a sharing indicator is always visible.
- **FR-10** Sharing **quality** (frame rate, resolution cap, encode quality) is configurable to
  trade off cost and bandwidth.
- **FR-11** `[LATER]` Choose the **source** — a specific display, or a single application window
  rather than a whole screen.

### 3.4 Transcript display `[NOW]`
- **FR-12** Present the conversation as a live, readable transcript with clear speaker distinction
  and visible status. Partial lines update in place; finalized lines are committed.
- **FR-13** The presentation surface is not fixed to one frontend; Joi is **TUI-first** today and
  may offer additional frontends later, all driven by the same engine.

### 3.5 Session lifecycle & cost control `[NOW]`
- **FR-14** **Start / stop** the live model. Stopping fully disconnects (no streaming cost).
- **FR-15** **Pause to save cost** — disconnect the live session while preserving context — and
  **resume** later, reconnecting with that context restored so the conversation continues coherently.
- **FR-16** `[LATER]` Recover gracefully from transient connection drops, restoring the live session
  where the provider supports it and otherwise falling back to a context-restoring restart; mic/share
  state is never silently lost. Today a provider/server close is surfaced and the live session stops
  cleanly.
- **FR-17** **Stop / quit** halts the live session and microphone in one action; the active frontend
  must also tear down screen sharing when the session stops.

### 3.6 Session management & persistence `[NOW]`
- **FR-18** Conversations **persist automatically** so context survives a system restart.
- **FR-19** Each conversation is a **resumable session** with a stable identity and a human-readable
  name; the name is **auto-derived from the first user message** and can be renamed.
- **FR-20** The user can, at runtime without restarting: **list** past sessions (most-recently-active
  first), **resume** one (its history re-seeds the model so it "remembers"), or **start a new** one.
- **FR-21** Persisted history is **bounded** — never infinite. On resume, only the history that fits
  the model's input budget is re-seeded; the persisted store is sized to be re-seedable, not to grow
  without limit.
- **FR-22** History persistence is durable and corruption-tolerant: a damaged entry is skipped rather
  than failing a load, and a lost index is rebuildable from the conversation logs.

### 3.7 Connection & keys `[NOW]`
- **FR-23** The user supplies a provider API key; Joi connects **directly** to the provider and
  surfaces connection, auth, and network errors plainly.

### 3.8 Tools & memory `[NOW/LATER]`
- **FR-24** `[NOW]` The agent can call registered built-in tools (`read`, `list`, `glob`, `grep`,
  `write`, `edit`, `bash`) when tools are explicitly enabled. Calls are routed through native
  provider function calling, schema validation, core policy, sandbox roots, time/output limits, and a
  permission gate requiring deliberate, non-voice approval before mutating or destructive actions.
  `read` returns line-hash edit tags, and `edit` validates those tags against current file content
  before writing. MCP tools and stronger sandbox hardening remain follow-up work.
- **FR-25** `[LATER]` A **memory** capability lets the agent persist and recall curated long-term
  facts across conversations — distinct from raw conversation history (FR-18) and subject to the same
  permission model.

---

## 4. Security & privacy requirements

- **SEC-1** `[NOW]` **Key handling.** The API key is held redacted in memory, kept out of logs,
  transcripts, history, and any external destination, and sent only to the provider. The user can
  provide it without writing it to disk in plaintext.
- **SEC-2** `[NOW]` **Local-only data.** History and logs are stored locally; detectable secrets are
  redacted from logs.
- **SEC-3** `[NOW]` **Disabled by default.** No model-driven command or tool path is reachable unless
  `tools.enabled` is explicitly true.
- **SEC-4** `[NOW]` **Non-voice consent.** Mutating or destructive tool actions require a deliberate,
  non-spoken approval of the *resolved* action. Denied tool calls return a structured result to the
  model that identifies whether `denied_by` was `user` or `system`.
- **SEC-5** `[NOW]` **Scoped execution.** Built-in tools run against configured readable/writable
  roots, with time/output caps. `bash` is non-interactive and obvious network commands are denied by
  default. Stronger kernel isolation is a future hardening layer behind the same tool context.
- **SEC-6** `[LATER]` **Anti-spoof.** Treat all on-screen/shared content as untrusted input; the
  permission prompt is application chrome, never rendered inside shared or streamed content.

---

## 5. Configuration

Joi reads one primary config file: `~/.joi/config.json`. On first run, Joi writes a defaults file.
A pre-JSON legacy `~/.joi/config` YAML file is migrated to `config.json` once when no JSON config
exists. The system prompt is stored separately in `~/.joi/prompt.md`; a present, non-blank prompt
file overrides `live_api.gemini.system_instruction`.

Precedence, lowest to highest:

1. Built-in defaults.
2. `~/.joi/config.json`, deep-merged over defaults.
3. `JOI_` environment variables, nested with `__`.
4. `GEMINI_API_KEY` and `GEMINI_MODEL`.

The prompt file is the one documented exception to env precedence for the system instruction.

Current config surface:

| Field | Type | Default | Notes |
|---|---|---|---|
| `live_api.provider` | `gemini` \| `mock` | `gemini` | `mock` is for tests/headless. |
| `live_api.reachability_probe_secs` | u64 | `20` | `0` disables periodic polling; startup/on-demand probes still work. |
| `live_api.gemini.model` | string | none | Required. Bare Live model name, no `models/` prefix. |
| `live_api.gemini.api_key` | string | `""` | Prefer `GEMINI_API_KEY`; never persisted by Joi writes. |
| `live_api.gemini.voice` | string \| null | `null` | Model default unless explicitly set through settings. |
| `live_api.gemini.system_instruction` | string | persona | Overridden by `~/.joi/prompt.md`. |
| `live_api.gemini.input_transcription` | bool | `true` | User transcript. |
| `live_api.gemini.output_transcription` | bool | `true` | Agent transcript. |
| `live_api.gemini.context_window_compression` | bool | `true` | Provider sliding-window compression for long live sessions. |
| `live_api.gemini.token_budget` | u32 | `117964` | History re-seed budget; 90% of Gemini's 128k Live input window. Min 1000. |
| `history.dir` | path \| null | `~/.joi/sessions` | Per-session `<uuid>.jsonl` logs plus `index.json`. |
| `logging.level` | enum | `info` | `error`, `warn`, `info`, `debug`, `trace`; `RUST_LOG` may override where honored. |
| `logging.file` | path \| null | `~/.joi/logs/joi.log` | Resolved by core; the TUI logs to `$JOI_TUI_LOG` or platform state dir. |
| `media.audio.frame_ms` | u32 | `20` | Mic frame size, validated 5-60 ms. |
| `media.audio.input_device` | string | `default` | OS default mic or exact device name. |
| `media.audio.output_device` | string | `default` | OS default output or exact device name. |
| `media.audio.echo_cancellation` | bool | `true` | AEC3. |
| `media.audio.high_pass_filter` | bool | `true` | DC/rumble cleanup before denoising. |
| `media.audio.noise_suppression` | enum | `classic` | `off`, `classic`, or `ai`. |
| `media.audio.mic_boost_db` | f32 | `0.0` | Fixed digital boost before the limiter, 0-36 dB. |
| `media.audio.agc_headroom_db` | f32 | `5.0` | AGC clipping headroom, 0-20 dB. Lower is louder. |
| `media.audio.agc_max_gain_db` | f32 | `50.0` | Maximum adaptive AGC gain, 0-60 dB. |
| `media.audio.agc_initial_gain_db` | f32 | `15.0` | Initial adaptive AGC gain, 0-`agc_max_gain_db`. |
| `media.audio.agc_gain_change_db_per_sec` | f32 | `6.0` | Maximum AGC gain-change rate, 0.1-60 dB/s. |
| `media.audio.auto_gain` | bool | `true` | AGC2. |
| `media.audio.leveler_enabled` | bool | `false` | Final compressor/limiter before provider audio send. |
| `media.audio.leveler_target_rms_dbfs` | f32 | `-20.0` | Final leveler RMS target, -40 to -6 dBFS. |
| `media.audio.leveler_max_gain_db` | f32 | `18.0` | Maximum final leveler gain, 0-36 dB. |
| `media.audio.leveler_max_reduction_db` | f32 | `24.0` | Maximum final leveler reduction, 0-48 dB. |
| `media.audio.limiter_ceiling_dbfs` | f32 | `-1.0` | Final limiter ceiling, -12 to 0 dBFS. |
| `media.screen.fps` | f32 | `1.0` | Validated `(0, 60]`. |
| `media.screen.max_width` | u32 | `768` | Gemini Live's useful per-frame width. |
| `media.screen.quality` | u8 | `80` | JPEG quality, 1-100. |
| `ui.terminal.background` | string | `transparent` | `#rrggbb` or `transparent`. |
| `ui.terminal.accent` | string | `#9aede4` | Hex or named color. |
| `ui.terminal.tool_accent` | string | `#c3b6e6` | Tool name/spinner accent; separate from user accent. |
| `ui.terminal.tool_text` | string | `#9aa4b0` | Tool detail text. |
| `ui.terminal.tool_success` | string | `#8fd69f` | Successful tool status. |
| `ui.terminal.tool_denied` | string | `#d8c08a` | Denied tool status. |
| `ui.terminal.tool_failed` | string | `#d8c08a` | Failed tool status. |
| `tools.enabled` | bool | `false` | Enables model-visible tools. |
| `tools.builtins` | string[] | `[]` | Empty means the standard built-in set. |
| `tools.readable_roots` | path[] | `[]` | Empty resolves to the filesystem root, so tools may read absolute paths. |
| `tools.writable_roots` | path[] | `[]` | Empty resolves to the process launch cwd. |
| `tools.timeout_secs` | u64 | `30` | Per-call timeout. |
| `tools.max_output_bytes` | usize | `65536` | Minimum 1024. |
| `tools.network` | bool | `false` | Allows obvious network shell commands only when true. |
| `tools.permissions` | rule[] | `[]` | Ordered key/subject/action rules; first match wins. |

Runtime-editable settings are a curated subset changed through the engine and persisted atomically.
Today: `Voice` (applies on next session), `Accent` (immediate), and `Background` (immediate).
The TUI exposes voice through `/voice`; a generic settings panel is future frontend work.

---

## 6. Error handling & edge cases

- **Connection loss while running:** surface the disconnect/error clearly and stop cleanly today;
  automatic provider resume and context-restoring restart are FR-16 follow-up work.
- **Auth failure:** an explicit, actionable "invalid/expired key" path.
- **Provider session-length cap:** resume if supported, else a context-restoring restart, surfaced to
  the user.
- **Resume with empty or corrupt history:** start fresh and warn; never load partial/garbled context
  silently or crash.
- **History at budget:** prune oldest within the same write; persistence must never block the audio
  path.
- **Screen capture fails or is empty:** disable sharing with a clear reason; never send blank frames
  silently.

---

## 7. Acceptance criteria

1. A user supplies a key, starts, and holds a natural spoken conversation with working turn-taking
   and barge-in; the transcript renders live, labeled by speaker. *(FR-1–7, 12, 23; SEC-1)*
2. A user shares a screen, starts/stops at will, and Joi can describe on-screen content; quality is
   adjustable. *(FR-8–10)*
3. A user stops/pauses to cut cost (no open connection), then resumes with context intact.
   *(FR-14–15)*
4. After a full system restart, prior conversations are listed and any one can be resumed with its
   context restored; history is bounded. *(FR-18–22)*
5. Stop/quit halts the session, mic, and sharing in one action. *(FR-17)*
6. The system behaves identically across providers behind the provider abstraction, with no
   provider-specific assumption leaking into conversation, history, or UI logic. *(§2)*
7. No model-driven tool or command path is reachable while tools are disabled. When tools are enabled,
   built-in calls use native function calling, the shared permission pipeline, and scoped execution.
   *(SEC-3–5, FR-24)*
