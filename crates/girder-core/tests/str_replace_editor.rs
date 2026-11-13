//! Tests for ordered replacement steps.
//!
//! The two failures worth pinning are the silent ones: a text that occurs several times being edited at a guessed
//! position, and a plan that changes nothing while reporting success. The success cases are here to prove those
//! refusals have not swallowed the ordinary edit.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile against it as
//! written even before the crate root wires it in. That is why nothing here reaches for anything else in
//! `girder_core`.

#[path = "../src/str_replace_editor.rs"]
mod str_replace_editor;

use str_replace_editor::{EditFailure, Replacement, apply};

const FILE: &str = "\
fn main() {
    let total = 0;
    println!(\"{total}\");
}

fn helper() {
    let total = 0;
    println!(\"{total}\");
}
";

// ---- the ordinary edit --------------------------------------------------------------------------

#[test]
fn a_unique_text_is_replaced() {
    let edited = apply(
        FILE,
        &[Replacement::unique(
            "fn helper() {",
            "fn helper_renamed() {",
        )],
    );

    let edited = edited.expect("a unique match should be replaced");
    assert!(edited.contains("fn helper_renamed() {"));
    assert!(!edited.contains("fn helper() {"));
    // Everything else is untouched, byte for byte.
    assert_eq!(edited.len(), FILE.len() + "_renamed".len());
}

#[test]
fn replacement_of_an_empty_text_is_refused_rather_than_matching_everywhere() {
    // An empty needle would otherwise match at every character boundary; there is no position to choose.
    let failure =
        apply(FILE, &[Replacement::unique("", "x")]).expect_err("an empty needle is refused");

    assert_eq!(failure, EditFailure::NotFound { old: String::new() });
}

#[test]
fn an_empty_plan_is_no_change() {
    // Asking for nothing is not a successful edit, and a caller that sent an empty list has a bug worth hearing
    // about.
    assert_eq!(apply(FILE, &[]), Err(EditFailure::NoChange));
}

// ---- the two refusals ---------------------------------------------------------------------------

#[test]
fn missing_text_is_not_found() {
    let failure = apply(
        FILE,
        &[Replacement::unique("fn nowhere() {", "fn somewhere() {")],
    )
    .expect_err("absent text must not be replaced");

    assert_eq!(
        failure,
        EditFailure::NotFound {
            old: "fn nowhere() {".to_owned()
        }
    );
}

#[test]
fn a_repeated_text_without_an_index_is_ambiguous_with_its_count() {
    // The same line appears in both functions. Taking the first match would rewrite `main`'s body while the caller
    // was looking at `helper`'s.
    let failure = apply(
        FILE,
        &[Replacement::unique("let total = 0;", "let total = 1;")],
    )
    .expect_err("an ambiguous text must not be replaced by guess");

    assert_eq!(
        failure,
        EditFailure::Ambiguous {
            old: "let total = 0;".to_owned(),
            count: 2
        }
    );
}

#[test]
fn ambiguity_reports_how_many_times_the_text_occurs() {
    let content = "x\n".repeat(3) + "unique\n";
    let failure = apply(&content, &[Replacement::unique("x", "y")])
        .expect_err("three occurrences without an index is ambiguous");

    match failure {
        EditFailure::Ambiguous { count, .. } => assert_eq!(count, 3),
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn a_replacement_onto_itself_is_no_change() {
    let failure = apply(FILE, &[Replacement::unique("fn main() {", "fn main() {")])
        .expect_err("a step that changes nothing must be refused");

    assert_eq!(failure, EditFailure::NoChange);
}

#[test]
fn steps_that_cancel_each_other_are_no_change() {
    // Each step does something, and the net effect is nothing. The caller expected an edit, so this is a bug in the
    // plan rather than a no-op to report as success.
    let failure = apply(
        FILE,
        &[
            Replacement::unique("fn main() {", "fn entry() {"),
            Replacement::unique("fn entry() {", "fn main() {"),
        ],
    )
    .expect_err("a plan that restores the original text must be refused");

    assert_eq!(failure, EditFailure::NoChange);
}

// ---- choosing an occurrence ---------------------------------------------------------------------

#[test]
fn a_named_occurrence_is_replaced() {
    let edited = apply(
        FILE,
        &[Replacement::nth(
            "let total = 0;",
            "let total = 0; // second",
            1,
        )],
    )
    .expect("a named occurrence should be replaced");

    // Only the second one changed: the first `let total = 0;` in `main` is untouched.
    assert!(edited.contains("    let total = 0;\n    println!"));
    assert!(edited.contains("    let total = 0; // second\n    println!(\"{total}\");"));
    let changed = edited.matches("// second").count();
    assert_eq!(changed, 1);
}

#[test]
fn the_first_occurrence_can_be_named_explicitly() {
    let edited = apply(
        FILE,
        &[Replacement::nth("let total = 0;", "let total = 1;", 0)],
    )
    .expect("index zero is a valid choice");

    assert!(edited.contains("    let total = 1;\n    println!(\"{total}\");\n}\n\nfn helper"));
    assert_eq!(edited.matches("let total = 1;").count(), 1);
    assert_eq!(edited.matches("let total = 0;").count(), 1);
}

#[test]
fn an_occurrence_past_the_end_is_out_of_range() {
    let failure = apply(
        FILE,
        &[Replacement::nth("let total = 0;", "let total = 1;", 2)],
    )
    .expect_err("index 2 of 2 matches does not exist");

    assert_eq!(failure, EditFailure::OutOfRange { index: 2, count: 2 });
}

#[test]
fn an_index_on_a_unique_text_is_accepted_when_it_is_zero() {
    // The index is checked against the real count rather than against the need for it, so a caller that always
    // sends an index works on a unique text too.
    let edited = apply(
        FILE,
        &[Replacement::nth("fn helper() {", "fn assist() {", 0)],
    )
    .expect("index zero of one match is valid");

    assert!(edited.contains("fn assist() {"));
}

#[test]
fn an_index_past_a_unique_text_is_out_of_range() {
    let failure = apply(
        FILE,
        &[Replacement::nth("fn helper() {", "fn assist() {", 1)],
    )
    .expect_err("index 1 of 1 match does not exist");

    assert_eq!(failure, EditFailure::OutOfRange { index: 1, count: 1 });
}

// ---- several steps ------------------------------------------------------------------------------

#[test]
fn steps_are_applied_in_order() {
    // The second step targets text the first step produced, which only works if the order is respected.
    let edited = apply(
        FILE,
        &[
            Replacement::nth("let total = 0;", "let total = compute();", 0),
            Replacement::unique("compute()", "compute(2)"),
            Replacement::unique("fn helper()", "fn helper_two()"),
        ],
    )
    .expect("ordered steps should apply");

    assert!(edited.contains("let total = compute(2);"));
    assert!(edited.contains("fn helper_two()"));
    assert_eq!(edited.matches("compute(2)").count(), 1);
}

#[test]
fn a_later_step_sees_the_text_of_an_earlier_one() {
    // Both functions contain the line, so the plan needs an index on the first step. Once `main`'s copy is gone the
    // remaining one is unique, and the plain step succeeds where it would have been ambiguous against the original.
    let edited = apply(
        FILE,
        &[
            Replacement::nth("let total = 0;", "let total = 1;", 0),
            Replacement::unique("let total = 0;", "let total = 2;"),
        ],
    )
    .expect("the second step should see the first step's text");

    assert!(edited.contains("let total = 1;"));
    assert!(edited.contains("let total = 2;"));
    assert!(!edited.contains("let total = 0;"));
}

#[test]
fn a_failing_step_stops_the_plan() {
    let failure = apply(
        FILE,
        &[
            Replacement::unique("fn helper() {", "fn assist() {"),
            Replacement::unique("fn nowhere() {", "fn somewhere() {"),
        ],
    )
    .expect_err("a later failure must not be swallowed");

    assert_eq!(
        failure,
        EditFailure::NotFound {
            old: "fn nowhere() {".to_owned()
        }
    );
}

// ---- the message a caller reads ----------------------------------------------------------------

#[test]
fn an_ambiguous_failure_names_the_lines_of_each_occurrence() {
    let failure = apply(
        FILE,
        &[Replacement::unique("let total = 0;", "let total = 1;")],
    )
    .expect_err("ambiguous");
    let rendered = failure.describe(FILE);

    // Both real line numbers, so the caller can look at either site.
    assert!(rendered.contains("occurs 2 times"), "rendered: {rendered}");
    assert!(
        rendered.contains("occurrence 0 is at line 2"),
        "rendered: {rendered}"
    );
    assert!(
        rendered.contains("occurrence 1 is at line 7"),
        "rendered: {rendered}"
    );
    // The matching line is marked, and neighbouring lines come along with it.
    assert!(
        rendered.contains(">     2 |     let total = 0;"),
        "rendered: {rendered}"
    );
    assert!(
        rendered.contains("      1 | fn main() {"),
        "rendered: {rendered}"
    );
}

#[test]
fn a_not_found_failure_shows_the_region_where_the_first_line_matches() {
    // The caller knows the file, but its `old` has the indentation wrong: pointing at the line that almost matched
    // is what makes that visible.
    let failure = apply(
        FILE,
        &[Replacement::unique(
            "let total = 0;\n    let x = 1;",
            "let total = 1;",
        )],
    )
    .expect_err("absent text");
    let rendered = failure.describe(FILE);

    assert!(rendered.contains("was not found"), "rendered: {rendered}");
    // The first line does match, so the message points at where the caller's snippet starts to drift.
    assert!(rendered.contains("at line 2"), "rendered: {rendered}");
    assert!(
        rendered.contains(">     2 |     let total = 0;"),
        "rendered: {rendered}"
    );
    assert!(rendered.contains("whitespace"), "rendered: {rendered}");
}

#[test]
fn a_not_found_failure_with_no_matching_line_says_the_versions_differ() {
    let failure = apply(
        FILE,
        &[Replacement::unique("fn nowhere() {", "fn somewhere() {")],
    )
    .expect_err("absent text");
    let rendered = failure.describe(FILE);

    assert!(
        rendered.contains("`fn nowhere() {`"),
        "rendered: {rendered}"
    );
    assert!(
        rendered.contains("different version of the text"),
        "rendered: {rendered}"
    );
}

#[test]
fn an_out_of_range_failure_states_the_valid_range() {
    let failure = apply(
        FILE,
        &[Replacement::nth("let total = 0;", "let total = 1;", 2)],
    )
    .expect_err("out of range");
    let rendered = failure.describe(FILE);

    assert!(rendered.contains("out of range"), "rendered: {rendered}");
    assert!(rendered.contains("from 0 to 1"), "rendered: {rendered}");
}

#[test]
fn a_no_change_failure_explains_the_two_ways_it_happens() {
    let failure =
        apply(FILE, &[Replacement::unique("fn main() {", "fn main() {")]).expect_err("no change");
    let rendered = failure.describe(FILE);

    assert!(
        rendered.contains("replaces text with itself"),
        "rendered: {rendered}"
    );
}

#[test]
fn a_long_snippet_is_quoted_on_one_bounded_line() {
    let old = format!("fn missing() {{\n{}\n}}", "\t".repeat(4));
    let failure = apply(FILE, &[Replacement::unique(old.clone(), "fn present() {}")])
        .expect_err("absent text");
    let rendered = failure.describe(FILE);

    // Newlines and tabs are escaped so the quote stays one readable line, and the snippet is not repeated whole.
    assert!(!rendered.contains('\t'), "rendered: {rendered}");
    assert!(rendered.contains("\\n"), "rendered: {rendered}");
}

#[test]
fn a_window_at_the_end_of_the_file_does_not_run_past_it() {
    let content = "one\ntwo\nthree\nfour\nfive\nsix\nseven\nseven\n";
    let failure = apply(content, &[Replacement::unique("seven", "SEVEN")]).expect_err("ambiguous");
    let rendered = failure.describe(content);

    // The second occurrence is the last line, so the window stops there rather than naming lines that do not exist.
    assert!(
        rendered.contains("occurrence 1 is at line 8"),
        "rendered: {rendered}"
    );
    assert!(!rendered.contains("later line"), "rendered: {rendered}");
}
