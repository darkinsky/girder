//! Tests for the one turn-closure verdict every reader of the log shares.
//!
//! The module exists because two functions answered "is a turn open?" differently for the same log,
//! and `session.state` could report `interrupted: false` beside `openTurn: 1`. So these tests are
//! written around the three properties that failure violated, in the order they matter:
//!
//! - **one verdict, not two** — [`is_interrupted`] and [`open_turn_of`] are asserted against each
//!   other on every fixture, including the malformed ones where "no turn is open" has to be said
//!   without claiming a pairing the log does not support
//! - **an end naming a different turn is damage** — not an open turn, and not a close either; the
//!   mis-paired log is the exact input the two old readers disagreed on, so it gets the most cases
//! - **records pair by turn number** — a start or end is matched by the number it carries, which is
//!   what makes interleaved and back-to-back turns come out right instead of "last boundary wins"
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in, which is the same reason the rest of
//! this crate's module tests reach their file under `src/` this way. It spells its import through
//! `girder_core::`, the alias the library declares for itself, so one source builds in both places.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/turn_closure.rs"]
mod turn_closure;

use girder_core::{SessionEvent, TurnEndReason};

use turn_closure::{Closure, evaluate, is_interrupted, open_turn_of};

// ---- fixtures ---------------------------------------------------------------------------------

/// A `turn/start` for `turn`.
fn start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

/// A `turn/end` for `turn`.
///
/// The reason is always `Completed`: none of the rules here read it, and a test that varied it would
/// be asserting behaviour the module does not have.
fn end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    }
}

// ---- the pair that must never disagree --------------------------------------------------------

/// Assert the derived answers agree with [`evaluate`], for one log.
///
/// Every test ends here. The two functions are defined from `evaluate`, so this can only fail if that
/// derivation is broken — which is exactly the regression worth catching, because the original defect
/// was two functions that each looked right on their own.
fn assert_derived_answers_agree(events: &[SessionEvent], closure: &Closure) {
    let expected_turn = match closure {
        Closure::Open { turn } => Some(*turn),
        Closure::Closed | Closure::Malformed { .. } => None,
    };

    assert_eq!(
        open_turn_of(events),
        expected_turn,
        "openTurn must be the verdict's turn, and nothing else"
    );
    assert_eq!(
        is_interrupted(events),
        expected_turn.is_some(),
        "interrupted must mean exactly that a turn is open"
    );
    // The property stated as the contradiction it prevents: one response cannot claim a turn is open
    // and that nothing is interrupted.
    assert_eq!(
        is_interrupted(events),
        open_turn_of(events).is_some(),
        "interrupted and openTurn must describe the same log"
    );
}

// ---- empty ------------------------------------------------------------------------------------

#[test]
fn an_empty_log_closes_nothing_and_opens_nothing() {
    // A session created and never used is a normal state, not a missing one. Its verdict is `Closed`
    // in the sense that no turn is outstanding, and both derived answers say "nothing running".
    let events: Vec<SessionEvent> = Vec::new();

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Closed);
    assert_derived_answers_agree(&events, &closure);
}

// ---- ordinary closure -------------------------------------------------------------------------

#[test]
fn a_start_and_its_end_are_closed() {
    let events = [start(1), end(1)];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Closed);
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn two_consecutive_turns_are_closed_and_neither_vouches_for_the_other() {
    // Back-to-back turns are the normal shape of a session that ran twice. The second turn's end must
    // not be what closes the first: if pairing were positional rather than by number, this log and
    // the mis-paired log below would be indistinguishable.
    let events = [start(1), end(1), start(2), end(2)];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Closed);
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn unrelated_events_between_the_boundaries_change_nothing() {
    // Real logs carry steps and messages between the boundaries. A reader that cleared or opened on
    // anything other than a boundary would report an open turn for a completed one.
    let events = [
        start(1),
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        end(1),
    ];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Closed);
    assert_derived_answers_agree(&events, &closure);
}

// ---- unclosed: the core case ----------------------------------------------------------------

#[test]
fn a_start_with_no_end_is_open() {
    let events = [start(1)];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Open { turn: 1 });
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn an_unfinished_turn_after_a_finished_one_is_open() {
    // The case a crash leaves behind: turn 1 completed, turn 2 was cut off. Reporting `Closed` here
    // would tell a client the session is idle while its last turn never finished.
    let events = [start(1), end(1), start(2)];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Open { turn: 2 });
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn work_inside_an_unclosed_turn_does_not_close_it() {
    let events = [
        start(3),
        SessionEvent::StepStart { turn: 3, step: 1 },
        SessionEvent::StepEnd { turn: 3, step: 1 },
    ];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Open { turn: 3 });
    assert_derived_answers_agree(&events, &closure);
}

// ---- malformed: the other core case ---------------------------------------------------------

#[test]
fn an_end_naming_a_different_turn_is_malformed_not_a_close() {
    // This is the exact log the two old readers disagreed on. `ends_mid_turn` cleared its flag on any
    // end, so `session.state` said `interrupted: false`; `open_turn` compared numbers and said turn 1
    // was still open. Neither answer is usable: the log is damaged, and the verdict has to say so.
    let events = [start(1), end(2)];

    let closure = evaluate(&events);

    assert!(
        matches!(closure, Closure::Malformed { .. }),
        "an end for a turn that never started is damage, got {closure:?}"
    );
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn the_malformed_verdict_names_the_turn_that_was_never_opened() {
    let events = [start(1), end(7)];

    match evaluate(&events) {
        Closure::Malformed { detail } => {
            assert!(
                detail.contains('7'),
                "the reason must name the unpaired turn, got {detail:?}"
            );
        }
        other => panic!("expected Malformed for turn 7, got {other:?}"),
    }
}

#[test]
fn ending_the_same_turn_twice_is_malformed() {
    // The other way a log corrupts. Folding the second end into "already closed" would report a log
    // that says two contradictory things as if it said one, and the extra end would explain nothing.
    let events = [start(1), end(1), end(1)];

    let closure = evaluate(&events);

    match &closure {
        Closure::Malformed { detail } => assert!(
            detail.contains("more than once"),
            "a doubled end must be told apart from an end with no start, got {detail:?}"
        ),
        other => panic!("expected Malformed for a doubled end, got {other:?}"),
    }
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn an_end_with_no_start_at_all_is_malformed() {
    let events = [end(1)];

    let closure = evaluate(&events);

    assert!(
        matches!(closure, Closure::Malformed { .. }),
        "an end with no start is damage, got {closure:?}"
    );
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn malformed_is_not_reported_as_closed() {
    // Stated as its own assertion because it is the mistake the module was written to prevent: a
    // caller branching only on closed-versus-open treats corruption as a clean close, and then a
    // damaged log renders as an idle session.
    let events = [start(1), end(2)];

    assert_ne!(evaluate(&events), Closure::Closed);
    assert!(
        !is_interrupted(&events),
        "a malformed log must not claim a turn is running"
    );
    assert_eq!(
        open_turn_of(&events),
        None,
        "a malformed log must not name an open turn it cannot vouch for"
    );
}

// ---- interleaved turns ------------------------------------------------------------------------

#[test]
fn interleaved_turns_close_the_turn_that_ended_not_the_last_one_seen() {
    // Turns 1 and 2 overlap, and 2 ends first. A single-slot reader would set the open turn to 2 and
    // then clear it, reporting nothing running while turn 1 is still open.
    let events = [start(1), start(2), end(2)];

    let closure = evaluate(&events);

    assert_eq!(
        closure,
        Closure::Open { turn: 1 },
        "turn 1 is the one still open, and no single end closes two turns"
    );
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn interleaved_turns_that_all_end_are_closed() {
    let events = [start(1), start(2), end(1), end(2)];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Closed);
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn the_open_turn_is_the_most_recently_started_one() {
    // With two turns open, "what is happening now" is the later one, and naming it keeps the answer
    // stable regardless of the order the set happens to iterate in.
    let events = [start(1), start(2)];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Open { turn: 2 });
    assert_derived_answers_agree(&events, &closure);
}

#[test]
fn a_repeated_start_for_the_same_turn_is_the_same_fact_restated() {
    // Every reader in this crate already treats a repeated start as the turn being open. Calling it
    // malformed here would reject logs the rest of the code accepts, without learning anything.
    let events = [start(1), start(1)];

    let closure = evaluate(&events);

    assert_eq!(closure, Closure::Open { turn: 1 });
    assert_derived_answers_agree(&events, &closure);
}

// ---- consistency, exhaustively ----------------------------------------------------------------

#[test]
fn the_derived_answers_agree_across_every_fixture() {
    // The guard against the original defect returning in a new form: whatever the verdict is, the two
    // answers a single response carries are one answer. Iterating the fixtures together makes a future
    // case added to this list participate automatically.
    let fixtures: Vec<Vec<SessionEvent>> = vec![
        vec![],
        vec![start(1)],
        vec![start(1), end(1)],
        vec![start(1), end(2)],
        vec![start(1), end(1), start(2)],
        vec![start(1), end(1), start(2), end(2)],
        vec![start(1), start(1)],
        vec![start(1), end(1), end(1)],
        vec![end(1)],
        vec![start(1), start(2)],
        vec![start(1), start(2), end(2)],
        vec![start(1), start(2), end(1), end(2)],
    ];

    for events in &fixtures {
        let closure = evaluate(events);
        assert_derived_answers_agree(events, &closure);
    }
}
