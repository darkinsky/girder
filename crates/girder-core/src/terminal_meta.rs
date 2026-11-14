//! Metadata for a terminal: which terminal it is, what a person calls it, and how long its output is
//! kept after it closes.
//!
//! [`crate::terminal`] owns the live object — the shell, its bytes, its status — and answers
//! everything that needs the process to exist. This module owns the facts that outlive the question:
//! whether a terminal was ever given a name, when it was created, and which policy governs the
//! transcript once the shell is gone. Those are the inputs `terminal.rename` and `terminal.retain`
//! need, and neither of them can read them off a [`crate::TerminalSession`], which carries an id and
//! a size and nothing a person chose.
//!
//! # In-memory only, and deliberately not persistence
//!
//! This store lives for as long as the process does. It is *not* what `terminal.retain` needs in
//! order to work: retention also has to keep the transcript bytes somewhere that survives the
//! process that produced them, and that is a different piece of machinery (`terminal_buffer` bounds
//! output while the process runs; a durable store would have to write it to disk, decide a format,
//! and reclaim it later). Keeping the two apart matters because they fail differently. Losing a
//! label costs a title. Losing a retained transcript costs the output a user was promised, after the
//! terminal is gone and cannot be re-read. A module that blurred the two would suggest the second
//! was already handled the moment the first was.
//!
//! So [`Retention`] is a *policy*, and [`Store::expiring`] answers "what should the cleanup pass
//! delete now". Nothing here deletes anything: a policy that quietly removed a terminal's metadata
//! would make an id that is still addressable at the RPC layer look unknown for reasons no caller
//! could see.
//!
//! # Why a closed terminal needs a separate record
//!
//! [`Meta`] has a creation time and no close time, because a terminal is described once and its
//! lifetime is an event stream. [`Store::close`] records the moment the terminal stopped being
//! usable, and every retention window is measured from *there* rather than from creation: "keep the
//! output for an hour" means an hour after the work finished, not an hour after a long-running shell
//! started. A terminal that is still open has no deadline at all, so it never appears in
//! [`Store::expiring`] — closing a terminal twice must not extend its life, which is why the first
//! close time is the one that is kept.

use std::collections::BTreeMap;

/// The longest permitted label, in characters.
///
/// Exposed because a UI enforces the same bound while the person types; a private limit would let the
/// text box accept a name the store then refuses, and the refusal would arrive at the least useful
/// moment. Characters rather than bytes so the number means what a person counting would get, and so
/// a Chinese label has the same budget as a Latin one.
pub const MAX_LABEL_CHARS: usize = 64;

/// What is known about one terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// The terminal id.
    ///
    /// The same id the terminal RPC surface uses. It is the only handle every layer agrees on, so
    /// metadata is keyed by it rather than by position in a list.
    pub id: String,
    /// The name a user gave it, if any.
    ///
    /// `None` is the normal state, not an error: a terminal is created by typing a command, and
    /// asking for a title first would make the common case require a second call. A caller that
    /// wants to show something still has the id.
    pub label: Option<String>,
    /// When it was created, in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// What happens to its output once it closes.
    pub retention: Retention,
}

impl Meta {
    /// Metadata for a terminal that has just been created.
    ///
    /// The retention default is [`Retention::Discard`] because that is the only policy that costs
    /// nothing if nobody chose one: keeping output forever by default would silently grow a store, and
    /// keeping it for a fixed window would promise a user something no one asked for.
    #[must_use]
    pub fn new(id: impl Into<String>, created_ms: u64) -> Self {
        Self {
            id: id.into(),
            label: None,
            created_ms,
            retention: Retention::Discard,
        }
    }
}

/// What should happen to a terminal's output once it closes.
///
/// The three cases are not degrees of the same thing, which is why this is an enum rather than a
/// number of milliseconds with a sentinel. `0` would have to mean either "delete now" or "keep
/// forever", and whichever it meant, every reader would have to remember which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retention {
    /// Delete the output as soon as the terminal closes.
    Discard,
    /// Keep the output for this many milliseconds after the close.
    ///
    /// A window rather than a deadline: the point is that time does not depend on when the user
    /// looked, so a retained transcript expires even if nobody opened it.
    KeepFor(u64),
    /// Keep the output indefinitely.
    ///
    /// Never returned by [`Store::expiring`]. An automated cleanup that could delete these would make
    /// "forever" mean "until the next cleanup pass", which is exactly what a user choosing this
    /// option is trying to avoid.
    Forever,
}

impl Retention {
    /// When the output becomes deletable, measured from the close.
    ///
    /// `None` means never, which is how [`Retention::Forever`] is represented instead of as a
    /// sentinel timestamp: `u64::MAX` would still be a time, and a caller comparing against it would
    /// be one overflow away from deleting everything.
    ///
    /// The addition saturates because `closed_ms` comes from a clock and `0..=u64::MAX` is a legal
    /// argument: a wrapped deadline would land in the past and expire a terminal that had just been
    /// given a window, which is worse than an unreachable one.
    #[must_use]
    pub fn expires_at(self, closed_ms: u64) -> Option<u64> {
        match self {
            Self::Discard => Some(closed_ms),
            Self::KeepFor(window_ms) => Some(closed_ms.saturating_add(window_ms)),
            Self::Forever => None,
        }
    }
}

/// A rejected metadata operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MetaError {
    /// The store holds no terminal with this id.
    ///
    /// Carries the id so a caller can report which terminal it meant: a rename that failed for a
    /// typo'd id and one that failed because the terminal had already been removed need the same
    /// fix, but the first one is a bug in the caller and the second is not.
    #[error("no terminal with id `{id}`")]
    Unknown {
        /// The id that was asked for.
        id: String,
    },
    /// The label is longer than [`MAX_LABEL_CHARS`], counted in characters.
    #[error("terminal label is {length} characters, over the {max}-character limit")]
    LabelTooLong {
        /// The length that was measured, after trimming.
        length: usize,
        /// The ceiling it exceeded.
        max: usize,
    },
    /// The label is empty, or nothing but whitespace.
    #[error("a terminal label must not be empty")]
    EmptyLabel,
}

/// In-memory metadata for every terminal this process knows about.
///
/// Keys are sorted, so iteration — and therefore [`Store::expiring`] — is deterministic. A cleanup
/// pass that reports terminals in hash order would be hard to read in a log and hard to pin in a
/// test.
#[derive(Debug, Default)]
pub struct Store {
    /// The metadata, by terminal id.
    metas: BTreeMap<String, Meta>,
    /// When each closed terminal closed, by id.
    ///
    /// Held beside the metadata rather than inside [`Meta`] because closing is something that happens
    /// to a terminal and metadata is something that is recorded about one; folding them together
    /// would make every field of `Meta` mutable state that a close has to touch.
    closed_at: BTreeMap<String, u64>,
}

impl Store {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a terminal, replacing any metadata already held for its id.
    ///
    /// The value is taken as given, including its label: this method has no way to report a rejected
    /// name, and a store that silently truncated one would hand back a title nobody typed. Labels
    /// that come from a user go through [`Store::rename`], which validates.
    ///
    /// A close time already recorded for the id is kept. Re-inserting metadata is a refresh of what
    /// is known, not a resurrection: a terminal that has expired must not become live again because
    /// its label was re-sent.
    pub fn insert(&mut self, meta: Meta) {
        self.metas.insert(meta.id.clone(), meta);
    }

    /// Gives a terminal a name, replacing any previous one.
    ///
    /// The name is trimmed before it is stored, and the trimmed form is what is kept. A trailing
    /// space from a text box is not information, and two labels that render identically but compare
    /// differently would be indistinguishable to every caller that later sorts or matches them.
    ///
    /// # Errors
    ///
    /// [`MetaError::Unknown`] when no terminal has this id, [`MetaError::EmptyLabel`] when the name
    /// is empty or only whitespace, and [`MetaError::LabelTooLong`] when it exceeds
    /// [`MAX_LABEL_CHARS`] characters.
    pub fn rename(&mut self, id: &str, label: &str) -> Result<(), MetaError> {
        let checked = check_label(label)?;
        let meta = self
            .metas
            .get_mut(id)
            .ok_or_else(|| MetaError::Unknown { id: id.to_owned() })?;
        meta.label = Some(checked.to_owned());
        Ok(())
    }

    /// Sets how long a terminal's output is kept after it closes.
    ///
    /// # Errors
    ///
    /// [`MetaError::Unknown`] when no terminal has this id.
    pub fn set_retention(&mut self, id: &str, retention: Retention) -> Result<(), MetaError> {
        let meta = self
            .metas
            .get_mut(id)
            .ok_or_else(|| MetaError::Unknown { id: id.to_owned() })?;
        meta.retention = retention;
        Ok(())
    }

    /// Records that a terminal closed at `now_ms`.
    ///
    /// This is the event every retention window starts from; without it a terminal has no deadline
    /// and is never expiring.
    ///
    /// Only the first close counts. Closing an already-closed terminal succeeds and changes nothing,
    /// because a repeated close is exactly what a cleanup pass or a retried RPC produces, and letting
    /// each one restart the window would keep an expiring terminal alive for as long as anything
    /// kept mentioning it.
    ///
    /// # Errors
    ///
    /// [`MetaError::Unknown`] when no terminal has this id.
    pub fn close(&mut self, id: &str, now_ms: u64) -> Result<(), MetaError> {
        if !self.metas.contains_key(id) {
            return Err(MetaError::Unknown { id: id.to_owned() });
        }
        self.closed_at.entry(id.to_owned()).or_insert(now_ms);
        Ok(())
    }

    /// The metadata for one terminal.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Meta> {
        self.metas.get(id)
    }

    /// The terminals whose retention window has elapsed at `now_ms`.
    ///
    /// Sorted by id, so a cleanup pass reports the same order on every run. A terminal that is still
    /// open is never included: its output is not garbage, it is the output of a running process.
    ///
    /// This answers *what may be deleted now*; it does not delete. The caller owns the transcript and
    /// therefore owns the deletion, and a store that removed its own entry would leave an id that
    /// still answers at the RPC layer with no metadata behind it.
    #[must_use]
    pub fn expiring(&self, now_ms: u64) -> Vec<&Meta> {
        let mut expiring = Vec::new();
        for (id, meta) in &self.metas {
            let Some(closed_ms) = self.closed_at.get(id) else {
                continue;
            };
            let Some(deadline) = meta.retention.expires_at(*closed_ms) else {
                continue;
            };
            if now_ms >= deadline {
                expiring.push(meta);
            }
        }
        expiring
    }

    /// How many terminals the store knows about.
    #[must_use]
    pub fn len(&self) -> usize {
        self.metas.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.metas.is_empty()
    }
}

/// Validates a user-supplied label and returns the form that should be stored.
///
/// The order of the checks is deliberate: an empty name is reported as empty rather than as
/// "0 characters, which is under the limit", and the length is measured after trimming so that a
/// name made entirely of spaces is empty rather than 3 characters long.
fn check_label(label: &str) -> Result<&str, MetaError> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err(MetaError::EmptyLabel);
    }
    let length = trimmed.chars().count();
    if length > MAX_LABEL_CHARS {
        return Err(MetaError::LabelTooLong {
            length,
            max: MAX_LABEL_CHARS,
        });
    }
    Ok(trimmed)
}
