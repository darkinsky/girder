//! Tests for the persisted search index.
//!
//! The tests are weighted toward the failures that would make a persisted cache *worse* than no
//! cache at all, because those are the ones a correctness-only test cannot see:
//!
//! - **an outdated file being read as if it were current.** A version bump that silently produced
//!   entries from the old shape would serve text that no longer matches the logs. The test pins that
//!   a mismatch yields an *empty* index, which is the rebuild path.
//! - **a damaged file turning into a search failure.** A truncated or garbled cache must degrade to
//!   "rebuild", never to "search is broken". Every corruption below asserts `Ok(empty)`.
//! - **a stale record being served.** A record whose log has grown must be dropped, since otherwise a
//!   search answers from text the conversation has already moved past.
//! - **a torn file.** The whole file is rewritten on every save, so a non-atomic write is a window in
//!   which the file is half old and half new and parses as neither.
//!
//! The module is included by path rather than through the crate's re-exports, because the crate root
//! is wired up separately. It resolves `girder_core::…` through the external crate, so these tests
//! compile against the module as written and against the real `SearchIndex` it persists.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/search_persist.rs"]
mod search_persist;

#[allow(unused_imports)]
use girder_core as _;
use girder_core::SearchIndexEntry;
use search_persist::{
    FILE_NAME, PersistError, Record, Snapshot, VERSION, load, load_for, path_in, save,
};
use std::path::{Path, PathBuf};

// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh directory per test, under the temp directory.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-persist-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// Writes a session log of exactly `len` bytes.
///
/// The contents are irrelevant: [`load_for`] compares the file's *length*, never its bytes, which is
/// the property that makes this cache cheap. A log of filler bytes therefore exercises the same path
/// a real `jsonl.zst` would.
fn write_log(root: &Path, id: &str, len: usize) -> u64 {
    let path = root.join(format!("{id}.jsonl.zst"));
    std::fs::write(&path, vec![b'x'; len]).expect("log written");
    std::fs::metadata(&path).expect("stat").len()
}

/// Appends to a log, growing it the way an append-only session file does.
fn grow_log(root: &Path, id: &str, extra: usize) -> u64 {
    use std::io::Write as _;
    let path = root.join(format!("{id}.jsonl.zst"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opened");
    file.write_all(&vec![b'y'; extra]).expect("appended");
    std::fs::metadata(&path).expect("stat").len()
}

/// A record for a session whose log is `size` bytes long.
fn record(session: &str, size: u64, lines: &[&str]) -> Record {
    Record {
        session: session.to_owned(),
        size,
        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
    }
}

/// Reads the file back as raw text, for tests that assert on the on-disk form.
fn raw(path: &Path) -> String {
    std::fs::read_to_string(path).expect("readable")
}

// ---- round trip --------------------------------------------------------------------------------

#[test]
fn a_saved_index_loads_back() {
    let dir = scratch("roundtrip");
    let size = write_log(&dir, "s1", 64);
    let path = path_in(&dir);
    let records = vec![record("s1", size, &["hello world", "second line"])];

    save(&path, &records).expect("saved");
    let index = load(&path).expect("loaded");

    assert_eq!(index.len(), 1);
    let entry = index.get("s1", size).expect("present and valid");
    assert_eq!(entry.size, size);
    assert_eq!(entry.events, vec!["hello world", "second line"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_loaded_index_answers_with_the_persisted_text() {
    // The point of the file is that the text survives the restart, so the assertion is on the strings
    // themselves rather than on the count.
    let dir = scratch("text");
    let size = write_log(&dir, "s1", 8);
    save(
        &path_in(&dir),
        &[record("s1", size, &["alpha", "beta", "gamma"])],
    )
    .expect("saved");

    let index = load(&path_in(&dir)).expect("loaded");

    assert_eq!(
        index.get("s1", size).map(|entry| entry.events.clone()),
        Some(vec![
            "alpha".to_owned(),
            "beta".to_owned(),
            "gamma".to_owned()
        ])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_index_round_trips() {
    let dir = scratch("empty");
    let path = path_in(&dir);

    save(&path, &[]).expect("saved");
    let index = load(&path).expect("loaded");

    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_index_lives_beside_the_logs() {
    // `load` infers the log directory from the file's parent, so the placement is part of the
    // contract rather than a convenience.
    let dir = scratch("placement");
    let size = write_log(&dir, "s1", 4);

    save(&path_in(&dir), &[record("s1", size, &["x"])]).expect("saved");

    assert!(dir.join(FILE_NAME).is_file());
    let index = load(&path_in(&dir)).expect("loaded with the parent as the log directory");
    assert_eq!(index.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_file_records_the_current_version() {
    // A bump that forgot to write the new number would make the next process discard this file, so
    // the persisted version is asserted rather than assumed.
    let dir = scratch("version");
    let size = write_log(&dir, "s1", 4);
    let path = path_in(&dir);

    save(&path, &[record("s1", size, &["x"])]).expect("saved");

    let snapshot: Snapshot = serde_json::from_str(&raw(&path)).expect("valid snapshot");
    assert_eq!(snapshot.version, VERSION);
    assert_eq!(snapshot.records.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- version mismatch: the core behaviour ------------------------------------------------------

#[test]
fn a_different_version_loads_empty() {
    // An old index is a cache that needs rebuilding, not an error that should stop a search. The
    // assertion is on `Ok` *and* on emptiness: returning an error would fail a query that could
    // simply have rebuilt.
    let dir = scratch("version-mismatch");
    let size = write_log(&dir, "s1", 4);
    let path = path_in(&dir);
    // A record that would be perfectly valid under the current version: the only thing wrong with
    // this file is its number, so the test cannot pass because of something else.
    let stale = serde_json::json!({
        "version": VERSION + 1,
        "records": [{"session": "s1", "size": size, "lines": ["x"]}],
    });
    std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).expect("written");

    let index = load(&path).expect("a version mismatch is not an error");

    assert!(
        index.is_empty(),
        "a file from another version must not be read as current"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_older_version_loads_empty_too() {
    // The check is equality, not "at least": a downgrade is the same situation as an upgrade, and
    // reading an older file with a newer reader is the case most likely to mis-parse silently.
    let dir = scratch("version-old");
    let size = write_log(&dir, "s1", 4);
    let path = path_in(&dir);
    let old = serde_json::json!({
        "version": 0,
        "records": [{"session": "s1", "size": size, "lines": ["x"]}],
    });
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).expect("written");

    let index = load(&path).expect("not an error");

    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_version_mismatch_is_overwritten_by_the_next_save() {
    // Rebuilding must be able to repair the file, or a version bump would leave a permanently
    // unusable cache that fails on every start.
    let dir = scratch("version-repair");
    let size = write_log(&dir, "s1", 4);
    let path = path_in(&dir);
    std::fs::write(&path, br#"{"version":99,"records":[]}"#).expect("written");

    save(&path, &[record("s1", size, &["fresh"])]).expect("saved");
    let index = load(&path).expect("loaded");

    assert_eq!(index.len(), 1);
    assert_eq!(
        index.get("s1", size).map(|entry| entry.events.clone()),
        Some(vec!["fresh".to_owned()])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- corruption: also the core behaviour -------------------------------------------------------

#[test]
fn a_truncated_file_loads_empty() {
    // This is what a non-atomic writer would leave behind, and what a crash mid-write used to
    // produce before the rename.
    let dir = scratch("truncated");
    let size = write_log(&dir, "s1", 4);
    let path = path_in(&dir);
    save(&path, &[record("s1", size, &["hello world"])]).expect("saved");
    let text = raw(&path);
    std::fs::write(&path, &text.as_bytes()[..text.len() / 2]).expect("truncated");

    let index = load(&path).expect("a damaged cache is not an error");

    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_zero_length_file_loads_empty() {
    let dir = scratch("zero");
    let path = path_in(&dir);
    std::fs::write(&path, b"").expect("written");

    let index = load(&path).expect("not an error");

    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn garbage_bytes_load_empty() {
    let dir = scratch("garbage");
    let path = path_in(&dir);
    std::fs::write(&path, [0xff_u8, 0xfe, 0x00, 0x7b, 0x7b]).expect("written");

    let index = load(&path).expect("not an error");

    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn valid_json_of_the_wrong_shape_loads_empty() {
    // Valid JSON is not a valid snapshot. A reader that only checked "did it parse" would accept the
    // first `object` value and then fail while interpreting it.
    let dir = scratch("shape");
    let path = path_in(&dir);

    for body in [
        "null",
        "[]",
        "{}",
        r#"{"version":1,"records":"nope"}"#,
        r#"{"version":1,"records":[{"session":"s1"}]}"#,
        r#"{"records":[]}"#,
    ] {
        std::fs::write(&path, body).expect("written");
        let index = load(&path).expect("not an error");
        assert!(index.is_empty(), "`{body}` must not be read as an index");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_file_loads_empty() {
    // The state every first run is in. Reporting it as an error would make the normal path look
    // exceptional.
    let dir = scratch("missing");
    let path = path_in(&dir);

    let index = load(&path).expect("an absent cache is not an error");

    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- staleness ---------------------------------------------------------------------------------

#[test]
fn a_record_whose_log_grew_is_dropped() {
    // A log is append-only, so a change always grows it. Serving the old text would answer from a
    // conversation that has moved on -- the failure mode that makes a cache worse than nothing.
    let dir = scratch("grew");
    let size = write_log(&dir, "s1", 16);
    let path = path_in(&dir);
    save(&path, &[record("s1", size, &["old text"])]).expect("saved");
    grow_log(&dir, "s1", 8);

    let index = load(&path).expect("loaded");

    assert!(
        index.is_empty(),
        "the entry's text no longer matches the log, so it must be rebuilt"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_the_changed_record_is_dropped() {
    // Dropping the whole file for one changed log would throw away the work the cache exists to
    // preserve, so the invalidation is per record.
    let dir = scratch("one-changed");
    let a = write_log(&dir, "a", 16);
    let b = write_log(&dir, "b", 16);
    let path = path_in(&dir);
    save(
        &path,
        &[record("a", a, &["a text"]), record("b", b, &["b text"])],
    )
    .expect("saved");
    grow_log(&dir, "b", 4);

    let index = load(&path).expect("loaded");

    assert_eq!(index.len(), 1);
    assert!(index.get("a", a).is_some(), "the untouched entry survives");
    assert!(
        index.get("b", b + 4).is_none(),
        "the grown log's record was dropped"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_record_whose_log_is_gone_is_dropped() {
    let dir = scratch("deleted");
    let size = write_log(&dir, "s1", 8);
    let path = path_in(&dir);
    save(&path, &[record("s1", size, &["x"])]).expect("saved");
    std::fs::remove_file(dir.join("s1.jsonl.zst")).expect("deleted");

    let index = load(&path).expect("loaded");

    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_id_that_would_address_another_file_is_dropped() {
    // The index file is not trusted input. An id with a separator or `..` would make the size check
    // stat a path outside the sessions directory.
    let dir = scratch("escape");
    let path = path_in(&dir);
    let inside = write_log(&dir, "s1", 4);
    let snapshot = serde_json::json!({
        "version": VERSION,
        "records": [
            {"session": "../etc/passwd", "size": 4, "lines": ["leak"]},
            {"session": "a/b", "size": 4, "lines": ["leak"]},
            {"session": "..", "size": 4, "lines": ["leak"]},
            {"session": "", "size": 4, "lines": ["leak"]},
            {"session": "s1", "size": inside, "lines": ["ok"]},
        ],
    });
    std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).expect("written");

    let index = load(&path).expect("loaded");

    assert_eq!(index.len(), 1, "only the well-formed id survives");
    assert!(index.get("s1", inside).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- saving ------------------------------------------------------------------------------------

#[test]
fn a_save_replaces_rather_than_appends() {
    // Two saves must not accumulate: the file is a snapshot of the caller's view, and a merge would
    // resurrect a record the caller deliberately dropped.
    let dir = scratch("replace");
    let size = write_log(&dir, "s1", 8);
    let path = path_in(&dir);

    save(&path, &[record("s1", size, &["first"])]).expect("saved");
    save(&path, &[record("s1", size, &["second"])]).expect("saved again");

    let index = load(&path).expect("loaded");
    assert_eq!(index.len(), 1);
    assert_eq!(
        index.get("s1", size).map(|entry| entry.events.clone()),
        Some(vec!["second".to_owned()])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rejected_id_fails_the_save_and_leaves_the_previous_file() {
    // Validation runs before anything is written, so a bad record cannot replace a good file with one
    // that `load` would then have to throw away -- which would lose the whole cache over one entry.
    let dir = scratch("refuse");
    let size = write_log(&dir, "s1", 8);
    let path = path_in(&dir);
    save(&path, &[record("s1", size, &["kept"])]).expect("saved");
    let before = raw(&path);

    let error = save(
        &path,
        &[
            record("s1", size, &["kept"]),
            record("../escape", size, &["bad"]),
        ],
    )
    .expect_err("an id with a separator must be refused");

    assert!(matches!(error, PersistError::Invalid { .. }));
    assert_eq!(raw(&path), before, "the previous file is untouched");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_save_leaves_no_staging_debris() {
    // The rename stages a sibling. A staging file that outlives its save is one nobody will ever
    // clean up, so the directory holds exactly the index afterwards.
    let dir = scratch("debris");
    let size = write_log(&dir, "s1", 8);
    let path = path_in(&dir);

    save(&path, &[record("s1", size, &["x"])]).expect("saved");

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("readable")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut expected = vec!["s1.jsonl.zst".to_owned(), FILE_NAME.to_owned()];
    expected.sort();
    assert_eq!(names, expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_save_creates_the_directory_when_it_is_absent() {
    // The index is written on a shutdown path, where failing because the directory was never created
    // would silently discard the cache of a session that ran only in memory.
    let dir = scratch("create-dir");
    let nested = dir.join("index-cache");
    let path = path_in(&nested);

    save(&path, &[]).expect("saved into a directory that did not exist");

    assert!(path.is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saving_with_an_explicit_log_directory() {
    // `load_for` exists because the file need not live beside the logs: a caller may keep the cache
    // under its own directory and pass the log root separately.
    let logs = scratch("explicit-logs");
    let cache = scratch("explicit-cache");
    let size = write_log(&logs, "s1", 8);
    let path = path_in(&cache);

    save(&path, &[record("s1", size, &["x"])]).expect("saved");
    let index = load_for(&path, &logs).expect("loaded");

    assert_eq!(index.len(), 1);
    let _ = std::fs::remove_dir_all(&logs);
    let _ = std::fs::remove_dir_all(&cache);
}

// ---- atomicity ---------------------------------------------------------------------------------

#[test]
fn a_concurrent_reader_never_sees_a_partial_file() {
    // The file is rewritten wholesale, so a direct write would leave a window in which the content is
    // a prefix of the new snapshot over the tail of the old one -- which parses as neither. A reader
    // must instead see the complete old snapshot or the complete new one, at every instant.
    //
    // The snapshots are large enough that a non-atomic write would be observably torn, and the reader
    // runs for the whole sequence rather than once at the end: a single read after the fact would
    // pass against an implementation that writes the target directly.
    let dir = scratch("atomic");
    let size = write_log(&dir, "s1", 8);
    let path = path_in(&dir);
    let long_a = vec!["alpha"; 4_000];
    let long_b = vec!["bravo"; 4_000];
    save(&path, &[record("s1", size, &long_a)]).expect("initial save");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop = std::sync::Arc::clone(&stop);
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        let mut reads = 0_usize;
        let mut failures: Vec<String> = Vec::new();
        while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
            match std::fs::read(&reader_path) {
                Ok(bytes) => match serde_json::from_slice::<Snapshot>(&bytes) {
                    Ok(snapshot) => {
                        if snapshot.version == VERSION && snapshot.records.len() == 1 {
                            reads += 1;
                        } else {
                            failures.push(format!(
                                "read a snapshot with version {} and {} records",
                                snapshot.version,
                                snapshot.records.len()
                            ));
                        }
                    }
                    // Exactly the failure a non-atomic writer produces: the reader caught the file
                    // between the old content and the new one.
                    Err(error) => {
                        failures.push(format!(
                            "read a torn file of {} bytes: {error}",
                            bytes.len()
                        ));
                    }
                },
                Err(error) => failures.push(format!("read failed: {error}")),
            }
        }
        (reads, failures)
    });

    for round in 0..25 {
        let lines = if round % 2 == 0 { &long_b } else { &long_a };
        save(&path, &[record("s1", size, lines)]).expect("saved during the read loop");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);

    let (reads, failures) = reader.join().expect("the reader did not panic");
    assert!(
        failures.is_empty(),
        "a reader observed an invalid index: {}",
        failures.join("; ")
    );
    assert!(reads > 0, "the reader never completed a read");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_target_file_always_exists_once_it_has_been_written() {
    // The rename replaces a name that already resolves, so a save never passes through a state where
    // the index is missing -- which a write-then-truncate implementation would.
    let dir = scratch("stable-name");
    let size = write_log(&dir, "s1", 8);
    let path = path_in(&dir);
    save(&path, &[record("s1", size, &["one"])]).expect("saved");
    let first = raw(&path);

    save(&path, &[record("s1", size, &["two"])]).expect("saved again");
    let second = raw(&path);

    for body in [&first, &second] {
        let snapshot: Snapshot = serde_json::from_str(body).expect("complete snapshot");
        assert_eq!(snapshot.version, VERSION);
        assert_eq!(snapshot.records.len(), 1);
    }
    assert_ne!(first, second, "the second save really replaced the file");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- interoperation with the in-memory cache ---------------------------------------------------

#[test]
fn a_loaded_index_is_usable_with_the_in_memory_cache() {
    // The loaded index is an ordinary `SearchIndex`: the point of persisting it is that the next query
    // reads from it without decoding, so it must satisfy the same `get(id, size)` contract.
    let dir = scratch("usable");
    let size = write_log(&dir, "s1", 8);
    let path = path_in(&dir);
    save(&path, &[record("s1", size, &["persisted"])]).expect("saved");

    let index = load(&path).expect("loaded");

    assert_eq!(index.len(), 1);
    assert!(
        index.get("s1", size).is_some(),
        "valid for the current size"
    );
    assert!(
        index.get("s1", size + 1).is_none(),
        "not valid for any other size"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_escape_sensitive_line_round_trips() {
    // The strings are the payload, so a line containing the encoder's own metacharacters is the case
    // a naive writer would corrupt.
    let dir = scratch("escapes");
    let size = write_log(&dir, "s1", 4);
    let path = path_in(&dir);
    let tricky = "quote \" backslash \\ newline \n tab \t unicode \u{6c49}\u{5b57}";

    save(&path, &[record("s1", size, &[tricky])]).expect("saved");
    let loaded = load(&path).expect("loaded");

    assert_eq!(
        loaded.get("s1", size).map(|entry| entry.events.clone()),
        Some(vec![tricky.to_owned()])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_index_rebuilt_after_load_keeps_working() {
    // The persisted entry is only useful if a query can go on to use it. Here the in-memory index is
    // handed a session it does not yet have, and must accept it through the ordinary `insert` path.
    let dir = scratch("rebuild");
    let size = write_log(&dir, "s1", 8);
    write_log(&dir, "s2", 8);
    let path = path_in(&dir);
    save(&path, &[record("s1", size, &["first"])]).expect("saved");

    let mut index = load(&path).expect("loaded");
    index.insert(
        "s2",
        SearchIndexEntry {
            size: 8,
            events: vec!["second".to_owned()],
        },
    );

    assert_eq!(index.len(), 2);
    assert!(index.get("s1", size).is_some());
    assert!(index.get("s2", 8).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
