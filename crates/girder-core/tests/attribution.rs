//! Tests for per-producer character attribution.
//!
//! The core claim under test is that attribution measures *volume*, not message count: a single
//! long model message must outweigh several short user messages. Every other test here exists to
//! pin down a boundary that a plausible implementation could get wrong — the empty conversation,
//! the share's range, and the unknown producer that must not be absorbed into a known bucket.

// The crate re-exports this type as `ContentAttribution` to keep it distinct from the provider
// `Attribution` vocabulary; the alias here just keeps the assertions readable.
use girder_core::{
    AgentId, ChatMessage, ChatRole, ContentAttribution as Attribution, ContentBlock, MessageId,
    MessageSource, ToolCallId,
};

/// Builds a message from a source and a single text block.
///
/// The role is derived from the source so that the fixture is always consistent: attribution reads
/// the source, and a mismatched role would make the test assert the wrong axis.
fn message(index: usize, source: MessageSource, text: &str) -> ChatMessage {
    let role = match &source {
        MessageSource::Model { .. } => ChatRole::Assistant,
        MessageSource::Tool { .. } => ChatRole::Tool,
        MessageSource::SystemPrompt => ChatRole::System,
        MessageSource::User { .. } | MessageSource::Relay { .. } => ChatRole::User,
    };
    let id = MessageId::new(format!("m-{index}")).expect("valid id");
    ChatMessage::new(
        id,
        role,
        source,
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    )
}

/// A source that belongs to the person.
fn user_source() -> MessageSource {
    MessageSource::User { form: None }
}

/// A source that belongs to the model.
fn model_source() -> MessageSource {
    MessageSource::Model {
        provider: "deepseek".to_owned(),
        model: "deepseek-v4".to_owned(),
        replay_state: None,
    }
}

/// A source belonging to a tool result.
fn tool_source() -> MessageSource {
    MessageSource::Tool {
        call_id: ToolCallId::new("call-1").expect("valid id"),
    }
}

/// A source belonging to another agent, which this type does not classify.
fn relay_source() -> MessageSource {
    MessageSource::Relay {
        from: AgentId::new("parent/child").expect("valid id"),
    }
}

#[test]
fn only_user_messages_are_attributed_to_the_user() {
    let messages = vec![
        message(0, user_source(), "hello"),
        message(1, user_source(), "again"),
    ];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.user, 10);
    assert_eq!(attribution.model, 0);
    assert_eq!(attribution.tool, 0);
    assert_eq!(attribution.system, 0);
    assert_eq!(attribution.unknown, 0);
    assert_eq!(attribution.total(), 10);
    assert!((attribution.model_share() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn only_model_messages_are_attributed_to_the_model() {
    let messages = vec![message(0, model_source(), "the model wrote this")];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.user, 0);
    // "the model wrote this" is 20 characters, spaces included.
    assert_eq!(attribution.model, 20);
    assert_eq!(attribution.total(), 20);
    assert!((attribution.model_share() - 1.0).abs() < f64::EPSILON);
}

#[test]
fn mixed_producers_are_counted_separately() {
    let messages = vec![
        message(0, MessageSource::SystemPrompt, "be brief"), // 8
        message(1, user_source(), "explain"),                // 7
        message(2, model_source(), "certainly"),             // 9
        message(3, tool_source(), "file contents"),          // 13
    ];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.system, 8);
    assert_eq!(attribution.user, 7);
    assert_eq!(attribution.model, 9);
    assert_eq!(attribution.tool, 13);
    assert_eq!(attribution.total(), 37);
    // The shares are fractions of one whole, so the model share plus every non-model share is one
    // when nothing is unknown. The non-model total is written as its literal (7 + 13 + 8) rather
    // than recomputed, so the assertion cannot pass by repeating a bug in the implementation.
    let non_model = 28.0 / 37.0;
    assert!((attribution.model_share() + non_model - 1.0).abs() < 1e-12);
}

#[test]
fn characters_outweigh_message_count() {
    // Three short user messages and one long model message: a message count would call this 3:1 in
    // the user's favour, while the model actually produced the overwhelming share of the text.
    let long_reply = "x".repeat(300);
    let messages = vec![
        message(0, user_source(), "ok"),   // 2
        message(1, user_source(), "yes"),  // 3
        message(2, user_source(), "fine"), // 4
        message(3, model_source(), &long_reply),
    ];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.user, 9);
    assert_eq!(attribution.model, 300);
    assert!(attribution.model > attribution.user);
    assert!((attribution.model_share() - 300.0 / 309.0).abs() < 1e-12);
    // A per-message count would report 0.25 here; the character share is nearly 0.97.
    assert!(attribution.model_share() > 0.9);
}

#[test]
fn an_empty_message_list_attributes_nothing() {
    let attribution = Attribution::of(&[]);

    assert_eq!(attribution, Attribution::default());
    assert_eq!(attribution.total(), 0);
    // Compared with a tolerance rather than `==` so the assertion still reads as "zero" if the
    // implementation ever computes the share instead of returning the literal.
    assert!(attribution.model_share().abs() < f64::EPSILON);
}

#[test]
fn model_share_is_zero_without_any_model_text() {
    // The boundary that a naive `model / (user + model)` would divide by zero on.
    let messages = vec![message(0, user_source(), "no reply yet")];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.model, 0);
    assert!(!attribution.model_share().is_nan());
    assert!((attribution.model_share() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn model_share_stays_within_zero_and_one() {
    let messages = vec![
        message(0, user_source(), "a"),
        message(1, model_source(), "b"),
        message(2, tool_source(), "c"),
        message(3, MessageSource::SystemPrompt, "d"),
        message(4, relay_source(), "e"),
    ];
    let attribution = Attribution::of(&messages);
    let share = attribution.model_share();

    assert!((0.0..=1.0).contains(&share));
    assert!((share - 0.2).abs() < 1e-12);
}

#[test]
fn a_relayed_message_is_not_credited_to_the_user_or_the_model() {
    // A relay is another agent's text. Crediting it to the person would overstate human authorship,
    // and crediting it to the model would overstate this model's, so it lands in `unknown`.
    let messages = vec![
        message(0, user_source(), "hi"),               // 2
        message(1, model_source(), "hello"),           // 5
        message(2, relay_source(), "parent speaking"), // 15
    ];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.user, 2);
    assert_eq!(attribution.model, 5);
    assert_eq!(attribution.unknown, 15);
    assert_eq!(attribution.total(), 22);
    assert!((attribution.model_share() - 5.0 / 22.0).abs() < 1e-12);
}

#[test]
fn unclassified_text_does_not_inflate_the_model_share() {
    // The failure this guards against: folding unknown producers into the model bucket would make
    // a session look model-dominated when another agent wrote it.
    let messages = vec![
        message(0, model_source(), "m"),             // 1
        message(1, relay_source(), &"r".repeat(99)), // 99
    ];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.unknown, 99);
    assert!((attribution.model_share() - 0.01).abs() < 1e-12);
}

#[test]
fn characters_are_counted_not_bytes() {
    // Three CJK characters are nine bytes; reporting nine would overstate this producer by 3x.
    let messages = vec![message(0, user_source(), "你好吗")];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.user, 3);
    assert_eq!(attribution.total(), 3);
}

#[test]
fn reasoning_and_tool_call_arguments_count_as_model_text() {
    // The model authored both; only the user does not see the reasoning.
    let id = MessageId::new("m-0").expect("valid id");
    let messages = vec![ChatMessage::new(
        id,
        ChatRole::Assistant,
        model_source(),
        vec![
            ContentBlock::Text {
                text: "answer".to_owned(),
            },
            ContentBlock::Reasoning {
                text: "because".to_owned(),
            },
            ContentBlock::ToolCall {
                id: ToolCallId::new("call-2").expect("valid id"),
                name: "read".to_owned(),
                arguments: "{\"p\":1}".to_owned(),
            },
        ],
    )];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.model, 6 + 7 + 7);
    assert_eq!(attribution.unknown, 0);
}

#[test]
fn attachment_handles_contribute_no_characters() {
    // The bytes live in the attachment service; counting the handle would measure bookkeeping.
    let id = MessageId::new("m-0").expect("valid id");
    let messages = vec![ChatMessage::new(
        id,
        ChatRole::User,
        user_source(),
        vec![
            ContentBlock::Text {
                text: "see attached".to_owned(),
            },
            ContentBlock::Image {
                attachment: "blob-very-long-handle-name".to_owned(),
            },
            ContentBlock::File {
                attachment: "blob-very-long-handle-name".to_owned(),
            },
        ],
    )];
    let attribution = Attribution::of(&messages);

    assert_eq!(attribution.user, 12);
}
