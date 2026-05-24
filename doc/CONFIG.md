# Joi — Configuration Reference

Joi reads one config file: **`~/.joi/config.json`** (JSON). It's written with built-in defaults on
first run, so you have a starting point to edit; `config/joi.example.json` mirrors that file. A
pre-JSON `~/.joi/config` (YAML) from an older build is migrated to `config.json` automatically the
first time the new build starts (the old file is left as a backup).

## Precedence (lowest → highest)

1. Built-in defaults (in code).
2. `~/.joi/config.json` (deep-merged over the defaults — you only need the keys you change).
3. `JOI_`-prefixed environment variables, nested with `__`
   (e.g. `JOI_MEDIA__AUDIO__FRAME_MS=30` sets `media.audio.frame_ms`).
4. The conventional shortcuts `GEMINI_API_KEY` and `GEMINI_MODEL`.

**Environment always wins over the file.** One exception, by design: the system prompt in
`~/.joi/prompt.md` (when present and non-blank) overrides `live_api.gemini.system_instruction`.

## Two things Joi never persists

- **The API key.** It is held as a redacting secret and is *never* written back to disk — set
  `GEMINI_API_KEY` in your environment (preferred), or put it in the file knowing Joi will strip it
  on the next write. Every config write goes through an atomic temp-write + rename, so a crash can't
  corrupt the file.
- **The system prompt.** It lives in `~/.joi/prompt.md`, not in the config (see below).

## Fields

### `live_api`

| Field | Type | Default | Notes |
|---|---|---|---|
| `provider` | `gemini` \| `mock` | `gemini` | `mock` is for tests/headless only. |
| `reachability_probe_secs` | u64 | `20` | Cadence of the token-free reachability probe; `0` disables periodic polling (startup + on-demand probes still run). |

### `live_api.gemini`

| Field | Type | Default | Notes |
|---|---|---|---|
| `model` | string | *(none — required)* | Bare Live model name, no `models/` prefix (e.g. `gemini-3.1-flash-live-preview`). Joi ships no default; set it here or via `GEMINI_MODEL`. |
| `api_key` | string | `""` | Prefer `GEMINI_API_KEY` in the environment. Never persisted by Joi (see above). |
| `voice` | string \| null | `Aoede` | Prebuilt voice. Widely available: Aoede, Charon, Fenrir, Kore, Puck, Leda, Orus, Zephyr. Unknown names fall back to the model default. |
| `system_instruction` | string | *(persona)* | Overridden by `~/.joi/prompt.md` when that file exists (the persona is bootstrapped into `prompt.md` on first run — edit the file, not this field). |
| `input_transcription` | bool | `true` | Render the user transcript (FR-3). |
| `output_transcription` | bool | `true` | Render the agent transcript (FR-3). |
| `context_window_compression` | bool | `true` | Server-side sliding-window compression. On = sessions aren't capped at the provider's default duration limits (15 min audio / 2 min audio+video); the oldest in-session turns are truncated as the live window fills. History on disk is unaffected. |
| `token_budget` | u32 | `117964` | History re-seed budget, in tokens — the Live session's **input** window for this provider/model (not the 1M text-model window). Provider-dependent, hence under the provider. Default = 90% of Gemini's 128k Live window. Min 1000. |

### `history`

| Field | Type | Default | Notes |
|---|---|---|---|
| `dir` | path \| null | `null` → `~/.joi/sessions` | Per-session `<uuid>.jsonl` logs + `index.json`. |

### `logging`

| Field | Type | Default | Notes |
|---|---|---|---|
| `level` | `error`\|`warn`\|`info`\|`debug`\|`trace` | `info` | `RUST_LOG` overrides. |
| `file` | path \| null | `null` → `~/.joi/logs/joi.log` | |

### `media.audio`

The wire sample rates are fixed by the provider (16 kHz mono in / 24 kHz mono out) and not
configurable; the pipeline resamples your device's own rate to/from them.

| Field | Type | Default | Notes |
|---|---|---|---|
| `frame_ms` | u32 | `20` | Mic frame size; 20 ms = 320 samples @ 16 kHz. Validated 5–60. |
| `input_device` | string | `default` | `"default"` follows the OS default mic; an exact name pins a device (bypass a virtual/processed default). |
| `output_device` | string | `default` | Same, for playback. |
| `echo_cancellation` | bool | `true` | Subtract Joi's own playback from the mic (AEC3). |
| `noise_suppression` | bool | `true` | High-pass filter + noise suppression. |
| `auto_gain` | bool | `true` | Automatic gain control (AGC2). |

Turn the conditioning off when an OS/server APM (e.g. PipeWire's echo-cancel source) already
conditions the input, to avoid double-processing.

### `media.screen`

| Field | Type | Default | Notes |
|---|---|---|---|
| `fps` | f32 | `1.0` | Validated `(0, 60]`. |
| `max_width` | u32 | `768` | Gemini Live downsamples each frame to ~768 px (one tile); more is wasted bytes. |
| `quality` | u8 | `80` | JPEG quality, 1–100. |

### `ui.terminal` (read by `joi-tui`)

| Field | Type | Default | Notes |
|---|---|---|---|
| `background` | string | `transparent` | `"#rrggbb"` or `transparent` (use the terminal's own background). |
| `accent` | string | `#9aede4` | Accent color (hex or named). |

## Runtime-editable settings

A subset of these fields can be changed at runtime through the app (see `doc/SETTINGS.md`), which
validates the change, persists it to `config.json` atomically, and applies it. Each editable field
has an *apply timing*: immediate, on the next session connect, or on restart. Everything else is
file/env only.
