//! Tests for the `tokenMeasurement` recovery fold.
//!
//! The module is included by path because `girder-core`'s crate root does not declare it yet —
//! wiring `pub mod token_measurement_gap;` and its `pub use` belongs to the integrator. Under that
//! inclusion the module is a child of *this* test crate, which is why it reaches the crate's types
//! through `girder_core::` re-exports rather than `crate::` paths: those resolve the same way in both
//! builds, so one source file is the single thing under test.
//!
//! # What these tests are for
//!
//! Three claims carry the weight, and each is checked from the side that can fail.
//!
//! The first is that the fold's state is **bounded**. Upstream's own test appends 300 messages and
//! asserts the checkpoint is unchanged and under 200 bytes; the same is asserted here, because a
//! projection that grew with the log would defeat the reason it is a projection rather than a scan.
//! A test that only checked "it serializes" would pass against a state carrying the whole surface.
//!
//! The second is that a failed boundary is **recorded, not thrown** — and that the diagnosis is not
//! lost. Upstream's `apply` swallows the error so sibling projections keep folding, and `measure()`
//! replays to the recorded position to re-raise it. Both halves are asserted: a nested `step/start`
//! freezes the state at the right sequence, later valid events leave it frozen, and `validate`
//! returns the *specific* error rather than a generic failure.
//!
//! The third is that the baseline choice is **not** "whichever number is available". The provider's
//! usage wins only when it is at least as large as the heuristic price of the same anchored surface;
//! a truncated usage record must lose to the heuristic. That is the one rule a naive
//! `usage.unwrap_or(estimate)` gets wrong, so it is tested in both directions, together with the
//! header-mismatch case where the delta must be zero rather than a like-for-like number.

#[path = "../src/token_measurement_gap.rs"]
mod token_measurement_gap;

use girder_core::request_header_gap::{AdapterDefaults, EpochHeader, HeaderConfig};
use girder_core::{
    ContentBlock, MessageSource, Projection, ProjectionCache, ProjectionSeq as Seq, SessionEvent,
    SurfaceEnvelope, ToolSchema, VersionedProjection, derive_event_message,
};
use token_measurement_gap::{
    AssistantMeasurement, Measurement, MeasurementAnchor, MeasurementBaseline, MeasurementError,
    MeasurementInput, MeasurementState, MeasurementStateError, ROLE_OVERHEAD, StepCoordinate,
    TOKEN_MEASUREMENT_KEY, TOKEN_MEASUREMENT_STATE_VERSION, TokenMeasurementProjection, UsageFacts,
    advance_measurement, measure_measurement, measurement_assistant_tokens, measurement_usage,
    token_measurement_definition,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// A header naming one route.
fn header(provider: &str, model: &str) -> EpochHeader {
    EpochHeader::new(HeaderConfig::new(provider, model))
}

/// The default test header.
fn default_header() -> EpochHeader {
    header("test", "test")
}

/// Splits a log into the `(seq, event)` pairs a `ProjectionCache` folds.
fn pairs(events: &[SessionEvent]) -> Vec<(Seq, SessionEvent)> {
    events
        .iter()
        .enumerate()
        .map(|(index, event)| (u64::try_from(index).unwrap_or(u64::MAX), event.clone()))
        .collect()
}

/// Folds a log through the projection, starting from an empty state.
fn fold(events: &[SessionEvent]) -> MeasurementState {
    events
        .iter()
        .enumerate()
        .fold(MeasurementState::empty(), |state, (index, event)| {
            let seq = u64::try_from(index).unwrap_or(u64::MAX);
            let measurement = assistant_measurement(&state, event);
            TokenMeasurementProjection::apply_typed(&state, event, seq, measurement)
        })
}

/// The assistant measurement a caller would offer for this event, using the open step.
fn assistant_measurement(
    state: &MeasurementState,
    event: &SessionEvent,
) -> Option<AssistantMeasurement> {
    let SessionEvent::AssistantMessage { .. } = event else {
        return None;
    };
    state.step_start?;
    Some(AssistantMeasurement {
        // A local `assistant/message` carries no turn/step; the open step is the only evidence.
        coordinates: None,
        assistant_tokens: measurement_assistant_tokens(event),
        usage: measurement_usage(event),
    })
}

/// Prices one logged message the way the module's anchor does: blocks plus role framing.
fn priced_message(message: &girder_core::ChatMessage) -> u64 {
    if message.content.is_empty() {
        return 0;
    }
    girder_core::estimate_blocks(&message.content) + ROLE_OVERHEAD
}

/// The log `/measurement-projection.spec.ts` calls `prefix()`: a user message, a route record, an
/// open step.
///
/// Upstream's prefix also has a `request/header`; this crate's log has no such event, so the header
/// arrives through `MeasurementInput` instead and this fixture covers the *lifecycle* half. The
/// `request/context` record at index 1 is deliberately present and deliberately not a header: it is
/// route capacity, and the fold must record no header position for it.
fn header_log() -> Vec<SessionEvent> {
    vec![
        SessionEvent::user_text("m1", "question"),
        SessionEvent::RequestContext {
            provider: "test".to_owned(),
            model: "test".to_owned(),
            context_window: Some(200_000),
        },
        SessionEvent::StepStart { turn: 1, step: 1 },
    ]
}

/// A complete answered step: step/start, assistant message, step/end.
fn answered_step(turn: u64, step: u64, text: &str) -> [SessionEvent; 3] {
    [
        SessionEvent::StepStart { turn, step },
        SessionEvent::assistant_text(&format!("a{turn}-{step}"), text, "test"),
        SessionEvent::StepEnd { turn, step },
    ]
}

/// The rest of a step whose `step/start` is already open: assistant message, step/end.
///
/// Separate from [`answered_step`] because [`header_log`] leaves step 1/1 open — appending a second
/// `step/start` for it would be a *nested* step, which the fold correctly refuses.
fn closing_step(turn: u64, step: u64, text: &str) -> [SessionEvent; 2] {
    [
        SessionEvent::assistant_text(&format!("a{turn}-{step}"), text, "test"),
        SessionEvent::StepEnd { turn, step },
    ]
}

// ---- the projection's declared shape -----------------------------------------------------------

#[test]
fn declares_upstreams_key_and_state_version() {
    let projection = TokenMeasurementProjection;
    assert_eq!(projection.key(), "tokenMeasurement");
    assert_eq!(projection.key(), TOKEN_MEASUREMENT_KEY);
    // Upstream's `stateVersion: 2`. Pinned exactly rather than merely checked for positivity: a
    // checkpoint written under a different meaning must be refused, and the version is the only
    // thing that refuses it.
    assert_eq!(projection.state_version(), 2);
    assert_eq!(projection.state_version(), TOKEN_MEASUREMENT_STATE_VERSION);
    // Upstream declares no `summary`, which means true: four scalars, under 200 bytes.
    assert!(projection.in_summary());
}

#[test]
fn init_is_the_four_null_fields() {
    let init = TokenMeasurementProjection.init();
    assert_eq!(
        init,
        serde_json::json!({
            "headerSeq": serde_json::Value::Null,
            "stepStart": serde_json::Value::Null,
            "anchor": serde_json::Value::Null,
            "invalidSeq": serde_json::Value::Null,
        })
    );
    // And it round-trips back into the typed state, which is what a checkpoint restore does.
    assert_eq!(
        MeasurementState::from_state(&init).expect("init is a valid state"),
        MeasurementState::empty()
    );
}

#[test]
fn rejects_a_state_carrying_a_field_upstream_never_had() {
    // Upstream's own test: `{ ...init, surface: [] }` must be refused. A `surface` array is a state
    // written by a build that kept positional prices in the checkpoint, which is precisely the
    // corruption a silent accept would hide.
    let mut init = TokenMeasurementProjection.init();
    init["surface"] = serde_json::json!([]);
    let error = MeasurementState::from_state(&init).expect_err("must refuse an unknown field");
    assert_eq!(
        error,
        MeasurementStateError::Malformed {
            detail: "unknown field `surface`".to_owned()
        }
    );
}

#[test]
fn a_negative_sequence_is_unrepresentable_and_so_refused() {
    // Upstream rejects `headerSeq: -1` in its schema. Rust's `u64` discharges that rule by
    // construction, so the equivalent assertion is that the value cannot be read at all.
    let mut init = TokenMeasurementProjection.init();
    init["headerSeq"] = serde_json::json!(-1);
    assert!(MeasurementState::from_state(&init).is_err());
    // A non-object is refused for the same reason.
    assert!(MeasurementState::from_state(&serde_json::json!([])).is_err());
}

// ---- the fold's transitions --------------------------------------------------------------------

#[test]
fn a_header_position_and_an_open_step_are_folded_independently() {
    let state = fold(&header_log());
    assert_eq!(state.step_start, Some(StepCoordinate::new(1, 1)));
    assert_eq!(state.anchor, None);
    assert!(state.is_valid());
    // The `request/context` record at index 1 is *not* a header. This crate's `request/header`
    // equivalent is caller-supplied, so the fold records no position rather than claiming one.
    assert_eq!(state.header_seq, None);
}

#[test]
fn an_assistant_message_records_an_anchor_with_the_open_steps_identity() {
    let mut log = header_log();
    log.extend(closing_step(1, 1, "answer"));
    let state = fold(&log);
    // The step closed, so no step is open — but the anchor survives its step, because the next
    // request is priced *from* the last successful call.
    assert_eq!(state.step_start, None);
    let anchor = state
        .anchor
        .as_ref()
        .expect("the assistant message anchored");
    assert_eq!(
        anchor.header_seq, None,
        "no header was folded, so none is cited"
    );
    assert_eq!(anchor.usage, None, "the message carries no usage record");
    // The anchor price is the message's own heuristic price, not the surface's.
    let expected = log
        .iter()
        .map(|event| SurfaceEnvelope::log_only(event.clone(), 0))
        .find_map(|envelope| derive_event_message(&envelope).map(priced_message))
        .expect("the log holds a message");
    assert_eq!(anchor.assistant_tokens, expected);
    assert!(state.is_valid());
}

#[test]
fn the_anchor_keeps_the_latest_call_not_the_first() {
    let mut log = header_log();
    log.extend(closing_step(1, 1, "first"));
    log.extend(answered_step(1, 2, "second answer is longer"));
    let state = fold(&log);
    let anchor = state.anchor.as_ref().expect("anchored");
    let longer = log
        .iter()
        .map(|event| SurfaceEnvelope::log_only(event.clone(), 0))
        .filter_map(|envelope| derive_event_message(&envelope).map(priced_message))
        .max()
        .expect("messages exist");
    assert_eq!(anchor.assistant_tokens, longer);
}

#[test]
fn a_nested_step_start_is_recorded_and_then_absorbing() {
    let log = vec![
        SessionEvent::StepStart { turn: 1, step: 1 },
        // Nested: seq 1, while turn 1/step 1 is still open.
        SessionEvent::StepStart { turn: 1, step: 2 },
        // Valid on its own, and must nevertheless be ignored: the prefix is already failed.
        SessionEvent::StepEnd { turn: 1, step: 2 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
    ];
    let state = fold(&log);
    assert_eq!(state.invalid_seq, Some(1));
    // The frozen state is exactly the state *before* the failing event — the failure changed
    // nothing but the recorded position. In particular the step is still open, because the
    // absorbing rule stopped the `step/end` at index 3 from closing it.
    assert_eq!(
        state,
        MeasurementState {
            invalid_seq: Some(1),
            step_start: Some(StepCoordinate::new(1, 1)),
            ..MeasurementState::empty()
        }
    );
}

#[test]
fn an_assistant_message_outside_a_step_is_a_recorded_failure() {
    let log = vec![SessionEvent::assistant_text("a1", "no step open", "test")];
    let state = fold(&log);
    assert_eq!(state.invalid_seq, Some(0));
}

#[test]
fn an_assistant_message_naming_a_different_step_is_a_recorded_failure() {
    let log = [
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::assistant_text("a1", "answer", "test"),
    ];
    let mut state = MeasurementState::empty();
    // Fold the step open, then offer an assistant measurement that names another step.
    state = TokenMeasurementProjection::apply_typed(
        &state,
        &log[0],
        0,
        assistant_measurement(&state, &log[0]),
    );
    let offered = AssistantMeasurement {
        coordinates: Some((1, 2)),
        assistant_tokens: 10,
        usage: None,
    };
    state = TokenMeasurementProjection::apply_typed(&state, &log[1], 1, Some(offered));
    assert_eq!(state.invalid_seq, Some(1));
}

#[test]
fn an_unmatched_step_end_is_a_recorded_failure() {
    let log = vec![
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 2 },
    ];
    let state = fold(&log);
    assert_eq!(state.invalid_seq, Some(1));
    // And a `step/end` with no step open at all.
    let log = vec![SessionEvent::StepEnd { turn: 1, step: 1 }];
    assert_eq!(fold(&log).invalid_seq, Some(0));
}

// ---- the deferred diagnosis --------------------------------------------------------------------

#[test]
fn validate_reraises_the_original_error_at_the_recorded_position() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::StepStart { turn: 1, step: 1 }, 0),
        SurfaceEnvelope::log_only(SessionEvent::StepStart { turn: 1, step: 2 }, 1),
        SurfaceEnvelope::log_only(SessionEvent::StepEnd { turn: 1, step: 2 }, 2),
    ];
    let events: Vec<SessionEvent> = log.iter().map(|envelope| envelope.event.clone()).collect();
    let state = fold(&events);
    assert_eq!(state.invalid_seq, Some(1));
    // The specific error, not merely "something failed": a test asserting only `is_err()` would pass
    // against a fold that refused every log.
    let error = TokenMeasurementProjection::validate(&state, &log)
        .expect_err("the recorded position must re-raise");
    assert_eq!(
        error,
        MeasurementError::NestedStepStart {
            seq: 1,
            open_turn: 1,
            open_step: 1,
        }
    );
    assert_eq!(
        error.to_string(),
        "token meter: step/start at seq 1 arrived before turn 1/step 1 ended"
    );
}

#[test]
fn validate_is_silent_for_a_clean_prefix() {
    let mut events = header_log();
    events.extend(closing_step(1, 1, "answer"));
    let envelopes: Vec<SurfaceEnvelope> = events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            SurfaceEnvelope::log_only(event.clone(), u64::try_from(index).unwrap_or(u64::MAX))
        })
        .collect();
    let state = fold(&events);
    assert!(state.is_valid());
    assert_eq!(
        TokenMeasurementProjection::validate(&state, &envelopes),
        Ok(())
    );
}

#[test]
fn the_recorded_failure_matches_what_a_throwing_transition_would_say() {
    // The two halves must agree: `apply` freezes the state, and `advance_measurement` throws. If the
    // recorded position's error differed from the thrown one, the deferred diagnosis would be a
    // different diagnosis.
    let state = MeasurementState::empty();
    let event = SessionEvent::StepEnd { turn: 1, step: 1 };
    let thrown = advance_measurement(&state, &event, 7, None).expect_err("no step is open");
    assert_eq!(thrown, MeasurementError::UnmatchedStepEnd { seq: 7 });
    let folded = TokenMeasurementProjection::apply_typed(&state, &event, 7, None);
    assert_eq!(folded.invalid_seq, Some(7));
}

// ---- bounded state -----------------------------------------------------------------------------

#[test]
fn checkpoint_is_independent_of_message_count() {
    // Upstream's own test, ported with its thresholds: after 300 appended messages the state is
    // unchanged and its JSON is under 200 characters. A state that grew with the surface would pass
    // a "does it serialize" test and fail this one.
    let mut log = header_log();
    log.extend(closing_step(1, 1, "answer"));
    let initial = fold(&log);
    assert!(initial.anchor.is_some());

    for n in 0..300u64 {
        log.push(SessionEvent::user_text(
            &format!("m{n}"),
            "message".repeat(usize::try_from(n + 1).unwrap_or(1)),
        ));
    }
    let state = fold(&log);
    assert_eq!(
        state, initial,
        "appending surface messages must not move the state"
    );

    let json = state.to_state().expect("state is JSON");
    let rendered = serde_json::to_string(&json).expect("renders");
    assert!(
        rendered.len() < 200,
        "checkpoint must stay bounded, was {} bytes: {rendered}",
        rendered.len()
    );
}

#[test]
fn state_round_trips_through_json_exactly() {
    let mut log = header_log();
    log.extend(closing_step(1, 1, "answer"));
    let state = fold(&log);
    let json = state.to_state().expect("JSON");
    let reparsed: MeasurementState = serde_json::from_value(
        serde_json::from_str(&serde_json::to_string(&json).expect("string")).expect("value"),
    )
    .expect("typed");
    assert_eq!(reparsed, state);
}

#[test]
fn provider_usage_fields_round_trip_without_value_restrictions() {
    // Upstream's own test: `{ inputTokens: 1000, outputTokens: -1, providerDetail: { total: 7 } }`
    // round-trips, and the anchor keeps the usage. Rust's `u64` cannot hold `-1`, which is a
    // producer-level difference this port states rather than hides; what *is* asserted is that every
    // bucket the local log can carry survives a checkpoint and that a declared total is kept
    // verbatim.
    let usage = UsageFacts::new(1000, 10);
    let state = MeasurementState {
        header_seq: Some(2),
        step_start: None,
        anchor: Some(MeasurementAnchor {
            header_seq: Some(2),
            assistant_tokens: 0,
            usage: Some(usage),
        }),
        invalid_seq: None,
    };
    let json = state.to_state().expect("JSON");
    assert_eq!(
        json["anchor"]["usage"]["inputTokens"],
        serde_json::json!(1000)
    );
    assert_eq!(MeasurementState::from_state(&json).expect("valid"), state);
    assert_eq!(usage.usage_tokens(), 1010);
}

#[test]
fn absent_cache_buckets_stay_absent_rather_than_becoming_zero() {
    // The distinction a `u64` field would erase: "the provider reported no cache activity" and "the
    // log never recorded the bucket" are different, and only the second is true here.
    let facts = UsageFacts::new(5, 3);
    let json = serde_json::to_value(facts).expect("JSON");
    assert!(json.get("cacheReadTokens").is_none());
    assert!(json.get("cacheWriteTokens").is_none());
    assert_eq!(facts.usage_tokens(), 8);

    let reported = UsageFacts {
        input_tokens: 5,
        output_tokens: 3,
        cache_read_tokens: Some(11),
        cache_write_tokens: Some(7),
    };
    // The buckets are disjoint, so the total is their sum and not a double count of input.
    assert_eq!(reported.usage_tokens(), 26);
}

// ---- the baseline decision ---------------------------------------------------------------------

#[test]
fn no_header_and_no_surface_measures_exactly_zero() {
    // Upstream's third arm: nothing was sent, so the answer is a known zero rather than a guess.
    let input = MeasurementInput {
        header: None,
        surface_tokens: 0,
        heuristic_surface_tokens: 0,
        anchor_surface_tokens: 0,
        tools_tokens: 0,
        anchor_header: None,
    };
    let measurement = measure_measurement(&MeasurementState::empty(), &input);
    assert_eq!(
        measurement,
        Measurement {
            baseline: MeasurementBaseline::None,
            surface_delta_tokens: 0,
            total_tokens: 0,
            surface_tokens: 0,
        }
    );
    assert_eq!(measurement.baseline.kind(), "none");
    assert_eq!(measurement.baseline.tokens(), 0);
}

#[test]
fn an_unanchored_request_is_a_plain_heuristic_of_tools_and_surface() {
    let input = MeasurementInput {
        header: Some(default_header()),
        surface_tokens: 120,
        heuristic_surface_tokens: 100,
        anchor_surface_tokens: 0,
        tools_tokens: 30,
        anchor_header: None,
    };
    let measurement = measure_measurement(&MeasurementState::empty(), &input);
    assert_eq!(measurement.baseline.kind(), "estimated");
    assert_eq!(measurement.baseline.tokens(), 130);
    assert_eq!(measurement.total_tokens, 130);
    // The ambient surface total is reported as the route-priced figure, which may differ from the
    // heuristic: a routed image price is not the fixed-density one.
    assert_eq!(measurement.surface_tokens, 120);
    // No anchor, so no delta to report.
    assert_eq!(measurement.surface_delta_tokens, 0);
}

#[test]
fn a_matching_anchor_reports_the_signed_surface_delta() {
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 20,
            usage: None,
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(default_header()),
        surface_tokens: 200,
        heuristic_surface_tokens: 200,
        // The surface before the anchored assistant message.
        anchor_surface_tokens: 100,
        tools_tokens: 0,
        anchor_header: Some(default_header()),
    };
    let measurement = measure_measurement(&state, &input);
    // The anchored surface is 100 + the assistant's own 20.
    assert_eq!(measurement.baseline.tokens(), 120);
    assert_eq!(measurement.surface_delta_tokens, 80);
    assert_eq!(measurement.total_tokens, 200);
}

#[test]
fn a_compaction_below_the_anchor_reports_a_negative_delta() {
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 0,
            // 800 is at least the 500-token anchored surface, so the provider total is trusted.
            usage: Some(UsageFacts::new(800, 0)),
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(default_header()),
        surface_tokens: 10,
        heuristic_surface_tokens: 10,
        anchor_surface_tokens: 500,
        tools_tokens: 0,
        anchor_header: Some(default_header()),
    };
    let measurement = measure_measurement(&state, &input);
    // Signed, not clamped: a shrink is a real quantity and clamping it early would hide it.
    assert_eq!(measurement.surface_delta_tokens, -490);
    assert_eq!(measurement.baseline.kind(), "usage");
    assert_eq!(measurement.total_tokens, 310);
}

#[test]
fn the_anchored_total_cannot_go_negative_so_the_floor_is_defensive_only() {
    // A property worth stating rather than a coincidence: upstream's guard requires
    // `usage >= anchor_surface + assistant`, and the total is `usage + (surface - anchor_surface -
    // assistant)`, so it is always at least `surface`, which is non-negative. `Math.max(0, ...)` is
    // therefore unreachable in the anchored arm — it protects the arithmetic, not a real case. This
    // asserts the bound holds even for a surface compacted to nothing, which is the extreme.
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 0,
            usage: Some(UsageFacts::new(800, 0)),
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(default_header()),
        surface_tokens: 0,
        heuristic_surface_tokens: 0,
        anchor_surface_tokens: 500,
        tools_tokens: 0,
        anchor_header: Some(default_header()),
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.surface_delta_tokens, -500);
    // 800 - 500, and never below zero however hard the surface shrinks.
    assert_eq!(measurement.total_tokens, 300);

    // The estimated arm has the same property by construction: the total is `tools + surface`.
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 0,
            usage: Some(UsageFacts::new(1, 0)),
        }),
        ..MeasurementState::empty()
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.baseline.kind(), "estimated");
    assert_eq!(measurement.total_tokens, 0);
}

#[test]
fn a_truncated_usage_report_loses_to_the_heuristic() {
    // Upstream's `usageTokens(usage) >= estimatedAnchorTokens` guard. A provider total below the
    // content the model was sent cannot be the whole bill, so the heuristic is the honest baseline.
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 0,
            // Eleven tokens, against a 990-token anchored surface: a truncated report.
            usage: Some(UsageFacts::new(10, 1)),
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(default_header()),
        surface_tokens: 1_000,
        heuristic_surface_tokens: 1_000,
        anchor_surface_tokens: 990,
        tools_tokens: 0,
        anchor_header: Some(default_header()),
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.baseline.kind(), "estimated");
    assert_eq!(measurement.baseline.tokens(), 990);
    assert_eq!(measurement.surface_delta_tokens, 10);
    assert_eq!(measurement.total_tokens, 1_000);

    // The same state with a usage report large enough *is* preferred, which is the other direction
    // of the same rule.
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 0,
            // 1,010 is at least the 990-token anchored surface, so the provider total wins.
            usage: Some(UsageFacts::new(1_000, 10)),
        }),
        ..MeasurementState::empty()
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.baseline.kind(), "usage");
    assert_eq!(measurement.baseline.tokens(), 1_010);
    assert_eq!(measurement.surface_delta_tokens, 10);
    assert_eq!(measurement.total_tokens, 1_020);
}

#[test]
fn a_different_header_zeroes_the_delta_instead_of_comparing_them() {
    // A delta between two differently-priced surfaces is a number about nothing, so upstream reports
    // zero and falls back to the plain current-surface heuristic.
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 20,
            usage: Some(UsageFacts::new(5_000, 10)),
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(header("test", "next")),
        surface_tokens: 200,
        heuristic_surface_tokens: 180,
        anchor_surface_tokens: 100,
        tools_tokens: 40,
        anchor_header: Some(default_header()),
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.surface_delta_tokens, 0);
    assert_eq!(measurement.baseline.kind(), "estimated");
    // Tools plus the *current* heuristic surface, not the anchored one.
    assert_eq!(measurement.baseline.tokens(), 220);
    assert_eq!(measurement.total_tokens, 220);
    // The ambient route-priced total is still reported.
    assert_eq!(measurement.surface_tokens, 200);
}

#[test]
fn an_unknown_anchor_header_is_never_treated_as_a_match() {
    // "Unknown" cannot be proven the same route, so the conservative reading is a mismatch.
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: Some(9),
            assistant_tokens: 20,
            usage: Some(UsageFacts::new(5_000, 10)),
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(default_header()),
        surface_tokens: 200,
        heuristic_surface_tokens: 200,
        anchor_surface_tokens: 100,
        tools_tokens: 0,
        // The caller could not produce the anchored header.
        anchor_header: None,
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.surface_delta_tokens, 0);
    assert_eq!(measurement.baseline.kind(), "estimated");
}

#[test]
fn a_headerless_anchor_still_matches_a_headerless_request() {
    // Upstream's `optionalHeaderEquals`: when both sides are absent they are equal, so a headerless
    // estimate can still track later surface deltas. That is the one case where `None == None` is a
    // match, and it is deliberate rather than an accident of the comparison.
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 0,
            usage: None,
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: None,
        surface_tokens: 50,
        heuristic_surface_tokens: 50,
        anchor_surface_tokens: 20,
        tools_tokens: 0,
        anchor_header: None,
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.surface_delta_tokens, 30);
    assert_eq!(measurement.total_tokens, 50);
}

#[test]
fn the_anchored_estimate_counts_the_tool_catalogue() {
    // The catalogue is part of the sent prompt, so an anchored estimate that omitted it would
    // understate every step of a tool-using turn.
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: None,
            assistant_tokens: 10,
            usage: Some(UsageFacts::new(40, 0)),
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(default_header()),
        surface_tokens: 100,
        heuristic_surface_tokens: 100,
        anchor_surface_tokens: 30,
        tools_tokens: 25,
        anchor_header: Some(default_header()),
    };
    let measurement = measure_measurement(&state, &input);
    // 25 tools + 30 anchor surface + 10 assistant = 65, which the 40-token usage report does not
    // cover, so the estimate wins.
    assert_eq!(measurement.baseline.tokens(), 65);
    assert_eq!(measurement.baseline.kind(), "estimated");
}

// ---- integration with the crate's cache --------------------------------------------------------

#[test]
fn folds_through_the_projection_cache_and_survives_a_checkpoint() {
    // The real consumer: a `ProjectionCache` drives the fold, writes a row, and a later process
    // restores from it. This is what makes "bounded state" more than a claim about a struct.
    let cache = ProjectionCache::new(vec![token_measurement_definition()]).expect("registered");
    let mut events = header_log();
    events.extend(closing_step(1, 1, "answer"));

    for (seq, event) in pairs(&events) {
        cache.observe(&event, seq).expect("contiguous");
    }
    let checkpoint = cache.checkpoint();
    let row = checkpoint
        .row(TOKEN_MEASUREMENT_KEY)
        .expect("the definition wrote a row");
    assert_eq!(row.ver, TOKEN_MEASUREMENT_STATE_VERSION);
    assert_eq!(row.seq, u64::try_from(events.len()).unwrap_or(0) - 1);
    let restored = MeasurementState::from_state(&row.val).expect("the row is a valid state");
    assert_eq!(restored, fold(&events));

    // A restore over the whole log reproduces the same value, which is the property the cache exists
    // to provide.
    let snapshot = cache
        .restore(&checkpoint, &pairs(&events), 0)
        .expect("the window covers the log");
    assert_eq!(
        snapshot.get(TOKEN_MEASUREMENT_KEY),
        Some(&restored.to_state().expect("JSON"))
    );
}

#[test]
fn a_stale_version_row_is_refused_rather_than_read() {
    // The version is the only thing standing between a checkpoint written under a different meaning
    // and a silently wrong measurement. A row at version 1 must not seed this version-2 fold.
    let definition = token_measurement_definition();
    assert_eq!(definition.state_version(), 2);

    let cache = ProjectionCache::new(vec![definition]).expect("registered");
    let events = header_log();
    for (seq, event) in pairs(&events) {
        cache.observe(&event, seq).expect("contiguous");
    }
    let mut checkpoint = cache.checkpoint();
    let row = checkpoint
        .rows
        .get_mut(TOKEN_MEASUREMENT_KEY)
        .expect("a row");
    row.ver = 1;

    // A window that starts after the row cannot be folded without it, so the restore refuses.
    let suffix = pairs(&events)[2..].to_vec();
    assert!(cache.restore(&checkpoint, &suffix, 2).is_err());
}

#[test]
fn the_assistant_price_matches_the_shared_estimator() {
    // The anchor's price is the shared block estimator plus role framing, not a local
    // re-implementation: a change to `estimate_blocks` must move both, or the anchor and every other
    // price would diverge.
    let event = SessionEvent::assistant_text("a1", "answer text", "provider");
    let envelope = SurfaceEnvelope::log_only(event.clone(), 0);
    let message = derive_event_message(&envelope).expect("an assistant message derives a message");
    assert_eq!(
        priced_message(message),
        measurement_assistant_tokens(&event)
    );

    // An empty assistant message prices at zero rather than at the role overhead: there is no
    // message to frame. Upstream's `content.length === 0 ? 0` arm.
    let empty = SessionEvent::AssistantMessage {
        message: girder_core::ChatMessage::new(
            girder_core::MessageId::new("empty").expect("valid"),
            girder_core::ChatRole::Assistant,
            MessageSource::Model {
                provider: "p".to_owned(),
                model: "m".to_owned(),
                replay_state: None,
            },
            Vec::<ContentBlock>::new(),
        ),
    };
    assert_eq!(measurement_assistant_tokens(&empty), 0);
}

#[test]
fn a_tool_catalogue_header_is_a_different_route_from_a_tool_less_one() {
    // Header equality includes the ordered tool list, so a tool added mid-turn invalidates the
    // anchor: the request is not the same request. This is the interaction the anchor's
    // `header_seq` exists to detect, exercised through the caller-supplied header.
    let with_tools = default_header().with_tools(vec![ToolSchema {
        name: "read".to_owned(),
        description: "reads".to_owned(),
        parameters: serde_json::json!({"type": "object"}),
    }]);
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: Some(2),
            assistant_tokens: 10,
            usage: None,
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(with_tools),
        surface_tokens: 100,
        heuristic_surface_tokens: 100,
        anchor_surface_tokens: 50,
        tools_tokens: 60,
        anchor_header: Some(default_header()),
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.surface_delta_tokens, 0);
    // The catalogue is charged on the current request.
    assert_eq!(measurement.total_tokens, 160);
}

#[test]
fn a_header_defaults_record_alone_does_not_split_the_route() {
    // The counterpart: the adapter record is canonical metadata, not a different request when it is
    // empty, so an anchored measurement must still apply.
    let anchored = default_header().with_adapter_defaults(AdapterDefaults::none());
    let state = MeasurementState {
        anchor: Some(MeasurementAnchor {
            header_seq: Some(2),
            assistant_tokens: 0,
            usage: None,
        }),
        ..MeasurementState::empty()
    };
    let input = MeasurementInput {
        header: Some(default_header()),
        surface_tokens: 100,
        heuristic_surface_tokens: 100,
        anchor_surface_tokens: 40,
        tools_tokens: 0,
        anchor_header: Some(anchored),
    };
    let measurement = measure_measurement(&state, &input);
    assert_eq!(measurement.surface_delta_tokens, 60);
}

// ---- declared shape of the error type ----------------------------------------------------------

#[test]
fn the_error_messages_keep_upstreams_wording() {
    // A reader diffing against the reference should recognise every message.
    assert_eq!(
        MeasurementError::UnmatchedAssistantMessage { seq: 4 }.to_string(),
        "token meter: assistant/message at seq 4 has no matching step/start event"
    );
    assert_eq!(
        MeasurementError::UnmatchedStepEnd { seq: 5 }.to_string(),
        "token meter: step/end at seq 5 has no matching step/start event"
    );
    assert_eq!(
        MeasurementError::NestedStepStart {
            seq: 6,
            open_turn: 2,
            open_step: 3,
        }
        .to_string(),
        "token meter: step/start at seq 6 arrived before turn 2/step 3 ended"
    );
}
