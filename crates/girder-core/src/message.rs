//! The message model: identity, provenance, and the five roles.
//!
//! Upstream's message model carries a `source` alongside every message, and the reason is worth
//! stating: a conversation log has to answer "who produced this" separately from "what role is it".
//! A user-role message may come from the person, from another agent relaying, or from a plugin
//! injecting context — all `role: user`, all different provenance.
//!
//! # The five roles
//!
//! | role | produced by | why it is not another role |
//! |---|---|---|
//! | `system` | the system-prompt plugin | sent once, not part of the conversation turn |
//! | `developer` | tool changes | a tool addition/removal rides a turn but is not the model speaking |
//! | `user` | the person, or anything injecting into their turn | |
//! | `assistant` | the model | carries provider identity for replay |
//! | `tool` | a tool's result | carries the call id it answers |
//!
//! # Why provenance is a separate axis
//!
//! `kind` answers *who produced this*; `form` answers *what kind of thing it is*. They are
//! deliberately independent: several producers share one form, and one producer may emit more than
//! one form over a session. Collapsing them would make "show instructions differently from a
//! notice" impossible without teaching every consumer about every producer.

use serde::{Deserialize, Serialize};

use crate::brand::MessageId;
use crate::llm::ContentBlock;

/// The bound on a one-line context summary.
///
/// Producers commit this to a durable log, and its inputs — task labels, goal objectives, tool
/// arguments — are caller text with no length of their own.
pub const CONTEXT_SUMMARY_MAX_CHARS: usize = 120;

/// Bounds a summary, ellipsizing when it exceeds the limit.
#[must_use]
pub fn bound_context_summary(summary: &str) -> String {
    if summary.chars().count() <= CONTEXT_SUMMARY_MAX_CHARS {
        return summary.to_owned();
    }
    let mut bounded: String = summary
        .chars()
        .take(CONTEXT_SUMMARY_MAX_CHARS - 1)
        .collect();
    bounded.push('…');
    bounded
}

/// What kind of information a producer supplied.
///
/// Semantic, never visual: a value says the content is a file's instructions or a catalog of
/// available items, and the consumer decides what that looks like. Colours, icons, and collapse
/// defaults are the consumer's business and must not enter this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextForm {
    /// Instructions read out of workspace files the model is expected to follow.
    Instructions,
    /// A catalog of items available in this session, republished as it changes.
    Catalog,
    /// Current state, where a later snapshot supersedes an earlier one.
    Snapshot,
    /// A one-off account of something that just happened; supersedes nothing.
    Notice,
    /// A message another agent addressed to this one.
    Relay,
    /// Material lifted out of another session's log, possibly reduced on the way in.
    Recall,
}

/// One named contribution to a snapshot-form context, in assembly order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSnapshotSection {
    /// The contributing subsystem's name.
    pub name: String,
    /// Its model-facing text, exactly as assembled.
    pub text: String,
}

/// Where a message came from.
///
/// A tagged union rather than a `role` plus flags: the fields a source needs differ, and a
/// `#[serde(tag = "kind")]` makes an unknown source a decode error rather than a silently ignored
/// field. Upstream is merge-extensible here; Rust's closed enums mean every producer's kind lands in
/// this one place, which is a real cost paid for compile-time exhaustiveness.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MessageSource {
    /// The person.
    User {
        /// What kind of context this is, when the producer declared one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        form: Option<ContextForm>,
    },

    /// The model, with the provider identity needed to replay its response.
    Model {
        /// Provider route that produced the message.
        provider: String,
        /// Model id that produced it.
        model: String,
        /// Lossless-JSON adapter state for replaying the response.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay_state: Option<serde_json::Value>,
    },

    /// A tool's result.
    Tool {
        /// The call this answers.
        call_id: crate::brand::ToolCallId,
    },

    /// The system prompt.
    SystemPrompt,

    /// Another agent relaying a message.
    Relay {
        /// The sending agent.
        from: crate::brand::AgentId,
    },
}

impl MessageSource {
    /// Whether this source is a model.
    #[must_use]
    pub fn is_model(&self) -> bool {
        matches!(self, Self::Model { .. })
    }

    /// The call id a tool source answers, when it is one.
    #[must_use]
    pub fn call_id(&self) -> Option<&crate::brand::ToolCallId> {
        match self {
            Self::Tool { call_id } => Some(call_id),
            _ => None,
        }
    }
}

/// Who authored a message.
///
/// Five roles rather than three because `developer` exists: a tool addition or removal rides a turn
/// but is not the model speaking and not the person speaking, and folding it into either would make
/// replay ambiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The system prompt.
    System,
    /// Tool changes riding a turn.
    Developer,
    /// The person, or anything injecting into their turn.
    User,
    /// The model.
    Assistant,
    /// A tool's result.
    Tool,
}

/// One conversation message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Stable identity, preserved across every representation boundary.
    pub id: MessageId,
    /// Who authored it.
    pub role: Role,
    /// Who produced it, in the harness's own vocabulary.
    pub source: MessageSource,
    /// Its blocks.
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Builds a message.
    #[must_use]
    pub fn new(
        id: MessageId,
        role: Role,
        source: MessageSource,
        content: Vec<ContentBlock>,
    ) -> Self {
        Self {
            id,
            role,
            source,
            content,
        }
    }

    /// Builds a user message.
    #[must_use]
    pub fn user(id: MessageId, text: impl Into<String>) -> Self {
        Self {
            id,
            role: Role::User,
            source: MessageSource::User { form: None },
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Builds a system message.
    #[must_use]
    pub fn system(id: MessageId, text: impl Into<String>) -> Self {
        Self {
            id,
            role: Role::System,
            source: MessageSource::SystemPrompt,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// The concatenated visible text.
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Whether a role and its source agree.
    ///
    /// A mismatch is a producer bug, not a runtime condition: an `assistant` message must come from
    /// a model, a `tool` message must answer a call. Checking it at construction would be better,
    /// but the log is decoded from the wire where a hand-edited file is possible, so this is the
    /// check a replay runs to refuse a corrupt history rather than act on it.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        match self.role {
            Role::Assistant => self.source.is_model(),
            Role::Tool => self.source.call_id().is_some(),
            Role::System => matches!(self.source, MessageSource::SystemPrompt),
            // A user or developer turn may carry any producer: that is the point of separating the
            // two axes.
            Role::User | Role::Developer => true,
        }
    }
}

/// The failure a replay reports for an inconsistent history.
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    /// A role and its source disagree.
    #[error("message `{id}` is role {role:?} but its source is {detail}")]
    InconsistentRole {
        /// The message.
        id: String,
        /// Its role.
        role: Role,
        /// Its source, formatted.
        detail: String,
    },
}

/// Checks that every message's role and source agree.
///
/// # Errors
///
/// Returns the first inconsistency, naming the message.
pub fn check_history(messages: &[Message]) -> Result<(), MessageError> {
    for message in messages {
        if !message.is_consistent() {
            return Err(MessageError::InconsistentRole {
                id: message.id.to_string(),
                role: message.role,
                detail: format!("{:?}", message.source),
            });
        }
    }
    Ok(())
}
