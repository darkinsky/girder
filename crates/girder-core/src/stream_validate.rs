//! Shape validation for a chunk stream, kept separate from assembling it.
//!
//! [`crate::BlockAssembler`] and [`crate::assemble_message`] turn chunks into blocks. They report
//! the disagreements they cannot work around — an orphan delta, a duplicate start, an unclosed
//! block at the end — but they are deliberately permissive about everything else: trailing deltas
//! after a `block-end` are dropped, a `block-end` with no start is accepted as a complete block,
//! and usage after the terminal `finish` is folded into the accounting as if it had arrived in
//! order. Each of those is the right call *for an assembler*, whose job is to produce the best
//! message it can. None of them is the right call *for a caller that wants to know whether the
//! stream it just consumed was well formed*, because the assembler's tolerance turns the answer
//! into "here is a message, trust it".
//!
//! This module answers the second question. It folds the stream without building anything and
//! reports every place where the stream contradicts itself, so a caller can log, count, or refuse.
//!
//! # Not every problem is fatal, and the distinction is the point
//!
//! Treating "any problem at all" as fatal would make this module unusable against real providers.
//! The classification below is what [`is_fatal`] encodes:
//!
//! * **`IndexGap` is not fatal.** The protocol puts no ordering requirement on block indices:
//!   `OpenAI`'s `tool_calls[].index` is a discriminator between parallel calls, not a sequence
//!   number, and parallel calls can be reported out of order or with gaps when a gateway drops
//!   one. The assembler keys its partials by index rather than appending to a cursor, so a gap
//!   costs nothing. It is still *recorded*, because a consumer that assumes contiguous indices
//!   (an array indexed by block number, say) would silently misplace content, and this is the only
//!   place that assumption is visible.
//! * **`NoBlock` and `AfterEnd` are fatal.** Both mean a delta and a block disagree about whether
//!   the block exists. `NoBlock` is content that belongs nowhere: `AssistantMessage::from_chunks`
//!   drops an orphan text delta without a trace, and materializes an orphan tool-call delta into a
//!   block that was never announced — silently losing or inventing content. `AfterEnd` is content
//!   that would rewrite a block the caller may already have been handed; the assembler ignores it
//!   to protect the delivered block, which means the delta is lost with no error. Either way the
//!   assembled message is not the message the stream described, and no later check can tell.
//! * **`Unclosed` and `UsageAfterFinish` are not fatal.** Concurrently open blocks are legal on
//!   this wire — the project's own `OpenAI` adapter opens reasoning, text, and tool blocks as the
//!   deltas arrive and closes them all at `finish` — so only a *second start for an index that is
//!   still open* is reported here, and that is a provider glitch whose consequence the assembler
//!   already refuses explicitly (`AssemblyError::DuplicateStart`). Usage after `finish` is
//!   accounting; the finish reason is still known, so the assembled message is unaffected.
//! * **`Truncated` is fatal only when the stream did not announce why it ended.** This is the case
//!   [`validate`] cannot decide on its own, because "the stream stopped mid-block" is exactly what
//!   a caller's own cancellation looks like. So [`validate_with`] takes [`Ending`], and
//!   [`validate`] passes [`Ending::Natural`]: a stream that stops with blocks open and no terminal
//!   `finish` is data loss, while the same shape under [`Ending::Cancelled`] is an ordinary cancel
//!   and is not reported at all. A terminal `finish` suppresses `Truncated` entirely, because a
//!   provider that says it is done and never closes a tool-call block is a documented shape this
//!   codebase relies on (see the `stream_blocks` module docs on implicit closing).
//!
//! A caller that cancelled therefore calls [`validate_with`] and gets a clean report; a caller
//! that read to the end calls [`validate`] and gets a fatal `Truncated` when the provider hung up
//! mid-block. Neither caller has to reinterpret a verdict.
//!
//! # What this module does not do
//!
//! It does not re-implement assembly, and it does not decide whether a stream is *acceptable* —
//! only whether its shape is self-consistent. A stream that validates cleanly can still assemble
//! to an empty message (an announced block that never carried content), and a stream with
//! problems can still assemble to something usable; the two questions are answered by different
//! code on purpose.

use std::collections::BTreeSet;

use girder_core::StreamChunk;

/// How the caller's reading of a stream ended.
///
/// The stream cannot record this, because a reader that stops early simply stops consuming and the
/// remaining chunks never exist. Passing it in is how a caller distinguishes "the provider hung up
/// mid-block" from "I cancelled and the half-block is expected".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The caller read until the stream ended on its own.
    ///
    /// Blocks still open at the end are unaccounted for: the provider stopped without saying it
    /// was done, so the missing `block-end` is data loss rather than an implicit close.
    Natural,
    /// The caller stopped reading before the stream ended (cancellation, timeout, shutdown).
    ///
    /// Open blocks are expected here and are not reported: an interrupted stream is truncated by
    /// construction, and flagging every cancellation would train callers to ignore the report.
    ///
    /// Constructed by *callers*, not by this crate: the crate root currently re-exports only
    /// [`validate`] and [`is_fatal`], so nothing inside the library names this variant yet. It is
    /// kept rather than removed because [`validate_with`] is the only way to ask for it, and a
    /// cancelling caller is the case the whole [`Ending`] parameter exists for.
    Cancelled,
}

/// One problem with a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Which chunk the problem was observed on, counted from 0.
    ///
    /// For [`ProblemKind::Truncated`] there is no offending chunk — the problem is the absence of
    /// one — so this is the position just past the last chunk.
    pub at: usize,
    /// What the stream did that this module can account for.
    pub kind: ProblemKind,
}

/// The kinds of disagreement a stream can contain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProblemKind {
    /// Indices skipped: block `from` was followed by block `to`, with `to > from + 1`.
    ///
    /// **Not an error.** `OpenAI`'s tool calls may arrive out of order and non-contiguously, and the
    /// assembler keys partials by index instead of assuming a cursor. Recorded because a consumer
    /// that *does* assume contiguous indices would silently misplace the block, and this is the
    /// only place that assumption becomes visible.
    IndexGap {
        /// The highest index started before the jump.
        from: usize,
        /// The index that started after the jump.
        to: usize,
    },
    /// A `block-start` arrived for an index whose previous `block-start` was never closed.
    ///
    /// Only the same-index restart is reported: two *different* indices open at once is legal on
    /// this wire and is how every interleaved stream looks, so reporting that would flag the
    /// project's own adapter on every response.
    Unclosed {
        /// The index that started again while still open.
        index: usize,
    },
    /// A delta arrived for a block that already ended (or a `block-start` tried to reopen it).
    ///
    /// Closing is one-way, so the content can neither be appended nor start a new block: it is
    /// lost. The assembler drops it silently to protect a block the caller may already hold, which
    /// makes this the failure a caller cannot discover from the assembled message.
    AfterEnd {
        /// The index the delta claimed.
        index: usize,
    },
    /// A delta arrived for an index whose block never started.
    ///
    /// The content belongs to no block. Folding the stream does not tell the caller: a `text` or
    /// `reasoning` delta grows only the text accumulator, never a slot, so it is dropped without a
    /// trace, while a `tool-call` delta does grow a slot and is materialized into a block that was
    /// never announced — silently losing or inventing content depending on the variant.
    NoBlock {
        /// The index the delta claimed.
        index: usize,
    },
    /// The stream ended with blocks still open.
    ///
    /// Only reported when nothing accounted for the ending: no terminal `finish`, and the caller
    /// did not cancel. Under [`Ending::Cancelled`] this is normal, and a terminal `finish` makes
    /// open blocks an implicit close — providers in this codebase leave tool-call blocks open and
    /// rely on exactly that.
    Truncated {
        /// The indices still open, ascending.
        open: Vec<usize>,
    },
    /// Token accounting arrived after the terminal `finish`.
    ///
    /// Not fatal: usage is bookkeeping and the finish reason is already known, so the assembled
    /// message is unaffected. Recorded because it means the provider kept sending after it declared
    /// the response complete, which is worth seeing in a log.
    UsageAfterFinish,
}

/// Validates a stream that the caller read to the end.
///
/// Equivalent to [`validate_with`] with [`Ending::Natural`]. A caller that cancelled should call
/// [`validate_with`] directly, or its expected truncation will be reported as data loss.
///
/// # Examples
///
/// A stream whose second block starts at index 2 reports exactly one problem, and it is not fatal:
///
/// ```text
/// BlockStart { index: 0 }, BlockEnd { index: 0 },
/// BlockStart { index: 2 }, BlockEnd { index: 2 },
/// Finish { reason: Stop }
///   -> [IndexGap { from: 0, to: 2 }], is_fatal == false
/// ```
///
/// The same stream without the trailing `Finish` is still not fatal, because every block closed:
/// only blocks left open make an unaccounted ending fatal.
#[must_use]
pub fn validate(chunks: &[StreamChunk]) -> Vec<Problem> {
    validate_with(chunks, Ending::Natural)
}

/// Validates a stream, told how the caller's reading of it ended.
///
/// See the [module docs](self) for why the ending has to be supplied and which problems depend on
/// it: it decides whether blocks left open at the end are a truncation worth reporting
/// ([`Ending::Natural`]) or the ordinary shape of a cancelled response ([`Ending::Cancelled`]).
#[must_use]
pub fn validate_with(chunks: &[StreamChunk], ending: Ending) -> Vec<Problem> {
    // Blocks that have started and not ended, and blocks that have ended. Kept as sets rather than
    // a single "current block" so interleaved streams — the normal case — are modelled exactly.
    let mut open: BTreeSet<usize> = BTreeSet::new();
    let mut closed: BTreeSet<usize> = BTreeSet::new();
    // The highest index started so far, for gap detection. Ordering is not required by the
    // protocol, so this is a high-water mark, not a cursor.
    let mut highest: Option<usize> = None;
    // Whether a terminal `finish` arrived, which is what turns open blocks into an implicit close.
    let mut finished = false;
    let mut problems = Vec::new();

    for (at, chunk) in chunks.iter().enumerate() {
        match chunk {
            StreamChunk::BlockStart { index, .. } => {
                if closed.contains(index) {
                    // Reopening a closed block is the same loss as a trailing delta: the block was
                    // handed out, and a second start cannot replace it.
                    problems.push(Problem {
                        at,
                        kind: ProblemKind::AfterEnd { index: *index },
                    });
                } else if !open.insert(*index) {
                    problems.push(Problem {
                        at,
                        kind: ProblemKind::Unclosed { index: *index },
                    });
                } else if let Some(from) = highest {
                    // Only jumps forward are gaps. Indices that arrive out of order (2 then 0) are
                    // exactly the case the assembler keys by index, so they are not reported as
                    // anything; `highest` is a high-water mark and never moves backwards.
                    if *index > from.saturating_add(1) {
                        problems.push(Problem {
                            at,
                            kind: ProblemKind::IndexGap { from, to: *index },
                        });
                    }
                }
                highest = Some(highest.map_or(*index, |seen| seen.max(*index)));
            }

            // All three delta variants carry content for a specific index, so they share one rule.
            StreamChunk::TextDelta { index, .. }
            | StreamChunk::ReasoningDelta { index, .. }
            | StreamChunk::ToolCallDelta { index, .. } => {
                if closed.contains(index) {
                    problems.push(Problem {
                        at,
                        kind: ProblemKind::AfterEnd { index: *index },
                    });
                } else if !open.contains(index) {
                    problems.push(Problem {
                        at,
                        kind: ProblemKind::NoBlock { index: *index },
                    });
                }
            }

            StreamChunk::BlockEnd { index, .. } => {
                // A second `block-end` for the same index is tolerated: closing is one-way, so the
                // first one already decided the block, and there is nothing left to corrupt. Not
                // reported because the assembler accepts the shape for providers that repeat it.
                open.remove(index);
                closed.insert(*index);
            }

            StreamChunk::Usage(_) => {
                if finished {
                    problems.push(Problem {
                        at,
                        kind: ProblemKind::UsageAfterFinish,
                    });
                }
            }

            StreamChunk::Finish { .. } => finished = true,
        }
    }

    if ending == Ending::Natural && !finished && !open.is_empty() {
        problems.push(Problem {
            // There is no chunk to blame, so the position points just past the stream.
            at: chunks.len(),
            kind: ProblemKind::Truncated {
                open: open.into_iter().collect(),
            },
        });
    }

    problems
}

/// Whether the problems mean the assembled content cannot be trusted.
///
/// Fatal: [`ProblemKind::NoBlock`], [`ProblemKind::AfterEnd`], and [`ProblemKind::Truncated`] —
/// each is content that was lost, content that cannot be applied, or content that never arrived.
/// Both callers and tests can rely on that: those are the shapes where the message the caller ends
/// up with is not the message the stream described, and no later check can repair it.
///
/// Not fatal, and deliberately so:
///
/// * [`ProblemKind::IndexGap`] — indices are a key, not a sequence; the assembler handles any order.
/// * [`ProblemKind::Unclosed`] — a same-index restart is already rejected by the assembler's own
///   `AssemblyError::DuplicateStart`; reporting it twice would not change the caller's options.
/// * [`ProblemKind::UsageAfterFinish`] — accounting only; the block content and finish reason are
///   unaffected.
///
/// `Truncated` is only ever emitted for an unaccounted ending (see [`validate_with`]), so this
/// predicate needs no knowledge of whether the caller cancelled: a cancelled stream never produces
/// the problem in the first place.
#[must_use]
pub fn is_fatal(problems: &[Problem]) -> bool {
    problems.iter().any(|problem| {
        matches!(
            problem.kind,
            ProblemKind::NoBlock { .. }
                | ProblemKind::AfterEnd { .. }
                | ProblemKind::Truncated { .. }
        )
    })
}
