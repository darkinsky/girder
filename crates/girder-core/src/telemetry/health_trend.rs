//! Health trends across many sessions: is this deployment getting worse than it was?
//!
//! # The two halves that already existed
//!
//! `session_health` answers a question about **one** log. [`diagnose_health`] returns that log's
//! issues, [`is_healthy`] turns them into a verdict, and `health_score` into a number between 0 and
//! 100. All three take a single `&[SessionEvent]` and none of them knows that other sessions exist,
//! so a caller holding a week of logs can score each one and still has nothing to compare.
//!
//! `stats_trend` answers a question about **a sequence of numbers**. It takes `Point`s and reports a
//! mean, a median, a spread, and a window-over-window [`compare`](crate::compare_windows). Useful for
//! "is today busier than yesterday", because a token count is a number whose direction is obvious.
//! Health is not: `stats_trend` receives floats with no verdict attached, so it cannot tell a window
//! whose sessions broke more from one whose sessions merely scored lower for the same reason, and a
//! `Point` carries no session count, so a window of three sessions weighs the same as a window of
//! three thousand.
//!
//! So the Monday question -- "are we breaking more conversations than last week?" -- needed the two
//! halves joined: a per-session verdict, aggregated into a sample per window, compared across
//! windows. That join is this module, and nothing in the crate did it before: [`sample`] is the only
//! function that reduces many logs to one number set, and [`analyze`] is the only place that turns a
//! change in that set into the word "degrading".
//!
//! # Why the comparison is two halves and not a regression
//!
//! A least-squares slope over the samples would be more machinery for a weaker answer here. The
//! samples are not a clean time series: windows have different lengths, different session counts,
//! and gaps where a deployment was idle, so a slope's units depend on the sampling cadence the
//! caller happened to choose, and a single spike at either end tilts it. Splitting the window in two
//! and pooling each half answers the question actually asked -- "is the recent half worse than the
//! earlier half" -- in a way a reader can check by hand, and it puts no weight on the exact spacing.
//!
//! The counts are pooled rather than averaged as rates, for the same reason `stats_trend` centres on
//! a median: an unweighted average of per-window rates lets a window of three sessions outvote a
//! window of three thousand. Pooling the counts makes the answer "of all sessions in the earlier
//! half, 95% were healthy", which is a sentence about the deployment rather than about the caller's
//! bucketing.
//!
//! # Why the threshold is not "any decline"
//!
//! A health rate computed over a week of real traffic moves by a point or two for reasons that have
//! nothing to do with health: a different mix of workloads, one long-running session that stays open
//! across the boundary, a quiet Monday. A rule that alarmed on any decline would fire most weeks, and
//! an alarm that fires most weeks is a line nobody reads. The floors are therefore absolute and
//! stated in the constants [`HEALTH_RATE_DROP`] and [`SCORE_DROP`], so the judgement is visible and
//! can be argued with, rather than buried in a comparison.
//!
//! # What this does not do
//!
//! No clock and no bucketing: [`Sample::at_ms`] is carried, and [`sample`] takes `now_ms` from the
//! caller, because only the caller knows which time zone "this week" is asked in. Nothing here reads
//! the filesystem or decodes a log either -- the events are handed in by a caller that already has
//! them, which keeps the module testable and keeps the I/O policy out of it.

// Named through `::girder_core` rather than `crate::` so this file compiles both as a module of the
// library and, by `tests/health_trend.rs`, as a module of the test crate; `extern crate self as
// girder_core` in the crate root is what makes the path resolve from inside the library too. The
// names are the crate root's re-exports, because `session_health` itself is a private module.
use ::girder_core::{HealthSeverity, SessionEvent, diagnose_health, health_score};

/// How far the pooled health rate must fall before a fall is called a trend.
///
/// In fractions of the whole, so `0.02` is two percentage points. Two rather than one because a
/// single point is inside the ordinary churn of a real deployment: it is what one session in a
/// hundred looks like, and a hundred-session window cannot estimate a rate more finely than that. A
/// decline of 1% is a number, not evidence; a decline of 2% is two-and-a-half sigma away from it on a
/// window of a few hundred sessions, which is worth a reader's attention.
pub const HEALTH_RATE_DROP: f64 = 0.02;

/// How far the pooled mean score must fall before a fall is called a trend.
///
/// In score points on the 0-100 scale of `health_score`, and equal to `WARNING_PENALTY`: a drop of
/// five points is one additional warning per session on average across the window. That is the
/// smallest change that corresponds to a record a reader can go and look at, so anything below it is
/// a shift in the mix rather than damage.
pub const SCORE_DROP: f64 = 5.0;

/// One deployment's health, observed at one point in time.
///
/// The counts are absolute rather than rates because a rate cannot be pooled without the counts
/// behind it: two windows at 95% and 60% do not average to anything meaningful when one covered a
/// thousand sessions and the other covered three. [`analyze`] pools the counts, so a small window
/// cannot outvote a large one.
///
/// `healthy` is expected to be at most `sessions`; a sample that reports otherwise is not clamped,
/// because a rate above 100% is a bug in the caller that should be visible rather than silently
/// rounded into something plausible.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// When the window this sample describes ended, in milliseconds since the Unix epoch.
    ///
    /// Not a metric: nothing here compares it to a clock. It is carried because [`analyze`] needs an
    /// order to know which half is earlier, and because a caller plotting the same samples should not
    /// have to carry the timestamps separately.
    pub at_ms: u64,

    /// How many sessions the window covered.
    ///
    /// Zero means the window is not a measurement of anything, which is how [`analyze`] tells an idle
    /// period apart from a bad one. See [`Sample::mean_score`] for why that distinction has to be
    /// made explicitly.
    pub sessions: usize,

    /// How many of those sessions passed [`is_healthy`].
    ///
    /// Healthy is "no issue above `Info`", so a session still waiting on a tool counts: that is what a
    /// live conversation looks like. Counting only logs with no issues at all would make every active
    /// session look broken and would put a deployment's health rate at the mercy of how many
    /// conversations happen to be running.
    pub healthy: usize,

    /// How many of those sessions had at least one `Error`-severity issue.
    ///
    /// Not the complement of [`Sample::healthy`]: a session with a warning is unhealthy but not
    /// errored, and the two numbers moving apart is itself informative -- a window can keep the same
    /// error count while its warning count climbs.
    pub errors: usize,

    /// The average `health_score` of the window's sessions.
    ///
    /// `0.0` when [`Sample::sessions`] is zero, which is **not** a score of zero: a window that
    /// recorded nothing did not fail everything. The two cases are told apart by `sessions`, and
    /// [`analyze`] drops the empty one instead of letting its `0.0` manufacture a collapse.
    pub mean_score: f64,
}

/// A health trend across the samples it was computed from.
#[derive(Clone, Debug, PartialEq)]
pub struct Trend {
    /// The change in health rate, as `after - before` in fractions of the whole.
    ///
    /// `-0.13` means the later half was thirteen percentage points less healthy. A fraction rather
    /// than points because it is the ratio the halves were compared in; [`Trend::summary`] renders the
    /// same number as points, which is the form a reader can check against the two rates it names.
    pub health_rate_delta: f64,

    /// The change in mean score, as `after - before` in score points.
    ///
    /// The rate alone hides a real shape: the same fraction of sessions can be broken while the
    /// broken ones fail harder. This is the signal for that, which is why [`Trend::degrading`] is
    /// decided by either number and not only the rate.
    pub mean_score_delta: f64,

    /// Whether the recent half is worse than the earlier one by more than the noise floor.
    ///
    /// True when either [`Trend::health_rate_delta`] fell by [`HEALTH_RATE_DROP`] or more, or
    /// [`Trend::mean_score_delta`] fell by [`SCORE_DROP`] or more. Either alone is enough because the
    /// two measure different failures: more sessions crossing the line, or the same sessions crossing
    /// it further.
    pub degrading: bool,

    /// A sentence naming both rates, both scores, and the verdict.
    ///
    /// The deltas are in this struct already, and they are still not the answer: `-0.13` does not say
    /// thirteen points, does not say from what to what, and does not say whether that is worth
    /// acting on. Whoever reads a trend in a log line, an alert, or a report gets this string.
    pub summary: String,
}

/// Analyses a deployment's health trend, or `None` if there is not enough to compare.
///
/// # Why at least two samples
///
/// One sample says what is true now; it cannot say whether that is a change, in the same way that one
/// `Point` cannot have a spread. `None` rather than a [`Trend`] of zeroes, because a zero trend is
/// indistinguishable from a genuinely stable window and would be displayed as a measurement of
/// stability that nobody made.
///
/// # Which samples count
///
/// Samples are compared by the *position* of their timestamp: the earlier half is the first
/// `len / 2` samples, the later half is the rest, so an odd count gives the later half the extra
/// sample. The slice is ordered by [`Sample::at_ms`] first, stably, so a caller that assembled its
/// samples out of order still gets "before" and "after" rather than an artefact of insertion --
/// equal timestamps keep their input order, which makes the function total over any input.
///
/// A sample with no sessions is dropped before that split, because it carries no verdicts: pooling
/// its `0.0` mean score into a half would report a collapse that no session experienced. If fewer
/// than two samples remain, the answer is `None` -- two idle windows have no health to compare.
///
/// Each half is pooled by count, so the halves are compared as "95% of the 1,200 sessions before"
/// against "82% of the 900 after", not as an average of per-window rates over possibly tiny windows.
///
/// # Non-finite scores
///
/// A `mean_score` that is `NaN` or infinite propagates into the deltas instead of being dropped, so
/// the input stays visibly wrong rather than quietly disappearing from the denominator.
#[must_use]
pub fn analyze(samples: &[Sample]) -> Option<Trend> {
    if samples.len() < 2 {
        return None;
    }

    // Copy, because the halves are defined by time order and the caller's slice is not ours to sort.
    let mut ordered: Vec<Sample> = samples
        .iter()
        .copied()
        .filter(|sample| sample.sessions > 0)
        .collect();
    if ordered.len() < 2 {
        return None;
    }

    // Stable, so samples sharing a timestamp keep the order the caller built them in.
    ordered.sort_by_key(|sample| sample.at_ms);

    let split = ordered.len() / 2;
    let (earlier, later) = ordered.split_at(split);
    let before = Pool::of(earlier)?;
    let after = Pool::of(later)?;

    let rate_delta = after.health_rate() - before.health_rate();
    let score_delta = after.mean_score() - before.mean_score();
    let verdict = Verdict::of(rate_delta, score_delta);

    Some(Trend {
        health_rate_delta: rate_delta,
        mean_score_delta: score_delta,
        degrading: verdict == Verdict::Degrading,
        summary: verdict.describe(&before, &after, rate_delta, score_delta),
    })
}

/// Reduces a batch of sessions to one [`Sample`], stamped with `now_ms`.
///
/// Each element of `events` is one session's log, in the form [`diagnose_health`] takes it: the caller
/// has already decoded the logs and decided what a window is, which is the part this module
/// deliberately does not know how to do.
///
/// The issues of a session are walked once. The score is asked of `health_score` instead of being
/// recomputed from those issues, because the weights live in `session_health` and a second definition
/// here would let the two numbers drift apart; the cost is a second pass over one log's records, which
/// is cheap next to the decode that produced them.
///
/// An empty batch yields a sample with no sessions and a `0.0` mean score. `0.0` is the only
/// representable choice, and it is explicitly not a verdict: [`analyze`] drops such a sample rather
/// than reading it as total failure.
#[must_use]
pub fn sample(events: &[Vec<SessionEvent>], now_ms: u64) -> Sample {
    let mut healthy = 0usize;
    let mut errors = 0usize;
    // A `u32` total: the sum of scores is at most 100 per session, so it is exact in an `f64`, and
    // `saturating_add` keeps the arithmetic total on a batch large enough to overflow it.
    let mut total_score = 0u32;

    for session in events {
        let issues = diagnose_health(session);

        // The same rule as `is_healthy`, applied to the issues already in hand rather than walking the
        // log a third time. `tests/health_trend.rs` pins that the two agree.
        if issues
            .iter()
            .all(|issue| issue.severity == HealthSeverity::Info)
        {
            healthy += 1;
        }
        if issues
            .iter()
            .any(|issue| issue.severity == HealthSeverity::Error)
        {
            errors += 1;
        }

        total_score = total_score.saturating_add(u32::from(health_score(session)));
    }

    let sessions = events.len();
    Sample {
        at_ms: now_ms,
        sessions,
        healthy,
        errors,
        mean_score: if sessions == 0 {
            0.0
        } else {
            f64::from(total_score) / count_as_f64(sessions)
        },
    }
}

/// One half of the samples, pooled by session count.
#[derive(Default)]
struct Pool {
    /// Total sessions across the half.
    sessions: usize,

    /// Total healthy sessions across the half.
    healthy: usize,

    /// Each sample's mean score, weighted by that sample's session count.
    score: f64,
}

impl Pool {
    /// Pools a half, or `None` if it covered no sessions at all.
    ///
    /// `None` rather than a pool of zeroes because a rate needs a denominator: zero sessions have no
    /// rate, and reporting `0.0` would turn "nothing ran" into "nothing survived".
    fn of(samples: &[Sample]) -> Option<Self> {
        let mut pool = Self::default();
        for sample in samples {
            pool.sessions = pool.sessions.saturating_add(sample.sessions);
            pool.healthy = pool.healthy.saturating_add(sample.healthy);
            // Weighting by the session count is what makes the halves comparable when the windows are
            // of different sizes; a window with no sessions contributes nothing either way.
            pool.score += sample.mean_score * count_as_f64(sample.sessions);
        }
        if pool.sessions == 0 { None } else { Some(pool) }
    }

    /// The fraction of the half's sessions that were healthy.
    ///
    /// Safe to divide because [`Pool::of`] refuses a pool with no sessions.
    fn health_rate(&self) -> f64 {
        count_as_f64(self.healthy) / count_as_f64(self.sessions)
    }

    /// The session-weighted mean score of the half.
    fn mean_score(&self) -> f64 {
        // `score` is the weighted sum, so its unit is score-points times sessions; dividing by the
        // session count brings it back to points.
        self.score / count_as_f64(self.sessions)
    }
}

/// What the two deltas add up to.
///
/// A named verdict rather than two booleans, so the thresholds are applied in exactly one place and
/// the summary cannot disagree with [`Trend::degrading`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Worse than before by more than the noise floor.
    Degrading,

    /// Better than before by more than the noise floor.
    Improving,

    /// A change too small to act on.
    Stable,
}

impl Verdict {
    /// Classifies a pair of deltas.
    ///
    /// Degrading is checked first: a half that both gained a point of health rate and lost ten points
    /// of mean score is not "improving" because one of the two numbers went up, and the ordering makes
    /// the pessimistic reading the one that survives a mixed signal.
    fn of(health_rate_delta: f64, mean_score_delta: f64) -> Self {
        if health_rate_delta <= -HEALTH_RATE_DROP || mean_score_delta <= -SCORE_DROP {
            Self::Degrading
        } else if health_rate_delta >= HEALTH_RATE_DROP || mean_score_delta >= SCORE_DROP {
            Self::Improving
        } else {
            Self::Stable
        }
    }

    /// The verdict as a clause that closes a sentence.
    fn phrase(self) -> &'static str {
        match self {
            Self::Degrading => "degrading",
            Self::Improving => "improving",
            Self::Stable => "stable within the noise floor",
        }
    }

    /// Renders the comparison as a sentence an operator can read.
    ///
    /// The two rates and the two scores are spelled out beside their deltas: a delta alone forces the
    /// reader to go and find the numbers it was computed from, which is exactly the work a summary is
    /// supposed to save. Percentages and points are kept distinct in the wording because they are
    /// different units and "down 13" is ambiguous without them.
    fn describe(
        self,
        before: &Pool,
        after: &Pool,
        health_rate_delta: f64,
        mean_score_delta: f64,
    ) -> String {
        format!(
            "health rate {:.1}% -> {:.1}% ({:+.1} points) over {} sessions before and {} after; \
             mean score {:.1} -> {:.1} ({:+.1}); {}",
            before.health_rate() * 100.0,
            after.health_rate() * 100.0,
            health_rate_delta * 100.0,
            before.sessions,
            after.sessions,
            before.mean_score(),
            after.mean_score(),
            mean_score_delta,
            self.phrase(),
        )
    }
}

/// A count as a float for a ratio, saturating above `u32::MAX`.
///
/// `count as f64` rounds above 2^53, and a linter is right to require that be acknowledged rather
/// than assumed away. The saturation is unreachable for session counts that fit in memory, and it
/// keeps the conversion total instead of panicking on a batch nobody could hold.
fn count_as_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}
