//! Tests for the `sessionStats` timed fold.
//!
//! The module is included by path because `girder-core`'s crate root does not declare
//! `session_stats_gap` yet — wiring `pub mod session_stats_gap;` and its `pub use` belongs to the
//! integrator. Under that inclusion the module is a child of *this* test crate, which is why it
//! reaches library types through `girder_core::` re-exports rather than `crate::` paths: those
//! resolve the same way in both builds, so one source file is the single thing under test.
//!
//! # What these tests are for
//!
//! Upstream's suite pins the fold's arithmetic against `step/start` → `assistant/message` →
//! `tool/call` → `tool/result` → `step/end` sequences. Four claims carry the weight here, and each
//! is checked from the side that can fail rather than from the side that agrees with the
//! implementation.
//!
//! The first is that `step/end` — not `assistant/message` — counts a step. A fold that counted
//! messages agrees with this one on every clean log and disagrees exactly on a cancelled step, so a
//! cancelled step is what the test feeds it.
//!
//! The second is that a turn counts once however many steps it closes. Two closed steps in one turn
//! must report one turn and two steps, which is the count that distinguishes "turns" from "steps"
//! at all.
//!
//! The third is the pairing discipline: a tool result with no recorded call, a duplicate assistant
//! message, and a usage record with no anchor must each accrue **nothing** while leaving the rest of
//! the fold intact. A test that only asserted a total would pass against a fold that counted all
//! three.
//!
//! The fourth is the local difference from upstream: a boundary the caller cannot date is counted
//! as unmeasured rather than contributing a zero, so `llm_ms: 0` never claims a measurement it does
//! not have. The wire view still carries exactly upstream's eight fields.

#[path = "../src/session/stats_rollup.rs"]
pub mod session_stats_gap;

use girder_core::llm::attempt::{StreamRecord, run_first_token_time};
use girder_core::{
    ContentBlock, Projection, ProjectionRegistry, SessionEvent, StreamChunk, TokenUsage,
    ToolCallId, VersionedProjection,
};
use session_stats_gap::{
    DecodeAnchor, INJECT, KEY, NAME, OpenStep, Recorded, STATE_VERSION, SchemaError, StatsInput,
    StatsProjection, StatsState, StatsView, TimeSource, Untimed, apply, fold,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn call(id: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(id).expect("valid id"),
        name: "bash".to_owned(),
        arguments: "{}".to_owned(),
    }
}

fn result(id: &str, is_error: bool) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(id).expect("valid id"),
        content: vec![ContentBlock::Text {
            text: "done".to_owned(),
        }],
        is_error,
    }
}

fn usage(model: &str, output: u64) -> SessionEvent {
    SessionEvent::ModelUsage {
        model: model.to_owned(),
        input_tokens: 10,
        output_tokens: output,
    }
}

fn message() -> SessionEvent {
    SessionEvent::assistant_text("a", "hi", "test")
}

fn step_start(turn: u64, step: u64) -> SessionEvent {
    SessionEvent::StepStart { turn, step }
}

fn step_end(turn: u64, step: u64) -> SessionEvent {
    SessionEvent::StepEnd { turn, step }
}

fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

fn turn_end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: girder_core::TurnEndReason::Completed,
    }
}

/// One durable input at a time.
fn at(event: &SessionEvent, at_ms: Option<i64>) -> StatsInput<'_> {
    StatsInput::Event {
        event,
        at_ms,
        stream: None,
    }
}

fn untimed(event: &SessionEvent) -> StatsInput<'_> {
    at(event, None)
}

/// A text delta at a time, which is what a first-token boundary is measured from.
fn delta(time: i64, text: &str) -> StreamRecord {
    StreamRecord::Chunk {
        time,
        chunk: StreamChunk::TextDelta {
            index: 0,
            text: text.to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Constants: the plugin's identity
// ---------------------------------------------------------------------------------------------

#[test]
fn the_plugin_identity_matches_upstream() {
    // Two plugins under one name cannot both load, and a log line naming the projection should name
    // the same string in both harnesses.
    assert_eq!(NAME, "session-stats");
    assert_eq!(INJECT, ["sessionProjections"]);
    assert_eq!(KEY, "sessionStats");
    assert_eq!(STATE_VERSION, 1);
}

// ---------------------------------------------------------------------------------------------
// Counts: turns, steps, and the difference between them
// ---------------------------------------------------------------------------------------------

#[test]
fn an_empty_log_has_a_state_and_it_is_all_zero() {
    let state = fold(&[]);

    assert_eq!(state, StatsState::new());
    assert_eq!(state.view(), StatsView::default());
    assert!(!state.has_unmeasured_durations());
}

#[test]
fn a_step_end_counts_a_step_and_its_turn() {
    let events = [step_end(1, 1)];
    let inputs: Vec<StatsInput<'_>> = events.iter().map(untimed).collect();

    let state = fold(&inputs);

    assert_eq!(state.steps, 1);
    assert_eq!(state.turns, 1);
}

#[test]
fn two_steps_in_one_turn_are_two_steps_and_one_turn() {
    // The count that distinguishes "turns" from "steps" at all: a turn is an exchange, not a model
    // round, so a tool-using turn must not look like several turns.
    let events = [step_end(1, 1), step_end(1, 2)];
    let inputs: Vec<StatsInput<'_>> = events.iter().map(untimed).collect();

    let state = fold(&inputs);

    assert_eq!(state.steps, 2);
    assert_eq!(state.turns, 1);
    assert_eq!(state.last_turn, Some(1));
}

#[test]
fn a_later_turn_counts_again() {
    let events = [step_end(1, 1), step_end(2, 1), step_end(2, 2)];
    let inputs: Vec<StatsInput<'_>> = events.iter().map(untimed).collect();

    let state = fold(&inputs);

    assert_eq!(state.steps, 3);
    assert_eq!(state.turns, 2);
}

#[test]
fn an_assistant_message_alone_is_not_a_step() {
    // `step/end` is the lifecycle authority: a cancelled step assembles no message but still lands
    // one, and a message with no step/end must not be counted as a step either. This is the case a
    // message-counting fold gets wrong in both directions.
    let events = [step_start(1, 1), message()];
    let inputs: Vec<StatsInput<'_>> = events.iter().map(untimed).collect();

    let state = fold(&inputs);

    assert_eq!(state.steps, 0);
    assert_eq!(state.turns, 0);
}

#[test]
fn a_cancelled_step_still_counts_once_its_boundary_lands() {
    // step/start, then turn/end with no message at all: a cancelled step. The step count must see
    // it when `step/end` arrives, and no time figure may claim it.
    let events = [turn_start(1), step_start(1, 1), turn_end(1), step_end(1, 1)];
    let times = [0i64, 100, 200, 300];
    let inputs: Vec<StatsInput<'_>> = events
        .iter()
        .zip(times)
        .map(|(event, time)| at(event, Some(time)))
        .collect();

    let state = fold(&inputs);

    assert_eq!(state.steps, 1);
    assert_eq!(state.turns, 1);
    assert_eq!(state.llm_ms, 0, "a cancelled step assembles no message");
    assert_eq!(state.ttft_steps, 0);
}

// ---------------------------------------------------------------------------------------------
// Model wall time
// ---------------------------------------------------------------------------------------------

#[test]
fn model_time_spans_step_start_to_assistant_message() {
    let events = [step_start(1, 1), message(), step_end(1, 1)];
    let inputs = [
        at(&events[0], Some(1_000)),
        at(&events[1], Some(1_500)),
        at(&events[2], Some(1_600)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.llm_ms, 500);
    assert!(!state.has_unmeasured_durations());
}

#[test]
fn model_time_accumulates_across_steps() {
    let events = [
        step_start(1, 1),
        message(),
        step_end(1, 1),
        step_start(1, 2),
        message(),
        step_end(1, 2),
    ];
    let times = [0, 250, 300, 400, 900, 950];
    let inputs: Vec<StatsInput<'_>> = events
        .iter()
        .zip(times)
        .map(|(event, time)| at(event, Some(time)))
        .collect();

    let state = fold(&inputs);

    assert_eq!(
        state.llm_ms, 750,
        "250 from the first step, 500 from the second"
    );
    assert_eq!(state.steps, 2);
    assert_eq!(state.turns, 1);
}

#[test]
fn a_backwards_clock_contributes_zero_rather_than_a_negative() {
    // `Math.max(0, …)` upstream. A clock that jumped must not corrupt every total the duration
    // enters, and a negative duration would do exactly that.
    let events = [step_start(1, 1), message()];
    let inputs = [at(&events[0], Some(5_000)), at(&events[1], Some(4_000))];

    let state = fold(&inputs);

    assert_eq!(state.llm_ms, 0);
}

#[test]
fn a_duplicate_message_does_not_accrue_twice() {
    // One assembled message per step. The second finds no open step and must leave the total alone.
    let events = [step_start(1, 1), message(), message()];
    let inputs = [
        at(&events[0], Some(0)),
        at(&events[1], Some(300)),
        at(&events[2], Some(900)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.llm_ms, 300);
}

#[test]
fn a_message_with_no_open_step_accrues_nothing() {
    let events = [message()];
    let inputs = [at(&events[0], Some(1_000))];

    let state = fold(&inputs);

    assert_eq!(state.llm_ms, 0);
    assert_eq!(state.steps, 0);
}

// ---------------------------------------------------------------------------------------------
// First-token latency and decode
// ---------------------------------------------------------------------------------------------

#[test]
fn first_token_latency_is_measured_from_the_step_start() {
    let stream = vec![delta(400, "He"), delta(500, "llo")];
    let attempt = StatsInput::Attempt {
        turn: 1,
        step: 1,
        stream: &stream,
    };
    let events = [step_start(1, 1), message()];
    let inputs = [
        at(&events[0], Some(100)),
        attempt,
        at(&events[1], Some(900)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.ttft_steps, 1);
    assert_eq!(state.ttft_ms, 300, "400 - 100");
    assert_eq!(state.llm_ms, 800);
}

#[test]
fn a_retry_does_not_replace_the_first_token_time() {
    // The first attempt's first token is the model's real latency boundary; a retry inside the same
    // step streams later, and taking the later time would report the retry's delay as the model's.
    let first = vec![delta(400, "a")];
    let retry = vec![delta(800, "b")];
    let events = [step_start(1, 1), message()];
    let inputs = [
        at(&events[0], Some(100)),
        StatsInput::Attempt {
            turn: 1,
            step: 1,
            stream: &first,
        },
        StatsInput::Attempt {
            turn: 1,
            step: 1,
            stream: &retry,
        },
        at(&events[1], Some(1_000)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.ttft_ms, 300);
}

#[test]
fn an_attempt_for_another_step_is_ignored() {
    let stream = vec![delta(400, "a")];
    let events = [step_start(1, 1), message()];
    let inputs = [
        at(&events[0], Some(100)),
        StatsInput::Attempt {
            turn: 1,
            step: 2,
            stream: &stream,
        },
        at(&events[1], Some(900)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.ttft_steps, 0);
    assert_eq!(state.ttft_ms, 0);
}

#[test]
fn an_empty_fragment_is_a_keepalive_not_a_first_token() {
    // The first-token boundary is the first *output*, and an empty delta is framing.
    let stream = vec![delta(200, ""), delta(600, "a")];
    let events = [step_start(1, 1), message()];
    let inputs = [
        at(&events[0], Some(100)),
        StatsInput::Attempt {
            turn: 1,
            step: 1,
            stream: &stream,
        },
        at(&events[1], Some(1_000)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.ttft_ms, 500, "600 - 100, not 100");
}

#[test]
fn the_message_stream_supplies_the_first_token_when_no_attempt_does() {
    // Upstream's `open.firstTokenTime ?? firstToken(event.stream)`: a log folded from a checkpoint
    // may begin after the attempt, so the message's own stream is the fallback source.
    let stream = vec![delta(450, "x")];
    let events = [step_start(1, 1), message()];
    let inputs = [
        at(&events[0], Some(50)),
        StatsInput::Event {
            event: &events[1],
            at_ms: Some(800),
            stream: Some(&stream),
        },
    ];

    let state = fold(&inputs);

    assert_eq!(state.ttft_steps, 1);
    assert_eq!(state.ttft_ms, 400);
}

#[test]
fn decode_time_and_tokens_accrue_together() {
    let stream = vec![delta(400, "a")];
    let events = [step_start(1, 1), message(), usage("m", 42)];
    let inputs = [
        at(&events[0], Some(100)),
        StatsInput::Attempt {
            turn: 1,
            step: 1,
            stream: &stream,
        },
        at(&events[1], Some(900)),
        at(&events[2], Some(910)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.decode_ms, 500, "900 - 400");
    assert_eq!(state.decode_tokens, 42);
}

#[test]
fn a_usage_record_with_no_anchor_accrues_nothing() {
    // A re-reported settlement, or a log that starts mid-turn. Counting its tokens would make
    // `decodeTokens` a total over a different set than `decodeMs`.
    let events = [usage("m", 42)];
    let inputs = [at(&events[0], Some(910))];

    let state = fold(&inputs);

    assert_eq!(state.decode_tokens, 0);
    assert_eq!(state.decode_ms, 0);
    assert!(state.has_unmeasured_durations());
}

#[test]
fn a_second_usage_record_for_one_step_accrues_once() {
    let stream = vec![delta(400, "a")];
    let events = [step_start(1, 1), message(), usage("m", 42), usage("m", 42)];
    let inputs = [
        at(&events[0], Some(100)),
        StatsInput::Attempt {
            turn: 1,
            step: 1,
            stream: &stream,
        },
        at(&events[1], Some(900)),
        at(&events[2], Some(910)),
        at(&events[3], Some(920)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.decode_tokens, 42);
    assert_eq!(state.decode_ms, 500);
}

#[test]
fn a_step_with_no_first_token_accrues_no_ttft_and_no_decode() {
    // A step that assembled a message without ever streaming: the model time is real, the latency
    // and decode figures are undefined, and an average over them must not include it.
    let events = [step_start(1, 1), message(), usage("m", 7)];
    let inputs = [
        at(&events[0], Some(100)),
        at(&events[1], Some(900)),
        at(&events[2], Some(950)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.llm_ms, 800);
    assert_eq!(state.ttft_steps, 0);
    assert_eq!(state.ttft_ms, 0);
    assert_eq!(state.decode_tokens, 0);
}

// ---------------------------------------------------------------------------------------------
// Tool wall time
// ---------------------------------------------------------------------------------------------

#[test]
fn tool_time_pairs_call_and_result_by_id() {
    let events = [call("c1"), result("c1", false)];
    let inputs = [at(&events[0], Some(1_000)), at(&events[1], Some(1_350))];

    let state = fold(&inputs);

    assert_eq!(state.tool_ms, 350);
    assert!(state.pending_calls.is_empty());
}

#[test]
fn interleaved_tool_calls_pair_with_their_own_results() {
    let events = [call("a"), call("b"), result("b", false), result("a", false)];
    let inputs = [
        at(&events[0], Some(0)),
        at(&events[1], Some(100)),
        at(&events[2], Some(150)),
        at(&events[3], Some(400)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.tool_ms, 50 + 400);
}

#[test]
fn a_result_for_an_unrecorded_call_is_not_a_pair() {
    // Matched on the removal rather than checked after it: a result whose call this log never saw
    // is not a tool pair, and counting its wall time would describe work the log does not contain.
    let events = [result("ghost", false)];
    let inputs = [at(&events[0], Some(1_000))];

    let state = fold(&inputs);

    assert_eq!(state.tool_ms, 0);
}

#[test]
fn a_prototype_property_name_is_an_ordinary_call_id() {
    // Upstream needs an `Object.hasOwn` guard because `constructor` and `toString` are inherited
    // properties of a plain object. A `BTreeMap` has no prototype chain, so the same sequence must
    // pair normally — and if it did not, the hazard would have been reintroduced by the port.
    let events = [call("constructor"), result("constructor", false)];
    let inputs = [at(&events[0], Some(0)), at(&events[1], Some(120))];

    let state = fold(&inputs);

    assert_eq!(state.tool_ms, 120);
}

#[test]
fn a_failed_tool_result_still_measures_its_wall_time() {
    // The result arrived; failure is a fact about the tool, not about the clock.
    let events = [call("c1"), result("c1", true)];
    let inputs = [at(&events[0], Some(0)), at(&events[1], Some(90))];

    let state = fold(&inputs);

    assert_eq!(state.tool_ms, 90);
}

#[test]
fn a_turn_end_drops_unanswered_calls() {
    // Results always land within their turn, so a leftover at `turn/end` belongs to a cancelled or
    // failed turn. Keeping it would grow persisted state forever.
    let events = [turn_start(1), call("c1"), turn_end(1), result("c1", false)];
    let inputs = [
        at(&events[0], Some(0)),
        at(&events[1], Some(10)),
        at(&events[2], Some(20)),
        at(&events[3], Some(30)),
    ];

    let state = fold(&inputs);

    assert!(state.pending_calls.is_empty());
    assert_eq!(
        state.tool_ms, 0,
        "the pair was broken by the turn boundary, so no duration is claimed"
    );
}

#[test]
fn tool_time_accumulates_over_several_pairs() {
    let events = [call("a"), result("a", false), call("b"), result("b", false)];
    let inputs = [
        at(&events[0], Some(0)),
        at(&events[1], Some(200)),
        at(&events[2], Some(300)),
        at(&events[3], Some(450)),
    ];

    let state = fold(&inputs);

    assert_eq!(state.tool_ms, 350);
}

// ---------------------------------------------------------------------------------------------
// The local difference: an undatable boundary is counted, not zeroed
// ---------------------------------------------------------------------------------------------

#[test]
fn a_log_with_no_clock_counts_structure_and_admits_the_durations_are_missing() {
    // The whole reason this port differs from upstream: `llmMs: 0` must not claim a measurement.
    let events = [step_start(1, 1), message(), usage("m", 5), step_end(1, 1)];
    let inputs: Vec<StatsInput<'_>> = events.iter().map(untimed).collect();

    let state = fold(&inputs);

    assert_eq!(state.steps, 1);
    assert_eq!(state.turns, 1);
    assert_eq!(state.llm_ms, 0);
    assert_eq!(state.decode_tokens, 0);
    assert_eq!(
        state.unmeasured_durations, 2,
        "one model boundary and one decode boundary"
    );
    assert!(state.has_unmeasured_durations());
}

#[test]
fn a_half_datable_tool_pair_is_counted_as_unmeasured() {
    let events = [call("c1"), result("c1", false)];
    let inputs = [at(&events[0], None), at(&events[1], Some(500))];

    let state = fold(&inputs);

    assert_eq!(state.tool_ms, 0);
    assert_eq!(state.unmeasured_durations, 1);
}

#[test]
fn the_unmeasured_counter_is_not_on_the_wire() {
    let events = [step_start(1, 1), message()];
    let inputs: Vec<StatsInput<'_>> = events.iter().map(untimed).collect();
    let state = fold(&inputs);

    let wire = state.view().to_json().expect("view encodes");
    let object = wire.as_object().expect("a JSON object");

    assert!(
        !object.contains_key("unmeasuredDurations"),
        "state-only: a client has no contract for it"
    );
    assert_eq!(object.len(), 8, "upstream's eight fields and nothing else");
}

// ---------------------------------------------------------------------------------------------
// The wire view and the two schemas
// ---------------------------------------------------------------------------------------------

#[test]
fn the_view_carries_exactly_the_eight_upstream_fields() {
    let view = StatsView {
        turns: 2,
        steps: 3,
        llm_ms: 100,
        tool_ms: 20,
        ttft_ms: 30,
        ttft_steps: 1,
        decode_ms: 40,
        decode_tokens: 50,
    };

    let json = view.to_json().expect("encodes");
    let object = json.as_object().expect("an object");
    let mut names: Vec<&str> = object.keys().map(String::as_str).collect();
    names.sort_unstable();

    assert_eq!(
        names,
        [
            "decodeMs",
            "decodeTokens",
            "llmMs",
            "steps",
            "toolMs",
            "ttftMs",
            "ttftSteps",
            "turns",
        ]
    );
}

#[test]
fn the_view_names_match_upstreams_camel_case_spellings() {
    // The wire names are the contract a client reads, so they are asserted as strings rather than
    // trusted to the serde attribute.
    let view = StatsView {
        turns: 1,
        steps: 1,
        llm_ms: 2,
        tool_ms: 3,
        ttft_ms: 4,
        ttft_steps: 5,
        decode_ms: 6,
        decode_tokens: 7,
    };

    let json = view.to_json().expect("encodes");

    assert_eq!(json["turns"], serde_json::json!(1));
    assert_eq!(json["steps"], serde_json::json!(1));
    assert_eq!(json["llmMs"], serde_json::json!(2));
    assert_eq!(json["toolMs"], serde_json::json!(3));
    assert_eq!(json["ttftMs"], serde_json::json!(4));
    assert_eq!(json["ttftSteps"], serde_json::json!(5));
    assert_eq!(json["decodeMs"], serde_json::json!(6));
    assert_eq!(json["decodeTokens"], serde_json::json!(7));
}

#[test]
fn the_view_round_trips_through_json() {
    let view = StatsView {
        turns: 2,
        steps: 9,
        llm_ms: 1_000,
        tool_ms: 250,
        ttft_ms: 300,
        ttft_steps: 4,
        decode_ms: 700,
        decode_tokens: 1_234,
    };

    let json = view.to_json().expect("encodes");
    let decoded = StatsView::from_json(&json).expect("decodes");

    assert_eq!(decoded, view);
}

#[test]
fn the_view_refuses_an_unknown_field() {
    // `strict()` upstream: an extra key means the producer and the consumer disagree about the
    // schema, and accepting it would let a renamed field go unnoticed.
    let json = serde_json::json!({
        "turns": 1, "steps": 1,
        "llmMs": 0, "toolMs": 0, "ttftMs": 0, "ttftSteps": 0, "decodeMs": 0, "decodeTokens": 0,
        "somethingElse": 5,
    });

    assert!(matches!(
        StatsView::from_json(&json),
        Err(SchemaError::Shape(_))
    ));
}

#[test]
fn the_view_refuses_a_missing_field() {
    let json = serde_json::json!({
        "turns": 1, "steps": 1, "llmMs": 0, "toolMs": 0, "ttftMs": 0, "ttftSteps": 0, "decodeMs": 0,
    });

    assert!(matches!(
        StatsView::from_json(&json),
        Err(SchemaError::Shape(_))
    ));
}

#[test]
fn the_view_refuses_a_negative_duration() {
    // The check the type system cannot make: `-1` decodes into an `i64` perfectly well.
    let json = serde_json::json!({
        "turns": 1, "steps": 1,
        "llmMs": -1, "toolMs": 0, "ttftMs": 0, "ttftSteps": 0, "decodeMs": 0, "decodeTokens": 0,
    });

    assert_eq!(
        StatsView::from_json(&json),
        Err(SchemaError::Negative {
            field: "llmMs",
            value: -1
        })
    );
}

#[test]
fn the_view_refuses_a_negative_count() {
    // `nonnegative()` upstream, and here `u64` refuses it at decode rather than after.
    let json = serde_json::json!({
        "turns": -1, "steps": 1,
        "llmMs": 0, "toolMs": 0, "ttftMs": 0, "ttftSteps": 0, "decodeMs": 0, "decodeTokens": 0,
    });

    assert!(matches!(
        StatsView::from_json(&json),
        Err(SchemaError::Shape(_))
    ));
}

#[test]
fn the_view_refuses_a_fractional_count() {
    // `int()` upstream. A `u64` field cannot hold `1.5`, so the decoder refuses it.
    let json = serde_json::json!({
        "turns": 1.5, "steps": 1,
        "llmMs": 0, "toolMs": 0, "ttftMs": 0, "ttftSteps": 0, "decodeMs": 0, "decodeTokens": 0,
    });

    assert!(matches!(
        StatsView::from_json(&json),
        Err(SchemaError::Shape(_))
    ));
}

#[test]
fn the_state_round_trips_with_its_boundaries() {
    let state = StatsState {
        turns: 1,
        steps: 1,
        llm_ms: 100,
        tool_ms: 10,
        ttft_ms: 50,
        ttft_steps: 1,
        decode_ms: 20,
        decode_tokens: 7,
        last_turn: Some(1),
        open_step: Some(OpenStep {
            turn: 1,
            step: 2,
            start_ms: Some(500),
            first_token_ms: Some(600),
        }),
        pending_calls: [("c1".to_owned(), Some(400))].into_iter().collect(),
        decode_anchor: Some(DecodeAnchor {
            first_token_ms: 300,
            message_ms: Some(900),
        }),
        unmeasured_durations: 0,
    };

    let json = state.to_json().expect("encodes");
    let decoded = StatsState::from_json(&json).expect("decodes");

    assert_eq!(decoded, state);
}

#[test]
fn the_state_refuses_a_negative_duration_in_an_open_step() {
    let mut state = StatsState::new();
    state.open_step = Some(OpenStep {
        turn: 1,
        step: 1,
        start_ms: Some(-5),
        first_token_ms: None,
    });

    let json = state.to_json().expect("encodes");

    assert_eq!(
        StatsState::from_json(&json),
        Err(SchemaError::Negative {
            field: "openStep.startMs",
            value: -5
        })
    );
}

#[test]
fn the_view_is_a_strict_subset_of_the_state() {
    let mut state = StatsState::new();
    state.steps = 3;
    state.pending_calls.insert("c1".to_owned(), Some(10));

    let view = state.view();

    assert_eq!(view.steps, 3);
    assert_eq!(view.turns, state.turns);
    assert_eq!(view.llm_ms, state.llm_ms);
}

// ---------------------------------------------------------------------------------------------
// The registry adapter
// ---------------------------------------------------------------------------------------------

#[test]
fn the_projection_declares_its_key_version_and_summary_exclusion() {
    let projection = StatsProjection::untimed();

    assert_eq!(projection.key(), "sessionStats");
    assert_eq!(projection.state_version(), 1);
    assert!(
        !projection.in_summary(),
        "upstream's `summary: false`: no list row renders these figures"
    );
}

#[test]
fn the_projection_starts_from_the_empty_state() {
    let projection = StatsProjection::untimed();

    let init = projection.init();
    let state = StatsState::from_json(&init).expect("init is a valid state");

    assert_eq!(state, StatsState::new());
}

#[test]
fn the_projection_folds_a_registry_log_with_recorded_times() {
    // The adapter's whole job: turn a `seq` into a time. The fold is the same one `apply` performs.
    let source = Recorded::new()
        .with_time(1, 1_000) // step/start
        .with_time(2, 1_500) // assistant/message
        .with_time(3, 1_510) // model/usage
        .with_time(4, 1_600); // step/end
    let projection = StatsProjection::new(Arc::new(source));

    let mut state = projection.init();
    state = projection.apply(state, &step_start(1, 1), 1);
    state = projection.apply(state, &message(), 2);
    state = projection.apply(state, &usage("m", 11), 3);
    state = projection.apply(state, &step_end(1, 1), 4);

    let folded = StatsState::from_json(&state).expect("a valid state");

    assert_eq!(folded.steps, 1);
    assert_eq!(folded.turns, 1);
    assert_eq!(folded.llm_ms, 500);
    assert_eq!(folded.decode_tokens, 0, "no attempt reported a first token");
}

#[test]
fn the_projection_reads_the_message_stream_from_the_source() {
    let source = Recorded::new()
        .with_time(1, 100)
        .with_time(2, 900)
        .with_time(3, 950)
        .with_stream(2, vec![delta(400, "a")]);
    let projection = StatsProjection::new(Arc::new(source));

    let mut state = projection.init();
    state = projection.apply(state, &step_start(1, 1), 1);
    state = projection.apply(state, &message(), 2);
    state = projection.apply(state, &usage("m", 3), 3);

    let folded = StatsState::from_json(&state).expect("a valid state");

    assert_eq!(folded.ttft_steps, 1);
    assert_eq!(folded.ttft_ms, 300);
    assert_eq!(folded.decode_ms, 500);
    assert_eq!(folded.decode_tokens, 3);
}

#[test]
fn the_projection_leaves_an_unparseable_state_alone() {
    // Rewriting it would silently discard a fold that was almost readable, and the registry's cursor
    // has already moved past the event that would have rebuilt it.
    let projection = StatsProjection::untimed();
    let broken = serde_json::json!({ "turns": "many" });

    let after = projection.apply(broken.clone(), &step_end(1, 1), 1);

    assert_eq!(after, broken);
}

#[test]
fn the_untimed_source_dates_nothing() {
    let source = Untimed;
    assert_eq!(source.at_ms(1), None);
    assert_eq!(source.stream_at(1), None);
}

#[test]
fn the_projection_registers_and_replays_a_log() {
    // Integration with the registry the plugin injects: registering adds the key, replaying the log
    // rebuilds the state from init, and reading the key returns the folded view.
    let source = Recorded::new()
        .with_time(1, 0)
        .with_time(2, 400)
        .with_time(3, 450)
        .with_time(4, 500);
    let projection = Arc::new(StatsProjection::new(Arc::new(source)));
    let mut registry = ProjectionRegistry::new();
    registry
        .register(projection)
        .expect("sessionStats is not registered twice");

    let log = vec![
        (1, step_start(1, 1)),
        (2, message()),
        (3, usage("m", 9)),
        (4, step_end(1, 1)),
    ];
    registry.replay(&log);

    let view = registry
        .read("sessionStats")
        .expect("the key is registered");
    let stats = StatsState::from_json(&view).expect("a valid state");

    assert_eq!(stats.llm_ms, 400);
    assert_eq!(stats.steps, 1);
    assert_eq!(stats.turns, 1);

    // Excluded from the summary, so a listing does not pay for figures no list row renders.
    assert!(!registry.summary().contains_key("sessionStats"));
    assert!(registry.view().contains_key("sessionStats"));
}

// ---------------------------------------------------------------------------------------------
// The packed-stream boundary the attempt variant depends on
// ---------------------------------------------------------------------------------------------

#[test]
fn the_packed_first_token_helper_is_the_one_used_here() {
    // The fold delegates to `llm::attempt::first_token_time`. Pinned here so that a change in the
    // helper's notion of "token" fails a test in this module rather than drifting silently.
    let name_only = StreamRecord::ToolCallChunks {
        time0: 250,
        index: 0,
        dt: Vec::new(),
        id: ToolCallId::new("c1").expect("valid"),
        name: Some("bash".to_owned()),
        // Empty arguments: announcing the call *is* output, which is why a name-bearing run counts
        // even with nothing in it.
        args: vec![String::new()],
    };
    let run = girder_core::llm::attempt::StreamRun::of(&name_only).expect("a packed run");

    assert_eq!(run_first_token_time(run), Some(250));
    assert_eq!(
        girder_core::llm::attempt::first_token_time(&[name_only]),
        Some(250)
    );
}

#[test]
fn a_tool_call_chunk_with_no_name_and_no_arguments_is_not_a_token() {
    let empty = StreamRecord::ToolCallChunks {
        time0: 250,
        index: 0,
        dt: Vec::new(),
        id: ToolCallId::new("c1").expect("valid"),
        name: None,
        args: Vec::new(),
    };

    assert_eq!(
        girder_core::llm::attempt::first_token_time(&[empty]),
        None
    );
}

#[test]
fn a_usage_or_finish_chunk_never_measures_latency() {
    let framing = vec![
        StreamRecord::Chunk {
            time: 100,
            chunk: StreamChunk::BlockStart {
                index: 0,
                block_type: "text".to_owned(),
            },
        },
        StreamRecord::Chunk {
            time: 200,
            chunk: StreamChunk::Usage(TokenUsage {
                input: 1,
                output: 1,
                cached: 0,
            }),
        },
    ];

    assert_eq!(
        girder_core::llm::attempt::first_token_time(&framing),
        None
    );
}

// ---------------------------------------------------------------------------------------------
// Folding the same log twice is the same state
// ---------------------------------------------------------------------------------------------

#[test]
fn a_fold_is_deterministic_and_incremental_folding_agrees_with_one_pass() {
    // The property the whole projection contract rests on: a cached accumulation and a fresh replay
    // must agree. Upstream gates its change feed on this; here it is what makes a checkpoint safe.
    let events = [
        turn_start(1),
        step_start(1, 1),
        message(),
        usage("m", 5),
        call("c1"),
        result("c1", false),
        step_end(1, 1),
    ];
    let times = [0i64, 100, 600, 610, 700, 900, 950];
    let inputs: Vec<StatsInput<'_>> = events
        .iter()
        .zip(times)
        .map(|(event, time)| at(event, Some(time)))
        .collect();

    let one_pass = fold(&inputs);
    let incremental = inputs
        .iter()
        .fold(StatsState::new(), |state, input| apply(&state, input));

    assert_eq!(one_pass, incremental);
    assert_eq!(fold(&inputs), one_pass);
}

#[test]
fn an_uninteresting_event_leaves_the_state_equal() {
    let state = StatsState::new();
    let event = SessionEvent::PlanMode { active: true };

    let after = apply(&state, &at(&event, Some(1_000)));

    assert_eq!(after, state);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_stats_gap as stats_rollup;
}
