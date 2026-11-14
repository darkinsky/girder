//! Durable terminal output, addressed by terminal id.
//!
//! A terminal lives only as long as the process that started it. `terminal.retain` answers with the
//! whole transcript a session has produced, and a client that reconnects after a restart has nothing
//! to read: the registry is empty and the output is gone. This module keeps one transcript per
//! terminal on disk, so the text outlives the process that printed it.
//!
//! # Why one file per terminal rather than one shared log
//!
//! A shared log would need framing to say where one terminal's output ends and the next begins, and
//! reading one transcript would mean scanning every other one. A file per id makes each operation a
//! single filesystem call: [`Store::load`] is one read, [`Store::remove`] is one unlink, and
//! [`Store::prune`] is one `stat` per stored terminal. It also means a terminal can be pruned while
//! another is written without either operation touching the other's file.
//!
//! # Why the session log's format is not reused here
//!
//! `session.rs` appends zstd frames to a per-session JSONL file, and it deliberately trades
//! crash-atomicity for append speed: a torn tail loses the last event, which its reader can drop
//! because a truncated frame fails to decode and a truncated line fails to parse. Two of those
//! properties do not survive translation to terminal output:
//!
//! - There is no framing. Output is one text blob, so a torn write appends half of a new transcript
//!   to an old one and the result is *valid text* that says something the terminal never printed.
//!   The session format can detect and discard a partial tail; text cannot.
//! - There is nothing to append. `retain` returns the transcript as a whole, not an increment, so
//!   every save replaces the file rather than extending it — which is exactly the case
//!   [`write_atomic`] exists for.
//!
//! So the storage *helper* is reused and the storage *format* is not. The transcript is stored as
//! plain text: it is bounded by the caller's own transcript bound (see `terminal_buffer`), the file
//! is written whole and read whole, and a format a person can `tail` is worth more here than the
//! bytes zstd would save.
//!
//! # Why an id is validated before it becomes a path
//!
//! The id arrives from a client, and `root.join(id)` with an id of `../../etc/passwd` writes outside
//! the store — an arbitrary-file-write primed by a request field. Every entry point therefore
//! validates first: a single path component, no separator, no `..`, no NUL, not empty. Ids that
//! begin with `.` are refused too, because that namespace belongs to the staging siblings
//! [`write_atomic`] creates beside a target; keeping it free means a transcript can never be
//! mistaken for an in-flight write.
//!
//! # Why writes are atomic
//!
//! [`Store::save`] rewrites the whole transcript, so a direct write is a window in which the file is
//! a prefix of the new text over the tail of the old, and a crash in that window leaves that mix on
//! disk permanently. [`write_atomic`] stages a sibling and renames it over the target: a concurrent
//! reader sees the old transcript or the new one, never a blend, and a crash leaves the old file
//! intact. A transcript is also the one artefact a user is most likely to read after something went
//! wrong, so a half-written one is worse than an absent one.
//!
//! # Why pruning measures age by modification time
//!
//! A deployment cannot let one file per terminal accumulate forever, and "old" has to mean something
//! that survives a restart: a counter kept in memory would reset with the process, and an id does
//! not encode a time. The modification time is set by every [`Store::save`], so age is "time since
//! this terminal last produced output", which is the only question a retention policy can actually
//! answer. Files with a timestamp in the future — a clock adjustment, a restored backup — are not
//! older than any cutoff and are kept, because deleting them on a threshold that cannot be
//! interpreted would be the destructive reading of an ambiguous fact.
//!
//! # What [`Store::list`] guarantees
//!
//! Every id it returns is one [`Store::load`] will accept: the scan skips entries that are not
//! regular files, are not named `<id>.<ext>`, or whose name is not a valid id. It also skips the
//! staging siblings of writes in flight, which is why deleting whatever the directory holds is not
//! an option — a prune that removed a staging file would abort a save that was in progress.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use girder_core::{Mode, write_atomic};

/// The extension of a stored transcript.
///
/// Carried on every file so a directory holding other things — a README, an editor's swap file,
/// another store's data — yields only the transcripts, and so a staging sibling can be told apart
/// from a finished file by its full name rather than by luck.
const EXTENSION: &str = "log";

/// The longest id that can become a file name.
///
/// A name longer than the filesystem's limit fails with `ENAMETOOLONG`, which names neither the id
/// nor the reason. Refusing it here turns that into a [`PersistError::InvalidId`] that says what is
/// wrong with the request.
const MAX_ID_BYTES: usize = 128;

/// Why a transcript could not be stored or read.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// The id could not name a file inside the store.
    ///
    /// This is a rejection, not a condition: nothing was written or read, so a caller may correct
    /// the id and retry without wondering what a partial attempt left behind.
    #[error(
        "`{id}` is not a usable terminal id: it must be one path component, must not begin with \
         `.`, and must not contain `..`, a separator, or NUL"
    )]
    InvalidId {
        /// The id as the caller supplied it.
        id: String,
    },

    /// The file could not be read, written, listed, or removed.
    ///
    /// The cause is flattened to a string because the caller's only response is to log it: there is
    /// no branch to take on an errno here, and keeping a `std::io::Error` would stop this type from
    /// being comparable in tests.
    #[error("terminal output could not be persisted: {detail}")]
    Io {
        /// What failed, including the path.
        detail: String,
    },
}

/// A terminal transcript found by scanning the store directory.
struct Entry {
    /// The id the file name decodes to. Already validated, so it is safe to hand to a caller.
    id: String,
    /// The file it was found in.
    path: PathBuf,
    /// The modification time in milliseconds since the epoch, or `None` when it could not be read.
    modified_ms: Option<u64>,
}

/// A persistent store of terminal output.
///
/// The store is cheap to construct and holds no state of its own: two instances over the same
/// directory see the same transcripts, and nothing is cached that would go stale when another
/// process writes.
#[derive(Debug, Clone)]
pub struct Store {
    /// The directory holding one file per terminal.
    root: PathBuf,
}

impl Store {
    /// Opens a store writing under `root`.
    ///
    /// The directory is created by the first [`Store::save`] rather than here, so a store built for
    /// a directory that never receives output does not leave an empty one behind. Reading from a
    /// directory that does not exist is the same as reading from an empty one: no transcripts.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Saves the whole output of `terminal_id`, replacing any transcript already stored.
    ///
    /// Returns the number of bytes written.
    ///
    /// Replaces rather than appends: the caller holds the transcript as a whole, and an append would
    /// duplicate everything already stored every time it saved.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::InvalidId`] when the id cannot name a file inside the store, before
    /// anything is written, and [`PersistError::Io`] when the file cannot be replaced.
    pub fn save(&self, terminal_id: &str, text: &str) -> Result<u64, PersistError> {
        let path = self.path_for(terminal_id)?;
        // Private: a transcript is what a shell printed, and that routinely includes tokens,
        // hostnames, and paths that are meant to be readable only by the account that ran it.
        write_atomic(&path, text.as_bytes(), Mode::PRIVATE)
            .map_err(|error| io_error("write", &path, &error))?;
        Ok(bytes_of(text))
    }

    /// Reads back the transcript stored for `terminal_id`.
    ///
    /// Returns `Ok(None)` for a terminal that was never saved or was already removed: "no transcript"
    /// is an ordinary answer, not a failure, because a client asking about an id it just learned
    /// from [`Store::list`] may have raced a prune.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::InvalidId`] when the id could not name a stored file, and
    /// [`PersistError::Io`] when the file exists but cannot be read. A file whose bytes are not
    /// UTF-8 is reported as the latter: this store only ever writes text, so undecodable content is
    /// damage to report rather than text to guess at.
    pub fn load(&self, terminal_id: &str) -> Result<Option<String>, PersistError> {
        let path = self.path_for(terminal_id)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("read", &path, &error)),
        }
    }

    /// Removes the transcript stored for `terminal_id`.
    ///
    /// Returns whether a file was actually removed, so a caller can tell "closed a terminal that had
    /// output" from "closed one that never printed".
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::InvalidId`] when the id could not name a stored file, and
    /// [`PersistError::Io`] when the file cannot be removed.
    pub fn remove(&self, terminal_id: &str) -> Result<bool, PersistError> {
        let path = self.path_for(terminal_id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("remove", &path, &error)),
        }
    }

    /// Lists the ids of every stored transcript, sorted.
    ///
    /// Sorted rather than in directory order because directory order is a filesystem detail, and a
    /// caller comparing two listings — before and after a prune, say — should not see them differ
    /// because the files were rewritten.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::Io`] when the directory cannot be read. A directory that does not
    /// exist is not an error: it lists nothing.
    pub fn list(&self) -> Result<Vec<String>, PersistError> {
        Ok(self.entries()?.into_iter().map(|entry| entry.id).collect())
    }

    /// Removes every transcript whose age exceeds `older_than_ms`, measured from `now_ms`.
    ///
    /// Returns how many files this call removed. A threshold larger than `now_ms` yields a cutoff at
    /// the epoch, so nothing is removed rather than everything — an uninterpretable policy must not
    /// delete the store.
    ///
    /// The comparison is against each file's modification time, which [`Store::save`] updates, so
    /// the age is "time since this terminal last produced output".
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::Io`] when the directory cannot be read or a file that is old enough
    /// cannot be removed. Removal stops at that error rather than continuing, so the caller's count
    /// is exact and the failure is not silently partial.
    pub fn prune(&self, now_ms: u64, older_than_ms: u64) -> Result<usize, PersistError> {
        // Saturating: a policy wider than the current clock is "keep everything", which the epoch
        // cutoff expresses without a special case.
        let cutoff = now_ms.saturating_sub(older_than_ms);
        let mut removed = 0;
        for entry in self.entries()? {
            let Some(modified) = entry.modified_ms else {
                // A file whose timestamp cannot be read is not provably old. Keeping it is the
                // recoverable mistake; deleting it is not.
                continue;
            };
            if modified >= cutoff {
                continue;
            }
            match std::fs::remove_file(&entry.path) {
                Ok(()) => removed += 1,
                // Another process pruned it first. Its goal is met either way, and counting it
                // would overstate what this call did.
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("remove", &entry.path, &error)),
            }
        }
        Ok(removed)
    }

    /// The file a transcript with this id lives in, after validating the id.
    ///
    /// The one place an id becomes a path, so every entry point inherits the same rejection.
    fn path_for(&self, terminal_id: &str) -> Result<PathBuf, PersistError> {
        validate(terminal_id)?;
        Ok(self.root.join(format!("{terminal_id}.{EXTENSION}")))
    }

    /// Scans the store directory, returning every regular file that decodes to a valid id.
    ///
    /// Sorted by id, and never including a staging sibling: those begin with `.`, which no valid id
    /// does, so a write in flight cannot be listed, loaded, or pruned.
    fn entries(&self) -> Result<Vec<Entry>, PersistError> {
        let directory = match std::fs::read_dir(&self.root) {
            Ok(directory) => directory,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error("read the directory", &self.root, &error)),
        };

        let mut entries = Vec::new();
        for entry in directory {
            let entry = entry.map_err(|error| io_error("read an entry in", &self.root, &error))?;
            let path = entry.path();
            let Some(id) = id_of(&path) else {
                continue;
            };
            // `DirEntry::metadata` does not follow a symlink, so a link named `<id>.log` is not a
            // regular file here and cannot be used to redirect a read outside the store.
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                // Removed between the scan and the stat: another process pruned or closed it, so it
                // is not part of the listing rather than an error.
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error("inspect", &path, &error)),
            };
            if !metadata.is_file() {
                continue;
            }
            entries.push(Entry {
                id,
                path,
                modified_ms: modified_ms(&metadata),
            });
        }
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(entries)
    }
}

/// Checks that an id can name exactly one file directly inside the store directory.
///
/// Kept as a free function taking the id alone so the rule reads the same in [`Store::path_for`] and
/// in [`id_of`], which is what keeps [`Store::list`] from returning an id that [`Store::load`] would
/// reject.
fn validate(id: &str) -> Result<(), PersistError> {
    let invalid = id.is_empty()
        || id.len() > MAX_ID_BYTES
        // Reserved for the staging siblings the atomic write creates. A dot-prefixed transcript
        // would also collide with them in a directory listing a person reads.
        || id.starts_with('.')
        // Both separators, on every platform: rejecting only the host's separator would let an id
        // through that becomes a traversal after the directory is copied to another filesystem.
        || id.contains(['/', '\\'])
        // `..` as a component is the traversal itself; refusing the substring also refuses `a..b`,
        // which is harmless but not worth a second rule to distinguish.
        || id.contains("..")
        // Would truncate the name at the syscall boundary.
        || id.contains('\0');
    if invalid {
        return Err(PersistError::InvalidId { id: id.to_owned() });
    }
    Ok(())
}

/// The id a stored transcript file name encodes, or `None` when the file is not one.
///
/// Applies [`validate`] rather than a bare extension test, so a file that happens to end in the
/// extension but whose stem is not a legal id is left out of listings instead of producing an id the
/// rest of this API would refuse.
fn id_of(path: &Path) -> Option<String> {
    if path.extension()? != EXTENSION {
        return None;
    }
    let id = path.file_stem()?.to_str()?;
    validate(id).ok()?;
    Some(id.to_owned())
}

/// A file's modification time in milliseconds since the epoch, when it can be read.
///
/// Milliseconds because that is the unit [`Store::prune`] takes, and a `SystemTime` cannot be
/// compared to a `u64` without choosing one.
fn modified_ms(metadata: &std::fs::Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since_epoch.as_millis()).ok()
}

/// The byte length of a transcript, as the count [`Store::save`] returns.
fn bytes_of(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

/// Wraps a filesystem failure with the operation and the path that produced it.
///
/// Takes any `Display` rather than a `std::io::Error`: the write path fails through
/// [`AtomicError`](girder_core::AtomicError), which already names its own cause, and flattening it
/// into a fresh `io::Error` just to fit one signature would lose that.
fn io_error(action: &str, path: &Path, error: &dyn std::fmt::Display) -> PersistError {
    PersistError::Io {
        detail: format!("could not {action} `{}`: {error}", path.display()),
    }
}
