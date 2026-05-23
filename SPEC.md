# Joi — Technical Specification

> Companion to [`DESIGN.md`](./DESIGN.md). DESIGN.md says **what** Joi is and **why** the big
> choices were made. This SPEC.md says **how** to build it and defines **what she must be able
> to do**, down to interfaces, message schemas, state machines, and acceptance criteria.
>
> When this document and DESIGN.md disagree, DESIGN.md wins on intent; raise the conflict and
> update both. The four hard requirements in DESIGN §2 (provider-agnostic, native S2S, gated
> local shell, portable tools) are non-negotiable and constrain everything below.

---

## 0. Status & scope

- **App shell:** Tauri v2 (Rust backend + system webview). Confirmed.
- **Frontend framework:** ✅ **React** (TypeScript) — decided 2026-05-21 for its best-in-class
  modern-UI ecosystem (shadcn/ui + Aceternity/Magic UI + Motion) and zero media/xterm.js
  friction. See §9 for the rationale and §8 for the UI stack.
- **Provider in MVP:** Gemini Live (native audio) only, behind the provider abstraction (§4).
- **Platforms:** Linux first (dev target), then macOS, then Windows.

### 0.1 What is in the MVP

The MVP is a **voice + screen + terminal-UI** companion with a **start/stop/resume** lifecycle
and **persistent context**:

- Full-duplex voice conversation (mic in, audio out, turn-taking, barge-in).
- System **microphone** capture with a **mute** control.
- **Audio output** from the live model.
- **Screen capture** as live video input to the model: choose a screen/source, start/stop,
  quality settable (default: native / the max the API accepts at its max frame rate).
- A **terminal-style web UI** (web terminal emulator) with stylish ANSI colorization that shows
  the model's **text output** / transcript.
- The live model is **start / stop / resume**-able so the user can **pause to save API cost**.
- **History persisted to disk** so a session's **context survives a system restart** and can be
  restored. History is bounded (not infinite) — see §6.

### 0.2 What is explicitly out of the MVP (but must not be designed against)

- **Tool calls of any kind**, including the permission-gated `bash`/shell tool. The MVP carries
  **no tools**, but the architecture must let tools — and the permission gate + sandboxed exec —
  be added later **without rewrites** (§10). This is the one place we invest in seams now and
  features later.
- **Application-window capture** (capture a single app rather than a whole screen). Screen
  capture only in MVP; the capture abstraction must allow app capture later (§7.3).
- **Multiple named sessions.** MVP persists **one** resumable conversation. The persistence
  layer must not preclude multiple sessions later (§6.4).
- **OpenAI Realtime adapter** (real). The abstraction is built and a stub compiles, but only the
  Gemini adapter is functional (§4.4).
- **Memory tool.** The *first tool we add after MVP* (§10.4) — distinct from history (§6).

Requirement IDs (`FR-*` functional, `NFR-*` non-functional, `SEC-*` security) are stable handles
for tracking and tests. `[MVP]` / `[POST]` mark milestone scope.

---

## 1. Glossary

| Term | Meaning |
|---|---|
| **Session** | One live connection to a realtime provider, from `connect` to `close`. |
| **Conversation** | The persisted dialogue that can outlive any single session (restored on resume). |
| **Turn** | One side speaking until it yields. Turns can be interrupted (barge-in). |
| **Adapter** | Provider-specific implementation of `RealtimeSession`. |
| **Pause / Resume** | Fully disconnect the live session (no streaming cost) and later reconnect with restored context. |
| **Context window** | The model's max input token budget; the bound for how much history we persist (§6). |
| **History** | Persisted conversation content needed to *restore context* across restart/pause. |
| **Memory** | Long-term, agent-curated facts written/read via a tool (post-MVP). Not the same as history. |
| **Provider session resumption** | A provider's short-lived token to reconnect a dropped socket (transient only). |
| **Tool call** | Model-emitted request to run a registered function. Post-MVP. |
| **Gate** | Permission system approving/denying a tool call before execution. Post-MVP. |
| **Terminal emulator** | Web component (e.g. xterm.js) rendering the model's text output with ANSI styling. |
| **Barge-in** | User starts speaking while the agent is talking; agent yields. |

---

## 2. System architecture

Two processes inside one Tauri app, plus the provider over the network.

```
┌─ Webview (frontend, framework TBD §9) ─┐     ┌─ Rust backend (core) ─────────────────┐
│ media-in:  mic capture (+ mute)         │ IPC │ lifecycle: start/stop/pause/resume FSM  │
│ media-in:  screen capture (source pick) │◄───►│ session:   RealtimeSession + adapter    │
│ media-out: audio playback (jitter buf)  │     │ history:   bounded context store (disk) │
│ ui:        terminal emulator (xterm-ish) │     │ media:     audio framing, screen frames │
│            + controls/settings          │     │ secrets:   OS keychain (API key)        │
│ store:     UI state, non-secret settings │     │ [POST] tools: registry · gate · exec    │
└─────────────────────────────────────────┘     │ log:       structured event log         │
                                                  └────────────────┬───────────────────────┘
                                                                   │ direct WebSocket (BYOK)
                                                           ┌───────▼────────┐
                                                           │  Gemini Live    │
                                                           └────────────────┘
```

**Placement rule (DESIGN §4):** the realtime connection, agent loop, history/persistence,
secrets, and (later) tools/gate/exec live in **Rust**. The webview only renders the terminal UI +
controls. No API key, conversation logic, or command string is ever decided in the webview.

> **Implementation note (current).** Media is **native** now — capture/playback/screen are in
> `crates/joi-media`, not the webview (see §7). The codebase is split into three independently-
> compilable layers (`PLAN-MODULARIZATION.md`): the **JOI engine** (`joi-core`/`joi-providers`/
> `joi-media`/`joi-app`, host-agnostic, no Tauri) exposing **`JoiApp`** (Seam A); the **Tauri
> backend** (`src-tauri`) as a thin adapter; and the **web frontend** (`src/`). The IPC below is
> Seam B. The diagram above is the original design sketch; the API key lives in config (§13, SEC-5),
> not a keychain.

### 2.1 Backend module layout

```
src-tauri/src/
  main.rs              # Tauri bootstrap, IPC command + event wiring
  lifecycle.rs         # start/stop/pause/resume state machine (§5)
  session/
    mod.rs             # RealtimeSession trait, SessionConfig, SessionEvent (§4)
    gemini.rs          # GeminiAdapter (§4.3)
    openai.rs          # OpenAI stub — compile-only in MVP (§4.4)
  history/
    mod.rs             # conversation model, bounded store, restore-to-context (§6)
    store.rs           # on-disk persistence (append + prune)
  # media is native and lives in crates/joi-media (cpal capture/playback, xcap screen);
  # pure DSP (framing, resample, jitter buffer) is in crates/joi-core::media (§7).
  # the API key is part of Config (live_api.gemini.api_key, from YAML/env) — see §13, SEC-5.
  log.rs               # structured event log
  ipc.rs               # serde IPC types shared in shape with frontend (§11)
  tools/               # [POST] mod.rs (Tool trait, registry), bash.rs, memory.rs
  gate/                # [POST] permission engine, rules, allowlist store
  exec/                # [POST] ExecEndpoint trait, local jail
```

### 2.2 Frontend layout (React + TypeScript; UI stack in §8)

The frontend is **UI only** — no media (§7, §8.2). All capture/playback/screen lives in native Rust
(`crates/joi-media`).

```
src/
  main.tsx             # React bootstrap, IPC bridge
  App.tsx              # composition: calls commands.start({resume}), renders UiEvents
  components/
    Terminal.tsx       # xterm.js — model text output, ANSI theming (§8)
    Controls.tsx       # start/stop, mic mute, screenshare toggle, status
  ipc.ts               # typed command wrappers + onUiEvent listener (JSON only, §11)
```
(`Settings.tsx` and a UI store are `[POST]`; API-key entry currently rides on the `set_api_key`
command.)

---

## 3. Capabilities — what Joi must be able to do

### 3.1 Voice conversation `[MVP]`
- **FR-1** Hold a full-duplex spoken conversation: user audio in, agent audio out, with natural
  turn-taking driven by the provider's VAD.
- **FR-2** Support **barge-in**: when the user speaks during agent speech, the agent stops
  promptly (target < 300 ms from detected speech to playback halt) and listens.
- **FR-3** Render a live **transcript** of both sides in the terminal UI, labeled by speaker.
- **FR-4** Surface a clear **state** at all times: `stopped`, `connecting`, `listening`,
  `thinking`, `speaking`, `reconnecting`, `error` (and later `tool-running`, `awaiting-approval`).

### 3.2 Audio I/O `[MVP]`
- **FR-5** Capture the **system microphone** and stream it to the model live.
- **FR-6** **Mute** control that stops sending mic audio at the source (native engine + manager),
  reachable in one action; state always visible.
- **FR-7** Play the model's **audio output** with low added latency and immediate flush on
  barge-in.

### 3.3 Screen capture as video input `[MVP]`
- **FR-8** Stream a chosen screen to the model as low-frame-rate video input.
- **FR-9** **Choose the source** (which screen/display) before/while sharing.
- **FR-10** **Start and stop** sharing at any time; stopping is immediate and revokes in-flight
  frames; a sharing indicator is always visible.
- **FR-11** **Quality is settable** (frame rate, resolution cap, encode quality). Default policy:
  go **native / the maximum the API accepts at its max supported frame rate**, clamped by a
  configurable ceiling for cost/bandwidth.
- **FR-12** `[POST]` **Application-window capture** (single app instead of full screen). The
  capture abstraction (§7.3) must accommodate this without reworking the pipeline.

### 3.4 Terminal UI `[MVP]`
- **FR-13** Display the model's **text output / transcript** in a **web terminal emulator** with
  **stylish ANSI colorization** (speaker colors, status lines). See §8.
- **FR-14** The terminal is the primary readable surface; controls (start/stop/pause/resume,
  mute, share) are always reachable.

### 3.5 Lifecycle & cost control `[MVP]`
- **FR-15** **Start / stop** the live model. Stop fully disconnects (no streaming cost).
- **FR-16** **Pause to save cost**: disconnect the live session while preserving context.
- **FR-17** **Resume**: reconnect and restore the prior context so the conversation continues
  coherently (§5, §6).
- **FR-18** Reconnect gracefully on transient drops via provider session resumption where
  available; fall back to a context-restoring restart (§5.2).

### 3.6 Persistence `[MVP]`
- **FR-19** Persist conversation **history to disk** continuously so context **survives a system
  restart** and can be restored on next launch.
- **FR-20** History is **bounded** — at least the model's context window, never infinite (§6).

### 3.7 Connection & keys `[MVP]`
- **FR-21** First-run: user pastes a Gemini API key; stored in the **OS keychain**, never in
  plaintext or webview storage.
- **FR-22** Connect **directly** to the provider over WebSocket (no relay); show connection
  status; surface auth/network errors plainly.

### 3.8 Tools `[POST]`
- **FR-23** `[POST]` The agent can call registered tools, routed through a permission gate, with
  the first real tool being permission-gated shell access (DESIGN §7). MVP ships none, but the
  seam (§10) exists.
- **FR-24** `[POST]` **Memory tool** — the agent can persist and recall curated long-term facts
  to learn/adapt across conversations (§10.4).

---

## 4. Provider abstraction layer (DESIGN §6.1)

The agnostic seam. App logic (lifecycle, history, terminal UI) talks **only** to this trait,
never to a provider SDK directly. This is the founding constraint (DESIGN §2.1) and is
independent of which Rust library backs the Gemini adapter (§4.5).

```rust
#[async_trait]
pub trait RealtimeSession: Send {
    async fn connect(&mut self, cfg: SessionConfig) -> Result<()>;

    // Inbound to provider
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<()>;          // 16 kHz mono
    async fn send_video_frame(&mut self, frame: &VideoFrame) -> Result<()>;
    async fn send_text(&mut self, text: &str) -> Result<()>;
    async fn send_tool_result(&mut self, id: ToolCallId, r: ToolResult) -> Result<()>; // [POST]

    // Outbound — one ordered event stream, taken ONCE after connect (see note)
    fn take_events(&mut self) -> EventReceiver;       // owned receiver (e.g. mpsc::Receiver<SessionEvent>)

    async fn close(&mut self) -> Result<()>;
    fn capabilities(&self) -> Capabilities;          // feature flags, never assumed by app logic
}
```

> Design note: DESIGN §6.1 listed per-callback setters (`on_audio_output`, …). This spec
> **consolidates them into one ordered `SessionEvent` stream** — same events, ordering preserved
> (transcript-before-turn-end), easier to test, no callback re-entrancy. The stream is handed back
> as an **owned receiver taken once at/after `connect`** (`take_events`), *not* a borrow off
> `&mut self`: a borrowed stream would alias the session and forbid calling `send_*(&mut self)`
> while reading events. The `SessionManager` is an actor that owns the session and `select!`s over
> this receiver and an inbound command channel (see PLAN §6). Adapters bridge provider
> messages/callbacks into this receiver.

```rust
pub enum SessionEvent {
    AudioOutput { pcm: Vec<i16> },                   // 24 kHz mono → playback
    Transcript { speaker: Speaker, text: String, final_: bool },
    TurnEvent(TurnEvent),                            // TurnStarted | TurnComplete | Interrupted
    ToolCall { id: ToolCallId, name: String, args: serde_json::Value }, // [POST]
    SessionResumptionUpdate { handle: String },      // for transient reconnect (§5.2)
    Error(SessionError),
    Closed { reason: CloseReason },
}

pub struct SessionConfig {
    pub model: String,
    pub system_instruction: String,
    pub voice: Option<String>,
    pub input_audio: AudioFormat,                    // 16 kHz mono PCM16
    pub output_audio: AudioFormat,                   // 24 kHz mono PCM16
    pub enable_input_transcription: bool,
    pub enable_output_transcription: bool,
    pub initial_context: Vec<HistoryTurn>,           // restore-on-resume seed (§6)
    pub resumption_handle: Option<String>,           // transient reconnect (§5.2)
    pub tools: Vec<ToolSchema>,                       // [POST] empty in MVP
}

pub struct Capabilities {
    pub session_resumption: bool,
    pub native_screen_input: bool,
    pub async_tool_calls: bool,                      // [POST] Gemini NON_BLOCKING; OpenAI=false
}
```

The adapter absorbs every divergence (DESIGN §6.1): audio formats, session/turn config,
VAD/interruption semantics, session length & resumption, and (later) tool-call schema. App code
must compile and behave identically against any adapter that honors the trait.

### 4.3 GeminiAdapter `[MVP]`
- Connects to a Gemini Live native-audio model (e.g. `gemini-live-2.5-flash-native-audio`) over
  WebSocket, BYOK.
- Maps Gemini bidi messages ↔ `SessionEvent` / trait calls. Input 16 kHz PCM16 mono, output
  24 kHz (DESIGN §6.3).
- Emits `SessionResumptionUpdate` handles and accepts `resumption_handle` for transient
  reconnects.
- Surfaces `async_tool_calls: true` but app logic must not depend on it (DESIGN §6.4).

### 4.4 OpenAIAdapter `[POST]` (compile-only in MVP)
- Implements the trait signature, returns `Err`/`unimplemented!` at runtime,
  `async_tool_calls: false`. Its MVP purpose is to **keep the abstraction honest** — the
  workspace must build with it present, proving no Gemini-ism leaked into app logic.

### 4.5 Decision: realtime SDK — **adk-rust** ✅ (decided 2026-05-21)

**Decision: build the realtime layer on `adk-rust` (zavora-ai), v0.8.x, Apache-2.0.** Our own
`RealtimeSession` trait (§4) remains the real abstraction boundary — adk-rust is an
implementation detail *inside* `GeminiAdapter` (and later `OpenAIAdapter`), so churn risk is
contained and the founding constraint stays ours, not a dependency's.

**Why (from the SDK landscape survey).** Realtime-capable Rust SDKs are a small, specialized
field — distinct from the popular *text/tool-calling* frameworks (Rig, AutoAgents, OpenFANG),
**none of which support bidirectional audio Live APIs**. Among the realtime-capable set, the
intersection of Joi's three founding constraints — **realtime S2S + provider-agnostic + a path to
tools/memory** — is met by exactly one SDK:

| | **adk-rust** (chosen) | gemini-rs (vamsiramakrishnan) | roll-our-own (raw WS) |
|---|---|---|---|
| Providers | **Gemini Live + OpenAI Realtime** (+ Vertex, LiveKit) | Gemini only | whatever we build |
| Realtime S2S | ✅ native audio, bidi | ✅ native audio, VAD, barge-in | ✅ (we implement it) |
| Agent layer (tools/memory) | ✅ built in (helps M6/M7) | ✅ agent runtime + fluent | ✗ we build it |
| Provider-agnostic for free? | **Yes** (matches DESIGN §2.1; OpenAI adapter rides same crate) | No — we build the seam (we do anyway, §4) | No |
| Maturity / license | v0.8.4, Apache-2.0, pre-1.0 "production-ready" | v0.6.0, MIT, very young (1★) | n/a |
| Main cost | framework coupling + pre-1.0 churn | Gemini-locked; more glue for OpenAI later | most work; we own wire/VAD/resumption/codecs |

adk-rust uniquely satisfies all three constraints and gives the future OpenAI adapter nearly for
free. The accepted cost — coupling to a pre-1.0 framework — is mitigated by wrapping it behind our
own trait in `session/gemini.rs`.

**Fallback (no rewrite of app logic):** if adk-rust's Gemini native-audio path proves leaky or too
heavy, swap the adapter internals to **gemini-rs** — isolated to `session/gemini.rs`. **Validate in
M1/M2:** confirm adk-rust's Gemini native-audio coverage, turn/VAD/barge-in fidelity, and
session-resumption behind the trait before committing hard.

---

## 5. Session lifecycle: start / stop / pause / resume (DESIGN §11; FR-15–18)

Two layers of "reconnect" that must not be conflated:

1. **App lifecycle** — deliberate start/stop/pause/resume for cost control. Crosses restarts.
2. **Provider session resumption** — transient socket reconnect within a live window.

### 5.1 Lifecycle state machine

```
            start (fresh) ─────────────►┌──────────┐
 ┌─────────┐ resume (restore context) ─►│ connecting├─ok─►┌─────────┐
 │ Stopped │◄──────────────────────────│            │     │ Running │  (listening/speaking/…)
 │(no cost)│       stop / pause ◄───────┴──────────┘     └────┬────┘
 └─────────┘◄──────────────────────────────────────────────┘ │ socket drop
       ▲                                                       ▼
       │ resumption fails / window expired         ┌──────────────────┐
       └───────────────────────────────────────────│ Reconnecting     │
                  reconnect ok (handle) ───────────►│ (provider resume) │──► Running
                                                     └──────────────────┘
```

- **start:** open a session with empty (`fresh`) or restored (`resume`) `initial_context`.
- **stop / pause:** `close()` the session → no socket, **no streaming cost**. History is already
  persisted (§6); intent flag distinguishes "done" vs "will resume" for UX only.
- **resume:** open a fresh session seeded with persisted context (§6.3). Tell the user context
  was restored.

### 5.2 Transient reconnect
On unexpected socket drop while `Running`: enter `Reconnecting`, retry with the last
`resumption_handle` (if `Capabilities.session_resumption`). On success → `Running`. On failure or
expired window → fall back to a **context-restoring restart** (§6.3). Never silently drop mic
state; always reflect `reconnecting` in FR-4 state.

### 5.3 Cost note
"Pause to save cost" means a real disconnect — streaming audio/video is what costs money, so
`Stopped` must hold **zero** open provider connections. Resuming pays only the (cheap) cost of
re-seeding text context, not replaying audio.

---

## 6. History & context persistence (FR-19, FR-20)

**Goal:** survive a system restart and resume the conversation with restored context — without
storing unbounded data, and without storing more than the model's context window per
conversation.

### 6.1 What is persisted
- An ordered log of `HistoryTurn`s: `{ role: user|assistant|system, text, ts, (later) tool_calls }`.
- We persist **text content** (transcripts), not raw audio. That is sufficient to reconstruct
  context for a fresh session. Audio is transient.
- Provider `resumption_handle` is persisted opportunistically but treated as best-effort
  (typically expires; not relied on across restart).

### 6.2 Bounding the history (the "not infinite" rule)
- A **token budget** equal to the **realtime/Live session's input limit** (configurable, with
  headroom) bounds the store. New turns append; oldest turns are **pruned** once the budget is
  exceeded. **Note:** the Live session's input budget is much smaller than the underlying text
  model's full context window — size this to the Live limit, *not* a 1M-class text-context number,
  so the persisted window is always re-seedable as `initial_context`.
- Net effect: persisted history is **at least one context window** and **never grows without
  bound**. We deliberately do **not** persist more than one context window per conversation.
- `[POST]` Optional rolling **summary** of pruned turns to retain gist beyond the window — not in
  MVP (the bound is a hard truncation in MVP).

### 6.3 Restore-to-context on resume
On `resume`/launch: load the persisted turns within budget → pass as `SessionConfig.initial_context`
→ the adapter seeds the new session (as prior conversation context / system preamble) so the
model continues coherently. This is the long-term mechanism; provider resumption (§5.2) is only
for transient drops.

### 6.4 Storage shape
- One conversation in MVP, stored under the app data dir (e.g. `history/current.*`).
- Format: append-friendly (e.g. JSONL) + a small index/meta file (model, token budget, last
  resumption handle). Writes are append-mostly; prune compacts periodically.
- **Multiple named sessions** `[POST]`: the schema keys on a `conversation_id`; MVP uses a single
  fixed id so adding sessions later is additive, not a migration.
- **Memory** (§10.4) is a *separate* store from history — do not conflate.

---

## 7. Media pipeline (DESIGN §6.3)

All media is captured, processed, and played **natively in Rust** (`crates/joi-media`); the webview
never touches audio or video. The DSP (framing, resample, jitter buffer, PCM/float conversion) is
pure, tested Rust in `joi-core::media`.

### 7.1 Audio in `[MVP]`
- Native **cpal** input (device from `config.audio.input_device`) → linear resample to
  **16 kHz mono PCM16** → frame to **20 ms** chunks (320 samples) → `send_audio`. The realtime cpal
  callback hands off to async via a lock-free `ringbuf`.
- **Mute (FR-6)** gates natively — the engine stops *pushing* and the manager drops muted audio
  (`set_mic_muted`), not just a UI flag.

### 7.2 Audio out `[MVP]`
- Provider **24 kHz mono PCM16** → `subscribe_audio()` → `joi-core::media::JitterBuffer` → native
  **cpal** output callback pulls fixed blocks (silence on underrun); target added latency ≤ 80 ms.
- On `Interrupted`/barge-in: a `Flush` command empties the jitter buffer and halts playback
  immediately (FR-2, target < 300 ms).

### 7.3 Screen capture `[MVP]`
- **Capture path:** native **`xcap`** grab loop (`crates/joi-media/src/screen.rs`) at `screen.fps`,
  downscaled so the longest edge ≤ `max_width`, JPEG-encoded at `quality`, pushed via `send_frame`.
  This is the **primary** path — no `getDisplayMedia`/webview capture. `scap` (PipeWire continuous)
  is a later upgrade if HW/continuous capture is needed.
- **Source (FR-9):** MVP captures the **primary monitor**; `start_screenshare` takes no source arg.
  Per-display selection and enumeration are `[POST]`.
- **Frames (FR-11):** sampled at `config.screen.fps`, downscaled to `max_width`, JPEG at `quality`
  — all configurable for cost/bandwidth.
- **Start/stop (FR-10):** immediate; the capture thread stops on signal, ending in-flight frames.
- **Capture-source abstraction (FR-12, `[POST]`):** widen the native grab to a `CaptureSource`
  (`Display(id)` now; `Window(id)` later) so app-window capture is an added variant, not a rewrite.

---

## 8. Terminal UI (FR-13, FR-14)

The primary readable surface is a **web terminal emulator** rendering the model's text output.

- **Component:** xterm.js (mature, framework-agnostic, ANSI/truecolor, addons for fit/links/web-gl
  renderer). Equivalent web terminal acceptable; xterm.js is the default.
- **Content:** streamed transcript (both speakers) and status lines, written as ANSI-styled text.
  Partial (non-final) transcript lines update in place; finalized lines are committed.
- **Styling:** "stylish colorization" via an ANSI theme — distinct colors per speaker, dim status
  lines, accent for the agent. Theme configurable (§13).
- **Future fit:** when tools land (§10), tool invocations and (gated) command output render in the
  same terminal with their own styling — the terminal is chosen partly because it's the natural
  surface for shell output later.
### 8.1 UI stack (React)
- **Base:** React + TypeScript + **Tailwind CSS** + **shadcn/ui** (copy-in primitives) for clean,
  modern app chrome (controls, settings, dialogs, the future permission prompt).
- **Flair:** **Aceternity UI** / **Magic UI** / **React Bits** + **Motion** (Framer Motion) for the
  techy/animated look — Aceternity ships an animated terminal component (typewriter + bash
  highlighting) usable for decorative output.
- **Real terminal:** **xterm.js** mounted in a `useEffect` on a container ref, with addons
  (fit, web-gl renderer, links). Used for true ANSI/scrollback output; a decorative styled shell
  may front it for flourish.
- **Typography/effects:** monospace (JetBrains Mono / Geist Mono); optional CRT/scanline CSS for
  the techy aesthetic. Theme configurable (§13).

### 8.2 Performance note (high-frequency updates)
React's re-render model must not sit on the hot paths. **Media never touches React — or the
webview at all**: capture, playback, and screen frames live entirely in native Rust (`joi-media`,
§7), so no audio/video crosses IPC. **Streaming transcript** writes go to xterm.js imperatively
(or a buffered, throttled commit), not a per-token React setState. React owns *control* UI (state,
buttons, settings) and renders backend `UiEvent`s; it never carries per-frame media.

---

## 9. Frontend framework decision — **React** ✅ (decided 2026-05-21)

**Decision: React + TypeScript.** The driving requirement is a **beautiful, stylized, modern UI**
(shadcn aesthetic + techy terminal). In 2026 that ecosystem is decisively React-native: shadcn/ui
is the de facto standard, and the flashy animated catalogs (Aceternity UI — incl. a ready-made
animated terminal, Magic UI, React Bits) plus Motion are **React-only**. Media APIs and xterm.js
are JS-native, so React has zero interop friction on the heavy paths. The accepted cost — heavier
runtime + re-render care for streaming — is contained by keeping media off React state (§8.2), and
is immaterial at this app's UI scale on a Tauri desktop.

Alternatives considered: **SolidJS** (faithful shadcn look via shadcn-solid + better runtime perf,
but the flashy animated libs are React-exclusive — you'd hand-build flair); **Rust/WASM**
(Leptos/Dioxus — shares IPC types with the backend, but weakest UI ecosystem and most
media/xterm.js interop friction). Decision record / criteria:

| Criterion | React | SolidJS | Rust/WASM (Leptos/Dioxus/Yew) |
|---|---|---|---|
| Real-time UI churn (streaming transcript, audio meters) | OK (needs care w/ re-renders) | **Excellent** (fine-grained reactivity) | Good |
| Media APIs (getUserMedia, AudioWorklet, getDisplayMedia) | **First-class** JS | **First-class** JS | Awkward via JS interop |
| xterm.js integration | Easy | Easy | JS interop glue |
| Type-sharing with Rust backend | None (hand-kept TS types) | None | **Shared types** (same language) |
| Ecosystem / hiring / examples | **Largest** | Moderate | Smallest |
| Bundle / runtime weight | Heaviest | **Light** | Wasm payload; heavier startup |

The media + streaming-terminal heavy lifting is JS-native, ruling out Rust/WASM's interop
friction; between the two JS options, the **beautiful-UI-with-least-effort** priority broke the tie
for **React** (its animated-component ecosystem has no SolidJS equivalent). See §8.1 for the
resulting UI stack.

---

## 10. Tool system — extensibility seam `[POST]` (DESIGN §6.4, §7, §8)

**No tools ship in the MVP.** This section specifies the seam so tools — including
permission-gated shell access and the memory tool — drop in **without rewrites**. The MVP must
leave these insertion points in place:

- `SessionEvent::ToolCall`, `RealtimeSession::send_tool_result`, and `SessionConfig.tools` already
  exist in the trait (§4) but go unused in MVP.
- The agent loop has a single dispatch point where `ToolCall` events would route.
- `Capabilities.async_tool_calls` is plumbed but ignored.

### 10.1 Tool trait & registry `[POST]`
```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;                          // name, description, JSON-schema params
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult;
}
```
Tools are plain provider-neutral functions in a registry keyed by name; schemas feed
`SessionConfig.tools`. Default contract is **blocking** (portable); talk-through (Gemini
`NON_BLOCKING`) is optional and capability-gated.

### 10.2 Permission gate `[POST]` (DESIGN §7)
The headline future tool is `bash`, behind a gate that:
- classifies the **resolved** command (read-only / mutating / destructive); unknown ⇒ prompt;
  pipelines take the highest tier;
- prompts for mutating, **always** prompts for destructive, may auto-allow read-only;
- requires a **deliberate non-voice** approval (click/keypress) of the resolved command — never a
  spoken "yes" (DESIGN §7.5, §8.1);
- supports allow-once / allow-always-pattern / deny, edit-before-run, and **default-deny on
  timeout**.

The terminal UI (§8) is the natural place to render the resolved command and its (gated) output;
the **approval prompt itself is app-chrome, not rendered inside shared/streamed content** (so
on-screen content can't spoof it).

### 10.3 Exec endpoint & sandbox `[POST]` (DESIGN §8.3–8.4)
```rust
#[async_trait]
pub trait ExecEndpoint: Send + Sync {
    async fn run(&self, command: &str, opts: ExecOpts) -> ExecOutput; // stdout/stderr/code/duration
}
```
First impl `LocalExec`: non-root, scoped cwd, no ambient creds, **no network by default**, full
logging. Swappable later for container / remote microVM (microVM > gVisor > Docker) without
touching the gate or tools.

### 10.4 Memory tool — first tool after MVP `[POST]` (FR-24)
- Lets the agent **persist and recall curated long-term facts** so it can learn/adapt across
  conversations.
- Implemented as tool functions (e.g. `memory.write{key,value,tags}`, `memory.search{query}`),
  backed by a **separate store from history (§6)** — memory is agent-curated knowledge, history is
  raw conversation context for resume.
- Subject to the same registry/dispatch seam; no special-casing in app logic.
- Storage: simple keyed/embedded store to start (file/SQLite); embeddings/semantic search are an
  internal upgrade behind the tool, invisible to the model contract.

---

## 11. IPC protocol (webview ↔ backend)

Two channels: **commands** (frontend → backend via `invoke`) and **events** (backend → frontend
via a single `ui_event` emit). **No media crosses IPC** — audio/video are native (§7), so the
protocol is **JSON only**; the binary `tauri::ipc::Channel` is gone.

### 11.1 Commands (frontend → backend)
Registered today in `src-tauri` (`generate_handler!`):

| Command | Args | Returns |
|---|---|---|
| `ping` | `{}` | `"pong"`-style string (health check) |
| `has_api_key` | `{}` | `{ present }` |
| `set_api_key` | `{ key }` | ok |
| `start` | `{ resume: bool }` | `{ session_id }` or error |
| `stop` | `{ pause: bool }` | ok (closes session, persists) |
| `send_text` | `{ text }` | ok |
| `set_mic_muted` | `{ muted }` | ok |
| `start_screenshare` | `{}` | ok (native primary-monitor capture; §7.3) |
| `stop_screenshare` | `{}` | ok |

`[POST]` (planned, not yet wired): `resume`, `get_history_meta`, `clear_history`, `panic_stop`,
`get_settings`/`set_settings`, `permission_decision`. (`resume`/history/panic helpers exist in the
TS `ipc.ts` wrapper but are not yet registered as commands.)

### 11.2 Media transport
**None.** Audio capture/playback (cpal) and screen capture (`xcap`) run entirely in native Rust
(`joi-media`, §7) against the `SessionManagerHandle`; no PCM or image bytes ever cross the IPC
boundary. This supersedes the earlier binary `tauri::ipc::Channel` audio path.

### 11.3 Events (backend → frontend)
A single Tauri event named **`ui_event`** carries the tagged `UiEvent` enum (serde `type` tag,
snake_case):

| `type` | Payload |
|---|---|
| `state` | `{ state }` (FR-4 enum) |
| `transcript` | `{ speaker, text, final }` → terminal (§8) |
| `connection` | `{ status, detail? }` |
| `history` | `HistoryMeta` `{ turns, token_estimate, budget }` (on append/prune) |
| `error` | `{ kind, message }` |
| `command_log_append` `[POST]` | LogEntry |
| `permission_request` `[POST]` | `{ call_id, command, tier, cwd, explanation, timeout_s }` |
| `tool_result` `[POST]` | `{ call_id, ok, display, exit_code? }` |

Command/event payloads share a serde-typed definition in `ipc.rs`, mirrored by a TS types file
kept in sync (or generated; trivial if §9 picks Rust/WASM).

---

## 12. Security model (DESIGN §8)

MVP carries no shell/tools, so the RCE surface (DESIGN §8.1) is **not yet open** — but the
controls are specified now so they exist *before* the first tool lands.

- **SEC-1** `[POST]` Non-voice consent for mutating/destructive commands (§10.2). The single most
  important control once tools exist.
- **SEC-2** `[POST]` Default-deny on approval timeout.
- **SEC-3** `[POST]` Local jail on every executed command (non-root, scoped cwd, no net, full log).
- **SEC-4** `[POST]` Exec endpoint swappable; first impl is local-jail.
- **SEC-5** `[MVP]` **Key handling:** the API key is part of config (`live_api.gemini.api_key`),
  set in the YAML file or — preferred — the `GEMINI_API_KEY` environment variable (env wins). It is
  held as a redacting `ApiKey` (Debug-safe), never sent to the webview, logs, history, or any
  Joi-operated server (none exists); it travels only to the provider over the direct WebSocket.
  Putting the key in the YAML stores it in plaintext on disk — the env var avoids that. (OS-keychain
  storage remains a possible future hardening, `[POST]`.)
- **SEC-6** `[POST]` Treat all on-screen content as hostile input; the permission prompt is
  app-chrome, never rendered inside shared/streamed content (anti-spoof).
- **SEC-7** `[MVP]` Webview never receives the key, never decides a command, never executes.
- **SEC-8** `[MVP]` Logging/persistence hygiene: redact detectable secrets from logs; history and
  logs are local-only.

---

## 13. Configuration & settings

All settings live in the YAML config or env (env wins). The API key may be set in config but is
**preferably provided via `GEMINI_API_KEY`** to avoid plaintext on disk (SEC-5). Top-level YAML
sections map 1:1 to modules (env nests with `JOI_` + `__`, e.g. `JOI_MEDIA__AUDIO__FRAME_MS`):
- `live_api` — provider + exact model id, key, voice, system instruction, transcription (`gemini.*`).
- `history` — token budget (Live session input limit, §6.2).
- `logging` — level, file.
- `media` — `audio` (devices, sample rates, frame_ms, echo_cancellation) and `screen` (source/path,
  fps, resolution cap, quality).
- `ui` — `terminal` theme / font / scrollback (a frontend concern, delivered to the UI by the host).
- `[POST]` Gate: read-only auto-allow toggle, approval timeout, allowlist management.

---

## 14. Error handling & edge cases

- **Connection loss while Running:** `reconnecting` state, provider-resume retry, then
  context-restoring restart (§5.2); never silently lose mic state.
- **Auth failure:** explicit "invalid/expired key" path → settings.
- **Provider session length cap:** resume if supported, else context-restoring restart, surfaced
  to the user.
- **Resume with empty/corrupt history:** start fresh; warn, don't crash; never load partial/garbled
  context silently.
- **History at budget:** prune oldest within the same write; persistence must never block the audio
  path.
- **Screen capture fails/empty (no monitor, `xcap` error):** disable screenshare with a clear
  reason — never send blank/black frames silently.
- **Long transcript/output:** terminal scrollback bounded; full content in history within budget.
- `[POST]` **Barge-in while a tool runs:** MVP-default once tools exist — conversation can be
  interrupted, but a running command keeps running until it finishes or `panic_stop`/cancel.

---

## 15. Observability

- Structured event log (`log.rs`): lifecycle transitions, connection events, turns, errors (and
  later tool calls + decisions + exit codes). Local file.
- Dev-build debug overlay: current state, audio levels, RTT, frame rate, history token estimate.

---

## 16. Testing strategy

- **Adapter conformance suite + mock adapter:** one test set run against any `RealtimeSession`
  impl; a scripted **mock adapter** exercises the whole app loop (lifecycle, history, terminal)
  with no network. The OpenAI stub must compile against it. This is how we *prove* provider-agnosticism
  (DESIGN §2.1) and de-risk the §4.5 library choice.
- **Lifecycle tests:** start/stop/pause/resume transitions; transient-reconnect → resume fallback;
  `Stopped` holds zero connections.
- **History tests:** append + prune at budget; restore-to-context round-trip; corrupt/empty load;
  bound never exceeded.
- **Media tests:** resample correctness, 20 ms framing, jitter-buffer flush on interrupt, mute
  stops sending at source, screenshare start/stop revokes frames.
- **SEC tests:** `[MVP]` key never appears in logs/history/events; `[POST]` no path executes a
  mutating command without a `permission_decision`; timeout denies.

---

## 17. Build order / milestones

Each milestone is independently demoable.

- **M0 — Skeleton.** Tauri v2 builds on Linux. Webview ↔ backend IPC roundtrip. Keychain
  read/write of API key. Settings screen accepts a key. (Frontend framework chosen here, §9.)
- **M1 — Loop on a mock.** `RealtimeSession` trait + **mock adapter**. Mic capture → 16k PCM →
  IPC → adapter; scripted audio out → playback; transcript → **terminal UI** (§8); state (FR-4).
  *Proves media path + abstraction with zero network.*
- **M2 — Gemini voice.** `GeminiAdapter` on the chosen library (§4.5): real S2S conversation with
  turn-taking and barge-in (FR-1–7). BYOK direct connect (FR-21–22).
- **M3 — Lifecycle + persistence.** start/stop/pause/resume FSM (§5), bounded history store (§6),
  restore-to-context on launch, transient reconnect (FR-15–20). *Pause/resume saves cost.*
- **M4 — Screen capture.** Source enumeration + selection, `getDisplayMedia` + native fallback,
  start/stop, quality settings, sharing indicator (FR-8–11).
- **M5 — Hardening.** Error paths, logging/persistence hygiene, panic-stop, cross-platform (macOS
  then Windows), OpenAI stub compiling against the conformance suite.
- **M6 `[POST]` — Tools seam.** Activate the tool registry/dispatch, then permission-gated `bash`
  + local-jail exec (DESIGN §7/§8).
- **M7 `[POST]` — Memory tool** (§10.4), the first tool that makes Joi learn/adapt.

**MVP = M0–M5.**

---

## 18. Acceptance criteria for the MVP

On Linux:
1. User enters a Gemini key (keychain), starts, and holds a natural spoken conversation with
   working turn-taking and barge-in; the transcript renders in a colorized terminal UI.
   *(FR-1–7, 13–14, 21–22; SEC-5, 7)*
2. User picks a screen, starts/stops sharing, and Joi can describe on-screen content; quality
   defaults to native/max-the-API-accepts and is adjustable. *(FR-8–11)*
3. User **stops/pauses** to cut cost (no open connection), then **resumes** and the conversation
   continues with restored context. *(FR-15–18)*
4. After a **full system restart**, relaunching restores the prior conversation context from disk;
   history is bounded and never grows without limit. *(FR-19–20)*
5. **Panic-stop** halts session, mic, and capture in one action.
6. The workspace compiles with the OpenAI stub present and the conformance suite passes against the
   mock adapter — no Gemini-specific assumption leaked into app logic. *(DESIGN §2.1)*
7. No tool/shell code path is reachable by the model (tools are absent), yet the registry/dispatch
   seam exists for M6. *(§10)*

---

## 19. Open questions & pending decisions

- **Frontend framework (§9):** ✅ **Decided — React + TypeScript** (shadcn/ui + Aceternity/Magic UI
  + Motion; xterm.js for real terminal output). Media kept off React state (§8.2).
- **Realtime SDK (§4.5):** ✅ **Decided — adk-rust** (provider-agnostic realtime, OpenAI adapter
  for free, tools/memory layer for M6/M7). Isolated to `session/gemini.rs`; gemini-rs is the
  no-rewrite fallback. **Validate adk-rust's Gemini native-audio path in M1/M2.**
- **Screen-capture reliability per platform (DESIGN §17):** confirm `getDisplayMedia` on
  WebKitGTK / WKWebView / WebView2; pick per-OS default (webview vs native). **Resolve in M4.**
- **adk-rust maturity (DESIGN §17):** pre-1.0 (~v0.8); the trait seam caps blast radius.
- **Context-restore fidelity:** how much pruning/summarization is acceptable before resume feels
  lossy; MVP uses hard truncation at the window bound — revisit a rolling summary `[POST]`.
- **Tool-result + command UX in the terminal (DESIGN §17):** spoken summary vs terminal panel for
  long output — decide with M6.
