//! One model attempt: its live framing, its durable compact stream, and the fold that replaces it.
//!
//! Upstream path: `packages/core/agent-loop/src/assistant-stream.ts` (140 lines). Two neighbours
//! complete the picture and are ported here because the first is meaningless without them:
//!
//! - `packages/llm/llm/src/assistant-stream.ts` (534 lines) — `AssistantStreamAccumulator`, the
//!   compact record format, and the record-level readers;
//! - `packages/api/session-controller/src/assistant-stream.ts` (102 lines) —
//!   `SessionAssistantStreamAccumulator`, the revision fold a reconnecting follower rebuilds from.
//!
//! # What is already here, and what is not
//!
//! [`crate::BlockAssembler`] assembles chunks into blocks, and [`crate::StreamObserver`] drives it
//! one chunk at a time. Both answer "what does this stream *mean*". Neither answers the three
//! questions an attempt raises:
//!
//! | question | upstream owner | local status before this module |
//! |---|---|---|
//! | who is streaming, and how does a live viewer know frame *N* is missing? | `AssistantStreamAttempt` | absent — nothing in this crate produces a frame, a revision, or an attempt id |
//! | what is the compact shape a durable `assistant/attempt` event stores? | `AssistantStreamAccumulator` | absent — `session_migration` *recognizes* `text-chunks`/`time0`/`dt` rows when deciding a log's format, but nothing constructs or decodes them |
//! | a follower reconnects mid-answer: which attempt is it looking at, and is it complete? | `SessionAssistantStreamAccumulator` | absent |
//!
//! The absence is not cosmetic. [`crate::session_migration`] declares `assistant/attempt` a v2-only
//! event, recognizes a packed row when it decides a log's format, and namespaces the block tags
//! inside an embedded `stream` — but only for entries it already understands as raw `chunk` rows,
//! and it never decodes a run. So the format is *referenced* in three places and produced and read
//! in none. [`Accumulator`] is the writer, [`expand`] the validating reader, and [`Attempt`] the
//! framing that ties a live stream to the durable event.
//!
//! # The three layers, and why they are one module
//!
//! 1. **[`Accumulator`]** turns `(time, chunk)` pairs into [`StreamRecord`]s. Adjacent deltas of the
//!    same kind, index, and (for tool calls) call identity are packed into one run with a
//!    *timeline* attached: `time0` and an inter-member gap list `dt`. The packing is lossless —
//!    [`expand`] reconstructs the original sequence exactly, deltas and timestamps both — which is
//!    what lets one durable record stand in for thousands of chunks while still replaying them.
//! 2. **[`Attempt`]** folds one attempt into one such stream plus ordered frames. A frame is the
//!    live view: [`Frame::Start`], a dense [`Frame::Chunk`] sequence, and exactly one
//!    [`Frame::End`] carrying either the durable settlement or an abandonment. Frames carry a
//!    revision allocated by [`Revision`], monotone across an *agent lifecycle* rather than an
//!    attempt, which is what lets a consumer detect a dropped frame instead of silently rendering a
//!    stream with a hole in it.
//! 3. **[`StreamFold`]** is the consumer side of that contract: it accepts frames, rejects a
//!    lifecycle whose revisions gap, drops an attempt whose id or index does not continue the one
//!    it held, and materializes the [`Baseline`] a reconnecting follower resumes from.
//!
//! They are one module because each is the other's only producer or consumer. Splitting them would
//! put the frame vocabulary on one side of a module boundary and its validation on the other, which
//! is exactly the seam where the two would drift.
//!
//! # Why the compact format is not just the chunk list
//!
//! An `assistant/message` event can carry tens of thousands of deltas — one per token. Storing them
//! as `{"type":"chunk","time":…,"chunk":{…}}` rows makes the log's size scale with the *token*
//! count of everything ever said, which is the dominant term in a long session. A run collapses
//! `n` deltas into one record of `n` strings plus `n-1` small integers, and the integers are what
//! keep the timeline: a UI that replays a response at its original cadence needs each fragment's
//! arrival time, not just its order.
//!
//! # Where this port deliberately differs
//!
//! - **Owned values instead of structural copies.** Upstream's snapshot returns a deep-frozen copy
//!   and its identity is tested with `toBe`. [`Accumulator::snapshot`] returns an owned `Vec` for the
//!   same guarantee without a freeze pass, and [`StreamFold::snapshot`] borrows a cached
//!   [`Baseline`] so a polling follower does not rebuild one per call.
//! - **An empty call id is unrepresentable.** Upstream forces an unpacked raw record for a
//!   tool-call delta whose `id` or `name` is empty, because adapters have been seen to send those.
//!   [`crate::ToolCallId`] is a branded [`crate::Id`] that refuses to be empty, so the empty-id half
//!   of that rule cannot arise; the empty-*name* half can and is handled.
//! - **Unsafe integers are rejected at the fields that can hold one.** Upstream's `snapshotChunk`
//!   re-serializes every chunk and refuses anything JSON cannot round-trip. This crate's
//!   [`StreamChunk`] is a closed enum of typed fields and the only field that can exceed a
//!   JavaScript safe integer is a [`TokenUsage`] counter, so that is what [`Accumulator::push`]
//!   checks.
//! - **Whitespace means Unicode `White_Space`, not JavaScript `\s`.** `\s` additionally matches
//!   U+FEFF, so a fragment consisting only of a BOM counts as visible here and as invisible
//!   upstream. A BOM-only fragment is not something a provider emits, and the local spelling is the
//!   one a reader would guess; the difference is recorded rather than silently inherited.
//! - **`replayState` is absent.** Upstream's `finish` chunk carries a `ReplayEnvelope`, which the
//!   attempt exposes after pruning per-block entries alongside the dropped tool calls. This crate's
//!   [`StreamChunk::Finish`] carries only a [`FinishReason`], and there is no `ReplayEnvelope` type
//!   anywhere in it, so there is nothing to expose. That is a gap in the LLM layer, not in this
//!   module: the accessor would be one line the day the chunk grows the field.
//! - **A tool-call name repeats when an expansion is fed back into the local assembler.** Upstream's
//!   `BlockAssembler` assigns `toolCallName = chunk.name`, so expanding a run — which repeats the
//!   run's single name on every member — lands on the right name. [`crate::BlockAssembler`] appends
//!   instead ("a provider may stream it in pieces"), so the same path produces `"readread"`.
//!   [`assemble_stream`] is unaffected because it emits one delta per run, and that is the one to
//!   use; this is recorded because a caller who reaches for [`expand`] plus a bare assembler would
//!   otherwise meet it as a mystery. Fixing it belongs in the assembler's name rule, not here.
//! - **`blocks()` and `interrupted_blocks()` are terminal reads.** Upstream's assembler keeps a
//!   partial per index and can be read repeatedly. [`crate::BlockAssembler::finish`] consumes the
//!   assembler, and this module closes the open blocks in order to read them, so both accessors take
//!   `&mut self`, may be called once, and take the other one with them. [`Attempt::stream`] is owned
//!   and therefore still readable afterwards, so a caller may read the stream before or after the
//!   blocks, indifferently.
//!
//! # Strictness inherited from the local assembler
//!
//! [`crate::BlockAssembler`] reports an orphan delta and a duplicate `block-start`, where upstream's
//! `BlockAssembler` silently tolerates both (see [`crate::stream_blocks`] for the full list of
//! divergences). [`Attempt::push`] latches the first such error the way [`crate::StreamObserver`]
//! does: the chunk still enters the durable stream — the log is evidence, not judgement — but
//! [`Attempt::blocks`] refuses to hand back a message built from a stream it knows was malformed.
//! [`Attempt::protocol_error`] exposes the latch to a caller that wants to log it immediately.
//!
//! This file is also compiled by `tests/llm_attempt_gap.rs` through `#[path]`, so every import
//! below goes through the crate root's public re-exports: in that build `crate::` names the test
//! crate, and a `crate::…` path would fail to resolve.

// The crate root declares this module but re-exports nothing from it yet, so in a library build no
// item here has a caller and `dead_code` would report the whole surface as waste. The allow says
// that plainly; the test target exercises every item below.
#![allow(dead_code)]

use std::fmt;

use girder_core::{
    AssemblyError, BlockAssembler, ContentBlock, FinishReason, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER,
    SessionId, StreamChunk, TokenUsage, ToolCallId,
};

/// The largest time the compact format can carry, as the signed integer it is stored as.
///
/// [`MAX_SAFE_INTEGER`] is unsigned at the crate root because it is used there for counts; a
/// timestamp is signed, so the same bound is taken once into the signed domain and reused.
fn max_safe_time() -> i64 {
    i64::try_from(MAX_SAFE_INTEGER).unwrap_or(i64::MAX)
}

/// Whether `value` survives a round trip through a JavaScript `number`.
fn is_safe_time(value: i64) -> bool {
    (MIN_SAFE_INTEGER..=max_safe_time()).contains(&value)
}

/// Whether `index` is small enough to survive the same round trip.
///
/// A block index is a `usize`, so it is never negative; the only way it can be unrepresentable is by
/// being too large, which no real stream does and which the format must still refuse rather than
/// truncate.
fn index_is_safe(index: usize) -> bool {
    u64::try_from(index).is_ok_and(|value| value <= MAX_SAFE_INTEGER)
}

/// A timestamp paired with the chunk that arrived at it.
///
/// The pair is what the accumulator stores and what [`expand`] returns; a bare chunk loses the only
/// information a replay of a live response is missing.
#[derive(Clone, Debug, PartialEq)]
pub struct TimedChunk {
    /// Arrival time, as the caller's clock reported it.
    pub time: i64,
    /// The chunk itself.
    pub chunk: StreamChunk,
}

/// Why a chunk, a record, or an expanded stream was refused.
///
/// One enum rather than a string: a caller that consumes a durable stream at start-up has to
/// distinguish "this log was written by a newer harness" from "this log is corrupt", and the
/// variants carry the numbers a report needs.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// A chunk's timestamp is not a JavaScript safe integer.
    #[error("assistant stream time must be a safe integer, got {time}")]
    UnsafeTime {
        /// The offending timestamp.
        time: i64,
    },

    /// A chunk's block index is too large to survive the same round trip.
    #[error("block index {index} is not a JavaScript safe integer")]
    UnsafeIndex {
        /// The offending index.
        index: usize,
    },

    /// A usage count exceeds what JavaScript can represent exactly.
    #[error("token usage `{field}` is {value}, above the JavaScript safe-integer limit")]
    UnsafeUsage {
        /// Which counter.
        field: &'static str,
        /// Its value.
        value: u64,
    },

    /// A packed run carries no members, so it stands for no chunk at all.
    #[error("`{label}` run must carry at least one member")]
    EmptyRun {
        /// The record tag.
        label: &'static str,
    },

    /// A run's gap list does not have exactly one entry fewer than its members.
    #[error("`{label}` run has {members} members but {gaps} gaps")]
    GapCountMismatch {
        /// The record tag.
        label: &'static str,
        /// Number of members.
        members: usize,
        /// Number of gaps.
        gaps: usize,
    },

    /// Replaying a run's gaps left the safe-integer range.
    #[error("`{label}` member time {time} is outside the JavaScript safe-integer range")]
    RunTimeUnsafe {
        /// The record tag.
        label: &'static str,
        /// The timestamp that overflowed.
        time: i64,
    },

    /// A tool-call run's recorded name is empty, which upstream never packs.
    #[error("`tool-call-chunks` name must not be empty")]
    EmptyToolCallName,
}

/// One durable stream record.
///
/// Three variants pack delta runs; the fourth is the escape hatch for every chunk that is not a
/// delta. A packed run is only ever produced when the packing loses nothing: same kind, same index,
/// and — for tool calls — same call identity and same name *presence*.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamRecord {
    /// A run of visible-text deltas for one block index.
    TextChunks {
        /// Time of the first member.
        time0: i64,
        /// Block index every member belongs to.
        index: usize,
        /// Gaps between consecutive members; one fewer entry than `texts`.
        dt: Vec<i64>,
        /// The fragment texts, in order.
        texts: Vec<String>,
    },

    /// A run of reasoning deltas for one block index.
    ReasoningChunks {
        /// Time of the first member.
        time0: i64,
        /// Block index every member belongs to.
        index: usize,
        /// Gaps between consecutive members; one fewer entry than `texts`.
        dt: Vec<i64>,
        /// The fragment texts, in order.
        texts: Vec<String>,
    },

    /// A run of tool-call argument deltas for one call.
    ToolCallChunks {
        /// Time of the first member.
        time0: i64,
        /// Block index every member belongs to.
        index: usize,
        /// Gaps between consecutive members; one fewer entry than `args`.
        dt: Vec<i64>,
        /// The call id shared by every member.
        id: ToolCallId,
        /// The name, when the deltas carried one.
        name: Option<String>,
        /// The argument fragments, in order.
        args: Vec<String>,
    },

    /// A chunk stored verbatim: every non-delta chunk, and the delta shapes that cannot be packed.
    Chunk {
        /// Arrival time.
        time: i64,
        /// The chunk.
        chunk: StreamChunk,
    },
}

impl StreamRecord {
    /// The record's wire tag, spelled as upstream spells it.
    ///
    /// A reader that dispatches on the tag and a writer that emits it must agree on one spelling,
    /// and the durable log is the place where a disagreement is discovered too late.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::TextChunks { .. } => "text-chunks",
            Self::ReasoningChunks { .. } => "reasoning-chunks",
            Self::ToolCallChunks { .. } => "tool-call-chunks",
            Self::Chunk { .. } => "chunk",
        }
    }

    /// The block index a packed run belongs to, or `None` for a raw record.
    #[must_use]
    pub fn index(&self) -> Option<usize> {
        match self {
            Self::TextChunks { index, .. }
            | Self::ReasoningChunks { index, .. }
            | Self::ToolCallChunks { index, .. } => Some(*index),
            Self::Chunk { .. } => None,
        }
    }
}

/// One packed run: every record except a raw [`StreamRecord::Chunk`].
///
/// A separate type because the readers below answer questions about runs that make no sense for a
/// raw record — "when did this run's first token arrive" is answered from `time0` and `dt`, which a
/// raw chunk does not have.
#[derive(Clone, Copy, Debug)]
pub enum StreamRun<'a> {
    /// A visible-text run.
    Text {
        /// Time of the first member.
        time0: i64,
        /// Gaps between consecutive members.
        dt: &'a [i64],
        /// The fragment texts.
        texts: &'a [String],
    },

    /// A reasoning run.
    Reasoning {
        /// Time of the first member.
        time0: i64,
        /// Gaps between consecutive members.
        dt: &'a [i64],
        /// The fragment texts.
        texts: &'a [String],
    },

    /// A tool-call run.
    ToolCall {
        /// Time of the first member.
        time0: i64,
        /// Gaps between consecutive members.
        dt: &'a [i64],
        /// The name, when the deltas carried one.
        name: Option<&'a str>,
        /// The argument fragments.
        args: &'a [String],
    },
}

impl<'a> StreamRun<'a> {
    /// Borrows a record as a run, or `None` when it is a raw chunk.
    #[must_use]
    pub fn of(record: &'a StreamRecord) -> Option<Self> {
        match record {
            StreamRecord::TextChunks {
                time0, dt, texts, ..
            } => Some(Self::Text {
                time0: *time0,
                dt,
                texts,
            }),
            StreamRecord::ReasoningChunks {
                time0, dt, texts, ..
            } => Some(Self::Reasoning {
                time0: *time0,
                dt,
                texts,
            }),
            StreamRecord::ToolCallChunks {
                time0,
                dt,
                name,
                args,
                ..
            } => Some(Self::ToolCall {
                time0: *time0,
                dt,
                name: name.as_deref(),
                args,
            }),
            StreamRecord::Chunk { .. } => None,
        }
    }

    /// The run's members, whichever field carries them.
    fn members(&self) -> &'a [String] {
        match self {
            Self::Text { texts, .. } | Self::Reasoning { texts, .. } => texts,
            Self::ToolCall { args, .. } => args,
        }
    }

    /// Time of the first member that satisfies `predicate`.
    ///
    /// Stops scanning at that member: this is "when did the first token arrive", not "how many
    /// tokens qualify", and a caller measuring latency does not want to walk a whole response.
    fn first_member_time(&self, predicate: impl Fn(&str) -> bool) -> Option<i64> {
        let mut time = self.origin();
        for (position, member) in self.members().iter().enumerate() {
            if position > 0 {
                time = time.checked_add(*self.gaps().get(position - 1)?)?;
            }
            if predicate(member) {
                return Some(time);
            }
        }
        None
    }

    /// Time of the first member.
    fn origin(&self) -> i64 {
        match self {
            Self::Text { time0, .. }
            | Self::Reasoning { time0, .. }
            | Self::ToolCall { time0, .. } => *time0,
        }
    }

    /// The run's gap list.
    fn gaps(&self) -> &'a [i64] {
        match self {
            Self::Text { dt, .. } | Self::Reasoning { dt, .. } | Self::ToolCall { dt, .. } => dt,
        }
    }
}

/// Time of the first token in one packed run.
///
/// A name-bearing tool-call run starts at its first member — the name *is* output — while every
/// other run starts at its first non-empty fragment. An empty fragment is a keep-alive, not a token,
/// and charging one to latency would report a prompt as answered before any content existed.
#[must_use]
pub fn run_first_token_time(run: StreamRun<'_>) -> Option<i64> {
    if let StreamRun::ToolCall {
        time0,
        name: Some(_),
        ..
    } = run
    {
        return Some(time0);
    }
    run.first_member_time(|fragment| !fragment.is_empty())
}

/// Time of the first reader-visible fragment in one packed run.
///
/// A tool-call run has none: a call is protocol, not transcript. Everything else qualifies on its
/// first fragment with non-whitespace content.
#[must_use]
pub fn run_first_visible_time(run: StreamRun<'_>) -> Option<i64> {
    match run {
        StreamRun::ToolCall { .. } => None,
        _ => run.first_member_time(has_non_whitespace),
    }
}

/// Whether `text` holds anything a reader would see.
///
/// JavaScript's `\s` and Unicode `White_Space` differ on U+FEFF; see the module docs.
fn has_non_whitespace(text: &str) -> bool {
    text.chars().any(|character| !character.is_whitespace())
}

/// Whether one chunk carries the model's first output token.
///
/// This is the latency boundary: the clock stops at the first fragment that is output rather than
/// framing. A name-bearing tool-call delta counts even with empty arguments, because announcing the
/// call is the model producing output; a block boundary, usage record, or finish never counts.
#[must_use]
pub fn is_token_delta(chunk: &StreamChunk) -> bool {
    match chunk {
        StreamChunk::TextDelta { text, .. } | StreamChunk::ReasoningDelta { text, .. } => {
            !text.is_empty()
        }
        StreamChunk::ToolCallDelta {
            name,
            arguments_delta,
            ..
        } => !arguments_delta.is_empty() || name.is_some(),
        StreamChunk::BlockStart { .. }
        | StreamChunk::BlockEnd { .. }
        | StreamChunk::Usage(_)
        | StreamChunk::Finish { .. } => false,
    }
}

/// Whether one chunk contributes reader-visible transcript content.
///
/// Text and reasoning count only with non-whitespace content, streamed as a delta or delivered as a
/// completed block. A block of any other kind counts at its boundaries — except a tool call, which
/// is protocol rather than content. Usage and finish never count.
#[must_use]
pub fn is_visible_chunk(chunk: &StreamChunk) -> bool {
    match chunk {
        StreamChunk::TextDelta { text, .. } | StreamChunk::ReasoningDelta { text, .. } => {
            has_non_whitespace(text)
        }
        StreamChunk::BlockStart { block_type, .. } => {
            !matches!(block_type.as_str(), "text" | "reasoning" | "tool-call")
        }
        StreamChunk::BlockEnd { block, .. } => block_is_visible(block),
        StreamChunk::ToolCallDelta { .. } | StreamChunk::Usage(_) | StreamChunk::Finish { .. } => {
            false
        }
    }
}

/// Whether a completed block is something a transcript reader would see.
fn block_is_visible(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::ToolCall { .. } => false,
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => has_non_whitespace(text),
        ContentBlock::Image { .. } | ContentBlock::File { .. } => true,
    }
}

/// Whether one chunk carries non-whitespace *text* — reasoning, tool calls, and every other block
/// kind excluded.
#[must_use]
pub fn chunk_has_visible_text(chunk: &StreamChunk) -> bool {
    match chunk {
        StreamChunk::TextDelta { text, .. } => has_non_whitespace(text),
        StreamChunk::BlockEnd { block, .. } => matches!(
            block,
            ContentBlock::Text { text } if has_non_whitespace(text)
        ),
        _ => false,
    }
}

/// Time of the first token anywhere in `stream`.
#[must_use]
pub fn first_token_time(stream: &[StreamRecord]) -> Option<i64> {
    for record in stream {
        let time = match record {
            StreamRecord::Chunk { time, chunk } => {
                if is_token_delta(chunk) {
                    Some(*time)
                } else {
                    None
                }
            }
            _ => StreamRun::of(record).and_then(run_first_token_time),
        };
        if time.is_some() {
            return time;
        }
    }
    None
}

/// Whether `stream` carries anything a transcript reader would see.
#[must_use]
pub fn has_visible_content(stream: &[StreamRecord]) -> bool {
    stream.iter().any(|record| match record {
        StreamRecord::Chunk { chunk, .. } => is_visible_chunk(chunk),
        _ => StreamRun::of(record)
            .and_then(run_first_visible_time)
            .is_some(),
    })
}

/// Whether `stream` carries non-whitespace visible text.
#[must_use]
pub fn has_visible_text(stream: &[StreamRecord]) -> bool {
    stream.iter().any(|record| match record {
        StreamRecord::TextChunks { texts, .. } => texts.iter().any(|text| has_non_whitespace(text)),
        StreamRecord::Chunk { chunk, .. } => chunk_has_visible_text(chunk),
        StreamRecord::ReasoningChunks { .. } | StreamRecord::ToolCallChunks { .. } => false,
    })
}

/// Every chunk stored verbatim, in stream order.
///
/// Runs are not expanded: this is the "which raw chunks did this attempt keep" question, and a run
/// is not one of them.
pub fn raw_chunks(stream: &[StreamRecord]) -> impl Iterator<Item = &StreamChunk> {
    stream.iter().filter_map(|record| match record {
        StreamRecord::Chunk { chunk, .. } => Some(chunk),
        _ => None,
    })
}

/// The stream's terminal `finish` chunk, scanning backwards.
///
/// Backwards because a stream has at most one and it is at the end; a forward scan would walk the
/// whole response to reach it.
#[must_use]
pub fn last_finish(stream: &[StreamRecord]) -> Option<&FinishReason> {
    stream.iter().rev().find_map(|record| match record {
        StreamRecord::Chunk {
            chunk: StreamChunk::Finish { reason },
            ..
        } => Some(reason),
        _ => None,
    })
}

/// Every streamed visible-text fragment, joined in stream order.
///
/// Reasoning and tool-call fragments are excluded: this answers "what text did the model say", and a
/// reasoning trace is not what it said.
#[must_use]
pub fn join_text(stream: &[StreamRecord]) -> String {
    let mut joined = String::new();
    for record in stream {
        match record {
            StreamRecord::TextChunks { texts, .. } => {
                for text in texts {
                    joined.push_str(text);
                }
            }
            StreamRecord::Chunk {
                chunk: StreamChunk::TextDelta { text, .. },
                ..
            } => joined.push_str(text),
            _ => {}
        }
    }
    joined
}

/// Expands compact records into the exact timed chunk sequence they were built from.
///
/// This is the validating path, and the one to run on records read at a durable boundary:
/// [`assemble_stream`] trusts its input, because a live attempt produced it.
///
/// Every delta boundary and every timestamp is reproduced, which is what makes a packed run a
/// *compression* rather than a rewrite.
///
/// # Errors
///
/// Returns [`StreamError::EmptyRun`] for a run with no members, [`StreamError::GapCountMismatch`]
/// when the gap list and the member list disagree, [`StreamError::RunTimeUnsafe`] when replaying the
/// gaps leaves the safe-integer range, and [`StreamError::EmptyToolCallName`] for a packed call whose
/// recorded name is empty.
pub fn expand(stream: &[StreamRecord]) -> Result<Vec<TimedChunk>, StreamError> {
    let mut chunks = Vec::new();
    for record in stream {
        match record {
            StreamRecord::Chunk { time, chunk } => chunks.push(TimedChunk {
                time: *time,
                chunk: chunk.clone(),
            }),
            StreamRecord::TextChunks {
                time0,
                index,
                dt,
                texts,
            } => expand_run(
                &mut chunks,
                "text-chunks",
                *time0,
                *index,
                dt,
                texts,
                |text| StreamChunk::TextDelta {
                    index: *index,
                    text: text.to_owned(),
                },
            )?,
            StreamRecord::ReasoningChunks {
                time0,
                index,
                dt,
                texts,
            } => expand_run(
                &mut chunks,
                "reasoning-chunks",
                *time0,
                *index,
                dt,
                texts,
                |text| StreamChunk::ReasoningDelta {
                    index: *index,
                    text: text.to_owned(),
                },
            )?,
            StreamRecord::ToolCallChunks {
                time0,
                index,
                dt,
                id,
                name,
                args,
            } => {
                if name.as_deref() == Some("") {
                    return Err(StreamError::EmptyToolCallName);
                }
                let id = id.clone();
                let name = name.clone();
                expand_run(
                    &mut chunks,
                    "tool-call-chunks",
                    *time0,
                    *index,
                    dt,
                    args,
                    |arguments| StreamChunk::ToolCallDelta {
                        index: *index,
                        id: id.clone(),
                        name: name.clone(),
                        arguments_delta: arguments.to_owned(),
                    },
                )?;
            }
        }
    }
    Ok(chunks)
}

/// Expands one run, validating its shape and timeline.
fn expand_run(
    chunks: &mut Vec<TimedChunk>,
    label: &'static str,
    time0: i64,
    index: usize,
    dt: &[i64],
    members: &[String],
    build: impl Fn(&str) -> StreamChunk,
) -> Result<(), StreamError> {
    if members.is_empty() {
        return Err(StreamError::EmptyRun { label });
    }
    if dt.len() + 1 != members.len() {
        return Err(StreamError::GapCountMismatch {
            label,
            members: members.len(),
            gaps: dt.len(),
        });
    }
    if !index_is_safe(index) {
        return Err(StreamError::UnsafeIndex { index });
    }
    if !is_safe_time(time0) {
        return Err(StreamError::RunTimeUnsafe { label, time: time0 });
    }
    let mut time = time0;
    for (position, member) in members.iter().enumerate() {
        if position > 0 {
            let gap = dt.get(position - 1).copied().unwrap_or_default();
            let next = time
                .checked_add(gap)
                .ok_or(StreamError::RunTimeUnsafe { label, time })?;
            time = next;
        }
        if !is_safe_time(time) {
            return Err(StreamError::RunTimeUnsafe { label, time });
        }
        chunks.push(TimedChunk {
            time,
            chunk: build(member),
        });
    }
    Ok(())
}

/// Assembles one compact stream into blocks.
///
/// Each run contributes one delta carrying its joined fragments, which assembles the same blocks as
/// the original per-member deltas because assembly only concatenates them. Raw chunks are pushed as
/// recorded. The records are trusted, not validated; run [`expand`] first when they came from a
/// durable boundary.
///
/// The reconstructed sequence goes through [`crate::assemble_message`] rather than through a bare
/// [`BlockAssembler`], because the two differ on exactly the case a compact stream hits: a block the
/// provider never closed. The observer closes such a block from its accumulated deltas once the
/// stream has announced `Finish`, and reports truncation when it has not. Reimplementing that choice
/// here would put a second copy of it where only one should exist.
///
/// # Errors
///
/// Returns whatever the assembler reports for the reconstructed chunk sequence — an orphan delta, a
/// duplicate `block-start`, or an unclosed block on a stream that never announced `Finish`.
pub fn assemble_stream(stream: &[StreamRecord]) -> Result<Vec<ContentBlock>, AssemblyError> {
    let chunks = stream
        .iter()
        .map(|record| match record {
            StreamRecord::Chunk { chunk, .. } => chunk.clone(),
            StreamRecord::TextChunks { index, texts, .. } => StreamChunk::TextDelta {
                index: *index,
                text: texts.concat(),
            },
            StreamRecord::ReasoningChunks { index, texts, .. } => StreamChunk::ReasoningDelta {
                index: *index,
                text: texts.concat(),
            },
            StreamRecord::ToolCallChunks {
                index,
                id,
                name,
                args,
                ..
            } => StreamChunk::ToolCallDelta {
                index: *index,
                id: id.clone(),
                name: name.clone(),
                arguments_delta: args.concat(),
            },
        })
        .collect::<Vec<_>>();
    girder_core::assemble_message(&chunks)
}

/// Incremental compaction of one attempt, without retaining a second raw-chunk list.
///
/// Feed it `(time, chunk)` pairs with [`push`](Self::push); read the durable shape with
/// [`snapshot`](Self::snapshot). The packing rule is upstream's, and it is deliberately timid: a run
/// continues only when the new delta agrees with the previous one on *everything* the record
/// semantically fixes — kind, block index, and, for a tool call, id and name — and when the timeline
/// can represent the gap exactly. Anything else starts a new record, so a reader never has to
/// reconstruct which member belonged to which call.
#[derive(Debug, Default)]
pub struct Accumulator {
    /// Records built so far, in stream order.
    records: Vec<StreamRecord>,
}

impl Accumulator {
    /// Creates an empty accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one timed chunk, returning a detached copy for assembly and live publication.
    ///
    /// The copy is what the caller publishes; the accumulator keeps its own. That split is what
    /// makes a snapshot immutable without a freeze pass: nothing outside ever holds a reference into
    /// the record list.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::UnsafeTime`] for a timestamp outside the JavaScript safe-integer
    /// range, [`StreamError::UnsafeIndex`] for an index above it, and [`StreamError::UnsafeUsage`]
    /// for a token count above it. All three would be *changed* by a JSON round trip, which is the
    /// one thing a durable record may not do.
    pub fn push(&mut self, time: i64, chunk: StreamChunk) -> Result<TimedChunk, StreamError> {
        if !is_safe_time(time) {
            return Err(StreamError::UnsafeTime { time });
        }
        match &chunk {
            StreamChunk::TextDelta { index, .. }
            | StreamChunk::ReasoningDelta { index, .. }
            | StreamChunk::ToolCallDelta { index, .. }
            | StreamChunk::BlockStart { index, .. }
            | StreamChunk::BlockEnd { index, .. } => {
                if !index_is_safe(*index) {
                    return Err(StreamError::UnsafeIndex { index: *index });
                }
            }
            StreamChunk::Usage(usage) => check_usage(usage)?,
            StreamChunk::Finish { .. } => {}
        }

        // Kept for the return value; the packing below consumes the chunk it was given.
        let detached = chunk.clone();
        match chunk {
            StreamChunk::TextDelta { index, text } => self.pack_text(index, time, &text),
            StreamChunk::ReasoningDelta { index, text } => self.pack_reasoning(index, time, &text),
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                // An empty name is a shape adapters have been seen to send, and packing it would let
                // a later run carrying a real name continue the same record while `expand` repeated
                // the empty name on every member. Upstream stores such a delta raw; so does this.
                if name.as_deref() == Some("") {
                    self.records.push(StreamRecord::Chunk {
                        time,
                        chunk: StreamChunk::ToolCallDelta {
                            index,
                            id,
                            name,
                            arguments_delta,
                        },
                    });
                } else {
                    self.pack_tool_call(index, time, id, name, &arguments_delta);
                }
            }
            other @ (StreamChunk::BlockStart { .. }
            | StreamChunk::BlockEnd { .. }
            | StreamChunk::Usage(_)
            | StreamChunk::Finish { .. }) => {
                self.records
                    .push(StreamRecord::Chunk { time, chunk: other });
            }
        }

        Ok(TimedChunk {
            time,
            chunk: detached,
        })
    }

    /// Appends a visible-text fragment, continuing the previous run when it is compatible.
    fn pack_text(&mut self, index: usize, time: i64, text: &str) {
        if let Some(StreamRecord::TextChunks {
            index: previous_index,
            dt,
            texts,
            time0,
        }) = self.records.last_mut()
            && *previous_index == index
            && let Some(gap) = safe_gap(*time0, dt, time)
        {
            dt.push(gap);
            texts.push(text.to_owned());
            return;
        }
        self.records.push(StreamRecord::TextChunks {
            time0: time,
            index,
            dt: Vec::new(),
            texts: vec![text.to_owned()],
        });
    }

    /// Appends a reasoning fragment, continuing the previous run when it is compatible.
    fn pack_reasoning(&mut self, index: usize, time: i64, text: &str) {
        if let Some(StreamRecord::ReasoningChunks {
            index: previous_index,
            dt,
            texts,
            time0,
        }) = self.records.last_mut()
            && *previous_index == index
            && let Some(gap) = safe_gap(*time0, dt, time)
        {
            dt.push(gap);
            texts.push(text.to_owned());
            return;
        }
        self.records.push(StreamRecord::ReasoningChunks {
            time0: time,
            index,
            dt: Vec::new(),
            texts: vec![text.to_owned()],
        });
    }

    /// Appends a tool-call argument fragment, continuing the previous run when it is compatible.
    ///
    /// "Compatible" includes the name's *presence*, not just its value: a run whose members
    /// sometimes carry a name and sometimes do not cannot be expanded back into the same chunks,
    /// because the expanded delta would repeat the run's single name on every member.
    fn pack_tool_call(
        &mut self,
        index: usize,
        time: i64,
        id: ToolCallId,
        name: Option<String>,
        arguments_delta: &str,
    ) {
        if let Some(StreamRecord::ToolCallChunks {
            index: previous_index,
            dt,
            id: previous_id,
            name: previous_name,
            args,
            time0,
        }) = self.records.last_mut()
            && *previous_index == index
            && previous_id == &id
            && previous_name == &name
            && let Some(gap) = safe_gap(*time0, dt, time)
        {
            dt.push(gap);
            args.push(arguments_delta.to_owned());
            return;
        }
        self.records.push(StreamRecord::ToolCallChunks {
            time0: time,
            index,
            dt: Vec::new(),
            id,
            name,
            args: vec![arguments_delta.to_owned()],
        });
    }

    /// The durable records built so far.
    ///
    /// Owned rather than borrowed: a durable event takes ownership of what it stores, and returning
    /// a clone keeps a caller from holding a view a later push would change underneath it.
    #[must_use]
    pub fn snapshot(&self) -> Vec<StreamRecord> {
        self.records.clone()
    }

    /// How many chunks were pushed, counting packed members individually.
    ///
    /// The number a progress indicator shows and a test asserts on: it counts *chunks*, not records,
    /// because packing is an implementation detail of storage.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records
            .iter()
            .map(|record| match record {
                StreamRecord::TextChunks { texts, .. }
                | StreamRecord::ReasoningChunks { texts, .. } => texts.len(),
                StreamRecord::ToolCallChunks { args, .. } => args.len(),
                StreamRecord::Chunk { .. } => 1,
            })
            .sum()
    }

    /// Whether nothing has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// The gap from the last member of a run to `next`, when the timeline can hold it.
///
/// `None` means "start a new record": either the arithmetic does not round-trip in the safe-integer
/// range, or the gap itself would not survive one. Returning `None` rather than an error is the
/// right shape here — an unrepresentable gap is not a malformed stream, it is a stream whose next
/// fragment must be stored with its own timestamp.
fn safe_gap(time0: i64, dt: &[i64], next: i64) -> Option<i64> {
    let previous = dt
        .iter()
        .try_fold(time0, |time, gap| time.checked_add(*gap))?;
    let gap = next.checked_sub(previous)?;
    if !is_safe_time(gap) {
        return None;
    }
    // This is what `expand` will do; requiring it here means a record this accumulator produced can
    // always be expanded back.
    if previous.checked_add(gap) != Some(next) {
        return None;
    }
    Some(gap)
}

/// Rejects a usage record whose counters cannot survive a JSON round trip.
fn check_usage(usage: &TokenUsage) -> Result<(), StreamError> {
    for (field, value) in [
        ("input", usage.input),
        ("output", usage.output),
        ("cached", usage.cached),
    ] {
        if value > MAX_SAFE_INTEGER {
            return Err(StreamError::UnsafeUsage { field, value });
        }
    }
    Ok(())
}

/// An attempt's identity, unique within one agent lifecycle.
///
/// Upstream brands a string built from the session id and the lifecycle-local attempt counter
/// ([`AttemptId::from_parts`]). Nothing validates it because nothing durable stores it — the durable
/// identity of a settled attempt is the sequence of its event, not this — so [`AttemptId::new`] does
/// not validate either. That is a deliberate divergence from [`crate::Id`], whose whole point is that
/// it cannot be built malformed; an id that never leaves the process does not earn that machinery.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttemptId(String);

impl AttemptId {
    /// Wraps a caller-chosen identity.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Builds the identity upstream derives: `"<session>:<attempt>"`.
    ///
    /// The session id is included so two agents attached to different sessions cannot collide in a
    /// consumer that folds frames from both; the counter is scoped to one agent lifecycle, which is
    /// why the session half is needed at all.
    #[must_use]
    pub fn from_parts(session: &SessionId, attempt: u64) -> Self {
        Self(format!("{}:{attempt}", session.as_str()))
    }

    /// Borrows the identity as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which durable event settled an attempt.
///
/// Two, because a settled attempt is either a message a reader renders or an attempt row that exists
/// only for accounting. The distinction reaches the client, which uses it to decide whether the
/// answer it streamed is part of the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// The attempt produced the step's assistant message.
    Message,
    /// The attempt was recorded for accounting only.
    Attempt,
}

impl Settlement {
    /// The durable event type, spelled as the session log spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "assistant/message",
            Self::Attempt => "assistant/attempt",
        }
    }
}

/// How one attempt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The matching durable event committed; the frame carries its type and sequence.
    ///
    /// The sequence travels in the frame on purpose: a follower can then resume from *the log*
    /// without re-reading the stream it just watched, which is why the terminal frame is emitted
    /// after the append rather than before.
    Committed {
        /// Which event type was written.
        event_type: Settlement,
        /// Its 1-based position in the session log.
        seq: u64,
    },

    /// No durable event was written, so a follower must discard the attempt.
    ///
    /// This is not an error: an attempt whose settlement itself failed reports that in the returned
    /// failure, and this outcome covers the case where the caller decided not to settle —
    /// cancellation before any content, or a provider failure that will be retried.
    Abandoned,
}

/// One process-local live publication from an [`Attempt`].
///
/// The three shapes are a state machine: one `Start`, a dense sequence of `Chunk`s, at most one
/// `End`. `revision` is the frame's position in the *agent lifecycle*, so a consumer that receives
/// `5` after `3` knows it missed one — which is exactly what [`StreamFold`] enforces.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// The attempt is opening, before its first chunk.
    Start {
        /// Which attempt.
        attempt_id: AttemptId,
        /// Lifecycle-local frame revision.
        revision: u64,
        /// Durable turn owning the request.
        turn: u64,
        /// Durable step owning the request.
        step: u64,
    },

    /// One chunk arrived.
    Chunk {
        /// Which attempt.
        attempt_id: AttemptId,
        /// Lifecycle-local frame revision.
        revision: u64,
        /// Dense zero-based position within the attempt.
        index: usize,
        /// Arrival time, reused by the durable embedded stream.
        time: i64,
        /// The chunk.
        chunk: StreamChunk,
    },

    /// The attempt ended.
    End {
        /// Which attempt.
        attempt_id: AttemptId,
        /// Lifecycle-local frame revision.
        revision: u64,
        /// Number of chunk frames emitted by this attempt.
        index: usize,
        /// Settlement or abandonment.
        outcome: Outcome,
    },
}

impl Frame {
    /// The frame's lifecycle revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        match self {
            Self::Start { revision, .. }
            | Self::Chunk { revision, .. }
            | Self::End { revision, .. } => *revision,
        }
    }

    /// The attempt this frame belongs to.
    #[must_use]
    pub fn attempt_id(&self) -> &AttemptId {
        match self {
            Self::Start { attempt_id, .. }
            | Self::Chunk { attempt_id, .. }
            | Self::End { attempt_id, .. } => attempt_id,
        }
    }
}

/// Allocates the revision stamped on every frame.
///
/// One allocator per attached agent lifecycle, shared by every attempt it runs: upstream states the
/// counter is "monotone within one attached Agent lifecycle", and a replacement attempt restarting
/// at 1 is what tells a consumer that the lifecycle itself was replaced. A per-attempt counter
/// could not express that, and would make a restarted lifecycle indistinguishable from a
/// continuing one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Revision(u64);

impl Revision {
    /// Creates an allocator that has issued nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues the next revision.
    pub fn issue(&mut self) -> u64 {
        self.0 = self.0.saturating_add(1);
        self.0
    }

    /// The revision issued most recently, or `0` when none has been.
    #[must_use]
    pub fn current(&self) -> u64 {
        self.0
    }
}

/// Why an attempt could not settle: the append failed, and the attempt was abandoned.
///
/// Both halves are carried because they are different facts with different owners: the cause belongs
/// to the log writer and is rethrown, while the frame is the notification a live follower needs in
/// order to drop the attempt. Upstream throws an `AggregateError` carrying both; a Rust caller
/// matches on this instead of parsing a message.
#[derive(Debug)]
pub struct SettleFailure<E> {
    /// The error the append raised.
    pub cause: E,
    /// The abandonment frame emitted in its place.
    pub frame: Frame,
}

impl<E: fmt::Display> fmt::Display for SettleFailure<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "assistant stream failed and its durable settlement was rejected: {}",
            self.cause
        )
    }
}

impl<E: std::error::Error + 'static> std::error::Error for SettleFailure<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// One model attempt: its compact stream, its live frames, and its settlement.
///
/// The lifecycle is [`start`](Self::start) → [`push`](Self::push)* → one of
/// [`settle`](Self::settle) or [`abandon`](Self::abandon). Reading happens after the stream ends:
/// [`stream`](Self::stream) is the durable shape, [`blocks`](Self::blocks) the canonical message, and
/// [`interrupted_blocks`](Self::interrupted_blocks) the safe prefix a cancelled attempt may still
/// claim.
///
/// # Why the revision allocator is a parameter and not a field
///
/// It outlives the attempt and is shared with every other attempt of the lifecycle. Storing it in
/// the struct would either force a shared-cell design — interior mutability, where a `&mut` will do —
/// or tie the attempt's lifetime to a borrow of the allocator. Taking it per call makes the shared
/// ownership explicit at exactly the three points that allocate, and leaves the attempt a plain
/// value a test can hold.
#[derive(Debug)]
pub struct Attempt {
    /// Identity within the agent lifecycle.
    id: AttemptId,
    /// Durable turn owning the request.
    turn: u64,
    /// Durable step owning the request.
    step: u64,
    /// The durable compact stream, built as chunks arrive.
    accumulator: Accumulator,
    /// The canonical block assembly over the same chunks.
    assembler: BlockAssembler,
    /// Number of chunk frames emitted, and the index the next one takes.
    index: usize,
    /// Whether a terminal frame has been emitted.
    terminal: bool,
    /// The first protocol error the assembler reported; see the module docs on strictness.
    protocol_error: Option<AssemblyError>,
    /// Indices that started and have not closed yet, in first-seen order.
    open: Vec<usize>,
    /// Latest adapter-reported usage, when one arrived.
    usage: Option<TokenUsage>,
    /// Terminal reason, when one arrived.
    finish: Option<FinishReason>,
}

impl Attempt {
    /// Opens an attempt that has emitted nothing.
    #[must_use]
    pub fn new(id: AttemptId, turn: u64, step: u64) -> Self {
        Self {
            id,
            turn,
            step,
            accumulator: Accumulator::new(),
            assembler: BlockAssembler::new(),
            index: 0,
            terminal: false,
            protocol_error: None,
            open: Vec::new(),
            usage: None,
            finish: None,
        }
    }

    /// The attempt's identity.
    #[must_use]
    pub fn attempt_id(&self) -> &AttemptId {
        &self.id
    }

    /// The durable turn owning the request.
    #[must_use]
    pub fn turn(&self) -> u64 {
        self.turn
    }

    /// The durable step owning the request.
    #[must_use]
    pub fn step(&self) -> u64 {
        self.step
    }

    /// Whether this attempt has emitted its terminal frame.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.terminal
    }

    /// How many chunk frames have been emitted.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// The first protocol error the assembler reported, if any.
    ///
    /// Latched rather than thrown: the chunk that caused it is still published and still stored, so
    /// a caller can log the defect and keep the evidence, and [`blocks`](Self::blocks) refuses to
    /// turn that evidence into a message.
    #[must_use]
    pub fn protocol_error(&self) -> Option<&AssemblyError> {
        self.protocol_error.as_ref()
    }

    /// Publishes the opening marker, before the first delivered chunk.
    ///
    /// A start frame is emitted for every attempt, including one that fails immediately: a consumer
    /// that only learns about attempts from their first chunk cannot clear the previous attempt's
    /// rendering when a retry replaces it without producing output.
    pub fn start(&self, revisions: &mut Revision) -> Frame {
        Frame::Start {
            attempt_id: self.id.clone(),
            revision: revisions.issue(),
            turn: self.turn,
            step: self.step,
        }
    }

    /// Records one chunk and returns the frame that publishes it.
    ///
    /// The chunk enters the durable stream before the assembler sees it, because the log is evidence
    /// of what arrived rather than a judgement about whether it made sense. An assembler rejection is
    /// latched for [`protocol_error`](Self::protocol_error) and still yields a frame.
    ///
    /// # Errors
    ///
    /// Returns a [`StreamError`] for a timestamp, index, or usage count that cannot be stored
    /// losslessly. Nothing is recorded in that case, so the frame index stays dense.
    pub fn push(
        &mut self,
        time: i64,
        chunk: StreamChunk,
        revisions: &mut Revision,
    ) -> Result<Frame, StreamError> {
        let timed = self.accumulator.push(time, chunk)?;
        match &timed.chunk {
            StreamChunk::Usage(usage) => self.usage = Some(*usage),
            StreamChunk::Finish { reason } => self.finish = Some(reason.clone()),
            _ => {}
        }
        self.track_open(&timed.chunk);
        if let Err(error) = self.assembler.push(timed.chunk.clone()) {
            self.protocol_error.get_or_insert(error);
        }
        let frame = Frame::Chunk {
            attempt_id: self.id.clone(),
            revision: revisions.issue(),
            index: self.index,
            time: timed.time,
            chunk: timed.chunk,
        };
        self.index += 1;
        Ok(frame)
    }

    /// Publishes the terminal settlement after the matching durable event commits.
    ///
    /// The append runs first and the frame is built from its result, which is what makes the
    /// terminal frame's `seq` trustworthy: a follower that resumes from it cannot land before the
    /// event it is meant to resume from.
    ///
    /// # Errors
    ///
    /// Returns [`SettleFailure`] carrying both the append's error and the abandonment frame emitted
    /// in its place. The attempt is terminal either way — a settlement that failed cannot be retried
    /// on the same attempt, because the stream it would settle has already been published.
    // The failure carries the terminal frame the follower must publish, so the `Err` variant is as
    // large as a frame. Boxing it would trade one allocation on a rare path for an awkward
    // `Box<Frame>` in every caller that wants to read the abandonment it was handed.
    #[allow(
        clippy::result_large_err,
        reason = "the error carries the abandonment frame the caller must publish"
    )]
    pub fn settle<E>(
        &mut self,
        event_type: Settlement,
        append: impl FnOnce() -> Result<u64, E>,
        revisions: &mut Revision,
    ) -> Result<Frame, SettleFailure<E>> {
        match append() {
            Ok(seq) => {
                self.terminal = true;
                Ok(self.end_frame(revisions, Outcome::Committed { event_type, seq }))
            }
            Err(cause) => {
                let frame = self.abandon(revisions);
                Err(SettleFailure { cause, frame })
            }
        }
    }

    /// Publishes abandonment when no durable attempt event can be committed.
    ///
    /// Not idempotent: a second abandonment produces a second terminal frame, which a consumer
    /// rejects as a revision it did not expect. That is the honest outcome, since the caller has
    /// published two contradicting endings.
    pub fn abandon(&mut self, revisions: &mut Revision) -> Frame {
        self.terminal = true;
        self.end_frame(revisions, Outcome::Abandoned)
    }

    /// Builds the terminal frame from the current index.
    fn end_frame(&self, revisions: &mut Revision, outcome: Outcome) -> Frame {
        Frame::End {
            attempt_id: self.id.clone(),
            revision: revisions.issue(),
            index: self.index,
            outcome,
        }
    }

    /// Notes which blocks started and which closed, in first-seen order.
    ///
    /// Only indices a `block-start` announced are tracked: a `block-end` with no start is a complete
    /// block the assembler accepts on its own, so there is nothing to close for it at the end of the
    /// stream.
    fn track_open(&mut self, chunk: &StreamChunk) {
        match chunk {
            // A second start for one index is a duplicate the assembler reports; it must not be
            // tracked twice here either, or the end of the stream would try to close it twice.
            StreamChunk::BlockStart { index, .. } if !self.open.contains(index) => {
                self.open.push(*index);
            }
            StreamChunk::BlockEnd { index, .. } => {
                self.open.retain(|open| open != index);
            }
            _ => {}
        }
    }

    /// The exact compact stream for the final durable event.
    ///
    /// Owned, and readable after [`blocks`](Self::blocks) or
    /// [`interrupted_blocks`](Self::interrupted_blocks): the accumulator is never consumed by a read.
    #[must_use]
    pub fn stream(&self) -> Vec<StreamRecord> {
        self.accumulator.snapshot()
    }

    /// Latest adapter-reported usage in the stream.
    #[must_use]
    pub fn usage(&self) -> Option<TokenUsage> {
        self.usage
    }

    /// Terminal reason, defaulting to [`FinishReason::Stop`] when the stream omitted one.
    ///
    /// The default is upstream's and it is not an oversight: every consumer downstream would
    /// otherwise need an `Option<FinishReason>` for a value that only ever means "the provider did
    /// not say, so treat it as a normal end". A stream that actually failed says so with
    /// [`FinishReason::Error`].
    #[must_use]
    pub fn finish(&self) -> FinishReason {
        self.finish.clone().unwrap_or(FinishReason::Stop)
    }

    /// Canonical completed-message blocks from the same chunks.
    ///
    /// A block the stream never closed is closed from its accumulated deltas — some providers never
    /// send `block-end` for a tool call — which is the same rule [`crate::StreamObserver::finish`]
    /// applies to a stream that announced `Finish`.
    ///
    /// Terminal and one-shot: closing the open blocks consumes them, so a second call returns what
    /// is left, and a call to [`interrupted_blocks`](Self::interrupted_blocks) after this one has
    /// nothing left to read.
    ///
    /// # Errors
    ///
    /// Returns the first protocol error the assembler reported while chunks were pushed. Handing back
    /// a message assembled across a stream known to be malformed would make a shifted index look like
    /// content.
    pub fn blocks(&mut self) -> Result<Vec<ContentBlock>, AssemblyError> {
        if let Some(error) = self.protocol_error.take() {
            return Err(error);
        }
        self.close_open();
        std::mem::take(&mut self.assembler).finish()
    }

    /// The safe visible prefix when cancellation interrupts the attempt.
    ///
    /// Closed and open text and reasoning blocks with non-whitespace content, in first-seen order.
    /// Tool calls are omitted, and not because they are inconvenient: an interruption precedes
    /// dispatch, so retaining one would demand a fabricated result to pair with it. Open blocks of an
    /// unknown kind are omitted for the same reason the assembler cannot materialize them — nothing
    /// says what they are.
    ///
    /// Infallible by design: a cancelled attempt's caller is already unwinding, and a second error on
    /// the way out would replace the failure that caused the cancellation with a less useful one. A
    /// stream the assembler cannot finish yields no prefix rather than a partial one.
    ///
    /// Terminal and one-shot, like [`blocks`](Self::blocks).
    pub fn interrupted_blocks(&mut self) -> Vec<ContentBlock> {
        self.close_open();
        match std::mem::take(&mut self.assembler).finish() {
            Ok(blocks) => blocks
                .into_iter()
                .filter(|block| match block {
                    ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                        has_non_whitespace(text)
                    }
                    _ => false,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Closes every block the stream left open, from its accumulated deltas.
    ///
    /// An empty text block is the one closing shape the assembler reads as "this carries no content
    /// of its own, read the deltas instead", whatever kind the partial is. One placeholder therefore
    /// closes a text, a reasoning, and a tool-call block alike.
    fn close_open(&mut self) {
        let open = std::mem::take(&mut self.open);
        for index in open {
            // A close the assembler refuses (a duplicate, or an index already taken) is ignored: the
            // block is gone either way, and the malformation that caused it is already latched.
            let _ = self.assembler.push(StreamChunk::BlockEnd {
                index,
                block: ContentBlock::Text {
                    text: String::new(),
                },
            });
        }
    }
}

/// One attempt as a reconnecting follower sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct AttemptView {
    /// Which attempt is streaming.
    pub attempt_id: AttemptId,
    /// Durable log cursor when the attempt's start frame was observed.
    ///
    /// A cursor, not a sequence: `-1` means "before the first event", which is what an empty log
    /// has. Storing `0` instead would claim the attempt started after a first event that does not
    /// exist, and a follower resuming from it would skip one.
    pub started_after_seq: i64,
    /// Durable turn owning the request.
    pub turn: u64,
    /// Durable step owning the request.
    pub step: u64,
    /// The next dense chunk index the attempt must publish.
    pub next_index: usize,
    /// Everything streamed so far, compacted.
    pub stream: Vec<StreamRecord>,
}

/// The immutable reconnect baseline for one accepted revision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Baseline {
    /// The last accepted lifecycle revision.
    ///
    /// `0` means "nothing accepted yet", which is distinct from "an attempt at revision 0" — there
    /// is no such attempt.
    pub revision: u64,
    /// The attempt streaming at that revision, when one is.
    pub active_attempt: Option<AttemptView>,
}

/// The attempt a fold is currently accumulating.
#[derive(Debug)]
struct Active {
    /// Which attempt.
    id: AttemptId,
    /// Cursor observed with the start frame.
    started_after_seq: i64,
    /// Durable turn.
    turn: u64,
    /// Durable step.
    step: u64,
    /// Compact stream built from the attempt's chunk frames.
    stream: Accumulator,
    /// Next index the attempt must publish.
    next_index: usize,
}

/// Folds dense live frames into one reconnect baseline.
///
/// # Why a gap is not a resync
///
/// Revisions are dense, and a consumer that receives `n+2` after `n` has missed exactly one frame —
/// a chunk in the middle of an answer. Continuing to append from there would render a response with
/// silently missing text, and there is no way to ask for the lost frame again. So a gap drops the
/// active attempt entirely and only records the new revision; the follower's next baseline says "an
/// attempt was streaming and is no longer available", which is the truth.
///
/// # Why a replacement restarts at revision 1
///
/// When the agent lifecycle itself is replaced — a session reloaded, a new attachment — its new
/// allocator starts again at 1. A fold that carried its revision across that boundary would reject
/// every frame of the new lifecycle as stale. Upstream's rule is therefore exact: a `start` at
/// revision 1 with a non-zero current revision *is* the replacement signal, and it resets both the
/// baseline and the accumulator before the frame is accepted in the ordinary way.
#[derive(Debug, Default)]
pub struct StreamFold {
    /// The attempt currently streaming, when one is.
    active: Option<Active>,
    /// The last accepted lifecycle revision.
    revision: u64,
    /// The baseline materialized at `revision`.
    baseline: Baseline,
    /// Whether `baseline` no longer reflects the fold's state.
    dirty: bool,
}

impl StreamFold {
    /// Creates a fold that has accepted nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one frame, observed when the durable cursor was `durable_cursor`.
    ///
    /// # Errors
    ///
    /// Returns a [`StreamError`] when a chunk frame's time or index cannot be stored losslessly. The
    /// frame is still consumed — its revision was already accepted — because the alternative is to
    /// stall a live follower on a frame it can never accept.
    pub fn accept(&mut self, frame: &Frame, durable_cursor: i64) -> Result<(), StreamError> {
        // A replacement lifecycle announces itself with `start` at revision 1; see the type docs.
        if matches!(frame, Frame::Start { revision: 1, .. }) && self.revision != 0 {
            self.active = None;
            self.revision = 0;
        }
        if frame.revision() != self.revision.saturating_add(1) {
            self.active = None;
            self.revision = frame.revision();
            self.dirty = true;
            return Ok(());
        }
        self.revision = frame.revision();
        match frame {
            Frame::Start {
                attempt_id,
                turn,
                step,
                ..
            } => {
                self.active = Some(Active {
                    id: attempt_id.clone(),
                    started_after_seq: durable_cursor,
                    turn: *turn,
                    step: *step,
                    stream: Accumulator::new(),
                    next_index: 0,
                });
            }
            Frame::Chunk {
                attempt_id,
                index,
                time,
                chunk,
                ..
            } => {
                let accepted = match self.active.as_mut() {
                    Some(active) if active.id == *attempt_id && *index == active.next_index => {
                        let pushed = active.stream.push(*time, chunk.clone());
                        if pushed.is_ok() {
                            active.next_index += 1;
                        }
                        Some(pushed)
                    }
                    _ => None,
                };
                match accepted {
                    // The frame did not continue the attempt being accumulated: drop it, because a
                    // stream assembled across a hole is worse than no stream.
                    None => self.active = None,
                    Some(Err(error)) => {
                        // The same reasoning applies to a chunk that cannot be stored: the attempt
                        // already has a hole where this frame should have been.
                        self.active = None;
                        self.dirty = true;
                        return Err(error);
                    }
                    Some(Ok(_)) => {}
                }
            }
            Frame::End { .. } => self.active = None,
        }
        self.dirty = true;
        Ok(())
    }

    /// The reconnect baseline for the latest accepted revision.
    ///
    /// Materialized lazily and cached: a follower polling a quiet session gets the same value back
    /// without rebuilding it, and the borrow makes that sharing visible in the type rather than in a
    /// comment.
    pub fn snapshot(&mut self) -> &Baseline {
        if !self.dirty {
            return &self.baseline;
        }
        self.baseline = Baseline {
            revision: self.revision,
            active_attempt: self.active.as_ref().map(|active| AttemptView {
                attempt_id: active.id.clone(),
                started_after_seq: active.started_after_seq,
                turn: active.turn,
                step: active.step,
                next_index: active.next_index,
                stream: active.stream.snapshot(),
            }),
        };
        self.dirty = false;
        &self.baseline
    }

    /// The last accepted revision, whether or not a baseline has been materialized.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }
}
