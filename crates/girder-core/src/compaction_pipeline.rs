//! The whole compaction pass in one call: decide, cut, summarise, splice.
//!
//! Three modules already own the pieces, and until now nothing joined them.
//! [`compaction`](crate::compaction) owns *where* a history may be cut —
//! [`pairing_balanced_at`](crate::pairing_balanced_at) is the legality rule.
//! [`compaction_hook`](crate::compaction_hook) owns *whether* to cut, and how much to keep —
//! [`CompactionPolicy::plan`] is the product decision. [`compaction_summary`](crate::compaction_summary)
//! owns *what the model sees in place of the dropped span* — [`summarize`](crate::summarize_for_compaction)
//! and [`render`](crate::render_compaction) are the deterministic floor.
//!
//! Each of the three was independently testable and independently correct, which is exactly how a
//! pipeline ends up with a hole in it: the loop knew how to plan but summarised the dropped span by
//! hand, so the "did the plan's cut agree with the pairing rule" question and the "is the summary
//! worth inserting" question were answered in a caller instead of in one place. This module is that
//! one place. Its only purpose is to make the three stages impossible to run out of order or with a
//! stage skipped.
//!
//! # Why the order is fixed
//!
//! Deciding first is what keeps the work proportional to the history: a policy that says
//! [`KeepAll`](crate::CompactionPlan::KeepAll) must not pay for a summary of a span that is going to
//! survive anyway. Cutting second is what makes the summary honest: the span handed to
//! `summarize` is the *decided* span, not a span reconstructed afterwards from what is missing.
//! Splicing last is what makes the result a single answer — a `kept` vector a caller can install
//! without doing arithmetic of its own.
//!
//! # Why the pairing predicate is injected
//!
//! A legal cut requires that no tool call is left unanswered, and this layer cannot answer that for
//! itself. It holds messages, while the legality rule is stated over the *event stream*: one
//! assistant message carries every call its response emitted while the log records each call as its
//! own event, a `tool/result` event has no message of its own beyond the `Tool` role that answers a
//! call, and a result whose call never appears is representable. Two histories that look identical
//! as messages can therefore differ in legality, so the caller — which holds the events — supplies
//! the predicate, and this module never guesses.
//!
//! The predicate's convention is deliberately the one [`pairing_balanced_at`](crate::pairing_balanced_at)
//! already uses: `balanced(i)` answers "is a cut immediately *after* entry `i` legal", with an index
//! at or past the end trivially balanced. Reusing the convention means the event-stream check the
//! caller passes in and the message-derived check inside `plan` are asking the same question, so a
//! disagreement is a real disagreement rather than a units mismatch.
//!
//! # Why refusing to compact and not needing to are reported differently
//!
//! [`Outcome::skipped`] is not a boolean. "The history is three messages long" and "the policy
//! refuses because every message is the pinned system prefix" produce the same empty diff and mean
//! opposite things to whoever reads the log: the first says nothing is wrong, the second says
//! something is configured or shaped unusually. A caller that only learns `None` cannot tell them
//! apart, and ends up either logging nothing or logging the wrong thing.

use std::fmt::Write as _;

use girder_core::{
    ChatMessage, CompactionPlan, CompactionPolicy, checkpoint_message, history_cost,
    render_compaction, summarize_for_compaction,
};

/// What one compaction pass produced.
///
/// Every pass produces a full history, never a mutation: a caller installs [`kept`](Self::kept) as
/// the new conversation, and a pass that changed nothing hands back the input so that the "nothing
/// happened" path needs no special case at the call site.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// The messages to carry forward, in order.
    ///
    /// When a summary was produced it is the first element — see [`run`] for why — and the surviving
    /// messages follow in their original order. When nothing was compacted this is the input.
    pub kept: Vec<ChatMessage>,
    /// The summary that stands in for the dropped span, if one was produced.
    ///
    /// The same text the leading checkpoint message carries. It is returned separately as well
    /// because the two audiences differ: the checkpoint is for the model, while the string is for
    /// logs, transcripts, and assertions, none of which should have to unpack a message to find it.
    pub summary: Option<String>,
    /// Why nothing was compacted, when nothing was.
    ///
    /// `None` exactly when [`summary`](Self::summary) is `Some`: a pass either compacted or it did
    /// not, and a reason without a skip (or the reverse) would be a bug in this module rather than a
    /// state a caller has to handle.
    pub skipped: Option<String>,
}

impl Outcome {
    /// Whether this pass actually replaced a span with a summary.
    ///
    /// Cheaper to read than matching on [`summary`](Self::summary), at call sites where the reason
    /// is not wanted.
    #[must_use]
    pub fn compacted(&self) -> bool {
        self.summary.is_some()
    }

    /// A pass that changed nothing, handing the input back unchanged.
    fn not_compacted(messages: &[ChatMessage], reason: String) -> Self {
        Self {
            kept: messages.to_vec(),
            summary: None,
            skipped: Some(reason),
        }
    }
}

/// Runs the whole compaction pass over `messages`.
///
/// 1. [`CompactionPolicy::plan`] decides whether a cut is warranted, and which span it would be.
/// 2. The cut is checked against `balanced`, the caller's event-stream reading of
///    [`pairing_balanced_at`](crate::pairing_balanced_at). The policy derives its own pairing from
///    the messages, so this is the authoritative check rather than a repeat of one: where the two
///    disagree, the event stream is right and the pass is refused.
/// 3. [`summarize_for_compaction`] and [`render_compaction`] turn the decided span into the text that
///    replaces it.
/// 4. The summary is inserted *in front of* the kept messages, as a checkpoint in the landed shape,
///    because it describes what came before the first of them. Anywhere else it would read as a
///    remark about the middle of a conversation the reader has not been shown.
///
/// `balanced(index)` must answer, for a cut immediately *after* message `index`, whether no tool
/// call is left unanswered; an index at or past the end is trivially balanced. That is the
/// convention of [`pairing_balanced_at`](crate::pairing_balanced_at), which is the function this
/// predicate stands in for — see the module documentation for why it cannot be called from here.
///
/// Returns [`Outcome`] rather than `Result`: there is no error state. "Not compacted" is an ordinary
/// answer with a reason attached, and a caller that had to unwrap a `Result` to find the unchanged
/// history would be encoding a policy decision as a failure.
#[must_use]
pub fn run(
    messages: &[ChatMessage],
    policy: &CompactionPolicy,
    balanced: impl Fn(usize) -> bool,
) -> Outcome {
    if messages.is_empty() {
        return Outcome::not_compacted(
            messages,
            "the history is empty, so there is nothing to compact".to_owned(),
        );
    }

    let (keep, drop) = match policy.plan(messages) {
        CompactionPlan::KeepAll => {
            return Outcome::not_compacted(messages, nothing_to_do(messages, policy));
        }
        CompactionPlan::Refuse { reason } => {
            // The policy's own words are kept: it is the layer that knows which rule refused, and
            // rewording it here would be a second, worse copy of that explanation.
            return Outcome::not_compacted(
                messages,
                format!("the policy refused to compact: {reason}"),
            );
        }
        // The plan's reason describes a compaction that this pass may still refuse, so it is not
        // surfaced on the success path; the facts a reader needs are the span and the summary.
        CompactionPlan::Compact { keep, drop, .. } => (keep, drop),
    };

    // `drop` is a contiguous ascending span by the plan's contract, so the cut is one past its last
    // index. Deriving it from `drop` rather than from the length of `keep` keeps the two in step
    // even if the pinned prefix is non-empty and `keep` is not one run of indices.
    let Some((first_dropped, last_dropped)) = drop.first().zip(drop.last()) else {
        return Outcome::not_compacted(
            messages,
            "the policy chose to keep every message after all: the span to drop is empty"
                .to_owned(),
        );
    };
    let cut = last_dropped + 1;

    // A cut at the very start of the history drops nothing, and a cut past the end keeps nothing;
    // both would be a plan this layer cannot honour, so they are refused rather than applied.
    if cut > messages.len() {
        return Outcome::not_compacted(
            messages,
            format!(
                "the plan cuts at message {cut}, past the end of a {}-message history",
                messages.len()
            ),
        );
    }

    // The cut sits between messages `cut - 1` and `cut`, and `cut` is at least one because the drop
    // span is non-empty and indices are 0-based. This is the check the whole module exists for: the
    // policy's pairing scan is a reading of the messages, while the caller's predicate is a reading
    // of the events, and a call that is still open here would reach the provider as an unanswered
    // tool request.
    if !balanced(cut - 1) {
        return Outcome::not_compacted(
            messages,
            format!(
                "the plan would drop messages {first_dropped}..{cut}, but the cut after message {} \
                 is not at a tool-pairing boundary",
                cut - 1
            ),
        );
    }

    let dropped: Vec<ChatMessage> = drop
        .iter()
        .filter_map(|index| messages.get(*index))
        .cloned()
        .collect();
    let summary = render_compaction(&summarize_for_compaction(&dropped));

    // A span whose summary renders empty — a run of checkpoints or of bare tool results, say —
    // would be replaced by nothing at all. That is strictly worse than not compacting: the history
    // shrinks and the reader is not told that anything was removed, which is the failure this
    // pipeline was built to end.
    if summary.is_empty() {
        return Outcome::not_compacted(
            messages,
            format!(
                "the span {first_dropped}..{cut} holds nothing a summary can record, so dropping it \
                 would leave a hole rather than a record"
            ),
        );
    }

    let survivors: Vec<ChatMessage> = keep
        .iter()
        .filter_map(|index| messages.get(*index))
        .cloned()
        .collect();
    if survivors.is_empty() {
        return Outcome::not_compacted(
            messages,
            "the plan would keep no message, leaving the model with no recent history".to_owned(),
        );
    }

    // The checkpoint reuses the identity of the first message it replaces. That id is uniquely free
    // — the message is being dropped — and minting a fresh one would need a fallible constructor for
    // a literal, which this layer has no way to report. `checkpoint_message` is the shape the loop
    // already recognises: a `Recall`-form user message carrying both tags, so a later compaction
    // merges into it instead of re-summarising it as a request.
    let Some(first) = dropped.first() else {
        return Outcome::not_compacted(
            messages,
            "the span to drop is empty, so there is nothing to summarise".to_owned(),
        );
    };

    let mut kept = Vec::with_capacity(survivors.len() + 1);
    kept.push(checkpoint_message(&summary, first.id.clone()));
    kept.extend(survivors);

    Outcome {
        kept,
        summary: Some(summary),
        skipped: None,
    }
}

/// Why a [`CompactionPlan::KeepAll`] decision left the history alone, in words that distinguish the
/// two ways it happens.
///
/// The two are genuinely different: "not long enough yet" is the normal state of every conversation
/// and says nothing about configuration, while "the policy keeps as much as the history holds" means
/// no amount of waiting will help. Byte totals come from `history_cost` rather than being recomputed
/// here, so the number in the log is the number the trigger compared.
fn nothing_to_do(messages: &[ChatMessage], policy: &CompactionPolicy) -> String {
    let len = messages.len();
    if len <= policy.keep_recent {
        return format!(
            "the history holds {len} message(s) and the policy keeps the last {}, so compaction \
             would drop nothing",
            policy.keep_recent
        );
    }

    let mut reason = format!(
        "the history holds {len} message(s), below the {} message trigger",
        policy.trigger_messages
    );
    match policy.trigger_bytes {
        Some(limit) => {
            let cost = history_cost(messages);
            let _ = write!(
                reason,
                ", and its {cost} bytes are below the {limit} byte trigger"
            );
        }
        // Naming the disabled trigger is worth a clause: a reader seeing only the message count
        // would otherwise assume a size trigger had been considered and had not fired.
        None => reason.push_str(", and the policy sets no byte trigger"),
    }
    reason
}
