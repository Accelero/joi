# PLAN — Native-media re-architecture (Rust owns all I/O)

> **Goal.** Make Joi a pure-Rust app with a thin web UI. Rust owns *all* logic and heavy lifting:
> audio capture + playback, screen capture, DSP, streaming, networking. The webview (React) only
> renders the UI (terminal, buttons, state) and dispatches commands / displays events. **No media
> ever crosses into JS.**
>
> **Why.** The webview media path (getUserMedia + Web Audio over `tauri::ipc::Channel`) forces a
> WebKitGTK + GStreamer dependency, a Wayland-fragile mic-permission handler, and extra latency.
> Native capture/playback removes all of it and is lower-latency. This supersedes SPEC §7–§8 audio
> transport and §11.2 (binary Channel).

## 1. Architecture: before → after

**Before** (current):
```
Webview: getUserMedia → AudioWorklet → PCM → Channel ─┐
                                      Web Audio ◄── Channel ─┤
Rust: SessionManager ↔ GeminiAdapter ↔ Gemini Live ─────────┘
```

**After**:
```
Webview (UI only): Controls/Terminal ── commands ──► Rust
                                       ◄── UiEvent ── (state, transcript, connection, errors)
Rust: cpal mic ─► resample/frame ─► SessionManager.send_audio ─► GeminiAdapter ─► Gemini
      cpal out ◄─ jitter buffer ◄─ AudioOutput events ◄─────────────────────────┘
      xcap/scap ─► JPEG encode ─► SessionManager.send_frame
```

The webview becomes stateless w.r.t. media. The `SessionManagerHandle` API (`send_audio`,
`subscribe_audio`, `send_frame`, `set_mic_muted`) is **unchanged** — a native **MediaEngine** simply
takes the role the frontend used to play.

## 2. Crates (native I/O)

| Concern | Crate | Notes |
|---|---|---|
| Audio in/out | **`cpal`** | Cross-platform; on Linux uses ALSA (PipeWire's ALSA compat works). i16/f32 streams. |
| Resampling | **`rubato`** | Device rate (44.1/48 kHz) ↔ 16 kHz in / 24 kHz out. |
| Lock-free audio handoff | **`ringbuf`** | Bridge realtime cpal callback ↔ tokio without locks. |
| Screen capture | **`xcap`** (MVP) | Per-frame grab at `screen.fps`; simplest for 1 fps. `scap` (PipeWire continuous) as a later upgrade. |
| JPEG encode | **`image`** (or `turbojpeg`) | Encode frames to `FrameEncoding::Jpeg`. |

## 3. Workspace + crate changes

- **New crate `crates/joi-media`** — native adapters + the MediaEngine. Depends on `joi-core`,
  `cpal`, `rubato`, `ringbuf`, `xcap`, `image`. Keeps all OS/audio deps out of `joi-core`.
- **`joi-core` (pure, no new I/O deps):**
  - New ports: `audio::AudioInput` (start→frames, stop) and `audio::AudioOutput` (enqueue, flush,
    stop). I/O-free traits; cpal impls live in `joi-media`. (`capture::ScreenSource` already exists.)
  - Move DSP into Rust as pure, tested code: port `JitterBuffer`, `FrameAccumulator`, linear
    `downsample`, `float↔pcm16` from the deleted `src/media/dsp.ts` into `joi-core::media`
    (`pcm16_to_le_bytes`/framing already there). Unit-test parity with the old TS tests.
  - Add an **interrupt/flush signal** on the audio path: on `TurnEvent::Interrupted` (barge-in), the
    manager notifies playback to flush. Add `SessionManagerHandle::subscribe_control()` (or reuse a
    broadcast) carrying a `Flush` event the MediaEngine consumes (replaces the JS "flush on
    state→listening").

## 4. MediaEngine (`joi-media`)

A small actor started by the composition root when a session starts; stopped on stop. Holds the
`SessionManagerHandle`.

- **Capture:** open cpal input (device from `config.audio.input_device`), resample to 16 kHz mono,
  frame to `config.audio.frame_ms` (320 samples @ 20 ms), `handle.send_audio(frame)`. Mute gate via
  `config`/`set_mic_muted` (manager already drops muted audio; engine can also stop pushing).
- **Playback:** `handle.subscribe_audio()` → `JitterBuffer` → cpal output callback pulls blocks
  (silence on underrun). `flush()` on the interrupt signal (barge-in, FR-2 < 300 ms).
- **Screen (when enabled):** xcap grab loop at `screen.fps` → downscale to `max_width` → JPEG at
  `quality` → `handle.send_frame(VideoFrame)`.

cpal callbacks are realtime; cross to tokio via `ringbuf` (capture) and feed the output ring from the
broadcast consumer (playback).

## 5. Composition root (`src-tauri`)

- On `start`: spawn the MediaEngine (capture + playback) bound to the manager handle; on `stop`:
  shut it down.
- **Delete** the WebKitGTK `with_webview` media-permission handler and `set_enable_media_stream`
  (no getUserMedia anymore).
- Commands become media-free: `start { resume }` (no `audioOut` Channel), `stop`, `send_text`,
  `set_mic_muted`, `start_screenshare`, `stop_screenshare`, `has_api_key`, `set_api_key`, `ping`,
  `get_history_meta`, `clear_history`, `panic_stop`.

## 6. Frontend (UI only)

- **Delete:** `src/media/mic.ts`, `playback.ts`, `mic-worklet.js`, `playback-worklet.js`,
  `dsp.ts`, `dsp.test.ts`, and the `audio`/`Channel` block in `ipc.ts`.
- **Keep/adjust:** `Terminal.tsx` (transcripts), `Controls.tsx` (Start/Stop/Mute + add a
  Screenshare toggle), `App.tsx` (calls `commands.start({resume:false})` directly — no playback/mic
  setup), `ipc.ts` (commands + `onUiEvent` only).
- Net effect: frontend shrinks substantially; no AudioWorklets, no binary transport.

## 7. Dependency / config cleanup

- Drop the GStreamer requirement from `scripts/setup-linux.sh` (no longer needed) and instead note
  ALSA/PipeWire dev headers if cpal needs them at build (`alsa-lib`).
- `config.audio.*` (input/output device + sample rates + frame_ms) now drives the **native** engine
  — fields already exist; wire them through.

## 8. SPEC updates

- §7 Audio: capture/playback are native (cpal), not webview Web Audio.
- §8.2: media no longer flows through the webview/React.
- §11.2: remove the binary `tauri::ipc::Channel` audio transport; IPC is JSON commands + `UiEvent`s
  only.
- §7.3 Screen: native (xcap/scap) is the primary path, not a fallback.

## 9. Staged delivery (each stage compiles, tests, and is demoable)

1. **N1 — DSP to Rust.** Port jitter buffer / framing / resample to `joi-core::media` + tests. No
   behavior change yet.
2. **N2 — Native playback.** `joi-media` cpal output + `AudioOutput` port; composition root feeds it
   from `subscribe_audio()`. Verify Gemini audio plays natively (we already connect). Web Audio
   still present but unused.
3. **N3 — Native mic.** cpal input + resample → `send_audio`. Verify voice-in. Add barge-in flush
   signal.
4. **N4 — Strip webview media.** Delete TS audio, the `Channel`, the getUserMedia handler, and the
   GStreamer dep. Confirm a full spoken turn with zero webview media.
5. **N5 — Native screen capture.** xcap loop → `send_frame`; Screenshare toggle in UI.
6. **N6 — Docs.** Update SPEC §7/§8/§11 and `setup-linux.sh`; refresh `NOTES`.

## 10. Risks / decisions

- **cpal on PipeWire/Wayland:** confirm default-device enumeration + format (prefer i16; else f32→
  i16). Spike in N2 before committing.
- **Realtime ↔ async bridging:** keep cpal callbacks allocation-free; use `ringbuf`. Jitter target
  ≤ 80 ms; barge-in halt < 300 ms (FR-2) — measure in N3.
- **Resampler latency:** `rubato` adds a little; linear is cheaper if quality suffices for 16 kHz
  speech. Decide in N2.
- **Mute semantics:** keep the manager as the authority (`set_mic_muted`), engine stops pushing too.
- **`scap` vs `xcap`:** start with `xcap` (1 fps grab); upgrade to `scap` only if continuous/HW
  capture is needed.
```
