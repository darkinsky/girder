//! Tests for the merged persistence layer.
//!
//! The module under test is small, so the tests are weighted toward the failures a round-trip test
//! cannot see — the ones where each store works on its own and the *combination* is still wrong:
//!
//! - **the three stores must share one directory.** A layer whose terminals and checkpoints land in
//!   different trees is one a deployment cannot measure or prune, which is the entire reason this
//!   type exists. Every test here writes through one store and looks for the file beside the others.
//! - **a prune must not delete what it does not own.** The directory also holds session logs, title
//!   sidecars, and archive marks. A sweep that removed everything old would delete conversations, and
//!   a test that only counted removals would call that success.
//! - **the suffixes must not collide with `{id}.jsonl.zst`.** If one did, a session log and a
//!   checkpoint could not both exist for a session, and the store that wrote second would destroy the
//!   other's file. The collision test is the one that pins the naming contract.
//! - **the footprint must count what the stores actually wrote.** It re-derives the file suffixes
//!   from constants of its own, so it can drift from the stores it is measuring; the assertions below
//!   compare against files produced by the real stores rather than against the constant.
//!
//! The module is compiled in by path rather than reached through the crate root, because the crate
//! root is wired up separately. It resolves `girder_core::…` through the real crate, so this suite
//! runs against the file as written and against the stores it composes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting, and \
              the indexes are over fixed-size literals defined in the test itself"
)]

#[path = "../src/session/persistence.rs"]
pub mod session_persistence;

use girder_core::{CheckpointKind, StoredCheckpoint};
use session_persistence::{Footprint, Persistence, Pruned};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh, empty directory per test, under the temp directory.
///
/// Named after the test so a failure leaves a directory a person can find, and counter-suffixed
/// because the same test name can run in parallel with itself across `cargo test` invocations.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-persist-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// The current time in milliseconds since the epoch.
fn now_ms() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch");
    u64::try_from(now.as_millis()).expect("fits")
}

/// Forces a file's modification time, so a prune test does not have to sleep.
///
/// The prune compares each file's own timestamp against the cutoff, so this is the only way to make a
/// file old without waiting; sleeping for the retention window would make the suite take days.
fn set_modified(path: &Path, millis: u64) {
    let when = UNIX_EPOCH + Duration::from_millis(millis);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("opened");
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("timestamp set");
}

/// Every file name in a directory, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the directory")
        .map(|entry| entry.expect("an entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A checkpoint for `session` at `seq`.
fn checkpoint(session: &str, seq: u64) -> StoredCheckpoint {
    StoredCheckpoint::now(session, seq, CheckpointKind::Manual)
}

// ---- construction ------------------------------------------------------------------------------

#[test]
fn new_leaves_the_directory_alone() {
    // Construction must not create the root. A layer over a directory that never receives data — a
    // client that only reports on a deployment, a probe — would otherwise leave empty directories
    // behind everywhere it was asked to look, and the directory's existence would stop meaning "this
    // deployment has data".
    let root = scratch("new-no-create").join("sessions");
    let persistence = Persistence::new(&root);
    assert!(
        !root.exists(),
        "constructing must not create the root, but `{}` exists",
        root.display()
    );
    // Still usable, just empty: every read answers as it would for an absent directory.
    assert!(
        persistence.terminals().list().expect("lists").is_empty(),
        "a store over an absent directory lists nothing"
    );
    assert_eq!(persistence.footprint(), Footprint::default());
}

#[test]
fn stores_all_point_at_the_one_root() {
    // The property the type exists for. Asserted through the files each store writes, not through
    // public accessors, because a layer that reported one root while writing to another would pass an
    // accessor check and fail every caller.
    let root = scratch("shared-root");
    let persistence = Persistence::new(&root);

    persistence
        .terminals()
        .save("term-1", "hello from a terminal")
        .expect("saved");
    persistence
        .checkpoints()
        .save(checkpoint("sess-1", 3))
        .expect("saved");

    let written = names(&root);
    assert!(
        written.contains(&"term-1.log".to_owned()),
        "the transcript must land in the shared root, got {written:?}"
    );
    assert!(
        written.contains(&"sess-1.checkpoints.json".to_owned()),
        "the checkpoints must land in the shared root, got {written:?}"
    );
    assert_eq!(
        persistence.search_index_path().parent(),
        Some(root.as_path()),
        "the search index must be addressed inside the same root"
    );
}

// ---- the three stores are usable ---------------------------------------------------------------

#[test]
fn checkpoints_are_readable_through_the_layer() {
    let root = scratch("checkpoints");
    let persistence = Persistence::new(&root);

    persistence
        .checkpoints()
        .save(checkpoint("sess-1", 4))
        .expect("saved");
    persistence
        .checkpoints()
        .save(checkpoint("sess-1", 9))
        .expect("saved");

    let stored = persistence.checkpoints().load("sess-1").expect("loaded");
    assert_eq!(stored.len(), 2, "both positions must be stored");
    assert_eq!(stored[0].seq, 4);
    assert_eq!(stored[1].seq, 9);
    let latest = persistence
        .checkpoints()
        .latest("sess-1")
        .expect("loaded")
        .expect("a latest checkpoint");
    assert_eq!(latest.seq, 9, "the latest is the highest position");
}

#[test]
fn terminals_are_readable_through_the_layer() {
    let root = scratch("terminals");
    let persistence = Persistence::new(&root);

    persistence
        .terminals()
        .save("term-a", "$ cargo test\nok")
        .expect("saved");

    assert_eq!(
        persistence.terminals().load("term-a").expect("loaded"),
        Some("$ cargo test\nok".to_owned()),
        "the transcript must round-trip through the layer"
    );
    assert_eq!(
        persistence.terminals().list().expect("listed"),
        vec!["term-a".to_owned()]
    );
}

#[test]
fn the_search_index_is_addressed_by_the_layer() {
    let root = scratch("search");
    let persistence = Persistence::new(&root);

    // The path the layer hands out must be the one `search_persist` reads and writes, and the one its
    // `path_in` helper would produce for the same root. If these disagreed, a caller would write a
    // cache nothing ever loads.
    assert_eq!(
        persistence.search_index_path(),
        girder_core::search_index_path(&root)
    );
    assert_eq!(
        persistence.search_index_path().file_name(),
        Some(std::ffi::OsStr::new(".search-index.json"))
    );

    girder_core::save_search_index(persistence.search_index_path(), &[])
        .expect("the index must be writable at the path the layer reports");
    assert!(persistence.search_index_path().is_file());
}

#[test]
fn a_reopened_layer_sees_what_was_written() {
    // The round trip that matters for a restart: drop the object entirely, build a new one over the
    // same root, and read back the content rather than merely checking for a file.
    let root = scratch("reopen");
    {
        let persistence = Persistence::new(&root);
        persistence
            .terminals()
            .save("term-x", "survives the restart")
            .expect("saved");
        persistence
            .checkpoints()
            .save(checkpoint("sess-x", 7))
            .expect("saved");
    }

    let reopened = Persistence::new(&root);
    assert_eq!(
        reopened.terminals().load("term-x").expect("loaded"),
        Some("survives the restart".to_owned())
    );
    let latest = reopened
        .checkpoints()
        .latest("sess-x")
        .expect("loaded")
        .expect("a checkpoint");
    assert_eq!(latest.seq, 7);
}

// ---- prune -------------------------------------------------------------------------------------

#[test]
fn prune_removes_old_terminals_and_checkpoints_together() {
    // One call, two stores. The failure this pins is the one that motivates the type: a deployment
    // that pruned terminals and forgot checkpoints would look healthy while one of its two
    // directories grew without bound.
    let root = scratch("prune-both");
    let persistence = Persistence::new(&root);

    persistence
        .terminals()
        .save("old-term", "old output")
        .expect("saved");
    persistence
        .checkpoints()
        .save(checkpoint("old-sess", 1))
        .expect("saved");
    persistence
        .terminals()
        .save("new-term", "new output")
        .expect("saved");
    persistence
        .checkpoints()
        .save(checkpoint("new-sess", 1))
        .expect("saved");

    let now = now_ms();
    let old = now - 60 * 60 * 1000;
    set_modified(&root.join("old-term.log"), old);
    set_modified(&root.join("old-sess.checkpoints.json"), old);

    let pruned = persistence
        .prune(now, 30 * 60 * 1000)
        .expect("pruned without error");
    assert_eq!(
        pruned,
        Pruned {
            terminals: 1,
            checkpoints: 1
        },
        "one of each must be pruned"
    );

    let remaining = names(&root);
    assert!(
        remaining.contains(&"new-term.log".to_owned()),
        "a recent transcript must survive, got {remaining:?}"
    );
    assert!(
        remaining.contains(&"new-sess.checkpoints.json".to_owned()),
        "a recent checkpoint must survive, got {remaining:?}"
    );
    assert!(
        !remaining.contains(&"old-term.log".to_owned()),
        "the old transcript must be gone, got {remaining:?}"
    );
    assert!(
        !remaining.contains(&"old-sess.checkpoints.json".to_owned()),
        "the old checkpoint must be gone, got {remaining:?}"
    );
}

#[test]
fn prune_keeps_a_file_exactly_at_the_cutoff() {
    // The boundary is keep-not-delete, matching the terminal store's own rule. A policy that deleted
    // at the boundary would make "keep for one day" mean "keep for slightly less than one day", and
    // the two spellings are impossible to tell apart from outside.
    let root = scratch("prune-boundary");
    let persistence = Persistence::new(&root);
    persistence
        .terminals()
        .save("term", "output")
        .expect("saved");

    let now = now_ms();
    let cutoff = now - 1000;
    set_modified(&root.join("term.log"), cutoff);

    let pruned = persistence.prune(now, 1000).expect("pruned without error");
    assert_eq!(pruned.terminals, 0, "a file at the cutoff must be kept");
    assert!(root.join("term.log").is_file());
}

#[test]
fn prune_does_not_touch_files_it_does_not_own() {
    // The directory holds session logs and their sidecars. A sweep that removed everything old would
    // delete conversations — the one irreversible mistake available to a retention job — so the
    // non-checkpoint, non-transcript files must survive an aggressive prune.
    let root = scratch("prune-foreign");
    let persistence = Persistence::new(&root);

    let log = root.join("sess-1.jsonl.zst");
    let title = root.join("sess-1.title");
    let archived = root.join("sess-1.archived");
    let unrelated = root.join("README");
    for path in [&log, &title, &archived, &unrelated] {
        std::fs::write(path, b"do not delete").expect("wrote");
    }

    let now = now_ms();
    let old = now - 365 * 24 * 60 * 60 * 1000;
    for path in [&log, &title, &archived, &unrelated] {
        set_modified(path, old);
    }

    // A cutoff far in the future relative to the files: everything old enough, so only ownership can
    // protect them.
    let pruned = persistence.prune(now, 1000).expect("pruned without error");
    assert_eq!(
        pruned,
        Pruned::default(),
        "nothing of ours was there to prune"
    );

    for path in [&log, &title, &archived, &unrelated] {
        assert!(
            path.is_file(),
            "`{}` must survive a prune that does not own it",
            path.display()
        );
    }
}

#[test]
fn prune_removes_the_search_index() {
    // The index is a cache of every session at once, so it has no per-file age that a retention pass
    // can act on. Dropping the whole file is the only prune available, and it is safe because the next
    // query rebuilds it — but it must actually happen, or the cache is the one artefact that never
    // expires.
    let root = scratch("prune-search");
    let persistence = Persistence::new(&root);
    girder_core::save_search_index(persistence.search_index_path(), &[]).expect("wrote the index");
    assert!(persistence.search_index_path().is_file());

    persistence
        .prune(now_ms(), 60 * 60 * 1000)
        .expect("pruned without error");

    assert!(
        !persistence.search_index_path().is_file(),
        "the search index must be removed"
    );
}

#[test]
fn prune_without_a_search_index_is_not_an_error() {
    // A deployment nothing has searched has no cache. Reporting a failure there would make the first
    // retention pass on a fresh install fail, which teaches an operator to ignore the error.
    let root = scratch("prune-no-search");
    let persistence = Persistence::new(&root);
    let pruned = persistence
        .prune(now_ms(), 60 * 60 * 1000)
        .expect("an absent cache is not a failure");
    assert_eq!(pruned, Pruned::default());
}

#[test]
fn a_policy_wider_than_the_clock_prunes_nothing() {
    // `now - older_than` underflows when the window is wider than the clock; saturating puts the
    // cutoff at the epoch, so nothing is old enough. Without that, an uninterpretable policy would
    // read as "delete everything", which is the destructive reading of an ambiguous input.
    let root = scratch("prune-wide");
    let persistence = Persistence::new(&root);
    persistence
        .terminals()
        .save("term", "output")
        .expect("saved");
    persistence
        .checkpoints()
        .save(checkpoint("sess", 1))
        .expect("saved");

    let pruned = persistence
        .prune(1000, u64::MAX)
        .expect("pruned without error");
    assert_eq!(
        pruned,
        Pruned::default(),
        "a window wider than the clock must keep everything"
    );
    assert!(root.join("term.log").is_file());
    assert!(root.join("sess.checkpoints.json").is_file());
}

#[test]
fn prune_over_a_missing_directory_is_not_an_error() {
    let root = scratch("prune-missing").join("never-created");
    let persistence = Persistence::new(&root);
    let pruned = persistence
        .prune(now_ms(), 1000)
        .expect("nothing to prune is not a failure");
    assert_eq!(pruned, Pruned::default());
}

// ---- footprint ---------------------------------------------------------------------------------

#[test]
fn footprint_is_zero_for_an_empty_directory() {
    let root = scratch("footprint-empty");
    let persistence = Persistence::new(&root);
    assert_eq!(persistence.footprint(), Footprint::default());
    assert_eq!(persistence.footprint().total(), 0);
}

#[test]
fn footprint_is_zero_for_a_directory_that_does_not_exist() {
    // A probe over a deployment that has never run must report zero rather than fail: this number is
    // shown to a person, and "unavailable" is a worse answer than "nothing yet".
    let root = scratch("footprint-missing").join("never-created");
    let persistence = Persistence::new(&root);
    assert_eq!(persistence.footprint(), Footprint::default());
}

#[test]
fn footprint_counts_what_each_store_wrote() {
    // Compared against the files on disk rather than against the module's own suffix constants: the
    // constants are this module's private copy of another module's naming, so a test built on them
    // would agree with a mistake. The lengths come from `stat` of the files the real stores wrote.
    let root = scratch("footprint-split");
    let persistence = Persistence::new(&root);

    persistence
        .terminals()
        .save("term-1", "12345")
        .expect("saved");
    persistence
        .checkpoints()
        .save(checkpoint("sess-1", 1))
        .expect("saved");
    girder_core::save_search_index(persistence.search_index_path(), &[]).expect("wrote the index");

    let expected_terminals = std::fs::metadata(root.join("term-1.log"))
        .expect("the transcript")
        .len();
    let expected_checkpoints = std::fs::metadata(root.join("sess-1.checkpoints.json"))
        .expect("the checkpoint file")
        .len();
    let expected_search = std::fs::metadata(persistence.search_index_path())
        .expect("the index")
        .len();

    let footprint = persistence.footprint();
    assert_eq!(footprint.terminals_bytes, expected_terminals);
    assert_eq!(footprint.checkpoints_bytes, expected_checkpoints);
    assert_eq!(footprint.search_index_bytes, expected_search);
    assert_eq!(
        footprint.total(),
        expected_terminals + expected_checkpoints + expected_search,
        "the total must be the sum of the parts"
    );
}

#[test]
fn footprint_does_not_count_the_session_log() {
    // The logs are the session store's footprint and are measured by the quota arithmetic. Counting
    // them here as well would report a deployment as using twice the disk it uses.
    let root = scratch("footprint-logs");
    let persistence = Persistence::new(&root);
    std::fs::write(
        root.join("sess-1.jsonl.zst"),
        b"a session log of some length",
    )
    .expect("wrote");
    assert_eq!(
        persistence.footprint(),
        Footprint::default(),
        "the session log is not part of this layer's footprint"
    );
}

#[test]
fn footprint_does_not_count_staging_siblings() {
    // `write_atomic` stages `.<name>.tmp-<pid>-<n>` beside the target. A sibling of the index begins
    // with a dot, so it is neither a checkpoint nor a transcript, and it is not the index name either
    // — the property that keeps a half-written file from being reported as a real one.
    let root = scratch("footprint-staging");
    let persistence = Persistence::new(&root);
    std::fs::write(root.join(".term.log.tmp-1-0"), b"staging").expect("wrote");
    std::fs::write(root.join(".search-index.json.tmp-1-0"), b"staging").expect("wrote");
    assert_eq!(persistence.footprint(), Footprint::default());
}

// ---- the naming contract -----------------------------------------------------------------------

#[test]
fn the_three_names_are_distinct_from_a_session_log() {
    // The collision this rules out: if a checkpoint were `{id}.jsonl.zst`-shaped, or a transcript were,
    // writing one for a session would overwrite the conversation. The names are read back from the
    // directory after the real stores wrote them, rather than from this module's own constants, so a
    // wrong constant cannot make the test agree with itself.
    let root = scratch("names");
    let persistence = Persistence::new(&root);
    persistence
        .terminals()
        .save("sess-1", "terminal output")
        .expect("saved the transcript");
    persistence
        .checkpoints()
        .save(checkpoint("sess-1", 1))
        .expect("saved the checkpoint");
    girder_core::save_search_index(persistence.search_index_path(), &[]).expect("wrote the index");

    let written = names(&root);
    let log = "sess-1.jsonl.zst".to_owned();
    let mut ours: Vec<&String> = written.iter().filter(|name| **name != log).collect();
    ours.sort();
    assert_eq!(
        ours.len(),
        3,
        "three artefacts must be written, got {written:?}"
    );

    for name in &ours {
        // No suffix of a session log can be read as one of these, and none of them is the log's name.
        assert_ne!(**name, log);
        assert!(
            !name.ends_with(".jsonl.zst"),
            "`{name}` must not be log-shaped"
        );
    }
    assert!(
        ours.iter().any(|name| name.starts_with('.')),
        "the index is dot-prefixed so it can never be a session id, got {written:?}"
    );
    assert_eq!(
        ours.len(),
        3,
        "the three names must be distinct from each other, got {written:?}"
    );
}

#[test]
fn a_session_log_and_its_sidecars_coexist() {
    // The end-to-end form of the test above: all four artefacts for one session in one directory,
    // written by the real stores, each still readable afterwards.
    let root = scratch("coexist");
    let persistence = Persistence::new(&root);

    let log = root.join("sess-1.jsonl.zst");
    std::fs::write(&log, b"a compressed session log").expect("wrote the log");
    std::fs::write(root.join("sess-1.title"), b"a title").expect("wrote the title");

    persistence
        .terminals()
        .save("sess-1", "terminal output")
        .expect("saved the transcript");
    persistence
        .checkpoints()
        .save(checkpoint("sess-1", 2))
        .expect("saved the checkpoint");
    girder_core::save_search_index(persistence.search_index_path(), &[]).expect("wrote the index");

    // Every artefact is still itself.
    assert_eq!(
        std::fs::read(&log).expect("read the log"),
        b"a compressed session log",
        "the session log must not have been overwritten"
    );
    assert_eq!(
        std::fs::read(root.join("sess-1.title")).expect("read the title"),
        b"a title"
    );
    assert_eq!(
        persistence.terminals().load("sess-1").expect("loaded"),
        Some("terminal output".to_owned())
    );
    assert_eq!(
        persistence
            .checkpoints()
            .load("sess-1")
            .expect("loaded")
            .len(),
        1
    );
    assert!(persistence.search_index_path().is_file());

    // And the terminal store's listing does not mistake the log for a transcript: the log's name does
    // not end in `.log`, so the store never sees it as a transcript it owns.
    assert_eq!(
        persistence.terminals().list().expect("listed"),
        vec!["sess-1".to_owned()],
        "only the transcript must be listed"
    );
}

#[test]
fn prune_leaves_the_session_log_of_a_pruned_session() {
    // The sharpest form of the coexistence test: the session whose checkpoint and transcript are both
    // pruned must still have its log. A sweep that removed `<id>.*` would pass every count assertion
    // above and still destroy the conversation.
    let root = scratch("coexist-prune");
    let persistence = Persistence::new(&root);
    std::fs::write(root.join("sess-1.jsonl.zst"), b"the conversation").expect("wrote the log");
    persistence
        .terminals()
        .save("sess-1", "output")
        .expect("saved");
    persistence
        .checkpoints()
        .save(checkpoint("sess-1", 1))
        .expect("saved");

    let now = now_ms();
    let old = now - 10 * 60 * 1000;
    set_modified(&root.join("sess-1.log"), old);
    set_modified(&root.join("sess-1.checkpoints.json"), old);
    set_modified(&root.join("sess-1.jsonl.zst"), old);

    let pruned = persistence
        .prune(now, 5 * 60 * 1000)
        .expect("pruned without error");
    assert_eq!(
        pruned,
        Pruned {
            terminals: 1,
            checkpoints: 1
        }
    );
    assert_eq!(
        std::fs::read(root.join("sess-1.jsonl.zst")).expect("the log must survive"),
        b"the conversation",
        "pruning a session's sidecars must never touch its log"
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_persistence as persistence;
}
