//! Tests for selecting a session's final assistant output.
//!
//! Three properties carry the weight here, and each one is a way a naive "last message" rule goes
//! wrong: selection follows event order rather than the turn counter, an empty-content message
//! must not erase the answer before it, and a tool-only turn is output that simply has no prose.
//! The distinction between "no output" and "output with no text" is the one callers act on, so it
//! is asserted rather than assumed.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written before the crate root wires it in. It resolves `girder_core::…`
//! through the alias the library declares for itself, which is why the same source builds here.

#[path = "../src/assistant_output.rs"]
mod assistant_output;

use girder_core::{
    ChatMessage, ChatRole, ContentBlock, MessageId, MessageSource, SessionEvent, ToolCallId,
};

use assistant_output::{final_output, final_text};

/// Builds an assistant message event carrying exactly `content`.
fn assistant(id: &str, content: Vec<ContentBlock>) -> SessionEvent {
    SessionEvent::AssistantMessage {
        message: ChatMessage::new(
            MessageId::new(id).expect("test ids are valid"),
            ChatRole::Assistant,
            MessageSource::Model {
                provider: "test".to_owned(),
                model: "test".to_owned(),
                replay_state: None,
            },
            content,
        ),
    }
}

/// Builds an assistant message event whose only block is `text`.
fn assistant_text(id: &str, text: &str) -> SessionEvent {
    assistant(
        id,
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    )
}

/// A text block, as a model would produce it.
fn text(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

/// A tool invocation, as a model would produce it.
fn tool_call() -> ContentBlock {
    ContentBlock::ToolCall {
        id: ToolCallId::new("call-1").expect("test ids are valid"),
        name: "read_file".to_owned(),
        arguments: "{\"path\":\"a\"}".to_owned(),
    }
}

/// The turn-start event a session opens with.
fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

// ---- absence ------------------------------------------------------------------------------------

#[test]
fn an_empty_event_stream_has_no_output() {
    assert_eq!(final_output(&[]), None);
    assert_eq!(final_text(&[]), None);
}

#[test]
fn a_log_with_no_assistant_message_has_no_output() {
    let events = vec![
        turn_start(1),
        SessionEvent::user_text("m1", "why did the release job fail?"),
        SessionEvent::ToolResult {
            id: ToolCallId::new("call-1").expect("test ids are valid"),
            content: vec![text("log output")],
            is_error: false,
        },
    ];

    assert_eq!(final_output(&events), None);
    assert_eq!(final_text(&events), None);
}

#[test]
fn a_log_of_only_empty_messages_has_no_output() {
    // The loop appends a blockless message after a truncated step to own its usage line. That is
    // accounting, not an answer, so it must not count as one.
    let events = vec![
        turn_start(1),
        assistant("a1", vec![]),
        assistant("a2", vec![]),
    ];

    assert_eq!(final_output(&events), None);
    assert_eq!(final_text(&events), None);
}

// ---- selection ----------------------------------------------------------------------------------

#[test]
fn a_single_assistant_message_is_the_output() {
    let events = vec![
        turn_start(1),
        assistant_text("a1", "the job failed on the second retry"),
    ];

    let output = final_output(&events).expect("one assistant message was appended");
    assert_eq!(
        output.blocks,
        vec![text("the job failed on the second retry")]
    );
    assert_eq!(output.turn, 1);
    assert_eq!(
        final_text(&events).as_deref(),
        Some("the job failed on the second retry")
    );
}

#[test]
fn several_assistant_messages_select_the_last_one() {
    let events = vec![
        turn_start(1),
        assistant_text("a1", "let me read the file"),
        assistant_text("a2", "the retry budget was set to zero"),
        turn_start(2),
        assistant_text("a3", "and the second retry therefore never ran"),
    ];

    let output = final_output(&events).expect("assistant messages were appended");
    assert_eq!(
        output.blocks,
        vec![text("and the second retry therefore never ran")]
    );
    assert_eq!(
        final_text(&events).as_deref(),
        Some("and the second retry therefore never ran")
    );
}

#[test]
fn an_empty_message_after_the_answer_does_not_erase_it() {
    let events = vec![
        turn_start(1),
        assistant_text("a1", "step one"),
        assistant_text("a2", "step two"),
        assistant("a3", vec![]),
    ];

    assert_eq!(
        final_text(&events).as_deref(),
        Some("step two"),
        "a blockless usage message must not replace earlier output"
    );
}

#[test]
fn selection_follows_event_order_rather_than_the_turn_counter() {
    // A resumed or forked log can carry turn numbers that no longer track append order. Selection
    // is positional, so the later event wins even though its turn number is lower.
    let events = vec![
        turn_start(7),
        assistant_text("a1", "the older message by position"),
        turn_start(3),
        assistant_text("a2", "the newer message by position"),
    ];

    assert_eq!(
        final_text(&events).as_deref(),
        Some("the newer message by position"),
        "the message appended last wins, whatever its turn number says"
    );
}

#[test]
fn the_turn_is_the_one_open_when_the_message_was_appended() {
    let events = vec![
        turn_start(1),
        assistant_text("a1", "first turn"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: girder_core::TurnEndReason::Completed,
        },
        turn_start(2),
        assistant_text("a2", "second turn"),
    ];

    assert_eq!(
        final_output(&events)
            .expect("an assistant message was appended")
            .turn,
        2
    );
}

#[test]
fn a_message_before_any_turn_start_reports_turn_zero() {
    // A log fragment has no turn to attribute; zero is the documented "no turn" value rather than
    // a guess.
    let events = vec![assistant_text("a1", "orphan")];

    assert_eq!(
        final_output(&events)
            .expect("an assistant message was appended")
            .turn,
        0
    );
}

// ---- text versus output -------------------------------------------------------------------------

#[test]
fn a_tool_only_turn_has_output_but_no_text() {
    let events = vec![turn_start(1), assistant("a1", vec![tool_call()])];

    let output = final_output(&events).expect("a tool-calling message is still output");
    assert_eq!(output.blocks, vec![tool_call()]);
    assert_eq!(output.turn, 1);
    assert_eq!(
        final_text(&events),
        None,
        "the model asked for work instead of answering; that is not a text output"
    );
}

#[test]
fn a_reasoning_only_turn_has_output_but_no_text() {
    let events = vec![assistant(
        "a1",
        vec![ContentBlock::Reasoning {
            text: "the retry budget is zero".to_owned(),
        }],
    )];

    assert!(final_output(&events).is_some());
    assert_eq!(
        final_text(&events),
        None,
        "reasoning is not visible text, so it is not the answer"
    );
}

#[test]
fn an_empty_text_block_does_not_count_as_text() {
    let events = vec![turn_start(1), assistant("a1", vec![text("")])];

    assert_eq!(final_text(&events), None);
    assert_eq!(
        final_output(&events)
            .expect("a message with blocks is output, even empty ones")
            .blocks,
        vec![text("")]
    );
}

#[test]
fn empty_text_blocks_around_real_text_are_skipped() {
    let events = vec![assistant(
        "a1",
        vec![text(""), text("the second retry never ran"), text("")],
    )];

    assert_eq!(
        final_text(&events).as_deref(),
        Some("the second retry never ran"),
        "joining an empty block must not change the answer"
    );
}

#[test]
fn several_text_blocks_are_concatenated_in_order() {
    let events = vec![assistant(
        "a1",
        vec![text("the retry budget "), text("was set to zero")],
    )];

    assert_eq!(
        final_text(&events).as_deref(),
        Some("the retry budget was set to zero")
    );
}

#[test]
fn only_text_blocks_contribute_to_the_text() {
    let events = vec![assistant(
        "a1",
        vec![text("reading "), tool_call(), text("the log")],
    )];

    assert_eq!(final_text(&events).as_deref(), Some("reading the log"));
    assert_eq!(
        final_output(&events)
            .expect("an assistant message was appended")
            .blocks
            .len(),
        3,
        "final_output keeps every block, including the tool call"
    );
}

#[test]
fn whitespace_is_text_even_though_empty_text_is_not() {
    let events = vec![assistant("a1", vec![text(" "), text("\t")])];

    assert_eq!(
        final_text(&events).as_deref(),
        Some(" \t"),
        "the model wrote these characters, so they are the output"
    );
}
