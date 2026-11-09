//! The agent loop: assemble, stream, dispatch tools, repeat.
//!
//! This is the vertical slice's spine. One turn is:
//!
//! 1. Build an [`LlmRequest`] from the conversation so far plus the tool schemas.
//! 2. Stream the response, appending chunks to the session as they arrive.
//! 3. Fold the stream into an [`AssistantMessage`] and record it.
//! 4. For each tool call, run the three-waterfall pipeline and record the result.
//! 5. If any tool ran, go to 1 with the results appended; otherwise the turn ends.
//!
//! # Why the loop yields steps instead of returning a result
//!
//! A `run(prompt) -> Result<()>` would hide every intermediate state behind one future. The CLI
//! needs to print deltas as they arrive, a test needs to assert the sequences, and cancellation
//! needs to be observable at a boundary. So the loop is a [`Stream`] of [`LoopStep`]: the caller
//! drives it, sees every stage, and can stop reading at any point.
//!
//! # Cancellation
//!
//! The token is checked between steps and forwarded into the provider. A cancelled loop returns
//! [`LoopError::Cancelled`] rather than a truncated success, because "the user pressed Ctrl-C" and
//! "the model finished" must not look the same to the caller writing the session log.

use std::sync::Arc;

use futures::Stream;
use tokio_util::sync::CancellationToken;

use crate::events::EventBus;
use crate::llm::{
    AssembleError, AssistantMessage, ContentBlock, FinishReason, LlmProvider, LlmRequest, Message,
    Role, StreamChunk,
};
use crate::scope::BoxFut;
use crate::session::{Session, SessionEvent};
use crate::tools::{
    PostExecuteArgs, PostToolDecision, PreToolDecision, ToolDefinition, ToolExecution,
    ToolPostExecute, ToolPreExecute, ToolRegistryView, ToolResult, ToolRunContext,
};

/// One observable step of the loop.
#[derive(Clone, Debug)]
pub enum LoopStep {
    /// A turn began.
    TurnStarted {
        /// Turn number, starting at 1.
        turn: u64,
    },

    /// A chunk arrived from the provider.
    Chunk(StreamChunk),

    /// The assistant's message for this turn is complete.
    AssistantMessage(AssistantMessage),

    /// A tool is about to run.
    ToolStarted {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
    },

    /// A tool finished.
    ToolFinished {
        /// Call id.
        id: String,
        /// Whether it failed.
        is_error: bool,
    },

    /// The turn ended.
    TurnEnded {
        /// Turn number.
        turn: u64,
        /// Why it stopped.
        reason: FinishReason,
    },
}

/// A failure the loop reports.
#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    /// The provider failed.
    #[error(transparent)]
    Llm(#[from] crate::llm::LlmError),

    /// The stream could not be assembled.
    #[error(transparent)]
    Assemble(#[from] AssembleError),

    /// The session log failed.
    #[error(transparent)]
    Session(#[from] crate::session::SessionError),

    /// The caller cancelled.
    #[error("loop cancelled")]
    Cancelled,

    /// A pre-execute layer denied the call.
    #[error("tool `{tool}` was denied: {reason}")]
    ToolDenied {
        /// Tool name.
        tool: String,
        /// Why.
        reason: String,
    },

    /// A tool failed in a way that is not a model-facing result.
    #[error(transparent)]
    Tool(#[from] crate::tools::ToolError),
}

/// What the loop needs to run.
///
/// Injected as a struct rather than resolved from the registry per call, so a test can build one
/// with fakes and no registry at all.
pub struct LoopContext {
    /// The provider.
    pub provider: Arc<dyn LlmProvider>,
    /// The tool registry.
    pub tools: Arc<dyn ToolRegistryView>,
    /// The session log.
    pub session: Arc<dyn Session>,
    /// The event bus, for the three tool waterfalls.
    pub bus: Arc<EventBus>,
    /// Model id to request.
    pub model: String,
    /// Which agent this loop runs for.
    pub agent_id: crate::brand::AgentId,
    /// Maximum turns before the loop stops on its own.
    pub max_turns: u32,
    /// How far this agent is from the root, for delegation budgeting.
    pub depth: crate::subagent::Depth,
    /// How many nested delegations this agent may make.
    pub max_depth: u32,
    /// Counter for minting message ids.
    ///
    /// A message id must be unique within a session, and a loop-local counter is enough: two loops
    /// never write to one session, because the runtime refuses a concurrent prompt.
    pub message_counter: Arc<std::sync::atomic::AtomicU64>,

    /// Whether the loop writes the prompt as a user message.
    ///
    /// True for a direct caller that owns the conversation (the CLI). False when the caller already
    /// recorded it — the RPC runtime writes the prompt before spawning, so that it is durable even
    /// if the model never answers, and a second write would put the same message in the log twice.
    pub record_prompt: bool,
}

impl LoopContext {
    /// Builds a context.
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<dyn ToolRegistryView>,
        session: Arc<dyn Session>,
        bus: Arc<EventBus>,
        model: impl Into<String>,
        agent_id: crate::brand::AgentId,
    ) -> Self {
        Self {
            provider,
            tools,
            session,
            bus,
            model: model.into(),
            agent_id,
            max_turns: 16,
            depth: crate::subagent::Depth::root(),
            max_depth: 2,
            message_counter: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            record_prompt: true,
        }
    }

    /// Sets the delegation depth and ceiling.
    #[must_use]
    pub fn with_depth(mut self, depth: crate::subagent::Depth, max_depth: u32) -> Self {
        self.depth = depth;
        self.max_depth = max_depth;
        self
    }
}

/// Runs one conversation turn by turn.
///
/// Returns a stream rather than a future; see the module docs.
pub fn run(
    context: Arc<LoopContext>,
    prompt: impl Into<String> + Send + 'static,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<LoopStep, LoopError>> + Send {
    let prompt = prompt.into();
    async_stream::stream! {
        use futures::StreamExt as _;
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt.clone() }],
        }];

        // The first user message is durable before any turn starts, unless the caller already
        // recorded it.
        if context.record_prompt {
            let prompt = crate::message::Message::new(
                context.next_message_id(),
                crate::message::Role::User,
                crate::message::MessageSource::User { form: None },
                messages[0].content.clone(),
            );
            if let Err(error) = context
                .session
                .append(SessionEvent::UserMessage { message: prompt })
                .await
            {
                yield Err(LoopError::Session(error));
                return;
            }
        }

        for turn in 1..=u64::from(context.max_turns) {
            if cancel.is_cancelled() {
                yield Err(LoopError::Cancelled);
                return;
            }

            let _ = context.session.append(SessionEvent::TurnStart { turn }).await;
            yield Ok(LoopStep::TurnStarted { turn });

            // Run the turn, forwarding every step it produces. The borrow of `messages` ends with
            // this block so the continuation decision below can read it.
            let (finished, ran_tools) = {
                let mut finished = false;
                let mut ran_tools = false;
                let mut folded = Box::pin(turn_stream(&context, &mut messages, turn, cancel.clone()));
                while let Some(step) = folded.next().await {
                    match &step {
                        Ok(LoopStep::TurnEnded { .. }) => finished = true,
                        Ok(LoopStep::ToolFinished { .. }) => ran_tools = true,
                        Err(_) => {
                            yield step;
                            return;
                        }
                        _ => {}
                    }
                    yield step;
                    if finished {
                        break;
                    }
                }
                (finished, ran_tools)
            };

            if !finished {
                // The turn produced no terminal step: treat it as a loop that ran out.
                return;
            }

            // A turn that ran tools continues so the model can see the results; a turn that only
            // answered ends the conversation.
            if !ran_tools {
                return;
            }
        }

        // The turn budget ran out without the model finishing without tools.
        yield Err(LoopError::Cancelled);
    }
}

/// Runs one turn: stream, assemble, dispatch tools.
///
/// Yields steps and appends to `messages` so the caller can decide whether to continue. Split out
/// of [`run`] because the streaming and tool dispatch are two concerns and joining them made one
/// function unreadable.
fn turn_stream<'a>(
    context: &'a Arc<LoopContext>,
    messages: &'a mut Vec<Message>,
    turn: u64,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<LoopStep, LoopError>> + Send + 'a {
    async_stream::stream! {
        use futures::StreamExt as _;
        let request = LlmRequest::new(context.model.clone(), messages.clone())
            .with_tools(context.tools.schemas());
        let stream = match context.provider.stream(request, cancel.clone()).await {
            Ok(stream) => stream,
            Err(error) => {
                yield Err(LoopError::Llm(error));
                return;
            }
        };

        let mut chunks: Vec<StreamChunk> = Vec::new();
        {
            use futures::StreamExt as _;
            let mut stream = stream;
            while let Some(chunk) = stream.next().await {
                if cancel.is_cancelled() {
                    yield Err(LoopError::Cancelled);
                    return;
                }
                chunks.push(chunk.clone());
                yield Ok(LoopStep::Chunk(chunk));
            }
        }

        let assistant = match AssistantMessage::from_chunks(chunks) {
            Ok(message) => message,
            Err(error) => {
                yield Err(LoopError::Assemble(error));
                return;
            }
        };
        let assistant_message = crate::message::Message::new(
            context.next_message_id(),
            crate::message::Role::Assistant,
            crate::message::MessageSource::Model {
                provider: context.model.clone(),
                model: context.model.clone(),
                replay_state: None,
            },
            assistant.content.clone(),
        );
        let _ = context
            .session
            .append(SessionEvent::AssistantMessage {
                message: assistant_message,
            })
            .await;
        yield Ok(LoopStep::AssistantMessage(assistant.clone()));

        let calls = assistant.tool_calls();
        if calls.is_empty() {
            let reason = assistant.finish.clone().unwrap_or(FinishReason::Stop);
            let _ = context
                .session
                .append(SessionEvent::TurnEnd {
                    turn,
                    reason: crate::session::TurnEndReason::from(reason.clone()),
                })
                .await;
            yield Ok(LoopStep::TurnEnded { turn, reason });
            return;
        }

        messages.push(Message {
            role: Role::Assistant,
            content: assistant.content.clone(),
        });

        let mut tool_steps = Box::pin(tool_calls(context, messages, calls, &cancel));
        while let Some(step) = tool_steps.next().await {
            let done = step.is_err();
            yield step;
            if done {
                return;
            }
        }

        let _ = context
            .session
            .append(SessionEvent::TurnEnd {
                turn,
                reason: crate::session::TurnEndReason::Completed,
            })
            .await;
        yield Ok(LoopStep::TurnEnded {
            turn,
            reason: FinishReason::ToolCalls,
        });
    }
}

/// Runs every tool call the model requested, appending results to `messages`.
fn tool_calls<'a>(
    context: &'a Arc<LoopContext>,
    messages: &'a mut Vec<Message>,
    calls: Vec<(crate::brand::ToolCallId, &'a str, &'a str)>,
    cancel: &'a CancellationToken,
) -> impl Stream<Item = Result<LoopStep, LoopError>> + Send + 'a {
    async_stream::stream! {
        for (id, name, arguments) in calls {
            if cancel.is_cancelled() {
                yield Err(LoopError::Cancelled);
                return;
            }
            yield Ok(LoopStep::ToolStarted {
                id: id.to_string(),
                name: name.to_owned(),
            });

            let execution = ToolExecution {
                call_id: id.clone(),
                name: name.to_owned(),
                arguments: arguments.to_owned(),
                agent_id: context.agent_id.clone(),
            };
            let _ = context
                .session
                .append(SessionEvent::ToolCall {
                    id: id.clone(),
                    name: name.to_owned(),
                    arguments: arguments.to_owned(),
                })
                .await;

            let result = match dispatch(context, execution, cancel.clone()).await {
                Ok(result) => result,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let is_error = result.is_error;

            let _ = context
                .session
                .append(SessionEvent::ToolResult {
                    id: id.clone(),
                    content: result.content.clone(),
                    is_error,
                })
                .await;
            yield Ok(LoopStep::ToolFinished {
                id: id.to_string(),
                is_error,
            });

            messages.push(Message {
                role: Role::Tool,
                content: result.content,
            });
        }
    }
}

/// Runs the three-waterfall pipeline for one call.
///
/// Order matters and is upstream's: pre-execute decides, execute calls, post-execute normalizes.
/// A `Deny` from the first stops before the body runs, which is why it returns a `ToolResult`
/// carrying the reason rather than an error — the model should see the denial and adapt.
pub async fn dispatch(
    context: &LoopContext,
    execution: ToolExecution,
    cancel: CancellationToken,
) -> Result<ToolResult, LoopError> {
    let Some(tool) = context.tools.get(&execution.name) else {
        return Ok(ToolResult::error(format!(
            "no tool named `{}`",
            execution.name
        )));
    };

    let decision = context
        .bus
        .waterfall::<ToolPreExecute, _>(execution.clone(), |_args| {
            Box::pin(async { PreToolDecision::Allow })
        })
        .await;

    match decision {
        PreToolDecision::Allow => {}
        PreToolDecision::Deny { reason } => {
            return Ok(ToolResult::error(format!(
                "tool `{}` denied: {reason}",
                execution.name
            )));
        }
        PreToolDecision::Cancel => {
            return Err(LoopError::Cancelled);
        }
    }

    // The execute waterfall wraps the body. Its terminal is the actual call.
    let call = execution.clone();
    let body_cancel = cancel.clone();
    let result = context
        .bus
        .waterfall::<crate::tools::ToolExecute, _>(execution.clone(), move |_args| {
            let call = call.clone();
            let tool = Arc::clone(&tool);
            let cancel = body_cancel.clone();
            Box::pin(async move { run_body(tool, call, cancel).await })
        })
        .await;

    let args = PostExecuteArgs {
        execution: execution.clone(),
        result: result.clone(),
    };
    let decision = context
        .bus
        .waterfall::<ToolPostExecute, _>(args, accept_terminal)
        .await;

    Ok(match decision {
        PostToolDecision::Accept => result,
        PostToolDecision::Replace { content } => ToolResult { content, ..result },
        PostToolDecision::Block { feedback } => ToolResult {
            content: feedback,
            value: None,
            is_error: true,
        },
    })
}

/// Runs a tool body, converting its error into a model-facing failure.
///
/// A tool error is not a loop error: the model asked for something that failed, and it should see
/// the failure and decide what to do. Only a cancellation escalates.
async fn run_body(
    tool: Arc<ToolDefinition>,
    execution: ToolExecution,
    cancel: CancellationToken,
) -> ToolResult {
    if cancel.is_cancelled() {
        return ToolResult::error("cancelled before the tool started");
    }

    let arguments: serde_json::Value = match serde_json::from_str(&execution.arguments) {
        Ok(value) => value,
        Err(error) => {
            return ToolResult::error(format!(
                "arguments for `{}` are not valid JSON: {error}",
                execution.name
            ));
        }
    };

    let context = ToolRunContext { execution, cancel };
    match (tool.execute)(arguments, context).await {
        Ok(result) => result,
        Err(error) => ToolResult::error(error.to_string()),
    }
}

/// The terminal for the post-execute waterfall: accept unless a layer replaces the result.
///
/// A free function rather than a closure because the waterfall's terminal must be `'static`.
fn accept_terminal(_args: PostExecuteArgs) -> BoxFut<PostToolDecision> {
    Box::pin(async { PostToolDecision::Accept })
}

impl LoopContext {
    /// Mints a message id for this session.
    ///
    /// `msg-<n>` rather than a UUID: the counter is per-loop, the loop owns one session, and a
    /// monotonic id makes a log readable and diffable.
    #[must_use]
    pub fn next_message_id(&self) -> crate::brand::MessageId {
        let value = self
            .message_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::brand::MessageId::new(format!("msg-{value}"))
            .unwrap_or_else(|_| crate::brand::MessageId::new("msg-0").expect("constant is valid"))
    }
}
