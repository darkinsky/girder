//! Subagent delegation: spawning a child agent with its own session, and the depth limit.
//!
//! Upstream's `packages/subagent` is 9,575 lines across eleven packages, because it supports three
//! external CLI drivers (claude-code, codex, acp) plus in-process fork and spawn. This module carries
//! only the in-process path — a child that runs the same loop with its own session and a trimmed tool
//! set — because that is the part that can be verified without those CLIs installed. See
//! `docs/phase7-scope.md`.
//!
//! # The depth limit is the safety property
//!
//! A `subagent` tool that takes a prompt and runs an agent which itself has the `subagent` tool is an
//! unbounded recursion: each level spends tokens, and a model that keeps delegating spends them until
//! something external stops it. So depth is a hard budget checked *before* spawning, not a heuristic
//! the child is trusted to respect.
//!
//! Upstream keeps this in `depth.ts`; the shape is the same: a `Depth` value that a child inherits as
//! one greater than its parent, with a configured ceiling.
//!
//! # Why the child gets a trimmed tool set
//!
//! A child with the parent's full tool set can re-delegate, so the depth limit is the only thing
//! stopping runaway fan-out. Removing `subagent` from a child at the depth ceiling makes the limit
//! structural rather than advisory — the model cannot ask for what is not offered.

use std::sync::Arc;

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{LoopContext, LoopError, LoopStep};
use crate::brand::AgentId;
use crate::session::{MemorySession, Session};
use crate::tools::{ToolDefinition, ToolRegistryView, ToolResult};

/// How far a delegation is from the root agent.
///
/// `root` is depth 0. Each delegation produces a child at `parent + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Depth(u32);

impl Depth {
    /// The root agent's depth.
    #[must_use]
    pub fn root() -> Self {
        Self(0)
    }

    /// The value as an integer.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }

    /// The depth of a delegation made by this agent.
    #[must_use]
    pub fn child(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Builds a depth from a raw value.
    ///
    /// Used by tests that need to reach a specific level, and by a caller restoring depth from a
    /// persisted record.
    #[must_use]
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// Whether a delegation at this depth is allowed under `max`.
    ///
    /// The check is `self < max`, so a `max` of 2 allows depths 0 and 1 to delegate, and a delegation
    /// at depth 2 is refused. That is the ceiling being a count of *nested delegations*, which is
    /// what an operator setting it expects.
    #[must_use]
    pub fn can_delegate(self, max: u32) -> bool {
        self.0 < max
    }
}

/// A failure raised while delegating.
#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    /// The depth ceiling was reached.
    #[error("delegation refused: depth {depth} reached the limit of {max}")]
    DepthExceeded {
        /// This agent's depth.
        depth: u32,
        /// The configured ceiling.
        max: u32,
    },

    /// The child loop failed.
    #[error("child agent failed: {0}")]
    Child(#[from] LoopError),

    /// The caller cancelled.
    #[error("delegation cancelled")]
    Cancelled,
}

/// What a delegation returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationResult {
    /// The child's final visible text.
    pub text: String,
    /// How many turns the child ran.
    pub turns: u32,
    /// The child agent's identity.
    pub child_id: AgentId,
}

impl DelegationResult {
    /// Renders the result as the tool content the parent's model sees.
    #[must_use]
    pub fn to_tool_result(&self) -> ToolResult {
        if self.text.trim().is_empty() {
            return ToolResult::error(format!(
                "the subagent finished after {} turn(s) without an answer",
                self.turns
            ));
        }
        ToolResult::text(self.text.clone())
    }
}

/// Runs one child agent to completion.
///
/// The child gets a fresh [`MemorySession`]: a subagent's transcript must not be mixed into the
/// parent's durable log, or replaying the parent would show turns it never took.
pub async fn delegate(
    context: &Arc<LoopContext>,
    prompt: String,
    depth: Depth,
    max_depth: u32,
    cancel: CancellationToken,
) -> Result<DelegationResult, SubagentError> {
    // Checked before anything is spent. A refusal must not cost a model call.
    if !depth.can_delegate(max_depth) {
        return Err(SubagentError::DepthExceeded {
            depth: depth.get(),
            max: max_depth,
        });
    }

    let child_depth = depth.child();
    // A child id is the parent id plus the depth, which is well formed whenever the parent is.
    let child_id = AgentId::new(format!(
        "{}/{}",
        context.agent_id.as_str(),
        child_depth.get()
    ))
    .map_err(|error| {
        SubagentError::Child(LoopError::Session(
            crate::session::SessionError::Compression(error.to_string()),
        ))
    })?;

    // The child's context: same provider and bus, its own session, and the depth it must respect.
    let child_session: Arc<dyn Session> = Arc::new(MemorySession::new());
    let mut child = LoopContext::new(
        Arc::clone(&context.provider),
        Arc::clone(&context.tools),
        child_session,
        Arc::clone(&context.bus),
        context.model.clone(),
        child_id.clone(),
    );
    child.max_turns = context.max_turns;
    child.depth = child_depth;
    child.max_depth = max_depth;
    let child = Arc::new(child);

    let mut stream = Box::pin(crate::agent_loop::run(child, prompt, cancel.clone()));
    let mut text = String::new();
    let mut turns = 0;

    while let Some(step) = stream.next().await {
        match step {
            Ok(LoopStep::Chunk(crate::llm::StreamChunk::TextDelta { text: delta, .. })) => {
                text.push_str(&delta);
            }
            Ok(LoopStep::TurnStarted { turn }) => {
                turns = u32::try_from(turn).unwrap_or(u32::MAX);
            }
            Ok(LoopStep::AssistantMessage(message)) => {
                // Each turn's visible text replaces the accumulation, so the answer is the *last*
                // turn's text rather than every turn concatenated.
                let turn_text = message.text();
                if !turn_text.is_empty() {
                    text = turn_text;
                }
            }
            Ok(_) => {}
            Err(LoopError::Cancelled) => return Err(SubagentError::Cancelled),
            Err(error) => return Err(SubagentError::Child(error)),
        }
    }

    Ok(DelegationResult {
        text,
        turns,
        child_id,
    })
}

/// Builds the `subagent` tool.
///
/// The tool configures delegation but does not decide whether it is allowed: the depth comes from
/// the calling context, so a model cannot widen its own budget by passing an argument.
pub fn subagent_tool(context: Arc<LoopContext>, max_depth: u32) -> ToolDefinition {
    ToolDefinition::new(
        "subagent",
        "Delegates a self-contained task to a child agent and returns its answer",
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "A complete, self-contained task description"
                }
            },
            "required": ["prompt"]
        }),
        Arc::new(
            move |args: serde_json::Value, ctx: crate::tools::ToolRunContext| {
                let context = Arc::clone(&context);
                Box::pin(async move {
                    let Some(prompt) = args
                        .get("prompt")
                        .and_then(serde_json::Value::as_str)
                        .filter(|prompt| !prompt.trim().is_empty())
                    else {
                        return Ok(ToolResult::error(
                            "`prompt` is required and must not be empty",
                        ));
                    };

                    // Depth comes from the *caller's* context, never from the arguments.
                    match delegate(
                        &context,
                        prompt.to_owned(),
                        context.depth,
                        max_depth,
                        ctx.cancel,
                    )
                    .await
                    {
                        Ok(result) => Ok(result.to_tool_result()),
                        Err(SubagentError::DepthExceeded { depth, max }) => {
                            Ok(ToolResult::error(format!(
                                "delegation refused: this agent is at depth {depth}, and the limit is {max}. \
                         Do this work yourself instead of delegating."
                            )))
                        }
                        Err(SubagentError::Cancelled) => {
                            Ok(ToolResult::error("delegation cancelled"))
                        }
                        Err(SubagentError::Child(error)) => {
                            Ok(ToolResult::error(format!("the subagent failed: {error}")))
                        }
                    }
                })
            },
        ),
    )
}

/// A tool registry view that hides `subagent` once the depth ceiling is reached.
///
/// Structural rather than advisory: at the ceiling the model is not offered the tool at all, so it
/// cannot call it by guessing the name either — the executor would report the tool as unknown.
pub struct DepthLimitedTools {
    inner: Arc<dyn ToolRegistryView>,
    depth: Depth,
    max_depth: u32,
    /// Name of the delegation tool to hide at the ceiling.
    subagent_name: String,
}

impl DepthLimitedTools {
    /// Wraps a registry, hiding delegation tools at the ceiling.
    #[must_use]
    pub fn new(
        inner: Arc<dyn ToolRegistryView>,
        depth: Depth,
        max_depth: u32,
        subagent_name: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            depth,
            max_depth,
            subagent_name: subagent_name.into(),
        }
    }

    fn hides_delegation(&self) -> bool {
        !self.depth.can_delegate(self.max_depth)
    }
}

impl ToolRegistryView for DepthLimitedTools {
    fn get(&self, name: &str) -> Option<Arc<ToolDefinition>> {
        if self.hides_delegation() && name == self.subagent_name {
            return None;
        }
        self.inner.get(name)
    }

    fn schemas(&self) -> Vec<crate::llm::ToolSchema> {
        self.inner
            .schemas()
            .into_iter()
            .filter(|schema| !(self.hides_delegation() && schema.name == self.subagent_name))
            .collect()
    }
}

/// A delegation that never leaves the process, for tests and for a caller that wants no model at all.
///
/// The child's answer is supplied by a closure, which is what makes the depth logic testable without
/// a provider.
pub struct ScriptedDelegator {
    /// Answers, one per delegation, in order.
    answers: std::sync::Mutex<Vec<String>>,
}

impl ScriptedDelegator {
    /// Creates a delegator that answers with `answers` in order.
    #[must_use]
    pub fn new(answers: Vec<String>) -> Self {
        Self {
            answers: std::sync::Mutex::new(answers),
        }
    }
}

impl std::fmt::Debug for ScriptedDelegator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedDelegator").finish_non_exhaustive()
    }
}

impl ScriptedDelegator {
    /// Answers one delegation.
    ///
    /// # Errors
    ///
    /// Returns [`SubagentError::Cancelled`] when `cancel` is already set, and fails when the script
    /// is exhausted.
    pub fn answer(&self, cancel: &CancellationToken) -> Result<DelegationResult, SubagentError> {
        if cancel.is_cancelled() {
            return Err(SubagentError::Cancelled);
        }
        let mut answers = self
            .answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if answers.is_empty() {
            return Err(SubagentError::Child(LoopError::Cancelled));
        }
        let text = answers.remove(0);
        Ok(DelegationResult {
            text,
            turns: 1,
            child_id: AgentId::new("scripted-child").expect("constant is valid"),
        })
    }
}
