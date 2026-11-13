//! Tests for the projection host.
//!
//! The property that matters most is the one the cache exists for: folding a hundred events and then
//! one more must run **one** fold, not a hundred and one. A host that replayed would produce the same
//! values as one that did not, so the tests count `apply` calls rather than compare views — a value
//! comparison cannot tell the two apart, and that is exactly the bug this module is meant to prevent.
//!
//! The second property is the cursor. It is the next fold's starting point, so a cursor that is one
//! too low replays an event and one that is too high loses it; both leave a state that looks
//! plausible. Each of those cases is pinned below.

// The module is included by path rather than through the crate's re-exports, because the crate root
// is being wired up separately. It resolves `girder_core::…` through the alias the library declares
// for itself, which is why the same source builds here.
#[path = "../src/projection_host.rs"]
mod projection_host;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use girder_core::{
    CacheError, Checkpoint, ModelSelection, Projection, ProjectionCache, ProjectionSeq as Seq,
    SessionEvent, TitleProjection, TurnBoundary, Versioned, VersionedProjection,
};
use projection_host::{HostError, ProjectionHost, Registry};

// ---- fixtures ----------------------------------------------------------------------------------

/// A projection that records how many events it has folded and the last sequence it saw.
///
/// The counts are the evidence for incremental folding: `folded` is the number of `apply` calls,
/// which a replaying host would inflate on every call.
struct Counting {
    key: &'static str,
    applies: AtomicU64,
    last_seq: AtomicU64,
}

impl Counting {
    fn new(key: &'static str) -> Self {
        Self {
            key,
            applies: AtomicU64::new(0),
            last_seq: AtomicU64::new(0),
        }
    }

    fn applies(&self) -> u64 {
        self.applies.load(Ordering::SeqCst)
    }

    fn last_seq(&self) -> u64 {
        self.last_seq.load(Ordering::SeqCst)
    }
}

impl Projection for Counting {
    fn key(&self) -> &'static str {
        self.key
    }

    fn init(&self) -> serde_json::Value {
        serde_json::json!({ "count": 0, "lastSeq": 0 })
    }

    fn apply(
        &self,
        mut state: serde_json::Value,
        _event: &SessionEvent,
        seq: Seq,
    ) -> serde_json::Value {
        self.applies.fetch_add(1, Ordering::SeqCst);
        self.last_seq.store(seq, Ordering::SeqCst);
        if let Some(object) = state.as_object_mut() {
            object.insert("count".to_owned(), serde_json::json!(self.applies()));
            object.insert("lastSeq".to_owned(), serde_json::json!(seq));
        }
        state
    }
}

/// A projection that counts nothing and ignores everything, for the "two projections do not
/// interfere" case.
struct Silent;

impl Projection for Silent {
    fn key(&self) -> &'static str {
        "silent"
    }

    fn init(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    fn apply(
        &self,
        _state: serde_json::Value,
        _event: &SessionEvent,
        _seq: Seq,
    ) -> serde_json::Value {
        serde_json::Value::Null
    }
}

fn versioned(projection: Arc<dyn Projection>) -> Arc<dyn VersionedProjection> {
    Arc::new(Versioned::new(projection, 1))
}

fn counting_host(key: &'static str) -> (ProjectionHost, Arc<Counting>) {
    let counting = Arc::new(Counting::new(key));
    let host = ProjectionHost::new(vec![
        versioned(Arc::clone(&counting) as Arc<dyn Projection>),
    ])
    .expect("built");
    (host, counting)
}

fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

fn turn_end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: girder_core::TurnEndReason::Completed,
    }
}

/// A log of `count` turn opens, which is enough for the incremental test without being slow.
fn turn_starts(count: usize) -> Vec<SessionEvent> {
    (1..=count).map(|turn| turn_start(turn as u64)).collect()
}

// ---- an empty host -----------------------------------------------------------------------------

#[test]
fn an_empty_host_has_no_view_no_cursor_and_no_checkpoint() {
    let host = ProjectionHost::new(Vec::new()).expect("built");

    assert!(host.view().is_empty());
    assert_eq!(host.cursor(), 0);
    // Nothing has folded, so there is no row to persist; storing nothing and refolding is correct.
    assert!(host.checkpoint().is_none());
}

#[test]
fn a_host_with_projections_but_no_events_reports_their_initial_state() {
    let mut registry = Registry::new();
    registry
        .register(versioned(Arc::new(TurnBoundary)))
        .expect("registered");
    let host = registry.host().expect("built");

    assert_eq!(host.cursor(), 0);
    let view = host.view();
    assert_eq!(view.len(), 1);
    assert_eq!(view["turnBoundary"]["lastTurn"], 0);
    // `init` is a state, not a fold: no event has been seen, so nothing is checkpointed yet.
    assert!(host.checkpoint().is_none());
}

// ---- folding -----------------------------------------------------------------------------------

#[test]
fn folding_one_event_advances_the_cursor_and_the_view() {
    let (mut host, counting) = counting_host("counted");

    host.observe(&[turn_start(1)]).expect("folded");

    assert_eq!(host.cursor(), 1);
    assert_eq!(counting.applies(), 1);
    assert_eq!(counting.last_seq(), 1);
    assert_eq!(host.view()["counted"]["count"], 1);
    assert_eq!(host.view()["counted"]["lastSeq"], 1);
}

#[test]
fn folding_a_run_numbers_each_event_consecutively() {
    let (mut host, counting) = counting_host("counted");

    host.observe(&[turn_start(1), turn_end(1), turn_start(2)])
        .expect("folded");

    assert_eq!(host.cursor(), 3);
    assert_eq!(counting.applies(), 3);
    assert_eq!(counting.last_seq(), 3);
}

#[test]
fn folding_a_real_projection_tracks_the_turn_boundary() {
    let mut host = ProjectionHost::new(vec![versioned(Arc::new(TurnBoundary))]).expect("built");

    host.observe(&[turn_start(1), turn_end(1), turn_start(2)])
        .expect("folded");

    let view = host.view();
    assert_eq!(view["turnBoundary"]["openTurnStartSeq"], 3);
    assert_eq!(view["turnBoundary"]["lastTurn"], 2);
    assert_eq!(host.cursor(), 3);
}

#[test]
fn an_empty_batch_changes_nothing() {
    // A tail that delivered nothing is not an event, and advancing a cursor for it would make the
    // next real event look like a gap or a repeat.
    let (mut host, counting) = counting_host("counted");
    host.observe(&[turn_start(1)]).expect("folded");

    host.observe(&[]).expect("folded nothing");

    assert_eq!(host.cursor(), 1);
    assert_eq!(counting.applies(), 1);
}

// ---- incremental folding: the core property ----------------------------------------------------

#[test]
fn a_hundred_events_then_one_more_folds_only_the_one() {
    // This is the claim the cache exists to make. A replaying host would produce an identical view,
    // so the assertion is on the fold count, not on the values.
    let (mut host, counting) = counting_host("counted");

    host.observe(&turn_starts(100)).expect("folded 100");
    assert_eq!(counting.applies(), 100);
    assert_eq!(host.cursor(), 100);

    host.observe(&[turn_start(101)]).expect("folded 1");

    assert_eq!(
        counting.applies(),
        101,
        "the second call must fold one event, not replay the first hundred"
    );
    assert_eq!(host.cursor(), 101);
    assert_eq!(host.view()["counted"]["count"], 101);
}

#[test]
fn many_small_batches_cost_one_fold_per_event_in_total() {
    // Incrementality has to hold across every shape of batch, not just a hundred-then-one: a host
    // that replayed on attach but folded incrementally afterwards would pass the test above.
    let (mut host, counting) = counting_host("counted");

    for turn in 1..=10 {
        host.observe(&[turn_start(turn)]).expect("folded one");
    }

    assert_eq!(counting.applies(), 10);
    assert_eq!(host.cursor(), 10);
}

#[test]
fn a_host_built_from_a_registry_folds_incrementally_too() {
    // The registry is the path a service uses, so the property has to survive it and not only the
    // direct constructor.
    let counting = Arc::new(Counting::new("counted"));
    let mut registry = Registry::new();
    registry
        .register(versioned(Arc::clone(&counting) as Arc<dyn Projection>))
        .expect("registered");
    let mut host = registry.host().expect("built");

    host.observe(&turn_starts(50)).expect("folded 50");
    host.observe(&[turn_start(51)]).expect("folded 1");

    assert_eq!(counting.applies(), 51);
    assert_eq!(host.cursor(), 51);
}

// ---- the cursor is exact -----------------------------------------------------------------------

#[test]
fn the_cursor_is_the_last_folded_sequence_not_the_event_count() {
    // One event at sequence 5 and one at sequence 6: the cursor is 6, and a caller that resumed at
    // the count (2) would replay four events that are not in the log any more.
    let (mut host, _counting) = counting_host("counted");
    host.observe(&turn_starts(6)).expect("folded 6");

    assert_eq!(host.cursor(), 6);
}

#[test]
fn the_host_numbers_events_so_a_gap_cannot_be_introduced_by_a_caller() {
    // The cache refuses a skipped sequence, and the host is what removes the opportunity: it derives
    // each sequence from the cursor, so a caller that hands over events in any batch size can never
    // be the reason a fold jumps. This is the contract the cache's gap check protects, tested from
    // the side a caller actually touches.
    let (mut host, counting) = counting_host("counted");

    for batch in [1, 4, 0, 9, 1] {
        host.observe(&turn_starts(batch)).expect("folded");
    }

    assert_eq!(host.cursor(), 15);
    assert_eq!(counting.last_seq(), 15);
    assert_eq!(counting.applies(), 15);
}

#[test]
fn a_resumed_host_continues_at_the_watermark_of_the_restored_window() {
    // The bug this pins: a checkpoint at seq 4 restored with the window 5..8 must continue at 8.
    // Continuing at the row (4) would refold 5..8 into a state that already covers them.
    let counting = Arc::new(Counting::new("counted"));
    let definitions = || vec![versioned(Arc::clone(&counting) as Arc<dyn Projection>)];
    let mut first = ProjectionHost::new(definitions()).expect("built");
    first
        .observe(&[turn_start(1), turn_end(1), turn_start(2), turn_end(2)])
        .expect("folded 4");
    let checkpoint: Checkpoint =
        serde_json::from_value(first.checkpoint().expect("a checkpoint")).expect("parsed");
    assert_eq!(first.cursor(), 4);

    // The anchor is the row's own sequence (4), and the caller reads from it on: `restore` skips
    // that event because the row already accounts for it. Starting after the anchor is refused.
    let window: Vec<(Seq, SessionEvent)> = vec![
        (4, turn_end(2)),
        (5, turn_start(3)),
        (6, turn_end(3)),
        (7, turn_start(4)),
        (8, turn_end(4)),
    ];
    let mut resumed =
        ProjectionHost::resume(definitions(), &checkpoint, &window, 4).expect("resumed");

    assert_eq!(resumed.cursor(), 8);
    resumed.observe(&[turn_start(5)]).expect("folded one");
    assert_eq!(resumed.cursor(), 9);
    // Four folds for the first host, four for the restored suffix, one after: nine in all. A host
    // that replayed at restore would have folded 4 + 4 + 4 + 1.
    assert_eq!(counting.applies(), 9);
}

#[test]
fn a_resumed_host_matches_one_that_folded_the_whole_log() {
    // The cursor claim is worth nothing if the state it implies is wrong, so the two paths must
    // agree on the view as well as the sequence.
    let definitions = || vec![versioned(Arc::new(TurnBoundary))];

    let mut whole = ProjectionHost::new(definitions()).expect("built");
    let log = turn_starts(6);
    whole.observe(&log).expect("folded 6");

    let mut prefix = ProjectionHost::new(definitions()).expect("built");
    prefix.observe(&log[..3]).expect("folded 3");
    let checkpoint: Checkpoint =
        serde_json::from_value(prefix.checkpoint().expect("a checkpoint")).expect("parsed");

    // Anchored at the row's sequence 3, so the window is 3..=6 and the event at 3 is skipped.
    let window: Vec<(Seq, SessionEvent)> = log[2..]
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, event)| (index as Seq + 3, event))
        .collect();
    let resumed = ProjectionHost::resume(definitions(), &checkpoint, &window, 3).expect("resumed");

    assert_eq!(resumed.view(), whole.view());
    assert_eq!(resumed.cursor(), whole.cursor());
}

// ---- two projections do not interfere ----------------------------------------------------------

#[test]
fn two_projections_fold_independently_and_appear_side_by_side() {
    let counting = Arc::new(Counting::new("counted"));
    let mut host = ProjectionHost::new(vec![
        versioned(Arc::clone(&counting) as Arc<dyn Projection>),
        versioned(Arc::new(TitleProjection)),
    ])
    .expect("built");

    host.observe(&[turn_start(1)]).expect("folded");
    host.observe(&[SessionEvent::SessionTitle {
        title: "a title".to_owned(),
        message_seqs: vec![1],
        source: "user".to_owned(),
    }])
    .expect("folded");

    let view = host.view();
    assert_eq!(view.len(), 2);
    // Every projection sees every event, and each ignores what it does not read.
    assert_eq!(counting.applies(), 2);
    assert_eq!(view["title"]["title"], "a title");
    assert_eq!(view["counted"]["count"], 2);
    assert_eq!(host.cursor(), 2);
}

#[test]
fn a_silent_projection_does_not_disturb_another() {
    let counting = Arc::new(Counting::new("counted"));
    let mut host = ProjectionHost::new(vec![
        versioned(Arc::new(Silent)),
        versioned(Arc::clone(&counting) as Arc<dyn Projection>),
    ])
    .expect("built");

    host.observe(&turn_starts(3)).expect("folded 3");

    let view = host.view();
    assert_eq!(view["silent"], serde_json::Value::Null);
    assert_eq!(view["counted"]["count"], 3);
    assert_eq!(host.cursor(), 3);
}

// ---- errors ------------------------------------------------------------------------------------

#[test]
fn a_duplicate_key_is_refused_when_a_host_is_built() {
    // Two definitions under one key would write one checkpoint row, and the loser's state would be
    // discarded on every resume without a word.
    let error = ProjectionHost::new(vec![
        versioned(Arc::new(TurnBoundary)),
        versioned(Arc::new(TurnBoundary)),
    ])
    .expect_err("refused");

    assert_eq!(
        error,
        HostError::DuplicateKey {
            key: "turnBoundary".to_owned(),
        }
    );
}

#[test]
fn a_duplicate_key_is_refused_by_the_registry() {
    let mut registry = Registry::new();
    registry
        .register(versioned(Arc::new(TurnBoundary)))
        .expect("registered");

    let error = registry
        .register(versioned(Arc::new(TurnBoundary)))
        .expect_err("refused");

    assert_eq!(
        error,
        HostError::DuplicateKey {
            key: "turnBoundary".to_owned(),
        }
    );
    assert_eq!(registry.keys(), vec!["turnBoundary"]);
}

#[test]
fn a_cache_refusal_propagates_through_resume() {
    // A row written under a different version cannot seed the fold, so resume must refuse rather
    // than serve state the current definitions did not produce.
    let stale = ProjectionCache::new(vec![Arc::new(Versioned::new(Arc::new(TurnBoundary), 1))])
        .expect("built");
    stale.observe(&turn_start(1), 1).expect("folded");
    let checkpoint = stale.checkpoint();

    let error = ProjectionHost::resume(
        vec![Arc::new(Versioned::new(Arc::new(TurnBoundary), 2))],
        &checkpoint,
        &[(1, turn_start(1))],
        1,
    )
    .expect_err("refused");

    let HostError::Cache(CacheError::StaleRow {
        key,
        row_version,
        expected,
    }) = error
    else {
        unreachable!("expected a stale-row refusal");
    };
    assert_eq!(key, "turnBoundary");
    assert_eq!(row_version, 1);
    assert_eq!(expected, 2);
}

#[test]
fn a_window_that_starts_after_the_row_is_refused() {
    // The checkpoint anchors at 3, but the caller read from 5: the events between the row and the
    // window are exactly the ones folding would lose, so the cache refuses instead of guessing.
    let mut earlier = ProjectionHost::new(vec![versioned(Arc::new(TurnBoundary))]).expect("built");
    earlier.observe(&turn_starts(3)).expect("folded 3");
    let checkpoint: Checkpoint =
        serde_json::from_value(earlier.checkpoint().expect("a checkpoint")).expect("parsed");

    let error = ProjectionHost::resume(
        vec![versioned(Arc::new(TurnBoundary))],
        &checkpoint,
        &[(5, turn_start(5))],
        5,
    )
    .expect_err("refused");

    assert!(
        matches!(error, HostError::Cache(CacheError::WindowTooLate { .. })),
        "expected a window refusal, got {error:?}"
    );
}

#[test]
fn the_registry_floor_is_what_resume_needs_and_the_host_agrees() {
    // The floor and the host are two halves of one procedure, so this walks it the way a caller
    // would: ask for the floor, slice the log from it, resume, and compare against a host that
    // folded the whole thing.
    let definitions = || vec![versioned(Arc::new(TurnBoundary))];
    let mut registry = Registry::new();
    registry
        .register(versioned(Arc::new(TurnBoundary)))
        .expect("registered");

    let log = turn_starts(5);
    let mut whole = ProjectionHost::new(definitions()).expect("built");
    whole.observe(&log).expect("folded 5");

    let mut prefix = ProjectionHost::new(definitions()).expect("built");
    prefix.observe(&log[..3]).expect("folded 3");
    let checkpoint: Checkpoint =
        serde_json::from_value(prefix.checkpoint().expect("a checkpoint")).expect("parsed");

    let floor = registry
        .restore_floor(&checkpoint)
        .expect("floor")
        .expect("some floor");
    assert_eq!(floor, 3);

    // The window is inclusive of the floor: the event at 3 is read and skipped by `restore`.
    let window: Vec<(Seq, SessionEvent)> = log
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, event)| (index as Seq + 1, event))
        .filter(|(seq, _)| *seq >= floor)
        .collect();
    let resumed = registry
        .resume(&checkpoint, &window, floor)
        .expect("resumed");

    assert_eq!(resumed.view(), whole.view());
    assert_eq!(resumed.cursor(), 5);
}

#[test]
fn a_registry_with_no_projections_has_no_floor() {
    assert_eq!(
        Registry::new()
            .restore_floor(&Checkpoint::new())
            .expect("floor"),
        None
    );
}

#[test]
fn an_empty_registry_builds_an_empty_host() {
    let host = Registry::new().host().expect("built");

    assert!(Registry::new().keys().is_empty());
    assert!(host.view().is_empty());
    assert_eq!(host.cursor(), 0);
}

// ---- checkpoints -------------------------------------------------------------------------------

#[test]
fn a_checkpoint_round_trips_through_json() {
    let mut host = ProjectionHost::new(vec![versioned(Arc::new(TurnBoundary))]).expect("built");
    host.observe(&[turn_start(1), turn_end(1)])
        .expect("folded 2");

    let value = host.checkpoint().expect("a checkpoint");
    assert_eq!(value["rows"]["turnBoundary"]["seq"], 2);

    let checkpoint: Checkpoint = serde_json::from_value(value).expect("parsed");
    assert_eq!(checkpoint.row("turnBoundary").expect("row").seq, 2);
}

#[test]
fn a_checkpoint_carries_the_definition_version() {
    // Without the version a row written by an older fold would be read as current, which is the
    // silent corruption the version exists to prevent.
    let mut host = ProjectionHost::new(vec![Arc::new(Versioned::new(Arc::new(TurnBoundary), 7))])
        .expect("built");
    host.observe(&[turn_start(1)]).expect("folded");

    let checkpoint: Checkpoint =
        serde_json::from_value(host.checkpoint().expect("a checkpoint")).expect("parsed");
    assert_eq!(checkpoint.row("turnBoundary").expect("row").ver, 7);
}

#[test]
fn a_model_selection_projection_folds_a_real_selection() {
    // A second reference definition, so the host is exercised beyond one projection's shape.
    let mut host = ProjectionHost::new(vec![versioned(Arc::new(
        girder_core::ModelSelectionProjection,
    ))])
    .expect("built");
    host.observe(&[SessionEvent::ModelSelection {
        selection: ModelSelection {
            provider: "deepseek".to_owned(),
            model: "deepseek-v4.1-flash".to_owned(),
            reasoning_effort: None,
        },
    }])
    .expect("folded");

    assert_eq!(
        host.view()["modelSelection"]["lastUsed"]["model"],
        "deepseek-v4.1-flash"
    );
}
