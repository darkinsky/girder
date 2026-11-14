//! Push delivery of a terminal's output to one observer, or to many at once.
//!
//! `terminal.read` asks the terminal for its next chunk and returns when that chunk arrives or the
//! wait expires: the *client* decides when to ask, and pays a round trip for every ask. An attached
//! client is the other shape — it subscribes once and is handed output as the terminal prints it, so
//! a pane redraws when a byte exists instead of once per poll interval. The two are not the same
//! contract under different names. A polling client cannot miss anything but is always behind by up
//! to one interval; an attached client is current but has to be told what to do when it falls behind.
//! This module supplies the delivery half of that second contract and nothing else: it holds no
//! process, no shell, and no transport.
//!
//! # Why offsets, and why they count bytes
//!
//! The terminal's output is a stream, not a document: `terminal.read` hands back the next piece and
//! a client that disconnects needs to say where it stopped. Every position here is therefore an
//! absolute byte offset in that one stream, the same numbers the transcript uses, so a saved offset
//! means the same thing to an attachment, to the transcript, and to a client that reconnects later.
//! Counting bytes rather than characters is what makes them comparable: a character count depends on
//! how text is decoded, and decoding on two sides of a connection can disagree.
//!
//! # Why `poll` returning `None` is not the end of the stream
//!
//! A shell sitting at a prompt prints nothing for minutes at a time. If `None` meant "finished", a
//! viewer would close a pane that is still perfectly alive, and the terminal would look dead exactly
//! when it is idle. `None` here means only "nothing new *right now*" — the same meaning
//! `TerminalSession::read` gives to an empty answer. Whether the terminal still exists is a question
//! for its status ([`crate::terminal::Status`]), not for the attachment, which never learns that the
//! process ended: the host stops feeding it and says so out of band.
//!
//! # Why each viewer keeps its own cursor
//!
//! Two panes can show one terminal — a browser tab and an editor, or two tabs of one session. If they
//! shared a cursor, the slower pane would decide how far the faster one could read, and a pane that
//! stopped polling would silently make the other one stall. Every viewer here therefore owns a cursor
//! and the output it has not polled yet. A viewer that never polls costs its own bounded backlog and
//! nothing else: the writer never waits, and a viewer that polls after every byte gets single-byte
//! chunks while one that polls once gets all of it in a single chunk.
//!
//! # Why a viewer that falls behind is resynchronised instead of blocking the writer
//!
//! Holding everything forever for a pane nobody is watching is a memory leak with a plausible excuse.
//! The backlog is therefore bounded ([`DEFAULT_BACKLOG_BYTES`], or whatever a host passes to
//! [`Attachment::with_backlog`]), and the oldest unpolled output is dropped when the bound is passed.
//! [`Chunk`] carries no `truncated` flag, so the loss is signalled by the offsets instead: the `from`
//! of the next chunk is larger than the offset the viewer had saved, which is exactly the fact a
//! client needs to resynchronise, and it cannot be confused with "nothing new" because that answer is
//! `None` rather than an offset jump.
//!
//! # Why detaching is not closing
//!
//! An attachment is an observation, not ownership. It holds an id, a cursor, and a copy of unpolled
//! output; it holds no handle on the shell, so nothing in this module can end a process. Closing the
//! terminal, when a client wants that, is `terminal.close`; detaching is how a client stops watching
//! without stopping what it watched.
//!
//! # Why this module is not built on `TerminalBuffer`
//!
//! The bounded buffer in this crate is read-by-offset over one retained window: `read(from)` is
//! answered from the window at a caller-supplied position, and the buffer keeps no idea of *who* is
//! reading. An attachment needs exactly that missing half — a cursor per viewer plus the unpolled
//! output that belongs to it — and a fanout needs the write side that hands the same bytes to every
//! viewer. Reusing the buffer would provide the window but not the cursors, and its own module would
//! still have to be wired through the crate root. Keeping this file self-contained also means its
//! test can compile it alone, the way the buffer's test does, so the file cannot quietly grow a
//! dependency on the rest of the crate.

use std::collections::{BTreeMap, VecDeque};

/// How much unpolled output one viewer may fall behind by before the oldest of it is dropped.
///
/// The same order of magnitude as the transcript a deployment retains by default, because a viewer
/// that is further behind than the transcript is behind by more than the host could still show it.
/// A viewer only reaches this bound by not polling, so the size is a ceiling on wasted memory, not a
/// target.
pub const DEFAULT_BACKLOG_BYTES: usize = 256 * 1024;

/// A run of output held for a viewer that has not polled it yet.
///
/// Stored in the chunks it arrived in rather than as one string so dropping old output is a pop from
/// the front, and so the remaining text is never re-copied to make room.
#[derive(Debug)]
struct Pending {
    /// The absolute offset of `text`'s first byte.
    ///
    /// Tracked per entry rather than recomputed from the lengths before it, because an entry that is
    /// dropped would otherwise shift every offset after it.
    start: u64,
    /// The output as it was written: never cut, so every offset here is a character boundary and the
    /// text a viewer receives is text the terminal actually printed.
    text: String,
}

/// One block of output handed to a viewer.
///
/// The two offsets are the point of the type: `text` is only meaningful together with where it sits
/// in the stream, and a client that stores `to` can be handed the next block without a gap or an
/// overlap. `from` is reported rather than assumed to be the viewer's saved position because a
/// viewer that fell behind the bound is resynchronised at the oldest byte still held, and it has to
/// be able to notice that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The output, as valid UTF-8.
    pub text: String,
    /// The stream offset of `text`'s first byte.
    pub from: u64,
    /// The stream offset just past `text`'s last byte, which is also the offset to save next.
    pub to: u64,
}

/// One observer of a terminal's output.
///
/// Attach, feed, poll, and keep the offset: an attachment is a position in a terminal's byte stream
/// plus the output that arrived after that position and has not been polled. It never reaches into
/// the terminal — the host feeds it what the terminal printed — which is what lets several of them
/// observe one terminal without sharing state.
#[derive(Debug)]
pub struct Attachment {
    /// The terminal being watched, kept so a chunk can be routed without a side table.
    terminal_id: String,
    /// The offset of the next byte this viewer has not been handed.
    cursor: u64,
    /// The offset of the next byte the terminal will print, as far as this attachment has been told.
    end: u64,
    /// Output between `cursor` and `end` that [`Attachment::poll`] has not returned yet.
    pending: VecDeque<Pending>,
    /// Bytes held in `pending`, tracked so the bound is enforced without rescanning the queue.
    held: usize,
    /// How far behind this viewer may fall before the oldest unpolled output is dropped.
    capacity_bytes: usize,
}

impl Attachment {
    /// Attaches to `terminal_id`, starting at `from`.
    ///
    /// `from` is a byte offset in the terminal's stream: the value a previous attachment returned
    /// from [`Attachment::offset`], or `0` for a viewer that wants everything still retained. The
    /// first [`Attachment::feed`] is taken to be the output at `from`, so the host must feed this
    /// attachment the transcript from `from` onward and nothing before it — handing it bytes it did
    /// not ask for would make every later offset it reports wrong.
    #[must_use]
    pub fn new(terminal_id: String, from: u64) -> Self {
        Self::with_backlog(terminal_id, from, DEFAULT_BACKLOG_BYTES)
    }

    /// Attaches with an explicit bound on unpolled output.
    ///
    /// Separate from [`Attachment::new`] so a host that knows the size of its panes can choose the
    /// bound, and so the bound can be exercised with a few bytes instead of a quarter of a megabyte.
    #[must_use]
    pub fn with_backlog(terminal_id: String, from: u64, backlog_bytes: usize) -> Self {
        Self {
            terminal_id,
            cursor: from,
            end: from,
            pending: VecDeque::new(),
            held: 0,
            // A zero bound is treated as one byte rather than as "hold nothing": holding nothing
            // would drop every byte before the viewer could poll it, which is not a viewer with a
            // small memory but a viewer that never sees output at all.
            capacity_bytes: backlog_bytes.max(1),
        }
    }

    /// The terminal this attachment observes.
    #[must_use]
    pub fn terminal_id(&self) -> &str {
        &self.terminal_id
    }

    /// The offset of the next byte this viewer has not been handed.
    ///
    /// This is the number to persist for a reconnection. It never runs ahead of what the viewer
    /// actually received, so resuming from it cannot skip output; output that arrived but was never
    /// polled is deliberately not counted, which makes a reconnect re-deliver at most the chunk that
    /// was in flight instead of losing it.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.cursor
    }

    /// Hands the attachment output the terminal just printed.
    ///
    /// Never fails and never blocks. Output the viewer has not polled is held — up to the bound —
    /// rather than refused, because a writer that had to wait for the slowest reader would let one
    /// idle pane freeze a running shell. Empty output is ignored: it would advance no offset and
    /// only make the next [`Attachment::poll`] return an empty chunk, which a client would have to
    /// special-case.
    pub fn feed(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.pending.push_back(Pending {
            start: self.end,
            text: text.to_owned(),
        });
        self.end = self.end.saturating_add(width(text));
        self.held = self.held.saturating_add(text.len());
        self.trim();
    }

    /// Takes everything the terminal printed since the last poll.
    ///
    /// `None` means "nothing new right now", not "the stream ended": a shell at a prompt prints
    /// nothing for minutes, and a viewer that read `None` as an ending would close a pane that is
    /// still open. Liveness belongs to the terminal's status, which this type never inspects.
    ///
    /// The returned chunk's `from` is where the text really starts, which may be later than the
    /// offset the viewer saved if it fell behind the bound; `to` is always the end of the stream as
    /// far as this attachment has been told, and becomes the value [`Attachment::offset`] reports.
    pub fn poll(&mut self) -> Option<Chunk> {
        if self.pending.is_empty() {
            return None;
        }
        let from = self
            .pending
            .front()
            .map_or(self.cursor, |front| front.start);
        let mut text = String::with_capacity(self.held);
        while let Some(front) = self.pending.pop_front() {
            text.push_str(&front.text);
        }
        self.held = 0;
        self.cursor = self.end;
        Some(Chunk {
            text,
            from,
            to: self.end,
        })
    }

    /// How many bytes are held for this viewer but not polled yet.
    ///
    /// A host uses this to notice a viewer that has stopped reading: a backlog that sits at the bound
    /// while the terminal keeps printing is a pane nobody is looking at, and dropping its attachment
    /// is cheaper than letting it discard its own output one poll at a time.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.held
    }

    /// Stops observing.
    ///
    /// The terminal is untouched: an attachment holds offsets and a copy of unpolled output, not a
    /// handle on the shell, so detaching cannot end a process. The backlog is released here rather
    /// than whenever the caller's last handle to it happens to go away, so a host that detaches a
    /// dead pane gets its memory back at a known moment.
    pub fn detach(mut self) {
        self.pending.clear();
        self.held = 0;
    }

    /// Drops the oldest unpolled output while the bound is exceeded.
    ///
    /// The newest entry is always kept, whatever its size. Cutting an entry would either split a
    /// character or return text that does not begin where its `from` claims, so the choice is
    /// between dropping whole feeds and keeping them; keeping the newest means the next poll always
    /// has something to compare against the saved offset, which is how a viewer notices it fell
    /// behind. Dropping everything would leave `poll` answering `None` — indistinguishable from an
    /// idle terminal — exactly when the viewer most needs to be told it lost output. The effective
    /// bound is therefore the configured capacity plus the size of the largest single feed.
    fn trim(&mut self) {
        while self.held > self.capacity_bytes && self.pending.len() > 1 {
            let Some(front) = self.pending.pop_front() else {
                break;
            };
            self.held = self.held.saturating_sub(front.text.len());
        }
    }
}

/// One terminal observed by any number of viewers.
///
/// The registry of viewers lives here rather than in a viewer: someone has to know which ids are
/// live, hand every one the same bytes, and keep the stream's end offset so a viewer joining now
/// knows where "now" is. That is this type. It owns the broadcast and the id allocator, and it owns
/// no part of the terminal.
#[derive(Debug)]
pub struct Fanout {
    /// The terminal being broadcast, copied into each viewer so a chunk identifies its source.
    terminal_id: String,
    /// The id the next viewer receives. Starts at 1 so `0` is never a live viewer id, leaving it
    /// free for a caller to use as "none" without a second enum.
    next_id: u64,
    /// The offset of the next byte to broadcast, which is also where a joining viewer starts.
    end: u64,
    /// The live viewers. Ordered by id so a debug dump or a listing is stable between runs.
    viewers: BTreeMap<u64, Attachment>,
}

impl Fanout {
    /// Creates the broadcast point for `terminal_id`.
    #[must_use]
    pub fn new(terminal_id: impl Into<String>) -> Self {
        Self {
            terminal_id: terminal_id.into(),
            next_id: 1,
            end: 0,
            viewers: BTreeMap::new(),
        }
    }

    /// The terminal being broadcast.
    #[must_use]
    pub fn terminal_id(&self) -> &str {
        &self.terminal_id
    }

    /// The offset of the next byte to broadcast.
    ///
    /// This is the end of the stream, not any viewer's position: it is where a viewer attached right
    /// now would start. Note the deliberate difference from [`Attachment::offset`], which reports
    /// how far one viewer has read.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.end
    }

    /// Adds a viewer and returns its id and the offset it starts at.
    ///
    /// The offset is the stream's end, so a viewer added now watches what happens next instead of
    /// replaying the past: two panes opened minutes apart should not both scroll back to the top of
    /// a transcript the user has already read in the other one. A client that wants the output it
    /// missed reads it from the terminal's own transcript first — those offsets are the same numbers
    /// this module counts — and then attaches here to continue.
    pub fn attach(&mut self) -> (u64, u64) {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let from = self.end;
        self.viewers
            .insert(id, Attachment::new(self.terminal_id.clone(), from));
        (id, from)
    }

    /// Hands output to every viewer.
    ///
    /// Each viewer keeps its own cursor, so a slow one costs its own bounded backlog and nothing
    /// else: this loop cannot wait on a viewer, and no viewer's position depends on another's. Empty
    /// output is ignored, because it is a fact about one write and not about the stream, and moving
    /// the stream's end for it would make every viewer's offsets disagree with the bytes.
    pub fn broadcast(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.end = self.end.saturating_add(width(text));
        for viewer in self.viewers.values_mut() {
            viewer.feed(text);
        }
    }

    /// Takes the output `id` has not seen yet.
    ///
    /// `None` covers two cases that a viewer usually does not need to tell apart: nothing new has
    /// been broadcast, or `id` is not attached at all. Neither is an ending — a detached viewer
    /// simply has nothing to read. A host that must distinguish them has [`Fanout::attached`] for
    /// the roster and the [`Fanout::detach`] result, which does report whether the id existed.
    pub fn poll(&mut self, id: u64) -> Option<Chunk> {
        self.viewers.get_mut(&id)?.poll()
    }

    /// Removes a viewer, reporting whether it existed.
    ///
    /// Removing the last viewer does not close the terminal. Broadcasting keeps advancing the
    /// stream, so a viewer attached afterwards starts where the stream has reached rather than
    /// re-reading a stream that restarted: the terminal's offsets are its own, not the fanout's.
    /// Returns `false` for an id that was never handed out or was already removed, so a caller
    /// cleaning up after a dropped connection does not have to know which of the two happened.
    pub fn detach(&mut self, id: u64) -> bool {
        self.viewers.remove(&id).is_some()
    }

    /// How many viewers are attached.
    #[must_use]
    pub fn attached(&self) -> usize {
        self.viewers.len()
    }
}

/// The byte width of `text`, as an offset.
fn width(text: &str) -> u64 {
    // Widening, not narrowing: on the 64-bit targets this crate builds for the conversion always
    // succeeds, and the clamp keeps a hypothetical 128-bit length from wrapping to a small offset.
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}
