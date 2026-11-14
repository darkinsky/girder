//! A cursor over a session's event log that keeps working while the log grows.
//!
//! `session.stream` is declared but unimplemented: [`crate::remote`] answers it with `NOT_IMPLEMENTED`,
//! and the polling shape that stands in for it, `session.follow`, returns at most
//! [`FOLLOW_LIMIT`](crate::remote::FOLLOW_LIMIT) events per call. The delivery half of streaming has no
//! implementation anywhere — [`Streams`](crate::stream_method::Streams) can *declare* and *open* a stream
//! but stores no frames, and nothing in this build opens one.
//!
//! This module is that missing half for sessions, and only for sessions: it turns a log plus a cursor
//! into a sequence of [`Frame`]s a transport can write one at a time.
//!
//! # One type for a finished log and a log being written
//!
//! A session that has stopped and a session that is mid-turn look identical here, because the difference
//! is not in the type — it is in whether more events arrive. [`SessionStream::push`] is how a producer
//! says "one more"; [`SessionStream::next_frame`] answers "what is there". A static replay is the special
//! case where nothing is pushed, and a live subscription is the case where something is.
//!
//! Splitting those into two types (a `Replay` and a `Subscription`) would duplicate the cursor arithmetic
//! and the tag derivation, and the arithmetic is exactly where an off-by-one becomes a duplicated or
//! dropped event at the client. One implementation means one place to be right.
//!
//! # Why exhaustion is not a latch
//!
//! [`SessionStream::next_frame`] returns `None` when it has caught up with the log. That is *not* the end
//! of the stream: a live session's log is empty for most of its life between steps, and a client that
//! treated the first `None` as terminal would stop reading at the first quiet moment. The cursor is an
//! index into the log, so a later [`SessionStream::push`] simply makes the index valid again and reading
//! resumes — there is no "done" flag to clear, because there is no "done" flag.
//!
//! Only [`SessionStream::cancel`] ends a stream permanently, and it does so on purpose: a consumer that
//! gave up is not waiting for the next event.
//!
//! # Why `seq` is one-based and never renumbered
//!
//! Every sequence number in this build is one-based — `session.page` has a `cursor` that is the inclusive
//! 1-based position of the last record, `session.follow` takes `afterSeq` as the last seq the client saw,
//! and `SessionStore::read_after` skips that many events. A stream that numbered from zero, or that
//! restarted its count at the point the subscriber joined, would make the client's cursor point at the
//! wrong event and either replay one or skip one. So a frame's `seq` is the 1-based position of its event
//! in the log, and starting mid-log changes which position is first, not how positions are counted.
//!
//! # What this module does not do
//!
//! - It does not read the store. A caller that wants a stream over a live [`SessionStore`] reads the log
//!   and calls [`SessionStream::new`] with it; keeping filesystem I/O out of this type is what lets it be
//!   tested without a file and used for a replay of events held in memory.
//! - It does not deliver. Waking a blocked reader, bounding the buffer, and closing the transport are the
//!   channel's job — see the "What this module does not do" note in [`crate::stream_method`] for why that
//!   half has to be designed against a bounded queue rather than added here.
//! - It is not registered as `session.stream`. Registering a streaming method is a change to
//!   [`crate::remote`] and exporting this module is a change to the crate root; both are separate steps.
//!
//! [`SessionStore`]: crate::session::SessionStore

use serde_json::Value;

use girder_core::SessionEvent;

/// The serde tag [`SessionEvent`] uses to name its variant.
///
/// Named once here because [`frame_for`] reads the tag back out of the encoding instead of matching on
/// the variant a second time. If the vocabulary ever changes the attribute, this is the string that has
/// to change with it, and the test that compares [`Frame::kind`] against `serde_json` fails if it does not.
const EVENT_TAG: &str = "type";

/// One frame of a session stream.
///
/// A frame is an event plus the two numbers a client needs to place it: where it sits in the log, and
/// what shape its body has. The body is the event's own JSON rather than a bespoke delivery envelope, so
/// a client that already decodes `session.page` records decodes these unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// The event's 1-based position in the session log.
    ///
    /// This is the same number `session.page` returns as its cursor and `session.follow` calls `seq`, which
    /// is what lets a client switch between paging, polling, and streaming without translating cursors.
    pub seq: u64,

    /// The event's type tag, spelled exactly as [`SessionEvent`]'s `serde` rename spells it — `"turn/start"`,
    /// `"tool/result"`, and so on.
    ///
    /// Taken from the encoded payload rather than written out a second time: a client dispatches on this
    /// string, and two spellings of the same event that agree today would not have to agree tomorrow.
    pub kind: String,

    /// The event body, as JSON.
    ///
    /// Includes the `type` tag, so a consumer that only looks at `payload` still sees the same object the
    /// log holds.
    pub payload: Value,
}

/// Why a stream has no more frames to give *right now*.
///
/// Reported instead of an `Option<Frame>`'s bare `None` because the three cases call for different client
/// behaviour: an exhausted live stream should keep its subscription open, a completed turn is a natural
/// place to render a boundary, and a cancelled stream should be torn down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// The log was read to its end — a static replay, or a live session between steps.
    Exhausted,

    /// The last frame was `turn/end`: the session finished the work it was asked for.
    Completed,

    /// The consumer cancelled. Terminal: later events do not revive the stream.
    Cancelled,
}

/// A position in a session's event log, and the frames read from it so far.
///
/// Construct with [`SessionStream::new`], read with [`SessionStream::next_frame`], extend with
/// [`SessionStream::push`]. See the module documentation for why a finished log and a growing log are the
/// same type here.
#[derive(Debug)]
pub struct SessionStream {
    /// The log itself. Owned rather than borrowed because a stream outlives the read that produced it —
    /// a caller that handed in a slice would have to keep the log alive for the whole subscription.
    events: Vec<SessionEvent>,

    /// Index of the next event to frame, which is the count of events already consumed.
    ///
    /// An index rather than a `seq` so the arithmetic is the log's own: event `i` has seq `i + 1`, and
    /// [`SessionStream::push`] cannot invalidate a position that was already read.
    next_index: usize,

    /// Frames handed out since construction. Not derived from `next_index` because a stream that starts
    /// mid-log begins with a non-zero index but has emitted nothing yet.
    emitted: u64,

    /// Whether the consumer gave up. Terminal, unlike exhaustion; see the module documentation.
    cancelled: bool,
}

impl SessionStream {
    /// Starts a stream at the first event *after* `from_seq`.
    ///
    /// `from_seq` is the last sequence number the caller has already seen, so `0` means "from the start"
    /// and `n` means "everything after the nth event" — the same contract as `session.follow`'s `afterSeq`
    /// and [`SessionStore::read_after`](crate::session::SessionStore::read_after).
    ///
    /// A `from_seq` past the end of the log is clamped rather than rejected: a client whose cursor is
    /// ahead (it was resumed from a snapshot of a log that has since been truncated, or it simply asked
    /// for more than exists) is asking for "whatever comes next", and the honest answer is nothing yet.
    /// Clamping also keeps sequence numbers honest — a frame's `seq` is its position, never a counter the
    /// caller could pick by naming a large cursor.
    #[must_use]
    pub fn new(events: Vec<SessionEvent>, from_seq: u64) -> Self {
        let start = usize::try_from(from_seq).unwrap_or(usize::MAX);
        Self {
            next_index: start.min(events.len()),
            events,
            emitted: 0,
            cancelled: false,
        }
    }

    /// Takes the next frame, or `None` when the log has been read to its current end.
    ///
    /// `None` means "nothing yet", not "nothing ever": after [`SessionStream::push`] the same call returns
    /// the new frame. The only terminal `None` is one following [`SessionStream::cancel`].
    pub fn next_frame(&mut self) -> Option<Frame> {
        if self.cancelled {
            return None;
        }

        let index = self.next_index;
        // `?` on the lookup is the whole exhaustion story: no event at this index means the producer has
        // not written it yet, and the index stays put so the next call asks the same question.
        let frame = frame_for(seq_of(index), self.events.get(index)?);

        self.next_index = index.saturating_add(1);
        self.emitted = self.emitted.saturating_add(1);
        Some(frame)
    }

    /// Appends an event, as a running session would.
    ///
    /// This is what makes a static replay and a live subscription one type: the producer reports an event,
    /// and a reader that had reached the end finds one more frame. Nothing needs resetting for that to
    /// work — the cursor is an index, and the index is still valid once the vector grows.
    pub fn push(&mut self, event: SessionEvent) {
        self.events.push(event);
    }

    /// How many frames this stream has handed out.
    ///
    /// Counted from construction, so a stream opened mid-log at seq 40 reports `1` after its first frame —
    /// the number is delivery progress, not a log position.
    #[must_use]
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Ends the stream, as a consumer pressing stop would.
    ///
    /// Terminal, and separate from exhaustion on purpose: an empty live log and an abandoned subscription
    /// both produce `None` from [`SessionStream::next_frame`], and only the second one should stop the
    /// transport from waiting.
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Why [`SessionStream::next_frame`] would return `None` right now, or `None` if it would not.
    ///
    /// This is a snapshot rather than a stored outcome: a stream that reported [`EndReason::Exhausted`]
    /// and then received an event reports `None` again, because new work means it is no longer ended.
    #[must_use]
    pub fn end_reason(&self) -> Option<EndReason> {
        if self.cancelled {
            return Some(EndReason::Cancelled);
        }
        if self.next_index < self.events.len() {
            return None;
        }

        // The last delivered frame decides between "the turn finished" and "the log ran out". Looking at the
        // event rather than at `Frame::kind` keeps this a compile-time-checked match on the vocabulary instead
        // of a string comparison that would silently answer `Exhausted` for a renamed variant.
        let completed = self
            .next_index
            .checked_sub(1)
            .and_then(|last| self.events.get(last))
            .is_some_and(|event| matches!(event, SessionEvent::TurnEnd { .. }));

        Some(if completed {
            EndReason::Completed
        } else {
            EndReason::Exhausted
        })
    }
}

/// Builds the frame for the event at log position `index` (`seq` is that position plus one).
///
/// Takes the seq rather than computing it so the caller's arithmetic is visible at the one place it can go
/// wrong, and so a test can pin the mapping without reaching into the struct.
fn frame_for(seq: u64, event: &SessionEvent) -> Frame {
    // `kind` is read back out of the encoding instead of being written twice. The tag in the payload is the
    // authoritative spelling — it is what a client decodes — so deriving `kind` from it means a variant added
    // or renamed in `session.rs` cannot leave the two disagreeing. `type_name` is the fallback for the case
    // that cannot happen with this enum (a payload with no tag) rather than an `unwrap` on a serialization
    // step whose failure mode is a panic inside a delivery path.
    match serde_json::to_value(event) {
        Ok(payload) => {
            let kind = payload
                .get(EVENT_TAG)
                .and_then(Value::as_str)
                .map_or_else(|| event.type_name().to_owned(), str::to_owned);
            Frame { seq, kind, payload }
        }
        Err(_) => Frame {
            seq,
            kind: event.type_name().to_owned(),
            payload: Value::Null,
        },
    }
}

/// The 1-based sequence number of the event at `index`.
///
/// Saturating rather than wrapping: a log with more than `u64::MAX` events cannot be indexed by a `usize`
/// that converts anyway, and a wrapped number would point a client's cursor back at the start of the log.
fn seq_of(index: usize) -> u64 {
    u64::try_from(index).map_or(u64::MAX, |position| position.saturating_add(1))
}
