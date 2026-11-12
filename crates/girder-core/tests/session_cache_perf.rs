//! Tests that the session read cache actually avoids re-decoding.
//!
//! The cache was added because the loop reads the log once per step, and the tests for it asserted correctness
//! only -- that a repeated read returns the same answer. Correctness is necessary and not sufficient: a cache that
//! decodes every time is also correct, and would have passed the same tests while doing nothing.

use std::time::Instant;

use girder_core::{SessionEvent, SessionStore};

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-cacheperf-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// Writes `count` events, each with a body large enough that decoding costs something measurable.
fn seed(path: &std::path::Path, count: usize) {
    let store = SessionStore::new(path);
    store.touch().expect("created");
    for index in 0..count {
        store
            .append(&SessionEvent::user_text(
                &format!("m{index}"),
                // A kilobyte per event: a few thousand of these make the decode dominate the assertion.
                "x".repeat(1024),
            ))
            .expect("appended");
    }
}

#[test]
fn a_cached_read_is_faster_than_the_first_one() {
    let dir = scratch("faster");
    let path = dir.join("s.jsonl.zst");
    seed(&path, 2_000);
    let store = SessionStore::new(&path);

    let first = Instant::now();
    let events = store.read_all().expect("read");
    let cold = first.elapsed();
    assert_eq!(events.len(), 2_000);

    let second = Instant::now();
    let again = store.read_all().expect("read");
    let warm = second.elapsed();

    assert_eq!(again.len(), 2_000, "the cache returns the same events");
    // A generous bound: the point is that the second read does not decompress, not that it takes a specific time. A
    // cache that re-decoded would be at least as slow as the cold read, and on any machine the ratio is orders of
    // magnitude -- so a factor of two is a safe assertion that still fails if the cache does nothing.
    assert!(
        warm * 2 < cold,
        "the cached read must be faster: cold {cold:?}, warm {warm:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_append_makes_the_next_read_cold_again() {
    // The cache is keyed on the file size, so this is the property that keeps it honest: a stale answer would be
    // fast and wrong, which is worse than slow and right.
    let dir = scratch("invalidate");
    let path = dir.join("s.jsonl.zst");
    seed(&path, 500);
    let store = SessionStore::new(&path);
    assert_eq!(store.read_all().expect("read").len(), 500);

    SessionStore::new(&path)
        .append(&SessionEvent::user_text("late", "y"))
        .expect("appended");

    assert_eq!(store.read_all().expect("read").len(), 501);
    let _ = std::fs::remove_dir_all(&dir);
}
