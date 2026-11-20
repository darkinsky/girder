//! Selecting the final assistant output of one session.
//!
//! A session log is a sequence of events, and the answer a caller wants back from it — the child's
//! result in `subagent`, the body of an exported transcript, the preview in a session list — is the
//! *last* thing the model said. Upstream states the rule once, in
//! `packages/subagent/subagent/src/assistant-output.ts`, so that its run settlement and its
//! `subagent/end` event cannot disagree about it. This module is that rule for the Rust side; the
//! session vocabulary here already has [`SessionEvent::AssistantMessage`], but nothing extracted an
//! answer out of the log.
//!
//! # Why the last message rather than every message
//!
//! A turn is a sequence of steps, and a step that calls a tool produces an assistant message too.
//! Concatenating all of them would return the model's bookkeeping — "let me read the file" — along
//! with its answer, and a caller that wanted the answer would have to separate the two by guessing.
//! Later messages supersede earlier ones, which is exactly what the model itself believes: its next
//! step was written knowing what the previous step said.
//!
//! # Why event order, not turn number
//!
//! Selection is the last *appended* assistant message, not the one with the highest turn counter.
//! The two agree in a well-formed log, but the counter is metadata a replay must not have to trust:
//! a resumed or forked log can carry a turn number that no longer reflects append order, and a
//! selection rule that sorted by it would silently pick an older message. Metadata describes the
//! log; position in the log is the log.
//!
//! # Why an empty-content message is skipped
//!
//! Upstream selects the last *non-empty* message, and the reason is a real failure mode rather than
//! tidiness: after a step that hit the output limit with nothing executable in it, the loop appends
//! a message carrying no blocks only so the usage line has an owner. That message is accounting,
//! not an answer, so letting it erase the previous answer would make a truncated final step look
//! like a silent agent. A message with *blocks* is always output, even when every block is a tool
//! call — the model asked for work, and a caller that only wants prose reads
//! [`final_text`] instead.
//!
//! # What is deliberately not here
//!
//! Upstream also exposes an incremental `AssistantOutputFold` with a streamed-text fallback, for
//! backends that watch a child's output arrive and for transports that carry raw text chunks
//! instead of session events. Neither applies here yet: [`SessionEvent::AssistantMessage`] is the
//! only carrier of assistant output in this vocabulary, so the fold would have nothing to fold that
//! a scan of the finished log does not already have, and selecting from a complete slice is the
//! whole operation. The fallback is worth porting the day a streamed-text-only transport exists.

// `girder_core::…` rather than `crate::…` so this file also builds when a test includes it by path;
// the library declares the alias for itself, which is what makes both spellings the same crate.
use girder_core::{ContentBlock, SessionEvent};

/// The first turn number the session vocabulary uses.
///
/// A turn is numbered from one, so zero can mean "no turn was open" without colliding with a real
/// turn.
const NO_TURN: u64 = 0;

/// A session's final assistant output.
///
/// The blocks are kept whole rather than reduced to their text, because a caller deciding whether a
/// delegation succeeded needs the tool calls too: an agent that ended on a tool request is not a
/// silent agent, it is one that ran out of budget mid-work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalOutput {
    /// The message's blocks, in the order the model produced them.
    pub blocks: Vec<ContentBlock>,

    /// The turn the message belonged to.
    ///
    /// Assistant messages do not carry a turn of their own, so this is the turn that was open when
    /// the message was appended, taken from the last [`SessionEvent::TurnStart`] before it. It is
    /// zero when no turn was ever started, which means the log is a fragment rather than a session.
    pub turn: u64,
}

/// Selects the last assistant message in `events`.
///
/// `None` means the session produced no assistant output at all: it has no assistant message, or
/// only the empty-content messages that record usage after a truncated step (see the module docs
/// for why those do not count as output).
#[must_use]
pub fn final_output(events: &[SessionEvent]) -> Option<FinalOutput> {
    let (index, turn) = select(events)?;
    let SessionEvent::AssistantMessage { message } = &events[index] else {
        // `select` only ever returns the index of an `AssistantMessage`, so this arm is
        // unreachable. It returns `None` rather than panicking because production code carries no
        // `unwrap`: a future change to `select` would degrade to "no output", not to a crash in a
        // caller that cannot do anything about it.
        return None;
    };
    Some(FinalOutput {
        blocks: message.content.clone(),
        turn,
    })
}

/// Renders the final assistant output as text, concatenating its text blocks in order.
///
/// `None` means the output carried no text, not that it carried no output: a turn that ended by
/// asking for a tool has blocks but no prose, and a caller that treats the two cases identically
/// reports a working child as silent. Empty text blocks (`text: ""`) are skipped rather than
/// joined, so a placeholder cannot turn "no prose" into `Some("")`; whitespace is kept, because it
/// is what the model wrote.
#[must_use]
pub fn final_text(events: &[SessionEvent]) -> Option<String> {
    let (index, _) = select(events)?;
    let SessionEvent::AssistantMessage { message } = &events[index] else {
        return None;
    };

    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text: piece } = block {
            text.push_str(piece);
        }
    }

    if text.is_empty() { None } else { Some(text) }
}

/// Finds the index of the last non-empty assistant message, and the turn it belonged to.
///
/// One forward pass rather than a backward scan with an early exit: the turn has to be known at the
/// moment the message is seen, and a backward scan would have to re-scan for the nearest preceding
/// `TurnStart` — the same work, in the worse direction for a log that has assistant messages
/// everywhere. Only an index is kept per candidate, so a long log costs one comparison per event
/// and one clone at the end instead of one clone per message.
fn select(events: &[SessionEvent]) -> Option<(usize, u64)> {
    let mut turn = NO_TURN;
    let mut last: Option<(usize, u64)> = None;

    for (index, event) in events.iter().enumerate() {
        match event {
            SessionEvent::TurnStart { turn: started } => turn = *started,
            SessionEvent::AssistantMessage { message } if !message.content.is_empty() => {
                last = Some((index, turn));
            }
            _ => {}
        }
    }

    last
}
