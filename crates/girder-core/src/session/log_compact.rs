//! Shrinking a session log without changing what it says.
//!
//! A session log is append-only and is the source of truth for every derived view (see
//! [`crate::session`]). Two neighbouring modules already bound its size, and neither of them removes
//! redundancy:
//!
//! - [`crate::session::archive`] moves a *prefix* into a sibling file verbatim. Every byte it moves is
//!   still recorded, just elsewhere; it answers "this log is too long to decode on every step" by
//!   cutting history in half.
//! - [`crate::session::migration`] rewrites a row from an older *format generation* into today's
//!   spelling. It changes how a row is written, never how many rows there are.
//!
//! So the question "is any of this log redundant?" was never actually answered, and this module is
//! the answer. It is deliberately narrow, because the wrong answer here is silent data loss.
//!
//! # What the log actually contains that could be called redundant
//!
//! Measured over the logs this build produces, four kinds of repetition exist, and they are *not*
//! equally removable:
//!
//! | shape | what it means | removable |
//! |---|---|---|
//! | `step/start` with no matching `step/end` | a step that failed, was cancelled, or a process that died mid-step — [`crate::core::agent_loop`] returns early on error and skips its `step/end` | **no** — it is the evidence a step did not finish, and [`crate::session::health`] reports exactly that |
//! | a repeated `turn/start` for one turn number | a resumed or forked log re-recording a boundary | **no** — boundaries are excluded on purpose, and every fold already deduplicates by turn number |
//! | a repeated `model/usage` record | one model call's cost written twice by a retried write | yes, when consecutive and identical |
//! | a repeated `session/title` record | the same title announced again, e.g. after a rename back | yes, when the same title text appears later |
//!
//! The first two are the ones a reader is most likely to mistake for garbage, and the reason they
//! stay is worth stating plainly: a hole in a log is unrecoverable, and an unclosed step is a *fact*
//! ("this step did not finish"), not a duplicate of one.
//!
//! # Can a 10000-event log be made safely smaller?
//!
//! Only marginally, and the honest answer is that this module is not where log size is won. In a log
//! this build writes, the events that carry the bytes are `tool/call`, `tool/result`,
//! `assistant/message`, and `user/message` — every one of them is a verbatim record of something that
//! happened, and compacting any of them would be fabricating a history. What this module can remove
//! is a small tail of repeated bookkeeping: on a clean log it returns `removed == 0` and the log is
//! byte-for-byte the same length. The real reductions live elsewhere and are different trades:
//! spilling a large tool result ([`crate::spill`]) keeps the result but moves its bytes, and folding a
//! span into a summary ([`crate::compaction`]) is lossy by definition and belongs to a model.
//!
//! # What "semantic equivalence" means here
//!
//! The contract is about **playback**: replaying the compacted log must reconstruct the same
//! conversation — the same prompts, answers, tool calls and results, in the same order, inside the
//! same turns and steps. Every rule below is chosen to keep that true, and
//! [`crate::export_transcript`], [`crate::digest_session`],
//! [`crate::session_outline`], and [`crate::final_text`] are what the
//! tests compare to prove it.
//!
//! The one thing that legitimately changes is *accounting of a duplicated write*. A run of identical
//! `model/usage` records is read by [`crate::SessionStats`] as a sum, so removing one lowers
//! that sum; and an empty `assistant/message` is counted as a message, so removing one lowers that
//! count. Both are records of the log mechanism rather than of the conversation, which is exactly why
//! they are the removable ones — but a caller that treats the log as an invoice should know it.
//!
//! # Positions shift, so stored cursors go stale
//!
//! No event is rewritten and no field is renumbered, but the *position* of every event after the
//! first removal moves. Sequence numbers ([`crate::SessionStore::read_after`]) are positional, so a
//! cursor or [`crate::RewindPoint`] captured before compaction refers to a different event
//! afterwards, and [`crate::SessionEvent::SessionTitle`]'s `message_seqs` names message positions the
//! same way. This is stated rather than repaired: repairing a cursor would mean inventing a mapping
//! the caller did not ask for, and a caller that compacts knows it moved the log.

use std::collections::BTreeSet;

use girder_core::{ChatRole, ContentBlock, SessionEvent};

/// What one compaction did.
///
/// A result type rather than a bare `Vec`: a caller has to be able to tell "nothing was redundant"
/// from "I compacted" without diffing two logs, and an operator reading a log-size report needs the
/// reasons, not just the count.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compacted {
    /// The events that remain.
    ///
    /// Every one is the original event, unmodified and in its original relative order. The only
    /// difference from the input is that some events are absent.
    pub events: Vec<SessionEvent>,

    /// How many events were removed or merged away.
    ///
    /// Always `input.len() - events.len()`, so a caller never has to trust this number to report a
    /// size change — it can check. Zero means the input was already compact.
    pub removed: usize,

    /// Why, one line per rule that fired, each naming how many records it removed.
    ///
    /// Only rules that actually removed something appear. An empty vector with a non-zero `removed`
    /// would be a compaction nobody can audit, so the two are always consistent.
    pub reasons: Vec<String>,
}

/// Compacts an event log.
///
/// **Only semantically equivalent compactions are performed** — nothing that changes what the
/// conversation said or did. Three rules exist, and each is restricted far more tightly than the
/// category it belongs to:
///
/// 1. A run of **consecutive identical** `model/usage` records collapses to its last member. Two
///    records that name different models or different token counts are two calls and both stay; only
///    a byte-identical neighbour can be a rewritten record.
/// 2. An `assistant/message` with no visible content — no blocks at all, or text blocks that are
///    empty or whitespace-only — is removed, but **only when some later assistant message still has
///    content**. A message carrying a tool call, an image, a file, or reasoning is *not* empty and
///    stays. The trailing-clause guard is not defensive noise: [`crate::final_output`] selects the
///    *last* assistant message with a non-empty content vector and then reads its text, so removing
///    the final one would promote an earlier answer and change what "the model's last word" is.
/// 3. An earlier `session/title` is removed when a **later** title carries the same text. A rename
///    chain (different texts) is history and stays; the same title announced twice is not, because
///    every consumer of a title reads the last one.
///
/// Never compacted, whatever they look like: `tool/call`, `tool/result`, `turn/start`, `turn/end`,
/// `user/message`, and the step boundaries. See the module documentation for why the unpaired
/// `step/start` a failed step leaves behind is evidence rather than garbage.
///
/// # Shape of the answer
///
/// Two identical `model/usage` records followed by a prompt come back as one usage record and the
/// prompt, with `removed == 1` and exactly one reason naming the duplicate-usage rule. A log with
/// nothing to remove comes back with the same events, `removed == 0`, and no reasons — which is the
/// common answer, because the events that carry a log's bytes are the ones that carry its meaning.
#[must_use]
pub fn compact(events: &[SessionEvent]) -> Compacted {
    let plan = plan(events);
    // Sized for the common case — a log with nothing to remove — without trusting it: the vector
    // grows if the plan turns out to be conservative.
    let mut kept = Vec::with_capacity(events.len().saturating_sub(plan.removed()));
    for (event, keep) in events.iter().zip(plan.keep.iter()) {
        // Zip rather than indexing: the plan is built from this same slice, so a short `keep` would be
        // a bug, and zip degrades to dropping the tail instead of panicking in production.
        if *keep {
            kept.push(event.clone());
        }
    }
    Compacted {
        events: kept,
        removed: plan.removed(),
        reasons: plan.reasons(),
    }
}

/// Whether [`compact`] would remove anything.
///
/// Cheap enough to call before deciding to rewrite a file: it runs the same analysis as [`compact`]
/// and stops at a count, without cloning an event. A caller that needs the events themselves should
/// call [`compact`] directly rather than asking this first and then asking again.
#[must_use]
pub fn is_compactable(events: &[SessionEvent]) -> bool {
    plan(events).removed() > 0
}

/// Which events a compaction would keep, and how many each rule accounts for.
///
/// One analysis, three rules, and no cloning. Separated from [`compact`] so that
/// [`is_compactable`] and [`compact`] can never disagree about what is removable — two
/// implementations with the same name is how the predicate and the transformation drift apart.
#[derive(Debug)]
struct Plan {
    /// One flag per input event, in input order. `true` keeps the event.
    keep: Vec<bool>,
    /// How many `model/usage` records the duplicate rule removes.
    duplicate_usage: usize,
    /// How many `assistant/message` records the empty-message rule removes.
    empty_assistant: usize,
    /// How many `session/title` records the superseded-title rule removes.
    superseded_title: usize,
}

impl Plan {
    /// An empty plan that keeps everything.
    fn keeping(len: usize) -> Self {
        Self {
            keep: vec![true; len],
            duplicate_usage: 0,
            empty_assistant: 0,
            superseded_title: 0,
        }
    }

    /// Total events to remove, which is the sum of what each rule claimed.
    ///
    /// Summed rather than counted from `keep` so that a rule which marks an event twice — the kind of
    /// mistake that would silently under-report a removal — cannot make the total disagree with the
    /// reasons.
    fn removed(&self) -> usize {
        self.duplicate_usage
            .saturating_add(self.empty_assistant)
            .saturating_add(self.superseded_title)
    }

    /// One line per rule that fired, in a fixed order so a report is diffable between runs.
    fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.duplicate_usage > 0 {
            reasons.push(format!(
                "removed {} duplicate `model/usage` record(s): consecutive records naming the same \
                 model and the same token counts are one call written twice, so the last is kept",
                self.duplicate_usage
            ));
        }
        if self.empty_assistant > 0 {
            reasons.push(format!(
                "removed {} empty `assistant/message` record(s): no block, or only whitespace text, \
                 so replaying them says nothing",
                self.empty_assistant
            ));
        }
        if self.superseded_title > 0 {
            reasons.push(format!(
                "removed {} superseded `session/title` record(s): a later record states the same \
                 title, and every reader takes the last title in the log",
                self.superseded_title
            ));
        }
        reasons
    }
}

/// Decides every removal in one place.
fn plan(events: &[SessionEvent]) -> Plan {
    let mut plan = Plan::keeping(events.len());
    plan_duplicate_usage(events, &mut plan);
    plan_empty_assistant(events, &mut plan);
    plan_superseded_titles(events, &mut plan);
    plan
}

/// Marks all but the last record of every run of identical, consecutive `model/usage` events.
///
/// Consecutiveness is the whole safety argument. A provider writes one usage record per model call, so
/// two records that are *adjacent* and *identical* describe one call recorded twice; the same two
/// records separated by an assistant message or a tool call are two calls that happened to cost the
/// same, and both are kept. Requiring exact equality of the model and both token counts is what makes
/// "identical" mean identical rather than "similar enough".
fn plan_duplicate_usage(events: &[SessionEvent], plan: &mut Plan) {
    let mut start = 0usize;
    while start < events.len() {
        let Some(run) = identical_usage_run(events, start) else {
            start = start.saturating_add(1);
            continue;
        };
        // Keep the last of the run: it is the record a retried write most recently affirmed, and it is
        // at the position the first one occupies relative to the events after the run.
        for offset in 0..run.saturating_sub(1) {
            if let Some(keep) = plan.keep.get_mut(start.saturating_add(offset)) {
                if *keep {
                    *keep = false;
                    plan.duplicate_usage = plan.duplicate_usage.saturating_add(1);
                }
            }
        }
        start = start.saturating_add(run);
    }
}

/// How many consecutive `model/usage` records starting at `start` are identical, if any is.
///
/// `None` when the event at `start` is not a usage record, which is the caller's signal to advance by
/// one. A run is always at least one, so `Some(1)` means "a usage record with no duplicate next to it".
fn identical_usage_run(events: &[SessionEvent], start: usize) -> Option<usize> {
    let first = events.get(start)?;
    let SessionEvent::ModelUsage {
        model,
        input_tokens,
        output_tokens,
    } = first
    else {
        return None;
    };
    let mut run = 1usize;
    while let Some(next) = events.get(start.saturating_add(run)) {
        match next {
            SessionEvent::ModelUsage {
                model: next_model,
                input_tokens: next_input,
                output_tokens: next_output,
            } if next_model == model
                && next_input == input_tokens
                && next_output == output_tokens =>
            {
                run = run.saturating_add(1);
            }
            _ => break,
        }
    }
    Some(run)
}

/// Marks the `assistant/message` records that carry nothing a reader or a replay would see, and that
/// are not the message [`crate::final_output`] would select.
///
/// Backwards, because the guard on this rule is a fact about the suffix: an empty record may only go
/// when a *later* assistant message still has a non-empty content vector. [`crate::final_output`] and
/// [`crate::final_text`] read the last such message, so removing the last one would promote an
/// earlier answer into "what the model said last" — a different conversation, not a shorter log.
fn plan_empty_assistant(events: &[SessionEvent], plan: &mut Plan) {
    let mut later_content = false;
    for (index, event) in events.iter().enumerate().rev() {
        let SessionEvent::AssistantMessage { message } = event else {
            continue;
        };
        if is_empty_assistant(message) {
            // A message with no blocks at all is skipped by every selector that looks at `content`,
            // so its position does not matter. A message whose blocks are all whitespace text is
            // *not* skipped — [`crate::final_output`] sees a non-empty `content` and returns it — so
            // it may only go once a later message supersedes it.
            let removable = message.content.is_empty() || later_content;
            if removable {
                if let Some(keep) = plan.keep.get_mut(index) {
                    if *keep {
                        *keep = false;
                        plan.empty_assistant = plan.empty_assistant.saturating_add(1);
                    }
                }
            }
        }
        // Updated after the decision, so a message never counts as its own successor.
        if !message.content.is_empty() {
            later_content = true;
        }
    }
}

/// Whether an assistant message has no visible content.
///
/// The role check is not redundant with the variant: a log is decoded from disk, and a hand-edited or
/// migrated file can put a `user`-role message in an `assistant/message` record. Removing that one
/// would delete a prompt, which is the one thing this module must never do, so a mismatched role is
/// treated as content this rule does not understand and the event stays.
///
/// A message is empty only when it has no blocks at all, or when every block is whitespace-only text.
/// Reasoning, image, file, and tool-call blocks are all content: reasoning is what the model actually
/// produced, the attachment references point at bytes elsewhere, and a tool call is the request half
/// of an exchange. None of them may be treated as "nothing".
fn is_empty_assistant(message: &girder_core::ChatMessage) -> bool {
    if message.role != ChatRole::Assistant {
        return false;
    }
    message.content.iter().all(|block| match block {
        ContentBlock::Text { text } => text.trim().is_empty(),
        ContentBlock::Reasoning { .. }
        | ContentBlock::ToolCall { .. }
        | ContentBlock::Image { .. }
        | ContentBlock::File { .. } => false,
    })
}

/// Marks every `session/title` whose text a later record also states.
///
/// Backwards, because "a later record says the same thing" is a fact about the suffix, and a forward
/// pass would have to remember candidates it might never confirm. Text equality rather than full
/// record equality: the `source` and `message_seqs` of a re-announcement are not what any reader of a
/// title consults — [`crate::session::preview`], [`crate::session::filter`], and the title projection
/// all take the last title in the log — so a differing `source` on the same text is still the same
/// title as far as anything downstream can tell.
///
/// A rename chain is deliberately preserved: different texts are different titles, and the sequence of
/// what a session was called is history, not redundancy.
fn plan_superseded_titles(events: &[SessionEvent], plan: &mut Plan) {
    let mut later: BTreeSet<&str> = BTreeSet::new();
    for (index, event) in events.iter().enumerate().rev() {
        let SessionEvent::SessionTitle { title, .. } = event else {
            continue;
        };
        if later.contains(title.as_str()) {
            if let Some(keep) = plan.keep.get_mut(index) {
                if *keep {
                    *keep = false;
                    plan.superseded_title = plan.superseded_title.saturating_add(1);
                }
            }
            continue;
        }
        later.insert(title.as_str());
    }
}
