# Joi — Tool System Plan (built-in tools first, MCP-ready)

> **Status:** plan, not yet implemented. Normative for the tool feature: it describes the layers,
> types, and pipeline the implementation must follow. Read `doc/ARCH.md` (layering) and `doc/SPEC.md`
> (`FR-24/25`, `SEC-3..6`) first. The realtime/provider seam already reserves the tool hooks
> (`joi-core/src/tools.rs`, `SessionConfig.tools`, `SessionEvent::ToolCall`,
> `RealtimeSession::send_tool_result`, `Capabilities.async_tool_calls`) — this plan fills them in.
>
> **Reference implementation studied:** `anomalyco/opencode` (`packages/opencode/src/tool/*`,
> `packages/llm/src/tool.ts`, `packages/llm/src/protocols/utils/gemini-tool-schema.ts`). We borrow
> its proven shapes — one tool type with typed *and* dynamic (JSON-Schema) construction,
> description-as-data, command-AST permission analysis, output caps, helpful error returns, and an
> isolated Gemini-schema projection — and adapt them to Joi's Rust/actor architecture.

---

## 1. Goals & non-goals

**Goal.** Give the model four permission-gated, sandboxed built-in tools — **`read`, `write`,
`edit`, `bash`** — through Joi's existing realtime seam, using **native structured function calling**
(no text parsing), proven end-to-end **headless** with the Mock provider. Build every shared piece so
that **MCP tools later slot in as just another source of `dyn Tool`**, reusing the entire pipeline.

**Non-goals (this pass).** No MCP client yet (designed-for, §12). No declarative/manifest tools. No
WASM plugins. No memory tool (`FR-25`) yet — it drops onto the same seam later. No `async`/non-blocking
tool calls (`Capabilities.async_tool_calls` stays `false`; calls are request/response within a turn).

**Invariant carried from `ARCH.md`.** All logic in Rust; the frontend only renders tool/permission
events and dispatches a resolve command. Mechanism (registry, validation, dispatch, permission state
machine) in `joi-core`; tool *implementations* + sandbox sealed in a new `joi-tools` crate; wiring in
`joi-app`. Wire-protocol specifics stay in `joi-providers`.

---

## 2. The one-pipeline principle

Every tool — built-in today, MCP tomorrow — reduces to **one interface and flows through one
pipeline**:

```
   source                       core (mechanism)                      provider (sealed)
 ┌──────────┐   Arc<dyn Tool>  ┌───────────────────────────────┐   ┌────────────────────┐
 │ built-in │ ───────────────▶ │ ToolRegistry                  │   │ schema sanitizer   │
 │ (joi-    │                  │   ├─ schemas() ──────────────────▶ │  → ToolDefinition   │
 │  tools)  │                  │   └─ get(name)                 │   │  (RealtimeConfig)  │
 ├──────────┤                  │ SessionManager dispatch:       │   │                    │
 │ MCP      │ ───(later)──────▶ │   validate → gate → run → result │◀─ │ FunctionCall* →    │
 │ (joi-    │                  │   (one state machine)          │   │   SessionEvent::Tool│
 │  tools)  │                  └───────────────────────────────┘   │   send_tool_result │
 └──────────┘                                                       └────────────────────┘
```

The registry, JSON-Schema validation, the permission gate, the sandbox-bearing `ToolCtx`, output
caps, the `UiEvent`s, the transcript rendering, and the provider-side schema projection are **source-
agnostic**. Adding MCP means adding a *source* (and an MCP client) — never touching the pipeline.

---

## 3. Layering — where each piece lives

```
joi-core   (mechanism: contracts + registry + validation + dispatch + permission state machine)
  ▲   ▲
  │   └── joi-tools   NEW, sealed: read/write/edit/bash impls, sandbox, bash command analysis
  │                   (deps: tree-sitter + tree-sitter-bash, tokio process; depends on joi-core)
  └────── joi-providers   Gemini: schema sanitizer, FunctionCall* mapping, send_tool_result
joi-app    builds the ToolRegistry from config (built-in now, MCP later) and injects it; adds the
           resolve_tool_permission command
joi-tui    renders tool-call/result lines + the permission modal; dispatches ResolveToolPermission
```

**New crate `crates/joi-tools/`** (added to the workspace `members`). It mirrors how
`joi-providers`/`joi-media` are sealed: concrete behavior behind a core trait. Layout:

```
crates/joi-tools/
  src/{lib, registry_build, sandbox, fs_path}.rs
  src/read/{mod.rs, description.md}
  src/write/{mod.rs, description.md}
  src/edit/{mod.rs, description.md}
  src/bash/{mod.rs, description.md, analyze.rs}   # analyze.rs = tree-sitter command classification
```

**`check.sh` dependency assertions to add** (mirrors §7 of `ARCH.md`):
- `joi-core` carries **no** `tree-sitter`/`tree-sitter-bash` and no tool-execution crate.
- `joi-tui` depends on `joi-app` + `joi-core` only — **never** `joi-tools` (it uses the `UiEvent`/
  `Command` types from core, not tool impls).
- the existing `jsonschema` (validation) is allowed in `joi-core` (pure, no device I/O).

---

## 4. The type surface

### 4.1 Reused as-is (already in the tree)

`joi-core/src/tools.rs`: `ToolCallId`, `ToolSchema { name, description, parameters: Value }`,
`ToolResult { ok, content: Value }`. `joi-core/src/session/mod.rs`: `SessionConfig.tools:
Vec<ToolSchema>` (mod.rs:49), `RealtimeSession::send_tool_result` (mod.rs:117), `Capabilities`
(mod.rs:78). `joi-core/src/session/event.rs:78`: `SessionEvent::ToolCall { id, name, args }`.

### 4.2 Extended / new in `joi-core`

```rust
// tools.rs — the trait grows a permission hook so each tool resolves WHAT a call will do (SEC-4).
#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;
    /// Classify this specific call. `Auto` runs without a prompt; `Ask` carries the *resolved*
    /// action to show the user (e.g. the exact file path, or the parsed shell commands).
    fn permission(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Permission { Permission::Auto }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult;
}

pub enum Permission {
    Auto,
    Ask { summary: String, detail: String },   // summary = one line; detail = full resolved action
}

// Ambient context handed to every run (was an empty marker). Policy is injected here by joi-app.
pub struct ToolCtx {
    pub roots: Vec<PathBuf>,        // filesystem roots tools may touch (sandbox boundary)
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub max_output_bytes: usize,    // hard cap before a result reaches the model
    pub network: bool,              // bash network policy flag; not kernel isolation by itself
    pub cancel: CancellationToken,   // cancelled when the requesting session stops/closes/restarts
    pub clock: Arc<dyn Clock>,
}

// Registry: name → tool. Concrete struct (mechanism), built by joi-app, held by the manager.
pub struct ToolRegistry { tools: BTreeMap<String, Arc<dyn Tool>> }
impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>);   // keyed by tool.schema().name
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    pub fn schemas(&self) -> Vec<ToolSchema>;          // → SessionConfig.tools at start
    pub fn is_empty(&self) -> bool;
}

impl ToolResult {
    pub fn ok(content: serde_json::Value) -> Self;
    pub fn error(msg: impl Into<String>) -> Self;      // ok=false, content={"error": msg}
}

// JSON-Schema validation of model args against a tool's schema (pure; `jsonschema` crate).
pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String>;
```

### 4.3 Event + command additions (the boundary contract)

```rust
// session/event.rs — UiEvent gains three serde variants (plain Rust, no ts-rs):
ToolCall      { id: ToolCallId, name: String, summary: String },          // invoked → transcript
ToolResult    { id: ToolCallId, name: String, ok: bool, summary: String },// finished → transcript
ToolPermission{ epoch: u64, id: ToolCallId, name: String, summary: String, detail: String }, // gate → modal

// manager.rs — Command gains one variant (host → engine, mirrors the 1:1 rule):
ResolveToolPermission { epoch: u64, id: ToolCallId, approve: bool },
```

`SessionManagerHandle` gains `resolve_tool_permission(epoch, id, approve)`; `JoiApp` mirrors it 1:1
(`resolve_tool_permission`). No tool state is invented by the frontend — it can only *resolve* a
pending gate using the opaque epoch/id pair the engine emitted.

---

## 5. The dispatch pipeline (the actor state machine)

The hard part is doing this **without blocking audio** and **without aliasing** the actor's
`session: Option<Box<dyn RealtimeSession>>` (which is a local in `SessionManager::run`, manager.rs:345,
and is the only thing that may call `send_tool_result`). Solution: tools run on **spawned tasks**, and
their outcomes return on a **new internal channel the actor `select!`s on** — the same shape the
provider event pump already uses (manager.rs:500-509).

### 5.1 New actor wiring

- `SessionManager::spawn` gains a parameter `registry: Arc<ToolRegistry>` (stored on the actor).
- Add `active_epoch: u64` to the actor. Increment it whenever the current provider session is invalidated
  (explicit stop, provider close/error, and before a new start). Capture it in every pending/running
  tool. The epoch is a race guard, not the primary stop mechanism: stop/close also cancels running
  tools so work does not continue after the user disconnects.
- Add an internal channel `tool_tx: mpsc::Sender<ToolOutcome>` (cloned onto the actor) + `tool_rx`
  local in `run`, where
  `struct ToolOutcome { epoch: u64, id: ToolCallId, name: String, result: ToolResult }`.
- Add `pending: HashMap<(u64, ToolCallId), PendingTool>` to the actor, where
  `PendingTool { epoch: u64, tool: Arc<dyn Tool>, args: Value, name: String }` — calls awaiting
  consent.
- Add `running: HashMap<(u64, ToolCallId), RunningTool>` to the actor, where
  `RunningTool { cancel: CancellationToken, handle: JoinHandle<()> }`. `do_stop`/provider close cancels
  tokens, aborts handles, and clears `pending`; stale approvals/results are still ignored because an
  already-queued `ToolOutcome` can race with cancellation.
- `run`'s `select!` gains an arm:
  ```rust
  Some(out) = tool_rx.recv() => self.complete_tool(out, &mut session).await,
  ```

### 5.2 At session start

In `do_start` (manager.rs:481), invalidate any old session/tools, then populate `SessionConfig.tools`
from the registry:
```rust
self.cancel_tools_for_epoch(self.active_epoch); // also clears pending/running for the old epoch
self.active_epoch = self.active_epoch.wrapping_add(1);
let mut cfg = SessionConfig::from_config(&self.config, initial_context, handle);
cfg.tools = self.registry.schemas();   // empty registry ⇒ no tools, behaves exactly as today
```

In `do_stop` and non-client provider close/error handling:
```rust
self.cancel_tools_for_epoch(self.active_epoch);
self.active_epoch = self.active_epoch.wrapping_add(1); // invalidate queued outcomes/UI approvals
```

### 5.3 On `SessionEvent::ToolCall { id, name, args }` (replaces the no-op at manager.rs:626)

```
1. let epoch = self.active_epoch
2. tool = registry.get(name)        — None ⇒ tool_tx.send(epoch error "unknown tool: {name}"); return
3. validate_args(tool.schema, args) — Err(e) ⇒ tool_tx.send(epoch error "invalid arguments: {e}"); return
4. match tool.permission(&args, &self.tool_ctx()):
     Auto                   ⇒ spawn_tool(epoch, id, name, tool, args)            // run immediately
     Ask { summary, detail} ⇒ pending.insert((epoch, id.clone()), PendingTool{ epoch, .. });
                              emit UiEvent::ToolPermission{epoch,id,name,summary,detail};
                              spawn a timeout task → Command::ResolveToolPermission{epoch,id, approve:false}
                              after `tools.permission_timeout_secs` (SEC-4: times out to deny)
   also emit UiEvent::ToolCall{id,name, summary} so the transcript shows the invocation
```

`spawn_tool` (non-blocking; audio keeps flowing):
```rust
fn spawn_tool(&mut self, epoch, id, name, tool, args) {
    let cancel = CancellationToken::new();
    let (mut ctx, tx) = (self.tool_ctx(), self.tool_tx.clone());
    let key = (epoch, id.clone());
    ctx.cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        let run = async {
            tokio::select! {
                _ = ctx.cancel.cancelled() => ToolResult::error("tool cancelled"),
                r = tool.run(args, &ctx) => r,
            }
        };
        let result = match timeout(ctx.timeout, run).await {
            Ok(r)  => r,
            Err(_) => ToolResult::error("tool timed out"),
        };
        let _ = tx.send(ToolOutcome { epoch, id, name, result }).await;
    });
    self.running.insert(key, RunningTool { cancel, handle });
}
```

`cancel_tools_for_epoch` cancels tokens first, then aborts task handles. Tools that spawn children must
also arrange cleanup inside their implementation: `bash` uses `tokio::process::Command::kill_on_drop`
and kills the child on `ctx.cancel.cancelled()` so aborting the Rust task does not leave a process
behind.

### 5.4 On `Command::ResolveToolPermission { epoch, id, approve }`

```
match pending.remove(&(epoch, id.clone())) { // pending map is the single source of truth (race-safe)
  None                  ⇒ ignore       // already resolved, or timed out — second resolution is a no-op
  Some(p) if p.epoch != active_epoch
                        ⇒ ignore       // stale UI action from a previous provider session
  Some(p) if approve    ⇒ spawn_tool(epoch, id, p.name, p.tool, p.args)
  Some(p) /* deny */    ⇒ tool_tx.send(ToolOutcome{ epoch, id, p.name, ToolResult::error("denied by user") })
}
```
A late approval and the timeout deny race only on `pending.remove`; whoever wins removes the entry, the
loser hits the `None` arm. No double `send_tool_result`.

### 5.5 On `ToolOutcome` (the `tool_rx` arm, has `&mut session`)

```rust
async fn complete_tool(&mut self, out: ToolOutcome, session: &mut Option<Box<dyn RealtimeSession>>) {
    self.running.remove(&(out.epoch, out.id.clone()));
    if out.epoch != self.active_epoch {
        tracing::debug!(id=%out.id.0, "dropping stale tool result from prior session");
        return;
    }
    if session.is_none() {
        tracing::debug!(id=%out.id.0, "dropping tool result after disconnect");
        return;
    }
    let capped = cap_output(out.result, self.config.tools.max_output_bytes); // §5.6
    if let Some(s) = session {
        if let Err(e) = s.send_tool_result(out.id.clone(), capped.clone()).await {
            tracing::warn!(error=%e, "send_tool_result failed");
        }
    }
    self.emit(UiEvent::ToolResult { id: out.id, name: out.name, ok: capped.ok, summary: short(&capped) });
}
```

After `send_tool_result`, Gemini Live continues the turn on its own (no `create_response` needed,
unlike typed text — verify against adk in M2). The model may emit several `functionCalls` in one turn;
each is an independent `SessionEvent::ToolCall` keyed by its own `id`, so N concurrent tools "just
work" through the `pending`/`spawn_tool` machinery.

### 5.6 The safety invariant ("nothing breaks")

**Every failure while the requesting session is still active becomes a `ToolResult { ok: false }`
returned to the model — never a panic, never a dropped active turn:** unknown tool, schema-invalid args,
permission denial, timeout, sandbox rejection, non-zero exit, oversized output. If the user stops or the
provider disconnects, pending/running tools are cancelled and any late outcomes are intentionally
dropped because there is no requesting session left to answer. `cap_output` truncates content to
`max_output_bytes` (keeping a "…truncated" marker) before it ever reaches the model — protecting the
Live API input window. The actor never `await`s a tool inline, so a hung tool cannot freeze audio or the
event loop.

---

## 6. The provider seam (Gemini) — sealed in `joi-providers`

### 6.1 Declare tools at connect (`gemini.rs:152`, in `connect`)

```rust
if !cfg.tools.is_empty() {
    let defs = cfg.tools.iter().map(to_tool_definition).collect::<Vec<_>>();
    rc = rc.with_tools(defs);                  // adk RealtimeConfig.tools
}
// to_tool_definition: ToolDefinition::new(name)
//   .with_description(desc)
//   .with_parameters(gemini_tool_schema::sanitize(&parameters))
```

### 6.2 Schema sanitizer — `gemini_tool_schema.rs` (ported from opencode's projection)

Gemini accepts only an OpenAPI subset of JSON Schema. Isolate the projection in one module
(provider-sealed). Port these transforms verbatim (with unit tests mirroring opencode's cases):
- coerce `enum` values to strings; `enum` + numeric `type` ⇒ `type: "string"`;
- `type: [T, "null"]` ⇒ `type: T` + `nullable: true`;
- `const` ⇒ `enum: [const]`;
- arrays must carry `items` (default `{ "type": "string" }`);
- non-object types: strip `properties`/`required`;
- filter `required` to keys present in `properties`;
- drop empty-object schemas; avoid/strip `anyOf`/`oneOf`/`allOf`.

### 6.3 Map function-call events → `SessionEvent::ToolCall`

In `EventMapper` (gemini.rs:339): handle `ServerEvent::FunctionCallDone { call_id, name, arguments,
.. }`. In the vendored adk this is already consolidated for Gemini and `arguments` is a JSON string,
so the provider mapper must parse it with `serde_json::from_str::<Value>()`. An empty string is treated
as `{}`. Invalid non-empty JSON is mapped to `args = {"__invalid_json": "<short error>"}` so core schema
validation rejects it and returns a normal tool error to the model instead of dropping the turn.

If a future provider surfaces `FunctionCallDelta`, accumulate deltas keyed by call id and emit a single
`SessionEvent::ToolCall { id, name, args }` on `FunctionCallDone` — exactly the open/close discipline
the transcript deltas already use (gemini.rs:354). No provider should pass raw text arguments into the
manager.

The vendored Gemini adapter currently collapses `toolCall.functionCalls` to `calls.first()`. M2 must
patch that adapter path to emit one `FunctionCallDone` per function call before claiming parallel
tool-call support for Gemini.

### 6.4 Return the result (`send_tool_result`, override the default at session/mod.rs:117)

```rust
async fn send_tool_result(&mut self, id: ToolCallId, result: ToolResult) -> Result<(), SessionError> {
    let session = self.session.as_ref().ok_or(SessionError::NotConnected)?;
    // Gemini wants functionResponse.response as an object; wrap non-object content.
    let output = if result.content.is_object() { result.content }
                 else { json!({ "ok": result.ok, "result": result.content }) };
    session.send_tool_response(ToolResponse { call_id: id.0, output })
        .await.map_err(|e| SessionError::Send(e.to_string()))
}
```

`Capabilities` is unchanged (`async_tool_calls: false`) — blocking tool calls work without it.

---

## 7. The built-in tools (`joi-tools`)

Each tool is a small struct implementing `Tool`. **Description-as-data:** the model-facing description
is `include_str!("description.md")` (the opencode `.txt` pattern, in Joi's `prompt.md` spirit). Schemas
are authored by hand as `serde_json::json!` JSON-Schema objects in the OpenAPI subset (so the sanitizer
is near-identity for our own tools). Each tool returns **actionable errors** (e.g. "file not found;
did you mean X?") so the model self-corrects.

| Tool | Params (JSON-Schema) | Permission | Sandbox / limits |
|---|---|---|---|
| **`read`** | `path: string`, `offset?: int`, `limit?: int` | `Auto` if `path` ∈ roots; else `Ask` | path canonicalized + must resolve under `roots`; line cap (2000) + byte cap (50 KB), "…truncated" marker (opencode constants) |
| **`write`** | `path: string`, `content: string` | `Ask { "write <path>" }` | path under a **writable** root; refuse symlink escape; size cap |
| **`edit`** | `path: string`, `old: string`, `new: string`, `replace_all?: bool` | `Ask { "edit <path>" }` | requires `old` to occur **exactly once** unless `replace_all` (the Joi-Edit uniqueness rule); path under writable root; returns a diff summary |
| **`bash`** | `command: string`, `timeout_ms?: int` | `Auto` only if **every** parsed command ∈ read-only allowlist; else `Ask` with the resolved command list | unprivileged child, `cwd` ∈ roots, scrubbed env, network command policy defaults to deny, `timeout`, stdout/stderr byte cap |

### 7.1 `bash` command analysis (`bash/analyze.rs`) — the SEC-4 enabler

Port opencode's `shell.ts` idea with the Rust `tree-sitter` + `tree-sitter-bash` crates: parse
`command` into an AST, walk `descendantsOfType("command")`, and for each extract the command name and
its file/dir arguments (unquoting, expanding `~`/`$HOME`/`$PWD`). Classify:
- **read-only allowlist** (`ls`, `cat`, `pwd`, `echo`, `grep`, `rg`, `find`, `git status/log/diff`, …)
  ⇒ contributes to `Auto` only when the command's flags are also safe. Examples: reject `find -exec`
  and `find -delete`; reject shell redirections; run `git` with config/env that disables pagers and
  external diff helpers; reject unknown flags whose behavior may execute helpers or write files.
- **mutating** (`rm`, `mv`, `cp`, `mkdir`, `chmod`, `touch`, redirections `>`/`>>`, …) ⇒ forces
  `Ask`, and the resolved targets go into the permission `detail`.
- **network-capable commands** (`curl`, `wget`, `ssh`, package managers, language package installers,
  etc.) ⇒ denied unless `tools.bash.network = true`, and even then still prompt unless explicitly
  allowed. This is policy classification, not kernel-level network isolation.

`permission(args)` returns `Auto` only when the whole pipeline is read-only **and** every touched path
is within `roots`; otherwise `Ask { summary: first line, detail: the parsed command + targets }`. The
gate is the backstop: argument expansion in a shell is undecidable in general (`$()`, variables, globs,
aliases/functions, command substitutions), so **anything not provably read-only prompts or denies**, and
the prompt shows what was parsed. Invoke `bash` without user rc files and with a scrubbed environment so
aliases/functions cannot change command meaning.

---

## 8. Sandbox & permission model (SEC-3/4/5/6)

- **SEC-3 (no surface until enabled).** The registry is empty unless `tools.enabled = true`; an empty
  registry means `SessionConfig.tools` is empty and the no-op path (today's behavior) is preserved.
  Per-tool `enabled` flags gate each tool individually.
- **SEC-4 (non-voice consent of the resolved action, timeout→deny).** The permission gate is driven by
  `Permission::Ask` carrying the **resolved** action (path / parsed commands), surfaced as
  `UiEvent::ToolPermission`, and resolved **only** by a keyboard `Command::ResolveToolPermission`
  (never by voice). Unresolved gates auto-deny after `tools.permission_timeout_secs`.
- **SEC-5 (sandboxed execution).** *This pass:* lightweight scoped — unprivileged child, `cwd`/paths
  confined to `roots` (canonicalize + reject symlink/`..` escape), scrubbed environment, policy-level
  denial/prompting for network-capable commands, wall-clock timeout, output byte cap, full `tracing` of
  every executed command. This is not a kernel-enforced network sandbox; do not claim that network is
  impossible until the strict OS sandbox follow-up lands.
  *Marked follow-up:* a strict OS sandbox (Linux `bubblewrap`/`landlock`) enforcing FS/network
  isolation in the kernel — slots behind the same `ToolCtx`/`run` boundary without pipeline changes.
- **SEC-6 (anti-spoof).** The permission modal is TUI chrome rendered by `joi-tui` — never inside
  shared/streamed content; tool args (which can echo screen/voice content) are treated as untrusted
  text and shown as data, not interpreted.

---

## 9. Config — the `[tools]` section (`joi-core/src/config.rs`)

A new `ToolsCfg` section (typed, validated, `#[serde(default)]` so old configs still parse), read as a
slice by `joi-app` when building the registry. **Default is disabled** (`enabled = false`) so existing
configs keep SEC-3's "no tool surface until explicitly enabled" invariant. Per-tool defaults may be
`enabled = true` under the disabled master switch, so opting in can be a one-line master-switch change.

```jsonc
"tools": {
  "enabled": false,                      // master switch (SEC-3); set true to opt in
  "roots": null,                         // null → [cwd, ~/.joi]; explicit list overrides
  "permission_timeout_secs": 60,         // SEC-4 deny timeout
  "max_output_bytes": 51200,             // 50 KB cap fed back to the model
  "read":  { "enabled": true },
  "write": { "enabled": true,  "permission": "ask" },
  "edit":  { "enabled": true,  "permission": "ask" },
  "bash":  { "enabled": true,  "permission": "ask", "network": false, "timeout_ms": 30000 }
}
```

`validate()` checks: `permission_timeout_secs ≥ 1`; `max_output_bytes ≥ 1024`; `bash.timeout_ms` in a
sane range; each `permission ∈ {auto, ask}`. Update `config/joi.example.json` and `doc/CONFIG.md`.

---

## 10. Composition — `joi-app`

```rust
// new: build the registry from config (built-in now; MCP appended here later — §12).
fn build_tool_registry(cfg: &Config) -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::default();
    if cfg.tools.enabled {
        joi_tools::register_builtins(&mut reg, &cfg.tools);   // read/write/edit/bash per flags
    }
    Arc::new(reg)
}
```

In `build_with_config_path` (lib.rs:96), after the factory branch (lib.rs:107) and before
`SessionManager::spawn` (lib.rs:144), build the registry and pass it into `spawn(... , registry)`.
Policy lives here (which tools, which roots, the in-memory-history fallback already present). Add the
`JoiApp::resolve_tool_permission(epoch, id, approve)` method (mirrors the handle 1:1).

`joi-tools::register_builtins` constructs each enabled tool with its config and inserts it — this is the
sole place built-in tools are named, exactly as `build_session_factory` is the sole place a provider is
named.

---

## 11. Frontend — `joi-tui`

Pure presentation over the new events (no logic):
- **Transcript:** fold `UiEvent::ToolCall` → a dim "⚙ {name}: {summary}" line; `UiEvent::ToolResult`
  → a ✓/✗ line. (`transcript.rs` reducer; `ui.rs` styling.)
- **Permission modal:** on `UiEvent::ToolPermission`, render a modal with `name`, `summary`, and the
  full `detail` (the resolved command/path); keys **`y`/`n`** (and `Esc` = deny) dispatch
  `Command::ResolveToolPermission { epoch, id, approve }` via
  `JoiApp::resolve_tool_permission(epoch, id, approve)`. The modal shows the deny-countdown (SEC-4).
  Pure reducer in `app.rs` (`on_ui_event`/`on_action`), no I/O.
- Keys are added in `keys.rs`; nothing else in the engine depends on the TUI.

---

## 12. MCP readiness — what this plan buys (the payoff)

When MCP is built next, it is **a new tool source, not a pipeline rewrite**. What it reuses unchanged:

- **`Tool` trait (dynamic mode).** An `McpTool { server, name, schema }` implements `Tool`: `schema()`
  returns the server-advertised JSON Schema (the same `parameters: Value` shape), `permission(args, ctx)`
  defaults to `Ask` (server tools are untrusted) unless annotated read-only, `run()` performs the
  JSON-RPC call. Drops into the **same `ToolRegistry`**.
- **Validation, gate, dispatch, `ToolCtx` caps, `UiEvent`s, transcript, timeout** — all source-agnostic.
- **The Gemini schema sanitizer** (§6.2) runs on MCP schemas too — server schemas are exactly the
  arbitrary-JSON-Schema case the projection was built for. Big reuse win.

MCP-specific work (later, in `joi-tools` or a `joi-mcp` module): an MCP **client** (spawn/connect stdio
or HTTP servers, the JSON-RPC framing, `tools/list` discovery, lifecycle/health), and a `[tools.mcp]`
config block of server entries. `build_tool_registry` gains one step: append discovered MCP tools. The
manager, providers, and TUI are untouched. **Design rule for this pass:** never put a tool-source
assumption into the manager or the provider — they speak only `dyn Tool` and `ToolSchema`/`ToolResult`.

---

## 13. Testing & verification

- **`joi-core` (unit, no devices).** `ToolRegistry` register/get/schemas; `validate_args` accept/reject;
  the dispatch state machine driven by a fake `EchoTool` + the existing `ScriptedSession` test double
  (manager.rs:763) extended to emit a `ToolCall`: assert auto-run → `send_tool_result` called →
  `UiEvent::ToolResult`; assert the `Ask` path emits `ToolPermission`, waits, and that
  approve/deny/timeout each produce the right outcome and are race-safe; assert stop/close cancels
  pending and running tools; assert a late tool result from a stopped/restarted prior session is dropped
  and never reaches the new `RealtimeSession`.
- **`joi-providers`.** `gemini_tool_schema::sanitize` unit tests (port opencode's cases);
  `FunctionCall*` → `ToolCall` mapping over fixtures; **Mock provider emits a deterministic `ToolCall`**
  so the rest of the engine can be driven with no network. Run the testkit conformance against Mock.
- **`joi-tools`.** `read`/`write`/`edit` happy paths **and** sandbox-escape rejection (`../`, symlink,
  out-of-root); `edit` uniqueness rule; `bash` classification (read-only auto vs mutating ask), timeout,
  output cap; assert each `description.md` is present and non-empty.
- **`joi-app` headless gate (invariant #8).** Build `JoiApp(MediaMode::None, Mock)` with a registry
  holding `EchoTool`; Mock emits a `ToolCall`; assert the tool ran and the result returned and the
  `UiEvent`s fired — **no devices, no GUI**. This *is* the headless proof for the feature.
- **`scripts/check.sh`.** Add the §3 dependency assertions; keep `fmt`/`clippy -D warnings`/`test`
  green.
- **Manual E2E (Gemini key).** Ask the model to read a file, write one (approve the prompt), edit one,
  and run `ls` (auto) then `rm` (prompted → deny → model reports denial). Confirm audio keeps flowing
  during a long `bash`.

---

## 14. Milestones (ordered; each ends green: build + tests + `clippy -D warnings`)

- **M1 — core + tool config skeleton.** Add `ToolsCfg` with secure disabled defaults; extend
  `Tool`/`ToolCtx`/`ToolResult`; add `Permission`, `ToolRegistry`, `validate_args`; add the
  `UiEvent`/`Command` variants; implement the manager dispatch state machine (internal
  `tool_tx`/`tool_rx`, epoch-keyed `pending`, `running` cancellation, timeout, `spawn_tool`,
  `complete_tool`) + `registry` param on `spawn`. Unit-tested with `EchoTool` + extended
  `ScriptedSession`, including cancellation on stop/close and stale-result drop across stop/restart.
  *Done:* `cargo test -p joi-core` green; no new device deps.
- **M2 — providers.** `gemini_tool_schema` sanitizer + `with_tools` in `connect`; `FunctionCall*`
  mapping; `send_tool_result`; Mock emits a `ToolCall`. *Done:* provider tests + conformance green.
- **M3 — joi-tools (fs tools).** New crate; `read`/`write`/`edit` + `sandbox`/`fs_path` + description
  files; `register_builtins` (fs only). *Done:* `cargo test -p joi-tools` incl. sandbox-escape tests.
- **M4 — joi-tools (bash).** `bash` + `analyze.rs` (tree-sitter), permission classification, lightweight
  sandbox, timeout, output cap; wire into `register_builtins`. *Done:* bash classification/timeout tests.
- **M5 — joi-app + config docs.** `build_tool_registry` + injection; `resolve_tool_permission`; example
  config + `CONFIG.md`; `check.sh` assertions added. *Done:* the headless gate test passes.
- **M6 — joi-tui.** Transcript tool lines + permission modal + keys; reducers unit-tested without a
  terminal. *Done:* manual E2E (§13) passes against a live key.
- **M7 — docs.** Flip `FR-24` from `[LATER]` to done in `SPEC.md`; note the tool layer in `ARCH.md`
  (mechanism in core, sealed `joi-tools`, MCP-ready registry); update `CLAUDE.md` status.
- **(future) M8 — MCP.** Per §12: MCP client + `[tools.mcp]` config + one extra `build_tool_registry`
  step. No pipeline changes.

---

## 15. Risks & gotchas

- **Aliasing `session` for `send_tool_result`.** Solved by the internal `tool_rx` arm in `run` (the only
  place with `&mut session`); never call `send_tool_result` from a spawned task.
- **Stop/disconnect while tools are running.** Cancellation is primary: stop/close cancels pending
  permission gates, cancels running tool tokens, and aborts task handles. `active_epoch` is still needed
  as the final race guard for outcomes already queued, tasks that complete while cancellation is being
  delivered, and stale UI approvals.
- **Child process leaks.** Aborting a Rust task is not enough for `bash`; child processes must use
  `kill_on_drop` and explicitly kill on cancellation/timeout.
- **Audio must not block on tools.** Tools always run on spawned tasks; the actor only routes results.
- **Permission race (approve vs timeout).** `pending.remove` is the single arbiter; the loser no-ops.
- **Gemini schema dialect.** Without the sanitizer, tool declarations are silently rejected and the
  model never calls them — port the full projection and unit-test it.
- **Gemini/adk event shape.** The vendored Gemini path exposes `FunctionCallDone` with stringified
  arguments and currently drops all but the first function call in a `toolCall`. Parse arguments in the
  provider mapper and patch/verify all function calls are emitted before relying on parallel calls.
- **Post-result continuation.** Verify in M2 whether adk needs an explicit trigger after
  `send_tool_response` or the Live model auto-continues (typed text needs `create_response`; tool
  responses should not — confirm).
- **Sandbox escape.** Canonicalize paths and reject anything resolving outside `roots`, including via
  symlink and `..`; the keyboard gate is the backstop for the undecidable shell-expansion cases.
- **Network isolation.** The MVP has policy-level denial/prompting for network-capable commands, not
  kernel network isolation. True "network off" requires the strict OS sandbox follow-up.
- **Token/window blowout.** `cap_output` before `send_tool_result`; tool output competes with the Live
  API input window, not the 1M text window.
- **Parallel tool calls.** One turn may carry several `functionCalls`; the `id`-keyed `pending`/spawn
  design handles N concurrently — don't assume a single in-flight call.
