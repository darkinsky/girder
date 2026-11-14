//! Tests for the sliding-window usage quota.
//!
//! Two properties carry the module and get most of the cases here. The first is that a window *forgets*:
//! a record older than `window_ms` must stop counting, or a day-long quota would be a lifetime one. The
//! second is that an over-quota record still lands: a tracker that dropped it would report a client that
//! retried a thousand times as one that stopped at the ceiling, which is precisely the client a quota
//! exists to catch.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the quota as written even before the crate root wires it in — the same arrangement
//! `tests/tool_limits.rs` uses.

#[path = "../src/usage_quota.rs"]
mod usage_quota;

use usage_quota::{Quota, Totals, Tracker, Usage, Verdict};

/// A one-second window allowing 100 input and 50 output tokens.
fn one_second() -> Quota {
    Quota::new(1_000, 100, 50)
}

// ---- within quota ------------------------------------------------------------------------------

#[test]
fn usage_inside_both_ceilings_is_within() {
    let mut tracker = Tracker::new(one_second());

    assert_eq!(
        tracker.record(Usage::new(0, 40, 20)),
        Verdict::Within,
        "both counts are under their ceilings"
    );
    assert_eq!(tracker.record(Usage::new(100, 30, 15)), Verdict::Within);
    assert_eq!(tracker.totals(200), Totals::new(70, 35));
}

#[test]
fn exactly_the_ceiling_is_still_within() {
    // "At most 100" allows the hundredth token. Stopping on it would make the ceiling mean 99, which is a
    // bug only a boundary test finds.
    let mut tracker = Tracker::new(one_second());

    assert_eq!(tracker.record(Usage::new(0, 100, 50)), Verdict::Within);
    assert_eq!(tracker.totals(0), Totals::new(100, 50));
}

#[test]
fn an_empty_tracker_reports_nothing_used_and_everything_left() {
    let tracker = Tracker::new(one_second());

    assert_eq!(tracker.totals(0), Totals::ZERO);
    assert!(tracker.totals(0).is_zero());
    assert!(
        !Totals::new(1, 0).is_zero(),
        "one direction being non-zero is enough to be a real spend"
    );
    assert_eq!(tracker.remaining(0), Totals::new(100, 50));
    assert_eq!(tracker.records(), 0);
}

// ---- over one ceiling --------------------------------------------------------------------------

#[test]
fn passing_the_input_ceiling_is_reported() {
    let mut tracker = Tracker::new(one_second());

    assert_eq!(
        tracker.record(Usage::new(0, 101, 10)),
        Verdict::InputExceeded {
            used: 101,
            max: 100
        },
        "output is well under its ceiling, so only input is named"
    );
}

#[test]
fn passing_the_output_ceiling_is_reported() {
    let mut tracker = Tracker::new(one_second());

    assert_eq!(
        tracker.record(Usage::new(0, 10, 51)),
        Verdict::OutputExceeded { used: 51, max: 50 },
        "input is well under its ceiling, so only output is named"
    );
}

#[test]
fn passing_both_ceilings_is_reported_once_not_twice() {
    let mut tracker = Tracker::new(one_second());

    assert_eq!(
        tracker.record(Usage::new(0, 150, 60)),
        Verdict::BothExceeded {
            input: 150,
            output: 60
        },
        "one verdict names both counts, so a caller cannot handle one and forget the other"
    );
}

#[test]
fn a_ceiling_of_zero_allows_nothing() {
    // Zero is a hard stop, not an "off" switch: a deployment staging a rollout may want to record without
    // allowing anything, and reading zero as unlimited would invert that into an open door.
    let mut tracker = Tracker::new(Quota::new(1_000, 0, 0));

    assert_eq!(
        tracker.record(Usage::new(0, 1, 1)),
        Verdict::BothExceeded {
            input: 1,
            output: 1
        }
    );
    assert_eq!(
        tracker.record(Usage::new(0, 0, 0)),
        Verdict::BothExceeded {
            input: 1,
            output: 1
        },
        "using nothing does not clear what was already spent"
    );

    // A zero-cost record on a fresh tracker is the only thing zero allows.
    let mut fresh = Tracker::new(Quota::new(1_000, 0, 0));
    assert!(fresh.record(Usage::new(0, 0, 0)).is_within());
}

// ---- over quota, still recorded -----------------------------------------------------------------

#[test]
fn an_over_quota_record_still_counts() {
    // The core property. A tracker that refused the record would report a client that retried as one that
    // stopped at the ceiling, and the retries are exactly the spend worth seeing.
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(0, 120, 0));
    assert_eq!(tracker.totals(0), Totals::new(120, 0));

    let _ = tracker.record(Usage::new(10, 80, 0));
    assert_eq!(
        tracker.totals(20),
        Totals::new(200, 0),
        "the second over-quota record is added, not discarded"
    );
    assert_eq!(tracker.records(), 2, "both records are held");
}

#[test]
fn repeated_retries_keep_growing_the_total() {
    // A retry loop is the client a quota is for. Every attempt is billed, so every attempt must show.
    let mut tracker = Tracker::new(one_second());

    for step in 0..5 {
        let _ = tracker.record(Usage::new(step * 10, 100, 0));
    }

    assert_eq!(tracker.totals(40), Totals::new(500, 0));
    assert_eq!(
        tracker.remaining(40),
        Totals::new(0, 50),
        "remaining is clamped at zero, so an overshoot does not wrap or go negative"
    );
}

#[test]
fn a_verdict_after_an_overshoot_stays_over() {
    // Because the overspending record persists, every later record is still judged against a passed
    // ceiling. A verdict computed from a discarded record would report "within" again.
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(0, 200, 0));
    assert_eq!(
        tracker.record(Usage::new(1, 0, 0)),
        Verdict::InputExceeded {
            used: 200,
            max: 100
        }
    );
}

// ---- the window --------------------------------------------------------------------------------

#[test]
fn records_outside_the_window_are_discarded() {
    // The core property of the sliding window. At `1_000` the record made at `0` is exactly one window
    // old, so it is out; at `999` it is still in.
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(0, 90, 40));
    assert_eq!(tracker.totals(999), Totals::new(90, 40));

    // Recording at the boundary is what physically drops the old entry: `totals` filters, `record` prunes.
    assert_eq!(
        tracker.record(Usage::new(1_000, 5, 5)),
        Verdict::Within,
        "the expired 90/40 must not be counted against the new record"
    );
    assert_eq!(tracker.totals(1_000), Totals::new(5, 5));
    assert_eq!(
        tracker.records(),
        1,
        "the expired record is gone from memory"
    );
}

#[test]
fn the_window_slides_rather_than_resetting_on_a_boundary() {
    // A fixed bucket would reset the whole total at `1_000`, so two records made either side of that
    // instant would never be counted together. A sliding window keeps every record younger than the
    // window, whatever boundary it falls on, so the two are summed until the older one is a full window
    // old.
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(0, 1, 1));
    let _ = tracker.record(Usage::new(900, 60, 30));

    assert_eq!(
        tracker.totals(999),
        Totals::new(61, 31),
        "both records are younger than one window"
    );
    assert_eq!(
        tracker.totals(1_000),
        Totals::new(60, 30),
        "at exactly one window the older record is out and the newer one is not"
    );
    assert_eq!(
        tracker.totals(1_899),
        Totals::new(60, 30),
        "and it survives until it too is a full window old"
    );
    assert_eq!(
        tracker.totals(1_900),
        Totals::ZERO,
        "once every record is older than the window the total is empty again"
    );
}

#[test]
fn a_fuller_window_frees_up_as_records_age_out() {
    // The point of the window: capacity returns without an operator doing anything. A lifetime ceiling
    // would leave this tracker exhausted forever.
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(0, 100, 50));
    assert_eq!(tracker.remaining(0), Totals::ZERO);
    assert_eq!(
        tracker.record(Usage::new(500, 1, 1)),
        Verdict::BothExceeded {
            input: 101,
            output: 51
        }
    );

    assert_eq!(
        tracker.remaining(1_500),
        Totals::new(100, 50),
        "a full window later, the spend is forgiven"
    );
    assert_eq!(tracker.record(Usage::new(1_500, 100, 50)), Verdict::Within);
}

#[test]
fn every_expired_record_is_dropped_not_just_the_oldest() {
    // A prune that popped only the front would leave a record behind whenever timestamps arrive out of
    // order, and the memory bound would be a lie.
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(100, 1, 1));
    let _ = tracker.record(Usage::new(0, 1, 1));
    let _ = tracker.record(Usage::new(900, 1, 1));

    let _ = tracker.record(Usage::new(2_000, 2, 2));
    assert_eq!(
        tracker.records(),
        1,
        "all three stale records are removed, despite the second being newer than the first"
    );
    assert_eq!(tracker.totals(2_000), Totals::new(2, 2));
}

#[test]
fn a_window_of_zero_accumulates_forever() {
    // A zero window is a configuration, not an error: a process whose lifetime is the billing period wants
    // a running total with no expiry.
    let mut tracker = Tracker::new(Quota::new(0, u64::MAX, u64::MAX));

    let _ = tracker.record(Usage::new(0, 10, 5));
    let _ = tracker.record(Usage::new(1_000_000, 20, 10));

    assert_eq!(
        tracker.totals(u64::MAX),
        Totals::new(30, 15),
        "no amount of elapsed time expires a record"
    );
    assert_eq!(tracker.records(), 2, "and nothing is discarded");
    assert!(!tracker.quota().is_windowed());
}

#[test]
fn a_window_of_zero_still_enforces_its_ceilings() {
    // Accumulating is not the same as unbounded: the ceilings apply to the running total.
    let mut tracker = Tracker::new(Quota::new(0, 20, u64::MAX));

    assert_eq!(tracker.record(Usage::new(0, 20, 0)), Verdict::Within);
    assert_eq!(
        tracker.record(Usage::new(10_000_000, 1, 0)),
        Verdict::InputExceeded { used: 21, max: 20 },
        "a day later, the earlier spend still counts"
    );
}

#[test]
fn a_clock_that_steps_backwards_does_not_expire_a_record() {
    // Wall clocks step. A saturating subtraction keeps a younger-than-the-window record alive rather than
    // panicking or resurrecting it as ancient.
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(1_000, 7, 3));
    assert_eq!(tracker.totals(500), Totals::new(7, 3));
}

// ---- remaining ---------------------------------------------------------------------------------

#[test]
fn remaining_is_the_ceiling_minus_the_live_total() {
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(0, 30, 20));
    let _ = tracker.record(Usage::new(100, 50, 10));

    assert_eq!(tracker.totals(200), Totals::new(80, 30));
    assert_eq!(tracker.remaining(200), Totals::new(20, 20));
}

#[test]
fn remaining_follows_the_window() {
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(0, 30, 20));
    assert_eq!(tracker.remaining(0), Totals::new(70, 30));
    assert_eq!(
        tracker.remaining(1_000),
        Totals::new(100, 50),
        "once the only record has aged out, the whole quota is available again"
    );
}

#[test]
fn remaining_saturates_instead_of_wrapping() {
    let mut tracker = Tracker::new(one_second());

    let _ = tracker.record(Usage::new(0, 1_000, 1_000));
    assert_eq!(
        tracker.remaining(0),
        Totals::ZERO,
        "an overshoot reports zero capacity, never a wrapped huge number"
    );
}

#[test]
fn an_unlimited_quota_never_refuses() {
    let mut tracker = Tracker::new(Quota::unlimited());

    for step in 0..10 {
        assert_eq!(
            tracker.record(Usage::new(step * 1_000, 1_000_000, 1_000_000)),
            Verdict::Within
        );
    }
    assert_eq!(tracker.totals(0), Totals::new(10_000_000, 10_000_000));
    assert_eq!(
        tracker.remaining(0),
        Totals::new(u64::MAX - 10_000_000, u64::MAX - 10_000_000)
    );
}

// ---- reporting ---------------------------------------------------------------------------------

#[test]
fn a_verdict_renders_the_numbers_a_reader_needs() {
    assert_eq!(Verdict::Within.to_string(), "within quota");
    assert_eq!(
        Verdict::InputExceeded {
            used: 101,
            max: 100
        }
        .to_string(),
        "input tokens over quota (101 of 100)"
    );
    assert_eq!(
        Verdict::OutputExceeded { used: 51, max: 50 }.to_string(),
        "output tokens over quota (51 of 50)"
    );
    assert_eq!(
        Verdict::BothExceeded {
            input: 150,
            output: 60
        }
        .to_string(),
        "both token counts over quota (150 in, 60 out)"
    );
}

#[test]
fn a_default_tracker_is_permissive() {
    // A deployment that does not charge must not acquire a ceiling because a field was omitted.
    let mut tracker = Tracker::default();

    assert_eq!(tracker.quota(), Quota::unlimited());
    assert_eq!(
        tracker.record(Usage::new(0, u64::MAX, u64::MAX)),
        Verdict::Within
    );
}
