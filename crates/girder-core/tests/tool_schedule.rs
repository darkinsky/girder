//! Tests for the tool-call scheduler.
//!
//! The property under test is the one a scheduler can get wrong without any visible failure: two
//! calls that write the same file must not share a batch, because the loser of that race is erased
//! with no error and no log line. The conservative direction is therefore pinned hardest — conflicts
//! split across batches, unknown writes alone — and the permissive direction is pinned too, so the
//! scheduler cannot pass by serialising everything, which would be safe and useless.
//!
//! Determinism is pinned alongside correctness: a plan that changes between two identical runs
//! cannot be compared in a test, logged usefully, or debugged.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/tool_schedule.rs"]
mod tool_schedule;

use tool_schedule::{Call, is_read_only, plan};

/// Builds a call from a name and the resources it affects.
fn call(index: usize, name: &str, affects: &[&str]) -> Call {
    Call::affecting(
        index,
        name,
        affects.iter().map(|resource| (*resource).to_owned()),
    )
}

/// The indexes of every batch, flattened, so a test can assert that nothing was dropped.
fn flattened(batches: &[Vec<usize>]) -> Vec<usize> {
    let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
    all.sort_unstable();
    all
}

// ---- the permissive case ------------------------------------------------------------------------

#[test]
fn all_read_only_calls_share_one_batch() {
    let calls = [
        call(0, "read", &["src/lib.rs"]),
        call(1, "grep", &["src/lib.rs"]),
        call(2, "glob", &["src"]),
    ];
    let outcome = plan(&calls);
    assert_eq!(outcome.batches, vec![vec![0, 1, 2]]);
}

#[test]
fn two_reads_of_the_same_file_do_not_conflict() {
    let calls = [call(0, "read", &["a.txt"]), call(1, "read", &["a.txt"])];
    assert_eq!(plan(&calls).batches, vec![vec![0, 1]]);
}

#[test]
fn read_only_calls_without_declared_resources_still_share_a_batch() {
    let calls = [Call::new(0, "web_search"), Call::new(1, "web_fetch")];
    assert_eq!(plan(&calls).batches, vec![vec![0, 1]]);
}

// ---- the core case: conflicts -------------------------------------------------------------------

#[test]
fn two_writes_of_the_same_file_land_in_different_batches() {
    let calls = [call(0, "write", &["a.txt"]), call(1, "write", &["a.txt"])];
    let outcome = plan(&calls);
    assert_eq!(outcome.batches, vec![vec![0], vec![1]]);
    assert!(
        outcome.reasons[1].contains("a.txt"),
        "the reason must name the conflicting resource: {:?}",
        outcome.reasons
    );
}

#[test]
fn a_read_of_a_file_a_later_batch_writes_is_kept_apart() {
    let calls = [call(0, "read", &["a.txt"]), call(1, "write", &["a.txt"])];
    assert_eq!(plan(&calls).batches, vec![vec![0], vec![1]]);
}

#[test]
fn a_write_of_a_file_an_earlier_batch_reads_is_kept_apart() {
    let calls = [call(0, "write", &["a.txt"]), call(1, "read", &["a.txt"])];
    assert_eq!(plan(&calls).batches, vec![vec![0], vec![1]]);
}

#[test]
fn paths_are_compared_after_normalization() {
    // The same file spelled two ways is the same file; a scheduler that missed this would let two
    // writes of one path overlap on a trailing separator alone.
    let calls = [call(0, "write", &["a.txt"]), call(1, "write", &["a.txt/"])];
    assert_eq!(plan(&calls).batches, vec![vec![0], vec![1]]);
}

#[test]
fn several_conflict_groups_interleave_into_two_batches() {
    // Three writers, each repeated once: a greedy first-fit schedule packs the second round into one
    // second batch, which is the point of grouping rather than serialising.
    let calls = [
        call(0, "write", &["a.txt"]),
        call(1, "write", &["b.txt"]),
        call(2, "write", &["c.txt"]),
        call(3, "write", &["a.txt"]),
        call(4, "write", &["b.txt"]),
        call(5, "write", &["c.txt"]),
    ];
    let outcome = plan(&calls);
    assert_eq!(outcome.batches, vec![vec![0, 1, 2], vec![3, 4, 5]]);
    assert_eq!(outcome.reasons.len(), outcome.batches.len());
}

#[test]
fn a_multi_resource_call_blocks_on_any_one_of_them() {
    let calls = [
        call(0, "write", &["a.txt", "b.txt"]),
        call(1, "write", &["b.txt"]),
        call(2, "read", &["b.txt"]),
    ];
    // Call 1 conflicts on `b.txt`; call 2 is a read, and the batch writes `b.txt`, so it waits too.
    assert_eq!(plan(&calls).batches, vec![vec![0], vec![1], vec![2]]);
}

#[test]
fn independent_writes_share_a_batch() {
    let calls = [call(0, "write", &["a.txt"]), call(1, "write", &["b.txt"])];
    assert_eq!(plan(&calls).batches, vec![vec![0, 1]]);
}

// ---- the other core case: unknown writes --------------------------------------------------------

#[test]
fn a_write_without_declared_resources_runs_alone() {
    let outcome = plan(&[Call::new(0, "bash")]);
    assert_eq!(outcome.batches, vec![vec![0]]);
    assert!(
        outcome.reasons[0].contains("without declaring"),
        "the reason must say the reach was unknown: {:?}",
        outcome.reasons
    );
}

#[test]
fn nothing_joins_an_unknown_write_even_on_unrelated_resources() {
    let calls = [
        call(0, "bash", &[]),
        call(1, "write", &["unrelated.txt"]),
        call(2, "read", &["another.txt"]),
    ];
    // The unknown call could be writing either of the other paths, so they must not share its batch.
    assert_eq!(plan(&calls).batches, vec![vec![0], vec![1, 2]]);
}

#[test]
fn two_unknown_writes_never_share_a_batch() {
    let calls = [Call::new(0, "bash"), Call::new(1, "bash")];
    assert_eq!(plan(&calls).batches, vec![vec![0], vec![1]]);
}

// ---- shape ---------------------------------------------------------------------------------------

#[test]
fn empty_input_gives_an_empty_plan() {
    let outcome = plan(&[]);
    assert!(outcome.batches.is_empty());
    assert!(outcome.reasons.is_empty());
}

#[test]
fn one_call_gives_one_batch() {
    let outcome = plan(&[call(7, "read", &["a.txt"])]);
    assert_eq!(outcome.batches, vec![vec![7]]);
    assert_eq!(outcome.reasons.len(), 1);
}

#[test]
fn every_call_appears_exactly_once() {
    let calls = [
        call(0, "write", &["a.txt"]),
        call(1, "read", &["a.txt"]),
        call(2, "bash", &[]),
        call(3, "read", &["b.txt"]),
        call(4, "write", &["a.txt"]),
    ];
    let outcome = plan(&calls);
    assert_eq!(flattened(&outcome.batches), vec![0, 1, 2, 3, 4]);
}

#[test]
fn indexes_are_carried_through_not_renumbered() {
    // A caller that filtered its list can still map a batch back to its own calls.
    let calls = [call(41, "write", &["a.txt"]), call(9, "read", &["b.txt"])];
    assert_eq!(plan(&calls).batches, vec![vec![41, 9]]);
}

#[test]
fn a_batch_holds_its_calls_in_input_order() {
    let calls = [
        call(0, "read", &["a.txt"]),
        call(1, "write", &["b.txt"]),
        call(2, "read", &["c.txt"]),
    ];
    assert_eq!(plan(&calls).batches, vec![vec![0, 1, 2]]);
}

#[test]
fn the_same_input_produces_the_same_plan_twice() {
    let calls = [
        call(0, "read", &["a.txt", "b.txt"]),
        call(1, "write", &["b.txt"]),
        call(2, "bash", &[]),
        call(3, "write", &["a.txt"]),
        call(4, "read", &["c.txt"]),
    ];
    let first = plan(&calls);
    let second = plan(&calls);
    assert_eq!(first.batches, second.batches);
    assert_eq!(first.reasons, second.reasons);
}

// ---- the allow-list ------------------------------------------------------------------------------

#[test]
fn observing_tools_are_read_only() {
    for tool in [
        "read",
        "grep",
        "glob",
        "lsp",
        "web_fetch",
        "web_search",
        "session_search",
        "instructions",
        "skill",
        "job_list",
        "job_output",
        "subagent_list",
        "workflow_status",
    ] {
        assert!(is_read_only(tool), "{tool} should be read-only");
    }
}

#[test]
fn changing_tools_are_not_read_only() {
    for tool in [
        "write",
        "edit",
        "apply_patch",
        "bash",
        "delete_path",
        "move_path",
        "copy_path",
        "make_dir",
        "subagent",
        "workflow_run",
        "job_start",
        "terminal_start",
        "terminal_send",
        "todo_write",
        "present",
        "goal",
        "unknown_tool",
    ] {
        assert!(!is_read_only(tool), "{tool} should not be read-only");
    }
}

#[test]
fn terminal_read_is_not_read_only_because_it_consumes_output() {
    // Two overlapping reads would race over which one receives what the terminal printed.
    assert!(!is_read_only("terminal_read"));
}
