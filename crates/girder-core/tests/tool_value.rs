//! Tests for the structured tool value: rendering, and the limits of inference.
//!
//! Two things are load-bearing here and neither is the happy path. The first is that the table
//! renderer actually aligns columns: a model reads the diagram, not the structure, so a drifted bar
//! is a value filed under the wrong heading. The second is that [`infer`] stays quiet. Most of the
//! cases below feed it ordinary output and assert `None`, because a wrong inference is trusted by
//! the client and is therefore worse than the plain text it replaced.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the renderer as written even before the crate root wires it in. That is also why nothing
// here reaches for the rest of `girder_core`.
#[path = "../src/tool_value.rs"]
mod tool_value;

use serde_json::json;
use tool_value::{Table, Value, infer, render};

/// Builds a table from literals, so the assertions below read as the table they describe.
fn table(headers: &[&str], rows: &[&[&str]]) -> Table {
    Table {
        headers: headers.iter().map(|cell| (*cell).to_string()).collect(),
        rows: rows
            .iter()
            .map(|row| row.iter().map(|cell| (*cell).to_string()).collect())
            .collect(),
    }
}

// ---- render: text and JSON ----------------------------------------------------------------------

#[test]
fn text_is_rendered_byte_for_byte() {
    // Leading spaces, a trailing newline and an internal tab are all data here. A renderer that
    // trimmed or re-indented would corrupt a diff or a code excerpt on its way to the model.
    let text = "  fn main() {\n\tprintln!(\"hi\");\n}\n";
    assert_eq!(render(&Value::Text(text.to_string())), text);
}

#[test]
fn empty_text_renders_as_empty() {
    assert_eq!(render(&Value::Text(String::new())), "");
}

#[test]
fn json_is_rendered_compactly() {
    let value = json!({ "ok": true, "count": 2 });
    assert_eq!(render(&Value::Json(value)), r#"{"count":2,"ok":true}"#);
}

#[test]
fn json_null_renders_as_its_literal() {
    assert_eq!(render(&Value::Json(json!(null))), "null");
}

#[test]
fn json_array_is_rendered_as_json() {
    assert_eq!(render(&Value::Json(json!([1, "two"]))), r#"[1,"two"]"#);
}

// ---- render: the table --------------------------------------------------------------------------

#[test]
fn table_columns_are_padded_to_the_widest_cell() {
    let value = Value::Table(table(
        &["name", "size"],
        &[&["a.rs", "12"], &["longer.rs", "3"]],
    ));
    // `longer.rs` is nine wide, so both `name` and `a.rs` are padded to nine. `size` is four, which
    // is exactly its heading, so the data cells are padded instead.
    let expected = [
        "| name      | size |",
        "| --------- | ---- |",
        "| a.rs      | 12   |",
        "| longer.rs | 3    |",
    ]
    .join("\n");
    assert_eq!(render(&value), expected);
}

#[test]
fn every_rendered_table_line_is_the_same_width() {
    // The alignment property, stated independently of any one expected string: if the widths ever
    // diverge, a model reading a column will pair a value with its neighbour's heading.
    let rendered = render(&Value::Table(table(
        &["id", "description"],
        &[
            &["1", "short"],
            &["22", "a considerably longer description"],
            &["333", ""],
        ],
    )));
    let widths: Vec<usize> = rendered.lines().map(str::len).collect();
    assert!(widths.len() >= 2, "expected several lines");
    assert!(
        widths.iter().all(|width| *width == widths[0]),
        "lines are not aligned: {widths:?}\n{rendered}"
    );
}

#[test]
fn table_with_many_rows_keeps_a_single_rule() {
    let rendered = render(&Value::Table(table(
        &["a", "b"],
        &[&["1", "2"], &["3", "4"], &["5", "6"]],
    )));
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(
        lines.len(),
        5,
        "one header, one rule, three rows: {rendered}"
    );
    assert_eq!(lines[0], "| a | b |");
    assert_eq!(lines[1], "| - | - |");
    assert_eq!(lines[2], "| 1 | 2 |");
    assert_eq!(lines[3], "| 3 | 4 |");
    assert_eq!(lines[4], "| 5 | 6 |");
}

#[test]
fn a_short_row_is_padded_rather_than_refused() {
    // A tool that knows its last column is empty should not have to spell out the padding, but the
    // missing cell must still occupy its column or the bars below it would shift.
    let rendered = render(&Value::Table(Table {
        headers: vec!["a".to_string(), "b".to_string()],
        rows: vec![
            vec!["1".to_string()],
            vec!["2".to_string(), "3".to_string()],
        ],
    }));
    assert_eq!(rendered, "| a | b |\n| - | - |\n| 1 |   |\n| 2 | 3 |");
}

#[test]
fn an_empty_table_renders_as_empty() {
    assert_eq!(render(&Value::Table(Table::default())), "");
}

#[test]
fn a_cell_with_a_newline_does_not_break_its_row() {
    let rendered = render(&Value::Table(table(&["a", "b"], &[&["x\ny", "z"]])));
    assert_eq!(
        rendered.lines().count(),
        3,
        "three lines, not four: {rendered}"
    );
    assert!(rendered.contains("x y"), "{rendered}");
}

#[test]
fn an_empty_column_still_gets_a_rule() {
    // `|  |` would read as a blank cell; the rule needs at least one dash to look like a rule.
    let rendered = render(&Value::Table(Table {
        headers: Vec::new(),
        rows: vec![vec![String::new(), String::new()]],
    }));
    assert_eq!(rendered, "|  |  |");
}

// ---- infer: JSON --------------------------------------------------------------------------------

#[test]
fn an_object_is_inferred_as_json() {
    let inferred = infer(r#"{"a":1}"#);
    assert_eq!(inferred, Some(Value::Json(json!({ "a": 1 }))));
}

#[test]
fn an_array_is_inferred_as_json() {
    let inferred = infer("[1, 2, 3]");
    assert_eq!(inferred, Some(Value::Json(json!([1, 2, 3]))));
}

#[test]
fn surrounding_whitespace_does_not_prevent_json() {
    assert_eq!(
        infer("  \n {\"a\": 1} \n "),
        Some(Value::Json(json!({ "a": 1 })))
    );
}

#[test]
fn json_scalars_are_not_inferred() {
    // Each of these is valid JSON and also plausible ordinary output. A client told "this is data"
    // would stop treating a bare number as text, so the shape is refused instead.
    for text in ["42", "true", "null", r#""a string""#, "1.5"] {
        assert_eq!(infer(text), None, "inferred a scalar from {text:?}");
    }
}

#[test]
fn malformed_json_is_not_inferred() {
    for text in ["{\"a\": }", "[1, 2", "{not json}", "[unterminated"] {
        assert_eq!(infer(text), None, "inferred a structure from {text:?}");
    }
}

// ---- infer: tables ------------------------------------------------------------------------------

#[test]
fn a_pipe_table_is_inferred() {
    assert_eq!(
        infer("| a | b |\n| 1 | 2 |"),
        Some(Value::Table(table(&["a", "b"], &[&["1", "2"]])))
    );
}

#[test]
fn a_rule_line_is_dropped_and_the_rest_kept() {
    assert_eq!(
        infer("| name | size |\n| --- | ---: |\n| a.rs | 12 |\n| b.rs | 3 |"),
        Some(Value::Table(table(
            &["name", "size"],
            &[&["a.rs", "12"], &["b.rs", "3"]],
        )))
    );
}

#[test]
fn blank_lines_around_a_table_are_ignored() {
    assert_eq!(
        infer("\n\n| a | b |\n\n| 1 | 2 |\n\n"),
        Some(Value::Table(table(&["a", "b"], &[&["1", "2"]])))
    );
}

#[test]
fn a_rendered_table_survives_a_round_trip() {
    // The renderer and the inferrer have to agree, or a table that is rendered for the model and
    // then re-read from that text would come back as something else.
    let original = table(&["id", "description"], &[&["1", "first"], &["2", "second"]]);
    let rendered = render(&Value::Table(original.clone()));
    assert_eq!(infer(&rendered), Some(Value::Table(original)));
}

#[test]
fn prose_with_pipes_is_not_inferred_as_a_table() {
    for text in [
        "run a | grep b",
        "note | here\nanother | line",
        "the command is `cat file | wc -l`",
        "a | b\nc",
    ] {
        assert_eq!(infer(text), None, "inferred a table from {text:?}");
    }
}

#[test]
fn a_single_pipe_line_is_not_inferred_as_a_table() {
    // One fenced line is a list that happens to use bars. There is no second row to confirm that
    // the bars are column boundaries rather than separators inside one row.
    assert_eq!(infer("| a | b |"), None);
    assert_eq!(infer("| a | b |\n| 1 | 2 | 3 |"), None);
}

#[test]
fn a_ragged_table_is_not_inferred() {
    // Padding a short row means guessing whether a cell was omitted from the middle or the end, and
    // the text does not say. Refusing is the only answer that cannot misfile a value.
    assert_eq!(infer("| a | b |\n| 1 | 2 |\n| 3 |"), None);
}

#[test]
fn a_table_without_a_heading_row_is_not_inferred() {
    assert_eq!(infer("| --- | --- |\n| 1 | 2 |"), None);
    assert_eq!(infer("| a |  |\n| 1 | 2 |"), None);
}

#[test]
fn a_one_column_table_is_not_inferred() {
    assert_eq!(infer("| a |\n| 1 |"), None);
}

// ---- infer: the empty and the ordinary ----------------------------------------------------------

#[test]
fn empty_and_blank_text_is_not_inferred() {
    for text in ["", "   ", "\n\n\t\n"] {
        assert_eq!(infer(text), None, "inferred a structure from {text:?}");
    }
}

#[test]
fn ordinary_prose_is_not_inferred() {
    for text in [
        "done",
        "wrote 3 files",
        "line one\nline two\nline three",
        "key: value\nother: value2",
        "|",
    ] {
        assert_eq!(infer(text), None, "inferred a structure from {text:?}");
    }
}
