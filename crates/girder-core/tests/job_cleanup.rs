//! Tests for forgetting settled jobs.
//!
//! Nothing removed them before, so a long-lived server accumulated a record per job it ever ran, each holding its
//! output ring. The properties that matter: a running job is never forgotten, and a forgotten one is the oldest.

use std::sync::Arc;

use girder_core::{JobId, JobRegistry};

fn registry() -> Arc<JobRegistry> {
    Arc::new(JobRegistry::new(4 * 1024))
}

/// Registers a job and settles it at `settled_at`.
fn settled(registry: &JobRegistry, id: &str, settled_at: u64) {
    let job = JobId::new(id).expect("valid");
    registry.register(&job, "test", id).expect("registered");
    registry.complete(id, 0, settled_at).expect("completed");
}

#[test]
fn a_running_job_is_never_forgotten() {
    // Removing a job a client is watching would make it disappear while it is still producing output.
    let registry = registry();
    let job = JobId::new("running").expect("valid");
    registry
        .register(&job, "test", "running")
        .expect("registered");

    let removed = registry.retain_recent(1_000_000, 1000);

    assert_eq!(removed, 0);
    assert_eq!(registry.ids().len(), 1);
}

#[test]
fn a_settled_job_older_than_the_horizon_is_forgotten() {
    let registry = registry();
    settled(&registry, "old", 1_000);

    let removed = registry.retain_recent(100_000, 1_000);

    assert_eq!(removed, 1);
    assert!(registry.ids().is_empty());
}

#[test]
fn a_recent_settled_job_is_kept() {
    // A client may still be reading the output of a job that finished a moment ago.
    let registry = registry();
    settled(&registry, "recent", 99_500);

    let removed = registry.retain_recent(100_000, 1_000);

    assert_eq!(removed, 0);
    assert_eq!(registry.ids().len(), 1);
}

#[test]
fn the_horizon_is_inclusive() {
    // Exactly at the horizon is recent, not old: an off-by-one here would drop the job a client just finished reading.
    let registry = registry();
    settled(&registry, "edge", 99_000);

    assert_eq!(registry.retain_recent(100_000, 1_000), 0);
    assert_eq!(registry.retain_recent(100_001, 1_000), 1);
}

#[test]
fn forgetting_settled_jobs_keeps_the_newest() {
    let registry = registry();
    for (index, at) in [(1u64, 100u64), (2, 200), (3, 300), (4, 400)] {
        settled(&registry, &format!("job-{index}"), at);
    }

    let removed = registry.forget_settled(2);

    assert_eq!(removed, 2);
    let left = registry.ids();
    assert_eq!(left.len(), 2);
    assert!(left.contains(&"job-3".to_owned()), "{left:?}");
    assert!(left.contains(&"job-4".to_owned()), "{left:?}");
}

#[test]
fn forgetting_never_touches_a_running_job() {
    let registry = registry();
    let job = JobId::new("running").expect("valid");
    registry
        .register(&job, "test", "running")
        .expect("registered");
    settled(&registry, "done", 100);

    let removed = registry.forget_settled(0);

    // The count is of settled jobs removed, so the running one is still there and the total is one.
    assert_eq!(removed, 1);
    assert_eq!(registry.ids(), vec!["running".to_owned()]);
}

#[test]
fn keeping_more_than_there_are_removes_nothing() {
    let registry = registry();
    settled(&registry, "one", 100);

    assert_eq!(registry.forget_settled(5), 0);
    assert_eq!(registry.ids().len(), 1);
}

#[test]
fn a_forgotten_job_is_really_gone() {
    // Returning a count while leaving the record would make the API report work it did not do.
    let registry = registry();
    settled(&registry, "old", 100);
    assert!(registry.get("old").is_ok());

    registry.forget_settled(0);

    assert!(registry.get("old").is_err());
}
