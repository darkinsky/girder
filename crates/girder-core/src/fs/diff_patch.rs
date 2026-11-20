//! Unified-diff parsing and application, with context validation.
//!
//! The existing [`patch`](crate::fs::patch) module parses the `*** Begin Patch` dialect, where every line
//! is relative to an anchor and a hunk may be relocated by searching for its context. This module
//! implements the *unified diff* dialect instead: `@@ -old_start,old_lines +new_start,new_lines @@`
//! with ` `, `-`, and `+` prefixed body lines. The two differ in the property that matters for
//! safety, and this module chooses the stricter one:
//!
//! # Why the line number decides, and the context only vetoes
//!
//! A patch carries both a position and a copy of the surrounding text. Trusting the position alone
//! corrupts a file that gained or lost a line above the hunk; trusting the context alone can apply a
//! hunk to a different function that happens to contain the same lines. So the position is where the
//! hunk *must* land, and every context and removed line is compared byte for byte there. A file that
//! has drifted fails with [`PatchError::ContextMismatch`] instead of being edited at the wrong
//! offset, and a hunk whose position is past the end fails with [`PatchError::OutOfRange`] rather
//! than being appended.
//!
//! # Why the declared counts are enforced
//!
//! `old_lines` and `new_lines` are the patch's own checksum over its body: they say how many lines
//! the body must contain on each side. A body that does not match its header was truncated in
//! transit, or had lines silently dropped by a tool that rewrote it. Applying such a hunk would
//! delete or duplicate whatever the missing lines covered, so a mismatch is a hard error
//! ([`PatchError::CountMismatch`]) rather than something to work around.
//!
//! # Why unsupported input is rejected instead of approximated
//!
//! A parser that guesses produces a *different* patch than the one it was handed, and the caller
//! cannot tell. Combined diffs (`@@@`), binary patches, the `\ No newline at end of file` marker,
//! unprefixed body lines, and multi-file diffs are therefore named errors. Callers get a patch that
//! means exactly what it said, or an error that says what was not understood.
//!
//! # Whitespace and line endings
//!
//! Lines are compared exactly, `\r` included: a patch made against a CRLF file carries `\r` at the
//! end of each body line, so it matches, and the rebuilt text keeps each line's `\r`. A LF patch
//! applied to a CRLF file fails on the first context line instead of silently mixing endings.
//! A text's trailing newline is preserved from the *input*: a patch never adds or removes one.

use std::fmt;

/// One hunk of a unified diff.
///
/// Constructing one by hand is allowed — [`apply`] re-checks the declared counts against the body
/// before it touches the text — but [`parse`] is the usual source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// One-based line in the original text where the hunk starts.
    ///
    /// Zero is legal only together with `old_lines == 0`, where it means "insert before line 1".
    pub old_start: u32,
    /// How many lines the body consumes from the original text.
    pub old_lines: u32,
    /// One-based line in the result where the hunk starts.
    pub new_start: u32,
    /// How many lines the body produces in the result.
    pub new_lines: u32,
    /// Body lines, each beginning with `' '`, `'-'`, or `'+'`.
    ///
    /// The prefix is kept rather than translated into a kind enum because it is what the caller
    /// reads when reporting a failure: the line is quoted the way the patch wrote it.
    pub lines: Vec<String>,
}

/// One unified diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    /// Its hunks, in the order they appear in the patch.
    pub hunks: Vec<Hunk>,
}

/// Why a patch was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchError {
    /// A header, or a line that had to be one, was malformed.
    InvalidHeader {
        /// One-based line in the patch text.
        ///
        /// [`apply`] has no patch text, so there it is the one-based ordinal of the hunk in
        /// [`Patch::hunks`].
        line: usize,
        /// What was expected, and usually what was found instead.
        detail: String,
    },
    /// A hunk's body does not have the number of lines its header declared.
    ///
    /// `expected` is the count from the header; `found` is the count in the body. `found` is larger
    /// than `expected` when the body holds lines beyond the declaration (the parser stops counting
    /// at the first line it cannot attribute to the hunk).
    CountMismatch {
        /// Declared by the header.
        expected: u32,
        /// Counted in the body.
        found: u32,
    },
    /// A context or removed line did not match the text at the position the hunk names.
    ///
    /// This is the error that makes a patch safe: the text changed since the patch was made, so
    /// applying it would edit lines the patch never saw.
    ContextMismatch {
        /// One-based line in the original text.
        line: u32,
        /// The line the patch expects there, without its prefix.
        expected: String,
        /// The line actually there.
        found: String,
    },
    /// A hunk starts past the end of the text.
    OutOfRange {
        /// The one-based line the hunk needs, past the end.
        line: u32,
        /// How many lines the text has.
        total: u32,
    },
}

impl fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHeader { line, detail } => {
                write!(formatter, "line {line}: {detail}")
            }
            Self::CountMismatch { expected, found } => write!(
                formatter,
                "the hunk declares {expected} line(s) but its body has {found}"
            ),
            Self::ContextMismatch {
                line,
                expected,
                found,
            } => write!(
                formatter,
                "line {line}: the patch expects `{expected}` but the text has `{found}`"
            ),
            Self::OutOfRange { line, total } => write!(
                formatter,
                "line {line} is past the end of the {total}-line text"
            ),
        }
    }
}

impl std::error::Error for PatchError {}

/// A hunk still being filled in, with the counts seen so far.
///
/// The counts live beside the hunk rather than being recomputed at the end so that the parser can
/// tell, line by line, whether the body is already complete — that is what lets a following `@@` or
/// file header be recognised as a boundary instead of being swallowed as body text.
struct OpenHunk {
    /// The hunk under construction.
    hunk: Hunk,
    /// Old-side lines seen.
    old_seen: u32,
    /// New-side lines seen.
    new_seen: u32,
}

impl OpenHunk {
    /// Whether the body already holds exactly the declared line counts.
    fn satisfied(&self) -> bool {
        self.old_seen == self.hunk.old_lines && self.new_seen == self.hunk.new_lines
    }

    /// Whether `line` cannot belong to this hunk.
    ///
    /// A `@@` always ends the hunk: the next header is unambiguous even when this body is short, and
    /// ending here is what turns a truncated body into a [`PatchError::CountMismatch`] instead of
    /// letting it absorb the next hunk's header as a body line.
    ///
    /// The other boundaries are only considered once the counts are satisfied. Before that, `---`,
    /// `+++`, `diff --git`, and blank lines are legitimate body text — a removed line may itself be
    /// `-- note`, which reads as `--- note` after its prefix, and a context line for an empty line
    /// may legitimately be blank when the writer omitted the single space. Treating those as headers
    /// early would reject valid patches; treating them as body text early is harmless because the
    /// counts still have to match at the end.
    fn closes(&self, line: &str) -> bool {
        line.starts_with("@@") || (self.satisfied() && (line.is_empty() || is_header_line(line)))
    }

    /// Records one blank line as context for an empty line.
    ///
    /// Many editors strip trailing whitespace, which turns the body line for an empty context line
    /// (`" "`, a single space) into an empty line. Accepting it while the counts are still open is
    /// safe: the counts must still match exactly, so a stray blank line cannot pass unnoticed — it
    /// shifts every following count until the body no longer agrees with its header.
    fn push_blank(&mut self) {
        self.old_seen = self.old_seen.saturating_add(1);
        self.new_seen = self.new_seen.saturating_add(1);
        self.hunk.lines.push(String::new());
    }

    /// Records one body line.
    fn push(&mut self, line: &str, number: usize) -> Result<(), PatchError> {
        let (prefix, _) = split_prefix(line).ok_or_else(|| PatchError::InvalidHeader {
            line: number,
            detail: "a hunk body line must begin with ' ', '-', or '+'".to_owned(),
        })?;
        match prefix {
            ' ' => {
                self.old_seen = self.old_seen.saturating_add(1);
                self.new_seen = self.new_seen.saturating_add(1);
            }
            '-' => self.old_seen = self.old_seen.saturating_add(1),
            '+' => self.new_seen = self.new_seen.saturating_add(1),
            _ => {
                return Err(PatchError::InvalidHeader {
                    line: number,
                    detail: describe_unprefixed(line),
                });
            }
        }
        self.hunk.lines.push(line.to_owned());
        Ok(())
    }

    /// Checks the body against the header and yields the hunk.
    fn finish(self) -> Result<Hunk, PatchError> {
        if self.old_seen != self.hunk.old_lines {
            return Err(PatchError::CountMismatch {
                expected: self.hunk.old_lines,
                found: self.old_seen,
            });
        }
        if self.new_seen != self.hunk.new_lines {
            return Err(PatchError::CountMismatch {
                expected: self.hunk.new_lines,
                found: self.new_seen,
            });
        }
        Ok(self.hunk)
    }
}

/// Splits a text into lines, keeping every byte of each line except the separator.
///
/// `str::lines` drops a trailing `\r`, which would make a CRLF file look like an LF file and let a
/// mismatched patch apply. Splitting on `\n` alone keeps `\r` in the line, so the comparison in
/// [`apply`] sees exactly what the file holds.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        // The final separator does not open an extra line.
        lines.pop();
    }
    lines
}

/// Converts a zero-based index into a one-based line number, saturating at the type's limit.
fn line_number(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1)
}

/// Splits the leading prefix character from a line's content.
fn split_prefix(line: &str) -> Option<(char, &str)> {
    let mut characters = line.chars();
    let prefix = characters.next()?;
    Some((prefix, characters.as_str()))
}

/// The detail for a body line that carries no recognised prefix.
fn describe_unprefixed(line: &str) -> String {
    if line.starts_with('\\') {
        // This marker changes the meaning of the line before it (no trailing newline), which this
        // module deliberately does not model; accepting it silently would lose that meaning.
        return "the `\\ No newline at end of file` marker is not supported; a text's trailing \
                newline is preserved from the input"
            .to_owned();
    }
    let shown = if line.chars().count() > 40 {
        let head: String = line.chars().take(40).collect();
        format!("{head}...")
    } else {
        line.to_owned()
    };
    format!("`{shown}` is not a hunk body line: it must begin with ' ', '-', or '+'")
}

/// Whether a line is one of the file headers a unified diff may carry.
///
/// These lines name paths, and [`Patch`] has no paths, so they are informational. They are still
/// recognised: skipping them keeps a real `git diff` parseable, and recognising them is what lets
/// [`apply`]'s caller pass such a diff through unchanged.
fn is_file_header(line: &str) -> bool {
    line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("diff --git ")
        || line == "---"
        || line == "+++"
}

/// Whether a line is git metadata that carries no hunk content.
///
/// Only lines whose whole meaning is a path, a mode, or a similarity score are listed: they cannot
/// change how a hunk applies. Anything with content semantics (`GIT binary patch`, `Binary files`,
/// `literal`, `delta`) is deliberately *not* here and is rejected by `parse` with its own message.
fn is_meta_line(line: &str) -> bool {
    const PREFIXES: [&str; 9] = [
        "index ",
        "new file mode ",
        "deleted file mode ",
        "old mode ",
        "new mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
    ];
    PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

/// Whether a line is a header of any kind, and therefore cannot be hunk body text.
fn is_header_line(line: &str) -> bool {
    is_file_header(line) || is_meta_line(line) || line.starts_with('\\')
}

/// Whether a line announces a binary patch, which this module cannot apply as text.
fn is_binary_marker(line: &str) -> bool {
    line.starts_with("GIT binary patch")
        || line.starts_with("Binary files ")
        || line.starts_with("literal ")
        || line.starts_with("delta ")
}

/// Parses a unified diff.
///
/// The accepted shape is one file: optional `---`/`+++`/`diff --git` headers, then one or more
/// `@@ -old_start,old_lines +new_start,new_lines @@` hunks. A count may be omitted, in which case it
/// is 1. Text after the closing `@@` (the "section heading") is ignored, as git's is.
///
/// # Errors
///
/// Returns [`PatchError::InvalidHeader`] for anything that is not that shape — including combined
/// diffs, binary patches, unprefixed body lines, and a second file's headers — and
/// [`PatchError::CountMismatch`] when a hunk's body does not match its declared counts.
///
/// An empty or header-only input parses to a [`Patch`] with no hunks; applying it returns the text
/// unchanged, which is the one case where a no-op beats an error.
pub fn parse(text: &str) -> Result<Patch, PatchError> {
    let lines = split_lines(text);
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut open: Option<OpenHunk> = None;

    for (index, line) in lines.iter().enumerate() {
        let number = index.saturating_add(1);
        let line = *line;

        if is_binary_marker(line) {
            return Err(PatchError::InvalidHeader {
                line: number,
                detail: "binary patches are not supported: this module applies text".to_owned(),
            });
        }

        // A line the open hunk cannot claim closes it, and the declared counts are checked here
        // rather than at the end of the file so the error points at the boundary that broke it.
        if open.as_ref().is_some_and(|current| current.closes(line)) {
            if let Some(finished) = open.take() {
                hunks.push(finished.finish()?);
            }
        }

        if line.is_empty() {
            if let Some(current) = open.as_mut() {
                current.push_blank();
            }
            continue;
        }

        if line.starts_with("@@") {
            open = Some(parse_hunk_header(line, number)?);
            continue;
        }

        if let Some(current) = open.as_mut() {
            current.push(line, number)?;
            continue;
        }

        if line.starts_with('\\') {
            return Err(PatchError::InvalidHeader {
                line: number,
                detail: describe_unprefixed(line),
            });
        }

        if is_file_header(line) || is_meta_line(line) {
            if !hunks.is_empty() {
                // Applying the second file's hunks at the first file's positions would edit the
                // wrong lines; there is no path here to tell them apart, so refuse.
                return Err(PatchError::InvalidHeader {
                    line: number,
                    detail: "the diff changes more than one file; parse each file separately"
                        .to_owned(),
                });
            }
            continue;
        }

        return Err(PatchError::InvalidHeader {
            line: number,
            detail: "expected a hunk header (`@@ -old,count +new,count @@`) or a file header"
                .to_owned(),
        });
    }

    if let Some(open) = open {
        hunks.push(open.finish()?);
    }

    Ok(Patch { hunks })
}

/// Parses one `@@` header line into an empty hunk.
fn parse_hunk_header(line: &str, number: usize) -> Result<OpenHunk, PatchError> {
    let rest = line
        .strip_prefix("@@")
        .ok_or_else(|| PatchError::InvalidHeader {
            line: number,
            detail: "expected a hunk header".to_owned(),
        })?;
    if rest.starts_with('@') {
        return Err(PatchError::InvalidHeader {
            line: number,
            detail: "combined diffs (`@@@`) are not supported".to_owned(),
        });
    }
    let close = rest.find("@@").ok_or_else(|| PatchError::InvalidHeader {
        line: number,
        detail: "the hunk header must end with `@@`".to_owned(),
    })?;
    let mut ranges = rest[..close].split_whitespace();
    let old = ranges.next().ok_or_else(|| PatchError::InvalidHeader {
        line: number,
        detail: "the hunk header is missing its `-old,count` range".to_owned(),
    })?;
    let new = ranges.next().ok_or_else(|| PatchError::InvalidHeader {
        line: number,
        detail: "the hunk header is missing its `+new,count` range".to_owned(),
    })?;
    if ranges.next().is_some() {
        return Err(PatchError::InvalidHeader {
            line: number,
            detail: format!("expected `@@ -old,count +new,count @@`, found extra text `{line}`"),
        });
    }
    let (old_start, old_lines) = parse_range(old, '-', number)?;
    let (new_start, new_lines) = parse_range(new, '+', number)?;
    Ok(OpenHunk {
        hunk: Hunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            lines: Vec::new(),
        },
        old_seen: 0,
        new_seen: 0,
    })
}

/// Parses one `-start,count` or `+start,count` range.
fn parse_range(token: &str, sign: char, number: usize) -> Result<(u32, u32), PatchError> {
    let body = token
        .strip_prefix(sign)
        .ok_or_else(|| PatchError::InvalidHeader {
            line: number,
            detail: format!("the range `{token}` must begin with `{sign}`"),
        })?;
    let (start, count) = match body.split_once(',') {
        Some((start, count)) => (start, Some(count)),
        None => (body, None),
    };
    let start = parse_count(start, number)?;
    // Unified diffs may omit a count of 1; there is no spelling for any other omitted count.
    let count = match count {
        Some(count) => parse_count(count, number)?,
        None => 1,
    };
    if start == 0 && count > 0 {
        // `-0,0` is the empty file; `-0,3` claims lines that cannot exist.
        return Err(PatchError::InvalidHeader {
            line: number,
            detail: format!("the range `{token}` cannot start at line 0 and consume {count} lines"),
        });
    }
    Ok((start, count))
}

/// Parses a non-negative decimal line number or count.
fn parse_count(text: &str, number: usize) -> Result<u32, PatchError> {
    text.parse::<u32>().map_err(|_| PatchError::InvalidHeader {
        line: number,
        detail: format!("`{text}` is not a line number or count"),
    })
}

/// Applies a patch to a text.
///
/// Each hunk lands exactly where its header says, and every context and removed line is compared
/// with the text there. A mismatch, a hunk past the end, an inconsistent body, or hunks that overlap
/// are errors: no partially applied text is ever returned.
///
/// # Errors
///
/// [`PatchError::ContextMismatch`] when the text has drifted, [`PatchError::OutOfRange`] when a hunk
/// starts past the end, [`PatchError::CountMismatch`] when a hunk's body and header disagree, and
/// [`PatchError::InvalidHeader`] when a body line has no prefix or the hunks are out of order.
pub fn apply(text: &str, patch: &Patch) -> Result<String, PatchError> {
    let source = split_lines(text);
    let total = u32::try_from(source.len()).unwrap_or(u32::MAX);
    let mut out: Vec<String> = Vec::with_capacity(source.len());
    let mut cursor = 0usize;

    for (ordinal, hunk) in patch.hunks.iter().enumerate() {
        let number = ordinal.saturating_add(1);
        // A hunk built by hand may disagree with itself; checking here means the loop below can
        // assume the prefixes are known and the position is in range.
        check_hunk(hunk, number)?;

        let start = hunk_start(hunk, source.len(), number)?;
        if start < cursor {
            return Err(PatchError::InvalidHeader {
                line: number,
                detail: "hunks must be ordered and must not overlap".to_owned(),
            });
        }
        out.extend(source[cursor..start].iter().map(|line| (*line).to_owned()));

        let mut position = start;
        for line in &hunk.lines {
            let (prefix, body) = split_prefix(line).ok_or_else(|| PatchError::InvalidHeader {
                line: number,
                detail: "a hunk body line must begin with ' ', '-', or '+'".to_owned(),
            })?;
            match prefix {
                '+' => out.push(body.to_owned()),
                ' ' | '-' => {
                    let found = source.get(position).ok_or(PatchError::OutOfRange {
                        line: line_number(position),
                        total,
                    })?;
                    if *found != body {
                        return Err(PatchError::ContextMismatch {
                            line: line_number(position),
                            expected: body.to_owned(),
                            found: (*found).to_owned(),
                        });
                    }
                    if prefix == ' ' {
                        out.push(body.to_owned());
                    }
                    position = position.saturating_add(1);
                }
                _ => {
                    return Err(PatchError::InvalidHeader {
                        line: number,
                        detail: describe_unprefixed(line),
                    });
                }
            }
        }
        cursor = position;
    }

    out.extend(source[cursor..].iter().map(|line| (*line).to_owned()));

    let mut result = out.join("\n");
    // A trailing newline belongs to the text, not to the patch, so it is carried over rather than
    // recomputed. An empty text counts as newline-terminated: every line a patch adds to it is a
    // whole line, so dropping the final separator would silently join the last two lines.
    let terminated = text.ends_with('\n') || text.is_empty();
    if terminated && !out.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

/// Checks a hunk's body against its own header before it is applied.
fn check_hunk(hunk: &Hunk, ordinal: usize) -> Result<(), PatchError> {
    let mut old_seen = 0u32;
    let mut new_seen = 0u32;
    for line in &hunk.lines {
        match split_prefix(line).map(|(prefix, _)| prefix) {
            Some(' ') => {
                old_seen = old_seen.saturating_add(1);
                new_seen = new_seen.saturating_add(1);
            }
            Some('-') => old_seen = old_seen.saturating_add(1),
            Some('+') => new_seen = new_seen.saturating_add(1),
            _ => {
                return Err(PatchError::InvalidHeader {
                    line: ordinal,
                    detail: describe_unprefixed(line),
                });
            }
        }
    }
    if old_seen != hunk.old_lines {
        return Err(PatchError::CountMismatch {
            expected: hunk.old_lines,
            found: old_seen,
        });
    }
    if new_seen != hunk.new_lines {
        return Err(PatchError::CountMismatch {
            expected: hunk.new_lines,
            found: new_seen,
        });
    }
    Ok(())
}

/// Where a hunk starts in the zero-based source, or why it cannot.
///
/// A hunk that consumes no old lines (`old_lines == 0`) inserts *after* `old_start`, so its start is
/// `old_start` itself; any other hunk starts at its one-based line converted to an index.
fn hunk_start(hunk: &Hunk, total_lines: usize, ordinal: usize) -> Result<usize, PatchError> {
    let start = if hunk.old_lines == 0 {
        usize::try_from(hunk.old_start).unwrap_or(usize::MAX)
    } else {
        if hunk.old_start == 0 {
            return Err(PatchError::InvalidHeader {
                line: ordinal,
                detail: "a hunk that consumes lines must start at line 1 or later".to_owned(),
            });
        }
        usize::try_from(hunk.old_start - 1).unwrap_or(usize::MAX)
    };
    if start > total_lines {
        return Err(PatchError::OutOfRange {
            line: hunk.old_start.max(1),
            total: u32::try_from(total_lines).unwrap_or(u32::MAX),
        });
    }
    Ok(start)
}
