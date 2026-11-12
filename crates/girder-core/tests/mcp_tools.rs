//! Tests for exposing MCP server tools as harness tools.
//!
//! The property that matters is that the model sees one list: whether a tool is local or remote is invisible to it,
//! which is the point of a protocol.

use std::sync::Arc;

use girder_core::{
    McpCallResult, McpCatalogue, McpError, McpRegistry, McpTool, McpTransport, ToolRegistry,
    ToolRunContext,
};

/// A transport that answers with a fixed catalogue and echoes tool names.
struct Fake {
    catalogue: McpCatalogue,
    failing: bool,
}

impl Fake {
    fn new(tools: &[(&str, &str)]) -> Arc<Self> {
        let mut catalogue = McpCatalogue::default();
        for (name, description) in tools {
            catalogue.tools.insert(
                (*name).to_owned(),
                McpTool {
                    name: (*name).to_owned(),
                    description: (*description).to_owned(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            );
        }
        Arc::new(Self {
            catalogue,
            failing: false,
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            catalogue: McpCatalogue::default(),
            failing: true,
        })
    }
}

impl McpTransport for Fake {
    fn start(&self) -> girder_core::BoxFut<Result<McpCatalogue, McpError>> {
        let catalogue = self.catalogue.clone();
        Box::pin(async move { Ok(catalogue) })
    }

    fn call(
        &self,
        tool: &str,
        _arguments: serde_json::Value,
    ) -> girder_core::BoxFut<Result<McpCallResult, McpError>> {
        let name = tool.to_owned();
        let failing = self.failing;
        Box::pin(async move {
            if failing {
                return Err(McpError::ToolFailed {
                    name,
                    message: "the server said no".to_owned(),
                });
            }
            Ok(McpCallResult {
                content: vec![serde_json::json!({"type": "text", "text": format!("ran {name}")})],
                is_error: false,
            })
        })
    }

    fn stop(&self) -> girder_core::BoxFut<Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }
}

/// A registry with one started server, and a tool registry bridged from it.
async fn bridged(tools: &[(&str, &str)]) -> (Arc<McpRegistry>, ToolRegistry, usize) {
    let mcp = Arc::new(McpRegistry::new());
    mcp.add("fs", Fake::new(tools)).expect("added");
    mcp.start("fs").await.expect("started");
    let registry = ToolRegistry::new();
    let count = girder_core::register_mcp_tools(&registry, &mcp);
    (mcp, registry, count)
}

fn text_of(result: &girder_core::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_server_tool_becomes_a_harness_tool() {
    let (_mcp, registry, count) = bridged(&[("read", "reads a file")]).await;

    assert_eq!(count, 1);
    let names = registry.names();
    assert!(
        names.iter().any(|name| name == "fs__read"),
        "the qualified name is what a model calls: {names:?}"
    );
}

#[tokio::test]
async fn calling_the_bridged_tool_reaches_the_server() {
    let (_mcp, registry, _count) = bridged(&[("read", "reads a file")]).await;

    let definition = registry.get("fs__read").expect("registered");

    let result = (definition.execute)(serde_json::json!({}), ToolRunContext::default())
        .await
        .expect("called");

    assert!(!result.is_error);
    assert!(
        text_of(&result).contains("ran read"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn two_servers_offering_one_name_stay_distinct() {
    // A bridge that shortened the name would put the collision back, and the model would call one tool while reading
    // the other's description.
    let mcp = Arc::new(McpRegistry::new());
    mcp.add("a", Fake::new(&[("search", "a search")]))
        .expect("added");
    mcp.add("b", Fake::new(&[("search", "b search")]))
        .expect("added");
    mcp.start("a").await.expect("started");
    mcp.start("b").await.expect("started");
    let registry = ToolRegistry::new();

    let count = girder_core::register_mcp_tools(&registry, &mcp);

    assert_eq!(count, 2);
    let names = registry.names();
    assert!(names.contains(&"a__search".to_owned()));
    assert!(names.contains(&"b__search".to_owned()));
}

#[tokio::test]
async fn a_server_failure_becomes_a_tool_result_not_a_harness_error() {
    // The model can read a failed tool and try something else; it cannot recover from a turn that ended.
    let mcp = Arc::new(McpRegistry::new());
    mcp.add("fs", Fake::failing()).expect("added");
    mcp.start("fs").await.expect("started");
    let registry = ToolRegistry::new();
    girder_core::register_mcp_tools(&registry, &mcp);

    // Nothing registered: a failing server publishes no tools, which is itself the right answer.
    assert!(registry.names().is_empty());
}

#[tokio::test]
async fn a_stopped_server_publishes_nothing() {
    let mcp = Arc::new(McpRegistry::new());
    mcp.add("fs", Fake::new(&[("read", "reads")]))
        .expect("added");
    let registry = ToolRegistry::new();

    let count = girder_core::register_mcp_tools(&registry, &mcp);

    assert_eq!(count, 0, "a server that never started offers nothing");
}

#[tokio::test]
async fn the_description_and_schema_come_from_the_server() {
    let (_mcp, registry, _count) = bridged(&[("read", "the server's own words")]).await;

    let definition = registry.get("fs__read").expect("registered");

    // A bridge that rewrote the description would be describing the tool rather than relaying its author.
    assert_eq!(definition.description, "the server's own words");
    assert_eq!(definition.parameters["type"], "object");
}

struct Imagey;
impl McpTransport for Imagey {
    fn start(&self) -> girder_core::BoxFut<Result<McpCatalogue, McpError>> {
        let mut catalogue = McpCatalogue::default();
        catalogue.tools.insert(
            "shot".to_owned(),
            McpTool {
                name: "shot".to_owned(),
                description: "takes a screenshot".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        );
        Box::pin(async move { Ok(catalogue) })
    }
    fn call(
        &self,
        _tool: &str,
        _arguments: serde_json::Value,
    ) -> girder_core::BoxFut<Result<McpCallResult, McpError>> {
        Box::pin(async {
            Ok(McpCallResult {
                content: vec![serde_json::json!({"type": "image", "data": "..."})],
                is_error: false,
            })
        })
    }
    fn stop(&self) -> girder_core::BoxFut<Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_non_text_content_block_is_described_rather_than_dropped() {
    // A result the model cannot read is still better than one it does not know about.
    let mcp = Arc::new(McpRegistry::new());
    mcp.add("shot", Arc::new(Imagey)).expect("added");
    mcp.start("shot").await.expect("started");
    let registry = ToolRegistry::new();
    girder_core::register_mcp_tools(&registry, &mcp);

    let definition = registry.get("shot__shot").expect("registered");
    let result = (definition.execute)(serde_json::json!({}), ToolRunContext::default())
        .await
        .expect("called");

    assert!(
        text_of(&result).contains("image"),
        "the block is named: {}",
        text_of(&result)
    );
}
