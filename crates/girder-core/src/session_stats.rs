//! Counting what a conversation did: turns, steps, tool calls, and tokens.
//!
//! A session log answers "what happened"; this answers "how much". The difference matters for a UI that shows a
//! conversation's shape at a glance and for an operator deciding whether a model is earning its cost.
//!
//! # What this can count, and what it cannot
//!
//! The log records no timestamps. `SessionEvent` carries content and identity, not wall-clock time, so every duration a
//! richer stats layer would report -- model latency, tool latency, first-token delay, decode time -- is not derivable
//! from it. Reporting those as zero would be worse than omitting them: a caller reading `llm_ms: 0` learns that the
//! model was instant rather than that nothing measured it.
//!
//! So this counts, and a `Duration`s field is deliberately absent rather than present and always zero. Adding time to
//! the log is a change to the event format, which is a different piece of work with a migration attached.
//!
//! # Why the counts are separated by kind
//!
//! A turn that produced no step is a turn that was rejected or never ran, and folding it into the step count would
//! make a conversation look busier than it was. The same for a tool call whose result never arrived: counting it as a
//! completed call would overstate what the tools did.

use std::collections::BTreeSet;

use crate::session::SessionEvent;

/// What a conversation did, in counts.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    /// Turns that closed at least one step.
    ///
    /// A turn that started and never ran a step is not counted: it produced nothing, and counting it would make an
    /// abandoned conversation look like a busy one.
    pub turns: u64,
    /// Steps that closed.
    pub steps: u64,
    /// Tool calls that received a result.
    pub tool_calls: u64,
    /// Tool calls whose result never arrived.
    ///
    /// Tracked apart because it is a fact about the conversation rather than about the tools: a crash mid-call leaves
    /// one, and a caller deciding whether a log is complete needs to see it.
    pub unanswered_tool_calls: u64,
    /// Tool calls that failed.
    pub failed_tool_calls: u64,
    /// Messages by role.
    pub user_messages: u64,
    /// Messages the model wrote.
    pub assistant_messages: u64,
    /// Output tokens the provider reported.
    pub output_tokens: u64,
    /// Prompt tokens the provider reported.
    pub input_tokens: u64,
    /// Which models answered, in first-use order.
    pub models: Vec<String>,
}

impl Stats {
    /// Counts one conversation.
    #[must_use]
    pub fn of(events: &[SessionEvent]) -> Self {
        let mut stats = Self::default();
        let mut closed_turns: BTreeSet<u64> = BTreeSet::new();
        // Calls opened and not yet answered, so an unanswered one is identified rather than merely not counted.
        let mut open: BTreeSet<String> = BTreeSet::new();
        let mut models: Vec<String> = Vec::new();

        for event in events {
            match event {
                SessionEvent::StepEnd { turn, .. } => {
                    stats.steps += 1;
                    closed_turns.insert(*turn);
                }
                SessionEvent::ToolCall { id, .. } => {
                    open.insert(id.to_string());
                }
                // Matched on the removal rather than checked after it: a result for a call this log never recorded is
                // not a tool call, and counting it would make the totals disagree with the conversation.
                SessionEvent::ToolResult { id, is_error, .. } if open.remove(&id.to_string()) => {
                    stats.tool_calls += 1;
                    if *is_error {
                        stats.failed_tool_calls += 1;
                    }
                }
                SessionEvent::UserMessage { .. } => stats.user_messages += 1,
                SessionEvent::AssistantMessage { .. } => stats.assistant_messages += 1,
                SessionEvent::ModelUsage {
                    model,
                    input_tokens,
                    output_tokens,
                } => {
                    stats.input_tokens = stats.input_tokens.saturating_add(*input_tokens);
                    stats.output_tokens = stats.output_tokens.saturating_add(*output_tokens);
                    if !models.iter().any(|seen| seen == model) {
                        models.push(model.clone());
                    }
                }
                _ => {}
            }
        }

        stats.turns = u64::try_from(closed_turns.len()).unwrap_or(u64::MAX);
        stats.unanswered_tool_calls = u64::try_from(open.len()).unwrap_or(u64::MAX);
        stats.models = models;
        stats
    }

    /// Whether the conversation recorded any model call.
    #[must_use]
    pub fn has_model_calls(&self) -> bool {
        self.output_tokens > 0 || self.input_tokens > 0 || !self.models.is_empty()
    }

    /// The average output tokens per assistant message, when there is one.
    ///
    /// `None` rather than zero for an empty conversation: an average of no messages is not zero, it is undefined, and
    /// a caller that displayed zero would be reporting a measurement it does not have.
    #[must_use]
    pub fn average_output_per_message(&self) -> Option<f64> {
        if self.assistant_messages == 0 {
            return None;
        }
        // `u64 as f64` rounds above 2^53, which for a token count is a difference a reader would never notice and a
        // linter is right to require be acknowledged rather than assumed.
        let total = f64::from(u32::try_from(self.output_tokens).unwrap_or(u32::MAX));
        let count = f64::from(u32::try_from(self.assistant_messages).unwrap_or(u32::MAX));
        Some(total / count)
    }
}
