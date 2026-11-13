//! An observing adapter that makes incremental block assembly usable by a streaming caller.
//!
//! [`girder_core::BlockAssembler`] can build blocks as chunks arrive, but nothing in the crate
//! calls it: the loop still buffers every chunk and folds the buffer once at the end with
//! `AssistantMessage::from_chunks`. This module is the seam between the two. It adds no assembly
//! logic of its own — it drives the assembler chunk by chunk and hands a block out the moment the
//! block closes, which is the whole reason the assembler exists.
//!
//! # Why an adapter rather than a rewrite
//!
//! `from_chunks` stays the reference implementation: it is the single place where "accumulate tool
//! arguments as a string" is decided, and every provider is assembled by it today. Rewriting the
//! loop onto a different assembler in the same change would leave two implementations of the
//! contract with no test tying them together. Instead, [`assemble_message`] is a drop-in for the
//! block-producing half of `from_chunks`, and the tests assert the two agree on the same input. A
//! disagreement is a defect in one of them, and the tests name which one and why.
//!
//! # Two jobs, deliberately separated
//!
//! * [`assemble_message`] takes a slice and returns every block, exactly as `from_chunks` does.
//!   It keeps every block in the assembler until the end, so the result is in index order even when
//!   blocks close out of order.
//! * [`StreamObserver`] takes one chunk at a time and returns only what closed *during that chunk*.
//!   That is a different question with a different answer, and the two are not interchangeable: a
//!   caller that drains incrementally must keep the blocks it was given, because they are removed
//!   from the assembler and [`StreamObserver::finish`] will not repeat them.
//!
//! # Where this module and `from_chunks` differ
//!
//! Both fold the same stream, but they do not agree on every input. The differences below are
//! covered by tests, so that the choice is recorded rather than accidental.
//!
//! 1. **Orphan delta.** A delta for an index that never started is reported as
//!    [`girder_core::AssemblyError::OrphanDelta`] here. `from_chunks` is inconsistent about the same
//!    input: an orphan *text* or *reasoning* delta is dropped outright — it never grew the slot
//!    vector, so materialization never visits that index — while an orphan *tool-call* delta grows
//!    the slot and is materialized into a block. Reporting is the better answer than either: an
//!    index that never started means the stream lost or reordered a `block-start`, so silently
//!    discarding the content hides data loss and silently promoting it invents a block; the error
//!    names the index, so the caller learns *which* block went missing.
//! 2. **Second `block-start` for one index.** Reported as
//!    [`girder_core::AssemblyError::DuplicateStart`]. `from_chunks` overwrites the recorded block
//!    kind and keeps accumulating, which merges two different blocks into one. Reporting is the
//!    better answer for the same reason as above, and more sharply: the merged block cannot be
//!    reconstructed at all.
//! 3. **Announced kind versus delta variant.** A block announced as `reasoning` that receives
//!    visible-text deltas becomes reasoning here (the announced kind wins) and visible text in
//!    `from_chunks` (the delta variant wins). Neither reading is derived from the protocol; a
//!    stream that contradicts itself is already malformed, and the two implementations resolve the
//!    contradiction in opposite directions. This module keeps the announced kind because
//!    `block-start` is the only chunk that states what a block *is*, while a delta says what one
//!    fragment looks like — and because a caller can see the mismatch in its own logs if it cares.
//!    Note that neither rule is fail-safe in both directions: `from_chunks` would expose
//!    reasoning-as-text when a block announced as reasoning streams text deltas, and this module
//!    would expose it when a block announced as text streams reasoning deltas.
//! 4. **Placeholder `block-end` on a tool call.** An empty text or reasoning block is a placeholder
//!    in both implementations, but a `block-end` carrying a tool call with no name and no arguments
//!    is a placeholder only here. `from_chunks` stores it verbatim, replacing a call the model
//!    actually made with one that cannot be invoked. Here it falls back to the accumulated deltas,
//!    which is strictly better: a nameless, argument-less call is not an invocation.
//! 5. **Missing terminal `finish`.** `from_chunks` rejects a stream that ends without `finish`
//!    (`AssembleError::Unfinished`) because it also reports the finish reason. This module only
//!    assembles blocks, so it cannot answer that question: it rejects a stream that ends with a
//!    block still open ([`girder_core::AssemblyError::Unclosed`]) and accepts one that ends cleanly
//!    between blocks. A caller that needs the terminal-chunk check keeps using `from_chunks` for it;
//!    the block contents are identical for every stream `from_chunks` accepts.
//!
//! # Closing a block the stream never closed
//!
//! Some providers never send `block-end` for a tool call: this project's own adapter pairs a
//! `content_block_stop` with a closing block only for text and reasoning, and emits nothing for
//! `tool_use`, because the arguments are complete once `message_delta` arrives. A stream like that
//! is complete, not truncated, so refusing to assemble it would make this module unusable with the
//! provider that ships in the same crate.
//!
//! The distinction that makes both cases decidable is the terminal `Finish` chunk: it says the
//! response ended, so any block still open at that point ended with it, and [`StreamObserver::finish`]
//! closes those blocks from their accumulated deltas. A stream that ends *without* `Finish` is
//! truncation, and there the open blocks are reported as `Unclosed` rather than handed out
//! half-built — half a tool call is not a tool call.

use std::collections::BTreeSet;

use girder_core::{AssemblyError, BlockAssembler, ContentBlock, StreamChunk};

/// The block used to close a partial the stream left open.
///
/// An empty text block is the one shape the assembler reads as "this closing block carries no
/// content of its own", and it then falls back to the deltas accumulated for the partial — whatever
/// kind that partial is. One placeholder therefore closes a text, a reasoning, and a tool-call block
/// alike, which is why this is a constant rather than a per-kind decision.
fn closing_placeholder() -> ContentBlock {
    ContentBlock::Text {
        text: String::new(),
    }
}

/// An observer that feeds chunks through the incremental assembler and reports closed blocks.
///
/// Feed one chunk with [`observe`](Self::observe) and it returns the blocks that closed *on that
/// chunk*; call [`finish`](Self::finish) at the end of the stream for what is left. A block is
/// never returned twice: `observe` drains it, and `finish` only sees what is still held.
///
/// A stream is malformed in the cases listed in the module docs (orphan delta, duplicate start), and
/// [`observe`](Self::observe) has no way to report that — it returns blocks, not a `Result`. The
/// first such error is therefore remembered, later chunks are ignored, and
/// [`finish`](Self::finish) returns the error. Blocks that closed before the error are still
/// delivered, so a caller that renders incrementally keeps what it already showed, and the error
/// tells it the stream was not trustworthy.
#[derive(Debug, Default)]
pub struct StreamObserver {
    /// The assembler doing the actual work; this type adds delivery, not assembly.
    assembler: BlockAssembler,
    /// Indices that started and have not seen a `block-end` yet.
    ///
    /// The assembler knows this too, but it does not expose *which* indices are open, and closing
    /// them at the end of a finished stream needs the indices to synthesize the missing
    /// `block-end`. Tracked here rather than added to the assembler so that the assembler keeps its
    /// one-way "closing is final" rule and no new public surface.
    open: BTreeSet<usize>,
    /// Blocks handed out so far, across `observe` and `finish`.
    completed: usize,
    /// The first protocol error seen, held until `finish` can return it.
    error: Option<AssemblyError>,
    /// Whether a terminal `Finish` chunk was seen; see the module docs on implicit closing.
    terminal_finish: bool,
}

impl StreamObserver {
    /// Creates an observer with no blocks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk, returning the blocks that completed *because of it*.
    ///
    /// The common case is an empty return: a block closes exactly once, on the chunk that carries
    /// its `block-end` (or, for a stream that never sends one, on [`finish`](Self::finish)). An
    /// empty vector means "nothing closed yet", not "nothing happened".
    ///
    /// The chunk is borrowed and cloned into the assembler, which consumes chunks by value. Cloning
    /// keeps the observer usable from a loop that needs to inspect or log the same chunk afterwards;
    /// a chunk is a few small strings, so the copy is cheaper than the ownership dance it avoids.
    ///
    /// Once a protocol error has been recorded, further chunks are ignored: continuing to feed a
    /// stream already known to be malformed would assemble blocks whose indices were shifted by the
    /// corruption, and returning those alongside the error invites a caller to render them.
    pub fn observe(&mut self, chunk: &StreamChunk) -> Vec<ContentBlock> {
        self.feed(chunk);
        self.drain()
    }

    /// Ends the stream and returns the blocks still held.
    ///
    /// Blocks already returned by [`observe`](Self::observe) are not repeated, so a caller that
    /// delivers incrementally must keep them and join them with this result — or, when it only
    /// needs the message, use [`assemble_message`], which never drains early.
    ///
    /// A block still open when the stream announced `Finish` is closed from its accumulated deltas:
    /// the provider said the response ended, and some providers never send a `block-end` for a tool
    /// call. A block still open when the stream never announced `Finish` is truncation and is
    /// reported as [`girder_core::AssemblyError::Unclosed`] instead of being handed out half-built.
    ///
    /// # Errors
    ///
    /// Returns the first protocol error recorded while observing (an orphan delta or a duplicate
    /// start), or [`girder_core::AssemblyError::Unclosed`] when the stream ended without `Finish`
    /// while a block was still open.
    pub fn finish(mut self) -> Result<Vec<ContentBlock>, AssemblyError> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        // Without a terminal `Finish`, an open block is the truncated-stream case and must be
        // reported. With one, the same block is a provider that simply does not close it; see the
        // module docs.
        if !self.terminal_finish && !self.open.is_empty() {
            return Err(AssemblyError::Unclosed {
                count: self.open.len(),
            });
        }
        let open = std::mem::take(&mut self.open);
        for index in open {
            self.assembler.push(StreamChunk::BlockEnd {
                index,
                block: closing_placeholder(),
            })?;
        }
        self.assembler.finish()
    }

    /// How many blocks have been handed out so far.
    ///
    /// Counts what a caller has already received, not what the assembler holds; it is the number a
    /// progress indicator shows while a response streams.
    #[must_use]
    pub fn completed(&self) -> usize {
        self.completed
    }

    /// Pushes one chunk and records what it means for the observer's bookkeeping.
    ///
    /// The chunk is pushed first and tracked only if the push succeeded: a rejected `block-start`
    /// (a duplicate) must not add its index to `open`, or the end of the stream would try to close
    /// an index that is already closed.
    fn feed(&mut self, chunk: &StreamChunk) {
        if self.error.is_some() {
            return;
        }
        if let Err(error) = self.assembler.push(chunk.clone()) {
            self.error = Some(error);
            return;
        }
        match chunk {
            StreamChunk::BlockStart { index, .. } => {
                self.open.insert(*index);
            }
            // A `block-end` for an index that never started is accepted by the assembler as a
            // complete block, so removing it here is a no-op rather than an error.
            StreamChunk::BlockEnd { index, .. } => {
                self.open.remove(index);
            }
            StreamChunk::Finish { .. } => self.terminal_finish = true,
            // Usage and deltas carry no block lifecycle of their own.
            StreamChunk::TextDelta { .. }
            | StreamChunk::ReasoningDelta { .. }
            | StreamChunk::ToolCallDelta { .. }
            | StreamChunk::Usage(_) => {}
        }
    }

    /// Takes the blocks that have closed and counts them.
    fn drain(&mut self) -> Vec<ContentBlock> {
        let finished = self.assembler.take_finished();
        self.completed += finished.len();
        finished
    }
}

/// Assembles a whole stream into blocks, using the incremental assembler instead of a final fold.
///
/// This is the drop-in replacement for the block-producing half of
/// `AssistantMessage::from_chunks`: on any stream that `from_chunks` accepts, the returned blocks
/// are the same, in the same index order. The module docs list the inputs where the two disagree and
/// why this module's answer is the one to prefer.
///
/// Nothing is drained until the end, so blocks come back in index order even though the assembler
/// delivers a block as soon as it closes — a stream free to interleave two blocks may close the
/// higher index first.
///
/// # Errors
///
/// Returns [`girder_core::AssemblyError::OrphanDelta`] for a delta whose block never started,
/// [`girder_core::AssemblyError::DuplicateStart`] for a second start of one index, and
/// [`girder_core::AssemblyError::Unclosed`] when the stream ended without `Finish` while a block was
/// still open.
pub fn assemble_message(chunks: &[StreamChunk]) -> Result<Vec<ContentBlock>, AssemblyError> {
    let mut observer = StreamObserver::new();
    for chunk in chunks {
        // `feed` rather than `observe`: observing would drain each block as it closes, and the
        // result of a drained stream is in *closing* order, which is not the index order
        // `from_chunks` produces.
        observer.feed(chunk);
    }
    observer.finish()
}
