//! A bounded output buffer for a terminal, addressed by absolute byte offset.
//!
//! A terminal that stays open for an hour prints more than anyone wants to keep, so what it printed has to
//! be bounded. Bounding it is the easy half; the hard half is telling a client what happened to the part
//! that is gone, because a client resumes from the offset it last read and the buffer may have dropped
//! exactly those bytes in the meantime.
//!
//! # Why offsets rather than "the last N bytes"
//!
//! A client that reconnects needs to continue where it stopped, not to receive a tail it has already seen.
//! Every appended byte therefore gets a permanent position in a stream that only grows, and
//! [`Buffer::read`] takes that position as its input. [`Buffer::total_bytes`] is the position of the next
//! byte, and it never decreases — including after content is dropped, because a counter that fell back
//! would look to a client like output had disappeared.
//!
//! # Why a dropped offset is an error, not an empty answer
//!
//! When the client's position is older than anything retained there are two possible answers, and they mean
//! opposite things: "nothing was printed since you last looked" and "what was printed since then has been
//! dropped". Returning an empty string for the second case makes a client conclude the first, and it then
//! waits for output that has already been discarded. [`Chunk`] therefore carries `truncated`, and the text
//! returned with it starts at the oldest offset that still exists, so the client resynchronises instead of
//! guessing.
//!
//! # Why two bounds
//!
//! A byte ceiling alone trims to a byte count that lands in the middle of a line, so the line a user is
//! looking at arrives cut in half. [`Buffer::new`] therefore also takes a minimum number of trailing lines
//! to retain, and that floor wins over the ceiling: whole lines are worth more than a byte-exact bound. The
//! one exception is a line longer than the ceiling itself — a progress bar redrawn on a single line can be
//! megabytes long, and protecting all of it would let one writer pin an arbitrary amount of memory. Such a
//! line is protected only for its most recent `capacity_bytes` bytes.
//!
//! # Why truncation never cuts a character
//!
//! The retained text is handed back as a `String`, and half of a UTF-8 sequence is not a character: turning
//! it into one would replace it with `U+FFFD` and silently corrupt the output. Every cut this module makes
//! is moved forward to the next character boundary, so what it returns is always text that was written.

use std::collections::VecDeque;

/// The largest minimum line count this buffer honours.
///
/// The line floor may lift the retained size above `capacity_bytes`, so an unbounded floor would remove the
/// byte bound altogether: a caller asking for a million protected lines would retain everything ever seen.
/// Clamping keeps the floor meaningful without letting it turn into unbounded retention.
const MAX_KEEP_LINES: usize = 1024;

/// One retained run of output and where it starts in the stream.
///
/// Output is stored in the chunks it arrived in rather than as one buffer because trimming then pops whole
/// entries: only the front entry is ever rewritten, and only when a cut lands inside it.
#[derive(Debug)]
struct Entry {
    /// Whole characters only: every cut this module makes is on a character boundary.
    text: String,
    /// The absolute offset of `text`'s first byte.
    start: u64,
}

/// A bounded output buffer with offset-addressed reads.
///
/// The buffer keeps at most `capacity_bytes` of output, plus enough of the trailing lines to satisfy the
/// minimum line count it was built with. Reads address the stream, not the retained window, so a reader
/// that remembers its position can continue after a reconnection — or be told that its position is gone.
#[derive(Debug)]
pub struct Buffer {
    /// Retained output, oldest first.
    entries: VecDeque<Entry>,
    /// The absolute offset of the next byte appended, which is also the count of bytes ever appended.
    ///
    /// Monotonic even when content is dropped: a reader compares it against its own position to decide
    /// whether new output exists, and a value that fell back after a trim would look like output vanishing.
    total: u64,
    /// The absolute offset of the oldest retained byte; everything before it is gone.
    first: u64,
    /// The byte ceiling.
    capacity_bytes: usize,
    /// The minimum number of trailing lines to retain regardless of the ceiling.
    keep_lines: usize,
    /// How many bytes the entries hold. Tracked rather than recomputed so an append stays cheap.
    held: usize,
}

/// What one read returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The output from the requested offset onward, as valid UTF-8.
    ///
    /// Empty is a fact about the stream ("nothing new"), never a way of reporting a loss: a loss sets
    /// [`Chunk::truncated`] and returns the oldest text that still exists.
    pub text: String,
    /// The offset to pass to the next [`Buffer::read`].
    ///
    /// The end of the stream rather than `text.len()` added to the request: a read that started at a
    /// truncated offset, or inside a character, does not begin where it was asked to.
    pub next: u64,
    /// Whether output before the requested offset was dropped.
    pub truncated: bool,
}

impl Buffer {
    /// Creates a buffer holding at most `capacity_bytes` of output.
    ///
    /// `keep_lines` is the minimum number of trailing lines to retain; it wins over `capacity_bytes` when
    /// whole lines and the byte ceiling disagree, except that a line longer than the whole capacity is
    /// protected only for its last `capacity_bytes` bytes (see the module docs).
    ///
    /// A zero capacity is treated as one byte rather than as "retain nothing": a buffer that keeps nothing
    /// is indistinguishable from a buffer that is broken, and the last byte is what a client reads to see
    /// that output is still arriving.
    #[must_use]
    pub fn new(capacity_bytes: usize, keep_lines: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            total: 0,
            first: 0,
            capacity_bytes: capacity_bytes.max(1),
            keep_lines: keep_lines.min(MAX_KEEP_LINES),
            held: 0,
        }
    }

    /// Appends output.
    ///
    /// Appending never fails and never returns an error for output that does not fit: the buffer's job is
    /// to keep what it can, and what it could not keep is reported by the next [`Buffer::read`] as
    /// truncation rather than by refusing the write. Refusing would push the decision onto every writer,
    /// which knows less about the reader's position than the buffer does.
    pub fn append(&mut self, text: &str) {
        if text.is_empty() {
            // An empty entry would be a chunk no read could start from, and one that a line-count scan
            // would have to step over for no reason.
            return;
        }
        let start = self.total;
        self.total = self.total.saturating_add(to_u64(text.len()));
        self.held = self.held.saturating_add(text.len());
        self.entries.push_back(Entry {
            text: text.to_owned(),
            start,
        });
        self.trim();
    }

    /// Reads the output from `from` onward.
    ///
    /// `from` is an offset previously returned as [`Chunk::next`] or taken from
    /// [`Buffer::first_readable`]. When it is older than anything retained the result carries
    /// `truncated: true` and the text starts at [`Buffer::first_readable`], because the alternative — an
    /// empty string — reads as "no output" and would make a client wait for bytes that were dropped.
    ///
    /// An offset that lands inside a character is moved forward to the next character boundary, so the
    /// returned text is always valid UTF-8; the partial character is skipped rather than replaced.
    #[must_use]
    pub fn read(&self, from: u64) -> Chunk {
        if from >= self.total {
            // Asking for the future is not a loss: nothing has been written past the end, and a client
            // that polls does this on every round.
            return Chunk {
                text: String::new(),
                next: self.total,
                truncated: false,
            };
        }
        if from < self.first {
            return Chunk {
                text: self.text(),
                next: self.total,
                truncated: true,
            };
        }
        Chunk {
            text: self.from_offset(from),
            next: self.total,
            truncated: false,
        }
    }

    /// The total number of bytes ever appended.
    ///
    /// Monotonic: it counts the stream, not the retained window, so a client can compare it with the
    /// offset it last read to learn whether there is anything new even after a truncation.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// How many bytes are currently retained.
    ///
    /// May exceed `capacity_bytes` when the minimum line count demands more than the ceiling allows: the
    /// floor is a promise about whole lines, and a byte-exact ceiling cannot keep it.
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        self.held
    }

    /// The offset of the oldest byte still readable.
    ///
    /// Equal to [`Buffer::total_bytes`] when nothing is retained, which is the same offset
    /// [`Buffer::read`] accepts to start over.
    #[must_use]
    pub fn first_readable(&self) -> u64 {
        self.first
    }

    /// Drops the oldest output until either the ceiling or the line floor is satisfied.
    ///
    /// The cut is taken at the earlier of two positions, not the later: the ceiling says how far the window
    /// *may* move, the floor says how far it *must not* move, so the reachable cut is the smaller of the
    /// two. Taking the larger would cut into the protected lines on every overflow.
    fn trim(&mut self) {
        let capacity = to_u64(self.capacity_bytes);
        if to_u64(self.held) <= capacity {
            return;
        }
        let ceiling = self.total.saturating_sub(capacity);
        let target = ceiling.min(self.protected_start());
        let target = self.boundary_from(target);
        if target <= self.first {
            // The whole retained window is protected, or the cut would not remove a byte.
            return;
        }
        self.drop_before(target);
    }

    /// The earliest offset the line floor allows the retained window to start at.
    ///
    /// This is the start of the last `keep_lines` lines, raised past any line that is longer than the whole
    /// capacity: such a line can only survive as its own tail, and saying so here is what stops one
    /// progress bar from pinning the buffer at an arbitrary size.
    fn protected_start(&self) -> u64 {
        if self.keep_lines == 0 || self.entries.is_empty() {
            return self.total;
        }
        self.cap_lines(self.last_lines_start())
    }

    /// The offset where the last `keep_lines` lines begin.
    ///
    /// Scanned backwards and stopped as soon as enough lines are counted, so the cost is the size of the
    /// protected tail rather than the size of the buffer.
    fn last_lines_start(&self) -> u64 {
        // Line boundaries still to cross. A line is a run of bytes between newlines, and a newline at the
        // very end of the text terminates the final line instead of starting an empty one — otherwise a
        // trailing newline would spend one of the caller's protected lines on nothing.
        let mut remaining = self.keep_lines;
        let mut skip_last = true;
        for entry in self.entries.iter().rev() {
            for (index, byte) in entry.text.as_bytes().iter().enumerate().rev() {
                if skip_last {
                    skip_last = false;
                    if *byte == b'\n' {
                        continue;
                    }
                }
                if *byte != b'\n' {
                    continue;
                }
                remaining -= 1;
                if remaining == 0 {
                    return entry
                        .start
                        .saturating_add(to_u64(index))
                        .saturating_add(1);
                }
            }
        }
        self.first
    }

    /// Raises `start` past any protected line that is longer than the whole capacity.
    fn cap_lines(&self, start: u64) -> u64 {
        let capacity = to_u64(self.capacity_bytes);
        let mut position = start;
        let mut line_start = start;
        let mut cut = start;
        for slice in self.bytes_from(start) {
            for byte in slice {
                position = position.saturating_add(1);
                if *byte != b'\n' {
                    continue;
                }
                cut = cut.max(shot_of(line_start, position, capacity));
                line_start = position;
            }
        }
        // The final line, which has no newline to end it.
        cut.max(shot_of(line_start, position, capacity))
    }

    /// Drops every retained byte before `offset`, which must be a character boundary.
    fn drop_before(&mut self, offset: u64) {
        while let Some(front) = self.entries.front() {
            let end = front.start.saturating_add(to_u64(front.text.len()));
            if end > offset {
                break;
            }
            self.held = self.held.saturating_sub(front.text.len());
            self.entries.pop_front();
        }
        if let Some(front) = self.entries.front_mut() {
            if front.start < offset {
                let cut = to_usize(offset.saturating_sub(front.start)).min(front.text.len());
                if cut > 0 && front.text.is_char_boundary(cut) {
                    front.text.drain(..cut);
                    front.start = offset;
                    self.held = self.held.saturating_sub(cut);
                }
            }
        }
        self.first = self
            .entries
            .front()
            .map_or(self.total, |front| front.start);
    }

    /// The first offset at or after `offset` that begins a character.
    fn boundary_from(&self, offset: u64) -> u64 {
        let mut position = offset;
        for slice in self.bytes_from(offset) {
            for byte in slice {
                if !is_continuation(*byte) {
                    return position;
                }
                position = position.saturating_add(1);
            }
        }
        position
    }

    /// Everything still retained, in order.
    fn text(&self) -> String {
        self.from_offset(self.first)
    }

    /// The retained text from `offset` onward.
    fn from_offset(&self, offset: u64) -> String {
        let mut text = String::new();
        for (index, slice) in self.bytes_from(offset).enumerate() {
            // A caller may hand back an offset that falls inside a character: a saved byte count can come
            // from anywhere. Skipping the partial character keeps the answer equal to what was written,
            // where lossy decoding would hand back replacement characters instead.
            let slice = if index == 0 { from_boundary(slice) } else { slice };
            text.push_str(&String::from_utf8_lossy(slice));
        }
        text
    }

    /// The retained bytes from `offset` onward, chunk by chunk.
    fn bytes_from(&self, offset: u64) -> impl Iterator<Item = &[u8]> + '_ {
        self.entries.iter().filter_map(move |entry| {
            let end = entry.start.saturating_add(to_u64(entry.text.len()));
            if end <= offset {
                return None;
            }
            let skip = to_usize(offset.saturating_sub(entry.start)).min(entry.text.len());
            entry.text.as_bytes().get(skip..)
        })
    }
}

/// Where a protected line has to start for it to fit inside `capacity`.
///
/// A line that already fits is left alone: `line_start` is returned, which raises nothing. A longer line
/// can only keep its tail, because keeping the whole of it would let one line defeat the ceiling.
const fn shot_of(line_start: u64, line_end: u64, capacity: u64) -> u64 {
    if line_end.saturating_sub(line_start) <= capacity {
        line_start
    } else {
        line_end.saturating_sub(capacity)
    }
}

/// Whether `byte` is a UTF-8 continuation byte, i.e. the tail of a character that began earlier.
const fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// `slice` without the continuation bytes at its start.
fn from_boundary(slice: &[u8]) -> &[u8] {
    let start = slice
        .iter()
        .position(|byte| !is_continuation(*byte))
        .unwrap_or(slice.len());
    slice.get(start..).unwrap_or_default()
}

/// Widens a length for offset arithmetic.
fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Narrows an offset for slicing, clamping rather than failing on a 32-bit target.
fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
