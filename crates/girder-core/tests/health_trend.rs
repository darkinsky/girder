//! Tests for deployment-wide health trends.
//!
//! The properties pinned here are the ones the module exists for, in the order they matter:
//!
//! - **Two samples are the minimum** -- one sample says what is true now and cannot say whether that
//!   is a change, so [`analyze`] returns `None` rather than a trend of zeroes that a caller would
//!   display as measured stability.
//! - **A fall is identified as a fall** -- more sessions breaking in the later half must produce
//!   `degrading`, because that is the Monday-morning question the module was written for.
//! - **A rise is identified as a rise** -- and must not be reported as `degrading`, which is the
//!   failure a comparison with a reversed sign produces.
//! - **Ordinary churn is not a trend** -- a one-point movement in the health rate is inside the noise
//!   of a real window, and an alarm that fires every week is a line nobody reads.
//! - **The summary is readable** -- it has to carry both rates and both scores, because a bare
//!   `-0.13` is the thing it exists to replace.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It names the crate through
//! `::girder_core`, which the `extern crate self as girder_core` alias lets resolve from either side.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/health_trend.rs"]
mod health_trend;

use girder_core::{ContentBlock, SessionEvent, ToolCallId, TurnEndReason};

use health_trend::{HEALTH_RATE_DROP, SCORE_DROP, Sample, analyze, sample};

// ---- fixtures ----------------------------------------------------------------------------------

/// A session with one complete, answered turn: no issue, so it is healthy and scores 100.
fn good_session() -> Vec<SessionEvent> {
    let id = ToolCallId::new("call-1").expect("valid id");
    vec![
        SessionEvent::TurnStart { turn: 1 },
        step(1, 1),
        SessionEvent::ToolCall {
            id: id.clone(),
            name: "bash".to_owned(),
            arguments: "{}".to_owned(),
        },
        SessionEvent::ToolResult {
            id,
            content: vec![ContentBlock::Text {
                text: "ok".to_owned(),
            }],
            is_error: false,
        },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ]
}

/// A session with an orphan tool result. Nothing normal produces that shape, so it is one
/// `Error`-severity issue: unhealthy, and scored 100 - `ERROR_PENALTY` = 75.
fn errored_session() -> Vec<SessionEvent> {
    vec![
        SessionEvent::TurnStart { turn: 1 },
        step(1, 1),
        SessionEvent::ToolResult {
            id: ToolCallId::new("ghost").expect("valid id"),
            content: vec![ContentBlock::Text {
                text: "orphan".to_owned(),
            }],
            is_error: false,
        },
    ]
}

/// A session with a turn left open at the end of the log. That is what a live conversation looks
/// like, so it is healthy and scores 100.
fn live_session() -> Vec<SessionEvent> {
    vec![SessionEvent::TurnStart { turn: 1 }, step(1, 1)]
}

/// A `step/start` record.
fn step(turn: u64, step: u64) -> SessionEvent {
    SessionEvent::StepStart { turn, step }
}

/// A sample at `at_ms` whose counts and mean score are given directly.
///
/// Used by the `analyze` tests, which are about the comparison and not about how a window was
/// reduced; `sample` has its own tests below.
fn sample_of(at_ms: u64, sessions: usize, healthy: usize, errors: usize, mean: f64) -> Sample {
    Sample {
        at_ms,
        sessions,
        healthy,
        errors,
        mean_score: mean,
    }
}

/// `count` copies of the given session log.
fn batch(count: usize, session: fn() -> Vec<SessionEvent>) -> Vec<Vec<SessionEvent>> {
    (0..count).map(|_| session()).collect()
}

/// Whether two floats agree closely enough for a test about a formula rather than about rounding.
fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-9
}

// ---- analyze: how many samples are enough ------------------------------------------------------

#[test]
fn one_sample_has_no_trend() {
    // The core boundary: one sample says what is true now, not whether it changed. A `Trend` of
    // zeroes would be displayed by a caller as a measurement of stability that nobody made.
    assert!(analyze(&[sample_of(0, 10, 9, 0, 90.0)]).is_none());
}

#[test]
fn no_samples_have_no_trend() {
    assert!(analyze(&[]).is_none());
}

#[test]
fn two_samples_have_a_trend() {
    // The core case: two samples are the minimum that can express a change, and the answer must be a
    // real comparison rather than `None`.
    let earlier = sample_of(0, 100, 95, 1, 95.0);
    let later = sample_of(1_000, 100, 82, 4, 82.0);

    let trend = analyze(&[earlier, later]).expect("two samples are enough");

    // 95% -> 82% is -0.13 as a fraction, and the score fell by the same 13 points.
    assert!(close(trend.health_rate_delta, -0.13));
    assert!(close(trend.mean_score_delta, -13.0));
    assert!(trend.degrading);
}

#[test]
fn samples_are_ordered_by_time_not_by_position() {
    // A caller that assembled its windows out of order still gets "before" and "after" from the
    // timestamps. Without the sort this pair would read as an improvement.
    let later = sample_of(2_000, 100, 82, 4, 82.0);
    let earlier = sample_of(1_000, 100, 95, 1, 95.0);

    let trend = analyze(&[later, earlier]).expect("two samples are enough");

    assert!(close(trend.health_rate_delta, -0.13));
    assert!(trend.degrading);
}

// ---- analyze: the verdicts ---------------------------------------------------------------------

#[test]
fn a_falling_health_rate_is_degrading() {
    // 96% -> 70%: twenty-six points down, well past the noise floor.
    let trend = analyze(&[
        sample_of(0, 200, 192, 1, 96.0),
        sample_of(1, 200, 140, 12, 70.0),
    ])
    .expect("two samples are enough");

    assert!(trend.degrading);
    assert!(trend.health_rate_delta < 0.0);
    assert!(trend.summary.contains("degrading"));
}

#[test]
fn a_falling_score_alone_is_degrading() {
    // The rate is unchanged, so the same fraction of sessions is broken -- but each broken one fails
    // harder. This is why the verdict reads the score as well as the rate.
    let trend = analyze(&[
        sample_of(0, 100, 90, 10, 90.0),
        sample_of(1, 100, 90, 10, 70.0),
    ])
    .expect("two samples are enough");

    assert!(close(trend.health_rate_delta, 0.0));
    assert!(close(trend.mean_score_delta, -20.0));
    assert!(trend.degrading);
}

#[test]
fn a_rising_health_rate_is_improving() {
    // The mirror of the degrading case, and not a `degrading` flag with the sign flipped: an
    // implementation that compared the halves backwards reports this as the alarm.
    let trend = analyze(&[
        sample_of(0, 100, 70, 12, 70.0),
        sample_of(1, 100, 95, 1, 95.0),
    ])
    .expect("two samples are enough");

    assert!(!trend.degrading);
    assert!(close(trend.health_rate_delta, 0.25));
    assert!(trend.summary.contains("improving"));
}

#[test]
fn ordinary_churn_is_not_reported_as_degrading() {
    // A point of health rate and four points of score is the churn of a real week: a different mix of
    // workloads, one session that stayed open across the boundary. Alarming on it would make the
    // alarm meaningless.
    let trend = analyze(&[
        sample_of(0, 500, 470, 5, 94.0),
        sample_of(1, 500, 465, 6, 90.0),
    ])
    .expect("two samples are enough");

    assert!(!trend.degrading);
    assert!(trend.summary.contains("stable"));
}

#[test]
fn a_drop_exactly_at_the_threshold_is_degrading() {
    // Pinned at the boundary in the inclusive direction, so the constant in the docs is the constant
    // the code applies. 100% -> 100% - HEALTH_RATE_DROP is exactly one floor.
    let earlier = sample_of(0, 100, 100, 0, 100.0);
    // The floor expressed in sessions out of a hundred. The cast is deliberate for a fixture: the
    // constant is a small exact literal, and the assertion below pins that it still is one, so a
    // truncating conversion cannot quietly move the boundary this test is about.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let drop_points = (HEALTH_RATE_DROP * 100.0).round() as usize;
    assert!(drop_points >= 1, "a floor below one point is not a test");
    let healthy_after = 100 - drop_points;
    let later = sample_of(1, 100, healthy_after, 1, 100.0);

    let trend = analyze(&[earlier, later]).expect("two samples are enough");

    assert!(trend.health_rate_delta <= -HEALTH_RATE_DROP);
    assert!(trend.degrading);
}

#[test]
fn a_drop_just_under_the_threshold_is_not_degrading() {
    // The negative control for the test above: one point less than the rate floor, with a score
    // change under the score floor, must leave the verdict alone.
    let earlier = sample_of(0, 100, 100, 0, 100.0);
    let later = sample_of(1, 100, 99, 1, 100.0 - (SCORE_DROP - 1.0));

    let trend = analyze(&[earlier, later]).expect("two samples are enough");

    assert!(!trend.degrading);
}

// ---- analyze: the halves -----------------------------------------------------------------------

#[test]
fn both_halves_are_pooled_by_session_count() {
    // Four samples split two and two; the later half is pooled by sessions, so the 95-of-100 window
    // outweighs the 5-of-10 one instead of averaging their rates to the same weight.
    let samples = [
        sample_of(0, 100, 95, 1, 95.0),
        sample_of(1, 100, 95, 1, 95.0),
        sample_of(2, 100, 95, 1, 95.0),
        sample_of(3, 10, 5, 2, 50.0),
    ];

    let trend = analyze(&samples).expect("four samples are enough");

    // Earlier half: 190/200 = 95%. Later half: 100/110 = 90.909...%, not the 72.5% an unweighted
    // average of the two per-window rates would give.
    let expected = 100.0 / 110.0 - 190.0 / 200.0;
    assert!(close(trend.health_rate_delta, expected));
    // The second window is small and scored 50, which pulls the pooled score down by more than the
    // score floor even though the rate held up. That is the second signal doing its job, not a bug.
    assert!(trend.degrading);
}

#[test]
fn an_odd_sample_count_gives_the_extra_sample_to_the_later_half() {
    // Three samples split one and two: the boundary is `len / 2`, so the recent side is the larger
    // one -- the side a reader cares about most.
    let samples = [
        sample_of(0, 100, 100, 0, 100.0),
        sample_of(1, 100, 100, 0, 100.0),
        sample_of(2, 100, 100, 0, 100.0),
    ];

    let trend = analyze(&samples).expect("three samples are enough");

    assert!(close(trend.health_rate_delta, 0.0));
    assert!(close(trend.mean_score_delta, 0.0));
    assert!(!trend.degrading);
    // 100 before, 200 after.
    assert!(
        trend
            .summary
            .contains("over 100 sessions before and 200 after")
    );
}

#[test]
fn a_window_with_no_sessions_does_not_count_as_a_collapse() {
    // An idle window is not a failing window. Its `0.0` mean score must not be pooled in, or a quiet
    // hour reads as every session in it having scored zero.
    let samples = [
        sample_of(0, 100, 95, 1, 95.0),
        sample_of(1, 100, 95, 1, 95.0),
        sample_of(2, 0, 0, 0, 0.0),
        sample_of(3, 100, 96, 1, 96.0),
        sample_of(4, 100, 96, 1, 96.0),
    ];

    let trend = analyze(&samples).expect("four real samples are enough");

    assert!(!trend.degrading);
    assert!(close(trend.health_rate_delta, 0.01));
    // The idle window is gone from the count: 200 real sessions on each side, not 300 then 200.
    assert!(
        trend
            .summary
            .contains("over 200 sessions before and 200 after")
    );
}

#[test]
fn only_idle_windows_have_no_trend() {
    // With the idle windows dropped there is one sample left, which is not a comparison.
    let samples = [sample_of(0, 0, 0, 0, 0.0), sample_of(1, 0, 0, 0, 0.0)];

    assert!(analyze(&samples).is_none());
    assert!(analyze(&[sample_of(0, 0, 0, 0, 0.0), sample_of(1, 10, 10, 0, 100.0)]).is_none());
}

// ---- analyze: the summary ----------------------------------------------------------------------

#[test]
fn the_summary_names_both_rates_and_both_scores() {
    // The point of the string: a reader gets the numbers it was computed from instead of a delta they
    // would have to go and interpret. `-0.13` does not say thirteen points, or from what.
    let trend = analyze(&[
        sample_of(0, 100, 95, 1, 95.0),
        sample_of(1, 100, 82, 4, 82.0),
    ])
    .expect("two samples are enough");

    assert!(trend.summary.contains("95.0%"), "{}", trend.summary);
    assert!(trend.summary.contains("82.0%"), "{}", trend.summary);
    assert!(trend.summary.contains("-13.0 points"), "{}", trend.summary);
    assert!(trend.summary.contains("95.0 -> 82.0"), "{}", trend.summary);
    assert!(trend.summary.contains("degrading"), "{}", trend.summary);
}

#[test]
fn a_stable_summary_says_so_in_words() {
    // Not just "not degrading": the sentence has to read as a verdict for a human, including when
    // the verdict is that nothing happened.
    let trend = analyze(&[
        sample_of(0, 100, 95, 1, 95.0),
        sample_of(1, 100, 95, 1, 95.0),
    ])
    .expect("two samples are enough");

    assert!(!trend.degrading);
    assert!(
        trend.summary.contains("stable within the noise floor"),
        "{}",
        trend.summary
    );
}

// ---- sample: reducing logs to one window -------------------------------------------------------

#[test]
fn an_empty_batch_is_a_sample_with_no_sessions() {
    // A representable but explicitly meaningless window: zero sessions and a `0.0` mean score, which
    // `analyze` drops rather than reading as total failure.
    let sampled = sample(&[], 42);

    assert_eq!(sampled.at_ms, 42);
    assert_eq!(sampled.sessions, 0);
    assert_eq!(sampled.healthy, 0);
    assert_eq!(sampled.errors, 0);
    assert!(close(sampled.mean_score, 0.0));
}

#[test]
fn the_health_rate_is_the_fraction_of_healthy_sessions() {
    // Three good, one broken: 75% healthy, and the broken one is the single error.
    let mut events = batch(3, good_session);
    events.push(errored_session());

    let sampled = sample(&events, 7);

    assert_eq!(sampled.at_ms, 7);
    assert_eq!(sampled.sessions, 4);
    assert_eq!(sampled.healthy, 3);
    assert_eq!(sampled.errors, 1);
    // Three sessions at 100 and one at 100 - 25.
    assert!(close(sampled.mean_score, 93.75), "{}", sampled.mean_score);
}

#[test]
fn a_live_session_counts_as_healthy() {
    // A session still in the middle of a turn is what a healthy running conversation looks like;
    // counting it as broken would make the health rate track how busy the deployment is.
    let events = batch(4, live_session);

    let sampled = sample(&events, 0);

    assert_eq!(sampled.healthy, 4);
    assert_eq!(sampled.errors, 0);
    assert!(close(sampled.mean_score, 100.0));
}

#[test]
fn the_healthy_count_agrees_with_the_per_session_verdict() {
    // `sample` applies the rule to the issues it already has instead of calling `is_healthy`, so the
    // two must be pinned to agree; otherwise a deployment's rate and its per-session sweep would
    // disagree about the same log.
    let events = vec![
        good_session(),
        errored_session(),
        live_session(),
        good_session(),
    ];

    let sampled = sample(&events, 0);
    let expected = events
        .iter()
        .filter(|session| girder_core::is_healthy(session))
        .count();

    assert_eq!(sampled.healthy, expected);
}

#[test]
fn errors_count_sessions_not_issues() {
    // Two orphan results in one log are one errored session, not two: the field is a count of
    // sessions a reader can go and open.
    let events = vec![
        vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::ToolResult {
                id: ToolCallId::new("ghost-a").expect("valid id"),
                content: Vec::new(),
                is_error: false,
            },
            SessionEvent::ToolResult {
                id: ToolCallId::new("ghost-b").expect("valid id"),
                content: Vec::new(),
                is_error: false,
            },
        ],
        good_session(),
    ];

    let sampled = sample(&events, 0);

    assert_eq!(sampled.sessions, 2);
    assert_eq!(sampled.errors, 1);
    assert_eq!(sampled.healthy, 1);
}

#[test]
fn two_windows_from_logs_report_a_real_trend() {
    // End to end: the shape the module was written for. A thousand sessions were fine last week; this
    // week a quarter of them have lost a tool result.
    let last_week: Vec<Vec<SessionEvent>> = batch(4, good_session);
    let this_week = vec![
        good_session(),
        good_session(),
        good_session(),
        errored_session(),
    ];

    let earlier = sample(&last_week, 0);
    let later = sample(&this_week, 1_000);

    let trend = analyze(&[earlier, later]).expect("two windows are enough");

    assert!(close(trend.health_rate_delta, -0.25));
    assert!(trend.degrading);
    assert!(trend.summary.contains("100.0%"), "{}", trend.summary);
    assert!(trend.summary.contains("75.0%"), "{}", trend.summary);
}
