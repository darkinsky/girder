//! Tests for diagnosing a session log's health.
//!
//! The properties pinned here are the ones a plausible implementation gets wrong, and the first two
//! are the reason the module exists:
//!
//! - **An unanswered call at the end of a log is `Info`** — a live session waiting on a tool is
//!   indistinguishable, structurally, from one that died waiting, and a score that charged for it
//!   would rank every running conversation below every idle one.
//! - **A finished session's unanswered call is `Warning`** — once the turn is closed, no result can
//!   arrive for it. This is the same evidence as above and the opposite verdict, so the fixtures
//!   differ in exactly one record: the `turn/end`.
//! - **An orphan result is always `Error`** — a result cannot precede the call that asked for it, so
//!   there is no live-session reading of that shape.
//! - **`is_healthy` must not mean "no issues"** — a log with only `Info` issues is healthy.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It names the crate through
//! `::girder_core`, which the library alias lets resolve from either side.

#[path = "../src/session_health.rs"]
mod session_health;

use girder_core::{ContentBlock, SessionEvent, ToolCallId, TurnEndReason};

use session_health::{
    ERROR_PENALTY, Issue, Kind, Severity, WARNING_PENALTY, diagnose, is_healthy, score,
};

// Re-exported by the crate root under names that say what is being diagnosed, so a caller need not
// import `session_health` (which is a private module) to reach it. Named here to pin that contract.
use girder_core::{diagnose_health as exported_diagnose, health_score as exported_score};

// ---- fixtures ----------------------------------------------------------------------------------

/// The tool call a log records when the model asks for a tool.
fn call(id: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(id).expect("valid id"),
        name: "bash".to_owned(),
        arguments: "{}".to_owned(),
    }
}

/// The result a log records when the tool answers.
fn result(id: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(id).expect("valid id"),
        content: vec![ContentBlock::Text {
            text: "ok".to_owned(),
        }],
        is_error: false,
    }
}

/// A turn's opening record.
fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

/// A turn's closing record.
fn turn_end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    }
}

/// A step's opening record.
fn step_start(turn: u64, step: u64) -> SessionEvent {
    SessionEvent::StepStart { turn, step }
}

/// A step's closing record.
fn step_end(turn: u64, step: u64) -> SessionEvent {
    SessionEvent::StepEnd { turn, step }
}

/// The one complete turn every fixture below is a variation on: prompt, model call, one tool call
/// answered, and both boundaries closed.
fn finished_turn(turn: u64, id: &str) -> Vec<SessionEvent> {
    vec![
        turn_start(turn),
        SessionEvent::user_text("m1", "hi"),
        step_start(turn, 1),
        call(id),
        result(id),
        SessionEvent::assistant_text("a1", "done", "test"),
        step_end(turn, 1),
        turn_end(turn),
    ]
}

/// An issue of `kind` at `at`, asserting there is exactly one and returning it.
///
/// The severity assertions live next to the kind assertions on purpose: a test that checked only the
/// kind would pass for the bug this module was written to avoid -- reporting the same shape at the
/// wrong severity.
fn only_issue_of(events: &[SessionEvent], kind: Kind, at: usize) -> Issue {
    let issues = diagnose(events);
    let matching: Vec<&Issue> = issues.iter().filter(|issue| issue.kind == kind).collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one {kind:?}, got {issues:#?}"
    );
    assert_eq!(
        matching[0].at, at,
        "reported at the wrong event: {issues:#?}"
    );
    matching[0].clone()
}

// ---- a healthy session -------------------------------------------------------------------------

#[test]
fn a_complete_turn_is_healthy_and_scores_full() {
    let events = finished_turn(1, "call-1");

    assert_eq!(diagnose(&events), vec![]);
    assert!(is_healthy(&events));
    assert_eq!(score(&events), 100);
}

#[test]
fn a_multi_turn_session_with_several_tools_is_healthy() {
    let mut events = finished_turn(1, "call-1");
    events.extend(finished_turn(2, "call-2"));

    assert_eq!(diagnose(&events), vec![]);
    assert_eq!(score(&events), 100);
}

#[test]
fn records_outside_any_turn_are_not_issues() {
    // A title and an accounting record belong to no turn, which several `SessionEvent::turn` answers
    // `None` for. They must not be read as an out-of-order boundary.
    let events = vec![
        SessionEvent::SessionTitle {
            title: "t".to_owned(),
            message_seqs: vec![],
            source: "human".to_owned(),
        },
        SessionEvent::ModelUsage {
            model: "test".to_owned(),
            input_tokens: 1,
            output_tokens: 1,
        },
    ];

    assert!(is_healthy(&events));
}

// ---- the core distinction: a tail is normal, a middle is damage --------------------------------

#[test]
fn an_unanswered_call_at_the_end_of_a_live_session_is_only_a_hint() {
    // The call is written before the tool runs, so a log whose *last* relevant record is the call is
    // the normal shape of a session waiting on a tool. Warning here would flag every live session.
    let events = vec![turn_start(1), step_start(1, 1), call("call-1")];

    let issue = only_issue_of(&events, Kind::UnansweredCall, 2);

    assert_eq!(issue.severity, Severity::Info);
    assert!(is_healthy(&events), "a waiting session is healthy");
    assert_eq!(score(&events), 100, "Info must not lower the score");
}

#[test]
fn an_unanswered_call_in_a_closed_turn_is_a_warning() {
    // Identical to the fixture above plus the `turn/end`: once the turn closed, no result can arrive,
    // which is what makes the same record a different verdict.
    let events = vec![
        turn_start(1),
        step_start(1, 1),
        call("call-1"),
        step_end(1, 1),
        turn_end(1),
    ];

    let issue = only_issue_of(&events, Kind::UnansweredCall, 2);

    assert_eq!(issue.severity, Severity::Warning);
    assert!(!is_healthy(&events));
    assert_eq!(score(&events), 100 - WARNING_PENALTY);
}

#[test]
fn an_unanswered_call_overtaken_by_the_next_turn_is_a_warning() {
    // No `turn/end` was written, but the next `turn/start` proves this turn is over. The issue points
    // at the call, not at the boundary that revealed it. Three warnings, because the abandoned turn
    // is reported too: the turn itself, the step it left open, and the call it never answered.
    let events = vec![
        turn_start(1),
        step_start(1, 1),
        call("call-1"),
        turn_start(2),
        step_start(2, 1),
        step_end(2, 1),
        turn_end(2),
    ];

    let issue = only_issue_of(&events, Kind::UnansweredCall, 2);

    assert_eq!(issue.severity, Severity::Warning);
    assert_eq!(score(&events), 100 - 3 * WARNING_PENALTY);
}

#[test]
fn an_orphan_result_is_always_an_error() {
    // No turn, no call: the smallest log that can hold this bug, so nothing else can explain the
    // severity.
    let events = vec![result("call-1")];

    let issue = only_issue_of(&events, Kind::OrphanResult, 0);

    assert_eq!(issue.severity, Severity::Error);
    assert!(!is_healthy(&events));
    assert_eq!(score(&events), 100 - ERROR_PENALTY);
}

#[test]
fn a_result_whose_call_was_already_answered_is_an_orphan() {
    // A duplicated result is the second half of the same failure: an id may be answered once, and a
    // replay that honoured both would show a tool having run twice.
    let events = vec![call("call-1"), result("call-1"), result("call-1")];

    let issue = only_issue_of(&events, Kind::OrphanResult, 2);

    assert_eq!(issue.severity, Severity::Error);
}

#[test]
fn an_orphan_result_inside_an_open_turn_is_still_an_error() {
    // The severity must not depend on the shape of the tail: whether the session is live is
    // irrelevant to a result that had no call.
    let events = vec![turn_start(1), step_start(1, 1), result("call-1")];

    let issue = only_issue_of(&events, Kind::OrphanResult, 2);

    assert_eq!(issue.severity, Severity::Error);
}

// ---- closure of turns and steps ---------------------------------------------------------------

#[test]
fn a_turn_still_open_at_the_end_is_a_hint() {
    let events = vec![turn_start(1), step_start(1, 1), step_end(1, 1)];

    let issue = only_issue_of(&events, Kind::UnclosedTurn, 0);

    assert_eq!(issue.severity, Severity::Info);
    assert!(is_healthy(&events));
}

#[test]
fn a_turn_overtaken_by_the_next_turn_is_a_warning() {
    let events = vec![turn_start(1), turn_start(2), turn_end(2)];

    let issue = only_issue_of(&events, Kind::UnclosedTurn, 0);

    assert_eq!(issue.severity, Severity::Warning);
    assert!(!is_healthy(&events));
}

#[test]
fn a_step_still_open_at_the_end_is_a_hint() {
    let events = vec![turn_start(1), step_start(1, 1)];

    let issue = only_issue_of(&events, Kind::UnclosedStep, 1);

    assert_eq!(issue.severity, Severity::Info);
    assert!(is_healthy(&events));
}

#[test]
fn a_step_overtaken_by_the_next_step_is_a_warning() {
    let events = vec![
        turn_start(1),
        step_start(1, 1),
        step_start(1, 2),
        step_end(1, 2),
        turn_end(1),
    ];

    let issue = only_issue_of(&events, Kind::UnclosedStep, 1);

    assert_eq!(issue.severity, Severity::Warning);
}

#[test]
fn a_step_open_when_its_turn_closes_is_a_warning() {
    // The turn's own end is proof that no `step/end` is coming, so the hint becomes a warning.
    let events = vec![turn_start(1), step_start(1, 1), turn_end(1)];

    let issue = only_issue_of(&events, Kind::UnclosedStep, 1);

    assert_eq!(issue.severity, Severity::Warning);
}

// ---- ordering ---------------------------------------------------------------------------------

#[test]
fn an_end_before_its_start_is_out_of_order() {
    // `event_index` deliberately tolerates a close naming another turn; a diagnosis must not, because
    // it is the record that says the log contradicts itself.
    let events = vec![turn_end(1), turn_start(1)];

    let issues = diagnose(&events);
    let out_of_order: Vec<&Issue> = issues
        .iter()
        .filter(|issue| issue.kind == Kind::OutOfOrder)
        .collect();

    assert_eq!(out_of_order.len(), 1, "{issues:#?}");
    assert_eq!(out_of_order[0].severity, Severity::Error);
    assert_eq!(out_of_order[0].at, 0);
    assert!(!is_healthy(&events));
}

#[test]
fn a_step_end_with_no_step_start_is_out_of_order() {
    let events = vec![turn_start(1), step_end(1, 1), turn_end(1)];

    let issue = only_issue_of(&events, Kind::OutOfOrder, 1);

    assert_eq!(issue.severity, Severity::Error);
}

#[test]
fn a_step_end_that_closes_the_wrong_step_is_out_of_order() {
    let events = vec![
        turn_start(1),
        step_start(1, 1),
        step_end(1, 2),
        step_end(1, 1),
        turn_end(1),
    ];

    let issue = only_issue_of(&events, Kind::OutOfOrder, 2);

    assert_eq!(issue.severity, Severity::Error);
}

// ---- is_healthy does not mean "no issues" -----------------------------------------------------

#[test]
fn is_healthy_ignores_every_info_issue() {
    // A live session mid-turn mid-step with a tool in flight: three issues, all hints.
    let events = vec![turn_start(1), step_start(1, 1), call("call-1")];

    let issues = diagnose(&events);

    assert_eq!(
        issues.len(),
        3,
        "unanswered call, open step, open turn: {issues:#?}"
    );
    assert!(issues.iter().all(|issue| issue.severity == Severity::Info));
    assert!(is_healthy(&events));
    assert_eq!(score(&events), 100);
}

#[test]
fn one_warning_makes_a_session_unhealthy() {
    let events = vec![turn_start(1), step_start(1, 1), call("call-1"), turn_end(1)];

    assert!(
        diagnose(&events)
            .iter()
            .any(|issue| issue.severity >= Severity::Warning)
    );
    assert!(!is_healthy(&events));
}

// ---- the score --------------------------------------------------------------------------------

#[test]
fn the_score_subtracts_one_weight_per_issue() {
    // Three orphan results at 25 each, and one of them also leaves the turn open, which is a hint and
    // therefore free.
    let events = vec![turn_start(1), result("a"), result("b"), result("c")];

    let issues = diagnose(&events);

    assert_eq!(
        issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
            .count(),
        3
    );
    assert_eq!(score(&events), 100 - 3 * ERROR_PENALTY);
    assert_eq!(score(&events), 25);
}

#[test]
fn the_score_never_goes_below_zero() {
    // Twenty errors would be -400; a ranking of broken sessions does not need the difference between
    // twenty and thirty of them, and a `u8` cannot hold a negative number.
    let events: Vec<SessionEvent> = (0..20).map(|i| result(&format!("call-{i}"))).collect();

    assert_eq!(score(&events), 0);
}

#[test]
fn the_score_orders_a_healthy_session_above_a_damaged_one() {
    let healthy = finished_turn(1, "call-1");
    let warned = vec![turn_start(1), step_start(1, 1), call("call-1"), turn_end(1)];
    let broken = vec![call("call-1"), result("other")];

    assert!(score(&healthy) > score(&warned));
    assert!(score(&warned) > score(&broken));
}

#[test]
fn the_penalties_are_ordered_and_info_is_free() {
    assert_eq!(Severity::Info.weight(), 0);
    assert!(Severity::Info.weight() < Severity::Warning.weight());
    assert!(Severity::Warning.weight() < Severity::Error.weight());
}

#[test]
fn the_names_are_stable_strings_not_debug_output() {
    // These end up in dashboards and metric labels, so they are spelled here rather than derived.
    assert_eq!(Kind::UnansweredCall.as_str(), "unanswered_call");
    assert_eq!(Kind::OrphanResult.as_str(), "orphan_result");
    assert_eq!(Kind::UnclosedTurn.as_str(), "unclosed_turn");
    assert_eq!(Kind::UnclosedStep.as_str(), "unclosed_step");
    assert_eq!(Kind::OutOfOrder.as_str(), "out_of_order");

    assert_eq!(Severity::Info.as_str(), "info");
    assert_eq!(Severity::Warning.as_str(), "warning");
    assert_eq!(Severity::Error.as_str(), "error");
}

#[test]
fn the_crate_level_reexports_answer_the_same_as_the_module() {
    // The deployment-facing path is `girder_core::diagnose_health`, so the two must not drift apart.
    let events = vec![turn_start(1), call("call-1"), step_end(1, 1), turn_end(1)];

    // The file is compiled twice -- once as a library module, once as this test's module -- so the
    // two `Issue` types are distinct and are compared field by field rather than by identity.
    let via_crate = exported_diagnose(&events);
    let via_module = diagnose(&events);
    assert_eq!(via_crate.len(), via_module.len());
    for (left, right) in via_crate.iter().zip(via_module.iter()) {
        assert_eq!(left.at, right.at);
        assert_eq!(left.kind.as_str(), right.kind.as_str());
        assert_eq!(left.severity.as_str(), right.severity.as_str());
        assert_eq!(left.detail, right.detail);
    }

    assert_eq!(exported_score(&events), score(&events));
    assert_eq!(girder_core::is_healthy(&events), is_healthy(&events));
}

// ---- empty and multiple problems --------------------------------------------------------------

#[test]
fn an_empty_log_is_healthy_and_scores_full() {
    // A session created but never prompted: `SessionStore::touch` makes exactly this file, and it is
    // not a damaged one.
    assert_eq!(diagnose(&[]), vec![]);
    assert!(is_healthy(&[]));
    assert_eq!(score(&[]), 100);
}

#[test]
fn several_problems_are_all_reported_in_log_order() {
    // A log that lost its tail and had a call spliced on: an unclosed turn overtaken by the next
    // one, an unanswered call in a closed turn, and an orphan result.
    let events = vec![
        turn_start(1),
        step_start(1, 1),
        call("call-1"),
        turn_start(2),
        step_start(2, 1),
        result("call-2"),
        step_end(2, 1),
        turn_end(2),
    ];

    let issues = diagnose(&events);
    let kinds: Vec<(usize, Kind)> = issues.iter().map(|issue| (issue.at, issue.kind)).collect();

    assert_eq!(
        kinds,
        vec![
            (0, Kind::UnclosedTurn),
            (1, Kind::UnclosedStep),
            (2, Kind::UnansweredCall),
            (5, Kind::OrphanResult),
        ],
        "{issues:#?}"
    );
    // Positions are non-decreasing, which is what makes the list readable as a log walk.
    assert!(issues.windows(2).all(|pair| pair[0].at <= pair[1].at));
    // Three warnings -- the abandoned turn, the step it left open, and the call that turn never
    // answered -- plus the orphan result.
    assert_eq!(score(&events), 100 - 3 * WARNING_PENALTY - ERROR_PENALTY);
    assert_eq!(score(&events), 60);
}

#[test]
fn duplicate_ids_are_reported_once_each_and_survive_as_an_orphan_check() {
    // One id used twice: the second call is a warning on its own, and its result answers only one of
    // the two, so the other stays unanswered rather than both being silently satisfied.
    let events = vec![call("same"), call("same"), result("same")];

    let issues = diagnose(&events);
    let unanswered = issues
        .iter()
        .filter(|issue| issue.kind == Kind::UnansweredCall)
        .count();

    assert_eq!(unanswered, 1, "{issues:#?}");
    assert!(
        issues
            .iter()
            .any(|issue| issue.kind == Kind::OutOfOrder && issue.severity == Severity::Warning)
    );
}
