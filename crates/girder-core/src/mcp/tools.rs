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

use crate::core::tools::{ToolDefinition, ToolResult, ToolRunContext};
use crate::mcp::{CallResult, McpError, Registry};

/// How many MCP tools one call to the bridge registers.
///
/// A server that published thousands would fill the model's tool list with things it cannot use; the cap is a guard
/// against a misconfigured server, and the count is reported so the truncation is visible.
pub const MAX_BRIDGED_TOOLS: usize = 200;

/// Registers every tool a registry's ready servers publish.
///
/// Returns how many were registered, so a caller can report what a deployment actually offers rather than what it
/// configured.
pub fn register_mcp_tools(tools: &crate::core::tools::ToolRegistry, mcp: &Arc<Registry>) -> usize {
    let published = mcp.tools();
    let mut registered = 0;
    for (qualified, tool) in published.into_iter().take(MAX_BRIDGED_TOOLS) {
        tools.register(bridge(mcp, qualified, tool));
        registered += 1;
    }
    registered
}

/// Registers `list_mcp_resources`, `list_mcp_resource_templates`, and `read_mcp_resource`.
///
/// The three tools are always present. A server name is an argument, so they can be registered
/// before any server has started and still refuse a name this deployment does not have.
pub fn register_mcp_resource_tools(tools: &crate::core::tools::ToolRegistry, mcp: &Arc<Registry>) {
    tools.register(resource_tool(
        mcp,
        "list_mcp_resources",
        "List resources available from an MCP server.",
        true,
        "resources/list",
    ));
    tools.register(resource_tool(
        mcp,
        "list_mcp_resource_templates",
        "List parameterized resource URI templates from an MCP server.",
        true,
        "resources/templates/list",
    ));
    tools.register(read_resource_tool(mcp));
}

fn resource_tool(
    mcp: &Arc<Registry>,
    name: &'static str,
    description: &'static str,
    cursor: bool,
    method: &'static str,
) -> ToolDefinition {
    let registry = Arc::clone(mcp);
    let mut properties = serde_json::json!({
        "server": { "type": "string", "description": "Configured MCP server name." }
    });
    if cursor {
        properties["cursor"] = serde_json::json!({
            "type": "string",
            "description": "Continuation cursor returned by this server."
        });
    }
    ToolDefinition::new(
        name,
        description,
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["server"]
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move {
                let Some(server) = required_string(&args, "server") else {
                    return Ok(ToolResult::error("`server` is required"));
                };
                let mut params = serde_json::json!({});
                if let Some(cursor) = args.get("cursor").and_then(serde_json::Value::as_str) {
                    if !cursor.is_empty() {
                        params["cursor"] = serde_json::json!(cursor);
                    }
                }
                match registry.request(&server, method, params).await {
                    Ok(value) => Ok(ToolResult::text(render_resource(&server, &value))),
                    Err(error) => Ok(ToolResult::error(error.to_string())),
                }
            })
        }),
    )
}

fn read_resource_tool(mcp: &Arc<Registry>) -> ToolDefinition {
    let registry = Arc::clone(mcp);
    ToolDefinition::new(
        "read_mcp_resource",
        "Read an MCP resource by URI from the named server. Use a listed URI or an expanded resource template.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "Configured MCP server name." },
                "uri": { "type": "string", "description": "Resource URI to read." }
            },
            "required": ["server", "uri"]
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move {
                let Some(server) = required_string(&args, "server") else {
                    return Ok(ToolResult::error("`server` is required"));
                };
                let Some(uri) = required_string(&args, "uri") else {
                    return Ok(ToolResult::error("`uri` is required"));
                };
                match registry
                    .request(&server, "resources/read", serde_json::json!({ "uri": uri }))
                    .await
                {
                    Ok(value) => Ok(ToolResult::text(render_resource(&server, &value))),
                    Err(error) => Ok(ToolResult::error(error.to_string())),
                }
            })
        }),
    )
}

fn required_string(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Renders a resource result, replacing binary blobs with their length.
#[must_use]
pub fn render_resource(server: &str, value: &serde_json::Value) -> String {
    let rendered = redact_blobs(value);
    format!(
        "MCP server: {server}\n{}",
        serde_json::to_string(&rendered).unwrap_or_else(|_| rendered.to_string())
    )
}

fn redact_blobs(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut next = serde_json::Map::new();
            for (key, item) in map {
                if key == "blob" {
                    if let Some(blob) = item.as_str() {
                        next.insert(
                            key.clone(),
                            serde_json::Value::String(format!(
                                "[binary resource: {} base64 characters; available to programmatic callers]",
                                blob.len()
                            )),
                        );
                        continue;
                    }
                }
                next.insert(key.clone(), redact_blobs(item));
            }
            serde_json::Value::Object(next)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_blobs).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod resource_tests {
    use super::render_resource;

    #[test]
    fn a_blob_is_replaced_by_its_length() {
        let value = serde_json::json!({
            "contents": [{ "uri": "file://a", "blob": "abcd" }]
        });
        let text = render_resource("files", &value);
        assert!(text.starts_with("MCP server: files\n"), "{text}");
        assert!(text.contains("4 base64 characters"), "{text}");
        assert!(!text.contains("abcd"), "{text}");
    }
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
