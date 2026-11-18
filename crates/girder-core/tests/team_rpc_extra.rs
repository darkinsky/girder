//! `team.checkTaskGraph`: the dependency validator, reachable from a client.
//!
//! The property under test is the one that makes a plan executable: a task graph with a cycle is
//! accepted by every individual edit and then never starts, so the refusal has to be available
//! where the edit is made.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use girder_core::team_rpc_extra::{MAX_TASKS, METHOD, NAMESPACE};
use girder_core::{RemoteCall, RemoteError, RemoteRegistry};
use serde_json::{Value, json};

/// A registry with the validator registered.
fn registry() -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    girder_core::register_team_graph_method(&mut registry);
    registry
}

/// Invokes `team.checkTaskGraph`.
fn check(tasks: &Value) -> Result<Value, RemoteError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let registry = registry();
    runtime.block_on(registry.dispatch(
        NAMESPACE,
        METHOD,
        RemoteCall {
            args: json!({ "tasks": tasks }),
            peer: "test".to_owned(),
        },
    ))
}

/// One task with the given id and blockers.
fn task(id: &str, blocked_by: &[&str]) -> Value {
    json!({
        "id": id,
        "status": "pending",
        "subject": id,
        "blockedBy": blocked_by,
    })
}

#[test]
fn a_linear_chain_is_accepted() {
    let answer = check(&json!([
        task("a", &[]),
        task("b", &["a"]),
        task("c", &["b"])
    ]))
    .expect("dispatched");
    assert_eq!(answer["ok"], json!(true));
}

#[test]
fn a_cycle_is_refused_by_code_rather_than_by_prose() {
    // The whole reason this method exists: `a -> b -> a` is valid edge by edge and unstartable.
    let answer = check(&json!([task("a", &["b"]), task("b", &["a"]),])).expect("dispatched");
    assert_eq!(answer["ok"], json!(false), "{answer}");
    assert!(
        answer["code"].as_str().is_some_and(|code| !code.is_empty()),
        "a refusal must carry a code a client can branch on: {answer}"
    );
}

#[test]
fn a_self_reference_is_refused() {
    let answer = check(&json!([task("a", &["a"])])).expect("dispatched");
    assert_eq!(answer["ok"], json!(false), "{answer}");
}

#[test]
fn a_missing_blocker_is_refused() {
    // A dependency on a task the graph does not contain can never be satisfied.
    let answer = check(&json!([task("a", &["ghost"])])).expect("dispatched");
    assert_eq!(answer["ok"], json!(false), "{answer}");
}

#[test]
fn a_diamond_is_accepted() {
    // Two paths to the same blocker is a DAG, not a cycle.
    let answer = check(&json!([
        task("root", &[]),
        task("left", &["root"]),
        task("right", &["root"]),
        task("join", &["left", "right"]),
    ]))
    .expect("dispatched");
    assert_eq!(answer["ok"], json!(true), "{answer}");
}

#[test]
fn an_empty_graph_is_accepted() {
    let answer = check(&json!([])).expect("dispatched");
    assert_eq!(answer["ok"], json!(true));
}

#[test]
fn a_graph_past_the_cap_is_refused_before_it_is_walked() {
    let tasks: Vec<Value> = (0..=MAX_TASKS)
        .map(|index| task(&format!("t{index}"), &[]))
        .collect();
    let error = check(&json!(tasks)).expect_err("past the cap");
    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error:?}"
    );
}

#[test]
fn a_task_without_an_id_is_refused() {
    // `id` is the one field the graph is actually about -- a task without one cannot participate in
    // a dependency, so it is refused rather than defaulted. Cosmetic fields such as `subject` are
    // defaulted instead, because a graph that is structurally sound is still a valid graph.
    let error = check(&json!([{ "status": "pending" }])).expect_err("no id");
    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error:?}"
    );
}

#[test]
fn a_task_without_a_subject_is_still_accepted() {
    let answer = check(&json!([{ "id": "a", "status": "pending" }])).expect("dispatched");
    assert_eq!(answer["ok"], json!(true), "{answer}");
}

#[test]
fn the_method_dispatches_rather_than_reporting_a_gap() {
    let answer = check(&json!([]));
    assert!(
        !matches!(
            answer,
            Err(RemoteError::NotImplemented { .. } | RemoteError::UnknownMethod { .. })
        ),
        "team.checkTaskGraph must dispatch: {answer:?}"
    );
}
