//! Tests for `@file` reference completion: the `@` token grammar and the workspace index.
//!
//! Corresponds to upstream `packages/context/file-reference` and `file-reference-local`, restricted
//! to the parts this port covers: the token grammar, mention formatting, directory-scoped listing,
//! the ranked global search, the containment refusals, and the `fileReferences.list` wire method.
//!
//! # Why the expected values are literal
//!
//! Every expectation below was read off the upstream implementation running against the same
//! fixture (a Node probe over `file-reference/lib/types/grammar.js` and
//! `file-reference-local/lib/types/search.js`), not derived from this port's own output. A test that
//! asserted what the port happens to do would pin the port's bugs.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than
//! a copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}` — including
//! the ones the module under test makes, which is why the module itself spells them that way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/api_tools_gap.rs"]
mod api_tools_gap;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{CancellationToken, RemoteCall, RemoteError, RemoteRegistry};
use serde_json::json;

use api_tools_gap::{
    ActiveToken, Candidate, Config, ConfigError, DEFAULT_EXCLUDED_DIRECTORIES, DEFAULT_MAX_ENTRIES,
    DEFAULT_MAX_RESULTS, Kind, LIST_METHOD, ListRequest, NAMESPACE, WorkspaceFileSearch,
    active_at_token, format_file_mention, list, parse_request, register_file_reference_methods,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// A scratch root, unique per call.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-apitools-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// Writes `path` under `root`, creating parents.
fn write(root: &Path, path: &str) {
    let target = root.join(path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).expect("parents");
    }
    std::fs::write(target, "x").expect("written");
}

/// The tree every search expectation below was captured against.
///
/// Deliberately holds all three shapes the index must get right: a directory that also matches a
/// file name (`src` vs `src/main.rs`), two files that tie on every rank but length (`a-b.md`,
/// `ab.md`), and three trees the default exclusions must keep out of the answer.
fn fixture(name: &str) -> PathBuf {
    let root = scratch(name);
    for path in [
        "src/main.rs",
        "src/mod.rs",
        "src/deep/mod.rs",
        "docs/readme.md",
        "README.md",
        "a-b.md",
        "ab.md",
        ".hidden.md",
        "main.rs",
        "node_modules/x/index.js",
        ".git/config",
        "dist/bundle.js",
    ] {
        write(&root, path);
    }
    root
}

fn search(root: &PathBuf) -> WorkspaceFileSearch {
    WorkspaceFileSearch::new(root, Config::default()).expect("valid config")
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

/// The `(path, kind)` pairs of a candidate list, as upstream's JSON compares.
fn pairs(candidates: &[Candidate]) -> Vec<(String, &'static str)> {
    candidates
        .iter()
        .map(|candidate| (candidate.path.clone(), candidate.kind.as_str()))
        .collect()
}

// ---- the `@` token grammar ----------------------------------------------------------------------

#[test]
fn a_plain_token_is_the_text_from_at_to_the_caret() {
    assert_eq!(
        active_at_token("hi @src/ma", 10),
        Some(ActiveToken {
            prefix: "@src/ma".to_owned(),
            query: "src/ma".to_owned(),
            quoted: false,
        })
    );
    // A bare `@` is a token with an empty query: that is the keystroke that opens the popup.
    assert_eq!(
        active_at_token("hi @", 4),
        Some(ActiveToken {
            prefix: "@".to_owned(),
            query: String::new(),
            quoted: false,
        })
    );
}

#[test]
fn an_at_sign_glued_to_another_token_is_not_a_completion_trigger() {
    // The `(?:^|\s)` separator is the whole rule: `a@b` is an address, `a @b` is a token.
    assert_eq!(active_at_token("a@b", 3), None);
    assert_eq!(active_at_token("x@", 2), None);
    assert_eq!(
        active_at_token("a @b", 4).map(|token| token.query),
        Some("b".to_owned())
    );
}

#[test]
fn every_javascript_whitespace_character_separates_a_token() {
    for separator in [
        '\u{0009}', '\u{000a}', '\u{000b}', '\u{000c}', '\u{000d}', '\u{0020}', '\u{00a0}',
        '\u{1680}', '\u{2000}', '\u{2009}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
        '\u{feff}',
    ] {
        let line = format!("{separator}@a");
        assert_eq!(
            active_at_token(&line, 3).map(|token| token.query),
            Some("a".to_owned()),
            "U+{:04X} must separate a token",
            u32::from(separator)
        );
    }
}

#[test]
fn a_separator_javascript_does_not_accept_does_not_start_a_token() {
    // U+0085 (NEL) and the C0 separators are whitespace to Rust's `char::is_whitespace` but not to
    // JavaScript's `\s`. Using the Rust notion would open a completion where the real client does
    // not, so the narrower set is the one asserted.
    for separator in ['\u{0085}', '\u{001c}', '\u{001f}'] {
        let line = format!("{separator}@a");
        assert_eq!(
            active_at_token(&line, 3),
            None,
            "U+{:04X} must not separate a token",
            u32::from(separator)
        );
    }
}

#[test]
fn the_quoted_form_is_tried_before_the_plain_one() {
    // The plain pattern also matches inside a quoted token and would stop at the space, truncating
    // `@"src/my dir` to `@"src/my`. Order is the rule.
    assert_eq!(
        active_at_token("x @\"a b", 7),
        Some(ActiveToken {
            prefix: "@\"a b".to_owned(),
            query: "a b".to_owned(),
            quoted: true,
        })
    );
    assert_eq!(
        active_at_token("@\"", 2),
        Some(ActiveToken {
            prefix: "@\"".to_owned(),
            query: String::new(),
            quoted: true,
        })
    );
}

#[test]
fn a_closed_quote_ends_the_token() {
    // With the closing quote typed, neither pattern runs to the caret, so there is no open token.
    assert_eq!(active_at_token("@\"a b\"", 6), None);
    // A quote inside a plain token disqualifies the quoted form but not the plain one.
    assert_eq!(
        active_at_token("@a@b", 4).map(|token| (token.query, token.quoted)),
        Some(("a@b".to_owned(), false))
    );
    assert_eq!(
        active_at_token("@\"a\"b", 5).map(|token| (token.query, token.quoted)),
        Some(("\"a\"b".to_owned(), false))
    );
}

#[test]
fn a_space_after_a_plain_token_closes_it() {
    assert_eq!(active_at_token("@a ", 3), None);
    assert_eq!(active_at_token("@a b", 4), None);
    assert_eq!(
        active_at_token("@\"\"", 3).map(|token| token.quoted),
        Some(false)
    );
}

#[test]
fn the_caret_is_a_character_index_not_a_byte_index() {
    // A multi-byte path must not be split. `@源` is two characters, so column 2 is the whole token.
    let token = active_at_token("@源", 2).expect("token");
    assert_eq!(token.query, "源");
    assert_eq!(token.prefix, "@源");
    // At column 1 only the `@` has been typed.
    assert_eq!(
        active_at_token("@源", 1).map(|token| token.query),
        Some(String::new())
    );
}

// ---- mention formatting -------------------------------------------------------------------------

#[test]
fn a_plain_path_is_mentioned_without_quotes() {
    assert_eq!(
        format_file_mention(&Candidate::new("ok", Kind::File), false),
        Some("@ok".to_owned())
    );
    // A directory gains its trailing slash; a file does not.
    assert_eq!(
        format_file_mention(&Candidate::new("src", Kind::Directory), false),
        Some("@src/".to_owned())
    );
}

#[test]
fn whitespace_forces_the_quoted_form() {
    assert_eq!(
        format_file_mention(&Candidate::new("a b", Kind::File), false),
        Some("@\"a b\"".to_owned())
    );
    // A quoted directory keeps its quote open after the slash, so completion can descend.
    assert_eq!(
        format_file_mention(&Candidate::new("a b", Kind::Directory), false),
        Some("@\"a b/".to_owned())
    );
}

#[test]
fn preserve_quote_quotes_a_path_that_would_not_need_it() {
    assert_eq!(
        format_file_mention(&Candidate::new("ok", Kind::File), true),
        Some("@\"ok\"".to_owned())
    );
    assert_eq!(
        format_file_mention(&Candidate::new("src", Kind::Directory), true),
        Some("@\"src/".to_owned())
    );
}

#[test]
fn a_path_the_grammar_cannot_represent_is_refused() {
    for path in [
        "tab\tx",
        "nl\nx",
        "del\u{007f}x",
        "c1\u{009f}x",
        "nul\u{0000}x",
        "q\"q",
    ] {
        assert_eq!(
            format_file_mention(&Candidate::new(path, Kind::File), false),
            None,
            "{path:?} must be refused"
        );
        assert_eq!(
            format_file_mention(&Candidate::new(path, Kind::File), true),
            None,
            "{path:?} must be refused even when quoted"
        );
    }
    // Non-breaking space is whitespace, so it quotes rather than refuses: it is representable.
    assert_eq!(
        format_file_mention(&Candidate::new("a\u{00a0}b", Kind::File), false),
        Some("@\"a\u{00a0}b\"".to_owned())
    );
}

// ---- config -------------------------------------------------------------------------------------

#[test]
fn the_defaults_are_upstreams_numbers_and_exclusions() {
    let config = Config::default();
    assert_eq!(config.max_results, DEFAULT_MAX_RESULTS);
    assert_eq!(config.max_entries, DEFAULT_MAX_ENTRIES);
    assert_eq!(DEFAULT_MAX_RESULTS, 20);
    assert_eq!(DEFAULT_MAX_ENTRIES, 50_000);
    assert_eq!(config.excluded_directories.len(), 15);
    assert_eq!(DEFAULT_EXCLUDED_DIRECTORIES[0], ".git");
    // `lib` is deliberately absent: Ruby gems and many npm packages keep sources there.
    assert!(!config.excludes("lib"));
    assert!(config.excludes("node_modules"));
    // The index keeps the config it was built with, so a caller can read back what it passed.
    let search = WorkspaceFileSearch::new("/tmp", config.clone()).expect("valid");
    assert_eq!(search.config(), &config);
}

#[test]
fn a_zero_limit_is_refused_with_upstreams_message() {
    let error = WorkspaceFileSearch::new(
        "/tmp",
        Config {
            max_results: 0,
            ..Config::default()
        },
    )
    .expect_err("zero results is refused");
    assert_eq!(error, ConfigError::MaxResults);
    assert_eq!(
        error.message(),
        "file search maxResults must be a positive safe integer"
    );

    let error = WorkspaceFileSearch::new(
        "/tmp",
        Config {
            max_entries: 0,
            ..Config::default()
        },
    )
    .expect_err("zero entries is refused");
    assert_eq!(error, ConfigError::MaxEntries);
    assert_eq!(
        error.message(),
        "file search maxEntries must be a positive safe integer"
    );
}

#[test]
fn an_exclusion_that_is_a_path_rather_than_a_basename_is_refused() {
    for bad in ["a/b", "a\\b", ""] {
        let error = WorkspaceFileSearch::new(
            "/tmp",
            Config {
                excluded_directories: vec![bad.to_owned()],
                ..Config::default()
            },
        )
        .expect_err("refused");
        assert_eq!(
            error,
            ConfigError::ExcludedDirectory {
                name: bad.to_owned()
            }
        );
        assert_eq!(
            error.message(),
            "file search excludedDirectories entries must be non-empty directory basenames"
        );
    }
}

// ---- directory-scoped listing -------------------------------------------------------------------

#[test]
fn the_empty_query_lists_the_root_with_directories_first() {
    let root = fixture("root");
    let candidates = search(&root).list("", &cancel());
    assert_eq!(
        pairs(&candidates),
        vec![
            ("docs".to_owned(), "directory"),
            ("src".to_owned(), "directory"),
            ("README.md".to_owned(), "file"),
            ("a-b.md".to_owned(), "file"),
            ("ab.md".to_owned(), "file"),
            ("main.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn a_trailing_slash_lists_that_directory() {
    let root = fixture("dir");
    let candidates = search(&root).list("src/", &cancel());
    assert_eq!(
        pairs(&candidates),
        vec![
            ("src/deep".to_owned(), "directory"),
            ("src/main.rs".to_owned(), "file"),
            ("src/mod.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn a_directory_listing_filters_by_the_text_after_the_slash() {
    let root = fixture("filter");
    assert_eq!(
        pairs(&search(&root).list("src/de", &cancel())),
        vec![("src/deep".to_owned(), "directory")]
    );
    // `deep/` descends one more level, and the display path keeps every slash the query had.
    assert_eq!(
        pairs(&search(&root).list("src/deep/", &cancel())),
        vec![("src/deep/mod.rs".to_owned(), "file")]
    );
}

#[test]
fn excluded_and_hidden_entries_are_absent_from_a_listing() {
    let root = fixture("excluded");
    // `node_modules` and `dist` are excluded by basename, so neither is offered nor descended.
    assert!(search(&root).list("node_modules/", &cancel()).is_empty());
    assert!(search(&root).list("dist/", &cancel()).is_empty());
    assert!(search(&root).list(".git/", &cancel()).is_empty());
    // `.hidden.md` is absent from the root listing because the query does not ask for dot files.
    assert!(
        !pairs(&search(&root).list("", &cancel()))
            .iter()
            .any(|(path, _)| path == ".hidden.md")
    );
    // ... and present when the fragment asks for them.
    assert_eq!(
        pairs(&search(&root).list(".hid", &cancel())),
        vec![(".hidden.md".to_owned(), "file")]
    );
}

#[test]
fn a_dot_segment_in_the_query_lists_the_hidden_entries_of_its_directory() {
    // Upstream answers `src/.` with the non-hidden entries: the fragment is `.`, which starts with a
    // dot, so the hidden filter stands down — the directory holds no hidden files, so the answer is
    // the same as `src/` minus the directory (`.`, `..` are never offered).
    let root = fixture("dotted");
    let candidates = search(&root).list("src/.", &cancel());
    assert_eq!(
        pairs(&candidates),
        vec![
            ("src/mod.rs".to_owned(), "file"),
            ("src/main.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn the_display_path_is_whatever_the_query_spelled() {
    let root = fixture("display");
    // `./` and `src//` are not normalised away: the query's own spelling is what the client gets
    // back, because the client replaces the token it typed.
    assert_eq!(
        pairs(&search(&root).list("./src/", &cancel())),
        vec![
            ("./src/deep".to_owned(), "directory"),
            ("./src/main.rs".to_owned(), "file"),
            ("./src/mod.rs".to_owned(), "file"),
        ]
    );
    assert_eq!(
        pairs(&search(&root).list("src//", &cancel())),
        vec![
            ("src//deep".to_owned(), "directory"),
            ("src//main.rs".to_owned(), "file"),
            ("src//mod.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn windows_separators_are_normalised_before_the_mode_switch() {
    let root = fixture("backslash");
    // `src\ma` must be a directory listing of `src`, not a global search for the literal `src\ma`.
    assert_eq!(
        pairs(&search(&root).list("src\\ma", &cancel())),
        vec![("src/main.rs".to_owned(), "file")]
    );
    assert_eq!(
        pairs(&search(&root).list("src\\", &cancel())),
        vec![
            ("src/deep".to_owned(), "directory"),
            ("src/main.rs".to_owned(), "file"),
            ("src/mod.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn a_path_that_climbs_out_of_the_root_is_refused() {
    let root = fixture("escape");
    let search = search(&root);
    for query in ["../", "../etc/", "/etc/", "/tmp/"] {
        assert!(
            search.list(query, &cancel()).is_empty(),
            "{query} must not be answered"
        );
    }
}

#[test]
fn a_dot_segment_inside_the_root_is_resolved_rather_than_refused() {
    // `src/../` names the root by a longer spelling. Upstream answers it, and the display path keeps
    // the `..` the client typed.
    let root = fixture("parent");
    assert_eq!(
        pairs(&search(&root).list("src/../", &cancel())),
        vec![
            ("src/../docs".to_owned(), "directory"),
            ("src/../src".to_owned(), "directory"),
            ("src/../README.md".to_owned(), "file"),
            ("src/../a-b.md".to_owned(), "file"),
            ("src/../ab.md".to_owned(), "file"),
            ("src/../main.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn a_sibling_directory_sharing_the_roots_prefix_is_outside_the_root() {
    // `strip_prefix` on text is not a path operation: `/tmp/ws` is a prefix of `/tmp/ws2`. Both a
    // lexical climb and an absolute query that lands on the sibling must be refused.
    let parent = scratch("sibling");
    let root = parent.join("ws");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::create_dir_all(parent.join("ws2")).expect("sibling");
    std::fs::write(parent.join("ws2/leak.txt"), "x").expect("written");
    std::fs::write(root.join("inside.txt"), "x").expect("written");

    let search = search(&root);
    let sibling_query = format!("{}/", parent.join("ws2").display());
    assert!(
        search.list(&sibling_query, &cancel()).is_empty(),
        "the sibling directory must not be listed"
    );
    assert_eq!(
        pairs(&search.list("", &cancel())),
        vec![("inside.txt".to_owned(), "file")]
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_inside_the_root_is_not_descended() {
    // A lexical containment check cannot see this: the link is inside the root and its target is
    // not. `lstat` is what refuses it, both in the listing and in the index.
    let root = scratch("symlink");
    std::fs::create_dir_all(root.join("real")).expect("real");
    std::fs::write(root.join("real/inside.txt"), "x").expect("written");
    std::os::unix::fs::symlink(root.join("real"), root.join("linkdir")).expect("symlink");
    std::os::unix::fs::symlink("/etc", root.join("escape")).expect("symlink");

    let search = search(&root);
    assert!(
        search.list("linkdir/", &cancel()).is_empty(),
        "a symlinked directory must not be listed"
    );
    assert!(
        search.list("escape/", &cancel()).is_empty(),
        "a symlink out of the root must not be listed"
    );
    // The index sees the link as neither a file nor a directory, so it is absent from a bare query
    // as well.
    let bare = pairs(&search.list("link", &cancel()));
    assert!(
        bare.iter().all(|(path, _)| !path.contains("linkdir")),
        "{bare:?}"
    );
}

// ---- the ranked global search -------------------------------------------------------------------

#[test]
fn a_bare_query_ranks_basename_matches_above_path_matches() {
    let root = fixture("rank");
    // `main` matches the basename of `main.rs` exactly and is a prefix of `src/main.rs`'s basename;
    // the shorter path comes first because both land in the same band.
    assert_eq!(
        pairs(&search(&root).list("main", &cancel())),
        vec![
            ("main.rs".to_owned(), "file"),
            ("src/main.rs".to_owned(), "file"),
        ]
    );
    assert_eq!(
        pairs(&search(&root).list("ma", &cancel())),
        vec![
            ("main.rs".to_owned(), "file"),
            ("src/main.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn a_directory_wins_a_tie_and_sorts_before_a_file() {
    let root = fixture("ties");
    // `doc` prefixes both `docs` (directory) and `docs/readme.md`'s basename is `readme.md`, which
    // does not contain it, so the second match is a whole-path match in a lower band.
    assert_eq!(
        pairs(&search(&root).list("doc", &cancel())),
        vec![
            ("docs".to_owned(), "directory"),
            ("docs/readme.md".to_owned(), "file"),
        ]
    );
}

#[test]
fn equal_scores_break_by_length_then_by_bytes() {
    let root = fixture("length");
    // `md` matches three basenames as a substring (`a-b.md`, `ab.md`, `README.md` is `readme.md`,
    // which contains `md` only at the end) — all in the 700 band, so length decides, then bytes.
    // `a-b.md` and `ab.md` are both six bytes, so byte order puts `a-b.md` first (`-` < `b`).
    assert_eq!(
        pairs(&search(&root).list("md", &cancel())),
        vec![
            ("ab.md".to_owned(), "file"),
            ("a-b.md".to_owned(), "file"),
            ("README.md".to_owned(), "file"),
            ("docs/readme.md".to_owned(), "file"),
            ("src/mod.rs".to_owned(), "file"),
            ("src/deep/mod.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn a_subsequence_match_is_offered_last_rather_than_dropped() {
    let root = fixture("subsequence");
    // `sr` is a subsequence of `src/...` and of nothing else, and `src` itself is a prefix match, so
    // it leads.
    let ranked = pairs(&search(&root).list("sr", &cancel()));
    assert_eq!(ranked.first(), Some(&("src".to_owned(), "directory")));
    assert!(
        ranked.contains(&("src/deep/mod.rs".to_owned(), "file")),
        "{ranked:?}"
    );
}

#[test]
fn a_bare_query_hides_dot_paths_unless_the_query_mentions_one() {
    let root = fixture("hidden");
    let search = search(&root);
    // `hid` matches only `.hidden.md`, and `.hidden.md` is a dot-segment path, so the answer is
    // empty: the dot rule withholds it even though the name matches. Verified against upstream,
    // where `list('hid')` is `[]` and `list('.hid')` is the one candidate.
    assert!(search.list("hid", &cancel()).is_empty());
    assert_eq!(
        pairs(&search.list(".hid", &cancel())),
        vec![(".hidden.md".to_owned(), "file")]
    );
    assert_eq!(
        pairs(&search.list(".hidden", &cancel())),
        vec![(".hidden.md".to_owned(), "file")]
    );
}

#[test]
fn the_result_limit_is_applied_after_ranking() {
    let root = fixture("limit");
    let search = WorkspaceFileSearch::new(
        &root,
        Config {
            max_results: 2,
            ..Config::default()
        },
    )
    .expect("valid");
    let ranked = pairs(&search.list("rs", &cancel()));
    assert_eq!(
        ranked,
        vec![
            ("main.rs".to_owned(), "file"),
            ("src/mod.rs".to_owned(), "file"),
        ]
    );
}

#[test]
fn the_entry_bound_can_leave_the_index_empty_at_the_root() {
    let root = fixture("entries");
    // `maxEntries` below the number of root entries: the root's own listing fills the budget before
    // any subdirectory is queued, and a bare query has nothing to rank against. This is upstream's
    // documented behaviour, not an accident of this port.
    let search = WorkspaceFileSearch::new(
        &root,
        Config {
            max_entries: 3,
            ..Config::default()
        },
    )
    .expect("valid");
    assert!(search.list("rs", &cancel()).is_empty());
    // A directory-scoped query does not consult the index at all, so it still answers.
    assert!(!search.list("src/", &cancel()).is_empty());
}

#[test]
fn a_missing_root_yields_nothing_rather_than_an_error() {
    let missing = std::env::temp_dir().join(format!(
        "girder-apitools-absent-{}-none",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&missing);
    let search = WorkspaceFileSearch::new(&missing, Config::default()).expect("valid config");
    assert!(search.list("rs", &cancel()).is_empty());
    assert!(search.list("src/", &cancel()).is_empty());
    assert!(search.list("", &cancel()).is_empty());
}

#[test]
fn a_cancelled_call_returns_nothing() {
    let root = fixture("cancel");
    let search = search(&root);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(search.list("", &cancelled).is_empty());
    assert!(search.list("rs", &cancelled).is_empty());
    assert!(search.list("src/", &cancelled).is_empty());
}

// ---- the wire method ----------------------------------------------------------------------------

#[test]
fn the_request_reads_the_wire_field_names() {
    let request = parse_request(&json!({"agentId": "s1", "query": "src/"})).expect("parsed");
    assert_eq!(
        request,
        ListRequest {
            agent_id: "s1".to_owned(),
            query: "src/".to_owned(),
        }
    );
    // An absent query is the empty query, which lists the root.
    let request = parse_request(&json!({"agentId": "s1"})).expect("parsed");
    assert_eq!(request.query, "");
}

#[test]
fn a_malformed_request_names_the_field_it_rejected() {
    let error = parse_request(&json!({})).expect_err("no agentId");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(
        error.to_string().contains("`agentId` is required"),
        "{error}"
    );

    let error = parse_request(&json!({"agentId": ""})).expect_err("empty agentId");
    assert!(error.to_string().contains("non-empty"), "{error}");

    let error = parse_request(&json!({"agentId": 7})).expect_err("numeric agentId");
    assert!(error.to_string().contains("must be a string"), "{error}");

    let error = parse_request(&json!({"agentId": "s1", "query": 3})).expect_err("numeric query");
    assert!(
        error.to_string().contains("`query` must be a string"),
        "{error}"
    );

    let error = parse_request(&json!("nope")).expect_err("not an object");
    assert!(
        error.to_string().contains("must be a JSON object"),
        "{error}"
    );
}

#[test]
fn the_method_lists_candidates_for_a_known_root() {
    let root = fixture("method");
    let candidates = list(
        &root,
        &ListRequest {
            agent_id: "s1".to_owned(),
            query: "src/".to_owned(),
        },
    )
    .expect("listed");
    assert_eq!(
        pairs(&candidates),
        vec![
            ("src/deep".to_owned(), "directory"),
            ("src/main.rs".to_owned(), "file"),
            ("src/mod.rs".to_owned(), "file"),
        ]
    );
}

fn registry(root: PathBuf) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    let root = Arc::new(root);
    register_file_reference_methods(&mut registry, move |_agent: &str| Some((*root).clone()));
    registry
}

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

#[tokio::test]
async fn the_registered_method_answers_with_the_upstream_wire_shape() {
    let root = fixture("wire");
    let registry = registry(root);
    let answer = registry
        .dispatch(
            NAMESPACE,
            LIST_METHOD,
            call(json!({"agentId": "s1", "query": "src/ma"})),
        )
        .await
        .expect("answered");
    assert_eq!(
        answer,
        json!({"candidates": [{"path": "src/main.rs", "kind": "file"}]})
    );
}

#[tokio::test]
async fn a_root_the_host_does_not_know_answers_empty_rather_than_failing() {
    let mut registry = RemoteRegistry::new();
    register_file_reference_methods(&mut registry, |_agent: &str| None);
    let answer = registry
        .dispatch(
            NAMESPACE,
            LIST_METHOD,
            call(json!({"agentId": "gone", "query": "src/"})),
        )
        .await
        .expect("answered");
    assert_eq!(answer, json!({"candidates": []}));
}

#[tokio::test]
async fn a_malformed_call_is_invalid_arguments_not_an_unknown_method() {
    let registry = registry(fixture("malformed"));
    let error = registry
        .dispatch(NAMESPACE, LIST_METHOD, call(json!({"query": "src/"})))
        .await
        .expect_err("refused");
    match error {
        RemoteError::InvalidArguments {
            namespace, method, ..
        } => {
            assert_eq!(namespace, NAMESPACE);
            assert_eq!(method, LIST_METHOD);
        }
        other => panic!("expected InvalidArguments, got {other}"),
    }
}

#[tokio::test]
async fn the_method_is_a_declared_read() {
    // A read-only deployment keeps `@` completion: it starts nothing and writes nothing.
    let root = fixture("policy");
    let registry = registry(root).with_policy(Arc::new(girder_core::ReadOnly));
    let answer = registry
        .dispatch(
            NAMESPACE,
            LIST_METHOD,
            call(json!({"agentId": "s1", "query": "src/"})),
        )
        .await
        .expect("allowed");
    assert!(
        answer["candidates"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
    );
}
