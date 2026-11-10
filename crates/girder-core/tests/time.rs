//! Tests for the time vocabulary.
//!
//! The civil-calendar arithmetic is the part that cannot be checked by inspection: an off-by-one in
//! the era calculation produces dates that look plausible and are wrong by days.

use girder_core::{
    MAX_FOUR_DIGIT_YEAR, MIN_FOUR_DIGIT_YEAR, MS_PER_DAY, MS_PER_HOUR, Span, TimeError,
    TimeInstant as Instant, WallClock, civil_from_days, days_from_civil, days_in_month,
    format_instant, format_span, is_four_digit_year, is_leap_year, parse_instant,
};

/// 2026-01-01T00:00:00.000Z.
const EPOCH_2026: Instant = 1_767_225_600_000;

// ---- the calendar -----------------------------------------------------------------------------

#[test]
fn the_epoch_is_day_zero() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(civil_from_days(0), (1970, 1, 1));
}

#[test]
fn known_dates_round_trip() {
    // Each pair is a date whose weekday and order are independently known.
    let cases = [
        (1970, 1, 1),
        (2000, 2, 29),
        (2024, 2, 29),
        (2026, 1, 1),
        (2026, 12, 31),
        (1999, 12, 31),
    ];
    for (year, month, day) in cases {
        let days = days_from_civil(year, month, day);
        assert_eq!(
            civil_from_days(days),
            (year, month, day),
            "{year}-{month:02}-{day:02} did not round-trip"
        );
    }
}

#[test]
fn a_negative_day_index_is_before_the_epoch() {
    // The era calculation has a separate branch for negatives; a wrong one shifts every date before
    // 1970 by 400 years.
    assert_eq!(civil_from_days(-1), (1969, 12, 31));
    assert_eq!(days_from_civil(1969, 12, 31), -1);
    assert_eq!(
        civil_from_days(-719_468),
        (0, 3, 1),
        "year zero is a real year"
    );
}

#[test]
fn consecutive_days_differ_by_one() {
    // A sampling check over a century: the arithmetic must be monotone with no gaps.
    let mut previous = days_from_civil(1926, 1, 1);
    for year in 1926..2026 {
        for month in 1..=12 {
            for day in 1..=days_in_month(year, month) {
                let current = days_from_civil(year, month, day);
                if year == 1926 && month == 1 && day == 1 {
                    previous = current;
                    continue;
                }
                assert_eq!(current, previous + 1, "{year}-{month}-{day}");
                previous = current;
            }
        }
    }
}

#[test]
fn leap_years_follow_the_gregorian_rule() {
    assert!(is_leap_year(2024));
    assert!(!is_leap_year(2026));
    assert!(!is_leap_year(1900), "a century is not a leap year");
    assert!(is_leap_year(2000), "a fourth century is");
}

#[test]
fn month_lengths_are_right() {
    assert_eq!(days_in_month(2026, 1), 31);
    assert_eq!(days_in_month(2026, 2), 28);
    assert_eq!(days_in_month(2024, 2), 29);
    assert_eq!(days_in_month(2026, 4), 30);
    assert_eq!(days_in_month(2026, 13), 0, "a thirteenth month has no days");
}

// ---- wall clock -------------------------------------------------------------------------------

#[test]
fn a_wall_clock_refuses_an_impossible_date() {
    // Refused where it was constructed, not where it was used.
    assert!(WallClock::new(2026, 2, 30, 0, 0, 0, 0).is_err());
    assert!(
        WallClock::new(2024, 2, 29, 0, 0, 0, 0).is_ok(),
        "2024 is a leap year"
    );
    assert!(WallClock::new(2026, 13, 1, 0, 0, 0, 0).is_err());
    assert!(WallClock::new(2026, 1, 1, 24, 0, 0, 0).is_err());
    assert!(WallClock::new(2026, 1, 1, 0, 60, 0, 0).is_err());
    assert!(WallClock::new(2026, 1, 1, 0, 0, 0, 1000).is_err());
}

#[test]
fn a_wall_clock_resolves_under_an_offset() {
    let wall = WallClock::new(2026, 1, 1, 0, 0, 0, 0).expect("valid");

    assert_eq!(wall.resolve(0), EPOCH_2026);
    // 09:00 in UTC+8 is 01:00 UTC.
    let nine = WallClock::new(2026, 1, 1, 9, 0, 0, 0).expect("valid");
    assert_eq!(nine.resolve(8 * MS_PER_HOUR), EPOCH_2026 + MS_PER_HOUR);
}

#[test]
fn a_wall_clock_round_trips_through_an_instant() {
    let wall = WallClock::new(2026, 6, 15, 13, 45, 30, 250).expect("valid");

    let instant = wall.resolve(0);
    let back = WallClock::from_instant(instant, 0);

    assert_eq!(back, wall);
}

#[test]
fn a_wall_clock_before_the_epoch_round_trips() {
    // The negative branch again, now through the instant conversion.
    let wall = WallClock::new(1960, 3, 1, 12, 0, 0, 0).expect("valid");

    let instant = wall.resolve(0);

    assert!(instant < 0);
    assert_eq!(WallClock::from_instant(instant, 0), wall);
}

// ---- canonical instants -----------------------------------------------------------------------

#[test]
fn an_instant_formats_canonically() {
    assert_eq!(
        format_instant(EPOCH_2026).expect("formats"),
        "2026-01-01T00:00:00.000Z"
    );
}

#[test]
fn a_canonical_instant_round_trips() {
    for instant in [EPOCH_2026, 0, -1, EPOCH_2026 + MS_PER_DAY] {
        let text = format_instant(instant).expect("formats");
        assert_eq!(parse_instant(&text).expect("parses"), instant, "{text}");
    }
}

#[test]
fn a_lenient_shape_is_refused() {
    // A parser that accepted these would accept strings a stricter peer rejects, and the difference
    // would appear as a value that round-trips locally and fails elsewhere.
    for text in [
        "2026-01-01T00:00:00Z",          // no milliseconds
        "2026-01-01 00:00:00.000Z",      // space instead of T
        "2026-01-01T00:00:00.000",       // no zone
        "2026-01-01T00:00:00.000+00:00", // explicit offset
        "26-01-01T00:00:00.000Z",        // two-digit year
    ] {
        assert!(parse_instant(text).is_err(), "{text} must be refused");
    }
}

#[test]
fn a_wrong_separator_is_refused() {
    // The separators are checked rather than skipped.
    assert!(parse_instant("2026x01x01T00:00:00.000Z").is_err());
    assert!(parse_instant("2026-01-01X00:00:00.000Z").is_err());
}

#[test]
fn an_impossible_date_is_refused_at_parse() {
    assert!(parse_instant("2026-02-30T00:00:00.000Z").is_err());
}

#[test]
fn the_four_digit_year_bounds_hold() {
    assert!(is_four_digit_year(MIN_FOUR_DIGIT_YEAR));
    assert!(is_four_digit_year(MAX_FOUR_DIGIT_YEAR));
    assert!(!is_four_digit_year(MIN_FOUR_DIGIT_YEAR - 1));
    assert!(!is_four_digit_year(MAX_FOUR_DIGIT_YEAR + 1));
}

#[test]
fn an_instant_outside_the_bounds_does_not_format() {
    let error = format_instant(MAX_FOUR_DIGIT_YEAR + 1).expect_err("out of range");

    assert!(matches!(error, TimeError::OutOfRange { .. }));
}

#[test]
fn the_bounds_format_as_the_documented_dates() {
    assert_eq!(
        format_instant(MIN_FOUR_DIGIT_YEAR).expect("formats"),
        "0001-01-01T00:00:00.000Z"
    );
    assert_eq!(
        format_instant(MAX_FOUR_DIGIT_YEAR).expect("formats"),
        "9999-12-31T23:59:59.999Z"
    );
}

// ---- spans ------------------------------------------------------------------------------------

#[test]
fn a_span_names_its_unit_on_conversion() {
    // A bare number on the wire does not say which unit it means.
    let span = Span::from_secs(30);

    assert_eq!(span.as_millis(), 30_000);
}

#[test]
fn a_span_splits_into_whole_parts() {
    let span = Span::from_millis(MS_PER_DAY + 2 * MS_PER_HOUR + 3 * 60_000 + 4_000 + 5);

    let parts = span.parts();

    assert_eq!(parts.days, 1);
    assert_eq!(parts.hours, 2);
    assert_eq!(parts.minutes, 3);
    assert_eq!(parts.seconds, 4);
    assert_eq!(parts.milliseconds, 5);
    assert!(!parts.negative);
}

#[test]
fn a_negative_span_keeps_its_sign_separate() {
    // A reading of "-1 day 2 hours" is ambiguous about whether the hours add or subtract.
    let parts = Span::from_secs(-90).parts();

    assert!(parts.negative);
    assert_eq!(parts.minutes, 1);
    assert_eq!(parts.seconds, 30);
}

#[test]
fn the_span_between_two_instants_can_be_negative() {
    assert_eq!(Span::between(1000, 500).as_millis(), -500);
    assert_eq!(Span::between(500, 1000).as_millis(), 500);
}

#[test]
fn applying_a_span_reports_overflow() {
    // A wrapped timestamp is a wrong date, and a caller cannot tell.
    assert_eq!(Span::from_secs(60).apply(0), Some(60_000));
    assert_eq!(Span::from_secs(1).apply(Instant::MAX), None);
}

#[test]
fn a_span_formats_the_way_a_person_reads_it() {
    assert_eq!(format_span(Span::from_secs(90)), "1m 30s");
    assert_eq!(format_span(Span::from_millis(400)), "400ms");
    assert_eq!(format_span(Span::ZERO), "0ms");
    assert_eq!(format_span(Span::from_millis(-400)), "-400ms");
    assert_eq!(
        format_span(Span::from_millis(MS_PER_DAY + MS_PER_HOUR)),
        "1d 1h"
    );
}

#[test]
fn a_sub_second_span_does_not_format_as_zero() {
    // A caller that displayed "0s" for 400ms would report a wait that did not happen.
    assert_eq!(format_span(Span::from_millis(1)), "1ms");
}
