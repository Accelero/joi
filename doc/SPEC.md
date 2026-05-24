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
- **Extensible toward tools.** Joi ships without tool/command execution today, but the model must be
  able to gain permission-gated tools (incl. shell access and memory) later without a redesign.

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
- **FR-16** Recover gracefully from transient connection drops, restoring the live session where the
  provider supports it and otherwise falling back to a context-restoring restart; mic/share state is
  never silently lost.
- **FR-17** **Panic-stop** halts the session, microphone, and screen sharing in a single action.

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

### 3.8 Tools & memory `[LATER]`
- **FR-24** `[LATER]` The agent can call registered tools, routed through a **permission gate** that
  requires deliberate, non-voice approval before any mutating or destructive action. The first such
  tool is permission-gated shell access, executed in a constrained sandbox.
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
- **SEC-3** `[NOW]` **No execution surface yet.** No model-driven command or tool path is reachable
  until the tool system (FR-24) is deliberately enabled.
- **SEC-4** `[LATER]` **Non-voice consent.** Mutating or destructive tool actions require a
  deliberate, non-spoken approval of the *resolved* action; approval times out to deny.
- **SEC-5** `[LATER]` **Sandboxed execution.** Executed commands run unprivileged, scoped, without
  ambient network/credentials by default, and are fully logged.
- **SEC-6** `[LATER]` **Anti-spoof.** Treat all on-screen/shared content as untrusted input; the
  permission prompt is application chrome, never rendered inside shared or streamed content.

---

## 5. Configuration

The user can configure, at minimum:
- **Provider & model** — which provider, the exact model, voice, and system instruction.
- **API key** — preferably via environment rather than on-disk plaintext.
- **History** — the token budget that bounds re-seeded context.
- **Media** — microphone/output devices and audio conditioning; screen frame rate, resolution cap,
  and quality.
- **Presentation** — theme/appearance of the active frontend.
- **Logging** — level and destination.

Configuration is layered so that environment values override file values.

---

## 6. Error handling & edge cases

- **Connection loss while running:** reflect a reconnecting state, attempt provider resume, then a
  context-restoring restart; never silently lose mic/share state.
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
   *(FR-14–16)*
4. After a full system restart, prior conversations are listed and any one can be resumed with its
   context restored; history is bounded. *(FR-18–22)*
5. Panic-stop halts session, mic, and sharing in one action. *(FR-17)*
6. The system behaves identically across providers behind the provider abstraction, with no
   provider-specific assumption leaking into conversation, history, or UI logic. *(§2)*
7. No model-driven tool or command path is reachable while tools are disabled, yet the design admits
   adding the gated tool system later without rework. *(SEC-3, FR-24)*
