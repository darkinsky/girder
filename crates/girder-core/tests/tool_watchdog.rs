//! Tests for the per-call tool time budget.
//!
//! The module is wired into the crate by the integrator, so this test loads the source file directly; that keeps the
//! test runnable against `src/tool_watchdog.rs` alone, before the crate root names it.
//!
//! What separates a useful watchdog from a blunt one is covered here: a single default cannot serve both a `read` and
//! a build, so overrides must win; the deadline is an inclusive instant, so landing exactly on it counts as expired
//! rather than granting a free millisecond; and a policy that forbids cancelling must produce a report rather than a
//! cancellation, because interrupting a half-written file is worse than waiting. The clock is injected everywhere, so
//! every boundary is asserted exactly, with no sleep and no tolerance window.

#[path = "../src/tool_watchdog.rs"]
mod tool_watchdog;

use tool_watchdog::{Policy, Verdict, Watch};

/// A policy with a one-second default and both directions pinned, which is the shape a deployment actually writes.
fn policy() -> Policy {
    Policy::new(1_000, true)
        .with_override("bash", 600_000)
        .with_override("read", 5_000)
}

#[test]
fn an_unknown_tool_uses_the_default() {
    let policy = policy();
    assert_eq!(policy.timeout_for("glob"), 1_000);
    assert_eq!(policy.timeout_for("plugin_supplied_tool"), 1_000);
    assert_eq!(Watch::start("glob", &policy, 0).deadline_ms(), 1_000);
}

#[test]
fn an_override_extends_or_shortens_the_default() {
    let policy = policy();
    // Longer than the default: a build is a legitimate minute of work.
    assert_eq!(policy.timeout_for("bash"), 600_000);
    // Shorter than the default: a read that takes a second is already broken.
    assert_eq!(policy.timeout_for("read"), 5_000);
}

#[test]
fn the_override_is_what_the_watch_actually_uses() {
    let policy = policy();
    let bash = Watch::start("bash", &policy, 0);
    let read = Watch::start("read", &policy, 0);

    assert_eq!(bash.timeout_ms(), 600_000);
    assert_eq!(bash.deadline_ms(), 600_000);
    assert_eq!(read.timeout_ms(), 5_000);
    assert_eq!(read.deadline_ms(), 5_000);

    // The same instant is inside the build's budget and past the read's, which is the whole point of the override.
    assert_eq!(bash.verdict(60_000), Verdict::Running);
    assert_eq!(read.verdict(60_000), Verdict::Cancel);
}

#[test]
fn expiring_exactly_at_the_deadline_counts_as_expired() {
    let watch = Watch::start("read", &policy(), 1_000);
    assert_eq!(watch.deadline_ms(), 6_000);

    assert!(!watch.expired(5_999));
    // `>=`, not `>`: the budget is spent at the deadline, not one millisecond after it.
    assert!(watch.expired(6_000));
    assert!(watch.expired(6_001));

    assert_eq!(watch.verdict(5_999), Verdict::Running);
    assert_eq!(watch.verdict(6_000), Verdict::Cancel);
}

#[test]
fn cancelling_forbidden_turns_a_timeout_into_a_report() {
    let policy = Policy::new(1_000, false);
    let watch = Watch::start("write", &policy, 0);

    assert_eq!(watch.verdict(999), Verdict::Running);
    // Expired, but the policy says the call must be allowed to finish.
    assert_eq!(watch.verdict(1_000), Verdict::Report);
    assert!(watch.verdict(1_000).is_expired());
    assert!(!watch.verdict(1_000).should_cancel());
    assert!(!watch.verdict(1_000).is_running());
}

#[test]
fn cancelling_allowed_turns_a_timeout_into_a_cancel() {
    let watch = Watch::start("bash", &Policy::new(1_000, true), 0);
    assert_eq!(watch.verdict(1_000), Verdict::Cancel);
    assert!(watch.verdict(1_000).should_cancel());
}

#[test]
fn remaining_is_zero_once_the_deadline_has_passed() {
    let watch = Watch::start("read", &policy(), 0);

    assert_eq!(watch.remaining_ms(0), 5_000);
    assert_eq!(watch.remaining_ms(4_999), 1);
    assert_eq!(watch.remaining_ms(5_000), 0);
    // Far past the deadline still reports zero rather than underflowing into a huge number.
    assert_eq!(watch.remaining_ms(5_001), 0);
    assert_eq!(watch.remaining_ms(u64::MAX), 0);
}

#[test]
fn an_empty_override_table_is_a_valid_policy() {
    let policy = Policy {
        default_ms: 2_500,
        overrides: std::collections::BTreeMap::new(),
        cancel_on_timeout: true,
    };

    assert!(policy.overrides.is_empty());
    assert_eq!(policy.timeout_for("bash"), 2_500);
    assert_eq!(policy.timeout_for("read"), 2_500);
    assert_eq!(Watch::start("bash", &policy, 100).deadline_ms(), 2_600);
}

#[test]
fn a_zero_timeout_expires_immediately() {
    let watch = Watch::start("read", &Policy::new(0, true), 42);
    assert!(watch.expired(42));
    assert_eq!(watch.remaining_ms(42), 0);
    assert_eq!(watch.verdict(42), Verdict::Cancel);
}

#[test]
fn a_huge_timeout_saturates_rather_than_wrapping() {
    // Wrapping here would turn "never" into "long ago", cancelling a call the policy meant to allow.
    let watch = Watch::start("bash", &Policy::new(u64::MAX, true), 10);
    assert_eq!(watch.deadline_ms(), u64::MAX);
    assert!(!watch.expired(u64::MAX - 1));
    assert!(watch.expired(u64::MAX));
}

#[test]
fn the_verdict_is_a_function_of_the_injected_clock_only() {
    // Asking twice with the same reading answers the same thing: no hidden `Instant::now()` inside.
    let watch = Watch::start("bash", &policy(), 0);
    assert_eq!(watch.verdict(0), Verdict::Running);
    assert_eq!(watch.verdict(0), Verdict::Running);
    assert_eq!(watch.verdict(600_000), Verdict::Cancel);
    assert_eq!(watch.verdict(600_000), Verdict::Cancel);
}

#[test]
fn the_default_policy_bounds_every_tool_and_cancels() {
    let policy = Policy::default();
    assert_eq!(policy.default_ms, Policy::DEFAULT_MS);
    assert!(policy.overrides.is_empty());
    assert!(policy.cancel_on_timeout);
    assert_eq!(policy.timeout_for("bash"), Policy::DEFAULT_MS);
    assert_eq!(policy.timeout_for("read"), Policy::DEFAULT_MS);
}
