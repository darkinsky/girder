//! Exposing MCP server tools as this harness's tools.
//!
//! An MCP server publishes tools with a name, a description, and a JSON Schema. That is exactly the shape a
//! [`ToolDefinition`] holds, so the bridge is a translation rather than an adapter: the model sees one tool list, and
//! whether a tool is local or remote is invisible to it -- which is the point of a protocol.
//!
//! # Why the qualification survives into the tool name
//!
//! Two servers can offer `search`. The registry already qualifies as `server__tool`, and a bridge that shortened it
//! would put the collision back: the second server's tool would shadow the first and the model would call one while
//! reading the other's description.
//!
//! # Why a call failure is a tool result
//!
//! A server that fails is a tool that failed, which the model can read and work around. Reporting it as a harness error
//! would end the turn for a problem the model could have solved by trying something else.
//!
//! # Why the tools are registered once at startup
//!
//! A server's tool list is fetched when it starts, and a model-facing tool list that changed mid-turn would make a
//! request the provider had already seen a different list for. The registry is rebuilt when a server restarts, which is
//! a deployment action rather than a per-turn one.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::mcp::{CallResult, McpError, Registry};
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// How many MCP tools one call to the bridge registers.
///
/// A server that published thousands would fill the model's tool list with things it cannot use; the cap is a guard
/// against a misconfigured server, and the count is reported so the truncation is visible.
pub const MAX_BRIDGED_TOOLS: usize = 200;

/// Registers every tool a registry's ready servers publish.
///
/// Returns how many were registered, so a caller can report what a deployment actually offers rather than what it
/// configured.
pub fn register_mcp_tools(tools: &crate::tools::ToolRegistry, mcp: &Arc<Registry>) -> usize {
    let published = mcp.tools();
    let mut registered = 0;
    for (qualified, tool) in published.into_iter().take(MAX_BRIDGED_TOOLS) {
        tools.register(bridge(mcp, qualified, tool));
        registered += 1;
    }
    registered
}

/// Builds one tool definition that forwards to its MCP server.
fn bridge(mcp: &Arc<Registry>, qualified: String, tool: crate::mcp::Tool) -> ToolDefinition {
    let registry = Arc::clone(mcp);
    let name = qualified.clone();
    ToolDefinition::new(
        qualified,
        // The description is the server's own: a bridge that rewrote it would be describing the tool rather than
        // relaying what its author said it does.
        tool.description,
        tool.input_schema,
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            let name = name.clone();
            Box::pin(async move {
                match registry.call(&name, args).await {
                    Ok(result) => Ok(to_result(&result)),
                    // A server that failed is a tool that failed: the model can read this and try something else,
                    // which it could not do if the turn had ended.
                    Err(error) => Ok(ToolResult::error(describe(&error))),
                }
            })
        }),
    )
}

/// Turns an MCP content list into a tool result.
///
/// The blocks are opaque JSON to this layer, so text is extracted and anything else is described rather than dropped:
/// a result the model cannot read is still better than one it does not know about.
fn to_result(result: &CallResult) -> ToolResult {
    let mut out = String::new();
    for block in &result.content {
        if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        } else {
            // A block this layer does not understand: name what it is so a reader knows something arrived.
            let kind = block
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let _ = writeln!(out, "({kind} content not shown)");
        }
    }
    ToolResult::text(out)
}

/// A one-line explanation of an MCP failure.
fn describe(error: &McpError) -> String {
    error.to_string()
}
