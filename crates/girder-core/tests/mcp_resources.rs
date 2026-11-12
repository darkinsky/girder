//! Tests for MCP resources.
//!
//! The protocol separates a tool (callable) from a resource (readable by address), and so does this layer. A test that
//! only checked tools would not notice that a server offering documents was invisible.

use std::sync::Arc;

use girder_core::{
    McpCallResult, McpCatalogue, McpError, McpRegistry, McpResource, McpResourceContent, McpTool,
    McpTransport,
};

/// A transport that offers one document and no tools.
struct Documents {
    uri: String,
    body: String,
}

impl McpTransport for Documents {
    fn start(&self) -> girder_core::BoxFut<Result<McpCatalogue, McpError>> {
        let uri = self.uri.clone();
        Box::pin(async move {
            Ok(McpCatalogue {
                tools: std::collections::BTreeMap::new(),
                resources: vec![uri],
            })
        })
    }

    fn call(
        &self,
        _tool: &str,
        _arguments: serde_json::Value,
    ) -> girder_core::BoxFut<Result<McpCallResult, McpError>> {
        Box::pin(async {
            Err(McpError::UnknownTool {
                name: "none".to_owned(),
            })
        })
    }

    fn stop(&self) -> girder_core::BoxFut<Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }

    fn resources(&self) -> girder_core::BoxFut<Result<Vec<McpResource>, McpError>> {
        let uri = self.uri.clone();
        Box::pin(async move {
            Ok(vec![McpResource {
                uri,
                name: "notes".to_owned(),
                description: Some("the notes".to_owned()),
                mime_type: Some("text/markdown".to_owned()),
            }])
        })
    }

    fn read_resource(
        &self,
        uri: &str,
    ) -> girder_core::BoxFut<Result<McpResourceContent, McpError>> {
        let wanted = self.uri.clone();
        let body = self.body.clone();
        let asked = uri.to_owned();
        Box::pin(async move {
            if asked != wanted {
                return Err(McpError::ToolFailed {
                    name: asked,
                    message: "no such resource".to_owned(),
                });
            }
            Ok(McpResourceContent {
                uri: wanted,
                mime_type: Some("text/markdown".to_owned()),
                text: body,
            })
        })
    }
}

/// A server with tools only, which must still work.
struct ToolsOnly;

impl McpTransport for ToolsOnly {
    fn start(&self) -> girder_core::BoxFut<Result<McpCatalogue, McpError>> {
        Box::pin(async {
            let mut tools = std::collections::BTreeMap::new();
            tools.insert(
                "read".to_owned(),
                McpTool {
                    name: "read".to_owned(),
                    description: "reads".to_owned(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            );
            Ok(McpCatalogue {
                tools,
                resources: Vec::new(),
            })
        })
    }

    fn call(
        &self,
        _tool: &str,
        _arguments: serde_json::Value,
    ) -> girder_core::BoxFut<Result<McpCallResult, McpError>> {
        Box::pin(async {
            Ok(McpCallResult {
                content: Vec::new(),
                is_error: false,
            })
        })
    }

    fn stop(&self) -> girder_core::BoxFut<Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }
}

async fn registry() -> Arc<McpRegistry> {
    let registry = Arc::new(McpRegistry::new());
    registry
        .add(
            "docs",
            Arc::new(Documents {
                uri: "doc://notes".to_owned(),
                body: "# Notes\n".to_owned(),
            }),
        )
        .expect("added");
    registry.add("plain", Arc::new(ToolsOnly)).expect("added");
    registry.start("docs").await.expect("started");
    registry.start("plain").await.expect("started");
    registry
}

#[tokio::test]
async fn a_server_that_offers_documents_is_visible() {
    // A layer that only knew about tools would make this server invisible.
    let registry = registry().await;

    let resources = registry.resources().await;

    assert_eq!(resources.len(), 1, "{resources:?}");
    assert_eq!(resources[0].0, "docs");
    assert_eq!(resources[0].1, "doc://notes");
}

#[tokio::test]
async fn a_tool_only_server_offers_no_resources() {
    // The default implementation must not break a server that has none: tools and resources are separate capabilities.
    let registry = registry().await;

    let resources = registry.resources().await;

    assert!(
        resources.iter().all(|(server, _)| server != "plain"),
        "{resources:?}"
    );
}

#[tokio::test]
async fn reading_a_resource_returns_its_content() {
    let registry = registry().await;

    let content = registry
        .read_resource("docs", "doc://notes")
        .await
        .expect("read");

    assert_eq!(content.text, "# Notes\n");
    assert_eq!(content.mime_type.as_deref(), Some("text/markdown"));
}

#[tokio::test]
async fn reading_an_unknown_resource_is_an_error() {
    let registry = registry().await;

    let error = registry
        .read_resource("docs", "doc://absent")
        .await
        .expect_err("unknown resource");

    assert!(error.to_string().contains("no such resource"), "{error}");
}

#[tokio::test]
async fn reading_from_an_unknown_server_names_it() {
    let registry = registry().await;

    let error = registry
        .read_resource("absent", "doc://notes")
        .await
        .expect_err("unknown server");

    assert!(matches!(error, McpError::UnknownServer { .. }), "{error:?}");
}

#[tokio::test]
async fn a_server_without_resources_refuses_rather_than_returning_empty() {
    // An empty string would be rendered as empty content, which is a different statement from "this server has none".
    let registry = registry().await;

    let error = registry
        .read_resource("plain", "doc://notes")
        .await
        .expect_err("no resources");

    assert!(
        error.to_string().contains("no resources"),
        "the refusal names the reason: {error}"
    );
}

#[tokio::test]
async fn a_server_that_never_started_offers_nothing() {
    let registry = Arc::new(McpRegistry::new());
    registry
        .add(
            "docs",
            Arc::new(Documents {
                uri: "doc://notes".to_owned(),
                body: "x".to_owned(),
            }),
        )
        .expect("added");

    assert!(registry.resources().await.is_empty());
}
