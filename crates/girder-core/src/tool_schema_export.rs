//! Projecting the registered tool set into the two wire formats clients actually speak.
//!
//! The model is not the only consumer of a tool list. An editor that mounts this harness as an
//! MCP server reads [`to_mcp`]; a process that speaks the `OpenAI` function-calling protocol reads
//! [`to_openai`]. Both formats describe exactly the same registry, and the failure that matters is
//! not a slightly wrong schema — it is a tool that exists in one protocol and not the other, where
//! a client calling it gets "no such tool" with no way to tell that from a broken build.
//!
//! # Why the projection lives here instead of at each export site
//!
//! Two hand-written projections of one registry drift the first time a tool is added to one of
//! them. Keeping both projections in one module means a new tool appears in both formats or in
//! neither, and [`same_tool_set`] turns that from a convention into an assertion a test can make.
//!
//! # Why the ordering is fixed
//!
//! The output is sorted by tool name rather than left in registration order. Two exports of an
//! unchanged registry are then byte-identical, so a diff of a committed schema snapshot shows the
//! tools that actually changed instead of every tool below the first insertion.

use serde_json::{Value, json};

use girder_core::{ToolRegistry, ToolSchema};

/// One tool's argument schema, guaranteed to be a JSON Schema *object*.
///
/// A registry accepts any JSON value for `parameters`, but both protocols require an object here:
/// MCP clients reject a listing whose `inputSchema` is not one, and the `OpenAI` API rejects a
/// `parameters` that is not one. Substituting the empty object schema keeps the remaining tools
/// visible. Failing the whole listing because one tool registered a scalar schema would hide every
/// working tool behind the one broken one, and a client showing no tools at all is the worse
/// failure — it looks like the server has nothing to offer.
fn argument_schema(parameters: &Value) -> Value {
    if parameters.is_object() {
        return parameters.clone();
    }
    json!({ "type": "object", "properties": {} })
}

/// The registry's schemas, sorted by name.
///
/// Sorted here rather than trusted from [`ToolRegistry::schemas`]: that method happens to return
/// name order today because it walks a `BTreeMap`, but the guarantee this module makes is about
/// *its own* output, and a registry whose storage changed would otherwise change the exported
/// bytes silently.
fn ordered(tools: &ToolRegistry) -> Vec<ToolSchema> {
    let mut schemas = tools.schemas();
    schemas.sort_by(|left, right| left.name.cmp(&right.name));
    schemas
}

/// Exports every registered tool in the MCP `tools/list` shape.
///
/// The result is `{"tools": [{"name", "description", "inputSchema"}]}`, which is what an MCP
/// server answers when a client asks what it can do. `inputSchema` is the tool's argument schema
/// passed through unmodified whenever it is a JSON object: rewriting a working schema would
/// change what a client validates against, and the harness's tool authors already write JSON
/// Schema for `parameters`.
#[must_use]
pub fn to_mcp(tools: &ToolRegistry) -> Value {
    let entries: Vec<Value> = ordered(tools)
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": argument_schema(&tool.parameters),
            })
        })
        .collect();
    json!({ "tools": entries })
}

/// Exports every registered tool in the `OpenAI` function-calling shape.
///
/// The result is `{"tools": [{"type": "function", "function": {"name", "description",
/// "parameters"}}]}`. The same registry and the same argument schemas are used as [`to_mcp`]; only
/// the envelope differs, which is why a client speaking either protocol sees the same capability
/// set.
#[must_use]
pub fn to_openai(tools: &ToolRegistry) -> Value {
    let entries: Vec<Value> = ordered(tools)
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": argument_schema(&tool.parameters),
                },
            })
        })
        .collect();
    json!({ "tools": entries })
}

/// Whether two exports describe the same set of tools.
///
/// This is the drift guard: the two formats have different envelopes, so a comparison cannot be
/// `==` on the values, but a comparison of names is exactly the property that must hold. Names are
/// compared case-sensitively because both protocols are case-sensitive, and a rename in one format
/// only would otherwise pass as "the same tool".
///
/// A malformed export — not an object, no `tools` array, an entry without a name, an empty name,
/// or a name that appears twice within one export — answers `false`. A duplicate name is not a
/// quirk to tolerate: the client would have one tool shadow the other, so the two lists would
/// describe different capabilities even with equal name lists.
#[must_use]
pub fn same_tool_set(mcp: &Value, openai: &Value) -> bool {
    let Some(mcp_names) = exported_names(mcp, |entry| entry.get("name")) else {
        return false;
    };
    let Some(openai_names) = exported_names(openai, |entry| {
        entry
            .get("function")
            .and_then(|function| function.get("name"))
    }) else {
        return false;
    };
    mcp_names == openai_names
}

/// The sorted names one export lists, or `None` when the export is not the shape it claims.
///
/// `None` and an empty list mean different things: an empty list is a valid export of an empty
/// registry, while `None` is an export that cannot be read at all. Collapsing the two would let a
/// malformed document compare equal to an empty one.
fn exported_names(
    export: &Value,
    name_of: impl Fn(&Value) -> Option<&Value>,
) -> Option<Vec<String>> {
    let entries = export.get("tools")?.as_array()?;
    let mut names = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = name_of(entry)?.as_str()?;
        if name.is_empty() {
            return None;
        }
        names.push(name.to_owned());
    }
    names.sort_unstable();
    if names
        .windows(2)
        .any(|pair| matches!(pair, [left, right] if left == right))
    {
        return None;
    }
    Some(names)
}
