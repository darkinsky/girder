//! Tests for the tool batch strategy.
//!
//! The property under test is narrow and load-bearing: a batch may only overlap when every call in it
//! is read-only. Everything else — writes, destructives, and mixtures — must serialize, because the
//! failure mode of getting this wrong is silent data loss rather than a visible error. These tests
//! therefore pin the conservative direction hardest: each case that should be sequential is asserted,
//! and the one case that may be parallel is asserted to make sure the conservative rule has not
//! collapsed into "always sequential", which would be safe but useless.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. That is why nothing here reaches for
//! anything else in `girder_core`.

#[path = "../src/tool_batching.rs"]
mod tool_batching;

use tool_batching::{Strategy, ToolCallInfo, choose};

/// A batch of `count` read-only calls.
fn read_only_batch(count: usize) -> Vec<ToolCallInfo> {
    (0..count)
        .map(|index| ToolCallInfo::read_only(format!("read-{index}")))
        .collect()
}

// ---- the four decisions -------------------------------------------------------------------------

#[test]
fn every_call_read_only_means_parallel() {
    let calls = [
        ToolCallInfo::read_only("read_file"),
        ToolCallInfo::read_only("grep"),
        ToolCallInfo::read_only("glob"),
    ];
    assert_eq!(choose(&calls), Strategy::Parallel);
}

#[test]
fn every_call_a_write_means_sequential() {
    let calls = [
        ToolCallInfo::write("write_file"),
        ToolCallInfo::write("write_file"),
        ToolCallInfo::write("patch"),
    ];
    assert_eq!(choose(&calls), Strategy::Sequential);
}

#[test]
fn a_mixed_batch_is_sequential_whatever_the_order() {
    let read_first = [
        ToolCallInfo::read_only("read_file"),
        ToolCallInfo::write("write_file"),
    ];
    let write_first = [
        ToolCallInfo::write("write_file"),
        ToolCallInfo::read_only("read_file"),
    ];
    // A write may consume the read's result, and the scheduler cannot see that data flow.
    assert_eq!(choose(&read_first), Strategy::Sequential);
    assert_eq!(choose(&write_first), Strategy::Sequential);
}

#[test]
fn one_destructive_call_serializes_a_batch_of_reads() {
    let calls = [
        ToolCallInfo::read_only("read_file"),
        ToolCallInfo::read_only("grep"),
        ToolCallInfo::destructive("remove_file"),
    ];
    assert_eq!(choose(&calls), Strategy::Sequential);
}

#[test]
fn one_destructive_call_serializes_a_batch_of_writes() {
    let calls = [
        ToolCallInfo::write("write_file"),
        ToolCallInfo::destructive("remove_file"),
    ];
    assert_eq!(choose(&calls), Strategy::Sequential);
}

// ---- edges --------------------------------------------------------------------------------------

#[test]
fn an_empty_batch_is_sequential() {
    // No call runs either way; `Sequential` is the answer that claims no concurrency policy that was
    // never chosen on evidence.
    let calls: [ToolCallInfo; 0] = [];
    assert_eq!(choose(&calls), Strategy::Sequential);
}

#[test]
fn a_single_read_only_call_may_run_in_parallel() {
    let calls = [ToolCallInfo::read_only("read_file")];
    assert_eq!(choose(&calls), Strategy::Parallel);
}

#[test]
fn a_single_write_call_runs_sequentially() {
    let calls = [ToolCallInfo::write("write_file")];
    assert_eq!(choose(&calls), Strategy::Sequential);
}

#[test]
fn a_single_destructive_call_runs_sequentially() {
    let calls = [ToolCallInfo::destructive("remove_file")];
    assert_eq!(choose(&calls), Strategy::Sequential);
}

#[test]
fn an_undeclared_call_is_not_parallel_safe() {
    // A caller that knows nothing about a tool must not get concurrency by omission.
    let unknown = ToolCallInfo::new("some_future_tool");
    assert!(!unknown.read_only);
    assert!(!unknown.destructive);
    assert!(!unknown.is_parallel_safe());
    assert_eq!(choose(&[unknown]), Strategy::Sequential);
}

#[test]
fn a_call_claiming_both_read_only_and_destructive_is_treated_as_destructive() {
    let contradictory = ToolCallInfo {
        name: "remove_file".to_owned(),
        read_only: true,
        destructive: true,
    };
    assert!(!contradictory.is_parallel_safe());
    assert_eq!(choose(&[contradictory]), Strategy::Sequential);
}

#[test]
fn a_large_read_only_batch_is_still_parallel() {
    // The decision is about safety, not about how many calls a caller should have asked for; capping
    // concurrency is the executor's job.
    let calls = read_only_batch(50);
    assert_eq!(choose(&calls), Strategy::Parallel);
}

// ---- FailFast is never inferred -----------------------------------------------------------------

#[test]
fn fail_fast_is_never_chosen_automatically() {
    // `FailFast` asserts that a batch's calls depend on each other, which only the caller can know.
    let cases: [&[ToolCallInfo]; 4] = [
        &[],
        &[ToolCallInfo::read_only("read_file")],
        &[
            ToolCallInfo::write("write_file"),
            ToolCallInfo::write("patch"),
        ],
        &[
            ToolCallInfo::read_only("read_file"),
            ToolCallInfo::destructive("remove_file"),
        ],
    ];
    for calls in cases {
        assert_ne!(choose(calls), Strategy::FailFast, "batch {calls:?}");
    }
}

// ---- accessors agree with the variants ----------------------------------------------------------

#[test]
fn strategy_accessors_describe_their_variant() {
    assert!(Strategy::Parallel.is_parallel());
    assert!(!Strategy::Sequential.is_parallel());
    assert!(!Strategy::FailFast.is_parallel());

    assert!(Strategy::FailFast.stops_on_first_failure());
    assert!(!Strategy::Sequential.stops_on_first_failure());
    assert!(!Strategy::Parallel.stops_on_first_failure());
}

#[test]
fn strategy_names_are_stable_and_distinct() {
    let names = [
        Strategy::Sequential.as_str(),
        Strategy::Parallel.as_str(),
        Strategy::FailFast.as_str(),
    ];
    assert_eq!(names, ["sequential", "parallel", "fail-fast"]);
    assert_eq!(Strategy::Parallel.to_string(), "parallel");
}
