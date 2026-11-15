//! A bounded, in-memory undo stack for file changes made inside one turn.
//!
//! A model that writes a file and then discovers the change was wrong has no way back. The write
//! tool is atomic and conflict-checked, so the failure is never a torn file — it is a file that is
//! exactly as intended, and exactly not what was wanted. Nothing in the write path keeps the bytes
//! that were replaced, and `write_atomic` is deliberately a "replace completely" operation rather
//! than a versioned store, so the previous content is gone the moment the rename lands.
//!
//! The two mechanisms nearby answer different questions and neither answers this one:
//!
//! - [`crate::file_lock`] makes writers take turns. It prevents *concurrent* writes; a lone writer
//!   that writes the wrong bytes is not a contention bug and is not detected there.
//! - [`crate::session_rewind`] cuts a session log back to the end of an earlier turn. It restores
//!   the *conversation*; the files on disk are untouched, so a rewound session still sits on top of
//!   the edits the dropped turns made.
//!
//! # Why the stack is bounded
//!
//! A long session may change thousands of files, and each entry holds the whole previous content.
//! Retaining all of them would turn a few edits into unbounded memory growth — and the entries at
//! the bottom are the least useful: undo is an operation on "the last thing that happened", so the
//! oldest snapshots are the ones nobody will ask for. The stack therefore keeps the most recent
//! [`DEFAULT_CAPACITY`] changes and drops the oldest to make room, which bounds memory at a constant
//! per entry. The bound is visible through [`Log::capacity`] so a caller can report what is still
//! undoable instead of assuming everything is.
//!
//! # Why `before` is an `Option`
//!
//! Creating a file and emptying a file are different states that would collapse into one if the
//! previous content were a plain `String`. `None` means the file did not exist before the change, so
//! undo must *delete* it; `Some("")` means it existed and was empty, so undo must write the empty
//! string back. Treating the first case as the second leaves an empty file behind where there was
//! none, which is a state the user never had.
//!
//! # Why this module does not touch the filesystem
//!
//! [`Log::undo`] returns what *should* be written, rather than writing it. Three reasons:
//!
//! - The caller owns the write policy. A restore is a normal write and should take the same route
//!   as any other — `atomic_write` for content, an unlink for a created file — including whatever
//!   locking and conflict checking that route applies. A module that wrote directly would be a
//!   second, quieter write path.
//! - Reading the previous content is the caller's step as well: it is the only place that knows
//!   whether a file existed, whether it was readable, and whether the change actually landed.
//! - What remains here is a pure function over strings, so it is testable without a temporary
//!   directory or a filesystem that happens to be writable.
//!
//! # Why a turn clears the stack
//!
//! Undo means "take back what this turn did". Once the turn ends and the user has seen the result,
//! the previous contents are stale — a later turn may legitimately rewrite the same file, and an
//! undo that reached across turns would silently revert work the user approved. Clearing at the turn
//! boundary keeps the guarantee narrow and therefore true.

use std::collections::VecDeque;

/// How many changes the stack retains before dropping the oldest.
///
/// Chosen to be large enough for a turn that fans out over a project and small enough that the
/// snapshots stay modest: 64 whole previous file contents is bounded work, while a turn that edits
/// more than that has long since stopped being something a single undo should walk back.
pub const DEFAULT_CAPACITY: usize = 64;

/// One file change, with enough information to reverse it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Which file was written.
    pub path: String,
    /// The content before the change, or `None` when the file did not exist.
    ///
    /// The two cases are not interchangeable: `None` means undo deletes, while an empty string means
    /// undo writes an empty file back. See the module documentation.
    pub before: Option<String>,
    /// When the change happened, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// Which tool produced the change, for reporting it back to the user.
    pub tool: String,
}

/// A bounded last-in-first-out stack of undoable changes.
#[derive(Clone, Debug, Default)]
pub struct Log {
    changes: VecDeque<Change>,
}

impl Log {
    /// Creates an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            changes: VecDeque::new(),
        }
    }

    /// Records one change as undoable.
    ///
    /// When the log is already at [`DEFAULT_CAPACITY`], the oldest entry is dropped. That entry is
    /// the one a user is least likely to reach for, and dropping it is what keeps memory bounded;
    /// the choice is made here rather than at the call site so every recorder gets the same bound.
    pub fn record(&mut self, change: Change) {
        if self.changes.len() == DEFAULT_CAPACITY {
            self.changes.pop_front();
        }
        self.changes.push_back(change);
    }

    /// Pops the most recent change, returning what a caller should restore.
    ///
    /// The returned [`Change::before`] is the instruction, not the action: `Some(content)` means
    /// write that content back, `None` means delete the file. Nothing is written here, and no entry
    /// is left behind — a second call walks one change further back.
    pub fn undo(&mut self) -> Option<Change> {
        self.changes.pop_back()
    }

    /// How many changes can be undone, at most.
    ///
    /// A method on the log rather than a module constant so that a caller holding a log asks that
    /// log what it can still undo, and so the bound can become per-instance later without changing
    /// a single call site. Today every log answers [`DEFAULT_CAPACITY`].
    #[allow(
        clippy::unused_self,
        reason = "the receiver is the log being asked, and keeping it makes a future per-instance \
                  bound a non-breaking change"
    )]
    #[must_use]
    pub fn capacity(&self) -> usize {
        DEFAULT_CAPACITY
    }

    /// Forgets every recorded change.
    ///
    /// Called at the end of a turn: undo only ever means "the current turn", and keeping entries
    /// across turns would let an undo revert work a later turn legitimately did.
    pub fn clear(&mut self) {
        self.changes.clear();
    }

    /// How many changes can currently be undone.
    #[must_use]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Whether there is nothing to undo.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}
