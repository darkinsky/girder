//! Tests for the persisted projection cache's identity and read ladder.
//!
//! Corresponds to upstream `packages/session/session-projection-cache/tests/cache.spec.ts` (identity
//! binding, the zero-I/O listing read, the predecessor title hint, cold-read seeding and write-back)
//! and to `session-projection/tests/registry.spec.ts`'s `viewCheckpoint` watermark expectations.
//!
//! # What each property protects
//!
//! - **A record is bound to a lifecycle, not an id.** A deleted-then-recreated id, a moved session, or a
//!   persistence root swapped under a surviving cache must not seed a fold from an unrelated log. The
//!   failure it prevents is silent and total: every projection of that session is wrong from the first
//!   read, and the next checkpoint writes the wrong state back.
//! - **The listing face ignores the inherited cut; the fold face requires it.** A header cannot witness
//!   its own cut, so requiring it would make a seeded session unlistable; a view never seeds a fold, so
//!   ignoring it there is safe. The fold face holds the session, so it holds the cut.
//! - **A predecessor is a predecessor only one way.** An absent format generation is *refused* by the
//!   current-generation check and *accepted* by the title hint. Collapsing the two would either let a
//!   pre-format record seed a fold or hide every predecessor title.
//! - **The block's watermark is the lowest *served* row.** A version-mismatched row is not in the block,
//!   so its seq must not drag the number down: a fresh record would look older than it is.
//! - **The durability barrier runs after the cut and before the write.** Flipping it would let a crash
//!   leave a cache row ahead of the log, folded from events no stored log holds — undetectable and beyond
//!   repair by any later refold.
//! - **A failed write costs a longer replay, nothing else.** The snapshot still arrives, the session is
//!   already clean, and the next event does not retry the failure.
//! - **A malformed or future document is moved aside.** Derived data must never cost a boot.
//! - **A non-lossless state is refused before it lands.** A value JSON cannot carry would be served as
//!   current on the next cold read, indistinguishable from a correct one.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not `girder-core`,
//! so every reference to the library goes through `girder_core::{...}` — including the ones the module
//! under test makes, which is why the module itself spells them that way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "tests assert by panicking, index their own fixtures, and read best as one scenario"
)]

#[path = "../src/session/projection_cache.rs"]
pub mod projection_extra_gap;

use std::sync::Arc;

use girder_core::session::session_final::HeaderFacts;
use girder_core::{
    Checkpoint, CheckpointRow, Projection, ProjectionCache, ProjectionSeq as Seq, SessionEvent,
    TurnEndReason, Versioned, VersionedProjection,
};
use projection_extra_gap::{
    CacheWriteError, CheckpointIdentity, CheckpointRecord, CheckpointStore, CheckpointViewer,
    ColdReadError, DomainSpec, FlushReason, HydrateError, IdentityError, InvalidRecords,
    KeySelection, Layout, MemoryStore, PREDECESSOR_TITLE_KEY, PersistedProjectionCache,
    QuarantineReason, StoreError, WriteBehind, WritePolicy, WritePolicyError,
    current_lifecycle_matches, identity_matches, identity_of, lifecycle_identity_of,
    predecessor_identity_matches,
};
use serde_json::{Value, json};

// ---- fixtures ----------------------------------------------------------------------------------

/// The title key, matching [`girder_core::TitleProjection`].
const TITLE: &str = "title";

/// The `state_version` every fixture projection declares.
const STATE_VERSION: u64 = 1;

/// A projection over how many events it has seen, so a fold's progress is observable.
struct Counter;

impl Projection for Counter {
    fn key(&self) -> &'static str {
        "counter"
    }

    fn init(&self) -> Value {
        json!({ "events": 0 })
    }

    fn apply(&self, mut state: Value, _event: &SessionEvent, _seq: Seq) -> Value {
        if let Some(object) = state.as_object_mut() {
            let seen = object.get("events").and_then(Value::as_u64).unwrap_or(0);
            object.insert("events".to_owned(), json!(seen + 1));
        }
        state
    }
}

/// A header for a plain, unseeded session.
fn header(id: &str) -> HeaderFacts {
    HeaderFacts::new(4, id, 1_700_000_000_000)
}

/// A fold over the title and the counter, at state version 1.
fn fold() -> ProjectionCache {
    ProjectionCache::new(vec![
        Arc::new(Versioned::new(Arc::new(Counter), 1)) as Arc<dyn VersionedProjection>,
        Arc::new(Versioned::new(Arc::new(girder_core::TitleProjection), 1))
            as Arc<dyn VersionedProjection>,
    ])
    .expect("built")
}

/// A four-event log: a turn boundary and a title.
fn log() -> Vec<(Seq, SessionEvent)> {
    vec![
        (1, SessionEvent::TurnStart { turn: 1 }),
        (
            2,
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
        ),
        (
            3,
            SessionEvent::SessionTitle {
                title: "a title".to_owned(),
                message_seqs: vec![1],
                source: "model".to_owned(),
            },
        ),
        (4, SessionEvent::TurnStart { turn: 2 }),
    ]
}

/// A policy with the thresholds upstream's test compositions use.
fn policy() -> WritePolicy {
    WritePolicy::validated(100, 60_000).expect("valid policy")
}

/// A cache over a fresh in-memory store.
fn cache(policy: WritePolicy) -> PersistedProjectionCache<MemoryStore> {
    PersistedProjectionCache::new(MemoryStore::session_projcache(), policy)
}

/// A cache over an *existing* store, so a seeded document is what the cache reads.
fn cache_over(
    store: &MemoryStore,
    policy: WritePolicy,
) -> PersistedProjectionCache<SeededStore<'_>> {
    PersistedProjectionCache::new(SeededStore { inner: store }, policy)
}

/// A [`CheckpointStore`] that borrows a [`MemoryStore`], so a test can keep a handle on the seed.
///
/// A store passed by value would be moved into the cache, and the fixtures that seed a document before
/// the cache exists would then inspect an empty one — a mistake that reads as a cache defect.
struct SeededStore<'a> {
    inner: &'a MemoryStore,
}

impl CheckpointStore for SeededStore<'_> {
    fn read(&self, id: &str) -> Option<CheckpointRecord> {
        self.inner.read(id)
    }

    fn invalid_records(&self) -> usize {
        self.inner.invalid_records()
    }

    fn write(&self, id: &str, document: Value) -> Result<(), StoreError> {
        self.inner.write(id, document)
    }
}

/// A record document at the current domain version, as the medium would hold it.
///
/// Built through the record type rather than by hand so the wire spelling is proved, not assumed: the
/// rows are a flat `key -> row` map and the identity keys are camelCase, both of which a hand-rolled
/// fixture would have to repeat from memory.
fn document(identity: CheckpointIdentity, rows: Checkpoint) -> Value {
    let record = CheckpointRecord::from_checkpoint(identity, rows);
    json!({ "version": 7, "record": record })
}

/// One checkpoint row.
fn row(ver: u64, seq: Seq, val: Value) -> CheckpointRow {
    CheckpointRow { ver, seq, val }
}

/// A checkpoint with the given rows.
fn checkpoint(entries: &[(&str, CheckpointRow)]) -> Checkpoint {
    Checkpoint {
        rows: entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    }
}

/// The created-at every fixture header carries, so an identity and a header can be made to match.
const CREATED_AT: u64 = 1_700_000_000_000;

/// The stored identity of an ordinary unseeded session: this build's generation, [`CREATED_AT`], no cwd.
fn plain_identity() -> CheckpointIdentity {
    CheckpointIdentity {
        format_version: Some(4),
        created_at: CREATED_AT,
        cwd: None,
        is_seeded: Some(false),
        inherited_event_count: Some(0),
    }
}

/// A viewer with every unit registered at [`STATE_VERSION`]: it serves each row whose version matches.
///
/// The version guard is the *viewer's* job, not this module's: only the owner of the units knows their
/// current `state_version`, so a row written by another build is dropped here rather than inside the
/// watermark arithmetic. Modelling that faithfully is what lets the watermark tests mean anything.
struct Everything;

impl CheckpointViewer for Everything {
    fn view_checkpoint(
        &self,
        checkpoint: &Checkpoint,
        keys: Option<KeySelection<'_>>,
    ) -> std::collections::BTreeMap<String, Value> {
        checkpoint
            .rows
            .iter()
            .filter(|(_, value)| value.ver == STATE_VERSION)
            .filter(|(key, _)| keys.is_none_or(|selection| selection.selects(key)))
            .map(|(key, value)| (key.clone(), value.val.clone()))
            .collect()
    }
}

/// A viewer with no projection registered: it serves nothing, whatever the rows say.
struct Nothing;

impl CheckpointViewer for Nothing {
    fn view_checkpoint(
        &self,
        _checkpoint: &Checkpoint,
        _keys: Option<KeySelection<'_>>,
    ) -> std::collections::BTreeMap<String, Value> {
        std::collections::BTreeMap::new()
    }
}

// -------------------------------------------------------------------------------------------------
// Identity: the fields a header witnesses
// -------------------------------------------------------------------------------------------------

#[test]
fn a_header_projects_onto_the_lifecycle_fields_and_carries_no_cut() {
    let mut facts = header("alpha");
    facts.cwd = Some("/work".to_owned());
    facts.is_seeded = true;

    let identity = lifecycle_identity_of(&facts);
    assert_eq!(identity.format_version, 4);
    assert_eq!(identity.created_at, 1_700_000_000_000);
    assert_eq!(identity.cwd.as_deref(), Some("/work"));
    assert!(identity.is_seeded);
}

#[test]
fn an_unseeded_session_with_a_non_zero_cut_is_refused() {
    // Refused rather than normalized to 0: the two name different logs, and a fold from the wrong init
    // mis-places every projection in it.
    let facts = header("alpha");
    assert_eq!(
        identity_of(&facts, 3),
        Err(IdentityError::UnseededInheritedCut {
            inherited_event_count: 3
        })
    );
    assert!(identity_of(&facts, 0).is_ok());
}

#[test]
fn a_seeded_session_may_carry_a_cut() {
    let mut facts = header("child");
    facts.is_seeded = true;
    let identity = identity_of(&facts, 2).expect("a seeded cut is valid");
    assert_eq!(identity.inherited_event_count, 2);
    assert!(identity.lifecycle.is_seeded);
}

#[test]
fn a_current_write_stores_every_identity_field() {
    let mut facts = header("alpha");
    facts.is_seeded = true;
    let stored = identity_of(&facts, 2).expect("valid").stored();
    // Every field present, so a later reader never has to decide whether an absent value means "unseeded"
    // or "not recorded".
    assert_eq!(stored.format_version, Some(4));
    assert_eq!(stored.created_at, 1_700_000_000_000);
    assert_eq!(stored.cwd, None);
    assert_eq!(stored.is_seeded, Some(true));
    assert_eq!(stored.inherited_event_count, Some(2));
}

// -------------------------------------------------------------------------------------------------
// Identity: the three faces disagree on purpose
// -------------------------------------------------------------------------------------------------

#[test]
fn the_listing_face_ignores_the_cut_while_the_fold_face_requires_it() {
    let mut facts = header("forked");
    facts.is_seeded = true;
    let expected = identity_of(&facts, 2).expect("valid");
    // The stored record was folded at cut 3: a different lifecycle for a fold, the same one for a view.
    let stored = CheckpointIdentity {
        inherited_event_count: Some(3),
        ..expected.stored()
    };

    assert!(
        current_lifecycle_matches(&stored, &expected.lifecycle),
        "a header cannot witness its own cut, so a listing must still serve the block"
    );
    assert!(
        !identity_matches(&stored, &expected),
        "continuing another cut's fold would carry it into the next checkpoint"
    );
}

#[test]
fn a_missing_cut_reads_as_unseeded_exactly() {
    let stored = CheckpointIdentity {
        format_version: Some(4),
        created_at: CREATED_AT,
        cwd: None,
        is_seeded: None,
        inherited_event_count: None,
    };
    let unseeded = identity_of(&header("plain"), 0).expect("valid");
    let mut seeded_facts = header("plain");
    seeded_facts.is_seeded = true;
    let seeded = identity_of(&seeded_facts, 0).expect("valid");

    assert!(
        identity_matches(&stored, &unseeded),
        "a lineage-less record admitted through a compatible version is exactly an unseeded caller's"
    );
    assert!(
        !identity_matches(&stored, &seeded),
        "and never a seeded caller's, whatever the cut"
    );
}

#[test]
fn an_absent_format_generation_never_seeds_a_contemporary_fold() {
    let stored = CheckpointIdentity {
        format_version: None,
        created_at: CREATED_AT,
        cwd: None,
        is_seeded: Some(false),
        inherited_event_count: Some(0),
    };
    let expected = identity_of(&header("pre-format"), 0).expect("valid");

    assert!(
        !current_lifecycle_matches(&stored, &expected.lifecycle),
        "no current log can prove which event semantics produced those rows"
    );
    assert!(!identity_matches(&stored, &expected));
    assert!(
        predecessor_identity_matches(&stored, &expected.lifecycle),
        "the one face that accepts it is the listing-only title hint"
    );
}

#[test]
fn a_predecessor_is_strictly_older_and_still_the_same_lifecycle() {
    let expected = lifecycle_identity_of(&header("alpha"));
    let older = CheckpointIdentity {
        format_version: Some(2),
        created_at: expected.created_at,
        cwd: None,
        is_seeded: Some(false),
        inherited_event_count: Some(0),
    };
    let same = CheckpointIdentity {
        format_version: Some(4),
        ..older.clone()
    };
    let mut other_lifecycle = older.clone();
    other_lifecycle.created_at = 1;

    assert!(predecessor_identity_matches(&older, &expected));
    assert!(
        !predecessor_identity_matches(&same, &expected),
        "a record at the caller's own generation is not a predecessor"
    );
    assert!(
        !predecessor_identity_matches(&other_lifecycle, &expected),
        "being older does not make a record about a different session"
    );
}

#[test]
fn cwd_and_seeded_both_bind_the_lifecycle() {
    let mut facts = header("alpha");
    facts.cwd = Some("/work".to_owned());
    let expected = lifecycle_identity_of(&facts);

    let homed = CheckpointIdentity {
        format_version: Some(4),
        created_at: expected.created_at,
        cwd: Some("/work".to_owned()),
        is_seeded: Some(false),
        inherited_event_count: Some(0),
    };
    let moved = CheckpointIdentity {
        cwd: Some("/elsewhere".to_owned()),
        ..homed.clone()
    };
    let unrecorded_cwd = CheckpointIdentity {
        cwd: None,
        ..homed.clone()
    };
    let seeded = CheckpointIdentity {
        is_seeded: Some(true),
        ..homed.clone()
    };

    assert!(current_lifecycle_matches(&homed, &expected));
    assert!(!current_lifecycle_matches(&moved, &expected));
    assert!(
        !current_lifecycle_matches(&unrecorded_cwd, &expected),
        "a moved session is a different lifecycle, and so is one whose directory was never recorded"
    );
    assert!(!current_lifecycle_matches(&seeded, &expected));
}

// -------------------------------------------------------------------------------------------------
// The domain spec
// -------------------------------------------------------------------------------------------------

#[test]
fn the_spec_accepts_the_current_and_compatible_versions_only() {
    let spec = DomainSpec::SESSION_PROJCACHE;
    assert_eq!(spec.name, "session_projcache");
    assert_eq!(spec.version, 7);
    assert_eq!(spec.compatible_versions, &[3, 4, 5, 6]);
    assert_eq!(spec.layout, Layout::PerRecord);
    assert_eq!(spec.invalid_records, InvalidRecords::BackupAndSkip);

    for accepted in [3, 4, 5, 6, 7] {
        assert!(spec.accepts_version(accepted), "{accepted} should be read");
    }
    for refused in [0, 1, 2, 8, 99] {
        // A *newer* version is refused rather than clamped: a future build's states may fold differently,
        // and treating them as current is how a downgrade silently corrupts a cache.
        assert!(
            !spec.accepts_version(refused),
            "{refused} should be refused"
        );
    }
}

// -------------------------------------------------------------------------------------------------
// The listing read: watermarks
// -------------------------------------------------------------------------------------------------

#[test]
fn the_block_watermark_is_the_lowest_served_row() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "mixed",
        document(
            plain_identity(),
            checkpoint(&[
                (TITLE, row(1, 4, json!("primary"))),
                ("counter", row(1, 2, json!({ "events": 2 }))),
            ]),
        ),
    );
    let cache = cache_over(&store, policy());

    // The two rows were folded to different points; one watermark has to describe both, and only the
    // lower one is safe to claim.
    let block = cache
        .cached_snapshot(&header("mixed"), None, &Everything)
        .expect("a matching lifecycle is served");
    assert_eq!(block.as_of, 2);
    assert_eq!(block.values.len(), 2);
}

#[test]
fn an_unserved_row_does_not_drag_the_watermark_down() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "stale-row",
        document(
            plain_identity(),
            checkpoint(&[
                (TITLE, row(1, 9, json!("fresh"))),
                ("counter", row(9, 1, json!({ "events": 1 }))),
            ]),
        ),
    );
    let cache = cache_over(&store, policy());

    // The version-mismatched row is not in the block, so its seq must not make a fresh record look old.
    let block = cache
        .cached_snapshot(&header("stale-row"), None, &Everything)
        .expect("the surviving row is served");
    assert_eq!(block.as_of, 9);
    assert_eq!(block.values.get(TITLE), Some(&json!("fresh")));
    assert!(!block.values.contains_key("counter"));
}

#[test]
fn a_block_with_no_served_key_is_not_served_at_all() {
    let store = MemoryStore::session_projcache();
    store.seed("empty", document(plain_identity(), checkpoint(&[])));
    let cache = cache_over(&store, policy());
    assert!(
        cache
            .cached_snapshot(&header("empty"), None, &Everything)
            .is_none(),
        "an empty cut has no watermark to report"
    );

    store.seed(
        "unregistered",
        document(
            plain_identity(),
            checkpoint(&[(TITLE, row(1, 3, json!("x")))]),
        ),
    );
    assert!(
        cache
            .cached_snapshot(&header("unregistered"), None, &Nothing)
            .is_none(),
        "a key no unit answers for is omitted, and omitting every key serves no block"
    );
}

#[test]
fn the_listing_read_selects_keys_without_folding_anything() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "selective",
        document(
            plain_identity(),
            checkpoint(&[
                (TITLE, row(1, 3, json!("t"))),
                ("counter", row(1, 5, json!({ "events": 5 }))),
            ]),
        ),
    );
    let cache = cache_over(&store, policy());

    let only_title = cache
        .cached_snapshot(
            &header("selective"),
            Some(KeySelection::Keys(&[TITLE])),
            &Everything,
        )
        .expect("served");
    assert_eq!(only_title.values.len(), 1);
    assert_eq!(only_title.as_of, 3);

    let mut set = std::collections::BTreeSet::new();
    set.insert("counter".to_owned());
    let only_counter = cache
        .cached_snapshot(
            &header("selective"),
            Some(KeySelection::Set(&set)),
            &Everything,
        )
        .expect("served");
    assert_eq!(only_counter.as_of, 5);
    assert!(only_counter.values.contains_key("counter"));
}

#[test]
fn a_listing_never_serves_another_lifecycle() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "recreated",
        document(
            CheckpointIdentity {
                created_at: 1,
                ..plain_identity()
            },
            checkpoint(&[(TITLE, row(1, 2, json!("old life")))]),
        ),
    );
    let cache = cache_over(&store, policy());
    // The id matches, the lifecycle does not: an id names a slot, not a lifecycle.
    assert!(
        cache
            .cached_snapshot(&header("recreated"), None, &Everything)
            .is_none()
    );
    assert!(
        cache
            .cached_snapshot(&header("never-cached"), None, &Everything)
            .is_none()
    );
}

// -------------------------------------------------------------------------------------------------
// The predecessor title hint
// -------------------------------------------------------------------------------------------------

#[test]
fn a_predecessor_serves_only_its_title() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "pre",
        document(
            CheckpointIdentity {
                format_version: Some(2),
                ..plain_identity()
            },
            checkpoint(&[
                (PREDECESSOR_TITLE_KEY, row(1, 3, json!("forked title"))),
                ("counter", row(1, 3, json!({ "events": 3 }))),
            ]),
        ),
    );
    let cache = cache_over(&store, policy());

    let hint = cache
        .cached_predecessor_title(&header("pre"), &Everything)
        .expect("a predecessor title is a genuine, possibly stale, fact");
    assert_eq!(hint.as_of, 3);
    assert_eq!(hint.values.len(), 1);
    // No other predecessor projection is exposed: format normalization can change their meaning.
    assert_eq!(
        hint.values.get(PREDECESSOR_TITLE_KEY),
        Some(&json!("forked title"))
    );

    // The block face keeps rejecting it, and the same record at the caller's generation is not a
    // predecessor at all.
    assert!(
        cache
            .cached_snapshot(&header("pre"), None, &Everything)
            .is_none()
    );
    store.seed(
        "current",
        document(
            plain_identity(),
            checkpoint(&[(PREDECESSOR_TITLE_KEY, row(1, 3, json!("now")))]),
        ),
    );
    assert!(
        cache
            .cached_predecessor_title(&header("current"), &Everything)
            .is_none()
    );
}

#[test]
fn a_predecessor_of_another_lifecycle_serves_nothing() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "moved",
        document(
            CheckpointIdentity {
                format_version: Some(1),
                cwd: Some("/old".to_owned()),
                ..plain_identity()
            },
            checkpoint(&[(PREDECESSOR_TITLE_KEY, row(1, 3, json!("t")))]),
        ),
    );
    let cache = cache_over(&store, policy());
    assert!(
        cache
            .cached_predecessor_title(&header("moved"), &Everything)
            .is_none()
    );
}

// -------------------------------------------------------------------------------------------------
// Cold reads
// -------------------------------------------------------------------------------------------------

#[test]
fn a_cold_read_seeds_from_a_matching_record_and_writes_the_refresh_back() {
    let store = MemoryStore::session_projcache();
    // A record covering the first two events, folded at cut 0 by this same lifecycle.
    store.seed(
        "cold",
        document(
            plain_identity(),
            checkpoint(&[("counter", row(1, 2, json!({ "events": 2 })))]),
        ),
    );
    let cache = cache_over(&store, policy());
    let fold = fold();

    let cold = cache
        .cold_snapshot(&header("cold"), 0, &log(), &fold)
        .expect("the log folds");
    assert_eq!(cold.snapshot.as_of, 4);
    // Seeded at 2, then folded forward over events 3 and 4.
    assert_eq!(cold.snapshot.get("counter"), Some(&json!({ "events": 4 })));
    assert_eq!(cold.write_back, Ok(()));
    // The refresh is what the next cold read will seed from, so it must have landed as-is.
    let stored = store.read("cold").expect("written back");
    assert_eq!(stored.checkpoint(), cold.refreshed);
    // Rewritten as a *current* record: all five identity fields present, whatever the seed hold had.
    assert_eq!(stored.identity, plain_identity());
}

#[test]
fn a_cold_read_of_an_unrelated_record_refolds_the_whole_log() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "reused",
        document(
            CheckpointIdentity {
                created_at: 999,
                ..plain_identity()
            },
            checkpoint(&[("counter", row(1, 2, json!({ "events": 99 })))]),
        ),
    );
    let cache = cache_over(&store, policy());
    let cold = cache
        .cold_snapshot(&header("reused"), 0, &log(), &fold())
        .expect("the log folds");
    // The foreign state is nowhere in the result: the fold started from init.
    assert_eq!(cold.snapshot.get("counter"), Some(&json!({ "events": 4 })));
}

#[test]
fn a_cold_read_refuses_an_unseeded_header_with_a_cut() {
    let cache = cache(policy());
    assert_eq!(
        cache.cold_snapshot(&header("bad"), 1, &[], &fold()),
        Err(ColdReadError::Identity(
            IdentityError::UnseededInheritedCut {
                inherited_event_count: 1
            }
        ))
    );
}

#[test]
fn a_cold_read_reports_a_failed_write_back_and_still_serves_the_snapshot() {
    let store = MemoryStore::session_projcache();
    store.refuse_writes();
    let cache = cache_over(&store, policy());

    let cold = cache
        .cold_snapshot(&header("losing"), 0, &log(), &fold())
        .expect("a cache miss is not an unreadable session");
    assert_eq!(cold.snapshot.as_of, 4);
    assert_eq!(
        cold.write_back,
        Err(CacheWriteError::Store(StoreError {
            detail: "the medium is unavailable".to_owned()
        })),
        "the failure is reported, not swallowed: the next read pays a longer replay and nothing else"
    );
}

// -------------------------------------------------------------------------------------------------
// Hydration
// -------------------------------------------------------------------------------------------------

#[test]
fn hydration_seeds_a_matching_record_and_writes_nothing() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "prepared",
        document(
            plain_identity(),
            checkpoint(&[("counter", row(1, 2, json!({ "events": 2 })))]),
        ),
    );
    let cache = cache_over(&store, policy());
    let before = store.raw("prepared");

    let snapshot = cache
        .hydrate_prepared(&header("prepared"), 0, &log(), &fold())
        .expect("hydrated");
    assert_eq!(snapshot.as_of, 4);
    assert_eq!(snapshot.get("counter"), Some(&json!({ "events": 4 })));
    // The logical observation may contain recovery events that are not durable yet, so hydration must
    // never put the cache ahead of the log.
    assert_eq!(store.raw("prepared"), before);
}

#[test]
fn hydration_ignores_a_record_folded_at_another_cut() {
    let store = MemoryStore::session_projcache();
    let mut facts = header("forked");
    facts.is_seeded = true;
    let foreign_cut = CheckpointIdentity {
        format_version: Some(4),
        created_at: facts.created_at,
        cwd: None,
        is_seeded: Some(true),
        // Folded at cut 3; this caller holds cut 2, so the states below encode a prefix it does not have.
        inherited_event_count: Some(3),
    };
    store.seed(
        "forked",
        document(
            foreign_cut,
            checkpoint(&[("counter", row(1, 4, json!({ "events": 40 })))]),
        ),
    );
    let cache = cache_over(&store, policy());
    let snapshot = cache
        .hydrate_prepared(&facts, 2, &log(), &fold())
        .expect("hydrated");
    // The foreign cut's state is gone; the log was folded from init.
    assert_eq!(snapshot.get("counter"), Some(&json!({ "events": 4 })));
}

#[test]
fn hydration_reports_a_broken_log_rather_than_hiding_it() {
    let cache = cache(policy());
    // A window with a hole: the fold refuses it, and the refusal is the log's, not the cache's.
    let holed = vec![
        (1, SessionEvent::TurnStart { turn: 1 }),
        (3, SessionEvent::TurnStart { turn: 2 }),
    ];
    assert!(matches!(
        cache.hydrate_prepared(&header("holed"), 0, &holed, &fold()),
        Err(HydrateError::Fold(_))
    ));
}

// -------------------------------------------------------------------------------------------------
// Durable writes
// -------------------------------------------------------------------------------------------------

#[test]
fn a_write_takes_the_cut_then_runs_the_barrier_then_stores() {
    let store = MemoryStore::session_projcache();
    let cache = cache_over(&store, policy());
    // One shared recorder, because both closures must append to the same ordering.
    let trace = std::cell::RefCell::new(Vec::new());

    cache
        .write(
            &header("barrier"),
            0,
            || {
                trace.borrow_mut().push("cut");
                checkpoint(&[(TITLE, row(1, 1, json!("t")))])
            },
            || trace.borrow_mut().push("flush"),
        )
        .expect("written");
    // Flipping the first two would let a crash leave a cache row ahead of the log: values folded from
    // events no stored log holds, which no later read can detect and no refold can repair.
    assert_eq!(trace.into_inner(), ["cut", "flush"]);
    assert!(store.read("barrier").is_some());
}

#[test]
fn a_write_stores_the_current_identity_and_stamps_the_domain_version() {
    let store = MemoryStore::session_projcache();
    let cache = cache_over(&store, policy());
    let mut facts = header("stamped");
    facts.is_seeded = true;
    facts.cwd = Some("/work".to_owned());

    cache
        .checkpoint_now(&facts, 0, &fold(), || {})
        .expect("written");

    let raw = store.raw("stamped").expect("stored");
    assert_eq!(raw["version"], json!(7));
    assert_eq!(raw["record"]["identity"]["formatVersion"], json!(4));
    assert_eq!(raw["record"]["identity"]["isSeeded"], json!(true));
    assert_eq!(raw["record"]["identity"]["cwd"], json!("/work"));
    assert_eq!(raw["record"]["identity"]["inheritedEventCount"], json!(0));
}

#[test]
fn an_older_domain_spec_stamps_its_own_version() {
    let store = MemoryStore::new(DomainSpec {
        version: 5,
        ..DomainSpec::SESSION_PROJCACHE
    });
    let cache = PersistedProjectionCache::with_spec(
        store,
        DomainSpec {
            version: 5,
            ..DomainSpec::SESSION_PROJCACHE
        },
        policy(),
    );
    cache
        .checkpoint_now(&header("old"), 0, &fold(), || {})
        .expect("written");
    assert_eq!(
        cache.store().raw("old").expect("stored")["version"],
        json!(5)
    );
}

#[test]
fn a_write_refuses_an_unseeded_cut() {
    let cache = cache(policy());
    assert_eq!(
        cache.checkpoint_now(&header("bad"), 2, &fold(), || {}),
        Err(CacheWriteError::Identity(
            IdentityError::UnseededInheritedCut {
                inherited_event_count: 2
            }
        ))
    );
    assert_eq!(cache.store().len(), 0, "nothing was stored");
}

#[test]
fn a_write_refuses_a_state_json_cannot_carry() {
    let cache = cache(policy());
    // An arbitrary-precision number: this crate's serde_json holds it, but no JavaScript peer can read it
    // back — upstream's non-finite case, which a `Value` cannot otherwise express.
    let poisoned: Value = serde_json::from_str(r#"{"events": 1e400}"#).expect("parsed");
    let result = cache.write(
        &header("poisoned"),
        0,
        || checkpoint(&[("counter", row(1, 1, poisoned))]),
        || {},
    );
    assert_eq!(result, Err(CacheWriteError::NotLossless));
    assert_eq!(
        cache.store().len(),
        0,
        "a mangled value would be served as current on the next cold read"
    );
}

#[test]
fn a_write_refuses_an_integer_a_javascript_peer_would_corrupt() {
    let cache = cache(policy());
    // 2^53 + 1 round-trips through Rust and becomes 2^53 in any JavaScript client.
    let unsafe_integer = json!(9_007_199_254_740_993_u64);
    assert_eq!(
        cache.write(
            &header("unsafe"),
            0,
            || checkpoint(&[("counter", row(1, 1, unsafe_integer))]),
            || {},
        ),
        Err(CacheWriteError::NotLossless)
    );
}

#[test]
fn a_failed_medium_write_is_reported_and_not_stored() {
    let store = MemoryStore::session_projcache();
    store.refuse_writes();
    let cache = cache_over(&store, policy());
    assert!(matches!(
        cache.checkpoint_now(&header("unlucky"), 0, &fold(), || {}),
        Err(CacheWriteError::Store(_))
    ));
}

// -------------------------------------------------------------------------------------------------
// The medium: backup and skip
// -------------------------------------------------------------------------------------------------

#[test]
fn a_malformed_document_is_moved_aside_and_read_as_absent() {
    let store = MemoryStore::session_projcache();
    store.seed("malformed", json!("not a record at all"));
    let cache = cache_over(&store, policy());

    assert!(
        cache
            .cached_snapshot(&header("malformed"), None, &Everything)
            .is_none()
    );
    let quarantined = store.quarantined();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0].backup, "malformed.json.bak.0");
    assert_eq!(quarantined[0].document, json!("not a record at all"));
    assert!(matches!(
        quarantined[0].reason,
        QuarantineReason::Malformed { .. }
    ));
    // Moved aside, not deleted: derived data must never cost a boot, and a debugging session needs the
    // bytes that failed.
    assert!(store.raw("malformed").is_none());
    assert_eq!(store.invalid_records(), 1);
}

#[test]
fn a_document_without_a_version_is_malformed() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "no-version",
        json!({ "record": { "identity": {}, "rows": {} } }),
    );
    let cache = cache_over(&store, policy());
    assert!(
        cache
            .cached_snapshot(&header("no-version"), None, &Everything)
            .is_none()
    );
    assert!(matches!(
        store.quarantined()[0].reason,
        QuarantineReason::Malformed { .. }
    ));
}

#[test]
fn a_newer_domain_version_is_refused_by_version() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "future",
        json!({ "version": 8, "record": { "identity": {}, "rows": {} } }),
    );
    let cache = cache_over(&store, policy());
    assert!(
        cache
            .cached_snapshot(&header("future"), None, &Everything)
            .is_none()
    );
    assert_eq!(
        store.quarantined()[0].reason,
        QuarantineReason::UnsupportedVersion { found: 8 }
    );
}

#[test]
fn a_compatible_predecessor_version_is_read() {
    let store = MemoryStore::session_projcache();
    store.seed(
        "compatible",
        json!({
            "version": 5,
            "record": {
                "identity": {
                    "formatVersion": 4,
                    "createdAt": 1_700_000_000_000_u64,
                    "isSeeded": false,
                    "inheritedEventCount": 0
                },
                "rows": { "title": { "ver": 1, "seq": 3, "val": "old build" } }
            }
        }),
    );
    let cache = cache_over(&store, policy());
    let block = cache
        .cached_snapshot(&header("compatible"), None, &Everything)
        .expect("a structurally valid predecessor record stays readable");
    assert_eq!(block.values.get(TITLE), Some(&json!("old build")));
    assert_eq!(store.invalid_records(), 0);
}

// -------------------------------------------------------------------------------------------------
// Write-behind
// -------------------------------------------------------------------------------------------------

#[test]
fn a_zero_threshold_is_refused() {
    assert_eq!(
        WritePolicy::validated(0, 1),
        Err(WritePolicyError::ZeroEvents)
    );
    assert_eq!(
        WritePolicy::validated(1, 0),
        Err(WritePolicyError::ZeroInterval)
    );
}

#[test]
fn the_count_threshold_fires_once_per_window() {
    let behind = WriteBehind::new(WritePolicy::validated(3, 60_000).expect("valid"));
    let event = SessionEvent::TurnStart { turn: 1 };

    assert_eq!(behind.observe("s", &event, 0), None);
    assert_eq!(behind.pending("s"), 1);
    assert_eq!(behind.observe("s", &event, 0), None);
    assert_eq!(
        behind.observe("s", &event, 0),
        Some(FlushReason::CountThreshold)
    );
    // The firing cleared the bookkeeping, so the next window starts from zero rather than firing again.
    assert_eq!(behind.pending("s"), 0);
    assert!(!behind.is_armed("s"));
    assert_eq!(behind.observe("s", &event, 0), None);
}

#[test]
fn a_turn_end_is_a_mandatory_point_that_does_not_count() {
    let behind = WriteBehind::new(WritePolicy::validated(3, 60_000).expect("valid"));
    behind.observe("s", &SessionEvent::TurnStart { turn: 1 }, 0);
    assert_eq!(behind.pending("s"), 1);

    let end = SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    };
    // The durable value most reads want is the turn-final one, so it flushes immediately and the run
    // behind it starts clean.
    assert_eq!(behind.observe("s", &end, 5), Some(FlushReason::TurnEnd));
    assert_eq!(behind.pending("s"), 0);
}

#[test]
fn the_interval_arms_at_the_first_dirty_event_and_fires_once() {
    let behind = WriteBehind::new(WritePolicy::validated(100, 1_000).expect("valid"));
    let event = SessionEvent::TurnStart { turn: 1 };
    behind.observe("s", &event, 10_000);
    // Arming on every event would push the deadline out forever, which is exactly what a throttle must
    // not do.
    behind.observe("s", &event, 10_500);
    assert!(behind.is_armed("s"));

    assert!(behind.due(10_999).is_empty());
    assert_eq!(behind.due(11_000), vec!["s".to_owned()]);
    // Firing clears the deadline, so a second ask before the write cannot fire again.
    assert!(behind.due(11_000).is_empty());
    assert!(!behind.is_armed("s"));
    assert_eq!(behind.pending("s"), 0);
}

#[test]
fn the_mandatory_points_mark_the_session_clean() {
    let behind = WriteBehind::new(WritePolicy::validated(100, 1_000).expect("valid"));
    let event = SessionEvent::TurnStart { turn: 1 };
    behind.observe("s", &event, 0);
    assert_eq!(behind.created("s"), FlushReason::Create);
    assert_eq!(behind.pending("s"), 0);

    behind.observe("s", &event, 0);
    assert_eq!(behind.disposed("s"), FlushReason::Detach);
    assert_eq!(
        behind.pending("s"),
        0,
        "a disposed session has no bookkeeping left to write"
    );
    assert!(!behind.is_armed("s"));
    assert_eq!(FlushReason::Detach.as_str(), "detach");
}

#[test]
fn clearing_drops_every_pending_timer() {
    let behind = WriteBehind::new(WritePolicy::validated(100, 1_000).expect("valid"));
    let event = SessionEvent::TurnStart { turn: 1 };
    behind.observe("a", &event, 0);
    behind.observe("b", &event, 0);
    assert!(behind.is_armed("a") && behind.is_armed("b"));

    // The plugin-disposal path: sessions outlive the cache, so a late flush must not land after the
    // domain closed.
    behind.clear();
    assert!(!behind.is_armed("a") && !behind.is_armed("b"));
    assert!(behind.due(9_999_999).is_empty());
}

#[test]
fn a_write_marks_the_session_clean_before_the_barrier() {
    let store = MemoryStore::session_projcache();
    let cache = cache_over(&store, policy());
    let event = SessionEvent::TurnStart { turn: 1 };
    cache.write_behind().observe("behind", &event, 0);
    assert_eq!(cache.write_behind().pending("behind"), 1);

    let mut barrier_ran = false;
    let fold = fold();
    cache
        .write(
            &header("behind"),
            0,
            || fold.checkpoint(),
            || {
                // A failed write still leaves the session clean, so the failure costs a longer tail replay
                // instead of a retry storm on every subsequent event.
                assert_eq!(cache.write_behind().pending("behind"), 0);
                barrier_ran = true;
            },
        )
        .expect("written");
    assert!(barrier_ran);
    assert!(store.read("behind").is_some());
}

#[test]
fn every_flush_reason_names_itself() {
    assert_eq!(FlushReason::Create.as_str(), "create");
    assert_eq!(FlushReason::TurnEnd.as_str(), "turn/end");
    assert_eq!(FlushReason::Detach.as_str(), "detach");
    assert_eq!(FlushReason::CountThreshold.as_str(), "count threshold");
    assert_eq!(FlushReason::Interval.as_str(), "interval");
}

#[test]
fn a_store_that_recovers_serves_the_next_write() {
    let store = MemoryStore::session_projcache();
    store.refuse_writes();
    let cache = cache_over(&store, policy());
    assert!(
        cache
            .checkpoint_now(&header("heal"), 0, &fold(), || {})
            .is_err()
    );
    store.allow_writes();
    // The cache self-heals on the next write: a lost write never poisons the bookkeeping.
    cache
        .checkpoint_now(&header("heal"), 0, &fold(), || {})
        .expect("written");
    assert!(cache.store().read("heal").is_some());
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::projection_extra_gap as projection_cache;
}
