//! The agent: identity, model selection, and the lifecycle a session-backed run needs.
//!
//! Upstream's `agent` is 1.8k lines. The part that has real semantics is **model selection**, because
//! it has three sources that disagree and a precedence rule between them; the rest is lifecycle.
//!
//! # The precedence rule
//!
//! | source | when it wins |
//! |---|---|
//! | recorded choice | a caller set one for the next step |
//! | last request | the session already routed something |
//! | live options | nothing else names both halves |
//!
//! The order is not arbitrary. A recorded choice is an explicit instruction and must survive a
//! restart; the last request is evidence of what worked; the live options are a default that applies
//! only when the session has no history. Choosing live options first would silently undo a caller's
//! explicit switch on the next step.
//!
//! # Why a partial selection is not a selection
//!
//! A provider without a model, or a model without a provider, is not a choice: neither half can be
//! resolved alone, and treating them as fallbacks separately would pair a provider with a model that
//! another source named. Every candidate here is all-or-nothing.
//!
//! # Why "next" and "last used" are separate
//!
//! A UI shows both: what the next step will use, and what the last one did. Collapsing them makes a
//! switch invisible until the step runs, and makes a failed switch look like it took effect.

use std::sync::Arc;

use crate::brand::SessionId;

/// A complete provider, model, and optional reasoning effort.
///
/// Complete by construction: a struct with two required fields cannot represent "provider but no
/// model", which is the state that produces a request nobody can route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelection {
    /// The registered provider route.
    pub provider: String,
    /// The provider-owned model id.
    pub model: String,
    /// The adapter-owned reasoning effort, when one was chosen.
    pub reasoning_effort: Option<String>,
}

impl ModelSelection {
    /// Builds a selection.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning_effort: None,
        }
    }

    /// Adds a reasoning effort.
    #[must_use]
    pub fn with_reasoning(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }
}

/// The live options an agent was created with.
///
/// Both halves or neither: a `provider` without a `model` is not a usable default, and accepting it
/// would let the fallback chain produce an unroutable request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentOptions {
    /// The provider route.
    pub provider: Option<String>,
    /// The model id.
    pub model: Option<String>,
    /// The reasoning effort.
    pub reasoning_effort: Option<String>,
    /// The per-request output cap.
    pub max_tokens: Option<u64>,
}

impl AgentOptions {
    /// Builds options naming a route.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            model: Some(model.into()),
            reasoning_effort: None,
            max_tokens: None,
        }
    }

    /// Sets the output cap.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the reasoning effort.
    #[must_use]
    pub fn with_reasoning(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// The selection these options name, when they name both halves.
    ///
    /// `None` for a partial pair rather than a selection with an empty half: callers must not be able
    /// to build an unroutable request from a fallback.
    #[must_use]
    pub fn selection(&self) -> Option<ModelSelection> {
        match (&self.provider, &self.model) {
            (Some(provider), Some(model)) => Some(ModelSelection {
                provider: provider.clone(),
                model: model.clone(),
                reasoning_effort: self.reasoning_effort.clone(),
            }),
            _ => None,
        }
    }
}

/// What the model-selection state reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelSelectionView {
    /// The selection the next step will use, or `None` while the live options supply it.
    pub next: Option<ModelSelection>,
    /// The selection the latest recorded request used.
    pub last_used: Option<ModelSelection>,
}

/// Why a selection was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectionError {
    /// The route does not exist.
    #[error("no provider route named `{provider}`")]
    UnknownProvider {
        /// The route.
        provider: String,
    },

    /// The route exists but does not serve that model.
    #[error("route `{provider}` does not serve model `{model}`")]
    UnknownModel {
        /// The route.
        provider: String,
        /// The model.
        model: String,
    },
}

/// Where a selection came from, for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionSource {
    /// A caller recorded it for the next step.
    Recorded,
    /// The session's last routed request named it.
    LastUsed,
    /// The agent's live options named it.
    Options,
}

/// The model-selection state machine.
#[derive(Clone, Debug, Default)]
pub struct ModelSelectionState {
    recorded: Option<ModelSelection>,
    last_used: Option<ModelSelection>,
    options: AgentOptions,
}

impl ModelSelectionState {
    /// Creates a state from live options.
    #[must_use]
    pub fn new(options: AgentOptions) -> Self {
        Self {
            recorded: None,
            last_used: None,
            options,
        }
    }

    /// The client-visible view.
    #[must_use]
    pub fn view(&self) -> ModelSelectionView {
        ModelSelectionView {
            next: self.next(),
            last_used: self.last_used.clone(),
        }
    }

    /// The selection the next step will use.
    ///
    /// The precedence is recorded, then last used, then live options. Choosing live options first
    /// would silently undo a caller's explicit switch on the next step.
    #[must_use]
    pub fn next(&self) -> Option<ModelSelection> {
        self.current().0
    }

    /// The selection and its source.
    #[must_use]
    pub fn current(&self) -> (Option<ModelSelection>, Option<SelectionSource>) {
        if let Some(recorded) = &self.recorded {
            return (Some(recorded.clone()), Some(SelectionSource::Recorded));
        }
        if let Some(last) = &self.last_used {
            return (Some(last.clone()), Some(SelectionSource::LastUsed));
        }
        match self.options.selection() {
            Some(selection) => (Some(selection), Some(SelectionSource::Options)),
            None => (None, None),
        }
    }

    /// Records a selection for the next step.
    ///
    /// Recording an equal selection is a no-op, so a UI that re-sends its current choice does not
    /// write a log entry per click.
    ///
    /// # Errors
    ///
    /// Never in this implementation, but the signature leaves room for a route check: a selection
    /// naming a route that is not mounted is a caller bug worth reporting where it happens rather
    /// than at request time.
    pub fn record(&mut self, selection: ModelSelection) -> Result<bool, SelectionError> {
        if self.recorded.as_ref() == Some(&selection) {
            return Ok(false);
        }
        self.recorded = Some(selection);
        Ok(true)
    }

    /// Clears a recorded selection, falling back to the last request.
    ///
    /// Returns whether anything was cleared, so a caller can avoid a no-op log entry.
    pub fn clear_recorded(&mut self) -> bool {
        self.recorded.take().is_some()
    }

    /// Records the selection a request actually used.
    pub fn note_used(&mut self, selection: ModelSelection) {
        self.last_used = Some(selection);
    }

    /// Replaces the live options.
    pub fn set_options(&mut self, options: AgentOptions) {
        self.options = options;
    }

    /// The live options.
    #[must_use]
    pub fn options(&self) -> &AgentOptions {
        &self.options
    }
}

/// Validates a selection against a route registry.
///
/// A free function rather than a method on the state: the state must stay usable without a registry
/// (a resumed session may not have its providers mounted yet), while a caller that *does* have one
/// can check before recording.
///
/// # Errors
///
/// Returns [`SelectionError::UnknownProvider`] or [`SelectionError::UnknownModel`].
pub fn validate(
    selection: &ModelSelection,
    registry: &crate::providers::RouteRegistry,
) -> Result<(), SelectionError> {
    let route = registry
        .get(&selection.provider)
        .map_err(|_| SelectionError::UnknownProvider {
            provider: selection.provider.clone(),
        })?;
    if route.model(&selection.model).is_none() {
        return Err(SelectionError::UnknownModel {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
        });
    }
    Ok(())
}

/// What an agent is created with.
#[derive(Clone, Debug)]
pub struct AgentSpec {
    /// Its identity, which is also its session id.
    pub session_id: SessionId,
    /// Its options.
    pub options: AgentOptions,
    /// The working directory it operates in.
    pub cwd: Option<std::path::PathBuf>,
    /// The delegation depth, for a subagent.
    pub depth: crate::subagent::Depth,
}

impl AgentSpec {
    /// Builds a root agent's spec.
    #[must_use]
    pub fn root(session_id: SessionId, options: AgentOptions) -> Self {
        Self {
            session_id,
            options,
            cwd: None,
            depth: crate::subagent::Depth::root(),
        }
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the delegation depth.
    #[must_use]
    pub fn with_depth(mut self, depth: crate::subagent::Depth) -> Self {
        self.depth = depth;
        self
    }
}

/// The agent handle: identity and mutable state behind one lock.
///
/// A `Mutex` rather than `&mut self` because the loop, a UI, and a model selector all need to read
/// and write the selection, and they are not one task.
pub struct Agent {
    spec: AgentSpec,
    selection: std::sync::Mutex<ModelSelectionState>,
    inbox: std::sync::Mutex<crate::inbox::Inbox>,
    cancel: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("session_id", &self.spec.session_id)
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Creates an agent.
    #[must_use]
    pub fn new(spec: AgentSpec) -> Self {
        let selection = ModelSelectionState::new(spec.options.clone());
        Self {
            spec,
            selection: std::sync::Mutex::new(selection),
            inbox: std::sync::Mutex::new(crate::inbox::Inbox::new()),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Its identity.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.spec.session_id
    }

    /// Its working directory.
    #[must_use]
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.spec.cwd.as_deref()
    }

    /// Its delegation depth.
    #[must_use]
    pub fn depth(&self) -> crate::subagent::Depth {
        self.spec.depth
    }

    /// The token that cancels its current turn.
    #[must_use]
    pub fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.clone()
    }

    /// The model selection view.
    #[must_use]
    pub fn model_view(&self) -> ModelSelectionView {
        self.lock_selection().view()
    }

    /// The selection for the next step.
    #[must_use]
    pub fn next_model(&self) -> Option<ModelSelection> {
        self.lock_selection().next()
    }

    /// Records a selection for the next step.
    ///
    /// # Errors
    ///
    /// See [`ModelSelectionState::record`].
    pub fn select_model(&self, selection: ModelSelection) -> Result<bool, SelectionError> {
        self.lock_selection().record(selection)
    }

    /// Notes the selection a request used.
    pub fn note_model_used(&self, selection: ModelSelection) {
        self.lock_selection().note_used(selection);
    }

    /// Applies a splice to the inbox.
    ///
    /// # Errors
    ///
    /// See [`crate::inbox::Inbox::apply`].
    pub fn splice_inbox(
        &self,
        splice: &crate::inbox::Splice,
    ) -> Result<crate::inbox::Inbox, crate::inbox::InboxError> {
        self.lock_inbox().apply(splice)
    }

    /// The pending inbox state, as a detached copy.
    #[must_use]
    pub fn inbox_state(&self) -> crate::inbox::Inbox {
        self.lock_inbox().clone()
    }

    /// Takes the queued next-turn messages, emptying that queue.
    ///
    /// A mutate-in-place operation rather than `inbox_state().take_turn()`: the latter drains a
    /// detached copy and leaves the agent's queue untouched, so a loop that used it would read the
    /// same messages on every turn and never make progress.
    pub fn take_queued_turn(&self) -> Vec<crate::message::Message> {
        self.lock_inbox().take_turn()
    }

    /// Takes the queued next-step messages, emptying that queue.
    pub fn take_queued_step(&self) -> Vec<crate::message::Message> {
        self.lock_inbox().take_step()
    }

    /// Cancels the current turn.
    ///
    /// `keep_inbox` decides whether queued input survives. Both behaviours are needed: a user
    /// interrupt usually wants to keep what they typed, while shutting down does not.
    pub fn cancel(&self, keep_inbox: bool) {
        self.cancel.cancel();
        if !keep_inbox {
            let mut inbox = self.lock_inbox();
            let _ = inbox.take_turn();
            let _ = inbox.take_step();
        }
    }

    /// Replaces the cancel token, for a new turn after a cancellation.
    ///
    /// A `CancellationToken` cannot be un-cancelled, so a fresh turn needs a fresh token. Making this
    /// explicit rather than hiding it in `cancel` keeps the state visible: a caller that forgets to
    /// reset starts every turn already cancelled.
    pub fn reset_cancel(&mut self) {
        self.cancel = tokio_util::sync::CancellationToken::new();
    }

    fn lock_selection(&self) -> std::sync::MutexGuard<'_, ModelSelectionState> {
        self.selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_inbox(&self) -> std::sync::MutexGuard<'_, crate::inbox::Inbox> {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Shared agent handle.
pub type AgentHandle = Arc<Agent>;
