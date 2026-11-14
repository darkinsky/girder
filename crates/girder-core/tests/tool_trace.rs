//! Tests for per-call phase timing.
//!
//! What is under test is a breakdown that must add up and a reduction that must rank the right
//! stage, and the interesting failures are all in the details: `slowest` returning the first entry
//! instead of the largest, a total that special-cases `Done` and therefore double-counts, and a
//! cross-call aggregate that ranks a single spike above a phase that is uniformly expensive. Each of
//! those is pinned here, together with the empty and unfinished cases that a report is most likely to
//! meet in the field.
//!
//! The module is included by path because `girder-core`'s crate root does not declare it yet —
//! wiring `mod tool_trace;` and its re-exports belongs to the integrator. Once that lands, the
//! `#[path]` line should be replaced with `use girder_core::tool_trace::*;`.

#[path = "../src/tool_trace.rs"]
mod tool_trace;

use tool_trace::{Phase, Summary, Trace, summarize};

/// A trace for call `id` with `(phase, ms)` entries recorded in order.
fn trace(id: &str, phases: &[(Phase, u64)]) -> Trace {
    let mut trace = Trace::new(id, "bash");
    for &(phase, ms) in phases {
        trace.record(phase, ms);
    }
    trace
}

#[test]
fn new_trace_is_empty_and_not_failed() {
    let trace = Trace::new("call-1", "read");
    assert_eq!(trace.call_id, "call-1");
    assert_eq!(trace.tool, "read");
    assert!(trace.phases.is_empty());
    assert!(!trace.failed);
    assert_eq!(trace.total_ms(), 0);
    assert_eq!(trace.slowest(), None);
    assert!(!trace.is_done());
}

#[test]
fn accepts_owned_and_borrowed_ids() {
    assert_eq!(Trace::new("a", "b").call_id, "a");
    assert_eq!(Trace::new(String::from("a"), String::from("b")).tool, "b");
}

#[test]
fn single_phase_is_its_own_total_and_slowest() {
    let trace = trace("c", &[(Phase::Executing, 1_200)]);
    assert_eq!(trace.total_ms(), 1_200);
    assert_eq!(trace.slowest(), Some((Phase::Executing, 1_200)));
}

#[test]
fn total_sums_every_phase() {
    let trace = trace(
        "c",
        &[
            (Phase::Generating, 300),
            (Phase::Validating, 2),
            (Phase::Deciding, 40),
            (Phase::Executing, 1_500),
            (Phase::PostProcessing, 8),
            (Phase::Done, 5),
        ],
    );
    assert_eq!(trace.total_ms(), 300 + 2 + 40 + 1_500 + 8 + 5);
}

#[test]
fn slowest_returns_the_largest_not_the_first() {
    // The first entry is deliberately not the largest; a `slowest` that returned the head of the
    // list would pass a naive test and fail here.
    let trace = trace(
        "c",
        &[
            (Phase::Generating, 10),
            (Phase::Validating, 1),
            (Phase::Deciding, 20),
            (Phase::Executing, 5_000),
            (Phase::PostProcessing, 30),
        ],
    );
    assert_eq!(trace.slowest(), Some((Phase::Executing, 5_000)));
}

#[test]
fn slowest_finds_a_dominant_early_phase() {
    // The mirror of the previous case: the largest entry is first, so a `slowest` that returned the
    // last entry would fail here and pass there.
    let trace = trace(
        "c",
        &[
            (Phase::Generating, 4_000),
            (Phase::Validating, 1),
            (Phase::Deciding, 900),
            (Phase::Executing, 40),
            (Phase::PostProcessing, 2),
        ],
    );
    assert_eq!(trace.slowest(), Some((Phase::Generating, 4_000)));
}

#[test]
fn slowest_breaks_ties_toward_the_earlier_recorded_entry() {
    let trace = trace(
        "c",
        &[
            (Phase::Executing, 500),
            (Phase::Deciding, 500),
            (Phase::Generating, 500),
        ],
    );
    assert_eq!(trace.slowest(), Some((Phase::Executing, 500)));
}

#[test]
fn repeated_phase_is_kept_and_summed() {
    // A retried body: both attempts happened, so both durations are in the total, while `slowest`
    // reports one occurrence.
    let trace = trace("c", &[(Phase::Executing, 400), (Phase::Executing, 600)]);
    assert_eq!(trace.phases.len(), 2);
    assert_eq!(trace.total_ms(), 1_000);
    assert_eq!(trace.slowest(), Some((Phase::Executing, 600)));
}

#[test]
fn done_is_a_stage_and_not_the_total() {
    // If `Done` carried the total, the total would be doubled.
    let trace = trace("c", &[(Phase::Executing, 700), (Phase::Done, 3)]);
    assert_eq!(trace.total_ms(), 703);
    assert!(trace.is_done());
    assert_eq!(trace.slowest(), Some((Phase::Executing, 700)));
}

#[test]
fn unfinished_trace_is_not_done_but_still_measured() {
    let trace = trace("c", &[(Phase::Validating, 1), (Phase::Executing, 9_000)]);
    assert!(!trace.is_done());
    assert_eq!(trace.total_ms(), 9_001);
    assert_eq!(trace.slowest(), Some((Phase::Executing, 9_000)));
}

#[test]
fn summarize_empty_batch_has_no_slowest_phase() {
    let summary = summarize(&[]);
    assert_eq!(
        summary,
        Summary {
            calls: 0,
            total_ms: 0,
            slowest_phase: None,
            failures: 0,
        }
    );
}

#[test]
fn summarize_counts_calls_and_sums_totals() {
    let traces = [
        trace("a", &[(Phase::Executing, 100), (Phase::Done, 1)]),
        trace("b", &[(Phase::Executing, 200), (Phase::Done, 2)]),
    ];
    let summary = summarize(&traces);
    assert_eq!(summary.calls, 2);
    assert_eq!(summary.total_ms, 303);
    assert_eq!(summary.failures, 0);
}

#[test]
fn summarize_counts_failures_without_excluding_their_time() {
    let mut failed = trace("b", &[(Phase::Executing, 5_000)]);
    failed.failed = true;

    let traces = [trace("a", &[(Phase::Executing, 100)]), failed];
    let summary = summarize(&traces);
    assert_eq!(summary.calls, 2);
    assert_eq!(summary.failures, 1);
    // A failing call still cost its five seconds.
    assert_eq!(summary.total_ms, 5_100);
}

#[test]
fn summarize_counts_an_empty_failure() {
    let mut trace = Trace::new("c", "bash");
    trace.failed = true;
    let summary = summarize(&[trace]);
    assert_eq!(summary.calls, 1);
    assert_eq!(summary.failures, 1);
    assert_eq!(summary.total_ms, 0);
    assert_eq!(summary.slowest_phase, None);
}

#[test]
fn slowest_phase_aggregates_across_calls() {
    // Per call, the slowest stage is `Executing` in every trace. The aggregate then reports the same
    // phase, because 500 + 90 + 60 outranks 40 + 40 + 40 -- this pins that the aggregate is a sum
    // over calls rather than a pick of one call's answer.
    let traces = [
        trace(
            "a",
            &[
                (Phase::Deciding, 40),
                (Phase::Executing, 500),
                (Phase::Done, 1),
            ],
        ),
        trace(
            "b",
            &[
                (Phase::Deciding, 40),
                (Phase::Executing, 90),
                (Phase::Done, 1),
            ],
        ),
        trace(
            "c",
            &[
                (Phase::Deciding, 40),
                (Phase::Executing, 60),
                (Phase::Done, 1),
            ],
        ),
    ];

    assert_eq!(traces[0].slowest(), Some((Phase::Executing, 500)));
    assert_eq!(traces[1].slowest(), Some((Phase::Executing, 90)));
    assert_eq!(traces[2].slowest(), Some((Phase::Executing, 60)));

    let summary = summarize(&traces);
    assert_eq!(summary.slowest_phase, Some((Phase::Executing, 650)));
}

#[test]
fn slowest_phase_prefers_repeated_cost_over_one_spike() {
    // Twelve calls with a uniform 100ms `Deciding` add up to 1200ms, which outranks the single
    // 900ms `Executing` spike. Ranking single durations would report the spike and hide this.
    let mut traces = vec![trace("spike", &[(Phase::Executing, 900)])];
    for index in 0..12 {
        let id = format!("call-{index}");
        traces.push(trace(&id, &[(Phase::Deciding, 100)]));
    }

    let summary = summarize(&traces);
    assert_eq!(summary.calls, 13);
    assert_eq!(summary.slowest_phase, Some((Phase::Deciding, 1_200)));
}

#[test]
fn slowest_phase_breaks_ties_toward_the_earlier_phase() {
    // Equal totals: `Deciding` precedes `Executing` in lifecycle order and wins.
    let traces = [
        trace("a", &[(Phase::Executing, 100)]),
        trace("b", &[(Phase::Deciding, 100)]),
    ];
    let summary = summarize(&traces);
    assert_eq!(summary.slowest_phase, Some((Phase::Deciding, 100)));
}

#[test]
fn slowest_phase_ignores_the_done_marker() {
    let traces = [
        trace("a", &[(Phase::Executing, 2_000), (Phase::Done, 1)]),
        trace("b", &[(Phase::Executing, 1_000), (Phase::Done, 1)]),
    ];
    let summary = summarize(&traces);
    assert_eq!(summary.slowest_phase, Some((Phase::Executing, 3_000)));
}

#[test]
fn summarize_counts_unfinished_traces() {
    let traces = [
        trace("a", &[(Phase::Executing, 10)]),
        trace("b", &[(Phase::Executing, 20)]),
    ];
    let summary = summarize(&traces);
    assert_eq!(summary.calls, 2);
    assert_eq!(summary.total_ms, 30);
    assert_eq!(summary.slowest_phase, Some((Phase::Executing, 30)));
}

#[test]
fn saturating_total_does_not_overflow() {
    let trace = trace(
        "c",
        &[(Phase::Executing, u64::MAX), (Phase::Executing, u64::MAX)],
    );
    assert_eq!(trace.total_ms(), u64::MAX);
    assert_eq!(trace.slowest(), Some((Phase::Executing, u64::MAX)));
}
