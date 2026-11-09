//! Tests for background jobs (M4 of the v2 plan).
//!
//! The output ring is the part with a subtle contract: offsets never move after a trim, and a reader
//! whose position fell off the head is told so rather than silently reading across the hole.

use girder_core::{JobChannel, JobError, JobId, JobRegistry, JobStatus, OutputRing};

fn id(value: &str) -> JobId {
    JobId::new(value).expect("valid")
}

// ---- the lifecycle ----------------------------------------------------------------------------

#[test]
fn a_registered_job_is_running() {
    let registry = JobRegistry::new(1024);
    let job = registry
        .register(&id("j1"), "bash", "dev server")
        .expect("registered");

    assert_eq!(job.status, JobStatus::Running);
    assert!(job.status.is_live());
    assert!(!job.is_settled());
}

#[test]
fn a_running_job_can_be_asked_to_stop() {
    // `stopping` rather than `killed`: a kill is not instantaneous, and a caller that saw `killed`
    // while children were alive would start a second job against resources the first still holds.
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "dev").unwrap();

    let job = registry.request_stop("j1").expect("stopping");

    assert_eq!(job.status, JobStatus::Stopping);
    assert!(job.status.is_live(), "stopping still occupies a slot");
    assert!(!job.is_settled());
}

#[test]
fn stopping_twice_is_idempotent() {
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "dev").unwrap();

    registry.request_stop("j1").unwrap();
    let again = registry.request_stop("j1").expect("still stopping");

    assert_eq!(again.status, JobStatus::Stopping);
}

#[test]
fn a_stopping_job_settles_as_killed() {
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "dev").unwrap();
    registry.request_stop("j1").unwrap();

    let job = registry.confirm_killed("j1", 5000).expect("killed");

    assert_eq!(job.status, JobStatus::Killed);
    assert_eq!(job.settled_at, Some(5000));
    assert!(job.is_settled());
}

#[test]
fn a_job_that_exits_zero_completes() {
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "build").unwrap();

    let job = registry.complete("j1", 0, 5000).expect("completed");

    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.code, Some(0));
}

#[test]
fn a_job_that_exits_nonzero_fails() {
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "build").unwrap();

    let job = registry.complete("j1", 2, 5000).expect("failed");

    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.code, Some(2));
}

#[test]
fn a_terminal_state_is_final() {
    // A second settle would rewrite history a reader already saw.
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "build").unwrap();
    registry.complete("j1", 0, 5000).unwrap();

    let error = registry
        .complete("j1", 1, 6000)
        .expect_err("already settled");

    let JobError::AlreadySettled { status, .. } = error else {
        panic!("expected AlreadySettled, got {error:?}");
    };
    assert_eq!(status, "completed");
}

#[test]
fn appending_to_a_settled_job_is_refused() {
    // Output nothing will ever settle against.
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "build").unwrap();
    registry.complete("j1", 0, 5000).unwrap();

    let error = registry
        .append("j1", "late output", JobChannel::Stdout)
        .expect_err("settled");

    assert!(matches!(
        error,
        JobError::WrongState {
            operation: "append",
            ..
        }
    ));
}

#[test]
fn an_unknown_job_is_named_in_the_error() {
    let registry = JobRegistry::new(1024);
    let error = registry.get("nope").expect_err("missing");

    let JobError::NotFound { id } = error else {
        panic!("expected NotFound");
    };
    assert_eq!(id, "nope");
}

#[test]
fn a_duplicate_id_is_refused() {
    // Reusing an id would make a reader's saved position point into a different job's output.
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "one").unwrap();

    let error = registry
        .register(&id("j1"), "bash", "two")
        .expect_err("taken");

    assert!(matches!(error, JobError::AlreadySettled { .. }));
}

// ---- live tracking ----------------------------------------------------------------------------

#[test]
fn only_live_jobs_are_listed_as_live() {
    let registry = JobRegistry::new(1024);
    registry.register(&id("a"), "bash", "one").unwrap();
    registry.register(&id("b"), "bash", "two").unwrap();
    registry.complete("b", 0, 5000).unwrap();

    assert_eq!(registry.live(), vec!["a"]);
}

#[test]
fn a_stopping_job_still_counts_as_live() {
    // The check a caller runs before starting a second job against the same port.
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "dev").unwrap();
    registry.request_stop("j1").unwrap();

    assert_eq!(registry.live_of_kind("bash"), vec!["j1"]);
}

#[test]
fn live_jobs_can_be_filtered_by_kind() {
    let registry = JobRegistry::new(1024);
    registry.register(&id("a"), "bash", "shell").unwrap();
    registry.register(&id("b"), "workflow", "flow").unwrap();

    assert_eq!(registry.live_of_kind("bash"), vec!["a"]);
    assert_eq!(registry.live_of_kind("workflow"), vec!["b"]);
    assert!(registry.live_of_kind("other").is_empty());
}

// ---- the output ring --------------------------------------------------------------------------

#[test]
fn appended_output_keeps_its_offset() {
    let mut ring = OutputRing::new(1024);
    ring.append("hello", JobChannel::Stdout);
    ring.append(" world", JobChannel::Stdout);

    let chunks: Vec<_> = ring.chunks().iter().collect();
    assert_eq!(chunks[0].at, 0);
    assert_eq!(chunks[1].at, 5);
    assert_eq!(ring.text(), "hello world");
    assert_eq!(ring.end_offset(), 11);
}

#[test]
fn a_trim_drops_the_head_but_offsets_never_move() {
    // The property that makes a saved reader position meaningful.
    let mut ring = OutputRing::new(10);
    ring.append("aaaa", JobChannel::Stdout); // 0..4
    ring.append("bbbb", JobChannel::Stdout); // 4..8
    ring.append("cccc", JobChannel::Stdout); // 8..12 -> trims "aaaa"

    assert_eq!(ring.head_offset(), 4, "the tail is retained");
    let retained: Vec<(u64, &str)> = ring
        .chunks()
        .iter()
        .map(|c| (c.at, c.text.as_str()))
        .collect();
    assert_eq!(retained, vec![(4, "bbbb"), (8, "cccc")]);
}

#[test]
fn a_reader_whose_position_fell_off_is_told() {
    // The difference between "the output started here" and "the output you missed was dropped".
    let mut ring = OutputRing::new(8);
    ring.append("aaaa", JobChannel::Stdout);
    ring.append("bbbb", JobChannel::Stdout);
    ring.append("cccc", JobChannel::Stdout);

    let (_, lost) = ring.read_from(0);
    assert!(lost, "reading from 0 after a trim means data was lost");

    let (_, lost) = ring.read_from(ring.head_offset());
    assert!(!lost, "reading from the head loses nothing");
}

#[test]
fn the_first_chunk_after_a_trim_carries_a_gap() {
    // A reader that sees a gap knows its position is no longer continuous.
    let mut ring = OutputRing::new(8);
    ring.append("aaaa", JobChannel::Stdout);
    ring.append("bbbb", JobChannel::Stdout);
    ring.append("cccc", JobChannel::Stdout);
    ring.append("dddd", JobChannel::Stdout);

    let gapped: Vec<&str> = ring
        .chunks()
        .iter()
        .filter(|c| c.gap)
        .map(|c| c.text.as_str())
        .collect();
    assert!(!gapped.is_empty(), "a gap must be reported once");
}

#[test]
fn a_gap_is_reported_once_not_per_chunk() {
    let mut ring = OutputRing::new(8);
    ring.append("aaaa", JobChannel::Stdout);
    ring.append("bbbb", JobChannel::Stdout);
    ring.append("cccc", JobChannel::Stdout);
    ring.append("dddd", JobChannel::Stdout);
    ring.append("eeee", JobChannel::Stdout);

    let gaps = ring.chunks().iter().filter(|c| c.gap).count();
    assert_eq!(gaps, 1, "only the chunk right after a trim carries the gap");
}

#[test]
fn reading_from_a_position_returns_the_chunks_from_there() {
    let mut ring = OutputRing::new(1024);
    ring.append("aaaa", JobChannel::Stdout);
    ring.append("bbbb", JobChannel::Stdout);

    let (chunks, lost) = ring.read_from(4);

    assert!(!lost);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "bbbb");
}

#[test]
fn a_single_oversized_chunk_is_kept_whole() {
    // Trimming it would discard the newest output, which is the only output a caller can act on.
    let mut ring = OutputRing::new(4);
    ring.append("this is much longer than four bytes", JobChannel::Stdout);

    assert_eq!(ring.chunks().len(), 1);
    assert!(ring.text().contains("much longer"));
}

#[test]
fn channels_are_preserved() {
    let mut ring = OutputRing::new(1024);
    ring.append("out", JobChannel::Stdout);
    ring.append("err", JobChannel::Stderr);
    ring.append("log", JobChannel::Log);

    let channels: Vec<JobChannel> = ring.chunks().iter().map(|c| c.channel).collect();
    assert_eq!(
        channels,
        vec![JobChannel::Stdout, JobChannel::Stderr, JobChannel::Log]
    );
}

#[test]
fn a_zero_cap_is_not_unbounded() {
    // A ring that grew without limit would be a memory leak wearing a configuration option.
    let mut ring = OutputRing::new(0);
    for _ in 0..100 {
        ring.append("x", JobChannel::Stdout);
    }

    assert_eq!(
        ring.chunks().len(),
        1,
        "kept to the minimum, not everything"
    );
}

#[test]
fn an_empty_ring_reads_as_empty() {
    let ring = OutputRing::new(1024);

    assert!(ring.chunks().is_empty());
    assert_eq!(ring.text(), "");
    assert_eq!(ring.end_offset(), 0);
}

// ---- status predicates ------------------------------------------------------------------------

#[test]
fn status_predicates_partition_the_lifecycle() {
    assert!(JobStatus::Running.is_live());
    assert!(JobStatus::Stopping.is_live());
    assert!(!JobStatus::Completed.is_live());

    assert!(!JobStatus::Running.is_terminal());
    assert!(JobStatus::Completed.is_terminal());
    assert!(JobStatus::Killed.is_terminal());
    assert!(JobStatus::Failed.is_terminal());
}

#[test]
fn status_wire_names_match_upstream() {
    assert_eq!(JobStatus::Running.as_str(), "running");
    assert_eq!(JobStatus::Stopping.as_str(), "stopping");
    assert_eq!(JobStatus::Completed.as_str(), "completed");
    assert_eq!(JobStatus::Killed.as_str(), "killed");
    assert_eq!(JobStatus::Failed.as_str(), "failed");
}

#[test]
fn an_outcome_is_available_only_when_settled() {
    let registry = JobRegistry::new(1024);
    registry.register(&id("j1"), "bash", "x").unwrap();
    assert!(registry.get("j1").unwrap().outcome().is_none());

    registry.fail("j1", Some(7), 5000).unwrap();
    let outcome = registry.get("j1").unwrap().outcome().expect("settled");

    assert_eq!(outcome.status, JobStatus::Failed);
    assert_eq!(outcome.code, Some(7));
}
