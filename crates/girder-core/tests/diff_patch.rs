//! Tests for unified-diff parsing and application.
//!
//! The cases are weighted toward the two refusals that make a patch safe to run unattended:
//!
//! - **a drifted file**, where the context no longer sits where the header says it does. A patch
//!   that relocated itself by searching would edit a function it was never written against, and a
//!   patch that ignored the context entirely would edit whatever line is now at that number;
//! - **a truncated body**, where the declared counts and the body disagree. That is what a patch cut
//!   off in transit looks like, and applying it deletes the lines the missing body covered.
//!
//! The rest pins the ordinary paths — one hunk, several hunks, pure insertion, pure deletion, an
//! empty patch — plus the two edges that produce off-by-one bugs: an insertion at the end of the
//! text, and a file with no trailing newline. Non-ASCII text is exercised because an implementation
//! that indexes by byte instead of by line panics or slices a character in half on Chinese input.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

#[path = "../src/fs/diff_patch.rs"]
pub mod diff_patch;

use diff_patch::{Hunk, Patch, PatchError, apply, parse};

/// A hunk with the four numbers spelled out, so a test states its own off-by-one assumptions.
fn hunk(old_start: u32, old_lines: u32, new_start: u32, new_lines: u32, lines: &[&str]) -> Hunk {
    Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
    }
}

/// A one-hunk patch around the given body.
fn patch_of(lines: &[&str]) -> Patch {
    let old = lines.iter().filter(|line| !line.starts_with('+')).count();
    let new = lines.iter().filter(|line| !line.starts_with('-')).count();
    Patch {
        hunks: vec![hunk(
            1,
            u32::try_from(old).unwrap_or(0),
            1,
            u32::try_from(new).unwrap_or(0),
            lines,
        )],
    }
}

// ---- parse: the accepted shape ---------------------------------------------------------------

#[test]
fn a_hunk_header_is_read_including_its_counts() {
    let parsed = parse("@@ -3,4 +3,5 @@\n a\n-b\n+b2\n+c\n d\n e\n").expect("parsed");

    assert_eq!(parsed.hunks.len(), 1);
    let only = &parsed.hunks[0];
    assert_eq!(only.old_start, 3);
    assert_eq!(only.old_lines, 4);
    assert_eq!(only.new_start, 3);
    assert_eq!(only.new_lines, 5);
    assert_eq!(only.lines.len(), 6);
}

#[test]
fn an_omitted_count_means_one_line() {
    let parsed = parse("@@ -2 +2 @@\n-old\n+new\n").expect("parsed");

    assert_eq!(parsed.hunks[0].old_start, 2);
    assert_eq!(parsed.hunks[0].old_lines, 1);
    assert_eq!(parsed.hunks[0].new_lines, 1);
}

#[test]
fn a_section_heading_after_the_header_is_ignored() {
    let parsed = parse("@@ -1,1 +1,1 @@ fn main() {\n-a\n+b\n").expect("parsed");

    assert_eq!(parsed.hunks.len(), 1);
    assert_eq!(parsed.hunks[0].old_lines, 1);
}

#[test]
fn file_headers_are_accepted_and_skipped() {
    // The shape `git diff` emits, including the metadata that precedes the first hunk.
    let text = "diff --git a/f.txt b/f.txt\nindex 1234567..89abcde 100644\n--- a/f.txt\n\
                +++ b/f.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n";

    let parsed = parse(text).expect("parsed");

    assert_eq!(parsed.hunks.len(), 1);
    assert_eq!(apply("one\ntwo\n", &parsed).expect("applied"), "one\nTWO\n");
}

// ---- parse: the rejected shapes --------------------------------------------------------------

#[test]
fn a_body_that_disagrees_with_its_header_is_rejected() {
    // The header promises two old lines, the body removes one: a patch truncated in transit.
    let error = parse("@@ -1,2 +1,2 @@\n one\n").expect_err("rejected");

    assert!(
        matches!(
            error,
            PatchError::CountMismatch {
                expected: 2,
                found: 1
            }
        ),
        "expected a count mismatch, got {error:?}"
    );
}

#[test]
fn a_body_with_a_missing_added_line_is_rejected() {
    // The new side declares three lines, the body produces two. Either count is checked, because a
    // patch is as wrong when its result is short as when its input is.
    let error = parse("@@ -1,2 +1,3 @@\n one\n two\n").expect_err("rejected");

    assert!(
        matches!(
            error,
            PatchError::CountMismatch {
                expected: 3,
                found: 2
            }
        ),
        "expected a count mismatch, got {error:?}"
    );
}

#[test]
fn an_unprefixed_body_line_is_rejected() {
    // No prefix at all: neither context (` `), nor removed (`-`), nor added (`+`).
    let error = parse("@@ -1,1 +1,1 @@\n-one\n+one\nsecond\n").expect_err("rejected");

    assert!(
        matches!(error, PatchError::InvalidHeader { line: 4, .. }),
        "expected an invalid header at line 4, got {error:?}"
    );
}

#[test]
fn a_combined_diff_is_rejected() {
    let error = parse("@@@ -1,1 -1,1 +1,1 @@@\n a\n").expect_err("rejected");

    assert!(
        matches!(error, PatchError::InvalidHeader { line: 1, .. }),
        "expected an invalid header, got {error:?}"
    );
}

#[test]
fn a_binary_patch_is_rejected() {
    let error = parse("diff --git a/x.png b/x.png\nGIT binary patch\n").expect_err("rejected");

    assert!(
        matches!(error, PatchError::InvalidHeader { line: 2, .. }),
        "expected an invalid header, got {error:?}"
    );
}

#[test]
fn the_no_newline_marker_is_rejected() {
    // Accepting it would silently keep the trailing newline the marker says is absent, which is a
    // different file than the patch describes.
    let error =
        parse("@@ -1,1 +1,1 @@\n-a\n+b\n\\ No newline at end of file\n").expect_err("rejected");

    assert!(
        matches!(error, PatchError::InvalidHeader { line: 4, .. }),
        "expected an invalid header at line 4, got {error:?}"
    );
}

#[test]
fn a_second_file_is_rejected() {
    let text =
        "@@ -1,1 +1,1 @@\n-a\n+b\n--- a/other.txt\n+++ b/other.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n";

    let error = parse(text).expect_err("rejected");

    assert!(
        matches!(error, PatchError::InvalidHeader { line: 4, .. }),
        "expected an invalid header at line 4, got {error:?}"
    );
}

#[test]
fn a_header_only_input_parses_to_an_empty_patch() {
    // A patch with no hunks is a no-op, not an error: `apply` then returns the text unchanged. This
    // is the one place a no-op beats a refusal, because nothing was asked to change.
    let parsed = parse("--- a/f.txt\n+++ b/f.txt\n").expect("parsed");

    assert!(parsed.hunks.is_empty());
    assert_eq!(apply("one\ntwo\n", &parsed).expect("applied"), "one\ntwo\n");
}

// ---- apply: the ordinary paths ---------------------------------------------------------------

#[test]
fn a_single_hunk_applies_in_place() {
    let patch = patch_of(&[" one", "-two", "+TWO", " three"]);

    assert_eq!(
        apply("one\ntwo\nthree\n", &patch).expect("applied"),
        "one\nTWO\nthree\n"
    );
}

#[test]
fn several_hunks_apply_in_one_pass() {
    // The second hunk is written against the *original* line numbers, which is what makes it a test:
    // an implementation that readjusted positions as it went would shift this one by the line the
    // first hunk added.
    let patch = Patch {
        hunks: vec![
            hunk(1, 1, 1, 2, &["-one", "+ONE", "+uno"]),
            hunk(4, 1, 5, 1, &["-four", "+FOUR"]),
        ],
    };

    assert_eq!(
        apply("one\ntwo\nthree\nfour\nfive\n", &patch).expect("applied"),
        "ONE\nuno\ntwo\nthree\nFOUR\nfive\n"
    );
}

#[test]
fn a_pure_insertion_adds_lines() {
    // `-2,0`: no old lines are consumed, so the four lines are inserted after line 2.
    let patch = Patch {
        hunks: vec![hunk(
            2,
            0,
            3,
            2,
            &["+two and a half", "+two and three quarters"],
        )],
    };

    assert_eq!(
        apply("one\ntwo\nthree\n", &patch).expect("applied"),
        "one\ntwo\ntwo and a half\ntwo and three quarters\nthree\n"
    );
}

#[test]
fn an_insertion_at_the_very_end_is_allowed() {
    // `old_start` equals the line count here. Rejecting it as out of range is the classic
    // off-by-one: appending to a file is a normal edit.
    let patch = Patch {
        hunks: vec![hunk(3, 0, 4, 1, &["+four"])],
    };

    assert_eq!(
        apply("one\ntwo\nthree\n", &patch).expect("applied"),
        "one\ntwo\nthree\nfour\n"
    );
}

#[test]
fn a_pure_deletion_removes_lines() {
    let patch = patch_of(&[" one", "-two", "-three", " four"]);

    assert_eq!(
        apply("one\ntwo\nthree\nfour\n", &patch).expect("applied"),
        "one\nfour\n"
    );
}

#[test]
fn deleting_every_line_empties_the_text() {
    let patch = patch_of(&["-one", "-two"]);

    assert_eq!(apply("one\ntwo\n", &patch).expect("applied"), "");
}

#[test]
fn an_empty_patch_returns_the_text_unchanged() {
    let patch = Patch { hunks: Vec::new() };

    assert_eq!(apply("one\ntwo\n", &patch).expect("applied"), "one\ntwo\n");
}

#[test]
fn a_text_without_a_trailing_newline_keeps_not_having_one() {
    let patch = patch_of(&[" one", "-two"]);

    assert_eq!(apply("one\ntwo", &patch).expect("applied"), "one");
}

#[test]
fn trailing_whitespace_is_part_of_the_context() {
    // `one ` and `one` are different lines. A matcher that trimmed would apply a hunk written
    // against a file that had trailing spaces to one that never did — and report success.
    let patch = patch_of(&[" one ", "-two", "+TWO"]);

    assert_eq!(
        apply("one \ntwo\n", &patch).expect("applied"),
        "one \nTWO\n"
    );
    let error = apply("one\ntwo\n", &patch).expect_err("rejected");
    assert!(
        matches!(error, PatchError::ContextMismatch { line: 1, .. }),
        "expected a context mismatch at line 1, got {error:?}"
    );
}

#[test]
fn a_patch_with_a_crlf_body_applies_to_a_crlf_text() {
    let patch = parse("@@ -1,2 +1,2 @@\n one\r\n-two\r\n+TWO\r\n").expect("parsed");

    assert_eq!(
        apply("one\r\ntwo\r\n", &patch).expect("applied"),
        "one\r\nTWO\r\n"
    );
}

// ---- apply: the refusals that make it safe ---------------------------------------------------

#[test]
fn a_changed_context_line_is_rejected() {
    // The file gained a line above the hunk, so line 2 is now `two`'s predecessor. The header still
    // says line 2, and the text there is not what the patch expects.
    let patch = parse("@@ -2,1 +2,1 @@\n-two\n+TWO\n").expect("parsed");

    let error = apply("one\none and a half\ntwo\n", &patch).expect_err("rejected");

    match error {
        PatchError::ContextMismatch {
            line,
            expected,
            found,
        } => {
            assert_eq!(line, 2);
            assert_eq!(expected, "two");
            assert_eq!(found, "one and a half");
        }
        other => panic!("expected a context mismatch, got {other:?}"),
    }
}

#[test]
fn a_removed_line_that_no_longer_exists_is_rejected() {
    // The body says to remove `two` from a file where `two` was already deleted. Silently treating
    // the line as absent would delete `three` instead.
    let patch = patch_of(&[" one", "-two", " three"]);

    let error = apply("one\nthree\nfour\n", &patch).expect_err("rejected");

    assert!(
        matches!(error, PatchError::ContextMismatch { line: 2, .. }),
        "expected a context mismatch at line 2, got {error:?}"
    );
}

#[test]
fn a_hunk_past_the_end_is_rejected() {
    let patch = patch_of(&["-one", "+ONE"]);
    let far = Patch {
        hunks: vec![hunk(9, 1, 9, 1, &["-one", "+ONE"])],
    };

    let error = apply("one\n", &far).expect_err("rejected");

    assert_eq!(patch.hunks[0].old_start, 1);
    assert!(
        matches!(error, PatchError::OutOfRange { line: 9, total: 1 }),
        "expected an out-of-range error, got {error:?}"
    );
}

#[test]
fn a_hunk_that_overlaps_its_predecessor_is_rejected() {
    // Applying both would consume the same source line twice, and the second application would be
    // against a text the patch never described.
    let patch = Patch {
        hunks: vec![
            hunk(2, 1, 2, 1, &["-two", "+TWO"]),
            hunk(1, 2, 1, 2, &["-one", "-two", "+ONE", "+TWO"]),
        ],
    };

    let error = apply("one\ntwo\n", &patch).expect_err("rejected");

    assert!(
        matches!(error, PatchError::InvalidHeader { .. }),
        "expected an invalid header, got {error:?}"
    );
}

#[test]
fn a_hand_built_hunk_with_wrong_counts_is_rejected() {
    // `apply` re-checks the counts, so a caller that builds a `Hunk` itself cannot skip the
    // truncation check the parser performs.
    let patch = Patch {
        hunks: vec![hunk(2, 2, 2, 2, &["-two", "+TWO"])],
    };

    let error = apply("one\ntwo\n", &patch).expect_err("rejected");

    assert!(
        matches!(
            error,
            PatchError::CountMismatch {
                expected: 2,
                found: 1
            }
        ),
        "expected a count mismatch, got {error:?}"
    );
}

// ---- apply: non-ASCII text -------------------------------------------------------------------

#[test]
fn chinese_context_and_changes_apply() {
    let patch =
        parse("@@ -1,3 +1,3 @@\n 第一行\n-第二行\n+第二行（改）\n 第三行\n").expect("parsed");

    assert_eq!(
        apply("第一行\n第二行\n第三行\n", &patch).expect("applied"),
        "第一行\n第二行（改）\n第三行\n"
    );
}

#[test]
fn chinese_context_that_does_not_match_is_rejected() {
    // Byte offsets and character offsets diverge on Chinese text, so a matcher that compared the
    // wrong slice could accept this. It must not.
    let patch = parse("@@ -1,2 +1,2 @@\n 第一行\n-第二行\n+第二行（改）\n").expect("parsed");

    let error = apply("第一行\n另一行\n", &patch).expect_err("rejected");

    assert!(
        matches!(error, PatchError::ContextMismatch { line: 2, .. }),
        "expected a context mismatch at line 2, got {error:?}"
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod fs {
    pub use super::diff_patch as diff_patch;
}
