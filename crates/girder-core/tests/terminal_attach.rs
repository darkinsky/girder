//! Tests for push delivery of terminal output.
//!
//! The properties pinned here are the ones a viewer of a live terminal depends on, and three of them
//! are about what a viewer must *not* be able to do to anyone else. One viewer falling behind must
//! not slow another down, and must not slow the writer down. Detaching must not end the terminal.
//! And `None` from a poll must not read as "the terminal is gone", because a shell at a prompt is
//! silent for minutes at a time and a pane that closed on silence would be useless.
//!
//! The module is included by path rather than reached through the crate root: wiring
//! `pub mod terminal_attach;` belongs to the integrator, and a test that waited for that wiring could
//! not be run against the file as it is written. Compiling the file directly also means this suite
//! fails if the module stops being self-contained.

#[path = "../src/terminal_attach.rs"]
mod terminal_attach;

use terminal_attach::{Attachment, Chunk, DEFAULT_BACKLOG_BYTES, Fanout};

/// Attaches at offset zero.
fn attach(id: &str) -> Attachment {
    Attachment::new(id.to_owned(), 0)
}

/// Polls `viewer`, asserting that it had output, and returns the text.
fn expect_chunk(viewer: &mut Attachment) -> Chunk {
    match viewer.poll() {
        Some(chunk) => chunk,
        None => panic!("expected output, got a pause"),
    }
}

// ---- one viewer --------------------------------------------------------------------------------

#[test]
fn a_fed_attachment_returns_what_was_fed() {
    let mut viewer = attach("terminal-1");
    viewer.feed("hello ");

    let chunk = expect_chunk(&mut viewer);
    assert_eq!(chunk.text, "hello ");
    assert_eq!(chunk.from, 0);
    assert_eq!(chunk.to, 6);
    assert_eq!(viewer.offset(), 6);
}

#[test]
fn consecutive_feeds_arrive_as_one_chunk_ending_at_the_stream_end() {
    // A writer feeding byte by byte and a viewer polling once a frame is the common case. The viewer
    // should receive one chunk covering everything, ending at the stream's end, so it needs one
    // offset to save rather than one per feed.
    let mut viewer = attach("terminal-1");
    viewer.feed("a");
    viewer.feed("b");
    viewer.feed("c");

    let chunk = expect_chunk(&mut viewer);
    assert_eq!(chunk.text, "abc");
    assert_eq!(chunk.from, 0);
    assert_eq!(chunk.to, 3);
}

#[test]
fn polling_twice_does_not_repeat_output() {
    // The offset is a cursor, not a replay button: a second poll with no new output has nothing to
    // say, which is what stops a client from appending the same text twice.
    let mut viewer = attach("terminal-1");
    viewer.feed("once");

    let first = expect_chunk(&mut viewer);
    assert_eq!(first.text, "once");
    assert_eq!(viewer.offset(), 4);
    assert!(viewer.poll().is_none());
    assert_eq!(viewer.offset(), 4);
}

#[test]
fn an_empty_feed_is_not_output() {
    // An empty write may mean "the shell wrote nothing" or "a flush", and neither is a fact about the
    // stream. Advancing an offset by zero is harmless; returning a chunk with no text would make
    // every client special-case it.
    let mut viewer = attach("terminal-1");
    viewer.feed("");
    viewer.feed("x");
    viewer.feed("");

    let chunk = expect_chunk(&mut viewer);
    assert_eq!(chunk.text, "x");
    assert_eq!(chunk.from, 0);
    assert_eq!(chunk.to, 1);
    assert!(viewer.poll().is_none());
}

#[test]
fn offsets_count_bytes_so_they_can_be_saved_and_compared() {
    // Offsets have to mean the same thing on both sides of a connection. Bytes are the unit a client
    // can measure without agreeing on a decoder, so a multi-byte character advances the offset by
    // its encoded length rather than by one.
    let mut viewer = attach("terminal-1");
    viewer.feed("héllo→");

    let chunk = expect_chunk(&mut viewer);
    assert_eq!(chunk.text, "héllo→");
    assert_eq!(chunk.from, 0);
    assert_eq!(chunk.to, 9);
    assert_eq!(viewer.offset(), 9);
}

#[test]
fn the_terminal_id_is_carried_so_a_chunk_can_be_routed() {
    let viewer = Attachment::new("terminal-7".to_owned(), 0);
    assert_eq!(viewer.terminal_id(), "terminal-7");
}

// ---- `None` is a pause, not an ending ----------------------------------------------------------

#[test]
fn polling_a_silent_terminal_returns_none_and_stays_usable() {
    // The case this pins: a shell at a prompt prints nothing for minutes. `None` must transport "no
    // news", and the very next feed must still be delivered — an attachment that treated silence as
    // an ending would return `None` forever after.
    let mut viewer = attach("terminal-1");
    assert!(viewer.poll().is_none());
    assert_eq!(viewer.offset(), 0);

    viewer.feed("woke up\n");
    let chunk = expect_chunk(&mut viewer);
    assert_eq!(chunk.text, "woke up\n");
    assert_eq!(chunk.from, 0);
}

#[test]
fn silence_between_feeds_does_not_close_the_stream() {
    let mut viewer = attach("terminal-1");
    viewer.feed("first\n");
    assert_eq!(expect_chunk(&mut viewer).text, "first\n");

    // Two idle rounds, the way a pane's frame loop would poll it.
    assert!(viewer.poll().is_none());
    assert!(viewer.poll().is_none());

    viewer.feed("second\n");
    let chunk = expect_chunk(&mut viewer);
    assert_eq!(chunk.text, "second\n");
    assert_eq!(chunk.from, 6);
    assert_eq!(chunk.to, 13);
}

// ---- reconnection ------------------------------------------------------------------------------

#[test]
fn a_reconnecting_viewer_resumes_from_its_saved_offset() {
    // The whole point of exposing `offset`: a client that dropped its connection saves the number,
    // and a new attachment fed the transcript from there must continue without a gap or a repeat.
    let mut first = attach("terminal-1");
    first.feed("one\n");
    let chunk = expect_chunk(&mut first);
    assert_eq!(chunk.text, "one\n");
    let saved = first.offset();
    first.detach();

    let mut second = Attachment::new("terminal-1".to_owned(), saved);
    second.feed("two\n");
    let resumed = expect_chunk(&mut second);
    assert_eq!(resumed.text, "two\n");
    assert_eq!(resumed.from, saved);
    assert_eq!(resumed.to, 8);
}

#[test]
fn a_viewer_started_from_the_end_sees_only_what_comes_next() {
    // `Fanout::attach`'s offset is the stream end. A viewer attached at it is current: it must not
    // receive output that was broadcast before it arrived, which is why `feed` is only called with
    // output at or after the starting offset.
    let mut viewer = Attachment::new("terminal-1".to_owned(), 100);
    viewer.feed("later");

    let chunk = expect_chunk(&mut viewer);
    assert_eq!(chunk.text, "later");
    assert_eq!(chunk.from, 100);
    assert_eq!(chunk.to, 105);
}

// ---- the backlog bound -------------------------------------------------------------------------

#[test]
fn a_viewer_that_never_polls_loses_its_oldest_output_and_keeps_the_newest() {
    // Holding everything for a pane nobody watches is a leak. Dropping everything is worse: `poll`
    // would answer `None`, which the viewer reads as "idle" exactly when it has lost output. So the
    // oldest is dropped and the newest kept, and the offset jump is what tells the viewer.
    let mut viewer = Attachment::with_backlog("terminal-1".to_owned(), 0, 4);
    viewer.feed("aaaa");
    viewer.feed("bbbb");
    viewer.feed("cccc");

    assert!(viewer.pending_bytes() <= 8, "the backlog is bounded");
    let chunk = expect_chunk(&mut viewer);
    assert!(chunk.text.ends_with("cccc"), "the newest output survives");
    assert!(!chunk.text.starts_with("aaaa"), "the oldest output is gone");
    assert_eq!(chunk.from, viewer.offset() - 4);
    assert_eq!(chunk.to, 12);
}

#[test]
fn a_viewer_that_polls_each_round_never_loses_a_byte() {
    // The bound is a ceiling on waste, not on delivery: a viewer polling faster than the writer
    // writes sees every byte, even with a bound far smaller than the total.
    let mut viewer = Attachment::with_backlog("terminal-1".to_owned(), 0, 4);
    let mut seen = String::new();
    for round in 0..32 {
        viewer.feed(if round % 2 == 0 { "ab" } else { "cd" });
        seen.push_str(&expect_chunk(&mut viewer).text);
    }
    assert_eq!(seen.len(), 64);
    assert_eq!(viewer.offset(), 64);
}

#[test]
fn a_zero_backlog_still_delivers_the_most_recent_feed() {
    // A zero bound is a configuration mistake, not a demand for a viewer that sees nothing. Treating
    // it as "hold one byte" keeps the attachment useful instead of silently empty.
    let mut viewer = Attachment::with_backlog("terminal-1".to_owned(), 0, 0);
    viewer.feed("abc");

    let chunk = expect_chunk(&mut viewer);
    assert_eq!(chunk.text, "abc");
}

#[test]
fn the_default_backlog_is_a_real_bound() {
    // The constant is part of the public surface, so it must describe memory, not a token gesture.
    assert_eq!(DEFAULT_BACKLOG_BYTES, 256 * 1024);
}

// ---- detaching ---------------------------------------------------------------------------------

#[test]
fn detaching_releases_the_backlog_without_touching_offsets() {
    let mut viewer = Attachment::new("terminal-1".to_owned(), 0);
    viewer.feed("unread");
    assert_eq!(viewer.pending_bytes(), 6);
    let saved = viewer.offset();

    viewer.detach();
    // The value is gone with the attachment; what a client keeps is the offset, which this test
    // asserted is the position *before* the unpolled output, so a reconnect re-delivers it.
    assert_eq!(saved, 0);
}

// ---- fanout: many viewers ----------------------------------------------------------------------

#[test]
fn broadcast_reaches_every_attached_viewer() {
    let mut fanout = Fanout::new("terminal-1");
    let (a, from_a) = fanout.attach();
    let (b, from_b) = fanout.attach();
    assert_eq!(fanout.attached(), 2);
    assert_eq!(from_a, 0);
    assert_eq!(from_b, 0);

    fanout.broadcast("shared\n");

    assert_eq!(fanout.poll(a).map(|c| c.text), Some("shared\n".to_owned()));
    assert_eq!(fanout.poll(b).map(|c| c.text), Some("shared\n".to_owned()));
}

#[test]
fn each_viewer_has_its_own_offset() {
    // The core property of a fanout. One viewer polling between two broadcasts must end up at the
    // same place as one that polled once at the end, and neither may be given the other's bytes
    // twice or out of order.
    let mut fanout = Fanout::new("terminal-1");
    let (fast, _) = fanout.attach();
    let (slow, _) = fanout.attach();

    fanout.broadcast("first\n");
    assert_eq!(
        fanout.poll(fast).map(|c| c.text),
        Some("first\n".to_owned())
    );

    fanout.broadcast("second\n");
    assert_eq!(
        fanout.poll(fast).map(|c| c.text),
        Some("second\n".to_owned())
    );
    assert_eq!(
        fanout.poll(slow).map(|c| c.text),
        Some("first\nsecond\n".to_owned()),
        "the viewer that waited gets the backlog in order, as one chunk"
    );

    assert!(fanout.poll(slow).is_none());
    assert!(fanout.poll(fast).is_none());
}

#[test]
fn a_slow_viewer_does_not_hold_back_a_fast_one() {
    // The reason a fanout exists rather than one shared attachment. A viewer that never polls must
    // not delay, coalesce, or duplicate what another viewer receives.
    let mut fanout = Fanout::new("terminal-1");
    let (fast, _) = fanout.attach();
    let (slow, _) = fanout.attach();

    for index in 0..100 {
        fanout.broadcast(&format!("{index}\n"));
        let chunk = fanout.poll(fast).expect("the fast viewer has output");
        assert_eq!(
            chunk.text,
            format!("{index}\n"),
            "the fast viewer gets exactly the round it asked for, whatever the slow one is doing"
        );
    }

    // The slow viewer now receives everything, in order, in one chunk — it lost nothing.
    let backlog = fanout.poll(slow).expect("the slow viewer has its backlog");
    assert!(backlog.text.starts_with("0\n1\n"));
    assert!(backlog.text.ends_with("99\n"));
    assert_eq!(backlog.to, fanout.offset());
}

#[test]
fn a_viewer_attached_later_starts_at_the_current_offset() {
    // A second pane opening mid-stream should watch the present, not scroll back through history it
    // can read from the transcript. Its first chunk therefore starts where it attached.
    let mut fanout = Fanout::new("terminal-1");
    let (early, _) = fanout.attach();

    fanout.broadcast("before\n");
    assert_eq!(fanout.offset(), 7);

    let (late, from_late) = fanout.attach();
    assert_eq!(from_late, 7);

    fanout.broadcast("after\n");
    let late_chunk = fanout.poll(late).expect("the late viewer has output");
    assert_eq!(late_chunk.text, "after\n");
    assert_eq!(late_chunk.from, 7);

    let early_chunk = fanout.poll(early).expect("the early viewer has output");
    assert_eq!(early_chunk.text, "before\nafter\n");
}

#[test]
fn attaching_hands_out_distinct_ids() {
    let mut fanout = Fanout::new("terminal-1");
    let (first, _) = fanout.attach();
    let (second, _) = fanout.attach();
    let (third, _) = fanout.attach();
    assert_ne!(first, second);
    assert_ne!(second, third);
    assert_ne!(first, third);
    assert_eq!(fanout.attached(), 3);
    assert_ne!(first, 0, "zero is left free to mean `no viewer`");
}

#[test]
fn detaching_one_viewer_leaves_the_others_and_the_terminal_alone() {
    // Detach is an observation ending, not a process ending: the stream keeps advancing, the other
    // viewers keep receiving, and a viewer can reattach where the stream now is.
    let mut fanout = Fanout::new("terminal-1");
    let (leaving, _) = fanout.attach();
    let (staying, _) = fanout.attach();

    assert!(fanout.detach(leaving));
    assert_eq!(fanout.attached(), 1);
    assert!(
        fanout.poll(leaving).is_none(),
        "a detached viewer has nothing to read"
    );

    fanout.broadcast("still running\n");
    assert_eq!(
        fanout.poll(staying).map(|c| c.text),
        Some("still running\n".to_owned())
    );
    assert_eq!(fanout.offset(), 14, "the stream did not restart");
}

#[test]
fn detaching_every_viewer_does_not_end_the_stream() {
    // The last viewer leaving is the ordinary end of a browser session. The terminal keeps its
    // offsets, so the next viewer attaches where the stream is instead of re-reading a stream that
    // started over — which would make a saved offset meaningless.
    let mut fanout = Fanout::new("terminal-1");
    let (only, _) = fanout.attach();
    fanout.broadcast("output\n");
    assert!(fanout.detach(only));
    assert_eq!(fanout.attached(), 0);

    fanout.broadcast("while nobody watched\n");
    assert_eq!(fanout.offset(), 28);

    let (next, from_next) = fanout.attach();
    assert_eq!(from_next, 28);
    assert!(fanout.poll(next).is_none(), "it attached at the stream end");
}

#[test]
fn detaching_twice_reports_the_second_as_absent() {
    // A dropped connection and an explicit close can both fire for the same viewer. The second must
    // be a plain `false` rather than a silent removal of somebody else's id.
    let mut fanout = Fanout::new("terminal-1");
    let (id, _) = fanout.attach();
    assert!(fanout.detach(id));
    assert!(!fanout.detach(id));
    assert!(!fanout.detach(999));
}

#[test]
fn polling_an_unknown_viewer_returns_none() {
    let mut fanout = Fanout::new("terminal-1");
    assert!(fanout.poll(42).is_none());
}

#[test]
fn an_empty_broadcast_moves_nobody() {
    let mut fanout = Fanout::new("terminal-1");
    let (id, _) = fanout.attach();
    fanout.broadcast("");
    assert_eq!(fanout.offset(), 0);
    assert!(fanout.poll(id).is_none());

    fanout.broadcast("x");
    assert_eq!(fanout.poll(id).map(|c| c.to), Some(1));
}

#[test]
fn a_fanout_with_no_viewers_still_counts_its_stream() {
    // Offsets belong to the terminal, not to the viewers: counting only while somebody watches would
    // make the same byte have two offsets depending on when it was printed, and break every saved
    // offset across a reconnect.
    let mut fanout = Fanout::new("terminal-1");
    fanout.broadcast("nobody\n");
    assert_eq!(fanout.offset(), 7);
    assert_eq!(fanout.attached(), 0);
    assert_eq!(fanout.terminal_id(), "terminal-1");
}

#[test]
fn a_fanout_viewer_that_stops_polling_does_not_stall_the_broadcast() {
    // The property in one place: broadcasting never consults a viewer's position, so a viewer that
    // stopped reading cannot make the writer wait. Its own backlog is the only thing that grows, and
    // it is bounded by the attachment's capacity.
    let mut fanout = Fanout::new("terminal-1");
    let (watcher, _) = fanout.attach();
    let (_gone, _) = fanout.attach();

    for _ in 0..1_000 {
        fanout.broadcast("0123456789\n");
        assert!(fanout.poll(watcher).is_some());
    }
    assert_eq!(fanout.offset(), 11_000);
}
