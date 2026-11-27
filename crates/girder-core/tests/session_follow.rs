//! Tests for `session.follow`.
//!
//! The Web client opens this as a stream. The first frame is the opening snapshot, and each later
//! frame is one durable event that arrived after that cursor.

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

struct Echo;

impl girder_core::LlmProvider for Echo {
    fn stream(
        &self,
        _request: girder_core::LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, girder_core::LlmError>> {
        let chunks = vec![
            girder_core::StreamChunk::BlockStart {
                index: 0,
                block_type: "text".to_owned(),
            },
            girder_core::StreamChunk::TextDelta {
                index: 0,
                text: "echoed".to_owned(),
            },
            girder_core::StreamChunk::Finish {
                reason: girder_core::FinishReason::Stop,
            },
        ];
        Box::pin(
            async move { Ok(Box::pin(futures::stream::iter(chunks)) as girder_core::ChunkStream) },
        )
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

async fn opened(
    registry: &girder_core::RemoteRegistry,
    args: serde_json::Value,
) -> (
    tokio_util::sync::CancellationToken,
    tokio::sync::mpsc::UnboundedReceiver<Result<serde_json::Value, girder_core::RemoteError>>,
    tokio::task::JoinHandle<()>,
) {
    let call = call(args);
    let handler = registry
        .stream_method("session", "follow", &call)
        .expect("follow is a stream");
    let cancel = tokio_util::sync::CancellationToken::new();
    let (open, frames) = girder_core::StreamOpen::pair(
        call,
        Arc::new(girder_core::api::extra::UplinkInbox::new(
            1024,
            "session/follow",
        )),
        cancel.clone(),
    );
    let running = tokio::spawn(async move {
        handler(open).await;
    });
    (cancel, frames, running)
}

async fn next_frame(
    frames: &mut tokio::sync::mpsc::UnboundedReceiver<
        Result<serde_json::Value, girder_core::RemoteError>,
    >,
) -> serde_json::Value {
    tokio::time::timeout(std::time::Duration::from_secs(1), frames.recv())
        .await
        .expect("frame arrived")
        .expect("stream open")
        .expect("frame ok")
}

#[tokio::test]
async fn the_opening_snapshot_contains_the_log() {
    let dir = scratch("first");
    seed(&dir, "s1", 3);
    let registry = registry(&dir);
    let (cancel, mut frames, running) = opened(
        &registry,
        json!({"address": {"kind": "session", "sessionId": "s1"}}),
    )
    .await;

    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["cursor"], 2);
    assert_eq!(snapshot["header"]["version"], 4);
    assert_eq!(snapshot["header"]["delegationDepth"], 0);
    assert_eq!(snapshot["header"]["id"], "s1");
    assert_eq!(snapshot["projections"]["asOfSeq"], 3);
    assert_eq!(
        snapshot["projections"]["values"]["turnBoundary"]["lastTurn"],
        0
    );
    let records = snapshot["records"].as_array().expect("records");
    assert_eq!(records.len(), 3);
    let event = &records[0]["event"];
    assert_eq!(event["type"], "user/message");
    assert_eq!(event["seq"], 0);
    assert_eq!(event["time"], 0);
    assert_eq!(event["surfaceOp"], "append");
    assert_eq!(event["data"]["role"], "user");
    assert!(event.get("message").is_none());
    girder_core::session::client::assert_session_wire_event(event).expect("wire envelope");
    assert!(snapshot.get("assistantStream").is_none());
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_opted_in_opening_carries_an_idle_assistant_baseline() {
    let dir = scratch("assistant");
    seed(&dir, "s1", 1);
    let registry = registry(&dir);
    let (cancel, mut frames, running) = opened(
        &registry,
        json!({
            "address": {"kind": "session", "sessionId": "s1"},
            "assistantStream": true,
        }),
    )
    .await;

    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["assistantStream"], json!({ "revision": 0 }));
    assert!(snapshot["assistantStream"].get("activeAttempt").is_none());
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn live_text_is_followed_by_the_message_and_then_the_end() {
    let dir = scratch("live");
    seed(&dir, "s1", 1);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let runtime = Arc::new(girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
        provider: Arc::new(Silent),
        tools: Arc::new(girder_core::ToolRegistry::new()),
        bus: Arc::new(girder_core::EventBus::new()),
        model: "test".to_owned(),
        max_turns: 1,
        token_budget: girder_core::Ceiling::Unlimited,
    }));
    let registry = girder_core::registry_with_runtime(catalog, Arc::clone(&runtime));
    let (cancel, mut frames, running) = opened(
        &registry,
        json!({
            "address": {"kind": "session", "sessionId": "s1"},
            "assistantStream": true,
        }),
    )
    .await;
    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["assistantStream"]["revision"], 0);

    runtime.assistant().begin("s1", 1, 0, 0);
    runtime.assistant().push_chunk(
        "s1",
        json!({"type": "text-delta", "index": 0, "text": "hi"}),
    );
    let start = next_frame(&mut frames).await;
    assert_eq!(start["type"], "assistant-stream");
    assert_eq!(start["frame"]["type"], "start");
    assert_eq!(start["frame"]["revision"], 1);
    let chunk = next_frame(&mut frames).await;
    assert_eq!(chunk["frame"]["type"], "chunk");
    assert_eq!(chunk["frame"]["revision"], 2);
    assert_eq!(chunk["frame"]["index"], 0);
    assert_eq!(chunk["frame"]["chunk"]["text"], "hi");

    SessionStore::new(dir.join("s1.jsonl.zst"))
        .append(&SessionEvent::assistant_text("a1", "hi", "test"))
        .expect("appended");
    runtime.assistant().commit("s1", 1);
    let event = next_frame(&mut frames).await;
    assert_eq!(event["type"], "event");
    assert_eq!(event["event"]["type"], "assistant/message");
    assert_eq!(event["event"]["seq"], 1);
    let end = next_frame(&mut frames).await;
    assert_eq!(end["frame"]["type"], "end");
    assert_eq!(end["frame"]["revision"], 3);
    assert_eq!(end["frame"]["index"], 1);
    assert_eq!(end["frame"]["outcome"]["kind"], "committed");
    assert_eq!(end["frame"]["outcome"]["eventType"], "assistant/message");
    assert_eq!(end["frame"]["outcome"]["seq"], 1);
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_late_follower_folds_the_open_attempt_and_does_not_replay_its_start() {
    let dir = scratch("late");
    seed(&dir, "s1", 1);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let runtime = Arc::new(girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
        provider: Arc::new(Silent),
        tools: Arc::new(girder_core::ToolRegistry::new()),
        bus: Arc::new(girder_core::EventBus::new()),
        model: "test".to_owned(),
        max_turns: 1,
        token_budget: girder_core::Ceiling::Unlimited,
    }));
    runtime.assistant().begin("s1", 1, 0, 0);
    runtime.assistant().push_chunk(
        "s1",
        json!({"type": "text-delta", "index": 0, "text": "hi"}),
    );
    let registry = girder_core::registry_with_runtime(catalog, Arc::clone(&runtime));
    let (cancel, mut frames, running) = opened(
        &registry,
        json!({
            "address": {"kind": "session", "sessionId": "s1"},
            "assistantStream": true,
        }),
    )
    .await;
    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["assistantStream"]["revision"], 2);
    assert_eq!(snapshot["assistantStream"]["activeAttempt"]["nextIndex"], 1);
    assert_eq!(
        snapshot["assistantStream"]["activeAttempt"]["stream"][0]["type"],
        "text-chunks"
    );
    assert_eq!(
        snapshot["assistantStream"]["activeAttempt"]["stream"][0]["texts"],
        json!(["hi"])
    );

    SessionStore::new(dir.join("s1.jsonl.zst"))
        .append(&SessionEvent::assistant_text("a1", "hi", "test"))
        .expect("appended");
    runtime.assistant().commit("s1", 1);
    let event = next_frame(&mut frames).await;
    assert_eq!(event["type"], "event");
    assert_eq!(event["event"]["seq"], 1);
    let mut end = None;
    for _ in 0..20 {
        let frame = next_frame(&mut frames).await;
        if frame["type"] == "assistant-stream" && frame["frame"]["type"] == "end" {
            end = Some(frame);
            break;
        }
    }
    let end = end.expect("assistant stream ended");
    assert_eq!(end["frame"]["revision"], 3);
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn prompt_runtime_reaches_follow_as_live_assistant_frames() {
    let dir = scratch("runtime-live");
    seed(&dir, "s1", 0);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let runtime = girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
        provider: Arc::new(Echo),
        tools: Arc::new(girder_core::ToolRegistry::new()),
        bus: Arc::new(girder_core::EventBus::new()),
        model: "test".to_owned(),
        max_turns: 1,
        token_budget: girder_core::Ceiling::Unlimited,
    });
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), runtime);
    let (cancel, mut frames, running) = opened(
        &registry,
        json!({
            "address": {"kind": "session", "sessionId": "s1"},
            "assistantStream": true,
        }),
    )
    .await;
    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["cursor"], -1);

    let accepted = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "s1", "text": "hello"})),
        )
        .await
        .expect("prompt accepted");
    assert_eq!(accepted["accepted"], true);

    let start = next_frame(&mut frames).await;
    assert_eq!(start["type"], "assistant-stream");
    assert_eq!(start["frame"]["type"], "start");
    let chunk = next_frame(&mut frames).await;
    assert_eq!(chunk["frame"]["type"], "chunk");
    assert_eq!(chunk["frame"]["chunk"]["type"], "text-delta");
    assert_eq!(chunk["frame"]["chunk"]["text"], "echoed");

    let mut assistant_event = None;
    let mut seen = Vec::new();
    for _ in 0..20 {
        let frame = next_frame(&mut frames).await;
        seen.push(frame.clone());
        if frame["type"] == "event" && frame["event"]["type"] == "assistant/message" {
            assistant_event = Some(frame);
            break;
        }
    }
    let assistant_event =
        assistant_event.unwrap_or_else(|| panic!("assistant message reached follow: {seen:?}"));
    let mut end = None;
    for _ in 0..20 {
        let frame = next_frame(&mut frames).await;
        if frame["type"] == "assistant-stream" && frame["frame"]["type"] == "end" {
            end = Some(frame);
            break;
        }
    }
    let end = end.expect("assistant stream ended");
    assert_eq!(end["type"], "assistant-stream");
    assert_eq!(end["frame"]["type"], "end");
    assert_eq!(end["frame"]["outcome"]["kind"], "committed");
    assert_eq!(
        end["frame"]["outcome"]["seq"],
        assistant_event["event"]["seq"]
    );
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn consecutive_attempts_keep_each_end_before_the_next_start() {
    let dir = scratch("consecutive");
    seed(&dir, "s1", 0);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let runtime = girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
        provider: Arc::new(Silent),
        tools: Arc::new(girder_core::ToolRegistry::new()),
        bus: Arc::new(girder_core::EventBus::new()),
        model: "test".to_owned(),
        max_turns: 1,
        token_budget: girder_core::Ceiling::Unlimited,
    });
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));
    let (cancel, mut frames, running) = opened(
        &registry,
        json!({
            "address": {"kind": "session", "sessionId": "s1"},
            "assistantStream": true,
        }),
    )
    .await;
    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["cursor"], -1);

    runtime.assistant().begin("s1", 1, 0, -1);
    runtime.assistant().push_chunk(
        "s1",
        json!({"type": "text-delta", "index": 0, "text": "one"}),
    );
    SessionStore::new(dir.join("s1.jsonl.zst"))
        .append(&SessionEvent::assistant_text("a1", "one", "test"))
        .expect("first message appended");
    runtime.assistant().commit("s1", 0);

    runtime.assistant().begin("s1", 1, 1, 0);
    runtime.assistant().push_chunk(
        "s1",
        json!({"type": "text-delta", "index": 0, "text": "two"}),
    );
    SessionStore::new(dir.join("s1.jsonl.zst"))
        .append(&SessionEvent::assistant_text("a2", "two", "test"))
        .expect("second message appended");
    runtime.assistant().commit("s1", 1);

    let mut received = Vec::new();
    for _ in 0..8 {
        received.push(next_frame(&mut frames).await);
    }
    let kinds = received
        .iter()
        .map(|frame| {
            if frame["type"] == "assistant-stream" {
                frame["frame"]["type"].as_str().unwrap_or("invalid")
            } else {
                frame["event"]["type"].as_str().unwrap_or("invalid")
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "start",
            "chunk",
            "assistant/message",
            "end",
            "start",
            "chunk",
            "assistant/message",
            "end",
        ]
    );
    assert_eq!(received[3]["frame"]["revision"], 3);
    assert_eq!(received[4]["frame"]["revision"], 4);
    assert_eq!(received[7]["frame"]["revision"], 6);
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_new_event_arrives_after_the_snapshot() {
    let dir = scratch("resume");
    seed(&dir, "s1", 2);
    let registry = registry(&dir);
    let (cancel, mut frames, running) = opened(&registry, json!({"sessionId": "s1"})).await;
    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["cursor"], 1);

    SessionStore::new(dir.join("s1.jsonl.zst"))
        .append(&SessionEvent::user_text("m9", "late"))
        .expect("appended");
    let frame = next_frame(&mut frames).await;
    assert_eq!(frame["type"], "event");
    assert_eq!(frame["event"]["seq"], 2);
    assert_eq!(frame["event"]["type"], "user/message");
    assert_eq!(frame["event"]["data"]["content"][0]["text"], "late");
    assert_eq!(frame["event"]["surfaceOp"], "append");
    girder_core::session::client::assert_session_wire_event(&frame["event"])
        .expect("wire envelope");
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_opening_snapshot_uses_the_persisted_creation_time() {
    let dir = scratch("created-at");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.create("s1").await.expect("session created");
    let header = girder_core::read_session_header(dir.join("s1.jsonl.zst"))
        .expect("session header read")
        .expect("session header");
    let created_at = header["createdAt"].as_u64().expect("creation timestamp");
    let registry = registry(&dir);
    let (cancel, mut frames, running) = opened(&registry, json!({"sessionId": "s1"})).await;

    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["header"]["createdAt"], created_at);
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_session_opens_with_an_empty_snapshot() {
    let dir = scratch("unknown");
    let registry = registry(&dir);
    let (cancel, mut frames, running) = opened(&registry, json!({"sessionId": "absent"})).await;

    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["header"]["version"], 4);
    assert_eq!(snapshot["header"]["delegationDepth"], 0);
    assert_eq!(snapshot["records"].as_array().map(Vec::len), Some(0));
    assert_eq!(snapshot["cursor"], -1);
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_session_id_is_refused() {
    let dir = scratch("missing");
    let registry = registry(&dir);
    let (_cancel, mut frames, running) = opened(&registry, json!({})).await;

    let error = tokio::time::timeout(std::time::Duration::from_secs(1), frames.recv())
        .await
        .expect("refusal arrived")
        .expect("stream open")
        .expect_err("missing id");
    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn max_messages_keeps_the_newest_window() {
    let dir = scratch("limit");
    seed(&dir, "s1", 10);
    let registry = registry(&dir);
    let (cancel, mut frames, running) =
        opened(&registry, json!({"sessionId": "s1", "maxMessages": 4})).await;

    let snapshot = next_frame(&mut frames).await;
    assert_eq!(snapshot["records"].as_array().map(Vec::len), Some(4));
    assert_eq!(snapshot["cursor"], 9);
    assert_eq!(snapshot["hasMore"], true);
    assert_eq!(snapshot["records"][0]["event"]["seq"], 6);
    cancel.cancel();
    let _ = running.await;
    let _ = std::fs::remove_dir_all(&dir);
}
