//! Choosing where a crashed session continues from, in the event log.
//!
//! # What the crate already answers, and what it did not
//!
//! Two modules already look at a damaged or reopened log, and neither of them answers this question:
//!
//! - [`crate::session_recovery`] answers a question about **bytes**. It walks zstd frames, finds the
//!   offset at which the file stops decoding, and cuts the torn append away. A log that decodes
//!   perfectly while ending in the middle of a tool call is `Recovery::Healthy` there, because
//!   nothing is wrong with the file.
//! - `session.resume` (see the `session_resume_rpc` module) answers "is this session usable here,
//!   and where does its log end". Its answer is a *cursor at the end of the log*: `events` and
//!   `lastSeq` are what a follow or page subscription starts from, so a client that reconnects
//!   repeats nothing and misses nothing. It additionally reports `interrupted` — "a `turn/start`
//!   came after the last `turn/end`" — but it deliberately does not act on that answer.
//!
//! So the position from which *execution* continues was, until this module, either "the end of the
//! log" (the resume cursor) or "everything up to the last `turn/end`" (the cut
//! `session.fork` and [`crate::session_rewind::rewind_to`] compute). Both are *prefixes to keep*.
//! Neither is a statement about **where a restarted process may begin writing again**, which is a
//! different number, and the difference is the whole subject here: a prefix ends at the last event
//! kept, while a resume point names the first event that has to be processed — the event *after*
//! that prefix.
//!
//! # Why the restart question had no answer
//!
//! A session that dies after a tool call leaves the log holding `tool/call` with no `tool/result`.
//! The loop cannot be re-entered from there: the model's history would carry a `tool_use` the
//! provider is never given an answer for, and every provider in this crate rejects that request.
//! Nothing in the log says whether the tool ran — a crash between the call being written and the
//! process exiting destroys that knowledge — so the only implementable choice is to *not* claim the
//! call happened. [`find`] therefore reports the position, [`Reason`] says why, and [`is_safe`]
//! verifies the claim instead of asserting it.
//!
//! # Why a turn boundary is the only guaranteed position
//!
//! A turn is a transaction: `turn/start`, a prompt, one or more steps (each a model call plus the
//! tools it requested), then `turn/end`. Only at `turn/end` has the writer finished everything it
//! opened. Mid-turn, any cut can separate a `tool/call` from its `tool/result` — and even a cut that
//! happens to land after every result cannot be *known* to be safe without scanning the prefix,
//! which is exactly what [`is_safe`] does. The boundary does not need the scan: the loop returns
//! from `run_steps` only after every step closed, so a written `turn/end` implies every call inside
//! its turn was answered or reported.
//!
//! That implication is a property of the writer, not of the data, so it is treated as a default
//! rather than a proof. A `turn/end` with reason `aborted` can still have been written after a
//! cancellation that skipped a tool's result, and a spliced or migrated log can hold records no
//! writer produced. This is why [`find`] and [`is_safe`] are separate calls.
//!
//! # The algorithm
//!
//! One rule, then the fallbacks for the logs it cannot apply to. Every fallback exists because the
//! rule needs something the log does not have.
//!
//! 1. **A `turn/end` exists** — resume one past the *last* one, [`Reason::TurnBoundary`]. Both the
//!    preference and the position come from the same fact: the newest boundary is the latest point
//!    at which the conversation is complete, and redoing turns that already finished would re-run
//!    their tools. This covers the case where records follow the boundary and belong to a turn that
//!    never closed: resuming on the boundary redoes that turn, which is the only reading that
//!    neither abandons the user's prompt nor invents an outcome.
//! 2. **No boundary, and every call in the log is paired** — nothing has to be redone, so the log is
//!    kept whole and the position is its end: [`Reason::CleanCallPair`]. The turn may still be
//!    unclosed (a crash between the last `tool/result` and `turn/end` looks exactly like this), and
//!    `crate::session_health` reports that as a hint rather than damage.
//! 3. **No boundary and an unanswered call inside a turn** — that turn is redone from its own
//!    `turn/start`: [`Reason::MidTurn`]. Redoing it is the only reading of a half-written tool call
//!    that neither fabricates a result nor hands a provider a `tool_use` it will reject, and keeping
//!    the turns before it is what stops a restart from replaying a whole conversation.
//! 4. **Nothing is recoverable** — no boundary, no turn to redo, and a half-written exchange leaves
//!    [`Reason::FromStart`]. An empty prefix is always safe, and saying so is more useful than
//!    refusing to answer.
//!
//! # What this module is not
//!
//! It does not repair, truncate, or write. It does not append synthetic closers (the reason is in
//! the `session_resume_rpc` docs: this build has no event that can carry "outcome unknown" as a
//! coded error, and a fabricated `tool/result` would hand the loop an observation no tool produced).
//! It reads a slice and returns a position, so calling it on a live log is a read and nothing else.

use std::collections::HashMap;

// `::girder_core::…` rather than `crate::…` so this file also builds when a test includes it by
// path; the library declares the alias for itself, which is what makes both spellings the same crate.
use ::girder_core::SessionEvent;

/// A position a restarted session can continue from.
///
/// Produced only by [`find`], so [`Point::at`] is always a position this log can be described from.
/// It is deliberately not trusted on its own: callers should ask [`is_safe`] before writing at the
/// position it names, because "the log closed a turn here" and "this prefix is a conversation a
/// provider accepts" are different sentences and the second one is the one that matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    /// Index of the first event to process when execution resumes (inclusive).
    ///
    /// Zero-based, and **not** the last completed event: an event at index `i` has already been
    /// handled, so resuming after it means starting at `i + 1`. `events.len()` therefore means
    /// "there is nothing left to replay", which is the normal answer for a log that ended on a
    /// `turn/end`.
    ///
    /// A position rather than a sequence number because this crate's sequence numbers are one-based
    /// and positional, so the two spellings differ by one; naming the index keeps this struct's
    /// meaning the same as the slice it describes, and a caller that needs a cursor converts where it
    /// asks for one instead of at every comparison.
    pub at: usize,

    /// The turn this position belongs to, or `0` when the log names no turn.
    ///
    /// For [`Reason::TurnBoundary`] it is the turn that closed at [`Point::at`] minus one — the last
    /// turn whose work is complete. For [`Reason::MidTurn`] it is the turn being redone, and for
    /// [`Reason::CleanCallPair`] the open turn the kept suffix belongs to. `0` is impossible as a
    /// written turn number — turns start at 1 — which is what makes it unambiguous as "none".
    pub turn: u64,

    /// Why this position was chosen.
    ///
    /// Carried in the struct rather than derivable from [`Point::at`] because the caller's next move
    /// differs: a boundary needs no explanation to the user, a mid-turn rewind re-runs tools, and a
    /// clean-pair resume keeps an unclosed turn as it stands.
    pub reason: Reason,
}

/// Why [`find`] chose a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A turn finished normally, and the log continues after its `turn/end`.
    ///
    /// The only position that is safe by construction: the writer had closed every step, every tool
    /// result, and the turn itself before it wrote this record.
    TurnBoundary,

    /// The records after the last boundary hold no half-written tool exchange.
    ///
    /// Nothing has to be redone, so the log is kept whole and the next prompt simply continues. The
    /// turn may still be unclosed — a crash between the last `tool/result` and `turn/end` looks
    /// exactly like this — and keeping it is deliberate: redoing a turn whose tools all completed
    /// would run those tools a second time, which is the one mistake a resume can make that the user
    /// cannot see and cannot undo.
    CleanCallPair,

    /// There is no usable position, so replay starts at the first event.
    ///
    /// Always safe and never lossless beyond what the log holds: the alternative would be to resume
    /// at a point whose prefix cannot be reconstructed, which is a conversation the model never had.
    FromStart,

    /// The log stops inside `turn`, and that turn has to be redone from its `turn/start`.
    ///
    /// Reported instead of [`Reason::FromStart`] when the open turn can be identified, so the caller
    /// can name the turn it is discarding and keep replaying the turns before it.
    MidTurn {
        /// The turn that is redone.
        turn: u64,
    },
}

/// Finds the position a restarted session continues from.
///
/// The four cases are listed in the module docs; in one line: the last `turn/end` wins, a boundary
/// with a half-written exchange after it still resumes on the boundary (the open turn is redone), a
/// turn-less log with paired calls resumes at its end, and a call with no turn to redo starts at zero.
///
/// # Why the answer is a position and not a verdict
///
/// This function never consults [`is_safe`], and deliberately so. Two questions have two answers: "the
/// log structurally ends after turn 3" is a fact about the records, and "the prefix through there is
/// a conversation a provider accepts" is a check a damaged log can fail even at a boundary. Folding
/// the check in would make [`find`] walk backwards over a broken turn and report an earlier point as
/// if the log were healthy, which is the failure an operator can never see. Keeping them apart lets a
/// caller choose: resume on a verified boundary, or surface the damage and leave the log alone.
#[must_use]
pub fn find(events: &[SessionEvent]) -> Point {
    // An empty log has nothing to keep and nothing to name: replaying it is already "from the
    // start", and reporting any other reason would describe records that are not there.
    if events.is_empty() {
        return Point {
            at: 0,
            turn: 0,
            reason: Reason::FromStart,
        };
    }

    // Case 1: the last boundary is the answer, whether or not records follow it. Anything after it
    // belongs to a turn that never closed, so resuming here redoes that turn and keeps the closed
    // ones — and past the boundary the prefix is complete by the writer's own contract.
    if let Some((index, turn)) = last_turn_end(events) {
        return Point {
            // `index` is a valid position inside the slice, so the increment cannot overflow in
            // practice; `saturating_add` keeps the arithmetic total rather than relying on that.
            at: index.saturating_add(1),
            turn,
            reason: Reason::TurnBoundary,
        };
    }

    // No boundary anywhere, so the whole log is the part in play. The open turn, when there is one,
    // is the first `turn/start`: the log never closed it, and everything from its start is what has
    // to be redone — an earlier start is the position that keeps the least history while discarding
    // all of the broken part.
    let open = open_turn_from(events, 0);

    // Case 2: every call in the log is answered, so there is nothing to redo. The log is kept whole —
    // including an unclosed turn, and including a prompt that was durable before the model answered —
    // and the caller continues at the end of the log.
    if is_paired_through(events, events.len()) {
        return Point {
            at: events.len(),
            turn: open.map_or(0, |(_, turn)| turn),
            reason: Reason::CleanCallPair,
        };
    }

    // Case 3: an unanswered call inside a turn that never closed. The whole turn is discarded from
    // its own start.
    if let Some((start, turn)) = open {
        return Point {
            at: start,
            turn,
            reason: Reason::MidTurn { turn },
        };
    }

    // Case 4: a half-written exchange with no turn to redo and no boundary to cut back to. There is
    // no position whose prefix is both non-empty and reconstructible, so the beginning is the only
    // answer that claims nothing it cannot support.
    Point {
        at: 0,
        turn: 0,
        reason: Reason::FromStart,
    }
}

/// The newest `turn/end`, with its index and the turn number it named.
///
/// Backwards, because the answer is the *last* closed turn: a log holds one boundary per finished
/// turn, and only the latest has the whole remainder of the conversation after it.
fn last_turn_end(events: &[SessionEvent]) -> Option<(usize, u64)> {
    events
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, event)| match event {
            SessionEvent::TurnEnd { turn, .. } => Some((index, *turn)),
            _ => None,
        })
}

/// The first `turn/start` at or after `from`, with its index and the turn number it named.
///
/// Returns `None` for a suffix that opens no turn — the turn-less records a session writes around
/// prompts (`session/title`, `model/selection`) do not name one.
fn open_turn_from(events: &[SessionEvent], from: usize) -> Option<(usize, u64)> {
    events
        .iter()
        .enumerate()
        .skip(from)
        .find_map(|(index, event)| match event {
            SessionEvent::TurnStart { turn } => Some((index, *turn)),
            _ => None,
        })
}

/// Whether resuming at `point` leaves no half-written tool exchange behind.
///
/// This really walks `events[..point.at]` and checks the pairing of `tool/call` against
/// `tool/result` by id; it does not assume that a [`Reason::TurnBoundary`] is safe. Three shapes fail:
///
/// - **A call with no result.** The most common crash shape: the log holds the `tool/call` and the
///   process died before the tool answered. A history carrying it is rejected by every provider.
/// - **A result with no call.** The mirror image, and evidence the log lost a record (a splice or a
///   migration): a replay that respected it would show the model answering a request it never made.
/// - **A position past the end of the log.** A stale point — taken from a longer log, or from a
///   different session — describes a prefix that does not exist, so it is not safe either.
///
/// Pairing is counted per id rather than as a total, because one result answers one call: two calls
/// that reuse an id are two requests, and a single result satisfies only one of them.
///
/// A point at `0`, an empty log, and a prefix with no tool traffic are all safe: there is nothing
/// before the position to be unbalanced.
#[must_use]
pub fn is_safe(events: &[SessionEvent], point: &Point) -> bool {
    // A position past the end is stale, not merely empty: the caller is holding a point that
    // describes a different, longer log.
    if point.at > events.len() {
        return false;
    }
    is_paired_through(events, point.at)
}

/// Whether the first `through` events hold no unanswered call and no orphan result.
///
/// `through` past the end is clamped rather than treated as an error: [`find`] asks about the whole
/// log, and a caller that sliced a log before checking it should get the same verdict for the part
/// that exists.
fn is_paired_through(events: &[SessionEvent], through: usize) -> bool {
    // Counts per id, not a single balance: see [`is_safe`] on why a repeated id is two calls.
    let mut open: HashMap<&str, u64> = HashMap::new();

    for event in events.iter().take(through) {
        match event {
            SessionEvent::ToolCall { id, .. } => {
                // Saturating because the count is diagnostic: a log with more than `u64::MAX`
                // unanswered calls for one id is not something to panic over.
                let count = open.entry(id.as_str()).or_insert(0);
                *count = count.saturating_add(1);
            }
            SessionEvent::ToolResult { id, .. } => {
                // An orphan result has no call to close, so the prefix is not a conversation a
                // provider would accept: the call it answers was never recorded.
                let Some(remaining) = open.get_mut(id.as_str()) else {
                    return false;
                };
                *remaining = remaining.saturating_sub(1);
                if *remaining == 0 {
                    open.remove(id.as_str());
                }
            }
            // Messages, boundaries, and accounting records say nothing about pairing.
            _ => {}
        }
    }

    open.is_empty()
}
