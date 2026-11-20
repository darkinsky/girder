//! Aligned text for the tools that list files and search through them.
//!
//! `glob`, `grep` and `ls` each build their output by concatenating strings, so a model reading a
//! listing has to infer which column is which from the prose. [`crate::core::tool_value`] can render a
//! table, but nothing in the crate has ever handed it one, and it pads by character count. This
//! module does the two things neither does: it turns the tools' own line-oriented output back into
//! rows ([`parse_glob_output`], [`parse_grep_output`]), and it renders those rows with every column
//! padded to its widest cell ([`render_files`], [`render_matches`]).
//!
//! # Why alignment is measured in display cells
//!
//! A path such as `文档/说明.md` occupies seven cells but only five characters, and its bytes number
//! eleven. Three different numbers, and only one of them answers the question the layout asks: how
//! far right does the next column start. Padding by `chars().count()` emits two spaces too few for
//! every wide character to its left, so each following column drifts one cell right per CJK
//! character — invisible in the byte length of the line, obvious in the grid, and exactly the
//! failure this module exists to prevent: a model pairs a size with its neighbour's path.
//!
//! The module therefore counts cells, using the East Asian Width rule a terminal applies. For text
//! that contains no wide characters the cell count equals the character count, so nothing about the
//! common ASCII listing changes; the two diverge only where counting characters would have been
//! wrong. Bytes are never the measure: a UTF-8 path mixes one-, two- and three-byte characters
//! freely, and no byte count corresponds to a column.
//!
//! # Why the parsers are defensive
//!
//! The tools append notices to their own output — ``no files match `*.xyz` `` and
//! `(stopped after 200 files; narrow the pattern to see the rest)` — on lines that are not paths.
//! A renderer that treated those as rows would print a table with a file named `(stopped after`.
//! Both parsers drop notice lines and blank lines rather than trusting the caller to strip them.

/// One file entry, as `glob` or `ls` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// The path as the tool reported it; relative to the workspace when the tool confined it so.
    pub path: String,
    /// Size in bytes. Kept exact rather than humanized: `1.5 KiB` is a reading, `1536` is a fact.
    pub bytes: u64,
    /// Modification time in milliseconds on the UTC timeline, the unit [`crate::TimeInstant`] uses.
    pub modified_ms: u64,
}

/// One search hit, as `grep` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchEntry {
    /// The file the match was found in.
    pub path: String,
    /// One-based line number, matching what an editor and the model's own `read` call report.
    pub line: u32,
    /// The matched line's text, without its trailing newline.
    pub text: String,
}

/// Renders file entries as an aligned three-column table.
///
/// The columns are `path`, `bytes` and `modified`, always in that order. `bytes` is right-aligned so
/// that magnitudes compare by eye down the column; `path` is left-aligned, as text always is.
/// An empty slice renders as the empty string — a heading with no rows under it reads as a broken
/// result rather than as "nothing was found", and the caller is expected to say "no files match" in
/// its own words.
///
/// The result carries no trailing newline, matching the rest of the crate's renderers; a tool that
/// sends it as `ToolResult::text` adds the newline it needs at the boundary.
#[must_use]
pub fn render_files(entries: &[FileEntry]) -> String {
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| {
            vec![
                entry.path.clone(),
                entry.bytes.to_string(),
                modified_cell(entry.modified_ms),
            ]
        })
        .collect();
    layout(
        &[
            Column::left("path"),
            Column::right("bytes"),
            Column::left("modified"),
        ],
        &rows,
    )
}

/// Renders match entries as an aligned three-column table.
///
/// The columns are `path`, `line` and `text`. `line` is right-aligned for the same reason `bytes`
/// is, so that a run of hits in one file reads as a staircase of line numbers. An empty slice
/// renders as the empty string, and the text of a hit is flattened onto one line: a newline inside
/// it would otherwise split one row in two and left-shift the second half by a column.
#[must_use]
pub fn render_matches(entries: &[MatchEntry]) -> String {
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| {
            vec![
                entry.path.clone(),
                entry.line.to_string(),
                entry.text.clone(),
            ]
        })
        .collect();
    layout(
        &[
            Column::left("path"),
            Column::right("line"),
            Column::left("text"),
        ],
        &rows,
    )
}

/// Extracts the paths from `glob`'s line output.
///
/// One path per line is the whole format, so a line is a path verbatim — not trimmed of leading
/// spaces, because a file name may legitimately begin with one. Blank lines, the
/// ``no files match `...` `` notice and the parenthesized `(stopped after ...)` trailer are
/// dropped, since none of them is a file and all of them would render as one.
#[must_use]
pub fn parse_glob_output(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !is_notice(line))
        .map(str::to_owned)
        .collect()
}

/// Extracts the match entries from `grep`'s `path:line:text` output.
///
/// The path may itself contain a colon — `C:\a\b.txt:12:x` is one line with one path, not three
/// fields — so the split point is the *leftmost* `:<digits>:` candidate rather than the first or
/// the last colon. Scanning left to right and rejecting any field that is not all decimal digits is
/// what makes the drive-letter case land on the right boundary: the first colon is separated from
/// the second by `\a\b.txt`, which is not a number, so the scan moves on.
///
/// Everything after the line number is text and is kept verbatim, colons included. Lines that
/// contain no such boundary — the `no matches for ...` notice, the `(stopped after ...)` trailer,
/// a `--no-heading` file header — are skipped rather than forced into a row with an invented line
/// number: a wrong number is read as a fact, a missing row is visible.
#[must_use]
pub fn parse_grep_output(text: &str) -> Vec<MatchEntry> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !is_notice(line))
        .filter_map(|line| {
            let (path, number, body) = split_match_line(line)?;
            Some(MatchEntry {
                path: path.to_owned(),
                line: number,
                text: body.to_owned(),
            })
        })
        .collect()
}

/// How a column's cells sit inside their padded width.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    /// Text: pad on the right, so the first character of each cell lines up.
    Left,
    /// Numbers: pad on the left, so the last digit of each cell lines up and magnitudes compare.
    Right,
}

/// One column of a rendered table: its heading, and how its cells are padded.
struct Column {
    /// The heading printed above the column, and the source of the column's minimum width.
    header: &'static str,
    /// Where the padding goes.
    align: Align,
}

impl Column {
    /// A column whose cells start at the same cell of every row.
    const fn left(header: &'static str) -> Self {
        Self {
            header,
            align: Align::Left,
        }
    }

    /// A column whose cells end at the same cell of every row.
    const fn right(header: &'static str) -> Self {
        Self {
            header,
            align: Align::Right,
        }
    }
}

/// Lays rows out under their headings, padded in display cells.
///
/// The shape — headings, a rule, then rows, each cell wrapped in a space and a bar — is the one
/// [`crate::render_tool_value`] produces for a table, so a listing built here and one built there
/// read the same way. Only the padding unit differs: display cells here.
fn layout(columns: &[Column], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    // Every cell is flattened before it is measured: a line break inside a cell would split one row
    // into two, and the second half would be read as a row with every value one column to the left.
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(|cell| flatten(cell)).collect())
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            std::iter::once(column.header)
                .chain(
                    cells
                        .iter()
                        .filter_map(|row| row.get(index).map(String::as_str)),
                )
                .map(display_width)
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut lines = Vec::with_capacity(cells.len() + 2);
    let headings: Vec<String> = columns
        .iter()
        .map(|column| column.header.to_owned())
        .collect();
    lines.push(row_line(&headings, columns, &widths));
    // The rule is what marks the block as a table for a reader, and it keeps the block recognisable
    // if the text is quoted onward into a message that also contains prose.
    lines.push(rule_line(&widths));
    for row in &cells {
        lines.push(row_line(row, columns, &widths));
    }
    lines.join("\n")
}

/// Builds one `| a | b |` line, padding each cell out to its column's display width.
fn row_line(cells: &[String], columns: &[Column], widths: &[usize]) -> String {
    let mut line = String::from("|");
    for (index, column) in columns.iter().enumerate() {
        let cell = cells.get(index).map_or("", String::as_str);
        let width = widths.get(index).copied().unwrap_or(0);
        // The count is in cells, not characters or bytes, because a wide character already occupies
        // two of them and padding it as one would shift every bar to its right.
        let padding = width.saturating_sub(display_width(cell));
        line.push(' ');
        match column.align {
            Align::Left => {
                line.push_str(cell);
                push_spaces(&mut line, padding);
            }
            Align::Right => {
                push_spaces(&mut line, padding);
                line.push_str(cell);
            }
        }
        line.push(' ');
        line.push('|');
    }
    line
}

/// Builds the `| --- | --- |` rule under the headings.
fn rule_line(widths: &[usize]) -> String {
    let mut line = String::from("|");
    for width in widths {
        line.push(' ');
        // One dash minimum: a zero-width column would otherwise render as `|  |`, which reads as a
        // blank cell rather than as a rule.
        for _ in 0..(*width).max(1) {
            line.push('-');
        }
        line.push(' ');
        line.push('|');
    }
    line
}

/// Appends `count` spaces.
fn push_spaces(line: &mut String, count: usize) {
    for _ in 0..count {
        line.push(' ');
    }
}

/// Whether a line is one of the notices the tools append rather than a path or a hit.
///
/// Recognizing notices here rather than at each call site keeps the rule in one place, and keeps a
/// tool that later changes its wording to a single edit. A parenthesized line is a trailer by
/// convention in this crate, which is why the parenthesis test is enough for the truncation notice.
fn is_notice(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("no files match")
        || trimmed.starts_with("no matches for")
        || (trimmed.starts_with('(') && trimmed.ends_with(')'))
}

/// Splits `path:line:text` at the leftmost boundary whose middle field is a decimal line number.
///
/// Returns `None` when no colon-separated field parses as a `u32`: a line without a recognisable
/// number is not a hit line, and guessing one would put a fabricated number in front of the model.
fn split_match_line(line: &str) -> Option<(&str, u32, &str)> {
    let bytes = line.as_bytes();
    let mut search = 0;
    while let Some(offset) = bytes.get(search..)?.iter().position(|byte| *byte == b':') {
        let colon = search + offset;
        // No second colon means no closing bound for the number, and no later candidate either.
        let tail = bytes.get(colon + 1..)?;
        let end = match tail.iter().position(|byte| *byte == b':') {
            Some(offset) => colon + 1 + offset,
            None => return None,
        };
        let digits = line.get(colon + 1..end)?;
        if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            // A number too large for the field is not a line number on any file, so the scan keeps
            // looking instead of failing the whole line.
            if let Ok(number) = digits.parse::<u32>() {
                return Some((line.get(..colon)?, number, line.get(end + 1..)?));
            }
        }
        search = colon + 1;
    }
    None
}

/// Replaces characters that would break the grid with a space.
///
/// A tab is one character in the string but eight cells on a terminal, so counting it as one makes
/// the padding wrong on that line only; a line break splits the row outright. Neither can be
/// represented faithfully inside a fixed-width cell, and a space is the only substitution that
/// keeps the cell where the reader expects it.
fn flatten(cell: &str) -> String {
    cell.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// The number of terminal cells a string occupies.
///
/// Deliberately a pure function of the string: consulting the locale or the terminal would make the
/// layout depend on where it was rendered, and the same listing would then align differently for
/// two readers of the same text.
fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// The number of terminal cells one character occupies.
fn char_width(ch: char) -> usize {
    if is_zero_width(ch) {
        0
    } else if is_wide(ch) {
        2
    } else {
        1
    }
}

/// Whether a character draws no cell of its own: combining marks, joiners and variation selectors.
///
/// Counting one of these as a cell would make the line one cell too wide and misalign every bar to
/// its right on that line only.
fn is_zero_width(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{200B}'..='\u{200F}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

/// Whether a character is double-width under East Asian Width.
///
/// The ranges are the wide and fullwidth blocks a path or a source line can contain: CJK
/// ideographs and punctuation, kana, Hangul, fullwidth forms, and the emoji blocks. A character
/// outside them is treated as one cell, which is the harmless direction to be wrong in — an
/// over-wide column costs a space, an under-wide one breaks the grid.
fn is_wide(ch: char) -> bool {
    matches!(
        ch,
        '\u{1100}'..='\u{115F}'
            | '\u{2E80}'..='\u{303E}'
            | '\u{3041}'..='\u{33FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{A000}'..='\u{A4CF}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{1F300}'..='\u{1F64F}'
            | '\u{1F900}'..='\u{1F9FF}'
            | '\u{1FA70}'..='\u{1FAFF}'
            | '\u{20000}'..='\u{2FFFD}'
            | '\u{30000}'..='\u{3FFFD}'
    )
}

/// Renders a modification time as the text a model reads.
///
/// A bare millisecond count is exact and unreadable. The canonical RFC 3339 form is what the rest of
/// the crate spells instants in, so a timestamp here compares directly with one in a log line. An
/// instant that form cannot name — a `u64` past the four-digit-year bound — falls back to the raw
/// count rather than being dropped: an odd-looking time is more useful than a missing one.
fn modified_cell(modified_ms: u64) -> String {
    i64::try_from(modified_ms)
        .ok()
        .and_then(|instant| girder_core::format_instant(instant).ok())
        .unwrap_or_else(|| modified_ms.to_string())
}
