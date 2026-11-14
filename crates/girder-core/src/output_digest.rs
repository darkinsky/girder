//! Picking the lines that explain a result, rather than the lines that fit a budget.
//!
//! # The gap this closes
//!
//! Two mechanisms already bound an oversized result, and both answer a *size* question.
//! `tool_result_pruning` keeps a result's head and tail and drops the middle; `tool_output_view`
//! enforces a character budget across a result's content blocks. Neither knows which lines carry
//! meaning, and for a build log that is the difference that matters.
//!
//! A five-thousand-line build log keeps, say, forty head lines and forty tail lines. The head is
//! the invocation and the crate list; the tail is the closing summary. The diagnostic — the
//! `error[E0308]` that names the mismatched types, and the `-->` line that locates it — sits at
//! line two thousand three hundred, which is precisely the region a head/tail cut removes. The
//! model then reads a log that has been trimmed to its least informative parts and reports on a
//! build it never saw fail.
//!
//! So this module answers a *relevance* question: rank every line, keep all of them, and let the
//! caller decide how much of the ranking to spend. A build failure is carried by a handful of
//! lines; the rest is progress output and repetition.
//!
//! # Nothing is dropped here except by the caller's request
//!
//! [`classify`] returns one [`Line`] per input line, noise included, in input order. Dropping
//! lines inside the classifier would merge two decisions — "what does this line mean" and "how
//! much can I afford" — and the second one belongs to the caller that knows the budget.
//! [`digest`] is the only function here that removes text, and it names how much it removed.
//!
//! # The heuristics lean toward silence
//!
//! A missed error means a failed build reported as successful, so one might expect the classifier
//! to lean toward calling things errors. It leans the other way, because a marker the model has
//! learned to distrust is worse than a marker that is occasionally absent.
//!
//! The tempting rule — "a line containing `error` is an error" — fires on `0 errors` (a success
//! line), on `src/error_guard.rs` in a file listing, and on a test named
//! `test_returns_error_on_empty_input ... ok`. Each false positive teaches the model that the
//! error marker means nothing, and the next real error is skimmed past. So a line counts as an
//! error only when a failure word appears as a **standalone word** (an underscore or a letter
//! joins it to its neighbours, which is what excludes `error_guard.rs`) *and* that occurrence is
//! not negated by a zero count or a "no"/"none"/"without" in front of it. [`has_errors`] is
//! defined on top of [`classify`] rather than beside it, so the two cannot disagree.
//!
//! # Noise is a category, not a deletion
//!
//! Carriage-return progress repaints, blank lines, rule lines (`====`) and immediate duplicates
//! are marked [`Importance::Noise`]. They stay in the returned vector; a caller that renders
//! [`Line`]s can filter the marker out, and one that counts lines still sees the true total.
//!
//! A content check runs before the noise rules on purpose. A line that a carriage return rewrote
//! can still end in an error message (`\rerror: no such file`), and calling that noise would hide
//! a real failure behind a rendering detail.

/// How much one line of output matters for the question "what happened here".
///
/// The order is the order of usefulness to a model reading a log: an error is the reason to look
/// at the log at all, a warning is the thing that will become an error later, `Info` is the body
/// of the log, and `Noise` is text a terminal would have overwritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Importance {
    /// The line reports a failure: a diagnostic, a failed test, a command that could not run.
    ///
    /// The most important category, and the only one [`has_errors`] answers on.
    Error,
    /// The line reports something wrong that did not stop the run.
    ///
    /// Ranked second because a warning is often the earliest evidence of the failure that comes
    /// next: a deprecation, an unused import, a retry.
    Warning,
    /// The line is ordinary output: a path, a count, a step that succeeded.
    Info,
    /// The line is an artefact of rendering rather than content: a progress repaint that was
    /// overwritten in place, a blank line, a rule line, or a line identical to the one before it.
    Noise,
}

impl Importance {
    /// Whether this line is one a reader should be shown when only a few lines fit.
    ///
    /// A private helper because it is this module's policy, not the caller's: warnings are kept
    /// beside errors so a caller does not have to rediscover the ranking.
    const fn is_key(self) -> bool {
        matches!(self, Self::Error | Self::Warning)
    }
}

/// One line of output and the verdict about it.
///
/// The text is the *visible* text: for a line a carriage return rewrote in place, this is the
/// final frame, which is what a terminal would leave on screen. Nothing else about the line is
/// changed — leading indentation is kept, because indentation is what makes a stack trace or a
/// `note:` block readable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The line as a reader would see it, without a trailing carriage return or newline.
    pub text: String,
    /// What this line means for the question "did this succeed".
    pub importance: Importance,
}

/// Classifies every line of `output`, in order, dropping nothing.
///
/// The returned vector has exactly one entry per line of `output` (`str::lines` semantics: a
/// trailing newline does not produce a final empty line, and a bare `\r` does not end a line).
/// Empty input yields an empty vector, which is the same shape as a result with no output rather
/// than a special case a caller has to learn.
///
/// # How each line is decided
///
/// 1. Content first: a failure or warning word that is a standalone, non-negated word wins, so a
///    progress line that ends in `error: no such file` is [`Importance::Error`], not noise.
/// 2. Then rendering noise: a repainted line (interior `\r`), a blank line, a rule line, or an
///    exact repeat of the line above it.
/// 3. Otherwise [`Importance::Info`].
///
/// The duplicate rule is deliberately the narrowest one that is still useful: only *adjacent*
/// identical lines are demoted, so a repeated error over time still reads as an error each time
/// it occurs.
#[must_use]
pub fn classify(output: &str) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    for raw in output.lines() {
        let text = visible(raw);
        let importance = if let Some(level) = read_line(text) {
            level
        } else {
            let repeats_previous = lines.last().is_some_and(|line| line.text == text);
            if is_blank(text) || is_separator(text) || was_rewritten(raw) || repeats_previous {
                Importance::Noise
            } else {
                Importance::Info
            }
        };
        lines.push(Line {
            text: text.to_owned(),
            importance,
        });
    }
    lines
}

/// Extracts the error and warning lines, with `context` lines on each side of every one of them.
///
/// # Why context is not optional
///
/// An isolated error line is usually not enough to act on. A rustc diagnostic is an `error[E0308]`
/// line, a `-->` that names the file and line, a source excerpt, and a `help:` — the error line
/// alone says the types mismatched without saying where. In a compiler log the context that
/// matters is therefore the lines *after* the error, and in a test log it is the lines *before*
/// it (the test name, the assertion that failed). Keeping `context` lines on both sides covers
/// both shapes without the caller having to know which log it has.
///
/// # Output shape
///
/// Key regions are separated by a marker that names how many lines were left out, in lines rather
/// than characters, because a log is read in lines. The marker is present for omitted text at the
/// start and the end too: a digest that begins with a diagnostic should still say that text came
/// before it, or a reader will take the first retained line for the first line of the output.
///
/// Returns an empty string when the output holds no error or warning line. That is a real answer
/// — "there is nothing to look at" — and it is the same condition [`has_errors`] reports, so a
/// caller that shows nothing can still explain why.
///
/// A `context` of zero keeps the key lines alone; a `context` past the start or the end of the
/// output is clamped rather than treated as an error.
#[must_use]
pub fn digest(output: &str, context: usize) -> String {
    let lines = classify(output);
    let key: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.importance.is_key())
        .map(|(index, _)| index)
        .collect();
    if key.is_empty() {
        return String::new();
    }

    // Merge the windows as they are produced instead of collecting every index and sorting: the
    // key indices are already ascending, so a window that starts no later than one past the
    // previous window's end extends it. Overlapping windows must not print a line twice, and
    // adjacent ones must not print an omission marker for zero lines.
    let total = lines.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &index in &key {
        let start = index.saturating_sub(context);
        let end = index.saturating_add(context).min(total - 1);
        match ranges.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => ranges.push((start, end)),
        }
    }

    let mut pieces: Vec<String> = Vec::new();
    let mut next = 0usize;
    for (start, end) in ranges {
        if start > next {
            pieces.push(omission(start - next, total));
        }
        pieces.extend(lines[start..=end].iter().map(|line| line.text.clone()));
        next = end + 1;
    }
    if next < total {
        pieces.push(omission(total - next, total));
    }
    pieces.join("\n")
}

/// Whether `output` contains a line classified as an error.
///
/// This is the "did it build" predicate, so it is defined on top of [`classify`] rather than
/// beside it: two implementations of the same heuristic drift, and the caller that shows a digest
/// and the caller that decides success are looking at the same log. The consequence to know about
/// is that the answer inherits the classifier's conservatism — `0 errors`, `no errors`, and
/// `errors: 0` are all `false`, because they report a count of zero rather than a failure.
///
/// Empty output has no errors: a command that printed nothing and exited zero is a success, and
/// an empty result is not evidence of failure.
#[must_use]
pub fn has_errors(output: &str) -> bool {
    classify(output)
        .iter()
        .any(|line| line.importance == Importance::Error)
}

/// The visible text of one raw line: what a terminal would leave after in-place rewrites.
///
/// `str::lines` has already dropped one trailing `\r` of a CRLF ending, so any `\r` still present
/// separates repaints of the same line. The final frame is returned because that is the text a
/// reader would have seen; the earlier frames are unrecoverable by design, which is exactly why
/// the line is reported as rewritten.
fn visible(raw: &str) -> &str {
    let body = raw.trim_end_matches('\r');
    body.rsplit_once('\r').map_or(body, |(_, last)| last)
}

/// Whether the raw line was rewritten in place at least once.
///
/// Checked on the raw line rather than the visible text: after the last frame is taken, nothing
/// in the text says a rewrite happened, and it is the rewrite that makes the line transient.
fn was_rewritten(raw: &str) -> bool {
    raw.trim_end_matches('\r').contains('\r')
}

/// Whether the line carries no content at all.
fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

/// Whether the line is a horizontal rule such as `====` or `-----`.
///
/// At least three characters of a single punctuation kind, so a YAML `---` document marker also
/// reads as a rule. That is the accepted cost: a three-dash separator carries no more meaning
/// than a three-equals one, and only lines made *entirely* of the character qualify — a `+++ b/f`
/// diff header contains letters and stays [`Importance::Info`].
fn is_separator(text: &str) -> bool {
    let mut kind: Option<char> = None;
    let mut count = 0usize;
    for ch in text.chars().filter(|ch| !ch.is_whitespace()) {
        if !"=-_*#~".contains(ch) {
            return false;
        }
        if kind.is_some_and(|seen| seen != ch) {
            return false;
        }
        kind = Some(ch);
        count += 1;
    }
    count >= 3
}

/// Classifies one line by its content alone, ignoring its position in the output.
///
/// Returns `None` when nothing on the line reports a problem, which leaves the rendering rules in
/// [`classify`] in charge of deciding between info and noise.
fn read_line(text: &str) -> Option<Importance> {
    // ASCII case folding only: it preserves byte offsets (the negation scan needs them) and CJK
    // text has no case, so the markers below are matched against the original string.
    let lower = text.to_ascii_lowercase();
    let mut warned = false;
    for (start, token) in ascii_words(&lower) {
        let end = start + token.len();
        if is_error_word(token) && !negated(&lower, start, end) {
            // An error outranks a warning on the same line: `error: unused variable` must not be
            // reported as a warning just because it also matches a warning word.
            return Some(Importance::Error);
        }
        if is_warning_word(token) && !negated(&lower, start, end) {
            warned = true;
        }
    }
    if has_chinese(text, ERROR_MARKERS) {
        return Some(Importance::Error);
    }
    if warned || has_chinese(text, WARNING_MARKERS) {
        return Some(Importance::Warning);
    }
    None
}

/// ASCII words of `text`, with the byte offset each one starts at.
///
/// A word is a run of ASCII alphanumerics and `_`, so an identifier keeps its parts glued
/// together. That is the whole point: `error_guard.rs` and `test_returns_error_on_empty_input`
/// must not match the word `error`, and the only way to know they do not is to see `_` as part of
/// the word rather than as a separator.
fn ascii_words(text: &str) -> Vec<(usize, &str)> {
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        let word_char = ch.is_ascii_alphanumeric() || ch == '_';
        match (word_char, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                words.push((from, &text[from..index]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        words.push((from, &text[from..]));
    }
    words
}

/// Words that report a failure, as whole words only.
///
/// Inflections are listed rather than derived from a `fail` prefix, because a prefix also matches
/// `failsafe` and `failure_mode_is_handled` in a log line that is reporting success. `cannot` is
/// included because it is how a tool says it refused to do something without ever using the word
/// `error` — `cannot find -lssl` is a build failure with no diagnostic attached.
const ERROR_WORDS: &[&str] = &[
    "error", "errors", "errored", "fail", "fails", "failed", "failing", "failure", "failures",
    "cannot",
];

/// Words that report a warning, as whole words only.
const WARNING_WORDS: &[&str] = &["warning", "warnings", "warn", "warns", "warned"];

/// Chinese markers for a failure, matched as substrings because Chinese has no word boundaries.
///
/// `无法` ("unable to") is included: a Chinese log reports a refused operation that way — `无法打开
/// 文件` — with no equivalent of the word `error`. `未能` covers a build step that did not complete.
const ERROR_MARKERS: &[&str] = &["错误", "失败", "无法", "出错", "未能"];

/// Chinese markers for a warning.
const WARNING_MARKERS: &[&str] = &["警告"];

/// Whether any of `markers` occurs in `text` without a zero count in front of it.
///
/// Negation is checked for CJK markers too, and it matters for the same reason as in English: a
/// summary line reading `错误: 0` reports that there were none.
fn has_chinese(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| {
        text.match_indices(marker)
            .any(|(start, _)| !negated(text, start, start + marker.len()))
    })
}

/// Whether the occurrence of a marker at `start..end` is negated by its surroundings.
fn negated(text: &str, start: usize, end: usize) -> bool {
    negated_before(text, start) || negated_after(text, end)
}

/// Whether the text before a marker says the count it refers to is zero.
///
/// This is the rule that makes `0 errors`, `no errors`, `without errors` and `没有错误` successes.
/// It scans back over the separator punctuation and the Chinese counters that sit between a
/// number and its noun (`0 个 错误`), then asks one question: is the number all zeros, or is the
/// last word a negation?
fn negated_before(text: &str, start: usize) -> bool {
    let before = text[..start].trim_end_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '个' | '条' | '处' | '项' | '次')
    });
    if NEGATION_SUFFIXES
        .iter()
        .any(|suffix| before.ends_with(suffix))
    {
        return true;
    }
    let digits: String = before
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    if !digits.is_empty() {
        // `10 errors` is a failure; `0 errors` is a summary of success. Only all-zero counts
        // negate, so the check is on the digits and not on their presence.
        return digits.chars().all(|digit| digit == '0');
    }
    matches!(
        before.rsplit(|ch: char| !ch.is_ascii_alphanumeric()).next(),
        Some("no" | "zero" | "none" | "without")
    )
}

/// Whether the text after a marker is a zero count, as in `errors: 0` or `failed: 0`.
///
/// The digit run must end at a boundary. `error: 0x80070005` is a real Windows failure code whose
/// leading zero is not a count, and treating it as one would silently drop a line the caller
/// needs; requiring the run to be followed by a non-word character distinguishes the two.
fn negated_after(text: &str, end: usize) -> bool {
    let after = text[end..].trim_start();
    let after = after.trim_start_matches([':', '=', ' ', '\t']);
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !digits.chars().all(|digit| digit == '0') {
        return false;
    }
    after[digits.len()..]
        .chars()
        .next()
        .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
}

/// Counters and negations that make a following Chinese marker a statement of absence.
const NEGATION_SUFFIXES: &[&str] = &["没有", "无", "零", "未发现", "未出现", "没有出现"];

/// Whether `word` reports a failure.
fn is_error_word(word: &str) -> bool {
    ERROR_WORDS.contains(&word)
}

/// Whether `word` reports a warning.
fn is_warning_word(word: &str) -> bool {
    WARNING_WORDS.contains(&word)
}

/// The marker standing in for text left out of a digest.
///
/// The counts are in lines because that is what a reader of a log thinks in, and the total is
/// included so the size of what is missing can be judged without the caller having to keep it.
fn omission(missing: usize, total: usize) -> String {
    format!("[... {missing} of {total} lines omitted ...]")
}
