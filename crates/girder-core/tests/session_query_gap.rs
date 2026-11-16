//! Tests for the structured half of session search (`session_query_gap`).
//!
//! Corresponds to upstream `packages/session-query/session-query/tests/search-helpers.spec.ts` and
//! `packages/session-query/session-query-sqlite/tests/query.spec.ts` for the filter, extraction,
//! snippet, normalization, fingerprint, and cursor behaviour this port covers, and to
//! `packages/core/session/src/surface.ts` through [`fold_surface`] for the placement the documents
//! carry.
//!
//! The properties that carry the weight, each tested from the side that can fail:
//!
//! - **A clause's value list is `OR`ed and the clauses are `AND`ed.** The empty list is therefore "no
//!   value is present" — not "no constraint" — which is the one way a filter can stop filtering.
//! - **The snippet is positioned on the match, and its ellipses are the only evidence it was cut.**
//!   The vectors are upstream's, character for character, because a snippet that is one character
//!   wide of the match is still a plausible-looking excerpt.
//! - **A cursor is bound to its request.** A token that decodes to an offset but belongs to another
//!   scope, instance, fingerprint, or generation must be refused; paging the wrong rows is worse
//!   than failing.
//! - **The surface classification is the fold's answer, not a second opinion.** A shadowed event is
//!   the case that distinguishes a filter over the log from a filter over the model's context.
//!
//! The module is included by path rather than through the crate root, so nothing here may reach for
//! `crate::`: the module's own imports resolve to `girder_core` root exports.

#[path = "../src/session_query_gap.rs"]
mod session_query_gap;

use girder_core::{
    ChatMessage, ContentBlock, SessionEvent, SurfaceEnvelope, ToolCallId, TurnEndReason,
};
use session_query_gap::{
    CursorScope, EventFilter, EventPredicate, FTS_HIGHLIGHT_END, FTS_HIGHLIGHT_START,
    LogicalSession, LogicalSessionHeader, MetadataFilter, QueryLimits, SQLITE_MAX_PAGE_LIMIT,
    SearchCursorBinding, SessionAvailability, SessionEventSearchDocument, SessionEventSurface,
    SessionFilter, SessionQueryErrorCode, SessionQueryRange, SessionTextFilter,
    build_session_event_records, build_session_event_search_documents, classify_surfaces,
    decode_search_cursor, encode_search_cursor, extract_session_event_text, filter_event_documents,
    filter_session_records, fingerprint_event_request, fingerprint_session_request, make_snippet,
    materialize_event_filters, materialize_metadata_filters, normalize_limit, normalize_query,
    quote_fts_data, sanitize_fts_text,
};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// A logical session with no working directory and no parent.
fn session(id: &str, created_at: f64, live: bool, persisted: bool) -> LogicalSession {
    LogicalSession {
        header: LogicalSessionHeader {
            id: id.to_owned(),
            created_at,
            cwd: None,
            parent_session: None,
        },
        live,
        persisted,
    }
}

/// A logical session with a working directory and a parent.
fn child(id: &str, created_at: f64, cwd: &str, parent: &str) -> LogicalSession {
    LogicalSession {
        header: LogicalSessionHeader {
            id: id.to_owned(),
            created_at,
            cwd: Some(cwd.to_owned()),
            parent_session: Some(parent.to_owned()),
        },
        live: true,
        persisted: true,
    }
}

/// A range with both bounds.
fn range(from: f64, to: f64) -> SessionQueryRange {
    SessionQueryRange::bounded(from, to).expect("the fixture range is valid")
}

/// A searchable document with an explicit surface and no timestamp.
fn document(
    seq: u64,
    type_name: &str,
    surface: SessionEventSurface,
    text: &str,
) -> SessionEventSearchDocument {
    SessionEventSearchDocument {
        record: session_query_gap::SessionEventRecord {
            session_id: "session".to_owned(),
            seq,
            type_name: type_name.to_owned(),
            time: None,
            surface,
        },
        text: text.to_owned(),
    }
}

/// A user message envelope that appends to the surface.
fn user(seq: u64, text: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::appended(SessionEvent::user_text(&format!("m{seq}"), text), seq)
}

/// A tool call envelope, which is log-only: a call is not a surface node in this crate.
fn call(seq: u64, id: &str, name: &str, arguments: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(
        SessionEvent::ToolCall {
            id: ToolCallId::new(id).expect("the fixture call id is legal"),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        },
        seq,
    )
}

/// A turn-end envelope carrying a failure.
fn failed_turn(seq: u64, turn: u64, message: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(
        SessionEvent::TurnEnd {
            turn,
            reason: TurnEndReason::Failed {
                message: message.to_owned(),
                code: "BOOM".to_owned(),
            },
        },
        seq,
    )
}

// ---------------------------------------------------------------------------------------------
// Range validation
// ---------------------------------------------------------------------------------------------

#[test]
fn an_empty_range_constrains_nothing_and_is_reported_as_unbounded() {
    let unbounded = SessionQueryRange::unbounded();
    assert!(unbounded.is_unbounded());
    assert_eq!(unbounded.from(), None);
    assert_eq!(unbounded.to(), None);
    // Upstream `buildSessionWhere([{ kind: 'created-at' }])` compiles to no predicate at all, which
    // is the SQL spelling of "every value is inside".
    assert!(unbounded.contains(-1.0));
    assert!(unbounded.contains(f64::MIN));
    assert!(unbounded.contains(f64::MAX));
}

#[test]
fn a_range_is_inclusive_on_both_bounds() {
    let interval = range(1.0, 2.0);
    assert!(!interval.contains(0.999));
    assert!(interval.contains(1.0));
    assert!(interval.contains(1.5));
    assert!(interval.contains(2.0));
    assert!(!interval.contains(2.001));
}

#[test]
fn an_inverted_or_non_finite_range_is_refused_at_construction() {
    // Upstream's `validateRange` raises inside the predicate builder; here the bound cannot be
    // written at all, which is the point of validating at construction.
    assert_eq!(
        SessionQueryRange::new(Some(2.0), Some(1.0))
            .expect_err("an inverted range must be refused")
            .code(),
        SessionQueryErrorCode::InvalidFilter
    );
    assert_eq!(
        SessionQueryRange::new(Some(f64::NAN), None)
            .expect_err("a NaN lower bound must be refused")
            .code(),
        SessionQueryErrorCode::InvalidFilter
    );
    assert_eq!(
        SessionQueryRange::new(None, Some(f64::INFINITY))
            .expect_err("an infinite upper bound must be refused")
            .code(),
        SessionQueryErrorCode::InvalidFilter
    );
}

#[test]
fn an_equal_pair_of_bounds_is_a_single_point_and_is_accepted() {
    let point = range(7.0, 7.0);
    assert!(point.contains(7.0));
    assert!(!point.contains(6.999));
    assert!(!point.contains(7.001));
}

#[test]
fn a_nan_value_satisfies_no_bound_and_is_accepted_by_no_filter() {
    // `>=` and `<=` are false against NaN, so a NaN cannot get past a bound — but an interval with no
    // bound at all has nothing to compare against and accepts it, which is upstream's behaviour too.
    // `accepts` is the predicate filters use, and it refuses a NaN outright.
    assert!(!range(0.0, f64::MAX).contains(f64::NAN));
    assert!(SessionQueryRange::unbounded().contains(f64::NAN));
    assert!(!SessionQueryRange::unbounded().accepts(f64::NAN));
    assert!(!range(0.0, 1.0).accepts(f64::NAN));
    assert!(!range(0.0, 1.0).accepts(f64::INFINITY));
}

#[test]
fn sequence_ranges_compare_in_the_recorded_domain() {
    let interval = range(2.0, 4.0);
    assert!(!interval.contains_seq(1));
    assert!(interval.contains_seq(2));
    assert!(interval.contains_seq(4));
    assert!(!interval.contains_seq(5));
}

// ---------------------------------------------------------------------------------------------
// Closed value sets
// ---------------------------------------------------------------------------------------------

#[test]
fn availability_and_surface_spellings_round_trip() {
    for availability in [SessionAvailability::Live, SessionAvailability::Persisted] {
        assert_eq!(
            SessionAvailability::from_wire(availability.as_str()),
            Some(availability)
        );
    }
    for surface in [
        SessionEventSurface::Current,
        SessionEventSurface::Shadowed,
        SessionEventSurface::LogOnly,
    ] {
        assert_eq!(
            SessionEventSurface::from_wire(surface.as_str()),
            Some(surface)
        );
    }
}

#[test]
fn an_unknown_spelling_is_refused_rather_than_defaulted() {
    // Upstream `assertAllowedValues` raises for these; silently ignoring the value would widen a
    // request the caller meant to narrow.
    assert_eq!(SessionAvailability::from_wire("future"), None);
    assert_eq!(SessionEventSurface::from_wire("future"), None);
    assert_eq!(SessionEventSurface::from_wire("log_only"), None);
}

// ---------------------------------------------------------------------------------------------
// Session filters
// ---------------------------------------------------------------------------------------------

#[test]
fn no_session_filter_keeps_every_record_in_order() {
    let sessions = [
        session("a", 1.0, true, false),
        session("b", 2.0, false, true),
    ];
    let kept = filter_session_records(&sessions, &[]);
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].header.id, "a");
    assert_eq!(kept[1].header.id, "b");
}

#[test]
fn session_clauses_are_anded_and_values_within_a_clause_are_ored() {
    let sessions = [
        child("a", 10.0, "/w", "root"),
        child("b", 20.0, "/w", "root"),
        child("c", 30.0, "/other", "root"),
    ];
    let kept = filter_session_records(
        &sessions,
        &[
            SessionFilter::Id(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]),
            SessionFilter::Cwd(vec![Some("/w".to_owned())]),
            SessionFilter::CreatedAt(range(5.0, 25.0)),
        ],
    );
    assert_eq!(
        kept.iter()
            .map(|entry| entry.header.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn an_empty_value_list_in_a_clause_matches_nothing() {
    // Upstream compiles this to the SQL literal `0`, which no row satisfies. The distinction from
    // "no clause at all" is what makes a facet UI able to disable every value and get no rows.
    let sessions = [session("a", 1.0, true, true)];
    assert!(filter_session_records(&sessions, &[SessionFilter::Id(Vec::new())]).is_empty());
    assert!(filter_session_records(&sessions, &[SessionFilter::Cwd(Vec::new())]).is_empty());
    assert!(filter_session_records(&sessions, &[SessionFilter::Parent(Vec::new())]).is_empty());
    assert!(
        filter_session_records(&sessions, &[SessionFilter::Availability(Vec::new())]).is_empty()
    );
}

#[test]
fn a_null_value_in_a_clause_matches_a_record_that_recorded_nothing() {
    let sessions = [
        session("plain", 1.0, true, true),
        child("kid", 2.0, "/w", "root"),
    ];
    assert_eq!(
        filter_session_records(&sessions, &[SessionFilter::Cwd(vec![None])])
            .iter()
            .map(|entry| entry.header.id.as_str())
            .collect::<Vec<_>>(),
        ["plain"]
    );
    assert_eq!(
        filter_session_records(&sessions, &[SessionFilter::Parent(vec![None])])
            .iter()
            .map(|entry| entry.header.id.as_str())
            .collect::<Vec<_>>(),
        ["plain"]
    );
}

#[test]
fn a_parent_clause_can_ask_for_a_named_parent_or_no_parent_at_all() {
    let sessions = [
        session("orphan", 1.0, true, true),
        child("kid", 2.0, "/w", "root"),
    ];
    let kept = filter_session_records(
        &sessions,
        &[SessionFilter::Parent(vec![Some("root".to_owned()), None])],
    );
    assert_eq!(kept.len(), 2);
}

#[test]
fn availability_is_a_disjunction_of_the_two_source_facts() {
    let sessions = [
        session("live-only", 1.0, true, false),
        session("persisted-only", 2.0, false, true),
        session("both", 3.0, true, true),
        session("neither", 4.0, false, false),
    ];
    let ids = |filters: &[SessionFilter]| {
        filter_session_records(&sessions, filters)
            .iter()
            .map(|entry| entry.header.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&[SessionFilter::Availability(vec![SessionAvailability::Live])]),
        ["live-only", "both"]
    );
    assert_eq!(
        ids(&[SessionFilter::Availability(vec![
            SessionAvailability::Persisted
        ])]),
        ["persisted-only", "both"]
    );
    assert_eq!(
        ids(&[SessionFilter::Availability(vec![
            SessionAvailability::Live,
            SessionAvailability::Persisted,
        ])]),
        ["live-only", "persisted-only", "both"]
    );
}

// ---------------------------------------------------------------------------------------------
// Text extraction
// ---------------------------------------------------------------------------------------------

#[test]
fn a_message_extracts_visible_text_and_nested_tool_calls_but_not_reasoning() {
    // Upstream's extraction fixture, reduced to the block kinds this crate carries: reasoning and
    // attachment handles are private or non-textual, and a future block kind is not searchable
    // merely because its payload holds strings.
    let message = ChatMessage::new(
        girder_core::MessageId::new("m").expect("the fixture message id is legal"),
        girder_core::ChatRole::Assistant,
        girder_core::MessageSource::Model {
            provider: "mock".to_owned(),
            model: "mock".to_owned(),
            replay_state: None,
        },
        vec![
            ContentBlock::Text {
                text: " visible ".to_owned(),
            },
            ContentBlock::Reasoning {
                text: "thought".to_owned(),
            },
            ContentBlock::ToolCall {
                id: ToolCallId::new("call").expect("the fixture call id is legal"),
                name: "read".to_owned(),
                arguments: "{\"path\":\"a\"}".to_owned(),
            },
            ContentBlock::Text {
                text: "nested".to_owned(),
            },
            ContentBlock::Image {
                attachment: "sha256:abc".to_owned(),
            },
        ],
    );
    assert_eq!(
        extract_session_event_text(&SessionEvent::AssistantMessage { message }),
        "visible\nread\n{\"path\":\"a\"}\nnested"
    );
}

#[test]
fn a_message_of_reasoning_only_has_no_searchable_text() {
    let message = ChatMessage::new(
        girder_core::MessageId::new("m").expect("the fixture message id is legal"),
        girder_core::ChatRole::Assistant,
        girder_core::MessageSource::Model {
            provider: "mock".to_owned(),
            model: "mock".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::Reasoning {
            text: "private thought".to_owned(),
        }],
    );
    assert_eq!(
        extract_session_event_text(&SessionEvent::AssistantMessage { message }),
        ""
    );
}

#[test]
fn a_tool_call_extracts_its_name_then_its_raw_arguments() {
    let event = SessionEvent::ToolCall {
        id: ToolCallId::new("call").expect("the fixture call id is legal"),
        name: "bash".to_owned(),
        arguments: "{\"cmd\":\"pwd\"}".to_owned(),
    };
    assert_eq!(
        extract_session_event_text(&event),
        "bash\n{\"cmd\":\"pwd\"}"
    );
}

#[test]
fn a_tool_result_extracts_only_its_text_content() {
    let event = SessionEvent::ToolResult {
        id: ToolCallId::new("call").expect("the fixture call id is legal"),
        content: vec![
            ContentBlock::Text {
                text: "failed".to_owned(),
            },
            ContentBlock::Image {
                attachment: "sha256:abc".to_owned(),
            },
        ],
        is_error: true,
    };
    assert_eq!(extract_session_event_text(&event), "failed");
}

#[test]
fn a_turn_outcome_is_searchable_except_a_completion() {
    let end = |reason| SessionEvent::TurnEnd { turn: 1, reason };
    assert_eq!(
        extract_session_event_text(&end(TurnEndReason::Completed)),
        ""
    );
    assert_eq!(
        extract_session_event_text(&end(TurnEndReason::Aborted {
            cause: "user".to_owned(),
        })),
        "aborted"
    );
    assert_eq!(
        extract_session_event_text(&end(TurnEndReason::Blocked)),
        "blocked"
    );
    assert_eq!(
        extract_session_event_text(&end(TurnEndReason::Budget)),
        "budget"
    );
    assert_eq!(
        extract_session_event_text(&end(TurnEndReason::Failed {
            message: "the model reached its output limit".to_owned(),
            code: "MAX_TOKENS".to_owned(),
        })),
        "error\nthe model reached its output limit"
    );
}

#[test]
fn boundaries_and_settings_and_prompts_contribute_no_text() {
    let events = [
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::ModelUsage {
            model: "mock".to_owned(),
            input_tokens: 1,
            output_tokens: 2,
        },
        SessionEvent::PermissionPreset {
            preset: "read-only".to_owned(),
        },
        SessionEvent::PlanMode { active: true },
        SessionEvent::SessionTitle {
            title: "a title".to_owned(),
            message_seqs: vec![0],
            source: "human".to_owned(),
        },
        SessionEvent::DeveloperMessage {
            turn: 1,
            step: 1,
            message: ChatMessage::user(
                girder_core::MessageId::new("d").expect("the fixture message id is legal"),
                "tool added",
            ),
        },
        SessionEvent::SystemMessage {
            turn: 1,
            step: 1,
            message: ChatMessage::user(
                girder_core::MessageId::new("s").expect("the fixture message id is legal"),
                "you are helpful",
            ),
        },
    ];
    for event in &events {
        assert_eq!(
            extract_session_event_text(event),
            "",
            "`{}` must not be searchable",
            event.type_name()
        );
    }
}

#[test]
fn the_extraction_contract_differs_from_the_generic_searchable_text_on_purpose() {
    // `searchable_text` is the projection the shipped search tool uses; it includes the prompts and
    // the title because it answers "what text does this event hold". This module answers "what may a
    // search find", and upstream excludes them. Both behaviours are pinned so the difference is a
    // decision rather than a drift.
    let system = SessionEvent::SystemMessage {
        turn: 1,
        step: 1,
        message: ChatMessage::user(
            girder_core::MessageId::new("s").expect("the fixture message id is legal"),
            "you are helpful",
        ),
    };
    assert_eq!(girder_core::searchable_text(&system), "you are helpful");
    assert_eq!(extract_session_event_text(&system), "");
}

// ---------------------------------------------------------------------------------------------
// Surface classification and documents
// ---------------------------------------------------------------------------------------------

#[test]
fn surface_classification_names_current_shadowed_and_log_only() {
    // A replacement shadows the user message it covers, so the message stays in the log as
    // `shadowed` — the distinction a filter over the model's context needs.
    let summary = SessionEvent::user_text("m9", "summary");
    let log = [
        user(0, "first"),
        call(1, "call", "read", "{}"),
        // A system message at node zero is protected, so the replacement targets node 0 with a
        // system message of its own; nodes 0 is the only surface node at this point.
        SurfaceEnvelope::replacing(
            SessionEvent::SystemMessage {
                turn: 1,
                step: 1,
                message: ChatMessage::user(
                    girder_core::MessageId::new("s").expect("the fixture message id is legal"),
                    "summary",
                ),
            },
            2,
            0,
            0,
            vec![0],
        ),
        SurfaceEnvelope::appended(summary, 3),
    ];
    let surfaces = classify_surfaces(&log).expect("the fixture log folds");
    assert_eq!(
        surfaces,
        [
            SessionEventSurface::Shadowed,
            SessionEventSurface::LogOnly,
            SessionEventSurface::Current,
            SessionEventSurface::Current,
        ]
    );
}

#[test]
fn a_log_that_does_not_fold_is_an_invalid_surface_failure() {
    // Sequence 2 where 1 was expected: upstream wraps the same fold failure in
    // `SESSION_QUERY_INVALID_SURFACE` rather than reporting a misleading placement for the events
    // after the gap.
    let log = [user(0, "first"), user(2, "third")];
    assert_eq!(
        classify_surfaces(&log)
            .expect_err("a gap in the log must be refused")
            .code(),
        SessionQueryErrorCode::InvalidSurface
    );
}

#[test]
fn records_carry_the_supplied_time_and_the_folded_placement() {
    let log = [user(0, "first"), call(1, "call", "read", "{}")];
    let records = build_session_event_records("s", &log, |seq| {
        Some(100.0 + f64::from(u32::try_from(seq).expect("the fixture log is small")))
    })
    .expect("the fixture log folds");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].session_id, "s");
    assert_eq!(records[0].seq, 0);
    assert_eq!(records[0].type_name, "user/message");
    assert_eq!(records[0].time, Some(100.0));
    assert_eq!(records[0].surface, SessionEventSurface::Current);
    assert_eq!(records[1].type_name, "tool/call");
    assert_eq!(records[1].time, Some(101.0));
    assert_eq!(records[1].surface, SessionEventSurface::LogOnly);
}

#[test]
fn a_caller_with_no_timestamps_reports_none_rather_than_a_fabricated_zero() {
    // A zero would sit inside every `from`-bounded time filter, which turns "I have no timestamps"
    // into "everything happened at the epoch".
    let log = [user(0, "first")];
    let records = build_session_event_records("s", &log, |_| None).expect("the fixture log folds");
    assert_eq!(records[0].time, None);
}

#[test]
fn documents_omit_events_with_no_searchable_text() {
    let log = [
        user(0, "hello"),
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 1),
        call(2, "call", "bash", "{\"cmd\":\"pwd\"}"),
        failed_turn(3, 1, "it broke"),
    ];
    let documents =
        build_session_event_search_documents("s", &log, |_| None).expect("the fixture log folds");
    assert_eq!(
        documents
            .iter()
            .map(SessionEventSearchDocument::seq)
            .collect::<Vec<_>>(),
        [0, 2, 3]
    );
    assert_eq!(documents[0].text(), "hello");
    assert_eq!(documents[0].session_id(), "s");
    assert_eq!(documents[1].type_name(), "tool/call");
    assert_eq!(documents[2].text(), "error\nit broke");
    assert_eq!(documents[2].surface(), SessionEventSurface::LogOnly);
}

// ---------------------------------------------------------------------------------------------
// Event filters
// ---------------------------------------------------------------------------------------------

#[test]
fn no_event_predicate_keeps_every_document() {
    let documents = [
        document(0, "user/message", SessionEventSurface::Current, "alpha"),
        document(1, "tool/call", SessionEventSurface::LogOnly, "beta"),
    ];
    assert_eq!(filter_event_documents(&documents, &[]).len(), 2);
}

#[test]
fn event_clauses_are_anded_and_values_within_a_clause_are_ored() {
    let documents = [
        document(0, "user/message", SessionEventSurface::Current, "alpha"),
        document(1, "tool/call", SessionEventSurface::Current, "beta"),
        document(
            2,
            "tool/result",
            SessionEventSurface::Shadowed,
            "beta again",
        ),
        document(3, "user/message", SessionEventSurface::LogOnly, "gamma"),
    ];
    let predicates = materialize_metadata_filters(&[
        MetadataFilter::Type(vec!["tool/call".to_owned(), "tool/result".to_owned()]),
        MetadataFilter::Surface(vec![SessionEventSurface::Current]),
        MetadataFilter::Seq(range(1.0, 3.0)),
    ]);
    assert_eq!(
        filter_event_documents(&documents, &predicates)
            .iter()
            .map(|document| document.seq())
            .collect::<Vec<_>>(),
        [1]
    );
}

#[test]
fn a_time_bound_does_not_match_a_document_with_no_timestamp() {
    let unknown = document(0, "user/message", SessionEventSurface::Current, "alpha");
    let mut known = document(1, "user/message", SessionEventSurface::Current, "beta");
    known.record.time = Some(50.0);
    let documents = [unknown.clone(), known];
    let predicates = materialize_metadata_filters(&[MetadataFilter::Time(range(0.0, 100.0))]);
    assert_eq!(
        filter_event_documents(&documents, &predicates)
            .iter()
            .map(|document| document.seq())
            .collect::<Vec<_>>(),
        [1]
    );
    // A bound that cannot be checked is not evidence that it holds, so an absent timestamp fails
    // even an unbounded interval.
    let unbounded =
        materialize_metadata_filters(&[MetadataFilter::Time(SessionQueryRange::unbounded())]);
    assert!(filter_event_documents(&[unknown], &unbounded).is_empty());
}

#[test]
fn a_text_clause_matches_case_insensitively_across_flexible_whitespace() {
    let documents = [document(
        0,
        "user/message",
        SessionEventSurface::Current,
        "the CAFÉ  was\n\nclosed",
    )];
    let predicates = materialize_event_filters(&[EventFilter::Text("café was closed".to_owned())])
        .expect("the fixture query compiles");
    assert_eq!(filter_event_documents(&documents, &predicates).len(), 1);

    let too_far = materialize_event_filters(&[EventFilter::Text("café and closed".to_owned())])
        .expect("the fixture query compiles");
    assert!(filter_event_documents(&documents, &too_far).is_empty());
}

#[test]
fn a_text_clause_is_data_and_not_a_pattern() {
    // Upstream compiles the literal after escaping every regex metacharacter, so `a.*b` searches
    // for those five characters. A scan over literal terms gives the same answer by construction.
    let documents = [
        document(0, "user/message", SessionEventSurface::Current, "axxb"),
        document(1, "user/message", SessionEventSurface::Current, "a.*b"),
    ];
    let predicates = materialize_event_filters(&[EventFilter::Text("a.*b".to_owned())])
        .expect("the fixture query compiles");
    assert_eq!(
        filter_event_documents(&documents, &predicates)
            .iter()
            .map(|document| document.seq())
            .collect::<Vec<_>>(),
        [1]
    );
}

#[test]
fn an_empty_text_clause_is_refused_rather_than_matching_everything() {
    assert_eq!(
        materialize_event_filters(&[EventFilter::Text(" \n\t ".to_owned())])
            .expect_err("a blank text clause must be refused")
            .code(),
        SessionQueryErrorCode::InvalidFilter
    );
    assert_eq!(
        SessionTextFilter::compile("")
            .expect_err("an empty text clause must be refused")
            .code(),
        SessionQueryErrorCode::InvalidFilter
    );
}

#[test]
fn a_text_clause_backtracks_to_a_later_occurrence_of_the_first_term() {
    // A single greedy pass would consume the first `a` and then fail; `a b` must still match.
    let filter = SessionTextFilter::compile("a b").expect("the fixture query compiles");
    assert!(filter.matches("a xa b"));
    assert!(filter.matches("a b a"));
    // `b` before `a` is a different order, and order is what a conjunction of terms in a query means.
    assert!(!filter.matches("b a"));
    assert_eq!(filter.terms(), vec!["a".to_owned(), "b".to_owned()]);
}

#[test]
fn a_two_term_clause_requires_whitespace_between_its_terms() {
    // Upstream joins the terms with `\s+`, so `ab` is not a match for `a b`.
    let filter = SessionTextFilter::compile("a b").expect("the fixture query compiles");
    assert!(!filter.matches("ab"));
    assert!(filter.matches("a\tb"));
    assert!(filter.matches("x a  b y"));
}

// ---------------------------------------------------------------------------------------------
// Snippet, marker hygiene, and query normalization
// ---------------------------------------------------------------------------------------------

#[test]
fn a_snippet_normalizes_whitespace_and_bounds_by_character() {
    // Upstream's vectors, verbatim.
    assert_eq!(make_snippet("  short\ntext  ", 20), "short text");
    assert_eq!(make_snippet("abcdefghij", 5), "abcd…");
    assert_eq!(
        make_snippet(
            &format!("abcde{FTS_HIGHLIGHT_START}f{FTS_HIGHLIGHT_END}"),
            1
        ),
        "…"
    );
    assert_eq!(
        make_snippet(
            &format!("ab{FTS_HIGHLIGHT_START}c{FTS_HIGHLIGHT_END}defghij"),
            5
        ),
        "…bcd…"
    );
    assert_eq!(
        make_snippet(
            &format!("ab{FTS_HIGHLIGHT_START}c{FTS_HIGHLIGHT_END}defghij"),
            3
        ),
        "…c…"
    );
    assert_eq!(
        make_snippet(
            &format!("abcde{FTS_HIGHLIGHT_START}f{FTS_HIGHLIGHT_END}"),
            2
        ),
        "…f"
    );
    assert_eq!(
        make_snippet(
            &format!("abcde{FTS_HIGHLIGHT_START}f{FTS_HIGHLIGHT_END}"),
            5
        ),
        "…cdef"
    );
    assert_eq!(
        make_snippet(
            &format!("  x—{FTS_HIGHLIGHT_START}café{FTS_HIGHLIGHT_END}\n y  "),
            20
        ),
        "x—café y"
    );
}

#[test]
fn a_snippet_of_a_document_that_fits_is_returned_whole_without_an_ellipsis() {
    // The absence of an ellipsis is a claim that nothing was cut, so a document that fits must not
    // carry one.
    assert_eq!(make_snippet("exactly ten", 20), "exactly ten");
    assert_eq!(make_snippet("", 20), "");
}

#[test]
fn a_snippet_with_no_room_shows_nothing_rather_than_breaking_the_bound() {
    assert_eq!(make_snippet("abcdef", 0), "");
}

#[test]
fn a_snippet_cut_at_the_end_keeps_the_marker_free_content() {
    // The window moves to the tail and drops the trailing ellipsis, because there is nothing after
    // the excerpt to mark.
    assert_eq!(make_snippet("abcdefghij", 8), "abcdefg…");
    let tail = make_snippet("0123456789", 10);
    assert_eq!(tail, "0123456789");
}

#[test]
fn snippet_positioning_counts_characters_and_not_bytes() {
    // A multi-byte document cut on a byte offset would hold a replacement character.
    let text = format!("ééé{FTS_HIGHLIGHT_START}é{FTS_HIGHLIGHT_END}éééééé");
    let snippet = make_snippet(&text, 4);
    assert_eq!(snippet.chars().count(), 4);
    assert!(!snippet.contains('\u{FFFD}'));
}

#[test]
fn reserved_markers_and_nul_are_replaced_before_text_is_indexed() {
    let hostile = format!("a\0b{FTS_HIGHLIGHT_START}c{FTS_HIGHLIGHT_END}d");
    assert_eq!(sanitize_fts_text(&hostile), "a\u{FFFD}b\u{FFFD}c\u{FFFD}d");
    // Ordinary text is untouched.
    assert_eq!(sanitize_fts_text("plain café"), "plain café");
}

#[test]
fn a_caller_query_is_quoted_so_its_syntax_stays_inert() {
    let quoted = quote_fts_data("foo\" OR bar");
    assert_eq!(quoted, "\"foo\"\" OR bar\"");
    assert_eq!(quote_fts_data("plain"), "\"plain\"");
}

#[test]
fn a_query_is_trimmed_and_its_whitespace_collapsed() {
    assert_eq!(
        normalize_query("  alpha\n beta  ").expect("the fixture query is valid"),
        "alpha beta"
    );
    assert_eq!(
        normalize_query("single").expect("the fixture query is valid"),
        "single"
    );
}

#[test]
fn an_empty_or_nul_bearing_query_is_refused() {
    assert_eq!(
        normalize_query(" \n\t ")
            .expect_err("a blank query must be refused")
            .code(),
        SessionQueryErrorCode::InvalidQuery
    );
    assert_eq!(
        normalize_query("a\0b")
            .expect_err("a NUL-bearing query must be refused")
            .code(),
        SessionQueryErrorCode::InvalidQuery
    );
}

#[test]
fn a_query_cannot_carry_a_reserved_marker_into_a_snippet() {
    // The marker is the only way a snippet knows where the match began; a query that held one could
    // forge that position.
    let normalized = normalize_query(&format!("a{FTS_HIGHLIGHT_START}b"))
        .expect("the marker is removed rather than refused");
    assert_eq!(normalized, "a\u{FFFD}b");
}

#[test]
fn a_limit_defaults_and_is_bounded_by_the_configured_maximum() {
    let limits = QueryLimits {
        default_limit: 20,
        max_limit: 100,
    };
    assert_eq!(
        normalize_limit(None, limits).expect("the default is valid"),
        20
    );
    assert_eq!(normalize_limit(Some(1), limits).expect("one is valid"), 1);
    assert_eq!(
        normalize_limit(Some(100), limits).expect("the maximum is valid"),
        100
    );
    for refused in [Some(0), Some(101)] {
        assert_eq!(
            normalize_limit(refused, limits)
                .expect_err("a limit outside the interval must be refused")
                .code(),
            SessionQueryErrorCode::InvalidLimit
        );
    }
}

#[test]
fn a_limit_above_the_engine_ceiling_is_refused_even_when_the_configuration_allows_it() {
    let limits = QueryLimits {
        default_limit: 1,
        max_limit: u64::MAX,
    };
    assert_eq!(
        normalize_limit(Some(SQLITE_MAX_PAGE_LIMIT + 1), limits)
            .expect_err("the engine ceiling binds")
            .code(),
        SessionQueryErrorCode::InvalidLimit
    );
    assert_eq!(
        normalize_limit(Some(SQLITE_MAX_PAGE_LIMIT), limits).expect("the ceiling itself is valid"),
        SQLITE_MAX_PAGE_LIMIT
    );
}

// ---------------------------------------------------------------------------------------------
// Request fingerprints
// ---------------------------------------------------------------------------------------------

#[test]
fn fingerprints_ignore_value_order_duplication_and_clause_order() {
    let first = fingerprint_session_request(
        "needle",
        &[
            SessionFilter::Availability(vec![SessionAvailability::Live]),
            SessionFilter::Id(vec!["a".to_owned(), "b".to_owned()]),
        ],
        &[MetadataFilter::Surface(vec![SessionEventSurface::Current])],
        2,
    );
    let second = fingerprint_session_request(
        "needle",
        &[
            SessionFilter::Id(vec!["b".to_owned(), "a".to_owned(), "a".to_owned()]),
            SessionFilter::Availability(vec![SessionAvailability::Live]),
        ],
        &[MetadataFilter::Surface(vec![SessionEventSurface::Current])],
        2,
    );
    assert_eq!(first, second);
}

#[test]
fn fingerprints_separate_the_things_that_change_the_answer() {
    let base = fingerprint_session_request("needle", &[], &[], 2);
    assert_ne!(base, fingerprint_session_request("needle ", &[], &[], 2));
    assert_ne!(base, fingerprint_session_request("needle", &[], &[], 3));
    assert_ne!(
        base,
        fingerprint_session_request(
            "needle",
            &[SessionFilter::Availability(vec![SessionAvailability::Live])],
            &[],
            2
        )
    );
    assert_ne!(
        base,
        fingerprint_session_request("needle", &[], &[MetadataFilter::Seq(range(0.0, 1.0))], 2)
    );
}

#[test]
fn an_absent_bound_and_a_present_one_are_different_requests() {
    let both = fingerprint_session_request(
        "needle",
        &[SessionFilter::CreatedAt(range(0.0, 1.0))],
        &[],
        2,
    );
    let lower_only = fingerprint_session_request(
        "needle",
        &[SessionFilter::CreatedAt(
            SessionQueryRange::new(Some(0.0), None).expect("the fixture range is valid"),
        )],
        &[],
        2,
    );
    let unbounded = fingerprint_session_request(
        "needle",
        &[SessionFilter::CreatedAt(SessionQueryRange::unbounded())],
        &[],
        2,
    );
    assert_ne!(both, lower_only);
    assert_ne!(both, unbounded);
    assert_ne!(lower_only, unbounded);
}

#[test]
fn the_two_scopes_never_share_a_fingerprint() {
    let sessions = fingerprint_session_request("needle", &[], &[], 2);
    let events = fingerprint_event_request("s", "needle", &[], 2);
    assert_ne!(sessions, events);
    assert_ne!(events, fingerprint_event_request("other", "needle", &[], 2));
}

#[test]
fn an_event_fingerprint_canonicalizes_its_clauses_too() {
    let first = fingerprint_event_request(
        "s",
        "needle",
        &[
            MetadataFilter::Surface(vec![
                SessionEventSurface::Current,
                SessionEventSurface::Shadowed,
            ]),
            MetadataFilter::Seq(SessionQueryRange::unbounded()),
        ],
        5,
    );
    let second = fingerprint_event_request(
        "s",
        "needle",
        &[
            MetadataFilter::Seq(SessionQueryRange::unbounded()),
            MetadataFilter::Surface(vec![
                SessionEventSurface::Shadowed,
                SessionEventSurface::Current,
                SessionEventSurface::Shadowed,
            ]),
        ],
        5,
    );
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------------------------
// Cursors
// ---------------------------------------------------------------------------------------------

/// A cursor binding for the cross-session scope.
fn binding(generation: &str) -> SearchCursorBinding {
    SearchCursorBinding {
        instance: "instance-a".to_owned(),
        scope: CursorScope::Sessions,
        fingerprint: "fp".to_owned(),
        generation: generation.to_owned(),
    }
}

#[test]
fn a_cursor_round_trips_to_its_offset() {
    let binding = binding("g1");
    let cursor = encode_search_cursor(40, &binding).expect("the fixture payload encodes");
    assert_eq!(
        decode_search_cursor(&cursor, &binding).expect("the cursor decodes"),
        40
    );
    let start = encode_search_cursor(0, &binding).expect("the fixture payload encodes");
    assert_eq!(
        decode_search_cursor(&start, &binding).expect("the cursor decodes"),
        0
    );
}

#[test]
fn a_cursor_is_url_safe_and_padding_free() {
    let cursor = encode_search_cursor(1234, &binding("g1")).expect("the fixture payload encodes");
    assert!(
        cursor
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'),
        "`{cursor}` must hold only base64url characters"
    );
    assert!(!cursor.contains('='));
}

#[test]
fn a_cursor_is_refused_by_another_scope_instance_or_fingerprint() {
    let cursor = encode_search_cursor(40, &binding("g1")).expect("the fixture payload encodes");
    let mut other_scope = binding("g1");
    other_scope.scope = CursorScope::Events;
    let mut other_instance = binding("g1");
    other_instance.instance = "instance-b".to_owned();
    let mut other_fingerprint = binding("g1");
    other_fingerprint.fingerprint = "other".to_owned();
    for wrong in [other_scope, other_instance, other_fingerprint] {
        assert_eq!(
            decode_search_cursor(&cursor, &wrong)
                .expect_err("a foreign cursor must be refused")
                .code(),
            SessionQueryErrorCode::InvalidCursor
        );
    }
}

#[test]
fn a_cursor_from_a_moved_corpus_is_stale_and_not_malformed() {
    // The remedy differs: stale means "restart the search", malformed means "fix the request".
    let cursor = encode_search_cursor(40, &binding("g1")).expect("the fixture payload encodes");
    assert_eq!(
        decode_search_cursor(&cursor, &binding("g2"))
            .expect_err("a cursor from an older generation must be refused")
            .code(),
        SessionQueryErrorCode::StaleCursor
    );
}

#[test]
fn text_that_is_not_a_cursor_is_refused_as_invalid() {
    let binding = binding("g1");
    for bad in ["", "not base64url!", "====", "AAAA", "eyJ2ZXJzaW9uIjo5fQ"] {
        assert_eq!(
            decode_search_cursor(bad, &binding)
                .expect_err("non-cursor text must be refused")
                .code(),
            SessionQueryErrorCode::InvalidCursor,
            "`{bad}` must be refused as an invalid cursor"
        );
    }
}

#[test]
fn a_cursor_with_a_foreign_version_is_refused() {
    // Upstream `decoded.version !== 1`. A v2 payload here is a v1 payload with the version byte
    // edited, which is exactly what a downgrade looks like.
    let binding = binding("g1");
    let payload = format!(
        "{{\"version\":9,\"instance\":\"{}\",\"scope\":\"sessions\",\"fingerprint\":\"fp\",\"generation\":\"g1\",\"offset\":3}}",
        binding.instance
    );
    let cursor = base64url_of(payload.as_bytes());
    assert_eq!(
        decode_search_cursor(&cursor, &binding)
            .expect_err("a foreign version must be refused")
            .code(),
        SessionQueryErrorCode::InvalidCursor
    );
}

/// Encodes bytes with this crate's own encoder, reached through a cursor round trip.
///
/// The encoder is private to the module under test, so the test builds a token the only way it can:
/// by asking the module for one and then editing the decoded payload. This helper re-implements the
/// final step — the alphabet — because a test that could not construct a malformed token could not
/// test the refusal.
fn base64url_of(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk.first().copied().unwrap_or(0));
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let packed = (first << 16) | (second << 8) | third;
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index > chunk.len() {
                break;
            }
            let sextet = usize::try_from((packed >> shift) & 0x3f).expect("a sextet is under 64");
            encoded.push(char::from(ALPHABET[sextet]));
        }
    }
    encoded
}

// ---------------------------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------------------------

#[test]
fn every_taxonomy_member_round_trips_through_its_wire_spelling() {
    let codes = [
        SessionQueryErrorCode::Aborted,
        SessionQueryErrorCode::CorruptSession,
        SessionQueryErrorCode::EventNotFound,
        SessionQueryErrorCode::IndexFailed,
        SessionQueryErrorCode::InvalidConfig,
        SessionQueryErrorCode::InvalidCursor,
        SessionQueryErrorCode::InvalidFilter,
        SessionQueryErrorCode::InvalidLimit,
        SessionQueryErrorCode::InvalidQuery,
        SessionQueryErrorCode::InvalidLineage,
        SessionQueryErrorCode::InvalidSurface,
        SessionQueryErrorCode::InvalidWindow,
        SessionQueryErrorCode::PersistenceFailed,
        SessionQueryErrorCode::SearchDisabled,
        SessionQueryErrorCode::SessionNotFound,
        SessionQueryErrorCode::StaleCursor,
        SessionQueryErrorCode::SourceConflict,
    ];
    for code in codes {
        assert_eq!(SessionQueryErrorCode::from_wire(code.as_str()), Some(code));
        assert!(code.as_str().starts_with("SESSION_QUERY_"));
    }
    assert_eq!(
        SessionQueryErrorCode::from_wire("SESSION_QUERY_FUTURE"),
        None
    );
}

#[test]
fn a_failure_renders_its_code_and_its_detail() {
    let failure = SessionTextFilter::compile("  ").expect_err("blank text is refused");
    assert_eq!(failure.code(), SessionQueryErrorCode::InvalidFilter);
    assert_eq!(
        failure.detail(),
        "session text filter must contain non-whitespace text"
    );
    let rendered = failure.to_string();
    assert!(rendered.contains("SESSION_QUERY_INVALID_FILTER"));
    assert!(rendered.contains("non-whitespace"));
}

#[test]
fn predicates_are_comparable_so_a_normalized_request_can_be_cached() {
    let compiled = materialize_metadata_filters(&[MetadataFilter::Seq(range(0.0, 2.0))]);
    assert_eq!(compiled, vec![EventPredicate::Seq(range(0.0, 2.0))]);
    assert_eq!(
        materialize_event_filters(&[EventFilter::Text("a b".to_owned())])
            .expect("the fixture query compiles"),
        vec![EventPredicate::Text(
            SessionTextFilter::compile("a b").expect("the fixture query compiles")
        )]
    );
}
