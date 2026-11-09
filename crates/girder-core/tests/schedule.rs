//! Tests for schedules (M4 of the v2 plan).
//!
//! Every rule is tested by computing the next instant rather than by waiting, which is the reason
//! the module separates "when would this fire" from "sleep until then".

use std::time::Duration;

use girder_core::{
    CronExpression, MIN_INTERVAL, Offset, Rule, ScheduleError, ScheduleId, TimeOfDay,
    create_schedule, next_fire, pause_schedule, resume_schedule,
};

/// 2026-01-01T00:00:00Z.
const EPOCH_2026: u64 = 1_767_225_600_000;
const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

fn id() -> ScheduleId {
    ScheduleId::new("s1").expect("valid")
}

fn at_time(hour: u32, minute: u32) -> TimeOfDay {
    TimeOfDay::new(hour, minute, 0).expect("valid time")
}

// ---- one-shot ---------------------------------------------------------------------------------

#[test]
fn an_after_rule_fires_once_after_its_delay() {
    let rule = Rule::after(Duration::from_secs(90), EPOCH_2026);

    assert_eq!(rule.next_after(EPOCH_2026).unwrap(), EPOCH_2026 + 90_000);
}

#[test]
fn an_at_rule_fires_at_its_instant() {
    let rule = Rule::At {
        at: EPOCH_2026 + DAY,
    };

    assert_eq!(rule.next_after(EPOCH_2026).unwrap(), EPOCH_2026 + DAY);
}

#[test]
fn a_passed_at_rule_is_exhausted() {
    // A one-shot whose instant has gone must not fire again; a caller that saw a value would
    // schedule it immediately and loop.
    let rule = Rule::At { at: EPOCH_2026 };

    assert!(matches!(
        rule.next_after(EPOCH_2026),
        Err(ScheduleError::Exhausted)
    ));
}

#[test]
fn one_shot_rules_report_themselves() {
    assert!(Rule::At { at: 0 }.is_one_shot());
    assert!(Rule::after(Duration::from_secs(1), 0).is_one_shot());
    assert!(!Rule::daily(at_time(9, 0), Offset::UTC).is_one_shot());
}

// ---- fixed rate -------------------------------------------------------------------------------

#[test]
fn an_every_rule_aligns_to_its_anchor() {
    // Aligned rather than "interval from now": a schedule created at 09:07 with a ten-minute
    // interval fires at :17, :27, not at fixed offsets from creation that drift.
    let rule = Rule::every(Duration::from_secs(600), EPOCH_2026).unwrap();

    assert_eq!(rule.next_after(EPOCH_2026).unwrap(), EPOCH_2026 + 600_000);
    assert_eq!(
        rule.next_after(EPOCH_2026 + 600_000).unwrap(),
        EPOCH_2026 + 1_200_000
    );
    // Part-way through a period, the next boundary is still aligned.
    assert_eq!(
        rule.next_after(EPOCH_2026 + 100_000).unwrap(),
        EPOCH_2026 + 600_000
    );
}

#[test]
fn an_interval_below_one_minute_is_refused() {
    // A per-second reminder fires 86,400 times a day, which nobody wants even when they ask.
    let error = Rule::every(Duration::from_secs(1), EPOCH_2026).expect_err("too short");

    assert!(matches!(error, ScheduleError::IntervalTooShort));
}

#[test]
fn the_minimum_interval_is_accepted() {
    assert!(Rule::every(MIN_INTERVAL, EPOCH_2026).is_ok());
}

#[test]
fn an_every_rule_before_its_anchor_fires_at_the_anchor() {
    let rule = Rule::every(Duration::from_secs(600), EPOCH_2026 + HOUR).unwrap();

    assert_eq!(rule.next_after(EPOCH_2026).unwrap(), EPOCH_2026 + HOUR);
}

#[test]
fn a_far_future_occurrence_is_computed_without_iterating() {
    // Ten years of hourly intervals would be 87,600 loop iterations; the arithmetic is direct.
    let rule = Rule::every(Duration::from_secs(3600), 0).unwrap();
    let ten_years = 10 * 365 * DAY;

    let next = rule.next_after(ten_years).unwrap();

    assert!(next > ten_years);
    assert_eq!((next - ten_years) / 1000, 3600, "the next hour boundary");
}

// ---- daily ------------------------------------------------------------------------------------

#[test]
fn a_daily_rule_fires_at_the_local_time() {
    let rule = Rule::daily(at_time(9, 30), Offset::UTC);

    // 2026-01-01T00:00Z -> the same day at 09:30.
    assert_eq!(
        rule.next_after(EPOCH_2026).unwrap(),
        EPOCH_2026 + 9 * HOUR + 30 * MINUTE
    );
}

#[test]
fn a_daily_rule_past_its_time_rolls_to_tomorrow() {
    let rule = Rule::daily(at_time(9, 0), Offset::UTC);
    let after = EPOCH_2026 + 10 * HOUR;

    assert_eq!(rule.next_after(after).unwrap(), EPOCH_2026 + DAY + 9 * HOUR);
}

#[test]
fn a_daily_rule_respects_its_offset() {
    // 09:00 in UTC+8 is 01:00 UTC.
    let rule = Rule::daily(at_time(9, 0), Offset::from_hours(8));

    assert_eq!(rule.next_after(EPOCH_2026).unwrap(), EPOCH_2026 + HOUR);
}

#[test]
fn a_daily_rule_is_stable_across_many_days() {
    // Each day's occurrence is exactly one day after the previous, in UTC, when the offset is fixed.
    let rule = Rule::daily(at_time(9, 0), Offset::UTC);
    let mut cursor = EPOCH_2026;

    let mut previous = None;
    for _ in 0..5 {
        let next = rule.next_after(cursor).unwrap();
        if let Some(previous) = previous {
            assert_eq!(next - previous, DAY);
        }
        previous = Some(next);
        cursor = next;
    }
}

// ---- weekly -----------------------------------------------------------------------------------

#[test]
fn a_weekly_rule_fires_only_on_its_weekdays() {
    // 2026-01-01 is a Thursday (ISO 4).
    let rule = Rule::weekly(at_time(9, 0), vec![1], Offset::UTC).unwrap();

    let next = rule.next_after(EPOCH_2026).unwrap();

    // The next Monday is 2026-01-05.
    assert_eq!(next, EPOCH_2026 + 4 * DAY + 9 * HOUR);
}

#[test]
fn a_weekly_rule_with_several_days_picks_the_nearest() {
    let rule = Rule::weekly(at_time(9, 0), vec![5, 4, 6], Offset::UTC).unwrap();

    // Thursday itself has not passed 09:00 yet at midnight.
    let next = rule.next_after(EPOCH_2026).unwrap();

    assert_eq!(next, EPOCH_2026 + 9 * HOUR, "Thursday is in the set");
}

#[test]
fn an_empty_weekday_set_is_refused() {
    let error = Rule::weekly(at_time(9, 0), vec![], Offset::UTC).expect_err("empty");

    assert!(matches!(error, ScheduleError::NoWeekdays));
}

#[test]
fn an_out_of_range_weekday_is_refused() {
    for day in [0, 8, 99] {
        let error = Rule::weekly(at_time(9, 0), vec![day], Offset::UTC).expect_err("invalid");
        assert!(matches!(error, ScheduleError::InvalidWeekday(_)), "{day}");
    }
}

#[test]
fn duplicate_weekdays_are_collapsed() {
    // Listing a weekday twice is a typo, not an intent to fire twice.
    let rule = Rule::weekly(at_time(9, 0), vec![1, 1, 1], Offset::UTC).unwrap();
    let Rule::Weekly { weekdays, .. } = rule else {
        panic!("expected weekly");
    };

    assert_eq!(weekdays, vec![1]);
}

#[test]
fn a_weekly_rule_returns_to_the_same_weekday() {
    let rule = Rule::weekly(at_time(9, 0), vec![3], Offset::UTC).unwrap();
    let first = rule.next_after(EPOCH_2026).unwrap();
    let second = rule.next_after(first).unwrap();

    assert_eq!(second - first, 7 * DAY);
}

// ---- cron -------------------------------------------------------------------------------------

#[test]
fn a_cron_expression_parses_five_fields() {
    let expression = CronExpression::parse("0 9 * * 1-5").expect("parses");

    assert_eq!(expression.source, "0 9 * * 1-5");
}

#[test]
fn a_cron_expression_needs_five_fields() {
    let error = CronExpression::parse("0 9 *").expect_err("too few");

    assert!(matches!(error, ScheduleError::CronFieldCount(3)));
}

#[test]
fn a_cron_field_outside_its_range_is_refused() {
    let error = CronExpression::parse("60 9 * * *").expect_err("minute 60");

    assert!(matches!(
        error,
        ScheduleError::InvalidCronField {
            field: "minute",
            ..
        }
    ));
}

#[test]
fn a_backwards_cron_range_is_refused() {
    let error = CronExpression::parse("0 17-9 * * *").expect_err("backwards");

    assert!(matches!(
        error,
        ScheduleError::InvalidCronField { field: "hour", .. }
    ));
}

#[test]
fn a_cron_step_of_zero_is_refused() {
    // A step of zero never advances and would loop forever.
    let error = CronExpression::parse("*/0 * * * *").expect_err("zero step");

    assert!(matches!(
        error,
        ScheduleError::InvalidCronField {
            field: "minute",
            ..
        }
    ));
}

#[test]
fn a_cron_rule_fires_at_the_matching_minute() {
    let expression = CronExpression::parse("30 9 * * *").unwrap();
    let rule = Rule::cron(expression, Offset::UTC);

    assert_eq!(
        rule.next_after(EPOCH_2026).unwrap(),
        EPOCH_2026 + 9 * HOUR + 30 * MINUTE
    );
}

#[test]
fn a_cron_rule_walks_to_the_next_matching_day() {
    // 2026-01-01 is a Thursday; the next Monday is the 5th.
    let expression = CronExpression::parse("0 9 * * 1").unwrap();
    let rule = Rule::cron(expression, Offset::UTC);

    assert_eq!(
        rule.next_after(EPOCH_2026).unwrap(),
        EPOCH_2026 + 4 * DAY + 9 * HOUR
    );
}

#[test]
fn a_cron_step_expression_matches_every_nth_minute() {
    let expression = CronExpression::parse("*/15 * * * *").unwrap();
    let rule = Rule::cron(expression, Offset::UTC);

    let first = rule.next_after(EPOCH_2026).unwrap();
    let second = rule.next_after(first).unwrap();

    // Strictly after: 00:00 itself is not "after" 00:00, so the first match is 00:15.
    assert_eq!(first, EPOCH_2026 + 15 * MINUTE);
    assert_eq!(second - first, 15 * MINUTE);
}

#[test]
fn a_cron_list_matches_each_value() {
    let expression = CronExpression::parse("0 9,17 * * *").unwrap();
    let rule = Rule::cron(expression, Offset::UTC);

    let morning = rule.next_after(EPOCH_2026).unwrap();
    let evening = rule.next_after(morning).unwrap();

    assert_eq!(morning, EPOCH_2026 + 9 * HOUR);
    assert_eq!(evening, EPOCH_2026 + 17 * HOUR);
}

#[test]
fn an_impossible_cron_expression_reports_exhaustion() {
    // February 30th never happens. An unbounded search would loop forever.
    let expression = CronExpression::parse("0 0 30 2 *").unwrap();
    let rule = Rule::cron(expression, Offset::UTC);

    assert!(matches!(
        rule.next_after(EPOCH_2026),
        Err(ScheduleError::Exhausted)
    ));
}

#[test]
fn cron_weekday_zero_and_seven_both_mean_sunday() {
    // Vixie cron accepts both; treating 0 as invalid would reject valid expressions.
    let zero = CronExpression::parse("0 9 * * 0").expect("0 parses");
    let seven = CronExpression::parse("0 9 * * 7").expect("7 parses");
    let rule_zero = Rule::cron(zero, Offset::UTC);
    let rule_seven = Rule::cron(seven, Offset::UTC);

    assert_eq!(
        rule_zero.next_after(EPOCH_2026).unwrap(),
        rule_seven.next_after(EPOCH_2026).unwrap()
    );
}

#[test]
fn a_cron_rule_fires_once_per_matching_minute_not_per_second() {
    // Granularity is a minute: an occurrence exactly at `after` is not after it.
    let expression = CronExpression::parse("* * * * *").unwrap();
    let rule = Rule::cron(expression, Offset::UTC);

    let next = rule.next_after(EPOCH_2026).unwrap();
    assert_eq!(next, EPOCH_2026 + MINUTE);
}

// ---- creation ---------------------------------------------------------------------------------

#[test]
fn a_schedule_trims_its_title() {
    let schedule = create_schedule(
        id(),
        "  morning  ",
        "say hi",
        Rule::daily(at_time(9, 0), Offset::UTC),
    )
    .expect("created");

    assert_eq!(schedule.title, "morning");
}

#[test]
fn a_blank_title_is_refused() {
    let error = create_schedule(id(), "   ", "p", Rule::daily(at_time(9, 0), Offset::UTC))
        .expect_err("blank title");

    assert!(matches!(error, girder_core::CreateError::InvalidTitle));
}

#[test]
fn an_overlong_title_is_refused() {
    let long = "x".repeat(121);
    let error = create_schedule(id(), &long, "p", Rule::daily(at_time(9, 0), Offset::UTC))
        .expect_err("too long");

    assert!(matches!(error, girder_core::CreateError::InvalidTitle));
}

#[test]
fn the_title_bound_counts_characters_not_bytes() {
    // A CJK title of 120 characters is 360 bytes; a byte bound would cut it to a third.
    let cjk = "字".repeat(120);
    let schedule = create_schedule(id(), &cjk, "p", Rule::daily(at_time(9, 0), Offset::UTC))
        .expect("exactly at the bound");

    assert_eq!(schedule.title.chars().count(), 120);
}

#[test]
fn a_blank_prompt_is_refused() {
    let error = create_schedule(id(), "t", "  ", Rule::daily(at_time(9, 0), Offset::UTC))
        .expect_err("blank prompt");

    assert!(matches!(error, girder_core::CreateError::EmptyPrompt));
}

// ---- lifecycle --------------------------------------------------------------------------------

#[test]
fn a_paused_schedule_does_not_fire() {
    let mut schedule =
        create_schedule(id(), "t", "p", Rule::daily(at_time(9, 0), Offset::UTC)).unwrap();
    pause_schedule(&mut schedule);

    assert!(next_fire(&schedule, EPOCH_2026).is_none());
}

#[test]
fn resuming_makes_it_fire_again() {
    let mut schedule =
        create_schedule(id(), "t", "p", Rule::daily(at_time(9, 0), Offset::UTC)).unwrap();
    pause_schedule(&mut schedule);
    resume_schedule(&mut schedule);

    assert!(next_fire(&schedule, EPOCH_2026).is_some());
}

#[test]
fn an_exhausted_one_shot_reports_no_next_fire() {
    let schedule = create_schedule(id(), "t", "p", Rule::At { at: EPOCH_2026 }).unwrap();

    assert!(next_fire(&schedule, EPOCH_2026 + 1).is_none());
}

// ---- time of day ------------------------------------------------------------------------------

#[test]
fn a_time_of_day_rejects_out_of_range_fields() {
    assert!(TimeOfDay::new(24, 0, 0).is_none());
    assert!(TimeOfDay::new(0, 60, 0).is_none());
    assert!(TimeOfDay::new(0, 0, 60).is_none());
    assert!(TimeOfDay::new(23, 59, 59).is_some());
}

#[test]
fn a_time_of_day_computes_its_offset_from_midnight() {
    assert_eq!(at_time(0, 0).since_midnight(), 0);
    assert_eq!(at_time(1, 0).since_midnight(), HOUR);
    assert_eq!(at_time(0, 1).since_midnight(), MINUTE);
}
