//! Tests for output-size prediction.
//!
//! The module exists because the two mechanisms that already bound output — [`spill`] and the round
//! [`budget`] — both act on a result that already exists. What is under test here is the *claim* the
//! predictor makes before that point: that it can tell a call whose size is on disk from one whose
//! size is decided by a program that has not run yet.
//!
//! So the load-bearing tests are not the ones that check a number. They are:
//!
//! - `bash` scores low and says why. A predictor that returned a confident figure for a shell
//!   command would be the failure this module was written to avoid, and it would pass every test
//!   that only asserted `bytes > 0`.
//! - An unpredicted tool still returns a non-zero size. Zero is a claim that a call returns
//!   nothing, which is a stronger and more harmful statement than "we do not know".
//! - A `read` of a path outside the workspace is *not* measured. Confinement is the point: a
//!   predictor that statted whatever it was handed would be an unbounded filesystem probe.
//!
//! The module is included by path rather than through the crate root, so these tests compile
//! against it before `lib.rs` declares it — wiring `pub mod output_predict;` and its re-exports
//! belongs to the integrator. `output_predict` imports from `girder_core` itself, which resolves
//! here because the module is compiled into a test binary that depends on the real crate.

#[path = "../src/core/output_predict.rs"]
pub mod output_predict;

use std::path::{Path, PathBuf};

use output_predict::{Prediction, predict};
use serde_json::{Value, json};

fn scratch(name: &str) -> PathBuf {
    // A counter as well as the pid: tests in one binary run in parallel and a shared directory
    // would have one test's file appear in another's listing.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-output-predict-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// Writes a file of exactly `bytes` bytes, with newlines so it looks like text.
fn write_sized(dir: &Path, name: &str, bytes: usize) -> PathBuf {
    let path = dir.join(name);
    let mut text = String::with_capacity(bytes);
    while text.len() < bytes {
        text.push_str("the quick brown fox jumps over the lazy dog\n");
    }
    text.truncate(bytes);
    std::fs::write(&path, &text).expect("written");
    path
}

fn predicted(tool: &str, arguments: &Value, workspace: &Path) -> Prediction {
    predict(tool, arguments, workspace)
}

// ---------------------------------------------------------------- read: measured

#[test]
fn a_whole_file_read_is_predicted_from_the_file_size() {
    // The size is a fact on disk, so this is the one case the module may be confident about.
    let dir = scratch("whole-file");
    let path = write_sized(&dir, "notes.txt", 4_000);

    let prediction = predicted("read", &json!({ "path": path.display().to_string() }), &dir);

    assert!(
        prediction.confidence >= 90,
        "a file's on-disk size is evidence, so confidence should be high: {prediction:?}"
    );
    assert!(
        prediction.bytes >= 4_000,
        "the prediction cannot be smaller than the content the read returns: {prediction:?}"
    );
    // The only extra is six-ish bytes of line numbering per estimated line.
    assert!(
        prediction.bytes < 4_000 + 4_000 / 40 * 6 + 64,
        "the estimate should be the size plus numbering, not a guess: {prediction:?}"
    );
    assert!(
        prediction.basis.contains("bytes on disk"),
        "the basis should say where the figure came from: {prediction:?}"
    );
}

#[test]
fn a_relative_read_inside_the_workspace_is_measured_too() {
    // A relative path is the spelling a caller actually uses, so the measurement cannot depend on
    // the caller having made the path absolute.
    let dir = scratch("relative");
    write_sized(&dir, "small.rs", 1_000);

    let prediction = predicted("read", &json!({ "path": "small.rs" }), &dir);

    assert!(prediction.confidence >= 90, "{prediction:?}");
    assert!(prediction.bytes >= 1_000, "{prediction:?}");
}

#[test]
fn a_limited_read_is_less_confident_than_a_whole_file_read() {
    // The file's size is known but where the requested lines break is not, so the confidence has to
    // fall — otherwise the two cases would be indistinguishable to a caller.
    let dir = scratch("limited");
    let path = write_sized(&dir, "big.txt", 40_000);

    let whole = predicted("read", &json!({ "path": path.display().to_string() }), &dir);
    let limited = predicted(
        "read",
        &json!({ "path": path.display().to_string(), "limit": 10 }),
        &dir,
    );

    assert!(
        limited.confidence < whole.confidence,
        "a bounded read is a weaker claim than a measured one: {limited:?} vs {whole:?}"
    );
    assert!(limited.confidence >= 60, "{limited:?}");
    assert!(
        limited.bytes < whole.bytes,
        "ten lines must be predicted smaller than the whole file: {limited:?} vs {whole:?}"
    );
}

#[test]
fn an_offset_read_is_bounded_by_the_whole_file() {
    // A line number's byte offset is only known after reading, so the whole file is the bound.
    let dir = scratch("offset");
    let path = write_sized(&dir, "long.txt", 8_000);

    let prediction = predicted(
        "read",
        &json!({ "path": path.display().to_string(), "offset": 50 }),
        &dir,
    );

    assert!(prediction.confidence >= 50, "{prediction:?}");
    assert!(
        prediction.bytes >= 8_000,
        "the bound cannot be below the file's size: {prediction:?}"
    );
    assert!(prediction.basis.contains("offset"), "{prediction:?}");
}

#[test]
fn a_file_over_the_read_limit_is_predicted_as_a_refusal() {
    // The tool refuses rather than truncating, so predicting the file's size here would be wrong by
    // orders of magnitude: what comes back is a message naming the size.
    let dir = scratch("over-limit");
    let path = dir.join("huge.bin");
    let file = std::fs::File::create(&path).expect("created");
    file.set_len(2 * 1024 * 1024).expect("sized");
    drop(file);

    let prediction = predicted("read", &json!({ "path": path.display().to_string() }), &dir);

    assert!(
        prediction.bytes < 2 * 1024 * 1024,
        "an oversized read returns a message, not the file: {prediction:?}"
    );
    assert!(
        prediction.basis.contains("refuses"),
        "the basis should say the call is refused: {prediction:?}"
    );
}

#[test]
fn a_read_of_a_missing_file_predicts_the_error_message() {
    // The call still returns text, so the prediction is small but not zero — and the file may exist
    // by the time the call runs, which is why the confidence is not 100.
    let dir = scratch("missing");
    let prediction = predicted("read", &json!({ "path": "does-not-exist.txt" }), &dir);

    assert!(prediction.bytes > 0, "an error message is still bytes");
    assert!(
        prediction.bytes < 1_000,
        "an error message is short: {prediction:?}"
    );
    assert!(
        prediction.confidence < 90,
        "a missing file may exist when the call runs, so this is not certain: {prediction:?}"
    );
    assert!(prediction.confidence > 0, "{prediction:?}");
}

// ------------------------------------------------------- bash: not predictable

#[test]
fn bash_is_predicted_with_low_confidence() {
    // The core property. A command's output is chosen by the command; two `bash` calls with the
    // same arguments can differ by megabytes. A confident figure here would be a lie a caller might
    // plan around, so the confidence must stay near zero and the basis must say why.
    let dir = scratch("bash");
    let prediction = predicted("bash", &json!({ "command": "cargo build" }), &dir);

    assert!(
        prediction.confidence <= 10,
        "a shell command's output cannot be bounded before it runs: {prediction:?}"
    );
    assert!(
        prediction.bytes > 0,
        "an unpredicted call still has a typical size, not a size of zero: {prediction:?}"
    );
    assert!(
        prediction.basis.contains("command"),
        "the basis should name the reason the output cannot be bounded: {prediction:?}"
    );
}

#[test]
fn bash_confidence_does_not_depend_on_the_command_text() {
    // A predictor that pattern-matched `ls` as "small" and `find` as "large" would be trusting the
    // spelling of a command to bound what the command prints. It cannot, so both must score alike.
    let dir = scratch("bash-stable");
    let quiet = predicted("bash", &json!({ "command": "true" }), &dir);
    let loud = predicted("bash", &json!({ "command": "find /" }), &dir);

    assert_eq!(quiet.confidence, loud.confidence, "{quiet:?} vs {loud:?}");
    assert_eq!(quiet.bytes, loud.bytes);
}

// -------------------------------------------------- glob and grep: mid confidence

#[test]
fn glob_is_predicted_with_middle_confidence() {
    // Between "measured" and "unknown": the call's own limit bounds the result, but the match count
    // does not, so the figure is a guess that is at least of the right order.
    let dir = scratch("glob");
    let prediction = predicted("glob", &json!({ "pattern": "**/*.rs" }), &dir);

    assert!(
        (20..80).contains(&prediction.confidence),
        "a glob's size depends on a match count nothing here knows: {prediction:?}"
    );
    assert!(prediction.bytes > 0, "{prediction:?}");
    assert!(
        prediction.basis.contains("match"),
        "the basis should name the unknowable: {prediction:?}"
    );
}

#[test]
fn glob_confidence_is_between_bash_and_a_measured_read() {
    // The three tiers have to be distinguishable, or a caller cannot use the confidence to decide
    // whether to act on the figure at all.
    let dir = scratch("tiers");
    let path = write_sized(&dir, "measured.txt", 500);

    let measured = predicted("read", &json!({ "path": path.display().to_string() }), &dir);
    let globbed = predicted("glob", &json!({ "pattern": "*.txt" }), &dir);
    let shell = predicted("bash", &json!({ "command": "ls" }), &dir);

    assert!(
        measured.confidence > globbed.confidence,
        "{measured:?} vs {globbed:?}"
    );
    assert!(
        globbed.confidence > shell.confidence,
        "{globbed:?} vs {shell:?}"
    );
}

#[test]
fn a_glob_limit_scales_the_prediction() {
    // The one thing a glob's size *is* bounded by is its own limit, so the prediction must respond
    // to it rather than being a constant.
    let dir = scratch("glob-limit");
    let small = predicted("glob", &json!({ "pattern": "*.rs", "limit": 10 }), &dir);
    let large = predicted("glob", &json!({ "pattern": "*.rs", "limit": 1_000 }), &dir);

    assert!(small.bytes < large.bytes, "{small:?} vs {large:?}");
}

#[test]
fn grep_is_less_confident_than_glob() {
    // A glob's line length is set by the path; a grep hit's is set by whatever the matching line
    // contains, which is strictly less predictable.
    let dir = scratch("grep");
    let globbed = predicted("glob", &json!({ "pattern": "**/*.rs" }), &dir);
    let grepped = predicted("grep", &json!({ "pattern": "TODO" }), &dir);

    assert!(grepped.confidence > 0, "{grepped:?}");
    assert!(
        grepped.confidence < globbed.confidence,
        "{grepped:?} vs {globbed:?}"
    );
    assert!(grepped.bytes > 0, "{grepped:?}");
}

// ------------------------------------------------------ never zero, never a probe

#[test]
fn an_unknown_tool_is_predicted_non_zero_and_without_confidence() {
    // The core invariant: "we do not know" is expressed as a low confidence, not as a zero byte
    // count. A zero would be read by a caller as "this call returns nothing" and planned around.
    let dir = scratch("unknown");
    let prediction = predicted("some_future_tool", &json!({}), &dir);

    assert_eq!(
        prediction.confidence, 0,
        "no model of the tool means no evidence: {prediction:?}"
    );
    assert!(
        prediction.bytes > 0,
        "an unknown tool still returns a non-zero figure: {prediction:?}"
    );
    assert!(
        prediction.basis.contains("some_future_tool"),
        "the basis should name the tool it has no model of: {prediction:?}"
    );
}

#[test]
fn an_empty_argument_object_still_predicts_non_zero() {
    // A malformed call is the case most likely to produce a zero by accident, because every field
    // the predictors read is absent.
    let dir = scratch("empty-args");
    for tool in ["read", "glob", "grep", "bash", "write"] {
        let prediction = predicted(tool, &json!({}), &dir);
        assert!(
            prediction.bytes > 0,
            "`{tool}` with no arguments must still predict a message's worth of bytes: {prediction:?}"
        );
        assert!(!prediction.basis.is_empty(), "`{tool}` must say why");
    }
}

#[test]
fn empty_and_blank_paths_are_refusals_rather_than_unknowns() {
    // A blank path is a call that certainly fails with a message — not a call whose size nobody can
    // bound — so the confidence should be that of a known short result.
    let dir = scratch("blank-path");

    for arguments in [json!({ "path": "" }), json!({ "path": "   " })] {
        let prediction = predicted("read", &arguments, &dir);
        assert!(prediction.bytes > 0, "{prediction:?}");
        assert!(prediction.bytes < 1_000, "{prediction:?}");
        assert!(
            prediction.basis.contains("refused"),
            "the basis should say the call is refused: {prediction:?}"
        );
    }
}

#[test]
fn a_read_outside_the_workspace_is_not_measured() {
    // Confinement is the reason: measuring this would mean statting an arbitrary host path, which
    // turns a hint into a probe. The honest answer is that the size is unknown.
    let dir = scratch("escape");
    let prediction = predicted("read", &json!({ "path": "../../etc/passwd" }), &dir);

    assert_eq!(
        prediction.confidence, 0,
        "an unconfined path is not measured: {prediction:?}"
    );
    assert!(
        prediction.bytes > 0,
        "not measuring is not the same as predicting nothing: {prediction:?}"
    );
    assert!(
        prediction.basis.contains("outside the workspace"),
        "the basis should say why it was not measured: {prediction:?}"
    );
}

#[test]
fn an_absolute_path_outside_the_workspace_is_not_measured_even_if_it_exists() {
    // The escape that matters is not a `..` spelling but any path the caller can name: a predictor
    // that measured every absolute path would report the size of any file on the host.
    let dir = scratch("escape-absolute");
    let outside = scratch("escape-absolute-target");
    let path = write_sized(&outside, "secret.txt", 2_000);

    let prediction = predicted("read", &json!({ "path": path.display().to_string() }), &dir);

    assert_eq!(prediction.confidence, 0, "{prediction:?}");
    assert!(
        prediction.bytes < 2_000,
        "the outside file's size must not leak into the prediction: {prediction:?}"
    );
}

#[test]
fn a_measured_read_and_an_unmeasured_one_are_told_apart_by_confidence_alone() {
    // A caller that only sees `bytes` cannot distinguish a 500-byte measurement from a 500-byte
    // guess, so the confidence is the field carrying the meaning. This test pins that it does.
    let dir = scratch("told-apart");
    let path = write_sized(&dir, "known.txt", 500);

    let measured = predicted("read", &json!({ "path": path.display().to_string() }), &dir);
    let guessed = predicted("read", &json!({ "path": "/nonexistent/outside" }), &dir);

    assert!(measured.confidence > guessed.confidence, "{measured:?}");
    assert!(measured.bytes > 0 && guessed.bytes > 0);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod core {
    pub use super::output_predict as output_predict;
}
