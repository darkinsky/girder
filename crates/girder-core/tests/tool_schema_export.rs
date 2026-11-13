//! Tests for the MCP and `OpenAI` tool-schema exports.
//!
//! The property that carries the weight here is agreement: two formats that describe different
//! tool sets are worse than one format, because a client on the wrong side of the disagreement
//! gets "no such tool" and cannot tell that from a broken build. So every test that builds a
//! registry asserts [`same_tool_set`] as well as the format's own shape, and the negative cases
//! check that the guard actually rejects a mismatch rather than answering `true` by construction.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written even before the crate root wires it in.
#[path = "../src/tool_schema_export.rs"]
mod tool_schema_export;

use std::sync::Arc;

use girder_core::{ToolDefinition, ToolRegistry, ToolResult};
use serde_json::{Value, json};
use tool_schema_export::{same_tool_set, to_mcp, to_openai};

/// A tool that does nothing. The exports read metadata only, so the body never runs; it exists
/// because a `ToolDefinition` cannot be built without one.
fn tool(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    ToolDefinition::new(
        name,
        description,
        parameters,
        Arc::new(|_args: Value, _ctx: girder_core::ToolRunContext| {
            Box::pin(async { Ok(ToolResult::text("ok")) })
        }),
    )
}

/// A registry holding the given tools.
fn registry(tools: Vec<ToolDefinition>) -> ToolRegistry {
    let registry = ToolRegistry::new();
    for definition in tools {
        registry.register(definition);
    }
    registry
}

/// The arguments schema most tools use.
fn object_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"]
    })
}

// ---- the empty registry -------------------------------------------------------------------------

#[test]
fn empty_registry_exports_empty_tool_lists() {
    let tools = ToolRegistry::new();

    assert_eq!(to_mcp(&tools), json!({ "tools": [] }));
    assert_eq!(to_openai(&tools), json!({ "tools": [] }));
    assert!(same_tool_set(&to_mcp(&tools), &to_openai(&tools)));
}

#[test]
fn empty_exports_do_not_look_malformed() {
    // An empty list is a legitimate answer for a registry with no tools; a guard that could not
    // tell it apart from a broken document would report a false drift on every empty deployment.
    assert!(same_tool_set(
        &json!({ "tools": [] }),
        &json!({ "tools": [] })
    ));
}

// ---- one tool, both formats ---------------------------------------------------------------------

#[test]
fn one_tool_exports_in_mcp_shape() {
    let tools = registry(vec![tool("read", "Reads a file", object_schema())]);

    let mcp = to_mcp(&tools);
    assert_eq!(
        mcp,
        json!({
            "tools": [{
                "name": "read",
                "description": "Reads a file",
                "inputSchema": object_schema(),
            }]
        })
    );
}

#[test]
fn one_tool_exports_in_openai_shape() {
    let tools = registry(vec![tool("read", "Reads a file", object_schema())]);

    let openai = to_openai(&tools);
    assert_eq!(
        openai,
        json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Reads a file",
                    "parameters": object_schema(),
                }
            }]
        })
    );
}

#[test]
fn one_tool_is_the_same_tool_in_both_formats() {
    let tools = registry(vec![tool("read", "Reads a file", object_schema())]);
    assert!(same_tool_set(&to_mcp(&tools), &to_openai(&tools)));
}

// ---- the drift guard ----------------------------------------------------------------------------

#[test]
fn same_tool_set_rejects_a_missing_tool() {
    let mcp = json!({ "tools": [{ "name": "read" }, { "name": "write" }] });
    let openai = json!({ "tools": [{ "type": "function", "function": { "name": "read" } }] });

    assert!(!same_tool_set(&mcp, &openai));
    assert!(!same_tool_set(&openai, &mcp));
}

#[test]
fn same_tool_set_rejects_a_renamed_tool() {
    let mcp = json!({ "tools": [{ "name": "read" }] });
    let openai = json!({ "tools": [{ "type": "function", "function": { "name": "Read" } }] });

    // Case matters in both protocols, so a case-only difference is still two different tools.
    assert!(!same_tool_set(&mcp, &openai));
}

#[test]
fn same_tool_set_rejects_a_duplicate_name() {
    // A duplicate name means one tool shadows the other at the client, so the exports describe
    // different capabilities even when the name *lists* are equal.
    let mcp = json!({ "tools": [{ "name": "read" }, { "name": "read" }] });
    let openai = json!({
        "tools": [
            { "function": { "name": "read" } },
            { "function": { "name": "read" } },
        ]
    });

    assert!(!same_tool_set(&mcp, &openai));
}

#[test]
fn same_tool_set_rejects_malformed_exports() {
    let openai = json!({ "tools": [{ "function": { "name": "read" } }] });

    assert!(!same_tool_set(&json!({}), &openai));
    assert!(!same_tool_set(&json!({ "tools": "read" }), &openai));
    assert!(!same_tool_set(
        &json!({ "tools": [{}] }),
        &json!({ "tools": [] })
    ));
    assert!(!same_tool_set(
        &json!({ "tools": [{ "name": "" }] }),
        &json!({ "tools": [] })
    ));
    // An `OpenAI` entry with no `function` wrapper is not a readable name, not a match for a tool
    // that happens to share its bare `name` field.
    assert!(!same_tool_set(
        &json!({ "tools": [{ "name": "read" }] }),
        &json!({ "tools": [{ "name": "read" }] })
    ));
}

// ---- several tools ------------------------------------------------------------------------------

#[test]
fn several_tools_export_in_both_formats() {
    let tools = registry(vec![
        tool("read", "Reads a file", object_schema()),
        tool("write", "Writes a file", object_schema()),
        tool("grep", "Searches files", json!({ "type": "object" })),
    ]);

    let mcp = to_mcp(&tools);
    let openai = to_openai(&tools);

    let mcp_names: Vec<&str> = mcp["tools"]
        .as_array()
        .expect("MCP export is a tools array")
        .iter()
        .map(|entry| entry["name"].as_str().expect("every MCP entry has a name"))
        .collect();
    let openai_names: Vec<&str> = openai["tools"]
        .as_array()
        .expect("`OpenAI` export is a tools array")
        .iter()
        .map(|entry| {
            entry["function"]["name"]
                .as_str()
                .expect("every `OpenAI` entry has a name")
        })
        .collect();

    assert_eq!(mcp_names, ["grep", "read", "write"]);
    assert_eq!(mcp_names, openai_names);
    assert!(same_tool_set(&mcp, &openai));
}

#[test]
fn a_later_registration_replaces_rather_than_duplicates() {
    let tools = registry(vec![tool("read", "old", object_schema())]);
    tools.register(tool("read", "new", object_schema()));

    let mcp = to_mcp(&tools);
    assert_eq!(mcp["tools"].as_array().expect("array").len(), 1);
    assert_eq!(mcp["tools"][0]["description"], "new");
}

// ---- schema pass-through ------------------------------------------------------------------------

#[test]
fn schemas_pass_through_unchanged() {
    // A rich schema with nesting, an enum, a default, and an empty required list. Every part of it
    // must survive: this module projects the envelope, not the argument schema, and a rewrite
    // would change what a client validates against.
    let schema = json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string", "enum": ["fast", "thorough"], "default": "fast" },
            "paths": { "type": "array", "items": { "type": "string" } },
            "nested": {
                "type": "object",
                "properties": { "depth": { "type": "number" } },
                "additionalProperties": false
            }
        },
        "required": []
    });
    let tools = registry(vec![tool("scan", "Scans", schema.clone())]);

    assert_eq!(to_mcp(&tools)["tools"][0]["inputSchema"], schema);
    assert_eq!(
        to_openai(&tools)["tools"][0]["function"]["parameters"],
        schema
    );
}

#[test]
fn a_non_object_schema_still_yields_a_valid_input_schema() {
    // `parameters` is an unvalidated `Value`, so a tool can register a scalar. Both protocols
    // require an object here; substituting the empty object schema keeps the other tools visible
    // instead of failing the whole listing. The tool is still listed, so the name sets agree.
    let tools = registry(vec![
        tool(
            "broken",
            "Registered a scalar schema",
            json!("not a schema"),
        ),
        tool("read", "Reads a file", object_schema()),
    ]);

    let mcp = to_mcp(&tools);
    let openai = to_openai(&tools);

    for entry in mcp["tools"].as_array().expect("array") {
        assert!(
            entry["inputSchema"].is_object(),
            "inputSchema must be a JSON Schema object, got {}",
            entry["inputSchema"]
        );
    }
    for entry in openai["tools"].as_array().expect("array") {
        assert!(entry["function"]["parameters"].is_object());
    }
    assert!(same_tool_set(&mcp, &openai));
    assert_eq!(mcp["tools"][0]["name"], "broken");
    assert_eq!(
        mcp["tools"][0]["inputSchema"],
        json!({ "type": "object", "properties": {} })
    );
}

// ---- stable ordering ----------------------------------------------------------------------------

#[test]
fn exports_are_byte_identical_across_calls() {
    let tools = registry(vec![
        tool("zebra", "Last by name", object_schema()),
        tool("alpha", "First by name", object_schema()),
        tool("middle", "Between them", object_schema()),
    ]);

    let first_mcp = to_mcp(&tools).to_string();
    let second_mcp = to_mcp(&tools).to_string();
    let first_openai = to_openai(&tools).to_string();
    let second_openai = to_openai(&tools).to_string();

    assert_eq!(first_mcp, second_mcp);
    assert_eq!(first_openai, second_openai);
    // The text is compared too, not only the values: `Value` equality hides key order, and a diff
    // of a committed snapshot sees the text.
    assert_eq!(to_mcp(&tools), to_mcp(&tools));
}

#[test]
fn ordering_is_by_name_regardless_of_registration_order() {
    let forward = registry(vec![
        tool("a", "A", object_schema()),
        tool("b", "B", object_schema()),
    ]);
    let backward = registry(vec![
        tool("b", "B", object_schema()),
        tool("a", "A", object_schema()),
    ]);

    assert_eq!(to_mcp(&forward).to_string(), to_mcp(&backward).to_string());
    assert_eq!(
        to_openai(&forward).to_string(),
        to_openai(&backward).to_string()
    );
}
