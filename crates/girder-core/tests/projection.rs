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

// ---- the projection-aware session -------------------------------------------------------------

use girder_core::{MemorySession, ProjectedSession, Session};

fn session() -> Arc<MemorySession> {
    Arc::new(MemorySession::new())
}

#[tokio::test]
async fn attaching_replays_an_existing_log() {
    // A resumed session must reflect the whole log, not only what happens after this process starts.
    let inner = session();
    inner.append(turn_start(1)).await.unwrap();
    inner.append(turn_start(2)).await.unwrap();

    let wrapper =
        ProjectedSession::attach(Arc::clone(&inner) as Arc<dyn Session>, Arc::new(registry()))
            .await
            .expect("attached");

    let state = wrapper.projections().read("turnBoundary").unwrap();
    assert_eq!(state["lastTurn"], 2);
    assert_eq!(wrapper.cursor(), 2);
}

#[tokio::test]
async fn an_append_is_folded() {
    let inner = session();
    let wrapper =
        ProjectedSession::attach(Arc::clone(&inner) as Arc<dyn Session>, Arc::new(registry()))
            .await
            .unwrap();

    wrapper.append(turn_start(1)).await.unwrap();

    assert_eq!(
        wrapper.projections().read("turnBoundary").unwrap()["lastTurn"],
        1
    );
    assert_eq!(wrapper.cursor(), 1);
}

#[tokio::test]
async fn several_appends_fold_in_order() {
    let inner = session();
    let wrapper =
        ProjectedSession::attach(Arc::clone(&inner) as Arc<dyn Session>, Arc::new(registry()))
            .await
            .unwrap();

    wrapper
        .append_all(vec![
            turn_start(1),
            step_start(1, 1),
            turn_end(1),
            turn_start(2),
        ])
        .await
        .unwrap();

    let state = wrapper.projections().read("turnBoundary").unwrap();
    assert_eq!(state["lastTurn"], 2);
    // The last event is `turn_start(2)` at sequence 4, so that turn is open.
    assert_eq!(state["openTurnStartSeq"], 4);
    assert_eq!(wrapper.cursor(), 4);
}

#[tokio::test]
async fn the_wrapper_and_a_direct_replay_agree() {
    // The property that makes the cache trustworthy.
    let inner = session();
    inner.append(turn_start(1)).await.unwrap();
    let wrapper =
        ProjectedSession::attach(Arc::clone(&inner) as Arc<dyn Session>, Arc::new(registry()))
            .await
            .unwrap();
    wrapper.append(step_start(1, 1)).await.unwrap();
    wrapper.append(turn_end(1)).await.unwrap();

    // Replay the log independently.
    let events = inner.replay().await.unwrap();
    let numbered: Vec<(u64, SessionEvent)> = events
        .into_iter()
        .enumerate()
        .map(|(i, e)| (i as u64 + 1, e))
        .collect();
    let fresh = registry();
    fresh.replay(&numbered);

    assert_eq!(wrapper.projections().view(), fresh.view());
}

#[tokio::test]
async fn the_log_and_the_projection_stay_in_step() {
    // Every appended event appears in the log exactly once and advances the cursor once.
    let inner = session();
    let wrapper =
        ProjectedSession::attach(Arc::clone(&inner) as Arc<dyn Session>, Arc::new(registry()))
            .await
            .unwrap();

    for turn in 1..=5 {
        wrapper.append(turn_start(turn)).await.unwrap();
    }

    assert_eq!(inner.replay().await.unwrap().len(), 5);
    assert_eq!(wrapper.cursor(), 5);
    assert_eq!(
        wrapper.projections().read("turnBoundary").unwrap()["lastTurn"],
        5
    );
}

#[tokio::test]
async fn attaching_to_an_empty_log_starts_at_the_initial_state() {
    let inner = session();
    let wrapper =
        ProjectedSession::attach(Arc::clone(&inner) as Arc<dyn Session>, Arc::new(registry()))
            .await
            .unwrap();

    assert_eq!(wrapper.cursor(), 0);
    assert_eq!(
        wrapper.projections().read("turnBoundary").unwrap()["lastTurn"],
        0
    );
}

#[tokio::test]
async fn a_multi_projection_wrapper_folds_them_all() {
    let inner = session();
    let mut projections = ProjectionRegistry::new();
    projections.register(Arc::new(TurnBoundary)).unwrap();
    projections.register(Arc::new(TitleProjection)).unwrap();
    projections
        .register(Arc::new(girder_core::ModelSelectionProjection))
        .unwrap();

    let wrapper = ProjectedSession::attach(
        Arc::clone(&inner) as Arc<dyn Session>,
        Arc::new(projections),
    )
    .await
    .unwrap();

    wrapper.append(turn_start(1)).await.unwrap();
    wrapper
        .append(SessionEvent::SessionTitle {
            title: "a title".to_owned(),
            message_seqs: vec![1],
            source: "human".to_owned(),
        })
        .await
        .unwrap();

    assert_eq!(
        wrapper.projections().read("turnBoundary").unwrap()["lastTurn"],
        1
    );
    assert_eq!(
        wrapper.projections().read("title").unwrap()["title"],
        "a title"
    );
}

#[tokio::test]
async fn the_debug_form_does_not_print_the_state() {
    // A projection can hold a transcript; a debug line should name it, not dump it.
    let inner = session();
    let wrapper =
        ProjectedSession::attach(Arc::clone(&inner) as Arc<dyn Session>, Arc::new(registry()))
            .await
            .unwrap();

    let text = format!("{wrapper:?}");
    assert!(text.contains("ProjectedSession"));
    assert!(text.contains("turnBoundary"));
}
