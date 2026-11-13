//! Polling-based detection of changes under a workspace directory.
//!
//! [`fs`](crate::fs) answers "what is at this path right now". Nothing in it answers "what changed
//! since I last looked", so this module adds that missing half as an explicitly stateless-scan
//! wrapper: each [`PollingWatcher::scan`] walks the tree, diffs it against the previous walk, and
//! reports the entries that appeared, changed, or vanished.
//!
//! # Why polling instead of an OS notification API
//!
//! The platform watchers (`inotify`, `kqueue`, `ReadDirectoryChangesW`) each need their own crate,
//! their own event vocabulary, and their own failure modes — watch descriptors run out under
//! `inotify`, recursive watches must be re-armed as directories appear, and a network filesystem
//! often delivers nothing at all while reporting success. A poll loop needs only [`std::fs`], so
//! the same code runs everywhere and never claims a change it did not observe.
//!
//! The cost is real and worth stating plainly:
//!
//! - **Latency** is the poll interval, not the moment of the write. A watcher built with a
//!   one-second interval can be up to a second stale, which is fine for "refresh the file tree" and
//!   wrong for "react before the next keystroke".
//! - **CPU and I/O scale with the tree, not with the change.** Every scan costs a `stat` per
//!   entry, so a large tree burns work on every tick whether or not anything happened. An event
//!   API costs nothing while idle. Poll intervals below a second over tens of thousands of files
//!   are a measurable drain on a laptop battery, and on mechanical or network storage the walk
//!   itself becomes the bottleneck.
//! - **Big directories are slow.** The scan is synchronous and single-threaded, so a tree with
//!   hundreds of thousands of entries produces a scan that takes seconds, during which it detects
//!   nothing else. Pick the interval *after* measuring one scan; an interval shorter than a scan
//!   just queues runs back to back.
//! - **Short-lived states are missed.** A file created and deleted between two scans is never
//!   reported. Polling observes states, not transitions.
//!
//! # Why a change is detected by mtime *and* size
//!
//! Modification time alone is not sufficient. Many filesystems store timestamps at a coarse
//! granularity (one second is common, and FAT is worse), so two writes inside the same tick leave
//! the mtime identical. Comparing the size as well catches the ordinary "same-second, different
//! content length" edit, which is exactly the shape of an editor saving a buffer. The pair is still
//! not a content hash: an edit that keeps both the mtime and the byte count is invisible, and a
//! caller that needs certainty must hash the file itself. That tradeoff is deliberate — hashing
//! every file on every tick would make the walk read the whole tree.
//!
//! # Which directories are skipped, and why
//!
//! [`is_skipped_dir`] excludes `.git`, `node_modules`, `target`, and the other build outputs listed
//! there. They are skipped because they are huge, machine-generated, and churn constantly: walking
//! `target` after a `cargo build` costs far more than the source tree it was built from, and its
//! entries are not something a workspace observer ever wants reported. The consequence is that a
//! change *inside* one of them is genuinely invisible to this watcher — if that matters for a
//! caller, it must watch the directory directly with a root of its own.
//!
//! The root itself is never excluded, so pointing a watcher at a `target` directory still works;
//! only nested occurrences are pruned.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How deep a scan descends before it stops.
///
/// A bound rather than a promise: it stops a symlink loop or a pathological tree from turning one
/// scan into an unbounded walk.
const MAX_DEPTH: usize = 32;

/// What happened to one path between two scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChangeKind {
    /// The path is present now and was absent from the previous snapshot.
    Created,
    /// The path is present in both snapshots but its size or mtime differs.
    Modified,
    /// The path was in the previous snapshot and is gone now.
    Removed,
}

impl ChangeKind {
    /// A short lowercase name, suitable for logs and wire formats.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Removed => "removed",
        }
    }
}

/// One observed change.
///
/// `path` is relative to the watched root and uses `/` as the separator, so a report reads the
/// same on every host and can be compared against a workspace-relative path directly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Change {
    /// Path relative to the workspace root.
    pub path: String,
    /// What happened to it.
    pub kind: ChangeKind,
}

/// Whether a directory name is pruned from a snapshot.
///
/// These are the directories that are large, generated, and constantly rewritten. Walking them
/// would dominate the scan and report changes nobody asked about. The list matches the one
/// [`search_tools`](crate::search_tools) uses for the same reason, so a user who understands why
/// `grep` skipped a directory understands why this did too.
#[must_use]
pub fn is_skipped_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".venv"
            | "__pycache__"
            | ".mypy_cache"
            | ".next"
            | ".cache"
    )
}

/// Takes one snapshot of `root`: relative path to `(mtime_millis, size_bytes)`.
///
/// Directories are included, with a size of zero, so that a removed file's parent directory and a
/// created one are both represented; that keeps the comparison in [`PollingWatcher::scan`] a plain
/// key diff over one map.
///
/// A path that cannot be read is omitted rather than reported as an error. A watcher is a best-
/// effort observer: one unreadable entry must not blind it to every other entry, and re-reporting
/// the failure on every tick would be noise. The corresponding absence shows up as a `Removed`
/// transition the first time it happens, which is the useful signal.
///
/// Paths are sorted because the result is a [`BTreeMap`]; callers can therefore compare snapshots
/// and iterate deterministically.
#[must_use]
pub fn snapshot(root: &Path) -> BTreeMap<String, (u64, u64)> {
    let mut entries = BTreeMap::new();
    walk(root, root, 0, &mut entries);
    entries
}

/// Depth-first walk that fills `entries` with `root`-relative keys.
fn walk(root: &Path, dir: &Path, depth: usize, entries: &mut BTreeMap<String, (u64, u64)>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Some(key) = relative_key(root, &path) else {
            continue;
        };
        if metadata.is_dir() {
            if is_skipped_dir(&entry.file_name().to_string_lossy()) {
                continue;
            }
            entries.insert(key, (mtime_millis(&metadata), 0));
            walk(root, &path, depth + 1, entries);
        } else if metadata.is_file() {
            entries.insert(key, (mtime_millis(&metadata), metadata.len()));
        }
        // Sockets, fifos, and dangling symlinks are skipped: they carry neither a meaningful size
        // nor stable content, so reporting them would be noise the caller cannot act on.
    }
}

/// Renders `path` relative to `root` with `/` separators.
///
/// Returns `None` for a path outside `root`, which cannot normally happen but must not be turned
/// into a bogus key.
fn relative_key(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut key = String::new();
    for component in relative.components() {
        if !key.is_empty() {
            key.push('/');
        }
        key.push_str(&component.as_os_str().to_string_lossy());
    }
    (!key.is_empty()).then_some(key)
}

/// Milliseconds since the Unix epoch, or zero when the clock is unrepresentable.
///
/// A pre-epoch or absurd timestamp is clamped instead of failing the walk: the exact value only
/// needs to be comparable between two scans, and zero still compares correctly against a later
/// value.
fn mtime_millis(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// A watcher that detects workspace changes by re-scanning on demand.
///
/// The watcher owns the previous snapshot; calling [`scan`](Self::scan) advances it. Nothing runs
/// in the background, so the poll interval is a property the caller schedules against rather than a
/// thread this type starts — which keeps the cost visible at the call site and makes the type
/// trivially testable.
///
/// ```
/// use std::time::Duration;
/// use girder_core::file_watch::PollingWatcher;
///
/// let mut watcher = PollingWatcher::new(std::env::temp_dir(), Duration::from_secs(1));
/// assert!(watcher.scan().is_empty(), "the first scan is the baseline");
/// ```
pub struct PollingWatcher {
    /// The directory the snapshots are taken of.
    workspace: PathBuf,
    /// Advisory interval between scans; see the module docs for what it does and does not control.
    interval: Duration,
    /// The snapshot the next [`scan`](Self::scan) compares against.
    previous: Option<BTreeMap<String, (u64, u64)>>,
}

impl std::fmt::Debug for PollingWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollingWatcher")
            .field("workspace", &self.workspace)
            .field("interval", &self.interval)
            .field("initialized", &self.previous.is_some())
            .finish()
    }
}

impl PollingWatcher {
    /// Creates a watcher over `workspace`, scanning no more often than `interval` is intended to be.
    ///
    /// The first [`scan`](Self::scan) establishes the baseline and reports nothing; otherwise every
    /// pre-existing file would be announced as `Created`. Call it once at startup.
    #[must_use]
    pub fn new(workspace: PathBuf, interval: Duration) -> Self {
        Self {
            workspace,
            interval,
            previous: None,
        }
    }

    /// The directory being watched.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The advisory poll interval.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// The current snapshot, or `None` before the first scan.
    ///
    /// Exposed so a caller can persist a baseline across restarts rather than re-announcing every
    /// file on the first tick.
    #[must_use]
    pub fn baseline(&self) -> Option<&BTreeMap<String, (u64, u64)>> {
        self.previous.as_ref()
    }

    /// Scans once and returns the changes since the previous scan.
    ///
    /// Ordering is by path, because the underlying maps are sorted; the result is deterministic
    /// for a given pair of snapshots.
    ///
    /// Returns every observation in one pass: a path in the new snapshot but not the old is
    /// [`ChangeKind::Created`], the reverse is [`ChangeKind::Removed`], and a path whose
    /// `(mtime, size)` pair differs is [`ChangeKind::Modified`].
    pub fn scan(&mut self) -> Vec<Change> {
        let current = snapshot(&self.workspace);
        let changes = match self.previous.take() {
            None => Vec::new(),
            Some(previous) => diff(&previous, &current),
        };
        self.previous = Some(current);
        changes
    }
}

/// Compares two snapshots and returns the path-ordered change list.
fn diff(
    previous: &BTreeMap<String, (u64, u64)>,
    current: &BTreeMap<String, (u64, u64)>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (path, stamp) in previous {
        match current.get(path) {
            None => changes.push(Change {
                path: path.clone(),
                kind: ChangeKind::Removed,
            }),
            Some(now) if now != stamp => changes.push(Change {
                path: path.clone(),
                kind: ChangeKind::Modified,
            }),
            Some(_) => {}
        }
    }
    for path in current.keys() {
        if !previous.contains_key(path) {
            changes.push(Change {
                path: path.clone(),
                kind: ChangeKind::Created,
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes
}

/// Resolves a modification time into a comparable stamp, for callers that need one.
///
/// A thin helper so external code does not have to repeat the epoch arithmetic in
/// [`snapshot`]. A time before the epoch maps to zero, matching the snapshot.
#[must_use]
pub fn stamp(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_kind_names_are_stable() {
        assert_eq!(ChangeKind::Created.as_str(), "created");
        assert_eq!(ChangeKind::Modified.as_str(), "modified");
        assert_eq!(ChangeKind::Removed.as_str(), "removed");
    }

    #[test]
    fn skipped_directories_are_recognized() {
        for name in [".git", "node_modules", "target", "dist", "build", ".venv"] {
            assert!(is_skipped_dir(name), "{name} must be skipped");
        }
        assert!(!is_skipped_dir("src"));
    }

    #[test]
    fn a_pre_epoch_stamp_is_zero() {
        assert_eq!(stamp(UNIX_EPOCH - Duration::from_secs(1)), 0);
        assert!(stamp(SystemTime::now()) > 0);
    }
}
