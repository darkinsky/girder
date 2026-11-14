//! Which working directory a session was started in, as an index keyed by both ends.
//!
//! # The gap this fills
//!
//! A session in this project is a flat file: `root / {id}.jsonl.zst` beside `{id}.title` and
//! `{id}.archived` (see `session_layout`). Upstream instead groups sessions under
//! `root / projectDir(cwd) / …`, so upstream can answer "which sessions belong to this checkout"
//! from the directory tree alone. This project cannot: the tree says nothing about a working
//! directory.
//!
//! The records do not say it either. [`SessionRecord`](crate::SessionRecord) is `id`, `updated_at`,
//! `blank`, and `events`; no event variant carries a working directory. `session.create` accepts a
//! `cwd` and then drops it, so a client that started one session in `/a/project` and another in
//! `/b/project` has no way to tell them apart afterwards — not from the log, not from the listing,
//! and not from the file names, which are id-shaped only.
//!
//! This module is the association that is missing, kept as a small, pure, in-memory index: it
//! writes nothing, reads no file, and never touches a log. A caller records an association at the
//! moment it knows one — a session created in a directory, a session resumed from one — and asks
//! the index the two questions a UI asks: *which sessions are in this workspace*, and *where does
//! this session live*.
//!
//! # Why absolute only
//!
//! A relative path does not name a directory; it names a directory *given a cwd*. `src` in one
//! process and `src` in another are different places, so an identifier built from one would merge
//! two unrelated workspaces — the exact mistake this index exists to prevent. There is no correct
//! way to make a relative path absolute here: resolving it needs a cwd, which the caller has and
//! this function deliberately does not. So it is refused rather than guessed.
//!
//! Absoluteness is decided by [`std::path::Path::is_absolute`], which asks the *host's* rules. A
//! Windows path on a Unix host is therefore refused, which is the right answer: it does not name a
//! place on this machine.
//!
//! # What normalization does, and what it refuses to do
//!
//! Two spellings of one directory would otherwise be two workspaces: a shell completes `/a/b/` for
//! `/a/b`, an editor writes `/a/./b`, and a doubled separator slips in from string concatenation.
//! All three name one directory on every platform, so [`WorkspaceId::from_path`] folds trailing
//! separators, lone `.` segments, and repeated separators. The folding runs through
//! [`std::path::Component`], so the platform's own rules apply rather than this module's guess at
//! them — a Windows drive prefix and a UNC prefix survive intact.
//!
//! `..` is **not** resolved, and that is a deliberate limitation with a visible consequence:
//! `/a/../b` and `/b` are two different [`WorkspaceId`]s even though they name the same directory.
//! Resolving `..` requires the filesystem — `..` of a symlink is not the `..` of its target's
//! spelling, and on a case-insensitive volume even the final component must be looked up before it
//! can be compared. This module is a pure function over a string, so it cannot ask. Callers that
//! receive paths from a shell are the ones that should have the filesystem in hand; they can
//! canonicalize before calling. Documenting the wart is better than hiding a filesystem read
//! inside a function whose name promises a string operation.
//!
//! # Why a session has exactly one workspace, and the later association wins
//!
//! A session runs in one directory at a time, so the index holds one workspace per session. When
//! the same session is associated twice — it was resumed from somewhere else, or it was opened in a
//! second checkout — the second association replaces the first, and the session is removed from the
//! workspace it no longer belongs to.
//!
//! Replacing rather than rejecting is a judgement about what the caller is saying. A rejected
//! re-association reports "no change" to a caller that has just observed a change, and the caller's
//! only recovery would be an "unassociate" step it would have to invent. Replacing is also the only
//! rule under which replaying a log of associations in order reproduces the state the log describes.
//! The alternative — keeping the first — would leave a workspace claiming a session that has not run
//! there since, which is a wrong answer rather than a stale one.
//!
//! Call order decides, not [`Association::at_ms`]. Timestamps come from callers' clocks, and two
//! hosts describing the same session may disagree by minutes; making the outcome depend on that
//! comparison would let clock skew silently win an argument about which directory a session is in.
//! The timestamp is kept as provenance — [`Index::associated_at`] reports when the association now
//! held was observed — but it never decides. This also means re-associating is idempotent even with
//! an unchanged timestamp, and that an association is never silently dropped, which is the failure
//! mode that would hide a caller bug.
//!
//! # Ordering
//!
//! Both indices are ordered maps, so [`Index::sessions_in`] returns sorted ids: a UI that lists a
//! workspace twice sees the same order, and a test can assert one without depending on insertion
//! order.

use std::collections::{BTreeMap, BTreeSet};

/// A workspace, identified by its absolute directory path.
///
/// Construction goes through [`WorkspaceId::from_path`], which is where normalization and the
/// absolute-path requirement live; there is no way to build one from an unchecked string, so a
/// value of this type always means one normalized, absolute path.
///
/// Equality is string equality on the normalized form, which is why the normalization is not
/// cosmetic: it is what makes `/a/b/` and `/a/b` the same key in an index. See the module docs for
/// the `..` case it deliberately does not unify.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Builds an id from a directory path.
    ///
    /// The path is normalized on the way in (see the module docs): trailing separators, lone `.`
    /// segments, and repeated separators are folded, while `..` is kept as a literal component. A
    /// path is otherwise taken verbatim — only a whitespace-only string counts as empty, because a
    /// trailing space can be part of a real directory name and trimming it would name a different
    /// place.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Empty`] when the path is empty or only whitespace, and
    /// [`WorkspaceError::NotAbsolute`] when it is relative. A relative path is refused rather than
    /// resolved because resolving it needs a cwd that this pure function does not have.
    pub fn from_path(path: &str) -> Result<Self, WorkspaceError> {
        if path.trim().is_empty() {
            return Err(WorkspaceError::Empty);
        }
        if !std::path::Path::new(path).is_absolute() {
            return Err(WorkspaceError::NotAbsolute {
                path: path.to_owned(),
            });
        }
        Ok(Self(normalize(path)))
    }

    /// The normalized path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why a path cannot name a workspace.
///
/// The variants name the offending property, and the rejected path is echoed, so a message can be
/// shown to whoever supplied the path without the caller re-deriving what was wrong.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// The path was empty or only whitespace, so there is nothing to identify.
    #[error("a workspace path cannot be empty")]
    Empty,
    /// The path was not absolute, so it does not name a directory without a cwd to resolve against.
    #[error("workspace path `{path}` is not absolute")]
    NotAbsolute {
        /// The rejected path, as supplied.
        path: String,
    },
}

/// One observation: this session ran in this workspace at this time.
///
/// `at_ms` is provenance, not precedence — [`Index::associate`] explains why call order decides
/// instead of the timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Association {
    /// The session id, as it appears in its log's file name.
    pub session: String,
    /// The directory the session ran in.
    pub workspace: WorkspaceId,
    /// When the association was observed, milliseconds since the epoch.
    ///
    /// Kept so [`Index::associated_at`] can answer when a session moved, and deliberately not used
    /// to order two associations: clocks on different hosts disagree, and the caller's own call
    /// order is the fact this index trusts.
    pub at_ms: u64,
}

/// What the index holds per session.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    /// The workspace the session is currently in.
    workspace: WorkspaceId,
    /// When that association was observed.
    at_ms: u64,
}

/// Sessions indexed by workspace and workspaces indexed by session.
///
/// Two maps rather than one, because the two questions run in opposite directions and answering one
/// by scanning the other would make opening a session proportional to the number of sessions in the
/// store. Both are ordered: [`Index::sessions_in`] is sorted, and no query depends on insertion
/// order.
///
/// Keeping only the forward map in sync would let the two disagree after a re-association — the
/// session would move in one direction and stay in the other — so [`Index::associate`] updates both
/// in one step and drops an emptied workspace from the reverse map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    /// Session id to its current workspace, which is where `at_ms` is kept.
    by_session: BTreeMap<String, Entry>,
    /// Workspace path to its session ids, in sorted order.
    by_workspace: BTreeMap<String, BTreeSet<String>>,
}

impl Index {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that a session runs in a workspace.
    ///
    /// A session has exactly one workspace, so this is an overwrite, not an insert: a session
    /// re-associated with a different workspace is removed from the one it left, which keeps
    /// [`Index::sessions_in`] from claiming a session that no longer runs there. Re-associating
    /// with the *same* workspace is a no-op apart from updating the recorded time, and never
    /// duplicates the session in its workspace's list.
    ///
    /// The second call wins regardless of the two timestamps — see the module docs for why call
    /// order, and not [`Association::at_ms`], decides.
    pub fn associate(&mut self, association: Association) {
        let Association {
            session,
            workspace,
            at_ms,
        } = association;
        let entry = Entry {
            workspace: workspace.clone(),
            at_ms,
        };
        if let Some(previous) = self.by_session.insert(session.clone(), entry) {
            // The workspace the session left may now be empty. Dropping it keeps the reverse map
            // from accumulating keys for workspaces nothing is in, which would make a caller
            // iterating it walk history rather than the present.
            let emptied = self
                .by_workspace
                .get_mut(previous.workspace.as_str())
                .is_some_and(|sessions| {
                    sessions.remove(&session);
                    sessions.is_empty()
                });
            if emptied {
                self.by_workspace.remove(previous.workspace.as_str());
            }
        }
        self.by_workspace
            .entry(workspace.as_str().to_owned())
            .or_default()
            .insert(session);
    }

    /// Every session in a workspace, sorted by id.
    ///
    /// Sorted rather than insertion-ordered so two listings of one workspace agree, whether they
    /// come from a replay, two calls, or two processes. A workspace nothing is in is an empty list,
    /// not an error: no sessions is an answer.
    #[must_use]
    pub fn sessions_in(&self, workspace: &WorkspaceId) -> Vec<&str> {
        self.by_workspace
            .get(workspace.as_str())
            .map_or_else(Vec::new, |sessions| {
                sessions.iter().map(String::as_str).collect()
            })
    }

    /// The workspace a session is in, or `None` when it has never been associated.
    ///
    /// `None` rather than a default workspace: a session from before this index existed has no
    /// known directory, and reporting the root — or the current directory — would be a guess the
    /// caller could not tell from a fact.
    #[must_use]
    pub fn workspace_of(&self, session: &str) -> Option<&WorkspaceId> {
        self.by_session.get(session).map(|entry| &entry.workspace)
    }

    /// How many sessions a workspace holds.
    ///
    /// Counts distinct sessions, so it agrees with [`Index::sessions_in`] even if a caller
    /// associated one session with the same workspace repeatedly.
    #[must_use]
    pub fn count_in(&self, workspace: &WorkspaceId) -> usize {
        self.by_workspace
            .get(workspace.as_str())
            .map_or(0, BTreeSet::len)
    }

    /// When the session's current association was observed, in milliseconds since the epoch.
    ///
    /// `None` when the session is unknown, which a caller must distinguish from "associated at
    /// time zero": an association observed at the epoch is still an association.
    #[must_use]
    pub fn associated_at(&self, session: &str) -> Option<u64> {
        self.by_session.get(session).map(|entry| entry.at_ms)
    }

    /// How many sessions are indexed in total.
    ///
    /// Each session appears in exactly one workspace, so this is also the sum of
    /// [`Index::count_in`] over every workspace.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_session.len()
    }

    /// Whether nothing has been associated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_session.is_empty()
    }
}

/// Folds the spellings that name the same directory, and nothing more.
///
/// The folding is [`std::path::Component`]'s, not a hand-rolled split, so this module inherits the
/// platform's own rules instead of re-deriving them: trailing separators go, lone `.` segments go,
/// repeated separators collapse, a Windows prefix and a UNC prefix survive, and `ParentDir` (`..`)
/// is kept verbatim. Rebuilding through [`std::path::PathBuf`] rather than concatenating strings is
/// what keeps the result a path the host would accept rather than a spelling this module invented.
///
/// The one visible consequence on Unix is that `//a/b` normalizes to `/a/b`. POSIX leaves a path
/// beginning with exactly two slashes implementation-defined, and Rust's Unix path rules already
/// treat a leading run of separators as one root; splitting that hair here would mean two spellings
/// that [`std::path::Path`] considers equal being unequal in this index.
fn normalize(path: &str) -> String {
    let components: std::path::PathBuf = std::path::Path::new(path).components().collect();
    // The rebuilt path is only empty when the input had no components, which `from_path` has
    // already refused. Falling back to the input keeps this total rather than aborting.
    if components.as_os_str().is_empty() {
        return path.to_owned();
    }
    components.to_string_lossy().into_owned()
}
