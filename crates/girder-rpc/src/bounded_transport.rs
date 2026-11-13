//! A bounded outbound channel that keeps the transport's `send` synchronous.
//!
//! `serve_full` builds its downlink queue with `mpsc::unbounded_channel`, so a peer that keeps
//! writing and never reads makes the queue grow until the process runs out of memory. The primitive
//! that fixes this already exists — [`girder_core::BoundedQueue`] — but swapping it in is not one
//! type name: `tokio::sync::mpsc::channel` makes every `send` asynchronous, and the RPC downlink has
//! call sites that are not in an `async` context. Making those `async` would ripple through
//! `ClientHandle`, `forward_events`, `SubscriptionHub` and every caller of both, for a queue that
//! only has to stop growing.
//!
//! This module is that missing adapter. It exposes exactly the *synchronous* shape the transport
//! already uses — `send(&self, frame) -> SendOutcome` — and bounds the buffer behind it. Waking a
//! waiting writer is still a task's job, and [`Receiver::recv`] is the only asynchronous operation.
//!
//! # Why the overflow policy is `Reject`
//!
//! [`girder_core::Overflow::DropOldest`] is the right policy for a pure event stream, but the
//! downlink is not one stream: it carries [`DownlinkFrame::Ready`], which binds a generation, and
//! [`DownlinkFrame::Waterfall`], which the client must answer or the host's waterfall chain stalls
//! forever. Evicting the oldest frame would silently discard those in favour of newer `Emit` frames,
//! and it would also hide *which* frame was lost — eviction happens inside the queue, which knows
//! nothing about frame kinds.
//!
//! `Reject` hands the refused frame back, so this adapter can classify it and count the loss per
//! kind before dropping it. That is the whole reason for the wrapper: "one `Emit` was dropped" and
//! "one `Ready` was dropped" are different incidents, and a single total would make them
//! indistinguishable in a log or a metric.
//!
//! # What closing means
//!
//! The semantics match `mpsc::UnboundedSender`/`UnboundedReceiver` so the writer loop
//! (`while let Some(frame) = rx.recv().await`) keeps its current shape:
//!
//! - Dropping the last [`Outbound`] closes the queue; [`Receiver::recv`] still yields every accepted
//!   frame and then returns `None`.
//! - Dropping the [`Receiver`] closes the queue too. That direction matters more: once the writer is
//!   gone, nothing drains the buffer, so continuing to accept frames would reintroduce exactly the
//!   unbounded growth this module exists to prevent. Senders get [`SendOutcome::Closed`] instead.
//!
//! # Zero capacity
//!
//! [`BoundedQueue`] panics on a zero capacity, and that panic is the wrong failure mode for a
//! network path — a misconfigured value would abort a connection setup rather than report itself. A
//! zero-capacity outbound can never deliver a frame, so this adapter closes it at construction: the
//! first `send` returns [`SendOutcome::Closed`], `recv` returns `None` immediately, and nothing is
//! left parked. The condition is logged at `error` level once, since a silently dead connection is
//! worse than a loud one.
//!
//! # Using it
//!
//! ```text
//! let (outbound, mut receiver) = Outbound::new(256);
//! match outbound.send(frame) {
//!     SendOutcome::Queued => {}          // accepted
//!     SendOutcome::Dropped => {}         // full; counted per frame kind
//!     SendOutcome::Closed => {}          // the connection is gone
//! }
//! while let Some(frame) = receiver.recv().await { /* write it to the socket */ }
//! ```

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use girder_core::{BoundedQueue, Overflow};

use crate::DownlinkFrame;

/// Which protocol frame a value is, for attributing a drop to its consequence.
///
/// The variants mirror [`DownlinkFrame`] exactly. A separate enum rather than a borrow of the frame
/// is deliberate: the frame is moved into the queue — or dropped — before the count is taken, so the
/// classification has to survive the move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FrameKind {
    /// [`DownlinkFrame::Ready`]: binds a generation. Losing it makes the connection unusable.
    Ready,
    /// [`DownlinkFrame::Emit`]: one notification. Losing it loses an event, not the connection.
    Emit,
    /// [`DownlinkFrame::Waterfall`]: the client must answer it, or the host stalls on a timeout.
    Waterfall,
    /// [`DownlinkFrame::Cancel`]: abandons a waterfall the client may still be working on.
    Cancel,
    /// [`DownlinkFrame::Subscription`]: one value or the end of a subscription series.
    Subscription,
    /// [`DownlinkFrame::SubscribeResult`]: the verdict on a subscription request.
    SubscribeResult,
}

impl FrameKind {
    /// The number of variants, used to size the per-kind counters.
    const COUNT: usize = 6;

    /// Classifies a frame without consuming it.
    #[must_use]
    pub fn of(frame: &DownlinkFrame) -> Self {
        match frame {
            DownlinkFrame::Ready { .. } => Self::Ready,
            DownlinkFrame::Emit { .. } => Self::Emit,
            DownlinkFrame::Waterfall { .. } => Self::Waterfall,
            DownlinkFrame::Cancel { .. } => Self::Cancel,
            DownlinkFrame::Subscription { .. } => Self::Subscription,
            DownlinkFrame::SubscribeResult { .. } => Self::SubscribeResult,
        }
    }

    /// The wire discriminator, for logs and metrics.
    ///
    /// Returns the same spelling `serde` emits for the variant, so a log line can be matched against
    /// a captured trace without a translation table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Emit => "emit",
            Self::Waterfall => "waterfall",
            Self::Cancel => "cancel",
            Self::Subscription => "subscription",
            Self::SubscribeResult => "subscribeResult",
        }
    }

    /// The index into the per-kind counters.
    const fn index(self) -> usize {
        match self {
            Self::Ready => 0,
            Self::Emit => 1,
            Self::Waterfall => 2,
            Self::Cancel => 3,
            Self::Subscription => 4,
            Self::SubscribeResult => 5,
        }
    }
}

impl std::fmt::Display for FrameKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The result of one synchronous attempt to enqueue a frame.
///
/// [`SendOutcome::Dropped`] and [`SendOutcome::Closed`] are deliberately distinct: a full queue is
/// transient and the connection is still usable, while a closed queue means the writer is gone and
/// the caller should stop producing. Collapsing them into one `false` — which is what a
/// `Result`-shaped API encourages — would make `forward_events` stop watching a bus that is still
/// connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SendOutcome {
    /// The frame is in the queue and will be delivered.
    Queued,
    /// The queue was full, so the frame was discarded. Counted per [`FrameKind`].
    Dropped,
    /// The channel is closed, so the frame was discarded without being counted as overflow.
    Closed,
}

impl SendOutcome {
    /// Whether the frame was accepted.
    ///
    /// This is the drop-in replacement for the `bool` that `UnboundedSender::send(frame).is_ok()`
    /// produced, with one important difference: `false` no longer means "disconnected". A caller that
    /// only breaks its loop on a dead connection must test [`SendOutcome::Closed`] instead, or it
    /// will tear down a healthy generation over one dropped event.
    #[must_use]
    pub fn is_queued(self) -> bool {
        self == Self::Queued
    }

    /// Whether the channel has no receiver left.
    #[must_use]
    pub fn is_closed(self) -> bool {
        self == Self::Closed
    }
}

/// Drop counters, split so a lost `Ready` is not averaged away with a lost `Emit`.
#[derive(Debug, Default)]
struct Counts {
    /// One counter per [`FrameKind`], indexed by [`FrameKind::index`].
    by_kind: [u64; FrameKind::COUNT],
    /// The sum of `by_kind`, kept separately so [`Outbound::dropped`] is one load, not six.
    total: u64,
}

/// The state every handle shares.
struct Shared {
    /// The capacity this channel was created with, reported in diagnostics.
    capacity: usize,
    /// The buffer itself. `Reject` is what makes a synchronous `send` possible and a refused frame
    /// available for classification; see the module documentation.
    queue: BoundedQueue<DownlinkFrame>,
    /// Filled under this mutex rather than with atomics so a total and its parts can never disagree,
    /// even if a reader samples both concurrently.
    counts: Mutex<Counts>,
    /// How many [`Outbound`] handles exist. The last drop closes the queue, which is what makes
    /// `recv` return `None` once nothing can produce again.
    senders: AtomicUsize,
    /// Whether the once-per-channel full-queue warning has been logged.
    ///
    /// Logging on every drop would turn one stalled client into a log flood — the drop path is the
    /// one place where the observability has to be bounded too.
    warned: AtomicBool,
}

impl Shared {
    /// Takes the counter lock, treating poisoning as a non-event.
    ///
    /// No user code runs under this lock and the only mutations are increments, so a panic elsewhere
    /// cannot have left the counters inconsistent. Propagating the poison would turn an unrelated
    /// panic into a failure of every later send on this connection.
    fn lock_counts(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The producing side: a bounded, non-blocking downlink queue.
///
/// Cheap to clone — clones share the buffer and count as senders, so the channel stays open as long
/// as any part of the connection holds one. `send` takes `&self` and never waits, which is what lets
/// it be called from a synchronously-polled task (the event forwarder) and from request handling
/// without colouring either.
pub struct Outbound {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Outbound {
    // Frames are elided: they carry event payloads, and a debug log of a full queue would be larger
    // than the queue it describes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outbound")
            .field("capacity", &self.shared.capacity)
            .field("queued", &self.queued())
            .field("dropped", &self.dropped())
            .finish()
    }
}

impl Outbound {
    /// Creates an outbound channel with room for `capacity` queued frames.
    ///
    /// Returns the producing handle and the one receiving handle. Capacity zero is accepted but
    /// results in a closed channel; see the module documentation for why that beats panicking.
    ///
    /// # Panics
    ///
    /// Does not panic. The bound is clamped to one for [`BoundedQueue`], whose constructor refuses
    /// zero; the zero case closes the queue instead of forwarding that panic to a connection setup.
    #[must_use]
    pub fn new(capacity: usize) -> (Self, Receiver) {
        let shared = Arc::new(Shared {
            capacity,
            queue: BoundedQueue::with_overflow(capacity.max(1), Overflow::Reject),
            counts: Mutex::new(Counts::default()),
            senders: AtomicUsize::new(1),
            warned: AtomicBool::new(false),
        });

        if capacity == 0 {
            // The bound is one so `BoundedQueue` can exist, but a zero capacity means "deliver
            // nothing". Leaving it open would drop every frame while parking the writer forever,
            // which looks like a hung connection instead of a misconfiguration.
            tracing::error!(
                "outbound channel created with zero capacity; closing it, no frame can be delivered"
            );
            shared.queue.close();
        }

        let outbound = Self {
            shared: Arc::clone(&shared),
        };
        let receiver = Receiver {
            queue: shared.queue.clone(),
            shared,
        };
        (outbound, receiver)
    }

    /// Enqueues a frame without waiting.
    ///
    /// Always returns immediately. When the queue is full the frame is discarded and the loss is
    /// counted against its [`FrameKind`]; when the channel is closed it is discarded uncounted,
    /// because a closed queue is not overflow — it is a connection that no longer exists.
    ///
    /// A refusal caused by a close that lands between the enqueue attempt and the closed check is
    /// reported as [`SendOutcome::Closed`] rather than counted as a drop: the terminal condition is
    /// the one the caller has to act on, and a race that resolves towards "stop producing" is the
    /// safe direction.
    pub fn send(&self, frame: DownlinkFrame) -> SendOutcome {
        // Captured before the move: after `try_send` the refused value is owned again, but under
        // `DropOldest` it would be the evicted frame, whose kind this adapter never sees. `Reject`
        // is what makes this line possible.
        let kind = FrameKind::of(&frame);

        match self.shared.queue.try_send(frame) {
            Ok(()) => SendOutcome::Queued,
            Err(refused) => {
                drop(refused);
                if self.shared.queue.is_closed() {
                    SendOutcome::Closed
                } else {
                    self.record_drop(kind);
                    SendOutcome::Dropped
                }
            }
        }
    }

    /// How many frames are queued right now.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.shared.queue.len()
    }

    /// How many frames were discarded because the queue was full.
    ///
    /// Excludes frames refused by a closed channel: those were never competing for room. A consumer
    /// falling behind is visible here, and stays visible as a rising count rather than as a memory
    /// graph.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.lock_counts().total
    }

    /// How many frames of `kind` were discarded because the queue was full.
    ///
    /// The point of the split: a dropped `ready` breaks the generation outright, a dropped
    /// `waterfall` leaves the host waiting on an answer that will never come, and a dropped `emit`
    /// costs one notification a later frame may supersede.
    #[must_use]
    pub fn dropped_by_kind(&self, kind: FrameKind) -> u64 {
        self.shared.lock_counts().by_kind[kind.index()]
    }

    /// Records one drop and logs the first one for this channel.
    fn record_drop(&self, kind: FrameKind) {
        {
            let mut counts = self.shared.lock_counts();
            counts.total = counts.total.saturating_add(1);
            counts.by_kind[kind.index()] = counts.by_kind[kind.index()].saturating_add(1);
        }

        // Once per channel, not once per drop: see `Shared::warned`.
        if !self.shared.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                capacity = self.shared.capacity,
                kind = kind.as_str(),
                "outbound queue is full; dropping frames until the consumer catches up"
            );
        }
    }
}

impl Clone for Outbound {
    /// Shares the same buffer and registers as another sender.
    ///
    /// This is what keeps the channel open while a `SubscriptionHub` holds a clone: the writer's
    /// `recv` must not return `None` merely because the `ClientHandle` was dropped.
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for Outbound {
    /// Closes the queue when the last sender goes away, so `recv` drains and then ends.
    ///
    /// `AcqRel` on the decrement pairs with the `Relaxed` increment in [`Outbound::clone`]: the only
    /// ordering that matters is "the last drop happens after every earlier clone", which the
    /// release-acquire pair establishes.
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.queue.close();
        }
    }
}

/// The consuming side: the one handle the socket writer task owns.
///
/// `recv` takes `&mut self`, so a design that needs a single consumer owns one handle; producers get
/// [`Outbound`] clones and never touch this type. Ending is part of the contract: `None` means the
/// channel is closed *and* drained, and every frame accepted before the close is delivered first.
pub struct Receiver {
    shared: Arc<Shared>,
    /// This handle's own view of the shared buffer.
    ///
    /// Held separately from [`Shared::queue`] because [`BoundedQueue::recv`] takes `&mut self`: a
    /// queue reached through an `Arc` cannot be borrowed mutably. The clone shares the same state, so
    /// this is one buffer seen through two handles, not a second buffer — and the mutability is
    /// confined to the single consumer, which is exactly the invariant `recv` exists to express.
    queue: BoundedQueue<DownlinkFrame>,
}

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("capacity", &self.shared.capacity)
            .field("queued", &self.shared.queue.len())
            .finish_non_exhaustive()
    }
}

impl Receiver {
    /// Receives the next frame, waiting for one to arrive.
    ///
    /// Returns `None` only once the channel is closed and everything already accepted has been
    /// yielded. The ordering is what lets a connection shut down without losing the last frames it
    /// promised to send.
    pub async fn recv(&mut self) -> Option<DownlinkFrame> {
        // Uses this handle's own `queue` field rather than cloning one per call. The clone is cheap,
        // but it is per frame, and this method is on the hot path of every connection. Clones share
        // one buffer, so producers and this consumer see the same queue.
        self.queue.recv().await
    }
}

impl Drop for Receiver {
    /// Closes the queue when the writer goes away.
    ///
    /// Without this, a dead writer would leave producers enqueueing into a buffer nobody drains —
    /// the unbounded-growth bug in a smaller box. Closing turns that into an immediate
    /// [`SendOutcome::Closed`] at every call site, which is a fact the caller already handles.
    fn drop(&mut self) {
        self.shared.queue.close();
    }
}
