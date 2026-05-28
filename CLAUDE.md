# Joi — agent guide

> **Status:** the clean rewrite is implemented — the seven crates in `crates/` (core, providers,
> media, app, tools, testkit, tui) follow the architecture below; `scripts/check.sh` is green. The
> normative architecture lives in @doc/ARCH.md, and current requirements/config live in
> @doc/SPEC.md. Read ARCH.md before making structural decisions.

## What Joi is

Joi is a local, **provider-agnostic** voice + screen companion: a Rust application that connects a
human to a realtime multimodal model, streams audio/video both ways, renders a live transcript, and
**persists every conversation as a resumable session** (the Claude-Code model — list past sessions,
resume one, branch a new one, and the history re-seeds the model so it "remembers").

It is a **Rust app**, not tied to any single frontend. The rewrite is **TUI-first**; more frontends
will follow. Everything substantive is engine logic that any frontend drives — so don't design a
feature "for the TUI." Design it once in the engine; the frontends get it for free.

## Coding principles

1. **All logic lives in Rust; frontends are presentation and input only.** Session lifecycle,
   provider protocol, history, audio DSP, config, state — all in the engine. A frontend renders
   events and dispatches commands. It never computes, buffers, transforms, orchestrates, or touches
   media. Litmus test: *"Could a headless process with no UI do this?"* If yes, it's engine logic.

2. **The engine is host-agnostic.** It knows nothing about any specific frontend, GUI toolkit, or
   transport, and can run headless. No frontend/host/transport types ever leak into it.

3. **Stay provider-agnostic.** All wire-protocol knowledge is sealed behind a provider trait. The
   domain never names a concrete provider; adding or swapping one touches only the provider adapter.

4. **Separate domain mechanism from composition/policy.** *Mechanism* — what a thing is and how it
   behaves, including its own file I/O (session format, budget rules, auto-naming) — lives in the
   domain core. *Policy/wiring* — which directory to use, what to fall back to, which devices to
   drive — lives at the composition root. "No I/O in core" means no **device** I/O (mic, speaker,
   screen), not "never open a file"; the domain owns its own data files.

5. **One event surface, plain serde boundary types.** State flows **command-in, event-out**.
   Frontend-facing types are ordinary Rust structs/enums with `serde` derives. There is no `ts-rs`,
   generated bindings directory, or parity gate in this TUI-first tree.

6. **Prove it headless.** A feature isn't done until it works with no GUI. Keep the seams honest with
   the kind of dependency/parity checks ARCH.md describes.

When in doubt about which layer something belongs in, consult @doc/ARCH.md — it works the layering
through with session management as the running example, and ends with an invariants checklist.
