//! Tests for the Codex app-server wire protocol state machine.
//!
//! Upstream `packages/subagent/subagent-codex/src/wire.ts` is 711 lines and has no local counterpart
//! at all: the crate can register a provider ([`girder_core::ProviderRegistry`]) and run an
//! in-process child, but nothing can interpret the frames an out-of-process CLI driver receives.
//! The properties under test, in the order they matter:
//!
//! - **association is checked, not assumed** — a peer can name any thread or turn id, and a frame
//!   that names someone else's must not be folded into this run's answer. Frames for another
//!   *thread* are ignored; a *server request* for another thread or turn is rejected, because
//!   answering it would be acting on a different run's behalf.
//! - **a premature frame is buffered, not rejected** — the server may announce its turn and finish
//!   an item before `turn/start` returns. Those frames are held and replayed in arrival order, and
//!   the diagnostic they carry keeps its original rank against later ones.
//! - **unattended means least authority** — every interactive request is answered, because an
//!   unanswered request blocks the turn, and each answer grants strictly less than the request.
//!   `cancel` outranks `decline` when both are offered; a request that offers neither is refused
//!   rather than answered with a guess.
//! - **the answer rule is upstream's** — `final_answer`, else `partial_answer`, else unphased;
//!   commentary never wins; whitespace-only text is no answer; non-empty bytes are preserved exactly.
//! - **a ceiling is not a failure and a failure is not a ceiling** — a context-window error keeps the
//!   text observed so far and is reported as a max-tokens ending, while a sandbox refusal records an
//!   access-policy fact *and* a diagnostic, and a completed turn with no answer is an invalid result.
//! - **the permission table is exact** — including which keys are absent, because `never` must not
//!   set a sandbox and only `approve-for-me` names a reviewer.
//!
//! Isolation: every test builds its own wire; nothing is global.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the wire as written even before the crate root wires it in. It spells its paths through
//! `girder_core::`, which is the alias the library declares for itself, so the same source builds
//! here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/subagent_deep_gap.rs"]
mod subagent_deep_gap;

use serde_json::{Value, json};

use girder_core::ContentBlock;
use subagent_deep_gap::{
    CodexFailureCategory, CodexFailureStage, CodexPermissionMode, CodexTurnOutcome,
    CodexUnattendedDecision, CodexUnattendedRequest, CodexWire, CodexWireError, failure_info,
    unattended_diagnostic,
};

/// The five server-request methods the driver implements, with no run ids attached.
const APPROVAL_METHODS: [&str; 5] = [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
];

fn object(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("expected an object, got {other}"),
    }
}

/// A wire that has adopted a thread but not started a turn.
fn connected() -> CodexWire {
    let mut wire = CodexWire::new(CodexPermissionMode::ApproveForMe, None);
    wire.accept_thread(&json!({ "thread": { "id": "t-1", "ephemeral": true } }))
        .expect("an ephemeral thread is adopted");
    wire
}

/// A wire with a committed turn, ready to receive item frames.
fn running() -> CodexWire {
    let mut wire = connected();
    wire.begin_turn();
    wire.commit_turn(&json!({ "turn": { "id": "turn-1" } }))
        .expect("the turn is committed");
    wire
}

/// Posts one `agentMessage` item through the notification path.
fn post_message(wire: &mut CodexWire, text: &str, phase: &Value) {
    wire.on_notification(
        "item/completed",
        &object(json!({
            "threadId": "t-1",
            "turnId": "turn-1",
            "item": { "type": "agentMessage", "text": text, "phase": phase.clone() },
        })),
    )
    .expect("an item for this turn is accepted");
}

/// Posts one terminal `turn/completed` notification.
fn post_terminal(wire: &mut CodexWire, status: &str, error: Option<Value>) {
    let mut turn = json!({ "id": "turn-1", "status": status });
    if let Some(error) = error {
        turn["error"] = error;
    }
    wire.on_notification(
        "turn/completed",
        &object(json!({ "threadId": "t-1", "turn": turn })),
    )
    .expect("a terminal notification is accepted");
}

/// The answer text the wire selected, if any.
fn answer(wire: &CodexWire) -> Option<String> {
    match wire.collect_output().as_slice() {
        [] => None,
        [ContentBlock::Text { text }] => Some(text.clone()),
        other => panic!("expected at most one text block, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Association: thread and turn identity.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_message_for_another_thread_never_becomes_the_answer() {
    let mut wire = running();
    wire.on_notification(
        "item/completed",
        &object(json!({
            "threadId": "someone-else",
            "turnId": "turn-1",
            "item": { "type": "agentMessage", "text": "not ours", "phase": "final_answer" },
        })),
    )
    .expect("a foreign thread is ignored, not an error");
    assert_eq!(answer(&wire), None);
}

#[test]
fn a_message_for_another_turn_never_becomes_the_answer() {
    let mut wire = running();
    wire.on_notification(
        "item/completed",
        &object(json!({
            "threadId": "t-1",
            "turnId": "turn-99",
            "item": { "type": "agentMessage", "text": "not ours", "phase": "final_answer" },
        })),
    )
    .expect("a foreign turn is ignored, not an error");
    assert_eq!(answer(&wire), None);
}

#[test]
fn a_server_request_for_another_thread_is_rejected_because_answering_would_act_for_it() {
    let mut wire = running();
    let error = wire
        .on_server_request(
            "item/permissions/requestApproval",
            &object(json!({ "threadId": "someone-else", "turnId": "turn-1" })),
        )
        .unwrap_err();
    assert_eq!(error, CodexWireError::ForeignThread);
    assert_eq!(
        error.to_string(),
        "subagent-codex: app-server request referenced another thread"
    );
}

#[test]
fn a_server_request_for_another_turn_is_rejected() {
    let mut wire = running();
    let error = wire
        .on_server_request(
            "item/tool/requestUserInput",
            &object(json!({ "threadId": "t-1", "turnId": "turn-99" })),
        )
        .unwrap_err();
    assert_eq!(error, CodexWireError::ForeignTurn);
}

#[test]
fn a_server_request_before_turn_start_is_an_error_not_a_buffer() {
    // Outside an in-flight turn there is nowhere to defer a diagnostic, so naming a turn is a
    // protocol error rather than something to hold.
    let mut wire = connected();
    let error = wire
        .on_server_request(
            "item/commandExecution/requestApproval",
            &object(json!({
                "threadId": "t-1",
                "turnId": "turn-1",
                "availableDecisions": ["decline"],
            })),
        )
        .unwrap_err();
    assert_eq!(error, CodexWireError::TurnBeforeStart);
}

// ---------------------------------------------------------------------------------------------
// Premature frames: buffering and replay.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_item_buffered_before_the_turn_id_is_replayed_after_the_commit() {
    let mut wire = connected();
    wire.begin_turn();
    wire.on_notification(
        "turn/started",
        &object(json!({ "threadId": "t-1", "turn": { "id": "turn-1" } })),
    )
    .expect("turn/started only learns the provisional id");
    post_message(&mut wire, "early answer", &json!("final_answer"));
    assert_eq!(answer(&wire), None, "nothing is folded before the commit");

    wire.commit_turn(&json!({ "turn": { "id": "turn-1" } }))
        .expect("the commit replays the buffered item");
    assert_eq!(answer(&wire).as_deref(), Some("early answer"));
}

#[test]
fn an_early_terminal_notification_is_replayed_and_settles_the_turn() {
    let mut wire = connected();
    wire.begin_turn();
    post_terminal(&mut wire, "completed", None);
    assert_eq!(
        wire.finish_turn().unwrap_err(),
        CodexWireError::TurnNotTerminal,
        "the terminal frame is buffered, not yet effective"
    );
    wire.commit_turn(&json!({ "turn": { "id": "turn-1" } }))
        .unwrap();
    assert_eq!(
        wire.finish_turn().unwrap_err(),
        CodexWireError::EmptyAnswer,
        "the replayed terminal frame is now effective and found no answer"
    );
}

#[test]
fn the_buffered_frames_are_drained_once_not_on_every_commit() {
    let mut wire = connected();
    wire.begin_turn();
    post_message(&mut wire, "early", &json!("final_answer"));
    wire.commit_turn(&json!({ "turn": { "id": "turn-1" } }))
        .unwrap();
    assert_eq!(answer(&wire).as_deref(), Some("early"));
    // A later commit with the same id must not re-apply the buffered frame (which would be a no-op
    // here, but the buffer must be empty as a matter of state hygiene).
    wire.commit_turn(&json!({ "turn": { "id": "turn-1" } }))
        .unwrap();
    assert_eq!(answer(&wire).as_deref(), Some("early"));
}

#[test]
fn a_second_different_provisional_turn_is_refused() {
    let mut wire = connected();
    wire.begin_turn();
    wire.on_notification(
        "turn/started",
        &object(json!({ "threadId": "t-1", "turn": { "id": "turn-1" } })),
    )
    .expect("the first provisional turn is adopted");
    let error = wire
        .on_notification(
            "turn/started",
            &object(json!({ "threadId": "t-1", "turn": { "id": "turn-2" } })),
        )
        .unwrap_err();
    assert_eq!(error, CodexWireError::ConflictingTurns);
}

#[test]
fn a_commit_that_disagrees_with_the_observed_turn_is_refused() {
    let mut wire = connected();
    wire.begin_turn();
    wire.on_notification(
        "turn/started",
        &object(json!({ "threadId": "t-1", "turn": { "id": "turn-1" } })),
    )
    .unwrap();
    assert_eq!(
        wire.commit_turn(&json!({ "turn": { "id": "turn-2" } }))
            .unwrap_err(),
        CodexWireError::TurnMismatch
    );
}

// ---------------------------------------------------------------------------------------------
// Unattended approval: least authority, always answered, always explained.
// ---------------------------------------------------------------------------------------------

#[test]
fn every_implemented_request_method_is_answered() {
    for method in APPROVAL_METHODS {
        let mut wire = running();
        let answer = wire
            .on_server_request(
                method,
                &object(json!({ "threadId": "t-1", "turnId": "turn-1" })),
            )
            .unwrap_or_else(|error| panic!("{method} must be answered, got {error}"));
        assert!(
            answer.is_object(),
            "{method} must answer with an object, got {answer}"
        );
    }
}

#[test]
fn no_answer_grants_authority() {
    let mut wire = running();
    let permissions = wire
        .on_server_request(
            "item/permissions/requestApproval",
            &object(json!({ "threadId": "t-1", "turnId": "turn-1" })),
        )
        .unwrap();
    assert_eq!(
        permissions,
        json!({ "permissions": {}, "scope": "turn" }),
        "a permission grant must be empty"
    );

    let input = wire
        .on_server_request(
            "item/tool/requestUserInput",
            &object(json!({ "threadId": "t-1", "turnId": "turn-1" })),
        )
        .unwrap();
    assert_eq!(
        input,
        json!({ "answers": {} }),
        "user input stays unanswered"
    );

    let elicitation = wire
        .on_server_request(
            "mcpServer/elicitation/request",
            &object(json!({ "threadId": "t-1", "turnId": null })),
        )
        .unwrap();
    assert_eq!(
        elicitation.get("action").and_then(Value::as_str),
        Some("decline")
    );
}

#[test]
fn cancel_outranks_decline_when_the_server_offers_both() {
    let mut wire = running();
    let answer = wire
        .on_server_request(
            "item/commandExecution/requestApproval",
            &object(json!({
                "threadId": "t-1",
                "turnId": "turn-1",
                "availableDecisions": ["decline", "cancel"],
            })),
        )
        .unwrap();
    assert_eq!(answer, json!({ "decision": "cancel" }));
}

#[test]
fn an_approval_that_offers_no_answerable_decision_is_refused_rather_than_guessed() {
    let mut wire = running();
    let error = wire
        .on_server_request(
            "item/fileChange/requestApproval",
            &object(json!({
                "threadId": "t-1",
                "turnId": "turn-1",
                "availableDecisions": ["accept"],
            })),
        )
        .unwrap_err();
    assert_eq!(error, CodexWireError::NoUnattendedDecision);
}

#[test]
fn an_approval_with_no_decision_list_defaults_to_decline() {
    let mut wire = running();
    let answer = wire
        .on_server_request(
            "item/commandExecution/requestApproval",
            &object(json!({ "threadId": "t-1", "turnId": "turn-1" })),
        )
        .unwrap();
    assert_eq!(answer, json!({ "decision": "decline" }));
}

#[test]
fn a_provisional_approval_publishes_its_diagnostic_only_after_the_commit() {
    let mut wire = connected();
    wire.begin_turn();
    wire.on_server_request(
        "item/commandExecution/requestApproval",
        &object(json!({
            "threadId": "t-1",
            "turnId": "turn-1",
            "availableDecisions": ["decline"],
        })),
    )
    .expect("the request is answered immediately");
    assert_eq!(wire.collect_diagnostic(), None, "rank is not known yet");

    wire.commit_turn(&json!({ "turn": { "id": "turn-1" } }))
        .unwrap();
    assert_eq!(
        wire.collect_diagnostic(),
        Some(
            "Codex unattended decision (mode: approve-for-me; request: command approval; \
             decision: declined): the provider does not grant interactive approval"
        )
    );
}

#[test]
fn an_unknown_server_request_is_refused_by_name_so_the_answer_is_auditable() {
    let mut wire = running();
    let error = wire
        .on_server_request("item/telepathy", &object(json!({})))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "subagent-codex: unsupported app-server request \"item/telepathy\""
    );
}

#[test]
fn a_declined_command_and_a_declined_file_change_both_become_diagnostics() {
    let mut wire = running();
    wire.on_notification(
        "item/completed",
        &object(json!({
            "threadId": "t-1",
            "turnId": "turn-1",
            "item": { "type": "commandExecution", "status": "declined" },
        })),
    )
    .unwrap();
    let command = wire.collect_diagnostic().map(str::to_owned).unwrap();
    assert!(command.contains("request: command execution"));
    assert!(command.contains("decision: declined"));

    wire.on_notification(
        "item/completed",
        &object(json!({
            "threadId": "t-1",
            "turnId": "turn-1",
            "item": { "type": "fileChange", "status": "declined" },
        })),
    )
    .unwrap();
    let file = wire.collect_diagnostic().map(str::to_owned).unwrap();
    assert!(file.contains("request: file change"), "the later fact wins");
}

#[test]
fn a_buffered_earlier_frame_keeps_its_rank_and_cannot_overwrite_a_later_fact() {
    // The buffered-then-replayed path is exactly where ranks can invert: an early frame is observed
    // before the commit but applied after it, so its rank must come from *arrival*, not replay.
    let mut wire = connected();
    wire.begin_turn();
    post_message(&mut wire, "the answer", &json!("final_answer"));
    wire.on_notification(
        "item/completed",
        &object(json!({
            "threadId": "t-1",
            "turnId": "turn-1",
            "item": { "type": "commandExecution", "status": "declined" },
        })),
    )
    .expect("the early declined item is buffered with its arrival rank");
    wire.commit_turn(&json!({ "turn": { "id": "turn-1" } }))
        .unwrap();
    let early = wire.collect_diagnostic().map(str::to_owned).unwrap();
    assert!(early.contains("request: command execution"));

    // A later fact observed after the commit outranks it.
    wire.on_notification(
        "item/completed",
        &object(json!({
            "threadId": "t-1",
            "turnId": "turn-1",
            "item": { "type": "fileChange", "status": "declined" },
        })),
    )
    .unwrap();
    let later = wire.collect_diagnostic().map(str::to_owned).unwrap();
    assert!(
        later.contains("request: file change"),
        "the later fact wins"
    );
}

#[test]
fn the_diagnostic_sentence_has_upstream_shape() {
    assert_eq!(
        unattended_diagnostic(
            CodexPermissionMode::DangerouslyBypass,
            CodexUnattendedRequest::McpElicitation,
            CodexUnattendedDecision::Declined,
            "because",
        ),
        "Codex unattended decision \
         (mode: dangerously-bypass-approvals-and-sandbox; request: MCP elicitation; \
         decision: declined): because"
    );
}

// ---------------------------------------------------------------------------------------------
// Answer selection.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_answer_precedence_is_final_then_partial_then_unphased() {
    let mut wire = running();
    post_message(&mut wire, "unphased", &Value::Null);
    assert_eq!(answer(&wire).as_deref(), Some("unphased"));
    post_message(&mut wire, "partial", &json!("partial_answer"));
    assert_eq!(answer(&wire).as_deref(), Some("partial"));
    post_message(&mut wire, "final", &json!("final_answer"));
    assert_eq!(answer(&wire).as_deref(), Some("final"));
}

#[test]
fn the_latest_message_at_a_given_rank_wins() {
    let mut wire = running();
    post_message(&mut wire, "first", &json!("final_answer"));
    post_message(&mut wire, "second", &json!("final_answer"));
    assert_eq!(answer(&wire).as_deref(), Some("second"));
}

#[test]
fn commentary_never_becomes_the_answer() {
    let mut wire = running();
    post_message(&mut wire, "let me read the file", &json!("commentary"));
    assert_eq!(answer(&wire), None);
    post_message(&mut wire, "the answer", &json!("final_answer"));
    post_message(&mut wire, "still thinking", &json!("commentary"));
    assert_eq!(
        answer(&wire).as_deref(),
        Some("the answer"),
        "commentary must not displace a selected answer"
    );
}

#[test]
fn a_whitespace_only_answer_is_no_answer_but_exact_bytes_are_kept() {
    let mut wire = running();
    post_message(&mut wire, "   \n\t  ", &json!("final_answer"));
    assert_eq!(answer(&wire), None);
    post_message(&mut wire, "  padded  ", &json!("final_answer"));
    assert_eq!(
        answer(&wire).as_deref(),
        Some("  padded  "),
        "selection must not trim the bytes it reports"
    );
}

#[test]
fn an_unknown_message_phase_is_a_protocol_error() {
    let mut wire = running();
    let error = wire
        .on_notification(
            "item/completed",
            &object(json!({
                "threadId": "t-1",
                "turnId": "turn-1",
                "item": { "type": "agentMessage", "text": "x", "phase": "brainstorm" },
            })),
        )
        .unwrap_err();
    assert!(matches!(error, CodexWireError::UnknownMessagePhase { .. }));
}

#[test]
fn a_non_message_item_is_ignored() {
    let mut wire = running();
    wire.on_notification(
        "item/completed",
        &object(json!({
            "threadId": "t-1",
            "turnId": "turn-1",
            "item": { "type": "reasoning", "text": "thinking" },
        })),
    )
    .expect("a non-message item is not an answer and not an error");
    assert_eq!(answer(&wire), None);
}

// ---------------------------------------------------------------------------------------------
// Endings: ceiling, failure, invalid result.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_context_ceiling_keeps_the_text_and_is_reported_as_max_tokens() {
    let mut wire = running();
    post_message(&mut wire, "a truncated answer", &json!("final_answer"));
    post_terminal(
        &mut wire,
        "failed",
        Some(json!({ "codexErrorInfo": "contextWindowExceeded" })),
    );
    let outcome = wire.finish_turn().expect("a ceiling is not an error");
    assert!(matches!(outcome, CodexTurnOutcome::MaxTokens { .. }));
    assert_eq!(outcome.output().len(), 1);
    let facts = wire.collect_failure().expect("facts are recorded");
    assert_eq!(facts.category, CodexFailureCategory::Limit);
    assert_eq!(facts.stage, CodexFailureStage::Turn);
    assert_eq!(facts.http_status, None);
}

#[test]
fn a_usage_ceiling_is_a_failure_not_a_max_tokens_ending() {
    let mut wire = running();
    post_terminal(
        &mut wire,
        "failed",
        Some(json!({ "codexErrorInfo": "usageLimitExceeded" })),
    );
    let error = wire.finish_turn().unwrap_err();
    assert!(matches!(error, CodexWireError::TurnFailed { .. }));
    assert_eq!(
        wire.collect_failure().map(|facts| facts.category),
        Some(CodexFailureCategory::Limit)
    );
}

#[test]
fn a_sandbox_refusal_records_an_access_policy_fact_and_a_diagnostic() {
    let mut wire = running();
    post_terminal(
        &mut wire,
        "failed",
        Some(json!({ "codexErrorInfo": "sandboxError" })),
    );
    let error = wire.finish_turn().unwrap_err();
    assert_eq!(
        error.to_string(),
        "subagent-codex: Codex turn ended with status failed: access-policy"
    );
    let facts = wire.collect_failure().copied().unwrap();
    assert_eq!(facts.category, CodexFailureCategory::AccessPolicy);
    let diagnostic = wire.collect_diagnostic().unwrap();
    assert!(diagnostic.contains("request: sandbox execution"));
    assert!(diagnostic.contains("decision: failed"));
}

#[test]
fn a_transport_failure_keeps_its_http_status() {
    let mut wire = running();
    post_terminal(
        &mut wire,
        "failed",
        Some(json!({
            "codexErrorInfo": { "responseStreamDisconnected": { "httpStatusCode": 503 } },
        })),
    );
    wire.finish_turn().unwrap_err();
    let facts = wire.collect_failure().copied().unwrap();
    assert_eq!(facts.category, CodexFailureCategory::Transport);
    assert_eq!(facts.http_status, Some(503));
}

#[test]
fn a_status_that_is_not_an_http_status_is_dropped_not_clamped() {
    for reported in [json!(70_000), json!(-1), json!(1.5), json!("503")] {
        let mut wire = running();
        post_terminal(
            &mut wire,
            "failed",
            Some(
                json!({ "codexErrorInfo": { "httpConnectionFailed": { "httpStatusCode": reported } } }),
            ),
        );
        wire.finish_turn().unwrap_err();
        let facts = wire.collect_failure().copied().unwrap();
        assert_eq!(facts.category, CodexFailureCategory::Transport);
        assert_eq!(facts.http_status, None, "for reported {reported}");
    }
}

#[test]
fn an_ambiguous_error_object_is_classified_unknown_rather_than_guessed() {
    let turn = object(json!({
        "status": "failed",
        "error": {
            "codexErrorInfo": {
                "httpConnectionFailed": { "httpStatusCode": 500 },
                "activeTurnNotSteerable": {},
            },
        },
    }));
    assert_eq!(
        failure_info(&turn).category,
        CodexFailureCategory::Unknown,
        "a two-key detail is ambiguous"
    );
}

#[test]
fn every_documented_error_tag_maps_to_exactly_its_category() {
    let cases = [
        ("contextWindowExceeded", CodexFailureCategory::Limit),
        ("sessionBudgetExceeded", CodexFailureCategory::Limit),
        ("usageLimitExceeded", CodexFailureCategory::Limit),
        ("serverOverloaded", CodexFailureCategory::Service),
        ("internalServerError", CodexFailureCategory::Service),
        ("cyberPolicy", CodexFailureCategory::AccessPolicy),
        (
            "misalignmentPolicyViolation",
            CodexFailureCategory::AccessPolicy,
        ),
        ("unauthorized", CodexFailureCategory::AccessPolicy),
        ("sandboxError", CodexFailureCategory::AccessPolicy),
        ("badRequest", CodexFailureCategory::ProductError),
        ("threadRollbackFailed", CodexFailureCategory::ProductError),
        ("other", CodexFailureCategory::ProductError),
        ("notInTheTable", CodexFailureCategory::Unknown),
    ];
    for (tag, expected) in cases {
        let turn = object(json!({ "status": "failed", "error": { "codexErrorInfo": tag } }));
        assert_eq!(failure_info(&turn).category, expected, "tag {tag}");
    }
}

#[test]
fn a_non_failed_turn_has_no_failure_information() {
    let turn = object(json!({
        "status": "completed",
        "error": { "codexErrorInfo": "sandboxError" },
    }));
    assert_eq!(
        failure_info(&turn).category,
        CodexFailureCategory::Unknown,
        "only a failed turn carries failure information"
    );
}

#[test]
fn a_completed_turn_with_no_text_is_an_invalid_result_not_a_success() {
    let mut wire = running();
    post_terminal(&mut wire, "completed", None);
    let error = wire.finish_turn().unwrap_err();
    assert_eq!(error, CodexWireError::EmptyAnswer);
    assert_eq!(
        error.to_string(),
        "subagent-codex: Codex completed without an answer"
    );
    assert_eq!(
        wire.collect_failure().map(|facts| facts.category),
        Some(CodexFailureCategory::InvalidResult)
    );
}

#[test]
fn a_completed_turn_with_text_is_the_answer() {
    let mut wire = running();
    post_message(&mut wire, "the answer", &json!("final_answer"));
    post_terminal(&mut wire, "completed", None);
    let outcome = wire
        .finish_turn()
        .expect("a completed turn with text succeeds");
    assert!(matches!(outcome, CodexTurnOutcome::Completed { .. }));
    assert_eq!(
        outcome.output(),
        [ContentBlock::Text {
            text: "the answer".to_owned()
        }]
    );
    assert!(wire.collect_failure().is_none());
}

#[test]
fn an_interrupted_turn_names_its_status_without_a_category() {
    let mut wire = running();
    post_terminal(&mut wire, "interrupted", None);
    assert_eq!(
        wire.finish_turn().unwrap_err().to_string(),
        "subagent-codex: Codex turn ended with status interrupted"
    );
}

#[test]
fn a_terminal_status_outside_the_protocol_is_refused_at_the_frame() {
    let mut wire = running();
    let error = wire
        .on_notification(
            "turn/completed",
            &object(
                json!({ "threadId": "t-1", "turn": { "id": "turn-1", "status": "vaporised" } }),
            ),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        CodexWireError::InvalidTerminalStatus { .. }
    ));
}

#[test]
fn a_turn_start_failure_is_recorded_as_turn_start_not_turn() {
    // Upstream records this in the `catch` around `turn/start`: nothing reached the model, so a
    // supervisor may retry safely. It must not be reported as a turn failure.
    let mut wire = connected();
    assert!(wire.collect_failure().is_none());
    wire.record_turn_start_failure();
    let facts = wire.collect_failure().copied().expect("facts are recorded");
    assert_eq!(facts.stage, CodexFailureStage::TurnStart);
    assert_eq!(facts.stage.to_string(), "turn-start");
    assert_eq!(facts.category, CodexFailureCategory::Unknown);
    assert_eq!(facts.http_status, None);
}

#[test]
fn a_closed_wire_refuses_further_frames() {
    let mut wire = running();
    wire.close();
    assert!(wire.is_closed());
    assert_eq!(
        wire.on_notification("turn/started", &object(json!({})))
            .unwrap_err(),
        CodexWireError::Closed
    );
    assert_eq!(
        wire.on_server_request("item/permissions/requestApproval", &object(json!({})))
            .unwrap_err(),
        CodexWireError::Closed
    );
}

#[test]
fn the_wire_reports_its_permission_mode() {
    let wire = CodexWire::new(CodexPermissionMode::DangerouslyBypass, None);
    assert_eq!(
        wire.permission_mode(),
        CodexPermissionMode::DangerouslyBypass
    );
    assert_eq!(
        wire.permission_mode().to_string(),
        "dangerously-bypass-approvals-and-sandbox"
    );
}

#[test]
fn settling_before_the_terminal_frame_is_refused() {
    let mut wire = running();
    assert_eq!(
        wire.finish_turn().unwrap_err(),
        CodexWireError::TurnNotTerminal
    );
}

#[test]
fn a_turn_settles_once_and_the_second_attempt_finds_nothing() {
    let mut wire = running();
    post_message(&mut wire, "done", &json!("final_answer"));
    post_terminal(&mut wire, "completed", None);
    wire.finish_turn().expect("the first settlement succeeds");
    assert_eq!(
        wire.finish_turn().unwrap_err(),
        CodexWireError::TurnNotTerminal,
        "the consumed completion must not be replayed"
    );
}

// ---------------------------------------------------------------------------------------------
// Handshake, thread parameters, teardown.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_handshake_is_the_fixed_envelope_upstream_sends() {
    let payload = CodexWire::initialize_params();
    assert_eq!(
        payload.pointer("/clientInfo/name").and_then(Value::as_str),
        Some("deepseek-harness")
    );
    assert_eq!(
        payload
            .pointer("/clientInfo/version")
            .and_then(Value::as_str),
        Some("0.0.1")
    );
    assert_eq!(
        payload
            .pointer("/capabilities/requestAttestation")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(CodexWire::initialized_method(), "initialized");
}

#[test]
fn a_thread_that_is_not_ephemeral_is_refused_because_it_would_outlive_the_child() {
    let mut wire = CodexWire::new(CodexPermissionMode::Never, None);
    let error = wire
        .accept_thread(&json!({ "thread": { "id": "t-1" } }))
        .unwrap_err();
    assert_eq!(error, CodexWireError::NotEphemeral);
    assert_eq!(
        error.to_string(),
        "subagent-codex: app-server did not create an ephemeral thread"
    );
}

#[test]
fn an_explicitly_false_ephemeral_flag_is_also_refused() {
    let mut wire = CodexWire::new(CodexPermissionMode::Never, None);
    assert_eq!(
        wire.accept_thread(&json!({ "thread": { "id": "t-1", "ephemeral": false } }))
            .unwrap_err(),
        CodexWireError::NotEphemeral
    );
}

#[test]
fn the_permission_table_is_exact_including_absent_keys() {
    let never = CodexWire::new(CodexPermissionMode::Never, None).permission_params();
    assert_eq!(never.len(), 1);
    assert_eq!(
        never.get("approvalPolicy").and_then(Value::as_str),
        Some("never")
    );
    assert!(
        !never.contains_key("sandbox"),
        "`never` must let Codex apply its own sandbox default"
    );

    let approve = CodexWire::new(CodexPermissionMode::ApproveForMe, None).permission_params();
    assert_eq!(approve.len(), 3);
    assert_eq!(
        approve.get("approvalPolicy").and_then(Value::as_str),
        Some("on-request")
    );
    assert_eq!(
        approve.get("approvalsReviewer").and_then(Value::as_str),
        Some("auto_review")
    );
    assert_eq!(
        approve.get("sandbox").and_then(Value::as_str),
        Some("workspace-write")
    );

    let bypass = CodexWire::new(CodexPermissionMode::DangerouslyBypass, None).permission_params();
    assert_eq!(bypass.len(), 2);
    assert_eq!(
        bypass.get("approvalPolicy").and_then(Value::as_str),
        Some("never")
    );
    assert_eq!(
        bypass.get("sandbox").and_then(Value::as_str),
        Some("danger-full-access")
    );
    assert!(
        !bypass.contains_key("approvalsReviewer"),
        "bypassing approvals has no reviewer"
    );
}

#[test]
fn an_omitted_model_is_absent_from_the_thread_request_not_null() {
    let inherited = CodexWire::new(CodexPermissionMode::Never, None).thread_start_params("/work");
    assert!(
        !inherited.as_object().unwrap().contains_key("model"),
        "an explicit null would mean `no model`, which is not the app-server default"
    );
    assert_eq!(inherited.get("cwd").and_then(Value::as_str), Some("/work"));
    assert_eq!(inherited.get("ephemeral"), Some(&Value::Bool(true)));

    let explicit = CodexWire::new(CodexPermissionMode::Never, Some("gpt-5".to_owned()));
    assert_eq!(
        explicit
            .thread_start_params("/work")
            .get("model")
            .and_then(Value::as_str),
        Some("gpt-5")
    );
}

#[test]
fn a_text_task_is_sent_as_text_blocks_with_an_empty_element_list() {
    let wire = connected();
    let payload = wire.turn_start_params(&["one".to_owned(), "two".to_owned()]);
    assert_eq!(
        payload.get("threadId").and_then(Value::as_str),
        Some("t-1"),
        "the turn is bound to the thread this run created"
    );
    let input = payload.get("input").and_then(Value::as_array).unwrap();
    assert_eq!(input.len(), 2);
    for block in input {
        assert_eq!(block.get("type").and_then(Value::as_str), Some("text"));
        assert_eq!(block.get("text_elements"), Some(&Value::Array(Vec::new())));
    }
}

#[test]
fn interrupting_requires_a_thread_a_committed_turn_and_an_open_wire() {
    let mut wire = connected();
    assert!(wire.interrupt_params().is_none(), "no turn yet");
    wire.begin_turn();
    assert!(
        wire.interrupt_params().is_none(),
        "the turn is not committed"
    );
    wire.commit_turn(&json!({ "turn": { "id": "turn-1" } }))
        .unwrap();
    assert_eq!(
        wire.interrupt_params(),
        Some(json!({ "threadId": "t-1", "turnId": "turn-1" }))
    );
    wire.close();
    assert!(wire.is_closed());
    assert!(
        wire.interrupt_params().is_none(),
        "a closed wire must not emit a request that can only fail"
    );
    wire.close();
    assert!(wire.is_closed(), "closing twice is not an error");
}

#[test]
fn a_run_that_lost_its_protocol_before_the_terminal_frame_is_distinguishable() {
    let mut wire = running();
    assert!(!wire.ended_before_terminal());
    wire.input_ended();
    wire.input_ended();
    assert!(
        wire.ended_before_terminal(),
        "the child died mid-turn, which a supervisor must not read as success"
    );

    let mut finished = running();
    post_message(&mut finished, "done", &json!("final_answer"));
    post_terminal(&mut finished, "completed", None);
    finished.input_ended();
    assert!(!finished.ended_before_terminal());
}

#[test]
fn a_malformed_frame_names_the_label_it_was_reading() {
    let mut wire = CodexWire::new(CodexPermissionMode::Never, None);
    let error = wire.accept_thread(&json!({ "thread": [] })).unwrap_err();
    assert_eq!(
        error,
        CodexWireError::InvalidFrame {
            label: "thread/start thread".to_owned()
        }
    );
    assert_eq!(
        error.to_string(),
        "subagent-codex: app-server returned invalid thread/start thread"
    );
}

#[test]
fn an_empty_string_is_not_a_usable_id_but_any_non_empty_string_is() {
    let mut wire = CodexWire::new(CodexPermissionMode::Never, None);
    let error = wire
        .accept_thread(&json!({ "thread": { "id": "" } }))
        .unwrap_err();
    assert_eq!(
        error,
        CodexWireError::InvalidFrame {
            label: "thread/start thread id".to_owned()
        }
    );

    // The boundary is emptiness, not whitespace: upstream's `string()` helper rejects `''` only, and
    // narrowing it here would refuse ids a real app-server is entitled to mint.
    let mut wire = CodexWire::new(CodexPermissionMode::Never, None);
    wire.accept_thread(&json!({ "thread": { "id": "  ", "ephemeral": true } }))
        .expect("a non-empty id is accepted whatever its whitespace");
}

#[test]
fn an_unknown_notification_method_is_ignored_because_servers_keep_growing() {
    let mut wire = running();
    wire.on_notification("item/started", &object(json!({ "anything": 1 })))
        .expect("a method this driver does not classify must not fail the run");
    wire.on_notification("some/future/notification", &object(json!({})))
        .expect("nor must a method it has never heard of");
}

#[test]
fn the_initialize_response_must_be_an_object() {
    let mut wire = CodexWire::new(CodexPermissionMode::Never, None);
    assert!(!wire.is_initialized());
    wire.accept_initialize(&json!({ "userAgent": "codex" }))
        .expect("an object response completes the handshake");
    assert!(wire.is_initialized());
    assert!(matches!(
        wire.accept_initialize(&json!([])).unwrap_err(),
        CodexWireError::InvalidFrame { .. }
    ));
}
