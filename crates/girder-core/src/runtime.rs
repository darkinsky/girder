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
}

/// A running turn.
struct Running {
    cancel: CancellationToken,
}

/// Runs prompts against sessions.
pub struct AgentRuntime {
    config: RuntimeConfig,
    /// One running turn per session id.
    running: Mutex<BTreeMap<String, Running>>,
}

impl AgentRuntime {
    /// Builds a runtime.
    #[must_use]
    pub fn new(config: RuntimeConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            running: Mutex::new(BTreeMap::new()),
        })
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
