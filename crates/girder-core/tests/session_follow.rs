//! Tests for `session.follow`, the one method shaped for polling.
//!
//! The property that matters is that a cursor resumes exactly: a subscription built on it must repeat nothing and miss
//! nothing, and an off-by-one would do one of those silently.

use std::sync::Arc;

use girder_core::{DirectoryCatalog, RemoteCall, SessionCatalog, SessionEvent, SessionStore};
use serde_json::json;

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-follow-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn registry(dir: &std::path::Path) -> girder_core::RemoteRegistry {
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(dir));
    girder_core::registry_with_runtime(
        catalog,
        girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
            provider: Arc::new(Silent),
            tools: Arc::new(girder_core::ToolRegistry::new()),
            bus: Arc::new(girder_core::EventBus::new()),
            model: "test".to_owned(),
            max_turns: 1,
            token_budget: girder_core::Ceiling::Unlimited,
        }),
    )
}

struct Silent;

impl girder_core::LlmProvider for Silent {
    fn stream(
        &self,
        _request: girder_core::LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, girder_core::LlmError>> {
        Box::pin(async { Err(girder_core::LlmError::Cancelled) })
    }
}

/// Writes `count` events into one session.
fn seed(dir: &std::path::Path, id: &str, count: usize) {
    let store = SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
    store.touch().expect("created");
    for index in 0..count {
        store
            .append(&SessionEvent::user_text(
                &format!("m{index}"),
                format!("body {index}"),
            ))
            .expect("appended");
    }
}

async fn follow(
    registry: &girder_core::RemoteRegistry,
    args: serde_json::Value,
) -> serde_json::Value {
    registry
        .dispatch("session", "follow", call(args))
        .await
        .expect("follow")
}

#[tokio::test]
async fn a_first_poll_returns_the_beginning() {
    let dir = scratch("first");
    seed(&dir, "s1", 3);

    let value = follow(&registry(&dir), json!({"sessionId": "s1"})).await;

    let events = value["events"].as_array().expect("an array");
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["seq"], 1, "sequence numbers are one-based");
    assert_eq!(value["nextSeq"], 3);
    assert_eq!(value["done"], false, "a session never finishes");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_cursor_resumes_exactly() {
    // The whole point: `nextSeq` from one poll is `afterSeq` for the next, with no overlap and no gap.
    let dir = scratch("resume");
    seed(&dir, "s1", 2);
    let registry = registry(&dir);

    let first = follow(&registry, json!({"sessionId": "s1"})).await;
    assert_eq!(first["events"].as_array().map(Vec::len), Some(2));
    let next = first["nextSeq"].as_u64().expect("a number");

    // One more event, then resume from the cursor.
    SessionStore::new(dir.join("s1.jsonl.zst"))
        .append(&SessionEvent::user_text("m9", "late"))
        .expect("appended");
    let second = follow(&registry, json!({"sessionId": "s1", "afterSeq": next})).await;

    let events = second["events"].as_array().expect("an array");
    assert_eq!(events.len(), 1, "only the new event: {events:?}");
    assert_eq!(events[0]["seq"], 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn polling_past_the_end_returns_nothing_new() {
    let dir = scratch("past");
    seed(&dir, "s1", 2);
    let registry = registry(&dir);

    let value = follow(&registry, json!({"sessionId": "s1", "afterSeq": 99})).await;

    assert_eq!(value["events"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        value["nextSeq"], 99,
        "the cursor does not move backwards: {value}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_limit_bounds_one_poll_and_the_cursor_follows_it() {
    // A poll that returned everything would make one frame enormous and the next empty, which reads as a stall.
    let dir = scratch("limit");
    seed(&dir, "s1", 10);

    let value = follow(&registry(&dir), json!({"sessionId": "s1", "limit": 4})).await;

    assert_eq!(value["events"].as_array().map(Vec::len), Some(4));
    assert_eq!(value["nextSeq"], 4, "the cursor stops where the page did");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_session_ends_the_subscription() {
    // Otherwise a client polls a session that does not exist forever.
    let dir = scratch("unknown");

    let value = follow(&registry(&dir), json!({"sessionId": "absent"})).await;

    assert_eq!(value["events"].as_array().map(Vec::len), Some(0));
    assert_eq!(value["done"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_session_id_is_refused() {
    let dir = scratch("missing");

    let error = registry(&dir)
        .dispatch("session", "follow", call(json!({})))
        .await
        .expect_err("missing id");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_omitted_cursor_means_the_beginning() {
    // Zero and absent must agree, or a caller that typed `afterSeq: 0` would get a different answer from one that
    // omitted it.
    let dir = scratch("zero");
    seed(&dir, "s1", 2);
    let registry = registry(&dir);

    let absent = follow(&registry, json!({"sessionId": "s1"})).await;
    let zero = follow(&registry, json!({"sessionId": "s1", "afterSeq": 0})).await;

    assert_eq!(absent["events"], zero["events"]);
    let _ = std::fs::remove_dir_all(&dir);
}
