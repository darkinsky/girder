//! Tests for one model attempt: framing, the compact durable stream, and the reconnect fold.
//!
//! The reference vectors come from three upstream suites, which is what makes this a port rather
//! than an interpretation:
//!
//! - `packages/llm/llm/tests/assistant-stream.spec.ts` — accumulator packing, run boundaries, the
//!   lossless round trip, the readers, and the validation refusals.
//! - `packages/api/session-controller/tests/assistant-stream.host.spec.ts` — the nineteen-frame
//!   sequence that exercises replacement, a revision gap, and baseline caching, reproduced frame for
//!   frame.
//! - `packages/llm/llm/tests/assembler.spec.ts` — the split between assembly and assembly-plus-entry
//!   pruning, which the local assembler resolves by not carrying replay metadata at all.
//!
//! # Why the module is compiled by path
//!
//! `girder-core`'s crate root does not declare `llm_attempt_gap`; adding `pub mod llm_attempt_gap;`
//! and its `pub use` belongs to the integrator. The module is therefore compiled into this test
//! binary directly. It touches the crate root only through `girder_core::…`, which is how the same
//! source builds in both places.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "tests assert by panicking; a failed unwrap is a failed test, and one long test is \
              clearer than four that share mutable setup"
)]

#[path = "../src/llm/attempt.rs"]
pub mod llm_attempt_gap;

use girder_core::{
    AssemblyError, ContentBlock, FinishReason, MIN_SAFE_INTEGER, SessionId, StreamChunk,
    TokenUsage, ToolCallId,
};

use llm_attempt_gap::{
    Accumulator, Attempt, AttemptId, AttemptView, Baseline, Frame, Outcome, Revision, Settlement,
    StreamError, StreamFold, StreamRecord, StreamRun, assemble_stream, chunk_has_visible_text,
    expand, first_token_time, has_visible_content, has_visible_text, is_token_delta,
    is_visible_chunk, join_text, last_finish, raw_chunks, run_first_token_time,
    run_first_visible_time,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// The largest timestamp the format carries, spelled without a lossy cast.
fn max_safe_time() -> i64 {
    i64::try_from(girder_core::MAX_SAFE_INTEGER).unwrap()
}

fn chunk(index: usize, text: &str) -> StreamChunk {
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
        id: ToolCallId::new(id).unwrap(),
        name: name.map(str::to_owned),
        arguments_delta: arguments.to_owned(),
    }
}

fn start(index: usize, block_type: &str) -> StreamChunk {
    StreamChunk::BlockStart {
        index,
        block_type: block_type.to_owned(),
    }
}

fn end(index: usize, block: ContentBlock) -> StreamChunk {
    StreamChunk::BlockEnd { index, block }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

fn finish(reason: FinishReason) -> StreamChunk {
    StreamChunk::Finish { reason }
}

// ---- accumulator: packing ---------------------------------------------------------------------

/// Upstream's first case, verbatim: two text deltas and two tool-call deltas pack into two runs
/// separated by the raw `finish` record.
#[test]
fn packs_delta_runs_and_reproduces_the_original_sequence() {
    let mut accumulator = Accumulator::new();
    let timed = [
        (1_000, chunk(0, "hel")),
        (1_006, chunk(0, "lo")),
        (1_008, tool(1, "call-1", Some("run_code"), "{")),
        (1_011, tool(1, "call-1", Some("run_code"), "}")),
        (1_020, finish(FinishReason::Stop)),
    ];
    for (time, chunk) in timed.clone() {
        accumulator.push(time, chunk).unwrap();
    }

    assert_eq!(
        accumulator.snapshot(),
        vec![
            StreamRecord::TextChunks {
                time0: 1_000,
                index: 0,
                dt: vec![6],
                texts: vec!["hel".to_owned(), "lo".to_owned()],
            },
            StreamRecord::ToolCallChunks {
                time0: 1_008,
                index: 1,
                dt: vec![3],
                id: ToolCallId::new("call-1").unwrap(),
                name: Some("run_code".to_owned()),
                args: vec!["{".to_owned(), "}".to_owned()],
            },
            StreamRecord::Chunk {
                time: 1_020,
                chunk: finish(FinishReason::Stop),
            },
        ]
    );

    let expanded = expand(&accumulator.snapshot()).unwrap();
    let expected: Vec<_> = timed
        .into_iter()
        .map(|(time, chunk)| llm_attempt_gap::TimedChunk { time, chunk })
        .collect();
    assert_eq!(expanded, expected);
}

/// Snapshotting is detached: a later push does not rewrite a stream a caller already holds, which is
/// what makes the value safe to hand to a durable append.
#[test]
fn snapshots_detach_from_later_pushes() {
    let mut accumulator = Accumulator::new();
    accumulator.push(10, chunk(0, "a")).unwrap();
    accumulator
        .push(
            11,
            StreamChunk::Usage(TokenUsage {
                input: 3,
                output: 2,
                cached: 0,
            }),
        )
        .unwrap();

    let first = accumulator.snapshot();
    accumulator.push(12, chunk(0, "b")).unwrap();
    let second = accumulator.snapshot();

    assert_eq!(expand(&first).unwrap().len(), 2);
    assert_eq!(expand(&second).unwrap().len(), 3);
    // The first snapshot did not grow with the accumulator.
    assert_eq!(
        first[0],
        StreamRecord::TextChunks {
            time0: 10,
            index: 0,
            dt: vec![],
            texts: vec!["a".to_owned()],
        }
    );
}

/// Upstream's second case: a run continues only on the same kind, the same index, and an
/// exactly-representable gap. Every incompatible neighbour starts a new record, and the expanded
/// sequence is still identical to the input.
#[test]
fn keeps_incompatible_runs_separate() {
    let mut accumulator = Accumulator::new();
    let timed = vec![
        (1, reasoning(0, "r1")),
        (2, reasoning(0, "r2")),
        (3, chunk(0, "a")),
        (4, chunk(1, "b")),
        (5, tool(0, "one", None, "{")),
        (6, tool(0, "one", None, "}")),
        (7, tool(0, "one", Some("read"), "")),
        (8, tool(1, "two", Some("read"), "x")),
        (max_safe_time(), chunk(1, "far")),
        (MIN_SAFE_INTEGER, chunk(1, "apart")),
    ];
    for (time, chunk) in timed.clone() {
        accumulator.push(time, chunk).unwrap();
    }

    let stream = accumulator.snapshot();
    assert_eq!(
        stream
            .iter()
            .map(StreamRecord::type_name)
            .collect::<Vec<_>>(),
        vec![
            "reasoning-chunks",
            "text-chunks",
            "text-chunks",
            "tool-call-chunks",
            "tool-call-chunks",
            "tool-call-chunks",
            "text-chunks",
            "text-chunks",
        ]
    );

    // A name's *presence* is part of the identity: the nameless run and the named run are separate
    // even though both are the same call, because expanding one record would repeat one name on
    // every member.
    let expanded = expand(&stream).unwrap();
    let reproduced: Vec<_> = expanded
        .iter()
        .map(|timed| (timed.time, timed.chunk.clone()))
        .collect();
    assert_eq!(reproduced, timed);
}

/// An empty `name` is stored raw rather than packed, so expansion cannot invent a name it never had.
#[test]
fn stores_an_empty_tool_name_raw() {
    let mut accumulator = Accumulator::new();
    accumulator
        .push(1, tool(0, "call", Some(""), "{}"))
        .unwrap();
    assert_eq!(accumulator.snapshot().len(), 1);
    assert!(matches!(
        accumulator.snapshot()[0],
        StreamRecord::Chunk {
            chunk: StreamChunk::ToolCallDelta { .. },
            ..
        }
    ));
}

/// A gap that cannot be represented exactly starts a new run instead of storing a wrong number.
#[test]
fn starts_a_new_run_when_the_gap_cannot_be_represented() {
    let mut accumulator = Accumulator::new();
    accumulator.push(max_safe_time(), chunk(0, "a")).unwrap();
    accumulator.push(MIN_SAFE_INTEGER, chunk(0, "b")).unwrap();

    let stream = accumulator.snapshot();
    assert_eq!(stream.len(), 2);
    assert_eq!(
        expand(&stream).unwrap().len(),
        2,
        "both fragments survive as separate records"
    );
}

// ---- accumulator: refusals --------------------------------------------------------------------

#[test]
fn refuses_unsafe_times_indices_and_usage() {
    let mut accumulator = Accumulator::new();
    assert_eq!(
        accumulator
            .push(max_safe_time() + 1, finish(FinishReason::Stop))
            .unwrap_err(),
        StreamError::UnsafeTime {
            time: max_safe_time() + 1
        }
    );
    assert_eq!(
        accumulator
            .push(i64::MIN, finish(FinishReason::Stop))
            .unwrap_err(),
        StreamError::UnsafeTime { time: i64::MIN }
    );
    assert!(matches!(
        accumulator
            .push(
                1,
                StreamChunk::Usage(TokenUsage {
                    input: girder_core::MAX_SAFE_INTEGER + 1,
                    output: 0,
                    cached: 0,
                })
            )
            .unwrap_err(),
        StreamError::UnsafeUsage { field: "input", .. }
    ));
    // Nothing was recorded by a refused push.
    assert!(accumulator.is_empty());
}

// ---- accumulator: bookkeeping ----------------------------------------------------------------

#[test]
fn counts_chunks_rather_than_records() {
    let mut accumulator = Accumulator::new();
    accumulator.push(1, chunk(0, "a")).unwrap();
    accumulator.push(2, chunk(0, "b")).unwrap();
    accumulator.push(3, finish(FinishReason::Stop)).unwrap();
    assert_eq!(accumulator.len(), 3);
    assert_eq!(accumulator.snapshot().len(), 2);
    assert!(!accumulator.is_empty());
}

// ---- expansion validation --------------------------------------------------------------------

#[test]
fn refuses_malformed_runs() {
    let id = ToolCallId::new("c").unwrap();
    assert_eq!(
        expand(&[StreamRecord::TextChunks {
            time0: 1,
            index: 0,
            dt: vec![],
            texts: vec![],
        }])
        .unwrap_err(),
        StreamError::EmptyRun {
            label: "text-chunks"
        }
    );
    assert_eq!(
        expand(&[StreamRecord::TextChunks {
            time0: 1,
            index: 0,
            dt: vec![1],
            texts: vec!["a".to_owned()],
        }])
        .unwrap_err(),
        StreamError::GapCountMismatch {
            label: "text-chunks",
            members: 1,
            gaps: 1,
        }
    );
    assert_eq!(
        expand(&[StreamRecord::ToolCallChunks {
            time0: 1,
            index: 0,
            dt: vec![],
            id,
            name: Some(String::new()),
            args: vec!["{}".to_owned()],
        }])
        .unwrap_err(),
        StreamError::EmptyToolCallName
    );
    assert!(matches!(
        expand(&[StreamRecord::TextChunks {
            time0: max_safe_time(),
            index: 0,
            dt: vec![1],
            texts: vec!["a".to_owned(), "b".to_owned()],
        }])
        .unwrap_err(),
        StreamError::RunTimeUnsafe { .. }
    ));
}

// ---- readers ---------------------------------------------------------------------------------

/// Upstream's latency rule: a token delta is output, not framing, and a name-bearing call counts
/// even with empty arguments.
#[test]
fn classifies_token_deltas() {
    assert!(is_token_delta(&chunk(0, "x")));
    assert!(!is_token_delta(&chunk(0, "")));
    assert!(is_token_delta(&reasoning(0, "x")));
    assert!(is_token_delta(&tool(0, "c", Some("read"), "")));
    assert!(is_token_delta(&tool(0, "c", None, "{")));
    assert!(!is_token_delta(&tool(0, "c", None, "")));
    assert!(!is_token_delta(&start(0, "text")));
    assert!(!is_token_delta(&end(0, text_block("x"))));
    assert!(!is_token_delta(&StreamChunk::Usage(TokenUsage::default())));
    assert!(!is_token_delta(&finish(FinishReason::Stop)));
}

/// Upstream's visibility rules, including the three exclusions that are easy to get wrong: a
/// whitespace-only fragment, a text block boundary, and a tool call.
#[test]
fn classifies_visible_chunks() {
    assert!(is_visible_chunk(&chunk(0, "hi")));
    assert!(!is_visible_chunk(&chunk(0, "  \n\t")));
    assert!(is_visible_chunk(&reasoning(0, "why")));
    assert!(!is_visible_chunk(&start(0, "text")));
    assert!(!is_visible_chunk(&start(0, "reasoning")));
    assert!(!is_visible_chunk(&start(0, "tool-call")));
    assert!(is_visible_chunk(&start(0, "image")));
    assert!(is_visible_chunk(&end(0, text_block("visible"))));
    assert!(!is_visible_chunk(&end(0, text_block("   "))));
    assert!(!is_visible_chunk(&end(
        0,
        ContentBlock::ToolCall {
            id: ToolCallId::new("c").unwrap(),
            name: "read".to_owned(),
            arguments: "{}".to_owned(),
        }
    )));
    assert!(!is_visible_chunk(&tool(0, "c", Some("read"), "{}")));
    assert!(!is_visible_chunk(&finish(FinishReason::Stop)));

    assert!(chunk_has_visible_text(&chunk(0, "hi")));
    assert!(!chunk_has_visible_text(&chunk(0, " ")));
    assert!(chunk_has_visible_text(&end(0, text_block("hi"))));
    assert!(!chunk_has_visible_text(&end(0, text_block(" "))));
    assert!(!chunk_has_visible_text(&reasoning(0, "hi")));
}

/// The first-token time stops at the first qualifying member, and a run carrying a name answers with
/// its own origin rather than scanning.
#[test]
fn reads_the_first_token_time_from_runs() {
    let run = StreamRecord::TextChunks {
        time0: 100,
        index: 0,
        dt: vec![5, 5],
        texts: vec![String::new(), "a".to_owned(), "b".to_owned()],
    };
    assert_eq!(
        run_first_token_time(StreamRun::of(&run).unwrap()),
        Some(105),
        "an empty fragment is a keep-alive, not a token"
    );
    assert_eq!(
        run_first_visible_time(StreamRun::of(&run).unwrap()),
        Some(105)
    );

    let named = StreamRecord::ToolCallChunks {
        time0: 7,
        index: 0,
        dt: vec![1],
        id: ToolCallId::new("c").unwrap(),
        name: Some("read".to_owned()),
        args: vec![String::new(), "{}".to_owned()],
    };
    assert_eq!(
        run_first_token_time(StreamRun::of(&named).unwrap()),
        Some(7)
    );
    assert_eq!(
        run_first_visible_time(StreamRun::of(&named).unwrap()),
        None,
        "a tool call is protocol, not transcript"
    );

    let whitespace = StreamRecord::ReasoningChunks {
        time0: 1,
        index: 0,
        dt: vec![2],
        texts: vec![" ".to_owned(), String::new()],
    };
    assert_eq!(
        run_first_visible_time(StreamRun::of(&whitespace).unwrap()),
        None
    );
    assert_eq!(first_token_time(&[run]), Some(105));
}

#[test]
fn answers_stream_level_questions() {
    let stream = vec![
        StreamRecord::Chunk {
            time: 5,
            chunk: start(0, "text"),
        },
        StreamRecord::Chunk {
            time: 9,
            chunk: finish(FinishReason::Length),
        },
        StreamRecord::TextChunks {
            time0: 6,
            index: 0,
            dt: vec![1],
            texts: vec!["he".to_owned(), "llo".to_owned()],
        },
    ];
    assert_eq!(join_text(&stream), "hello");
    assert_eq!(last_finish(&stream), Some(&FinishReason::Length));
    assert_eq!(raw_chunks(&stream).count(), 2);
    assert_eq!(first_token_time(&stream), Some(6));

    let empty = vec![StreamRecord::TextChunks {
        time0: 1,
        index: 0,
        dt: vec![],
        texts: vec!["  ".to_owned()],
    }];
    assert!(!has_visible_content(&empty));
    assert!(!has_visible_text(&empty));
    assert_eq!(
        first_token_time(&empty),
        Some(1),
        "whitespace is still a token"
    );
    assert!(has_visible_content(&stream));
    assert!(has_visible_text(&stream));
}

/// `assemble_stream` must agree with feeding the expanded chunks, because assembly only
/// concatenates — that agreement is the whole justification for collapsing a run into one delta.
///
/// The stream carries its `block-start` records because the local assembler reports an orphan delta
/// where upstream's tolerates it; a stream a real attempt produces always has them.
#[test]
fn assembling_a_stream_matches_its_expansion() {
    let stream = vec![
        StreamRecord::Chunk {
            time: 0,
            chunk: start(0, "text"),
        },
        StreamRecord::TextChunks {
            time0: 1,
            index: 0,
            dt: vec![1],
            texts: vec!["he".to_owned(), "llo".to_owned()],
        },
        StreamRecord::Chunk {
            time: 3,
            chunk: start(1, "tool-call"),
        },
        StreamRecord::ToolCallChunks {
            time0: 3,
            index: 1,
            dt: vec![1],
            id: ToolCallId::new("c").unwrap(),
            name: Some("read".to_owned()),
            args: vec!["{".to_owned(), "}".to_owned()],
        },
        StreamRecord::Chunk {
            time: 4,
            chunk: finish(FinishReason::ToolCalls),
        },
    ];
    // The stream announced `Finish`, so the two blocks it never closed are closed from their
    // deltas rather than reported as truncation.
    let from_runs = assemble_stream(&stream).unwrap();
    assert_eq!(from_runs.len(), 2);
    assert_eq!(from_runs[0], text_block("hello"));

    let tool_call = || ContentBlock::ToolCall {
        id: ToolCallId::new("c").unwrap(),
        name: "read".to_owned(),
        arguments: "{}".to_owned(),
    };
    assert_eq!(from_runs[1], tool_call());

    // The documented divergence: feeding an expansion back through the local assembler repeats the
    // run's single name once per member, because that assembler appends names. `assemble_stream`
    // does not, so it is the path to use.
    let expanded = expand(&stream)
        .unwrap()
        .into_iter()
        .map(|timed| timed.chunk)
        .collect::<Vec<_>>();
    let from_expansion = girder_core::assemble_message(&expanded).unwrap();
    assert_eq!(from_expansion[0], text_block("hello"));
    assert_eq!(
        from_expansion[1],
        ContentBlock::ToolCall {
            id: ToolCallId::new("c").unwrap(),
            name: "readread".to_owned(),
            arguments: "{}".to_owned(),
        }
    );
}

/// A stream that names a delta's block without ever starting it is refused by the local assembler,
/// and the refusal is the documented divergence rather than a surprise.
#[test]
fn assembling_a_stream_without_its_start_records_is_refused() {
    let stream = vec![StreamRecord::TextChunks {
        time0: 1,
        index: 0,
        dt: vec![],
        texts: vec!["orphan".to_owned()],
    }];
    assert_eq!(
        assemble_stream(&stream).unwrap_err(),
        AssemblyError::OrphanDelta { index: 0 }
    );
}

// ---- attempt: framing ------------------------------------------------------------------------

/// The full lifecycle: one start, a dense chunk sequence, and one committed end carrying the
/// settlement sequence.
#[test]
fn frames_one_attempt_densely() {
    let mut revisions = Revision::new();
    let mut attempt = Attempt::new(AttemptId::new("s:1"), 2, 3);

    let opening = attempt.start(&mut revisions);
    assert_eq!(
        opening,
        Frame::Start {
            attempt_id: AttemptId::new("s:1"),
            revision: 1,
            turn: 2,
            step: 3,
        }
    );
    assert!(!attempt.ended());

    let first = attempt.push(10, start(0, "text"), &mut revisions).unwrap();
    let second = attempt.push(11, chunk(0, "hi"), &mut revisions).unwrap();
    let third = attempt
        .push(12, end(0, text_block("hi")), &mut revisions)
        .unwrap();
    for (expected_index, frame) in [&first, &second, &third].into_iter().enumerate() {
        match frame {
            Frame::Chunk {
                index, revision, ..
            } => {
                assert_eq!(*index, expected_index);
                assert_eq!(*revision, 2 + expected_index as u64);
            }
            other => panic!("expected a chunk frame, got {other:?}"),
        }
    }
    assert_eq!(attempt.index(), 3);

    let ending = attempt
        .settle(Settlement::Message, || Ok::<u64, ()>(42), &mut revisions)
        .unwrap();
    assert_eq!(
        ending,
        Frame::End {
            attempt_id: AttemptId::new("s:1"),
            revision: 5,
            index: 3,
            outcome: Outcome::Committed {
                event_type: Settlement::Message,
                seq: 42,
            },
        }
    );
    assert!(attempt.ended());
}

/// A settlement whose append fails still emits a terminal frame, and reports both halves: the cause
/// belongs to the writer, the frame to the follower.
#[test]
fn a_failed_settlement_abandons_the_attempt() {
    let mut revisions = Revision::new();
    let mut attempt = Attempt::new(AttemptId::new("s:1"), 1, 1);
    let _ = attempt.start(&mut revisions);
    let _ = attempt.push(1, chunk(0, "hi"), &mut revisions).unwrap();

    let failure = attempt
        .settle(
            Settlement::Attempt,
            || Err::<u64, &str>("log closed"),
            &mut revisions,
        )
        .unwrap_err();
    assert_eq!(failure.cause, "log closed");
    assert_eq!(
        failure.frame,
        Frame::End {
            attempt_id: AttemptId::new("s:1"),
            revision: 3,
            index: 1,
            outcome: Outcome::Abandoned,
        }
    );
    assert!(attempt.ended());
    assert!(failure.to_string().contains("log closed"));
}

/// A cancellation before any content is the `Abandoned` path, and the attempt's durable stream is
/// still readable after it.
#[test]
fn abandoning_keeps_the_stream_readable() {
    let mut revisions = Revision::new();
    let mut attempt = Attempt::new(AttemptId::new("s:1"), 1, 1);
    let _ = attempt.start(&mut revisions);
    let _ = attempt
        .push(1, chunk(0, "partial"), &mut revisions)
        .unwrap();
    let frame = attempt.abandon(&mut revisions);
    assert_eq!(
        frame,
        Frame::End {
            attempt_id: AttemptId::new("s:1"),
            revision: 3,
            index: 1,
            outcome: Outcome::Abandoned,
        }
    );
    assert_eq!(join_text(&attempt.stream()), "partial");
}

// ---- attempt: assembly -----------------------------------------------------------------------

/// Blocks come from the same chunks, and a tool call the provider never closed is closed from its
/// deltas — the local assembler's rule, reached through the attempt.
#[test]
fn assembles_blocks_and_closes_an_unclosed_tool_call() {
    let mut revisions = Revision::new();
    let mut attempt = Attempt::new(AttemptId::new("s:1"), 1, 1);
    let _ = attempt.push(1, start(0, "text"), &mut revisions).unwrap();
    let _ = attempt.push(2, chunk(0, "hello"), &mut revisions).unwrap();
    let _ = attempt
        .push(3, end(0, text_block("hello")), &mut revisions)
        .unwrap();
    let _ = attempt
        .push(4, start(1, "tool-call"), &mut revisions)
        .unwrap();
    let _ = attempt
        .push(5, tool(1, "c1", Some("read"), "{\"p\":1}"), &mut revisions)
        .unwrap();

    assert_eq!(
        attempt.blocks().unwrap(),
        vec![
            text_block("hello"),
            ContentBlock::ToolCall {
                id: ToolCallId::new("c1").unwrap(),
                name: "read".to_owned(),
                arguments: "{\"p\":1}".to_owned(),
            },
        ]
    );
    // The durable stream outlives the block read.
    assert_eq!(join_text(&attempt.stream()), "hello");
    assert_eq!(attempt.finish(), FinishReason::Stop);
}

/// `interrupted_blocks` keeps only non-whitespace text and reasoning: a tool call would need a
/// fabricated result, and whitespace is not content.
#[test]
fn interrupted_blocks_keep_only_visible_text_and_reasoning() {
    let mut revisions = Revision::new();
    let mut attempt = Attempt::new(AttemptId::new("s:1"), 1, 1);
    let _ = attempt.push(1, start(0, "text"), &mut revisions).unwrap();
    let _ = attempt
        .push(2, chunk(0, "keep me"), &mut revisions)
        .unwrap();
    let _ = attempt
        .push(3, start(1, "reasoning"), &mut revisions)
        .unwrap();
    let _ = attempt
        .push(4, reasoning(1, "  \n"), &mut revisions)
        .unwrap();
    let _ = attempt
        .push(5, start(2, "tool-call"), &mut revisions)
        .unwrap();
    let _ = attempt
        .push(6, tool(2, "c", Some("read"), "{}"), &mut revisions)
        .unwrap();

    assert_eq!(attempt.interrupted_blocks(), vec![text_block("keep me")]);
}

/// The local assembler is stricter than upstream's, so an orphan delta is latched: the chunk is
/// still durable, and the message is refused rather than built across the hole.
#[test]
fn a_protocol_error_is_latched_but_the_chunk_stays_durable() {
    let mut revisions = Revision::new();
    let mut attempt = Attempt::new(AttemptId::new("s:1"), 1, 1);
    let _ = attempt.start(&mut revisions);
    // A delta for index 0, which never started.
    let frame = attempt.push(1, chunk(0, "orphan"), &mut revisions).unwrap();
    assert!(matches!(frame, Frame::Chunk { index: 0, .. }));
    assert_eq!(
        attempt.protocol_error(),
        Some(&AssemblyError::OrphanDelta { index: 0 })
    );
    assert_eq!(
        attempt.blocks(),
        Err(AssemblyError::OrphanDelta { index: 0 })
    );
    // Evidence survives the refusal: the durable stream records what actually arrived.
    assert_eq!(join_text(&attempt.stream()), "orphan");
}

/// Usage and finish are read off the stream, and an unstated finish defaults to stop.
#[test]
fn reads_usage_and_finish() {
    let mut revisions = Revision::new();
    let mut attempt = Attempt::new(AttemptId::new("s:1"), 1, 1);
    assert_eq!(attempt.usage(), None);
    assert_eq!(attempt.finish(), FinishReason::Stop);

    let usage = TokenUsage {
        input: 3,
        output: 2,
        cached: 1,
    };
    let _ = attempt
        .push(1, StreamChunk::Usage(usage), &mut revisions)
        .unwrap();
    let _ = attempt
        .push(2, finish(FinishReason::Length), &mut revisions)
        .unwrap();
    assert_eq!(attempt.usage(), Some(usage));
    assert_eq!(attempt.finish(), FinishReason::Length);
    assert_eq!(
        attempt.attempt_id(),
        &AttemptId::new("s:1"),
        "identity, turn, and step are exposed for the caller that settles"
    );
    assert_eq!((attempt.turn(), attempt.step()), (1, 1));
}

// ---- fold: the upstream reconnect sequence ----------------------------------------------------

/// Upstream's `SessionAssistantStreamAccumulator` case, frame for frame: a stale lifecycle at
/// revision 2, a replacement at 1, a chunk from the wrong attempt, a settled pair, and an end.
#[test]
fn fold_replaces_stale_lifecycles_and_rejects_gaps() {
    let mut fold = StreamFold::new();
    let empty = fold.snapshot().clone();
    assert_eq!(fold.snapshot(), &empty, "the baseline is cached");
    assert_eq!(fold.revision(), 0);

    fold.accept(
        &Frame::Start {
            attempt_id: AttemptId::new("stale"),
            revision: 2,
            turn: 1,
            step: 1,
        },
        -1,
    )
    .unwrap();
    assert_eq!(
        fold.snapshot(),
        &Baseline {
            revision: 2,
            active_attempt: None,
        }
    );

    fold.accept(
        &Frame::Start {
            attempt_id: AttemptId::new("current"),
            revision: 1,
            turn: 2,
            step: 3,
        },
        5,
    )
    .unwrap();
    assert_eq!(
        fold.snapshot(),
        &Baseline {
            revision: 1,
            active_attempt: Some(AttemptView {
                attempt_id: AttemptId::new("current"),
                started_after_seq: 5,
                turn: 2,
                step: 3,
                next_index: 0,
                stream: Vec::new(),
            }),
        }
    );

    // Revision 2, but from a different attempt: the active attempt is dropped.
    fold.accept(
        &Frame::Chunk {
            attempt_id: AttemptId::new("other"),
            revision: 2,
            index: 0,
            time: 4,
            chunk: chunk(0, "lost"),
        },
        5,
    )
    .unwrap();
    assert_eq!(fold.snapshot().revision, 2);
    assert!(fold.snapshot().active_attempt.is_none());

    // A replacement start at revision 3, then its chunk at 4.
    fold.accept(
        &Frame::Start {
            attempt_id: AttemptId::new("settled"),
            revision: 3,
            turn: 2,
            step: 4,
        },
        8,
    )
    .unwrap();
    fold.accept(
        &Frame::Chunk {
            attempt_id: AttemptId::new("settled"),
            revision: 4,
            index: 0,
            time: 5,
            chunk: chunk(0, "ok"),
        },
        8,
    )
    .unwrap();
    let active = fold.snapshot().clone();
    assert_eq!(active.revision, 4);
    let attempt = active.active_attempt.clone().unwrap();
    assert_eq!(attempt.next_index, 1);
    assert_eq!(attempt.started_after_seq, 8);
    assert_eq!(
        attempt.stream,
        vec![StreamRecord::TextChunks {
            time0: 5,
            index: 0,
            dt: vec![],
            texts: vec!["ok".to_owned()],
        }]
    );
    // Still cached: nothing was accepted between these two reads.
    assert_eq!(fold.snapshot(), &active);

    fold.accept(
        &Frame::End {
            attempt_id: AttemptId::new("settled"),
            revision: 5,
            index: 1,
            outcome: Outcome::Abandoned,
        },
        9,
    )
    .unwrap();
    assert_eq!(
        fold.snapshot(),
        &Baseline {
            revision: 5,
            active_attempt: None,
        }
    );
}

/// A dense gap — one frame lost — drops the attempt instead of appending across it.
#[test]
fn fold_drops_the_attempt_on_a_revision_gap() {
    let mut fold = StreamFold::new();
    fold.accept(
        &Frame::Start {
            attempt_id: AttemptId::new("a"),
            revision: 1,
            turn: 1,
            step: 1,
        },
        -1,
    )
    .unwrap();
    assert!(fold.snapshot().active_attempt.is_some());

    // Revision 3 arrives where 2 was expected.
    fold.accept(
        &Frame::Chunk {
            attempt_id: AttemptId::new("a"),
            revision: 3,
            index: 1,
            time: 2,
            chunk: chunk(0, "hole"),
        },
        -1,
    )
    .unwrap();
    assert_eq!(fold.snapshot().revision, 3);
    assert!(fold.snapshot().active_attempt.is_none());
}

/// A replacement lifecycle restarts at revision 1 even when the current revision is not 1, which is
/// the signal that the agent itself was re-attached.
#[test]
fn fold_resets_on_a_replacement_lifecycle() {
    let mut fold = StreamFold::new();
    for (revision, index) in [(1_u64, None), (2, Some(0_usize))] {
        let frame = match index {
            None => Frame::Start {
                attempt_id: AttemptId::new("old"),
                revision,
                turn: 1,
                step: 1,
            },
            Some(index) => Frame::Chunk {
                attempt_id: AttemptId::new("old"),
                revision,
                index,
                time: 1,
                chunk: chunk(0, "x"),
            },
        };
        fold.accept(&frame, 1).unwrap();
    }
    assert_eq!(fold.revision(), 2);

    // A brand-new lifecycle announces itself at revision 1.
    fold.accept(
        &Frame::Start {
            attempt_id: AttemptId::new("new"),
            revision: 1,
            turn: 2,
            step: 1,
        },
        7,
    )
    .unwrap();
    let baseline = fold.snapshot();
    assert_eq!(baseline.revision, 1);
    let active = baseline.active_attempt.as_ref().unwrap();
    assert_eq!(active.attempt_id, AttemptId::new("new"));
    assert_eq!(active.started_after_seq, 7);
}

/// A chunk whose time cannot be stored is reported, and the revision was still consumed, so a
/// follower is not stalled on a frame it can never accept.
#[test]
fn fold_reports_an_unstorable_chunk_and_consumes_its_revision() {
    let mut fold = StreamFold::new();
    fold.accept(
        &Frame::Start {
            attempt_id: AttemptId::new("a"),
            revision: 1,
            turn: 1,
            step: 1,
        },
        -1,
    )
    .unwrap();
    let error = fold
        .accept(
            &Frame::Chunk {
                attempt_id: AttemptId::new("a"),
                revision: 2,
                index: 0,
                time: i64::MAX,
                chunk: chunk(0, "x"),
            },
            -1,
        )
        .unwrap_err();
    assert!(matches!(error, StreamError::UnsafeTime { .. }));
    assert_eq!(fold.revision(), 2);
}

// ---- identity --------------------------------------------------------------------------------

#[test]
fn attempt_ids_derive_from_the_session() {
    let session = SessionId::new("sess-1").unwrap();
    assert_eq!(
        AttemptId::from_parts(&session, 3).as_str(),
        "sess-1:3",
        "upstream's `<session>:<attempt>` spelling"
    );
    assert_eq!(AttemptId::from_parts(&session, 3).to_string(), "sess-1:3");
    assert_eq!(
        Settlement::Message.as_str(),
        "assistant/message",
        "the durable event names are spelled as the log spells them"
    );
    assert_eq!(Settlement::Attempt.as_str(), "assistant/attempt");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod llm {
    pub use super::llm_attempt_gap as attempt;
}
