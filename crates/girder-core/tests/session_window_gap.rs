//! Tests for the event window and Assistant-stream fold of upstream `packages/api/session-controller`.
//!
//! Corresponds to upstream `tests/event-window.client.spec.ts` (replace/prepend/append/settle
//! algebra and synchronous publication), `tests/client-assistant-stream.spec.ts` (the start/chunk/end
//! state machine and its rebaseline triggers), and the settlement assertions in
//! `packages/client/ui-chat/tests/turn-process-tail.client.spec.ts` (`settled.order === replay.order`).
//!
//! # What each property protects
//!
//! - **A live row sits between durable rows, not among them.** A chunk that arrived after seq 2 and
//!   before seq 3 must sort there, and two chunks from one gap must stay in arrival order.
//! - **A settlement is a splice, not an append.** The committed message's seq predates rows that
//!   landed while the stream was live, so appending it would put the answer after the tool results.
//! - **A settled window equals a replayed one.** This is upstream's own acceptance property.
//! - **A redelivered durable row is a no-op.** A reconnect may replay frames; replaying one must not
//!   duplicate a row or bump the revision.
//! - **Publication is synchronous and re-entrant-safe.** A listener may read the window it watches.
//! - **A foreign settlement is not this attempt's settlement.** A `surface_append: false` message, a
//!   message at or before `startedAfterSeq`, and a message for another turn/step all publish directly.
//! - **A committed message keeps its live rows until the step ends**, while an interrupted one, an
//!   abandoned attempt, and a failed attempt retire immediately.
//! - **A hole in the dense chunk index is a rebaseline**, never a silently short stream.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module `crate::` would name *this* test crate, which is why the module
//! under test spells every library reference `girder_core::…`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::float_cmp,
    reason = "tests assert by panicking, index their own fixtures, and read best as one scenario"
)]

#[path = "../src/session_window_gap.rs"]
mod session_window_gap;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use girder_core::{ContentBlock, FinishReason, SessionId, StreamChunk, TokenUsage, ToolCallId};
use serde_json::json;
use session_window_gap::{
    ActiveAttempt, AssistantSettlement, AssistantStreamFrame, AssistantStreamOutcome,
    ClientAssistantStream, DurableEntry, EventWindow, EventWindowChange, FoldDecision, LiveChunk,
    SessionEntry, SettlementKind, StreamFoldError, fold_stream,
};

/// A session id.
fn session(name: &str) -> SessionId {
    SessionId::new(name).unwrap()
}

/// A durable row with no settlement semantics.
fn event(name: &str, seq: u64, type_name: &str) -> SessionEntry {
    SessionEntry::event(session(name), DurableEntry::new(seq, type_name))
}

/// A durable row carrying JSON, for the step-end retirement test.
fn event_data(name: &str, seq: u64, type_name: &str, data: serde_json::Value) -> SessionEntry {
    SessionEntry::event(
        session(name),
        DurableEntry::new(seq, type_name).with_data(data),
    )
}

/// A settlement wrapper as it appears in the durable log.
fn settled(name: &str, settlement: AssistantSettlement) -> SessionEntry {
    SessionEntry::event(session(name), DurableEntry::settling(settlement))
}

/// A text delta chunk, as the stream carries it.
fn text(index: usize, value: &str) -> StreamChunk {
    StreamChunk::TextDelta {
        index,
        text: value.to_owned(),
    }
}

/// The `block-start` that opens the text block at `index`.
fn text_start(index: usize) -> StreamChunk {
    StreamChunk::BlockStart {
        index,
        block_type: "text".to_owned(),
    }
}

/// A live chunk row with no gap assigned; the window resolves it onto the live tail.
fn live(name: &str, attempt: &str, turn: u64, step: u64, chunk: StreamChunk) -> SessionEntry {
    SessionEntry::live(session(name), LiveChunk::new(attempt, turn, step, 0, chunk))
}

/// A live chunk row that belongs to the gap above `floor`.
fn live_in_gap(
    name: &str,
    attempt: &str,
    turn: u64,
    step: u64,
    floor: u64,
    chunk: StreamChunk,
) -> SessionEntry {
    SessionEntry::in_gap(
        session(name),
        LiveChunk::new(attempt, turn, step, 0, chunk),
        floor,
    )
}

/// The `type` tags of a window's entries, in window order.
fn tags(entries: &[SessionEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| match entry {
            SessionEntry::Event { durable, .. } => durable.type_name.clone(),
            SessionEntry::Transient { live, .. } => {
                format!("live:{}", live.chunk_type())
            }
        })
        .collect()
}

/// Extension for readable tags in assertions.
trait ChunkType {
    /// The chunk's `type` tag.
    fn chunk_type(&self) -> &'static str;
}

impl ChunkType for LiveChunk {
    fn chunk_type(&self) -> &'static str {
        match self.chunk {
            StreamChunk::BlockStart { .. } => "block-start",
            StreamChunk::TextDelta { .. } => "text-delta",
            StreamChunk::ReasoningDelta { .. } => "reasoning-delta",
            StreamChunk::ToolCallDelta { .. } => "tool-call-delta",
            StreamChunk::BlockEnd { .. } => "block-end",
            StreamChunk::Usage(_) => "usage",
            StreamChunk::Finish { .. } => "finish",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Entry vocabulary
// ---------------------------------------------------------------------------------------------

#[test]
fn entries_carry_the_transport_discriminator_and_a_total_order() {
    let durable = event("s1", 7, "turn/start");
    let transient = live("s1", "a1", 1, 1, text(0, "hi"));

    assert_eq!(durable.kind(), "event");
    assert_eq!(transient.kind(), "transient");
    assert_eq!(durable.durable_seq(), Some(7));
    assert_eq!(transient.durable_seq(), None);
    assert_eq!(durable.session(), &session("s1"));

    // A live row sorts after the durable row at its floor and before the next durable seq.
    let at_floor = SessionEntry::Transient {
        session: session("s1"),
        live: LiveChunk::new("a1", 1, 1, 0, text(0, "x")),
        floor: 7,
        rank: 0,
    };
    let second = SessionEntry::Transient {
        session: session("s1"),
        live: LiveChunk::new("a1", 1, 1, 0, text(0, "y")),
        floor: 7,
        rank: 1,
    };
    assert!(durable.order_key() < at_floor.order_key());
    assert!(at_floor.order_key() < second.order_key());
    assert!(second.order_key() < event("s1", 8, "turn/end").order_key());

    // Two gaps never interleave, even at equal rank.
    let other_gap = SessionEntry::Transient {
        session: session("s1"),
        live: LiveChunk::new("a2", 1, 1, 0, text(0, "z")),
        floor: 9,
        rank: 0,
    };
    assert!(other_gap.order_key() > second.order_key());
}

#[test]
fn a_settlement_reports_only_its_own_attempt() {
    let settlement = AssistantSettlement::message(4, "a1", 1, 1);
    let entry = settled("s1", settlement.clone());
    assert_eq!(entry.attempt_id().map(String::as_str), Some("a1"));
    assert_eq!(event("s1", 1, "turn/start").attempt_id(), None);

    let attempt = ActiveAttempt {
        attempt_id: "a1".to_owned(),
        started_after_seq: 2,
        turn: 1,
        step: 1,
        next_index: 0,
    };
    assert!(settlement.settles(&attempt));
    assert!(
        !settlement
            .clone()
            .with_surface_append(false)
            .settles(&attempt)
    );
    assert!(settlement.clone().with_interrupted(true).settles(&attempt));
    assert!(
        !settlement
            .clone()
            .with_interrupted(true)
            .with_surface_append(false)
            .settles(&attempt)
    );
    assert!(!AssistantSettlement::message(2, "a1", 1, 1).settles(&attempt));
    assert!(!AssistantSettlement::message(4, "a1", 1, 2).settles(&attempt));
    assert!(!AssistantSettlement::message(4, "a1", 2, 1).settles(&attempt));
}

// ---------------------------------------------------------------------------------------------
// Window algebra
// ---------------------------------------------------------------------------------------------

#[test]
fn replace_sorts_entries_and_reports_its_delta() {
    let window = EventWindow::new();
    assert!(window.is_empty());
    assert_eq!(window.revision(), 0);
    assert_eq!(
        window.snapshot().change,
        EventWindowChange::Replace { entries: vec![] }
    );

    window.replace(
        vec![
            event("s1", 3, "step/start"),
            live_in_gap("s1", "a1", 1, 1, 2, text(0, "chunk")),
            event("s1", 1, "turn/start"),
            event("s1", 2, "step/start"),
        ],
        true,
    );

    assert_eq!(
        window.durable_seqs(),
        vec![1, 2, 3],
        "the window is ordered by durable seq regardless of arrival order"
    );
    assert_eq!(
        tags(&window.entries()),
        vec!["turn/start", "step/start", "live:text-delta", "step/start"],
        "the live row lands in the gap above its floor, before the next durable seq"
    );
    assert_eq!(window.revision(), 1);
    assert!(window.has_more());
    assert!(window.history_was_partial());
    assert_eq!(window.snapshot().change.kind(), "replace");
    assert_eq!(window.snapshot().change.introduced_len(), 4);
}

#[test]
fn prepend_adds_older_entries_once_and_updates_has_more() {
    let window = EventWindow::new();
    window.replace(vec![event("s1", 5, "turn/start")], true);
    window.prepend(
        vec![
            event("s1", 2, "turn/end"),
            event("s1", 1, "turn/start"),
            event("s1", 5, "turn/start"),
        ],
        false,
    );

    assert_eq!(
        window.durable_seqs(),
        vec![1, 2, 5],
        "the duplicate is dropped"
    );
    assert!(
        !window.has_more(),
        "a page that reaches the log start clears it"
    );
    assert!(
        window.history_was_partial(),
        "the window was once partial, and that is sticky across the prepend"
    );
    let snapshot = window.snapshot();
    assert_eq!(snapshot.change.kind(), "prepend");
    assert_eq!(snapshot.change.introduced_len(), 2);
    assert_eq!(window.revision(), 2);
}

#[test]
fn a_redelivered_durable_append_is_a_noop() {
    let window = EventWindow::new();
    assert!(window.append(event("s1", 1, "turn/start")));
    let revision = window.revision();
    assert!(!window.append(event("s1", 1, "turn/start")));
    assert_eq!(
        window.revision(),
        revision,
        "a replay must not bump the revision"
    );
    assert_eq!(window.len(), 1);
}

#[test]
fn live_chunks_accumulate_inside_the_newest_gap_and_survive_a_page() {
    let window = EventWindow::new();
    window.replace(vec![event("s1", 1, "turn/start")], true);
    window.append_live(session("s1"), LiveChunk::new("a1", 1, 1, 10, text(0, "he")));
    window.append_live(
        session("s1"),
        LiveChunk::new("a1", 1, 1, 11, text(0, "llo")),
    );

    let entries = window.entries();
    assert_eq!(
        tags(&entries),
        vec!["turn/start", "live:text-delta", "live:text-delta"]
    );
    // The two chunks keep arrival order, which is what makes the assembled text "hello".
    let mut chunks = vec![text_start(0)];
    chunks.extend(
        entries
            .iter()
            .filter_map(SessionEntry::live_chunk)
            .map(|live| live.chunk.clone()),
    );
    let folded = fold_stream(&chunks);
    assert_eq!(
        folded.unwrap(),
        vec![ContentBlock::Text {
            text: "hello".to_owned()
        }]
    );

    // An older page must not disturb the live tail.
    window.prepend(vec![event("s1", 0, "turn/start")], false);
    let entries = window.entries();
    assert_eq!(
        tags(&entries),
        vec![
            "turn/start",
            "turn/start",
            "live:text-delta",
            "live:text-delta"
        ]
    );
}

#[test]
fn settling_an_attempt_removes_its_live_rows_and_splices_the_message() {
    let window = EventWindow::new();
    window.replace(
        vec![
            event("s1", 2, "step/start"),
            event("s1", 4, "tool/call"),
            event("s1", 5, "tool/result"),
        ],
        false,
    );
    window.append_live(
        session("s1"),
        LiveChunk::new("a1", 1, 1, 10, text(0, "par")),
    );
    window.append_live(
        session("s1"),
        LiveChunk::new("a1", 1, 1, 11, text(0, "tial")),
    );
    window.append_live(
        session("s1"),
        LiveChunk::new("a2", 1, 1, 12, text(0, "other")),
    );

    let settlement = AssistantSettlement::message(3, "a1", 1, 1);
    window.settle_assistant("a1", Some(settled("s1", settlement)));

    assert_eq!(
        tags(&window.entries()),
        vec![
            "step/start",
            "assistant/message",
            "tool/call",
            "tool/result",
            "live:text-delta"
        ],
        "the message is spliced at seq 3, and only a1's live rows are gone"
    );
    let snapshot = window.snapshot();
    assert_eq!(snapshot.change.kind(), "settle-assistant");
    assert_eq!(snapshot.change.introduced_len(), 1);
    match &snapshot.change {
        EventWindowChange::SettleAssistant { attempt_id, entry } => {
            assert_eq!(attempt_id, "a1");
            assert_eq!(entry.as_ref().and_then(SessionEntry::durable_seq), Some(3));
        }
        other => panic!("expected settle-assistant, got {}", other.kind()),
    }

    // An abandonment retires without inserting anything.
    let before = window.revision();
    assert_eq!(window.retire_attempt("a2"), 1);
    assert!(window.revision() > before);
    assert_eq!(
        tags(&window.entries()),
        vec![
            "step/start",
            "assistant/message",
            "tool/call",
            "tool/result"
        ]
    );
    // A second retirement removes nothing and publishes nothing.
    let before = window.revision();
    assert_eq!(window.retire_attempt("a2"), 0);
    assert_eq!(window.revision(), before);
}

#[test]
fn a_settled_window_equals_a_replayed_one() {
    // The live picture: rows 2, 4, 5 arrived durably, then chunks, then the message at seq 3.
    let streaming = EventWindow::new();
    streaming.replace(
        vec![
            event("s1", 2, "step/start"),
            event("s1", 4, "tool/call"),
            event("s1", 5, "tool/result"),
        ],
        false,
    );
    streaming.append_live(
        session("s1"),
        LiveChunk::new("a1", 1, 1, 10, text(0, "partial")),
    );
    streaming.settle_assistant(
        "a1",
        Some(settled("s1", AssistantSettlement::message(3, "a1", 1, 1))),
    );

    // The replay: the same settled log opened cold.
    let replay = EventWindow::new();
    replay.replace(
        vec![
            event("s1", 2, "step/start"),
            event("s1", 3, "assistant/message"),
            event("s1", 4, "tool/call"),
            event("s1", 5, "tool/result"),
        ],
        false,
    );

    assert_eq!(
        tags(&streaming.entries()),
        tags(&replay.entries()),
        "a settled window must render exactly like a replay of the settled log"
    );
}

#[test]
fn publication_is_synchronous_and_a_listener_may_read_the_window() {
    let window = Arc::new(EventWindow::new());
    let observed = Arc::new(AtomicUsize::new(0));
    let revisions = Arc::new(AtomicUsize::new(0));

    let listener = {
        let window = Arc::clone(&window);
        let observed = Arc::clone(&observed);
        let revisions = Arc::clone(&revisions);
        move || {
            // Reading the window being published to must not deadlock.
            let snapshot = window.snapshot();
            observed.store(snapshot.entries.len(), Ordering::SeqCst);
            revisions.store(revisions.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
        }
    };
    let id = window.subscribe(listener);
    assert_eq!(window.listener_count(), 1);

    window.replace(vec![event("s1", 1, "turn/start")], false);
    assert_eq!(
        observed.load(Ordering::SeqCst),
        1,
        "the listener ran inside the mutating call"
    );
    assert_eq!(revisions.load(Ordering::SeqCst), 1);

    window.append(event("s1", 2, "step/start"));
    assert_eq!(observed.load(Ordering::SeqCst), 2);
    assert_eq!(window.revision(), 2);

    window.unsubscribe(id);
    window.append(event("s1", 3, "step/start"));
    assert_eq!(
        revisions.load(Ordering::SeqCst),
        2,
        "an unsubscribed listener is silent"
    );
    assert_eq!(window.listener_count(), 0);
}

#[test]
fn the_debug_surface_reports_state_without_listening() {
    let window = EventWindow::new();
    let text = format!("{window:?}");
    assert!(text.contains("EventWindow"));
    assert!(text.contains("revision"));
}

// ---------------------------------------------------------------------------------------------
// Window + fold integration
// ---------------------------------------------------------------------------------------------

#[test]
fn folding_live_chunks_builds_the_assistant_blocks() {
    let chunks = vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "reasoning".to_owned(),
        },
        StreamChunk::ReasoningDelta {
            index: 0,
            text: "thinking".to_owned(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Reasoning {
                text: "thinking".to_owned(),
            },
        },
        StreamChunk::BlockStart {
            index: 1,
            block_type: "tool-call".to_owned(),
        },
        StreamChunk::ToolCallDelta {
            index: 1,
            id: ToolCallId::new("call-1").unwrap(),
            name: Some("write".to_owned()),
            arguments_delta: "{\"content\":".to_owned(),
        },
        StreamChunk::ToolCallDelta {
            index: 1,
            id: ToolCallId::new("call-1").unwrap(),
            name: None,
            arguments_delta: "\"hello\"}".to_owned(),
        },
        StreamChunk::Usage(TokenUsage {
            input: 5,
            output: 7,
            cached: 0,
        }),
        StreamChunk::Finish {
            reason: FinishReason::Stop,
        },
    ];
    let blocks = fold_stream(&chunks).unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        blocks[0],
        ContentBlock::Reasoning {
            text: "thinking".to_owned()
        }
    );
    assert_eq!(
        blocks[1],
        ContentBlock::ToolCall {
            id: ToolCallId::new("call-1").unwrap(),
            name: "write".to_owned(),
            arguments: "{\"content\":\"hello\"}".to_owned(),
        }
    );
}

#[test]
fn a_block_end_is_authoritative_over_accumulated_deltas() {
    let chunks = vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".to_owned(),
        },
        text(0, "partial"),
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: "complete".to_owned(),
            },
        },
    ];
    assert_eq!(
        fold_stream(&chunks).unwrap(),
        vec![ContentBlock::Text {
            text: "complete".to_owned()
        }]
    );
}

#[test]
fn a_corrupt_chunk_stream_is_refused_rather_than_guessed() {
    assert_eq!(
        fold_stream(&[text(3, "orphan")]).unwrap_err(),
        StreamFoldError::MissingIndex { index: 3 }
    );
    assert_eq!(
        fold_stream(&[StreamChunk::BlockEnd {
            index: 1,
            block: ContentBlock::Text {
                text: String::new()
            },
        }])
        .unwrap_err(),
        StreamFoldError::OutOfOrder { index: 1 }
    );
    assert_eq!(
        fold_stream(&[StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".to_owned(),
        }])
        .unwrap_err(),
        StreamFoldError::IncompleteToolCall { index: 0 }
    );
    // Display is exercised so the error is usable in a log.
    assert!(
        StreamFoldError::MissingIndex { index: 2 }
            .to_string()
            .contains('2')
    );
}

// ---------------------------------------------------------------------------------------------
// The Assistant stream fold
// ---------------------------------------------------------------------------------------------

/// A fold over `s1` with one attempt open and two chunks consumed.
fn open_attempt() -> ClientAssistantStream {
    let mut fold = ClientAssistantStream::new(session("s1"));
    fold.replace(&[event("s1", 1, "turn/start"), event("s1", 2, "step/start")]);
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::Start {
            attempt_id: "a1".to_owned(),
            started_after_seq: 2,
            turn: 1,
            step: 1,
        })
        .kind(),
        "quiet"
    );
    for (index, value) in ["he", "llo"].iter().enumerate() {
        #[allow(clippy::cast_possible_truncation, reason = "fixture indices")]
        let index = index as u64;
        let decision = fold.accept_frame(AssistantStreamFrame::Chunk {
            attempt_id: "a1".to_owned(),
            index,
            time: 10 + index,
            chunk: text(0, value),
        });
        assert_eq!(decision.kind(), "transient");
    }
    fold
}

#[test]
fn a_frame_stream_becomes_ordered_live_rows_in_one_gap() {
    let mut fold = open_attempt();
    assert!(fold.active_attempt().is_some());
    assert_eq!(fold.active_attempt().unwrap().next_index, 2);
    assert_eq!(fold.durable_cursor(), Some(2));

    let decision = fold.accept_frame(AssistantStreamFrame::Chunk {
        attempt_id: "a1".to_owned(),
        index: 2,
        time: 20,
        chunk: text(0, "!"),
    });
    let FoldDecision::Transient { entry } = decision else {
        panic!("expected a transient row");
    };
    assert_eq!(entry.kind(), "transient");
    assert_eq!(
        entry.order_key(),
        (2, 1, 2),
        "the third chunk ranks third in the gap above seq 2"
    );
    assert_eq!(entry.session(), &session("s1"));
}

#[test]
fn a_hole_in_the_dense_index_forces_a_rebaseline() {
    let mut fold = open_attempt();
    let decision = fold.accept_frame(AssistantStreamFrame::Chunk {
        attempt_id: "a1".to_owned(),
        index: 5,
        time: 20,
        chunk: text(0, "skipped"),
    });
    assert_eq!(decision, FoldDecision::Rebaseline);
}

#[test]
fn a_frame_without_a_start_is_presentation_only() {
    let mut fold = ClientAssistantStream::new(session("s1"));
    // A controller mounted after the Host began the attempt: no start frame to reconstruct.
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::Chunk {
            attempt_id: "a1".to_owned(),
            index: 0,
            time: 1,
            chunk: text(0, "x"),
        }),
        FoldDecision::Quiet
    );
    assert!(fold.active_attempt().is_none());
}

#[test]
fn a_committed_message_is_staged_then_published_and_retired_at_step_end() {
    let mut fold = open_attempt();
    let message = AssistantSettlement::message(3, "a1", 1, 1);

    // The durable message arrives while the attempt is still open: it is staged, not published.
    assert_eq!(
        fold.accept_durable(settled("s1", message.clone())),
        FoldDecision::Quiet
    );
    assert_eq!(fold.pending_len(), 1);
    assert_eq!(fold.durable_cursor(), Some(3));

    let decision = fold.accept_frame(AssistantStreamFrame::End {
        attempt_id: "a1".to_owned(),
        index: 2,
        outcome: AssistantStreamOutcome::Committed {
            event_type: SettlementKind::Message,
            seq: 3,
        },
    });
    let FoldDecision::Publish {
        entry,
        retire_attempt_id,
    } = decision
    else {
        panic!("a committed message publishes");
    };
    assert_eq!(entry.durable_seq(), Some(3));
    assert_eq!(retire_attempt_id, None);
    assert_eq!(fold.pending_len(), 0);
    assert!(fold.active_attempt().is_none());
    // The live rows are retained until the step ends.
    let step_end = event_data("s1", 4, "step/end", json!({ "turn": 1, "step": 1 }));
    let decision = fold.accept_durable(step_end);
    let FoldDecision::Publish {
        retire_attempt_id, ..
    } = decision
    else {
        panic!("step/end publishes");
    };
    assert_eq!(retire_attempt_id.as_deref(), Some("a1"));
}

#[test]
fn an_interrupted_message_settles_immediately() {
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_durable(settled(
            "s1",
            AssistantSettlement::message(3, "a1", 1, 1).with_interrupted(true)
        )),
        FoldDecision::Quiet
    );
    let decision = fold.accept_frame(AssistantStreamFrame::End {
        attempt_id: "a1".to_owned(),
        index: 2,
        outcome: AssistantStreamOutcome::Committed {
            event_type: SettlementKind::Message,
            seq: 3,
        },
    });
    match decision {
        FoldDecision::Settlement { attempt_id, entry } => {
            assert_eq!(attempt_id, "a1");
            assert_eq!(entry.durable_seq(), Some(3));
        }
        other => panic!("expected a settlement, got {}", other.kind()),
    }
}

#[test]
fn an_attempt_record_settles_immediately() {
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_durable(settled("s1", AssistantSettlement::attempt(3, "a1", 1, 1))),
        FoldDecision::Quiet
    );
    let decision = fold.accept_frame(AssistantStreamFrame::End {
        attempt_id: "a1".to_owned(),
        index: 2,
        outcome: AssistantStreamOutcome::Committed {
            event_type: SettlementKind::Attempt,
            seq: 3,
        },
    });
    assert_eq!(decision.kind(), "settlement");
}

#[test]
fn a_foreign_settlement_publishes_directly() {
    // A message surfaced by something other than `append` belongs to a replay or a fork.
    let mut fold = open_attempt();
    let deciding = settled(
        "s1",
        AssistantSettlement::message(3, "a1", 1, 1).with_surface_append(false),
    );
    let decision = fold.accept_durable(deciding);
    assert_eq!(
        decision.kind(),
        "publish",
        "a non-append message is not this attempt's settlement"
    );
    assert_eq!(fold.pending_len(), 0);

    // A message at or before `startedAfterSeq` cannot be this attempt's answer.
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_durable(settled("s1", AssistantSettlement::message(2, "a1", 1, 1)))
            .kind(),
        "publish"
    );

    // A message for a different step belongs to another attempt of the same turn.
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_durable(settled("s1", AssistantSettlement::message(3, "a1", 1, 2)))
            .kind(),
        "publish"
    );
}

#[test]
fn a_duplicate_settlement_forces_a_rebaseline() {
    let mut fold = open_attempt();
    let message = AssistantSettlement::message(3, "a1", 1, 1);
    assert_eq!(
        fold.accept_durable(settled("s1", message.clone())),
        FoldDecision::Quiet
    );
    assert_eq!(
        fold.accept_durable(settled("s1", message)),
        FoldDecision::Rebaseline,
        "which copy of a redelivered settlement is authoritative cannot be known"
    );
}

#[test]
fn an_abandoned_attempt_retires_its_rows_and_a_pending_settlement_blocks_it() {
    let mut fold = open_attempt();
    let decision = fold.accept_frame(AssistantStreamFrame::End {
        attempt_id: "a1".to_owned(),
        index: 2,
        outcome: AssistantStreamOutcome::Abandoned,
    });
    match decision {
        FoldDecision::Abandonment { attempt_id } => assert_eq!(attempt_id, "a1"),
        other => panic!("expected an abandonment, got {}", other.kind()),
    }

    // With a settlement staged that nothing can release, the two pictures disagree.
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_durable(settled("s1", AssistantSettlement::message(3, "a1", 1, 1)))
            .kind(),
        "quiet"
    );
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::End {
            attempt_id: "a1".to_owned(),
            index: 2,
            outcome: AssistantStreamOutcome::Abandoned,
        }),
        FoldDecision::Rebaseline
    );
}

#[test]
fn an_end_with_the_wrong_index_or_unknown_seq_forces_a_rebaseline() {
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::End {
            attempt_id: "a1".to_owned(),
            index: 9,
            outcome: AssistantStreamOutcome::Committed {
                event_type: SettlementKind::Message,
                seq: 3,
            },
        }),
        FoldDecision::Rebaseline
    );

    // The end names a settlement that was never staged.
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::End {
            attempt_id: "a1".to_owned(),
            index: 2,
            outcome: AssistantStreamOutcome::Committed {
                event_type: SettlementKind::Message,
                seq: 3,
            },
        }),
        FoldDecision::Rebaseline
    );

    // The end names a different event type than the staged settlement.
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_durable(settled("s1", AssistantSettlement::message(3, "a1", 1, 1)))
            .kind(),
        "quiet"
    );
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::End {
            attempt_id: "a1".to_owned(),
            index: 2,
            outcome: AssistantStreamOutcome::Committed {
                event_type: SettlementKind::Attempt,
                seq: 3,
            },
        }),
        FoldDecision::Rebaseline
    );
}

#[test]
fn a_start_on_top_of_an_open_attempt_forces_a_rebaseline() {
    let mut fold = open_attempt();
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::Start {
            attempt_id: "a2".to_owned(),
            started_after_seq: 2,
            turn: 1,
            step: 1,
        }),
        FoldDecision::Rebaseline
    );
}

#[test]
fn an_already_published_settlement_is_quiet() {
    let mut fold = ClientAssistantStream::new(session("s1"));
    fold.replace(&[
        event("s1", 1, "turn/start"),
        settled("s1", AssistantSettlement::message(3, "a1", 1, 1)),
    ]);
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::Start {
            attempt_id: "a1".to_owned(),
            started_after_seq: 2,
            turn: 1,
            step: 1,
        })
        .kind(),
        "quiet"
    );
    // Staged: the fold has an open attempt and the settlement is new to it.
    let mut fold = ClientAssistantStream::new(session("s1"));
    fold.replace(&[event("s1", 1, "turn/start")]);
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::Start {
            attempt_id: "a1".to_owned(),
            started_after_seq: 1,
            turn: 1,
            step: 1,
        })
        .kind(),
        "quiet"
    );
    assert_eq!(
        fold.accept_durable(settled("s1", AssistantSettlement::message(2, "a1", 1, 1)))
            .kind(),
        "quiet"
    );
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::End {
            attempt_id: "a1".to_owned(),
            index: 0,
            outcome: AssistantStreamOutcome::Committed {
                event_type: SettlementKind::Message,
                seq: 2,
            },
        })
        .kind(),
        "publish"
    );
    // A terminal frame for an attempt that is no longer open is quiet, not a second retirement.
    assert_eq!(
        fold.accept_frame(AssistantStreamFrame::End {
            attempt_id: "a1".to_owned(),
            index: 0,
            outcome: AssistantStreamOutcome::Committed {
                event_type: SettlementKind::Message,
                seq: 2,
            },
        }),
        FoldDecision::Quiet
    );
}

#[test]
fn frames_round_trip_through_the_wire_form() {
    let frame = AssistantStreamFrame::End {
        attempt_id: "a1".to_owned(),
        index: 2,
        outcome: AssistantStreamOutcome::Committed {
            event_type: SettlementKind::Attempt,
            seq: 7,
        },
    };
    let encoded = serde_json::to_value(&frame).unwrap();
    assert_eq!(encoded["type"], json!("end"));
    assert_eq!(encoded["attemptId"], json!("a1"));
    assert_eq!(encoded["outcome"]["kind"], json!("committed"));
    assert_eq!(encoded["outcome"]["eventType"], json!("assistant/attempt"));
    let decoded: AssistantStreamFrame = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, frame);

    let start = AssistantStreamFrame::Start {
        attempt_id: "a1".to_owned(),
        started_after_seq: 4,
        turn: 1,
        step: 2,
    };
    let encoded = serde_json::to_value(&start).unwrap();
    assert_eq!(encoded["type"], json!("start"));
    assert_eq!(encoded["startedAfterSeq"], json!(4));
    assert_eq!(
        serde_json::from_value::<AssistantStreamFrame>(encoded).unwrap(),
        start
    );
}

#[test]
fn the_fold_debug_surface_names_its_state() {
    let fold = open_attempt();
    let text = format!("{fold:?}");
    assert!(text.contains("ClientAssistantStream"));
    assert!(text.contains("a1"));
    assert_eq!(fold.session(), &session("s1"));
}
