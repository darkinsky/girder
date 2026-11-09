//! Retry policy: bounded exponential backoff with jitter.
//!
//! Upstream exposes one resolved policy per provider route, and a separate plugin executes it on the
//! agent's failed-step extension point. This module carries the policy itself — the part whose
//! arithmetic is worth testing — and leaves the wiring to whoever owns the request loop.
//!
//! # Why jitter
//!
//! Without it, N clients that fail together retry together, and the provider sees a thundering herd
//! exactly when it is already struggling. Symmetric jitter spreads them without changing the mean.
//!
//! # Why a delay budget rather than a raw retry count
//!
//! A retry count alone lets a pathological policy wait hours: five retries with a doubling delay
//! starting at a second is already half a minute, and a misconfigured initial delay makes it worse.
//! The delay is therefore clamped per attempt and the total is bounded by the attempt count, which is
//! what [`RetryPolicy::delay_for`] returns.

use std::time::Duration;

/// A failure a retry decision can branch on.
///
/// A stable code rather than a message: a message is for a human, a code is for a policy, and
/// matching on prose is how a retry loop silently stops retrying after a provider rewords an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureCode(pub String);

impl FailureCode {
    /// The provider returned an empty response.
    pub const EMPTY_RESPONSE: &'static str = "EMPTY_RESPONSE";
    /// The provider rate-limited the request.
    pub const RATE_LIMIT: &'static str = "RATE_LIMIT";
    /// The provider reported an internal error.
    pub const SERVER: &'static str = "SERVER";
    /// The request timed out.
    pub const TIMEOUT: &'static str = "TIMEOUT";
    /// The transport failed.
    pub const TRANSPORT: &'static str = "TRANSPORT";
}

/// Exponential backoff parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Backoff {
    /// Delay before the first retry.
    pub initial: Duration,
    /// Ceiling for any single delay.
    pub max: Duration,
    /// Symmetric jitter ratio around one. `0.1` means a delay lands in `[0.9d, 1.1d]`.
    pub jitter_ratio: f64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(10),
            jitter_ratio: 0.1,
        }
    }
}

impl Backoff {
    /// The delay before attempt `attempt`, where `attempt` 1 is the first retry.
    ///
    /// Deterministic in `attempt`: the jitter is applied by [`Backoff::jittered`], which takes the
    /// random draw as an argument. Keeping the draw outside makes the schedule testable and keeps a
    /// hidden `rand` dependency out of a library that does not otherwise need one.
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        // `attempt` is 1-based; `attempt - 1` doublings after the initial delay.
        let factor = 2u32.saturating_pow(attempt.saturating_sub(1).min(16));
        self.initial.saturating_mul(factor).min(self.max)
    }

    /// Applies symmetric jitter to a base delay.
    ///
    /// `draw` is a value in `[0, 1)`, as a uniform random source would produce. The result is
    /// clamped to `[1ms, max]` so a jitter draw can never produce zero (a retry storm) or exceed the
    /// ceiling (a policy that ignores its own limit).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "a delay in milliseconds is far below 2^52, so f64 represents it exactly"
    )]
    pub fn jittered(&self, base: Duration, draw: f64) -> Duration {
        let ratio = self.jitter_ratio.clamp(0.0, 1.0);
        // Map [0,1) to [-1, 1) then scale by the ratio.
        let offset = (draw.clamp(0.0, 1.0) * 2.0 - 1.0) * ratio;
        // `as_millis` is u128; a delay beyond u64 milliseconds is already clamped by `max`, so the
        // narrowing is safe after the clamp rather than before it.
        let base_millis = u64::try_from(base.as_millis()).unwrap_or(u64::MAX) as f64;
        let ceiling = u64::try_from(self.max.as_millis()).unwrap_or(u64::MAX) as f64;
        let millis = (base_millis * (1.0 + offset)).clamp(1.0, ceiling);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the value is clamped to [1, ceiling] and ceiling came from a u64"
        )]
        let millis = millis as u64;
        Duration::from_millis(millis)
    }
}

/// When to retry.
#[derive(Clone, Debug, PartialEq)]
pub enum RetryMode {
    /// Retry only the listed failure codes.
    Normal {
        /// Codes eligible for a retry.
        retryable: Vec<String>,
    },
    /// Retry every failure until success or cancellation.
    ///
    /// Used when the caller would rather wait than surface a failure — an interactive session where
    /// the alternative is ending the turn.
    Always,
}

/// A resolved retry policy.
#[derive(Clone, Debug, PartialEq)]
pub struct RetryPolicy {
    /// When to retry.
    pub mode: RetryMode,
    /// Maximum retries after the first attempt. Ignored when `mode` is `Always`.
    pub max_retries: u32,
    /// Backoff parameters.
    pub backoff: Backoff,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            mode: RetryMode::Normal {
                retryable: vec![
                    FailureCode::EMPTY_RESPONSE.to_owned(),
                    FailureCode::RATE_LIMIT.to_owned(),
                    FailureCode::SERVER.to_owned(),
                    FailureCode::TIMEOUT.to_owned(),
                    FailureCode::TRANSPORT.to_owned(),
                ],
            },
            max_retries: 5,
            backoff: Backoff::default(),
        }
    }
}

impl RetryPolicy {
    /// Whether `code` is eligible for a retry.
    #[must_use]
    pub fn is_retryable(&self, code: &str) -> bool {
        match &self.mode {
            // `Always` retries everything, including a code it has never seen.
            RetryMode::Always => true,
            RetryMode::Normal { retryable } => retryable.iter().any(|known| known == code),
        }
    }

    /// Whether an attempt may be retried, given how many have already failed.
    ///
    /// `failed` counts retries already made, so the first retry passes `failed = 0`.
    #[must_use]
    pub fn should_retry(&self, code: &str, failed: u32) -> bool {
        if !self.is_retryable(code) {
            return false;
        }
        match self.mode {
            RetryMode::Always => true,
            RetryMode::Normal { .. } => failed < self.max_retries,
        }
    }

    /// The base delay before retry number `attempt` (1-based).
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        self.backoff.delay_for(attempt)
    }

    /// The delay to actually wait, given a random draw in `[0, 1)`.
    #[must_use]
    pub fn wait_for(&self, attempt: u32, draw: f64) -> Duration {
        self.backoff.jittered(self.delay_for(attempt), draw)
    }

    /// The longest a full retry sequence can take, for diagnosing a policy before it runs.
    ///
    /// A caller that cannot tolerate this should lower `max_retries` or the initial delay, and
    /// knowing the number is what makes that decision possible.
    #[must_use]
    pub fn worst_case_total(&self) -> Duration {
        match self.mode {
            // `Always` has no bound; report the ceiling of one delay as the honest answer for "how
            // long a single wait can be", and let the caller read the mode.
            RetryMode::Always => self.backoff.max,
            RetryMode::Normal { .. } => (1..=self.max_retries)
                .map(|attempt| self.delay_for(attempt))
                .fold(Duration::ZERO, Duration::saturating_add),
        }
    }
}

/// A failure the retry loop reports when it gives up.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    /// The failure was not eligible for a retry.
    #[error("`{code}` is not retryable")]
    NotRetryable {
        /// The failure code.
        code: String,
    },

    /// Every allowed retry was used.
    #[error("gave up after {attempts} attempt(s); last failure `{code}`")]
    Exhausted {
        /// How many attempts were made, including the first.
        attempts: u32,
        /// The last failure code.
        code: String,
    },
}

/// Runs `operation` under `policy`, retrying eligible failures.
///
/// `draw` supplies the jitter randomness so the loop is deterministic in a test.
///
/// # Errors
///
/// Returns [`RetryError::NotRetryable`] for a failure the policy excludes, and
/// [`RetryError::Exhausted`] when the retries run out.
pub async fn run_with_retry<F, T, E, D>(
    policy: &RetryPolicy,
    mut operation: F,
    code_of: impl Fn(&E) -> String,
    mut draw: D,
) -> Result<T, RetryError>
where
    F: FnMut() -> crate::scope::BoxFut<Result<T, E>>,
    D: FnMut() -> f64,
{
    let mut failed = 0u32;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                let code = code_of(&error);
                if !policy.should_retry(&code, failed) {
                    return Err(if policy.is_retryable(&code) {
                        RetryError::Exhausted {
                            attempts: failed + 1,
                            code,
                        }
                    } else {
                        RetryError::NotRetryable { code }
                    });
                }
                failed += 1;
                let wait = policy.wait_for(failed, draw());
                tokio::time::sleep(wait).await;
            }
        }
    }
}
