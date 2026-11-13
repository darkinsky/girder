//! A session host that keeps a [`ProjectionCache`] in step with a session log.
//!
//! Two fold implementations already exist and one of them has no caller. [`ProjectionRegistry`]
//! holds state for the life of a process and is what `session.state` reads today; it is built by
//! replaying the whole log on attach. [`ProjectionCache`] is the half that can be persisted — it
//! writes checkpoint rows, refuses to skip a sequence number, and can be seeded from a row instead
//! of from `init` — and until this module it was reachable only from its own tests. This module is
//! the caller: one host per session owns one cache, numbers the events it is handed, and answers
//! reads from the folded state, so a read costs a clone of the view rather than a replay of history.
//!
//! # Why the host, and not the registry, is the thing to wire up
//!
//! The registry replays on every attach, and the log grows without bound, so opening a session gets
//! slower the longer it has existed — on exactly the path a UI hits when it lists sessions. A host
//! carries the folded state in the cache and can be resumed from a checkpoint instead, which is why
//! it is a distinct type rather than another flag on the registry: the registry's contract is "state
//! for this process", and the cache's is "state that outlives it". Merging them would make every
//! registry read pay for a durability it does not ask for.
//!
//! # Why the host assigns the sequence numbers
//!
//! A log's position *is* its sequence number, but a caller that tails a log has the events and not
//! the arithmetic. Assigning one past the cursor here is also what gives the cache's gap check
//! something to check: the cache refuses to skip a sequence because a skipped fold produces a state
//! that looks correct afterwards and is not, and a caller that numbered events itself could skip one
//! without noticing.
//!
//! # Why the cursor is kept here as well as in the cache
//!
//! The cache knows, per projection, how far each fold reached; the host needs one number on every
//! append. Asking the cache means `snapshot()`, which clones every projection's state to produce a
//! value the host would immediately discard — on a session with a transcript projection that is the
//! whole log copied per appended event. The two numbers cannot drift: the cursor moves only after
//! the cache accepted the event at exactly the sequence the cursor named, and `resume` seeds both
//! from the cache's own watermark.
//!
//! # What the host deliberately does not do
//!
//! It does not deduplicate events. [`ProjectionCache::observe`] is idempotent *per sequence number*,
//! and the host is what assigns them, so a re-delivered event is indistinguishable from a new one.
//! The caller tails the log and therefore knows what it has already folded; [`ProjectionHost::cursor`]
//! is how it knows. Pretending otherwise would need an event id the log does not carry.

#![allow(dead_code)]
use std::collections::BTreeMap;
use std::sync::Arc;

use girder_core::{
    CacheError, Checkpoint, ProjectionCache, ProjectionSeq as Seq, SessionEvent,
    VersionedProjection,
};

/// Why a host could not be built.
///
/// Every variant means the host was not created: there is no partially built value to fall back to,
/// because a host that is missing a projection would answer every later read with a view that looks
/// complete and is not.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// Two definitions claimed one key.
    ///
    /// Refused rather than resolved by keeping one of them: in a cache the two would write the same
    /// checkpoint row, and the loser's state would be discarded on every resume without a word.
    #[error("a projection named `{key}` is already registered in this host")]
    DuplicateKey {
        /// The key registered twice.
        key: String,
    },

    /// The cache refused the checkpoint or the log window it was offered.
    ///
    /// Wrapped rather than flattened into one message: a stale row, a shrunk log and a hole in the
    /// window call for different operator responses even though all three end in a refold.
    #[error(transparent)]
    Cache(#[from] CacheError),
}

/// A session host: one projection cache, the state folded into it, and how far it has folded.
///
/// Not `Clone` and not shared: the folded state is mutable by definition, and two handles on one
/// host would each assume they own the cursor. A caller that needs the state in two places shares
/// the *view*, which is a snapshot, rather than the host.
pub struct ProjectionHost {
    cache: ProjectionCache,
    /// The sequence number of the last event this host folded; the next one is `folded + 1`.
    ///
    /// 0 means nothing has been folded, which is also the state of an empty log, so the first event
    /// a host ever sees is sequence 1.
    folded: Seq,
}

impl ProjectionHost {
    /// Creates a host over a set of projections, with nothing folded.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::DuplicateKey`] when two definitions claim the same key. This is the
    /// cache's own construction check, surfaced under a name this crate exports; it is fallible
    /// rather than infallible because the alternative would be to keep one of the duplicates
    /// silently, and the discarded projection's readers would see another definition's state under
    /// their own name.
    pub fn new(projections: Vec<Arc<dyn VersionedProjection>>) -> Result<Self, HostError> {
        let cache = build_cache(projections)?;
        Ok(Self { cache, folded: 0 })
    }

    /// Creates a host from a checkpoint plus the log window the checkpoint asked for.
    ///
    /// This is the resume path, and the reason a cache exists: ask [`Registry::restore_floor`] for
    /// the first sequence the checkpoint can resume from, read only that suffix of the log, and hand
    /// both here. A session with a long history then costs the suffix rather than the whole log.
    ///
    /// `base_seq` is the anchor the floor named and is **inclusive**: the window must contain the
    /// event at `base_seq`, which `restore` then skips because the row already accounts for it.
    /// Starting after the anchor is refused rather than folded, because the events between the row
    /// and the window are exactly the ones that would be silently lost.
    ///
    /// `events` must hold the events from `base_seq` on, in order, exactly as
    /// [`ProjectionCache::restore`] describes; rows that are stale, missing or beyond the window are
    /// refused instead of being folded over.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::DuplicateKey`] for a repeated key, and [`HostError::Cache`] for any
    /// refusal the cache makes — a stale row, a window that starts after the row, a log that shrank
    /// under the checkpoint, or a hole in the window.
    pub fn resume(
        projections: Vec<Arc<dyn VersionedProjection>>,
        checkpoint: &Checkpoint,
        events: &[(Seq, SessionEvent)],
        base_seq: Seq,
    ) -> Result<Self, HostError> {
        let cache = build_cache(projections)?;
        let restored = cache.restore(checkpoint, events, base_seq)?;
        // The cache reports the watermark the restored cells actually reached, which is the end of
        // the window and not the row's own sequence: a row at 4 resumed with a window ending at 8
        // covers 8, and continuing at 5 would fold 5..8 a second time. The cache knows this number
        // because it just did the folds; asking it is what keeps the cursor from being a guess.
        Ok(Self {
            cache,
            folded: restored.as_of,
        })
    }

    /// Folds a run of events into the cache, numbering them from the cursor.
    ///
    /// Only the events in this call are folded. The cache holds the previous state per projection,
    /// so a call with one event runs one fold per projection regardless of how much history the
    /// session has — that is the entire point of owning the cache.
    ///
    /// A failure stops the run: the cache refuses a sequence it cannot continue from, and folding
    /// the events after it would leave the state inconsistent for every later read while looking
    /// like a partial success. The events already folded stay folded — they were accepted at their
    /// own sequence numbers — and the cursor names the last of them, so a retry resumes there.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Gap`] when the event is not the one after the last folded sequence.
    /// The host numbers what it is given, so this cannot happen to a host that owns its cache from
    /// construction; it is propagated rather than asserted because the cache is the authority on
    /// whether its state can continue, and a host that swallowed a refusal would serve corrupt
    /// state as current.
    pub fn observe(&mut self, events: &[SessionEvent]) -> Result<(), CacheError> {
        for event in events {
            let seq = self.folded.saturating_add(1);
            self.cache.observe(event, seq)?;
            self.folded = seq;
        }
        Ok(())
    }

    /// The current view: one entry per projection, keyed by projection key.
    ///
    /// Sorted by key, because a view is rendered from it and an order that changed between runs
    /// would make the UI jump. The map is copied out rather than referenced so a caller can hold it
    /// while the host keeps folding.
    #[must_use]
    pub fn view(&self) -> serde_json::Map<String, serde_json::Value> {
        self.cache.snapshot().values.into_iter().collect()
    }

    /// The sequence number of the last event folded; 0 when nothing has been folded.
    ///
    /// This is the next fold's starting point — the next event is `cursor() + 1` — so a caller that
    /// reattaches to a log resumes here. Off by one in either direction either replays an event or
    /// loses one, which is why the host derives it from accepted folds rather than from the log's
    /// length.
    #[must_use]
    pub fn cursor(&self) -> Seq {
        self.folded
    }

    /// The checkpoint rows for this host, as JSON, when there is anything to persist.
    ///
    /// `None` means no projection has folded an event, so there is no row to write; a caller stores
    /// nothing and refolds from the log, which produces the same state. The value round-trips
    /// through [`Checkpoint`] and is what [`ProjectionHost::resume`] takes back.
    ///
    /// A serialization failure is also reported as `None` rather than as an error: the alternative
    /// is a caller storing a row it cannot read back, and refolding is always a correct answer for
    /// a cache that cannot be written.
    #[must_use]
    pub fn checkpoint(&self) -> Option<serde_json::Value> {
        let checkpoint = self.cache.checkpoint();
        if checkpoint.is_empty() {
            return None;
        }
        serde_json::to_value(checkpoint).ok()
    }
}

impl std::fmt::Debug for ProjectionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The folded state is not printed: a projection can hold a transcript, and a debug line
        // should name the host rather than dump it. Keys and cursor are what a caller needs.
        f.debug_struct("ProjectionHost")
            .field("projections", &self.cache.keys())
            .field("cursor", &self.folded)
            .finish_non_exhaustive()
    }
}

/// A named set of projection definitions that hosts are built from.
///
/// The registry exists because a host is per session and the definitions are per deployment: a
/// service registers its projections once and mints one host per session it attaches, so adding a
/// projection is one line in one place instead of a change at every construction site.
///
/// Named `Registry` inside this module for the same reason the cache and the fold are named after
/// what they hold; it is deliberately not the crate's [`girder_core::Registry`], which resolves
/// services rather than projections.
#[derive(Default)]
pub struct Registry {
    /// Ordered by key so two hosts built from one registry fold in the same order, which keeps a
    /// debugging session's view comparable to the next run's.
    definitions: BTreeMap<&'static str, Arc<dyn VersionedProjection>>,
}

impl Registry {
    /// Creates a registry with no projections.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a projection, refusing a key that is already taken.
    ///
    /// A duplicate is an error rather than a replacement, matching [`ProjectionCache::new`]: two
    /// definitions under one key would write one checkpoint row, and a silent replacement would
    /// make the previous projection's readers see someone else's state.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::DuplicateKey`] when the key is already registered.
    pub fn register(&mut self, projection: Arc<dyn VersionedProjection>) -> Result<(), HostError> {
        let key = projection.key();
        if self.definitions.contains_key(key) {
            return Err(HostError::DuplicateKey {
                key: key.to_owned(),
            });
        }
        self.definitions.insert(key, projection);
        Ok(())
    }

    /// The registered keys, sorted.
    ///
    /// Sorted because it is what a host renders its view from, and an order that varied between
    /// runs would make the view jump.
    #[must_use]
    pub fn keys(&self) -> Vec<&'static str> {
        self.definitions.keys().copied().collect()
    }

    /// Creates a host over the registered projections, with nothing folded.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::DuplicateKey`] only if a duplicate reached the definitions behind
    /// registration's back. The check is propagated rather than asserted because a panic in a
    /// constructor is a worse failure than an error a caller can handle.
    pub fn host(&self) -> Result<ProjectionHost, HostError> {
        ProjectionHost::new(self.definitions.values().cloned().collect())
    }

    /// The first sequence a checkpoint can resume from, given these definitions.
    ///
    /// The resume path needs this number *before* it can build a host — it decides how much of the
    /// log to read — and the host's cache is private by design, so the answer belongs to the
    /// registry that holds the definitions. It delegates to the cache's own floor rather than
    /// computing one: the floor is the minimum anchor across the projections, and a second
    /// implementation of that rule would be a second thing to keep in step.
    ///
    /// `None` means there are no projections, so there is nothing to read and an empty host is the
    /// correct result.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::DuplicateKey`] only if a duplicate reached the definitions behind
    /// registration's back.
    pub fn restore_floor(&self, checkpoint: &Checkpoint) -> Result<Option<Seq>, HostError> {
        let cache = build_cache(self.definitions.values().cloned().collect())?;
        Ok(cache.restore_floor(checkpoint))
    }

    /// Creates a host from a checkpoint plus the log window the checkpoint asked for.
    ///
    /// The paired half of [`Registry::restore_floor`]: call it with the same definitions and the same
    /// checkpoint, and the anchor it returned as `base_seq`. The window is inclusive of the anchor —
    /// see [`ProjectionHost::resume`] for why.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::DuplicateKey`] for a repeated key, and [`HostError::Cache`] for any
    /// refusal the cache makes on the way in.
    pub fn resume(
        &self,
        checkpoint: &Checkpoint,
        events: &[(Seq, SessionEvent)],
        base_seq: Seq,
    ) -> Result<ProjectionHost, HostError> {
        ProjectionHost::resume(
            self.definitions.values().cloned().collect(),
            checkpoint,
            events,
            base_seq,
        )
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionSet")
            .field("projections", &self.keys())
            .finish()
    }
}

/// Builds the cache, turning the cache's unexported duplicate-key error into this module's error.
///
/// A free function so `new` and `resume` cannot disagree about what a failed construction means; the
/// mapping reads `DuplicateKey`'s public field rather than naming the type, because the crate root
/// does not re-export it and a host that could not describe its own failure would have to panic.
fn build_cache(
    projections: Vec<Arc<dyn VersionedProjection>>,
) -> Result<ProjectionCache, HostError> {
    ProjectionCache::new(projections)
        .map_err(|duplicate| HostError::DuplicateKey { key: duplicate.key })
}
