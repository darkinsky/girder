//! Per-connection admission control for the RPC surface.
//!
//! A connection is a trust boundary, but not a trustworthy one: whatever a client sends arrives at
//! the speed the client chooses, and the server's reader loop consumes it at the speed the work
//! behind it allows. Without a ceiling in between, one client is enough to starve every other one.
//! This module is that ceiling, and it is deliberately *not* shared: a [`Limiter`] belongs to one
//! connection, so a client that exhausts its quota refuses only itself.
//!
//! # Why per connection, and not global
//!
//! A global limiter makes every client's fate depend on every other client's behaviour. One
//! misbehaving socket could then deny service to a healthy one, which turns a quota into an outage
//! amplifier. The failure this module must prevent is "one client starves the rest", and the
//! cheapest way to prevent it is to never let two clients share a counter. A deployment that also
//! wants a global ceiling should put a second limiter in front of the process; the two concerns
//! compose, and mixing them into one counter would make neither work.
//!
//! # The refill algorithm
//!
//! The rate limit is a token bucket: capacity `per_second` tokens, refilled at `per_second` tokens
//! per second, one token per admitted request. The subtle part is the refill, and the trap is to
//! remember "the last time I looked" and add `rate * (now - last)` on every call. That accumulates
//! a little error at every admission — a truncated remainder here, a rounding there — and after ten
//! thousand requests the bucket level is no longer the level the client actually spent. Instead
//! there is one fixed baseline, the `Instant` the limiter was built with, and every call recomputes
//! the bucket from the whole elapsed span:
//!
//! ```text
//! refilled  = rate.micro_tokens_per_second * elapsed_microseconds / 1_000_000
//! granted   = micro-tokens handed out since the baseline, moved up to `refilled` when the bucket
//!             would otherwise have been full (a full bucket cannot bank surplus)
//! level     = capacity - (granted - refilled)
//! ```
//!
//! Because `granted` and `refilled` are both totals measured from the same immutable origin, the
//! only error is the single truncation in `refilled`, and it never compounds: the next call
//! recomputes it from scratch. All of it is integer arithmetic in millionths of a token, so there
//! is no float drift either.
//!
//! # Why `retry_after` is computed, not assumed
//!
//! Telling a client "wait one second" when its next token is 40 ms away wastes 960 ms of a
//! connection's throughput, and telling it "wait" with no number invites a retry storm. The wait is
//! therefore derived from how much of a token the bucket is missing, divided by the refill rate,
//! rounded *up* so the client never comes back slightly too early.
//!
//! # What cannot be checked before the bytes arrive
//!
//! [`Limiter::check_size`] is intended for the header stage: an `HTTP` caller can run it against
//! `Content-Length` before reading the body, and a framed caller can run it against the declared
//! frame size before parsing. Two cases it cannot help with, and pretending otherwise would be
//! worse than saying so:
//!
//! - A chunked or otherwise length-less body has no size to check until it is already read.
//! - A WebSocket message is delivered whole by the transport before this code sees it, so the
//!   check prevents *dispatch*, not the allocation of the message itself. The transport's own
//!   maximum message size (if it has one) is the bound for that.
//!
//! [`Limiter::admit`] runs the same check, but by then the bytes are in hand; that is a second
//! line of defence, not the primary one.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The accounting unit: one admitted request costs exactly one token.
///
/// Millionths are used so that a sub-second refill (a fraction of a token) is still an integer and
/// so that the conversion to microseconds is exact.
const UNIT: u128 = 1_000_000;

/// Microseconds per second, the scale `Instant::as_micros` reports in.
const MICROS_PER_SECOND: u128 = 1_000_000;

/// Distinguishes one limiter's permits from another's.
static NEXT_LIMITER_ID: AtomicU64 = AtomicU64::new(1);

/// The ceiling one connection is allowed to reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    /// How many requests the connection may have in flight at once.
    ///
    /// This is the backpressure that matters: the work behind a request is what competes for the
    /// process, so the bound is on concurrent work, not on queued messages.
    pub in_flight: usize,
    /// How many requests the connection may start per second, and equally how many it may burst.
    ///
    /// A zero rate denies every request; it is not read as "unlimited", because a zero that
    /// silently meant infinity would turn a misconfiguration into an open door.
    pub per_second: u32,
    /// How large a single request may be, in bytes.
    pub max_request_bytes: usize,
}

/// One connection's admission controller.
///
/// The limiter owns its counters and is not shared between connections; see the module docs for
/// why that isolation is the point. It is `&mut self` throughout because a connection's reader
/// loop is the only caller, and a lock around a counter this cheap would cost more than the
/// counter.
#[derive(Debug)]
pub struct Limiter {
    /// The ceiling this limiter enforces, kept so a refusal can quote the number it used.
    quota: Quota,
    /// The fixed baseline every refill is measured from. It is never advanced, which is what keeps
    /// the arithmetic from drifting across calls.
    epoch: Instant,
    /// Bucket capacity in micro-tokens, i.e. `per_second` tokens.
    capacity: u128,
    /// Refill rate in micro-tokens per second, numerically equal to `capacity` here because the
    /// bucket is sized at one second's worth.
    rate: u128,
    /// Micro-tokens handed out since `epoch`, never decreased; the bucket is derived against it.
    granted: u128,
    /// Requests admitted but not yet released.
    in_flight: usize,
    /// See [`NEXT_LIMITER_ID`]; lets [`Limiter::release`] reject a foreign permit.
    id: u64,
}

impl Limiter {
    /// Builds the limiter for one connection.
    ///
    /// `now` is the refill baseline and the injected clock's origin, which is why it is passed in
    /// rather than read here: a test that has to wait for real seconds to observe a refill is a
    /// test that is slow and flaky in equal measure.
    pub fn new(quota: Quota, now: Instant) -> Self {
        let capacity = u128::from(quota.per_second) * UNIT;

        Self {
            quota,
            epoch: now,
            capacity,
            // Capacity and rate are the same number: a bucket that holds one second of traffic
            // also refills at that rate.
            rate: capacity,
            granted: 0,
            in_flight: 0,
            id: NEXT_LIMITER_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Rejects a request that is larger than the quota, without touching any other counter.
    ///
    /// Call this as early as the size is known — against `Content-Length`, or a declared frame
    /// length — so an oversized request is refused before its body is read. See the module docs
    /// for the cases where no early answer exists.
    pub fn check_size(&self, bytes: usize) -> Result<(), Refusal> {
        if bytes > self.quota.max_request_bytes {
            return Err(Refusal::TooLarge {
                limit: self.quota.max_request_bytes,
                got: bytes,
            });
        }
        Ok(())
    }

    /// Tries to admit one request of `bytes` bytes.
    ///
    /// On success the returned [`Permit`] holds one in-flight slot until it is handed back to
    /// [`Limiter::release`]. On failure nothing is consumed: a refused request costs neither a slot
    /// nor a token, so a client that backs off finds the bucket exactly as it left it.
    ///
    /// The checks run cheapest-and-most-definite first: size (a property of the request alone),
    /// then in-flight (a local count with a definite answer), then the bucket (which needs the
    /// clock and yields a time to wait).
    pub fn admit(&mut self, now: Instant, bytes: usize) -> Result<Permit, Refusal> {
        self.check_size(bytes)?;

        if self.in_flight >= self.quota.in_flight {
            return Err(Refusal::TooManyInFlight {
                limit: self.quota.in_flight,
            });
        }

        let level = self.level(now);
        if level < UNIT {
            return Err(Refusal::RateLimited {
                retry_after: self.wait_for_one_token(level),
            });
        }

        self.granted += UNIT;
        self.in_flight += 1;

        Ok(Permit {
            bytes,
            owner: self.id,
        })
    }

    /// Returns an in-flight slot to the pool.
    ///
    /// A permit issued by a *different* limiter is ignored: subtracting it here would free a slot
    /// this connection never used, and a leaked slot is a bug a caller can see (through
    /// [`Limiter::in_flight`]) while a silently inflated quota is not.
    ///
    /// The permit is taken by value and cannot be cloned, so one admission cannot be released
    /// twice. Dropping a permit without releasing it leaks its slot, which is why the release is an
    /// explicit call at the end of the request rather than a `Drop` impl: `Drop` would need the
    /// permit to borrow the limiter, and then a permit could not be held across the `await` that
    /// the request itself is.
    // Taken by value on purpose: a by-value argument can only be used once, so the compiler — not a
    // convention — prevents one admission from releasing two slots. A reference would compile at
    // every double release.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "ownership makes a double release impossible to write"
    )]
    pub fn release(&mut self, permit: Permit) {
        if permit.owner == self.id {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
    }

    /// How many requests are admitted and not yet released.
    ///
    /// Exposed so a caller can assert the invariant it is responsible for — every `admit` pairs
    /// with exactly one `release` — instead of trusting that it holds.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Recomputes the bucket from the fixed baseline and returns its level in micro-tokens.
    ///
    /// `saturating_duration_since` is deliberate: an injected clock that steps backwards yields
    /// zero elapsed time, which grants nothing. Reading it as a huge elapsed span — what an
    /// unsigned subtraction would do — would hand the client a full bucket for free.
    fn level(&mut self, now: Instant) -> u128 {
        let elapsed_micros = now.saturating_duration_since(self.epoch).as_micros();
        let refilled = self.rate.saturating_mul(elapsed_micros) / MICROS_PER_SECOND;

        if self.granted < refilled {
            // The bucket would have overflowed: a token bucket cannot bank more than its capacity,
            // so the surplus is discarded by moving the accounting baseline up to "full". Without
            // this, an idle connection would accumulate a credit it must never be able to spend as
            // one unbounded burst.
            self.granted = refilled;
            return self.capacity;
        }

        // `granted - refilled` is what is still missing after all refills so far; the in-flight
        // invariant keeps it at or below capacity, and the `min` keeps the subtraction total even
        // if a future change breaks that invariant.
        self.capacity
            .saturating_sub((self.granted - refilled).min(self.capacity))
    }

    /// How long until the bucket holds one whole token again.
    ///
    /// `level` is below one token, so the shortfall is `UNIT - level` micro-tokens; dividing it by
    /// the refill rate gives the wait, rounded up by `div_ceil` so the answer is never a microsecond
    /// short of a token. A rate of zero has no such time, and `Duration::MAX` says exactly that:
    /// this quota will never admit another request.
    fn wait_for_one_token(&self, level: u128) -> Duration {
        if self.rate == 0 {
            return Duration::MAX;
        }

        let missing = UNIT - level;
        let micros = (missing * MICROS_PER_SECOND).div_ceil(self.rate);

        // A `u128` of microseconds cannot be represented in `Duration`; clamping to `u64::MAX`
        // leaves a wait of ~584,000 years, which is far past "never" for any caller.
        Duration::from_micros(u64::try_from(micros).unwrap_or(u64::MAX))
    }
}

/// Proof that one request holds an in-flight slot.
///
/// Deliberately not `Clone`: cloning it would allow the same admission to be released twice and
/// inflate the effective quota.
#[derive(Debug)]
pub struct Permit {
    /// The size the request was admitted at, so a caller can log or re-check it without threading
    /// the number through by hand.
    bytes: usize,
    /// The limiter that issued this permit; see [`Limiter::release`].
    owner: u64,
}

impl Permit {
    /// The request size this permit was issued for.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Why a request was not admitted.
///
/// Each variant answers the question a client actually has: the first two quote the limit that was
/// hit, and the third says how long to wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The connection already has as many requests in flight as its quota allows.
    TooManyInFlight {
        /// The in-flight ceiling, so the client can tell "back off" from "this will never fit".
        limit: usize,
    },
    /// The token bucket is empty.
    RateLimited {
        /// How long until one token is available. Derived from the bucket's shortfall, not a fixed
        /// interval; see the module docs.
        retry_after: Duration,
    },
    /// The request is larger than the quota allows, checked before its body is read when the size
    /// is known that early.
    TooLarge {
        /// The largest request the connection may send, in bytes.
        limit: usize,
        /// The size that was refused, in bytes.
        got: usize,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyInFlight { limit } => {
                write!(f, "too many requests in flight (limit {limit})")
            }
            Self::RateLimited { retry_after } => {
                write!(f, "request rate exceeded (retry after {retry_after:?})")
            }
            Self::TooLarge { limit, got } => {
                write!(f, "request of {got} bytes exceeds the {limit}-byte limit")
            }
        }
    }
}

impl std::error::Error for Refusal {}
