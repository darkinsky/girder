//! The client projection store of upstream `packages/api/session-controller`: per-key merged rows,
//! the frame/baseline precedence rules, and the batching notifier that publishes them.
//!
//! # Where this comes from
//!
//! Upstream keeps this capability in two small files with a large blast radius:
//!
//! - `src/client/sessions/projection-store.ts` (244 lines) — `ProjectionValueStore`: per-key rows of
//!   two kinds (`sequenced`, `cached`), the merge rules between them, a reference-stable value
//!   snapshot, and identity-stable per-key observable faces.
//! - `src/client/sessions/notifier.ts` (97 lines) — `Notifier`: dirty tracking, microtask/frame
//!   batching with generation-guarded publication, and a synchronous flush for controlled inputs.
//!
//! Neither symbol existed in this crate. The port report carries the coverage table for the whole
//! client engine (2,972 lines across `client/*`, `client/sessions/*` and `client/contract/*`).
//!
//! # Why this capability, and why now
//!
//! The client engine's other two large files are orchestration *around* this store: `manager.ts`
//! (857 lines) decides which control frame is a baseline and rebuilds one list snapshot from it, and
//! `session.ts` (1,055 lines) decides which history window to request and how to prepend it. Both read
//! every finished projection value through `ProjectionValueStore`, and upstream's own tests for them
//! assert through `values()`, `seqOf()` and the per-key faces. Porting either one first would mean
//! inventing a second place where projection precedence lives — the two sources of truth this port
//! exists to avoid.
//!
//! # The rules, and why each one is load-bearing
//!
//! - **Higher seq wins among sequenced rows.** A follow session receives a baseline and then delta
//!   frames; a reconnecting stream can redeliver a frame it already sent. Comparing seqs is what makes
//!   a replay a no-op instead of a regression.
//! - **A cached row never outranks a sequenced one.** The session list may carry a zero-I/O view of
//!   the persisted checkpoint for sessions nobody has opened. That view has no comparable seq — its
//!   origin cannot vouch that its stored seq lines up with the log a later open will read — so it may
//!   only fill keys no sequenced row holds.
//! - **A baseline discards every cached row first,** regardless of seq: the connected session is the
//!   truth about that session, and a checkpoint view is outranked wholesale rather than per key.
//! - **A key a baseline omits clears,** unless a newer sequenced row already superseded the cut. The
//!   baseline is a consistent cut, so "the host computes no value for this key as of seq N" is
//!   information; keeping the old value would show a capability that is no longer registered.
//! - **`clear()` drops values and watermarks but keeps subscribed faces.** A client generation change
//!   invalidates everything the previous connection computed, while the components watching those
//!   keys are the same components — dropping their faces would make them resubscribe on every
//!   reconnect.
//! - **Publication is batched, and a batch is generation-guarded.** Twenty frames inside one microtask
//!   are one notification, not twenty, and a superseded schedule must not publish the batch that
//!   replaced it (a controlled input would otherwise notify twice in one tick and lose the caret).
//! - **The rebuild runs before the listeners do, and only if someone is listening.** A listener that
//!   reads the snapshot in its callback must see the change it was told about; with no listener the
//!   dirty flag stays set and the next read rebuilds synchronously.
//!
//! # What this module deliberately does not do
//!
//! - No React hooks and no `requestAnimationFrame`. Upstream's `frame` scheduling defers to a browser
//!   repaint; this crate has no render loop to defer to, so [`PublishKind::Frame`] is *recorded* and
//!   closed by the same flush the microtask batch uses. A caller on a render loop drives it through
//!   [`Notifier::complete_schedule`]; recording the kind keeps upstream's distinction visible instead
//!   of silently collapsing the two.
//! - No `useProjection` selector overload. Its `selector`/`eq` pair is a hook-shape concern with no
//!   Rust analogue; [`ProjectionValueStore::value`] hands back a cloned value and the caller compares.
//! - No `Object.freeze`. Rust's type system already refuses mutation through a shared read.
//! - No cross-thread notification. Listeners are `Fn()` called by the thread that flushes — the same
//!   thread that wrote, which is the ordering a UI needs. The store is `Send + Sync` anyway, so a
//!   caller may flush from wherever it serialises its own work.
//!
//! # Module-include note
//!
//! Both this file and its test are included by path from `tests/session_engine_gap.rs`, so every
//! reference to the library goes through `girder_core::…` rather than `crate::…`: inside a
//! `#[path]` module, `crate::` names the *test* crate.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// The sequence cursor a projection value is consistent with.
///
/// Upstream's `SessionSeqCursor`. An alias rather than a newtype because it is compared against, and
/// sourced from, the same log positions [`girder_core::ProjectionCache`] folds: a second integer type
/// here would add conversions around every read and buy nothing.
pub type SeqCursor = u64;

/// How a value reached the client, which is also the precedence class it competes in.
///
/// Upstream keeps this in an anonymous union (`{ kind: 'cached' } | { kind: 'sequenced', seq }`); an
/// enum is the same two states, with the seq that `cached` deliberately does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    /// A view of the persisted checkpoint, carrying no comparable seq.
    Cached,
    /// A value the host computed for this session, consistent with `seq`.
    Sequenced(SeqCursor),
}

/// One key's accepted row.
#[derive(Clone, Debug)]
struct Row {
    kind: RowKind,
    value: serde_json::Value,
}

/// What a scheduled batch was scheduled as.
///
/// Upstream chooses between `queueMicrotask` and `requestAnimationFrame`; the difference only matters
/// once there is a render loop, which is why the choice is recorded rather than acted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishKind {
    /// Microtask batching: publish at the end of the current task.
    Microtask,
    /// Frame batching: publish at most once per repaint.
    Frame,
}

/// The outcome of one [`Notifier::flush`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publish {
    /// Nothing was pending, so no listener ran.
    Idle,
    /// The listeners were called once.
    Published,
    /// A batch was pending but every listener has unsubscribed.
    ///
    /// Distinct from [`Publish::Idle`] because upstream leaves the dirty flag alone here on purpose:
    /// with no observer, the next read rebuilds synchronously through [`Notifier::ensure_fresh`]
    /// instead of doing work nobody sees.
    Unobserved,
}

/// The handle [`Notifier::subscribe`] returns.
///
/// A newtype rather than a bare `u64` so it cannot be confused with a session sequence or a schedule
/// generation — both integers that flow through this module. The numbers come from one process-wide
/// counter rather than a per-notifier one: a token stored from one store must not silently unsubscribe
/// a listener on another that happened to hand out the same number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenerId {
    /// The process-wide subscription number.
    id: u64,
}

/// Hands out [`ListenerId`] numbers unique across every notifier in the process.
static NEXT_LISTENER: AtomicU64 = AtomicU64::new(0);

/// A listener and its identity.
///
/// Upstream holds a `Set<() => void>`, where the closure's *identity* is what makes the returned
/// unsubscribe function target that one subscription. A boxed `Fn` has no usable identity in Rust, so
/// subscriptions are numbered and unsubscribing is by number.
#[derive(Clone)]
struct ListenerSlot {
    id: u64,
    listener: Arc<dyn Fn() + Send + Sync>,
}

/// The pending-publication state a [`Notifier`] carries between calls.
#[derive(Clone, Copy, Debug, Default)]
struct Batch {
    dirty: bool,
    notify_pending: bool,
    /// The pending publication's kind, when one is scheduled.
    pending: Option<PublishKind>,
}

/// Dirty tracking and batched publication for one observable.
///
/// The store owns one per key plus one for the "any key" channel, exactly as upstream does; the type
/// is public so a caller building another observable over the same data gets the same batching.
pub struct Notifier {
    listeners: Mutex<Vec<ListenerSlot>>,
    batch: Mutex<Batch>,
    generation: AtomicU64,
    rebuilds: AtomicU64,
    rebuild: Box<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Listeners are not named: a debug line reports the batching state, not a list of closures.
        let batch = self.lock_batch();
        f.debug_struct("Notifier")
            .field("listeners", &self.lock_listeners().len())
            .field("dirty", &batch.dirty)
            .field("notify_pending", &batch.notify_pending)
            .field("pending", &batch.pending)
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .field("rebuilds", &self.rebuilds.load(Ordering::Relaxed))
            .field("rebuild", &"<closure>")
            .finish_non_exhaustive()
    }
}

impl Notifier {
    /// Builds a notifier around a snapshot rebuild function.
    ///
    /// The rebuild runs before subscribers are notified whenever the state is dirty, so a listener
    /// that reads the snapshot in its callback sees the change it was told about. The projection store
    /// passes a no-op: its faces read rows directly and have no snapshot to rebuild.
    #[must_use]
    pub fn new(rebuild: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            listeners: Mutex::new(Vec::new()),
            batch: Mutex::new(Batch::default()),
            generation: AtomicU64::new(0),
            rebuilds: AtomicU64::new(0),
            rebuild: Box::new(rebuild),
        }
    }

    /// Subscribes a listener.
    ///
    /// The returned token unsubscribes it. It is a plain value rather than a guard object because
    /// upstream's unsubscribe function may be called more than once, and after the owner is gone; an
    /// id behaves the same way without needing a lifetime tied to the notifier.
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> ListenerId {
        let id = NEXT_LISTENER.fetch_add(1, Ordering::Relaxed);
        self.lock_listeners().push(ListenerSlot {
            id,
            listener: Arc::new(listener),
        });
        ListenerId { id }
    }

    /// Removes the subscription `id` names. Idempotent, and a no-op for a foreign id.
    pub fn unsubscribe(&self, id: ListenerId) {
        self.lock_listeners().retain(|slot| slot.id != id.id);
    }

    /// How many listeners are currently subscribed.
    #[must_use]
    pub fn listener_count(&self) -> usize {
        self.lock_listeners().len()
    }

    /// Marks the state dirty and schedules a microtask publication.
    ///
    /// Scheduling again while anything is already pending keeps the earlier batch: upstream's guard is
    /// `if (this.scheduled === 'microtask') return`, so a write inside an outstanding batch neither
    /// creates a second schedule nor downgrades a frame batch.
    pub fn mark_dirty(&self) {
        self.mark(PublishKind::Microtask);
    }

    /// Marks the state dirty and schedules at most one publication per frame.
    ///
    /// Only the first schedule after an idle period wins, which is what coalesces a burst of frame
    /// writes into one repaint — and, in this crate, into one flush.
    pub fn mark_frame_dirty(&self) {
        self.mark(PublishKind::Frame);
    }

    /// Publishes immediately, superseding any scheduled batch.
    ///
    /// A controlled input writes and needs its listener to run in the same tick; deferring to a
    /// microtask would let the framework re-render from the stale snapshot first.
    pub fn notify_now(&self) -> Publish {
        self.invalidate_schedule();
        self.flush()
    }

    /// Rebuilds synchronously if dirty, without consuming the pending notification.
    ///
    /// The read path: a caller about to return a snapshot may need it fresh even though nobody has
    /// been told yet, and telling them here would notify during a render. Returns whether it rebuilt.
    pub fn ensure_fresh(&self) -> bool {
        let dirty = {
            let mut batch = self.lock_batch();
            if !batch.dirty {
                return false;
            }
            batch.dirty = false;
            true
        };
        if dirty {
            self.run_rebuild();
        }
        dirty
    }

    /// Whether a publication is scheduled but not yet delivered.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        let batch = self.lock_batch();
        batch.notify_pending || batch.pending.is_some()
    }

    /// The kind of the outstanding schedule, if any.
    #[must_use]
    pub fn scheduled_kind(&self) -> Option<PublishKind> {
        self.lock_batch().pending
    }

    /// How many times the rebuild function has run.
    ///
    /// Exposed because "three writes, one rebuild" is the property batching exists for, and asserting
    /// it from the outside is otherwise impossible.
    #[must_use]
    pub fn rebuild_count(&self) -> u64 {
        self.rebuilds.load(Ordering::Relaxed)
    }

    /// The current schedule generation, which a scheduled callback must carry.
    ///
    /// Bumped by every schedule and every invalidation, so a callback can prove it still owns the
    /// batch it was created for.
    #[must_use]
    pub fn schedule_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Delivers the pending batch, if there is one and anyone is listening.
    ///
    /// This is what a scheduled callback (or a caller with its own render loop) invokes.
    pub fn flush(&self) -> Publish {
        let listeners = {
            let mut batch = self.lock_batch();
            batch.pending = None;
            if !batch.notify_pending {
                return Publish::Idle;
            }
            let listeners = self.lock_listeners().clone();
            if listeners.is_empty() {
                // Lazy: the dirty flag stays set, so the next `ensure_fresh` rebuilds for whoever
                // eventually subscribes.
                return Publish::Unobserved;
            }
            batch.notify_pending = false;
            let rebuild = std::mem::take(&mut batch.dirty);
            (listeners, rebuild)
        };
        let (listeners, rebuild) = listeners;
        if rebuild {
            self.run_rebuild();
        }
        for slot in listeners {
            (slot.listener)();
        }
        Publish::Published
    }

    /// Completes the batch one generation scheduled, or drops it when it was superseded.
    ///
    /// `None` means the batch was superseded: [`Notifier::notify_now`] already published it, or a
    /// later schedule replaced it. A caller keeping its own timer closes the batch exactly as
    /// upstream's `queueMicrotask` callback does.
    pub fn complete_schedule(&self, generation: u64) -> Option<Publish> {
        if generation != self.schedule_generation() {
            return None;
        }
        Some(self.flush())
    }

    /// Invalidates any scheduled batch without publishing it.
    ///
    /// [`Notifier::notify_now`] calls this before its synchronous flush: a batch whose callback has
    /// not run yet must not run later and notify a second time.
    pub fn invalidate_schedule(&self) {
        let _ = self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a dirty mark and schedules the batch when nothing is pending.
    fn mark(&self, kind: PublishKind) {
        let mut batch = self.lock_batch();
        batch.dirty = true;
        batch.notify_pending = true;
        if batch.pending.is_some() {
            return;
        }
        batch.pending = Some(kind);
        let _ = self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn run_rebuild(&self) {
        self.rebuilds.fetch_add(1, Ordering::Relaxed);
        (self.rebuild)();
    }

    fn lock_listeners(&self) -> MutexGuard<'_, Vec<ListenerSlot>> {
        self.listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_batch(&self) -> MutexGuard<'_, Batch> {
        self.batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One key's observable face: the identity-stable handle a view subscribes to.
///
/// Upstream keeps this inline in a `Channel` record. A struct with a name is what lets
/// [`ProjectionValueStore::face`] hand out an owned handle instead of a borrow of the store's map.
pub struct Channel {
    key: String,
    notifier: Notifier,
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("key", &self.key)
            .field("notifier", &self.notifier)
            .finish_non_exhaustive()
    }
}

impl Channel {
    /// The projection key this face reads.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Subscribes to this key's changes.
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> ListenerId {
        self.notifier.subscribe(listener)
    }

    /// Removes a subscription to this key.
    pub fn unsubscribe(&self, id: ListenerId) {
        self.notifier.unsubscribe(id);
    }

    /// How many listeners this key has.
    #[must_use]
    pub fn listener_count(&self) -> usize {
        self.notifier.listener_count()
    }

    /// Whether this key has a batch waiting to be published.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.notifier.is_pending()
    }

    /// Delivers this key's pending batch.
    pub fn flush(&self) -> Publish {
        self.notifier.flush()
    }
}

/// One session's accepted projection values.
///
/// The store is the client's only computation-free sink for host-computed values: dropping a frame in
/// here is the whole client-side "folding" story, which is why precedence is the entire contract.
pub struct ProjectionValueStore {
    rows: Mutex<BTreeMap<String, Row>>,
    /// Per-key faces, created on demand and never removed.
    channels: Mutex<BTreeMap<String, Arc<Channel>>>,
    /// The coarse any-key channel (no snapshot cache to rebuild: reads hit rows directly).
    any: Notifier,
}

impl std::fmt::Debug for ProjectionValueStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Values are not printed: a projection can hold a transcript, and a debug line should name
        // the store rather than dump it.
        let rows = self.lock_rows();
        f.debug_struct("ProjectionValueStore")
            .field("keys", &rows.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Default for ProjectionValueStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectionValueStore {
    /// Creates an empty store: every key reads as capability-absent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            channels: Mutex::new(BTreeMap::new()),
            any: Notifier::new(|| {}),
        }
    }

    /// The identity-stable face for one key.
    ///
    /// Always returns a face, even for a key that has never carried a value: a component may
    /// subscribe before the host has computed anything, which is why upstream's `faceOf` is total and
    /// why absence is an `undefined` snapshot rather than a missing face. The face is created once and
    /// cached, so two calls for one key compare by [`Arc::ptr_eq`].
    #[must_use]
    pub fn face(&self, key: &str) -> Arc<Channel> {
        let mut channels = self.lock_channels();
        if let Some(channel) = channels.get(key) {
            return Arc::clone(channel);
        }
        let channel = Arc::new(Channel {
            key: key.to_owned(),
            // No snapshot cache to rebuild: the face reads rows directly.
            notifier: Notifier::new(|| {}),
        });
        channels.insert(key.to_owned(), Arc::clone(&channel));
        channel
    }

    /// One key's current whole value, or `None` when the key is absent.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<serde_json::Value> {
        self.lock_rows().get(key).map(|row| row.value.clone())
    }

    /// One key's accepted host watermark.
    ///
    /// `None` for both an absent key and a cached row: a cached row has no comparable seq, and
    /// returning its stored one would let a caller compare across connection generations.
    #[must_use]
    pub fn seq_of(&self, key: &str) -> Option<SeqCursor> {
        match self.lock_rows().get(key).map(|row| row.kind) {
            Some(RowKind::Sequenced(seq)) => Some(seq),
            _ => None,
        }
    }

    /// Whether a key holds a row at all, of either kind.
    ///
    /// Separate from [`ProjectionValueStore::value`] because a row may legitimately hold JSON `null`:
    /// "present and null" and "absent" are different capability states.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.lock_rows().contains_key(key)
    }

    /// Whether a key's row came from the persisted checkpoint rather than the host.
    #[must_use]
    pub fn is_cached(&self, key: &str) -> bool {
        matches!(
            self.lock_rows().get(key).map(|row| row.kind),
            Some(RowKind::Cached)
        )
    }

    /// Every current value, keyed by projection key.
    ///
    /// A clone rather than a memoised snapshot: upstream freezes and caches the object because React
    /// compares it by identity on every render, and Rust has no such comparison to satisfy.
    #[must_use]
    pub fn values(&self) -> BTreeMap<String, serde_json::Value> {
        self.lock_rows()
            .iter()
            .map(|(key, row)| (key.clone(), row.value.clone()))
            .collect()
    }

    /// The keys that currently hold a row, sorted.
    ///
    /// Sorted because a view rendered from these would otherwise jump between reads.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.lock_rows().keys().cloned().collect()
    }

    /// How many keys hold a row.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_rows().len()
    }

    /// Whether no key holds a row.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock_rows().is_empty()
    }

    /// Subscribes to any-key changes, batched.
    ///
    /// The coarse channel is for consumers that watch the whole set: upstream's session list rebuilds
    /// from it after reading the `title` key and does not need a per-key face.
    pub fn subscribe_any(&self, listener: impl Fn() + Send + Sync + 'static) -> ListenerId {
        self.any.subscribe(listener)
    }

    /// Removes an any-key subscription.
    pub fn unsubscribe_any(&self, id: ListenerId) {
        self.any.unsubscribe(id);
    }

    /// Applies one finished value from the session control stream.
    ///
    /// Higher seq wins among sequenced rows: a lower-or-equal seq loses, which is what makes a
    /// replayed frame a no-op. A cached row has no comparable seq and always yields, which is what
    /// lets the first host frame for a key take it over from the list's checkpoint view.
    ///
    /// Returns whether the row changed, so a caller that already knows it has nothing to publish need
    /// not re-read the store.
    pub fn apply(&self, key: &str, value: serde_json::Value, seq: SeqCursor) -> bool {
        {
            let rows = self.lock_rows();
            if let Some(Row {
                kind: RowKind::Sequenced(current),
                ..
            }) = rows.get(key)
                && seq <= *current
            {
                // A replay, or a frame older than the accepted one: no change, no notification, and a
                // later read still reports the newer seq.
                return false;
            }
        }
        self.lock_rows().insert(
            key.to_owned(),
            Row {
                kind: RowKind::Sequenced(seq),
                value,
            },
        );
        self.changed(key);
        true
    }

    /// Fills keys from a host block labelled `cached`: a zero-I/O view of the persisted checkpoint.
    ///
    /// A cached value lands only where no *sequenced* row exists. A key already holding a cached row is
    /// overwritten: both rows are the same class, and a refreshed view of the checkpoint is the better
    /// one — the session list must not keep showing a title the checkpoint has since replaced.
    ///
    /// Returns how many keys changed.
    pub fn apply_cached(&self, values: &BTreeMap<String, serde_json::Value>) -> usize {
        let mut changed: Vec<&str> = Vec::new();
        {
            let mut rows = self.lock_rows();
            for (key, value) in values {
                if matches!(
                    rows.get(key).map(|row| row.kind),
                    Some(RowKind::Sequenced(_))
                ) {
                    // The connected session has already answered for this key, and a checkpoint view
                    // cannot be newer than it.
                    continue;
                }
                rows.insert(
                    key.clone(),
                    Row {
                        kind: RowKind::Cached,
                        value: value.clone(),
                    },
                );
                changed.push(key.as_str());
            }
        }
        for key in &changed {
            self.changed(key);
        }
        changed.len()
    }

    /// Seeds the store from a history tail page's projections block.
    ///
    /// Three passes, in this order, because each one changes what the next may see:
    ///
    /// 1. Every cached row is discarded, regardless of seq: the block comes from the connected
    ///    session, and a value viewed from the persisted checkpoint never outranks it.
    /// 2. Every carried key lands under the same rule as a frame ([`ProjectionValueStore::apply`]), so
    ///    a frame that already advanced past the cut is not regressed by a stale baseline.
    /// 3. Every remaining row the block omits is cleared *unless* its seq is newer than the cut: a key
    ///    absent from the cut means the capability is absent as of that cut, not that a frame arriving
    ///    afterwards should be forgotten.
    ///
    /// Returns the keys that hold no value afterwards, sorted. A cached row the block carries is swept
    /// and re-seeded rather than removed, so it is not reported; the rows the block carried are
    /// republished by [`ProjectionValueStore::apply`].
    pub fn seed(
        &self,
        as_of_seq: SeqCursor,
        values: &BTreeMap<String, serde_json::Value>,
    ) -> Vec<String> {
        let mut removed: Vec<String> = Vec::new();

        // Pass 1: cached rows never survive a baseline.
        {
            let mut rows = self.lock_rows();
            let cached: Vec<String> = rows
                .iter()
                .filter(|(_, row)| row.kind == RowKind::Cached)
                .map(|(key, _)| key.clone())
                .collect();
            for key in cached {
                rows.remove(&key);
                removed.push(key);
            }
        }

        // Pass 2: seed under the sequenced rule.
        for (key, value) in values {
            self.apply(key, value.clone(), as_of_seq);
        }

        // Pass 3: keys the cut omits are capability-absent as of the cut.
        {
            let mut rows = self.lock_rows();
            let stale: Vec<String> = rows
                .iter()
                .filter(|(key, row)| {
                    if values.contains_key(*key) {
                        return false;
                    }
                    match row.kind {
                        // Pass 1 already removed every cached row; the arm stays explicit so a writer
                        // that reintroduces one cannot silently pin a key against a baseline.
                        RowKind::Cached => true,
                        RowKind::Sequenced(seq) => seq <= as_of_seq,
                    }
                })
                .map(|(key, _)| key.clone())
                .collect();
            for key in stale {
                rows.remove(&key);
                removed.push(key);
            }
        }

        // A cached row that the block carries was swept in pass 1 and re-seeded in pass 2; it is
        // present, so reporting it as removed would make a caller drop a value it just accepted.
        removed.retain(|key| !values.contains_key(key));
        removed.sort_unstable();
        removed.dedup();
        for key in &removed {
            self.channel_changed(key);
        }
        if !removed.is_empty() {
            self.any.mark_dirty();
        }
        removed
    }

    /// Discards every value and watermark while preserving subscribed faces.
    ///
    /// A client generation change invalidates everything the previous connection computed, but the
    /// components watching it are the same components — dropping their faces would make them
    /// resubscribe on every reconnect.
    ///
    /// Returns the keys that were dropped, sorted.
    pub fn clear(&self) -> Vec<String> {
        let keys: Vec<String> = {
            let mut rows = self.lock_rows();
            let keys: Vec<String> = rows.keys().cloned().collect();
            rows.clear();
            keys
        };
        for key in &keys {
            self.channel_changed(key);
        }
        if !keys.is_empty() {
            self.any.mark_dirty();
        }
        keys
    }

    /// Delivers every pending publication on this store: per-key faces first, the coarse channel last.
    ///
    /// A caller with its own event loop calls this from its frame callback; one without calls it at
    /// the end of the task that wrote.
    pub fn flush(&self) -> Vec<Publish> {
        let channels: Vec<Arc<Channel>> = self.lock_channels().values().cloned().collect();
        let mut published: Vec<Publish> = channels.iter().map(|channel| channel.flush()).collect();
        published.push(self.any.flush());
        published
    }

    fn changed(&self, key: &str) {
        self.channel_changed(key);
        self.any.mark_dirty();
    }

    fn channel_changed(&self, key: &str) {
        if let Some(channel) = self.lock_channels().get(key) {
            channel.notifier.mark_dirty();
        }
    }

    fn lock_rows(&self) -> MutexGuard<'_, BTreeMap<String, Row>> {
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_channels(&self) -> MutexGuard<'_, BTreeMap<String, Arc<Channel>>> {
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
