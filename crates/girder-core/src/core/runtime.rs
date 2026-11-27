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

use crate::core::agent::{Agent, AgentOptions, AgentSpec};
use crate::core::agent_loop::{LoopContext, LoopStep};
use crate::core::events::{Event, EventBus};
use crate::core::tools::ToolRegistryView;
use crate::llm::{ContentBlock, FinishReason, LlmProvider, StreamChunk};
use crate::session::message::{Message, MessageSource, Role};
use crate::session::{Session, SessionEvent};
use crate::util::brand::{AgentId, MessageId, SessionId};

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
    pub token_budget: crate::util::limits::Ceiling,
}

fn user_prompt(turn: u64, text: &str) -> Message {
    prompt_message(&format!("msg-{turn}"), text)
}

fn queued_prompt(text: &str) -> Message {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    prompt_message(&format!("msg-q-{n}"), text)
}

fn prompt_message(id: &str, text: &str) -> Message {
    Message::new(
        MessageId::new(id)
            .unwrap_or_else(|_| MessageId::new("msg-0").expect("a constant id is valid")),
        Role::User,
        MessageSource::User { form: None },
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    )
}

/// A running turn.
struct Running {
    cancel: CancellationToken,
    /// The inbox a later `queue` or `steer` prompt writes while this turn is in flight.
    agent: std::sync::Arc<Agent>,
    /// The turn number already given to the client.
    turn: u64,
}

/// Runs prompts against sessions.
pub struct AgentRuntime {
    /// The model's context window, when the deployment states one.
    context_window: Option<u64>,
    /// The output limit to ask for, which is subtracted from the window before the input is measured.
    max_output_tokens: Option<u32>,
    /// Whether the route accepts image input, read from its declared modalities.
    accepts_images: bool,
    config: RuntimeConfig,
    /// Sessions whose tool execution is waiting for an approval decision.
    pending_approvals: Arc<Mutex<std::collections::BTreeSet<String>>>,
    /// Where an oversized tool result is saved, when this deployment has storage for it.
    spill: Option<Arc<dyn crate::spill::Store>>,
    /// How many bytes a tool result may carry before it is spilled.
    spill_limit: usize,
    /// How the history is compacted, when a deployment configured it.
    compaction: Option<crate::compaction::hook::CompactionPolicy>,
    /// Who answers an `Ask`, when this deployment has a person to ask.
    approver: Option<Arc<dyn crate::interaction::approval::Approver>>,
    /// The sandbox knobs copied onto every tool call this runtime starts.
    permissions: Option<Arc<crate::credentials::permissions::State>>,
    /// Which calls wait for that person. `None` allows every call.
    approval_rules: Option<Arc<crate::interaction::approval_rules::Rules>>,
    /// The session directory copied onto every tool call this runtime starts.
    working_directory: Option<Arc<tokio::sync::Mutex<crate::shell::working_dir::WorkingDirectory>>>,
    /// Where `tools.maxCalls` is read, when this deployment stores settings.
    tool_settings: Option<Arc<dyn crate::SettingsStore>>,
    /// Whether the opening message is sent to the model for a title.
    refine_titles: bool,
    /// Where `exit_plan_mode` records the session it is reviewing.
    plan_sessions: Option<Arc<crate::core::plan_mode::PlanSessions>>,
    /// Where the team task tools record the session they are reviewing.
    team_sessions: Option<Arc<crate::team::tools::TeamSessions>>,
    /// Turns currently inside the loop, so a tool registered once can find the caller.
    live_parents: Option<Arc<crate::core::agent_loop::LiveParents>>,
    /// How many nested delegations a turn started here may make.
    max_depth: u32,
    /// The goal the loop advances, when one is set.
    ///
    /// Shared with the `goal` tool rather than owned: a tool that kept its own copy would let the model complete a
    /// goal the loop still believed was open, and the loop would keep continuing toward something already done.
    goal: Option<Arc<std::sync::Mutex<crate::goal::GoalState>>>,
    /// One running turn per session id.
    running: Mutex<BTreeMap<String, Running>>,
    /// Wakes `observeState` and `session.wait` after a turn, approval, or inbox change.
    activity: std::sync::Arc<tokio::sync::Notify>,
    /// Live assistant frames `session.follow` forwards while a model step is open.
    assistant: std::sync::Arc<crate::core::assistant_live::AssistantHub>,
}

/// How many read results one conversation remembers.
///
/// Small on purpose: the cache exists to stop a model re-reading the same file twice in a turn, not to be a store. Each
/// entry holds a whole file's text, and the point at which remembering more costs more than the reads it saves is well
/// below a hundred.
const CACHE_CAPACITY: usize = 32;

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
        compaction: Option<crate::compaction::hook::CompactionPolicy>,
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
        compaction: Option<crate::compaction::hook::CompactionPolicy>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            running: Mutex::new(BTreeMap::new()),
            activity: std::sync::Arc::new(tokio::sync::Notify::new()),
            pending_approvals: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
            spill,
            spill_limit,
            goal,
            compaction,
            approver: None,
            permissions: None,
            approval_rules: None,
            working_directory: None,
            tool_settings: None,
            refine_titles: false,
            plan_sessions: None,
            team_sessions: None,
            live_parents: None,
            max_depth: 2,
            // Unset until a deployment states them: the runtime does not know which model it is talking to beyond the
            // name, and a guessed window would make the budget check refuse requests the model would accept.
            context_window: None,
            max_output_tokens: None,
            accepts_images: false,
            assistant: std::sync::Arc::new(crate::core::assistant_live::AssistantHub::new()),
        })
    }

    /// The assistant frames a follow connection reads while this runtime is generating.
    #[must_use]
    pub fn assistant(&self) -> &std::sync::Arc<crate::core::assistant_live::AssistantHub> {
        &self.assistant
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

    /// Declares whether the route accepts image input.
    ///
    /// Separate from [`Self::set_model_limits`] because the two facts come from different parts of a
    /// route: a limit is a number, a modality is a set. Both are read once at assembly, so the loop
    /// never has to guess from a provider's error.
    pub fn set_accepts_images(this: &mut Arc<Self>, accepts: bool) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.accepts_images = accepts;
        }
    }

    /// Installs the sandbox knobs every tool call of this runtime will see.
    ///
    /// Same sharing constraint as [`Self::set_model_limits`].
    pub fn set_permissions(
        this: &mut Arc<Self>,
        permissions: Arc<crate::credentials::permissions::State>,
    ) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.permissions = Some(permissions);
        }
    }

    /// Installs the person and the rules a `workspace-write` turn asks.
    ///
    /// Same sharing constraint as [`Self::set_model_limits`]: call it before the runtime is cloned into
    /// the registry, or the server keeps a runtime that still allows every call.
    pub fn set_approval(
        this: &mut Arc<Self>,
        approver: Arc<dyn crate::interaction::approval::Approver>,
        rules: Arc<crate::interaction::approval_rules::Rules>,
    ) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.approver = Some(approver);
            inner.approval_rules = Some(rules);
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
    pub fn set_compaction(&mut self, policy: crate::compaction::hook::CompactionPolicy) {
        self.compaction = Some(policy);
    }

    /// Attaches the session directory before the runtime is cloned.
    ///
    /// The same value the `working_directory` tool holds. A second directory would let the tool
    /// move one place while `bash` and `read` kept starting in another.
    pub fn set_working_directory(
        this: &mut Arc<Self>,
        directory: Arc<tokio::sync::Mutex<crate::shell::working_dir::WorkingDirectory>>,
    ) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.working_directory = Some(directory);
        }
    }

    /// Attaches the settings store the tool-call ceiling is read from.
    ///
    /// Called before the runtime is cloned, same as the directory. A missing `tools.maxCalls` leaves
    /// the turn unlimited; a stored number is the ceiling.
    pub fn set_tool_settings(this: &mut Arc<Self>, store: Arc<dyn crate::SettingsStore>) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.tool_settings = Some(store);
        }
    }

    /// Turns on the background model title for sessions this runtime opens.
    pub fn set_refine_titles(this: &mut Arc<Self>, enabled: bool) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.refine_titles = enabled;
        }
    }

    /// Attaches the table `exit_plan_mode` writes through.
    pub fn set_plan_sessions(
        this: &mut Arc<Self>,
        sessions: Arc<crate::core::plan_mode::PlanSessions>,
    ) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.plan_sessions = Some(sessions);
        }
    }

    /// Attaches the table the team task tools write through.
    pub fn set_team_sessions(
        this: &mut Arc<Self>,
        sessions: Arc<crate::team::tools::TeamSessions>,
    ) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.team_sessions = Some(sessions);
        }
    }

    /// Publishes each turn this runtime starts, so `subagent` can find the conversation that called it.
    ///
    /// Call this before the runtime is cloned. A clone already shared with the server ignores the
    /// assignment, and the tools then answer that no turn is running.
    pub fn set_live_parents(
        this: &mut Arc<Self>,
        parents: Arc<crate::core::agent_loop::LiveParents>,
    ) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.live_parents = Some(parents);
        }
    }

    /// How deep a delegation from a turn started here may go.
    pub fn set_max_depth(this: &mut Arc<Self>, depth: u32) {
        if let Some(inner) = Arc::get_mut(this) {
            inner.max_depth = depth;
        }
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

    /// The signal a passive observer waits on.
    #[must_use]
    pub fn activity(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.activity)
    }

    fn poke(&self) {
        self.activity.notify_waiters();
    }

    /// Whether a tool in `session_id` is waiting for an approval decision.
    #[must_use]
    pub fn has_pending_approval(&self, session_id: &str) -> bool {
        self.pending_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session_id)
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
        let agent = std::sync::Arc::new(Agent::new(AgentSpec::root(
            SessionId::new(session_id).unwrap_or_else(|_| {
                SessionId::new("invalid-session-id").expect("a constant id is valid")
            }),
            AgentOptions::default(),
        )));
        agent.bind_activity(std::sync::Arc::clone(&this.activity));
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
                    agent: std::sync::Arc::clone(&agent),
                    turn: 0,
                },
            );
        }
        this.poke();

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

        if let Some(running) = this.lock().get_mut(session_id) {
            running.turn = turn;
        }
        let prompt = user_prompt(turn, text);
        // Written before the loop is spawned, which means `user/message` precedes `turn/start` --
        // upstream opens the turn first. The order is chosen deliberately: a prompt that reached the
        // server must survive a crash before the model answered, and a `turn/start` with no message
        // would then be a turn that began for nothing.
        if let Err(error) = session
            .append(SessionEvent::UserMessage { message: prompt })
            .await
        {
            // Admission reserved the session before writing. Release that reservation when the
            // durable prompt cannot be recorded, otherwise every later prompt would see a ghost
            // turn forever.
            this.lock().remove(session_id);
            this.poke();
            return Err(error.into());
        }
        // The list name, from the opening the user just sent. A later prompt finds the record and
        // leaves it; a rename stored beside the log still wins when the catalog is read.
        crate::session::title::record_opening_title(session.as_ref()).await?;
        if this.refine_titles {
            crate::session::title_dispatch::spawn_provider_title(
                Arc::clone(&this.config.provider),
                this.config.model.clone(),
                Arc::clone(&session),
            );
        }

        let cancel = this
            .lock()
            .get(session_id)
            .map(|running| running.cancel.clone())
            .unwrap_or_default();

        let runtime = Arc::clone(this);
        let session_id = session_id.to_owned();
        let text = text.to_owned();
        tokio::spawn(async move {
            runtime
                .drive(&session_id, text, session, cancel, turn, agent)
                .await;
            // Cleared however the turn ended, so a failed turn does not wedge the session forever.
            runtime.lock().remove(&session_id);
            runtime.poke();
        });

        Ok(turn)
    }

    /// The agent holding the session's inbox, while a turn is in flight.
    ///
    /// Queue edits and the passive state snapshot read this handle. It disappears when the turn
    /// is reaped, which is the same moment [`Self::enqueue`] starts returning `None`.
    #[must_use]
    pub fn live_agent(&self, session_id: &str) -> Option<std::sync::Arc<Agent>> {
        self.lock()
            .get(session_id)
            .map(|running| std::sync::Arc::clone(&running.agent))
    }

    /// Puts a prompt on the running turn's inbox.
    ///
    /// `steer` joins the next model step. Otherwise the prompt waits for the next turn. Returns the
    /// turn number the queued prompt will run as, or `None` when the session is idle and the caller
    /// should start a turn instead.
    pub fn enqueue(&self, session_id: &str, text: &str, steer: bool) -> Option<u64> {
        let running = self.lock();
        let running = running.get(session_id)?;
        let message = queued_prompt(text);
        let depth = if steer {
            running.agent.steer(message)
        } else {
            running.agent.followup(message)
        };
        Some(
            running
                .turn
                .saturating_add(u64::try_from(depth).unwrap_or(1)),
        )
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
        first_turn: u64,
        agent: std::sync::Arc<Agent>,
    ) {
        let events = session.replay().await.unwrap_or_default();
        let working_directory = match &self.working_directory {
            Some(template) => {
                let default_directory = template.lock().await.default_directory().to_owned();
                let state = crate::shell::working_dir::WorkingDirectory::new(default_directory)
                    .ok()
                    .and_then(|state| state.with_original(session.working_directory()).ok())
                    .map(|mut state| {
                        for event in &events {
                            if let SessionEvent::WorkspaceRootChange { cwd } = event {
                                if crate::shell::working_dir::is_absolute(cwd) {
                                    state.commit(cwd.clone(), false);
                                }
                            }
                        }
                        Arc::new(tokio::sync::Mutex::new(state))
                    });
                state
            }
            None => None,
        };
        let session_for_live = Arc::clone(&session);
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
        context.accepts_images = self.accepts_images;
        context.budget = self.config.token_budget;
        context.spill = self.spill.clone();
        context.spill_limit = self.spill_limit;
        // A deadline around every tool call. Without one, `tool.execute().await` is a future that never returns if the
        // tool never does, and the cancellation token is only read between steps -- which is exactly what a hung tool
        // prevents. The budget is per tool so a build can take minutes and a read cannot.
        context = context.with_tool_timeout(crate::core::tool_watchdog::Policy::default());
        // The read cache, on by default. Its whitelist is one tool -- `read` -- and it invalidates on a write through
        // `cache_invalidate`, so the failure it can cause is a re-read rather than a stale answer. Before this line the
        // cache was implemented, exported and entirely dead: a deployment could not get a hit.
        context = context.with_tool_cache(CACHE_CAPACITY);
        // The loop's own goal, shared with the tool. Without this assignment the continuation logic below was dead
        // code: it read a field nothing ever set, so a created goal recorded an intent that never affected a turn.
        context.goal = self.goal.clone();
        context.compaction = self.compaction.clone();
        context.approver = self.approver.clone();
        context.approval_rules = self.approval_rules.clone();
        let base = self.permissions.as_deref().cloned().unwrap_or_default();
        let (applied, logged_mode) =
            crate::sandbox::policy::permissions_for_session(&base, &events);
        context.permissions = if applied.knobs() == base.knobs() {
            self.permissions.clone()
        } else {
            Some(Arc::new(applied))
        };
        context.session_sandbox = logged_mode;
        context.pending_approvals = Some(Arc::clone(&self.pending_approvals));
        context.activity = Some(std::sync::Arc::clone(&self.activity));
        context.working_directory = working_directory;
        if let Some(store) = self.tool_settings.clone() {
            context = context.with_tool_budget(store);
        }
        context.refine_titles = self.refine_titles;
        context.plan_sessions = self.plan_sessions.clone();
        context.team_sessions = self.team_sessions.clone();
        context.live_parents = self.live_parents.clone();
        context.max_depth = self.max_depth;
        // The model's limits, when the catalogue states them. Without this the loop sends whatever the conversation has
        // accumulated and learns the answer from the provider's error -- which names a limit without saying how far past
        // it the request was.
        let (window, output) = self.model_limits();
        context = context.with_window(window, output);
        // The prompt was already written by `prompt()`, before the spawn.
        context.record_prompt = false;
        // The turn the client was told about, so the log agrees with the answer it already received.
        context.first_turn = first_turn;
        context.agent = Some(agent);
        let context = Arc::new(context);
        let _lease = self
            .live_parents
            .as_ref()
            .map(|parents| parents.publish(session_id, Arc::clone(&context)));

        let mut stream = Box::pin(crate::core::agent_loop::run(context, text, cancel));
        let mut assistant_turn = first_turn;
        let mut assistant_step = 0_u64;
        let mut assistant_open = false;
        while let Some(step) = stream.next().await {
            match step {
                Ok(LoopStep::TurnStarted { turn }) => {
                    assistant_turn = turn;
                    assistant_step = 0;
                    self.publish(AgentEvent::TurnStarted {
                        session_id: session_id.to_owned(),
                        turn,
                    });
                }
                Ok(LoopStep::Chunk(chunk)) => {
                    if let Some(visible) = visible_chunk(&chunk) {
                        if !assistant_open {
                            let cursor = session_for_live
                                .replay()
                                .await
                                .unwrap_or_default()
                                .len()
                                .checked_sub(1)
                                .and_then(|index| i64::try_from(index).ok())
                                .unwrap_or(-1);
                            self.assistant.begin(
                                session_id,
                                assistant_turn,
                                assistant_step,
                                cursor,
                            );
                            assistant_open = true;
                        }
                        self.assistant.push_chunk(session_id, visible);
                    }
                    if let StreamChunk::TextDelta { text, .. } = chunk {
                        self.publish(AgentEvent::TextDelta {
                            session_id: session_id.to_owned(),
                            text,
                        });
                    }
                }
                Ok(LoopStep::AssistantMessage(_)) => {
                    if assistant_open {
                        let seq = session_for_live
                            .replay()
                            .await
                            .unwrap_or_default()
                            .iter()
                            .enumerate()
                            .rev()
                            .find_map(|(index, event)| {
                                matches!(event, SessionEvent::AssistantMessage { .. })
                                    .then_some(u64::try_from(index).unwrap_or(u64::MAX))
                            });
                        if let Some(seq) = seq {
                            self.assistant.commit(session_id, seq);
                        } else {
                            self.assistant.abandon(session_id);
                        }
                        assistant_open = false;
                        assistant_step = assistant_step.saturating_add(1);
                    }
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
                    //
                    // A cancellation is not a failure, and reporting it as one told a UI that turn 0 errored: turn zero
                    // does not exist, so a client grouping deltas by turn had nothing to attach the failure to, and a
                    // user pressing Ctrl-C was shown a provider error. `Aborted` is the variant for it and was only ever
                    // produced by the `OpenAI` provider's own chunk mapping, never by this loop.
                    let reason = if matches!(error, crate::core::agent_loop::LoopError::Cancelled) {
                        FinishReason::Aborted
                    } else {
                        FinishReason::Error {
                            message: error.to_string(),
                        }
                    };
                    if assistant_open {
                        self.assistant.abandon(session_id);
                    }
                    self.publish(AgentEvent::TurnEnded {
                        session_id: session_id.to_owned(),
                        turn: 0,
                        reason,
                    });
                    return;
                }
            }
        }
        if assistant_open {
            self.assistant.abandon(session_id);
        }
    }

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

fn visible_chunk(chunk: &StreamChunk) -> Option<serde_json::Value> {
    match chunk {
        StreamChunk::TextDelta { index, text } => Some(serde_json::json!({
            "type": "text-delta",
            "index": index,
            "text": text,
        })),
        StreamChunk::ReasoningDelta { index, text } => Some(serde_json::json!({
            "type": "reasoning-delta",
            "index": index,
            "text": text,
        })),
        StreamChunk::BlockStart { .. }
        | StreamChunk::ToolCallDelta { .. }
        | StreamChunk::BlockEnd { .. }
        | StreamChunk::Usage(_)
        | StreamChunk::Finish { .. } => None,
    }
}

impl std::fmt::Debug for AgentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntime")
            .field("running", &self.running_count())
            .finish_non_exhaustive()
    }
}
