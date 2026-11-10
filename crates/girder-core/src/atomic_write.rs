//! Atomic file replacement and cross-process writer coordination.
//!
//! Two guarantees, and they are separate on purpose:
//!
//! | mechanism | what it guarantees |
//! |---|---|
//! | [`write_atomic`] | a reader sees either the old content or the new, never a torn mix |
//! | [`with_lock`] | two writers do not interleave a read-modify-write cycle |
//!
//! # Why the rename is the atomic part
//!
//! Writing into the target file directly is not atomic: a reader that opens it mid-write sees a
//! prefix, and a crash leaves one permanently. Writing a sibling and renaming over the target is, on
//! every filesystem this runs on — the rename either happened or it did not.
//!
//! # Why a lock is still needed
//!
//! The rename protects a *reader*. It does nothing for a read-modify-write: two writers can each read
//! the old value, each compute a new one, and each rename over the other. The second rename wins and
//! the first writer's change vanishes with no error. [`with_lock`] serialises that cycle.
//!
//! # Why a lock holder is identified by a process id
//!
//! A lock is a file, and a process that dies leaves it behind. Without a liveness check every later
//! writer waits out its timeout forever. The lock records its holder, and a lock whose holder no longer
//! exists is taken over — which is safe because a dead process cannot still be writing.
//!
//! # Why the lock does not protect readers
//!
//! Readers stay lock-free because the rename commit is atomic. Making them take the lock would mean a
//! reader could block a writer, and the whole point of the rename is that it cannot.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The default time a writer waits for the lock.
pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(5);

/// The first retry delay while contending for a lock.
const RETRY_INITIAL: Duration = Duration::from_millis(5);

/// The ceiling for a retry delay.
const RETRY_MAX: Duration = Duration::from_millis(100);

/// Why an atomic write or a lock failed.
#[derive(Debug, thiserror::Error)]
pub enum AtomicError {
    /// The lock could not be taken before the deadline.
    #[error("timed out waiting for the writer lock at `{path}`")]
    LockTimeout {
        /// The lock file.
        path: String,
    },

    /// A sibling file could not be written or renamed.
    #[error("atomic write of `{path}` failed: {source}")]
    Write {
        /// The target.
        path: String,
        /// The cause.
        source: std::io::Error,
    },

    /// The operation ran, but the lock could not be released.
    #[error("could not release the lock at `{path}`: {source}")]
    Unlock {
        /// The lock file.
        path: String,
        /// The cause.
        source: std::io::Error,
    },
}

fn write_error(path: &Path, source: std::io::Error) -> AtomicError {
    AtomicError::Write {
        path: path.display().to_string(),
        source,
    }
}

/// The mode a replaced file ends up with.
///
/// Required rather than defaulted so the permission decision is visible at every call site: a file that
/// silently became `0600` because a helper defaulted would be a bug nobody notices until another user
/// cannot read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode(pub u32);

impl Mode {
    /// Owner read and write only.
    pub const PRIVATE: Self = Self(0o600);
    /// Owner read and write, everyone read.
    pub const PUBLIC: Self = Self(0o644);
}

/// Writes `contents` to `path`, replacing it atomically.
///
/// The sibling is created with the given mode and renamed over the target, so:
///
/// - a reader observes either the complete old content or the complete new one;
/// - the target ends up with exactly `mode`, because a rename does not preserve the sibling's mode
///   differently from any other file;
/// - a crash mid-write leaves the target untouched rather than truncated.
///
/// # Errors
///
/// Returns [`AtomicError::Write`] when the sibling cannot be written or renamed. The sibling is removed
/// on failure, so a failed write leaves no debris.
pub fn write_atomic(path: &Path, contents: &[u8], mode: Mode) -> Result<(), AtomicError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent).map_err(|error| write_error(parent, error))?;
    }

    // A random sibling name, so two concurrent writers do not collide on one staging file. The pid and a
    // counter rather than a cryptographic random: a collision needs two writers in one process within
    // one nanosecond, and this crate should not gain a randomness dependency for a name.
    let sibling = sibling_path(path);

    let result = (|| {
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&sibling)
                .map_err(|error| write_error(&sibling, error))?;
            file.write_all(contents)
                .map_err(|error| write_error(&sibling, error))?;
            // Fsync before the rename: without it a crash can leave the *new* name pointing at
            // content the filesystem had not yet written, which is worse than the old content.
            file.sync_all()
                .map_err(|error| write_error(&sibling, error))?;
        }
        set_mode(&sibling, mode).map_err(|error| write_error(&sibling, error))?;
        std::fs::rename(&sibling, path).map_err(|error| write_error(path, error))
    })();

    if result.is_err() {
        // A failed write must not leave debris: a staging file that outlives its attempt is one nobody
        // will ever clean up.
        let _ = std::fs::remove_file(&sibling);
    }
    result
}

/// Writes text to `path`, replacing it atomically.
///
/// # Errors
///
/// Same as [`write_atomic`].
pub fn write_text_atomic(path: &Path, contents: &str, mode: Mode) -> Result<(), AtomicError> {
    write_atomic(path, contents.as_bytes(), mode)
}

/// Sets a file's permission bits.
#[cfg(unix)]
fn set_mode(path: &Path, mode: Mode) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode.0))
}

/// A no-op off Unix, where the permission bits do not exist in this form.
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Mode) -> std::io::Result<()> {
    Ok(())
}

/// A staging sibling beside `path`.
fn sibling_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = path.file_name().map_or_else(
        || "write".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(".{name}.tmp-{pid}-{n}"))
}

/// The lock file beside `path`.
fn lock_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "lock".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(".{name}.lock"))
}

/// Whether a process id is still alive.
///
/// Signal 0 asks the kernel whether the process exists without sending anything. A process we have no
/// permission to signal answers `EPERM`, which means it exists — treating that as dead would let two
/// writers hold one lock.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // `EPERM` means the process exists but belongs to someone else; treating that as dead would
        // let two writers hold one lock.
        matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
    }
    #[cfg(not(unix))]
    {
        // Without a portable liveness probe, a lock is assumed live: taking over a lock whose holder is
        // alive is worse than waiting.
        let _ = pid;
        true
    }
}

/// Runs `operation` while holding an exclusive lock for `path`.
///
/// The lock is a sibling file created with `create_new`, which is atomic: exactly one contender wins.
/// A lock whose recorded holder is gone is taken over, because a dead process cannot still be writing.
///
/// # Errors
///
/// Returns [`AtomicError::LockTimeout`] when the deadline passes, and whatever `operation` returns
/// otherwise. The lock is released on both paths.
pub async fn with_lock<F, T, E>(path: &Path, wait: Duration, operation: F) -> Result<T, E>
where
    F: FnOnce() -> crate::scope::BoxFut<Result<T, E>>,
    E: From<AtomicError>,
{
    let lock = lock_path(path);
    let deadline = Instant::now() + wait;
    let mut delay = RETRY_INITIAL;

    loop {
        match acquire(&lock) {
            Ok(()) => break,
            Err(error) => {
                // A holder that is gone leaves the lock behind; taking it over is safe because a dead
                // process cannot be mid-write.
                if is_stale(&lock) && std::fs::remove_file(&lock).is_ok() {
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(E::from(AtomicError::LockTimeout {
                        path: lock.display().to_string(),
                    }));
                }
                let _ = error;
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_MAX);
            }
        }
    }

    let result = operation().await;
    // Released on every path: a lock that survives a failed operation blocks every later writer.
    if let Err(source) = std::fs::remove_file(&lock) {
        // The operation's own result is what the caller asked for; a failed release is reported only
        // when the operation succeeded, so a real failure is not masked by a cleanup problem.
        if result.is_ok() {
            return Err(E::from(AtomicError::Unlock {
                path: lock.display().to_string(),
                source,
            }));
        }
    }
    result
}

/// Creates the lock file exclusively, recording this process as its holder.
fn acquire(lock: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock)?;
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()
}

/// Whether the lock's recorded holder no longer exists.
fn is_stale(lock: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(lock) else {
        // An unreadable lock is most likely one being written right now; treating it as stale would
        // take over a live lock.
        return false;
    };
    match text.trim().parse::<u32>() {
        Ok(pid) => !process_alive(pid),
        // A file with no readable holder cannot be attributed to anyone, so it is stale by definition.
        Err(_) => true,
    }
}
