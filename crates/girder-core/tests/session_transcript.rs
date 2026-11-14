//! Tests for exporting a session into another product's chat vocabulary.
//!
//! The properties under test, in the order they matter:
//!
//! - **only the conversation is exported** — turns, steps, usage, model selection, sandbox mode, and
//!   the title are frame and configuration, and a transcript carrying them would not be a
//!   conversation. This is the difference between "export" and "dump the log".
//! - **tool calls survive as labelled text** — both protocols have a native tool protocol and
//!   neither is usable (see the module docs), so the whole value of the projection is that a call
//!   and its result are still *visible* and clearly not something a participant said. A test that
//!   only checked "the call is not lost" would pass on a projection that attributed it to the
//!   assistant, which is the mistake worth catching.
//! - **the system prompt sits in a different place per format** — `OpenAI` keeps it as a message,
//!   `Anthropic` lifts it to a top-level field and has no `system` role at all. This is the one
//!   structural difference between the two, so it is asserted structurally, not by substring.
//! - **an empty session exports as an empty transcript** — a created-but-unused session is normal,
//!   and reporting an error would claim it was missing.
//! - **the JSON is the shape a client posts** — field names, value types, and the absence of a
//!   fabricated `model` or `max_tokens`.
//!
//! The module is included by path rather than through the crate root, so these tests compile against
//! it as written before the crate root wires it in. It spells its paths through `girder_core::`, the
//! alias the library declares for itself, so the same source builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/session_transcript.rs"]
mod session_transcript;

use girder_core::{ChatMessage, ChatRole, ContentBlock, MessageId, MessageSource, ToolCallId};
use girder_core::{SessionEvent, TurnEndReason};
use serde_json::json;
use session_transcript::{
    Format, MAX_TOOL_TEXT_CHARS, Message, ROLE_ASSISTANT, ROLE_SYSTEM, ROLE_USER, Transcript,
    export, to_json,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// A user message event with `text`.
fn user(id: &str, text: &str) -> SessionEvent {
    SessionEvent::user_text(id, text)
}

/// An assistant message event with `text`.
fn assistant(id: &str, text: &str) -> SessionEvent {
    SessionEvent::assistant_text(id, text, "deepseek")
}

/// An assistant message event carrying exactly `blocks`.
fn assistant_blocks(id: &str, blocks: Vec<ContentBlock>) -> SessionEvent {
    SessionEvent::AssistantMessage {
        message: model_message(id, blocks),
    }
}

/// A model-sourced message carrying `blocks`.
fn model_message(id: &str, blocks: Vec<ContentBlock>) -> ChatMessage {
    ChatMessage::new(
        MessageId::new(id).expect("valid message id"),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "deepseek".to_owned(),
            model: "deepseek-chat".to_owned(),
            replay_state: None,
        },
        blocks,
    )
}

/// A system prompt record, as a turn records it once per step.
fn system(turn: u64, step: u64, text: &str) -> SessionEvent {
    SessionEvent::SystemMessage {
        turn,
        step,
        message: ChatMessage::new(
            MessageId::new(format!("sys-{turn}-{step}")).expect("valid message id"),
            ChatRole::System,
            MessageSource::SystemPrompt,
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        ),
    }
}

/// A tool call event.
fn tool_call(id: &str, name: &str, arguments: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(id).expect("valid call id"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

/// A tool result event with one text block.
fn tool_result(id: &str, text: &str, is_error: bool) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(id).expect("valid call id"),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        is_error,
    }
}

/// One ordinary turn: prompt, tool call, result, answer — with the frame and bookkeeping around it.
fn conversation() -> Vec<SessionEvent> {
    vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        system(1, 1, "You are a helpful engineer."),
        user("m1", "How many files are here?"),
        assistant("m2", "Let me look."),
        tool_call("call-1", "list_files", "{\"path\":\".\"}"),
        tool_result("call-1", "a.txt\nb.txt", false),
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 12,
            output_tokens: 8,
        },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        assistant("m3", "There are two files."),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ]
}

/// The roles of `transcript`, in order.
fn roles(transcript: &Transcript) -> Vec<&str> {
    transcript
        .messages
        .iter()
        .map(|message| message.role.as_str())
        .collect()
}

/// The contents of `transcript`, in order.
fn contents(transcript: &Transcript) -> Vec<&str> {
    transcript
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect()
}

// ---- the basic projection ---------------------------------------------------------------------

#[test]
fn a_conversation_exports_its_messages_in_order() {
    let transcript = export(&conversation(), Format::OpenAi);

    // The system prompt first, then what was said. `Let me look.` is a real assistant message, so it
    // is exported even though the next thing the model did was call a tool.
    assert_eq!(
        contents(&transcript),
        [
            "You are a helpful engineer.",
            "How many files are here?",
            "Let me look.",
            "[tool call] list_files {\"path\":\".\"}",
            "[tool result] list_files\na.txt\nb.txt",
            "There are two files.",
        ]
    );
}

#[test]
fn frame_and_bookkeeping_events_are_not_exported() {
    let transcript = export(&conversation(), Format::OpenAi);
    let text = contents(&transcript).join("\n");

    // Turn and step boundaries, usage, model selection, and settings describe how a turn ran. None
    // of them may reach the transcript as text.
    for absent in [
        "turn/start",
        "turn/end",
        "step/start",
        "step/end",
        "model/usage",
        "deepseek-chat",
        "12",
        "reason:",
    ] {
        assert!(
            !text.contains(absent),
            "`{absent}` is bookkeeping and must not be exported: {text}"
        );
    }
}

#[test]
fn only_user_and_assistant_roles_are_exported_as_conversation() {
    let transcript = export(&conversation(), Format::OpenAi);

    for role in roles(&transcript) {
        assert!(
            [ROLE_USER, ROLE_ASSISTANT, ROLE_SYSTEM].contains(&role),
            "unexpected role `{role}`"
        );
    }
    assert_eq!(
        roles(&transcript),
        [
            ROLE_SYSTEM,
            ROLE_USER,
            ROLE_ASSISTANT,
            ROLE_USER,
            ROLE_USER,
            ROLE_ASSISTANT,
        ]
    );
}

#[test]
fn an_empty_session_exports_an_empty_transcript() {
    let transcript = export(&[], Format::OpenAi);

    assert!(transcript.is_empty());
    assert_eq!(transcript.len(), 0);
    assert_eq!(
        to_json(&transcript, Format::OpenAi),
        json!({ "messages": [] })
    );
}

#[test]
fn a_session_with_no_messages_exports_an_empty_transcript() {
    // A session created and immediately ended: frame events only, nothing said.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    assert!(export(&events, Format::OpenAi).is_empty());
    assert!(export(&events, Format::Anthropic).is_empty());
}

// ---- tool calls: the core decision ------------------------------------------------------------

#[test]
fn a_tool_call_becomes_labelled_user_text_not_assistant_text() {
    let events = vec![
        user("m1", "Check the build."),
        assistant_blocks(
            "m2",
            vec![ContentBlock::ToolCall {
                id: ToolCallId::new("call-1").expect("valid call id"),
                name: "cargo_build".to_owned(),
                arguments: "{\"jobs\":2}".to_owned(),
            }],
        ),
        tool_call("call-1", "cargo_build", "{\"jobs\":2}"),
        tool_result("call-1", "finished in 3s", false),
        assistant("m3", "It builds."),
    ];
    let transcript = export(&events, Format::OpenAi);

    // The call is exported once — from its own event, not twice via the message's block — and it
    // rides the user channel with a label, because a tool result is input to the model and the two
    // target protocols have no third participant to attribute environment text to.
    assert_eq!(
        contents(&transcript),
        [
            "Check the build.",
            "[tool call] cargo_build {\"jobs\":2}",
            "[tool result] cargo_build\nfinished in 3s",
            "It builds.",
        ]
    );
    assert_eq!(
        roles(&transcript),
        [ROLE_USER, ROLE_USER, ROLE_USER, ROLE_ASSISTANT]
    );
    // Nothing the model did not say is attributed to it.
    let said_by_model: Vec<&str> = transcript
        .messages
        .iter()
        .filter(|message| message.role == ROLE_ASSISTANT)
        .map(|message| message.content.as_str())
        .collect();
    assert_eq!(said_by_model, ["It builds."]);
}

#[test]
fn an_assistant_step_that_only_called_tools_produces_no_empty_message() {
    let events = vec![
        user("m1", "List the files."),
        assistant_blocks(
            "m2",
            vec![ContentBlock::ToolCall {
                id: ToolCallId::new("call-1").expect("valid call id"),
                name: "list_files".to_owned(),
                arguments: "{}".to_owned(),
            }],
        ),
        tool_call("call-1", "list_files", "{}"),
    ];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(
        contents(&transcript),
        ["List the files.", "[tool call] list_files {}"]
    );
    assert!(
        !transcript
            .messages
            .iter()
            .any(|message| message.content.is_empty()),
        "an empty message would be a blank bubble: {:?}",
        transcript.messages
    );
}

#[test]
fn a_failed_result_says_it_failed() {
    let events = vec![
        tool_call("call-1", "read_file", "{\"path\":\"gone.txt\"}"),
        tool_result("call-1", "no such file", true),
    ];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(
        contents(&transcript),
        [
            "[tool call] read_file {\"path\":\"gone.txt\"}",
            "[tool result] read_file (failed)\nno such file",
        ]
    );
}

#[test]
fn a_call_with_no_result_is_left_as_the_call_alone() {
    // An interrupted turn: the log is exported as it is, not repaired.
    let events = vec![
        user("m1", "Run the tests."),
        tool_call("call-1", "cargo_test", "{}"),
    ];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(
        contents(&transcript),
        ["Run the tests.", "[tool call] cargo_test {}"]
    );
}

#[test]
fn a_result_whose_text_is_empty_still_reports_what_ran() {
    let events = vec![
        tool_call("call-1", "touch", "{\"path\":\"a\"}"),
        tool_result("call-1", "", false),
    ];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(
        contents(&transcript),
        [
            "[tool call] touch {\"path\":\"a\"}",
            "[tool result] touch\n(no text output)",
        ]
    );
}

#[test]
fn a_result_with_no_recorded_call_is_labelled_with_its_id() {
    // A log truncated before the call, or one whose call was lost. The result is still exported —
    // dropping it would hide a real tool run — under the only identifier available.
    let events = vec![tool_result("call-9", "orphan output", false)];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(
        contents(&transcript),
        ["[tool result] call-9\norphan output"]
    );
}

#[test]
fn oversized_tool_output_is_cut_and_marked() {
    let long = "x".repeat(MAX_TOOL_TEXT_CHARS + 500);
    let events = vec![
        tool_call("call-1", "cat", "{}"),
        tool_result("call-1", &long, false),
    ];
    let transcript = export(&events, Format::OpenAi);

    let body = transcript
        .messages
        .last()
        .expect("the result is exported")
        .content
        .clone();
    assert!(body.contains("[truncated]"), "the cut is announced: {body}");
    assert!(
        body.chars().count() < MAX_TOOL_TEXT_CHARS + 100,
        "the body is bounded, not merely marked"
    );
}

#[test]
fn a_multibyte_tool_result_is_cut_on_a_character_boundary() {
    // Three-byte characters: a byte-offset cut would split one and produce invalid UTF-8. The test
    // reads the result back as a `String`, so a corrupt cut would fail here rather than in a client.
    let long = "工".repeat(MAX_TOOL_TEXT_CHARS + 10);
    let events = vec![
        tool_call("call-1", "cat", "{}"),
        tool_result("call-1", &long, false),
    ];
    let transcript = export(&events, Format::OpenAi);
    let body = transcript
        .messages
        .last()
        .expect("exported")
        .content
        .clone();

    assert!(body.starts_with("[tool result] cat\n工"));
    assert!(body.contains("[truncated]"));
    // The character count is what bounds the text, so the prefix keeps every code point intact.
    assert_eq!(
        body.chars().filter(|c| *c == '工').count(),
        MAX_TOOL_TEXT_CHARS
    );
}

#[test]
fn reasoning_is_not_exported() {
    let events = vec![assistant_blocks(
        "m2",
        vec![
            ContentBlock::Reasoning {
                text: "the person is asking about the build".to_owned(),
            },
            ContentBlock::Text {
                text: "It builds.".to_owned(),
            },
        ],
    )];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(contents(&transcript), ["It builds."]);
}

#[test]
fn attachments_become_labels_rather_than_bytes_or_nothing() {
    let events = vec![assistant_blocks(
        "m2",
        vec![
            ContentBlock::Image {
                attachment: "att-1".to_owned(),
            },
            ContentBlock::File {
                attachment: "att-2".to_owned(),
            },
        ],
    )];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(
        contents(&transcript),
        ["[image attachment: att-1][file attachment: att-2]"]
    );
}

#[test]
fn developer_messages_are_not_exported() {
    // A tool addition riding a turn is this harness's bookkeeping; exporting it would smuggle a tool
    // description into the receiving product's conversation.
    let events = vec![
        user("m1", "hello"),
        SessionEvent::DeveloperMessage {
            turn: 1,
            step: 1,
            message: ChatMessage::new(
                MessageId::new("dev-1").expect("valid message id"),
                ChatRole::Developer,
                MessageSource::User { form: None },
                vec![ContentBlock::Text {
                    text: "tool `read_file` was added".to_owned(),
                }],
            ),
        },
    ];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(contents(&transcript), ["hello"]);
}

// ---- the system prompt: where the two formats differ ------------------------------------------

#[test]
fn openai_keeps_the_system_prompt_as_a_message() {
    let events = vec![system(1, 1, "Be terse."), user("m1", "hi")];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(roles(&transcript), [ROLE_SYSTEM, ROLE_USER]);
    let body = to_json(&transcript, Format::OpenAi);
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "Be terse.");
    assert!(
        body.get("system").is_none(),
        "OpenAI has no top-level `system` field: {body}"
    );
}

#[test]
fn anthropic_lifts_the_system_prompt_out_of_the_messages() {
    let events = vec![system(1, 1, "Be terse."), user("m1", "hi")];
    let transcript = export(&events, Format::Anthropic);

    // The neutral transcript still holds the prompt as a `system` message; the placement is the
    // renderer's decision.
    assert!(roles(&transcript).contains(&ROLE_SYSTEM));

    let body = to_json(&transcript, Format::Anthropic);
    assert_eq!(body["system"], "Be terse.");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hi");
    // `system` is not a role the Anthropic API accepts inside `messages`.
    for message in messages {
        assert_ne!(
            message["role"], "system",
            "Anthropic has no `system` role inside messages: {body}"
        );
    }
}

#[test]
fn anthropic_omits_the_system_field_when_there_is_no_prompt() {
    let transcript = export(&[user("m1", "hi")], Format::Anthropic);
    let body = to_json(&transcript, Format::Anthropic);

    assert!(
        body.get("system").is_none(),
        "an absent prompt is not an empty one: {body}"
    );
    assert_eq!(body["messages"].as_array().expect("array").len(), 1);
}

#[test]
fn a_prompt_rerecorded_unchanged_appears_once() {
    // The prompt is recorded every step, and copying each record would repeat it down the whole
    // transcript.
    let events = vec![
        system(1, 1, "Be terse."),
        system(1, 2, "Be terse."),
        system(2, 1, "Be terse."),
        user("m1", "hi"),
    ];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(contents(&transcript), ["Be terse.", "hi"]);
}

#[test]
fn openai_places_a_changed_prompt_where_it_changed() {
    let events = vec![
        system(1, 1, "Be terse."),
        user("m1", "hi"),
        system(1, 2, "Be terse. The workspace moved."),
        assistant("m2", "noted"),
    ];
    let transcript = export(&events, Format::OpenAi);

    assert_eq!(
        roles(&transcript),
        [ROLE_SYSTEM, ROLE_USER, ROLE_SYSTEM, ROLE_ASSISTANT]
    );
    assert_eq!(
        contents(&transcript),
        ["Be terse.", "hi", "Be terse. The workspace moved.", "noted"]
    );
}

#[test]
fn anthropic_joins_a_changed_prompt_into_one_leading_system_field() {
    // Anthropic has no positional system channel, so the prompt's changes are joined in log order
    // rather than dropped or repeated mid-conversation.
    let events = vec![
        system(1, 1, "Be terse."),
        user("m1", "hi"),
        system(1, 2, "Be terse. The workspace moved."),
        assistant("m2", "noted"),
    ];
    let body = to_json(&export(&events, Format::Anthropic), Format::Anthropic);

    assert_eq!(
        body["system"],
        "Be terse.\n\nBe terse. The workspace moved."
    );
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
}

// ---- the JSON shape ---------------------------------------------------------------------------

#[test]
fn openai_json_is_a_messages_array_of_role_and_content_objects() {
    let mut transcript = Transcript::default();
    transcript.messages.push(Message::new(ROLE_USER, "hello"));

    assert_eq!(
        to_json(&transcript, Format::OpenAi),
        json!({ "messages": [{ "role": "user", "content": "hello" }] })
    );
}

#[test]
fn anthropic_json_is_a_system_field_plus_a_messages_array() {
    let mut transcript = Transcript::default();
    transcript
        .messages
        .push(Message::new(ROLE_SYSTEM, "be nice"));
    transcript.messages.push(Message::new(ROLE_USER, "hello"));

    assert_eq!(
        to_json(&transcript, Format::Anthropic),
        json!({
            "system": "be nice",
            "messages": [{ "role": "user", "content": "hello" }],
        })
    );
}

#[test]
fn the_json_is_only_the_messages_portion_of_a_request() {
    // A session log does not name a model or a token limit, so a renderer that filled them in would
    // be making a claim the log does not support.
    for format in [Format::OpenAi, Format::Anthropic] {
        let body = to_json(&export(&conversation(), format), format);
        for absent in ["model", "max_tokens", "maxTokens", "temperature", "stream"] {
            assert!(
                body.get(absent).is_none(),
                "{} must not invent a `{absent}` field: {body}",
                format.as_str()
            );
        }
    }
}

#[test]
fn content_is_a_string_in_both_formats() {
    // Typed content blocks are a protocol feature whose shape differs between the two formats; a
    // string is the one shape both read, and every block this projection keeps is text.
    for format in [Format::OpenAi, Format::Anthropic] {
        let body = to_json(&export(&conversation(), format), format);
        let messages = body["messages"].as_array().expect("messages array");
        assert!(!messages.is_empty());
        for message in messages {
            assert!(
                message["content"].is_string(),
                "{} content must be a string: {message}",
                format.as_str()
            );
            assert!(message["role"].is_string());
        }
    }
}

#[test]
fn the_same_session_renders_the_same_json_twice() {
    let events = conversation();
    let first = to_json(&export(&events, Format::OpenAi), Format::OpenAi);
    let second = to_json(&export(&events, Format::OpenAi), Format::OpenAi);

    assert_eq!(first, second, "export is deterministic");
}

#[test]
fn format_names_are_distinct_and_stable() {
    assert_eq!(Format::OpenAi.as_str(), "openai");
    assert_eq!(Format::Anthropic.as_str(), "anthropic");
    assert_ne!(Format::OpenAi, Format::Anthropic);
}

#[test]
fn a_transcript_built_by_hand_keeps_its_roles_verbatim() {
    // `Message` takes a `String` role so a caller can build one without this module's vocabulary
    // agreeing in advance; the renderer copies what it is given rather than remapping it.
    let transcript = Transcript {
        messages: vec![Message::new("tool", "raw")],
    };
    let body = to_json(&transcript, Format::OpenAi);

    assert_eq!(body["messages"][0]["role"], "tool");
}
