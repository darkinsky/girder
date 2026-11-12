//! Versioned projection caches: resume a fold from a checkpoint instead of replaying the log.
//!
//! [`crate::projection`] folds an event log into state and keeps that state in memory for the life of
//! a process. That is the right shape while a session is open, and the wrong shape at the moment a
//! session is *attached*: `ProjectedSession::attach` replays the entire log from `init` because it has
//! nothing else to start from. The log is append-only and grows without bound, so the cost of opening
//! a session climbs with its history — on exactly the path a UI hits every time it lists sessions.
//!
//! This module adds the missing half of a cache: the part that survives a process. A cache writes
//! [`Checkpoint`] rows — one per projection, each carrying the fold's state, the sequence number it
//! covers, and the definition's [`VersionedProjection::state_version`] — and a later process asks
//! [`ProjectionCache::restore_floor`] where to resume reading, reads only that suffix of the log, and
//! calls [`ProjectionCache::restore`].
//!
//! # Why a version, and not just a sequence number
//!
//! A row is only usable if the code that folded it is the code that will read it. Changing what a
//! projection means — adding a field it accumulates, moving a boundary — leaves old rows
//! syntactically valid and semantically wrong, and the failure is silent: the state looks like JSON.
//! A version that a definition bumps when its fold changes turns that silent corruption into a
//! refold.
//!
//! # Why a row that is too far ahead is an error rather than a refold
//!
//! A row claiming sequence 40 when the log now ends at 30 means the log shrank under the checkpoint —
//! a crash-repair truncation. Folding the remaining 30 events on top of a state that already covers 40
//! would silently double-apply them, so every such case is refused and the caller re-reads from the
//! start. That is the one situation where paying the full replay is the only correct answer.
//!
//! # Why a gap is refused rather than folded
//!
//! [`ProjectionCache::observe`] checks that the sequence it receives is the one after the sequence it
//! has folded. The in-memory [`crate::ProjectionRegistry`] deliberately tolerates a jump — it only
//! compares against its cursor — and a jumped fold is indistinguishable from a correct one afterwards.
//! A cache that can be persisted cannot afford that: the wrong state would be written to disk and
//! outlive the mistake. Detecting the gap at the one place events enter is what keeps the persisted
//! state trustworthy.
//!
//! # Why the definitions are held here
//!
//! `restore` needs each definition's `init` and `state_version` to decide whether a row applies. A
//! caller that passed them in per call would have to keep them in step with the cache by hand, so the
//! cache owns them and the two cannot disagree.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use girder_core::{Projection, ProjectionSeq as Seq, SessionEvent};
use serde::{Deserialize, Serialize};

/// A projection whose persisted state carries a version.
///
/// Separate from [`Projection`] rather than a method on it: a projection is a pure fold and knows
/// nothing about being persisted, and requiring a version on every definition would force that
/// concern on the ones that are never cached. The version lives here, where caching begins.
pub trait VersionedProjection: Projection {
    /// The version of this projection's persisted state.
    ///
    /// A non-negative integer that a definition bumps whenever its `init` or `apply` changes the
    /// meaning of a stored state. Rows written under a different version are discarded and refolded,
    /// which is the only safe response to a changed fold.
    fn state_version(&self) -> u64;
}

/// A projection paired with a state version, for definitions that do not declare their own.
///
/// An adapter rather than a blanket implementation: a blanket `impl<V: Projection> VersionedProjection
/// for V` would make every projection cacheable at whatever version the adapter chose, and a caller
/// who forgot to bump it would get stale rows instead of a compile-time decision about the version.
pub struct Versioned {
    projection: Arc<dyn Projection>,
    state_version: u64,
}

impl Versioned {
    /// Pairs a projection with the version of the state it persists.
    #[must_use]
    pub fn new(projection: Arc<dyn Projection>, state_version: u64) -> Self {
        Self {
            projection,
            state_version,
        }
    }

    /// The projection being versioned.
    #[must_use]
    pub fn projection(&self) -> &Arc<dyn Projection> {
        &self.projection
    }
}

impl Projection for Versioned {
    fn key(&self) -> &'static str {
        self.projection.key()
    }

    fn init(&self) -> serde_json::Value {
        self.projection.init()
    }

    fn apply(&self, state: serde_json::Value, event: &SessionEvent, seq: Seq) -> serde_json::Value {
        self.projection.apply(state, event, seq)
    }

    fn in_summary(&self) -> bool {
        self.projection.in_summary()
    }
}

impl VersionedProjection for Versioned {
    fn state_version(&self) -> u64 {
        self.state_version
    }
}

/// Why a cache operation failed.
///
/// Every variant means "this state cannot be trusted", never "retry with the same input": each one
/// names an inconsistency between the checkpoint and the log, and the caller's fix is to refold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CacheError {
    /// The next event is not the one after the last folded event.
    #[error("projection `{key}` folded to seq {folded} but was offered seq {found}")]
    Gap {
        /// The projection.
        key: String,
        /// The last sequence folded.
        folded: Seq,
        /// The sequence offered.
        found: Seq,
    },

    /// A checkpoint row was written by a different version of the definition.
    #[error(
        "projection `{key}` checkpoint is at version {row_version}, this build expects {expected}"
    )]
    StaleRow {
        /// The projection.
        key: String,
        /// The version in the row.
        row_version: u64,
        /// The version this build declares.
        expected: u64,
    },

    /// A checkpoint row claims more of the log than the log still holds.
    #[error(
        "projection `{key}` checkpoint claims seq {row_seq} but the supplied log ends at {end_seq}"
    )]
    RowBeyondLogEnd {
        /// The projection.
        key: String,
        /// The sequence in the row.
        row_seq: Seq,
        /// The last sequence supplied.
        end_seq: Seq,
    },

    /// The supplied log window starts after the point the row would have to be refolded from.
    #[error("projection `{key}` cannot restore from seq {base_seq}: no usable row at or before it")]
    WindowTooLate {
        /// The projection.
        key: String,
        /// The first sequence supplied.
        base_seq: Seq,
        /// The sequence in the row, when one was present.
        row_seq: Option<Seq>,
    },

    /// An event list skipped a sequence number.
    #[error("projection `{key}` expected seq {expected} but the log supplied {found}")]
    MissingSeq {
        /// The projection.
        key: String,
        /// The sequence the fold needed next.
        expected: Seq,
        /// The sequence that was supplied.
        found: Seq,
    },
}

/// One projection's persisted state.
///
/// `seq` is the last event folded into `val`, so a reader knows exactly how much of the log the state
/// accounts for without trusting the log's length.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRow {
    /// The definition's state version at fold time.
    pub ver: u64,
    /// The last sequence folded into `val`; 0 for an empty log.
    pub seq: Seq,
    /// The projection's state, as JSON, per the projection contract.
    pub val: serde_json::Value,
}

/// A session's persisted rows, keyed by projection key.
///
/// A newtype over the map rather than a bare map: it is what crosses a storage boundary, and giving
/// it a name is what lets `restore` take a checkpoint instead of "some map a caller happened to have".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// One row per projection that has been folded.
    pub rows: BTreeMap<String, CheckpointRow>,
}

impl Checkpoint {
    /// An empty checkpoint, the state of a session that has never been cached.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One project's row, when the checkpoint holds it.
    #[must_use]
    pub fn row(&self, key: &str) -> Option<&CheckpointRow> {
        self.rows.get(key)
    }

    /// Whether the checkpoint holds no rows at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// A consistent read of every projection at one sequence number.
///
/// `as_of` is the shared watermark: the last sequence every value reflects, and 0 for an empty log.
/// Carrying it is what lets a caller tell "these values are current" from "these values are as of
/// three events ago" — a difference that matters once values can come from a checkpoint rather than a
/// live fold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The last sequence the values reflect; 0 for an empty log.
    pub as_of: Seq,
    /// One value per projection.
    pub values: BTreeMap<String, serde_json::Value>,
}

impl Snapshot {
    /// One projection's value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.values.get(key)
    }
}

/// One projection's folded state and how far it has been folded.
#[derive(Clone, Debug)]
struct Cell {
    state: serde_json::Value,
    observed: Seq,
}

/// The projections a session caches, and the state folded so far.
///
/// Per session: a checkpoint is a session's cache row set, and a cache that held several sessions
/// would have to key every read by one anyway. One cache per session keeps the watermark unambiguous.
pub struct ProjectionCache {
    definitions: Vec<Arc<dyn VersionedProjection>>,
    cells: Mutex<BTreeMap<String, Cell>>,
}

impl ProjectionCache {
    /// Builds a cache over the given projections.
    ///
    /// The definitions are validated at construction rather than at first use: a duplicate key would
    /// make two projections write one checkpoint row, and the loser's state would be silently
    /// discarded on every restore.
    ///
    /// # Errors
    ///
    /// Returns [`DuplicateKey`] when two definitions claim the same key.
    pub fn new(definitions: Vec<Arc<dyn VersionedProjection>>) -> Result<Self, DuplicateKey> {
        let mut seen: Vec<&'static str> = Vec::with_capacity(definitions.len());
        for definition in &definitions {
            let key = definition.key();
            if seen.contains(&key) {
                return Err(DuplicateKey {
                    key: key.to_owned(),
                });
            }
            seen.push(key);
        }
        Ok(Self {
            definitions,
            cells: Mutex::new(BTreeMap::new()),
        })
    }

    /// The registered keys, sorted.
    ///
    /// Sorted for the same reason the registry sorts them: a view rendered from these would jump if
    /// the order changed between runs.
    #[must_use]
    pub fn keys(&self) -> Vec<&'static str> {
        let mut keys: Vec<&'static str> = self.definitions.iter().map(|d| d.key()).collect();
        keys.sort_unstable();
        keys
    }

    /// Folds one event into every projection, refusing to skip a sequence number.
    ///
    /// The gap check is a separate pass before any fold: a failure halfway through the definitions
    /// would leave some projections advanced and others not, and a cache with two watermarks for one
    /// event has no correct checkpoint to write.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Gap`] when `seq` is more than one past a projection's watermark. A repeat
    /// is not an error: it is skipped, because the caller is a subscription.
    pub fn observe(&self, event: &SessionEvent, seq: Seq) -> Result<(), CacheError> {
        {
            let cells = self.lock_cells();
            for definition in &self.definitions {
                let folded = cells.get(definition.key()).map_or(0, |cell| cell.observed);
                if seq > folded.saturating_add(1) {
                    return Err(CacheError::Gap {
                        key: definition.key().to_owned(),
                        folded,
                        found: seq,
                    });
                }
            }
        }
        let mut cells = self.lock_cells();
        for definition in &self.definitions {
            let key = definition.key().to_owned();
            let cell = cells.entry(key).or_insert_with(|| Cell {
                state: definition.init(),
                observed: 0,
            });
            if seq <= cell.observed {
                // Idempotent by sequence: the caller is a subscription, and a subscription can deliver
                // the same event twice.
                continue;
            }
            cell.state = definition.apply(cell.state.clone(), event, seq);
            cell.observed = seq;
        }
        Ok(())
    }

    /// The current state of every projection.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let cells = self.lock_cells();
        let mut as_of = Seq::MAX;
        let mut values = BTreeMap::new();
        for definition in &self.definitions {
            let cell = cells.get(definition.key());
            values.insert(
                definition.key().to_owned(),
                cell.map_or_else(|| definition.init(), |cell| cell.state.clone()),
            );
            as_of = as_of.min(cell.map_or(0, |cell| cell.observed));
        }
        if values.is_empty() {
            as_of = 0;
        }
        Snapshot { as_of, values }
    }

    /// One projection's current state.
    #[must_use]
    pub fn read(&self, key: &str) -> Option<serde_json::Value> {
        self.lock_cells().get(key).map(|cell| cell.state.clone())
    }

    /// The rows a durable writer persists for this session.
    ///
    /// A value is cloned out rather than referenced: the cache's cells are its authoritative mutable
    /// state, and a caller holding the live reference could change every later read and every later
    /// checkpoint through it.
    #[must_use]
    pub fn checkpoint(&self) -> Checkpoint {
        let cells = self.lock_cells();
        let mut rows = BTreeMap::new();
        for definition in &self.definitions {
            let cell = cells.get(definition.key());
            let Some(cell) = cell else {
                continue;
            };
            rows.insert(
                definition.key().to_owned(),
                CheckpointRow {
                    ver: definition.state_version(),
                    seq: cell.observed,
                    val: cell.state.clone(),
                },
            );
        }
        Checkpoint { rows }
    }

    /// The first sequence a restoring reader must supply.
    ///
    /// The *lowest* anchor across the projections, because one window has to satisfy all of them and
    /// each can only resume from its own last folded sequence. A projection whose row is missing or
    /// was written by another version anchors at 0, which pulls the whole window back to the start of
    /// the log — the price of one stale row is a full refold, and paying it for one key is cheaper
    /// than serving a wrong state for it.
    ///
    /// The anchor is inclusive: the caller reads from this sequence on, and `restore` skips the events
    /// already accounted for. Reading the anchor event itself is what lets a truncated log be noticed
    /// — a log that ends before the anchor yields no event at all, and `restore` then refuses rather
    /// than serving a row that covers history the log no longer has.
    ///
    /// `None` means no projections are registered, so there is nothing to read.
    #[must_use]
    pub fn restore_floor(&self, checkpoint: &Checkpoint) -> Option<Seq> {
        let mut floor: Option<Seq> = None;
        for definition in &self.definitions {
            let anchor = checkpoint
                .rows
                .get(definition.key())
                .filter(|row| row.ver == definition.state_version())
                .map_or(0, |row| row.seq);
            floor = Some(floor.map_or(anchor, |current: Seq| current.min(anchor)));
        }
        floor
    }

    /// Rebuilds every projection from a checkpoint plus the log suffix, installing the result.
    ///
    /// `events` must hold the events from `base_seq` on, in order — the window
    /// [`ProjectionCache::restore_floor`] asked for. Each projection is seeded from its row when the
    /// row is usable and folded forward from the events the row does not already cover.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::StaleRow`], [`CacheError::RowBeyondLogEnd`] or
    /// [`CacheError::WindowTooLate`] when a row cannot seed the fold and the window does not start at
    /// the beginning of the log, [`CacheError::MissingSeq`] when the window has a hole, and
    /// [`CacheError::Gap`] never — a gap is a live-drive condition.
    pub fn restore(
        &self,
        checkpoint: &Checkpoint,
        events: &[(Seq, SessionEvent)],
        base_seq: Seq,
    ) -> Result<Snapshot, CacheError> {
        let end_seq = events
            .last()
            .map_or(base_seq.saturating_sub(1), |(seq, _)| *seq);
        let mut restored: BTreeMap<String, Cell> = BTreeMap::new();

        for definition in &self.definitions {
            let key = definition.key().to_owned();
            let expected_version = definition.state_version();
            let row = checkpoint.rows.get(definition.key());
            let usable = row.is_some_and(|row| {
                row.ver == expected_version && row.seq >= base_seq && row.seq <= end_seq
            });

            let (mut state, mut folded) = match (usable, row) {
                (true, Some(row)) => (row.val.clone(), row.seq),
                (false, _) if base_seq > 0 => {
                    return Err(refusal(key, expected_version, row, base_seq, end_seq));
                }
                // The window covers the whole log, so a missing or unusable row is refolded from the
                // beginning rather than refused.
                _ => (definition.init(), base_seq.saturating_sub(1)),
            };

            for (seq, event) in events {
                if *seq <= folded {
                    // Already accounted for by the row, or before this projection's anchor because a
                    // sibling's row was older. Re-applying it would fold one event twice.
                    continue;
                }
                let expected = folded.saturating_add(1);
                if *seq != expected {
                    return Err(CacheError::MissingSeq {
                        key,
                        expected,
                        found: *seq,
                    });
                }
                state = definition.apply(state, event, *seq);
                folded = *seq;
            }

            restored.insert(
                key,
                Cell {
                    state,
                    observed: end_seq,
                },
            );
        }

        let mut values = BTreeMap::new();
        for definition in &self.definitions {
            if let Some(cell) = restored.get(definition.key()) {
                values.insert(definition.key().to_owned(), cell.state.clone());
            }
        }

        {
            let mut cells = self.lock_cells();
            cells.clear();
            for (key, cell) in restored {
                cells.insert(key, cell);
            }
        }

        Ok(Snapshot {
            as_of: end_seq,
            values,
        })
    }

    /// The values a checkpoint can serve without reading the log.
    ///
    /// Rows whose version does not match are omitted rather than refolded: this is the zero-I/O rung
    /// of the read ladder, and a caller that needs the values must fall back to
    /// [`ProjectionCache::restore`]. A value here is as stale as its row, never wrong.
    ///
    /// The cache's own cells are deliberately untouched, so a listing path that only wants to show a
    /// sidebar cannot accidentally claim the state has been advanced.
    #[must_use]
    pub fn view_checkpoint(&self, checkpoint: &Checkpoint) -> Snapshot {
        let mut values = BTreeMap::new();
        let mut as_of: Option<Seq> = None;
        for definition in &self.definitions {
            let Some(row) = checkpoint.rows.get(definition.key()) else {
                continue;
            };
            if row.ver != definition.state_version() {
                continue;
            }
            values.insert(definition.key().to_owned(), row.val.clone());
            as_of = Some(as_of.map_or(row.seq, |current: Seq| current.min(row.seq)));
        }
        Snapshot {
            as_of: as_of.unwrap_or(0),
            values,
        }
    }

    fn lock_cells(&self) -> MutexGuard<'_, BTreeMap<String, Cell>> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for ProjectionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The folded state is not printed: a projection can hold a transcript, and a debug line should
        // name the cache rather than dump it.
        f.debug_struct("ProjectionCache")
            .field("definitions", &self.keys())
            .finish_non_exhaustive()
    }
}

/// Why a cache could not be built.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a projection named `{key}` is already registered in this cache")]
pub struct DuplicateKey {
    /// The key registered twice.
    pub key: String,
}

/// The most specific reason a row cannot seed a fold.
///
/// Factored out of [`ProjectionCache::restore`] so the three refusals stay distinguishable: a stale
/// version, a shrunk log, and a window that started too late call for different operator responses
/// even though all three end in a refold.
fn refusal(
    key: String,
    expected_version: u64,
    row: Option<&CheckpointRow>,
    base_seq: Seq,
    end_seq: Seq,
) -> CacheError {
    match row {
        None => CacheError::WindowTooLate {
            key,
            base_seq,
            row_seq: None,
        },
        Some(row) if row.ver != expected_version => CacheError::StaleRow {
            key,
            row_version: row.ver,
            expected: expected_version,
        },
        Some(row) if row.seq > end_seq => CacheError::RowBeyondLogEnd {
            key,
            row_seq: row.seq,
            end_seq,
        },
        Some(row) => CacheError::WindowTooLate {
            key,
            base_seq,
            row_seq: Some(row.seq),
        },
    }
}
