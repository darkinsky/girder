//! The `subagent_interrupt` tool: ask one tracked subagent to stop what it is doing.
//!
//! [`crate::subagent_tools`] can start a child, list it, and wait for it, but nothing can tell a
//! child to stop. This module is that missing verb.
//!
//! # Why this is a request and not a kill
//!
//! A running child is in the middle of something: writing a file, holding a tool call open, part-way
//! through appending a log record. Cutting it down where it stands leaves a half-applied change that
//! nothing downstream can distinguish from a complete one. So the tool *asks* the child to stop,
//! returns immediately, and reports that the request was made rather than that the child has
//! stopped. [`InterruptOutcome::Requested`] is named for exactly that gap: a caller that needs to
//! know the child actually stopped calls `subagent_wait` afterwards.
//!
//! # Why a finished child is not an error
//!
//! "Stop that child" has two satisfying answers: it stopped, or it had already stopped. Reporting
//! the second as a failure pushes a model into retrying a call that can never succeed, or into
//! concluding its own bookkeeping is corrupt. [`InterruptOutcome::AlreadyFinished`] carries the state
//! the child actually reached, so the caller can move on with a fact instead of an error.
//!
//! # Why [`Interrupter`] is a trait
//!
//! The mechanism that can cancel a child is a `CancellationToken` owned by the runtime that started
//! it. That token is not reachable from the registry, and this module has no business reaching into
//! the runtime for it. Depending on the trait instead keeps this file testable with no runtime at
//! all, and lets the wiring supply the real cancellation later without changing the tool.
//!
//! # Why the registry is consulted before the interrupter
//!
//! The registry is this session's record of what was spawned, so "there is no such id" and "it is
//! already done" are answerable from a snapshot, without touching cancellation machinery that may
//! have nothing to cancel. The interrupter's answer is still honoured afterwards rather than
//! discarded: a child can finish between the lookup and the request, and reporting the snapshot
//! over the interrupter's fresher answer would claim a request that was never made.

#![allow(dead_code)]
use std::sync::Arc;

use serde_json::Value;

use girder_core::{SubagentRegistry, ToolDefinition, ToolResult, ToolRunContext};

/// The name the model calls.
const TOOL_NAME: &str = "subagent_interrupt";

/// What asking a subagent to stop produced.
///
/// The three variants are deliberately not a `Result`: two of them are normal answers to the
/// question, and only [`Self::Unknown`] is a caller mistake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterruptOutcome {
    /// The stop was requested. The child may still be working; it stops at its next safe point.
    Requested,

    /// The subagent had already ended, so there was nothing to interrupt.
    AlreadyFinished {
        /// The state it reached, as its listing labels it: `finished` or `failed`.
        status: String,
    },

    /// No tracked spawn has this id.
    Unknown {
        /// The id that was asked for.
        id: String,
    },
}

/// Asks one subagent to stop.
///
/// Implementations are expected to signal cancellation and return, never to block until the child
/// unwinds: the child may be mid-tool-call, and a tool call that waits for that is a tool call that
/// can hang the turn that issued it.
pub trait Interrupter: Send + Sync {
    /// Requests that the subagent with `id` stop its current work.
    fn interrupt(&self, id: &str) -> InterruptOutcome;
}

/// Builds the `subagent_interrupt` tool.
///
/// The description warns that an interrupt discards unfinished work, because that is the one
/// consequence a model cannot recover by reading the result: a stopped child has no answer to
/// collect, and `subagent_wait` will only confirm that it stopped.
pub fn subagent_interrupt_tool(
    registry: Arc<SubagentRegistry>,
    interrupter: Arc<dyn Interrupter>,
) -> ToolDefinition {
    ToolDefinition::new(
        TOOL_NAME,
        "Requests that a tracked subagent stop its current work. This is a request, not a kill: the \
         subagent stops at its next safe point, so this call returning does not mean it has \
         stopped. The subagent's unfinished work is discarded — anything it had not yet delivered \
         when it stops is lost, and it produces no answer. Returns immediately; use `subagent_wait` \
         afterwards to learn when it actually stopped. A subagent that already ended is reported as \
         such, not as an error. Ids come from `subagent_list`.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id from `subagent_list`, such as `sa-0`"
                }
            },
            "required": ["id"]
        }),
        Arc::new(move |args: Value, _ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            let interrupter = Arc::clone(&interrupter);
            Box::pin(async move {
                let Some(id) = string_arg(&args, "id") else {
                    return Ok(ToolResult::error(
                        "`id` is required; call `subagent_list` for the ids this session spawned",
                    ));
                };

                // Classified from the registry first: a snapshot answers "no such id" and "already
                // done" without asking a cancel mechanism to cancel nothing.
                let Some(entry) = registry.get(id) else {
                    return Ok(unknown_id(id));
                };
                if !entry.is_running() {
                    return Ok(already_finished(id, entry.status.label()));
                }

                match interrupter.interrupt(id) {
                    InterruptOutcome::Requested => Ok(ToolResult::json(serde_json::json!({
                        "id": id,
                        "outcome": "requested",
                        "note": "the request was made, not the stop; the subagent may still be \
                                 working until it reaches a safe point",
                    }))),
                    // The interrupter saw a state newer than the snapshot above. Its answer wins:
                    // whichever of the two raced, the caller is told what happened rather than what
                    // this tool intended to cause.
                    InterruptOutcome::AlreadyFinished { status } => {
                        Ok(already_finished(id, &status))
                    }
                    InterruptOutcome::Unknown { id } => Ok(unknown_id(&id)),
                }
            })
        }),
    )
}

/// The result for a subagent that had already ended.
///
/// Not an error: the caller's goal is satisfied, and the status tells it whether there is a result
/// to collect (a finished child has one) or a failure to report.
fn already_finished(id: &str, status: &str) -> ToolResult {
    ToolResult::json(serde_json::json!({
        "id": id,
        "outcome": "already_finished",
        "status": status,
        "note": "nothing was interrupted: this subagent has already stopped",
    }))
}

/// The result for an id no tracked spawn has.
fn unknown_id(id: &str) -> ToolResult {
    ToolResult::error(format!(
        "no subagent named `{id}`; call `subagent_list` to see what this session spawned"
    ))
}

/// Reads a required non-empty string argument.
///
/// A blank id is treated as absent rather than looked up: an empty string cannot name a tracked
/// spawn, and passing it through would report "no subagent named" with nothing after it instead of
/// telling the caller what it left out.
fn string_arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The default interrupter, for a deployment that has no cancellation mechanism of its own.
///
/// Returns the shared set so a caller can arm a child before starting it. A tool built without one cannot interrupt
/// anything, which is why this exists rather than making the caller find it.
#[must_use]
pub fn default_interrupter() -> Arc<dyn Interrupter> {
    Arc::new(Cancellers::new())
}

/// An interrupter backed by cancellation tokens the runtime holds.
///
/// The registry records that a subagent is running; it does not hold the token that stops it. Keeping the two apart is
/// what lets this module exist without the registry depending on a runtime, and what makes `Interrupter` the seam a
/// deployment with a different cancellation mechanism can implement.
#[derive(Default)]
pub struct Cancellers {
    tokens:
        std::sync::Mutex<std::collections::BTreeMap<String, tokio_util::sync::CancellationToken>>,
}

impl std::fmt::Debug for Cancellers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cancellers").finish_non_exhaustive()
    }
}

impl Cancellers {
    /// Creates an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the token that stops `id`, returning a guard that removes it on drop.
    ///
    /// A guard rather than a paired `forget`: a caller that returns early from an error path would otherwise leave a
    /// token in the map for a subagent that no longer exists, and the map would grow with every failure.
    pub fn arm(&self, id: impl Into<String>) -> Armed<'_> {
        let id = id.into();
        let token = tokio_util::sync::CancellationToken::new();
        self.lock().insert(id.clone(), token.clone());
        Armed {
            id,
            token,
            cancellers: self,
        }
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        std::collections::BTreeMap<String, tokio_util::sync::CancellationToken>,
    > {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Interrupter for Cancellers {
    fn interrupt(&self, id: &str) -> InterruptOutcome {
        // Cloned out of the map rather than cancelled under the lock: `cancel` wakes every waiter, and a waiter that
        // wanted to arm or disarm would deadlock on the same mutex.
        let token = self.lock().get(id).cloned();
        match token {
            Some(token) => {
                token.cancel();
                InterruptOutcome::Requested
            }
            None => InterruptOutcome::Unknown { id: id.to_owned() },
        }
    }
}

/// A registered token, removed when it goes out of scope.
///
/// Cancelling on drop as well as forgetting: a guard that only forgot would leave a subagent running when its scope
/// ended, which is the opposite of what the guard is for.
pub struct Armed<'a> {
    id: String,
    token: tokio_util::sync::CancellationToken,
    cancellers: &'a Cancellers,
}

impl Armed<'_> {
    /// The token to pass to the work this guard covers.
    #[must_use]
    pub fn token(&self) -> tokio_util::sync::CancellationToken {
        self.token.clone()
    }
}

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        self.token.cancel();
        self.cancellers.lock().remove(&self.id);
    }
}
