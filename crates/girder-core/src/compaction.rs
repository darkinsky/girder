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
