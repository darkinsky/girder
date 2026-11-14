//! Tests for in-process file locks.
//!
//! The properties pinned here are the ones a lock table gets wrong, and the first two are the
//! reason the module exists:
//!
//! - **A second owner is refused, and told who holds it.** A lock that admits a contender is not a
//!   lock; a refusal that does not name the holder is a "busy" a model cannot act on.
//! - **A duplicate acquisition by the holder is an error.** There is no depth counter, so a second
//!   `Ok` would mean the first `release` frees a lock the outer frame still believes it owns.
//! - **A dead holder does not wedge the path forever.** `reap` is how a crash, a panic, or an
//!   aborted task is cleaned up; without it the path stays busy for the life of the process.
//! - **Only the holder releases.** Otherwise one session can free a lock it never took and hand the
//!   file to a third writer while the real holder still thinks it is alone.
//! - **One file, one lock, regardless of spelling.** `/a/b/`, `/a/b`, and `/a//b` are the same file,
//!   and a table that keyed on the raw string would let two callers both "hold" it.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/file_lock.rs"]
mod file_lock;

use file_lock::{Contention, Lock, Locks};

// ---- acquiring ---------------------------------------------------------------------------------

#[test]
fn an_unheld_path_can_be_acquired() {
    let mut locks = Locks::new();

    assert_eq!(
        locks.acquire("/workspace/src/lib.rs", "session-a", 1_000),
        Ok(())
    );

    let held = locks
        .holder("/workspace/src/lib.rs")
        .expect("the lock is held");
    assert_eq!(held.path, "/workspace/src/lib.rs");
    assert_eq!(held.owner, "session-a");
    assert_eq!(held.at_ms, 1_000);
    assert_eq!(locks.len(), 1);
}

#[test]
fn different_paths_do_not_contend() {
    let mut locks = Locks::new();

    assert_eq!(locks.acquire("/a", "session-a", 1), Ok(()));
    assert_eq!(locks.acquire("/b", "session-b", 2), Ok(()));
    assert_eq!(locks.len(), 2);
    assert_eq!(
        locks.holder("/b").map(|lock| lock.owner.as_str()),
        Some("session-b")
    );
}

#[test]
fn different_owners_share_the_table_but_not_the_path() {
    // The realistic shape: two subagents of one process writing different files run in parallel,
    // and neither should be refused by the other.
    let mut locks = Locks::new();

    assert_eq!(locks.acquire("/a", "agent-1", 10), Ok(()));
    assert_eq!(locks.acquire("/b", "agent-2", 10), Ok(()));
    assert_eq!(locks.len(), 2);
}

// ---- contention: the core property --------------------------------------------------------------

#[test]
fn a_second_owner_is_refused_and_told_who_holds_it() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));

    let refusal = locks.acquire("/a", "agent-2", 2_000);

    // The holder and its timestamp, not a bare "busy": a refusal a model cannot attribute is one it
    // will retry unchanged.
    assert_eq!(
        refusal,
        Err(Contention::HeldBy {
            owner: "agent-1".to_owned(),
            since_ms: 1_000,
        })
    );
}

#[test]
fn a_refused_acquisition_changes_nothing() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));

    assert!(locks.acquire("/a", "agent-2", 9_000).is_err());

    // Neither the owner nor the timestamp moves: letting the refusal refresh `at_ms` would let a
    // contender keep a live lock alive past its reap timeout.
    let held = locks.holder("/a").expect("still held");
    assert_eq!(held.owner, "agent-1");
    assert_eq!(held.at_ms, 1_000);
    assert_eq!(locks.len(), 1);
}

#[test]
fn a_live_holder_is_not_reaped_and_still_contends() {
    // The contender's protocol — reap, then retry — has to leave a live lock alone, or reaping would
    // be indistinguishable from releasing.
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));

    assert!(locks.reap(1_500, 1_000).is_empty());
    assert_eq!(
        locks.acquire("/a", "agent-2", 1_500),
        Err(Contention::HeldBy {
            owner: "agent-1".to_owned(),
            since_ms: 1_000,
        })
    );
}

// ---- duplicate acquisition: the other core property ---------------------------------------------

#[test]
fn the_holder_cannot_acquire_its_own_lock_twice() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));

    assert_eq!(
        locks.acquire("/a", "agent-1", 2_000),
        Err(Contention::AlreadyYours)
    );

    // The original acquisition still stands: the duplicate was refused, not silently merged.
    let held = locks.holder("/a").expect("still held");
    assert_eq!(held.at_ms, 1_000);
}

#[test]
fn a_duplicate_after_a_release_is_a_fresh_acquisition() {
    // `AlreadyYours` must not leave any state behind that would make the next acquisition a
    // duplicate too.
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));
    assert!(locks.release("/a", "agent-1"));

    assert_eq!(locks.acquire("/a", "agent-1", 5_000), Ok(()));
    assert_eq!(locks.holder("/a").map(|lock| lock.at_ms), Some(5_000));
}

// ---- release -----------------------------------------------------------------------------------

#[test]
fn a_released_path_can_be_acquired_again() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));

    assert!(locks.release("/a", "agent-1"));

    assert!(locks.holder("/a").is_none());
    assert!(locks.is_empty());
    assert_eq!(locks.acquire("/a", "agent-2", 2_000), Ok(()));
    assert_eq!(
        locks.holder("/a").map(|lock| lock.owner.as_str()),
        Some("agent-2")
    );
}

#[test]
fn a_non_holder_cannot_release() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));

    // Reporting `true` here would be worse than the refusal itself: the caller would believe the
    // path is free when the real holder still holds it.
    assert!(!locks.release("/a", "agent-2"));

    let held = locks.holder("/a").expect("still held");
    assert_eq!(held.owner, "agent-1");
}

#[test]
fn releasing_an_unheld_path_is_not_an_error() {
    // Release runs on error branches where the acquire may never have happened; requiring every
    // caller to remember whether it did would produce leaks rather than correctness.
    let mut locks = Locks::new();

    assert!(!locks.release("/never-locked", "agent-1"));
    assert!(locks.is_empty());
}

#[test]
fn releasing_a_path_twice_reports_the_second_as_a_no_op() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));

    assert!(locks.release("/a", "agent-1"));
    assert!(!locks.release("/a", "agent-1"));
}

// ---- reaping: the crashed-holder property -------------------------------------------------------

#[test]
fn reap_removes_a_lock_whose_holder_timed_out() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "dead-agent", 1_000), Ok(()));

    let reaped = locks.reap(2_000, 500);

    assert_eq!(reaped.len(), 1);
    let lock = reaped.first().expect("one lock was reaped");
    assert_eq!(
        lock,
        &Lock {
            path: "/a".to_owned(),
            owner: "dead-agent".to_owned(),
            at_ms: 1_000,
        }
    );
    assert!(locks.is_empty());
}

#[test]
fn a_reaped_path_can_be_acquired_by_the_next_writer() {
    // The point of reaping: a process that died holding a lock must not wedge the path forever.
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "dead-agent", 1_000), Ok(()));
    assert!(locks.acquire("/a", "agent-2", 1_100).is_err());

    assert_eq!(locks.reap(10_000, 5_000).len(), 1);
    assert_eq!(locks.acquire("/a", "agent-2", 10_001), Ok(()));
}

#[test]
fn a_lock_exactly_at_the_timeout_is_reaped() {
    // The boundary is inclusive: a holder at exactly `timeout_ms` has held it for the whole timeout.
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));

    assert_eq!(locks.reap(1_500, 500).len(), 1);
    assert!(locks.is_empty());
}

#[test]
fn reap_only_removes_the_stale_locks() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/old", "agent-1", 1_000), Ok(()));
    assert_eq!(locks.acquire("/new", "agent-2", 9_000), Ok(()));

    let reaped = locks.reap(9_500, 1_000);

    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped.first().map(|lock| lock.path.as_str()), Some("/old"));
    assert_eq!(
        locks.holder("/new").map(|lock| lock.owner.as_str()),
        Some("agent-2")
    );
}

#[test]
fn reap_on_an_empty_table_returns_nothing() {
    let mut locks = Locks::new();

    assert!(locks.reap(u64::MAX, 0).is_empty());
    assert!(locks.is_empty());
}

#[test]
fn a_clock_that_went_backwards_does_not_reap_a_fresh_lock() {
    // `now_ms` before `at_ms` must saturate to zero age, not underflow into "infinitely old".
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 5_000), Ok(()));

    assert!(locks.reap(100, 1_000).is_empty());
    assert_eq!(locks.holder("/a").map(|lock| lock.at_ms), Some(5_000));
}

#[test]
fn a_zero_timeout_reaps_everything() {
    // The caller saying "assume every holder is gone" — a shutdown path, not an ordinary reap.
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a", "agent-1", 1_000), Ok(()));
    assert_eq!(locks.acquire("/b", "agent-2", 1_000), Ok(()));

    let reaped = locks.reap(1_000, 0);

    assert_eq!(reaped.len(), 2);
    assert!(locks.is_empty());
}

// ---- path normalization --------------------------------------------------------------------------

#[test]
fn a_trailing_separator_names_the_same_lock() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/workspace/src/", "agent-1", 1_000), Ok(()));

    assert_eq!(
        locks.acquire("/workspace/src", "agent-2", 2_000),
        Err(Contention::HeldBy {
            owner: "agent-1".to_owned(),
            since_ms: 1_000,
        })
    );
    assert_eq!(locks.len(), 1);
}

#[test]
fn redundant_separators_and_dot_segments_name_the_same_lock() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a/b/c", "agent-1", 1_000), Ok(()));

    for spelling in ["/a/b/c/", "/a//b/c", "/a/./b/c", "/a/b/../b/c"] {
        assert_eq!(
            locks.acquire(spelling, "agent-2", 2_000),
            Err(Contention::HeldBy {
                owner: "agent-1".to_owned(),
                since_ms: 1_000,
            }),
            "`{spelling}` should name the same file as `/a/b/c`"
        );
    }
}

#[test]
fn the_holder_is_found_through_any_spelling_of_its_path() {
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a/b/c", "agent-1", 1_000), Ok(()));

    // What is reported is the normalized path: two callers that spelled it differently must read
    // the same string back, or the refusal text would not line up with the lock.
    let held = locks
        .holder("/a//b/./c/")
        .expect("held under a normalized key");
    assert_eq!(held.path, "/a/b/c");
}

#[test]
fn releasing_through_a_different_spelling_works() {
    // The mirror of the acquisition case: if the key were the raw string, this release would find
    // nothing and leave a lock that can never be freed.
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("/a/b/", "agent-1", 1_000), Ok(()));

    assert!(locks.release("/a/b", "agent-1"));
    assert!(locks.is_empty());
}

#[test]
fn relative_paths_are_distinct_from_absolute_ones() {
    // Lexical normalization cannot resolve a relative path against a working directory, and
    // pretending it did would silently merge two different files.
    let mut locks = Locks::new();
    assert_eq!(locks.acquire("src/lib.rs", "agent-1", 1_000), Ok(()));

    assert_eq!(locks.acquire("/src/lib.rs", "agent-2", 2_000), Ok(()));
    assert_eq!(locks.len(), 2);
}

// ---- the empty table -----------------------------------------------------------------------------

#[test]
fn the_empty_table_is_empty() {
    let locks = Locks::new();

    assert!(locks.is_empty());
    assert_eq!(locks.len(), 0);
    assert!(locks.holder("/anything").is_none());
}

#[test]
fn the_default_table_is_empty() {
    // `Default` is derived and must agree with `new`; a mismatch would make one construction path
    // behave differently from the other.
    let locks = Locks::default();

    assert!(locks.is_empty());
    assert!(locks.holder("/anything").is_none());
}
