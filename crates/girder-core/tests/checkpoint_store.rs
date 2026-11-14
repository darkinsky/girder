//! Tests for the durable checkpoint store.
//!
//! The properties here are the ones a rewind depends on, and each pair below is a way for the store
//! to look correct while being wrong:
//!
//! - **the newest checkpoint is the highest position, not the last one written.** A store that
//!   answered with insertion order would send a rewind to a point the conversation had already left.
//! - **a truncation removes exactly the records past the cut.** Keeping them leaves `latest` naming a
//!   position beyond the end of the log; removing too many destroys the point the user rewound to.
//! - **an id that escapes the directory is refused before anything is written.** Accepting one writes
//!   a session's checkpoints outside the store, where nothing will ever find them again.
//! - **an interrupted write leaves the previous file.** Every save is a whole-file replacement, so a
//!   torn read is the failure that would make a checkpoint list unparsable rather than stale.
//!
//! The module is included by path rather than through the crate's re-exports, because the crate root
//! is wired up separately. It resolves `girder_core::…` through the external crate, so these tests
//! compile against the module as written and against the real [`girder_core::write_atomic`] it uses.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/checkpoint_store.rs"]
mod checkpoint_store;

use checkpoint_store::{Checkpoint, FILE_SUFFIX, Kind, Store, StoreError, now_ms};
use std::path::PathBuf;

// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh, empty directory per test, under the temp directory.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-ckpt-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A checkpoint at `seq`, taken automatically, with no summary.
fn at(session: &str, seq: u64) -> Checkpoint {
    Checkpoint {
        session: session.to_owned(),
        seq,
        kind: Kind::Automatic,
        summary: None,
        at_ms: 1_700_000_000_000 + seq,
    }
}

/// The sequences of a checkpoint list, for assertions about order and contents.
fn seqs(checkpoints: &[Checkpoint]) -> Vec<u64> {
    checkpoints.iter().map(|checkpoint| checkpoint.seq).collect()
}

/// The file names directly inside `dir`, sorted.
fn names(dir: &PathBuf) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("a readable directory")
        .map(|entry| {
            entry
                .expect("a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

// ---- round trip --------------------------------------------------------------------------------

#[test]
fn a_saved_checkpoint_reads_back_unchanged() {
    let dir = scratch("roundtrip");
    let store = Store::new(&dir);
    let checkpoint = Checkpoint {
        session: "s1".to_owned(),
        seq: 42,
        kind: Kind::Compaction,
        summary: Some("## Next Step\n- keep going".to_owned()),
        at_ms: 1_700_000_000_123,
    };

    store.save(checkpoint.clone()).expect("saved");

    // Every field, because each one is a distinct failure if it is lost: `seq` addresses the log,
    // `kind` decides whether a pruning policy may drop it, and `summary` is the only copy of what a
    // compaction removed.
    assert_eq!(store.load("s1").expect("loaded"), vec![checkpoint]);
}

#[test]
fn the_kind_has_a_stable_on_disk_spelling() {
    let dir = scratch("kind");
    let store = Store::new(&dir);
    let mut checkpoint = at("s1", 1);
    checkpoint.kind = Kind::Manual;
    store.save(checkpoint).expect("saved");

    let path = store.path_for("s1").expect("a path");
    let text = std::fs::read_to_string(&path).expect("readable");
    // The field names and the variant spelling are the format. This pins the one that a rename
    // would change silently, since nothing in the type system refers to it.
    assert!(text.contains("\"kind\":\"manual\""), "{text}");
    assert!(text.contains("\"session\":\"s1\""), "{text}");
}

#[test]
fn a_summary_is_optional_and_survives_either_state() {
    let dir = scratch("summary");
    let store = Store::new(&dir);
    let mut with = at("s1", 1);
    with.summary = Some("nothing".to_owned());
    let without = at("s1", 2);
    store.save(with.clone()).expect("saved");
    store.save(without.clone()).expect("saved");

    let loaded = store.load("s1").expect("loaded");
    // `None` and `Some("")` are different statements — "no summary" versus "a summary that is
    // empty" — so the round trip must not collapse them.
    assert_eq!(loaded, vec![with, without]);
}

// ---- ordering ----------------------------------------------------------------------------------

#[test]
fn checkpoints_come_back_ordered_by_seq() {
    let dir = scratch("ordered");
    let store = Store::new(&dir);
    // Written in an order no rewind would produce, so a store that simply echoed the file would fail.
    for seq in [30, 10, 20, 5] {
        store.save(at("s1", seq)).expect("saved");
    }

    assert_eq!(seqs(&store.load("s1").expect("loaded")), vec![5, 10, 20, 30]);
}

#[test]
fn saving_the_same_position_twice_replaces_it() {
    let dir = scratch("unique");
    let store = Store::new(&dir);
    let mut first = at("s1", 7);
    first.kind = Kind::Automatic;
    let mut second = at("s1", 7);
    second.kind = Kind::Manual;
    store.save(first).expect("saved");
    store.save(second.clone()).expect("replaced");

    // Two records at one position describe the same prefix of the log, so a list of both would make
    // "the checkpoint at 7" ambiguous; the later one is the better answer.
    assert_eq!(store.load("s1").expect("loaded"), vec![second]);
}

// ---- latest ------------------------------------------------------------------------------------

#[test]
fn latest_is_the_highest_seq_even_when_it_was_written_first() {
    let dir = scratch("latest");
    let store = Store::new(&dir);
    // 9 first, 3 second: insertion order would answer 3 and send a rewind backwards.
    store.save(at("s1", 9)).expect("saved");
    store.save(at("s1", 3)).expect("saved");

    let latest = store.latest("s1").expect("loaded").expect("a checkpoint");
    assert_eq!(latest.seq, 9);
}

#[test]
fn latest_of_an_empty_session_is_none() {
    let dir = scratch("latest-empty");
    let store = Store::new(&dir);
    assert!(store.latest("s1").expect("loaded").is_none());
}

#[test]
fn latest_ignores_other_sessions() {
    let dir = scratch("latest-scope");
    let store = Store::new(&dir);
    store.save(at("s1", 4)).expect("saved");
    store.save(at("s2", 400)).expect("saved");

    assert_eq!(store.latest("s1").expect("loaded").expect("one").seq, 4);
}

// ---- truncation --------------------------------------------------------------------------------

#[test]
fn truncate_after_keeps_the_position_and_drops_the_rest() {
    let dir = scratch("truncate");
    let store = Store::new(&dir);
    for seq in 1..=5 {
        store.save(at("s1", seq)).expect("saved");
    }

    let removed = store.truncate_after("s1", 3).expect("truncated");

    // Exactly two, and the position itself survives: it is where the log was rewound to, so the
    // record describing it is still true.
    assert_eq!(removed, 2);
    assert_eq!(seqs(&store.load("s1").expect("loaded")), vec![1, 2, 3]);
}

#[test]
fn truncate_after_the_latest_touches_nothing() {
    let dir = scratch("truncate-noop");
    let store = Store::new(&dir);
    for seq in 1..=3 {
        store.save(at("s1", seq)).expect("saved");
    }
    let path = store.path_for("s1").expect("a path");
    let before = std::fs::read(&path).expect("readable");

    let removed = store.truncate_after("s1", 3).expect("truncated");

    assert_eq!(removed, 0);
    // Not rewritten: a write with nothing to write is one more window for a reader to see a partial
    // file, for no gain.
    assert_eq!(std::fs::read(&path).expect("readable"), before);
}

#[test]
fn truncate_after_an_unknown_session_removes_nothing() {
    let dir = scratch("truncate-empty");
    let store = Store::new(&dir);
    assert_eq!(store.truncate_after("s1", 5).expect("truncated"), 0);
    assert!(
        store.load("s1").expect("loaded").is_empty(),
        "truncating a session with no file must not create one"
    );
    assert!(names(&dir).is_empty(), "no file was written: {:?}", names(&dir));
}

#[test]
fn truncate_after_zero_clears_every_checkpoint() {
    let dir = scratch("truncate-all");
    let store = Store::new(&dir);
    store.save(at("s1", 1)).expect("saved");
    store.save(at("s1", 2)).expect("saved");

    assert_eq!(store.truncate_after("s1", 0).expect("truncated"), 2);
    assert!(store.load("s1").expect("loaded").is_empty());
}

#[test]
fn truncating_one_session_leaves_another_alone() {
    let dir = scratch("truncate-scope");
    let store = Store::new(&dir);
    store.save(at("s1", 1)).expect("saved");
    store.save(at("s2", 1)).expect("saved");
    store.save(at("s2", 2)).expect("saved");

    store.truncate_after("s1", 0).expect("truncated");

    // The whole reason for a file per session: a rewind in one conversation must not be able to
    // invalidate the checkpoints of another.
    assert!(store.load("s1").expect("loaded").is_empty());
    assert_eq!(seqs(&store.load("s2").expect("loaded")), vec![1, 2]);
}

// ---- session id validation ---------------------------------------------------------------------

#[test]
fn an_unusable_session_id_is_refused_before_anything_is_written() {
    let dir = scratch("invalid");
    let store = Store::new(&dir);
    for id in ["", ".", "..", "a/b", "a\\b", "../escape", "a..b", "nul\0byte"] {
        assert!(
            matches!(
                store.path_for(id),
                Err(StoreError::InvalidSession { .. })
            ),
            "`{id}` should not name a file"
        );
        // Save and load refuse through the same check, so a caller cannot get in by another door.
        let mut checkpoint = at(id, 1);
        checkpoint.session = id.to_owned();
        assert!(
            matches!(store.save(checkpoint), Err(StoreError::InvalidSession { .. })),
            "`{id}` should not be savable"
        );
        assert!(matches!(
            store.load(id),
            Err(StoreError::InvalidSession { .. })
        ));
        assert!(matches!(
            store.truncate_after(id, 0),
            Err(StoreError::InvalidSession { .. })
        ));
    }

    assert!(
        names(&dir).is_empty(),
        "a refused id must leave nothing behind: {:?}",
        names(&dir)
    );
}

#[test]
fn a_traversal_id_cannot_reach_outside_the_directory() {
    let dir = scratch("traversal");
    let store = Store::new(&dir);
    let checkpoint = Checkpoint {
        session: "../escaped".to_owned(),
        ..at("x", 1)
    };

    assert!(store.save(checkpoint).is_err());

    // The concrete consequence, checked on the filesystem rather than on the error: nothing was
    // created in the parent, which is the directory the id was trying to address.
    let parent = dir.parent().expect("a parent directory");
    let escaped = parent.join(format!("escaped{FILE_SUFFIX}"));
    assert!(!escaped.exists(), "`{}` was written", escaped.display());
}

#[test]
fn an_unknown_session_is_empty_not_an_error() {
    let dir = scratch("missing");
    let store = Store::new(&dir);
    // A new session is the normal case; a caller listing rewind points should not have to special-case
    // it against a session whose file was pruned to nothing.
    assert!(store.load("never-checkpointed").expect("loaded").is_empty());
}

// ---- damage, and atomicity ---------------------------------------------------------------------

#[test]
fn a_damaged_file_is_reported_rather_than_read_as_empty() {
    let dir = scratch("damaged");
    let store = Store::new(&dir);
    let path = store.path_for("s1").expect("a path");
    std::fs::write(&path, b"{\"not\":\"a checkpoint file\"").expect("written");

    // "No checkpoints" would become "the rewind button is gone"; the caller can only rebuild from the
    // log if it is told the file is unusable.
    assert!(matches!(store.load("s1"), Err(StoreError::Corrupt { .. })));
    assert!(matches!(
        store.truncate_after("s1", 10),
        Err(StoreError::Corrupt { .. })
    ));
    assert!(
        matches!(store.save(at("s1", 1)), Err(StoreError::Corrupt { .. })),
        "a save must not replace a file it could not read"
    );

    // And the file is untouched, byte for byte: a refused operation that truncated the file would
    // destroy the evidence along with the data.
    assert_eq!(
        std::fs::read(&path).expect("readable"),
        b"{\"not\":\"a checkpoint file\""
    );
}

#[test]
fn a_file_holding_another_session_is_refused() {
    let dir = scratch("foreign");
    let store = Store::new(&dir);
    // Copied from another session, or edited by hand: the positions describe a log this session does
    // not have, so serving them would rewind to the wrong place.
    let path = store.path_for("s1").expect("a path");
    let foreign = serde_json::to_vec(&vec![at("s2", 3)]).expect("serialised");
    std::fs::write(&path, foreign).expect("written");

    assert!(matches!(store.load("s1"), Err(StoreError::Corrupt { .. })));
}

#[test]
fn saving_leaves_no_staging_files_behind() {
    let dir = scratch("staging");
    let store = Store::new(&dir);
    for seq in 1..=5 {
        store.save(at("s1", seq)).expect("saved");
    }

    // `write_atomic` stages a sibling and renames it; a leftover sibling would be debris nobody
    // cleans up, and a sign that the rename is not what is doing the commit.
    assert_eq!(names(&dir), vec![format!("s1{FILE_SUFFIX}")]);
}

#[test]
fn a_reader_never_sees_a_torn_file() {
    let dir = scratch("torn");
    let store = Store::new(&dir);
    let writer = store.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_writer = std::sync::Arc::clone(&stop);

    let handle = std::thread::spawn(move || {
        for seq in 1..=64 {
            writer.save(at("s1", seq)).expect("saved");
        }
        stop_writer.store(true, std::sync::atomic::Ordering::Release);
    });

    let mut reads = 0_u32;
    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        // Every read is of a file that is being replaced underneath it. A non-atomic write would show
        // up here as `Corrupt`, since half of the new records over the tail of the old file is not
        // valid JSON.
        let loaded = store.load("s1").expect("no torn read");
        let seen = seqs(&loaded);
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "the order broke mid-write: {seen:?}"
        );
        reads += 1;
    }
    handle.join().expect("the writer finished");

    assert!(reads > 0, "the reader never ran");
    assert_eq!(seqs(&store.load("s1").expect("loaded")), (1..=64).collect::<Vec<_>>());
}

// ---- clock -------------------------------------------------------------------------------------

#[test]
fn now_ms_is_in_milliseconds() {
    // A fixed past instant, so the assertion holds for any correct clock and fails loudly if the unit
    // ever becomes seconds: 1_600_000_000_000 ms is 2020-09-13, and 1_600_000_000 is 1970-01-19.
    assert!(
        now_ms() > 1_600_000_000_000,
        "now_ms() is not in milliseconds: {}",
        now_ms()
    );
}
