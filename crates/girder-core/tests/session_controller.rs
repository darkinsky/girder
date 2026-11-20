//! Tests for the history-read core of upstream `packages/api/session-controller`.
//!
//! Corresponds to upstream `packages/api/session-controller/tests/transport.host.spec.ts`'s page-cut
//! and cursor-validation cases, `history-records.client.spec.ts` (the record wrappers), and
//! `lineage.client.spec.ts` (the lineage flattening). The client `mergeOrderedBaseline` case comes
//! from `manager.client.spec.ts`'s baseline merging.
//!
//! # What each property protects
//!
//! - **The budget counts messages, not records.** A page of `max_messages` messages contains more
//!   records than that, because tool calls and step markers ride between them. Counting records
//!   yields a page that is too short and cuts mid-turn.
//! - **A grouped message moves the cut to its sources.** A retry re-derives a message from earlier
//!   events; cutting at the message's own seq leaves those sources on neither page.
//! - **`has_more` is about the cut.** A page that stops at seq 0 is complete; one that stops later is
//!   not, even with few records. Reporting `has_more` from `records.len()` strands the log's tail.
//! - **A turn window is a stop rule.** It may end a page at a `turn/start` once both minima are met,
//!   and it never overrides `max_messages`.
//! - **The empty cursor is not seq 0.** Upstream's `-1` means "read nothing"; `0` is a real event.
//!   Conflating them turns an empty session into a read of its first event.
//! - **An unreadable address is refused, not redirected.** A subagent child this build cannot resolve
//!   must not silently read the parent's log, which would hand a caller another conversation.
//! - **Lineage never drops or duplicates a row.** Orphans degrade to roots, cycles fail soft, and
//!   ordinary forks stay at root level.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}` — including the
//! ones the module under test makes, which is why the module itself spells them that way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]
#![allow(
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "a test that indexes its own fixture is clearer than one that re-derives bounds"
)]

#[path = "../src/session/controller.rs"]
pub mod session_controller_gap;

use girder_core::{SessionAddress, SessionEvent, SurfaceEnvelope, SurfaceOp};
use session_controller_gap::{
    DEFAULT_MAX_MESSAGES, HistoryError, JUMP_PAGE_MESSAGES, LineageRow, PAGE_MESSAGES,
    ResolvedHistoryRequest, SessionCursor, TurnWindow, address_id, entry_for, envelope_with_op,
    flatten_lineage, history_record_first_seq, merge_ordered_baseline, page_records, paginate,
    reject_not_found, resolve_address,
};

/// Builds a device event that is surface-eligible but carries no placement.
///
/// `turn/start` is used for both turn boundaries and filler: it is the one non-message,
/// surface-eligible type whose payload the tests never need to inspect.
fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

/// Wraps `events` in envelopes as a writer would publish an ordinary conversation.
///
/// Only surface-eligible types get an `Append` placement; a log-only event such as `turn/start` must
/// carry none, which is exactly the distinction [`paginate`] relies on when it decides what counts
/// toward a message budget.
fn appends(events: Vec<SessionEvent>) -> (Vec<SessionEvent>, Vec<SurfaceEnvelope>) {
    let envelopes: Vec<SurfaceEnvelope> = events
        .iter()
        .cloned()
        .enumerate()
        .map(|(seq, event)| {
            let op = eligible(&event).then_some(SurfaceOp::Append);
            SurfaceEnvelope::new(event, seq as u64, op, Vec::new())
                .expect("the placement matches the event's eligibility")
        })
        .collect();
    (events, envelopes)
}

/// Whether `event` may carry a surface placement.
fn eligible(event: &SessionEvent) -> bool {
    girder_core::is_surface_eligible_type(event.type_name())
}

/// A request with an explicit budget and no turn window.
fn request(max_messages: usize) -> ResolvedHistoryRequest {
    ResolvedHistoryRequest::new(SessionCursor::LogEnd, None, Some(max_messages), None)
        .expect("a positive budget is valid")
}

/// A three-turn conversation: each turn is a start, a user message, and an assistant message.
///
/// Layout (log positions 0..8):
///
/// ```text
/// 0 turn/start(1)   1 user      2 assistant
/// 3 turn/start(2)   4 user      5 assistant
/// 6 turn/start(3)   7 user      8 assistant
/// ```
fn conversation() -> (Vec<SessionEvent>, Vec<SurfaceEnvelope>) {
    let mut events = Vec::new();
    for turn in 1..=3u64 {
        events.push(turn_start(turn));
        events.push(SessionEvent::user_text(&format!("u{turn}"), "hello"));
        events.push(SessionEvent::assistant_text(
            &format!("a{turn}"),
            "hi",
            "deepseek-chat",
        ));
    }
    appends(events)
}

#[test]
fn a_page_counts_messages_not_records() {
    let (events, envelopes) = conversation();
    // Budget of three messages. Walking back from seq 8: assistant(1), user(2), turn/start skipped,
    // assistant(3) — so the cut lands at seq 5 and the page carries the turn/start at 6 as well.
    let page = paginate(&events, &envelopes, &request(3), 8).expect("a valid cut paginates");

    let messages = page
        .events
        .iter()
        .filter(|event| matches!(event.type_name(), "user/message" | "assistant/message"))
        .count();
    assert_eq!(messages, 3, "the budget counts three messages");
    assert_eq!(
        page.events.len(),
        4,
        "a non-message record rides inside the page, so records outnumber messages"
    );
    assert!(
        page.events
            .iter()
            .any(|event| event.type_name() == "turn/start"),
        "the turn start between the messages is part of the page"
    );
    assert!(page.has_more, "older turns remain");
}

#[test]
fn a_budget_that_lands_on_a_message_cuts_exactly_there() {
    let (events, envelopes) = conversation();
    // Budget of two: walking back from 8 the second message is seq 7, so the cut is 7 itself.
    let page = paginate(&events, &envelopes, &request(2), 8).expect("a valid cut paginates");
    assert_eq!(page.events.len(), 2, "both events on the page are messages");
    assert_eq!(page.events[0].type_name(), "user/message");
    assert_eq!(page.events[1].type_name(), "assistant/message");
    assert!(page.has_more);
}

#[test]
fn a_page_that_reaches_seq_zero_reports_no_more() {
    let (events, envelopes) = conversation();
    // A budget wide enough to cover the whole log stops at seq 0.
    let page = paginate(&events, &envelopes, &request(50), 8).expect("a valid cut paginates");
    assert_eq!(page.events.len(), 9, "every event is on the page");
    assert!(!page.has_more, "a cut at zero has nothing older");
}

#[test]
fn a_budget_of_one_moves_the_cut_to_the_newest_message_group() {
    let (events, envelopes) = conversation();
    let page = paginate(&events, &envelopes, &request(1), 8).expect("a valid cut paginates");
    assert_eq!(page.events.len(), 1, "one message is one event");
    assert_eq!(
        page.events.first().map(SessionEvent::type_name),
        Some("assistant/message"),
        "the newest message is the page"
    );
    assert!(page.has_more);
}

#[test]
fn a_grouped_message_pulls_its_sources_onto_the_same_page() {
    let mut events = vec![
        SessionEvent::user_text("u1", "first"),
        SessionEvent::assistant_text("a1", "reply", "deepseek-chat"),
    ];
    // A retry re-derives a message from the two earlier events; its group start is seq 0, not seq 2.
    let envelope = SurfaceEnvelope::new(
        SessionEvent::user_text("u2", "retry"),
        2,
        Some(SurfaceOp::Append),
        vec![0, 1],
    )
    .expect("a grouped append is valid");
    events.push(envelope.event.clone());
    let mut envelopes: Vec<SurfaceEnvelope> = events
        .iter()
        .take(2)
        .cloned()
        .enumerate()
        .map(|(seq, event)| {
            SurfaceEnvelope::new(event, seq as u64, Some(SurfaceOp::Append), Vec::new())
                .expect("append placement is valid")
        })
        .collect();
    envelopes.push(envelope);

    // A budget of one stops at the grouped message, whose sources move the cut back to seq 0.
    let page = paginate(&events, &envelopes, &request(1), 2).expect("a valid cut paginates");
    assert_eq!(
        page.events.len(),
        3,
        "the page carries the group's cited sources rather than orphaning them"
    );
    assert!(!page.has_more, "the cut reached zero");
}

#[test]
fn a_log_only_message_does_not_consume_the_budget() {
    // A replacement message sits on the log but not on the append surface. A backwards page stops at
    // append-origin messages only: if the replacement counted, a budget of two would cut at seq 2.
    let events = vec![
        SessionEvent::user_text("u1", "first"),
        SessionEvent::assistant_text("a1", "reply", "deepseek-chat"),
        SessionEvent::user_text("u2", "replacement"),
        SessionEvent::assistant_text("a2", "reply", "deepseek-chat"),
    ];
    let envelopes = vec![
        SurfaceEnvelope::new(events[0].clone(), 0, Some(SurfaceOp::Append), Vec::new())
            .expect("append placement is valid"),
        SurfaceEnvelope::new(events[1].clone(), 1, Some(SurfaceOp::Append), Vec::new())
            .expect("append placement is valid"),
        SurfaceEnvelope::new(
            events[2].clone(),
            2,
            Some(SurfaceOp::Replace { start: 0, end: 0 }),
            vec![0],
        )
        .expect("replacement placement is valid"),
        SurfaceEnvelope::new(events[3].clone(), 3, Some(SurfaceOp::Append), Vec::new())
            .expect("append placement is valid"),
    ];

    let page = paginate(&events, &envelopes, &request(2), 3).expect("a valid cut paginates");
    assert_eq!(
        page.events.len(),
        3,
        "the cut is set by the two append-origin messages at seq 3 and 1, so the replacement at 2 is inside the page"
    );
    assert_eq!(
        page.events[0].type_name(),
        "assistant/message",
        "the page starts at the second counted message"
    );
    assert!(page.has_more, "an older message remains before seq 1");
}

#[test]
fn a_turn_window_stops_at_a_turn_start_after_both_minima() {
    let (events, envelopes) = conversation();
    // Both minima of one are met by seq 7, so the turn start at seq 6 ends the page: cut = 6.
    let windowed = ResolvedHistoryRequest::new(
        SessionCursor::LogEnd,
        None,
        Some(50),
        Some(TurnWindow {
            min_messages: 1,
            min_turns: 1,
        }),
    )
    .expect("a window within the budget is valid");
    let page = paginate(&events, &envelopes, &windowed, 8).expect("a valid cut paginates");
    assert_eq!(
        page.events.len(),
        3,
        "the page is exactly one whole turn: turn/start, user, assistant"
    );
    assert_eq!(page.events[0].type_name(), "turn/start");
    assert!(
        page.has_more,
        "the window stopped before the log's start, so older turns remain"
    );
}

#[test]
fn without_a_turn_window_the_budget_alone_decides_the_cut() {
    let (events, envelopes) = conversation();
    // The same request with no window runs to the budget instead: two messages at seq 8 and 7, cut 7.
    let page = paginate(&events, &envelopes, &request(2), 8).expect("a valid cut paginates");
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events[0].type_name(), "user/message");
}

#[test]
fn a_turn_window_is_ignored_until_both_minima_are_met() {
    let (events, envelopes) = conversation();
    // Two turns are required, so the first turn start crossed (seq 6) cannot end the page.
    let windowed = ResolvedHistoryRequest::new(
        SessionCursor::LogEnd,
        None,
        Some(50),
        Some(TurnWindow {
            min_messages: 5,
            min_turns: 2,
        }),
    )
    .expect("a window within the budget is valid");
    let page = paginate(&events, &envelopes, &windowed, 8).expect("a valid cut paginates");
    assert_eq!(
        page.events.len(),
        9,
        "the page ran to the log's start rather than stopping at one turn"
    );
}

#[test]
fn the_message_budget_wins_over_an_unsatisfiable_turn_window() {
    let (events, envelopes) = conversation();
    // The window wants two turns, but a one-message budget stops the page long before them.
    let windowed = ResolvedHistoryRequest::new(
        SessionCursor::LogEnd,
        None,
        Some(1),
        Some(TurnWindow {
            min_messages: 1,
            min_turns: 2,
        }),
    )
    .expect("a window within the budget is valid");
    let page = paginate(&events, &envelopes, &windowed, 8).expect("a valid cut paginates");
    assert_eq!(page.events.len(), 1, "the budget cut the page first");
    assert!(page.has_more);
}

#[test]
fn before_seq_bounds_an_older_page() {
    let (events, envelopes) = conversation();
    let older = ResolvedHistoryRequest::new(SessionCursor::LogEnd, Some(2), Some(50), None)
        .expect("a valid before cursor is accepted");
    let page = paginate(&events, &envelopes, &older, 8).expect("a valid cut paginates");
    assert_eq!(page.events.len(), 2, "events 0 and 1");
    assert!(!page.has_more, "the cut reached zero");
}

#[test]
fn before_seq_never_exceeds_the_requested_cut() {
    let (events, envelopes) = conversation();
    // `before_seq` larger than the cut must not expose events past the cut.
    let bounded = ResolvedHistoryRequest::new(SessionCursor::At(4), Some(99), Some(50), None)
        .expect("a valid before cursor is accepted");
    let page = paginate(&events, &envelopes, &bounded, 8).expect("a valid cut paginates");
    assert_eq!(page.events.len(), 5, "the cut at 4 wins over beforeSeq 99");
}

#[test]
fn the_empty_cursor_reads_nothing_even_on_a_non_empty_log() {
    let (events, envelopes) = conversation();
    let empty = ResolvedHistoryRequest::new(SessionCursor::Empty, None, Some(50), None)
        .expect("the empty cursor is valid");
    let page = paginate(&events, &envelopes, &empty, 8).expect("a valid cut paginates");
    assert!(page.events.is_empty(), "an empty cut reads no events");
    assert!(!page.has_more);
}

#[test]
fn a_cut_past_the_observed_end_is_refused() {
    let (events, envelopes) = conversation();
    let ahead = ResolvedHistoryRequest::new(SessionCursor::At(99), None, Some(50), None)
        .expect("the cursor itself is well formed");
    let error =
        paginate(&events, &envelopes, &ahead, 8).expect_err("a cut past the end is refused");
    assert_eq!(error.code(), "gateway/bad-request");
    assert!(error.detail().contains("99"), "the detail names the cut");
}

#[test]
fn a_cut_at_the_observed_end_is_accepted() {
    let (events, envelopes) = conversation();
    let at_end = ResolvedHistoryRequest::new(SessionCursor::At(8), None, Some(50), None)
        .expect("the cursor itself is well formed");
    let page = paginate(&events, &envelopes, &at_end, 8).expect("a cut at the end is valid");
    assert_eq!(page.events.len(), 9);
}

#[test]
fn paginate_rejects_mismatched_inputs() {
    let (events, envelopes) = conversation();
    let error = paginate(&events, &envelopes[..2], &request(5), 8)
        .expect_err("a length disagreement is refused");
    assert_eq!(error.code(), "gateway/bad-request");
}

#[test]
fn the_cursor_round_trips_its_wire_states() {
    assert_eq!(SessionCursor::from_wire(None), Ok(SessionCursor::LogEnd));
    assert_eq!(SessionCursor::from_wire(Some(-1)), Ok(SessionCursor::Empty));
    assert_eq!(SessionCursor::from_wire(Some(0)), Ok(SessionCursor::At(0)));
    assert_eq!(SessionCursor::from_wire(Some(7)), Ok(SessionCursor::At(7)));
    // Below -1 is malformed, matching upstream's `throughSeq must be an integer >= -1`.
    assert!(SessionCursor::from_wire(Some(-2)).is_err());
}

#[test]
fn a_resolved_cursor_substitutes_the_observed_end() {
    assert_eq!(SessionCursor::LogEnd.resolve(42), 42);
    assert_eq!(SessionCursor::Empty.resolve(42), 0);
    assert_eq!(SessionCursor::At(7).resolve(42), 7);
    assert!(SessionCursor::Empty.is_empty());
    assert!(!SessionCursor::At(0).is_empty(), "seq zero is not empty");
}

#[test]
fn the_default_budget_applies_when_omitted() {
    let resolved = ResolvedHistoryRequest::new(SessionCursor::LogEnd, None, None, None)
        .expect("an omitted budget is valid");
    assert_eq!(resolved.max_messages, DEFAULT_MAX_MESSAGES);
}

#[test]
fn a_zero_budget_is_refused() {
    let error = ResolvedHistoryRequest::new(SessionCursor::LogEnd, None, Some(0), None)
        .expect_err("a zero budget is refused");
    assert_eq!(error.code(), "gateway/bad-request");
}

#[test]
fn a_turn_window_above_the_budget_is_refused() {
    let error = ResolvedHistoryRequest::new(
        SessionCursor::LogEnd,
        None,
        Some(20),
        Some(TurnWindow {
            min_messages: 21,
            min_turns: 1,
        }),
    )
    .expect_err("a window demanding more than the budget is refused");
    assert_eq!(error.code(), "gateway/bad-request");
    assert!(
        error.detail().contains("21"),
        "the detail names the minimum"
    );
}

#[test]
fn a_turn_window_with_a_zero_minimum_is_refused() {
    for window in [
        TurnWindow {
            min_messages: 0,
            min_turns: 1,
        },
        TurnWindow {
            min_messages: 1,
            min_turns: 0,
        },
    ] {
        let error =
            ResolvedHistoryRequest::new(SessionCursor::LogEnd, None, Some(50), Some(window))
                .expect_err("a zero minimum is refused");
        assert_eq!(error.code(), "gateway/bad-request");
    }
}

#[test]
fn the_page_size_constants_match_upstream() {
    assert_eq!(PAGE_MESSAGES, 50);
    assert_eq!(DEFAULT_MAX_MESSAGES, 50);
    assert_eq!(JUMP_PAGE_MESSAGES, 200);
}

#[test]
fn records_wrap_events_without_reordering_them() {
    let (events, _) = conversation();
    let records = page_records(&events);
    assert_eq!(records.len(), events.len());
    assert_eq!(records[0].kind, "event");
    for (record, event) in records.iter().zip(events.iter()) {
        assert_eq!(&record.event, event, "records preserve page order");
    }
    let single = entry_for(SessionEvent::user_text("u1", "hi"));
    assert_eq!(single.kind, "event");
    assert_eq!(single.event.type_name(), "user/message");
}

#[test]
fn a_record_reports_the_log_position_of_its_event() {
    let (events, envelopes) = conversation();
    let record = entry_for(events[4].clone());
    assert_eq!(
        history_record_first_seq(&record, &envelopes),
        Some(4),
        "the record stands at its envelope's seq"
    );
}

#[test]
fn an_envelope_rejects_a_placement_it_cannot_carry() {
    // `model/usage` is not surface-eligible, so it cannot take an append placement.
    let event = SessionEvent::ModelUsage {
        model: "deepseek-chat".to_owned(),
        input_tokens: 1,
        output_tokens: 2,
    };
    let error = envelope_with_op(event, 0, SurfaceOp::Append)
        .expect_err("a non-surface event cannot carry a placement");
    assert_eq!(error.code(), "gateway/bad-request");
}

#[test]
fn the_merge_keeps_the_established_order_and_adopts_baseline_values() {
    let current = vec![("a", 1), ("b", 1), ("c", 1)];
    let baseline = vec![("a", 2), ("b", 2), ("c", 2)];
    let merged = merge_ordered_baseline(&current, &baseline, |row| row.0);
    assert_eq!(
        merged,
        vec![("a", 2), ("b", 2), ("c", 2)],
        "order is the client's, values are the baseline's"
    );
}

#[test]
fn the_merge_drops_rows_the_baseline_no_longer_lists() {
    let current = vec![("a", 1), ("gone", 1), ("b", 1)];
    let baseline = vec![("a", 2), ("b", 2)];
    let merged = merge_ordered_baseline(&current, &baseline, |row| row.0);
    assert_eq!(merged, vec![("a", 2), ("b", 2)]);
}

#[test]
fn the_merge_inserts_a_new_row_before_its_nearest_known_follower() {
    let current = vec![("a", 1), ("c", 1)];
    let baseline = vec![("a", 2), ("new", 2), ("c", 2)];
    let merged = merge_ordered_baseline(&current, &baseline, |row| row.0);
    assert_eq!(
        merged,
        vec![("a", 2), ("new", 2), ("c", 2)],
        "the new row lands before c rather than at the end"
    );
}

#[test]
fn the_merge_appends_a_row_with_no_known_follower() {
    let current = vec![("a", 1)];
    let baseline = vec![("a", 2), ("tail", 2)];
    let merged = merge_ordered_baseline(&current, &baseline, |row| row.0);
    assert_eq!(merged, vec![("a", 2), ("tail", 2)]);
}

#[test]
fn the_merge_of_an_empty_client_takes_the_baseline_order() {
    let merged = merge_ordered_baseline(&[], &[("a", 1), ("b", 2)], |row| row.0);
    assert_eq!(merged, vec![("a", 1), ("b", 2)]);
}

#[test]
fn lineage_keeps_root_order_and_expands_children_depth_first() {
    let rows = vec![
        LineageRow {
            session_id: "old-root",
            parent_session_id: None,
            subagent: false,
        },
        LineageRow {
            session_id: "new-root",
            parent_session_id: None,
            subagent: false,
        },
        LineageRow {
            session_id: "kid-old",
            parent_session_id: Some("new-root"),
            subagent: true,
        },
        LineageRow {
            session_id: "kid-new",
            parent_session_id: Some("new-root"),
            subagent: true,
        },
        LineageRow {
            session_id: "grandkid",
            parent_session_id: Some("kid-new"),
            subagent: true,
        },
    ];
    let out = flatten_lineage(&rows);
    let shape: Vec<(&str, usize)> = out
        .iter()
        .map(|entry| (entry.session_id.as_str(), entry.depth))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("old-root", 0),
            ("new-root", 0),
            ("kid-old", 1),
            ("kid-new", 1),
            ("grandkid", 2),
        ]
    );
}

#[test]
fn an_ordinary_fork_stays_at_root_level() {
    // A historical fork may carry a parent id without being a subagent; it must not be nested.
    let rows = vec![
        LineageRow {
            session_id: "source",
            parent_session_id: None,
            subagent: false,
        },
        LineageRow {
            session_id: "unrelated",
            parent_session_id: None,
            subagent: false,
        },
        LineageRow {
            session_id: "legacy-fork",
            parent_session_id: Some("source"),
            subagent: false,
        },
    ];
    let out = flatten_lineage(&rows);
    assert!(out.iter().all(|entry| entry.depth == 0));
    assert_eq!(
        out.iter()
            .map(|entry| entry.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["source", "unrelated", "legacy-fork"],
        "input order is preserved"
    );
}

#[test]
fn an_orphan_degrades_to_a_root_without_being_dropped() {
    let rows = vec![
        LineageRow {
            session_id: "orphan",
            parent_session_id: Some("ghost-parent"),
            subagent: true,
        },
        LineageRow {
            session_id: "root",
            parent_session_id: None,
            subagent: false,
        },
    ];
    let out = flatten_lineage(&rows);
    assert_eq!(out.len(), 2, "no row is lost");
    assert_eq!(out[0].session_id, "orphan");
    assert_eq!(out[0].depth, 0, "an absent parent means root level");
}

#[test]
fn a_two_node_cycle_emits_every_row_once_and_terminates() {
    let rows = vec![
        LineageRow {
            session_id: "a",
            parent_session_id: Some("b"),
            subagent: true,
        },
        LineageRow {
            session_id: "b",
            parent_session_id: Some("a"),
            subagent: true,
        },
        LineageRow {
            session_id: "root",
            parent_session_id: None,
            subagent: false,
        },
    ];
    let out = flatten_lineage(&rows);
    let mut ids: Vec<&str> = out.iter().map(|entry| entry.session_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["a", "b", "root"],
        "every row appears exactly once"
    );
}

#[test]
fn a_self_reference_is_a_cycle_member_at_root_level() {
    let rows = vec![LineageRow {
        session_id: "self",
        parent_session_id: Some("self"),
        subagent: true,
    }];
    let out = flatten_lineage(&rows);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].session_id, "self");
    assert_eq!(out[0].depth, 0);
}

#[test]
fn lineage_rows_index_their_input() {
    let rows = vec![
        LineageRow {
            session_id: "root",
            parent_session_id: None,
            subagent: false,
        },
        LineageRow {
            session_id: "kid",
            parent_session_id: Some("root"),
            subagent: true,
        },
    ];
    let out = flatten_lineage(&rows);
    assert_eq!(out[0].index, 0);
    assert_eq!(out[1].index, 1);
}

#[test]
fn an_empty_list_flattens_to_nothing() {
    assert!(flatten_lineage(&[]).is_empty());
}

#[test]
fn addresses_resolve_to_the_session_they_name() {
    let ordinary = SessionAddress {
        kind: "session".to_owned(),
        session_id: "s1".to_owned(),
    };
    assert_eq!(address_id(&ordinary), "s1");
    assert_eq!(resolve_address(&ordinary), Ok("s1".to_owned()));
    assert_eq!(reject_not_found(&ordinary).code(), "session/not-found");
}

#[test]
fn a_subagent_address_is_refused_rather_than_redirected_to_its_parent() {
    // This build has no subagent registry: reading the parent's log instead would hand the caller
    // another session's conversation, so the only safe answer is a refusal.
    let child = SessionAddress {
        kind: "subagent".to_owned(),
        session_id: "child".to_owned(),
    };
    let error = resolve_address(&child).expect_err("a child read is refused");
    assert_eq!(error.code(), "subagent/not-found");
    assert_eq!(reject_not_found(&child).code(), "subagent/not-found");
    assert_eq!(
        address_id(&child),
        "child",
        "the child's own id is named even when the read is refused"
    );
}

#[test]
fn an_error_renders_its_code_and_detail() {
    let error = HistoryError::bad_request("nope");
    assert_eq!(error.code(), "gateway/bad-request");
    assert_eq!(error.detail(), "nope");
    assert_eq!(format!("{error}"), "gateway/bad-request: nope");
    // The trait object is what a transport erases it into.
    let erased: &dyn std::error::Error = &error;
    assert!(erased.to_string().contains("nope"));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_controller_gap as controller;
}
