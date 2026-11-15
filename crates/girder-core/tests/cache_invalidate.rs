//! Tests for inferring and applying a tool call's effect on the read cache.
//!
//! The tests are weighted toward the two ways this module can be wrong in the dangerous direction:
//!
//! - **missing a mutation**, which leaves a pre-write read live in the cache — the model then edits
//!   a file that no longer has that content, and no error is ever raised;
//! - **missing a spelling**, which is the same failure with a different cause: `/a/b` and `/a/b/` are
//!   one file, so invalidation that compares raw strings silently keeps the stale entry.
//!
//! `bash` is therefore tested as a first-class case rather than as an edge: a shell command can write
//! anywhere, so the only correct answer is to drop everything. The rest pin the cheap cases (a
//! read-only tool does nothing), the multi-path case (`apply_patch`), and the conservative default
//! for a tool the module has never heard of.
//!
//! Included by path rather than through the crate's re-exports, so the module is tested exactly as
//! written and its own doc examples do not depend on the crate root being wired up.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/cache_invalidate.rs"]
mod cache_invalidate;

use cache_invalidate::{Effect, apply, effect, normalized_path};
use girder_core::{ToolCache, ToolCacheEntry, ToolCacheKey};

// ---- fixtures ----------------------------------------------------------------------------------

/// A successful cached read of `path`, last modified at `mtime`.
fn cache_with(path: &str, mtime: u64) -> ToolCache {
    let mut cache = ToolCache::new(8);
    let key = ToolCacheKey::of("read", &format!("{{\"path\":\"{path}\"}}"));
    let entry = ToolCacheEntry {
        text: format!("contents of {path}"),
        is_error: false,
        source_mtime_ms: Some(mtime),
    };
    cache.put(key, entry);
    cache
}

/// Whether `path` can still be served from `cache` at `mtime`.
///
/// A hit is exactly the failure this module prevents, so the test reads as "is the stale answer
/// still there".
fn still_cached(cache: &mut ToolCache, path: &str, mtime: u64) -> bool {
    let key = ToolCacheKey::of("read", &format!("{{\"path\":\"{path}\"}}"));
    cache.get(&key, Some(mtime)).is_some()
}

/// The paths of an `Invalidate` effect, or a panic naming the variant that was returned instead.
fn paths_of(effect: &Effect) -> &[String] {
    match effect {
        Effect::Invalidate { paths } => paths,
        other => panic!("expected Invalidate, got {other:?}"),
    }
}

// ---- write invalidates by path -----------------------------------------------------------------

#[test]
fn a_write_invalidates_the_path_it_names() {
    // The reason `Effect` exists at all: our own `write` may leave the mtime untouched on a
    // coarse-grained filesystem, so the mtime gate alone would serve the pre-write text.
    let effect = effect("write", r#"{"path":"/work/a.txt","content":"new"}"#);
    assert_eq!(
        effect,
        Effect::Invalidate {
            paths: vec!["/work/a.txt".to_owned()]
        }
    );

    let mut cache = cache_with("/work/a.txt", 1_000);
    assert!(still_cached(&mut cache, "/work/a.txt", 1_000));
    apply(&effect, &mut cache);
    assert!(
        !still_cached(&mut cache, "/work/a.txt", 1_000),
        "a write must not leave its own path cached at the old contents"
    );
}

#[test]
fn an_edit_invalidates_the_path_it_names() {
    // `edit` rewrites one occurrence in the same file, so it is a writer with a path just like
    // `write` -- and a same-tick edit is exactly the case the mtime gate cannot see.
    let effect = effect(
        "edit",
        r#"{"path":"/work/src/main.rs","old":"a","new":"b"}"#,
    );
    assert_eq!(
        paths_of(&effect),
        ["/work/src/main.rs".to_owned()],
        "edit names its file in `path`, like write"
    );

    let mut cache = cache_with("/work/src/main.rs", 2_000);
    apply(&effect, &mut cache);
    assert!(!still_cached(&mut cache, "/work/src/main.rs", 2_000));
}

#[test]
fn write_paths_are_normalized_so_one_file_has_one_identity() {
    // Every one of these names the same file. A raw-string comparison would miss all but the first
    // and leave the stale entry live, which is the same failure as not invalidating at all.
    for spelling in [
        "/work/a.txt",
        "/work/a.txt/",
        "/work//a.txt",
        "/work/./a.txt",
        "/work/b/../a.txt",
    ] {
        let effect = effect(
            "write",
            &format!("{{\"path\":\"{spelling}\",\"content\":\"new\"}}"),
        );
        assert_eq!(
            paths_of(&effect),
            ["/work/a.txt".to_owned()],
            "{spelling} must fold onto the same path as /work/a.txt"
        );
    }
}

#[test]
fn a_trailing_separator_does_not_defeat_invalidation() {
    // The end-to-end form of the normalization rule, through the cache: the read was keyed on the
    // bare spelling and the write names the trailing-slash one.
    let mut cache = cache_with("/work/a.txt", 1_000);
    let effect = effect("write", r#"{"path":"/work/a.txt/","content":"new"}"#);
    apply(&effect, &mut cache);
    assert!(
        !still_cached(&mut cache, "/work/a.txt", 1_000),
        "a trailing separator must not leave the entry live"
    );
}

#[test]
fn a_relative_path_is_folded_but_stays_relative() {
    // Relative and absolute spellings cannot be reconciled without a workspace, and guessing one
    // would be worse than folding correctly. What is guaranteed is that the folding is consistent.
    assert_eq!(normalized_path("a/./b/../c"), "a/c");
    assert_eq!(normalized_path("./a/b/"), "a/b");
    assert_eq!(normalized_path("/a/../../b"), "/b");
}

// ---- bash invalidates everything ---------------------------------------------------------------

#[test]
fn bash_invalidates_the_whole_cache() {
    // The core case. A shell command can write any file -- the path may be built at run time, a
    // called binary may write, and a redirect is an ordinary token -- so "these paths" does not
    // exist for it, and any finite list would be a guess with a stale entry behind it.
    let effect = effect("bash", r#"{"command":"true"}"#);
    assert_eq!(effect, Effect::InvalidateAll);

    let mut cache = cache_with("/work/a.txt", 1_000);
    let other = ToolCacheKey::of("read", r#"{"path":"/work/b.txt"}"#);
    cache.put(
        other,
        ToolCacheEntry {
            text: "b".to_owned(),
            is_error: false,
            source_mtime_ms: Some(1_000),
        },
    );
    assert_eq!(cache.len(), 2);

    apply(&effect, &mut cache);
    assert!(cache.is_empty(), "bash must drop every cached read");
}

#[test]
fn bash_is_conservative_even_for_an_obviously_harmless_command() {
    // Deliberately not special-cased: reading `ls` as harmless would make the answer depend on
    // parsing a shell, and the moment that parse is wrong the cache serves text that was deleted.
    for command in [
        r#"{"command":"ls"}"#,
        r#"{"command":"git status"}"#,
        r#"{"command":"echo hi"}"#,
        // Malformed arguments must not be read as "wrote nothing".
        "",
        "not json",
    ] {
        assert_eq!(
            effect("bash", command),
            Effect::InvalidateAll,
            "bash must invalidate everything for {command:?}"
        );
    }
}

// ---- read-only tools do nothing ----------------------------------------------------------------

#[test]
fn a_read_only_tool_has_no_effect() {
    // Nothing to do, and the cache keeps the entry: a miss here would defeat the feature.
    for (tool, arguments) in [
        ("read", r#"{"path":"/work/a.txt"}"#),
        ("glob", r#"{"pattern":"**/*.rs"}"#),
        ("grep", r#"{"pattern":"fn main"}"#),
        ("lsp", r#"{"path":"/work/a.rs"}"#),
        ("web_fetch", r#"{"url":"https://example.invalid"}"#),
    ] {
        assert_eq!(
            effect(tool, arguments),
            Effect::None,
            "{tool} cannot change a file"
        );
    }

    let mut cache = cache_with("/work/a.txt", 1_000);
    apply(&Effect::None, &mut cache);
    assert!(
        still_cached(&mut cache, "/work/a.txt", 1_000),
        "a read-only call must not evict anything"
    );
}

#[test]
fn an_unknown_tool_is_treated_as_a_writer() {
    // The safe default. A tool this module has not been taught about is not evidence of a read-only
    // one, and guessing "read-only" would leave a stale entry live after whatever it did.
    for tool in [
        "",
        "some_new_tool",
        "terminal_start",
        "subagent",
        "mcp__fs__write",
    ] {
        assert_eq!(
            effect(tool, r#"{"path":"/work/a.txt"}"#),
            Effect::InvalidateAll,
            "{tool:?} must be assumed to have written"
        );
    }
}

#[test]
fn a_writer_with_unreadable_arguments_invalidates_everything() {
    // Absence of evidence is not evidence of absence: a `write` whose arguments did not parse may
    // still have run, so the whole cache goes rather than an empty path list.
    for arguments in ["", "not json", "{}", r#"{"path":""}"#, r#"{"content":"x"}"#] {
        assert_eq!(
            effect("write", arguments),
            Effect::InvalidateAll,
            "unreadable arguments ({arguments:?}) must not be read as `wrote nothing`"
        );
    }
}

// ---- multi-path: apply_patch -------------------------------------------------------------------

#[test]
fn apply_patch_invalidates_every_file_it_names() {
    // A patch is one call that can touch several files; taking only the first would leave the rest
    // cached at their pre-patch contents.
    let arguments = serde_json::json!({
        "patch": "*** Begin Patch\n\
                  *** Update File: src/a.rs\n\
                  @@\n-old\n+new\n\
                  *** Add File: src/b.rs\n\
                  +fresh\n\
                  *** Delete File: src/c.rs\n\
                  *** End Patch"
    })
    .to_string();

    let effect = effect("apply_patch", &arguments);
    assert_eq!(
        paths_of(&effect),
        [
            "src/a.rs".to_owned(),
            "src/b.rs".to_owned(),
            "src/c.rs".to_owned()
        ]
    );

    let mut cache = cache_with("src/a.rs", 1_000);
    apply(&effect, &mut cache);
    assert!(!still_cached(&mut cache, "src/a.rs", 1_000));
}

#[test]
fn apply_patch_includes_the_destination_of_a_move() {
    // A move writes the destination as well as removing the source, so both ends are affected.
    let arguments = serde_json::json!({
        "patch": "*** Begin Patch\n\
                  *** Update File: src/old.rs\n\
                  *** Move to: src/new.rs\n\
                  @@\n-old\n+new\n\
                  *** End Patch"
    })
    .to_string();
    assert_eq!(
        paths_of(&effect("apply_patch", &arguments)),
        ["src/old.rs".to_owned(), "src/new.rs".to_owned()]
    );
}

#[test]
fn apply_patch_paths_are_normalized_and_deduplicated() {
    // The grammar lets one file be named twice, and a repeated or differently-spelled path must
    // still fold onto one entry.
    let arguments = serde_json::json!({
        "patch": "*** Begin Patch\n\
                  *** Update File: ./src/a.rs\n\
                  *** Update File: src/a.rs/\n\
                  *** Update File: src//a.rs\n\
                  *** End Patch"
    })
    .to_string();
    assert_eq!(
        paths_of(&effect("apply_patch", &arguments)),
        ["src/a.rs".to_owned()],
        "three spellings of one file are one path"
    );
}

#[test]
fn a_patch_with_no_file_marker_invalidates_everything() {
    // Nothing recoverable means nothing provable.
    for patch in ["", "*** Begin Patch\n*** End Patch", "not a patch"] {
        let arguments = serde_json::json!({ "patch": patch }).to_string();
        assert_eq!(
            effect("apply_patch", &arguments),
            Effect::InvalidateAll,
            "{patch:?} names no file, so nothing can be ruled out"
        );
    }
}

// ---- the other mutating tools ------------------------------------------------------------------

#[test]
fn delete_and_make_dir_invalidate_their_path() {
    // Removing a file leaves any cached read of it describing something that no longer exists.
    assert_eq!(
        paths_of(&effect("delete_path", r#"{"path":"/work/a.txt/"}"#)),
        ["/work/a.txt".to_owned()]
    );
    assert_eq!(
        paths_of(&effect("make_dir", r#"{"path":"/work/new/"}"#)),
        ["/work/new".to_owned()]
    );
}

#[test]
fn copy_and_move_invalidate_both_ends() {
    // The destination is written; for a move the source is also gone.
    for tool in ["copy_path", "move_path"] {
        assert_eq!(
            paths_of(&effect(
                tool,
                r#"{"source":"/work/a.txt","destination":"/work/b.txt"}"#
            )),
            ["/work/a.txt".to_owned(), "/work/b.txt".to_owned()],
            "{tool} affects both ends"
        );
    }
}

// ---- apply is total ----------------------------------------------------------------------------

#[test]
fn applying_an_effect_twice_is_harmless() {
    // `apply` runs after every call, including ones that already invalidated; it must not panic or
    // depend on the cache being non-empty.
    let mut cache = cache_with("/work/a.txt", 1_000);
    let effect = effect("write", r#"{"path":"/work/a.txt","content":"x"}"#);
    apply(&effect, &mut cache);
    apply(&effect, &mut cache);
    assert!(cache.is_empty());

    // And on an already-empty cache.
    apply(&Effect::InvalidateAll, &mut cache);
    assert!(cache.is_empty());
}

#[test]
fn invalidation_keeps_the_counters_a_session_reports() {
    // Counters are the evidence about how well caching worked; clearing entries must not erase them.
    let mut cache = cache_with("/work/a.txt", 1_000);
    assert!(still_cached(&mut cache, "/work/a.txt", 1_000));
    let before = cache.stats().hits;

    apply(&Effect::InvalidateAll, &mut cache);
    assert_eq!(
        cache.stats().hits,
        before,
        "a flush must not reset counters"
    );
}
