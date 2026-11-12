//! Tests for stream statistics.
//!
//! The property that matters is that "alive but silent" is distinguishable from "no output at all"
//! and from "produced reasoning only": every case below reproduces one of those shapes with the
//! smallest possible stream.

use girder_core::{
    ContentBlock, FinishReason, LlmStats as Stats, StreamChunk, TokenUsage, ToolCallId,
};

fn block_start(index: usize, block_type: &str) -> StreamChunk {
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

fn tool_call(index: usize, name: &str, arguments: &str) -> StreamChunk {
    StreamChunk::ToolCallDelta {
        index,
        id: ToolCallId::new("call-1").expect("valid call id"),
        name: Some(name.to_owned()),
        arguments_delta: arguments.to_owned(),
    }
}

fn usage() -> StreamChunk {
    StreamChunk::Usage(TokenUsage {
        input: 10,
        output: 3,
        cached: 0,
    })
}

fn finish() -> StreamChunk {
    StreamChunk::Finish {
        reason: FinishReason::Stop,
    }
}

fn block_end(index: usize) -> StreamChunk {
    StreamChunk::BlockEnd {
        index,
        block: ContentBlock::Text {
            text: "done".to_owned(),
        },
    }
}

#[test]
fn empty_stream_reports_nothing() {
    let stats = Stats::from_chunks(&[]);

    assert_eq!(stats.first_chunk_ms, None);
    assert_eq!(stats.first_text_ms, None);
    assert!(!stats.has_visible_text);
    assert_eq!(stats.visible_chars, 0);
    assert!(!stats.has_reasoning);
    assert_eq!(stats.chunks, 0);
}

#[test]
fn block_start_only_is_alive_but_silent() {
    let stats = Stats::from_chunks(&[(40, block_start(0, "text"))]);

    assert_eq!(stats.first_chunk_ms, Some(40));
    assert_eq!(stats.first_text_ms, None);
    assert!(!stats.has_visible_text);
    assert_eq!(stats.visible_chars, 0);
    assert_eq!(stats.chunks, 1);
}

#[test]
fn text_delta_reports_text_time_at_or_after_first_chunk() {
    let stats = Stats::from_chunks(&[
        (0, block_start(0, "text")),
        (120, text(0, "hello")),
        (130, text(0, " world")),
        (140, usage()),
        (150, finish()),
    ]);

    assert_eq!(stats.first_chunk_ms, Some(0));
    assert_eq!(stats.first_text_ms, Some(120));
    assert!(stats.has_visible_text);
    assert_eq!(stats.visible_chars, 11);
    assert_eq!(stats.chunks, 5);

    let first_chunk = stats.first_chunk_ms.expect("chunk arrived");
    let first_text = stats.first_text_ms.expect("text arrived");
    assert!(first_text >= first_chunk);
}

#[test]
fn first_chunk_can_precede_first_text_by_a_long_gap() {
    // The shape this module exists for: the transport answered, then nothing for five seconds.
    let stats = Stats::from_chunks(&[(5, block_start(0, "text")), (5_000, text(0, "late"))]);

    assert_eq!(stats.first_chunk_ms, Some(5));
    assert_eq!(stats.first_text_ms, Some(5_000));
    assert!(stats.has_visible_text);
}

#[test]
fn reasoning_only_is_not_visible_text() {
    let stats = Stats::from_chunks(&[
        (0, block_start(0, "reasoning")),
        (10, reasoning(0, "let me think")),
        (20, reasoning(0, " about it")),
        (30, block_end(0)),
    ]);

    assert!(stats.has_reasoning);
    assert!(!stats.has_visible_text);
    assert_eq!(stats.first_text_ms, None);
    assert_eq!(stats.visible_chars, 0);
    assert_eq!(stats.first_chunk_ms, Some(0));
    assert_eq!(stats.chunks, 4);
}

#[test]
fn mixed_stream_counts_only_visible_text() {
    let stats = Stats::from_chunks(&[
        (0, block_start(0, "reasoning")),
        (10, reasoning(0, "planning")),
        (20, block_end(0)),
        (30, block_start(1, "text")),
        (60, text(1, "answer")),
        (70, tool_call(2, "read", "{\"path\":\"a\"}")),
        (80, usage()),
        (90, finish()),
    ]);

    assert_eq!(stats.first_chunk_ms, Some(0));
    assert_eq!(stats.first_text_ms, Some(60));
    assert!(stats.has_visible_text);
    assert!(stats.has_reasoning);
    assert_eq!(stats.visible_chars, 6);
    assert_eq!(stats.chunks, 8);
}

#[test]
fn usage_and_finish_are_not_visible_text() {
    let stats = Stats::from_chunks(&[
        (0, block_start(0, "tool-call")),
        (10, tool_call(0, "read", "{}")),
        (20, block_end(0)),
        (30, usage()),
        (40, finish()),
    ]);

    assert_eq!(stats.first_chunk_ms, Some(0));
    assert_eq!(stats.first_text_ms, None);
    assert!(!stats.has_visible_text);
    assert!(!stats.has_reasoning);
    assert_eq!(stats.chunks, 5);
}

#[test]
fn assembled_block_end_does_not_double_count_streamed_text() {
    let stats = Stats::from_chunks(&[
        (0, block_start(0, "text")),
        (10, text(0, "hi")),
        (20, block_end(0)),
    ]);

    // `block_end` carries the assembled "done" text, but that is not a new delta.
    assert_eq!(stats.visible_chars, 2);
    assert_eq!(stats.first_text_ms, Some(10));
}

#[test]
fn empty_text_delta_is_not_visible_text() {
    let stats = Stats::from_chunks(&[(0, block_start(0, "text")), (10, text(0, ""))]);

    assert!(!stats.has_visible_text);
    assert_eq!(stats.first_text_ms, None);
    assert_eq!(stats.visible_chars, 0);
    assert_eq!(stats.first_chunk_ms, Some(0));
}

#[test]
fn visible_chars_counts_characters_not_bytes() {
    let stats = Stats::from_chunks(&[
        (0, block_start(0, "text")),
        (10, text(0, "héllo")),
        (20, text(0, " 世界")),
    ]);

    // 5 + 3 characters; the same text is 14 bytes.
    assert_eq!(stats.visible_chars, 8);
    assert!(stats.has_visible_text);
    assert_eq!(stats.first_text_ms, Some(10));
}

#[test]
fn whitespace_only_text_still_counts_as_text() {
    // A provider that streams only whitespace did emit text; whether that is *useful* is a policy
    // question for the caller, so it is reported rather than hidden here.
    let stats = Stats::from_chunks(&[(0, block_start(0, "text")), (10, text(0, " "))]);

    assert!(stats.has_visible_text);
    assert_eq!(stats.visible_chars, 1);
    assert_eq!(stats.first_text_ms, Some(10));
}
