//! Tests for per-tool usage statistics.
//!
//! What is under test is a fold, and the failure modes are all in the details rather than the totals: a tool
//! called many times but answering in a few bytes is a different optimization problem from one called rarely with
//! enormous results, an orphan result must not be attributed to anyone, and a tool with no calls must not divide by
//! zero on its way into a sort. Each of those is pinned here, together with the ordering that makes a "top N"
//! listing meaningful.
//!
//! The module is included by path because `girder-core`'s crate root does not declare it yet — wiring
//! `mod tool_usage;` and its re-exports belongs to the integrator. Once that lands, the `#[path]` line and the two
//! shims below should be replaced with `use girder_core::tool_usage::*;`.

// The source imports `crate::session::SessionEvent` and `crate::llm::ContentBlock`. Because it is included by path
// into this test crate, `crate::` resolves here, so these two shims supply the same types at the same paths. They
// exist only until the crate root declares the module.
mod llm {
    pub use girder_core::ContentBlock;
}

mod session {
    pub use girder_core::SessionEvent;
}

#[path = "../src/tool_usage.rs"]
mod tool_usage;

use girder_core::{ContentBlock, SessionEvent, ToolCallId};
use tool_usage::{ToolStats, usage};

/// A call to `name` with `arguments`, answered later by the result bearing `id`.
fn call(id: &str, name: &str, arguments: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(id).expect("test ids are valid"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

/// A successful result for `id`, carrying `text`.
fn result(id: &str, text: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(id).expect("test ids are valid"),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        is_error: false,
    }
}

/// A failed result for `id`, carrying `text`.
fn failure(id: &str, text: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(id).expect("test ids are valid"),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        is_error: true,
    }
}

/// The row for `name`, or a panic naming what was there instead.
fn row<'a>(rows: &'a [ToolStats], name: &str) -> &'a ToolStats {
    rows.iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("no row for {name}; rows are {rows:?}"))
}

/// The row names, in order.
fn names(rows: &[ToolStats]) -> Vec<&str> {
    rows.iter().map(|row| row.name.as_str()).collect()
}

// ---- one tool -----------------------------------------------------------------------------------

#[test]
fn a_single_call_is_one_row_with_its_bytes() {
    let events = [
        call("c1", "read_file", "{\"path\":\"a\"}"),
        result("c1", "hello"),
    ];
    let rows = usage(&events);
    assert_eq!(rows.len(), 1);
    let stats = &rows[0];
    assert_eq!(stats.name, "read_file");
    assert_eq!(stats.calls, 1);
    assert_eq!(stats.failures, 0);
    assert_eq!(stats.argument_bytes, 12);
    assert_eq!(stats.result_bytes, 5);
    // A rate of exactly zero is the assertion: `0.0` rather than "close to zero", because a tool that never failed
    // must not be reported as having failed a little.
    assert!(stats.failure_rate().abs() < f64::EPSILON);
    assert_eq!(stats.average_result_bytes(), Some(5.0));
}

#[test]
fn repeated_calls_to_one_tool_accumulate_into_one_row() {
    let events = [
        call("c1", "read_file", "aaaa"),
        result("c1", "bb"),
        call("c2", "read_file", "cccccc"),
        result("c2", "bbb"),
    ];
    let rows = usage(&events);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].calls, 2);
    assert_eq!(rows[0].argument_bytes, 10);
    assert_eq!(rows[0].result_bytes, 5);
    assert_eq!(rows[0].average_result_bytes(), Some(2.5));
}

// ---- several tools ------------------------------------------------------------------------------

#[test]
fn each_tool_gets_its_own_row() {
    let events = [
        call("c1", "read_file", "a"),
        result("c1", "rrrr"),
        call("c2", "bash", "bb"),
        result("c2", "ss"),
    ];
    let rows = usage(&events);
    assert_eq!(rows.len(), 2);
    assert_eq!(row(&rows, "read_file").argument_bytes, 1);
    assert_eq!(row(&rows, "read_file").result_bytes, 4);
    assert_eq!(row(&rows, "bash").argument_bytes, 2);
    assert_eq!(row(&rows, "bash").result_bytes, 2);
}

// ---- ordering -----------------------------------------------------------------------------------

#[test]
fn rows_are_ordered_by_calls_descending() {
    // Deliberately built in ascending order of use, so a fold that preserved insertion order would fail here.
    let events = [
        call("c1", "rare", "x"),
        result("c1", "x"),
        call("c2", "common", "x"),
        result("c2", "x"),
        call("c3", "common", "x"),
        result("c3", "x"),
        call("c4", "middling", "x"),
        result("c4", "x"),
        call("c5", "common", "x"),
        result("c5", "x"),
        call("c6", "middling", "x"),
        result("c6", "x"),
    ];
    let rows = usage(&events);
    assert_eq!(names(&rows), ["common", "middling", "rare"]);
    assert_eq!(rows[0].calls, 3);
}

#[test]
fn equally_called_tools_are_ordered_by_name() {
    // Without a tie-break the order would depend on map iteration, and two reports of the same log could disagree.
    let events = [
        call("c1", "write", "x"),
        result("c1", "x"),
        call("c2", "bash", "x"),
        result("c2", "x"),
        call("c3", "grep", "x"),
        result("c3", "x"),
    ];
    assert_eq!(names(&usage(&events)), ["bash", "grep", "write"]);
}

#[test]
fn call_count_decides_before_alphabetical_order() {
    // A tool late in the alphabet but used more often must still come first; the name only breaks ties.
    let events = [
        call("c1", "aaa", "x"),
        result("c1", "x"),
        call("c2", "zzz", "x"),
        result("c2", "x"),
        call("c3", "zzz", "x"),
        result("c3", "x"),
    ];
    assert_eq!(names(&usage(&events)), ["zzz", "aaa"]);
}

// ---- failures -----------------------------------------------------------------------------------

#[test]
fn failure_rate_is_failures_over_calls() {
    let events = [
        call("c1", "read_file", "x"),
        result("c1", "ok"),
        call("c2", "read_file", "x"),
        failure("c2", "no such file"),
        call("c3", "read_file", "x"),
        failure("c3", "no such file"),
        call("c4", "read_file", "x"),
        result("c4", "ok"),
    ];
    let rows = usage(&events);
    assert_eq!(rows[0].calls, 4);
    assert_eq!(rows[0].failures, 2);
    assert!((rows[0].failure_rate() - 0.5).abs() < f64::EPSILON);
}

#[test]
fn a_tool_that_never_fails_has_a_zero_rate() {
    let events = [call("c1", "read_file", "x"), result("c1", "ok")];
    assert!(usage(&events)[0].failure_rate().abs() < f64::EPSILON);
}

#[test]
fn failure_rate_of_no_calls_is_zero_not_nan() {
    // Constructed directly: `usage` cannot produce a zero-call row, but a caller assembling one (a tool listed in
    // the catalogue and never used) must get a sortable, serializable number rather than `NaN`.
    let stats = ToolStats {
        name: "never_used".to_owned(),
        calls: 0,
        failures: 0,
        argument_bytes: 0,
        result_bytes: 0,
    };
    let rate = stats.failure_rate();
    assert!(rate.abs() < f64::EPSILON);
    assert!(!rate.is_nan());
    // The point of the guarantee: comparisons keep working, so a sort by rate stays a sort.
    assert!(rate < 0.5);
    assert!(rate.partial_cmp(&0.0).is_some());
}

#[test]
fn an_unanswered_call_counts_as_a_call_but_not_as_a_failure() {
    // The model asked for the tool, so it is a call; nothing is known about the outcome, so calling it a failure
    // would invent a fact about a tool that may have worked.
    let events = [call("c1", "bash", "x")];
    let rows = usage(&events);
    assert_eq!(rows[0].calls, 1);
    assert_eq!(rows[0].failures, 0);
    assert!(rows[0].failure_rate().abs() < f64::EPSILON);
    assert_eq!(rows[0].result_bytes, 0);
    assert_eq!(rows[0].average_result_bytes(), Some(0.0));
}

// ---- orphans ------------------------------------------------------------------------------------

#[test]
fn a_result_with_no_call_is_counted_for_no_tool() {
    let events = [result("ghost", "nobody asked for this")];
    assert!(usage(&events).is_empty());
}

#[test]
fn an_orphan_does_not_change_a_real_tools_totals() {
    let events = [
        call("c1", "read_file", "a"),
        result("c1", "bb"),
        result("ghost", "cccccccc"),
    ];
    let rows = usage(&events);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].calls, 1);
    assert_eq!(rows[0].result_bytes, 2);
}

#[test]
fn a_duplicate_result_for_one_call_is_counted_once() {
    // The second result answers a call that is no longer open, so it is an orphan; counting it twice would let a
    // retried write inflate the totals.
    let events = [
        call("c1", "bash", "a"),
        result("c1", "bbb"),
        result("c1", "bbbbbb"),
    ];
    let rows = usage(&events);
    assert_eq!(rows[0].result_bytes, 3);
}

#[test]
fn a_result_is_attributed_to_its_own_tool_not_the_last_one_seen() {
    let events = [
        call("c1", "read_file", "a"),
        call("c2", "bash", "b"),
        result("c1", "read answer"),
        result("c2", "bash answer"),
    ];
    let rows = usage(&events);
    assert_eq!(row(&rows, "read_file").result_bytes, 11);
    assert_eq!(row(&rows, "bash").result_bytes, 11);
    assert_eq!(row(&rows, "bash").argument_bytes, 1);
}

// ---- bytes kept apart ---------------------------------------------------------------------------

#[test]
fn cheap_calls_with_huge_results_are_visible_as_such() {
    // The case the two byte counters exist for: `bash` costs almost nothing to call and returns a wall of text,
    // while `read` is called often and answers in a few bytes. A single byte total per tool would hide this.
    let mut events = Vec::new();
    for index in 0..3 {
        events.push(call(&format!("b{index}"), "bash", "ls"));
        events.push(result(&format!("b{index}"), &"x".repeat(1000)));
    }
    for index in 0..10 {
        events.push(call(&format!("r{index}"), "read", "abcdefghij"));
        events.push(result(&format!("r{index}"), "yyyy"));
    }
    let rows = usage(&events);
    // `read` is called more, so it leads...
    assert_eq!(names(&rows), ["read", "bash"]);
    // ...but `bash` moved far more bytes with a third of the calls.
    assert_eq!(row(&rows, "bash").calls, 3);
    assert_eq!(row(&rows, "bash").argument_bytes, 6);
    assert_eq!(row(&rows, "bash").result_bytes, 3000);
    assert_eq!(row(&rows, "read").calls, 10);
    assert_eq!(row(&rows, "read").argument_bytes, 100);
    assert_eq!(row(&rows, "read").result_bytes, 40);
    assert!(
        row(&rows, "bash").average_result_bytes().expect("called")
            > row(&rows, "read").average_result_bytes().expect("called")
    );
}

#[test]
fn arguments_and_results_are_never_folded_together() {
    let events = [call("c1", "bash", "12345"), result("c1", "abc")];
    let rows = usage(&events);
    assert_eq!(rows[0].argument_bytes, 5);
    assert_eq!(rows[0].result_bytes, 3);
}

#[test]
fn a_result_counts_every_block_it_carries() {
    let events = [
        call("c1", "bash", "x"),
        SessionEvent::ToolResult {
            id: ToolCallId::new("c1").expect("test ids are valid"),
            content: vec![
                ContentBlock::Text {
                    text: "abc".to_owned(),
                },
                ContentBlock::Reasoning {
                    text: "de".to_owned(),
                },
            ],
            is_error: false,
        },
    ];
    assert_eq!(usage(&events)[0].result_bytes, 5);
}

#[test]
fn an_attachment_result_counts_its_reference_not_its_payload() {
    // The bytes live in the attachment store, not in the log, so counting the reference is the only honest figure.
    let events = [
        call("c1", "read_image", "x"),
        SessionEvent::ToolResult {
            id: ToolCallId::new("c1").expect("test ids are valid"),
            content: vec![ContentBlock::Image {
                attachment: "att-1".to_owned(),
            }],
            is_error: false,
        },
    ];
    assert_eq!(usage(&events)[0].result_bytes, 5);
}

// ---- empty --------------------------------------------------------------------------------------

#[test]
fn an_empty_event_log_yields_no_rows() {
    assert!(usage(&[]).is_empty());
}

#[test]
fn a_log_without_tool_events_yields_no_rows() {
    let events = [
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: girder_core::TurnEndReason::Completed,
        },
    ];
    assert!(usage(&events).is_empty());
}

#[test]
fn an_empty_row_reports_no_average() {
    let stats = ToolStats {
        name: "never_used".to_owned(),
        calls: 0,
        failures: 0,
        argument_bytes: 0,
        result_bytes: 0,
    };
    assert_eq!(stats.average_result_bytes(), None);
}

// ---- the reported shape -------------------------------------------------------------------------

#[test]
fn a_row_names_the_tool_the_model_called() {
    let events = [call("c1", "grep", "x"), result("c1", "ok")];
    assert_eq!(usage(&events)[0].name, "grep");
}

#[test]
fn two_spellings_of_a_tool_are_two_rows() {
    // Normalising case or punctuation here would report a call count no single registered tool has.
    let events = [
        call("c1", "read_file", "x"),
        result("c1", "ok"),
        call("c2", "read-file", "x"),
        result("c2", "ok"),
    ];
    assert_eq!(names(&usage(&events)), ["read-file", "read_file"]);
}
