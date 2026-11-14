//! Line-level comparison of two whole-file texts, for showing which lines an edit changed.
//!
//! [`file_watch`](crate::file_watch) answers "which paths changed": it walks the tree and compares
//! size and mtime against the previous walk. It never opens a file, so it cannot say *what* changed
//! inside one. Upstream has the other half: `packages/deliverables/workspace-changes/src/compare.ts`
//! compares the turn-start and turn-end text of a file and emits unified-diff hunks with three
//! context lines. That is what lets a client render "this turn changed these lines".
//!
//! This module is that second half, as a pure function over two `&str`. It touches no filesystem,
//! spawns no process, and knows nothing about git: the caller decides which two texts to compare.
//!
//! # Algorithm, and why it is not a real diff algorithm
//!
//! Upstream hands the comparison to the `diff` package, which implements Myers' algorithm. Myers is
//! optimal but it is hundreds of lines of back-tracking with subtle tie-breaking, and every one of
//! those branches is a place where an off-by-one produces a hunk that renders wrongly. This module
//! trades optimality for something small enough to read in one sitting:
//!
//! 1. Strip the longest common **prefix** and **suffix**. Those lines cannot be part of any change,
//!    so one edited path in a large file costs almost nothing.
//! 2. Align the remaining middle with a **longest common subsequence** table, which is exact but
//!    quadratic. This is the expensive step, and step 3 bounds it.
//! 3. If the middle is too large for that table ([`MAX_LCS_CELLS`]), give up on alignment and treat
//!    the middle as a full replacement: every old middle line removed, every new middle line added.
//! 4. Drop unchanged lines farther than `context` from a change, so the result is a set of windows
//!    rather than the whole file.
//!
//! So the *outer* part of a change is exact and the *middle* is exact whenever it is small enough to
//! align. The two places where it degrades are stated plainly:
//!
//! - **Coarse middle.** Past [`MAX_LCS_CELLS`], a small edit inside a large and badly-shifted region
//!   is reported as deleting and re-adding the whole middle. The line counts become upper bounds
//!   (`old_middle.len()` and `new_middle.len()`) rather than minimal edit counts.
//! - **No intra-line diff.** A one-character edit to a long line shows the whole line as removed
//!   and re-added. Word-level diffing is a separate problem and is not attempted here.
//!
//! Neither limitation produces a wrong *rendering*: the emitted lines still describe `after`
//! faithfully. They only describe it less tersely than an optimal diff would.
//!
//! # Bounds
//!
//! Two independent caps, each with a different consequence:
//!
//! - [`MAX_LCS_CELLS`] bounds quadratic work in the alignment step. Exceeding it degrades accuracy
//!   (see above) but never output size.
//! - [`MAX_DIFF_LINES`] bounds the size of [`TextDiff::lines`]. A hundred-thousand-line rewrite must
//!   not hand a client a hundred thousand lines, so emission stops at the cap and
//!   [`TextDiff::truncated`] says so. The counts in [`TextDiff::added`] and [`TextDiff::removed`]
//!   still describe the *whole* change: a caller showing "1,234 additions" must not under-report
//!   because the preview was cut.
//!
//! Truncation is applied to the end of the emitted sequence. The prefix of the diff, which is
//! usually the part a reader looks at, survives intact.
//!
//! # Line splitting
//!
//! Text is split the way a unified diff reads it: an empty string is zero lines, a trailing newline
//! terminates the final line instead of creating an empty one after it, and internal blank lines are
//! real lines. `"a\n"` and `"a"` therefore compare equal, matching upstream's `terminated`/`lines`
//! pair. Newlines are not carried in the emitted lines, so a client re-adds them when rendering.

/// Context lines kept around each change when the caller does not choose.
///
/// Three is the unified-diff default and what upstream uses (`CONTEXT_LINES` in
/// `workspace-changes/src/compare.ts`). It is enough to locate a change by eye without burying it.
pub const DEFAULT_CONTEXT: usize = 3;

/// Largest alignment table, in cells, that the LCS step will allocate.
///
/// The table is `(old + 1) * (new + 1)` entries, so this caps both time and memory at a known
/// ceiling (a quarter of a million entries). Beyond it the middle is reported as a replacement; see
/// the module docs. The constant is public so a caller can reason about the accuracy it is getting.
pub const MAX_LCS_CELLS: usize = 262_144;

/// Largest number of lines [`TextDiff::lines`] may hold.
///
/// A client renders a preview, not a storage format, so an unbounded list is a liability: a
/// regenerated lockfile would produce tens of thousands of `<div>`s for no benefit. The cap keeps
/// the payload bounded while [`TextDiff::truncated`] keeps it honest.
pub const MAX_DIFF_LINES: usize = 4_096;

/// One line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineDiff {
    /// Unchanged text, kept for context around a change.
    Context {
        /// The line's text, without its newline.
        line: String,
    },
    /// Text present in `before` and absent from `after`.
    Removed {
        /// The line's text, without its newline.
        line: String,
    },
    /// Text absent from `before` and present in `after`.
    Added {
        /// The line's text, without its newline.
        line: String,
    },
}

/// The line diff of two texts, with the counts and the truncation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDiff {
    /// The diff in reading order, already trimmed to the context windows.
    pub lines: Vec<LineDiff>,
    /// Lines added, counted over the whole change even when `lines` was truncated.
    pub added: usize,
    /// Lines removed, counted over the whole change even when `lines` was truncated.
    pub removed: usize,
    /// Whether `lines` stops before the end of the change.
    ///
    /// True only when the [`MAX_DIFF_LINES`] cap cut the output. A coarse middle (see the module
    /// docs) does not set it: that degradation changes how much is reported as changed, not how
    /// much of the report is present.
    pub truncated: bool,
}

/// One changed file, as a client would list it.
///
/// The counts are always present so a summary can show them; the diff is optional because a large
/// or binary file may be listed without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path relative to the workspace root.
    pub path: String,
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
    /// The full diff, when one was computed and is small enough to be worth sending.
    pub diff: Option<TextDiff>,
}

/// Compare two texts line by line.
///
/// `context` is the number of unchanged lines kept on each side of every change; zero keeps only
/// the changed lines themselves. Passing [`DEFAULT_CONTEXT`] matches upstream's rendering.
///
/// The result is bounded by [`MAX_DIFF_LINES`]; when that cap cuts the output, `truncated` is set
/// while `added` and `removed` still count the whole change.
pub fn diff_text(before: &str, after: &str, context: usize) -> TextDiff {
    let old = split_lines(before);
    let new = split_lines(after);

    // Lines outside the common ends cannot belong to any change, so excluding them first is what
    // keeps the quadratic step small for the ordinary "one edit in a large file" case.
    let prefix = common_prefix(&old, &new);
    let suffix = common_suffix(&old[prefix..], &new[prefix..]);
    let old_middle = &old[prefix..old.len() - suffix];
    let new_middle = &new[prefix..new.len() - suffix];

    let (pieces, matches) = match align(old_middle, new_middle) {
        Some((pieces, matches)) => (pieces, matches),
        None => (replace_all(old_middle, new_middle), 0),
    };

    let layout = Layout {
        before: &old,
        after: &new,
        prefix,
        pieces: &pieces,
        suffix,
    };

    let mut lines = Vec::new();
    let mut truncated = false;
    for (start, end) in windows(&layout, context) {
        if !push_window(&layout, start, end, &mut lines) {
            truncated = true;
            break;
        }
    }

    TextDiff {
        lines,
        added: new_middle.len() - matches,
        removed: old_middle.len() - matches,
        truncated,
    }
}

/// One line of the alignment, borrowing the text it came from.
///
/// Emission clones only the lines that survive the context windows and the cap, which is why the
/// alignment carries references rather than owned [`LineDiff`] values.
#[derive(Debug, Clone, Copy)]
enum Piece<'a> {
    Context(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

impl Piece<'_> {
    /// Materialise the piece as the public line type.
    fn to_line(self) -> LineDiff {
        match self {
            Piece::Context(line) => LineDiff::Context {
                line: line.to_owned(),
            },
            Piece::Removed(line) => LineDiff::Removed {
                line: line.to_owned(),
            },
            Piece::Added(line) => LineDiff::Added {
                line: line.to_owned(),
            },
        }
    }
}

/// The aligned middle between the common prefix and the common suffix.
///
/// Indices are absolute positions in the concatenation `prefix + pieces + suffix`, so the context
/// windows can be computed without materialising the unchanged ends.
struct Layout<'a> {
    before: &'a [&'a str],
    after: &'a [&'a str],
    prefix: usize,
    pieces: &'a [Piece<'a>],
    suffix: usize,
}

impl<'a> Layout<'a> {
    /// Total number of lines in the aligned sequence.
    fn len(&self) -> usize {
        self.prefix + self.pieces.len() + self.suffix
    }

    /// The piece at an absolute index.
    ///
    /// The suffix is taken from `after`; the two suffixes are equal by construction, so either side
    /// would do and only one is read.
    fn piece(&self, index: usize) -> Piece<'a> {
        if index < self.prefix {
            return Piece::Context(self.before[index]);
        }
        let offset = index - self.prefix;
        if offset < self.pieces.len() {
            return self.pieces[offset];
        }
        let suffix_offset = offset - self.pieces.len();
        Piece::Context(self.after[self.after.len() - self.suffix + suffix_offset])
    }
}

/// Split text into lines, treating a trailing newline as a terminator rather than a separator.
///
/// This mirrors how a unified diff presents a file: `"a\n"` is the single line `a`, and `""` is no
/// lines at all. The alternative (splitting on every `\n`) would make every file appear to have an
/// extra empty last line and would report a spurious change whenever one side gained a newline.
fn split_lines(text: &str) -> Vec<&str> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    body.split('\n').collect()
}

/// Length of the common prefix of two line slices.
fn common_prefix(old: &[&str], new: &[&str]) -> usize {
    old.iter().zip(new).take_while(|(a, b)| a == b).count()
}

/// Length of the common suffix of two line slices, not counting past the shorter side.
fn common_suffix(old: &[&str], new: &[&str]) -> usize {
    let limit = old.len().min(new.len());
    let mut count = 0;
    while count < limit && old[old.len() - 1 - count] == new[new.len() - 1 - count] {
        count += 1;
    }
    count
}

/// Align two middles with an LCS table.
///
/// Returns the pieces in reading order and the number of matched lines, or `None` when the table
/// would exceed [`MAX_LCS_CELLS`]. A matched line is emitted as context; everything else is a
/// removal or an addition. Where the table offers two equally good moves the walk prefers an
/// addition, which keeps a replaced line's `-` before its `+` in the common case.
fn align<'a>(old: &[&'a str], new: &[&'a str]) -> Option<(Vec<Piece<'a>>, usize)> {
    let cells = (old.len() + 1).checked_mul(new.len() + 1)?;
    if cells > MAX_LCS_CELLS {
        return None;
    }

    let width = new.len() + 1;
    let mut table = vec![0usize; cells];
    for (i, old_line) in old.iter().enumerate() {
        for (j, new_line) in new.iter().enumerate() {
            let here = (i + 1) * width + (j + 1);
            table[here] = if old_line == new_line {
                table[i * width + j] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + (j + 1)])
            };
        }
    }

    let mut pieces = Vec::with_capacity(old.len() + new.len());
    let (mut i, mut j) = (old.len(), new.len());
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old[i - 1] == new[j - 1] {
            pieces.push(Piece::Context(old[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || table[i * width + (j - 1)] >= table[(i - 1) * width + j]) {
            pieces.push(Piece::Added(new[j - 1]));
            j -= 1;
        } else {
            pieces.push(Piece::Removed(old[i - 1]));
            i -= 1;
        }
    }
    pieces.reverse();

    Some((pieces, table[old.len() * width + new.len()]))
}

/// Report the middle as a full replacement, used when alignment is too expensive.
///
/// Removals come first so the rendering reads "old block, then new block" rather than interleaving
/// the two, which is the shape a reader expects from a rewritten region.
fn replace_all<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Piece<'a>> {
    let mut pieces = Vec::with_capacity(old.len() + new.len());
    pieces.extend(old.iter().map(|line| Piece::Removed(line)));
    pieces.extend(new.iter().map(|line| Piece::Added(line)));
    pieces
}

/// Merge the windows of context that must be kept, in ascending order.
///
/// Each change asks for `[index - context, index + context]`; overlapping requests are merged so a
/// run of changes renders as one block with no repeated or out-of-order lines.
fn windows(layout: &Layout<'_>, context: usize) -> Vec<(usize, usize)> {
    let total = layout.len();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (offset, piece) in layout.pieces.iter().enumerate() {
        if matches!(piece, Piece::Context(_)) {
            continue;
        }
        let index = layout.prefix + offset;
        let start = index.saturating_sub(context);
        let end = index.saturating_add(context).saturating_add(1).min(total);
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Append one window, stopping at [`MAX_DIFF_LINES`].
///
/// Returns false when a line had to be dropped. The check sits inside the loop rather than before
/// it so a window that ends exactly at the cap is not reported as truncated.
fn push_window(layout: &Layout<'_>, start: usize, end: usize, lines: &mut Vec<LineDiff>) -> bool {
    for index in start..end {
        if lines.len() >= MAX_DIFF_LINES {
            return false;
        }
        lines.push(layout.piece(index).to_line());
    }
    true
}
