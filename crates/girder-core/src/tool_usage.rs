//! Per-tool usage statistics: which tools are called, which of them fail, and how large their answers are.
//!
//! A session log answers "what happened" and [`crate::SessionStats`] answers "how much". Neither answers "which
//! tool", and that is the question a deployment optimising its tool set actually asks: a tool that is never called
//! is a schema every request pays for and no step uses, and a tool that fails a third of the time is one whose
//! description or contract is wrong. The log already carries the evidence — every `tool/call` names its tool and
//! every `tool/result` carries an error flag — but only one event at a time, so the answer has to be folded by name.
//!
//! # Why arguments and results are counted separately
//!
//! A tool that is called rarely can still dominate a conversation's context. One `bash` call that returns a build log
//! costs more window than fifty `read` calls on short files, and a single "bytes" total per tool would hide exactly
//! that. So [`ToolStats::argument_bytes`] — what the model wrote into the call — and [`ToolStats::result_bytes`] —
//! what the harness put back into the window — are kept apart. A high call count with a low average result is a
//! cheap tool; a low call count with a high total is an expensive one, and the two need different remedies.
//!
//! The numbers are bytes, not tokens. No tokenizer is available at this layer, and a caller that needs tokens should
//! apply its own ratio rather than be handed a figure that pretends to be one.
//!
//! # Why results are matched to calls
//!
//! A result names only the call id, so the tool it belongs to is known only from the call that opened it. A result
//! whose call is absent from the log is an *orphan* — a truncated log, or a result recorded after its call was
//! pruned — and it is counted for no tool. Guessing a name for it would inflate a tool's counts on evidence that does
//! not exist, and the guess would be invisible in the output.
//!
//! The mirror case is a call whose result never arrived. It is counted as a call, because the model did ask for it,
//! and contributes neither a failure nor result bytes, because neither is known. That is the same fact
//! [`crate::SessionStats::unanswered_tool_calls`] tracks, kept here per tool so a stuck tool is identifiable.
//!
//! # Why the result is already sorted
//!
//! The rows come back ordered by call count, most-used first, with ties broken by name. Sorting here rather than
//! leaving it to every caller means two deployments' reports are comparable, a "top N" listing needs no second pass,
//! and the order does not depend on a hash map's iteration order, which would silently change between runs.

use std::collections::BTreeMap;

use crate::llm::ContentBlock;
use crate::session::SessionEvent;

/// One tool's usage, aggregated over a slice of events.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolStats {
    /// The tool's name, as the model wrote it.
    ///
    /// Kept verbatim rather than normalised: two spellings of one tool are two tools to a registry, and folding them
    /// together here would report a call count no single tool has.
    pub name: String,
    /// How many times the tool was called.
    ///
    /// Counts every `tool/call`, including one whose result never arrived — the model asked for it, so it belongs in
    /// the call count even though nothing is known about how it ended.
    pub calls: u32,
    /// How many of those calls failed.
    ///
    /// Counts results flagged as errors and only those. A call with no result is not a failure: it is an unanswered
    /// call, which is a fact about the log rather than about the tool.
    pub failures: u32,
    /// Total bytes of the arguments the caller passed, summed over every call.
    ///
    /// These are the bytes the model generated, so this is the tool's *input* cost.
    pub argument_bytes: u64,
    /// Total bytes of the results, summed over every answered call.
    ///
    /// This is the tool's *output* cost: what the harness put back into the conversation. Attachment payloads live
    /// outside the log, so an image or file block contributes only the reference the log records, not the bytes it
    /// points at.
    pub result_bytes: u64,
}

impl ToolStats {
    /// The share of calls that failed, in `0.0..=1.0`.
    ///
    /// A tool with no calls returns `0.0` rather than dividing. Division would produce `NaN`, and a `NaN` is not a
    /// number a caller can use: it compares false against everything, so a sort by failure rate silently stops being
    /// a sort, and it serializes as `null` in JSON or fails to serialize at all. A tool that was never called has a
    /// zero failure rate, and a caller that needs to tell "never called" from "never failed" reads
    /// [`Self::calls`].
    #[must_use]
    pub fn failure_rate(&self) -> f64 {
        if self.calls == 0 {
            return 0.0;
        }
        // `u32 as f64` is exact for every `u32`, so `f64::from` is the lossless conversion here, and failures can
        // never exceed calls: a failure is only counted for an open call.
        f64::from(self.failures) / f64::from(self.calls)
    }

    /// The average bytes per answered call, when the tool was called at least once.
    ///
    /// `None` rather than `0.0` for a tool with no calls: an average over nothing is undefined, and a caller that
    /// displayed zero would be reporting a measurement nobody took.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // A byte average is a display figure; a count above 2^53 cannot come from any log this crate reads.
    pub fn average_result_bytes(&self) -> Option<f64> {
        if self.calls == 0 {
            return None;
        }
        Some(self.result_bytes as f64 / f64::from(self.calls))
    }
}

/// Folds `events` into one [`ToolStats`] per tool, most-called first.
///
/// Results are attributed through the call that opened them, so an orphan result is dropped rather than guessed at.
/// An empty slice yields an empty vector: a conversation with no tool use has no tools to report, which is different
/// from reporting tools with zero calls.
#[must_use]
pub fn usage(events: &[SessionEvent]) -> Vec<ToolStats> {
    let mut by_name: BTreeMap<String, ToolStats> = BTreeMap::new();
    // Call ids whose result has not arrived, each mapped to the tool it invoked. The result carries only the id, so
    // the name has to be remembered at call time; a map also makes "was this call ever opened" a single lookup.
    let mut open: BTreeMap<String, String> = BTreeMap::new();

    for event in events {
        match event {
            SessionEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                let stats = by_name.entry(name.clone()).or_insert_with(|| ToolStats {
                    name: name.clone(),
                    calls: 0,
                    failures: 0,
                    argument_bytes: 0,
                    result_bytes: 0,
                });
                stats.calls = stats.calls.saturating_add(1);
                stats.argument_bytes = stats
                    .argument_bytes
                    .saturating_add(as_bytes(arguments.len()));
                // Last call wins for a reused id. A provider that mints a duplicate id has already broken the pairing
                // the log relies on; the alternative — keeping the first name — would attribute the *newest* result to
                // an older call, which is the less plausible reading of a malformed log.
                open.insert(id.to_string(), name.clone());
            }
            SessionEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                // A result with no open call belongs to no tool. Dropping it keeps the totals honest: counting it
                // under any name would credit a tool for something the log cannot prove it did.
                if let Some(name) = open.remove(&id.to_string()) {
                    // The entry is guaranteed present: it was created by the call that opened this id. Looked up
                    // rather than indexed so a malformed log degrades to a dropped result instead of a panic.
                    if let Some(stats) = by_name.get_mut(&name) {
                        if *is_error {
                            stats.failures = stats.failures.saturating_add(1);
                        }
                        stats.result_bytes =
                            stats.result_bytes.saturating_add(content_bytes(content));
                    }
                }
            }
            _ => {}
        }
    }

    let mut stats: Vec<ToolStats> = by_name.into_values().collect();
    // Descending by calls, then by name: without the tie-break, tools called equally often would be ordered by the map
    // rather than by anything a reader can predict, and two runs over the same log could disagree.
    stats.sort_by(|left, right| {
        right
            .calls
            .cmp(&left.calls)
            .then_with(|| left.name.cmp(&right.name))
    });
    stats
}

/// Converts a length to bytes, saturating rather than wrapping.
///
/// A length that cannot fit in `u64` cannot be held in memory on any target this crate builds for, so the saturated
/// value is a bound rather than a guess; wrapping would report a huge result as a small one.
fn as_bytes(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// The bytes a result's content contributes, as the log records them.
fn content_bytes(content: &[ContentBlock]) -> u64 {
    content.iter().fold(0_u64, |total, block| {
        total.saturating_add(block_bytes(block))
    })
}

/// The bytes one block contributes to a result.
///
/// Every block is counted, including reasoning and a nested tool call: they are text the log holds and the next
/// request re-sends. Image and file blocks carry a reference to bytes stored elsewhere, so only the reference is
/// counted here — the payload's size is not in the log and guessing it would make the figure a fabrication.
fn block_bytes(block: &ContentBlock) -> u64 {
    match block {
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => as_bytes(text.len()),
        ContentBlock::ToolCall {
            name, arguments, ..
        } => as_bytes(name.len().saturating_add(arguments.len())),
        ContentBlock::Image { attachment } | ContentBlock::File { attachment } => {
            as_bytes(attachment.len())
        }
    }
}
