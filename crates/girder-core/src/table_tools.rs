//! Structured tables for the tools that list files and search text.
//!
//! `glob`, `ls` and `grep` all report rows of the same three facts, and each of them today builds
//! that report by concatenating strings inline. The result is three output formats that only
//! coincidentally resemble each other, and no way for a caller to ask "was this a table?" without
//! re-parsing text.
//!
//! [`crate::tool_value`] already answers the second half — it can render a table and infer one from
//! text — but nothing in the crate ever produced a [`crate::StructuredValue::Table`]. This module is
//! the producer, kept in its own file so it can be adopted tool by tool: a tool that switches to
//! [`file_table`] gets a value the client can render, and a tool that does not loses nothing.
//!
//! # Why padding is measured in display cells
//!
//! A path such as `文档/说明.md` is fewer characters than it is columns wide: East Asian characters
//! occupy two terminal cells each. Padding by `chars().count()` therefore emits fewer spaces than the
//! cell needs, and every bar after it drifts by one for each wide character to its left. The drift is
//! invisible in the byte length of the line and obvious in the diagram, which is exactly the failure
//! mode the table exists to avoid — a model pairs a value with its neighbour's heading.
//!
//! [`render_table`] therefore measures cells in display cells and pads to that. For text that
//! contains no wide characters the two measures coincide, so its output stays byte-identical to
//! [`crate::render_tool_value`]; the two renderers diverge only where counting characters would have
//! been wrong.
//!
//! # Why the byte and line columns stay strings
//!
//! The value contract is a grid of strings. Keeping the numeric columns as decimal text instead of a
//! typed cell keeps [`crate::RenderedTable`] usable for every tool, and lets [`render_table`] pad a
//! number exactly like any other cell. The raw numbers remain on [`FileRow`] and [`MatchRow`], which
//! is where a caller that needs them arithmetically reads them.

use girder_core::{RenderedTable, StructuredValue};

/// One file's information, as `glob` and `ls` report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    /// The path as the tool reported it, relative to the workspace when the tool confined it so.
    pub path: String,
    /// Size in bytes. Kept exact rather than humanized: `1.5 KiB` is a reading, `1536` is a fact.
    pub bytes: u64,
    /// Modification time in milliseconds on the UTC timeline, the representation
    /// [`crate::TimeInstant`] uses elsewhere.
    pub modified_ms: u64,
}

/// One search hit, as `grep` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchRow {
    /// The file the match was found in.
    pub path: String,
    /// One-based line number, matching what an editor and the model's own `read` call report.
    pub line: u32,
    /// The matched line's text, without its trailing newline.
    pub text: String,
}

/// Renders file rows as a three-column table value.
///
/// The columns are `path`, `bytes` and `modified`, always in that order. An empty slice still
/// produces a table with its headings, so a consumer can tell "no files matched" from "the tool
/// returned nothing at all".
#[must_use]
pub fn file_table(rows: &[FileRow]) -> StructuredValue {
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                row.path.clone(),
                row.bytes.to_string(),
                modified_cell(row.modified_ms),
            ]
        })
        .collect();
    StructuredValue::Table(RenderedTable {
        headers: vec![
            String::from("path"),
            String::from("bytes"),
            String::from("modified"),
        ],
        rows: body,
    })
}

/// Renders search hits as a three-column table value.
///
/// The columns are `path`, `line` and `text`. A hit's text may contain a newline when the matcher
/// captured one; [`render_table`] replaces it with a space so the row cannot break in two.
#[must_use]
pub fn match_table(rows: &[MatchRow]) -> StructuredValue {
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|row| vec![row.path.clone(), row.line.to_string(), row.text.clone()])
        .collect();
    StructuredValue::Table(RenderedTable {
        headers: vec![
            String::from("path"),
            String::from("line"),
            String::from("text"),
        ],
        rows: body,
    })
}

/// Renders a structured value as the text a model reads.
///
/// A [`StructuredValue::Table`] is laid out with every column padded to its widest cell, measured in
/// display cells rather than characters, and a rule under the headings. Every other variant is
/// returned by [`crate::render_tool_value`] unchanged: this function exists to fix alignment, not to
/// re-interpret what a tool decided its text should look like.
#[must_use]
pub fn render_table(value: &StructuredValue) -> String {
    match value {
        StructuredValue::Table(table) => layout(table),
        other => girder_core::render_tool_value(other),
    }
}

/// Lays a table out with one padded column per heading.
///
/// The shape — headings, a rule, then rows, each cell wrapped in a space and a bar — is the same
/// shape [`crate::render_tool_value`] produces. Only the padding unit differs: display cells here,
/// characters there.
fn layout(table: &RenderedTable) -> String {
    // A cell that contains a line break would otherwise split one row into two, and the second half
    // would be read as a row of its own with every value one column to the left.
    let headers: Vec<String> = table.headers.iter().map(|cell| flatten(cell)).collect();
    let rows: Vec<Vec<String>> = table
        .rows
        .iter()
        .map(|row| row.iter().map(|cell| flatten(cell)).collect())
        .collect();
    if headers.is_empty() && rows.is_empty() {
        return String::new();
    }
    // A row wider than the headings widens the grid instead of being refused: the extra cell was
    // reported by the tool and dropping it would hide data. It renders without a heading, which is
    // visible rather than silent.
    let columns = rows
        .iter()
        .map(Vec::len)
        .chain(std::iter::once(headers.len()))
        .max()
        .unwrap_or(0);
    let widths = column_widths(&headers, &rows, columns);
    let mut lines = Vec::with_capacity(rows.len() + 2);
    if !headers.is_empty() {
        lines.push(row_line(&headers, &widths, columns));
        // The rule is what separates a heading from data for a reader, and it keeps the block
        // recognisable as a table if the text is quoted onward.
        lines.push(rule_line(&widths));
    }
    for row in &rows {
        lines.push(row_line(row, &widths, columns));
    }
    lines.join("\n")
}

/// Measures every column in display cells, taking the widest of its heading and its cells.
fn column_widths(headers: &[String], rows: &[Vec<String>], columns: usize) -> Vec<usize> {
    let mut widths = vec![0_usize; columns];
    for row in std::iter::once(headers).chain(rows.iter().map(Vec::as_slice)) {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(display_width(cell));
            }
        }
    }
    widths
}

/// Builds one `| a | b |` line, padding each cell out to its column's display width.
fn row_line(cells: &[String], widths: &[usize], columns: usize) -> String {
    let mut line = String::new();
    line.push('|');
    for index in 0..columns {
        let cell = cells.get(index).map_or("", String::as_str);
        let width = widths.get(index).copied().unwrap_or(0);
        line.push(' ');
        line.push_str(cell);
        // Padding on the right is what puts the next bar in the same place on every line. The count
        // is in cells, not characters, because a wide character already occupies two of them.
        let padding = width.saturating_sub(display_width(cell));
        for _ in 0..padding {
            line.push(' ');
        }
        line.push(' ');
        line.push('|');
    }
    line
}

/// Builds the `| --- | --- |` rule under the headings.
fn rule_line(widths: &[usize]) -> String {
    let mut line = String::new();
    line.push('|');
    for width in widths {
        line.push(' ');
        // One dash minimum: an empty column would otherwise render as `|  |`, which reads as a blank
        // cell rather than as a rule.
        for _ in 0..(*width).max(1) {
            line.push('-');
        }
        line.push(' ');
        line.push('|');
    }
    line
}

/// Replaces embedded line breaks with spaces so a cell cannot break its row in two.
fn flatten(cell: &str) -> String {
    cell.chars()
        .map(|ch| if matches!(ch, '\n' | '\r') { ' ' } else { ch })
        .collect()
}

/// The number of terminal cells a string occupies.
///
/// This is the East Asian Width rule a terminal applies, restricted to the classes a path or a
/// source line can plausibly contain. It is deliberately a pure function of the string: consulting
/// the locale or the terminal would make the layout depend on where it was rendered.
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
/// Counting these as one would pad a cell that is already the right width, which shifts nothing but
/// makes the line one cell too wide and misaligns every bar after it on that line only.
fn is_zero_width(ch: char) -> bool {
    matches!(ch,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{200B}'..='\u{200F}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{E0100}'..='\u{E01EF}')
}

/// Whether a character is double-width under East Asian Width.
///
/// The ranges are the wide and fullwidth ones a path, a commit message or a search hit can contain:
/// CJK ideographs and punctuation, kana, Hangul, fullwidth forms, and the emoji blocks. Everything
/// outside them is one cell, which is the harmless direction to be wrong in — an over-wide column
/// wastes a space, an under-wide one breaks the grid.
fn is_wide(ch: char) -> bool {
    matches!(ch,
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
            | '\u{30000}'..='\u{3FFFD}')
}

/// Renders a modification time as the text a model reads.
///
/// A bare millisecond count is exact and unreadable. The canonical RFC 3339 form is what the rest of
/// the crate already spells instants in, so a timestamp here compares with one in a log line. An
/// instant that form cannot name — a `u64` past the four-digit-year bound — falls back to the raw
/// count rather than being dropped: an odd-looking time is more useful than a missing one.
fn modified_cell(modified_ms: u64) -> String {
    i64::try_from(modified_ms)
        .ok()
        .and_then(|instant| girder_core::format_instant(instant).ok())
        .unwrap_or_else(|| modified_ms.to_string())
}
