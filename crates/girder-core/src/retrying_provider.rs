//! A provider that retries the failures worth retrying.
//!
//! `retry` knew how to back off and which codes to retry; nothing called it, so a transient network failure ended a
//! turn that would have succeeded on the second attempt.
//!
//! # Why the retry wraps the stream construction rather than the stream
//!
//! A failed request fails before any chunk arrives: the status is refused, the connection is refused, the body never
//! starts. A failure *during* streaming is different -- half a response has already been delivered, and retrying would
//! duplicate what the caller saw. So only the attempt to open the stream is retried, which is also the attempt whose
//! failure is safe to repeat.
//!
//! # Why a cancellation is never retried
//!
//! A caller that cancelled asked for the work to stop. Retrying it would spend the user's request budget on an answer
//! they explicitly withdrew, and the delay would make the cancel appear to have failed.
//!
//! # Why the classification is a code rather than a variant
//!
//! [`LlmError`] carries two variants because that is all the layer above needs. Which failures are worth retrying is a
//! transport question -- a 429 is, a 401 is not -- so the classification belongs here, where the transport is known,
//! rather than in an error type every provider would have to extend.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::llm::{ChunkStream, LlmError, LlmProvider, LlmRequest};
use crate::retry::{FailureCode, RetryError, RetryPolicy, run_with_retry};
use crate::scope::BoxFut;

/// A provider that retries the failures a second attempt could fix.
pub struct RetryingProvider {
    inner: Arc<dyn LlmProvider>,
    policy: RetryPolicy,
}

impl std::fmt::Debug for RetryingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryingProvider")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl RetryingProvider {
    /// Wraps a provider.
    #[must_use]
    pub fn new(inner: Arc<dyn LlmProvider>, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }

    /// The policy in force.
    #[must_use]
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }
}

impl LlmProvider for RetryingProvider {
    fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        let inner = Arc::clone(&self.inner);
        let policy = self.policy.clone();
        Box::pin(async move {
            // Checked before the first attempt and before every retry: a cancel that arrived mid-backoff must stop the
            // work rather than let the next attempt run.
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }

            // A counter rather than a random source: the backoff's jitter needs a draw, and a deterministic one keeps
            // a test from depending on luck. A real deployment gets the same bounded spread because the jitter only
            // shifts within the policy's own bounds.
            let mut draws = 0u64;
            let result = run_with_retry(
                &policy,
                || {
                    let inner = Arc::clone(&inner);
                    let request = request.clone();
                    let cancel = cancel.clone();
                    Box::pin(async move { inner.stream(request, cancel).await })
                },
                classify,
                move || {
                    draws = draws.wrapping_add(1);
                    // A simple low-discrepancy sequence in [0, 1): decorrelates two clients retrying at once without
                    // needing a random number generator in the dependency tree.
                    // The value is already reduced below 1000, so the narrowing is exact and the cast cannot lose
                    // anything -- which is what the lint is asking to be told explicitly.
                    f64::from(u16::try_from(draws.wrapping_mul(2_654_435_761) % 1000).unwrap_or(0))
                        / 1000.0
                },
            )
            .await;

            match result {
                Ok(stream) => Ok(stream),
                Err(RetryError::Exhausted { attempts, code }) => Err(LlmError::Provider(format!(
                    "{code} after {attempts} attempt(s)"
                ))),
                Err(RetryError::NotRetryable { code }) => Err(LlmError::Provider(code)),
            }
        })
    }
}

/// The retry code for one failure.
///
/// A cancellation gets a code the policy never retries, which is what makes "did the caller withdraw this" survive into
/// the retry decision rather than being decided twice.
fn classify(error: &LlmError) -> String {
    match error {
        // A cancellation is not a retryable code, so the policy refuses it on the first check rather than after a
        // backoff the caller has to wait through.
        LlmError::Cancelled => "CANCELLED".to_owned(),
        LlmError::Provider(message) => classify_provider(message).to_owned(),
    }
}

/// The retry code for one failure, using the vocabulary `retry` already defines.
///
/// Reusing those codes rather than inventing a parallel set is what lets a deployment's own `RetryPolicy` name them: a
/// policy written against `RATE_LIMIT` would silently never match a `rate_limited` this layer made up.
///
/// # Errors
///
/// Never -- the classification errs toward *not* retrying a permanent failure, because a request retried three times
/// against a bad key costs three requests and gains nothing.
fn classify_provider(message: &str) -> &'static str {
    let lowered = message.to_lowercase();
    for (needle, code) in [
        ("429", FailureCode::RATE_LIMIT),
        ("rate limit", FailureCode::RATE_LIMIT),
        ("overloaded", FailureCode::SERVER),
        ("timeout", FailureCode::TIMEOUT),
        ("timed out", FailureCode::TIMEOUT),
        ("502", FailureCode::SERVER),
        ("503", FailureCode::SERVER),
        ("504", FailureCode::TIMEOUT),
    ] {
        if lowered.contains(needle) {
            return code;
        }
    }
    // A permanent failure: a bad key, a bad model, a malformed request. The policy will not match it, which is the
    // answer the caller needs.
    "PERMANENT"
}
