//! Cheap statistics over one streamed response, for "is this model stuck?" questions.
//!
//! The answer is not the same as "did the request fail". A model that emits `block-start` and then
//! goes quiet, and a model that produced an entire answer, look identical to a caller that only
//! checks whether the stream ended — both streams are simply old. The statistics here describe the
//! *shape* of that silence: how long until the first chunk, how long until the first visible text,
//! and whether there was any visible text at all. That difference is the point of the module.
//!
//! # What counts as visible text
//!
//! Only [`StreamChunk::TextDelta`]. Reasoning deltas are excluded deliberately: they are written for
//! the model, not the reader, so a stream that produced megabytes of reasoning and no answer is
//! still "no output" from the user's point of view — the case this module exists to surface. Tool
//! call deltas are excluded for the same reason: arguments are protocol, not content.
//!
//! [`StreamChunk::BlockEnd`] is excluded too, even when it carries a text block. A streamed
//! response already delivered that text as deltas, and counting the assembled block as well would
//! double [`Stats::visible_chars`] for every well-behaved provider.
//!
//! An empty delta is not a token: providers routinely emit `""` for a block boundary, and treating
//! it as the first visible text would report output that no reader can see.
//!
//! Timestamps are supplied by the caller — the stream itself carries none — so `first_chunk_ms` and
//! `first_text_ms` are on whatever monotonic clock the caller used, assumed non-decreasing along
//! the slice.

use girder_core::StreamChunk;

/// One streamed response, reduced to the few numbers a stall diagnosis needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Time of the first chunk, in milliseconds since the caller's start point.
    ///
    /// `None` only for an empty stream. A response that sent a lone `block-start` has a value here
    /// but no value in `first_text_ms`, which is exactly how a stalled response is told apart from
    /// one that produced nothing at all.
    pub first_chunk_ms: Option<u64>,
    /// Time of the first visible text delta, in milliseconds since the caller's start point.
    ///
    /// Also `None` when the stream carried only reasoning, tool calls, or block/usage/finish
    /// markers.
    pub first_text_ms: Option<u64>,
    /// Whether any visible text arrived.
    pub has_visible_text: bool,
    /// Number of characters of visible text, counted as Unicode scalar values rather than bytes so
    /// the number matches what a reader sees.
    pub visible_chars: usize,
    /// Whether any reasoning content arrived.
    ///
    /// Reported separately from `has_visible_text` because it distinguishes the two ways a stream
    /// can be busy without answering: the model is thinking, or the model produced nothing at all.
    pub has_reasoning: bool,
    /// Number of chunks received.
    pub chunks: usize,
}

impl Stats {
    /// Computes the statistics for one timed chunk stream.
    ///
    /// The whole slice is a parameter, rather than a streaming accumulator, because these numbers
    /// are read after the fact: a response is already over when a caller asks whether it stalled,
    /// and recomputing from the retained chunks keeps the accumulator path free of bookkeeping.
    #[must_use]
    pub fn from_chunks(chunks: &[(u64, StreamChunk)]) -> Self {
        let mut stats = Self::default();

        for (time, chunk) in chunks {
            stats.chunks += 1;
            // The first chunk of any kind, including `block-start` and `usage`: this is the moment
            // the transport proved it was alive, which is what "the model accepted our request"
            // means in practice.
            if stats.first_chunk_ms.is_none() {
                stats.first_chunk_ms = Some(*time);
            }

            match chunk {
                StreamChunk::TextDelta { text, .. } if !text.is_empty() => {
                    if stats.first_text_ms.is_none() {
                        stats.first_text_ms = Some(*time);
                    }
                    stats.visible_chars += text.chars().count();
                }
                StreamChunk::ReasoningDelta { text, .. } if !text.is_empty() => {
                    stats.has_reasoning = true;
                }
                _ => {}
            }
        }

        // Derived from the character count instead of being set per chunk so the two fields cannot
        // disagree: there is no state where text was counted but not considered visible.
        stats.has_visible_text = stats.visible_chars > 0;
        stats
    }
}
