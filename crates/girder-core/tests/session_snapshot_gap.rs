//! Tests for the snapshot layer of upstream `packages/api/session-controller`.
//!
//! Corresponds to upstream `tests/manager.client.spec.ts` (control-frame dispatch, the engagement
//! ledger, list mutation replay, lineage rows, projection blocks, title projections),
//! `tests/session-pending-submissions.client.spec.ts` (echo publication, placement, retirement), and
//! `tests/projection-store.client.spec.ts`'s baseline seeding cases as they are reached through the
//! manager.
//!
//! # What each property protects
//!
//! - **A replayed pull is merged, not installed.** A list response predates the frames that landed
//!   while it was in flight; installing it wholesale loses an addition or undoes a removal the user
//!   just watched happen.
//! - **A fenced completion applies nothing.** A reconnect invalidates the pull in flight, so its
//!   rows belong to a generation whose watermarks are not comparable with the current one.
//! - **`phase` never steps back.** One successful pull makes the list `ready` forever; a later
//!   failure is a `state`/`error` fact, not evidence that nothing ever arrived.
//! - **A baseline clears what it omits; a frame clears nothing.** They are different questions
//!   ("what does the host compute at this cut" versus "what did this event change").
//! - **A cached block yields to a sequenced row.** The list's checkpoint view is a zero-I/O
//!   convenience; the moment the host answers for a key, the checkpoint loses.
//! - **A published echo appears only after acceptance.** Publishing earlier shows a message the host
//!   rejected; retiring on rejection without removing it leaves a ghost in the composer.
//! - **The first settlement wins.** A durable message and an inbox row can both report the same
//!   request; the second must be a no-op rather than a second retirement callback.
//! - **A refusal cannot retire an admitted echo.** The message exists, so telling its owner to
//!   restore the local preview would duplicate an attachment.
//! - **Running clears blank and never restores it.** A stopped turn is still durable history.
//! - **A prompt attempt blocks re-blanking.** Opening a conversation keeps it visible against a
//!   stale list row that still says "no turn ever started".
//! - **Row identity survives a rebuild that changed nothing.** Otherwise every memoised row
//!   re-renders on every host frame.
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
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "tests assert by panicking, index their own fixtures, and read best as one scenario"
)]

#[path = "../src/session_snapshot_gap.rs"]
mod session_snapshot_gap;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use girder_core::ProjectionValueStore;
use girder_core::gateway_client_gap::RemoteFailure;
use serde_json::{Value, json};
use session_snapshot_gap::{
    ControlBaseline, ControlFrame, FormatStatus, HintKind, HistoryLoadStatus, ListEngine,
    ListPhase, ListRead, ListState, ListSummary, OpenState, PendingAttachment, PendingImage,
    Placement, ProjectionHints, ProjectionRead, ProjectionState, ProjectionUpdate, PromptOp,
    SessionView, SubagentAddress, SubagentMode, Watermark, seed_baseline,
};

/// A failure with a code and a message, as a Remote call would fold one.
fn failure(code: &str, message: &str) -> RemoteFailure {
    RemoteFailure::new(code, message, serde_json::Map::new())
}

/// A block of projection values.
fn values(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

/// A list row with the fields a Host summary carries.
fn summary(id: &str, updated_at: u64) -> ListSummary {
    ListSummary {
        session_id: id.to_owned(),
        updated_at,
        running: false,
        blank: true,
        agent_available: true,
        parent_session_id: None,
        origin_subagent: false,
        cwd: Some("/work".to_owned()),
        format_status: Some(FormatStatus::Current),
        projections: None,
    }
}

/// A running row: already has a turn.
fn running_summary(id: &str, updated_at: u64) -> ListSummary {
    ListSummary {
        running: true,
        blank: false,
        ..summary(id, updated_at)
    }
}

/// A fresh per-session view with its own store.
fn view(id: &str) -> SessionView {
    SessionView::new(id, Arc::new(ProjectionValueStore::new()))
}

// ---------------------------------------------------------------------------------------------
// Session snapshot
// ---------------------------------------------------------------------------------------------

#[test]
fn a_new_session_is_blank_and_cold() {
    let view = view("s1");
    let snapshot = view.snapshot();
    assert_eq!(snapshot.session_id, "s1");
    assert!(snapshot.blank);
    assert_eq!(snapshot.open_state, OpenState::Cold);
    assert!(!snapshot.running);
    assert!(!snapshot.removed);
    assert!(!snapshot.prompt_attempted);
    assert!(!snapshot.awaiting_first_turn);
    assert!(snapshot.pending_submissions.is_empty());
    assert!(snapshot.subagent.is_none());
    assert_eq!(snapshot.history_load_status, HistoryLoadStatus::Idle);
    assert!(!snapshot.loading_older);
}

#[test]
fn running_clears_blank_and_a_stopped_turn_does_not_restore_it() {
    let view = view("s1");
    assert!(view.handle_running(true));
    let snapshot = view.snapshot();
    assert!(snapshot.running);
    assert!(!snapshot.blank, "a started turn is not a blank session");

    view.handle_running(false);
    let snapshot = view.snapshot();
    assert!(!snapshot.running);
    assert!(
        !snapshot.blank,
        "a stopped turn is still durable history, so blank stays clear"
    );
}

#[test]
fn a_prompt_attempt_blocks_reblanking_but_a_metadata_projection_also_does() {
    // A stale list row says "blank"; the composer must not re-hide an opened conversation.
    let attempted = view("s1");
    attempted.begin_prompt();
    attempted.accept_prompt(None);
    assert!(
        !attempted.handle_blank(true),
        "a non-blank session with a prompt attempt refuses to re-blank"
    );

    // A fresh session accepts the re-blank; the metadata projection can still override it.
    let fresh = view("s2");
    assert!(
        !fresh.handle_blank(true),
        "already blank, so nothing changes"
    );
    assert!(fresh.handle_blank(false));
    assert!(
        fresh.handle_blank(true),
        "no attempt yet, so blank may return"
    );
}

#[test]
fn the_metadata_projection_outranks_a_stale_list_row() {
    let store = Arc::new(ProjectionValueStore::new());
    store.apply(
        "sessionListMetadata",
        json!({ "blank": false, "lastPromptAt": 7 }),
        3,
    );
    let view = SessionView::new("s1", store);
    assert!(
        view.handle_blank(true),
        "the projection overrides the stale row's blank"
    );
    assert!(!view.snapshot().blank);
}

#[test]
fn the_engaging_flag_needs_an_accepted_prompt_and_a_missing_turn_start() {
    let view = view("s1");
    // The flag is set before the receipt but is not observable while the session is still blank.
    view.begin_prompt();
    assert!(!view.snapshot().awaiting_first_turn);

    view.accept_prompt(None);
    let snapshot = view.snapshot();
    assert!(snapshot.prompt_attempted);
    assert!(!snapshot.blank);
    assert!(
        snapshot.awaiting_first_turn,
        "accepted, and no durable turn/start yet"
    );

    // A window that already contains a turn retires the flag.
    view.note_window_replaced(false, true);
    assert!(!view.snapshot().awaiting_first_turn);
}

#[test]
fn observing_running_retires_the_engaging_flag_without_a_turn_start() {
    let view = view("s1");
    view.begin_prompt();
    view.accept_prompt(None);
    assert!(view.snapshot().awaiting_first_turn);
    view.handle_running(true);
    assert!(
        !view.snapshot().awaiting_first_turn,
        "the host admitted the prompt and started running"
    );
}

#[test]
fn a_rejected_first_prompt_leaves_the_session_blank_and_reusable() {
    let view = view("s1");
    view.begin_prompt();
    assert!(view.reject_prompt(failure("session/conflict", "cwd changed")));
    let snapshot = view.snapshot();
    assert!(snapshot.blank, "no turn was ever admitted");
    assert!(!snapshot.awaiting_first_turn);
    assert!(!snapshot.prompt_attempted);
    let error = snapshot.prompt_error.expect("the failure surfaces");
    assert_eq!(error.op, PromptOp::Send);
    assert_eq!(error.failure.code, "session/conflict");
}

#[test]
fn a_new_prompt_attempt_clears_the_previous_error_surfaces() {
    let view = view("s1");
    view.reject_prompt(failure("session/conflict", "cwd changed"));
    view.handle_agent_error("agent died");
    assert!(view.snapshot().prompt_error.is_some());
    assert_eq!(
        view.snapshot().last_agent_error.as_deref(),
        Some("agent died")
    );
    view.begin_prompt();
    let snapshot = view.snapshot();
    assert!(snapshot.prompt_error.is_none());
    assert!(snapshot.last_agent_error.is_none());
}

#[test]
fn a_failed_stop_shares_the_error_strip_slot() {
    let view = view("s1");
    view.fail_stop(failure("session/not-running", "nothing to stop"));
    let error = view.snapshot().prompt_error.expect("surfaced");
    assert_eq!(error.op, PromptOp::Stop);
}

#[test]
fn history_partiality_is_sticky_across_replacements() {
    let view = view("s1");
    view.note_window_replaced(true, true);
    assert!(view.snapshot().history_was_partial);
    assert!(view.snapshot().has_more);

    // A later complete replacement clears `has_more` but not the memory of the partial window.
    view.note_window_replaced(false, false);
    let snapshot = view.snapshot();
    assert!(!snapshot.has_more);
    assert!(
        snapshot.history_was_partial,
        "the client did expose an incomplete window"
    );
}

#[test]
fn loading_older_tracks_the_status_and_pending_consumption_is_not_loading() {
    let view = view("s1");
    view.set_history_load_status(HistoryLoadStatus::Loading);
    assert!(view.snapshot().loading_older);
    view.set_history_load_status(HistoryLoadStatus::PendingConsumption);
    let snapshot = view.snapshot();
    assert!(!snapshot.loading_older, "a held page must stop the spinner");
    assert_eq!(
        snapshot.history_load_status,
        HistoryLoadStatus::PendingConsumption
    );
}

#[test]
fn an_open_failure_keeps_the_failure_for_the_error_surface() {
    let view = view("s1");
    view.begin_open();
    assert_eq!(view.snapshot().open_state, OpenState::Loading);
    view.open_failed(failure("session/format-unsupported", "v9"));
    let snapshot = view.snapshot();
    assert_eq!(snapshot.open_state, OpenState::Error);
    assert_eq!(
        snapshot
            .open_error
            .as_ref()
            .map(|error| error.code.as_str()),
        Some("session/format-unsupported")
    );

    // A successful open clears the error.
    view.begin_open();
    view.opened();
    let snapshot = view.snapshot();
    assert_eq!(snapshot.open_state, OpenState::Open);
    assert!(snapshot.open_error.is_none());
}

#[test]
fn removal_is_reported_and_sticky() {
    let view = view("s1");
    assert!(view.handle_removed());
    assert!(!view.handle_removed(), "already removed");
    assert!(view.snapshot().removed);
}

#[test]
fn a_changed_address_resyncs_only_an_open_window() {
    let view = view("s1");
    let address = SubagentAddress {
        parent_session_id: "parent".to_owned(),
        child_session_id: "s1".to_owned(),
        mode: SubagentMode::Continuable,
    };
    let cold = view.configure_subagent(Some(address.clone()));
    assert!(cold.changed);
    assert!(!cold.resync, "a cold window has nothing to rebuild");

    view.begin_open();
    view.opened();
    let same = view.configure_subagent(Some(address.clone()));
    assert!(!same.changed);
    assert!(!same.resync);

    let different = view.configure_subagent(Some(SubagentAddress {
        mode: SubagentMode::OneShot,
        ..address
    }));
    assert!(different.changed);
    assert!(
        different.resync,
        "the same id through a new route is a new log"
    );
}

#[test]
fn an_unknown_address_mode_is_retyped_from_the_identity_projection() {
    let store = Arc::new(ProjectionValueStore::new());
    store.apply("subagent", json!({ "mode": "continuable" }), 4);
    let view = SessionView::new("child", store);
    view.configure_subagent(Some(SubagentAddress {
        parent_session_id: "parent".to_owned(),
        child_session_id: "child".to_owned(),
        mode: SubagentMode::Unknown,
    }));
    assert_eq!(
        view.snapshot().subagent.map(|address| address.mode),
        Some(SubagentMode::Continuable)
    );
}

#[test]
fn a_subscriber_is_notified_once_per_batch_and_reads_a_fresh_snapshot() {
    let view = Arc::new(view("s1"));
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let seen_running = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&seen_running);
    let observed = Arc::clone(&view);
    let _subscription = view.subscribe(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        if observed.snapshot().running {
            flag.fetch_add(1, Ordering::SeqCst);
        }
    });

    // Three writes, one publication.
    view.begin_open();
    view.opened();
    view.handle_running(true);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "batched, not synchronous");
    view.flush();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        seen_running.load(Ordering::SeqCst),
        1,
        "the rebuild runs before the listener, so the listener sees the change"
    );
}

#[test]
fn a_reader_rebuilds_a_dirty_snapshot_without_consuming_the_notification() {
    let view = view("s1");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let _subscription = view.subscribe(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    view.handle_running(true);
    // A read while dirty rebuilds synchronously; nobody has been told yet.
    assert!(view.snapshot().running);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    view.flush();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------------------------
// Prompt echoes
// ---------------------------------------------------------------------------------------------

#[test]
fn an_echo_is_published_only_after_the_host_accepts_it() {
    let view = view("s1");
    let placement = view.begin_submission("req-1", PromptOp::Send, "hello", Vec::new(), 100);
    assert_eq!(placement, Placement::Transcript);
    assert!(
        view.snapshot().pending_submissions.is_empty(),
        "registered, but not accepted"
    );

    view.accept_prompt(Some("req-1"));
    let pending = view.snapshot().pending_submissions;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].request_id, "req-1");
    assert_eq!(pending[0].text, "hello");
    assert_eq!(pending[0].placement, Placement::Transcript);
}

#[test]
fn the_second_acceptance_proof_is_a_no_op() {
    let view = view("s1");
    view.begin_submission("req-1", PromptOp::Send, "hello", Vec::new(), 100);
    assert!(view.accept_prompt(Some("req-1")));
    assert!(
        !view.accept_prompt(Some("req-1")),
        "a receipt and an inbox row can both report the same acceptance"
    );
}

#[test]
fn placement_is_fixed_at_submission_time() {
    let view = view("s1");
    assert_eq!(
        view.begin_submission("idle", PromptOp::Send, "a", Vec::new(), 1),
        Placement::Transcript
    );
    view.handle_running(true);
    assert_eq!(
        view.begin_submission("queued", PromptOp::Send, "b", Vec::new(), 2),
        Placement::Queued
    );
    assert_eq!(
        view.begin_submission("steer", PromptOp::Stop, "c", Vec::new(), 3),
        Placement::Steering
    );
}

#[test]
fn an_accepted_echo_retires_exactly_once() {
    let view = view("s1");
    view.begin_submission("req-1", PromptOp::Send, "hello", Vec::new(), 100);
    view.accept_prompt(Some("req-1"));
    assert!(view.retire_submission("req-1", true));
    assert!(view.snapshot().pending_submissions.is_empty());
    assert!(
        !view.retire_submission("req-1", true),
        "the settlement latched, so a second observer cannot report it again"
    );
}

#[test]
fn an_unaccepted_echo_stays_hidden_and_retires_as_failed() {
    let view = view("s1");
    let _ = view.begin_submission("req-1", PromptOp::Send, "hello", Vec::new(), 100);
    assert!(view.snapshot().pending_submissions.is_empty());
    assert!(view.abandon_submission("req-1"));
    assert!(view.snapshot().pending_submissions.is_empty());
}

#[test]
fn a_refusal_cannot_retire_an_admitted_echo() {
    let view = view("s1");
    view.begin_submission("req-1", PromptOp::Send, "hello", Vec::new(), 100);
    view.accept_prompt(Some("req-1"));
    assert!(view.note_submission_admitted("req-1"));
    assert!(
        !view.abandon_submission("req-1"),
        "the durable message exists, so the owner must not restore its preview"
    );
    assert_eq!(
        view.snapshot().pending_submissions.len(),
        1,
        "the echo stays until the observation path retires it"
    );
    assert!(view.retire_submission("req-1", true));
}

#[test]
fn an_echo_keeps_its_attachments_in_prompt_order() {
    let view = view("s1");
    let attachments = vec![
        PendingAttachment::Image(PendingImage {
            preview_url: "blob:first".to_owned(),
            name: Some("a.png".to_owned()),
            width: Some(64),
            height: Some(32),
        }),
        PendingAttachment::File {
            attachment_id: "att-1".to_owned(),
            name: "notes.pdf".to_owned(),
            bytes: 11,
        },
    ];
    view.begin_submission(
        "req-1",
        PromptOp::Send,
        "see attached",
        attachments.clone(),
        5,
    );
    view.accept_prompt(Some("req-1"));
    let pending = view.snapshot().pending_submissions;
    assert_eq!(pending[0].attachments, attachments);
}

#[test]
fn rapid_submissions_retire_independently() {
    let view = view("s1");
    for id in ["a", "b", "c"] {
        view.begin_submission(id, PromptOp::Send, id, Vec::new(), 1);
    }
    view.accept_prompt(Some("a"));
    view.accept_prompt(Some("c"));
    let ids: Vec<String> = view
        .snapshot()
        .pending_submissions
        .into_iter()
        .map(|echo| echo.request_id)
        .collect();
    assert_eq!(ids, ["a", "c"], "insertion order, accepted only");
    assert!(view.retire_submission("a", true));
    let ids: Vec<String> = view
        .snapshot()
        .pending_submissions
        .into_iter()
        .map(|echo| echo.request_id)
        .collect();
    assert_eq!(ids, ["c"], "retiring one leaves the others alone");
}

// ---------------------------------------------------------------------------------------------
// Projection stores through the manager
// ---------------------------------------------------------------------------------------------

#[test]
fn a_baseline_seeds_a_real_cut_and_clears_what_it_omits() {
    let store = Arc::new(ProjectionValueStore::new());
    store.apply("stale", json!(1), 2);
    store.apply("kept", json!(1), 2);
    let removed = seed_baseline(
        &store,
        Watermark::At(9),
        &values(&[("kept", json!(2)), ("fresh", json!(3))]),
    );
    assert_eq!(removed, ["stale"], "the cut's omission is information");
    assert_eq!(store.value("kept"), Some(json!(2)));
    assert_eq!(store.value("fresh"), Some(json!(3)));
    assert_eq!(store.value("stale"), None);
}

#[test]
fn a_newer_frame_survives_a_stale_baseline() {
    let store = Arc::new(ProjectionValueStore::new());
    store.apply("title", json!("newer"), 20);
    let removed = seed_baseline(&store, Watermark::At(5), &values(&[]));
    assert!(
        removed.is_empty(),
        "the frame's seq is above the cut, so neither overwriting nor clearing is correct"
    );
    assert_eq!(store.value("title"), Some(json!("newer")));
}

#[test]
fn the_empty_cut_sentinel_yields_to_every_real_frame() {
    let store = Arc::new(ProjectionValueStore::new());
    seed_baseline(
        &store,
        Watermark::BeforeFirstEvent,
        &values(&[("sessionListMetadata", json!({ "blank": true }))]),
    );
    assert!(store.contains("sessionListMetadata"));
    assert!(
        store.is_cached("sessionListMetadata"),
        "the sentinel cannot be a sequenced watermark below seq 0"
    );

    // Any real cut, including the very first event, outranks it.
    store.apply("sessionListMetadata", json!({ "blank": false }), 0);
    assert_eq!(store.seq_of("sessionListMetadata"), Some(0));
    assert_eq!(
        store.value("sessionListMetadata"),
        Some(json!({ "blank": false }))
    );
}

#[test]
fn the_empty_cut_sweeps_cached_rows_and_keeps_sequenced_ones_above_it() {
    let store = Arc::new(ProjectionValueStore::new());
    store.apply_cached(&values(&[("cold", json!("cached"))]));
    store.apply("live", json!("host"), 4);
    let removed = seed_baseline(
        &store,
        Watermark::BeforeFirstEvent,
        &values(&[("live", json!("ignored"))]),
    );
    assert_eq!(removed, ["cold"]);
    assert_eq!(
        store.value("live"),
        Some(json!("host")),
        "a sequenced row outranks the empty cut"
    );
}

#[test]
fn the_watermark_reader_distinguishes_the_empty_cut_from_an_illegal_one() {
    assert_eq!(Watermark::from_wire(-1), Some(Watermark::BeforeFirstEvent));
    assert_eq!(Watermark::from_wire(0), Some(Watermark::At(0)));
    assert_eq!(Watermark::from_wire(12), Some(Watermark::At(12)));
    assert_eq!(Watermark::from_wire(-2), None);
    assert_eq!(Watermark::BeforeFirstEvent.seq(), None);
    assert_eq!(Watermark::At(0).seq(), Some(0));
}

// ---------------------------------------------------------------------------------------------
// Control frames
// ---------------------------------------------------------------------------------------------

#[test]
fn a_baseline_frame_seeds_every_session_and_reports_cleared_keys() {
    let engine = ListEngine::new();
    // A cold session's title lands before the session itself is opened.
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "s1".to_owned(),
        seq: 3,
        changes: vec![("title".to_owned(), json!("first"))],
    }));
    assert_eq!(
        engine
            .projection_values("s1")
            .and_then(|v| v.get("title").cloned()),
        Some(json!("first"))
    );

    let mut projections = BTreeMap::new();
    projections.insert(
        "s1".to_owned(),
        (Watermark::At(10), values(&[("title", json!("second"))])),
    );
    let removed =
        engine.handle_control_frame(&ControlFrame::Baseline(ControlBaseline { projections }));
    assert!(removed.is_empty());
    assert_eq!(
        engine
            .projection_values("s1")
            .and_then(|v| v.get("title").cloned()),
        Some(json!("second"))
    );
}

#[test]
fn a_per_event_frame_applies_every_key_under_one_watermark() {
    let engine = ListEngine::new();
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "s1".to_owned(),
        seq: 7,
        changes: vec![
            ("title".to_owned(), json!("renamed")),
            ("sessionListMetadata".to_owned(), json!({ "blank": false })),
        ],
    }));
    let held = engine.projection_values("s1").expect("a store exists");
    assert_eq!(held.get("title"), Some(&json!("renamed")));
    assert_eq!(
        held.get("sessionListMetadata"),
        Some(&json!({ "blank": false }))
    );
}

#[test]
fn a_replayed_frame_does_not_regress_a_value() {
    let engine = ListEngine::new();
    let update = ProjectionUpdate {
        session_id: "s1".to_owned(),
        seq: 8,
        changes: vec![("title".to_owned(), json!("new"))],
    };
    engine.handle_control_frame(&ControlFrame::Projection(update.clone()));
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "s1".to_owned(),
        seq: 4,
        changes: vec![("title".to_owned(), json!("old"))],
    }));
    assert_eq!(
        engine
            .projection_values("s1")
            .and_then(|v| v.get("title").cloned()),
        Some(json!("new")),
        "the lower seq loses"
    );
}

// ---------------------------------------------------------------------------------------------
// List pull, mutations, phase
// ---------------------------------------------------------------------------------------------

#[test]
fn the_first_pull_establishes_the_order_and_phase() {
    let engine = ListEngine::new();
    assert_eq!(engine.snapshot().phase, ListPhase::Pending);
    let ticket = engine.begin_list_refresh().expect("first pull");
    assert_eq!(engine.snapshot().state, ListState::Loading);
    assert!(engine.complete_list_refresh(
        ticket,
        Ok(ListRead {
            items: vec![summary("a", 1), summary("b", 2)],
        })
    ));
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.phase, ListPhase::Ready);
    assert_eq!(snapshot.state, ListState::Idle);
    let ids: Vec<&str> = snapshot
        .items()
        .iter()
        .map(|row| row.session_id.as_str())
        .collect();
    assert_eq!(ids, ["a", "b"]);
}

#[test]
fn a_second_pull_cannot_start_while_the_first_is_in_flight() {
    let engine = ListEngine::new();
    assert!(engine.begin_list_refresh().is_some());
    assert!(
        engine.begin_list_refresh().is_none(),
        "two pulls would each install a baseline, and the later one may be older"
    );
}

#[test]
fn a_pull_preserves_established_order_and_inserts_unknown_rows_after_their_neighbour() {
    let engine = ListEngine::new();
    let first = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        first,
        Ok(ListRead {
            items: vec![summary("a", 1), summary("b", 2), summary("c", 3)],
        }),
    );
    let second = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        second,
        Ok(ListRead {
            // The host re-ordered: `c` first. Established order must survive.
            items: vec![
                summary("c", 3),
                summary("new", 9),
                summary("a", 1),
                summary("b", 2),
            ],
        }),
    );
    let ids: Vec<String> = engine
        .snapshot()
        .items()
        .iter()
        .map(|row| row.session_id.clone())
        .collect();
    assert_eq!(
        ids,
        ["new", "a", "b", "c"],
        "the established order survives, and a new row lands before the identity that follows it"
    );
}

#[test]
fn a_pull_drops_identities_the_host_no_longer_lists() {
    let engine = ListEngine::new();
    let first = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        first,
        Ok(ListRead {
            items: vec![summary("a", 1), summary("b", 2)],
        }),
    );
    let second = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        second,
        Ok(ListRead {
            items: vec![summary("a", 1)],
        }),
    );
    assert_eq!(engine.snapshot().items().len(), 1);
}

#[test]
fn a_mutation_during_a_pull_is_replayed_over_the_response() {
    let engine = ListEngine::new();
    let first = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        first,
        Ok(ListRead {
            items: vec![summary("a", 1)],
        }),
    );

    let ticket = engine.begin_list_refresh().unwrap();
    // A removal the user already watched land arrives while the response is in flight.
    engine.handle_session_removed("a");
    assert!(engine.snapshot().items().is_empty());
    engine.complete_list_refresh(
        ticket,
        Ok(ListRead {
            items: vec![summary("a", 1)],
        }),
    );
    assert!(
        engine.snapshot().items().is_empty(),
        "the response predates the removal, so installing it would resurrect the row"
    );
}

#[test]
fn a_fenced_completion_applies_nothing() {
    let engine = ListEngine::new();
    let stale = engine.begin_list_refresh().unwrap();
    engine.cancel_list_refresh();
    assert!(
        !engine.complete_list_refresh(
            stale,
            Ok(ListRead {
                items: vec![summary("a", 1)]
            })
        ),
        "the pull belongs to a generation the client abandoned"
    );
    assert_eq!(engine.snapshot().phase, ListPhase::Pending);
    assert!(engine.snapshot().items().is_empty());
}

#[test]
fn a_failed_pull_keeps_the_error_and_never_resets_the_phase() {
    let engine = ListEngine::new();
    let first = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        first,
        Ok(ListRead {
            items: vec![summary("a", 1)],
        }),
    );

    let failing = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(failing, Err(failure("gateway/offline", "socket closed")));
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.state, ListState::Error);
    assert_eq!(
        snapshot.error.as_ref().map(|error| error.code.as_str()),
        Some("gateway/offline")
    );
    assert_eq!(snapshot.phase, ListPhase::Ready, "phase is monotone");
    assert_eq!(snapshot.items().len(), 1, "a failure does not erase rows");

    // A later success clears the error.
    let retry = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        retry,
        Ok(ListRead {
            items: vec![summary("a", 1)],
        }),
    );
    assert_eq!(engine.snapshot().state, ListState::Idle);
    assert!(engine.snapshot().error.is_none());
}

#[test]
fn a_removal_during_a_pull_supersedes_running_reported_by_it() {
    let engine = ListEngine::new();
    let first = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        first,
        Ok(ListRead {
            items: vec![summary("a", 1)],
        }),
    );

    let ticket = engine.begin_list_refresh().unwrap();
    engine.handle_session_removed("a");
    engine.complete_list_refresh(
        ticket,
        Ok(ListRead {
            items: vec![running_summary("a", 1)],
        }),
    );
    assert!(
        engine.snapshot().items().is_empty(),
        "the removal the user already watched supersedes the baseline that predates it"
    );

    // The superseded baseline's running bit must not have engaged the ledger either: a later blank
    // row for the same identity stays blank.
    engine.handle_session_added(summary("a", 1));
    assert!(
        engine.snapshot().items()[0].blank,
        "running from a superseded baseline must not resurrect the row's liveness"
    );
}

// ---------------------------------------------------------------------------------------------
// Engagement, blank, and running
// ---------------------------------------------------------------------------------------------

#[test]
fn engagement_survives_a_stale_pull_that_still_says_blank() {
    let engine = ListEngine::new();
    let first = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        first,
        Ok(ListRead {
            items: vec![summary("a", 1)],
        }),
    );

    // The sender flipped locally: the row surfaces before any host frame.
    assert!(engine.engage("a"));
    assert!(!engine.snapshot().items()[0].blank);

    // A subsequent pull's row still says blank; engagement must outlive it.
    let second = engine.begin_list_refresh().unwrap();
    engine.complete_list_refresh(
        second,
        Ok(ListRead {
            items: vec![summary("a", 1)],
        }),
    );
    assert!(
        !engine.snapshot().items()[0].blank,
        "the ledger remembers the acceptance"
    );
}

#[test]
fn engaging_an_unknown_identity_is_refused() {
    let engine = ListEngine::new();
    assert!(
        !engine.engage("ghost"),
        "an engagement belongs to a session somebody is listing"
    );
}

#[test]
fn a_running_row_engages_the_ledger_and_a_status_flip_clears_blank() {
    let engine = ListEngine::new();
    engine.handle_session_added(running_summary("a", 1));
    let row = engine.snapshot().items()[0].clone();
    assert!(row.running);
    assert!(!row.blank, "running converts display state");

    engine.handle_session_status("a", false);
    let row = engine.snapshot().items()[0].clone();
    assert!(!row.running);
    assert!(
        !row.blank,
        "a stopped turn does not restore blank; the history is durable"
    );
}

#[test]
fn a_placeholder_fills_metadata_without_overwriting_live_state() {
    let engine = ListEngine::new();
    engine.handle_session_added(running_summary("a", 5));
    // Create/fork mints a blank placeholder for the same identity.
    engine.add_placeholder("a", Some("/other".to_owned()), 9);
    let row = engine.snapshot().items()[0].clone();
    assert!(row.running, "a local placeholder cannot un-run a live turn");
    assert!(!row.blank);
    assert_eq!(
        row.cwd.as_deref(),
        Some("/work"),
        "an existing cwd is not overwritten"
    );
}

#[test]
fn a_placeholder_for_a_new_identity_appears_immediately() {
    let engine = ListEngine::new();
    engine.add_placeholder("fresh", Some("/work".to_owned()), 42);
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.items().len(), 1);
    assert!(snapshot.items()[0].blank);
    assert_eq!(snapshot.items()[0].updated_at, 42);
    assert_eq!(snapshot.phase, ListPhase::Pending, "no pull has landed");
}

#[test]
fn activity_only_advances_recency() {
    let engine = ListEngine::new();
    engine.handle_session_added(summary("a", 10));
    engine.handle_session_activity("a", 4);
    assert_eq!(
        engine.snapshot().items()[0].updated_at,
        10,
        "an older message cannot move a row backwards"
    );
    engine.handle_session_activity("a", 20);
    assert_eq!(engine.snapshot().items()[0].updated_at, 20);
}

#[test]
fn the_metadata_projection_corrects_recency_and_blank_in_the_list() {
    let engine = ListEngine::new();
    engine.handle_session_added(summary("a", 10));
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "a".to_owned(),
        seq: 3,
        changes: vec![(
            "sessionListMetadata".to_owned(),
            json!({ "blank": false, "lastPromptAt": 99 }),
        )],
    }));
    let row = engine.snapshot().items()[0].clone();
    assert_eq!(row.updated_at, 99, "the projection knows the prompt time");
    assert!(!row.blank, "the projection outranks the row's own bit");
}

#[test]
fn a_removed_child_stays_listed_when_a_catalog_holds_it() {
    let engine = ListEngine::new();
    engine.handle_session_added(ListSummary {
        parent_session_id: Some("parent".to_owned()),
        origin_subagent: true,
        ..summary("child", 1)
    });
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "parent".to_owned(),
        seq: 2,
        changes: vec![(
            "subagentCatalog".to_owned(),
            json!([{ "id": "child", "mode": "continuable" }]),
        )],
    }));
    engine.handle_session_removed("child");
    let ids: Vec<String> = engine
        .snapshot()
        .items()
        .iter()
        .map(|row| row.session_id.clone())
        .collect();
    assert_eq!(
        ids,
        ["child"],
        "a durable subagent child is governed by its parent's catalog"
    );
    assert!(!engine.snapshot().items()[0].running);
}

#[test]
fn a_plain_removal_drops_the_row_and_its_store() {
    let engine = ListEngine::new();
    engine.handle_session_added(summary("a", 1));
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "a".to_owned(),
        seq: 2,
        changes: vec![("title".to_owned(), json!("gone"))],
    }));
    engine.handle_session_removed("a");
    assert!(engine.snapshot().items().is_empty());
    assert!(
        engine.projection_values("a").is_none(),
        "nothing else holds the identity, so its store goes too"
    );
}

// ---------------------------------------------------------------------------------------------
// Lineage
// ---------------------------------------------------------------------------------------------

#[test]
fn lineage_indents_children_under_their_parent() {
    let engine = ListEngine::new();
    engine.handle_session_added(summary("root", 1));
    engine.handle_session_added(ListSummary {
        parent_session_id: Some("root".to_owned()),
        origin_subagent: true,
        ..summary("child", 2)
    });
    // A later addition becomes the newest row, so it stays above the family rather than splitting it.
    engine.handle_session_added(summary("other", 3));
    let rows = engine.snapshot().items().to_vec();
    let ids: Vec<String> = rows.iter().map(|row| row.session_id.clone()).collect();
    assert_eq!(
        ids,
        ["other", "root", "child"],
        "a child stays adjacent to its parent, in the parent's position"
    );
    assert_eq!(rows[0].depth, 0);
    assert_eq!(rows[1].depth, 0);
    assert_eq!(rows[2].depth, 1);
}

#[test]
fn an_ordinary_fork_is_not_indented() {
    let engine = ListEngine::new();
    engine.handle_session_added(summary("root", 1));
    // A historical fork keeps a parent id without being a subagent.
    engine.handle_session_added(ListSummary {
        parent_session_id: Some("root".to_owned()),
        origin_subagent: false,
        ..summary("fork", 2)
    });
    let rows = engine.snapshot().items().to_vec();
    assert_eq!(rows[1].depth, 0);
}

// ---------------------------------------------------------------------------------------------
// Row identity
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unchanged_row_set_keeps_its_allocation() {
    let engine = ListEngine::new();
    engine.handle_session_added(summary("a", 1));
    let first = engine.snapshot();
    // A replayed frame the store already rejected changes no value, so no row changes either.
    let replay = ProjectionUpdate {
        session_id: "a".to_owned(),
        seq: 5,
        changes: vec![("title".to_owned(), json!("same"))],
    };
    engine.handle_control_frame(&ControlFrame::Projection(replay.clone()));
    let titled = engine.snapshot();
    assert!(
        !Arc::ptr_eq(&first.items, &titled.items),
        "the title landed"
    );
    engine.handle_control_frame(&ControlFrame::Projection(replay));
    let second = engine.snapshot();
    assert!(
        Arc::ptr_eq(&titled.items, &second.items),
        "a replayed frame changes nothing, so no memoised row component re-renders"
    );

    // A change a row renders does mint a new allocation.
    engine.handle_session_activity("a", 50);
    let third = engine.snapshot();
    assert!(!Arc::ptr_eq(&second.items, &third.items));
    assert_eq!(third.items()[0].updated_at, 50);
}

// ---------------------------------------------------------------------------------------------
// Instances
// ---------------------------------------------------------------------------------------------

#[test]
fn an_instance_is_built_lazily_and_keeps_the_same_identity() {
    let engine = ListEngine::new();
    assert!(!engine.is_resident("a"));
    let first = engine.get("a");
    assert!(engine.is_resident("a"));
    let second = engine.get("a");
    assert!(Arc::ptr_eq(&first, &second), "one instance per identity");
}

#[test]
fn a_new_instance_adopts_the_lists_blank_and_running_bits() {
    let engine = ListEngine::new();
    engine.handle_session_added(running_summary("a", 1));
    let view = engine.get("a");
    let snapshot = view.snapshot();
    assert!(snapshot.running);
    assert!(!snapshot.blank);
}

#[test]
fn dropping_a_replaced_instance_leaves_the_replacement_alone() {
    let engine = ListEngine::new();
    let resident = engine.get("a");
    engine.drop_session("a", &resident).expect("dropped");
    let replacement = engine.get("a");
    let stale = Arc::new(SessionView::new("a", Arc::new(ProjectionValueStore::new())));
    assert!(
        engine.drop_session("a", &stale).is_none(),
        "an old stream's teardown must not drop the new instance"
    );
    assert!(Arc::ptr_eq(&engine.get("a"), &replacement));
}

#[test]
fn disposal_hands_back_every_resident_instance() {
    let engine = ListEngine::new();
    let _ = engine.get("a");
    let _ = engine.get("b");
    let mut residents = engine.dispose();
    residents.sort_by(|left, right| left.session_id().cmp(right.session_id()));
    assert_eq!(residents.len(), 2);
    assert_eq!(residents[0].session_id(), "a");
    assert!(!engine.is_resident("a"));
    assert!(
        !engine.engage("a"),
        "a disposed cluster applies no further row mutations"
    );
}

#[test]
fn a_disposed_cluster_ignores_late_completions() {
    let engine = ListEngine::new();
    let ticket = engine.begin_list_refresh().unwrap();
    engine.dispose();
    assert!(!engine.complete_list_refresh(
        ticket,
        Ok(ListRead {
            items: vec![summary("a", 1)]
        })
    ));
    assert!(engine.snapshot().items().is_empty());
}

#[test]
fn resolve_target_refuses_an_unknown_identity() {
    let engine = ListEngine::new();
    let error = engine.resolve_target("ghost", None).expect_err("refused");
    assert!(error.contains("unknown session ghost"));
    engine.handle_session_added(summary("a", 1));
    assert_eq!(
        engine.resolve_target("a", None).expect("retained"),
        "a",
        "a listed row proves the identity exists"
    );
}

#[test]
fn resolve_target_retains_an_address_and_installs_it() {
    let engine = ListEngine::new();
    let view = engine.get("child");
    let address = SubagentAddress {
        parent_session_id: "parent".to_owned(),
        child_session_id: "child".to_owned(),
        mode: SubagentMode::Continuable,
    };
    assert_eq!(
        engine
            .resolve_target("child", Some(address.clone()))
            .unwrap(),
        "child"
    );
    assert_eq!(view.snapshot().subagent, Some(address.clone()));
    assert_eq!(engine.subagent_address("child"), Some(address));
}

#[test]
fn an_address_is_derived_from_a_catalog_but_not_retained() {
    let engine = ListEngine::new();
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "parent".to_owned(),
        seq: 5,
        changes: vec![(
            "subagentCatalog".to_owned(),
            json!([
                { "id": "child", "mode": "one-shot" },
                { "id": "external-child", "mode": "external" }
            ]),
        )],
    }));
    assert_eq!(
        engine.subagent_address("child"),
        Some(SubagentAddress {
            parent_session_id: "parent".to_owned(),
            child_session_id: "child".to_owned(),
            mode: SubagentMode::OneShot,
        })
    );
    assert_eq!(
        engine.subagent_address("external-child"),
        None,
        "an externally delegated child read through the parent would show the wrong log"
    );
}

#[test]
fn a_listed_child_without_a_mode_resolves_as_unknown() {
    let engine = ListEngine::new();
    engine.handle_session_added(ListSummary {
        parent_session_id: Some("parent".to_owned()),
        origin_subagent: true,
        ..summary("child", 1)
    });
    assert_eq!(
        engine.subagent_address("child").map(|address| address.mode),
        Some(SubagentMode::Unknown),
        "a header-only row carries no mode, and the address is still usable for navigation"
    );
}

// ---------------------------------------------------------------------------------------------
// Projection blocks
// ---------------------------------------------------------------------------------------------

#[test]
fn a_cached_block_fills_a_cold_title_and_yields_to_a_sequenced_row() {
    let engine = ListEngine::new();
    engine.handle_session_added(ListSummary {
        projections: Some(ProjectionHints {
            kind: HintKind::Cached,
            watermark: Watermark::At(2),
            values: values(&[("title", json!("cold title"))]),
        }),
        ..summary("a", 1)
    });
    assert_eq!(
        engine.snapshot().items()[0].title.as_deref(),
        Some("cold title"),
        "a title surfaces without opening the session"
    );

    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "a".to_owned(),
        seq: 6,
        changes: vec![("title".to_owned(), json!("live title"))],
    }));
    assert_eq!(
        engine.snapshot().items()[0].title.as_deref(),
        Some("live title")
    );

    // A later cached block cannot take the key back.
    engine.handle_session_added(ListSummary {
        projections: Some(ProjectionHints {
            kind: HintKind::Cached,
            watermark: Watermark::At(99),
            values: values(&[("title", json!("stale checkpoint"))]),
        }),
        ..summary("a", 1)
    });
    assert_eq!(
        engine.snapshot().items()[0].title.as_deref(),
        Some("live title"),
        "the checkpoint view never outranks the host"
    );
}

#[test]
fn a_sequenced_block_lands_under_higher_seq_wins() {
    let engine = ListEngine::new();
    engine.handle_session_added(ListSummary {
        projections: Some(ProjectionHints {
            kind: HintKind::Sequenced,
            watermark: Watermark::At(10),
            values: values(&[("title", json!("ten"))]),
        }),
        ..summary("a", 1)
    });
    engine.handle_session_added(ListSummary {
        projections: Some(ProjectionHints {
            kind: HintKind::Sequenced,
            watermark: Watermark::At(4),
            values: values(&[("title", json!("four"))]),
        }),
        ..summary("a", 1)
    });
    assert_eq!(
        engine.snapshot().items()[0].title.as_deref(),
        Some("ten"),
        "the lower watermark loses"
    );
}

#[test]
fn a_rename_settles_the_row_without_opening_the_session() {
    let engine = ListEngine::new();
    engine.handle_session_added(summary("a", 1));
    engine.apply_title("a", "renamed", 12);
    assert_eq!(
        engine.snapshot().items()[0].title.as_deref(),
        Some("renamed")
    );

    // The control frame that follows is a no-op replay.
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "a".to_owned(),
        seq: 9,
        changes: vec![("title".to_owned(), json!("older"))],
    }));
    assert_eq!(
        engine.snapshot().items()[0].title.as_deref(),
        Some("renamed")
    );
}

// ---------------------------------------------------------------------------------------------
// Projection reads
// ---------------------------------------------------------------------------------------------

#[test]
fn a_projection_read_is_single_flighted_and_ready_rows_do_not_reread() {
    let engine = ListEngine::new();
    let ticket = engine.begin_projection_read("parent").expect("first read");
    assert!(
        engine.begin_projection_read("parent").is_none(),
        "the second caller shares the in-flight read"
    );
    assert_eq!(
        engine.snapshot().projections_by_session["parent"].state,
        ProjectionState::Loading
    );
    assert!(engine.complete_projection_read(
        ticket,
        ProjectionRead::Sequenced {
            watermark: Watermark::At(3),
            values: values(&[("title", json!("parent title"))]),
        }
    ));
    assert_eq!(
        engine.snapshot().projections_by_session["parent"].state,
        ProjectionState::Ready
    );
    assert!(
        engine.begin_projection_read("parent").is_none(),
        "a ready row needs no re-read"
    );
}

#[test]
fn a_projection_read_failure_is_retried() {
    let engine = ListEngine::new();
    let ticket = engine.begin_projection_read("parent").unwrap();
    engine.complete_projection_read(
        ticket,
        ProjectionRead::Failed(failure("gateway/offline", "down")),
    );
    let read = engine.snapshot().projections_by_session["parent"].clone();
    assert_eq!(read.state, ProjectionState::Error);
    assert_eq!(
        read.error.as_ref().map(|error| error.code.as_str()),
        Some("gateway/offline")
    );
    assert!(
        engine.begin_projection_read("parent").is_some(),
        "a failed read may be retried"
    );
}

#[test]
fn a_migration_required_read_lands_as_cached_rows() {
    let engine = ListEngine::new();
    let ticket = engine.begin_projection_read("old").unwrap();
    engine.complete_projection_read(
        ticket,
        ProjectionRead::MigrationRequired {
            values: values(&[("title", json!("checkpoint"))]),
        },
    );
    let read = engine.snapshot().projections_by_session["old"].clone();
    assert_eq!(read.state, ProjectionState::MigrationRequired);
    assert_eq!(read.values.get("title"), Some(&json!("checkpoint")));
}

#[test]
fn an_absent_read_does_not_erase_a_value_that_arrived_meanwhile() {
    let engine = ListEngine::new();
    let ticket = engine.begin_projection_read("parent").unwrap();
    // A frame proves existence while the (stale) read is in flight.
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "parent".to_owned(),
        seq: 2,
        changes: vec![("title".to_owned(), json!("live"))],
    }));
    engine.complete_projection_read(ticket, ProjectionRead::Absent);
    assert_eq!(
        engine
            .projection_values("parent")
            .and_then(|v| v.get("title").cloned()),
        Some(json!("live")),
        "a missing read is older news than a frame"
    );
}

#[test]
fn a_projection_read_fenced_by_a_reconnect_applies_nothing() {
    let engine = ListEngine::new();
    let ticket = engine.begin_projection_read("parent").unwrap();
    let _ = engine.handle_connected();
    assert!(
        !engine.complete_projection_read(
            ticket,
            ProjectionRead::Sequenced {
                watermark: Watermark::At(40),
                values: values(&[("title", json!("previous generation"))]),
            }
        ),
        "a watermark from another connection is not comparable with this one's"
    );
    assert!(
        engine
            .projection_values("parent")
            .is_none_or(|held| held.is_empty())
    );
}

// ---------------------------------------------------------------------------------------------
// Reconnect
// ---------------------------------------------------------------------------------------------

#[test]
fn a_reconnect_clears_every_cut_and_names_the_parents_to_refresh() {
    let engine = ListEngine::new();
    engine
        .resolve_target(
            "child",
            Some(SubagentAddress {
                parent_session_id: "parent".to_owned(),
                child_session_id: "child".to_owned(),
                mode: SubagentMode::Continuable,
            }),
        )
        .expect("the address itself proves the identity exists");
    let _ = engine.get("child");
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "parent".to_owned(),
        seq: 9,
        changes: vec![("title".to_owned(), json!("parent"))],
    }));

    let plan = engine.handle_connected();
    assert_eq!(
        plan.refresh_projections,
        ["parent".to_owned()].into_iter().collect()
    );
    assert!(
        engine
            .projection_values("parent")
            .expect("store retained")
            .is_empty(),
        "a cut from the previous connection is discarded, not kept"
    );
    assert!(
        engine.is_resident("child"),
        "a reconnect repairs baselines; it does not evict instances"
    );
}

#[test]
fn a_reconnect_fences_the_pull_in_flight() {
    let engine = ListEngine::new();
    let ticket = engine.begin_list_refresh().unwrap();
    let _ = engine.handle_connected();
    assert!(!engine.complete_list_refresh(
        ticket,
        Ok(ListRead {
            items: vec![summary("a", 1)]
        })
    ));
    assert!(
        engine.begin_list_refresh().is_some(),
        "the new generation may pull again"
    );
}

// ---------------------------------------------------------------------------------------------
// Subscription plumbing and remaining surfaces
// ---------------------------------------------------------------------------------------------

#[test]
fn the_list_engine_publishes_once_per_batch_and_unsubscribes_by_token() {
    let engine = ListEngine::new();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&calls);
    let token = engine.subscribe(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(engine.listener_count(), 1);

    engine.handle_session_added(summary("a", 1));
    engine.handle_session_activity("a", 9);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "batched, not synchronous");
    engine.flush();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "two writes, one publication"
    );

    engine.unsubscribe(token);
    assert_eq!(engine.listener_count(), 0);
    // Unsubscribing twice is a no-op, exactly as upstream's returned function is.
    engine.unsubscribe(token);
    assert_eq!(engine.listener_count(), 0);
    let after = calls.load(Ordering::SeqCst);
    engine.handle_session_activity("a", 11);
    engine.flush();
    assert_eq!(calls.load(Ordering::SeqCst), after, "nobody is listening");
}

#[test]
fn an_agent_failure_reaches_only_a_materialised_session() {
    let engine = ListEngine::new();
    assert!(
        !engine.handle_session_error("cold", "boom"),
        "a session nobody opened has no error surface to write"
    );
    let view = engine.get("warm");
    assert!(engine.handle_session_error("warm", "boom"));
    assert_eq!(view.snapshot().last_agent_error.as_deref(), Some("boom"));
}

#[test]
fn a_view_surface_reports_its_identity_store_and_subscriptions() {
    let store = std::sync::Arc::new(ProjectionValueStore::new());
    let view = SessionView::new("s1", std::sync::Arc::clone(&store));
    assert_eq!(view.session_id(), "s1");
    assert!(std::sync::Arc::ptr_eq(&view.projections(), &store));

    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&calls);
    let token = view.subscribe(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(view.listener_count(), 1);
    view.note_window_prepended(true);
    view.flush();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    view.unsubscribe(token);
    assert_eq!(view.listener_count(), 0);
    view.unsubscribe(token);
    assert_eq!(view.listener_count(), 0);
}

#[test]
fn a_prepended_page_keeps_the_partial_memory() {
    let view = view("s1");
    view.note_window_prepended(true);
    assert!(view.snapshot().has_more);
    assert!(view.snapshot().history_was_partial);
    // Reaching the top of the log clears `has_more` but not the memory of the partial window.
    view.note_window_prepended(false);
    assert!(!view.snapshot().has_more);
    assert!(view.snapshot().history_was_partial);
}

#[test]
fn a_history_failure_is_reported_without_retiring_the_window() {
    let view = view("s1");
    view.begin_open();
    view.opened();
    view.set_history_load_status(HistoryLoadStatus::Error);
    let snapshot = view.snapshot();
    assert_eq!(snapshot.history_load_status, HistoryLoadStatus::Error);
    assert!(!snapshot.loading_older);
    assert_eq!(
        snapshot.open_state,
        OpenState::Open,
        "the window still exists"
    );
}

#[test]
fn a_migration_required_row_keeps_its_format_classification_through_lineage() {
    let engine = ListEngine::new();
    engine.handle_session_added(ListSummary {
        format_status: Some(FormatStatus::MigrationRequired),
        ..summary("old", 1)
    });
    let row = engine.snapshot().items()[0].clone();
    assert_eq!(row.format_status, Some(FormatStatus::MigrationRequired));
    assert_eq!(row.depth, 0);
}

#[test]
fn the_wire_spellings_round_trip_and_an_unknown_mode_is_absent() {
    for mode in [
        SubagentMode::OneShot,
        SubagentMode::Continuable,
        SubagentMode::Unknown,
    ] {
        assert_eq!(SubagentMode::from_wire(Some(mode.as_str())), Some(mode));
    }
    assert_eq!(SubagentMode::from_wire(Some("external")), None);
    assert_eq!(SubagentMode::from_wire(None), None);
}

#[test]
fn the_flush_order_publishes_store_changes_before_the_list() {
    let engine = ListEngine::new();
    engine.handle_session_added(summary("a", 1));
    engine.handle_control_frame(&ControlFrame::Projection(ProjectionUpdate {
        session_id: "a".to_owned(),
        seq: 2,
        changes: vec![(
            "sessionListMetadata".to_owned(),
            json!({ "blank": false, "lastPromptAt": null }),
        )],
    }));
    // One flush drains the projection stores and then the list.
    engine.flush();
    assert!(
        !engine.snapshot().items()[0].blank,
        "the metadata projection reached the row"
    );
}

// ---------------------------------------------------------------------------------------------
// Debug surfaces
// ---------------------------------------------------------------------------------------------

#[test]
fn the_debug_surfaces_report_state_without_leaking_listeners() {
    let view = view("s1");
    let engine = ListEngine::new();
    engine.get("s1");
    let view_debug = format!("{view:?}");
    let engine_debug = format!("{engine:?}");
    assert!(view_debug.contains("s1"));
    assert!(view_debug.contains("blank"));
    assert!(engine_debug.contains("ListEngine"));
    assert!(engine_debug.contains("sessions"));
}
