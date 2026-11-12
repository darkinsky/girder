//! Tests for counting what a conversation did.
//!
//! The counts are deliberately separated: a turn that never ran a step and a tool call whose result never arrived are
//! both facts about the conversation, and folding either into the other would make a log look more complete than it is.

use girder_core::{SessionEvent, SessionStats};

fn user(text: &str) -> SessionEvent {
    SessionEvent::user_text("m", text)
}

fn assistant(text: &str) -> SessionEvent {
    SessionEvent::assistant_text("a", text, "test")
}

fn usage(model: &str, input: u64, output: u64) -> SessionEvent {
    SessionEvent::ModelUsage {
        model: model.to_owned(),
        input_tokens: input,
        output_tokens: output,
    }
}

#[test]
fn an_empty_log_counts_nothing() {
    let stats = SessionStats::of(&[]);

    assert_eq!(stats, SessionStats::default());
    assert!(!stats.has_model_calls());
    assert_eq!(stats.average_output_per_message(), None);
}

#[test]
fn a_step_end_counts_a_step_and_its_turn() {
    let events = vec![SessionEvent::StepEnd { turn: 1, step: 1 }];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.steps, 1);
    assert_eq!(stats.turns, 1);
}

#[test]
fn two_steps_in_one_turn_count_as_one_turn() {
    // A turn is an exchange, not a model round: folding rounds into turns would make a tool-using conversation look
    // like several conversations.
    let events = vec![
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 2 },
    ];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.steps, 2);
    assert_eq!(stats.turns, 1);
}

#[test]
fn a_tool_pair_counts_once() {
    let events = vec![
        SessionEvent::ToolCall {
            id: girder_core::ToolCallId::new("c1").expect("valid"),
            name: "bash".to_owned(),
            arguments: "{}".to_owned(),
        },
        SessionEvent::ToolResult {
            id: girder_core::ToolCallId::new("c1").expect("valid"),
            content: Vec::new(),
            is_error: false,
        },
    ];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.unanswered_tool_calls, 0);
    assert_eq!(stats.failed_tool_calls, 0);
}

#[test]
fn an_unanswered_call_is_reported_separately() {
    // A crash mid-call leaves one, and a caller deciding whether a log is complete needs to see it.
    let events = vec![SessionEvent::ToolCall {
        id: girder_core::ToolCallId::new("c1").expect("valid"),
        name: "bash".to_owned(),
        arguments: "{}".to_owned(),
    }];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.tool_calls, 0, "no result arrived");
    assert_eq!(stats.unanswered_tool_calls, 1);
}

#[test]
fn a_failed_call_is_counted_as_answered_and_as_failed() {
    let events = vec![
        SessionEvent::ToolCall {
            id: girder_core::ToolCallId::new("c1").expect("valid"),
            name: "bash".to_owned(),
            arguments: "{}".to_owned(),
        },
        SessionEvent::ToolResult {
            id: girder_core::ToolCallId::new("c1").expect("valid"),
            content: Vec::new(),
            is_error: true,
        },
    ];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.failed_tool_calls, 1);
}

#[test]
fn a_result_for_an_unrecorded_call_is_not_counted() {
    // Otherwise the totals disagree with the conversation: a result with no call is a corrupt log, not a tool call.
    let events = vec![SessionEvent::ToolResult {
        id: girder_core::ToolCallId::new("orphan").expect("valid"),
        content: Vec::new(),
        is_error: false,
    }];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.tool_calls, 0);
    assert_eq!(stats.unanswered_tool_calls, 0);
}

#[test]
fn messages_are_counted_by_role() {
    let events = vec![user("one"), assistant("two"), user("three")];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.user_messages, 2);
    assert_eq!(stats.assistant_messages, 1);
}

#[test]
fn usage_sums_both_directions() {
    let events = vec![
        usage("deepseek-chat", 100, 50),
        usage("deepseek-chat", 20, 10),
    ];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.input_tokens, 120);
    assert_eq!(stats.output_tokens, 60);
    assert!(stats.has_model_calls());
}

#[test]
fn models_are_listed_once_in_first_use_order() {
    let events = vec![usage("a", 1, 1), usage("b", 1, 1), usage("a", 1, 1)];

    let stats = SessionStats::of(&events);

    assert_eq!(stats.models, vec!["a", "b"]);
}

#[test]
fn the_average_is_undefined_for_no_messages() {
    // An average of no messages is not zero, and a caller that displayed zero would be reporting a measurement it does
    // not have.
    let events = vec![usage("a", 10, 10)];

    assert_eq!(SessionStats::of(&events).average_output_per_message(), None);
}

#[test]
fn the_average_divides_tokens_by_messages() {
    let events = vec![assistant("one"), assistant("two"), usage("a", 0, 100)];

    let average = SessionStats::of(&events)
        .average_output_per_message()
        .expect("two messages");

    assert!((average - 50.0).abs() < f64::EPSILON);
}
