//! Tests for the `session.search` Remote method.
//!
//! The properties worth pinning are the ones a client depends on: a hit says where it is, a bound is
//! reported rather than silently applied, and the text of a non-ASCII conversation survives being cut.

use girder_core::{RemoteCall, RemoteError, RemoteRegistry, SessionEvent, SessionStore};
use serde_json::{Value, json};

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-srpc-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn registry(root: &std::path::Path) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    girder_core::register_session_query_methods(&mut registry, root);
    registry
}

async fn search(root: &std::path::Path, args: Value) -> Value {
    registry(root)
        .dispatch("session", "search", call(args))
        .await
        .expect("a search is a result")
}

/// Writes one session holding the given user messages.
fn seed(dir: &std::path::Path, id: &str, messages: &[&str]) {
    let store = SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
    store.touch().expect("created");
    for (index, text) in messages.iter().enumerate() {
        store
            .append(&SessionEvent::user_text(&format!("m{index}"), *text))
            .expect("appended");
    }
}

fn hits(value: &Value) -> &Vec<Value> {
    value["hits"].as_array().expect("an array")
}

// ---- what a hit carries -----------------------------------------------------------------------

#[tokio::test]
async fn a_match_reports_its_session_position_kind_and_text() {
    let dir = scratch("hit");
    seed(&dir, "s1", &["first", "the needle", "third"]);

    let value = search(&dir, json!({"text": "needle"})).await;

    let hits = hits(&value);
    assert_eq!(hits.len(), 1, "{value}");
    assert_eq!(hits[0]["session"], "s1");
    // One-based, so a client can index the session log with it directly.
    assert_eq!(hits[0]["seq"], 2);
    assert_eq!(hits[0]["kind"], "user/message");
    assert_eq!(hits[0]["text"], "the needle");
    assert_eq!(value["sessionsScanned"], 1);
    assert_eq!(value["truncated"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_query_lists_every_event_of_every_session() {
    // "Show me what happened" is the same read with no filter, and it must not need a second method.
    let dir = scratch("all");
    seed(&dir, "a", &["one"]);
    seed(&dir, "b", &["two"]);

    let value = search(&dir, json!({})).await;

    assert_eq!(hits(&value).len(), 2, "{value}");
    assert_eq!(value["sessionsScanned"], 2);
    assert_eq!(value["truncated"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- no matches -------------------------------------------------------------------------------

#[tokio::test]
async fn no_match_is_an_empty_array_rather_than_an_error() {
    let dir = scratch("none");
    seed(&dir, "s1", &["nothing relevant"]);

    let value = search(&dir, json!({"text": "absent"})).await;

    assert_eq!(hits(&value).len(), 0, "{value}");
    // The scan count is what distinguishes "no such text" from "no sessions were read".
    assert_eq!(value["sessionsScanned"], 1);
    assert_eq!(value["truncated"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_root_is_no_matches_rather_than_an_error() {
    // A deployment with no sessions yet has no directory; that is an empty history, not a failure.
    let dir = scratch("missing");
    let root = dir.join("never-created");

    let value = search(&root, json!({"text": "anything"})).await;

    assert_eq!(hits(&value).len(), 0, "{value}");
    assert_eq!(value["sessionsScanned"], 0);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the bound --------------------------------------------------------------------------------

#[tokio::test]
async fn a_limit_bounds_the_hits_and_is_reported_as_truncation() {
    let dir = scratch("limit");
    seed(&dir, "s1", &["needle one", "needle two", "needle three"]);

    let value = search(&dir, json!({"text": "needle", "limit": 2})).await;

    assert_eq!(hits(&value).len(), 2, "{value}");
    // Recorded by the search, not inferred from the count: two hits out of three is not a complete
    // answer, and a client told otherwise would stop looking.
    assert_eq!(value["truncated"], true);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_default_limit_is_fifty() {
    let dir = scratch("default");
    let messages: Vec<String> = (0..60).map(|index| format!("needle {index}")).collect();
    let borrowed: Vec<&str> = messages.iter().map(String::as_str).collect();
    seed(&dir, "s1", &borrowed);

    let value = search(&dir, json!({"text": "needle"})).await;

    assert_eq!(hits(&value).len(), 50, "{value}");
    assert_eq!(value["truncated"], true);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_zero_limit_still_returns_one_hit() {
    // Zero hits would report `truncated: true` with nothing shown, which claims matches exist that
    // the caller has no way to see.
    let dir = scratch("zero");
    seed(&dir, "s1", &["needle"]);

    let value = search(&dir, json!({"text": "needle", "limit": 0})).await;

    assert_eq!(hits(&value).len(), 1, "{value}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- non-ASCII text ---------------------------------------------------------------------------

#[tokio::test]
async fn a_long_chinese_message_is_cut_on_a_character_boundary() {
    let dir = scratch("cjk");
    // Multi-byte characters throughout: a byte-offset cut would land inside one and produce U+FFFD.
    let long = "会话历史搜索引擎".repeat(40);
    seed(&dir, "s1", &[long.as_str()]);

    let value = search(&dir, json!({"text": "会话"})).await;

    let text = hits(&value)[0]["text"]
        .as_str()
        .expect("a string")
        .to_owned();
    assert!(text.ends_with('…'), "the cut is marked: {text}");
    assert_eq!(text.chars().count(), 201, "200 characters plus the marker");
    assert!(!text.contains('\u{fffd}'), "no character was split: {text}");
    assert!(long.starts_with(text.trim_end_matches('…')));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_long_emoji_message_is_cut_on_a_character_boundary() {
    let dir = scratch("emoji");
    // Astral-plane characters: four bytes each, so any byte-offset cut is almost certainly invalid.
    let long = "🚀".repeat(300);
    seed(&dir, "s1", &[long.as_str()]);

    let value = search(&dir, json!({"text": "🚀"})).await;

    let text = hits(&value)[0]["text"]
        .as_str()
        .expect("a string")
        .to_owned();
    assert!(text.ends_with('…'), "the cut is marked: {text}");
    assert_eq!(text.chars().count(), 201);
    assert!(!text.contains('\u{fffd}'), "no character was split: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_short_message_is_returned_whole() {
    let dir = scratch("short");
    seed(&dir, "s1", &["short enough"]);

    let value = search(&dir, json!({"text": "short"})).await;

    // No marker: an ellipsis on an uncut string would say the opposite of what happened.
    assert_eq!(hits(&value)[0]["text"], "short enough");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- filtering --------------------------------------------------------------------------------

#[tokio::test]
async fn a_kind_filter_excludes_other_events() {
    let dir = scratch("kind");
    let store = SessionStore::new(dir.join("s1.jsonl.zst"));
    store.touch().expect("created");
    store
        .append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    store
        .append(&SessionEvent::user_text("m1", "needle in a message"))
        .expect("appended");

    let messages = search(&dir, json!({"text": "needle", "kind": "messages"})).await;
    assert_eq!(hits(&messages).len(), 1, "{messages}");
    assert_eq!(hits(&messages)[0]["kind"], "user/message");

    let boundaries = search(&dir, json!({"kind": "boundaries"})).await;
    assert_eq!(hits(&boundaries).len(), 1, "{boundaries}");
    assert_eq!(hits(&boundaries)[0]["kind"], "turn/start");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_session_filter_narrows_the_scan() {
    let dir = scratch("session");
    seed(&dir, "a", &["needle"]);
    seed(&dir, "b", &["needle"]);

    let value = search(&dir, json!({"text": "needle", "session": "b"})).await;

    assert_eq!(hits(&value).len(), 1, "{value}");
    assert_eq!(hits(&value)[0]["session"], "b");
    // Only the named session was read: the count is of sessions examined, not sessions that exist.
    assert_eq!(value["sessionsScanned"], 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_kind_is_rejected_rather_than_widened() {
    // Dropping an unrecognised filter would return hits the caller meant to exclude, and the caller
    // would never learn that their word was wrong.
    let dir = scratch("badkind");
    seed(&dir, "s1", &["needle"]);

    let error = registry(&dir)
        .dispatch(
            "session",
            "search",
            call(json!({"text": "needle", "kind": "prompts"})),
        )
        .await
        .expect_err("rejected");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("boundaries"), "{error}");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_wrongly_typed_field_names_the_method() {
    let dir = scratch("badtype");
    seed(&dir, "s1", &["needle"]);

    let error = registry(&dir)
        .dispatch("session", "search", call(json!({"limit": "lots"})))
        .await
        .expect_err("rejected");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("session.search"), "{error}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn null_fields_are_treated_as_absent() {
    // A generated client sends every field, filling the unused ones with null; rejecting that would
    // make the method unusable from the client this surface exists for.
    let dir = scratch("null");
    seed(&dir, "s1", &["needle"]);

    let value = search(
        &dir,
        json!({"text": "needle", "session": null, "kind": null, "limit": null}),
    )
    .await;

    assert_eq!(hits(&value).len(), 1, "{value}");
    assert_eq!(value["truncated"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- registration -----------------------------------------------------------------------------

#[tokio::test]
async fn the_method_is_registered_under_the_session_namespace() {
    let dir = scratch("registered");
    let registry = registry(&dir);

    assert!(registry.get("session", "search").is_some());
    assert!(
        registry
            .names()
            .contains(&("session".to_owned(), "search".to_owned()))
    );

    let _ = std::fs::remove_dir_all(&dir);
}
