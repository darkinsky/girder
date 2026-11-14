//! Backoff arithmetic and the decision that gates it.
//!
//! `retry` owns the policy a deployment resolves, and `retrying_provider` executes it. Neither
//! carried the part a caller reasons about beforehand -- how long retry *n* waits, whether a 429
//! and a dropped connection deserve the same wait, and whether the server's own `Retry-After`
//! outranks local arithmetic. This module is that part, and nothing calls it until it is wired in.
//!
//! # Why rate limiting and a network timeout are not the same wait
//!
//! They are different failures with different recovery timescales. A dropped connection is a blip:
//! the next attempt opens a new connection and usually succeeds, so a short wait is right and a
//! long one wastes the user's time. A 429 is the server saying "I am saturated": it recovers on
//! *its* schedule, and a client that hammers a rate limit with quarter-second retries is how a
//! client gets throttled harder. So the wait is not one universal formula -- the caller picks a
//! [`Strategy`], and [`Strategy::rate_limited`] and [`Strategy::transient`] exist because those two
//! causes must not share one set of numbers.
//!
//! # Why the ceiling is not optional
//!
//! An uncapped `base * multiplier^attempt` is a footgun rather than a policy: with a 500ms base and
//! a multiplier of 2, attempt 10 is over four minutes and attempt 20 is three days. The cap is the
//! promise that one wait stays inside a budget a caller can tolerate, which is also why the
//! exponent itself is clamped: computing `2^4_000_000_000` would hang before the min could rescue
//! it.
//!
//! # Why the random draw is injected
//!
//! Jitter without injection is untestable: an assertion on a delay that came from an RNG can only
//! be a range check, which passes just as happily when the distribution is wrong. Taking the draw
//! as an argument makes the schedule a pure function, keeps `rand` out of the dependency tree, and
//! lets a caller supply its own sequence.

/// Ceiling on the exponent applied to the multiplier.
///
/// 63 doublings already exceed `u64::MAX` milliseconds for any base above zero, so a higher
/// exponent can only change how long the computation takes, not its result.
const MAX_EXPONENT: u32 = 63;

/// A backoff strategy.
///
/// The fields are public because a deployment tunes them per provider, and `Copy` because a
/// strategy is a handful of scalars -- a caller that needs one per request should not have to
/// clone an allocation to get it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Strategy {
    /// How long the first retry waits.
    pub base_ms: u64,
    /// Growth factor (1 = fixed interval, 2 = doubling).
    pub multiplier: u32,
    /// Ceiling for any single wait.
    pub max_ms: u64,
    /// Random spread as a fraction of the computed delay, `0.0..=1.0`.
    ///
    /// `0.1` means a wait lands in `[0.9d, 1.1d]`. `0.0` disables jitter, which is what a test
    /// wants and what a production caller does not: without it, every client that failed together
    /// retries together, and the herd arrives exactly when the server is least able to take it.
    pub jitter: f64,
}

impl Default for Strategy {
    /// The same shape as `retry::Backoff::default`, so a caller migrating between the two does not
    /// silently change its timing.
    fn default() -> Self {
        Self {
            base_ms: 500,
            multiplier: 2,
            max_ms: 10_000,
            jitter: 0.1,
        }
    }
}

impl Strategy {
    /// The strategy for a failure that has no HTTP status: a refused or dropped connection.
    ///
    /// Short and heavily jittered. The failure is usually over by the time the wait ends, so a
    /// quarter second is enough, and spreading the herd matters more than the exact delay because
    /// every affected client is ready to retry almost immediately.
    #[must_use]
    pub const fn transient() -> Self {
        Self {
            base_ms: 250,
            multiplier: 2,
            max_ms: 10_000,
            jitter: 0.3,
        }
    }

    /// The strategy for a 429.
    ///
    /// Far slower than [`Strategy::transient`], because capacity does not come back in 250ms. When
    /// the response carries `Retry-After`, that number should be preferred to this arithmetic
    /// entirely -- see [`Strategy::wait_ms`].
    #[must_use]
    pub const fn rate_limited() -> Self {
        Self {
            base_ms: 2_000,
            multiplier: 2,
            max_ms: 60_000,
            jitter: 0.1,
        }
    }

    /// How long the `attempt`-th retry waits, where `attempt` starts at 1.
    ///
    /// `random` is the injected draw in `0.0..=1.0` used for the jitter. A `NaN` or infinite draw
    /// is treated as `0.5` (no displacement) rather than propagated: a broken RNG should cost a
    /// worse spread, not a `NaN` delay or a panic in the retry loop.
    ///
    /// `attempt == 0` is treated as the first retry. The alternative -- underflowing to a huge
    /// exponent -- would turn the caller's off-by-one into a wait clamped to `max_ms`, and the
    /// zero-wait reading would turn it into a retry storm.
    ///
    /// The result is clamped to `1..=max(max_ms, 1)`: never zero, because a zero wait is a busy
    /// loop against a server that just asked for relief, and never above the ceiling, because a
    /// strategy that ignores its own limit is how a five-second budget becomes an hour.
    #[must_use]
    pub fn delay_ms(&self, attempt: u32, random: f64) -> u64 {
        let ceiling = self.max_ms.max(1);
        // 1-based, so the first retry applies `multiplier^0` and waits exactly `base_ms`.
        let exponent = attempt.saturating_sub(1).min(MAX_EXPONENT);
        // A zero multiplier would collapse every wait after the first to nothing; reading it as 1
        // (a fixed interval) is the safe interpretation of an input that has no useful meaning.
        let factor = self.multiplier.max(1).saturating_pow(exponent);
        // Saturating first and capping second: `base * factor` overflows long before the cap is
        // interesting, and a panic or a wrap here would be a policy bug at the worst possible time.
        let base = self.base_ms.saturating_mul(u64::from(factor)).min(ceiling);
        spread(base, ceiling, self.jitter, random)
    }

    /// The wait before retry `attempt`, preferring the server's hint when there is one.
    ///
    /// The hint does not get jittered and does not get compared against the arithmetic: the server
    /// is the only party that knows when it will accept the request, and undercutting its answer
    /// earns another rejection. The strategy's ceiling still applies, because a hint is an
    /// untrusted number from the network -- `Retry-After: 86400` from a misconfigured proxy must
    /// not park a turn for a day. A caller that genuinely wants to honour a long hint raises
    /// `max_ms`.
    #[must_use]
    pub fn wait_ms(&self, attempt: u32, random: f64, retry_after_ms: Option<u64>) -> u64 {
        match retry_after_ms {
            Some(hint) => hint.clamp(1, self.max_ms.max(1)),
            None => self.delay_ms(attempt, random),
        }
    }
}

/// Applies symmetric jitter to `base`, then clamps to `[1, ceiling]`.
///
/// Symmetric rather than upward-only because it leaves the mean wait unchanged: an upward-only
/// spread lengthens every retry sequence by `jitter/2` for no extra herd protection.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "delays are clamped to max_ms and converted only for one multiply, add and round; \
              an f64 mantissa covers 52 bits, i.e. delays far past any budget a caller would set, \
              and the final comparisons guarantee the narrowed value is within [1, ceiling]"
)]
fn spread(base: u64, ceiling: u64, jitter: f64, random: f64) -> u64 {
    // `clamp` propagates NaN, so non-finite inputs are handled before it rather than after.
    let ratio = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let draw = if random.is_finite() {
        random.clamp(0.0, 1.0)
    } else {
        0.5
    };
    // Maps the draw onto `[1 - ratio, 1 + ratio]`.
    let offset = (draw * 2.0 - 1.0) * ratio;
    let scaled = (base as f64 * (1.0 + offset)).round();
    let scaled = if scaled.is_finite() {
        scaled
    } else {
        base as f64
    };
    if scaled <= 1.0 {
        1
    } else if scaled >= ceiling as f64 {
        ceiling
    } else {
        scaled as u64
    }
}

/// Whether a failure is worth another attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retryable {
    /// Worth retrying.
    Yes {
        /// Why, in terms a log line or an operator can use.
        reason: String,
    },
    /// Not worth retrying.
    No {
        /// Why not, so a caller does not re-derive the decision from the status code.
        reason: String,
    },
}

impl Retryable {
    /// Whether this decision permits another attempt.
    #[must_use]
    pub const fn is_yes(&self) -> bool {
        matches!(self, Self::Yes { .. })
    }

    /// The explanation, whichever way the decision went.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Yes { reason } | Self::No { reason } => reason,
        }
    }
}

/// Statuses whose meaning is "ask again later", with the reason each one carries.
///
/// An explicit table rather than `500..600` or `!400..500`: 501 means the server will never
/// implement the endpoint, and retrying it spends the caller's budget on an answer that is already
/// final.
const TRANSIENT: &[(u16, &str)] = &[
    (
        408,
        "the server timed out waiting for the request, so re-sending the same request can succeed",
    ),
    (
        425,
        "the server refused an early replay; waiting and retrying is the intended response",
    ),
    (
        429,
        "rate limited; capacity recovers on the server's timescale, not the client's",
    ),
    (
        500,
        "the server failed unexpectedly; a repeat can land on a healthy instance",
    ),
    (
        502,
        "a gateway got an unusable response from upstream; the upstream may recover",
    ),
    (
        503,
        "the server is temporarily unable to handle the request, which is what the status means",
    ),
    (
        504,
        "a gateway timed out waiting for upstream; the upstream may answer on a second try",
    ),
];

/// Permanent failures worth naming, with the reason each one carries.
///
/// A 401 retried a hundred times is still a 401: the credentials do not change between attempts.
const PERMANENT: &[(u16, &str)] = &[
    (
        400,
        "the request itself is malformed, so a repeat sends the same malformed request",
    ),
    (
        401,
        "the credentials were rejected, and a repeat presents the same credentials",
    ),
    (
        403,
        "the caller is not allowed, and permission does not appear on a second attempt",
    ),
    (
        404,
        "the endpoint or resource does not exist, and it does not appear on a second attempt",
    ),
    (
        405,
        "the method is not allowed for this resource, which is a property of the route",
    ),
    (
        409,
        "the request conflicts with the resource's state, and the same conflict will recur",
    ),
    (410, "the resource is gone for good"),
    (
        413,
        "the request is too large, and a repeat is the same size",
    ),
    (
        422,
        "the request was understood and rejected on its merits, so a transport retry cannot help",
    ),
];

/// Whether a failure is worth retrying.
///
/// `status` is the HTTP status when a response arrived; `None` means the request never got one (a
/// refused, reset or timed-out connection), which is the most retryable failure there is because
/// nothing about the request was judged. `retry_after_ms` is the server's suggested wait, when the
/// response carried one.
///
/// The status decides *whether*; the hint only ever affects *how long*. A `Retry-After` on a 401
/// is a delay, not an argument that repeating a request will change its outcome, so it cannot turn
/// a permanent failure into a retryable one.
#[must_use]
pub fn should_retry(status: Option<u16>, retry_after_ms: Option<u64>) -> Retryable {
    let Some(status) = status else {
        return Retryable::Yes {
            reason: format!(
                "no HTTP response arrived (refused, reset or timed out), and a fresh connection \
                 can succeed{}",
                hint_clause(retry_after_ms)
            ),
        };
    };

    if let Some((_, why)) = TRANSIENT.iter().find(|(code, _)| *code == status) {
        return Retryable::Yes {
            reason: format!("{status}: {why}{}", hint_clause(retry_after_ms)),
        };
    }

    let why = PERMANENT
        .iter()
        .find(|(code, _)| *code == status)
        .map_or_else(
            || {
                format!(
                    "{status} is not a transient failure; a retry repeats the same request and \
                     gets the same answer"
                )
            },
            |(_, why)| format!("{status}: {why}"),
        );

    Retryable::No {
        reason: match retry_after_ms {
            // Said explicitly because the hint is the one field that *looks* like permission to
            // retry, and a caller reading only the number would conclude it was.
            Some(hint) => format!(
                "{why}; the server's {hint}ms hint is a delay, not a reason to repeat a request \
                 that will fail the same way"
            ),
            None => why,
        },
    }
}

/// The clause appended when the server suggested a wait.
fn hint_clause(retry_after_ms: Option<u64>) -> String {
    retry_after_ms.map_or_else(String::new, |hint| {
        format!("; the server asked for {hint}ms, which overrides the computed backoff")
    })
}
