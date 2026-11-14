//! Tests for `terminal.attach` served as a begin-and-poll method.
//!
//! The properties pinned here are the ones a client of a live terminal pane depends on, and most of
//! them are about facts that are easy to get subtly wrong once the answer is an array instead of a
//! stream:
//!
//! - **Attaching hands back a usable handle.** The viewer id is the only way to ask for output, so a
//!   response whose id addresses nothing would make every later call answer nothing and look like an
//!   idle terminal.
//! - **Polling returns what the terminal printed.** This is the whole point of the exchange; a client
//!   that never receives bytes has an attachment that only consumes state.
//! - **An empty poll is not an ending.** A shell at a prompt is silent for minutes. A test that only
//!   checked "empty means empty" would pass against an implementation that also treated it as closed,
//!   so the assertion is that output arriving *after* an empty poll is still delivered.
//! - **`limit` bounds a call.** A terminal that printed while the client was away accumulates frames,
//!   and one call must not be obliged to return all of them.
//! - **Detaching stops delivery and nothing else.** The terminal keeps running, and a viewer attached
//!   afterwards still joins at the right offset.
//! - **Two viewers do not share a cursor.** One pane polling must not decide what another pane sees.
//! - **Offsets survive a reconnection.** A client resumes from the offset it saved, which is only
//!   meaningful if a new attachment reports a position comparable with the last frame's `to`.
//!
//! # Why the module is compiled by path
//!
//! `terminal_attach_rpc` is not yet declared in the crate root; mounting the method name is a
//! separate change. Compiling this exact source file — rather than a copy — keeps these tests
//! exercising the same code a consumer will reach through the crate root, and makes the suite fail if
//! the module stops being self-contained.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/terminal_attach_rpc.rs"]
mod terminal_attach_rpc;

use girder_core::TerminalFanout as Fanout;

use terminal_attach_rpc::{NO_VIEWER, attach, detach, poll};

// ---- fixtures ----------------------------------------------------------------------------------

/// A fanout for one terminal, which is what a host holds per open terminal.
fn fanout(id: &str) -> Fanout {
    Fanout::new(id)
}

// ---- attaching ---------------------------------------------------------------------------------

#[test]
fn attach_returns_a_viewer_that_can_be_polled() {
    // The id is the handle for the rest of the attachment. Asserting only "not zero" would pass for
    // an id the fanout had never handed out, so the id is used for a call that requires it to exist.
    let mut fans = fanout("terminal-1");
    fans.broadcast("$ ");

    let response = attach(&mut fans, "terminal-1");
    assert_ne!(response.viewer, NO_VIEWER);
    assert_eq!(response.from, 2, "a viewer joins at the stream's end");

    // The viewer really is attached: it is in the roster, and polling it is a call the fanout serves
    // rather than one it silently ignores.
    assert_eq!(fans.attached(), 1);
    assert!(poll(&mut fans, response.viewer, 10).is_empty());

    fans.broadcast("ls\n");
    let frames = poll(&mut fans, response.viewer, 10);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].text, "ls\n");
}

#[test]
fn attaching_to_a_different_terminal_attaches_nothing() {
    // A fanout holds one terminal's stream. Serving another terminal's ids through it would make
    // every offset the client saves point into the wrong stream, so the mismatch is refused instead
    // of being resolved to whatever the fanout already holds.
    let mut fans = fanout("terminal-1");
    fans.broadcast("from terminal 1");

    let response = attach(&mut fans, "terminal-2");
    assert_eq!(response.viewer, NO_VIEWER);
    assert_eq!(response.from, 0);
    assert_eq!(fans.attached(), 0, "nothing was attached");
}

#[test]
fn each_attach_hands_out_its_own_viewer() {
    // Two panes on one terminal are the case the fanout exists for; if the second attach returned the
    // first id, the two would share a cursor and one would starve the other.
    let mut fans = fanout("terminal-1");

    let first = attach(&mut fans, "terminal-1");
    let second = attach(&mut fans, "terminal-1");

    assert_ne!(first.viewer, NO_VIEWER);
    assert_ne!(second.viewer, NO_VIEWER);
    assert_ne!(first.viewer, second.viewer);
    assert_eq!(fans.attached(), 2);
}

// ---- polling -----------------------------------------------------------------------------------

#[test]
fn poll_returns_what_the_terminal_printed() {
    // The core property: the bytes the host broadcast are the bytes the client receives, in the same
    // order, with the offsets that place them in the stream.
    let mut fans = fanout("terminal-1");
    let response = attach(&mut fans, "terminal-1");
    assert_eq!(response.from, 0);

    fans.broadcast("hello ");
    fans.broadcast("world");

    let frames = poll(&mut fans, response.viewer, 10);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].text, "hello world");
    assert_eq!(frames[0].from, response.from);
    assert_eq!(frames[0].to, 11);
}

#[test]
fn an_empty_poll_is_a_pause_and_not_an_ending() {
    // The assertion that matters is the second one: output broadcast *after* an empty answer is still
    // delivered. A test that stopped at "the empty answer was empty" would pass against an
    // implementation that had closed the viewer, which is exactly the bug a shell at a prompt would
    // expose in production.
    let mut fans = fanout("terminal-1");
    let response = attach(&mut fans, "terminal-1");

    assert!(poll(&mut fans, response.viewer, 10).is_empty());

    fans.broadcast("later");
    let frames = poll(&mut fans, response.viewer, 10);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].text, "later");
}

#[test]
fn polling_twice_does_not_repeat_output() {
    // The offset is a cursor, not a replay button. A second poll with nothing new has nothing to say,
    // which is what stops a client from appending the same text to a pane twice.
    let mut fans = fanout("terminal-1");
    let response = attach(&mut fans, "terminal-1");
    fans.broadcast("once");

    let first = poll(&mut fans, response.viewer, 10);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].text, "once");
    assert!(poll(&mut fans, response.viewer, 10).is_empty());
}

#[test]
fn polling_a_viewer_that_was_never_attached_returns_nothing() {
    // A stale or invented id is a caller's mistake, not an ending, and the two calls that could report
    // it differently are poll and detach: an empty answer costs a client nothing, whereas an error
    // type would have to be agreed with the transport.
    let mut fans = fanout("terminal-1");
    fans.broadcast("output nobody watches");

    assert!(poll(&mut fans, NO_VIEWER, 10).is_empty());
    assert!(poll(&mut fans, 99, 10).is_empty());
}

#[test]
fn a_fast_terminal_does_not_outrun_the_limit() {
    // A viewer that was away while the terminal printed must not be obliged to receive everything in
    // one answer. The bound is on the call, so the check is that no answer exceeds it — not that the
    // fanout produces many small frames, which is a different design.
    let mut fans = fanout("terminal-1");
    let response = attach(&mut fans, "terminal-1");
    for line in 0..50 {
        fans.broadcast(&format!("line {line}\n"));
    }

    let frames = poll(&mut fans, response.viewer, 1);
    assert!(frames.len() <= 1);
    assert_eq!(frames.len(), 1);
}

#[test]
fn a_limit_of_zero_asks_without_consuming() {
    // "Is there output?" is a question a client may want to ask before committing to render it. A zero
    // limit that consumed the backlog would answer it by destroying the answer.
    let mut fans = fanout("terminal-1");
    let response = attach(&mut fans, "terminal-1");
    fans.broadcast("pending");

    assert!(poll(&mut fans, response.viewer, 0).is_empty());

    let frames = poll(&mut fans, response.viewer, 10);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].text, "pending");
}

// ---- detaching ---------------------------------------------------------------------------------

#[test]
fn detach_stops_delivery_and_reports_whether_it_happened() {
    // `false` for an id that was never handed out is what lets a client clean up after a dropped
    // connection without first knowing whether the connection had got as far as attaching.
    let mut fans = fanout("terminal-1");
    let response = attach(&mut fans, "terminal-1");
    fans.broadcast("before");

    assert!(detach(&mut fans, response.viewer));
    assert!(!detach(&mut fans, response.viewer), "already detached");
    assert!(!detach(&mut fans, NO_VIEWER));
    assert_eq!(fans.attached(), 0);
}

#[test]
fn a_detached_viewer_polls_nothing_and_the_terminal_keeps_its_offset() {
    // Detaching is an observation ending, not a process ending: the stream's end keeps advancing while
    // nobody watches, so a viewer attached afterwards joins where the stream has reached instead of
    // re-reading output it already missed.
    let mut fans = fanout("terminal-1");
    let response = attach(&mut fans, "terminal-1");
    fans.broadcast("abc");
    assert_eq!(poll(&mut fans, response.viewer, 10)[0].to, 3);

    assert!(detach(&mut fans, response.viewer));
    fans.broadcast("def");
    assert!(poll(&mut fans, response.viewer, 10).is_empty());

    let rejoined = attach(&mut fans, "terminal-1");
    assert_eq!(rejoined.from, 6);
}

// ---- several viewers ---------------------------------------------------------------------------

#[test]
fn two_viewers_each_receive_the_same_output_independently() {
    // Neither viewer may consume the other's bytes. Polling one must leave the other's backlog intact,
    // which is the property that makes two panes on one terminal possible at all.
    let mut fans = fanout("terminal-1");
    let first = attach(&mut fans, "terminal-1");
    let second = attach(&mut fans, "terminal-1");
    fans.broadcast("shared");

    let first_frames = poll(&mut fans, first.viewer, 10);
    assert_eq!(first_frames.len(), 1);
    assert_eq!(first_frames[0].text, "shared");

    let second_frames = poll(&mut fans, second.viewer, 10);
    assert_eq!(second_frames.len(), 1);
    assert_eq!(second_frames[0].text, "shared");
    assert_eq!(second_frames[0].from, first_frames[0].from);
    assert_eq!(second_frames[0].to, first_frames[0].to);
}

#[test]
fn a_detached_viewer_does_not_affect_the_one_still_watching() {
    // A pane that closes must not disturb the pane that stays open — the same reasoning as the
    // cursor independence above, with the cleanup path as the trigger.
    let mut fans = fanout("terminal-1");
    let leaving = attach(&mut fans, "terminal-1");
    let staying = attach(&mut fans, "terminal-1");

    assert!(detach(&mut fans, leaving.viewer));
    fans.broadcast("after the close");

    let frames = poll(&mut fans, staying.viewer, 10);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].text, "after the close");
}

// ---- reconnecting ------------------------------------------------------------------------------

#[test]
fn a_saved_offset_places_a_reconnected_viewer() {
    // This is what the offsets are for. A client saves the `to` of the last frame it rendered, comes
    // back later, and must be able to tell "nothing happened" from "output happened while I was away"
    // by comparing its saved number with the offset a fresh attachment reports. The two cases are
    // asserted separately because an implementation that always joined at zero would pass only the
    // second one, and one that always joined at the end would pass only the first.
    let mut fans = fanout("terminal-1");

    let first = attach(&mut fans, "terminal-1");
    fans.broadcast("prompt$ ");
    let frames = poll(&mut fans, first.viewer, 10);
    let saved = frames[0].to;
    assert_eq!(saved, 8);
    assert!(detach(&mut fans, first.viewer));

    // Quiet while the client was away: rejoining lands exactly on the saved offset.
    let quiet = attach(&mut fans, "terminal-1");
    assert_eq!(quiet.from, saved);

    // Output happened while the client was away: the new offset is past the saved one, and the
    // distance is the gap the transcript can still serve.
    fans.broadcast("unseen");
    let busy = attach(&mut fans, "terminal-1");
    assert_eq!(busy.from, saved + 6);
    assert!(busy.from > saved);
}

#[test]
fn a_reconnected_viewer_continues_without_repeating_the_past() {
    // Joining at the stream's end means the first new frame starts where the client stopped, so a
    // client that appends frames to one pane never renders a byte twice or renders the old prompt
    // again.
    let mut fans = fanout("terminal-1");

    let first = attach(&mut fans, "terminal-1");
    fans.broadcast("a");
    let first_frames = poll(&mut fans, first.viewer, 10);
    let saved = first_frames[0].to;
    assert!(detach(&mut fans, first.viewer));

    let second = attach(&mut fans, "terminal-1");
    assert_eq!(second.from, saved);
    fans.broadcast("b");

    let frames = poll(&mut fans, second.viewer, 10);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].text, "b");
    assert_eq!(frames[0].from, saved);
    assert_eq!(frames[0].to, saved + 1);
}
