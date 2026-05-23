# PLAN — Move audio conditioning in-app (sonora APM) on a stable hardware-mic capture

Status: **DONE — verified working on speakers (2026-05-24)** · Owner: backend · Scope: `joi-media`, `joi-app`, `joi.yaml`

## Outcome (what actually happened)
- **Device-by-name plumbing** landed (`MediaConfig.input_device/output_device`, picker + enumeration log) — but the raw-ALSA card device (`sysdefault:CARD=Generic_1`) gives **silence**: the mic is an **AMD ACP DMIC** that only works through PipeWire/UCM. Resolution: set PipeWire **default source → Mic1** ("Digital Microphone") and **default sink → raw Speaker** via `wpctl`, with Joi on `"default"`.
- **Stage 0 (rate):** hardware mic is a stable 1× — the intermittent 2× was specific to the PipeWire `echo-cancel-source` virtual node.
- **Stage 2 (NS):** the raw DMIC has a large **DC offset** (flat ~−14 dBFS) that buried speech; fixed by coupling a **high-pass filter** with NS in `capture.rs`. Mic became intelligible.
- **Stage 3 (AEC):** first attempt failed (self-barge-in) because the reference was fed at **bursty provider-arrival**. Fixed by **re-tapping the reference at the playback output callback** (real-time paced, at the playback device rate via new `RenderRef`), plus routing output direct to the hardware speaker. Self-barge-in gone, user voice preserved. ✅
- **Stage 1 (AGC):** left off (`auto_gain: false`); worked well without it. Optional lever for more consistent level.

Remaining: instrumentation (`mic levels` / `mic input rate`) still on at `debug`; not committed; working state depends on PipeWire defaults staying Mic1/Speaker.

---

Original plan below.

Scope: `joi-media`, `joi-app`, `joi.yaml`
Prereq context: this is attempt N+1 — prior tries failed. The difference now is the **armed
instrumentation** (per-second `mic input rate` probe + `mic levels` dBFS taps in `capture.rs`) and a
**staged** bring-up that turns one APM stage on at a time so a regression is attributable.

## 1. Why

We currently capture through PipeWire's **virtual** `echo-cancel-source` (it's the system default
input) and let PipeWire do AEC/NS/AGC; the in-app APM is off. Two problems:

1. **Intermittent garbling (the live bug).** The virtual source occasionally delivers **~2× the
   sample rate** (measured: 2 level-logs/s ⇒ ~88.2 k mono samples/s while the app assumes 44.1 k).
   The audio shipped to Gemini is then time/pitch-distorted → "can't understand me." It's
   per-session-random (capture #1 = 1×, #2 = 2×, #3 = 1×), which is why it's *sometimes*
   immediate and *sometimes* "after the first sentence."
2. **Linux-only.** PipeWire conditioning does nothing for the macOS/Windows targets.

Fix: capture from the **real hardware mic** (stable rate) and run conditioning **in-app** via
`sonora` (the pure-Rust WebRTC APM: AEC3 + NS + AGC2) — cross-platform, and we control the AEC
far-end reference (Joi's exact playback).

## 2. Evidence already gathered (do not re-litigate)

- Mic **levels are healthy**: speech −21…−30 dBFS, floor ~−48 dBFS. Not a gain problem.
- `resample_linear` is **correct** and deterministic (`out_len = in_len × out/in`). Not a resampler
  bug.
- Measured input rate is **correct when it's correct** (44100), so the 2× is upstream delivery from
  the virtual device, not our math.
- `audio.input_device` / `audio.output_device` config fields are **ignored** by `joi-media` today
  (dead config). This is the core plumbing gap to close.

## 3. Target signal path (all in-app, cross-platform)

```
HW mic ──cpal──▶ downmix mono ──▶ resample dev→16k ──▶ sonora APM ──▶ 20ms PCM16 ──▶ Gemini
                                                          ▲
provider 24k audio ──▶ resample 16k ──▶ render ref ───────┘ (AEC far-end, fed 1:1 w/ capture frames)
provider 24k audio ──▶ resample dev ──▶ HW speaker (cpal playback)
```

No PipeWire virtual devices in the path. AEC reference = the actual provider audio Joi plays.

## 4. Code changes (precise)

**4.1 `crates/joi-media/src/engine.rs` — add device fields to `MediaConfig`**
- Add `pub input_device: String` and `pub output_device: String` (values: `"default"` or an exact
  device name).
- Pass `input_device` into `spawn_capture(...)` and `output_device` into `spawn_playback(...)`.

**4.2 `crates/joi-app/src/lib.rs` (~line 59) — plumb from `Config`**
- `input_device: config.media.audio.input_device.clone()`,
  `output_device: config.media.audio.output_device.clone()`.

**4.3 `crates/joi-media/src/capture.rs` — select input device by name**
- New helper: `fn pick_input_device(host, name: &str) -> Option<cpal::Device>`:
  `"default"` ⇒ `host.default_input_device()`; otherwise first of `host.input_devices()` whose
  `.name()? == name` (fall back to default + `warn!` if not found, so a stale name never silently
  kills capture).
- **One-time enumeration log** at capture start: `info!` the available input device names + the
  chosen one. We need this to learn the exact cpal strings (cpal's ALSA/PipeWire names ≠ the
  `pactl` node names — must verify, see §6 Stage 0).
- Thread `input_device: String` through `spawn_capture`.

**4.4 `crates/joi-media/src/playback.rs` — same for output** (`pick_output_device`, enumeration log).

**4.5 Instrumentation** — keep the `mic input rate` probe and `mic levels` taps **as-is** for the
whole bring-up (this is the "armed debugger"). After Stage 3 passes, demote/trim per §7.

> Note: editing any watched source triggers a `tauri dev` rebuild that **restarts the app and drops
> the live session**. Batch each stage's edits, then test once — don't edit mid-conversation.

## 5. Config changes (`~/.config/joi/joi.yaml`)

- `media.audio.input_device`: hardware mic (exact cpal name from §4.3 enumeration; candidate
  PipeWire node `alsa_input.pci-0000_c2_00.6.HiFi__Mic1__source`).
- `media.audio.output_device`: hardware speaker (candidate `…Speaker__sink`) — so playback also
  bypasses the echo-cancel **sink**, keeping the AEC reference == what's actually played.
- `echo_cancellation` / `noise_suppression` / `auto_gain`: enabled **one stage at a time** (§6).

> **Ship default stays `"default"`.** On macOS/Windows `"default"` already *is* the hardware mic;
> explicit ALSA names are a **Linux-only override** to dodge this box's echo-cancel default. Do not
> bake Linux device names into `Config::default()`.

## 6. Staged bring-up + verification (the part that was missing before)

Restart a fresh session for each stage; do a multi-turn conversation on **speakers**.

**Stage 0 — hardware mic, conditioning OFF** (isolate the rate fix)
- Config: `input_device`/`output_device` = hardware; all three APM flags `false`.
- PASS iff: `mic input rate` `actual ≈ device_rate` (likely 48000) and `mic levels` fire **1×/s**
  across **≥3 session restarts** (no 2×); speech is intelligible to Gemini. Echo/self-interrupt on
  speakers is *expected* here and ignored.
- This proves the rate bug is gone independent of the APM.

**Stage 1 — + AGC** (`auto_gain: true`)
- PASS iff: `post_apm_dbfs` is leveled up vs `pre_apm_dbfs` on quiet speech; still intelligible; no
  pumping/clipping.

**Stage 2 — + NS** (`noise_suppression: true`)
- PASS iff: silence-window `post_apm` drops below `pre_apm` (floor cleaned); speech `post_apm`
  preserved (not gated).

**Stage 3 — + AEC** (`echo_cancellation: true`) — *the risky one; this is where it broke before*
- Watch `render_dbfs`: must rise when Joi speaks (reference present) and sit at floor otherwise.
- PASS iff: while you speak **after** Joi has replied, `post_apm` tracks `pre_apm` (your voice is
  **not** cancelled — the documented drift failure) **and** Joi's echo doesn't read as user speech
  (no self-interrupt). Hold across several turns to catch slow drift.
- If `post_apm` collapses during your speech post-playback → AEC drift regression; capture the trace
  and fall back (see §8).

## 7. Cleanup after Stage 3 passes
- Keep the instrumentation but ensure it's `debug!` (already is) so production `info` stays quiet;
  consider a one-line `info!` summary every N seconds instead of per-second if it's noisy.
- Update `crates/joi-media/src/capture.rs` module docs to describe the hardware-mic + in-app path.
- Decide whether `Config::default()` should keep all three flags `true` (it does today) — yes, since
  in-app is now the intended cross-platform path.

## 8. Risks & mitigations
- **sonora maturity (0.1.0).** We already vendor-patched an AEC3 panic. Mitigation: stages 0–2 don't
  use AEC3; if Stage 3 misbehaves, `webrtc-audio-processing` (C++ FFI reference) is the documented
  upgrade path — but it adds a C++/cross-compile burden, so only if sonora's AEC proves unfit.
- **AEC drift** (the historical failure). The every-capture-frame render cadence fix is already in
  `capture.rs`; Stage 3's `render`/`post_apm` taps are exactly how we confirm it holds.
- **cpal device names ≠ pactl names.** Stage 0's enumeration log resolves this before we commit a
  name to config.
- **Cross-platform names.** Covered: ship `"default"`, override only on this box.

## 9. Rollback
Set `joi.yaml` `input_device`/`output_device` back to `default` and all three APM flags to `false`
— returns to the current PipeWire path. Code changes (device selection) are inert when names are
`"default"`.

## 10. Open decisions
1. **RESOLVED → device-by-name selection.** Implement the plumbing in §4 (fixes the dead config,
   Joi owns its audio path, portable mechanism). Not touching system PipeWire defaults.
2. Keep per-second level logs in-tree long-term, or gate behind an env/feature after bring-up?
   (defer until after Stage 3)
