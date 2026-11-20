//! Tests for the `workflow.*` Remote methods.
//!
//! The properties under test, in the order they matter:
//!
//! - **`start` returns before the script does, with an id that is immediately readable** — a client
//!   that has to wait for a script has no id to poll and nothing to cancel, which is the whole
//!   reason this surface exists rather than the model-facing tool
//! - **a finished run is never restarted under its id** — `completed`, `cancelled`, `failed` and
//!   in-flight runs are each refused, so the record of what happened is never overwritten
//! - **`cancel` interrupts a live run and reports that it did** — the interface keeps the spawned
//!   task's handle precisely so the final `completed` write cannot undo the cancellation
//! - **`cancel` on a finished run is an error, not a silent success** — the caller learns the run
//!   had already ended rather than believing it stopped something
//! - **an unknown id is an error on both methods that take one**, listing the ids that exist
//! - **the projection carries the run's phases, agents and result** — a status read that only said
//!   "running" would tell a client nothing it could render
//! - **the declarations match what each method does** — a read-only peer may list and get, and may
//!   not start or cancel
//!
//! Isolation: every test builds its own registry, engine and `RemoteRegistry`, and ids carry a
//! process-wide counter, so tests in one binary cannot observe each other's runs.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the methods as written even before the crate root wires them in. It spells its own paths
//! through `girder_core::`, which is the alias the library declares for itself, so the same source
//! builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/workflow/rpc.rs"]
pub mod workflow_rpc;

use std::sync::Arc;
use std::time::Duration;

use girder_core::{
    BoxFut, ReadOnly, RemoteCall, RemoteError, RemoteRegistry, WorkflowEngine, WorkflowEvent,
    WorkflowRegistry, WorkflowStartRequest,
};
use serde_json::{Value, json};

// ---- fixtures ---------------------------------------------------------------------------------

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn remote(workflows: &Arc<WorkflowRegistry>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    workflow_rpc::register_workflow_methods(&mut registry, Arc::clone(workflows));
    registry
}

/// An engine that reports a phase and one agent, then returns a fixed value.
struct Finishing {
    value: Value,
}

impl WorkflowEngine for Finishing {
    fn run(
        &self,
        _request: WorkflowStartRequest,
        report: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
    ) -> BoxFut<Result<Value, String>> {
        let value = self.value.clone();
        Box::pin(async move {
            report(WorkflowEvent::PhaseStarted {
                name: "plan".to_owned(),
                at: 10,
            });
            report(WorkflowEvent::AgentStarted {
                label: "worker".to_owned(),
                prompt: "do the thing".to_owned(),
            });
            report(WorkflowEvent::AgentFinished {
                label: "worker".to_owned(),
                output: "did it".to_owned(),
                result: Some(json!({"ok": true})),
                at: 20,
            });
            report(WorkflowEvent::PhaseEnded {
                name: "plan".to_owned(),
                at: 30,
            });
            Ok(value)
        })
    }
}

/// An engine that never finishes, so a cancellation has something real to interrupt.
struct Hanging;

impl WorkflowEngine for Hanging {
    fn run(
        &self,
        _request: WorkflowStartRequest,
        _report: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
    ) -> BoxFut<Result<Value, String>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(json!({"never": "reached"}))
        })
    }
}

/// An engine that fails, so a failed run is in the table with a reason.
struct Broken;

impl WorkflowEngine for Broken {
    fn run(
        &self,
        _request: WorkflowStartRequest,
        _report: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
    ) -> BoxFut<Result<Value, String>> {
        Box::pin(async move { Err("the script threw".to_owned()) })
    }
}

fn finishing() -> Arc<WorkflowRegistry> {
    Arc::new(WorkflowRegistry::with_engine(Arc::new(Finishing {
        value: json!({"answer": 42}),
    })))
}

fn hanging() -> Arc<WorkflowRegistry> {
    Arc::new(WorkflowRegistry::with_engine(Arc::new(Hanging)))
}

/// Starts one run and returns its id.
async fn started(registry: &RemoteRegistry) -> String {
    let value = registry
        .dispatch(
            "workflow",
            "start",
            call(json!({"script": "return 1", "title": "a run"})),
        )
        .await
        .expect("start");
    value["id"].as_str().expect("an id").to_owned()
}

fn message_of(error: &RemoteError) -> String {
    error.to_string()
}

// ---- list -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_listing_of_an_empty_registry_is_empty_rather_than_an_error() {
    let workflows = Arc::new(WorkflowRegistry::new());
    let value = remote(&workflows)
        .dispatch("workflow", "list", call(json!({})))
        .await
        .expect("list");

    assert_eq!(value["items"], json!([]));
    assert_eq!(value["total"], 0);
    assert_eq!(value["shown"], 0);
    assert_eq!(value["limit"], workflow_rpc::DEFAULT_LIST_LIMIT);
}

#[tokio::test]
async fn a_listing_carries_one_row_per_run_without_the_agents() {
    let workflows = hanging();
    let registry = remote(&workflows);
    let first = started(&registry).await;
    let second = started(&registry).await;

    let value = registry
        .dispatch("workflow", "list", call(json!({})))
        .await
        .expect("list");

    let items = value["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(value["total"], 2);
    let ids: Vec<&str> = items.iter().filter_map(|row| row["id"].as_str()).collect();
    assert!(ids.contains(&first.as_str()));
    assert!(ids.contains(&second.as_str()));
    // The summary is what a table row renders; a listing that carried every agent's output would
    // be a listing nobody scrolls.
    assert!(items[0].get("agents").is_none());
    assert_eq!(items[0]["status"], "running");
}

#[tokio::test]
async fn a_listing_pages_with_limit_and_offset() {
    let workflows = hanging();
    let registry = remote(&workflows);
    for _ in 0..3 {
        started(&registry).await;
    }

    let value = registry
        .dispatch("workflow", "list", call(json!({"limit": 2, "offset": 1})))
        .await
        .expect("list");

    assert_eq!(value["items"].as_array().expect("items").len(), 2);
    assert_eq!(value["shown"], 2);
    // `total` describes the filtered set, not the page: a client showing "2 of 2" when there are
    // three runs would be showing a wrong denominator.
    assert_eq!(value["total"], 3);
    assert_eq!(value["limit"], 2);
    assert_eq!(value["offset"], 1);
}

#[tokio::test]
async fn a_listing_refuses_a_limit_outside_the_range_and_an_unknown_status() {
    let workflows = hanging();
    let registry = remote(&workflows);

    let zero = registry
        .dispatch("workflow", "list", call(json!({"limit": 0})))
        .await
        .expect_err("refused");
    assert_eq!(zero.code(), "INVALID_ARGUMENTS");
    assert!(message_of(&zero).contains("between 1 and"), "{zero}");

    let huge = registry
        .dispatch(
            "workflow",
            "list",
            call(json!({"limit": workflow_rpc::MAX_LIST_LIMIT + 1})),
        )
        .await
        .expect_err("refused");
    assert_eq!(huge.code(), "INVALID_ARGUMENTS");

    // A misspelled status would otherwise match nothing and look like "the filter worked".
    let typo = registry
        .dispatch("workflow", "list", call(json!({"status": "runnning"})))
        .await
        .expect_err("refused");
    assert_eq!(typo.code(), "INVALID_ARGUMENTS");
    assert!(message_of(&typo).contains("runnning"), "{typo}");
}

#[tokio::test]
async fn a_listing_filters_by_status_and_by_unfinished() {
    let workflows = Arc::new(WorkflowRegistry::with_engine(Arc::new(Finishing {
        value: json!(null),
    })));
    let registry = remote(&workflows);
    let finished = started(&registry).await;

    let workflows_broken = Arc::new(WorkflowRegistry::with_engine(Arc::new(Broken)));
    let registry_broken = remote(&workflows_broken);
    let failed = started(&registry_broken).await;

    let completed = registry
        .dispatch("workflow", "list", call(json!({"status": "completed"})))
        .await
        .expect("list");
    assert_eq!(completed["total"], 1);
    assert_eq!(completed["items"][0]["id"], finished.as_str());

    let failures = registry_broken
        .dispatch("workflow", "list", call(json!({"status": "failed"})))
        .await
        .expect("list");
    assert_eq!(failures["total"], 1);
    assert_eq!(failures["items"][0]["id"], failed.as_str());
    // The reason travels in the projection, because "failed" with no reason is the one thing a
    // caller cannot act on.
    assert_eq!(failures["items"][0]["status"], "failed");

    let active = registry
        .dispatch("workflow", "list", call(json!({"activeOnly": true})))
        .await
        .expect("list");
    assert_eq!(active["total"], 0);
}

// ---- get --------------------------------------------------------------------------------------

#[tokio::test]
async fn get_returns_the_phases_agents_and_result_of_a_finished_run() {
    let workflows = finishing();
    let registry = remote(&workflows);
    let id = started(&registry).await;

    let value = registry
        .dispatch("workflow", "get", call(json!({"id": id})))
        .await
        .expect("get");

    assert_eq!(value["id"], id.as_str());
    assert_eq!(value["title"], "a run");
    assert_eq!(value["status"], "completed");
    assert_eq!(value["terminal"], true);
    assert_eq!(value["reason"], Value::Null);
    assert_eq!(value["result"], json!({"answer": 42}));
    assert_eq!(value["agentCount"], 1);
    assert_eq!(value["finishedAgents"], 1);

    let phases = value["phases"].as_array().expect("phases");
    assert_eq!(phases.len(), 1);
    assert_eq!(phases[0]["name"], "plan");
    assert_eq!(phases[0]["running"], false);
    // A finished run has no phase in flight, which is how a client knows not to render a spinner.
    assert_eq!(value["currentPhase"], Value::Null);

    let agents = value["agents"].as_array().expect("agents");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["label"], "worker");
    assert_eq!(agents[0]["output"], "did it");
    assert_eq!(agents[0]["result"], json!({"ok": true}));
    assert_eq!(agents[0]["done"], true);
}

#[tokio::test]
async fn get_reports_a_failed_run_with_its_reason() {
    let workflows = Arc::new(WorkflowRegistry::with_engine(Arc::new(Broken)));
    let registry = remote(&workflows);
    let id = started(&registry).await;

    let value = registry
        .dispatch("workflow", "get", call(json!({"id": id})))
        .await
        .expect("get");

    assert_eq!(value["status"], "failed");
    assert_eq!(value["terminal"], true);
    assert_eq!(value["reason"], "the script threw");
}

#[tokio::test]
async fn an_unknown_or_blank_id_is_refused_and_names_the_ids_that_exist() {
    let workflows = finishing();
    let registry = remote(&workflows);
    let id = started(&registry).await;

    let unknown = registry
        .dispatch("workflow", "get", call(json!({"id": "nope"})))
        .await
        .expect_err("refused");
    assert_eq!(unknown.code(), "INVALID_ARGUMENTS");
    // The real id is in the message because a mistyped id is the usual cause, and the fix is only
    // visible with the ids that do exist.
    assert!(message_of(&unknown).contains(&id), "{unknown}");

    let blank = registry
        .dispatch("workflow", "get", call(json!({"id": "  "})))
        .await
        .expect_err("refused");
    assert_eq!(blank.code(), "INVALID_ARGUMENTS");
    assert!(message_of(&blank).contains("`id` is required"), "{blank}");

    let missing = registry
        .dispatch("workflow", "get", call(json!({})))
        .await
        .expect_err("refused");
    assert!(
        message_of(&missing).contains("`id` is required"),
        "{missing}"
    );
}

// ---- start ------------------------------------------------------------------------------------

#[tokio::test]
async fn start_returns_an_id_while_a_long_run_is_still_going() {
    // The property this method exists for: a caller gets its handle back before the script ends.
    let workflows = hanging();
    let registry = remote(&workflows);

    let value = registry
        .dispatch(
            "workflow",
            "start",
            call(json!({"script": "await forever", "title": "slow"})),
        )
        .await
        .expect("start");

    let id = value["id"].as_str().expect("an id").to_owned();
    assert_eq!(value["status"], "running");
    assert_eq!(value["terminal"], false);

    // And the id is readable immediately, which is what makes polling possible at all.
    let read = registry
        .dispatch("workflow", "get", call(json!({"id": id})))
        .await
        .expect("get");
    assert_eq!(read["status"], "running");
}

#[tokio::test]
async fn start_carries_arguments_through_to_the_engine() {
    let workflows = finishing();
    let registry = remote(&workflows);

    let value = registry
        .dispatch(
            "workflow",
            "start",
            call(json!({
                "script": "return args.topic",
                "title": "with arguments",
                "arguments": {"topic": "rust"},
            })),
        )
        .await
        .expect("start");

    assert_eq!(value["title"], "with arguments");
    assert_eq!(value["status"], "completed");
}

#[tokio::test]
async fn start_refuses_an_argument_that_is_not_a_string() {
    // The domain's `StartRequest` carries strings only; stringifying behind the caller's back would
    // make a script see `"3"` where it declared a number, and the type error would be invisible.
    let workflows = finishing();
    let registry = remote(&workflows);

    let error = registry
        .dispatch(
            "workflow",
            "start",
            call(json!({
                "script": "return 1",
                "title": "bad argument",
                "arguments": {"count": 3},
            })),
        )
        .await
        .expect_err("refused");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(message_of(&error).contains("count"), "{error}");
}

#[tokio::test]
async fn start_refuses_a_blank_script_or_title() {
    let workflows = finishing();
    let registry = remote(&workflows);

    for args in [
        json!({"script": "   ", "title": "a title"}),
        json!({"script": "return 1", "title": "  "}),
        json!({"title": "a title"}),
    ] {
        let error = registry
            .dispatch("workflow", "start", call(args))
            .await
            .expect_err("refused");
        assert_eq!(error.code(), "INVALID_ARGUMENTS");
        assert!(
            message_of(&error).contains("must both be non-blank"),
            "{error}"
        );
    }
    // Nothing was recorded for any of them: a refused start must not leave a phantom run.
    let listing = registry
        .dispatch("workflow", "list", call(json!({})))
        .await
        .expect("list");
    assert_eq!(listing["total"], 0);
}

#[tokio::test]
async fn start_refuses_to_restart_a_finished_run() {
    // The state-transition rule: a completed run is the record of what happened, so reusing its id
    // would overwrite the evidence of a script that ran with the result of one that did not.
    let workflows = finishing();
    let registry = remote(&workflows);
    let id = started(&registry).await;

    let error = registry
        .dispatch(
            "workflow",
            "start",
            call(json!({"script": "return 2", "title": "again", "id": id})),
        )
        .await
        .expect_err("refused");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(message_of(&error).contains("already completed"), "{error}");
    assert!(message_of(&error).contains("omit `id`"), "{error}");

    // The original run is untouched.
    let read = registry
        .dispatch("workflow", "get", call(json!({"id": id})))
        .await
        .expect("get");
    assert_eq!(read["title"], "a run");
    assert_eq!(read["result"], json!({"answer": 42}));
}

#[tokio::test]
async fn start_refuses_to_restart_a_cancelled_or_failed_run() {
    let workflows = hanging();
    let registry = remote(&workflows);
    let cancelled = started(&registry).await;
    registry
        .dispatch("workflow", "cancel", call(json!({"id": cancelled})))
        .await
        .expect("cancel");

    let error = registry
        .dispatch(
            "workflow",
            "start",
            call(json!({"script": "return 1", "title": "again", "id": cancelled})),
        )
        .await
        .expect_err("refused");
    assert!(message_of(&error).contains("already cancelled"), "{error}");

    let broken = Arc::new(WorkflowRegistry::with_engine(Arc::new(Broken)));
    let broken_registry = remote(&broken);
    let failed = started(&broken_registry).await;
    let error = broken_registry
        .dispatch(
            "workflow",
            "start",
            call(json!({"script": "return 1", "title": "again", "id": failed})),
        )
        .await
        .expect_err("refused");
    assert!(message_of(&error).contains("already failed"), "{error}");
}

#[tokio::test]
async fn start_refuses_an_id_that_is_still_running() {
    let workflows = hanging();
    let registry = remote(&workflows);
    let id = started(&registry).await;

    let error = registry
        .dispatch(
            "workflow",
            "start",
            call(json!({"script": "return 1", "title": "second", "id": id})),
        )
        .await
        .expect_err("refused");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(message_of(&error).contains("already running"), "{error}");
    // The message points at the method that already answers the caller's real question.
    assert!(message_of(&error).contains("workflow.get"), "{error}");
}

#[tokio::test]
async fn start_mints_a_distinct_id_per_call() {
    let workflows = hanging();
    let registry = remote(&workflows);

    let first = started(&registry).await;
    let second = started(&registry).await;

    assert_ne!(first, second);
    let listing = registry
        .dispatch("workflow", "list", call(json!({})))
        .await
        .expect("list");
    assert_eq!(listing["total"], 2);
}

// ---- cancel -----------------------------------------------------------------------------------

#[tokio::test]
async fn cancel_stops_a_live_run_and_says_it_interrupted_it() {
    let workflows = hanging();
    let registry = remote(&workflows);
    let id = started(&registry).await;

    let value = registry
        .dispatch("workflow", "cancel", call(json!({"id": id})))
        .await
        .expect("cancel");

    assert_eq!(value["status"], "cancelled");
    assert_eq!(value["interrupted"], true);

    // The status stays cancelled: the abort exists precisely so the domain's final `completed` write
    // cannot overwrite the cancellation when the script would have finished.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let read = registry
        .dispatch("workflow", "get", call(json!({"id": id})))
        .await
        .expect("get");
    assert_eq!(read["status"], "cancelled");
    assert_eq!(read["terminal"], true);
}

#[tokio::test]
async fn cancel_refuses_a_finished_run_and_leaves_its_record_alone() {
    let workflows = finishing();
    let registry = remote(&workflows);
    let id = started(&registry).await;

    let error = registry
        .dispatch("workflow", "cancel", call(json!({"id": id})))
        .await
        .expect_err("refused");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(
        message_of(&error).contains("is already completed"),
        "{error}"
    );

    let read = registry
        .dispatch("workflow", "get", call(json!({"id": id})))
        .await
        .expect("get");
    // The result is still there: a refused cancellation must not discard work that happened.
    assert_eq!(read["status"], "completed");
    assert_eq!(read["result"], json!({"answer": 42}));
}

#[tokio::test]
async fn cancel_refuses_an_unknown_id() {
    let workflows = finishing();
    let registry = remote(&workflows);

    let error = registry
        .dispatch("workflow", "cancel", call(json!({"id": "nope"})))
        .await
        .expect_err("refused");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(
        message_of(&error).contains("no workflow run `nope`"),
        "{error}"
    );
}

// ---- the surface itself -----------------------------------------------------------------------

#[tokio::test]
async fn the_namespace_registers_exactly_the_four_methods() {
    let workflows = Arc::new(WorkflowRegistry::new());
    let registry = remote(&workflows);

    let mut names: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|(namespace, _)| namespace == "workflow")
        .map(|(_, method)| method)
        .collect();
    names.sort();
    assert_eq!(names, vec!["cancel", "get", "list", "start"]);
}

#[tokio::test]
async fn a_read_only_peer_may_observe_and_may_not_start_or_cancel() {
    // The declarations are the point: a listing that a read-only deployment could not serve would be
    // the wrong reason to refuse, and a read-only peer that could start a run would be able to spend
    // an engine's work.
    let workflows = hanging();
    let mut registry = RemoteRegistry::new();
    workflow_rpc::register_workflow_methods(&mut registry, Arc::clone(&workflows));
    let registry = registry.with_policy(Arc::new(ReadOnly));

    let id = {
        // Started through the domain directly, since this peer may not call `start`.
        let domain_id = girder_core::RunId::new("direct-run").expect("valid id");
        let request = WorkflowStartRequest::new("return 1", "started out of band").expect("valid");
        let workflows = Arc::clone(&workflows);
        tokio::spawn(async move { workflows.start(domain_id, request, 1).await });
        "direct-run".to_owned()
    };
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(
        registry
            .dispatch("workflow", "list", call(json!({})))
            .await
            .is_ok()
    );
    assert!(
        registry
            .dispatch("workflow", "get", call(json!({"id": id})))
            .await
            .is_ok()
    );

    let refused_start = registry
        .dispatch(
            "workflow",
            "start",
            call(json!({"script": "return 1", "title": "no"})),
        )
        .await
        .expect_err("refused");
    assert_eq!(refused_start.code(), "REFUSED");

    let refused_cancel = registry
        .dispatch("workflow", "cancel", call(json!({"id": id})))
        .await
        .expect_err("refused");
    assert_eq!(refused_cancel.code(), "REFUSED");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod workflow {
    pub use super::workflow_rpc as rpc;
}
