//! CSV export for the statistics modules: the shape an operator opens in a spreadsheet.
//!
//! `/api` already answers "how big was this conversation" twice, and neither answer is a file. [`crate::to_prometheus`]
//! renders one line per sample for a scraper: no column schema, no rows, and nothing a person pivots by hand.
//! [`crate::core::tool_usage`] returns `Vec<ToolStats>` for a program to consume. Operations asked a third question — "put
//! a week of these in Excel and sort by failure rate" — and a scraper endpoint is not that, so the counts have to be
//! laid out as a table first.
//!
//! # Why the escaper is written here and not pulled in
//!
//! The crate's dependency list is short on purpose, and a CSV writer is two rules: quote what has to be quoted, and
//! refuse to hand a spreadsheet something it will execute. Both fit in a screen. The risk in a CSV writer is not its
//! size, it is one edge case being wrong in a way that silently corrupts a column, which is why every rule below has
//! a test that pins it rather than a comment that promises it.
//!
//! # Why CRLF and not LF
//!
//! RFC 4180 mandates `CRLF` as the record separator, Excel writes it on every platform, and the code that reads a CSV
//! file is usually not the code that wrote it. A bare `LF` is accepted by many readers, but "many" is not "all", and
//! the extra byte costs nothing on a file that is opened once and read by hand.
//!
//! # Why a leading apostrophe, and why it is not optional
//!
//! Excel, `LibreOffice` Calc and Google Sheets all evaluate a cell that begins with `=`, `+`, `-` or `@` as a formula
//! when a CSV is opened. That turns a data field into code: `=HYPERLINK(...)` exfiltrates a cell, `=cmd|...` is the
//! classic DDE payload on Windows, and `=1+1` at best corrupts the column it lands in. The values in these tables do
//! not come from us — a session id, a tool name, and a model's own output end up in them — so every field is
//! untrusted by construction. Prefixing with `'` is what a spreadsheet understands as "the rest of this cell is
//! text", and it hides the marker on display; readers that are not spreadsheets see the literal `'`, which is a
//! visible question mark on a field that was already odd, and a much better outcome than code running when the file
//! is double-clicked. There is no exemption for values that look like numbers (`-5` is neutralised too): a rule with
//! exceptions is a rule an attacker probes, and a count this module renders itself never reaches that path.
//!
//! Leading spaces and tabs are skipped when looking for that first character, because a spreadsheet strips them
//! before deciding whether the cell is a formula — ` =1+1` fires exactly like `=1+1`. A value whose *first* character
//! is `CR` or `LF` is not neutralised: such a field is always quoted, so it is one multi-line text cell and never a
//! record boundary with a formula at the head of the next one.

use std::borrow::Cow;

/// The record separator, mandated by RFC 4180.
const RECORD_SEPARATOR: &str = "\r\n";

/// Characters a spreadsheet reads as the head of a formula.
const FORMULA_PREFIXES: [char; 4] = ['=', '+', '-', '@'];

/// Prepended to a field that would otherwise be evaluated as a formula.
///
/// A spreadsheet reads it as a text marker and does not display it; any other reader sees an extra character, which
/// is the acceptable half of that trade.
const TEXT_MARKER: char = '\'';

/// One CSV table: a header record and the rows under it.
///
/// Header and rows are kept apart rather than folded into one `Vec<Vec<String>>` because the header is a different
/// kind of thing: it is written once, its width is the contract for every row, and a caller that inserts a row must
/// not be able to replace it by accident.
pub struct Table {
    /// Column names, in output order.
    pub headers: Vec<String>,
    /// Rows, in insertion order.
    ///
    /// Not sorted and not de-duplicated: the statistics modules already return their rows in a defined order, and a
    /// second sort here would either duplicate that decision or silently override it.
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// A table with `headers` and no rows.
    #[must_use]
    pub fn new(headers: Vec<String>) -> Self {
        Self {
            headers,
            rows: Vec::new(),
        }
    }

    /// Appends one row.
    ///
    /// The row is rendered exactly as given: a row shorter than the header is not padded and a longer one is not
    /// truncated. Padding would invent empty values the caller never supplied, and truncation would drop the ones it
    /// did; a visibly ragged record is a bug in the caller, and it stays visible.
    pub fn push(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    /// Renders the table as CSV, header first, every record ending in `CRLF`.
    ///
    /// An empty header list renders no header record at all rather than a blank line: a blank line would be a record
    /// holding one empty field, which is a different (and wrong) answer to "this table has no columns". The result
    /// for a table with neither headers nor rows is therefore the empty string, which is the only faithful rendering
    /// of nothing.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.headers.is_empty() {
            push_record(&mut out, &self.headers);
        }
        for row in &self.rows {
            push_record(&mut out, row);
        }
        out
    }
}

/// Escapes one CSV field.
///
/// RFC 4180: a field containing `,`, `"`, `LF` or `CR` is wrapped in double quotes, and an inner `"` becomes `""`.
/// Fields with leading or trailing spaces or tabs are quoted as well, because the RFC makes those characters part of
/// the field and some readers trim an unquoted one — quoting is what keeps the value the value.
///
/// A field that a spreadsheet would evaluate as a formula gets a leading `'` before any quoting happens, so the
/// marker sits against the formula and not against a quote character. See the module documentation for why that is
/// not restricted to "obviously dangerous" fields.
///
/// *Not* escaped: everything else. The field is emitted as UTF-8 text, so the caller owns the encoding decision;
/// this function never rejects input, which is why it returns a `String` rather than a `Result`.
#[must_use]
pub fn escape_field(value: &str) -> String {
    let neutralised = neutralise(value);
    if !needs_quoting(&neutralised) {
        return neutralised.into_owned();
    }
    let mut out = String::with_capacity(neutralised.len() + 2);
    out.push('"');
    for ch in neutralised.chars() {
        // Doubling, not backslash-escaping: RFC 4180 has no escape character, so `\"` would terminate the quoted
        // field at the backslash and leave the rest of the value as a new record.
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Renders one session's counts as a table.
///
/// Each tuple is `(session id, turns, tool calls, tokens)`, matching the fields of [`crate::SessionStats`] that an
/// operator sorts a report by. The id is a `String` because it is what the session was named, and it is escaped like
/// any other text — a model-chosen title is untrusted input, not a key we control.
///
/// The rows keep the order they are given in. Callers that want "busiest first" sort before calling; this function
/// has no clock and no notion of a session set, so it cannot know which order is the useful one.
#[must_use]
#[allow(
    clippy::needless_pass_by_value,
    reason = "the tuples own their strings and are moved into the table; a slice would force a clone per id"
)]
pub fn session_table(rows: Vec<(String, u64, u64, u64)>) -> Table {
    let mut table = Table::new(vec![
        String::from("session_id"),
        String::from("turns"),
        String::from("tool_calls"),
        String::from("tokens"),
    ]);
    for (id, turns, tool_calls, tokens) in rows {
        table.push(vec![
            id,
            turns.to_string(),
            tool_calls.to_string(),
            tokens.to_string(),
        ]);
    }
    table
}

/// Renders per-tool usage as a table.
///
/// Columns are the tool name and the numbers [`girder_core::ToolStats`] carries, with `failure_rate` as a ratio
/// rounded to four places. The ratio is folded here rather than left out because "which tool fails most often" is
/// the question the table exists to answer, and doing that arithmetic on a spreadsheet formula would require the
/// reader to notice that a tool with zero calls must not divide by zero — [`girder_core::ToolStats::failure_rate`]
/// already decided that, and returns `0.0`. The rounded ratio is a convenience only: the raw `calls` and `failures`
/// columns are next to it, so nothing in the report depends on the fourth decimal place.
///
/// Order is the slice's order. [`crate::core::tool_usage`] returns most-called first, which is the order a reader wants,
/// and sorting again here would silently disagree with it.
#[must_use]
pub fn tool_table(stats: &[girder_core::ToolStats]) -> Table {
    let mut table = Table::new(vec![
        String::from("tool"),
        String::from("calls"),
        String::from("failures"),
        String::from("failure_rate"),
        String::from("argument_bytes"),
        String::from("result_bytes"),
    ]);
    for stat in stats {
        let rate = stat.failure_rate();
        table.push(vec![
            stat.name.clone(),
            stat.calls.to_string(),
            stat.failures.to_string(),
            format!("{rate:.4}"),
            stat.argument_bytes.to_string(),
            stat.result_bytes.to_string(),
        ]);
    }
    table
}

/// Writes one record, escaping every cell and terminating it with `CRLF`.
fn push_record(out: &mut String, cells: &[String]) {
    for (index, cell) in cells.iter().enumerate() {
        // The separator goes between cells, never after the last, so a record with one cell has no comma and a
        // record with an empty cell list is just the terminator.
        if index > 0 {
            out.push(',');
        }
        out.push_str(&escape_field(cell));
    }
    out.push_str(RECORD_SEPARATOR);
}

/// Prepends [`TEXT_MARKER`] when a spreadsheet would read `value` as a formula.
///
/// Borrows when there is nothing to do, because most fields in a table are ordinary text and copying each one just
/// to hand it back unchanged is wasted work in the common case.
fn neutralise(value: &str) -> Cow<'_, str> {
    if !starts_like_formula(value) {
        return Cow::Borrowed(value);
    }
    let mut marked = String::with_capacity(value.len() + 1);
    marked.push(TEXT_MARKER);
    marked.push_str(value);
    Cow::Owned(marked)
}

/// Whether a spreadsheet would evaluate `value` as a formula.
fn starts_like_formula(value: &str) -> bool {
    // Spaces and tabs are skipped because a spreadsheet strips them before deciding, so ` =1+1` is as dangerous as
    // `=1+1`. `CR` and `LF` are *not* in this set: a field containing them is quoted, which makes it one text cell.
    let first = value.chars().find(|ch| !matches!(ch, ' ' | '\t'));
    first.is_some_and(|ch| FORMULA_PREFIXES.contains(&ch))
}

/// Whether a field must be wrapped in quotes.
fn needs_quoting(value: &str) -> bool {
    let structural = value
        .chars()
        .any(|ch| matches!(ch, ',' | '"' | '\n' | '\r'));
    // A leading or trailing space is quoted so that a reader which trims unquoted whitespace still hands back the
    // exact value the caller stored; RFC 4180 says spaces belong to the field, and quoting is what enforces it.
    structural || value.starts_with([' ', '\t']) || value.ends_with([' ', '\t'])
}
