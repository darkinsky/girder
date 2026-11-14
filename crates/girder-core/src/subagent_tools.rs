//! The subagent control tools: `subagent_list`, `subagent_send`, and `subagent_wait`.
//!
//! [`crate::subagent`] can *start* a child, but a model that delegated once has no way to ask what it
//! started, to collect a result it did not wait for, or to say one more thing to a child that is still
//! working. This module is that surface, plus the bookkeeping it needs.
//!
//! # Two tools work and one does not, on purpose
//!
//! `subagent_list` and `subagent_wait` are implementable because the missing information is merely
//! *unrecorded*, not unreachable: this module records it. `subagent_send` is not implementable at all
//! — [`crate::subagent::delegate`] gives a caller no channel into a running child — so its tool
//! returns an error naming each missing piece. An error that says "there is no inbox" is worth more
//! to a model than a tool that accepts a message and drops it.
//!
//! # Why the registry lives here rather than in `subagent`
//!
//! `delegate` returns a [`DelegationResult`] only after the child has finished; while the child runs
//! there is no value a caller can hold. So "which subagents did this session start" cannot be read
//! back from the delegation API — it has to be written down by whoever calls it. That is why the
//! spawn tool here is a *traced wrapper*: it calls the same `delegate` that `subagent` calls and
//! records what it started. Composition rather than modification, so `subagent.rs` keeps its one job.
//!
//! # Why the registry mints its own ids
//!
//! `delegate` derives a child's [`AgentId`] from the parent's id and the depth (`parent/1`), so two
//! sequential delegations from one parent produce the *same* child id. That is fine as a log
//! correlation and useless as a handle, so each tracked spawn gets a registry id (`sa-0`, `sa-1`, …)
//! and the child id is carried as an attribute of the entry.
//!
//! # What "running" can and cannot mean here
//!
//! A traced spawn runs the child to completion inside one tool call, so from the *same* agent a
//! `subagent_wait` never has anything to wait for: by the time the spawn call returns, the child is
//! finished. A `running` entry is observable only from another task — a UI, or a second agent that
//! shares the registry — and that is also the only situation in which `subagent_wait` blocks. This is
//! a property of the delegation API, not of this module, and the tool descriptions say so rather than
//! letting a model discover it as a timeout.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use girder_core::{
    AgentId, DelegationResult, Depth, LoopContext, SubagentError, ToolDefinition, ToolRegistry,
    ToolResult, ToolRunContext, delegate,
};

/// The id prefix for a tracked spawn.
const ID_PREFIX: &str = "sa-";

/// How many characters of a prompt a listing keeps before eliding it.
///
/// A listing is read to choose an id, not to re-read the task, and a full prompt would push the other
/// rows out of view.
const DESCRIPTION_CHARS: usize = 80;

/// How long `subagent_wait` waits when the call does not say.
pub const DEFAULT_WAIT_MS: u64 = 30_000;

/// The longest a single `subagent_wait` may block.
///
/// A tool call holds the turn open, so an unbounded wait would pin the agent until the child finished
/// or the process ended.
pub const MAX_WAIT_MS: u64 = 600_000;

/// The shortest wait worth taking, so a caller that passes `0` still yields once.
pub const MIN_WAIT_MS: u64 = 100;

/// What a listing says when nothing was ever delegated.
const NO_SUBAGENTS: &str =
    "no subagents have been spawned in this session; the `subagent` tool starts one";

/// Where a tracked spawn is in its life.
///
/// Only three states, because a traced spawn cannot be "suspended": `delegate` either is still running
/// it or has returned. There is no resumable child to describe as inactive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The child is being run by a spawn call that has not returned yet.
    Running,

    /// The child returned an answer.
    Finished {
        /// The child agent's identity, which is `parent/depth` and therefore **not** unique.
        child_id: AgentId,
        /// How many turns it took.
        turns: u32,
    },

    /// The delegation failed, including a cancellation.
    Failed {
        /// What went wrong, as the parent's model would be told.
        reason: String,
    },
}

impl Status {
    /// The wire-ish label a listing prints.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished { .. } => "finished",
            Self::Failed { .. } => "failed",
        }
    }

    /// Whether the child will never change state again.
    ///
    /// A wait ends here, and only here: `running` is the state a wait is waiting to leave.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One subagent this session started.
///
/// Entries are minted by [`SubagentRegistry`] and are read-only to a caller: an entry is a snapshot,
/// and mutating a snapshot would change nothing that `wait` observes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentEntry {
    /// The registry handle, which is what `subagent_wait` and `subagent_send` take.
    pub id: String,
    /// The prompt, collapsed and elided, for a listing.
    pub description: String,
    /// The depth the child ran at.
    pub depth: Depth,
    /// Where it is.
    pub status: Status,
    /// Its final text, when it produced one.
    ///
    /// Kept separately from [`Self::status`] so "finished with no answer" stays distinguishable from
    /// "finished with an empty answer", which `delegate` itself folds into one error.
    pub answer: Option<String>,
    /// Spawn order, which is the order a listing uses.
    ///
    /// Private because it is ordering bookkeeping, not something a caller should key on: the id is
    /// the handle.
    seq: u64,
}

impl SubagentEntry {
    /// Whether the child is still being run.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.status.is_terminal()
    }

    /// The child's final text, when there is one.
    #[must_use]
    pub fn answer(&self) -> Option<&str> {
        self.answer.as_deref()
    }
}

impl std::fmt::Display for SubagentEntry {
    /// Renders one listing row: handle, state, description, and the detail that differs per state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} [{}] — {}",
            self.id,
            self.status.label(),
            self.description
        )?;
        match &self.status {
            Status::Running => Ok(()),
            Status::Finished { child_id, turns } => {
                write!(f, " (child {}, {turns} turn(s))", child_id.as_str())
            }
            Status::Failed { reason } => write!(f, " ({reason})"),
        }
    }
}

/// Why a wait did not produce an answer.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WaitError {
    /// No tracked spawn has that id.
    #[error("no subagent named `{id}` in this session")]
    Unknown {
        /// The id that was asked for.
        id: String,
    },

    /// The wait expired while the child was still running.
    #[error("`{id}` was still running after {millis}ms")]
    Timeout {
        /// The id that was asked for.
        id: String,
        /// How long the wait allowed.
        millis: u64,
    },

    /// The caller cancelled.
    #[error("cancelled while waiting for `{id}`")]
    Cancelled {
        /// The id that was asked for.
        id: String,
    },
}

/// One registry slot: the entry and the signal a waiter blocks on.
struct Slot {
    entry: SubagentEntry,
    /// Publishes every state change. A `watch` rather than a `Notify` because a waiter that arrives
    /// after the child finished must still see the result: `Notify` stores a permit at most, which is
    /// enough to wake one late waiter but says nothing about *what* happened.
    tx: watch::Sender<SubagentEntry>,
}

/// What this session's delegations did, in spawn order.
///
/// Shared as an `Arc` between the spawn tool that writes and the listing and wait tools that read.
/// Interior mutability rather than `&mut self` because those are separate tool bodies, each holding
/// its own clone of the handle.
/// How many subagents may run at once when nobody says otherwise.
///
/// Eight, matching upstream: enough for a fan-out a person would actually ask for, small enough that the provider's
/// rate limit is not split beyond usefulness.
pub const DEFAULT_CONCURRENCY: usize = 8;

pub struct SubagentRegistry {
    slots: std::sync::Mutex<BTreeMap<String, Slot>>,
    next_seq: AtomicU64,

    /// How many may run at once.
    ///
    /// A registry with no bound answers "how many are running" truthfully while letting the caller start a hundred
    /// provider streams, which is a fan-out the process-wide rate limit then gets divided across. The bound belongs
    /// here because this is where a delegation is recorded, and a limit enforced somewhere else would be a second
    /// place for the count to disagree.
    limit: usize,

    /// The tokens that stop each running child.
    ///
    /// Held here rather than by the caller because the id is minted here: an interrupter that had to be told about a
    /// child separately from the registry that tracks it would be told about it by every caller, or by none. Before this
    /// field existed, `Cancellers::arm` had no production caller and the interrupt tool always answered `Unknown`.
    cancellers: Arc<girder_core::Cancellers>,
}

impl std::fmt::Debug for SubagentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentRegistry")
            .field("tracked", &self.entries().len())
            .finish()
    }
}

/// Why a spawn was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartRefusal {
    /// The bound is reached.
    AtCapacity {
        /// How many may run.
        limit: usize,
        /// How many are running.
        active: usize,
    },
}

impl std::fmt::Display for StartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtCapacity { limit, active } => write!(
                f,
                "{active} subagent(s) already running and the limit is {limit}; wait for one to finish"
            ),
        }
    }
}

impl std::error::Error for StartRefusal {}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentRegistry {
    /// An empty registry that allows the default concurrency.
    #[must_use]
    pub fn new() -> Self {
        Self::with_concurrency(DEFAULT_CONCURRENCY)
    }

    /// An empty registry that allows `limit` subagents at once.
    ///
    /// A limit of zero is raised to one rather than refused: a registry that can never admit anything is a
    /// configuration mistake whose only symptom is every delegation failing, and the error would be several layers
    /// away from the cause.
    #[must_use]
    pub fn with_concurrency(limit: usize) -> Self {
        Self {
            slots: std::sync::Mutex::new(BTreeMap::new()),
            next_seq: AtomicU64::new(0),
            limit: limit.max(1),
            cancellers: Arc::new(girder_core::Cancellers::new()),
        }
    }

    /// How many may run at once.
    #[must_use]
    pub fn concurrency(&self) -> usize {
        self.limit
    }

    /// How many are running now.
    #[must_use]
    pub fn active(&self) -> usize {
        self.entries()
            .iter()
            .filter(|entry| matches!(entry.status, Status::Running))
            .count()
    }

    /// Whether a further delegation is allowed.
    ///
    /// A cheap question for a caller deciding whether to offer the tool at all, as [`Self::try_record_start`] does
    /// before it takes the lock. It is a snapshot: between this answer and the attempt, another caller may take the
    /// last slot, which is why the admission itself re-counts under the lock.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.active() < self.limit
    }

    /// An empty registry behind a shareable handle.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Records a spawn that is about to happen and returns its handle.
    ///
    /// Called *before* the child runs, so an observer that lists while the child works sees it. The
    /// caller must follow with [`Self::record_finished`] or [`Self::record_failed`], or the entry
    /// stays `running` for the life of the process.
    /// # Errors
    ///
    /// Returns [`StartRefusal::AtCapacity`] when the bound is already reached. Refusing here rather than letting the
    /// caller start a hundred is the point of keeping the count in one place: a limit checked anywhere else would be a
    /// second count that could disagree with this one.
    pub fn try_record_start(
        &self,
        description: &str,
        depth: Depth,
    ) -> Result<String, StartRefusal> {
        // Checked and recorded under one lock, so two callers racing cannot both see room and both take it.
        let mut slots = self.lock();
        // Counted from the map under the same lock the insert takes, rather than through `active()`, which would take
        // the lock a second time and could see a different count than the one it inserts against.
        let active = slots
            .values()
            .filter(|slot| matches!(slot.entry.status, Status::Running))
            .count();
        if active >= self.limit {
            return Err(StartRefusal::AtCapacity {
                limit: self.limit,
                active,
            });
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let entry = SubagentEntry {
            id: format!("{ID_PREFIX}{seq}"),
            description: summarize(description),
            depth,
            status: Status::Running,
            answer: None,
            seq,
        };
        let id = entry.id.clone();
        let (tx, _rx) = watch::channel(entry.clone());
        slots.insert(id.clone(), Slot { entry, tx });
        // Armed under the same lock the slot was inserted under, so an interrupt that arrives right after this returns
        // finds a token rather than racing the arming and reporting `Unknown` for a child that exists.
        let _ = self.cancellers.arm(id.clone());
        Ok(id)
    }

    /// Records a spawn, ignoring the bound.
    ///
    /// Kept for a caller that has already admitted the work by some other route; new code should prefer
    /// [`Self::try_record_start`], which cannot exceed the limit.
    pub fn record_start(&self, description: &str, depth: Depth) -> String {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let entry = SubagentEntry {
            id: format!("{ID_PREFIX}{seq}"),
            description: summarize(description),
            depth,
            status: Status::Running,
            answer: None,
            seq,
        };
        let id = entry.id.clone();
        // `watch::channel` needs no runtime, so a registry can be built and recorded into outside a
        // tokio context; only `wait` needs one.
        let (tx, _rx) = watch::channel(entry.clone());
        self.lock().insert(id.clone(), Slot { entry, tx });
        id
    }

    /// Records that the child finished, with what it said.
    ///
    /// Returns `false` when the handle is unknown, which a caller that lost its id should treat as a
    /// bug rather than a condition to retry.
    pub fn record_finished(&self, id: &str, result: &DelegationResult) -> bool {
        let answer = if result.text.trim().is_empty() {
            None
        } else {
            Some(result.text.clone())
        };
        let status = Status::Finished {
            child_id: result.child_id.clone(),
            turns: result.turns,
        };
        self.update(id, |entry| {
            entry.status = status;
            entry.answer = answer;
        })
    }

    /// Records that the delegation failed.
    ///
    /// A failure is terminal and visible: an entry that vanished would make "the subagent crashed"
    /// indistinguishable from "nothing was ever spawned", and the model would delegate again.
    pub fn record_failed(&self, id: &str, reason: impl Into<String>) -> bool {
        let reason = reason.into();
        self.update(id, |entry| {
            entry.status = Status::Failed {
                reason: reason.clone(),
            };
        })
    }

    /// Every entry, in spawn order.
    #[must_use]
    pub fn entries(&self) -> Vec<SubagentEntry> {
        let mut entries: Vec<SubagentEntry> = {
            let slots = self.lock();
            slots.values().map(|slot| slot.entry.clone()).collect()
        };
        // Sorted by spawn order rather than by id: `sa-10` sorts before `sa-2` as a string, and a
        // listing that reorders itself at the tenth child is confusing to read.
        entries.sort_by_key(|entry| entry.seq);
        entries
    }

    /// One entry by handle.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<SubagentEntry> {
        self.lock().get(id).map(|slot| slot.entry.clone())
    }

    /// Waits for one child to reach a terminal state.
    ///
    /// Returns immediately when the child already finished, which is the common case for a child
    /// spawned by a traced `subagent` call: that call does not return until the child is done.
    ///
    /// # Deadlock note
    ///
    /// A traced spawn occupies the calling agent's turn, so an agent that spawned a child and then
    /// waits on it from the *same* turn can only ever time out — the spawn has already returned
    /// before the wait is reached, so in practice the wait finds a finished entry. A `running` entry
    /// belongs to a spawn on another task, and that task is what publishes the result.
    ///
    /// # Errors
    ///
    /// [`WaitError::Unknown`] when no entry has the id, [`WaitError::Timeout`] when the child is
    /// still running at the deadline, and [`WaitError::Cancelled`] when `cancel` fires first.
    pub async fn wait(
        &self,
        id: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<SubagentEntry, WaitError> {
        let mut rx = {
            let slots = self.lock();
            let Some(slot) = slots.get(id) else {
                return Err(WaitError::Unknown { id: id.to_owned() });
            };
            if slot.entry.status.is_terminal() {
                return Ok(slot.entry.clone());
            }
            slot.tx.subscribe()
        };

        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                () = &mut deadline => {
                    return Err(WaitError::Timeout {
                        id: id.to_owned(),
                        millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    });
                }
                () = cancel.cancelled() => {
                    return Err(WaitError::Cancelled { id: id.to_owned() });
                }
                changed = rx.changed() => {
                    // A closed channel means every sender is gone, so no further state can arrive.
                    // Reporting the last known state is better than waiting out the timeout for a
                    // signal that cannot come.
                    if changed.is_err() {
                        return Ok(rx.borrow().clone());
                    }
                    let entry = rx.borrow().clone();
                    if entry.status.is_terminal() {
                        return Ok(entry);
                    }
                }
            }
        }
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut SubagentEntry)) -> bool {
        let mut slots = self.lock();
        let Some(slot) = slots.get_mut(id) else {
            return false;
        };
        change(&mut slot.entry);
        // Published after the mutation, never before: a waiter woken by a half-applied entry would
        // see `running` and go back to sleep, and the real transition would be lost.
        let snapshot = slot.entry.clone();
        slot.tx.send_replace(snapshot);
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Builds the `subagent` tool with tracking.
///
/// Drop-in for [`crate::subagent::subagent_tool`]: same name, same arguments, same model-facing
/// refusals. The difference is that the spawn is recorded, which is what gives `subagent_list` and
/// `subagent_wait` something to report. Register this **instead of** `subagent_tool`, not as well;
/// both register under the name `subagent` and the later one wins.
///
/// The depth ceiling is checked before anything is recorded, so a refusal leaves no `running` ghost
/// behind for a later listing to show.
pub fn traced_subagent_tool(
    context: Arc<LoopContext>,
    max_depth: u32,
    registry: Arc<SubagentRegistry>,
) -> ToolDefinition {
    ToolDefinition::new(
        "subagent",
        "Delegates a self-contained task to a child agent and returns its answer. \
         The spawn is tracked: `subagent_list` shows what this session started and `subagent_wait` \
         collects a result.",
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
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let context = Arc::clone(&context);
            let registry = Arc::clone(&registry);
            Box::pin(async move {
                let Some(prompt) = args
                    .get("prompt")
                    .and_then(Value::as_str)
                    .filter(|prompt| !prompt.trim().is_empty())
                else {
                    return Ok(ToolResult::error(
                        "`prompt` is required and must not be empty",
                    ));
                };

                // Checked before the registry is touched: a refused delegation must not leave an
                // entry that looks like work in progress.
                if !context.depth.can_delegate(max_depth) {
                    return Ok(ToolResult::error(format!(
                        "delegation refused: this agent is at depth {}, and the limit is \
                             {max_depth}. Do this work yourself instead of delegating.",
                        context.depth.get()
                    )));
                }

                // The bound is enforced here, where a delegation is actually dispatched. A depth limit alone constrained
                // how deep a chain could go and said nothing about how wide, so a model could start as many children as
                // it liked at the same depth -- each one a provider stream, each sharing one rate limit.
                // A cheap pre-check before the lock: the common case is a registry with room, and asking the question
                // twice costs less than taking the lock for every delegation that will be admitted anyway.
                if !registry.has_room() {
                    return Ok(ToolResult::error(format!(
                        "delegation refused: {} subagent(s) are running and this registry allows {}. Wait for one \
                         to finish, or do this work yourself.",
                        registry.active(),
                        registry.concurrency()
                    )));
                }
                let ticket = match registry.try_record_start(prompt, context.depth) {
                    Ok(ticket) => ticket,
                    Err(refusal) => {
                        return Ok(ToolResult::error(format!(
                            "delegation refused: {refusal}. This registry allows {} at a time and {} are \
                             running; do this work yourself, or wait for one to finish.",
                            registry.concurrency(),
                            registry.active()
                        )));
                    }
                };
                let outcome = delegate(
                    &context,
                    prompt.to_owned(),
                    context.depth,
                    max_depth,
                    ctx.cancel.clone(),
                )
                .await;

                match outcome {
                    Ok(result) => {
                        registry.record_finished(&ticket, &result);
                        Ok(result.to_tool_result())
                    }
                    Err(error) => {
                        registry.record_failed(&ticket, error.to_string());
                        Ok(ToolResult::error(describe(&error)))
                    }
                }
            })
        }),
    )
}

/// Builds the `subagent_list` tool.
pub fn subagent_list_tool(registry: Arc<SubagentRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "subagent_list",
        "Lists every subagent this session has spawned, oldest first, with its id, description, and \
         state (running, finished, or failed). An id is what `subagent_wait` takes.",
        serde_json::json!({ "type": "object", "properties": {} }),
        Arc::new(move |_args: Value, _ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move {
                let entries = registry.entries();
                if entries.is_empty() {
                    return Ok(ToolResult::text(NO_SUBAGENTS));
                }
                let mut text = format!("{} subagent(s) tracked by this session:", entries.len());
                for entry in entries {
                    text.push('\n');
                    let _ = write!(text, "{entry}");
                }
                Ok(ToolResult::text(text))
            })
        }),
    )
}

/// Builds the `subagent_wait` tool.
pub fn subagent_wait_tool(registry: Arc<SubagentRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "subagent_wait",
        "Waits for one subagent to finish and returns its final answer. Returns at once when it has \
         already finished. A child started by this agent has always finished before the spawn call \
         returns, so this tool is useful for a child started elsewhere; when it times out the child \
         is still running.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id from `subagent_list`, such as `sa-0`"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "How long to wait, in milliseconds. Defaults to 30000, capped at 600000."
                }
            },
            "required": ["id"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move {
                let Some(id) = string_arg(&args, "id") else {
                    return Ok(ToolResult::error(
                        "`id` is required; call `subagent_list` for the ids this session spawned",
                    ));
                };
                let timeout = Duration::from_millis(wait_timeout_ms(&args));

                match registry.wait(id, timeout, &ctx.cancel).await {
                    Ok(entry) => Ok(wait_succeeded(&entry)),
                    Err(WaitError::Unknown { id }) => Ok(ToolResult::error(format!(
                        "no subagent named `{id}`; call `subagent_list` to see what this session \
                         spawned"
                    ))),
                    Err(WaitError::Timeout { id, millis }) => Ok(ToolResult::error(format!(
                        "`{id}` was still running after {millis}ms. It has not failed; it is simply \
                         not done. If it was spawned by this agent's own `subagent` call it cannot \
                         still be running, so check the id with `subagent_list`."
                    ))),
                    Err(WaitError::Cancelled { id }) => Ok(ToolResult::error(format!(
                        "cancelled while waiting for `{id}`"
                    ))),
                }
            })
        }),
    )
}

/// Builds the `subagent_send` tool.
///
/// The tool always refuses, and the refusal is the deliverable: it names every missing piece of the
/// delegation API. It is registered anyway so a model that tries to steer finds out *why* it cannot,
/// instead of concluding the harness has no notion of a subagent at all.
pub fn subagent_send_tool(registry: Arc<SubagentRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "subagent_send",
        "Not available in this build: sends a message to a subagent. The delegation API has no \
         channel into a running child, so the call returns an error explaining what is missing.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id from `subagent_list`"
                },
                "message": {
                    "type": "string",
                    "description": "What to say to the subagent"
                }
            },
            "required": ["id", "message"]
        }),
        Arc::new(move |args: Value, _ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move {
                let (Some(id), Some(_message)) =
                    (string_arg(&args, "id"), string_arg(&args, "message"))
                else {
                    return Ok(ToolResult::error(
                        "`id` and `message` are both required and must not be empty",
                    ));
                };

                // The registry lookup is what makes the refusal specific: whether the child is
                // running, finished, or unknown changes what the model should do next.
                let Some(entry) = registry.get(id) else {
                    return Ok(ToolResult::error(format!(
                        "no subagent named `{id}`; call `subagent_list` to see what this session \
                         spawned"
                    )));
                };
                Ok(ToolResult::error(steering_unavailable(&entry)))
            })
        }),
    )
}

/// Registers all four subagent tools on `tools`.
///
/// The spawn tool is registered under the name `subagent`, so a caller uses this **or**
/// [`crate::subagent::subagent_tool`] — not both. Registration order decides which one a model gets,
/// and a silently replaced tool is a confusing thing to debug.
pub fn register_subagent_tools(
    tools: &ToolRegistry,
    registry: Arc<SubagentRegistry>,
    context: Arc<LoopContext>,
    max_depth: u32,
) {
    tools.register(traced_subagent_tool(
        context,
        max_depth,
        Arc::clone(&registry),
    ));
    tools.register(subagent_list_tool(Arc::clone(&registry)));
    tools.register(subagent_wait_tool(Arc::clone(&registry)));
    tools.register(subagent_send_tool(registry));
    // The interrupt path needs a token per child, and the registry records liveness without holding one. `Cancellers`
    // is the seam: a deployment that cancels some other way implements `Interrupter` and registers that instead.
    // `subagent_interrupt` is registered by its own function rather than here: it needs a token per child, and the
    // registry records liveness without holding one. A deployment that stops work some other way calls
    // `register_subagent_interrupt` with its own `Interrupter` instead of taking this path at all.
}

/// The model-facing text for a delegation failure.
///
/// Deliberately the same wording as [`crate::subagent::subagent_tool`]: the traced tool is a drop-in,
/// so a model that learned the untraced one must not be surprised by a different sentence.
fn describe(error: &SubagentError) -> String {
    match error {
        SubagentError::DepthExceeded { depth, max } => format!(
            "delegation refused: this agent is at depth {depth}, and the limit is {max}. Do this \
             work yourself instead of delegating."
        ),
        SubagentError::Cancelled => "delegation cancelled".to_owned(),
        SubagentError::Child(error) => format!("the subagent failed: {error}"),
    }
}

/// Renders the outcome of a successful wait as the tool result the parent's model sees.
fn wait_succeeded(entry: &SubagentEntry) -> ToolResult {
    match entry.answer() {
        Some(answer) => ToolResult::text(answer.to_owned()),
        None => match &entry.status {
            Status::Finished { turns, .. } => ToolResult::error(format!(
                "`{}` finished after {turns} turn(s) without an answer",
                entry.id
            )),
            Status::Failed { reason } => {
                ToolResult::error(format!("`{}` failed: {reason}", entry.id))
            }
            Status::Running => ToolResult::error(format!("`{}` is still running", entry.id)),
        },
    }
}

/// The refusal `subagent_send` returns, naming every missing piece.
fn steering_unavailable(entry: &SubagentEntry) -> String {
    format!(
        "`subagent_send` is not available: this build has no channel into a child agent, so no \
         message can be delivered to `{}` (it is {}). What is missing:\n\
         - `subagent::delegate` is a one-shot future: it returns the child's final answer and \
           exposes no handle while the child runs.\n\
         - the child `LoopContext` is built with `agent: None` and has no steering field, so there \
           is no `Agent` inbox to splice a message into.\n\
         - the child's `MemorySession` is created inside `delegate` and dropped when it returns, so \
           a finished child's transcript cannot be continued either.\n\
         - `subagent_providers::SubagentProvider` declares a `steering` capability, but no trait \
           method uses it.\n\
         Use `subagent_list` to see what this session started and `subagent_wait` to collect a \
         result.",
        entry.id,
        entry.status.label()
    )
}

/// Reads a non-empty trimmed string argument.
fn string_arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Reads `timeout_ms`, clamped so one call cannot hold a turn open indefinitely.
///
/// Public because the bound is part of the tool's contract: a caller that wants to assert "a huge
/// timeout is clamped" should not have to wait out `u64::MAX` to find out.
#[must_use]
pub fn wait_timeout_ms(args: &Value) -> u64 {
    args.get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WAIT_MS)
        .clamp(MIN_WAIT_MS, MAX_WAIT_MS)
}

/// Collapses a prompt onto one line and elides it.
///
/// Whitespace is collapsed because a prompt is usually several lines and a listing row is one: a raw
/// newline would break the row into something that reads like another entry.
fn summarize(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= DESCRIPTION_CHARS {
        return collapsed;
    }
    let mut elided: String = collapsed.chars().take(DESCRIPTION_CHARS).collect();
    elided.push('…');
    elided
}
