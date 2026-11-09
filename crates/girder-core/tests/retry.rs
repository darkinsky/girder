//! Tests for the retry policy (M3 of the v2 plan).
//!
//! The arithmetic is what a caller reasons about before a request runs: how long the worst case
//! takes, whether jitter can produce zero, whether a policy can outlive its own ceiling.

use std::time::Duration;

use girder_core::{Backoff, FailureCode, RetryError, RetryMode, RetryPolicy};

fn policy() -> RetryPolicy {
    RetryPolicy::default()
}

// ---- backoff arithmetic -----------------------------------------------------------------------

#[test]
fn the_delay_doubles_each_attempt() {
    let backoff = Backoff {
        initial: Duration::from_millis(500),
        max: Duration::from_secs(60),
        jitter_ratio: 0.0,
    };

    assert_eq!(backoff.delay_for(1), Duration::from_millis(500));
    assert_eq!(backoff.delay_for(2), Duration::from_secs(1));
    assert_eq!(backoff.delay_for(3), Duration::from_secs(2));
    assert_eq!(backoff.delay_for(4), Duration::from_secs(4));
}

#[test]
fn a_delay_never_exceeds_the_ceiling() {
    // Without the clamp a misconfigured initial delay produces a wait measured in hours.
    let backoff = Backoff {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(5),
        jitter_ratio: 0.0,
    };

    for attempt in 1..20 {
        assert!(
            backoff.delay_for(attempt) <= Duration::from_secs(5),
            "attempt {attempt} exceeded the ceiling"
        );
    }
}

#[test]
fn a_large_attempt_number_does_not_overflow() {
    // `2u32.pow(31)` would panic in debug; the shift is saturated.
    let backoff = Backoff::default();
    let delay = backoff.delay_for(u32::MAX);
    assert!(delay <= backoff.max);
}

// ---- jitter ------------------------------------------------------------------------------------

#[test]
fn jitter_stays_within_the_configured_ratio() {
    let backoff = Backoff {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(10),
        jitter_ratio: 0.1,
    };

    // Draws across the whole range must land inside [0.9d, 1.1d].
    for draw in [0.0, 0.25, 0.5, 0.75, 0.99] {
        let wait = backoff.jittered(Duration::from_secs(1), draw);
        assert!(
            wait >= Duration::from_millis(900) && wait <= Duration::from_millis(1100),
            "draw {draw} produced {wait:?}"
        );
    }
}

#[test]
fn jitter_never_produces_zero() {
    // A zero delay is a retry storm: every client retries immediately, which is the condition jitter
    // exists to prevent.
    let backoff = Backoff {
        initial: Duration::from_millis(1),
        max: Duration::from_secs(10),
        jitter_ratio: 1.0,
    };

    for draw in [0.0, 0.001, 0.5] {
        assert!(
            backoff.jittered(Duration::from_millis(1), draw) >= Duration::from_millis(1),
            "draw {draw} produced a zero wait"
        );
    }
}

#[test]
fn jitter_respects_the_ceiling() {
    // A ratio above zero must not push a delay past the configured maximum.
    let backoff = Backoff {
        initial: Duration::from_secs(10),
        max: Duration::from_secs(10),
        jitter_ratio: 0.5,
    };

    let wait = backoff.jittered(Duration::from_secs(10), 1.0);
    assert!(wait <= Duration::from_secs(10), "{wait:?}");
}

#[test]
fn zero_jitter_is_deterministic() {
    let backoff = Backoff {
        initial: Duration::from_millis(500),
        max: Duration::from_secs(10),
        jitter_ratio: 0.0,
    };

    for draw in [0.0, 0.5, 0.9] {
        assert_eq!(
            backoff.jittered(Duration::from_millis(500), draw),
            Duration::from_millis(500)
        );
    }
}

// ---- retry decisions --------------------------------------------------------------------------

#[test]
fn the_default_policy_retries_transient_codes() {
    let policy = policy();
    for code in [
        FailureCode::RATE_LIMIT,
        FailureCode::SERVER,
        FailureCode::TIMEOUT,
        FailureCode::TRANSPORT,
        FailureCode::EMPTY_RESPONSE,
    ] {
        assert!(policy.is_retryable(code), "{code}");
    }
}

#[test]
fn an_unknown_code_is_not_retried_by_default() {
    // A policy that retried everything would hammer a provider over a bug in the request.
    assert!(!policy().is_retryable("INVALID_REQUEST"));
}

#[test]
fn always_mode_retries_anything() {
    let policy = RetryPolicy {
        mode: RetryMode::Always,
        max_retries: 0, // ignored in this mode
        backoff: Backoff::default(),
    };

    assert!(policy.is_retryable("ANYTHING"));
    assert!(
        policy.should_retry("ANYTHING", 100),
        "always retries regardless of count"
    );
}

#[test]
fn normal_mode_stops_at_the_retry_limit() {
    let policy = RetryPolicy {
        max_retries: 3,
        ..RetryPolicy::default()
    };

    assert!(policy.should_retry(FailureCode::SERVER, 0));
    assert!(policy.should_retry(FailureCode::SERVER, 2));
    assert!(
        !policy.should_retry(FailureCode::SERVER, 3),
        "the limit is exclusive"
    );
}

#[test]
fn a_non_retryable_code_is_refused_regardless_of_count() {
    let policy = policy();
    assert!(!policy.should_retry("INVALID_REQUEST", 0));
}

// ---- worst case -------------------------------------------------------------------------------

#[test]
fn the_worst_case_is_the_sum_of_the_delays() {
    // A caller that cannot tolerate this must lower the retries or the initial delay, and knowing
    // the number is what makes that decision possible.
    let policy = RetryPolicy {
        max_retries: 3,
        backoff: Backoff {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(60),
            jitter_ratio: 0.0,
        },
        ..RetryPolicy::default()
    };

    // 500 + 1000 + 2000
    assert_eq!(policy.worst_case_total(), Duration::from_millis(3500));
}

#[test]
fn the_worst_case_of_a_clamped_policy_is_bounded() {
    let policy = RetryPolicy {
        max_retries: 10,
        backoff: Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(5),
            jitter_ratio: 0.0,
        },
        ..RetryPolicy::default()
    };

    // The clamp is what keeps this finite: without it, ten doublings would be over 17 minutes.
    assert!(policy.worst_case_total() <= Duration::from_secs(50));
}

// ---- the loop ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_operation_is_not_retried() {
    let mut calls = 0;
    let result = girder_core::run_with_retry(
        &policy(),
        || {
            calls += 1;
            Box::pin(async { Ok::<_, String>(42) })
        },
        |error: &String| error.clone(),
        || 0.5,
    )
    .await;

    assert_eq!(result.unwrap(), 42);
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn an_eligible_failure_is_retried_until_it_succeeds() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let policy = RetryPolicy {
        max_retries: 5,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(2),
            jitter_ratio: 0.0,
        },
        ..RetryPolicy::default()
    };

    let result = girder_core::run_with_retry(
        &policy,
        {
            let attempts = std::sync::Arc::clone(&attempts);
            move || {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    if attempt < 2 {
                        Err(FailureCode::SERVER.to_owned())
                    } else {
                        Ok(7)
                    }
                })
            }
        },
        |error: &String| error.clone(),
        || 0.5,
    )
    .await;

    assert_eq!(result.unwrap(), 7);
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_non_retryable_failure_fails_immediately() {
    let mut calls = 0;
    let error = girder_core::run_with_retry(
        &policy(),
        || {
            calls += 1;
            Box::pin(async { Err::<i32, _>("INVALID_REQUEST".to_owned()) })
        },
        |error: &String| error.clone(),
        || 0.5,
    )
    .await
    .expect_err("must not retry");

    assert!(matches!(error, RetryError::NotRetryable { .. }));
    assert_eq!(
        calls, 1,
        "a non-retryable failure must not be attempted twice"
    );
}

#[tokio::test]
async fn exhausting_the_retries_reports_the_count() {
    let policy = RetryPolicy {
        max_retries: 2,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(2),
            jitter_ratio: 0.0,
        },
        ..RetryPolicy::default()
    };

    let error = girder_core::run_with_retry(
        &policy,
        || Box::pin(async { Err::<i32, _>(FailureCode::SERVER.to_owned()) }),
        |error: &String| error.clone(),
        || 0.5,
    )
    .await
    .expect_err("exhausted");

    let RetryError::Exhausted { attempts, code } = error else {
        panic!("expected Exhausted, got {error:?}");
    };
    // Two retries plus the first attempt.
    assert_eq!(attempts, 3);
    assert_eq!(code, FailureCode::SERVER);
}
