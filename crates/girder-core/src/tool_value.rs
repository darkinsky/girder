//! A tool result as a structured value, and the text a model actually reads.
//!
//! [`crate::tools::ToolResult`] already carries an optional `value`, but its only shape is
//! [`crate::tools::ToolValue`] — text or opaque JSON. Two things follow from that. A tool with a
//! table to report has no way to say so, so it formats the table into a string by hand, and every
//! tool that does so invents its own column widths. And a client reading a result has no way to ask
//! "is this a table?" without re-parsing the text it was given.
//!
//! This module is a parallel type rather than a change to `ToolResult`, deliberately. Adding a
//! variant to `ToolValue` would touch every construction site and every serializer at once, and the
//! question this module answers — what *is* this output, and how should it be rendered — is still
//! being settled. A parallel type can be adopted where it helps and ignored where it does not.
//!
//! # Why the rendered text is the contract
//!
//! A model never sees [`Value`]. It sees [`render`]'s output, which is why the table arm does real
//! work: a model cannot read a table structure, but it can read aligned columns, and misaligned
//! columns make it associate a value with the wrong header. The renderer therefore pads every cell
//! to its column width instead of joining with a separator and hoping.
//!
//! [`render`] leaves [`Value::Text`] untouched — not trimmed, not re-wrapped, not re-indented. A
//! tool that returns text has already decided how it should look, and a renderer that "tidies" it
//! corrupts diffs, code, and anything else where leading whitespace is data.
//!
//! # Why [`infer`] is conservative
//!
//! [`infer`] guesses a structure from text that a tool emitted as a plain string. A wrong guess is
//! worse than no guess: the client trusts the structure it is handed and stops looking at the text,
//! so a false table silently mis-files every value in it. It therefore returns `Some` only for the
//! two shapes that are unambiguous — a JSON object or array that parses, and a pipe table whose
//! every row has the same width — and `None` for everything else, including prose, JSON scalars,
//! space-aligned columns, and text that merely starts like JSON.

/// A tool's result, in the shape the tool meant rather than the shape it had to serialize.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Text, rendered exactly as given.
    Text(String),
    /// JSON, rendered as a compact serialization.
    Json(serde_json::Value),
    /// A table, rendered with aligned columns.
    Table(Table),
}

/// A table a tool wants the model to read down the columns of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    /// Column names, in display order.
    pub headers: Vec<String>,
    /// Rows, in the order the tool produced them.
    ///
    /// A row may be shorter than `headers`; [`render`] pads it with empty cells rather than
    /// refusing, because a tool that knows a trailing column is empty should not have to spell out
    /// the padding. A row wider than `headers` widens the grid, and the extra cells are rendered
    /// without a header, which is visible in the output rather than silently dropped.
    pub rows: Vec<Vec<String>>,
}

/// Renders a value as the text a model reads.
///
/// [`Value::Text`] is returned byte for byte, including leading whitespace and a trailing newline.
/// [`Value::Json`] is serialized compactly, matching what
/// [`crate::tools::ToolValue::to_content`] already produces. [`Value::Table`] is laid out with one
/// padded column per heading and a header rule.
#[must_use]
pub fn render(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        // `Display` for a JSON value is its compact serialization and cannot fail, so a caller
        // never has to decide what to show instead.
        Value::Json(json) => json.to_string(),
        Value::Table(table) => render_table(table),
    }
}

/// Infers a structure from a tool's text output.
///
/// Returns `Some` only when the shape is unambiguous:
///
/// - `Some(Value::Json)` for text that parses as a JSON **object or array**. The top level must be a
///   container: `42`, `true`, `null` and `"a string"` are all valid JSON and also all plausible
///   ordinary output, so inferring them would turn text into data at the client.
/// - `Some(Value::Table)` for pipe tables: at least a header row and one data row, every line
///   starting and ending with `|`, every row with the same number of at least two columns, a
///   non-empty heading in each column, no heading that is itself a rule, and an optional
///   `---`/`:---:` rule on the second line.
///
/// Returns `None` for everything else, including empty or whitespace-only text, prose that contains
/// a pipe, space-aligned columns, ragged pipe tables, and text that begins like JSON but does not
/// parse. Guessing on any of those would be a structure the client trusts and the text does not
/// support.
#[must_use]
pub fn infer(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first = trimmed.chars().next()?;
    if matches!(first, '{' | '[') {
        // Either this parses and is returned as JSON, or it is malformed and is refused. It is not
        // considered as a table: a line that starts with a brace is not a pipe row, and a partial
        // JSON document is exactly the case where a guess would look authoritative and be wrong.
        let json: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        return if json.is_object() || json.is_array() {
            Some(Value::Json(json))
        } else {
            None
        };
    }
    as_table(trimmed).map(Value::Table)
}

/// Renders a table with every column padded to a single width.
fn render_table(table: &Table) -> String {
    if table.headers.is_empty() && table.rows.is_empty() {
        return String::new();
    }
    let headers: Vec<String> = table.headers.iter().map(|cell| flatten(cell)).collect();
    let rows: Vec<Vec<String>> = table
        .rows
        .iter()
        .map(|row| row.iter().map(|cell| flatten(cell)).collect())
        .collect();
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
        // Markdown's rule row: it is what separates a heading from data for a reader, and it is
        // also what makes the block recognisable as a table if the text is quoted onward.
        lines.push(rule_line(&widths));
    }
    for row in &rows {
        lines.push(row_line(row, &widths, columns));
    }
    lines.join("\n")
}

/// Measures every column, taking the widest of its heading and its cells.
fn column_widths(headers: &[String], rows: &[Vec<String>], columns: usize) -> Vec<usize> {
    let mut widths = vec![0_usize; columns];
    for row in std::iter::once(headers).chain(rows.iter().map(Vec::as_slice)) {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    widths
}

/// Builds one `| a | b |` line, padding each cell out to its column width.
fn row_line(cells: &[String], widths: &[usize], columns: usize) -> String {
    let mut line = String::new();
    line.push('|');
    for index in 0..columns {
        let cell = cells.get(index).map_or("", String::as_str);
        let width = widths.get(index).copied().unwrap_or(0);
        line.push(' ');
        line.push_str(cell);
        // Padding on the right is what puts the next `|` in the same place on every line. Without
        // it the bars drift and a model pairs a value with its neighbour's heading.
        for _ in cell.chars().count()..width {
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
        // One dash minimum: an empty column would otherwise render as `|  |`, which reads as a
        // blank cell rather than as a rule.
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

/// Parses a pipe table, or reports that the text is not one.
fn as_table(text: &str) -> Option<Table> {
    // Blank lines are dropped rather than treated as a broken row: tools insert them liberally, and
    // an empty line carries no cells to compare against the header.
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    // A heading and at least one row. A single pipe-delimited line is prose far more often than it
    // is a table, and there is no row underneath to confirm the shape.
    if lines.len() < 2 {
        return None;
    }
    let headers = split_row(lines[0])?;
    // An unnamed column cannot be trusted to line up with anything: the reader would have to count
    // columns, which is the failure this module exists to prevent.
    if headers.iter().any(String::is_empty) {
        return None;
    }
    // A first row that is nothing but rule cells is a separator that lost its heading, not a set of
    // columns. Keeping it would put `---` at the top of every column in the inferred structure.
    if is_rule(&headers) {
        return None;
    }
    let mut body = &lines[1..];
    if let Some(candidate) = body.first() {
        let cells = split_row(candidate)?;
        if cells.len() != headers.len() {
            return None;
        }
        if is_rule(&cells) {
            body = &body[1..];
        }
    }
    if body.is_empty() {
        return None;
    }
    let mut rows = Vec::with_capacity(body.len());
    for line in body {
        let cells = split_row(line)?;
        // Ragged rows are refused rather than padded. A tool that emitted a short row may have
        // omitted its last cell or its middle one, and padding guesses which; the text does not say.
        if cells.len() != headers.len() {
            return None;
        }
        rows.push(cells);
    }
    Some(Table { headers, rows })
}

/// Splits one `| a | b |` line into trimmed cells, or reports that it is not such a line.
fn split_row(line: &str) -> Option<Vec<String>> {
    // Both bars are required. `a | b` is ordinary prose about a shell pipe far more often than it is
    // a table row, so the leading and trailing bar is what makes the shape explicit.
    let inner = line.strip_prefix('|')?.strip_suffix('|')?;
    let cells: Vec<String> = inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect();
    // One column is not a table; it is a list that happened to be fenced.
    if cells.len() < 2 {
        return None;
    }
    Some(cells)
}

/// Reports whether every cell is a Markdown rule cell such as `---` or `:---:`.
fn is_rule(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells
            .iter()
            .all(|cell| !cell.is_empty() && cell.chars().all(|ch| matches!(ch, '-' | ':')))
}
