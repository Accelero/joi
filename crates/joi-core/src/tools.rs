//! `[POST]` Tool-system seam (SPEC §10). **No tools ship in the MVP.**
//!
//! These types exist only so tools — including the permission-gated `bash` tool and the memory
//! tool — can drop in later without rewrites (SPEC §10, DESIGN §6.4). They are referenced by the
//! [`crate::session::RealtimeSession`] trait (`send_tool_result`, [`crate::session::SessionConfig`]
//! `tools`) but are unused at runtime in the MVP.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

/// Ambient context handed to a tool at run time (post-MVP; intentionally empty for now).
#[derive(Debug, Default, Clone, Copy)]
pub struct ToolCtx;

/// A provider-neutral tool. Registered by name; its [`ToolSchema`] feeds `SessionConfig.tools`.
#[async_trait]
pub trait Tool: Send + Sync {
    /// This tool's schema.
    fn schema(&self) -> ToolSchema;
    /// Run the tool with model-supplied `args`.
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult;
}
