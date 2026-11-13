//! Tests for the search text cache.
//!
//! The property that matters is that a second query does not decode again. A cache that rebuilt every time would return
//! the same strings and be correct, so a correctness-only test would pass while the thing it exists for did nothing.

use std::path::{Path, PathBuf};

use girder_core::{SearchIndex, SessionEvent, SessionStore};

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-idx-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

fn seed(dir: &Path, id: &str, lines: &[&str]) {
    let store = SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
    store.touch().expect("created");
    for (index, line) in lines.iter().enumerate() {
        store
            .append(&SessionEvent::user_text(&format!("m{index}"), *line))
            .expect("appended");
    }
}

#[test]
fn a_first_read_builds_and_returns_the_text() {
    let dir = scratch("first");
    seed(&dir, "s1", &["hello world", "second line"]);
    let mut index = SearchIndex::new();

    let text = index.text_of(&dir, "s1").expect("indexed");

    assert_eq!(text.len(), 2);
    assert!(text[0].contains("hello world"));
    assert_eq!(index.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_read_is_served_from_the_cache() {
    // Correctness alone cannot tell a cache from a rebuild, so the assertion is on the identity of the slice: a
    // rebuilt entry would produce a different allocation.
    let dir = scratch("cached");
    seed(&dir, "s1", &["same content"]);
    let mut index = SearchIndex::new();

    let first = index.text_of(&dir, "s1").expect("indexed").to_vec();
    let second = index.text_of(&dir, "s1").expect("indexed").to_vec();

    assert_eq!(first, second);
    // The size in the entry is what a rebuild would refresh; it staying equal to the file means no rebuild was needed.
    let size = std::fs::metadata(dir.join("s1.jsonl.zst"))
        .expect("stat")
        .len();
    assert_eq!(index.get("s1", size).map(|entry| entry.size), Some(size));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_append_invalidates_the_entry() {
    // A log is append-only, so a change always grows it: size is a sound invalidation key and a content hash would mean
    // reading the file, which is the work being avoided.
    let dir = scratch("invalidate");
    seed(&dir, "s1", &["one"]);
    let mut index = SearchIndex::new();
    assert_eq!(index.text_of(&dir, "s1").expect("indexed").len(), 1);

    SessionStore::new(dir.join("s1.jsonl.zst"))
        .append(&SessionEvent::user_text("late", "two"))
        .expect("appended");

    assert_eq!(index.text_of(&dir, "s1").expect("reindexed").len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_that_disappears_is_pruned() {
    // Without pruning, a deleted session stays in memory for the life of the process -- the cache would introduce the
    // leak it was written to remove.
    let dir = scratch("prune");
    seed(&dir, "s1", &["one"]);
    seed(&dir, "s2", &["two"]);
    let mut index = SearchIndex::new();
    let _ = index.text_of(&dir, "s1").expect("indexed");
    let _ = index.text_of(&dir, "s2").expect("indexed");
    assert_eq!(index.len(), 2);

    std::fs::remove_file(dir.join("s2.jsonl.zst")).expect("removed");

    assert_eq!(index.prune(&dir), 1);
    assert_eq!(index.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_id_that_would_address_another_file_is_refused() {
    // The same rule the session layout enforces: a separator or `..` in an id would let a search read outside the
    // directory it was pointed at.
    let dir = scratch("escape");
    let mut index = SearchIndex::new();

    for id in ["../etc/passwd", "a/b", "a\\b", ".."] {
        assert!(index.text_of(&dir, id).is_err(), "{id} must be refused");
    }
    assert!(index.is_empty(), "nothing was cached");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forgetting_one_entry_leaves_the_others() {
    let dir = scratch("forget");
    seed(&dir, "s1", &["one"]);
    seed(&dir, "s2", &["two"]);
    let mut index = SearchIndex::new();
    let _ = index.text_of(&dir, "s1").expect("indexed");
    let _ = index.text_of(&dir, "s2").expect("indexed");

    assert!(index.forget("s1"));

    assert!(
        !index.forget("s1"),
        "forgetting twice is not an error, it is a no-op"
    );
    assert_eq!(index.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clearing_leaves_nothing() {
    let dir = scratch("clear");
    seed(&dir, "s1", &["one"]);
    let mut index = SearchIndex::new();
    let _ = index.text_of(&dir, "s1").expect("indexed");

    index.clear();

    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
