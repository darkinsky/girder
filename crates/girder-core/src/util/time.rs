//! Time vocabulary: instants, spans, and the canonical forms a boundary accepts.
//!
//! Three things, and each exists because a mistake with the other two is easy:
//!
//! | concept | representation | the mistake it prevents |
//! |---|---|---|
//! | [`Instant`] | milliseconds on the UTC timeline | a local time compared to a UTC one |
//! | [`Span`] | a type, not a number | seconds added to milliseconds |
//! | [`WallClock`] | a local date and time plus an offset | a time of day used as an instant |
//!
//! # Why a span is a type
//!
//! `Duration::from_secs(30)` and `Duration::from_millis(30)` differ by a factor of a thousand, and both
//! are perfectly good integers. The standard library already refuses to add them, so the hazard is not
//! the arithmetic — it is serialization: a bare `number` on the wire does not say which unit it means.
//! [`Span`] is therefore never converted to a number implicitly; a caller names the unit it wants.
//!
//! # Why the four-digit-year bound is enforced
//!
//! An RFC 3339 instant with a four-digit year is what every other system in this project parses. A
//! year outside that range is not a valid instant *for this protocol* even when the arithmetic would
//! accept it, and letting one through produces a timestamp a peer silently rejects or truncates.
//!
//! # Why local times are a separate type
//!
//! "02:30 tomorrow" is not an instant until an offset is chosen, and twice a year the choice is not
//! available at all. Keeping [`WallClock`] distinct from [`Instant`] is what forces a caller to resolve
//! that instead of accidentally treating a time of day as a moment.

/// Milliseconds on the UTC timeline.
///
/// A signed integer because a timestamp before 1970 is a real timestamp, and a `u64` would make one
/// unrepresentable rather than merely old.
pub type Instant = i64;

/// The earliest instant a four-digit-year RFC 3339 value names: `0001-01-01T00:00:00.000Z`.
///
/// Computed from the civil calendar rather than hard-coded so the constant and the parser cannot
/// disagree.
pub const MIN_FOUR_DIGIT_YEAR: Instant = -62_135_596_800_000;

/// The latest instant a four-digit-year RFC 3339 value names: `9999-12-31T23:59:59.999Z`.
pub const MAX_FOUR_DIGIT_YEAR: Instant = 253_402_300_799_999;

/// Milliseconds in one second.
pub const MS_PER_SECOND: i64 = 1_000;
/// Milliseconds in one minute.
pub const MS_PER_MINUTE: i64 = 60 * MS_PER_SECOND;
/// Milliseconds in one hour.
pub const MS_PER_HOUR: i64 = 60 * MS_PER_MINUTE;
/// Milliseconds in one day.
pub const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;

/// Whether an instant is one a four-digit-year RFC 3339 value can name.
#[must_use]
pub fn is_four_digit_year(instant: Instant) -> bool {
    (MIN_FOUR_DIGIT_YEAR..=MAX_FOUR_DIGIT_YEAR).contains(&instant)
}

/// A span of time.
///
/// A distinct type rather than a count of milliseconds: a bare number on the wire does not say which
/// unit it means, and a caller that guessed wrong is off by a factor of a thousand with no error
/// anywhere. Conversion to a number always names the unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span(i64);

impl Span {
    /// Zero.
    pub const ZERO: Self = Self(0);

    /// Builds a span from milliseconds.
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// Builds a span from whole seconds.
    #[must_use]
    pub const fn from_secs(seconds: i64) -> Self {
        Self(seconds * MS_PER_SECOND)
    }

    /// Builds a span from whole minutes.
    #[must_use]
    pub const fn from_minutes(minutes: i64) -> Self {
        Self(minutes * MS_PER_MINUTE)
    }

    /// The length in milliseconds.
    ///
    /// Named rather than `Into<i64>`: an implicit conversion is exactly what lets a span be mistaken
    /// for a different unit.
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// Whether the span is negative.
    ///
    /// Allowed: "one hour ago" is a span, and a type that only held forward spans would push the sign
    /// into the caller's arithmetic where it is easy to drop.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// The whole parts a reading is written in.
    #[must_use]
    pub fn parts(self) -> SpanParts {
        let absolute = self.0.unsigned_abs();
        let days = absolute / MS_PER_DAY as u64;
        let after_days = absolute % MS_PER_DAY as u64;
        let hours = after_days / MS_PER_HOUR as u64;
        let after_hours = after_days % MS_PER_HOUR as u64;
        let minutes = after_hours / MS_PER_MINUTE as u64;
        let after_minutes = after_hours % MS_PER_MINUTE as u64;
        let seconds = after_minutes / MS_PER_SECOND as u64;
        let milliseconds = after_minutes % MS_PER_SECOND as u64;
        SpanParts {
            days,
            hours,
            minutes,
            seconds,
            milliseconds,
            negative: self.0 < 0,
        }
    }

    /// The span between two instants, which is negative when `later` precedes `earlier`.
    #[must_use]
    pub const fn between(earlier: Instant, later: Instant) -> Self {
        Self(later - earlier)
    }

    /// Adds the span to an instant, reporting overflow rather than wrapping.
    ///
    /// `None` rather than a saturating result: a wrapped timestamp is a wrong date, and a caller that
    /// cannot tell the difference would store it.
    #[must_use]
    pub fn apply(self, instant: Instant) -> Option<Instant> {
        instant.checked_add(self.0)
    }
}

/// The whole parts of a span, in the units a reading is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanParts {
    /// Whole days.
    pub days: u64,
    /// Whole hours left after the days, from 0 through 23.
    pub hours: u64,
    /// Whole minutes left after the hours, from 0 through 59.
    pub minutes: u64,
    /// Whole seconds left after the minutes, from 0 through 59.
    pub seconds: u64,
    /// Milliseconds left after the seconds, from 0 up to (excluding) 1000.
    pub milliseconds: u64,
    /// Whether the original span was negative.
    ///
    /// Recorded rather than applied to each part: a reading of "-1 day 2 hours" is ambiguous about
    /// whether the hours add or subtract, so the parts stay unsigned and the sign is separate.
    pub negative: bool,
}

/// A local date and time, with no offset chosen.
///
/// Distinct from [`Instant`] because "02:30 tomorrow" is not a moment until an offset is picked, and
/// twice a year that offset does not exist. Keeping them apart forces the choice to happen where it can
/// be made rather than where it is silently assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WallClock {
    /// Year, four digits.
    pub year: i32,
    /// Month, 1 through 12.
    pub month: u32,
    /// Day of the month, 1 through 31.
    pub day: u32,
    /// Hour, 0 through 23.
    pub hour: u32,
    /// Minute, 0 through 59.
    pub minute: u32,
    /// Second, 0 through 59.
    pub second: u32,
    /// Millisecond, 0 through 999.
    pub millisecond: u32,
}

impl WallClock {
    /// Builds a wall-clock value.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError`] for a field outside its range or a day that does not exist in its month.
    /// Validating here rather than at resolution means a nonsensical date is refused where it was
    /// constructed, not where it was used.
    pub fn new(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
        millisecond: u32,
    ) -> Result<Self, TimeError> {
        if !(1..=12).contains(&month) {
            return Err(TimeError::Field {
                field: "month",
                value: i64::from(month),
            });
        }
        if day == 0 || day > days_in_month(year, month) {
            return Err(TimeError::Field {
                field: "day",
                value: i64::from(day),
            });
        }
        if hour > 23 {
            return Err(TimeError::Field {
                field: "hour",
                value: i64::from(hour),
            });
        }
        if minute > 59 {
            return Err(TimeError::Field {
                field: "minute",
                value: i64::from(minute),
            });
        }
        if second > 59 {
            return Err(TimeError::Field {
                field: "second",
                value: i64::from(second),
            });
        }
        if millisecond > 999 {
            return Err(TimeError::Field {
                field: "millisecond",
                value: i64::from(millisecond),
            });
        }
        Ok(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            millisecond,
        })
    }

    /// Resolves to an instant under a fixed offset.
    ///
    /// A fixed offset rather than a named zone: resolving a named zone needs a database that changes
    /// when a government changes its rules. A caller with one resolves the offset for this date and
    /// passes it in — which is also where a daylight-saving gap has to be decided.
    #[must_use]
    pub fn resolve(self, offset_millis: i64) -> Instant {
        let days = days_from_civil(self.year, self.month, self.day);
        let millis = days * MS_PER_DAY
            + i64::from(self.hour) * MS_PER_HOUR
            + i64::from(self.minute) * MS_PER_MINUTE
            + i64::from(self.second) * MS_PER_SECOND
            + i64::from(self.millisecond);
        millis - offset_millis
    }

    /// The wall-clock value of an instant under a fixed offset.
    #[must_use]
    pub fn from_instant(instant: Instant, offset_millis: i64) -> Self {
        let local = instant + offset_millis;
        let days = local.div_euclid(MS_PER_DAY);
        let within = local.rem_euclid(MS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: u32::try_from(within / MS_PER_HOUR).unwrap_or(0),
            minute: u32::try_from((within % MS_PER_HOUR) / MS_PER_MINUTE).unwrap_or(0),
            second: u32::try_from((within % MS_PER_MINUTE) / MS_PER_SECOND).unwrap_or(0),
            millisecond: u32::try_from(within % MS_PER_SECOND).unwrap_or(0),
        }
    }
}

/// Why a time operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TimeError {
    /// A field was outside its range, or named a day that does not exist.
    #[error("`{field}` cannot be {value}")]
    Field {
        /// Which field.
        field: &'static str,
        /// The rejected value.
        value: i64,
    },

    /// A string was not the canonical form.
    #[error("`{text}` is not a canonical instant")]
    NotAnInstant {
        /// The input.
        text: String,
    },

    /// The instant is outside the four-digit-year range.
    #[error("{instant} is outside the four-digit-year range")]
    OutOfRange {
        /// The instant.
        instant: Instant,
    },
}

/// Whether a year is a leap year.
#[must_use]
pub const fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// How many days a month has.
#[must_use]
pub const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from the Unix epoch to a civil date.
///
/// Howard Hinnant's algorithm: exact over the whole range, and it needs no table.
#[must_use]
pub fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = i64::from(if month <= 2 { year - 1 } else { year });
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date of a day index.
#[must_use]
pub fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        i32::try_from(year).unwrap_or(0),
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// Formats an instant as the canonical four-digit-year RFC 3339 UTC form.
///
/// Always with milliseconds and always ending in `Z`: one canonical spelling means two systems that
/// both format an instant produce the same string, which is what makes a stored value comparable.
///
/// # Errors
///
/// Returns [`TimeError::OutOfRange`] for an instant a four-digit year cannot name.
pub fn format_instant(instant: Instant) -> Result<String, TimeError> {
    if !is_four_digit_year(instant) {
        return Err(TimeError::OutOfRange { instant });
    }
    let (year, month, day) = civil_from_days(instant.div_euclid(MS_PER_DAY));
    let within = instant.rem_euclid(MS_PER_DAY);
    let hour = within / MS_PER_HOUR;
    let minute = (within % MS_PER_HOUR) / MS_PER_MINUTE;
    let second = (within % MS_PER_MINUTE) / MS_PER_SECOND;
    let millisecond = within % MS_PER_SECOND;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z"
    ))
}

/// Parses the canonical four-digit-year RFC 3339 UTC form.
///
/// Accepts exactly the shape `format_instant` produces: four-digit year, `T`, milliseconds, `Z`. A
/// lenient parser would accept strings that a stricter peer rejects, and the difference would appear
/// as a value that round-trips locally and fails elsewhere.
///
/// # Errors
///
/// Returns [`TimeError::NotAnInstant`] for any other shape.
pub fn parse_instant(text: &str) -> Result<Instant, TimeError> {
    let reject = || TimeError::NotAnInstant {
        text: text.to_owned(),
    };
    let bytes = text.as_bytes();
    // `YYYY-MM-DDTHH:MM:SS.mmmZ` is exactly 24 bytes.
    if bytes.len() != 24 {
        return Err(reject());
    }
    let number = |start: usize, length: usize| -> Result<i64, TimeError> {
        let slice = &text[start..start + length];
        slice.parse::<i64>().map_err(|_| reject())
    };
    let year = i32::try_from(number(0, 4)?).map_err(|_| reject())?;
    let month = u32::try_from(number(5, 2)?).map_err(|_| reject())?;
    let day = u32::try_from(number(8, 2)?).map_err(|_| reject())?;
    let hour = u32::try_from(number(11, 2)?).map_err(|_| reject())?;
    let minute = u32::try_from(number(14, 2)?).map_err(|_| reject())?;
    let second = u32::try_from(number(17, 2)?).map_err(|_| reject())?;
    let millisecond = u32::try_from(number(20, 3)?).map_err(|_| reject())?;

    // The separators are checked rather than skipped: a parser that ignored them would accept
    // `2026x01x01...`, and the value would round-trip locally and fail at a stricter peer.
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return Err(reject());
    }

    let wall = WallClock::new(year, month, day, hour, minute, second, millisecond)?;
    Ok(wall.resolve(0))
}

/// Renders a span the way a person reads it: `1d 2h 3m 4s`.
///
/// Zero parts are omitted, and a span under a second shows milliseconds rather than `0s` — a caller
/// that displayed `0s` for 400ms would report a wait that did not happen.
#[must_use]
pub fn format_span(span: Span) -> String {
    use std::fmt::Write as _;
    let parts = span.parts();
    let mut out = String::new();
    if parts.negative {
        out.push('-');
    }
    if parts.days > 0 {
        let _ = write!(out, "{}d ", parts.days);
    }
    if parts.hours > 0 {
        let _ = write!(out, "{}h ", parts.hours);
    }
    if parts.minutes > 0 {
        let _ = write!(out, "{}m ", parts.minutes);
    }
    if parts.seconds > 0 {
        let _ = write!(out, "{}s ", parts.seconds);
    }
    if parts.milliseconds > 0 || out.is_empty() || out == "-" {
        let _ = write!(out, "{}ms ", parts.milliseconds);
    }
    out.trim_end().to_owned()
}
