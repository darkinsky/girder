//! Tests for the aligned file and match listings, and for the parsers that feed them.
//!
//! The load-bearing property is alignment: a model reads the diagram, not the structure, so a
//! single misplaced bar files a size under its neighbour's path. Half of the assertions below
//! therefore compare bar positions across lines rather than cell contents, and the CJK cases assert
//! exact golden strings, because the right byte offsets for a wide character are not the right cell
//! offsets — the alignment is correct precisely when the byte offsets differ.
//!
//! The module is included by path rather than through the crate's re-exports, matching
//! `tests/table_tools.rs`: the tests then compile against the module as written, whether or not the
//! crate root has wired it up yet.

#[path = "../src/util/list_render.rs"]
pub mod list_render;

use list_render::{
    FileEntry, MatchEntry, parse_glob_output, parse_grep_output, render_files, render_matches,
};

/// The byte offset at which each `|` appears on a line.
///
/// Comparing these vectors across lines is the whole alignment question for ASCII fixtures: equal
/// positions mean the grid holds, and any drift shows up as the index where two vectors differ.
///
/// This is only a valid check where one byte is one cell. A CJK line is *correctly* aligned at
/// different byte offsets, so those cases assert golden strings instead of reusing this helper.
fn bar_offsets(line: &str) -> Vec<usize> {
    line.bytes()
        .enumerate()
        .filter_map(|(index, byte)| (byte == b'|').then_some(index))
        .collect()
}

/// Asserts that every line puts its bars at the same byte offsets.
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

/// A file entry with a fixed timestamp, so goldens stay stable.
fn file(path: &str, bytes: u64) -> FileEntry {
    FileEntry {
        path: String::from(path),
        bytes,
        modified_ms: 1_700_000_000_000,
    }
}

/// A match entry with a fixed line number.
fn hit(path: &str, line: u32, text: &str) -> MatchEntry {
    MatchEntry {
        path: String::from(path),
        line,
        text: String::from(text),
    }
}

// ---- render_files: alignment -------------------------------------------------------------------

#[test]
fn file_rows_align_their_bars() {
    let text = render_files(&[
        file("src/main.rs", 1_536),
        file("README.md", 12),
        file("a", 7),
    ]);
    assert_aligned(&text);
    assert_three_columns(&text);
}

#[test]
fn file_table_is_exactly_the_documented_grid() {
    let text = render_files(&[file("src/main.rs", 1_536), file("README.md", 12)]);
    assert_eq!(
        text,
        concat!(
            "| path        | bytes | modified                 |\n",
            "| ----------- | ----- | ------------------------ |\n",
            "| src/main.rs |  1536 | 2023-11-14T22:13:20.000Z |\n",
            "| README.md   |    12 | 2023-11-14T22:13:20.000Z |",
        )
    );
}

#[test]
fn the_path_column_is_padded_to_its_widest_cell() {
    let text = render_files(&[file("a.txt", 1), file("a-very-long-path.txt", 2)]);
    let lines: Vec<&str> = text.lines().collect();
    // The short path gains spaces so that its bar lands where the long path's bar does.
    assert!(lines[2].starts_with("| a.txt"), "{:?}", lines[2]);
    assert_eq!(lines[2].find('|'), lines[1].find('|'));
    assert_aligned(&text);
}

#[test]
fn the_bytes_column_is_right_aligned() {
    let text = render_files(&[file("a", 1), file("b", 12), file("c", 1_234_567)]);
    let numbers: Vec<&str> = text
        .lines()
        .skip(2)
        .map(|line| {
            let cells: Vec<&str> = line.split('|').collect();
            cells[2]
        })
        .collect();
    // Padding on the left puts every last digit in the same cell, which is what makes magnitudes
    // comparable down the column. The only space after the digits is the cell's framing space; a
    // left-aligned number would instead carry its padding there.
    for cell in &numbers {
        let digits = cell.trim();
        assert!(digits.chars().all(|ch| ch.is_ascii_digit()), "{cell:?}");
        assert_eq!(
            cell.len() - cell.trim_end().len(),
            1,
            "cell is not right-aligned: {cell:?}"
        );
    }
    let widths: Vec<usize> = numbers.iter().map(|cell| cell.chars().count()).collect();
    assert!(
        widths.iter().all(|width| *width == widths[0]),
        "numeric cells have different widths: {widths:?}"
    );
    assert_aligned(&text);
}

#[test]
fn the_bytes_column_stays_right_aligned_when_a_path_is_wider() {
    let text = render_files(&[file("long/path/name.rs", 5), file("x.rs", 4_096)]);
    let rows: Vec<&str> = text.lines().skip(2).collect();
    assert!(rows[0].contains("|     5 |"), "{:?}", rows[0]);
    assert!(rows[1].contains("|  4096 |"), "{:?}", rows[1]);
    assert_aligned(&text);
}

// ---- render_files: sizes and shapes ------------------------------------------------------------

#[test]
fn an_empty_file_list_renders_nothing() {
    assert_eq!(render_files(&[]), "");
}

#[test]
fn a_single_file_renders_headings_a_rule_and_one_row() {
    let text = render_files(&[file("only.txt", 9)]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with("| path "), "{:?}", lines[0]);
    assert!(lines[1].contains('-'));
    assert!(lines[2].contains("only.txt"));
    assert!(!text.ends_with('\n'));
}

#[test]
fn the_modified_column_is_iso_not_a_raw_count() {
    let text = render_files(&[file("a.txt", 1)]);
    assert!(text.contains("2023-11-14T22:13:20.000Z"), "{text}");
    assert!(!text.contains("1700000000000"), "{text}");
}

#[test]
fn an_unnameable_timestamp_falls_back_to_the_raw_count() {
    // Past the four-digit-year bound, so the canonical form cannot spell it.
    let text = render_files(&[FileEntry {
        path: String::from("a.txt"),
        bytes: 1,
        modified_ms: u64::MAX,
    }]);
    assert!(text.contains(&u64::MAX.to_string()), "{text}");
    assert_aligned(&text);
}

// ---- CJK widths: the alignment question --------------------------------------------------------

#[test]
fn a_cjk_path_is_padded_by_display_cells_not_characters() {
    // `文档/a.md` is eight characters but nine cells: each ideograph is two cells wide, and one byte
    // longer than one character would suggest. Padding by char count would leave the bar one cell
    // short of `b.txt`'s, which is the drift this module exists to prevent.
    let text = render_files(&[file("文档/a.md", 1_234), file("b.txt", 7)]);
    assert_eq!(
        text,
        concat!(
            "| path      | bytes | modified                 |\n",
            "| --------- | ----- | ------------------------ |\n",
            "| 文档/a.md |  1234 | 2023-11-14T22:13:20.000Z |\n",
            "| b.txt     |     7 | 2023-11-14T22:13:20.000Z |",
        )
    );
}

#[test]
fn a_cjk_path_widens_the_column_for_every_row() {
    let text = render_files(&[file("说明文档.md", 1), file("a.md", 2)]);
    // Eleven cells for the wide name, four for `a.md`, so the short row gains seven spaces.
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[2].contains("说明文档.md"), "{:?}", lines[2]);
    assert!(lines[3].starts_with("| a.md        |"), "{:?}", lines[3]);
}

#[test]
fn mixed_width_paths_put_every_bar_in_the_same_cell() {
    let text = render_files(&[
        file("文档/a.md", 1),
        file("b.txt", 22),
        file("説明.md", 333),
    ]);
    // A byte-offset comparison would reject this correctly aligned grid; a cell comparison must not.
    let cell_positions: Vec<Vec<usize>> = text
        .lines()
        .map(|line| {
            let mut cells = Vec::new();
            let mut column = 0_usize;
            for ch in line.chars() {
                if ch == '|' {
                    cells.push(column);
                }
                column += char_cells(ch);
            }
            cells
        })
        .collect();
    assert!(cell_positions[0].len() > 1);
    for (index, positions) in cell_positions.iter().enumerate() {
        assert_eq!(
            positions, &cell_positions[0],
            "line {index} drifts in cells: {text:?}"
        );
    }
}

#[test]
fn a_match_text_with_a_wide_character_does_not_shift_the_bar() {
    let text = render_matches(&[hit("a.rs", 1, "let 名 = 1;"), hit("b.rs", 22, "ok")]);
    let last_bar_cells: Vec<usize> = text
        .lines()
        .map(|line| {
            line.chars()
                .map(char_cells)
                .sum::<usize>()
                .saturating_sub(1)
        })
        .collect();
    assert_eq!(last_bar_cells[0], last_bar_cells[1]);
}

// ---- render_matches ----------------------------------------------------------------------------

#[test]
fn match_rows_align_their_bars() {
    let text = render_matches(&[
        hit("src/main.rs", 12, "fn main() {"),
        hit("a.rs", 3, "let x = 1;"),
    ]);
    assert_aligned(&text);
    assert_three_columns(&text);
}

#[test]
fn the_line_column_is_right_aligned() {
    let text = render_matches(&[hit("a.rs", 5, "x"), hit("b.rs", 1_200, "y")]);
    let rows: Vec<&str> = text.lines().skip(2).collect();
    assert!(rows[0].contains("|    5 |"), "{:?}", rows[0]);
    assert!(rows[1].contains("| 1200 |"), "{:?}", rows[1]);
    assert_aligned(&text);
}

#[test]
fn an_empty_match_list_renders_nothing() {
    assert_eq!(render_matches(&[]), "");
}

#[test]
fn a_single_match_renders_headings_a_rule_and_one_row() {
    let text = render_matches(&[hit("a.rs", 1, "x")]);
    assert_eq!(text.lines().count(), 3);
    assert!(!text.ends_with('\n'));
}

#[test]
fn a_newline_inside_a_match_text_cannot_split_its_row() {
    let text = render_matches(&[hit("a.rs", 1, "first\nsecond"), hit("b.rs", 2, "other")]);
    assert_eq!(text.lines().count(), 4);
    assert_aligned(&text);
}

// ---- parse_glob_output -------------------------------------------------------------------------

#[test]
fn glob_output_parses_one_path_per_line() {
    let paths = parse_glob_output("src/main.rs\nsrc/lib.rs\nREADME.md\n");
    assert_eq!(paths, vec!["src/main.rs", "src/lib.rs", "README.md"]);
}

#[test]
fn a_no_files_match_notice_produces_no_entries() {
    assert!(parse_glob_output("no files match `*.xyz`").is_empty());
    assert!(render_files(&[]).is_empty());
}

#[test]
fn a_truncation_trailer_is_not_a_path() {
    let text = "a.rs\nb.rs\n\n(stopped after 200 files; narrow the pattern to see the rest)\n";
    assert_eq!(parse_glob_output(text), vec!["a.rs", "b.rs"]);
}

#[test]
fn glob_output_keeps_a_path_that_begins_with_a_space() {
    // Trimming would silently rename the file, and the model would then try to open a path that
    // does not exist.
    assert_eq!(parse_glob_output(" sp ace.txt\n"), vec![" sp ace.txt"]);
}

#[test]
fn empty_glob_output_produces_no_entries() {
    assert!(parse_glob_output("").is_empty());
    assert!(parse_glob_output("\n\n\n").is_empty());
}

// ---- parse_grep_output -------------------------------------------------------------------------

#[test]
fn grep_output_parses_path_line_and_text() {
    let entries = parse_grep_output("src/main.rs:12:fn main() {\nsrc/lib.rs:3:use std::io;\n");
    assert_eq!(
        entries,
        vec![
            hit("src/main.rs", 12, "fn main() {"),
            hit("src/lib.rs", 3, "use std::io;")
        ]
    );
}

#[test]
fn a_path_containing_a_colon_is_split_at_the_line_number() {
    // The drive letter makes the first colon a decoy: the field after it is `\a\b.txt`, which is not
    // a number, so the split must happen at the second colon and the path must come back whole.
    let entries = parse_grep_output("C:\\a\\b.txt:12:x\n");
    assert_eq!(entries, vec![hit("C:\\a\\b.txt", 12, "x")]);
}

#[test]
fn a_windows_path_keeps_every_backslash() {
    let entries = parse_grep_output("D:\\work\\src\\main.rs:7:return 0;\n");
    assert_eq!(entries, vec![hit("D:\\work\\src\\main.rs", 7, "return 0;")]);
}

#[test]
fn the_text_after_the_line_number_keeps_its_colons() {
    let entries = parse_grep_output("a.rs:12:url: http://host:8080/x\n");
    assert_eq!(entries, vec![hit("a.rs", 12, "url: http://host:8080/x")]);
}

#[test]
fn a_hit_on_line_zero_is_still_a_hit() {
    // `0` is not a plausible line, but it is a number, and dropping the row would hide a match the
    // tool actually reported.
    let entries = parse_grep_output("a.rs:0:edge\n");
    assert_eq!(entries, vec![hit("a.rs", 0, "edge")]);
}

#[test]
fn a_line_without_a_numeric_boundary_is_not_a_hit() {
    assert!(parse_grep_output("a.rs:text:more\n").is_empty());
    assert!(parse_grep_output("no separators at all\n").is_empty());
    assert!(parse_grep_output("a.rs:12\n").is_empty());
}

#[test]
fn grep_notices_produce_no_entries() {
    assert!(parse_grep_output("no matches for `zzz`\n").is_empty());
    assert!(parse_grep_output("a.rs:1:x\n\n(stopped after 50 matches)\n").len() == 1);
}

#[test]
fn a_line_number_too_large_for_the_field_is_skipped() {
    assert!(parse_grep_output("a.rs:99999999999999999999:x\n").is_empty());
}

// ---- end to end --------------------------------------------------------------------------------

#[test]
fn parsed_output_renders_as_an_aligned_table() {
    let entries = parse_grep_output(concat!(
        "src/main.rs:12:fn main() {\n",
        "C:\\a\\b.txt:1:let x = 1;\n",
        "(stopped after 2 matches)\n",
    ));
    let text = render_matches(&entries);
    assert_three_columns(&text);
    // Headings, rule, then one row per hit — the trailer produced no row.
    assert_eq!(text.lines().count(), 4);
    assert!(text.contains("C:\\a\\b.txt"), "{text}");
}

// ---- the cell-width rule -----------------------------------------------------------------------

/// The number of terminal cells a character occupies, for the fixtures used here.
///
/// Deliberately independent of the module's own table so the alignment assertion cannot pass by
/// agreeing with a bug: only the blocks these fixtures contain are handled.
fn char_cells(ch: char) -> usize {
    match ch {
        '\u{3000}'..='\u{9FFF}' | '\u{FF00}'..='\u{FFEF}' => 2,
        _ => 1,
    }
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod util {
    pub use super::list_render as list_render;
}
