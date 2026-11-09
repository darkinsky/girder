//! Context compaction: deciding what to drop when a conversation outgrows its window.
//!
//! Upstream's `compaction` is 2.8k lines across five packages. The part whose correctness can be
//! reasoned about in isolation is **which span may be cut**, and that is what this module carries.
//! The summarizer that turns a span into a paragraph needs a model, so it stays with whoever owns
//! the provider.
//!
//! # The one invariant
//!
//! A cut must not separate a tool call from its result. A history where a `tool/call` survives
//! without its `tool/result` — or the reverse — is not merely incomplete: the model's next request is
//! rejected by every provider, because a tool call with no answer is a protocol violation.
//!
//! So the only legal cut positions are those where the number of unanswered calls crossing the point
//! is zero. [`pairing_balanced_at`] is that check, and every selection goes through it.
//!
//! # Why compaction is a range, not a filter
//!
//! Filtering individual messages looks simpler and is wrong: dropping one message from the middle of
//! a turn leaves a history whose tool calls and results reference a hole. A range keeps the surviving
//! prefix and suffix each internally consistent, and the summary stands in for the middle.

use crate::brand::ToolCallId;
use crate::session::{SessionEvent, TurnEndReason};

/// One position in a session's surface, with the event it holds.
///
/// `PartialEq` but not `Eq`: an event may hold a `serde_json::Value`, which has no total equality.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceEntry {
    /// 1-based sequence number, matching `SessionPageRequest::through_seq`.
    pub seq: u64,
    /// The event at this position.
    pub event: SessionEvent,
}

/// The tool calls opened and not yet answered at some point in the history.
///
/// A single number is enough because a balanced cut requires zero: a call and its result are one
/// pair, and two calls opened before either is answered still leave the count positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairingBalance {
    /// Calls opened minus results seen.
    pub open: i64,
}

impl PairingBalance {
    /// Whether the history is balanced here.
    #[must_use]
    pub fn is_balanced(self) -> bool {
        self.open == 0
    }
}

/// A failure raised while selecting a compaction range.
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    /// No cut position leaves a balanced history.
    #[error("no tool-pairing-balanced cut exists in the requested range")]
    NoBalancedCut,

    /// The selected range contains no messages worth summarizing.
    #[error("the selected range holds no messages")]
    EmptyRange,

    /// The range would keep nothing, leaving the model with no history at all.
    #[error("the selection would compact the entire surface")]
    WholeSurface,
}

/// What a compaction asks for.
#[derive(Clone, Debug)]
pub struct CompactionRequest {
    /// Keep at least this many of the most recent entries.
    ///
    /// A floor rather than a target: the selection may keep more when a tool pair forces it.
    pub keep_recent: usize,
    /// The furthest back the selection may reach, as a 0-based index into the surface.
    ///
    /// Bounds the work: compacting a thousand old turns is not more useful than compacting fifty.
    pub max_reach: Option<usize>,
}

impl Default for CompactionRequest {
    fn default() -> Self {
        Self {
            keep_recent: 8,
            max_reach: None,
        }
    }
}

/// A selected span, with the entries it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    /// 0-based index of the first selected entry.
    pub start_index: usize,
    /// 0-based index one past the last selected entry.
    pub end_index: usize,
    /// The entries being replaced by a summary.
    pub entries: Vec<SurfaceEntry>,
}

impl Selection {
    /// How many entries are selected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.end_index.saturating_sub(self.start_index)
    }

    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Computes the balance at every position in `surface`.
///
/// Index `i` holds the balance *after* entry `i`, so `balances[i] == 0` means the cut immediately
/// after entry `i` is balanced.
#[must_use]
pub fn balances(surface: &[SurfaceEntry]) -> Vec<PairingBalance> {
    let mut open: std::collections::BTreeSet<ToolCallId> = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(surface.len());

    for entry in surface {
        match &entry.event {
            SessionEvent::ToolCall { id, .. } => {
                open.insert(id.clone());
            }
            SessionEvent::ToolResult { id, .. } => {
                // A result with no preceding call is a broken history; removing a nonexistent entry
                // keeps the balance from going negative in a way that would hide the problem.
                open.remove(id);
            }
            _ => {}
        }
        out.push(PairingBalance {
            open: i64::try_from(open.len()).unwrap_or(i64::MAX),
        });
    }

    out
}

/// Whether the cut immediately after entry `index` is pairing-balanced.
///
/// `index` may be `surface.len()`, which is the end of the history and always balanced.
#[must_use]
pub fn pairing_balanced_at(surface: &[SurfaceEntry], index: usize) -> bool {
    if index >= surface.len() {
        return true;
    }
    balances(surface)
        .get(index)
        .is_none_or(|balance| balance.is_balanced())
}

/// Selects the span to replace with a summary.
///
/// The selection starts at the oldest entry within reach and ends at the latest position that keeps
/// `keep_recent` entries and is pairing-balanced. A tool pair that straddles the end pushes the end
/// earlier rather than splitting the pair — which is why the selection may keep fewer than
/// requested rather than more: cutting a pair is a protocol violation, keeping one extra message is
/// not.
///
/// # Errors
///
/// Returns [`CompactionError::NoBalancedCut`] when no legal end exists,
/// [`CompactionError::EmptyRange`] when nothing is selected, and
/// [`CompactionError::WholeSurface`] when the selection would remove everything.
pub fn select(
    surface: &[SurfaceEntry],
    request: &CompactionRequest,
) -> Result<Selection, CompactionError> {
    if surface.is_empty() {
        return Err(CompactionError::EmptyRange);
    }

    let balance = balances(surface);

    // The newest legal end: keep `keep_recent` entries, then walk backwards until the cut is
    // balanced.
    let wanted_end = surface.len().saturating_sub(request.keep_recent);
    let mut end = None;
    for candidate in (1..=wanted_end).rev() {
        // A cut *after* entry `candidate - 1` is `balances[candidate - 1]`.
        if balance.get(candidate - 1).is_some_and(|b| b.is_balanced()) {
            end = Some(candidate);
            break;
        }
    }
    let Some(end) = end else {
        return Err(CompactionError::NoBalancedCut);
    };

    if end >= surface.len() {
        return Err(CompactionError::WholeSurface);
    }

    // The oldest entry within reach, but never newer than the end.
    let start = request
        .max_reach
        .map_or(0, |reach| surface.len().saturating_sub(reach));
    let start = start.min(end.saturating_sub(1));

    // The start too must not split a pair: a range beginning inside a tool pair would leave the
    // surviving prefix with a result whose call was summarized away.
    let start = (start..end)
        .find(|candidate| {
            *candidate == 0 || balance.get(candidate - 1).is_some_and(|b| b.is_balanced())
        })
        .ok_or(CompactionError::NoBalancedCut)?;

    if start >= end {
        return Err(CompactionError::EmptyRange);
    }

    Ok(Selection {
        start_index: start,
        end_index: end,
        entries: surface[start..end].to_vec(),
    })
}

/// A trigger for compaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// The context grew past its window.
    Pressure {
        /// The measured size.
        measured: u64,
        /// The window it exceeded.
        window: u64,
    },
    /// The user asked.
    Manual,
}

/// What a finished compaction reports.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionOutcome {
    /// The span that was replaced.
    pub selection: Selection,
    /// The summary that stands in for it.
    pub summary: String,
    /// What started it.
    pub trigger: CompactionTrigger,
}

impl CompactionOutcome {
    /// How many entries the summary replaced.
    #[must_use]
    pub fn compacted(&self) -> usize {
        self.selection.len()
    }
}

/// Builds the text a summarizer receives, so the prompt shape is decided once.
///
/// The transcript keeps role labels because a summary that loses who said what is not usable: "the
/// user asked for X and the assistant said Y" is the whole content of a useful summary.
#[must_use]
pub fn transcript_for_summary(entries: &[SurfaceEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        match &entry.event {
            SessionEvent::UserMessage { message } => {
                out.push_str("user: ");
                out.push_str(&message.text());
                out.push('\n');
            }
            SessionEvent::AssistantMessage { message } => {
                out.push_str("assistant: ");
                out.push_str(&message.text());
                out.push('\n');
            }
            SessionEvent::ToolCall {
                name, arguments, ..
            } => {
                out.push_str("tool call: ");
                out.push_str(name);
                out.push(' ');
                out.push_str(arguments);
                out.push('\n');
            }
            SessionEvent::ToolResult {
                content, is_error, ..
            } => {
                out.push_str(if *is_error { "tool error: " } else { "tool: " });
                for block in content {
                    if let crate::llm::ContentBlock::Text { text } = block {
                        out.push_str(text);
                    }
                }
                out.push('\n');
            }
            SessionEvent::TurnEnd { reason, .. } => {
                // Only a failure is worth carrying into the summary; a normal turn boundary is
                // already implied by the message order.
                if matches!(
                    reason,
                    TurnEndReason::Failed { .. } | TurnEndReason::Aborted { .. }
                ) {
                    use std::fmt::Write as _;
                    let _ = writeln!(out, "turn ended: {reason:?}");
                }
            }
            _ => {}
        }
    }
    out.trim_end().to_owned()
}

/// Whether a measured context needs compaction.
///
/// A ratio rather than a fixed byte count: the same conversation is fine on a 200k window and past
/// the limit on an 8k one.
#[must_use]
pub fn needs_compaction(measured: u64, window: u64, threshold: f64) -> bool {
    if window == 0 {
        return false;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "token counts are far below 2^52, so f64 represents them exactly"
    )]
    let ratio = measured as f64 / window as f64;
    ratio >= threshold
}

// ---- summarization ----------------------------------------------------------------------------

/// Tags wrapping a checkpoint in the landed history.
pub const SUMMARY_OPEN_TAG: &str = "<compacted-summary>";
/// Closing tag for a checkpoint.
pub const SUMMARY_CLOSE_TAG: &str = "</compacted-summary>";

/// The preamble that makes a checkpoint read as established background.
///
/// Without it the model treats the summary as a message addressed to it and starts answering it,
/// which loses the whole point: the summary stands in for a conversation, not for a prompt.
pub const CHECKPOINT_PREAMBLE: &str = "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.";

/// The directive sent as the final user message.
///
/// A fixed structure rather than free-form "summarize this": a summary that omits what was tried and
/// failed makes the next model repeat the failure. Every section is required, with `(none)` for an
/// empty one, so an omission is visibly empty rather than silently missing.
pub const COMPACTION_INSTRUCTION: &str = "\
You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context.

Output EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write \"(none)\" for an empty section - never drop a section.

## Primary Request and Intent
- [the user's original and evolving goals; quote verbatim where the exact wording matters]

## Key Technical Concepts
- [technologies, frameworks, patterns, and conventions in play]

## Files and Code
- [exact path: why it matters, key changes or snippets]

## Errors and Fixes
- [error: how it was resolved, plus any related user feedback]

## Pending Jobs
- [explicitly requested work not yet completed]

## Current Work
- [precisely what was in progress at this checkpoint]

## Next Step
- [the single next action, directly in line with the most recent request, or \"(none)\"]

## Critical Context
- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]

Rules:
- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.
- Capture user feedback and explicit instructions faithfully, especially corrections.
- Do NOT mention this summarization request or that the context was compacted.
- Output only the checkpoint text: do not call any tool or take any other action.
- If the conversation already contains a <compacted-summary> block, it is a PRIOR checkpoint. Do not copy it forward verbatim: preserve still-true facts, drop stale ones, and merge newer information into a single consolidated summary under the same structure.";

/// Why summarization failed.
#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    /// The model returned no text.
    #[error("summarization produced no text")]
    Empty,

    /// The model reported a failure rather than a summary.
    #[error("the summarization request failed: {reason}")]
    Provider {
        /// The failure.
        reason: String,
    },
}

/// Builds the message list for a summarization request.
///
/// The instruction is appended as the final user message rather than passed as a separate system
/// prompt. That is not cosmetic: keeping the conversation's own system prompt, tools, and leading
/// messages in front of it makes the auxiliary call a genuine prefix of the last routed request, so
/// the provider's prefix cache is reused instead of invalidated. A separate system prompt would
/// change the prefix and pay full price for every compaction.
#[must_use]
pub fn summary_request(conversation: Vec<crate::llm::Message>) -> Vec<crate::llm::Message> {
    let mut messages = conversation;
    messages.push(crate::llm::Message::new(
        crate::llm::Role::User,
        vec![crate::llm::ContentBlock::Text {
            text: COMPACTION_INSTRUCTION.to_owned(),
        }],
    ));
    messages
}

/// Extracts the summary text from an assistant message, refusing an empty one.
///
/// # Errors
///
/// Returns [`SummaryError::Empty`] when the model produced whitespace or nothing, and
/// [`SummaryError::Provider`] when it reported a failure instead of answering.
pub fn extract_summary(message: &crate::llm::AssistantMessage) -> Result<String, SummaryError> {
    match &message.finish {
        Some(crate::llm::FinishReason::Error { message: reason }) => {
            return Err(SummaryError::Provider {
                reason: reason.clone(),
            });
        }
        Some(crate::llm::FinishReason::Aborted) => {
            return Err(SummaryError::Provider {
                reason: "aborted".to_owned(),
            });
        }
        _ => {}
    }

    let text = message.text();
    if text.trim().is_empty() {
        return Err(SummaryError::Empty);
    }
    Ok(text)
}

/// Wraps a summary as the checkpoint message that replaces the compacted span.
///
/// The preamble and tags are what make the checkpoint usable: without the preamble the model reads
/// the summary as a request, and without the tags a second compaction cannot tell a checkpoint from
/// something the user wrote.
#[must_use]
pub fn checkpoint_message(summary: &str, id: crate::brand::MessageId) -> crate::message::Message {
    let text =
        format!("{CHECKPOINT_PREAMBLE}\n\n{SUMMARY_OPEN_TAG}\n{summary}\n{SUMMARY_CLOSE_TAG}");
    crate::message::Message::new(
        id,
        crate::message::Role::User,
        crate::message::MessageSource::User {
            form: Some(crate::message::ContextForm::Recall),
        },
        vec![crate::llm::ContentBlock::Text { text }],
    )
}

/// Whether a message is a prior checkpoint.
///
/// A second compaction must merge into an existing checkpoint rather than nesting one inside another,
/// and detecting it needs the tags rather than a heuristic on the text.
#[must_use]
pub fn is_checkpoint(message: &crate::message::Message) -> bool {
    let text = message.text();
    text.contains(SUMMARY_OPEN_TAG) && text.contains(SUMMARY_CLOSE_TAG)
}
