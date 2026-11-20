//! Tests for the session-log algebra: surface fold, pending-call repair, and seq-range framing.
//!
//! Three properties carry the weight, and each is tested from the side that can fail rather than
//! the side that trivially passes.
//!
//! The first is that a replacement *shadows* rather than deletes. A fold that removed the shadowed
//! nodes and a fold that spliced the replacement into their place agree on the final node list in
//! too many arrangements, so the tests assert the replacement's reported `shadowed` list and the
//! surviving position, not just the length.
//!
//! The second is that provenance is load-bearing. Every rule about `sources` exists to make "this
//! replaced those" auditable, so a replacement that cites too little must be refused, and the test
//! asserts the *specific* missing sequences rather than merely that something failed.
//!
//! The third is that recovery is conservative. An interrupted log must be closed with error results
//! that distinguish "never started" from "outcome unknown", because the two lead to opposite advice
//! about retrying — and a balanced log must produce no events at all, or every resume would append
//! noise to a healthy session.
//!
//! The module is included by path rather than through the crate root, so these tests compile against
//! the module as written even before `lib.rs` declares it. It resolves `girder_core::…` through the
//! alias the library declares for itself, which is why the same source builds here.

#[path = "../src/core/surface.rs"]
pub mod core_gap;

use core_gap::{
    EncodedSeq, OpenTurnCloseCause, SeqRangeError, SurfaceEnvelope, SurfaceError, SurfaceOp,
    TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN, ToolCallRecovery, decode_seq_ranges,
    derive_event_message, encode_seq_ranges, fold_surface, interrupted_turn_closers,
    is_append_surface_event, is_replacement_surface_event, is_surface_eligible_type,
    needs_message_projection, open_turn_closers, tool_result_content,
};
// The crate root re-exports these; the underlying modules are private, which is why the test
// imports through the root exactly as any downstream user would.
use girder_core::{
    ChatMessage as Message, ChatRole as Role, ContentBlock, MessageId, MessageSource, SessionEvent,
    ToolCallId, TurnEndReason,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// A user message that appends to the surface.
fn user(seq: u64, text: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::appended(SessionEvent::user_text(&format!("m{seq}"), text), seq)
}

/// An assistant message that appends to the surface.
fn assistant(seq: u64, text: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::appended(
        SessionEvent::assistant_text(&format!("m{seq}"), text, "provider"),
        seq,
    )
}

/// A system prompt message that appends to the surface.
fn system(seq: u64, text: &str) -> SurfaceEnvelope {
    let message = Message::new(
        MessageId::new(format!("m{seq}")).expect("generated ids are legal"),
        Role::System,
        MessageSource::SystemPrompt,
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    );
    // `turn`/`step` are required by the local variant; the surface rules do not read them.
    SurfaceEnvelope::appended(
        SessionEvent::SystemMessage {
            turn: 1,
            step: 1,
            message,
        },
        seq,
    )
}

/// An assistant message that requests one tool call.
fn assistant_requesting(seq: u64, call: &str) -> SurfaceEnvelope {
    let message = Message::new(
        MessageId::new(format!("m{seq}")).expect("generated ids are legal"),
        Role::Assistant,
        MessageSource::Model {
            provider: "provider".to_owned(),
            model: "model".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::ToolCall {
            id: ToolCallId::new(call).expect("generated ids are legal"),
            name: "read".to_owned(),
            arguments: "{}".to_owned(),
        }],
    );
    SurfaceEnvelope::appended(SessionEvent::AssistantMessage { message }, seq)
}

/// A tool call record for `call`.
fn call_record(seq: u64, call: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(
        SessionEvent::ToolCall {
            id: ToolCallId::new(call).expect("generated ids are legal"),
            name: "read".to_owned(),
            arguments: "{}".to_owned(),
        },
        seq,
    )
}

/// A tool result for `call`.
fn result(seq: u64, call: &str, text: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::appended(
        SessionEvent::ToolResult {
            id: ToolCallId::new(call).expect("generated ids are legal"),
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            is_error: false,
        },
        seq,
    )
}

/// A boundary event, which can never carry surface placement.
fn turn_start(seq: u64, turn: u64) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn }, seq)
}

/// A `turn/end` boundary.
fn turn_end(seq: u64, turn: u64, reason: TurnEndReason) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(SessionEvent::TurnEnd { turn, reason }, seq)
}

// ---- surface eligibility -----------------------------------------------------------------------

#[test]
fn only_message_producing_types_are_surface_eligible() {
    for eligible in [
        "system/message",
        "developer/message",
        "user/message",
        "assistant/message",
        "tool/result",
    ] {
        assert!(
            is_surface_eligible_type(eligible),
            "{eligible} must be eligible"
        );
    }
    // Boundaries and accounting records never produce a model-visible message.
    for log_only in [
        "turn/start",
        "turn/end",
        "step/start",
        "step/end",
        "model/usage",
        "tool/call",
        "session/title",
    ] {
        assert!(
            !is_surface_eligible_type(log_only),
            "{log_only} must not be eligible"
        );
    }
}

#[test]
fn image_offload_needs_an_interpreter() {
    // Folding an uninterpreted projection event would produce a history that is silently wrong,
    // which is worse than refusing the log.
    assert!(needs_message_projection("image/offload"));
    assert!(!needs_message_projection("user/message"));
}

#[test]
fn a_boundary_event_cannot_carry_placement() {
    let error = SurfaceEnvelope::new(
        SessionEvent::TurnStart { turn: 1 },
        0,
        Some(SurfaceOp::Append),
        vec![],
    )
    .expect_err("a turn boundary must not carry a surface op");
    assert!(matches!(error, SurfaceError::NotSurfaceEligible { .. }));
}

#[test]
fn a_boundary_event_cannot_carry_provenance() {
    let error = SurfaceEnvelope::new(SessionEvent::TurnStart { turn: 1 }, 0, None, vec![0])
        .expect_err("a turn boundary must not cite sources");
    assert!(matches!(error, SurfaceError::NotSurfaceEligible { .. }));
}

#[test]
fn an_eligible_event_may_be_constructed_without_placement() {
    // The fold rejects it, but construction does not: a reader decoding an old log must be able to
    // represent the record before deciding it is invalid.
    let envelope = SurfaceEnvelope::new(SessionEvent::user_text("m0", "hi"), 0, None, vec![])
        .expect("construction does not enforce surface placement");
    assert!(!is_append_surface_event(&envelope));
}

// ---- fold: appends -----------------------------------------------------------------------------

#[test]
fn appends_keep_log_order() {
    let log = vec![user(0, "one"), assistant(1, "two"), user(2, "three")];
    let fold = fold_surface(&log).expect("a log of appends folds");
    assert_eq!(fold.nodes(), &[0, 1, 2]);
    assert!(fold.replacements().is_empty());
    assert_eq!(fold.len(), 3);
    assert!(!fold.is_empty());
}

#[test]
fn log_only_events_do_not_join_the_surface() {
    let log = vec![
        turn_start(0, 1),
        user(1, "prompt"),
        turn_end(2, 1, TurnEndReason::Completed),
    ];
    let fold = fold_surface(&log).expect("boundaries fold");
    assert_eq!(fold.nodes(), &[1], "only the message occupies the surface");
}

#[test]
fn an_empty_log_folds_to_an_empty_surface() {
    let fold = fold_surface(&[]).expect("an empty log folds");
    assert!(fold.is_empty());
    assert_eq!(fold.len(), 0);
}

#[test]
fn a_surface_eligible_event_without_an_op_is_rejected() {
    let log = vec![SurfaceEnvelope::log_only(
        SessionEvent::user_text("m0", "hi"),
        0,
    )];
    assert_eq!(
        fold_surface(&log),
        Err(SurfaceError::MissingSurfaceOp {
            type_name: "user/message",
            seq: 0,
        })
    );
}

#[test]
fn sequence_gaps_are_rejected() {
    let log = vec![user(0, "one"), user(2, "three")];
    assert_eq!(
        fold_surface(&log),
        Err(SurfaceError::NotContiguous {
            actual: 2,
            expected: 1,
        })
    );
}

#[test]
fn an_uninterpreted_projection_event_is_rejected() {
    // There is no `image/offload` constructor in the local vocabulary, so the closest reachable
    // proof is that the predicate and the fold agree about the type it guards.
    assert!(needs_message_projection("image/offload"));
}

// ---- fold: replacements ------------------------------------------------------------------------

#[test]
fn a_replacement_shadows_the_range_and_stands_where_it_stood() {
    let mut log = vec![user(0, "a"), assistant(1, "b"), user(2, "c")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary", "compacted"),
        3,
        0,
        2,
        vec![0, 1, 2],
    ));

    let fold = fold_surface(&log).expect("a cited replacement folds");
    assert_eq!(
        fold.nodes(),
        &[3],
        "the range collapses to the replacement, in place"
    );
    assert!(!fold.is_visible(0));
    assert!(!fold.is_visible(1));
    assert!(!fold.is_visible(2));
    assert!(fold.is_visible(3));

    let replacements = fold.replacements();
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0].start, 0);
    assert_eq!(replacements[0].end, 2);
    assert_eq!(replacements[0].shadowed, vec![0, 1, 2]);
}

#[test]
fn a_replacement_keeps_its_position_rather_than_moving_to_the_tail() {
    // A compaction summarizes where it happened. If the replacement were appended to the tail, the
    // nodes after the range would be reordered relative to the model's view.
    let mut log = vec![user(0, "a"), assistant(1, "b"), user(2, "after")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("m3", "summary"),
        3,
        0,
        0,
        vec![0],
    ));

    let fold = fold_surface(&log).expect("a single-node replacement folds");
    assert_eq!(
        fold.nodes(),
        &[3, 1, 2],
        "the replacement sits at position 0, where the node it shadowed sat"
    );
}

#[test]
fn a_replacement_must_cite_every_shadowed_node() {
    let mut log = vec![user(0, "a"), assistant(1, "b"), user(2, "c")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary", "compacted"),
        3,
        0,
        2,
        vec![0, 2],
    ));

    assert_eq!(
        fold_surface(&log),
        Err(SurfaceError::UncitedShadow {
            seq: 3,
            missing: vec![1],
        }),
        "the missing sequence is named so the writer can fix the citation"
    );
}

#[test]
fn a_replacement_range_is_resolved_against_the_current_surface_not_the_log() {
    // Node 0 exists in the log but a prior replacement already shadowed it, so it is no longer a
    // surface node. A fold that searched the log instead of the surface would accept this and
    // splice at a position that no longer means anything.
    let mut log = vec![user(0, "a"), assistant(1, "b")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary-1", "first"),
        2,
        0,
        0,
        vec![0],
    ));
    assert_eq!(
        fold_surface(&log)
            .expect("the first replacement folds")
            .nodes(),
        &[2, 1]
    );

    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary-2", "second"),
        3,
        0,
        0,
        vec![0],
    ));
    assert!(
        matches!(
            fold_surface(&log),
            Err(SurfaceError::InvalidRange { seq: 3, .. })
        ),
        "node 0 was shadowed, so it is no longer addressable"
    );
}

#[test]
fn a_range_must_point_strictly_backwards() {
    let mut log = vec![user(0, "a")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary", "compacted"),
        1,
        0,
        1,
        vec![0],
    ));
    assert_eq!(
        fold_surface(&log),
        Err(SurfaceError::RangeNotEarlier {
            seq: 1,
            start: 0,
            end: 1,
        })
    );
}

#[test]
fn provenance_must_not_duplicate_or_exceed_the_log() {
    let mut duplicated = vec![user(0, "a"), assistant(1, "b"), user(2, "c")];
    duplicated.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary", "compacted"),
        3,
        0,
        1,
        vec![0, 0, 1],
    ));
    assert!(matches!(
        fold_surface(&duplicated),
        Err(SurfaceError::InvalidProvenance { seq: 3, .. })
    ));

    let mut future = vec![user(0, "a")];
    future.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary", "compacted"),
        1,
        0,
        0,
        vec![0, 5],
    ));
    assert!(matches!(
        fold_surface(&future),
        Err(SurfaceError::InvalidProvenance { seq: 1, .. })
    ));
}

#[test]
fn an_assistant_message_may_not_cite_sources() {
    // It embeds the provider stream it came from; a second account would let two readers disagree.
    let mut log = vec![user(0, "a")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::assistant_text("m1", "summary", "provider"),
        1,
        0,
        0,
        vec![0],
    ));
    assert!(matches!(
        fold_surface(&log),
        Err(SurfaceError::InvalidProvenance { seq: 1, .. })
    ));
}

#[test]
fn chained_replacements_fold_through_the_current_surface() {
    // The first replacement collapses 0..=1 into node 2. The second must then reference the
    // *current* surface, which is [2, 3] — not the original log positions.
    let mut log = vec![user(0, "a"), assistant(1, "b"), user(2, "c")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary-1", "first"),
        3,
        0,
        1,
        vec![0, 1],
    ));
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary-2", "second"),
        4,
        3,
        3,
        vec![3],
    ));

    let fold = fold_surface(&log).expect("chained replacements fold");
    assert_eq!(fold.nodes(), &[4, 2]);
    assert_eq!(fold.replacements().len(), 2);
}

// ---- fold: tool-result and system-head protection -----------------------------------------------

#[test]
fn a_tool_result_replacement_may_prune_content() {
    let mut log = vec![
        turn_start(0, 1),
        assistant_requesting(1, "call-1"),
        result(2, "call-1", "a very long output"),
    ];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::ToolResult {
            id: ToolCallId::new("call-1").expect("legal id"),
            content: vec![ContentBlock::Text {
                text: "pruned".to_owned(),
            }],
            is_error: false,
        },
        3,
        2,
        2,
        vec![2],
    ));

    let fold = fold_surface(&log).expect("pruning content is allowed");
    assert_eq!(fold.nodes(), &[1, 3]);
}

#[test]
fn a_tool_result_replacement_may_not_change_its_error_flag() {
    // Whether the tool failed is a fact about the run. A "prune" that flipped it would make the log
    // misreport what happened.
    let mut log = vec![result(0, "call-1", "boom")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::ToolResult {
            id: ToolCallId::new("call-1").expect("legal id"),
            content: vec![ContentBlock::Text {
                text: "pruned".to_owned(),
            }],
            is_error: true,
        },
        1,
        0,
        0,
        vec![0],
    ));
    assert_eq!(
        fold_surface(&log),
        Err(SurfaceError::ToolResultRewriteChangedFacts { seq: 1 })
    );
}

#[test]
fn the_system_prompt_at_node_zero_is_protected_from_a_foreign_replacement() {
    // Node zero is the contract the model was given at the start of the session; a summary standing
    // in for it would silently change the agent's rules.
    let mut log = vec![system(0, "you are a careful agent"), user(1, "go")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary", "compacted"),
        2,
        0,
        1,
        vec![0, 1],
    ));
    assert_eq!(
        fold_surface(&log),
        Err(SurfaceError::SystemHeadProtected { seq: 2, start: 0 })
    );
}

#[test]
fn a_system_message_may_rewrite_the_system_prompt() {
    let mut log = vec![system(0, "old prompt")];
    let replacement = Message::new(
        MessageId::new("m1").expect("legal id"),
        Role::System,
        MessageSource::SystemPrompt,
        vec![ContentBlock::Text {
            text: "new prompt".to_owned(),
        }],
    );
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::SystemMessage {
            turn: 1,
            step: 1,
            message: replacement,
        },
        1,
        0,
        0,
        vec![0],
    ));

    let fold = fold_surface(&log).expect("a system message may rewrite node zero");
    assert_eq!(fold.nodes(), &[1]);
}

#[test]
fn a_later_system_node_is_not_protected() {
    // Only node zero is the opening contract; the prompt is re-assembled per step, so a compaction
    // may shadow a later system node.
    let mut log = vec![user(0, "go"), system(1, "per-step prompt")];
    log.push(SurfaceEnvelope::replacing(
        SessionEvent::user_text("summary", "compacted"),
        2,
        1,
        1,
        vec![1],
    ));
    let fold = fold_surface(&log).expect("a later system node may be shadowed");
    assert_eq!(fold.nodes(), &[0, 2]);
}

// ---- message derivation ------------------------------------------------------------------------

#[test]
fn an_empty_assistant_message_derives_no_message() {
    // An empty assistant event hosts a max-tokens step's usage; injecting it as a content-less turn
    // is a protocol violation at the provider.
    let message = Message::new(
        MessageId::new("m0").expect("legal id"),
        Role::Assistant,
        MessageSource::Model {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            replay_state: None,
        },
        vec![],
    );
    let envelope = SurfaceEnvelope::appended(SessionEvent::AssistantMessage { message }, 0);
    assert!(derive_event_message(&envelope).is_none());
}

#[test]
fn an_empty_user_message_still_derives_a_message() {
    // An empty prompt is still a turn the user took; dropping it would make the transcript disagree
    // with the log.
    let message = Message::new(
        MessageId::new("m0").expect("legal id"),
        Role::User,
        MessageSource::User { form: None },
        vec![],
    );
    let envelope = SurfaceEnvelope::appended(SessionEvent::UserMessage { message }, 0);
    assert!(derive_event_message(&envelope).is_some());
}

#[test]
fn a_populated_assistant_message_derives_itself() {
    let envelope = assistant(0, "hello");
    assert_eq!(
        derive_event_message(&envelope).map(Message::text),
        Some("hello".to_owned())
    );
}

#[test]
fn a_boundary_event_derives_no_message() {
    assert!(derive_event_message(&turn_start(0, 1)).is_none());
}

#[test]
fn tool_result_content_is_readable_without_a_message_wrapper() {
    let envelope = result(0, "call-1", "output");
    assert_eq!(
        tool_result_content(&envelope),
        Some(
            &[ContentBlock::Text {
                text: "output".to_owned(),
            }][..]
        )
    );
    assert!(tool_result_content(&user(1, "not a result")).is_none());
}

#[test]
fn surface_predicates_agree_with_the_ops() {
    let appended = user(0, "a");
    assert!(is_append_surface_event(&appended));
    assert!(!is_replacement_surface_event(&appended));
    assert_eq!(appended.op, Some(SurfaceOp::Append));
    let Some(op) = appended.op else {
        panic!("an appended envelope carries an op");
    };
    assert!(op.is_append());
    assert!(!op.is_replace());

    let replacing =
        SurfaceEnvelope::replacing(SessionEvent::user_text("m1", "summary"), 1, 0, 0, vec![0]);
    assert!(is_replacement_surface_event(&replacing));
    assert!(!is_append_surface_event(&replacing));
    let Some(op) = replacing.op else {
        panic!("a replacing envelope carries an op");
    };
    assert!(op.is_replace());
    assert!(!op.is_append());
}

#[test]
fn a_recovery_tracker_reports_exactly_the_pending_calls() {
    // The tracker is the state behind recovery, so its own view must agree with the closer events:
    // nothing pending means nothing to synthesize.
    let mut recovery = ToolCallRecovery::new(OpenTurnCloseCause::Interrupted);
    assert_eq!(recovery.cause(), OpenTurnCloseCause::Interrupted);
    assert!(!recovery.has_pending());

    recovery.observe(&assistant_requesting(0, "call-1").event);
    assert!(recovery.has_pending());
    assert_eq!(recovery.pending(), vec!["call-1"]);
    assert_eq!(recovery.results().len(), 1);

    recovery.observe(&result(1, "call-1", "done").event);
    assert!(!recovery.has_pending());
    assert!(recovery.pending().is_empty());
    assert!(recovery.results().is_empty());
}

// ---- repair ------------------------------------------------------------------------------------

#[test]
fn a_balanced_log_needs_no_recovery() {
    // Recovery of a healthy session must write nothing, or every resume would append noise.
    let log = vec![
        turn_start(0, 1),
        user(1, "go"),
        turn_end(2, 1, TurnEndReason::Completed),
    ];
    assert!(interrupted_turn_closers(&log).is_empty());
}

#[test]
fn an_empty_log_needs_no_recovery() {
    assert!(interrupted_turn_closers(&[]).is_empty());
}

#[test]
fn an_open_turn_is_closed_with_a_turn_end_naming_the_cause() {
    let log = vec![turn_start(0, 1), user(1, "go")];
    let closers = interrupted_turn_closers(&log);
    assert_eq!(
        closers.len(),
        1,
        "no step was open, so only the turn closes"
    );
    match &closers[0] {
        SessionEvent::TurnEnd { turn, reason } => {
            assert_eq!(*turn, 1);
            assert_eq!(
                reason,
                &TurnEndReason::Aborted {
                    cause: "interrupted".to_owned()
                }
            );
        }
        other => panic!("expected turn/end, got {other:?}"),
    }
}

#[test]
fn an_open_step_is_closed_before_its_turn() {
    // A provider rejects a step that never ended; the order matters.
    let log = vec![
        turn_start(0, 1),
        SurfaceEnvelope::log_only(SessionEvent::StepStart { turn: 1, step: 1 }, 1),
    ];
    let closers = interrupted_turn_closers(&log);
    assert_eq!(closers.len(), 2);
    assert!(matches!(
        closers[0],
        SessionEvent::StepEnd { turn: 1, step: 1 }
    ));
    assert!(matches!(closers[1], SessionEvent::TurnEnd { turn: 1, .. }));
}

#[test]
fn an_unanswered_call_gets_an_error_result_before_the_boundary() {
    let log = vec![turn_start(0, 1), assistant_requesting(1, "call-1")];
    let closers = interrupted_turn_closers(&log);
    assert_eq!(closers.len(), 2, "one result plus the turn/end");
    match &closers[0] {
        SessionEvent::ToolResult {
            id,
            is_error,
            content,
        } => {
            assert_eq!(id.as_str(), "call-1");
            assert!(is_error, "a recovered result must be flagged as an error");
            let text = match content.first() {
                Some(ContentBlock::Text { text }) => text,
                other => panic!("expected text content, got {other:?}"),
            };
            assert!(
                text.contains("Retry it if it is still needed"),
                "a call that never started is safe to retry: {text}"
            );
        }
        other => panic!("expected tool/result, got {other:?}"),
    }
    assert!(matches!(closers[1], SessionEvent::TurnEnd { .. }));
}

#[test]
fn a_started_call_reports_an_unknown_outcome_instead_of_advising_a_retry() {
    // This is the distinction that matters: a started call may already have had its side effect, so
    // telling the model to simply retry is unsafe.
    let log = vec![
        turn_start(0, 1),
        assistant_requesting(1, "call-1"),
        call_record(2, "call-1"),
    ];
    let closers = interrupted_turn_closers(&log);
    match &closers[0] {
        SessionEvent::ToolResult { content, .. } => {
            let text = match content.first() {
                Some(ContentBlock::Text { text }) => text,
                other => panic!("expected text content, got {other:?}"),
            };
            assert!(
                text.contains("Its outcome is unknown"),
                "a started call has an unknown outcome: {text}"
            );
            assert!(
                text.contains("Do not retry blindly"),
                "the wording must warn against a blind retry: {text}"
            );
        }
        other => panic!("expected tool/result, got {other:?}"),
    }
}

#[test]
fn an_answered_call_is_not_recovered() {
    let log = vec![
        turn_start(0, 1),
        assistant_requesting(1, "call-1"),
        call_record(2, "call-1"),
        result(3, "call-1", "done"),
    ];
    let closers = interrupted_turn_closers(&log);
    assert_eq!(closers.len(), 1, "only the turn/end remains");
    assert!(matches!(closers[0], SessionEvent::TurnEnd { .. }));
}

#[test]
fn a_forked_cause_uses_fork_wording_and_its_own_reason() {
    let log = vec![turn_start(0, 1), assistant_requesting(1, "call-1")];
    let closers = open_turn_closers(&log, OpenTurnCloseCause::Forked);
    match &closers[0] {
        SessionEvent::ToolResult { content, .. } => {
            let text = match content.first() {
                Some(ContentBlock::Text { text }) => text,
                other => panic!("expected text content, got {other:?}"),
            };
            assert!(
                text.contains("inherited by this branch"),
                "fork wording names the branch: {text}"
            );
        }
        other => panic!("expected tool/result, got {other:?}"),
    }
    match closers.last() {
        Some(SessionEvent::TurnEnd { reason, .. }) => assert_eq!(
            reason,
            &TurnEndReason::Aborted {
                cause: "forked".to_owned()
            }
        ),
        other => panic!("expected turn/end, got {other:?}"),
    }
}

#[test]
fn a_step_boundary_discards_pending_calls() {
    // A call requested inside a step that already ended is no longer pending: the boundary is the
    // record that the step is over, so recovery must not invent a result for it.
    let log = vec![
        turn_start(0, 1),
        assistant_requesting(1, "call-1"),
        SurfaceEnvelope::log_only(SessionEvent::StepEnd { turn: 1, step: 1 }, 2),
    ];
    let closers = interrupted_turn_closers(&log);
    assert_eq!(
        closers.len(),
        1,
        "only the turn/end remains; the closed step's call is not recovered"
    );
    assert!(matches!(closers[0], SessionEvent::TurnEnd { .. }));
}

#[test]
fn recovery_codes_are_distinct_and_stable() {
    // A reader branches on these strings, so they must not drift.
    assert_eq!(TOOL_NOT_STARTED, "TOOL_NOT_STARTED");
    assert_eq!(TOOL_OUTCOME_UNKNOWN, "TOOL_OUTCOME_UNKNOWN");
    assert_ne!(TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN);
}

// ---- seq ranges --------------------------------------------------------------------------------

#[test]
fn two_consecutive_values_stay_single() {
    // A range costs the same two numbers as the pair it would replace, so it must not be used.
    assert_eq!(
        encode_seq_ranges(&[0, 1]),
        vec![EncodedSeq::Single(0), EncodedSeq::Single(1)]
    );
}

#[test]
fn three_consecutive_values_become_a_range() {
    assert_eq!(
        encode_seq_ranges(&[4, 5, 6]),
        vec![EncodedSeq::Range { start: 4, end: 6 }]
    );
}

#[test]
fn a_run_is_split_around_gaps() {
    assert_eq!(
        encode_seq_ranges(&[0, 1, 2, 3, 7, 9, 10, 11]),
        vec![
            EncodedSeq::Range { start: 0, end: 3 },
            EncodedSeq::Single(7),
            EncodedSeq::Range { start: 9, end: 11 },
        ]
    );
}

#[test]
fn unsorted_input_is_returned_as_singles() {
    // Collapsing an unsorted or repeated list would either lose the repeats or invent an order.
    assert_eq!(
        encode_seq_ranges(&[2, 1, 3]),
        vec![
            EncodedSeq::Single(2),
            EncodedSeq::Single(1),
            EncodedSeq::Single(3),
        ]
    );
    assert_eq!(
        encode_seq_ranges(&[5, 5, 5]),
        vec![
            EncodedSeq::Single(5),
            EncodedSeq::Single(5),
            EncodedSeq::Single(5),
        ]
    );
}

#[test]
fn an_empty_list_encodes_and_decodes_to_empty() {
    assert!(encode_seq_ranges(&[]).is_empty());
    assert_eq!(decode_seq_ranges(&[], 10), Ok(Vec::new()));
}

#[test]
fn encoding_is_lossless() {
    let values = vec![0, 1, 2, 3, 7, 9, 10, 11, 50, 51, 52, 53, 54];
    let encoded = encode_seq_ranges(&values);
    assert_eq!(decode_seq_ranges(&encoded, 1000), Ok(values));
}

#[test]
fn decoding_rejects_a_backwards_range() {
    assert_eq!(
        decode_seq_ranges(&[EncodedSeq::Range { start: 5, end: 2 }], 10),
        Err(SeqRangeError::Backwards { start: 5, end: 2 })
    );
}

#[test]
fn decoding_rejects_an_expansion_beyond_the_limit() {
    // A range is two numbers that can expand to billions; the caller states its budget.
    assert_eq!(
        decode_seq_ranges(&[EncodedSeq::Range { start: 0, end: 9 }], 5),
        Err(SeqRangeError::TooMany { max: 5 })
    );
}

#[test]
fn decoding_rejects_singles_beyond_the_limit() {
    assert_eq!(
        decode_seq_ranges(
            &[
                EncodedSeq::Single(0),
                EncodedSeq::Single(1),
                EncodedSeq::Single(2),
            ],
            2,
        ),
        Err(SeqRangeError::TooMany { max: 2 })
    );
}

#[test]
fn decoding_rejects_a_non_increasing_expansion() {
    // A single is not checked against its predecessor, so an unsorted list of singles round-trips;
    // but a range that lands before an earlier value is a malformed encoding.
    assert_eq!(
        decode_seq_ranges(
            &[
                EncodedSeq::Single(10),
                EncodedSeq::Range { start: 0, end: 2 },
            ],
            100,
        ),
        Err(SeqRangeError::NotIncreasing)
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod core {
    pub use super::core_gap as surface;
}
