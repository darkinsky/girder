//! Exporting conversation counts as metrics, in Prometheus' text exposition format.
//!
//! [`crate::session_stats`] answers "how much did this conversation do". This answers "how much is the
//! fleet doing", which is a different question asked by a different consumer: a time-series database
//! scraping an endpoint every fifteen seconds, not a UI rendering one log.
//!
//! # Why cardinality is the design constraint
//!
//! A metric is a name plus a set of label pairs, and the cost of a series in Prometheus is the number
//! of distinct label *combinations* ever observed, not the number of samples retained. Labelling a
//! sample with `session="<id>"` therefore creates one series per conversation that has ever run, none of
//! which is ever deleted: an installation left running for a week accumulates series until the scrape
//! times out and the server runs out of memory, and the failure looks like a monitoring outage rather
//! than like the mislabelling that caused it.
//!
//! So the caller chooses the mode explicitly rather than getting a default:
//!
//! - [`to_samples`] labels each sample with the session id. That is correct for a per-session view --
//!   one process exporting its own conversation, or an operator debugging a single run -- and wrong for
//!   anything that aggregates many conversations.
//! - [`to_aggregate_samples`] drops the label entirely, producing instructions rather than labels: a
//!   single `girder.session.turns` series that every process reports and the server sums. This is the
//!   mode a long-lived exporter should use.
//!
//! Neither function decides for the caller. A default that quietly emitted session-labelled samples
//! would be used by exactly the exporter that cannot afford it, and a default that dropped the label
//! would silently hide which conversation a sample described. The two names make the choice visible at
//! the call site, which is the only place the number of conversations is known.
//!
//! # Why the output is text rather than a struct an encoder consumes
//!
//! The exposition format is the wire contract, and it is small. Rendering it here keeps the escaping and
//! the name normalisation in one auditable place instead of spread across an encoder, and lets a test
//! assert on the exact bytes a scraper will receive.
//!
//! # What this does not do
//!
//! No timestamps. Metrics are counters and gauges derived from a log that records no wall-clock time,
//! so the scraper's clock is the only one available and an exporter should let it stamp the samples.
//!
//! No histograms. [`crate::session_stats`] produces totals, and a histogram over a total is a constant.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use girder_core::SessionStats as Stats;

/// The metric prefix every sample name carries.
///
/// A namespace rather than a bare name so that an exporter can share a registry with other components:
/// `girder.session.turns` cannot collide with another library's `turns`.
pub const NAMESPACE: &str = "girder.session";

/// Whether a metric only ever increases.
///
/// Prometheus uses the type to reject queries that are meaningless for it: `rate()` over a gauge is a
/// question the server cannot answer, and `resets()` over a counter that never reset hides a restart. The
/// distinction is per prefix rather than per sample because it is a property of the name, and carrying it
/// on each [`Sample`] would let two samples of the same metric disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricType {
    /// A value that only ever increases, reported as a running total.
    Counter,
    /// A value that can go up or down, reported as it stands at scrape time.
    Gauge,
}

impl MetricType {
    /// The word this type is written as in a `# TYPE` line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// One metric sample: a name, a value, and the labels that distinguish it from its siblings.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The metric name, e.g. `girder.session.turns`.
    ///
    /// Dotted, not underscored. Dots are the convention every other metric system uses for a namespace
    /// hierarchy, and [`to_prometheus`] converts them on the way out; storing the converted form here
    /// would make the sample useless to anything that is not Prometheus.
    pub name: String,
    /// The value.
    ///
    /// `f64` because that is what every exposition format carries, including integer counters: Prometheus
    /// stores all samples as float64 and a `u64` would be rounded at 2^53 regardless.
    pub value: f64,
    /// Labels that distinguish this sample from others of the same metric.
    ///
    /// A `BTreeMap` rather than a `HashMap` so that rendering is deterministic: an unstable label order
    /// would make every scrape look like a change to anything diffing the output, and would make tests
    /// depend on hash iteration order.
    pub labels: BTreeMap<String, String>,
}

impl Sample {
    /// A sample with no labels.
    #[must_use]
    pub fn new(name: impl Into<String>, value: f64) -> Self {
        Self {
            name: name.into(),
            value,
            labels: BTreeMap::new(),
        }
    }

    /// This sample with one more label.
    ///
    /// Takes and returns `self` so a sample can be built without a mutable binding, which is how most
    /// samples in a conversion are produced: a base value plus the labels that identify it.
    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// The metric name as Prometheus will store it.
    ///
    /// Prometheus restricts names to `[a-zA-Z_:][a-zA-Z0-9_:]*`, and its own documentation says a dot is
    /// *converted* to an underscore on ingestion. Doing the conversion here rather than letting the server
    /// do it means the name a test asserts on is the name a query must use; a sample written as
    /// `girder.session.turns` would otherwise answer to `girder_session_turns` and the mismatch would only
    /// show up as a query returning nothing.
    ///
    /// Conversions, in order:
    ///
    /// - any character outside the allowed set becomes `_`, which covers the dot and also spaces, dashes
    ///   and non-ASCII that a caller might have put in a name;
    /// - a name that would start with a digit gets a leading `_`, because the grammar requires a letter,
    ///   `_`, or `:`;
    /// - an empty name becomes `_`, which is legal and, unlike an empty string, does not produce a line
    ///   that the parser rejects.
    #[must_use]
    pub fn prometheus_name(&self) -> String {
        sanitize_name(&self.name)
    }
}

/// The metric type of a name, chosen by its last dot-separated segment.
///
/// Almost everything this module emits is a counter: turns, steps, tool calls, messages and tokens are all
/// totals, and a caller exporting them repeatedly should export the new total rather than the delta --
/// Prometheus subtracts the previous value itself.
///
/// A name that does not match a known counter is reported as a gauge, since a total this function cannot
/// vouch for is safer described as a value that may move either way than as one that only increases. That
/// is also how a future non-monotonic metric, such as a gauge of currently open sessions, gets the right
/// type without changing this list: it simply is not in it.
///
/// The per-model `model_present` sample is a gauge for exactly that reason: a model that stops appearing
/// makes that value go from 1 to 0 between one export and the next, which is a decrease a counter must
/// never have.
#[must_use]
pub fn metric_type(name: &str) -> MetricType {
    // Matched on the suffix rather than on the full name so that a caller who renames the namespace still
    // gets the right type.
    const COUNTER_SUFFIXES: [&str; 9] = [
        "turns",
        "steps",
        "tool_calls",
        "unanswered_tool_calls",
        "failed_tool_calls",
        "user_messages",
        "assistant_messages",
        "input_tokens",
        "output_tokens",
    ];
    let suffix = name.rsplit('.').next().unwrap_or(name);
    if COUNTER_SUFFIXES.contains(&suffix) {
        MetricType::Counter
    } else {
        MetricType::Gauge
    }
}

/// Converts one conversation's statistics into samples labelled with `session_id`.
///
/// `session_id` is a label rather than part of the name because a name is a metric's identity: putting it
/// in the name would create a new metric per conversation, which no query language can aggregate over
/// without first discovering every name. Labels are the dimension Prometheus understands.
///
/// # Cardinality
///
/// The session label creates one time series per session id, and series are never deleted. Call this for
/// a per-session view only; for an exporter that reports every conversation in a process, call
/// [`to_aggregate_samples`] instead. The label is on all samples including the per-model one, so the cost
/// grows with `sessions * models`.
///
/// # Which samples
///
/// One sample per counter in [`Stats`], plus one `model_present` sample per model in `stats.models`. The
/// per-model samples carry a `model` label and are reused by the aggregate mode, where they are the only
/// remaining dimension.
#[must_use]
pub fn to_samples(stats: &Stats, session_id: &str) -> Vec<Sample> {
    let mut samples = samples_for(stats);
    for sample in &mut samples {
        sample
            .labels
            .insert("session".to_owned(), session_id.to_owned());
    }
    samples
}

/// Converts statistics into samples with no session label, for summing across conversations.
///
/// This is the mode an exporter should use. Every process reports the same series and the monitoring
/// server sums them, so the number of series is bounded by the number of models rather than by the number
/// of conversations. The trade-off is that a sample no longer says which conversation produced it, which
/// is the point: a per-session breakdown belongs in a query over the session log, not in a metric label.
///
/// Per-model samples are kept, because a model name is a bounded set chosen by configuration rather than a
/// value that grows with use -- the distinction that separates a label from a cardinality bomb.
#[must_use]
pub fn to_aggregate_samples(stats: &Stats) -> Vec<Sample> {
    samples_for(stats)
}

/// The samples every mode shares, before any session label is applied.
fn samples_for(stats: &Stats) -> Vec<Sample> {
    // Counts are `u64`; the cast to `f64` is the representation every exposition format uses and is exact
    // below 2^53, which a conversation's counts are by many orders of magnitude.
    let totals: [(&str, u64); 9] = [
        ("turns", stats.turns),
        ("steps", stats.steps),
        ("tool_calls", stats.tool_calls),
        ("unanswered_tool_calls", stats.unanswered_tool_calls),
        ("failed_tool_calls", stats.failed_tool_calls),
        ("user_messages", stats.user_messages),
        ("assistant_messages", stats.assistant_messages),
        ("input_tokens", stats.input_tokens),
        ("output_tokens", stats.output_tokens),
    ];

    let mut samples: Vec<Sample> = totals
        .into_iter()
        .map(|(suffix, value)| Sample::new(format!("{NAMESPACE}.{suffix}"), count_to_f64(value)))
        .collect();

    // One sample per model, valued at `1`, which is the same shape as Prometheus' own `up`: the metric does
    // not measure a quantity, it records that the set contains this member, so `count by (model)` answers
    // "which models ran" and summing across models cannot double-count turns this module never attributed.
    // A model name is a bounded set chosen by configuration, which is what keeps the dimension safe to label.
    for model in &stats.models {
        samples.push(
            Sample::new(format!("{NAMESPACE}.model_present"), count_to_f64(1))
                .with_label("model", model.clone()),
        );
    }

    samples
}

/// Renders samples in Prometheus' text exposition format.
///
/// The output is what a `text/plain; version=0.0.4` scrape returns: for each metric name, a `# HELP` line,
/// a `# TYPE` line, and then one line per sample.
///
/// # What the format requires, and what this does about it
///
/// - **Name conversion.** Dots become underscores, as described on [`Sample::prometheus_name`].
/// - **Label escaping.** A label value may be any UTF-8, but the format delimits with `"` and escapes with
///   `\`, so `\` becomes `\\`, `"` becomes `\"`, and a newline becomes `\n`. An unescaped newline would
///   end the line early and turn the rest of the value into a parse error.
/// - **Label order.** Sorted by key, so a scrape of unchanged data is byte-identical and a diff shows only
///   real changes.
/// - **Grouping.** Samples are grouped under their converted name even if the input interleaved them,
///   because a `# HELP`/`# TYPE` block applies to every following line of that name until the next one.
///   Here each name gets exactly one block, emitted before its first sample.
/// - **Values.** Rendered as decimal integers when the value is integral, which is what a counter should
///   look like: Prometheus accepts `12`, and printing `12.0` would suggest a precision the count does not
///   have. Non-finite values are rendered as `NaN`, `+Inf` and `-Inf`, the three spellings the format
///   defines, since `f64`'s display would write `inf`, which the parser rejects.
///
/// The returned string ends with a newline, or is empty when there are no samples.
#[must_use]
pub fn to_prometheus(samples: &[Sample]) -> String {
    // Grouped by the converted name, and ordered by it so the output does not depend on input order.
    let mut groups: BTreeMap<String, Vec<&Sample>> = BTreeMap::new();
    for sample in samples {
        groups
            .entry(sample.prometheus_name())
            .or_default()
            .push(sample);
    }

    // `write!` into a `String` cannot fail, but an `unwrap` here would be a lie about where the risk is.
    // `let _ =` states that the result is unreachable rather than unchecked, and keeps the no-unwrap rule
    // intact without an allow attribute.
    let mut out = String::new();
    for (name, group) in &groups {
        let metric_type = metric_type(&group[0].name);
        // Bound first: a call inside a format string is parsed as a named argument, and the `&` in it is not
        // part of that grammar.
        let help = help_for(&group[0].name);
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {}", metric_type.as_str());
        for sample in group {
            let _ = write!(out, "{name}");
            if !sample.labels.is_empty() {
                let mut first = true;
                for (key, value) in &sample.labels {
                    // Commas separate label pairs but not the first one from the name, so the separator
                    // depends on position rather than on a trailing strip.
                    out.push(if first { '{' } else { ',' });
                    first = false;
                    // Label *names* are sanitised the same way metric names are; the value is escaped, not
                    // sanitised, because a label value is free-form text and altering it would change what
                    // the sample reports.
                    let _ = write!(out, "{}=", sanitize_name(key));
                    write_quoted(&mut out, value);
                }
                out.push('}');
            }
            let _ = writeln!(out, " {}", format_value(sample.value));
        }
    }
    out
}

/// The `# HELP` text for a metric, looked up by suffix.
///
/// Written here rather than carried on [`Sample`] because the help text describes the metric, not the
/// sample: it is identical for every sample of a name, and duplicating it on each one would create a way
/// for two samples to disagree about what their shared metric means. An unknown name gets a generic line
/// rather than an empty one, since `# HELP` is required to be non-empty by some parsers and a metric with
/// no description is what a reader would otherwise have to infer.
fn help_for(name: &str) -> &'static str {
    let suffix = name.rsplit('.').next().unwrap_or(name);
    match suffix {
        "turns" => "Number of turns that closed at least one step",
        "steps" => "Number of steps that closed",
        "tool_calls" => "Number of tool calls that received a result",
        "unanswered_tool_calls" => "Number of tool calls whose result never arrived",
        "failed_tool_calls" => "Number of tool calls that failed",
        "user_messages" => "Number of user messages",
        "assistant_messages" => "Number of assistant messages",
        "input_tokens" => "Prompt tokens reported by the provider",
        "output_tokens" => "Output tokens reported by the provider",
        "model_present" => "1 when the model answered at least one turn in this conversation",
        _ => "Girder session metric",
    }
}

/// Applies the conversions described on [`Sample::prometheus_name`].
fn sanitize_name(name: &str) -> String {
    if name.is_empty() {
        return "_".to_owned();
    }
    let mut out: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == ':' {
                character
            } else {
                '_'
            }
        })
        .collect();
    // Prometheus' grammar does not allow a leading digit, and a name that starts with one is rejected at
    // the scrape rather than stored, so the fix has to happen before it leaves here.
    if out.starts_with(|character: char| character.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Writes a label value in the quoted, escaped form the format requires.
fn write_quoted(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            // Backslash first among the escapes: escaping it after adding the others would double-escape
            // the backslash that `\"` and `\n` introduce.
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            // The format defines only those three escapes. A carriage return is written literally, which
            // parsers accept inside a quoted value; an unescaped newline is the one that breaks a line.
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Renders a value in the format's spelling.
fn format_value(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "+Inf"
        } else {
            "-Inf"
        }
        .to_owned();
    }
    // `f64::fract` is zero for every integral value, including the large ones, so this renders `12` for a
    // count of twelve and `0.5` for a fraction. A fractional value uses the default rendering, which never
    // emits an exponent for the magnitudes a count can hold.
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

/// Converts a count to the `f64` a sample carries.
///
/// `u64 as f64` is lossy above 2^53, which a conversation's counts are not reachable by; the point of
/// routing it through one function is that the lossiness is stated once rather than assumed at each call
/// site. Saturating rather than wrapping if the cast were ever to fail: an unreachable maximum is a
/// clearer signal than a count that wrapped to a small number.
#[allow(
    clippy::cast_precision_loss,
    reason = "a conversation's counts stay far below 2^53, where an f64 holds every integer exactly"
)]
fn count_to_f64(value: u64) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_name_is_sanitized_rather_than_dropped() {
        assert_eq!(sanitize_name(""), "_");
    }

    #[test]
    fn a_name_starting_with_a_digit_gets_a_prefix() {
        assert_eq!(sanitize_name("1abc"), "_1abc");
    }

    #[test]
    fn non_finite_values_use_the_format_spellings() {
        assert_eq!(format_value(f64::NAN), "NaN");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
    }
}
