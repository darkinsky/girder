//! Tests for the tool tables: the value each tool hands over, and the text the model reads.
//!
//! The load-bearing case is alignment, and the hardest part of alignment is width. A model reads the
//! diagram, not the structure, so a single misplaced bar files a value under its neighbour's heading.
//! Half the assertions below therefore check bar positions rather than cell contents, and the CJK
//! cases check that a two-cell character was counted as two.
//!
//! The module is included by path rather than through the crate's re-exports, matching
//! `tests/tool_value.rs`: these tests then compile against the module as written, whether or not the
//! crate root has wired it up yet.

#[path = "../src/core/table_tools.rs"]
pub mod table_tools;

use girder_core::{RenderedTable, StructuredValue, render_tool_value};
use table_tools::{FileRow, MatchRow, file_table, match_table, render_table};

/// Extracts the table from a value, failing the test with a readable message if it is not one.
fn table_of(value: &StructuredValue) -> &RenderedTable {
    match value {
        StructuredValue::Table(table) => table,
        other => panic!("expected a table, got {other:?}"),
    }
}

/// The byte offset at which each `|` appears on a line.
///
/// Comparing these vectors across lines is the whole alignment question: equal positions mean the
/// grid holds, and any drift shows up as the index where two vectors first differ.
///
/// This is only a valid alignment check for ASCII fixtures, where a byte is a cell. A CJK line is
/// *correctly* aligned at different byte offsets, so the wide-character tests assert exact golden
/// strings instead of reusing this helper.
fn bar_offsets(line: &str) -> Vec<usize> {
    line.bytes()
        .enumerate()
        .filter_map(|(index, byte)| (byte == b'|').then_some(index))
        .collect()
}

/// Asserts that every line of an ASCII rendering puts its bars at the same byte offsets.
///
/// A row rendered with a newline inside a cell is caught here too: it would be an extra line with
/// the wrong number of bars, and the failure names the line rather than only the fact.
fn assert_aligned(text: &str) {
    let lines: Vec<&str> = text.lines().collect();
    let Some(first) = lines.first() else {
        panic!("rendering was empty");
    };
    let expected = bar_offsets(first);
    assert!(expected.len() > 1, "rendering has no columns: {text:?}");
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(
            bar_offsets(line),
            expected,
            "line {index} is not aligned with the header: {text:?}"
        );
    }
}

/// Asserts that every line has exactly four bars, which is what three columns produce.
fn assert_three_columns(text: &str) {
    for line in text.lines() {
        assert_eq!(
            bar_offsets(line).len(),
            4,
            "line does not have three columns: {line:?}"
        );
    }
}

// ---- file_table: structure ---------------------------------------------------------------------

#[test]
fn file_table_has_the_three_documented_columns() {
    let rows = vec![
        FileRow {
            path: String::from("src/main.rs"),
            bytes: 1_536,
            modified_ms: 1_700_000_000_000,
        },
        FileRow {
            path: String::from("README.md"),
            bytes: 12,
            modified_ms: 1_700_000_001_000,
        },
    ];
    let value = file_table(&rows);
    let table = table_of(&value);

    assert_eq!(table.headers, vec!["path", "bytes", "modified"]);
    assert_eq!(table.rows.len(), 2);
    assert_eq!(table.rows[0][0], "src/main.rs");
    assert_eq!(table.rows[0][1], "1536");
    // The instant is spelled the way the rest of the crate spells one, not as a raw count.
    assert_eq!(table.rows[0][2], "2023-11-14T22:13:20.000Z");
    assert_eq!(table.rows[1][0], "README.md");
}

#[test]
fn file_table_keeps_row_order() {
    let rows: Vec<FileRow> = (0..3)
        .map(|index| FileRow {
            path: format!("f{index}.txt"),
            bytes: index,
            modified_ms: 1_700_000_000_000,
        })
        .collect();
    let value = file_table(&rows);
    let paths: Vec<&str> = table_of(&value)
        .rows
        .iter()
        .map(|row| row[0].as_str())
        .collect();
    assert_eq!(paths, vec!["f0.txt", "f1.txt", "f2.txt"]);
}

#[test]
fn file_table_with_no_rows_keeps_its_headings() {
    let value = file_table(&[]);
    let table = table_of(&value);
    // Headings without rows is how a consumer tells "nothing matched" from "no table at all".
    assert_eq!(table.headers, vec!["path", "bytes", "modified"]);
    assert!(table.rows.is_empty());
    // The rendering is still a header and a rule, and still aligned.
    let text = render_table(&value);
    assert_eq!(text.lines().count(), 2);
    assert_aligned(&text);
}

// ---- file_table: rendering ---------------------------------------------------------------------

#[test]
fn file_table_renders_aligned_columns() {
    let rows = vec![
        FileRow {
            path: String::from("a.rs"),
            bytes: 1,
            modified_ms: 1_700_000_000_000,
        },
        FileRow {
            path: String::from("a_much_longer_file_name.rs"),
            bytes: 999_999,
            modified_ms: 1_700_000_000_000,
        },
        FileRow {
            path: String::from("mid.rs"),
            bytes: 4_096,
            modified_ms: 1_700_000_000_000,
        },
    ];
    let text = render_table(&file_table(&rows));
    assert_aligned(&text);
    // Every line has exactly four bars: three columns, both ends included.
    assert_three_columns(&text);
}

#[test]
fn file_table_pads_short_paths_to_the_widest_one() {
    let rows = vec![
        FileRow {
            path: String::from("x"),
            bytes: 1,
            modified_ms: 1_700_000_000_000,
        },
        FileRow {
            path: String::from("xxxxxx"),
            bytes: 2,
            modified_ms: 1_700_000_000_000,
        },
    ];
    let text = render_table(&file_table(&rows));
    let lines: Vec<&str> = text.lines().collect();
    // Line 2 is the first data row — the short one. Its path cell is padded out, so its bars sit
    // exactly where the long row's bars do.
    let first = lines
        .get(2)
        .copied()
        .unwrap_or_else(|| panic!("missing row line: {text:?}"));
    assert!(
        first.contains("| x      |"),
        "short path not padded: {first:?}"
    );
    assert_aligned(&text);
}

#[test]
fn file_table_renders_one_row_of_columns() {
    let rows = vec![FileRow {
        path: String::from("only.rs"),
        bytes: 7,
        modified_ms: 0,
    }];
    let text = render_table(&file_table(&rows));
    // Headings, rule, and exactly one row.
    assert_eq!(text.lines().count(), 3);
    assert_aligned(&text);
    assert!(text.contains("only.rs"));
}

#[test]
fn file_table_renders_a_single_column_of_data() {
    // A single row still has all three columns, and the rule spans each column's width.
    let rows = vec![FileRow {
        path: String::from("solo"),
        bytes: 42,
        modified_ms: 1_700_000_000_000,
    }];
    let text = render_table(&file_table(&rows));
    let rule = text.lines().nth(1).unwrap_or_default();
    assert!(rule.starts_with("| ---"), "missing rule: {rule:?}");
    assert_aligned(&text);
}

#[test]
fn file_table_survives_an_out_of_range_instant() {
    let rows = vec![FileRow {
        path: String::from("ancient.bin"),
        bytes: 0,
        // Past the four-digit-year bound `format_instant` refuses; the raw count is still shown.
        modified_ms: u64::MAX,
    }];
    let value = file_table(&rows);
    let table = table_of(&value);
    assert_eq!(table.rows[0][2], u64::MAX.to_string());
    assert_aligned(&render_table(&value));
}

// ---- CJK width --------------------------------------------------------------------------------

#[test]
fn cjk_paths_align_by_display_width() {
    // Four ideographs occupy eight cells, twice their character count. Padding by characters would
    // leave the other rows short by six spaces and every bar after this one would drift.
    let rows = vec![
        FileRow {
            path: String::from("文档/说明.md"),
            bytes: 1,
            modified_ms: 1_700_000_000_000,
        },
        FileRow {
            path: String::from("ascii_file_name_longer.md"),
            bytes: 2,
            modified_ms: 1_700_000_000_000,
        },
        FileRow {
            path: String::from("混合/mixed_name.rs"),
            bytes: 3,
            modified_ms: 1_700_000_000_000,
        },
    ];
    let text = render_table(&file_table(&rows));
    // The expected text is spelled out rather than measured with this crate's own width function:
    // checking the layout with the code that produced it would pass whatever that code did. These
    // lines were confirmed independently against Python's `unicodedata.east_asian_width`, and every
    // one of them is exactly 64 cells wide. `文档/说明.md` is 4 ideographs + 5 narrow characters = 13
    // cells but 11 characters, which is the discrepancy that makes character-count padding wrong.
    let expected = concat!(
        "| path                      | bytes | modified                 |\n",
        "| ------------------------- | ----- | ------------------------ |\n",
        "| 文档/说明.md              | 1     | 2023-11-14T22:13:20.000Z |\n",
        "| ascii_file_name_longer.md | 2     | 2023-11-14T22:13:20.000Z |\n",
        "| 混合/mixed_name.rs        | 3     | 2023-11-14T22:13:20.000Z |"
    );
    assert_eq!(text, expected);
}

#[test]
fn cjk_match_text_aligns_by_display_width() {
    let rows = vec![
        MatchRow {
            path: String::from("src/中文.rs"),
            line: 1,
            text: String::from("这是一个中文注释"),
        },
        MatchRow {
            path: String::from("src/lib.rs"),
            line: 22,
            text: String::from("fn main() {}"),
        },
    ];
    let text = render_table(&match_table(&rows));
    // `这是一个中文注释` is 8 ideographs = 16 cells but only 8 characters. Counting characters would
    // make that cell's bar land 8 cells early.
    let expected = concat!(
        "| path        | line | text             |\n",
        "| ----------- | ---- | ---------------- |\n",
        "| src/中文.rs | 1    | 这是一个中文注释 |\n",
        "| src/lib.rs  | 22   | fn main() {}     |"
    );
    assert_eq!(text, expected);
}

#[test]
fn fullwidth_and_hangul_are_counted_as_two_cells() {
    // Fullwidth Latin (U+FF21), Hangul (U+AC00) and kana (U+3042) are all East Asian Wide, so each
    // is three characters of UTF-8 and two cells. All four rows are padded to the same column.
    let rows = vec![
        FileRow {
            path: String::from("ＡＢＣ"),
            bytes: 1,
            modified_ms: 0,
        },
        FileRow {
            path: String::from("가나다"),
            bytes: 1,
            modified_ms: 0,
        },
        FileRow {
            path: String::from("あいう"),
            bytes: 1,
            modified_ms: 0,
        },
        FileRow {
            path: String::from("abc"),
            bytes: 1,
            modified_ms: 0,
        },
    ];
    let text = render_table(&file_table(&rows));
    let expected = concat!(
        "| path   | bytes | modified                 |\n",
        "| ------ | ----- | ------------------------ |\n",
        "| ＡＢＣ | 1     | 1970-01-01T00:00:00.000Z |\n",
        "| 가나다 | 1     | 1970-01-01T00:00:00.000Z |\n",
        "| あいう | 1     | 1970-01-01T00:00:00.000Z |\n",
        "| abc    | 1     | 1970-01-01T00:00:00.000Z |"
    );
    assert_eq!(text, expected);
}

#[test]
fn combining_marks_do_not_widen_a_cell() {
    // `e` followed by a combining acute is two characters but one cell. Counting characters would
    // over-pad this cell by one and push its trailing bar to the right of every other line's.
    let rows = vec![
        FileRow {
            path: String::from("cafe\u{0301}.txt"),
            bytes: 1,
            modified_ms: 0,
        },
        FileRow {
            path: String::from("cafes.txt"),
            bytes: 1,
            modified_ms: 0,
        },
    ];
    let text = render_table(&file_table(&rows));
    let expected = concat!(
        "| path      | bytes | modified                 |\n",
        "| --------- | ----- | ------------------------ |\n",
        "| cafe\u{0301}.txt  | 1     | 1970-01-01T00:00:00.000Z |\n",
        "| cafes.txt | 1     | 1970-01-01T00:00:00.000Z |"
    );
    assert_eq!(text, expected);
}

// ---- match_table: structure -------------------------------------------------------------------

#[test]
fn match_table_has_the_three_documented_columns() {
    let rows = vec![
        MatchRow {
            path: String::from("src/lib.rs"),
            line: 12,
            text: String::from("fn main() {"),
        },
        MatchRow {
            path: String::from("src/boot.rs"),
            line: 3,
            text: String::from("use crate::boot;"),
        },
    ];
    let value = match_table(&rows);
    let table = table_of(&value);

    assert_eq!(table.headers, vec!["path", "line", "text"]);
    assert_eq!(table.rows.len(), 2);
    assert_eq!(table.rows[0], vec!["src/lib.rs", "12", "fn main() {"]);
    assert_eq!(table.rows[1], vec!["src/boot.rs", "3", "use crate::boot;"]);
}

#[test]
fn match_table_with_no_rows_keeps_its_headings() {
    let value = match_table(&[]);
    let table = table_of(&value);
    assert_eq!(table.headers, vec!["path", "line", "text"]);
    assert!(table.rows.is_empty());
    // Two lines: the headings and the rule. Still aligned, still a table.
    let text = render_table(&value);
    assert_eq!(text.lines().count(), 2);
    assert_aligned(&text);
}

#[test]
fn match_table_renders_aligned_columns() {
    let rows = vec![
        MatchRow {
            path: String::from("a.rs"),
            line: 1,
            text: String::from("short"),
        },
        MatchRow {
            path: String::from("a_very_long_path_name.rs"),
            line: 1_000,
            text: String::from("a considerably longer line of matched source text"),
        },
    ];
    let text = render_table(&match_table(&rows));
    assert_aligned(&text);
    assert_three_columns(&text);
}

#[test]
fn match_table_renders_a_single_row() {
    let rows = vec![MatchRow {
        path: String::from("only.rs"),
        line: 1,
        text: String::from("hit"),
    }];
    let text = render_table(&match_table(&rows));
    assert_eq!(text.lines().count(), 3);
    assert_aligned(&text);
}

#[test]
fn a_multiline_match_text_cannot_break_its_row() {
    // A captured newline would split one row into two lines, and the tail would read as a row whose
    // every value is one column to the left.
    let rows = vec![
        MatchRow {
            path: String::from("a.rs"),
            line: 1,
            text: String::from("first\r\nsecond"),
        },
        MatchRow {
            path: String::from("b.rs"),
            line: 2,
            text: String::from("plain"),
        },
    ];
    let text = render_table(&match_table(&rows));
    assert_eq!(text.lines().count(), 4, "a cell broke its row: {text:?}");
    assert!(!text.contains('\r'), "a carriage return survived: {text:?}");
    assert_aligned(&text);
}

// ---- rendering contract -----------------------------------------------------------------------

#[test]
fn render_table_matches_render_tool_value_for_ascii() {
    // For text with no wide characters the two renderers must agree byte for byte: diverging here
    // would mean two answers to "what does this table look like".
    let rows = vec![
        FileRow {
            path: String::from("a.rs"),
            bytes: 1,
            modified_ms: 1_700_000_000_000,
        },
        FileRow {
            path: String::from("bbb.rs"),
            bytes: 100,
            modified_ms: 1_700_000_000_000,
        },
    ];
    let value = file_table(&rows);
    assert_eq!(render_table(&value), render_tool_value(&value));
}

#[test]
fn render_table_matches_render_tool_value_for_pipe_tables() {
    // The same agreement, reached through inference rather than construction: text rendered here,
    // inferred back, then rendered again must land on the same bytes.
    let inferred = girder_core::infer_tool_value("| a | b |\n| --- | --- |\n| 1 | 2 |");
    let Some(value) = inferred else {
        panic!("the pipe table should have been inferred");
    };
    assert_eq!(render_table(&value), render_tool_value(&value));
}

#[test]
fn render_table_passes_text_through_untouched() {
    // Leading whitespace, a trailing newline and CRLF are all data; a renderer that tidies them
    // corrupts diffs and code.
    let text = String::from("  indented\n\tTabbed\r\n");
    let value = StructuredValue::Text(text.clone());
    assert_eq!(render_table(&value), text);
}

#[test]
fn render_table_passes_json_through_compactly() {
    let value = StructuredValue::Json(serde_json::json!({"count": 2, "ok": true}));
    assert_eq!(render_table(&value), render_tool_value(&value));
    assert_eq!(render_table(&value), r#"{"count":2,"ok":true}"#);
}

#[test]
fn render_table_of_an_empty_table_is_empty() {
    let value = StructuredValue::Table(RenderedTable {
        headers: Vec::new(),
        rows: Vec::new(),
    });
    assert_eq!(render_table(&value), "");
}

#[test]
fn render_table_keeps_rows_wider_than_the_headers() {
    // The extra cell is data the tool reported. Dropping it would be silent; rendering it without a
    // heading is visible.
    let value = StructuredValue::Table(RenderedTable {
        headers: vec![String::from("a"), String::from("b")],
        rows: vec![vec![
            String::from("1"),
            String::from("2"),
            String::from("3"),
        ]],
    });
    let text = render_table(&value);
    assert_eq!(
        bar_offsets(text.lines().next().unwrap_or_default()).len(),
        4
    );
    assert_aligned(&text);
    assert!(text.contains('3'));
}

#[test]
fn render_table_keeps_short_rows_aligned() {
    let value = StructuredValue::Table(RenderedTable {
        headers: vec![String::from("a"), String::from("b"), String::from("c")],
        rows: vec![vec![String::from("1")]],
    });
    let text = render_table(&value);
    assert_aligned(&text);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod core {
    pub use super::table_tools as table_tools;
}
