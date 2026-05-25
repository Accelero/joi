//! Provider-neutral tool contracts and the core-owned permission/runtime pipeline.
//!
//! `joi-core` owns the mechanism only: schemas, registry, permission policy, validation, and the
//! runtime bundle held by the session actor. Concrete built-in implementations live in
//! `joi-tools`; provider adapters only project schemas and forward native function calls/results.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::clock::Clock;

/// Identifier the provider assigns to a model-emitted tool call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ToolCallId(pub String);

/// A tool's name, description, and JSON-schema parameters, fed to `SessionConfig.tools`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolSchema {
    /// Unique tool name.
    pub name: String,
    /// Human/model-facing description.
    pub description: String,
    /// JSON-schema of the parameters object.
    pub parameters: serde_json::Value,
}

/// The result of running a tool, returned to the provider.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolResult {
    /// Whether the tool succeeded.
    pub ok: bool,
    /// Arbitrary JSON payload returned to the model.
    pub content: serde_json::Value,
}

impl ToolResult {
    /// Successful tool result.
    pub fn ok(content: serde_json::Value) -> Self {
        Self { ok: true, content }
    }

    /// Error result that is still delivered back to the model as a structured tool response.
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            content: serde_json::json!({ "error": msg.into() }),
        }
    }
}

/// The default policy action for a resolved tool request, and the action after policy evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    /// Run without asking the frontend.
    Allow,
    /// Ask the frontend to approve this exact call.
    Ask,
    /// Deny and send an error result to the model.
    Deny,
}

/// A resolved, auditable permission request for one specific tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    /// Policy key, e.g. `read`, `edit`, `bash`, or later `mcp:<server>`.
    pub key: String,
    /// Normalized subject: path, glob, grep pattern, command prefix, or external tool name.
    pub subject: String,
    /// Tool-provided baseline action before profile rules are applied.
    pub default_action: PermissionAction,
    /// One-line transcript/modal summary.
    pub summary: String,
    /// Full trusted detail for a permission prompt.
    pub detail: String,
}

/// Ambient context handed to a tool at run time.
#[derive(Clone)]
pub struct ToolCtx {
    /// Filesystem roots tools may read.
    pub readable_roots: Vec<PathBuf>,
    /// Filesystem roots tools may mutate.
    pub writable_roots: Vec<PathBuf>,
    /// Current working directory used to resolve relative tool paths/commands.
    pub cwd: PathBuf,
    /// Per-call timeout.
    pub timeout: Duration,
    /// Hard cap before a result reaches the model.
    pub max_output_bytes: usize,
    /// Whether shell commands may use obvious network operations.
    pub network: bool,
    /// Cancelled when the requesting session stops/closes/restarts.
    pub cancel: CancellationToken,
    /// Clock injected for deterministic tests and future tools.
    pub clock: Arc<dyn Clock>,
}

/// A provider-neutral tool. Registered by name; its [`ToolSchema`] feeds `SessionConfig.tools`.
#[async_trait]
pub trait Tool: Send + Sync {
    /// This tool's schema.
    fn schema(&self) -> ToolSchema;

    /// Resolve this call into an auditable permission request. Core applies policy to the result.
    fn permission(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Permission;

    /// Run the tool with model-supplied `args`.
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult;
}

/// Name-keyed tool registry.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace a tool keyed by its schema name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.schema().name;
        self.tools.insert(name, tool);
    }

    /// Find a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Sorted schemas for provider session setup.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|tool| tool.schema()).collect()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// One configured permission rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionRule {
    /// Permission key to match.
    pub key: String,
    /// Subject pattern. `*` matches all; a trailing `*` is treated as a prefix match.
    pub subject: String,
    /// Action to apply on match.
    pub action: PermissionAction,
}

/// Core-owned permission profile. Later MCP tools use the same profile with namespaced keys.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionProfile {
    /// Ordered rules; the first matching rule wins.
    pub rules: Vec<PermissionRule>,
}

/// Evaluate a resolved request against the profile.
pub fn evaluate_permission(profile: &PermissionProfile, request: &Permission) -> PermissionAction {
    profile
        .rules
        .iter()
        .find(|rule| rule.key == request.key && subject_matches(&rule.subject, &request.subject))
        .map_or(request.default_action, |rule| rule.action)
}

fn subject_matches(pattern: &str, subject: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return subject.starts_with(prefix);
    }
    pattern == subject
}

/// Runtime bundle injected into the session actor.
#[derive(Clone)]
pub struct ToolRuntime {
    /// Available tools.
    pub registry: Arc<ToolRegistry>,
    /// Context template cloned per call with a fresh cancellation token.
    pub ctx_template: ToolCtx,
    /// Permission rules for this run.
    pub permission_profile: PermissionProfile,
}

impl ToolRuntime {
    /// A runtime with no registered tools.
    pub fn disabled(clock: Arc<dyn Clock>) -> Self {
        Self {
            registry: Arc::new(ToolRegistry::new()),
            ctx_template: ToolCtx {
                readable_roots: Vec::new(),
                writable_roots: Vec::new(),
                cwd: PathBuf::new(),
                timeout: Duration::from_secs(30),
                max_output_bytes: 64 * 1024,
                network: false,
                cancel: CancellationToken::new(),
                clock,
            },
            permission_profile: PermissionProfile::default(),
        }
    }
}

/// Lightweight argument validation against the subset of JSON Schema Joi emits for built-ins.
pub fn validate_args(schema: &serde_json::Value, args: &serde_json::Value) -> Result<(), String> {
    let object = args
        .as_object()
        .ok_or_else(|| "tool arguments must be a JSON object".to_string())?;
    if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
        for item in required {
            let Some(name) = item.as_str() else {
                continue;
            };
            if !object.contains_key(name) {
                return Err(format!("missing required argument `{name}`"));
            }
        }
    }
    let Some(props) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(());
    };
    for (name, value) in object {
        let Some(prop) = props.get(name) else {
            continue;
        };
        if let Some(kind) = prop.get("type").and_then(serde_json::Value::as_str) {
            let ok = match kind {
                "string" => value.is_string(),
                "boolean" => value.is_boolean(),
                "integer" => value.is_i64() || value.is_u64(),
                "number" => value.is_number(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                _ => true,
            };
            if !ok {
                return Err(format!("argument `{name}` must be {kind}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_profile_first_match_wins() {
        let profile = PermissionProfile {
            rules: vec![PermissionRule {
                key: "read".to_string(),
                subject: "/tmp/*".to_string(),
                action: PermissionAction::Deny,
            }],
        };
        let request = Permission {
            key: "read".to_string(),
            subject: "/tmp/a".to_string(),
            default_action: PermissionAction::Allow,
            summary: String::new(),
            detail: String::new(),
        };
        assert_eq!(
            evaluate_permission(&profile, &request),
            PermissionAction::Deny
        );
    }
}
