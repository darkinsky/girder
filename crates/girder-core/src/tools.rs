//! The tools seam: definitions, the three-waterfall pipeline, and one real tool.
//!
//! Upstream's tool executor is a three-stage waterfall around a dispatch:
//!
//! 1. `tools/pre-execute` — allow, deny, cancel, or ask. This is where approvals and policy live.
//! 2. `tools/execute` — around the call itself. Timeouts, retries, and metrics wrap here.
//! 3. `tools/post-execute` — accept, replace, or block the result.
//!
//! Each stage is a [`Waterfall`], so a listener that does not call `next` vetoes the rest. That is
//! how a deny decision stops the tool body: the rejecting layer never delegates.
//!
//! # Why the pipeline is three waterfalls and not one
//!
//! A single `tools/execute` waterfall could carry everything, but the three stages observe different
//! values: pre-execute sees only the call, execute sees the call and owns the signal, post-execute
//! sees the normalized result. Collapsing them would force every listener to receive a value it does
//! not need and to distinguish "am I before or after" from a field. Upstream split them for the same
//! reason, and the split survives the port because it is about *what is known when*, not about
//! JavaScript.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::observation::{Conflict, Observation, Session as ObservationSession};
use crate::permissions::State as PermissionState;

use crate::brand::{AgentId, ToolCallId};
use crate::llm::{ContentBlock, ToolSchema};
use crate::scope::BoxFut;

/// What a tool is about to be asked to do.
#[derive(Clone, Debug, Serialize)]
pub struct ToolExecution {
    /// Provider-issued call id.
    pub call_id: ToolCallId,
    /// Tool name.
    pub name: String,
    /// Raw JSON arguments, exactly as the model produced them.
    pub arguments: String,
    /// Which agent requested the call.
    pub agent_id: AgentId,
}

/// The context handed to a tool body.
#[derive(Clone, Debug)]
pub struct ToolRunContext {
    /// The call being run.
    pub execution: ToolExecution,
    /// Cancellation. A tool that does work must forward this; a tool that ignores it cannot be
    /// cancelled and will be reported as such.
    pub cancel: CancellationToken,
    /// The permission state a tool consults before acting.
    ///
    /// `None` means the caller mounted no permission layer, and a tool that writes must decide for itself what that
    /// means. It is not a default that quietly permits: a tool that needs the state and finds none refuses, because a
    /// write that proceeded because nobody configured a policy is the failure mode the policy exists to prevent.
    pub permissions: Option<Arc<PermissionState>>,
    /// Where the session's workspace is, for a tool that must decide whether a path is inside it.
    pub workspace: Option<std::path::PathBuf>,
    /// What this session has seen, so a write does not clobber a change nobody looked at.
    pub observations: Option<Arc<ObservationSession>>,
}

impl Default for ToolRunContext {
    /// A context with nothing attached.
    ///
    /// The cancellation token is a fresh one, which is never cancelled. That is right for a test and wrong for a
    /// turn, which builds its context with [`ToolRunContext::new`] so a cancel actually reaches the tool.
    fn default() -> Self {
        Self {
            execution: ToolExecution {
                call_id: ToolCallId::new("default").expect("a literal id is valid"),
                name: String::new(),
                arguments: "{}".to_owned(),
                agent_id: AgentId::new("default").expect("a literal id is valid"),
            },
            cancel: CancellationToken::new(),
            permissions: None,
            workspace: None,
            observations: None,
        }
    }
}

impl ToolRunContext {
    /// Builds a context with only what the turn provides.
    #[must_use]
    pub fn new(execution: ToolExecution, cancel: CancellationToken) -> Self {
        Self {
            execution,
            cancel,
            permissions: None,
            workspace: None,
            observations: None,
        }
    }

    /// Attaches a permission state.
    #[must_use]
    pub fn with_permissions(mut self, permissions: Arc<PermissionState>) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// Attaches a workspace root.
    #[must_use]
    pub fn in_workspace(mut self, workspace: impl Into<std::path::PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Records what this session has now seen of a path.
    ///
    /// A no-op without an observation session: a deployment that mounted no observation layer has nothing to
    /// contradict, and a tool must not refuse to work because a policy is absent.
    pub fn observe(&self, path: &std::path::Path, observation: Observation) {
        if let Some(session) = &self.observations {
            session.observe(path, observation);
        }
    }

    /// Forgets a path, after a successful write or a delete.
    pub fn forget(&self, path: &std::path::Path) {
        if let Some(session) = &self.observations {
            session.forget(path);
        }
    }

    /// Forgets every observation, returning how many were dropped.
    ///
    /// The answer to a tool whose effect on the filesystem cannot be known in advance — a shell command. A no-op
    /// without an observation session, and it returns zero rather than failing, so a caller can report the count
    /// without first asking whether the layer is mounted.
    pub fn forget_all(&self) -> usize {
        self.observations
            .as_ref()
            .map_or(0, |session| session.forget_all())
    }

    /// Whether a write to `path` would clobber a change this session did not see.
    ///
    /// # Errors
    ///
    /// Returns the conflict, which the caller reports rather than swallowing: a write refused without a reason is a
    /// tool that appears broken.
    pub fn check_write(&self, path: &std::path::Path) -> Result<(), Conflict> {
        let Some(session) = &self.observations else {
            return Ok(());
        };
        match session.observed(path) {
            Some(Observation::Present(expected)) => {
                // The file on disk is what a write would replace, so the check compares against it rather than
                // against a cached copy.
                let Ok(text) = std::fs::read_to_string(path) else {
                    return Err(Conflict::Deleted);
                };
                // Both sides are built by `version_of` on purpose. It stamps the current time while a read recorded
                // the text it saw, so comparing them on the cheap fields would report every unmodified file as
                // changed. The hash is the part that identifies content, and `matches` uses it whenever both sides
                // carry one.
                let current = crate::observation::version_of(&text);
                if expected.matches(&current) {
                    Ok(())
                } else {
                    Err(Conflict::Changed {
                        observed: expected,
                        current,
                    })
                }
            }
            Some(Observation::Absent) => {
                if path.exists() {
                    Err(Conflict::Created)
                } else {
                    Ok(())
                }
            }
            None => Ok(()),
        }
    }
}

/// A tool's canonical result value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolValue {
    /// A JSON value.
    Json(serde_json::Value),
    /// Plain text, wrapped as JSON so the log stays lossless.
    Text(String),
}

impl ToolValue {
    /// Renders the value for the model.
    #[must_use]
    pub fn to_content(&self) -> Vec<ContentBlock> {
        let text = match self {
            Self::Text(text) => text.clone(),
            Self::Json(value) => value.to_string(),
        };
        vec![ContentBlock::Text { text }]
    }
}

/// A normalized tool outcome.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolResult {
    /// What the tool produced, for the model.
    pub content: Vec<ContentBlock>,
    /// The canonical value, when the tool returned structured data.
    pub value: Option<ToolValue>,
    /// Whether this is a failure the model should see and react to.
    pub is_error: bool,
}

impl ToolResult {
    /// A successful text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text { text: text.into() }],
            value: None,
            is_error: false,
        }
    }

    /// A successful structured result.
    #[must_use]
    pub fn json(value: serde_json::Value) -> Self {
        let content = vec![ContentBlock::Text {
            text: value.to_string(),
        }];
        Self {
            content,
            value: Some(ToolValue::Json(value)),
            is_error: false,
        }
    }

    /// A failure the model should see.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text {
                text: message.into(),
            }],
            value: None,
            is_error: true,
        }
    }
}

/// The pre-dispatch decision.
///
/// Upstream distinguishes `deny` from `cancel` because a policy denial shows a reason while a
/// cancellation shows the canonical cancellation result. Keeping them separate preserves that.
#[derive(Clone, Debug, PartialEq)]
pub enum PreToolDecision {
    /// Run the call.
    Allow,
    /// Refuse it with a model-facing reason.
    Deny {
        /// Shown to the model.
        reason: String,
    },
    /// Refuse it as a cancellation rather than a policy decision.
    Cancel,
    /// Stop and ask a person before deciding.
    ///
    /// A distinct variant rather than a `Deny` with a different reason: a denial is final for this turn and a person's
    /// answer is not, and the loop has to be able to tell them apart to know whether to wait.
    Ask {
        /// Shown to the person deciding.
        question: String,
    },
}

/// The post-dispatch decision.
#[derive(Clone, Debug, PartialEq)]
pub enum PostToolDecision {
    /// Keep the result.
    Accept,
    /// Replace the result's content.
    Replace {
        /// The replacement content.
        content: Vec<ContentBlock>,
    },
    /// Turn the result into an error carrying corrective feedback.
    Block {
        /// Feedback the model should act on.
        feedback: Vec<ContentBlock>,
    },
}

/// The body of a tool.
///
/// `Arc<dyn Fn>` rather than `async fn` in a trait: the registry stores tools of different concrete
/// types behind one name, so the callable must be object-safe. That is migration-plan §1.3
/// constraint one, and it is why every tool pays one `Box` allocation per call.
pub type ToolBody = Arc<
    dyn Fn(serde_json::Value, ToolRunContext) -> BoxFut<Result<ToolResult, ToolError>>
        + Send
        + Sync,
>;

/// One registered tool.
#[derive(Clone)]
pub struct ToolDefinition {
    /// Name the model calls.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
    /// The implementation.
    pub execute: ToolBody,
}

impl std::fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ToolDefinition {
    /// Builds a tool definition.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        execute: ToolBody,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            execute,
        }
    }

    /// The schema the model sees.
    #[must_use]
    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

/// A failure raised by a tool body.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The arguments did not match the tool's schema.
    #[error("invalid arguments for `{tool}`: {message}")]
    InvalidArguments {
        /// Tool name.
        tool: String,
        /// What was wrong.
        message: String,
    },

    /// The tool failed while running.
    #[error("`{tool}` failed: {message}")]
    Failed {
        /// Tool name.
        tool: String,
        /// What went wrong.
        message: String,
    },

    /// The caller cancelled.
    #[error("`{tool}` cancelled")]
    Cancelled {
        /// Tool name.
        tool: String,
    },
}

/// The registry of available tools.
///
/// A `BTreeMap` so `schemas()` is deterministic: two runs with the same tools must send the model
/// the same schema order, or prompt caching and snapshot tests both become flaky.
#[derive(Default)]
pub struct ToolRegistry {
    tools: std::sync::Mutex<BTreeMap<String, Arc<ToolDefinition>>>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool, replacing any previous one with the same name.
    ///
    /// Takes `&self` rather than `&mut self` because a tool may need a reference to the loop that
    /// owns the registry — the subagent tool holds the context it delegates from. Interior
    /// mutability breaks that cycle, at the cost of a lock per registration (not per lookup).
    pub fn register(&self, tool: ToolDefinition) {
        self.lock().insert(tool.name.clone(), Arc::new(tool));
    }

    /// Looks up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<ToolDefinition>> {
        self.lock().get(name).cloned()
    }

    /// Every tool's schema, in name order.
    #[must_use]
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.lock().values().map(|tool| tool.schema()).collect()
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Registered tool names, in order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<ToolDefinition>>> {
        self.tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

/// The registry key for the tool registry.
pub struct ToolsKey;

impl crate::ServiceKey for ToolsKey {
    type Service = dyn ToolRegistryView;
    const ID: &'static str = "tools";
}

/// The read-only view of the registry that consumers resolve.
///
/// Split from [`ToolRegistry`] so a consumer cannot register a tool by resolving the service: the
/// registry is mutated only by the loader, and a `dyn` view makes that a type-level guarantee
/// rather than a convention.
pub trait ToolRegistryView: Send + Sync {
    /// Looks up a tool by name.
    fn get(&self, name: &str) -> Option<Arc<ToolDefinition>>;
    /// Every tool's schema, in name order.
    fn schemas(&self) -> Vec<ToolSchema>;
}

impl ToolRegistryView for ToolRegistry {
    fn get(&self, name: &str) -> Option<Arc<ToolDefinition>> {
        Self::get(self, name)
    }

    fn schemas(&self) -> Vec<ToolSchema> {
        Self::schemas(self)
    }
}

// ---- the three waterfall events ----------------------------------------------------------------

/// Pre-dispatch: allow, deny, cancel.
pub struct ToolPreExecute;

impl crate::Waterfall for ToolPreExecute {
    const NAME: &'static str = "tools/pre-execute";
    type Args = ToolExecution;
    type Out = PreToolDecision;
}

/// Around-dispatch: wrap the call.
pub struct ToolExecute;

impl crate::Waterfall for ToolExecute {
    const NAME: &'static str = "tools/execute";
    type Args = ToolExecution;
    type Out = ToolResult;
}

/// Post-dispatch: accept, replace, block.
pub struct ToolPostExecute;

impl crate::Waterfall for ToolPostExecute {
    const NAME: &'static str = "tools/post-execute";
    type Args = PostExecuteArgs;
    type Out = PostToolDecision;
}

/// The value `tools/post-execute` observes: the call and its normalized result.
#[derive(Clone, Debug, Serialize)]
pub struct PostExecuteArgs {
    /// The call that ran.
    pub execution: ToolExecution,
    /// What it produced.
    pub result: ToolResult,
}
