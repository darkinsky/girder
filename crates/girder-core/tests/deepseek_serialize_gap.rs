//! Tests for `DeepSeek Messages` request serialization.
//!
//! The reference vectors are transcriptions, not interpretations. Each test names the upstream case
//! it comes from (`packages/llm/llm-deepseek/tests/serialize.spec.ts`), because the point of a port
//! is that an implementation that disagrees with these is wrong regardless of how defensible the
//! disagreement looks. Where a case cannot be transcribed — upstream's suite has `plugin:text`
//! blocks, `tool-addition` blocks, and an image preparation pipeline that this crate's durable model
//! does not have — the deviation is stated in the test that covers the neighbouring behaviour.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `deepseek_serialize_gap`; adding `pub mod
//! deepseek_serialize_gap;` and its `pub use` belongs to the integrator, along with switching
//! `llm_deepseek::DeepSeekProvider::body` over to it. Compiling the module into this test binary
//! directly makes the tests independent of that wiring and keeps the crate root untouched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "tests assert by panicking; a failed unwrap is a failed test, and the per-rule tables \
              are clearer as long cases than as dozens of near-identical functions"
)]

#[path = "../src/deepseek_serialize_gap.rs"]
mod deepseek_serialize_gap;

use deepseek_serialize_gap::{
    ImageLookup, ReasoningEffort, ReplayNotice, SerializeError, SerializeImage, SerializeRequest,
    SerializeRoute, SerializeTool, SilentReplay, developer_update_from_blocks, image_handle_text,
    replay_state, serialize, tool_input,
};
use girder_core::llm_stream_gap::DeepSeekReplayBlock;
use girder_core::{
    ChatMessage, ChatRole, ContentBlock, LlmRequest, Message, MessageId, MessageSource, Role,
    ToolCallId, ToolSchema,
};
use serde_json::{Value, json};

const MODEL: &str = "deepseek-official";

// =============================================================================================
// Fixtures
// =============================================================================================

/// Builds a durable message with a unique id.
fn message(role: ChatRole, source: MessageSource, content: Vec<ContentBlock>) -> ChatMessage {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ChatMessage::new(
        MessageId::new(format!("m-{id}")).unwrap(),
        role,
        source,
        content,
    )
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.to_owned(),
    }
}

fn reasoning(value: &str) -> ContentBlock {
    ContentBlock::Reasoning {
        text: value.to_owned(),
    }
}

fn call(id: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        id: ToolCallId::new(id).unwrap(),
        name: "read".to_owned(),
        arguments: r#"{"path":"a"}"#.to_owned(),
    }
}

fn image(attachment: &str) -> ContentBlock {
    ContentBlock::Image {
        attachment: attachment.to_owned(),
    }
}

fn user(value: &str) -> ChatMessage {
    message(
        ChatRole::User,
        MessageSource::User { form: None },
        vec![text(value)],
    )
}

fn user_blocks(content: Vec<ContentBlock>) -> ChatMessage {
    message(ChatRole::User, MessageSource::User { form: None }, content)
}

fn system(value: &str) -> ChatMessage {
    message(
        ChatRole::System,
        MessageSource::SystemPrompt,
        vec![text(value)],
    )
}

fn system_blocks(content: Vec<ContentBlock>) -> ChatMessage {
    message(ChatRole::System, MessageSource::SystemPrompt, content)
}

fn developer(content: Vec<ContentBlock>) -> ChatMessage {
    message(
        ChatRole::Developer,
        MessageSource::User { form: None },
        content,
    )
}

fn assistant(content: Vec<ContentBlock>) -> ChatMessage {
    message(
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "deepseek-official".to_owned(),
            model: MODEL.to_owned(),
            replay_state: None,
        },
        content,
    )
}

fn assistant_with_replay(content: Vec<ContentBlock>, replay: Option<Value>) -> ChatMessage {
    message(
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "deepseek-official".to_owned(),
            model: MODEL.to_owned(),
            replay_state: replay,
        },
        content,
    )
}

fn assistant_from(model: &str, content: Vec<ContentBlock>) -> ChatMessage {
    message(
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "deepseek-official".to_owned(),
            model: model.to_owned(),
            replay_state: None,
        },
        content,
    )
}

fn result(id: &str, content: Vec<ContentBlock>) -> ChatMessage {
    message(
        ChatRole::Tool,
        MessageSource::Tool {
            call_id: ToolCallId::new(id).unwrap(),
        },
        content,
    )
}

fn tool_result(id: &str) -> ChatMessage {
    result(id, vec![text("result")])
}

fn route() -> SerializeRoute {
    SerializeRoute::new(MODEL, 8192)
}

fn capable() -> SerializeRoute {
    SerializeRoute::new(MODEL, 8192).with_system_prompt_update_in_history(true)
}

fn request() -> SerializeRequest {
    SerializeRequest::new(route())
}

/// Serializes with no image versions available.
fn body_with(request: &SerializeRequest, history: &[ChatMessage]) -> Result<Value, SerializeError> {
    let none: ImageLookup<'_> = &|_| None;
    serialize(request, history, none, &mut SilentReplay)
}

fn body(history: &[ChatMessage]) -> Value {
    body_with(&request(), history).unwrap()
}

fn native_body(history: &[ChatMessage]) -> Value {
    body_with(&SerializeRequest::new(capable()), history).unwrap()
}

fn roles(body: &Value) -> Vec<&str> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect()
}

fn content_at(body: &Value, index: usize) -> Value {
    body["messages"][index]["content"].clone()
}

fn types_at(body: &Value, index: usize) -> Vec<String> {
    content_at(body, index)
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap().to_owned())
        .collect()
}

fn tool(name: &str, deferred: bool) -> SerializeTool {
    let schema = ToolSchema {
        name: name.to_owned(),
        description: format!("{name} tool"),
        parameters: json!({ "type": "object" }),
    };
    if deferred {
        SerializeTool::deferred(schema)
    } else {
        SerializeTool::new(schema)
    }
}

/// A notice that records every diagnostic it is given.
#[derive(Default)]
struct Recording {
    reasons: Vec<String>,
}

impl ReplayNotice for Recording {
    fn degrade(&mut self, reason: &str) {
        self.reasons.push(reason.to_owned());
    }
}

// =============================================================================================
// Rules 1–4: assistant content
// =============================================================================================

/// Upstream: "omits saved empty assistant text without changing tool ids or thinking signatures".
///
/// The content is `['', reasoning, call, '']` with a four-entry envelope. Both empty texts are
/// dropped; the reasoning keeps the signature recorded at index 1, which only works if the reader
/// indexes durable positions rather than emitted ones.
#[test]
fn drops_empty_assistant_text_without_shifting_think_signatures() {
    // Upstream's fixture uses `toolu_example|toolu_example`; `|` is outside this crate's
    // `ToolCallId` alphabet, which is the one place the port cannot transcribe the literal.
    let id = "toolu_example";
    let replay = replay_state(
        MODEL,
        &[
            DeepSeekReplayBlock::text(),
            DeepSeekReplayBlock::reasoning("signed"),
            DeepSeekReplayBlock::tool_call(),
            DeepSeekReplayBlock::text(),
        ],
    );
    let history = vec![
        user("hello"),
        assistant(vec![text("")]),
        assistant_with_replay(
            vec![text(""), reasoning("reason"), call(id), text("")],
            Some(replay),
        ),
        tool_result(id),
    ];
    let request_body = body(&history);
    assert_eq!(
        request_body["messages"],
        json!([
            { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "reason", "signature": "signed" },
                { "type": "tool_use", "id": id, "name": "read", "input": { "path": "a" } },
            ] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": id, "content": [{ "type": "text", "text": "result" }] },
            ] },
        ])
    );
    // A single space is not empty, so it survives.
    assert_eq!(
        body(&[assistant(vec![text(" ")])])["messages"][0]["content"],
        json!([{ "type": "text", "text": " " }])
    );
}

/// Upstream: "preserves own signed thinking, omits absent signatures".
#[test]
fn keeps_signed_thinking_and_omits_an_absent_signature() {
    let replay = replay_state(
        MODEL,
        &[
            DeepSeekReplayBlock::reasoning("signed"),
            DeepSeekReplayBlock::text(),
        ],
    );
    let signed = assistant_with_replay(vec![reasoning(""), text("answer")], Some(replay));
    assert_eq!(
        content_at(&body(&[user("q"), signed, user("again")]), 1),
        json!([
            { "type": "thinking", "thinking": "", "signature": "signed" },
            { "type": "text", "text": "answer" },
        ])
    );
    // No envelope at all: the thinking block appears without a signature field, not with `null`.
    let unsigned = body(&[assistant(vec![reasoning("foreign thought")])]);
    assert_eq!(
        content_at(&unsigned, 0),
        json!([{ "type": "thinking", "thinking": "foreign thought" }])
    );
}

/// Rule 2's cross-model rule: a signature recorded by another model is not replayed, and that
/// decline is silent rather than a diagnostic.
#[test]
fn declines_signatures_recorded_by_another_model() {
    // The envelope was written by `MODEL`, and the request is for a different one.
    let replay = replay_state(MODEL, &[DeepSeekReplayBlock::reasoning("not-portable")]);
    let history = vec![assistant_with_replay(vec![reasoning("t")], Some(replay))];
    let mut notice = Recording::default();
    let none: ImageLookup<'_> = &|_| None;
    let request = SerializeRequest::new(SerializeRoute::new("another-model", 8192));
    let body = serialize(&request, &history, none, &mut notice).unwrap();
    assert_eq!(
        content_at(&body, 0),
        json!([{ "type": "thinking", "thinking": "t" }])
    );
    assert!(notice.reasons.is_empty(), "cross-model decline is silent");
}

/// Rule 4: an assistant image has no representation, because a model cannot have produced one.
#[test]
fn refuses_an_assistant_image_block() {
    let error = body_with(&request(), &[assistant(vec![image("sha256:abc")])]).unwrap_err();
    assert_eq!(
        error,
        SerializeError::UnsupportedContent {
            detail: "assistant content image".to_owned()
        }
    );
    assert_eq!(
        error.to_string(),
        "DeepSeek Messages cannot represent assistant content image"
    );
}

/// Rule 3: `toolInput`'s three `{}` paths, plus the object that survives.
#[test]
fn parses_tool_arguments_into_an_object_and_falls_back_to_empty_input() {
    assert_eq!(tool_input(r#"{"a":1}"#), json!({ "a": 1 }));
    assert_eq!(tool_input("{"), json!({}));
    assert_eq!(tool_input("[1,2]"), json!({}));
    assert_eq!(tool_input("null"), json!({}));
    assert_eq!(tool_input("\"text\""), json!({}));

    let malformed = message(
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "deepseek-official".to_owned(),
            model: MODEL.to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::ToolCall {
            id: ToolCallId::new("a").unwrap(),
            name: "read".to_owned(),
            arguments: "{".to_owned(),
        }],
    );
    let body = body(&[malformed, tool_result("a")]);
    assert_eq!(
        content_at(&body, 0),
        json!([{ "type": "tool_use", "id": "a", "name": "read", "input": {} }])
    );
}

// =============================================================================================
// Rules 5–8: user and tool-result content
// =============================================================================================

/// Upstream: "omits assistant-only blocks from user input while preserving text and durable
/// content".
#[test]
fn drops_assistant_only_blocks_from_user_input() {
    let relayed = user_blocks(vec![
        text("Background subagent finished.\n"),
        reasoning("child reasoning"),
        call("child-call"),
        text("  child answer  "),
    ]);
    assert_eq!(
        body(&[relayed])["messages"],
        json!([{ "role": "user", "content": [
            { "type": "text", "text": "Background subagent finished.\n" },
            { "type": "text", "text": "  child answer  " },
        ] }])
    );
}

/// Upstream: "omits assistant-only blocks inside tool results while retaining calls, errors and
/// empty results".
#[test]
fn drops_assistant_only_blocks_inside_tool_results() {
    let history = vec![
        assistant(vec![reasoning("tool reasoning"), call("a"), call("b")]),
        result(
            "a",
            vec![
                reasoning("tool reasoning"),
                call("nested-call"),
                text("  result\n"),
            ],
        ),
        result("b", vec![reasoning("x"), call("another-nested-call")]),
    ];
    let body = body(&history);
    assert_eq!(
        content_at(&body, 0),
        json!([
            { "type": "thinking", "thinking": "tool reasoning" },
            { "type": "tool_use", "id": "a", "name": "read", "input": { "path": "a" } },
            { "type": "tool_use", "id": "b", "name": "read", "input": { "path": "a" } },
        ])
    );
    // Rule 14: no `is_error` key at all, because the durable source has no error flag. The second
    // result keeps its empty content rather than inventing model-visible output.
    assert_eq!(
        content_at(&body, 1),
        json!([
            { "type": "tool_result", "tool_use_id": "a", "content": [{ "type": "text", "text": "  result\n" }] },
            { "type": "tool_result", "tool_use_id": "b", "content": [] },
        ])
    );
}

/// Rule 7 and 8: handle text, then the source — inline base64 or a Files reference.
#[test]
fn emits_image_handle_text_then_the_source() {
    let handle = image_handle_text("sha256:e", None, 1, 1, "image/png", None);
    let version = SerializeImage::inline("sha256:e", &handle, "image/png", vec![1, 2, 3]);
    let lookup = |id: &str| (id == "sha256:e").then(|| version.clone());
    let images: ImageLookup<'_> = &lookup;
    let value = serialize(
        &request(),
        &[user_blocks(vec![image("sha256:e")])],
        images,
        &mut SilentReplay,
    )
    .unwrap();
    assert_eq!(
        content_at(&value, 0),
        json!([
            { "type": "text", "text": "Image sha256:e; request preview 1x1px. It may be resized or re-encoded; source dimensions, format, and byte size may differ." },
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AQID" } },
        ])
    );

    // Same image, resolved through Files: the source switches, the handle text does not.
    let filed = version.clone().with_file_id("file-1");
    let lookup = |id: &str| (id == "sha256:e").then(|| filed.clone());
    let images: ImageLookup<'_> = &lookup;
    let value = serialize(
        &SerializeRequest::new(route()).with_require_file_ids(true),
        &[user_blocks(vec![image("sha256:e")])],
        images,
        &mut SilentReplay,
    )
    .unwrap();
    assert_eq!(
        content_at(&value, 0),
        json!([
            { "type": "text", "text": handle },
            { "type": "image", "source": { "type": "file", "file_id": "file-1" } },
        ])
    );
}

/// Rule 7 and 8's two refusals, which upstream raises as `INVALID_REQUEST`.
#[test]
fn refuses_an_image_without_a_version_or_without_a_required_file_id() {
    assert_eq!(
        body_with(&request(), &[user_blocks(vec![image("sha256:missing")])]).unwrap_err(),
        SerializeError::MissingImage {
            attachment: "sha256:missing".to_owned()
        }
    );
    let version = SerializeImage::inline("sha256:e", "handle", "image/png", vec![1]);
    let lookup = |_: &str| Some(version.clone());
    let images: ImageLookup<'_> = &lookup;
    let error = serialize(
        &SerializeRequest::new(route()).with_require_file_ids(true),
        &[user_blocks(vec![image("sha256:e")])],
        images,
        &mut SilentReplay,
    )
    .unwrap_err();
    assert_eq!(
        error,
        SerializeError::MissingFileId {
            attachment: "sha256:e".to_owned()
        }
    );
}

/// The handle text's two shapes: unnamed, and named with a readable normalized copy.
#[test]
fn renders_both_handle_text_forms() {
    assert_eq!(
        image_handle_text("sha256:a", None, 1708, 961, "image/png", None),
        "Image sha256:a; request preview 1708x961px. It may be resized or re-encoded; source \
         dimensions, format, and byte size may differ."
    );
    assert_eq!(
        image_handle_text(
            "sha256:a",
            Some("shot.png"),
            4,
            2,
            "image/jpeg",
            Some("/workspace/a.jpg")
        ),
        "Image \"shot.png\" (sha256:a); request preview 4x2px. Normalized copy (read-only; may be \
         resized or re-encoded): \"/workspace/a.jpg\" (4x2px, image/jpeg). Source dimensions, \
         format, and byte size may differ. Copy to a writable path ending in .jpg before editing."
    );
}

// =============================================================================================
// Rules 9–13: developer and system turns
// =============================================================================================

/// Upstream: "places developer tool changes as a system update after the preceding user turn".
///
/// The tool-change half needs [`developer_update_from_blocks`] because this crate's durable
/// `ContentBlock` cannot express a `tool-addition`; the placement half runs through `serialize`.
#[test]
fn places_a_developer_update_after_the_preceding_user_turn() {
    assert_eq!(
        developer_update_from_blocks(&["search"], &["fetch"], &["", "search replaces fetch"]),
        vec![
            json!({ "type": "tool_addition", "tool": { "type": "tool_reference", "name": "search" } }),
            json!({ "type": "tool_removal", "tool": { "type": "tool_reference", "name": "fetch" } }),
            json!({ "type": "text", "text": "search replaces fetch" }),
        ]
    );

    let changes = developer(vec![text("search replaces fetch")]);
    let value = native_body(&[user("hello"), changes, assistant(vec![text("ok")])]);
    assert_eq!(
        value["messages"],
        json!([
            { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
            { "role": "system", "content": [{ "type": "text", "text": "search replaces fetch" }] },
            { "role": "assistant", "content": [{ "type": "text", "text": "ok" }] },
        ])
    );
    // An all-empty developer message emits nothing at all.
    assert_eq!(
        native_body(&[user("hello"), developer(vec![text("")])])["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // A developer message before any user turn is placed after it, not dropped.
    assert_eq!(
        roles(&native_body(&[developer(vec![text("u")]), user("hello")])),
        vec!["user", "system"]
    );
    // A developer message with no user turn to follow is refused.
    assert_eq!(
        body_with(
            &SerializeRequest::new(capable()),
            &[developer(vec![text("u")])]
        )
        .unwrap_err(),
        SerializeError::UnsupportedContent {
            detail: "system update without a preceding user or tool-result turn".to_owned()
        }
    );
    // Rule 9's other half: a developer message may only carry text in this crate's model.
    assert_eq!(
        body_with(
            &SerializeRequest::new(capable()),
            &[developer(vec![image("a")])]
        )
        .unwrap_err(),
        SerializeError::UnsupportedContent {
            detail: "developer content image".to_owned()
        }
    );
}

/// Upstream: "keeps the original top-level prompt and cached prefix while appending native system
/// updates".
#[test]
fn keeps_the_cached_prefix_when_appending_in_history_updates() {
    let head = system("original");
    let first = [head.clone(), user("first")];
    let second = {
        let mut history = first.to_vec();
        history.push(assistant(vec![text("one")]));
        history.push(system("updated"));
        history.push(user("second"));
        history
    };
    let before = native_body(&first);
    let after = native_body(&second);
    assert_eq!(after["system"], before["system"]);
    assert_eq!(
        after["messages"].as_array().unwrap()[..before["messages"].as_array().unwrap().len()],
        before["messages"].as_array().unwrap()[..]
    );
    let tail =
        &after["messages"].as_array().unwrap()[after["messages"].as_array().unwrap().len() - 2..];
    assert_eq!(
        Value::from(tail.iter().map(|m| m["role"].clone()).collect::<Vec<_>>()),
        json!(["user", "system"])
    );
    assert_eq!(roles(&after), vec!["user", "assistant", "user", "system"]);
}

/// Upstream: "places system updates after all parallel tool results and before the next assistant".
#[test]
fn places_system_updates_after_every_parallel_tool_result() {
    let history = vec![
        system("original"),
        user("hello"),
        assistant(vec![call("a"), call("b")]),
        system("first update"),
        tool_result("a"),
        system("second update"),
        tool_result("b"),
        user("more input"),
        assistant(vec![text("done")]),
    ];
    let value = native_body(&history);
    assert_eq!(
        roles(&value),
        vec!["user", "assistant", "user", "system", "system", "assistant"]
    );
    assert_eq!(
        types_at(&value, 2),
        vec!["tool_result", "tool_result", "text"]
    );
    assert_eq!(
        content_at(&value, 3),
        json!([{ "type": "text", "text": "first update" }])
    );
    assert_eq!(
        content_at(&value, 4),
        json!([{ "type": "text", "text": "second update" }])
    );
    // The leading snapshot became the prompt, not a turn.
    assert_eq!(value["system"], json!("original"));
}

/// Upstream: "accepts native trailing updates without a top-level prompt and rejects
/// unrepresentable positions".
#[test]
fn accepts_a_trailing_update_and_refuses_impossible_positions() {
    let value = native_body(&[user("hello"), system("update")]);
    assert_eq!(roles(&value), vec!["user", "system"]);
    assert_eq!(value.get("system"), None);

    assert_eq!(
        body_with(
            &SerializeRequest::new(capable()),
            &[
                user("hello"),
                assistant(vec![text("done")]),
                system("update")
            ]
        )
        .unwrap_err()
        .to_string(),
        "DeepSeek Messages cannot represent system update without a preceding user or \
         tool-result turn"
    );
    assert_eq!(
        body_with(
            &SerializeRequest::new(capable()),
            &[user("hello"), system("")]
        )
        .unwrap_err(),
        SerializeError::UnsupportedContent {
            detail: "empty in-history system update".to_owned()
        }
    );
    // This is upstream's "immediate results" row: the trailing update has a valid slot, but the
    // second call is never answered, so pairing fails before the position question is asked.
    assert_eq!(
        body_with(
            &SerializeRequest::new(capable()),
            &[
                user("hello"),
                assistant(vec![call("a"), call("b")]),
                system("update"),
                tool_result("a"),
            ]
        )
        .unwrap_err(),
        SerializeError::PendingToolCalls {
            ids: vec!["b".to_owned()]
        }
    );
}

/// Upstream: "does not flush a system update at an omitted empty assistant message".
#[test]
fn does_not_flush_a_system_update_at_an_omitted_empty_assistant() {
    let value = native_body(&[
        user("hello"),
        system("updated"),
        assistant(vec![text("")]),
        user("hello again"),
        assistant(vec![text("answer")]),
    ]);
    assert_eq!(
        value["messages"],
        json!([
            { "role": "user", "content": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "hello again" },
            ] },
            { "role": "system", "content": [{ "type": "text", "text": "updated" }] },
            { "role": "assistant", "content": [{ "type": "text", "text": "answer" }] },
        ])
    );
}

/// Upstream: "rejects native system updates when their user input is omitted", plus the retained
/// variants that must still work.
#[test]
fn keeps_updates_beside_retained_tool_results_and_refuses_them_beside_omitted_input() {
    let update = system("updated");
    for omitted in [
        vec![],
        vec![reasoning("child reasoning")],
        vec![call("child-call")],
    ] {
        let history = vec![
            user("hello"),
            assistant(vec![text("answer")]),
            update.clone(),
            user_blocks(omitted),
        ];
        assert_eq!(
            body_with(&SerializeRequest::new(capable()), &history).unwrap_err(),
            SerializeError::UnsupportedContent {
                detail: "system update without a preceding user or tool-result turn".to_owned()
            }
        );
    }

    // A tool result survives even with its assistant-only blocks omitted, so the update has a slot.
    let history = vec![
        user("hello"),
        assistant(vec![call("a")]),
        update.clone(),
        result("a", vec![reasoning("child reasoning")]),
        assistant(vec![text("done")]),
    ];
    let value = native_body(&history);
    assert_eq!(
        roles(&value),
        vec!["user", "assistant", "user", "system", "assistant"]
    );
    assert_eq!(
        content_at(&value, 2),
        json!([{ "type": "tool_result", "tool_use_id": "a", "content": [] }])
    );
    assert_eq!(
        content_at(&value, 3),
        json!([{ "type": "text", "text": "updated" }])
    );
}

/// Upstream: "collects leading system text", "uses the latest complete system snapshot", "replaces
/// adjacent system snapshots", "clears earlier prompt snapshots with empty content", and "rejects
/// non-text system content even when a later snapshot supersedes it".
#[test]
fn folds_system_snapshots_into_one_prompt() {
    let with_prompt = body_with(
        &SerializeRequest::new(route())
            .with_system("top")
            .with_max_tokens(123)
            .with_stop_sequences(vec!["END".to_owned()])
            .with_tools(vec![tool("read", false)]),
        &[system("instructions"), user("hello")],
    )
    .unwrap();
    assert_eq!(with_prompt["system"], json!("top\n\ninstructions"));
    assert_eq!(with_prompt["max_tokens"], json!(123));
    assert_eq!(with_prompt["stop_sequences"], json!(["END"]));
    assert_eq!(
        with_prompt["tools"],
        json!([{
            "name": "read",
            "description": "read tool",
            "input_schema": { "type": "object" },
        }])
    );

    // A history-only prompt still lands in `system`.
    assert_eq!(
        body(&[user("hello"), system("instructions")])["system"],
        json!("instructions")
    );

    // Later snapshots replace the prompt and leave the turns alone.
    let conversation = vec![
        user("hello"),
        assistant(vec![call("a")]),
        tool_result("a"),
        assistant(vec![text("done")]),
        user("continue"),
    ];
    let mut history = vec![system("obsolete")];
    history.extend_from_slice(&conversation[..2]);
    history.push(system("intermediate"));
    history.extend_from_slice(&conversation[2..4]);
    history.push(system("current"));
    history.push(conversation[4].clone());
    assert_eq!(body(&history), {
        let mut value = body(&conversation);
        value["system"] = json!("current");
        value
    });
    assert_eq!(
        body_with(
            &SerializeRequest::new(route()).with_system("one-shot prefix"),
            &history
        )
        .unwrap()["system"],
        json!("one-shot prefix\n\ncurrent")
    );

    // Adjacent snapshots: the last wins, and its own blocks join with no separator.
    let latest = system_blocks(vec![text("part one"), text(" and part two")]);
    assert_eq!(
        body(&[system("old"), latest, user("hello")])["system"],
        json!("part one and part two")
    );

    // An empty snapshot clears the prompt rather than leaving the earlier text in force.
    for cleared in [system_blocks(vec![]), system_blocks(vec![text("")])] {
        let history = vec![system("old"), user("hello"), cleared];
        assert_eq!(body(&history).get("system"), None);
        assert_eq!(
            body_with(
                &SerializeRequest::new(route()).with_system("one-shot prefix"),
                &history
            )
            .unwrap()["system"],
            json!("one-shot prefix")
        );
    }
    assert_eq!(
        body_with(
            &SerializeRequest::new(route()).with_system(""),
            &[system("old"), user("hello"), system("")]
        )
        .unwrap()
        .get("system"),
        None
    );

    // A non-text snapshot is refused even when a later one would have superseded it.
    assert_eq!(
        body_with(
            &SerializeRequest::new(route()),
            &[
                system_blocks(vec![reasoning("bad")]),
                user("hello"),
                system("current")
            ]
        )
        .unwrap_err(),
        SerializeError::UnsupportedContent {
            detail: "non-text system message".to_owned()
        }
    );
}

// =============================================================================================
// Rules 15–21: turn folding and tool pairing
// =============================================================================================

/// Upstream: "groups parallel results before ordinary text and keeps tool failure content".
///
/// The failure half cannot be transcribed: this crate's durable tool source has no `is_error`, so
/// the assertion is that the key is *absent* rather than `false`.
#[test]
fn groups_parallel_results_before_ordinary_text() {
    let history = vec![
        user("hello"),
        assistant(vec![call("a"), call("b")]),
        user("follow-up"),
        tool_result("a"),
        tool_result("b"),
    ];
    let value = body(&history);
    assert_eq!(
        value["messages"],
        json!([
            { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "a", "name": "read", "input": { "path": "a" } },
                { "type": "tool_use", "id": "b", "name": "read", "input": { "path": "a" } },
            ] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "a", "content": [{ "type": "text", "text": "result" }] },
                { "type": "tool_result", "tool_use_id": "b", "content": [{ "type": "text", "text": "result" }] },
                { "type": "text", "text": "follow-up" },
            ] },
        ])
    );
}

/// Upstream: "preserves empty results without inventing model-visible output" and "omits empty user
/// input after conversion".
#[test]
fn keeps_empty_tool_results_and_drops_empty_user_turns() {
    let value = body(&[
        user("hello"),
        assistant(vec![call("a")]),
        result("a", vec![]),
    ]);
    assert_eq!(
        content_at(&value, 2),
        json!([{ "type": "tool_result", "tool_use_id": "a", "content": [] }])
    );

    let empty = || user_blocks(vec![]);
    for content in [
        vec![],
        vec![reasoning("child reasoning")],
        vec![call("child-call")],
    ] {
        let value = body(&[
            user_blocks(content),
            user("hello"),
            assistant(vec![text("answer")]),
            empty(),
        ]);
        assert_eq!(
            value["messages"],
            json!([
                { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "answer" }] },
            ])
        );
    }
}

/// Rule 18: a repeated call id inside one assistant turn.
#[test]
fn refuses_a_duplicate_tool_call_id_in_one_assistant_turn() {
    let error = body_with(
        &request(),
        &[assistant(vec![call("a"), call("a")]), tool_result("a")],
    )
    .unwrap_err();
    assert_eq!(
        error,
        SerializeError::DuplicateToolCallId { id: "a".to_owned() }
    );
    assert_eq!(
        error.to_string(),
        "DeepSeek Messages duplicate tool call id `a`"
    );
}

/// Rule 19: a result answering nothing, a call answered late, and a call answered never.
#[test]
fn refuses_unpaired_tool_calls_and_results() {
    assert_eq!(
        body_with(&request(), &[user("hello"), tool_result("ghost")]).unwrap_err(),
        SerializeError::UnmatchedToolResult {
            id: "ghost".to_owned()
        }
    );
    // Two results for one call: the second has nothing left to answer.
    assert_eq!(
        body_with(
            &request(),
            &[
                assistant(vec![call("a")]),
                tool_result("a"),
                tool_result("a")
            ]
        )
        .unwrap_err(),
        SerializeError::UnmatchedToolResult { id: "a".to_owned() }
    );
    // A call answered only after the model has already spoken again. The next assistant turn is
    // what closes the user turn, so this is the shape the "immediate results" rule is about; note
    // that `tool_result` and `user` text merge into one wire turn, so an interjection in between
    // does *not* create a boundary.
    assert_eq!(
        body_with(
            &request(),
            &[
                assistant(vec![call("a"), call("b")]),
                tool_result("a"),
                assistant(vec![text("carrying on")]),
            ]
        )
        .unwrap_err(),
        SerializeError::PendingToolCalls {
            ids: vec!["b".to_owned()]
        }
    );
    // The interjection really does merge, so all three results land in one turn and pair cleanly.
    assert_eq!(
        roles(
            &body_with(
                &request(),
                &[
                    assistant(vec![call("a"), call("b")]),
                    tool_result("a"),
                    user("interjection"),
                    tool_result("b"),
                ]
            )
            .unwrap()
        ),
        vec!["assistant", "user"]
    );
    // A history that simply stops.
    assert_eq!(
        body_with(&request(), &[assistant(vec![call("a")])]).unwrap_err(),
        SerializeError::UnresolvedTrailingToolCalls {
            ids: vec!["a".to_owned()]
        }
    );
}

// =============================================================================================
// Rules 22–26: the envelope
// =============================================================================================

/// Upstream: "maps reasoning effort", "disables thinking for the off effort", "passes temperature
/// with thinking" and "refuses unsupported effort".
#[test]
fn maps_effort_onto_thinking_and_output_config() {
    for (effort, wire) in [
        (ReasoningEffort::Off, "off"),
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::Max, "max"),
    ] {
        let value = body_with(
            &SerializeRequest::new(route()).with_reasoning_effort(effort),
            &[user("hello")],
        )
        .unwrap();
        assert_eq!(
            value["thinking"],
            json!({ "type": if effort.is_off() { "disabled" } else { "enabled" } })
        );
        match effort {
            ReasoningEffort::Off => assert_eq!(value.get("output_config"), None),
            _ => assert_eq!(value["output_config"], json!({ "effort": wire })),
        }
        assert_eq!(ReasoningEffort::parse(wire), Some(effort));
        assert_eq!(effort.as_str(), wire);
    }
    // Unsupported spellings cannot reach `serialize` at all.
    assert_eq!(ReasoningEffort::parse("medium"), None);

    // Temperature survives thinking being on, including an explicit `0`.
    let value = body_with(
        &SerializeRequest::new(route()).with_temperature(0.0),
        &[user("hello")],
    )
    .unwrap();
    assert_eq!(value["thinking"], json!({ "type": "enabled" }));
    assert_eq!(value["temperature"], json!(0.0));

    // A route that disables thinking forces `off` when the request is silent …
    let disabled = SerializeRoute::new(MODEL, 8192).with_thinking_enabled(false);
    assert_eq!(
        body_with(&SerializeRequest::new(disabled), &[user("hello")]).unwrap()["thinking"],
        json!({ "type": "disabled" })
    );
    // … and refuses an effort it cannot honour.
    let disabled = SerializeRoute::new(MODEL, 8192).with_thinking_enabled(false);
    assert_eq!(
        body_with(
            &SerializeRequest::new(disabled).with_reasoning_effort(ReasoningEffort::High),
            &[user("hello")]
        )
        .unwrap_err(),
        SerializeError::UnsupportedReasoningEffort {
            effort: "high".to_owned()
        }
    );

    // The route default fills in, and `high` is the fallback when nothing names an effort.
    let defaulted = SerializeRoute::new(MODEL, 8192).with_default_effort(ReasoningEffort::Low);
    assert_eq!(defaulted.resolve_effort(None), ReasoningEffort::Low);
    assert_eq!(route().resolve_effort(None), ReasoningEffort::High);
    assert_eq!(
        route().resolve_effort(Some(ReasoningEffort::Max)),
        ReasoningEffort::Max
    );
}

/// Rule 24: request, then model entry, then route default.
#[test]
fn resolves_max_tokens_through_three_levels() {
    let capped = route().with_model_max_tokens(321);
    assert_eq!(
        body_with(&SerializeRequest::new(capped.clone()), &[user("hello")]).unwrap()["max_tokens"],
        json!(321)
    );
    assert_eq!(
        body_with(
            &SerializeRequest::new(capped).with_max_tokens(7),
            &[user("hello")]
        )
        .unwrap()["max_tokens"],
        json!(7)
    );
    assert_eq!(
        body_with(&SerializeRequest::new(route()), &[user("hello")]).unwrap()["max_tokens"],
        json!(8192)
    );
}

/// Rule 26: deferred tools, an all-deferred list, and the absent/empty distinction.
#[test]
fn declares_deferred_tools_and_preserves_absent_versus_empty_lists() {
    let value = body_with(
        &SerializeRequest::new(route())
            .with_tools(vec![tool("search", true), tool("fetch", false)]),
        &[user("hello")],
    )
    .unwrap();
    assert_eq!(
        value["tools"],
        json!([
            { "name": "search", "description": "search tool", "input_schema": { "type": "object" }, "defer_loading": true },
            { "name": "fetch", "description": "fetch tool", "input_schema": { "type": "object" } },
        ])
    );

    let value = body_with(
        &SerializeRequest::new(route()).with_tools(vec![tool("search", true)]),
        &[user("hello")],
    )
    .unwrap();
    assert_eq!(value["tools"].as_array().unwrap().len(), 1);

    // Absent stays absent; empty stays present and empty.
    assert_eq!(
        body_with(&SerializeRequest::new(route()), &[user("hello")])
            .unwrap()
            .get("tools"),
        None
    );
    assert_eq!(
        body_with(
            &SerializeRequest::new(route()).with_tools(vec![]),
            &[user("hello")]
        )
        .unwrap()["tools"],
        json!([])
    );
}

/// The envelope's constant fields, and `stream: true`.
#[test]
fn emits_the_envelope_this_dialect_requires() {
    let value = body(&[user("hello")]);
    assert_eq!(value["model"], json!(MODEL));
    assert_eq!(value["stream"], json!(true));
    assert_eq!(value["thinking"], json!({ "type": "enabled" }));
    assert_eq!(value["output_config"], json!({ "effort": "high" }));
    assert_eq!(value.get("temperature"), None);
    assert_eq!(value.get("stop_sequences"), None);
}

// =============================================================================================
// Rules 17 and 23: the log is not rewritten, and degraded replay is reported
// =============================================================================================

/// Rule 17: serializing never mutates the durable history, on success or on refusal.
#[test]
fn leaves_the_durable_history_byte_identical() {
    let history = vec![
        user("hello"),
        assistant(vec![reasoning("think"), call("a"), text("")]),
        user("follow-up"),
        tool_result("a"),
        user_blocks(vec![reasoning("relayed"), text("kept")]),
    ];
    let saved = serde_json::to_string(&history).unwrap();
    let _ = body(&history);
    assert_eq!(serde_json::to_string(&history).unwrap(), saved);

    let failing = vec![assistant(vec![call("a")])];
    let saved = serde_json::to_string(&failing).unwrap();
    assert!(body_with(&request(), &failing).is_err());
    assert_eq!(serde_json::to_string(&failing).unwrap(), saved);
}

/// Rule 23: an unusable envelope is reported once and the durable content is still serialized.
///
/// Upstream's ten unusable states are covered by `llm_stream_gap`'s reader tests; this asserts the
/// serializer's half — the diagnostic reaches the notice exactly once and no signature is emitted.
#[test]
fn reports_a_degraded_envelope_and_keeps_the_content() {
    let damaged = assistant_with_replay(
        vec![reasoning("think")],
        Some(json!({ "response": {}, "blocks": [] })),
    );
    let mut notice = Recording::default();
    let none: ImageLookup<'_> = &|_| None;
    let value = serialize(&request(), &[damaged], none, &mut notice).unwrap();
    assert_eq!(notice.reasons.len(), 1);
    assert_eq!(
        content_at(&value, 0),
        json!([{ "type": "thinking", "thinking": "think" }])
    );

    // A wrong version degrades; the same invalid arguments still become `{}`.
    let replay = json!({
        "response": { "kind": "deepseek-messages", "version": 2, "model": MODEL },
        "blocks": [{ "type": "tool-call" }],
    });
    let stale = assistant_with_replay(
        vec![ContentBlock::ToolCall {
            id: ToolCallId::new("a").unwrap(),
            name: "read".to_owned(),
            arguments: "{".to_owned(),
        }],
        Some(replay),
    );
    let mut notice = Recording::default();
    let none: ImageLookup<'_> = &|_| None;
    let value = serialize(&request(), &[stale, tool_result("a")], none, &mut notice).unwrap();
    assert_eq!(notice.reasons.len(), 1);
    assert_eq!(
        content_at(&value, 0),
        json!([{ "type": "tool_use", "id": "a", "name": "read", "input": {} }])
    );
}

/// A foreign assistant message (no `Model` source) never consults replay at all.
#[test]
fn ignores_replay_for_a_message_whose_source_is_not_a_model() {
    let foreign = message(
        ChatRole::Assistant,
        MessageSource::User { form: None },
        vec![reasoning("t")],
    );
    let mut notice = Recording::default();
    let none: ImageLookup<'_> = &|_| None;
    let value = serialize(&request(), &[foreign], none, &mut notice).unwrap();
    assert!(notice.reasons.is_empty());
    assert_eq!(
        content_at(&value, 0),
        json!([{ "type": "thinking", "thinking": "t" }])
    );
}

// =============================================================================================
// The `LlmRequest` bridge
// =============================================================================================

/// [`SerializeRequest::from_llm`] walks the local request type, so a caller does not restate it.
#[test]
fn bridges_a_durable_llm_request() {
    let tools = vec![ToolSchema {
        name: "read".to_owned(),
        description: "Read a file".to_owned(),
        parameters: json!({ "type": "object" }),
    }];
    let mut request = LlmRequest::new(MODEL, vec![Message::new(Role::User, vec![text("hello")])])
        .with_system_prompt("policy")
        .with_tools(tools);
    request.max_tokens = Some(64);
    let value =
        deepseek_serialize_gap::serialize_llm_request(SerializeRoute::new(MODEL, 8192), &request)
            .unwrap();
    assert_eq!(value["system"], json!("policy"));
    assert_eq!(value["max_tokens"], json!(64));
    assert_eq!(
        value["tools"],
        json!([{
            "name": "read",
            "description": "Read a file",
            "input_schema": { "type": "object" },
        }])
    );
    assert_eq!(value["messages"], json!([]));
}

/// Rule 11's separator, and `serialize_llm_request`'s empty-history path.
#[test]
fn serializes_an_empty_history_without_messages() {
    let request = LlmRequest::new(MODEL, Vec::new());
    let value =
        deepseek_serialize_gap::serialize_llm_request(SerializeRoute::new(MODEL, 1), &request)
            .unwrap();
    assert_eq!(value["messages"], json!([]));
    assert_eq!(value["max_tokens"], json!(1));
}

/// `assistant_from` exists so a history can carry a message another model produced.
#[test]
fn serializes_a_history_produced_by_another_model() {
    let value = body(&[
        user("hello"),
        assistant_from("other-model", vec![text("hi"), reasoning("t")]),
    ]);
    assert_eq!(
        content_at(&value, 1),
        json!([
            { "type": "text", "text": "hi" },
            { "type": "thinking", "thinking": "t" },
        ])
    );
}
