//! Tests for the shared Team task board: input normalization, the dependency graph, the
//! commit/authorize semantics of every action, and the `agentTeam` projection.
//!
//! The properties are transcribed from `packages/team/agent-team/tests/team.spec.ts` and
//! `projection-events.spec.ts`, which pin them through a live agent tree and a session log.
//! Those tests can assert the same rules because the rules are pure; what changes here is
//! that the tree and the log are supplied directly, so a failure names the rule rather than
//! a missing service.
//!
//! Three properties carry the weight, and each is tested from the side that can fail:
//!
//! 1. **A revision is a compare-and-set.** The tests drive the *stale* path and assert the
//!    reported numbers, because a refusal that names the wrong revision teaches a caller to
//!    retry with the wrong one.
//! 2. **The graph is validated whole.** A cycle is built through a third task, so a
//!    validator that only looked at the candidate's own edges would accept it.
//! 3. **A tombstone persists.** The id-after-delete, budget-after-delete, and
//!    blocked-by-a-tombstone cases are all asserted, because "deleted" is where an
//!    off-by-one in the id sequence or the active count hides.
//!
//! The module is included by path rather than through the crate root, so these tests compile
//! against the module as written even before `lib.rs` declares it. It resolves
//! `girder_core::…` through the crate root, which is why the same source builds here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/team/board.rs"]
pub mod team_gap;

use serde_json::{Value, json};

use girder_core::SessionId;

use team_gap::{
    Assignee, CreateTeamTaskRequest, DEFAULT_MAX_TASKS, MAX_DESCRIPTION_CHARS, MAX_SAFE_INTEGER,
    MAX_SUBJECT_CHARS, MemoryJournal, REMOVED_CONFIG_KEYS, TASK_ID_PREFIX, TEAM_PROJECTION_KEY,
    TEAM_PROJECTION_STATE_VERSION, TeamConfig, TeamErrorCode, TeamMember, TeamState,
    TeamTaskAction, TeamTaskBoard, TeamTaskFilter, TeamTaskId, TeamTaskJournal, TeamTaskList,
    TeamTaskSnapshot, TeamTaskStatus, UpdateTeamTaskRequest, assert_task_graph_candidate,
    is_valid_task_id, numbered_task_id, project_task_view, reject_removed_config, render_board,
    replay_events, required_text, scopes_overlap, task_number, task_ready, team_projection_view,
    write_scope,
};

// ---- helpers -----------------------------------------------------------------------------------

fn session(id: &str) -> SessionId {
    SessionId::new(id).expect("a literal session id is valid")
}

fn member(id: &str) -> TeamMember {
    TeamMember::new(session(id))
}

fn named(id: &str, name: &str) -> TeamMember {
    TeamMember::named(session(id), name)
}

fn task_id(id: &str) -> TeamTaskId {
    TeamTaskId::new(id).expect("a literal task id is valid")
}

/// A board for one root with Team mode already on, which is the state every command but
/// `set_enabled` requires.
fn board() -> (TeamTaskBoard, TeamMember) {
    let lead = member("root");
    let mut board = TeamTaskBoard::new(session("root"), DEFAULT_MAX_TASKS).expect("valid limits");
    board.set_enabled(&lead, true).expect("the root may enable");
    (board, lead)
}

fn request(subject: &str, description: &str) -> CreateTeamTaskRequest {
    CreateTeamTaskRequest {
        subject: subject.to_owned(),
        description: description.to_owned(),
        blocked_by: Vec::new(),
        write_scopes: Vec::new(),
    }
}

fn snapshot(id: &str, revision: u64, status: TeamTaskStatus) -> TeamTaskSnapshot {
    TeamTaskSnapshot {
        id: task_id(id),
        revision,
        subject: id.to_owned(),
        description: id.to_owned(),
        status,
        owner_id: None,
        owner_name: None,
        blocked_by: Vec::new(),
        write_scopes: Vec::new(),
    }
}

fn task_event(team: &str, task: &TeamTaskSnapshot) -> Value {
    json!({
        "version": 2,
        "teamId": team,
        "task": serde_json::to_value(task).expect("a snapshot serializes"),
    })
}

// ---- required text -----------------------------------------------------------------------------

#[test]
fn required_text_trims_and_counts_the_trimmed_length() {
    assert_eq!(
        required_text("  blocked  ", "subject", MAX_SUBJECT_CHARS).expect("valid"),
        "blocked"
    );
    // 200 characters exactly is accepted; the check is on the text a reader will see, which
    // is why padding does not count.
    let at_limit = "x".repeat(MAX_SUBJECT_CHARS);
    let padded = format!("  {at_limit}  ");
    assert_eq!(
        required_text(&padded, "subject", MAX_SUBJECT_CHARS)
            .expect("valid")
            .chars()
            .count(),
        MAX_SUBJECT_CHARS
    );
    assert_eq!(
        required_text(
            &"x".repeat(MAX_SUBJECT_CHARS + 1),
            "subject",
            MAX_SUBJECT_CHARS
        )
        .unwrap_err()
        .code(),
        TeamErrorCode::InvalidArgument
    );
}

#[test]
fn required_text_refuses_blank_input_by_the_field_name() {
    let error = required_text("   ", "description", MAX_DESCRIPTION_CHARS).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::InvalidArgument);
    assert_eq!(error.to_string(), "description must be non-empty");
}

// ---- write scopes ------------------------------------------------------------------------------

#[test]
fn write_scope_normalizes_the_three_mechanical_rewrites() {
    assert_eq!(write_scope("src").expect("valid"), "src");
    assert_eq!(write_scope("./src").expect("valid"), "src");
    assert_eq!(write_scope("src/").expect("valid"), "src");
    assert_eq!(write_scope("src///").expect("valid"), "src");
    assert_eq!(write_scope(r"src\feature").expect("valid"), "src/feature");
    assert_eq!(write_scope("./a/b/").expect("valid"), "a/b");
}

#[test]
fn write_scope_refuses_every_shape_upstream_refuses() {
    // The list is upstream's, including the two drive-letter spellings and both dot
    // segments; each would make the overlap test lie rather than merely look odd.
    for scope in [
        "", ".", "..", "/root", r"C:\root", "C:root", "a//b", "a/../b", "./",
    ] {
        let error = write_scope(scope).unwrap_err();
        assert_eq!(
            error.code(),
            TeamErrorCode::InvalidWriteScope,
            "scope {scope:?} should be refused"
        );
        assert_eq!(error.code_str(), "TEAM_INVALID_WRITE_SCOPE");
    }
}

#[test]
fn write_scope_reports_the_value_it_refused() {
    let error = write_scope("a/../b").unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid workspace-relative write scope \"a/../b\""
    );
}

#[test]
fn overlap_is_component_wise_so_a_sibling_prefix_is_disjoint() {
    assert!(scopes_overlap("src", "src"));
    assert!(scopes_overlap("src", "src/feature"));
    assert!(scopes_overlap("src/feature", "src"));
    // The case a string `starts_with` gets wrong: `src2` is not inside `src`.
    assert!(!scopes_overlap("src", "src2"));
    assert!(!scopes_overlap("src/feature", "src/other"));
}

// ---- readiness and views -----------------------------------------------------------------------

#[test]
fn readiness_requires_every_blocker_to_have_completed() {
    let mut state = TeamState::empty(session("root"));
    let blocker = snapshot("task-1", 1, TeamTaskStatus::Pending);
    state.apply_task(blocker).expect("a first revision applies");
    let mut dependent = snapshot("task-2", 1, TeamTaskStatus::Pending);
    dependent.blocked_by = vec![task_id("task-1")];
    state
        .apply_task(dependent.clone())
        .expect("a valid edge applies");

    assert!(
        !task_ready(&state, &dependent),
        "a pending blocker is not done"
    );

    let mut completed = snapshot("task-1", 2, TeamTaskStatus::Completed);
    completed.blocked_by.clear();
    state
        .apply_task(completed)
        .expect("a contiguous revision applies");
    assert!(
        task_ready(&state, &dependent),
        "a completed blocker is done"
    );

    // A blocker that no longer exists counts as incomplete: absence is not completion.
    let mut dangling = dependent.clone();
    dangling.blocked_by = vec![task_id("task-9")];
    assert!(!task_ready(&state, &dangling));
}

#[test]
fn a_view_is_ready_only_while_pending_and_warns_only_about_in_progress_peers() {
    let mut state = TeamState::empty(session("root"));
    let mut first = snapshot("task-1", 1, TeamTaskStatus::InProgress);
    first.write_scopes = vec!["src".to_owned()];
    state.apply_task(first).expect("first revision");
    let mut second = snapshot("task-2", 1, TeamTaskStatus::Pending);
    second.write_scopes = vec!["src/feature".to_owned(), "docs".to_owned()];
    state.apply_task(second.clone()).expect("first revision");

    let view = project_task_view(&state, &second);
    assert!(view.ready, "no blockers means ready");
    assert_eq!(
        view.write_scope_warnings,
        vec!["write scopes overlap with task-1"]
    );

    // A *pending* peer is not a conflict yet: flagging it would teach a reader to ignore the
    // warning.
    let mut third = snapshot("task-3", 1, TeamTaskStatus::Pending);
    third.write_scopes = vec!["docs".to_owned()];
    state.apply_task(third.clone()).expect("first revision");
    assert!(
        project_task_view(&state, &third)
            .write_scope_warnings
            .is_empty()
    );

    // Readiness is derived, not stored: an in-progress task is never "ready".
    let mut in_progress = second.clone();
    in_progress.status = TeamTaskStatus::InProgress;
    in_progress.revision = 2;
    state
        .apply_task(in_progress.clone())
        .expect("contiguous revision");
    assert!(!project_task_view(&state, &in_progress).ready);
}

// ---- the dependency graph ----------------------------------------------------------------------

#[test]
fn a_duplicate_blocker_is_refused_as_an_invalid_argument() {
    let first = snapshot("task-1", 1, TeamTaskStatus::Pending);
    let mut candidate = snapshot("task-2", 1, TeamTaskStatus::Pending);
    candidate.blocked_by = vec![task_id("task-1"), task_id("task-1")];
    let error = assert_task_graph_candidate(std::slice::from_ref(&first), &candidate).unwrap_err();
    assert_eq!(error.code_str(), "TEAM_INVALID_ARGUMENT");
    assert_eq!(error.message, "team task task-2 repeats blocker task-1");
}

#[test]
fn a_missing_or_tombstoned_blocker_is_refused() {
    let first = snapshot("task-1", 1, TeamTaskStatus::Deleted);
    let mut candidate = snapshot("task-2", 1, TeamTaskStatus::Pending);
    candidate.blocked_by = vec![task_id("task-1")];
    let error = assert_task_graph_candidate(std::slice::from_ref(&first), &candidate).unwrap_err();
    assert_eq!(error.code_str(), "TEAM_TASK_NOT_FOUND");
    assert_eq!(
        error.message,
        "blocker task task-1 for task-2 is missing or deleted"
    );
}

#[test]
fn self_blocking_is_a_cycle_not_an_invalid_argument() {
    let mut candidate = snapshot("task-1", 1, TeamTaskStatus::Pending);
    candidate.blocked_by = vec![task_id("task-1")];
    let error = assert_task_graph_candidate(&[], &candidate).unwrap_err();
    assert_eq!(error.code_str(), "TEAM_TASK_DEPENDENCY_CYCLE");
    assert_eq!(error.message, "team task task-1 cannot block itself");
}

#[test]
fn a_cycle_through_a_third_task_is_refused_by_the_whole_graph_walk() {
    // The candidate's own edge is legal in isolation; only the complete walk sees the loop.
    let first = snapshot("task-1", 1, TeamTaskStatus::Pending);
    let mut second = snapshot("task-2", 1, TeamTaskStatus::Pending);
    second.blocked_by = vec![task_id("task-1")];
    let mut third = snapshot("task-3", 1, TeamTaskStatus::Pending);
    third.blocked_by = vec![task_id("task-2")];
    let mut candidate = snapshot("task-1", 2, TeamTaskStatus::Pending);
    candidate.blocked_by = vec![task_id("task-3")];

    let error = assert_task_graph_candidate(&[first, second, third], &candidate).unwrap_err();
    assert_eq!(error.code_str(), "TEAM_TASK_DEPENDENCY_CYCLE");
    assert!(
        error.message.contains("cycle includes"),
        "{}",
        error.message
    );
}

#[test]
fn a_tombstone_is_skipped_as_a_subject_but_missing_as_a_blocker() {
    // A tombstone keeps whatever edges it was committed with, and they are not re-walked:
    // deleting a task must never be blocked by the graph it left behind. But it cannot
    // *block* anything either, because a tombstone never completes, so an edge to one is
    // reported as missing rather than left to block forever.
    let mut tombstone = snapshot("task-1", 2, TeamTaskStatus::Deleted);
    tombstone.blocked_by = vec![task_id("task-2")];
    let live_with_own_edge = snapshot("task-2", 1, TeamTaskStatus::Pending);
    assert_task_graph_candidate(&[tombstone], &live_with_own_edge)
        .expect("the tombstone's own edges are not re-walked");

    let mut blocked_by_tombstone = snapshot("task-3", 1, TeamTaskStatus::Pending);
    blocked_by_tombstone.blocked_by = vec![task_id("task-1")];
    let error = assert_task_graph_candidate(
        &[snapshot("task-1", 2, TeamTaskStatus::Deleted)],
        &blocked_by_tombstone,
    )
    .unwrap_err();
    assert_eq!(error.code_str(), "TEAM_TASK_NOT_FOUND");
}

// ---- projection --------------------------------------------------------------------------------

#[test]
fn a_mode_event_before_the_inherited_boundary_is_ignored() {
    let mut state = TeamState::empty_at(session("root"), 10);
    state.apply_event(3, "team/mode", &json!({ "enabled": true }));
    assert_eq!(
        state.enabled(),
        None,
        "a forked prefix cannot select a mode"
    );
    state.apply_event(10, "team/mode", &json!({ "enabled": true }));
    assert!(state.is_enabled());
}

#[test]
fn a_task_event_for_another_team_is_ignored_and_leaves_no_failure() {
    let mut state = TeamState::empty(session("root"));
    let event = task_event(
        "someone-else",
        &snapshot("task-1", 1, TeamTaskStatus::Pending),
    );
    state.apply_event(1, "team/task", &event);
    assert!(state.tasks().is_empty());
    assert_eq!(
        state.failure(),
        None,
        "another Team's row is not this Team's error"
    );
}

#[test]
fn an_unsupported_event_version_is_reported_by_name() {
    let mut data = task_event("root", &snapshot("task-1", 1, TeamTaskStatus::Pending));
    data["version"] = json!(3);
    let mut state = TeamState::empty(session("root"));
    state.apply_event(1, "team/task", &data);
    assert_eq!(
        state.failure(),
        Some("unsupported Agent Teams event version 3")
    );
    assert!(state.tasks().is_empty());
}

#[test]
fn a_persisted_row_with_an_unknown_field_is_invalid() {
    // Upstream's schema is `.strict()`: silently dropping a field a newer build wrote would
    // let this reader re-persist the event without it.
    let mut task = serde_json::to_value(snapshot("task-1", 1, TeamTaskStatus::Pending))
        .expect("a snapshot serializes");
    task["unexpected"] = json!(1);
    let mut state = TeamState::empty(session("root"));
    state.apply_event(
        1,
        "team/task",
        &json!({ "version": 2, "teamId": "root", "task": task }),
    );
    assert_eq!(
        state.failure(),
        Some("persisted Agent Teams team/task payload is invalid")
    );
}

#[test]
fn a_non_contiguous_revision_is_invalid() {
    let mut state = TeamState::empty(session("root"));
    state.apply_event(
        1,
        "team/task",
        &task_event("root", &snapshot("task-1", 2, TeamTaskStatus::Pending)),
    );
    assert_eq!(
        state.failure(),
        Some("persisted Agent Teams team/task payload is invalid")
    );

    let mut state = TeamState::empty(session("root"));
    state
        .apply_task(snapshot("task-1", 1, TeamTaskStatus::Pending))
        .expect("first revision");
    assert_eq!(
        state
            .apply_task(snapshot("task-1", 3, TeamTaskStatus::Pending))
            .unwrap_err(),
        "team task task-1 revision is not contiguous"
    );
}

#[test]
fn the_first_projection_failure_is_sticky() {
    // The log is replayed from the start on every load, so a later valid event must not make
    // a board with an earlier broken revision look healthy.
    let mut state = TeamState::empty(session("root"));
    state.apply_event(
        1,
        "team/task",
        &task_event("root", &snapshot("task-1", 7, TeamTaskStatus::Pending)),
    );
    assert!(state.failure().is_some());
    state.apply_event(
        2,
        "team/task",
        &task_event("root", &snapshot("task-2", 1, TeamTaskStatus::Pending)),
    );
    assert!(state.tasks().is_empty(), "no task applies after a failure");
    assert!(state.failure().is_some());
}

#[test]
fn applying_a_task_event_also_turns_team_mode_on() {
    // A historical task can only exist on a board that was enabled, so replaying one
    // restores the selection rather than leaving the tasks behind a disabled flag.
    let state = replay_events(
        session("root"),
        0,
        vec![(
            1,
            "team/task".to_owned(),
            task_event("root", &snapshot("task-1", 1, TeamTaskStatus::Pending)),
        )],
    );
    assert!(state.is_enabled());
    assert_eq!(state.next_task_number(), 2);
}

#[test]
fn the_wire_view_hides_tombstones_but_keeps_the_failure() {
    let mut state = TeamState::empty(session("root"));
    state
        .apply_task(snapshot("task-1", 1, TeamTaskStatus::Pending))
        .expect("first revision");
    state
        .apply_task(snapshot("task-1", 2, TeamTaskStatus::Deleted))
        .expect("contiguous revision");

    let view = team_projection_view(&state);
    assert!(view.enabled);
    assert!(view.tasks.is_empty(), "a tombstone is not on the board");
    assert_eq!(view.failure, None);

    let wire = view.to_json();
    assert_eq!(wire["enabled"], json!(true));
    assert_eq!(wire["tasks"], json!([]));
    assert_eq!(wire.get("failure"), None, "an absent failure is omitted");
}

#[test]
fn the_module_reports_the_projection_identity_upstream_declares() {
    assert_eq!(TEAM_PROJECTION_KEY, "agentTeam");
    assert_eq!(TEAM_PROJECTION_STATE_VERSION, 7);
}

// ---- board: creation ---------------------------------------------------------------------------

#[test]
fn a_disabled_board_refuses_every_command_with_team_disabled() {
    let lead = member("root");
    let mut board = TeamTaskBoard::new(session("root"), DEFAULT_MAX_TASKS).expect("valid limits");
    assert_eq!(
        board.create(&lead, request("a", "b")).unwrap_err().code(),
        TeamErrorCode::Disabled
    );
    assert_eq!(board.list().unwrap_err().code(), TeamErrorCode::Disabled);
    assert_eq!(
        board.get(&task_id("task-1")).unwrap_err().code(),
        TeamErrorCode::Disabled
    );
}

#[test]
fn only_the_root_may_change_team_mode() {
    let lead = member("root");
    let child = named("child", "lead/child");
    let mut board = TeamTaskBoard::new(session("root"), DEFAULT_MAX_TASKS).expect("valid limits");
    let error = board.set_enabled(&child, true).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::LeadRequired);
    assert_eq!(error.to_string(), "change Team mode from the root Session");
    board.set_enabled(&lead, true).expect("the root may enable");
}

#[test]
fn creation_numbers_tasks_from_one_and_normalizes_its_input() {
    let (mut board, lead) = board();
    let mut first = request("  first  ", "  first task  ");
    first.write_scopes = vec!["src".to_owned(), "./src".to_owned(), "src/".to_owned()];
    let created = board.create(&lead, first).expect("enabled");

    assert_eq!(created.id.as_str(), "task-1");
    assert_eq!(created.revision, 1);
    assert_eq!(created.status, TeamTaskStatus::Pending);
    assert_eq!(created.subject, "first");
    assert_eq!(created.description, "first task");
    assert_eq!(created.write_scopes, vec!["src"], "duplicates collapse");
    assert!(created.ready);
    assert_eq!(created.owner_id, None);
    assert_eq!(created.owner_name, None);

    let second = board
        .create(&lead, request("second", "second"))
        .expect("enabled");
    assert_eq!(second.id.as_str(), "task-2");
}

#[test]
fn a_deleted_task_keeps_its_id_and_its_budget_slot_is_freed() {
    let lead = member("root");
    let mut board = TeamTaskBoard::new(session("root"), 1).expect("valid limits");
    board.set_enabled(&lead, true).expect("enable");
    let first = board
        .create(&lead, request("first", "first"))
        .expect("enabled");
    assert_eq!(
        board
            .create(&lead, request("overflow", "overflow"))
            .unwrap_err()
            .code(),
        TeamErrorCode::TaskLimit
    );

    let mut delete =
        UpdateTeamTaskRequest::new(first.id.clone(), first.revision, TeamTaskAction::Delete);
    delete.owner = None;
    let deleted = board.update(&lead, &delete, &[]).expect("lead may delete");
    assert_eq!(deleted.status, TeamTaskStatus::Deleted);

    let second = board
        .create(&lead, request("second", "second"))
        .expect("a slot is free");
    // The id advances past the tombstone: reissuing `task-1` would hand a caller's stale id
    // to a different task.
    assert_eq!(second.id.as_str(), "task-2");
    assert_eq!(
        board
            .get(&first.id)
            .expect("a tombstone is readable")
            .status,
        TeamTaskStatus::Deleted
    );
    assert_eq!(
        board
            .list()
            .expect("enabled")
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>(),
        vec![second.id]
    );
}

#[test]
fn an_exhausted_numeric_id_space_fails_loudly() {
    let (mut board, lead) = board();
    let mut state = TeamState::empty(session("root"));
    state
        .apply_task(snapshot(
            &format!("{TASK_ID_PREFIX}{MAX_SAFE_INTEGER}"),
            1,
            TeamTaskStatus::Pending,
        ))
        .expect("the ceiling id is valid");
    let mut resumed = TeamTaskBoard::resume(state, DEFAULT_MAX_TASKS, MemoryJournal::new())
        .expect("valid limits");
    resumed.set_enabled(&lead, true).expect("enable");

    let error = resumed
        .create(&lead, request("cannot", "allocate"))
        .unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::TaskLimit);
    assert_eq!(error.to_string(), "Team task id space exhausted");
    let _ = &mut board;
}

#[test]
fn creation_refuses_a_bad_dependency_and_a_bad_scope_before_committing() {
    let (mut board, lead) = board();
    let mut missing = request("two", "two");
    missing.blocked_by = vec![task_id("missing")];
    let error = board.create(&lead, missing).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::TaskNotFound);
    assert_eq!(error.to_string(), "blocker task \"missing\" not found");

    let mut bad_scope = request("two", "two");
    bad_scope.write_scopes = vec!["../escape".to_owned()];
    assert_eq!(
        board.create(&lead, bad_scope).unwrap_err().code(),
        TeamErrorCode::InvalidWriteScope
    );
    // Nothing was committed, so the next id is still the first.
    assert_eq!(
        board
            .create(&lead, request("ok", "ok"))
            .expect("enabled")
            .id
            .as_str(),
        "task-1"
    );
}

// ---- board: claim, release, complete -----------------------------------------------------------

#[test]
fn claiming_takes_a_ready_pending_task_and_records_the_agent_path() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let child = named("child", "lead/child");

    let claim =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Claim);
    let claimed = board
        .update(&child, &claim, &[])
        .expect("any member may claim");
    assert_eq!(claimed.status, TeamTaskStatus::InProgress);
    assert_eq!(
        claimed.owner_id.as_ref().map(SessionId::as_str),
        Some("child")
    );
    assert_eq!(claimed.owner_name.as_deref(), Some("lead/child"));

    // The wire shape collapses the two owner fields into one display name.
    assert_eq!(claimed.to_json()["owner"], json!("lead/child"));
}

#[test]
fn an_unnamed_claimer_carries_no_redundant_owner_name() {
    // Upstream omits `ownerName` when the resolved name *is* the Session id, so a client
    // does not display the same value twice under two labels.
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let unnamed = member("nameless");
    let claim =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Claim);
    let claimed = board.update(&unnamed, &claim, &[]).expect("claim");
    assert_eq!(claimed.owner_name, None);
    assert_eq!(
        claimed.to_json()["owner"],
        json!("nameless"),
        "the id is the label"
    );
}

#[test]
fn a_blocked_task_cannot_be_claimed_or_reassigned() {
    let (mut board, lead) = board();
    let blocker = board
        .create(&lead, request("blocker", "blocker"))
        .expect("enabled");
    let mut dependent = request("dependent", "dependent");
    dependent.blocked_by = vec![blocker.id.clone()];
    let task = board.create(&lead, dependent).expect("enabled");

    let claim = UpdateTeamTaskRequest::new(task.id.clone(), task.revision, TeamTaskAction::Claim);
    let error = board.update(&lead, &claim, &[]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::TaskBlocked);
    assert_eq!(error.to_string(), "team task task-2 is not ready to claim");

    let mut reassign =
        UpdateTeamTaskRequest::new(task.id.clone(), task.revision, TeamTaskAction::Reassign);
    reassign.owner = Some(Assignee::Member(session("root")));
    let error = board.update(&lead, &reassign, &[]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::TaskBlocked);
    assert_eq!(error.to_string(), "team task task-2 is blocked");
}

#[test]
fn claiming_a_task_owned_by_another_member_reports_already_claimed() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let first =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Claim);
    let claimed = board.update(&lead, &first, &[]).expect("claim");
    let second =
        UpdateTeamTaskRequest::new(created.id.clone(), claimed.revision, TeamTaskAction::Claim);
    let error = board.update(&member("other"), &second, &[]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::TaskAlreadyClaimed);
}

#[test]
fn a_stale_revision_names_both_revisions() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let claim =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Claim);
    board.update(&lead, &claim, &[]).expect("claim");
    // Replaying the same revision is the double-claim the compare-and-set exists to stop.
    let again = UpdateTeamTaskRequest::new(
        created.id.clone(),
        created.revision,
        TeamTaskAction::Complete,
    );
    let error = board.update(&lead, &again, &[]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::StaleRevision);
    assert_eq!(
        error.to_string(),
        "stale team task task-1 revision 1; current revision is 2"
    );
}

#[test]
fn a_mutation_by_a_member_who_is_neither_owner_nor_lead_is_unauthorized() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let claim =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Claim);
    let claimed = board.update(&lead, &claim, &[]).expect("claim");
    let complete = UpdateTeamTaskRequest::new(
        created.id.clone(),
        claimed.revision,
        TeamTaskAction::Complete,
    );
    let error = board
        .update(&member("stranger"), &complete, &[])
        .unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::TaskUnauthorized);
    assert_eq!(
        error.to_string(),
        "task mutation requires its owner or Team Lead"
    );
}

#[test]
fn release_and_reopen_clear_both_owner_fields() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let claim =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Claim);
    let claimed = board.update(&lead, &claim, &[]).expect("claim");

    let release = UpdateTeamTaskRequest::new(
        created.id.clone(),
        claimed.revision,
        TeamTaskAction::Release,
    );
    let released = board.update(&lead, &release, &[]).expect("release");
    assert_eq!(released.status, TeamTaskStatus::Pending);
    assert!(released.ready);
    assert_eq!(released.owner_id, None);
    assert_eq!(released.owner_name, None);
    assert_eq!(released.to_json().get("owner"), None);

    let claim_again =
        UpdateTeamTaskRequest::new(created.id.clone(), released.revision, TeamTaskAction::Claim);
    let reclaimed = board.update(&lead, &claim_again, &[]).expect("claim");
    let complete = UpdateTeamTaskRequest::new(
        created.id.clone(),
        reclaimed.revision,
        TeamTaskAction::Complete,
    );
    let completed = board.update(&lead, &complete, &[]).expect("complete");
    assert_eq!(completed.status, TeamTaskStatus::Completed);

    let reopen = UpdateTeamTaskRequest::new(
        created.id.clone(),
        completed.revision,
        TeamTaskAction::Reopen,
    );
    let reopened = board.update(&lead, &reopen, &[]).expect("reopen");
    assert_eq!(reopened.status, TeamTaskStatus::Pending);
    assert_eq!(reopened.owner_id, None);
    assert_eq!(reopened.revision, 6);
}

#[test]
fn every_illegal_transition_is_refused_with_the_status_it_started_from() {
    let (mut board, lead) = board();
    let pending = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    for action in [
        TeamTaskAction::Release,
        TeamTaskAction::Complete,
        TeamTaskAction::Reopen,
    ] {
        let update = UpdateTeamTaskRequest::new(pending.id.clone(), pending.revision, action);
        let error = board.update(&lead, &update, &[]).unwrap_err();
        assert_eq!(
            error.code(),
            TeamErrorCode::InvalidTransition,
            "{} from pending",
            action.as_str()
        );
    }
    // `edit` with no field set is a caller bug, not a request to change nothing.
    let edit =
        UpdateTeamTaskRequest::new(pending.id.clone(), pending.revision, TeamTaskAction::Edit);
    assert_eq!(
        board.update(&lead, &edit, &[]).unwrap_err().code(),
        TeamErrorCode::InvalidArgument
    );
}

// ---- board: edit and dependencies --------------------------------------------------------------

#[test]
fn edit_replaces_only_the_fields_it_is_given() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let mut edit =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Edit);
    edit.subject = Some("  renamed  ".to_owned());
    let edited = board.update(&lead, &edit, &[]).expect("edit");
    assert_eq!(edited.subject, "renamed");
    assert_eq!(edited.description, "work", "an absent field is unchanged");

    let mut rescan =
        UpdateTeamTaskRequest::new(edited.id.clone(), edited.revision, TeamTaskAction::Edit);
    rescan.write_scopes = Some(vec!["./src/".to_owned(), "src".to_owned()]);
    let scoped = board.update(&lead, &rescan, &[]).expect("edit");
    assert_eq!(scoped.write_scopes, vec!["src"]);
}

#[test]
fn set_dependencies_requires_a_list_and_refuses_a_cycle_through_the_board() {
    let (mut board, lead) = board();
    let first = board.create(&lead, request("one", "one")).expect("enabled");
    let mut second_request = request("two", "two");
    second_request.blocked_by = vec![first.id.clone()];
    let second = board.create(&lead, second_request).expect("enabled");

    let unset = UpdateTeamTaskRequest::new(
        first.id.clone(),
        first.revision,
        TeamTaskAction::SetDependencies,
    );
    assert_eq!(
        board.update(&lead, &unset, &[]).unwrap_err().code(),
        TeamErrorCode::InvalidArgument
    );

    let mut cycle = UpdateTeamTaskRequest::new(
        first.id.clone(),
        first.revision,
        TeamTaskAction::SetDependencies,
    );
    cycle.blocked_by = Some(vec![second.id.clone()]);
    let error = board.update(&lead, &cycle, &[]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::DependencyCycle);

    let mut self_edge = UpdateTeamTaskRequest::new(
        first.id.clone(),
        first.revision,
        TeamTaskAction::SetDependencies,
    );
    self_edge.blocked_by = Some(vec![first.id.clone()]);
    assert_eq!(
        board.update(&lead, &self_edge, &[]).unwrap_err().code(),
        TeamErrorCode::DependencyCycle
    );

    let mut duplicate = UpdateTeamTaskRequest::new(
        first.id.clone(),
        first.revision,
        TeamTaskAction::SetDependencies,
    );
    duplicate.blocked_by = Some(vec![second.id.clone(), second.id.clone()]);
    assert_eq!(
        board.update(&lead, &duplicate, &[]).unwrap_err().code(),
        TeamErrorCode::InvalidArgument
    );
}

// ---- board: reassignment and deletion ----------------------------------------------------------

#[test]
fn reassignment_is_lead_only_and_validates_the_assignee() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let child = named("child", "lead/child");

    let mut not_lead = UpdateTeamTaskRequest::new(
        created.id.clone(),
        created.revision,
        TeamTaskAction::Reassign,
    );
    not_lead.owner = Some(Assignee::Member(child.id.clone()));
    assert_eq!(
        board
            .update(&child, &not_lead, std::slice::from_ref(&child))
            .unwrap_err()
            .code(),
        TeamErrorCode::LeadRequired
    );

    let mut unknown = UpdateTeamTaskRequest::new(
        created.id.clone(),
        created.revision,
        TeamTaskAction::Reassign,
    );
    unknown.owner = Some(Assignee::Member(session("ghost")));
    let error = board
        .update(&lead, &unknown, std::slice::from_ref(&child))
        .unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::MemberNotFound);
    assert_eq!(
        error.to_string(),
        "agent \"ghost\" is not a continuable descendant of this task board's root"
    );

    let mut assigned = UpdateTeamTaskRequest::new(
        created.id.clone(),
        created.revision,
        TeamTaskAction::Reassign,
    );
    assigned.owner = Some(Assignee::Member(child.id.clone()));
    let view = board
        .update(&lead, &assigned, &[child])
        .expect("lead may assign");
    assert_eq!(view.status, TeamTaskStatus::InProgress);
    assert_eq!(view.owner_name.as_deref(), Some("lead/child"));

    let mut unassign =
        UpdateTeamTaskRequest::new(created.id.clone(), view.revision, TeamTaskAction::Reassign);
    unassign.owner = Some(Assignee::Nobody);
    let view = board
        .update(&lead, &unassign, &[])
        .expect("lead may unassign");
    assert_eq!(view.status, TeamTaskStatus::Pending);
    assert_eq!(view.owner_id, None);
}

#[test]
fn the_root_assigns_to_itself_even_when_it_is_not_listed_as_a_descendant() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let mut assign = UpdateTeamTaskRequest::new(
        created.id.clone(),
        created.revision,
        TeamTaskAction::Reassign,
    );
    assign.owner = Some(Assignee::Member(session("root")));
    let view = board
        .update(&lead, &assign, &[])
        .expect("the root is always valid");
    assert_eq!(view.owner_id.as_ref().map(SessionId::as_str), Some("root"));
    // An unnamed root falls back to its Session id rather than inventing a label.
    assert_eq!(view.owner_name.as_deref(), Some("root"));
}

#[test]
fn a_live_dependent_prevents_deletion() {
    let (mut board, lead) = board();
    let first = board.create(&lead, request("one", "one")).expect("enabled");
    let mut second_request = request("two", "two");
    second_request.blocked_by = vec![first.id.clone()];
    let second = board.create(&lead, second_request).expect("enabled");

    let delete =
        UpdateTeamTaskRequest::new(first.id.clone(), first.revision, TeamTaskAction::Delete);
    let error = board.update(&lead, &delete, &[]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::HasDependents);
    assert_eq!(
        error.to_string(),
        format!("team task task-1 still blocks {}", second.id.as_str())
    );
}

#[test]
fn a_tombstone_refuses_further_mutation_and_missing_ids_are_not_found() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let delete =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Delete);
    let deleted = board.update(&lead, &delete, &[]).expect("delete");
    let edit =
        UpdateTeamTaskRequest::new(created.id.clone(), deleted.revision, TeamTaskAction::Edit);
    let mut edit = edit;
    edit.subject = Some("late".to_owned());
    let error = board.update(&lead, &edit, &[]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::TaskDeleted);
    assert_eq!(error.to_string(), "team task task-1 is deleted");

    let missing = UpdateTeamTaskRequest::new(task_id("missing-update"), 1, TeamTaskAction::Delete);
    let error = board.update(&lead, &missing, &[]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::TaskNotFound);
    assert_eq!(error.to_string(), "team task \"missing-update\" not found");
}

// ---- board: the paged list ---------------------------------------------------------------------

#[test]
fn the_list_page_filters_before_paging_so_the_cursor_is_a_position_in_the_filtered_list() {
    let (mut board, lead) = board();
    for index in 0..5 {
        let created = board
            .create(&lead, request(&format!("task {index}"), "d"))
            .expect("enabled");
        if index < 3 {
            let claim = UpdateTeamTaskRequest::new(
                created.id.clone(),
                created.revision,
                TeamTaskAction::Claim,
            );
            board.update(&lead, &claim, &[]).expect("claim");
        }
    }

    let mut filter = TeamTaskFilter::first_page();
    filter.limit = 2;
    let page = board.list_page(&filter).expect("enabled");
    assert_eq!(page.tasks.len(), 2);
    assert_eq!(page.next_cursor, Some(2));
    assert_eq!(page.to_json()["nextCursor"], json!(2));

    let mut second = TeamTaskFilter::first_page();
    second.limit = 2;
    second.cursor = 4;
    let page = board.list_page(&second).expect("enabled");
    assert_eq!(page.tasks.len(), 1);
    assert_eq!(page.next_cursor, None, "the last page has no cursor");
    assert_eq!(page.to_json().get("nextCursor"), None);

    let mut pending = TeamTaskFilter::first_page();
    pending.status = Some(TeamTaskStatus::Pending);
    let page = board.list_page(&pending).expect("enabled");
    assert_eq!(page.tasks.len(), 2, "only the two unclaimed tasks");
    assert!(page.tasks.iter().all(|task| task.ready));

    let mut unowned = TeamTaskFilter::first_page();
    unowned.unowned = Some(true);
    assert_eq!(board.list_page(&unowned).expect("enabled").tasks.len(), 2);

    let mut owned = TeamTaskFilter::first_page();
    owned.owner = Some(session("root"));
    assert_eq!(board.list_page(&owned).expect("enabled").tasks.len(), 3);

    let mut both = TeamTaskFilter::first_page();
    both.owner = Some(session("root"));
    both.unowned = Some(true);
    assert_eq!(
        board.list_page(&both).unwrap_err().code(),
        TeamErrorCode::InvalidArgument
    );

    let mut bad_limit = TeamTaskFilter::first_page();
    bad_limit.limit = 0;
    assert_eq!(
        board.list_page(&bad_limit).unwrap_err().code(),
        TeamErrorCode::InvalidArgument
    );
    bad_limit.limit = 101;
    assert_eq!(
        board.list_page(&bad_limit).unwrap_err().code(),
        TeamErrorCode::InvalidArgument
    );
}

// ---- journal -----------------------------------------------------------------------------------

#[test]
fn every_commit_is_appended_after_it_becomes_visible() {
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let claim =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Claim);
    board.update(&lead, &claim, &[]).expect("claim");
    assert_eq!(
        board.journal().len(),
        2,
        "one append per committed revision"
    );
    assert_eq!(
        board.journal().committed().last().map(|task| task.revision),
        Some(2)
    );
}

#[test]
fn replaying_the_journal_reproduces_the_board() {
    // The log is the source of truth: a board that disagrees with a replay has drifted.
    let (mut board, lead) = board();
    let created = board
        .create(&lead, request("work", "work"))
        .expect("enabled");
    let claim =
        UpdateTeamTaskRequest::new(created.id.clone(), created.revision, TeamTaskAction::Claim);
    board.update(&lead, &claim, &[]).expect("claim");

    let journal = board.journal().clone();
    let replayed = journal
        .replay(session("root"))
        .expect("a clean journal replays");
    assert_eq!(replayed.tasks(), board.state().tasks());
    assert_eq!(
        replayed.next_task_number(),
        board.state().next_task_number()
    );
}

#[test]
fn a_visible_revision_without_durability_says_to_read_before_retrying() {
    // A journal whose append fails leaves the revision visible; a blind retry would append a
    // second revision of the same change, so the error has to say so.
    struct Failing;
    impl TeamTaskJournal for Failing {
        fn append(&mut self, _task: &TeamTaskSnapshot) -> Result<(), team_gap::JournalError> {
            Err(team_gap::JournalError::new(
                "no durability listener participated",
            ))
        }
        fn committed(&self) -> &[TeamTaskSnapshot] {
            &[]
        }
    }

    let lead = member("root");
    let mut board = TeamTaskBoard::resume(
        TeamState::empty(session("root")),
        DEFAULT_MAX_TASKS,
        Failing,
    )
    .expect("valid limits");
    board.enable();
    let error = board.create(&lead, request("work", "work")).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::PersistenceUnconfirmed);
    assert!(error.to_string().contains("read the task before retrying"));
    // The revision is visible even though the append failed, which is exactly the hazard.
    assert_eq!(board.state().tasks().len(), 1);
}

// ---- ids, config, rendering --------------------------------------------------------------------

#[test]
fn numeric_task_ids_are_bounded_by_the_safe_integer_range() {
    assert!(is_valid_task_id("task-1"));
    assert!(
        is_valid_task_id("custom-id"),
        "a non-numeric suffix is accepted"
    );
    assert!(is_valid_task_id("task-"));
    assert!(is_valid_task_id(&format!("task-{MAX_SAFE_INTEGER}")));
    assert!(!is_valid_task_id(&format!("task-{}", MAX_SAFE_INTEGER + 1)));

    assert_eq!(task_number(&task_id("task-12")), Some(12));
    assert_eq!(task_number(&task_id("custom")), None);
    assert_eq!(task_number(&task_id("task-")), None);

    assert_eq!(numbered_task_id(3).expect("valid").as_str(), "task-3");
}

#[test]
fn a_snapshot_validates_the_rules_its_schema_encodes() {
    let good = snapshot("task-1", 1, TeamTaskStatus::Pending);
    good.validate().expect("a plain snapshot is valid");

    let mut nameless_owner = good.clone();
    nameless_owner.owner_name = Some("lead/child".to_owned());
    assert_eq!(
        nameless_owner.validate().unwrap_err(),
        "team task task-1 ownerName requires ownerId"
    );

    let mut zero = good.clone();
    zero.revision = 0;
    assert!(zero.validate().is_err());

    let mut empty_name = good.clone();
    empty_name.owner_id = Some(session("child"));
    empty_name.owner_name = Some(String::new());
    assert!(empty_name.validate().is_err());
}

#[test]
fn the_configuration_rejects_removed_keys_and_a_zero_cap() {
    assert_eq!(
        REMOVED_CONFIG_KEYS,
        ["maxMembers", "maxMessageBytes", "disposalTimeoutMs"]
    );
    let error = reject_removed_config(vec!["maxMembers".to_owned()]).unwrap_err();
    assert_eq!(error.code(), TeamErrorCode::InvalidConfig);
    assert!(error.to_string().contains("no longer supported"));
    reject_removed_config(vec!["maxTasks".to_owned()]).expect("a live key is fine");

    assert_eq!(
        TeamConfig::resolve(Some(0), None, Vec::new())
            .unwrap_err()
            .code(),
        TeamErrorCode::InvalidConfig
    );
    let resolved = TeamConfig::resolve(None, None, Vec::new()).expect("defaults");
    assert_eq!(resolved.max_tasks, DEFAULT_MAX_TASKS);
    assert!(!resolved.default_enabled);
    assert_eq!(
        TeamConfig::resolve(Some(4), Some(true), Vec::new()).expect("explicit"),
        TeamConfig {
            max_tasks: 4,
            default_enabled: true
        }
    );
    assert_eq!(
        TeamTaskBoard::new(session("root"), 0).unwrap_err().code(),
        TeamErrorCode::InvalidConfig
    );
}

#[test]
fn a_status_round_trips_through_its_wire_string() {
    for status in [
        TeamTaskStatus::Pending,
        TeamTaskStatus::InProgress,
        TeamTaskStatus::Completed,
        TeamTaskStatus::Deleted,
    ] {
        assert_eq!(TeamTaskStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(TeamTaskStatus::parse("nope"), None);
    for action in [
        TeamTaskAction::Claim,
        TeamTaskAction::Release,
        TeamTaskAction::Edit,
        TeamTaskAction::SetDependencies,
        TeamTaskAction::Complete,
        TeamTaskAction::Reopen,
        TeamTaskAction::Reassign,
        TeamTaskAction::Delete,
    ] {
        assert_eq!(TeamTaskAction::parse(action.as_str()), Some(action));
    }
    assert_eq!(TeamTaskAction::parse("nope"), None);
}

#[test]
fn every_error_code_round_trips_through_its_wire_string() {
    for code in [
        TeamErrorCode::Disabled,
        TeamErrorCode::InvalidArgument,
        TeamErrorCode::InvalidWriteScope,
        TeamErrorCode::InvalidConfig,
        TeamErrorCode::TaskLimit,
        TeamErrorCode::TaskNotFound,
        TeamErrorCode::StaleRevision,
        TeamErrorCode::TaskDeleted,
        TeamErrorCode::TaskUnauthorized,
        TeamErrorCode::TaskAlreadyClaimed,
        TeamErrorCode::TaskBlocked,
        TeamErrorCode::InvalidTransition,
        TeamErrorCode::HasDependents,
        TeamErrorCode::DependencyCycle,
        TeamErrorCode::MemberNotFound,
        TeamErrorCode::LeadRequired,
        TeamErrorCode::PersistenceUnconfirmed,
    ] {
        assert_eq!(TeamErrorCode::parse(code.as_str()), Some(code));
        assert!(code.as_str().starts_with("TEAM_"));
    }
    assert_eq!(TeamErrorCode::parse("TEAM_UNKNOWN"), None);
}

#[test]
fn the_board_renders_a_digest_that_names_the_owner_and_each_blocker() {
    let (mut board, lead) = board();
    let first = board
        .create(&lead, request("first", "first"))
        .expect("enabled");
    let mut second_request = request("second", "second");
    second_request.blocked_by = vec![first.id.clone()];
    board.create(&lead, second_request).expect("enabled");

    let rendered = render_board(&board.list().expect("enabled"));
    assert!(rendered.contains("[pending] rev 1 second"), "{rendered}");
    assert!(rendered.contains("blocked by task-1"), "{rendered}");
    assert!(rendered.contains("owner=-"), "{rendered}");
    assert_eq!(render_board(&[]), "(no tasks)");
}

#[test]
fn a_task_list_serializes_its_rows_and_omits_an_absent_cursor() {
    let list = TeamTaskList {
        tasks: Vec::new(),
        next_cursor: None,
    };
    assert_eq!(list.to_json(), json!({ "tasks": [] }));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod team {
    pub use super::team_gap as board;
}
