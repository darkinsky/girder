//! Tests for the provider that retries.
//!
//! The two properties that matter: a transient failure is retried so a turn that could succeed does, and a cancellation
//! is never retried because the caller asked for the work to stop.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use girder_core::{
    ChunkStream, FinishReason, LlmError, LlmProvider, LlmRequest, RetryPolicy, RetryingProvider,
    StreamChunk,
};
use tokio_util::sync::CancellationToken;

/// A provider that fails a set number of times, then answers.
struct Flaky {
    failures: usize,
    attempts: Arc<AtomicUsize>,
    code: String,
    /// Whether to answer without an error once the failures are exhausted.
    succeed: bool,
}

impl Flaky {
    fn new(failures: usize, code: &str) -> (Self, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        (
            Self {
                failures,
                attempts: Arc::clone(&attempts),
                code: code.to_owned(),
                succeed: true,
            },
            attempts,
        )
    }

    fn always_failing(code: &str) -> (Self, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        (
            Self {
                failures: usize::MAX,
                attempts: Arc::clone(&attempts),
                code: code.to_owned(),
                succeed: false,
            },
            attempts,
        )
    }
}

impl LlmProvider for Flaky {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        let failing = attempt < self.failures;
        let code = self.code.clone();
        let succeed = self.succeed;
        Box::pin(async move {
            if failing || !succeed {
                return Err(LlmError::Provider(code));
            }
            let chunks = vec![
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: "text".to_owned(),
                },
                StreamChunk::TextDelta {
                    index: 0,
                    text: "ok".to_owned(),
                },
                StreamChunk::Finish {
                    reason: FinishReason::Stop,
                },
            ];
            Ok(Box::pin(futures::stream::iter(chunks)) as ChunkStream)
        })
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "test".to_owned(),
        messages: Vec::new(),
        tools: Vec::new(),
        system_prompt: None,
        max_tokens: None,
    }
}

/// A policy that retries three times with a negligible backoff, so a test does not sleep.
fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        backoff: girder_core::Backoff {
            initial: std::time::Duration::from_millis(1),
            max: std::time::Duration::from_millis(2),
            jitter_ratio: 0.0,
        },
        max_retries: 3,
        ..RetryPolicy::default()
    }
}

#[tokio::test]
async fn a_transient_failure_is_retried_and_the_second_attempt_succeeds() {
    // The whole point: a turn that could succeed does.
    let (flaky, attempts) = Flaky::new(1, "rate limit");
    let provider = RetryingProvider::new(Arc::new(flaky), fast_policy());

    let result = provider.stream(request(), CancellationToken::new()).await;

    assert!(result.is_ok(), "the retry must succeed");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "one failure then one success"
    );
}

#[tokio::test]
async fn a_permanent_failure_is_not_retried() {
    // Three attempts against a bad key costs three requests and gains nothing.
    let (flaky, attempts) = Flaky::always_failing("invalid api key");
    let provider = RetryingProvider::new(Arc::new(flaky), fast_policy());

    let result = provider.stream(request(), CancellationToken::new()).await;

    assert!(result.is_err());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "a permanent failure is tried once"
    );
}

#[tokio::test]
async fn retries_stop_after_the_policy_is_exhausted() {
    let (flaky, attempts) = Flaky::always_failing("503 unavailable");
    let provider = RetryingProvider::new(Arc::new(flaky), fast_policy());

    // A stream has no `Debug`, so `expect_err` cannot be used on the result.
    let Err(error) = provider.stream(request(), CancellationToken::new()).await else {
        panic!("the policy was exhausted");
    };

    // The policy allows three retries, so four attempts in total.
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    let LlmError::Provider(message) = error else {
        panic!("expected a provider error");
    };
    assert!(message.contains("attempt"), "{message}");
}

#[tokio::test]
async fn a_cancelled_request_is_never_retried() {
    // The caller asked for the work to stop; retrying would spend their budget on an answer they withdrew.
    let (flaky, attempts) = Flaky::new(usize::MAX, "rate limit");
    let provider = RetryingProvider::new(Arc::new(flaky), fast_policy());
    // Actually cancelled: the earlier version passed a fresh token, so the test asserted a cancel that had not happened
    // and failed for the right reason -- four attempts were made because nothing asked them to stop.
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = provider.stream(request(), cancel).await;

    assert!(result.is_err());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "a cancelled request must not even make the first attempt"
    );
}

#[tokio::test]
async fn a_cancellation_during_the_attempt_is_not_retried() {
    // A provider that reports the cancellation is the case where the transport learned about it mid-flight.
    struct Cancelling(Arc<AtomicUsize>);
    impl LlmProvider for Cancelling {
        fn stream(
            &self,
            _request: LlmRequest,
            _cancel: CancellationToken,
        ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(LlmError::Cancelled) })
        }
    }
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider =
        RetryingProvider::new(Arc::new(Cancelling(Arc::clone(&attempts))), fast_policy());

    let result = provider.stream(request(), CancellationToken::new()).await;

    assert!(result.is_err());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "a cancel is tried once and reported"
    );
}

#[test]
fn the_policy_is_readable() {
    let policy = fast_policy();
    let provider = RetryingProvider::new(
        Arc::new(Flaky {
            failures: 0,
            attempts: Arc::new(AtomicUsize::new(0)),
            code: String::new(),
            succeed: true,
        }),
        policy.clone(),
    );

    assert_eq!(provider.policy().max_retries, policy.max_retries);
}
