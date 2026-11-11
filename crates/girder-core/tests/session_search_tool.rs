//! Tests for the `session_search` tool.
//!
//! The distinction the tool rests on is that "no match" and "I stopped looking" are different answers: a model told the
//! first stops, and one told the second asks a narrower question.

use girder_core::{SessionEvent, SessionStore, ToolResult, ToolRunContext, session_search_tool};

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-ssearch-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Writes one session holding the given messages.
fn seed(dir: &std::path::Path, id: &str, messages: &[&str]) {
    let store = SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
    store.touch().expect("created");
    for (index, text) in messages.iter().enumerate() {
        store
            .append(&SessionEvent::user_text(&format!("m{index}"), *text))
            .expect("appended");
    }
}

async fn run(dir: &std::path::Path, args: serde_json::Value) -> ToolResult {
    let tool = session_search_tool(dir.to_path_buf());
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a search is a result")
}

#[tokio::test]
async fn a_message_is_found_with_its_session_and_position() {
    let dir = scratch("find");
    seed(&dir, "s1", &["first", "the needle", "third"]);

    let text = text_of(&run(&dir, serde_json::json!({"text": "needle"})).await);

    assert!(text.contains("s1"), "{text}");
    assert!(text.contains("(2)"), "the sequence is one-based: {text}");
    assert!(text.contains("the needle"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_search_spans_every_session() {
    let dir = scratch("spans");
    seed(&dir, "a", &["needle here"]);
    seed(&dir, "b", &["needle too"]);

    let text = text_of(&run(&dir, serde_json::json!({"text": "needle"})).await);

    assert!(text.contains('a'), "{text}");
    assert!(text.contains('b'), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_session_restriction_narrows_the_scan() {
    let dir = scratch("restrict");
    seed(&dir, "a", &["needle"]);
    seed(&dir, "b", &["needle"]);

    let text = text_of(&run(&dir, serde_json::json!({"text": "needle", "session": "b"})).await);

    assert!(text.contains('b'), "{text}");
    assert!(!text.contains("a("), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_kind_restriction_excludes_other_events() {
    let dir = scratch("kind");
    let store = SessionStore::new(dir.join("s1.jsonl.zst"));
    store.touch().expect("created");
    store
        .append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    store
        .append(&SessionEvent::user_text("m1", "needle in a message"))
        .expect("appended");

    let text = text_of(
        &run(
            &dir,
            serde_json::json!({"text": "needle", "kind": "messages"}),
        )
        .await,
    );

    assert!(text.contains("user/message"), "{text}");
    assert!(!text.contains("turn/start"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn no_match_reports_the_scan_count() {
    // "no match in 0 sessions" and "no match in 40" are different answers.
    let dir = scratch("none");
    seed(&dir, "s1", &["nothing relevant"]);

    let text = text_of(&run(&dir, serde_json::json!({"text": "absent"})).await);

    assert!(text.contains("no matches"), "{text}");
    assert!(
        text.contains("1 session"),
        "the scan count is shown: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_truncated_search_says_so() {
    let dir = scratch("limit");
    seed(&dir, "s1", &["needle one", "needle two", "needle three"]);

    let text = text_of(&run(&dir, serde_json::json!({"text": "needle", "limit": 2})).await);

    assert!(text.contains("stopped after 2"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_query_lists_the_conversation() {
    // What a caller asking "show me this session" means, without needing a second tool for the same read.
    let dir = scratch("all");
    seed(&dir, "s1", &["hello", "world"]);

    let text = text_of(&run(&dir, serde_json::json!({"session": "s1"})).await);

    assert!(text.contains("hello"), "{text}");
    assert!(text.contains("world"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_long_message_is_truncated_on_a_character_boundary() {
    // A cut inside a multi-byte character would produce a replacement character, and the session would look corrupt.
    let dir = scratch("long");
    let long: String = "字".repeat(500);
    seed(&dir, "s1", &[&long]);

    let text = text_of(&run(&dir, serde_json::json!({"text": "字"})).await);

    assert!(text.contains('…'), "the snippet is marked: {text}");
    assert!(
        !text.contains('\u{fffd}'),
        "no replacement character: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_search_of_an_empty_directory_is_empty_not_an_error() {
    let dir = scratch("empty");

    let result = run(&dir, serde_json::json!({"text": "x"})).await;

    assert!(!result.is_error);
    let _ = std::fs::remove_dir_all(&dir);
}
