//! Tests for exporting conversation counts as Prometheus metrics.
//!
//! The assertions are on exact bytes rather than on substrings, because the exposition format is a wire
//! contract: a scraper parses lines, and a line that differs by an escape or a period is a different line.
//! The same reasoning applies to the values, which is why counts are compared exactly: a count of twelve
//! must render as exactly `12`, and a tolerance would let a rounding regression pass as correct.
//!
//! The converter is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It reaches for `crate::session_stats`,
//! which this test binary does not have, so that path is stood up here as an alias of the crate's own
//! re-export: the converter then sees the same `Stats` the rest of the crate publishes.

#![allow(
    clippy::float_cmp,
    reason = "counts below 2^53 convert to f64 exactly; a tolerance would hide a rounding regression"
)]

mod session_stats {
    pub use girder_core::SessionStats as Stats;
}

#[path = "../src/session_metrics.rs"]
#[allow(dead_code)]
mod session_metrics;

use std::collections::BTreeMap;

use session_metrics::{
    MetricType, Sample, metric_type, to_aggregate_samples, to_prometheus, to_samples,
};
use session_stats::Stats as SessionStats;

fn stats() -> SessionStats {
    SessionStats {
        turns: 12,
        steps: 30,
        tool_calls: 7,
        unanswered_tool_calls: 1,
        failed_tool_calls: 2,
        user_messages: 5,
        assistant_messages: 12,
        output_tokens: 900,
        input_tokens: 4000,
        models: vec!["deepseek-chat".to_owned()],
    }
}

/// The sample whose name ends in `suffix`, which is how a test picks one metric out of the vector without
/// depending on its position.
fn sample_suffix<'a>(samples: &'a [Sample], suffix: &str) -> &'a Sample {
    let wanted = format!("girder.session.{suffix}");
    samples
        .iter()
        .find(|sample| sample.name == wanted)
        .unwrap_or_else(|| panic!("no sample named {wanted}"))
}

#[test]
fn an_empty_conversation_still_produces_every_counter() {
    // Zero is a measurement -- the conversation ran and did nothing -- so the counters are emitted rather
    // than omitted, and a dashboard reading them does not have to distinguish "absent" from "zero".
    let samples = to_samples(&SessionStats::default(), "s1");

    assert_eq!(samples.len(), 9);
    assert!(samples.iter().all(|sample| sample.value == 0.0));
    assert!(
        samples
            .iter()
            .all(|sample| sample.labels.contains_key("session"))
    );
}

#[test]
fn one_conversation_is_labelled_with_its_session() {
    let samples = to_samples(&stats(), "s1");

    let turns = sample_suffix(&samples, "turns");
    assert_eq!(turns.value, 12.0);
    assert_eq!(turns.labels.get("session"), Some(&"s1".to_owned()));
    // The identifier is a label and not a name: a name containing it would be a new metric per session.
    assert!(!turns.name.contains("s1"));

    assert_eq!(sample_suffix(&samples, "input_tokens").value, 4000.0);
    assert_eq!(sample_suffix(&samples, "output_tokens").value, 900.0);
}

#[test]
fn the_aggregate_mode_omits_the_session_label() {
    // The high-cardinality escape hatch: the same totals, no per-session dimension, so the number of series
    // is bounded by the number of metrics rather than by the number of conversations ever run.
    let samples = to_aggregate_samples(&stats());

    assert_eq!(samples.len(), 10);
    assert!(
        samples
            .iter()
            .all(|sample| !sample.labels.contains_key("session"))
    );
    assert_eq!(sample_suffix(&samples, "turns").value, 12.0);
}

#[test]
fn each_model_gets_one_presence_sample() {
    let stats = SessionStats {
        models: vec!["a".to_owned(), "b".to_owned()],
        ..stats()
    };

    let samples = to_aggregate_samples(&stats);
    let models: Vec<&Sample> = samples
        .iter()
        .filter(|sample| sample.name == "girder.session.model_present")
        .collect();

    assert_eq!(models.len(), 2);
    assert!(models.iter().all(|sample| sample.value == 1.0));
    assert_eq!(models[0].labels.get("model"), Some(&"a".to_owned()));
    assert_eq!(models[1].labels.get("model"), Some(&"b".to_owned()));
}

#[test]
fn the_rendered_output_has_help_and_type_lines() {
    let rendered = to_prometheus(&to_samples(&stats(), "s1"));

    assert!(
        rendered.contains(
            "# HELP girder_session_turns Number of turns that closed at least one step\n"
        ),
        "missing or wrong HELP line in:\n{rendered}"
    );
    assert!(
        rendered.contains("# TYPE girder_session_turns counter\n"),
        "missing or wrong TYPE line in:\n{rendered}"
    );
    assert!(
        rendered.contains("girder_session_turns{session=\"s1\"} 12\n"),
        "missing or wrong sample line in:\n{rendered}"
    );
}

#[test]
fn dots_in_metric_names_become_underscores() {
    // Prometheus rewrites a dot to an underscore on ingestion. Doing it here means the name a query must use
    // is the name the test asserts on, instead of a silent mismatch discovered as an empty query result.
    let samples = vec![Sample::new("girder.session.turns", 1.0)];
    let rendered = to_prometheus(&samples);

    assert_eq!(samples[0].prometheus_name(), "girder_session_turns");
    assert!(rendered.contains("# HELP girder_session_turns "));
    assert!(!rendered.contains("girder.session"));
}

#[test]
fn a_four_part_name_keeps_every_segment() {
    let samples = [Sample::new("girder.session.a.b", 1.0)];

    assert_eq!(samples[0].prometheus_name(), "girder_session_a_b");
}

#[test]
fn a_name_with_illegal_characters_is_repaired() {
    // A hyphen, a space and a non-ASCII character are all outside the grammar; a name starting with a digit
    // does not satisfy it either. Each is rewritten rather than passed through to be rejected at scrape time.
    assert_eq!(
        Sample::new("girder.session.turns-1", 1.0).prometheus_name(),
        "girder_session_turns_1"
    );
    assert_eq!(
        Sample::new("girder session turns", 1.0).prometheus_name(),
        "girder_session_turns"
    );
    assert_eq!(
        Sample::new("girder.会话", 1.0).prometheus_name(),
        "girder___"
    );
    assert_eq!(Sample::new("1turns", 1.0).prometheus_name(), "_1turns");
    assert_eq!(Sample::new("", 1.0).prometheus_name(), "_");
}

#[test]
fn label_values_are_escaped() {
    let samples = vec![Sample::new("girder.session.turns", 1.0).with_label(
        "session",
        // One value carrying all three escapes, so the assertions below read against a single rendered line
        // instead of three separate ones.
        "a\"b\\c\nd",
    )];

    let rendered = to_prometheus(&samples);

    assert!(
        rendered.contains("girder_session_turns{session=\"a\\\"b\\\\c\\nd\"} 1\n"),
        "escapes not applied in:\n{rendered}"
    );
    // The newline must not have reached the output raw: an unescaped one ends the line early and turns the
    // remainder of the value into a parse error.
    assert_eq!(rendered.lines().count(), 3);
}

#[test]
fn label_order_is_stable() {
    // Two samples of one metric must render identically however the labels were inserted, or every scrape
    // would look like a change to anything diffing the output.
    let first = Sample::new("girder.session.turns", 1.0)
        .with_label("session", "s1")
        .with_label("model", "m");
    let second = Sample::new("girder.session.turns", 1.0)
        .with_label("model", "m")
        .with_label("session", "s1");

    let rendered = to_prometheus(&[first.clone(), second]);

    let expected = "girder_session_turns{model=\"m\",session=\"s1\"} 1\n";
    assert_eq!(rendered.matches(expected).count(), 2);
    assert_eq!(
        first.labels,
        BTreeMap::from([
            ("model".to_owned(), "m".to_owned()),
            ("session".to_owned(), "s1".to_owned()),
        ])
    );
}

#[test]
fn samples_of_one_metric_share_one_help_block() {
    let samples = vec![
        Sample::new("girder.session.turns", 1.0).with_label("session", "s1"),
        Sample::new("girder.session.turns", 2.0).with_label("session", "s2"),
        Sample::new("girder.session.steps", 3.0).with_label("session", "s1"),
    ];

    let rendered = to_prometheus(&samples);

    assert_eq!(rendered.matches("# HELP girder_session_turns").count(), 1);
    assert_eq!(rendered.matches("# TYPE girder_session_turns").count(), 1);
    assert_eq!(rendered.matches("# HELP girder_session_steps").count(), 1);
    // Grouped by name, so the two turns samples are adjacent lines under their own header block rather
    // than split apart by the steps sample that arrived between them in the input.
    let lines: Vec<&str> = rendered.lines().collect();
    let turns = lines
        .iter()
        .position(|line| line.starts_with("# HELP girder_session_turns"))
        .unwrap_or_else(|| panic!("no turns header in:\n{rendered}"));

    assert!(lines[turns + 1].starts_with("# TYPE girder_session_turns"));
    assert!(lines[turns + 2].contains("session=\"s1\""));
    assert!(lines[turns + 3].contains("session=\"s2\""));
}

#[test]
fn an_empty_sample_list_renders_nothing() {
    // Not a lone newline: an empty body is what a scrape with no metrics returns, and a blank line is a
    // parse error in the exposition format.
    assert_eq!(to_prometheus(&[]), "");
}

#[test]
fn counters_and_gauges_are_distinguished_by_name() {
    assert_eq!(metric_type("girder.session.turns"), MetricType::Counter);
    assert_eq!(
        metric_type("girder.session.unanswered_tool_calls"),
        MetricType::Counter
    );
    assert_eq!(
        metric_type("girder.session.model_present"),
        MetricType::Gauge
    );
    assert_eq!(metric_type("turns"), MetricType::Counter);

    let rendered = to_prometheus(&to_aggregate_samples(&stats()));
    assert!(rendered.contains("# TYPE girder_session_model_present gauge\n"));
    assert!(rendered.contains("# TYPE girder_session_tool_calls counter\n"));
}

#[test]
fn integral_values_render_without_a_decimal_point() {
    // A counter is a count; `12.0` would suggest a precision the number does not have, and some tooling
    // parses the value with an integer type.
    let rendered = to_prometheus(&[Sample::new("girder.session.turns", 12.0)]);

    assert!(
        rendered.ends_with("} 12\n") || rendered.ends_with(" 12\n"),
        "{rendered}"
    );
    assert!(!rendered.contains("12.0"));
}

#[test]
fn fractional_and_non_finite_values_use_format_spellings() {
    let rendered = to_prometheus(&[Sample::new("girder.session.average", 0.5)]);
    assert!(rendered.ends_with(" 0.5\n"), "{rendered}");

    for (value, expected) in [
        (f64::NAN, " NaN\n"),
        (f64::INFINITY, " +Inf\n"),
        (f64::NEG_INFINITY, " -Inf\n"),
    ] {
        let rendered = to_prometheus(&[Sample::new("girder.session.average", value)]);
        assert!(
            rendered.ends_with(expected),
            "expected {expected:?} in {rendered:?}"
        );
    }
}

#[test]
fn the_full_conversation_renders_one_block_per_metric() {
    let rendered = to_prometheus(&to_samples(&stats(), "s1"));

    // Nine counters plus one model presence sample, each with its own HELP and TYPE pair: 10 metrics, 20
    // header lines, 10 sample lines.
    assert_eq!(rendered.matches("# HELP ").count(), 10);
    assert_eq!(rendered.matches("# TYPE ").count(), 10);
    assert_eq!(rendered.lines().count(), 30);
    assert!(rendered.ends_with('\n'));
}
