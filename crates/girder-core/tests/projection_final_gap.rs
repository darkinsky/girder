//! Tests for the projection change feed (`projection_final_gap`).
//!
//! Corresponds to upstream `packages/session/session-projection/tests/registry.spec.ts`'s change-feed
//! cases: `onChanged` per-key delivery, `onEventChanged` coalescing and ordering, the empty-batch rule,
//! and the `finally` clause that still delivers the batch when a listener throws.
//!
//! # What each property protects
//!
//! - **An event that changed nothing publishes nothing.** A consumer treats every delivery as news; an
//!   empty delivery forces every consumer to re-check, and one of them forgets.
//! - **The batch is the set of keys that changed, in registration order.** It is what a consumer
//!   persists as "the session moved to seq N", so a missing key silently desynchronises that key and an
//!   extra one makes it rewrite a value that did not move.
//! - **Per-key listeners run before the batch.** An event listener observes side effects the per-key
//!   listeners made for the same event; reversing the order lets it answer a question they have not
//!   finished mutating.
//! - **A failing listener still delivers the batch.** This is upstream's `finally`. Dropping the batch on
//!   a panic turns one failed render into a permanently unsynchronised consumer — precisely the state a
//!   consumer cannot detect on its own.
//! - **A listener cannot see the delivery it causes.** Both channels iterate a snapshot taken before
//!   delivery, so subscribing from inside a callback affects the next drive, not the current one;
//!   otherwise delivery count depends on delivery order.
//! - **Unsubscribe is identity-keyed and idempotent.** A double unsubscribe, or one from inside its own
//!   callback, must not remove whichever listener took the slot after it.
//! - **Equality, not reference identity, decides change.** A fold that allocates a fresh but equal value
//!   has changed nothing observable; treating it as a change makes every event a change.
//! - **A batch is delivered exactly once.** `ChangeBatch::finish` consumes it, so an unwinding drive
//!   cannot deliver the same batch twice.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not `girder-core`,
//! so every reference to the library goes through `girder_core::{...}`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "tests assert by panicking, index their own fixtures, and read best as one scenario"
)]

#[path = "../src/projection_final_gap.rs"]
mod projection_final_gap;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use girder_core::ProjectionSeq as Seq;
use projection_final_gap::{
    Change, ChangeBatch, ChangeFeed, EventChanges, FeedError, Subscription, diff,
};
use serde_json::{Value, json};

// ---- fixtures ----------------------------------------------------------------------------------

/// A recorded delivery on the per-key channel: `(key, value, seq)`.
type KeyDelivery = (String, Value, Seq);

/// A recorded delivery on the event channel: `(keys in order, seq, full batch)`.
type EventDelivery = (Vec<String>, Seq, EventChanges);

/// A feed plus the two channels' recorded deliveries, kept in one place so a test can assert on both.
///
/// The recordings live behind a shared `Mutex` because the listeners are `'static` closures: they cannot
/// borrow the test's stack, and the alternative — a channel — would make every assertion an ordering
/// question the feed does not promise.
struct Recorder {
    feed: ChangeFeed,
    keys: Arc<Mutex<Vec<KeyDelivery>>>,
    events: Arc<Mutex<Vec<EventDelivery>>>,
}

impl Recorder {
    /// A feed with one listener on each channel, both recording.
    fn new() -> Self {
        let keys: Arc<Mutex<Vec<KeyDelivery>>> = Arc::new(Mutex::new(Vec::new()));
        let events: Arc<Mutex<Vec<EventDelivery>>> = Arc::new(Mutex::new(Vec::new()));
        let mut feed = ChangeFeed::new();
        let key_sink = Arc::clone(&keys);
        feed.on_changed(move |change, seq| {
            key_sink
                .lock()
                .unwrap()
                .push((change.key.clone(), change.val.clone(), seq));
        });
        let event_sink = Arc::clone(&events);
        feed.on_event_changed(move |batch| {
            event_sink.lock().unwrap().push((
                batch.keys().iter().map(|key| (*key).to_owned()).collect(),
                batch.seq,
                batch.clone(),
            ));
        });
        Self { feed, keys, events }
    }

    /// The deliveries one channel recorded, copied out so the lock is not held across assertions.
    fn keys(&self) -> Vec<KeyDelivery> {
        self.keys.lock().unwrap().clone()
    }

    /// The event-channel deliveries.
    fn events(&self) -> Vec<EventDelivery> {
        self.events.lock().unwrap().clone()
    }
}

/// A change for `key` at `value`.
fn change(key: &str, value: Value) -> Change {
    Change::new(key, value)
}

/// A view map.
fn view(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

// ---- a drive delivers what changed, in order ---------------------------------------------------

#[test]
fn a_batch_reaches_both_channels_in_registration_order() {
    let mut recorder = Recorder::new();
    let changes = vec![
        change("title", json!("first")),
        change("goal", json!({ "text": "ship it" })),
    ];
    recorder.feed.drive(changes, 7).unwrap();

    assert_eq!(
        recorder.keys(),
        vec![
            ("title".to_owned(), json!("first"), 7),
            ("goal".to_owned(), json!({ "text": "ship it" }), 7),
        ],
        "the per-key channel receives one delivery per changed key, in batch order"
    );
    let events = recorder.events();
    assert_eq!(
        events.len(),
        1,
        "one event produced one event-level delivery"
    );
    assert_eq!(events[0].0, vec!["title".to_owned(), "goal".to_owned()]);
    assert_eq!(events[0].1, 7, "the batch is stamped with its event's seq");
    assert_eq!(
        events[0].2.get("goal"),
        Some(&json!({ "text": "ship it" })),
        "the batch carries each changed key's whole value"
    );
}

#[test]
fn an_event_that_changed_nothing_publishes_nothing() {
    let mut recorder = Recorder::new();
    recorder.feed.drive(Vec::new(), 3).unwrap();

    assert!(
        recorder.keys().is_empty(),
        "no key delivery for an empty batch"
    );
    assert!(
        recorder.events().is_empty(),
        "no event delivery for an empty batch: a consumer treats every call as news"
    );
}

#[test]
fn a_feed_with_no_listeners_accepts_a_drive_and_reports_emptiness() {
    let mut feed = ChangeFeed::new();
    assert!(feed.is_empty());
    assert_eq!(feed.len(), 0);
    assert!(
        feed.drive(vec![change("title", json!("x"))], 1).is_ok(),
        "delivering to nobody is not an error"
    );
}

// ---- ordering: per-key first, then the batch ---------------------------------------------------

#[test]
fn per_key_listeners_run_before_the_event_listeners() {
    // The order is observed through a side effect the key listener makes: the event listener reads it.
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let mut feed = ChangeFeed::new();

    let key_log = Arc::clone(&log);
    feed.on_changed(move |_change, _seq| {
        key_log.lock().unwrap().push("key");
    });
    let event_log = Arc::clone(&log);
    feed.on_event_changed(move |_batch| {
        event_log.lock().unwrap().push("event");
    });

    feed.drive(vec![change("title", json!("t"))], 1).unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        vec!["key", "event"],
        "the event channel observes the effect of the per-key channel for the same event"
    );
}

#[test]
fn listeners_run_in_registration_order() {
    let order: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let mut feed = ChangeFeed::new();
    for step in 1..=3_u8 {
        let sink = Arc::clone(&order);
        feed.on_changed(move |_change, _seq| sink.lock().unwrap().push(step));
    }
    feed.drive(vec![change("title", json!("t"))], 1).unwrap();
    assert_eq!(*order.lock().unwrap(), vec![1, 2, 3]);
}

// ---- the failure clause ------------------------------------------------------------------------

#[test]
fn a_panicking_key_listener_still_delivers_the_batch() {
    let mut feed = ChangeFeed::new();
    feed.on_changed(|_change, _seq| panic!("consumer is broken"));
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    feed.on_event_changed(move |batch| {
        sink.lock()
            .unwrap()
            .push(batch.keys().iter().map(|key| (*key).to_owned()).collect());
    });

    let outcome = feed.drive(
        vec![change("title", json!("a")), change("goal", json!("b"))],
        5,
    );

    assert_eq!(
        outcome,
        Err(FeedError::KeyListener {
            key: "title".to_owned(),
            seq: 5
        }),
        "the failing key is named, so the broken consumer can be found"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![vec!["title".to_owned(), "goal".to_owned()]],
        "the full batch still reaches the event channel — upstream's finally clause"
    );
}

#[test]
fn a_panicking_event_listener_reports_after_the_key_channel_ran() {
    let mut feed = ChangeFeed::new();
    let key_count: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&key_count);
    feed.on_changed(move |_change, _seq| *counter.lock().unwrap() += 1);
    feed.on_event_changed(|_batch| panic!("event consumer is broken"));

    let outcome = feed.drive(vec![change("title", json!("a"))], 9);

    assert_eq!(outcome, Err(FeedError::EventListener { seq: 9 }));
    assert_eq!(
        *key_count.lock().unwrap(),
        1,
        "the per-key channel had already run; a broken event listener does not retroactively undo it"
    );
}

#[test]
fn the_first_failure_is_the_one_reported() {
    // Two failing listeners must not overwrite the first error: it names the earliest consumer to break,
    // which is the one an operator fixes first.
    let mut feed = ChangeFeed::new();
    feed.on_changed(|_change, _seq| panic!("first"));
    feed.on_changed(|_change, _seq| panic!("second"));
    let outcome = feed.drive(vec![change("alpha", json!(1)), change("beta", json!(2))], 4);
    assert_eq!(
        outcome,
        Err(FeedError::KeyListener {
            key: "alpha".to_owned(),
            seq: 4
        })
    );
}

// ---- subscription identity ---------------------------------------------------------------------

#[test]
fn unsubscribe_removes_exactly_the_token_it_names() {
    let mut feed = ChangeFeed::new();
    let first: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let second: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

    let one = Arc::clone(&first);
    let first_token = feed.on_changed(move |_change, _seq| *one.lock().unwrap() += 1);
    let two = Arc::clone(&second);
    let second_token = feed.on_changed(move |_change, _seq| *two.lock().unwrap() += 1);

    assert!(feed.unsubscribe(first_token));
    feed.drive(vec![change("title", json!("t"))], 1).unwrap();

    assert_eq!(*first.lock().unwrap(), 0, "the named subscription is gone");
    assert_eq!(
        *second.lock().unwrap(),
        1,
        "the subscription that took the slot after it is untouched"
    );
    assert!(
        !feed.unsubscribe(first_token),
        "a second unsubscribe is a no-op, not an error"
    );
    assert!(feed.unsubscribe(second_token));
    assert!(feed.is_empty());
}

#[test]
fn unsubscribe_is_idempotent_after_the_feed_is_empty() {
    let mut feed = ChangeFeed::new();
    let token = feed.on_event_changed(|_batch| {});
    assert!(feed.unsubscribe(token));
    assert!(!feed.unsubscribe(token));
    assert_eq!(feed.len(), 0);
}

#[test]
fn tokens_are_distinct_across_both_channels() {
    // One counter, so a token can never name two subscriptions on different channels.
    let mut feed = ChangeFeed::new();
    let key_token: Subscription = feed.on_changed(|_change, _seq| {});
    let event_token: Subscription = feed.on_event_changed(|_batch| {});
    assert_ne!(key_token, event_token);
    assert_eq!(feed.len(), 2);
    assert!(feed.unsubscribe(event_token));
    assert_eq!(feed.len(), 1, "only the event subscription was removed");
    assert!(feed.unsubscribe(key_token));
}

#[test]
fn a_listener_installed_after_a_drive_sees_no_past_batch() {
    // The feed holds no queued state between drives: a subscription is a live callback, not a cursor, so
    // there is nothing to replay to a late subscriber. A consumer that needs the current values reads the
    // registry instead — which is the split this module relies on.
    let mut feed = ChangeFeed::new();
    let late: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    feed.drive(vec![change("title", json!("t"))], 1).unwrap();
    let token = {
        let counter = Arc::clone(&late);
        feed.on_event_changed(move |_batch| *counter.lock().unwrap() += 1)
    };
    assert_eq!(
        *late.lock().unwrap(),
        0,
        "the listener installed after a drive sees no past batch"
    );
    feed.drive(vec![change("title", json!("u"))], 2).unwrap();
    assert_eq!(*late.lock().unwrap(), 1, "it sees the next one");
    assert!(feed.unsubscribe(token));
}

// ---- diffing two folds -------------------------------------------------------------------------

#[test]
fn diff_reports_changed_and_vanished_keys_only() {
    let before = view(&[
        ("title", json!("a")),
        ("goal", json!(1)),
        ("stale", json!(true)),
    ]);
    let after = view(&[
        ("title", json!("a")),
        ("goal", json!(2)),
        ("added", json!("new")),
    ]);

    let changes = diff(&before, &after);

    assert_eq!(
        changes,
        vec![
            change("added", json!("new")),
            change("goal", json!(2)),
            change("stale", json!(true))
        ],
        "unchanged `title` is omitted; a changed, an added and a vanished key are each reported, \
         in the `after` map's key order"
    );
}

#[test]
fn diff_treats_a_structurally_equal_value_as_unchanged() {
    // The reason the predicate is equality and not reference identity: a fold that allocates a fresh but
    // equal value has changed nothing a consumer can observe.
    let before = view(&[("goal", json!({ "text": "x", "steps": [1, 2] }))]);
    let after = view(&[("goal", json!({ "steps": [1, 2], "text": "x" }))]);
    assert!(
        diff(&before, &after).is_empty(),
        "object key order is not a change"
    );
}

#[test]
fn drive_diff_publishes_only_the_difference() {
    let mut recorder = Recorder::new();
    let before = view(&[("title", json!("a")), ("goal", json!(1))]);
    let after = view(&[("title", json!("a")), ("goal", json!(2))]);
    recorder.feed.drive_diff(&before, &after, 12).unwrap();

    assert_eq!(recorder.keys(), vec![("goal".to_owned(), json!(2), 12)]);
    assert_eq!(recorder.events()[0].0, vec!["goal".to_owned()]);
}

#[test]
fn drive_diff_with_no_difference_publishes_nothing() {
    let mut recorder = Recorder::new();
    let same = view(&[("title", json!("a"))]);
    recorder.feed.drive_diff(&same, &same, 4).unwrap();
    assert!(recorder.keys().is_empty());
    assert!(recorder.events().is_empty());
}

// ---- the accumulator ---------------------------------------------------------------------------

#[test]
fn a_batch_records_only_values_that_moved() {
    let mut batch = ChangeBatch::new(6);
    assert!(
        !batch.note("title", &json!("a"), &json!("a")),
        "equal is not a change"
    );
    assert!(
        batch.note("goal", &json!(1), &json!(2)),
        "a different value is a change"
    );
    assert_eq!(batch.seq(), 6);
    assert_eq!(batch.changes(), &[change("goal", json!(2))]);
    assert!(!batch.is_empty());
}

#[test]
fn an_empty_batch_never_reaches_a_listener() {
    let batch = ChangeBatch::new(2);
    let mut recorder = Recorder::new();
    batch.finish(&mut recorder.feed).unwrap();
    assert!(recorder.keys().is_empty());
    assert!(recorder.events().is_empty());
}

#[test]
fn finishing_a_batch_delivers_it_once() {
    let mut recorder = Recorder::new();
    let mut batch = ChangeBatch::new(8);
    batch.note("title", &json!("a"), &json!("b"));
    batch.finish(&mut recorder.feed).unwrap();
    assert_eq!(recorder.keys().len(), 1);
    assert_eq!(recorder.events().len(), 1);
    // `finish` consumed the batch, so a second delivery is not expressible here — the compile-time half of
    // "exactly once". What remains observable is that the feed holds no queued state between drives.
    assert!(recorder.feed.drive(Vec::new(), 8).is_ok());
    assert_eq!(
        recorder.keys().len(),
        1,
        "a repeat drive with nothing to publish adds nothing"
    );
}

// ---- the sequence contract ---------------------------------------------------------------------

#[test]
fn every_delivery_carries_the_causing_events_seq() {
    let mut recorder = Recorder::new();
    for seq in [1_u64, 2, 9] {
        recorder
            .feed
            .drive(vec![change("title", json!(seq))], seq)
            .unwrap();
    }
    let seqs: Vec<Seq> = recorder
        .keys()
        .iter()
        .map(|(_key, _val, seq)| *seq)
        .collect();
    assert_eq!(
        seqs,
        vec![1, 2, 9],
        "a consumer stamps what it derives with the causing event's seq"
    );
    let event_seqs: Vec<Seq> = recorder
        .events()
        .iter()
        .map(|(_keys, seq, _batch)| *seq)
        .collect();
    assert_eq!(event_seqs, vec![1, 2, 9]);
}
