//! Tests for the backoff strategy and retry decision.
//!
//! The module is wired into the crate by the integrator, so this test loads the source file
//! directly; that keeps the suite runnable against `src/backoff.rs` alone.
//!
//! What matters here is arithmetic a caller can predict: a doubling that is exact, a ceiling that
//! actually holds at attempt 10 and 4000, jitter that stays inside the band it advertises, a
//! server hint that outranks local arithmetic, and a status table that never retries a failure
//! whose repeat would be identical.

#[path = "../src/backoff.rs"]
mod backoff;

use backoff::{Retryable, Strategy, should_retry};

/// A doubling strategy with jitter off, so an assertion can state an exact number.
fn doubling() -> Strategy {
    Strategy {
        base_ms: 100,
        multiplier: 2,
        max_ms: 10_000,
        jitter: 0.0,
    }
}

fn fixed() -> Strategy {
    Strategy {
        base_ms: 250,
        multiplier: 1,
        max_ms: 10_000,
        jitter: 0.0,
    }
}

// ---- fixed interval ---------------------------------------------------------------------------

#[test]
fn multiplier_one_never_grows() {
    let strategy = fixed();
    for attempt in 1..=6 {
        assert_eq!(strategy.delay_ms(attempt, 0.0), 250, "attempt {attempt}");
    }
}

#[test]
fn a_fixed_interval_still_respects_its_ceiling() {
    let strategy = Strategy {
        base_ms: 5_000,
        multiplier: 1,
        max_ms: 2_000,
        jitter: 0.0,
    };
    // `base_ms > max_ms` is a tuning error rather than a crash: the ceiling wins.
    assert_eq!(strategy.delay_ms(1, 0.0), 2_000);
    assert_eq!(strategy.delay_ms(9, 0.0), 2_000);
}

// ---- exponential growth (core) ----------------------------------------------------------------

#[test]
fn the_delay_doubles_each_attempt() {
    let strategy = doubling();
    assert_eq!(strategy.delay_ms(1, 0.0), 100);
    assert_eq!(strategy.delay_ms(2, 0.0), 200);
    assert_eq!(strategy.delay_ms(3, 0.0), 400);
    assert_eq!(strategy.delay_ms(4, 0.0), 800);
    assert_eq!(strategy.delay_ms(5, 0.0), 1_600);
    assert_eq!(strategy.delay_ms(6, 0.0), 3_200);
    assert_eq!(strategy.delay_ms(7, 0.0), 6_400);
}

#[test]
fn the_first_retry_waits_exactly_the_base() {
    // Jitter is switched off here: the claim is about the schedule, and a draw of `0.0` is the
    // *most negative* jitter case, not the neutral one.
    for named in [
        doubling(),
        fixed(),
        Strategy::transient(),
        Strategy::rate_limited(),
    ] {
        let strategy = Strategy {
            jitter: 0.0,
            ..named
        };
        assert_eq!(
            strategy.delay_ms(1, 0.0),
            strategy.base_ms,
            "a ratio of 1 at attempt 1 means the base is the first wait"
        );
        // The neutral draw reproduces the base even with jitter enabled.
        assert_eq!(
            named.delay_ms(1, 0.5),
            named.base_ms,
            "the midpoint draw is the unjittered delay"
        );
    }
}

#[test]
fn a_multiplier_of_three_triples() {
    let strategy = Strategy {
        base_ms: 10,
        multiplier: 3,
        max_ms: 100_000,
        jitter: 0.0,
    };
    assert_eq!(strategy.delay_ms(1, 0.0), 10);
    assert_eq!(strategy.delay_ms(2, 0.0), 30);
    assert_eq!(strategy.delay_ms(3, 0.0), 90);
    assert_eq!(strategy.delay_ms(4, 0.0), 270);
}

// ---- the cap (core) ---------------------------------------------------------------------------

#[test]
fn the_delay_is_capped() {
    let strategy = Strategy {
        base_ms: 100,
        multiplier: 2,
        max_ms: 5_000,
        jitter: 0.0,
    };
    // 100, 200, 400, ... 3200 are under the cap; 6400 would exceed it.
    assert_eq!(strategy.delay_ms(6, 0.0), 3_200);
    assert_eq!(strategy.delay_ms(7, 0.0), 5_000);
    // The un-capped formula would be ~3.4 years at attempt 30; the cap is the whole point.
    assert_eq!(strategy.delay_ms(30, 0.0), 5_000);
}

#[test]
fn a_large_attempt_cannot_overflow_or_hang() {
    let strategy = doubling();
    // `2^63` and beyond: the exponent is clamped, the multiply saturates, and the cap applies.
    for attempt in [64, 1_000, 100_000, u32::MAX] {
        assert_eq!(strategy.delay_ms(attempt, 0.0), 10_000, "attempt {attempt}");
    }
}

#[test]
fn no_wait_is_ever_zero() {
    // A zero base with jitter at its most negative would otherwise produce a busy loop.
    let strategy = Strategy {
        base_ms: 0,
        multiplier: 2,
        max_ms: 1_000,
        jitter: 1.0,
    };
    for attempt in 1..=5 {
        assert!(
            strategy.delay_ms(attempt, 0.0) >= 1,
            "attempt {attempt} must still wait"
        );
    }
    let zero_ceiling = Strategy {
        base_ms: 0,
        multiplier: 1,
        max_ms: 0,
        jitter: 0.0,
    };
    assert_eq!(zero_ceiling.delay_ms(3, 0.0), 1);
}

// ---- attempt = 0 ------------------------------------------------------------------------------

#[test]
fn attempt_zero_is_a_first_retry_not_an_underflow() {
    let strategy = doubling();
    assert_eq!(strategy.delay_ms(0, 0.0), 100);
    assert_eq!(strategy.delay_ms(0, 0.0), strategy.delay_ms(1, 0.0));
}

#[test]
fn attempt_zero_is_one_retry_later_at_every_multiplier() {
    for multiplier in [1, 2, 3, 7] {
        let strategy = Strategy {
            base_ms: 40,
            multiplier,
            max_ms: 100_000,
            jitter: 0.0,
        };
        assert_eq!(
            strategy.delay_ms(0, 0.0),
            strategy.delay_ms(1, 0.0),
            "multiplier {multiplier}"
        );
    }
}

// ---- jitter -----------------------------------------------------------------------------------

#[test]
fn zero_jitter_is_exactly_the_base_schedule() {
    let strategy = doubling();
    // Every draw, including the extremes, must leave a jitter-free schedule untouched.
    for draw in [0.0, 0.25, 0.5, 0.75, 1.0] {
        assert_eq!(strategy.delay_ms(4, draw), 800, "draw {draw}");
    }
}

#[test]
fn jitter_stays_inside_its_band() {
    let strategy = Strategy {
        base_ms: 1_000,
        multiplier: 2,
        max_ms: 1_000_000,
        jitter: 0.2,
    };
    // The band is symmetric: [0.8d, 1.2d].
    let low = strategy.delay_ms(1, 0.0);
    let high = strategy.delay_ms(1, 1.0);
    let middle = strategy.delay_ms(1, 0.5);
    assert_eq!(low, 800);
    assert_eq!(high, 1_200);
    assert_eq!(middle, 1_000);

    for attempt in 1..=6 {
        let base = strategy.delay_ms(attempt, 0.5);
        // Integer arithmetic for the band: multiply by 5 and by 6 to avoid a float cast and the
        // rounding questions that come with it.
        let lo = base * 4 / 5;
        let hi = base * 6 / 5;
        for step in 0..=20 {
            let draw = f64::from(step) / 20.0;
            let delay = strategy.delay_ms(attempt, draw);
            assert!(
                delay >= lo && delay <= hi,
                "attempt {attempt} draw {draw} gave {delay}, outside [{lo}, {hi}]"
            );
        }
    }
}

#[test]
fn jitter_is_monotonic_in_the_draw() {
    // A higher draw must never produce a shorter wait, or the distribution is not monotone and a
    // range check would pass while the spread was scrambled.
    let strategy = Strategy {
        base_ms: 1_000,
        multiplier: 2,
        max_ms: 1_000_000,
        jitter: 0.5,
    };
    let mut previous = 0;
    for step in 0..=100 {
        let delay = strategy.delay_ms(3, f64::from(step) / 100.0);
        assert!(
            delay >= previous,
            "draw {} went backwards",
            f64::from(step) / 100.0
        );
        previous = delay;
    }
    assert_eq!(strategy.delay_ms(3, 0.0), 2_000);
    assert_eq!(strategy.delay_ms(3, 1.0), 6_000);
}

#[test]
fn jitter_never_exceeds_the_ceiling() {
    let strategy = Strategy {
        base_ms: 1_000,
        multiplier: 2,
        max_ms: 1_000,
        jitter: 1.0,
    };
    // Base and ceiling coincide, so jitter 1.0 maps the draw onto [0, 2d]: only the top half of the
    // range can reach the cap, and nothing may go above it or reach zero either.
    assert_eq!(strategy.delay_ms(1, 0.0), 1);
    assert_eq!(strategy.delay_ms(1, 0.25), 500);
    for draw in [0.5, 0.75, 1.0] {
        assert_eq!(strategy.delay_ms(1, draw), 1_000, "draw {draw}");
    }
    // Jitter is applied after the base is capped, so a negative draw may fall below the cap but the
    // ceiling still holds above and the 1ms floor holds below.
    let roomy = Strategy {
        base_ms: 1_000,
        multiplier: 2,
        max_ms: 1_100,
        jitter: 1.0,
    };
    for draw in [0.0, 0.5, 1.0] {
        let delay = roomy.delay_ms(2, draw);
        assert!(
            (1..=1_100).contains(&delay),
            "draw {draw} gave {delay}, outside [1, 1100]"
        );
    }
    assert_eq!(roomy.delay_ms(2, 1.0), 1_100);
    assert_eq!(roomy.delay_ms(2, 0.0), 1);
}

#[test]
fn a_non_finite_draw_is_neutral_rather_than_fatal() {
    let strategy = Strategy {
        base_ms: 1_000,
        multiplier: 2,
        max_ms: 10_000,
        jitter: 0.2,
    };
    for draw in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(strategy.delay_ms(2, draw), 2_000, "draw {draw}");
    }
    // An out-of-range draw is clamped rather than trusted.
    assert_eq!(strategy.delay_ms(2, -5.0), 1_600);
    assert_eq!(strategy.delay_ms(2, 5.0), 2_400);
}

#[test]
fn a_non_finite_jitter_disables_jitter() {
    for jitter in [f64::NAN, f64::INFINITY] {
        let strategy = Strategy {
            base_ms: 500,
            multiplier: 2,
            max_ms: 10_000,
            jitter,
        };
        assert_eq!(strategy.delay_ms(1, 0.0), 500, "jitter {jitter}");
        assert_eq!(strategy.delay_ms(1, 1.0), 500, "jitter {jitter}");
    }
}

// ---- retry_after takes priority ---------------------------------------------------------------

#[test]
fn the_server_hint_outranks_the_computed_delay() {
    let strategy = Strategy {
        base_ms: 100,
        multiplier: 2,
        max_ms: 10_000,
        jitter: 0.0,
    };
    // The local schedule says 800ms; the server says 3s, and the server is the one that knows.
    assert_eq!(strategy.wait_ms(4, 0.0, Some(3_000)), 3_000);
    // And the hint is not jittered: undercutting it earns another rejection.
    assert_eq!(strategy.wait_ms(4, 1.0, Some(3_000)), 3_000);
    assert_eq!(strategy.wait_ms(4, 0.0, None), 800);
}

#[test]
fn the_hint_applies_even_when_it_is_shorter() {
    let strategy = doubling();
    // A hint below the local delay is still the server's answer, so it wins.
    assert_eq!(strategy.wait_ms(6, 0.0, Some(50)), 50);
}

#[test]
fn a_hint_cannot_exceed_the_strategy_ceiling() {
    let strategy = Strategy {
        base_ms: 100,
        multiplier: 2,
        max_ms: 30_000,
        jitter: 0.0,
    };
    // `Retry-After: 86400` from a misconfigured proxy must not park a turn for a day.
    assert_eq!(strategy.wait_ms(1, 0.0, Some(86_400_000)), 30_000);
    // A zero hint would otherwise mean a busy loop.
    assert_eq!(strategy.wait_ms(1, 0.0, Some(0)), 1);
}

#[test]
fn the_hint_does_not_change_retryability() {
    // A delay on a 401 is a delay, not permission: the repeat presents the same credentials.
    let decision = should_retry(Some(401), Some(30_000));
    assert!(!decision.is_yes());
    assert!(decision.reason().contains("30000ms"));
}

// ---- retryable vs not -------------------------------------------------------------------------

#[test]
fn the_listed_transient_statuses_are_retryable() {
    for status in [408, 425, 429, 500, 502, 503, 504] {
        let decision = should_retry(Some(status), None);
        assert!(
            decision.is_yes(),
            "{status} should be retryable, got: {}",
            decision.reason()
        );
        assert!(
            decision.reason().starts_with(&status.to_string()),
            "the reason names the status: {}",
            decision.reason()
        );
    }
}

#[test]
fn the_listed_permanent_statuses_are_not_retryable() {
    for status in [400, 401, 403, 404, 405, 409, 410, 413, 422] {
        let decision = should_retry(Some(status), None);
        assert!(
            !decision.is_yes(),
            "{status} should not be retryable, got: {}",
            decision.reason()
        );
    }
}

#[test]
fn an_unlisted_status_does_not_retry_by_default() {
    // 418 is a test convention, 501 means the server will never implement the endpoint, and a 2xx
    // reaching this function means the caller asked the wrong question -- none is worth repeating.
    for status in [200, 204, 418, 451, 501, 505] {
        let decision = should_retry(Some(status), None);
        assert!(!decision.is_yes(), "{status} should not be retryable");
        assert!(
            decision.reason().contains(&status.to_string()),
            "an unlisted status still explains itself: {}",
            decision.reason()
        );
    }
}

#[test]
fn a_missing_status_is_the_most_retryable_failure() {
    let decision = should_retry(None, None);
    assert!(decision.is_yes());
    assert!(decision.reason().contains("no HTTP response"));
}

#[test]
fn a_hint_is_mentioned_on_a_transient_failure() {
    let decision = should_retry(Some(429), Some(2_000));
    assert!(decision.is_yes());
    assert!(decision.reason().contains("2000ms"));
    assert!(
        decision.reason().contains("overrides"),
        "the reason says the hint outranks local arithmetic: {}",
        decision.reason()
    );
}

#[test]
fn every_decision_explains_itself() {
    let statuses = [
        None,
        Some(400),
        Some(408),
        Some(401),
        Some(429),
        Some(500),
        Some(503),
        Some(999),
    ];
    for status in statuses {
        for hint in [None, Some(1_500)] {
            let decision = should_retry(status, hint);
            assert!(
                !decision.reason().trim().is_empty(),
                "a decision without a reason cannot be logged: {status:?} {hint:?}"
            );
        }
    }
}

#[test]
fn the_retryable_helpers_agree_with_the_variants() {
    let yes = Retryable::Yes {
        reason: "transient".to_owned(),
    };
    let no = Retryable::No {
        reason: "permanent".to_owned(),
    };
    assert!(yes.is_yes());
    assert_eq!(yes.reason(), "transient");
    assert!(!no.is_yes());
    assert_eq!(no.reason(), "permanent");
}

// ---- named strategies differ where it matters -------------------------------------------------

#[test]
fn rate_limiting_waits_longer_than_a_dropped_connection() {
    // The whole reason the module has two named strategies: a 429 is not a blip.
    let transient = Strategy::transient();
    let limited = Strategy::rate_limited();
    for attempt in 1..=5 {
        assert!(
            limited.delay_ms(attempt, 0.5) > transient.delay_ms(attempt, 0.5),
            "attempt {attempt}: rate limiting must back off further"
        );
    }
    assert!(limited.max_ms > transient.max_ms);
}

#[test]
fn the_named_strategies_stay_inside_their_own_ceilings() {
    for strategy in [
        Strategy::transient(),
        Strategy::rate_limited(),
        Strategy::default(),
    ] {
        for attempt in [1, 2, 3, 7, 20, 100, u32::MAX] {
            for draw in [0.0, 0.5, 1.0] {
                let delay = strategy.delay_ms(attempt, draw);
                assert!(
                    delay >= 1 && delay <= strategy.max_ms.max(1),
                    "attempt {attempt} draw {draw} gave {delay}, outside the strategy's ceiling"
                );
            }
        }
    }
}

#[test]
fn the_default_matches_the_retry_module_it_mirrors() {
    let strategy = Strategy::default();
    assert_eq!(strategy.base_ms, 500);
    assert_eq!(strategy.multiplier, 2);
    assert_eq!(strategy.max_ms, 10_000);
    assert!((strategy.jitter - 0.1).abs() < f64::EPSILON);
}
