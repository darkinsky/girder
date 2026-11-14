//! Tests for indexing a session log by turn position.
//!
//! Three properties carry the weight. A span must be half-open, because that is what makes
//! `events[span.start..span.end]` the turn and nothing else, and what lets consecutive turns share a
//! boundary index. An unclosed turn must still get a span reaching the end of the stream *and* be
//! distinguishable from a turn that happened to close on the last record. And [`Index::turn_of`] must
//! cover every position inside a turn while answering `None` outside one — the title records and the
//! prompt written before `turn/start` are exactly the positions a naive "nearest boundary" fold would
//! misattribute.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It resolves `girder_core::…` through
//! the alias the library declares for itself, which is why the same source builds here.

#[path = "../src/event_index.rs"]
mod event_index;

use girder_core::{SessionEvent, ToolCallId, TurnEndReason};

use event_index::{Index, TurnSpan};

// ---- fixtures ----------------------------------------------------------------------------------

/// A finished turn of exactly `turn/start` plus `turn/end`, which is the smallest closed turn.
fn closed_turn(turn: u64) -> [SessionEvent; 2] {
    [
        SessionEvent::TurnStart { turn },
        SessionEvent::TurnEnd {
            turn,
            reason: TurnEndReason::Completed,
        },
    ]
}

/// A turn with a prompt and one step in it, so a span is wider than its two boundaries.
fn working_turn(turn: u64) -> Vec<SessionEvent> {
    vec![
        SessionEvent::TurnStart { turn },
        SessionEvent::user_text(&format!("m{turn}"), "go"),
        SessionEvent::StepStart { turn, step: 1 },
        SessionEvent::assistant_text(&format!("a{turn}"), "done", "deepseek"),
        SessionEvent::StepEnd { turn, step: 1 },
        SessionEvent::TurnEnd {
            turn,
            reason: TurnEndReason::Completed,
        },
    ]
}

/// A session title record, which belongs to no turn because it describes the whole session.
fn title(text: &str) -> SessionEvent {
    SessionEvent::SessionTitle {
        title: text.to_owned(),
        message_seqs: vec![1],
        source: "model".to_owned(),
    }
}

// ---- a single turn -----------------------------------------------------------------------------

#[test]
fn a_single_turn_spans_its_own_boundaries() {
    let events = closed_turn(1);

    let index = Index::build(&events);

    assert_eq!(index.len(), 1);
    assert!(!index.is_empty());
    assert_eq!(
        index.turn_span(1),
        Some(TurnSpan {
            turn: 1,
            start: 0,
            end: 2,
        })
    );
    assert_eq!(index.turns().len(), 1);
    assert_eq!(index.open_turn(), None);
}

#[test]
fn a_single_turn_of_many_events_ends_one_past_its_end_record() {
    let events = working_turn(1);

    let index = Index::build(&events);

    let span = index.turn_span(1).expect("the turn is indexed");
    assert_eq!(span.start, 0);
    // `turn/end` is the last record and belongs to the turn, so the exclusive end is one past it.
    assert_eq!(span.end, events.len());
    assert_eq!(span.len(), 6);
    assert!(!span.is_empty());
    assert_eq!(index.turn_of(events.len() - 1), Some(1));
}

#[test]
fn slicing_by_the_span_yields_the_turn() {
    // The point of the half-open range: the crate's paging slices `events[start..end]`, so a span must
    // be usable as one without adjustment.
    let mut events = working_turn(1);
    events.extend(working_turn(2));

    let index = Index::build(&events);
    let span = index.turn_span(2).expect("indexed");

    let slice = &events[span.start..span.end];
    assert_eq!(slice.len(), span.len());
    assert!(matches!(
        slice.first(),
        Some(SessionEvent::TurnStart { turn: 2 })
    ));
    assert!(matches!(
        slice.last(),
        Some(SessionEvent::TurnEnd { turn: 2, .. })
    ));
}

// ---- many turns --------------------------------------------------------------------------------

#[test]
fn several_turns_are_indexed_in_log_order() {
    let mut events = Vec::new();
    for turn in 1..=3 {
        events.extend(working_turn(turn));
    }

    let index = Index::build(&events);

    assert_eq!(index.len(), 3);
    let turns: Vec<u64> = index.turns().iter().map(|span| span.turn).collect();
    assert_eq!(turns, vec![1, 2, 3]);
    // Consecutive spans meet with no gap and no overlap.
    assert_eq!(index.turns()[0].end, index.turns()[1].start);
    assert_eq!(index.turns()[1].end, index.turns()[2].start);
    assert_eq!(index.turns()[2].end, events.len());
}

#[test]
fn turn_span_answers_by_number_not_by_position() {
    // A forked or resumed log can repeat and reorder numbers, so the table must be keyed by the number
    // the log carried rather than by where the turn sits.
    let mut events = closed_turn(7).to_vec();
    events.extend(closed_turn(3));

    let index = Index::build(&events);

    assert_eq!(index.turns()[0].turn, 7);
    assert_eq!(index.turns()[1].turn, 3);
    assert_eq!(index.turn_span(7).map(|span| span.start), Some(0));
    assert_eq!(index.turn_span(3).map(|span| span.start), Some(2));
    assert_eq!(index.turn_span(1), None);
}

// ---- an unclosed turn (the core property) ------------------------------------------------------

#[test]
fn an_unclosed_turn_runs_to_the_end_of_the_stream() {
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::user_text("m1", "still working"),
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::ToolCall {
            id: ToolCallId::new("call-1").expect("constant is valid"),
            name: "bash".to_owned(),
            arguments: "{}".to_owned(),
        },
    ];

    let index = Index::build(&events);

    let span = index.turn_span(1).expect("the open turn is indexed");
    assert_eq!(span.start, 0);
    assert_eq!(span.end, events.len());
    assert_eq!(index.open_turn(), Some(1));
    assert!(index.is_open(1));
}

#[test]
fn an_unclosed_turn_is_distinguishable_from_one_closed_on_the_last_event() {
    // Both have `end == events.len()`, so the range alone cannot tell them apart; the index has to
    // keep the difference. A client drawing a spinner on the live turn depends on it.
    let unclosed = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::user_text("m1", "go"),
    ];
    let closed = closed_turn(1);

    let open_index = Index::build(&unclosed);
    let closed_index = Index::build(&closed);

    assert_eq!(open_index.turn_span(1).map(|span| span.end), Some(2));
    assert_eq!(closed_index.turn_span(1).map(|span| span.end), Some(2));
    assert_eq!(open_index.open_turn(), Some(1));
    assert_eq!(closed_index.open_turn(), None);
}

#[test]
fn an_open_turn_followed_by_nothing_still_attributes_its_events() {
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
        SessionEvent::TurnStart { turn: 2 },
        SessionEvent::user_text("m2", "next"),
    ];

    let index = Index::build(&events);

    assert_eq!(index.len(), 2);
    assert_eq!(index.open_turn(), Some(2));
    assert!(!index.is_open(1));
    assert_eq!(index.turn_of(3), Some(2));
    assert_eq!(index.turn_span(1).map(|span| span.end), Some(2));
}

// ---- turn_of over the whole range (the core property) ------------------------------------------

#[test]
fn turn_of_covers_every_position_of_every_turn() {
    let mut events = vec![title("a session")];
    for turn in 1..=4 {
        events.extend(working_turn(turn));
    }

    let index = Index::build(&events);

    for span in index.turns() {
        for position in span.start..span.end {
            assert_eq!(
                index.turn_of(position),
                Some(span.turn),
                "position {position} of turn {}",
                span.turn
            );
        }
    }
}

#[test]
fn turn_of_names_the_turn_a_position_belongs_to() {
    let mut events = working_turn(1);
    events.extend(working_turn(2));

    let index = Index::build(&events);

    assert_eq!(index.turn_of(0), Some(1), "turn 1 starts here");
    assert_eq!(index.turn_of(5), Some(1), "turn 1 ends here");
    assert_eq!(index.turn_of(6), Some(2), "turn 2 starts here");
    assert_eq!(index.turn_of(11), Some(2), "turn 2 ends here");
}

// ---- positions outside any turn ----------------------------------------------------------------

#[test]
fn a_title_after_the_last_turn_belongs_to_no_turn() {
    let mut events = closed_turn(1).to_vec();
    events.push(title("named afterwards"));

    let index = Index::build(&events);

    assert_eq!(index.turn_of(2), None);
    assert_eq!(index.len(), 1);
}

#[test]
fn a_prompt_written_before_its_turn_belongs_to_no_turn() {
    // This build records the prompt before spawning the loop so it survives a crash before the model
    // answers. Attributing it to the previous turn would name the one turn it certainly is not part of.
    let events = vec![
        SessionEvent::user_text("m1", "first"),
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
        SessionEvent::user_text("m2", "second"),
        title("after"),
    ];

    let index = Index::build(&events);

    assert_eq!(index.turn_of(0), None);
    assert_eq!(index.turn_of(1), Some(1));
    assert_eq!(index.turn_of(3), None);
    assert_eq!(index.turn_of(4), None);
}

#[test]
fn a_position_past_the_end_of_the_stream_belongs_to_no_turn() {
    let events = closed_turn(1);
    let index = Index::build(&events);

    assert_eq!(index.turn_of(events.len()), None);
    assert_eq!(index.turn_of(usize::MAX), None);
}

// ---- empty and absent --------------------------------------------------------------------------

#[test]
fn an_empty_stream_has_no_turns() {
    let index = Index::build(&[]);

    assert!(index.is_empty());
    assert_eq!(index.len(), 0);
    assert!(index.turns().is_empty());
    assert_eq!(index.turn_span(1), None);
    assert_eq!(index.turn_of(0), None);
    assert_eq!(index.open_turn(), None);
}

#[test]
fn a_stream_with_only_title_records_has_no_turns() {
    let events = vec![title("empty session")];
    let index = Index::build(&events);

    assert!(index.is_empty());
    assert_eq!(index.turn_of(0), None);
}

#[test]
fn a_new_index_is_the_empty_index() {
    let index = Index::new();

    assert!(index.is_empty());
    assert_eq!(index.turn_span(0), None);
}

#[test]
fn an_absent_turn_has_no_span() {
    let mut events = closed_turn(1).to_vec();
    events.extend(closed_turn(2));

    let index = Index::build(&events);

    assert_eq!(index.turn_span(0), None);
    assert_eq!(index.turn_span(3), None);
    assert_eq!(index.turn_span(u64::MAX), None);
}

// ---- consecutive turns -------------------------------------------------------------------------

#[test]
fn consecutive_turns_partition_the_stream() {
    let events: Vec<SessionEvent> = (1..=5)
        .flat_map(|turn| {
            [
                SessionEvent::TurnStart { turn },
                SessionEvent::user_text(&format!("m{turn}"), "go"),
                SessionEvent::TurnEnd {
                    turn,
                    reason: TurnEndReason::Completed,
                },
            ]
        })
        .collect();

    let index = Index::build(&events);

    assert_eq!(index.len(), 5);
    assert_eq!(index.turns().first().map(|span| span.start), Some(0));
    assert_eq!(
        index.turns().last().map(|span| span.end),
        Some(events.len())
    );
    let mut expected_start = 0;
    for (position, span) in index.turns().iter().enumerate() {
        let turn = u64::try_from(position).map_or(0, |value| value + 1);
        assert_eq!(span.turn, turn);
        assert_eq!(span.start, expected_start);
        expected_start = span.end;
    }
    // Every event in the stream belongs to some turn, and the position past the end belongs to none.
    for position in 0..events.len() {
        assert!(index.turn_of(position).is_some(), "position {position}");
    }
    assert_eq!(index.turn_of(events.len()), None);
}

#[test]
fn a_long_run_of_empty_turns_stays_addressable() {
    // Binary search over one entry per turn: every span is still found exactly, including the last,
    // which is the one an off-by-one in the search would lose.
    let events: Vec<SessionEvent> = (1..=64).flat_map(closed_turn).collect();

    let index = Index::build(&events);

    assert_eq!(index.len(), 64);
    for turn in 1..=64 {
        let span = index.turn_span(turn).expect("indexed");
        let start = usize::try_from(turn - 1).map_or(0, |value| value * 2);
        assert_eq!(
            span,
            TurnSpan {
                turn,
                start,
                end: start + 2
            }
        );
        assert_eq!(index.turn_of(start), Some(turn));
        assert_eq!(index.turn_of(start + 1), Some(turn));
    }
}
