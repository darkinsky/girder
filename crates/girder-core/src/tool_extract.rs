//! Structure recovered from the text a tool returned.
//!
//! # The results are text, not data
//!
//! The filesystem and search tools finish through `ToolResult::text`, which stores a `String` and
//! leaves the canonical `value` field `None`. `glob`, `grep` and `read` are therefore only
//! *conventional* formats: a client that wants to render `grep` matches as a clickable list has
//! nothing to read but the string, and would otherwise carry its own copy of each convention. Those
//! copies drift — one client accepts `path:line:text`, another grows a rule for a path containing a
//! colon — and each drift produces a rendering that is confidently wrong. This module is the single
//! place where the three conventions are read, so a client can ask for a list instead of matching
//! on punctuation.
//!
//! Extraction is deliberately one-way: it does not redefine the wire format and it does not change
//! what the tools emit. A structured result field is the real fix; until then this is a parser over
//! what exists, and it is honest about where the format is ambiguous.
//!
//! # The three shapes, as emitted today
//!
//! - `grep` — one match per line as `path:line:content`, one-based line numbers, no heading. A
//!   result with no matches is the single line ``no matches for `<pattern>` ``. Truncation is a
//!   blank line followed by `(stopped after N matches)`.
//! - `glob` — one display path per line, workspace-relative when the file is inside the workspace
//!   and absolute when it is not. No matches is ``no files match `<pattern>` ``; truncation is a
//!   blank line followed by `(stopped after N files; narrow the pattern to see the rest)`.
//! - `read` — an optional language hint on the first line (only for a path whose extension is
//!   known), then one `line\tcontent` per line, with the line number the caller asked for. A file
//!   over the size cap is refused rather than truncated, so the shape itself never says "partial".
//!
//! # Paths may contain colons
//!
//! `grep` separates the path from the line number with a colon, and a path may contain a colon:
//! `C:\repo\src\main.rs`, or a file literally named `a:b.rs`. A parser therefore has to decide
//! where the path ends, and no decision is always right — `notes:2:backup.txt:9:hello` is either
//! the path `notes` at line 2 or the path `notes:2:backup.txt` at line 9, and the string alone does
//! not say which.
//!
//! The rule here is to split at the **leftmost** `:<digits>:` that can be a boundary. Two facts
//! make that the better guess: a drive letter is followed by `\` rather than a digit, so `C:\...`
//! passes over the first colon, and a colon followed by digits inside a *path* is rare while a
//! `:number:` inside a *matched line* of source code is ordinary. The cost is paid on the rare
//! side: a path whose own name contains `:<digits>:` is split at the wrong colon. Keeping the
//! reported path whole in that case would require a filesystem lookup or a richer transport, and
//! inventing one here would make the common case unreliable to fix the uncommon one. A client that
//! cannot afford the ambiguity should resolve each reported path and fall back to plain text when
//! it does not exist.
//!
//! The `glob` convention has no such ambiguity, because a line *is* a path; the trade there is the
//! opposite one, documented on [`extract`].
//!
//! # Truncation is reported, never smoothed over
//!
//! A capped list that looks complete is worse than no list: a caller draws a conclusion from a
//! file it was never shown. [`Extracted::Paths`] therefore carries `truncated`, taken from the
//! marker the tools append — and from the marker the output-budget layer appends when it shortens
//! a result after the tool produced it, because that cut is just as invisible to a reader.
//!
//! # An empty result is a result
//!
//! Empty output, and the explicit no-match line, are **definite** answers: [`Extracted::Paths`]
//! with no paths. [`Extracted::Unknown`] is reserved for text that does not match a convention at
//! all, because that is the only case where a caller has to fall back to showing the raw text. A
//! wrong `Paths` is worse than an `Unknown`: the first renders a plausible list of files that were
//! never matched, the second only declines to improve on the text.
//!
//! This module has no dependencies on the rest of the crate, so it compiles both as a crate module
//! and through `#[path]` in an integration test that has no crate root of its own.

/// The prefix of the marker a search tool appends when it stops before the end of its result.
const STOPPED_PREFIX: &str = "(stopped after";

/// The prefix of the marker the output-budget layer appends to a shortened block.
const VIEW_TRUNCATED_PREFIX: &str = "[truncated:";

/// The prefix of the notice the output-budget layer appends when it dropped blocks.
const VIEW_NOTICE_PREFIX: &str = "[view:";

/// The whole text `grep` returns when nothing matched.
const GREP_NO_MATCH_PREFIX: &str = "no matches for";

/// The whole text `glob` returns when nothing matched.
const GLOB_NO_MATCH_PREFIX: &str = "no files match";

/// A structured reading of a tool result.
///
/// Derived equality is part of the contract: a test asserts that a shape was recognized, and a
/// caller comparing two extractions of the same text should not have to unroll the fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Extracted {
    /// A set of file paths, from `glob` or `grep`.
    Paths {
        /// The paths, deduplicated, in the order they first appeared.
        paths: Vec<String>,
        /// Whether the result was capped, which means the list may be incomplete.
        truncated: bool,
    },
    /// The content of one file, from `read`.
    Content {
        /// The line number of the first content line, when the output carried numbers.
        first_line: Option<u32>,
        /// The content lines, with the tool's line numbers stripped.
        lines: Vec<String>,
    },
    /// Text this module does not recognize; the caller should show it as it arrived.
    Unknown,
}

/// Attempts to recover structure from a tool's name and output.
///
/// The tool name is matched exactly (`grep`, `glob`, `read`) after trimming; a tool this module
/// does not know returns [`Extracted::Unknown`] without inspecting the output, because guessing
/// that an unrelated tool's text is a path list is how a client ends up rendering a fake table.
///
/// # `grep` and path ambiguity
///
/// See the module documentation: matches are split at the leftmost `:<digits>:`. Every non-blank
/// line that is not a marker must split, or the result is [`Extracted::Unknown`] — a single
/// unreadable line means the text is not the format this module claims to understand, and
/// returning the lines that did split would hide the disagreement.
///
/// # `glob` and the missing guard
///
/// A path may contain any character, including spaces, so nothing in a `glob` line distinguishes a
/// path from prose. Only the two shapes the tool produces itself — the no-match line and the
/// truncation marker — are excluded; anything else is taken as a path. The format is one path per
/// line and carries no further structure to check against, so the honest description is "a list
/// unless the tool said otherwise", and a caller that needs certainty should resolve the paths.
///
/// # `read` and partial content
///
/// [`Extracted::Content`] has no field that says "this is only part of the file", and `read`
/// refuses an oversized file rather than shortening it — so a numbered block is complete by
/// construction. When the output carries a shortening marker anyway, that construction no longer
/// holds and the result is [`Extracted::Unknown`] rather than `Content`: reporting the surviving
/// lines as the file would assert a completeness the text does not have, and the enum cannot say
/// otherwise.
#[must_use]
pub fn extract(tool: &str, output: &str) -> Extracted {
    match tool.trim() {
        "grep" => match grep_paths_checked(output) {
            Some(paths) => Extracted::Paths {
                paths: deduplicate(paths),
                truncated: is_truncated(output),
            },
            None => Extracted::Unknown,
        },
        "glob" => Extracted::Paths {
            paths: deduplicate(glob_paths(output)),
            truncated: is_truncated(output),
        },
        "read" => content(output),
        _ => Extracted::Unknown,
    }
}

/// Extracts the paths from `grep` output, whose format is `path:line:content`.
///
/// This is the lenient half of [`extract`]: a line that does not split is skipped rather than
/// turning the whole result into [`Extracted::Unknown`], because a caller asking only for paths has
/// no use for the distinction. One path is returned per matching *line*, so a file matched four
/// times appears four times; use [`extract`] when a set is wanted.
///
/// # Ambiguity
///
/// Splitting is at the leftmost `:<digits>:` boundary; see the module documentation for why, and
/// for the case — a path containing `:<digits>:` — where the reported path is truncated at the
/// wrong colon.
#[must_use]
pub fn grep_paths(output: &str) -> Vec<String> {
    grep_paths_checked(output).unwrap_or_default()
}

/// Extracts the paths from `glob` output, which is one path per line.
///
/// The no-match line and the truncation markers are not paths, and blank lines separate the marker
/// from the list, so all three are skipped. A trailing carriage return is removed so that text
/// transported with CRLF endings does not leave `\r` on the last component of a path; other
/// whitespace is preserved, because a path may legitimately contain it.
#[must_use]
pub fn glob_paths(output: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in output.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.is_empty() || is_truncation_marker(trimmed) {
            continue;
        }
        if trimmed.starts_with(GLOB_NO_MATCH_PREFIX) {
            continue;
        }
        paths.push(line.to_owned());
    }
    paths
}

/// Reads `grep` output, returning `None` when any line is not a match line.
///
/// The strictness lives here so that [`extract`] and [`grep_paths`] can disagree about it in one
/// place instead of two.
fn grep_paths_checked(output: &str) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    for line in output.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.is_empty() || is_truncation_marker(trimmed) {
            continue;
        }
        if trimmed.starts_with(GREP_NO_MATCH_PREFIX) {
            continue;
        }
        match split_match(line) {
            Some((path, _, _)) => paths.push(path.to_owned()),
            None => return None,
        }
    }
    Some(paths)
}

/// Reads `read` output, returning `None` when it is not the numbered shape.
fn content(output: &str) -> Extracted {
    let empty = Extracted::Content {
        first_line: None,
        lines: Vec::new(),
    };
    // Whitespace-only output is the same definite answer as no output at all: nothing was read,
    // and there is nothing here to misread.
    if output.trim().is_empty() {
        return empty;
    }

    let lines: Vec<&str> = output
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    // A shortening marker means the numbered block is not the whole file, and `Content` has no way
    // to record that; see `extract`'s documentation for why this is `Unknown` instead.
    if lines.iter().any(|line| is_truncation_marker(line)) {
        return Extracted::Unknown;
    }

    // Every content line carries `number\t`, so a first line without a tab can only be the
    // language hint the reader prepends to a known extension.
    let mut start = 0;
    if split_numbered(lines.first().copied().unwrap_or_default()).is_none() {
        start = 1;
    }

    let mut first_line = None;
    let mut content_lines = Vec::new();
    for line in &lines[start..] {
        if line.trim().is_empty() {
            continue;
        }
        let Some((number, text)) = split_numbered(line) else {
            return Extracted::Unknown;
        };
        if first_line.is_none() {
            first_line = Some(number);
        }
        content_lines.push(text.to_owned());
    }
    if start == 1 && content_lines.is_empty() {
        // One unnumbered line and nothing after it is either a language hint on an empty file or
        // text from somewhere else entirely, and this module has no language table to tell the two
        // apart. Declining is the honest answer: `Content` with no lines would claim a file was
        // read and empty.
        return Extracted::Unknown;
    }
    Extracted::Content {
        first_line,
        lines: content_lines,
    }
}

/// Splits one `grep` match line into path, one-based line number and content.
///
/// Candidates are scanned left to right and the first one that is a boundary wins: a colon followed
/// by at least one digit and then by a colon or the end of the line. The end-of-line case is a
/// matched blank line, which the tool prints as `path:3:` with nothing after it. A path may be
/// empty in the format, but an empty path is not a file, so a leading colon is not accepted.
fn split_match(line: &str) -> Option<(&str, u32, &str)> {
    let bytes = line.as_bytes();
    let mut search_from = 0;
    while let Some(found) = line[search_from..].find(':') {
        let colon = search_from + found;
        let digits_start = colon + 1;
        let mut end = digits_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if colon > 0
            && end > digits_start
            && (end == bytes.len() || bytes[end] == b':')
            && let Ok(number) = line[digits_start..end].parse::<u32>()
        {
            let content = if end == bytes.len() {
                ""
            } else {
                &line[end + 1..]
            };
            return Some((&line[..colon], number, content));
        }
        search_from = colon + 1;
    }
    None
}

/// Splits one `read` content line into its line number and text.
///
/// The tab, not a run of spaces, is the separator: a content line may begin with any indentation,
/// and the reader writes exactly one tab after the number.
fn split_numbered(line: &str) -> Option<(u32, &str)> {
    let (number, text) = line.split_once('\t')?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((number.parse::<u32>().ok()?, text))
}

/// Whether a line is a marker rather than content.
///
/// Three shapes are recognized because three layers can cap a result: the search tools say
/// `(stopped after N ...)`, and the output-budget layer appends `[truncated: ...]` or a `[view:
/// ...]` notice. All three are model-facing prose on their own line, never part of a path or a file
/// line, so a caller that extracts structure must drop them.
fn is_truncation_marker(line: &str) -> bool {
    let line = line.trim();
    line.starts_with(STOPPED_PREFIX)
        || line.starts_with(VIEW_TRUNCATED_PREFIX)
        || line.starts_with(VIEW_NOTICE_PREFIX)
}

/// Whether any marker line says the result was capped or shortened.
fn is_truncated(output: &str) -> bool {
    output.lines().any(is_truncation_marker)
}

/// Drops repeated paths, keeping the order in which each first appeared.
///
/// A `grep` result names the same file once per matching line, and a list for a client to render is
/// a list of files. The order is the tool's, not sorted: it is the order of the matches, which is
/// the order a reader is already scanning.
fn deduplicate(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(paths.len());
    for path in paths {
        if seen.insert(path.clone()) {
            unique.push(path);
        }
    }
    unique
}
