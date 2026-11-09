//! Tests for the compaction summarizer (M3 of the v2 plan).
//!
//! The prompt shape is not cosmetic: the instruction rides as the final user message so the
//! auxiliary call reuses the provider's prefix cache, and the checkpoint is tagged so a second
//! compaction can find and merge it.

use girder_core::{
    AssistantMessage, CHECKPOINT_PREAMBLE, COMPACTION_INSTRUCTION, ChatMessage, ContentBlock,
    FinishReason, MessageId, Role, SUMMARY_CLOSE_TAG, SUMMARY_OPEN_TAG, SummaryError,
    checkpoint_message, extract_summary, is_checkpoint, summary_request,
};

fn assistant(text: &str, finish: Option<FinishReason>) -> AssistantMessage {
    AssistantMessage {
        content: if text.is_empty() {
            vec![]
        } else {
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }]
        },
        usage: None,
        finish,
    }
}

// ---- the request shape -------------------------------------------------------------------------

#[test]
fn the_instruction_is_the_final_user_message() {
    // Not a separate system prompt: keeping the conversation's own prefix makes the auxiliary call a
    // genuine prefix of the last routed request, so the provider's cache is reused rather than
    // invalidated.
    let conversation = vec![
        girder_core::Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "hello".to_owned(),
            }],
        ),
        girder_core::Message::new(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: "hi".to_owned(),
            }],
        ),
    ];

    let messages = summary_request(conversation);

    assert_eq!(messages.len(), 3, "the conversation is preserved verbatim");
    assert_eq!(messages[2].role, Role::User);
    let ContentBlock::Text { text } = &messages[2].content[0] else {
        panic!("expected text");
    };
    assert_eq!(text, COMPACTION_INSTRUCTION);
}

#[test]
fn the_instruction_requires_every_section() {
    // A summary that omits what was tried and failed makes the next model repeat the failure, so an
    // empty section must be visibly "(none)" rather than silently missing.
    for section in [
        "## Primary Request and Intent",
        "## Key Technical Concepts",
        "## Files and Code",
        "## Errors and Fixes",
        "## Pending Jobs",
        "## Current Work",
        "## Next Step",
        "## Critical Context",
    ] {
        assert!(
            COMPACTION_INSTRUCTION.contains(section),
            "the instruction must require {section}"
        );
    }
    assert!(COMPACTION_INSTRUCTION.contains("(none)"));
}

#[test]
fn the_instruction_tells_the_model_not_to_nest_checkpoints() {
    // A second compaction must merge into the prior checkpoint rather than copying it forward.
    assert!(COMPACTION_INSTRUCTION.contains(SUMMARY_OPEN_TAG));
    assert!(COMPACTION_INSTRUCTION.contains("PRIOR checkpoint"));
}

// ---- extracting the summary --------------------------------------------------------------------

#[test]
fn a_text_response_is_the_summary() {
    let message = assistant("## Current Work\n- testing", Some(FinishReason::Stop));
    assert_eq!(
        extract_summary(&message).unwrap(),
        "## Current Work\n- testing"
    );
}

#[test]
fn an_empty_response_is_refused() {
    // A provider can return a well-formed message with no content; presenting that as a checkpoint
    // would replace the conversation with nothing.
    let message = assistant("", Some(FinishReason::Stop));
    assert!(matches!(
        extract_summary(&message),
        Err(SummaryError::Empty)
    ));
}

#[test]
fn a_whitespace_response_is_refused() {
    let message = assistant("   \n\t\n", Some(FinishReason::Stop));
    assert!(matches!(
        extract_summary(&message),
        Err(SummaryError::Empty)
    ));
}

#[test]
fn a_provider_error_is_reported_as_such() {
    // Distinguishable from "the model said nothing": one is retryable, the other is a bad prompt.
    let message = assistant(
        "",
        Some(FinishReason::Error {
            message: "rate limited".to_owned(),
        }),
    );
    let error = extract_summary(&message).expect_err("provider error");
    let SummaryError::Provider { reason } = error else {
        panic!("expected Provider, got {error:?}");
    };
    assert_eq!(reason, "rate limited");
}

#[test]
fn an_aborted_request_is_reported() {
    let message = assistant("", Some(FinishReason::Aborted));
    assert!(matches!(
        extract_summary(&message),
        Err(SummaryError::Provider { .. })
    ));
}

// ---- the checkpoint ----------------------------------------------------------------------------

#[test]
fn a_checkpoint_carries_the_preamble_and_tags() {
    // Without the preamble the model reads the summary as a request addressed to it; without the
    // tags a second compaction cannot tell a checkpoint from something the user wrote.
    let message = checkpoint_message("## Next Step\n- keep going", MessageId::new("c1").unwrap());
    let text = message.text();

    assert!(text.starts_with(CHECKPOINT_PREAMBLE), "{text}");
    assert!(text.contains(SUMMARY_OPEN_TAG));
    assert!(text.contains(SUMMARY_CLOSE_TAG));
    assert!(text.contains("keep going"));
}

#[test]
fn a_checkpoint_is_attributed_as_recall() {
    // The provenance says the content came from another session's log, which is what a checkpoint is.
    let message = checkpoint_message("summary", MessageId::new("c1").unwrap());

    let girder_core::MessageSource::User { form } = &message.source else {
        panic!("expected a user source");
    };
    assert_eq!(form, &Some(girder_core::ContextForm::Recall));
}

#[test]
fn a_checkpoint_is_detected_by_its_tags() {
    let checkpoint = checkpoint_message("summary", MessageId::new("c1").unwrap());
    assert!(is_checkpoint(&checkpoint));
}

#[test]
fn an_ordinary_message_is_not_a_checkpoint() {
    // A heuristic on the text would misfire on a user who quotes a summary.
    let ordinary = ChatMessage::user(
        MessageId::new("m1").unwrap(),
        "here is a summary of what I did",
    );
    assert!(!is_checkpoint(&ordinary));
}

#[test]
fn a_message_quoting_only_the_open_tag_is_not_a_checkpoint() {
    let partial = ChatMessage::user(
        MessageId::new("m1").unwrap(),
        format!("I saw {SUMMARY_OPEN_TAG} somewhere"),
    );
    assert!(!is_checkpoint(&partial), "both tags are required");
}

#[test]
fn a_checkpoint_round_trips_through_json() {
    // It lands in the session log, so it must survive the wire.
    let message = checkpoint_message("## Next Step\n- go", MessageId::new("c1").unwrap());
    let text = serde_json::to_string(&message).unwrap();
    let restored: ChatMessage = serde_json::from_str(&text).unwrap();

    assert!(is_checkpoint(&restored));
}
