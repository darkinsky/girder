//! Tests for the request trace tree.
//!
//! The assertions are on exact milliseconds because the clock is injected: a test that had to sleep to
//! observe a duration would be a test that fails on a loaded machine. Every timestamp below is a number
//! chosen by the test, so equal numbers mean equal behaviour on every run.
//!
//! Three properties get the most attention, because each one is a mistake that reads as correct:
//!
//! - an unfinished span reports `None`, not `0` — a hung tool must not look instant;
//! - children may exceed their parent — parallel work makes the sum of child durations a poor estimate
//!   of the parent's, and the module must not quietly reconcile them;
//! - `render` shows depth — a flat list of numbers cannot answer "which tool call was in which step".
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#![allow(
    clippy::indexing_slicing,
    reason = "a test asserting a tree shape indexes it; an out-of-range index is a failed test, not a panic to design around"
)]

#[path = "../src/trace_span.rs"]
mod trace_span;

use trace_span::{Span, Tracer};

/// Builds a finished span, so a test that only cares about the tree need not repeat two timestamps.
fn spanned(name: &str, start: u64, end: u64) -> Span {
    let mut span = Span::start(name, start);
    span.end(end);
    span
}

#[test]
fn a_finished_span_reports_its_duration() {
    let span = spanned("turn", 1_000, 1_250);

    assert_eq!(span.duration_ms(), Some(250));
    assert!(span.is_ended());
}

#[test]
fn a_span_that_has_not_ended_has_no_duration() {
    // The core distinction of the module: `None` forces a consumer to decide, where `0` would claim the
    // span was instantaneous. The assertion is on `None` specifically, because `Some(0)` would be the
    // regression and would compare equal to a genuinely fast span.
    let span = Span::start("tool:bash", 500);

    assert_eq!(span.duration_ms(), None);
    assert!(!span.is_ended());
}

#[test]
fn ending_a_span_twice_keeps_the_first_end() {
    // A caller that ends a span on both a success and an error path must not be able to overwrite the
    // measurement that actually happened.
    let mut span = Span::start("turn", 100);
    span.end(200);
    span.end(900);

    assert_eq!(span.duration_ms(), Some(100));
}

#[test]
fn a_span_that_starts_and_ends_at_the_same_instant_takes_zero() {
    // The other half of the distinction: a real zero is representable, and is not the same as `None`.
    let span = spanned("noop", 42, 42);

    assert_eq!(span.duration_ms(), Some(0));
}

#[test]
fn a_clock_that_goes_backwards_does_not_underflow() {
    // Injected clocks can be wrong. An enormous duration would look like a hung span; zero says "this
    // measurement is useless" without pretending to a number.
    let span = spanned("misconfigured", 900, 100);

    assert_eq!(span.duration_ms(), Some(0));
}

#[test]
fn nested_spans_keep_their_parent() {
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("step", 10);
    tracer.begin("tool:bash", 20);
    assert!(tracer.end(70));
    assert!(tracer.end(90));
    assert!(tracer.end(100));

    assert_eq!(tracer.open_depth(), 0);
    let roots = tracer.roots();
    assert_eq!(roots.len(), 1);

    let turn = &roots[0];
    assert_eq!(turn.name, "turn");
    assert_eq!(turn.duration_ms(), Some(100));
    assert_eq!(turn.children.len(), 1);

    let step = &turn.children[0];
    assert_eq!(step.name, "step");
    assert_eq!(step.duration_ms(), Some(80));

    let tool = &step.children[0];
    assert_eq!(tool.name, "tool:bash");
    assert_eq!(tool.duration_ms(), Some(50));
    assert!(tool.children.is_empty());
}

#[test]
fn siblings_belong_to_the_same_parent() {
    // Several children under one step is the ordinary shape of a step that called three tools, and it
    // must not look like a chain.
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("step", 1);
    tracer.begin("tool:read", 2);
    tracer.end(12);
    tracer.begin("tool:bash", 3);
    tracer.end(33);
    tracer.begin("tool:edit", 4);
    tracer.end(14);
    tracer.end(40);
    tracer.end(50);

    let step = &tracer.roots()[0].children[0];
    let names: Vec<&str> = step.children.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["tool:read", "tool:bash", "tool:edit"]);
    assert_eq!(step.duration_ms(), Some(39));
}

#[test]
fn an_empty_tracer_has_no_roots() {
    let tracer = Tracer::new();

    assert_eq!(tracer.roots(), []);
    assert_eq!(tracer.open_depth(), 0);
    assert_eq!(tracer.render(0), "");
}

#[test]
fn ending_with_nothing_open_reports_failure_without_recording() {
    let mut tracer = Tracer::new();

    assert!(!tracer.end(10));
    assert!(tracer.roots().is_empty());
}

#[test]
fn a_total_covers_the_whole_tree() {
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("step", 5);
    tracer.end(35);
    tracer.end(100);

    let turn = &tracer.roots()[0];
    assert_eq!(turn.duration_ms(), Some(100));
    assert_eq!(turn.total_duration_ms(), Some(100));
    assert_eq!(turn.children[0].total_duration_ms(), Some(30));
}

#[test]
fn a_running_span_makes_the_total_unknown() {
    // The total is not "the parent's end": it walks the tree, so a span that has not finished leaves the
    // whole subtree without a total. `None` here is what stops a caller from publishing a partial number.
    let mut parent = spanned("turn", 0, 100);
    parent.children.push(Span::start("tool:bash", 10));

    assert_eq!(parent.duration_ms(), Some(100));
    assert_eq!(parent.total_duration_ms(), None);
}

#[test]
fn a_child_that_outlives_its_parent_widens_the_total() {
    // Timestamps are facts, not budgets. When a child ends after its parent the total reports the real
    // coverage rather than clamping to the parent's duration; that difference is the signal.
    let mut parent = spanned("turn", 0, 100);
    parent.children.push(spanned("tool:bash", 10, 250));

    assert_eq!(parent.duration_ms(), Some(100));
    assert_eq!(parent.total_duration_ms(), Some(250));
    assert!(parent.total_duration_ms() > parent.duration_ms());
}

#[test]
fn parallel_children_can_exceed_their_parent() {
    // Three tools started at the same instant and each ran for 100ms: the children add up to 300ms while
    // the parent measured 120ms of wall time. No rule in this module folds one number into the other, and
    // the test pins that so nobody later "fixes" the apparent inconsistency by clamping.
    let mut tracer = Tracer::new();
    tracer.begin("step", 0);
    for name in ["tool:a", "tool:b", "tool:c"] {
        tracer.begin(name, 10);
        tracer.end(110);
    }
    tracer.end(120);

    let step = &tracer.roots()[0];
    let summed: u64 = step.children.iter().filter_map(Span::duration_ms).sum();

    assert_eq!(step.duration_ms(), Some(120));
    assert_eq!(summed, 300);
    assert!(summed > step.duration_ms().unwrap_or(0));
    // The wall-clock total is still the parent's coverage, not the sum.
    assert_eq!(step.total_duration_ms(), Some(120));
}

#[test]
fn render_shows_hierarchy() {
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("step", 0);
    tracer.begin("tool:bash", 10);
    tracer.end(40);
    tracer.end(90);
    tracer.end(100);

    let rendered = tracer.render(0);

    assert_eq!(
        rendered, "turn 100ms\n  step 90ms\n    tool:bash 30ms\n",
        "each level adds two spaces, and a finished span shows its milliseconds"
    );
}

#[test]
fn render_marks_a_running_span_as_open() {
    // The word rather than `0ms`: a reader scanning a log must be able to see an unfinished span without
    // knowing the difference between "not ended" and "took no time".
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("tool:bash", 10);

    let rendered = tracer.render(0);

    assert_eq!(rendered, "turn open\n  tool:bash open\n");
    assert!(
        rendered.contains("open"),
        "an unfinished span reads {rendered:?}"
    );
    assert!(!rendered.contains("0ms"));
}

#[test]
fn render_starts_at_the_requested_indent() {
    // A caller nesting a trace under its own log line gets the same tree, shifted, not a second format.
    let span = spanned("turn", 0, 10);

    assert_eq!(span.render(0), "turn 10ms\n");
    assert_eq!(span.render(1), "  turn 10ms\n");
    assert_eq!(span.render(2), "    turn 10ms\n");
}

#[test]
fn render_prints_attributes_in_key_order() {
    // Sorted because a trace is compared between runs: hash order would make two identical requests render
    // differently and the difference would be invisible in a diff.
    let span = Span::start("tool:bash", 0)
        .with_attribute("zebra", "last")
        .with_attribute("alpha", "first")
        .with_attribute("middle", "second");
    let mut span = span;
    span.end(5);

    assert_eq!(
        span.render(0),
        "tool:bash 5ms alpha=first middle=second zebra=last\n"
    );
}

#[test]
fn a_span_with_no_attributes_renders_without_trailing_space() {
    let span = spanned("turn", 0, 7);

    assert!(span.attributes.is_empty());
    assert_eq!(span.render(0), "turn 7ms\n");
}

#[test]
fn an_empty_attribute_value_still_renders_its_key() {
    // "present but empty" is different from "absent", and dropping the key would merge the two.
    let mut span = spanned("turn", 0, 1);
    span.attributes.insert("error".to_owned(), String::new());

    assert_eq!(span.render(0), "turn 1ms error=\n");
}

#[test]
fn the_tracer_labels_the_innermost_open_span() {
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("tool:bash", 1);
    assert!(tracer.attribute("exit", "0"));
    assert!(tracer.attribute("exit", "1"));
    tracer.end(10);
    tracer.end(20);

    let tool = &tracer.roots()[0].children[0];
    assert_eq!(tool.attributes.get("exit").map(String::as_str), Some("1"));
    assert!(
        tracer.roots()[0].attributes.is_empty(),
        "the label belongs to the innermost span, not the root"
    );
}

#[test]
fn labelling_with_nothing_open_is_refused() {
    let mut tracer = Tracer::new();

    assert!(!tracer.attribute("exit", "0"));
}

#[test]
fn begin_with_labels_a_span_at_creation() {
    let mut tracer = Tracer::new();
    tracer.begin_with("tool:bash", 0, [("cmd".to_owned(), "ls".to_owned())]);
    tracer.end(5);

    assert_eq!(
        tracer.roots()[0].attributes.get("cmd").map(String::as_str),
        Some("ls")
    );
}

#[test]
fn into_roots_keeps_spans_that_never_ended() {
    // An abandoned request is the most interesting trace there is; discarding it would produce an empty
    // result for exactly the case someone is investigating.
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("tool:bash", 10);

    let roots = tracer.into_roots();

    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].name, "turn");
    assert_eq!(roots[0].duration_ms(), None);
    assert_eq!(roots[0].children.len(), 1);
    assert_eq!(roots[0].children[0].name, "tool:bash");
    assert_eq!(roots[0].children[0].duration_ms(), None);
}

#[test]
fn into_roots_returns_finished_roots_in_order() {
    let mut tracer = Tracer::new();
    tracer.begin("first", 0);
    tracer.end(10);
    tracer.begin("second", 20);
    tracer.end(30);

    let roots = tracer.into_roots();

    let names: Vec<&str> = roots.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["first", "second"]);
}

#[test]
fn span_count_counts_the_whole_tree() {
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("step", 1);
    tracer.end(10);
    tracer.begin("step", 11);
    tracer.begin("tool:bash", 12);
    tracer.end(20);
    tracer.end(30);
    tracer.end(40);

    // turn, two steps, one tool: the closed tree holds four.
    assert_eq!(tracer.span_count(), 4);
}

#[test]
fn span_count_includes_spans_still_open() {
    // A running trace is the one an operator is looking at, so "how many spans" must not read zero while
    // work is in flight.
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin("step", 1);
    tracer.begin("tool:bash", 2);

    assert_eq!(tracer.span_count(), 3);
}

#[test]
fn render_includes_the_open_chain() {
    // A live trace renders the running work as a chain below the finished roots, so the tree is readable
    // while the request is still going rather than only after it ends.
    let mut tracer = Tracer::new();
    tracer.begin("first-turn", 0);
    tracer.end(10);
    tracer.begin("turn", 20);
    tracer.begin("step", 21);
    tracer.begin("tool:bash", 22);

    assert_eq!(
        tracer.render(0),
        "first-turn 10ms\nturn open\n  step open\n    tool:bash open\n"
    );
}

#[test]
fn a_full_turn_renders_readably() {
    // The shape the tracer exists for: one turn, two steps, the second with two tools in parallel. Rendered
    // as text because that is how it reaches a log.
    let mut tracer = Tracer::new();
    tracer.begin("turn", 0);
    tracer.begin_with("step", 5, [("n".to_owned(), "1".to_owned())]);
    tracer.begin("llm", 6);
    tracer.end(106);
    tracer.end(120);
    tracer.begin_with("step", 121, [("n".to_owned(), "2".to_owned())]);
    tracer.begin("tool:bash", 122);
    tracer.end(222);
    tracer.begin("tool:read", 123);
    tracer.end(143);
    tracer.end(230);
    tracer.end(240);

    assert_eq!(
        tracer.render(0),
        "turn 240ms\n  step 115ms n=1\n    llm 100ms\n  step 109ms n=2\n    tool:bash 100ms\n    tool:read 20ms\n",
    );
    assert_eq!(tracer.roots()[0].total_duration_ms(), Some(240));
}
