//! Retention: bounded model-facing output, and an exact record of what was dropped.
//!
//! A tool that reads a large file cannot return all of it. Something has to decide what to keep, and
//! the decision has to be *reportable*: a model told "these are all the matches" when they are not
//! draws a conclusion the evidence does not support.
//!
//! # What this owns, and what it deliberately does not
//!
//! It owns one mechanical question — what was kept, what was omitted, and how much — and nothing else.
//! No headers, no line numbers, no exit markers, no "output was truncated, use grep" prose. Those are
//! the tool's business: only the tool knows whether a truncation means "the file is long" or "I stopped
//! reading because of a budget", and folding the two together produces a message that lies about one of
//! them.
//!
//! # Why `truncated` is not "the output was incomplete"
//!
//! A permission failure, an unreadable candidate, and a provider that stopped early are all
//! *incompleteness* but none of them is a retention decision. [`Omitted`] describes only what this layer
//! dropped, so a caller can report "12 more matches" without implying the tool failed.
//!
//! # Why two retainers rather than one
//!
//! They bound different resources. [`ItemRetainer`] bounds ordered units — paths, matches, sources — and
//! a unit is atomic: half a match is not a match. [`TextRetainer`] bounds bytes and must cut on a
//! character boundary, because half a UTF-8 sequence is not text.
//!
//! # Why the head and the tail are different strategies
//!
//! A log's interesting part is usually the end, and a listing's interesting part is the beginning. One
//! strategy cannot serve both, and a caller that had to reverse its input to get a tail would produce
//! output in the wrong order.

/// What a retainer dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Omitted {
    /// Nothing was dropped.
    None,
    /// Exactly this many units or bytes were dropped.
    ///
    /// The normal shape: every unit was observed, so the count is precise.
    Exact {
        /// How many.
        count: u64,
    },
    /// Content was dropped without a count.
    ///
    /// Reserved for a caller that omits something it cannot count. The retainers here never produce it,
    /// but a caller assembling output from several sources might.
    Unknown,
}

impl Omitted {
    /// Whether anything was dropped.
    #[must_use]
    pub fn is_some(self) -> bool {
        !matches!(self, Self::None)
    }

    /// The count, when it is known.
    #[must_use]
    pub fn count(self) -> Option<u64> {
        match self {
            Self::Exact { count } => Some(count),
            Self::None => Some(0),
            Self::Unknown => None,
        }
    }
}

/// The result of retaining items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedItems<T> {
    /// What was kept, in input order.
    pub items: Vec<T>,
    /// Whether anything was dropped because of the budget.
    ///
    /// Not "the source was incomplete": a permission failure is the caller's field, never this flag.
    pub truncated: bool,
    /// How many units were offered.
    pub seen: u64,
    /// How many were kept.
    pub kept: u64,
    /// What was dropped.
    pub omitted: Omitted,
}

impl<T> RetainedItems<T> {
    /// An empty result.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            truncated: false,
            seen: 0,
            kept: 0,
            omitted: Omitted::None,
        }
    }
}

/// How an item retainer bounds its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemStrategy {
    /// Keep the first `max_items` units.
    ///
    /// The only strategy here, and the right one for a listing: the first matches of a search are the
    /// ones a reader wants, and keeping the last would require reading the whole source first.
    Head {
        /// How many units to keep.
        max_items: usize,
    },
}

/// Bounds ordered logical units.
///
/// A unit is atomic: half a match is not a match, so the retainer never splits one.
#[derive(Debug)]
pub struct ItemRetainer<T> {
    strategy: ItemStrategy,
    kept: Vec<T>,
    seen: u64,
}

impl<T> ItemRetainer<T> {
    /// Creates a retainer.
    #[must_use]
    pub fn new(strategy: ItemStrategy) -> Self {
        Self {
            strategy,
            kept: Vec::new(),
            seen: 0,
        }
    }

    /// Offers one unit.
    ///
    /// Returns whether it was kept, so a caller streaming items can stop generating more once the budget
    /// is full — which is the difference between bounding output and bounding work.
    pub fn push(&mut self, item: T) -> bool {
        self.seen += 1;
        match self.strategy {
            ItemStrategy::Head { max_items } => {
                if self.kept.len() < max_items {
                    self.kept.push(item);
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Whether the budget is full, so a caller can stop producing.
    #[must_use]
    pub fn is_full(&self) -> bool {
        match self.strategy {
            ItemStrategy::Head { max_items } => self.kept.len() >= max_items,
        }
    }

    /// How many units have been offered.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Finishes, reporting what was kept and what was dropped.
    #[must_use]
    pub fn finish(self) -> RetainedItems<T> {
        let kept = u64::try_from(self.kept.len()).unwrap_or(u64::MAX);
        let omitted = self.seen.saturating_sub(kept);
        RetainedItems {
            truncated: omitted > 0,
            items: self.kept,
            seen: self.seen,
            kept,
            omitted: if omitted == 0 {
                Omitted::None
            } else {
                Omitted::Exact { count: omitted }
            },
        }
    }
}

/// How a text retainer bounds its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStrategy {
    /// Keep the first `max_bytes`.
    Head {
        /// The byte budget.
        max_bytes: usize,
    },
    /// Keep the last `max_bytes`.
    Tail {
        /// The byte budget.
        max_bytes: usize,
    },
    /// Keep `head_bytes` from the start and `tail_bytes` from the end.
    ///
    /// What a shell tool wants: the command's echo is at the start and its result at the end, and the
    /// middle is the part a reader skips.
    HeadTail {
        /// The byte budget for the start.
        head_bytes: usize,
        /// The byte budget for the end.
        tail_bytes: usize,
    },
}

/// A text retainer's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedText {
    /// The retained text, safe to hand to a formatter.
    ///
    /// No headers or markers: only the caller knows what a truncation means.
    pub text: String,
    /// Whether anything was dropped because of the budget.
    pub truncated: bool,
    /// How many bytes were dropped.
    pub omitted_bytes: Omitted,
}

/// Bounds a byte-oriented text stream.
///
/// Cuts on a character boundary at [`TextRetainer::finish`]: half a UTF-8 sequence is not text, and a
/// `String` built from one would be lossily replaced rather than refused.
#[derive(Debug)]
pub struct TextRetainer {
    strategy: TextStrategy,
    /// Bytes at the start, when the strategy keeps a head.
    head: Vec<u8>,
    /// Bytes at the end, when the strategy keeps a tail.
    tail: std::collections::VecDeque<u8>,
    /// Total bytes offered.
    seen: u64,
}

impl TextRetainer {
    /// Creates a retainer.
    #[must_use]
    pub fn new(strategy: TextStrategy) -> Self {
        Self {
            strategy,
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            seen: 0,
        }
    }

    /// Offers one chunk.
    pub fn push(&mut self, chunk: &str) {
        let bytes = chunk.as_bytes();
        self.seen = self
            .seen
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        match self.strategy {
            TextStrategy::Head { max_bytes } => {
                let room = max_bytes.saturating_sub(self.head.len());
                self.head.extend_from_slice(&bytes[..bytes.len().min(room)]);
            }
            TextStrategy::Tail { max_bytes } => {
                self.tail.extend(bytes.iter().copied());
                // Bounded as it grows rather than at the end: a caller streaming a large command's
                // output would otherwise hold the whole thing in memory before discarding most of it.
                while self.tail.len() > max_bytes {
                    self.tail.pop_front();
                }
            }
            TextStrategy::HeadTail {
                head_bytes,
                tail_bytes,
            } => {
                let room = head_bytes.saturating_sub(self.head.len());
                let taken = bytes.len().min(room);
                self.head.extend_from_slice(&bytes[..taken]);
                self.tail.extend(bytes[taken..].iter().copied());
                while self.tail.len() > tail_bytes {
                    self.tail.pop_front();
                }
            }
        }
    }

    /// Finishes, cutting on a character boundary.
    #[must_use]
    pub fn finish(self) -> RetainedText {
        let mut text = String::new();

        // The head keeps a whole prefix: a cut is only needed when the budget fell inside the offered
        // bytes, and a boundary walk backwards is what keeps the result valid UTF-8.
        let head_len = floor_char_boundary(&self.head);
        text.push_str(&String::from_utf8_lossy(&self.head[..head_len]));

        let tail: Vec<u8> = self.tail.into_iter().collect();
        let tail_start = ceil_char_boundary(&tail);
        text.push_str(&String::from_utf8_lossy(&tail[tail_start..]));

        let kept = u64::try_from(text.len()).unwrap_or(u64::MAX);
        let omitted = self.seen.saturating_sub(kept);
        RetainedText {
            text,
            truncated: omitted > 0,
            omitted_bytes: if omitted == 0 {
                Omitted::None
            } else {
                Omitted::Exact { count: omitted }
            },
        }
    }
}

/// The largest index at or below the end that is a character boundary.
///
/// The head may end mid-sequence when its byte budget fell inside a character, so the walk starts at
/// `len - 1` and steps back through continuation bytes. Starting at `len` would be out of bounds, and
/// skipping the walk would produce a `String` whose last character was lossily replaced.
fn floor_char_boundary(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut index = bytes.len() - 1;
    while index > 0 && (bytes[index] & 0xc0) == 0x80 {
        index -= 1;
    }
    // `index` is now at a leading byte. If its character does not fit entirely inside `bytes`, the
    // character is incomplete and the cut belongs before it.
    let width = char_width(bytes[index]);
    if index + width <= bytes.len() {
        index + width
    } else {
        index
    }
}

/// The byte width a leading byte announces.
fn char_width(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead < 0xe0 {
        2
    } else if lead < 0xf0 {
        3
    } else {
        4
    }
}

/// The smallest index at or above `index` that is a character boundary.
fn ceil_char_boundary(bytes: &[u8]) -> usize {
    let mut index = 0;
    // Walk forward to the first boundary at or after a notional cut of zero: the tail's first byte may
    // be a continuation byte because the ring dropped the bytes before it.
    while index < bytes.len() && (bytes[index] & 0xc0) == 0x80 {
        index += 1;
    }
    index
}
