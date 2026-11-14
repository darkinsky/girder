//! In-process file locks: one writer at a time, per path, inside one process.
//!
//! # What this is, and what it is not
//!
//! This is a **process-internal** lock table, not a filesystem lock. It holds an entry in a map
//! that this process owns; nothing is written to disk, no `flock`/`fcntl` is taken, and no other
//! process can see or respect it. Two consequences follow, and both matter:
//!
//! - Two subagents, two sessions, two `write` calls dispatched concurrently from one process all
//!   contend on the same table, and one of them is refused.
//! - A second *process* is a different table and does not contend at all. Cross-process writer
//!   coordination is [`crate::atomic_write::with_lock`](crate::atomic_write), which is a real
//!   lock file with a liveness-checked holder. Naming the two apart is deliberate: a caller that
//!   reaches for this module expecting to exclude an editor, a build, or another copy of the
//!   binary would get a lock that silently does nothing.
//!
//! # Why the in-process case needs its own lock
//!
//! The observation layer answers a different question. `Session::check_write` compares the version
//! the session last saw against the version on disk, and that detects a change made by *someone
//! else* before this write. It cannot detect a change made by *this* write's sibling: two
//! concurrent writers can both pass the check — both read the same version, both compare equal —
//! and then both write, with the second quietly discarding the first. The check has no memory of
//! "a write is in flight", because an in-flight write has not changed the disk yet. A lock closes
//! exactly that window: it makes the check and the write mutual, so the second writer's check runs
//! after the first writer's bytes are on disk and fails for the right reason.
//!
//! # Why `AlreadyYours` is an error rather than a success
//!
//! Re-acquiring a lock you already hold is almost never an intentional nesting; there is no depth
//! counter here to make it one. In practice it means one of two bugs: a recursive call that
//! re-entered a write path it was already inside, or a caller that forgot to release after a
//! failure and is now masking the leak. Both are invisible if the second acquire returns `Ok(())`,
//! and both lead to the *first* `release` freeing a lock that the outer frame still believes it
//! holds — after which a third writer walks in. Reporting the duplicate makes the mistake visible
//! at the moment it happens instead of at the interleaving it causes later.
//!
//! # Why locks are reaped rather than expired on read
//!
//! A holder can disappear without releasing: a panic unwinds past the release, a task is aborted,
//! a subagent is cancelled. If the entry stayed forever, that path would be unwritable for the rest
//! of the process's life, and the failure would look like a hang rather than an error. [`Locks::reap`]
//! removes entries older than a timeout, and it is an explicit call rather than a rule hidden in
//! [`Locks::acquire`] so that the timeout is one number the caller owns: a long build and a quick
//! edit do not deserve the same one, and a lock table that silently guessed would either drop a
//! live lock or keep a dead one.
//!
//! The protocol for a contender is therefore: `acquire`, and on [`Contention::HeldBy`] call
//! [`Locks::reap`] once with the timeout that suits the work, then retry. The second `acquire` then
//! distinguishes "the holder died" (succeeds) from "the holder is alive and busy" (still refuses).
//!
//! # Why paths are normalized
//!
//! `/a/b/`, `/a/b`, `/a//b`, and `/a/./b` name one file, and a lock table keyed on the raw string
//! would let two callers hold "different" locks on the same file — the exact failure this module
//! exists to prevent. Normalization is purely lexical: it collapses separators and `.`/`..`, and it
//! never touches the filesystem. It does not resolve symlinks or relative paths against a working
//! directory, because doing so would require I/O on the acquire path and would make two spellings
//! of the same file agree only when the process happened to be in the right directory. The caller
//! is expected to have resolved the path already; what this module guarantees is that the lexical
//! spellings of one path share one lock.
//!
//! # Example
//!
//! ```
//! use girder_core::{Contention, FileLocks};
//!
//! let mut locks = FileLocks::new();
//! assert!(locks.acquire("/tmp/notes.md", "session-a", 1_000).is_ok());
//!
//! // A trailing slash names the same file, so the same lock.
//! match locks.acquire("/tmp/notes.md/", "session-b", 1_001) {
//!     Err(Contention::HeldBy { owner, since_ms }) => {
//!         assert_eq!(owner, "session-a");
//!         assert_eq!(since_ms, 1_000);
//!     }
//!     other => panic!("expected contention, got {other:?}"),
//! }
//!
//! assert!(locks.release("/tmp/notes.md", "session-a"));
//! assert!(locks.acquire("/tmp/notes.md", "session-b", 1_002).is_ok());
//! ```

use std::collections::BTreeMap;

/// One held lock.
///
/// Returned by [`Locks::holder`] and by [`Locks::reap`], so a caller can tell a subagent *who* is
/// blocking it and *since when* without keeping a second copy of that fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lock {
    /// The path that is locked, normalized as described in the module docs.
    pub path: String,
    /// Who holds it.
    ///
    /// Opaque to this module: a session id, a subagent name, a tool call id — anything the caller
    /// can compare for equality. It is echoed back rather than interpreted, so the module never
    /// needs to know what an owner is in order to enforce exclusivity.
    pub owner: String,
    /// When the lock was acquired, in milliseconds on the caller's clock.
    ///
    /// This is the caller's clock, not `SystemTime::now`, for two reasons: the value is compared
    /// against [`Locks::reap`]'s argument and must come from the same clock, and tests need to
    /// drive time forward without sleeping.
    pub at_ms: u64,
}

/// Why an [`Locks::acquire`] was refused.
///
/// Both variants are errors. See the module docs for why a duplicate acquisition is not folded
/// into the success case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Contention {
    /// Someone else holds the lock.
    ///
    /// Carries the holder and its acquisition time so the refusal can be reported as a fact
    /// ("`session-a` has held `/tmp/notes.md` since 1000 ms") rather than a bare "busy", which a
    /// model cannot act on.
    HeldBy {
        /// The current holder.
        owner: String,
        /// When the current holder acquired it, in milliseconds.
        since_ms: u64,
    },

    /// The same owner already holds the lock.
    ///
    /// A duplicate, not a conflict: it usually means a recursive write path or a release that was
    /// skipped on an error branch, and treating it as success would hide both.
    AlreadyYours,
}

/// The set of locks this process currently holds.
///
/// One instance is the whole table. Storing the table here rather than in a global keeps the lock
/// state visible to whoever owns it and testable without process-wide fixtures; the caller that
/// wires tools together owns the single instance they share.
#[derive(Clone, Debug, Default)]
pub struct Locks {
    /// Keyed by normalized path. A `BTreeMap` rather than a `HashMap` so that [`Locks::reap`]
    /// returns locks in a stable order: a reaped-lock report that reshuffled its lines between runs
    /// would be hard to read and harder to test.
    held: BTreeMap<String, Lock>,
}

impl Locks {
    /// Creates an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            held: BTreeMap::new(),
        }
    }

    /// Takes the lock on `path` for `owner`.
    ///
    /// # Errors
    ///
    /// Returns [`Contention::HeldBy`] when another owner holds the lock, and
    /// [`Contention::AlreadyYours`] when `owner` already holds it. On an error nothing changes: a
    /// refused acquisition never leaves a half-taken lock behind, and it never extends the existing
    /// holder's timestamp, which would let a contender keep a live lock alive indefinitely.
    ///
    /// A lock whose holder has died is still reported as held: deciding that a holder is dead is
    /// what [`Locks::reap`] does, and it needs a timeout this call does not take. A caller that gets
    /// [`Contention::HeldBy`] should reap once and retry before concluding the file is busy.
    pub fn acquire(&mut self, path: &str, owner: &str, now_ms: u64) -> Result<(), Contention> {
        let key = normalize(path);
        if let Some(lock) = self.held.get(&key) {
            return Err(if lock.owner == owner {
                Contention::AlreadyYours
            } else {
                Contention::HeldBy {
                    owner: lock.owner.clone(),
                    since_ms: lock.at_ms,
                }
            });
        }
        self.held.insert(
            key.clone(),
            Lock {
                path: key,
                owner: owner.to_owned(),
                at_ms: now_ms,
            },
        );
        Ok(())
    }

    /// Releases the lock on `path`, but only for the owner that holds it.
    ///
    /// Returns whether a lock was released. `false` means there was nothing to release *for this
    /// owner* — the lock was free, or someone else holds it. A non-holder cannot release another
    /// owner's lock, because that would let one session free a lock it never took and hand the file
    /// to a third writer while the real holder still believes it is alone.
    ///
    /// Releasing a lock that is not held is not an error: the release path runs on error branches
    /// where the acquire may never have happened, and forcing every caller to remember whether it
    /// did would produce leaks rather than correctness. The boolean is for reporting, not for
    /// branching.
    pub fn release(&mut self, path: &str, owner: &str) -> bool {
        let key = normalize(path);
        match self.held.get(&key) {
            Some(lock) if lock.owner == owner => self.held.remove(&key).is_some(),
            _ => false,
        }
    }

    /// Returns the lock currently held on `path`, if any.
    ///
    /// The same normalization the other methods use applies here, so a caller can ask about the
    /// path it is about to write with the same spelling it will pass to [`Locks::acquire`].
    #[must_use]
    pub fn holder(&self, path: &str) -> Option<&Lock> {
        self.held.get(&normalize(path))
    }

    /// Removes locks older than `timeout_ms` and returns them.
    ///
    /// Age is `now_ms - at_ms`, saturating so a clock that went backwards — or a `now_ms` smaller
    /// than the recorded acquisition time — cannot underflow into "infinitely old" and reap a lock
    /// that was just taken. A lock exactly `timeout_ms` old is reaped; a `timeout_ms` of zero
    /// therefore reaps everything, which is the caller saying "assume every holder is gone".
    ///
    /// The removed locks are returned rather than dropped so the caller can report who was holding
    /// what when it decided they were dead: a lock silently vanished from a table is a diagnosis
    /// that has to be reconstructed later from logs that may not have it.
    pub fn reap(&mut self, now_ms: u64, timeout_ms: u64) -> Vec<Lock> {
        let mut reaped = Vec::new();
        self.held.retain(|_, lock| {
            let stale = now_ms.saturating_sub(lock.at_ms) >= timeout_ms;
            if stale {
                reaped.push(lock.clone());
            }
            !stale
        });
        reaped
    }

    /// Returns how many locks are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Returns whether the table holds no locks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

/// Reduces a path to the key a lock is stored under.
///
/// Lexical only — see the module docs for why it does not resolve symlinks or a working directory.
/// A Windows path spelled with `\` is treated as a single opaque component, which is honest: this
/// module makes no claim about separator equivalence it cannot check without knowing the platform.
fn normalize(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                // `..` cancels the previous component. At the root there is nothing left to cancel
                // and the root is its own parent, so `/..` is `/`; a relative path keeps the `..`
                // because only the working directory could resolve it.
                if matches!(parts.last(), Some(last) if *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    if absolute {
        // `parts.join` is empty for the root, which would otherwise normalize to `""`.
        format!("/{}", parts.join("/"))
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::{Contention, Locks, normalize};

    #[test]
    fn normalize_collapses_separators_and_dots() {
        assert_eq!(normalize("/a/b/"), "/a/b");
        assert_eq!(normalize("/a//b"), "/a/b");
        assert_eq!(normalize("/a/./b"), "/a/b");
        assert_eq!(normalize("/a/b/../c"), "/a/c");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("//"), "/");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("a/b"), "a/b");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn a_duplicate_acquisition_is_reported() {
        let mut locks = Locks::new();
        assert!(locks.acquire("/a", "me", 1).is_ok());
        assert_eq!(locks.acquire("/a", "me", 2), Err(Contention::AlreadyYours));
    }

    #[test]
    fn reap_uses_the_clock_arithmetic_safely() {
        let mut locks = Locks::new();
        assert!(locks.acquire("/a", "me", 100).is_ok());
        // A clock that went backwards must not make the lock look ancient.
        assert!(locks.reap(50, 10).is_empty());
        assert_eq!(locks.reap(100, 0).len(), 1);
    }
}
