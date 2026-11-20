//! Tests for the incremental `DeepSeek` session-log contribution.
//!
//! Corresponds to upstream `packages/session/session-log-deepseek/tests/{upload,config,feedback-composition}.spec.ts`,
//! restricted to the pure half this port covers: the request-extension service registration and the
//! `WeakMap` handoff are this crate's registry concern rather than a session concern.
//!
//! Every property below is a way for a byte-bounded, resumable upload to look correct while being
//! wrong:
//!
//! - **the byte budget is exact.** An off-by-one in the envelope, the comma, or the watermark's
//!   digits either exceeds the server's limit or drops an event that fit. Both are asserted against
//!   the serialized field rather than against the arithmetic that produced it.
//! - **a prefix is contiguous.** A field that skips an event describes a log the server cannot
//!   fold, and would look like a successful delivery.
//! - **a watermark is evidence.** A malformed acceptance event refuses rather than resetting to
//!   `-1`: resending a whole session on every request is the failure mode that hides a corrupt log.
//! - **an acceptance belongs to one generation and one session.** A fork marker carries the
//!   parent's id; treating it as this session's acknowledgement would skip this session's events.
//! - **an exclusion suppresses the field *and* its callback.** A field without a callback is
//!   resent forever; a callback without a field acknowledges what was never sent.
//! - **a block is reported once.** A session stuck behind one oversized event must not warn on
//!   every request for the rest of its life.
//!
//! The module is included by path rather than through the crate's re-exports, so it compiles against
//! this file as written even though the crate root does not declare it yet. Every path into the crate
//! goes through `girder_core::…` for that reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic
)]

#[path = "../src/session/session_final.rs"]
pub mod session_final_gap;

use girder_core::{SessionEvent, SurfaceOp};

use session_final_gap::{
    ACCEPTANCE_EVENT_TYPE, AcceptanceFold, BlockWarning, Config, ConfigError, DEFAULT_MAX_BYTES,
    EXTENSION_KEY, EXTENSION_VERSION, EmitError, ExclusionError, Exclusions, FitOutcome,
    HeaderFacts, MAX_SAFE_INTEGER, PendingEvent, Placement, Preparation, PrepareError,
    SessionLogCursor, SessionLogExtension, UploadRequest, Uploader, WatermarkError,
    accepted_through, decimal_digits, fit_prefix, json_bytes, wire_event, wire_surface_op,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// A header for `id` at the current generation.
fn header(id: &str) -> HeaderFacts {
    HeaderFacts::new(4, id, 1_700_000_000_000)
}

/// A user prompt at `seq`.
fn user(seq: u64) -> PendingEvent {
    PendingEvent::append(
        &SessionEvent::user_text("m1", "hello"),
        seq,
        1_700_000_000_000 + seq,
    )
    .expect("a user message serializes")
}

/// A non-surface event at `seq`.
fn turn_start(seq: u64) -> PendingEvent {
    PendingEvent::append(
        &SessionEvent::TurnStart { turn: 1 },
        seq,
        1_700_000_000_000 + seq,
    )
    .expect("a turn start serializes")
}

/// An acceptance payload for `session` up to `through`, recorded at `seq`.
fn acceptance(seq: u64, session: &str, version: u64, through: u64) -> PendingEvent {
    let data = serde_json::json!({
        "sessionId": session,
        "sessionFormatVersion": version,
        "throughSeq": through,
    });
    PendingEvent::log_only(ACCEPTANCE_EVENT_TYPE, seq, 1_700_000_000_000 + seq, data)
}

/// A contiguous log with nothing acknowledged: turn start, prompt.
///
/// An acceptance marker is itself a log event, so it is forwarded like any other row once it is
/// past the watermark — which is why the "nothing pending" fixtures are built without one.
fn pending_log() -> Vec<PendingEvent> {
    vec![turn_start(1), user(2)]
}

/// The same log with an acceptance of everything up to seq 2 recorded at seq 3.
fn acknowledged_log() -> Vec<PendingEvent> {
    let mut events = pending_log();
    events.push(acceptance(3, "s1", 4, 2));
    events
}

// ---- configuration -----------------------------------------------------------------------------

#[test]
fn the_default_config_enables_an_eight_mebibyte_budget() {
    let config = Config::default();
    assert!(config.enabled);
    assert_eq!(config.max_bytes, DEFAULT_MAX_BYTES);
    assert_eq!(config.max_bytes, 8 * 1024 * 1024);
}

#[test]
fn a_zero_budget_is_refused_because_no_event_could_ever_fit() {
    assert_eq!(Config::new(true, 0), Err(ConfigError::ZeroMaxBytes));
    assert!(Config::new(true, 1).is_ok());
    assert!(ConfigError::ZeroMaxBytes.to_string().contains("maxBytes"));
}

#[test]
fn the_volatile_switch_is_read_per_contribution() {
    let mut uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let events = pending_log();

    uploader.set_enabled(false);
    assert_eq!(
        uploader.prepare(&mut cursor, &header("s1"), &events, &request),
        Ok(Preparation::Disabled)
    );
    uploader.set_enabled(true);
    assert!(matches!(
        uploader.prepare(&mut cursor, &header("s1"), &events, &request),
        Ok(Preparation::Ready(_))
    ));
}

// ---- wire header -------------------------------------------------------------------------------

#[test]
fn the_wire_header_carries_every_released_field() {
    let mut facts = header("s1");
    facts.cwd = Some("/work".to_owned());
    facts.parent_session = Some("p1".to_owned());
    facts.is_seeded = true;
    facts.inherited_event_count = 7;
    facts.origin = Some("subagent".to_owned());
    facts.delegation_depth = 2;
    facts.agent_preset = Some("default".to_owned());

    let wire = session_final_gap::WireHeader::from_facts(&facts);
    let value = wire.to_value();
    assert_eq!(value["version"], serde_json::json!(4));
    assert_eq!(value["id"], serde_json::json!("s1"));
    assert_eq!(value["createdAt"], serde_json::json!(1_700_000_000_000u64));
    assert_eq!(value["cwd"], serde_json::json!("/work"));
    assert_eq!(value["parentSession"], serde_json::json!("p1"));
    assert_eq!(value["origin"], serde_json::json!("subagent"));
    assert_eq!(value["delegationDepth"], serde_json::json!(2));
    assert_eq!(value["agentPreset"], serde_json::json!("default"));
}

#[test]
fn an_unseeded_session_omits_the_seed_length_rather_than_writing_zero() {
    // The distinction is the whole point: `seedLength: 0` says "a cut exists and is empty", while
    // absence says "this log starts here". Only one of them is true for a fresh session.
    let value = session_final_gap::WireHeader::from_facts(&header("s1")).to_value();
    assert!(value.get("seedLength").is_none(), "{value}");
    assert_eq!(value.get("parentSession"), None);
    assert_eq!(value.get("cwd"), None);

    let mut seeded = header("s1");
    seeded.is_seeded = true;
    seeded.inherited_event_count = 7;
    let value = session_final_gap::WireHeader::from_facts(&seeded).to_value();
    assert_eq!(value["seedLength"], serde_json::json!(7));
}

#[test]
fn a_seeded_session_with_an_empty_cut_still_states_it() {
    // Zero is a legitimate inherited length for a seeded session — a fork taken at the start of
    // its parent — and it must not collapse into "unseeded".
    let mut seeded = header("s1");
    seeded.is_seeded = true;
    seeded.inherited_event_count = 0;
    let value = session_final_gap::WireHeader::from_facts(&seeded).to_value();
    assert_eq!(value["seedLength"], serde_json::json!(0));
}

// ---- wire events -------------------------------------------------------------------------------

#[test]
fn an_append_surface_op_serializes_as_the_bare_string() {
    assert_eq!(
        wire_surface_op(SurfaceOp::Append),
        serde_json::json!("append")
    );
}

#[test]
fn a_replacement_surface_op_serializes_its_inclusive_range() {
    assert_eq!(
        wire_surface_op(SurfaceOp::Replace { start: 3, end: 5 }),
        serde_json::json!({"op": "replace", "startSeq": 3, "endSeq": 5})
    );
}

#[test]
fn a_surface_eligible_event_appends_by_default() {
    let wire = wire_event(&user(2)).expect("a prompt is surface-eligible");
    assert_eq!(wire["type"], serde_json::json!("user/message"));
    assert_eq!(wire["seq"], serde_json::json!(2));
    assert_eq!(wire["surfaceOp"], serde_json::json!("append"));
    assert!(wire.get("sourceEventSeqs").is_none());
    assert!(wire.get("ignorable").is_none());
}

#[test]
fn a_non_surface_event_carries_no_placement() {
    let wire = wire_event(&turn_start(1)).expect("a turn start is log-only");
    assert_eq!(wire["type"], serde_json::json!("turn/start"));
    assert!(wire.get("surfaceOp").is_none(), "{wire}");
}

#[test]
fn a_surface_eligible_event_without_a_placement_is_refused() {
    let event = PendingEvent::log_only("user/message", 2, 5, serde_json::json!({}));
    assert_eq!(
        wire_event(&event),
        Err(EmitError::MissingSurfaceOp {
            type_name: "user/message".to_owned(),
            seq: 2,
        })
    );
}

#[test]
fn a_non_surface_event_claiming_a_placement_is_refused() {
    let event = PendingEvent::log_only("turn/start", 1, 5, serde_json::json!({}));
    let event = PendingEvent {
        placement: Placement::Surface {
            op: SurfaceOp::Append,
            sources: Vec::new(),
        },
        ..event
    };
    assert_eq!(
        wire_event(&event),
        Err(EmitError::UnexpectedSurfaceOp {
            type_name: "turn/start".to_owned(),
            seq: 1,
        })
    );
}

#[test]
fn provenance_is_emitted_only_when_it_is_non_empty() {
    let mut event = user(4);
    event.placement = Placement::Surface {
        op: SurfaceOp::Replace { start: 2, end: 2 },
        sources: vec![2],
    };
    let wire = wire_event(&event).expect("a replacement with provenance");
    assert_eq!(wire["sourceEventSeqs"], serde_json::json!([2]));
    assert_eq!(
        wire["surfaceOp"],
        serde_json::json!({"op": "replace", "startSeq": 2, "endSeq": 2})
    );

    // An empty citation list means the same as none, and only "none" is what this build writes.
    event.placement = Placement::Surface {
        op: SurfaceOp::Append,
        sources: Vec::new(),
    };
    let wire = wire_event(&event).expect("an append needs no provenance");
    assert!(wire.get("sourceEventSeqs").is_none());
}

#[test]
fn an_assistant_message_may_not_cite_sources() {
    // The model's answer is not derived from earlier surface nodes, so upstream's wire type forbids
    // the field outright. A value that carries one is a writer bug, not something to strip.
    let event = PendingEvent::current(
        &SessionEvent::assistant_text("m2", "hi", "deepseek"),
        4,
        9,
        Placement::Surface {
            op: SurfaceOp::Replace { start: 2, end: 2 },
            sources: vec![2],
        },
    )
    .expect("an assistant message serializes");
    assert_eq!(
        wire_event(&event),
        Err(EmitError::UnexpectedSources {
            type_name: "assistant/message".to_owned(),
            seq: 4,
        })
    );
}

#[test]
fn an_opaque_record_keeps_its_placement_unexamined() {
    let event = PendingEvent {
        placement: Placement::Opaque {
            surface_op: Some(serde_json::json!("append")),
            sources: Some(serde_json::json!([7])),
        },
        ..PendingEvent::opaque("plugin/custom", 9, 11, serde_json::json!({"x": 1}))
    };
    let wire = wire_event(&event).expect("an ignorable opaque record keeps its fields");
    assert_eq!(wire["ignorable"], serde_json::json!(true));
    assert_eq!(wire["surfaceOp"], serde_json::json!("append"));
    assert_eq!(wire["sourceEventSeqs"], serde_json::json!([7]));
}

#[test]
fn an_opaque_record_that_is_not_ignorable_is_refused() {
    let event = PendingEvent {
        ignorable: false,
        ..PendingEvent::opaque("plugin/custom", 9, 11, serde_json::json!({}))
    };
    assert_eq!(
        wire_event(&event),
        Err(EmitError::OpaqueNotIgnorable {
            type_name: "plugin/custom".to_owned(),
            seq: 9,
        })
    );
}

#[test]
fn a_known_type_outside_the_surface_is_uploadable_as_log_only() {
    // Includes the acceptance marker itself, whose type this crate models as a wire string rather
    // than as a `SessionEvent` variant: a transport layer that refused it could never forward the
    // evidence its own watermark fold depends on.
    for type_name in [
        "turn/start",
        "model/usage",
        "session/title",
        "llm/retry",
        ACCEPTANCE_EVENT_TYPE,
    ] {
        let event = PendingEvent::log_only(type_name, 1, 1, serde_json::json!({}));
        let wire = wire_event(&event).unwrap_or_else(|error| panic!("{type_name}: {error}"));
        assert_eq!(wire["type"], serde_json::json!(type_name));
        assert!(wire.get("surfaceOp").is_none());
    }
}

#[test]
fn the_payload_loses_only_its_type_tag() {
    let event = PendingEvent::append(&SessionEvent::TurnStart { turn: 7 }, 1, 3)
        .expect("a turn start serializes");
    assert_eq!(event.data, serde_json::json!({"turn": 7}));
    assert_eq!(event.type_name, "turn/start");
}

// ---- byte accounting ---------------------------------------------------------------------------

#[test]
fn decimal_digits_are_counted_without_formatting() {
    assert_eq!(decimal_digits(0), 1);
    assert_eq!(decimal_digits(9), 1);
    assert_eq!(decimal_digits(10), 2);
    assert_eq!(decimal_digits(99), 2);
    assert_eq!(decimal_digits(100), 3);
    assert_eq!(decimal_digits(MAX_SAFE_INTEGER), 16);
}

#[test]
fn json_bytes_measures_the_utf8_length_of_the_rendering() {
    let value = serde_json::json!({"a": "héllo"});
    assert_eq!(
        json_bytes(&value),
        Some(serde_json::to_string(&value).unwrap().len())
    );
    // The accented character is two bytes; a `.chars().count()` would under-count by one.
    assert_eq!(json_bytes(&serde_json::json!("é")), Some(4));
}

#[test]
fn the_base_field_bytes_exclude_the_events_and_the_watermark_digits() {
    let extension = SessionLogExtension {
        version: EXTENSION_VERSION,
        session_format_version: 4,
        session: session_final_gap::WireHeader::from_facts(&header("s1")),
        after_seq: -1,
        through_seq: 0,
        events: Vec::new(),
    };
    let base = extension
        .base_field_bytes()
        .expect("the envelope serializes");
    let empty = extension.to_value();
    // The rendered field is the base plus exactly the digits of `throughSeq` (one, for zero).
    assert_eq!(
        json_bytes(&empty).expect("serializes"),
        base + decimal_digits(0)
    );
}

#[test]
fn the_base_field_bytes_grow_with_the_envelope_that_has_to_carry_them() {
    let small = SessionLogExtension {
        version: EXTENSION_VERSION,
        session_format_version: 4,
        session: session_final_gap::WireHeader::from_facts(&header("s1")),
        after_seq: -1,
        through_seq: 0,
        events: Vec::new(),
    };
    let mut large_facts = header("s1");
    large_facts.cwd = Some("/a/very/long/project/directory/name".to_owned());
    large_facts.parent_session = Some("parent-session-with-a-long-id".to_owned());
    let large = SessionLogExtension {
        session: session_final_gap::WireHeader::from_facts(&large_facts),
        ..small.clone()
    };
    assert!(large.base_field_bytes() > small.base_field_bytes());
}

// ---- prefix fitting ----------------------------------------------------------------------------

#[test]
fn an_empty_pending_tail_needs_no_field() {
    let base = SessionLogExtension {
        version: EXTENSION_VERSION,
        session_format_version: 4,
        session: session_final_gap::WireHeader::from_facts(&header("s1")),
        after_seq: -1,
        through_seq: 0,
        events: Vec::new(),
    }
    .base_field_bytes()
    .unwrap();
    assert_eq!(fit_prefix(base, &[], 1024), Ok(FitOutcome::Empty));
}

#[test]
fn every_pending_event_fits_when_the_budget_is_generous() {
    let events = vec![turn_start(1), user(2)];
    let outcome = fit_prefix(0, &events, 16 * 1024).expect("both events project");
    let FitOutcome::Fit(fit) = outcome else {
        panic!("expected a fit, got {outcome:?}");
    };
    assert_eq!(fit.events.len(), 2);
    assert_eq!(fit.through_seq, 2);
}

#[test]
fn the_reported_field_bytes_equal_the_serialized_field() {
    let extension = SessionLogExtension {
        version: EXTENSION_VERSION,
        session_format_version: 4,
        session: session_final_gap::WireHeader::from_facts(&header("s1")),
        after_seq: -1,
        through_seq: 0,
        events: Vec::new(),
    };
    let base = extension
        .base_field_bytes()
        .expect("the envelope serializes");
    let events = vec![turn_start(1), user(2), user(3)];
    let outcome = fit_prefix(base, &events, 16 * 1024).expect("all events project");
    let FitOutcome::Fit(fit) = outcome else {
        panic!("expected a fit, got {outcome:?}");
    };
    let rendered = SessionLogExtension {
        through_seq: fit.through_seq,
        events: fit.events.clone(),
        ..extension
    }
    .to_value();
    // The whole point of the arithmetic: the number it returns is the number the request carries.
    assert_eq!(fit.field_bytes, json_bytes(&rendered).expect("serializes"));
}

#[test]
fn the_longest_fitting_prefix_is_the_one_admitted() {
    let extension = SessionLogExtension {
        version: EXTENSION_VERSION,
        session_format_version: 4,
        session: session_final_gap::WireHeader::from_facts(&header("s1")),
        after_seq: -1,
        through_seq: 0,
        events: Vec::new(),
    };
    let base = extension
        .base_field_bytes()
        .expect("the envelope serializes");
    let events = vec![turn_start(1), user(2), user(3), user(4)];

    // Measure the field that carries exactly two events, then present that as the budget: one more
    // event must not fit, and dropping one must succeed.
    let two = fit_prefix(base, &events, 16 * 1024).expect("fits");
    let FitOutcome::Fit(all) = two else {
        panic!("expected a fit");
    };
    assert_eq!(all.events.len(), 4);

    let exact = fit_prefix(base, &events[..2], 16 * 1024).expect("fits");
    let FitOutcome::Fit(two_events) = exact else {
        panic!("expected a fit");
    };
    let budget = two_events.field_bytes;

    let outcome = fit_prefix(base, &events, budget).expect("fits within the exact budget");
    let FitOutcome::Fit(fit) = outcome else {
        panic!("expected a fit, got {outcome:?}");
    };
    assert_eq!(
        fit.events.len(),
        2,
        "an exact budget must admit exactly its events"
    );
    assert_eq!(fit.through_seq, 2);

    let outcome = fit_prefix(base, &events, budget + 1).expect("fits");
    let FitOutcome::Fit(fit) = outcome else {
        panic!("expected a fit");
    };
    assert_eq!(
        fit.events.len(),
        2,
        "one extra byte is not enough for a third event"
    );
}

#[test]
fn an_event_that_alone_exceeds_the_budget_blocks_the_whole_session() {
    let extension = SessionLogExtension {
        version: EXTENSION_VERSION,
        session_format_version: 4,
        session: session_final_gap::WireHeader::from_facts(&header("s1")),
        after_seq: -1,
        through_seq: 0,
        events: Vec::new(),
    };
    let base = extension
        .base_field_bytes()
        .expect("the envelope serializes");
    let events = vec![turn_start(1)];
    let outcome = fit_prefix(base, &events, 1).expect("the event projects");
    let FitOutcome::Blocked(blocked) = outcome else {
        panic!("expected a block, got {outcome:?}");
    };
    assert_eq!(blocked.seq, 1);
    assert!(blocked.bytes.is_some_and(|bytes| bytes > 1));
}

#[test]
fn a_later_event_is_not_admitted_around_a_blocked_one() {
    // A prefix has to stay contiguous, so the scan stops at the first event that does not fit even
    // when every later event is small enough on its own.
    let extension = SessionLogExtension {
        version: EXTENSION_VERSION,
        session_format_version: 4,
        session: session_final_gap::WireHeader::from_facts(&header("s1")),
        after_seq: -1,
        through_seq: 0,
        events: Vec::new(),
    };
    let base = extension
        .base_field_bytes()
        .expect("the envelope serializes");
    let mut huge = user(1);
    huge.data = serde_json::json!({ "message": { "text": "x".repeat(4096) } });
    let events = vec![huge, turn_start(2), user(3)];
    let outcome = fit_prefix(base, &events, base + 512).expect("all events project");
    let FitOutcome::Blocked(blocked) = outcome else {
        panic!("expected a block, got {outcome:?}");
    };
    assert_eq!(blocked.seq, 1);
}

// ---- acceptance watermark ----------------------------------------------------------------------

#[test]
fn a_fresh_log_has_acknowledged_nothing() {
    let mut fold = AcceptanceFold::default();
    let events = vec![turn_start(1), user(2)];
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(-1));
    assert!(!fold.has_acknowledgement());
}

#[test]
fn an_acceptance_advances_the_watermark() {
    let mut fold = AcceptanceFold::default();
    let events = acknowledged_log();
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(2));
    assert!(fold.has_acknowledgement());
    assert_eq!(fold.scanned_events, 3);
}

#[test]
fn the_highest_acceptance_wins() {
    let mut fold = AcceptanceFold::default();
    let events = vec![
        turn_start(1),
        user(2),
        acceptance(3, "s1", 4, 1),
        user(4),
        acceptance(5, "s1", 4, 4),
        acceptance(6, "s1", 4, 2),
    ];
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(4));
}

#[test]
fn an_acceptance_from_another_generation_is_ignored() {
    // A log that was migrated in place carries the older generation's acknowledgements. They name
    // events that no longer mean the same thing, so they must not skip the new lane's prefix.
    let mut fold = AcceptanceFold::default();
    let events = vec![turn_start(1), user(2), acceptance(3, "s1", 3, 2)];
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(-1));
}

#[test]
fn an_acceptance_carrying_the_parents_id_is_ignored() {
    // A seeded log inherits its parent's acceptance markers verbatim. Folding them would skip this
    // session's whole inherited prefix on the strength of a delivery of the parent's events.
    let mut fold = AcceptanceFold::default();
    let events = vec![turn_start(1), user(2), acceptance(3, "parent", 4, 2)];
    let mut seeded = header("s1");
    seeded.is_seeded = true;
    seeded.parent_session = Some("parent".to_owned());
    seeded.inherited_event_count = 2;
    assert_eq!(accepted_through(&events, &seeded, &mut fold), Ok(-1));
}

#[test]
fn a_zero_generation_is_the_absent_one() {
    // An acceptance with no `sessionFormatVersion` was recorded by the version-zero lane; it is not
    // malformed, it is historical.
    let mut fold = AcceptanceFold::default();
    let data = serde_json::json!({"sessionId": "s1", "throughSeq": 2});
    let events = vec![
        turn_start(1),
        user(2),
        PendingEvent::log_only(ACCEPTANCE_EVENT_TYPE, 3, 9, data),
    ];
    let mut v0 = header("s1");
    v0.version = 0;
    assert_eq!(accepted_through(&events, &v0, &mut fold), Ok(2));
    // The same marker is ignored by a current-generation session.
    let mut fold = AcceptanceFold::default();
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(-1));
}

#[test]
fn a_malformed_format_version_refuses_rather_than_resetting() {
    for bad in [
        serde_json::json!("4"),
        serde_json::json!(-1),
        serde_json::json!(4.5),
        serde_json::json!(MAX_SAFE_INTEGER + 1),
    ] {
        let mut fold = AcceptanceFold::default();
        let data = serde_json::json!({
            "sessionId": "s1",
            "sessionFormatVersion": bad,
            "throughSeq": 2,
        });
        let events = vec![
            turn_start(1),
            user(2),
            PendingEvent::log_only(ACCEPTANCE_EVENT_TYPE, 3, 9, data),
        ];
        assert_eq!(
            accepted_through(&events, &header("s1"), &mut fold),
            Err(WatermarkError::MalformedFormatVersion { seq: 3 }),
            "for {bad}"
        );
    }
}

#[test]
fn a_malformed_watermark_refuses_rather_than_resetting() {
    let cases = [
        serde_json::json!({"sessionId": "s1", "sessionFormatVersion": 4, "throughSeq": -1}),
        serde_json::json!({"sessionId": "s1", "sessionFormatVersion": 4, "throughSeq": "2"}),
        serde_json::json!({"sessionId": "s1", "sessionFormatVersion": 4, "throughSeq": 2.5}),
        serde_json::json!({"sessionId": "", "sessionFormatVersion": 4, "throughSeq": 2}),
        serde_json::json!({"sessionFormatVersion": 4, "throughSeq": 2}),
        serde_json::json!({"sessionId": "s1", "sessionFormatVersion": 4}),
        serde_json::json!({"sessionId": "s1", "sessionFormatVersion": 4,
            "throughSeq": MAX_SAFE_INTEGER + 1}),
    ];
    for data in cases {
        let mut fold = AcceptanceFold::default();
        let events = vec![
            turn_start(1),
            user(2),
            PendingEvent::log_only(ACCEPTANCE_EVENT_TYPE, 3, 9, data.clone()),
        ];
        assert_eq!(
            accepted_through(&events, &header("s1"), &mut fold),
            Err(WatermarkError::MalformedWatermark { seq: 3 }),
            "for {data}"
        );
    }
}

#[test]
fn a_watermark_at_or_after_its_own_event_is_refused() {
    // The marker is appended *after* what it acknowledges, so a watermark at or beyond its own
    // position is either a self-acknowledgement or a claim about events that follow it.
    for through in [3, 4, 99] {
        let mut fold = AcceptanceFold::default();
        let events = vec![turn_start(1), user(2), acceptance(3, "s1", 4, through)];
        assert_eq!(
            accepted_through(&events, &header("s1"), &mut fold),
            Err(WatermarkError::WatermarkNotBeforeEvent { seq: 3 }),
            "for throughSeq {through}"
        );
    }
}

#[test]
fn a_non_object_acceptance_payload_is_refused() {
    let mut fold = AcceptanceFold::default();
    let events = vec![
        turn_start(1),
        PendingEvent::log_only(ACCEPTANCE_EVENT_TYPE, 2, 9, serde_json::json!("nope")),
    ];
    assert_eq!(
        accepted_through(&events, &header("s1"), &mut fold),
        Err(WatermarkError::MalformedWatermark { seq: 2 })
    );
}

#[test]
fn a_hole_in_the_log_refuses_the_upload() {
    let mut fold = AcceptanceFold::default();
    let events = vec![turn_start(1), user(3)];
    assert_eq!(
        accepted_through(&events, &header("s1"), &mut fold),
        Err(WatermarkError::NonContiguous {
            expected: 2,
            actual: 3,
        })
    );
}

#[test]
fn a_second_scan_reads_only_the_appended_events() {
    let mut fold = AcceptanceFold::default();
    let mut events = vec![turn_start(1), user(2)];
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(-1));
    assert_eq!(fold.scanned_events, 2);

    // Appending must not re-walk the folded prefix: the cursor is the whole cache.
    events.push(user(3));
    events.push(acceptance(4, "s1", 4, 3));
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(3));
    assert_eq!(fold.scanned_events, 4);
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(3));
}

#[test]
fn a_shrunk_log_is_refused_because_the_cursor_remembers_more_than_exists() {
    let mut fold = AcceptanceFold::default();
    let events = vec![turn_start(1), user(2)];
    assert_eq!(accepted_through(&events, &header("s1"), &mut fold), Ok(-1));

    let shrunk = vec![turn_start(1)];
    assert_eq!(
        accepted_through(&shrunk, &header("s1"), &mut fold),
        Err(WatermarkError::MissingEvent {
            index: 1,
            captured_length: 2,
        })
    );
}

// ---- exclusions --------------------------------------------------------------------------------

#[test]
fn a_registered_exclusion_suppresses_matching_requests() {
    let mut exclusions = Exclusions::new();
    exclusions.register(|request| Ok(request.purpose == Some("session-title")));
    assert!(
        exclusions
            .excludes(&UploadRequest {
                session_id: "s1",
                purpose: Some("session-title"),
            })
            .expect("the predicate answers")
    );
    assert!(
        !exclusions
            .excludes(&UploadRequest {
                session_id: "s1",
                purpose: None,
            })
            .expect("the predicate answers")
    );
}

#[test]
fn any_matching_exclusion_is_enough() {
    let mut exclusions = Exclusions::new();
    exclusions.register(|_| Ok(false));
    exclusions.register(|request| Ok(request.session_id == "s2"));
    assert!(
        exclusions
            .excludes(&UploadRequest {
                session_id: "s2",
                purpose: None,
            })
            .expect("the predicate answers")
    );
    assert_eq!(exclusions.len(), 2);
}

#[test]
fn a_removed_exclusion_stops_applying_and_removal_is_idempotent() {
    let mut exclusions = Exclusions::new();
    let handle = exclusions.register(|_| Ok(true));
    assert!(exclusions.remove(handle));
    assert!(!exclusions.remove(handle));
    assert!(exclusions.is_empty());
    assert!(
        !exclusions
            .excludes(&UploadRequest {
                session_id: "s1",
                purpose: None,
            })
            .expect("no predicate answers")
    );
}

#[test]
fn a_failing_exclusion_fails_the_contribution_and_names_why() {
    let mut uploader = Uploader::new(Config::default());
    uploader.register_exclusion(|_| Err(ExclusionError::new("boom")));
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    assert_eq!(
        uploader.prepare(&mut cursor, &header("s1"), &pending_log(), &request),
        Err(PrepareError::Exclusion(ExclusionError::new("boom")))
    );
}

#[test]
fn an_exclusion_is_evaluated_before_the_log_is_read() {
    // Otherwise a suppressed auxiliary request would still pay for the acceptance fold, and — worse
    // — would advance the cursor past a malformed marker it was never going to act on.
    let broken = vec![
        turn_start(1),
        // A matching generation with no watermark: this reaches validation instead of being
        // skipped as another lane's marker.
        PendingEvent::log_only(
            ACCEPTANCE_EVENT_TYPE,
            2,
            9,
            serde_json::json!({"sessionId": "s1", "sessionFormatVersion": 4}),
        ),
    ];
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };

    // Not excluded, so the malformed marker is reached and reported.
    let uploader = Uploader::new(Config::default());
    assert!(matches!(
        uploader.prepare(
            &mut SessionLogCursor::new(),
            &header("s1"),
            &broken,
            &request
        ),
        Err(PrepareError::Watermark(_))
    ));

    // Excluded, so the log is never folded at all.
    let mut excluding = Uploader::new(Config::default());
    excluding.register_exclusion(|_| Ok(true));
    let mut cursor = SessionLogCursor::new();
    assert_eq!(
        excluding.prepare(&mut cursor, &header("s1"), &broken, &request),
        Ok(Preparation::Excluded)
    );
    // The cursor is the proof: a fold that had run would have advanced it.
    assert_eq!(cursor.fold.scanned_events, 0);
}

#[test]
fn the_registry_exposes_what_is_registered() {
    let mut uploader = Uploader::new(Config::default());
    assert!(uploader.exclusions().is_empty());
    let handle = uploader.register_exclusion(|_| Ok(false));
    assert_eq!(uploader.exclusions().len(), 1);
    assert!(uploader.remove_exclusion(handle));
    assert!(uploader.exclusions().is_empty());
}

// ---- block reporting ---------------------------------------------------------------------------

#[test]
fn a_block_is_reported_once_per_sequence() {
    let mut warning = BlockWarning::new();
    assert_eq!(warning.last(), None);
    assert!(warning.should_log(7));
    assert!(!warning.should_log(7));
    assert!(!warning.should_log(7));
    assert_eq!(warning.last(), Some(7));
    // A different blocked event is a different report.
    assert!(warning.should_log(8));
}

// ---- the prepare decision ----------------------------------------------------------------------

/// A prepared preparation, or a panic naming what came back instead.
fn ready(preparation: Preparation) -> session_final_gap::PreparedUpload {
    match preparation {
        Preparation::Ready(upload) => upload,
        other => panic!("expected a ready upload, got {other:?}"),
    }
}

#[test]
fn a_session_with_pending_events_prepares_a_field() {
    let uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let upload = ready(
        uploader
            .prepare(&mut cursor, &header("s1"), &pending_log(), &request)
            .expect("a clean log prepares"),
    );
    assert_eq!(upload.through_seq(), 2);
    let value = upload.to_value();
    assert_eq!(value["version"], serde_json::json!(EXTENSION_VERSION));
    assert_eq!(value["sessionFormatVersion"], serde_json::json!(4));
    assert_eq!(value["afterSeq"], serde_json::json!(-1));
    assert_eq!(value["throughSeq"], serde_json::json!(2));
    assert_eq!(value["events"].as_array().map(Vec::len), Some(2));
    assert_eq!(value["session"]["id"], serde_json::json!("s1"));
    // The key the request carries the field under.
    assert_eq!(EXTENSION_KEY, "dsh_session_log");
}

#[test]
fn a_fully_acknowledged_log_contributes_nothing() {
    let uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let events = vec![turn_start(1), user(2), acceptance(3, "s1", 4, 2)];
    assert_eq!(
        uploader
            .prepare(&mut cursor, &header("s1"), &events, &request)
            .expect("a clean log prepares"),
        Preparation::Absent
    );
}

#[test]
fn only_the_unacknowledged_tail_is_uploaded() {
    let uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let events = vec![
        turn_start(1),
        user(2),
        acceptance(3, "s1", 4, 2),
        user(4),
        user(5),
    ];
    let upload = ready(
        uploader
            .prepare(&mut cursor, &header("s1"), &events, &request)
            .expect("a clean log prepares"),
    );
    assert_eq!(upload.value.after_seq, 2);
    assert_eq!(upload.through_seq(), 5);
    assert_eq!(upload.value.events.len(), 2);
    let seqs: Vec<_> = upload
        .value
        .events
        .iter()
        .map(|event| event["seq"].clone())
        .collect();
    assert_eq!(seqs, vec![serde_json::json!(4), serde_json::json!(5)]);
}

#[test]
fn the_acceptance_callback_names_exactly_what_the_field_covered() {
    let uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let upload = ready(
        uploader
            .prepare(&mut cursor, &header("s1"), &pending_log(), &request)
            .expect("a clean log prepares"),
    );
    let acceptance = upload.acceptance();
    assert_eq!(acceptance.session_id, "s1");
    assert_eq!(acceptance.session_format_version, 4);
    assert_eq!(acceptance.through_seq, upload.through_seq());
    assert_eq!(
        acceptance.data(),
        serde_json::json!({
            "sessionId": "s1",
            "sessionFormatVersion": 4,
            "throughSeq": 2,
        })
    );
}

#[test]
fn an_accepted_field_is_not_resent() {
    let uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let mut events = pending_log();
    let upload = ready(
        uploader
            .prepare(&mut cursor, &header("s1"), &events, &request)
            .expect("a clean log prepares"),
    );
    // Simulate the endpoint accepting: the caller appends the marker the callback produced.
    let through = upload.through_seq();
    events.push(PendingEvent::log_only(
        ACCEPTANCE_EVENT_TYPE,
        through + 1,
        9,
        upload.acceptance().data(),
    ));
    assert_eq!(
        uploader
            .prepare(&mut cursor, &header("s1"), &events, &request)
            .expect("the acknowledged log prepares"),
        Preparation::Absent
    );
}

#[test]
fn a_blocked_upload_reports_the_event_and_its_cost() {
    let uploader = Uploader::new(Config::new(true, 8).expect("a valid budget"));
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let Preparation::Blocked(blocked) = uploader
        .prepare(&mut cursor, &header("s1"), &pending_log(), &request)
        .expect("a clean log prepares")
    else {
        panic!("expected a block");
    };
    assert_eq!(blocked.seq, 1);
    assert!(blocked.bytes.is_some_and(|bytes| bytes > 8));
}

#[test]
fn a_budget_that_admits_the_first_event_only_uploads_that_one() {
    // A real end-to-end byte bound: measure the field carrying one event, then use it as the budget
    // and check that the second event is left behind rather than admitted.
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let events = vec![turn_start(1), user(2), user(3)];
    let generous = Uploader::new(Config::default());
    let one = ready(
        generous
            .prepare(
                &mut SessionLogCursor::new(),
                &header("s1"),
                &events[..1],
                &request,
            )
            .expect("the first event prepares"),
    );
    let budget = json_bytes(&one.to_value()).expect("serializes");

    let tight = Uploader::new(Config::new(true, budget).expect("a valid budget"));
    let fit = ready(
        tight
            .prepare(
                &mut SessionLogCursor::new(),
                &header("s1"),
                &events,
                &request,
            )
            .expect("the log prepares"),
    );
    assert_eq!(fit.through_seq(), 1);
    assert_eq!(fit.value.events.len(), 1);
    assert!(json_bytes(&fit.to_value()).expect("serializes") <= budget);
}

#[test]
fn a_malformed_log_fails_prepare_rather_than_uploading_a_guess() {
    let uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let events = vec![
        turn_start(1),
        PendingEvent::log_only(
            ACCEPTANCE_EVENT_TYPE,
            2,
            9,
            serde_json::json!({"sessionId": "s1", "sessionFormatVersion": 4, "throughSeq": -1}),
        ),
    ];
    assert!(matches!(
        uploader.prepare(&mut cursor, &header("s1"), &events, &request),
        Err(PrepareError::Watermark(
            WatermarkError::MalformedWatermark { seq: 2 }
        ))
    ));
}

#[test]
fn an_event_that_cannot_be_projected_fails_prepare() {
    let uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    // A prompt with no placement: the field would tell the server the session's surface is one
    // this build never folded.
    let events = vec![PendingEvent::log_only(
        "user/message",
        1,
        9,
        serde_json::json!({}),
    )];
    assert_eq!(
        uploader.prepare(&mut cursor, &header("s1"), &events, &request),
        Err(PrepareError::Emit(EmitError::MissingSurfaceOp {
            type_name: "user/message".to_owned(),
            seq: 1,
        }))
    );
}

#[test]
fn the_prepare_error_renders_its_cause() {
    let cases = [
        PrepareError::Exclusion(ExclusionError::new("boom")),
        PrepareError::Watermark(WatermarkError::MalformedWatermark { seq: 3 }),
        PrepareError::Emit(EmitError::Unserializable { seq: 4 }),
    ];
    for error in cases {
        assert!(!error.to_string().is_empty(), "{error:?}");
    }
}

// ---- the field version -------------------------------------------------------------------------

#[test]
fn the_extension_version_is_the_only_one_written() {
    let uploader = Uploader::new(Config::default());
    let mut cursor = SessionLogCursor::new();
    let request = UploadRequest {
        session_id: "s1",
        purpose: None,
    };
    let upload = ready(
        uploader
            .prepare(&mut cursor, &header("s1"), &pending_log(), &request)
            .expect("a clean log prepares"),
    );
    assert_eq!(upload.value.version, EXTENSION_VERSION);
    assert_eq!(EXTENSION_VERSION, 1);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_final_gap as session_final;
}
