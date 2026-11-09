//! The filesystem seam: targets, metadata, and version-guarded writes.
//!
//! Upstream's `fs` is 7.3k lines of abstract `FileSystem` plus a local backend, because it also
//! backs remote and sandboxed filesystems. The abstraction is worth keeping; the 7.3k is mostly one
//! backend's edge cases.
//!
//! # Why a target is not a path
//!
//! A [`FsTarget`] carries an opaque key and a display string. The key is what a stale guard compares;
//! the display string is what a model or a user sees. Keeping them apart lets a remote backend use a
//! URI for display while the key stays a stable identity — and lets the local backend use the
//! canonical path for both without the interface assuming it.
//!
//! # Why writes carry a version
//!
//! A read-modify-write that does not check freshness silently overwrites a change made in between.
//! Upstream refuses the write instead, and the model is told to re-read. [`FsVersion`] is the token
//! that makes this possible: it comes from `stat` or from a previous write, and the writer either
//! matches it or is refused.
//!
//! # What this module does not do
//!
//! No watching, no symlink policy, no workspace rooting. Those belong with the sandbox layer
//! (`exec.rs` owns the path policy) and with M3's tool layer, which is where a model-supplied path
//! first appears.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::scope::BoxFut;

/// A freshness token for one file state.
///
/// Opaque to consumers: a writer receives one from [`FileSystem::stat`] and hands it back. The local
/// backend derives it from modification time and size, which is cheap to compute and catches every
/// change that matters — a same-mtime, same-size edit within one filesystem tick is the only miss,
/// and upstream's local backend has the same one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FsVersion(pub String);

impl std::fmt::Display for FsVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An opaque identity for a filesystem target.
///
/// Separate from the display path so a backend whose display form is a URI still has a stable key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FsTargetKey(pub String);

/// What a caller points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsTarget {
    /// Stable identity, used for stale guards.
    pub key: FsTargetKey,
    /// What a model or user sees: an absolute path, a workspace-relative path, or a URI.
    pub display_path: String,
}

impl FsTarget {
    /// Builds a target whose key and display form are both the path.
    ///
    /// The local backend's shape. A remote backend would use its own pairing.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        let display = path.display().to_string();
        Self {
            key: FsTargetKey(display.clone()),
            display_path: display,
        }
    }

    /// The path this target names, when the backend is local.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.display_path)
    }
}

/// What kind of thing a target is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsType {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A device, socket, pipe, or symlink — anything a tool must not read as text.
    Other,
}

/// One authoritative observation of a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsInfo {
    /// Freshness token right now.
    pub version: FsVersion,
    /// What it is.
    pub file_type: FsType,
    /// Size in bytes.
    pub size: u64,
}

/// One directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsDirEntry {
    /// Its name, not a path.
    pub name: String,
    /// What it is.
    pub file_type: FsType,
}

/// A failure the filesystem reports.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// The target does not exist.
    #[error("`{path}` does not exist")]
    Missing {
        /// The path that was looked for.
        path: String,
    },

    /// The target exists but the caller expected it not to.
    #[error("`{path}` already exists")]
    Exists {
        /// The path.
        path: String,
    },

    /// The target changed since the caller read it.
    ///
    /// The error a model must see: it means someone else edited the file, and the correct response
    /// is to re-read rather than to force the write.
    #[error("`{path}` changed since it was read; re-read it before writing")]
    Stale {
        /// The path.
        path: String,
        /// The version the caller held.
        expected: FsVersion,
        /// The version on disk.
        actual: FsVersion,
    },

    /// The caller asked for a file and got a directory, or the reverse.
    #[error("`{path}` is a {actual:?}, not a {expected:?}")]
    WrongType {
        /// The path.
        path: String,
        /// What the caller wanted.
        expected: FsType,
        /// What is there.
        actual: FsType,
    },

    /// The target is not valid text.
    #[error("`{path}` is not valid UTF-8")]
    NotText {
        /// The path.
        path: String,
    },

    /// The target exceeds the caller's byte budget.
    #[error("`{path}` is {size} bytes, above the {limit}-byte limit")]
    TooLarge {
        /// The path.
        path: String,
        /// Its size.
        size: u64,
        /// The caller's budget.
        limit: u64,
    },

    /// An underlying I/O failure.
    #[error("io on `{path}`: {source}")]
    Io {
        /// The path.
        path: String,
        /// The cause.
        source: std::io::Error,
    },

    /// The caller cancelled.
    #[error("cancelled")]
    Cancelled,
}

impl FsError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        if source.kind() == std::io::ErrorKind::NotFound {
            return Self::Missing {
                path: path.display().to_string(),
            };
        }
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

/// The filesystem contract.
///
/// Async and `Send + Sync` because a tool call runs on the loop's task; a synchronous backend would
/// block the executor while a slow disk answers.
pub trait FileSystem: Send + Sync {
    /// Resolves a user-supplied path against an optional working directory.
    ///
    /// Separate from the read methods so a policy layer can inspect the target before anything
    /// touches the disk — which is what lets a sandbox refuse a path before the first syscall.
    fn resolve(&self, path: &str, cwd: Option<&Path>) -> FsTarget;

    /// Observes a target, or `None` when it is absent.
    ///
    /// Absence is `Ok(None)` rather than an error because it is the normal answer to "does this
    /// exist", not a failure.
    fn stat(
        &self,
        target: &FsTarget,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Option<FsInfo>, FsError>>;

    /// Reads a target as text.
    fn read_text(
        &self,
        target: &FsTarget,
        cancel: CancellationToken,
    ) -> BoxFut<Result<String, FsError>>;

    /// Reads at most `limit` bytes, refusing a larger target by its reported size.
    ///
    /// A separate method rather than a parameter on `read_text` because the refusal must happen
    /// before the read: reading a 10 GB file to discover it is too large defeats the limit.
    fn read_bytes(
        &self,
        target: &FsTarget,
        limit: u64,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Vec<u8>, FsError>>;

    /// Lists a directory.
    fn list_dir(
        &self,
        target: &FsTarget,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Vec<FsDirEntry>, FsError>>;

    /// Writes text, guarded by an expected version.
    ///
    /// `expected` is `None` for a create: the write fails if the target already exists. Pass a
    /// version from [`FileSystem::stat`] to overwrite only what was read.
    fn write_text(
        &self,
        target: &FsTarget,
        contents: &str,
        expected: Option<&FsVersion>,
        cancel: CancellationToken,
    ) -> BoxFut<Result<FsVersion, FsError>>;

    /// Removes a file, guarded by an expected version.
    fn remove(
        &self,
        target: &FsTarget,
        expected: &FsVersion,
        cancel: CancellationToken,
    ) -> BoxFut<Result<(), FsError>>;
}

/// The local filesystem: `tokio::fs` over whatever the process can reach.
///
/// No rooting of its own. A caller that must confine a path uses [`crate::join_within`] or the
/// path policy in `exec`, which is where the decision belongs — a backend that silently rooted would
/// make the sandbox's own check unobservable.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFileSystem;

impl LocalFileSystem {
    /// Derives a version from a metadata snapshot.
    fn version_of(metadata: &std::fs::Metadata) -> FsVersion {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_nanos());
        FsVersion(format!("{}-{}", modified, metadata.len()))
    }

    fn type_of(metadata: &std::fs::Metadata) -> FsType {
        if metadata.is_file() {
            FsType::File
        } else if metadata.is_dir() {
            FsType::Directory
        } else {
            FsType::Other
        }
    }
}

impl FileSystem for LocalFileSystem {
    fn resolve(&self, path: &str, cwd: Option<&Path>) -> FsTarget {
        // `~` expands against the process home; a relative path joins the working directory. The
        // result is lexical, so a path that does not exist still resolves — which `resolve` must
        // allow, since creating a file is the common case.
        let expanded = crate::paths::expand_home(path);
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            cwd.map_or_else(|| expanded.clone(), |dir| dir.join(&expanded))
        };
        FsTarget::from_path(&joined)
    }

    fn stat(
        &self,
        target: &FsTarget,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Option<FsInfo>, FsError>> {
        let path = target.as_path().to_path_buf();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(FsError::Cancelled);
            }
            match tokio::fs::metadata(&path).await {
                Ok(metadata) => Ok(Some(FsInfo {
                    version: Self::version_of(&metadata),
                    file_type: Self::type_of(&metadata),
                    size: metadata.len(),
                })),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(FsError::io(&path, error)),
            }
        })
    }

    fn read_text(
        &self,
        target: &FsTarget,
        cancel: CancellationToken,
    ) -> BoxFut<Result<String, FsError>> {
        let path = target.as_path().to_path_buf();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(FsError::Cancelled);
            }
            // Type is checked first so a directory is reported as a wrong type rather than as an I/O
            // error whose message depends on the platform.
            if let Ok(metadata) = tokio::fs::metadata(&path).await {
                let kind = Self::type_of(&metadata);
                if kind != FsType::File {
                    return Err(FsError::WrongType {
                        path: path.display().to_string(),
                        expected: FsType::File,
                        actual: kind,
                    });
                }
            }
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|error| FsError::io(&path, error))?;
            String::from_utf8(bytes).map_err(|_| FsError::NotText {
                path: path.display().to_string(),
            })
        })
    }

    fn read_bytes(
        &self,
        target: &FsTarget,
        limit: u64,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Vec<u8>, FsError>> {
        let path = target.as_path().to_path_buf();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(FsError::Cancelled);
            }
            // Refuse by reported size before reading: reading a huge file to find it is too large
            // defeats the limit.
            let metadata = tokio::fs::metadata(&path)
                .await
                .map_err(|error| FsError::io(&path, error))?;
            if metadata.len() > limit {
                return Err(FsError::TooLarge {
                    path: path.display().to_string(),
                    size: metadata.len(),
                    limit,
                });
            }
            tokio::fs::read(&path)
                .await
                .map_err(|error| FsError::io(&path, error))
        })
    }

    fn list_dir(
        &self,
        target: &FsTarget,
        cancel: CancellationToken,
    ) -> BoxFut<Result<Vec<FsDirEntry>, FsError>> {
        let path = target.as_path().to_path_buf();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(FsError::Cancelled);
            }
            if let Ok(metadata) = tokio::fs::metadata(&path).await {
                let kind = Self::type_of(&metadata);
                if kind != FsType::Directory {
                    return Err(FsError::WrongType {
                        path: path.display().to_string(),
                        expected: FsType::Directory,
                        actual: kind,
                    });
                }
            }

            let mut entries = Vec::new();
            let mut reader = tokio::fs::read_dir(&path)
                .await
                .map_err(|error| FsError::io(&path, error))?;
            while let Some(entry) = reader
                .next_entry()
                .await
                .map_err(|error| FsError::io(&path, error))?
            {
                let file_type = entry
                    .file_type()
                    .await
                    .map_err(|error| FsError::io(&path, error))?;
                let kind = if file_type.is_file() {
                    FsType::File
                } else if file_type.is_dir() {
                    FsType::Directory
                } else {
                    FsType::Other
                };
                entries.push(FsDirEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    file_type: kind,
                });
            }
            // Sorted so two listings of the same directory agree: a directory's own order is
            // filesystem-dependent, and an unstable order makes output untestable and diffs noisy.
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        })
    }

    fn write_text(
        &self,
        target: &FsTarget,
        contents: &str,
        expected: Option<&FsVersion>,
        cancel: CancellationToken,
    ) -> BoxFut<Result<FsVersion, FsError>> {
        let path = target.as_path().to_path_buf();
        let contents = contents.to_owned();
        let expected = expected.cloned();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(FsError::Cancelled);
            }

            // The version check and the write are one step: checking first and writing after leaves
            // a window in which another writer lands.
            //
            // The guard must not be alive across an `await` -- a `std` mutex guard is not `Send`, so
            // holding it would make this future un-spawnable. The critical section is therefore
            // driven through a `tokio` mutex keyed by path, which is `Send`.
            let lock = crate::session::async_path_lock(&path);
            let _guard = lock.lock().await;

            let current = match tokio::fs::metadata(&path).await {
                Ok(metadata) => Some(Self::version_of(&metadata)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(FsError::io(&path, error)),
            };

            match (&expected, &current) {
                // Create: refuse when the target exists.
                (None, Some(_)) => {
                    return Err(FsError::Exists {
                        path: path.display().to_string(),
                    });
                }
                // Overwrite: refuse unless the caller read this exact version.
                (Some(expected), Some(actual)) if expected != actual => {
                    return Err(FsError::Stale {
                        path: path.display().to_string(),
                        expected: expected.clone(),
                        actual: actual.clone(),
                    });
                }
                // Overwrite: the target vanished since it was read.
                (Some(_), None) => {
                    return Err(FsError::Missing {
                        path: path.display().to_string(),
                    });
                }
                _ => {}
            }

            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|error| FsError::io(parent, error))?;
                }
            }
            tokio::fs::write(&path, contents.as_bytes())
                .await
                .map_err(|error| FsError::io(&path, error))?;

            let metadata = tokio::fs::metadata(&path)
                .await
                .map_err(|error| FsError::io(&path, error))?;
            Ok(Self::version_of(&metadata))
        })
    }

    fn remove(
        &self,
        target: &FsTarget,
        expected: &FsVersion,
        cancel: CancellationToken,
    ) -> BoxFut<Result<(), FsError>> {
        let path = target.as_path().to_path_buf();
        let expected = expected.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(FsError::Cancelled);
            }

            let lock = crate::session::async_path_lock(&path);
            let _guard = lock.lock().await;

            let metadata = tokio::fs::metadata(&path)
                .await
                .map_err(|error| FsError::io(&path, error))?;
            let actual = Self::version_of(&metadata);
            if actual != expected {
                return Err(FsError::Stale {
                    path: path.display().to_string(),
                    expected,
                    actual,
                });
            }
            tokio::fs::remove_file(&path)
                .await
                .map_err(|error| FsError::io(&path, error))
        })
    }
}

/// Reads a file the way a tool does: resolve, stat, refuse a directory, read.
///
/// A free function rather than a caller-side sequence because the three steps are always together
/// and the ordering is the part that is easy to get wrong.
///
/// # Errors
///
/// Returns [`FsError::Missing`] for an absent target, [`FsError::WrongType`] for a directory, and
/// whatever the underlying read reports.
pub async fn read_file(
    fs: &dyn FileSystem,
    path: &str,
    cwd: Option<&Path>,
    cancel: CancellationToken,
) -> Result<(FsTarget, String), FsError> {
    let target = fs.resolve(path, cwd);
    match fs.stat(&target, cancel.clone()).await? {
        None => Err(FsError::Missing {
            path: target.display_path.clone(),
        }),
        Some(info) if info.file_type != FsType::File => Err(FsError::WrongType {
            path: target.display_path.clone(),
            expected: FsType::File,
            actual: info.file_type,
        }),
        Some(_) => {
            let text = fs.read_text(&target, cancel).await?;
            Ok((target, text))
        }
    }
}

/// A target built from a path that the caller already resolved.
#[must_use]
pub fn target_at(path: impl Into<PathBuf>) -> FsTarget {
    FsTarget::from_path(&path.into())
}
