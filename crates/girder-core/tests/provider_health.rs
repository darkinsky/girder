//! Tests for the per-provider health tracker.
//!
//! The tests are weighted toward the failure that would make the tracker worse than doing nothing:
//! tripping on a single error. A tracker that does that converts normal flakiness into an outage,
//! so `one_failure_does_not_trip_the_cooldown` and its neighbours below are the core of this file.
//!
//! The rest pins the state machine the module documents: consecutive failures reach the threshold,
//! a success resets the count, an expired cooldown yields `Degraded` rather than `Healthy`, and
//! `pick` preserves the caller's order while skipping providers that are cooling down.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the tracker as written even before the crate root wires it in.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../src/provider_health.rs"]
mod provider_health;

use provider_health::{Health, HealthTracker};

/// A threshold of three and a one-minute cooldown, so the arithmetic in a test is obvious.
fn tracker() -> HealthTracker {
    HealthTracker::new(3, 60_000)
}

/// The providers used by the ordering tests.
fn providers(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

// ---- the core property --------------------------------------------------------------------------

#[test]
fn one_failure_does_not_trip_the_cooldown() {
    let mut tracker = tracker();
    tracker.record_failure("alpha", 1_000, "timeout");

    // Still attemptable, and reported as such: a single dropped connection must not pull a provider
    // out of rotation.
    assert_eq!(
        tracker.health("alpha", 1_000),
        Health::Degraded { failures: 1 }
    );
    assert_eq!(
        tracker
            .pick(&providers(&["alpha"]), 1_000)
            .map(String::as_str),
        Some("alpha")
    );
}

#[test]
fn failures_below_the_threshold_stay_degraded() {
    let mut tracker = tracker();
    tracker.record_failure("alpha", 1_000, "timeout");
    tracker.record_failure("alpha", 2_000, "timeout");

    assert_eq!(
        tracker.health("alpha", 2_000),
        Health::Degraded { failures: 2 }
    );
}

#[test]
fn consecutive_failures_at_the_threshold_become_unavailable() {
    let mut tracker = tracker();
    tracker.record_failure("alpha", 1_000, "served 503");
    tracker.record_failure("alpha", 2_000, "served 503");
    tracker.record_failure("alpha", 3_000, "served 503");

    assert_eq!(
        tracker.health("alpha", 3_000),
        Health::Unavailable {
            until_ms: 63_000,
            reason: "served 503".to_owned(),
        }
    );
}

#[test]
fn the_cooldown_starts_at_the_tripping_failure() {
    let mut tracker = tracker();
    tracker.record_failure("alpha", 1_000, "timeout");
    tracker.record_failure("alpha", 2_000, "timeout");
    // The third failure lands ten seconds after the first: the cooldown runs from *it*, not from
    // the start of the run.
    tracker.record_failure("alpha", 12_000, "timeout");

    assert_eq!(
        tracker.health("alpha", 12_000),
        Health::Unavailable {
            until_ms: 72_000,
            reason: "timeout".to_owned(),
        }
    );
}

#[test]
fn a_failure_during_the_cooldown_re_opens_it() {
    let mut tracker = tracker();
    tracker.record_failure("alpha", 0, "timeout");
    tracker.record_failure("alpha", 0, "timeout");
    tracker.record_failure("alpha", 0, "timeout");
    // A caller that attempts a cooling-down provider and fails again pushes the deadline out.
    tracker.record_failure("alpha", 30_000, "timeout");

    assert_eq!(
        tracker.health("alpha", 30_000),
        Health::Unavailable {
            until_ms: 90_000,
            reason: "timeout".to_owned(),
        }
    );
}

// ---- recovery -----------------------------------------------------------------------------------

#[test]
fn a_success_resets_the_failure_count() {
    let mut tracker = tracker();
    tracker.record_failure("alpha", 1_000, "timeout");
    tracker.record_failure("alpha", 2_000, "timeout");
    tracker.record_success("alpha");

    assert_eq!(tracker.health("alpha", 2_000), Health::Healthy);

    // The definition of the threshold is *consecutive* failures: two before the success plus two
    // after it is four failures, but it is not a run of three, so it must not trip.
    tracker.record_failure("alpha", 3_000, "timeout");
    tracker.record_failure("alpha", 4_000, "timeout");
    assert_eq!(
        tracker.health("alpha", 4_000),
        Health::Degraded { failures: 2 }
    );
}

#[test]
fn a_success_clears_a_tripped_cooldown() {
    let mut tracker = tracker();
    for _ in 0..3 {
        tracker.record_failure("alpha", 1_000, "timeout");
    }
    // An explicit success is better evidence than the passage of time: it clears the cooldown
    // outright rather than leaving the provider degraded.
    tracker.record_success("alpha");

    assert_eq!(tracker.health("alpha", 1_000), Health::Healthy);
    assert_eq!(
        tracker
            .pick(&providers(&["alpha"]), 1_000)
            .map(String::as_str),
        Some("alpha")
    );
}

#[test]
fn an_expired_cooldown_restores_degraded_not_healthy() {
    let mut tracker = tracker();
    for _ in 0..3 {
        tracker.record_failure("alpha", 1_000, "timeout");
    }

    // All three failures landed at 1_000, so the cooldown runs until 61_000. It is still in force
    // one millisecond before that.
    assert!(matches!(
        tracker.health("alpha", 60_999),
        Health::Unavailable { .. }
    ));

    // At expiry the provider may be attempted again -- but the evidence against it is only stale,
    // so it comes back degraded and the first failure re-opens the cooldown.
    assert_eq!(
        tracker.health("alpha", 61_000),
        Health::Degraded { failures: 3 }
    );
    assert_eq!(
        tracker
            .pick(&providers(&["alpha"]), 61_000)
            .map(String::as_str),
        Some("alpha")
    );
}

#[test]
fn a_failure_after_recovery_trips_again_without_a_success() {
    let mut tracker = tracker();
    for _ in 0..3 {
        tracker.record_failure("alpha", 1_000, "timeout");
    }
    // Degraded after the cooldown, the very next failure is enough to cool down again: the count
    // was never reset, and doing so would make a permanently broken provider retry forever.
    tracker.record_failure("alpha", 61_000, "timeout");

    assert_eq!(
        tracker.health("alpha", 61_000),
        Health::Unavailable {
            until_ms: 121_000,
            reason: "timeout".to_owned(),
        }
    );
}

#[test]
fn an_unknown_provider_is_healthy() {
    let tracker = tracker();

    assert_eq!(tracker.health("never-seen", 1_000), Health::Healthy);
    assert_eq!(
        tracker
            .pick(&providers(&["never-seen"]), 1_000)
            .map(String::as_str),
        Some("never-seen")
    );
}

// ---- selection ----------------------------------------------------------------------------------

#[test]
fn pick_skips_a_provider_that_is_cooling_down() {
    let mut tracker = tracker();
    for _ in 0..3 {
        tracker.record_failure("alpha", 1_000, "timeout");
    }

    assert_eq!(
        tracker
            .pick(&providers(&["alpha", "beta"]), 1_000)
            .map(String::as_str),
        Some("beta")
    );
}

#[test]
fn pick_keeps_the_callers_order() {
    let mut tracker = tracker();
    // `alpha` has failed once and `gamma` has never failed, yet `alpha` still wins: the list is the
    // caller's preference, and a tracker that preferred the healthier provider would be overriding
    // a priority it cannot see.
    tracker.record_failure("alpha", 1_000, "timeout");

    assert_eq!(
        tracker
            .pick(&providers(&["alpha", "beta", "gamma"]), 1_000)
            .map(String::as_str),
        Some("alpha")
    );

    // Even when the first candidate is the *only* degraded one, order still decides.
    tracker.record_failure("alpha", 2_000, "timeout");
    assert_eq!(
        tracker
            .pick(&providers(&["alpha", "beta"]), 2_000)
            .map(String::as_str),
        Some("alpha")
    );
}

#[test]
fn pick_skips_every_cooling_provider_in_order() {
    let mut tracker = tracker();
    for _ in 0..3 {
        tracker.record_failure("alpha", 1_000, "timeout");
        tracker.record_failure("beta", 1_000, "timeout");
    }

    assert_eq!(
        tracker
            .pick(&providers(&["alpha", "beta", "gamma"]), 1_000)
            .map(String::as_str),
        Some("gamma")
    );
}

#[test]
fn pick_returns_none_when_every_provider_is_unavailable() {
    let mut tracker = tracker();
    for _ in 0..3 {
        tracker.record_failure("alpha", 1_000, "timeout");
        tracker.record_failure("beta", 1_000, "timeout");
    }

    assert_eq!(tracker.pick(&providers(&["alpha", "beta"]), 1_000), None);
}

#[test]
fn pick_returns_none_for_an_empty_list() {
    let tracker = tracker();

    // Nothing to pick from is not an error: a caller with no configured providers asks the same
    // question and gets the same honest `None`.
    assert_eq!(tracker.pick(&[], 1_000), None);
}

// ---- configuration ------------------------------------------------------------------------------

#[test]
fn a_zero_threshold_is_clamped_to_one() {
    let mut tracker = HealthTracker::new(0, 1_000);
    assert_eq!(tracker.threshold(), 1);

    // A single failure trips, which is the behaviour the caller asked for by naming a threshold
    // below the minimum -- the alternative, never tripping, would be silently different.
    tracker.record_failure("alpha", 0, "timeout");
    assert!(matches!(
        tracker.health("alpha", 0),
        Health::Unavailable { .. }
    ));
}

#[test]
fn a_zero_cooldown_expires_immediately() {
    let mut tracker = HealthTracker::new(1, 0);
    tracker.record_failure("alpha", 1_000, "timeout");

    // The deadline is 1_000 + 0, so the cooldown is already over at the instant it tripped:
    // `now_ms < until_ms` is false for an equal deadline. A zero cooldown therefore degrades rather
    // than refusing, which is what a caller asking for "no cooldown" should get.
    assert_eq!(
        tracker.health("alpha", 1_000),
        Health::Degraded { failures: 1 }
    );
}

#[test]
fn a_saturating_cooldown_does_not_overflow() {
    let mut tracker = HealthTracker::new(1, u64::MAX);
    tracker.record_failure("alpha", u64::MAX, "timeout");

    assert_eq!(
        tracker.health("alpha", 0),
        Health::Unavailable {
            until_ms: u64::MAX,
            reason: "timeout".to_owned(),
        }
    );
}

#[test]
fn providers_are_tracked_independently() {
    let mut tracker = tracker();
    for _ in 0..3 {
        tracker.record_failure("alpha", 1_000, "timeout");
    }

    assert!(matches!(
        tracker.health("alpha", 1_000),
        Health::Unavailable { .. }
    ));
    assert_eq!(tracker.health("beta", 1_000), Health::Healthy);
}
