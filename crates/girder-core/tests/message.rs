//! Tests for the message model (M3 of the v2 plan).
//!
//! The property that matters is that provenance and role are separate axes: a `user` message may be
//! authored by the person, relayed by another agent, or injected by a plugin, and a replay must be
//! able to tell them apart. The consistency check exists because a hand-edited log can lie.

use girder_core::{AgentId, MessageId, ToolCallId};
use girder_core::{
    CONTEXT_SUMMARY_MAX_CHARS, ChatMessage, ChatRole, ContentBlock, ContextForm, MessageError,
    MessageSource, bound_context_summary, check_history,
};

fn id(value: &str) -> MessageId {
    MessageId::new(value).expect("valid id")
}

// ---- roles ------------------------------------------------------------------------------------

#[test]
fn a_user_message_carries_text_and_a_user_source() {
    let message = ChatMessage::user(id("m1"), "hello");

    assert_eq!(message.role, ChatRole::User);
    assert_eq!(message.text(), "hello");
    assert!(matches!(message.source, MessageSource::User { .. }));
}

#[test]
fn a_system_message_is_attributed_to_the_system_prompt() {
    let message = ChatMessage::system(id("m1"), "be brief");

    assert_eq!(message.role, ChatRole::System);
    assert!(matches!(message.source, MessageSource::SystemPrompt));
}

#[test]
fn five_roles_exist_because_developer_is_its_own_role() {
    // A tool addition or removal rides a turn but is neither the model nor the person speaking;
    // folding it into either makes replay ambiguous.
    let developer = ChatMessage::new(
        id("m1"),
        ChatRole::Developer,
        MessageSource::User { form: None },
        vec![ContentBlock::Text {
            text: "tool added".to_owned(),
        }],
    );

    assert_eq!(developer.role, ChatRole::Developer);
    assert!(
        developer.is_consistent(),
        "a developer turn may carry any producer"
    );
}

// ---- provenance -------------------------------------------------------------------------------

#[test]
fn a_model_message_records_the_provider_for_replay() {
    let message = ChatMessage::new(
        id("m1"),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "deepseek".to_owned(),
            model: "deepseek-chat".to_owned(),
            replay_state: Some(serde_json::json!({"sig": "abc"})),
        },
        vec![ContentBlock::Text {
            text: "hi".to_owned(),
        }],
    );

    let MessageSource::Model {
        provider,
        model,
        replay_state,
    } = &message.source
    else {
        panic!("expected a model source");
    };
    assert_eq!(provider, "deepseek");
    assert_eq!(model, "deepseek-chat");
    assert!(replay_state.is_some(), "replay state must survive");
}

#[test]
fn a_tool_message_names_the_call_it_answers() {
    let call = ToolCallId::new("call-1").unwrap();
    let message = ChatMessage::new(
        id("m1"),
        ChatRole::Tool,
        MessageSource::Tool {
            call_id: call.clone(),
        },
        vec![ContentBlock::Text {
            text: "result".to_owned(),
        }],
    );

    assert_eq!(message.source.call_id(), Some(&call));
}

#[test]
fn a_relayed_message_records_the_sending_agent() {
    // Another agent's message is `role: user` from this agent's viewpoint but has different
    // provenance; the two axes are why that is expressible.
    let sender = AgentId::new("agent-7").unwrap();
    let message = ChatMessage::new(
        id("m1"),
        ChatRole::User,
        MessageSource::Relay {
            from: sender.clone(),
        },
        vec![ContentBlock::Text {
            text: "a peer says hi".to_owned(),
        }],
    );

    let MessageSource::Relay { from } = &message.source else {
        panic!("expected a relay source");
    };
    assert_eq!(from, &sender);
    assert_eq!(
        message.role,
        ChatRole::User,
        "relayed content still occupies a user turn"
    );
}

#[test]
fn a_user_source_can_declare_a_context_form() {
    let message = ChatMessage::new(
        id("m1"),
        ChatRole::User,
        MessageSource::User {
            form: Some(ContextForm::Instructions),
        },
        vec![ContentBlock::Text {
            text: "read AGENTS.md".to_owned(),
        }],
    );

    let MessageSource::User { form } = &message.source else {
        panic!("expected a user source");
    };
    assert_eq!(*form, Some(ContextForm::Instructions));
}

// ---- serialization ----------------------------------------------------------------------------

#[test]
fn the_source_round_trips_with_its_kind_tag() {
    let message = ChatMessage::new(
        id("m1"),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::Text {
            text: "x".to_owned(),
        }],
    );

    let text = serde_json::to_string(&message).unwrap();
    assert!(text.contains(r#""kind":"model""#), "{text}");

    let restored: ChatMessage = serde_json::from_str(&text).unwrap();
    assert_eq!(restored, message);
}

#[test]
fn an_unknown_source_kind_is_refused() {
    // A closed enum means a newer producer's kind is a decode error rather than a silently dropped
    // field. That is the trade for exhaustiveness.
    let text = r#"{"id":"m1","role":"user","source":{"kind":"future-thing"},"content":[]}"#;
    let error = serde_json::from_str::<ChatMessage>(text).expect_err("unknown kind");

    assert!(error.to_string().contains("future-thing") || error.to_string().contains("unknown"));
}

#[test]
fn the_replay_state_is_omitted_when_absent() {
    let source = MessageSource::Model {
        provider: "p".to_owned(),
        model: "m".to_owned(),
        replay_state: None,
    };
    let text = serde_json::to_string(&source).unwrap();

    assert!(!text.contains("replay_state"), "{text}");
}

// ---- consistency ------------------------------------------------------------------------------

#[test]
fn an_assistant_message_from_a_non_model_is_inconsistent() {
    // A hand-edited log can lie, so replay checks rather than trusts.
    let message = ChatMessage::new(
        id("m1"),
        ChatRole::Assistant,
        MessageSource::User { form: None },
        vec![],
    );

    assert!(!message.is_consistent());
}

#[test]
fn a_tool_message_without_a_call_id_is_inconsistent() {
    let message = ChatMessage::new(
        id("m1"),
        ChatRole::Tool,
        MessageSource::User { form: None },
        vec![],
    );

    assert!(!message.is_consistent());
}

#[test]
fn a_consistent_history_passes_the_check() {
    let history = vec![
        ChatMessage::system(id("m1"), "prompt"),
        ChatMessage::user(id("m2"), "hello"),
        ChatMessage::new(
            id("m3"),
            ChatRole::Assistant,
            MessageSource::Model {
                provider: "p".to_owned(),
                model: "m".to_owned(),
                replay_state: None,
            },
            vec![],
        ),
        ChatMessage::new(
            id("m4"),
            ChatRole::Tool,
            MessageSource::Tool {
                call_id: ToolCallId::new("c1").unwrap(),
            },
            vec![],
        ),
    ];

    check_history(&history).expect("consistent");
}

#[test]
fn check_history_names_the_offending_message() {
    let history = vec![
        ChatMessage::user(id("ok"), "fine"),
        ChatMessage::new(
            id("bad-one"),
            ChatRole::Assistant,
            MessageSource::User { form: None },
            vec![],
        ),
    ];

    let error = check_history(&history).expect_err("inconsistent");
    // One variant today; the pattern stays exhaustive so adding a second is a compile error here.
    let MessageError::InconsistentRole { id, .. } = error;
    assert_eq!(id, "bad-one");
}

// ---- context summary bounding -----------------------------------------------------------------

#[test]
fn a_short_summary_is_unchanged() {
    assert_eq!(bound_context_summary("short"), "short");
}

#[test]
fn a_long_summary_is_ellipsized_at_the_bound() {
    // Producers commit this to a durable log; its inputs are caller text with no length of its own.
    let long = "x".repeat(CONTEXT_SUMMARY_MAX_CHARS + 50);
    let bounded = bound_context_summary(&long);

    assert_eq!(bounded.chars().count(), CONTEXT_SUMMARY_MAX_CHARS);
    assert!(bounded.ends_with('…'));
}

#[test]
fn the_bound_counts_characters_not_bytes() {
    // A CJK summary of N characters is 3N bytes; a byte bound would truncate it at a third.
    let cjk = "字".repeat(CONTEXT_SUMMARY_MAX_CHARS + 10);
    let bounded = bound_context_summary(&cjk);

    assert_eq!(bounded.chars().count(), CONTEXT_SUMMARY_MAX_CHARS);
}

#[test]
fn a_summary_exactly_at_the_bound_is_unchanged() {
    let exact = "y".repeat(CONTEXT_SUMMARY_MAX_CHARS);
    assert_eq!(
        bound_context_summary(&exact).chars().count(),
        CONTEXT_SUMMARY_MAX_CHARS
    );
    assert!(!bound_context_summary(&exact).ends_with('…'));
}
