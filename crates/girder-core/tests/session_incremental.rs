//! Tests for reading a session log incrementally.
//!
//! `read_all` answers the same question, so the property here is not correctness -- it is that a caller that already
//! holds the conversation does not pay to copy it again on every poll that found nothing new.

use girder_core::{SessionEvent, SessionStore};

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-incr-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

fn seeded(name: &str, count: usize) -> (std::path::PathBuf, SessionStore) {
    let dir = scratch(name);
    let path = dir.join("s.jsonl.zst");
    let store = SessionStore::new(&path);
    store.touch().expect("created");
    for index in 0..count {
        store
            .append(&SessionEvent::user_text(
                &format!("m{index}"),
                format!("body {index}"),
            ))
            .expect("appended");
    }
    (dir, store)
}

#[test]
fn a_cursor_of_zero_returns_everything() {
    let (dir, store) = seeded("zero", 3);

    let events = store.read_after(0).expect("read");

    assert_eq!(events.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cursor_returns_only_what_came_after() {
    // The whole point: a caller that holds the first two events asks for the rest and gets exactly one.
    let (dir, store) = seeded("after", 3);

    let events = store.read_after(2).expect("read");

    assert_eq!(events.len(), 1, "{events:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cursor_past_the_end_returns_nothing() {
    // A poller that has not missed anything is the normal case, not a mistake.
    let (dir, store) = seeded("past", 2);

    assert!(store.read_after(99).expect("read").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cursor_at_the_end_returns_nothing() {
    let (dir, store) = seeded("exact", 2);

    assert!(store.read_after(2).expect("read").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cursor_agrees_with_reading_everything() {
    // The two must not disagree about what the log contains, or a client polling and a client reading would see
    // different conversations.
    let (dir, store) = seeded("agree", 5);
    let all = store.read_all().expect("read");

    for after in 0..=5u64 {
        let tail = store.read_after(after).expect("read");
        let expected = &all[usize::try_from(after).expect("fits")..];
        assert_eq!(tail.len(), expected.len(), "after = {after}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_log_has_no_events() {
    let dir = scratch("empty");
    let store = SessionStore::new(dir.join("s.jsonl.zst"));
    store.touch().expect("created");

    assert!(store.read_after(0).expect("read").is_empty());
    assert_eq!(store.event_count().expect("count"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_count_matches_the_events() {
    // A status line showing "3 of 40 events" needs the length, not the events; asking for the events to count them is
    // how a status line ends up decoding a conversation.
    let (dir, store) = seeded("count", 4);

    assert_eq!(store.event_count().expect("count"), 4);
    assert_eq!(
        store.event_count().expect("count"),
        u64::try_from(store.read_all().expect("read").len()).expect("fits")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn appending_is_visible_to_the_next_read() {
    let (dir, store) = seeded("append", 1);
    assert_eq!(store.read_after(1).expect("read").len(), 0);

    store
        .append(&SessionEvent::user_text("late", "new"))
        .expect("appended");

    assert_eq!(store.read_after(1).expect("read").len(), 1);
    assert_eq!(store.event_count().expect("count"), 2);
    let _ = std::fs::remove_dir_all(&dir);
}
