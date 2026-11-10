//! An append-only list whose older versions share storage.
//!
//! A session's events are appended forever and read from the beginning. Copying the list on every append would make
//! the cost quadratic in the number of events, and a session with ten thousand events would spend its time copying.
//!
//! Chunks make it linear: an append copies only the last chunk, and every earlier chunk is shared with the previous
//! version. A snapshot taken at event 5000 and one taken at 5001 differ by one chunk, not by five thousand elements.
//!
//! # Why a chunk rather than a linked list
//!
//! A linked list shares perfectly but reads badly: element 5000 costs 5000 hops, and a page of results that reads a
//! range would do it once per element. A chunk makes a range read a handful of slice copies.
//!
//! # Why the chunk is full before a new one starts
//!
//! A chunk that grew and shrank would copy on most appends. Filling one before starting the next makes the common
//! append a push into a `Vec` that already has room.
//!
//! # Why the list is immutable
//!
//! A snapshot must not change when a later append happens: a caller holding one while another writes would see its
//! own data move under it. Every operation returns a new list and shares what it can.

use std::sync::Arc;

/// How many values one chunk holds before a new one starts.
///
/// 64: small enough that a snapshot differs from its predecessor by a bounded amount, large enough that the per-chunk
/// overhead disappears against the values.
pub const CHUNK_CAPACITY: usize = 64;

/// One full chunk plus the partial chunk at the end.
#[derive(Debug)]
struct Chunks<T> {
    /// Full chunks, never modified after creation.
    full: Arc<Vec<Arc<Vec<T>>>>,
    /// The chunk currently being filled.
    tail: Arc<Vec<T>>,
}

/// An immutable append-only list.
///
/// Cheap to clone: the clone shares every chunk and differs only in the tail, so a snapshot taken before an append
/// does not observe it.
#[derive(Debug)]
pub struct List<T> {
    chunks: Option<Chunks<T>>,
    len: usize,
}

impl<T> Clone for List<T> {
    fn clone(&self) -> Self {
        // Every chunk is behind an `Arc`, so a clone shares all of them: a snapshot taken before an append does not
        // observe it.
        Self {
            chunks: self.chunks.as_ref().map(|chunks| Chunks {
                full: Arc::clone(&chunks.full),
                tail: Arc::clone(&chunks.tail),
            }),
            len: self.len,
        }
    }
}

impl<T> Default for List<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> List<T> {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chunks: None,
            len: 0,
        }
    }

    /// How many values it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The value at an index, if it exists.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len {
            return None;
        }
        let chunks = self.chunks.as_ref()?;
        let chunk = index / CHUNK_CAPACITY;
        let within = index % CHUNK_CAPACITY;
        match chunks.full.get(chunk) {
            Some(values) => values.get(within),
            // The index is in the tail.
            None if chunk == chunks.full.len() => chunks.tail.get(within),
            None => None,
        }
    }

    /// The last value.
    #[must_use]
    pub fn last(&self) -> Option<&T> {
        self.len.checked_sub(1).and_then(|index| self.get(index))
    }

    /// The values from `start` up to (not including) `end`.
    ///
    /// Clamped to the list rather than refused: a caller reading a page past the end wants the little there is, not
    /// an error about arithmetic.
    #[must_use]
    pub fn range(&self, start: usize, end: usize) -> Vec<&T> {
        let start = start.min(self.len);
        let end = end.min(self.len);
        (start..end).filter_map(|index| self.get(index)).collect()
    }

    /// Every value, in order.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len).filter_map(|index| self.get(index))
    }
}

impl<T: Clone> List<T> {
    /// Appends several values.
    #[must_use]
    pub fn extend<I: IntoIterator<Item = T>>(&self, values: I) -> Self {
        values
            .into_iter()
            .fold(self.clone(), |list, value| list.push(value))
    }

    /// Appends one value, returning the new list.
    ///
    /// The receiver is unchanged: a snapshot held by another caller does not observe the append.
    #[must_use]
    pub fn push(&self, value: T) -> Self {
        let chunks = match &self.chunks {
            None => Chunks {
                full: Arc::new(Vec::new()),
                tail: Arc::new(vec![value]),
            },
            Some(chunks) if chunks.tail.len() < CHUNK_CAPACITY => {
                // The common case: the tail has room, so one chunk is copied and the rest shared.
                let mut tail = Vec::with_capacity(chunks.tail.len() + 1);
                tail.extend(chunks.tail.iter().cloned());
                tail.push(value);
                Chunks {
                    full: Arc::clone(&chunks.full),
                    tail: Arc::new(tail),
                }
            }
            Some(chunks) => {
                // The tail is full: it becomes immutable and a new one starts.
                let mut full = Vec::with_capacity(chunks.full.len() + 1);
                full.extend(chunks.full.iter().cloned());
                full.push(Arc::clone(&chunks.tail));
                Chunks {
                    full: Arc::new(full),
                    tail: Arc::new(vec![value]),
                }
            }
        };
        Self {
            chunks: Some(chunks),
            len: self.len + 1,
        }
    }
}

impl<T: Clone> List<T> {
    /// Every value as an owned vector.
    #[must_use]
    pub fn to_vec(&self) -> Vec<T> {
        self.iter().cloned().collect()
    }
}
