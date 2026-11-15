//! Predicting how much a tool call will return, before the call is made.
//!
//! Two mechanisms already bound output, and both of them run **after** the output exists:
//!
//! - [`crate::spill`] receives the finished text and only then decides whether it is too large to
//!   return, replacing it with a locator;
//! - [`crate::output_budget`] divides one round's allowance across results whose sizes are already
//!   known to it.
//!
//! So neither mechanism can answer the question a caller has *before* acting: is this call about to
//! cost a context window, or a few hundred bytes? The gap is real, and it is not the same gap for
//! every tool. `read` names a file, and the file's size is on disk before the call runs — that is
//! knowledge, not a guess. `bash` names a program, and what a program prints is chosen by the
//! program: `true` prints nothing and a build prints megabytes. A module that gave both the same
//! answer would be lying about one of them.
//!
//! # Why a prediction is a number with a confidence rather than `None`
//!
//! `Option<u64>` would force every caller to branch on "known or not" and then invent its own
//! fallback for the unknown case — several fallbacks in several call sites, each with its own idea
//! of what "unknown" costs. Reporting the figure and how well it is supported keeps that decision in
//! one place. It also makes the honest case expressible: a `glob` is neither known nor unknowable,
//! and a single `Option` has no room for "about halfway".
//!
//! # Why the byte count is never zero
//!
//! Zero is a claim — "this call returns nothing" — and a caller that multiplies, sums or compares
//! will treat it as a fact rather than as an absence. A tool whose output cannot be bounded has a
//! *typical* size, not a size of zero, and
//! the typical size of a chatty call is a small number of kilobytes rather than none. The floor is
//! therefore one byte, applied in one place so that no future branch can publish a zero.
//!
//! # Why being wrong is harmless
//!
//! Nothing here enforces anything. The prediction is a hint: a caller may use it to choose a range
//! read instead of a whole-file read, or to warn before a call that is likely to be large. A wrong
//! prediction costs a hint that was not useful; it cannot refuse a call, truncate a result or
//! replace one with a locator — those remain the tools' and [`crate::spill`]'s decisions, made on
//! the real bytes. That is what makes it safe to be approximate, and what makes it wrong to be
//! authoritative.

use std::path::Path;

use girder_core::{MAX_READ_BYTES, SEARCH_DEFAULT_LIMIT, resolve_in_workspace, workspace_contains};
use serde_json::Value;

/// What one call is expected to return.
///
/// The three fields are meant to be read together: `bytes` without `confidence` says nothing about
/// whether it may be relied on, and `basis` is what lets a reader decide between the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prediction {
    /// The number of bytes the call is expected to return.
    ///
    /// Never zero. When the output cannot be bounded this is a typical figure rather than a bound,
    /// and [`Prediction::confidence`] is the field that says which of the two it is.
    pub bytes: u64,
    /// How well the estimate is supported, from 0 (no evidence) to 100 (the size is known).
    ///
    /// This measures *evidence*, not accuracy: a whole-file `read` is above 90 because the size is
    /// on disk, and a `bash` call is at 10 because nothing outside the command can bound it. A value
    /// that is low does not mean the figure is wrong — it means nothing here checked it.
    pub confidence: u8,
    /// What the figure was derived from, in one sentence.
    ///
    /// Written for whoever reads the prediction, so a low-confidence number can explain itself
    /// instead of inviting a reader to trust it.
    pub basis: String,
}

impl Prediction {
    /// Builds a prediction, holding the two published invariants.
    ///
    /// The floor and the cap are applied here rather than at each return site: a single place is
    /// what keeps a future branch from publishing a zero-byte prediction or a confidence above 100,
    /// and both would be read by a caller as a defect in the estimate rather than as a guess.
    fn new(bytes: u64, confidence: u8, basis: String) -> Self {
        Self {
            bytes: bytes.max(1),
            confidence: confidence.min(100),
            basis,
        }
    }
}

/// The figure used when nothing can bound a call's output.
///
/// 1 KiB, which is the size of a short listing or a small diagnostic. It is a central estimate for
/// "a tool said something", not a ceiling: the point of the number is to be non-zero and in the
/// right order of magnitude, since [`NOMINAL_CONFIDENCE`] tells the reader not to rely on it.
const NOMINAL_BYTES: u64 = 1024;

/// Confidence for a tool this module has no model of.
const NOMINAL_CONFIDENCE: u8 = 0;

/// Confidence for a command's output.
///
/// Not zero, because a command that runs usually does print *something*, and the nominal figure is
/// in that neighbourhood. Not more than this either, because a single `find` can outproduce every
/// other call in a session.
const UNBOUNDED_CONFIDENCE: u8 = 10;

/// Bytes of a one-line refusal, on top of the path it quotes.
///
/// A refused call still returns a message, and a prediction of zero for a call that certainly
/// produces a message would be wrong in the direction that matters: it would read as "nothing to
/// plan around".
const SHORT_MESSAGE_BYTES: u64 = 64;

/// Confidence that a call returns a one-line message instead of content.
///
/// Short of "certain": the prediction sees the filesystem and the arguments now, and the call runs
/// later, so a file that is missing here can exist by then.
const SHORT_MESSAGE_CONFIDENCE: u8 = 60;

/// The average number of bytes one line of a text file occupies.
///
/// Only used to turn a byte count into an estimated line count, which the read tool needs for its
/// per-line numbering. Wider lines shrink the estimate; that error is small next to the file size,
/// which is why a whole-file read still scores high.
const AVERAGE_LINE_BYTES: u64 = 40;

/// Bytes the read tool adds per line for its `{number}\t` prefix.
///
/// Six covers a five-digit line number and the tab, which is the common case for the files a read
/// tool is pointed at.
const LINE_NUMBER_BYTES: u64 = 6;

/// Confidence for a read of a whole file whose size is on disk.
///
/// The size is a fact and everything else is a rounding correction, so this is the highest figure
/// the module issues — but not 100, because the file can grow between the prediction and the call.
const WHOLE_FILE_CONFIDENCE: u8 = 92;

/// Confidence for a read bounded by `limit`.
///
/// The file's size is known but the output is not: the result is "at most this many lines", and
/// where those lines end is a property of content this module has not read.
const LIMITED_CONFIDENCE: u8 = 70;

/// Confidence for a read that starts at an `offset` and runs to the end.
///
/// The byte offset of a line number is only known after reading, so the file's whole size is the
/// bound rather than the estimate.
const OFFSET_CONFIDENCE: u8 = 60;

/// Average bytes one workspace-relative path occupies in a `glob` listing.
const GLOB_LINE_BYTES: u64 = 48;

/// Confidence for a `glob`, whose size follows a match count nothing here knows.
const GLOB_CONFIDENCE: u8 = 45;

/// Average bytes one `grep` hit occupies: `path:line:text`.
const GREP_LINE_BYTES: u64 = 96;

/// Confidence for a `grep`, one step below `glob`.
///
/// A glob's line length is fixed by the path; a grep's is fixed by nothing, since the matching text
/// is whatever the file happens to contain.
const GREP_CONFIDENCE: u8 = 40;

/// Predicts how much a tool call will return.
///
/// Only tools whose output has a shape this module understands are predicted from evidence:
///
/// - `read` is measured — a whole-file read is the file's size, which is known before the call;
/// - `glob` and `grep` are bounded — the tool's own `limit` caps the result, the match count does
///   not, so the figure is a middle-of-the-road guess between nothing and the limit;
/// - `bash` is not predictable at all — the command decides, and it has not run yet;
/// - every other tool falls back to [`NOMINAL_BYTES`] with no confidence at all.
///
/// A `read` whose path resolves outside `workspace` is *not* measured. Statting an arbitrary host
/// path would turn a hint into a filesystem probe, and the confinement this harness otherwise
/// enforces is not something a predictor should be the first to break; the honest answer for such a
/// path is that this module does not know.
///
/// # Errors
///
/// None: an unreadable path, an absent argument and an unknown tool are all predictions, not
/// failures. A caller that wants to act on "cannot tell" reads [`Prediction::confidence`].
#[must_use]
pub fn predict(tool: &str, arguments: &Value, workspace: &Path) -> Prediction {
    match tool {
        "read" => read(arguments, workspace),
        "glob" => glob(arguments),
        "grep" => grep(arguments),
        "bash" => bash(),
        other => unpredicted(other),
    }
}

/// Predicts a `read`.
fn read(arguments: &Value, workspace: &Path) -> Prediction {
    let Some(path) = arguments.get("path").and_then(Value::as_str) else {
        return Prediction::new(
            SHORT_MESSAGE_BYTES,
            SHORT_MESSAGE_CONFIDENCE,
            "`read` without a `path` is refused with a one-line message, before anything is opened"
                .to_owned(),
        );
    };
    if path.trim().is_empty() {
        return Prediction::new(
            SHORT_MESSAGE_BYTES,
            SHORT_MESSAGE_CONFIDENCE,
            "`read` with a blank `path` is refused with a one-line message, before anything is opened"
                .to_owned(),
        );
    }

    let resolved = resolve_in_workspace(Some(workspace), path);
    if !workspace_contains(workspace, &resolved) {
        // Deliberately not measured: see `predict`'s note on paths outside the workspace.
        return Prediction::new(
            NOMINAL_BYTES,
            NOMINAL_CONFIDENCE,
            format!(
                "`{path}` resolves outside the workspace, and this prediction does not stat paths it is not confined to, so the file's size is unknown"
            ),
        );
    }

    // The message a failed read returns quotes the path, so its length is the path's plus a fixed
    // tail; that keeps the estimate honest for a long path instead of pretending every error is one
    // size.
    let short_message = SHORT_MESSAGE_BYTES.saturating_add(byte_len(path));

    let Ok(metadata) = std::fs::metadata(&resolved) else {
        return Prediction::new(
            short_message,
            SHORT_MESSAGE_CONFIDENCE,
            format!(
                "`{path}` has no readable metadata, so `read` returns a short error message rather than content"
            ),
        );
    };
    if !metadata.is_file() {
        return Prediction::new(
            short_message,
            SHORT_MESSAGE_CONFIDENCE,
            format!(
                "`{path}` is not a regular file, so `read` returns a short error message rather than content"
            ),
        );
    }

    let size = metadata.len();
    // Over the limit the tool refuses and names the size; returning a megabyte-sized prediction for
    // a call that returns a hundred-byte message would be wrong by four orders of magnitude.
    if size > u64::try_from(MAX_READ_BYTES).unwrap_or(u64::MAX) {
        return Prediction::new(
            short_message,
            WHOLE_FILE_CONFIDENCE,
            format!(
                "`{path}` is {size} bytes, over the {MAX_READ_BYTES}-byte read limit, so `read` refuses and names the size instead of returning content"
            ),
        );
    }

    // Numbering is the only thing the read adds to the bytes it returns, and it is per line, so a
    // line count has to be estimated from the file's size alone — the cheapest honest correction.
    let lines = size / AVERAGE_LINE_BYTES;
    let whole_file = size.saturating_add(lines.saturating_mul(LINE_NUMBER_BYTES));

    if let Some(limit) = limit_of(arguments) {
        // `limit` caps the output at a number of lines; a line's length is not known, so the average
        // is the estimate, bounded by the file's own size.
        let capped = limit.min(lines.max(1));
        let bytes = capped
            .saturating_mul(AVERAGE_LINE_BYTES.saturating_add(LINE_NUMBER_BYTES))
            .min(whole_file);
        return Prediction::new(
            bytes,
            LIMITED_CONFIDENCE,
            format!(
                "`{path}` is {size} bytes and `limit` asks for at most {limit} lines; where those lines break is only known after reading"
            ),
        );
    }

    let offset = arguments
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    if offset > 1 {
        Prediction::new(
            whole_file,
            OFFSET_CONFIDENCE,
            format!(
                "`{path}` is {size} bytes and `offset` starts partway through, so the whole file's size is the bound: a line's byte offset is only known after reading"
            ),
        )
    } else {
        Prediction::new(
            whole_file,
            WHOLE_FILE_CONFIDENCE,
            format!(
                "`{path}` is {size} bytes on disk, and the difference is the line numbering a read adds"
            ),
        )
    }
}

/// Predicts a `glob`.
fn glob(arguments: &Value) -> Prediction {
    let pattern = arguments
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if pattern.trim().is_empty() {
        return Prediction::new(
            SHORT_MESSAGE_BYTES,
            SHORT_MESSAGE_CONFIDENCE,
            "`glob` without a `pattern` is refused with a one-line message".to_owned(),
        );
    }

    let limit = line_limit(arguments);
    // Half the limit: a glob that matches every file it could is the exception, and one that
    // matches nothing is common — but the answer is a size, so the midpoint is the useful guess.
    let expected = limit.div_ceil(2);
    Prediction::new(
        expected.saturating_mul(GLOB_LINE_BYTES),
        GLOB_CONFIDENCE,
        format!(
            "`{pattern}` may match anywhere from no files to the {limit} this call allows, each a workspace-relative path of roughly {GLOB_LINE_BYTES} bytes; half the limit is assumed"
        ),
    )
}

/// Predicts a `grep`.
fn grep(arguments: &Value) -> Prediction {
    let pattern = arguments
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if pattern.trim().is_empty() {
        return Prediction::new(
            SHORT_MESSAGE_BYTES,
            SHORT_MESSAGE_CONFIDENCE,
            "`grep` without a `pattern` is refused with a one-line message".to_owned(),
        );
    }

    let limit = line_limit(arguments);
    let expected = limit.div_ceil(2);
    Prediction::new(
        expected.saturating_mul(GREP_LINE_BYTES),
        GREP_CONFIDENCE,
        format!(
            "`{pattern}` may match no line or the {limit} this call allows, and a hit's length is whatever the matching line contains; half the limit at roughly {GREP_LINE_BYTES} bytes per hit is assumed"
        ),
    )
}

/// Predicts a `bash` call.
fn bash() -> Prediction {
    Prediction::new(
        NOMINAL_BYTES,
        UNBOUNDED_CONFIDENCE,
        "a shell command's output is chosen by the command and not by its arguments: `true` prints nothing and a build prints megabytes, so nothing here can bound it before it runs"
            .to_owned(),
    )
}

/// Predicts a tool this module has no model of.
fn unpredicted(tool: &str) -> Prediction {
    Prediction::new(
        NOMINAL_BYTES,
        NOMINAL_CONFIDENCE,
        format!(
            "`{tool}` has no output model here, so {NOMINAL_BYTES} bytes is a typical figure rather than a bound"
        ),
    )
}

/// The line ceiling this call will be run with, mirroring what the search tools do.
///
/// Read from their own default rather than repeated as a number: a prediction that assumed a
/// different limit than the tool applies would drift the first time either changed, and a drifting
/// bound is worse than a guess because a reader would treat it as exact.
fn line_limit(arguments: &Value) -> u64 {
    let default = u64::try_from(SEARCH_DEFAULT_LIMIT).unwrap_or(u64::MAX);
    // `.max(1)` as the tools do: a limit of zero is raised to one rather than returning nothing.
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .max(1)
}

/// The `limit` argument, when the call supplies one.
fn limit_of(arguments: &Value) -> Option<u64> {
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .map(|l| l.max(1))
}

/// The length of a string in bytes, without a lossy cast.
fn byte_len(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}
