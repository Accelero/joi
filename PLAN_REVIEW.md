# PLAN.md Review — Joi MVP Implementation Plan

Reviewer pass over `PLAN.md` against `SPEC.md`, the locked decisions, and verified
external facts (adk-rust v0.8, WebKitGTK getUserMedia/getDisplayMedia, Tauri v2 IPC).

---

## Verdict

This is a strong, genuinely well-structured plan. The crate split (`joi-core` pure / `joi-providers` /
`joi-testkit` / `src-tauri` composition root) is textbook ports-and-adapters, the dependency direction
is stated and correct, the mock-adapter conformance approach is the right way to prove
provider-agnosticism, and scope discipline against the post-MVP seams is mostly honored. A competent
fresh agent could build large parts of this without backfilling.

However, it is **not yet ready to execute as written**. The single biggest gap is that the riskiest,
most MVP-critical path — capturing mic and screen inside WebKitGTK on Linux — is under-specified and, on
the evidence, mis-framed: WebKitGTK requires non-default WRY settings and a `permissions-request`
handler (without it *all* getUserMedia/getDisplayMedia requests are silently denied), specific GStreamer
plugins that the dependency list omits, and the one documented working Tauri-on-Linux report needed the
**X11** GDK backend because **Wayland** produced GBM buffer errors — the opposite of the plan's
"Wayland portal" framing. M1 ("core loop on a mock") is sequenced as a warm-up but actually hides this
top project risk, and the spike for it is deferred to M2/M4. A few other items (adk-rust event API
unknown until M2, audio-out transport over Tauri's slow event channel, token-budget vs context-window
unit mismatch) need pinning down too.

**Readiness rating: needs revision** (close to "ready with fixes" — the architecture is sound; fix the
Linux-media de-risking, the M0/M1 audio-capability spike, and a handful of underspecified contracts and
it is executable).

---

## Strengths

- **Clean architecture, stated as enforceable rules** (§1): dependency inversion with `joi-core`
  holding zero Tauri/SDK deps, one composition root in `main.rs`, ports as traits, `unwrap`/`expect`
  denied in libs. This is the right backbone and matches SPEC §2.
- **Conformance-suite-as-proof** (§8, M5 task 4): running one suite against `MockSession`,
  `GeminiAdapter` (fixtures), and the `OpenAIAdapter` stub is exactly how you prove no Gemini-ism leaked
  (SPEC §16). Good.
- **Determinism baked in** (§1, §4.50): injected `Clock`, no wall-clock sleeps, `proptest` for history
  bounds and framing. The history/FSM tests in M3 are concrete and meaningful.
- **adk-rust kept behind the trait, in one file** (§0 rule 4, M2 task 2): churn risk is genuinely
  contained, fallback to gemini-rs is isolated. Correct execution of the locked decision.
- **Config layering is sound** (§4): figment defaults → TOML → `JOI_` env → CLI, secrets explicitly
  excluded, XDG paths via `directories`. The dev-only `GEMINI_API_KEY` fallback with a warning is a
  reasonable, well-bounded concession.
- **Post-MVP seams are real, not gold-plated** (§2 `tools/` seam-only, `ScreenSource`/`CaptureSource`
  enum, `SecretStore` trait): they exist as type surface without dragging post-MVP behavior into MVP.

---

## Issues by severity

### Blocking (fix before a fresh agent starts)

**B1. WebKitGTK media capture on Linux is under-specified and likely mis-framed (§3, M1, M4; SPEC §7,
§17).** Verified externally: getUserMedia/getDisplayMedia in WebKitGTK under Tauri/WRY needs
(a) WRY settings enabled — `set_enable_media_stream(true)`, `set_enable_webrtc(true)`,
`set_enable_media(true)`; (b) a handler on the `permissions-request` signal — *without it every capture
request is denied*, which a fresh agent will hit as a silent failure with no hint in this plan;
(c) GStreamer plugins (`gst-plugins-base`, `gst-plugins-good`, and critically `gst-plugins-bad` for
WebRTC) — **absent from the §3 dependency list**; (d) the only documented working Tauri-Linux report
required forcing the **X11** GDK backend because **Wayland** caused GBM buffer errors and
`WEBKIT_DISABLE_COMPOSITING_MODE=1`. The plan's §3/§17 framing assumes the Wayland `xdg-desktop-portal`
path is the happy path; the evidence says it is fragile and X11 may be the only thing that works today.
*Why it matters:* this is the core of the product (voice + screen). A fresh agent following §3 verbatim
will install incomplete deps, get silent denials, and have no debugging breadcrumb. *Fix:* add the WRY
settings + `permissions-request` handler as an explicit M0 task and call it out as the first thing to
verify; add the GStreamer plugin packages to §3; document the X11-vs-Wayland reality with a concrete
fallback (force X11 backend if Wayland capture fails); and **move a getUserMedia smoke check into M0**
(see B2).

**B2. M1 hides the top risk; the audio-capability spike is mis-sequenced (M1 vs §11).** §11 says "audio
latency/jitter over IPC: measure in M1" and "media off React state," and M1's exit is "type/click input
produces scripted response with no network." But M1 also requires real `getUserMedia` +
`AudioWorklet` mic capture and Web Audio playback to be *working in the actual WebKitGTK webview* — i.e.
B1 must already be solved — yet M1 is positioned as the gentle mock milestone before the "real" Gemini
work in M2. The plan can pass M1's stated exit criteria with mocked frames and never prove that capture
works in the target webview, deferring discovery of B1 to M2/M4. *Why it matters:* the project's
single largest unknown (does media capture even work in this shell on Linux?) is not de-risked until
two milestones in. *Fix:* add an explicit, tiny **M0 spike**: in the real Tauri window on Linux, prove
`getUserMedia({audio})` yields samples and Web Audio plays a tone — before any abstraction work. Then
M1's mock milestone is genuinely low-risk.

**B3. adk-rust's realtime API surface is unknown and the M2 mapping is the real schedule risk
(M2 task 2; SPEC §4.5).** Verified: adk-rust is v0.8.x, the `adk-realtime` crate does name
`gemini-live-2.5-flash-native-audio` (good — the model in §4.3 is real), supports server VAD and
mid-session context mutation, and OpenAI Realtime. But the public docs do not pin down whether it
exposes an event *stream* (which maps cleanly to our `BoxStream<SessionEvent>`), callbacks, or a
channel — nor exactly how interruption/barge-in and session-resumption handles surface. The plan's M2
task list ("map adk-rust events ↔ SessionEvent") assumes a clean mapping exists. *Why it matters:* if
adk-rust is callback-based or owns its own audio I/O loop (it has a "zero-allocation LiveKit audio
output path"), wrapping it behind a `&mut self` trait that takes `&[i16]` and returns a borrowed
`BoxStream` may fight the SDK's ownership model and force a redesign of the trait's event accessor.
*Fix:* make the **adk-rust API spike a named precondition of M2** (read `examples/gemini_audio` and the
`adk-realtime` crate API first; write down the actual connect/send/recv shape), and explicitly note that
`fn events(&mut self) -> BoxStream<'_, SessionEvent>` may need to become an owned channel/`Stream` taken
once at connect time if adk-rust's model demands it. Decide the gemini-rs fallback trigger *before*
starting M2, not mid-stream.

### Major (will cause rework or confusion)

**M-1. `events(&mut self) -> BoxStream<'_, SessionEvent>` is a fragile trait shape (§6; SPEC §4).** A
borrowed stream off `&mut self` means you cannot hold the event stream and call `send_audio(&mut self)`
concurrently — the borrow checker forbids simultaneous `&mut`. The `SessionManager` event pump needs to
read events while sends happen on the same session. *Why it matters:* this will not compile the way the
pump is described in §6 ("runs the event-pump task that maps SessionEvent → UiEvent"). *Fix:* take the
event stream **once** at/after `connect` and return an owned `Stream`/receiver
(`fn take_events(&mut self) -> impl Stream<...>` or hand back a `mpsc`/`broadcast` receiver), so sends
and receives are independent. Reconcile this with SPEC §4's `events()` signature and note the
divergence.

**M-2. Token budget vs context window — unit mismatch and pruning order bug risk (§4.3, M3 task 2;
SPEC §6.2).** The example config sets `token_budget = 1000000` "≈ model context window," and M3 prunes
"oldest beyond budget." Two problems: (a) `gemini-live-2.5-flash-native-audio` does **not** have a 1M
context window for the Live session — the realtime/live context is far smaller than the 1M text-model
window, so this default likely over-persists and may exceed what `initial_context` can re-seed; the plan
should not copy the text-model number. (b) "load newest-first within budget" + "prune oldest beyond
budget" must agree on accounting or you can persist a window that no longer fits when re-seeded. *Fix:*
pin the budget default to the *Live* session's real input limit (with headroom), document the unit
(tokens via chars/4) explicitly, and add a test that the re-seeded `initial_context` is guaranteed to
fit the configured budget (not just that the file is bounded).

**M-3. Audio-out transport choice is risky and self-contradictory (§6 "Tauri v2 `Channel<>`/event for
media", SPEC §11.2).** SPEC §11.2 says "audio out (BE→FE): binary 24 kHz PCM16 frames via **event
channel**." Verified: Tauri's event system is JSON-based and documented as slow for large/frequent
payloads; the recommended path for streaming binary (audio/video frames) is `tauri::ipc::Channel` (or a
custom URI scheme), not the event emitter. The plan lists `Channel<>`/event interchangeably and never
commits. *Why it matters:* routing 24 kHz PCM through the JSON event emitter will blow the ≤80 ms
latency target and stutter. *Fix:* commit to `tauri::ipc::Channel` for both mic-in and audio-out binary
streams in §6/M1, and update SPEC §11.2's "event channel" wording to match. Add the IPC-latency
measurement to the M0/M1 spike (B2).

**M-4. Frontend test/CI realism: jsdom cannot exercise the hot paths (§5, §8, M1 tests).** The plan
leans on Vitest for "downsample-to-16k correctness," "jitter buffer enqueue/flush," and "terminal write
throttling." `AudioWorklet`, `getUserMedia`, `getDisplayMedia`, and the WebGL xterm renderer do not
exist in jsdom; these tests can only cover pure helper functions, not the worklet/Web-Audio integration
that is the actual risk. *Why it matters:* the plan implies more coverage than is achievable, and the
genuinely risky integration (worklet ↔ IPC ↔ playback) is only checked by the non-gating, optional
`tauri-driver` e2e. *Fix:* be explicit that Vitest covers only pure DSP/framing/throttle helpers
(extract them so they're testable), and either promote a minimal in-webview smoke (manual or
`tauri-driver`) to a gating check for the media path or state clearly it is manual-verified per the M1
exit demo.

**M-5. Screen "native fallback" (scap/xcap) is a parallel capture pipeline, not a fallback, and M4
under-budgets it (M4 tasks 3, SPEC §7.3).** The webview path produces encoded frames in JS that cross
IPC; the native path captures in Rust and "never crosses IPC" — these are two different frame sources,
two different encode paths, two different start/stop/revoke implementations, and two different
source-enumeration semantics, both feeding `send_video_frame`. Calling it a "fallback" undersells that
M4 must build and test *both* end-to-end. *Why it matters:* M4 looks like one feature but is two; given
B1's Wayland fragility, the native path may end up being the *primary* Linux path, not the fallback.
*Fix:* split M4 deliverables explicitly into "webview capture path" and "native capture path," give each
its own tests, and decide which is the Linux default based on the B1 spike rather than assuming webview.

**M-6. SEC redaction is asserted but the mechanism is hand-wavy (§5, M5 task 3; SPEC SEC-8).** "A
helper ensures the API key / obvious secrets never enter logs" and the M5 SEC scan "feeds a key through
a full mock session." But the key is a high-entropy opaque string with no fixed prefix guaranteed across
providers; a generic regex won't reliably catch it, and structured `tracing` fields can leak it if any
adapter logs its config. *Why it matters:* SEC-5/8 are MVP requirements with a real test, but the test
as described (does the *known* key string appear?) only catches verbatim logging, not formatted/partial
leaks. *Fix:* specify the redaction as (a) wrap the key in a `Secret<String>` newtype whose `Debug`/
`Display` is redacted (e.g. `secrecy` crate) so it *cannot* be formatted into logs by construction, plus
(b) the scan test. Construction-level prevention beats a regex.

### Minor (polish)

- **m-1. `directories` vs `directories-next`/XDG on Tauri** (§4.2): Tauri v2 also provides
  `app_data_dir()`/`app_config_dir()` via its path API; using both `directories` and Tauri's path
  resolver risks divergent locations. Pick one source of truth (note that `joi-core` is pure, so
  `directories` in core is fine — but ensure `src-tauri` passes resolved paths in rather than
  re-deriving).
- **m-2. `enable-input/output-transcription` plumbing** (SPEC §4 `SessionConfig`): FR-3 transcript
  rendering depends on these flags being set in M2, but no M2 task explicitly wires them. Add a task.
- **m-3. CLI flags listed as precedence tier 4 (§4.1) but Tauri apps have no obvious argv path** —
  clarify how `--config`/`--log` reach `Config::load` in a windowed Tauri binary, or drop the tier for
  MVP.
- **m-4. `Speaker`/transcript `final_` field naming** (SPEC §4 uses `final_:`, a raw-keyword dodge) —
  fine, but ensure the TS mirror and the IPC parity test use `final` consistently; easy drift point.
- **m-5. "M5 build test: `cargo tauri build` ... or at least `cargo build --release`"** (§8) — the "or
  at least" escape hatch means CI may never actually prove a bundle builds. Make `cargo tauri build` the
  gate on Linux or explicitly accept the weaker check; don't leave it ambiguous.
- **m-6. `#![deny(warnings)]` "in CI"** (§5) — pinning `deny(warnings)` to source breaks builds on every
  future compiler lint bump; keeping it CI-only via `RUSTFLAGS=-Dwarnings` is the right call, but the
  wording mixes the attribute and the CI flag. State it's CI-only.

---

## Gaps / missing pieces (MVP needs these and they're absent or thin)

1. **No M0 in-webview media capability spike** (ties to B1/B2) — the project's biggest unknown is
   un-probed until M1+. Add it to M0.
2. **GStreamer runtime dependencies missing from §3** — `gst-plugins-base/good/bad` (and likely a
   `gst-plugin-pipewire` for portal screencast) are required for getUserMedia/getDisplayMedia in
   WebKitGTK and are not listed. The `setup-linux.sh` will produce a non-working media app.
3. **WRY/WebView init configuration missing** — `set_enable_media_stream`, `permissions-request`
   handler, and possibly forcing the X11 GDK backend are mandatory and have no home in any milestone.
   This belongs in M0 (Tauri shell) tasks.
4. **adk-rust API spike has no deliverable** — §11 says "validate in M2" but there is no
   "write down adk-rust's actual realtime API and confirm event-stream shape" task gating M2 (B3).
5. **Barge-in timing budget (<300 ms, FR-2) is never measured** — M2 task 4 implements the flush but no
   test/measurement validates the SPEC's latency target. The 80 ms playback latency target (§11) is
   similarly only aspirational with no measurement task.
6. **System-instruction / persona injection on resume** — §6/M3 seeds `initial_context` from history,
   but how the persisted system turn interacts with the configured `system_instruction` (dedupe? both?)
   is unspecified; "resume with empty/corrupt → start fresh" is covered, but the normal-resume
   composition of system prompt + restored turns is not.
7. **Concurrency/ownership model of `SessionManager` ↔ adapter** — `Box<dyn RealtimeSession>` held in an
   `Arc<SessionManager>` with `&mut self` send methods implies interior mutability (a `Mutex`/actor
   task). The plan never says which; this is load-bearing for whether M-1's borrow problem even arises.
   Specify: SessionManager should likely be an actor owning the session, with commands over an mpsc.
8. **Screenshare frame backpressure** — if encode/IPC can't keep up at the configured fps, what drops?
   Not specified; matters for the "revoke in-flight frames on stop" guarantee (FR-10).

---

## Specific recommendations (ordered, actionable punch list)

1. **Add an M0 "Linux webview media spike" task** (B1, B2, gap 1–3): in the real Tauri window, enable
   WRY media settings, register a `permissions-request` handler, install GStreamer plugins, and prove
   `getUserMedia({audio})` → samples and Web Audio tone playback work. Document X11-vs-Wayland outcome.
   Treat this as a go/no-go gate for the whole UI shell.
2. **Fix §3 dependency list**: add `gstreamer`, `gst-plugins-base`, `gst-plugins-good`,
   `gst-plugins-bad`, and the PipeWire/portal screencast plugin; note `xdg-desktop-portal` backend; note
   the X11 fallback env (`GDK_BACKEND=x11`, `WEBKIT_DISABLE_COMPOSITING_MODE=1`).
3. **Add an adk-rust API spike as a named M2 precondition** (B3): read `adk-realtime` + `examples/
   gemini_audio`, write down the real connect/send/recv/interrupt/resumption shape, and confirm or
   redesign the event accessor before coding the adapter. Pre-commit the gemini-rs fallback trigger.
4. **Redesign the events accessor** (M-1, gap 7): take the event stream once (owned `Stream`/receiver),
   make `SessionManager` an actor that owns `Box<dyn RealtimeSession>` and serves commands over mpsc;
   reconcile with SPEC §4 and note the divergence.
5. **Commit to `tauri::ipc::Channel` for binary mic-in and audio-out** (M-3), update SPEC §11.2 wording,
   and add an IPC round-trip latency measurement to the M0/M1 spike.
6. **Correct the history token budget** (M-2): set the default to the Live model's real input limit (not
   the 1M text window), document the chars/4 unit, and test that re-seeded `initial_context` fits.
7. **Make secret handling construction-safe** (M-6): introduce a redacted `Secret<String>` newtype
   (e.g. `secrecy`) for the key, in addition to the scan test.
8. **Split M4 into webview-capture and native-capture sub-deliverables** (M-5) with separate tests, and
   pick the Linux default from the B1 spike result rather than assuming webview-primary.
9. **Add measurement tasks/tests for the latency targets** (gap 5): barge-in <300 ms and playback
   ≤80 ms — at least a logged dev-overlay metric and a manual acceptance step.
10. **Clarify the smaller items**: frontend test scope vs jsdom limits (M-4), `cargo tauri build` as a
    real M5 gate (m-5), CLI-flag delivery in a windowed binary (m-3), transcription-flag wiring in M2
    (m-2), and `deny(warnings)` as CI-only (m-6).

---

*Sources consulted for the external claims above:* adk-rust GitHub (zavora-ai) and crates.io (v0.8.x,
`adk-realtime`, model `gemini-live-2.5-flash-native-audio`); WebKitGTK getDisplayMedia bug 186294 and
the `enable-media-stream`/`enable-mediastream` settings docs; the tauri-apps discussion #8426 on
working WebRTC/getUserMedia in WebKitGTK under Tauri (WRY settings, `permissions-request` handler,
GStreamer plugins, X11 backend requirement); Tauri v2 IPC docs and issues #7127 / #13405 / discussion
#5690 on raw payloads, slow event channel, and `tauri::ipc::Channel` for streaming binary.
