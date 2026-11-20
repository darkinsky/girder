//! The projection registry's change feed: per-key publication and event-level coalescing.
//!
//! [`crate::ProjectionRegistry`] answers "what is the state now". It folds every registered projection on
//! every committed event and keeps the result, which is everything a *reader* needs. What it has no
//! notion of is a *subscriber*: a consumer that must learn that a value changed, and must learn it once
//! per event rather than once per read, because a consumer that polls cannot tell a value that changed
//! from a value that was merely read again.
//!
//! # The two channels upstream keeps, and why one is not enough
//!
//! Upstream (`session-projection/src/index.ts`, `SessionProjectionRegistry`) exposes two subscriptions
//! that fire from the same drive:
//!
//! - **`onChanged`** — per key. Called once for each unit whose *view* changed by `Object.is`, carrying
//!   `(session, key, value, seq)`. A consumer that renders one panel wants exactly this: it is told
//!   which key changed and can re-render that panel alone.
//! - **`onEventChanged`** — per event. Called once per committed event, after every `onChanged` call for
//!   that event, carrying `(session, [ {key, val}, … ], seq)` — one entry per unit that changed, in
//!   registration order.
//!
//! The event channel is not a convenience wrapper over the key channel. Its contract has two clauses
//! that cannot be recovered by a listener of the first:
//!
//! 1. **Once per event, not once per key.** A consumer that persists "the session changed at seq N" must
//!    write once per event. Deriving that from the key channel means debouncing a burst it cannot
//!    delimit — the listener has no way to know that the third `onChanged` it received was the last one
//!    for that event — so it either writes three times or writes on a timer it invented. The event
//!    channel makes the batch a fact rather than an inference.
//! 2. **Ordering: after every `onChanged` for the event.** A key listener may take a side effect on the
//!    value it was handed; the event listener then sees the *result* of those side effects. Reversing the
//!    order lets an event listener answer a question the key listeners have not finished mutating.
//!
//! And the failure clause, which is the one that is easy to get wrong: *if a unit or an `onChanged`
//! listener throws during that drive, the event listener still receives the changes made before the
//! throw.* Upstream gets this from a `try { … } finally { … }` around the whole drive: the collected
//! `changes` array is flushed to the event listeners in the `finally`, so a mid-drive panic still
//! delivers the prefix, and then the error propagates. A naive "notify at the end of the loop" port
//! silently drops the batch whenever a listener fails, which is exactly when a consumer most needs to
//! hear about it.
//!
//! # The change predicate, and what this crate can honestly observe
//!
//! Upstream's predicate is `Object.is` on the *raw view result*, not on the state: a unit whose `apply`
//! returns a new state but whose wire view is unchanged publishes nothing (a counter that is not rendered
//! does not re-render a panel), and a unit whose `apply` returns the *same state reference* does not even
//! recompute its view.
//!
//! This crate has no wire layer: [`crate::Projection`] folds state and nothing else, so the view *is* the
//! state. That collapses upstream's two-level check into one — state identity — and the collapse is safe
//! rather than lossy, because the omitted level only ever suppressed publications that a state-only
//! predicate also suppresses. What is *not* honest is claiming reference identity on a [`serde_json::Value`]:
//! a fold that returns a structurally equal value has, for every purpose a consumer has, not changed it,
//! and a fold that allocates a fresh but equal `Value` would otherwise publish a spurious change on every
//! event. [`ChangeFeed::changed`] therefore compares by value equality. The one case this differs from
//! upstream is a unit that deliberately reuses its state reference to signal "nothing changed" while
//! returning a *different* value — a contradiction upstream documents as impossible, so the difference is
//! in a region no correct unit occupies.
//!
//! # What this module is careful not to duplicate
//!
//! - **Not the fold.** [`crate::ProjectionRegistry`] and [`crate::ProjectionCache`] still own `apply`;
//!   this module never calls it. It observes the values a fold produced and decides who hears about them.
//!   A second fold here would create a second definition of what an event means.
//! - **Not the persisted cache.** [`crate::session::projection_cache`] decides *which durable record* may seed
//!   which session's fold, and its write-behind decides *when* a checkpoint lands. A change feed says
//!   nothing about either: it announces a value, and a cache may or may not be behind it.
//! - **Not the client merge.** [`crate::session::engine::ProjectionValueStore`] resolves precedence
//!   between values a client already holds. This module is upstream of that: it is what *produces* the
//!   "sequenced" frame the merge then compares against a cached block.
//! - **Not the summary key set.** [`crate::ProjectionRegistry::summary`] filters by
//!   [`crate::Projection::in_summary`], which is the same declaration upstream's `summaryKeys()` reads.
//!   The difference is only that upstream caches the derived set as a stable instance; that is a
//!   property of a live registration table this crate's registry does not maintain, and copying the
//!   filter here would put a second definition of "summary" in the workspace.
//!
//! # The invariants that are load-bearing
//!
//! - **A batch is flushed even when the drive fails.** The prefix reaches the event listeners, and then
//!   the error propagates. Losing the batch on a panic turns one failed render into a permanently
//!   unsynchronised consumer.
//! - **An empty batch publishes nothing.** Upstream notifies only when at least one unit changed, so a
//!   listener may treat every call as "something changed". Calling it with an empty array would force
//!   every consumer to re-check that, and one of them would forget.
//! - **Per-key listeners run, in registration order, before the batch is delivered.** That is upstream's
//!   ordering clause, and it is what lets the batch carry side effects the key listeners made.
//! - **A listener cannot see a value its own delivery produced.** Both channels iterate a snapshot of the
//!   listener list taken before delivery, so a listener that subscribes or unsubscribes during a delivery
//!   affects the *next* drive, not the current one. Upstream's `Set` iteration has the same shape, and
//!   the alternative — racing a live list — makes delivery count depend on delivery order.
//! - **Unsubscribing is idempotent and keyed by identity.** A consumer that unsubscribes twice, or from
//!   inside its own callback, must not remove a *different* listener that re-used the slot.
//!
//! # Module-include note
//!
//! This file and its test are included by path from `tests/projection_final_gap.rs`, so every reference to
//! the library goes through `girder_core::…` rather than `crate::…`: inside a `#[path]` module, `crate::`
//! names the *test* crate.

// The crate root will declare this module, so its `pub` items are reached from outside the library and the
// library build is clean. The *test* build is not: `tests/projection_final_gap.rs` compiles this file by
// path as a private module, where the accessors the test does not exercise are legitimately dead.
// `projection_extra_gap` carries the same exemption for the same reason.
#![allow(dead_code)]

use std::collections::BTreeMap;

use girder_core::ProjectionSeq as Seq;
use serde_json::Value;

// -------------------------------------------------------------------------------------------------
// The change record
// -------------------------------------------------------------------------------------------------

/// One unit's published change within a single event's drive.
///
/// The pair upstream's `onEventChanged` carries as `{key, val}`: the projection that changed and its
/// whole current value. A whole value rather than a delta, for the same reason every state-carrying
/// event in this crate carries a whole state — it keeps the consumer's update a replacement rather than
/// a fold, and a consumer that missed an earlier delivery is still correct.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// The projection key that changed.
    pub key: String,
    /// The projection's whole current value.
    pub val: Value,
}

impl Change {
    /// One change for `key` at `val`.
    #[must_use]
    pub fn new(key: impl Into<String>, val: Value) -> Self {
        Self {
            key: key.into(),
            val,
        }
    }
}

/// One event's coalesced changes, as `onEventChanged` receives them.
///
/// `seq` is the sequence of the event that caused every change in `changes` — the same number the per-key
/// channel was handed, and the number a consumer stamps on whatever it derives. `changes` is in
/// registration order and never empty: an event that changed nothing produces no delivery at all.
#[derive(Clone, Debug, PartialEq)]
pub struct EventChanges {
    /// The sequence of the event that caused these changes.
    pub seq: Seq,
    /// One entry per changed projection, in registration order.
    pub changes: Vec<Change>,
}

impl EventChanges {
    /// Whether no projection changed, which is the condition for publishing nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The keys that changed, in registration order.
    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        self.changes
            .iter()
            .map(|change| change.key.as_str())
            .collect()
    }

    /// One changed projection's value, when it changed in this batch.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.changes
            .iter()
            .find(|change| change.key == key)
            .map(|change| &change.val)
    }
}

// -------------------------------------------------------------------------------------------------
// Subscription identity
// -------------------------------------------------------------------------------------------------

/// The identity of one subscription.
///
/// A token rather than a bare index: an unsubscribe must name the subscription it created, not the slot
/// it happened to occupy. Without that, a consumer that unsubscribes twice — or unsubscribes from inside
/// its own callback — removes whatever listener was installed after it, and the bug shows up as a
/// rendering panel that goes dead for no reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Subscription(u64);

impl Subscription {
    /// The token's raw number, for logging.
    #[must_use]
    pub fn id(self) -> u64 {
        self.0
    }
}

/// Why a delivery was refused.
///
/// Not a failure of the fold: a listener panicking is a fact about the consumer, and the drive's job is
/// to report it without pretending the remaining listeners were told. The distinction matters because
/// the two responses differ — a fold failure means "refold", a listener failure means "this consumer is
/// broken and the others were not notified".
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FeedError {
    /// A per-key listener panicked. The event listeners were still given every change collected before it.
    #[error("a projection change listener failed while being delivered `{key}` at seq {seq}")]
    KeyListener {
        /// The key whose delivery failed.
        key: String,
        /// The sequence being delivered.
        seq: Seq,
    },

    /// An event-level listener panicked. Earlier listeners in the same batch were already called.
    #[error("a projection event listener failed while being delivered seq {seq}")]
    EventListener {
        /// The sequence being delivered.
        seq: Seq,
    },
}

// -------------------------------------------------------------------------------------------------
// The feed
// -------------------------------------------------------------------------------------------------

/// One per-key subscription's callback.
type KeyListener = Box<dyn FnMut(&Change, Seq) + Send>;

/// One event-level subscription's callback.
type EventListener = Box<dyn FnMut(&EventChanges) + Send>;

/// The published change feed for one session's projections.
///
/// The registry owns one of these in its own lock, separate from the folded state. [`ChangeFeed::drive`]
/// runs only after that state lock is released, so a listener that reads the registry does not deadlock
/// against the fold that is notifying it. A listener must not call back into `observe` on the same
/// registry: that would try to lock this feed again while the drive still holds it.
#[derive(Default)]
pub struct ChangeFeed {
    next_token: u64,
    key_listeners: BTreeMap<Subscription, KeyListener>,
    event_listeners: BTreeMap<Subscription, EventListener>,
}

impl std::fmt::Debug for ChangeFeed {
    /// Reports the subscription count, not the listeners: a closure has no `Debug`, and a feed's useful
    /// property in a log line is how many consumers are attached, not who they are.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChangeFeed")
            .field("key_listeners", &self.key_listeners.len())
            .field("event_listeners", &self.event_listeners.len())
            .field("issued_tokens", &self.next_token)
            .finish()
    }
}

impl ChangeFeed {
    /// An empty feed with no subscribers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribes to per-key changes.
    ///
    /// Returns the token [`ChangeFeed::unsubscribe`] takes. The listener runs synchronously inside
    /// [`ChangeFeed::drive`], once per changed key, in registration order.
    pub fn on_changed<F>(&mut self, listener: F) -> Subscription
    where
        F: FnMut(&Change, Seq) + Send + 'static,
    {
        self.insert_key_listener(Box::new(listener))
    }

    /// Subscribes to event-level changes.
    ///
    /// The listener runs once per event that changed at least one key, after every [`ChangeFeed::on_changed`]
    /// listener for that event.
    pub fn on_event_changed<F>(&mut self, listener: F) -> Subscription
    where
        F: FnMut(&EventChanges) + Send + 'static,
    {
        self.insert_event_listener(Box::new(listener))
    }

    fn insert_key_listener(&mut self, listener: KeyListener) -> Subscription {
        self.next_token += 1;
        let token = Subscription(self.next_token);
        self.key_listeners.insert(token, listener);
        token
    }

    fn insert_event_listener(&mut self, listener: EventListener) -> Subscription {
        self.next_token += 1;
        let token = Subscription(self.next_token);
        self.event_listeners.insert(token, listener);
        token
    }

    /// Removes one subscription.
    ///
    /// Returns whether the token named a live subscription. Idempotent: a second call with the same token
    /// is a no-op rather than an error, so a consumer may unsubscribe from a `Drop` and from an explicit
    /// path without coordinating which ran first.
    pub fn unsubscribe(&mut self, token: Subscription) -> bool {
        self.key_listeners.remove(&token).is_some() | self.event_listeners.remove(&token).is_some()
    }

    /// Whether anything is subscribed.
    ///
    /// A caller uses this to skip building change records at all: with no listeners there is no one to
    /// tell, and upstream's `observed` flag exists for exactly that saving.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.key_listeners.is_empty() && self.event_listeners.is_empty()
    }

    /// How many subscriptions of either kind are live.
    #[must_use]
    pub fn len(&self) -> usize {
        self.key_listeners.len() + self.event_listeners.len()
    }

    /// Publishes one event's changes on both channels.
    ///
    /// The order is upstream's: every per-key listener first, in registration order, then the event
    /// listeners with the whole batch. An empty batch publishes nothing at all — not even an empty
    /// delivery — so a listener may treat every call as news.
    ///
    /// A listener that panics does not silently truncate the drive: the batch is still delivered to the
    /// event listeners, and the first failure is then reported as a [`FeedError`]. Every change
    /// *collected so far* is delivered, which for a per-key failure is the full batch — the changes were
    /// all computed before delivery began — and that is deliberately upstream's `finally` behaviour
    /// rather than "drop everything".
    ///
    /// # Errors
    ///
    /// Returns [`FeedError::KeyListener`] or [`FeedError::EventListener`] when a listener panicked, after
    /// the event listeners have been given the batch. The panic is contained, not resumed: a consumer's
    /// failure must not take down the fold that produced the value it was told about.
    pub fn drive(&mut self, changes: Vec<Change>, seq: Seq) -> Result<(), FeedError> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut failure: Option<FeedError> = None;
        for change in &changes {
            for listener in self.key_listeners.values_mut() {
                // A listener that fails stops the rest of *this* key's delivery — it is a queue of
                // callbacks for one fact, and continuing past a panic would hand later listeners a
                // fact the failed one never got. The batch still reaches the event channel below.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(change, seq)))
                    .is_err()
                    && failure.is_none()
                {
                    failure = Some(FeedError::KeyListener {
                        key: change.key.clone(),
                        seq,
                    });
                }
            }
        }
        let batch = EventChanges { seq, changes };
        for listener in self.event_listeners.values_mut() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(&batch))).is_err()
                && failure.is_none()
            {
                failure = Some(FeedError::EventListener { seq });
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Publishes the differences between two folds of one log, as one event's changes.
    ///
    /// The convenience for a caller that has a [`crate::ProjectionRegistry`] and just observed an event:
    /// it holds the values from before, reads the values after, and this computes the per-key diff in
    /// registration order. A key present in `after` but absent from `before` changed by definition, and so
    /// did one that disappeared — a registry's key set is fixed, but a caller comparing two checkpoints
    /// of different vintages is not.
    ///
    /// # Errors
    ///
    /// As [`ChangeFeed::drive`].
    pub fn drive_diff(
        &mut self,
        before: &BTreeMap<String, Value>,
        after: &BTreeMap<String, Value>,
        seq: Seq,
    ) -> Result<(), FeedError> {
        let changes = diff(before, after);
        self.drive(changes, seq)
    }
}

/// The per-key differences between two views, in the `after` map's key order.
///
/// Value equality, deliberately, for the reason the module note gives: a fold that allocates a fresh but
/// equal value has not changed anything a consumer can observe, and publishing it would make every event
/// a change and the feed useless.
#[must_use]
pub fn diff(before: &BTreeMap<String, Value>, after: &BTreeMap<String, Value>) -> Vec<Change> {
    let mut changes = Vec::new();
    for (key, value) in after {
        if before.get(key) != Some(value) {
            changes.push(Change::new(key.clone(), value.clone()));
        }
    }
    for (key, value) in before {
        if !after.contains_key(key) {
            changes.push(Change::new(key.clone(), value.clone()));
        }
    }
    changes
}

// -------------------------------------------------------------------------------------------------
// The batch accumulator the drive fills
// -------------------------------------------------------------------------------------------------

/// The changes one event produced, accumulated while the fold runs.
///
/// Upstream threads a local `changes` array through `drive` and flushes it in a `finally`. An explicit
/// type is what makes that flush had-to-happen rather than remembered: [`ChangeBatch::finish`] consumes
/// the batch, so a caller cannot reach the end of a drive holding an undelivered one without the compiler
/// noticing.
#[derive(Debug, Default)]
pub struct ChangeBatch {
    seq: Seq,
    changes: Vec<Change>,
}

impl ChangeBatch {
    /// A batch for the event at `seq`.
    #[must_use]
    pub fn new(seq: Seq) -> Self {
        Self {
            seq,
            changes: Vec::new(),
        }
    }

    /// The sequence this batch is for.
    #[must_use]
    pub fn seq(&self) -> Seq {
        self.seq
    }

    /// Records one projection's new value when it differs from what the caller saw before.
    ///
    /// Returns whether the change was recorded, which is the same answer as "did this key change" and is
    /// what a caller branches on to decide whether to recompute anything downstream.
    pub fn note(&mut self, key: &str, previous: &Value, next: &Value) -> bool {
        if previous == next {
            return false;
        }
        self.changes.push(Change::new(key, next.clone()));
        true
    }

    /// The changes collected so far.
    #[must_use]
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// Whether nothing changed, so the batch must not be published.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Publishes the batch, consuming it.
    ///
    /// Consuming is the point: the batch is delivered exactly once, and a drive that unwinds through `?`
    /// cannot both deliver it and deliver it again.
    ///
    /// # Errors
    ///
    /// As [`ChangeFeed::drive`].
    pub fn finish(self, feed: &mut ChangeFeed) -> Result<(), FeedError> {
        feed.drive(self.changes, self.seq)
    }
}

#[cfg(test)]
mod self_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_reports_only_unequal_keys() {
        let before = BTreeMap::from([("a".to_owned(), json!(1)), ("b".to_owned(), json!(2))]);
        let after = BTreeMap::from([("a".to_owned(), json!(1)), ("b".to_owned(), json!(3))]);
        let changes = diff(&before, &after);
        assert_eq!(changes, vec![Change::new("b", json!(3))]);
    }

    #[test]
    fn an_empty_batch_publishes_nothing() {
        let mut feed = ChangeFeed::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(0_u32));
        let counter = std::sync::Arc::clone(&seen);
        feed.on_event_changed(move |_| {
            *counter.lock().unwrap() += 1;
        });
        assert!(feed.drive(Vec::new(), 1).is_ok());
        assert_eq!(*seen.lock().unwrap(), 0);
    }
}
