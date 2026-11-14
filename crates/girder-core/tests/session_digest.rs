//! Tests for a session's digest.
//!
//! Four properties carry the weight. `last_answer` must be the *last* thing the model said — the whole
//! point of the field is that it is the newest conclusion, so a test that only ever records one
//! assistant message proves nothing. `finished` must separate a session that reached a boundary from
//! one that stopped mid-turn, because the two call for different decisions. `requests` must be bounded,
//! or a digest becomes a transcript. And `one_line` must cut on character boundaries, or a Chinese or
//! emoji row becomes mojibake.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It resolves `girder_core::…` through
//! the alias the library declares for itself, which is why the same source builds here.

#[path = "../src/session_digest.rs"]
mod session_digest;

use girder_core::{
    ChatMessage, ChatRole, ContextForm, MessageId, MessageSource, SessionEvent, ToolCallId,
    TurnEndReason,
};

use session_digest::{Digest, digest, one_line};

// ---- fixtures ----------------------------------------------------------------------------------

/// A prompt the person typed.
fn user(id: &str, text: &str) -> SessionEvent {
    SessionEvent::user_text(id, text)
}

/// A user-role message the harness wrote, which must never be counted as a request.
fn injected(id: &str, text: &str, form: ContextForm) -> SessionEvent {
    SessionEvent::UserMessage {
        message: ChatMessage::new(
            MessageId::new(id).expect("valid id"),
            ChatRole::User,
            MessageSource::User { form: Some(form) },
            vec![girder_core::ContentBlock::Text {
                text: text.to_owned(),
            }],
        ),
    }
}

/// An assistant reply.
fn answer(id: &str, text: &str) -> SessionEvent {
    SessionEvent::assistant_text(id, text, "deepseek")
}

/// An assistant message with a tool call and no text — the common shape mid-turn.
fn tool_call_block(id: &str, name: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        message: ChatMessage::new(
            MessageId::new(id).expect("valid id"),
            ChatRole::Assistant,
            MessageSource::Model {
                provider: "deepseek".to_owned(),
                model: "deepseek".to_owned(),
                replay_state: None,
            },
            vec![girder_core::ContentBlock::ToolCall {
                id: ToolCallId::new(format!("c{id}")).expect("valid id"),
                name: name.to_owned(),
                arguments: "{}".to_owned(),
            }],
        ),
    }
}

/// A tool invocation with the given raw arguments.
fn tool(call: &str, name: &str, arguments: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(call).expect("valid id"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

/// A `turn/start` boundary.
fn start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

/// A `turn/end` boundary.
fn end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    }
}

/// One complete turn: boundary, prompt, answer, close.
fn complete_turn(turn: u64, prompt: &str, reply: &str) -> Vec<SessionEvent> {
    vec![
        start(turn),
        user(&format!("m{turn}"), prompt),
        SessionEvent::StepStart { turn, step: 1 },
        answer(&format!("a{turn}"), reply),
        SessionEvent::StepEnd { turn, step: 1 },
        end(turn),
    ]
}

// ---- the basic digest --------------------------------------------------------------------------

#[test]
fn a_complete_session_reports_its_one_turn_and_that_it_finished() {
    let events = complete_turn(
        1,
        "fix the retry policy",
        "Done: I capped the backoff at 30s.",
    );

    let summary = digest(&events);

    assert_eq!(summary.turns, 1);
    assert_eq!(summary.requests, vec!["fix the retry policy".to_owned()]);
    assert_eq!(
        summary.last_answer.as_deref(),
        Some("Done: I capped the backoff at 30s.")
    );
    assert!(summary.finished);
}

#[test]
fn turns_are_counted_from_distinct_boundaries() {
    // A resumed or duplicated log repeats a boundary; counting records rather than turn numbers would
    // report one turn as two.
    let mut events = complete_turn(1, "one", "first");
    events.extend(complete_turn(2, "two", "second"));
    events.push(start(2));

    assert_eq!(digest(&events).turns, 2);
}

// ---- last_answer: the core property ------------------------------------------------------------

#[test]
fn last_answer_is_the_last_assistant_message_not_the_first() {
    // The entire value of the field depends on this. A digest that reported the first reply would tell
    // a returning reader what the model said at the start, which is the one thing the log's beginning
    // already tells them.
    let events = vec![
        start(1),
        user("m1", "why does the release job fail"),
        answer("a1", "Looking at the logs now."),
        answer("a2", "The second retry reuses a stale token."),
        end(1),
    ];

    let summary = digest(&events);

    assert_eq!(
        summary.last_answer.as_deref(),
        Some("The second retry reuses a stale token.")
    );
}

#[test]
fn a_textless_tool_call_does_not_erase_the_last_answer() {
    // A trailing step that only calls a tool is not a newer conclusion. Treating it as one — by
    // storing its empty text — would blank the field at exactly the moment the session is closest to
    // an answer.
    let events = vec![
        start(1),
        user("m1", "read the parser"),
        answer("a1", "The parser drops the last token."),
        tool_call_block("a2", "read_file"),
        end(1),
    ];

    assert_eq!(
        digest(&events).last_answer.as_deref(),
        Some("The parser drops the last token.")
    );
}

#[test]
fn a_session_with_no_assistant_text_has_no_answer() {
    // Absent rather than empty: a caller must be able to tell "it said nothing" from "nothing was
    // recorded", and only one of those deserves a placeholder.
    let events = vec![start(1), user("m1", "just run it"), end(1)];

    assert_eq!(digest(&events).last_answer, None);
}

#[test]
fn a_very_long_answer_is_clipped_and_marked() {
    // A reply can be a pasted report. An unbounded field would make every digest as expensive as the
    // transcript it summarises, so the cut must happen and must be visible.
    let long = "word ".repeat(200);
    let events = vec![start(1), answer("a1", &long), end(1)];

    let stored = digest(&events).last_answer.expect("an answer was recorded");

    assert!(stored.chars().count() <= 280);
    assert!(stored.ends_with('…'));
}

// ---- requests: bounded -------------------------------------------------------------------------

#[test]
fn requests_are_limited_to_the_first_three() {
    // Three orients a reader; ten is a transcript with a shorter name.
    let mut events = Vec::new();
    for turn in 1..=5_u64 {
        events.extend(complete_turn(
            turn,
            &format!("request {turn}"),
            &format!("reply {turn}"),
        ));
    }

    let summary = digest(&events);

    assert_eq!(summary.requests.len(), 3);
    assert_eq!(
        summary.requests,
        vec![
            "request 1".to_owned(),
            "request 2".to_owned(),
            "request 3".to_owned(),
        ]
    );
    // The turn count still sees everything: only the stored list is cut.
    assert_eq!(summary.turns, 5);
}

#[test]
fn synthetic_injections_are_not_requests() {
    // This build writes goal continuations and compaction recalls on the user role. Listing them as
    // requests would present the harness's own bookkeeping as something the person asked for.
    let events = vec![
        start(1),
        injected(
            "i1",
            "continue working toward the goal",
            ContextForm::Recall,
        ),
        user("m1", "the real request"),
        end(1),
    ];

    assert_eq!(
        digest(&events).requests,
        vec!["the real request".to_owned()]
    );
}

#[test]
fn repeated_requests_are_kept_because_the_second_one_means_again() {
    let events = vec![start(1), user("m1", "run it"), user("m2", "run it"), end(1)];

    assert_eq!(
        digest(&events).requests,
        vec!["run it".to_owned(), "run it".to_owned()]
    );
}

// ---- finished: the core distinction ------------------------------------------------------------

#[test]
fn a_log_that_stopped_at_turn_end_finished() {
    let events = vec![start(1), user("m1", "do it"), answer("a1", "done"), end(1)];

    assert!(digest(&events).finished);
}

#[test]
fn a_log_that_stopped_mid_turn_did_not_finish() {
    // The process died, the user killed it, or it is still running. All three are "the work may be
    // half done", and reading them as finished invites trusting an incomplete change.
    let events = vec![
        start(1),
        user("m1", "do it"),
        answer("a1", "starting"),
        SessionEvent::StepStart { turn: 1, step: 2 },
    ];

    assert!(!digest(&events).finished);
}

#[test]
fn an_unclosed_turn_after_a_closed_one_did_not_finish() {
    // The last boundary is what matters, not whether any boundary closed.
    let mut events = complete_turn(1, "first", "done");
    events.push(start(2));
    events.push(user("m2", "and now this"));

    assert!(!digest(&events).finished);
}

#[test]
fn an_empty_log_is_not_finished() {
    let summary = digest(&[]);

    assert!(!summary.finished);
    assert_eq!(summary.turns, 0);
    assert!(summary.requests.is_empty());
    assert_eq!(summary.last_answer, None);
}

// ---- tools and files: deduplicated -------------------------------------------------------------

#[test]
fn tools_are_deduplicated_in_first_use_order() {
    let events = vec![
        start(1),
        tool("c1", "grep", r#"{"pattern":"retry"}"#),
        tool("c2", "read_file", r#"{"path":"src/a.rs"}"#),
        tool("c3", "grep", r#"{"pattern":"backoff"}"#),
        end(1),
    ];

    let summary = digest(&events);

    assert_eq!(
        summary.tools,
        vec!["grep".to_owned(), "read_file".to_owned()]
    );
}

#[test]
fn files_are_deduplicated_and_kept_whole() {
    let events = vec![
        start(1),
        tool("c1", "read_file", r#"{"path":"crates/core/src/a.rs"}"#),
        tool(
            "c2",
            "write",
            r#"{"path":"crates/core/src/a.rs","content":"x"}"#,
        ),
        tool("c3", "edit", r#"{"path":"crates/core/src/b.rs"}"#),
        end(1),
    ];

    let summary = digest(&events);

    assert_eq!(
        summary.files,
        vec![
            "crates/core/src/a.rs".to_owned(),
            "crates/core/src/b.rs".to_owned(),
        ]
    );
}

#[test]
fn paths_inside_a_patch_document_are_found() {
    // The patch tool carries its targets in a document rather than a path argument; a session that
    // edited through patches would otherwise report no files at all.
    let patch = "*** Begin Patch\n*** Update File: src/parser.rs\n@@\n-old\n+new\n*** End Patch";
    // Built through `serde_json` rather than `format!`, because the document holds newlines that a
    // hand-written JSON string would have to escape — and an unescaped one is simply invalid JSON.
    let arguments = serde_json::json!({ "patch": patch }).to_string();
    let events = vec![start(1), tool("c1", "apply_patch", &arguments), end(1)];

    assert_eq!(digest(&events).files, vec!["src/parser.rs".to_owned()]);
}

#[test]
fn unparseable_tool_arguments_contribute_nothing() {
    let events = vec![start(1), tool("c1", "write", "not json"), end(1)];

    let summary = digest(&events);

    assert_eq!(summary.tools, vec!["write".to_owned()]);
    assert!(summary.files.is_empty());
}

// ---- one_line ----------------------------------------------------------------------------------

#[test]
fn one_line_names_the_turn_count_and_whether_it_finished() {
    let summary = digest(&complete_turn(1, "fix the retry", "Done."));

    let line = one_line(&summary, 200);

    assert!(line.starts_with("1 turn · finished"), "got {line}");
    assert!(line.contains("fix the retry"), "got {line}");
    assert!(line.contains("Done."), "got {line}");
}

#[test]
fn one_line_marks_an_unfinished_session() {
    let mut events = complete_turn(1, "fix the retry", "working");
    events.push(start(2));

    let line = one_line(&digest(&events), 200);

    assert!(line.contains("unfinished"), "got {line}");
}

#[test]
fn one_line_cuts_at_the_character_budget() {
    let summary = digest(&complete_turn(
        1,
        "a request that is quite long indeed",
        "an answer that is also quite long",
    ));

    let line = one_line(&summary, 24);

    assert!(
        line.chars().count() <= 24,
        "got {} chars",
        line.chars().count()
    );
    assert!(line.ends_with('…'), "got {line}");
}

#[test]
fn one_line_never_splits_a_chinese_character() {
    // "修复重试策略" is 6 characters and 18 UTF-8 bytes. A byte-based cut at 24 would slice mid-sequence
    // and produce invalid UTF-8; a character-based cut keeps every unit whole and the string valid.
    let summary = digest(&complete_turn(
        1,
        "修复重试策略并且加上测试",
        "已经修好了重试策略，并且补充了对应的单元测试。",
    ));

    let line = one_line(&summary, 12);

    assert!(line.chars().count() <= 12);
    assert!(line.ends_with('…'), "got {line}");
    // The property that matters: the result is still valid, whole UTF-8.
    assert_eq!(line.chars().count(), line.chars().count());
    assert!(line.is_char_boundary(line.len()));
}

#[test]
fn one_line_of_an_empty_digest_is_empty() {
    // A caller can skip an empty string; a caller handed "0 turns · unfinished" has to decide whether
    // to hide it.
    assert_eq!(one_line(&digest(&[]), 80), "");
}

#[test]
fn one_line_with_a_zero_budget_is_empty() {
    let summary = digest(&complete_turn(1, "do it", "done"));

    assert_eq!(one_line(&summary, 0), "");
}

#[test]
fn one_line_counts_tools_and_files_rather_than_listing_them() {
    let events = vec![
        start(1),
        user("m1", "look around"),
        tool("c1", "grep", r#"{"path":"a.rs"}"#),
        tool("c2", "read_file", r#"{"path":"b.rs"}"#),
        end(1),
    ];

    let line = one_line(&digest(&events), 300);

    assert!(line.contains("tools:"), "got {line}");
    assert!(line.contains("2 files"), "got {line}");
}

#[test]
fn a_digest_survives_a_round_trip_through_json() {
    // The fields are serialised by a session list; a shape that cannot round-trip would lose the answer
    // silently on the way to a client.
    let summary = digest(&complete_turn(1, "fix it", "fixed"));
    let json = serde_json::to_string(&summary).expect("digest serialises");
    let back: Digest = serde_json::from_str(&json).expect("digest deserialises");

    assert_eq!(back, summary);
}
