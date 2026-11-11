//! Tests that a tool's result reaches the caller that renders the turn.
//!
//! Found by running the CLI: it printed `[tool] done` and nothing else, so a person who asked for a command to run
//! never saw what it returned. The loop was yielding the step without the content, and nothing in the test suite
//! noticed because every test read the session log instead of the step stream.

use std::sync::Arc;

use futures::StreamExt as _;
use girder_core::{
    ChunkStream, ContentBlock, EventBus, FinishReason, LlmError, LlmProvider, LlmRequest, LoopStep,
    SessionStore, StreamChunk, TokenUsage, ToolRegistry,
};
use tokio_util::sync::CancellationToken;

/// A provider that calls a tool once, then answers.
struct CallsOnce {
    done: std::sync::atomic::AtomicBool,
}

impl LlmProvider for CallsOnce {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        let first = !self.done.swap(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            let mut chunks = vec![
                StreamChunk::Usage(TokenUsage {
                    input: 1,
                    output: 1,
                    cached: 0,
                }),
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: if first { "tool-call" } else { "text" }.to_owned(),
                },
            ];
            if first {
                chunks.push(StreamChunk::ToolCallDelta {
                    index: 0,
                    id: girder_core::ToolCallId::new("c1").expect("valid"),
                    name: Some("echo".to_owned()),
                    arguments_delta: "{}".to_owned(),
                });
            } else {
                chunks.push(StreamChunk::TextDelta {
                    index: 0,
                    text: "done".to_owned(),
                });
            }
            chunks.push(StreamChunk::BlockEnd {
                index: 0,
                block: if first {
                    ContentBlock::ToolCall {
                        id: girder_core::ToolCallId::new("c1").expect("valid"),
                        name: "echo".to_owned(),
                        arguments: "{}".to_owned(),
                    }
                } else {
                    ContentBlock::Text {
                        text: "done".to_owned(),
                    }
                },
            });
            chunks.push(StreamChunk::Finish {
                reason: if first {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                },
            });
            Ok(Box::pin(futures::stream::iter(chunks)) as ChunkStream)
        })
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-vis-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

#[tokio::test]
async fn a_tool_result_reaches_the_step_stream() {
    // This is the property the CLI depended on and did not have.
    let dir = scratch("steps");
    let store = Arc::new(SessionStore::new(dir.join("s.jsonl.zst")));
    store.touch().expect("created");
    let session: Arc<dyn girder_core::Session> =
        Arc::clone(&store) as Arc<dyn girder_core::Session>;

    let registry = ToolRegistry::new();
    registry.register(girder_core::ToolDefinition::new(
        "echo",
        "returns a line",
        serde_json::json!({"type": "object"}),
        Arc::new(
            |_args: serde_json::Value, _ctx: girder_core::ToolRunContext| {
                Box::pin(async { Ok(girder_core::ToolResult::text("the tool said this")) })
            },
        ),
    ));

    // Driven through the loop directly so the steps are observable: `prompt` spawns and discards them, which is
    // exactly why the CLI's own rendering had drifted from what the loop produced.
    let context = girder_core::LoopContext::new(
        runtime_provider(),
        runtime_tools(),
        session,
        Arc::new(EventBus::new()),
        "test",
        girder_core::AgentId::new("a").expect("valid"),
    );
    let context = Arc::new(context);
    let mut stream = Box::pin(girder_core::run(
        context,
        "go".to_owned(),
        CancellationToken::new(),
    ));
    let mut finished = Vec::new();
    while let Some(step) = stream.next().await {
        if let Ok(LoopStep::ToolFinished { content, .. }) = step {
            finished.push(content);
        }
    }

    assert_eq!(finished.len(), 1, "one tool ran");
    let text: String = finished[0]
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("the tool said this"),
        "the result must reach the caller that renders the turn: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn runtime_provider() -> Arc<dyn LlmProvider> {
    Arc::new(CallsOnce {
        done: std::sync::atomic::AtomicBool::new(false),
    })
}

fn runtime_tools() -> Arc<dyn girder_core::ToolRegistryView> {
    let registry = ToolRegistry::new();
    registry.register(girder_core::ToolDefinition::new(
        "echo",
        "returns a line",
        serde_json::json!({"type": "object"}),
        Arc::new(
            |_args: serde_json::Value, _ctx: girder_core::ToolRunContext| {
                Box::pin(async { Ok(girder_core::ToolResult::text("the tool said this")) })
            },
        ),
    ));
    Arc::new(registry)
}
