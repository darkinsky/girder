//! Tests for the model-selection history.
//!
//! The properties pinned here are the ones a plausible implementation gets wrong, and the first two
//! are the reason the module exists at all:
//!
//! - **`model_at` answers for the turn asked, not the newest model** — the easy bug is to return
//!   `current()` for every turn, which passes any test that only ever looks at the last turn. Every
//!   switching test therefore asks an *old* turn and a *new* one.
//! - **Repeated selections of one model are not switches** — the log writes a selection at each turn
//!   boundary, so an implementation that counts events reports a session that never changed models
//!   as one that changed constantly. The fixtures deliberately repeat a model several times.
//! - **Coverage is per turn, in descending order** — a model's turns span every run it owned, and
//!   the first row is the model that covered the most turns.
//! - **Incremental and rebuilt histories agree** — `record` and `build` are two paths to the same
//!   answer, and a resumed session reads one while a replayed session reads the other.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It names the crate through
//! `::girder_core`, which the library alias lets resolve from either side.

#[path = "../src/model_memory.rs"]
mod model_memory;

use girder_core::{ModelSelection, SessionEvent, TurnEndReason};
use model_memory::{Memory, Selection};

// ---- fixtures ----------------------------------------------------------------------------------

/// The selection event a log records when `model` is chosen.
fn selected(model: &str) -> SessionEvent {
    SessionEvent::ModelSelection {
        selection: ModelSelection::new("ds", model),
    }
}

/// A turn's opening event.
fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

/// A turn's closing event.
fn turn_end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    }
}

/// A model call, which names the model that answered.
fn usage(model: &str) -> SessionEvent {
    SessionEvent::ModelUsage {
        model: model.to_owned(),
        input_tokens: 10,
        output_tokens: 5,
    }
}

/// One complete turn using `model`, written the way the agent loop writes it.
fn turn(number: u64, model: &str) -> Vec<SessionEvent> {
    vec![
        selected(model),
        turn_start(number),
        SessionEvent::user_text(&format!("msg-u{number}"), format!("ask {number}")),
        SessionEvent::StepStart {
            turn: number,
            step: 1,
        },
        usage(model),
        SessionEvent::StepEnd {
            turn: number,
            step: 1,
        },
        turn_end(number),
    ]
}

/// A history recorded one selection at a time.
fn recorded(selections: &[(usize, &str, u64)]) -> Memory {
    let mut memory = Memory::new();
    for (at, model, from_turn) in selections {
        memory.record(Selection::new(*at, *model, *from_turn));
    }
    memory
}

// ---- the empty history -------------------------------------------------------------------------

#[test]
fn an_empty_history_answers_nothing() {
    let memory = Memory::new();

    // `None`, not a default model name: no selection has been made, and naming one would be a
    // measurement the history does not have.
    assert_eq!(memory.current(), None);
    assert_eq!(memory.model_at(1), None);
    assert_eq!(memory.model_at(0), None);
    assert_eq!(memory.switches(), 0);
    assert!(memory.turns_per_model().is_empty());
}

#[test]
fn an_empty_log_rebuilds_an_empty_history() {
    let memory = Memory::build(&[]);

    assert_eq!(memory, Memory::new());
    assert_eq!(memory.current(), None);
    assert!(memory.turns_per_model().is_empty());
}

// ---- one model ---------------------------------------------------------------------------------

#[test]
fn a_single_model_covers_every_turn() {
    // The writer records a selection at each turn boundary, so the model repeats.
    let memory = recorded(&[(0, "a", 1), (5, "a", 2), (10, "a", 3)]);

    assert_eq!(memory.current(), Some("a"));
    assert_eq!(memory.model_at(1), Some("a"));
    assert_eq!(memory.model_at(2), Some("a"));
    assert_eq!(memory.model_at(3), Some("a"));
    // The history ends at turn 3; a turn beyond it was never governed by any selection.
    assert_eq!(memory.model_at(4), None);
    // Repeated selections are one run, not three.
    assert_eq!(memory.switches(), 0);
    assert_eq!(memory.turns_per_model(), vec![("a".to_owned(), 3)]);
}

// ---- the core claim: a mid-session switch ------------------------------------------------------

#[test]
fn model_at_answers_for_the_turn_asked_not_the_newest_model() {
    // `a` ran turns 1 to 3, then the session switched to `b` from turn 4.
    let memory = recorded(&[
        (0, "a", 1),
        (5, "a", 2),
        (10, "a", 3),
        (15, "b", 4),
        (20, "b", 5),
    ]);

    // The turns before the switch still answer the old model. An implementation that returned
    // `current()` here would fail on the next three assertions.
    assert_eq!(memory.model_at(1), Some("a"));
    assert_eq!(memory.model_at(2), Some("a"));
    assert_eq!(memory.model_at(3), Some("a"));
    // The turn the selection names, and every turn after it, answer the new model.
    assert_eq!(memory.model_at(4), Some("b"));
    assert_eq!(memory.model_at(5), Some("b"));

    assert_eq!(memory.current(), Some("b"));
    // One transition, however many events recorded it.
    assert_eq!(memory.switches(), 1);
}

#[test]
fn a_turn_before_the_first_selection_has_no_model() {
    // A log can hold turns recorded before any selection (an imported or hand-edited log), and the
    // turn before the first selection was not governed by a model this history knows about.
    let memory = recorded(&[(0, "a", 3)]);

    assert_eq!(memory.model_at(1), None);
    assert_eq!(memory.model_at(2), None);
    assert_eq!(memory.model_at(3), Some("a"));
}

#[test]
fn every_turn_answers_the_model_of_the_run_it_belongs_to() {
    // Three runs: a on 1-2, b on 3-5, a again on 6-7. Walking every turn is what catches a lookup
    // that is off by one at a boundary.
    let memory = recorded(&[
        (0, "a", 1),
        (1, "a", 2),
        (2, "b", 3),
        (3, "b", 4),
        (4, "b", 5),
        (5, "a", 6),
        (6, "a", 7),
    ]);

    let answers: Vec<Option<&str>> = (1..=7).map(|turn| memory.model_at(turn)).collect();
    assert_eq!(
        answers,
        vec![
            Some("a"),
            Some("a"),
            Some("b"),
            Some("b"),
            Some("b"),
            Some("a"),
            Some("a"),
        ]
    );
}

// ---- switch counting ---------------------------------------------------------------------------

#[test]
fn consecutive_selections_of_one_model_are_not_switches() {
    // `a` chosen four times, then `b` three times: exactly one change of model.
    let memory = recorded(&[
        (0, "a", 1),
        (1, "a", 2),
        (2, "a", 3),
        (3, "a", 4),
        (4, "b", 5),
        (5, "b", 6),
        (6, "b", 7),
    ]);

    assert_eq!(memory.switches(), 1);
}

#[test]
fn a_history_with_no_repetition_counts_every_transition() {
    // `a b a b` is three transitions. The alternating fixture is what a dedup that also collapsed
    // non-adjacent repeats would break.
    let memory = recorded(&[(0, "a", 1), (1, "b", 2), (2, "a", 3), (3, "b", 4)]);

    assert_eq!(memory.switches(), 3);
}

#[test]
fn recording_out_of_order_still_counts_switches_by_run() {
    // A resumed session can hand selections over in any order; the history sorts them, so the run
    // boundaries are the same ones a chronological record would produce.
    let memory = recorded(&[(20, "b", 6), (0, "a", 1), (10, "a", 3), (30, "c", 9)]);

    assert_eq!(memory.model_at(1), Some("a"));
    assert_eq!(memory.model_at(6), Some("b"));
    assert_eq!(memory.model_at(9), Some("c"));
    assert_eq!(memory.switches(), 2);
}

// ---- turns per model ---------------------------------------------------------------------------

#[test]
fn turns_per_model_counts_each_models_turns_in_descending_order() {
    // a: turns 1-2 (2), b: turns 3-7 (5).
    let memory = recorded(&[(0, "a", 1), (1, "b", 3), (2, "b", 7)]);

    assert_eq!(
        memory.turns_per_model(),
        vec![("b".to_owned(), 5), ("a".to_owned(), 2)]
    );
}

#[test]
fn turns_per_model_sums_a_models_separate_runs() {
    // a ran 1-2 and 5-6 (4 turns), b ran 3-4 (2 turns). The answer is how much of the conversation
    // ran on a model, not how it was interleaved, so a is one row of four.
    let memory = recorded(&[(0, "a", 1), (1, "b", 3), (2, "a", 5), (3, "a", 6)]);

    assert_eq!(
        memory.turns_per_model(),
        vec![("a".to_owned(), 4), ("b".to_owned(), 2)]
    );
}

#[test]
fn turns_per_model_covers_the_turns_the_last_selection_governs() {
    // The last run is bounded by the furthest turn any selection named, so `b` gets 5-9, not just 5.
    let memory = recorded(&[(0, "a", 1), (1, "a", 4), (2, "b", 5), (3, "b", 9)]);

    assert_eq!(
        memory.turns_per_model(),
        vec![("b".to_owned(), 5), ("a".to_owned(), 4)]
    );
}

#[test]
fn an_even_split_is_ordered_by_model_name() {
    // Ties are ordered by name so that two readers of the same history print the same table.
    // `z` covers turns 1-2 and `m` covers 3-4, because the last selection reaches turn 4.
    let memory = recorded(&[(0, "z", 1), (1, "m", 3), (2, "m", 4)]);

    assert_eq!(
        memory.turns_per_model(),
        vec![("m".to_owned(), 2), ("z".to_owned(), 2)]
    );
}

// ---- rebuilding from a log ---------------------------------------------------------------------

#[test]
fn rebuild_and_incremental_record_agree() {
    // Five turns: a, a, b, b, b. The rebuilt history reads the turn of each selection off its
    // position; the incremental one is told the same turns directly. Both must land on the same
    // answer, including the coverage of the last run.
    let events: Vec<SessionEvent> = [("a", 1), ("a", 2), ("b", 3), ("b", 4), ("b", 5)]
        .into_iter()
        .flat_map(|(model, number)| turn(number, model))
        .collect();

    let rebuilt = Memory::build(&events);
    let incremental = recorded(&[
        (0, "a", 1),
        (7, "a", 2),
        (14, "b", 3),
        (21, "b", 4),
        (28, "b", 5),
    ]);

    assert_eq!(rebuilt, incremental);
    assert_eq!(rebuilt.current(), Some("b"));
    assert_eq!(rebuilt.switches(), 1);
    assert_eq!(
        rebuilt.turns_per_model(),
        vec![("b".to_owned(), 3), ("a".to_owned(), 2)]
    );
    for number in 1..=5 {
        assert_eq!(rebuilt.model_at(number), incremental.model_at(number));
    }
}

#[test]
fn a_selection_written_mid_turn_governs_that_turn() {
    // The writer emits the selection before `turn/start`, but a resumed session can write one inside
    // an open turn; the position says it governs the turn already running, not the next one.
    let events = vec![
        turn_start(1),
        selected("a"),
        turn_end(1),
        turn_start(2),
        turn_end(2),
        turn_start(3),
        turn_end(3),
    ];

    let memory = Memory::build(&events);

    assert_eq!(memory.model_at(1), Some("a"));
    // No further selection was written, so the log's last selection still governs the earlier turns
    // only: turn 3 was never covered by a selection, and guessing would credit it to `a`.
    assert_eq!(memory.model_at(2), None);
    assert_eq!(memory.current(), Some("a"));
}

#[test]
fn a_selection_before_any_turn_governs_turn_one() {
    // A selection written before the first `turn/start` is the choice made ahead of turn 1.
    let events = vec![selected("a"), turn_start(1), turn_end(1)];

    let memory = Memory::build(&events);

    assert_eq!(memory.model_at(1), Some("a"));
    assert_eq!(memory.turns_per_model(), vec![("a".to_owned(), 1)]);
}

#[test]
fn a_selection_between_turns_governs_the_next_turn() {
    // Between a closed turn and the next `turn/start` there is no open turn, so the selection belongs
    // to the turn it precedes. Reading it as the closed turn would attribute the new model to a turn
    // that already ran.
    let events = vec![
        selected("a"),
        turn_start(1),
        turn_end(1),
        selected("b"),
        turn_start(2),
        turn_end(2),
    ];

    let memory = Memory::build(&events);

    assert_eq!(memory.model_at(1), Some("a"));
    assert_eq!(memory.model_at(2), Some("b"));
    assert_eq!(memory.switches(), 1);
}

#[test]
fn a_rebuild_ignores_events_that_are_not_selections() {
    // Messages, usage, and tool traffic carry no selection; the history must not read a model name
    // out of a usage event, because usage says who answered rather than who was chosen.
    let events = vec![
        selected("a"),
        turn_start(1),
        usage("a"),
        SessionEvent::user_text("msg-u1", "ask"),
        SessionEvent::assistant_text("msg-a1", "answer", "a"),
        turn_end(1),
    ];

    let memory = Memory::build(&events);

    assert_eq!(memory.switches(), 0);
    assert_eq!(memory.current(), Some("a"));
    assert_eq!(memory.turns_per_model(), vec![("a".to_owned(), 1)]);
}

// ---- current -----------------------------------------------------------------------------------

#[test]
fn current_returns_the_last_model_chosen() {
    let memory = recorded(&[(0, "a", 1), (1, "b", 2), (2, "c", 3)]);

    assert_eq!(memory.current(), Some("c"));
}

#[test]
fn current_and_model_at_agree_only_at_the_end() {
    // Pins the distinction the module exists for: `current` is one answer, `model_at` is the answer
    // per turn, and they coincide only for turns the last model covered.
    let memory = recorded(&[(0, "a", 1), (1, "a", 2), (2, "b", 3)]);

    assert_eq!(memory.current(), Some("b"));
    assert_eq!(memory.model_at(3), memory.current());
    assert_ne!(memory.model_at(2), memory.current());
}
