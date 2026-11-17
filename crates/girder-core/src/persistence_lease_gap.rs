//! The per-session write lease: who may write one session's artifacts, and the in-process claim
//! that answers the same question one level down.
//!
//! # What upstream has, and what this file is
//!
//! `packages/session/session-persistence-jsonl/src/lease.ts` is 135 lines and answers exactly one
//! question: *may this process publish bytes into this session's directory right now?* Its answer is
//! not an expiry and not a heartbeat — it is a kernel lock that the kernel releases when the holder's
//! descriptor closes, **including on any process death**. That is the whole design, and three of its
//! consequences are easy to get wrong:
//!
//! - **There is no timeout, deliberately.** A live but wedged holder keeps the lease until its
//!   process exits. An expiry would let a successor expropriate a stalled writer whose resumed
//!   appends would then tear the log — corrupting the artifact to avoid a wait.
//! - **The lock is the `flock`, not the file.** A leftover `session.lock` therefore blocks nobody;
//!   what excludes a second writer is the descriptor held against that inode.
//! - **A POSIX lock names an inode, not a path.** If the lock file is unlinked and recreated, a lock
//!   on the orphaned inode proves nothing about the path any more, so the holder re-stats the path and
//!   starts over rather than trusting the descriptor it already holds.
//!
//! `storage.ts` answers a *different* question one level down: which session ids this **process**
//! already has a writer for. [`Claims`] is that table (`JsonlBackendTracker`'s writer half). The two
//! are complements, not alternatives: the claim stops two handles in one process, the lease stops two
//! processes. Upstream's `lease.ts` says so itself — the browser worker stubs the native `flock` entry
//! because "it is single-process, so the in-process write claim already excludes every writer".
//!
//! # What this file covers, and what it deliberately does not
//!
//! Covered, and tested end to end against a real kernel lock:
//!
//! | upstream | here |
//! |---|---|
//! | `LEASE_FILENAME` / `join(dir, …)` | [`LEASE_FILENAME`], [`lease_path`] |
//! | `SessionWriteLease.acquire` | [`SessionWriteLease::acquire`] |
//! | `SessionWriteLease.release` | [`SessionWriteLease::release`] |
//! | the inode-verification retry, bounded at 3 | [`verify_locked`], [`ACQUIRE_ATTEMPTS`] |
//! | `SessionAlreadyOwnedError` | [`LeaseError::AlreadyOwned`] |
//! | `claimWrite` / `releaseClaim` / `adopt` / `release` | [`Claims`] |
//! | `open()`'s claim-then-lease rollback | [`claim_and_lease`] |
//!
//! Not covered, and the reason is a fact about this build rather than a preference:
//!
//! - **The Win32 named-semaphore lease** (`win32.ts`, the `acquireLockHandleWin32` /
//!   `releaseLockHandleWin32` half). Upstream reaches `CreateSemaphoreW`/`WaitForSingleObject` through
//!   `koffi` FFI. This crate has no `windows-sys` dependency, and its one sanctioned `unsafe` exemption
//!   (`flock`, below) is POSIX-only, so there would be no way to call those entry points *and* no way
//!   to test them on this crate's CI — an untested lock is a worse claim than a documented gap.
//!   [`SessionWriteLease::acquire`] therefore returns [`LeaseError::Unsupported`] off Unix instead of
//!   pretending. Note that this is *only* the lease: the Win32 **publication** half of `win32.ts`
//!   (`MoveFileExW(..., MOVEFILE_WRITE_THROUGH)`) is a separate gap, recorded against
//!   `publish_exclusive`.
//! - **File permissions of the lock file.** Upstream opens it with Node's `'w'` (mode `0666` masked by
//!   the umask). This build uses `0600`, matching the crate's `Mode::PRIVATE`. No reader ever opens the
//!   lock file — upstream's contract says readers never touch the lock — so the only thing the mode
//!   can affect is who may *hard-link or read* a file whose contents are always empty. Stricter is
//!   the safe direction, and the divergence is recorded here rather than left to be discovered.
//!
//! # The lifecycle this models
//!
//! ```text
//! create()  → registerCreated        no filesystem footprint, no lease
//!    ↓ first materializing write
//!            claimAndLease           in-process claim, then the kernel lease
//!    ↓
//!            SessionWriteLease held  across every append until close
//!    ↓ close()
//!            release + releaseClaim  lease first, claim always, even if the lease fails
//! ```
//!
//! That ordering is upstream's, and it matters in one direction: a failed lease release must still
//! free the in-process claim, or the session stays unwritable for the rest of this process's life
//! even though no kernel lock is held.
//!
//! # Testing note
//!
//! This file is compiled by `tests/persistence_lease_gap.rs` through `#[path]`, so it must not name
//! any other part of the crate — every item below is `std` plus `cfg`. It has been kept to that rule
//! deliberately, because a `crate::` path would fail to resolve in that build.

// The crate root has not been wired to this module yet, so in a library build nothing here has a
// caller and `dead_code` would report the whole surface as waste.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::collections::btree_map::Entry as MapEntry;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::fs::File;

/// Base name of the kernel lock file inside a session's directory.
pub const LEASE_FILENAME: &str = "session.lock";

/// How many times acquisition re-opens the lock path before giving up.
///
/// Upstream's bound is 3 and the reasoning is the same: locking an inode a releasing creator just
/// unlinked — or a path somebody recreated — re-opens the fresh file, and steady state needs exactly
/// one pass. A bound rather than a loop because a path being rewritten in a tight cycle must fail as
/// contention, not spin.
pub const ACQUIRE_ATTEMPTS: u32 = 3;

/// The mode a session directory is created with: owner only.
///
/// Upstream creates it `0o700` here *and* in `materializePosix`, and both creators must agree: a
/// directory created by one with the other's mode is a permission mystery nobody looks at until a
/// second user cannot read their own session.
pub const SESSION_DIR_MODE: u32 = 0o700;

/// The lock file's mode; see the module docs for why this is stricter than upstream.
pub const LEASE_FILE_MODE: u32 = 0o600;

// ---------------------------------------------------------------------------------------------
// Where the lease lives
// ---------------------------------------------------------------------------------------------

/// The lease file for the session whose artifact directory is `dir`.
///
/// A function rather than a `.join()` at each call site so acquiring and documenting the lock cannot
/// disagree about the name.
pub fn lease_path(dir: &Path) -> PathBuf {
    dir.join(LEASE_FILENAME)
}

// ---------------------------------------------------------------------------------------------
// Identity of the locked inode
// ---------------------------------------------------------------------------------------------

/// The stat identity a POSIX lock is actually held against.
///
/// A `flock` is attached to an open file description, and an open file description names an inode.
/// Two filesystem paths can therefore be "the same lock" only if they resolve to the same inode on the
/// same device — which is why this is the pair compared after locking, and not the path string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockIdentity {
    /// Device the inode lives on.
    pub device: u64,
    /// Inode number.
    pub inode: u64,
}

/// What re-statting the lock path found after the lock was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockPathState {
    /// The path still names the inode this descriptor locked.
    Same,
    /// The path names a different inode — it was unlinked and recreated, or replaced.
    Different,
    /// The path no longer exists.
    Gone,
}

/// What to do with a lock that has been taken but not yet proven to cover the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The descriptor locks the file the path names: hold it.
    Accept,
    /// The descriptor locks an orphan: close it and start over against whatever stands there now.
    Retry,
}

/// Whether a held descriptor still covers the lock path.
///
/// Only [`LockPathState::Same`] accepts. `Gone` retries rather than accepting, which is upstream's
/// choice and the conservative one: if nothing is at the path, the next attempt recreates it and
/// proves the new file — whereas accepting an orphaned inode would mean holding a lock no successor
/// can ever contend on, silently excluding nobody.
pub const fn verify_locked(found: LockPathState) -> Decision {
    match found {
        LockPathState::Same => Decision::Accept,
        LockPathState::Different | LockPathState::Gone => Decision::Retry,
    }
}

// ---------------------------------------------------------------------------------------------
// The lease
// ---------------------------------------------------------------------------------------------

/// Why a lease could not be acquired or released.
#[derive(Debug)]
pub enum LeaseError {
    /// Another live holder keeps the lease on this session.
    ///
    /// Carries the session id, not just the path, because that is what a caller can act on: the
    /// refusal a user sees is about a session, and the path is an implementation detail of where its
    /// artifacts happen to live.
    AlreadyOwned {
        /// The session whose write ownership is taken.
        id: String,
    },
    /// The lock file or its directory could not be created, locked, or stat'ed.
    Io {
        /// The path the failure concerns.
        path: PathBuf,
        /// The underlying filesystem failure.
        source: io::Error,
    },
    /// This platform has no implemented lease.
    ///
    /// Off Unix the lease would need a named kernel semaphore reached through FFI, which this crate's
    /// lint configuration forbids. Reported rather than silently degrading to "always succeeds",
    /// which would tell a caller it holds exclusion it does not have.
    Unsupported,
}

impl fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyOwned { id } => {
                write!(
                    formatter,
                    "session `{id}` is already owned by an active write handle"
                )
            }
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "session lease at `{}` failed: {source}",
                    path.display()
                )
            }
            Self::Unsupported => formatter
                .write_str("session write leases are only implemented on Unix in this build"),
        }
    }
}

impl std::error::Error for LeaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyOwned { .. } | Self::Unsupported => None,
        }
    }
}

/// One held write lease on a session's artifact directory.
///
/// Constructed only by [`SessionWriteLease::acquire`]. Dropping it without calling
/// [`SessionWriteLease::release`] releases the kernel lock too — a descriptor closing is what releases
/// a `flock` — but a caller that wants to *report* a release failure has to call `release`.
#[derive(Debug)]
pub struct SessionWriteLease {
    id: String,
    path: PathBuf,
    released: bool,
    /// The locked descriptor. `None` after a successful release, so nothing can unlock twice.
    #[cfg(unix)]
    lock: Option<File>,
}

impl SessionWriteLease {
    /// Acquire the session directory's kernel write lock, creating the directory if it is absent.
    ///
    /// # Errors
    ///
    /// - [`LeaseError::AlreadyOwned`] while another holder keeps the lock, and after
    ///   [`ACQUIRE_ATTEMPTS`] attempts that each locked an inode the path no longer names.
    /// - [`LeaseError::Io`] for a filesystem failure.
    /// - [`LeaseError::Unsupported`] off Unix.
    #[cfg(unix)]
    pub fn acquire(dir: &Path, id: impl Into<String>) -> Result<Self, LeaseError> {
        let id = id.into();
        let path = lease_path(dir);
        create_session_dir(dir).map_err(|source| LeaseError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        for _ in 0..ACQUIRE_ATTEMPTS {
            let file = open_lock_file(&path)?;
            // Non-blocking is the whole contract: contention must become an error the caller reports,
            // never a wait, because a blocking acquire on a wedged holder is an unbounded hang with no
            // diagnostic.
            match try_lock_exclusive(&file) {
                Ok(()) => {}
                Err(source) if is_contention(&source) => {
                    return Err(LeaseError::AlreadyOwned { id });
                }
                Err(source) => return Err(LeaseError::Io { path, source }),
            }
            let held = lock_identity(&file).map_err(|source| LeaseError::Io {
                path: path.clone(),
                source,
            })?;
            let found = lock_path_state(&path, held).map_err(|source| LeaseError::Io {
                path: path.clone(),
                source,
            })?;
            if let Decision::Accept = verify_locked(found) {
                return Ok(Self {
                    id,
                    path,
                    released: false,
                    lock: Some(file),
                });
            }
            // The locked inode is no longer the file at the lock path: start over against whatever
            // now stands there. Dropping `file` releases the orphaned lock before the next attempt.
        }
        Err(LeaseError::AlreadyOwned { id })
    }

    /// Acquire the session directory's kernel write lock.
    ///
    /// # Errors
    ///
    /// Always [`LeaseError::Unsupported`]: see the module docs for why the Win32 named-semaphore lease
    /// is not implemented rather than approximated.
    #[cfg(not(unix))]
    pub fn acquire(dir: &Path, id: impl Into<String>) -> Result<Self, LeaseError> {
        let _ = (dir, id.into());
        Err(LeaseError::Unsupported)
    }

    /// The session this lease guards.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The lock file this lease guards.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether [`Self::release`] has already run.
    pub fn is_released(&self) -> bool {
        self.released
    }

    /// Release the kernel lock by closing its descriptor. Idempotent.
    ///
    /// The lock file is **not** removed. Every acquired lease belongs to a materialized or
    /// materializing session, and the surviving file keeps the stable inode later lockers verify
    /// against — removing it would let a successor lock an orphan while a third writer locks the
    /// fresh replacement, and both would believe they hold exclusion.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::Io`] when the kernel refuses the unlock. The lease is marked released
    /// either way: retrying an unlock that already failed cannot succeed, and leaving the flag clear
    /// would let a caller believe it still holds a lock it is about to lose with the descriptor.
    pub fn release(&mut self) -> Result<(), LeaseError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        #[cfg(unix)]
        {
            if let Some(file) = self.lock.take() {
                // The explicit unlock is what can *fail* and be reported; closing the descriptor would
                // release the lock silently on drop. Both happen, so a refused unlock still drops the
                // descriptor and the lock with it — the error is the caller's diagnostic, not a state
                // it can recover by retrying.
                if let Err(source) = unlock(&file) {
                    return Err(LeaseError::Io {
                        path: self.path.clone(),
                        source,
                    });
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Filesystem calls
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
fn create_session_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(SESSION_DIR_MODE);
    builder.create(dir)
}

#[cfg(not(unix))]
fn create_session_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Opens (creating if needed) the lock file, owner-only.
#[cfg(unix)]
fn open_lock_file(path: &Path) -> Result<File, LeaseError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(LEASE_FILE_MODE)
        .open(path)
        .map_err(|source| LeaseError::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// Takes a non-blocking exclusive `flock` on `file`.
///
/// The crate has no way to reach this through a safe wrapper: `nix`'s `Flock` guard is behind its
/// `fs` feature, which this crate does not enable, and `std` exposes no file locking. The raw call is
/// therefore taken here with an explicit reason — the same exemption, for the same reason, as the
/// Landlock probe in `exec.rs`.
#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> Result<(), io::Error> {
    use std::os::fd::AsRawFd as _;

    #[allow(
        unsafe_code,
        reason = "`flock(2)` on an open descriptor is the only reachable non-blocking exclusive file lock here; no memory is involved"
    )]
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Drops the lock `file` holds, without closing it.
///
/// Split from [`try_lock_exclusive`] because this one is called on an explicit release path where the
/// kernel's answer is reportable, while the descriptor is closed by ordinary drop in every other case.
#[cfg(unix)]
fn unlock(file: &File) -> Result<(), io::Error> {
    use std::os::fd::AsRawFd as _;

    #[allow(
        unsafe_code,
        reason = "`flock(2)`'s unlock is the counterpart of the acquire above; no memory is involved"
    )]
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether a `flock` failure means another descriptor holds the lock.
///
/// `flock(2)` reports `EAGAIN`; some libcs spell it `EWOULDBLOCK`. On every platform this build
/// targets they are the same value, and naming both here would be a second arm that can never be
/// taken. A failure that is *not* contention is reported as I/O rather than as "already owned",
/// because a caller told it lost a race would stop looking for the real problem.
#[cfg(unix)]
fn is_contention(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EAGAIN
    )
}

/// The identity of the inode `file` was opened on.
#[cfg(unix)]
fn lock_identity(file: &File) -> io::Result<LockIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    Ok(LockIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// What the lock path names right now, compared to the inode that was locked.
///
/// The comparison is done here rather than by the caller so that "the path is still the file I locked"
/// is answered by one function reading one `stat`, not by two callers each assembling it.
#[cfg(unix)]
fn lock_path_state(path: &Path, held: LockIdentity) -> io::Result<LockPathState> {
    use std::os::unix::fs::MetadataExt as _;

    match std::fs::metadata(path) {
        Ok(metadata) => Ok(classify(held, metadata.dev(), metadata.ino())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(LockPathState::Gone),
        Err(error) => Err(error),
    }
}

/// Classifies a freshly stat'ed inode against the one that was locked.
///
/// Separated from [`lock_path_state`] so the comparison is a pure function the tests can drive with
/// values a real filesystem will not produce on demand — two devices that differ while the inode
/// matches, and the reverse.
pub const fn classify(held: LockIdentity, device: u64, inode: u64) -> LockPathState {
    if held.device == device && held.inode == inode {
        LockPathState::Same
    } else {
        LockPathState::Different
    }
}

// ---------------------------------------------------------------------------------------------
// In-process write claims
// ---------------------------------------------------------------------------------------------

/// Why an in-process write claim was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimError {
    /// A write handle already holds this id, so a second one would interleave appends.
    AlreadyOwned {
        /// The contested session.
        id: String,
    },
    /// A `create` targeted an id this process already has a writer or a pending entry for.
    AlreadyExists {
        /// The occupied session.
        id: String,
    },
}

impl fmt::Display for ClaimError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyOwned { id } => write!(
                formatter,
                "session `{id}` is already owned by an active write handle"
            ),
            Self::AlreadyExists { id } => write!(formatter, "session `{id}` already exists"),
        }
    }
}

impl std::error::Error for ClaimError {}

/// Who holds a session id inside this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writer {
    /// Claimed by an open that has not finished constructing its handle.
    ///
    /// Upstream models this as a `null` handle for the same reason: the claim must be visible the
    /// instant `open` decides to take it, before the handle exists, or two concurrent opens both pass
    /// the check.
    Reserved,
    /// Held by a constructed write handle.
    Active(u64),
}

/// A created-but-unmaterialized session: visible in this process, absent from disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The in-memory revision a reader sees before the artifact exists.
    pub revision: String,
    /// The exact fork-inherited prefix length supplied at create.
    pub inherited_event_count: u64,
}

/// The sessions this process already has a writer for, and the ones it created but never wrote.
///
/// This is bookkeeping, not exclusion: it cannot stop another process, and it is not meant to —
/// [`SessionWriteLease`] is. It stops the case the kernel lock cannot see, which is two handles in
/// *this* process racing each other before either has a descriptor to lock with.
#[derive(Debug, Default)]
pub struct Claims {
    writers: BTreeMap<String, Writer>,
    pending: BTreeMap<String, Pending>,
    next_handle: u64,
    next_revision: u64,
}

impl Claims {
    /// An empty table.
    pub fn new() -> Self {
        Self {
            writers: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_handle: 1,
            next_revision: 1,
        }
    }

    /// Claim write ownership of an existing session.
    ///
    /// # Errors
    ///
    /// [`ClaimError::AlreadyOwned`] when any writer — reserved or active — holds the id.
    pub fn claim_write(&mut self, id: &str) -> Result<(), ClaimError> {
        match self.writers.entry(id.to_owned()) {
            MapEntry::Occupied(_) => Err(ClaimError::AlreadyOwned { id: id.to_owned() }),
            MapEntry::Vacant(slot) => {
                slot.insert(Writer::Reserved);
                Ok(())
            }
        }
    }

    /// Register a created-but-unmaterialized session and claim it in the same step.
    ///
    /// # Errors
    ///
    /// [`ClaimError::AlreadyExists`] when the id is already claimed *or* already pending. Upstream
    /// refuses on either because for `create` a duplicate is not a race to arbitrate — it is the fact
    /// that the caller asked for an id that is taken.
    pub fn register_created(
        &mut self,
        id: &str,
        inherited_event_count: u64,
    ) -> Result<Pending, ClaimError> {
        if self.writers.contains_key(id) || self.pending.contains_key(id) {
            return Err(ClaimError::AlreadyExists { id: id.to_owned() });
        }
        let pending = Pending {
            revision: format!("memory:{}:{}", self.next_revision, id),
            inherited_event_count,
        };
        self.next_revision += 1;
        self.writers.insert(id.to_owned(), Writer::Reserved);
        self.pending.insert(id.to_owned(), pending.clone());
        Ok(pending)
    }

    /// Roll a failed write open back: drop the claim, keep any pending entry.
    ///
    /// Used on the error path of an open, where nothing was constructed and so nothing owns the claim.
    pub fn release_claim(&mut self, id: &str) -> bool {
        self.writers.remove(id).is_some()
    }

    /// Bind a constructed handle to the claim it reserved, returning its token.
    ///
    /// A claim that is absent is inserted as `Active` rather than refused: upstream's `adopt` does the
    /// same, because a `create` path adopts a handle it just registered and refusing would make two
    /// call sites disagree about who claims.
    pub fn adopt_write(&mut self, id: &str) -> u64 {
        let token = self.next_handle;
        self.next_handle += 1;
        self.writers.insert(id.to_owned(), Writer::Active(token));
        token
    }

    /// Release one handle's bookkeeping on close.
    ///
    /// A token that no longer matches means the claim was re-taken by a later handle, and this stale
    /// release must not free it — that would hand a live writer's id to a third caller.
    ///
    /// A creator that never materialized leaves nothing behind: the session never existed, so its
    /// pending entry goes too.
    pub fn release(&mut self, id: &str, token: u64, materialized: bool) -> bool {
        match self.writers.get(id) {
            Some(Writer::Active(held)) if *held == token => {}
            Some(Writer::Reserved) => {
                // A reserved claim has no handle, so any caller presenting a token is stale; but a
                // create path releases through `release_claim`, not here.
                return false;
            }
            Some(Writer::Active(_)) | None => return false,
        }
        self.writers.remove(id);
        if !materialized {
            self.pending.remove(id);
        }
        true
    }

    /// Drop a pending entry once the session reached durable storage.
    pub fn materialized(&mut self, id: &str) -> bool {
        self.pending.remove(id).is_some()
    }

    /// Whether any writer holds this id.
    pub fn is_claimed(&self, id: &str) -> bool {
        self.writers.contains_key(id)
    }

    /// Who holds this id, if anyone.
    pub fn writer(&self, id: &str) -> Option<Writer> {
        self.writers.get(id).copied()
    }

    /// Whether this process still tracks a created-but-unmaterialized session.
    pub fn has_pending(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    /// The pending entry for a created-but-unmaterialized session.
    pub fn pending(&self, id: &str) -> Option<&Pending> {
        self.pending.get(id)
    }

    /// Every pending session id, in order.
    pub fn pending_ids(&self) -> impl Iterator<Item = &str> {
        self.pending.keys().map(String::as_str)
    }

    /// How many sessions this process has a writer for.
    pub fn writer_count(&self) -> usize {
        self.writers.len()
    }
}

// ---------------------------------------------------------------------------------------------
// The open-for-write transaction
// ---------------------------------------------------------------------------------------------

/// Why a write open failed.
#[derive(Debug)]
pub enum OpenWriteError {
    /// This process already has a writer for the session.
    AlreadyOwned {
        /// The contested session.
        id: String,
    },
    /// The kernel lease could not be taken.
    Lease(LeaseError),
}

impl fmt::Display for OpenWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyOwned { id } => write!(
                formatter,
                "session `{id}` is already owned by an active write handle"
            ),
            Self::Lease(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OpenWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lease(error) => Some(error),
            Self::AlreadyOwned { .. } => None,
        }
    }
}

impl From<LeaseError> for OpenWriteError {
    fn from(error: LeaseError) -> Self {
        match error {
            LeaseError::AlreadyOwned { id } => Self::AlreadyOwned { id },
            other => Self::Lease(other),
        }
    }
}

/// Claim the session in this process and take its cross-process lease, in that order.
///
/// The order is upstream's and it is not arbitrary: the claim is the cheap refusal, and taking it
/// first means a second open in this process is refused without touching the filesystem. The claim is
/// rolled back on every lease failure — otherwise a session whose lease could not be taken would
/// remain unwritable here forever, while no kernel lock exists to justify it.
///
/// The returned lease is *not* yet bound to a handle; the caller adopts it with
/// [`Claims::adopt_write`] once its handle exists, which is what keeps the claim visible during
/// construction.
///
/// # Errors
///
/// [`OpenWriteError::AlreadyOwned`] for the in-process claim, [`OpenWriteError::Lease`] for the kernel
/// lease.
pub fn claim_and_lease(
    claims: &mut Claims,
    dir: &Path,
    id: &str,
) -> Result<SessionWriteLease, OpenWriteError> {
    claims.claim_write(id).map_err(|error| match error {
        // Both claim refusals become the same report: from the caller's side a second writer exists,
        // and whether it is a live handle or a pending create is a detail its response does not differ
        // on. Upstream throws `SessionAlreadyOwnedError` for both at this point too.
        ClaimError::AlreadyOwned { id } | ClaimError::AlreadyExists { id } => {
            OpenWriteError::AlreadyOwned { id }
        }
    })?;
    match SessionWriteLease::acquire(dir, id) {
        Ok(lease) => Ok(lease),
        Err(error) => {
            claims.release_claim(id);
            Err(OpenWriteError::from(error))
        }
    }
}
