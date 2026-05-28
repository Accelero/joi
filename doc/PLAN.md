# Joi Implementation Plan

> Status: current forward plan for Joi. `doc/ARCH.md` remains normative for architecture and
> layering; `doc/SPEC.md` remains normative for requirements and configuration. This file orders the
> remaining product and engineering work. Completed or stale implementation notes have been removed.

## Priority 1. Automatic reconnect and live session recovery

Goal: recover from transient provider drops without treating them as explicit user stops.

- Add reconnect policy config: enabled flag, attempt budget, base backoff, max backoff or max elapsed
  time.
- Teach `SessionManager` to distinguish explicit stop, provider/server close, provider/network
  error, and auth/config failure.
- On transient close, emit reconnecting state, cancel tools from the old provider epoch, stop sending
  media to the dead session, and preserve intended mic mute and screen-share state.
- Reconnect by using provider-sealed native resume tokens when available; otherwise create a fresh
  realtime session seeded from the persisted session history.
- Never reconnect after explicit user stop, and allow stop to cancel the reconnect loop immediately.

Verification:

- Add `joi-core` manager tests for close-triggered reconnect, explicit-stop no-reconnect,
  stop-during-reconnect cancellation, budget exhaustion, and history reseeding.
- Add Mock provider fixtures for close/error/reconnect paths.
- Extend `joi-app` headless coverage with reconnect scenarios.

## Priority 2. Tool safety and provider schema hardening

Goal: strengthen the existing one-pipeline tool harness before adding new tool sources.

- Keep the core invariant: every tool source enters through `ToolRegistry`, schema validation,
  permission evaluation, the trusted UI gate, sandbox/context selection, output caps, and provider
  result return.
- Add a provider-sealed Gemini schema sanitizer before accepting arbitrary external tool schemas:
  enum coercion, nullable type projection, `const` to single-value enum, array `items` defaults,
  non-object cleanup, required-key filtering, and unsupported composition stripping.
- Convert invalid provider function-call arguments into ordinary invalid-args tool results instead
  of dropping the active turn.
- Consider replacing the current schema-shaped validator with full `jsonschema` validation behind
  the existing validation seam.
- Add timeout-to-deny for pending permission prompts.
- Improve bash permission ergonomics with clearer subjects, documented prefix/glob matching, and
  examples for common safe rules such as `git status*`.
- Strengthen bash cleanup: no inherited stdin, no TTY, scrubbed environment, process group kill where
  available, cancellation tests for child processes.
- Add optional OS sandboxing behind the current `ToolCtx`/runner boundary, starting with Linux
  `bubblewrap` or `landlock`.
- Keep network policy language precise: current policy-level denial is not kernel isolation until an
  OS sandbox is enabled.

Verification:

- Add Gemini schema sanitizer fixture tests and `FunctionCallDone` mapping tests.
- Add permission timeout race tests.
- Add output-cap tests for every built-in, especially `bash`.
- Add sandbox escape tests for paths, symlinks, and shell working directories.
- Add process cancellation/timeout tests that assert children are not left running.

## Priority 3. Anti-spoof hardening

Goal: keep trusted app chrome structurally distinct from model, tool, and screen-derived content.

- Audit permission surfaces: tool approvals now, memory approvals and settings/system-action prompts
  before they ship.
- Treat model output, tool arguments, and screen-derived text as untrusted data: render as plain text,
  strip or escape terminal control characters, and never interpret it as trusted chrome.
- Standardize trusted modal labels for requester, resolved action, scope, and approve/deny controls.
- Ensure permission prompts are rendered only by app UI, never inside shared screen content.
- For future GUI hosts, consider explicit capture exclusion or a visibly non-shared approval region.

Verification:

- Add TUI render tests for malicious-looking tool arguments and memory proposals.
- Add terminal control-character sanitization tests.
- Add wrapping tests for long untrusted text in permission modals.
- Manually verify that a fake prompt on the shared screen cannot be confused with the real UI modal.

## Priority 4. MCP tools

Goal: add MCP as a new tool source behind the existing `dyn Tool` pipeline.

- Add `tools.mcp` config for server name, enabled flag, transport, command/args/env or URL, and
  permission defaults or overrides.
- Implement an MCP client, likely in `joi-tools` or a new `joi-mcp` crate: JSON-RPC framing,
  initialize/lifecycle, `tools/list`, `tools/call`, shutdown, and health checks.
- Start with stdio transport; add HTTP later.
- Wrap each discovered MCP tool as a `Tool` implementation with server-advertised schema,
  namespaced permission requests such as `mcp:<server>`, and JSON-RPC execution.
- Append MCP tools during registry build in `joi-app`; if discovery must happen before session
  start, add an async registry-build path or a lazy refresh before `start()`.
- Keep source assumptions out of `SessionManager` and providers.

Verification:

- Add a fake MCP server fixture.
- Test discovery success/failure, call success/failure, timeout, cancellation, and permission gates.
- Test MCP schema projection through the Gemini sanitizer.
- Run headless app tests with Mock-emitted MCP tool calls.

## Priority 5. Generic settings UI

Goal: expose the existing runtime settings surface without adding one-off commands for every setting.

- Build the TUI settings panel from `JoiApp::settings_schema()`.
- Support current setting types: voice choice, terminal accent, and terminal background.
- Route changes through `JoiApp::update_setting()` so validation, persistence, and event broadcast
  remain in `joi-app` and `joi-core`.
- Keep `/voice` as a shortcut into the same settings machinery.
- Render apply timing clearly: immediate or next session start.
- Surface validation failures as UI errors without mutating local TUI state as if the setting applied.

Verification:

- Add pure TUI reducer tests for open/edit/cancel/apply flows.
- Add tests for invalid values and validation errors.
- Verify `UiEvent::Settings` refreshes visible state.
- Confirm config writes keep API keys redacted.

## Priority 6. Session rename exposure

Goal: expose the existing session rename mechanism through `JoiApp` and the TUI.

- Decide whether rename is current-session-only or can target any persisted session id.
- Add `rename_current_session(name)` or `rename_session(id, name)` at the app layer.
- Wire the method to `SessionStore::rename`.
- Add a TUI command such as `/rename`.
- Preserve explicit names over auto-derived first-user-turn names.
- Refresh session picker rows after rename.

Verification:

- Add `joi-app` tests for rename persistence.
- Add TUI command parsing and reducer tests.
- Verify resume/list ordering remains based on activity timestamps unless the product chooses
  otherwise.

## Priority 7. Long-term memory

Goal: support curated long-term facts across conversations, separate from raw persisted history.

- Define memory domain types in `joi-core`: id, content, tags or namespaces, source session id,
  timestamps, and optional provenance.
- Add a `MemoryStore` trait and local implementation, mirroring the session-store durability pattern
  with corruption-tolerant reads.
- Add config for enabled flag, max entries or bytes, and recall budget.
- Add permission-gated memory actions: propose/save, recall/search, update, and delete.
- Do not auto-save arbitrary transcript content in the first pass. Memory writes should be curated
  and explicitly approved.
- Inject relevant memories into provider initial context within a bounded budget, distinct from the
  conversation-history budget.
- Add trusted TUI approval chrome for proposed memory writes.

Verification:

- Unit-test persistence, index rebuild, corruption tolerance, and budgeted recall.
- Test approval and denial flows with Mock.
- Confirm memory is unavailable when disabled.
- Confirm memory content is not written into provider config or logs as a side effect.

## Priority 8. Future hosts and frontends

Goal: add new user surfaces as thin hosts over the existing engine.

- Define Seam B as a JSON mirror of `JoiApp` methods and `UiEvent`.
- Add one host transport at a time, such as a WebSocket server or a Tauri IPC backend.
- Keep media native: audio and screen capture remain in Rust between `joi-media`, the manager, and
  provider adapters.
- Add serde compatibility tests for key command/event payloads.
- Keep `scripts/check.sh` assertions preventing host/UI dependencies from entering engine crates.

Verification:

- Add command/event round-trip tests.
- Add a headless transport smoke test.
- Confirm engine crates remain free of host/UI dependencies.
- Confirm frontend code can mutate engine state only through exposed commands.

## Priority 9. Additional tool families

Goal: add new tool categories only after the common harness is hardened.

- Declarative or manifest tools: compile descriptors into `ToolSchema` plus a runner, with existing
  permission and execution flow.
- LSP tool: start read-only and provide code intelligence without shell access.
- Web tools: require explicit network policy and surface provenance plus fetched URLs.
- Patch/multiedit: build on hash-addressed edits, validate all edits before writing any file, and
  return structured partial-failure information without partial writes.
- Todo/task/subagent tools: define product semantics before implementation.
- WASM plugins: defer until permission and sandbox boundaries are stronger.

Verification:

- Each tool family must enter through `ToolRegistry`.
- Each tool family needs headless Mock coverage before TUI-specific workflow is considered done.
- No new tool family should require manager or provider pipeline changes.
