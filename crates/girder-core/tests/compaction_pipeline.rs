//! Tests for the whole-history compaction pipeline.
//!
//! Three things are checked here that no single module could check for itself:
//!
//! - the pass is a *composition*, so a policy decision reaches the summary and the summary reaches
//!   the caller's history rather than stopping at a stage boundary;
//! - the skip path always explains itself, because "nothing happened" is the answer most of the
//!   time and an unexplained one is indistinguishable from a bug;
//! - the injected pairing predicate is authoritative, so a cut the policy would accept is still
//!   refused when the event stream says the boundary is illegal.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the pipeline as written even before the crate root wires it in. It resolves `girder_core::…`
// through the alias the library declares for itself, which is why the same source builds here.
#[path = "../src/compaction_pipeline.rs"]
mod compaction_pipeline;

use compaction_pipeline::{Outcome, run};
use girder_core::{
    ChatMessage, ChatRole, CompactionPolicy, ContentBlock, MessageId, MessageSource, ToolCallId,
    is_checkpoint,
};

fn id(value: &str) -> MessageId {
    MessageId::new(value).expect("valid message id")
}

fn call_id(value: &str) -> ToolCallId {
    ToolCallId::new(value).expect("valid call id")
}

fn user(index: usize, text: &str) -> ChatMessage {
    ChatMessage::user(id(&format!("u{index}")), text)
}

fn assistant(index: usize, text: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("a{index}")),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "test".to_owned(),
            model: "test".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    )
}

/// An assistant message asking for one tool call.
fn call(index: usize, call: &str, name: &str, arguments: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("a{index}")),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "test".to_owned(),
            model: "test".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::ToolCall {
            id: call_id(call),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }],
    )
}

/// A tool-role message answering `call`.
fn result(index: usize, call: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("r{index}")),
        ChatRole::Tool,
        MessageSource::Tool {
            call_id: call_id(call),
        },
        vec![ContentBlock::Text {
            text: "ok".to_owned(),
        }],
    )
}

/// A history no tool call ever opens, so every boundary is balanced.
fn conversation(len: usize) -> Vec<ChatMessage> {
    (0..len)
        .map(|index| {
            if index % 2 == 0 {
                user(index, &format!("request number {index}"))
            } else {
                assistant(index, &format!("reply number {index}"))
            }
        })
        .collect()
}

/// A policy that always treats every boundary as legal, for tests about other stages.
fn always_balanced(_: usize) -> bool {
    true
}

// ---- the skip path ---------------------------------------------------------------------------

#[test]
fn empty_history_is_not_compacted_and_says_why() {
    let policy = CompactionPolicy::new(1, 1);
    let outcome = run(&[], &policy, always_balanced);

    assert!(!outcome.compacted());
    assert_eq!(outcome.summary, None);
    assert!(outcome.kept.is_empty());
    let reason = outcome
        .skipped
        .expect("an empty history is skipped, not silent");
    assert!(
        reason.contains("empty"),
        "the reason should name the actual cause: {reason}"
    );
}

#[test]
fn short_history_is_skipped_because_dropping_it_would_drop_nothing() {
    let messages = conversation(3);
    // The keep floor is larger than the history, so no cut can free anything no matter the trigger.
    let policy = CompactionPolicy::new(1, 8);
    let outcome = run(&messages, &policy, always_balanced);

    assert!(!outcome.compacted());
    assert_eq!(
        outcome.kept, messages,
        "a skipped pass hands the input back"
    );
    let reason = outcome.skipped.expect("a skip carries a reason");
    assert!(
        reason.contains("drop nothing"),
        "too-few-messages and policy-refusal must read differently: {reason}"
    );
}

#[test]
fn history_below_the_trigger_is_skipped_as_not_yet() {
    let messages = conversation(5);
    // Enough messages that a cut could free something, but the trigger has not fired.
    let policy = CompactionPolicy::new(40, 2);
    let outcome = run(&messages, &policy, always_balanced);

    assert!(!outcome.compacted());
    let reason = outcome.skipped.expect("a skip carries a reason");
    assert!(
        reason.contains("below the 40 message trigger"),
        "the reason should name the trigger that did not fire: {reason}"
    );
}

#[test]
fn policy_refusal_is_reported_verbatim() {
    let messages = conversation(6);
    // A keep floor of zero is a configuration the policy refuses outright, which is a different
    // answer from "there is nothing to do here".
    let policy = CompactionPolicy::new(1, 0);
    let outcome = run(&messages, &policy, always_balanced);

    assert!(!outcome.compacted());
    assert_eq!(outcome.summary, None);
    assert_eq!(outcome.kept, messages);
    let reason = outcome.skipped.expect("a refusal is a skip with a reason");
    assert!(
        reason.contains("refused"),
        "a refusal should be labelled as one, not as a short history: {reason}"
    );
    assert!(
        reason.contains("entire history"),
        "the policy's own explanation should survive the pipeline: {reason}"
    );
}

// ---- the compaction path ---------------------------------------------------------------------

#[test]
fn compaction_produces_a_summary_and_a_leading_checkpoint() {
    let messages = conversation(10);
    let policy = CompactionPolicy::new(4, 3);
    let outcome = run(&messages, &policy, always_balanced);

    assert!(outcome.compacted());
    assert_eq!(outcome.skipped, None);

    let summary = outcome.summary.expect("a compaction has a summary");
    assert!(
        summary.contains("compacted span"),
        "the rendered summary should be the block the model reads: {summary}"
    );

    let checkpoint = outcome
        .kept
        .first()
        .expect("a compacted history keeps a checkpoint");
    assert!(
        is_checkpoint(checkpoint),
        "the summary must travel as the checkpoint shape the loop already recognises"
    );
    assert!(
        checkpoint.text().contains(&summary),
        "the returned summary and the checkpoint text must be the same text"
    );
    assert_eq!(
        checkpoint.id.as_str(),
        messages[0].id.as_str(),
        "the checkpoint takes the identity of the first message it replaces"
    );
}

#[test]
fn summary_carries_the_users_request_verbatim() {
    let messages = conversation(10);
    let policy = CompactionPolicy::new(4, 3);
    let outcome = run(&messages, &policy, always_balanced);

    let summary = outcome.summary.expect("a compaction has a summary");
    // Requests inside the dropped span are the part of the history that may never be paraphrased.
    assert!(
        summary.contains("request number 0"),
        "the earliest requirement must survive the cut: {summary}"
    );
    assert!(
        summary.contains("request number 2"),
        "every dropped requirement must survive, not only the first: {summary}"
    );
}

#[test]
fn kept_messages_keep_their_original_order() {
    let messages = conversation(10);
    let policy = CompactionPolicy::new(4, 3);
    let outcome = run(&messages, &policy, always_balanced);

    // The leading element is the checkpoint; what survives after it must be the input's own tail.
    let survivors: Vec<&ChatMessage> = outcome.kept.iter().skip(1).collect();
    let expected: Vec<&ChatMessage> = messages.iter().skip(7).collect();
    assert_eq!(survivors.len(), expected.len());
    for (kept, original) in survivors.iter().zip(expected.iter()) {
        assert_eq!(
            kept.id.as_str(),
            original.id.as_str(),
            "the surviving history must keep the conversation's own order"
        );
    }
    assert!(
        outcome.kept.len() < messages.len(),
        "the point of compacting is that the history gets shorter"
    );
}

#[test]
fn tool_call_and_result_survive_or_go_together() {
    // A completed exchange at the very front, so the cut can be taken after it.
    let mut messages = vec![
        user(0, "run the tests"),
        call(1, "c1", "bash", r#"{"command":"cargo test"}"#),
        result(2, "c1"),
        assistant(3, "the tests pass"),
    ];
    messages.extend(conversation(8));
    let policy = CompactionPolicy::new(4, 2);
    let outcome = run(&messages, &policy, always_balanced);

    assert!(outcome.compacted());
    let kept_ids: Vec<&str> = outcome
        .kept
        .iter()
        .map(|message| message.id.as_str())
        .collect();
    let keeps_call = kept_ids.contains(&"a1");
    let keeps_result = kept_ids.contains(&"r2");
    assert_eq!(
        keeps_call, keeps_result,
        "a call and its result must be kept or dropped together, never split"
    );
}

// ---- the injected predicate ------------------------------------------------------------------

#[test]
fn a_cut_off_a_pairing_boundary_is_refused() {
    let messages = conversation(10);
    let policy = CompactionPolicy::new(4, 3);
    // The event stream says the boundary the policy chose is illegal, which outranks the policy's
    // own message-level reading.
    let outcome = run(&messages, &policy, |_| false);

    assert!(!outcome.compacted());
    assert_eq!(outcome.summary, None);
    assert_eq!(
        outcome.kept, messages,
        "a refused cut must leave the history exactly as it was"
    );
    let reason = outcome.skipped.expect("a refused cut carries a reason");
    assert!(
        reason.contains("tool-pairing boundary"),
        "the reason should name the rule that stopped it: {reason}"
    );
}

#[test]
fn the_predicate_is_asked_about_the_cut_the_plan_chose() {
    let messages = conversation(10);
    let policy = CompactionPolicy::new(4, 3);

    // 10 messages, keep the last 3, so the cut falls after message 6 — index 6, not 7.
    let asked = std::cell::Cell::new(None);
    let outcome = run(&messages, &policy, |index| {
        asked.set(Some(index));
        true
    });

    assert!(outcome.compacted());
    assert_eq!(
        asked.get(),
        Some(6),
        "the predicate must be asked about the boundary the plan actually cuts at"
    );
}

// ---- outcome invariants ----------------------------------------------------------------------

#[test]
fn summary_and_skip_reason_are_never_both_present() {
    let cases = [
        (Vec::new(), CompactionPolicy::new(1, 1)),
        (conversation(3), CompactionPolicy::new(1, 8)),
        (conversation(10), CompactionPolicy::new(4, 3)),
        (conversation(10), CompactionPolicy::new(1, 0)),
    ];

    for (messages, policy) in cases {
        let outcome: Outcome = run(&messages, &policy, always_balanced);
        assert_eq!(
            outcome.summary.is_some(),
            outcome.skipped.is_none(),
            "a pass either compacted or explained itself, never both and never neither"
        );
    }
}
