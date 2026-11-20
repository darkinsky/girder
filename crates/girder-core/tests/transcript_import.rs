//! Tests for importing a foreign chat transcript as session events.
//!
//! The properties under test, in the order they matter:
//!
//! - **an `Anthropic` body imports** — its system prompt is a top-level field, not a message, and an
//!   importer that only walked `messages` would lose it without noticing. This is the core case,
//!   because it is the one whose shape differs from the exporter's neutral form.
//! - **the imported events are a valid session** — every turn is closed, every step is paired, and
//!   every message's role agrees with its source. An import that produced events a replay would
//!   reject has not imported anything; it has produced a log that fails on its first use.
//! - **a malformed value is refused by position** — the alternative failure is an empty session
//!   reported as success, which loses the conversation and the report of it at once.
//! - **nothing is dropped silently** — a skipped role, a dropped block, and a skipped empty message
//!   each show up in `skipped` or in `warnings`. `skipped` is the number a caller shows a person, so
//!   it is asserted rather than inferred from the warning text.
//! - **`detect` answers from structure** — and refuses a value with no conversation in it, rather
//!   than guessing a format for something that is not a transcript at all.
//! - **the system prompt does not become this harness's prompt** — it becomes one advisory
//!   `developer/message`. Asserting the *absence* of a `system/message` is the half that matters,
//!   because a `system/message` here would make a replay explain a setup this harness never ran.
//! - **export then import round-trips the conversation** — not byte for byte, since the projection
//!   is lossy by design, but turn for turn and word for word.
//!
//! The two modules are included by path rather than through the crate root, so these tests compile
//! against them as written before the crate root wires them in.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

// The exporter is included whole, and this test file deliberately does not exercise its `len` and
// `is_empty` helpers — the importer asks about the events, not about the transcript. Without this
// the shared source would fail a dead-code check that is about *this* test, not about the module.
#[allow(dead_code)]
#[path = "../src/session/transcript.rs"]
pub mod session_transcript;

#[path = "../src/session/transcript_import.rs"]
pub mod transcript_import;

use girder_core::{
    ChatMessage, ContentBlock, MessageSource, SessionEvent, TurnEndReason, check_history,
    diagnose_health, is_healthy,
};
use serde_json::{Value, json};
use session_transcript::{Format, export, to_json};
use transcript_import::{IMPORTED_PROVIDER, ImportError, Imported, detect, import};

// ---- fixtures ---------------------------------------------------------------------------------

/// An `OpenAI` chat-completions body: the prompt is one more message.
fn openai_body() -> Value {
    json!({
        "messages": [
            { "role": "system", "content": "Be terse." },
            { "role": "user", "content": "How many files are here?" },
            { "role": "assistant", "content": "Let me look." },
            { "role": "user", "content": "Thanks." },
            { "role": "assistant", "content": "Two." },
        ]
    })
}

/// An `Anthropic` Messages body: the prompt is a top-level field.
fn anthropic_body() -> Value {
    json!({
        "system": "Be terse.",
        "messages": [
            { "role": "user", "content": "How many files are here?" },
            { "role": "assistant", "content": [{ "type": "text", "text": "Let me look." }] },
            { "role": "user", "content": [{ "type": "text", "text": "Thanks." }] },
            { "role": "assistant", "content": "Two." },
        ]
    })
}

/// The `type_name` of every event, in order.
fn types(imported: &Imported) -> Vec<&str> {
    imported
        .events
        .iter()
        .map(SessionEvent::type_name)
        .collect()
}

/// The text of every message-bearing event, in order.
fn texts(imported: &Imported) -> Vec<String> {
    imported
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::UserMessage { message }
            | SessionEvent::AssistantMessage { message }
            | SessionEvent::DeveloperMessage { message, .. } => Some(message.text()),
            _ => None,
        })
        .collect()
}

/// Asserts every invariant the session vocabulary imposes on an imported log.
///
/// Written once and called from every import test rather than asserted piecemeal: the point is that
/// *any* transcript this module accepts produces a log the rest of the crate can consume, and a
/// per-test assertion would only prove it for the fixture that test happened to use.
fn assert_valid_session(imported: &Imported) {
    assert!(
        is_healthy(&imported.events),
        "diagnose: {:?}",
        diagnose_health(&imported.events)
    );

    // Every message's role and source must agree, which is what a replay checks before it acts.
    let messages: Vec<ChatMessage> = imported
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::UserMessage { message }
            | SessionEvent::AssistantMessage { message }
            | SessionEvent::DeveloperMessage { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    check_history(&messages).expect("every imported message is consistent");

    // Turns are numbered from one, in order, and each is closed before the next begins.
    let mut open: Option<u64> = None;
    let mut expected: u64 = 1;
    for event in &imported.events {
        match event {
            SessionEvent::TurnStart { turn } => {
                assert!(
                    open.is_none(),
                    "turn {turn} started while {open:?} was open"
                );
                assert_eq!(*turn, expected, "turns are numbered in order");
                expected += 1;
                open = Some(*turn);
            }
            SessionEvent::TurnEnd { turn, .. } => {
                assert_eq!(open, Some(*turn), "turn {turn} ended without being open");
                open = None;
            }
            // A step may only appear inside a turn, and must be closed by its own end.
            SessionEvent::StepStart { turn, .. } | SessionEvent::StepEnd { turn, .. } => {
                assert_eq!(open, Some(*turn), "a step outside its turn");
            }
            _ => {}
        }
    }
    assert!(open.is_none(), "the last turn is closed: {open:?}");
}

/// The `detail` of a [`ImportError::NotThisFormat`], or a panic naming what else came back.
fn not_this_format(error: ImportError) -> String {
    if let ImportError::NotThisFormat { detail } = error {
        return detail;
    }
    panic!("expected NotThisFormat, got {error:?}");
}

/// The `detail` of a [`ImportError::Malformed`], or a panic naming what else came back.
///
/// A helper rather than a `match` at each call site: every refusal test wants the same thing, and
/// repeating the arm is how one of them ends up accepting the wrong variant.
fn malformed(error: ImportError) -> String {
    if let ImportError::Malformed { detail } = error {
        return detail;
    }
    panic!("expected Malformed, got {error:?}");
}

// ---- detecting the format ---------------------------------------------------------------------

#[test]
fn detect_recognises_an_openai_body() {
    assert_eq!(detect(&openai_body()), Some(Format::OpenAi));
}

#[test]
fn detect_recognises_an_anthropic_body() {
    // The top-level `system` field is what makes this unambiguous.
    assert_eq!(detect(&anthropic_body()), Some(Format::Anthropic));
}

#[test]
fn detect_recognises_anthropic_without_a_system_prompt() {
    // The overlap case: an Anthropic body with no prompt is shaped exactly like an OpenAI one, so
    // the answer here is the documented default. What matters is that it is *an* answer rather than
    // `None`, so a caller that only asks can still import.
    let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });

    assert_eq!(detect(&body), Some(Format::OpenAi));
}

#[test]
fn detect_uses_markers_that_are_not_the_system_field() {
    // Anthropic's own request fields, and OpenAI's tool channel, each decide alone.
    assert_eq!(
        detect(&json!({
            "anthropic_version": "2023-06-01",
            "messages": [{ "role": "user", "content": "hi" }],
        })),
        Some(Format::Anthropic)
    );
    assert_eq!(
        detect(&json!({
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": null, "tool_calls": [] },
            ],
        })),
        Some(Format::OpenAi)
    );
    assert_eq!(
        detect(&json!({
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "tool", "tool_call_id": "call-1", "content": "ok" },
            ],
        })),
        Some(Format::OpenAi)
    );
}

#[test]
fn detect_refuses_a_value_with_no_conversation_in_it() {
    // A format guess about something that is not a transcript would only produce a confusing error
    // later, so the answer is `None` rather than a default.
    for value in [
        json!("just a string"),
        json!([1, 2, 3]),
        json!({ "choices": [] }),
        json!({ "messages": "not an array" }),
    ] {
        assert_eq!(detect(&value), None, "{value} is not a chat payload");
    }
}

// ---- importing the two formats ----------------------------------------------------------------

#[test]
fn an_openai_transcript_imports_as_turns_and_steps() {
    let imported = import(&openai_body(), Format::OpenAi).expect("a valid transcript imports");

    // The system prompt is an advisory developer message inside turn 1's first step, so the step is
    // a real model call boundary and not a message pretending to be one.
    assert_eq!(
        types(&imported),
        [
            "turn/start",
            "step/start",
            "developer/message",
            "step/end",
            "user/message",
            "step/start",
            "assistant/message",
            "step/end",
            "turn/end",
            "turn/start",
            "user/message",
            "step/start",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(
        texts(&imported),
        [
            "Be terse.",
            "How many files are here?",
            "Let me look.",
            "Thanks.",
            "Two.",
        ]
    );
    assert_eq!(imported.skipped, 0);
    assert_valid_session(&imported);
}

#[test]
fn an_anthropic_transcript_imports_its_top_level_system_prompt() {
    // The core case: the prompt is not in `messages` at all, and an importer that walked only the
    // array would lose it while reporting success.
    let imported =
        import(&anthropic_body(), Format::Anthropic).expect("a valid transcript imports");

    let developer: Vec<String> = imported
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::DeveloperMessage { message, .. } => Some(message.text()),
            _ => None,
        })
        .collect();
    assert_eq!(developer, ["Be terse."]);

    // Typed `text` blocks and plain strings both became text, and the conversation is intact.
    assert_eq!(
        texts(&imported),
        [
            "Be terse.",
            "How many files are here?",
            "Let me look.",
            "Thanks.",
            "Two.",
        ]
    );
    assert_eq!(imported.skipped, 0);
    assert_valid_session(&imported);
}

#[test]
fn both_formats_import_the_same_conversation() {
    // The two shapes disagree only about where the prompt sits, so the import must not depend on
    // which of them the caller happened to hold.
    let openai = import(&openai_body(), Format::OpenAi).expect("openai");
    let anthropic = import(&anthropic_body(), Format::Anthropic).expect("anthropic");

    assert_eq!(types(&openai), types(&anthropic));
    assert_eq!(texts(&openai), texts(&anthropic));
    assert_eq!(openai.skipped, anthropic.skipped);
}

// ---- the frame is a valid session --------------------------------------------------------------

#[test]
fn an_imported_assistant_message_names_its_provider_as_imported() {
    // A transcript carries no model id. Reporting a real one would attribute the reply to a model
    // the source may never have used, and an empty provider reads as a decoding failure.
    let imported = import(&anthropic_body(), Format::Anthropic).expect("imports");
    let models: Vec<&MessageSource> = imported
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::AssistantMessage { message } => Some(&message.source),
            _ => None,
        })
        .collect();

    assert_eq!(models.len(), 2);
    for source in models {
        match source {
            MessageSource::Model {
                provider, model, ..
            } => {
                assert_eq!(provider, IMPORTED_PROVIDER);
                assert_eq!(model, IMPORTED_PROVIDER);
            }
            other => panic!("an assistant message must be model-sourced: {other:?}"),
        }
    }
}

#[test]
fn a_turn_with_only_an_assistant_first_reply_is_kept() {
    // Some sources open with the model speaking. That is turn 1, and the person's reply joins it:
    // nothing has been answered yet, so no second turn is invented in front of their first prompt.
    let value = json!({
        "messages": [
            { "role": "assistant", "content": "How can I help?" },
            { "role": "user", "content": "List the files." },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert_eq!(
        types(&imported),
        [
            "turn/start",
            "step/start",
            "assistant/message",
            "step/end",
            "user/message",
            "turn/end",
        ]
    );
    assert_valid_session(&imported);
}

#[test]
fn consecutive_user_messages_stay_in_one_turn() {
    // A person who sent two lines before the model answered. Splitting them into two turns would
    // claim two separate exchanges happened.
    let value = json!({
        "messages": [
            { "role": "user", "content": "one" },
            { "role": "user", "content": "two" },
            { "role": "assistant", "content": "both" },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert_eq!(
        imported
            .events
            .iter()
            .filter(|event| matches!(event, SessionEvent::TurnStart { .. }))
            .count(),
        1
    );
    assert_eq!(texts(&imported), ["one", "two", "both"]);
    assert_valid_session(&imported);
}

#[test]
fn every_turn_ends_as_completed() {
    // Nothing in a transcript failed, was blocked, or ran out of budget; the source simply ends.
    let imported = import(&openai_body(), Format::OpenAi).expect("imports");

    let ends: Vec<&TurnEndReason> = imported
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::TurnEnd { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 2);
    for reason in ends {
        assert_eq!(*reason, TurnEndReason::Completed);
    }
}

#[test]
fn a_developer_role_is_treated_as_instructions_not_as_conversation() {
    // OpenAI's `o1`-era spelling of the system prompt. It is not the person speaking and not the
    // model, so it belongs in the advisory message rather than in the turn.
    let value = json!({
        "messages": [
            { "role": "developer", "content": "Be terse." },
            { "role": "user", "content": "hi" },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert_eq!(
        types(&imported),
        [
            "turn/start",
            "step/start",
            "developer/message",
            "step/end",
            "user/message",
            "turn/end"
        ]
    );
    assert_eq!(imported.skipped, 0);
}

// ---- structures that are refused --------------------------------------------------------------

#[test]
fn a_value_that_is_not_an_object_is_refused() {
    let error =
        import(&json!([1, 2, 3]), Format::OpenAi).expect_err("an array is not a transcript");
    assert!(matches!(error, ImportError::Malformed { .. }), "{error:?}");
}

#[test]
fn a_value_without_a_messages_array_is_refused() {
    let error = import(&json!({ "choices": [] }), Format::OpenAi).expect_err("no messages");
    let detail = malformed(error);
    assert!(detail.contains("messages"), "{detail}");

    let error = import(&json!({ "messages": {} }), Format::OpenAi).expect_err("not an array");
    assert!(matches!(error, ImportError::Malformed { .. }), "{error:?}");
}

#[test]
fn a_message_that_is_not_an_object_is_refused_by_position() {
    // The alternative is to skip it, which would import a hole and report success.
    let value = json!({ "messages": [{ "role": "user", "content": "hi" }, 7] });
    let error = import(&value, Format::OpenAi).expect_err("message 1 is not an object");

    let detail = malformed(error);
    assert!(detail.contains('1'), "{detail}");
}

#[test]
fn a_message_without_a_string_role_is_refused() {
    let value = json!({ "messages": [{ "content": "hi" }] });
    let error = import(&value, Format::OpenAi).expect_err("no role");

    let detail = malformed(error);
    assert!(detail.contains("role"), "{detail}");
}

#[test]
fn a_content_of_the_wrong_type_is_refused() {
    let value = json!({ "messages": [{ "role": "user", "content": 42 }] });
    let error = import(&value, Format::OpenAi).expect_err("content is not text");

    let detail = malformed(error);
    assert!(detail.contains("content"), "{detail}");
}

#[test]
fn the_other_formats_marker_is_refused_rather_than_reinterpreted() {
    // Asked for OpenAI, given an Anthropic body. Reinterpreting it would drop the top-level prompt
    // and report success, which is the silent data loss this error exists to prevent.
    let error = import(&anthropic_body(), Format::OpenAi).expect_err("wrong format");
    let detail = not_this_format(error);
    assert!(detail.contains("anthropic"), "{detail}");
    assert!(detail.contains("openai"), "{detail}");

    let value = json!({
        "messages": [
            { "role": "user", "content": "hi" },
            { "role": "tool", "tool_call_id": "call-1", "content": "ok" },
        ]
    });
    let error = import(&value, Format::Anthropic).expect_err("wrong format");
    assert!(
        matches!(error, ImportError::NotThisFormat { .. }),
        "{error:?}"
    );
}

#[test]
fn the_overlap_is_accepted_by_either_format() {
    // A body with no decisive marker is a legal reading of both, so refusing it as either would
    // break the round trip for a session that had no prompt.
    let value = json!({ "messages": [{ "role": "user", "content": "hi" }] });

    for format in [Format::OpenAi, Format::Anthropic] {
        let imported = import(&value, format).expect("the overlap is accepted");
        assert_eq!(texts(&imported), ["hi"]);
    }
}

// ---- nothing is dropped silently --------------------------------------------------------------

#[test]
fn an_unsupported_role_is_skipped_and_counted() {
    // A tool message is OpenAI's native tool channel. It has no event to become here, so it is
    // counted rather than dropped in silence.
    let value = json!({
        "messages": [
            { "role": "user", "content": "read a.txt" },
            { "role": "assistant", "content": "reading" },
            { "role": "tool", "tool_call_id": "call-1", "content": "the contents" },
            { "role": "user", "content": "thanks" },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert_eq!(imported.skipped, 1);
    assert!(
        imported
            .warnings
            .iter()
            .any(|warning| warning.contains("tool")),
        "the skipped role is named: {:?}",
        imported.warnings
    );
    assert_eq!(texts(&imported), ["read a.txt", "reading", "thanks"]);
    assert_valid_session(&imported);
}

#[test]
fn a_tool_call_array_is_counted_as_dropped_rather_than_reconstructed() {
    // Rebuilding a `tool/call` here would need a provider-issued id, and a fabricated one is a lie a
    // replay and a provider both act on.
    let value = json!({
        "messages": [
            { "role": "user", "content": "list the files" },
            {
                "role": "assistant",
                "content": "looking",
                "tool_calls": [
                    { "id": "call-1", "type": "function", "function": { "name": "list", "arguments": "{}" } },
                    { "id": "call-2", "type": "function", "function": { "name": "stat", "arguments": "{}" } },
                ],
            },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert_eq!(imported.skipped, 0, "the message itself was imported");
    assert!(
        imported
            .warnings
            .iter()
            .any(|warning| warning.contains("tool_calls") && warning.contains('2')),
        "both calls are counted: {:?}",
        imported.warnings
    );
    assert!(
        !imported
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolCall { .. })),
        "no call event was fabricated"
    );
    assert_valid_session(&imported);
}

#[test]
fn a_message_with_no_text_is_skipped_and_counted() {
    // An image-only turn, or one whose only block was a tool call.
    let value = json!({
        "messages": [
            { "role": "user", "content": [{ "type": "image_url", "image_url": { "url": "http://x/y.png" } }] },
            { "role": "assistant", "content": null, "tool_calls": [ { "id": "call-1" } ] },
            { "role": "user", "content": "hello" },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert!(
        imported
            .warnings
            .iter()
            .any(|warning| warning.contains("no text")),
        "{:?}",
        imported.warnings
    );
    assert_eq!(texts(&imported), ["hello"]);
    assert_valid_session(&imported);
}

#[test]
fn a_non_text_block_inside_a_text_message_is_dropped_and_named() {
    // The message survives with its text; the block that could not be represented does not.
    let value = json!({
        "messages": [
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "look at this" },
                    { "type": "image_url", "image_url": { "url": "http://x/y.png" } },
                ],
            },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert_eq!(texts(&imported), ["look at this"]);
    assert!(
        imported
            .warnings
            .iter()
            .any(|warning| warning.contains("image_url")),
        "{:?}",
        imported.warnings
    );
}

#[test]
fn an_anthropic_tool_result_block_is_dropped_not_turned_into_an_event() {
    let value = json!({
        "messages": [
            { "role": "user", "content": [
                { "type": "text", "text": "result:" },
                { "type": "tool_result", "tool_use_id": "t-1", "content": "the contents" },
            ] },
        ]
    });
    let imported = import(&value, Format::Anthropic).expect("imports");

    assert_eq!(texts(&imported), ["result:"]);
    assert!(
        imported
            .events
            .iter()
            .all(|event| !matches!(event, SessionEvent::ToolResult { .. })),
        "no result event without a call to answer"
    );
    assert_valid_session(&imported);
}

// ---- the empty transcript ---------------------------------------------------------------------

#[test]
fn an_empty_messages_array_imports_as_no_events() {
    // A created-but-unused session is normal, and its export is an empty `messages` array. Importing
    // it back must produce an empty log, not an error.
    for format in [Format::OpenAi, Format::Anthropic] {
        let imported = import(&json!({ "messages": [] }), format).expect("empty is valid");
        assert!(imported.events.is_empty(), "{}", format.as_str());
        assert_eq!(imported.skipped, 0);
        assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);
        assert!(is_healthy(&imported.events));
    }
}

#[test]
fn a_prompt_with_no_messages_is_dropped_and_says_so() {
    // There is no turn to attach the advisory message to, so it is dropped — and reported, because
    // a prompt vanishing without a word is the failure this module exists to avoid.
    let imported = import(
        &json!({ "system": "Be terse.", "messages": [] }),
        Format::Anthropic,
    )
    .expect("imports");

    assert!(imported.events.is_empty());
    assert!(
        imported
            .warnings
            .iter()
            .any(|warning| warning.contains("dropped")),
        "{:?}",
        imported.warnings
    );
}

#[test]
fn a_transcript_of_only_unsupported_roles_produces_no_events_and_counts_them() {
    let value = json!({
        "messages": [
            { "role": "tool", "tool_call_id": "c1", "content": "a" },
            { "role": "tool", "tool_call_id": "c2", "content": "b" },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert!(imported.events.is_empty());
    assert_eq!(imported.skipped, 2);
    assert!(is_healthy(&imported.events));
}

// ---- the system prompt ------------------------------------------------------------------------

#[test]
fn the_system_prompt_never_becomes_a_system_message() {
    // The decisive assertion: a `system/message` here would claim this harness rendered a prompt it
    // never assembled, and a replay would explain the wrong setup.
    for (body, format) in [
        (openai_body(), Format::OpenAi),
        (anthropic_body(), Format::Anthropic),
    ] {
        let imported = import(&body, format).expect("imports");
        assert!(
            !imported
                .events
                .iter()
                .any(|event| matches!(event, SessionEvent::SystemMessage { .. })),
            "{} must not import a system message: {:?}",
            format.as_str(),
            types(&imported)
        );
        assert!(
            imported
                .warnings
                .iter()
                .any(|warning| warning.contains("developer")),
            "the translation is announced: {:?}",
            imported.warnings
        );
    }
}

#[test]
fn a_prompt_in_the_middle_of_a_conversation_is_joined_into_the_one_advisory_message() {
    // OpenAI accepts a system message at any position. A single advisory message cannot express the
    // position, so the texts are joined in log order and the position is not claimed.
    let value = json!({
        "messages": [
            { "role": "system", "content": "Be terse." },
            { "role": "user", "content": "hi" },
            { "role": "system", "content": "The workspace moved." },
            { "role": "assistant", "content": "noted" },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    let developer: Vec<String> = imported
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::DeveloperMessage { message, .. } => Some(message.text()),
            _ => None,
        })
        .collect();
    assert_eq!(developer, ["Be terse.\n\nThe workspace moved."]);
    assert_eq!(imported.skipped, 0);
}

#[test]
fn a_repeated_system_message_is_kept_once_per_record() {
    // Unlike the exporter, which collapses a per-step re-record, an importer cannot tell a repeat
    // from a genuine repeat of the same instruction, and dropping either would edit the source.
    let value = json!({
        "messages": [
            { "role": "system", "content": "Be terse." },
            { "role": "user", "content": "hi" },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    assert_eq!(
        imported
            .events
            .iter()
            .filter(|event| matches!(event, SessionEvent::DeveloperMessage { .. }))
            .count(),
        1
    );
}

// ---- the round trip ---------------------------------------------------------------------------

#[test]
fn export_then_import_keeps_the_conversation_and_settles_after_one_pass() {
    // Not byte for byte — the projection is lossy by design — but every turn and every word. The
    // one documented loss on the way out is the advisory prompt, because the exporter drops
    // developer messages; after that first pass the shape is a fixed point, which is what makes a
    // second migration of the same conversation a no-op rather than a slow decay.
    let value = json!({
        "messages": [
            { "role": "system", "content": "Be terse." },
            { "role": "user", "content": "How many files are here?" },
            { "role": "assistant", "content": "Let me look." },
            { "role": "user", "content": "Thanks." },
            { "role": "assistant", "content": "Two." },
        ]
    });
    let imported = import(&value, Format::OpenAi).expect("imports");

    for format in [Format::OpenAi, Format::Anthropic] {
        let once = import(&to_json(&export(&imported.events, format), format), format)
            .expect("the export imports back");

        // The prompt is gone, as documented; everything either party said is intact.
        assert_eq!(
            texts(&once),
            [
                "How many files are here?",
                "Let me look.",
                "Thanks.",
                "Two."
            ],
            "{}",
            format.as_str()
        );
        assert_eq!(once.skipped, 0, "{}", format.as_str());
        assert_valid_session(&once);

        let twice = import(&to_json(&export(&once.events, format), format), format)
            .expect("the second export imports back");
        assert_eq!(twice, once, "{} is a fixed point", format.as_str());
    }
}

#[test]
fn round_tripping_an_actual_session_matches_its_export() {
    // The direction that matters in practice: a session this harness wrote, exported, and read back
    // by someone else must import as the same conversation.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::user_text("m1", "How many files are here?"),
        SessionEvent::assistant_text("m2", "Let me look.", "deepseek"),
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    for format in [Format::OpenAi, Format::Anthropic] {
        let body = to_json(&export(&events, format), format);
        let imported = import(&body, format).expect("the export imports back");

        assert_eq!(
            texts(&imported),
            ["How many files are here?", "Let me look."],
            "{}",
            format.as_str()
        );
        assert_valid_session(&imported);
    }
}

#[test]
fn importing_the_same_value_twice_is_identical() {
    let value = anthropic_body();
    let first = import(&value, Format::Anthropic).expect("imports");
    let second = import(&value, Format::Anthropic).expect("imports");

    assert_eq!(first, second, "import is deterministic");
}

#[test]
fn every_imported_message_carries_exactly_one_text_block() {
    // A session message carries content blocks; this module produces exactly one, so a consumer
    // reading `message.text()` and one reading the blocks agree, and the role it was built with is
    // the role it carries.
    let imported = import(&anthropic_body(), Format::Anthropic).expect("imports");

    let mut seen = 0;
    for event in &imported.events {
        let (message, expected) = match event {
            SessionEvent::UserMessage { message } => (message, girder_core::ChatRole::User),
            SessionEvent::AssistantMessage { message } => {
                (message, girder_core::ChatRole::Assistant)
            }
            SessionEvent::DeveloperMessage { message, .. } => {
                (message, girder_core::ChatRole::Developer)
            }
            _ => continue,
        };
        seen += 1;
        assert_eq!(message.role, expected);
        assert_eq!(message.content.len(), 1, "{:?}", message.content);
        assert!(
            matches!(message.content.first(), Some(ContentBlock::Text { .. })),
            "{:?}",
            message.content
        );
        assert!(!message.text().is_empty());
    }
    assert_eq!(seen, 5, "every message this transcript holds was imported");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_transcript as transcript;
    pub use super::transcript_import as transcript_import;
}
