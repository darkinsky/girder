//! Tests for the agent inbox (M4 of the v2 plan).
//!
//! The inbox is durable, so its correctness is about replay: a sequence of splices must reconstruct
//! the same state, and an invalid one must be refused where it appears rather than corrupting state
//! that is only noticed later.

use girder_core::{ChatMessage, Inbox, InboxError, InboxTarget, MessageId, Splice, replay_inbox};

fn message(id: &str) -> ChatMessage {
    ChatMessage::user(MessageId::new(id).expect("valid"), format!("from {id}"))
}

fn append(target: InboxTarget, ids: &[&str]) -> Splice {
    Splice::append(target, ids.iter().map(|id| message(id)).collect())
}

// ---- two queues -------------------------------------------------------------------------------

#[test]
fn the_two_queues_are_separate() {
    // A message sent mid-answer belongs to the next step; one sent while the model thinks belongs to
    // the next turn.
    let mut inbox = Inbox::new();
    inbox
        .apply(&append(InboxTarget::NextTurn, &["turn-1"]))
        .unwrap();
    inbox
        .apply(&append(InboxTarget::NextStep, &["step-1"]))
        .unwrap();

    assert_eq!(inbox.next_turn().len(), 1);
    assert_eq!(inbox.next_step().len(), 1);
    assert_eq!(inbox.len(), 2);
}

#[test]
fn draining_a_queue_leaves_the_other() {
    let mut inbox = Inbox::new();
    inbox.apply(&append(InboxTarget::NextTurn, &["t"])).unwrap();
    inbox.apply(&append(InboxTarget::NextStep, &["s"])).unwrap();

    let step = inbox.take_step();

    assert_eq!(step.len(), 1);
    assert_eq!(inbox.next_turn().len(), 1, "the other queue is untouched");
    assert!(inbox.next_step().is_empty());
}

#[test]
fn draining_preserves_order() {
    // The model must see the user's messages in the order they were sent.
    let mut inbox = Inbox::new();
    inbox
        .apply(&append(InboxTarget::NextStep, &["a", "b", "c"]))
        .unwrap();

    let taken: Vec<String> = inbox.take_step().iter().map(|m| m.id.to_string()).collect();

    assert_eq!(taken, vec!["a", "b", "c"]);
}

#[test]
fn an_empty_inbox_is_empty() {
    assert!(Inbox::new().is_empty());
}

// ---- range validation -------------------------------------------------------------------------

#[test]
fn a_splice_past_the_end_is_refused() {
    let mut inbox = Inbox::new();
    inbox.apply(&append(InboxTarget::NextStep, &["a"])).unwrap();

    let error = inbox
        .apply(&Splice::remove(InboxTarget::NextStep, 5, 1))
        .expect_err("out of range");

    let InboxError::OutOfRange { start, len, .. } = error else {
        panic!("expected OutOfRange, got {error:?}");
    };
    assert_eq!((start, len), (5, 1));
}

#[test]
fn a_removal_past_the_end_is_refused() {
    let mut inbox = Inbox::new();
    inbox
        .apply(&append(InboxTarget::NextStep, &["a", "b"]))
        .unwrap();

    let error = inbox
        .apply(&Splice::remove(InboxTarget::NextStep, 1, 5))
        .expect_err("removal past the end");

    assert!(matches!(error, InboxError::OutOfRange { .. }));
}

#[test]
fn a_refused_splice_leaves_the_inbox_untouched() {
    // Not half-applied: a refused splice must leave the previous state exactly as it was.
    let mut inbox = Inbox::new();
    inbox
        .apply(&append(InboxTarget::NextStep, &["a", "b"]))
        .unwrap();
    let before = inbox.clone();

    let _ = inbox.apply(&Splice::remove(InboxTarget::NextStep, 0, 99));

    assert_eq!(inbox, before);
}

// ---- the duplicate invariant ------------------------------------------------------------------

#[test]
fn the_same_message_cannot_be_pending_twice() {
    // A duplicate would be consumed twice, and the model would see one instruction as two messages --
    // or, worse, a re-send would look like the user repeating themselves.
    let mut inbox = Inbox::new();
    inbox.apply(&append(InboxTarget::NextStep, &["a"])).unwrap();

    let error = inbox
        .apply(&append(InboxTarget::NextStep, &["a"]))
        .expect_err("duplicate");

    let InboxError::Duplicate { id } = error else {
        panic!("expected Duplicate, got {error:?}");
    };
    assert_eq!(id, "a");
}

#[test]
fn the_duplicate_check_spans_both_queues() {
    // One message must not be pending in `next-turn` and waiting in `next-step`.
    let mut inbox = Inbox::new();
    inbox
        .apply(&append(InboxTarget::NextTurn, &["shared"]))
        .unwrap();

    let error = inbox
        .apply(&append(InboxTarget::NextStep, &["shared"]))
        .expect_err("cross-queue duplicate");

    assert!(matches!(error, InboxError::Duplicate { .. }));
}

#[test]
fn a_message_can_return_after_being_consumed() {
    // The invariant is about *pending* duplicates. Once drained, the id is free again, which is what
    // lets a retried delivery work.
    let mut inbox = Inbox::new();
    inbox.apply(&append(InboxTarget::NextStep, &["a"])).unwrap();
    let _ = inbox.take_step();

    inbox
        .apply(&append(InboxTarget::NextStep, &["a"]))
        .expect("the id is free once drained");
}

// ---- splice operations ------------------------------------------------------------------------

#[test]
fn a_removal_edits_the_queue() {
    let mut inbox = Inbox::new();
    inbox
        .apply(&append(InboxTarget::NextStep, &["a", "b", "c"]))
        .unwrap();

    inbox
        .apply(&Splice::remove(InboxTarget::NextStep, 1, 1))
        .unwrap();

    let ids: Vec<String> = inbox.next_step().iter().map(|m| m.id.to_string()).collect();
    assert_eq!(ids, vec!["a", "c"], "only the middle one was removed");
}

#[test]
fn an_insertion_lands_at_its_position() {
    let mut inbox = Inbox::new();
    inbox
        .apply(&append(InboxTarget::NextStep, &["a", "c"]))
        .unwrap();

    inbox
        .apply(&Splice {
            target: InboxTarget::NextStep,
            start: 1,
            removed: 0,
            inserted: vec![message("b")],
        })
        .unwrap();

    let ids: Vec<String> = inbox.next_step().iter().map(|m| m.id.to_string()).collect();
    assert_eq!(ids, vec!["a", "b", "c"]);
}

#[test]
fn a_splice_can_replace_in_one_operation() {
    // The reason the log records splices rather than add/remove pairs: the whole edit is one event,
    // so replay is order-independent within it.
    let mut inbox = Inbox::new();
    inbox
        .apply(&append(InboxTarget::NextStep, &["a", "b", "c"]))
        .unwrap();

    inbox
        .apply(&Splice {
            target: InboxTarget::NextStep,
            start: 1,
            removed: 2,
            inserted: vec![message("x")],
        })
        .unwrap();

    let ids: Vec<String> = inbox.next_step().iter().map(|m| m.id.to_string()).collect();
    assert_eq!(ids, vec!["a", "x"]);
}

// ---- undo -------------------------------------------------------------------------------------

#[test]
fn apply_returns_the_previous_state_for_undo() {
    // Used when a caller took input and then failed before consuming it, so the input is not lost.
    let mut inbox = Inbox::new();
    let previous = inbox.apply(&append(InboxTarget::NextStep, &["a"])).unwrap();
    assert!(previous.is_empty());

    inbox.restore(previous);

    assert!(inbox.is_empty());
}

// ---- replay -----------------------------------------------------------------------------------

#[test]
fn replaying_splices_reconstructs_the_state() {
    let splices = vec![
        append(InboxTarget::NextStep, &["a", "b"]),
        Splice::remove(InboxTarget::NextStep, 0, 1),
        append(InboxTarget::NextTurn, &["c"]),
    ];

    let inbox = replay_inbox(&splices).expect("replay");

    let step: Vec<String> = inbox.next_step().iter().map(|m| m.id.to_string()).collect();
    let turn: Vec<String> = inbox.next_turn().iter().map(|m| m.id.to_string()).collect();
    assert_eq!(step, vec!["b"]);
    assert_eq!(turn, vec!["c"]);
}

#[test]
fn replay_reports_which_splice_was_invalid() {
    // The index is where the corruption appears, not a session sequence number: this layer does not
    // know where the splice was stored.
    let splices = vec![
        append(InboxTarget::NextStep, &["a"]),
        append(InboxTarget::NextStep, &["a"]),
    ];

    let (index, error) = replay_inbox(&splices).expect_err("the second one duplicates");

    assert_eq!(index, 1);
    assert!(matches!(error, InboxError::Duplicate { .. }));
}

#[test]
fn replaying_nothing_yields_an_empty_inbox() {
    assert!(replay_inbox(&[]).expect("replay").is_empty());
}

#[test]
fn the_wire_names_match_upstream() {
    // These names are the log format.
    assert_eq!(InboxTarget::NextTurn.as_str(), "next-turn");
    assert_eq!(InboxTarget::NextStep.as_str(), "next-step");
}
