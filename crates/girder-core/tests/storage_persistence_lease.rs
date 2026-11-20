//! Tests for the per-session write lease and the in-process write claim.
//!
//! Every property here is one where the code can look correct and still hand two writers the same
//! session:
//!
//! - **the second lease on one directory is refused.** That is the entire point; a lock that only
//!   excludes nothing reads as a lock.
//! - **a released lease can be re-taken, and a dropped one is released too.** A lock that survives its
//!   handle turns one failed close into a permanently unwritable session.
//! - **the lock file survives release.** Removing it would let a successor lock an orphan while a third
//!   writer locked the fresh replacement — the double-hold the inode check exists to prevent.
//! - **an unlinked-and-recreated lock file is not mistaken for the held one.** This is the inode half of
//!   the contract, and it is driven here through the pure classifier because a real filesystem will not
//!   hand out a chosen inode on demand.
//! - **a second claim in one process is refused, and a stale release does not free a live one.** The
//!   claim cannot see other processes, so getting it wrong is invisible until two handles interleave.
//! - **a failed lease rolls the claim back.** Otherwise the session stays unwritable in this process
//!   for the rest of its life with no kernel lock to justify it.
//!
//! The module is included by path rather than through the crate's re-exports, which is why it names
//! nothing from `girder_core` at all.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic
)]

#[path = "../src/storage/persistence_lease.rs"]
pub mod persistence_lease_gap;

use std::path::PathBuf;

use persistence_lease_gap::{
    ACQUIRE_ATTEMPTS, ClaimError, Claims, Decision, LEASE_FILENAME, LeaseError, LockIdentity,
    LockPathState, OpenWriteError, Pending, SESSION_DIR_MODE, Writer, claim_and_lease, classify,
    lease_path, verify_locked,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh, empty directory per test, under the temp directory.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-persistence-lease-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

// ---- where the lock lives ----------------------------------------------------------------------

#[test]
fn the_lock_file_sits_beside_the_log() {
    let dir = scratch("path");
    assert_eq!(lease_path(&dir), dir.join(LEASE_FILENAME));
    assert_eq!(LEASE_FILENAME, "session.lock");
}

#[test]
fn the_session_directory_is_owner_only() {
    // The mode is a constant rather than a literal at the call site so the two creators — this module
    // and the publication path — cannot disagree.
    assert_eq!(SESSION_DIR_MODE, 0o700);
}

#[test]
fn acquisition_is_bounded() {
    // The retry exists for an inode that moved, not for a busy lock; an unbounded loop over a path
    // being rewritten in a cycle would spin forever instead of reporting contention.
    assert_eq!(ACQUIRE_ATTEMPTS, 3);
}

// ---- the identity classifier -------------------------------------------------------------------

#[test]
fn the_same_inode_on_the_same_device_is_the_same_lock() {
    let held = LockIdentity {
        device: 5,
        inode: 42,
    };
    assert_eq!(classify(held, 5, 42), LockPathState::Same);
}

#[test]
fn a_matching_inode_on_another_device_is_not_the_same_file() {
    // A copy of the lock file on a different filesystem is a different inode in the same way two
    // files with equal names are different files. Comparing device and inode together — never the
    // inode alone — is what makes this true.
    let held = LockIdentity {
        device: 5,
        inode: 42,
    };
    assert_eq!(classify(held, 6, 42), LockPathState::Different);
}

#[test]
fn a_matching_device_with_another_inode_is_not_the_same_file() {
    // This is the unlinked-and-recreated case: a successor creates a new `session.lock` and it gets a
    // fresh inode on the same device. The lock we hold is on the orphan.
    let held = LockIdentity {
        device: 5,
        inode: 42,
    };
    assert_eq!(classify(held, 5, 43), LockPathState::Different);
}

#[test]
fn only_a_verified_path_is_accepted() {
    assert_eq!(verify_locked(LockPathState::Same), Decision::Accept);
}

#[test]
fn a_replaced_lock_path_is_retried() {
    assert_eq!(verify_locked(LockPathState::Different), Decision::Retry);
}

#[test]
fn a_vanished_lock_path_is_retried_rather_than_accepted() {
    // Accepting here would mean holding a lock on an inode nobody can reach by name, which excludes
    // exactly nobody. Retrying recreates the file and proves the new one.
    assert_eq!(verify_locked(LockPathState::Gone), Decision::Retry);
}

// ---- the kernel lease --------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_lease_is_exclusive_within_one_process() {
    let dir = scratch("exclusive");
    let first =
        persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("a free lease");
    assert_eq!(first.id(), "s1");
    assert!(!first.is_released());

    // `flock` locks are per open file description, so a second descriptor on the same inode in this
    // very process contends. That makes this a real exclusion test, not a simulation.
    match persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1") {
        Err(LeaseError::AlreadyOwned { id }) => assert_eq!(id, "s1"),
        other => panic!("expected already-owned, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn a_released_lease_can_be_taken_again() {
    let dir = scratch("released");
    let mut lease = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("lease");
    lease.release().expect("release");
    assert!(lease.is_released());

    let again = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("re-acquired");
    assert_eq!(again.id(), "s1");
}

#[cfg(unix)]
#[test]
fn releasing_twice_is_harmless() {
    let dir = scratch("idempotent");
    let mut lease = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("lease");
    lease.release().expect("first release");
    // The second call must not report a failure for an unlock that is already done, and must not
    // unlock a descriptor something else may now own.
    lease.release().expect("second release");
}

#[cfg(unix)]
#[test]
fn dropping_a_lease_releases_it() {
    let dir = scratch("dropped");
    {
        let lease = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("lease");
        assert!(!lease.is_released());
        // Dropped without `release`, which closing the descriptor still unlocks.
    }
    let again = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1")
        .expect("a dropped lease releases its lock");
    assert_eq!(again.id(), "s1");
}

#[cfg(unix)]
#[test]
fn release_keeps_the_lock_file() {
    let dir = scratch("keeps-file");
    let path = lease_path(&dir);
    let mut lease = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("lease");
    lease.release().expect("release");
    // Every acquired lease belongs to a materialized session, and the surviving file keeps the stable
    // inode later lockers verify against.
    assert!(path.exists(), "the lock file must survive release");

    // And the surviving file is reusable: the next holder locks that same inode.
    let again = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("re-acquired");
    assert_eq!(again.path(), path.as_path());
}

#[cfg(unix)]
#[test]
fn acquisition_creates_a_missing_session_directory() {
    let root = scratch("creates-dir");
    let dir = root.join("project").join("s1");
    assert!(!dir.exists());
    let lease = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("lease");
    assert!(
        dir.is_dir(),
        "acquisition must create the session directory"
    );
    assert!(lease_path(&dir).is_file());
    // The lease must be held against the file it just created, not against a path it never locked.
    assert_eq!(lease.path(), lease_path(&dir).as_path());
}

#[cfg(unix)]
#[test]
fn the_session_directory_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = scratch("dir-mode");
    let dir = root.join("s1");
    let lease = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("lease");
    assert_eq!(lease.id(), "s1");
    let mode = std::fs::metadata(&dir)
        .expect("the directory")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, SESSION_DIR_MODE);
}

#[cfg(unix)]
#[test]
fn a_session_in_a_read_only_parent_reports_io_not_contention() {
    // The two failure kinds have to stay distinct: a permission problem reported as "already owned"
    // sends the caller looking for another writer who is not there.
    use std::os::unix::fs::PermissionsExt as _;

    let root = scratch("readonly");
    let blocked = root.join("blocked");
    std::fs::create_dir(&blocked).expect("the parent directory");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500))
        .expect("drop write permission");

    let result = persistence_lease_gap::SessionWriteLease::acquire(&blocked.join("s1"), "s1");

    // Restore before asserting so a failed assertion does not leave an unremovable directory behind.
    let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700));

    match result {
        Err(LeaseError::Io { .. }) => {}
        // Running as root ignores the permission bits entirely; the test cannot distinguish the
        // failure there, so it asserts only that permission denial never becomes contention.
        Ok(lease) => {
            drop(lease);
        }
        Err(other) => panic!("expected an I/O failure, got {other:?}"),
    }
}

// ---- in-process claims -------------------------------------------------------------------------

#[test]
fn a_second_claim_on_one_id_is_refused() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("the first claim");
    assert_eq!(
        claims.claim_write("s1"),
        Err(ClaimError::AlreadyOwned {
            id: "s1".to_owned()
        })
    );
}

#[test]
fn competing_ids_do_not_contend() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("s1");
    claims.claim_write("s2").expect("s2");
    assert_eq!(claims.writer_count(), 2);
}

#[test]
fn a_claim_is_visible_before_any_handle_exists() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("claim");
    // Note the absent handle: a second open must lose the race even though the first has not finished
    // constructing anything. `Reserved` is exactly that window.
    assert_eq!(claims.writer("s1"), Some(Writer::Reserved));
    assert!(claims.is_claimed("s1"));
}

#[test]
fn adopting_a_claim_binds_it_to_a_handle() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("claim");
    let token = claims.adopt_write("s1");
    assert_eq!(claims.writer("s1"), Some(Writer::Active(token)));
    assert!(claims.is_claimed("s1"));
}

#[test]
fn releasing_an_adopted_claim_frees_the_id() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("claim");
    let token = claims.adopt_write("s1");
    assert!(claims.release("s1", token, true));
    assert!(!claims.is_claimed("s1"));
    claims
        .claim_write("s1")
        .expect("re-claimable after release");
}

#[test]
fn a_stale_release_does_not_free_a_live_claim() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("claim");
    let first = claims.adopt_write("s1");
    assert!(claims.release("s1", first, true));

    // A later handle takes the id.
    claims.claim_write("s1").expect("re-claim");
    let second = claims.adopt_write("s1");
    assert_ne!(first, second);

    // The old handle closing now must not free the new one's claim: that would hand a live writer's
    // id to a third caller while it is still appending.
    assert!(!claims.release("s1", first, true));
    assert_eq!(claims.writer("s1"), Some(Writer::Active(second)));
}

#[test]
fn releasing_an_unknown_id_changes_nothing() {
    let mut claims = Claims::new();
    assert!(!claims.release("absent", 1, true));
    assert_eq!(claims.writer_count(), 0);
}

#[test]
fn a_reserved_claim_is_not_released_by_a_handle_token() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("claim");
    // No handle was adopted, so any token presented here belongs to nothing.
    assert!(!claims.release("s1", 1, true));
    assert!(claims.is_claimed("s1"));
    // The open's own rollback path is the one that frees a reservation.
    assert!(claims.release_claim("s1"));
    assert!(!claims.is_claimed("s1"));
}

#[test]
fn releasing_a_claim_twice_is_refused_the_second_time() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("claim");
    let token = claims.adopt_write("s1");
    assert!(claims.release("s1", token, true));
    assert!(!claims.release("s1", token, true));
}

// ---- pending created sessions ------------------------------------------------------------------

#[test]
fn a_created_session_is_observable_before_it_is_materialized() {
    let mut claims = Claims::new();
    let pending = claims.register_created("s1", 7).expect("a fresh id");
    assert_eq!(pending.inherited_event_count, 7);
    assert_eq!(claims.pending("s1"), Some(&pending));
    assert!(claims.has_pending("s1"));
    assert!(claims.is_claimed("s1"));
}

#[test]
fn creating_an_id_that_is_already_claimed_is_refused() {
    let mut claims = Claims::new();
    claims.claim_write("s1").expect("claim");
    assert_eq!(
        claims.register_created("s1", 0),
        Err(ClaimError::AlreadyExists {
            id: "s1".to_owned()
        })
    );
}

#[test]
fn creating_an_id_twice_is_refused() {
    let mut claims = Claims::new();
    claims.register_created("s1", 0).expect("first create");
    assert_eq!(
        claims.register_created("s1", 0),
        Err(ClaimError::AlreadyExists {
            id: "s1".to_owned()
        })
    );
}

#[test]
fn each_pending_entry_gets_its_own_revision() {
    let mut claims = Claims::new();
    let first = claims.register_created("s1", 0).expect("s1");
    let second = claims.register_created("s2", 0).expect("s2");
    // Two in-memory sessions must not share a revision token, or a reader cannot tell them apart.
    assert_ne!(first.revision, second.revision);
}

#[test]
fn materializing_drops_the_pending_entry_but_keeps_the_claim() {
    let mut claims = Claims::new();
    claims.register_created("s1", 0).expect("create");
    assert!(claims.materialized("s1"));
    assert!(!claims.has_pending("s1"));
    // The writer still owns the session: it is on disk now, which is not the same as closed.
    assert!(claims.is_claimed("s1"));
    assert!(!claims.materialized("s1"));
}

#[test]
fn closing_an_unmaterialized_session_leaves_nothing_behind() {
    let mut claims = Claims::new();
    claims.register_created("s1", 0).expect("create");
    let token = claims.adopt_write("s1");
    // `materialized = false`: the session never reached disk, so the pending entry must go too.
    assert!(claims.release("s1", token, false));
    assert!(!claims.has_pending("s1"));
    assert!(!claims.is_claimed("s1"));
}

#[test]
fn closing_a_materialized_session_keeps_no_pending_entry_either() {
    let mut claims = Claims::new();
    claims.register_created("s1", 0).expect("create");
    let token = claims.adopt_write("s1");
    claims.materialized("s1");
    assert!(claims.release("s1", token, true));
    assert!(!claims.is_claimed("s1"));
    assert!(!claims.has_pending("s1"));
}

#[test]
fn pending_ids_are_listed_in_order() {
    let mut claims = Claims::new();
    claims.register_created("s2", 0).expect("s2");
    claims.register_created("s1", 0).expect("s1");
    let ids: Vec<&str> = claims.pending_ids().collect();
    assert_eq!(ids, ["s1", "s2"]);
}

#[test]
fn a_pending_entry_survives_a_failed_open_rollback() {
    // `release_claim` is the open path's rollback; it must not delete a pending entry, because that
    // entry is the only thing making a created-but-unmaterialized session visible.
    let mut claims = Claims::new();
    let pending = claims.register_created("s1", 3).expect("create");
    assert!(claims.release_claim("s1"));
    assert!(!claims.is_claimed("s1"));
    assert_eq!(claims.pending("s1"), Some(&pending));
}

#[test]
fn a_pending_entry_is_a_value_a_reader_can_be_handed() {
    let mut claims = Claims::new();
    let pending: Pending = claims.register_created("s1", 0).expect("create");
    assert!(pending.revision.starts_with("memory:"));
    assert_eq!(pending.inherited_event_count, 0);
}

// ---- the open-for-write transaction ------------------------------------------------------------

#[cfg(unix)]
#[test]
fn claiming_and_leasing_takes_both() {
    let dir = scratch("both");
    let mut claims = Claims::new();
    let lease = claim_and_lease(&mut claims, &dir, "s1").expect("claim and lease");
    assert_eq!(lease.id(), "s1");
    // The claim is taken before the lease, so it is `Reserved` until a handle adopts it.
    assert_eq!(claims.writer("s1"), Some(Writer::Reserved));
    assert!(claims.is_claimed("s1"));
}

#[cfg(unix)]
#[test]
fn a_second_open_of_a_claimed_session_is_refused_before_touching_the_filesystem() {
    let dir = scratch("claimed");
    let mut claims = Claims::new();
    let first = claim_and_lease(&mut claims, &dir, "s1").expect("first open");
    assert!(claims.is_claimed("s1"));

    match claim_and_lease(&mut claims, &dir, "s1") {
        Err(OpenWriteError::AlreadyOwned { id }) => assert_eq!(id, "s1"),
        other => panic!("expected already-owned, got {other:?}"),
    }
    drop(first);
}

#[cfg(unix)]
#[test]
fn a_lease_refusal_rolls_the_claim_back() {
    let dir = scratch("rollback");
    let mut claims = Claims::new();

    // Hold the kernel lease directly, so the claim below succeeds and the lease fails — the exact
    // ordering a real second `open` cannot produce, because the claim refuses first.
    let held = persistence_lease_gap::SessionWriteLease::acquire(&dir, "s1").expect("held lease");
    // The refusal collapses to one shape on purpose, which is upstream's choice: `open` reports
    // `SessionAlreadyOwnedError` whether this process or another one holds the session, because the
    // caller's response is the same either way — it cannot write this session now.
    match claim_and_lease(&mut claims, &dir, "s1") {
        Err(OpenWriteError::AlreadyOwned { id }) => assert_eq!(id, "s1"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The claim must not survive the failed open: leaving it would make the session unwritable for
    // the rest of this process's life while no kernel lock justifies it.
    assert!(!claims.is_claimed("s1"));

    drop(held);
    let lease = claim_and_lease(&mut claims, &dir, "s1").expect("a later open succeeds");
    assert_eq!(lease.id(), "s1");
}

#[test]
fn the_refusal_message_names_the_session() {
    // The refusal a user sees is about a session; the path is where its artifacts happen to live.
    let error = LeaseError::AlreadyOwned {
        id: "s1".to_owned(),
    };
    assert_eq!(
        error.to_string(),
        "session `s1` is already owned by an active write handle"
    );

    let error = OpenWriteError::AlreadyOwned {
        id: "s2".to_owned(),
    };
    assert!(error.to_string().contains("s2"));

    let error = ClaimError::AlreadyExists {
        id: "s3".to_owned(),
    };
    assert_eq!(error.to_string(), "session `s3` already exists");
}

#[test]
fn an_unsupported_platform_is_reported_rather_than_approximated() {
    // Off Unix the lease would need a named kernel semaphore reached through FFI; this crate forbids
    // that. The variant exists so the refusal cannot silently become "lock acquired".
    let error = LeaseError::Unsupported;
    assert!(error.to_string().contains("only implemented on Unix"));
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn an_io_failure_keeps_its_cause() {
    use std::error::Error as _;

    let error = LeaseError::Io {
        path: PathBuf::from("/tmp/session.lock"),
        source: std::io::Error::from_raw_os_error(13),
    };
    assert!(error.to_string().contains("session.lock"));
    assert!(error.source().is_some());
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod storage {
    pub use super::persistence_lease_gap as persistence_lease;
}
