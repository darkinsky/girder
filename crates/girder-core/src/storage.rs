//! The storage hub: versioned key-value units over a pluggable backend.
//!
//! Upstream's `storage` is 2.6k lines defining a hub plus backends that own a medium. The vocabulary
//! it settles on is the part worth carrying: a **unit** is a named container with a declared shape;
//! a **table** is a named collection of records inside it; a **global** is one singleton slot. Values
//! are opaque JSON — this layer has no schema and no domain meaning.
//!
//! # Versioning is the load-bearing part
//!
//! A unit declares a format version. A backend that finds a stored version outside the declared set
//! must refuse rather than read it: an older document read under a newer schema silently produces
//! wrong data, and that is worse than a failure. [`UnitDescriptor::accepts_version`] is where that
//! decision lives.
//!
//! # Layout
//!
//! Two layouts, and the difference is real:
//!
//! - [`Layout::Single`] keeps the whole unit in one document. A write rewrites everything. Simple,
//!   atomic, and fine for small units.
//! - [`Layout::PerRecord`] keeps each record in its own document, so a write touches one file. A
//!   unit whose records are large or sparse stops rewriting the rest on every change.
//!
//! The per-record layout constrains keys to `[A-Za-z0-9_-]+` because a key becomes a path segment.
//! An unsafe key is refused rather than escaped, because two spellings of one key would be a bug
//! waiting to happen.
//!
//! # What this module does not do
//!
//! It does not serialize concurrent writers to one unit. Upstream says the same in its contract:
//! write ordering is the caller's responsibility, and the domain layer runs one write chain per
//! unit. Adding a lock here would hide that requirement rather than satisfy it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::scope::BoxFut;

/// The allowed shape of a unit or table name.
///
/// Lowercase, starting with a letter, so a name is safe as both a file name and a SQL identifier
/// segment without escaping. Two spellings of one name — `Foo` and `foo` — would be one unit on a
/// case-insensitive filesystem and two on a case-sensitive one, which is exactly the kind of
/// ambiguity a name rule prevents.
#[must_use]
pub fn is_valid_unit_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The allowed shape of a record key under [`Layout::PerRecord`].
#[must_use]
pub fn is_valid_record_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// How a unit stores its records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    /// The whole unit is one document.
    #[default]
    Single,
    /// Each record is its own document.
    PerRecord,
}

/// The static identity and shape of one unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitDescriptor {
    /// Unit name; also the file-name segment.
    pub name: String,
    /// Current format version, stamped on materialization and every mutation.
    pub version: u32,
    /// Table names.
    pub tables: Vec<String>,
    /// Whether the unit carries a global singleton.
    pub has_global: bool,
    /// Document layout.
    pub layout: Layout,
    /// Older versions whose records are also readable under current schemas.
    ///
    /// The owner vouches for this, typically by declaring fields old records lack as optional.
    pub compatible_versions: Vec<u32>,
}

impl UnitDescriptor {
    /// Builds a single-document descriptor.
    #[must_use]
    pub fn new(name: impl Into<String>, version: u32, tables: Vec<String>) -> Self {
        Self {
            name: name.into(),
            version,
            tables,
            has_global: false,
            layout: Layout::Single,
            compatible_versions: Vec::new(),
        }
    }

    /// Declares a global singleton slot.
    #[must_use]
    pub fn with_global(mut self) -> Self {
        self.has_global = true;
        self
    }

    /// Uses the per-record layout.
    #[must_use]
    pub fn per_record(mut self) -> Self {
        self.layout = Layout::PerRecord;
        self
    }

    /// Declares an older version as readable.
    #[must_use]
    pub fn compatible_with(mut self, version: u32) -> Self {
        self.compatible_versions.push(version);
        self
    }

    /// Whether a stored version is readable under this descriptor.
    ///
    /// A stored version above the declared one is never readable: it was written by a newer build
    /// whose records this one cannot interpret.
    #[must_use]
    pub fn accepts_version(&self, stored: u32) -> bool {
        stored == self.version || self.compatible_versions.contains(&stored)
    }

    fn validate(&self) -> Result<(), StorageError> {
        if !is_valid_unit_name(&self.name) {
            return Err(StorageError::InvalidName {
                name: self.name.clone(),
                what: "unit",
            });
        }
        for table in &self.tables {
            if !is_valid_unit_name(table) {
                return Err(StorageError::InvalidName {
                    name: table.clone(),
                    what: "table",
                });
            }
        }
        Ok(())
    }
}

/// A failure the storage layer reports.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// A unit or table name broke the shape rule.
    #[error("invalid {what} name `{name}`: must match `^[a-z][a-z0-9_]*$`")]
    InvalidName {
        /// The offending name.
        name: String,
        /// Whether it was a unit or a table.
        what: &'static str,
    },

    /// A record key broke the shape rule of the per-record layout.
    #[error("invalid record key `{key}`: must match `^[A-Za-z0-9_-]+$` in the per-record layout")]
    InvalidKey {
        /// The offending key.
        key: String,
    },

    /// The stored version is not one this descriptor reads.
    #[error(
        "unit `{name}` holds version {stored}, which this build does not read (declared {declared})"
    )]
    VersionMismatch {
        /// The unit.
        name: String,
        /// What the medium holds.
        stored: u32,
        /// What this build declares.
        declared: u32,
    },

    /// The medium holds something that is not a readable unit.
    #[error("unit `{name}` is not parseable: {reason}")]
    Malformed {
        /// The unit.
        name: String,
        /// What was wrong.
        reason: String,
    },

    /// The unit was closed.
    #[error("unit `{name}` is closed")]
    Closed {
        /// The unit.
        name: String,
    },

    /// A table was not declared.
    #[error("unit `{name}` declares no table `{table}`")]
    UnknownTable {
        /// The unit.
        name: String,
        /// The table.
        table: String,
    },

    /// Reading or writing the medium failed.
    #[error("storage io on `{path}`: {source}")]
    Io {
        /// The path.
        path: String,
        /// The cause.
        source: std::io::Error,
    },
}

impl StorageError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

/// One table's records: key to opaque value.
pub type Records = BTreeMap<String, Value>;

/// A full snapshot of a unit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Records by table name.
    pub tables: BTreeMap<String, Records>,
    /// The global singleton, when the unit declares one and it was written.
    pub global: Option<Value>,
}

/// An opened unit.
pub struct Unit {
    descriptor: UnitDescriptor,
    backend: Arc<dyn KvBackend>,
    closed: bool,
}

impl std::fmt::Debug for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unit")
            .field("name", &self.descriptor.name)
            .field("version", &self.descriptor.version)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

use std::sync::Arc;

impl Unit {
    /// The descriptor this unit was opened with.
    #[must_use]
    pub fn descriptor(&self) -> &UnitDescriptor {
        &self.descriptor
    }

    /// Reads the full current snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Closed`] after `close`, and whatever the backend reports.
    pub async fn load_all(&self, cancel: CancellationToken) -> Result<Snapshot, StorageError> {
        if self.closed {
            return Err(StorageError::Closed {
                name: self.descriptor.name.clone(),
            });
        }
        self.backend.load(&self.descriptor, cancel).await
    }

    /// Upserts one record durably.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::UnknownTable`] for an undeclared table,
    /// [`StorageError::InvalidKey`] for an unsafe key under the per-record layout, and
    /// [`StorageError::Closed`] after `close`.
    pub async fn put_record(
        &self,
        table: &str,
        key: &str,
        value: Value,
        cancel: CancellationToken,
    ) -> Result<(), StorageError> {
        self.assert_open()?;
        self.assert_table(table)?;
        if self.descriptor.layout == Layout::PerRecord && !is_valid_record_key(key) {
            return Err(StorageError::InvalidKey {
                key: key.to_owned(),
            });
        }
        self.backend
            .put(&self.descriptor, table, key, value, cancel)
            .await
    }

    /// Deletes one record durably.
    ///
    /// Idempotent: a missing key is a no-op.
    ///
    /// # Errors
    ///
    /// Same as [`Unit::put_record`].
    pub async fn delete_record(
        &self,
        table: &str,
        key: &str,
        cancel: CancellationToken,
    ) -> Result<(), StorageError> {
        self.assert_open()?;
        self.assert_table(table)?;
        self.backend
            .delete(&self.descriptor, table, key, cancel)
            .await
    }

    /// Writes the global singleton durably.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::UnknownTable`] when the descriptor did not declare a global.
    pub async fn set_global(
        &self,
        value: Value,
        cancel: CancellationToken,
    ) -> Result<(), StorageError> {
        self.assert_open()?;
        if !self.descriptor.has_global {
            return Err(StorageError::UnknownTable {
                name: self.descriptor.name.clone(),
                table: "global".to_owned(),
            });
        }
        self.backend
            .set_global(&self.descriptor, value, cancel)
            .await
    }

    /// Releases the unit. Idempotent.
    pub fn close(&mut self) {
        self.closed = true;
    }

    fn assert_open(&self) -> Result<(), StorageError> {
        if self.closed {
            return Err(StorageError::Closed {
                name: self.descriptor.name.clone(),
            });
        }
        Ok(())
    }

    fn assert_table(&self, table: &str) -> Result<(), StorageError> {
        if self.descriptor.tables.iter().any(|known| known == table) {
            Ok(())
        } else {
            Err(StorageError::UnknownTable {
                name: self.descriptor.name.clone(),
                table: table.to_owned(),
            })
        }
    }
}

/// A medium that can hold units.
///
/// The hub's backend contract. A backend owns one medium and serves every unit opened on it.
pub trait KvBackend: Send + Sync {
    /// Opens a unit, creating it when the medium holds no trace of it.
    fn open(
        &self,
        descriptor: UnitDescriptor,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Unit, StorageError>>;

    /// Reads a full snapshot.
    fn load(
        &self,
        descriptor: &UnitDescriptor,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Snapshot, StorageError>>;

    /// Upserts one record.
    fn put(
        &self,
        descriptor: &UnitDescriptor,
        table: &str,
        key: &str,
        value: Value,
        cancel: CancellationToken,
    ) -> BoxFut<Result<(), StorageError>>;

    /// Deletes one record.
    fn delete(
        &self,
        descriptor: &UnitDescriptor,
        table: &str,
        key: &str,
        cancel: CancellationToken,
    ) -> BoxFut<Result<(), StorageError>>;

    /// Writes the global singleton.
    fn set_global(
        &self,
        descriptor: &UnitDescriptor,
        value: Value,
        cancel: CancellationToken,
    ) -> BoxFut<Result<(), StorageError>>;
}

/// What a `Single` layout writes to disk.
#[derive(Debug, Deserialize, Serialize)]
struct StoredUnit {
    version: u32,
    #[serde(default)]
    tables: BTreeMap<String, Records>,
    #[serde(default)]
    global: Option<Value>,
}

/// A file-tree backend, one document per unit.
///
/// The `Single` layout, implemented for real. The `PerRecord` layout is declared in the vocabulary
/// but not implemented here: it needs one document per record plus a directory fsync, and shipping a
/// half-version that silently behaves like `Single` would be worse than declaring the gap.
pub struct FileBackend {
    root: PathBuf,
}

impl FileBackend {
    /// Creates a backend rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_of(&self, descriptor: &UnitDescriptor) -> PathBuf {
        self.root.join(format!("{}.json", descriptor.name))
    }

    /// Reads the stored unit, or `None` when the medium has no trace of it.
    fn read_stored(&self, descriptor: &UnitDescriptor) -> Result<Option<StoredUnit>, StorageError> {
        let path = self.path_of(descriptor);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(StorageError::io(&path, error)),
        };
        let stored: StoredUnit =
            serde_json::from_str(&text).map_err(|error| StorageError::Malformed {
                name: descriptor.name.clone(),
                reason: error.to_string(),
            })?;
        // The version gate: an older document read under a newer schema silently produces wrong
        // data, which is worse than a failure.
        if !descriptor.accepts_version(stored.version) {
            return Err(StorageError::VersionMismatch {
                name: descriptor.name.clone(),
                stored: stored.version,
                declared: descriptor.version,
            });
        }
        Ok(Some(stored))
    }

    fn write_stored(
        &self,
        descriptor: &UnitDescriptor,
        stored: &StoredUnit,
    ) -> Result<(), StorageError> {
        let path = self.path_of(descriptor);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| StorageError::io(parent, error))?;
        }
        let text =
            serde_json::to_string_pretty(stored).map_err(|error| StorageError::Malformed {
                name: descriptor.name.clone(),
                reason: error.to_string(),
            })?;

        // Write to a unique staging file and rename: a crash mid-write leaves the previous document
        // intact rather than a truncated one. The rename is atomic on POSIX within one filesystem.
        let staging = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&staging, text.as_bytes())
            .map_err(|error| StorageError::io(&staging, error))?;
        std::fs::rename(&staging, &path).map_err(|error| StorageError::io(&path, error))?;
        Ok(())
    }
}

impl KvBackend for FileBackend {
    fn open(
        &self,
        descriptor: UnitDescriptor,
        _cancel: CancellationToken,
    ) -> BoxFut<Result<Unit, StorageError>> {
        let backend: Arc<dyn KvBackend> = Arc::new(Self {
            root: self.root.clone(),
        });
        let reader = Self {
            root: self.root.clone(),
        };
        Box::pin(async move {
            descriptor.validate()?;
            // Reading at open materializes the version check now rather than on the first write, so
            // a mismatched unit fails where the caller can attribute it.
            let _ = reader.read_stored(&descriptor)?;
            Ok(Unit {
                descriptor,
                backend,
                closed: false,
            })
        })
    }

    fn load(
        &self,
        descriptor: &UnitDescriptor,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Snapshot, StorageError>> {
        let backend = Self {
            root: self.root.clone(),
        };
        let descriptor = descriptor.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(StorageError::Closed {
                    name: descriptor.name.clone(),
                });
            }
            Ok(backend
                .read_stored(&descriptor)?
                .map_or_else(Snapshot::default, |stored| Snapshot {
                    tables: stored.tables,
                    global: stored.global,
                }))
        })
    }

    fn put(
        &self,
        descriptor: &UnitDescriptor,
        table: &str,
        key: &str,
        value: Value,
        _cancel: CancellationToken,
    ) -> BoxFut<Result<(), StorageError>> {
        let backend = Self {
            root: self.root.clone(),
        };
        let descriptor = descriptor.clone();
        let table = table.to_owned();
        let key = key.to_owned();
        Box::pin(async move {
            let mut stored = backend.read_stored(&descriptor)?.unwrap_or(StoredUnit {
                version: descriptor.version,
                tables: BTreeMap::new(),
                global: None,
            });
            // A mutation stamps the current version, so a record written by this build is never
            // mistaken for one written by an older one.
            stored.version = descriptor.version;
            stored.tables.entry(table).or_default().insert(key, value);
            backend.write_stored(&descriptor, &stored)
        })
    }

    fn delete(
        &self,
        descriptor: &UnitDescriptor,
        table: &str,
        key: &str,
        _cancel: CancellationToken,
    ) -> BoxFut<Result<(), StorageError>> {
        let backend = Self {
            root: self.root.clone(),
        };
        let descriptor = descriptor.clone();
        let table = table.to_owned();
        let key = key.to_owned();
        Box::pin(async move {
            let Some(mut stored) = backend.read_stored(&descriptor)? else {
                // Idempotent: deleting from a unit that does not exist is a no-op, not an error.
                return Ok(());
            };
            let removed = stored
                .tables
                .get_mut(&table)
                .and_then(|records| records.remove(&key))
                .is_some();
            if !removed {
                // A missing key does not advance the version, matching upstream's contract.
                return Ok(());
            }
            stored.version = descriptor.version;
            backend.write_stored(&descriptor, &stored)
        })
    }

    fn set_global(
        &self,
        descriptor: &UnitDescriptor,
        value: Value,
        _cancel: CancellationToken,
    ) -> BoxFut<Result<(), StorageError>> {
        let backend = Self {
            root: self.root.clone(),
        };
        let descriptor = descriptor.clone();
        Box::pin(async move {
            let mut stored = backend.read_stored(&descriptor)?.unwrap_or(StoredUnit {
                version: descriptor.version,
                tables: BTreeMap::new(),
                global: None,
            });
            stored.version = descriptor.version;
            stored.global = Some(value);
            backend.write_stored(&descriptor, &stored)
        })
    }
}
