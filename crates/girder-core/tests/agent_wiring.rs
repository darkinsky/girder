//! Tests for the agent-to-loop wiring (M4 of the v2 plan).
//!
//! Before this wiring the loop kept its own model string, its own cancellation, and no idea that
//! pending input existed. Each of those is a way for the UI to see one thing and the loop do another,
//! so each has a test here.

use futures::StreamExt as _;
use girder_core::{
    Agent, AgentId, AgentOptions, AgentSpec, ChatMessage, Depth, EventBus, FinishReason,
    InboxTarget, LlmError, LlmProvider, LlmRequest, LoopContext, MemorySession, MessageId, Session,
    SessionEvent, SessionId, Splice, StreamChunk, ToolRegistryView,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A provider that streams one assistant text message and stops.
struct OneShot {
    calls: Arc<std::sync::atomic::AtomicU32>,
}

impl LlmProvider for OneShot {
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
                    text: "ok".to_owned(),
                },
                StreamChunk::Finish {
                    reason: FinishReason::Stop,
                },
            ];
            Ok(Box::pin(futures::stream::iter(chunks)) as girder_core::ChunkStream)
        })
    }
}

/// A provider that keeps streaming text until cancelled, like a long answer.
struct Endless;

impl LlmProvider for Endless {
    fn stream(
        &self,
        _request: LlmRequest,
        cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, LlmError>> {
        Box::pin(async move {
            let stream = async_stream::stream! {
                let mut index = 0u64;
                loop {
                    if cancel.is_cancelled() {
                        yield StreamChunk::Finish { reason: FinishReason::Aborted };
                        break;
                    }
                    yield StreamChunk::TextDelta { index: 0, text: format!("{index}") };
                    index += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            };
            Ok(Box::pin(stream) as girder_core::ChunkStream)
        })
    }
}

fn agent(_session: &Arc<MemorySession>) -> Arc<Agent> {
    Arc::new(Agent::new(AgentSpec::root(
        SessionId::new("session-1").expect("valid"),
        AgentOptions::new("test", "test-model"),
    )))
}

fn context(
    provider: Arc<dyn LlmProvider>,
    session: Arc<MemorySession>,
    agent: Option<Arc<Agent>>,
) -> Arc<LoopContext> {
    let base = LoopContext::new(
        provider,
        Arc::new(girder_core::ToolRegistry::default()) as Arc<dyn ToolRegistryView>,
        session as Arc<dyn Session>,
        Arc::new(EventBus::new()),
        "test-model",
        AgentId::new("a1").expect("valid"),
    );
    Arc::new(match agent {
        Some(agent) => base.with_agent(agent),
        None => base,
    })
}

fn new_session(_name: &str) -> Arc<MemorySession> {
    Arc::new(MemorySession::new())
}

// ---- model selection --------------------------------------------------------------------------

#[tokio::test]
async fn the_loop_records_the_model_it_used() {
    // So "last used" reflects reality rather than the last thing a caller asked for.
    let session = new_session("wiring-model");
    let agent = agent(&session);
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let context = context(
        Arc::new(OneShot {
            calls: calls.clone(),
        }),
        Arc::clone(&session),
        Some(Arc::clone(&agent)),
    );

    let steps: Vec<_> = girder_core::run(context, "hello", CancellationToken::new())
        .collect()
        .await;

    assert!(steps.iter().any(Result::is_ok));
    let view = agent.model_view();
    assert!(
        view.last_used.is_some(),
        "the loop must record the selection it used"
    );
    assert_eq!(view.last_used.unwrap().model, "test-model");
}

#[tokio::test]
async fn a_loop_without_an_agent_still_runs() {
    // The CLI path attaches no agent; attaching one must not become mandatory.
    let session = new_session("wiring-none");
    let context = context(
        Arc::new(OneShot {
            calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }),
        Arc::clone(&session),
        None,
    );

    let steps: Vec<_> = girder_core::run(context, "hello", CancellationToken::new())
        .collect()
        .await;

    assert!(steps.iter().any(Result::is_ok));
}

// ---- cancellation -----------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_through_the_agent_stops_the_loop() {
    // The property the wiring exists for: a UI cancels through the agent, and a loop watching only
    // the caller's token would keep running after the user's interrupt.
    let session = new_session("wiring-cancel");
    let agent = agent(&session);
    let context = context(
        Arc::new(Endless),
        Arc::clone(&session),
        Some(Arc::clone(&agent)),
    );

    let mut stream = Box::pin(girder_core::run(
        context,
        "talk forever",
        CancellationToken::new(),
    ));

    // Let the loop start streaming, then cancel through the agent.
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await;
    assert!(first.is_ok(), "the loop must start");
    agent.cancel(true);

    let mut saw_cancel = false;
    for _ in 0..40 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
            Ok(Some(Err(girder_core::LoopError::Cancelled))) => {
                saw_cancel = true;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }

    assert!(saw_cancel, "the loop must observe the agent's cancellation");
}

// ---- pending input ----------------------------------------------------------------------------

#[tokio::test]
async fn queued_input_continues_the_conversation() {
    // A user who types while the model is answering expects that message to be answered. Ending the
    // turn silently drops it, which is what happened before the wiring.
    let session = new_session("wiring-inbox");
    let agent = agent(&session);
    // Queue before the loop runs, so it is present when the first turn ends.
    agent
        .splice_inbox(&Splice::append(
            InboxTarget::NextTurn,
            vec![ChatMessage::user(
                MessageId::new("queued").unwrap(),
                "follow up",
            )],
        ))
        .expect("spliced");

    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let context = context(
        Arc::new(OneShot {
            calls: calls.clone(),
        }),
        Arc::clone(&session),
        Some(Arc::clone(&agent)),
    );

    let steps: Vec<_> = girder_core::run(context, "first", CancellationToken::new())
        .collect()
        .await;

    // Two model calls: the first answer, then the queued follow-up.
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the queued message must be answered, not dropped"
    );
    // The queued message reached the log.
    let events: Vec<SessionEvent> = session.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::UserMessage { message } if message.text() == "follow up"
        )),
        "the queued message must be durable"
    );
    assert!(steps.iter().any(Result::is_ok));
}

#[tokio::test]
async fn an_empty_inbox_does_not_continue() {
    // The normal case must not loop forever: no tools and nothing queued ends the conversation.
    let session = new_session("wiring-empty");
    let agent = agent(&session);
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let context = context(
        Arc::new(OneShot {
            calls: calls.clone(),
        }),
        Arc::clone(&session),
        Some(Arc::clone(&agent)),
    );

    let steps: Vec<_> = girder_core::run(context, "hello", CancellationToken::new())
        .collect()
        .await;

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an empty inbox must not trigger a second turn"
    );
    assert!(steps.iter().any(Result::is_ok));
}

#[tokio::test]
async fn the_queued_message_is_taken_from_the_inbox() {
    // Consumed, not merely observed: leaving it queued would answer it again on the next run.
    let session = new_session("wiring-take");
    let agent = agent(&session);
    agent
        .splice_inbox(&Splice::append(
            InboxTarget::NextTurn,
            vec![ChatMessage::user(MessageId::new("q1").unwrap(), "once")],
        ))
        .unwrap();

    let context = context(
        Arc::new(OneShot {
            calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }),
        Arc::clone(&session),
        Some(Arc::clone(&agent)),
    );

    let _: Vec<_> = girder_core::run(context, "go", CancellationToken::new())
        .collect()
        .await;

    assert!(
        agent.inbox_state().next_turn().is_empty(),
        "the queued message must be consumed"
    );
}

// ---- depth ------------------------------------------------------------------------------------

#[tokio::test]
async fn the_loop_carries_its_depth() {
    let session = new_session("wiring-depth");
    let base = LoopContext::new(
        Arc::new(OneShot {
            calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }),
        Arc::new(girder_core::ToolRegistry::default()) as Arc<dyn ToolRegistryView>,
        Arc::clone(&session) as Arc<dyn Session>,
        Arc::new(EventBus::new()),
        "m",
        AgentId::new("a1").unwrap(),
    )
    .with_depth(Depth::root().child(), 3);

    assert_eq!(base.depth.get(), 1);
    assert_eq!(base.max_depth, 3);
}
