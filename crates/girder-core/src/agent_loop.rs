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
    /// The token budget ran out.
    ///
    /// A step rather than an error: running out of budget is an outcome a caller reports to a person, and an
    /// error would make it look like the harness broke.
    BudgetExhausted {
        /// How many tokens were spent.
        spent: u64,
    },

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
        /// What the tool produced.
        ///
        /// Carried here rather than left for the caller to fetch from the session: the session is the durable record,
        /// and a caller rendering a turn would have to re-read the log to show what a tool returned. A CLI that only
        /// saw `is_error` could report that a command failed without saying why, which is the half of the answer the
        /// person needs.
        content: Vec<ContentBlock>,
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

    /// The conversation has grown past what this model accepts.
    ///
    /// Reported before the request is sent, and with the number of tokens that have to go: a provider's own
    /// over-window error names a limit without saying how far past it the request was, which leaves the caller to
    /// guess whether to compact a little or a lot.
    #[error("the conversation is {excess} token(s) past what this model accepts")]
    OverBudget {
        /// How many tokens have to be removed.
        excess: u64,
    },

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
    ///
    /// Kept alongside the agent rather than read from it on each step: a step must use one selection,
    /// and re-resolving mid-step would let a concurrent model switch change a request that has
    /// already started.
    pub model: String,

    /// What this turn may spend on tokens.
    ///
    /// Unlimited when the caller did not set one: a deployment that does not charge should not acquire a ceiling
    /// because a field was omitted.
    pub budget: crate::limits::Ceiling,
    /// What it has spent so far in this turn.
    ///
    /// A counter rather than a `Budget`, because a turn is the unit a ceiling applies to: a budget that outlived
    /// one turn would refuse a prompt because a previous prompt was expensive.
    pub spent: std::sync::atomic::AtomicU64,

    /// Where an oversized tool result is saved.
    ///
    /// `None` means this deployment has no spill storage, and an oversized result is returned as it is. That is the
    /// honest behaviour: truncating it would lose output the model could have read, and refusing to return anything
    /// would lose it entirely.
    pub spill: Option<Arc<dyn crate::spill::Store>>,
    /// How many bytes a tool result may carry before it is spilled.
    pub spill_limit: usize,

    /// How many bytes of tool output one turn may add before the rest is trimmed.
    ///
    /// `spill_limit` bounds one result; this bounds the round. A model calling five tools that each return 100 KB
    /// produces 500 KB, which no per-result limit sees. `None` leaves the round unbounded, which is the behaviour
    /// before this field existed.
    pub round_output_limit: Option<usize>,

    /// Bytes of tool output this turn has added so far. Reset at the start of each turn.
    pub round_output_spent: std::sync::atomic::AtomicUsize,

    /// How many times one tool may be called in a turn, and how often it may fail in a row.
    ///
    /// A turn budget bounds how many turns happen and nothing bounds how many calls one tool receives within them: forty
    /// reads and forty shell invocations are the same shape to `max_turns` and a different situation to a person
    /// watching. `None` leaves it unbounded, which is the behaviour before this field existed.
    pub tool_limits: Option<crate::tool_limits::Limit>,

    /// The counts `tool_limits` needs, shared because the dispatch path does not own the context.
    pub tool_tracker: Option<std::sync::Mutex<crate::tool_limits::Tracker>>,

    /// Remembers a read between calls, when the deployment enables it.
    ///
    /// `None` by default: a cache changes which syscall happens, and a deployment that did not ask for that should not
    /// get it. The whitelist inside `tool_cache` is what keeps a write out of it, but the decision to have one at all
    /// belongs here.
    pub tool_cache: Option<std::sync::Mutex<crate::tool_cache::Cache>>,

    /// How long one tool call may run before it is stopped.
    ///
    /// `None` leaves it unbounded, which is the behaviour before this field existed. A budget rather than a checked flag:
    /// the process layer already has 120-second timeouts per command, and what was missing was the deadline around the
    /// call itself.
    pub tool_timeout: Option<Arc<crate::tool_watchdog::Policy>>,

    /// The rules that turn a call into allow, deny or ask.
    ///
    /// `None` allows everything: the absence of a policy is not a policy.
    pub approval_rules: Option<Arc<crate::approval_rules::Rules>>,

    /// The approver a hook's `Ask` is resolved through.
    ///
    /// `None` refuses an `Ask` rather than running it: a call that was supposed to be checked and was not is worse than
    /// a call that did not happen.
    pub approver: Option<Arc<dyn crate::approval::Approver>>,

    /// How the history is compacted when it outgrows the context.
    ///
    /// `None` keeps everything: a deployment that has not decided what to drop should not have a default decide for
    /// it, and the failure mode of dropping the wrong messages is a model that has forgotten what it was doing.
    pub compaction: Option<crate::compaction_hook::CompactionPolicy>,

    /// The agent this loop runs for.
    ///
    /// The single source of truth for cancellation, model selection, and pending input. The loop
    /// holds it rather than duplicating those three as its own fields, because two copies of the same
    /// state is how a cancel that the UI sees and the loop does not becomes possible.
    pub agent: Option<Arc<crate::agent::Agent>>,

    /// The goal this loop advances, when one is attached.
    ///
    /// Separate from the agent because a goal is a *session* concept, not an agent one: a resumed
    /// session restores its goal before it has an agent, and a loop may run with no goal at all.
    pub goal: Option<Arc<std::sync::Mutex<crate::goal::GoalState>>>,
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

    /// The largest context the model accepts, when the deployment knows it.
    ///
    /// `None` means nobody stated one, and the loop does not guess: a budget check against an invented window would
    /// refuse requests the model would have accepted, which is worse than letting the provider be the authority.
    pub context_window: Option<u64>,

    /// How many output tokens the model will be asked for, which is subtracted from the window before the input is
    /// measured. A request that fills the window leaves nothing to answer with.
    pub max_output_tokens: Option<u32>,
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
            agent: None,
            goal: None,
            agent_id,
            max_turns: 16,
            depth: crate::subagent::Depth::root(),
            max_depth: 2,
            message_counter: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            record_prompt: true,
            budget: crate::limits::Ceiling::Unlimited,
            spent: std::sync::atomic::AtomicU64::new(0),
            spill: None,
            spill_limit: crate::spill::DEFAULT_SPILL_LIMIT,
            round_output_limit: None,
            round_output_spent: std::sync::atomic::AtomicUsize::new(0),
            tool_limits: None,
            tool_tracker: None,
            tool_cache: None,
            approver: None,
            approval_rules: None,
            tool_timeout: None,
            compaction: None,
            context_window: None,
            max_output_tokens: None,
        }
    }

    /// Bounds how long one tool call may run.
    #[must_use]
    pub fn with_tool_timeout(mut self, policy: crate::tool_watchdog::Policy) -> Self {
        self.tool_timeout = Some(Arc::new(policy));
        self
    }

    /// Applies a policy to every call before it runs.
    ///
    /// Takes the rules rather than building them: which calls need a person is the deployment's decision, and this layer
    /// only knows how to carry it out.
    #[must_use]
    pub fn with_approval_rules(mut self, rules: crate::approval_rules::Rules) -> Self {
        self.approval_rules = Some(Arc::new(rules));
        self
    }

    /// Sets the approver an `Ask` is resolved through.
    #[must_use]
    pub fn with_approver(mut self, approver: Arc<dyn crate::approval::Approver>) -> Self {
        self.approver = Some(approver);
        self
    }

    /// Enables remembering a read between calls.
    #[must_use]
    pub fn with_tool_cache(mut self, capacity: usize) -> Self {
        self.tool_cache = Some(std::sync::Mutex::new(crate::tool_cache::Cache::new(
            capacity,
        )));
        self
    }

    /// Bounds how many times one tool may be called in a turn.
    ///
    /// Built from the limits so the two cannot disagree: a tracker with its own ceiling and a field with another would
    /// be two answers to one question.
    #[must_use]
    pub fn with_tool_limits(mut self, limits: crate::tool_limits::Limit) -> Self {
        self.tool_tracker = Some(std::sync::Mutex::new(crate::tool_limits::Tracker::new(
            limits,
        )));
        self.tool_limits = Some(limits);
        self
    }

    /// Tells the loop what the model accepts, so an over-window request is refused before it is sent.
    ///
    /// Both numbers come from the deployment because only it knows which model it is talking to: a provider whose
    /// catalogue states a window has one, and a provider that states nothing leaves both unset and the check skipped.
    #[must_use]
    pub fn with_window(
        mut self,
        context_window: Option<u64>,
        max_output_tokens: Option<u32>,
    ) -> Self {
        self.context_window = context_window;
        self.max_output_tokens = max_output_tokens;
        self
    }

    /// Attaches the goal this loop advances.
    ///
    /// An armed goal makes the loop keep going after a turn that would otherwise end, which is the
    /// whole point of a goal: the user asked for an outcome, not for one answer.
    #[must_use]
    pub fn with_goal(self, goal: Arc<std::sync::Mutex<crate::goal::GoalState>>) -> Self {
        Self {
            goal: Some(goal),
            ..self
        }
    }

    /// Attaches the agent this loop runs for.
    ///
    /// When attached, the loop cancels through it and records the model it used, so a caller's
    /// selection and cancellation reach the loop instead of living in a second copy.
    #[must_use]
    pub fn with_agent(mut self, agent: Arc<crate::agent::Agent>) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Sets the delegation depth and ceiling.
    #[must_use]
    pub fn with_depth(mut self, depth: crate::subagent::Depth, max_depth: u32) -> Self {
        self.depth = depth;
        self.max_depth = max_depth;
        self
    }
}

/// Saves an oversized tool result and returns the locator that replaces it.
///
/// Both halves are needed: the text is written to the store so it is still readable, and the model is told where it
/// went with a hint about how to read it. Replacing without saving would lose the output; saving without replacing
/// would put the output in the context window anyway.
async fn spill_if_needed(context: &LoopContext, blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    // The round is checked before the result, because the two answer different questions: `spill_limit` asks whether
    // one result is too large to keep, and this asks whether the turn has room for it. A turn that has already spent
    // its budget trims what follows even when each result is small, which no per-result limit can see.
    let blocks = match context.round_output_limit {
        Some(limit) => {
            let text: String = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let remaining = limit.saturating_sub(
                context
                    .round_output_spent
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            // Counted whether or not it is kept: the point of a round budget is that a result which was trimmed still
            // consumed the room it was trimmed for, and forgetting it would let a loop of trimmed results run forever.
            let _ = context
                .round_output_spent
                .fetch_add(text.len(), std::sync::atomic::Ordering::Relaxed);
            trim_to(blocks, remaining)
        }
        None => blocks,
    };

    let Some(store) = &context.spill else {
        return blocks;
    };

    let text: String = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    let (action, _reason) = crate::spill_policy::decide(
        crate::spill_policy::Context {
            bytes: text.len(),
            truncated: false,
        },
        context.spill_limit,
    );
    if !action.should_spill() {
        return blocks;
    }

    let Some(request) = crate::spill::SaveRequest::new(text, "tool-output") else {
        return blocks;
    };
    let Ok(reference) = store.save(request).await else {
        // A store that fails leaves the output in place: losing it would be worse than a large result.
        return blocks;
    };
    vec![ContentBlock::Text {
        text: reference.to_text(),
    }]
}

/// Drops the history's old middle when the policy says it has outgrown the context.
///
/// The policy reads `message::Message`, the recorded form a session stores, while this loop keeps `llm::Message`, the
/// wire form a provider receives. The two differ by an id and a source, neither of which affects which messages belong
/// together, so the adapter maps only those two fields and the decision transfers by position. Applying the plan to the
/// recorded form would be wrong: the loop needs the wire form, and rebuilding it from recorded messages would drop the
/// tool-call ids a provider matches results against.
///
/// The kept selection is not a summary: this layer has no model to summarise with and no tokenizer to measure the
/// result, so it removes messages and leaves the rest untouched. That is the honest operation — a summary produced
/// without a model would be a fabrication, and one produced with a model would need a second call the caller did not
/// ask for.
///
/// A refusal leaves the history alone. Compacting into an illegal shape (a tool call without its result) would make
/// every subsequent request fail, which is worse than a long context.
fn compact_history(context: &LoopContext, messages: &mut Vec<Message>) {
    let Some(policy) = &context.compaction else {
        return;
    };
    // Decided and applied under one borrow so a concurrent read cannot see the intermediate vector.
    // The plan is computed from a shared view of the same vector it is applied to: `plan` returns positions in the
    // input, so the borrow ends before the assignment below.
    // Built in the loop's own order, so an index in the plan addresses the message it was computed from.
    let recorded: Vec<crate::message::Message> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            crate::message::Message::new(
                crate::brand::MessageId::new(format!("m{index}")).unwrap_or_else(|_| {
                    crate::brand::MessageId::new("m0").expect("a literal id is valid")
                }),
                recorded_role(message.role),
                crate::message::MessageSource::User { form: None },
                message.content.clone(),
            )
        })
        .collect();

    let plan = policy.plan(recorded.as_slice());
    match plan {
        crate::compaction_hook::Plan::KeepAll | crate::compaction_hook::Plan::Refuse { .. } => {}
        crate::compaction_hook::Plan::Compact { keep, .. } => {
            // What is dropped is summarised before it is removed. Without this the conversation became a transcript
            // with a hole in it, and a request made before the cut -- "keep the old spelling for one release" -- was
            // gone, not shortened.
            let dropped: Vec<crate::message::Message> = recorded
                .iter()
                .enumerate()
                .filter(|(index, _)| !keep.contains(index))
                .map(|(_, message)| message.clone())
                .collect();
            let summary = (!dropped.is_empty()).then(|| {
                crate::compaction_summary::render(&crate::compaction_summary::summarize(&dropped))
            });
            // The indices were computed against this vector and are used against it: `plan` returns positions in the
            // input, so applying them to a different vector would drop the wrong messages.
            let kept: Vec<Message> = keep
                .into_iter()
                .filter_map(|index| messages.get(index).cloned())
                .collect();
            if !kept.is_empty() {
                // The note goes in front, where a reader starts: it describes what came before the first kept message,
                // and putting it anywhere else would read as a remark about the middle of the conversation.
                let mut next: Vec<Message> = summary
                    .filter(|text| !text.is_empty())
                    .map(|text| Message {
                        role: crate::llm::Role::User,
                        content: vec![ContentBlock::Text { text }],
                        tool_call_id: None,
                    })
                    .into_iter()
                    .collect();
                next.extend(kept);
                *messages = next;
            }
        }
    }
}

/// Maps a wire role onto the recorded vocabulary.
///
/// The two enums are not in bijection: the wire form has `Tool` where the recorded form distinguishes `Developer` from
/// `User`, and it has no representation for a system prompt. Only the pairing rules matter for compaction, and for those
/// the three wire roles are enough, so the lossy direction is the one taken here and the loss is confined to this
/// adapter rather than to the recorded history.
fn recorded_role(role: crate::llm::Role) -> crate::message::Role {
    match role {
        crate::llm::Role::User => crate::message::Role::User,
        crate::llm::Role::Assistant => crate::message::Role::Assistant,
        crate::llm::Role::Tool => crate::message::Role::Developer,
    }
}

/// Builds and records a goal-continuation prompt, when the goal says to keep going.
///
/// A separate function for two reasons: `run` was long enough to hide the order in which it decides
/// to continue, and the guard-taking must not sit across an `await` — a `std` mutex guard is not
/// `Send`, so holding one would make the whole stream un-spawnable. Deciding here and awaiting after
/// keeps both properties.
async fn goal_continuation(
    context: &Arc<LoopContext>,
    turn: u64,
) -> Option<crate::message::Message> {
    let goal = context.goal.as_ref()?;

    // Decide under the lock and release it before the await below.
    let (used, budget) = {
        let mut state = goal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.may_continue() {
            return None;
        }
        // A budget that is spent is not an error here: the loop simply ends.
        let updated = state.start_round(turn).ok()?;
        (updated.rounds_started, updated.max_rounds)
    };

    // The prompt says what to do without restating the objective: the model has just been working on
    // it, and repeating it invites a restatement rather than progress.
    let prompt = format!(
        "Continue working toward the goal. You have used {used}/{budget} rounds. \
         If the goal is complete, mark it complete. If something concrete blocks you, say what it is."
    );
    let message = crate::message::Message::new(
        context.next_message_id(),
        crate::message::Role::User,
        crate::message::MessageSource::User {
            form: Some(crate::message::ContextForm::Notice),
        },
        vec![ContentBlock::Text { text: prompt }],
    );
    let _ = context
        .session
        .append(SessionEvent::UserMessage {
            message: message.clone(),
        })
        .await;
    Some(message)
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
        let mut messages = vec![Message::new(Role::User, vec![ContentBlock::Text { text: prompt.clone() }])];

        // The first user message is durable before any turn starts, unless the caller already
        // recorded it.
        // The prompt is durable, but not here: upstream opens `turn/start` *before* claiming the
        // input, and a `user/message` written first makes a replay show the user speaking before the
        // turn that carries their message began. It is emitted inside the first iteration instead.
        let mut prompt_pending = context.record_prompt;

        for turn in 1..=u64::from(context.max_turns) {
            // The agent's token is a second cancellation source: a UI cancels through the agent, and
            // a loop that only watched the caller's token would keep running after the user's
            // interrupt.
            let agent_cancelled = context
                .agent
                .as_ref()
                .is_some_and(|agent| agent.cancel_token().is_cancelled());
            if cancel.is_cancelled() || agent_cancelled {
                yield Err(LoopError::Cancelled);
                return;
            }

            // The budget gates the next turn, not the one that already ran: a spend cannot be undone, so the
            // useful moment to refuse is before more is spent. Refusing mid-turn would leave the log with a
            // `turn/start` and no `turn/end`.
            if !context.has_budget() {
                yield Ok(LoopStep::BudgetExhausted {
                    spent: context
                        .spent
                        .load(std::sync::atomic::Ordering::Relaxed),
                });
                return;
            }

            // Record the selection this turn actually uses, so "last used" reflects reality rather
            // than the last thing a caller asked for.
            if let Some(agent) = &context.agent {
                agent.note_model_used(crate::agent::ModelSelection::new(
                    agent
                        .next_model()
                        .map_or_else(|| context.model.clone(), |selection| selection.provider),
                    context.model.clone(),
                ));
            }

            let _ = context.session.append(SessionEvent::TurnStart { turn }).await;
            yield Ok(LoopStep::TurnStarted { turn });

            // The prompt lands after `turn/start` and only once: a later turn is a continuation and
            // must not re-record what the user already said.
            //
            // `prompt_pending` is false when the caller already wrote it — the RPC runtime does,
            // before spawning, so a prompt is durable even if the model never answers.
            if prompt_pending {
                prompt_pending = false;
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

            // Compacted before the step, not after: a step that ran on an oversized history would pay for the
            // window it was about to exceed, and the provider would refuse the request rather than the loop
            // shrinking it.
            compact_history(&context, &mut messages);

            // Each step is forwarded as it is produced: a caller that renders the answer incrementally
            // would otherwise see nothing until the whole turn finished.
            let steps = match run_steps(&context, &mut messages, turn, cancel.clone()).await {
                Ok(steps) => steps,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            for step in steps {
                yield Ok(step);
            }
            // The turn is over: every step finished. Written here rather than inside the step, which
            // is the only place that knows whether another step follows.
            let _ = context
                .session
                .append(SessionEvent::TurnEnd {
                    turn,
                    reason: crate::session::TurnEndReason::Completed,
                })
                .await;

            // Input queued for the next turn continues the conversation even when this turn ended
            // without tools. A user who typed while the model was answering expects that message to
            // be answered, and ending here would silently drop it.
            let queued = context
                .agent
                .as_ref()
                .map_or_else(Vec::new, |agent| agent.take_queued_turn());

            if !queued.is_empty() {
                for message in &queued {
                    let _ = context.session.append(SessionEvent::UserMessage {
                        message: message.clone(),
                    }).await;
                    messages.push(Message::new(
                        Role::User,
                        message.content.clone(),
                    ));
                }
                // No `TurnStarted` here: the next iteration of the outer loop writes `turn/start`
                // itself. Yielding one as well produced two `turn/start` records for one
                // `turn/end`, which a replay reads as a turn that began twice.
            }

            // An armed goal keeps going after a turn that would otherwise end. This is what makes a
            // goal different from a prompt: the user asked for an outcome, and one answer is not it.
            //
            // The round budget is checked before continuing rather than after, so an unreachable
            // objective stops instead of spending one more turn discovering that.
            let mut goal_continued = false;
            if let Some(message) = goal_continuation(&context, turn).await {
                messages.push(Message::new(Role::User, message.content.clone()));
                goal_continued = true;
            }

            // A turn now consumes its own tool steps, so reaching here means the model answered.
            // The conversation continues only when something arrived while it was answering, or a
            // goal asked for another round.
            if queued.is_empty() && !goal_continued {
                return;
            }
        }

        // The turn budget ran out without the model finishing without tools.
        yield Err(LoopError::Cancelled);
    }
}

/// Merges two cancellation sources into one token.
///
/// A child token that fires when either input fires. Needed because a turn has two cancellation
/// sources -- the caller's and the agent's -- and a provider only accepts one token.
fn merge_cancel(first: CancellationToken, second: Option<CancellationToken>) -> CancellationToken {
    let Some(second) = second else {
        return first;
    };
    let merged = CancellationToken::new();
    for source in [first, second] {
        let merged = merged.clone();
        tokio::spawn(async move {
            source.cancelled().await;
            merged.cancel();
        });
    }
    merged
}

/// Runs one turn's steps, returning the steps it produced.
///
/// A separate function because `run` had grown past the point where the order of its decisions was
/// readable. The steps of one turn are the part that must stay together: each is one model call plus
/// the tools it requested, and whether another follows depends on whether this one ran tools.
async fn run_steps(
    context: &Arc<LoopContext>,
    messages: &mut Vec<Message>,
    turn: u64,
    cancel: CancellationToken,
) -> Result<Vec<LoopStep>, LoopError> {
    use futures::StreamExt as _;

    let mut produced = Vec::new();
    let mut step_index = 0u64;
    loop {
        step_index += 1;
        let _ = context
            .session
            .append(SessionEvent::StepStart {
                turn,
                step: step_index,
            })
            .await;

        // Whether *this* step ran tools decides whether another follows.
        let mut ran_tools = false;
        let mut finished = false;
        let mut folded = Box::pin(turn_stream(context, messages, turn, cancel.clone()));
        while let Some(step) = folded.next().await {
            let step = step?;
            match &step {
                LoopStep::TurnEnded { .. } => finished = true,
                LoopStep::ToolFinished { .. } => ran_tools = true,
                _ => {}
            }
            produced.push(step);
            if finished {
                break;
            }
        }

        let _ = context
            .session
            .append(SessionEvent::StepEnd {
                turn,
                step: step_index,
            })
            .await;

        if !finished || !ran_tools {
            // No terminal step, or a step with no tools: the turn is over either way.
            return Ok(produced);
        }
    }
}

/// Runs one turn: stream, assemble, dispatch tools.
///
/// Yields steps and appends to `messages` so the caller can decide whether to continue. Split out
/// of [`run`] because the streaming and tool dispatch are two concerns and joining them made one
/// function unreadable.
/// The modification time of the file a `read` call names, when it names one.
///
/// Read from the arguments rather than from the tool's own report: the result carries no version, so the cache would
/// otherwise have no way to notice an edit between two reads. An absent or unparseable path yields `None`, which the
/// cache treats as "cannot verify" and therefore as a miss.
fn read_mtime(execution: &ToolExecution) -> Option<u64> {
    let arguments: serde_json::Value = serde_json::from_str(&execution.arguments).ok()?;
    let path = arguments.get("path")?.as_str()?;
    let metadata = std::fs::metadata(path).ok()?;
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// Keeps at most `limit` bytes of a result, on a character boundary.
///
/// Truncation rather than removal: a result the model cannot see is a tool call that appears to have returned nothing,
/// which is a different claim from one whose output was cut. The marker says which happened.
fn trim_to(blocks: Vec<ContentBlock>, limit: usize) -> Vec<ContentBlock> {
    let mut spent = 0;
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        let ContentBlock::Text { text } = block else {
            // A non-text block carries a reference rather than a payload, so it does not consume the budget and is not
            // trimmed: cutting an image reference would make the attachment unreachable rather than smaller.
            out.push(block);
            continue;
        };
        let room = limit.saturating_sub(spent);
        if text.len() <= room {
            spent += text.len();
            out.push(ContentBlock::Text { text });
            continue;
        }
        // A boundary, so a multi-byte character is never split into a replacement character.
        let mut cut = room;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(ContentBlock::Text {
            text: format!(
                "{}\n\n[output trimmed: this turn had {limit} bytes of tool output left]",
                &text[..cut]
            ),
        });
        spent = limit;
    }
    out
}

/// How far past the model's input allowance a request is, if it is past at all.
///
/// A free function rather than a block inside `turn_stream`, which was already at the length the linter enforces and
/// whose job is streaming rather than arithmetic.
fn over_budget(
    context: &LoopContext,
    messages: &[crate::llm::Message],
    tools: &[crate::llm::ToolSchema],
) -> Option<u64> {
    // A provider whose window nobody stated cannot be checked, so it is not: refusing on a guess would make an
    // unconfigured model unusable.
    let window = context.context_window?;
    let usage = crate::context_budget::Usage {
        // No system prompt is assembled at this layer, so there is nothing to measure here. A caller that grows one
        // passes it through `LoopContext`.
        system: 0,
        messages: messages
            .iter()
            // `estimate_blocks` rather than `estimate_message`, because the request carries `llm::Message` and the
            // estimator is written against the session's own message type. The difference is the role overhead, which
            // the budget's margin absorbs.
            .map(|message| crate::token_meter::estimate_blocks(&message.content))
            .sum(),
        tools: tools
            .iter()
            .map(|schema| {
                crate::token_meter::estimate_text(&schema.name)
                    + crate::token_meter::estimate_text(&schema.description)
                    + crate::token_meter::estimate_text(&schema.parameters.to_string())
            })
            .sum(),
    };
    let budget = crate::context_budget::Budget {
        window,
        reserve_output: u64::from(context.max_output_tokens.unwrap_or(4096)),
        // A margin rather than the exact allowance: the estimate is a character heuristic, and a check that refused at
        // the exact number would reject requests the model would have accepted.
        margin: window / 20,
    };
    match crate::context_budget::check(&budget, &usage) {
        crate::context_budget::Decision::Fits => None,
        crate::context_budget::Decision::Over { excess } => Some(excess),
    }
}

fn turn_stream<'a>(
    context: &'a Arc<LoopContext>,
    messages: &'a mut Vec<Message>,
    turn: u64,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<LoopStep, LoopError>> + Send + 'a {
    async_stream::stream! {
        use futures::StreamExt as _;
        // Fold the agent's token into this turn's cancellation. A UI cancels through the agent, and a
        // turn that watched only the caller's token would keep streaming after the interrupt -- which
        // is what a test with a never-ending provider exposed.
        let agent_cancel = context
            .agent
            .as_ref()
            .map(|agent| agent.cancel_token());
        let cancel = merge_cancel(cancel, agent_cancel);

        let tool_schemas = context.tools.schemas();
        // Checked before the request exists, because the alternative is learning from the provider.
        if let Some(excess) = over_budget(context, messages.as_slice(), &tool_schemas) {
            yield Err(LoopError::OverBudget { excess });
            return;
        }

        let request = LlmRequest::new(context.model.clone(), messages.clone())
            .with_tools(tool_schemas);
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

        // The cost is recorded as its own event rather than a field on the message: a reader that renders content
        // does not need a number it never shows, and a reader that sums cost should not parse messages to find it.
        if let Some(usage) = &assistant.usage {
            let input = usage.input;
            let output = usage.output;
            let _ = context
                .session
                .append(SessionEvent::ModelUsage {
                    model: context.model.clone(),
                    input_tokens: input,
                    output_tokens: output,
                })
                .await;

            // Checked after recording, so a session that overspent says what it spent rather than only that it
            // stopped. The spend cannot be undone, which is why the next step is the one refused.
            context.record_spend(input.saturating_add(output));
        }
        yield Ok(LoopStep::AssistantMessage(assistant.clone()));

        let calls = assistant.tool_calls();
        if calls.is_empty() {
            // No tools, so this is the turn's last step. The `turn/end` record belongs to the caller,
            // which knows whether another step follows: writing it here made every step close the
            // turn, so a tool-using exchange recorded two `turn/end` for one `turn/start`.
            let reason = assistant.finish.clone().unwrap_or(FinishReason::Stop);
            yield Ok(LoopStep::TurnEnded { turn, reason });
            return;
        }

        messages.push(Message::new(Role::Assistant, assistant.content.clone()));

        let mut tool_steps = Box::pin(tool_calls(context, messages, calls, &cancel));
        while let Some(step) = tool_steps.next().await {
            let done = step.is_err();
            yield step;
            if done {
                return;
            }
        }

        // The tools ran; the model must see their results, so the caller runs another step. The turn
        // is not over, which is why no `turn/end` is written here.
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

            // Checked before the call is recorded, so a refused call does not appear in the log as one that happened.
            // The count still rises: a model that asked fifty times should be visible as having asked fifty times, not
            // as having asked five and been silently ignored.
            if let Some(tracker) = &context.tool_tracker {
                let verdict = tracker
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record(name, true);
                if let crate::tool_limits::Verdict::Allow = verdict {
                    // Admitted; the failure count is corrected below if the call fails.
                } else {
                    let text = verdict.explain();
                    let _ = context
                        .session
                        .append(SessionEvent::ToolResult {
                            id: id.clone(),
                            content: vec![ContentBlock::Text { text: text.clone() }],
                            is_error: true,
                        })
                        .await;
                    yield Ok(LoopStep::ToolFinished {
                        id: id.to_string(),
                        content: vec![ContentBlock::Text { text }],
                        is_error: true,
                    });
                    continue;
                }
            }

            let result = match dispatch(context, execution, cancel.clone()).await {
                Ok(result) => result,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let is_error = result.is_error;

            // An oversized result is saved and replaced by a locator. The decision is made here rather than inside
            // each tool because every tool produces output and only this layer knows what the session can afford:
            // a tool that spilled on its own would do it at a threshold of its own choosing.
            let blocks = spill_if_needed(context, result.content.clone()).await;

            let _ = context
                .session
                .append(SessionEvent::ToolResult {
                    id: id.clone(),
                    content: blocks.clone(),
                    is_error,
                })
                .await;
            yield Ok(LoopStep::ToolFinished {
                id: id.to_string(),
                is_error,
                content: blocks,
            });

            // A tool message records which call it answers. Without the call id the provider cannot
            // emit a `tool_result` block, and the API rejects the following request: every
            // `tool_use` must be answered by the very next message.
            messages.push(Message::tool_result(id.clone(), result.content));
        }
    }
}

/// Runs the three-waterfall pipeline for one call.
///
/// Order matters and is upstream's: pre-execute decides, execute calls, post-execute normalizes.
/// A `Deny` from the first stops before the body runs, which is why it returns a `ToolResult`
/// carrying the reason rather than an error — the model should see the denial and adapt.
/// Applies the deployment's approval rules to one call.
///
/// A `None` rule set allows: the absence of a policy is not a policy, and refusing every call in a deployment that never
/// configured one would make the feature mandatory rather than available.
fn decide_by_rules(
    rules: Option<&crate::approval_rules::Rules>,
    execution: &ToolExecution,
) -> PreToolDecision {
    let Some(rules) = rules else {
        return PreToolDecision::Allow;
    };
    let decision = rules.decide(&execution.name, &execution.arguments);
    // The reason names the rule that matched, because a person asked "why is this stopping" needs to know which of their
    // rules fired -- and `matched: None` says the default did, which is itself the answer when no rule was written.
    let question = match decision.matched {
        Some(index) => format!("{} (rule {index}): {}", decision.reason, execution.name),
        None => format!("{}: {}", decision.reason, execution.name),
    };
    match decision.action {
        crate::approval_rules::Action::Allow => PreToolDecision::Allow,
        crate::approval_rules::Action::Deny => PreToolDecision::Deny { reason: question },
        crate::approval_rules::Action::Ask => PreToolDecision::Ask { question },
    }
}

/// Resolves a hook's `Ask` through the deployment's approver.
///
/// A free function because `dispatch` was at the length the linter enforces and because the three ways this can refuse --
/// no approver, a denial, an unanswered request -- are the interesting part and read better named.
async fn ask_approver(
    context: &LoopContext,
    tool: &str,
    question: &str,
) -> Result<(), crate::tools::ToolResult> {
    // A hook that wants a person decides the shape of the pause; without an approver there is nobody to ask, and the
    // honest answer is to refuse rather than to run the call that was supposed to be checked.
    let Some(approver) = &context.approver else {
        return Err(crate::tools::ToolResult::error(format!(
            "`{tool}` needs approval but no approver is configured: {question}"
        )));
    };
    // A blank action or description cannot be put in front of a person; the constructor refuses it, and there is nothing
    // to ask about.
    let Some(request) = crate::approval::Request::new(tool, question) else {
        return Err(crate::tools::ToolResult::error(format!(
            "`{tool}` needs approval but the question was blank"
        )));
    };
    match approver.ask(request).await {
        Ok(crate::approval::Decision::Approved { .. }) => Ok(()),
        Ok(crate::approval::Decision::Denied { .. }) => Err(crate::tools::ToolResult::error(
            format!("`{tool}` was not approved: {question}"),
        )),
        // A timed-out or failed approval is not consent. Treating "nobody answered" as yes is the one reading that
        // cannot be undone.
        Err(error) => Err(crate::tools::ToolResult::error(format!(
            "`{tool}` could not be approved: {error}"
        ))),
    }
}

/// Runs the call's body under the deployment's deadline.
///
/// `None` means the deadline expired. A free function because `dispatch` was at the length the linter enforces, and
/// because the reason a deadline belongs here rather than at the process layer deserves to be written down where the
/// decision is: the process layer's timeouts are per command, and a tool that never returns is not a command.
async fn run_with_deadline(
    context: &LoopContext,
    execution: &ToolExecution,
    tool: Arc<crate::tools::ToolDefinition>,
    cancel: CancellationToken,
) -> Option<ToolResult> {
    let call = execution.clone();
    let body = async {
        context
            .bus
            .waterfall::<crate::tools::ToolExecute, _>(execution.clone(), move |_args| {
                let call = call.clone();
                let tool = Arc::clone(&tool);
                let cancel = cancel.clone();
                Box::pin(async move { run_body(tool, call, cancel).await })
            })
            .await
    };
    // Around the call rather than checked after it: a future that never completes never returns control, and the
    // cancellation token is only read between steps -- which is exactly what a hung tool prevents.
    let Some(policy) = context.tool_timeout.as_ref() else {
        return Some(body.await);
    };
    let budget = std::time::Duration::from_millis(policy.timeout_for(&execution.name));
    tokio::time::timeout(budget, body).await.ok()
}

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

    // Checked before the waterfall, because a cached answer means the tool does not run at all -- and a permission hook
    // that never sees the call cannot deny it. The whitelist inside `tool_cache` admits only tools whose output depends
    // on a file's contents alone, which is what makes answering from memory the same answer.
    let cache_key = crate::tool_cache::is_cacheable(&execution.name)
        .then(|| crate::tool_cache::Key::of(&execution.name, &execution.arguments));
    if let (Some(cache), Some(key)) = (&context.tool_cache, &cache_key) {
        let mtime = read_mtime(&execution);
        let hit = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key, mtime)
            .map(|entry| (entry.text.clone(), entry.is_error));
        if let Some((text, is_error)) = hit {
            return Ok(if is_error {
                ToolResult::error(text)
            } else {
                ToolResult::text(text)
            });
        }
    }

    // The rules are the terminal, not a hook, because the terminal is what runs when no plugin has an opinion -- and a
    // deployment that configured "ask before rm" has one even when nothing else does. A hook registered later still
    // overrides, which is the point of the waterfall.
    let call = execution.clone();
    let rules = context.approval_rules.clone();
    let decision = context
        .bus
        .waterfall::<ToolPreExecute, _>(execution.clone(), move |_args| {
            let call = call.clone();
            let rules = rules.clone();
            Box::pin(async move { decide_by_rules(rules.as_deref(), &call) })
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
        PreToolDecision::Ask { question } => {
            if let Err(refusal) = ask_approver(context, &execution.name, &question).await {
                return Ok(refusal);
            }
        }
    }

    // The execute waterfall wraps the body, and the deadline wraps that.
    let Some(result) =
        run_with_deadline(context, &execution, Arc::clone(&tool), cancel.clone()).await
    else {
        return Ok(ToolResult::error(format!(
            "`{}` did not finish within its budget and was stopped",
            execution.name
        )));
    };

    let args = PostExecuteArgs {
        execution: execution.clone(),
        result: result.clone(),
    };
    let decision = context
        .bus
        .waterfall::<ToolPostExecute, _>(args, accept_terminal)
        .await;

    // Stored after the post-execute waterfall rather than before it, so what the model will actually see is what gets
    // remembered: a hook that rewrote the result would otherwise be undone by a later hit.
    if let (Some(cache), Some(key)) = (&context.tool_cache, cache_key.clone()) {
        let text: String = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let entry = crate::tool_cache::Entry {
            text,
            is_error: result.is_error,
            source_mtime_ms: read_mtime(&execution),
        };
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .put(key, entry);
    }

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
            // The parser's message names a line and column, and a model given only that has to scan the whole argument
            // string to find them. The corrective text points at the position and lists the four shapes that produce
            // one, which is the difference between a model that can fix this and one that retries the same mistake.
            let failure = crate::tool_retry::Failure {
                tool: execution.name.clone(),
                reason: crate::tool_retry::FailureReason::MalformedJson {
                    detail: error.to_string(),
                },
            };
            return ToolResult::error(crate::tool_retry::corrective_message(&failure));
        }
    };

    let context = ToolRunContext::new(execution, cancel);
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
    /// Records a spend and reports whether the budget still allows another step.
    ///
    /// Called after a step rather than before: the step already happened, and a check that ran first could only
    /// refuse work the caller had not yet paid for. The refusal this enables is of the *next* step.
    pub fn record_spend(&self, tokens: u64) -> bool {
        let total = self
            .spent
            .fetch_add(tokens, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(tokens);
        self.budget.allows(total, 1)
    }

    /// Whether the budget allows one more step.
    pub fn has_budget(&self) -> bool {
        let spent = self.spent.load(std::sync::atomic::Ordering::Relaxed);
        self.budget.allows(spent, 1)
    }

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
