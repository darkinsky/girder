//! End-to-end tests for the goal path through `AgentRuntime`.
//!
//! `tests/goal_wiring.rs` proves that `agent_loop::run` continues when its `LoopContext` carries an armed
//! goal. That is necessary and not sufficient: the defect this file exists to catch was in the *runtime*,
//! which built a context without ever reading the goal it had been handed. A loop-level test passes while
//! the shipped product has a goal tool mutating a state no loop ever reads.
//!
//! So these tests go through the same door the CLI and the RPC server use — `AgentRuntime::with_goal` plus
//! `AgentRuntime::prompt` — and assert on what lands in the session log, because the log is what a client
//! replays.

use std::sync::Arc;

use girder_core::{
    AgentRuntime, Ceiling, ChunkStream, ContentBlock, EventBus, FinishReason, GoalId, GoalState,
    LlmError, LlmProvider, LlmRequest, MemorySession, RuntimeConfig, Session, SessionEvent,
    StreamChunk, ToolRegistry, ToolRegistryView,
};
use tokio_util::sync::CancellationToken;

/// A provider that always answers with plain text and no tool calls, counting its calls.
///
/// Counting is how the test tells "the loop continued" from "the loop stopped": one call is one answer, so
/// more than one call can only come from a continuation.
struct PlainAnswer {
    calls: Arc<std::sync::atomic::AtomicU32>,
}

impl LlmProvider for PlainAnswer {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            let chunks = vec![
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: "text".to_owned(),
                },
                StreamChunk::TextDelta {
                    index: 0,
                    text: "working".to_owned(),
                },
                StreamChunk::Finish {
                    reason: FinishReason::Stop,
                },
            ];
            Ok(Box::pin(futures::stream::iter(chunks)) as ChunkStream)
        })
    }
}

/// Builds an armed goal with `rounds` of budget, the way `GoalState::create` does for a user request.
fn armed_goal(rounds: u32) -> Arc<std::sync::Mutex<GoalState>> {
    let mut state = GoalState::new();
    state
        .create(
            GoalId::new("g1").expect("a non-empty id is valid"),
            "reach the objective",
            Some(rounds),
            0,
        )
        .expect("an armed goal is created from a fresh state");
    Arc::new(std::sync::Mutex::new(state))
}

/// Runs one prompt through the runtime and returns the session log and the model-call count.
///
/// The runtime spawns the loop rather than awaiting it, so the test waits for `running_count` to reach zero.
/// Reading the log immediately would see only the prompt the runtime wrote before the spawn.
async fn run_with_goal(goal: Option<Arc<std::sync::Mutex<GoalState>>>) -> (Vec<SessionEvent>, u32) {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let session = Arc::new(MemorySession::new());

    let config = RuntimeConfig {
        provider: Arc::new(PlainAnswer {
            calls: Arc::clone(&calls),
        }),
        tools: Arc::new(ToolRegistry::new()) as Arc<dyn ToolRegistryView>,
        bus: Arc::new(EventBus::new()),
        model: "test".to_owned(),
        max_turns: 8,
        token_budget: Ceiling::Unlimited,
    };
    let runtime = match goal {
        Some(goal) => AgentRuntime::with_goal(config, goal),
        // The control case: the same runtime with `new`, which must behave exactly as it did before the goal
        // existed.
        None => AgentRuntime::new(config),
    };

    let result = AgentRuntime::prompt(
        &runtime,
        "s",
        "start",
        Arc::clone(&session) as Arc<dyn Session>,
    )
    .await;
    assert!(
        result.is_ok(),
        "the prompt must start the first turn: {result:?}"
    );
    assert_eq!(result.ok(), Some(1), "a fresh session starts at turn 1");

    for _ in 0..200 {
        if runtime.running_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        runtime.running_count(),
        0,
        "the loop must finish within the test's wait"
    );

    (
        session.events(),
        calls.load(std::sync::atomic::Ordering::SeqCst),
    )
}

/// The defect: the runtime held no goal, so an armed goal changed nothing about a prompt.
///
/// This is the assertion that would have failed before the wiring: `with_goal` is the only difference between
/// this test and its control, and the model is called more than once.
#[tokio::test]
async fn an_armed_goal_continues_a_runtime_prompt() {
    let goal = armed_goal(3);
    let (events, calls) = run_with_goal(Some(Arc::clone(&goal))).await;

    assert!(
        calls > 1,
        "an armed goal must keep the loop going past the first answer, but the model was called {calls} time(s)"
    );

    // The budget is what stops it: one initial turn plus one turn per continuation round.
    assert_eq!(
        calls, 4,
        "three continuation rounds must produce exactly three continuation calls"
    );

    let rounds = goal
        .lock()
        .ok()
        .and_then(|state| state.current().map(|goal| goal.rounds_started));
    assert_eq!(
        rounds,
        Some(3),
        "every continuation must be charged against the round budget"
    );

    // The session log is what a client replays, so the proof belongs there rather than only in a counter.
    let starts = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::TurnStart { .. }))
        .count();
    assert_eq!(starts, 4, "four model rounds must be four recorded turns");
}

/// The continuation must be durable and distinguishable from the user's own words.
///
/// A replay that could not tell them apart would show the user asking to continue a goal they never saw.
#[tokio::test]
async fn the_continuation_prompt_is_recorded_with_its_own_text() {
    let goal = armed_goal(1);
    let (events, calls) = run_with_goal(Some(goal)).await;

    assert_eq!(calls, 2, "one continuation round means one extra call");

    let continuations: Vec<&ContentBlock> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::UserMessage { message }
                if message.text().contains("Continue working toward the goal") =>
            {
                message.content.first()
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        continuations.len(),
        1,
        "the continuation prompt must be recorded once, not silently kept in memory"
    );
}

/// The control: without a goal the runtime behaves exactly as before.
///
/// Pinned because the fix must not turn every prompt into a multi-turn loop; a runtime with no goal is the
/// common case and one answer is the correct number.
#[tokio::test]
async fn a_runtime_without_a_goal_answers_once() {
    let (events, calls) = run_with_goal(None).await;

    assert_eq!(calls, 1, "no goal means no continuation");
    let starts = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::TurnStart { .. }))
        .count();
    assert_eq!(starts, 1, "one turn recorded for one answer");
}

/// A spent budget stops the continuation instead of looping forever.
///
/// `GoalState::create` treats a cap of zero as the default, so a goal that cannot continue is built by
/// completing the round budget rather than by asking for zero rounds.
#[tokio::test]
async fn an_exhausted_goal_round_budget_stops_the_runtime_loop() {
    let goal = armed_goal(1);
    // Spend the only round before the prompt: the loop may start, but there is nothing left to continue with.
    {
        let mut state = goal.lock().expect("the goal mutex is not poisoned");
        state
            .start_round(0)
            .expect("the first round is within budget");
    }

    let (_, calls) = run_with_goal(Some(goal)).await;

    assert_eq!(calls, 1, "an exhausted budget must not buy another turn");
}
