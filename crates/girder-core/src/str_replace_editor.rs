//! Ordered replacement steps over one text, where ambiguity is a hard failure.
//!
//! `observation::replace_exactly_once` already refuses to edit a text whose target is not unique, and the `edit`
//! tool is built on it. What that function cannot express is a *plan*: several replacements that must land in a
//! known order, or a deliberate choice of the third occurrence of a line that legitimately repeats. This module
//! carries those two additions and nothing else.
//!
//! Two refusals are the point of the module rather than an inconvenience around it:
//!
//! * [`EditFailure::Ambiguous`] — a text that occurs more than once is refused unless the caller names which
//!   occurrence it means. Taking the first match silently is how an editor rewrites the wrong function body while
//!   reporting success; the caller only finds out on the next read, if at all.
//! * [`EditFailure::NoChange`] — a plan that produces the identical text is refused too. That means the caller's
//!   model of the file is wrong (a stale copy, a typo on the `new` side, two steps that cancel), and reporting
//!   success would hide exactly that.
//!
//! Steps run in order, each against the text the previous step produced, so `occurrence` always refers to the text
//! as it stands at that step. The failure type carries what went wrong but not where it went wrong, because an
//! error is often logged without the content in hand; [`EditFailure::describe`] takes the content back and renders
//! numbered lines around the match, which is what a caller needs to see its own mistake.

use std::fmt::Write as _;

/// Lines of context rendered on each side of a located match.
const CONTEXT_RADIUS: usize = 2;

/// Most occurrences spelled out in one failure message, so a pathological plan cannot produce a wall of text.
const MAX_LISTED_OCCURRENCES: usize = 4;

/// Longest caller-supplied snippet quoted back into a message.
const MAX_QUOTED_CHARS: usize = 240;

/// One replacement step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    /// The text to replace. It must occur exactly once, or [`Replacement::occurrence`] must say which match is
    /// meant.
    pub old: String,
    /// What to put in its place.
    pub new: String,
    /// Which match to replace, counting from zero, when `old` occurs more than once. `None` demands uniqueness.
    ///
    /// An explicit index is not a convenience for the lazy caller: it is how a caller states that it knows the text
    /// repeats and has chosen a specific one. Without that statement the edit is refused.
    pub occurrence: Option<usize>,
}

impl Replacement {
    /// A step that requires `old` to occur exactly once.
    ///
    /// The common case, and the safe one: the text itself proves the edit cannot land in the wrong place.
    #[must_use]
    pub fn unique(old: impl Into<String>, new: impl Into<String>) -> Self {
        Self {
            old: old.into(),
            new: new.into(),
            occurrence: None,
        }
    }

    /// A step that replaces the `occurrence`-th (zero based) match of `old`.
    ///
    /// Prefer [`Replacement::unique`]: an index is stable only until the text around it changes, and a call that
    /// reads "the second one" ages badly. It exists for the case where the repeated text is genuinely repeated and
    /// adding context would be a lie about the file.
    #[must_use]
    pub fn nth(old: impl Into<String>, new: impl Into<String>, occurrence: usize) -> Self {
        Self {
            old: old.into(),
            new: new.into(),
            occurrence: Some(occurrence),
        }
    }
}

/// Why a replacement plan was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditFailure {
    /// The text to replace does not appear in the content.
    #[error("the text to replace was not found")]
    NotFound {
        /// The text that was looked for.
        old: String,
    },

    /// The text occurs more than once and the step did not say which occurrence it meant.
    ///
    /// Refused rather than resolved to the first match: choosing for the caller edits text the caller never
    /// identified, and the wrong choice is invisible in the result.
    #[error("the text to replace occurs {count} times; set `occurrence` to pick one")]
    Ambiguous {
        /// The text that was looked for.
        old: String,
        /// How many times it occurs.
        count: usize,
    },

    /// The requested occurrence is past the last match.
    #[error("`occurrence` {index} is out of range; the text occurs {count} time(s)")]
    OutOfRange {
        /// The index the caller asked for.
        index: usize,
        /// How many times the text occurs.
        count: usize,
    },

    /// Applying the plan produced the original text.
    ///
    /// Either a step replaces text with itself, or the steps cancel each other. Both mean the caller expected a
    /// change that is not there.
    #[error("the replacement plan leaves the text unchanged")]
    NoChange,
}

impl EditFailure {
    /// Renders this failure against the text it was raised on, with numbered lines around the match.
    ///
    /// The variants record what went wrong and not where, so that an error can be logged on its own; this method is
    /// the second half, and it is what turns "not found" into a line number and a window the caller can compare
    /// with its own `old` string.
    ///
    /// [`EditFailure::OutOfRange`] carries no `old`, so it can report the valid index range but not a location; the
    /// caller already knows which text it asked for there.
    #[must_use]
    pub fn describe(&self, content: &str) -> String {
        let mut message = self.to_string();
        match self {
            Self::NotFound { old } => {
                let _ = write!(message, ": `{}`", quote(old));
                if old.is_empty() {
                    message.push_str("\n`old` is empty, so there is nothing to search for");
                    return message;
                }
                if let Some(at) = anchor_offset(content, old) {
                    let _ = write!(
                        message,
                        "\nthe first line of `old` does appear, at line {}; the text around it is:\n",
                        line_number(content, at)
                    );
                    message.push_str(&window(content, at));
                    message.push_str(
                        "\nthe rest of `old` does not match there verbatim: check indentation and whitespace",
                    );
                } else {
                    message.push_str(
                        "\nno line of `old` appears verbatim, so the caller is working from a different version of the text: check spelling, indentation, and line endings",
                    );
                }
            }
            Self::Ambiguous { old, .. } => {
                let _ = write!(message, ": `{}`", quote(old));
                let offsets = offsets_of(content, old);
                for (index, at) in offsets.iter().take(MAX_LISTED_OCCURRENCES).enumerate() {
                    let _ = write!(
                        message,
                        "\noccurrence {index} is at line {}:\n",
                        line_number(content, *at)
                    );
                    message.push_str(&window(content, *at));
                }
                if let Some(hidden) = offsets.len().checked_sub(MAX_LISTED_OCCURRENCES) {
                    if hidden > 0 {
                        let _ = write!(
                            message,
                            "\nand {hidden} further occurrence(s) are not shown"
                        );
                    }
                }
                message.push_str(
                    "\nextend `old` with neighbouring lines until it is unique, or set `occurrence` to choose one",
                );
            }
            Self::OutOfRange { count, .. } => {
                if *count == 0 {
                    message.push_str("\nthe text does not occur at all");
                } else if let Some(last) = count.checked_sub(1) {
                    let _ = write!(
                        message,
                        "\nthe valid indices run from 0 to {last}, so the requested match does not exist"
                    );
                }
            }
            Self::NoChange => {
                message.push_str(
                    "\nno step changed the text: either a step replaces text with itself, or the steps cancel each other",
                );
            }
        }
        message
    }
}

/// Applies `replacements` to `content` in order and returns the result.
///
/// Each step sees the output of the step before it, so a later step may target text an earlier one introduced.
///
/// No partial result is returned and nothing is written: the caller receives the finished text or the first
/// failure, which is what lets a plan be validated before a file is touched.
///
/// # Errors
///
/// Returns [`EditFailure::NotFound`] when a step's text is absent (or empty, since an empty needle matches at every
/// position), [`EditFailure::Ambiguous`] when it occurs more than once and the step names no occurrence,
/// [`EditFailure::OutOfRange`] when the named occurrence does not exist, and [`EditFailure::NoChange`] when the
/// finished text equals `content` — including an empty plan, which asks for nothing.
pub fn apply(content: &str, replacements: &[Replacement]) -> Result<String, EditFailure> {
    let mut current = content.to_owned();
    for step in replacements {
        current = apply_one(&current, step)?;
    }
    if current == content {
        return Err(EditFailure::NoChange);
    }
    Ok(current)
}

/// Applies one step, refusing the two ways a step can land in the wrong place.
fn apply_one(text: &str, step: &Replacement) -> Result<String, EditFailure> {
    // An empty needle matches at every character boundary, so it is reported as absent: there is no "the empty
    // string occurs 0 times" position to replace, and `str::match_indices` would otherwise hand back a match per
    // byte offset.
    if step.old.is_empty() {
        return Err(EditFailure::NotFound {
            old: step.old.clone(),
        });
    }
    // A step that maps a text onto itself cannot be what the caller meant. Refused here rather than left for the
    // final NoChange check, so a batch containing one dead step fails at that step instead of succeeding because
    // its other steps happened to change something.
    if step.old == step.new {
        return Err(EditFailure::NoChange);
    }

    let offsets = offsets_of(text, &step.old);
    let count = offsets.len();
    if count == 0 {
        return Err(EditFailure::NotFound {
            old: step.old.clone(),
        });
    }
    let index = match step.occurrence {
        Some(index) => {
            if index >= count {
                return Err(EditFailure::OutOfRange { index, count });
            }
            index
        }
        None if count > 1 => {
            return Err(EditFailure::Ambiguous {
                old: step.old.clone(),
                count,
            });
        }
        None => 0,
    };

    let at = offsets.get(index).copied().unwrap_or(0);
    let mut out = String::with_capacity(text.len() + step.new.len());
    out.push_str(&text[..at]);
    out.push_str(&step.new);
    // `at` came from a match of `old`, so both boundaries are character boundaries.
    out.push_str(&text[at + step.old.len()..]);
    Ok(out)
}

/// Byte offsets of every non-overlapping match of `needle`, in order.
fn offsets_of(text: &str, needle: &str) -> Vec<usize> {
    text.match_indices(needle).map(|(at, _)| at).collect()
}

/// Byte offsets at which each line starts.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (at, character) in text.char_indices() {
        // A newline at the very end does not begin a line: the file ends rather than starting an empty one.
        if character == '\n' && at + 1 < text.len() {
            starts.push(at + 1);
        }
    }
    starts
}

/// The one-based line number containing `offset`.
fn line_number(text: &str, offset: usize) -> usize {
    line_index(&line_starts(text), offset) + 1
}

/// The zero-based line index containing `offset`.
fn line_index(starts: &[usize], offset: usize) -> usize {
    starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1)
}

/// Numbered lines around `offset`, with the matching line marked.
fn window(text: &str, offset: usize) -> String {
    let starts = line_starts(text);
    let line = line_index(&starts, offset);
    let first = line.saturating_sub(CONTEXT_RADIUS);
    let last = (line + CONTEXT_RADIUS).min(starts.len().saturating_sub(1));

    let mut out = String::new();
    if first > 0 {
        let _ = writeln!(out, "      ... {first} earlier line(s) ...");
    }
    for index in first..=last {
        let start = starts.get(index).copied().unwrap_or(text.len());
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        let body = text
            .get(start..end)
            .unwrap_or_default()
            .trim_end_matches('\n');
        let marker = if index == line { '>' } else { ' ' };
        let _ = writeln!(out, "{marker}{:>6} | {body}", index + 1);
    }
    let remaining = starts.len().saturating_sub(1).saturating_sub(last);
    if remaining > 0 {
        let _ = writeln!(out, "      ... {remaining} later line(s) ...");
    }
    out
}

/// Where to look when the whole of `old` is absent: its first non-blank line, which is what a caller usually
/// mistyped the rest of.
fn anchor_offset(text: &str, old: &str) -> Option<usize> {
    let anchor = old.lines().find(|line| !line.trim().is_empty())?;
    offsets_of(text, anchor).into_iter().next()
}

/// A single-line rendering of a caller-supplied snippet, bounded in length.
fn quote(text: &str) -> String {
    let mut out = String::new();
    for (count, character) in text.chars().enumerate() {
        if count >= MAX_QUOTED_CHARS {
            out.push('…');
            break;
        }
        match character {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}
