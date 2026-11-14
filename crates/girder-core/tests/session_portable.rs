//! Tests for the portable session bundle.
//!
//! A bundle exists to move one session between instances, so the properties that matter are that a
//! round trip loses nothing, and that every way of receiving a broken bundle fails loudly and
//! specifically. Both are tested here rather than left to the importer's good intentions.
//!
//! # Why the module is compiled by path
//!
//! `session_portable` is not exported from the crate root yet; the wiring into `lib.rs` is a
//! separate change. Compiling the exact source file keeps these tests runnable and independent of
//! that wiring, while still exercising the same code a consumer will reach through
//! `girder_core::session_portable`. Nothing is duplicated: this is the real file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

// The module is included by path, so its `crate::session` import would resolve against this test
// crate's root rather than `girder-core`'s. This shim supplies the same type there. It exists only
// because `girder-core`'s crate root does not yet declare the module — wiring
// `pub mod session_portable;` belongs to the integrator — and should be deleted together with the
// `#[path]` attribute once that lands.
mod session {
    pub use girder_core::SessionEvent;
}

#[path = "../src/session_portable.rs"]
mod session_portable;

use girder_core::{SessionEvent, TurnEndReason};
use session_portable::{Bundle, BundleError, VERSION, export, import};

/// A session with more than one event type, so a round trip that dropped a variant is visible.
fn sample() -> Vec<SessionEvent> {
    vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::user_text("m1", "hello"),
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::assistant_text("m2", "hi there", "p"),
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ]
}

// ---- round trip -------------------------------------------------------------------------------

#[test]
fn export_then_import_restores_every_event_in_order() {
    let events = sample();
    let text = export(&events, Some("a session"), 1_700_000_000_000).unwrap();

    let bundle = import(&text).unwrap();

    assert_eq!(bundle.version, VERSION);
    assert_eq!(bundle.exported_at, 1_700_000_000_000);
    assert_eq!(bundle.title.as_deref(), Some("a session"));
    // The typed events, not just the JSON shape: this is the comparison that catches a field the
    // serializer dropped, which a JSON-to-JSON comparison against the input would not.
    assert_eq!(bundle.events().unwrap(), events);
}

#[test]
fn the_round_trip_preserves_optional_fields_and_payloads() {
    // A tool call carries free-form arguments and a retry carries an optional reason; both are the
    // fields most likely to be mangled by a naive transcription.
    let events = vec![
        SessionEvent::ToolCall {
            id: girder_core::ToolCallId::new("call-1").unwrap(),
            name: "read".to_owned(),
            arguments: r#"{"path":"a.rs"}"#.to_owned(),
        },
        SessionEvent::LlmRetry {
            attempt: 2,
            reason: Some("overloaded".to_owned()),
        },
        SessionEvent::SandboxMode {
            mode: "workspace-write".to_owned(),
            source: None,
        },
    ];

    let text = export(&events, None, 1).unwrap();
    let restored = import(&text).unwrap().events().unwrap();

    assert_eq!(restored, events);
}

#[test]
fn the_exported_bundle_names_its_version() {
    // Pinned by assertion rather than implied: a bundle that lost its version would be accepted by
    // this build and unreadable by every future one, and nothing else here would notice.
    let text = export(&sample(), None, 0).unwrap();
    assert!(text.contains(r#""version":1"#), "{text}");
}

// ---- version rejection is the core case -------------------------------------------------------

#[test]
fn an_unknown_version_is_refused() {
    let text = r#"{"version":99,"exported_at":5,"events":[]}"#;

    let error = import(text).unwrap_err();

    assert_eq!(
        error,
        BundleError::UnsupportedVersion {
            found: 99,
            supported: VERSION,
        }
    );
    // The message names both numbers, because "unsupported" alone does not tell a caller whether to
    // upgrade the bundle or the binary.
    let message = error.to_string();
    assert!(message.contains("99"), "{message}");
    assert!(message.contains(&VERSION.to_string()), "{message}");
}

#[test]
fn a_future_version_is_refused_before_its_events_are_judged() {
    // The events here are nonsense for the current vocabulary. The version must be reported anyway:
    // a newer bundle is expected to hold variants this build lacks, and complaining about one of
    // them would send the reader looking for a corrupt event instead of an old binary.
    let text = r#"{"version":2,"exported_at":5,"events":[{"type":"turn/start","turn":1},{"type":"future/thing"}]}"#;

    let error = import(text).unwrap_err();

    assert_eq!(
        error,
        BundleError::UnsupportedVersion {
            found: 2,
            supported: VERSION,
        }
    );
}

#[test]
fn a_past_version_is_also_refused() {
    // Not a special case for "newer": any version this build does not write is unknown to it, and
    // silently reading version 0 as version 1 is how a migration bug becomes silent data loss.
    let error = import(r#"{"version":0,"exported_at":0,"events":[]}"#).unwrap_err();
    assert_eq!(
        error,
        BundleError::UnsupportedVersion {
            found: 0,
            supported: VERSION,
        }
    );
}

#[test]
fn a_bundle_without_a_version_is_rejected_rather_than_defaulted() {
    let error = import(r#"{"exported_at":0,"events":[]}"#).unwrap_err();
    assert!(matches!(error, BundleError::InvalidJson { .. }), "{error}");
}

// ---- invalid JSON -----------------------------------------------------------------------------

#[test]
fn malformed_json_is_refused() {
    let error = import("{\"version\":1,").unwrap_err();
    assert!(matches!(error, BundleError::InvalidJson { .. }), "{error}");
}

#[test]
fn a_json_value_that_is_not_a_bundle_is_refused() {
    for text in ["null", "[]", "\"a string\"", "42"] {
        let error = import(text).unwrap_err();
        assert!(
            matches!(error, BundleError::InvalidJson { .. }),
            "{text} produced {error}"
        );
    }
}

#[test]
fn a_bundle_missing_its_events_is_refused() {
    // `events` has no serde default on purpose: a bundle with no event list is not an empty
    // session, it is an incomplete file, and restoring it as empty would lose the session quietly.
    let error = import(r#"{"version":1,"exported_at":0}"#).unwrap_err();
    assert!(matches!(error, BundleError::InvalidJson { .. }), "{error}");
}

#[test]
fn the_parser_detail_is_carried_through() {
    // The detail is what makes the error usable; a variant that dropped it would leave the caller
    // with "invalid JSON" and no line or column.
    let error = import("not json at all").unwrap_err();
    match error {
        BundleError::InvalidJson { detail } => assert!(!detail.is_empty(), "empty detail"),
        other => panic!("expected InvalidJson, got {other}"),
    }
}

// ---- per-event validation is the other core case ----------------------------------------------

#[test]
fn an_event_missing_a_required_field_is_reported_with_its_index() {
    // Valid JSON, invalid event: the shape-check-only importer accepts this bundle and breaks on the
    // first replay. The index is the whole point of the variant, so it is asserted exactly.
    let text = r#"{"version":1,"exported_at":0,"events":[
        {"type":"turn/start","turn":1},
        {"type":"user/message"},
        {"type":"turn/end","turn":1,"reason":{"kind":"completed"}}
    ]}"#;

    let bundle = import(text).unwrap();
    let error = bundle.events().unwrap_err();

    match error {
        BundleError::InvalidEvent { index, detail } => {
            assert_eq!(index, 1, "the bad event is the second one");
            assert!(!detail.is_empty(), "the detail must say what was wrong");
        }
        other => panic!("expected InvalidEvent, got {other}"),
    }
}

#[test]
fn every_position_in_the_bundle_is_reported_accurately() {
    // The same broken event moved through the list, so an importer that hardcoded 0 or reported the
    // last index instead of the failing one fails here rather than in a bug report.
    for position in 0..4 {
        let mut events = Vec::new();
        for slot in 0..4 {
            if slot == position {
                events.push(r#"{"type":"user/message"}"#.to_owned());
            } else {
                events.push(r#"{"type":"turn/start","turn":1}"#.to_owned());
            }
        }
        let text = format!(
            r#"{{"version":1,"exported_at":0,"events":[{}]}}"#,
            events.join(",")
        );

        let error = import(&text).unwrap().events().unwrap_err();

        match error {
            BundleError::InvalidEvent { index, detail } => {
                assert_eq!(index, position);
                assert!(!detail.is_empty(), "the detail must say what was wrong");
            }
            other => panic!("expected InvalidEvent at {position}, got {other}"),
        }
    }
}

#[test]
fn an_unknown_event_type_is_reported_with_its_index() {
    // A vocabulary this build does not have is indistinguishable from a typo at the JSON layer, and
    // either way the event cannot be restored; the index is what distinguishes them for a reader.
    let text = r#"{"version":1,"exported_at":0,"events":[
        {"type":"turn/start","turn":1},
        {"type":"quantum/entangle"}
    ]}"#;

    let error = import(text).unwrap().events().unwrap_err();

    assert!(
        matches!(error, BundleError::InvalidEvent { index: 1, .. }),
        "{error}"
    );
}

#[test]
fn the_error_message_names_the_position() {
    let text = r#"{"version":1,"exported_at":0,"events":[
        {"type":"turn/start","turn":1},
        {"type":"turn/start","turn":"not a number"}
    ]}"#;

    let error = import(text).unwrap().events().unwrap_err();
    let message = error.to_string();

    assert!(message.contains('1'), "{message}");
}

// ---- empty and optional cases -----------------------------------------------------------------

#[test]
fn an_empty_session_round_trips() {
    let text = export(&[], None, 42).unwrap();
    let bundle = import(&text).unwrap();

    assert!(bundle.events.is_empty());
    assert!(bundle.events().unwrap().is_empty());
    assert_eq!(bundle.exported_at, 42);
}

#[test]
fn the_title_is_optional_and_absent_means_absent() {
    let text = export(&sample(), None, 0).unwrap();
    let bundle = import(&text).unwrap();

    assert_eq!(bundle.title, None);
    // Not serialized as `null`: an unset title is a different value from the string "null" to a
    // consumer that renders a header, and omitting the field keeps that unambiguous.
    assert!(!text.contains("\"title\""), "{text}");
}

#[test]
fn an_empty_title_is_kept_distinct_from_no_title() {
    // A session named the empty string is unusual but not the same as an unnamed one; a bundle that
    // collapsed the two would rename the session on restore.
    let text = export(&[], Some(""), 0).unwrap();
    assert_eq!(import(&text).unwrap().title.as_deref(), Some(""));
}

#[test]
fn an_empty_bundle_string_is_refused() {
    let error = import("").unwrap_err();
    assert!(matches!(error, BundleError::InvalidJson { .. }), "{error}");
}

#[test]
fn the_exported_at_stamp_is_the_value_the_caller_gave() {
    // The clock is a parameter so the stamp is the caller's, not the exporter's; a bundle stamped
    // at export time cannot be reproduced byte for byte in a test or a batch.
    let bundle = import(&export(&[], None, 1_234_567).unwrap()).unwrap();
    assert_eq!(bundle.exported_at, 1_234_567);
}

#[test]
fn a_hand_written_bundle_imports() {
    // The format is a contract with other instances, not just with this exporter; a bundle another
    // writer produced, with fields in a different order, must read the same.
    let text =
        r#"{"events":[{"turn":3,"type":"turn/start"}],"title":"t","exported_at":7,"version":1}"#;
    let bundle = import(text).unwrap();

    assert_eq!(bundle.title.as_deref(), Some("t"));
    assert_eq!(
        bundle.events().unwrap(),
        vec![SessionEvent::TurnStart { turn: 3 }]
    );
}

#[test]
fn a_bundle_produced_by_export_has_the_shape_bundle_declares() {
    // Belt and braces for the pair of functions above: whatever `export` emits is deserializable as
    // the public `Bundle` type directly, so the two are not drifting apart through the indirection.
    let text = export(&sample(), Some("t"), 9).unwrap();
    let typed: Bundle = serde_json::from_str(&text).unwrap();

    assert_eq!(typed.version, VERSION);
    assert_eq!(typed.exported_at, 9);
    assert_eq!(typed.events.len(), 6);
}
