//! Tests for the persistent terminal output store.
//!
//! The tests are weighted toward the failures that would make persistence *worse* than no
//! persistence, because those are the ones a round-trip test cannot see:
//!
//! - **an id that escapes the directory.** The id arrives from a client, and `root.join(id)` with an
//!   id of `../../x` writes wherever the caller likes. Every traversal shape is refused here, and
//!   the refusal must leave nothing behind — a rejected write that still created a file is a
//!   rejection in the return value only.
//! - **a torn transcript.** The file is rewritten whole on every save, so a non-atomic write is a
//!   window in which an old transcript and a new one are mixed, which is *valid text* claiming
//!   something the terminal never printed. Unlike the session log there is no framing to detect it.
//! - **unbounded growth.** One file per terminal, forever, is a deployment that fills its disk. The
//!   prune tests pin that age is measured from the file, that a threshold wider than the clock is
//!   not read as "delete everything", and that the boundary is keep-not-delete.
//! - **a listing that is not a usable set of ids.** Every id `list` returns must be one `load`
//!   accepts, and the staging siblings of writes in flight must never appear.
//!
//! The module is compiled in by path rather than reached through the crate root, so this suite runs
//! against the file as written and fails if the module ever stops being self-contained. It resolves
//! `girder_core::…` through the real crate, which is where `write_atomic` comes from.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/terminal_persist.rs"]
mod terminal_persist;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use terminal_persist::{PersistError, Store};

// ---- fixtures ----------------------------------------------------------------------------------

/// A store plus the directory it writes into, both under a fresh scratch directory.
///
/// The root is a `store` subdirectory of the scratch directory and is *not* created, so the
/// directory a failed save would escape into is empty and controlled: an escaping id that wrote
/// anywhere under it is visible as a file in `parent`.
struct Fixture {
    /// The store under test.
    store: Store,
    /// The directory transcripts are written to.
    root: PathBuf,
    /// The directory above the root, which nothing should ever be written into.
    parent: PathBuf,
}

/// A fresh scratch directory per test, under the temp directory.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-terminal-persist-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// A store over a fresh, not-yet-created directory.
fn fixture(name: &str) -> Fixture {
    let parent = scratch(name);
    let root = parent.join("store");
    Fixture {
        store: Store::new(&root),
        root,
        parent,
    }
}

/// Every file name directly in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("listed")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Forces a file's modification time, so a prune test does not have to sleep.
fn set_modified(path: &Path, millis: u64) {
    let when = UNIX_EPOCH + Duration::from_millis(millis);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("opened");
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("timestamp set");
}

/// The current time in milliseconds since the epoch.
fn now_ms() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch");
    u64::try_from(now.as_millis()).expect("fits")
}

// ---- saving and loading ------------------------------------------------------------------------

#[test]
fn a_saved_transcript_reads_back_unchanged() {
    let fixture = fixture("roundtrip");
    let text = "$ ls\r\nsrc\r\nCargo.toml\r\n";

    let bytes = fixture.store.save("terminal-1", text).expect("saved");
    assert_eq!(bytes, u64::try_from(text.len()).expect("fits"));
    assert_eq!(
        fixture.store.load("terminal-1").expect("loaded"),
        Some(text.into())
    );
}

#[test]
fn saving_again_replaces_the_transcript_rather_than_extending_it() {
    // `retain` hands over the transcript as a whole, so a store that appended would duplicate
    // everything already saved on every call.
    let fixture = fixture("replace");
    fixture.store.save("terminal-1", "first\n").expect("saved");
    fixture.store.save("terminal-1", "second\n").expect("saved");

    assert_eq!(
        fixture.store.load("terminal-1").expect("loaded"),
        Some("second\n".to_owned())
    );
}

#[test]
fn an_empty_transcript_is_a_stored_transcript() {
    // A terminal that printed nothing still has a transcript, and telling it apart from one that was
    // never stored is the difference between "no output yet" and "no such terminal".
    let fixture = fixture("empty-text");
    assert_eq!(fixture.store.save("terminal-1", "").expect("saved"), 0);
    assert_eq!(
        fixture.store.load("terminal-1").expect("loaded"),
        Some(String::new())
    );
}

#[test]
fn a_transcript_reads_back_byte_for_byte_including_non_ascii() {
    // Terminal output is UTF-8, including box drawing and a client's cursor moves; a store that
    // mangled either would corrupt what the user saw.
    let fixture = fixture("unicode");
    let text = "┌─ name ─┐\r\n│ café │\r\n└───────┘\r\n\u{1b}[0m";
    fixture.store.save("terminal-1", text).expect("saved");
    assert_eq!(
        fixture.store.load("terminal-1").expect("loaded"),
        Some(text.into())
    );
}

// ---- invalid ids (core) ------------------------------------------------------------------------

#[test]
fn an_id_that_escapes_the_directory_is_refused_everywhere() {
    // The whole point of the validation: an id is a request field, and a request field must not be
    // able to name a path outside the store.
    let fixture = fixture("escape");
    let hostile = [
        "..",
        "../escape",
        "../../etc/passwd",
        "a/b",
        "/absolute",
        "nested/deep",
        "win\\path",
        "trailing..",
        "..hidden",
    ];

    for id in hostile {
        // Only the invalid-id error: an `Io` here would mean the write was attempted.
        assert!(
            matches!(
                fixture.store.save(id, "x"),
                Err(PersistError::InvalidId { .. })
            ),
            "save accepted `{id}`"
        );
        assert!(
            matches!(fixture.store.load(id), Err(PersistError::InvalidId { .. })),
            "load accepted `{id}`"
        );
        assert!(
            matches!(
                fixture.store.remove(id),
                Err(PersistError::InvalidId { .. })
            ),
            "remove accepted `{id}`"
        );
    }

    // Nothing was created anywhere: not the root, and not the directory a traversal would reach.
    assert!(!fixture.root.exists(), "a rejected save created the root");
    assert!(
        names(&fixture.parent).is_empty(),
        "a rejected id escaped into the parent: {:?}",
        names(&fixture.parent)
    );
}

#[test]
fn a_refused_save_leaves_the_previous_transcript_intact() {
    // Validation happens before the write, so a rejected save is not a partial one.
    let fixture = fixture("refused-save");
    fixture.store.save("terminal-1", "good\n").expect("saved");

    assert!(matches!(
        fixture.store.save("../terminal-1", "bad\n"),
        Err(PersistError::InvalidId { .. })
    ));
    assert_eq!(
        fixture.store.load("terminal-1").expect("loaded"),
        Some("good\n".to_owned())
    );
}

#[test]
fn the_empty_id_and_over_long_ids_are_refused() {
    let fixture = fixture("empty-id");
    assert!(matches!(
        fixture.store.save("", "x"),
        Err(PersistError::InvalidId { .. })
    ));
    // A name past the filesystem limit would otherwise fail with an errno that names neither the id
    // nor the reason.
    let long = "t".repeat(4096);
    assert!(matches!(
        fixture.store.save(&long, "x"),
        Err(PersistError::InvalidId { .. })
    ));
}

#[test]
fn a_dot_prefixed_id_is_refused_because_that_namespace_is_staging_files() {
    // `write_atomic` stages a sibling named `.<file>.tmp-<pid>-<n>`; a transcript under the same
    // prefix could be mistaken for one, and a listing could show a half-written file as a terminal.
    let fixture = fixture("dot-id");
    assert!(matches!(
        fixture.store.save(".terminal-1", "x"),
        Err(PersistError::InvalidId { .. })
    ));
}

#[test]
fn the_invalid_id_error_names_the_id() {
    // The message is what a client sees, and an id it cannot recognise is one it cannot correct.
    let fixture = fixture("id-message");
    match fixture.store.save("../x", "y") {
        Err(PersistError::InvalidId { id }) => assert_eq!(id, "../x"),
        other => panic!("expected an invalid id, got {other:?}"),
    }
}

// ---- absence -----------------------------------------------------------------------------------

#[test]
fn loading_a_transcript_that_was_never_saved_answers_none() {
    let fixture = fixture("missing");
    assert_eq!(fixture.store.load("terminal-never").expect("loaded"), None);
}

#[test]
fn removing_a_transcript_that_was_never_saved_reports_false() {
    let fixture = fixture("remove-missing");
    assert!(!fixture.store.remove("terminal-never").expect("removed"));
}

#[test]
fn a_store_whose_directory_does_not_exist_is_empty_rather_than_broken() {
    // The directory is created by the first save, so a store that has never saved is a normal state:
    // listing and pruning it must not fail, and nothing needs to exist on disk.
    let dir = scratch("no-directory");
    let root = dir.join("never-created");
    let store = Store::new(&root);

    assert!(store.list().expect("listed").is_empty());
    assert_eq!(store.load("terminal-1").expect("loaded"), None);
    assert_eq!(store.prune(now_ms(), 0).expect("pruned"), 0);
    assert!(!root.exists(), "a read created the directory");

    // And a save creates it.
    store.save("terminal-1", "hi\n").expect("saved");
    assert!(root.is_dir());
}

// ---- removing ----------------------------------------------------------------------------------

#[test]
fn a_removed_transcript_is_gone() {
    let fixture = fixture("remove");
    fixture.store.save("terminal-1", "output\n").expect("saved");
    assert!(fixture.store.remove("terminal-1").expect("removed"));
    assert_eq!(fixture.store.load("terminal-1").expect("loaded"), None);
    // The second removal is a no-op, not an error: two clients may both close one terminal.
    assert!(!fixture.store.remove("terminal-1").expect("removed again"));
}

#[test]
fn removing_one_transcript_leaves_the_others() {
    let fixture = fixture("remove-one");
    fixture.store.save("terminal-1", "one\n").expect("saved");
    fixture.store.save("terminal-2", "two\n").expect("saved");

    fixture.store.remove("terminal-1").expect("removed");
    assert_eq!(
        fixture.store.list().expect("listed"),
        vec!["terminal-2".to_owned()]
    );
    assert_eq!(
        fixture.store.load("terminal-2").expect("loaded"),
        Some("two\n".to_owned())
    );
}

// ---- listing -----------------------------------------------------------------------------------

#[test]
fn listing_returns_every_stored_terminal_sorted() {
    let fixture = fixture("list");
    for id in ["terminal-3", "terminal-1", "terminal-2"] {
        fixture.store.save(id, "x\n").expect("saved");
    }
    assert_eq!(
        fixture.store.list().expect("listed"),
        vec![
            "terminal-1".to_owned(),
            "terminal-2".to_owned(),
            "terminal-3".to_owned()
        ]
    );
}

#[test]
fn an_empty_store_lists_nothing() {
    let fixture = fixture("list-empty");
    assert!(fixture.store.list().expect("listed").is_empty());
}

#[test]
fn listing_ignores_files_that_are_not_transcripts() {
    // A store directory is a directory: a README, an editor's swap file, and a stray file with the
    // right extension but an illegal stem all live beside the transcripts. Every id `list` returns
    // must be one `load` accepts, so none of these may appear.
    let fixture = fixture("list-noise");
    fixture.store.save("terminal-1", "kept\n").expect("saved");

    std::fs::write(fixture.root.join("README"), "notes\n").expect("written");
    std::fs::write(fixture.root.join("terminal-2.txt"), "wrong extension\n").expect("written");
    std::fs::write(
        fixture.root.join(".terminal-3.log.tmp-1-0"),
        "half a write\n",
    )
    .expect("written");
    std::fs::create_dir(fixture.root.join("terminal-4.log")).expect("created");

    assert_eq!(
        fixture.store.list().expect("listed"),
        vec!["terminal-1".to_owned()]
    );
}

#[test]
fn listing_never_includes_the_staging_file_of_a_write_in_flight() {
    // The failure this prevents is a prune deleting a staging file under a save that is halfway
    // through the rename, which turns a crash-safe write into a lost one.
    let fixture = fixture("list-staging");
    fixture.store.save("terminal-1", "text\n").expect("saved");
    // The shape `write_atomic` stages, spelled out so this test fails if either side changes.
    let staging = fixture.root.join(".terminal-1.log.tmp-4242-7");
    std::fs::write(&staging, "partial").expect("written");

    assert_eq!(
        fixture.store.list().expect("listed"),
        vec!["terminal-1".to_owned()]
    );
    assert_eq!(
        fixture.store.load("terminal-1").expect("loaded"),
        Some("text\n".to_owned())
    );
    // Prune with a threshold that removes everything, and the staging file is still there.
    fixture.store.prune(now_ms(), 0).expect("pruned");
    assert!(staging.exists(), "prune removed a staging file");
}

// ---- pruning (core) ----------------------------------------------------------------------------

#[test]
fn pruning_removes_only_transcripts_older_than_the_threshold() {
    // The property the policy exists for: age is measured from each file, so a recently written
    // terminal survives while an untouched one goes.
    let fixture = fixture("prune");
    fixture.store.save("terminal-old", "old\n").expect("saved");
    fixture.store.save("terminal-new", "new\n").expect("saved");

    let now = now_ms();
    set_modified(&fixture.root.join("terminal-old.log"), now - 10 * 60 * 1000);

    // Ten minutes old against a five-minute threshold: one goes.
    let removed = fixture.store.prune(now, 5 * 60 * 1000).expect("pruned");
    assert_eq!(removed, 1);
    assert_eq!(
        fixture.store.list().expect("listed"),
        vec!["terminal-new".to_owned()]
    );
    assert_eq!(fixture.store.load("terminal-old").expect("loaded"), None);
}

#[test]
fn pruning_keeps_a_transcript_exactly_at_the_threshold() {
    // The boundary is keep: a file exactly as old as the policy is not *older* than it, so "keep an
    // hour" does not remove an hour-old transcript on the tick it turns an hour old.
    let fixture = fixture("prune-boundary");
    fixture.store.save("terminal-1", "x\n").expect("saved");
    let path = fixture.root.join("terminal-1.log");
    set_modified(&path, 1_000_000);

    // One millisecond past the cutoff: removed, so the test proves the boundary is the only thing
    // keeping the next case alive.
    assert_eq!(fixture.store.prune(1_000_001, 0).expect("pruned"), 1);

    fixture.store.save("terminal-2", "y\n").expect("saved");
    set_modified(&fixture.root.join("terminal-2.log"), 1_000_000);
    // Exactly at the cutoff: kept.
    assert_eq!(fixture.store.prune(1_000_001, 1).expect("pruned"), 0);
    assert_eq!(
        fixture.store.list().expect("listed"),
        vec!["terminal-2".to_owned()]
    );
}

#[test]
fn pruning_with_a_threshold_wider_than_the_clock_removes_nothing() {
    // A misconfigured policy must not delete the store. The cutoff saturates at the epoch, where
    // nothing is old enough.
    let fixture = fixture("prune-wide");
    fixture.store.save("terminal-1", "x\n").expect("saved");

    assert_eq!(fixture.store.prune(1_000, u64::MAX).expect("pruned"), 0);
    assert_eq!(
        fixture.store.list().expect("listed"),
        vec!["terminal-1".to_owned()]
    );
}

#[test]
fn pruning_ignores_files_that_are_not_transcripts() {
    let fixture = fixture("prune-noise");
    fixture.store.save("terminal-1", "x\n").expect("saved");
    std::fs::write(fixture.root.join("README"), "notes\n").expect("written");

    // A threshold that removes every transcript removes the transcript and not the README.
    assert_eq!(fixture.store.prune(now_ms(), 0).expect("pruned"), 1);
    assert!(fixture.root.join("README").exists());
}

#[test]
fn pruning_a_store_that_is_already_empty_reports_zero() {
    let fixture = fixture("prune-empty");
    assert_eq!(fixture.store.prune(now_ms(), 0).expect("pruned"), 0);
    assert!(fixture.store.list().expect("listed").is_empty());
}

// ---- atomicity ---------------------------------------------------------------------------------

#[test]
fn a_save_leaves_no_staging_file_behind() {
    // A staging file that outlives its save is one nobody cleans up, and — because it is named like
    // a transcript's sibling — one that looks like a terminal to anyone reading the directory.
    let fixture = fixture("atomic-clean");
    for round in 0..8 {
        fixture
            .store
            .save("terminal-1", &format!("round {round}\n"))
            .expect("saved");
        assert_eq!(names(&fixture.root), vec!["terminal-1.log".to_owned()]);
    }
}

#[test]
fn a_reader_sees_a_whole_transcript_or_the_previous_one() {
    // The torn-write case: a transcript is one text blob with no framing, so a reader that caught a
    // half-finished write would read valid text that the terminal never printed. A rename makes the
    // two states the only observable ones, so every concurrent read is a complete transcript.
    let fixture = fixture("atomic-torn");
    let first = "a".repeat(64 * 1024);
    let second = "b".repeat(64 * 1024);
    fixture.store.save("terminal-1", &first).expect("saved");

    let reader = {
        let store = fixture.store.clone();
        let (first, second) = (first.clone(), second.clone());
        std::thread::spawn(move || {
            for _ in 0..200 {
                let seen = store.load("terminal-1").expect("loaded").expect("present");
                assert!(
                    seen == first || seen == second,
                    "read a transcript that was neither version: {} bytes",
                    seen.len()
                );
            }
        })
    };

    for round in 0..40 {
        let text = if round % 2 == 0 { &second } else { &first };
        fixture.store.save("terminal-1", text).expect("saved");
    }
    reader.join().expect("reader finished");

    // And the last write is the one that is there. The loop's final round is odd, so that is `first`.
    assert_eq!(
        fixture.store.load("terminal-1").expect("loaded"),
        Some(first)
    );
}

#[test]
fn the_stored_file_holds_the_transcript_as_plain_text() {
    // The transcript is the artefact someone reads after a failure, so it is stored as what it is.
    let fixture = fixture("plain-text");
    fixture.store.save("terminal-1", "hello\n").expect("saved");
    let bytes = std::fs::read(fixture.root.join("terminal-1.log")).expect("read");
    assert_eq!(bytes, b"hello\n");
}
