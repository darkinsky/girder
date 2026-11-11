//! The `goal` tool: the model reading and editing the session's persisted objective.
//!
//! A goal is what a long task is *for*. It outlives a turn, so a model that lost its context can ask what it was
//! doing and why, and the answer comes from a record rather than from a re-read of the conversation.
//!
//! # Why the operations are one tool with an action
//!
//! Five separate tools — create, pause, resume, complete, block — would take five slots in every schema list, and a
//! model that was told `goal_create` exists would still need `goal_pause` to be discoverable. One tool with a closed
//! action set keeps the surface small and the actions enumerable, at the cost of a longer parameter list for the
//! actions that take one.
//!
//! # Why a refusal is a result
//!
//! Resuming a goal that is already complete is a normal mistake, and the model can correct it. An error would end the
//! turn and lose the rest of what it was doing.
//!
//! # Why `status` is separate from the mutations
//!
//! Reading is what a model does most: it asks what it is working on far more often than it changes it. Folding the
//! read into every action would make the common case pay for the rare one.

use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::goal::{Activation, BlockReason, GoalId, GoalRef, GoalState};
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// Builds the `goal` tool.
///
/// The state is shared rather than owned so the loop and the tool see one goal: a tool that kept its own copy would
/// let the model complete a goal the loop still believed was open.
#[must_use]
pub fn goal_tool(state: Arc<Mutex<GoalState>>) -> ToolDefinition {
    ToolDefinition::new(
        "goal",
        "Reads or changes the objective this session is working toward",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "create", "pause", "resume", "complete", "block"],
                    "description": "What to do; `status` only reads"
                },
                "objective": {
                    "type": "string",
                    "description": "What to accomplish; required for `create`"
                },
                "reason": {
                    "type": "string",
                    "description": "Why it is blocked; required for `block`"
                },
                "max_rounds": {
                    "type": "number",
                    "description": "How many continuation rounds `create` allows"
                }
            },
            "required": ["action"]
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let state = Arc::clone(&state);
            Box::pin(async move { Ok(goal(&args, &state)) })
        }),
    )
}

/// Runs one goal action.
///
/// Synchronous: the state is a mutex around small values, and making this `async` would hold the guard across an await
/// point on a future that has nothing to await.
fn goal(args: &serde_json::Value, state: &Arc<Mutex<GoalState>>) -> ToolResult {
    let action = args
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let Ok(mut state) = state.lock() else {
        return ToolResult::error("the goal state is unavailable");
    };

    match action {
        "status" => ToolResult::text(describe(&state)),
        "create" => {
            let objective = args
                .get("objective")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if objective.trim().is_empty() {
                return ToolResult::error("`objective` is required for `create`");
            }
            let max_rounds = args
                .get("max_rounds")
                .and_then(serde_json::Value::as_u64)
                .and_then(|rounds| u32::try_from(rounds).ok());

            // The id is derived from the objective's digest rather than a counter: a goal restored after a restart
            // keeps the id a client already knows, and two goals with one objective are the same intent.
            let Some(id) = GoalId::new(format!("goal-{}", crate::instructions::digest(objective)))
            else {
                return ToolResult::error("the goal id could not be built");
            };

            match state.create(id, objective, max_rounds, crate::schedule::now_millis()) {
                Ok(goal) => ToolResult::text(format!(
                    "created goal `{}`: {}\n\n{}",
                    goal.id,
                    goal.objective,
                    describe(&state)
                )),
                Err(error) => ToolResult::error(format!("cannot create the goal: {error}")),
            }
        }
        action @ ("pause" | "resume" | "complete" | "block") => {
            let Some(reference) = current_reference(&state) else {
                return ToolResult::error("there is no goal to change");
            };

            let result = match action {
                "pause" => state.pause(&reference).map(|_| "paused".to_owned()),
                "resume" => state.resume(&reference).map(|_| "resumed".to_owned()),
                "complete" => state.complete(&reference).map(|_| "completed".to_owned()),
                _ => {
                    let reason = args
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if reason.trim().is_empty() {
                        return ToolResult::error("`reason` is required for `block`");
                    }
                    match BlockReason::new("blocked", reason) {
                        Some(reason) => state
                            .block(&reference, reason)
                            .map(|_| "blocked".to_owned()),
                        None => return ToolResult::error("the reason must not be blank"),
                    }
                }
            };

            match result {
                Ok(what) => ToolResult::text(format!("goal {what}\n\n{}", describe(&state))),
                Err(error) => ToolResult::error(format!("cannot {action} the goal: {error}")),
            }
        }
        "" => ToolResult::error("`action` is required"),
        other => ToolResult::error(format!(
            "`{other}` is not a goal action; expected status, create, pause, resume, complete, or block"
        )),
    }
}

/// The current goal's reference, when there is one.
fn current_reference(state: &GoalState) -> Option<GoalRef> {
    state.current().map(crate::goal::Goal::reference)
}

/// Describes the current goal for a model.
///
/// A `String` rather than a `ToolResult` so the readers above can embed it: a status call returns it alone, and a
/// mutation appends it to what changed.
fn describe(state: &GoalState) -> String {
    let Some(goal) = state.current() else {
        return "no goal is set".to_owned();
    };

    // The activation is shown because it decides whether the loop continues: a model told "disarmed" knows its next
    // turn is the last, and one that did not know would plan work it will not get to do.
    let activation = match state.activation() {
        Activation::Armed => "armed",
        Activation::Disarmed => "disarmed",
    };

    format!(
        "goal `{}` ({activation}, {}/{} rounds): {}{}",
        goal.id,
        goal.rounds_started,
        goal.max_rounds,
        goal.objective,
        goal.blocked_reason
            .as_ref()
            .map(|reason| format!("\n\nblocked: {}", reason.message))
            .unwrap_or_default()
    )
}
