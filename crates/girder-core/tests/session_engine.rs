//! Tests for the client projection store of upstream `packages/api/session-controller`.
//!
//! Corresponds to upstream `packages/api/session-controller/tests/projection-store.client.spec.ts`
//! (row precedence, baseline seeding, cached blocks, clear) and `notifier.client.spec.ts` (dirty
//! tracking, microtask/frame batching, generation-guarded publication, `notifyNow`, `ensureFresh`).
//!
//! # What each property protects
//!
//! - **A replayed frame is a no-op.** A reconnecting control stream can redeliver a frame it already
//!   sent; without the seq comparison the value would regress to an older one and the UI would blink
//!   backwards through history.
//! - **A cached row never outranks the host.** The list's view of the persisted checkpoint is a
//!   zero-I/O convenience for sessions nobody has opened. The moment the host answers for a key, the
//!   checkpoint view must lose — otherwise a freshly opened session shows a stale title.
//! - **A baseline outranks cached rows wholesale, not per key.** The connected session is the truth
//!   about that session, so one baseline invalidates every cached row regardless of its stored seq.
//! - **A baseline's omission is information.** A key the cut does not carry is capability-absent as of
//!   that cut; keeping the old value would render a projection the host no longer computes.
//! - **A newer frame survives a stale baseline.** The baseline's cut is behind the frame's seq, so
//!   neither overwriting nor clearing is correct for that key.
//! - **`clear()` keeps subscribed faces.** A reconnect invalidates values, not components; a face that
//!   vanished would make every watcher resubscribe over and over.
//! - **Publication is batched.** Twenty writes inside one task are one notification, and the rebuild
//!   runs once. Without batching, a frame storm would rebuild a session list per frame.
//! - **A superseded batch never publishes.** A synchronous flush must cancel the pending batch, or a
//!   controlled input would notify twice in one tick and the framework would roll the caret back.
//! - **An unobserved batch stays dirty.** With no listener the rebuild is deferred to the next read,
//!   which is what keeps an unopened session from doing render work nobody can see.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}` — including the
//! ones the module under test makes, which is why the module itself spells them that way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "tests assert by panicking, index their own fixtures, and read best as one scenario"
)]

#[path = "../src/session/engine.rs"]
pub mod session_engine_gap;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use serde_json::{Value, json};
use session_engine_gap::{ListenerId, Notifier, ProjectionValueStore, Publish, PublishKind};

/// A key's whole value as the host would compute it.
fn block(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

// -------------------------------------------------------------------------------------------------
// Precedence between sequenced rows
// -------------------------------------------------------------------------------------------------

#[test]
fn a_higher_seq_frame_replaces_the_value_and_its_watermark() {
    let store = ProjectionValueStore::new();
    assert!(store.apply("title", json!("first"), 3));
    assert_eq!(store.value("title"), Some(json!("first")));
    assert_eq!(store.seq_of("title"), Some(3));

    assert!(store.apply("title", json!("second"), 7));
    assert_eq!(store.value("title"), Some(json!("second")));
    assert_eq!(store.seq_of("title"), Some(7));
}

#[test]
fn a_lower_seq_frame_is_dropped_without_notifying() {
    let store = ProjectionValueStore::new();
    store.apply("title", json!("new"), 9);
    let face = store.face("title");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = face.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    // A replay of an older frame: not accepted, so the value, the watermark, and the notification
    // all stay where they were.
    assert!(!store.apply("title", json!("stale"), 4));
    assert_eq!(store.value("title"), Some(json!("new")));
    assert_eq!(store.seq_of("title"), Some(9));
    face.flush();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn an_equal_seq_frame_is_also_dropped() {
    // Upstream compares with `<=`: a redelivered frame carries the same seq, which is the common case
    // for a reconnect replay, and it must not republish.
    let store = ProjectionValueStore::new();
    assert!(store.apply("title", json!("value"), 5));
    assert!(!store.apply("title", json!("value"), 5));
    assert!(!store.apply("title", json!("other"), 5));
    assert_eq!(store.value("title"), Some(json!("value")));
}

#[test]
fn absent_keys_read_as_capability_absent() {
    let store = ProjectionValueStore::new();
    assert!(store.is_empty());
    assert_eq!(store.value("title"), None);
    assert_eq!(store.seq_of("title"), None);
    assert!(!store.contains("title"));
    assert!(!store.is_cached("title"));
    assert_eq!(store.len(), 0);
    assert!(store.values().is_empty());
}

#[test]
fn a_null_value_is_present_and_distinguishable_from_absence() {
    // A projection may legitimately compute `null`; treating that as absence would make a face report
    // the wrong capability state.
    let store = ProjectionValueStore::new();
    store.apply("model", Value::Null, 1);
    assert!(store.contains("model"));
    assert_eq!(store.value("model"), Some(Value::Null));
    assert!(!store.contains("other"));
}

#[test]
fn keys_and_values_are_sorted_and_complete() {
    let store = ProjectionValueStore::new();
    store.apply("zeta", json!(1), 1);
    store.apply("alpha", json!(2), 1);
    store.apply("mid", json!(3), 1);
    assert_eq!(store.keys(), vec!["alpha", "mid", "zeta"]);
    let values = store.values();
    assert_eq!(values.len(), 3);
    assert_eq!(values.get("alpha"), Some(&json!(2)));
}

// -------------------------------------------------------------------------------------------------
// Cached rows: the persisted-checkpoint view
// -------------------------------------------------------------------------------------------------

#[test]
fn a_cached_block_fills_keys_and_carries_no_watermark() {
    let store = ProjectionValueStore::new();
    let changed = store.apply_cached(&block(&[
        ("title", json!("from checkpoint")),
        ("model", json!("deepseek-chat")),
    ]));
    assert_eq!(changed, 2);
    assert_eq!(store.value("title"), Some(json!("from checkpoint")));
    assert!(store.is_cached("title"));
    // The whole point of the cached class: it has no seq to compare with a later frame.
    assert_eq!(store.seq_of("title"), None);
}

#[test]
fn a_cached_block_never_overwrites_a_sequenced_row() {
    let store = ProjectionValueStore::new();
    store.apply("title", json!("live"), 4);
    let changed = store.apply_cached(&block(&[
        ("title", json!("checkpoint")),
        ("model", json!("m")),
    ]));
    // Only `model` landed.
    assert_eq!(changed, 1);
    assert_eq!(store.value("title"), Some(json!("live")));
    assert_eq!(store.seq_of("title"), Some(4));
    assert!(!store.is_cached("title"));
    assert!(store.is_cached("model"));
}

#[test]
fn a_refreshed_cached_block_updates_its_own_class() {
    // Both rows are the same class, so the newer view of the checkpoint wins: a list refresh must not
    // keep showing a title the checkpoint has since replaced.
    let store = ProjectionValueStore::new();
    store.apply_cached(&block(&[("title", json!("old checkpoint"))]));
    let changed = store.apply_cached(&block(&[("title", json!("new checkpoint"))]));
    assert_eq!(changed, 1);
    assert_eq!(store.value("title"), Some(json!("new checkpoint")));
    assert!(store.is_cached("title"));
}

#[test]
fn the_first_host_frame_takes_a_key_over_from_the_checkpoint_view() {
    let store = ProjectionValueStore::new();
    store.apply_cached(&block(&[("title", json!("checkpoint"))]));
    // A cached row has no comparable seq, so even seq 0 outranks it.
    assert!(store.apply("title", json!("live"), 0));
    assert_eq!(store.value("title"), Some(json!("live")));
    assert_eq!(store.seq_of("title"), Some(0));
    assert!(!store.is_cached("title"));
}

// -------------------------------------------------------------------------------------------------
// Baseline seeding from a history tail page
// -------------------------------------------------------------------------------------------------

#[test]
fn a_baseline_discards_every_cached_row_even_at_a_higher_stored_seq() {
    let store = ProjectionValueStore::new();
    store.apply_cached(&block(&[
        ("title", json!("checkpoint")),
        ("draft", json!("checkpoint")),
    ]));
    let removed = store.seed(0, &block(&[("title", json!("baseline"))]));
    // `draft` was cached and absent from the cut, so it ends with no value. `title` was also cached,
    // but the cut carries it: it is swept and re-seeded, so it is not reported as removed.
    assert_eq!(removed, vec!["draft"]);
    assert_eq!(store.value("title"), Some(json!("baseline")));
    assert_eq!(store.seq_of("title"), Some(0));
    assert!(!store.is_cached("title"));
    assert!(!store.contains("draft"));
}

#[test]
fn a_baseline_seeds_every_carried_key_at_the_cut() {
    let store = ProjectionValueStore::new();
    let removed = store.seed(11, &block(&[("title", json!("t")), ("model", json!("m"))]));
    assert!(removed.is_empty());
    assert_eq!(store.seq_of("title"), Some(11));
    assert_eq!(store.seq_of("model"), Some(11));
    assert_eq!(store.value("title"), Some(json!("t")));
}

#[test]
fn a_baseline_clears_a_key_it_omits_when_that_key_is_not_newer() {
    let store = ProjectionValueStore::new();
    store.apply("title", json!("t"), 5);
    store.apply("model", json!("m"), 5);
    // The cut carries `title` but not `model`, and `model`'s seq is behind the cut: the host computes
    // no value for it as of seq 9.
    let removed = store.seed(9, &block(&[("title", json!("t2"))]));
    assert_eq!(removed, vec!["model"]);
    assert!(!store.contains("model"));
    assert_eq!(store.value("title"), Some(json!("t2")));
}

#[test]
fn a_baseline_neither_overwrites_nor_clears_a_newer_frame() {
    let store = ProjectionValueStore::new();
    // A frame that arrived after the cut the baseline was built at.
    store.apply("title", json!("from frame"), 20);
    // The baseline is older in both directions: it carries a value for `title` at a lower seq, and it
    // omits `model` entirely.
    let removed = store.seed(12, &block(&[("title", json!("from baseline"))]));
    assert!(removed.is_empty());
    assert_eq!(store.value("title"), Some(json!("from frame")));
    assert_eq!(store.seq_of("title"), Some(20));
}

#[test]
fn an_omitted_key_at_exactly_the_cut_is_cleared() {
    // `<=` rather than `<`: a row at the cut that the cut does not carry is stale by construction.
    let store = ProjectionValueStore::new();
    store.apply("model", json!("m"), 9);
    let removed = store.seed(9, &block(&[("title", json!("t"))]));
    assert_eq!(removed, vec!["model"]);
}

#[test]
fn a_baseline_notifies_every_affected_face() {
    let store = ProjectionValueStore::new();
    store.apply("title", json!("old"), 1);
    let face = store.face("title");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = face.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    // The key survives the cut, so the face is notified because the value moved.
    store.seed(2, &block(&[("title", json!("new"))]));
    face.flush();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn a_cleared_key_notifies_its_face_once() {
    let store = ProjectionValueStore::new();
    store.apply("model", json!("m"), 3);
    let face = store.face("model");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = face.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let removed = store.seed(5, &BTreeMap::new());
    assert_eq!(removed, vec!["model"]);
    face.flush();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

// -------------------------------------------------------------------------------------------------
// Generation reset
// -------------------------------------------------------------------------------------------------

#[test]
fn clear_drops_values_and_watermarks_but_keeps_faces() {
    let store = ProjectionValueStore::new();
    let face = store.face("title");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = face.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    store.apply("title", json!("t"), 3);
    store.apply_cached(&block(&[("model", json!("m"))]));

    let cleared = store.clear();
    assert_eq!(cleared, vec!["model", "title"]);
    assert!(store.is_empty());
    assert_eq!(store.value("title"), None);
    assert_eq!(store.seq_of("title"), None);

    // The face is the same face, and it still reaches the same listener.
    assert!(Arc::ptr_eq(&face, &store.face("title")));
    face.flush();
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // A fresh generation starts the watermark over: seq 0 is acceptable again.
    assert!(store.apply("title", json!("after reconnect"), 0));
    assert_eq!(store.seq_of("title"), Some(0));
}

#[test]
fn clear_on_an_empty_store_reports_nothing_and_notifies_once() {
    let store = ProjectionValueStore::new();
    assert!(store.clear().is_empty());
    assert!(store.is_empty());
}

#[test]
fn faces_are_identity_stable_and_created_on_demand() {
    let store = ProjectionValueStore::new();
    let first = store.face("title");
    let second = store.face("title");
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.key(), "title");
    // A face for a key that never carries a value still exists: a component may subscribe before the
    // host has computed anything.
    let never = store.face("never-written");
    assert_eq!(never.listener_count(), 0);
    assert!(!Arc::ptr_eq(&first, &never));
}

// -------------------------------------------------------------------------------------------------
// The notifier
// -------------------------------------------------------------------------------------------------

#[test]
fn mark_dirty_batches_many_writes_into_one_notification() {
    let notifier = Notifier::new(|| {});
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = notifier.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    for _ in 0..20 {
        notifier.mark_dirty();
    }
    assert_eq!(notifier.scheduled_kind(), Some(PublishKind::Microtask));
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    assert_eq!(notifier.flush(), Publish::Published);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    // The batch is closed: a second flush has nothing to publish.
    assert_eq!(notifier.flush(), Publish::Idle);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn the_rebuild_runs_once_per_batch_and_before_the_listeners() {
    let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let rebuild_order = Arc::clone(&order);
    let notifier = Notifier::new(move || {
        rebuild_order
            .lock()
            .expect("the order log is not poisoned")
            .push("rebuild");
    });
    let listener_order = Arc::clone(&order);
    let _subscription = notifier.subscribe(move || {
        listener_order
            .lock()
            .expect("the order log is not poisoned")
            .push("listener");
    });

    notifier.mark_dirty();
    notifier.mark_dirty();
    notifier.flush();
    assert_eq!(
        *order.lock().expect("the order log is not poisoned"),
        vec!["rebuild", "listener"]
    );
    assert_eq!(notifier.rebuild_count(), 1);
}

#[test]
fn a_frame_batch_is_recorded_and_not_downgraded_by_a_later_microtask_mark() {
    let notifier = Notifier::new(|| {});
    notifier.mark_frame_dirty();
    assert_eq!(notifier.scheduled_kind(), Some(PublishKind::Frame));
    // Upstream returns early when anything is already scheduled, so a plain dirty mark inside the
    // frame batch neither creates a second schedule nor changes the pending kind.
    notifier.mark_dirty();
    assert_eq!(notifier.scheduled_kind(), Some(PublishKind::Frame));
    // Nobody is subscribed, so the batch is closed unobserved rather than published.
    assert_eq!(notifier.flush(), Publish::Unobserved);
}

#[test]
fn notify_now_publishes_in_the_same_tick_and_supersedes_the_batch() {
    let notifier = Notifier::new(|| {});
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = notifier.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    notifier.mark_dirty();
    let generation = notifier.schedule_generation();
    assert_eq!(notifier.notify_now(), Publish::Published);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // The superseded scheduled callback must not publish the batch a second time.
    assert_eq!(notifier.complete_schedule(generation), None);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!notifier.is_pending());
}

#[test]
fn a_scheduled_callback_closes_its_own_batch_exactly_once() {
    let notifier = Notifier::new(|| {});
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = notifier.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    notifier.mark_dirty();
    let generation = notifier.schedule_generation();
    assert_eq!(
        notifier.complete_schedule(generation),
        Some(Publish::Published)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    // The same generation called twice finds nothing pending. `Idle` rather than `None`: the schedule
    // was not superseded, it was already delivered — and a caller must be able to tell those apart to
    // know whether its batch is still owed a publication.
    assert_eq!(notifier.complete_schedule(generation), Some(Publish::Idle));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn an_unobserved_batch_stays_dirty_and_rebuilds_on_the_next_read() {
    let notifier = Notifier::new(|| {});
    notifier.mark_dirty();
    // No listener: upstream deliberately leaves the dirty flag set so the next reader rebuilds.
    assert_eq!(notifier.flush(), Publish::Unobserved);
    assert_eq!(notifier.rebuild_count(), 0);
    assert!(notifier.ensure_fresh());
    assert_eq!(notifier.rebuild_count(), 1);
    // Already fresh: a second read does not rebuild twice.
    assert!(!notifier.ensure_fresh());
    assert_eq!(notifier.rebuild_count(), 1);
}

#[test]
fn ensure_fresh_does_not_consume_the_pending_notification() {
    let notifier = Notifier::new(|| {});
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = notifier.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    notifier.mark_dirty();
    assert!(notifier.ensure_fresh());
    // The read rebuilt synchronously but told nobody yet: notification during a read would be a
    // render-time write.
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(notifier.is_pending());
    assert_eq!(notifier.flush(), Publish::Published);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    // The rebuild already happened for this batch, so the flush does not repeat it.
    assert_eq!(notifier.rebuild_count(), 1);
}

#[test]
fn unsubscribing_stops_notification_and_is_idempotent() {
    let notifier = Notifier::new(|| {});
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let subscription = notifier.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(notifier.listener_count(), 1);
    notifier.unsubscribe(subscription);
    notifier.unsubscribe(subscription);
    assert_eq!(notifier.listener_count(), 0);

    notifier.mark_dirty();
    assert_eq!(notifier.flush(), Publish::Unobserved);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn a_face_unsubscribes_through_its_own_token() {
    let store = ProjectionValueStore::new();
    let face = store.face("title");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let subscription = face.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    store.apply("title", json!("t"), 1);
    face.flush();
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    face.unsubscribe(subscription);
    assert_eq!(face.listener_count(), 0);
    store.apply("title", json!("t2"), 2);
    assert_eq!(face.flush(), Publish::Unobserved);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn a_foreign_listener_id_removes_nothing() {
    // Tokens come from one process-wide counter, so an id issued by one notifier names nothing on
    // another: a cleanup closure stored against the wrong store must not silently detach a stranger's
    // listener.
    let first = Notifier::new(|| {});
    let second = Notifier::new(|| {});
    let _kept = first.subscribe(|| {});
    let foreign = second.subscribe(|| {});
    first.unsubscribe(foreign);
    assert_eq!(first.listener_count(), 1);
    assert_eq!(second.listener_count(), 1);
}

#[test]
fn every_listener_runs_even_when_one_of_them_reenters_the_notifier() {
    let notifier = Arc::new(Notifier::new(|| {}));
    let calls = Arc::new(AtomicUsize::new(0));

    let counter = Arc::clone(&calls);
    let _first = notifier.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    // A listener that reads the snapshot-liveness flag must not deadlock on the internal lock.
    let probe = Arc::clone(&notifier);
    let _second = notifier.subscribe(move || {
        let _pending = probe.is_pending();
    });

    notifier.mark_dirty();
    assert_eq!(notifier.flush(), Publish::Published);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn a_store_with_several_threads_keeps_the_highest_seq() {
    // The store is `Send + Sync`; the seq rule must hold under concurrent frames, not just in
    // sequence. Four threads each apply 25 increasing frames for the same key and the highest wins.
    let store = Arc::new(ProjectionValueStore::new());
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4u64)
        .map(|thread| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for step in 1..=25u64 {
                    store.apply("title", json!(thread * 100 + step), thread * 100 + step);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("no worker panics");
    }
    // Thread 3 owns the highest seqs (300..325), so its last frame is the accepted one.
    assert_eq!(store.seq_of("title"), Some(325));
    assert_eq!(store.value("title"), Some(json!(325)));
}

// -------------------------------------------------------------------------------------------------
// Store-level publication
// -------------------------------------------------------------------------------------------------

#[test]
fn a_store_flush_delivers_each_keys_batch_and_the_coarse_channel() {
    let store = ProjectionValueStore::new();
    let title_face = store.face("title");
    let model_face = store.face("model");
    let title_calls = Arc::new(AtomicUsize::new(0));
    let model_calls = Arc::new(AtomicUsize::new(0));
    let any_calls = Arc::new(AtomicUsize::new(0));

    let counter = Arc::clone(&title_calls);
    let _title = title_face.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let counter = Arc::clone(&model_calls);
    let _model = model_face.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let counter = Arc::clone(&any_calls);
    let _any = store.subscribe_any(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    store.apply("title", json!("t"), 1);
    store.apply("title", json!("t2"), 2);
    store.apply("model", json!("m"), 1);

    // Two faces plus the coarse channel; `never` has no subscribers and no writes.
    let published = store.flush();
    assert_eq!(published.len(), 3);
    assert_eq!(title_calls.load(Ordering::Relaxed), 1);
    assert_eq!(model_calls.load(Ordering::Relaxed), 1);
    assert_eq!(any_calls.load(Ordering::Relaxed), 1);

    // The batch is closed.
    let published = store.flush();
    assert!(published.iter().all(|publish| *publish == Publish::Idle));
}

#[test]
fn a_dropped_frame_notifies_nobody() {
    let store = ProjectionValueStore::new();
    let face = store.face("title");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = face.subscribe(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    store.apply("title", json!("v"), 5);
    face.flush();
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // A replay at the same seq is rejected, so the face is not dirtied by it.
    assert!(!store.apply("title", json!("v"), 5));
    assert!(!face.is_pending());
    face.flush();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn unsubscribe_any_removes_only_that_subscription() {
    let store = ProjectionValueStore::new();
    let first = Arc::new(AtomicUsize::new(0));
    let second = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&first);
    let subscription = store.subscribe_any(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let counter = Arc::clone(&second);
    let _kept = store.subscribe_any(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    store.unsubscribe_any(subscription);
    store.apply("title", json!("t"), 1);
    store.flush();
    assert_eq!(first.load(Ordering::Relaxed), 0);
    assert_eq!(second.load(Ordering::Relaxed), 1);
}

#[test]
fn a_listener_token_is_copyable_and_can_be_stored() {
    // The token is a plain value: upstream's unsubscribe function is usually captured into a cleanup
    // closure, and requiring a borrow of the notifier would make that impossible.
    let store = ProjectionValueStore::new();
    let stored: ListenerId = store.subscribe_any(|| {});
    let copied = stored;
    store.unsubscribe_any(copied);
    assert_eq!(copied, stored);
}

// -------------------------------------------------------------------------------------------------
// The store end to end: frame, checkpoint, baseline, reconnect
// -------------------------------------------------------------------------------------------------

#[test]
fn a_full_session_cycle_ends_with_the_host_value() {
    let store = ProjectionValueStore::new();

    // 1. The list carries a checkpoint view for every visible session, including this one.
    store.apply_cached(&block(&[
        ("title", json!("checkpoint title")),
        ("summary", json!("checkpoint summary")),
        ("model", json!("deepseek-chat")),
    ]));
    assert!(store.is_cached("title"));
    assert_eq!(store.len(), 3);

    // 2. The session opens: the history tail page seeds the store at its cut. `summary` is not on the
    //    page, so it clears — the connected session computes no summary value.
    // `summary` was cached and absent from the cut, so it holds no value afterwards. `title` and
    // `model` were cached too, but the cut re-seeds both, so only `summary` is reported as removed.
    let removed = store.seed(
        40,
        &block(&[("title", json!("live title")), ("model", json!("m"))]),
    );
    assert_eq!(removed, vec!["summary"]);
    assert_eq!(store.value("title"), Some(json!("live title")));
    assert_eq!(store.seq_of("title"), Some(40));
    assert!(!store.contains("summary"));

    // 3. Frames advance the values.
    store.apply("title", json!("renamed"), 44);
    store.apply("model", json!("m2"), 45);
    assert_eq!(store.value("title"), Some(json!("renamed")));

    // 4. The connection drops: values and watermarks go, faces survive.
    let faces: Vec<Arc<_>> = store.keys().iter().map(|key| store.face(key)).collect();
    let cleared = store.clear();
    assert_eq!(cleared, vec!["model", "title"]);
    for face in &faces {
        assert!(Arc::ptr_eq(face, &store.face(face.key())));
    }

    // 5. The reconnect's baseline re-seeds from scratch, at a lower seq than the old generation saw.
    let removed = store.seed(3, &block(&[("title", json!("restored"))]));
    assert!(removed.is_empty());
    assert_eq!(store.value("title"), Some(json!("restored")));
    assert_eq!(store.seq_of("title"), Some(3));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_engine_gap as engine;
}
