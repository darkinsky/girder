//! Incremental assembly of a chunk stream into complete content blocks.
//!
//! [`AssistantMessage::from_chunks`](crate::AssistantMessage::from_chunks) already folds a whole
//! stream, and that is the right shape when the caller has every chunk in hand. It is the *wrong*
//! shape when the caller has to react while the response is still arriving: a renderer wants a
//! block the moment it closes, and a tool call can be dispatched before the model has finished the
//! block that follows it. A fold that runs at the end cannot do either, because nothing exists
//! until the stream is over.
//!
//! # Interleaving is the reason this module exists
//!
//! A stream is a sequence of deltas tagged with a block index, and nothing in the protocol says the
//! indices arrive in order or contiguously. A provider may start block 0, start block 1, then
//! alternate deltas between them — that is what streaming two blocks concurrently looks like on the
//! wire. An assembler that keeps one "current block" slot handles the simple case and corrupts
//! every real one: the second `block-start` either resets the first block or the deltas of both
//! blocks are appended to whichever slot was last written. Keeping one partial per index is the
//! whole point, so blocks are held in an index-keyed map rather than a single slot.
//!
//! # Errors are the contract, not noise
//!
//! A delta for an index that never started, or a second start for the same index, means the
//! upstream produced something this module does not model. Dropping it silently would surface
//! later as a missing block or a rewritten one, far from the cause, so both are reported. The same
//! reasoning is why [`Assembler::finish`] refuses to return a block that never closed: half a tool
//! call is not a tool call, and returning it would let a truncated response look complete.
//!
//! Closing is one-way. Deltas that arrive after a block closed are ignored rather than reported,
//! matching upstream: a provider can trail a stray delta after `block-end`, and accepting it would
//! rewrite content the caller has already been handed.

use std::collections::{BTreeMap, BTreeSet};

use girder_core::{ContentBlock, StreamChunk, ToolCallId};

/// Block type tag for visible text.
const TEXT: &str = "text";
/// Block type tag for model reasoning.
const REASONING: &str = "reasoning";
/// Block type tag for a tool invocation.
const TOOL_CALL: &str = "tool-call";

/// The state of one block index: the deltas seen so far, and the closing block if one arrived.
#[derive(Debug)]
struct Partial {
    /// The type announced by `block-start`, or the closing block's own tag when no start arrived.
    ///
    /// Authoritative for how the accumulated buffers are read: a partial accumulates into one text
    /// or tool buffer, and only this tag says which of them the result is.
    kind: String,
    /// Text accumulated for this index, whichever of visible or reasoning the tag says it is.
    ///
    /// A partial is one block of one kind, so one buffer serves both: keeping two would need a
    /// rule for a stream that announces `reasoning` and sends text deltas, and there is no such
    /// rule — the announced kind wins.
    text: String,
    /// Call id, when a delta carried one.
    ///
    /// Providers differ on whether the id arrives with the first delta or with every one, so it is
    /// remembered from the first delta that has it rather than required on the start.
    tool_call_id: Option<ToolCallId>,
    /// Tool name, accumulated because a provider may stream it in pieces.
    tool_call_name: String,
    /// Raw JSON arguments, concatenated verbatim.
    tool_arguments: String,
    /// The authoritative block from `block-end`. Set exactly once; its presence *is* "closed".
    block: Option<ContentBlock>,
}

impl Partial {
    /// Starts an open partial of `kind`.
    fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            text: String::new(),
            tool_call_id: None,
            tool_call_name: String::new(),
            tool_arguments: String::new(),
            block: None,
        }
    }

    /// Whether a closing block has arrived.
    fn is_closed(&self) -> bool {
        self.block.is_some()
    }

    /// Turns the partial into a block, or `None` when it holds nothing worth emitting.
    ///
    /// A closed block normally wins outright. The exception is an *empty* text or reasoning block:
    /// providers use that shape as a placeholder on `block-end` and rely on the deltas for the
    /// content (this project's own adapter does exactly that), so taking it verbatim would deliver
    /// an empty answer for a response that streamed text.
    fn into_block(self, index: usize) -> Option<ContentBlock> {
        let Self {
            kind,
            text,
            tool_call_id,
            tool_call_name,
            tool_arguments,
            block,
        } = self;
        match block {
            Some(block) if !is_placeholder(&block) => Some(block),
            // Either no closing block arrived, or the one that did is a placeholder: in both cases
            // the accumulated deltas are the only real content.
            _ => assemble_deltas(
                &kind,
                &text,
                tool_call_id.as_ref(),
                &tool_call_name,
                &tool_arguments,
                index,
            ),
        }
    }
}

/// Whether `block` is a placeholder rather than real content.
///
/// Two shapes qualify. An empty text or reasoning block is what this project's own adapter sends
/// on `block-end`, relying on the deltas for the content. A tool call with neither a name nor
/// arguments cannot be an invocation at all, so taking it verbatim would replace a call the model
/// made with an uncallable one. In both cases the accumulated deltas are the only real content.
fn is_placeholder(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.is_empty(),
        ContentBlock::ToolCall {
            name, arguments, ..
        } => name.is_empty() && arguments.is_empty(),
        ContentBlock::Image { .. } | ContentBlock::File { .. } => false,
    }
}

/// Builds a block from accumulated deltas, or `None` when they carry no content.
///
/// `None` is not an error: an empty text block is indistinguishable from a placeholder, and an
/// unknown block type never says anything content-bearing through deltas at all.
fn assemble_deltas(
    kind: &str,
    text: &str,
    tool_call_id: Option<&ToolCallId>,
    tool_call_name: &str,
    tool_arguments: &str,
    index: usize,
) -> Option<ContentBlock> {
    match kind {
        TEXT if !text.is_empty() => Some(ContentBlock::Text {
            text: text.to_owned(),
        }),
        REASONING if !text.is_empty() => Some(ContentBlock::Reasoning {
            text: text.to_owned(),
        }),
        TOOL_CALL => {
            // A tool call with neither a name nor arguments carries no invocation: emitting it
            // would put an uncallable block in the message.
            if tool_call_name.is_empty() && tool_arguments.is_empty() {
                return None;
            }
            // The id only correlates a call with its result, so a provider that never sent one
            // still gets a call the model asked for. The synthesized id is index-derived rather
            // than random so a replayed stream assembles to the same message. If the derived id
            // cannot be built (impossible for an index, and not worth a panic in production code),
            // the call is dropped rather than carried with an id that cannot be correlated.
            let id = match tool_call_id {
                Some(id) => id.clone(),
                None => ToolCallId::new(format!("call-{index}")).ok()?,
            };
            Some(ContentBlock::ToolCall {
                id,
                name: tool_call_name.to_owned(),
                arguments: tool_arguments.to_owned(),
            })
        }
        _ => None,
    }
}

/// Narrows a stream index onto the error type's `u32`.
///
/// Indices are `usize` on the stream and `u32` in the error, so an index no block could ever have
/// saturates rather than wrapping: a wrapped number in a diagnostic points at the wrong block.
fn narrow(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

/// Assembles streamed chunks into complete content blocks, one partial per block index.
///
/// See the [module docs](self) for why interleaving, not the happy path, drives the design.
#[derive(Debug, Default)]
pub struct Assembler {
    /// Open and not-yet-taken closed partials, keyed by index.
    ///
    /// A `BTreeMap` because the output must be in index order and a stream is free to announce
    /// blocks out of order; sorting on every read would be the same cost with more code.
    partials: BTreeMap<usize, Partial>,
    /// Indices already handed out by [`Assembler::take_finished`].
    ///
    /// Kept so a delta that trails its block by one chunk is ignored instead of reported as an
    /// orphan: it is the same malformed-stream case, but the block it belongs to is gone, and only
    /// this set remembers that it existed.
    taken: BTreeSet<usize>,
}

impl Assembler {
    /// Creates an assembler with no blocks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk into the assembler.
    ///
    /// Usage and finish chunks carry accounting rather than content and are ignored: the caller
    /// reads them off the same stream, and folding them in here would give the assembler two
    /// unrelated jobs.
    ///
    /// # Errors
    ///
    /// Returns [`AssemblyError::OrphanDelta`] for a delta whose block never started, and
    /// [`AssemblyError::DuplicateStart`] for a second start of the same index.
    pub fn push(&mut self, chunk: StreamChunk) -> Result<(), AssemblyError> {
        match chunk {
            StreamChunk::BlockStart { index, block_type } => {
                if self.partials.contains_key(&index) || self.taken.contains(&index) {
                    return Err(AssemblyError::DuplicateStart {
                        index: narrow(index),
                    });
                }
                self.partials.insert(index, Partial::new(block_type));
                Ok(())
            }

            // Both text kinds accumulate the same way: the announced block type, not the delta
            // variant, decides what the buffer becomes.
            StreamChunk::TextDelta { index, text }
            | StreamChunk::ReasoningDelta { index, text } => self.append_text(index, &text),

            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let Some(partial) = self.open(index)? else {
                    return Ok(());
                };
                if let Some(name) = name {
                    partial.tool_call_name.push_str(&name);
                }
                partial.tool_arguments.push_str(&arguments_delta);
                // First id wins. Only the `block-start` event carries the real id; an
                // arguments-only delta repeats a placeholder, and letting it overwrite would
                // replace the id the call is dispatched and matched by.
                if partial.tool_call_id.is_none() {
                    partial.tool_call_id = Some(id);
                }
                Ok(())
            }

            StreamChunk::BlockEnd { index, block } => {
                if let Some(partial) = self.partials.get_mut(&index) {
                    // First close wins. A re-close is a straggler from the same malformed stream,
                    // and honouring it would change a block the caller may already hold.
                    if !partial.is_closed() {
                        partial.block = Some(block);
                    }
                    return Ok(());
                }
                if self.taken.contains(&index) {
                    return Ok(());
                }
                // A closing block carries the finished content, so it does not need a start to be
                // interpretable. Providers that emit whole blocks without streaming deltas exist,
                // and refusing them would reject a perfectly complete stream.
                let mut partial = Partial::new(block.type_name());
                partial.block = Some(block);
                self.partials.insert(index, partial);
                Ok(())
            }

            StreamChunk::Usage(_) | StreamChunk::Finish { .. } => Ok(()),
        }
    }

    /// Appends text to the open partial at `index`.
    fn append_text(&mut self, index: usize, delta: &str) -> Result<(), AssemblyError> {
        let Some(partial) = self.open(index)? else {
            return Ok(());
        };
        partial.text.push_str(delta);
        Ok(())
    }

    /// The open partial at `index`, `None` when the block is already closed or taken.
    ///
    /// One lookup serves both delta kinds, so "did this block ever start" is answered in exactly
    /// one place: a missing partial that was never taken is an orphan, everywhere.
    fn open(&mut self, index: usize) -> Result<Option<&mut Partial>, AssemblyError> {
        match self.partials.get_mut(&index) {
            // A closed block ignores trailing deltas; see the module docs.
            Some(partial) if partial.is_closed() => Ok(None),
            Some(partial) => Ok(Some(partial)),
            None if self.taken.contains(&index) => Ok(None),
            None => Err(AssemblyError::OrphanDelta {
                index: narrow(index),
            }),
        }
    }

    /// Takes the blocks that have closed, in index order, leaving the open ones in place.
    ///
    /// Draining rather than copying is what lets a caller render incrementally without holding
    /// every block twice; the index of a taken block is remembered only as "already seen", so a
    /// late delta for it is ignored rather than resurrecting it.
    pub fn take_finished(&mut self) -> Vec<ContentBlock> {
        let closed: Vec<usize> = self
            .partials
            .iter()
            .filter(|(_, partial)| partial.is_closed())
            .map(|(index, _)| *index)
            .collect();
        let mut finished = Vec::with_capacity(closed.len());
        for index in closed {
            let Some(partial) = self.partials.remove(&index) else {
                continue;
            };
            self.taken.insert(index);
            if let Some(block) = partial.into_block(index) {
                finished.push(block);
            }
        }
        finished
    }

    /// Consumes the assembler and returns every block, in index order.
    ///
    /// # Errors
    ///
    /// Returns [`AssemblyError::Unclosed`] when any block is still open. A stream that ends
    /// mid-block was truncated or the provider died; returning the half-built blocks would make
    /// that indistinguishable from a complete answer, which is the one failure a caller cannot
    /// detect afterwards.
    pub fn finish(self) -> Result<Vec<ContentBlock>, AssemblyError> {
        let unclosed = self
            .partials
            .values()
            .filter(|partial| !partial.is_closed())
            .count();
        if unclosed > 0 {
            return Err(AssemblyError::Unclosed { count: unclosed });
        }
        Ok(self
            .partials
            .into_iter()
            .filter_map(|(index, partial)| partial.into_block(index))
            .collect())
    }

    /// Whether any block has started and not closed yet.
    ///
    /// Lets a caller tell "the model is still inside a block" from "the model stopped between
    /// blocks" before deciding what an ending stream means.
    #[must_use]
    pub fn has_open_blocks(&self) -> bool {
        self.partials.values().any(|partial| !partial.is_closed())
    }
}

/// A failure while assembling a chunk stream into blocks.
///
/// Each variant names a disagreement between what the stream said and what the protocol allows,
/// because the caller's response differs per case: an orphan or duplicate is a bug to report, while
/// an unclosed block at the end is a truncation to surface as such.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AssemblyError {
    /// A delta arrived for an index whose block never started.
    #[error("delta for block {index} arrived with no `block-start`")]
    OrphanDelta {
        /// The index the delta claimed.
        index: u32,
    },

    /// A second `block-start` arrived for an index that is already assembling.
    #[error("second `block-start` for block {index}")]
    DuplicateStart {
        /// The index that started twice.
        index: u32,
    },

    /// The stream ended with blocks still open.
    #[error("{count} block(s) were still open when the stream ended")]
    Unclosed {
        /// How many blocks never received a `block-end`.
        count: usize,
    },
}
