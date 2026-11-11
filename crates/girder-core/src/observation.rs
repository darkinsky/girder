//! File observation: what this session has seen, and whether a write would clobber someone else's change.
//!
//! A tool reads a file, the model decides what to change, and the write lands. In between, the file can change —
//! another process, another session, a build. The write would then silently undo that change, which is the failure
//! mode this module exists to prevent.
//!
//! # Why observation is per-session
//!
//! What a session has seen is what its model's decision was based on. A process-wide cache would let one session's
//! read make another session's write pass a freshness check it should have failed.
//!
//! # Why the check is a version rather than a timestamp
//!
//! A timestamp has a resolution and a clock, and two writes inside one second are indistinguishable. A version —
//! size plus modification time plus content hash — is what the provider can compute cheaply and compare exactly. The
//! hash is only needed when the first two agree, which is the case where a change could hide.
//!
//! # Why an unobserved write is allowed
//!
//! A session that never read a file has no expectation to violate. Refusing an unobserved write would make the tool
//! unusable for a file the model was told about but did not open, and the clobber risk there is not something this
//! layer can reason about.
//!
//! # Why a shell command invalidates every observation
//!
//! A shell command is opaque. `echo x > f` and `rm -rf build` look the same to this layer, and no static reading of a
//! script can tell what it wrote: the script can build the path at run time, call a binary that writes, or write
//! through a symlink. Keeping the old expectations after such a command would be worse than dropping them — a later
//! write would be checked against a version the command itself had already invalidated, and the check would either
//! pass a clobber or refuse a write that was safe. Dropping them is the honest answer, and it costs only the reads
//! that came before the command.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What identifies one version of a file.
///
/// Size and modification time are cheap; the hash catches a change that kept both — a same-length edit within one
/// filesystem tick, which is exactly the case a timestamp-only check misses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// Its size in bytes.
    pub bytes: u64,
    /// Its modification time in milliseconds since the epoch.
    pub modified_ms: u64,
    /// A hash of its contents, when the provider computed one.
    pub hash: Option<u64>,
}

impl Version {
    /// Builds a version.
    #[must_use]
    pub fn new(bytes: u64, modified_ms: u64) -> Self {
        Self {
            bytes,
            modified_ms,
            hash: None,
        }
    }

    /// Attaches a content hash.
    #[must_use]
    pub fn with_hash(mut self, hash: u64) -> Self {
        self.hash = Some(hash);
        self
    }

    /// Whether this version is the one the caller expected.
    ///
    /// A hash, when both sides have one, decides: a same-size edit within one timestamp tick is invisible to the
    /// cheaper fields, and that is precisely the change a clobber check exists to catch. Otherwise the cheap fields
    /// are compared, and a mismatch is a mismatch.
    ///
    /// The `bytes` check outside the hash comparison is load-bearing rather than redundant: two FNV-1a hashes can
    /// collide, and a collision that also agreed on size would make a changed file look untouched. Comparing size as
    /// well narrows the blind spot to a same-size collision, which for a 64-bit hash is not a practical one.
    #[must_use]
    pub fn matches(&self, other: &Version) -> bool {
        if let (Some(mine), Some(theirs)) = (self.hash, other.hash) {
            return mine == theirs && self.bytes == other.bytes;
        }
        self.bytes == other.bytes && self.modified_ms == other.modified_ms
    }
}

/// What a session last saw of one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    /// The path existed with this version.
    Present(Version),
    /// The path did not exist.
    ///
    /// Recorded rather than absent: a session that read a missing file has an expectation — that it is still
    /// missing — and a write that creates it is a change worth reporting to the model that did not expect one.
    Absent,
}

/// Why a write was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// The file changed since the session saw it.
    Changed {
        /// What the session saw.
        observed: Version,
        /// What is there now.
        current: Version,
    },
    /// The file was deleted since the session saw it.
    Deleted,
    /// The file appeared where the session saw nothing.
    Created,
}

impl Conflict {
    /// A one-line explanation, for the model.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Changed { observed, current } => format!(
                "the file changed since it was read: {} bytes at {}, now {} bytes at {}",
                observed.bytes, observed.modified_ms, current.bytes, current.modified_ms
            ),
            Self::Deleted => "the file was deleted since it was read".to_owned(),
            Self::Created => "the file appeared since the read found nothing".to_owned(),
        }
    }
}

/// One session's observations.
#[derive(Default)]
pub struct Session {
    seen: std::sync::Mutex<BTreeMap<PathBuf, Observation>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("observed", &self.paths().len())
            .finish()
    }
}

impl Session {
    /// A session that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that a path was read.
    pub fn observe(&self, path: impl Into<PathBuf>, observation: Observation) {
        self.lock().insert(path.into(), observation);
    }

    /// Records that a path was read and existed.
    pub fn observe_present(&self, path: impl Into<PathBuf>, version: Version) {
        self.observe(path, Observation::Present(version));
    }

    /// Records that a path was read and did not exist.
    pub fn observe_absent(&self, path: impl Into<PathBuf>) {
        self.observe(path, Observation::Absent);
    }

    /// What the session last saw of a path.
    #[must_use]
    pub fn observed(&self, path: &Path) -> Option<Observation> {
        self.lock().get(path).cloned()
    }

    /// How many paths have been observed.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.lock().keys().cloned().collect()
    }

    /// Forgets one path.
    ///
    /// Called after a successful write: the session's own write is the new expectation, and keeping the old one
    /// would make its next write fail against a change it made itself.
    pub fn forget(&self, path: &Path) {
        self.lock().remove(path);
    }

    /// Forgets every path, returning how many expectations were dropped.
    ///
    /// Called after a shell command, whose effect on the filesystem cannot be inferred from the script: the command
    /// could have written any file, through a path it built at run time or a binary it called. Removed rather than
    /// marked, because there is nothing left to compare against — and the module's rule is that an unobserved write
    /// is allowed, so the alternative would need a new state that refuses every path a command might have touched,
    /// which in practice is every path.
    ///
    /// The count is returned rather than swallowed so the tool can tell the model what it invalidated: a warning for
    /// a session that had read nothing is noise, and a silent drop for one that had read ten files hides why its
    /// next write suddenly succeeds.
    pub fn forget_all(&self) -> usize {
        let mut seen = self.lock();
        let dropped = seen.len();
        seen.clear();
        dropped
    }

    /// Checks whether a write may proceed.
    ///
    /// `current` is what the provider sees now, or `None` when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`Conflict`] when the write would undo a change this session did not see. An unobserved path is
    /// allowed: a session that never read a file has no expectation to violate.
    pub fn check_write(&self, path: &Path, current: Option<&Version>) -> Result<(), Conflict> {
        let Some(observed) = self.observed(path) else {
            return Ok(());
        };
        match (observed, current) {
            (Observation::Present(observed), Some(current)) => {
                if observed.matches(current) {
                    Ok(())
                } else {
                    Err(Conflict::Changed {
                        observed,
                        current: current.clone(),
                    })
                }
            }
            (Observation::Present(_), None) => Err(Conflict::Deleted),
            (Observation::Absent, Some(_)) => Err(Conflict::Created),
            // The session saw nothing and still sees nothing: creating it is what it expects.
            (Observation::Absent, None) => Ok(()),
        }
    }
}

/// Why an edit was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    /// The text to replace was not found.
    #[error("the text to replace was not found")]
    NotFound,

    /// The text to replace occurs more than once.
    ///
    /// Refused rather than replaced wholesale: an editor that replaced every occurrence would change code the
    /// caller never looked at, and the caller would not find out until the file was read again.
    #[error("the text to replace occurs {matches} times; give more context")]
    Ambiguous {
        /// How many times.
        matches: usize,
    },

    /// The replacement did not change anything.
    #[error("the replacement is identical to the original")]
    NoChange,
}

/// A version for a text this process just produced.
///
/// Size and a content hash, with no modification time: the caller has the text in hand rather than a stat, and a
/// timestamp taken now would be a guess about what the filesystem will report afterwards.
#[must_use]
pub fn version_of(text: &str) -> Version {
    Version {
        bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
        modified_ms: crate::schedule::now_millis(),
        hash: Some(hash(text)),
    }
}

/// A hash of a text, for comparing versions.
///
/// FNV-1a: small, dependency-free, and good enough to catch the change a size-and-time comparison misses. This is not
/// a security boundary — a caller that needs one hashes the file itself.
#[must_use]
pub fn hash(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Replaces one occurrence of `old` with `new` in `text`.
///
/// Refuses when `old` is missing or occurs more than once. A wholesale replace would change code the caller never
/// looked at.
///
/// # Errors
///
/// Returns [`EditError`].
pub fn replace_exactly_once(text: &str, old: &str, new: &str) -> Result<String, EditError> {
    if old.is_empty() {
        return Err(EditError::NotFound);
    }
    if old == new {
        return Err(EditError::NoChange);
    }
    let matches: Vec<usize> = text.match_indices(old).map(|(at, _)| at).collect();
    match matches.len() {
        0 => Err(EditError::NotFound),
        1 => {
            let at = matches[0];
            let mut out = String::with_capacity(text.len() + new.len());
            out.push_str(&text[..at]);
            out.push_str(new);
            out.push_str(&text[at + old.len()..]);
            Ok(out)
        }
        matches => Err(EditError::Ambiguous { matches }),
    }
}

/// Replaces with a line range, which is what a patch or a numbered read produces.
///
/// `first` is one-based and inclusive, matching what a person sees. Taking a zero-based index here would make every
/// caller subtract one, and one of them would forget.
///
/// # Errors
///
/// Returns [`EditError::NotFound`] when the range is outside the text.
pub fn replace_lines(
    text: &str,
    first: u32,
    last: u32,
    replacement: &str,
) -> Result<String, EditError> {
    if first == 0 || last < first {
        return Err(EditError::NotFound);
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = usize::try_from(first - 1).unwrap_or(usize::MAX);
    let end = usize::try_from(last).unwrap_or(usize::MAX);
    if start >= lines.len() || end > lines.len() {
        return Err(EditError::NotFound);
    }

    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + 4);
    out.extend_from_slice(&lines[..start]);
    out.extend(replacement.lines());
    out.extend_from_slice(&lines[end..]);

    let mut joined = out.join("\n");
    // A text that had a trailing newline keeps one, or every line edit shows as a whitespace change at the end.
    if text.ends_with('\n') {
        joined.push('\n');
    }
    Ok(joined)
}

impl Session {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Observation>> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
