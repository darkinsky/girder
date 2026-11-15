//! One persistence layer per deployment, so the three durable stores are constructed, pruned, and
//! measured together instead of being rediscovered by each caller.
//!
//! The crate already has three durable stores — [`crate::CheckpointStore`],
//! [`crate::TerminalPersistStore`], and the search cache in [`crate::search_persist`] — and all three
//! are individually tested. What is missing is a single object that *holds* them. Each one takes a
//! directory of its own, so a caller that wires them up by hand has to decide the layout itself, and
//! two callers that decide differently put one deployment's checkpoints somewhere its terminals are
//! not. This module fixes the layout in one place: everything lives directly under one root.
//!
//! # Why one root rather than three directories
//!
//! A session's log, its checkpoints, and its terminal output all describe the same session, and the
//! questions that matter span them: how much disk is this deployment using, what can be deleted, is
//! this session taking space. Split across three trees those become three scans with three roots to
//! configure, three chances to misconfigure one, and no way to answer a question about a deployment
//! with a single number. One root makes the answers additive.
//!
//! The stores already coexist in one directory without colliding, which is what makes this possible
//! rather than merely tidy — see [`Persistence::new`] for the exact name each one claims.
//!
//! # Why pruning is one call
//!
//! The alternative is a retention policy that has to remember three function names, and forgetting
//! one is not a visible failure: the forgotten store simply grows until the disk fills, months later,
//! in a deployment where every individual store is working correctly. The fan-out lives here so a
//! caller cannot forget a store that was added after it wrote its policy. The search index is pruned
//! for the same reason, by dropping the whole file — it is a cache, and a cache that outlives its
//! usefulness is rebuilt cheaply, which is the argument [`crate::search_persist`] itself makes for a
//! version mismatch.
//!
//! # Why pruning stops at the first failure
//!
//! The three prunes are not independent enough to continue past an error: the usual cause is a
//! filesystem problem — a full disk, a permissions change, an I/O error on the volume — and every
//! later unlink will hit the same one. Continuing would produce a count that mixes "this was old and
//! is now gone" with "this could not be removed", which is exactly the number a retention report must
//! not be wrong about. So a returned count describes removals that all succeeded.
//!
//! # Why the footprint is measured rather than maintained
//!
//! A running total would have to be adjusted by every write in every process, including writes this
//! object never sees — another process pruning the directory, a user deleting a file, a restored
//! backup. A `stat` of the directory is true for the moment it is taken and needs no agreement
//! between writers, and this is a number shown to a person or compared against a quota, not one used
//! to decide whether a write may proceed.

use std::path::{Path, PathBuf};

use girder_core::{
    CheckpointError, CheckpointStore, PersistError as SearchPersistError, TerminalPersistError,
    TerminalPersistStore, search_index_path,
};

/// The suffix the checkpoint store gives a per-session file.
///
/// Spelled out rather than imported because `checkpoint_store`'s own `FILE_SUFFIX` is not reachable
/// from here without widening that module's surface, and this module is not allowed to. The coupling
/// is real and is pinned by a test: it asserts this constant equals `checkpoint_store::FILE_SUFFIX`
/// and that a file written through the real [`CheckpointStore`] is counted by
/// [`Persistence::footprint`] and removed by [`Persistence::prune`], so a change to either side fails
/// the build rather than silently orphaning files.
const CHECKPOINT_SUFFIX: &str = ".checkpoints.json";

/// The suffix the terminal store gives a transcript file.
///
/// Spelled out for the same reason, and pinned the same way: the tests save through the real
/// [`TerminalPersistStore`] and assert the bytes appear under [`Footprint::terminals_bytes`].
const TERMINAL_SUFFIX: &str = ".log";

/// Why this layer could not prune.
///
/// Each variant carries the failing store's own error unchanged, because the caller's response is the
/// same in every case — log it and retry later — and collapsing them into a string would throw away
/// the store-specific detail that says which of the three failed. The variants stay distinct so a
/// test and a person reading a log can tell a terminal-store failure from a checkpoint one without
/// parsing a message.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// The terminal store could not be pruned.
    #[error("terminal output could not be pruned: {0}")]
    Terminals(#[from] TerminalPersistError),

    /// The checkpoint store reported a failure.
    ///
    /// The sweep below reads the directory itself rather than calling into the checkpoint store,
    /// which has no directory-wide `prune`, so this variant is where the I/O failures of that sweep
    /// are reported: they name a path inside the same directory and are the same class of problem,
    /// and reusing the checkpoint store's error shape keeps a caller from having to learn a second
    /// one for the same directory.
    #[error("checkpoints could not be pruned: {0}")]
    Checkpoints(#[from] CheckpointError),

    /// The search index could not be removed.
    #[error("the search index could not be removed: {0}")]
    SearchIndex(#[from] SearchPersistError),
}

/// What one [`Persistence::prune`] call removed.
///
/// Counts are reported separately rather than as a total because the numbers mean different things
/// to a caller: a removed checkpoint is a rewind point that no longer exists, while a removed
/// transcript is output a client can no longer attach to, and neither is interchangeable with the
/// other in a log line or a report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Transcript files removed.
    pub terminals: usize,
    /// Checkpoint files removed.
    pub checkpoints: usize,
}

/// Bytes this layer occupies on disk, by store.
///
/// The split exists so a report can say which store grew. The values come from `stat` of the
/// directory entries, so they describe what is on disk now rather than what this process wrote.
#[allow(
    clippy::struct_field_names,
    reason = "the `_bytes` suffix is the unit, and every field needs it: a caller reading \
              `checkpoints_bytes` cannot mistake it for a count of files, which `Pruned` does count. \
              Dropping the suffix would make the two types look interchangeable at the call site"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    /// Bytes in `{id}.checkpoints.json` files.
    pub checkpoints_bytes: u64,
    /// Bytes in `{id}.log` files.
    pub terminals_bytes: u64,
    /// Bytes in the search index, or zero when there is none.
    pub search_index_bytes: u64,
}

impl Footprint {
    /// The total bytes this layer occupies.
    ///
    /// Saturating for the same reason [`crate::StorageQuota`]'s arithmetic is: a sum that wrapped
    /// would report a deployment as nearly empty at exactly the moment it is full, which is the one
    /// direction of error a quota check must not make.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.checkpoints_bytes
            .saturating_add(self.terminals_bytes)
            .saturating_add(self.search_index_bytes)
    }
}

/// The persistence layer of one deployment.
///
/// Cheap to construct and stateless beyond its paths: two instances over the same root see the same
/// data, because every value is read from disk rather than cached here.
#[derive(Debug, Clone)]
pub struct Persistence {
    /// The checkpoint store, which reads and writes `{id}.checkpoints.json`.
    checkpoints: CheckpointStore,
    /// The terminal store, which reads and writes `{id}.log`.
    terminals: TerminalPersistStore,
    /// The search cache, which reads and writes `.search-index.json`.
    ///
    /// Held as a path because `search_persist` is a set of free functions over a path rather than a
    /// store object, and wrapping it in one here would add a type whose only method delegates.
    search_index_path: PathBuf,
}

impl Persistence {
    /// Builds the layer over `root`, the directory that holds a deployment's session data.
    ///
    /// All three stores live directly in `root`, beside the session logs (`{id}.jsonl.zst`). The names
    /// do not collide, and the reason is that each store is identified by a suffix rather than by a
    /// directory of its own:
    ///
    /// - checkpoints are `{id}.checkpoints.json`;
    /// - terminal transcripts are `{id}.log`;
    /// - the search cache is the single `.search-index.json`.
    ///
    /// None of those can be produced from another. A log is `{id}.jsonl.zst`, whose stem is `{id}`, so
    /// no suffix of it is `.log` and it never ends in `.checkpoints.json`; and the two dotted names
    /// begin with `.`, which no session id may, because a leading dot is reserved for the staging
    /// siblings `write_atomic` creates. A caller therefore does not have to keep the stores apart —
    /// the layout does it.
    ///
    /// The directory is not created here: a layer nothing has written to needs no directory, and
    /// every store below creates what it needs on its first write.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            checkpoints: CheckpointStore::new(root.clone()),
            terminals: TerminalPersistStore::new(root.clone()),
            search_index_path: search_index_path(&root),
        }
    }

    /// The checkpoint store, for reading and writing rewind points.
    ///
    /// Borrowed rather than returned by value so the caller cannot take one away and pair it with a
    /// root of its own — the whole point of this type is that the three stores share one.
    #[must_use]
    pub fn checkpoints(&self) -> &CheckpointStore {
        &self.checkpoints
    }

    /// The terminal store, for reading and writing transcripts.
    #[must_use]
    pub fn terminals(&self) -> &TerminalPersistStore {
        &self.terminals
    }

    /// The path of the search cache.
    ///
    /// A path rather than a store because saving and loading the cache are free functions over one;
    /// handing back the path keeps this layer from inventing a second API for them.
    #[must_use]
    pub fn search_index_path(&self) -> &Path {
        &self.search_index_path
    }

    /// Removes data older than `older_than_ms`, measured back from `now_ms`.
    ///
    /// Takes both ends of the comparison rather than reading the clock here, so a test can pin the
    /// cutoff and so a deployment running retention on a schedule can use one `now` for every store —
    /// two reads of the clock would put the stores in different windows by the microseconds between
    /// them.
    ///
    /// The search index has no age of its own that can be tested this way: it describes every session
    /// at once and is rewritten wholesale, so a single stale record is not a stale file. It is pruned
    /// by removing the whole file, which is safe because it is a cache rebuilt on the next query —
    /// see [`crate::search_persist`] for why a missing cache is not a failure.
    ///
    /// # Errors
    ///
    /// Returns [`PersistError::Terminals`], [`PersistError::Checkpoints`], or
    /// [`PersistError::SearchIndex`] when that stage failed, in which case the stages after it were
    /// not attempted. In a successful result the counts are exact: nothing that failed is counted as
    /// removed.
    pub fn prune(&self, now_ms: u64, older_than_ms: u64) -> Result<Pruned, PersistError> {
        let terminals = self.terminals.prune(now_ms, older_than_ms)?;
        let checkpoints = self.prune_checkpoints(now_ms, older_than_ms)?;
        // Removed after the stages that report counts, so a failure here is the last thing that can
        // happen and the counts above still describe exactly what was deleted.
        self.remove_search_index()?;
        Ok(Pruned {
            terminals,
            checkpoints,
        })
    }

    /// Bytes this layer occupies on disk, by store.
    ///
    /// Never fails: a directory that cannot be read measures as zero. That is deliberate. This value
    /// is displayed and compared against a quota, and a caller has no better answer when a `stat`
    /// fails — returning an error would make the disk report unavailable for a whole deployment
    /// because one entry could not be inspected, while a zero keeps the rest of the numbers usable
    /// and is visibly "nothing measured".
    ///
    /// The session logs themselves are not counted: this is the persistence layer's footprint, and the
    /// logs belong to the session store, which measures them in [`crate::StorageQuota`].
    #[must_use]
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::default();
        let Ok(directory) = std::fs::read_dir(self.root()) else {
            return footprint;
        };
        // Resolved once rather than per entry, and as a `&str` so the comparison below is between the
        // same types: the index is one file at a known name, not one file per id.
        let index_name = self
            .search_index_path
            .file_name()
            .and_then(|name| name.to_str());
        for entry in directory.flatten() {
            // `DirEntry::metadata` does not follow a symlink, so a link named like one of these files
            // is not counted as a regular file — the same rule the terminal store applies when it
            // lists transcripts, and for the same reason: a link can point outside the root.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.ends_with(CHECKPOINT_SUFFIX) {
                footprint.checkpoints_bytes =
                    footprint.checkpoints_bytes.saturating_add(metadata.len());
            } else if name.ends_with(TERMINAL_SUFFIX) {
                footprint.terminals_bytes =
                    footprint.terminals_bytes.saturating_add(metadata.len());
            } else if Some(name) == index_name {
                footprint.search_index_bytes = metadata.len();
            }
        }
        footprint
    }

    /// The root every store is rooted at.
    ///
    /// Derived from the index path rather than stored a second time: two fields holding one fact can
    /// disagree after a clone-and-mutate, and the index path is the one computed from the root at
    /// construction, so it is the root plus [`girder_core::search_persist::FILE_NAME`].
    fn root(&self) -> &Path {
        self.search_index_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
    }

    /// Removes every checkpoint file whose modification time is older than the cutoff.
    ///
    /// The checkpoint store has no directory-wide `prune` of its own — its API is per session, because
    /// a checkpoint is only meaningful for the session it names — so the sweep is done here over the
    /// directory. Only files carrying the checkpoint suffix are considered, and only their
    /// modification time is read: the contents are irrelevant to the question "is this old", and
    /// parsing them would turn a retention pass into a way for one damaged file to stop the others
    /// from being pruned.
    ///
    /// A file that disappears between the scan and the unlink is not an error: another process pruned
    /// it first, and the caller's goal is met either way.
    fn prune_checkpoints(&self, now_ms: u64, older_than_ms: u64) -> Result<usize, PersistError> {
        // Saturating for the same reason the terminal store saturates: a policy wider than the clock
        // is "keep everything", which the epoch cutoff expresses without a special case.
        let cutoff = now_ms.saturating_sub(older_than_ms);
        let directory = match std::fs::read_dir(self.root()) {
            Ok(directory) => directory,
            // Nothing has ever been written here, so there is nothing to prune. An error would make
            // the first retention run on a fresh deployment fail for no reason.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(CheckpointError::Io {
                    path: self.root().display().to_string(),
                    detail: error.to_string(),
                }
                .into());
            }
        };

        let mut removed = 0;
        for entry in directory {
            let entry = entry.map_err(|error| CheckpointError::Io {
                path: self.root().display().to_string(),
                detail: error.to_string(),
            })?;
            let path = entry.path();
            let is_checkpoint = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(CHECKPOINT_SUFFIX));
            if !is_checkpoint {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                // Removed since the scan. Not this call's removal, so not counted.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(CheckpointError::Io {
                        path: path.display().to_string(),
                        detail: error.to_string(),
                    }
                    .into());
                }
            };
            if !metadata.is_file() {
                continue;
            }
            let Some(modified) = modified_ms(&metadata) else {
                // Not provably old. Keeping it is the recoverable mistake; the other one is not.
                continue;
            };
            if modified >= cutoff {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CheckpointError::Io {
                        path: path.display().to_string(),
                        detail: error.to_string(),
                    }
                    .into());
                }
            }
        }
        Ok(removed)
    }

    /// Removes the search cache, treating an absent file as success.
    ///
    /// A missing cache is the state a deployment is in before its first search, so it is not a
    /// failure: a retention pass over a deployment nothing has searched must not report an error.
    fn remove_search_index(&self) -> Result<(), PersistError> {
        match std::fs::remove_file(&self.search_index_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SearchPersistError::Io {
                detail: format!("`{}`: {error}", self.search_index_path.display()),
            }
            .into()),
        }
    }
}

/// A file's modification time in milliseconds since the epoch, when it can be read.
///
/// Milliseconds because that is the unit [`Persistence::prune`] takes, and a `SystemTime` cannot be
/// compared to a `u64` without choosing one.
fn modified_ms(metadata: &std::fs::Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(since_epoch.as_millis()).ok()
}
