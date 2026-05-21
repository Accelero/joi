# NOTES-adk.md — M2 precondition spike (adk-rust realtime API)

> **Status: spike complete (go).** This records the real `adk-realtime` API as read from
> source on 2026-05-21, per PLAN §M2 (the go/no-go gate). Any adk-rust churn is adapted in
> `gemini.rs` **only** so it never leaks past `joi_core::RealtimeSession`.

## Crate & feature

- Crate: **`adk-realtime`** (latest published **0.8.x**; family repo `zavora-ai/adk-rust`).
- Gemini support is gated behind the **`gemini`** feature (pulls the optional `adk-gemini`
  backend). We enable **only** `gemini` — not `openai` (async-openai), `livekit`, or `vertex` —
  to keep the dependency tree minimal.
- Realtime path is `adk-realtime/src/gemini/{model,session}.rs`. The `adk-rust` `examples/gemini_audio`
  the PLAN pointed at is **NOT** realtime (it's agentic TTS/STT); the realtime example is
  `adk-realtime/examples/debug_gemini.rs`.

```toml
# crates/joi-providers/Cargo.toml
[dependencies]
adk-realtime = { version = "0.8", default-features = false, features = ["gemini"], optional = true }
# gemini feature (joi) → enables the adk-realtime dep + its "gemini" feature
```

## Required runtime init

adk-realtime uses rustls; the process must install a crypto provider **once at startup**
(composition root, not the adapter):

```rust
rustls::crypto::aws_lc_rs::default_provider().install_default().ok();
```

## Construction & connect (BYOK / AI Studio)

```rust
use adk_realtime::RealtimeConfig;
use adk_realtime::gemini::{GeminiLiveBackend, GeminiRealtimeModel};
use adk_realtime::model::RealtimeModel;     // trait: connect()
use adk_realtime::session::RealtimeSession;  // trait: send_audio/next_event/...

let backend = GeminiLiveBackend::studio(api_key);          // BYOK direct connect (vs ::vertex)
let model   = GeminiRealtimeModel::new(backend, model_id); // model_id e.g. "models/gemini-2.5-flash-native-audio-latest"
let config  = RealtimeConfig::default()
    .with_voice("Aoede")
    .with_instruction(system_instruction)
    .with_transcription()    // input transcription (FR-3)
    .with_server_vad();      // server-side VAD → barge-in
let session /* : BoxedSession = Arc<dyn RealtimeSession> */ = model.connect(config).await?;
```

⚠️ **Model id mismatch to reconcile:** our `joi.example.toml` uses
`gemini-live-2.5-flash-native-audio`; the adk example uses the `models/…-latest` form. The adapter
must pass whatever Gemini's BiDi endpoint accepts — confirm the exact id during the live smoke test
and update the config default if needed.

## The session API (low-level `RealtimeSession` trait) — what we bridge to

All methods take **`&self`** and the session is an `Arc<dyn RealtimeSession>`, so a send-side and a
receive task share it concurrently with **no borrow conflict**. This is the key finding:

- `async fn send_audio(&self, audio: &AudioChunk) -> Result<()>`
- `async fn send_text(&self, text: &str) -> Result<()>`
- `async fn interrupt(&self) -> Result<()>`        // barge-in / cancel current response
- `async fn create_response(&self) -> Result<()>`  // manual-VAD trigger (unused with server VAD)
- `async fn next_event(&self) -> Option<Result<ServerEvent>>`  // None = closed
- `fn events(&self) -> Pin<Box<dyn Stream<Item = Result<ServerEvent>> + Send + '_>>`
- `async fn close(&self) -> Result<()>`
- `async fn mutate_context(&self, cfg) -> Result<ContextMutationOutcome>`
  - **Gemini returns `RequiresResumption(config)`** (no native hot-swap) → matters for M3 config edits.

`AudioChunk { data: Vec<u8>, format: AudioFormat }` with helpers `AudioChunk::pcm16_16khz(bytes)`
(input) and the 24 kHz form for output. Joi holds PCM as `&[i16]`, so the adapter converts
**i16 → little-endian bytes** on send and **bytes → i16** on receive.

## Event mapping: `ServerEvent` → `joi_core::SessionEvent`

| adk `ServerEvent`                         | joi `SessionEvent` / action                       |
|-------------------------------------------|---------------------------------------------------|
| `SessionCreated { .. }`                   | connection established                            |
| `AudioDelta { delta: Vec<u8>, .. }`       | `AudioOutput` (24 kHz PCM16 bytes → i16)          |
| `TranscriptDelta` / `TranscriptDone`      | `Transcript` (agent; + input transcription)       |
| `TextDelta` / `TextDone`                  | `Transcript` text (when modality is text)         |
| `SpeechStarted`                           | barge-in signal → emit `Interrupted`, flush playback |
| `ResponseDone { .. }`                     | `TurnEvent` complete                              |
| `Error { error, .. }`                     | `SessionError::Provider` / `::Auth`               |

## Loop ownership decision (PLAN_REVIEW B3 / M-1) — **GO, no fallback**

adk-realtime offers two layers:
1. **High-level `RealtimeRunner`** + `EventHandler` callbacks (`on_text`, …) — owns its own loop.
2. **Low-level `RealtimeModel::connect → BoxedSession`** with pollable `next_event()` / `events()`.

We use **layer 2**. Because sends and `next_event()` are both `&self` on a shared `Arc`, the adapter
spawns one task that polls `next_event()` and forwards mapped `SessionEvent`s into Joi's owned
`mpsc`/`EventReceiver` (`take_events`). This does **not** fight the actor model, so:

- **gemini-rs fallback trigger:** only if (a) `adk-realtime` + `gemini` fails to build/resolve, or
  (b) the live smoke test cannot connect/stream against Gemini BiDi. Not needed on API grounds.

## Open items to verify at live-test time (needs a real `GEMINI_API_KEY`)

- Exact accepted `model_id` string for native-audio BiDi.
- Whether input transcription arrives as `TranscriptDelta` vs a distinct input event.
- Barge-in latency (`SpeechStarted` → playback halt) < 300 ms (FR-2).
