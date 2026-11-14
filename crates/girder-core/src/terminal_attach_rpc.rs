//! `terminal.attach` as a begin-and-poll method, which is the only shape the transport can serve.
//!
//! [`Fanout`] owns the delivery half of watching a terminal: one cursor per viewer plus the output no
//! viewer has collected yet. It owns no wire shape, and a client cannot call a type. This module is
//! the wire shape — three calls that a request/response transport can answer — and nothing else: it
//! holds no process, no shell, no connection, and no output of its own.
//!
//! # Why this is begin-and-poll, and not a push
//!
//! Upstream declares `terminal.attach` as a duplex stream: frames out, keystrokes in, held open until
//! either side closes. [`crate::remote::RemoteRegistry`] has no stream mode — a handler resolves one
//! value and returns — so a method registered as `attach` that pushed frames cannot exist here. The
//! two possible dishonesties are to register `attach` as a normal method and answer it with one array
//! (a client opens a subscription and receives exactly one reply, then silence), or to leave it
//! declared as a gap forever (the delivery machinery exists and no client can reach it).
//!
//! This module takes the third option: the *client* drives the exchange, and the server keeps the
//! per-viewer state between calls. [`attach`] starts a viewer and answers where it joined, [`poll`]
//! hands back what arrived since the last poll, and [`detach`] ends the viewer. There is no
//! subscription a client waits on, because there is no connection that could be kept open. The
//! contract is stated as what it is so a client can be written against it; calling it a stream and
//! returning arrays would be a different protocol under the same name.
//!
//! # Why the viewer id is a handle the client must keep
//!
//! Output is per-viewer, not per-terminal: two panes on one terminal must not steal each other's
//! bytes, so [`Fanout`] hands every viewer its own cursor and its own backlog (see the module
//! documentation there for why). That makes a viewer an object the server holds and the client names,
//! and the only name is the id [`attach`] returns. A client that lost it cannot address its own
//! backlog — [`poll`] takes the id, not the terminal's — so the id is returned rather than inferred
//! from the connection. Ids start at 1, which is what makes [`NO_VIEWER`] usable as "no viewer".
//!
//! # Why an empty poll is not the end of the stream
//!
//! A shell sitting at a prompt prints nothing for minutes. If an empty answer meant "finished", a
//! client would close a pane that is still perfectly alive, and the pane would look dead exactly when
//! the terminal is idle. An empty array means only "nothing new *right now*" — the same meaning
//! `TerminalSession::read` gives to an empty chunk. Whether the terminal still exists is a question
//! for its status, which this module never inspects.
//!
//! # Why offsets, and why the client compares them
//!
//! Every position here is an absolute byte offset in the terminal's one output stream — the same
//! coordinates `terminal.read` uses. A client that saves the `to` of the last frame it received can
//! reconnect later and compare: a new attachment whose `from` equals the saved value missed nothing,
//! and one whose `from` is larger was away while output happened. Reading that missed range is
//! `terminal.read` from the saved offset, not a replay feature of this module, because the fanout
//! deliberately does not replay the past on attach (two panes opened minutes apart should both show
//! what happens next, not both scroll back).
//!
//! # What this module does not do
//!
//! It does not register the method. Mounting a name is a change to
//! [`crate::terminal_rpc::register_terminal_methods`] and to that module's declared gaps, which is a
//! separate step; this file compiles and is tested on its own so the reasoning above can be read next
//! to the code that implements it. It also does not feed output: the host that drives the terminal
//! calls [`Fanout::broadcast`] with what the process printed, and this module never invents a frame.

// `poll` and `detach` are the other two thirds of the begin-poll contract and have no caller yet: the registry mounts
// `attach`, and a follow-up poll needs a method of its own. They are exported rather than removed because the three
// functions are one wire shape -- deleting two would leave the third describing a lifecycle nobody can complete.
#![allow(dead_code)]

use girder_core::TerminalFanout as Fanout;

/// The viewer id that names no viewer, returned by [`attach`] when it attached nothing.
///
/// [`Fanout`] numbers real viewers from 1, so 0 can never name one. Returning a value rather than a
/// `Result` is not a loss of information for the caller: the id is the only handle, so a sentinel that
/// no call will accept ([`poll`] answers nothing for it, [`detach`] reports `false`) is exactly as
/// addressable as an error would be, and it keeps the three calls free of an error type the transport
/// would have to agree with.
pub const NO_VIEWER: u64 = 0;

/// What [`attach`] answers: the handle for the rest of the attachment, and where it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AttachResponse {
    /// The viewer id, which [`poll`] and [`detach`] take.
    ///
    /// The client must keep it: the server routes output by this id, so a client that dropped it has
    /// no way to ask for its own backlog. [`NO_VIEWER`] means nothing was attached.
    pub viewer: u64,

    /// The stream offset this viewer starts at, which is the end of the stream when it joined.
    ///
    /// Compare it with the `to` of the last frame received before disconnecting: equal means nothing
    /// was missed, larger means output happened in between and the gap `[saved, from)` is what the
    /// transcript holds.
    pub from: u64,
}

/// One piece of terminal output, with the two offsets that place it in the stream.
///
/// The offsets are the point of the type. `text` alone would be ambiguous across a reconnection — the
/// same bytes can arrive twice or not at all — while `[from, to)` names exactly which bytes these are
/// and what the client should save next.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TerminalFrame {
    /// The output, as the terminal printed it (always valid UTF-8).
    pub text: String,

    /// The stream offset of `text`'s first byte.
    ///
    /// Reported rather than assumed to be the viewer's saved position: a viewer that fell behind the
    /// backlog is resynchronised at the oldest byte still held, so a `from` larger than the saved
    /// offset is how the client notices that output was lost rather than merely delayed.
    pub from: u64,

    /// The stream offset just past `text`'s last byte, which is the value to save for a reconnection.
    pub to: u64,
}

/// Starts a viewer on the terminal `terminal_id`, which `fanout` must be the fanout for.
///
/// The fanout is per-terminal bookkeeping: it holds the stream's end offset and every viewer's
/// cursor. Handing a client the bytes of terminal B under terminal A's id would make every offset the
/// client saves meaningless, so the two are checked against each other here. A mismatch attaches
/// nothing and answers [`NO_VIEWER`] with `from` 0 — the caller's lookup of the fanout was keyed by
/// something other than the terminal's own identity, and no id this module hands out would fix that.
///
/// The viewer starts at the stream's end, not at its start: attaching is how a client sees what
/// happens *next*. A client that needs the output it missed reads it from the transcript
/// (`terminal.read`, same byte coordinates) and then attaches here to continue.
#[must_use]
pub fn attach(fanout: &mut Fanout, terminal_id: &str) -> AttachResponse {
    if fanout.terminal_id() != terminal_id {
        return AttachResponse {
            viewer: NO_VIEWER,
            from: 0,
        };
    }

    let (viewer, from) = fanout.attach();
    AttachResponse { viewer, from }
}

/// Takes up to `limit` frames of the output `viewer` has not received yet.
///
/// An empty answer is not an ending: it means the terminal printed nothing since the last poll, which
/// is the normal state of a shell waiting for input. The only thing that ends an attachment is
/// [`detach`] (or the host dropping the fanout).
///
/// `limit` caps how many frames one call returns, so a client that has been away cannot be handed an
/// unbounded batch. A `limit` of zero returns nothing and consumes nothing — the fanout is not polled
/// at all — which lets a caller ask "is there output?" without risking the loss of it.
///
/// The loop is written against "the fanout may return one frame per poll" rather than assuming a
/// single answer. Today [`Fanout::poll`] drains a viewer's whole backlog into one chunk, so in
/// practice the first iteration is also the last; splitting that chunk to fill the limit is not an
/// option, because this module keeps no per-viewer state of its own in which the remainder could wait,
/// and everything past a cut would be dropped. A fanout that later hands back one chunk per feed would
/// need no change here.
#[must_use]
pub fn poll(fanout: &mut Fanout, viewer: u64, limit: usize) -> Vec<TerminalFrame> {
    let mut frames = Vec::new();
    while frames.len() < limit {
        let Some(chunk) = fanout.poll(viewer) else {
            // Nothing new right now, or no such viewer. Neither is an ending, so the answer is the
            // frames collected so far rather than an error.
            break;
        };
        frames.push(TerminalFrame {
            text: chunk.text,
            from: chunk.from,
            to: chunk.to,
        });
    }
    frames
}

/// Stops watching, reporting whether `viewer` was attached.
///
/// The terminal is untouched: a viewer holds offsets and unpolled output, not a handle on the shell,
/// so detaching cannot end a process, and the fanout keeps counting offsets afterwards — a viewer
/// attached later starts where the stream has reached rather than re-reading a restarted stream.
///
/// `false` means the id was never handed out or was already detached. The two are deliberately
/// indistinguishable: a client cleaning up a dropped connection knows only that its id is no longer
/// live, and asking it to tell those apart would make duplicate cleanup an error.
#[must_use]
pub fn detach(fanout: &mut Fanout, viewer: u64) -> bool {
    fanout.detach(viewer)
}
