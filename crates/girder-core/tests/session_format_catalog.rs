//! Tests for the Session format catalog: the version → codec inventory and its adjacent-edge chain.
//!
//! Every refusal below is one `createSessionFormatCatalog` / `createSessionFormatChain` throws
//! upstream, and every success is a `sessionFormatCatalog` operation it performs. The point of
//! testing them here rather than trusting the transcription is that each one guards a *silent*
//! failure mode:
//!
//! - **a duplicated or missing codec is a misread log, not a crash.** With two readers for one
//!   version the last registration wins and a log decodes under rules that were not meant for it;
//!   with a hole in the version range a whole generation becomes unreadable with no diagnostic.
//!   Both are refused at assembly, which is the only moment the declaration is fully visible.
//! - **the plan is a slice only because the chain is dense.** Upstream can plan by slicing because
//!   construction proved adjacency and completeness; a gap would make `plan(v1)` return an edge
//!   that starts at v2. The tests check the slice boundaries directly, including the empty plan for
//!   a log that is already current.
//! - **`migration-required` is a promise.** Upstream reports it only when every edge to current
//!   exists. This build cannot keep that promise for v1 — `v1 → v2` has no implementation — so a v1
//!   header must report `unsupported` with a reason naming the edge, not a route that fails one
//!   step later.
//! - **unsupported and malformed are different answers.** A newer harness wrote the first; a
//!   corrupt file produced the second. Collapsing them turns "upgrade the harness" into "the file
//!   is broken", or the reverse.
//! - **`-0`, `4.0`, and `4` are not the same version.** JavaScript's `Number.isSafeInteger` refuses
//!   the first, accepts the other two, and refuses anything past 2^53-1. A version field that
//!   classifies differently from the writer's own encoder would round-trip to different bytes.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written even before `crates/girder-core/src/lib.rs` declares it. Paths into
//! the crate are spelled through `girder_core::`, the alias the library declares for itself, so the
//! same source builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::too_many_lines,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/session/format_catalog.rs"]
pub mod format_catalog_gap;

use format_catalog_gap::{
    CatalogError, CodecEntry, Edge, EdgeKind, FormatCatalog, HeaderRead, VersionClass,
};
use girder_core::FormatVersion;
use serde_json::{Value, json};

/// The released codec list, as a test-local value so the tests exercise `build` rather than
/// `released`.
fn codecs() -> Vec<CodecEntry> {
    vec![
        CodecEntry::new(0, "c0"),
        CodecEntry::new(1, "c1"),
        CodecEntry::new(2, "c2"),
        CodecEntry::new(3, "c3"),
        CodecEntry::new(4, "c4"),
    ]
}

/// The released edge list, as a test-local value.
fn edges() -> Vec<Edge> {
    vec![
        Edge::row(0, 1, "e01"),
        Edge::missing(1, 2, "e12"),
        Edge::artifact(2, 3, "e23"),
        Edge::row(3, 4, "e34"),
    ]
}

/// A well-formed current header, ready to be adjusted per test.
fn header(version: u64) -> Value {
    json!({
        "type": "session",
        "version": version,
        "id": "catalog-session",
        "createdAt": 1,
        "isSeeded": false,
        "delegationDepth": 0,
    })
}

// ---- assembly ---------------------------------------------------------------------------------

#[test]
fn the_released_inventory_is_five_codecs_and_four_edges() {
    let catalog = FormatCatalog::released().unwrap();
    assert_eq!(catalog.current(), 4);
    assert_eq!(catalog.current(), FormatVersion::CURRENT.as_number());
    assert_eq!(catalog.codecs().len(), 5);
    assert_eq!(catalog.edges().len(), 4);
}

#[test]
fn the_released_inventory_agrees_with_the_rest_of_the_build() {
    // `check_capabilities` is the only thing tying the catalog's literals to `FormatVersion` and
    // `Migration`; `released` runs it, and it must pass on the shipped declaration.
    let catalog = FormatCatalog::released().unwrap();
    catalog.check_capabilities().unwrap();
    for version in catalog.readable_versions() {
        assert!(
            FormatVersion::from_number(version).is_some(),
            "v{version} has no FormatVersion variant"
        );
    }
}

#[test]
fn a_duplicated_codec_is_refused() {
    let mut list = codecs();
    list.push(CodecEntry::new(3, "c3-again"));
    assert_eq!(
        FormatCatalog::build(4, &list, &edges()).unwrap_err(),
        CatalogError::DuplicatedCodec { version: 3 }
    );
}

#[test]
fn a_missing_codec_is_refused() {
    let list: Vec<CodecEntry> = codecs().into_iter().filter(|c| c.version != 2).collect();
    assert_eq!(
        FormatCatalog::build(4, &list, &edges()).unwrap_err(),
        CatalogError::MissingCodec { version: 2 }
    );
}

#[test]
fn a_codec_above_the_current_writer_is_refused() {
    let mut list = codecs();
    list.push(CodecEntry::new(5, "c5"));
    assert_eq!(
        FormatCatalog::build(4, &list, &edges()).unwrap_err(),
        CatalogError::CodecNewerThanCurrent {
            version: 5,
            current: 4
        }
    );
}

#[test]
fn a_duplicated_edge_is_refused() {
    let list = vec![
        Edge::row(0, 1, "e01"),
        Edge::row(0, 1, "e01-again"),
        Edge::missing(1, 2, "e12"),
        Edge::artifact(2, 3, "e23"),
        Edge::row(3, 4, "e34"),
    ];
    assert_eq!(
        FormatCatalog::build(4, &codecs(), &list).unwrap_err(),
        CatalogError::DuplicatedMigration { from: 0, to: 1 }
    );
}

#[test]
fn a_duplicated_edge_name_is_refused() {
    let list = vec![
        Edge::row(0, 1, "same"),
        Edge::missing(1, 2, "same"),
        Edge::artifact(2, 3, "e23"),
        Edge::row(3, 4, "e34"),
    ];
    assert_eq!(
        FormatCatalog::build(4, &codecs(), &list).unwrap_err(),
        CatalogError::DuplicatedMigrationName {
            name: "same".to_owned()
        }
    );
}

#[test]
fn a_non_adjacent_edge_is_refused() {
    let list = vec![Edge::row(0, 2, "skip")];
    assert_eq!(
        FormatCatalog::build(4, &codecs(), &list).unwrap_err(),
        CatalogError::NonAdjacentMigration {
            name: "skip".to_owned(),
            from: 0,
            to: 2,
            want: 1,
        }
    );
}

#[test]
fn an_unnamed_edge_is_refused() {
    let list = vec![Edge::row(0, 1, "")];
    assert_eq!(
        FormatCatalog::build(4, &codecs(), &list).unwrap_err(),
        CatalogError::EmptyMigrationName
    );
}

#[test]
fn a_gap_in_the_chain_is_an_unsupported_artifact_not_a_malformed_declaration() {
    let list: Vec<Edge> = edges().into_iter().filter(|edge| edge.from != 1).collect();
    let error = FormatCatalog::build(4, &codecs(), &list).unwrap_err();
    assert_eq!(error, CatalogError::MissingMigration { from: 1, to: 2 });
    assert!(
        error.is_unsupported(),
        "a hole in the released chain means the log is unreadable, not that the catalog is broken"
    );
}

#[test]
fn an_edge_above_the_writer_is_refused() {
    let mut list = edges();
    list.push(Edge::row(4, 5, "e45"));
    assert_eq!(
        FormatCatalog::build(4, &codecs(), &list).unwrap_err(),
        CatalogError::MigrationBeyondCurrent {
            from: 4,
            current: 4
        }
    );
}

// ---- version → reader -------------------------------------------------------------------------

#[test]
fn every_supported_version_resolves_to_exactly_one_reader() {
    let catalog = FormatCatalog::released().unwrap();
    for (expected, codec) in catalog.codecs().iter().enumerate() {
        let expected = u64::try_from(expected).unwrap();
        assert_eq!(codec.version, expected);
        assert_eq!(catalog.codec(expected), Some(codec));
    }
    assert_eq!(catalog.codec(5), None);
}

#[test]
fn the_readable_range_is_exactly_zero_through_current() {
    let catalog = FormatCatalog::released().unwrap();
    assert_eq!(catalog.readable_versions(), 0..=4);
    for version in 0..=4 {
        assert!(catalog.reads(version), "v{version} must be readable");
    }
    assert!(!catalog.reads(5));
}

#[test]
fn a_version_is_classified_without_touching_a_row() {
    let catalog = FormatCatalog::released().unwrap();
    assert_eq!(catalog.classify(4), VersionClass::Current);
    for version in 0..4 {
        assert_eq!(
            catalog.classify(version),
            VersionClass::MigrationRequired,
            "v{version} is older than the writer"
        );
    }
    assert_eq!(
        catalog.classify(9),
        VersionClass::NewerThanBuild {
            stored: 9,
            current: 4
        },
        "a newer log must be reported, never clamped to current"
    );
}

// ---- the plan ---------------------------------------------------------------------------------

#[test]
fn the_edge_leaving_a_version_is_looked_up_by_its_source() {
    let catalog = FormatCatalog::released().unwrap();
    assert_eq!(catalog.edge(0).map(|edge| edge.to), Some(1));
    assert_eq!(
        catalog.edge(2).map(|edge| edge.kind),
        Some(EdgeKind::Artifact)
    );
    assert_eq!(
        catalog.edge(3).map(|edge| edge.to),
        Some(4),
        "the edge leaving v3 is the v3 -> v4 package"
    );
    assert!(catalog.edge(4).is_none(), "the writer leaves no edge");
    assert!(catalog.edge(9).is_none());
}

#[test]
fn the_plan_is_the_dense_suffix_from_the_stored_version() {
    let catalog = FormatCatalog::released().unwrap();
    assert_eq!(catalog.plan(0).unwrap().len(), 4);
    assert_eq!(catalog.plan(1).unwrap().len(), 3);
    assert_eq!(catalog.plan(3).unwrap().len(), 1);
    assert!(catalog.plan(4).unwrap().is_empty());
    assert_eq!(catalog.plan(1).unwrap()[0].from, 1);
    assert_eq!(catalog.plan(2).unwrap()[1].from, 3);
}

#[test]
fn a_plan_above_the_writer_is_refused() {
    let catalog = FormatCatalog::released().unwrap();
    assert_eq!(
        catalog.plan(5).unwrap_err(),
        CatalogError::NewerThanBuild {
            from: 5,
            current: 4
        }
    );
    assert!(catalog.plan(5).unwrap_err().is_unsupported());
}

#[test]
fn a_route_reports_where_this_build_stops() {
    let catalog = FormatCatalog::released().unwrap();
    let route = catalog.route(1).unwrap();
    assert_eq!(route.source(), 1);
    assert_eq!(route.target(), 4);
    assert_eq!(route.edges().len(), 3);
    assert!(!route.is_complete());
    // `v1 -> v2` is the unimplemented edge, and it is the *first* edge from v1.
    assert_eq!(route.blocked_at(), Some(0));
    assert_eq!(
        route.blocked_edge().unwrap().name,
        "@deepseek-ai/dsh-session-format-v1-to-v2",
        "the blocked edge must be the unimplemented v1 -> v2 package"
    );
    assert_eq!(route.reachable_version(), 1);
}

#[test]
fn a_route_past_the_gap_is_complete_from_v2_on() {
    let catalog = FormatCatalog::released().unwrap();
    let route = catalog.route(2).unwrap();
    assert!(route.is_complete(), "v2 -> v3 -> v4 are both implemented");
    assert_eq!(route.blocked_at(), None);
    assert_eq!(route.reachable_version(), 4);
    assert_eq!(route.edges().len(), 2);
}

#[test]
fn a_route_from_current_needs_no_edges() {
    let catalog = FormatCatalog::released().unwrap();
    let route = catalog.route(4).unwrap();
    assert!(route.is_complete());
    assert!(route.edges().is_empty());
    assert_eq!(route.reachable_version(), 4);
}

#[test]
fn an_unimplemented_edge_is_not_traversable() {
    assert!(Edge::row(0, 1, "x").is_traversable());
    assert!(Edge::artifact(2, 3, "x").is_traversable());
    assert!(!Edge::missing(1, 2, "x").is_traversable());
    assert_eq!(Edge::missing(1, 2, "x").kind, EdgeKind::Missing);
}

// ---- header-only classification ---------------------------------------------------------------

#[test]
fn a_current_header_needs_no_migration() {
    let catalog = FormatCatalog::released().unwrap();
    let read = catalog.read_header(&header(4));
    assert_eq!(read, HeaderRead::Current { version: 4 });
    assert!(read.is_current());
    assert!(!read.is_refused());
    assert_eq!(read.reason(), None);
}

#[test]
fn a_restorable_older_header_reports_migration_required_with_its_route() {
    let catalog = FormatCatalog::released().unwrap();
    let read = catalog.read_header(&header(2));
    let HeaderRead::MigrationRequired {
        stored,
        target,
        route,
    } = read.clone()
    else {
        panic!("v2 must classify as migration-required, got {read:?}");
    };
    assert_eq!((stored, target), (2, 4));
    assert_eq!(route.edges().len(), 2);
    assert!(route.is_complete());
    assert!(read.is_migration_required());
    assert_eq!(read.stored_version(), Some(2));
}

#[test]
fn a_header_behind_the_unimplemented_edge_is_refused_with_the_edge_named() {
    let catalog = FormatCatalog::released().unwrap();
    let read = catalog.read_header(&header(1));
    assert!(read.is_refused());
    let reason = read.reason().unwrap();
    assert!(
        reason.contains("v1->v2"),
        "the reason must name the missing edge, got {reason:?}"
    );
    assert!(
        reason.contains("@deepseek-ai/dsh-session-format-v1-to-v2"),
        "the reason must name the edge's package, got {reason:?}"
    );
    assert!(
        !read.is_migration_required(),
        "claiming migration-required would start a loop that fails one edge later"
    );
    assert_eq!(
        read,
        HeaderRead::Unsupported {
            stored: Some(1),
            target: 4,
            reason: reason.to_owned(),
        }
    );
}

#[test]
fn a_header_from_a_newer_harness_is_unsupported_before_any_shape_check() {
    let catalog = FormatCatalog::released().unwrap();
    // Deliberately missing `isSeeded`: "too new" outranks "malformed", because the first tells the
    // user to upgrade while the second tells them the file is corrupt.
    let read = catalog.read_header(&json!({ "version": 9, "id": "x" }));
    assert!(matches!(
        read,
        HeaderRead::Unsupported {
            stored: Some(9),
            ..
        }
    ));
    assert!(read.reason().unwrap().contains("newer format v9"));
}

#[test]
fn a_header_that_is_not_an_object_is_malformed() {
    let catalog = FormatCatalog::released().unwrap();
    for value in [json!("session"), json!([]), json!(null), json!(4)] {
        let read = catalog.read_header(&value);
        assert!(matches!(read, HeaderRead::Malformed { stored: None, .. }));
        assert!(read.reason().unwrap().contains("must be a JSON object"));
    }
}

#[test]
fn a_header_without_a_readable_version_is_malformed() {
    let catalog = FormatCatalog::released().unwrap();
    let cases = [
        json!({ "id": "x", "createdAt": 1, "isSeeded": false, "delegationDepth": 0 }),
        json!({ "version": "4" }),
        json!({ "version": -1 }),
        json!({ "version": 4.5 }),
        json!({ "version": 9_007_199_254_740_992_u64 }),
    ];
    for case in cases {
        let read = catalog.read_header(&case);
        assert!(
            matches!(read, HeaderRead::Malformed { stored: None, .. }),
            "expected malformed for {case}, got {read:?}"
        );
    }
}

#[test]
fn negative_zero_is_not_a_version() {
    let catalog = FormatCatalog::released().unwrap();
    let value: Value = serde_json::from_str(r#"{"version":-0,"id":"x"}"#).unwrap();
    let read = catalog.read_header(&value);
    assert!(
        matches!(read, HeaderRead::Malformed { stored: None, .. }),
        "-0 serialises differently from 0 and is refused upstream, got {read:?}"
    );
}

#[test]
fn an_equivalent_spelling_of_an_integer_version_is_accepted() {
    let catalog = FormatCatalog::released().unwrap();
    // `4.0` and `4e0` are the same JavaScript number as `4`; classifying them differently from what
    // the encoder writes would make a byte-identical log read as a different generation.
    let value: Value = serde_json::from_str(
        r#"{"version":4.0,"id":"x","createdAt":1,"isSeeded":false,"delegationDepth":0}"#,
    )
    .unwrap();
    assert_eq!(
        catalog.read_header(&value),
        HeaderRead::Current { version: 4 }
    );
}

#[test]
fn a_header_missing_a_required_field_is_malformed_even_at_the_current_version() {
    let catalog = FormatCatalog::released().unwrap();
    let cases = [
        json!({ "version": 4, "createdAt": 1, "isSeeded": false, "delegationDepth": 0 }),
        json!({ "version": 4, "id": 7, "createdAt": 1, "isSeeded": false, "delegationDepth": 0 }),
        json!({ "version": 4, "id": "x", "isSeeded": false, "delegationDepth": 0 }),
        json!({ "version": 4, "id": "x", "createdAt": 1, "delegationDepth": 0 }),
        json!({ "version": 4, "id": "x", "createdAt": 1, "isSeeded": "no", "delegationDepth": 0 }),
        json!({ "version": 4, "id": "x", "createdAt": 1, "isSeeded": false }),
    ];
    for case in cases {
        let read = catalog.read_header(&case);
        assert!(
            matches!(
                read,
                HeaderRead::Malformed {
                    stored: Some(4),
                    ..
                }
            ),
            "expected malformed at v4 for {case}, got {read:?}"
        );
        assert!(read.stored_version() == Some(4));
    }
}

// ---- the historical variant -------------------------------------------------------------------

#[test]
fn the_historical_catalog_stops_below_current() {
    let historical = FormatCatalog::historical().unwrap();
    assert_eq!(historical.current(), 3);
    assert_eq!(historical.readable_versions(), 0..=3);
    assert!(historical.codec(3).is_some());
    assert_eq!(historical.codec(4), None);
    assert_eq!(historical.edges().len(), 3);
    assert!(!historical.reads(4));
}

#[test]
fn the_historical_catalog_reports_a_current_log_as_newer_than_build() {
    let historical = FormatCatalog::historical().unwrap();
    assert_eq!(
        historical.classify(4),
        VersionClass::NewerThanBuild {
            stored: 4,
            current: 3
        }
    );
    // This is exactly upstream's behaviour: the historical catalog exists so a prerequisite
    // collection cannot recursively open a current session.
    let read = historical.read_header(&header(4));
    assert!(matches!(
        read,
        HeaderRead::Unsupported {
            stored: Some(4),
            ..
        }
    ));
    assert_eq!(
        read.reason().unwrap(),
        "stored Session uses newer format v4; this build writes v3"
    );
}

#[test]
fn the_historical_catalog_reads_v2_as_migration_required_through_a_complete_route() {
    let historical = FormatCatalog::historical().unwrap();
    let read = historical.read_header(&header(2));
    let HeaderRead::MigrationRequired { route, .. } = read else {
        panic!("v2 must be migratable inside the historical catalog");
    };
    assert_eq!(route.target(), 3);
    assert!(route.is_complete());
}

// ---- capability drift -------------------------------------------------------------------------

#[test]
fn a_catalog_whose_edge_overstates_the_row_migrator_is_refused() {
    let list = vec![
        // The row migrator does perform v0 -> v1, so declaring it whole-artifact disagrees with it.
        Edge::artifact(0, 1, "e01"),
        Edge::missing(1, 2, "e12"),
        Edge::artifact(2, 3, "e23"),
        Edge::row(3, 4, "e34"),
    ];
    let catalog = FormatCatalog::build(4, &codecs(), &list).unwrap();
    assert_eq!(
        catalog.check_capabilities().unwrap_err(),
        CatalogError::CapabilityDrift {
            from: 0,
            to: 1,
            declared: EdgeKind::Artifact,
            reason: "the row migrator already performs the edge",
        }
    );
}

#[test]
fn a_catalog_that_understates_a_supported_edge_is_refused() {
    let list = vec![
        Edge::row(0, 1, "e01"),
        Edge::missing(1, 2, "e12"),
        Edge::artifact(2, 3, "e23"),
        // v3 -> v4 *is* supported by the row migrator.
        Edge::missing(3, 4, "e34"),
    ];
    let catalog = FormatCatalog::build(4, &codecs(), &list).unwrap();
    assert_eq!(
        catalog.check_capabilities().unwrap_err(),
        CatalogError::CapabilityDrift {
            from: 3,
            to: 4,
            declared: EdgeKind::Missing,
            reason: "the row migrator already performs the edge",
        }
    );
}

#[test]
fn a_catalog_that_disagrees_about_the_writer_is_refused() {
    let catalog = FormatCatalog::build(3, &codecs()[..4], &edges()[..3]).unwrap();
    assert_eq!(
        catalog.check_capabilities().unwrap_err(),
        CatalogError::CurrentVersionMismatch {
            catalog: 3,
            build: 4
        }
    );
}

#[test]
fn a_catalog_naming_a_generation_the_build_cannot_represent_is_refused() {
    let mut list = codecs();
    list.push(CodecEntry::new(5, "c5"));
    let mut chain = edges();
    chain.push(Edge::row(4, 5, "e45"));
    let catalog = FormatCatalog::build(5, &list, &chain).unwrap();
    assert_eq!(
        catalog.check_capabilities().unwrap_err(),
        CatalogError::UnrepresentableVersion { version: 5 }
    );
}

// ---- the error taxonomy -----------------------------------------------------------------------

#[test]
fn only_artifact_problems_are_classified_as_unsupported() {
    let artifact = [
        CatalogError::MissingMigration { from: 1, to: 2 },
        CatalogError::NewerThanBuild {
            from: 9,
            current: 4,
        },
        CatalogError::MigrationBeyondCurrent {
            from: 4,
            current: 4,
        },
    ];
    for error in artifact {
        assert!(error.is_unsupported(), "{error} describes the log");
    }
    let declaration = [
        CatalogError::DuplicatedCodec { version: 0 },
        CatalogError::MissingCodec { version: 1 },
        CatalogError::CodecNewerThanCurrent {
            version: 5,
            current: 4,
        },
        CatalogError::DuplicatedMigration { from: 0, to: 1 },
        CatalogError::DuplicatedMigrationName {
            name: "x".to_owned(),
        },
        CatalogError::NonAdjacentMigration {
            name: "x".to_owned(),
            from: 0,
            to: 2,
            want: 1,
        },
        CatalogError::EmptyMigrationName,
    ];
    for error in declaration {
        assert!(!error.is_unsupported(), "{error} describes the build");
    }
}

#[test]
fn every_refusal_carries_a_sentence_with_the_numbers_in_it() {
    let errors = [
        (
            CatalogError::DuplicatedCodec { version: 3 },
            "Session format codec v3 is duplicated",
        ),
        (
            CatalogError::MissingCodec { version: 2 },
            "Session format codec v2 is missing",
        ),
        (
            CatalogError::CodecNewerThanCurrent {
                version: 5,
                current: 4,
            },
            "Session format codec v5 is newer than current v4",
        ),
        (
            CatalogError::NonAdjacentMigration {
                name: "e".to_owned(),
                from: 0,
                to: 2,
                want: 1,
            },
            "e must declare adjacent v0->v1",
        ),
        (
            CatalogError::MissingMigration { from: 1, to: 2 },
            "Session migration v1->v2 is missing",
        ),
        (
            CatalogError::MigrationBeyondCurrent {
                from: 4,
                current: 4,
            },
            "Session migration from v4 does not lead to current v4",
        ),
        (
            CatalogError::NewerThanBuild {
                from: 9,
                current: 4,
            },
            "stored Session uses newer format v9; this build writes v4",
        ),
    ];
    for (error, expected) in errors {
        assert_eq!(error.to_string(), expected);
    }
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::format_catalog_gap as format_catalog;
}
