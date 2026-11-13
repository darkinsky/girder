//! Tests for the incremental observer that sits between the block assembler and a caller.
//!
//! The test that matters most is `assemble_message_agrees_with_from_chunks`: it runs the same chunk
//! stream through both implementations and requires the same blocks, in the same order. Two
//! implementations of one contract that are never compared are two contracts; a failure there is the
//! only signal that the adapter is not a drop-in replacement.
//!
//! The remaining tests fall into two groups. Incremental delivery — what closes, and on which chunk
//! — is what the assembler exists for, so it is asserted chunk by chunk rather than only on the
//! final result. The documented differences from `from_chunks` each get a test, so that the
//! divergence stays a decision instead of drifting into a bug.
//!
//! # Why the module is compiled by path
//!
//! `stream_blocks` is not declared in `lib.rs` yet; wiring it up is a separate change to a file this
//! test must not touch. Compiling the exact source file here keeps these tests runnable now and
//! still exercises the same code a consumer will reach through the crate root. Nothing is
//! duplicated: this is the real file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/stream_blocks.rs"]
mod stream_blocks;

use girder_core::{
    AssistantMessage, ContentBlock, FinishReason, StreamChunk, TokenUsage, ToolCallId,
};
use stream_blocks::{StreamObserver, assemble_message};

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
fn placeholder(index: usize) -> StreamChunk {
    end(
        index,
        ContentBlock::Text {
            text: String::new(),
        },
    )
}

/// An id the adapter emits on argument-only deltas; it is ignored because the first id wins.
fn placeholder_id() -> ToolCallId {
    ToolCallId::new("invalid").expect("constant is valid")
}

fn finish() -> StreamChunk {
    StreamChunk::Finish {
        reason: FinishReason::Stop,
    }
}

fn visible(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

fn think(text: &str) -> ContentBlock {
    ContentBlock::Reasoning {
        text: text.to_owned(),
    }
}

fn call(id: &str, name: &str, arguments: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        id: ToolCallId::new(id).expect("valid test id"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

/// Runs `chunks` through the observer, asserting that both readers agree.
///
/// The whole point of this module: a disagreement means one of the two implementations is wrong.
fn assert_agrees(chunks: &[StreamChunk]) {
    let reference = AssistantMessage::from_chunks(chunks.to_owned())
        .expect("the reference assembles this stream");
    let assembled = assemble_message(chunks).expect("the assembler accepts the same stream");
    assert_eq!(
        assembled, reference.content,
        "assemble_message and from_chunks disagree on {chunks:#?}"
    );
}

#[test]
fn a_block_is_delivered_on_the_chunk_that_closes_it() {
    let mut observer = StreamObserver::new();
    assert!(observer.observe(&start(0, "text")).is_empty());
    assert!(observer.observe(&text(0, "hel")).is_empty());
    assert!(observer.observe(&text(0, "lo")).is_empty());
    assert_eq!(observer.completed(), 0, "an open block is not delivered");

    // The `block-end` is what delivers; nothing waits for the end of the stream.
    assert_eq!(observer.observe(&placeholder(0)), vec![visible("hello")]);
    assert_eq!(observer.completed(), 1);
    assert_eq!(observer.observe(&finish()), Vec::new());
}

#[test]
fn assemble_message_agrees_with_from_chunks() {
    // Text and reasoning, both with the empty placeholder `block-end` this project's adapter sends.
    assert_agrees(&[
        start(0, "text"),
        text(0, "hello"),
        placeholder(0),
        start(1, "reasoning"),
        reasoning(1, "thinking"),
        placeholder(1),
        finish(),
    ]);

    // A `block-end` that carries the content itself, as other providers send it.
    assert_agrees(&[
        start(0, "text"),
        text(0, "ign"),
        end(0, visible("done")),
        finish(),
    ]);
    assert_agrees(&[end(0, visible("whole block, no start")), finish()]);
    assert_agrees(&[start(0, "text"), placeholder(0), finish()]);
    assert_agrees(&[finish()]);

    // A tool call whose arguments stream in pieces.
    assert_agrees(&[
        start(0, "tool-call"),
        tool(0, "call_1", Some("read"), ""),
        tool(0, "invalid", None, r#"{"path":"#),
        tool(0, "invalid", None, r#""a.rs"}"#),
        finish(),
    ]);

    // Text followed by a tool call, with accounting interleaved.
    assert_agrees(&[
        StreamChunk::Usage(TokenUsage {
            input: 10,
            output: 4,
            cached: 0,
        }),
        start(0, "text"),
        text(0, "Reading the file."),
        placeholder(0),
        start(1, "tool-call"),
        tool(1, "call_2", Some("read"), r#"{"path":"a.rs"}"#),
        StreamChunk::Usage(TokenUsage {
            input: 10,
            output: 9,
            cached: 2,
        }),
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        },
    ]);

    // A closing block that carries real content of the same kind the deltas also streamed: the
    // closing block wins in both, so the deltas are not folded in twice.
    assert_agrees(&[
        start(0, "reasoning"),
        reasoning(0, "streamed "),
        end(0, think("settled")),
        finish(),
    ]);
    assert_agrees(&[
        start(0, "text"),
        text(0, "streamed"),
        end(0, visible("settled")),
        finish(),
    ]);

    // An attachment reference is real content even though it is small, and arrives whole.
    assert_agrees(&[
        start(0, "image"),
        end(
            0,
            ContentBlock::Image {
                attachment: "att_1".to_owned(),
            },
        ),
        start(1, "file"),
        end(
            1,
            ContentBlock::File {
                attachment: "att_2".to_owned(),
            },
        ),
        finish(),
    ]);

    // Empty deltas and an empty announced block change nothing on either side.
    assert_agrees(&[
        start(0, "text"),
        text(0, ""),
        text(0, "body"),
        text(0, ""),
        placeholder(0),
        finish(),
    ]);

    // A tool call whose arguments stream in one delta and never repeats the name.
    assert_agrees(&[
        start(0, "tool-call"),
        tool(0, "call_3", Some("bash"), ""),
        tool(0, "invalid", None, r#"{"command":"ls"}"#),
        finish(),
    ]);
}

#[test]
fn interleaved_blocks_deliver_independently_and_stay_in_index_order() {
    // Two blocks streaming concurrently: the higher index closes first, which is legal on the wire.
    let chunks = vec![
        start(0, "text"),
        start(1, "text"),
        text(0, "a"),
        text(1, "b"),
        text(0, "c"),
        placeholder(1),
        placeholder(0),
        finish(),
    ];

    let mut observer = StreamObserver::new();
    assert!(observer.observe(&chunks[0]).is_empty());
    assert!(observer.observe(&chunks[1]).is_empty());
    assert!(observer.observe(&chunks[2]).is_empty());
    assert!(observer.observe(&chunks[3]).is_empty());
    assert!(observer.observe(&chunks[4]).is_empty());
    // Closing block 1 delivers block 1 alone: block 0 is still open and is not waited for.
    assert_eq!(observer.observe(&chunks[5]), vec![visible("b")]);
    assert_eq!(observer.observe(&chunks[6]), vec![visible("ac")]);
    assert_eq!(observer.completed(), 2);

    // The folded result is in index order regardless of the order the blocks closed in.
    assert_eq!(
        assemble_message(&chunks).expect("assembles"),
        vec![visible("ac"), visible("b")]
    );
    assert_agrees(&chunks);
}

#[test]
fn a_delta_that_trails_its_closed_block_is_ignored() {
    let chunks = vec![
        start(0, "text"),
        text(0, "a"),
        placeholder(0),
        text(0, "late"),
        finish(),
    ];
    assert_eq!(
        assemble_message(&chunks).expect("assembles"),
        vec![visible("a")],
        "content already handed out must not be rewritten"
    );

    let mut observer = StreamObserver::new();
    for chunk in &chunks {
        observer.observe(chunk);
    }
    assert_eq!(observer.completed(), 1, "the block is delivered once");
    assert_eq!(observer.finish().expect("assembles"), Vec::new());
}

#[test]
fn a_second_block_end_does_not_rewrite_a_delivered_block() {
    // Documented difference: `from_chunks` keeps the last closing block, the assembler the first.
    // First-close-wins is correct for an incremental caller, which may already have rendered it.
    let chunks = vec![
        start(0, "text"),
        text(0, "a"),
        end(0, visible("first")),
        end(0, visible("second")),
        finish(),
    ];
    assert_eq!(
        assemble_message(&chunks).expect("assembles"),
        vec![visible("first")]
    );
    assert_eq!(
        AssistantMessage::from_chunks(chunks)
            .expect("assembles")
            .content,
        vec![visible("second")],
        "the reference keeps the last close; this difference is documented"
    );
}

#[test]
fn orphan_delta_is_reported_where_from_chunks_is_inconsistent() {
    // Documented difference: a delta for an index that never started means the stream lost a
    // `block-start`. `from_chunks` is inconsistent about it, which is the worse answer either way.
    let chunks = vec![text(3, "no start"), finish()];
    assert_eq!(
        assemble_message(&chunks),
        Err(girder_core::AssemblyError::OrphanDelta { index: 3 })
    );
    assert_eq!(
        AssistantMessage::from_chunks(chunks)
            .expect("accepts it")
            .content,
        Vec::new(),
        "the reference silently drops an orphan text delta: data loss with no signal"
    );

    // The same input with a tool-call delta instead grows the slot, so the reference invents a
    // block at the orphan index. The two inputs mean the same thing and are treated differently.
    let chunks = vec![tool(3, "call_1", Some("read"), "{}"), finish()];
    assert_eq!(
        assemble_message(&chunks),
        Err(girder_core::AssemblyError::OrphanDelta { index: 3 })
    );
    assert_eq!(
        AssistantMessage::from_chunks(chunks)
            .expect("accepts it")
            .content,
        vec![call("call_1", "read", "{}")],
        "the reference promotes an orphan tool-call delta: an invented block"
    );
}

#[test]
fn a_duplicate_start_is_reported_where_from_chunks_merges() {
    // Documented difference: two blocks merged into one index cannot be separated afterwards.
    let chunks = vec![
        start(0, "text"),
        text(0, "a"),
        start(0, "text"),
        text(0, "b"),
        finish(),
    ];

    assert_eq!(
        assemble_message(&chunks),
        Err(girder_core::AssemblyError::DuplicateStart { index: 0 })
    );
    assert_eq!(
        AssistantMessage::from_chunks(chunks)
            .expect("merges")
            .content,
        vec![visible("ab")],
        "the reference merges the two blocks; this difference is documented"
    );
}

#[test]
fn the_announced_kind_wins_over_the_delta_variant() {
    // Documented difference: a stream that announces one kind and streams the other is malformed,
    // and the two implementations resolve it in opposite directions.
    let chunks = vec![start(0, "reasoning"), text(0, "spoken"), finish()];
    assert_eq!(
        assemble_message(&chunks).expect("assembles"),
        vec![think("spoken")],
        "`block-start` is the only chunk that states what a block is"
    );
    assert_eq!(
        AssistantMessage::from_chunks(chunks)
            .expect("assembles")
            .content,
        vec![visible("spoken")]
    );

    let chunks = vec![start(0, "text"), reasoning(0, "silent"), finish()];
    assert_eq!(
        assemble_message(&chunks).expect("assembles"),
        vec![visible("silent")]
    );
    assert_eq!(
        AssistantMessage::from_chunks(chunks)
            .expect("assembles")
            .content,
        vec![think("silent")]
    );
}

#[test]
fn a_placeholder_tool_call_block_end_falls_back_to_the_deltas() {
    // Documented difference: an uncallable closing block must not replace the call the model made.
    let chunks = vec![
        start(0, "tool-call"),
        tool(0, "call_1", Some("read"), r#"{"path":"a.rs"}"#),
        end(0, call("call_1", "", "")),
        finish(),
    ];

    assert_eq!(
        assemble_message(&chunks).expect("assembles"),
        vec![call("call_1", "read", r#"{"path":"a.rs"}"#)]
    );
    assert_eq!(
        AssistantMessage::from_chunks(chunks)
            .expect("assembles")
            .content,
        vec![call("call_1", "", "")],
        "the reference keeps the uncallable block; this difference is documented"
    );
}

#[test]
fn a_tool_call_left_open_by_the_provider_is_closed_at_finish() {
    // What this project's own adapter emits: `content_block_stop` produces a `block-end` for text
    // and reasoning only, never for a tool call, because the arguments are complete by then.
    let chunks = vec![
        start(0, "text"),
        text(0, "Reading it."),
        placeholder(0),
        start(1, "tool-call"),
        tool(1, "call_9", Some("read"), ""),
        tool(1, "invalid", None, r#"{"path":"#),
        tool(1, "invalid", None, r#""a.rs"}"#),
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];

    let mut observer = StreamObserver::new();
    let mut delivered = Vec::new();
    for chunk in &chunks {
        delivered.extend(observer.observe(chunk));
    }
    assert_eq!(delivered, vec![visible("Reading it.")]);
    // `Finish` is the evidence the response ended, so the open block is complete, not truncated.
    assert_eq!(
        observer
            .finish()
            .expect("a finished stream is not truncated"),
        vec![call("call_9", "read", r#"{"path":"a.rs"}"#)]
    );
    assert_agrees(&chunks);
}

#[test]
fn a_stream_without_finish_and_with_an_open_block_is_reported_as_unclosed() {
    // Documented difference: `from_chunks` reports a missing `finish`; this module, which only
    // assembles blocks, reports the half-built block that the missing terminator left behind.
    let chunks = vec![start(0, "text"), text(0, "half")];
    assert_eq!(
        assemble_message(&chunks),
        Err(girder_core::AssemblyError::Unclosed { count: 1 })
    );

    let mut observer = StreamObserver::new();
    for chunk in &chunks {
        observer.observe(chunk);
    }
    assert_eq!(
        observer.finish(),
        Err(girder_core::AssemblyError::Unclosed { count: 1 })
    );
}

#[test]
fn an_empty_stream_assembles_to_no_blocks() {
    assert_eq!(assemble_message(&[]), Ok(Vec::new()));

    let observer = StreamObserver::new();
    assert_eq!(observer.completed(), 0);
    assert_eq!(observer.finish(), Ok(Vec::new()));

    // The reference requires a terminal chunk, so a truly empty stream is an error there. That is
    // an accounting question this module does not answer; see the module docs.
    assert!(AssistantMessage::from_chunks(Vec::new()).is_err());
    assert_agrees(&[finish()]);
}

#[test]
fn a_tool_call_block_is_delivered_when_its_block_end_arrives() {
    let mut observer = StreamObserver::new();
    assert!(observer.observe(&start(0, "tool-call")).is_empty());
    assert!(
        observer
            .observe(&tool(0, "call_7", Some("read"), ""))
            .is_empty()
    );
    assert!(
        observer
            .observe(&tool(0, "invalid", None, r#"{"path":"#))
            .is_empty()
    );
    assert_eq!(
        observer
            .observe(&tool(0, "invalid", None, r#""a.rs"}"#))
            .len(),
        0,
        "deltas alone do not close a block"
    );
    assert_eq!(
        observer.observe(&end(0, call("call_7", "read", r#"{"path":"a.rs"}"#))),
        vec![call("call_7", "read", r#"{"path":"a.rs"}"#)]
    );
    assert_eq!(observer.completed(), 1);
    assert_eq!(observer.finish().expect("nothing left"), Vec::new());
}

#[test]
fn a_tool_call_without_an_id_gets_an_index_derived_one_in_both_readers() {
    // No `block-start` id reaches the assembler through a delta-only tool call, so the id is
    // synthesized from the index. Both readers must synthesize the same one, or a call could be
    // matched with the wrong result.
    let chunks = vec![
        start(2, "tool-call"),
        tool(2, "call_provided", Some("read"), "{}"),
        finish(),
    ];
    assert_eq!(
        assemble_message(&chunks).expect("assembles"),
        vec![call("call_provided", "read", "{}")]
    );
    assert_agrees(&chunks);
}

#[test]
fn blocks_delivered_by_observe_are_not_repeated_by_finish() {
    let mut observer = StreamObserver::new();
    observer.observe(&start(0, "text"));
    observer.observe(&text(0, "one"));
    assert_eq!(observer.observe(&placeholder(0)), vec![visible("one")]);

    observer.observe(&start(1, "text"));
    observer.observe(&text(1, "two"));
    assert_eq!(observer.completed(), 1);

    // Block 1 is still open with no `finish`, so its content is not handed out at all.
    assert_eq!(
        observer.finish(),
        Err(girder_core::AssemblyError::Unclosed { count: 1 })
    );
}

#[test]
fn a_usage_chunk_never_completes_a_block() {
    let mut observer = StreamObserver::new();
    observer.observe(&start(0, "text"));
    assert!(
        observer
            .observe(&StreamChunk::Usage(TokenUsage {
                input: 1,
                output: 1,
                cached: 0,
            }))
            .is_empty()
    );
    assert_eq!(observer.completed(), 0);
}

#[test]
fn an_unmatched_placeholder_id_is_ignored_by_the_first_id_rule() {
    let chunks = vec![
        start(0, "tool-call"),
        tool(0, "call_real", Some("write"), ""),
        tool(0, "invalid", None, "{}"),
        finish(),
    ];
    assert_eq!(
        assemble_message(&chunks).expect("assembles"),
        vec![call("call_real", "write", "{}")],
        "an id that arrives first must not be replaced by a later placeholder"
    );
    assert_eq!(placeholder_id().as_str(), "invalid");
    assert_agrees(&chunks);
}
