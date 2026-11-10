//! Tests for the skill registry and the MCP client vocabulary.
//!
//! Both are registries over providers, and both get the same thing wrong if unguarded: a name collision that is
//! resolved silently hides which source answered.

use std::sync::Arc;

use girder_core::{
    McpCallResult, McpCatalogue, McpError, McpRegistry, McpStatus, McpTool, McpTransport,
    SkillDefinition, SkillError, SkillRegistry, SkillSummary, StaticSkills,
};

// ---- skills -----------------------------------------------------------------------------------

fn summary(name: &str, provider: &str, layer: u32) -> SkillSummary {
    SkillSummary {
        name: name.to_owned(),
        description: format!("{name} from {provider}"),
        provider: provider.to_owned(),
        layer,
    }
}

fn skill(name: &str, provider: &str, layer: u32) -> SkillDefinition {
    SkillDefinition::new(summary(name, provider, layer), format!("body of {name}")).expect("valid")
}

#[tokio::test]
async fn a_registered_skill_appears_in_the_catalog() {
    let mut provider = StaticSkills::new("builtin", 0);
    provider.add(skill("review", "builtin", 0));
    let registry = SkillRegistry::new();
    registry.register(Arc::new(provider)).expect("registered");

    let catalog = registry.catalog().await.expect("listed");

    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].name, "review");
    assert_eq!(catalog[0].provider, "builtin");
}

#[tokio::test]
async fn a_higher_layer_shadows_a_lower_one() {
    // A project's own `review` should shadow a built-in one, because the more specific source is the one the
    // person meant.
    let mut builtin = StaticSkills::new("builtin", 0);
    builtin.add(skill("review", "builtin", 0));
    let mut project = StaticSkills::new("project", 10);
    project.add(skill("review", "project", 10));

    let registry = SkillRegistry::new();
    registry.register(Arc::new(builtin)).expect("registered");
    registry.register(Arc::new(project)).expect("registered");

    let catalog = registry.catalog().await.expect("listed");

    assert_eq!(catalog.len(), 1, "one entry per name");
    assert_eq!(
        catalog[0].provider, "project",
        "the more specific source wins"
    );
}

#[tokio::test]
async fn the_winner_is_recorded_so_a_user_can_see_why() {
    // A user who expected their project's skill and got the built-in one needs to see which answered.
    let mut builtin = StaticSkills::new("builtin", 0);
    builtin.add(skill("review", "builtin", 0));
    let registry = SkillRegistry::new();
    registry.register(Arc::new(builtin)).expect("registered");

    let catalog = registry.catalog().await.expect("listed");

    assert_eq!(catalog[0].provider, "builtin");
    assert_eq!(catalog[0].layer, 0);
}

#[tokio::test]
async fn skills_from_different_providers_merge() {
    let mut a = StaticSkills::new("a", 0);
    a.add(skill("one", "a", 0));
    let mut b = StaticSkills::new("b", 0);
    b.add(skill("two", "b", 0));

    let registry = SkillRegistry::new();
    registry.register(Arc::new(a)).expect("registered");
    registry.register(Arc::new(b)).expect("registered");

    let names: Vec<String> = registry
        .catalog()
        .await
        .expect("listed")
        .into_iter()
        .map(|summary| summary.name)
        .collect();

    assert_eq!(names, vec!["one", "two"]);
}

#[tokio::test]
async fn a_skill_loads_its_body_and_files() {
    let mut provider = StaticSkills::new("builtin", 0);
    provider.add(skill("review", "builtin", 0).with_file("template.md", "# Template"));
    let registry = SkillRegistry::new();
    registry.register(Arc::new(provider)).expect("registered");

    let definition = registry.load("review").await.expect("loaded");

    assert_eq!(definition.body, "body of review");
    assert_eq!(
        definition.files.get("template.md").map(String::as_str),
        Some("# Template")
    );
}

#[tokio::test]
async fn an_unknown_skill_is_named_in_the_error() {
    let registry = SkillRegistry::new();
    let error = registry.load("nope").await.expect_err("unknown");

    let SkillError::Unknown { name } = error else {
        panic!("expected Unknown");
    };
    assert_eq!(name, "nope");
}

#[test]
fn a_duplicate_provider_name_is_refused() {
    // Two providers under one name would make a collision message name a source that is not the one that
    // answered.
    let registry = SkillRegistry::new();
    registry
        .register(Arc::new(StaticSkills::new("a", 0)))
        .expect("registered");

    let error = registry
        .register(Arc::new(StaticSkills::new("a", 1)))
        .expect_err("duplicate");

    assert_eq!(error, "a");
}

#[test]
fn providers_are_listed_in_precedence_order() {
    let registry = SkillRegistry::new();
    registry
        .register(Arc::new(StaticSkills::new("low", 0)))
        .expect("registered");
    registry
        .register(Arc::new(StaticSkills::new("high", 10)))
        .expect("registered");

    assert_eq!(registry.provider_names(), vec!["high", "low"]);
}

#[test]
fn a_skill_needs_a_name_a_description_and_a_body() {
    let empty_body = SkillDefinition::new(summary("x", "p", 0), "  ");
    assert!(empty_body.is_none(), "a skill with no body teaches nothing");

    let blank_name = SkillDefinition::new(
        SkillSummary {
            name: String::new(),
            description: "d".to_owned(),
            provider: "p".to_owned(),
            layer: 0,
        },
        "body",
    );
    assert!(blank_name.is_none());
}

// ---- mcp --------------------------------------------------------------------------------------

/// A transport that records whether it was started, and answers from a fixed catalogue.
struct FakeTransport {
    catalogue: McpCatalogue,
    fail_start: bool,
    fail_call: bool,
    started: std::sync::atomic::AtomicBool,
}

impl FakeTransport {
    fn new(catalogue: McpCatalogue) -> Arc<Self> {
        Arc::new(Self {
            catalogue,
            fail_start: false,
            fail_call: false,
            started: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            catalogue: McpCatalogue::default(),
            fail_start: true,
            fail_call: false,
            started: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

impl McpTransport for FakeTransport {
    fn start(&self) -> girder_core::BoxFut<Result<McpCatalogue, McpError>> {
        if self.fail_start {
            return Box::pin(async {
                Err(McpError::Transport {
                    name: "fake".to_owned(),
                    reason: "cannot start".to_owned(),
                })
            });
        }
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let catalogue = self.catalogue.clone();
        Box::pin(async move { Ok(catalogue) })
    }

    fn call(
        &self,
        tool: &str,
        _arguments: serde_json::Value,
    ) -> girder_core::BoxFut<Result<McpCallResult, McpError>> {
        let fail = self.fail_call;
        let name = tool.to_owned();
        Box::pin(async move {
            if fail {
                return Ok(McpCallResult {
                    content: vec![serde_json::json!({"type": "text", "text": "it broke"})],
                    is_error: true,
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

fn catalogue(tool: &str) -> McpCatalogue {
    let mut catalogue = McpCatalogue::default();
    catalogue.tools.insert(
        tool.to_owned(),
        McpTool {
            name: tool.to_owned(),
            description: format!("the {tool} tool"),
            input_schema: serde_json::json!({"type": "object"}),
        },
    );
    catalogue
}

#[tokio::test]
async fn a_started_server_offers_its_tools() {
    let registry = McpRegistry::new();
    registry
        .add("fs", FakeTransport::new(catalogue("read")))
        .expect("added");

    registry.start("fs").await.expect("started");
    let tools = registry.tools();

    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].0, "fs__read", "the server qualifies the name");
}

#[test]
fn two_servers_offering_one_name_do_not_collide() {
    // A bare name would let one shadow the other, and the model would call a tool that does not do what its
    // description said.
    assert_ne!(
        McpTool::qualified_name("a", "search"),
        McpTool::qualified_name("b", "search")
    );
}

#[tokio::test]
async fn a_stopped_server_offers_nothing() {
    // A stopped server keeps no catalogue: offering its tools would invite a call that cannot answer.
    let registry = McpRegistry::new();
    registry
        .add("fs", FakeTransport::new(catalogue("read")))
        .expect("added");

    assert!(registry.tools().is_empty());
    assert_eq!(registry.status("fs"), Some(McpStatus::Stopped));
}

#[tokio::test]
async fn a_server_that_fails_to_start_is_recorded_as_failed() {
    // A server that did not start must be distinguishable from one that was never configured.
    let registry = McpRegistry::new();
    registry
        .add("broken", FakeTransport::failing())
        .expect("added");

    let error = registry.start("broken").await.expect_err("failed");

    assert!(matches!(error, McpError::Transport { .. }));
    assert!(matches!(
        registry.status("broken"),
        Some(McpStatus::Failed { .. })
    ));
}

#[tokio::test]
async fn calling_a_tool_reaches_its_server() {
    let registry = McpRegistry::new();
    registry
        .add("fs", FakeTransport::new(catalogue("read")))
        .expect("added");
    registry.start("fs").await.expect("started");

    let result = registry
        .call("fs__read", serde_json::json!({}))
        .await
        .expect("called");

    assert_eq!(result.content[0]["text"], "ran read");
}

#[tokio::test]
async fn calling_a_tool_on_a_stopped_server_reports_not_ready() {
    let registry = McpRegistry::new();
    registry
        .add("fs", FakeTransport::new(catalogue("read")))
        .expect("added");

    let error = registry
        .call("fs__read", serde_json::json!({}))
        .await
        .expect_err("not ready");

    assert!(matches!(error, McpError::NotReady { .. }));
}

#[tokio::test]
async fn calling_an_unqualified_name_is_refused() {
    let registry = McpRegistry::new();

    let error = registry
        .call("read", serde_json::json!({}))
        .await
        .expect_err("unqualified");

    assert!(matches!(error, McpError::UnknownTool { .. }));
}

#[tokio::test]
async fn a_server_reported_failure_becomes_an_error() {
    let transport = Arc::new(FakeTransport {
        catalogue: catalogue("read"),
        fail_start: false,
        fail_call: true,
        started: std::sync::atomic::AtomicBool::new(false),
    });
    let registry = McpRegistry::new();
    registry.add("fs", transport).expect("added");
    registry.start("fs").await.expect("started");

    let error = registry
        .call("fs__read", serde_json::json!({}))
        .await
        .expect_err("failed");

    let McpError::ToolFailed { message, .. } = error else {
        panic!("expected ToolFailed, got {error:?}");
    };
    assert_eq!(
        message, "it broke",
        "the server's own message reaches the caller"
    );
}

#[tokio::test]
async fn stopping_clears_the_catalogue() {
    let registry = McpRegistry::new();
    registry
        .add("fs", FakeTransport::new(catalogue("read")))
        .expect("added");
    registry.start("fs").await.expect("started");
    assert_eq!(registry.tools().len(), 1);

    registry.stop("fs").await.expect("stopped");

    assert!(registry.tools().is_empty());
    assert_eq!(registry.status("fs"), Some(McpStatus::Stopped));
}

#[test]
fn a_duplicate_server_name_is_refused() {
    let registry = McpRegistry::new();
    registry
        .add("fs", FakeTransport::new(catalogue("a")))
        .expect("added");

    let error = registry
        .add("fs", FakeTransport::new(catalogue("b")))
        .expect_err("duplicate");

    assert_eq!(error, "fs");
}

#[test]
fn the_status_names_are_stable() {
    assert_eq!(McpStatus::Stopped.as_str(), "stopped");
    assert_eq!(McpStatus::Ready.as_str(), "ready");
    assert_eq!(
        McpStatus::Failed {
            reason: "x".to_owned()
        }
        .as_str(),
        "failed"
    );
}

#[test]
fn a_catalogue_lists_its_tools() {
    let catalogue = catalogue("read");
    assert!(catalogue.tool("read").is_some());
    assert!(catalogue.tool("write").is_none());
    assert_eq!(catalogue.tools().len(), 1);
}
