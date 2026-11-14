//! Tests for the session stream cursor.
//!
//! Four properties carry this module, and each is asserted against something that would break in a way a
//! user sees, not merely a way a test notices:
//!
//! - **`seq` is 1-based and matches the log position.** It is the same number `session.page` calls its
//!   cursor and `session.follow` calls `seq`. If it were 0-based, every client resuming from a cursor would
//!   replay one event; if it restarted at 1 for a mid-log start, a client would drop the event it was
//!   waiting for. Both are tested by resuming and asserting the exact set of frames.
//! - **`kind` is the `serde` rename, not a second spelling.** A client dispatches on `kind`, so it is
//!   compared against the tag inside the serialized event for every variant this test constructs — a string
//!   that merely looks right is not evidence.
//! - **`None` is not terminal.** A live session is quiet between steps; a stream that latched on the first
//!   `None` would stop at the first pause. Reading after a `push` is tested directly.
//! - **Starting mid-log is a cursor, not a window.** `from_seq = n` must yield exactly the events after `n`,
//!   with no overlap and no gap; that is the one arithmetic error that is invisible until a client is
//!   resuming a real conversation.
//!
//! # Why the module is compiled by path
//!
//! The stream is not exported from the crate root yet; that wiring is a separate change to `lib.rs`.
//! Compiling the exact source file here — rather than a copy — keeps these tests exercising the same code a
//! consumer will reach through `girder_core::session_stream`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/session_stream.rs"]
mod session_stream;

use girder_core::{ChatRole, ContentBlock, SessionEvent, ToolCallId, TurnEndReason};

use session_stream::{EndReason, SessionStream};

// ---- fixtures ----------------------------------------------------------------------------------

/// A turn opening, the cheapest event that names a turn.
fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

/// A turn closing, which is what makes a stream report `Completed`.
fn turn_end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    }
}

/// A user prompt, which carries a message and therefore the most structure of any fixture here.
fn prompt(id: &str, text: &str) -> SessionEvent {
    SessionEvent::user_text(id, text)
}

/// A tool invocation, whose payload includes a nested id type.
fn tool_call(call: &str, name: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(call).expect("valid call id"),
        name: name.to_owned(),
        arguments: "{}".to_owned(),
    }
}

/// A tool result, whose payload includes a nested block enum.
fn tool_result(call: &str, text: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(call).expect("valid call id"),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        is_error: false,
    }
}

/// A three-event log: one turn that ran one tool and finished.
fn one_turn() -> Vec<SessionEvent> {
    vec![
        turn_start(1),
        prompt("m1", "fix the retry"),
        tool_call("c1", "bash"),
        tool_result("c1", "ok"),
        turn_end(1),
    ]
}

// ---- streaming from the beginning --------------------------------------------------------------

#[test]
fn streams_every_event_from_the_start() {
    let mut stream = SessionStream::new(one_turn(), 0);

    let first = stream.next_frame().expect("the log has events");
    assert_eq!(first.seq, 1, "the first event's seq is one, not zero");
    assert_eq!(first.kind, "turn/start");

    // Drain the rest and collect the sequence numbers, which is what a client's cursor is built from.
    let mut seqs = vec![first.seq];
    while let Some(frame) = stream.next_frame() {
        seqs.push(frame.seq);
    }
    assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    assert_eq!(stream.emitted(), 5);
}

#[test]
fn an_empty_log_yields_no_frames_and_reports_exhausted() {
    let mut stream = SessionStream::new(Vec::new(), 0);

    assert!(stream.next_frame().is_none());
    assert_eq!(stream.emitted(), 0);
    // Exhaustion, not cancellation: the caller must keep the subscription open for a session that has not
    // written anything yet.
    assert_eq!(stream.end_reason(), Some(EndReason::Exhausted));
}

#[test]
fn the_body_is_the_serialized_event_including_its_tag() {
    let mut stream = SessionStream::new(vec![prompt("m1", "hello")], 0);

    let frame = stream.next_frame().expect("one event");
    let expected = serde_json::to_value(prompt("m1", "hello")).expect("serializable");
    assert_eq!(frame.payload, expected);
    // The tag is in the payload, so a consumer that reads only the body can still tell what it has.
    assert_eq!(frame.payload["type"], "user/message");
    assert_eq!(frame.kind, "user/message");
}

// ---- streaming from the middle: the core property ----------------------------------------------

#[test]
fn resuming_from_a_cursor_repeats_nothing_and_skips_nothing() {
    let log = one_turn();

    // A client that has seen seq 2 asks for what came after it.
    let mut resumed = SessionStream::new(log.clone(), 2);
    let seqs: Vec<u64> = std::iter::from_fn(|| resumed.next_frame())
        .map(|frame| frame.seq)
        .collect();

    assert_eq!(seqs, vec![3, 4, 5], "the first frame is 3, not 2 or 4");
    assert_eq!(resumed.emitted(), 3);

    // Those frames are the same ones the full stream produced for those positions, not a re-sliced view: a
    // resume that rebuilt its payloads differently would make a client render one event two ways.
    let mut full = SessionStream::new(log, 0);
    let tail: Vec<_> = std::iter::from_fn(|| full.next_frame())
        .filter(|frame| frame.seq > 2)
        .collect();
    let mut resumed_all = SessionStream::new(one_turn(), 2);
    let resumed_frames: Vec<_> = std::iter::from_fn(|| resumed_all.next_frame()).collect();
    assert_eq!(tail, resumed_frames);
}

#[test]
fn a_cursor_past_the_end_is_clamped_not_rejected() {
    // A cursor ahead of the log is "nothing new", which is the normal answer after a resume from a stale
    // snapshot. It must not panic, and it must not invent a seq the log does not have.
    let mut stream = SessionStream::new(one_turn(), 99);
    assert!(stream.next_frame().is_none());
    assert_eq!(stream.emitted(), 0);

    // A push afterwards continues at the log's own next position, not at the caller's bad cursor.
    stream.push(turn_start(2));
    let frame = stream.next_frame().expect("the pushed event");
    assert_eq!(
        frame.seq, 6,
        "seq is the log position, never the caller's number"
    );
}

#[test]
fn a_cursor_exactly_at_the_end_yields_only_what_is_pushed_later() {
    let mut stream = SessionStream::new(one_turn(), 5);
    assert!(stream.next_frame().is_none());

    stream.push(prompt("m2", "one more"));
    assert_eq!(stream.next_frame().expect("the new event").seq, 6);
    assert!(stream.next_frame().is_none());
}

// ---- seq monotonicity --------------------------------------------------------------------------

#[test]
fn seq_increases_by_exactly_one_per_frame() {
    let mut stream = SessionStream::new(
        vec![
            turn_start(1),
            prompt("m1", "a"),
            tool_call("c1", "bash"),
            tool_result("c1", "ok"),
            turn_end(1),
            turn_start(2),
        ],
        0,
    );

    let mut previous = 0;
    while let Some(frame) = stream.next_frame() {
        assert_eq!(
            frame.seq,
            previous + 1,
            "a gap or a repeat would make a client duplicate or drop an event"
        );
        previous = frame.seq;
    }
    assert_eq!(previous, 6);
}

#[test]
fn seq_stays_contiguous_across_a_push() {
    // The interesting case for monotonicity is not a static log but the seam between what was written and
    // what arrives later: a counter that restarted, or reused the last number, would look fine statically.
    let mut stream = SessionStream::new(vec![turn_start(1), prompt("m1", "a")], 0);
    let first: Vec<u64> = std::iter::from_fn(|| stream.next_frame())
        .map(|frame| frame.seq)
        .collect();
    assert_eq!(first, vec![1, 2]);

    stream.push(tool_call("c1", "bash"));
    stream.push(turn_end(1));

    let second: Vec<u64> = std::iter::from_fn(|| stream.next_frame())
        .map(|frame| frame.seq)
        .collect();
    assert_eq!(second, vec![3, 4]);
    assert_eq!(stream.emitted(), 4);
}

// ---- kind agrees with the serde rename ---------------------------------------------------------

#[test]
fn kind_is_the_serde_rename_for_every_variant() {
    // Built from the real constructors and compared against the encoding, so a variant whose rename changes
    // fails here rather than at a client. `to_value` is the authority: `kind` is supposed to be read from it.
    let log = vec![
        turn_start(1),
        prompt("m1", "hi"),
        tool_call("c1", "bash"),
        tool_result("c1", "ok"),
        turn_end(1),
    ];
    let expected = [
        "turn/start",
        "user/message",
        "tool/call",
        "tool/result",
        "turn/end",
    ];

    let mut stream = SessionStream::new(log, 0);
    for want in expected {
        let frame = stream.next_frame().expect("a frame per event");
        assert_eq!(frame.kind, want);
        assert_eq!(
            frame.payload["type"], want,
            "kind must be spelled exactly as the serialized tag"
        );
    }
}

#[test]
fn kind_uses_a_slash_not_a_kebab_case_hyphen() {
    // The distinction that would break dispatch silently: `kebab-case` would render "turn-start" and every
    // client matching "turn/start" would drop the frame into its unknown branch.
    let mut stream = SessionStream::new(vec![turn_start(1)], 0);
    let frame = stream.next_frame().expect("one event");
    assert!(frame.kind.contains('/'), "the vocabulary uses a slash");
    assert!(
        !frame.kind.contains('-'),
        "kebab-case is not the wire format"
    );
}

// ---- a pushed event revives the stream ---------------------------------------------------------

#[test]
fn pushing_after_exhaustion_makes_more_frames_available() {
    let mut stream = SessionStream::new(vec![turn_start(1)], 0);
    assert!(stream.next_frame().is_some());
    assert!(stream.next_frame().is_none());

    // This is what a live session looks like between steps: the log did not end, it was only caught up.
    stream.push(prompt("m1", "still here"));
    let frame = stream.next_frame().expect("the pushed event is readable");
    assert_eq!(frame.seq, 2);
    assert_eq!(frame.kind, "user/message");
    assert_eq!(stream.emitted(), 2);
    assert!(stream.next_frame().is_none());
}

#[test]
fn a_stream_that_starts_empty_can_grow() {
    let mut stream = SessionStream::new(Vec::new(), 0);
    assert_eq!(stream.end_reason(), Some(EndReason::Exhausted));

    stream.push(turn_start(1));
    assert_eq!(
        stream.end_reason(),
        None,
        "new work means it is no longer ended"
    );
    assert_eq!(stream.next_frame().expect("visible now").seq, 1);
}

// ---- the end reason ----------------------------------------------------------------------------

#[test]
fn a_log_that_ends_on_turn_end_reports_completed() {
    let mut stream = SessionStream::new(one_turn(), 0);
    while stream.next_frame().is_some() {}
    assert_eq!(stream.end_reason(), Some(EndReason::Completed));
}

#[test]
fn a_log_that_stops_mid_turn_reports_exhausted() {
    // A turn still running is not a finished session; a client that treated it as complete would close the
    // subscription exactly when the user is watching it.
    let mut stream = SessionStream::new(vec![turn_start(1), prompt("m1", "a")], 0);
    while stream.next_frame().is_some() {}
    assert_eq!(stream.end_reason(), Some(EndReason::Exhausted));
}

#[test]
fn cancellation_is_terminal_and_outranks_the_log() {
    let mut stream = SessionStream::new(one_turn(), 0);
    assert_eq!(stream.next_frame().expect("one event").seq, 1);

    stream.cancel();
    assert_eq!(stream.end_reason(), Some(EndReason::Cancelled));
    assert!(stream.next_frame().is_none());

    // Unlike exhaustion, new events do not revive a cancelled stream: the consumer stopped waiting.
    stream.push(prompt("m1", "ignored"));
    assert!(stream.next_frame().is_none());
    assert_eq!(stream.emitted(), 1);
}

#[test]
fn end_reason_is_none_while_frames_remain() {
    let mut stream = SessionStream::new(one_turn(), 0);
    assert_eq!(stream.end_reason(), None);
    assert!(stream.next_frame().is_some());
    assert_eq!(stream.end_reason(), None, "four events are still unread");
}

// ---- the emitted counter -----------------------------------------------------------------------

#[test]
fn emitted_counts_frames_delivered_not_log_positions() {
    // Starting mid-log is the case that separates the two: the first frame is seq 3 but only the first frame
    // this stream produced.
    let mut stream = SessionStream::new(one_turn(), 2);
    assert_eq!(stream.emitted(), 0, "construction emits nothing");

    let frame = stream.next_frame().expect("the event after seq 2");
    assert_eq!(frame.seq, 3);
    assert_eq!(stream.emitted(), 1);

    while stream.next_frame().is_some() {}
    assert_eq!(stream.emitted(), 3);
    // Reading past the end must not inflate the count: it is frames delivered, not calls made.
    assert!(stream.next_frame().is_none());
    assert_eq!(stream.emitted(), 3);
}

#[test]
fn emitted_does_not_count_pushes() {
    let mut stream = SessionStream::new(vec![turn_start(1)], 0);
    stream.push(prompt("m1", "queued"));
    assert_eq!(
        stream.emitted(),
        0,
        "a pushed event is not a delivered frame"
    );

    let _ = stream.next_frame();
    assert_eq!(stream.emitted(), 1);
}

#[test]
fn emitting_the_same_log_twice_gives_equal_frames() {
    // Determinism: a replay and a live read of the same events must produce byte-identical frames, or a client
    // that caches by seq would show two different renderings of one event.
    let log = one_turn();
    let mut left = SessionStream::new(log.clone(), 0);
    let mut right = SessionStream::new(log, 0);

    while let Some(frame) = left.next_frame() {
        assert_eq!(Some(frame), right.next_frame());
    }
    assert!(right.next_frame().is_none());
}

// ---- the payload survives the round trip -------------------------------------------------------

#[test]
fn a_frame_payload_decodes_back_into_the_event() {
    let event = tool_result("c1", "ok");
    let mut stream = SessionStream::new(vec![event.clone()], 0);

    let frame = stream.next_frame().expect("one event");
    let decoded: SessionEvent = serde_json::from_value(frame.payload).expect("a decodable body");
    assert_eq!(decoded, event);
}

#[test]
fn a_message_event_keeps_its_role_through_the_frame() {
    // A payload that merely parses is not enough: the body must carry the fields a renderer reads. The role is
    // the one a client branches on to choose a bubble.
    let mut stream = SessionStream::new(vec![prompt("m1", "hello")], 0);
    let frame = stream.next_frame().expect("one event");

    let decoded: SessionEvent = serde_json::from_value(frame.payload).expect("a decodable body");
    let SessionEvent::UserMessage { message } = decoded else {
        panic!("a prompt must decode to user/message");
    };
    assert_eq!(message.role, ChatRole::User);
    assert_eq!(message.content.len(), 1);
}
