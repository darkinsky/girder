//! Tests for the session event vocabulary (M3 of the v2 plan).
//!
//! The vocabulary is the log's wire format, so two properties matter: a tag must never change
//! silently, and every event must carry what a replay needs. Both are asserted here rather than
//! implied by other tests.

use girder_core::{ContentBlock, SessionEvent, TurnEndReason};
use girder_core::{MessageId, ToolCallId};

// ---- tags are the wire format -----------------------------------------------------------------

#[test]
fn every_tag_matches_the_upstream_spelling() {
    // A changed tag silently breaks every existing log, so each is pinned.
    let cases: Vec<(SessionEvent, &str)> = vec![
        (SessionEvent::TurnStart { turn: 1 }, "turn/start"),
        (
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            "turn/end",
        ),
        (SessionEvent::StepStart { turn: 1, step: 1 }, "step/start"),
        (SessionEvent::StepEnd { turn: 1, step: 1 }, "step/end"),
        (SessionEvent::user_text("m1", "hi"), "user/message"),
        (
            SessionEvent::assistant_text("m2", "hi", "p"),
            "assistant/message",
        ),
        (
            SessionEvent::ToolCall {
                id: ToolCallId::new("c").expect("valid"),
                name: "n".to_owned(),
                arguments: "{}".to_owned(),
            },
            "tool/call",
        ),
        (
            SessionEvent::ToolResult {
                id: ToolCallId::new("c").expect("valid"),
                content: vec![],
                is_error: false,
            },
            "tool/result",
        ),
        (
            SessionEvent::LlmRetry {
                attempt: 1,
                reason: None,
            },
            "llm/retry",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(event.type_name(), expected);
    }
}

#[test]
fn a_tag_survives_the_json_round_trip() {
    let event = SessionEvent::StepStart { turn: 2, step: 3 };
    let text = serde_json::to_string(&event).unwrap();

    assert!(text.contains(r#""type":"step/start""#), "{text}");
    assert_eq!(serde_json::from_str::<SessionEvent>(&text).unwrap(), event);
}

#[test]
fn an_unknown_tag_is_refused() {
    // A log written by a newer build must fail loudly rather than replay a partial history.
    let error = serde_json::from_str::<SessionEvent>(r#"{"type":"turn/rewind","turn":1}"#)
        .expect_err("unknown variant");

    assert!(error.to_string().contains("turn/rewind") || error.to_string().contains("unknown"));
}

// ---- the step layer ---------------------------------------------------------------------------

#[test]
fn a_step_names_its_turn_and_step() {
    // A turn is a sequence of steps, not one model call. Without the step layer a replay could not
    // reconstruct the model's view mid-turn.
    let event = SessionEvent::StepStart { turn: 3, step: 7 };

    assert_eq!(event.turn(), Some(3));
    let SessionEvent::StepStart { turn, step } = event else {
        panic!("expected StepStart");
    };
    assert_eq!((turn, step), (3, 7));
}

#[test]
fn a_turn_bounds_its_steps() {
    let start = SessionEvent::TurnStart { turn: 1 };
    let step = SessionEvent::StepStart { turn: 1, step: 1 };
    let end = SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    };

    assert_eq!(start.turn(), Some(1));
    assert_eq!(step.turn(), Some(1));
    assert_eq!(end.turn(), Some(1));
}

#[test]
fn a_message_does_not_name_a_turn() {
    // Messages carry their own identity; the turn is implied by position in the log. Naming it
    // twice would let the two disagree.
    assert_eq!(SessionEvent::user_text("m1", "x").turn(), None);
}

// ---- messages carry identity and provenance ---------------------------------------------------

#[test]
fn a_user_event_carries_a_full_message() {
    let event = SessionEvent::user_text("msg-7", "hello");
    let SessionEvent::UserMessage { message } = event else {
        panic!("expected UserMessage");
    };

    assert_eq!(message.id, MessageId::new("msg-7").expect("valid"));
    assert_eq!(message.text(), "hello");
    assert!(message.is_consistent());
}

#[test]
fn an_assistant_event_records_the_provider() {
    let event = SessionEvent::assistant_text("msg-8", "hi", "deepseek");
    let SessionEvent::AssistantMessage { message } = event else {
        panic!("expected AssistantMessage");
    };

    let girder_core::MessageSource::Model { provider, .. } = &message.source else {
        panic!("expected a model source");
    };
    assert_eq!(provider, "deepseek");
}

#[test]
fn a_developer_event_names_the_turn_and_step_it_rode() {
    let developer = girder_core::ChatMessage::new(
        MessageId::new("msg-9").expect("valid"),
        girder_core::ChatRole::Developer,
        girder_core::MessageSource::User { form: None },
        vec![ContentBlock::Text {
            text: "a tool was added".to_owned(),
        }],
    );
    let event = SessionEvent::DeveloperMessage {
        turn: 2,
        step: 1,
        message: developer,
    };

    assert_eq!(event.turn(), Some(2));
    assert_eq!(event.type_name(), "developer/message");
}

// ---- turn end reasons -------------------------------------------------------------------------

#[test]
fn the_end_reason_names_the_kind() {
    // A turn can end for reasons a model never reports, and overloading the model's stop reason
    // would make "the model finished" and "the user pressed Ctrl-C" the same value.
    let cases = [
        (TurnEndReason::Completed, "completed"),
        (
            TurnEndReason::Aborted {
                cause: "user".to_owned(),
            },
            "aborted",
        ),
        (TurnEndReason::Blocked, "blocked"),
        (
            TurnEndReason::Failed {
                message: "boom".to_owned(),
                code: "SERVER".to_owned(),
            },
            "failed",
        ),
    ];

    for (reason, kind) in cases {
        let text = serde_json::to_string(&reason).unwrap();
        assert!(text.contains(&format!(r#""kind":"{kind}""#)), "{text}");
    }
}

#[test]
fn a_model_stop_reason_maps_to_a_turn_reason() {
    use girder_core::FinishReason;

    assert_eq!(
        TurnEndReason::from(FinishReason::Stop),
        TurnEndReason::Completed
    );
    assert_eq!(
        TurnEndReason::from(FinishReason::ToolCalls),
        TurnEndReason::Completed,
        "a tool-calling turn is not a failure"
    );
    assert!(matches!(
        TurnEndReason::from(FinishReason::Length),
        TurnEndReason::Failed { .. }
    ));
    assert!(matches!(
        TurnEndReason::from(FinishReason::Error {
            message: "x".to_owned()
        }),
        TurnEndReason::Failed { .. }
    ));
}

#[test]
fn an_aborted_turn_is_distinguishable_from_a_failed_one() {
    // The whole reason `TurnEndReason` is not `FinishReason`.
    let aborted = serde_json::to_string(&TurnEndReason::Aborted {
        cause: "user".to_owned(),
    })
    .unwrap();
    let failed = serde_json::to_string(&TurnEndReason::Failed {
        message: "boom".to_owned(),
        code: "SERVER".to_owned(),
    })
    .unwrap();

    assert_ne!(aborted, failed);
    assert!(aborted.contains("aborted"));
    assert!(failed.contains("failed"));
}

// ---- retry events -----------------------------------------------------------------------------

#[test]
fn a_retry_event_records_the_attempt() {
    // Without it, two assistant messages for one step look like a protocol violation.
    let event = SessionEvent::LlmRetry {
        attempt: 2,
        reason: Some("RATE_LIMIT".to_owned()),
    };

    let text = serde_json::to_string(&event).unwrap();
    assert!(text.contains(r#""attempt":2"#), "{text}");
    assert!(text.contains("RATE_LIMIT"));
}

#[test]
fn a_first_attempt_omits_the_reason() {
    let event = SessionEvent::LlmRetry {
        attempt: 1,
        reason: None,
    };
    let text = serde_json::to_string(&event).unwrap();

    assert!(!text.contains("reason"), "{text}");
}
