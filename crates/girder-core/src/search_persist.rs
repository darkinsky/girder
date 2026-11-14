//! A persisted copy of the search text cache, so a restart does not re-decode every log.
//!
//! [`girder_core::SearchIndex`] removes the decode from the *second* query in a process. It does
//! nothing for the first one after a restart, because the cache lives only in memory: the process
//! that just started has an empty map, and the first search it serves rebuilds every entry it needs
//! by opening, decompressing, and JSON-parsing every session log it can see.
//!
//! # What that costs
//!
//! Concretely, at the size this harness is deployed at: 500 sessions of 100 KB each is ~49 MiB
//! compressed on disk, which decodes to roughly 73–244 MiB of text depending on the log's content,
//! and every one of those lines is then parsed into a `SessionEvent` before the searchable string can
//! be extracted from it. That is the work a single keystroke in a search box pays for once per
//! process lifetime — and a harness restarts often, because that is what deploying and upgrading are.
//! The persisted index turns it into one file read: no zstd, no JSON event parsing.
//!
//! # Why the version field, and what a mismatch means
//!
//! [`Snapshot`] carries [`VERSION`] because the shape of a record will change. When it does, an old
//! file is *not* an error: it is a cache that needs rebuilding. [`load`] therefore returns an empty
//! index for any version it does not recognise, and the caller proceeds to rebuild exactly as it
//! would have with no file at all. Failing here would turn an upgrade into "search is broken", which
//! is a strictly worse outcome than a slow first query.
//!
//! # Why a damaged file is treated the same way
//!
//! A truncated file, a file of zero bytes, a file someone edited, a file whose JSON is valid but
//! whose shape is not — all of these are the same situation as a version mismatch: the cache is
//! unusable and rebuildable. A broken *cache* must never disable the *feature*, so [`load`] answers
//! with an empty index rather than a [`PersistError`]. [`PersistError`] is reserved for the one case
//! a caller can act on, which is an I/O failure to read or write the file at all.
//!
//! # Why writes are atomic
//!
//! The cache is rewritten wholesale, so a non-atomic write is a window in which the file is a
//! prefix of the new content over the tail of the old — which parses as neither. [`save`] goes
//! through [`girder_core::write_atomic`], which stages a sibling and renames it over the target:
//! a concurrent reader sees the old file or the new one, and a crash mid-write leaves the old file,
//! not a permanently half-written one.
//!
//! # Why `size` is the invalidation key
//!
//! The same reason the in-memory cache uses it: a session log is append-only, so a change always
//! grows it, and comparing a `stat` length against the recorded one is a sound test that costs
//! nothing. [`load`] drops every record whose log is gone or whose size no longer matches, so a
//! restart picks up only the entries that are still true. Keeping a stale record would hand a search
//! text from a conversation that has since been extended.
//!
//! # Why JSON rather than a compact binary form
//!
//! The file is a cache, but it is a cache a person will occasionally want to look at when something
//! is wrong with search, and `serde_json` is already a dependency. Compression is the obvious next
//! step if the file proves too large to write quickly, and [`VERSION`] is what makes that change
//! safe: a reader that cannot understand a file rebuilds instead of guessing.
//!
//! # Why [`save`] takes records rather than an `&Index`
//!
//! It cannot take an `Index`. `search_index::Index` keeps its entries in a private map and exposes
//! no iterator — only `len`, `get(id, size)`, `insert`, `forget`, `clear`, and `prune`. There is no
//! way to enumerate what is in it from another module, and this module deliberately does not change
//! that type: its field visibility is its own decision, and a persistence layer is not a reason to
//! widen it. The caller therefore supplies the records it wants written, which it can build from the
//! public API by walking the sessions directory and calling `Index::get(id, size)` for each id it
//! finds. That keeps the coupling at the module boundary instead of inside this file.
//!
//! # Why saves do not merge
//!
//! [`save`] writes exactly the records it is given. A merge would have to read the existing file and
//! reconcile it with the caller's view, and the two can legitimately disagree: a record the caller
//! dropped may have been dropped *because* the log changed. Replacing is the honest semantic, and it
//! means a caller that wants to preserve other entries must read them with [`load`] first.

// The crate root is wired up separately, so until then nothing in the library references these
// items. The allow is scoped to this module and disappears in effect once it is exported.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use girder_core::{Mode, SearchIndex, SearchIndexEntry, write_atomic};

/// The name [`load`]'s default placement gives the index file.
///
/// It sits beside the session logs it describes, which is what lets [`load`] find the logs to check
/// against when the caller does not say where they are. The leading dot keeps it out of a directory
/// listing of sessions.
pub const FILE_NAME: &str = ".search-index.json";

/// The format version this build writes.
///
/// Bump it when [`Record`] or [`Snapshot`] changes shape incompatibly. [`load`] accepts only this
/// version and rebuilds on anything else, so an old file is never misread as a new one.
pub const VERSION: u32 = 1;

/// One session's persisted searchable text.
///
/// The field names are the on-disk contract: renaming one is a breaking change to the format and
/// must come with a [`VERSION`] bump.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// The session id, which is also the log's file stem.
    pub session: String,
    /// The log's length when the text was built, which is the invalidation key.
    ///
    /// [`load`] compares it with the log's length now and drops the record when they differ. It is
    /// recorded rather than recomputed because recomputing it means reading the log, which is the
    /// work this file exists to avoid.
    pub size: u64,
    /// One searchable string per event, in log order.
    ///
    /// Order is carried because a later reader may want to report a hit's position; it is otherwise
    /// opaque text, which is why nothing here describes what kind of event produced it.
    pub lines: Vec<String>,
}

/// The index file's contents.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    /// The format version, so a reader knows whether it understands the rest before reading it.
    pub version: u32,
    /// The entries. Order is not significant; [`load`] keys them by session id.
    pub records: Vec<Record>,
}

/// Why a snapshot could not be written, or its file could not be read.
///
/// A malformed or outdated *file* is deliberately not one of these: see the module docs for why that
/// is a rebuild rather than an error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PersistError {
    /// The file could not be read or replaced.
    ///
    /// The cause is flattened to a string because the caller's only sensible response is to log it
    /// and carry on without a cache; keeping a `std::io::Error` would make this type unable to
    /// derive the comparisons its callers use in tests.
    #[error("the search index could not be persisted: {detail}")]
    Io {
        /// What failed, including the path.
        detail: String,
    },

    /// A record was refused before anything was written.
    ///
    /// This is a programming error rather than a runtime condition: it means a caller built a record
    /// whose session id [`load`] would have to reject. Refusing at the write keeps a file that can
    /// never be fully used from being created in the first place.
    #[error("the search index was refused: {detail}")]
    Invalid {
        /// Which record, and why.
        detail: String,
    },
}

/// The index file's path under a sessions directory.
///
/// Paired with [`load`], which assumes the index sits beside the logs it describes.
#[must_use]
pub fn path_in(root: &Path) -> PathBuf {
    root.join(FILE_NAME)
}

/// Writes `records` to `path`, replacing any existing file atomically.
///
/// Every record is validated first, so a rejected save leaves the previous file untouched rather
/// than replacing it with a snapshot that [`load`] would have to discard.
///
/// # Errors
///
/// Returns [`PersistError::Invalid`] when a record's session id could not be a session id, and
/// [`PersistError::Io`] when the file cannot be written.
pub fn save(path: &Path, records: &[Record]) -> Result<(), PersistError> {
    for record in records {
        if !id_is_safe(&record.session) {
            return Err(PersistError::Invalid {
                detail: format!(
                    "session id `{}` contains a separator or `..`, so it could not address a log \
                     inside the sessions directory",
                    record.session
                ),
            });
        }
    }

    let snapshot = Snapshot {
        version: VERSION,
        records: records.to_vec(),
    };
    // Compact rather than pretty: the file is machine-read, and the pretty form multiplies the size
    // of the text this exists to avoid re-reading.
    let bytes = serde_json::to_vec(&snapshot).map_err(|error| PersistError::Invalid {
        detail: format!("the snapshot could not be serialised: {error}"),
    })?;
    // Private, because the file holds the searchable text of conversations: anyone who can read it
    // can read the sessions without the session permissions applying.
    write_atomic(path, &bytes, Mode::PRIVATE).map_err(|error| PersistError::Io {
        detail: format!("`{}`: {error}", path.display()),
    })
}

/// Reads the index at `path`, assuming the logs it describes sit beside it.
///
/// This is the placement [`path_in`] produces. A caller keeping the file elsewhere should use
/// [`load_for`], which is the same function with the log directory supplied explicitly.
///
/// # Errors
///
/// Returns [`PersistError::Io`] only when the file exists and cannot be read. A missing file, an
/// outdated version, and a damaged file all yield an empty index, because each of those is the same
/// situation to a caller: there is no usable cache, so build one.
pub fn load(path: &Path) -> Result<SearchIndex, PersistError> {
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    load_for(path, root)
}

/// Reads the index at `path`, dropping every record that no longer matches `root`.
///
/// A record survives only when its log still exists and is still the recorded length. That is what
/// makes a persisted cache safe without a rebuild of the whole file: the entries that are still true
/// are reused, and the rest are handed back to [`SearchIndex`] as work to do, which it already knows
/// how to do.
///
/// # Errors
///
/// Same as [`load`].
pub fn load_for(path: &Path, root: &Path) -> Result<SearchIndex, PersistError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // No cache is not a failure: it is the state a first run is in.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(SearchIndex::new()),
        Err(error) => {
            return Err(PersistError::Io {
                detail: format!("`{}`: {error}", path.display()),
            });
        }
    };

    // A damaged file is a cache to rebuild, not an error to report. This covers a truncated file, a
    // zero-length file, and one whose JSON is valid but whose shape is not.
    let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&bytes) else {
        return Ok(SearchIndex::new());
    };
    if snapshot.version != VERSION {
        return Ok(SearchIndex::new());
    }

    let mut index = SearchIndex::new();
    for record in snapshot.records {
        // An id that could address a file outside `root` is dropped rather than read: this file is
        // not trusted input, and the check must be the one `search_index` itself applies to an id.
        if !id_is_safe(&record.session) {
            continue;
        }
        // Gone, or changed since it was indexed. Either way the text is no longer the log's text.
        let Some(size) = log_size(root, &record.session) else {
            continue;
        };
        if size != record.size {
            continue;
        }
        index.insert(
            record.session,
            SearchIndexEntry {
                size,
                events: record.lines,
            },
        );
    }
    Ok(index)
}

/// The current length of a session log, or `None` when it is absent or not a file.
///
/// A `stat` rather than a read: this is the comparison that decides whether the whole point of this
/// module -- not reading the log -- is preserved.
fn log_size(root: &Path, id: &str) -> Option<u64> {
    let meta = std::fs::metadata(root.join(format!("{id}.jsonl.zst"))).ok()?;
    meta.is_file().then_some(meta.len())
}

/// Whether `id` could name a log directly inside the sessions directory.
///
/// The same rule `search_index::log_path` applies, and it must stay the same: if this were laxer, a
/// hand-written index file could make a load read a length from outside the directory it names.
fn id_is_safe(id: &str) -> bool {
    !id.is_empty() && !id.contains(['/', '\\']) && !id.contains("..") && !id.contains('\0')
}
