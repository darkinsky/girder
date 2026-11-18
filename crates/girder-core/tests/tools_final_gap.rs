//! Integration tests for `tools_final_gap`: the decision layer end to end.
//!
//! The module's own unit tests pin each function. This file pins the *sequence*, because the layer's
//! job is a sequence and its bugs live in the joins: a call gates, dispatches, gets post-executed, is
//! settled into the report, and reaches the transcript. Each of those steps has been ported from a
//! different upstream file — `pipeline.ts`, `batch.ts`, `index.ts` — so a bug at a boundary would
//! pass every unit test and still hand the model the wrong transcript.
//!
//! Four joins are load-bearing enough to be asserted here specifically:
//!
//! - **approval → gate → outcome.** The approval seam is data here, so all four answers can be driven
//!   without a bus. What must survive is that the three non-grants are distinguishable *after*
//!   materialization, not just before it.
//! - **cancellation at the three points it can land.** Before the gate, during approval, and after
//!   the body. Each has a different correct identity, and reading the wrong one tells the model that
//!   a call with side effects provably did not run.
//! - **post-execute → settlement.** A blocked result must reach the transcript as an error with no
//!   value, and it must not conclude the turn.
//! - **skip synthesis.** An aborted batch must still answer every `tool_use`, in model order.
//!
//! The module is included by path because the crate root does not declare it yet: every path into
//! the crate goes through `girder_core::…` for that reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::items_after_statements
)]

#[path = "../src/tools_final_gap.rs"]
mod tools_final_gap;

use girder_core::{ChatMessage, ContentBlock, MessageId};
use serde_json::json;
use tools_final_gap::{
    APPROVAL_CANCELLED, APPROVAL_ERROR_NAME, APPROVAL_REJECTED, APPROVAL_UNAVAILABLE,
    ApprovalOutcome, AskChannel, Callback, DEFAULT_MAX_PARALLEL, ErrorInfo, Fallback, Gate, Mode,
    Outcome, PostDecision, Settlement, TOOL_ABORTED, TOOL_ABORTED_BEFORE_DISPATCH, ToolFailure,
    UNKNOWN_TOOL, aborted_before_dispatch_outcome, admits_through, apply_post_decision,
    approval_denial, cancellation_outcome, compile_restriction, denial_outcome, execution_mode,
    finalize_after_cancellation, group_end, parse_arguments, plan_groups, resolve_ask,
    resolve_max_parallel,
};

/// The visible text of an outcome, concatenated.
fn text_of(outcome: &Outcome) -> String {
    outcome
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The error code an outcome carries.
fn code_of(outcome: &Outcome) -> &str {
    outcome
        .error
        .as_ref()
        .and_then(|failure| failure.info.as_ref())
        .map_or("", |info| info.code.as_str())
}

fn note(id: &str) -> ChatMessage {
    ChatMessage::user(MessageId::new(id).expect("valid id"), "context")
}

// ---- the approval join -------------------------------------------------------------------------

#[test]
fn every_approval_answer_survives_transformation_into_a_model_facing_outcome() {
    // The point of the identity is that it is still there after materialization: a client that only
    // sees the rendered text cannot tell a rejection from an absent channel.
    let cases = [
        (
            ApprovalOutcome::Rejected,
            APPROVAL_REJECTED,
            "the user rejected tool \"shell\"",
        ),
        (
            ApprovalOutcome::Cancelled,
            APPROVAL_CANCELLED,
            "approval for tool \"shell\" was cancelled",
        ),
        (
            ApprovalOutcome::Unavailable,
            APPROVAL_UNAVAILABLE,
            "tool \"shell\" requires approval, but no approval channel is available",
        ),
    ];

    for (answer, expected_code, expected_text) in cases {
        let resolved = resolve_ask("shell", None, AskChannel::Answered(answer));
        let outcome = denial_outcome(&resolved.gate).expect("a non-grant materializes");

        assert!(outcome.is_error, "{answer:?} must fail");
        assert_eq!(code_of(&outcome), expected_code, "{answer:?}");
        assert_eq!(
            outcome.error.as_ref().unwrap().info.as_ref().unwrap().name,
            APPROVAL_ERROR_NAME
        );
        assert_eq!(text_of(&outcome), format!("Error: {expected_text}"));
        assert_eq!(outcome.value, None, "a denial never carries a value");
    }
}

#[test]
fn a_granted_call_is_the_only_answer_that_produces_no_denial() {
    let granted = resolve_ask(
        "shell",
        None,
        AskChannel::Answered(ApprovalOutcome::AllowedOnce),
    );
    assert!(granted.gate.allows());
    assert!(denial_outcome(&granted.gate).is_none());
}

#[test]
fn a_cancelled_approval_is_the_one_non_grant_that_means_the_body_never_ran() {
    // This flag is what selects the abort identity downstream, and getting it wrong tells the model
    // a call that did run provably did not.
    let cancelled = resolve_ask(
        "shell",
        None,
        AskChannel::Answered(ApprovalOutcome::Cancelled),
    );
    assert!(cancelled.approval_cancelled);

    for answer in [
        ApprovalOutcome::Rejected,
        ApprovalOutcome::Unavailable,
        ApprovalOutcome::AllowedOnce,
    ] {
        let resolved = resolve_ask("shell", None, AskChannel::Answered(answer));
        assert!(!resolved.approval_cancelled, "{answer:?}");
    }
}

// ---- cancellation at its three landing points --------------------------------------------------

#[test]
fn cancellation_before_the_gate_never_reaches_the_body() {
    // Nothing ran, so the canonical result names the pre-dispatch identity.
    let outcome = cancellation_outcome(false, None);
    assert_eq!(code_of(&outcome), TOOL_ABORTED_BEFORE_DISPATCH);
    assert_eq!(
        text_of(&outcome),
        "Error: tool call aborted before dispatch"
    );
}

#[test]
fn cancellation_during_dispatch_still_reports_an_abort() {
    // The body was invoked: the side effect may be partial, so the identity must not claim otherwise.
    let outcome = cancellation_outcome(true, None);
    assert_eq!(code_of(&outcome), TOOL_ABORTED);
}

#[test]
fn a_cancelled_call_that_also_failed_keeps_the_tools_own_error() {
    // The single most consequential rule in this layer: the tool's words are evidence, and the abort
    // would erase them.
    let failed = Outcome::failure(ToolFailure::identified(
        "permission denied on /etc/shadow",
        ErrorInfo::new("IoError", "EACCES"),
    ));
    let finalized = finalize_after_cancellation(true, true, failed);

    assert_eq!(
        text_of(&finalized),
        "Error: permission denied on /etc/shadow"
    );
    assert_eq!(code_of(&finalized), "EACCES");
    assert_ne!(code_of(&finalized), TOOL_ABORTED);
}

#[test]
fn an_abort_keeps_context_that_work_already_produced() {
    // A nested dispatch's context describes a call that did happen; dropping it on cancellation would
    // hide exactly the information a reader needs about a partial batch.
    let prior = Outcome::text("partial").with_contexts(vec![note("nested-dispatch")]);
    let aborted = finalize_after_cancellation(true, true, prior);

    assert!(aborted.is_error);
    assert_eq!(aborted.additional_contexts.len(), 1);
    assert_eq!(
        aborted.additional_contexts[0].id.to_string(),
        "nested-dispatch"
    );
}

// ---- the post-execute join ---------------------------------------------------------------------

#[test]
fn a_blocked_result_reaches_the_settlement_as_a_valueless_error() {
    let mut settlement = Settlement::new(1);
    let result = Outcome::success(
        vec![ContentBlock::Text { text: "raw".into() }],
        Some(json!({"rows": 3})),
    );
    let blocked = apply_post_decision(
        result,
        PostDecision::Block {
            feedback: vec![ContentBlock::Text {
                text: "quote the path".into(),
            }],
            info: Some(ErrorInfo::new("PolicyError", "NEEDS_QUOTE")),
            additional_contexts: vec![note("policy-comment")],
        },
        |_| Vec::new(),
    )
    .expect("block applies");

    assert!(settlement.commit(0, "call-1", blocked));
    let report = settlement.report();
    let committed = &report.results[0].outcome;

    assert!(committed.is_error);
    assert_eq!(committed.value, None, "a blocked result keeps no value");
    assert_eq!(committed.error.as_ref().unwrap().message, "quote the path");
    assert_eq!(code_of(committed), "NEEDS_QUOTE");
    // The policy's context reaches the transcript; the body's (there was none) would not have.
    assert_eq!(report.additional_contexts.len(), 1);
    assert!(!report.concluded, "a block must not end the turn");
}

#[test]
fn a_value_replacement_projects_through_the_tools_renderer() {
    let result = Outcome::success(
        vec![ContentBlock::Text { text: "raw".into() }],
        Some(json!({"n": 1})),
    );
    let applied = apply_post_decision(
        result,
        PostDecision::Accept {
            content: None,
            value: Some(json!({"n": 7})),
            additional_contexts: Vec::new(),
        },
        |value| {
            vec![ContentBlock::Text {
                text: format!("n is {}", value["n"]),
            }]
        },
    )
    .expect("accept applies");

    assert_eq!(text_of(&applied), "n is 7");
    assert_eq!(applied.value, Some(json!({"n": 7})));
}

// ---- the scheduling join -----------------------------------------------------------------------

#[test]
fn a_batch_of_reads_and_a_write_plans_as_parallel_run_then_barrier() {
    // The common real shape: several reads overlap, and the write after them is a barrier.
    let modes = [Mode::Parallel, Mode::Parallel, Mode::Exclusive];
    assert_eq!(group_end(&modes, 0), 2);
    assert_eq!(plan_groups(&modes), vec![(0, 2), (2, 1)]);
}

#[test]
fn an_override_cap_applies_to_the_planned_groups_without_changing_them() {
    // The cap bounds how many calls inside one group run at once; it does not re-cut the groups, and
    // a group larger than the cap still completes.
    let modes = [Mode::Parallel; 5];
    assert_eq!(plan_groups(&modes), vec![(0, 5)]);
    assert_eq!(resolve_max_parallel(Some(2), DEFAULT_MAX_PARALLEL), Ok(2));
}

#[test]
fn arguments_that_do_not_parse_still_reach_the_tool_for_its_own_diagnostic() {
    // The scheduler must not turn a model typo into a generic parse error; the waiting tool has the
    // better message.
    let parsed = parse_arguments("{oops");
    assert_eq!(parsed, serde_json::Value::String(String::from("{oops")));
}

// ---- the abort-and-settle join ------------------------------------------------------------------

#[test]
fn an_aborted_batch_answers_every_call_in_model_order() {
    let calls = vec![
        String::from("call-a"),
        String::from("call-b"),
        String::from("call-c"),
    ];
    let mut settlement = Settlement::new(calls.len());
    settlement.commit(0, "call-a", Outcome::text("ran"));
    // The batch aborts here; every remaining call still needs an answer.
    assert_eq!(settlement.skip_from(1, &calls), 2);

    let report = settlement.report();
    assert!(report.is_complete_for(3));
    let ids: Vec<&str> = report
        .results
        .iter()
        .map(|call| call.call_id.as_str())
        .collect();
    assert_eq!(ids, vec!["call-a", "call-b", "call-c"]);

    for skipped in &report.results[1..] {
        assert_eq!(code_of(&skipped.outcome), TOOL_ABORTED_BEFORE_DISPATCH);
    }
}

#[test]
fn a_skipped_call_is_synthesized_even_when_the_batch_never_started() {
    let mut settlement = Settlement::new(1);
    assert!(settlement.skip(0, "call-a"));
    let report = settlement.report();
    assert!(report.is_complete_for(1));
    assert_eq!(
        report.results[0].outcome,
        aborted_before_dispatch_outcome(None)
    );
}

// ---- the restriction join ----------------------------------------------------------------------

#[test]
fn two_restrictions_that_together_exclude_everything_admit_nothing() {
    let known = ["read_file".to_owned(), "shell".to_owned()];
    let allow_read = compile_restriction(Some(&["read_file"]), None, &known).expect("valid");
    let allow_shell = compile_restriction(Some(&["shell"]), None, &known).expect("valid");

    // Intersection, not union: each layer had the authority to remove what it named.
    assert!(!admits_through(
        &[allow_read.clone(), allow_shell.clone()],
        "read_file"
    ));
    assert!(!admits_through(&[allow_read, allow_shell], "shell"));
}

#[test]
fn a_restriction_does_not_silently_change_the_classification_of_a_masked_tool() {
    // A tool hidden by a restriction does not resolve, so it is exclusive — the fail-closed
    // direction. This pins that the two decisions agree about an invisible name.
    let known = ["read_file".to_owned()];
    let restriction = compile_restriction(None, Some(&["read_file"]), &known).expect("valid");

    assert!(!admits_through(&[restriction], "read_file"));
    assert_eq!(execution_mode(false, false, Some(true)), Mode::Exclusive);
}

// ---- the definition join -----------------------------------------------------------------------

#[test]
fn a_definition_that_validates_arguments_only_raises_in_its_body() {
    // A presenter or classifier that raised on obsolete arguments would make replay of an older
    // session fail for a display reason.
    for callback in [Callback::PresentCall, Callback::PresentResult] {
        assert!(!callback.rejects_invalid_arguments());
        assert_eq!(callback.fallback_on_invalid_arguments(), Fallback::Omit);
    }
    assert_eq!(
        Callback::IsConcurrencySafe.fallback_on_invalid_arguments(),
        Fallback::Exclusive
    );
    assert_eq!(
        Callback::Execute.fallback_on_invalid_arguments(),
        Fallback::Raise
    );
}

// ---- identities are the persisted contract ------------------------------------------------------

#[test]
fn the_persisted_error_codes_are_the_strings_upstream_records() {
    // These strings land in session logs and are compared by consumers across versions, so a rename
    // is a migration rather than a refactor.
    assert_eq!(TOOL_ABORTED, "ABORTED");
    assert_eq!(TOOL_ABORTED_BEFORE_DISPATCH, "ABORTED_BEFORE_DISPATCH");
    assert_eq!(UNKNOWN_TOOL, "UNKNOWN_TOOL");
    assert_eq!(APPROVAL_ERROR_NAME, "ApprovalError");
    assert_eq!(APPROVAL_REJECTED, "APPROVAL_REJECTED");
    assert_eq!(APPROVAL_CANCELLED, "APPROVAL_CANCELLED");
    assert_eq!(APPROVAL_UNAVAILABLE, "APPROVAL_UNAVAILABLE");
}

#[test]
fn an_approval_denial_built_directly_matches_the_one_from_the_seam() {
    let direct = approval_denial(
        "the user rejected tool \"shell\"",
        tools_final_gap::ApprovalCode::Rejected,
    );
    let from_seam = resolve_ask(
        "shell",
        None,
        AskChannel::Answered(ApprovalOutcome::Rejected),
    );
    assert_eq!(direct, from_seam.gate);

    // An `ask` gate on its own has no denial: it is a question, not an answer.
    assert!(denial_outcome(&Gate::Ask { reason: None }).is_none());
}
