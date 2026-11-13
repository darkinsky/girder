//! The agent runtime: turns a `session.prompt` into a running loop, and streams its steps out.
//!
//! Upstream's `session.prompt` enqueues and returns; the turn's output reaches the client through
//! `session/event` frames on the mux. This module does the same: [`AgentRuntime::prompt`] records
//! the user message, spawns the loop, and returns immediately. Steps are published on the event bus
//! as [`AgentEvent`], which the RPC layer already knows how to forward.
//!
//! # Why the runtime owns running agents
//!
//! Two prompts to the same session must not interleave — a second prompt while a turn runs would
//! append messages out of order and the model would see a conversation that never happened. The
//! runtime therefore tracks one running turn per session and refuses a second one.
//!
//! # Cancellation
//!
//! A running turn holds a [`CancellationToken`]. `session.cancel` upstream aborts the turn; this
//! module exposes the same through [`AgentRuntime::cancel`]. The token is checked between steps by
//! the loop, so cancellation is bounded by one model chunk rather than by the whole turn.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{LoopContext, LoopStep};
use crate::brand::AgentId;
use crate::events::{Event, EventBus};
use crate::llm::{ContentBlock, FinishReason, LlmProvider, StreamChunk};
use crate::session::{Session, SessionEvent};
use crate::tools::ToolRegistryView;

/// A step the runtime publishes as it runs a turn.
///
/// Deliberately a small, flat vocabulary: a client renders these, and forwarding the loop's own
/// `LoopStep` would couple the wire format to an internal enum that will grow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AgentEvent {
    /// A turn began.
    TurnStarted {
        /// Session it belongs to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Turn number.
        turn: u64,
    },

    /// Visible text arrived.
    TextDelta {
        /// Session it belongs to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// The increment.
        text: String,
    },

    /// A tool is about to run.
    ToolStarted {
        /// Session it belongs to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Tool name.
        name: String,
    },

    /// A tool finished.
    ToolFinished {
        /// Session it belongs to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Whether it failed.
        #[serde(rename = "isError")]
        is_error: bool,
    },

    /// The turn ended.
    TurnEnded {
        /// Session it belongs to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Turn number.
        turn: u64,
        /// Why it stopped.
        reason: FinishReason,
    },
}

/// Emitted when a turn's text grows.
///
/// A separate event rather than a field on a generic "step" event, because a client subscribes to
/// exactly this to paint streaming output.
pub struct AgentTurn;

impl Event for AgentTurn {
    const NAME: &'static str = "agent/turn";
    type Args = AgentEvent;
}

/// A failure raised while prompting.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// A turn is already running for this session.
    #[error("session `{session}` is already running a turn")]
    Busy {
        /// The session.
        session: String,
    },

    /// The session log could not be written.
    #[error("session log: {0}")]
    Session(#[from] crate::session::SessionError),
}

/// What the runtime needs to run a turn.
pub struct RuntimeConfig {
    /// The provider.
    pub provider: Arc<dyn LlmProvider>,
    /// The tool registry.
    pub tools: Arc<dyn ToolRegistryView>,
    /// The event bus, for both tool waterfalls and turn events.
    pub bus: Arc<EventBus>,
    /// Model id to request.
    pub model: String,
    /// Maximum turns per prompt.
    pub max_turns: u32,
    /// What this session may spend on tokens.
    ///
    /// Unlimited by default: a deployment that does not charge should not acquire a ceiling because a field was
    /// omitted. A caller that does charge sets it, and the loop refuses a step that would exceed it rather than
    /// discovering the overshoot afterwards — the spend cannot be undone.
    pub token_budget: crate::limits::Ceiling,
}

/// A running turn.
struct Running {
    cancel: CancellationToken,
}

/// Runs prompts against sessions.
pub struct AgentRuntime {
    /// The model's context window, when the deployment states one.
    context_window: Option<u64>,
    /// The output limit to ask for, which is subtracted from the window before the input is measured.
    max_output_tokens: Option<u32>,
    config: RuntimeConfig,
    /// Where an oversized tool result is saved, when this deployment has storage for it.
    spill: Option<Arc<dyn crate::spill::Store>>,
    /// How many bytes a tool result may carry before it is spilled.
    spill_limit: usize,
    /// How the history is compacted, when a deployment configured it.
    compaction: Option<crate::compaction_hook::CompactionPolicy>,
    /// The goal the loop advances, when one is set.
    ///
    /// Shared with the `goal` tool rather than owned: a tool that kept its own copy would let the model complete a
    /// goal the loop still believed was open, and the loop would keep continuing toward something already done.
    goal: Option<Arc<std::sync::Mutex<crate::goal::GoalState>>>,
    /// One running turn per session id.
    running: Mutex<BTreeMap<String, Running>>,
}

impl AgentRuntime {
    /// Builds a runtime.
    #[must_use]
    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        Self::assemble(config, None, crate::spill::DEFAULT_SPILL_LIMIT, None, None)
    }

    /// Builds a runtime already wired to the goal the loop advances.
    ///
    /// A constructor rather than "call [`Self::set_goal`] on what [`Self::new`] returned" because `new` hands back an
    /// `Arc`: a fresh `Arc` has no other owner, so `Arc::get_mut` happens to work today, but it silently returns
    /// `None` the moment a caller clones the runtime before setting the goal — leaving the loop with no goal while
    /// the tool mutates one. Construction has no such ordering trap.
    ///
    /// The goal is a separate parameter rather than a [`RuntimeConfig`] field because it is *shared*, not owned: the
    /// same `Arc` goes to the `goal` tool, and a config that took the state by value would invite two copies.
    #[must_use]
    pub fn with_goal(
        config: RuntimeConfig,
        goal: Arc<std::sync::Mutex<crate::goal::GoalState>>,
    ) -> Arc<Self> {
        Self::assemble(
            config,
            None,
            crate::spill::DEFAULT_SPILL_LIMIT,
            Some(goal),
            None,
        )
    }

    /// Builds a runtime with every optional capability a server attaches.
    ///
    /// One constructor rather than four setters because the runtime is shared the moment it exists, and a setter
    /// applied to a clone wires only that clone -- the server keeps driving the original. The earlier `set_goal`
    /// carried that hazard in its documentation instead of its type, which is worse than a long argument list.
    #[must_use]
    pub fn assemble_with(
        config: RuntimeConfig,
        goal: Option<Arc<std::sync::Mutex<crate::goal::GoalState>>>,
        compaction: Option<crate::compaction_hook::CompactionPolicy>,
    ) -> Arc<Self> {
        Self::assemble(
            config,
            None,
            crate::spill::DEFAULT_SPILL_LIMIT,
            goal,
            compaction,
        )
    }

    /// Builds a runtime that spills oversized tool results.
    ///
    /// Separate from [`Self::new`] rather than a field on the config because the store is a capability a deployment
    /// either has or does not: a builder that accepted `None` would make every caller decide, and most have no
    /// opinion.
    #[must_use]
    pub fn with_spill(
        config: RuntimeConfig,
        spill: Option<Arc<dyn crate::spill::Store>>,
        spill_limit: usize,
    ) -> Arc<Self> {
        Self::assemble(config, spill, spill_limit, None, None)
    }

    /// The one place a runtime is assembled.
    ///
    /// Private because a constructor that forgot a field is a bug no type can catch: with every `Arc::new(Self { .. })`
    /// in one place, adding a field means visiting one function rather than three.
    fn assemble(
        config: RuntimeConfig,
        spill: Option<Arc<dyn crate::spill::Store>>,
        spill_limit: usize,
        goal: Option<Arc<std::sync::Mutex<crate::goal::GoalState>>>,
        compaction: Option<crate::compaction_hook::CompactionPolicy>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            running: Mutex::new(BTreeMap::new()),
            spill,
            spill_limit,
            goal,
            compaction,
            // Unset until a deployment states them: the runtime does not know which model it is talking to beyond the
            // name, and a guessed window would make the budget check refuse requests the model would accept.
            context_window: None,
            max_output_tokens: None,
        })
    }

    /// States what the model accepts, so a request is refused before it is sent rather than after.
    ///
    /// Takes `&mut Arc<Self>` because the runtime is shared by the time a caller knows: the same allocation every
    /// session holds is the one that has to carry the number.
    pub fn set_model_limits(this: &mut Arc<Self>, window: Option<u64>, output: Option<u32>) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.context_window = window;
            inner.max_output_tokens = output;
        }
    }

    /// Attaches the goal state the loop advances to a runtime that is not shared yet.
    ///
    /// The same `Arc` must go to the `goal` tool, or the model and the loop disagree about whether the goal is open.
    ///
    /// Takes `&mut self` and therefore only works while the runtime has no other owner: a caller that has already
    /// shared the `Arc` cannot reach this method at all. [`Self::with_goal`] is the form to prefer, because it
    /// cannot be skipped between construction and the first prompt.
    pub fn set_goal(&mut self, goal: Arc<std::sync::Mutex<crate::goal::GoalState>>) {
        self.goal = Some(goal);
    }

    /// Sets how the history is compacted when it outgrows the context.
    ///
    /// Setting it is opt-in: the default keeps everything, because a deployment that has not decided what to drop
    /// should not have a default decide for it.
    pub fn set_compaction(&mut self, policy: crate::compaction_hook::CompactionPolicy) {
        self.compaction = Some(policy);
    }

    /// Number of sessions with a turn in flight, for tests and diagnosis.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.lock().len()
    }

    /// Whether `session_id` has a turn in flight.
    #[must_use]
    pub fn is_running(&self, session_id: &str) -> bool {
        self.lock().contains_key(session_id)
    }

    /// Starts a turn for `session_id`, returning the turn number once the loop has been spawned.
    ///
    /// The user message is written before the loop starts, so a replay shows the prompt even if the
    /// model never answers. That ordering matches upstream: the prompt is durable, the response is
    /// not yet.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Busy`] when the session already has a turn in flight, and
    /// [`RuntimeError::Session`] when the prompt cannot be recorded.
    pub async fn prompt(
        this: &Arc<Self>,
        session_id: &str,
        text: &str,
        session: Arc<dyn Session>,
    ) -> Result<u64, RuntimeError> {
        // Taken and released before the await: the lock must not be held across a write, and the
        // check-then-insert must be atomic or two prompts could both see "idle".
        {
            let mut running = this.lock();
            if running.contains_key(session_id) {
                return Err(RuntimeError::Busy {
                    session: session_id.to_owned(),
                });
            }
            running.insert(
                session_id.to_owned(),
                Running {
                    cancel: CancellationToken::new(),
                },
            );
        }

        // The turn number counts *user messages*, not `TurnStart` records.
        //
        // Counting `TurnStart` looks equivalent and is not: the loop writes one per model round, so
        // a two-round turn leaves two, and the next prompt would skip ahead. What a client means by
        // "turn" is "which exchange is this", which is the number of prompts sent.
        let events = session.replay().await.unwrap_or_default();
        let turn = events
            .iter()
            .filter(|event| matches!(event, SessionEvent::UserMessage { .. }))
            .count() as u64
            + 1;

        let prompt = crate::message::Message::new(
            crate::brand::MessageId::new(format!("msg-{turn}")).unwrap_or_else(|_| {
                crate::brand::MessageId::new("msg-0").expect("constant is valid")
            }),
            crate::message::Role::User,
            crate::message::MessageSource::User { form: None },
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        );
        // Written before the loop is spawned, which means `user/message` precedes `turn/start` --
        // upstream opens the turn first. The order is chosen deliberately: a prompt that reached the
        // server must survive a crash before the model answered, and a `turn/start` with no message
        // would then be a turn that began for nothing. The deviation is recorded in
        // `docs/migration-v2.md` rather than left for a reader to notice.
        session
            .append(SessionEvent::UserMessage { message: prompt })
            .await?;

        let cancel = this
            .lock()
            .get(session_id)
            .map(|running| running.cancel.clone())
            .unwrap_or_default();

        let runtime = Arc::clone(this);
        let session_id = session_id.to_owned();
        let text = text.to_owned();
        tokio::spawn(async move {
            runtime.drive(&session_id, text, session, cancel).await;
            // Cleared however the turn ended, so a failed turn does not wedge the session forever.
            runtime.lock().remove(&session_id);
        });

        Ok(turn)
    }

    /// Cancels a running turn, returning whether one was running.
    pub fn cancel(&self, session_id: &str) -> bool {
        let running = self.lock();
        match running.get(session_id) {
            Some(running) => {
                running.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Runs the loop and publishes its steps.
    async fn drive(
        self: &Arc<Self>,
        session_id: &str,
        text: String,
        session: Arc<dyn Session>,
        cancel: CancellationToken,
    ) {
        let mut context = LoopContext::new(
            Arc::clone(&self.config.provider),
            Arc::clone(&self.config.tools),
            session,
            Arc::clone(&self.config.bus),
            self.config.model.clone(),
            // The session id was already validated when the session was created; an agent id is
            // built from it, so this cannot fail for a real session.
            AgentId::new(session_id).unwrap_or_else(|_| {
                AgentId::new("invalid-session-id").expect("a constant id is valid")
            }),
        );
        context.max_turns = self.config.max_turns;
        context.budget = self.config.token_budget;
        context.spill = self.spill.clone();
        context.spill_limit = self.spill_limit;
        // The loop's own goal, shared with the tool. Without this assignment the continuation logic below was dead
        // code: it read a field nothing ever set, so a created goal recorded an intent that never affected a turn.
        context.goal = self.goal.clone();
        context.compaction = self.compaction.clone();
        // The model's limits, when the catalogue states them. Without this the loop sends whatever the conversation has
        // accumulated and learns the answer from the provider's error -- which names a limit without saying how far past
        // it the request was.
        let (window, output) = self.model_limits();
        context = context.with_window(window, output);
        // The prompt was already written by `prompt()`, before the spawn.
        context.record_prompt = false;
        let context = Arc::new(context);

        let mut stream = Box::pin(crate::agent_loop::run(context, text, cancel));
        while let Some(step) = stream.next().await {
            match step {
                Ok(LoopStep::Chunk(StreamChunk::TextDelta { text, .. })) => {
                    self.publish(AgentEvent::TextDelta {
                        session_id: session_id.to_owned(),
                        text,
                    });
                }
                Ok(LoopStep::TurnStarted { turn }) => {
                    self.publish(AgentEvent::TurnStarted {
                        session_id: session_id.to_owned(),
                        turn,
                    });
                }
                Ok(LoopStep::ToolStarted { name, .. }) => {
                    self.publish(AgentEvent::ToolStarted {
                        session_id: session_id.to_owned(),
                        name,
                    });
                }
                Ok(LoopStep::ToolFinished { is_error, .. }) => {
                    self.publish(AgentEvent::ToolFinished {
                        session_id: session_id.to_owned(),
                        is_error,
                    });
                }
                Ok(LoopStep::TurnEnded { turn, reason }) => {
                    self.publish(AgentEvent::TurnEnded {
                        session_id: session_id.to_owned(),
                        turn,
                        reason,
                    });
                }
                // Every other chunk and the assembled message are internal bookkeeping; a client
                // receives text deltas, tool transitions, and the turn boundary.
                Ok(_) => {}
                Err(error) => {
                    // A failed turn still reports an end, or a client would wait forever for one.
                    self.publish(AgentEvent::TurnEnded {
                        session_id: session_id.to_owned(),
                        turn: 0,
                        reason: FinishReason::Error {
                            message: error.to_string(),
                        },
                    });
                    return;
                }
            }
        }
    }

    /// Publishes one event on the bus.
    /// The context window and output limit the configured model declares.
    ///
    /// `None` for either when the catalogue is silent, which is the honest answer: a budget check against an invented
    /// window would refuse requests the model would have accepted. A wrong window is worse than no check, because the
    /// check is the thing that is supposed to stop the request.
    fn model_limits(&self) -> (Option<u64>, Option<u32>) {
        // From the runtime rather than from a route table: the routes live in the CLI's configuration, and reaching back
        // into it from here would make the runtime depend on how one caller happens to build its provider.
        (self.context_window, self.max_output_tokens)
    }

    fn publish(&self, event: AgentEvent) {
        self.config.bus.emit::<AgentTurn>(event);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Running>> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for AgentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntime")
            .field("running", &self.running_count())
            .finish_non_exhaustive()
    }
}
