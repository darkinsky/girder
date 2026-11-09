//! End-to-end job tests (M4 of the v2 plan).
//!
//! Every other job test drives the registry directly. These spawn real processes, which is the only
//! way to check that output actually arrives while the command runs rather than all at once at the
//! end — the property that distinguishes a job from a tool call.

use std::sync::Arc;
use std::time::Duration;

use girder_core::{JobChannel, JobId, JobRegistry, JobStatus, SpawnOptions, spawn_job};

fn registry() -> Arc<JobRegistry> {
    Arc::new(JobRegistry::new(64 * 1024))
}

fn sh(script: &str) -> SpawnOptions {
    SpawnOptions::new("sh", vec!["-c".to_owned(), script.to_owned()])
}

/// Waits for a job to reach a terminal state, or panics after a bound.
async fn wait_settled(registry: &JobRegistry, id: &str) -> girder_core::Job {
    for _ in 0..200 {
        let job = registry.get(id).expect("registered");
        if job.is_settled() {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job `{id}` did not settle");
}

#[tokio::test]
async fn a_successful_command_settles_as_completed() {
    let registry = registry();
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "echo",
        sh("echo hello"),
    )
    .expect("spawned");

    let job = wait_settled(&registry, "j1").await;
    handle.await.expect("task finished");

    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.code, Some(0));
    assert!(
        job.output.text().contains("hello"),
        "{:?}",
        job.output.text()
    );
}

#[tokio::test]
async fn a_failing_command_settles_as_failed() {
    let registry = registry();
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "fail",
        sh("echo oops >&2; exit 3"),
    )
    .expect("spawned");

    let job = wait_settled(&registry, "j1").await;
    handle.await.expect("task finished");

    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.code, Some(3));
    assert!(job.output.text().contains("oops"));
}

#[tokio::test]
async fn stderr_is_recorded_on_its_own_channel() {
    let registry = registry();
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "mixed",
        sh("echo out; echo err >&2"),
    )
    .expect("spawned");

    let job = wait_settled(&registry, "j1").await;
    handle.await.expect("task finished");

    let channels: Vec<JobChannel> = job.output.chunks().iter().map(|c| c.channel).collect();
    assert!(channels.contains(&JobChannel::Stdout), "{channels:?}");
    assert!(channels.contains(&JobChannel::Stderr), "{channels:?}");
}

#[tokio::test]
async fn output_arrives_while_the_command_still_runs() {
    // The property that distinguishes a job from a tool call. A collecting implementation would show
    // nothing until the sleep finishes.
    let registry = registry();
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "slow",
        sh("echo early; sleep 1; echo late"),
    )
    .expect("spawned");

    // Before the command finishes, the early line must already be readable.
    let mut saw_early = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let job = registry.get("j1").expect("registered");
        if job.output.text().contains("early") && !job.is_settled() {
            saw_early = true;
            break;
        }
    }

    assert!(saw_early, "output must be readable while the command runs");
    let _ = wait_settled(&registry, "j1").await;
    handle.await.ok();
}

#[tokio::test]
async fn requesting_a_stop_settles_as_killed() {
    let registry = registry();
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "long",
        sh("sleep 30"),
    )
    .expect("spawned");

    // Let it start, then ask it to stop.
    tokio::time::sleep(Duration::from_millis(100)).await;
    registry.request_stop("j1").expect("stopping");

    let job = wait_settled(&registry, "j1").await;
    handle.await.ok();

    assert_eq!(job.status, JobStatus::Killed);
    assert!(job.output.text().is_empty() || job.output.text().len() < 100);
}

#[tokio::test]
async fn a_job_is_live_while_its_process_runs() {
    let registry = registry();
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "slow",
        sh("sleep 1"),
    )
    .expect("spawned");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(registry.live_of_kind("bash"), vec!["j1"]);

    wait_settled(&registry, "j1").await;
    handle.await.ok();
    assert!(registry.live().is_empty());
}

#[tokio::test]
async fn a_command_that_cannot_start_settles_as_failed() {
    // A spawn failure must settle rather than leave a caller waiting on a job that never existed.
    let registry = registry();
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "missing",
        SpawnOptions::new("girder-no-such-program-xyz", vec![]),
    )
    .expect("registered");

    let job = wait_settled(&registry, "j1").await;
    handle.await.expect("task finished");

    assert_eq!(job.status, JobStatus::Failed);
}

#[tokio::test]
async fn large_output_is_bounded_by_the_ring() {
    // A dev server logs forever; the ring keeps a tail rather than growing without limit.
    let registry = Arc::new(JobRegistry::new(1024));
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "chatty",
        sh("seq 1 20000"),
    )
    .expect("spawned");

    let job = wait_settled(&registry, "j1").await;
    handle.await.expect("task finished");

    let retained: usize = job.output.chunks().iter().map(|c| c.text.len()).sum();
    assert!(
        retained <= 4096,
        "the ring must stay bounded, kept {retained}"
    );
    assert!(job.output.head_offset() > 0, "the head must have moved");
}

#[tokio::test]
async fn a_duplicate_job_id_is_refused() {
    let registry = registry();
    let handle = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "one",
        sh("sleep 0.2"),
    )
    .expect("spawned");

    let error = spawn_job(
        Arc::clone(&registry),
        &JobId::new("j1").unwrap(),
        "bash",
        "two",
        sh("true"),
    )
    .expect_err("duplicate");

    assert!(matches!(
        error,
        girder_core::JobError::AlreadySettled { .. }
    ));
    wait_settled(&registry, "j1").await;
    handle.await.ok();
}
