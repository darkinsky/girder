//! Tests for the `workflow_run` and `workflow_status` tools.
//!
//! The module under test is included by path because the crate's `lib.rs` is not this test's to change; when the
//! main agent adds `mod workflow_tool;` and the re-exports, the `use` statements below can name the crate instead
//! and this file keeps working unchanged (see the module's own documentation on the paths it spells).
//!
//! Four properties are the reason these tests exist:
//!
//! - `workflow_run` returns the finished run's status and result rather than an id, or the caller pays a second
//!   call to learn what it already asked for
//! - a failed script comes back as a readable error that names the run, or the caller cannot find the record of
//!   what failed
//! - `workflow_status` reports a run's phases, agents and result, or "status" would mean only "is it done"
//! - an unknown id is an error listing the ids that do exist, because a mistyped id is the usual cause
//!
//! Isolation: every test builds its own registry, and the two tests that ask for a listing use a per-test title
//! built from the process id and a counter, so a shared process running tests in parallel cannot make one test's
//! listing contain another's run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use girder_core::{
    RunId, ToolDefinition, ToolResult, ToolRunContext, WorkflowEngine, WorkflowEvent,
    WorkflowRegistry, WorkflowStartRequest, WorkflowStatus,
};
use serde_json::json;

/// An engine that reports a fixed sequence of phases and agents, then succeeds or fails.
struct Scripted {
    phases: Vec<&'static str>,
    agents: Vec<&'static str>,
    fail: Option<&'static str>,
}

impl WorkflowEngine for Scripted {
    fn run(
        &self,
        request: WorkflowStartRequest,
        report: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
    ) -> girder_core::BoxFut<Result<serde_json::Value, String>> {
        let phases: Vec<String> = self.phases.iter().map(|name| (*name).to_owned()).collect();
        let agents: Vec<String> = self.agents.iter().map(|name| (*name).to_owned()).collect();
        let fail = self.fail.map(str::to_owned);
        Box::pin(async move {
            // The title travels through the engine, so a tool that dropped it would still pass every assertion
            // about phases and fail here.
            let title = request.title;
            for name in phases {
                report(WorkflowEvent::PhaseStarted {
                    name: name.clone(),
                    at: 1,
                });
                for label in &agents {
                    report(WorkflowEvent::AgentStarted {
                        label: label.clone(),
                        prompt: format!("do {label}"),
                    });
                    report(WorkflowEvent::AgentFinished {
                        label: label.clone(),
                        output: "done".to_owned(),
                        result: Some(json!({"ok": true})),
                        at: 2,
                    });
                }
                report(WorkflowEvent::PhaseEnded { name, at: 3 });
            }
            if let Some(reason) = fail {
                return Err(reason);
            }
            Ok(json!({"answer": 42, "title": title}))
        })
    }
}

/// An engine whose run never finishes, for the cancellation path.
struct Blocking;

impl WorkflowEngine for Blocking {
    fn run(
        &self,
        _request: WorkflowStartRequest,
        _report: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
    ) -> girder_core::BoxFut<Result<serde_json::Value, String>> {
        Box::pin(async {
            std::future::pending::<()>().await;
            Ok(serde_json::Value::Null)
        })
    }
}

/// A registry over one scripted engine.
fn registry(phases: Vec<&'static str>, agents: Vec<&'static str>) -> Arc<WorkflowRegistry> {
    Arc::new(WorkflowRegistry::with_engine(Arc::new(Scripted {
        phases,
        agents,
        fail: None,
    })))
}

/// A unique title, so a listing assertion can tell this test's runs from another's.
fn unique(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{unique}", std::process::id())
}

/// The text a result renders to, whichever variant carried it.
fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Calls a tool with `args` and the default context.
async fn call(tool: &ToolDefinition, args: serde_json::Value) -> ToolResult {
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("the workflow tools report failures as results, not as errors")
}

/// The only run id the registry holds.
fn only_id(registry: &WorkflowRegistry) -> String {
    let ids = registry.ids();
    assert_eq!(ids.len(), 1, "exactly one run must be recorded: {ids:?}");
    ids.into_iter().next().expect("one id")
}

// ---- workflow_run -----------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_run_returns_completed_and_its_result() {
    let registry = registry(vec!["plan", "gather"], vec!["researcher"]);
    let tool = girder_core::workflow_run_tool(Arc::clone(&registry));
    let title = unique("success");

    let result = call(
        &tool,
        json!({"script": "plan(); gather()", "title": title, "arguments": {"topic": "rust"}}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains("completed"), "{text}");
    assert!(text.contains("answer"), "the result must be shown: {text}");
    assert!(text.contains("42"), "{text}");
    assert!(text.contains("researcher"), "agents must be shown: {text}");
    assert!(text.contains("gather"), "phases must be shown: {text}");
    assert!(text.contains(&title), "the title must be shown: {text}");

    // The receipt must name a run the registry really holds, or `workflow_status` has nothing to read.
    let id = only_id(&registry);
    assert!(text.contains(&id), "the receipt must name the run: {text}");
    let recorded = registry.get(&id).expect("recorded");
    assert_eq!(recorded.status, WorkflowStatus::Completed);
    assert_eq!(recorded.result, Some(json!({"answer": 42, "title": title})));
    assert_eq!(recorded.finished_agents(), 1);
}

#[tokio::test]
async fn a_failed_script_returns_a_readable_error_naming_the_run() {
    let registry = Arc::new(WorkflowRegistry::with_engine(Arc::new(Scripted {
        phases: vec!["plan"],
        agents: vec![],
        fail: Some("the script threw"),
    })));
    let tool = girder_core::workflow_run_tool(Arc::clone(&registry));

    let result = call(
        &tool,
        json!({"script": "plan()", "title": unique("failure")}),
    )
    .await;

    assert!(result.is_error, "a failed script is an error: {result:?}");
    let text = text_of(&result);
    assert!(text.contains("the script threw"), "{text}");
    // The failed run stays in the table, and the error must carry its id: the tool call that could have reported
    // the id failed, so this message is the only place it appears.
    let id = only_id(&registry);
    assert!(text.contains(&id), "the error must name the run: {text}");
    let recorded = registry.get(&id).expect("recorded");
    assert!(matches!(recorded.status, WorkflowStatus::Failed { .. }));
    assert!(
        text.contains("plan"),
        "the phases reached before the failure must be shown: {text}"
    );
}

#[tokio::test]
async fn a_registry_without_an_engine_returns_the_recorded_reason() {
    let registry = Arc::new(WorkflowRegistry::new());
    let tool = girder_core::workflow_run_tool(Arc::clone(&registry));

    let result = call(
        &tool,
        json!({"script": "s()", "title": unique("no-engine")}),
    )
    .await;

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("no workflow engine is mounted"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn a_blank_script_is_refused_before_a_run_is_recorded() {
    let registry = registry(vec![], vec![]);
    let tool = girder_core::workflow_run_tool(Arc::clone(&registry));

    let result = call(&tool, json!({"script": "  ", "title": "t"})).await;

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("non-blank"),
        "{}",
        text_of(&result)
    );
    // A refused request must not leave a run behind: a table full of runs that never started is a table whose
    // status reads are noise.
    assert!(registry.ids().is_empty());
}

#[tokio::test]
async fn a_non_string_argument_is_refused_with_its_name() {
    let registry = registry(vec![], vec![]);
    let tool = girder_core::workflow_run_tool(Arc::clone(&registry));

    let result = call(
        &tool,
        json!({"script": "s()", "title": "t", "arguments": {"count": 3}}),
    )
    .await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("count"), "{}", text_of(&result));
    assert!(registry.ids().is_empty());
}

#[tokio::test]
async fn two_runs_get_distinct_ids() {
    let registry = registry(vec![], vec![]);
    let tool = girder_core::workflow_run_tool(Arc::clone(&registry));

    for _ in 0..2 {
        let result = call(&tool, json!({"script": "s()", "title": unique("distinct")})).await;
        assert!(!result.is_error, "{}", text_of(&result));
    }

    let ids = registry.ids();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test]
async fn cancelling_the_call_stops_waiting_and_marks_the_run_cancelled() {
    let registry = Arc::new(WorkflowRegistry::with_engine(Arc::new(Blocking)));
    let tool = girder_core::workflow_run_tool(Arc::clone(&registry));

    let ctx = ToolRunContext::default();
    let cancel = ctx.cancel.clone();
    let canceller = tokio::spawn(async move {
        // Long enough that the tool has polled the engine and is genuinely waiting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let result = (tool.execute)(json!({"script": "slow()", "title": unique("cancel")}), ctx)
        .await
        .expect("handled");
    canceller.await.expect("the canceller ran");

    assert!(result.is_error);
    let text = text_of(&result);
    assert!(text.contains("cancelled"), "{text}");
    // The tool must record the cancellation rather than leaving a `running` row forever.
    let id = only_id(&registry);
    assert_eq!(
        registry.get(&id).expect("recorded").status,
        WorkflowStatus::Cancelled
    );
}

// ---- workflow_status --------------------------------------------------------------------------

/// A registry holding one finished run, started directly so the status tool is tested on its own.
async fn finished_run() -> (Arc<WorkflowRegistry>, String) {
    let registry = registry(vec!["plan"], vec!["researcher"]);
    let id = RunId::new(unique("status")).expect("valid id");
    let request = WorkflowStartRequest::new("plan()", unique("status-title")).expect("valid");
    registry.start(id.clone(), request, 100).await.expect("ran");
    (registry, id.to_string())
}

#[tokio::test]
async fn status_with_an_id_returns_the_runs_detail() {
    let (registry, id) = finished_run().await;
    let tool = girder_core::workflow_status_tool(Arc::clone(&registry));

    let result = call(&tool, json!({"id": id})).await;

    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains(&id), "{text}");
    assert!(text.contains("completed"), "{text}");
    assert!(text.contains("plan"), "phases must be shown: {text}");
    assert!(text.contains("researcher"), "agents must be shown: {text}");
    assert!(text.contains("answer"), "the result must be shown: {text}");
    assert!(text.contains("started at 100"), "{text}");
}

#[tokio::test]
async fn status_without_an_id_lists_every_run() {
    let registry = registry(vec![], vec![]);
    let a = RunId::new(unique("list-a")).expect("valid");
    let b = RunId::new(unique("list-b")).expect("valid");
    for id in [&a, &b] {
        registry
            .start(
                id.clone(),
                WorkflowStartRequest::new("s()", unique("listed")).expect("valid"),
                1,
            )
            .await
            .expect("ran");
    }
    let tool = girder_core::workflow_status_tool(Arc::clone(&registry));

    let result = call(&tool, json!({})).await;

    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains("2 run(s)"), "{text}");
    assert!(text.contains(a.as_str()), "{text}");
    assert!(text.contains(b.as_str()), "{text}");
    assert!(text.contains("completed"), "{text}");
}

#[tokio::test]
async fn status_of_an_empty_registry_says_so() {
    let registry = Arc::new(WorkflowRegistry::new());
    let tool = girder_core::workflow_status_tool(registry);

    let result = call(&tool, json!({})).await;

    assert!(!result.is_error, "an empty table is not a failure");
    assert_eq!(text_of(&result), "no workflow runs");
}

#[tokio::test]
async fn status_of_an_unknown_id_is_an_error_listing_the_known_ids() {
    let (registry, id) = finished_run().await;
    let tool = girder_core::workflow_status_tool(registry);

    let result = call(&tool, json!({"id": "nope"})).await;

    assert!(result.is_error);
    let text = text_of(&result);
    assert!(text.contains("no workflow run `nope`"), "{text}");
    // The real id is in the message because a mistyped id is the usual cause, and the fix is only visible with
    // the ids that exist.
    assert!(text.contains(&id), "{text}");
}

#[tokio::test]
async fn status_with_an_empty_id_asks_for_a_listing_instead() {
    let registry = Arc::new(WorkflowRegistry::new());
    let tool = girder_core::workflow_status_tool(registry);

    let result = call(&tool, json!({"id": ""})).await;

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("omit it to list"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn both_tools_register_under_their_names() {
    let registry = registry(vec![], vec![]);
    let tools = girder_core::ToolRegistry::new();

    girder_core::register_workflow_tools(&tools, registry);

    let names = tools.names();
    assert_eq!(names, vec!["workflow_run", "workflow_status"]);
    let schema = tools.get("workflow_run").expect("registered").schema();
    assert_eq!(schema.parameters["required"], json!(["script", "title"]));
}
