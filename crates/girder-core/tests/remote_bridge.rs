//! End-to-end tests for the projection and job Remote methods.
//!
//! These are the bridge between a running system and a browser: a client reads folded session state
//! and manages background jobs without replaying the log or holding the registry itself. The tests
//! go through the same `RemoteCall` a socket would produce, so a change to the argument shape breaks
//! here rather than in a browser.

use std::sync::Arc;

use girder_core::{
    JobId, JobRegistry, ProjectionRegistry, register_job_methods, register_projection_methods,
};

/// A call with the given arguments, as the HTTP endpoint would build one.
fn call(args: serde_json::Value) -> girder_core::RemoteCall {
    girder_core::RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn registry() -> girder_core::RemoteRegistry {
    let mut registry = girder_core::RemoteRegistry::new();
    register_projection_methods(&mut registry, &Arc::new(ProjectionRegistry::new()));
    register_job_methods(&mut registry, &Arc::new(JobRegistry::new(4096)));
    registry
}

// ---- projections ------------------------------------------------------------------------------

#[tokio::test]
async fn the_projections_method_returns_a_map() {
    // A client renders a session list from this; an empty map is a valid answer for a fresh session.
    let registry = registry();
    let method = registry.get("session", "projections").expect("registered");

    let value = method(call(serde_json::json!({}))).await.expect("answered");

    assert!(value["projections"].is_object());
}

#[tokio::test]
async fn the_projections_method_takes_no_arguments() {
    // A method that demanded arguments the browser does not have would fail on every call.
    let registry = registry();
    let method = registry.get("session", "projections").expect("registered");

    method(call(serde_json::json!({}))).await.expect("answered");
}

// ---- jobs -------------------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_job_list_returns_an_empty_array() {
    // Not an error: a session with no jobs is the normal state.
    let registry = registry();
    let method = registry.get("job", "list").expect("registered");

    let value = method(call(serde_json::json!({}))).await.expect("answered");

    assert_eq!(value["jobs"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn the_job_list_reports_a_registered_job() {
    let jobs = Arc::new(JobRegistry::new(4096));
    jobs.register(&JobId::new("j1").expect("valid"), "bash", "dev server")
        .expect("registered");
    let mut registry = girder_core::RemoteRegistry::new();
    register_job_methods(&mut registry, &jobs);

    let value = registry.get("job", "list").expect("registered")(call(serde_json::json!({})))
        .await
        .expect("answered");

    let views = value["jobs"].as_array().expect("an array");
    assert_eq!(views.len(), 1);
    assert_eq!(views[0]["id"], "j1");
    assert_eq!(views[0]["status"], "running");
    assert!(views[0]["code"].is_null(), "a running job has no code");
}

#[tokio::test]
async fn the_job_list_carries_the_whole_set() {
    // A reconnecting client's first frame must already be the truth, so the frame is never a delta.
    let jobs = Arc::new(JobRegistry::new(4096));
    for id in ["a", "b"] {
        jobs.register(&JobId::new(id).expect("valid"), "bash", id)
            .expect("registered");
    }
    let mut registry = girder_core::RemoteRegistry::new();
    register_job_methods(&mut registry, &jobs);

    let value = registry.get("job", "list").expect("registered")(call(serde_json::json!({})))
        .await
        .expect("answered");

    assert_eq!(value["jobs"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn killing_a_live_job_reports_requested() {
    let jobs = Arc::new(JobRegistry::new(4096));
    jobs.register(&JobId::new("j1").expect("valid"), "bash", "x")
        .expect("registered");
    let mut registry = girder_core::RemoteRegistry::new();
    register_job_methods(&mut registry, &jobs);

    let value =
        registry.get("job", "kill").expect("registered")(call(serde_json::json!({"jobId": "j1"})))
            .await
            .expect("answered");

    assert_eq!(value["outcome"], "requested");
    assert_eq!(
        jobs.get("j1").expect("present").status,
        girder_core::JobStatus::Stopping,
        "the registry must actually be asked to stop"
    );
}

#[tokio::test]
async fn killing_a_settled_job_reports_already_finished() {
    // Not an error: a client that showed one would alarm a user over a harmless race.
    let jobs = Arc::new(JobRegistry::new(4096));
    jobs.register(&JobId::new("j1").expect("valid"), "bash", "x")
        .expect("registered");
    jobs.complete("j1", 0, 5000).expect("settled");
    let mut registry = girder_core::RemoteRegistry::new();
    register_job_methods(&mut registry, &jobs);

    let value =
        registry.get("job", "kill").expect("registered")(call(serde_json::json!({"jobId": "j1"})))
            .await
            .expect("answered");

    assert_eq!(value["outcome"], "already-finished");
}

#[tokio::test]
async fn killing_an_unknown_job_is_an_argument_error() {
    // A client sent an id this registry does not hold, which is the same class of mistake as a
    // missing field.
    let registry = registry();
    let error = registry.get("job", "kill").expect("registered")(call(
        serde_json::json!({"jobId": "nope"}),
    ))
    .await
    .expect_err("unknown job");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
}

#[tokio::test]
async fn a_missing_job_id_is_an_argument_error() {
    let registry = registry();
    let error = registry.get("job", "kill").expect("registered")(call(serde_json::json!({})))
        .await
        .expect_err("missing jobId");

    let girder_core::RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("jobId"));
}

// ---- registration -----------------------------------------------------------------------------

#[test]
fn the_bridge_registers_three_methods() {
    let registry = registry();
    let names = registry.names();

    assert!(names.contains(&("session".to_owned(), "projections".to_owned())));
    assert!(names.contains(&("job".to_owned(), "list".to_owned())));
    assert!(names.contains(&("job".to_owned(), "kill".to_owned())));
}
