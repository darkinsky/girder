//! Tests for trend analysis over a window of samples.
//!
//! The property this module exists for is the one the median is there to provide: an outlier must not be able to
//! hide itself. The naive rule -- flag anything more than N times the mean -- is computed over the same values
//! that contain the anomaly, so a large enough sample raises the mean until it falls back inside the band. The
//! test `an_outlier_does_not_shift_the_median` pins that directly, and `outliers_finds_a_planted_outlier` pins the
//! rule that depends on it.
//!
//! Float assertions use an explicit tolerance rather than `==`, including where the exact answer is representable:
//! the point of these tests is the formula, and a bit-exact assertion would fail on a mathematically equivalent
//! rewrite (a different summation order, or the midpoint written as `(a + b) / 2`).
//!
//! # Why the module is compiled by path
//!
//! The trend module is not exported from the crate root yet; that wiring is a separate change to `lib.rs`.
//! Compiling the exact source file here -- rather than a copy -- keeps these tests exercising the same code a
//! consumer will reach through `girder_core::stats_trend`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/stats_trend.rs"]
mod stats_trend;

use stats_trend::{Point, Trend, analyze, compare, outliers};

/// A point at a synthetic time; only `value` matters to any statistic here.
fn point(at_ms: u64, value: f64) -> Point {
    Point { at_ms, value }
}

/// A window of values, timestamped from zero in one-millisecond steps.
fn window(values: &[f64]) -> Vec<Point> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            // A millisecond per sample keeps the timestamps distinct; no function reads them.
            point(u64::try_from(index).unwrap_or(u64::MAX), *value)
        })
        .collect()
}

/// Whether two floats agree to within a tolerance small enough to catch a wrong formula and large enough to
/// survive a reordering of the same arithmetic.
fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-9
}

// ---- analyze -----------------------------------------------------------------------------------

#[test]
fn analyze_of_no_samples_is_none() {
    // Not a zero trend: an average over nothing is undefined, and a caller that rendered it would be showing a
    // measurement it does not have.
    assert!(analyze(&[]).is_none());
}

#[test]
fn analyze_of_one_sample_is_that_sample() {
    let trend = analyze(&window(&[42.0])).expect("one sample has a summary");

    assert_eq!(trend.samples, 1);
    assert!(close(trend.mean, 42.0));
    assert!(close(trend.median, 42.0));
    // A single point cannot vary. This is the case the sample estimator (`n - 1`) would divide by zero on.
    assert!(close(trend.stddev, 0.0));
    assert!(close(trend.min, 42.0));
    assert!(close(trend.max, 42.0));
}

#[test]
fn analyze_of_even_count_medians_between_the_middle_two() {
    // Four values: the middle two are 20 and 40, so the median is their midpoint, 30 -- not either one, and not
    // the mean's 30 by coincidence. 10 and 60 are chosen to make the two disagree if the wrong pair is taken.
    let trend = analyze(&window(&[10.0, 20.0, 40.0, 60.0])).expect("four samples have a summary");

    assert_eq!(trend.samples, 4);
    assert!(close(trend.median, 30.0));
    assert!(close(trend.mean, 32.5));
}

#[test]
fn analyze_of_odd_count_medians_the_middle_value() {
    let trend =
        analyze(&window(&[10.0, 20.0, 40.0, 60.0, 1_000.0])).expect("five samples have a summary");

    assert_eq!(trend.samples, 5);
    assert!(close(trend.median, 40.0));
    // The mean is dragged by the large sample; the median is not. That gap is the reason both are reported.
    assert!(close(trend.mean, 226.0));
}

#[test]
fn analyze_is_order_independent() {
    let ascending = analyze(&window(&[1.0, 2.0, 3.0, 4.0, 5.0])).expect("samples have a summary");
    let shuffled = analyze(&window(&[4.0, 1.0, 5.0, 3.0, 2.0])).expect("samples have a summary");

    assert!(close(ascending.median, shuffled.median));
    assert!(close(ascending.mean, shuffled.mean));
    assert!(close(ascending.stddev, shuffled.stddev));
}

#[test]
fn analyze_reports_min_and_max() {
    let trend = analyze(&window(&[7.0, -3.0, 12.0, 0.5])).expect("samples have a summary");

    assert!(close(trend.min, -3.0));
    assert!(close(trend.max, 12.0));
}

#[test]
fn analyze_computes_population_standard_deviation() {
    // Deviation from the mean of 3 is (-2, -1, 0, 1, 2); the squares sum to 10 and divide by n = 5 for a variance
    // of 2, so the population standard deviation is sqrt(2). The sample estimator would give sqrt(2.5) = 1.5811,
    // so this assertion distinguishes the two.
    let trend = analyze(&window(&[1.0, 2.0, 3.0, 4.0, 5.0])).expect("samples have a summary");

    assert!(close(trend.mean, 3.0));
    assert!(close(trend.stddev, 2.0_f64.sqrt()), "got {}", trend.stddev);
    assert!(!close(trend.stddev, (2.5_f64).sqrt()));
}

#[test]
fn analyze_of_identical_samples_has_no_spread() {
    let trend = analyze(&window(&[6.0, 6.0, 6.0, 6.0])).expect("samples have a summary");

    assert!(close(trend.stddev, 0.0));
    assert!(close(trend.mean, trend.median));
}

// ---- the property the median exists for --------------------------------------------------------

#[test]
fn an_outlier_does_not_shift_the_median() {
    // Nine conversations at 100 tokens and one at 100,000. The mean jumps to 10,090 -- the outlier has dragged
    // the centre of the distribution to within 10x of itself, which is what lets a "10x the mean" rule miss it.
    // The median stays at 100.
    let trend = analyze(&window(&[
        100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100_000.0,
    ]))
    .expect("samples have a summary");

    assert!(close(trend.median, 100.0), "median was {}", trend.median);
    assert!(trend.mean > 10_000.0, "mean was {}", trend.mean);

    // The same window without the outlier leaves the median where it was; this is the assertion that says the
    // stability above is a property of the median rather than of these particular numbers.
    let without = analyze(&window(&[100.0; 9])).expect("samples have a summary");
    assert!(close(without.median, trend.median));
}

#[test]
fn outliers_finds_a_planted_outlier() {
    // One sample at 1,000 against nine at 100. Centred on the median of 100, the outlier is 900 away; the
    // standard deviation of the window is ~285, so it sits about 3.2 deviations out and a threshold of 3 catches
    // it. A mean-centred band would be 280 wide and a "larger than 3x the mean" rule (mean ~190) would not.
    let points = window(&[
        100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 1_000.0,
    ]);

    let found = outliers(&points, 3.0);

    assert_eq!(found.len(), 1, "found {found:?}");
    assert!(close(found[0].value, 1_000.0));
    // Borrowed, not copied: the caller can locate the sample in its own data through this reference.
    assert!(std::ptr::eq(found[0], &raw const points[9]));
}

#[test]
fn outliers_returns_input_order() {
    // Two outliers, the earlier one below the median and the later one above it, against twenty ordinary samples.
    // Both must come back, and in the order the caller passed them rather than sorted by magnitude, so a caller
    // pairing them with something else does not have to re-sort.
    //
    // Twenty ordinary samples rather than a handful because the standard deviation is taken over the window that
    // contains the candidates: each outlier widens the band it is measured against, so a short window with two of
    // them would need a threshold low enough to make this a test of the threshold rather than of the ordering.
    let mut values: Vec<f64> = vec![-900.0];
    values.extend(std::iter::repeat_n(100.0, 20));
    values.push(1_000.0);
    let points = window(&values);

    let found = outliers(&points, 2.0);

    assert_eq!(found.len(), 2, "found {found:?}");
    assert!(
        close(found[0].value, -900.0),
        "first was {}",
        found[0].value
    );
    assert!(
        close(found[1].value, 1_000.0),
        "second was {}",
        found[1].value
    );
    // Both are the same points the caller passed in, in the same relative order.
    assert!(std::ptr::eq(found[0], &raw const points[0]));
    assert!(std::ptr::eq(found[1], &raw const points[points.len() - 1]));
}

#[test]
fn outliers_of_a_flat_window_is_empty() {
    // No spread means a bound of zero, and every deviation is also zero -- nothing is *greater* than zero, so a
    // perfectly steady window reports no anomalies rather than all of them.
    let points = window(&[5.0, 5.0, 5.0, 5.0]);

    assert!(outliers(&points, 1.0).is_empty());
}

#[test]
fn outliers_of_no_samples_is_empty() {
    assert!(outliers(&[], 3.0).is_empty());
}

#[test]
fn outliers_ignores_a_non_positive_or_non_finite_threshold() {
    let points = window(&[1.0, 1.0, 1.0, 1_000.0]);

    // A zero or negative threshold has no meaning, and NaN fails every comparison. An empty result keeps a
    // mis-specified call visible as "nothing found" instead of as an alarm on every sample.
    assert!(outliers(&points, 0.0).is_empty());
    assert!(outliers(&points, -1.0).is_empty());
    assert!(outliers(&points, f64::NAN).is_empty());
    assert!(outliers(&points, f64::INFINITY).is_empty());
}

#[test]
fn a_higher_threshold_selects_no_more_than_a_lower_one() {
    // Monotonicity, which a percentage-based rule does not have: widening the band can only ever drop points.
    let points = window(&[100.0, 105.0, 95.0, 400.0, 102.0, 98.0]);

    let loose = outliers(&points, 5.0).len();
    let tight = outliers(&points, 1.0).len();

    assert!(loose <= tight, "loose {loose} exceeded tight {tight}");
}

// ---- compare -----------------------------------------------------------------------------------

#[test]
fn compare_reports_the_percentage_change() {
    let before = window(&[100.0, 200.0]); // mean 150
    let after = window(&[300.0, 300.0]); // mean 300

    let comparison = compare(&before, &after).expect("both windows have means");

    assert!(close(comparison.before, 150.0));
    assert!(close(comparison.after, 300.0));
    assert!(
        close(comparison.change_pct, 100.0),
        "got {}",
        comparison.change_pct
    );
}

#[test]
fn compare_reports_a_decrease_as_negative() {
    let before = window(&[200.0]);
    let after = window(&[50.0]);

    let comparison = compare(&before, &after).expect("both windows have means");

    assert!(
        close(comparison.change_pct, -75.0),
        "got {}",
        comparison.change_pct
    );
}

#[test]
fn compare_of_equal_windows_is_no_change() {
    let comparison =
        compare(&window(&[10.0, 20.0]), &window(&[20.0, 10.0])).expect("both windows have means");

    assert!(close(comparison.change_pct, 0.0));
}

#[test]
fn compare_with_a_zero_baseline_is_none() {
    // The core guard: a zero `before` mean divides to infinity, and a caller formatting `+inf%` would be showing
    // a number that is not a measurement. `None` forces the caller to decide what to display instead.
    assert!(compare(&window(&[0.0, 0.0]), &window(&[10.0])).is_none());
}

#[test]
fn compare_with_an_empty_window_is_none() {
    // Both directions, because "before" and "after" are different arguments and a guard that only checks one of
    // them would pass a one-sided test.
    assert!(compare(&[], &window(&[10.0])).is_none());
    assert!(compare(&window(&[10.0]), &[]).is_none());
    assert!(compare(&[], &[]).is_none());
}

#[test]
fn compare_with_a_negative_baseline_is_none() {
    // A percentage change around a negative baseline inverts the sign: a rise from -100 to -50 would report as
    // -50%, reading as a drop. Rejected rather than supported, since a reader could not act on it.
    assert!(compare(&window(&[-100.0]), &window(&[-50.0])).is_none());
}

// ---- the round trip a caller actually makes ----------------------------------------------------

#[test]
fn a_window_can_be_summarised_flagged_and_compared() {
    // Not a unit test of one function: it is the sequence an operator dashboard performs, to catch an API shape
    // that is usable per function but awkward in combination.
    let yesterday = window(&[
        100.0, 101.0, 99.0, 100.0, 98.0, 102.0, 100.0, 99.0, 101.0, 100.0,
    ]);
    let mut today_values: Vec<f64> = yesterday.iter().map(|point| point.value).collect();
    today_values.push(1_000.0);
    let today = window(&today_values);

    let summary: Trend = analyze(&today).expect("today has samples");
    let flagged = outliers(&today, 3.0);
    let comparison = compare(&yesterday, &today).expect("neither window is empty or zero-mean");

    // The planted sample is the only one outside the band.
    assert_eq!(flagged.len(), 1, "flagged {flagged:?}");
    assert!(close(flagged[0].value, 1_000.0));
    // The median is unmoved by it, so the summary still describes the typical conversation.
    assert!(
        close(summary.median, 100.0),
        "median was {}",
        summary.median
    );
    // And the rise is visible in the comparison, tempered by the same outlier sitting in the mean.
    assert!(comparison.change_pct > 0.0, "got {}", comparison.change_pct);
}
