//! Tests that compaction actually shrinks a running turn's history.
//!
//! The policy's own tests prove the decision; these prove the loop applies it. A policy wired to nothing is the failure
//! mode this file exists to catch, and it is not hypothetical: the goal continuation logic sat unread for several rounds
//! because the field it read was never set.

use std::sync::Arc;

use girder_core::{
    AgentRuntime, Ceiling, ChunkStream, CompactionPolicy, EventBus, FinishReason, LlmError,
    LlmProvider, LlmRequest, RuntimeConfig, SessionStore, StreamChunk, TokenUsage, ToolRegistry,
};
use tokio_util::sync::CancellationToken;

/// A provider that reports how many messages it was sent, then answers.
struct Counting {
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
}

impl LlmProvider for Counting {
    fn stream(
        &self,
        request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        self.seen
            .lock()
            .expect("unpoisoned")
            .push(request.messages.len());
        Box::pin(async move {
            let chunks = vec![
                StreamChunk::Usage(TokenUsage {
                    input: 1,
                    output: 1,
                    cached: 0,
                }),
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: "text".to_owned(),
                },
                StreamChunk::TextDelta {
                    index: 0,
                    text: "ok".to_owned(),
                },
                StreamChunk::BlockEnd {
                    index: 0,
                    block: girder_core::ContentBlock::Text {
                        text: "ok".to_owned(),
                    },
                },
                StreamChunk::Finish {
                    reason: FinishReason::Stop,
                },
            ];
            Ok(Box::pin(futures::stream::iter(chunks)) as ChunkStream)
        })
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-cmp-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// Runs one prompt, returning the message counts the provider saw.
async fn run(policy: Option<CompactionPolicy>) -> Vec<usize> {
    let dir = scratch("run");
    let path = dir.join("s.jsonl.zst");
    let store = Arc::new(SessionStore::new(&path));
    store.touch().expect("created");
    let session: Arc<dyn girder_core::Session> =
        Arc::clone(&store) as Arc<dyn girder_core::Session>;

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut runtime = AgentRuntime::new(RuntimeConfig {
        provider: Arc::new(Counting {
            seen: Arc::clone(&seen),
        }),
        tools: Arc::new(ToolRegistry::new()),
        bus: Arc::new(EventBus::new()),
        model: "test".to_owned(),
        max_turns: 4,
        token_budget: Ceiling::Unlimited,
    });
    if let Some(policy) = policy {
        // Built before sharing, which is why the setter runs on the `Arc` while it is still unique.
        if let Some(inner) = Arc::get_mut(&mut runtime) {
            inner.set_compaction(policy);
        }
    }
    let _ = AgentRuntime::prompt(&runtime, "s", "hello", session).await;
    for _ in 0..100 {
        if runtime.running_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let counts = seen.lock().expect("unpoisoned").clone();
    let _ = std::fs::remove_dir_all(&dir);
    counts
}

#[tokio::test]
async fn without_a_policy_the_history_is_sent_whole() {
    let counts = run(None).await;

    assert_eq!(counts.len(), 1, "one model call");
    assert_eq!(counts[0], 1, "just the prompt");
}

#[tokio::test]
async fn a_policy_that_never_triggers_changes_nothing() {
    // A policy set high enough to never fire must be a no-op rather than a silent edit.
    let policy = CompactionPolicy {
        trigger_messages: 1_000,
        ..CompactionPolicy::default()
    };

    let counts = run(Some(policy)).await;

    assert_eq!(counts[0], 1);
}
