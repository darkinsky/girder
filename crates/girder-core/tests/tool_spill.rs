//! Tests for spilling an oversized tool result.
//!
//! Both halves matter: the text is saved so it is still readable, and the model is told where it went. Replacing
//! without saving loses the output, and saving without replacing puts it in the context window anyway.

use std::sync::Arc;

use girder_core::{
    AgentRuntime, Ceiling, ChunkStream, ContentBlock, EventBus, FinishReason, LlmError,
    LlmProvider, LlmRequest, RuntimeConfig, SessionStore, StreamChunk, TokenUsage, ToolRegistry,
};
use tokio_util::sync::CancellationToken;

/// A provider that asks for a tool once, then answers.
struct ToolCaller {
    emitted: std::sync::atomic::AtomicBool,
}

impl LlmProvider for ToolCaller {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        let first = !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            let mut chunks = vec![
                StreamChunk::Usage(TokenUsage {
                    input: 1,
                    output: 1,
                    cached: 0,
                }),
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: if first { "tool_use" } else { "text" }.to_owned(),
                },
            ];
            if first {
                chunks.push(StreamChunk::ToolCallDelta {
                    index: 0,
                    id: girder_core::ToolCallId::new("c1").expect("valid"),
                    name: Some("chatty".to_owned()),
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
                        name: "chatty".to_owned(),
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
    let dir =
        std::env::temp_dir().join(format!("girder-spill-t-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// A tool that returns `bytes` of output.
fn chatty_tool(bytes: usize) -> Arc<dyn girder_core::ToolRegistryView> {
    let registry = ToolRegistry::new();
    registry.register(girder_core::ToolDefinition::new(
        "chatty",
        "returns a lot",
        serde_json::json!({"type": "object"}),
        Arc::new(
            move |_args: serde_json::Value, _ctx: girder_core::ToolRunContext| {
                Box::pin(async move { Ok(girder_core::ToolResult::text("x".repeat(bytes))) })
            },
        ),
    ));
    Arc::new(registry)
}

/// Runs one prompt against a store, returning the tool result content.
async fn run(
    bytes: usize,
    limit: usize,
    with_store: bool,
) -> (Vec<ContentBlock>, Vec<String>, Vec<String>) {
    let dir = scratch("run");
    let spill_dir = dir.join("spills");
    let store = Arc::new(SessionStore::new(dir.join("s.jsonl.zst")));
    store.touch().expect("created");
    let session: Arc<dyn girder_core::Session> =
        Arc::clone(&store) as Arc<dyn girder_core::Session>;

    let spill_store: Option<Arc<dyn girder_core::SpillStore>> = with_store.then(|| {
        Arc::new(girder_core::LocalSpillStore::new(&spill_dir)) as Arc<dyn girder_core::SpillStore>
    });

    let runtime = AgentRuntime::with_spill(
        RuntimeConfig {
            provider: Arc::new(ToolCaller {
                emitted: std::sync::atomic::AtomicBool::new(false),
            }),
            tools: chatty_tool(bytes),
            bus: Arc::new(EventBus::new()),
            model: "test".to_owned(),
            max_turns: 2,
            token_budget: Ceiling::Unlimited,
        },
        spill_store,
        limit,
    );
    let runtime = Arc::new(runtime);
    let _ = AgentRuntime::prompt(&runtime, "s", "go", session).await;
    for _ in 0..100 {
        if runtime.running_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let events = store.read_all().expect("read");
    let content = events
        .iter()
        .find_map(|event| match event {
            girder_core::SessionEvent::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("a tool result");
    let spilled: Vec<String> = std::fs::read_dir(&spill_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path().display().to_string())
                .collect()
        })
        .unwrap_or_default();
    // Read the saved files before the directory goes away: a cleanup that races the assertion it exists for is
    // worse than no cleanup.
    let bodies: Vec<String> = spilled
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    (content, spilled, bodies)
}

#[tokio::test]
async fn a_small_result_is_returned_as_it_is() {
    let (content, spilled, _) = run(100, 32 * 1024, true).await;

    assert!(spilled.is_empty(), "nothing was saved");
    let text: String = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text.len(), 100);
}

#[tokio::test]
async fn an_oversized_result_is_replaced_by_a_locator() {
    let (content, spilled, _) = run(50_000, 1024, true).await;

    assert_eq!(spilled.len(), 1, "the output was saved");
    let text: String = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("too large"), "{text}");
    assert!(
        text.contains(&spilled[0]),
        "the locator names the file: {text}"
    );
    assert!(text.len() < 1000, "the result is a pointer, not the output");
}

#[tokio::test]
async fn the_saved_file_holds_the_whole_output() {
    // Replacing without saving would lose the output.
    let (_, spilled, bodies) = run(50_000, 1024, true).await;

    assert_eq!(spilled.len(), 1);
    assert_eq!(bodies.len(), 1, "the saved file was readable");
    assert_eq!(bodies[0].len(), 50_000);
    assert!(bodies[0].chars().all(|c| c == 'x'));
}

#[tokio::test]
async fn without_a_store_an_oversized_result_is_returned_unchanged() {
    // Losing it would be worse than a large result, and truncating it would lose output the model could have read.
    let (content, spilled, _) = run(50_000, 1024, false).await;

    assert!(spilled.is_empty(), "there is nowhere to save it");
    let text: String = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text.len(), 50_000, "the whole output survives");
}
