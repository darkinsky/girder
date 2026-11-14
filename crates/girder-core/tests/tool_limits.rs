//! Tests for the per-tool call budget.
//!
//! The failure modes under test are the ones that separate a useful budget from a blunt one: a total across tools
//! would stop a model that is reading files, a failure count that never reset would stop a tool that is recovering,
//! and a counter that reset on success without clearing the run would allow an unbounded retry loop. Each case is
//! pinned here, along with the wording the model is handed, because a refusal it cannot act on is a refusal it will
//! retry.
//!
//! The module is included by path so that the test runs on this file alone, before or independently of the crate
//! root's wiring. The module imports nothing from the crate, so no shims are needed. Once the wiring is settled, the
//! `#[path]` line can become `use girder_core::{ToolLimit, ToolTracker, ToolVerdict};`, which is what the crate root
//! exports today.

#[path = "../src/tool_limits.rs"]
mod tool_limits;

use tool_limits::{Limit, Tracker, Verdict};

/// A tracker whose calls are all allowed, so a test can watch the counting without tripping a ceiling.
fn permissive() -> Tracker {
    Tracker::new(Limit::new(100, 100))
}

#[test]
fn calls_within_the_limit_are_allowed() {
    let mut tracker = Tracker::new(Limit::new(3, 5));

    for expected in 1..=3 {
        assert_eq!(tracker.record("bash", true), Verdict::Allow);
        assert_eq!(tracker.calls("bash"), expected);
    }
    assert_eq!(tracker.consecutive_failures("bash"), 0);
}

#[test]
fn the_call_after_the_limit_is_refused() {
    let mut tracker = Tracker::new(Limit::new(3, 5));
    for _ in 0..3 {
        assert_eq!(tracker.record("bash", true), Verdict::Allow);
    }

    assert_eq!(
        tracker.record("bash", true),
        Verdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 3,
        }
    );
}

#[test]
fn a_refused_call_is_still_counted() {
    // The count answers "how many times did the model ask", which is the fact a diagnosis needs; a count that froze
    // at the ceiling would report fifty attempts as four.
    let mut tracker = Tracker::new(Limit::new(1, 5));
    assert_eq!(tracker.record("bash", true), Verdict::Allow);
    for attempt in 2..=6 {
        assert!(matches!(
            tracker.record("bash", true),
            Verdict::TooManyCalls { .. }
        ));
        assert_eq!(tracker.calls("bash"), attempt);
    }
}

#[test]
fn counts_are_per_tool() {
    // The core property: forty reads are a model doing its job, forty shells is a model thrashing.
    let mut tracker = Tracker::new(Limit::new(40, 5));

    for _ in 0..40 {
        assert_eq!(tracker.record("read", true), Verdict::Allow);
    }
    assert_eq!(tracker.calls("read"), 40);
    assert_eq!(
        tracker.record("read", true),
        Verdict::TooManyCalls {
            tool: "read".to_owned(),
            limit: 40,
        }
    );

    // A different tool has spent nothing, whatever its neighbour did.
    assert_eq!(tracker.calls("bash"), 0);
    for expected in 1..=40 {
        assert_eq!(tracker.record("bash", true), Verdict::Allow);
        assert_eq!(tracker.calls("bash"), expected);
    }
}

#[test]
fn failures_of_one_tool_do_not_stop_another() {
    let mut tracker = Tracker::new(Limit::new(10, 3));

    for _ in 0..3 {
        let _ = tracker.record("bash", false);
    }
    assert!(matches!(
        tracker.record("bash", false),
        Verdict::TooManyFailures { .. }
    ));

    assert_eq!(tracker.record("read", true), Verdict::Allow);
    assert_eq!(tracker.consecutive_failures("read"), 0);
}

#[test]
fn consecutive_failures_are_refused() {
    let mut tracker = Tracker::new(Limit::new(10, 3));

    for expected in 1..=3 {
        assert_eq!(tracker.record("bash", false), Verdict::Allow);
        assert_eq!(tracker.consecutive_failures("bash"), expected);
    }

    assert_eq!(
        tracker.record("bash", false),
        Verdict::TooManyFailures {
            tool: "bash".to_owned(),
            failures: 4,
            limit: 3,
        }
    );
}

#[test]
fn a_success_clears_the_failure_run() {
    // Failing five, succeeding, failing five again is a tool that is working. Nothing about the first run may leak
    // into the second.
    let mut tracker = Tracker::new(Limit::new(50, 3));

    for _ in 0..3 {
        assert_eq!(tracker.record("bash", false), Verdict::Allow);
    }
    assert_eq!(tracker.record("bash", true), Verdict::Allow);
    assert_eq!(tracker.consecutive_failures("bash"), 0);

    for expected in 1..=3 {
        assert_eq!(tracker.record("bash", false), Verdict::Allow);
        assert_eq!(tracker.consecutive_failures("bash"), expected);
    }

    // The call count still remembers the whole turn: the reset is of the run, not of the budget.
    assert_eq!(tracker.calls("bash"), 7);
}

#[test]
fn reset_clears_every_tool() {
    let mut tracker = Tracker::new(Limit::new(2, 2));

    for _ in 0..2 {
        assert_eq!(tracker.record("read", true), Verdict::Allow);
        let _ = tracker.record("bash", false);
    }
    assert!(matches!(
        tracker.record("read", true),
        Verdict::TooManyCalls { .. }
    ));

    tracker.reset();

    assert_eq!(tracker.calls("read"), 0);
    assert_eq!(tracker.calls("bash"), 0);
    assert_eq!(tracker.consecutive_failures("bash"), 0);
    // The next turn starts with full budget, not with the previous turn's spent amount.
    for _ in 0..2 {
        assert_eq!(tracker.record("read", true), Verdict::Allow);
    }
}

#[test]
fn reset_does_not_change_the_limits() {
    let mut tracker = Tracker::new(Limit::new(1, 1));
    tracker.reset();
    tracker.reset();

    assert_eq!(tracker.record("bash", true), Verdict::Allow);
    assert!(matches!(
        tracker.record("bash", true),
        Verdict::TooManyCalls { .. }
    ));
}

#[test]
fn a_zero_call_ceiling_refuses_the_first_call() {
    // `0` is a hard stop, not an "off" switch. Reading it as unlimited would invert a deliberate configuration.
    let mut tracker = Tracker::new(Limit::new(0, 5));
    assert_eq!(
        tracker.record("bash", true),
        Verdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 0,
        }
    );
}

#[test]
fn a_zero_failure_ceiling_still_allows_a_success() {
    // `0 > 0` is false, so a success can never trip the failure ceiling, whatever that ceiling is set to.
    let mut tracker = Tracker::new(Limit::new(5, 0));
    assert_eq!(tracker.record("bash", true), Verdict::Allow);
    assert_eq!(
        tracker.record("bash", false),
        Verdict::TooManyFailures {
            tool: "bash".to_owned(),
            failures: 1,
            limit: 0,
        }
    );
}

#[test]
fn the_call_ceiling_is_reported_before_the_failure_ceiling() {
    // A model told both limits has to guess which one it is against; naming one lets it act on the message.
    let mut tracker = Tracker::new(Limit::new(1, 1));
    assert_eq!(tracker.record("bash", false), Verdict::Allow);

    assert_eq!(
        tracker.record("bash", true),
        Verdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 1,
        }
    );
}

#[test]
fn an_unknown_tool_has_spent_nothing() {
    let tracker = permissive();
    assert_eq!(tracker.calls("bash"), 0);
    assert_eq!(tracker.consecutive_failures("bash"), 0);
}

#[test]
fn unlimited_never_refuses_but_still_counts() {
    let mut tracker = Tracker::new(Limit::unlimited());
    for _ in 0..50 {
        assert_eq!(tracker.record("bash", false), Verdict::Allow);
    }
    // Reporting still works: a deployment that only watches must be able to say what it saw.
    assert_eq!(tracker.calls("bash"), 50);
    assert_eq!(tracker.consecutive_failures("bash"), 50);
}

#[test]
fn explain_tells_the_model_what_to_do_instead() {
    let too_many = Verdict::TooManyCalls {
        tool: "bash".to_owned(),
        limit: 40,
    };
    let text = too_many.explain();
    assert!(text.contains("bash"), "{text}");
    assert!(text.contains("40"), "{text}");
    assert!(
        text.contains("another way") || text.contains("different tool"),
        "a refusal must name an alternative, got: {text}"
    );

    let failing = Verdict::TooManyFailures {
        tool: "bash".to_owned(),
        failures: 6,
        limit: 5,
    };
    let text = failing.explain();
    assert!(text.contains("bash"), "{text}");
    assert!(text.contains('6') && text.contains('5'), "{text}");
    assert!(
        text.contains("different tool") || text.contains("blocking you"),
        "a refusal must name an alternative, got: {text}"
    );
}

#[test]
fn verdicts_describe_themselves() {
    assert!(!Verdict::Allow.explain().is_empty());
    assert!(Verdict::Allow.is_allow());
    assert!(
        !Verdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 1,
        }
        .is_allow()
    );

    // `Display` exists so a log line needs no `match`; it must name the tool and the ceiling.
    let text = Verdict::TooManyCalls {
        tool: "bash".to_owned(),
        limit: 1,
    }
    .to_string();
    assert!(text.contains("bash") && text.contains('1'), "{text}");
}

#[test]
fn separate_trackers_do_not_share_state() {
    // Two agents in one session each get their own turn budget.
    let mut first = Tracker::new(Limit::new(1, 5));
    let second = Tracker::new(Limit::new(1, 5));

    assert_eq!(first.record("bash", true), Verdict::Allow);
    assert!(matches!(
        first.record("bash", true),
        Verdict::TooManyCalls { .. }
    ));
    assert_eq!(second.calls("bash"), 0);
}
