//! Trends over a sequence of samples: is today busier than yesterday, and which conversation is the odd one out.
//!
//! `girder_core::session_stats` counts a *single* conversation: turns, steps, tool calls, tokens. That answers
//! "how big was this one". It cannot answer the two questions an operator actually asks next, because both need
//! more than one conversation:
//!
//! - "How much more did we use today than yesterday?" needs two windows and a ratio between them.
//! - "Which conversation burned ten times the usual?" needs a notion of *usual* computed across many
//!   conversations, and a rule for when a departure from it is worth reporting.
//!
//! The metric exporter answers neither. It renders counts for a scraper, so the arithmetic over a *window* of
//! samples has to happen either in `PromQL` or in Rust; this module is the Rust side, where it can be tested.
//!
//! # Why the median, not the mean, decides what is abnormal
//!
//! The obvious rule -- "flag anything more than N times the average" -- is broken in a way that is easy to miss.
//! The average is computed *over the same values that include the anomaly*, so a large enough outlier raises the
//! average toward itself and can hide inside the band it created: with nine samples at 100 and one at 100,000 the
//! average is 10,090, and a "10x the average" rule then clears everything. The median is unmoved by that one
//! sample (it stays 100), so a band drawn around it still selects the outlier. The same property makes
//! [`Comparison`] readable: a window's mean is dragged by its worst conversation, so [`compare`] reports it as
//! what it is -- a mean -- while [`Trend`] carries the median alongside for the caller who needs the typical case.
//!
//! Spread is measured as a standard deviation in units of the same values, which is why the threshold in
//! [`outliers`] is a number of standard deviations rather than a percentage: a fixed percentage is too tight for
//! a noisy workload and too loose for a steady one, and the same 20% means different things in each.
//!
//! # What this does not do
//!
//! No bucketing and no windowing: a caller decides what "today" means and passes the samples, because only the
//! caller knows the time zone that question is asked in. Nothing here reads a clock either -- [`Point::at_ms`] is
//! carried so a caller can plot or bucket the same values, and every statistic here is order-independent.
//!
//! Non-finite values are not filtered out. A NaN in the input propagates into [`Trend::mean`] and
//! [`Trend::stddev`] rather than being silently dropped, because a dropped sample changes the denominator and
//! turns a data problem into a wrong answer that looks fine.

/// A single sample at a point in time.
///
/// Time is carried, not used: no function here reads [`Point::at_ms`], because ordering and bucketing are the
/// caller's decisions. It travels with the value so [`outliers`] can hand back points a caller can place on a
/// chart or map back to the conversation they describe, instead of bare floats with the context stripped off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    /// Time (in milliseconds).
    ///
    /// An absolute or relative instant, whichever the caller has: no function here compares two of them.
    pub at_ms: u64,
    /// The value.
    pub value: f64,
}

/// A summary of a window of samples.
///
/// The median is here next to the mean on purpose, and it is not redundant. They answer different questions --
/// "what did an average sample look like" versus "what did a typical sample look like" -- and the gap between
/// them is itself a signal: a mean far above the median says the window is dominated by a few large samples,
/// which is exactly the shape of a workload with one runaway conversation in it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trend {
    /// How many samples the summary was computed from.
    ///
    /// Carried because the same mean means different things at 3 samples and at 3,000: at three, one sample is a
    /// third of the answer, and a caller weighing whether to trust the band needs to know which case it is in.
    pub samples: usize,
    /// Arithmetic mean.
    pub mean: f64,
    /// Median.
    ///
    /// The middle value, or the midpoint of the two middle values for an even count. Chosen as the centre for
    /// anomaly detection because it resists the very samples the detection is looking for.
    pub median: f64,
    /// Population standard deviation (divided by `n`, not `n - 1`).
    ///
    /// # Why population rather than sample
    ///
    /// The samples passed to [`analyze`] are the whole window being described, not a random draw from a larger
    /// population that someone is trying to estimate. The window is the subject, so there is no correction to
    /// make. The practical argument points the same way: the sample estimator divides by `n - 1`, which is a
    /// division by zero at a single sample -- the one case a caller is most likely to hand in -- while the
    /// population estimator reports `0.0` there, which is true: one point cannot vary. A caller who does want
    /// the sample estimator can scale by `sqrt(n / (n - 1))`; that is not recoverable if it is baked in.
    pub stddev: f64,
    /// Smallest value in the window.
    pub min: f64,
    /// Largest value in the window.
    pub max: f64,
}

/// Summarises a window of samples, or `None` if it is empty.
///
/// `None` rather than a [`Trend`] of zeroes: an average over no samples is undefined, not zero, and an all-zero
/// trend is indistinguishable from a genuinely idle window. A caller that displayed it would be reporting a
/// measurement it does not have -- and would then divide by that zero mean in [`compare`].
#[must_use]
pub fn analyze(points: &[Point]) -> Option<Trend> {
    // Copy rather than read in place, because the median needs an ordered sequence and the caller's slice is not
    // ours to reorder.
    let mut values: Vec<f64> = points.iter().map(|point| point.value).collect();
    let mean = mean(&values)?;

    // `total_cmp` rather than `partial_cmp`: it is a total order over every `f64` including NaN and `-0.0`, so the
    // sort cannot fail and there is no comparison result to discard. Filtering the NaNs out first would be worse
    // than ordering them, because it would change the count the mean is divided by.
    values.sort_by(f64::total_cmp);

    // The slice is non-empty: `mean` returned `Some`, which it only does for a non-empty input.
    let middle = values.len() / 2;
    let median = if values.len() % 2 == 1 {
        values[middle]
    } else {
        // Midpoint written as `lower + half the gap` rather than `(lower + upper) / 2`, because the sum of two
        // large values overflows to infinity while the gap between them is small by definition.
        let lower = values[middle - 1];
        lower + (values[middle] - lower) / 2.0
    };

    // Population variance: the sum of squared deviations from the mean, divided by `n`.
    let variance = values
        .iter()
        .map(|value| {
            let deviation = value - mean;
            deviation * deviation
        })
        .sum::<f64>()
        / count_as_f64(values.len());

    Some(Trend {
        samples: values.len(),
        mean,
        median,
        stddev: variance.sqrt(),
        min: values[0],
        max: values[values.len() - 1],
    })
}

/// Finds the points that depart from the window's median by more than `threshold` standard deviations.
///
/// The band is centred on the **median** and sized by the window's standard deviation. Centring on the median is
/// the point: a band centred on the mean is dragged toward the outlier by the outlier itself, so the further out
/// the anomaly sits the wider the band it gets to hide behind. The median does not move, so the same sample stays
/// outside the band regardless of how extreme it is.
///
/// `threshold` is in standard deviations, not percent, and must be positive and finite; anything else selects
/// nothing. A threshold of zero would flag every point that is not exactly the median (and, on a flat window,
/// every point, since a deviation of zero is not *greater* than a bound of zero), and a negative or NaN
/// threshold has no meaning at all -- returning an empty set keeps a mis-specified call visible as "nothing
/// found" instead of as an alarm on every sample.
///
/// Returned in input order, borrowed rather than copied so a caller can find the sample again.
///
/// # Caveat
///
/// The standard deviation is computed over the same window that contains the candidate outliers, so a single
/// gross outlier still widens the band slightly -- less than it moves a mean, but not nothing. A threshold of
/// three or below is where the median fully pays off; a threshold large enough to clear a truly extreme sample
/// can hide a moderate one next to it.
#[must_use]
pub fn outliers(points: &[Point], threshold: f64) -> Vec<&Point> {
    if !threshold.is_finite() || threshold <= 0.0 {
        return Vec::new();
    }
    // A window with no spread -- one sample, or many identical ones -- yields a bound of zero and a median equal
    // to every value, so nothing is selected below without a special case. The check is still needed for the
    // empty window, where there is no median to measure against.
    let Some(trend) = analyze(points) else {
        return Vec::new();
    };

    let bound = threshold * trend.stddev;
    points
        .iter()
        .filter(|point| (point.value - trend.median).abs() > bound)
        .collect()
}

/// A comparison of two consecutive windows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Comparison {
    /// The earlier window's mean.
    pub before: f64,
    /// The later window's mean.
    pub after: f64,
    /// Change in percent, relative to `before`: `(after - before) / before * 100`.
    ///
    /// A percentage rather than a difference because the windows may not be the same length -- an hour against a
    /// day -- and an absolute delta would be unreadable across such a pair.
    pub change_pct: f64,
}

/// Compares the mean of an earlier window with the mean of a later one, or `None` if they cannot be compared.
///
/// `None` when either window is empty (a mean over nothing is undefined) and when `before` has no usable
/// baseline: a mean of zero divides to infinity, a subnormal one underflows to it, a negative one inverts the
/// sign of the result so that a drop reports as a rise, and NaN fails every comparison. Returning `+inf` or a
/// sign-flipped number would be worse than returning nothing, because a caller formatting it would print
/// "+inf%" or "-40% growth" as if it were a measurement.
///
/// Non-negative counts are the intended input; the negative case is rejected rather than supported because a
/// percentage change around a negative baseline has no interpretation a reader could act on.
#[must_use]
pub fn compare(before: &[Point], after: &[Point]) -> Option<Comparison> {
    let previous = mean_of(before)?;
    let current = mean_of(after)?;

    if !previous.is_finite() || previous <= 0.0 {
        return None;
    }

    Some(Comparison {
        before: previous,
        after: current,
        change_pct: (current - previous) / previous * 100.0,
    })
}

/// The mean of a window of points, or `None` if it is empty.
fn mean_of(points: &[Point]) -> Option<f64> {
    let values: Vec<f64> = points.iter().map(|point| point.value).collect();
    mean(&values)
}

/// The arithmetic mean of a non-empty slice, or `None` if it is empty.
fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / count_as_f64(values.len()))
}

/// A sample count as `f64`.
///
/// `usize as f64` rounds above 2^53 and a linter is right to require that be acknowledged rather than assumed, as
/// it is elsewhere in this crate. The clamp only ever bites for a window with more samples than a `u32` can
/// count, where a statistics result is not the problem worth reporting.
fn count_as_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}
