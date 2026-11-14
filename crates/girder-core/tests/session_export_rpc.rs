//! Tests for `session.export`, the portable-transcript method.
//!
//! The properties under test are the ones a client depends on:
//!
//! - **the Markdown form says who spoke and which turn** — a transcript a person cannot follow is
//!   not an export, so the headings and the message text both have to be there
//! - **the JSON form round-trips** — a program that reads the export back gets an array of events,
//!   which is what makes the format portable rather than a log of strings
//! - **the size budget is enforced on a character boundary** — a cut inside a multi-byte character
//!   reads as corrupt data rather than as a truncation, and the flag has to say it happened
//! - **an unknown id and an unknown format are errors** — an empty transcript or a silent fallback
//!   would answer a question nobody asked
//! - **an empty session exports nothing without failing** — a session that was created and never
//!   used is a normal state, not a defect
//!
//! Isolation: every test builds its own registry and its own catalog over a scratch directory named
//! from the process id and a counter, so tests cannot collide.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the method as written before the crate root wires it in. It spells its paths through
//! `girder_core::`, the alias the library declares for itself, so the same source builds here and
//! inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/session_export_rpc.rs"]
mod session_export_rpc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    ContentBlock, DirectoryCatalog, MessageId, RemoteCall, RemoteError, RemoteRegistry,
    SessionCatalog, SessionEvent, SessionStore, ToolCallId,
};
use serde_json::{Value, json};

use session_export_rpc::{MAX_EXPORT_BYTES, register_session_export_method};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-export-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory is created");
    dir
}

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn registry(catalog: &Arc<dyn SessionCatalog>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_session_export_method(&mut registry, catalog);
    registry
}

async fn export(root: &Path, args: Value) -> Result<Value, RemoteError> {
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(root));
    registry(&catalog)
        .dispatch("session", "export", call(args))
        .await
}

fn store(dir: &Path, id: &str) -> SessionStore {
    SessionStore::new(dir.join(format!("{id}.jsonl.zst")))
}

/// One turn holding a user message, a tool call and its result, and an assistant reply.
fn seed_conversation(dir: &Path, id: &str) {
    let store = store(dir, id);
    store
        .append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    store
        .append(&SessionEvent::user_text("m1", "How many files are here?"))
        .expect("appended");
    store
        .append(&SessionEvent::AssistantMessage {
            message: girder_core::ChatMessage::new(
                MessageId::new("m2").expect("valid"),
                girder_core::ChatRole::Assistant,
                girder_core::MessageSource::Model {
                    provider: "deepseek".to_owned(),
                    model: "deepseek-chat".to_owned(),
                    replay_state: None,
                },
                vec![ContentBlock::Text {
                    text: "Let me look.".to_owned(),
                }],
            ),
        })
        .expect("appended");
    store
        .append(&SessionEvent::ToolCall {
            id: ToolCallId::new("call-1").expect("valid"),
            name: "list_files".to_owned(),
            arguments: "{\"path\":\".\"}".to_owned(),
        })
        .expect("appended");
    store
        .append(&SessionEvent::ToolResult {
            id: ToolCallId::new("call-1").expect("valid"),
            content: vec![ContentBlock::Text {
                text: "a.txt\nb.txt".to_owned(),
            }],
            is_error: false,
        })
        .expect("appended");
    store
        .append(&SessionEvent::assistant_text(
            "m3",
            "There are two files.",
            "deepseek",
        ))
        .expect("appended");
    store
        .append(&SessionEvent::TurnEnd {
            turn: 1,
            reason: girder_core::TurnEndReason::Completed,
        })
        .expect("appended");
}

/// The `content` of a successful export response.
fn content(value: &Value) -> &str {
    value["content"].as_str().expect("content is a string")
}

// ---- Markdown ---------------------------------------------------------------------------------

#[tokio::test]
async fn markdown_carries_turn_headings_speakers_and_tool_names() {
    let dir = scratch("markdown");
    seed_conversation(&dir, "s1");

    let value = export(&dir, json!({"sessionId": "s1"}))
        .await
        .expect("a known session exports");
    assert_eq!(value["format"], json!("markdown"));
    assert_eq!(value["truncated"], json!(false));

    let text = content(&value);
    assert!(
        text.starts_with("# Session s1\n"),
        "the transcript is titled with its id: {text}"
    );
    assert!(
        text.contains("\n## Turn 1\n"),
        "the turn boundary becomes a heading: {text}"
    );
    assert!(
        text.contains("### User\nHow many files are here?"),
        "the prompt is attributed and quoted: {text}"
    );
    assert!(
        text.contains("### Assistant\nThere are two files."),
        "the reply is attributed: {text}"
    );
    // A tool result is headed with the tool's name, not its call id.
    assert!(
        text.contains("### Tool: list_files\na.txt\nb.txt"),
        "the result names the tool that produced it: {text}"
    );
    // Boundary bookkeeping stays out of the readable body.
    assert!(
        !text.contains("step/start") && !text.contains("turn/end"),
        "settings and step records are not rendered: {text}"
    );
}

#[tokio::test]
async fn markdown_defaults_to_markdown_when_format_is_absent_or_null() {
    let dir = scratch("default");
    seed_conversation(&dir, "s1");

    for args in [
        json!({"sessionId": "s1"}),
        json!({"sessionId": "s1", "format": null}),
    ] {
        let value = export(&dir, args).await.expect("exports");
        assert_eq!(value["format"], json!("markdown"));
    }
}

#[tokio::test]
async fn markdown_reports_a_call_that_never_produced_a_result() {
    let dir = scratch("dangling");
    let store = store(&dir, "s1");
    store
        .append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    store
        .append(&SessionEvent::ToolCall {
            id: ToolCallId::new("call-9").expect("valid"),
            name: "bash".to_owned(),
            arguments: "{\"command\":\"sleep 1000\"}".to_owned(),
        })
        .expect("appended");

    let value = export(&dir, json!({"sessionId": "s1"}))
        .await
        .expect("exports");
    let text = content(&value);
    assert!(
        text.contains("### Tool: bash") && text.contains("no result recorded"),
        "an unfinished call is still visible: {text}"
    );
}

// ---- JSON -------------------------------------------------------------------------------------

#[tokio::test]
async fn json_exports_an_array_that_parses_back_into_the_events() {
    let dir = scratch("json");
    seed_conversation(&dir, "s1");

    let value = export(&dir, json!({"sessionId": "s1", "format": "json"}))
        .await
        .expect("exports");
    assert_eq!(value["format"], json!("json"));
    assert_eq!(value["truncated"], json!(false));

    let parsed: Vec<SessionEvent> =
        serde_json::from_str(content(&value)).expect("the export is an array of events");
    assert_eq!(parsed.len(), 7, "every appended event is exported");
    assert_eq!(parsed[0], SessionEvent::TurnStart { turn: 1 });
    assert!(matches!(parsed[1], SessionEvent::UserMessage { .. }));
    assert!(matches!(parsed[2], SessionEvent::AssistantMessage { .. }));
    assert!(matches!(parsed[5], SessionEvent::AssistantMessage { .. }));

    // The value is the log's own serialization: a client may index it as an array of objects.
    let raw: Value = serde_json::from_str(content(&value)).expect("valid JSON");
    assert_eq!(raw[0]["type"], json!("turn/start"));
}

// ---- the size budget --------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_export_is_cut_on_a_character_boundary_and_says_so() {
    let dir = scratch("truncated");
    let store = store(&dir, "s1");
    store
        .append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    // Three-byte characters, so a cut at the byte limit almost always lands mid-sequence.
    let body = "语".repeat(MAX_EXPORT_BYTES / 3 + 1000);
    store
        .append(&SessionEvent::user_text("m1", body.clone()))
        .expect("appended");

    let value = export(&dir, json!({"sessionId": "s1"}))
        .await
        .expect("exports");
    assert_eq!(value["truncated"], json!(true), "the cut is reported");

    let text = content(&value);
    assert!(
        text.len() <= MAX_EXPORT_BYTES,
        "the budget is a hard limit: {} bytes",
        text.len()
    );
    // A cut that backed up to a boundary loses at most the three bytes of the character it landed
    // in, so the response is within a character of the limit rather than well under it.
    assert!(
        text.len() + 3 > MAX_EXPORT_BYTES,
        "the cut backs up by a character at most: {} bytes",
        text.len()
    );
    let retained = text
        .split_once("### User\n")
        .expect("the prompt heading survives the cut")
        .1;
    assert!(
        body.starts_with(retained),
        "the retained body is a prefix of the original"
    );
    // `String` is UTF-8 by construction, so the proof that the cut respected a boundary is that the
    // content is a prefix of the original *and* decodes: a mid-character cut cannot be a `String`.
    assert_eq!(
        value["bytes"],
        json!(text.len()),
        "`bytes` describes what was returned, not what was cut"
    );
}

#[tokio::test]
async fn an_export_under_the_budget_is_not_marked_truncated() {
    let dir = scratch("under");
    seed_conversation(&dir, "s1");

    let value = export(&dir, json!({"sessionId": "s1"}))
        .await
        .expect("exports");
    assert_eq!(value["truncated"], json!(false));
    assert_eq!(value["bytes"], json!(content(&value).len()));
}

// ---- refusals ---------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_session_is_an_error() {
    let dir = scratch("unknown");
    seed_conversation(&dir, "s1");

    let error = export(&dir, json!({"sessionId": "nope"}))
        .await
        .expect_err("an unknown session is not an empty export");
    assert_eq!(error.code(), "FAILED");
    let message = error.to_string();
    assert!(
        message.contains("nope"),
        "the message names the id that was missing: {message}"
    );
}

#[tokio::test]
async fn an_unknown_format_is_an_error() {
    let dir = scratch("format");
    seed_conversation(&dir, "s1");

    // `html` was the unknown format until it became one; a name that is genuinely not a format is the case now, and it
    // still has to be refused rather than defaulted.
    let error = export(&dir, json!({"sessionId": "s1", "format": "pdf"}))
        .await
        .expect_err("an unknown format is refused, not defaulted");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    let message = error.to_string();
    assert!(
        message.contains("pdf") && message.contains("markdown"),
        "the message names the offending value and the accepted ones: {message}"
    );
}

#[tokio::test]
async fn a_missing_session_id_is_an_error() {
    let dir = scratch("missing");
    let error = export(&dir, json!({}))
        .await
        .expect_err("`sessionId` is required");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
}

// ---- degenerate sessions ----------------------------------------------------------------------

#[tokio::test]
async fn an_empty_session_exports_a_title_and_no_body() {
    let dir = scratch("empty");
    store(&dir, "s1").touch().expect("created");

    let value = export(&dir, json!({"sessionId": "s1"}))
        .await
        .expect("an empty session is not an error");
    assert_eq!(value["truncated"], json!(false));
    assert_eq!(
        content(&value),
        "# Session s1\n",
        "only the title is rendered"
    );

    let json_form = export(&dir, json!({"sessionId": "s1", "format": "json"}))
        .await
        .expect("an empty session exports as JSON too");
    let parsed: Vec<Value> =
        serde_json::from_str(content(&json_form)).expect("an empty array parses");
    assert!(
        parsed.is_empty(),
        "nothing was recorded, so nothing is exported"
    );
}

// ---- access -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_read_only_policy_can_still_export() {
    let dir = scratch("readonly");
    seed_conversation(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let registry = registry(&catalog).with_policy(Arc::new(girder_core::ReadOnly));

    let value = registry
        .dispatch("session", "export", call(json!({"sessionId": "s1"})))
        .await
        .expect("reading a session is a read");
    assert_eq!(value["format"], json!("markdown"));
}

// ---- the session trait stays usable -----------------------------------------------------------

#[test]
fn the_method_name_is_the_one_upstream_declares() {
    assert_eq!(session_export_rpc::METHOD, "export");
    assert_eq!(session_export_rpc::NAMESPACE, "session");
    // The default format is the one a caller gets for free; pinning it here keeps a rename from
    // silently changing what an existing client receives.
    assert_eq!(session_export_rpc::DEFAULT_FORMAT, "markdown");
}
