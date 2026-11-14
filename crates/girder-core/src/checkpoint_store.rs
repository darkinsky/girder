//! Durable checkpoints, so a rewind does not have to re-read the log to find its own history.
//!
//! [`crate::session_rewind`] computes *where* a log can be rewound to: it walks a slice of events and
//! returns the positions where a turn closed. It deliberately does not write anything, and it takes
//! the whole log as its input, so every rewind — and every "where can I go back to?" question asked
//! before one — pays for reading and decoding the entire session. This module is the other half: it
//! records the points that were already computed, one small file per session, so the answers survive
//! the process that computed them and a later reader can list them without touching the log.
//!
//! # Why a checkpoint is stored rather than derived on demand
//!
//! Deriving is correct and slow in the same breath. A log is compressed JSONL read frame by frame
//! ([`crate::SessionStore::read_all`]); a hundred-turn session is hundreds of events to decompress
//! and parse before the first `turn/end` can be named, and that cost is paid *again* for each of the
//! three questions a rewind UI asks: how many points are there, which is the latest, where does it
//! land. Storing the position turns all three into one small file read. The stored record is also the
//! only place a checkpoint can carry something the log does not — [`Kind`] and the compaction
//! [`Checkpoint::summary`] — because the log records events, not decisions about them.
//!
//! # Why one file per session
//!
//! The alternative is one index for every session, and it fails in the direction that hurts: a
//! crash, a partial write, or a single unparsable byte would invalidate the checkpoints of every
//! session at once, including sessions that were never being written to. With a file per session,
//! the blast radius of a bad write is exactly the session being written. It also removes contention
//! entirely: two agents compacting two conversations share no file, so neither can delay or corrupt
//! the other, and the only writer coordination that remains is between two writes to *one* session.
//!
//! # Why the session id is validated
//!
//! The id is used as a file name. An id containing `/` or `..` would address a path outside the
//! directory this store was given — `../../etc/foo` writes wherever the process can write — so the
//! check is a containment guarantee, not a tidiness one. The rule matches
//! [`crate::search_persist`]'s for the same reason, and it is enforced in one place
//! ([`Store::path_for`]) so no method can be added later that forgets it.
//!
//! # Why every write is a whole-file atomic replacement
//!
//! A checkpoint file is tiny — one record per turn at most — so appending in place buys nothing and
//! costs the one property that matters: an interrupted append leaves a half-written record that
//! parses as neither the old file nor the new one, and there is no framing to tell the two apart
//! (this is the same problem that stops the session log itself from being truncated in place). So
//! [`Store::save`] reads the file, adds the record, and replaces the file through
//! [`girder_core::write_atomic`], which stages a sibling and renames it: a crash leaves the previous
//! complete file, never a torn one.
//!
//! The rename protects a *reader*; it does not serialise two writers, which would otherwise both
//! read the same file and the second rename would silently drop the first writer's record. Saves
//! therefore take a process-wide mutex for the duration of the read-modify-write. A cross-process
//! race remains, and is accepted rather than solved here: checkpoints for one session are written by
//! that session's agent loop, and the cost of losing one is a rewind point that has to be recomputed
//! from the log, not a corrupted conversation. [`girder_core::with_lock`] is the existing tool if a
//! future caller needs the cross-process guarantee, but it is async and this store's callers are not.
//!
//! # Why a damaged file is an error here, and a rebuild in the search cache
//!
//! [`crate::search_persist`] treats a corrupt file as "rebuild it", because a cache that cannot be
//! read costs a slow query and nothing else. This data is not a cache of something derivable at a
//! known path: the log is the source of truth, but the *decision* to checkpoint — a manual mark, the
//! summary taken before a compaction — exists nowhere else. Silently answering "no checkpoints" would
//! turn into "the rewind button is gone" or, worse, into a [`Store::truncate_after`] that deletes
//! nothing and reports success. The caller can rebuild the positions from
//! [`crate::checkpoints`], so the error is recoverable; it just has to be visible.
//!
//! # How this relates to a rewind
//!
//! A rewind moves the log back to a stored [`Checkpoint::seq`], which invalidates every point past
//! it: those positions describe events the conversation no longer has. [`Store::truncate_after`] is
//! how that invalidation is recorded. If it were skipped, [`Store::latest`] would keep naming a
//! position beyond the end of the log, and the next rewind — which clamps a stale cursor back to the
//! nearest `turn/end` — would land somewhere the user never chose. Truncation is not cleanup; it is
//! the half of the rewind that makes the stored history agree with the log.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use girder_core::{Mode, write_atomic};

/// The suffix that turns a session id into the name of its checkpoint file.
///
/// Suffixed rather than used bare so a checkpoint file can never be mistaken for the session log
/// (`.jsonl.zst`) or for the search index that lives beside it, and so a person looking at the
/// directory can tell what a file is without opening it.
pub const FILE_SUFFIX: &str = ".checkpoints.json";

/// Serialises a read-modify-write of a checkpoint file within this process.
///
/// One lock for all sessions rather than one per file: a save is a read, a few serialisation
/// microseconds, and a rename, taken at most once per turn, so the contention this could cause is
/// smaller than the bookkeeping a per-session lock map would need. Readers do not take it — they rely
/// on the rename being atomic.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Why a checkpoint could not be produced or reached.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The session id could not name a file inside the store's directory.
    ///
    /// A programming error rather than a runtime condition: the id came from the caller, and the
    /// refusal is the point — accepting it would write outside the directory.
    #[error("`{session}` is not a usable session id: {reason}")]
    InvalidSession {
        /// The id that was refused, quoted back so the caller can see what it passed.
        session: String,
        /// Which rule it broke.
        reason: &'static str,
    },

    /// The file could not be read or replaced.
    #[error("the checkpoints at `{path}` could not be reached: {detail}")]
    Io {
        /// The file, as it was addressed.
        path: String,
        /// What failed.
        detail: String,
    },

    /// The file exists and is not a checkpoint file this store wrote.
    ///
    /// Deliberately not folded into [`StoreError::Io`]: a caller may choose to rebuild a damaged file
    /// from the log, and it can only make that choice if it can tell damage from an I/O failure.
    #[error("the checkpoints at `{path}` are not a checkpoint file: {detail}")]
    Corrupt {
        /// The file that could not be understood.
        path: String,
        /// What about it was wrong.
        detail: String,
    },
}

/// What produced a checkpoint.
///
/// The reason a checkpoint exists is not recoverable from its position, and the three kinds mean
/// different things to a user: an automatic mark is filler, a manual one is something they asked for,
/// and a compaction one is the only remaining handle on messages that no longer exist. Rendering them
/// identically would leave a rewind list where the one point that matters is indistinguishable from
/// the dozens that do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Taken immediately before a compaction dropped a span of the conversation.
    ///
    /// This is the checkpoint that makes a compaction survivable: the dropped messages are gone from
    /// the log, so rewinding to this point is the only way back to the context that produced
    /// [`Checkpoint::summary`].
    Compaction,

    /// Marked by the user.
    ///
    /// Nothing computes it and nothing may discard it: a manual mark is an instruction about what the
    /// user intends to return to, so a policy that prunes checkpoints has to keep these.
    Manual,

    /// Taken on a schedule — for example every N turns.
    ///
    /// These are the ones that can be pruned; their value is that one is close by, not that a
    /// particular one survives.
    Automatic,
}

/// One recorded position in a session.
///
/// Field names are the on-disk contract: renaming one is a breaking change to the format, so a future
/// change bumps the shape rather than repurposing a name.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    /// The session this belongs to.
    ///
    /// Carried inside the record even though the file name already says it: a file copied, moved, or
    /// restored by hand would otherwise lose the association silently, and [`Store::load`] uses this
    /// field to notice exactly that.
    pub session: String,

    /// The position, as a [`crate::RewindPoint::after_seq`]-style event count: everything up to and
    /// including this sequence is kept.
    ///
    /// The same number the log's own cursor takes, so a stored checkpoint can be handed to a rewind
    /// without conversion and without an off-by-one between the two spellings.
    pub seq: u64,

    /// What produced it.
    pub kind: Kind,

    /// The summary that was produced when this checkpoint was taken, if any.
    ///
    /// Present for [`Kind::Compaction`], where the summary of the dropped span exists nowhere else —
    /// the compaction replaced those messages with it in the model's view, and only this field keeps
    /// a copy once the log has moved past the span. `None` means "no summary was taken", which is a
    /// different statement from "the summary was empty".
    pub summary: Option<String>,

    /// When it was taken, in milliseconds since the Unix epoch.
    ///
    /// Milliseconds rather than seconds because these are taken inside a turn: two compaction
    /// checkpoints a second apart are ordinary, and a whole-second clock would order them by luck.
    pub at_ms: u64,
}

/// A directory of checkpoint files, one per session.
#[derive(Clone, Debug)]
pub struct Store {
    /// The directory the per-session files live in.
    dir: PathBuf,
}

impl Store {
    /// Opens the store rooted at `dir`.
    ///
    /// The directory is not created here: a store that has never saved anything needs no directory,
    /// and creating one at construction would make every read-only caller leave an empty directory
    /// behind. [`Store::save`] creates it on first write, through
    /// [`girder_core::write_atomic`], which creates the parent it is given.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The file that holds `session`'s checkpoints.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidSession`] when the id could not name a file inside [`Store`]'s
    /// directory — see the module docs for why that is refused rather than escaped.
    pub fn path_for(&self, session: &str) -> Result<PathBuf, StoreError> {
        validate_session(session)?;
        Ok(self.dir.join(format!("{session}{FILE_SUFFIX}")))
    }

    /// Adds a checkpoint to `checkpoint.session`, replacing any record already at that position.
    ///
    /// Replacing rather than appending a duplicate is what keeps a position unambiguous. Two records
    /// at one `seq` describe the *same* prefix of the log, so the newer one is the better answer to
    /// every question this store is asked, and uniquing here is what lets [`Store::latest`] return a
    /// single checkpoint instead of a set of candidates.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidSession`] for an unusable session id, [`StoreError::Corrupt`]
    /// when the existing file cannot be understood (the previous file is then left in place), and
    /// [`StoreError::Io`] when it cannot be read or replaced.
    pub fn save(&self, checkpoint: Checkpoint) -> Result<(), StoreError> {
        let path = self.path_for(&checkpoint.session)?;

        // Held across the read and the rename: the rename alone does not stop two writers from each
        // reading the old file and the second one dropping the first one's record.
        let _guard = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);

        let mut checkpoints = self.load(&checkpoint.session)?;
        if let Some(existing) = checkpoints
            .iter_mut()
            .find(|existing| existing.seq == checkpoint.seq)
        {
            *existing = checkpoint;
        } else {
            checkpoints.push(checkpoint);
        }
        self.replace(&path, &checkpoints)
    }

    /// Every checkpoint recorded for `session`, ordered by [`Checkpoint::seq`].
    ///
    /// Missing is empty: a session that has never been checkpointed is the normal state of a new
    /// session, not an error, and a caller listing rewind points should not have to distinguish it
    /// from one whose file was pruned to nothing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidSession`] for an unusable session id, [`StoreError::Corrupt`]
    /// when the file is not a checkpoint file for this session, and [`StoreError::Io`] when it exists
    /// and cannot be read.
    pub fn load(&self, session: &str) -> Result<Vec<Checkpoint>, StoreError> {
        let path = self.path_for(session)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // No file is not a failure: it is the state a session is in until it is first marked.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(StoreError::Io {
                    path: path.display().to_string(),
                    detail: error.to_string(),
                });
            }
        };

        let mut checkpoints: Vec<Checkpoint> =
            serde_json::from_slice(&bytes).map_err(|error| StoreError::Corrupt {
                path: path.display().to_string(),
                detail: error.to_string(),
            })?;

        // A record naming another session means the file was copied or edited rather than written
        // here. Serving it would hand the caller positions from a log it is not looking at, so the
        // mismatch is reported instead of dropped.
        if let Some(foreign) = checkpoints
            .iter()
            .find(|checkpoint| checkpoint.session != session)
        {
            return Err(StoreError::Corrupt {
                path: path.display().to_string(),
                detail: format!(
                    "it holds a checkpoint for session `{}`",
                    foreign.session
                ),
            });
        }

        // Sorted on read rather than only on write: the file is the boundary a hand edit or an older
        // version crosses, and every caller of this method — including `latest` — is allowed to
        // assume the order instead of re-establishing it.
        checkpoints.sort_by(|left, right| {
            left.seq
                .cmp(&right.seq)
                .then_with(|| left.at_ms.cmp(&right.at_ms))
        });
        Ok(checkpoints)
    }

    /// The checkpoint at the highest [`Checkpoint::seq`], or `None` when there is none.
    ///
    /// The highest position, not the last one written: [`Store::save`] is allowed to fill a gap, and
    /// a rewind always means "the furthest point the log actually reached", which is a property of
    /// the sequence rather than of the order the records arrived in.
    ///
    /// # Errors
    ///
    /// Same as [`Store::load`].
    pub fn latest(&self, session: &str) -> Result<Option<Checkpoint>, StoreError> {
        // `load` sorted ascending, so the maximum is the last element.
        Ok(self.load(session)?.pop())
    }

    /// Drops every checkpoint past `seq`, returning how many were removed.
    ///
    /// `seq` itself survives: it is the position the log was rewound *to*, so the record describing
    /// it is still true. This is the operation that makes a rewind stick — see the module docs for
    /// what leaving the later records behind would cost.
    ///
    /// A session with no file, or with nothing after `seq`, is left untouched: rewriting an unchanged
    /// file would be a write with nothing to write, and one that a concurrent reader would have to
    /// tolerate for no benefit.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidSession`] for an unusable session id, [`StoreError::Corrupt`]
    /// when the file cannot be understood (nothing is deleted then, so a damaged file is never
    /// silently emptied), and [`StoreError::Io`] when it cannot be read or replaced.
    pub fn truncate_after(&self, session: &str, seq: u64) -> Result<usize, StoreError> {
        let path = self.path_for(session)?;

        let _guard = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);

        let mut checkpoints = self.load(session)?;
        let before = checkpoints.len();
        checkpoints.retain(|checkpoint| checkpoint.seq <= seq);
        let removed = before - checkpoints.len();
        if removed == 0 {
            return Ok(0);
        }
        self.replace(&path, &checkpoints)?;
        Ok(removed)
    }

    /// Replaces the file at `path` with `checkpoints`, atomically.
    ///
    /// An emptied list is still written as an empty array rather than removing the file: "every
    /// checkpoint was truncated" and "this session was never checkpointed" both answer the same way
    /// to [`Store::load`], but only the file distinguishes them for a person looking at the
    /// directory, and a delete is one more failure path for no gain.
    fn replace(&self, path: &Path, checkpoints: &[Checkpoint]) -> Result<(), StoreError> {
        // Compact rather than pretty: the file is machine-read, and it is rewritten on every mark.
        let bytes = serde_json::to_vec(checkpoints).map_err(|error| StoreError::Corrupt {
            path: path.display().to_string(),
            detail: format!("the checkpoints could not be serialised: {error}"),
        })?;
        // Private, because a summary carries the conversation it summarises: anyone who can read the
        // file can read part of a session without the session's own permissions applying.
        write_atomic(path, &bytes, Mode::PRIVATE).map_err(|error| StoreError::Io {
            path: path.display().to_string(),
            detail: error.to_string(),
        })
    }
}

/// Refuses a session id that could not name a file directly inside the store's directory.
///
/// The rules are deliberately the ones in [`crate::search_persist`], so the two stores cannot
/// disagree about which ids are addressable.
fn validate_session(session: &str) -> Result<(), StoreError> {
    let reason = if session.is_empty() {
        Some("it is empty")
    } else if session == "." || session == ".." {
        // `.` would name the directory itself and `..` its parent; neither is a file this store owns.
        Some("it is a directory reference")
    } else if session.contains(['/', '\\']) {
        Some("it contains a path separator")
    } else if session.contains("..") {
        // Also covers a name that merely embeds `..`: there is no reason for one to, and refusing the
        // whole class is cheaper to keep correct than reasoning about each spelling.
        Some("it contains `..`")
    } else if session.contains('\0') {
        // A NUL truncates the name at the syscall boundary, so a validated string and the name the
        // filesystem sees would differ.
        Some("it contains a NUL byte")
    } else {
        None
    };

    match reason {
        Some(reason) => Err(StoreError::InvalidSession {
            session: session.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// The current time in milliseconds since the Unix epoch, for [`Checkpoint::at_ms`].
///
/// Provided rather than left to each caller because the unit is the part that goes wrong: a caller
/// that reached for seconds would still typecheck and would only be visible as wrongly ordered
/// checkpoints long afterwards. A clock set before the epoch yields `0`, which sorts first and is
/// visibly wrong rather than an error on a path nobody expects to fail.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
