//! Whether a turn in a session log is closed, open, or damaged — one verdict, computed once.
//!
//! # The defect this module exists to remove
//!
//! Two implementations answered this question about the same log, and they disagreed:
//!
//! | reader | rule |
//! |--------|------|
//! | `ends_mid_turn` in `session_state_rpc` and `session_resume_rpc` | any `turn/start` sets the flag, any `turn/end` clears it — the turn number is never compared |
//! | `open_turn` in `session_timeline` | clears only when the ending turn number equals the open one |
//!
//! A log with a `turn/end` naming a *different* turn is answered twice: the first rule says "closed",
//! the second says "turn 1 is still open". `session.state` then reports `interrupted: false` beside
//! `openTurn: 1`, and a client that renders both has to choose which of two fields on one response to
//! call a lie. The two readers were each defensible in isolation; the split is the bug.
//!
//! The fix is structural rather than a third rule: [`evaluate`] is the only implementation, and
//! [`is_interrupted`] and [`open_turn_of`] are defined from it. They are now two spellings of one
//! verdict, so they cannot drift apart again — a future change lands in one place or it does not land.
//!
//! # The rule
//!
//! Records are paired by turn number, and only by turn number:
//!
//! - a `turn/end` closes the open turn with the same number;
//! - a `turn/end` whose number is not open is corruption, and is reported as such rather than folded
//!   into "closed" or ignored;
//! - whatever is still open at the end of the log is the open turn, and the *most recently started*
//!   one is named, because that is the turn a reader means by "what is happening now".
//!
//! # Why malformation is reported instead of smoothed over
//!
//! "No open turn" and "the log no longer says" are different facts with different remedies, exactly
//! as with an empty versus a torn log. Treating a stray `turn/end` as a close would report a turn as
//! finished that never finished — the first rule above, and the reason `interrupted` could be wrong.
//! Reporting [`Closure::Malformed`] keeps the difference until a caller decides what to do with it.
//!
//! For that reason the pair [`is_interrupted`] / [`open_turn_of`], which answer only the "is a turn
//! open" question, answer *no* for a malformed log: naming a turn there would assert a pairing the
//! log no longer supports. A caller that must not mistake damage for a clean close reads [`evaluate`].
//!
//! # Interleaved turns
//!
//! Normal logs open one turn at a time, but a forked or merged log can interleave them. Open turns are
//! therefore tracked as a set rather than a single slot, so `start 1, start 2, end 2` leaves turn 1
//! open instead of reporting "nothing runs" because the last boundary happened to match.

use std::collections::BTreeSet;

use girder_core::SessionEvent;

/// How a sequence of events leaves its turns.
///
/// Three outcomes, not two, because "all turns are closed" and "the log cannot say" are different
/// answers. A caller that only branches on closed-versus-open will treat corruption as a clean close,
/// which is the mistake this module was written to end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Closure {
    /// Every turn that started also ended.
    Closed,

    /// A turn started and nothing closed it.
    Open {
        /// Which turn, by the number the log pairs its records on.
        ///
        /// With more than one turn open, the most recently started one.
        turn: u64,
    },

    /// The log is malformed: a turn ended twice, or ended without having started.
    ///
    /// Carries the reason as text because the two faults look identical in a verdict enum and a
    /// person reading a damaged log needs the turn number and which fault it was.
    Malformed {
        /// What went wrong, in one line.
        detail: String,
    },
}

/// Decide the turn closure of `events`.
///
/// This is the only implementation of the question; [`is_interrupted`] and [`open_turn_of`] are
/// derived from it so that no two readers of the same log can give contradicting answers.
///
/// Turns are paired by number. A `turn/end` naming a turn that is not open is reported as
/// [`Closure::Malformed`] — first fault wins, and scanning stops there, because the turn numbers after
/// the first unpaired record can no longer be trusted to describe the same session.
///
/// A repeated `turn/start` for a turn already open is tolerated rather than called malformed: the
/// pairing key is the turn number, so the turn is open either way, and every reader in this crate
/// already treats the repeat as the same fact restated. Turning that into corruption would reject
/// logs the rest of the code accepts, without learning anything new.
#[must_use]
pub fn evaluate(events: &[SessionEvent]) -> Closure {
    // Open turns in start order. A `Vec` rather than a set because "which turn is open" has more than
    // one candidate when turns interleave, and start order is what records which is the most recent.
    let mut open: Vec<u64> = Vec::new();
    // Every turn number that ever started. Only used to say *why* an end is malformed: ending a closed
    // turn twice and ending a turn that never started are both corruption, but they are different
    // faults and a reader debugging a log needs to know which one it is looking at.
    let mut started: BTreeSet<u64> = BTreeSet::new();

    for event in events {
        match event {
            SessionEvent::TurnStart { turn } => {
                if !open.contains(turn) {
                    open.push(*turn);
                }
                started.insert(*turn);
            }
            SessionEvent::TurnEnd { turn, .. } => {
                // Compared by number, which is how the log pairs the two records. Clearing on any end,
                // regardless of number, is what let `interrupted` and `openTurn` disagree: an end
                // naming a different turn says nothing about this one.
                if let Some(position) = open.iter().position(|candidate| candidate == turn) {
                    open.remove(position);
                } else if started.contains(turn) {
                    return Closure::Malformed {
                        detail: format!("turn {turn} ended more than once"),
                    };
                } else {
                    return Closure::Malformed {
                        detail: format!("turn {turn} ended without a matching start"),
                    };
                }
            }
            _ => {}
        }
    }

    match open.last() {
        Some(turn) => Closure::Open { turn: *turn },
        None => Closure::Closed,
    }
}

/// The turn that is open, if the log supports naming one.
///
/// Derived from [`evaluate`] rather than recomputed, so this answer and [`is_interrupted`] cannot
/// describe different logs.
///
/// [`None`] for a malformed log: the numbers after the first unpaired record no longer say which turn
/// was running, and returning a guess would put a confident wrong number in `openTurn`. Use
/// [`evaluate`] to tell "nothing is open" from "the log is damaged".
#[must_use]
pub fn open_turn_of(events: &[SessionEvent]) -> Option<u64> {
    match evaluate(events) {
        Closure::Open { turn } => Some(turn),
        Closure::Closed | Closure::Malformed { .. } => None,
    }
}

/// Whether a turn is still open — the answer `session.state.interrupted` needs.
///
/// Defined as `open_turn_of(events).is_some()` on purpose. Some flag and some turn number are two
/// renderings of one verdict; computing them separately is precisely how the two disagreed before, so
/// here the equality is not a property to be tested but the definition itself. Both are `false` /
/// [`None`] for a malformed log, which answers "no turn is open" and leaves the damage to
/// [`evaluate`].
#[must_use]
pub fn is_interrupted(events: &[SessionEvent]) -> bool {
    open_turn_of(events).is_some()
}
