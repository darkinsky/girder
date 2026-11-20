//! Patch parsing and text application, independent of any filesystem.
//!
//! A patch is a compact way to say how a file changed: hunks of context, removed, and added lines. Applying one
//! is a text operation, not a filesystem one, which is why it lives here rather than beside the write path — a
//! caller can validate a patch before touching a disk, and a test can check the application without one.
//!
//! # Why context decides where a hunk lands
//!
//! A hunk carries a line number, and that number is wrong whenever the file changed since the patch was made. The
//! context is what survives: if the surrounding lines are where the hunk says they are, the hunk applies, and if
//! they are a few lines away the hunk applies there. Trusting the number would corrupt a file whose only change was
//! an inserted line above.
//!
//! # Why a failure is a list rather than the first error
//!
//! A patch with three hunks and one that does not apply is a patch that needs editing. Reporting only the first bad
//! hunk makes the caller fix one, re-run, and discover the next — three round trips where one would do.
//!
//! # Why the old lines are compared exactly
//!
//! Whitespace is content. A matcher that normalised trailing whitespace would apply a hunk to a line that differs
//! from the one the patch was made against, and the resulting file would be subtly wrong in a way no test catches.

/// One contiguous update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// The one-based input line where the hunk begins.
    ///
    /// A hint, not a guarantee: [`apply`] searches for the context and uses this as a starting point.
    pub line_number: u32,
    /// The text after `@@ `, used as a search anchor when the line number is wrong.
    pub context: Option<String>,
    /// Context and removed lines, without patch prefixes.
    pub old_lines: Vec<String>,
    /// Context and added lines, without patch prefixes.
    pub new_lines: Vec<String>,
    /// Whether `*** End of File` follows this hunk.
    ///
    /// Recorded because it constrains where the hunk may land: a hunk marked as at the end must match at the end.
    pub is_end_of_file: bool,
}

impl Hunk {
    /// How many lines this hunk removes.
    #[must_use]
    pub fn removed(&self) -> usize {
        self.old_lines.len().saturating_sub(self.context_lines())
    }

    /// How many lines this hunk adds.
    #[must_use]
    pub fn added(&self) -> usize {
        self.new_lines.len().saturating_sub(self.context_lines())
    }

    /// How many context lines it shares with the file.
    ///
    /// Counted as the lines present in both, which is what makes a hunk findable: a hunk with no context can only
    /// be applied at its stated line.
    fn context_lines(&self) -> usize {
        self.old_lines
            .iter()
            .zip(self.new_lines.iter())
            .take_while(|(old, new)| old == new)
            .count()
            .max(
                self.old_lines
                    .iter()
                    .rev()
                    .zip(self.new_lines.iter().rev())
                    .take_while(|(old, new)| old == new)
                    .count(),
            )
    }
}

/// One file operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Create a file with this content.
    Add {
        /// Where.
        path: String,
        /// The line the operation appeared at, for an error message.
        line_number: u32,
        /// Its content, with a trailing newline for every line.
        content: String,
    },
    /// Delete a file.
    Delete {
        /// Where.
        path: String,
        /// The line the operation appeared at.
        line_number: u32,
    },
    /// Update a file.
    Update {
        /// Where.
        path: String,
        /// The line the operation appeared at.
        line_number: u32,
        /// Where to move it after updating, when the patch says so.
        ///
        /// A move is an update with a destination: the content changes and the path changes, and modelling it as
        /// two operations would let one succeed without the other.
        move_to: Option<String>,
        /// Its hunks, in order.
        hunks: Vec<Hunk>,
    },
}

impl Operation {
    /// The path it names.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path, .. } | Self::Update { path, .. } => path,
        }
    }

    /// The line it appeared at.
    #[must_use]
    pub fn line_number(&self) -> u32 {
        match self {
            Self::Add { line_number, .. }
            | Self::Delete { line_number, .. }
            | Self::Update { line_number, .. } => *line_number,
        }
    }
}

/// Why a patch could not be parsed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    /// A marker line was not understood.
    #[error("line {line}: {message}")]
    Marker {
        /// The one-based line.
        line: u32,
        /// What was expected.
        message: String,
    },

    /// A hunk began without a header.
    #[error("line {line}: a hunk must begin with `@@`")]
    HunkHeader {
        /// The one-based line.
        line: u32,
    },

    /// A patch named no operation.
    #[error("the patch is empty")]
    Empty,
}

/// Closes the open hunk and operation, pushing them onto the finished list.
///
/// A hunk must be closed before its operation: pushing the operation first would move it past the hunk that belongs
/// to it.
fn flush(
    current: &mut Option<Operation>,
    hunk: &mut Option<Hunk>,
    operations: &mut Vec<Operation>,
) {
    if let (Some(Operation::Update { hunks, .. }), Some(open)) = (current.as_mut(), hunk.take()) {
        hunks.push(open);
    }
    if let Some(done) = current.take() {
        operations.push(done);
    }
}

/// Records one body line for the open operation.
///
/// A separate function so the parser stays under the line limit the lint enforces, and so the one place a prefix is
/// interpreted is readable on its own: an added file's lines are all additions, while an update's carry a prefix.
fn push_body_line(
    current: &mut Option<Operation>,
    hunk: &mut Option<Hunk>,
    trimmed: &str,
    line: u32,
) -> Result<(), ParseError> {
    match current.as_mut() {
        Some(Operation::Add { content, .. }) => {
            // A leading `+` is part of the syntax, not the content.
            let body = trimmed.strip_prefix('+').unwrap_or(trimmed);
            content.push_str(body);
            content.push('\n');
            Ok(())
        }
        Some(Operation::Update { .. }) => {
            let open = hunk.as_mut().ok_or(ParseError::HunkHeader { line })?;
            if trimmed == "*** End of File" {
                open.is_end_of_file = true;
                return Ok(());
            }
            let (prefix, rest) = trimmed.split_at(
                trimmed
                    .char_indices()
                    .nth(1)
                    .map_or(trimmed.len(), |(index, _)| index),
            );
            match prefix {
                " " => {
                    open.old_lines.push(rest.to_owned());
                    open.new_lines.push(rest.to_owned());
                }
                "-" => open.old_lines.push(rest.to_owned()),
                "+" => open.new_lines.push(rest.to_owned()),
                // An unprefixed line is context, which is how a hand-written patch reads.
                _ => {
                    open.old_lines.push(trimmed.to_owned());
                    open.new_lines.push(trimmed.to_owned());
                }
            }
            Ok(())
        }
        Some(Operation::Delete { .. }) => Ok(()),
        None => Err(ParseError::Marker {
            line,
            message: "expected a file operation".to_owned(),
        }),
    }
}

/// Parses a patch in the `*** Begin Patch` / `*** Update File:` form.
///
/// # Errors
///
/// Returns [`ParseError`] naming the line where parsing failed, so a caller can point at it rather than at the
/// patch as a whole.
pub fn parse(text: &str) -> Result<Vec<Operation>, ParseError> {
    let mut operations: Vec<Operation> = Vec::new();
    let mut current: Option<Operation> = None;
    let mut hunk: Option<Hunk> = None;

    for (index, raw) in text.lines().enumerate() {
        let line = u32::try_from(index).unwrap_or(u32::MAX) + 1;
        let trimmed = raw.trim_end();

        if trimmed == "*** Begin Patch" || trimmed == "*** End Patch" || trimmed.is_empty() {
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("*** Add File: ") {
            flush(&mut current, &mut hunk, &mut operations);
            current = Some(Operation::Add {
                path: path.trim().to_owned(),
                line_number: line,
                content: String::new(),
            });
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("*** Delete File: ") {
            flush(&mut current, &mut hunk, &mut operations);
            current = Some(Operation::Delete {
                path: path.trim().to_owned(),
                line_number: line,
            });
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("*** Update File: ") {
            flush(&mut current, &mut hunk, &mut operations);
            current = Some(Operation::Update {
                path: path.trim().to_owned(),
                line_number: line,
                move_to: None,
                hunks: Vec::new(),
            });
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("*** Move to: ") {
            match current.as_mut() {
                Some(Operation::Update { move_to, .. }) => {
                    *move_to = Some(path.trim().to_owned());
                    continue;
                }
                _ => {
                    return Err(ParseError::Marker {
                        line,
                        message: "`*** Move to:` must follow an update".to_owned(),
                    });
                }
            }
        }

        if let Some(rest) = trimmed.strip_prefix("@@") {
            // A hunk header closes the previous hunk and opens a new one.
            if let (Some(Operation::Update { hunks, .. }), Some(open)) =
                (current.as_mut(), hunk.take())
            {
                hunks.push(open);
            }
            let context = rest.trim();
            hunk = Some(Hunk {
                line_number: line,
                context: (!context.is_empty()).then(|| context.to_owned()),
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                is_end_of_file: false,
            });
            continue;
        }

        push_body_line(&mut current, &mut hunk, trimmed, line)?;
    }

    flush(&mut current, &mut hunk, &mut operations);
    if operations.is_empty() {
        return Err(ParseError::Empty);
    }
    Ok(operations)
}

/// Why one hunk did not apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// Which hunk, zero-based within its file.
    pub hunk: usize,
    /// The line the hunk expected to start at.
    pub expected: u32,
    /// Why it failed.
    pub reason: Reason,
}

/// Why a hunk did not apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The context was not found anywhere in the file.
    ContextNotFound,
    /// The context was found more than once, so the hunk is ambiguous.
    Ambiguous {
        /// How many places matched.
        matches: usize,
    },
    /// The hunk is marked as at the end but the context is not.
    NotAtEnd,
}

/// The result of applying hunks to a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The text after every hunk that applied.
    ///
    /// When any hunk failed, this is the input unchanged: a partial application would leave a file that is neither
    /// the old one nor the new one, which is worse than not applying at all.
    pub text: String,
    /// The hunks that did not apply.
    pub failures: Vec<Failure>,
    /// How many hunks applied.
    pub applied: usize,
}

impl Applied {
    /// Whether every hunk applied.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Applies hunks to a text.
///
/// All or nothing: a partial application would leave a file that is neither the old one nor the new one.
#[must_use]
pub fn apply(text: &str, hunks: &[Hunk]) -> Applied {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut failures = Vec::new();
    let mut applied = 0;

    for (index, hunk) in hunks.iter().enumerate() {
        match find(&lines, hunk) {
            Ok(at) => {
                lines.splice(at..at + hunk.old_lines.len(), hunk.new_lines.clone());
                applied += 1;
            }
            Err(reason) => failures.push(Failure {
                hunk: index,
                expected: hunk.line_number,
                reason,
            }),
        }
    }

    if !failures.is_empty() {
        return Applied {
            text: text.to_owned(),
            failures,
            applied: 0,
        };
    }

    let mut out = lines.join("\n");
    // A text that had a trailing newline keeps one: dropping it would make every applied patch show as a
    // whitespace change at the end of the file.
    if text.ends_with('\n') {
        out.push('\n');
    }
    Applied {
        text: out,
        failures,
        applied,
    }
}

/// Finds where a hunk belongs.
fn find(lines: &[String], hunk: &Hunk) -> Result<usize, Reason> {
    if hunk.old_lines.is_empty() {
        // An insertion with no context can only go at its stated line.
        let at = usize::try_from(hunk.line_number.saturating_sub(1))
            .unwrap_or(0)
            .min(lines.len());
        return Ok(at);
    }

    let mut matches = Vec::new();
    if hunk.old_lines.len() <= lines.len() {
        for start in 0..=lines.len() - hunk.old_lines.len() {
            if lines[start..start + hunk.old_lines.len()] == hunk.old_lines[..] {
                matches.push(start);
            }
        }
    }

    if matches.is_empty() {
        return Err(Reason::ContextNotFound);
    }

    let at = if hunk.is_end_of_file {
        // A hunk marked as at the end must match at the end.
        *matches
            .last()
            .filter(|start| *start + hunk.old_lines.len() == lines.len())
            .ok_or(Reason::NotAtEnd)?
    } else if matches.len() == 1 {
        matches[0]
    } else {
        // Several places match. The stated line breaks the tie only when it points at one of them: a patch made
        // against a different version of the file has a number that lands nowhere near the block, and picking the
        // nearest would edit a function the patch was never about.
        let stated = usize::try_from(hunk.line_number.saturating_sub(1)).unwrap_or(0);
        let exact: Vec<usize> = matches
            .iter()
            .copied()
            .filter(|start| *start == stated)
            .collect();
        if let [at] = exact.as_slice() {
            return Ok(*at);
        }
        return Err(Reason::Ambiguous {
            matches: matches.len(),
        });
    };
    Ok(at)
}
