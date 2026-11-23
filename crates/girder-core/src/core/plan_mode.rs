//! Plan mode: the model proposes, a person approves, and only then does the session leave the mode.
//!
//! Upstream `packages/plan/plan-mode` logs `plan/mode`, includes a guidance section while the mode
//! is active, and admits `exit_plan_mode` so the model can present the plan. A `/plan` line from
//! the user enters the mode; `/plan off` leaves it. Approval of the exit tool writes `active: false`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::Session;
use crate::interaction::approval::{Approver, Decision, Request};
use crate::session::SessionEvent;

/// Guidance included in the system prompt while plan mode is active.
///
/// The wording is the `plan-mode` section from the base profile.
pub const SECTION: &str = "\
You are in plan mode. Stay in plan mode until the user approves your plan through exit_plan_mode or switches the session mode. Imperative language to implement changes means plan the implementation, not execute it. A user's conversational agreement — including an answer confirming something you asked — approves nothing and does not end plan mode; fold the confirmed decision into the plan and submit it through exit_plan_mode.

Explore first. Use non-mutating reads, searches, static analysis, and checks to ground the plan in the actual repository. Do not edit or write files, change configuration, run formatters or code generation that rewrites tracked files, commit, or otherwise carry out the plan. Prefer existing functions and patterns over new machinery.

The tool catalog stays the same across modes for request-cache stability. These plan-mode rules override any later tool description or guidance that suggests using mutation tools; those tools remain listed only to keep the request shape stable. Do not use todo_write to track this planning phase: it tracks implementation after an approved plan, while the plan itself belongs in exit_plan_mode.

Resolve discoverable facts by inspection. Use ask_user_question only for user-owned choices or material ambiguity that inspection cannot answer. Do not ask the user where code lives or how current behavior works when you can find out.

Make the plan decision-complete: state the goal and success criteria; group implementation changes by subsystem; identify public API, schema, and data-flow changes; cover edge cases, failure modes, tests, acceptance criteria, and explicit assumptions. Keep it concise enough to review but detailed enough that another engineer can implement it without making design decisions.

When ready, call exit_plan_mode with the complete plan markdown, starting with a # title. Make exit_plan_mode the only and final tool call in that assistant response: it presents the plan for approval, and implementation begins only in a later step after approval. Do not paste the final plan as a plain reply or ask \"should I proceed?\" through prose or ask_user_question. If review rejects it, incorporate the feedback and present again. If the review channel is unavailable or aborted, stay in plan mode and ask the user to switch modes manually; do not proceed with implementation.";

/// The model-facing tool.
pub const EXIT_TOOL: &str = "exit_plan_mode";

const APPROVED: &str =
    "Plan approved — plan mode exited; carry out the plan starting with your next step.";
const KEEP_PLANNING: &str =
    "The user chose to keep planning; revise the plan and present it again.";

/// What a `/plan` line asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanCommand {
    /// Whether plan mode should be on after the line.
    pub active: bool,
    /// The rest of the line, which is the user's actual prompt.
    pub rest: String,
}

/// Reads `/plan` and `/plan off`. A word that merely starts with `plan` is left alone.
#[must_use]
pub fn plan_command(text: &str) -> Option<PlanCommand> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("/plan")?;
    if rest.is_empty() {
        return Some(PlanCommand {
            active: true,
            rest: String::new(),
        });
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    if rest == "off" {
        return Some(PlanCommand {
            active: false,
            rest: String::new(),
        });
    }
    Some(PlanCommand {
        active: true,
        rest: rest.to_owned(),
    })
}

/// The sentence shown when `/plan` carried no further prompt.
#[must_use]
pub fn switch_notice(active: bool) -> &'static str {
    if active {
        "The user switched this session to plan mode."
    } else {
        "The user switched this session back to the default mode."
    }
}

/// The last `plan/mode` event wins. A log with none is not in plan mode.
#[must_use]
pub fn active_from(events: &[SessionEvent]) -> bool {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::PlanMode { active } => Some(*active),
            _ => None,
        })
        .unwrap_or(false)
}

/// Whether this session's log currently has plan mode on.
pub async fn session_is_planning(session: &dyn Session) -> bool {
    session
        .replay()
        .await
        .map(|events| active_from(&events))
        .unwrap_or(false)
}

/// Upstream accepts a plan whose first character is `#`, then whitespace, then a title.
#[must_use]
pub fn has_plan_heading(plan: &str) -> bool {
    let Some(rest) = plan.trim().strip_prefix('#') else {
        return false;
    };
    let Some(first) = rest.chars().next() else {
        return false;
    };
    first.is_whitespace()
        && rest
            .trim_start()
            .chars()
            .next()
            .is_some_and(|char| !char.is_whitespace())
}

/// Live sessions a turn can hand to `exit_plan_mode`, keyed by agent id.
///
/// The tool is registered once for the process. The loop binds the session it is running before
/// the first step and unbinds it when the turn stream is dropped.
#[derive(Default)]
pub struct PlanSessions {
    sessions: Mutex<HashMap<String, Arc<dyn Session>>>,
}

impl PlanSessions {
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

    fn get(&self, agent: &str) -> Option<Arc<dyn Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(agent)
            .cloned()
    }
}

/// Removes the binding when the turn stream is dropped.
pub struct PlanBinding {
    gate: Arc<PlanSessions>,
    agent: String,
}

impl PlanBinding {
    /// Binds `session` for `agent` and unbinds it on drop.
    #[must_use]
    pub fn bind(gate: Arc<PlanSessions>, agent: &str, session: Arc<dyn Session>) -> Self {
        gate.bind(agent, session);
        Self {
            gate,
            agent: agent.to_owned(),
        }
    }
}

impl Drop for PlanBinding {
    fn drop(&mut self) {
        self.gate.unbind(&self.agent);
    }
}

/// The `exit_plan_mode` tool.
///
/// It asks `approver` with the plan as the detail. Approval appends `plan/mode` with `active: false`.
/// A denial stays in plan mode and returns the person's reason to the model.
pub fn exit_plan_mode_tool(
    gate: Arc<PlanSessions>,
    approver: Arc<dyn Approver>,
) -> crate::core::tools::ToolDefinition {
    crate::core::tools::ToolDefinition::new(
        EXIT_TOOL,
        "Use only in plan mode. Present your plan for the user's review and, on approval, leave plan mode. \
         The user may approve (carry out the plan from your next step) or keep planning — their feedback \
         comes back in the tool result; revise and present again.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The complete plan, as markdown, starting with a # heading that names it."
                }
            },
            "required": ["plan"]
        }),
        Arc::new(
            move |args: Value, ctx: crate::core::tools::ToolRunContext| {
                let gate = Arc::clone(&gate);
                let approver = Arc::clone(&approver);
                Box::pin(async move {
                    let Some(plan) = args.get("plan").and_then(Value::as_str) else {
                        return Ok(crate::core::tools::ToolResult::error(
                            "`plan` is required and must be markdown starting with a # heading",
                        ));
                    };
                    if !has_plan_heading(plan) {
                        return Ok(crate::core::tools::ToolResult::error(
                            "exit_plan_mode requires a non-empty markdown plan starting with a # heading",
                        ));
                    }
                    let Some(session) = gate.get(ctx.execution.agent_id.as_str()) else {
                        return Ok(crate::core::tools::ToolResult::error(
                            "exit_plan_mode is only available in plan mode",
                        ));
                    };
                    if !session_is_planning(session.as_ref()).await {
                        return Ok(crate::core::tools::ToolResult::error(
                            "exit_plan_mode is only available in plan mode",
                        ));
                    }
                    let Some(mut request) =
                        Request::new(EXIT_TOOL, "Approve this plan and leave plan mode?")
                    else {
                        return Ok(crate::core::tools::ToolResult::error(
                            "exit_plan_mode could not ask for review",
                        ));
                    };
                    request = request.with_detail(plan);
                    let decision = match approver.ask(request).await {
                        Ok(decision) => decision,
                        Err(_) => {
                            return Ok(crate::core::tools::ToolResult::error(
                                "no user-questions channel is available to review the plan; ask the user to switch the session mode instead",
                            ));
                        }
                    };
                    if !decision.is_approved() {
                        let feedback = match &decision {
                            Decision::Denied {
                                reason: Some(reason),
                                ..
                            } if !reason.trim().is_empty() => {
                                format!("The user chose to keep planning; their feedback: {reason}")
                            }
                            _ => KEEP_PLANNING.to_owned(),
                        };
                        return Ok(crate::core::tools::ToolResult::error(feedback));
                    }
                    if let Err(error) = session
                        .append(SessionEvent::PlanMode { active: false })
                        .await
                    {
                        return Ok(crate::core::tools::ToolResult::error(format!(
                            "the plan was approved but the session could not record it: {error}"
                        )));
                    }
                    Ok(crate::core::tools::ToolResult::text(APPROVED))
                })
            },
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::{PlanCommand, active_from, has_plan_heading, plan_command};
    use crate::session::SessionEvent;

    #[test]
    fn a_plan_line_enters_and_off_leaves() {
        assert_eq!(
            plan_command("  /plan  "),
            Some(PlanCommand {
                active: true,
                rest: String::new(),
            })
        );
        assert_eq!(
            plan_command("/plan off"),
            Some(PlanCommand {
                active: false,
                rest: String::new(),
            })
        );
        assert_eq!(
            plan_command("/plan fix the retry"),
            Some(PlanCommand {
                active: true,
                rest: "fix the retry".to_owned(),
            })
        );
        assert!(plan_command("/planning").is_none());
        assert!(plan_command("please /plan").is_none());
    }

    #[test]
    fn the_last_plan_event_wins() {
        let events = vec![
            SessionEvent::PlanMode { active: true },
            SessionEvent::PlanMode { active: false },
        ];
        assert!(!active_from(&events));
        assert!(active_from(&[SessionEvent::PlanMode { active: true }]));
        assert!(!active_from(&[]));
    }

    #[test]
    fn a_plan_needs_a_markdown_heading() {
        assert!(has_plan_heading("# Ship the retry fix\n\nDetails."));
        assert!(!has_plan_heading("## Ship it"));
        assert!(!has_plan_heading("# "));
        assert!(!has_plan_heading("Ship it"));
    }
}
