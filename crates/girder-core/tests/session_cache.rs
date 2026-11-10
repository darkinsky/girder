//! Tests for the session read cache.
//!
//! The cache exists because the loop reads the log once per step, and decompressing it each time was work that
//! produced the same answer. The property that matters is that it never serves a stale answer.

use girder_core::{SessionEvent, SessionStore};

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-cache-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

#[test]
fn a_repeated_read_returns_the_same_events() {
    let dir = scratch("repeat");
    let store = SessionStore::new(dir.join("s.jsonl.zst"));
    store.touch().expect("created");
    store
        .append(&SessionEvent::user_text("m1", "hello"))
        .expect("appended");

    let first = store.read_all().expect("read");
    let second = store.read_all().expect("read");

    assert_eq!(first, second);
    assert_eq!(first.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_append_invalidates_the_cache() {
    // The whole risk of a cache: serving an answer the file no longer has.
    let dir = scratch("invalidate");
    let store = SessionStore::new(dir.join("s.jsonl.zst"));
    store.touch().expect("created");
    store
        .append(&SessionEvent::user_text("m1", "first"))
        .expect("appended");
    assert_eq!(store.read_all().expect("read").len(), 1);

    store
        .append(&SessionEvent::user_text("m2", "second"))
        .expect("appended");

    assert_eq!(
        store.read_all().expect("read").len(),
        2,
        "the cache must not hide the append"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_clone_shares_the_cache_without_serving_stale_events() {
    // Two handles on one path must both see an append the other made.
    let dir = scratch("clone");
    let path = dir.join("s.jsonl.zst");
    let store = SessionStore::new(&path);
    store.touch().expect("created");
    store
        .append(&SessionEvent::user_text("m1", "first"))
        .expect("appended");
    let reader = SessionStore::new(&path);
    assert_eq!(reader.read_all().expect("read").len(), 1);

    store
        .append(&SessionEvent::user_text("m2", "second"))
        .expect("appended");

    assert_eq!(reader.read_all().expect("read").len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_log_reads_as_no_events() {
    let dir = scratch("empty");
    let store = SessionStore::new(dir.join("s.jsonl.zst"));
    store.touch().expect("created");

    assert!(store.read_all().expect("read").is_empty());
    assert!(store.read_all().expect("read").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_log_reads_as_no_events() {
    let dir = scratch("missing");
    let store = SessionStore::new(dir.join("absent.jsonl.zst"));

    assert!(store.read_all().expect("read").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_large_log_round_trips_through_the_cache() {
    // More events than one chunk holds, so the chunked storage is exercised across a boundary.
    let dir = scratch("large");
    let store = SessionStore::new(dir.join("s.jsonl.zst"));
    store.touch().expect("created");
    for index in 0..150 {
        store
            .append(&SessionEvent::user_text(
                &format!("m{index}"),
                format!("body {index}"),
            ))
            .expect("appended");
    }

    let first = store.read_all().expect("read");
    let second = store.read_all().expect("read");

    assert_eq!(first.len(), 150);
    assert_eq!(first, second, "the cached copy matches what was decoded");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn appending_after_a_cached_read_returns_every_event() {
    let dir = scratch("append-after");
    let store = SessionStore::new(dir.join("s.jsonl.zst"));
    store.touch().expect("created");
    for index in 0..10 {
        store
            .append(&SessionEvent::user_text(&format!("m{index}"), "x"))
            .expect("appended");
    }
    assert_eq!(store.read_all().expect("read").len(), 10);

    store
        .append(&SessionEvent::user_text("last", "y"))
        .expect("appended");

    let events = store.read_all().expect("read");
    assert_eq!(events.len(), 11);
    assert_eq!(
        events.last().map(SessionEvent::type_name),
        Some("user/message")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
