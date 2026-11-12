//! Tests for versioned projection caches.
//!
//! The property that matters is the one a cache exists to claim: resuming from a checkpoint plus the
//! log suffix must produce exactly the state a full replay from `init` produces. Everything else here
//! is about the cases where that cannot hold, and the point of each is that the cache refuses instead
//! of writing a plausible but wrong state.

// The module is included by path rather than through the crate's re-exports, because the crate root
// has not been wired to it yet. It resolves `girder_core::…` through the alias the library declares
// for itself, which is why the same source builds here.
#[path = "../src/projection_extra.rs"]
mod projection_extra;

use std::sync::Arc;

use girder_core::{Projection, ProjectionSeq as Seq, SessionEvent, TurnBoundary};
use projection_extra::{
    CacheError, Checkpoint, DuplicateKey, ProjectionCache, Versioned, VersionedProjection,
};

// ---- fixtures ----------------------------------------------------------------------------------

fn cache_at(version: u64) -> ProjectionCache {
    ProjectionCache::new(vec![Arc::new(Versioned::new(
        Arc::new(TurnBoundary),
        version,
    ))])
    .expect("built")
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

/// A four-event log: two turns, each opening and closing.
fn log() -> Vec<(Seq, SessionEvent)> {
    vec![
        (1, turn_start(1)),
        (2, turn_end(1)),
        (3, turn_start(2)),
        (4, turn_end(2)),
    ]
}

fn folded(log: &[(Seq, SessionEvent)]) -> ProjectionCache {
    let cache = cache_at(1);
    for (seq, event) in log {
        cache.observe(event, *seq).expect("folded");
    }
    cache
}

/// A projection whose state grows without bound, so a suffix fold is measurably different from a full
/// one. `TurnBoundary` is too small for that: its state is the same size at every sequence.
struct Counter;

impl Projection for Counter {
    fn key(&self) -> &'static str {
        "counter"
    }

    fn init(&self) -> serde_json::Value {
        serde_json::json!({ "seen": [] })
    }

    fn apply(
        &self,
        mut state: serde_json::Value,
        event: &SessionEvent,
        seq: Seq,
    ) -> serde_json::Value {
        if let Some(object) = state.as_object_mut() {
            if let Some(seen) = object
                .get_mut("seen")
                .and_then(serde_json::Value::as_array_mut)
            {
                seen.push(serde_json::json!({ "seq": seq, "kind": event.type_name() }));
            }
        }
        state
    }
}

fn counter_cache() -> ProjectionCache {
    ProjectionCache::new(vec![Arc::new(Versioned::new(Arc::new(Counter), 1))]).expect("built")
}

// ---- the claim ---------------------------------------------------------------------------------

#[test]
fn a_checkpoint_carries_the_state_the_watermark_and_the_version() {
    let cache = folded(&log());
    let checkpoint = cache.checkpoint();

    let row = checkpoint
        .row("turnBoundary")
        .expect("a row per projection");
    assert_eq!(row.ver, 1);
    assert_eq!(row.seq, 4);
    assert_eq!(row.val["lastTurn"], 2);
    // Between turns, so the open-turn marker is null; the row is the state, not a re-derivation.
    assert!(row.val["openTurnStartSeq"].is_null());
}

#[test]
fn resuming_from_a_checkpoint_equals_a_full_replay() {
    // The whole reason a checkpoint exists: the two paths must not disagree.
    let whole = folded(&log());
    let checkpoint = whole.checkpoint();

    let resumed = cache_at(1);
    let floor = resumed.restore_floor(&checkpoint).expect("a floor");
    assert_eq!(floor, 4, "one projection at seq 4 anchors at its own seq");

    // The reader supplies the window from the floor on. Restoring consumes it, and the result must be
    // the state the whole log produces.
    let window = &log()[3..];
    let snapshot = resumed
        .restore(&checkpoint, window, floor)
        .expect("restored");

    assert_eq!(snapshot.as_of, 4);
    assert_eq!(snapshot.values, whole.snapshot().values);
    assert_eq!(resumed.read("turnBoundary"), whole.read("turnBoundary"));
}

#[test]
fn a_missing_row_anchors_the_floor_at_the_start_of_the_log() {
    // One stale key costs a full refold, and paying it is cheaper than serving a wrong state.
    let cache = cache_at(1);
    assert_eq!(cache.restore_floor(&Checkpoint::new()), Some(0));
}

#[test]
fn the_floor_is_the_lowest_anchor_across_projections() {
    // One window has to satisfy every projection, so it starts where the least advanced one can.
    let cache = ProjectionCache::new(vec![
        Arc::new(Versioned::new(Arc::new(TurnBoundary), 1)),
        Arc::new(Versioned::new(Arc::new(Counter), 1)),
    ])
    .expect("built");
    for (seq, event) in log() {
        cache.observe(&event, seq).expect("folded");
    }
    let mut checkpoint = cache.checkpoint();
    checkpoint.rows.get_mut("counter").expect("counter row").seq = 2;

    assert_eq!(cache.restore_floor(&checkpoint), Some(2));
}

#[test]
fn restoring_a_suffix_applies_only_the_events_the_row_does_not_cover() {
    // A growing state is the case where applying an event twice is visible rather than masked by an
    // idempotent overwrite.
    let whole = {
        let cache = counter_cache();
        for (seq, event) in log() {
            cache.observe(&event, seq).expect("folded");
        }
        cache
    };

    let first = counter_cache();
    for (seq, event) in &log()[..2] {
        first.observe(event, *seq).expect("folded");
    }
    let checkpoint = first.checkpoint();
    let floor = first.restore_floor(&checkpoint).expect("a floor");
    assert_eq!(floor, 2);

    let resumed = counter_cache();
    let snapshot = resumed
        .restore(&checkpoint, &log()[1..], floor)
        .expect("restored");
    assert_eq!(snapshot.values, whole.snapshot().values);

    let seen = resumed.read("counter").expect("present");
    let entries = seen["seen"].as_array().expect("an array");
    assert_eq!(entries.len(), 4, "four events, each folded once");
    assert_eq!(entries[0]["seq"], 1);
    assert_eq!(entries[3]["seq"], 4);
}

// ---- refusals ----------------------------------------------------------------------------------

#[test]
fn a_row_from_another_version_pulls_the_floor_back_to_the_start() {
    // Old rows stay syntactically valid after a fold changes meaning, so only the version can reject
    // them. The failure it prevents is silent: the state looks like JSON either way.
    let cache = folded(&log());
    let checkpoint = cache.checkpoint();

    let upgraded = cache_at(2);
    assert_eq!(upgraded.restore_floor(&checkpoint), Some(0));
}

#[test]
fn restoring_a_suffix_onto_a_stale_row_is_refused_rather_than_refolded() {
    // Refolding from `init` is only sound over the whole log. Over a suffix it would fold the tail onto
    // a state that never happened, so this has to be an error and not a silent reset.
    let cache = folded(&log());
    let checkpoint = cache.checkpoint();

    let upgraded = cache_at(2);
    let error = upgraded
        .restore(&checkpoint, &log()[3..], 4)
        .expect_err("refused");

    assert_eq!(
        error,
        CacheError::StaleRow {
            key: "turnBoundary".to_owned(),
            row_version: 1,
            expected: 2,
        }
    );
}

#[test]
fn a_row_claiming_more_history_than_the_log_holds_is_refused() {
    // A shrunk log is a crash-repair truncation. Folding what remains on top of a state that already
    // covers the lost events would double-apply them.
    let cache = folded(&log());
    let mut checkpoint = cache.checkpoint();
    checkpoint.rows.get_mut("turnBoundary").expect("row").seq = 10;

    let error = cache
        .restore(&checkpoint, &log()[3..], 4)
        .expect_err("refused");

    assert_eq!(
        error,
        CacheError::RowBeyondLogEnd {
            key: "turnBoundary".to_owned(),
            row_seq: 10,
            end_seq: 4,
        }
    );
}

#[test]
fn a_window_that_starts_after_the_row_is_refused() {
    // The caller read from the wrong offset. Seeding from the row would skip the events in between and
    // produce a state that never occurred, and there is no way to tell that apart from success later.
    let checkpoint = Checkpoint::new();
    let cache = cache_at(1);

    let error = cache
        .restore(&checkpoint, &log()[2..], 3)
        .expect_err("refused");

    assert_eq!(
        error,
        CacheError::WindowTooLate {
            key: "turnBoundary".to_owned(),
            base_seq: 3,
            row_seq: None,
        }
    );
}

#[test]
fn a_window_with_a_hole_is_refused() {
    let cache = cache_at(1);
    let holed = vec![(1, turn_start(1)), (3, turn_start(2))];

    let error = cache
        .restore(&Checkpoint::new(), &holed, 0)
        .expect_err("refused");

    assert_eq!(
        error,
        CacheError::MissingSeq {
            key: "turnBoundary".to_owned(),
            expected: 2,
            found: 3,
        }
    );
}

// ---- the live drive ----------------------------------------------------------------------------

#[test]
fn delivering_an_event_twice_folds_it_once() {
    // The caller is a subscription, and a subscription can repeat.
    let cache = counter_cache();
    for (seq, event) in &log()[..2] {
        cache.observe(event, *seq).expect("folded");
    }
    cache.observe(&log()[1].1, 2).expect("duplicate accepted");

    let entries = cache.read("counter").expect("present");
    assert_eq!(entries["seen"].as_array().expect("array").len(), 2);
    assert_eq!(cache.snapshot().as_of, 2);
}

#[test]
fn skipping_a_sequence_number_is_refused() {
    // The registry tolerates a jump because nothing it holds is persisted. A cache cannot: the wrong
    // state would be written to disk and outlive the mistake.
    let cache = cache_at(1);
    cache.observe(&turn_start(1), 1).expect("folded");

    let error = cache.observe(&turn_start(2), 3).expect_err("refused");

    assert_eq!(
        error,
        CacheError::Gap {
            key: "turnBoundary".to_owned(),
            folded: 1,
            found: 3,
        }
    );
}

#[test]
fn a_refused_gap_leaves_every_projection_untouched() {
    // Half a fold would give the cache two watermarks for one event, and then no checkpoint it writes
    // would be correct.
    let cache = ProjectionCache::new(vec![
        Arc::new(Versioned::new(Arc::new(TurnBoundary), 1)),
        Arc::new(Versioned::new(Arc::new(Counter), 1)),
    ])
    .expect("built");
    cache.observe(&turn_start(1), 1).expect("folded");

    assert!(cache.observe(&turn_end(1), 3).is_err());

    assert_eq!(cache.snapshot().as_of, 1);
    assert_eq!(
        cache.read("counter").expect("present")["seen"]
            .as_array()
            .expect("array")
            .len(),
        1
    );
}

// ---- the zero-I/O rung -------------------------------------------------------------------------

#[test]
fn a_checkpoint_can_be_viewed_without_reading_the_log() {
    let checkpoint = folded(&log()).checkpoint();
    let cold = cache_at(1);

    let values = cold.view_checkpoint(&checkpoint);

    assert_eq!(values.as_of, 4);
    assert_eq!(values.get("turnBoundary").expect("present")["lastTurn"], 2);
    // Nothing was folded: the read served the persisted row and left the cache alone.
    assert!(cold.read("turnBoundary").is_none());
    assert_eq!(cold.snapshot().as_of, 0);
}

#[test]
fn a_stale_row_is_omitted_from_a_checkpoint_view() {
    // As stale as its row, never wrong: an omitted key is a caller's signal to refold.
    let checkpoint = folded(&log()).checkpoint();

    let values = cache_at(2).view_checkpoint(&checkpoint);

    assert_eq!(values.as_of, 0);
    assert!(values.get("turnBoundary").is_none());
}

#[test]
fn a_checkpoint_round_trips_through_json() {
    // It crosses a storage boundary, so the wire form is load-bearing.
    let checkpoint = folded(&log()).checkpoint();
    let text = serde_json::to_string(&checkpoint).expect("serialized");
    let restored: Checkpoint = serde_json::from_str(&text).expect("deserialized");

    assert_eq!(restored, checkpoint);
    assert_eq!(
        cache_at(1).restore_floor(&restored),
        Some(4),
        "a round-tripped checkpoint is still usable"
    );
}

#[test]
fn a_hand_written_checkpoint_deserializes() {
    // The shape a durable store would hand back, spelled out rather than produced by `checkpoint()`, so
    // the field names are pinned by a test and not only by the serializer.
    let text = r#"{
        "rows": {
            "turnBoundary": {
                "ver": 1,
                "seq": 4,
                "val": {
                    "openTurnStartSeq": null,
                    "lastStepStartSeq": null,
                    "lastTurn": 2
                }
            }
        }
    }"#;
    let checkpoint: Checkpoint = serde_json::from_str(text).expect("deserialized");

    assert_eq!(cache_at(1).restore_floor(&checkpoint), Some(4));
    assert!(!checkpoint.is_empty());
    assert_eq!(checkpoint.row("turnBoundary").expect("row").seq, 4);
}

// ---- construction ------------------------------------------------------------------------------

#[test]
fn a_duplicate_key_is_refused_at_construction() {
    // Two definitions under one key would write one row, and the loser's state would vanish on every
    // restore.
    let error = ProjectionCache::new(vec![
        Arc::new(Versioned::new(Arc::new(TurnBoundary), 1)),
        Arc::new(Versioned::new(Arc::new(TurnBoundary), 1)),
    ])
    .expect_err("refused");

    assert_eq!(
        error,
        DuplicateKey {
            key: "turnBoundary".to_owned(),
        }
    );
}

#[test]
fn an_empty_cache_has_no_floor_and_an_empty_snapshot() {
    let cache = ProjectionCache::new(Vec::new()).expect("built");

    assert_eq!(cache.restore_floor(&Checkpoint::new()), None);
    assert_eq!(cache.snapshot().as_of, 0);
    assert!(cache.keys().is_empty());
}

#[test]
fn keys_are_sorted() {
    let cache = ProjectionCache::new(vec![
        Arc::new(Versioned::new(Arc::new(TurnBoundary), 1)),
        Arc::new(Versioned::new(Arc::new(Counter), 1)),
    ])
    .expect("built");

    assert_eq!(cache.keys(), vec!["counter", "turnBoundary"]);
}

#[test]
fn the_adapter_keeps_the_projection_reachable() {
    let versioned = Versioned::new(Arc::new(TurnBoundary), 7);

    assert_eq!(versioned.key(), "turnBoundary");
    assert_eq!(VersionedProjection::state_version(&versioned), 7);
    assert_eq!(versioned.projection().key(), "turnBoundary");
}
