# Joi — Tool System Plan (built-in tools first, MCP-ready)

> **Status:** built-in tool pipeline implemented; MCP and hardening remain planned. Normative for
> follow-up tool work: it describes the target layers, type surface, and pipeline. Read `doc/ARCH.md`
> (layering) and `doc/SPEC.md` (`FR-24/25`, `SEC-3..6`) first. The current tree has the core
> registry/permission/dispatch pipeline, `crates/joi-tools`, Gemini/Mock provider bridges, and app
> injection behind `tools.enabled=false` defaults.
>
> **Reference implementation studied:** OpenCode's public tool/agent docs and current `sst/opencode`
> source (`packages/opencode/src/tool/*`), plus OpenAI Codex's public Rust CLI docs and current
> `codex-rs/core/src/tools/*` source. We borrow the durable shapes — one registry for built-ins and
> external tools, dedicated file/search tools instead of overusing shell, description-as-data,
> command-AST permission analysis, policy-driven approval, output caps, helpful error returns,
> provider-specific schema projection, and a central approval/sandbox/execution pipeline — and adapt
> them to Joi's realtime Rust actor architecture.

---

## 1. Goals & non-goals

**Goal.** Give the model a small agent-harness built-in set — **`read`, `list`, `glob`, `grep`,
`write`, `edit`, `bash`** — through Joi's existing realtime seam, using **native structured function
calling** (no text parsing), proven end-to-end **headless** with the Mock provider. The set deliberately
includes dedicated search/navigation tools so routine codebase inspection does not require shell
approval. Build every shared piece so that **MCP tools later slot in as just another source of
`dyn Tool`**, reusing the entire pipeline.

**Remaining non-goals.** No MCP client yet (designed-for, §12). No declarative/manifest tools. No
WASM plugins. No memory tool (`FR-25`) yet — it drops onto the same seam later. No subagent/task tool,
LSP tool, web tools, todo tool, skill loader, or patch/multiedit tool in the first built-in pass; those
are follow-ups after the common pipeline is proven. No provider-declared async/non-blocking tool calls
(`Capabilities.async_tool_calls` stays `false`; calls are request/response within a turn).

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

The registry, schema-shaped argument validation, the permission gate, the sandbox-bearing `ToolCtx`, output
caps, the `UiEvent`s, the transcript rendering, and the provider-side schema projection are **source-
agnostic**. Adding MCP means adding a *source* (and an MCP client) — never touching the pipeline.

The pipeline order is fixed and must stay central:

```
model call → schema validate → tool resolves permission request → core policy allow/ask/deny
           → sandbox/context selection → spawned execution → cap/shape output
           → send provider tool result → emit UI/tool audit events
```

That order is the architectural guardrail. Provider adapters never run tools; tool implementations
never prompt; the TUI never decides policy; and `bash` does not become the generic escape hatch for
basic codebase inspection.

---

## 3. Layering — where each piece lives

```
joi-core   (mechanism: contracts + registry + validation + policy + dispatch + permission state machine)
  ▲   ▲
  │   └── joi-tools   sealed: read/list/glob/grep/write/edit/bash impls, sandbox,
  │                   hashline editing + conservative bash classification
  │                   (tokio process; depends on joi-core)
  └────── joi-providers   Gemini: schema sanitizer, FunctionCall* mapping, send_tool_result
joi-app    builds the ToolRegistry from config (built-in now, MCP later) and injects it; adds the
           resolve_tool_permission command
joi-tui    renders tool-call/result lines + the permission modal; dispatches ResolveToolPermission
```

**Crate `crates/joi-tools/`** (added to the workspace `members`). It mirrors how
`joi-providers`/`joi-media` are sealed: concrete behavior behind a core trait. Layout:

```
crates/joi-tools/
  src/{lib, registry_build, sandbox, fs_path}.rs
  src/read/{mod.rs, description.md}
  src/list/{mod.rs, description.md}
  src/glob/{mod.rs, description.md}
  src/grep/{mod.rs, description.md}
  src/write/{mod.rs, description.md}
  src/edit/{mod.rs, description.md}
  src/bash/{mod.rs, description.md}
```

Target `joi-tools` implementation deps for future hardening: `ignore` + `globset` for walks/globs,
`regex` (or `grep-searcher`) for content search and `tokio::process` for `bash`. The initial
implementation uses a smaller dependency set and keeps all concrete tool execution out of `joi-core`.

**`check.sh` dependency assertions to add** (mirrors §7 of `ARCH.md`):
- `joi-core` carries **no** concrete tool-execution crate.
- `joi-tui` uses `UiEvent`/`Command` types from core and never names concrete tool impls.
- Full `jsonschema` validation is optional future hardening; the initial implementation validates
  the object/required/type subset emitted by built-ins.
- `tokio-util` is added where `CancellationToken` lives; `joi-tools` enables the `tokio/process`
  functionality it needs for `bash`.

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
    /// Classify this specific call. The tool returns the resolved action; core applies the configured
    /// permission profile to it (`allow`/`ask`/`deny`) before execution.
    fn permission(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Permission;
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult;
}

pub enum PermissionAction { Allow, Ask, Deny }

pub struct Permission {
    pub key: PermissionKey,          // read | edit | glob | grep | list | bash | external_directory | mcp:<server>
    pub subject: String,             // normalized path, glob, grep pattern, bash command prefix, or tool name
    pub default_action: PermissionAction,
    pub summary: String,             // one line for transcript/modal
    pub detail: String,              // full resolved action shown in trusted UI chrome
}

// Ambient context handed to every run (was an empty marker). Policy is injected here by joi-app.
pub struct ToolCtx {
    pub readable_roots: Vec<PathBuf>, // filesystem roots tools may read (sandbox boundary)
    pub writable_roots: Vec<PathBuf>, // filesystem roots tools may mutate
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

// Schema-shaped validation of model args against a tool's schema (pure).
pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String>;

// Core-owned policy engine, used for built-ins now and MCP later.
pub struct PermissionProfile { /* ordered rules: exact/pattern subject -> allow|ask|deny */ }
pub fn evaluate_permission(profile: &PermissionProfile, request: &Permission) -> PermissionAction;

pub struct ToolRuntime {
    pub registry: Arc<ToolRegistry>,
    pub ctx_template: ToolCtx,
    pub permission_profile: PermissionProfile,
}
```

Implementation note: today's `ToolCtx` is `#[derive(Copy)]` because it is empty. This plan removes
`Copy`; the manager clones a template and replaces `cancel` per call.

This is intentionally a two-step decision: a tool resolves the call into an auditable request, then
core applies policy. That matches the direction of modern harnesses: `write` and `edit` share the
`edit` permission key, `grep`/`glob`/`list` can be independently allowed, bash can match command
prefix patterns, and future MCP tools use the same exact/pattern machinery under namespaced tool names.
For the first implementation, pattern rules may be minimal (`*` and prefix/glob matching); the API must
not hard-code built-in-only policy.

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

- `SessionManager::spawn` gains a parameter `tool_runtime: ToolRuntime` (stored on the actor; registry,
  context template, and permission profile stay together).
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
4. request = tool.permission(&args, &self.tool_ctx()) — the resolved, auditable action
5. match evaluate_permission(&self.permission_profile, &request):
     Allow                  ⇒ spawn_tool(epoch, id, name, tool, args)            // run immediately
     Deny                   ⇒ tool_tx.send(epoch error "denied by policy: {request.summary}"); return
     Ask                    ⇒ pending.insert((epoch, id.clone()), PendingTool{ epoch, .. });
                              emit UiEvent::ToolPermission{epoch,id,name,request.summary,request.detail};
                              waits for Command::ResolveToolPermission{epoch,id, approve}
                              (future hardening: timeout task → deny)
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

The vendored Gemini adapter is patched to emit one `FunctionCallDone` per entry in `functionCalls`.
Keep that invariant before claiming parallel tool-call support for any provider.

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

| Tool | Params (JSON-Schema) | Permission key / default | Sandbox / limits |
|---|---|---|---|
| **`read`** | `path: string`, `max_bytes?: int` | `read` / `Allow` inside readable roots | path canonicalized + must resolve under `readable_roots`; byte cap; returns `text` where every line is prefixed `line:hash|` for hash-addressed edits |
| **`list`** | `path?: string`, `depth?: int`, `include_hidden?: bool` | `list` / `Allow` inside readable roots | bounded directory walk; ignores heavyweight dirs (`.git`, `target`, `node_modules`) unless requested; stable sorted output |
| **`glob`** | `pattern: string`, `path?: string` | `glob` / `Allow` inside readable roots | `globset`/`ignore` walker; result count cap; dotfile and ignore behavior explicit in args |
| **`grep`** | `pattern: string`, `path?: string`, `case_sensitive?: bool` | `grep` / `Allow` inside readable roots | match cap; each match includes line number, text, and the same `line:hash` tag used by `edit` |
| **`write`** | `path: string`, `content: string` | `edit` / `Ask` | path under a **writable** root; refuse symlink escape; size cap; create parent dirs only when requested |
| **`edit`** | `path: string`, `new: string`, `start?: string`, `end?: string`, `after?: string`, `old?: string`, `replace_all?: bool` | `edit` / `Ask` | preferred mode is hash-addressed: replace `start`..`end` inclusive or insert `after`, validating the current line hash before writing; legacy exact `old` replacement remains as fallback |
| **`bash`** | `command: string`, `timeout_ms?: int` | `bash` / `Ask`; policy may allow proven read-only command patterns | non-interactive child, `cwd` under readable root and writable only per config, scrubbed env, network command policy defaults to deny, `timeout`, stdout/stderr byte cap |

`write` and `edit` intentionally share the `edit` permission key, matching existing harness practice:
users usually think in terms of "can this agent modify files?" rather than separate write/edit toggles.
`read`, `list`, `glob`, and `grep` stay separate so a read-only agent can be broad or tight without
forcing shell access.

### 7.1 `bash` command classification — the SEC-4 enabler

Joi is not a coding-only harness, so bash classification stays conservative and language-agnostic.
Classify obvious command prefixes rather than depending on code-oriented parsers:
- **read-only allowlist** (`ls`, `cat`, `pwd`, `echo`, `grep`, `rg`, `find`, `git status/log/diff`, …)
  ⇒ contributes to `Allow` only when the command is simple enough to classify.
- **mutating** (`rm`, `mv`, `cp`, `mkdir`, `chmod`, `touch`, redirections `>`/`>>`, …) ⇒ forces
  `Ask`, and the resolved command goes into the permission `detail`.
- **network-capable commands** (`curl`, `wget`, `ssh`, package managers, installers, etc.) ⇒ denied
  unless `tools.network = true`, and even then still prompt unless explicitly allowed. This is policy
  classification, not kernel-level network isolation.

`permission(args)` returns a `Permission { key: bash, subject: normalized_command_prefix, ... }`.
Its `default_action` is `Allow` only when the whole pipeline is provably read-only **and** every touched
path is within `readable_roots`; mutating commands default to `Ask`; network-capable commands default
to `Deny` unless network is enabled, then `Ask`. The policy profile may still tighten any of those.
The gate is the backstop: argument expansion in a shell is undecidable in general (`$()`, variables,
globs, aliases/functions, command substitutions), so **anything not provably read-only prompts or
denies**, and the prompt shows what was parsed. Invoke `bash` without user rc files and with a scrubbed
environment so aliases/functions cannot change command meaning.

`bash` execution details are part of the contract, not implementation trivia: close stdin, allocate no
TTY, set `NO_COLOR=1`, set `GIT_PAGER=cat`/`PAGER=cat`, set `LC_ALL=C`, clear tool-unrelated secrets,
use `kill_on_drop`, kill the process group on cancellation/timeout, and return stdout/stderr/exit
status as structured JSON. Interactive programs and commands that request a TTY return an actionable
error rather than hanging the voice session.

---

## 8. Sandbox & permission model (SEC-3/4/5/6)

- **SEC-3 (no surface until enabled).** The registry is empty unless `tools.enabled = true`; an empty
  registry means `SessionConfig.tools` is empty and the no-op path (today's behavior) is preserved.
  Per-tool `enabled` flags gate each tool individually.
- **SEC-4 (non-voice consent of the resolved action).** Each tool first produces a
  `Permission` request carrying the **resolved** action (path / parsed commands / MCP server+tool).
  Core evaluates the request against a permission profile (`allow`/`ask`/`deny`, with exact and pattern
  matches), surfaces `Ask` as `UiEvent::ToolPermission`, and resolves it **only** by
  `Command::ResolveToolPermission` (never by voice). Future hardening should add timeout-to-deny.
- **SEC-5 (sandboxed execution).** *This pass:* lightweight scoped — unprivileged child, `cwd`/paths
  confined to `readable_roots`/`writable_roots` (canonicalize + reject symlink/`..` escape), scrubbed
  environment, policy-level denial/prompting for network-capable commands, wall-clock timeout, output
  byte cap, full `tracing` of every executed command. This is not a kernel-enforced network sandbox;
  do not claim that network is impossible until the strict OS sandbox follow-up lands.
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
  "builtins": [],                        // [] → standard set
  "readable_roots": [],                  // [] → [process cwd]; explicit list overrides
  "writable_roots": [],                  // [] → [process cwd]; never defaults to ~/.joi
  "timeout_secs": 30,
  "max_output_bytes": 65536,
  "network": false,
  "permissions": [
    { "key": "bash", "subject": "git status*", "action": "allow" }
  ]
}
```

`readable_roots`/`writable_roots` default to the process cwd because `~/.joi` contains config,
sessions, prompts, and possibly secrets; it must not become tool-readable or writable by default.
`validate()` checks: `timeout_secs ≥ 1`; `max_output_bytes ≥ 1024`; roots are absolute or
resolvable from cwd; writable roots are also readable roots (or are explicitly added to both);
`bash.timeout_ms` is in a sane range; each permission action is `allow`, `ask`, or `deny`; wildcard
rules are ordered with last-match-wins. Update `config/joi.example.json` and `doc/SPEC.md`.

---

## 10. Composition — `joi-app`

```rust
// new: build the registry and permission profile from config (MCP appended here later — §12).
fn build_tool_registry(cfg: &Config, cwd: &Path) -> ToolRuntime {
    let mut reg = ToolRegistry::default();
    if cfg.tools.enabled {
        joi_tools::register_builtins(&mut reg, &cfg.tools);   // read/list/glob/grep/write/edit/bash
    }
    ToolRuntime {
        registry: Arc::new(reg),
        ctx_template: ToolCtx::from_config(cfg, cwd),
        permission_profile: PermissionProfile::from_config(&cfg.tools.permission),
    }
}
```

In `build_with_config_path` (lib.rs:96), after the factory branch (lib.rs:107) and before
`SessionManager::spawn` (lib.rs:144), build the registry/runtime and pass it into
`spawn(..., tool_runtime)`. Policy lives here (which tools, which roots, cwd resolution, the
in-memory-history fallback already present). Add the `JoiApp::resolve_tool_permission(epoch, id,
approve)` method (mirrors the handle 1:1).

`joi_tools::builtins` constructs each enabled tool with its config and inserts it — this is the
sole place built-in tools are named, exactly as `build_session_factory` is the sole place a provider is
named.

### 10.1 Current codebase adaptations required

The current tree has these bindings. Future changes should preserve them:

- `crates/joi-core/src/tools.rs`: replace the empty `ToolCtx` marker with the real context, remove its
  `Copy` derive, add `Permission`/`PermissionProfile`/`ToolRegistry`/`ToolRuntime`, and keep `ToolSchema`
  and `ToolResult` provider-neutral.
- `crates/joi-core/src/manager.rs`: replace the existing `SessionEvent::ToolCall { .. }` no-op with
  the dispatch state machine; extend `Command`, `SessionManagerHandle`, and `SessionManager::spawn`.
- `crates/joi-core/src/session/event.rs`: add the tool UI events to the serializable `UiEvent` enum.
- `crates/joi-core/src/config.rs`: add `ToolsCfg` under the existing JSON config and keep defaults
  disabled.
- `crates/joi-app/src/lib.rs`: build and inject `ToolRuntime`; expose `resolve_tool_permission`.
- `crates/joi-providers/src/gemini.rs`: project schemas into `RealtimeConfig::with_tools`, map
  `FunctionCallDone` to `SessionEvent::ToolCall`, and override `send_tool_result`.
- `vendor/adk-realtime/src/gemini/session.rs`: keep `toolCall.functionCalls` emitting one
  `FunctionCallDone` per function call before claiming parallel tool-call support.
- `crates/joi-providers/src/mock.rs`: add scripted tool-call fixtures and record tool results for
  headless conformance tests.

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
  returns the server-advertised JSON Schema (the same `parameters: Value` shape), `permission(args,
  ctx)` returns a namespaced request such as `key=mcp:<server>`, `subject=<server>_<tool>`, defaulting
  to `Ask` unless the server/tool is explicitly configured read-only, and `run()` performs the JSON-RPC
  call. Drops into the **same `ToolRegistry`**.
- **Validation, permission policy, gate, dispatch, `ToolCtx` caps, `UiEvent`s, transcript, timeout** —
  all source-agnostic.
- **The Gemini schema sanitizer** (§6.2) runs on MCP schemas too — server schemas are exactly the
  arbitrary-JSON-Schema case the projection was built for. Big reuse win.

MCP-specific work (later, in `joi-tools` or a `joi-mcp` module): an MCP **client** (spawn/connect stdio
or HTTP servers, the JSON-RPC framing, `tools/list` discovery, lifecycle/health), and a `[tools.mcp]`
config block of server entries. `build_tool_registry` gains one step: append discovered MCP tools. If
MCP discovery must run before the session starts, `JoiApp::build_with_config_path` may need an async
variant or the registry builder may need a lazy refresh before `start`; the manager/provider contract
does not change. **Design rule for this pass:** never put a tool-source assumption into the manager or
the provider — they speak only `dyn Tool` and `ToolSchema`/`ToolResult`.

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
- **`joi-tools`.** `read`/`list`/`glob`/`grep`/`write`/`edit` happy paths **and** sandbox-escape
  rejection (`../`, symlink, out-of-root); `grep`/`glob` result caps; `edit` uniqueness rule; `bash`
  classification (read-only allow vs mutating ask vs network deny), timeout, output cap; assert each
  `description.md` is present and non-empty.
- **`joi-app` headless gate (invariant #8).** Build `JoiApp(MediaMode::None, Mock)` with a registry
  holding `EchoTool`; Mock emits a `ToolCall`; assert the tool ran and the result returned and the
  `UiEvent`s fired — **no devices, no GUI**. This *is* the headless proof for the feature.
- **`scripts/check.sh`.** Add the §3 dependency assertions; keep `fmt`/`clippy -D warnings`/`test`
  green.
- **Manual E2E (Gemini key).** Ask the model to list files, glob Rust files, grep for a symbol, read a
  file, write one (approve the prompt), edit one, and run `ls` (allowed by policy) then `rm`
  (prompted → deny → model reports denial). Confirm audio keeps flowing during a long `bash`.

---

## 14. Milestones (ordered; each ends green: build + tests + `clippy -D warnings`)

- **M1 — core + tool config skeleton. DONE.** Add `ToolsCfg` with secure disabled defaults; extend
  `Tool`/`ToolCtx`/`ToolResult`; add `Permission`, `PermissionProfile`, `ToolRegistry`,
  `validate_args`; add the
  `UiEvent`/`Command` variants; implement the manager dispatch state machine (internal
  `tool_tx`/`tool_rx`, epoch-keyed `pending`, `running` cancellation, timeout, `spawn_tool`,
  `complete_tool`) + `registry` param on `spawn`. Unit-tested with `EchoTool` + extended
  `ScriptedSession`, including cancellation on stop/close and stale-result drop across stop/restart.
  *Done:* `cargo test -p joi-core` green; no new device deps.
- **M2 — providers. PARTIAL DONE.** `with_tools` in `connect`; `FunctionCallDone`
  mapping; `send_tool_result`; Mock emits a `ToolCall`. Remaining: full Gemini schema sanitizer tests.
- **M3 — joi-tools (read/search/fs tools). INITIAL DONE.** New crate; `read`/`list`/`glob`/`grep`/`write`/`edit` +
  `sandbox`/`fs_path` + description files; `register_builtins` (non-bash only). *Done:*
  `cargo test -p joi-tools` incl. sandbox-escape and result-cap tests.
- **M4 — joi-tools (bash). PARTIAL DONE.** `bash` exists with non-interactive execution, timeout, and
  simple read-only/network classification. Remaining: stronger generic command classification,
  lightweight sandbox hardening, and broader output-cap tests. *Done:* bash classification/timeout
  tests cover allow/ask/deny policy outcomes.
- **M5 — joi-app + config docs. DONE.** `build_tool_registry` + injection; `resolve_tool_permission`; example
  config + `SPEC.md`; `check.sh` assertions added. *Done:* the headless gate test passes.
- **M6 — joi-tui. DONE.** Transcript tool lines + permission modal + keys; reducers unit-tested without a
  terminal. *Done:* manual E2E (§13) passes against a live key.
- **M7 — docs. DONE.** Flip `FR-24` from `[LATER]` to done in `SPEC.md`; note the tool layer in `ARCH.md`
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
- **Sandbox escape.** Canonicalize paths and reject anything resolving outside
  `readable_roots`/`writable_roots`, including via symlink and `..`; the keyboard gate is the backstop
  for the undecidable shell-expansion cases.
- **Network isolation.** The MVP has policy-level denial/prompting for network-capable commands, not
  kernel network isolation. True "network off" requires the strict OS sandbox follow-up.
- **Token/window blowout.** `cap_output` before `send_tool_result`; tool output competes with the Live
  API input window, not the 1M text window.
- **Parallel tool calls.** One turn may carry several `functionCalls`; the `id`-keyed `pending`/spawn
  design handles N concurrently — don't assume a single in-flight call.
