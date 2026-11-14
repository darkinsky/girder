//! Live liveness detection for a streamed model response.
//!
//! A streaming response has two independent ways of being late, and they are different incidents
//! with different answers:
//!
//! - **Nothing has arrived yet.** The request was accepted but the provider has not sent its first
//!   chunk: the prompt is queued behind other traffic, the model is cold, the scheduler is holding
//!   it. None of that is a fault, and a bound tuned for transport silence would kill a request that
//!   is about to answer. The honest report is "still waiting, and here is how long".
//! - **Something arrived and then the stream went quiet.** Once a provider is emitting, the
//!   connection has proved alive, the prompt has been processed, and silence is no longer queueing:
//!   it is a dead socket, a wedged proxy, or a provider that forgot to close its stream. This is
//!   the case worth bounding tightly, and its bound has nothing to do with the first one.
//!
//! Collapsing those two into a single timeout is the mistake this module exists to prevent: set it
//! generously and a dead connection holds a turn open for minutes; set it tightly and a normal
//! queue delay kills a request that was working. [`Policy`] carries both numbers, and
//! [`Health::WaitingForFirst`] versus [`Health::Stalled`] keeps them distinguishable at the point
//! of decision rather than in a log line written after the fact.
//!
//! # Why this is separate from the post-mortem statistics
//!
//! [`crate::llm_stats`] answers "what shape did that finished stream have?" — it is computed from
//! retained chunks after the stream is over, which is the right shape for a transcript or a cost
//! report. It cannot help a turn that is still hanging, because there is nothing to compute until
//! the stream ends. This module is the live side: it is fed as chunks arrive and is asked for a
//! verdict while the answer can still change the outcome.
//!
//! # Why the clock is injected
//!
//! Every method takes `now_ms` rather than calling `Instant::now()`. Tests advance time by passing
//! larger numbers instead of sleeping, so a thirty-second bound is exercised in microseconds, and
//! the production clock stays a single choice made at the call site instead of one hidden here.
//!
//! Timestamps are assumed to be milliseconds on a non-decreasing clock anchored at the moment the
//! request was sent, the same convention as [`crate::llm_stats`]. A caller that prefers an absolute
//! clock calls [`Detector::start`] with the request's send time; without it the detector treats
//! timestamp `0` as the start, which is exactly right for the relative convention.
//!
//! # Why a detector alone cannot call a stall
//!
//! [`Detector::check`] never reports [`Health::Stalled`] before the first chunk. From its own
//! numbers alone, a long first wait is indistinguishable from a connection that died before
//! answering — and the detector has no business guessing which. Judging that needs the first-chunk
//! budget, which is the caller's policy, so the bare detector reports the wait and
//! [`Policy::check`] makes the call.

/// What a liveness check found.
///
/// The variants are deliberately not a boolean. "Waiting" and "stalled" lead to opposite actions —
/// keep waiting, or tear the request down and retry — so a caller that could only ask "is it fine?"
/// would have to re-derive the distinction from timestamps it does not keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// A chunk arrived recently enough that the stream is alive.
    Flowing,
    /// No chunk has arrived yet, `elapsed_ms` after the start.
    ///
    /// This is not a failure. It is the report a caller needs to log a request that is queued, and
    /// the number is the real queueing time observed so far.
    WaitingForFirst {
        /// Milliseconds since the request started, not since the last event.
        elapsed_ms: u64,
    },
    /// The stream produced at least one chunk and then went silent for `idle_ms`.
    ///
    /// The field is the *observed* silence, not the threshold that tripped. A caller already knows
    /// its own threshold; what it cannot reconstruct later is how long the stream was actually
    /// quiet, which is the number worth putting in a log or an error message.
    Stalled {
        /// Milliseconds since the last chunk arrived.
        idle_ms: u64,
    },
}

/// Tracks the arrival times of a stream's chunks and reports whether it is still alive.
///
/// The detector owns the clock reading and the counters; the bounds live either here (a single
/// idle bound, for the simple case) or in [`Policy`] (two bounds, when first-chunk latency and
/// transport silence must be judged differently).
///
/// It holds no buffer: chunks are counted and timestamped, not retained, so a detector costs the
/// same whether a response is one token or a million.
#[derive(Clone, Debug)]
pub struct Detector {
    /// Allowed silence between two chunks, used by [`Detector::check`].
    idle_ms: u64,
    /// When the request started, when the caller supplied it.
    start_ms: Option<u64>,
    /// When the first chunk arrived. `None` means the stream has produced nothing yet.
    first_chunk_ms: Option<u64>,
    /// When the most recent chunk arrived. `None` means no chunk has arrived.
    last_chunk_ms: Option<u64>,
    /// How many chunks have arrived, including empty deltas and protocol markers.
    chunks: u64,
}

impl Detector {
    /// Creates a detector that allows `idle_ms` of silence between chunks.
    ///
    /// `idle_ms` applies only to [`Detector::check`]; a detector driven through [`Policy::check`]
    /// takes its bounds from the policy. A `0` bound is allowed and means "every check between
    /// chunks trips", which is occasionally useful in tests and never in production.
    #[must_use]
    pub const fn new(idle_ms: u64) -> Self {
        Self {
            idle_ms,
            start_ms: None,
            first_chunk_ms: None,
            last_chunk_ms: None,
            chunks: 0,
        }
    }

    /// Records the moment the request was sent, on the caller's clock.
    ///
    /// Optional: with the relative convention (timestamps counted from the request) the origin is
    /// already `0`. It is required when `now_ms` is an absolute reading, because otherwise the
    /// first wait would be measured from 1970 — or from boot, depending on the clock — and every
    /// request would look stalled the instant it was sent.
    pub fn start(&mut self, now_ms: u64) {
        self.start_ms = Some(now_ms);
    }

    /// Records that a chunk arrived at `now_ms`.
    ///
    /// Every chunk counts, including empty text deltas and protocol markers such as `block-start`
    /// and `usage`: any of them proves the transport is still delivering, which is the question
    /// this detector asks. Filtering for "useful" chunks would report a stalled stream while bytes
    /// were plainly still arriving.
    ///
    /// A timestamp older than the previous chunk is clamped rather than stored. A clock that steps
    /// backwards (an NTP correction, a test that rewinds) must not be able to manufacture silence;
    /// the chunk is still counted, because it did arrive.
    pub fn note_chunk(&mut self, now_ms: u64) {
        if self.first_chunk_ms.is_none() {
            self.first_chunk_ms = Some(now_ms);
        }
        self.last_chunk_ms = Some(match self.last_chunk_ms {
            Some(last) => last.max(now_ms),
            None => now_ms,
        });
        self.chunks = self.chunks.saturating_add(1);
    }

    /// Checks the stream's health at `now_ms`.
    ///
    /// Before the first chunk this always reports [`Health::WaitingForFirst`], however long the
    /// wait. That is deliberate: a bare idle bound cannot tell a busy queue from a dead socket, and
    /// reporting the former as a stall would abort requests that are merely slow to start. Use
    /// [`Policy::check`] to apply a first-chunk budget.
    ///
    /// The bound is inclusive: a silence exactly equal to `idle_ms` is already a stall, because the
    /// caller asked for at most that much silence and has now had all of it.
    #[must_use]
    pub fn check(&self, now_ms: u64) -> Health {
        let Some(last_chunk_ms) = self.last_chunk_ms else {
            return Health::WaitingForFirst {
                elapsed_ms: now_ms.saturating_sub(self.origin_ms()),
            };
        };
        let idle_ms = now_ms.saturating_sub(last_chunk_ms);
        if idle_ms >= self.idle_ms {
            Health::Stalled { idle_ms }
        } else {
            Health::Flowing
        }
    }

    /// Time to the first chunk, in milliseconds since the request started.
    ///
    /// Before the first chunk arrives this is the wait *so far*, which is what a heartbeat log
    /// wants to print for a request still queued. Once the chunk arrives the value freezes, so
    /// asking again later cannot make a fast response look slow.
    ///
    /// `None` when the detector has neither been started nor seen a chunk: with no origin, the only
    /// number available is the caller's own `now_ms`, and returning that as a duration would report
    /// a clock reading as a measurement.
    ///
    /// The subtraction saturates, so a chunk timestamped before the start (a clock step backwards)
    /// reports `0` rather than wrapping to a value near `u64::MAX`.
    #[must_use]
    pub fn ttft_ms(&self, now_ms: u64) -> Option<u64> {
        if self.start_ms.is_none() && self.chunks == 0 {
            return None;
        }
        // A first chunk freezes the measurement; without one, the wait so far is the best answer.
        let end_ms = self.first_chunk_ms.unwrap_or(now_ms);
        Some(end_ms.saturating_sub(self.origin_ms()))
    }

    /// How many chunks have arrived.
    ///
    /// Useful next to [`Detector::ttft_ms`] in a diagnosis: a response with a large TTFT and a
    /// healthy chunk count was queued, while one with a small TTFT and no growth stalled after
    /// starting.
    #[must_use]
    pub const fn chunks(&self) -> u64 {
        self.chunks
    }

    /// The silence allowance this detector was built with.
    #[must_use]
    pub const fn idle_ms(&self) -> u64 {
        self.idle_ms
    }

    /// The origin of the timeline: the recorded start, or `0` for the relative convention.
    ///
    /// Private because it is an implementation detail of two public numbers (the waiting elapsed
    /// time and TTFT) that must agree with each other; exposing it would invite callers to
    /// recompute one of them slightly differently.
    const fn origin_ms(&self) -> u64 {
        match self.start_ms {
            Some(start_ms) => start_ms,
            None => 0,
        }
    }
}

/// The two timeouts a streamed response needs, and the policy built from them.
///
/// `first_chunk_ms` and `idle_ms` are separate fields rather than one number with a multiplier
/// because they bound different failures. A queue delay is expected, unbounded by anything the
/// client controls, and harmless to wait out; a silence after the stream started is a broken
/// connection, and every second spent waiting on it is a second the user is staring at nothing.
/// A single number would have to be one of the two, and both choices are wrong.
///
/// The typical shape is `first_chunk_ms` several times `idle_ms` — for example two minutes to
/// start, thirty seconds of silence once started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The longest acceptable wait for the first chunk, including queueing.
    pub first_chunk_ms: u64,
    /// The longest acceptable silence between two chunks, once the stream has started.
    pub idle_ms: u64,
}

impl Policy {
    /// Builds a policy from its two bounds.
    #[must_use]
    pub const fn new(first_chunk_ms: u64, idle_ms: u64) -> Self {
        Self {
            first_chunk_ms,
            idle_ms,
        }
    }

    /// Checks `detector` at `now_ms` against both bounds.
    ///
    /// The bounds here win over [`Detector::idle_ms`]: a policy is the production configuration,
    /// and a detector's own bound exists for callers that never build one. Mixing the two would
    /// make the effective timeout depend on construction order, which is not something a caller
    /// should have to reason about.
    ///
    /// Both comparisons are inclusive, matching [`Detector::check`]: a wait equal to its budget has
    /// used the whole budget up.
    ///
    /// A detector that has produced nothing is judged against `first_chunk_ms` and reported as
    /// [`Health::Stalled`] once that budget is spent — the point of the split is that this moment is
    /// much later than the one at which an established stream is declared dead.
    #[must_use]
    pub fn check(&self, detector: &Detector, now_ms: u64) -> Health {
        match detector.first_chunk_ms {
            None => {
                let elapsed_ms = now_ms.saturating_sub(detector.origin_ms());
                if elapsed_ms >= self.first_chunk_ms {
                    // Reported as a stall, not a wait: the budget that allows for queueing is
                    // spent, so this is no longer explainable as a slow start.
                    Health::Stalled {
                        idle_ms: elapsed_ms,
                    }
                } else {
                    Health::WaitingForFirst { elapsed_ms }
                }
            }
            Some(_) => {
                // The idle bound is measured from the last chunk. Using the first chunk's timestamp
                // here would call a stream stalled one idle period after it started, even while it
                // was streaming steadily.
                let idle_ms = now_ms.saturating_sub(detector.last_chunk_ms.unwrap_or(now_ms));
                if idle_ms >= self.idle_ms {
                    Health::Stalled { idle_ms }
                } else {
                    Health::Flowing
                }
            }
        }
    }
}
