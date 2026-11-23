//! Human commands that finish before a model turn starts.
//!
//! `/compact`, `/goal`, `/feedback`, and `/team` are the base profile's command rows. Each one
//! answers the person directly. The line stays in the session log and is left out of the next
//! model request.

use crate::core::agent_loop::LoopContext;
use crate::goal::{Activation, Goal, GoalId, GoalState, Phase};
use crate::session::SessionEvent;

const COMPACT_USAGE: &str = "Usage: /compact (no arguments)";
const GOAL_USAGE: &str = "Usage: /goal [<objective>|clear|edit <objective>|pause|resume]";
const FEEDBACK_USAGE: &str = "Usage: /feedback <text>";
const TEAM_USAGE: &str = "Usage: /team [on|off]";

/// A command the loop handles itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Compact older history. The string is any trailing text, which is a usage error.
    Compact(String),
    /// Show or change the session goal.
    Goal(GoalLine),
    /// Record a remark. The string is the remark, already trimmed.
    Feedback(String),
    /// Show or change Agent Teams.
    Team(TeamLine),
}

/// What `/team` asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeamLine {
    /// `/team` with nothing after it.
    Show,
    /// Turn the board on.
    On,
    /// Turn the board off.
    Off,
    /// Anything else.
    Invalid,
}

/// What `/goal` asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalLine {
    /// `/goal` with nothing after it.
    Show,
    /// A new objective.
    Create(String),
    /// Replace the objective.
    Edit(String),
    /// `/goal edit` with no replacement.
    InvalidEdit,
    /// Stop automatic continuation.
    Pause,
    /// Continue an existing goal.
    Resume,
    /// Drop the goal.
    Clear,
}

/// Reads `/compact`, `/goal`, and `/feedback`. A longer word is left for the model.
#[must_use]
pub fn parse(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    if let Some(rest) = command_rest(trimmed, "compact") {
        return Some(Command::Compact(rest));
    }
    if let Some(rest) = command_rest(trimmed, "feedback") {
        return Some(Command::Feedback(rest));
    }
    if let Some(rest) = command_rest(trimmed, "goal") {
        return Some(Command::Goal(parse_goal(&rest)));
    }
    if let Some(rest) = command_rest(trimmed, "team") {
        return Some(Command::Team(parse_team(&rest)));
    }
    None
}

fn parse_team(rest: &str) -> TeamLine {
    match rest.trim() {
        "" => TeamLine::Show,
        "on" => TeamLine::On,
        "off" => TeamLine::Off,
        _ => TeamLine::Invalid,
    }
}

fn command_rest(text: &str, name: &str) -> Option<String> {
    let rest = text.strip_prefix('/')?.strip_prefix(name)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim().to_owned())
    } else {
        None
    }
}

fn parse_goal(rest: &str) -> GoalLine {
    let input = rest.trim();
    if input.is_empty() {
        return GoalLine::Show;
    }
    match input.to_ascii_lowercase().as_str() {
        "clear" => return GoalLine::Clear,
        "pause" => return GoalLine::Pause,
        "resume" => return GoalLine::Resume,
        "edit" => return GoalLine::InvalidEdit,
        _ => {}
    }
    let bytes = input.as_bytes();
    if bytes.len() > 4 && bytes[..4].eq_ignore_ascii_case(b"edit") && bytes[4].is_ascii_whitespace()
    {
        let objective = input[4..].trim();
        if objective.is_empty() {
            return GoalLine::InvalidEdit;
        }
        return GoalLine::Edit(objective.to_owned());
    }
    GoalLine::Create(input.to_owned())
}

/// Runs one command and returns the text the person should see.
pub async fn execute(context: &LoopContext, command: Command) -> String {
    let text = match command {
        Command::Compact(rest) => compact(context, &rest).await,
        Command::Goal(line) => goal(context, line),
        Command::Feedback(text) => feedback(context, &text).await,
        Command::Team(line) => team(context, line).await,
    };
    format!("{text}\n")
}

async fn compact(context: &LoopContext, rest: &str) -> String {
    if !rest.is_empty() {
        return COMPACT_USAGE.to_owned();
    }
    let Some(policy) = context.compaction.clone() else {
        return "Compaction is not configured for this session.".to_owned();
    };
    let events = match context.session.replay().await {
        Ok(events) => events,
        Err(error) => return format!("Compaction could not read the session: {error}"),
    };
    match plan_compact(&events, &policy) {
        CompactPlan::Nothing => "No compactable history yet.".to_owned(),
        CompactPlan::Refused(reason) => reason,
        CompactPlan::Ready {
            id,
            text,
            shadowed,
            count,
        } => {
            if let Err(error) = context
                .session
                .append(SessionEvent::CompactionSummary { id, text, shadowed })
                .await
            {
                return format!("Compaction finished, but the session could not be saved: {error}");
            }
            format!("Compacted {count} history items.")
        }
    }
}

/// What a manual compaction decided, before it is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactPlan {
    /// The history is shorter than the keep floor.
    Nothing,
    /// A safe summary could not be made.
    Refused(String),
    /// A summary that replaces `count` rows.
    Ready {
        /// Summary id.
        id: String,
        /// Summary text.
        text: String,
        /// Ids the summary replaces.
        shadowed: Vec<String>,
        /// How many rows were replaced.
        count: usize,
    },
}

/// Plans one manual compaction. The automatic trigger is ignored: `/compact` asks now.
#[must_use]
pub fn plan_compact(
    events: &[SessionEvent],
    policy: &crate::compaction::hook::CompactionPolicy,
) -> CompactPlan {
    let mut manual = policy.clone();
    manual.trigger_messages = 1;
    manual.trigger_bytes = None;
    let visible = crate::session::visible_turns(events);
    let chats: Vec<crate::session::message::Message> =
        visible.iter().map(|turn| turn.chat.clone()).collect();
    match manual.plan(&chats) {
        crate::compaction::hook::Plan::KeepAll => CompactPlan::Nothing,
        crate::compaction::hook::Plan::Refuse { reason } => CompactPlan::Refused(reason),
        crate::compaction::hook::Plan::Compact { drop, .. } => {
            let dropped: Vec<crate::session::message::Message> = drop
                .iter()
                .filter_map(|index| chats.get(*index).cloned())
                .collect();
            let text = crate::compaction::summary::render(&crate::compaction::summary::summarize(
                &dropped,
            ));
            if text.is_empty() {
                return CompactPlan::Refused(
                    "Compaction could not produce a useful summary. The attempt is recorded in the session log."
                        .to_owned(),
                );
            }
            let shadowed: Vec<String> = drop
                .iter()
                .filter_map(|index| visible.get(*index).map(|turn| turn.key.clone()))
                .collect();
            CompactPlan::Ready {
                id: format!("c{}-{}", events.len(), crate::now_millis()),
                text,
                count: shadowed.len(),
                shadowed,
            }
        }
    }
}

fn goal(context: &LoopContext, line: GoalLine) -> String {
    let Some(slot) = &context.goal else {
        return "No goal state is attached to this session.".to_owned();
    };
    let mut state = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    apply_goal(&mut state, line, crate::now_millis())
}

/// Applies one `/goal` line to `state`.
#[must_use]
pub fn apply_goal(state: &mut GoalState, line: GoalLine, now: u64) -> String {
    match line {
        GoalLine::Show => match state.current().cloned() {
            None => format!("No goal is currently set.\n{GOAL_USAGE}"),
            Some(goal) => render_goal("Goal", &goal, state.activation()),
        },
        GoalLine::InvalidEdit => {
            format!("Goal editing requires a replacement objective.\n{GOAL_USAGE}")
        }
        GoalLine::Clear => {
            if state.clear().is_none() {
                "No goal to clear.".to_owned()
            } else {
                "Goal cleared.".to_owned()
            }
        }
        GoalLine::Create(objective) => {
            if let Some(goal) = state.current() {
                if goal.phase != Phase::Complete {
                    return format!(
                        "A goal is already {}. Use /goal edit <objective> to change it or /goal clear before replacing it.",
                        goal.phase.as_str()
                    );
                }
            }
            finish(
                state.create(fresh_id(now), objective, None, now),
                "Goal created",
                state,
            )
        }
        GoalLine::Edit(objective) => match state.current().cloned() {
            None => missing("edit"),
            Some(goal) if goal.phase == Phase::Complete => finish(
                state.create(fresh_id(now), objective, None, now),
                "Goal created",
                state,
            ),
            Some(goal) => finish(
                state.edit(&goal.reference(), Some(objective), None),
                "Goal updated",
                state,
            ),
        },
        GoalLine::Pause => match state.current().cloned() {
            None => missing("pause"),
            Some(goal) => finish(state.pause(&goal.reference()), "Goal paused", state),
        },
        GoalLine::Resume => match state.current().cloned() {
            None => missing("resume"),
            Some(goal) => finish(state.resume(&goal.reference()), "Goal resumed", state),
        },
    }
}

fn fresh_id(now: u64) -> GoalId {
    GoalId::new(format!("goal-{now}")).expect("the id contains a number")
}

fn missing(action: &str) -> String {
    format!("No goal is currently set; /goal {action} requires one. {GOAL_USAGE}")
}

fn finish(outcome: Result<Goal, crate::goal::GoalError>, title: &str, state: &GoalState) -> String {
    match outcome {
        Ok(goal) => render_goal(title, &goal, state.activation()),
        Err(_) => "The goal command is not valid for the current state. Run /goal to view available commands.".to_owned(),
    }
}

fn render_goal(title: &str, goal: &Goal, activation: Activation) -> String {
    let mut lines = vec![title.to_owned(), format!("Status: {}", goal.phase.as_str())];
    if let Some(reason) = &goal.blocked_reason {
        lines.push(format!("Blocker: {}: {}", reason.code, reason.message));
    }
    lines.push(format!("Objective: {}", goal.objective));
    lines.push(format!(
        "Rounds: {}/{}",
        goal.rounds_started, goal.max_rounds
    ));
    lines.push(format!("Activation: {}", activation.as_str()));
    lines.push(String::new());
    lines.push(format!("Commands: {}", command_hint(goal, activation)));
    lines.join("\n")
}

fn command_hint(goal: &Goal, activation: Activation) -> &'static str {
    if goal.phase == Phase::Active {
        return if activation == Activation::Armed {
            "/goal edit <objective>, /goal pause, /goal clear"
        } else {
            "/goal edit <objective>, /goal resume, /goal clear"
        };
    }
    match goal.phase {
        Phase::Paused | Phase::Blocked => "/goal edit <objective>, /goal resume, /goal clear",
        Phase::Complete => "/goal <objective>, /goal clear",
        Phase::Active => "/goal edit <objective>, /goal pause, /goal clear",
    }
}

async fn feedback(context: &LoopContext, text: &str) -> String {
    if text.trim().is_empty() {
        return format!("Feedback text is required. {FEEDBACK_USAGE}");
    }
    if let Err(error) = context
        .session
        .append(SessionEvent::SessionFeedbackRecord {
            text: Some(text.trim().to_owned()),
            category: None,
        })
        .await
    {
        return format!("Feedback could not be recorded: {error}");
    }
    format!(
        "Feedback recorded for session {}.",
        context.agent_id.as_str()
    )
}

async fn team(context: &LoopContext, line: TeamLine) -> String {
    let enabled = match line {
        TeamLine::Invalid => return TEAM_USAGE.to_owned(),
        TeamLine::Show => {
            let events = match context.session.replay().await {
                Ok(events) => events,
                Err(error) => return format!("Agent Teams could not read the session: {error}"),
            };
            return if crate::team::agent_teams::is_enabled(&events) {
                "Agent Teams is on.".to_owned()
            } else {
                "Agent Teams is off.".to_owned()
            };
        }
        TeamLine::On => true,
        TeamLine::Off => false,
    };
    let events = match context.session.replay().await {
        Ok(events) => events,
        Err(error) => return format!("Agent Teams could not read the session: {error}"),
    };
    if crate::team::agent_teams::is_enabled(&events) == enabled {
        return if enabled {
            "Agent Teams is already on.".to_owned()
        } else {
            "Agent Teams is already off.".to_owned()
        };
    }
    if let Err(error) = context
        .session
        .append(SessionEvent::TeamMode { enabled })
        .await
    {
        return format!("Agent Teams could not be saved: {error}");
    }
    if enabled {
        "Agent Teams is on. Shared tasks are available to this session.".to_owned()
    } else {
        "Agent Teams is off.".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, GoalLine, TeamLine, apply_goal, parse, plan_compact};
    use crate::compaction::hook::CompactionPolicy;
    use crate::goal::GoalState;
    use crate::session::{SessionEvent, model_messages};

    #[test]
    fn slash_words_parse_and_longer_words_do_not() {
        assert!(matches!(parse("/compact"), Some(Command::Compact(rest)) if rest.is_empty()));
        assert!(matches!(parse("/compact now"), Some(Command::Compact(rest)) if rest == "now"));
        assert!(parse("/compaction").is_none());
        assert!(matches!(
            parse("/goal"),
            Some(Command::Goal(GoalLine::Show))
        ));
        assert!(matches!(
            parse("/goal ship it"),
            Some(Command::Goal(GoalLine::Create(objective))) if objective == "ship it"
        ));
        assert!(matches!(
            parse("/goal edit"),
            Some(Command::Goal(GoalLine::InvalidEdit))
        ));
        assert!(matches!(
            parse("/goal edit rename"),
            Some(Command::Goal(GoalLine::Edit(objective))) if objective == "rename"
        ));
        assert!(matches!(parse("/feedback"), Some(Command::Feedback(rest)) if rest.is_empty()));
        assert!(matches!(
            parse("/team"),
            Some(Command::Team(TeamLine::Show))
        ));
        assert!(matches!(
            parse("/team on"),
            Some(Command::Team(TeamLine::On))
        ));
        assert!(matches!(
            parse("/team off"),
            Some(Command::Team(TeamLine::Off))
        ));
        assert!(matches!(
            parse("/team later"),
            Some(Command::Team(TeamLine::Invalid))
        ));
        assert!(parse("/teammate").is_none());
        assert!(parse("please /goal this").is_none());
    }

    #[test]
    fn a_goal_can_be_created_edited_and_cleared() {
        let mut state = GoalState::new();
        let created = apply_goal(&mut state, GoalLine::Create("ship it".to_owned()), 1);
        assert!(created.contains("Goal created"), "{created}");
        assert!(created.contains("Objective: ship it"), "{created}");
        let edited = apply_goal(&mut state, GoalLine::Edit("ship the fix".to_owned()), 2);
        assert!(edited.contains("Objective: ship the fix"), "{edited}");
        assert_eq!(apply_goal(&mut state, GoalLine::Clear, 3), "Goal cleared.");
        assert!(apply_goal(&mut state, GoalLine::Show, 4).contains("No goal is currently set"));
    }

    #[test]
    fn a_manual_compact_hides_the_replaced_rows() {
        let events: Vec<SessionEvent> = (0..6)
            .map(|index| SessionEvent::user_text(&format!("m{index}"), format!("ask {index}")))
            .collect();
        let plan = plan_compact(&events, &CompactionPolicy::new(40, 2));
        let super::CompactPlan::Ready {
            id, text, shadowed, ..
        } = plan
        else {
            panic!("expected a summary, got {plan:?}");
        };
        assert!(shadowed.len() > 1, "{shadowed:?}");
        let mut stored = events;
        stored.push(SessionEvent::CompactionSummary {
            id,
            text: text.clone(),
            shadowed,
        });
        let visible = model_messages(&stored);
        let texts: Vec<String> = visible
            .iter()
            .map(|message| format!("{:?}", message.content))
            .collect();
        assert!(texts[0].contains("compacted span"), "{texts:?}");
        assert!(
            texts.iter().skip(1).any(|text| text.contains("ask 5")),
            "{texts:?}"
        );
        assert!(
            texts.iter().skip(1).all(|text| !text.contains("ask 0")),
            "{texts:?}"
        );
    }
}
