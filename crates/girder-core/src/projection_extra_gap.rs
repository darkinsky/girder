//! The persisted projection cache: which stored checkpoint may seed which session's fold.
//!
//! [`crate::projection_extra`] answered "how do checkpoint rows round-trip": a row carries `ver`, `seq`
//! and `val`, a stale row is refolded, a row past the log end is refused. That module can be handed a
//! checkpoint and a log and it will produce the right state — for the session the checkpoint came from.
//! It has no way to tell *whether it did*, and that is the gap this module closes.
//!
//! # Why a row set needs an identity at all
//!
//! A session id names a slot, not a lifecycle. The same id can be deleted and recreated, and a
//! persistence root can be swapped under a cache that survives the swap. In both cases a stored record
//! passes every watermark check [`crate::ProjectionCache::restore_floor`] makes — its rows are
//! syntactically valid, its versions match, its sequence numbers sit inside the log — and seeds a fold
//! from a log it was never folded from. The failure is silent and total: every projection of that
//! session is wrong from the first read, and the next checkpoint writes the wrong state back under the
//! new lifecycle, so a later reader inherits it.
//!
//! Upstream therefore binds each stored record to the immutable header fields that distinguish one
//! lifecycle from another — the format generation the fold ran under, the creation time, the working
//! directory, whether the session was seeded, and (for the fold face only) the exact inherited cut —
//! and refuses a record whose identity is not the caller's. That machinery exists nowhere in this
//! crate: [`crate::CheckpointRow`] has no identity, [`crate::ProjectionCache`] cannot see a header, and
//! no other module in the workspace mentions `formatVersion`, `isSeeded` or an inherited cut.
//!
//! # The three faces, and why the ladder is not one predicate
//!
//! Upstream does not use one equality test, because three kinds of reader hold three different amounts
//! of evidence:
//!
//! - **A listing** holds only a header (no log, no body): [`current_lifecycle_matches`]. It checks the
//!   format generation and the lifecycle fields, and deliberately ignores the inherited cut — within
//!   one format generation the cut is fixed at fork time, so it distinguishes no lifecycle the other
//!   fields do not. A viewed value never seeds a fold, so ignoring it is safe; requiring it would make
//!   a seeded session unlistable, since a header cannot witness its own cut.
//! - **A fold** (hydration, cold read, checkpoint write) holds the session, so it holds the exact cut:
//!   [`identity_matches`]. A record folded under another cut encodes that cut in the unit states it
//!   holds, and continuing that fold would carry it forward into the next checkpoint.
//! - **A title hint** wants to serve a predecessor's title to a listing: [`predecessor_identity_matches`].
//!   It accepts an absent or older format generation, because a title is normalized across generations
//!   and the row version plus schema still gate it. No other projection is exposed this way: format
//!   normalization can change what a stored state means, and the other two faces keep rejecting it.
//!
//! Neither predicate is a relaxation of the next: an absent format generation is *refused* by the
//! current-generation check (no current log can prove that record's fold semantics) and *accepted* by
//! the predecessor check. Collapsing them into one would either let a pre-format record seed a fold or
//! hide every predecessor title.
//!
//! # What this module is careful not to duplicate
//!
//! - **Not the client merge.** [`crate::session_engine_gap::ProjectionValueStore`] decides precedence
//!   between values a client already holds: a `sequenced` frame beats a `cached` block, a baseline
//!   sweeps every cached row. That is the *consumer* side, comparing watermarks it was handed. This
//!   module is the *producer* side: it decides whether a durable record may be turned into a block at
//!   all, before any client sees one. The two meet at exactly one place — a value this module serves
//!   as `cached` — and neither can answer the other's question.
//! - **Not the fold.** [`crate::ProjectionCache`] still owns `restore`/`view_checkpoint`/`checkpoint`;
//!   this module calls them. Reimplementing the fold here would create a second definition of what a
//!   checkpoint row means, which is the failure mode the crate has been avoiding throughout.
//! - **Not the storage medium.** Upstream's `session_projcache` domain is a `per-record` table on a
//!   json backend. This crate has no storage-domain layer, so a [`CheckpointStore`] trait stands in
//!   for it and [`MemoryStore`] is the shipped implementation: it keeps raw documents (so a malformed
//!   one is quarantined exactly as the medium would move it aside) and decodes on read.
//! - **Not the timers.** Upstream's write-behind arms a `setTimeout` per session. A timer is a
//!   mechanism, and the crate has no render loop or scheduler to hand it to, so [`WriteBehind`] keeps
//!   the *policy* — the dirty counter, the two thresholds, the three mandatory points — and reports
//!   which sessions the interval has come due for. A caller with a scheduler drives [`WriteBehind::due`]
//!   from it; the decisions are identical either way.
//!
//! # The invariants that are load-bearing, and what breaks without each
//!
//! - **The durability barrier runs after the cut and before the write.** [`PersistedProjectionCache::write`]
//!   takes the checkpoint cut, then flushes the session log, then writes the record. Flipping the first
//!   two lets a crash leave a cache row ahead of the log — values folded from events no stored log
//!   contains, which no later read can detect and no refold can repair. This ordering is why the cut is
//!   a closure rather than an argument: a caller that computed it earlier would silently move the cut
//!   before the barrier.
//! - **A record is fail-soft on the way out, fail-hard on the way in.** A write failure costs a longer
//!   tail replay on the next cold read and nothing else, so [`PersistedProjectionCache::cold_snapshot`]
//!   reports the failure instead of losing the snapshot. A *malformed* record never reaches the fold:
//!   [`MemoryStore`] quarantines it and serves the session as uncached, because derived data must never
//!   cost a boot.
//! - **A checkpoint must be lossless JSON before it lands.** Upstream refuses a state that would not
//!   survive the round trip instead of storing a mangled one, and clearing the dirty bookkeeping before
//!   that check is what keeps a poisoned projection from rewriting its failure on every event.
//!
//! # What is deliberately not ported
//!
//! - **The schema layer.** Upstream validates a row's `val` against the unit's `stateSchema` before
//!   viewing it, and omits a key whose stored state fails. [`crate::Projection`] has no schema — its
//!   state is "JSON, per contract" — so a view here omits only version-mismatched rows. Adding a
//!   validator to the trick would require the trait change this module is not allowed to make; it is
//!   called out as the next gap in the report rather than approximated.
//! - **The wire view.** Upstream's `viewCheckpoint` applies each unit's `view` and `viewSchema`; this
//!   crate's fold has no wire concept, so a viewed value is the unit's state. The watermark arithmetic
//!   and key selection — the parts that decide *whether* a block is served — are unaffected.
//! - **`-1` cursors.** Upstream's `seq` is `-1` for an empty log; this crate's [`crate::ProjectionSeq`]
//!   is 1-based with `0` meaning "nothing folded". The mapping is `local = upstream + 1` throughout, so
//!   a stored row's `seq` and a served `as_of` stay consistent with [`crate::ProjectionCache`]; the
//!   upstream schema rule `seq >= -1` is structurally unrepresentable here.
//!
//! # Module-include note
//!
//! This file and its test are included by path from `tests/projection_extra_gap.rs`, so every reference
//! to the library goes through `girder_core::…` rather than `crate::…`: inside a `#[path]` module,
//! `crate::` names the *test* crate.

// The crate root declares this module, so its `pub` items are reached from outside the library and the
// library build is clean. The *test* build is not: `tests/projection_extra_gap.rs` compiles this file by
// path as a private module, where the items the test does not touch — the accessors a caller would use —
// are legitimately dead. `projection_host` carries the same exemption for the same reason.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};

use girder_core::session_final_gap::HeaderFacts;
use girder_core::{
    CacheError, Checkpoint, CheckpointRow, ProjectionCache, ProjectionSeq as Seq,
    ProjectionSnapshot, SessionEvent, is_lossless,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The projection key whose predecessor row a listing may serve as a hint.
///
/// Only the title: it is the one projection upstream allows across a format generation, because a title
/// is normalized rather than folded from semantically fixed events, and because the row version and
/// schema still gate it. Every other key keeps the two stricter faces.
pub const PREDECESSOR_TITLE_KEY: &str = "title";

// -------------------------------------------------------------------------------------------------
// The domain
// -------------------------------------------------------------------------------------------------

/// How a domain's records are laid out on the medium.
///
/// Upstream's `per-record` layout scopes a version bump per session: after a bump, one stale session
/// document is discarded on open while the rest of the domain stays usable. A single-unit layout would
/// reject the whole medium instead, which is the difference between one slow session and a broken boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// One document per session, keyed by session id.
    PerRecord,
}

/// What a domain does with a stored record that fails its schema anyway.
///
/// A stored record is disposable derived data, so it must never cost the boot: the medium moves the
/// document aside as `<key>.json.bak.<stamp>`, logs the concrete failure, and serves the session as
/// uncached. A cold read then rebuilds the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidRecords {
    /// Move the document aside and continue as if it were absent.
    BackupAndSkip,
}

/// The identity, version, layout and retention policy of the projection-cache domain.
///
/// Upstream derives all of it from one `defineDomain` call; the values here are the same ones, spelled
/// out so a reader can see what a stored document's `version` has to be for this build to accept it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DomainSpec {
    /// The domain name on the medium.
    pub name: &'static str,
    /// The version this build writes.
    pub version: u64,
    /// Older versions whose records are structurally valid and stay readable.
    pub compatible_versions: &'static [u64],
    /// What to do with a record that fails the schema.
    pub invalid_records: InvalidRecords,
    /// How records are laid out.
    pub layout: Layout,
}

impl DomainSpec {
    /// The `session_projcache` domain.
    ///
    /// Version `7` with `3..=6` compatible, matching upstream. The compatible entries keep structurally
    /// valid predecessor records available for a later current checkpoint rewrite; a record admitted
    /// through them still has to pass the identity and per-row version guards before it seeds a fold.
    pub const SESSION_PROJCACHE: Self = Self {
        name: "session_projcache",
        version: 7,
        compatible_versions: &[3, 4, 5, 6],
        invalid_records: InvalidRecords::BackupAndSkip,
        layout: Layout::PerRecord,
    };

    /// Whether a stored document's version is one this build reads.
    ///
    /// A *newer* version is refused, not clamped: a record written by a future build may carry states
    /// whose fold semantics this one cannot reproduce, and treating it as current is how a downgrade
    /// silently corrupts a cache.
    #[must_use]
    pub fn accepts_version(&self, version: u64) -> bool {
        version == self.version || self.compatible_versions.contains(&version)
    }
}

// -------------------------------------------------------------------------------------------------
// Identity
// -------------------------------------------------------------------------------------------------

/// The stored identity a checkpoint record is bound to.
///
/// Every field past `created_at` is optional because a record admitted through
/// [`DomainSpec::compatible_versions`] predates it. An absent format generation is *not* read as "assume
/// current": no current session log can prove which event semantics produced those rows, which is why
/// [`current_lifecycle_matches`] refuses it and only [`predecessor_identity_matches`] accepts it.
/// Absent lineage (`is_seeded`, `inherited_event_count`) reads as the unseeded cut, which is exact for
/// an unseeded caller and fails a seeded one.
///
/// The wire spelling is upstream's: `formatVersion`, `createdAt`, `cwd`, `isSeeded`,
/// `inheritedEventCount`, with the optional keys omitted rather than written as `null`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointIdentity {
    /// The session format generation the fold ran under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_version: Option<u64>,
    /// The session's creation time in milliseconds since the Unix epoch.
    pub created_at: u64,
    /// The working directory the session was created in, when the header recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether the session inherited a prefix from a parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_seeded: Option<bool>,
    /// How many leading events came from the parent.
    ///
    /// Absent means the unseeded cut. A header cannot witness this — only a caller holding the session
    /// or its body can — which is why the listing faces ignore it and the fold faces require it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_event_count: Option<u64>,
}

/// The identity a session header alone witnesses.
///
/// Deliberately *not* the complete identity: a header records the format generation and the lifecycle
/// fields but not the inherited cut, so this type simply has no field for one. Making the cut
/// unavailable here is what keeps a header-only reader from accidentally gating on a number it cannot
/// know — the compiler enforces the distinction the doc comment describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleIdentity {
    /// The format generation the log is written in.
    pub format_version: u64,
    /// The session's creation time in milliseconds since the Unix epoch.
    pub created_at: u64,
    /// The working directory the session was created in, when the header recorded one.
    pub cwd: Option<String>,
    /// Whether the session inherited a prefix from a parent.
    pub is_seeded: bool,
}

/// The complete identity a fold face checks: the lifecycle plus the exact inherited cut.
///
/// Only a caller holding the session or its body can produce one, which is the whole point: hydration and
/// checkpoint writes are the two paths that continue a stored state, and both run where that cut is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentCheckpointIdentity {
    /// The fields a header witnesses.
    pub lifecycle: LifecycleIdentity,
    /// The exact inherited prefix length the fold starts from.
    pub inherited_event_count: u64,
}

impl CurrentCheckpointIdentity {
    /// The record identity a write stores, with all three current-generation fields present.
    ///
    /// A current write stores every field rather than only what differs from a default, so a later
    /// reader never has to decide whether an absent value means "unseeded" or "not recorded".
    #[must_use]
    pub fn stored(&self) -> CheckpointIdentity {
        CheckpointIdentity {
            format_version: Some(self.lifecycle.format_version),
            created_at: self.lifecycle.created_at,
            cwd: self.lifecycle.cwd.clone(),
            is_seeded: Some(self.lifecycle.is_seeded),
            inherited_event_count: Some(self.inherited_event_count),
        }
    }
}

/// Why a session header could not be projected into a complete identity.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// An unseeded session claimed an inherited prefix.
    ///
    /// Refused rather than normalized to `0`: the two are different logs. A session that inherited
    /// nothing has no parent prefix, so a non-zero cut names a different lifecycle — and folding from
    /// the wrong init would mis-place every projection in the log.
    #[error(
        "unseeded projection-cache identity inherited event count must be 0 (got {inherited_event_count})"
    )]
    UnseededInheritedCut {
        /// The cut the caller supplied.
        inherited_event_count: u64,
    },
}

/// Projects a header onto the identity fields a header alone can witness.
///
/// No validation happens here and none can: every field this reads is exactly a header field, so there
/// is nothing to reject. The inherited cut is the one thing a header cannot witness, and it is absent
/// from [`LifecycleIdentity`] by construction.
#[must_use]
pub fn lifecycle_identity_of(header: &HeaderFacts) -> LifecycleIdentity {
    LifecycleIdentity {
        format_version: header.version,
        created_at: header.created_at,
        cwd: header.cwd.clone(),
        is_seeded: header.is_seeded,
    }
}

/// Projects a header and its exact inherited cut onto the complete fold identity.
///
/// # Errors
///
/// Returns [`IdentityError::UnseededInheritedCut`] when an unseeded header claims a non-zero cut. The
/// cut is an argument rather than a header field because it lives on the session body, and a caller that
/// forgot to pass it would otherwise get a silently unseeded identity for a forked session.
pub fn identity_of(
    header: &HeaderFacts,
    inherited_event_count: u64,
) -> Result<CurrentCheckpointIdentity, IdentityError> {
    let lifecycle = lifecycle_identity_of(header);
    if !lifecycle.is_seeded && inherited_event_count != 0 {
        return Err(IdentityError::UnseededInheritedCut {
            inherited_event_count,
        });
    }
    Ok(CurrentCheckpointIdentity {
        lifecycle,
        inherited_event_count,
    })
}

/// Whether a stored record was folded from the caller's lifecycle at the current format.
///
/// This is the whole identity a header-only reader can check, and the whole identity a *view* needs: a
/// viewed value never seeds a fold, so the cut it was folded at cannot make the value wrong, only stale.
/// An absent format generation never matches — see [`predecessor_identity_matches`] for the one face
/// that accepts it.
#[must_use]
pub fn current_lifecycle_matches(
    stored: &CheckpointIdentity,
    expected: &LifecycleIdentity,
) -> bool {
    stored.format_version == Some(expected.format_version)
        && lifecycle_identity_matches(stored, expected)
}

/// Whether a stored record may seed the caller's fold: current generation, same lifecycle, exact cut.
///
/// A record folded under another cut encodes that cut in the unit states it holds (`schedule`,
/// `subagentCatalog`, `permissions` all derive from the inherited prefix), so continuing that fold would
/// carry a foreign cut into the continued states and into the next checkpoint. The cut is compared with
/// the documented default rather than refused when absent: a predecessor record admitted without lineage
/// is *exactly* an unseeded caller's record and a seeded caller's mismatch.
#[must_use]
pub fn identity_matches(stored: &CheckpointIdentity, expected: &CurrentCheckpointIdentity) -> bool {
    current_lifecycle_matches(stored, &expected.lifecycle)
        && stored.inherited_event_count.unwrap_or(0) == expected.inherited_event_count
}

/// Whether a stored record is a *predecessor* of the authoritative listed lifecycle.
///
/// An absent generation qualifies: records admitted through the compatible versions predate the field
/// and cannot prove their semantics, so they may serve a listing hint and never seed a fold. A record at
/// the caller's own generation or newer is not a predecessor. The lifecycle fields must still match —
/// being older does not make a record about a different session.
#[must_use]
pub fn predecessor_identity_matches(
    stored: &CheckpointIdentity,
    expected: &LifecycleIdentity,
) -> bool {
    let predecessor = stored
        .format_version
        .is_none_or(|version| version < expected.format_version);
    predecessor && lifecycle_identity_matches(stored, expected)
}

/// Whether two identities describe the same session lifecycle, ignoring the format generation.
///
/// The three fields here are the ones that distinguish one lifecycle from another under one id: a
/// deleted-then-recreated session gets a new creation time, a moved session a new directory, and a fork
/// is seeded where its parent is not. Format is excluded because the generation is what the caller-side
/// predicates compare *separately*, in opposite directions.
#[must_use]
pub fn lifecycle_identity_matches(
    stored: &CheckpointIdentity,
    expected: &LifecycleIdentity,
) -> bool {
    stored.created_at == expected.created_at
        && stored.cwd == expected.cwd
        && stored.is_seeded.unwrap_or(false) == expected.is_seeded
}

// -------------------------------------------------------------------------------------------------
// Records
// -------------------------------------------------------------------------------------------------

/// One session's stored record: the log identity it was folded from plus its checkpoint rows.
///
/// The whole record is replaced on every write — the registry checkpoint is always the complete
/// per-session cut, so a partial update would have no meaning to express. `rows` is
/// [`crate::Checkpoint`], the same type [`crate::ProjectionCache::checkpoint`] produces and
/// [`crate::ProjectionCache::restore`] consumes: a second row type here would be a second definition of
/// what a checkpoint is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    /// The log identity the rows were folded from.
    pub identity: CheckpointIdentity,
    /// One row per projection key.
    ///
    /// A flat `key -> row` map rather than [`crate::Checkpoint`] itself: `Checkpoint` is a newtype over
    /// the map, so serializing it would nest every row set one level deeper than upstream's record and
    /// make a document written here unreadable by the composition this port is measured against.
    /// [`CheckpointRecord::checkpoint`] is the conversion, and it is total in both directions.
    pub rows: BTreeMap<String, CheckpointRow>,
}

impl CheckpointRecord {
    /// The record as the fold's own checkpoint type.
    #[must_use]
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            rows: self.rows.clone(),
        }
    }

    /// A record from the fold's checkpoint type.
    #[must_use]
    pub fn from_checkpoint(identity: CheckpointIdentity, checkpoint: Checkpoint) -> Self {
        Self {
            identity,
            rows: checkpoint.rows,
        }
    }
}

/// One stored document: the domain version that wrote it and the record it holds.
///
/// The version sits *outside* the record because it describes the document's layout, not the session's
/// log: [`DomainSpec::accepts_version`] decides whether the record may be read at all, before any
/// identity or row guard runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredDocument {
    /// The domain version that wrote this document.
    pub version: u64,
    /// The record itself.
    pub record: CheckpointRecord,
}

// -------------------------------------------------------------------------------------------------
// Storage
// -------------------------------------------------------------------------------------------------

/// Why a store refused a write.
///
/// Every variant means the record did not land, never "retry with the same input": the cache is
/// fail-soft on this path by contract, so a caller logs it and lets the next cold read rebuild.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the projection-cache store rejected the write: {detail}")]
pub struct StoreError {
    /// What the medium reported.
    pub detail: String,
}

/// The storage face the cache needs.
///
/// A trait rather than a concrete table because this crate has no storage-domain layer: upstream opens a
/// `per-record` table on a json backend, and the two operations it uses are `get` and `put`. Writes take a
/// [`Value`] rather than a [`CheckpointRecord`] because that is what the medium stores, and a malformed
/// document is a fact about the medium that [`MemoryStore`] has to be able to reproduce.
pub trait CheckpointStore: Send + Sync {
    /// The decoded record for one session, or `None` when it is absent, unsupported or unreadable.
    ///
    /// `None` is not an error: the caller serves the session uncached and a cold read rebuilds it. The
    /// `Result` inner layer is what lets a *read* refusal stay distinguishable from a write refusal
    /// without giving the read path an error channel nothing would act on.
    fn read(&self, id: &str) -> Option<CheckpointRecord>;

    /// How many documents the medium moved aside because it could not accept them.
    ///
    /// A caller that wants the reasons calls [`MemoryStore::quarantined`]; the count is here so the cache
    /// can surface "this session was served uncached because its record was refused" without knowing which
    /// store it holds.
    fn invalid_records(&self) -> usize;

    /// Replaces one session's stored document.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the medium cannot accept the write.
    fn write(&self, id: &str, document: Value) -> Result<(), StoreError>;
}

/// Why a stored document was moved aside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuarantineReason {
    /// The document's domain version is neither current nor compatible.
    UnsupportedVersion {
        /// The version the document declared.
        found: u64,
    },
    /// The document did not parse as a [`StoredDocument`], or declared no version at all.
    Malformed {
        /// What failed, for the log line the medium would write.
        detail: String,
    },
}

/// One document the medium moved aside.
///
/// The raw document is kept, mirroring the `<key>.json.bak.<stamp>` file upstream's json backend leaves:
/// a debugging session needs the bytes that failed, and the stamp is a monotonic counter here because a
/// cache has no clock of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct Quarantine {
    /// The session id the document was stored under.
    pub id: String,
    /// The name the document was moved aside as.
    pub backup: String,
    /// Why it was refused.
    pub reason: QuarantineReason,
    /// The document as stored.
    pub document: Value,
}

/// A [`CheckpointStore`] that keeps raw documents in memory.
///
/// Deliberately stores the JSON *document*, not the decoded record: a test that seeds a malformed
/// document, or one from a newer domain version, has to see the same backup-and-skip behaviour the json
/// backend would produce, and a store that decoded on the way in could not represent either.
pub struct MemoryStore {
    spec: DomainSpec,
    documents: Mutex<BTreeMap<String, Value>>,
    quarantined: Mutex<Vec<Quarantine>>,
    refuse_writes: std::sync::atomic::AtomicBool,
}

impl MemoryStore {
    /// Creates an empty store for one domain.
    #[must_use]
    pub fn new(spec: DomainSpec) -> Self {
        Self {
            spec,
            documents: Mutex::new(BTreeMap::new()),
            quarantined: Mutex::new(Vec::new()),
            refuse_writes: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Creates an empty store for the `session_projcache` domain.
    #[must_use]
    pub fn session_projcache() -> Self {
        Self::new(DomainSpec::SESSION_PROJCACHE)
    }

    /// The domain this store serves.
    #[must_use]
    pub fn spec(&self) -> DomainSpec {
        self.spec
    }

    /// Stores a raw document without decoding it, the way a fixture or a previous build would leave one.
    pub fn seed(&self, id: &str, document: Value) {
        self.lock_documents().insert(id.to_owned(), document);
    }

    /// The raw document stored under one id, when it is still readable.
    #[must_use]
    pub fn raw(&self, id: &str) -> Option<Value> {
        self.lock_documents().get(id).cloned()
    }

    /// Makes every later write fail, so a fail-soft path can be exercised.
    ///
    /// A store that can only succeed cannot show the one property the write path claims: that losing a
    /// write costs a longer tail replay and nothing else. The flag is settable at any time and is not
    /// cleared by a read.
    pub fn refuse_writes(&self) {
        self.refuse_writes
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Allows writes again.
    pub fn allow_writes(&self) {
        self.refuse_writes
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Every document the medium moved aside, in the order it did so.
    #[must_use]
    pub fn quarantined(&self) -> Vec<Quarantine> {
        self.lock_quarantined().clone()
    }

    /// How many sessions the store holds a document for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_documents().len()
    }

    /// Whether the store holds no document at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock_documents().is_empty()
    }

    /// Moves one document aside, keeping its bytes and naming the backup `<id>.json.bak.<n>`.
    ///
    /// The removal and the record happen in that order, and the two locks are never held at once: a
    /// reader that quarantines must not block a writer that is storing a fresh record for another id.
    fn quarantine(&self, id: &str, reason: QuarantineReason) {
        let Some(document) = self.lock_documents().remove(id) else {
            return;
        };
        let mut quarantined = self.lock_quarantined();
        let backup = format!("{id}.json.bak.{}", quarantined.len());
        quarantined.push(Quarantine {
            id: id.to_owned(),
            backup,
            reason,
            document,
        });
    }

    /// Decodes one stored document, quarantining it when the domain version or the schema refuses it.
    fn decode(&self, id: &str) -> Option<CheckpointRecord> {
        let document = self.lock_documents().get(id).cloned()?;
        let Some(version) = document.get("version").and_then(Value::as_u64) else {
            self.quarantine(
                id,
                QuarantineReason::Malformed {
                    detail: "the document declares no version".to_owned(),
                },
            );
            return None;
        };
        if !self.spec.accepts_version(version) {
            self.quarantine(id, QuarantineReason::UnsupportedVersion { found: version });
            return None;
        }
        match serde_json::from_value::<StoredDocument>(document) {
            Ok(stored) => Some(stored.record),
            Err(error) => {
                self.quarantine(
                    id,
                    QuarantineReason::Malformed {
                        detail: error.to_string(),
                    },
                );
                None
            }
        }
    }

    fn lock_documents(&self) -> MutexGuard<'_, BTreeMap<String, Value>> {
        self.documents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_quarantined(&self) -> MutexGuard<'_, Vec<Quarantine>> {
        self.quarantined
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CheckpointStore for MemoryStore {
    fn read(&self, id: &str) -> Option<CheckpointRecord> {
        self.decode(id)
    }

    fn invalid_records(&self) -> usize {
        self.lock_quarantined().len()
    }

    fn write(&self, id: &str, document: Value) -> Result<(), StoreError> {
        if self.refuse_writes.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError {
                detail: "the medium is unavailable".to_owned(),
            });
        }
        self.lock_documents().insert(id.to_owned(), document);
        Ok(())
    }
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryStore")
            .field("domain", &self.spec.name)
            .field("version", &self.spec.version)
            .field("records", &self.len())
            .field("quarantined", &self.lock_quarantined().len())
            .field("refuse_writes", &self.refuse_writes.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

// -------------------------------------------------------------------------------------------------
// Viewing a checkpoint's rows
// -------------------------------------------------------------------------------------------------

/// Which of a projection's client-visible keys one read wants.
///
/// Upstream accepts an explicit list or a `ReadonlySet` and uses a set as given, copying only a list.
/// Rust has no aliasing rule that makes that optimization meaningful, so both variants answer
/// [`KeySelection::selects`] and neither allocates.
#[derive(Clone, Copy, Debug)]
pub enum KeySelection<'a> {
    /// An explicit list of keys.
    Keys(&'a [&'a str]),
    /// A derived set, such as a registry's stable summary-key set.
    Set(&'a BTreeSet<String>),
}

impl KeySelection<'_> {
    /// Whether this selection wants one key.
    #[must_use]
    pub fn selects(&self, key: &str) -> bool {
        match self {
            Self::Keys(keys) => keys.contains(&key),
            Self::Set(set) => set.contains(key),
        }
    }
}

/// Turns checkpoint rows into the whole values a reader is served.
///
/// The registry's job upstream, not the cache's: only the owner of the units can decide which rows are
/// current for a key and what a stored state looks like as a client value. Modelling it as a trait keeps
/// this module from reimplementing [`crate::ProjectionCache::view_checkpoint`] while still letting the
/// watermark arithmetic — which keys made it into the block, and the lowest seq among them — live where a
/// test can drive it with a deliberately partial viewer.
pub trait CheckpointViewer {
    /// The whole values this checkpoint can serve without reading a log.
    ///
    /// A key whose row is absent or unusable is omitted; [`PersistedProjectionCache`] counts only the
    /// keys actually served when it computes the block's watermark.
    fn view_checkpoint(
        &self,
        checkpoint: &Checkpoint,
        keys: Option<KeySelection<'_>>,
    ) -> BTreeMap<String, Value>;
}

impl CheckpointViewer for ProjectionCache {
    fn view_checkpoint(
        &self,
        checkpoint: &Checkpoint,
        keys: Option<KeySelection<'_>>,
    ) -> BTreeMap<String, Value> {
        // The inherent method is the zero-I/O rung this trait wraps: it omits version-mismatched rows and
        // leaves the cache's own cells untouched, which is exactly the contract above.
        ProjectionCache::view_checkpoint(self, checkpoint)
            .values
            .into_iter()
            .filter(|(key, _)| keys.is_none_or(|selection| selection.selects(key)))
            .collect()
    }
}

// -------------------------------------------------------------------------------------------------
// Write-behind policy
// -------------------------------------------------------------------------------------------------

/// The two throttles between the mandatory write points.
///
/// Both are deployment choices with no universally correct value, which is why upstream states them in
/// the composition. The three mandatory points are policy rather than tunables and always fire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePolicy {
    /// Committed events per session that force a durable checkpoint write between mandatory points.
    pub write_every_events: u32,
    /// Longest time in milliseconds a dirty checkpoint may stay unwritten between mandatory points.
    pub write_interval_ms: u64,
}

/// Why a write policy was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WritePolicyError {
    /// A zero event threshold would make every event a write.
    #[error("session projection cache writeEveryEvents must be at least 1")]
    ZeroEvents,
    /// A zero interval would make the interval trigger meaningless.
    #[error("session projection cache writeIntervalMs must be at least 1")]
    ZeroInterval,
}

impl WritePolicy {
    /// Validates the two thresholds.
    ///
    /// # Errors
    ///
    /// Returns [`WritePolicyError`] for either zero: upstream's schema requires both to be natural
    /// numbers of at least one, and a zero would silently turn the throttle into "write every event".
    pub fn validated(
        write_every_events: u32,
        write_interval_ms: u64,
    ) -> Result<Self, WritePolicyError> {
        if write_every_events == 0 {
            return Err(WritePolicyError::ZeroEvents);
        }
        if write_interval_ms == 0 {
            return Err(WritePolicyError::ZeroInterval);
        }
        Ok(Self {
            write_every_events,
            write_interval_ms,
        })
    }
}

/// Why a durable write is happening now.
///
/// Carried so the log line on a failed write can name the trigger: "the interval fired and the write
/// failed" and "the session was detaching and the write failed" call for different follow-ups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushReason {
    /// Session creation, the first mandatory point.
    Create,
    /// A `turn/end` event, the mandatory in-turn point.
    TurnEnd,
    /// Session disposal, the live-to-cold moment.
    Detach,
    /// The committed-event threshold was reached.
    CountThreshold,
    /// The interval deadline passed with a dirty checkpoint.
    Interval,
}

impl FlushReason {
    /// The label upstream logs with a failed write.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::TurnEnd => "turn/end",
            Self::Detach => "detach",
            Self::CountThreshold => "count threshold",
            Self::Interval => "interval",
        }
    }
}

/// One session's write-behind bookkeeping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DirtyState {
    /// Committed events since the last durable write.
    pending: u32,
    /// When the interval trigger was armed, in the caller's clock.
    armed_at: Option<u64>,
}

/// Decides *when* a checkpoint is written, and nothing about how.
///
/// The three mandatory points (create, `turn/end`, detach) are policy; the two throttles
/// ([`WritePolicy`]) bound the in-turn stream. A `turn/end` deliberately does **not** count toward the
/// threshold: the durable value most reads want is the turn-final one, so that event flushes immediately
/// and the run of events behind it starts clean.
///
/// The interval trigger is a timer upstream. Here it is [`WriteBehind::due`], which reports the sessions
/// whose deadline has passed; the caller's scheduler decides how often to ask. Firing clears the armed
/// deadline and the counter, which is what upstream's `markClean` does inside `write` — *before* the
/// durability barrier, so a write that then fails still leaves the session clean and the failure costs
/// only a longer tail replay.
pub struct WriteBehind {
    policy: WritePolicy,
    dirty: Mutex<BTreeMap<String, DirtyState>>,
}

impl WriteBehind {
    /// Creates the policy state for one deployment.
    #[must_use]
    pub fn new(policy: WritePolicy) -> Self {
        Self {
            policy,
            dirty: Mutex::new(BTreeMap::new()),
        }
    }

    /// The thresholds in force.
    #[must_use]
    pub fn policy(&self) -> WritePolicy {
        self.policy
    }

    /// Folds one committed event into the decision.
    ///
    /// Returns the trigger when this event is itself a write point, and `None` when the event was only
    /// counted. A `turn/end` returns [`FlushReason::TurnEnd`] without touching the counter: it is a
    /// mandatory point, not one of the `writeEveryEvents` events.
    ///
    /// `now_ms` is the caller's monotonic clock. It is only consulted to arm the interval trigger at the
    /// first dirty event after a clean write — arming on every event would push the deadline out forever,
    /// and a checkpoint that never lands is the failure mode the throttle exists to bound.
    pub fn observe(&self, id: &str, event: &SessionEvent, now_ms: u64) -> Option<FlushReason> {
        if matches!(event, SessionEvent::TurnEnd { .. }) {
            self.mark_clean(id);
            return Some(FlushReason::TurnEnd);
        }
        let mut dirty = self.lock_dirty();
        let state = dirty.entry(id.to_owned()).or_default();
        state.pending = state.pending.saturating_add(1);
        if state.pending >= self.policy.write_every_events {
            // The trigger fires, so the bookkeeping resets here exactly as `write` would have reset it.
            state.pending = 0;
            state.armed_at = None;
            return Some(FlushReason::CountThreshold);
        }
        state.armed_at.get_or_insert(now_ms);
        None
    }

    /// Records the creation mandatory point.
    ///
    /// Creation is the first mandatory point, and it is not redundant: a session that never talks — a
    /// forked child seeded with its ancestor's title, say — would otherwise get its first row only at
    /// detach, so a crash would leave the seed-derived values unreadable on the cold list.
    pub fn created(&self, id: &str) -> FlushReason {
        self.mark_clean(id);
        FlushReason::Create
    }

    /// Records the disposal mandatory point and drops the session's bookkeeping.
    ///
    /// This is the live-to-cold moment: after the write the cold-read ladder serves the session from the
    /// cache, and the dirty entry has no meaning once the session is gone.
    pub fn disposed(&self, id: &str) -> FlushReason {
        self.lock_dirty().remove(id);
        FlushReason::Detach
    }

    /// The sessions whose interval deadline has passed, sorted by id.
    ///
    /// Firing clears the deadline so a second call before the write cannot fire again; the session stays
    /// otherwise untouched, because a failed write still leaves it clean by contract.
    pub fn due(&self, now_ms: u64) -> Vec<String> {
        let interval = self.policy.write_interval_ms;
        let mut fired = Vec::new();
        let mut dirty = self.lock_dirty();
        for (id, state) in dirty.iter_mut() {
            let Some(armed_at) = state.armed_at else {
                continue;
            };
            if now_ms.saturating_sub(armed_at) < interval {
                continue;
            }
            state.armed_at = None;
            state.pending = 0;
            fired.push(id.clone());
        }
        fired
    }

    /// Marks one session's checkpoint clean: its cut is being written.
    ///
    /// Idempotent, and called by [`PersistedProjectionCache::write`] before the durability barrier. A
    /// session with no bookkeeping is not created by this call — a clean session should cost nothing.
    pub fn mark_clean(&self, id: &str) {
        if let Some(state) = self.lock_dirty().get_mut(id) {
            state.pending = 0;
            state.armed_at = None;
        }
    }

    /// How many committed events are pending for one session.
    #[must_use]
    pub fn pending(&self, id: &str) -> u32 {
        self.lock_dirty().get(id).map_or(0, |state| state.pending)
    }

    /// Whether one session's interval trigger is armed.
    #[must_use]
    pub fn is_armed(&self, id: &str) -> bool {
        self.lock_dirty()
            .get(id)
            .is_some_and(|state| state.armed_at.is_some())
    }

    /// Drops every pending timer and every session's bookkeeping.
    ///
    /// The plugin-disposal path: sessions outlive the cache, so a late flush must not land after the
    /// domain closed. Clearing here is what makes that impossible rather than merely unlikely.
    pub fn clear(&self) {
        self.lock_dirty().clear();
    }

    fn lock_dirty(&self) -> MutexGuard<'_, BTreeMap<String, DirtyState>> {
        self.dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for WriteBehind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriteBehind")
            .field("write_every_events", &self.policy.write_every_events)
            .field("write_interval_ms", &self.policy.write_interval_ms)
            .field("sessions", &self.lock_dirty().len())
            .finish_non_exhaustive()
    }
}

// -------------------------------------------------------------------------------------------------
// The cache
// -------------------------------------------------------------------------------------------------

/// Why a durable checkpoint write did not land.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CacheWriteError {
    /// The session's identity could not be projected.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// A unit's state would not survive a JSON round trip.
    ///
    /// Refused instead of stored: a mangled value would be served as current on the next cold read, and
    /// nothing downstream could tell it from a correct one.
    #[error(
        "projection checkpoint is not losslessly JSON-serializable (a unit state violates the plain-JSON contract)"
    )]
    NotLossless,
    /// The medium refused the write.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Why a cold read could not produce a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ColdReadError {
    /// The caller's identity is not a valid one.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The fold refused the checkpoint or the log.
    #[error(transparent)]
    Fold(#[from] CacheError),
}

/// Why prepared-session hydration could not produce a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HydrateError {
    /// The caller's identity is not a valid one.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The fold refused the exact log the fallback offered.
    #[error(transparent)]
    Fold(#[from] CacheError),
}

/// One cold read's result: the snapshot, the rows it produced, and how the write-back went.
///
/// The write-back result is *reported* rather than discarded the way upstream's fire-and-forget promise
/// is, so a caller can log the trigger-labelled warning and a test can prove the snapshot still arrives.
/// The refresh itself is upstream's behaviour and is kept: a cold read that did not write back would pay
/// the full log fold on every open, forever.
#[derive(Clone, Debug, PartialEq)]
pub struct ColdRead {
    /// The projection cut at the log end.
    pub snapshot: ProjectionSnapshot,
    /// The refreshed rows, ready for a durable write-back and identical to what was stored.
    pub refreshed: Checkpoint,
    /// Whether the refreshed rows reached the medium.
    pub write_back: Result<(), CacheWriteError>,
}

/// The projection cache's policy layer: stored records in, blocks and fold seeds out.
///
/// Per deployment rather than per session (the store is keyed by session id), which is the opposite of
/// [`crate::ProjectionCache`]: that one holds one session's folded cells and is what this type hands a
/// checkpoint to.
pub struct PersistedProjectionCache<S> {
    store: S,
    spec: DomainSpec,
    behind: WriteBehind,
}

impl<S: CheckpointStore> PersistedProjectionCache<S> {
    /// Wraps a store with the `session_projcache` domain and the given write policy.
    #[must_use]
    pub fn new(store: S, policy: WritePolicy) -> Self {
        Self::with_spec(store, DomainSpec::SESSION_PROJCACHE, policy)
    }

    /// Wraps a store with an explicit domain spec.
    ///
    /// Exposed rather than inferred so a test or a migration can open a predecessor domain version and
    /// see the same backup-and-skip behaviour the medium would produce.
    #[must_use]
    pub fn with_spec(store: S, spec: DomainSpec, policy: WritePolicy) -> Self {
        Self {
            store,
            spec,
            behind: WriteBehind::new(policy),
        }
    }

    /// The store being written.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The domain spec in force.
    #[must_use]
    pub fn spec(&self) -> DomainSpec {
        self.spec
    }

    /// The write-behind policy state.
    #[must_use]
    pub fn write_behind(&self) -> &WriteBehind {
        &self.behind
    }

    /// The stored record for one session, accepted only when its bound log identity is the caller's.
    fn record_for(
        &self,
        id: &str,
        expected: &CurrentCheckpointIdentity,
    ) -> Option<CheckpointRecord> {
        let record = self.store.read(id)?;
        if identity_matches(&record.identity, expected) {
            Some(record)
        } else {
            None
        }
    }

    /// The zero-I/O listing read: whole values viewed straight from the stored rows of the record bound
    /// to the caller's lifecycle.
    ///
    /// The header is the only identity witness a listing holds, so this face matches the lifecycle and
    /// not the inherited cut. The view is as stale as the last durable checkpoint but never wrong and
    /// never from an unrelated log.
    ///
    /// The block's `as_of` is the **lowest** watermark among the served rows — the seq every served value
    /// has folded through at least. The number is the record's own position, which a header cannot relate
    /// to the log a caller later opens; the listing therefore labels the block as cached, and the client
    /// lets every value the connected session produces supersede it whatever this number says.
    #[must_use]
    pub fn cached_snapshot(
        &self,
        meta: &HeaderFacts,
        keys: Option<KeySelection<'_>>,
        viewer: &dyn CheckpointViewer,
    ) -> Option<ProjectionSnapshot> {
        let expected = lifecycle_identity_of(meta);
        let record = self.store.read(&meta.id)?;
        if !current_lifecycle_matches(&record.identity, &expected) {
            return None;
        }
        view_record(&record, keys, viewer)
    }

    /// Reads only a predecessor checkpoint's title as a zero-I/O listing hint.
    ///
    /// The authoritative header supplies the lifecycle identity. A cache checkpoint can lag that log but
    /// cannot lead it, because writes flush the log first, so a matching predecessor title is a genuine
    /// (possibly stale) fact from this session. The registry still requires the current title row's
    /// version and schema — [`CheckpointViewer`] is what enforces that. No other predecessor projection
    /// is exposed: format normalization can change their current meaning.
    #[must_use]
    pub fn cached_predecessor_title(
        &self,
        meta: &HeaderFacts,
        viewer: &dyn CheckpointViewer,
    ) -> Option<ProjectionSnapshot> {
        let expected = lifecycle_identity_of(meta);
        let record = self.store.read(&meta.id)?;
        if !predecessor_identity_matches(&record.identity, &expected) {
            return None;
        }
        view_record(
            &record,
            Some(KeySelection::Keys(&[PREDECESSOR_TITLE_KEY])),
            viewer,
        )
    }

    /// Cold-reads one session's projections from its complete log.
    ///
    /// Each unit is seeded from the identity-checked rows — the fold skips the already-folded prefix — and
    /// the refreshed checkpoint is written back so the first cold read creates the record and later ones
    /// seed from it. The caller supplies the complete log in seq order: this service never consults a
    /// persistence layer of its own.
    ///
    /// The write-back is fail-soft, so its failure is reported in [`ColdRead::write_back`] rather than
    /// returned: a failed refresh only costs a longer tail replay on the next cold read, and losing the
    /// snapshot over it would turn a cache miss into an unreadable session.
    ///
    /// # Errors
    ///
    /// Returns [`ColdReadError::Identity`] when an unseeded header claims a non-zero cut, and
    /// [`ColdReadError::Fold`] when the fold refuses the checkpoint or the log — a row past the log end,
    /// or a hole in the supplied window.
    pub fn cold_snapshot(
        &self,
        meta: &HeaderFacts,
        inherited_event_count: u64,
        events: &[(Seq, SessionEvent)],
        fold: &ProjectionCache,
    ) -> Result<ColdRead, ColdReadError> {
        let identity = identity_of(meta, inherited_event_count)?;
        let checkpoint = self
            .record_for(&meta.id, &identity)
            .map_or_else(Checkpoint::new, |record| record.checkpoint());
        let snapshot = fold.restore(&checkpoint, events, 0)?;
        let refreshed = fold.checkpoint();
        let write_back = self.put(&meta.id, identity.stored(), refreshed.clone());
        Ok(ColdRead {
            snapshot,
            refreshed,
            write_back,
        })
    }

    /// Hydrates projection cells for an already-prepared session without another persistence read.
    ///
    /// The cache seeds matching rows and the supplied exact log advances every unit to the observation
    /// cut. No checkpoint is written, because the logical observation may contain recovery events that
    /// are not durable yet — writing here would put the cache ahead of the log, which is the one thing the
    /// durability barrier exists to prevent.
    ///
    /// Upstream wraps this in a retry from `{}` for a *schema* failure on a stored state. That retry has no
    /// analogue here and is deliberately not faked: [`crate::Projection`] has no schema, so the only
    /// record-level failure upstream guards against — a `val` the unit's schema refuses — cannot arise.
    /// A record that is not the caller's is already excluded by the identity check below, which is
    /// upstream's other half of the same protection.
    ///
    /// # Errors
    ///
    /// Returns [`HydrateError::Identity`] for an unseeded header with a non-zero cut, and
    /// [`HydrateError::Fold`] when the exact log cannot be folded — a hole in the supplied window or a
    /// stored row claiming events past its end. Both are facts about the log, not about the cache.
    pub fn hydrate_prepared(
        &self,
        meta: &HeaderFacts,
        inherited_event_count: u64,
        events: &[(Seq, SessionEvent)],
        fold: &ProjectionCache,
    ) -> Result<ProjectionSnapshot, HydrateError> {
        let identity = identity_of(meta, inherited_event_count)?;
        let checkpoint = self
            .record_for(&meta.id, &identity)
            .map_or_else(Checkpoint::new, |record| record.checkpoint());
        Ok(fold.restore(&checkpoint, events, 0)?)
    }

    /// Durably checkpoints one live session now.
    ///
    /// The registry cut is taken inside this call, on purpose: the durability barrier must run **after**
    /// the cut and **before** the write. A cut taken by the caller before the barrier could name a state
    /// built from events the flushed log does not contain, and a crash between the two would leave a cache
    /// row ahead of the log — values folded from events no stored log holds, which no later read can
    /// detect. The cut is therefore a closure, not a value.
    ///
    /// The order is: refuse an invalid identity, take the cut, mark the session clean, run the barrier,
    /// then store. Marking clean before the barrier is upstream's choice and is what makes a failed write
    /// cost only a longer tail replay instead of a retry storm on every subsequent event.
    ///
    /// # Errors
    ///
    /// Returns [`CacheWriteError::Identity`] for an invalid identity, [`CacheWriteError::NotLossless`] for a
    /// state JSON cannot carry, and [`CacheWriteError::Store`] when the medium refuses the record. A caller
    /// on a fail-soft path logs the error and continues; the cache self-heals on the next write.
    pub fn write<C, F>(
        &self,
        meta: &HeaderFacts,
        inherited_event_count: u64,
        cut: C,
        flush_log: F,
    ) -> Result<(), CacheWriteError>
    where
        C: FnOnce() -> Checkpoint,
        F: FnOnce(),
    {
        let identity = identity_of(meta, inherited_event_count)?;
        let rows = cut();
        self.behind.mark_clean(&meta.id);
        flush_log();
        self.put(&meta.id, identity.stored(), rows)
    }

    /// Durably checkpoints one live session from a [`ProjectionCache`]'s current cut.
    ///
    /// The common call site: [`PersistedProjectionCache::write`] with `fold.checkpoint()` as the cut.
    /// # Errors
    ///
    /// As [`PersistedProjectionCache::write`].
    pub fn checkpoint_now<F>(
        &self,
        meta: &HeaderFacts,
        inherited_event_count: u64,
        fold: &ProjectionCache,
        flush_log: F,
    ) -> Result<(), CacheWriteError>
    where
        F: FnOnce(),
    {
        self.write(meta, inherited_event_count, || fold.checkpoint(), flush_log)
    }

    /// Replaces one session's stored record.
    ///
    /// The rows are checked for lossless JSON *before* the medium sees them: a state that cannot survive
    /// the round trip would be served as current on the next cold read, and nothing downstream could tell
    /// it from a correct one. The check is [`girder_core::is_lossless`] plus a rejection of numbers this
    /// crate's `serde_json` can hold but a JavaScript peer cannot read back (an arbitrary-precision literal
    /// such as `1e400`).
    fn put(
        &self,
        id: &str,
        identity: CheckpointIdentity,
        rows: Checkpoint,
    ) -> Result<(), CacheWriteError> {
        if !rows.rows.values().all(|row| snapshot_json(&row.val)) {
            return Err(CacheWriteError::NotLossless);
        }
        let document = StoredDocument {
            version: self.spec.version,
            record: CheckpointRecord::from_checkpoint(identity, rows),
        };
        let document = serde_json::to_value(document).map_err(|_| CacheWriteError::NotLossless)?;
        self.store.write(id, document)?;
        Ok(())
    }
}

impl<S> std::fmt::Debug for PersistedProjectionCache<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistedProjectionCache")
            .field("domain", &self.spec.name)
            .field("version", &self.spec.version)
            .field("behind", &self.behind)
            .finish_non_exhaustive()
    }
}

/// Views one record's rows as a block bound to the lowest served watermark.
///
/// The watermark is computed over the rows that *made it into the block*, not over every stored row: a
/// version-mismatched row is not served, so counting its seq would understate the block's position and
/// make a fresh record look older than it is.
fn view_record(
    record: &CheckpointRecord,
    keys: Option<KeySelection<'_>>,
    viewer: &dyn CheckpointViewer,
) -> Option<ProjectionSnapshot> {
    let checkpoint = record.checkpoint();
    let values = viewer.view_checkpoint(&checkpoint, keys);
    let mut as_of: Option<Seq> = None;
    for (key, row) in &record.rows {
        if !values.contains_key(key) {
            continue;
        }
        as_of = Some(as_of.map_or(row.seq, |current: Seq| current.min(row.seq)));
    }
    as_of.map(|as_of| ProjectionSnapshot { as_of, values })
}

/// Whether a value survives the JSON round trip this cache promises.
///
/// Two rules, and the second is why this is not just [`girder_core::is_lossless`]:
///
/// 1. [`girder_core::is_lossless`] rejects an integer above `2^53`, which a JavaScript peer would read
///    back as a different number. Stricter than upstream (every JavaScript number *is* an f64), and
///    deliberate: a checkpoint's whole purpose is to be read by another process, over there.
/// 2. A number this crate's `serde_json` can hold but no JavaScript peer can read — an
///    arbitrary-precision literal such as `1e400`, whose `as_f64` is `None` — is exactly upstream's
///    non-finite case (`JSON.stringify(Infinity)` is `null`), which a `Value` cannot otherwise express.
fn snapshot_json(value: &Value) -> bool {
    fn walk(value: &Value) -> bool {
        match value {
            Value::Null | Value::Bool(_) | Value::String(_) => true,
            Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
            Value::Array(items) => items.iter().all(walk),
            Value::Object(map) => map.values().all(walk),
        }
    }
    walk(value) && is_lossless(value)
}
