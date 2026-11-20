//! Tests for the CSV export of session and tool statistics.
//!
//! Three properties carry the weight here, and each has a test named after it rather than a comment promising it:
//!
//! - `comma_field_is_quoted`, `quote_is_doubled` and `newline_field_is_quoted` pin the RFC 4180 rules. They are the
//!   ones a hand-rolled writer gets subtly wrong — a doubled quote that is not doubled corrupts a value, and a
//!   comma that is not quoted silently shifts every later column of that row.
//! - `formula_injection_is_neutralised` pins the security rule. A CSV opened in Excel executes a cell that starts
//!   with `=`, `+`, `-` or `@`, and these tables carry text we do not control, so the test plants each of the four
//!   prefixes and asserts on the exact rendered byte string rather than on a "contains" check.
//! - `records_end_in_crlf` pins the line terminator, because the difference between `LF` and `CRLF` is invisible in
//!   a terminal and is discovered by the spreadsheet that mis-parses the file.
//!
//! # Why the module is compiled by path
//!
//! `stats_csv` is not yet exported from the crate root — that wiring is a separate change to `lib.rs`. Building the
//! exact source file here, rather than a copy of it, keeps these tests exercising the code a consumer will reach
//! through `girder_core::telemetry::stats_csv`. `ToolStats` *is* exported, so it is imported from the crate instead of being
//! re-declared: a test that invented its own struct would not compile against the real one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/telemetry/stats_csv.rs"]
pub mod stats_csv;

use girder_core::ToolStats;
use stats_csv::{Table, escape_field, session_table, tool_table};

/// A table of one column with the values given, for tests that only care about escaping.
fn one_column(values: &[&str]) -> Table {
    let mut table = Table::new(vec![String::from("value")]);
    for value in values {
        table.push(vec![(*value).to_string()]);
    }
    table
}

#[test]
fn a_plain_table_renders_header_then_rows() {
    let mut table = Table::new(vec![String::from("a"), String::from("b")]);
    table.push(vec![String::from("1"), String::from("2")]);
    table.push(vec![String::from("3"), String::from("4")]);

    assert_eq!(table.render(), "a,b\r\n1,2\r\n3,4\r\n");
}

#[test]
fn comma_field_is_quoted() {
    // The core case: an unquoted comma would split one field into two and shift the rest of the row.
    assert_eq!(escape_field("a,b"), "\"a,b\"");
    assert_eq!(
        one_column(&["left,right"]).render(),
        "value\r\n\"left,right\"\r\n"
    );
}

#[test]
fn quote_is_doubled() {
    // An inner quote must become `""`, and the field must be quoted so the pair is unambiguous.
    assert_eq!(escape_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    assert_eq!(escape_field("\""), "\"\"\"\"");
    // Two adjacent quotes become four: the doubling is per character, not a replace of a pair.
    assert_eq!(escape_field("\"\""), "\"\"\"\"\"\"");
}

#[test]
fn newline_and_carriage_return_fields_are_quoted() {
    // A newline inside a field is legal when quoted; unquoted it would end the record.
    assert_eq!(escape_field("a\nb"), "\"a\nb\"");
    assert_eq!(escape_field("a\rb"), "\"a\rb\"");
    assert_eq!(escape_field("a\r\nb"), "\"a\r\nb\"");
}

#[test]
fn formula_injection_is_neutralised() {
    // Every spreadsheet formula prefix is prefixed with a text marker, so the cell renders as text.
    assert_eq!(escape_field("=1+1"), "'=1+1");
    assert_eq!(escape_field("+1"), "'+1");
    assert_eq!(escape_field("-1"), "'-1");
    assert_eq!(escape_field("@SUM(A1)"), "'@SUM(A1)");

    // The DDE payloads that make this a real attack, not a curiosity.
    assert_eq!(escape_field("=cmd|'/c calc'!A0"), "'=cmd|'/c calc'!A0");
    assert_eq!(
        escape_field("=HYPERLINK(\"http://evil.example/\",\"click\")"),
        "\"'=HYPERLINK(\"\"http://evil.example/\"\",\"\"click\"\")\""
    );

    // A leading space or tab is stripped by a spreadsheet before it decides, so the marker goes in front of the
    // whole field. The marker lands first, so the field no longer starts with whitespace and needs no quoting —
    // which is why the space survives inside the text run rather than being trimmed.
    assert_eq!(escape_field(" =1+1"), "' =1+1");
    assert_eq!(escape_field("\t=1+1"), "'\t=1+1");

    // Neutralising is not applied to a prefix that is not at the head of the field: `a=b` is text.
    assert_eq!(escape_field("a=b"), "a=b");
    assert_eq!(escape_field("1+1"), "1+1");
}

#[test]
fn empty_fields_render_as_nothing_between_separators() {
    assert_eq!(escape_field(""), "");
    let mut table = Table::new(vec![String::from("a"), String::from("b")]);
    table.push(vec![String::new(), String::from("x")]);
    // An empty field is empty, not `""`: quoting nothing is legal but noisy, and no reader needs it.
    assert_eq!(table.render(), "a,b\r\n,x\r\n");
}

#[test]
fn unicode_passes_through_unchanged() {
    assert_eq!(escape_field("工具"), "工具");
    assert_eq!(escape_field("é,ü"), "\"é,ü\"");
    // Escaping is per `char`, so a multi-byte character is never split and a quote next to one still doubles.
    assert_eq!(escape_field("工具,\"注\""), "\"工具,\"\"注\"\"\"");
    assert_eq!(one_column(&["会话"]).render(), "value\r\n会话\r\n");
}

#[test]
fn headers_are_escaped_too() {
    // A header is data like any other: a comma in one would shift every column of every row under it.
    let mut table = Table::new(vec![String::from("a,b"), String::from("=x")]);
    table.push(vec![String::from("1"), String::from("2")]);
    assert_eq!(table.render(), "\"a,b\",'=x\r\n1,2\r\n");
}

#[test]
fn records_end_in_crlf() {
    // RFC 4180 asks for CRLF; checked on the exact bytes so an LF-only writer cannot pass.
    let rendered = one_column(&["x", "y"]).render();
    assert_eq!(rendered, "value\r\nx\r\ny\r\n");
    assert!(!rendered.contains("\ny") || rendered.contains("\r\ny"));
    assert_eq!(rendered.matches("\r\n").count(), 3);
}

#[test]
fn empty_table_renders_nothing() {
    // No columns and no rows is no file, not a blank line.
    let empty = Table::new(Vec::new());
    assert_eq!(empty.render(), "");

    // Headers with no rows still renders the header record, terminated like any other.
    let headers_only = Table::new(vec![String::from("a")]);
    assert_eq!(headers_only.render(), "a\r\n");
}

#[test]
fn spaces_at_the_edges_are_quoted() {
    // RFC 4180 counts spaces as part of the field; quoting is what stops a reader from trimming them.
    assert_eq!(escape_field(" x"), "\" x\"");
    assert_eq!(escape_field("x "), "\"x \"");
    assert_eq!(escape_field(" x "), "\" x \"");
    assert_eq!(escape_field("x y"), "x y");
}

#[test]
fn session_table_has_the_documented_columns_and_numbers() {
    let table = session_table(vec![
        (String::from("s1"), 3, 7, 100),
        (String::from("s2"), 0, 0, 0),
    ]);

    assert_eq!(
        table.headers,
        vec![
            String::from("session_id"),
            String::from("turns"),
            String::from("tool_calls"),
            String::from("tokens"),
        ]
    );
    assert_eq!(
        table.render(),
        "session_id,turns,tool_calls,tokens\r\ns1,3,7,100\r\ns2,0,0,0\r\n"
    );
}

#[test]
fn session_table_neutralises_a_hostile_id() {
    // The id comes from a session title, which a model wrote; it is untrusted like every other field.
    let table = session_table(vec![(String::from("=1+1"), 1, 1, 1)]);
    assert_eq!(
        table.render(),
        "session_id,turns,tool_calls,tokens\r\n'=1+1,1,1,1\r\n"
    );
}

#[test]
fn tool_table_has_the_documented_columns_and_rates() {
    let stats = vec![
        ToolStats {
            name: String::from("bash"),
            calls: 4,
            failures: 1,
            argument_bytes: 40,
            result_bytes: 400,
        },
        // Zero calls: the rate is `0.0000` rather than a division by zero, which is `ToolStats`' own decision.
        ToolStats {
            name: String::from("never"),
            calls: 0,
            failures: 0,
            argument_bytes: 0,
            result_bytes: 0,
        },
    ];

    let table = tool_table(&stats);
    assert_eq!(
        table.headers,
        vec![
            String::from("tool"),
            String::from("calls"),
            String::from("failures"),
            String::from("failure_rate"),
            String::from("argument_bytes"),
            String::from("result_bytes"),
        ]
    );
    assert_eq!(
        table.render(),
        "tool,calls,failures,failure_rate,argument_bytes,result_bytes\r\n\
         bash,4,1,0.2500,40,400\r\n\
         never,0,0,0.0000,0,0\r\n"
    );
}

#[test]
fn tool_table_escapes_a_hostile_tool_name() {
    let stats = vec![ToolStats {
        name: String::from("@evil,inc"),
        calls: 1,
        failures: 0,
        argument_bytes: 1,
        result_bytes: 2,
    }];

    // The marker goes on first and the quoting wraps the whole thing, so the comma stays inside one field.
    assert_eq!(
        tool_table(&stats).render(),
        "tool,calls,failures,failure_rate,argument_bytes,result_bytes\r\n\
         \"'@evil,inc\",1,0,0.0000,1,2\r\n"
    );
}

#[test]
fn a_ragged_row_is_rendered_as_given() {
    // Neither padded nor truncated: inventing cells would report values nobody supplied, and dropping them would
    // lose real ones.
    let mut table = Table::new(vec![String::from("a"), String::from("b")]);
    table.push(vec![String::from("1")]);
    table.push(vec![
        String::from("1"),
        String::from("2"),
        String::from("3"),
    ]);
    assert_eq!(table.render(), "a,b\r\n1\r\n1,2,3\r\n");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod telemetry {
    pub use super::stats_csv as stats_csv;
}
