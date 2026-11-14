//! Tests for deciding whether a crash-interrupted tool call may run again.
//!
//! The properties pinned here are the ones a recovery path gets wrong, and the first two are the
//! reason the module exists:
//!
//! - **A call with a result is `Skip`, never `Run`.** `agent_loop` records the call before it
//!   dispatches and the result after it returns, so a result is proof the body ran. Re-running a
//!   `write` that already happened is the failure that cannot be undone.
//! - **A call without a result is `Confirm` for anything unsafe.** The absence of a result is the
//!   absence of an observation, not evidence that nothing happened — the crash lands between the
//!   side effect and the append. Only a tool known to be read-only may be retried silently.
//! - **A read is retried, even on the same call id.** Asking a human to re-authorise `read` would
//!   make the conservative default expensive enough that callers would bypass it.
//! - **Arguments are part of the identity.** One id with two argument sets is two calls; a module
//!   that matched on the id alone would let the first answer the second.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It names the crate through
//! `::girder_core`, which the library alias lets resolve from either side.

#[path = "../src/tool_idempotency.rs"]
mod tool_idempotency;

use girder_core::{ContentBlock, SessionEvent, ToolCallId};

use tool_idempotency::{
    Decision, Executed, Idempotency, decide, executed, fingerprint, idempotency,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// The record `agent_loop` writes before it dispatches a call.
fn call(id: &str, tool: &str, arguments: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(id).expect("valid id"),
        name: tool.to_owned(),
        arguments: arguments.to_owned(),
    }
}

/// The record `agent_loop` writes once a call returns.
fn result(id: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(id).expect("valid id"),
        content: vec![ContentBlock::Text {
            text: "ok".to_owned(),
        }],
        is_error: false,
    }
}

/// The argument text a `write`-shaped call carries.
const WRITE_ARGS: &str = r#"{"path":"a.rs","content":"x"}"#;

/// The fingerprint of a `write`-shaped call, built through the module so the test never re-encodes
/// by hand and cannot drift from the implementation.
fn write_fingerprint() -> String {
    fingerprint("write", WRITE_ARGS)
}

/// The fingerprint of a `read`-shaped call.
fn read_fingerprint(arguments: &str) -> String {
    fingerprint("read", arguments)
}

/// Asserts a decision is `Run`, printing the decision on failure.
///
/// Taken by reference so a failing assertion can still print what it got: the value is the whole
/// diagnostic when a recovery path answers the wrong question.
fn assert_run(decision: &Decision) {
    assert_eq!(decision, &Decision::Run, "expected Run, got {decision:?}");
}

/// Asserts a decision is `Skip` and returns its reason, so a test can check the reason is not empty.
fn assert_skip(decision: &Decision) -> String {
    match decision {
        Decision::Skip { reason } => {
            assert!(!reason.is_empty(), "Skip must explain itself");
            reason.clone()
        }
        other => panic!("expected Skip, got {other:?}"),
    }
}

/// Asserts a decision is `Confirm` and returns its reason.
fn assert_confirm(decision: &Decision) -> String {
    match decision {
        Decision::Confirm { reason } => {
            assert!(!reason.is_empty(), "Confirm must explain itself");
            reason.clone()
        }
        other => panic!("expected Confirm, got {other:?}"),
    }
}

// ---- the first run -----------------------------------------------------------------------------

#[test]
fn an_unseen_call_runs() {
    let events: Vec<SessionEvent> = vec![call("call-1", "write", WRITE_ARGS)];

    // No result, so this is the crash-in-the-middle shape only if the call was already dispatched;
    // here the caller is asking about a call it is about to make against a log that does not know it.
    assert_run(&decide(&events, "write", "call-2", &write_fingerprint()));
}

#[test]
fn an_empty_log_runs_everything() {
    let events: Vec<SessionEvent> = vec![];

    assert_run(&decide(&events, "write", "call-1", &write_fingerprint()));
    assert_run(&decide(&events, "bash", "call-2", "anything"));
    assert_run(&decide(&events, "read", "call-3", &read_fingerprint("{}")));
    assert_eq!(executed(&events), vec![]);
}

// ---- the core: a call that already has a result is not re-run ----------------------------------

#[test]
fn a_call_that_already_has_a_result_is_skipped() {
    let events = vec![call("call-1", "write", WRITE_ARGS), result("call-1")];

    let reason = assert_skip(&decide(&events, "write", "call-1", &write_fingerprint()));
    // The reason must name the call, because the caller's next action is to report which call it
    // declined to repeat.
    assert!(
        reason.contains("call-1"),
        "reason should name the call: {reason}"
    );
}

#[test]
fn a_read_with_a_result_is_also_skipped() {
    // Skip is about the observation existing, not about safety: a result already answers the
    // question, and a second read could answer it differently after an edit.
    let events = vec![call("call-1", "read", "{}"), result("call-1")];

    assert_skip(&decide(&events, "read", "call-1", &read_fingerprint("{}")));
}

#[test]
fn an_unknown_tool_with_a_result_is_skipped() {
    // The result is evidence regardless of classification. A module that consulted only the
    // idempotency table would return Confirm here and ask about a call that plainly finished.
    let events = vec![call("call-1", "lsp", "{}"), result("call-1")];

    assert_skip(&decide(&events, "lsp", "call-1", &fingerprint("lsp", "{}")));
}

#[test]
fn a_result_without_its_call_is_still_evidence_of_execution() {
    // A log can lose the call record (a splice, a damaged tail). The call is written before the body
    // runs, so a result is surer evidence than the missing record; the only safe reading is that the
    // call may have run.
    let events = vec![result("call-1")];

    let reason = assert_skip(&decide(&events, "write", "call-1", &write_fingerprint()));
    assert!(
        reason.contains("no recorded call"),
        "reason should say the call is missing: {reason}"
    );
}

// ---- the other core: an unsafe retry needs a human ----------------------------------------------

#[test]
fn an_unsafe_call_without_a_result_needs_confirmation() {
    let events = vec![call("call-1", "write", WRITE_ARGS)];

    let reason = assert_confirm(&decide(&events, "write", "call-1", &write_fingerprint()));
    assert!(
        reason.contains("write") && reason.contains("call-1"),
        "reason should name the tool and the call: {reason}"
    );
}

#[test]
fn every_unsafe_tool_needs_confirmation_on_a_retry() {
    for tool in ["write", "edit", "bash"] {
        let events = vec![call("call-1", tool, "{}")];

        assert_confirm(&decide(&events, tool, "call-1", &fingerprint(tool, "{}")));
    }
}

#[test]
fn a_retry_that_ran_several_times_still_needs_confirmation() {
    // Two dispatches and still no result: the second attempt did not observe the first, which is the
    // shape of a crash, a restart, and a retry loop that is about to happen a third time.
    let events = vec![
        call("call-1", "write", WRITE_ARGS),
        call("call-1", "write", WRITE_ARGS),
    ];

    let reason = assert_confirm(&decide(&events, "write", "call-1", &write_fingerprint()));
    assert!(
        reason.contains('2'),
        "reason should report the number of attempts: {reason}"
    );
}

#[test]
fn an_unknown_tool_without_a_result_needs_confirmation() {
    // Unknown is conservative: a tool this crate has never classified could be a writer, so it is
    // treated as one.
    let events = vec![call("call-1", "web_fetch", "{}")];

    let reason = assert_confirm(&decide(
        &events,
        "web_fetch",
        "call-1",
        &fingerprint("web_fetch", "{}"),
    ));
    assert!(
        reason.contains("not known"),
        "reason should say the tool is unclassified: {reason}"
    );
}

// ---- idempotent tools retry silently -----------------------------------------------------------

#[test]
fn a_safe_call_without_a_result_runs_again() {
    let events = vec![call("call-1", "read", "{}")];

    assert_run(&decide(&events, "read", "call-1", &read_fingerprint("{}")));
}

#[test]
fn every_safe_tool_runs_again_without_a_result() {
    for tool in ["read", "glob", "grep"] {
        let events = vec![call("call-1", tool, "{}")];

        assert_run(&decide(&events, tool, "call-1", &fingerprint(tool, "{}")));
    }
}

// ---- the whitelist -----------------------------------------------------------------------------

#[test]
fn the_idempotency_whitelist_is_exactly_what_it_claims() {
    for tool in ["read", "glob", "grep"] {
        assert_eq!(
            idempotency(tool),
            Idempotency::Safe,
            "{tool} should be Safe"
        );
    }
    for tool in ["write", "edit", "bash"] {
        assert_eq!(
            idempotency(tool),
            Idempotency::Unsafe,
            "{tool} should be Unsafe"
        );
    }
}

#[test]
fn unknown_tools_are_unknown() {
    // A new tool, a near-miss spelling, and an empty name all land on the conservative default.
    for tool in [
        "",
        "Read",
        "read_file",
        "apply_patch",
        "web_search",
        "subagent",
    ] {
        assert_eq!(
            idempotency(tool),
            Idempotency::Unknown,
            "{tool:?} should be Unknown"
        );
    }
}

// ---- fingerprints ------------------------------------------------------------------------------

#[test]
fn different_arguments_are_a_different_call() {
    let first = r#"{"path":"a.rs"}"#;
    let second = r#"{"path":"b.rs"}"#;
    assert_ne!(fingerprint("write", first), fingerprint("write", second));

    // Same id, different arguments: the earlier run does not answer this one, and `write` is unsafe,
    // so a human is asked rather than the call being skipped or silently repeated.
    let events = vec![call("call-1", "write", first), result("call-1")];
    assert_confirm(&decide(
        &events,
        "write",
        "call-1",
        &fingerprint("write", second),
    ));
}

#[test]
fn different_arguments_for_a_safe_tool_run_again() {
    let events = vec![
        call("call-1", "read", r#"{"path":"a.rs"}"#),
        result("call-1"),
    ];

    assert_run(&decide(
        &events,
        "read",
        "call-1",
        &fingerprint("read", r#"{"path":"b.rs"}"#),
    ));
}

#[test]
fn a_fingerprint_survives_a_call_under_a_different_id() {
    // Two ids with identical arguments are two executions of the same call: the log gave each its own
    // identity, and both ran. Recorded as separate entries rather than merged by argument text.
    let events = vec![
        call("call-1", "write", WRITE_ARGS),
        result("call-1"),
        call("call-2", "write", WRITE_ARGS),
        result("call-2"),
    ];

    let records = executed(&events);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].fingerprint, records[1].fingerprint);
    assert_skip(&decide(&events, "write", "call-2", &write_fingerprint()));
}

#[test]
fn tool_name_and_arguments_cannot_be_confused_across_the_boundary() {
    // Without length prefixes, `read` + `{"path":"a"}` and `read{"path"}` + `"a"` would encode
    // identically and one call could answer the other.
    assert_ne!(
        fingerprint("read", r#"{"path":"a"}"#),
        fingerprint(r#"read{"path"}"#, r#""a""#)
    );
    // The empty argument string of a tool whose name ends in the same characters must differ too.
    assert_ne!(fingerprint("a", "b"), fingerprint("ab", ""));
}

// ---- rebuilding the record ---------------------------------------------------------------------

#[test]
fn executed_reports_one_record_per_call_with_its_attempts() {
    let events = vec![
        call("call-1", "read", "{}"),
        result("call-1"),
        call("call-2", "bash", "ls"),
        call("call-2", "bash", "ls"),
        call("call-3", "write", WRITE_ARGS),
    ];

    assert_eq!(
        executed(&events),
        vec![
            Executed {
                call_id: "call-1".to_owned(),
                tool: "read".to_owned(),
                fingerprint: fingerprint("read", "{}"),
                attempts: 1,
            },
            Executed {
                call_id: "call-2".to_owned(),
                tool: "bash".to_owned(),
                fingerprint: fingerprint("bash", "ls"),
                attempts: 2,
            },
            Executed {
                call_id: "call-3".to_owned(),
                tool: "write".to_owned(),
                fingerprint: write_fingerprint(),
                attempts: 1,
            },
        ]
    );
}

#[test]
fn executed_keeps_log_order_and_ignores_other_events() {
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        call("call-2", "read", "{}"),
        SessionEvent::user_text("m1", "hi"),
        result("call-2"),
        call("call-1", "read", "{}"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: girder_core::TurnEndReason::Completed,
        },
    ];

    let records = executed(&events);
    let ids: Vec<&str> = records
        .iter()
        .map(|record| record.call_id.as_str())
        .collect();
    assert_eq!(ids, vec!["call-2", "call-1"]);
}

#[test]
fn executed_skips_a_result_that_has_no_call() {
    // No name and no arguments exist to build a record from, so the orphan is not reported here. The
    // decision path still honours it: the mixed fixture below has both shapes.
    let events = vec![result("orphan")];

    assert_eq!(executed(&events), vec![]);
}

#[test]
fn attempts_count_every_dispatch() {
    // A count, not a flag: "dispatched twice, answered never" and "dispatched once, answered never"
    // are different amounts of risk, and the reason a human reads should be able to say which.
    let events: Vec<SessionEvent> = std::iter::repeat_n(call("call-1", "bash", "ls"), 3).collect();

    assert_eq!(executed(&events)[0].attempts, 3);
}

#[test]
fn a_result_after_repeated_calls_still_skips() {
    // The interesting mixed case: dispatched twice, answered once. The answer proves at least one
    // attempt ran, and a retry would repeat whatever both of them did.
    let events = vec![
        call("call-1", "bash", "ls"),
        call("call-1", "bash", "ls"),
        result("call-1"),
    ];

    assert_skip(&decide(
        &events,
        "bash",
        "call-1",
        &fingerprint("bash", "ls"),
    ));
    assert_eq!(executed(&events)[0].attempts, 2);
}
