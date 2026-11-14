//! Tests for line-level text comparison.
//!
//! The properties worth pinning down are the ones a renderer depends on: that `context` really
//! trims, that the counts describe the whole change even when the preview is cut, and that a
//! rewrite too large to align still reports an honest, bounded answer.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from
//! `lib.rs` is a separate change made by whoever wires it up. When it is exported, this can become
//! `use girder_core::workspace_diff::{LineDiff, MAX_DIFF_LINES, diff_text};`.

#[path = "../src/workspace_diff.rs"]
mod workspace_diff;

use workspace_diff::{DEFAULT_CONTEXT, FileChange, LineDiff, MAX_DIFF_LINES, TextDiff, diff_text};

/// Render a diff as compact ` `, `-`, `+` prefixed lines, the way a unified diff reads.
///
/// Comparing rendered text rather than the enum keeps the assertions about *order* and *content* at
/// once, and a failure prints the whole diff instead of the first mismatching variant.
fn render(diff: &TextDiff) -> Vec<String> {
    diff.lines
        .iter()
        .map(|line| match line {
            LineDiff::Context { line } => format!(" {line}"),
            LineDiff::Removed { line } => format!("-{line}"),
            LineDiff::Added { line } => format!("+{line}"),
        })
        .collect()
}

/// Text holding one line per value of `1..=count`, each rendered by `label`.
///
/// Built with `write!` rather than `map(..).collect()` because clippy's `format_collect` flags the
/// latter, and silencing it locally would be noisier than writing the loop.
fn lines(count: usize, label: impl Fn(usize) -> String) -> String {
    let mut text = String::new();
    for value in 1..=count {
        let line = label(value);
        text.push_str(&line);
        text.push('\n');
    }
    text
}

/// A file body of `count` lines, `line1`, `line2`, ...
fn body(count: usize) -> String {
    lines(count, |n| format!("line{n}"))
}

// ---- identical and empty -----------------------------------------------------------------------

#[test]
fn identical_text_has_no_changes_and_no_lines() {
    let text = "one\ntwo\nthree\n";

    let diff = diff_text(text, text, 3);

    assert_eq!(diff.added, 0);
    assert_eq!(diff.removed, 0);
    assert!(!diff.truncated);
    // Context is only kept around a change; with nothing changed there is no window to keep.
    assert!(diff.lines.is_empty(), "got {:?}", render(&diff));
}

#[test]
fn empty_inputs_compare_equal() {
    let diff = diff_text("", "", 3);

    assert!(diff.lines.is_empty());
    assert_eq!((diff.added, diff.removed), (0, 0));
}

#[test]
fn empty_before_is_pure_addition() {
    let diff = diff_text("", "one\ntwo\n", 3);

    assert_eq!(render(&diff), vec!["+one", "+two"]);
    assert_eq!((diff.added, diff.removed), (2, 0));
}

#[test]
fn empty_after_is_pure_removal() {
    let diff = diff_text("one\ntwo\n", "", 3);

    assert_eq!(render(&diff), vec!["-one", "-two"]);
    assert_eq!((diff.added, diff.removed), (0, 2));
}

#[test]
fn a_trailing_newline_alone_is_not_a_change() {
    // A file that gains or loses its final newline holds the same lines; reporting a change here
    // would make every editor save look like an edit.
    let diff = diff_text("one\ntwo\n", "one\ntwo", 3);

    assert!(diff.lines.is_empty());
    assert_eq!((diff.added, diff.removed), (0, 0));
}

#[test]
fn a_blank_line_is_a_real_line() {
    let diff = diff_text("one\n\ntwo\n", "one\ntwo\n", 3);

    assert_eq!(diff.added, 0);
    assert_eq!(diff.removed, 1);
    assert!(
        diff.lines
            .iter()
            .any(|l| matches!(l, LineDiff::Removed { line } if line.is_empty()))
    );
}

// ---- pure insertion and deletion ---------------------------------------------------------------

#[test]
fn appending_lines_adds_only_those_lines() {
    let diff = diff_text(&body(3), &format!("{}line4\n", body(3)), 0);

    assert_eq!(render(&diff), vec!["+line4"]);
    assert_eq!((diff.added, diff.removed), (1, 0));
}

#[test]
fn prepending_lines_adds_only_those_lines() {
    let after = format!("line0\n{}", body(3));

    let diff = diff_text(&body(3), &after, 0);

    assert_eq!(render(&diff), vec!["+line0"]);
    assert_eq!((diff.added, diff.removed), (1, 0));
}

#[test]
fn inserting_in_the_middle_adds_only_that_line() {
    let before = "one\ntwo\nthree\n";
    let after = "one\ntwo\ninserted\nthree\n";

    let diff = diff_text(before, after, 0);

    assert_eq!(render(&diff), vec!["+inserted"]);
    assert_eq!((diff.added, diff.removed), (1, 0));
}

#[test]
fn deleting_from_the_middle_removes_only_that_line() {
    let before = "one\ntwo\ngone\nthree\n";
    let after = "one\ntwo\nthree\n";

    let diff = diff_text(before, after, 0);

    assert_eq!(render(&diff), vec!["-gone"]);
    assert_eq!((diff.added, diff.removed), (0, 1));
}

// ---- a single changed line, and the context window ---------------------------------------------

#[test]
fn one_changed_line_keeps_context_on_both_sides() {
    let before = &body(5);
    let after = &before.replace("line3", "changed");

    let diff = diff_text(before, after, 1);

    assert_eq!(
        render(&diff),
        vec![" line2", "-line3", "+changed", " line4"]
    );
    assert_eq!((diff.added, diff.removed), (1, 1));
}

#[test]
fn zero_context_keeps_only_the_changed_lines() {
    let before = "one\ntwo\nthree\n";
    let after = "one\nTWO\nthree\n";

    let diff = diff_text(before, after, 0);

    assert_eq!(render(&diff), vec!["-two", "+TWO"]);
    assert_eq!((diff.added, diff.removed), (1, 1));
}

#[test]
fn context_is_clamped_at_the_start_of_the_file() {
    let before = "one\ntwo\nthree\n";
    let after = "ONE\ntwo\nthree\n";

    let diff = diff_text(before, after, 5);

    // The window would reach five lines back and five forward; both ends clamp.
    assert_eq!(render(&diff), vec!["-one", "+ONE", " two", " three"]);
}

#[test]
fn context_is_clamped_at_the_end_of_the_file() {
    let before = "one\ntwo\nthree\n";
    let after = "one\ntwo\nTHREE\n";

    let diff = diff_text(before, after, 5);

    assert_eq!(render(&diff), vec![" one", " two", "-three", "+THREE"]);
}

#[test]
fn a_larger_context_includes_more_unchanged_lines() {
    let before = &body(9);
    let after = &before.replace("line5", "changed");

    let one = diff_text(before, after, 1);
    let two = diff_text(before, after, 2);

    // The counts never depend on context; only the preview does.
    assert_eq!((one.added, one.removed), (two.added, two.removed));
    assert_eq!(render(&one).len(), 4);
    assert_eq!(
        render(&two),
        vec![" line3", " line4", "-line5", "+changed", " line6", " line7"]
    );
}

#[test]
fn distant_changes_get_separate_windows() {
    let before = &body(20);
    let after = &before.replace("line2\n", "A\n").replace("line18\n", "B\n");

    let diff = diff_text(before, after, 1);

    // Two windows with a gap between them: lines 3..=17 are omitted because no change is near them.
    assert_eq!(
        render(&diff),
        vec![
            " line1", "-line2", "+A", " line3", //
            " line17", "-line18", "+B", " line19",
        ]
    );
    assert_eq!((diff.added, diff.removed), (2, 2));
}

#[test]
fn nearby_changes_merge_into_one_window() {
    let before = "one\ntwo\nthree\nfour\nfive\n";
    let after = "one\nTWO\nthree\nFOUR\nfive\n";

    let diff = diff_text(before, after, 2);

    // The two windows share `three`, so it appears once instead of twice.
    assert_eq!(
        render(&diff),
        vec![" one", "-two", "+TWO", " three", "-four", "+FOUR", " five"]
    );
    assert_eq!((diff.added, diff.removed), (2, 2));
}

#[test]
fn adjacent_changes_merge_without_repeating_a_line() {
    let before = "one\ntwo\nthree\nfour\n";
    let after = "one\nTWO\nTHREE\nfour\n";

    let diff = diff_text(before, after, 0);

    assert_eq!(render(&diff), vec!["-two", "-three", "+TWO", "+THREE"]);
    assert_eq!((diff.added, diff.removed), (2, 2));
}

// ---- counts -------------------------------------------------------------------------------------

#[test]
fn counts_are_exact_for_a_replacement_of_equal_length() {
    let before = "a\nb\nc\nd\n";
    let after = "a\nB\nC\nd\n";

    let diff = diff_text(before, after, 3);

    assert_eq!((diff.added, diff.removed), (2, 2));
}

#[test]
fn counts_are_exact_when_a_block_grows() {
    let before = "a\nb\nc\n";
    let after = "a\nb\nnew1\nnew2\nnew3\nc\n";

    let diff = diff_text(before, after, 1);

    assert_eq!((diff.added, diff.removed), (3, 0));
}

#[test]
fn counts_are_exact_when_a_block_shrinks() {
    let before = "a\ngone1\ngone2\ngone3\nz\n";
    let after = "a\nz\n";

    let diff = diff_text(before, after, 1);

    assert_eq!((diff.added, diff.removed), (0, 3));
}

#[test]
fn counts_match_the_emitted_lines_when_nothing_is_truncated() {
    let before = lines(40, |n| format!("old{n}"));
    let after = lines(40, |n| format!("new{n}"));

    let diff = diff_text(&before, &after, 2);

    let emitted_added = diff
        .lines
        .iter()
        .filter(|l| matches!(l, LineDiff::Added { .. }))
        .count();
    let emitted_removed = diff
        .lines
        .iter()
        .filter(|l| matches!(l, LineDiff::Removed { .. }))
        .count();
    assert!(!diff.truncated);
    assert_eq!(diff.added, 40);
    assert_eq!(diff.removed, 40);
    assert_eq!((emitted_added, emitted_removed), (diff.added, diff.removed));
}

// ---- large input --------------------------------------------------------------------------------

#[test]
fn a_large_rewrite_is_truncated_but_still_counted() {
    // 20k old lines and 20k new lines share nothing, so the middle far exceeds the alignment cap
    // and the whole region is reported as a replacement.
    let before = lines(20_000, |n| format!("old{n}"));
    let after = lines(20_000, |n| format!("new{n}"));

    let diff = diff_text(&before, &after, 3);

    assert!(
        diff.truncated,
        "a rewrite this size must be flagged as truncated"
    );
    assert!(
        diff.lines.len() <= MAX_DIFF_LINES,
        "got {} lines",
        diff.lines.len()
    );
    assert_eq!(diff.added, 20_000);
    assert_eq!(diff.removed, 20_000);
    // The prefix of the diff survives, so the first change is still visible.
    assert_eq!(
        &render(&diff)[..2],
        &["-old1".to_owned(), "-old2".to_owned()][..]
    );
}

#[test]
fn truncation_happens_exactly_at_the_cap() {
    // Every line changes and there is no context, so the emitted count equals the change size. One
    // fewer than the cap fits; the cap itself is where the flag flips.
    let fits = MAX_DIFF_LINES / 2;
    let before = lines(fits, |n| format!("old{n}"));
    let after = lines(fits, |n| format!("new{n}"));

    let diff = diff_text(&before, &after, 0);

    assert_eq!(diff.lines.len(), fits * 2);
    assert!(
        !diff.truncated,
        "a diff that fills the cap exactly is complete"
    );
}

#[test]
fn one_edit_in_a_very_large_file_stays_small_and_exact() {
    // The common prefix and suffix absorb everything but the edited line, so no truncation and no
    // coarse fallback: this is the case the algorithm is optimised for.
    let before = body(100_000);
    let after = before.replacen("line50000\n", "changed\n", 1);

    let diff = diff_text(&before, &after, 3);

    assert!(!diff.truncated);
    assert_eq!((diff.added, diff.removed), (1, 1));
    assert_eq!(
        render(&diff),
        vec![
            " line49997",
            " line49998",
            " line49999",
            "-line50000",
            "+changed",
            " line50001",
            " line50002",
            " line50003",
        ]
    );
}

#[test]
fn a_coarse_rewrite_still_renders_the_new_text_faithfully() {
    // Past the alignment cap the middle becomes a replacement. The rendering is less terse, but
    // every line of `after` is still present exactly once, which is what a reader relies on.
    let before = lines(800, |n| format!("old{n}"));
    let after = lines(800, |n| format!("new{n}"));

    let diff = diff_text(&before, &after, 0);

    let added: Vec<&str> = diff
        .lines
        .iter()
        .filter_map(|l| match l {
            LineDiff::Added { line } => Some(line.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(added.len(), 800);
    assert_eq!(added.first().copied(), Some("new1"));
    assert_eq!(added.last().copied(), Some("new800"));
    assert_eq!((diff.added, diff.removed), (800, 800));
}

// ---- the file-level shape -----------------------------------------------------------------------

#[test]
fn default_context_is_the_unified_diff_standard() {
    // Named so a caller can ask for the conventional window without hard-coding three.
    assert_eq!(DEFAULT_CONTEXT, 3);

    let before = &body(9);
    let after = &before.replace("line5", "changed");

    // Three context lines each side, plus the removed and the added line.
    assert_eq!(render(&diff_text(before, after, DEFAULT_CONTEXT)).len(), 8);
}

#[test]
fn a_file_change_carries_counts_and_an_optional_diff() {
    let before = "one\ntwo\nthree\n";
    let after = "one\nTWO\nthree\n";
    let diff = diff_text(before, after, DEFAULT_CONTEXT);

    let with_diff = FileChange {
        path: "src/lib.rs".to_owned(),
        added: diff.added,
        removed: diff.removed,
        diff: Some(diff),
    };
    let without = FileChange {
        path: "Cargo.lock".to_owned(),
        added: 0,
        removed: 0,
        diff: None,
    };

    assert_eq!((with_diff.added, with_diff.removed), (1, 1));
    assert_eq!(with_diff.path, "src/lib.rs");
    assert_eq!(
        with_diff.diff.as_ref().map(render),
        Some(vec![
            " one".to_owned(),
            "-two".to_owned(),
            "+TWO".to_owned(),
            " three".to_owned()
        ])
    );
    // A file may be listed by counts alone; the diff is a preview, not part of the identity.
    assert!(without.diff.is_none());
}
