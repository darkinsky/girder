//! Tests for `tool_extract`, the reader that recovers structure from tool text.
//!
//! Two properties carry this module, and both are about not inventing things. A shape that is not
//! understood must be [`Extracted::Unknown`], because a wrong path list renders a file the tool
//! never matched; and a definite empty result must not be `Unknown`, because "nothing matched" is
//! an answer a caller can act on. The rest of the file pins the two formats that are genuinely
//! ambiguous — `grep`'s `path:line:content` with a colon in the path, and `read`'s numbered block —
//! to the behavior the module documents.
//!
//! The source is included by path rather than through a crate re-export, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/tool_extract.rs"]
mod tool_extract;

use tool_extract::{Extracted, extract, glob_paths, grep_paths};

// ---- grep: the standard shape -----------------------------------------------------------------

#[test]
fn grep_matches_become_deduplicated_paths() {
    let output = "src/main.rs:3:fn main() {}\nsrc/lib.rs:10:pub fn helper() {}\nsrc/main.rs:9:}\n";

    let extracted = extract("grep", output);

    assert_eq!(
        extracted,
        Extracted::Paths {
            paths: vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()],
            truncated: false,
        }
    );
}

#[test]
fn a_grep_match_with_a_colon_in_its_content_keeps_the_path_whole() {
    // The colon inside the matched text is the common case and must not be mistaken for a boundary.
    let output = "src/app.ts:42:const ratio = a ? 1 : 2;\n";

    assert_eq!(grep_paths(output), vec!["src/app.ts".to_owned()]);
}

#[test]
fn a_grep_match_of_a_blank_line_has_no_content() {
    // The tool prints `path:line:` with nothing after it when the matched line is empty.
    let extracted = extract("grep", "src/empty.rs:7:\n");

    assert_eq!(
        extracted,
        Extracted::Paths {
            paths: vec!["src/empty.rs".to_owned()],
            truncated: false,
        }
    );
}

// ---- grep: the hard shape, a colon inside the path --------------------------------------------

#[test]
fn a_windows_drive_letter_is_not_a_line_number() {
    // `C:` is followed by `\`, not by digits, so the scan passes over it and finds `:12:`.
    let output = "C:\\repo\\src\\main.rs:12:fn main() {}\n";

    assert_eq!(grep_paths(output), vec![r"C:\repo\src\main.rs".to_owned()]);
}

#[test]
fn a_colon_inside_a_directory_name_stays_in_the_path() {
    let output = "src/gen:2024/out.rs:31:let x = 1;\n";

    assert_eq!(grep_paths(output), vec!["src/gen:2024/out.rs".to_owned()]);
}

#[test]
fn a_path_whose_own_name_has_a_line_number_is_documented_as_ambiguous() {
    // The documented limitation: splitting is at the leftmost `:<digits>:`, and here that lands
    // inside the path. The test pins the boundary so the behavior cannot drift unnoticed.
    let output = "notes:2:backup.txt:9:hello\n";

    assert_eq!(grep_paths(output), vec!["notes".to_owned()]);
}

#[test]
fn a_path_with_a_trailing_colon_number_is_read_from_the_right_place() {
    // A single `:<digits>:` boundary whose path itself contains `:` and no digits after it.
    let output = "src/a:b.rs:5:let y = 2;\n";

    assert_eq!(grep_paths(output), vec!["src/a:b.rs".to_owned()]);
}

// ---- grep: answers that are not matches -------------------------------------------------------

#[test]
fn no_matches_at_all_is_an_empty_list_not_unknown() {
    assert_eq!(
        extract("grep", "no matches for `SessionStore`\n"),
        Extracted::Paths {
            paths: Vec::new(),
            truncated: false,
        }
    );
}

#[test]
fn empty_output_is_an_empty_list_not_unknown() {
    assert_eq!(
        extract("grep", ""),
        Extracted::Paths {
            paths: Vec::new(),
            truncated: false,
        }
    );
    assert_eq!(
        extract("glob", ""),
        Extracted::Paths {
            paths: Vec::new(),
            truncated: false,
        }
    );
    assert_eq!(
        extract("read", ""),
        Extracted::Content {
            first_line: None,
            lines: Vec::new(),
        }
    );
}

#[test]
fn a_line_that_is_not_a_match_makes_the_whole_grep_result_unknown() {
    // One unreadable line means the text is not the format; returning the lines that did split
    // would present a partial list as the list.
    let output = "src/main.rs:3:fn main() {}\nwarning: something the tool said\n";

    assert_eq!(extract("grep", output), Extracted::Unknown);
}

// ---- grep: truncation -------------------------------------------------------------------------

#[test]
fn the_grep_stop_marker_is_reported_and_is_not_a_path() {
    let output = "src/a.rs:1:a\nsrc/b.rs:2:b\n\n(stopped after 200 matches)\n";

    assert_eq!(
        extract("grep", output),
        Extracted::Paths {
            paths: vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()],
            truncated: true,
        }
    );
}

#[test]
fn a_shortened_view_marker_also_marks_the_result_truncated() {
    // The output-budget layer can cap a result after the tool produced it, and that cut is just as
    // invisible to a reader as the tool's own limit.
    let output = "src/a.rs:1:a\n[truncated: kept 12 of 400 characters]\n";

    assert_eq!(
        extract("grep", output),
        Extracted::Paths {
            paths: vec!["src/a.rs".to_owned()],
            truncated: true,
        }
    );
}

// ---- glob -------------------------------------------------------------------------------------

#[test]
fn each_glob_line_is_one_path() {
    let output = "src/main.rs\nsrc/lib.rs\nREADME.md\n";

    assert_eq!(
        extract("glob", output),
        Extracted::Paths {
            paths: vec![
                "src/main.rs".to_owned(),
                "src/lib.rs".to_owned(),
                "README.md".to_owned(),
            ],
            truncated: false,
        }
    );
}

#[test]
fn glob_paths_ignores_the_no_match_line_and_the_stop_marker() {
    let output = "a.rs\nb.rs\n\n(stopped after 2 files; narrow the pattern to see the rest)\n";

    assert_eq!(
        glob_paths(output),
        vec!["a.rs".to_owned(), "b.rs".to_owned()]
    );
    assert_eq!(
        extract("glob", output),
        Extracted::Paths {
            paths: vec!["a.rs".to_owned(), "b.rs".to_owned()],
            truncated: true,
        }
    );
}

#[test]
fn glob_keeps_spaces_in_a_path_and_drops_a_carriage_return() {
    // A path may contain spaces and the format has no quoting, so the line is taken as written
    // apart from the CRLF ending a transport may have added.
    let output = "my notes/plan.md\r\nsrc/a.rs\r\n";

    assert_eq!(
        glob_paths(output),
        vec!["my notes/plan.md".to_owned(), "src/a.rs".to_owned()]
    );
}

#[test]
fn no_glob_match_is_an_empty_list() {
    assert_eq!(
        extract("glob", "no files match `*.xyz`\n"),
        Extracted::Paths {
            paths: Vec::new(),
            truncated: false,
        }
    );
}

// ---- read: the numbered block -------------------------------------------------------------------

#[test]
fn read_lines_are_split_into_numbers_and_content() {
    let output = "1\tfirst\n2\tsecond\n3\t  indented\n";

    assert_eq!(
        extract("read", output),
        Extracted::Content {
            first_line: Some(1),
            lines: vec![
                "first".to_owned(),
                "second".to_owned(),
                "  indented".to_owned(),
            ],
        }
    );
}

#[test]
fn a_read_range_starts_at_the_number_the_caller_asked_for() {
    let output = "40\tforty\n41\tforty-one\n";

    assert_eq!(
        extract("read", output),
        Extracted::Content {
            first_line: Some(40),
            lines: vec!["forty".to_owned(), "forty-one".to_owned()],
        }
    );
}

#[test]
fn a_language_hint_line_is_not_file_content() {
    let output = "rust\n1\tfn main() {}\n2\t}\n";

    assert_eq!(
        extract("read", output),
        Extracted::Content {
            first_line: Some(1),
            lines: vec!["fn main() {}".to_owned(), "}".to_owned()],
        }
    );
}

#[test]
fn a_tab_inside_the_content_is_preserved() {
    assert_eq!(
        extract("read", "1\tlet x =\t1;\n"),
        Extracted::Content {
            first_line: Some(1),
            lines: vec!["let x =\t1;".to_owned()],
        }
    );
}

#[test]
fn a_blank_content_line_is_kept_as_an_empty_string() {
    assert_eq!(
        extract("read", "1\tfirst\n2\t\n3\tthird\n"),
        Extracted::Content {
            first_line: Some(1),
            lines: vec!["first".to_owned(), String::new(), "third".to_owned()],
        }
    );
}

// ---- unknown ----------------------------------------------------------------------------------

#[test]
fn text_that_is_not_a_known_shape_is_unknown() {
    // Prose from a tool this module does not understand is not a path list, and saying otherwise
    // would render a table of files that do not exist.
    assert_eq!(extract("grep", "the build failed\n"), Extracted::Unknown);
    // A first line that is neither numbered nor hint-shaped is not a header to skip: skipping it
    // would drop content and leave the rest looking complete.
    assert_eq!(
        extract("read", "no tab here\n1\tbut a number later\n"),
        Extracted::Unknown
    );
    assert_eq!(extract("read", "line one\nline two\n"), Extracted::Unknown);
}

#[test]
fn an_unrelated_tool_name_is_unknown_without_reading_the_output() {
    // A `bash` result that happens to look like a path list is not a path list.
    assert_eq!(
        extract("bash", "src/main.rs\nsrc/lib.rs\n"),
        Extracted::Unknown
    );
    assert_eq!(extract("", "anything"), Extracted::Unknown);
}

#[test]
fn a_read_result_that_was_shortened_cannot_claim_to_be_the_file() {
    // `Content` has no field for "partial", so an honest answer here is `Unknown`; reporting the
    // surviving lines would assert a completeness the text does not have.
    let output = "1\tfn main() {}\n[truncated: kept 14 of 4000 characters]\n";

    assert_eq!(extract("read", output), Extracted::Unknown);
}

// ---- shared behavior ---------------------------------------------------------------------------

#[test]
fn a_trailing_newline_does_not_add_an_empty_path() {
    assert_eq!(
        glob_paths("a.rs\nb.rs\n"),
        vec!["a.rs".to_owned(), "b.rs".to_owned()]
    );
    assert_eq!(
        glob_paths("a.rs\nb.rs"),
        vec!["a.rs".to_owned(), "b.rs".to_owned()]
    );
}

#[test]
fn the_tool_name_is_trimmed_before_it_is_matched() {
    assert_eq!(
        extract(" grep ", "src/a.rs:1:a\n"),
        Extracted::Paths {
            paths: vec!["src/a.rs".to_owned()],
            truncated: false,
        }
    );
}

#[test]
fn duplicate_paths_collapse_only_in_the_structured_form() {
    let output = "src/a.rs:1:a\nsrc/a.rs:2:b\n";

    // `grep_paths` keeps one entry per matching line, because that is what it says it does.
    assert_eq!(
        grep_paths(output),
        vec!["src/a.rs".to_owned(), "src/a.rs".to_owned()]
    );
    assert_eq!(
        extract("grep", output),
        Extracted::Paths {
            paths: vec!["src/a.rs".to_owned()],
            truncated: false,
        }
    );
}
