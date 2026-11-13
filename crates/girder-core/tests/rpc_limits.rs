//! Tests for per-connection admission control.
//!
//! Every test drives an injected [`Instant`], so "one second has passed" is an addition rather than
//! a sleep. A limiter tested against the real clock would be both slower and less predictable: a
//! refill assertion would race the scheduler, and the retry-after test below would have to sleep for
//! exactly as long as it is asserting.
//!
//! # Why the module is compiled by path
//!
//! The limiter is not exported from the crate root yet; that wiring is a separate change to
//! `lib.rs`. Compiling the exact source file here — rather than a copy — keeps these tests
//! exercising the same code a consumer will reach through `girder_core::rpc_limits`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/rpc_limits.rs"]
mod rpc_limits;

use std::time::{Duration, Instant};

use rpc_limits::{Limiter, Quota, Refusal};

/// A quota that admits 10 requests per second and 4 at a time, each at most 1 KiB.
///
/// Small enough to exhaust in a few lines, and the three numbers are distinct so a test that
/// confuses one for another fails instead of passing by coincidence.
fn quota() -> Quota {
    Quota {
        in_flight: 4,
        per_second: 10,
        max_request_bytes: 1024,
    }
}

/// A fixed origin, offset a little past the monotonic clock's own start so the backwards-clock test
/// can subtract from it without underflowing. Nothing here sleeps or compares against the wall
/// clock; every later instant in these tests is this one plus an explicit duration.
fn t0() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

/// `t0` plus a number of milliseconds, for readable refill arithmetic.
fn after(base: Instant, millis: u64) -> Instant {
    base + Duration::from_millis(millis)
}

// ---- within quota -----------------------------------------------------------------------------

#[test]
fn requests_within_the_quota_are_admitted() {
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    let first = limiter.admit(now, 100).expect("first request fits");
    let second = limiter.admit(now, 100).expect("second request fits");

    assert_eq!(first.bytes(), 100);
    assert_eq!(second.bytes(), 100);
    assert_eq!(limiter.in_flight(), 2);
}

#[test]
fn a_request_at_exactly_the_size_limit_is_admitted() {
    // The limit is inclusive: a client that sends precisely what it was told it may send must not be
    // refused, or the documented maximum would be wrong by one byte.
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    assert!(limiter.admit(now, 1024).is_ok());
}

// ---- in-flight ceiling -------------------------------------------------------------------------

#[test]
fn exceeding_the_in_flight_limit_is_refused() {
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    for _ in 0..4 {
        limiter
            .admit(now, 8)
            .expect("fits under the in-flight ceiling");
    }

    // Not rate limited: the token bucket still holds six tokens, so the only ceiling left is
    // in-flight, and the refusal must name it rather than blaming the rate.
    assert_eq!(
        limiter.admit(now, 8).expect_err("the ceiling is reached"),
        Refusal::TooManyInFlight { limit: 4 }
    );
    assert_eq!(limiter.in_flight(), 4, "a refused request takes no slot");
}

#[test]
fn a_refused_request_consumes_no_token() {
    // A refusal must leave the bucket untouched. If an in-flight rejection spent a token, retrying
    // after a release would slowly drain the quota of a client that was only ever waiting its turn.
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    let held: Vec<_> = (0..4)
        .map(|_| limiter.admit(now, 8).expect("first four fit"))
        .collect();
    let _ = limiter.admit(now, 8);

    for permit in held {
        limiter.release(permit);
    }

    // Four tokens spent, four releases, and six tokens left in the bucket: the full remaining
    // budget of the second must still be available.
    for _ in 0..6 {
        let permit = limiter.admit(now, 8).expect("token survived the refusal");
        limiter.release(permit);
    }
    assert_eq!(
        limiter.admit(now, 8).expect_err("the bucket is empty"),
        Refusal::RateLimited {
            retry_after: Duration::from_millis(100)
        }
    );
}

// ---- rate limiting -----------------------------------------------------------------------------

#[test]
fn the_rate_limit_stops_a_burst_at_the_quota() {
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    // Ten requests per second is also ten tokens of burst; the eleventh must be refused, and it is
    // not in-flight limited because every permit is released as it is taken.
    for _ in 0..10 {
        let permit = limiter.admit(now, 1).expect("within the per-second budget");
        limiter.release(permit);
    }

    match limiter.admit(now, 1) {
        Err(Refusal::RateLimited { .. }) => {}
        other => {
            panic!("the eleventh request in the same instant must be rate limited, got {other:?}")
        }
    }
}

#[test]
fn retry_after_reflects_the_shortfall_not_a_fixed_second() {
    // Half a token's worth of refill has happened since the burst, so the next token is 50 ms away.
    // Reporting a full second would idle the connection for twenty times as long as it must.
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    for _ in 0..10 {
        let permit = limiter.admit(now, 1).expect("the burst fits");
        limiter.release(permit);
    }

    let Err(Refusal::RateLimited { retry_after }) = limiter.admit(after(now, 50), 1) else {
        panic!("half a token refilled is not a whole one");
    };

    assert_eq!(retry_after, Duration::from_millis(50));
}

#[test]
fn waiting_retry_after_makes_the_request_admissible() {
    // The core contract: `retry_after` is an instruction a client can follow literally. If waiting
    // exactly that long still left the bucket short, every well-behaved client would be refused and
    // retry forever.
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    for _ in 0..10 {
        let permit = limiter.admit(now, 1).expect("the burst fits");
        limiter.release(permit);
    }

    let Err(Refusal::RateLimited { retry_after }) = limiter.admit(after(now, 50), 1) else {
        panic!("the bucket is half refilled");
    };
    assert_eq!(retry_after, Duration::from_millis(50));

    // Exactly the suggested wait, no margin: `div_ceil` in the wait calculation is what makes the
    // boundary land on "admitted" rather than one microsecond short.
    let later = after(now, 50) + retry_after;
    let permit = limiter
        .admit(later, 1)
        .expect("waiting retry_after is enough");
    assert_eq!(limiter.in_flight(), 1);
    limiter.release(permit);
}

#[test]
fn waiting_less_than_retry_after_is_still_refused() {
    // The other half of the boundary: a client that shaves the wait by a millisecond is early, and
    // must be told so — with the smaller remaining wait — rather than admitted.
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    for _ in 0..10 {
        let permit = limiter.admit(now, 1).expect("the burst fits");
        limiter.release(permit);
    }

    let early = after(now, 49);
    assert_eq!(
        limiter.admit(early, 1).expect_err("one millisecond early"),
        Refusal::RateLimited {
            retry_after: Duration::from_millis(51)
        }
    );
}

#[test]
fn a_full_bucket_does_not_bank_surplus() {
    // An idle connection refills to capacity and stops. If the bucket kept counting past its size,
    // a client that disconnected for a minute could come back and fire a minute's worth of requests
    // in one instant, which is a burst the quota is supposed to forbid.
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    let much_later = after(now, 60_000);

    for _ in 0..10 {
        let permit = limiter
            .admit(much_later, 1)
            .expect("the bucket refilled to capacity");
        limiter.release(permit);
    }
    assert_eq!(
        limiter
            .admit(much_later, 1)
            .expect_err("the bucket is capped at capacity"),
        Refusal::RateLimited {
            retry_after: Duration::from_millis(100)
        },
        "an idle minute must not buy more than one second of budget"
    );
}

#[test]
fn a_backwards_clock_grants_no_tokens() {
    // An injected clock is allowed to misbehave; the limiter must not. Reading a backwards step as a
    // huge elapsed span would refill the bucket for free.
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    for _ in 0..10 {
        let permit = limiter.admit(now, 1).expect("the burst fits");
        limiter.release(permit);
    }

    let backwards = now
        .checked_sub(Duration::from_secs(5))
        .expect("the origin is offset so a step back is representable");

    assert!(matches!(
        limiter.admit(backwards, 1),
        Err(Refusal::RateLimited { .. })
    ));
}

#[test]
fn a_zero_rate_denies_everything_and_never_claims_a_retry() {
    // A zero rate is a closed door, not an open one. `Duration::MAX` is the honest answer: there is
    // no time at which a token appears.
    let now = t0();
    let mut limiter = Limiter::new(
        Quota {
            in_flight: 4,
            per_second: 0,
            max_request_bytes: 1024,
        },
        now,
    );

    assert_eq!(
        limiter
            .admit(now, 1)
            .expect_err("a zero rate admits nothing"),
        Refusal::RateLimited {
            retry_after: Duration::MAX
        }
    );
}

// ---- oversized requests ------------------------------------------------------------------------

#[test]
fn an_oversized_request_is_refused() {
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    assert_eq!(
        limiter
            .admit(now, 1025)
            .expect_err("one byte over the limit"),
        Refusal::TooLarge {
            limit: 1024,
            got: 1025
        }
    );
    assert_eq!(limiter.in_flight(), 0, "a refused request takes no slot");
}

#[test]
fn the_size_can_be_checked_before_the_body_is_read() {
    // The header-stage check: the caller knows `Content-Length` before it reads anything, so a
    // request that is too large is refused without allocating its body.
    let limiter = Limiter::new(quota(), t0());

    assert_eq!(
        limiter.check_size(4096),
        Err(Refusal::TooLarge {
            limit: 1024,
            got: 4096
        })
    );
    assert!(limiter.check_size(1024).is_ok());
    assert_eq!(limiter.in_flight(), 0);
}

#[test]
fn a_size_check_spends_nothing() {
    // `check_size` is a query, not an admission: it must be callable as often as a caller likes
    // without moving any counter.
    let now = t0();
    let limiter = Limiter::new(quota(), now);

    for _ in 0..100 {
        assert!(limiter.check_size(1).is_ok());
    }
    assert_eq!(limiter.in_flight(), 0);
}

// ---- release restores the quota -----------------------------------------------------------------

#[test]
fn releasing_permits_restores_the_in_flight_quota() {
    let now = t0();
    let mut limiter = Limiter::new(quota(), now);

    let permits: Vec<_> = (0..4)
        .map(|_| limiter.admit(now, 8).expect("fits"))
        .collect();
    assert_eq!(limiter.in_flight(), 4);

    for permit in permits {
        limiter.release(permit);
    }

    assert_eq!(limiter.in_flight(), 0);
    assert!(limiter.admit(now, 8).is_ok(), "a released slot is reusable");
}

#[test]
fn a_permit_from_another_limiter_frees_nothing() {
    // Cross-connection leak: releasing a foreign permit must not raise this connection's in-flight
    // ceiling. The slot is more valuable than the confusion of a silently inflated quota.
    let now = t0();
    let mut first = Limiter::new(quota(), now);
    let mut second = Limiter::new(quota(), now);

    let foreign = second.admit(now, 8).expect("fits on the second connection");
    let own = first.admit(now, 8).expect("fits on the first connection");
    assert_eq!(first.in_flight(), 1);

    first.release(foreign);
    assert_eq!(
        first.in_flight(),
        1,
        "a permit from another limiter must not release this one's slot"
    );

    first.release(own);
    assert_eq!(first.in_flight(), 0);
}

// ---- isolation between connections --------------------------------------------------------------

#[test]
fn one_limiter_exhausting_its_quota_does_not_affect_another() {
    // The property the whole module exists for: one client's behaviour must not become another
    // client's outage. Drain the first limiter on every axis, then check the second is untouched.
    let now = t0();
    let mut starved = Limiter::new(quota(), now);
    let mut healthy = Limiter::new(quota(), now);

    let held: Vec<_> = (0..4)
        .map(|_| starved.admit(now, 8).expect("fits"))
        .collect();
    assert!(starved.admit(now, 8).is_err());

    for _ in 0..10 {
        let permit = healthy.admit(now, 8).expect("its own budget is untouched");
        healthy.release(permit);
    }
    assert_eq!(healthy.in_flight(), 0);

    // And the drained connection still refills on its own schedule, at the same rate: once the
    // held permits go back, one token is enough for the next request.
    for permit in held {
        starved.release(permit);
    }
    let later = after(now, 100);
    let permit = starved.admit(later, 8).expect("one token refilled");
    starved.release(permit);
    assert_eq!(starved.in_flight(), 0);
}

#[test]
fn two_limiters_have_independent_buckets() {
    // Same instant, same request: one being out of tokens must not refuse the other, or the quota
    // would be global in all but name.
    let now = t0();
    let mut drained = Limiter::new(quota(), now);
    let mut fresh = Limiter::new(quota(), now);

    for _ in 0..10 {
        let permit = drained.admit(now, 1).expect("fits");
        drained.release(permit);
    }

    assert!(matches!(
        drained.admit(now, 1),
        Err(Refusal::RateLimited { .. })
    ));
    assert!(fresh.admit(now, 1).is_ok(), "the other bucket is full");
}

// ---- display -----------------------------------------------------------------------------------

#[test]
fn a_refusal_explains_itself() {
    // The message is what a client logs; it must carry the number that was hit, since "refused" with
    // no reason is unactionable.
    let too_large = Refusal::TooLarge {
        limit: 1024,
        got: 2048,
    };
    assert!(too_large.to_string().contains("2048"));
    assert!(too_large.to_string().contains("1024"));

    let in_flight = Refusal::TooManyInFlight { limit: 4 };
    assert!(in_flight.to_string().contains('4'));

    let rate = Refusal::RateLimited {
        retry_after: Duration::from_millis(100),
    };
    assert!(rate.to_string().contains("100"));
}

#[test]
fn a_refusal_is_a_std_error() {
    // `?` through an `anyhow`/`Box<dyn Error>` boundary is how the RPC handler will report it.
    fn takes_error(_: &dyn std::error::Error) {}

    takes_error(&Refusal::TooManyInFlight { limit: 1 });
}
