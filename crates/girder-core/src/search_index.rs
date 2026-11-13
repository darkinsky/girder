//! A cache of the searchable text of each session log.
//!
//! `search_files` decompresses every log on every query. That is correct and it is O(everything) per keystroke: a person
//! typing into a search box makes one full pass over the directory per character, and each pass reads and decodes every
//! frame.
//!
//! # Why the index is keyed on size rather than on time
//!
//! A log is append-only, so a change always grows it. Size is therefore a sound invalidation key: a log whose size
//! matches what was indexed has not changed. A modification time would be a weaker test -- coarse clocks can leave it
//! identical across a real change -- and a content hash would mean reading the file, which is the work being avoided.
//!
//! # Why the text and not the events
//!
//! The events are large: every content block, every tool argument, every token count. Searching needs one string per
//! event and nothing else, so the index keeps the concatenation and drops the rest. A thousand-session directory then
//! costs one string each rather than one decoded transcript each.
//!
//! # Why this is not a full-text index
//!
//! A real index would answer a substring query without touching the text. This one still scans the cached string; what
//! it avoids is the decode, which dominates. Building an inverted index would be a much larger piece of work and would
//! have to be rebuilt on every append, which is the opposite of what an append-only log wants.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One session's cached text.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The size the log had when this was built, which is the invalidation key.
    pub size: u64,
    /// One searchable string per event, in order.
    pub events: Vec<String>,
}

/// The cache.
#[derive(Debug, Default)]
pub struct Index {
    entries: BTreeMap<String, Entry>,
}

impl Index {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many sessions are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cached entry for `id`, when it is still valid for `size`.
    #[must_use]
    pub fn get(&self, id: &str, size: u64) -> Option<&Entry> {
        self.entries.get(id).filter(|entry| entry.size == size)
    }

    /// Records an entry, replacing whatever was there.
    pub fn insert(&mut self, id: impl Into<String>, entry: Entry) {
        self.entries.insert(id.into(), entry);
    }

    /// Forgets an entry, so the next query rebuilds it.
    pub fn forget(&mut self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Forgets entries whose log no longer exists.
    ///
    /// Returns how many were dropped. A deleted session would otherwise stay in memory for the life of the process,
    /// which is the leak this cache would otherwise introduce in place of the work it removes.
    pub fn prune(&mut self, root: &Path) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|id, _| log_path(root, id).is_some_and(|path| path.exists()));
        before - self.entries.len()
    }

    /// The text for `id`, building and caching it when the cache is stale or absent.
    ///
    /// # Errors
    ///
    /// Returns the reason the log could not be read. A log that cannot be decoded is not cached: caching the failure
    /// would make a transient read error permanent for the life of the process.
    pub fn text_of(
        &mut self,
        root: &Path,
        id: &str,
    ) -> Result<&[String], crate::session::SessionError> {
        let path = log_path(root, id).ok_or_else(|| {
            crate::session::SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no log for `{id}`"),
            ))
        })?;
        let size = std::fs::metadata(&path).map_or(0, |meta| meta.len());

        // Rebuilt when the size moved. The borrow checker is the reason this is an `if` returning early rather than a
        // `match` on `get`: the `&mut self` rebuild would otherwise overlap the shared borrow the lookup took.
        let stale = self.get(id, size).is_none();
        if stale {
            let events = crate::session::read_session_file(&path)?;
            let text: Vec<String> = events
                .iter()
                .map(crate::session_query::searchable_text)
                .collect();
            self.insert(id.to_owned(), Entry { size, events: text });
        }
        let entry = self.entries.get(id).ok_or_else(|| {
            crate::session::SessionError::Io(std::io::Error::other(
                "the entry vanished after being inserted",
            ))
        })?;
        Ok(&entry.events)
    }
}

/// The log path for `id`, refusing an id that would address a different file.
///
/// The same rule the session layout enforces: an id containing a separator or `..` would let a search read outside the
/// directory it was pointed at.
fn log_path(root: &Path, id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.contains(['/', '\\']) || id.contains("..") || id.contains('\0') {
        return None;
    }
    Some(root.join(format!("{id}.jsonl.zst")))
}

// # Why there is no `Catalogue` implementation here
//
// The obvious next step is a `session_query::Catalogue` that answers from this cache, and it is the wrong step:
// `Candidate` carries `SessionEvent`s, and this cache deliberately keeps only the searchable text. Rebuilding an event
// from a string would have to invent a type -- every hit would come back as a user message, so a search for an error
// message would report it as something a person said.
//
// A catalogue that avoids the decode needs an index keyed on *what a hit is*, not only on its text. That is a larger
// structure than this one, and building it here would mean either holding every event in memory again (undoing the
// saving) or lying about what a match was.
