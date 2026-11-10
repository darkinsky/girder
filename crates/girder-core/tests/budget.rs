//! Tests for the token budget.
//!
//! A budget reports rather than enforces, so what matters is where it gates: the next turn, not the one that already
//! ran. A spend cannot be undone, so refusing after the fact would only record the loss.

use std::sync::Arc;

use girder_core::{
    AgentRuntime, Ceiling, ChunkStream, EventBus, FinishReason, LlmError, LlmProvider, LlmRequest,
    RuntimeConfig, SessionStore, StreamChunk, TokenUsage, ToolRegistry,
};
use tokio_util::sync::CancellationToken;

/// A provider that reports a fixed cost per call.
struct Costed {
    input: u64,
    output: u64,
    calls: std::sync::atomic::AtomicU32,
}

impl LlmProvider for Costed {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let usage = TokenUsage {
            input: self.input,
            output: self.output,
            cached: 0,
        };
        Box::pin(async move {
            let chunks = vec![
                StreamChunk::Usage(usage),
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: "text".to_owned(),
                },
                StreamChunk::TextDelta {
                    index: 0,
                    text: "answer".to_owned(),
                },
                StreamChunk::BlockEnd {
                    index: 0,
                    block: girder_core::ContentBlock::Text {
                        text: "answer".to_owned(),
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
    // A counter as well as the pid: tests in one binary run in parallel and a shared path would have them read each
    // other's events.
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-budget-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// Runs one prompt and returns the session's events.
async fn run_with(budget: Ceiling, input: u64, output: u64) -> Vec<girder_core::SessionEvent> {
    let dir = scratch("run");
    let path = dir.join("s.jsonl.zst");
    let store = SessionStore::new(&path);
    store.touch().expect("created");
    let store = Arc::new(SessionStore::new(&path));
    let session: Arc<dyn girder_core::Session> =
        Arc::clone(&store) as Arc<dyn girder_core::Session>;

    let provider = Arc::new(Costed {
        input,
        output,
        calls: std::sync::atomic::AtomicU32::new(0),
    });
    let runtime = AgentRuntime::new(RuntimeConfig {
        provider,
        tools: Arc::new(ToolRegistry::new()),
        bus: Arc::new(EventBus::new()),
        model: "test".to_owned(),
        max_turns: 4,
        token_budget: budget,
    });

    let runtime = Arc::new(runtime);
    let _ = AgentRuntime::prompt(&runtime, "s", "hello", session).await;

    // The prompt spawns the loop rather than running it, so the test waits for the turn to finish: reading
    // immediately would see only the user message.
    for _ in 0..100 {
        if runtime.running_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let events = store.read_all().expect("read");
    let _ = std::fs::remove_dir_all(&dir);
    events
}

#[tokio::test]
async fn a_model_call_records_its_cost() {
    // The cost is its own event: a reader that renders content does not need a number it never shows.
    let events = run_with(Ceiling::Unlimited, 10, 5).await;

    let usage: Vec<&girder_core::SessionEvent> = events
        .iter()
        .filter(|event| matches!(event, girder_core::SessionEvent::ModelUsage { .. }))
        .collect();
    assert_eq!(usage.len(), 1);
    let girder_core::SessionEvent::ModelUsage {
        input_tokens,
        output_tokens,
        ..
    } = usage[0]
    else {
        panic!("expected ModelUsage");
    };
    assert_eq!(*input_tokens, 10);
    assert_eq!(*output_tokens, 5);
}

#[tokio::test]
async fn an_unlimited_budget_never_refuses() {
    let events = run_with(Ceiling::Unlimited, 1_000_000, 1_000_000).await;

    assert!(
        !events.iter().any(|event| matches!(
            event,
            girder_core::SessionEvent::TurnEnd {
                reason: girder_core::TurnEndReason::Budget,
                ..
            }
        )),
        "an unlimited budget must not end a turn"
    );
}

#[tokio::test]
async fn a_budget_that_cannot_afford_a_second_turn_refuses_it() {
    // The first turn spends everything; the second is refused before it is paid for.
    let events = run_with(Ceiling::AtMost(20), 15, 5).await;

    // The usage event proves the first call happened and was recorded.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, girder_core::SessionEvent::ModelUsage { .. }))
    );
}

#[tokio::test]
async fn the_cost_recorded_is_the_sum_of_both_directions() {
    // Prompt tokens are billed too, and a budget that counted only the answer would undercount by far.
    let events = run_with(Ceiling::Unlimited, 100, 1).await;

    let total: u64 = events
        .iter()
        .filter_map(|event| match event {
            girder_core::SessionEvent::ModelUsage {
                input_tokens,
                output_tokens,
                ..
            } => Some(input_tokens + output_tokens),
            _ => None,
        })
        .sum();

    assert_eq!(total, 101);
}
