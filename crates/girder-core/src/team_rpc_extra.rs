//! `team.checkTaskGraph`: the dependency validator, reachable from a client.
//!
//! [`crate::team_gap`] holds the team's whole domain — task graphs, write scopes, ownership — and
//! nothing registered any of it as a Remote. The validator is the piece a client needs before it
//! submits a plan: a task graph with a dependency cycle is accepted by every individual edit and
//! then never starts, so the refusal has to be available where the edit is made.
//!
//! # Why the graph travels rather than the edit
//!
//! A cycle is a property of the *whole* graph, not of the edge being added. Checking one candidate
//! against the current set is what makes the answer correct without the server holding a copy, and
//! it is what lets a client validate before it sends anything.

use serde_json::{Value, json};

use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::team_gap::{
    TeamTaskGraphError, TeamTaskId, TeamTaskSnapshot, TeamTaskStatus, assert_task_graph_candidate,
};

/// The namespace `team.checkTaskGraph` lives in.
pub const NAMESPACE: &str = "team";

/// The method name.
pub const METHOD: &str = "checkTaskGraph";

/// The largest graph one call may carry.
///
/// A cycle check is quadratic in the worst case, and a client that sent a hundred thousand tasks
/// would turn a validation call into a denial of service against itself.
pub const MAX_TASKS: usize = 2_000;

/// Reads one task snapshot from its wire shape.
fn task_from_wire(value: &Value) -> Result<TeamTaskSnapshot, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "a task must be an object".to_owned())?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("a task needs a string `{key}`"))
    };
    let id = text("id")?;
    let status = match text("status")? {
        "pending" => TeamTaskStatus::Pending,
        "inProgress" | "in_progress" => TeamTaskStatus::InProgress,
        "completed" => TeamTaskStatus::Completed,
        "deleted" => TeamTaskStatus::Deleted,
        other => return Err(format!("unknown task status `{other}`")),
    };
    let blocked_by = object
        .get("blockedBy")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(TeamTaskSnapshot {
        id: TeamTaskId::new(id.to_owned()).map_err(|error| format!("{error}"))?,
        revision: object.get("revision").and_then(Value::as_u64).unwrap_or(1),
        subject: object
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        description: object
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        status,
        owner_id: None,
        owner_name: object
            .get("ownerName")
            .and_then(Value::as_str)
            .map(str::to_owned),
        blocked_by: blocked_by
            .into_iter()
            .map(|id| TeamTaskId::new(id).map_err(|error| format!("{error}")))
            .collect::<Result<Vec<_>, _>>()?,
        write_scopes: object
            .get("writeScopes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Renders a validator refusal so a client can branch on the reason.
fn refusal(error: &TeamTaskGraphError) -> Value {
    // The code is what a client branches on; the message is what a person reads. Both travel,
    // because a client that only had the message would parse prose to tell "cycle" from "missing".
    json!({ "ok": false, "code": error.code_str(), "reason": error.message })
}

/// Registers `team.checkTaskGraph`.
pub fn register_team_extra_methods(registry: &mut RemoteRegistry) {
    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        Box::pin(async move {
            let tasks = call
                .args
                .get("tasks")
                .and_then(Value::as_array)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: METHOD.to_owned(),
                    message: "`tasks` must be an array".to_owned(),
                })?;
            if tasks.len() > MAX_TASKS {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: METHOD.to_owned(),
                    message: format!("at most {MAX_TASKS} tasks, got {}", tasks.len()),
                });
            }
            let current = tasks
                .iter()
                .map(task_from_wire)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|message| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: METHOD.to_owned(),
                    message,
                })?;
            // The candidate is the graph *including* the edit, so the answer covers the whole
            // property rather than the edge. A `candidate` argument is accepted for the shape a
            // client naturally sends, but the check runs over `tasks` alone.
            let candidate = match call.args.get("candidate") {
                Some(value) => Some(task_from_wire(value).map_err(|message| {
                    RemoteError::InvalidArguments {
                        namespace: NAMESPACE.to_owned(),
                        method: METHOD.to_owned(),
                        message,
                    }
                })?),
                None => None,
            };
            // Either the caller named the edit, or the graph's last task is the edit. The second
            // shape is what a client that appends one task naturally sends.
            let (graph, candidate) = if let Some(candidate) = candidate {
                (current.clone(), candidate)
            } else {
                let Some(last) = current.last().cloned() else {
                    return Ok(json!({ "ok": true }));
                };
                (current[..current.len() - 1].to_vec(), last)
            };
            if let Err(error) = assert_task_graph_candidate(&graph, &candidate) {
                return Ok(refusal(&error));
            }
            Ok(json!({ "ok": true }))
        })
    });
    registry.declare(NAMESPACE, METHOD, crate::access::Declaration::read("team"));
}

/// The task ids a graph names, in the order they first appear.
///
/// Exposed because a client rendering the graph needs the same order the validator walked, and two
/// implementations of "first appearance" is how a UI and a refusal come to disagree.
#[must_use]
pub fn graph_ids(tasks: &[TeamTaskSnapshot]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for task in tasks {
        if seen.insert(task.id.as_str().to_owned()) {
            out.push(task.id.as_str().to_owned());
        }
    }
    out
}

/// The id type this module re-exports so a caller need not reach into `team_gap`.
pub use crate::team_gap::TeamTaskId as Id;
