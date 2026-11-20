//! Tests for retaining terminal output across the process boundary.
//!
//! The properties here are the ones a caller cannot check for itself once the transcript is on
//! disk, weighted toward the ways retention can look successful while losing what mattered:
//!
//! - **a cut keeps the end, not the beginning.** The last line is the error message or the prompt;
//!   the first is the banner. A cut from the wrong end is a store full of startup noise.
//! - **`truncated` matches what was actually dropped.** A caller reads it as "this is a suffix", so
//!   reporting `false` over a cut transcript makes a partial history read as the whole one.
//! - **empty output is refused.** A silently stored empty transcript is indistinguishable from a
//!   terminal that never printed, and the caller would believe the output was saved.
//! - **what `read` returns is what was written.** Bytes and lines are counted separately, so a
//!   transcript can pass the byte check and still be cut on the wrong boundary.
//!
//! The module is included by path rather than through the crate root, because the crate root is
//! wired up separately. It resolves `girder_core::…` through the external crate, so these tests
//! compile against the module as written and against the real store it writes to.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/terminal/retain.rs"]
pub mod terminal_retain;

use std::path::PathBuf;

use girder_core::TerminalPersistStore;
use terminal_retain::{Request, RetainError, read, retain};

// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh scratch directory per test, under the temp directory.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-terminal-retain-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created a scratch directory");
    dir
}

/// A store writing into a fresh scratch directory of its own.
fn store(name: &str) -> TerminalPersistStore {
    TerminalPersistStore::new(scratch(name).join("store"))
}

/// A request for `id` keeping `lines` trailing lines.
fn keep(id: &str, lines: Option<usize>) -> Request {
    Request {
        id: id.to_owned(),
        lines,
    }
}

// ---- keeping everything ------------------------------------------------------------------------

#[test]
fn no_limit_keeps_the_whole_transcript() {
    let store = store("all");
    let output = "one\ntwo\nthree\n";

    let retained = retain(&store, &keep("t1", None), output).expect("retained");

    assert_eq!(retained.bytes, 14);
    assert_eq!(retained.lines, 3);
    assert!(!retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some(output));
}

#[test]
fn no_limit_keeps_a_transcript_without_a_trailing_newline() {
    let store = store("all-unterminated");
    let output = "one\ntwo\nthree";

    let retained = retain(&store, &keep("t1", None), output).expect("retained");

    assert_eq!(retained.bytes, 13);
    assert_eq!(retained.lines, 3);
    assert!(!retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some(output));
}

#[test]
fn no_limit_keeps_a_single_unterminated_line() {
    let store = store("all-single");
    let output = "$ cargo build";

    let retained = retain(&store, &keep("t1", None), output).expect("retained");

    assert_eq!(retained.bytes, 13);
    assert_eq!(retained.lines, 1);
    assert!(!retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some(output));
}

#[test]
fn a_limit_at_or_above_the_line_count_keeps_everything() {
    let store = store("limit-above");
    let output = "one\ntwo\nthree\n";

    let retained = retain(&store, &keep("t1", Some(3)), output).expect("retained");

    assert_eq!(retained.lines, 3);
    assert!(
        !retained.truncated,
        "keeping every line is not a truncation"
    );
    assert_eq!(read(&store, "t1").as_deref(), Some(output));
}

// ---- keeping the tail --------------------------------------------------------------------------

#[test]
fn a_limit_keeps_the_last_lines() {
    let store = store("tail");
    let output = "one\ntwo\nthree\nfour\n";

    let retained = retain(&store, &keep("t1", Some(2)), output).expect("retained");

    assert_eq!(retained.bytes, 11);
    assert_eq!(retained.lines, 2);
    assert!(retained.truncated);
    assert_eq!(
        read(&store, "t1").as_deref(),
        Some("three\nfour\n"),
        "the tail is the part worth keeping"
    );
}

#[test]
fn a_limit_keeps_the_last_line_of_an_unterminated_transcript() {
    let store = store("tail-unterminated");
    let output = "one\ntwo\nthree";

    let retained = retain(&store, &keep("t1", Some(1)), output).expect("retained");

    assert_eq!(retained.lines, 1);
    assert!(retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some("three"));
}

#[test]
fn a_limit_of_one_keeps_only_the_final_line() {
    let store = store("tail-one");
    let output = "banner\nnoise\nerror: it broke\n";

    let retained = retain(&store, &keep("t1", Some(1)), output).expect("retained");

    assert_eq!(retained.lines, 1);
    assert!(retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some("error: it broke\n"));
}

#[test]
fn a_trailing_newline_does_not_spend_one_of_the_kept_lines() {
    let store = store("tail-blank");
    let output = "first\nsecond\n";

    // Two lines and two newlines: the newline closes the second line rather than opening a third,
    // so a cut of two keeps the whole transcript instead of a leading blank line.
    let retained = retain(&store, &keep("t1", Some(2)), output).expect("retained");

    assert_eq!(retained.lines, 2);
    assert!(!retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some(output));
}

// ---- truncation reporting ----------------------------------------------------------------------

#[test]
fn truncation_is_reported_only_when_output_was_dropped() {
    let store = store("truncated-flag");
    let output = "one\ntwo\nthree\n";

    let whole = retain(&store, &keep("t1", Some(3)), output).expect("retained");
    assert!(!whole.truncated);

    let cut = retain(&store, &keep("t2", Some(2)), output).expect("retained");
    assert!(cut.truncated);

    // The flag describes the cut, not the size: the same transcript can be stored whole by one
    // request and partially by another.
    assert!(whole.bytes > cut.bytes);
}

#[test]
fn a_limit_above_the_line_count_is_not_reported_as_truncation() {
    let store = store("truncated-none");
    let output = "one\ntwo\n";

    let retained = retain(&store, &keep("t1", Some(99)), output).expect("retained");

    assert!(!retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some(output));
}

// ---- refusing nothing --------------------------------------------------------------------------

#[test]
fn empty_output_is_refused() {
    let store = store("empty");

    let error = retain(&store, &keep("t1", None), "").expect_err("empty output must be refused");

    assert_eq!(error, RetainError::Empty);
    assert_eq!(
        read(&store, "t1"),
        None,
        "a refused retain must leave nothing behind"
    );
}

#[test]
fn empty_output_with_a_limit_is_refused_before_the_cut() {
    let store = store("empty-limited");

    let error = retain(&store, &keep("t1", Some(3)), "").expect_err("refused");

    assert_eq!(error, RetainError::Empty);
    assert_eq!(read(&store, "t1"), None);
}

#[test]
fn keeping_zero_lines_is_refused() {
    let store = store("zero-lines");

    let error =
        retain(&store, &keep("t1", Some(0)), "one\ntwo\n").expect_err("zero lines keeps nothing");

    assert_eq!(error, RetainError::Empty);
    assert_eq!(read(&store, "t1"), None);
}

#[test]
fn a_bad_id_is_reported_rather_than_stored() {
    let store = store("bad-id");

    let error = retain(&store, &keep("../escape", None), "one\n").expect_err("refused");

    assert!(
        matches!(error, RetainError::Io { .. }),
        "the store's rejection is carried through as an io failure, got {error:?}"
    );
    assert_eq!(read(&store, "../escape"), None);
}

// ---- reading back ------------------------------------------------------------------------------

#[test]
fn reading_an_unknown_terminal_yields_nothing() {
    let store = store("read-unknown");

    assert_eq!(read(&store, "never-retained"), None);
}

#[test]
fn a_second_retain_replaces_the_first() {
    let store = store("replace");

    retain(&store, &keep("t1", None), "old\n").expect("retained");
    retain(&store, &keep("t1", None), "new\n").expect("retained");

    assert_eq!(
        read(&store, "t1").as_deref(),
        Some("new\n"),
        "a retain stores the transcript as a whole, so it cannot append"
    );
}

// ---- awkward text ------------------------------------------------------------------------------

#[test]
fn one_very_long_line_is_kept_whole() {
    let store = store("long-line");
    // A progress bar redrawn on a single line is the shape that breaks a byte-based cut.
    let output = format!("{}\n", "x".repeat(200_000));

    let retained = retain(&store, &keep("t1", Some(1)), output.as_str()).expect("retained");

    assert_eq!(retained.lines, 1);
    assert_eq!(retained.bytes, 200_001);
    assert!(!retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some(output.as_str()));
}

#[test]
fn a_cut_after_one_long_line_starts_on_a_line_boundary() {
    let store = store("long-line-cut");
    let output = format!("{}\nshort\n", "y".repeat(100_000));

    let retained = retain(&store, &keep("t1", Some(1)), output.as_str()).expect("retained");

    assert_eq!(retained.lines, 1);
    assert_eq!(retained.bytes, 6);
    assert!(retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some("short\n"));
}

#[test]
fn a_cut_never_splits_a_multibyte_character() {
    let store = store("multibyte");
    // Three-byte characters on every line, one of which the cut must start after.
    let output = "日本語\n한국어\n中文\n";

    let retained = retain(&store, &keep("t1", Some(1)), output).expect("retained");

    assert_eq!(retained.lines, 1);
    assert_eq!(read(&store, "t1").as_deref(), Some("中文\n"));
    // A cut into the middle of a character would have replaced it with U+FFFD on the way in.
    assert!(!read(&store, "t1").unwrap_or_default().contains('\u{fffd}'));
}

#[test]
fn blank_lines_are_kept_and_counted() {
    let store = store("blank-lines");
    let output = "a\n\nb\n\n";

    let retained = retain(&store, &keep("t1", None), output).expect("retained");

    assert_eq!(retained.lines, 4);
    assert!(!retained.truncated);
    assert_eq!(read(&store, "t1").as_deref(), Some(output));
}

// ---- the cut rule, directly --------------------------------------------------------------------

#[test]
fn the_line_rule_matches_the_cut() {
    let store = store("rule");

    // The count a retain reports must be the number of lines a reader of the stored text sees, so
    // an unterminated line counts and a trailing newline does not add a line of its own. The
    // expectations are the ones `str::lines` gives, so this rule agrees with the rest of Rust.
    let cases = [
        ("a\n", 1_usize),
        ("a", 1),
        ("a\nb\n", 2),
        ("a\nb", 2),
        // A lone newline is one empty line, not zero and not two.
        ("\n", 1),
        // The second newline terminates an empty second line; it does not open a third.
        ("a\n\n", 2),
    ];

    for (index, (output, expected)) in cases.into_iter().enumerate() {
        let id = format!("t{index}");
        let retained = retain(&store, &keep(&id, None), output).expect("retained");
        assert_eq!(
            retained.lines, expected,
            "{output:?} should count as {expected} lines"
        );
        assert_eq!(read(&store, &id).as_deref(), Some(output));
    }
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod terminal {
    pub use super::terminal_retain as retain;
}
