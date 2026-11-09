//! The agent inbox: durable queues of input waiting for the loop.
//!
//! An agent does not process input the moment it arrives. A message sent while a tool call is running
//! belongs to the *next* step; one sent while the model is thinking belongs to the next *turn*. The
//! inbox is where that distinction lives, and it has to be durable, because a restart mid-turn must
//! not lose what the user typed.
//!
//! # Two queues, and why
//!
//! | queue | when it is consumed |
//! |---|---|
//! | [`Target::NextTurn`] | after the current turn finishes |
//! | [`Target::NextStep`] | after the current step finishes |
//!
//! A user typing mid-answer goes to `next-step`: the model should see it before it decides it is
//! done. Context injected by a tool goes to `next-turn`: it describes something that already
//! happened, and interrupting the current step with it would be wrong.
//!
//! # Why the log records splices rather than messages
//!
//! An inbox is mutable — input can be edited or withdrawn before it is consumed. Recording "message
//! added" and "message withdrawn" as separate events would make the fold depend on order in a way
//! that is hard to validate. A splice records the whole edit as one operation: at position `start`,
//! remove `count`, insert these. Replaying a sequence of splices is deterministic, and an invalid one
//! is detectable at the point it appears rather than as a corrupted state later.
//!
//! # The invariant
//!
//! No message id may appear twice across both queues. A duplicate would be consumed twice, and the
//! model would see the same instruction as two separate messages — or, worse, a re-sent message would
//! look like the user repeating themselves.

use crate::message::Message;

/// Which queue a splice targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// Consumed after the current turn finishes.
    NextTurn,
    /// Consumed after the current step finishes.
    NextStep,
}

impl Target {
    /// The wire name, matching the upstream spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NextTurn => "next-turn",
            Self::NextStep => "next-step",
        }
    }
}

/// One edit to a queue.
///
/// The whole edit as one operation rather than a sequence of add/remove events: replaying splices is
/// deterministic, and an invalid one is detectable where it appears.
#[derive(Clone, Debug, PartialEq)]
pub struct Splice {
    /// Which queue.
    pub target: Target,
    /// 0-based position to edit at.
    pub start: usize,
    /// How many entries to remove, starting at `start`.
    pub removed: usize,
    /// Entries to insert at `start`.
    pub inserted: Vec<Message>,
}

impl Splice {
    /// Appends messages to a queue.
    #[must_use]
    pub fn append(target: Target, messages: Vec<Message>) -> Self {
        Self {
            target,
            start: 0,
            removed: 0,
            inserted: messages,
        }
    }

    /// Removes `count` messages starting at `start`.
    #[must_use]
    pub fn remove(target: Target, start: usize, count: usize) -> Self {
        Self {
            target,
            start,
            removed: count,
            inserted: Vec::new(),
        }
    }
}

/// Why a splice was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InboxError {
    /// The splice addresses a position past the end of the queue.
    #[error("splice at {start} removing {removed} is outside a queue of {len}")]
    OutOfRange {
        /// The requested start.
        start: usize,
        /// The requested removal count.
        removed: usize,
        /// The queue's length.
        len: usize,
    },

    /// The splice would make one message id appear twice.
    #[error("message `{id}` would be pending twice")]
    Duplicate {
        /// The duplicated id.
        id: String,
    },
}

/// The pending queues.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inbox {
    next_turn: Vec<Message>,
    next_step: Vec<Message>,
}

impl Inbox {
    /// Creates an empty inbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The messages waiting for the next turn.
    #[must_use]
    pub fn next_turn(&self) -> &[Message] {
        &self.next_turn
    }

    /// The messages waiting for the next step.
    #[must_use]
    pub fn next_step(&self) -> &[Message] {
        &self.next_step
    }

    /// Whether both queues are empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.next_turn.is_empty() && self.next_step.is_empty()
    }

    /// Total pending messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.next_turn.len() + self.next_step.len()
    }

    /// Applies a splice, returning the previous state so a caller can undo it.
    ///
    /// # Errors
    ///
    /// Returns [`InboxError::OutOfRange`] for a splice past the end, and [`InboxError::Duplicate`]
    /// when the result would hold one id twice.
    pub fn apply(&mut self, splice: &Splice) -> Result<Inbox, InboxError> {
        let previous = self.clone();

        // Read both queues before taking a mutable borrow: the duplicate check spans the target queue
        // and the other one, and holding a `&mut` to one while reading the other does not compile.
        let (target_queue, other_queue) = match splice.target {
            Target::NextTurn => (&self.next_turn, &self.next_step),
            Target::NextStep => (&self.next_step, &self.next_turn),
        };

        // Range and duplicate checks happen before mutating anything, so a refused splice leaves the
        // inbox untouched rather than half-applied.
        let end = splice.start.saturating_add(splice.removed);
        if splice.start > target_queue.len() || end > target_queue.len() {
            return Err(InboxError::OutOfRange {
                start: splice.start,
                removed: splice.removed,
                len: target_queue.len(),
            });
        }

        let mut candidate = target_queue.clone();
        candidate.splice(splice.start..end, splice.inserted.iter().cloned());

        // The invariant spans both queues: a message must not be pending in one and waiting in the
        // other.
        let mut seen = std::collections::BTreeSet::new();
        for message in candidate.iter().chain(other_queue.iter()) {
            if !seen.insert(message.id.to_string()) {
                return Err(InboxError::Duplicate {
                    id: message.id.to_string(),
                });
            }
        }

        match splice.target {
            Target::NextTurn => self.next_turn = candidate,
            Target::NextStep => self.next_step = candidate,
        }
        Ok(previous)
    }

    /// Drains the next-turn queue, in order.
    pub fn take_turn(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.next_turn)
    }

    /// Drains the next-step queue, in order.
    pub fn take_step(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.next_step)
    }

    /// Restores a previous state.
    ///
    /// Used when a caller took input and then failed before consuming it, so the input is not lost.
    pub fn restore(&mut self, previous: Inbox) {
        *self = previous;
    }
}

/// Replays a sequence of splices, as a session log would.
///
/// # Errors
///
/// Returns the first splice that is invalid, with the index of the splice rather than the session
/// sequence number: this layer does not know where the splice was stored.
pub fn replay(splices: &[Splice]) -> Result<Inbox, (usize, InboxError)> {
    let mut inbox = Inbox::new();
    for (index, splice) in splices.iter().enumerate() {
        inbox.apply(splice).map_err(|error| (index, error))?;
    }
    Ok(inbox)
}
