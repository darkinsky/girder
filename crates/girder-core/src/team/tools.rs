//! The four model-facing team task tools.
//!
//! Upstream registers them in `packages/team/tool-agent-team`. They read and write the session
//! log: each call replays `team/mode` and `team/task`, applies one board command, and appends the
//! revision the board committed. The in-memory board is dropped afterwards, so a lost append is a
//! lost command and a retry sees the same revision.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::core::tools::{ToolDefinition, ToolResult, ToolRunContext};
use crate::session::{Session, SessionEvent};
use crate::team::board::{
    Assignee, CreateTeamTaskRequest, DEFAULT_MAX_TASKS, MemoryJournal, TEAM_TASK_EVENT_VERSION,
    TeamMember, TeamTaskAction, TeamTaskBoard, TeamTaskFilter, TeamTaskId, TeamTaskJournal,
    TeamTaskStatus, UpdateTeamTaskRequest, replay_events,
};
use crate::util::brand::SessionId;

/// Guidance added to the system prompt while Agent Teams is on.
pub const TEAM_POLICY: &str = "Agent Teams is available in this session.\n\n\
The Team Lead and all teammates share the same working directory and filesystem. Edits are immediately visible to every member. Split write work into disjoint scopes, record expected write scopes on shared tasks, and use task dependencies when work must be ordered. Write-scope overlap is advisory, not a lock.\n\n\
Use the names returned by subagent, subagent_fork, or list_agents as owner when assigning or filtering shared tasks. The root can reassign tasks to itself or any continuable descendant; other agents claim ready tasks themselves. Shared-task workflow is list, get, claim with the current revision, perform the work, then complete. Task readiness never starts an owner. The root agent must wait for required teammates before giving the final answer.\n\n\
Task changes do not end wait_agent calls, add inbox messages, or start idle agents. Use send_message when another agent needs to act on a task.";

/// Live sessions the team tools can write, keyed by agent id.
#[derive(Default)]
pub struct TeamSessions {
    sessions: Mutex<HashMap<String, Arc<dyn Session>>>,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl TeamSessions {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `session` the one `agent` is running.
    pub fn bind(&self, agent: &str, session: Arc<dyn Session>) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(agent.to_owned(), session);
    }

    /// Forgets `agent` when its turn ends.
    pub fn unbind(&self, agent: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(agent);
    }

    fn session(&self, agent: &str) -> Option<Arc<dyn Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(agent)
            .cloned()
    }

    fn gate(&self, agent: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(agent.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// Removes the binding when the turn stream is dropped.
pub struct TeamBinding {
    gate: Arc<TeamSessions>,
    agent: String,
}

impl TeamBinding {
    /// Binds `session` for `agent` and unbinds it on drop.
    #[must_use]
    pub fn bind(gate: Arc<TeamSessions>, agent: &str, session: Arc<dyn Session>) -> Self {
        gate.bind(agent, session);
        Self {
            gate,
            agent: agent.to_owned(),
        }
    }
}

impl Drop for TeamBinding {
    fn drop(&mut self) {
        self.gate.unbind(&self.agent);
    }
}

/// Registers `team_task_create`, `team_task_list`, `team_task_get`, and `team_task_update`.
pub fn register_team_tools(tools: &crate::core::tools::ToolRegistry, gate: &Arc<TeamSessions>) {
    tools.register(create_tool(gate));
    tools.register(list_tool(gate));
    tools.register(get_tool(gate));
    tools.register(update_tool(gate));
}

fn create_tool(gate: &Arc<TeamSessions>) -> ToolDefinition {
    let gate = Arc::clone(gate);
    ToolDefinition::new(
        "team_task_create",
        "Create one unowned pending task on the shared Team task board.",
        json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string", "description": "Concise task title." },
                "description": { "type": "string", "description": "Complete task details and acceptance criteria." },
                "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Task ids that must complete first." },
                "write_scopes": { "type": "array", "items": { "type": "string" }, "description": "Advisory workspace-relative file or directory prefixes this task expects to modify." }
            },
            "required": ["subject", "description"]
        }),
        Arc::new(move |args, ctx| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let Some(subject) = required_string(&args, "subject") else {
                    return Ok(ToolResult::error("`subject` is required"));
                };
                let Some(description) = required_string(&args, "description") else {
                    return Ok(ToolResult::error("`description` is required"));
                };
                let blocked_by = match id_list(&args, "blocked_by") {
                    Ok(ids) => ids,
                    Err(error) => return Ok(ToolResult::error(error)),
                };
                let write_scopes = match string_list(&args, "write_scopes") {
                    Ok(scopes) => scopes,
                    Err(error) => return Ok(ToolResult::error(error)),
                };
                let request = CreateTeamTaskRequest {
                    subject,
                    description,
                    blocked_by,
                    write_scopes,
                };
                answer(&gate, &ctx, |board, caller| {
                    board.create(caller, request).map(|task| task.to_json())
                })
                .await
            })
        }),
    )
}

fn list_tool(gate: &Arc<TeamSessions>) -> ToolDefinition {
    let gate = Arc::clone(gate);
    ToolDefinition::new(
        "team_task_list",
        "List shared tasks, including readiness, owner, revision, blockers, and write-scope warnings.",
        json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Optional exact status filter." },
                "owner": { "type": "string", "description": "Optional owner agent name filter; cannot be combined with unowned." },
                "unowned": { "type": "boolean", "description": "Filter tasks by whether they have no owner; cannot be combined with owner." },
                "ready": { "type": "boolean", "description": "Optional readiness filter." },
                "cursor": { "type": "integer", "description": "Zero-based result offset. Defaults to 0." },
                "limit": { "type": "integer", "description": "Number of rows, 1 through 100. Defaults to 50." }
            }
        }),
        Arc::new(move |args, ctx| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let cursor = args.get("cursor").and_then(Value::as_i64).unwrap_or(0);
                let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(50);
                if cursor < 0 {
                    return Ok(ToolResult::error(
                        "cursor must be a non-negative safe integer",
                    ));
                }
                if !(1..=100).contains(&limit) {
                    return Ok(ToolResult::error(
                        "limit must be an integer from 1 through 100",
                    ));
                }
                let status = match args.get("status").and_then(Value::as_str) {
                    None => None,
                    Some(status) => match TeamTaskStatus::parse(status) {
                        Some(status) if status != TeamTaskStatus::Deleted => Some(status),
                        _ => {
                            return Ok(ToolResult::error(
                                "status must be pending, in_progress, or completed",
                            ));
                        }
                    },
                };
                answer(&gate, &ctx, |board, caller| {
                    let owner = match args.get("owner").and_then(Value::as_str) {
                        None => None,
                        Some(name) => Some(resolve_owner(board, caller, name)?),
                    };
                    let filter = TeamTaskFilter {
                        status,
                        owner,
                        unowned: args.get("unowned").and_then(Value::as_bool),
                        ready: args.get("ready").and_then(Value::as_bool),
                        cursor: u64::try_from(cursor).unwrap_or(u64::MAX),
                        limit: u16::try_from(limit).unwrap_or(100),
                    };
                    board.list_page(&filter).map(|page| page.to_json())
                })
                .await
            })
        }),
    )
}

fn get_tool(gate: &Arc<TeamSessions>) -> ToolDefinition {
    let gate = Arc::clone(gate);
    ToolDefinition::new(
        "team_task_get",
        "Read the complete latest value of one shared task before changing or executing it.",
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Shared task id." }
            },
            "required": ["task_id"]
        }),
        Arc::new(move |args, ctx| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let Some(task_id) = required_string(&args, "task_id") else {
                    return Ok(ToolResult::error("`task_id` is required"));
                };
                let task_id = match parse_task_id(&task_id) {
                    Ok(id) => id,
                    Err(error) => return Ok(ToolResult::error(error)),
                };
                answer(&gate, &ctx, |board, _caller| {
                    board.get(&task_id).map(|task| task.to_json())
                })
                .await
            })
        }),
    )
}

fn update_tool(gate: &Arc<TeamSessions>) -> ToolDefinition {
    let gate = Arc::clone(gate);
    ToolDefinition::new(
        "team_task_update",
        "Compare-and-set a shared task action using the latest revision from team_task_get or team_task_list.",
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Shared task id." },
                "expected_revision": { "type": "integer", "description": "Current task revision used as the CAS precondition." },
                "action": { "type": "string", "enum": ["claim", "release", "edit", "set_dependencies", "complete", "reopen", "reassign", "delete"], "description": "Task transition to apply." },
                "subject": { "type": "string", "description": "Replacement title for edit." },
                "description": { "type": "string", "description": "Replacement details for edit." },
                "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Complete blocker list for set_dependencies." },
                "write_scopes": { "type": "array", "items": { "type": "string" }, "description": "Replacement advisory write scopes for edit." },
                "owner": { "type": "string", "description": "Agent name of the root or any continuable descendant for root-only reassign; omit to unassign." }
            },
            "required": ["task_id", "expected_revision", "action"]
        }),
        Arc::new(move |args, ctx| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let Some(task_id) = required_string(&args, "task_id") else {
                    return Ok(ToolResult::error("`task_id` is required"));
                };
                let task_id = match parse_task_id(&task_id) {
                    Ok(id) => id,
                    Err(error) => return Ok(ToolResult::error(error)),
                };
                let Some(expected) = args.get("expected_revision").and_then(Value::as_u64) else {
                    return Ok(ToolResult::error("`expected_revision` is required"));
                };
                let Some(action) = args
                    .get("action")
                    .and_then(Value::as_str)
                    .and_then(TeamTaskAction::parse)
                else {
                    return Ok(ToolResult::error("`action` is not a team task transition"));
                };
                let blocked_by = match id_list(&args, "blocked_by") {
                    Ok(ids) => ids,
                    Err(error) => return Ok(ToolResult::error(error)),
                };
                let write_scopes = match string_list(&args, "write_scopes") {
                    Ok(scopes) => scopes,
                    Err(error) => return Ok(ToolResult::error(error)),
                };
                let mut request = UpdateTeamTaskRequest::new(task_id, expected, action);
                request.subject = args
                    .get("subject")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                request.description = args
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if args.get("blocked_by").is_some() {
                    request.blocked_by = Some(blocked_by);
                }
                if args.get("write_scopes").is_some() {
                    request.write_scopes = Some(write_scopes);
                }
                let owner_name = args.get("owner").and_then(Value::as_str).map(str::to_owned);
                answer(&gate, &ctx, |board, caller| {
                    if action == TeamTaskAction::Reassign {
                        request.owner = Some(match owner_name.as_deref() {
                            None | Some("") => Assignee::Nobody,
                            Some(name) if name == caller.id.as_str() => {
                                Assignee::Member(caller.id.clone())
                            }
                            Some(name) => Assignee::Member(resolve_owner(board, caller, name)?),
                        });
                    }
                    let members = [caller.clone()];
                    board
                        .update(caller, &request, &members)
                        .map(|task| task.to_json())
                })
                .await
            })
        }),
    )
}

async fn answer(
    gate: &TeamSessions,
    ctx: &ToolRunContext,
    op: impl FnOnce(
        &mut TeamTaskBoard<MemoryJournal>,
        &TeamMember,
    ) -> Result<Value, crate::team::board::TeamError>,
) -> Result<ToolResult, crate::core::tools::ToolError> {
    let agent = ctx.execution.agent_id.as_str();
    let Some(session) = gate.session(agent) else {
        return Ok(ToolResult::error(
            "team tools require a running turn for this agent",
        ));
    };
    let lock = gate.gate(agent);
    let _guard = lock.lock().await;
    let events = match session.replay().await {
        Ok(events) => events,
        Err(error) => {
            return Ok(ToolResult::error(format!(
                "the session could not be read: {error}"
            )));
        }
    };
    let Ok(root) = SessionId::new(agent.to_owned()) else {
        return Ok(ToolResult::error("this agent id cannot own a team board"));
    };
    let state = replay_events(root.clone(), 0, team_records(&events));
    let mut board = match TeamTaskBoard::resume(state, DEFAULT_MAX_TASKS, MemoryJournal::new()) {
        Ok(board) => board,
        Err(error) => return Ok(ToolResult::error(error.to_string())),
    };
    let caller = TeamMember::new(root.clone());
    let value = match op(&mut board, &caller) {
        Ok(value) => value,
        Err(error) => return Ok(ToolResult::error(error.to_string())),
    };
    for task in board.journal().committed() {
        let encoded = match serde_json::to_value(task) {
            Ok(encoded) => encoded,
            Err(error) => {
                return Ok(ToolResult::error(format!(
                    "the task could not be stored: {error}"
                )));
            }
        };
        if let Err(error) = session
            .append(SessionEvent::TeamTask {
                version: TEAM_TASK_EVENT_VERSION,
                team_id: root.as_str().to_owned(),
                task: encoded,
            })
            .await
        {
            return Ok(ToolResult::error(format!(
                "task \"{}\" revision {} is visible, but persistence was not confirmed; read the task before retrying ({error})",
                task.id.as_str(),
                task.revision
            )));
        }
    }
    Ok(ToolResult::text(value.to_string()))
}

fn team_records(events: &[SessionEvent]) -> Vec<(u64, String, Value)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            let seq = u64::try_from(index).unwrap_or(u64::MAX);
            match event {
                SessionEvent::TeamMode { enabled } => {
                    Some((seq, "team/mode".to_owned(), json!({ "enabled": enabled })))
                }
                SessionEvent::TeamTask {
                    version,
                    team_id,
                    task,
                } => Some((
                    seq,
                    "team/task".to_owned(),
                    json!({ "version": version, "teamId": team_id, "task": task }),
                )),
                _ => None,
            }
        })
        .collect()
}

fn resolve_owner(
    board: &TeamTaskBoard<MemoryJournal>,
    caller: &TeamMember,
    name: &str,
) -> Result<SessionId, crate::team::board::TeamError> {
    if name == caller.id.as_str() {
        return Ok(caller.id.clone());
    }
    if let Some(owner) = board.state().tasks().iter().find_map(|task| {
        let same_id = task.owner_id.as_ref().is_some_and(|id| id.as_str() == name);
        let same_name = task.owner_name.as_deref() == Some(name);
        if same_id || same_name {
            task.owner_id.clone()
        } else {
            None
        }
    }) {
        return Ok(owner);
    }
    Err(crate::team::board::TeamError::new(
        crate::team::board::TeamErrorCode::MemberNotFound,
        format!("agent \"{name}\" is not a continuable descendant of this task board's root"),
    ))
}

fn required_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn string_list(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(format!("`{key}` must be an array of strings"));
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("`{key}` must be an array of strings"))
        })
        .collect()
}

fn id_list(args: &Value, key: &str) -> Result<Vec<TeamTaskId>, String> {
    string_list(args, key)?
        .into_iter()
        .map(|id| parse_task_id(&id))
        .collect()
}

fn parse_task_id(value: &str) -> Result<TeamTaskId, String> {
    if !crate::team::board::is_valid_task_id(value) {
        return Err(format!("`{value}` is not a team task id"));
    }
    TeamTaskId::new(value.to_owned()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tools::{ToolExecution, ToolRegistry};
    use crate::session::MemorySession;
    use crate::util::brand::{AgentId, ToolCallId};

    fn context(agent: &str) -> ToolRunContext {
        ToolRunContext::new(
            ToolExecution {
                call_id: ToolCallId::new("call-1").expect("literal"),
                name: "team_task_create".to_owned(),
                arguments: "{}".to_owned(),
                agent_id: AgentId::new(agent).expect("literal"),
            },
            tokio_util::sync::CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn a_lead_creates_claims_and_completes_one_task() {
        let session = Arc::new(MemorySession::new());
        session
            .append(SessionEvent::TeamMode { enabled: true })
            .await
            .expect("mode");
        let gate = Arc::new(TeamSessions::new());
        let tools = ToolRegistry::new();
        register_team_tools(&tools, &gate);
        let _binding = TeamBinding::bind(Arc::clone(&gate), "lead", session);

        let created = (tools.get("team_task_create").expect("create").execute)(
            json!({ "subject": "Write the note", "description": "One paragraph." }),
            context("lead"),
        )
        .await
        .expect("ran");
        assert!(!created.is_error, "{:?}", created.content);
        let text = match &created.content[0] {
            crate::llm::ContentBlock::Text { text } => text.clone(),
            _ => panic!("text"),
        };
        assert!(text.contains("\"id\":\"task-1\""), "{text}");
        assert!(text.contains("\"revision\":1"), "{text}");

        let claimed = (tools.get("team_task_update").expect("update").execute)(
            json!({ "task_id": "task-1", "expected_revision": 1, "action": "claim" }),
            context("lead"),
        )
        .await
        .expect("ran");
        assert!(!claimed.is_error, "{:?}", claimed.content);
        let finished = (tools.get("team_task_update").expect("update").execute)(
            json!({ "task_id": "task-1", "expected_revision": 2, "action": "complete" }),
            context("lead"),
        )
        .await
        .expect("ran");
        assert!(!finished.is_error, "{:?}", finished.content);

        let listed = (tools.get("team_task_list").expect("list").execute)(
            json!({ "status": "completed" }),
            context("lead"),
        )
        .await
        .expect("ran");
        let listed = match &listed.content[0] {
            crate::llm::ContentBlock::Text { text } => text.clone(),
            _ => panic!("text"),
        };
        assert!(listed.contains("task-1"), "{listed}");
        assert!(listed.contains("completed"), "{listed}");
    }

    #[tokio::test]
    async fn a_board_that_is_off_refuses_the_create() {
        let session = Arc::new(MemorySession::new());
        let gate = Arc::new(TeamSessions::new());
        let tools = ToolRegistry::new();
        register_team_tools(&tools, &gate);
        let _binding = TeamBinding::bind(Arc::clone(&gate), "lead", session);
        let created = (tools.get("team_task_create").expect("create").execute)(
            json!({ "subject": "Write the note", "description": "One paragraph." }),
            context("lead"),
        )
        .await
        .expect("ran");
        assert!(created.is_error);
    }
}
