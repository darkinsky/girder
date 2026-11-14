//! Tests for the tool result cache.
//!
//! The tests are weighted toward the two failures that make a cache worse than no cache:
//!
//! - **a writer being cached**, which turns the second `write`/`edit`/`bash` call into a no-op that
//!   still reports success — silent data loss;
//! - **a stale file being served**, which puts text in the model's context that is no longer on disk.
//!
//! Both are gated in `put`/`get` rather than at the call site, so both are tested through the public
//! API only. The rest pins hit behaviour, capacity eviction by *last use* rather than insertion
//! order, clearing, and the counters.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the cache as written even before the crate root wires it in. The module depends on
//! nothing but the standard library, so it also builds outside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/tool_cache.rs"]
mod tool_cache;

use tool_cache::{Cache, Entry, Key, is_cacheable};

// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh key for a `read` of `path`, with the arguments spelled the way the model writes them.
fn read_key(path: &str) -> Key {
    Key::of("read", &format!("{{\"path\":\"{path}\"}}"))
}

/// A successful entry for a file last modified at `mtime`.
fn entry(text: &str, mtime: u64) -> Entry {
    Entry {
        text: text.to_owned(),
        is_error: false,
        source_mtime_ms: Some(mtime),
    }
}

// ---- which tools may be cached -----------------------------------------------------------------

#[test]
fn the_read_tool_is_cacheable() {
    assert!(is_cacheable("read"));
}

#[test]
fn writers_are_never_cacheable() {
    // The core safety property. A cached writer returns the first call's receipt without running:
    // the model believes the file changed when it did not, and the next turn builds on a fabrication.
    for tool in ["write", "edit", "apply_patch", "bash"] {
        assert!(!is_cacheable(tool), "`{tool}` must not be cacheable");
    }
}

#[test]
fn a_shell_tool_is_never_cacheable_even_under_another_name() {
    // `bash` output reports the clock, the process table, and the network; no mtime could ever
    // invalidate it. The other shell-shaped tools are process handles for the same reason.
    for tool in [
        "bash",
        "terminal_start",
        "terminal_send",
        "terminal_read",
        "job_start",
        "job_output",
        "job_list",
    ] {
        assert!(!is_cacheable(tool), "`{tool}` must not be cacheable");
    }
}

#[test]
fn tools_whose_source_is_not_one_file_are_not_cacheable() {
    // Read-only, but their output depends on a whole tree or a remote service. One file's mtime
    // cannot prove that no file was added, removed, renamed, or changed upstream.
    for tool in [
        "glob",
        "grep",
        "web_fetch",
        "web_search",
        "lsp",
        "session_search",
    ] {
        assert!(!is_cacheable(tool), "`{tool}` must not be cacheable");
    }
}

#[test]
fn an_unknown_tool_is_not_cacheable() {
    // The safe default: a tool nobody has classified might write.
    assert!(!is_cacheable("some_future_tool"));
    assert!(!is_cacheable(""));
}

#[test]
fn a_writer_is_refused_by_put_even_when_the_caller_forgets_to_ask() {
    // The gate is inside `put`, not only in `is_cacheable`, so a wiring mistake cannot smuggle a
    // writer in. This is the one test that would fail if the check moved to the call site.
    let mut cache = Cache::new(4);
    let key = Key::of("write", "{\"path\":\"a.txt\",\"content\":\"hello\"}");
    cache.put(
        key.clone(),
        Entry {
            text: "wrote a.txt".to_owned(),
            is_error: false,
            source_mtime_ms: None,
        },
    );

    assert!(
        cache.get(&key, None).is_none(),
        "the writer must not be stored"
    );
    assert!(cache.is_empty());
    assert_eq!(cache.stats().refused, 1);
}

#[test]
fn a_writer_key_cannot_read_another_entrys_result() {
    // Same arguments, different tool: the key must include the tool name, or a later `read` would
    // answer a `write` and vice versa.
    let mut cache = Cache::new(4);
    let args = "{\"path\":\"a.txt\"}";
    cache.put(
        Key::of("read", args),
        Entry {
            text: "file body".to_owned(),
            is_error: false,
            source_mtime_ms: Some(7),
        },
    );

    assert!(cache.get(&Key::of("write", args), Some(7)).is_none());
    assert_eq!(cache.len(), 1);
}

// ---- keys --------------------------------------------------------------------------------------

#[test]
fn different_arguments_are_different_keys() {
    assert_ne!(read_key("a.txt"), read_key("b.txt"));
    assert_eq!(read_key("a.txt"), read_key("a.txt"));
}

#[test]
fn key_encoding_cannot_be_confused_by_a_colon_or_a_brace() {
    // Length-prefixed, so a name and an argument that together look like another pair still differ.
    let one = Key::of("read", "{\"path\":\"a\"}");
    let two = Key::of("read{\"path\":", "\"a\"}");
    assert_ne!(one, two);
    assert_ne!(one.as_str(), two.as_str());
}

#[test]
fn the_key_recovers_its_tool_name() {
    let key = Key::of("read", "{}");
    assert!(key.as_str().contains("read"));
    // The recovered name drives the cacheability gate; a `read` key stays cacheable.
    let mut cache = Cache::new(2);
    cache.put(key.clone(), entry("body", 1));
    assert_eq!(cache.len(), 1);
}

// ---- hits --------------------------------------------------------------------------------------

#[test]
fn a_lookup_with_an_unchanged_mtime_hits() {
    let mut cache = Cache::new(4);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("line one", 1_000));

    let hit = cache.get(&key, Some(1_000)).expect("a fresh entry hits");
    assert_eq!(hit.text, "line one");
    assert!(!hit.is_error);
    assert_eq!(hit.source_mtime_ms, Some(1_000));
    assert_eq!(cache.stats().hits, 1);
    assert_eq!(cache.stats().misses, 0);
}

#[test]
fn a_lookup_of_something_never_stored_misses() {
    let mut cache = Cache::new(4);
    assert!(cache.get(&read_key("a.txt"), Some(1_000)).is_none());
    assert_eq!(cache.stats().misses, 1);
}

#[test]
fn an_error_result_is_kept_verbatim() {
    // A cached "cannot read that file" is still the answer to the same call. Flipping it to a
    // success on the second attempt would hide the failure.
    let mut cache = Cache::new(4);
    let key = read_key("missing.txt");
    cache.put(
        key.clone(),
        Entry {
            text: "cannot read `missing.txt`".to_owned(),
            is_error: true,
            source_mtime_ms: Some(5),
        },
    );

    let hit = cache.get(&key, Some(5)).expect("an error entry still hits");
    assert!(hit.is_error);
}

// ---- invalidation ------------------------------------------------------------------------------

#[test]
fn a_changed_mtime_invalidates_the_entry() {
    // The core safety property of the module: the file changed, so the stored text is a lie.
    let mut cache = Cache::new(4);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("old contents", 1_000));

    assert!(
        cache.get(&key, Some(2_000)).is_none(),
        "a newer mtime must not serve the old text"
    );
    assert_eq!(cache.stats().stale, 1);
    assert_eq!(cache.stats().hits, 0);
    assert_eq!(cache.stats().misses, 1);
}

#[test]
fn an_older_mtime_also_invalidates() {
    // A downgrade means the file was replaced by something older — a restore, a `mv`, a checkout.
    // Comparing with `>` instead of `!=` would serve the old contents.
    let mut cache = Cache::new(4);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("contents", 2_000));

    assert!(cache.get(&key, Some(1_000)).is_none());
    assert_eq!(cache.stats().stale, 1);
}

#[test]
fn a_deleted_file_invalidates_the_entry() {
    // The caller observes no mtime because the file is gone. `None` must not match `Some`.
    let mut cache = Cache::new(4);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("contents", 1_000));

    assert!(cache.get(&key, None).is_none());
    assert_eq!(cache.stats().stale, 1);
}

#[test]
fn an_invalidated_entry_is_dropped_rather_than_kept() {
    // Keeping it would leak a result that can never be served again and would evict a live entry
    // when the cache is full.
    let mut cache = Cache::new(4);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("contents", 1_000));
    assert_eq!(cache.len(), 1);

    assert!(cache.get(&key, Some(1_001)).is_none());
    assert_eq!(cache.len(), 0);
    assert!(cache.is_empty());
}

#[test]
fn rewriting_the_source_and_storing_again_serves_the_new_text() {
    // The normal edit-read-edit-read loop: stale is discarded, the new read is stored, and the text
    // the model sees follows the file.
    let mut cache = Cache::new(4);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("before", 1_000));
    assert!(cache.get(&key, Some(2_000)).is_none());

    cache.put(key.clone(), entry("after", 2_000));
    assert_eq!(
        cache.get(&key, Some(2_000)).expect("fresh read").text,
        "after"
    );
}

#[test]
fn invalidate_drops_an_entry_without_waiting_for_a_lookup() {
    // For a caller that knows it changed the file itself: on a coarse-grained filesystem the mtime
    // may not have moved, so the mtime comparison alone would still hit.
    let mut cache = Cache::new(4);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("contents", 1_000));

    assert!(cache.invalidate(&key));
    assert!(cache.get(&key, Some(1_000)).is_none());
    assert!(!cache.invalidate(&key), "a second invalidate finds nothing");
}

// ---- capacity ----------------------------------------------------------------------------------

#[test]
fn the_least_recently_used_entry_is_evicted() {
    // Not the oldest: a file re-read on every turn has to survive while files read once age out.
    let mut cache = Cache::new(2);
    let first = read_key("first.txt");
    let second = read_key("second.txt");
    let third = read_key("third.txt");

    cache.put(first.clone(), entry("one", 1));
    cache.put(second.clone(), entry("two", 2));
    // `first` is now the most recently used, so `second` becomes the victim.
    assert!(cache.get(&first, Some(1)).is_some());

    cache.put(third.clone(), entry("three", 3));

    assert!(
        cache.get(&first, Some(1)).is_some(),
        "recently used entry survives"
    );
    assert!(
        cache.get(&second, Some(2)).is_none(),
        "least recently used is gone"
    );
    assert!(cache.get(&third, Some(3)).is_some());
    assert_eq!(cache.stats().evictions, 1);
    assert_eq!(cache.len(), 2);
}

#[test]
fn storing_the_same_key_twice_replaces_rather_than_grows() {
    let mut cache = Cache::new(2);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("old", 1));
    cache.put(key.clone(), entry("new", 2));

    assert_eq!(cache.len(), 1);
    assert_eq!(
        cache.get(&key, Some(2)).expect("replaced entry").text,
        "new"
    );
}

#[test]
fn a_zero_capacity_cache_stores_nothing() {
    // The documented way to disable caching without branching at every call site.
    let mut cache = Cache::new(0);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("contents", 1));

    assert!(cache.is_empty());
    assert!(cache.get(&key, Some(1)).is_none());
}

#[test]
fn the_capacity_is_reported() {
    assert_eq!(Cache::new(7).capacity(), 7);
    assert_eq!(Cache::new(7).len(), 0);
    assert!(Cache::new(7).is_empty());
}

// ---- clearing and counters ---------------------------------------------------------------------

#[test]
fn clear_drops_every_entry_and_keeps_the_counters() {
    // The counters are the session's evidence about how well caching worked; zeroing them on a flush
    // would erase exactly the numbers an operator is looking at.
    let mut cache = Cache::new(4);
    cache.put(read_key("a.txt"), entry("a", 1));
    cache.put(read_key("b.txt"), entry("b", 2));
    assert!(cache.get(&read_key("a.txt"), Some(1)).is_some());

    cache.clear();

    assert!(cache.is_empty());
    assert_eq!(cache.len(), 0);
    assert_eq!(cache.stats().hits, 1);
    assert!(cache.get(&read_key("a.txt"), Some(1)).is_none());
}

#[test]
fn stats_count_hits_and_misses_separately() {
    let mut cache = Cache::new(4);
    let key = read_key("a.txt");
    cache.put(key.clone(), entry("contents", 1));

    assert!(cache.get(&key, Some(1)).is_some());
    assert!(cache.get(&key, Some(1)).is_some());
    assert!(cache.get(&read_key("b.txt"), Some(1)).is_none());

    let stats = cache.stats();
    assert_eq!(stats.hits, 2);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.requests(), 3);
    assert!((stats.hit_rate() - 2.0 / 3.0).abs() < 1e-9);
}

#[test]
fn an_unused_cache_reports_a_zero_rate_rather_than_nan() {
    // A `NaN` propagating into a metric poisons every average it enters.
    let stats = Cache::new(4).stats();
    assert_eq!(stats.requests(), 0);
    assert_eq!(stats.hit_rate(), 0.0);
    assert!(stats.hit_rate().is_finite());
}

#[test]
fn a_put_is_not_counted_as_a_miss() {
    // A caller that looks up first and then stores would otherwise look twice as cold as it is.
    let mut cache = Cache::new(4);
    cache.put(read_key("a.txt"), entry("contents", 1));
    let stats = cache.stats();
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.misses, 0);
    assert_eq!(stats.requests(), 0);
}

#[test]
fn the_debug_form_does_not_print_file_contents() {
    // An entry is a whole file; a derived `Debug` would put it in a log line.
    let mut cache = Cache::new(4);
    cache.put(read_key("a.txt"), entry("a secret line", 1));
    let shown = format!("{cache:?}");

    assert!(!shown.contains("a secret line"), "got {shown}");
    assert!(shown.contains("Cache"), "got {shown}");
}
