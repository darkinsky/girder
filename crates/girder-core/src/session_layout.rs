//! Where a session's files live on disk.
//!
//! A session is not one file. The log carries the conversation, but a rename and an archive mark are
//! states about a session, not events inside it — appending them would make every history read carry
//! records the model never saw, and a replay would see a title change as a turn. So each of those
//! states lives beside the log in a companion file, and every operation that copies, lists, or
//! deletes a session has to know the whole set. This module is that knowledge in one place: one
//! function builds the paths, one lists the ids, one deletes everything.
//!
//! # This project's layout, and how it differs from upstream
//!
//! Upstream (`packages/session/session-persistence-jsonl`) gives a session a *directory*:
//! `root / projectDir(cwd) / encodeSegment(id) / session.vN.jsonl.zstd`. Upstream therefore never has
//! to worry about an id colliding with another session's files, but it pays for that with a project
//! key, a path-encoding step, and a generation number in the filename.
//!
//! This project is flat: `root / {id}.jsonl.zst`, with `{id}.title` and `{id}.archived` alongside in
//! the same directory. Three differences follow, and each one is a decision rather than an
//! oversight:
//!
//! - **Flat, not per-session directories.** [`DirectoryCatalog`](crate::DirectoryCatalog) and the
//!   search tool both walk one directory, so a listing is one `read_dir` and not a walk. The price
//!   is that the id must be safe as a *file name*, which is why [`Layout::files`] validates it.
//! - **`zst`, not `zstd`.** The suffix matches what this project's reader and writer already use;
//!   changing it would orphan every session already on disk.
//! - **No generation component.** Format generations are recognised from the log's own rows (see
//!   `session_migration`), so a log keeps one name for its whole life and an old log stays where its
//!   id says it is.
//!
//! # Why a missing sidecar is not an error, and a missing log is not a session
//!
//! `/home/me/.girder/sessions` for a fresh install is empty or absent, and a listing that refused to
//! answer until a directory existed would make an empty session list look like a broken install.
//! For that reason [`Layout::ids`] treats an absent directory as "no sessions". The converse is the
//! sharper rule: an id is a session only if its *log* exists. A `.title` file left behind by an
//! interrupted delete, or written by an older build, names a conversation that is gone, and listing
//! it would put a row in the UI that cannot be opened.
//!
//! # The id check is a security boundary
//!
//! An id arrives from a client — a URL path segment, a JSON argument — and is concatenated into a
//! path. `../../.ssh/authorized_keys` would therefore address a file outside the session directory,
//! and every read, rename, and delete would operate on it. [`Layout::files`] is the single gate that
//! every path in this module goes through, and it refuses separators, `.`/`..`, control characters,
//! and over-long ids before any path is built. The check is repeated on `remove`, because a delete is
//! the operation where a wrong path is unrecoverable.

use std::path::PathBuf;

/// Suffix of a session's event log.
///
/// Public because a caller that has to recognise a session file — a directory scan, a migration —
/// must agree with this module rather than respell the suffix and drift.
pub const LOG_SUFFIX: &str = ".jsonl.zst";

/// Suffix of a session's title companion file.
pub const TITLE_SUFFIX: &str = ".title";

/// Suffix of a session's archive-mark companion file.
pub const ARCHIVED_SUFFIX: &str = ".archived";

/// The longest id a file name can hold, less the longest suffix.
///
/// Most filesystems cap one component at 255 bytes. Checking it here turns a confusing `ENAMETOOLONG`
/// from the middle of a write into a refusal that names the id, and it also bounds what a caller can
/// make this module build.
const MAX_ID_BYTES: usize = 255 - LOG_SUFFIX.len();

/// All the files one session owns on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFiles {
    /// The event log. Its presence is what makes an id a session.
    pub log: PathBuf,
    /// The title, written by a rename. Absent for a session nobody renamed.
    pub title: PathBuf,
    /// The archive mark. Absent for a session nobody archived.
    pub archived: PathBuf,
}

/// Why an id cannot name a session.
///
/// The variants name the offending property rather than echoing "invalid id": the caller is usually
/// a client that sent the id, and a message that says *what* was wrong is what lets it correct the
/// request instead of retrying it unchanged.
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    /// The id was empty.
    #[error("a session id cannot be empty")]
    EmptyId,
    /// The id is `.` or `..`, which name directories rather than a file.
    #[error("session id `{id}` names a directory, not a session")]
    DirectoryName {
        /// The rejected id.
        id: String,
    },
    /// The id contains a path separator, so it would address another directory.
    #[error("session id `{id}` contains a path separator")]
    Separator {
        /// The rejected id.
        id: String,
    },
    /// The id contains a control character, which would end up in a file name or a terminal.
    #[error("session id `{id}` contains a control character")]
    ControlCharacter {
        /// The rejected id.
        id: String,
    },
    /// The id is too long to be a file name once the suffix is appended.
    #[error("session id is {len} bytes; at most {max} fit in a file name")]
    TooLong {
        /// The id's length in bytes.
        len: usize,
        /// The longest id this layout accepts.
        max: usize,
    },
    /// The built path did not stay inside the session directory.
    ///
    /// This is a bug guard rather than an expected outcome: the checks above are meant to make it
    /// unreachable. Reporting it instead of returning a path would mean a path-composition mistake
    /// turns into a read or a delete somewhere else, so the guard stays even though no test can
    /// trigger it.
    #[error("session id `{id}` would address a file outside the session directory")]
    EscapesRoot {
        /// The rejected id.
        id: String,
    },
}

/// The directory a session's files live in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// The layout rooted at `root`.
    ///
    /// The root is not created here: a layout is a way to name paths, and creating directories as a
    /// side effect of constructing one would make a read-only deployment fail before it could answer
    /// a listing.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory this layout names.
    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Every file a session with `id` owns.
    ///
    /// The paths are built whether or not the files exist: a caller that is about to write a title
    /// needs the name of a file that is not there yet.
    ///
    /// # Errors
    ///
    /// Returns a [`LayoutError`] when `id` is not a safe file-name segment — it is empty, contains a
    /// path separator or a control character, is `.` or `..`, or is too long. See the module docs
    /// for why this is a security boundary rather than a convenience check.
    pub fn files(&self, id: &str) -> Result<SessionFiles, LayoutError> {
        validate_id(id)?;
        let files = SessionFiles {
            log: self.root.join(format!("{id}{LOG_SUFFIX}")),
            title: self.root.join(format!("{id}{TITLE_SUFFIX}")),
            archived: self.root.join(format!("{id}{ARCHIVED_SUFFIX}")),
        };
        // Defense in depth. `validate_id` should already have made every path a direct child of the
        // root, and the assertion is what makes that a property of this module instead of a property
        // of the caller's id.
        for path in [&files.log, &files.title, &files.archived] {
            if path.parent() != Some(self.root.as_path()) {
                return Err(LayoutError::EscapesRoot { id: id.to_owned() });
            }
        }
        Ok(files)
    }

    /// Every session id in the directory.
    ///
    /// Ordered, because a caller that paginates or diffs two listings must not depend on the order
    /// the filesystem happened to return entries in.
    ///
    /// # Errors
    ///
    /// Returns the error from reading the directory when it exists but cannot be read (permissions,
    /// for instance). A directory that does not exist is *not* an error: a fresh install has no
    /// sessions, and that is an answer.
    pub fn ids(&self) -> std::io::Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };

        let mut ids = Vec::new();
        // `entry?` rather than `flatten`: an entry that could not be read means the listing is
        // incomplete, and an incomplete listing that claimed to be complete would hide a session.
        for entry in entries {
            let file_name = entry?.file_name();
            // A name that is not UTF-8 cannot have been written by this module, which only ever
            // builds names from a `str` id; skipping it is what keeps `ids` total.
            let Some(name) = file_name.to_str() else {
                continue;
            };
            // The log is the only file that makes a session. Counting `.title`/`.archived` here
            // would list sessions whose conversation is gone — see the module docs.
            let Some(id) = name.strip_suffix(LOG_SUFFIX) else {
                continue;
            };
            // `.jsonl.zst` on its own is a file with no id in it, so it names nothing.
            if id.is_empty() {
                continue;
            }
            ids.push(id.to_owned());
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Deletes every file of the session with `id`.
    ///
    /// Idempotent: a file that is not there is not an error, because deleting a session twice is an
    /// ordinary retry and a half-deleted session (log gone, title left behind) is exactly the state a
    /// second attempt has to be able to finish.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::InvalidInput`] when `id` is not a safe file-name segment: a
    /// delete is the one operation where following a crafted path is unrecoverable, so it refuses
    /// rather than deletes something outside the directory. Other errors are the ones
    /// [`std::fs::remove_file`] reports for a file that exists but could not be removed.
    pub fn remove(&self, id: &str) -> std::io::Result<()> {
        let files = self
            .files(id)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        for path in [files.log, files.title, files.archived] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// How many sessions the directory holds.
    ///
    /// Derived from [`Layout::ids`] rather than from a second scan, so the count and the listing
    /// cannot disagree about what a session is.
    ///
    /// # Errors
    ///
    /// The errors of [`Layout::ids`].
    pub fn count(&self) -> std::io::Result<usize> {
        self.ids().map(|ids| ids.len())
    }
}

/// Refuses an id that is not safe to use as a file-name segment.
///
/// `id` is concatenated with a suffix and joined onto a directory, so anything that a path parser
/// treats as structure — a separator, a `.`/`..` component — addresses a file other than the one the
/// id names. Control characters are refused too: a newline in a name is a forged log line for
/// anything that prints one, and a NUL truncates the name in the syscall.
fn validate_id(id: &str) -> Result<(), LayoutError> {
    if id.is_empty() {
        return Err(LayoutError::EmptyId);
    }
    if id == "." || id == ".." {
        return Err(LayoutError::DirectoryName { id: id.to_owned() });
    }
    // Checked before the separator so `.`/`..` report the clearer reason.
    if id.contains('/') || id.contains('\\') {
        return Err(LayoutError::Separator { id: id.to_owned() });
    }
    if id.chars().any(char::is_control) {
        return Err(LayoutError::ControlCharacter { id: id.to_owned() });
    }
    if id.len() > MAX_ID_BYTES {
        return Err(LayoutError::TooLong {
            len: id.len(),
            max: MAX_ID_BYTES,
        });
    }
    Ok(())
}
