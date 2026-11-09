//! Tests for goal-driven continuation (M4 of the v2 plan).
//!
//! A goal differs from a prompt in exactly one way: it makes the loop keep going after a turn that
//! would otherwise end. These tests pin that, and pin that the round budget stops it.

use futures::StreamExt as _;
use girder_core::{
    AgentId, EventBus, FinishReason, GoalId, GoalState, LlmError, LlmProvider, LlmRequest,
    LoopContext, MemorySession, Session, StreamChunk, ToolRegistryView,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A provider that always answers with plain text and no tools.
struct PlainAnswer {
    calls: Arc<std::sync::atomic::AtomicU32>,
}

impl LlmProvider for PlainAnswer {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, LlmError>> {
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
            Ok(Box::pin(futures::stream::iter(chunks)) as girder_core::ChunkStream)
        })
    }
}

fn context(
    calls: Arc<std::sync::atomic::AtomicU32>,
    goal: Option<Arc<std::sync::Mutex<GoalState>>>,
) -> Arc<LoopContext> {
    let base = LoopContext::new(
        Arc::new(PlainAnswer { calls }),
        Arc::new(girder_core::ToolRegistry::default()) as Arc<dyn ToolRegistryView>,
        Arc::new(MemorySession::new()) as Arc<dyn Session>,
        Arc::new(EventBus::new()),
        "m",
        AgentId::new("a1").expect("valid"),
    );
    Arc::new(match goal {
        Some(goal) => base.with_goal(goal),
        None => base,
    })
}

fn armed_goal(rounds: u32) -> Arc<std::sync::Mutex<GoalState>> {
    let mut state = GoalState::new();
    state
        .create(
            GoalId::new("g1").unwrap(),
            "reach the objective",
            Some(rounds),
            0,
        )
        .expect("created");
    Arc::new(std::sync::Mutex::new(state))
}

// ---- without a goal ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_turn_ends_after_one_answer() {
    // The baseline: one model call, then the conversation ends.
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let steps: Vec<_> = girder_core::run(
        context(calls.clone(), None),
        "hello",
        CancellationToken::new(),
    )
    .collect()
    .await;

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(steps.iter().any(Result::is_ok));
}

// ---- with an armed goal -----------------------------------------------------------------------

#[tokio::test]
async fn an_armed_goal_keeps_the_loop_going() {
    // The single property that makes a goal different from a prompt.
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let goal = armed_goal(3);

    let _: Vec<_> = girder_core::run(
        context(calls.clone(), Some(goal.clone())),
        "start",
        CancellationToken::new(),
    )
    .collect()
    .await;

    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) > 1,
        "an armed goal must continue past the first answer"
    );
    assert_eq!(
        goal.lock().unwrap().current().map(|g| g.rounds_started),
        Some(3),
        "the budget must be reached and then stop"
    );
}

#[tokio::test]
async fn the_round_budget_stops_continuation() {
    // An unreachable objective must stop rather than spend turns forever.
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let goal = armed_goal(2);

    let _: Vec<_> = girder_core::run(
        context(calls.clone(), Some(goal.clone())),
        "start",
        CancellationToken::new(),
    )
    .collect()
    .await;

    // One initial answer plus two continuation rounds.
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the loop must stop when the budget is spent"
    );
}

#[tokio::test]
async fn a_paused_goal_does_not_continue() {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let goal = armed_goal(5);
    {
        let mut state = goal.lock().unwrap();
        let reference = state.current().unwrap().reference();
        state.pause(&reference).unwrap();
    }

    let _: Vec<_> = girder_core::run(
        context(calls.clone(), Some(goal)),
        "start",
        CancellationToken::new(),
    )
    .collect()
    .await;

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a paused goal must not continue"
    );
}

#[tokio::test]
async fn a_disarmed_goal_does_not_continue() {
    // The restart case: the goal is active on disk but this process has not been asked to continue.
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut state = GoalState::new();
    let created = state
        .create(GoalId::new("g1").unwrap(), "objective", Some(5), 0)
        .unwrap();
    // Restoring the same goal leaves it disarmed, which is what a fresh process sees.
    let mut restored = GoalState::new();
    restored.restore(created);
    let goal = Arc::new(std::sync::Mutex::new(restored));

    let _: Vec<_> = girder_core::run(
        context(calls.clone(), Some(goal)),
        "start",
        CancellationToken::new(),
    )
    .collect()
    .await;

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a restored goal must not continue on its own"
    );
}

#[tokio::test]
async fn a_continuation_prompt_is_recorded_in_the_log() {
    // The continuation must be durable, or a replay cannot explain why the loop took another turn.
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let goal = armed_goal(1);
    let session = Arc::new(MemorySession::new());

    let context = {
        let base = LoopContext::new(
            Arc::new(PlainAnswer { calls }),
            Arc::new(girder_core::ToolRegistry::default()) as Arc<dyn ToolRegistryView>,
            Arc::clone(&session) as Arc<dyn Session>,
            Arc::new(EventBus::new()),
            "m",
            AgentId::new("a1").unwrap(),
        );
        Arc::new(base.with_goal(goal))
    };

    let _: Vec<_> = girder_core::run(context, "start", CancellationToken::new())
        .collect()
        .await;

    let events = session.events();
    let continuation = events.iter().any(|event| {
        matches!(
            event,
            girder_core::SessionEvent::UserMessage { message }
                if message.text().contains("Continue working toward the goal")
        )
    });
    assert!(continuation, "the continuation prompt must be durable");
}

#[tokio::test]
async fn a_completed_goal_does_not_continue() {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let goal = armed_goal(5);
    {
        let mut state = goal.lock().unwrap();
        let reference = state.current().unwrap().reference();
        state.complete(&reference).unwrap();
    }

    let _: Vec<_> = girder_core::run(
        context(calls.clone(), Some(goal)),
        "start",
        CancellationToken::new(),
    )
    .collect()
    .await;

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}
