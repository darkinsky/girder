//! A bounded queue whose full-queue behaviour is a policy, not a fixed rule.
//!
//! Every channel in the transport layer is, or should be, bounded: a producer that outruns its
//! consumer is normal (a socket that writes slowly, a client that stopped reading), and an
//! unbounded buffer turns that ordinary condition into unbounded memory. The usual fix — swap
//! `unbounded_channel` for `channel(capacity)` — changes *two* things at once: it adds a bound, and
//! it makes every send asynchronous. That is why this module exists as its own primitive: the bound
//! and the overflow behaviour are separate decisions, and a caller that has not yet chosen a
//! behaviour should be able to see the choice instead of inheriting "block" by default.
//!
//! # The three policies, and which one a given stream wants
//!
//! [`Overflow::Reject`] refuses the new item when the queue is full. It fits work that must not be
//! lost and must not be silently reordered — an uplink result, a request the caller retries,
//! anything where "the queue was full" is a fact the sender has to react to. The price is that every
//! call site has to handle a refusal.
//!
//! [`Overflow::DropOldest`] evicts the front of the queue to make room for the new item. **For an
//! event stream this is the right default, and it is the most useful policy here.** A consumer of
//! emitted state wants the *current* state: the events it missed are superseded by the ones that
//! arrive next, so blocking the producer to preserve history the consumer can never use would trade
//! one slow consumer's latency for everyone's throughput. Blocking also fails in the direction that
//! matters — a WebSocket peer is allowed to write without reading, so a producer blocked on that
//! peer stays blocked until the socket closes, and one stalled client becomes a stalled host.
//! Dropping the oldest keeps the newest, which is the part of the stream a consumer can still act
//! on, and keeps the producer running. The cost is that the stream is lossy, so the loss is counted
//! ([`BoundedQueue::dropped`]) rather than hidden.
//!
//! [`Overflow::Close`] closes the queue when it is full. It fits the case where a full buffer is
//! evidence of a broken peer rather than momentary backpressure: the queue stops accepting, the
//! waiters wake, and the receive side drains what was already accepted before ending. A partial
//! stream that keeps trickling after the peer proved it cannot keep up is harder to reason about
//! than an explicit end.
//!
//! # Why capacity zero is refused
//!
//! A zero-capacity queue is not "a queue with no buffer", it is a queue that cannot make progress:
//! nothing can ever be enqueued, so [`BoundedQueue::send`] waits for a slot that no receive can
//! create and [`BoundedQueue::try_send`] fails forever. What a caller means by "capacity zero" is an
//! unbuffered rendezvous, which is a different primitive with a different guarantee (the sender
//! hands off to a receiver that is already waiting), and expressing it as this type would mean
//! lying about [`BoundedQueue::len`] and about the overflow policies. The constructor therefore
//! refuses it, as `tokio::sync::mpsc::channel` does.
//!
//! # Using it
//!
//! ```text
//! let mut events = BoundedQueue::with_overflow(64, Overflow::DropOldest);
//! events.try_send(frame)?;              // never blocks; evicts the oldest frame when full
//! while let Some(frame) = events.recv().await { /* ... */ }
//! events.close();                       // recv still yields the queued frames, then None
//! ```
//!
//! # Where the call sites move when the transport adopts this
//!
//! [`BoundedQueue::send`] and [`BoundedQueue::recv`] are the only asynchronous operations, so the
//! call sites that must move into an `async` context are exactly the ones that may block. For the
//! RPC server that is a short list: the per-frame pushes from request handling stay synchronous
//! (they use [`BoundedQueue::try_send`], and `Reject` is the policy that keeps them so), while the
//! event forwarder and the socket writer are already tasks and can `await` a `DropOldest` send that,
//! in practice, never suspends. This module deliberately does not make that change; see the last
//! section of its module documentation in the transport for why the two steps are separate.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

/// The two wakeup signals a queue needs.
///
/// Shared next to the state rather than inside it, so a waiter can register before taking the lock;
/// see the ordering note in [`BoundedQueue::send`].
#[derive(Debug, Default)]
struct Wakeups {
    /// A sender is waiting for a free slot.
    room: Notify,
    /// A receiver is waiting for an item.
    ready: Notify,
}

/// What a queue does when a send arrives and there is no room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overflow {
    /// Refuse the new item; the send fails and the queue is unchanged.
    Reject,
    /// Evict the oldest queued item and accept the new one.
    ///
    /// The policy for event streams: a consumer wants the newest state, and blocking a producer on a
    /// consumer that may never read is how one slow peer stalls the whole host.
    DropOldest,
    /// Close the queue; the send fails, waiters wake, and the receive side drains what was accepted.
    Close,
}

/// Returned by [`BoundedQueue::send`] when the queue is closed and the item was not accepted.
///
/// The item is dropped: there is no receiver left to hand it to, and a caller that needs it back
/// should not have sent it to a closed queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded queue is closed")
    }
}

impl std::error::Error for Closed {}

/// The mutex-protected body, shared by every handle.
#[derive(Debug)]
struct State<T> {
    capacity: usize,
    overflow: Overflow,
    queue: Mutex<Inner<T>>,
    wakeups: Wakeups,
}

/// The items plus the flags that describe them.
#[derive(Debug)]
struct Inner<T> {
    items: VecDeque<T>,
    closed: bool,
    /// How many items were discarded because the queue was full.
    ///
    /// A policy that silently throws work away has to be observable, otherwise "the consumer missed
    /// an event" is indistinguishable from "the event was never produced".
    dropped: u64,
}

/// One attempt to enqueue, decided under the lock and acted on after it is released.
enum Attempt<T> {
    /// The item is queued.
    Accepted,
    /// The item broke the bound and the queue refused it; it is returned unchanged.
    Refused(T),
    /// The item is queued and this older item was evicted to make room.
    Evicted(T),
    /// The queue was closed, by an earlier call or by this one, so nothing was accepted.
    Closed(T),
}

/// A bounded queue whose behaviour when full is chosen at construction.
///
/// Producers share one through [`Arc`] and never need `&mut`: [`BoundedQueue::try_send`] and
/// [`BoundedQueue::send`] both take `&self`. The draining side takes `&mut self`, which is what makes
/// one owner the only receiver even when many producers exist.
#[derive(Debug)]
pub struct BoundedQueue<T> {
    state: Arc<State<T>>,
}

impl<T> BoundedQueue<T> {
    /// Creates a queue that holds `capacity` items and refuses new ones when full.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero. A zero-capacity queue can never make progress; an unbuffered
    /// handoff is a different primitive, not a degenerate case of this one.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_overflow(capacity, Overflow::Reject)
    }

    /// Creates a queue that holds `capacity` items with an explicit overflow policy.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero, for the reason given on [`BoundedQueue::new`].
    #[must_use]
    pub fn with_overflow(capacity: usize, overflow: Overflow) -> Self {
        assert!(
            capacity > 0,
            "a bounded queue needs room for at least one item; an unbuffered handoff is a \
             different primitive"
        );
        Self {
            state: Arc::new(State {
                capacity,
                overflow,
                queue: Mutex::new(Inner {
                    items: VecDeque::with_capacity(capacity),
                    closed: false,
                    dropped: 0,
                }),
                wakeups: Wakeups::default(),
            }),
        }
    }

    /// The number of items the queue can hold at once.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.state.capacity
    }

    /// How many items are queued right now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// Whether nothing is queued.
    ///
    /// A closed but not yet drained queue is *not* empty: the items accepted before the close are
    /// still there, which is exactly why [`BoundedQueue::recv`] yields them before returning `None`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the queue has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// How many items this queue has discarded because it was full.
    ///
    /// Non-zero only under [`Overflow::DropOldest`] (or after an [`Overflow::Close`] refusal, which
    /// counts the refused item's slot as no loss and so leaves the count alone). Exposed so a
    /// producer can report that a consumer is falling behind instead of inferring it from timing.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Tries to enqueue an item without waiting.
    ///
    /// On success the item is queued — under [`Overflow::DropOldest`], possibly at the cost of an
    /// older one. On failure the item is handed back unchanged, so the caller can retry, forward it,
    /// or drop it deliberately.
    ///
    /// The error does not say *why*: a full queue and a closed queue both refuse. A caller that has
    /// to tell them apart asks [`BoundedQueue::is_closed`]; a caller that only needs to keep going
    /// treats both as "not accepted", which is the common case.
    ///
    /// # Errors
    ///
    /// Returns the item when the queue is full, when it is closed, or when this call closed it.
    pub fn try_send(&self, item: T) -> Result<(), T> {
        match self.attempt(item) {
            Attempt::Accepted => {
                self.state.wakeups.ready.notify_one();
                Ok(())
            }
            Attempt::Evicted(old) => {
                // Outside the lock: dropping a value may run user code, and user code must never run
                // while this queue's mutex is held.
                drop(old);
                self.state.wakeups.ready.notify_one();
                Ok(())
            }
            Attempt::Refused(item) => Err(item),
            Attempt::Closed(item) => {
                // Harmless when the close came from an earlier call: a closed queue has no waiters,
                // because both wait loops re-check `closed` before sleeping.
                self.wake_everyone();
                Err(item)
            }
        }
    }

    /// Enqueues an item, waiting for room when the queue is full.
    ///
    /// Under [`Overflow::DropOldest`] this never waits: making room is always possible by evicting,
    /// so the only failure is a closed queue. Under [`Overflow::Reject`] the caller waits for a
    /// slot, which is the honest meaning of "bounded" — the producer's rate becomes the consumer's
    /// rate instead of its memory footprint.
    ///
    /// # Errors
    ///
    /// Returns [`Closed`] when the queue is closed, or becomes closed while this call is waiting.
    /// The item is dropped in that case.
    pub async fn send(&self, item: T) -> Result<(), Closed> {
        let mut item = item;
        loop {
            // Registered *before* the queue is inspected, and enabled so the registration is live
            // before the check. `Notify` stores one permit at most for a waiter that arrives later,
            // so a wakeup delivered between the check and the await would otherwise be dropped and
            // this sender would sleep on a queue that already had room.
            let mut waiting = std::pin::pin!(self.state.wakeups.room.notified());
            waiting.as_mut().enable();

            match self.attempt(item) {
                Attempt::Accepted => {
                    self.state.wakeups.ready.notify_one();
                    return Ok(());
                }
                Attempt::Evicted(old) => {
                    drop(old);
                    self.state.wakeups.ready.notify_one();
                    return Ok(());
                }
                Attempt::Refused(returned) => item = returned,
                Attempt::Closed(_) => {
                    self.wake_everyone();
                    return Err(Closed);
                }
            }

            waiting.await;
        }
    }

    /// Receives the oldest queued item, waiting for one to arrive.
    ///
    /// Returns `None` once the queue is closed *and* drained: a close stops new items, it does not
    /// throw away the ones a receiver has not seen yet. That ordering is what lets a producer close
    /// on shutdown and still have the consumer observe everything that was accepted.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            let mut waiting = std::pin::pin!(self.state.wakeups.ready.notified());
            waiting.as_mut().enable();

            {
                let mut inner = self.lock();
                if let Some(item) = inner.items.pop_front() {
                    drop(inner);
                    // Exactly one slot is free, so wake exactly one sender waiting for room. Waking
                    // all of them would cause a thundering herd on every single receive.
                    self.state.wakeups.room.notify_one();
                    return Some(item);
                }
                if inner.closed {
                    return None;
                }
            }

            waiting.await;
        }
    }

    /// Closes the queue.
    ///
    /// Idempotent. Items already queued stay queued, so a receiver drains them before
    /// [`BoundedQueue::recv`] returns `None`; senders waiting for room wake and fail with [`Closed`]
    /// rather than sleeping until the socket times out.
    pub fn close(&self) {
        {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            inner.closed = true;
        }
        self.wake_everyone();
    }

    /// Wakes every waiter, after the closed flag is visible.
    ///
    /// `notify_waiters` rather than `notify_one`: every waiter has to learn about the close, not just
    /// the next one in line. It is called outside the lock so another task's continuation never runs
    /// while this task holds the mutex.
    fn wake_everyone(&self) {
        self.state.wakeups.room.notify_waiters();
        self.state.wakeups.ready.notify_waiters();
    }

    /// Takes the lock, treating poisoning as a non-event.
    ///
    /// No user code runs under this lock — evicted and returned values are dropped after it is
    /// released — so a panic elsewhere cannot have left the queue half-updated. Propagating the
    /// poison would turn an unrelated panic into a permanent outage of the transport.
    fn lock(&self) -> MutexGuard<'_, Inner<T>> {
        self.state.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies the policy once, without notifying anyone.
    ///
    /// One place decides `Reject`, `DropOldest` and `Close`, so a future policy cannot be added to
    /// one entry point and forgotten in the other.
    fn attempt(&self, item: T) -> Attempt<T> {
        let mut inner = self.lock();

        if inner.closed {
            return Attempt::Closed(item);
        }

        if inner.items.len() < self.state.capacity {
            inner.items.push_back(item);
            return Attempt::Accepted;
        }

        match self.state.overflow {
            Overflow::Reject => Attempt::Refused(item),
            Overflow::DropOldest => {
                let old = inner.items.pop_front();
                inner.items.push_back(item);
                inner.dropped = inner.dropped.saturating_add(1);
                match old {
                    Some(old) => Attempt::Evicted(old),
                    // Unreachable with a non-zero capacity, but the item is already queued if it ever
                    // happens, which is the observable outcome the caller expects.
                    None => Attempt::Accepted,
                }
            }
            Overflow::Close => {
                inner.closed = true;
                Attempt::Closed(item)
            }
        }
    }
}
