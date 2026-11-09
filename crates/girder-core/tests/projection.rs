//! Tests for session projections (M3 of the v2 plan).
//!
//! Two properties matter most. An event delivered twice must fold once, because the caller that
//! feeds them is a subscription and a subscription can repeat. And a cached fold must equal a fresh
//! replay, because that is the whole claim of having both.

use std::sync::Arc;

use girder_core::{
    ChatMessage, ContentBlock, MessageId, ModelSelection, ProjectionRegistry, SessionEvent,
    TitleProjection, TurnBoundary,
};

fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

fn turn_end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: girder_core::TurnEndReason::Completed,
    }
}

fn step_start(turn: u64, step: u64) -> SessionEvent {
    SessionEvent::StepStart { turn, step }
}

fn registry() -> ProjectionRegistry {
    let mut registry = ProjectionRegistry::new();
    registry
        .register(Arc::new(TurnBoundary))
        .expect("registered");
    registry
}

// ---- registration ------------------------------------------------------------------------------

#[test]
fn a_registered_projection_starts_at_its_initial_state() {
    let registry = registry();
    let state = registry.read("turnBoundary").expect("present");

    assert_eq!(state["lastTurn"], 0);
    assert!(state["openTurnStartSeq"].is_null());
}

#[test]
fn registering_the_same_key_twice_is_refused() {
    // Replacing silently would leave a UI reading one definition's state under another's name.
    let mut registry = registry();
    let error = registry
        .register(Arc::new(TurnBoundary))
        .expect_err("duplicate");

    assert!(matches!(
        error,
        girder_core::ProjectionError::Duplicate { .. }
    ));
}

#[test]
fn keys_are_sorted() {
    // A view renders from them, and an order that changes between runs makes the UI jump.
    let mut registry = ProjectionRegistry::new();
    registry.register(Arc::new(TurnBoundary)).unwrap();
    registry.register(Arc::new(TitleProjection)).unwrap();
    registry
        .register(Arc::new(girder_core::ModelSelectionProjection))
        .unwrap();

    assert_eq!(
        registry.keys(),
        vec!["modelSelection", "title", "turnBoundary"]
    );
}

// ---- folding -----------------------------------------------------------------------------------

#[test]
fn a_turn_start_records_the_open_turn() {
    let registry = registry();
    registry.observe(&turn_start(1), 1);

    let state = registry.read("turnBoundary").unwrap();
    assert_eq!(state["openTurnStartSeq"], 1);
    assert_eq!(state["lastTurn"], 1);
}

#[test]
fn a_turn_end_closes_it() {
    // An open turn that has ended is not open; the field returns to null rather than keeping a stale
    // sequence number.
    let registry = registry();
    registry.observe(&turn_start(1), 1);
    registry.observe(&turn_end(1), 5);

    assert!(registry.read("turnBoundary").unwrap()["openTurnStartSeq"].is_null());
}

#[test]
fn a_step_start_records_its_sequence() {
    let registry = registry();
    registry.observe(&turn_start(1), 1);
    registry.observe(&step_start(1, 1), 2);

    assert_eq!(
        registry.read("turnBoundary").unwrap()["lastStepStartSeq"],
        2
    );
}

#[test]
fn an_irrelevant_event_leaves_the_state_unchanged() {
    let registry = registry();
    let before = registry.read("turnBoundary").unwrap();
    registry.observe(&SessionEvent::user_text("m", "hi"), 1);

    assert_eq!(registry.read("turnBoundary").unwrap(), before);
}

#[test]
fn the_model_selection_projection_records_the_last_selection() {
    let mut registry = ProjectionRegistry::new();
    registry
        .register(Arc::new(girder_core::ModelSelectionProjection))
        .unwrap();

    registry.observe(
        &SessionEvent::ModelSelection {
            selection: ModelSelection::new("ds", "deepseek-reasoner"),
        },
        1,
    );

    let state = registry.read("modelSelection").unwrap();
    assert_eq!(state["lastUsed"]["model"], "deepseek-reasoner");
}

#[test]
fn the_title_projection_records_the_title_and_its_source() {
    let mut registry = ProjectionRegistry::new();
    registry.register(Arc::new(TitleProjection)).unwrap();

    registry.observe(
        &SessionEvent::SessionTitle {
            title: "porting a project".to_owned(),
            message_seqs: vec![1],
            source: "model".to_owned(),
        },
        1,
    );

    let state = registry.read("title").unwrap();
    assert_eq!(state["title"], "porting a project");
    assert_eq!(state["source"], "model");
}

// ---- idempotence -------------------------------------------------------------------------------

#[test]
fn a_repeated_event_folds_once() {
    // The caller is a subscription, and a subscription can deliver the same event twice.
    let registry = registry();
    registry.observe(&turn_start(1), 1);
    registry.observe(&turn_start(1), 1);
    registry.observe(&turn_start(1), 1);

    assert_eq!(
        registry.read("turnBoundary").unwrap()["openTurnStartSeq"],
        1
    );
}

#[test]
fn an_out_of_order_replay_does_not_move_the_cursor_backwards() {
    let registry = registry();
    registry.observe(&turn_start(1), 5);
    registry.observe(&turn_end(1), 4);

    // The cursor is at 5, so the event at 4 is skipped.
    assert_eq!(
        registry.read("turnBoundary").unwrap()["openTurnStartSeq"],
        5
    );
}

// ---- replay ------------------------------------------------------------------------------------

#[test]
fn a_fresh_replay_matches_an_incremental_fold() {
    // The claim of having both a cache and a replay: they agree.
    let events = vec![
        (1, turn_start(1)),
        (2, step_start(1, 1)),
        (3, turn_end(1)),
        (4, turn_start(2)),
    ];

    let incremental = registry();
    for (seq, event) in &events {
        incremental.observe(event, *seq);
    }

    let replayed = registry();
    replayed.replay(&events);

    assert_eq!(incremental.view(), replayed.view());
}

#[test]
fn replay_resets_rather_than_continues() {
    // A resumed session must see a state built from the whole log, not from whatever the previous
    // process folded.
    let registry = registry();
    registry.observe(&turn_start(9), 99);
    assert_eq!(registry.read("turnBoundary").unwrap()["lastTurn"], 9);

    registry.replay(&[(1, turn_start(1))]);

    assert_eq!(registry.read("turnBoundary").unwrap()["lastTurn"], 1);
}

#[test]
fn replaying_an_empty_log_yields_the_initial_state() {
    let registry = registry();
    registry.observe(&turn_start(3), 3);

    registry.replay(&[]);

    assert_eq!(registry.read("turnBoundary").unwrap()["lastTurn"], 0);
}

// ---- views -------------------------------------------------------------------------------------

#[test]
fn the_summary_holds_every_projection_that_declares_one() {
    let mut registry = ProjectionRegistry::new();
    registry.register(Arc::new(TurnBoundary)).unwrap();
    registry.register(Arc::new(TitleProjection)).unwrap();

    let summary = registry.summary();

    assert_eq!(summary.len(), 2);
    assert!(summary.contains_key("turnBoundary"));
    assert!(summary.contains_key("title"));
}

#[test]
fn the_view_holds_every_projection() {
    let registry = registry();
    let view = registry.view();

    assert_eq!(view.len(), 1);
    assert!(view.contains_key("turnBoundary"));
}

#[test]
fn reading_an_unknown_projection_is_none() {
    assert!(registry().read("nope").is_none());
}

#[test]
fn the_state_survives_a_json_round_trip() {
    // Projections are read by a UI over the wire, so the state must be JSON.
    let registry = registry();
    registry.observe(&turn_start(2), 2);

    let state = registry.read("turnBoundary").unwrap();
    let text = serde_json::to_string(&state).unwrap();
    let restored: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(restored, state);
}

#[test]
fn a_content_block_event_is_ignored_without_corrupting_state() {
    // The common case: a log holds every kind of event, and each projection reads a few.
    let registry = registry();
    registry.observe(&turn_start(1), 1);

    registry.observe(
        &SessionEvent::AssistantMessage {
            message: ChatMessage::new(
                MessageId::new("m").unwrap(),
                girder_core::ChatRole::Assistant,
                girder_core::MessageSource::Model {
                    provider: "p".to_owned(),
                    model: "m".to_owned(),
                    replay_state: None,
                },
                vec![ContentBlock::Text {
                    text: "hi".to_owned(),
                }],
            ),
        },
        2,
    );

    assert_eq!(registry.read("turnBoundary").unwrap()["lastTurn"], 1);
}
