//! The `per-record` unit layout: one durable document per record.
//!
//! Upstream path: `packages/storage/storage-json/src/per-record-unit.ts`, with the document format
//! from `packages/storage/storage-json/src/format.ts` and the publish protocol from
//! `packages/storage/storage-json/src/atomic.ts`.
//!
//! # Why this module exists
//!
//! [`crate::storage`] declares [`crate::storage::Layout::PerRecord`] and then implements only
//! `Single`. Both its module header and [`crate::storage::FileBackend`] say so plainly: the layout is
//! "declared in the vocabulary but not implemented here". That is the one gap in the storage port that
//! is a *correctness* problem rather than a missing convenience. A caller who asks for `PerRecord` gets
//! a unit that silently behaves like `Single`: every `put_record` reads the whole unit, rewrites every
//! record, and reports success. The records are right; the documented property — "a write touches one
//! file", which is the entire reason to choose this layout for large or sparse records — is false, and
//! nothing says so at the call site.
//!
//! This module supplies the missing half: `<root>/<unit>/<table>/<key>.json`, one record per document,
//! plus `<root>/<unit>/global.json` for the global slot.
//!
//! # The four rules that make the layout different from `Single`
//!
//! 1. **The directory is the state.** This unit holds no in-memory copy of the records. `load`
//!    re-reads the tree every time, and each mutation touches exactly one document. Upstream says the
//!    same thing and relies on it: the domain layer owns the live tables, seeded by one open-time
//!    read, and serializes writes through its own chain.
//!
//! 2. **A foreign document reads as absent, not as a failure.** One record file that is malformed, or
//!    stamped with a version outside the accepted set, is *skipped* — it never bricks the unit. This
//!    is deliberately the opposite of [`crate::storage::FileBackend`], which rejects the whole unit on
//!    a bad document. The asymmetry is the point: under `Single` there is exactly one document, so a
//!    bad one is a total loss; under `PerRecord` a bad one is one record, and discarding it costs the
//!    unit nothing. An unaccepted version stamp discards the record rather than migrating it, because
//!    migrating data the owner never vouched for silently upgrades values under an unknown schema.
//!
//! 3. **Keys are path segments.** A key becomes a file name, so it must match `^[A-Za-z0-9_-]+$`. An
//!    unsafe key is refused at write rather than escaped: two spellings of one key that map to one
//!    path is a bug waiting for a filesystem to expose it.
//!
//! 4. **A batch delete stages before it unlinks.** Every document is renamed out of the readable set
//!    (`<key>.json` → `<key>.json.deleting`) and the directory is synced before the staged copies are
//!    removed. A failure while staging restores what it moved, so a rejection is normally unapplied.
//!    If the restore itself fails, the readable state is unknown and the unit reports
//!    [`RecordError::UnknownState`] on every later call instead of pretending to be usable.
//!
//! # Legacy bootstrap
//!
//! A unit that has per-record documents has a directory containing a declared table directory, or a
//! `global.json` when it declares a global. When neither exists, the tree is unmaterialized, and a
//! whole-unit file at `<root>/<unit>.json` — the `Single` layout's shape — is copied record by record
//! into the new tree, stamped with the *current* version. The legacy file is never modified or
//! deleted. It is migrated only when its `unit.name` matches and its stamped version is in the
//! accepted set; a foreign, malformed, or stale legacy document is left alone and reads as the empty
//! unit.
//!
//! # Why the API is synchronous
//!
//! The same reason [`crate::storage::FileBackend`] is: the backend contract in this crate is
//! [`crate::storage::KvBackend`], whose methods return [`crate::BoxFut`], but its implementation is
//! `std::fs` all the way down because there is no async filesystem in the dependency set. Making these
//! methods `async` would add a runtime hop over the same blocking calls. A caller that needs to get off
//! the reactor thread wraps the call in `spawn_blocking` at the boundary where it already knows the
//! cost.
//!
//! # What this module does not do
//!
//! It does not serialize concurrent writers to one unit, because upstream does not either: write
//! ordering is the caller's responsibility and the domain layer runs one write chain per unit. Adding
//! a lock here would hide that requirement rather than satisfy it.
//!
//! # Wiring
//!
//! This module is self-contained — it does not reference `crate::` items, so it can be compiled into
//! `tests/storage_gap.rs` through `#[path]` before it is declared in the crate root. [`Descriptor`]
//! carries exactly the fields of [`crate::storage::UnitDescriptor`] that this layout reads, and
//! [`Loaded`] has the shape of [`crate::storage::Snapshot`], so the adapter is a field-by-field move.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The file name of a unit's global-slot document.
///
/// Upstream names it literally `global.json` inside the unit directory, and that name is part of the
/// on-disk format.
pub const GLOBAL_DOCUMENT: &str = "global.json";

/// The suffix that marks a record document as still readable.
///
/// Only files ending in this suffix are read, which is what lets a staged deletion and a backup sit in
/// the same directory as unreadable residue.
pub const RECORD_SUFFIX: &str = ".json";

/// The suffix a record document carries while a batch delete is staged.
///
/// It deliberately does not end in [`RECORD_SUFFIX`], so a staged document — or residue from a crash
/// mid-cleanup — is invisible to every later read.
pub const STAGED_SUFFIX: &str = ".json.deleting";

/// A failure this layout reports.
///
/// The variants mirror the `StorageErrorCode` set upstream attaches to these failures
/// (`packages/storage/storage/src/error.ts`), minus the codes owned by the hub rather than a backend.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// A unit or table name broke the shape rule.
    #[error("invalid {what} name `{name}`: must match `^[a-z][a-z0-9_]*$`")]
    InvalidName {
        /// The offending name.
        name: String,
        /// Whether it was a unit or a table.
        what: &'static str,
    },

    /// A record key would not be safe as a path segment.
    #[error(
        "unit `{unit}`: per-record key `{key}` is not path-safe (must match `^[A-Za-z0-9_-]+$`)"
    )]
    UnsafeKey {
        /// The unit.
        unit: String,
        /// The offending key.
        key: String,
    },

    /// A table was not declared by the unit's descriptor.
    #[error("unit `{unit}` does not declare table `{table}`")]
    UnknownTable {
        /// The unit.
        unit: String,
        /// The table.
        table: String,
    },

    /// The unit declares no global slot.
    #[error("unit `{unit}` does not declare a global slot")]
    NoGlobal {
        /// The unit.
        unit: String,
    },

    /// The unit was closed.
    #[error("unit `{unit}` is closed")]
    Closed {
        /// The unit.
        unit: String,
    },

    /// A failed batch delete could not be restored, so the unit's readable state is unknown.
    ///
    /// Every later call on the unit reports this rather than operating on a tree that may hold half a
    /// batch. Upstream calls this code `unknown-state`.
    #[error("unit `{unit}` could not restore a failed batch delete; its state is unknown")]
    UnknownState {
        /// The unit.
        unit: String,
    },

    /// Reading, writing, renaming, or removing a document failed.
    #[error("storage io on `{path}`: {source}")]
    Io {
        /// The path.
        path: String,
        /// The cause.
        source: std::io::Error,
    },

    /// A record document could not be encoded.
    #[error("could not encode a record document: {reason}")]
    Encode {
        /// Why encoding failed.
        reason: String,
    },

    /// A backup stamp could not be formatted for the supplied instant and offset.
    #[error("could not format a backup stamp for {millis} ms at offset {offset} minutes")]
    InvalidStamp {
        /// The instant, in milliseconds on the UTC timeline.
        millis: i64,
        /// The local offset, in minutes.
        offset: i32,
    },
}

impl RecordError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

/// The allowed shape of a unit or table name.
///
/// Lowercase, starting with a letter, so a name is safe as a directory segment without escaping.
/// This is the same rule as [`crate::storage::is_valid_unit_name`]; it is restated here because this
/// module must compile without a crate root, and `tests/storage_gap.rs` asserts the two agree across a
/// corpus so the restatement cannot drift.
#[must_use]
pub fn is_valid_unit_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The allowed shape of a record key under this layout.
///
/// Keys become path segments, so the set is the intersection of what every mainstream filesystem
/// accepts and what needs no escaping. Same rule as [`crate::storage::is_valid_record_key`], restated
/// for the same reason as [`is_valid_unit_name`].
#[must_use]
pub fn is_safe_record_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The directory that holds one unit's documents: `<root>/<unit>/`.
#[must_use]
pub fn unit_directory(root: &Path, unit: &str) -> PathBuf {
    root.join(unit)
}

/// The document that holds one record: `<root>/<unit>/<table>/<key>.json`.
///
/// This function is the layout's public shape. It does not validate its arguments; a caller holding
/// arbitrary input must check [`is_safe_record_key`] first, which is what [`PerRecordUnit`] does.
#[must_use]
pub fn record_path(root: &Path, unit: &str, table: &str, key: &str) -> PathBuf {
    unit_directory(root, unit)
        .join(table)
        .join(format!("{key}{RECORD_SUFFIX}"))
}

/// The document that holds one unit's global singleton: `<root>/<unit>/global.json`.
#[must_use]
pub fn global_path(root: &Path, unit: &str) -> PathBuf {
    unit_directory(root, unit).join(GLOBAL_DOCUMENT)
}

/// The static identity and shape of one unit, as this layout reads it.
///
/// The fields are exactly the [`crate::storage::UnitDescriptor`] fields that affect the on-disk tree;
/// `layout` is implied by the type, so it is absent rather than carried and ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    /// Unit name; also the directory segment.
    pub name: String,
    /// Current format version, stamped on every document this unit writes.
    pub version: u32,
    /// Declared table names.
    pub tables: Vec<String>,
    /// Whether the unit carries a global singleton.
    pub has_global: bool,
    /// Older versions whose documents are also read, without being restamped.
    pub compatible_versions: Vec<u32>,
}

impl Descriptor {
    /// Builds a descriptor with no compatible versions and no global slot.
    #[must_use]
    pub fn new(name: impl Into<String>, version: u32, tables: Vec<String>) -> Self {
        Self {
            name: name.into(),
            version,
            tables,
            has_global: false,
            compatible_versions: Vec::new(),
        }
    }

    /// Declares a global singleton slot.
    #[must_use]
    pub fn with_global(mut self) -> Self {
        self.has_global = true;
        self
    }

    /// Declares an older version as readable.
    #[must_use]
    pub fn compatible_with(mut self, version: u32) -> Self {
        self.compatible_versions.push(version);
        self
    }

    /// The version stamps this unit reads as its own: the current one plus the declared compatible
    /// ones. A document stamped with anything else is foreign and reads as absent.
    #[must_use]
    pub fn accepted_versions(&self) -> Vec<u32> {
        let mut accepted = vec![self.version];
        accepted.extend(self.compatible_versions.iter().copied());
        accepted
    }

    /// Whether the names in this descriptor are safe as directory segments.
    fn validate(&self) -> Result<(), RecordError> {
        if !is_valid_unit_name(&self.name) {
            return Err(RecordError::InvalidName {
                name: self.name.clone(),
                what: "unit",
            });
        }
        for table in &self.tables {
            if !is_valid_unit_name(table) {
                return Err(RecordError::InvalidName {
                    name: table.clone(),
                    what: "table",
                });
            }
        }
        Ok(())
    }
}

/// A full read of one unit, reconstructed from the tree.
///
/// Every declared table is present, even when it has no records — a caller iterating the map does not
/// have to distinguish "declared and empty" from "not declared". `global` is `None` when the unit
/// declares no slot or the slot was never written. The shape matches [`crate::storage::Snapshot`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loaded {
    /// Records by table name, then by key.
    pub tables: BTreeMap<String, BTreeMap<String, Value>>,
    /// The global singleton, when declared and written.
    pub global: Option<Value>,
}

/// One opened `per-record` unit.
///
/// Stateless by design: it holds the descriptor and the unit directory, never the records. Every read
/// walks the tree and every write touches one document.
#[derive(Debug)]
pub struct PerRecordUnit {
    descriptor: Descriptor,
    directory: PathBuf,
    closed: bool,
    unusable: bool,
}

impl PerRecordUnit {
    /// Attaches to the unit `descriptor.name` under `root`.
    ///
    /// Touches nothing on the medium: the first [`PerRecordUnit::load`] or write materializes what it
    /// needs. The descriptor's names are validated here, because an invalid one would otherwise become
    /// a directory path on the first write.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError::InvalidName`] for a unit or table name that is not a safe directory
    /// segment.
    pub fn open(root: &Path, descriptor: Descriptor) -> Result<Self, RecordError> {
        descriptor.validate()?;
        let directory = unit_directory(root, &descriptor.name);
        Ok(Self {
            descriptor,
            directory,
            closed: false,
            unusable: false,
        })
    }

    /// The descriptor this unit was opened with.
    #[must_use]
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// The unit directory on the medium.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Reads the full current state by walking the tree.
    ///
    /// A missing unit directory is the empty unit, and still runs the legacy bootstrap. A directory
    /// holding no declared table directory and no declared `global.json` is *unmaterialized*, so the
    /// bootstrap also runs — that shape is exactly a unit freshly upgraded from `Single`.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError::Closed`] after `close`, [`RecordError::UnknownState`] after a batch delete
    /// that could not be restored, and [`RecordError::Io`] for a directory it cannot list. A single
    /// unreadable *record* document is not an error: it reads as absent.
    pub fn load(&self) -> Result<Loaded, RecordError> {
        self.assert_open()?;
        let accepted = self.descriptor.accepted_versions();
        let mut state = Loaded::default();
        for table in &self.descriptor.tables {
            state.tables.insert(table.clone(), BTreeMap::new());
        }

        let mut materialized = false;
        match fs::read_dir(&self.directory) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|error| RecordError::io(&self.directory, error))?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
                    if is_directory && self.declares_table(name) {
                        materialized = true;
                        load_table(&mut state, name, &accepted, &entry.path())?;
                    } else if name == GLOBAL_DOCUMENT && self.descriptor.has_global {
                        // The entry's presence is what suppresses the bootstrap, even when the
                        // document itself is foreign: a global document that exists but is
                        // unreadable must not resurrect a legacy copy behind it.
                        materialized = true;
                        if let Some(value) =
                            read_record(&global_path_of(&self.directory), &accepted)
                        {
                            state.global = Some(value);
                        }
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(RecordError::io(&self.directory, error)),
        }

        if !materialized {
            bootstrap_legacy(&self.descriptor, &self.directory, &accepted, &mut state)?;
        }
        Ok(state)
    }

    /// Durably replaces one record: its own document, atomically.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError::UnsafeKey`] for a key that is not path-safe,
    /// [`RecordError::UnknownTable`] for an undeclared table, [`RecordError::Closed`] after `close`, and
    /// [`RecordError::Io`] when the document cannot be written.
    pub fn put(&mut self, table: &str, key: &str, value: &Value) -> Result<(), RecordError> {
        self.assert_open()?;
        let directory = self.table_directory(table)?;
        if !is_safe_record_key(key) {
            return Err(RecordError::UnsafeKey {
                unit: self.descriptor.name.clone(),
                key: key.to_owned(),
            });
        }
        let text = serialize_record(self.descriptor.version, value)?;
        write_document(&directory.join(format!("{key}{RECORD_SUFFIX}")), &text)
    }

    /// Durably deletes one record.
    ///
    /// Idempotent: a missing key is a no-op. This is the one-key case of
    /// [`PerRecordUnit::delete_many`], so it takes the same staging path.
    ///
    /// # Errors
    ///
    /// Same as [`PerRecordUnit::delete_many`].
    pub fn delete(&mut self, table: &str, key: &str) -> Result<usize, RecordError> {
        self.delete_many(table, &[key])
    }

    /// Durably deletes many records as one batch, returning how many documents were removed.
    ///
    /// Documents are renamed out of the readable set first; only after the whole batch is staged and
    /// the directory is synced are the staged copies unlinked. A failure while staging restores what it
    /// moved, so a rejection normally leaves the batch unapplied. If that restoration itself fails, the
    /// readable state is unknown: the unit reports [`RecordError::UnknownState`] from then on, while
    /// `close` still releases it.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError::UnsafeKey`] for a key that is not path-safe,
    /// [`RecordError::UnknownTable`] for an undeclared table, [`RecordError::Closed`] after `close`, and
    /// [`RecordError::Io`] when a document cannot be staged.
    pub fn delete_many(&mut self, table: &str, keys: &[&str]) -> Result<usize, RecordError> {
        self.assert_open()?;
        let directory = self.table_directory(table)?;

        // Validate every key before touching the medium: a batch that is rejected halfway would leave
        // earlier documents staged for no reason.
        let mut unique: Vec<&str> = Vec::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for key in keys {
            if !is_safe_record_key(key) {
                return Err(RecordError::UnsafeKey {
                    unit: self.descriptor.name.clone(),
                    key: (*key).to_owned(),
                });
            }
            if seen.insert(key) {
                unique.push(key);
            }
        }

        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut failure: Option<RecordError> = None;
        for key in &unique {
            let from = directory.join(format!("{key}{RECORD_SUFFIX}"));
            let to = directory.join(format!("{key}{STAGED_SUFFIX}"));
            match fs::rename(&from, &to) {
                Ok(()) => staged.push((from, to)),
                // Idempotent: a key that was never written is not an error.
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    failure = Some(RecordError::io(&from, error));
                    break;
                }
            }
        }
        if failure.is_none() && !staged.is_empty() {
            if let Err(error) = sync_directory(&directory) {
                failure = Some(RecordError::io(&directory, error));
            }
        }

        if let Some(failure) = failure {
            let mut restore_failed = false;
            for (from, to) in staged.iter().rev() {
                if fs::rename(to, from).is_err() {
                    restore_failed = true;
                }
            }
            if !staged.is_empty() && sync_directory(&directory).is_err() {
                restore_failed = true;
            }
            if restore_failed {
                self.unusable = true;
                return Err(RecordError::UnknownState {
                    unit: self.descriptor.name.clone(),
                });
            }
            return Err(failure);
        }

        let removed = staged.len();
        // The renames already removed the batch from the readable set, so unlinking is cleanup: a
        // failed unlink leaves an unreadable file rather than a resurrected record.
        for (_, staged_path) in &staged {
            let _ = fs::remove_file(staged_path);
        }
        Ok(removed)
    }

    /// Moves one record's document aside as `<key>.json.bak.<YYYYMMDDHHmm>`, returning the new path.
    ///
    /// The moved file no longer ends in `.json`, so every later read ignores it while the bytes stay on
    /// disk for inspection. A same-minute backup of the same key overwrites the previous backup, because
    /// the newer bytes are the ones worth keeping.
    ///
    /// The stamp is *local* time, matching upstream, so the caller supplies the offset: this module has
    /// no time zone database and the clock-to-local conversion belongs to the clock that owns it.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError::InvalidStamp`] when the instant and offset do not name a four-digit year,
    /// [`RecordError::UnsafeKey`] for an unsafe key, [`RecordError::UnknownTable`] for an undeclared
    /// table, and [`RecordError::Io`] when the document cannot be moved.
    pub fn backup(
        &mut self,
        table: &str,
        key: &str,
        millis: i64,
        offset_minutes: i32,
    ) -> Result<PathBuf, RecordError> {
        self.assert_open()?;
        let directory = self.table_directory(table)?;
        if !is_safe_record_key(key) {
            return Err(RecordError::UnsafeKey {
                unit: self.descriptor.name.clone(),
                key: key.to_owned(),
            });
        }
        let stamp =
            format_backup_stamp(millis, offset_minutes).ok_or(RecordError::InvalidStamp {
                millis,
                offset: offset_minutes,
            })?;
        let path = directory.join(format!("{key}{RECORD_SUFFIX}"));
        let moved = directory.join(format!("{key}{RECORD_SUFFIX}.bak.{stamp}"));
        fs::rename(&path, &moved).map_err(|error| RecordError::io(&path, error))?;
        Ok(moved)
    }

    /// Durably replaces the global singleton.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError::NoGlobal`] when the unit declares no slot. A stale `global.json` left by a
    /// descriptor that used to declare one is simply invisible to this unit.
    pub fn set_global(&mut self, value: &Value) -> Result<(), RecordError> {
        self.assert_open()?;
        if !self.descriptor.has_global {
            return Err(RecordError::NoGlobal {
                unit: self.descriptor.name.clone(),
            });
        }
        let text = serialize_record(self.descriptor.version, value)?;
        write_document(&global_path_of(&self.directory), &text)
    }

    /// Releases the unit. Idempotent, and always possible: a unit flagged
    /// [`RecordError::UnknownState`] is still closable.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Whether [`PerRecordUnit::close`] has been called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn declares_table(&self, name: &str) -> bool {
        self.descriptor.tables.iter().any(|table| table == name)
    }

    fn table_directory(&self, table: &str) -> Result<PathBuf, RecordError> {
        if self.declares_table(table) {
            Ok(self.directory.join(table))
        } else {
            Err(RecordError::UnknownTable {
                unit: self.descriptor.name.clone(),
                table: table.to_owned(),
            })
        }
    }

    fn assert_open(&self) -> Result<(), RecordError> {
        if self.closed {
            return Err(RecordError::Closed {
                unit: self.descriptor.name.clone(),
            });
        }
        if self.unusable {
            return Err(RecordError::UnknownState {
                unit: self.descriptor.name.clone(),
            });
        }
        Ok(())
    }
}

fn global_path_of(directory: &Path) -> PathBuf {
    directory.join(GLOBAL_DOCUMENT)
}

/// Reads one declared table's records into `state`.
///
/// Only `*.json` files whose stem is a safe key are considered; a file that fails that test, or whose
/// document is foreign, is skipped rather than reported.
fn load_table(
    state: &mut Loaded,
    table: &str,
    accepted: &[u32],
    directory: &Path,
) -> Result<(), RecordError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        // A table directory that vanished between the unit listing and this read is an empty table:
        // the per-record contract prefers a lost record over a failed unit.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(RecordError::io(directory, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| RecordError::io(directory, error))?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(key) = file_name.strip_suffix(RECORD_SUFFIX) else {
            continue;
        };
        if !is_safe_record_key(key) {
            continue;
        }
        let Some(value) = read_record(&entry.path(), accepted) else {
            continue;
        };
        if let Some(records) = state.tables.get_mut(table) {
            records.insert(key.to_owned(), value);
        }
    }
    Ok(())
}

/// Reads one record document, or `None` when it is foreign.
///
/// "Foreign" covers every way a document can fail to be one of ours: unreadable, not JSON, not an
/// object, missing a numeric version stamp, or stamped with a version outside `accepted`. None of them
/// is an error, because one bad record must not fail the unit.
fn read_record(path: &Path, accepted: &[u32]) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    parse_record(&text, accepted)
}

/// Copies a whole-unit document into the per-record tree, once.
///
/// Runs only when the tree holds no declared table directory and no declared global document. The
/// legacy file is read, never written: a migration that deleted its source would lose the only copy if
/// the new tree turned out to be wrong.
fn bootstrap_legacy(
    descriptor: &Descriptor,
    directory: &Path,
    accepted: &[u32],
    state: &mut Loaded,
) -> Result<(), RecordError> {
    let legacy_path = directory.parent().map_or_else(
        || PathBuf::from(format!("{}.json", descriptor.name)),
        |parent| parent.join(format!("{}.json", descriptor.name)),
    );
    let Ok(text) = fs::read_to_string(&legacy_path) else {
        // A missing legacy file is the ordinary fresh-unit case, and an unreadable one is not this
        // unit's to interpret. Either way the empty tree stands.
        return Ok(());
    };
    let Some(document) = serde_json::from_str::<Value>(&text).ok() else {
        // Malformed: not ours to interpret or delete.
        return Ok(());
    };
    let Some(document) = document.as_object() else {
        return Ok(());
    };
    // The legacy document is runtime data, so only its header and table shape are checked here. The
    // record values are migrated as-is; the domain layer's schemas judge them.
    let Some(header) = document.get("unit").and_then(Value::as_object) else {
        return Ok(());
    };
    if header.get("name").and_then(Value::as_str) != Some(descriptor.name.as_str()) {
        return Ok(());
    }
    let Some(stamped) = header.get("version").and_then(Value::as_i64) else {
        return Ok(());
    };
    let Ok(stamped) = u32::try_from(stamped) else {
        return Ok(());
    };
    if !accepted.contains(&stamped) {
        // Migrating records the owner never vouched for would stamp them with the current version and
        // turn a discardable stale cache into schema failures at the domain layer.
        return Ok(());
    }
    let Some(tables) = document.get("tables").and_then(Value::as_object) else {
        return Ok(());
    };

    for (table, records) in tables {
        let Some(target) = state.tables.get_mut(table) else {
            // An undeclared table in the legacy document has nowhere to go.
            continue;
        };
        let Some(records) = records.as_object() else {
            continue;
        };
        for (key, value) in records {
            // Upstream writes `${key}.json` here without a safety check. A legacy key that is not
            // path-safe would become a path outside the table directory, so it is refused: this is a
            // deliberate narrowing, and it costs only a record whose key could never have been written
            // through this layout anyway.
            if !is_safe_record_key(key) {
                continue;
            }
            let text = serialize_record(descriptor.version, value)?;
            write_document(
                &directory.join(table).join(format!("{key}{RECORD_SUFFIX}")),
                &text,
            )?;
            target.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

/// Encodes one record document: the version stamp plus the value, pretty-printed with a trailing
/// newline.
///
/// Upstream's `serializeRecord` exists to keep the format legible — pretty-printing and a trailing
/// newline are part of it, because a human reading the tree is this backend's reason to exist.
///
/// # Errors
///
/// Returns [`RecordError::Encode`] when the value cannot be serialized. A [`Value`] always can, so this
/// is a structural guard rather than an expected failure.
pub fn serialize_record(version: u32, value: &Value) -> Result<String, RecordError> {
    let document = serde_json::json!({ "version": version, "record": value });
    let text = serde_json::to_string_pretty(&document).map_err(|error| RecordError::Encode {
        reason: error.to_string(),
    })?;
    Ok(format!("{text}\n"))
}

/// Decodes one record document, returning `None` when it is foreign.
///
/// Foreign means: not JSON, not an object, no numeric `version`, a `version` outside `accepted`, or no
/// `record` member at all. A present `record` of `null` is a *value*, and reads as `Some(Value::Null)`.
/// The distinction matters: upstream returns the destructured field and tests it against `undefined`,
/// so an absent member is absent while a null is data.
#[must_use]
pub fn parse_record(text: &str, accepted: &[u32]) -> Option<Value> {
    let document: Value = serde_json::from_str(text).ok()?;
    let document = document.as_object()?;
    let stamped = document.get("version")?.as_i64()?;
    let stamped = u32::try_from(stamped).ok()?;
    if !accepted.contains(&stamped) {
        return None;
    }
    document.get("record").cloned()
}

/// Formats the local-time `YYYYMMDDHHmm` suffix a backup document carries.
///
/// `millis` is an instant on the UTC timeline; `offset_minutes` is the local offset to apply. Returns
/// `None` when the result is not a four-digit year, because a five-digit stamp would sort wrongly
/// against every other backup.
#[must_use]
pub fn format_backup_stamp(millis: i64, offset_minutes: i32) -> Option<String> {
    let local = millis.checked_add(i64::from(offset_minutes).checked_mul(60_000)?)?;
    let days = local.div_euclid(MILLIS_PER_DAY);
    let within_day = local.rem_euclid(MILLIS_PER_DAY);
    let (year, month, day) = civil_from_days(days)?;
    if !(0..=9999).contains(&year) {
        return None;
    }
    let minutes = within_day / 60_000;
    let hour = minutes / 60;
    let minute = minutes % 60;
    Some(format!("{year:04}{month:02}{day:02}{hour:02}{minute:02}"))
}

/// Milliseconds in one day.
const MILLIS_PER_DAY: i64 = 86_400_000;

/// The civil date `days` days after 1970-01-01, as Howard Hinnant's `civil_from_days`.
///
/// A private copy because this module compiles without a crate root. It is the same algorithm as
/// [`crate::time::civil_from_days`], and the test asserts the two agree on a corpus of instants.
fn civil_from_days(days: i64) -> Option<(i32, u32, u32)> {
    let shifted = days.checked_add(719_468)?;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    }
    .div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    Some((
        i32::try_from(year).ok()?,
        u32::try_from(month).ok()?,
        u32::try_from(day).ok()?,
    ))
}

/// Durably replaces one document: staging sibling, fsync, rename, fsync the directory.
///
/// The protocol is upstream's `writeAtomic`: the staging file is created exclusively with `0600`, its
/// bytes are synced, then it is renamed over the target, and finally the parent directory is synced so
/// the new entry is crash-durable on POSIX. A failure removes the staging file, so a rejected write
/// leaves no debris.
fn write_document(path: &Path, text: &str) -> Result<(), RecordError> {
    let parent = path.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(&parent).map_err(|error| RecordError::io(&parent, error))?;
    }
    let staging = staging_path(path);

    let written = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&staging)
            .map_err(|error| RecordError::io(&staging, error))?;
        file.write_all(text.as_bytes())
            .map_err(|error| RecordError::io(&staging, error))?;
        // Sync before the rename: without it a crash can leave the *new* name pointing at content the
        // filesystem had not written, which is worse than the old content.
        file.sync_all()
            .map_err(|error| RecordError::io(&staging, error))?;
        drop(file);
        fs::rename(&staging, path).map_err(|error| RecordError::io(path, error))
    })();

    if written.is_err() {
        let _ = fs::remove_file(&staging);
        return written;
    }
    if !parent.as_os_str().is_empty() {
        sync_directory(&parent).map_err(|error| RecordError::io(&parent, error))?;
    }
    Ok(())
}

/// A staging sibling beside `path`, unique within this process.
fn staging_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = path.file_name().map_or_else(
        || "record".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(".{name}.tmp-{pid}-{n}"))
}

/// Persists directory-entry changes on POSIX.
///
/// Off Unix the operation does not exist in this form, so it is a no-op — the same concession upstream
/// makes on Windows.
#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}
