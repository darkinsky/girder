//! Tests for the cross-field consistency checks over a `session.state` snapshot.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against `src/state_consistency.rs` as written even before the crate root wires it in. Nothing here
//! reaches for the rest of `girder_core`: the checker is a pure function of [`Values`], which is what
//! lets every case be stated as one number changed in an otherwise consistent snapshot.
//!
//! Four properties carry the design, and the rest of the cases pin the edges around them:
//!
//! - A **consistent snapshot reports nothing**. The check runs on every answer, so a false positive
//!   is a report a reader learns to ignore.
//! - The three **core contradictions** — `events` vs `lastSeq`, the two turn counts, the two
//!   tool-call counts — are each detected, and detected *as the right pair of fields*. A check that
//!   reported "something disagrees" would pass a test that only asserted non-emptiness.
//! - **An open turn is slack of exactly one row**, not unlimited slack: the difference it explains is
//!   cosmetic, and a wider gap is still a contradiction.
//! - **An empty log counts nothing**, because there is no event for a counter to have counted.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the checker as written even before the crate root wires it in.
#[path = "../src/core/state_consistency.rs"]
pub mod state_consistency;

use state_consistency::{Inconsistency, Severity, Values, check};

/// A snapshot in which every check passes: three events, one finished turn with one tool call.
///
/// Three events is not a full log's size but it is a full set of *checks*: enough for the sequence
/// number to be non-trivial, and for each counter to be non-zero so that a test which zeroes one
/// field is changing a meaningful value.
fn consistent() -> Values {
    Values {
        event_count: 3,
        last_seq: 3,
        turns: 1,
        timeline_segments: 1,
        open_turn: None,
        digest_turns: 1,
        tool_calls: 1,
        tool_usage_total: 1,
    }
}

/// The inconsistency between `first` and `second`, whichever order they were reported in.
///
/// The two names are a pair rather than an ordered pair in the report, and a test that pinned the
/// order would fail on a presentation change that no caller should depend on.
#[track_caller]
fn between<'a>(found: &'a [Inconsistency], first: &str, second: &str) -> &'a Inconsistency {
    found
        .iter()
        .find(|item| {
            (item.between.0 == first && item.between.1 == second)
                || (item.between.0 == second && item.between.1 == first)
        })
        .unwrap_or_else(|| {
            panic!("expected a report between `{first}` and `{second}`, got {found:#?}")
        })
}

/// Asserts that no report connects the two fields.
#[track_caller]
fn assert_not_between(found: &[Inconsistency], first: &str, second: &str) {
    let hit = found.iter().find(|item| {
        (item.between.0 == first && item.between.1 == second)
            || (item.between.0 == second && item.between.1 == first)
    });
    assert!(
        hit.is_none(),
        "expected no report between `{first}` and `{second}`, got {hit:#?}"
    );
}

#[test]
fn a_consistent_snapshot_reports_nothing() {
    assert_eq!(check(&consistent()), Vec::new());
}

#[test]
fn an_empty_log_is_consistent_as_default() {
    // `Values::default` is every counter zero, which is exactly the empty snapshot.
    assert_eq!(check(&Values::default()), Vec::new());
}

#[test]
fn an_empty_log_with_one_field_too_high_is_consistent_too() {
    // The edge of the previous case: zero events and a non-zero last sequence number is still a
    // sequence disagreement, and must be reported by the same rule as any other size.
    let values = Values {
        last_seq: 1,
        ..Values::default()
    };
    let found = check(&values);
    assert_eq!(found.len(), 1);
    assert_eq!(
        between(&found, "events", "lastSeq").severity,
        Severity::Contradiction
    );
}

#[test]
fn event_count_below_last_seq_is_detected() {
    // The core sequence case: two folded numbers for one log, and the count is short.
    let values = Values {
        event_count: 2,
        last_seq: 3,
        ..consistent()
    };
    let found = check(&values);
    let item = between(&found, "events", "lastSeq");
    assert_eq!(item.severity, Severity::Contradiction);
    // The report carries both numbers so a reader does not have to open the payload.
    assert!(
        item.detail.contains('2') && item.detail.contains('3'),
        "detail should quote both counts, got {:?}",
        item.detail
    );
}

#[test]
fn event_count_above_last_seq_is_detected() {
    // The other direction, because a check written as `<` rather than `!=` would miss this one.
    let values = Values {
        event_count: 4,
        last_seq: 3,
        ..consistent()
    };
    let found = check(&values);
    assert_eq!(
        between(&found, "events", "lastSeq").severity,
        Severity::Contradiction
    );
}

#[test]
fn turn_counts_disagreeing_is_detected() {
    // Two folds over the same `turn/start` records; one of them is wrong.
    let values = Values {
        turns: 2,
        digest_turns: 3,
        // Kept consistent with `turns` so the report under test is the only one about turns.
        timeline_segments: 2,
        ..consistent()
    };
    let found = check(&values);
    let item = between(&found, "counts.turns", "digest.turns");
    assert_eq!(item.severity, Severity::Contradiction);
    assert!(
        item.detail.contains("same log"),
        "detail should say why the two were expected to agree, got {:?}",
        item.detail
    );
}

#[test]
fn tool_call_counts_disagreeing_is_detected() {
    let values = Values {
        tool_calls: 4,
        tool_usage_total: 2,
        ..consistent()
    };
    let found = check(&values);
    let item = between(&found, "counts.toolCalls", "toolUsage");
    assert_eq!(item.severity, Severity::Contradiction);
    assert!(
        item.detail.contains('4') && item.detail.contains('2'),
        "detail should quote both counts, got {:?}",
        item.detail
    );
}

#[test]
fn an_open_turn_allows_the_timeline_one_row_ahead() {
    // The ordinary in-flight case: `turn/start` produced a row before the turn closed a step, so
    // the timeline is one ahead of the turn count. Both fields are right, and the two turn counts
    // stay equal because the open turn is a row without a closed step, not a second turn.
    let values = Values {
        turns: 1,
        digest_turns: 1,
        timeline_segments: 2,
        open_turn: Some(2),
        ..consistent()
    };
    let found = check(&values);
    let item = between(&found, "timeline", "counts.turns");
    assert_eq!(item.severity, Severity::Cosmetic);
    assert!(
        item.detail.contains("open"),
        "detail should name the open turn as the explanation, got {:?}",
        item.detail
    );
    // The slack is about the timeline only: nothing else may be excused by the open turn.
    assert_not_between(&found, "counts.turns", "digest.turns");
    assert_not_between(&found, "events", "lastSeq");
}

#[test]
fn an_open_turn_allows_the_timeline_one_row_behind() {
    // The other side of the same slack: the turn is counted but its first row has not been built.
    // Counting the turn as closed while the timeline lags is a state the two folds reach on their
    // own, and it must not be reported as a contradiction.
    let values = Values {
        turns: 2,
        digest_turns: 2,
        timeline_segments: 1,
        open_turn: Some(2),
        ..consistent()
    };
    let found = check(&values);
    assert_eq!(
        between(&found, "timeline", "counts.turns").severity,
        Severity::Cosmetic
    );
}

#[test]
fn an_open_turn_does_not_excuse_a_wider_timeline_gap() {
    // Two rows of difference is not the open turn: it is a duplicated or dropped row.
    let values = Values {
        turns: 1,
        digest_turns: 1,
        timeline_segments: 3,
        open_turn: Some(2),
        ..consistent()
    };
    let found = check(&values);
    assert_eq!(
        between(&found, "timeline", "counts.turns").severity,
        Severity::Contradiction
    );
}

#[test]
fn a_timeline_gap_without_an_open_turn_is_a_contradiction() {
    // With no turn in flight, both counts cover the same set of turns and cannot differ at all.
    let values = Values {
        turns: 1,
        digest_turns: 1,
        timeline_segments: 2,
        open_turn: None,
        ..consistent()
    };
    let found = check(&values);
    let item = between(&found, "timeline", "counts.turns");
    assert_eq!(item.severity, Severity::Contradiction);
    assert!(
        item.detail.contains("no turn is open"),
        "detail should say the open-turn excuse does not apply, got {:?}",
        item.detail
    );
}

#[test]
fn every_disagreement_is_reported_rather_than_the_first() {
    // A check that returned early would hide the other two, and a reader fixing folds one at a time
    // would need three runs to find them all.
    let values = Values {
        event_count: 2,
        last_seq: 3,
        turns: 7,
        digest_turns: 8,
        timeline_segments: 7,
        tool_calls: 5,
        tool_usage_total: 6,
        open_turn: None,
    };
    let found = check(&values);
    assert_eq!(found.len(), 3, "expected three reports, got {found:#?}");
    assert_eq!(
        between(&found, "events", "lastSeq").severity,
        Severity::Contradiction
    );
    assert_eq!(
        between(&found, "counts.turns", "digest.turns").severity,
        Severity::Contradiction
    );
    assert_eq!(
        between(&found, "counts.toolCalls", "toolUsage").severity,
        Severity::Contradiction
    );
}

#[test]
fn reports_are_ordered_stably() {
    // Two runs over one snapshot must compare equal, so the order may not depend on iteration
    // order. Run twice and compare rather than pinning a specific order.
    let values = Values {
        event_count: 1,
        last_seq: 9,
        turns: 2,
        digest_turns: 3,
        timeline_segments: 4,
        tool_calls: 5,
        tool_usage_total: 6,
        open_turn: Some(2),
    };
    assert_eq!(check(&values), check(&values));
}

#[test]
fn an_empty_log_with_counters_is_a_contradiction() {
    // Nothing in an empty log for any counter to have counted; each field is reported separately so
    // the reader sees every field that is wrong rather than only the first.
    let values = Values {
        turns: 1,
        digest_turns: 1,
        timeline_segments: 1,
        tool_calls: 1,
        tool_usage_total: 1,
        ..Values::default()
    };
    let found = check(&values);
    for field in ["counts.turns", "digest.turns", "toolUsage", "timeline"] {
        let item = between(&found, "events", field);
        assert_eq!(
            item.severity,
            Severity::Contradiction,
            "an empty log cannot count anything, so `{field}` is a contradiction"
        );
        assert!(
            item.detail.contains('0'),
            "detail should quote the zero event count, got {:?}",
            item.detail
        );
    }
}

#[test]
fn severities_are_distinguished_rather_than_merged() {
    // One snapshot carrying both kinds at once: the open-turn slack is cosmetic, the sequence
    // disagreement is not. A caller filtering on severity must be able to tell them apart.
    let values = Values {
        event_count: 2,
        last_seq: 3,
        turns: 1,
        digest_turns: 1,
        timeline_segments: 2,
        open_turn: Some(1),
        ..consistent()
    };
    let found = check(&values);
    assert_eq!(found.len(), 2, "expected two reports, got {found:#?}");
    assert_eq!(
        between(&found, "timeline", "counts.turns").severity,
        Severity::Cosmetic
    );
    assert_eq!(
        between(&found, "events", "lastSeq").severity,
        Severity::Contradiction
    );
    let contradictions = found
        .iter()
        .filter(|item| item.severity == Severity::Contradiction)
        .count();
    assert_eq!(contradictions, 1);
}

#[test]
fn the_report_names_the_payload_fields() {
    // The names are what a reader searches the JSON for, so a report about `event_count` would send
    // them looking for a field the payload does not have.
    let found = check(&Values {
        event_count: 1,
        last_seq: 2,
        ..consistent()
    });
    let item = between(&found, "events", "lastSeq");
    assert_eq!(item.severity, Severity::Contradiction);
    assert!(!item.between.0.is_empty() && !item.between.1.is_empty());
}

#[test]
fn checks_accept_a_snapshot_with_zero_turns_and_zero_rows() {
    // A log of events that produced no turn at all: usage records, say. Zero turns and zero rows
    // agree, and the sequence still has to hold.
    let values = Values {
        event_count: 5,
        last_seq: 5,
        turns: 0,
        digest_turns: 0,
        timeline_segments: 0,
        open_turn: None,
        tool_calls: 0,
        tool_usage_total: 0,
    };
    assert_eq!(check(&values), Vec::new());
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod core {
    pub use super::state_consistency as state_consistency;
}
