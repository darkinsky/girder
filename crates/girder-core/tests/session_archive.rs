//! Tests for archiving a session log instead of trimming it.
//!
//! The property that carries the module is stated once and checked everywhere: **no event may
//! disappear**. A cut that deletes is indistinguishable, later, from a log that was always short, so
//! every archive test reconstructs the full history from the two surviving files and compares it to
//! the history that existed before the archive ran.
//!
//! The remaining properties are about the edges that decide whether the feature is usable: a small
//! log must be left alone rather than rewritten, an impossible policy must be refused rather than
//! silently doing nothing, an empty log must not be an error, and the log must still be appendable
//! afterwards — an archive that produced a file the store could no longer write would turn every
//! long session into a dead one.
//!
//! The module is included by path rather than through the crate root, so these tests compile against
//! it as written even before the crate root wires it in.

#[path = "../src/session_archive.rs"]
mod session_archive;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{SessionEvent, SessionStore};
use session_archive::{ArchivePolicy, LogStats, analyze, archive};

/// A scratch directory unique to one test.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-archive-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory is created");
    dir
}

/// A user-message event carrying `text`.
fn user(text: &str) -> SessionEvent {
    SessionEvent::user_text("m", text)
}

/// An assistant-message event carrying `text`.
fn assistant(text: &str) -> SessionEvent {
    SessionEvent::assistant_text("a", text, "test")
}

/// Writes `events` as the log at `path`, through the real store so the fixture is the real format.
fn seed(path: &Path, events: &[SessionEvent]) {
    let store = SessionStore::new(path);
    for event in events {
        store.append(event).expect("fixture event is appended");
    }
}

/// `count` alternating events, texted so an individual event is identifiable after a round trip.
fn seeded(count: usize) -> Vec<SessionEvent> {
    (0..count)
        .map(|index| {
            if index % 2 == 0 {
                user(&format!("u{index}"))
            } else {
                assistant(&format!("a{index}"))
            }
        })
        .collect()
}

/// Every archive file beside `log`, sorted by name.
///
/// Sorted so a test that archives twice can name the first and second archive without depending on
/// the filesystem's ordering. The stamp is fixed-width, so name order is chronological order.
fn archives_beside(log: &Path) -> Vec<PathBuf> {
    let dir = log.parent().expect("the log has a parent");
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the scratch directory is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".archive-"))
        })
        .collect();
    found.sort();
    found
}

/// Reads the events of the log at `path`, without creating it when it is absent.
fn read(path: &Path) -> Vec<SessionEvent> {
    girder_core::read_session_file(path).expect("the log decodes")
}

// ---- a log that is small enough -----------------------------------------------------------------

#[test]
fn a_small_log_is_left_alone() {
    let dir = scratch("small");
    let log = dir.join("s.jsonl.zst");
    seed(&log, &seeded(4));
    let before = std::fs::read(&log).expect("the log is readable");

    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 10,
            keep_last: 2,
        },
    )
    .expect("a small log is not an error");

    assert!(outcome.is_none(), "a small log must not be archived");
    assert_eq!(
        std::fs::read(&log).expect("the log is readable"),
        before,
        "a log below the threshold must be byte-identical, not rewritten"
    );
    assert!(
        archives_beside(&log).is_empty(),
        "a log below the threshold must not produce an archive"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_log_exactly_at_the_threshold_is_left_alone() {
    // The trigger is "more than", not "at least": archiving at exactly `max_events` would move
    // events out of a log the caller declared acceptable.
    let dir = scratch("exact");
    let log = dir.join("s.jsonl.zst");
    seed(&log, &seeded(10));

    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 10,
            keep_last: 3,
        },
    )
    .expect("a log at the threshold is not an error");

    assert!(outcome.is_none());
    assert_eq!(read(&log).len(), 10);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- a log that is too large --------------------------------------------------------------------

#[test]
fn a_large_log_keeps_only_its_tail() {
    let dir = scratch("tail");
    let log = dir.join("s.jsonl.zst");
    let events = seeded(40);
    seed(&log, &events);

    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 10,
            keep_last: 6,
        },
    )
    .expect("the log is archived")
    .expect("a log over the threshold is archived");

    assert_eq!(outcome.kept, 6);
    assert_eq!(outcome.archived, 34);
    assert_eq!(
        read(&log),
        events[34..].to_vec(),
        "the live log must hold exactly the last `keep_last` events, in order"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_archive_holds_the_events_that_were_cut() {
    // The core property. A cut that merely dropped the prefix would pass every other test here.
    let dir = scratch("nocutloss");
    let log = dir.join("s.jsonl.zst");
    let events = seeded(40);
    seed(&log, &events);

    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 10,
            keep_last: 6,
        },
    )
    .expect("the log is archived")
    .expect("a log over the threshold is archived");

    let archived = read(&outcome.archive);
    assert_eq!(
        archived,
        events[..34].to_vec(),
        "the archive must hold exactly the events removed from the log, in order"
    );

    // And the two halves together are the history that existed before the archive ran.
    let mut rejoined = archived;
    rejoined.extend(read(&log));
    assert_eq!(
        rejoined, events,
        "archive plus log must reconstruct the log"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_archive_is_named_after_its_session_and_sits_beside_it() {
    let dir = scratch("naming");
    let log = dir.join("session-7.jsonl.zst");
    seed(&log, &seeded(20));

    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 5,
            keep_last: 3,
        },
    )
    .expect("the log is archived")
    .expect("a log over the threshold is archived");

    assert_eq!(outcome.archived, 17);
    assert_eq!(outcome.archive.parent(), Some(dir.as_path()));
    let name = outcome
        .archive
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the archive name is UTF-8");
    assert!(
        name.starts_with("session-7.archive-"),
        "the archive must be named after the session: {name}"
    );
    assert!(
        name.ends_with(".jsonl.zst"),
        "the archive must be a readable session log: {name}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_archive_does_not_overwrite_the_first() {
    // Two archives of one session can land in the same millisecond. Overwriting would be data loss
    // of exactly the kind this module exists to prevent.
    let dir = scratch("collision");
    let log = dir.join("s.jsonl.zst");
    seed(&log, &seeded(20));

    let first = archive(
        &log,
        &ArchivePolicy {
            max_events: 5,
            keep_last: 4,
        },
    )
    .expect("the first archive succeeds")
    .expect("the first archive runs");

    for index in 0..10 {
        girder_core::SessionStore::new(&log)
            .append(&user(&format!("later{index}")))
            .expect("the log accepts appends after an archive");
    }
    let second = archive(
        &log,
        &ArchivePolicy {
            max_events: 5,
            keep_last: 4,
        },
    )
    .expect("the second archive succeeds")
    .expect("the second archive runs");

    assert_ne!(first.archive, second.archive);
    // 20 events seeded, 4 kept, then 10 appended: the second archive moves the 10 that arrived
    // after the first cut, and leaves the 4 the policy keeps.
    assert_eq!(read(&first.archive).len(), 16);
    assert_eq!(read(&second.archive).len(), 10);
    assert_eq!(read(&log).len(), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- statistics ---------------------------------------------------------------------------------

#[test]
fn statistics_count_events_by_type_and_size() {
    let dir = scratch("stats");
    let log = dir.join("s.jsonl.zst");
    let events = vec![
        user("one"),
        assistant("two"),
        user("three"),
        assistant("four"),
        user("five"),
    ];
    seed(&log, &events);

    let stats: LogStats = analyze(&log).expect("the log is analysed");

    assert_eq!(stats.events, 5);
    assert_eq!(stats.by_type.get("user/message"), Some(&3));
    assert_eq!(stats.by_type.get("assistant/message"), Some(&2));
    assert_eq!(
        stats.by_type.values().sum::<usize>(),
        stats.events,
        "the breakdown must add up to the total"
    );
    assert_eq!(
        stats.bytes_on_disk,
        std::fs::metadata(&log).expect("the log exists").len()
    );
    assert!(stats.bytes_on_disk > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn statistics_survive_the_archive_split() {
    // A summary of the live log after an archive must describe what is left, and the archive's own
    // summary must describe what moved — otherwise the numbers cannot be reconciled.
    let dir = scratch("statsplit");
    let log = dir.join("s.jsonl.zst");
    seed(&log, &seeded(40));

    let before = analyze(&log).expect("the log is analysed");
    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 10,
            keep_last: 6,
        },
    )
    .expect("the log is archived")
    .expect("a log over the threshold is archived");
    let after = analyze(&log).expect("the trimmed log is analysed");
    let moved = analyze(&outcome.archive).expect("the archive is analysed");

    assert_eq!(before.events, 40);
    assert_eq!(after.events, outcome.kept);
    assert_eq!(moved.events, outcome.archived);
    assert_eq!(after.events + moved.events, before.events);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- refusals and edges -------------------------------------------------------------------------

#[test]
fn an_overlapping_policy_is_refused() {
    let dir = scratch("invalid");
    let log = dir.join("s.jsonl.zst");
    seed(&log, &seeded(20));
    let before = std::fs::read(&log).expect("the log is readable");

    for policy in [
        ArchivePolicy {
            max_events: 10,
            keep_last: 10,
        },
        ArchivePolicy {
            max_events: 5,
            keep_last: 9,
        },
        ArchivePolicy {
            max_events: 0,
            keep_last: 0,
        },
        ArchivePolicy {
            max_events: 10,
            keep_last: 0,
        },
    ] {
        let error = archive(&log, &policy).expect_err("an impossible policy is refused");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "an invalid policy is a caller error, not an I/O failure: {policy:?}"
        );
    }

    assert_eq!(
        std::fs::read(&log).expect("the log is readable"),
        before,
        "a refused policy must not touch the log"
    );
    assert!(archives_beside(&log).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_log_is_not_archived() {
    let dir = scratch("empty");
    let log = dir.join("s.jsonl.zst");
    seed(&log, &[]);

    let stats = analyze(&log).expect("an empty log is analysed");
    assert_eq!(stats.events, 0);
    assert!(stats.by_type.is_empty());

    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 10,
            keep_last: 5,
        },
    )
    .expect("an empty log is not an error");
    assert!(outcome.is_none());
    assert!(archives_beside(&log).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_log_is_empty_rather_than_an_error() {
    // A directory scan and the file it names can disagree when a session is deleted in between.
    let dir = scratch("missing");
    let log = dir.join("gone.jsonl.zst");

    let stats = analyze(&log).expect("a missing log is analysed as empty");
    assert_eq!(stats.events, 0);
    assert_eq!(stats.bytes_on_disk, 0);

    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 2,
            keep_last: 1,
        },
    )
    .expect("a missing log is not an error");
    assert!(outcome.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_corrupt_log_is_reported_and_left_untouched() {
    let dir = scratch("corrupt");
    let log = dir.join("s.jsonl.zst");
    let garbage = b"this is not a zstd frame".to_vec();
    std::fs::write(&log, &garbage).expect("the corrupt fixture is written");

    let error = analyze(&log).expect_err("a corrupt log is reported");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let error = archive(
        &log,
        &ArchivePolicy {
            max_events: 2,
            keep_last: 1,
        },
    )
    .expect_err("a corrupt log is not archived");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

    assert_eq!(
        std::fs::read(&log).expect("the log is readable"),
        garbage,
        "a failed archive must leave the original bytes in place"
    );
    assert!(
        archives_beside(&log).is_empty(),
        "a decode failure must happen before any archive is written"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the log stays usable -----------------------------------------------------------------------

#[test]
fn the_log_accepts_appends_after_an_archive() {
    // An archive that produced a file the store could no longer write would turn every long session
    // into a dead one.
    let dir = scratch("append");
    let log = dir.join("s.jsonl.zst");
    seed(&log, &seeded(30));

    let outcome = archive(
        &log,
        &ArchivePolicy {
            max_events: 10,
            keep_last: 8,
        },
    )
    .expect("the log is archived")
    .expect("a log over the threshold is archived");
    let kept = read(&log);
    assert_eq!(kept.len(), 8);

    let store = SessionStore::new(&log);
    store.append(&user("after")).expect("the log is appendable");
    store
        .append(&assistant("again"))
        .expect("the log is appendable");

    let mut expected = kept;
    expected.push(user("after"));
    expected.push(assistant("again"));
    assert_eq!(
        read(&log),
        expected,
        "appends must land after the kept tail"
    );
    assert_eq!(
        read(&outcome.archive).len(),
        22,
        "appending must not reach into the archive"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_store_reads_the_trimmed_log_as_its_own_history() {
    // The trimmed log is only useful if the ordinary reader sees it: the format must not have
    // drifted from what `SessionStore` writes.
    let dir = scratch("storeview");
    let log = dir.join("s.jsonl.zst");
    let events = seeded(20);
    seed(&log, &events);
    archive(
        &log,
        &ArchivePolicy {
            max_events: 5,
            keep_last: 4,
        },
    )
    .expect("the log is archived")
    .expect("a log over the threshold is archived");

    let store = SessionStore::new(&log);
    assert_eq!(store.len().expect("the trimmed log is readable"), 4);
    assert_eq!(
        store.read_all().expect("the trimmed log replays"),
        events[16..]
    );
    assert!(
        store
            .has_user_message()
            .expect("the trimmed log is scannable"),
        "the kept tail must still be a recognisable conversation"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
