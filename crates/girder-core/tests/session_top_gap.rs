//! Tests for the top-level Host producers of upstream `packages/api/session-controller`.
//!
//! # Why these cases and not others
//!
//! The module exists because of one finding: the consumers of `sessionListMetadata` already existed in
//! `session_snapshot_gap` and nothing produced the key. A test suite that only checked "the fold
//! compiles and produces JSON" would leave that finding unverified, so the first cases here are
//! *end-to-end through the existing consumer*: fold real events, publish the value, hand it to
//! `ListEngine`, and read the list row the user would see.
//!
//! Every remaining case pins one rule whose violation is expensive and silent:
//!
//! - **`blank` never rises.** A fold that recomputed blankness would re-hide a conversation after a
//!   compaction dropped its first turn.
//! - **Only a human prompt moves `lastPromptAt`.** A relay, an injection, or a system prompt arriving
//!   as user-role content would otherwise make "last activity" report the harness talking to itself.
//! - **A continuation needs admitted work AND a failed attempt AND a continuable code.** Dropping any
//!   one turns a failed turn into a silent retry of something nobody asked for.
//! - **`turn/end` clears on every non-continuable outcome.** A stale eligible seq would let an empty
//!   prompt replay a turn that already completed.
//! - **The wire carries only `ending`.** Publishing `admitted` invites a client to re-derive a rule
//!   whose `stateVersion` it cannot see.
//! - **`has_more` is `accepted > limit`, not `== limit`.** A provider that returned exactly the limit
//!   has not shown that anything follows.
//! - **A snippet counts code points, not bytes.** Cutting mid-code-point is the mojibake that reads as
//!   log corruption.
//! - **An unloadable parent is still checked against the archive set.** Treating "not resident" as
//!   "not archived" runs a model step inside an archived lineage.
//! - **A fork breaks lineage.** Only a subagent-origin Session has an ancestor to walk to.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module the `crate::` prefix names *this* test crate, so every reference to
//! the library goes through `girder_core::{...}`, and the module under test does the same.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "tests assert by panicking, index their own fixtures, and read best as one scenario"
)]

#[path = "../src/session_top_gap.rs"]
mod session_top_gap;

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use girder_core::session_snapshot_gap::{ControlFrame, FormatStatus, ListEngine, ListSummary};
use girder_core::session_stats_gap::{Recorded, TimeSource, Untimed};
use girder_core::{
    AgentId, ChatMessage, ChatRole, ContentBlock, MessageId, MessageSource, Projection,
    SessionEvent, TurnEndReason, VersionedProjection,
};

use session_top_gap::{
    CONTINUABLE_FAILURES, CONTINUATION_KEY, CONTINUATION_STATE_VERSION, ContinuationEvent,
    ContinuationProjection, ContinuationSource, ContinuationState, ContinuationSurface,
    LIST_METADATA_KEY, LIST_METADATA_STATE_VERSION, LineageHeader, SEARCH_PROVIDER_CALL_LIMIT,
    SESSION_SEARCH_QUERY_MAX_CHARS, SESSION_SEARCH_RESULT_LIMIT,
    SESSION_SEARCH_SNIPPET_MAX_CODE_POINTS, SearchAccumulator, SearchError, SearchHit, SearchPager,
    SearchStop, SearchVerdict, SessionListMetadata, SessionListMetadataProjection,
    apply_session_list_metadata, fold_continuation, is_continuable_failure, normalize_search_query,
    publish_wire, summary_block, truncate_unicode_code_points, under_archived_session, updated_at,
};

// ---- fixtures ---------------------------------------------------------------------------------

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

fn chat(source: MessageSource, blocks: Vec<ContentBlock>) -> ChatMessage {
    ChatMessage::new(
        MessageId::new("m1").expect("a legal message id"),
        ChatRole::User,
        source,
        blocks,
    )
}

fn user_message(text: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        message: chat(MessageSource::User { form: None }, vec![text_block(text)]),
    }
}

fn relay_message() -> SessionEvent {
    SessionEvent::UserMessage {
        message: chat(
            MessageSource::Relay {
                from: AgentId::new("root").expect("a legal agent id"),
            },
            vec![text_block("relayed")],
        ),
    }
}

fn system_message() -> SessionEvent {
    SessionEvent::SystemMessage {
        turn: 1,
        step: 1,
        message: chat(MessageSource::SystemPrompt, vec![text_block("prompt")]),
    }
}

fn turn_start() -> SessionEvent {
    SessionEvent::TurnStart { turn: 1 }
}

fn failed_turn(code: &str) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Failed {
            message: "boom".to_owned(),
            code: code.to_owned(),
        },
    }
}

/// One local durable event mapped to the continuation input, appending to the surface.
fn mapped(event: &SessionEvent) -> ContinuationEvent {
    ContinuationEvent::from_session_event(event, ContinuationSurface::Append)
        .expect("the fixture event maps")
}

fn human_input(text: &str, blocks: usize) -> ContinuationEvent {
    ContinuationEvent::UserMessage {
        surface: ContinuationSurface::Append,
        source: ContinuationSource::User,
        content_blocks: blocks.max(1).saturating_sub(1) + usize::from(!text.is_empty()),
    }
}

fn summary(id: &str, updated_at: u64) -> ListSummary {
    ListSummary {
        session_id: id.to_owned(),
        updated_at,
        running: false,
        blank: true,
        agent_available: false,
        parent_session_id: None,
        origin_subagent: false,
        cwd: Some("/work".to_owned()),
        format_status: Some(FormatStatus::Current),
        projections: None,
    }
}

fn archived(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

// ---- the producer/consumer join: the finding this module was written for ---------------------

#[test]
fn the_metadata_producer_feeds_the_list_row_that_already_read_it() {
    // Before this module, `session_snapshot_gap` read `"sessionListMetadata"` and nothing wrote it:
    // every test that exercised the reconciliation hand-wrote the JSON. This case closes the loop
    // through `ListEngine`, so the producer and its existing consumer are verified together.
    let clock: Arc<dyn TimeSource> = Arc::new(Recorded::new().with_time(1, 1_700));
    let projection = SessionListMetadataProjection::new(Arc::clone(&clock));

    // seq 1: the person speaks. seq 2: the turn starts.
    let after_prompt = projection.apply(projection.init(), &user_message("hello"), 1);
    let after_turn = projection.apply(after_prompt.clone(), &turn_start(), 2);

    let metadata =
        SessionListMetadata::from_json(&after_turn).expect("the producer wrote its own shape");
    assert!(
        !metadata.blank,
        "a turn started, so the session has history"
    );
    assert_eq!(metadata.last_prompt_at, Some(1_700), "the prompt was dated");

    // The row now carries the projection the way a list frame would, and the existing engine folds it.
    let engine = ListEngine::new();
    engine.handle_session_added(summary("s1", 10));
    assert!(
        engine.snapshot().items()[0].blank,
        "a fresh row starts blank"
    );
    engine.handle_control_frame(&ControlFrame::Projection(
        girder_core::session_snapshot_gap::ProjectionUpdate {
            session_id: "s1".to_owned(),
            seq: 2,
            changes: vec![(LIST_METADATA_KEY.to_owned(), after_turn.clone())],
        },
    ));
    engine.flush();
    let snapshot = engine.snapshot();
    let row = &snapshot.items()[0];
    assert!(
        !row.blank,
        "the produced metadata cleared the stale row's blank"
    );
    assert_eq!(
        row.updated_at, 1_700,
        "the produced lastPromptAt corrected the row's time"
    );
}

#[test]
fn the_projection_publishes_the_exact_shape_the_readers_parse() {
    let value = SessionListMetadata::new().to_json();
    let object = value.as_object().expect("an object");
    assert_eq!(
        object.len(),
        2,
        "exactly the two fields the schema declares"
    );
    assert_eq!(object.get("blank"), Some(&json!(true)));
    assert_eq!(
        object.get("lastPromptAt"),
        Some(&Value::Null),
        "null rather than absent or zero: the reader is `number().nullable()`"
    );
}

#[test]
fn the_projection_is_registrable_and_versioned() {
    let projection = SessionListMetadataProjection::untimed();
    assert_eq!(projection.key(), "sessionListMetadata");
    assert_eq!(projection.key(), LIST_METADATA_KEY);
    assert_eq!(projection.state_version(), 1);
    assert_eq!(projection.state_version(), LIST_METADATA_STATE_VERSION);
    assert!(
        projection.in_summary(),
        "a list row renders its own blankness"
    );

    let continuation = ContinuationProjection;
    assert_eq!(continuation.key(), CONTINUATION_KEY);
    assert_eq!(
        continuation.state_version(),
        4,
        "upstream's stateVersion: 4"
    );
    assert_eq!(continuation.state_version(), CONTINUATION_STATE_VERSION);
    assert!(continuation.in_summary());
}

#[test]
fn an_unparsable_state_is_left_untouched_rather_than_reinitialised() {
    // The registry's cursor has already advanced past the event, so replacing a corrupt state with
    // `init` would discard a fold that could not be rebuilt on the next event.
    let projection = SessionListMetadataProjection::untimed();
    let corrupt = json!({ "blank": "yes" });
    assert_eq!(
        projection.apply(corrupt.clone(), &turn_start(), 1),
        corrupt,
        "the unreadable state survives"
    );

    let continuation = ContinuationProjection;
    let corrupt = json!({ "admitted": 1 });
    assert_eq!(
        continuation.apply(corrupt.clone(), &turn_start(), 1),
        corrupt
    );
}

// ---- sessionListMetadata rules ----------------------------------------------------------------

#[test]
fn blank_falls_exactly_once_and_never_rises() {
    let mut state = SessionListMetadata::new();
    assert!(state.blank);

    state = apply_session_list_metadata(state, &user_message("hi"), Some(5));
    assert!(state.blank, "a prompt alone starts no turn");

    state = apply_session_list_metadata(state, &turn_start(), Some(6));
    assert!(!state.blank, "the first turn/start lowers blank");

    // A per-turn compaction can drop the original `turn/start`; a fold that recomputed blankness
    // would re-hide the conversation.
    state = apply_session_list_metadata(state, &user_message("again"), Some(7));
    assert!(!state.blank, "blank never rises");
    assert_eq!(state.last_prompt_at, Some(7));
}

#[test]
fn an_unchanged_fold_returns_the_same_state() {
    // Upstream returns the original object so a long replay does not reallocate every state.
    let state = SessionListMetadata::with(false, Some(9));
    let unchanged = apply_session_list_metadata(
        state,
        &SessionEvent::StepStart { turn: 1, step: 1 },
        Some(9),
    );
    assert_eq!(
        unchanged, state,
        "an event that changes nothing produces the same value"
    );
}

#[test]
fn only_a_human_prompt_moves_last_prompt_at() {
    let mut state = SessionListMetadata::new();

    state = apply_session_list_metadata(state, &user_message("mine"), Some(10));
    assert_eq!(state.last_prompt_at, Some(10));

    // A relayed peer message is user-role content but not the person's word.
    state = apply_session_list_metadata(state, &relay_message(), Some(20));
    assert_eq!(state.last_prompt_at, Some(10), "a relay is not a prompt");

    // A system prompt rides the surface as a message too.
    state = apply_session_list_metadata(state, &system_message(), Some(30));
    assert_eq!(
        state.last_prompt_at,
        Some(10),
        "the harness is not the person"
    );

    // And an undated log keeps the previous answer instead of inventing an instant.
    state = apply_session_list_metadata(state, &user_message("late"), None);
    assert_eq!(state.last_prompt_at, Some(10), "unknown is not zero");
}

#[test]
fn the_metadata_projection_without_a_clock_reports_blank_exactly_and_time_not_at_all() {
    let projection = SessionListMetadataProjection::untimed();
    let after = projection.apply(projection.init(), &user_message("hi"), 1);
    let metadata = SessionListMetadata::from_json(&after).expect("parses");
    assert!(metadata.blank, "blankness needs no clock");
    assert_eq!(
        metadata.last_prompt_at, None,
        "an untimed source reports no time rather than 0"
    );
}

#[test]
fn updated_at_is_the_later_of_the_header_and_the_prompt() {
    assert_eq!(
        updated_at(10, SessionListMetadata::with(false, Some(20))),
        20
    );
    assert_eq!(
        updated_at(30, SessionListMetadata::with(false, Some(20))),
        30,
        "a header timestamp is a real lower bound"
    );
    assert_eq!(updated_at(30, SessionListMetadata::new()), 30);
    assert_eq!(
        updated_at(30, SessionListMetadata::with(false, Some(-5))),
        30,
        "a negative time is not a time"
    );
}

// ---- conversationContinuation rules -----------------------------------------------------------

#[test]
fn a_continuation_needs_admitted_work_a_failed_attempt_and_a_continuable_code() {
    // All three, and it is eligible.
    let mut state = fold_continuation(ContinuationState::empty(), human_input("hi", 1), 1);
    assert!(state.admitted);
    state = fold_continuation(state, ContinuationEvent::FailedAttempt, 2);
    assert!(state.failed_attempt);
    state = fold_continuation(
        state,
        ContinuationEvent::TurnEnd {
            failure_code: Some("RATE_LIMIT".to_owned()),
        },
        3,
    );
    assert_eq!(state.ending, Some(3), "the turn/end seq is the answer");
    assert!(state.is_continuable());

    // Without admitted work, no continuation.
    let mut state = fold_continuation(
        ContinuationState::empty(),
        ContinuationEvent::FailedAttempt,
        1,
    );
    state = fold_continuation(
        state,
        ContinuationEvent::TurnEnd {
            failure_code: Some("RATE_LIMIT".to_owned()),
        },
        2,
    );
    assert_eq!(
        state,
        ContinuationState::empty(),
        "a turn nobody spoke in is not continuable"
    );

    // Without a failed attempt, no continuation.
    let mut state = fold_continuation(ContinuationState::empty(), human_input("hi", 1), 1);
    state = fold_continuation(
        state,
        ContinuationEvent::TurnEnd {
            failure_code: Some("RATE_LIMIT".to_owned()),
        },
        2,
    );
    assert_eq!(
        state,
        ContinuationState::empty(),
        "a turn that never failed is not continuable"
    );
}

#[test]
fn every_continuable_failure_is_reported_and_every_other_code_is_not() {
    for code in CONTINUABLE_FAILURES {
        assert!(is_continuable_failure(code), "{code} is continuable");
    }
    for code in ["QUOTA", "CONTEXT_WINDOW_EXCEEDED", "UNKNOWN_MODEL", ""] {
        assert!(
            !is_continuable_failure(code),
            "{code} is not something a person's retry can repair"
        );
    }
}

#[test]
fn a_completed_or_aborted_turn_clears_an_earlier_eligible_continuation() {
    // An earlier turn failed and was eligible; the *next* turn completes. The ending must be erased,
    // not retained: eligibility is about the last turn.
    let mut state = fold_continuation(ContinuationState::empty(), human_input("hi", 1), 1);
    state = fold_continuation(state, ContinuationEvent::FailedAttempt, 2);
    state = fold_continuation(
        state,
        ContinuationEvent::TurnEnd {
            failure_code: Some("TIMEOUT".to_owned()),
        },
        3,
    );
    assert!(state.is_continuable());

    state = fold_continuation(state, ContinuationEvent::TurnEnd { failure_code: None }, 4);
    assert_eq!(
        state,
        ContinuationState::empty(),
        "a completed turn erases it"
    );
}

#[test]
fn turn_start_resets_everything() {
    let state = ContinuationState::with(Some(9), true, true);
    assert_eq!(
        fold_continuation(state, ContinuationEvent::TurnStart, 10),
        ContinuationState::empty()
    );
}

#[test]
fn progress_clears_only_the_failed_attempt_bit() {
    let state = ContinuationState::with(None, true, true);
    let progressed = fold_continuation(state, ContinuationEvent::Progressed, 2);
    assert!(progressed.admitted, "the person's work survives a retry");
    assert!(
        !progressed.failed_attempt,
        "reaching the model clears the failure"
    );

    // Already cleared: nothing changes, and the same value is returned.
    assert_eq!(
        fold_continuation(progressed, ContinuationEvent::Progressed, 3),
        progressed
    );
}

#[test]
fn only_an_appended_human_or_retry_message_admits_work() {
    // A replacement shadows history rather than adding to it.
    let replaced = ContinuationEvent::UserMessage {
        surface: ContinuationSurface::Replace,
        source: ContinuationSource::User,
        content_blocks: 1,
    };
    assert!(!fold_continuation(ContinuationState::empty(), replaced, 1).admitted);

    // A human message with no blocks admits nothing: an empty prompt from a person is a mistake.
    let empty_human = ContinuationEvent::UserMessage {
        surface: ContinuationSurface::Append,
        source: ContinuationSource::User,
        content_blocks: 0,
    };
    assert!(!fold_continuation(ContinuationState::empty(), empty_human, 1).admitted);

    // The retry placeholder is the mirror image: empty by design, and it admits.
    let retry = ContinuationEvent::UserMessage {
        surface: ContinuationSurface::Append,
        source: ContinuationSource::Retry,
        content_blocks: 0,
    };
    assert!(
        fold_continuation(ContinuationState::empty(), retry, 1).admitted,
        "the empty retry message is the feature"
    );

    // A retry that carries content is not the retry path.
    let retry_with_content = ContinuationEvent::UserMessage {
        surface: ContinuationSurface::Append,
        source: ContinuationSource::Retry,
        content_blocks: 1,
    };
    assert!(!fold_continuation(ContinuationState::empty(), retry_with_content, 1).admitted);

    // And anything else never admits, whatever it carries.
    let other = ContinuationEvent::UserMessage {
        surface: ContinuationSurface::Append,
        source: ContinuationSource::Other,
        content_blocks: 3,
    };
    assert!(!fold_continuation(ContinuationState::empty(), other, 1).admitted);
}

#[test]
fn the_wire_carries_only_the_ending() {
    let state = ContinuationState::with(Some(7), true, true);
    assert_eq!(state.to_wire(), json!(7));
    assert_eq!(
        ContinuationState::empty().to_wire(),
        Value::Null,
        "not continuable is null, not zero: seq 0 is a real event"
    );
    // The internal state keeps all three, and round-trips.
    let json = state.to_json();
    assert_eq!(
        json.as_object().expect("an object").len(),
        3,
        "the internal state carries all three facts"
    );
    assert_eq!(ContinuationState::from_json(&json), Some(state));
}

#[test]
fn the_continuation_projection_folds_the_local_events_it_can_see() {
    let projection = ContinuationProjection;
    let mut state = projection.init();

    state = projection.apply(state, &user_message("hi"), 1);
    state = projection.apply(state.clone(), &turn_start(), 2);
    // The local enum has no `assistant/attempt`, so a failed settlement cannot be produced from it:
    // this is the boundary the module documents rather than papering over with an invented tag.
    state = projection.apply(state.clone(), &failed_turn("RATE_LIMIT"), 3);
    let parsed = ContinuationState::from_json(&state).expect("parses");
    assert_eq!(
        parsed,
        ContinuationState::empty(),
        "without the attempt fact, no continuation is claimed"
    );
}

#[test]
fn a_local_user_message_maps_to_an_appending_human_input() {
    let SessionEvent::UserMessage { .. } = user_message("hi") else {
        panic!("fixture is a user message");
    };
    assert_eq!(
        mapped(&user_message("hi")),
        ContinuationEvent::UserMessage {
            surface: ContinuationSurface::Append,
            source: ContinuationSource::User,
            content_blocks: 1,
        }
    );
    assert_eq!(mapped(&turn_start()), ContinuationEvent::TurnStart);
    assert_eq!(
        mapped(&SessionEvent::StepStart { turn: 1, step: 1 }),
        ContinuationEvent::Progressed
    );
    assert_eq!(
        mapped(&failed_turn("TIMEOUT")),
        ContinuationEvent::TurnEnd {
            failure_code: Some("TIMEOUT".to_owned())
        }
    );
    // A non-failed ending reports no code at all.
    let completed = SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    };
    assert_eq!(
        mapped(&completed),
        ContinuationEvent::TurnEnd { failure_code: None }
    );
    // An event outside the fold answers `None` rather than a fabricated default.
    assert_eq!(
        ContinuationEvent::from_session_event(
            &SessionEvent::ModelUsage {
                model: "m".to_owned(),
                input_tokens: 1,
                output_tokens: 2,
            },
            ContinuationSurface::Append
        ),
        None
    );
}

#[test]
fn publication_selects_the_wire_view_per_unit() {
    let continuation = ContinuationState::with(Some(4), true, true);
    assert_eq!(
        publish_wire(CONTINUATION_KEY, &continuation.to_json()),
        Some(json!(4)),
        "the continuation publishes `ending`, not its internal state"
    );

    let metadata = SessionListMetadata::with(false, Some(2)).to_json();
    assert_eq!(
        publish_wire(LIST_METADATA_KEY, &metadata),
        Some(metadata.clone()),
        "the list unit publishes its whole state"
    );
    assert_eq!(publish_wire("unknown", &json!(1)), None);

    let mut states = Map::new();
    states.insert(LIST_METADATA_KEY.to_owned(), metadata);
    states.insert(CONTINUATION_KEY.to_owned(), continuation.to_json());
    states.insert("notAModel".to_owned(), json!(1));
    let block = summary_block(&states);
    assert_eq!(block.len(), 2, "only modelled units reach a summary");
    assert_eq!(block.get(CONTINUATION_KEY), Some(&json!(4)));
}

// ---- the pure half of list.ts -----------------------------------------------------------------

#[test]
fn a_snippet_is_cut_on_code_point_boundaries() {
    // Eight code points, each two or three bytes in UTF-8; a byte cut would produce mojibake.
    assert_eq!(truncate_unicode_code_points("αβγδεζηθ", 3), "αβγ");
    assert_eq!(truncate_unicode_code_points("αβγ", 10), "αβγ");
    assert_eq!(truncate_unicode_code_points("αβγ", 0), "");
    assert_eq!(truncate_unicode_code_points("", 5), "");

    let long: String = "語".repeat(SESSION_SEARCH_SNIPPET_MAX_CODE_POINTS + 50);
    let cut = truncate_unicode_code_points(&long, SESSION_SEARCH_SNIPPET_MAX_CODE_POINTS);
    assert_eq!(cut.chars().count(), SESSION_SEARCH_SNIPPET_MAX_CODE_POINTS);
    assert!(long.starts_with(&cut), "a prefix, never a re-encoding");
}

#[test]
fn a_search_query_is_trimmed_measured_and_refused_as_upstream_does() {
    assert_eq!(
        normalize_search_query("  needle  "),
        Ok("needle".to_owned()),
        "trimmed before it is measured"
    );
    assert_eq!(normalize_search_query("   "), Err(SearchError::Empty));
    assert_eq!(normalize_search_query("a\0b"), Err(SearchError::Nul));

    // Measured in UTF-16 code units, which is what upstream's `String.length` counts: an astral
    // character is two units, so the same string is longer by this measure than by code points.
    let astral = "𝄞".repeat(SESSION_SEARCH_QUERY_MAX_CHARS / 2 + 1);
    assert_eq!(
        astral.chars().count(),
        SESSION_SEARCH_QUERY_MAX_CHARS / 2 + 1
    );
    assert_eq!(
        normalize_search_query(&astral),
        Err(SearchError::TooLong {
            length: astral.encode_utf16().count(),
            limit: SESSION_SEARCH_QUERY_MAX_CHARS,
        }),
        "a string inside the code-point budget can exceed the UTF-16 budget"
    );
    let at_limit = "a".repeat(SESSION_SEARCH_QUERY_MAX_CHARS);
    assert_eq!(normalize_search_query(&at_limit), Ok(at_limit));
}

#[test]
fn a_hit_whose_session_is_invisible_is_refused_before_its_content_is_read() {
    let mut acc = SearchAccumulator::new(archived(&["visible"]));
    assert!(!acc.is_visible("hidden"));
    assert_eq!(
        acc.accept(&SearchHit {
            header_session_id: "hidden",
            best_match_session_id: "hidden",
            best_match_surface: "current",
            best_match_type: "user/message",
            snippet: "secret",
        }),
        SearchVerdict::Invisible
    );
    assert_eq!(
        acc.accepted(),
        0,
        "the snippet never entered the result set"
    );
}

#[test]
fn a_hit_must_be_a_current_message_naming_its_own_session() {
    let mut acc = SearchAccumulator::new(archived(&["s"]));
    let base = SearchHit {
        header_session_id: "s",
        best_match_session_id: "s",
        best_match_surface: "current",
        best_match_type: "user/message",
        snippet: "hit",
    };
    assert_eq!(
        acc.accept(&SearchHit {
            best_match_session_id: "other",
            ..base
        }),
        SearchVerdict::NotAMessage,
        "a hit that names another session is a provider bug, not a result"
    );
    assert_eq!(
        acc.accept(&SearchHit {
            best_match_surface: "history",
            ..base
        }),
        SearchVerdict::NotAMessage
    );
    assert_eq!(
        acc.accept(&SearchHit {
            best_match_type: "tool/result",
            ..base
        }),
        SearchVerdict::NotAMessage
    );
    assert_eq!(
        acc.accept(&SearchHit {
            best_match_type: "assistant/message",
            ..base
        }),
        SearchVerdict::Accepted
    );
}

#[test]
fn one_session_yields_one_result_and_the_limit_decides_has_more() {
    let ids: Vec<String> = (0..=SESSION_SEARCH_RESULT_LIMIT)
        .map(|i| format!("s{i}"))
        .collect();
    let visible: BTreeSet<String> = ids.iter().cloned().collect();
    let mut acc = SearchAccumulator::new(visible);

    for id in &ids {
        let hit = SearchHit {
            header_session_id: id,
            best_match_session_id: id,
            best_match_surface: "current",
            best_match_type: "user/message",
            snippet: "text",
        };
        let first = acc.accept(&hit);
        let second = acc.accept(&hit);
        assert_eq!(first, SearchVerdict::Accepted);
        assert_eq!(second, SearchVerdict::Duplicate, "one row per session");
    }

    // Accepted `LIMIT + 1` identities: the overflow one is the proof that more exist.
    assert_eq!(acc.accepted(), SESSION_SEARCH_RESULT_LIMIT + 1);
    let value = acc.value();
    assert_eq!(value.items.len(), SESSION_SEARCH_RESULT_LIMIT);
    assert!(value.has_more, "one identity did not fit");

    // Exactly the limit, with nothing after it, is not "more".
    let mut exact = SearchAccumulator::new(archived(&["a", "b"]));
    for id in ["a", "b"] {
        let _ = exact.accept(&SearchHit {
            header_session_id: id,
            best_match_session_id: id,
            best_match_surface: "current",
            best_match_type: "assistant/message",
            snippet: "text",
        });
    }
    assert!(!exact.value().has_more, "no overflow, so nothing follows");
}

#[test]
fn an_accepted_snippet_is_bounded_by_the_snippet_limit() {
    let mut acc = SearchAccumulator::new(archived(&["s"]));
    let long = "x".repeat(SESSION_SEARCH_SNIPPET_MAX_CODE_POINTS + 10);
    assert_eq!(
        acc.accept(&SearchHit {
            header_session_id: "s",
            best_match_session_id: "s",
            best_match_surface: "current",
            best_match_type: "user/message",
            snippet: &long,
        }),
        SearchVerdict::Accepted
    );
    assert_eq!(
        acc.value().items[0].snippet.chars().count(),
        SESSION_SEARCH_SNIPPET_MAX_CODE_POINTS
    );
}

#[test]
fn the_pager_stops_a_runaway_provider_three_different_ways() {
    let mut pager = SearchPager::new(10);
    assert!(!pager.is_finished());

    // The work budget is charged before each call, so a provider cannot exceed it by one.
    for _ in 0..SEARCH_PROVIDER_CALL_LIMIT {
        pager.charge_call().expect("inside the budget");
    }
    assert_eq!(pager.calls(), SEARCH_PROVIDER_CALL_LIMIT);
    assert_eq!(
        pager.charge_call(),
        Err(SearchStop::ProviderBudgetExhausted {
            limit: SEARCH_PROVIDER_CALL_LIMIT
        })
    );
    assert!(pager.is_finished());

    // A page larger than the asked-for size means the bound was not honoured.
    let pager = SearchPager::new(4);
    assert_eq!(pager.check_page(4), Ok(()));
    assert_eq!(
        pager.check_page(5),
        Err(SearchStop::OverlongPage { got: 5, asked: 4 })
    );

    // A repeated cursor would request the same page forever.
    let mut pager = SearchPager::new(4);
    assert_eq!(pager.observe_cursor("c1"), Ok(()));
    assert_eq!(pager.observe_cursor("c2"), Ok(()));
    assert_eq!(pager.observe_cursor("c1"), Err(SearchStop::RepeatedCursor));
    assert!(pager.is_finished());
}

#[test]
fn a_refused_page_size_halves_and_floors_at_one() {
    // Upstream recovers from SESSION_QUERY_INVALID_LIMIT by asking for less, never by retrying the
    // same number; one item is the floor because zero is a request no provider accepts.
    let mut pager = SearchPager::new(20);
    pager.shrink_page_limit();
    assert_eq!(pager.page_limit(), 10);
    pager.shrink_page_limit();
    assert_eq!(pager.page_limit(), 5);
    pager.shrink_page_limit();
    assert_eq!(pager.page_limit(), 2);
    pager.shrink_page_limit();
    assert_eq!(pager.page_limit(), 1);
    pager.shrink_page_limit();
    assert_eq!(pager.page_limit(), 1, "floors at one");
}

// ---- archived-session-gate ------------------------------------------------------------------

fn header<'a>(id: &'a str, subagent: bool, parent: Option<&'a str>) -> LineageHeader<'a> {
    LineageHeader {
        id,
        origin_subagent: subagent,
        parent_session: parent,
    }
}

#[test]
fn an_archived_root_owns_its_own_step() {
    let root = header("root", false, None);
    assert!(under_archived_session(root, &archived(&["root"]), |_| None));
    assert!(!under_archived_session(root, &archived(&[]), |_| None));
}

#[test]
fn an_archived_ancestor_owns_a_descendants_step() {
    let child = header("child", true, Some("root"));
    let owned = under_archived_session(child, &archived(&["root"]), |id| {
        (id == "root").then_some(LineageHeader {
            id: "root",
            origin_subagent: false,
            parent_session: None,
        })
    });
    assert!(owned);

    // Two levels down: a grandchild reaches the archived root through a live middle Session.
    let grandchild = header("grandchild", true, Some("child"));
    let owned = under_archived_session(grandchild, &archived(&["root"]), |id| match id {
        "child" => Some(LineageHeader {
            id: "child",
            origin_subagent: true,
            parent_session: Some("root"),
        }),
        "root" => Some(LineageHeader {
            id: "root",
            origin_subagent: false,
            parent_session: None,
        }),
        _ => None,
    });
    assert!(
        owned,
        "the walk follows subagent ancestry to the archived root"
    );
}

#[test]
fn a_fork_breaks_the_line_because_it_has_no_ancestor_to_walk_to() {
    // A fork of an archived Session is an independent conversation: it inherited a prefix, not a
    // lifecycle. Only `origin === 'subagent'` carries ancestry.
    let fork = header("fork", false, Some("archived-root"));
    assert!(
        !under_archived_session(fork, &archived(&["archived-root"]), |_| None),
        "a fork is not a descendant"
    );
}

#[test]
fn an_unloadable_parent_is_still_checked_against_the_archive_set() {
    // The parent Session is not resident. Assuming "not resident" means "not archived" is the one
    // answer that runs a model step inside an archived lineage.
    let child = header("child", true, Some("gone"));
    assert!(
        under_archived_session(child, &archived(&["gone"]), |_| None),
        "an unloadable archived parent still owns the step"
    );
    assert!(!under_archived_session(
        child,
        &archived(&["someone-else"]),
        |_| None
    ));
}

#[test]
fn a_cyclic_lineage_terminates_and_claims_nothing() {
    // Corrupt data: the walk must stop rather than loop, and no archived Session was reached.
    let start = header("a", true, Some("b"));
    let owned = under_archived_session(start, &archived(&["unrelated"]), |id| {
        Some(if id == "b" {
            LineageHeader {
                id: "b",
                origin_subagent: true,
                parent_session: Some("a"),
            }
        } else {
            LineageHeader {
                id: "a",
                origin_subagent: true,
                parent_session: Some("b"),
            }
        })
    });
    assert!(!owned, "a cycle terminates without claiming ownership");
}

#[test]
fn a_subagent_with_no_parent_stops_the_walk() {
    let orphan = header("child", true, None);
    assert!(!under_archived_session(
        orphan,
        &archived(&["root"]),
        |_| None
    ));
}

// ---- a clock that cannot date anything ---------------------------------------------------------

#[test]
fn choosing_the_untimed_source_keeps_counts_exact_and_times_absent() {
    // Documented so the choice is visible: the untimed source never fabricates a time, and the list
    // unit's `blank` does not depend on one.
    let untimed = Untimed;
    assert_eq!(TimeSource::at_ms(&untimed, 1), None);

    let projection = SessionListMetadataProjection::new(Arc::new(Untimed));
    let state = projection.apply(projection.init(), &turn_start(), 1);
    assert_eq!(
        state,
        SessionListMetadata::with(false, None).to_json(),
        "blank is exact; time is absent rather than zero"
    );
}

#[test]
fn the_block_and_the_content_helpers_are_public_surface() {
    // A compile-level check that the helpers accept the values a caller actually holds: a `Message`
    // carries its blocks as a `Vec<ContentBlock>`, and the continuation fold counts them.
    let message = chat(
        MessageSource::User { form: None },
        vec![text_block("a"), text_block("b")],
    );
    let event = SessionEvent::UserMessage { message };
    assert_eq!(
        mapped(&event),
        ContinuationEvent::UserMessage {
            surface: ContinuationSurface::Append,
            source: ContinuationSource::User,
            content_blocks: 2,
        }
    );
    assert_eq!(human_input("x", 2).clone(), mapped(&event));
}
