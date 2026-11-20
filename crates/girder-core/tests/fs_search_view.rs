//! Tests for the search-result projection gap (`fs_gap`).
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written even before the crate root wires it in. That is why nothing here
//! reaches for `crate::`: the module's own imports already resolve to `girder_core` root exports.
//!
//! What is worth pinning is the honesty of a bounded result. A search that showed 10 of 400 matches
//! must say so, a previewed line must be marked as cut, and a sampled `glob` page must not look
//! like a complete modification-time listing. Each of those is a fact a model will act on, and each
//! is silent when it is wrong.

#[path = "../src/fs/search_view.rs"]
pub mod fs_gap;

use fs_gap::{
    GLOB_MAX_RESULTS, GREP_MAX_LINE_BYTES, GREP_MAX_MATCHES, GlobSample, GrepMatch,
    SEARCH_META_MAX_BYTES, SearchMeta, format_glob_output, format_grep_matches, format_grep_output,
    format_retained_grep, glob_search_meta, grep_search_meta, group_matches_by_file, preview_line,
    render_glob_paths, retain_glob_paths, retain_grep_matches, sample_across_top_level,
    to_workdir_relative,
};
use girder_core::{Omitted, RetainedItems, SpillReference};

fn matches_of(pairs: &[(&str, u64, &str)]) -> Vec<GrepMatch> {
    pairs
        .iter()
        .map(|(path, line_number, line)| GrepMatch {
            path: (*path).to_owned(),
            line_number: *line_number,
            line: (*line).to_owned(),
        })
        .collect()
}

fn spill() -> SpillReference {
    SpillReference {
        id: "sp1".to_owned(),
        path: std::path::PathBuf::from("/tmp/spill/grep.txt"),
        bytes: 4096,
        hint: "Read it with the read tool.".to_owned(),
    }
}

// ---- retention: what was kept, what was dropped, and the exact count ---------------------------

#[test]
fn a_match_list_within_the_cap_is_not_marked_truncated() {
    let matches = matches_of(&[("src/a.rs", 1, "alpha"), ("src/b.rs", 2, "beta")]);
    let retained = retain_grep_matches(matches, GREP_MAX_MATCHES, GREP_MAX_LINE_BYTES);

    assert_eq!(retained.seen, 2);
    assert_eq!(retained.kept, 2);
    assert!(
        !retained.truncated,
        "an uncapped result must not claim a cap"
    );
}

#[test]
fn an_over_cap_list_counts_every_match_but_keeps_only_the_head() {
    let matches: Vec<GrepMatch> = (1..=400)
        .map(|index| GrepMatch {
            path: "src/big.rs".to_owned(),
            line_number: index,
            line: format!("hit {index}"),
        })
        .collect();
    let retained = retain_grep_matches(matches, GREP_MAX_MATCHES, GREP_MAX_LINE_BYTES);

    assert_eq!(
        retained.seen, 400,
        "seen is the search's fact, not the cap's"
    );
    assert_eq!(
        retained.kept,
        u64::try_from(GREP_MAX_MATCHES).expect("fits")
    );
    assert!(retained.truncated);
    assert_eq!(
        retained.omitted,
        Omitted::Exact {
            count: 400 - u64::try_from(GREP_MAX_MATCHES).expect("fits"),
        }
    );
    // Head retention: the first match survives and the last does not.
    assert_eq!(
        retained.items.first().map(|entry| entry.line_number),
        Some(1)
    );
    assert_eq!(
        retained.items.last().map(|entry| entry.line_number),
        Some(u64::try_from(GREP_MAX_MATCHES).expect("fits"))
    );
}

#[test]
fn a_long_matched_line_is_previewed_and_marked() {
    let line = "x".repeat(GREP_MAX_LINE_BYTES * 2);
    let preview = preview_line(&line, GREP_MAX_LINE_BYTES);

    assert!(
        preview.ends_with(" (line truncated)"),
        "a cut line must say so"
    );
    assert!(
        preview.len() < line.len(),
        "the preview must actually be shorter than the line"
    );
}

#[test]
fn a_short_line_is_returned_untouched_by_the_preview() {
    assert_eq!(
        preview_line("fn main() {", GREP_MAX_LINE_BYTES),
        "fn main() {"
    );
}

#[test]
fn a_preview_cuts_on_a_character_boundary() {
    // Three bytes per character, a budget of four: a naive cut would split the second character and
    // produce a replacement character, which a model would then quote as file content.
    let preview = preview_line("是是是", 4);

    assert!(preview.starts_with('是'));
    assert!(
        preview.chars().all(|character| character == '是'
            || character == ' '
            || character == '('
            || character == 'l'
            || character == 'i'
            || character == 'n'
            || character == 'e'
            || character == 't'
            || character == 'r'
            || character == 'u'
            || character == 'n'
            || character == 'c'
            || character == 'a'
            || character == 'd'
            || character == ')'),
        "the preview must not contain a replacement character: {preview:?}"
    );
}

// ---- formatting: the header and footer are the contract --------------------------------------

#[test]
fn a_complete_result_reports_its_exact_count() {
    let retained = RetainedItems {
        items: matches_of(&[("src/a.rs", 7, "alpha")]),
        truncated: false,
        seen: 1,
        kept: 1,
        omitted: Omitted::None,
    };
    let text = format_grep_output(&retained, None);

    assert!(text.starts_with("Found 1 match\n\n"), "got: {text}");
    assert!(text.contains("src/a.rs\nLine 7: alpha"), "got: {text}");
    assert!(
        !text.contains("stored at"),
        "a complete result needs no recovery path"
    );
}

#[test]
fn a_capped_result_says_how_much_it_showed_and_where_the_rest_is() {
    let retained = RetainedItems {
        items: matches_of(&[("src/a.rs", 7, "alpha")]),
        truncated: true,
        seen: 400,
        kept: 1,
        omitted: Omitted::Exact { count: 399 },
    };
    let text = format_grep_output(&retained, Some(&spill()));

    assert!(text.starts_with("Found 1 of 400 matches"), "got: {text}");
    assert!(
        text.contains("Full grep result stored at: /tmp/spill/grep.txt."),
        "got: {text}"
    );
    assert!(text.contains("Read it with the read tool."), "got: {text}");
    assert!(text.trim_end().ends_with(')'), "the footer must be closed");
}

#[test]
fn a_capped_result_with_no_saved_copy_names_the_next_narrowing_step() {
    let retained = RetainedItems {
        items: matches_of(&[("src/a.rs", 7, "alpha")]),
        truncated: true,
        seen: 3,
        kept: 1,
        omitted: Omitted::Exact { count: 2 },
    };
    let text = format_grep_output(&retained, None);

    assert!(
        text.contains("The complete result could not be saved"),
        "got: {text}"
    );
    // The `include` filter is part of the remedy and must not be dropped.
    assert!(text.contains("include"), "got: {text}");
    assert!(!text.contains("stored at"), "nothing was stored");
}

#[test]
fn a_zero_result_search_is_an_answer_rather_than_an_empty_body() {
    let retained = RetainedItems {
        items: Vec::new(),
        truncated: false,
        seen: 0,
        kept: 0,
        omitted: Omitted::None,
    };

    assert_eq!(format_retained_grep(&retained, None), "No matches found");
}

#[test]
fn matches_are_grouped_by_file_in_first_seen_order() {
    let matches = matches_of(&[
        ("src/a.rs", 1, "one"),
        ("src/b.rs", 2, "two"),
        ("src/a.rs", 9, "nine"),
    ]);
    let groups = group_matches_by_file(&matches);

    assert_eq!(groups.len(), 2, "two distinct files");
    assert_eq!(groups[0].path, "src/a.rs");
    assert_eq!(groups[0].matches.len(), 2);
    assert_eq!(groups[0].matches[1].line_number, 9);
    assert_eq!(groups[1].path, "src/b.rs");

    let text = format_grep_matches(&matches);
    assert!(
        text.contains("src/a.rs\nLine 1: one\nLine 9: nine"),
        "got: {text}"
    );
    assert!(
        text.contains("\n\nsrc/b.rs\nLine 2: two"),
        "files are separated"
    );
}

// ---- glob sampling: the page must reflect the shape of the match ------------------------------

#[test]
fn an_over_cap_glob_page_reaches_every_top_level_entry_before_a_second_item() {
    // Four entries, five items to fill: every entry must appear once, then one gets a second slot.
    let paths: Vec<String> = vec![
        "src/a.rs".to_owned(),
        "src/b.rs".to_owned(),
        "src/c.rs".to_owned(),
        "docs/d.md".to_owned(),
        "docs/e.md".to_owned(),
        "tests/f.rs".to_owned(),
        "tests/g.rs".to_owned(),
    ];
    let sample = sample_across_top_level(&paths, 5, ".");

    assert_eq!(sample.total, 3, "three top-level entries");
    assert_eq!(sample.items.len(), 5, "the page holds five paths");
    assert_eq!(sample.shown, 3, "all three entries are reached");

    let reached: Vec<&str> = sample
        .items
        .iter()
        .map(|path| path.split('/').next().unwrap_or(path))
        .collect();
    for entry in ["src", "docs", "tests"] {
        assert!(
            reached.contains(&entry),
            "entry `{entry}` must appear in the page"
        );
    }
}

#[test]
fn a_page_smaller_than_the_entry_count_still_takes_one_item_from_the_first_entries() {
    let paths: Vec<String> = vec!["a/1".to_owned(), "b/2".to_owned(), "c/3".to_owned()];
    let sample = sample_across_top_level(&paths, 2, ".");

    assert_eq!(sample.items.len(), 2);
    assert_eq!(sample.shown, 2);
    assert_eq!(sample.total, 3);
    assert_eq!(sample.items, vec!["a/1".to_owned(), "b/2".to_owned()]);
}

#[test]
fn a_flat_result_reproduces_the_modification_time_head() {
    let paths: Vec<String> = (0..5).map(|index| format!("file{index}.rs")).collect();
    let sample = sample_across_top_level(&paths, 3, ".");

    // One group (`file0.rs`, `file1.rs`, … each with no separator is its own top-level entry), so
    // round-robin degrades to the input order — which is the head the caller already had.
    assert_eq!(sample.total, 5);
    assert_eq!(sample.items, paths[..3].to_vec());
}

#[test]
fn the_search_root_prefix_is_stripped_before_grouping() {
    let paths: Vec<String> = vec!["src/a/1.rs".to_owned(), "src/b/2.rs".to_owned()];
    // Grouped relative to `src`: the top level is then `a` and `b`, not one `src` group.
    let sample = sample_across_top_level(&paths, 10, "src");

    assert_eq!(
        sample.total, 2,
        "root prefix must not become the only group"
    );
    assert_eq!(sample.items.len(), 2);
}

#[test]
fn a_glob_result_within_the_cap_is_shown_whole_and_in_order() {
    let paths: Vec<String> = vec!["b.rs".to_owned(), "a.rs".to_owned()];
    let text = render_glob_paths(&paths, GLOB_MAX_RESULTS, ".", true, None);

    assert_eq!(text, "b.rs\na.rs", "a complete result is untouched");
    assert!(!text.contains("Showing"), "no caveat is needed");
}

#[test]
fn an_empty_glob_result_is_named_rather_than_left_blank() {
    let text = render_glob_paths(&[], GLOB_MAX_RESULTS, ".", true, None);
    assert_eq!(text, "No files found");
}

#[test]
fn an_over_cap_glob_result_reports_how_much_of_the_tree_it_sampled() {
    let paths: Vec<String> = (0..12).map(|index| format!("src/{index}.rs")).collect();
    let saved = spill();
    let text = render_glob_paths(&paths, 4, ".", true, Some(&saved));

    assert!(text.starts_with("src/0.rs\n"), "got: {text}");
    assert!(text.contains("Showing 4 of 12 paths"), "got: {text}");
    assert!(
        text.contains("Full sorted result stored at: /tmp/spill/grep.txt."),
        "got: {text}"
    );
}

#[test]
fn the_headed_glob_page_does_not_claim_sampling() {
    let paths: Vec<String> = (0..12).map(|index| format!("src/{index}.rs")).collect();
    let text = render_glob_paths(&paths, 4, ".", false, None);

    assert!(text.contains("Showing 4 of 12 paths."), "got: {text}");
    assert!(
        !text.contains("sampled across"),
        "the head was taken, not sampled: {text}"
    );
    assert!(
        text.contains("The complete result could not be saved"),
        "got: {text}"
    );
}

#[test]
fn a_sampled_page_that_reaches_every_entry_states_the_basis_without_overstating_it() {
    // Three entries, a page of three: every entry is reached, so the "narrow the path" advice is
    // wrong and must be absent.
    let sample = GlobSample {
        items: vec!["a/1".to_owned(), "b/2".to_owned(), "c/3".to_owned()],
        shown: 3,
        total: 3,
    };
    let text = format_glob_output(&sample, 10, None);

    assert!(
        text.contains("sampled across 3 of the 3 top-level entries"),
        "got: {text}"
    );
    assert!(
        !text.contains("Narrow path"),
        "every entry was reached: {text}"
    );
}

#[test]
fn an_unreached_entry_earns_the_narrowing_advice() {
    let sample = GlobSample {
        items: vec!["a/1".to_owned()],
        shown: 1,
        total: 5,
    };
    let text = format_glob_output(&sample, 40, None);

    assert!(
        text.contains("sampled across 1 of the 5 top-level entries"),
        "got: {text}"
    );
    assert!(
        text.contains("Narrow path to inspect a specific subtree"),
        "got: {text}"
    );
}

// ---- display paths ----------------------------------------------------------------------------

#[test]
fn a_path_under_the_workdir_is_shown_relative_to_it() {
    assert_eq!(
        to_workdir_relative("/work/project/src/a.rs", "/work/project"),
        "src/a.rs"
    );
    assert_eq!(
        to_workdir_relative("/work/project/deep/nested/a.rs", "/work/project"),
        "deep/nested/a.rs"
    );
}

#[test]
fn the_workdir_itself_is_shown_as_a_dot() {
    assert_eq!(to_workdir_relative("/work/project", "/work/project"), ".");
}

#[test]
fn a_path_outside_the_workdir_keeps_its_absolute_form() {
    // Rewriting it to `../elsewhere/a.rs` would make the display depend on where the process ran.
    assert_eq!(
        to_workdir_relative("/elsewhere/a.rs", "/work/project"),
        "/elsewhere/a.rs"
    );
}

#[test]
fn a_relative_input_is_already_in_the_callers_frame() {
    assert_eq!(to_workdir_relative("src/a.rs", "/work/project"), "src/a.rs");
}

#[test]
fn a_path_sharing_a_prefix_with_the_workdir_is_not_inside_it() {
    // `/work/project2` starts with `/work/project` as a string but is a sibling directory.
    assert_eq!(
        to_workdir_relative("/work/project2/a.rs", "/work/project"),
        "/work/project2/a.rs"
    );
}

#[test]
fn dot_segments_are_resolved_before_the_containment_test() {
    assert_eq!(
        to_workdir_relative("/work/project/./src/../a.rs", "/work/project"),
        "a.rs"
    );
}

// ---- card metadata: bounded, replay-safe, and honest about totals ------------------------------

#[test]
fn grep_metadata_groups_matches_and_counts_every_one() {
    let matches = matches_of(&[
        ("src/a.rs", 1, "one"),
        ("src/a.rs", 4, "four"),
        ("src/b.rs", 2, "two"),
    ]);
    let retained = retain_grep_matches(matches, GREP_MAX_MATCHES, GREP_MAX_LINE_BYTES);
    let meta = grep_search_meta(&retained, SEARCH_META_MAX_BYTES);

    match &meta {
        SearchMeta::Matches {
            files,
            truncated,
            total,
        } => {
            assert_eq!(files.len(), 2);
            assert!(!truncated);
            assert_eq!(*total, 3);
            assert_eq!(files[0].matches.len(), 2);
        }
        other @ SearchMeta::Paths { .. } => panic!("expected the matches shape, got {other:?}"),
    }
}

#[test]
fn a_zero_result_projection_is_a_valid_card() {
    let retained = retain_grep_matches(Vec::new(), GREP_MAX_MATCHES, GREP_MAX_LINE_BYTES);
    let meta = grep_search_meta(&retained, SEARCH_META_MAX_BYTES);
    let json = meta.to_value();

    assert!(
        SearchMeta::from_value(&json).is_some(),
        "an empty card is still a card"
    );
    assert_eq!(
        json.get("total").and_then(serde_json::Value::as_u64),
        Some(0)
    );
}

#[test]
fn glob_metadata_is_path_shaped() {
    let paths: Vec<String> = vec!["a.rs".to_owned(), "b.rs".to_owned()];
    let retained = retain_glob_paths(paths, GLOB_MAX_RESULTS);
    let meta = glob_search_meta(&retained, SEARCH_META_MAX_BYTES);

    let json = meta.to_value();
    assert_eq!(
        json.get("shape").and_then(serde_json::Value::as_str),
        Some("paths")
    );
    assert_eq!(
        json.get("paths")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(2)
    );
}

#[test]
fn metadata_round_trips_through_json() {
    let matches = matches_of(&[("src/a.rs", 1, "one"), ("src/b.rs", 2, "two")]);
    let retained = retain_grep_matches(matches, GREP_MAX_MATCHES, GREP_MAX_LINE_BYTES);
    let meta = grep_search_meta(&retained, SEARCH_META_MAX_BYTES);

    let json = meta.to_value();
    let restored = SearchMeta::from_value(&json);

    assert_eq!(restored, Some(meta));
}

#[test]
fn metadata_is_capped_by_dropping_trailing_groups_only() {
    // A tiny budget forces the cap; the first group must survive even though it alone may not fit.
    let matches = matches_of(&[
        ("src/a.rs", 1, "one"),
        ("src/b.rs", 2, "two"),
        ("src/c.rs", 3, "three"),
        ("src/d.rs", 4, "four"),
    ]);
    let retained = retain_grep_matches(matches, GREP_MAX_MATCHES, GREP_MAX_LINE_BYTES);
    let meta = grep_search_meta(&retained, 80);

    match meta {
        SearchMeta::Matches {
            files,
            truncated,
            total,
        } => {
            assert!(truncated, "the byte cap bit");
            assert_eq!(total, 4, "total counts what was found, not what fit");
            assert!(!files.is_empty(), "an empty card would hide a real result");
            assert!(files.len() < 4, "something was dropped to fit");
            assert_eq!(files[0].path, "src/a.rs", "the head is what survives");
        }
        other @ SearchMeta::Paths { .. } => panic!("expected the matches shape, got {other:?}"),
    }
}

#[test]
fn a_single_oversized_group_is_kept_rather_than_emptied() {
    let long = "y".repeat(4_000);
    let matches = matches_of(&[("src/a.rs", 1, &long)]);
    let retained = retain_grep_matches(matches, GREP_MAX_MATCHES, GREP_MAX_LINE_BYTES);
    let meta = grep_search_meta(&retained, 64);

    match meta {
        SearchMeta::Matches { files, .. } => {
            assert_eq!(files.len(), 1, "the only group must not be dropped");
        }
        other @ SearchMeta::Paths { .. } => panic!("expected the matches shape, got {other:?}"),
    }
}

#[test]
fn malformed_metadata_declines_rather_than_panicking() {
    // Every one of these is something a hand-edited or obsolete log could hold. Replay must fall
    // back to the generic card, so each has to be `None` and none may panic.
    let cases = [
        serde_json::json!(null),
        serde_json::json!("a string"),
        serde_json::json!([]),
        serde_json::json!({}),
        serde_json::json!({ "shape": "matches", "truncated": false }),
        serde_json::json!({ "shape": "unknown", "truncated": false, "total": 0 }),
        serde_json::json!({ "shape": "paths", "truncated": "no", "total": 0 }),
        serde_json::json!({ "shape": "paths", "truncated": false, "total": -1 }),
        serde_json::json!({ "shape": "matches", "truncated": false, "total": 1, "files": [{}] }),
        serde_json::json!({
            "shape": "matches",
            "truncated": false,
            "total": 1,
            "files": [{ "path": "a.rs", "matches": [{ "line": "x" }] }],
        }),
        serde_json::json!({
            "shape": "paths",
            "truncated": false,
            "total": 1,
            "paths": [1, 2],
        }),
    ];

    for case in cases {
        assert_eq!(
            SearchMeta::from_value(&case),
            None,
            "should decline: {case}"
        );
    }
}

#[test]
fn the_match_noun_is_singular_only_for_exactly_one() {
    let one = RetainedItems {
        items: matches_of(&[("a.rs", 1, "x")]),
        truncated: false,
        seen: 1,
        kept: 1,
        omitted: Omitted::None,
    };
    let two = RetainedItems {
        items: matches_of(&[("a.rs", 1, "x"), ("a.rs", 2, "y")]),
        truncated: false,
        seen: 2,
        kept: 2,
        omitted: Omitted::None,
    };

    assert!(format_grep_output(&one, None).starts_with("Found 1 match\n"));
    assert!(format_grep_output(&two, None).starts_with("Found 2 matches\n"));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod fs {
    pub use super::fs_gap as search_view;
}
