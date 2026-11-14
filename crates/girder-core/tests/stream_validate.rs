//! Tests for stream shape validation.
//!
//! The cases that drive the design come first: an index gap must be *recorded* but must not be
//! fatal, because `OpenAI` reports parallel tool calls out of order and non-contiguously, and a
//! delta after a block closed must be fatal, because that content can never be applied. The
//! truncation cases follow the same split along [`Ending`]: the identical stream is a data-loss
//! report when the caller read to the end and a normal cancellation when it did not.
//!
//! # Why the module is compiled by path
//!
//! `stream_validate` is not exported from the crate root; the wiring is a separate change to
//! `lib.rs`. Compiling the exact source file here keeps these tests runnable and independent of
//! that wiring, while still exercising the same code a consumer will reach through
//! `girder_core::stream_validate`. Nothing is duplicated: this is the real file. This mirrors how
//! `tests/block_assembler.rs` tests the assembler before it was exported.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/stream_validate.rs"]
mod stream_validate;

use girder_core::{ContentBlock, FinishReason, StreamChunk, TokenUsage, ToolCallId};
use stream_validate::{Ending, Problem, ProblemKind, is_fatal, validate, validate_with};

fn start(index: usize, block_type: &str) -> StreamChunk {
    StreamChunk::BlockStart {
        index,
        block_type: block_type.to_owned(),
    }
}

fn text(index: usize, text: &str) -> StreamChunk {
    StreamChunk::TextDelta {
        index,
        text: text.to_owned(),
    }
}

fn reasoning(index: usize, text: &str) -> StreamChunk {
    StreamChunk::ReasoningDelta {
        index,
        text: text.to_owned(),
    }
}

fn tool_delta(index: usize, arguments: &str) -> StreamChunk {
    StreamChunk::ToolCallDelta {
        index,
        id: ToolCallId::new(format!("call-{index}")).unwrap(),
        name: None,
        arguments_delta: arguments.to_owned(),
    }
}

fn end_text(index: usize, text: &str) -> StreamChunk {
    StreamChunk::BlockEnd {
        index,
        block: ContentBlock::Text {
            text: text.to_owned(),
        },
    }
}

fn end_placeholder(index: usize) -> StreamChunk {
    StreamChunk::BlockEnd {
        index,
        block: ContentBlock::Text {
            text: String::new(),
        },
    }
}

fn usage() -> StreamChunk {
    StreamChunk::Usage(TokenUsage {
        input: 10,
        output: 5,
        cached: 0,
    })
}

fn finish() -> StreamChunk {
    StreamChunk::Finish {
        reason: FinishReason::Stop,
    }
}

fn kinds(problems: &[Problem]) -> Vec<ProblemKind> {
    problems
        .iter()
        .map(|problem| problem.kind.clone())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------------------------

#[test]
fn well_formed_stream_has_no_problems() {
    let chunks = vec![
        start(0, "reasoning"),
        reasoning(0, "think"),
        end_placeholder(0),
        start(1, "text"),
        text(1, "hello "),
        text(1, "world"),
        end_text(1, "hello world"),
        usage(),
        finish(),
    ];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
    assert!(!is_fatal(&problems));
}

#[test]
fn empty_stream_has_no_problems() {
    // An empty stream is a caller that read nothing at all. There is nothing malformed about it
    // and no open block, so it is clean rather than truncated.
    let problems = validate(&[]);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
    assert!(!is_fatal(&problems));
}

#[test]
fn tool_call_stream_validates_cleanly() {
    let chunks = vec![
        start(0, "tool-call"),
        tool_delta(0, "{\"path\":"),
        tool_delta(0, "\"a\"}"),
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::new("call-0").unwrap(),
                name: "read".to_owned(),
                arguments: "{\"path\":\"a\"}".to_owned(),
            },
        },
        usage(),
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
}

// ---------------------------------------------------------------------------------------------
// IndexGap: recorded, never fatal (the core case)
// ---------------------------------------------------------------------------------------------

#[test]
fn index_gap_is_recorded_but_not_fatal() {
    // OpenAI's tool_calls carry an index that is a discriminator, not a sequence number, so a
    // gateway can report parallel calls non-contiguously. The assembler keys by index and is
    // unaffected; this module records the gap because a consumer that indexes an array by block
    // number would silently misplace the block.
    let chunks = vec![
        start(0, "text"),
        text(0, "first"),
        end_text(0, "first"),
        start(2, "text"),
        text(2, "third"),
        end_text(2, "third"),
        finish(),
    ];

    let problems = validate(&chunks);

    assert_eq!(
        kinds(&problems),
        vec![ProblemKind::IndexGap { from: 0, to: 2 }]
    );
    assert!(!is_fatal(&problems), "gaps are legal on this wire");
}

#[test]
fn out_of_order_indices_are_not_a_gap() {
    // Indices that arrive backwards are the interleaved case, not a gap: block 2 starts before
    // block 0 has ever been seen. `from` is a high-water mark, so there is nothing to report.
    let chunks = vec![
        start(2, "text"),
        start(0, "text"),
        text(2, "later"),
        text(0, "earlier"),
        end_text(0, "earlier"),
        end_text(2, "later"),
        finish(),
    ];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
}

#[test]
fn many_open_blocks_are_not_reported_as_unclosed() {
    // Two indices open at once is legal — this project's own OpenAI adapter opens reasoning,
    // text, and tool blocks as deltas arrive and closes them at finish. Only a restart of the
    // *same* index is reported, so this stream must be clean.
    let chunks = vec![
        start(0, "reasoning"),
        start(1, "text"),
        start(2, "tool-call"),
        reasoning(0, "r"),
        text(1, "t"),
        tool_delta(2, "{}"),
        end_placeholder(0),
        end_text(1, "t"),
        StreamChunk::BlockEnd {
            index: 2,
            block: ContentBlock::ToolCall {
                id: ToolCallId::new("call-2").unwrap(),
                name: "noop".to_owned(),
                arguments: "{}".to_owned(),
            },
        },
        usage(),
        finish(),
    ];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
}

#[test]
fn gap_and_fatal_problem_can_coexist() {
    // A stream can be both gapped and broken. `is_fatal` must report the verdict for the whole
    // list, not for the first problem it happens to see.
    let chunks = vec![
        start(0, "text"),
        end_text(0, "a"),
        start(2, "text"),
        text(7, "orphan"),
        finish(),
    ];

    let problems = validate(&chunks);

    assert_eq!(
        kinds(&problems),
        vec![
            ProblemKind::IndexGap { from: 0, to: 2 },
            ProblemKind::NoBlock { index: 7 },
        ]
    );
    assert!(is_fatal(&problems));
}

// ---------------------------------------------------------------------------------------------
// AfterEnd: fatal (the other core case)
// ---------------------------------------------------------------------------------------------

#[test]
fn delta_after_end_is_fatal() {
    // The block closed and may already have been handed to the caller. The delta can neither be
    // appended nor start a new block, so the content is lost — and the assembler drops it with no
    // error, which is why this module has to report it.
    let chunks = vec![
        start(0, "text"),
        text(0, "hi"),
        end_text(0, "hi"),
        text(0, "more"),
        finish(),
    ];

    let problems = validate(&chunks);

    assert_eq!(kinds(&problems), vec![ProblemKind::AfterEnd { index: 0 }]);
    assert!(is_fatal(&problems));
}

#[test]
fn tool_delta_after_end_is_fatal() {
    let chunks = vec![
        start(0, "tool-call"),
        tool_delta(0, "{"),
        end_placeholder(0),
        tool_delta(0, "\"late\":1}"),
        finish(),
    ];

    let problems = validate(&chunks);

    assert_eq!(kinds(&problems), vec![ProblemKind::AfterEnd { index: 0 }]);
    assert!(is_fatal(&problems));
}

#[test]
fn restart_after_end_is_fatal() {
    // A second start cannot reopen a block that was already closed and possibly delivered, so it
    // is the same loss as a trailing delta rather than an `Unclosed` restart.
    let chunks = vec![
        start(0, "text"),
        end_text(0, "a"),
        start(0, "text"),
        text(0, "b"),
        finish(),
    ];

    let problems = validate(&chunks);

    assert_eq!(
        kinds(&problems),
        vec![
            ProblemKind::AfterEnd { index: 0 },
            ProblemKind::AfterEnd { index: 0 },
        ]
    );
    assert!(is_fatal(&problems));
}

#[test]
fn repeated_block_end_is_tolerated() {
    // Closing is one-way, so a repeated `block-end` decides nothing new. Providers repeat it, so
    // it is not reported; the first close already fixed the block.
    let chunks = vec![
        start(0, "text"),
        end_text(0, "a"),
        end_text(0, "a"),
        finish(),
    ];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
}

// ---------------------------------------------------------------------------------------------
// NoBlock: fatal
// ---------------------------------------------------------------------------------------------

#[test]
fn delta_without_block_is_fatal() {
    let chunks = vec![text(0, "orphan"), finish()];

    let problems = validate(&chunks);

    assert_eq!(kinds(&problems), vec![ProblemKind::NoBlock { index: 0 }]);
    assert!(is_fatal(&problems));
}

#[test]
fn reasoning_delta_without_block_is_fatal() {
    let chunks = vec![reasoning(3, "orphan"), finish()];

    let problems = validate(&chunks);

    assert_eq!(kinds(&problems), vec![ProblemKind::NoBlock { index: 3 }]);
    assert!(is_fatal(&problems));
}

#[test]
fn tool_delta_without_block_is_fatal() {
    // The worst variant: folding the stream materializes this into a block that was never
    // announced, inventing a tool call the model did not make in the order the stream described.
    let chunks = vec![tool_delta(1, "{}"), finish()];

    let problems = validate(&chunks);

    assert_eq!(kinds(&problems), vec![ProblemKind::NoBlock { index: 1 }]);
    assert!(is_fatal(&problems));
}

#[test]
fn block_end_without_start_is_not_reported() {
    // A `block-end` carries the finished content, so it is interpretable on its own and the
    // assembler accepts it as a complete block. Refusing it would reject a complete stream.
    let chunks = vec![end_text(0, "whole block"), finish()];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
}

// ---------------------------------------------------------------------------------------------
// Unclosed: recorded, not fatal
// ---------------------------------------------------------------------------------------------

#[test]
fn same_index_restart_is_unclosed() {
    // A second start for an index that is still open merges two blocks. The assembler already
    // rejects it explicitly as `DuplicateStart`, so this module records the shape without also
    // declaring the content untrustworthy.
    let chunks = vec![
        start(0, "text"),
        text(0, "a"),
        start(0, "text"),
        text(0, "b"),
        end_text(0, "ab"),
        finish(),
    ];

    let problems = validate(&chunks);

    assert_eq!(kinds(&problems), vec![ProblemKind::Unclosed { index: 0 }]);
    assert!(!is_fatal(&problems));
}

#[test]
fn open_block_with_finish_is_not_truncated() {
    // This is the documented provider shape: some providers never send `block-end` for a tool
    // call, and `StreamObserver` closes them from the terminal `Finish`. The stream said it was
    // done, so the open block is an implicit close and not data loss.
    let chunks = vec![
        start(0, "tool-call"),
        tool_delta(0, "{}"),
        usage(),
        finish(),
    ];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
}

// ---------------------------------------------------------------------------------------------
// Truncated: depends on how the caller's reading ended
// ---------------------------------------------------------------------------------------------

#[test]
fn truncated_stream_is_recorded() {
    // No terminal `finish` and a block still open: the provider hung up mid-block. The problem is
    // attributed to the position just past the last chunk, because no chunk is to blame.
    let chunks = vec![start(0, "tool-call"), tool_delta(0, "{\"path\":")];

    let problems = validate(&chunks);

    assert_eq!(
        problems,
        vec![Problem {
            at: 2,
            kind: ProblemKind::Truncated { open: vec![0] },
        }]
    );
    assert!(is_fatal(&problems));
}

#[test]
fn truncated_lists_open_indices_ascending() {
    // Started out of order, so the report must sort rather than echo arrival order: two calls to
    // the same stream must produce the same report.
    let chunks = vec![start(2, "text"), start(0, "text")];

    let problems = validate(&chunks);

    assert_eq!(
        kinds(&problems),
        vec![ProblemKind::Truncated { open: vec![0, 2] }]
    );
}

#[test]
fn cancelled_stream_is_not_truncated() {
    // The identical shape as `truncated_stream_is_recorded`, read under cancellation: an
    // interrupted response is truncated by construction, so reporting it would train callers to
    // ignore the report.
    let chunks = vec![start(0, "tool-call"), tool_delta(0, "{\"path\":")];

    let problems = validate_with(&chunks, Ending::Cancelled);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
    assert!(!is_fatal(&problems));
}

#[test]
fn cancelled_stream_still_reports_real_defects() {
    // Cancellation excuses the *ending* only. A delta for a block that never started is a defect
    // however the caller stopped reading, and burying it under "we cancelled" would hide it.
    let chunks = vec![start(0, "text"), text(5, "nowhere")];

    let problems = validate_with(&chunks, Ending::Cancelled);

    assert_eq!(kinds(&problems), vec![ProblemKind::NoBlock { index: 5 }]);
    assert!(is_fatal(&problems));
}

#[test]
fn closed_stream_without_finish_is_not_truncated() {
    // Everything closed, but no `finish` chunk. Nothing is missing from the content, so there is
    // no truncation to report even though the ending was unaccounted for.
    let chunks = vec![start(0, "text"), text(0, "a"), end_text(0, "a")];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
}

// ---------------------------------------------------------------------------------------------
// UsageAfterFinish
// ---------------------------------------------------------------------------------------------

#[test]
fn usage_after_finish_is_recorded_but_not_fatal() {
    // Accounting only: the finish reason is already known and the blocks are unaffected. Recorded
    // because a provider that keeps sending after declaring the response complete is worth a log
    // line.
    let chunks = vec![
        start(0, "text"),
        text(0, "a"),
        end_text(0, "a"),
        finish(),
        usage(),
    ];

    let problems = validate(&chunks);

    assert_eq!(kinds(&problems), vec![ProblemKind::UsageAfterFinish]);
    assert!(!is_fatal(&problems));
}

#[test]
fn usage_before_finish_is_clean() {
    let chunks = vec![
        start(0, "text"),
        text(0, "a"),
        end_text(0, "a"),
        usage(),
        finish(),
    ];

    let problems = validate(&chunks);

    assert!(problems.is_empty(), "unexpected problems: {problems:?}");
}

#[test]
fn every_usage_after_finish_is_recorded() {
    let chunks = vec![finish(), usage(), usage()];

    let problems = validate(&chunks);

    assert_eq!(
        kinds(&problems),
        vec![ProblemKind::UsageAfterFinish, ProblemKind::UsageAfterFinish]
    );
}

// ---------------------------------------------------------------------------------------------
// is_fatal classification
// ---------------------------------------------------------------------------------------------

#[test]
fn is_fatal_treats_only_content_losing_problems_as_fatal() {
    let not_fatal = [
        ProblemKind::IndexGap { from: 0, to: 2 },
        ProblemKind::Unclosed { index: 0 },
        ProblemKind::UsageAfterFinish,
    ];
    for kind in not_fatal {
        let problems = vec![Problem {
            at: 0,
            kind: kind.clone(),
        }];
        assert!(!is_fatal(&problems), "{kind:?} must not be fatal");
    }

    let fatal = [
        ProblemKind::AfterEnd { index: 0 },
        ProblemKind::NoBlock { index: 0 },
        ProblemKind::Truncated { open: vec![0] },
    ];
    for kind in fatal {
        let problems = vec![Problem {
            at: 0,
            kind: kind.clone(),
        }];
        assert!(is_fatal(&problems), "{kind:?} must be fatal");
    }
}

#[test]
fn is_fatal_is_false_for_no_problems() {
    assert!(!is_fatal(&[]));
}

#[test]
fn a_fatal_problem_after_a_benign_one_still_counts() {
    // Guards the "any" in `is_fatal`: a list whose first problem is benign must not short-circuit
    // to the benign verdict.
    let problems = vec![
        Problem {
            at: 0,
            kind: ProblemKind::IndexGap { from: 0, to: 3 },
        },
        Problem {
            at: 1,
            kind: ProblemKind::UsageAfterFinish,
        },
        Problem {
            at: 2,
            kind: ProblemKind::NoBlock { index: 9 },
        },
    ];

    assert!(is_fatal(&problems));
}

// ---------------------------------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------------------------------

#[test]
fn problems_point_at_the_offending_chunk() {
    let chunks = vec![
        start(0, "text"), // 0
        end_text(0, "a"), // 1
        start(2, "text"), // 2 -> gap
        text(2, "b"),     // 3
        end_text(2, "b"), // 4
        text(2, "late"),  // 5 -> after end
        finish(),         // 6
        usage(),          // 7 -> usage after finish
    ];

    let problems = validate(&chunks);

    assert_eq!(
        problems,
        vec![
            Problem {
                at: 2,
                kind: ProblemKind::IndexGap { from: 0, to: 2 },
            },
            Problem {
                at: 5,
                kind: ProblemKind::AfterEnd { index: 2 },
            },
            Problem {
                at: 7,
                kind: ProblemKind::UsageAfterFinish,
            },
        ]
    );
}
