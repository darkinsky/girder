//! Tests for `output_digest`, the classifier that decides which lines of a log explain it.
//!
//! Two properties carry this module, and both are about what a caller is allowed to conclude.
//! The first is that "0 errors" is a success: it is the single spelling most likely to make a
//! naive `contains("error")` report a green build as broken, and the classifier has to survive
//! every form of it — a count, a negation, a zero column, a Chinese counter. The second is that a
//! digest keeps the neighbours of a diagnostic, because an error line without its `-->` locator
//! and its source excerpt is a complaint without an address.
//!
//! The rest of the file pins the conservative half of the heuristic: words that merely look like
//! failures (`error_guard.rs`, `failsafe`, a test named `..._error_...`) stay [`Importance::Info`],
//! and noise is marked rather than dropped so a caller can still count the lines it did not show.
//!
//! The source is included by path rather than through a crate re-export, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/output_digest.rs"]
mod output_digest;

use output_digest::{Importance, classify, digest, has_errors};

/// The importances of a set of lines, in order, for assertions that care about the ranking.
fn ranked(output: &str) -> Vec<Importance> {
    classify(output)
        .iter()
        .map(|line| line.importance)
        .collect()
}

// ---- error lines are found ----------------------------------------------------------------

#[test]
fn an_error_prefix_is_an_error() {
    let output = "error[E0308]: mismatched types\n";

    assert_eq!(ranked(output), vec![Importance::Error]);
    assert!(has_errors(output));
}

#[test]
fn failed_and_cannot_are_errors() {
    // Neither line says "error", and both are how a tool actually reports a failure: a test runner
    // uppercase-marks the test, and the linker refuses without a diagnostic.
    let output = "test tests::it_works ... FAILED\ncannot find -lssl\n";

    assert_eq!(
        ranked(output),
        vec![Importance::Error, Importance::Error],
        "a failed test and a refused link are both failures"
    );
}

#[test]
fn an_error_anywhere_in_the_line_is_an_error() {
    let output = "thread 'main' panicked: cannot read config: permission denied\n";

    assert!(has_errors(output));
}

#[test]
fn a_real_failure_code_after_a_colon_is_not_a_zero_count() {
    // `error: 0x80070005` starts like `errors: 0` and is the opposite: a Windows failure code.
    // The zero-count rule must require a boundary after the digits to tell them apart.
    assert!(
        has_errors("error: 0x80070005\n"),
        "a hex code beginning with zero is a failure, not a count of zero"
    );
}

// ---- "0 errors" is a success (the core case) ------------------------------------------------

#[test]
fn zero_errors_is_not_an_error() {
    for output in [
        "0 errors\n",
        "errors: 0\n",
        "Errors: 0, Warnings: 0\n",
        "no errors\n",
        "finished without errors\n",
        "cargo build succeeded: 0 errors, 0 warnings\n",
        "0 errors found\n",
    ] {
        assert!(
            !has_errors(output),
            "{output:?} reports a count of zero and must not read as a failure"
        );
    }
}

#[test]
fn zero_errors_still_classifies_the_rest_of_the_line() {
    // The line is not an error, but it is not noise either: it is the summary a reader wants.
    assert_eq!(ranked("0 errors\n"), vec![Importance::Info]);
}

#[test]
fn a_nonzero_count_is_still_an_error() {
    // The negation rule must not swallow the real case: only an all-zero count negates.
    for output in ["10 errors\n", "errors: 3\n", "2 failed\n"] {
        assert!(has_errors(output), "{output:?} reports a real failure");
    }
}

// ---- warnings are found, and rank below errors ---------------------------------------------

#[test]
fn warnings_are_warnings() {
    let output = "warning: unused import `std::fmt`\n";
    assert_eq!(ranked(output), vec![Importance::Warning]);
    assert!(!has_errors(output), "a warning is not a failure");
}

#[test]
fn a_warning_beside_a_zero_error_count_is_still_a_warning() {
    let output = "finished with 0 errors and 3 warnings\n";

    assert_eq!(ranked(output), vec![Importance::Warning]);
    assert!(
        !has_errors(output),
        "three warnings do not make the build an error"
    );
}

#[test]
fn an_error_wins_over_a_warning_on_the_same_line() {
    let output = "error: unused variable `x`; warning treated as error\n";

    assert_eq!(ranked(output), vec![Importance::Error]);
}

// ---- conservative: lookalikes stay info ----------------------------------------------------

#[test]
fn a_path_containing_error_is_not_an_error() {
    // `_` joins the word, which is what keeps this file listing out of the error class.
    let output = "src/error_guard.rs\nsrc/failure_handler.rs:12: fn recover() {}\n";

    assert_eq!(
        ranked(output),
        vec![Importance::Info, Importance::Info],
        "an identifier that contains a failure word reports nothing"
    );
}

#[test]
fn a_test_name_containing_error_is_not_an_error() {
    let output = "test returns_error_on_empty_input ... ok\n";

    assert_eq!(ranked(output), vec![Importance::Info]);
    assert!(!has_errors(output));
}

#[test]
fn a_word_that_only_starts_like_a_failure_is_not_an_error() {
    // `failsafe` is not `fail`, and a prefix rule would call the line a failure.
    assert_eq!(ranked("failsafe mode enabled\n"), vec![Importance::Info]);
}

// ---- noise is marked, not deleted ----------------------------------------------------------

#[test]
fn rendering_noise_is_classified_as_noise() {
    let output = [
        "Compiling demo v0.1.0",
        "",                                 // blank
        "================================", // rule
        "----------",                       // rule
        "",                                 // blank
        "Compiling demo v0.1.0",            // adjacent duplicate of line 0? no: separated, so info
    ]
    .join("\n");

    assert_eq!(
        ranked(&output),
        vec![
            Importance::Info,
            Importance::Noise,
            Importance::Noise,
            Importance::Noise,
            Importance::Noise,
            Importance::Info,
        ],
        "only the adjacent repeat counts as noise"
    );
}

#[test]
fn an_adjacent_duplicate_is_noise() {
    let output = "Downloading crate\nDownloading crate\nDownloading crate\n";

    assert_eq!(
        ranked(output),
        vec![Importance::Info, Importance::Noise, Importance::Noise]
    );
}

#[test]
fn a_carriage_return_progress_line_is_noise() {
    // A repaint is transient by nature: the terminal showed only the last frame.
    let output = "Downloading [==>   ] 40%\rDownloading [=====>] 90%\rDownloading [======] 100%\n";

    let lines = classify(output);

    assert_eq!(lines.len(), 1, "one repaint sequence is one line");
    assert_eq!(lines[0].importance, Importance::Noise);
    assert_eq!(
        lines[0].text, "Downloading [======] 100%",
        "the visible text is the last frame a terminal would have left"
    );
}

#[test]
fn a_progress_line_that_ends_in_an_error_is_an_error() {
    // The content check runs before the noise rules: a repaint can end in the failure itself, and
    // calling that noise would hide the one line that mattered.
    let output = "Fetching crate\rerror: failed to download\n";

    assert_eq!(ranked(output), vec![Importance::Error]);
    assert!(has_errors(output));
}

#[test]
fn a_separator_mixed_with_letters_is_not_noise() {
    // A diff header is made of punctuation and identifiers; it is content, not a rule.
    assert_eq!(ranked("+++ b/src/main.rs\n"), vec![Importance::Info]);
}

#[test]
fn classify_drops_nothing() {
    let output = "error: boom\n\n====\rprogress 50%\rprogress 100%\nso-so\nso-so\n";

    let lines = classify(output);

    assert_eq!(
        lines.len(),
        output.lines().count(),
        "every input line is returned, noise included"
    );
}

// ---- digest keeps context (the core case) --------------------------------------------------

/// A build log whose single diagnostic sits in the middle, where a head/tail cut would remove it.
const BUILD_LOG: &str = "\
   Compiling demo v0.1.0
info: building crate graph
info: resolving dependencies
src/main.rs:10:5: let here
error[E0308]: mismatched types
   --> src/main.rs:10:5
   |
info: continuing
cargo: build aborted
   Finished in 0.3s";

#[test]
fn digest_keeps_the_error_and_its_context() {
    let text = digest(BUILD_LOG, 2);

    assert!(text.contains("error[E0308]: mismatched types"), "{text}");
    assert!(
        text.contains("   --> src/main.rs:10:5") && text.contains("   |"),
        "the locator and source excerpt after the error are what make it actionable:\n{text}"
    );
    assert!(
        text.contains("info: resolving dependencies")
            && text.contains("src/main.rs:10:5: let here"),
        "the lines before the error are part of the diagnostic:\n{text}"
    );
}

#[test]
fn digest_names_what_it_left_out() {
    let text = digest(BUILD_LOG, 2);

    assert!(!text.contains("Compiling demo v0.1.0"), "{text}");
    assert!(!text.contains("Finished in 0.3s"), "{text}");
    assert!(
        text.contains("[... 2 of 10 lines omitted ...]"),
        "the two lines before the window must be accounted for:\n{text}"
    );
    assert!(
        text.contains("[... 3 of 10 lines omitted ...]"),
        "the three lines after the window must be accounted for:\n{text}"
    );
}

#[test]
fn digest_without_context_keeps_only_the_key_lines() {
    let text = digest(BUILD_LOG, 0);

    assert_eq!(
        text,
        "[... 4 of 10 lines omitted ...]\nerror[E0308]: mismatched types\n[... 5 of 10 lines omitted ...]"
    );
}

#[test]
fn digest_merges_nearby_windows_without_repeating_lines() {
    let output = "a\nwarning: one\nerror: two\nb\n";

    let text = digest(output, 1);

    assert_eq!(
        text, "a\nwarning: one\nerror: two\nb",
        "overlapping context windows share their lines and print no marker"
    );
}

#[test]
fn digest_keeps_the_whole_output_when_the_context_covers_it() {
    let text = digest(BUILD_LOG, 100);

    assert_eq!(
        text, BUILD_LOG,
        "a window that covers everything cuts nothing"
    );
    assert!(!text.contains("omitted"));
}

#[test]
fn digest_of_output_without_errors_is_empty() {
    assert_eq!(digest("all good\nnothing to report\n", 3), "");
    assert_eq!(digest("0 errors\n", 3), "");
}

#[test]
fn digest_is_utf8_safe() {
    // Cuts land between lines, never inside a character: no replacement characters may appear.
    let output = "构建开始\n错误: 无法打开文件 src/main.rs\n构建结束\n";

    let text = digest(output, 1);

    assert!(text.contains("错误: 无法打开文件 src/main.rs"), "{text}");
    assert!(
        text.contains("构建开始") && text.contains("构建结束"),
        "{text}"
    );
    assert!(!text.contains('\u{FFFD}'));
}

// ---- has_errors is the "did it build" answer ------------------------------------------------

#[test]
fn has_errors_answers_on_the_classifier() {
    assert!(has_errors("error: boom\n"));
    assert!(has_errors("Build FAILED\n"));
    assert!(!has_errors("error: 0\n"));
    assert!(!has_errors("warning: only a warning\n"));
    assert!(!has_errors("Compiling demo v0.1.0\nFinished\n"));
}

#[test]
fn empty_output_has_no_errors() {
    assert!(!has_errors(""));
    assert!(classify("").is_empty());
    assert_eq!(digest("", 2), "");
}

#[test]
fn a_blank_only_output_is_noise_and_not_a_failure() {
    let lines = classify("\n\n");

    assert_eq!(lines.len(), 2);
    assert!(
        lines
            .iter()
            .all(|line| line.importance == Importance::Noise)
    );
    assert!(!has_errors("\n\n"));
}

// ---- Chinese output ------------------------------------------------------------------------

#[test]
fn chinese_failures_are_errors() {
    for output in ["错误: 无法打开文件\n", "构建失败\n", "编译出错\n"] {
        assert!(has_errors(output), "{output:?} reports a failure");
        assert_eq!(ranked(output), vec![Importance::Error], "{output:?}");
    }
}

#[test]
fn chinese_warnings_are_warnings() {
    let output = "警告: 未使用的导入\n";

    assert_eq!(ranked(output), vec![Importance::Warning]);
    assert!(!has_errors(output));
}

#[test]
fn chinese_zero_counts_are_not_errors() {
    // The Chinese shape of "0 errors": a zero, an optional counter, then the noun.
    for output in ["错误: 0\n", "0 个错误\n", "没有错误\n", "0 个失败\n"] {
        assert!(
            !has_errors(output),
            "{output:?} reports that there were none, and must not read as a failure"
        );
    }
}

#[test]
fn chinese_digest_keeps_context() {
    let text = digest("构建开始\n错误: 无法打开文件\n构建结束\n", 1);

    assert_eq!(
        text, "构建开始\n错误: 无法打开文件\n构建结束",
        "a three-line block fits whole, so nothing is omitted"
    );
}
