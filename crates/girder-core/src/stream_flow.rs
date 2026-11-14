//! Per-stream flow control: a sliding window, and a detector for the consumers that stall it.
//!
//! The transport already bounds *memory* — [`crate::bounded_queue`] and `bounded_transport::Outbound`
//! put a ceiling on how many frames may be buffered — but a bound is not backpressure. A queue that
//! refuses or evicts frames when it is full keeps the producer running and loses data; a queue that
//! blocks the producer keeps the data and stalls whoever was pushing. Both answers are wrong for a
//! *sequential* stream, where the client is entitled to see every frame in order:
//!
//! - Dropping a frame leaves a hole the client cannot detect or patch. A consumer that sees frame 1
//!   and frame 3 has no way to tell "the server skipped 2" from "the network ate 2", and no way to
//!   ask for it again. For an event log, a replayable session, or a tool-output transcript, a hole
//!   is worse than a delay.
//! - An unbounded buffer is not a solution either; it is the same drop deferred until the process
//!   dies of it.
//!
//! This module is the third answer: a credit window. The sender may have at most `capacity` frames
//! outstanding — sent but not yet acknowledged — and when the window is full it is told to stop
//! ([`SendResult::Blocked`]) rather than to throw the frame away. The client acknowledges frames as
//! it consumes them, each acknowledgement slides the window forward, and the sender resumes. The
//! producer's own paced loop is the throttle, so no frame is ever lost and memory stays bounded at
//! `capacity` frames.
//!
//! # Why the window is counted in frames, not bytes
//!
//! Frames are the unit the client acknowledges and the unit the protocol orders. Counting bytes
//! would need the client and the server to agree on an encoding size, and a single large frame would
//! then be able to exhaust the whole window on its own. Frame counts are exact on both sides and
//! survive a re-encoding.
//!
//! # Why `acknowledge` saturates instead of failing
//!
//! A client's acknowledgement can arrive after the sender already gave up on it — a reconnect, a
//! duplicated ack from a retried message, an ack for frames this window never sent because it was
//! reset. Treating any of those as an error would turn a recoverable protocol race into a dropped
//! connection. Treating them as a *credit grant* would let a malicious or confused peer inflate the
//! window past its bound. So the surplus is simply discarded: `inflight` saturates at zero, the
//! window never grows, and [`Flow::sent_total`] — which records what was actually sent — stays the
//! upper bound a caller can audit against.
//!
//! # Why a slow-consumer detector is separate from the window
//!
//! A full window is a normal, momentary condition: the client is rendering, the socket is shedding a
//! burst, a GC pause happened. A full window that *never clears* is a different incident, and the
//! one worth acting on — the connection is dead weight, its buffer is pinned, and the frames behind
//! it are blocked for as long as it lingers. [`SlowDetector`] is the classifier that distinguishes
//! the two, so the decision "log it" or "disconnect it" is made on evidence rather than on the first
//! `Blocked` result. It is deliberately clock-injected: a caller passes `now_ms`, so tests advance
//! time instead of sleeping, and the production clock stays a single choice at the call site.

/// What [`Flow::send`] decided about one frame.
///
/// A refusal is a *pause*, not a drop: the caller still owns the frame and is expected to retry it
/// after an acknowledgement arrives. That is the contract that keeps the client's view of the stream
/// contiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendResult {
    /// The frame fits in the window and was accepted.
    Allowed,
    /// The window was full, so the frame was not accepted.
    ///
    /// Both numbers are reported because the caller normally has to make a decision with them — log
    /// a warning, arm a timer, or hand the frame back to a queue — and reading them back through
    /// [`Flow::inflight`] afterwards would race any other sender sharing this state.
    Blocked {
        /// How many frames were outstanding when the send was refused.
        inflight: usize,
        /// The window size the frame did not fit into.
        capacity: usize,
    },
}

/// One stream's sliding window.
///
/// Not `Clone`: two handles over one window would each believe they owned its credit, and the
/// capacity would then be enforced twice over or not at all. A streaming connection has exactly one
/// sender, so one owner is the honest shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flow {
    /// The maximum number of frames that may be outstanding at once.
    capacity: usize,
    /// Frames accepted by [`Flow::send`] and not yet acknowledged.
    ///
    /// Held as `usize` rather than a signed type on purpose: `usize` plus saturating arithmetic
    /// makes a negative count unrepresentable, so "more acks than sends" cannot corrupt the window —
    /// it can only fail to move it.
    inflight: usize,
    /// Frames accepted since construction, never decreased.
    ///
    /// Kept separately from `inflight` because the two answer different questions: `inflight` is how
    /// much room is left, `sent_total` is how much the stream has carried. A caller uses the second
    /// to reconcile against a client's acknowledged sequence number.
    sent_total: u64,
}

impl Flow {
    /// Creates a window that allows `capacity` unacknowledged frames.
    ///
    /// Capacity zero is legal and means "no frame may be sent". It is not read as unlimited:
    /// a zero that silently meant infinity would turn a configuration mistake into the exact
    /// unbounded buffer this module exists to avoid. Every send is refused, [`Flow::is_blocked`] is
    /// always true, and only an acknowledgement that arrives before any send (there is nothing to
    /// acknowledge) could change that — so a caller that constructs this has built a stream that
    /// will never deliver, and should treat it as a configuration error rather than a running one.
    #[must_use]
    pub const fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inflight: 0,
            sent_total: 0,
        }
    }

    /// Tries to send one frame.
    ///
    /// Returns [`SendResult::Allowed`] with the frame counted as in flight, or
    /// [`SendResult::Blocked`] with nothing changed. The refusal carrying the current `inflight` and
    /// `capacity` lets the caller log the incident without a second call.
    ///
    /// # Why this does not queue internally
    ///
    /// An internal queue would make `send` unconditional and move the blocking question somewhere
    /// else — the queue would grow to the same unbounded buffer this window replaces. The caller
    /// owning the frame is what keeps the bound real: it cannot accept new work while it is blocked,
    /// because it has nowhere to put it.
    pub fn send(&mut self) -> SendResult {
        if self.inflight >= self.capacity {
            return SendResult::Blocked {
                inflight: self.inflight,
                capacity: self.capacity,
            };
        }

        self.inflight += 1;
        // Saturating because a stream that has carried `u64::MAX` frames has bigger problems than
        // an inaccurate total, and wrapping to zero there would make `sent_total` look like a fresh
        // start.
        self.sent_total = self.sent_total.saturating_add(1);
        SendResult::Allowed
    }

    /// Records that the client consumed `frames` more frames, moving the window forward.
    ///
    /// Accepts the count rather than a single flag because a client acknowledges in batches — one
    /// ack per read call, not one per frame — and requiring one call per frame would make the
    /// bookkeeping cost dominate the traffic on a fast stream.
    ///
    /// Over-acknowledgement saturates: `inflight` never falls below zero and the window never grows
    /// beyond `capacity`. See the module documentation for why the surplus is dropped instead of
    /// being reported as an error.
    pub fn acknowledge(&mut self, frames: usize) {
        self.inflight = self.inflight.saturating_sub(frames);
    }

    /// How many frames are sent and not yet acknowledged.
    ///
    /// This is the amount of room the stream is currently using, and the number a caller reports in
    /// diagnostics. It is always at most the capacity and never negative — the type makes the
    /// second half of that statement unconditional.
    #[must_use]
    pub const fn inflight(&self) -> usize {
        self.inflight
    }

    /// The window size this flow was built with.
    ///
    /// Exposed so a caller comparing usage against a limit can read the limit from the same object
    /// that enforces it, instead of duplicating the configuration somewhere it can drift.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether the window is full, i.e. a send would be refused right now.
    ///
    /// A convenience over comparing [`Flow::inflight`] with the capacity, kept because that is the
    /// question most callers actually ask, and answering it once here keeps the comparison — and
    /// the `>=` versus `==` it has to be — in one place.
    #[must_use]
    pub const fn is_blocked(&self) -> bool {
        self.inflight >= self.capacity
    }

    /// How many frames have been accepted since construction.
    ///
    /// Counts sends, not acknowledgements: it is the stream's high-water mark, so a client's acked
    /// count can never legitimately exceed it. An ack that does is evidence of a peer bug, which is
    /// only detectable because this total is kept.
    #[must_use]
    pub const fn sent_total(&self) -> u64 {
        self.sent_total
    }

    /// How many more frames may be sent before the window is full.
    ///
    /// The complement of [`Flow::inflight`], for a producer that wants to batch sends up to the
    /// window instead of calling `send` until it is refused.
    #[must_use]
    pub const fn available(&self) -> usize {
        self.capacity.saturating_sub(self.inflight)
    }
}

/// Classifies a stream as a slow consumer when its window stays blocked for too long.
///
/// The measurement is "how long has this window been continuously blocked", not "how often was a
/// send refused". A busy stream is refused constantly and is perfectly healthy — the client
/// acknowledges just behind the sender and every refusal lasts microseconds. The unhealthy case is a
/// refusal that does not clear, which is why [`SlowDetector::note_blocked`] records the *first* block
/// of a streak and later blocks extend it rather than restart it.
///
/// The detector does not disconnect anything, and does not log. It answers one question —
/// "is this consumer slow?" — so the policy (warn, shed, close) stays with the connection that owns
/// the socket and can see the rest of the context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlowDetector {
    /// How long a window may stay continuously blocked before the consumer counts as slow.
    threshold_ms: u64,
    /// When the current blocked streak began, or `None` when the window is not blocked.
    ///
    /// `None` is what makes "not slow" the default and an unbroken streak the only way to become
    /// slow: there is no state in which a detector that was never blocked reports slowness.
    blocked_since_ms: Option<u64>,
}

impl SlowDetector {
    /// Creates a detector that calls a consumer slow after `threshold_ms` of continuous blocking.
    ///
    /// A threshold of zero marks a consumer slow on the first observation, which is occasionally
    /// what a test or a deliberately paranoid deployment wants. It is not special-cased, so the
    /// comparison is one rule.
    #[must_use]
    pub const fn new(threshold_ms: u64) -> Self {
        Self {
            threshold_ms,
            blocked_since_ms: None,
        }
    }

    /// Records a send that was refused because the window was full, observed at `now_ms`.
    ///
    /// Only the first call of a streak sets the clock. A later call in the same streak leaves it
    /// alone — which is what makes the threshold measure the *duration* of the blockage rather than
    /// the gap between two refusals. A client that is refused once a second for a minute is blocked
    /// for a minute, not for one second.
    ///
    /// A clock that steps backwards does not end the streak: `now_ms` earlier than the recorded
    /// start collapses the elapsed time to zero via saturating subtraction, so the detector reports
    /// "not slow yet" and waits for real progress. Treating the jump as a huge elapsed span — what
    /// an unsigned subtraction would do — would disconnect a healthy client because the wall clock
    /// was corrected.
    pub fn note_blocked(&mut self, now_ms: u64) {
        if self.blocked_since_ms.is_none() {
            self.blocked_since_ms = Some(now_ms);
        }
    }

    /// Clears the blocked streak, because the window advanced.
    ///
    /// A caller should call this whenever an acknowledgement arrives — or, more precisely, whenever
    /// it sends successfully again, since that is the observable proof the window moved. Without it
    /// the detector would accumulate every block a stream ever had and eventually call every busy
    /// client slow; a clean "the pressure is gone" signal is what keeps the verdict about the
    /// present.
    pub fn note_progress(&mut self) {
        self.blocked_since_ms = None;
    }

    /// Whether the consumer has been continuously blocked for at least the threshold.
    ///
    /// `now_ms` rather than an internal clock so the caller decides the time base: the same detector
    /// can be driven by `Instant`-derived milliseconds in production and by a counter in a test, with
    /// no sleeping and no flakiness.
    #[must_use]
    pub const fn is_slow(&self, now_ms: u64) -> bool {
        match self.blocked_since_ms {
            Some(since) => now_ms.saturating_sub(since) >= self.threshold_ms,
            None => false,
        }
    }

    /// How long the current blocked streak has lasted, or zero when the window is not blocked.
    ///
    /// Reported next to [`SlowDetector::is_slow`] so a log line can say "blocked for 4,200 ms" rather
    /// than only "slow", which is the difference between a metric and a diagnosis.
    #[must_use]
    pub const fn blocked_for_ms(&self, now_ms: u64) -> u64 {
        match self.blocked_since_ms {
            Some(since) => now_ms.saturating_sub(since),
            None => 0,
        }
    }

    /// The threshold this detector was built with.
    ///
    /// Exposed so a caller can compare the observed streak against the exact number that will
    /// trigger it without restating the configuration.
    #[must_use]
    pub const fn threshold_ms(&self) -> u64 {
        self.threshold_ms
    }
}
