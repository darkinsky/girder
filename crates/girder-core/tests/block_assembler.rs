//! Tests for incremental block assembly.
//!
//! The case that drives the design is [`interleaved_blocks_do_not_corrupt_each_other`]: two blocks
//! whose deltas alternate. A single-slot assembler passes "one block at a time" and fails that one,
//! so it is tested before anything else.
//!
//! # Why the module is compiled by path
//!
//! `block_assembler` is not exported from the crate root yet; the wiring is a separate change to
//! `lib.rs`. Compiling the exact source file here keeps these tests runnable and independent of
//! that wiring, while still exercising the same code a consumer will reach through
//! `girder_core::block_assembler`. Nothing is duplicated: this is the real file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/block_assembler.rs"]
mod block_assembler;

use block_assembler::{Assembler, AssemblyError};
use girder_core::{ContentBlock, FinishReason, StreamChunk, TokenUsage, ToolCallId};

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

fn tool(index: usize, id: &str, name: Option<&str>, arguments: &str) -> StreamChunk {
    StreamChunk::ToolCallDelta {
        index,
        id: ToolCallId::new(id).expect("valid test id"),
        name: name.map(str::to_owned),
        arguments_delta: arguments.to_owned(),
    }
}

fn end(index: usize, block: ContentBlock) -> StreamChunk {
    StreamChunk::BlockEnd { index, block }
}

/// The placeholder `block-end` this project's own adapter emits: the content is in the deltas.
fn placeholder_text(index: usize) -> StreamChunk {
    end(
        index,
        ContentBlock::Text {
            text: String::new(),
        },
    )
}

fn visible(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

fn feed(assembler: &mut Assembler, chunks: Vec<StreamChunk>) {
    for chunk in chunks {
        assembler.push(chunk).expect("chunk must be accepted");
    }
}

// ---- the happy path -----------------------------------------------------------------------------

#[test]
fn a_single_text_block_assembles_from_its_deltas() {
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(0, "text"),
            text(0, "he"),
            text(0, "llo"),
            placeholder_text(0),
        ],
    );

    assert!(!assembler.has_open_blocks());
    assert_eq!(assembler.take_finished(), vec![visible("hello")]);
    assert_eq!(assembler.finish().expect("all closed"), Vec::new());
}

#[test]
fn a_closing_block_with_content_is_taken_verbatim() {
    // Providers that assemble the block themselves send it complete; the deltas are then a
    // duplicate and must not be appended to the authoritative text.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![start(0, "text"), text(0, "h"), end(0, visible("hello"))],
    );

    assert_eq!(assembler.finish().expect("closed"), vec![visible("hello")]);
}

#[test]
fn take_finished_drains_and_leaves_open_blocks_behind() {
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(0, "text"),
            text(0, "first"),
            placeholder_text(0),
            start(1, "text"),
            text(1, "still going"),
        ],
    );

    assert!(assembler.has_open_blocks());
    assert_eq!(assembler.take_finished(), vec![visible("first")]);
    // Draining is not a copy: a second read must not repeat the block a caller already rendered.
    assert!(assembler.take_finished().is_empty());
    assert!(assembler.has_open_blocks());
}

// ---- interleaving, the reason the module exists -------------------------------------------------

#[test]
fn interleaved_blocks_do_not_corrupt_each_other() {
    // Block 0 and block 1 alternate, which is what two concurrently streamed blocks look like on
    // the wire. A single-slot assembler appends both streams into one buffer and yields "abab".
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(0, "text"),
            text(0, "a"),
            start(1, "reasoning"),
            reasoning(1, "x"),
            text(0, "b"),
            reasoning(1, "y"),
            placeholder_text(0),
            end(
                1,
                ContentBlock::Reasoning {
                    text: String::new(),
                },
            ),
        ],
    );

    assert_eq!(
        assembler.finish().expect("both closed"),
        vec![
            visible("ab"),
            ContentBlock::Reasoning {
                text: "xy".to_owned()
            }
        ]
    );
}

#[test]
fn a_later_block_can_start_before_an_earlier_one_closes() {
    // The indices need not arrive in order either: block 1 starts first and finishes first.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(1, "text"),
            text(1, "second"),
            start(0, "text"),
            text(0, "first"),
            placeholder_text(1),
        ],
    );

    // Only the closed block comes out of the incremental read; the open one stays.
    assert_eq!(assembler.take_finished(), vec![visible("second")]);

    feed(&mut assembler, vec![placeholder_text(0)]);
    assert_eq!(assembler.take_finished(), vec![visible("first")]);
}

// ---- errors -------------------------------------------------------------------------------------

#[test]
fn a_delta_without_a_start_is_an_error() {
    // Silently dropping it would show up later as a missing block, far from the cause.
    let mut assembler = Assembler::new();

    let error = assembler
        .push(text(3, "orphan"))
        .expect_err("a delta with no block-start must not be accepted");

    assert_eq!(error, AssemblyError::OrphanDelta { index: 3 });
}

#[test]
fn an_orphan_reasoning_delta_is_also_an_error() {
    let mut assembler = Assembler::new();

    let error = assembler
        .push(reasoning(1, "orphan"))
        .expect_err("a reasoning delta with no block-start must not be accepted");

    assert_eq!(error, AssemblyError::OrphanDelta { index: 1 });
}

#[test]
fn an_orphan_tool_call_delta_is_also_an_error() {
    let mut assembler = Assembler::new();

    let error = assembler
        .push(tool(2, "call-1", Some("list"), "{}"))
        .expect_err("a tool delta with no block-start must not be accepted");

    assert_eq!(error, AssemblyError::OrphanDelta { index: 2 });
}

#[test]
fn a_second_start_for_the_same_index_is_an_error() {
    let mut assembler = Assembler::new();
    feed(&mut assembler, vec![start(0, "text"), text(0, "kept")]);

    let error = assembler
        .push(start(0, "text"))
        .expect_err("a repeated block-start must not reset the accumulated text");

    assert_eq!(error, AssemblyError::DuplicateStart { index: 0 });
    // The rejected start must not have disturbed the block it collided with.
    feed(&mut assembler, vec![placeholder_text(0)]);
    assert_eq!(assembler.take_finished(), vec![visible("kept")]);
}

#[test]
fn a_start_for_an_already_taken_index_is_an_error() {
    // The index was drained by `take_finished`, so a new start is the stream contradicting itself.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![start(0, "text"), text(0, "done"), placeholder_text(0)],
    );
    assert_eq!(assembler.take_finished(), vec![visible("done")]);

    let error = assembler
        .push(start(0, "text"))
        .expect_err("an index that already produced a block cannot be reused");

    assert_eq!(error, AssemblyError::DuplicateStart { index: 0 });
}

#[test]
fn finishing_with_an_open_block_is_an_error() {
    // Half a block is not a block. Returning it would make a truncated stream look complete.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![start(0, "text"), text(0, "half"), start(1, "text")],
    );

    let error = assembler
        .finish()
        .expect_err("an unclosed block must not be returned");

    assert_eq!(error, AssemblyError::Unclosed { count: 2 });
}

#[test]
fn finishing_with_only_taken_blocks_succeeds() {
    // Blocks drained by `take_finished` are complete; they must not count as unclosed.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![start(0, "text"), text(0, "done"), placeholder_text(0)],
    );
    assert_eq!(assembler.take_finished(), vec![visible("done")]);

    assert_eq!(assembler.finish().expect("nothing open"), Vec::new());
}

// ---- ordering and emptiness ---------------------------------------------------------------------

#[test]
fn blocks_come_back_in_index_order_regardless_of_arrival_order() {
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(2, "text"),
            text(2, "third"),
            start(0, "text"),
            text(0, "first"),
            start(1, "text"),
            text(1, "second"),
            placeholder_text(2),
            placeholder_text(0),
            placeholder_text(1),
        ],
    );

    assert_eq!(
        assembler.finish().expect("all closed"),
        vec![visible("first"), visible("second"), visible("third")]
    );
}

#[test]
fn an_empty_stream_finishes_with_no_blocks() {
    let assembler = Assembler::new();

    assert!(!assembler.has_open_blocks());
    assert_eq!(assembler.finish().expect("nothing open"), Vec::new());
}

#[test]
fn usage_and_finish_chunks_carry_no_blocks() {
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            StreamingAccounting::usage(),
            start(0, "text"),
            text(0, "answer"),
            placeholder_text(0),
            StreamChunk::Finish {
                reason: FinishReason::Stop,
            },
        ],
    );

    assert!(!assembler.has_open_blocks());
    assert_eq!(assembler.finish().expect("closed"), vec![visible("answer")]);
}

/// Accounting chunks in the shape a provider sends them.
struct StreamingAccounting;

impl StreamingAccounting {
    fn usage() -> StreamChunk {
        StreamChunk::Usage(TokenUsage {
            input: 7,
            output: 3,
            cached: 0,
        })
    }
}

// ---- tool calls ---------------------------------------------------------------------------------

#[test]
fn a_tool_call_accumulates_name_and_arguments() {
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(0, "tool-call"),
            // The name and id arrive complete with the start event; only the arguments stream, and
            // the adapter fills their deltas with a placeholder id. That placeholder must not
            // replace the real one, which is what the call is dispatched and matched by.
            tool(0, "call-1", Some("read_file"), ""),
            tool(0, "invalid", None, r#"{"path":"#),
            tool(0, "invalid", None, r#" "/tmp/a"}"#),
            // A closing tool-call block with neither name nor arguments is the placeholder shape:
            // the content lives in the deltas, so taking it verbatim would drop the arguments.
            end(
                0,
                ContentBlock::ToolCall {
                    id: ToolCallId::new("call-1").expect("valid"),
                    name: String::new(),
                    arguments: String::new(),
                },
            ),
        ],
    );

    let blocks = assembler.finish().expect("closed");
    let ContentBlock::ToolCall {
        id,
        name,
        arguments,
    } = &blocks[0]
    else {
        panic!("expected a tool call, got {:?}", blocks[0]);
    };
    assert_eq!(id.as_str(), "call-1");
    assert_eq!(name, "read_file");
    assert_eq!(arguments, r#"{"path": "/tmp/a"}"#);
}

#[test]
fn a_tool_call_that_never_closes_is_reported_as_unclosed() {
    // This project's adapter emits `block-end` for text and reasoning only, so a streamed tool
    // call can arrive as `block-start` plus arguments and nothing else. `finish` refuses it on
    // purpose: a caller that wants a truncated stream reported, not silently half-assembled,
    // relies on this error.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![start(0, "tool-call"), tool(0, "c1", Some("list"), "{}")],
    );

    assert_eq!(
        assembler.finish().expect_err("open"),
        AssemblyError::Unclosed { count: 1 }
    );
}

#[test]
fn a_tool_call_that_closes_assembles_from_name_and_arguments() {
    // The negative case above holds only while no close arrives: a closed tool call is real
    // content even when the closing block carries neither a name nor arguments.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(0, "tool-call"),
            tool(0, "c1", Some("list"), r#"{"path":"/"}"#),
            end(
                0,
                ContentBlock::ToolCall {
                    id: ToolCallId::new("c1").expect("valid"),
                    name: String::new(),
                    arguments: String::new(),
                },
            ),
        ],
    );

    let blocks = assembler.finish().expect("closed");
    assert_eq!(
        blocks[0],
        ContentBlock::ToolCall {
            id: ToolCallId::new("c1").expect("valid"),
            name: "list".to_owned(),
            arguments: r#"{"path":"/"}"#.to_owned(),
        }
    );
}

#[test]
fn an_interleaved_tool_call_and_text_block_stay_separate() {
    // The failure mode a single-slot assembler has on a real tool-calling turn: text and the
    // tool call alternate, and each must keep its own content.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(0, "text"),
            text(0, "Let me look"),
            start(1, "tool-call"),
            tool(1, "call-9", Some("list"), r#"{"pa"#),
            text(0, " that up."),
            tool(1, "invalid", None, r#"th":"/"}"#),
            placeholder_text(0),
            end(
                1,
                ContentBlock::ToolCall {
                    id: ToolCallId::new("call-9").expect("valid"),
                    name: String::new(),
                    arguments: String::new(),
                },
            ),
        ],
    );

    let blocks = assembler.finish().expect("both closed");
    assert_eq!(blocks[0], visible("Let me look that up."));
    let ContentBlock::ToolCall {
        id,
        name,
        arguments,
    } = &blocks[1]
    else {
        panic!("expected a tool call, got {:?}", blocks[1]);
    };
    assert_eq!(id.as_str(), "call-9");
    assert_eq!(name, "list");
    assert_eq!(arguments, r#"{"path":"/"}"#);
}

#[test]
fn a_tool_call_with_no_name_or_arguments_produces_no_block() {
    // A start with nothing behind it is not an invocation; emitting it would put an uncallable
    // block in the message.
    let mut assembler = Assembler::new();
    feed(&mut assembler, vec![start(0, "tool-call")]);

    assert_eq!(
        assembler.finish().expect_err("open"),
        AssemblyError::Unclosed { count: 1 }
    );
}

// ---- stragglers ---------------------------------------------------------------------------------

#[test]
fn a_delta_after_its_block_closed_is_ignored() {
    // A provider can trail a stray delta past `block-end`. Accepting it would rewrite content the
    // caller has already been handed, so closing is one-way.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(0, "text"),
            text(0, "done"),
            placeholder_text(0),
            text(0, "late"),
        ],
    );

    assert_eq!(assembler.finish().expect("closed"), vec![visible("done")]);
}

#[test]
fn a_delta_after_take_finished_is_ignored() {
    // The block is gone, but the index is remembered as seen: a trailing delta is the same
    // malformed-stream case, not a resurrected block.
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![start(0, "text"), text(0, "done"), placeholder_text(0)],
    );
    assert_eq!(assembler.take_finished(), vec![visible("done")]);

    assembler
        .push(text(0, "late"))
        .expect("a straggler for a taken block is ignored, not an error");
    assert_eq!(assembler.finish().expect("closed"), Vec::new());
}

#[test]
fn a_second_block_end_does_not_replace_the_first() {
    let mut assembler = Assembler::new();
    feed(
        &mut assembler,
        vec![
            start(0, "text"),
            end(0, visible("first")),
            end(0, visible("second")),
        ],
    );

    assert_eq!(assembler.finish().expect("closed"), vec![visible("first")]);
}

#[test]
fn a_block_end_without_a_start_is_accepted() {
    // A whole block delivered in one chunk is complete on its own; there is nothing to accumulate
    // and no ambiguity about what it says.
    let mut assembler = Assembler::new();
    assembler
        .push(end(0, visible("whole")))
        .expect("a complete block needs no deltas");

    assert!(!assembler.has_open_blocks());
    assert_eq!(assembler.finish().expect("closed"), vec![visible("whole")]);
}
