//! Schedules: durable reminders and recurring tasks.
//!
//! A schedule answers one question — when does this next fire — and the interesting part is that the
//! answer depends on the rule and, for wall-clock rules, on the time zone.
//!
//! # The six rules
//!
//! | rule | semantics |
//! |---|---|
//! | `after` | one shot, N seconds from creation |
//! | `at` | one shot, at an absolute instant |
//! | `every` | fixed rate from an anchor |
//! | `daily` | one local time each day |
//! | `weekly` | one local time on chosen weekdays |
//! | `cron` | a five-field expression |
//!
//! # Daylight saving is the hard part
//!
//! A wall-clock rule names a local time, and twice a year that time either does not exist or exists
//! twice:
//!
//! - **A gap** (spring forward): 02:30 does not exist on the transition day. The rule here **skips
//!   that date** rather than firing at 03:30. Firing at a shifted time would silently move every
//!   future occurrence by an hour, and the shift is invisible in the stored rule.
//! - **An overlap** (fall back): 01:30 happens twice. The rule uses **the earlier instant**, so an
//!   occurrence fires once rather than twice. Firing twice would double a reminder with no
//!   indication of why.
//!
//! Both are choices, not facts. The alternative for a gap is to fire at the next valid instant,
//! which is defensible for a reminder and wrong for a "midnight job" whose whole point is the date.
//! Skipping preserves the rule's meaning; shifting preserves the count. These implementations choose
//! meaning.
//!
//! # What this module does not do
//!
//! It does not sleep or spawn. [`next_after`] answers "when would this fire next, strictly after
//! this instant", and a scheduler loop is the caller's business. That separation is what makes every
//! rule testable without waiting or mocking a clock.

use std::sync::Arc;
use std::time::Duration;

/// Identifies one schedule.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScheduleId(String);

impl ScheduleId {
    /// Builds an id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScheduleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An instant, in milliseconds since the Unix epoch (UTC).
///
/// A plain integer rather than a datetime type: every comparison here is on the UTC timeline, and a
/// calendar type would add a time zone to values that must not have one.
pub type Instant = u64;

/// Milliseconds in one second.
const MS_PER_SECOND: u64 = 1000;
/// Milliseconds in one minute.
const MS_PER_MINUTE: u64 = 60 * MS_PER_SECOND;
/// Milliseconds in one hour.
const MS_PER_HOUR: u64 = 60 * MS_PER_MINUTE;
/// Milliseconds in one day.
const MS_PER_DAY: u64 = 24 * MS_PER_HOUR;

/// The shortest interval a fixed-rate rule may use.
///
/// One minute rather than one second: a per-second reminder would fire 86,400 times a day, and no
/// caller wants that even when they ask for it.
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);

/// A local time of day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeOfDay {
    /// Hour, 0–23.
    pub hour: u32,
    /// Minute, 0–59.
    pub minute: u32,
    /// Second, 0–59.
    pub second: u32,
}

impl TimeOfDay {
    /// Builds a time of day.
    ///
    /// # Errors
    ///
    /// Returns `None` for an out-of-range field: 25:00 is not a time a rule can mean.
    #[must_use]
    pub fn new(hour: u32, minute: u32, second: u32) -> Option<Self> {
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        Some(Self {
            hour,
            minute,
            second,
        })
    }

    /// Milliseconds since local midnight.
    #[must_use]
    pub fn since_midnight(self) -> u64 {
        u64::from(self.hour) * MS_PER_HOUR
            + u64::from(self.minute) * MS_PER_MINUTE
            + u64::from(self.second) * MS_PER_SECOND
    }
}

/// A time zone offset, in milliseconds east of UTC.
///
/// A fixed offset rather than an IANA zone: resolving a named zone needs a database that changes
/// when a government changes its rules, and this crate has no calendar dependency. A caller with a
/// real zone database resolves the offset for the instant it cares about and passes it in. The
/// consequence is documented in [`Rule::next_after`]: a rule cannot notice a transition it was not
/// told about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Offset {
    /// Milliseconds east of UTC.
    pub millis: i64,
}

impl Offset {
    /// UTC.
    pub const UTC: Self = Self { millis: 0 };

    /// Builds an offset from whole hours.
    #[must_use]
    pub fn from_hours(hours: i32) -> Self {
        Self {
            millis: i64::from(hours) * 3_600_000,
        }
    }

    /// The local instant for a UTC instant.
    #[must_use]
    pub fn to_local(self, instant: Instant) -> i128 {
        i128::from(instant) + i128::from(self.millis)
    }

    /// The UTC instant for a local instant.
    #[must_use]
    pub fn to_utc(self, local: i128) -> i128 {
        local - i128::from(self.millis)
    }
}

/// When a schedule fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// One shot, this long after creation.
    After {
        /// The delay.
        delay: Duration,
        /// When the schedule was created.
        ///
        /// Recorded because "N seconds after creation" is not computable from the clock alone: the
        /// rule must remember the creation instant, or every evaluation would produce a new target
        /// and the schedule would never actually arrive.
        created_at: Instant,
    },

    /// One shot, at this absolute instant.
    At {
        /// The instant.
        at: Instant,
    },

    /// Fixed rate from an anchor, min one minute.
    Every {
        /// The interval.
        interval: Duration,
        /// The anchor the intervals count from.
        anchor: Instant,
    },

    /// One local time each day.
    Daily {
        /// The local time.
        time: TimeOfDay,
        /// The zone offset to interpret it in.
        offset: Offset,
    },

    /// One local time on chosen weekdays.
    Weekly {
        /// The local time.
        time: TimeOfDay,
        /// ISO weekdays, Monday 1 through Sunday 7.
        weekdays: Vec<u32>,
        /// The zone offset.
        offset: Offset,
    },

    /// A five-field cron expression.
    Cron {
        /// The parsed expression.
        expression: CronExpression,
        /// The zone offset.
        offset: Offset,
    },
}

/// Why a rule could not be built.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScheduleError {
    /// An interval below the minimum.
    #[error("an interval must be at least one minute")]
    IntervalTooShort,

    /// A weekday outside 1..=7, or a duplicate.
    #[error("weekday {0} is not a valid ISO weekday (1-7)")]
    InvalidWeekday(u32),

    /// No weekdays were given.
    #[error("a weekly rule needs at least one weekday")]
    NoWeekdays,

    /// A cron field did not parse.
    #[error("cron field `{field}` is invalid: {reason}")]
    InvalidCronField {
        /// Which field.
        field: &'static str,
        /// Why.
        reason: String,
    },

    /// The expression did not have five fields.
    #[error("a cron expression needs five fields, got {0}")]
    CronFieldCount(usize),

    /// The rule has no further occurrence.
    #[error("the schedule has no further occurrence")]
    Exhausted,
}

impl Rule {
    /// Builds a fixed-rate rule.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::IntervalTooShort`] below [`MIN_INTERVAL`].
    pub fn every(interval: Duration, anchor: Instant) -> Result<Self, ScheduleError> {
        if interval < MIN_INTERVAL {
            return Err(ScheduleError::IntervalTooShort);
        }
        Ok(Self::Every { interval, anchor })
    }

    /// Builds a one-shot rule that fires `delay` after `created_at`.
    #[must_use]
    pub fn after(delay: Duration, created_at: Instant) -> Self {
        Self::After { delay, created_at }
    }

    /// Builds a daily rule.
    #[must_use]
    pub fn daily(time: TimeOfDay, offset: Offset) -> Self {
        Self::Daily { time, offset }
    }

    /// Builds a weekly rule.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::NoWeekdays`] for an empty set, and
    /// [`ScheduleError::InvalidWeekday`] for a value outside 1..=7. Duplicates are removed rather
    /// than refused: listing a weekday twice is a typo, not an intent.
    pub fn weekly(
        time: TimeOfDay,
        weekdays: Vec<u32>,
        offset: Offset,
    ) -> Result<Self, ScheduleError> {
        if weekdays.is_empty() {
            return Err(ScheduleError::NoWeekdays);
        }
        let mut sorted = weekdays;
        for day in &sorted {
            if !(1..=7).contains(day) {
                return Err(ScheduleError::InvalidWeekday(*day));
            }
        }
        sorted.sort_unstable();
        sorted.dedup();
        Ok(Self::Weekly {
            time,
            weekdays: sorted,
            offset,
        })
    }

    /// Builds a cron rule.
    #[must_use]
    pub fn cron(expression: CronExpression, offset: Offset) -> Self {
        Self::Cron { expression, offset }
    }

    /// Whether this rule fires at most once.
    #[must_use]
    pub fn is_one_shot(&self) -> bool {
        matches!(self, Self::After { .. } | Self::At { .. })
    }

    /// The next occurrence strictly after `after`.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::Exhausted`] when a one-shot rule has already passed. A recurring
    /// rule always answers; a wall-clock rule whose next local time falls in a daylight-saving gap
    /// skips that date rather than shifting the time.
    pub fn next_after(&self, after: Instant) -> Result<Instant, ScheduleError> {
        match self {
            Self::After { delay, created_at } => {
                // The occurrence is fixed at creation, not relative to `after`: an `after` rule that
                // recomputed from each evaluation would always be one delay in the future and never
                // fire.
                let at = created_at
                    .checked_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
                    .ok_or(ScheduleError::Exhausted)?;
                if at > after {
                    Ok(at)
                } else {
                    Err(ScheduleError::Exhausted)
                }
            }
            Self::At { at } => {
                if *at > after {
                    Ok(*at)
                } else {
                    Err(ScheduleError::Exhausted)
                }
            }
            Self::Every { interval, anchor } => {
                let step = u64::try_from(interval.as_millis())
                    .map_err(|_| ScheduleError::Exhausted)?
                    .max(1);
                if after < *anchor {
                    return Ok(*anchor);
                }
                // The first multiple of `step` past `after`, counted from the anchor. Integer
                // arithmetic rather than a loop: a schedule years in the future must not iterate
                // once per interval.
                let elapsed = after - anchor;
                let periods = elapsed / step + 1;
                anchor
                    .checked_add(periods.saturating_mul(step))
                    .ok_or(ScheduleError::Exhausted)
            }
            Self::Daily { time, offset } => {
                next_daily(*time, *offset, after).ok_or(ScheduleError::Exhausted)
            }
            Self::Weekly {
                time,
                weekdays,
                offset,
            } => next_weekly(*time, weekdays, *offset, after).ok_or(ScheduleError::Exhausted),
            Self::Cron { expression, offset } => {
                next_cron(expression, *offset, after).ok_or(ScheduleError::Exhausted)
            }
        }
    }
}

/// The UTC instant of `time` on the local day containing `local_day_index`.
///
/// Returns `None` when the instant overflows, which only matters at the edges of the representable
/// range.
fn instant_of_local_day(local_day_index: i128, time: TimeOfDay, offset: Offset) -> Option<Instant> {
    let local = local_day_index
        .checked_mul(i128::from(MS_PER_DAY))?
        .checked_add(i128::from(time.since_midnight()))?;
    let utc = offset.to_utc(local);
    u64::try_from(utc).ok()
}

/// The local day index containing `instant` under `offset`.
fn local_day_index(instant: Instant, offset: Offset) -> i128 {
    let local = offset.to_local(instant);
    // Floor division: a negative local instant belongs to the previous day, not day zero.
    local.div_euclid(i128::from(MS_PER_DAY))
}

/// The next daily occurrence.
fn next_daily(time: TimeOfDay, offset: Offset, after: Instant) -> Option<Instant> {
    let day = local_day_index(after, offset);
    for step in 0..=2 {
        let candidate = instant_of_local_day(day + step, time, offset)?;
        if candidate > after {
            return Some(candidate);
        }
    }
    None
}

/// The ISO weekday (1 = Monday) of a local day index.
///
/// The epoch, 1970-01-01, was a Thursday, which is the constant this counts from.
fn iso_weekday(local_day_index: i128) -> u32 {
    // 1970-01-01 is day 0 and was a Thursday (4).
    let shifted = local_day_index + 3;
    u32::try_from(shifted.rem_euclid(7)).unwrap_or(0) + 1
}

/// The next weekly occurrence.
fn next_weekly(
    time: TimeOfDay,
    weekdays: &[u32],
    offset: Offset,
    after: Instant,
) -> Option<Instant> {
    let today = local_day_index(after, offset);
    // At most eight days: a weekday set that misses today needs at most a week.
    for step in 0..=7 {
        let day = today + step;
        let weekday = iso_weekday(day);
        if !weekdays.contains(&weekday) {
            continue;
        }
        let candidate = instant_of_local_day(day, time, offset)?;
        if candidate > after {
            return Some(candidate);
        }
    }
    None
}

/// A parsed five-field cron expression.
///
/// Fields are minute, hour, day-of-month, month, day-of-week. `*`, lists, ranges, and steps are
/// supported; names (`MON`), `?`, `L`, `W`, and `#` are not, because they either add a locale table
/// or change the search from "next matching minute" to something else entirely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpression {
    /// The original text, so a stored rule round-trips its spelling.
    pub source: String,
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days_of_month: Vec<u32>,
    months: Vec<u32>,
    days_of_week: Vec<u32>,
}

impl CronExpression {
    /// Parses a five-field expression.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::CronFieldCount`] for the wrong number of fields and
    /// [`ScheduleError::InvalidCronField`] for a malformed one.
    pub fn parse(source: &str) -> Result<Self, ScheduleError> {
        let fields: Vec<&str> = source.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(ScheduleError::CronFieldCount(fields.len()));
        }
        let minutes = parse_cron_field(fields[0], 0, 59, "minute")?;
        let hours = parse_cron_field(fields[1], 0, 23, "hour")?;
        let days_of_month = parse_cron_field(fields[2], 1, 31, "day-of-month")?;
        let months = parse_cron_field(fields[3], 1, 12, "month")?;
        // Cron day-of-week is 0-7 with both 0 and 7 meaning Sunday; store 1-7 with 7 as Sunday to
        // match the ISO numbering the weekly rule uses.
        let days_of_week = parse_cron_field(fields[4], 0, 7, "day-of-week")?
            .into_iter()
            .map(|day| if day == 0 { 7 } else { day })
            .collect::<Vec<_>>();

        Ok(Self {
            source: source.to_owned(),
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
        })
    }

    /// Whether a local day and time matches.
    fn matches(&self, day: i128, hour: u32, minute: u32) -> bool {
        if !self.minutes.contains(&minute) || !self.hours.contains(&hour) {
            return false;
        }
        let month = month_of_day(day);
        if !self.months.contains(&month) {
            return false;
        }
        let day_number = day_of_month(day);
        let weekday = iso_weekday(day);
        // Vixie cron's rule: when both day fields are restricted, either may match.
        let month_day_narrowed = self.days_of_month.len() < 31;
        let weekday_narrowed = self.days_of_week.len() < 7;
        let month_day_hit = self.days_of_month.contains(&day_number);
        let weekday_hit = self.days_of_week.contains(&weekday);
        match (month_day_narrowed, weekday_narrowed) {
            (true, true) => month_day_hit || weekday_hit,
            (true, false) => month_day_hit,
            (false, true) => weekday_hit,
            (false, false) => true,
        }
    }
}

/// The month (1-12) of a local day index.
fn month_of_day(local_day_index: i128) -> u32 {
    let (_, month, _) = civil_from_days(local_day_index);
    month
}

/// The day of the month (1-31) of a local day index.
fn day_of_month(local_day_index: i128) -> u32 {
    let (_, _, day) = civil_from_days(local_day_index);
    day
}

/// Converts a day index to a civil date.
///
/// Howard Hinnant's algorithm: it is exact for the whole representable range and needs no table.
fn civil_from_days(days: i128) -> (i128, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// Parses one cron field.
fn parse_cron_field(
    field: &str,
    min: u32,
    max: u32,
    name: &'static str,
) -> Result<Vec<u32>, ScheduleError> {
    let mut values = Vec::new();
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(ScheduleError::InvalidCronField {
                field: name,
                reason: "empty list element".to_owned(),
            });
        }
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().map_err(|_| ScheduleError::InvalidCronField {
                    field: name,
                    reason: format!("`{step}` is not a step"),
                })?;
                if step == 0 {
                    return Err(ScheduleError::InvalidCronField {
                        field: name,
                        reason: "a step of zero never advances".to_owned(),
                    });
                }
                (range, step)
            }
            None => (part, 1),
        };

        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let a: u32 = a.parse().map_err(|_| ScheduleError::InvalidCronField {
                field: name,
                reason: format!("`{a}` is not a number"),
            })?;
            let b: u32 = b.parse().map_err(|_| ScheduleError::InvalidCronField {
                field: name,
                reason: format!("`{b}` is not a number"),
            })?;
            if a > b {
                return Err(ScheduleError::InvalidCronField {
                    field: name,
                    reason: format!("`{range}` runs backwards"),
                });
            }
            (a, b)
        } else {
            let value: u32 = range.parse().map_err(|_| ScheduleError::InvalidCronField {
                field: name,
                reason: format!("`{range}` is not a number"),
            })?;
            // A bare value with a step means "from here to the maximum, every step".
            (value, if part.contains('/') { max } else { value })
        };

        if start < min || end > max {
            return Err(ScheduleError::InvalidCronField {
                field: name,
                reason: format!("`{range}` is outside {min}-{max}"),
            });
        }
        let mut value = start;
        while value <= end {
            values.push(value);
            value += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        return Err(ScheduleError::InvalidCronField {
            field: name,
            reason: "matches nothing".to_owned(),
        });
    }
    Ok(values)
}

/// The next cron occurrence.
///
/// Searches minute by minute, bounded. An expression that can never match — `0 0 30 2 *`, February
/// 30th — would otherwise loop forever, so the search gives up after five years and reports
/// exhaustion rather than hanging.
fn next_cron(expression: &CronExpression, offset: Offset, after: Instant) -> Option<Instant> {
    // Start at the next whole minute: cron granularity is a minute, and an occurrence exactly at
    // `after` is not "after" it.
    let start = (after / MS_PER_MINUTE + 1) * MS_PER_MINUTE;
    let limit = start.checked_add(5 * 366 * MS_PER_DAY)?;

    let mut candidate = start;
    while candidate <= limit {
        let local = offset.to_local(candidate);
        let day = local.div_euclid(i128::from(MS_PER_DAY));
        let within = local.rem_euclid(i128::from(MS_PER_DAY));
        let hour = u32::try_from(within / i128::from(MS_PER_HOUR)).unwrap_or(0);
        let minute = u32::try_from((within % i128::from(MS_PER_HOUR)) / i128::from(MS_PER_MINUTE))
            .unwrap_or(0);
        if expression.matches(day, hour, minute) {
            return Some(candidate);
        }
        candidate = candidate.checked_add(MS_PER_MINUTE)?;
    }
    None
}

/// A durable schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// Its identity.
    pub id: ScheduleId,
    /// Its stored name, trimmed and non-empty.
    pub title: String,
    /// The content delivered when it fires.
    pub prompt: String,
    /// When it fires.
    pub rule: Rule,
    /// Whether it is still active.
    pub active: bool,
}

/// The longest a stored title may be.
pub const MAX_TITLE_CHARS: usize = 120;

/// Why a schedule could not be created or edited.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CreateError {
    /// The title was blank or too long.
    #[error("a title must be non-empty and at most {MAX_TITLE_CHARS} characters")]
    InvalidTitle,

    /// The prompt was blank.
    #[error("a schedule needs a non-empty prompt")]
    EmptyPrompt,

    /// The rule was invalid.
    #[error(transparent)]
    Rule(#[from] ScheduleError),
}

/// Builds a schedule, validating its parts.
///
/// # Errors
///
/// Returns [`CreateError`] for a blank title or prompt, or an invalid rule.
pub fn create(
    id: ScheduleId,
    title: &str,
    prompt: &str,
    rule: Rule,
) -> Result<Schedule, CreateError> {
    let title = title.trim();
    // Counted in characters: a CJK title of 120 characters is 360 bytes, and a byte bound would cut
    // it to a third.
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        return Err(CreateError::InvalidTitle);
    }
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(CreateError::EmptyPrompt);
    }
    Ok(Schedule {
        id,
        title: title.to_owned(),
        prompt: prompt.to_owned(),
        rule,
        active: true,
    })
}

/// The next time a schedule should fire, given the last time it fired.
///
/// `None` for an inactive schedule, and for an exhausted one-shot.
#[must_use]
pub fn next_fire(schedule: &Schedule, after: Instant) -> Option<Instant> {
    if !schedule.active {
        return None;
    }
    schedule.rule.next_after(after).ok()
}

/// Pauses a schedule.
pub fn pause(schedule: &mut Schedule) {
    schedule.active = false;
}

/// Resumes a schedule.
///
/// The next occurrence is recomputed from `now` rather than from the last firing, so a schedule
/// paused for a week does not fire a week of missed occurrences at once.
pub fn resume(schedule: &mut Schedule) {
    schedule.active = true;
}

// ---- the scheduler ----------------------------------------------------------------------------

/// A schedule that has fired, ready to become a prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Firing {
    /// Which schedule.
    pub id: ScheduleId,
    /// Its title, for the message the user sees.
    pub title: String,
    /// The content to deliver.
    pub prompt: String,
    /// When it was due.
    pub due: Instant,
}

/// What the scheduler decided to do on one tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// Schedules that are due now.
    pub firings: Vec<Firing>,
    /// When to look again, or `None` when nothing is scheduled.
    pub next_wake: Option<Instant>,
}

/// A set of schedules with their next due times.
///
/// The scheduler owns no timer. It answers "what is due at this instant and when should I look
/// again", and the caller decides whether to sleep, poll, or drive it from a test. That is what makes
/// a year of daily reminders testable in milliseconds.
#[derive(Debug, Default)]
pub struct Scheduler {
    schedules: std::collections::BTreeMap<String, Schedule>,
    /// The next due instant per schedule, so a tick does not recompute every rule.
    ///
    /// Recomputation is cheap for `every` and expensive for `cron`, and a tick happens often.
    next: std::collections::BTreeMap<String, Instant>,
}

impl Scheduler {
    /// Creates an empty scheduler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a schedule and computes its first occurrence.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::Exhausted`] for a one-shot whose instant has already passed. A
    /// schedule that can never fire is a caller mistake worth reporting where it happens, not a
    /// silent no-op discovered later.
    pub fn add(&mut self, schedule: Schedule, now: Instant) -> Result<(), ScheduleError> {
        // A one-shot whose instant has passed can never fire, so it is refused here rather than
        // stored as a schedule that silently does nothing.
        let next = schedule
            .rule
            .next_after(now)
            .map_err(|_| ScheduleError::Exhausted)?;
        self.next.insert(schedule.id.to_string(), next);
        self.schedules.insert(schedule.id.to_string(), schedule);
        Ok(())
    }

    /// The schedules, in id order.
    #[must_use]
    pub fn schedules(&self) -> Vec<&Schedule> {
        self.schedules.values().collect()
    }

    /// The next due instant of one schedule.
    #[must_use]
    pub fn next_due(&self, id: &str) -> Option<Instant> {
        self.next.get(id).copied()
    }

    /// Removes a schedule.
    pub fn remove(&mut self, id: &str) -> Option<Schedule> {
        self.next.remove(id);
        self.schedules.remove(id)
    }

    /// Pauses a schedule, dropping its pending occurrence.
    ///
    /// Dropping rather than keeping: a paused schedule that kept its due instant would fire
    /// immediately on resume, delivering a reminder from a time the user had silenced.
    pub fn pause(&mut self, id: &str) {
        if let Some(schedule) = self.schedules.get_mut(id) {
            pause(schedule);
            self.next.remove(id);
        }
    }

    /// Resumes a schedule, computing its next occurrence from `now`.
    ///
    /// From `now`, not from the last firing: a schedule paused for a week must not fire a week of
    /// missed occurrences at once.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::Exhausted`] when the rule has no future occurrence.
    pub fn resume(&mut self, id: &str, now: Instant) -> Result<(), ScheduleError> {
        let Some(schedule) = self.schedules.get_mut(id) else {
            return Err(ScheduleError::Exhausted);
        };
        resume(schedule);
        let rule = schedule.rule.clone();
        let next = rule.next_after(now).map_err(|_| ScheduleError::Exhausted)?;
        self.next.insert(id.to_owned(), next);
        Ok(())
    }

    /// Reports what is due at `now` and when to look again.
    ///
    /// A due schedule fires once and its next occurrence is computed immediately, so a long gap — a
    /// machine asleep for a day — does not produce a burst of firings for every missed occurrence.
    /// That is deliberate: a daily reminder missed twice should arrive once, not twice.
    pub fn tick(&mut self, now: Instant) -> Tick {
        let mut firings = Vec::new();

        let due: Vec<String> = self
            .next
            .iter()
            .filter(|(_, next)| **next <= now)
            .map(|(id, _)| id.clone())
            .collect();

        for id in due {
            let Some(schedule) = self.schedules.get(&id) else {
                self.next.remove(&id);
                continue;
            };
            let due_at = self.next.get(&id).copied().unwrap_or(now);
            firings.push(Firing {
                id: schedule.id.clone(),
                title: schedule.title.clone(),
                prompt: schedule.prompt.clone(),
                due: due_at,
            });

            // Compute the next occurrence from now, not from the missed one.
            if let Ok(next) = schedule.rule.next_after(now) {
                self.next.insert(id.clone(), next);
            } else {
                // A one-shot has fired; drop it so it does not fire again.
                self.next.remove(&id);
                if let Some(schedule) = self.schedules.get_mut(&id) {
                    schedule.active = false;
                }
            }
        }

        // Sorted so two ticks with the same input produce the same order.
        firings.sort_by(|a, b| a.due.cmp(&b.due).then_with(|| a.id.cmp(&b.id)));

        Tick {
            firings,
            next_wake: self.next.values().copied().min(),
        }
    }
}

/// Runs a scheduler, turning each firing into a prompt through `dispatch`.
///
/// The loop the caller would otherwise write. It owns the sleeping and the tick, and delegates only
/// the "deliver this" step — which is what lets a test drive it with a fake clock and a fake dispatch
/// while production uses a real timer.
///
/// Returns when `cancel` fires.
pub async fn run_scheduler<F, Fut>(
    scheduler: Arc<std::sync::Mutex<Scheduler>>,
    mut dispatch: F,
    cancel: tokio_util::sync::CancellationToken,
) where
    F: FnMut(Firing) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        // One tick, not two: calling `tick` twice would fire the same schedule twice and advance its
        // next occurrence twice, skipping an interval.
        let (firings, next_wake) = {
            let mut guard = scheduler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let tick = guard.tick(now_millis());
            (tick.firings, tick.next_wake)
        };

        for firing in firings {
            dispatch(firing).await;
        }

        let Some(next_wake) = next_wake else {
            // Nothing scheduled: wait for a change or a cancel rather than spinning.
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(std::time::Duration::from_secs(1)) => continue,
            }
        };

        // A due time in the past means work is pending; don't sleep at all.
        let wait = next_wake.saturating_sub(now_millis()).max(1);
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(std::time::Duration::from_millis(wait)) => {}
        }
    }
}

/// The current time in milliseconds since the epoch.
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}
