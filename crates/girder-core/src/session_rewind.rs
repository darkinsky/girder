//! Turning the append-only session log back into a conversation the user can stand on.
//!
//! The session log is append-only on purpose — see [`crate::session`] — and that is what makes
//! replay, crash recovery, and fork the same operation. But an append-only log leaves the *user*
//! without the one motion they expect from a chat surface: taking back the last thing that happened.
//! This module computes that motion. It is pure: it reads a slice of events and returns a shorter
//! slice, so nothing here writes, deletes, or recompresses anything on disk.
//!
//! # Why the cut is a turn boundary and never a position
//!
//! A turn is not one model call. It is a sequence of steps, each one model call plus the tool
//! executions that call requested — so the events between `turn/start` and `turn/end` are a
//! *transaction*: a `user/message`, an `assistant/message` carrying a `tool/call`, the matching
//! `tool/result`, another `assistant/message`, and so on. Cutting between the call and its result
//! yields a log whose `tool_use` has no answer, which every provider rejects; cutting between the
//! user's prompt and the reply yields a turn that opened and never closed, which a replay renders as
//! a pending assistant message forever. Both are logs no reader in this crate can resume, so this
//! module never produces one: the only points it offers are the events where a turn closed.
//!
//! [`crate::remote`]'s `fork` reaches the same conclusion from the other direction — a cut that lands
//! mid-turn is moved backwards to the nearest `turn/end` — and the two agreeing is not a coincidence:
//! "the last point a replay can resume from" is one property, and a fork and a rewind differ only in
//! where the surviving prefix is written.
//!
//! # Why this belongs here even though upstream's snapshot package is a test tool
//!
//! `packages/test-support/session-snapshot/` upstream is *not* a rewind feature; it is the record /
//! replay harness behind `pnpm run test:snapshot`, and it exists to make a session log comparable
//! across runs, not to shorten one. What it does establish is the fact this module depends on: a
//! session log is a durable, ordered, self-describing sequence whose turn boundaries are explicit in
//! the data rather than inferred by a reader. Once that is true, "give me the log as it stood at the
//! end of turn N" is a pure function of the log rather than a feature of the writer — which is
//! exactly what makes rewind implementable without touching persistence at all.
//!
//! The user-facing need is real and independent of the test tool: undoing the last exchange is the
//! one operation a conversation surface is expected to offer, and the honest way to offer it is to
//! keep the events the user wants and drop the ones they do not, rather than to mutate state in
//! place.
//!
//! # Why this does not write
//!
//! [`rewind_to`] returns a `Vec`, and `SessionStore` has no truncate: logs are compressed JSONL
//! (`.jsonl.zst`) appended frame by frame, so "removing the tail" is not a file operation the
//! current writer offers — a truncated frame is indistinguishable from an interrupted write. Writing
//! the result is therefore the caller's decision and the caller's format problem: the usual move is
//! to write the returned prefix as a new session (which is what `session.fork` already does), and a
//! future in-place rewind would need a writer that can rewrite a zstd frame, not a different cut.

use girder_core::SessionEvent;

/// A point the log can be rewound to.
///
/// Produced only by [`checkpoints`] and [`checkpoint_for_turn`], so a `Checkpoint` always names an
/// event that closed a turn in the log it came from. Handing one to [`rewind_to`] together with a
/// *different* log is tolerated rather than trusted: see [`rewind_to`] for what happens then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The sequence number of the last event kept, inclusive.
    ///
    /// This is the cursor [`crate::SessionStore::read_after`] expects: sequence numbers in this crate
    /// are one-based, so `after_seq = n` means "everything after the nth event" and a log truncated
    /// to a checkpoint is exactly "everything up to and including event n".
    pub after_seq: u64,

    /// The turn this point closed, as the `turn/end` event named it.
    ///
    /// Display metadata, not an address: a resumed or forked log can carry turn numbers that no
    /// longer track append order, so two checkpoints may share a `turn`. [`rewind_to`] cuts by
    /// position for that reason — the position is unambiguous where the number is not.
    pub turn: u64,

    /// How many events the log held at that moment.
    ///
    /// For every log this crate writes — starting at sequence 1 with no gaps — this equals
    /// [`Checkpoint::after_seq`], and the two can never disagree. Both are kept because they are two
    /// spellings of one position that different call sites need: `after_seq` is what a cursor-based
    /// reader takes, `events` is what slicing a `Vec` takes. Naming each one at its call site is
    /// cheaper than making every caller convert and getting it off by one.
    ///
    /// Both positions are derived from the same index, so a checkpoint can only ever describe one
    /// place in the log. If a future log format introduces gaps or sequence offsets, this is the
    /// invariant that breaks first, and [`checkpoints`] is the only place that has to change.
    pub events: u64,
}

/// Finds every point the log can be rewound to: the end of each turn.
///
/// One checkpoint per `turn/end` event, in log order. Turns that opened and never closed contribute
/// nothing — that is the whole point, since the closing event is what makes the prefix resumable.
///
/// An empty log, or a log holding only an unfinished turn, yields an empty vector rather than an
/// error: "there is nowhere to go back to" is an answer, and a first turn still in flight is the
/// normal state of a session that is running.
#[must_use]
pub fn checkpoints(events: &[SessionEvent]) -> Vec<Checkpoint> {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            SessionEvent::TurnEnd { turn, .. } => {
                // One-based, because every cursor in this crate is.
                let position = u64::try_from(index + 1).unwrap_or(u64::MAX);
                Some(Checkpoint {
                    after_seq: position,
                    turn: *turn,
                    events: position,
                })
            }
            _ => None,
        })
        .collect()
}

/// Finds the most recent point at which `turn` closed.
///
/// The *last* match, not the first: after a fork or a resume a turn number can appear more than once,
/// and the point a user means by "go back to turn 3" is the one the conversation most recently
/// reached, not the earliest occurrence of the label.
///
/// Returns `None` when that turn never closed — still running, cancelled before the closing event was
/// durable, or simply not in this log.
#[must_use]
pub fn checkpoint_for_turn(events: &[SessionEvent], turn: u64) -> Option<Checkpoint> {
    events
        .iter()
        .enumerate()
        .rfind(|(_, event)| matches!(event, SessionEvent::TurnEnd { turn: ended, .. } if *ended == turn))
        .map(|(index, _)| {
            let position = u64::try_from(index + 1).unwrap_or(u64::MAX);
            Checkpoint {
                after_seq: position,
                turn,
                events: position,
            }
        })
}

/// Returns the log truncated to `checkpoint`, as a new vector.
///
/// The result is a prefix: it ends with the `turn/end` the checkpoint names, so the caller holds a
/// log that a replay can resume from and a provider will accept. Nothing on disk is touched; see the
/// module docs for why writing is the caller's job.
///
/// # Out-of-range checkpoints are clamped, not rejected
///
/// `after_seq` past the end of `events` is the expected outcome of a stale checkpoint — the log was
/// appended to, or the caller handed in a different session — not a programming error worth an
/// exception on a user's "undo" button. This signature has no error channel by design, so the
/// behavior is defined instead of surprising:
///
/// 1. The requested position is clamped to the length of the log.
/// 2. The cut is then moved *backwards* to the nearest `turn/end` at or before it.
///
/// Step 2 is what makes the clamp safe. Clamping alone could leave the cut in the middle of a turn
/// — the log may have grown a new turn whose `turn/start` is past the checkpoint but whose earlier
/// events are not — and a prefix ending mid-turn is precisely the log this module refuses to
/// produce. So the guarantee is unconditional: **the returned vector is empty, or it ends with a
/// `turn/end`.** A checkpoint that names no reachable boundary (including `after_seq = 0`, and any
/// checkpoint taken against a log with no completed turn) yields an empty vector, which is the
/// correct answer for "rewind to before anything happened".
///
/// Rejecting the input with a `Result` was the alternative. It would push a decision the user cannot
/// act on into every call site: the caller would have to choose between an error dialog and the
/// clamp anyway, and the clamp it would write is this one.
#[must_use]
pub fn rewind_to(events: &[SessionEvent], checkpoint: &Checkpoint) -> Vec<SessionEvent> {
    // Clamp first: a stale cursor must not index past the log.
    let requested = usize::try_from(checkpoint.after_seq).unwrap_or(usize::MAX);
    let clamped = requested.min(events.len());

    // Then walk back to the boundary. `clamped` is one past the last kept event, so the search starts
    // at the last event itself.
    let cut = (0..clamped)
        .rev()
        .find(|index| matches!(events.get(*index), Some(SessionEvent::TurnEnd { .. })))
        .map_or(0, |index| index + 1);

    events.iter().take(cut).cloned().collect()
}
