//! Tests for the rewind checkpoints.
//!
//! Two properties are load-bearing and everything else follows from them. First, a checkpoint exists
//! only where a turn *closed*, because only there is the prefix a log a replay can resume. Second,
//! [`rewind_to`] can never return a prefix that ends mid-turn — not for a stale cursor, not for an
//! out-of-range one, not for a log the checkpoint was not taken from. The out-of-range cases are
//! tested as hard as the happy path because that is where an "undo" button meets a log that moved
//! underneath it.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the module as written even before the crate root wires it in. It resolves `girder_core::…`
// through the same crate name the library uses for itself.
#[path = "../src/session_rewind.rs"]
mod session_rewind;

use girder_core::{SessionEvent, ToolCallId, TurnEndReason};
use session_rewind::{Checkpoint, checkpoint_for_turn, checkpoints, rewind_to};

/// Appends one complete turn: start, prompt, reply, end.
fn push_turn(events: &mut Vec<SessionEvent>, turn: u64) {
    events.push(SessionEvent::TurnStart { turn });
    events.push(SessionEvent::user_text(
        &format!("u{turn}"),
        format!("question {turn}"),
    ));
    events.push(SessionEvent::assistant_text(
        &format!("a{turn}"),
        format!("answer {turn}"),
        "test",
    ));
    events.push(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    });
}

/// A log of `turns` complete turns, four events each.
fn log(turns: u64) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    for turn in 1..=turns {
        push_turn(&mut events, turn);
    }
    events
}

// ---- checkpoints -------------------------------------------------------------------------------

#[test]
fn an_empty_log_has_no_checkpoints() {
    // "There is nowhere to go back to" is an answer, not an error: a brand-new session is this case.
    assert!(checkpoints(&[]).is_empty());
    assert!(checkpoint_for_turn(&[], 1).is_none());
}

#[test]
fn a_single_turn_has_one_checkpoint_at_its_end() {
    let events = log(1);

    let points = checkpoints(&events);
    assert_eq!(points.len(), 1);
    assert_eq!(
        points[0],
        Checkpoint {
            after_seq: 4,
            turn: 1,
            events: 4,
        }
    );
}

#[test]
fn an_unfinished_turn_offers_no_checkpoint() {
    // The state of a session that is running: the only legal rewind point is the previous turn.
    let mut events = log(1);
    events.push(SessionEvent::TurnStart { turn: 2 });
    events.push(SessionEvent::user_text("u2", "still running"));

    let points = checkpoints(&events);
    assert_eq!(points.len(), 1, "only turn 1 closed");
    assert_eq!(points[0].turn, 1);
    assert_eq!(points[0].after_seq, 4);
    assert!(checkpoint_for_turn(&events, 2).is_none());
}

#[test]
fn many_turns_yield_one_checkpoint_each_in_log_order() {
    let events = log(3);

    let points = checkpoints(&events);
    assert_eq!(points.len(), 3);
    assert_eq!(
        points.iter().map(|point| point.turn).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    // Four events per turn, so the boundaries are 4, 8, 12 — one-based, as every cursor is.
    assert_eq!(
        points
            .iter()
            .map(|point| point.after_seq)
            .collect::<Vec<_>>(),
        vec![4, 8, 12]
    );
    for point in &points {
        assert_eq!(
            point.events, point.after_seq,
            "the event count and the cursor are the same position"
        );
    }
}

#[test]
fn every_checkpoint_lands_on_a_turn_end() {
    // The invariant the whole module rests on, checked on a log with tool traffic in it rather than
    // on the uniform log above.
    let mut events = Vec::new();
    events.push(SessionEvent::TurnStart { turn: 1 });
    events.push(SessionEvent::user_text("u1", "go"));
    for step in 1..=3u64 {
        events.push(SessionEvent::StepStart { turn: 1, step });
        events.push(SessionEvent::assistant_text(
            &format!("a{step}"),
            "working",
            "test",
        ));
        events.push(SessionEvent::ToolCall {
            id: ToolCallId::new("call-1").expect("valid call id"),
            name: "read".to_owned(),
            arguments: "{}".to_owned(),
        });
        events.push(SessionEvent::ToolResult {
            id: ToolCallId::new("call-1").expect("valid call id"),
            content: Vec::new(),
            is_error: false,
        });
        events.push(SessionEvent::StepEnd { turn: 1, step });
    }
    events.push(SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    });

    let points = checkpoints(&events);
    assert_eq!(points.len(), 1);
    let position = usize::try_from(points[0].after_seq).expect("fits");
    assert!(matches!(
        events.get(position - 1),
        Some(SessionEvent::TurnEnd { .. })
    ));
}

// ---- checkpoint_for_turn -----------------------------------------------------------------------

#[test]
fn checkpoint_for_turn_finds_each_closed_turn() {
    let events = log(3);

    for turn in 1..=3u64 {
        let point = checkpoint_for_turn(&events, turn).expect("a checkpoint");
        assert_eq!(point.turn, turn);
        assert_eq!(point.after_seq, turn * 4);
    }
}

#[test]
fn checkpoint_for_turn_reports_a_turn_that_never_closed() {
    let events = log(2);

    // Turn 3 is not in the log at all; turn 2 closed, so only the out-of-range answer is `None`.
    assert!(checkpoint_for_turn(&events, 3).is_none());
    assert!(checkpoint_for_turn(&events, 0).is_none());
}

#[test]
fn checkpoint_for_turn_picks_the_latest_occurrence() {
    // A resumed or forked log can carry the same turn number twice. "Go back to turn 1" means the
    // point the conversation most recently reached, not the first time the label appeared.
    let mut events = log(1);
    events.push(SessionEvent::TurnStart { turn: 1 });
    events.push(SessionEvent::user_text("u1b", "again"));
    events.push(SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    });

    let point = checkpoint_for_turn(&events, 1).expect("a checkpoint");
    assert_eq!(point.after_seq, 7);
    assert_eq!(events.len(), 7);
}

// ---- rewind_to ---------------------------------------------------------------------------------

#[test]
fn rewind_to_returns_the_prefix_ending_at_the_checkpoint() {
    let events = log(3);
    let point = checkpoint_for_turn(&events, 2).expect("a checkpoint");

    let rewound = rewind_to(&events, &point);

    assert_eq!(rewound.len(), 8);
    assert_eq!(rewound, events[..8].to_vec(), "a prefix, unchanged");
    assert!(matches!(
        rewound.last(),
        Some(SessionEvent::TurnEnd { turn: 2, .. })
    ));
    // The input is untouched: this function computes, it does not edit.
    assert_eq!(events.len(), 12);
}

#[test]
fn rewinding_to_the_first_turn_drops_everything_after_it() {
    let events = log(3);
    let point = checkpoint_for_turn(&events, 1).expect("a checkpoint");

    let rewound = rewind_to(&events, &point);

    assert_eq!(rewound.len(), 4);
    assert!(matches!(
        rewound.last(),
        Some(SessionEvent::TurnEnd { turn: 1, .. })
    ));
}

#[test]
fn rewinding_to_the_last_checkpoint_is_the_whole_log() {
    let events = log(2);
    let point = checkpoint_for_turn(&events, 2).expect("a checkpoint");

    assert_eq!(rewind_to(&events, &point), events);
}

#[test]
fn an_empty_log_rewinds_to_nothing() {
    let point = Checkpoint {
        after_seq: 0,
        turn: 0,
        events: 0,
    };
    assert!(rewind_to(&[], &point).is_empty());
}

#[test]
fn a_rewind_never_stops_in_the_middle_of_a_turn() {
    // The important negative case. A checkpoint whose cursor sits inside turn 2 — which a caller can
    // construct, and which a stale checkpoint against a newer log can effectively become — must back
    // up to the end of turn 1 rather than return a log whose turn 2 opened and never closed.
    let events = log(2);

    for after_seq in 5..=7u64 {
        let point = Checkpoint {
            after_seq,
            turn: 2,
            events: after_seq,
        };
        let rewound = rewind_to(&events, &point);

        assert_eq!(rewound.len(), 4, "cursor {after_seq} backs up to turn 1");
        assert!(matches!(
            rewound.last(),
            Some(SessionEvent::TurnEnd { turn: 1, .. })
        ));
    }
}

#[test]
fn a_checkpoint_with_no_reachable_boundary_rewinds_to_nothing() {
    // No turn has closed, so there is no prefix a replay could resume from. Empty is the honest
    // answer; returning the log as-is would be a rewind that silently did nothing.
    let mut events = vec![SessionEvent::TurnStart { turn: 1 }];
    events.push(SessionEvent::user_text("u1", "hello"));

    let point = Checkpoint {
        after_seq: 2,
        turn: 1,
        events: 2,
    };

    assert!(rewind_to(&events, &point).is_empty());
}

#[test]
fn an_out_of_range_checkpoint_clamps_to_the_end() {
    // A checkpoint taken before the log grew, or taken from another session. No error channel is
    // wanted on an undo button, so the defined behavior is "as far as the log goes", then back to a
    // boundary.
    let events = log(2);
    let point = Checkpoint {
        after_seq: 999,
        turn: 99,
        events: 999,
    };

    let rewound = rewind_to(&events, &point);

    assert_eq!(rewound, events, "clamped to the end, which is a boundary");
}

#[test]
fn an_out_of_range_checkpoint_still_ends_on_a_boundary() {
    // The same clamp when the tail is unfinished: clamping alone would keep the open turn, so the
    // cut must move back to the last `turn/end`.
    let mut events = log(2);
    events.push(SessionEvent::TurnStart { turn: 3 });
    events.push(SessionEvent::user_text("u3", "running"));

    let point = Checkpoint {
        after_seq: 10_000,
        turn: 3,
        events: 10_000,
    };

    let rewound = rewind_to(&events, &point);

    assert_eq!(rewound.len(), 8);
    assert!(matches!(
        rewound.last(),
        Some(SessionEvent::TurnEnd { turn: 2, .. })
    ));
}

#[test]
fn a_zero_checkpoint_rewinds_to_nothing() {
    // `after_seq = 0` is "before the first event" — the cursor `read_after(0)` means.
    let events = log(1);
    let point = Checkpoint {
        after_seq: 0,
        turn: 0,
        events: 0,
    };

    assert!(rewind_to(&events, &point).is_empty());
}
