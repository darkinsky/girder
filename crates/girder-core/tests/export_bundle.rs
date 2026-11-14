//! Tests for the multi-session export manifest.
//!
//! A manifest exists to be trusted before anything expensive is read: a caller looks at it to know
//! what a package contains, and then decides whether to transfer, restore, or delete the source it
//! came from. So the properties under test are the ones a wrong answer would cost data:
//!
//! - **the total is the sum of the entries** — a manifest that misstates its own size is either
//!   damaged or built wrong, and either way the caller must hear about it
//! - **an empty package is refused** — the one manifest that is always a mistake, and the one a
//!   caller is most likely to mistake for "nothing to do"
//! - **the version is checked before the content** — a v2 manifest measured against v1 rules would
//!   be reported as corrupt, sending the caller after damage that does not exist
//! - **a manifest survives a JSON round trip unchanged** — it travels between instances as bytes,
//!   not as a Rust value
//! - **counts that cannot be summed do not wrap** — a wrapped total could make a damaged manifest
//!   pass its own consistency check
//!
//! # Why the module is compiled by path
//!
//! `export_bundle` is not yet declared in the crate root; that wiring is a separate change. The file
//! has no `crate::` imports, so including it by path needs no shim and exercises exactly the code a
//! consumer will reach through `girder_core::export_bundle`. Nothing is copied: this is the real
//! file, and the `#[path]` attribute should be replaced by the crate-root import once it lands.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/export_bundle.rs"]
mod export_bundle;

use export_bundle::{Bundle, BundleError, Entry, VERSION, manifest, validate};

/// One session as the listing shape `manifest` accepts.
fn entry(id: &str, title: Option<&str>, events: usize) -> (String, Option<String>, usize) {
    (id.to_owned(), title.map(ToOwned::to_owned), events)
}

// ---- construction -----------------------------------------------------------------------------

#[test]
fn a_manifest_carries_the_listing_it_was_built_from() {
    let bundle = manifest(
        vec![
            entry("s1", Some("first"), 3),
            entry("s2", None, 0),
            entry("s3", Some("third"), 7),
        ],
        1_700_000_000_000,
    );

    assert_eq!(bundle.version, VERSION);
    assert_eq!(bundle.exported_at, 1_700_000_000_000);
    assert_eq!(
        bundle.entries,
        vec![
            Entry {
                id: "s1".to_owned(),
                title: Some("first".to_owned()),
                events: 3,
            },
            Entry {
                id: "s2".to_owned(),
                title: None,
                events: 0,
            },
            Entry {
                id: "s3".to_owned(),
                title: Some("third".to_owned()),
                events: 7,
            },
        ]
    );
    // Order is preserved rather than sorted: the caller listed by its own rule.
    assert_eq!(
        bundle
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec!["s1", "s2", "s3"]
    );
}

#[test]
fn the_total_is_the_sum_of_the_entries() {
    let bundle = manifest(vec![entry("a", None, 2), entry("b", None, 5)], 0);

    assert_eq!(bundle.total_events, 7);
    assert_eq!(validate(&bundle), Ok(()));
}

#[test]
fn a_manifest_this_builder_produced_always_validates() {
    // The builder and the checker must agree on both the sum and the saturation rule; if they ever
    // diverged, every package this build wrote would be rejected by it.
    for counts in [
        vec![0usize],
        vec![1, 1, 1],
        vec![0, 0, 0],
        vec![usize::MAX],
        vec![usize::MAX, usize::MAX],
        vec![usize::MAX, 1],
    ] {
        let entries = counts
            .iter()
            .enumerate()
            .map(|(index, count)| entry(&format!("s{index}"), None, *count))
            .collect::<Vec<_>>();
        let bundle = manifest(entries, 42);

        assert_eq!(validate(&bundle), Ok(()), "counts {counts:?}");
    }
}

// ---- version rejection is the core case -------------------------------------------------------

#[test]
fn an_unknown_version_is_refused() {
    let bundle = Bundle {
        version: 99,
        exported_at: 5,
        entries: vec![Entry {
            id: "s1".to_owned(),
            title: None,
            events: 4,
        }],
        total_events: 4,
    };

    let error = validate(&bundle).unwrap_err();

    assert_eq!(
        error,
        BundleError::UnsupportedVersion {
            found: 99,
            supported: VERSION,
        }
    );
    // Both numbers are in the message, because "unsupported" alone does not tell a caller whether to
    // upgrade the package or the binary.
    let message = error.to_string();
    assert!(message.contains("99"), "{message}");
    assert!(message.contains(&VERSION.to_string()), "{message}");
}

#[test]
fn the_version_is_checked_before_the_entries_are() {
    // A future format is not a damaged current one. Each of these would fail a content check, and
    // each must still be reported as a version problem so the diagnosis is not misleading.
    let cases = [
        Bundle {
            version: VERSION + 1,
            exported_at: 0,
            entries: Vec::new(),
            total_events: 0,
        },
        Bundle {
            version: VERSION + 1,
            exported_at: 0,
            entries: vec![Entry {
                id: "s1".to_owned(),
                title: None,
                events: 1,
            }],
            total_events: 999,
        },
    ];

    for bundle in cases {
        assert!(
            matches!(
                validate(&bundle),
                Err(BundleError::UnsupportedVersion { .. })
            ),
            "expected a version refusal, got {:?}",
            validate(&bundle)
        );
    }
}

// ---- count mismatch is the other core case ----------------------------------------------------

#[test]
fn a_total_that_disagrees_with_the_entries_is_refused() {
    let bundle = Bundle {
        version: VERSION,
        exported_at: 1,
        entries: vec![
            Entry {
                id: "s1".to_owned(),
                title: None,
                events: 3,
            },
            Entry {
                id: "s2".to_owned(),
                title: None,
                events: 4,
            },
        ],
        total_events: 10,
    };

    let error = validate(&bundle).unwrap_err();

    assert_eq!(
        error,
        BundleError::CountMismatch {
            declared: 10,
            actual: 7,
        }
    );
    // Both numbers again: the caller can tell a lost entry from an inflated total by comparing them,
    // and a message carrying only one of them would leave that inference to guesswork.
    let message = error.to_string();
    assert!(message.contains("10"), "{message}");
    assert!(message.contains('7'), "{message}");
}

#[test]
fn a_total_below_the_sum_is_refused_too() {
    // The check is equality, not "at least": a manifest that understates its size is as wrong as one
    // that overstates it, and the likely cause — a dropped entry — is the one that loses data.
    let bundle = Bundle {
        version: VERSION,
        exported_at: 1,
        entries: vec![Entry {
            id: "s1".to_owned(),
            title: None,
            events: 5,
        }],
        total_events: 2,
    };

    assert_eq!(
        validate(&bundle),
        Err(BundleError::CountMismatch {
            declared: 2,
            actual: 5,
        })
    );
}

// ---- the empty package is refused -------------------------------------------------------------

#[test]
fn an_empty_package_is_refused() {
    let bundle = manifest(Vec::new(), 1_234_567);

    assert!(bundle.entries.is_empty());
    assert_eq!(validate(&bundle), Err(BundleError::Empty));
}

#[test]
fn emptiness_is_judged_by_the_entries_not_by_the_total() {
    // A zero total with entries is a package of empty sessions, which is legitimate; a nonzero total
    // with no entries is a contradiction, and "empty" is still the honest name for it.
    let empty_sessions = manifest(vec![entry("a", None, 0), entry("b", None, 0)], 0);
    assert_eq!(validate(&empty_sessions), Ok(()));

    let contradictory = Bundle {
        version: VERSION,
        exported_at: 0,
        entries: Vec::new(),
        total_events: 9,
    };
    assert_eq!(validate(&contradictory), Err(BundleError::Empty));
}

// ---- session counts ---------------------------------------------------------------------------

#[test]
fn a_single_session_package_validates() {
    let bundle = manifest(vec![entry("only", Some("the only one"), 12)], 7);

    assert_eq!(validate(&bundle), Ok(()));
    assert_eq!(bundle.entries.len(), 1);
    assert_eq!(bundle.total_events, 12);
}

#[test]
fn a_multi_session_package_validates() {
    // The case this module exists for: a project, or a machine being migrated.
    let entries = (0..25)
        .map(|index| entry(&format!("session-{index}"), None, index))
        .collect::<Vec<_>>();
    let bundle = manifest(entries, 7);

    assert_eq!(validate(&bundle), Ok(()));
    assert_eq!(bundle.entries.len(), 25);
    // 0 + 1 + ... + 24
    assert_eq!(bundle.total_events, 300);
}

// ---- serialization ----------------------------------------------------------------------------

#[test]
fn a_manifest_round_trips_through_json_unchanged() {
    // The format is a contract with other instances, so the value has to survive bytes, not just a
    // move within one process.
    let original = manifest(
        vec![
            entry("s1", Some("named"), 3),
            entry("s2", None, 0),
            entry("s3", Some(""), 9),
        ],
        1_700_000_000_000,
    );

    let text = serde_json::to_string(&original).unwrap();
    let restored: Bundle = serde_json::from_str(&text).unwrap();

    assert_eq!(restored, original);
    assert_eq!(validate(&restored), Ok(()));
}

#[test]
fn a_missing_title_reads_back_as_none() {
    // Another writer may simply omit the field; a manifest is a contract, so the absent and the null
    // title must both mean "unnamed" rather than failing to deserialize.
    let text =
        r#"{"version":1,"exported_at":3,"entries":[{"id":"s1","events":2}],"total_events":2}"#;
    let bundle: Bundle = serde_json::from_str(text).unwrap();

    assert_eq!(
        bundle.entries,
        vec![Entry {
            id: "s1".to_owned(),
            title: None,
            events: 2,
        }]
    );
    assert_eq!(validate(&bundle), Ok(()));
}

#[test]
fn the_serialized_manifest_names_its_version_and_carries_no_payload() {
    // Pinned by assertion rather than implied. A manifest that lost its version would be accepted by
    // this build and unreadable by every future one; a manifest that grew an events array would have
    // stopped being a manifest.
    let text = serde_json::to_string(&manifest(vec![entry("s1", None, 4)], 1)).unwrap();

    assert!(text.contains(r#""version":1"#), "{text}");
    assert!(text.contains(r#""total_events":4"#), "{text}");
    assert!(!text.contains("events\":["), "{text}");
    assert!(!text.contains(r#""id":"s1","title""#), "{text}");
}

// ---- saturation -------------------------------------------------------------------------------

#[test]
fn counts_too_large_to_sum_saturate_instead_of_wrapping() {
    // A wrapped total is the one way a consistency check can report success on nonsense: the builder
    // and the checker would both wrap to the same small number and agree. Saturation makes both stop
    // at the same ceiling, so the manifest stays internally consistent and is reported as the size it
    // actually is.
    let bundle = manifest(
        vec![
            entry("a", None, usize::MAX),
            entry("b", None, usize::MAX),
            entry("c", None, 1),
        ],
        0,
    );

    assert_eq!(bundle.total_events, usize::MAX);
    assert_eq!(validate(&bundle), Ok(()));
}

#[test]
fn a_saturated_total_still_rejects_a_wrong_declaration() {
    // The saturation rule must not become a hole in the check: a manifest claiming less than the
    // saturated sum is still a mismatch.
    let bundle = Bundle {
        version: VERSION,
        exported_at: 0,
        entries: vec![
            Entry {
                id: "a".to_owned(),
                title: None,
                events: usize::MAX,
            },
            Entry {
                id: "b".to_owned(),
                title: None,
                events: 1,
            },
        ],
        total_events: usize::MAX - 1,
    };

    assert_eq!(
        validate(&bundle),
        Err(BundleError::CountMismatch {
            declared: usize::MAX - 1,
            actual: usize::MAX,
        })
    );
}
