//! Tests for the goal state machine (M4 of the v2 plan).
//!
//! Two properties dominate. A restart must not resume an objective on its own, and two concurrent
//! edits must not silently overwrite each other.

use girder_core::{
    Activation, BlockReason, DEFAULT_MAX_GOAL_ROUNDS, Goal, GoalError, GoalId, GoalRef, GoalState,
    Phase,
};

fn id() -> GoalId {
    GoalId::new("g1").expect("valid")
}

fn state_with_goal() -> (GoalState, Goal) {
    let mut state = GoalState::new();
    let goal = state
        .create(id(), "port the project", None, 1000)
        .expect("created");
    (state, goal)
}

// ---- creation ---------------------------------------------------------------------------------

#[test]
fn a_new_goal_is_active_and_armed() {
    // The user just asked for it, so automatic continuation is what they want.
    let (state, goal) = state_with_goal();

    assert_eq!(goal.phase, Phase::Active);
    assert_eq!(goal.revision, 1);
    assert_eq!(state.activation(), Activation::Armed);
    assert!(state.may_continue());
}

#[test]
fn an_empty_objective_is_refused() {
    // A goal whose objective is blank gives the loop nothing to work toward.
    let mut state = GoalState::new();
    let error = state.create(id(), "   ", None, 1000).expect_err("empty");

    assert!(matches!(error, GoalError::EmptyObjective));
}

#[test]
fn an_omitted_round_cap_uses_the_default() {
    let (_, goal) = state_with_goal();
    assert_eq!(goal.max_rounds, DEFAULT_MAX_GOAL_ROUNDS);
}

#[test]
fn a_zero_round_cap_uses_the_default() {
    // An explicit zero would make the goal unable to take a single round, which is a trap rather
    // than a policy.
    let mut state = GoalState::new();
    let goal = state.create(id(), "obj", Some(0), 1000).unwrap();

    assert_eq!(goal.max_rounds, DEFAULT_MAX_GOAL_ROUNDS);
}

#[test]
fn an_explicit_round_cap_is_kept() {
    let mut state = GoalState::new();
    let goal = state.create(id(), "obj", Some(3), 1000).unwrap();

    assert_eq!(goal.max_rounds, 3);
}

#[test]
fn creating_while_an_open_goal_exists_is_refused() {
    // Two open goals would make "the current goal" ambiguous.
    let (mut state, _) = state_with_goal();
    let error = state
        .create(id(), "another", None, 2000)
        .expect_err("already open");

    assert!(matches!(
        error,
        GoalError::InvalidTransition {
            operation: "create",
            ..
        }
    ));
}

#[test]
fn creating_after_completion_is_allowed() {
    // A finished goal is terminal but not permanent; the user starts a new one.
    let (mut state, goal) = state_with_goal();
    state.complete(&goal.reference()).unwrap();

    state
        .create(id(), "next objective", None, 2000)
        .expect("a new goal");
}

// ---- the restart rule -------------------------------------------------------------------------

#[test]
fn a_restored_goal_is_disarmed() {
    // The essential restart rule: the phase survives, the activation does not. A process that
    // resumed an objective on its own because the previous one was mid-flight would act without
    // being asked.
    let (_, goal) = state_with_goal();
    let mut restored = GoalState::new();
    restored.restore(goal);

    assert_eq!(restored.current().map(|g| g.phase), Some(Phase::Active));
    assert_eq!(restored.activation(), Activation::Disarmed);
    assert!(
        !restored.may_continue(),
        "a restart must not continue by itself"
    );
}

#[test]
fn a_restored_goal_can_be_resumed_by_the_user() {
    let (_, goal) = state_with_goal();
    let mut restored = GoalState::new();
    restored.restore(goal.clone());

    let resumed = restored.resume(&goal.reference()).expect("resumed");

    assert_eq!(restored.activation(), Activation::Armed);
    assert!(restored.may_continue());
    assert_eq!(resumed.revision, goal.revision + 1);
}

// ---- transitions ------------------------------------------------------------------------------

#[test]
fn pause_disarms() {
    let (mut state, goal) = state_with_goal();

    let paused = state.pause(&goal.reference()).expect("paused");

    assert_eq!(paused.phase, Phase::Paused);
    assert_eq!(state.activation(), Activation::Disarmed);
    assert!(!state.may_continue());
}

#[test]
fn resume_rearms_a_paused_goal() {
    let (mut state, goal) = state_with_goal();
    let paused = state.pause(&goal.reference()).unwrap();

    let resumed = state.resume(&paused.reference()).expect("resumed");

    assert_eq!(resumed.phase, Phase::Active);
    assert_eq!(state.activation(), Activation::Armed);
}

#[test]
fn resume_refuses_an_already_armed_active_goal() {
    // Recording a no-op revision would make the log claim a change that did not happen.
    let (mut state, goal) = state_with_goal();
    let error = state.resume(&goal.reference()).expect_err("already armed");

    assert!(matches!(error, GoalError::AlreadyArmed { .. }));
}

#[test]
fn a_completed_goal_is_terminal() {
    // Reopening it would make "complete" mean "probably complete".
    let (mut state, goal) = state_with_goal();
    let completed = state.complete(&goal.reference()).unwrap();

    assert_eq!(completed.phase, Phase::Complete);
    assert!(matches!(
        state.resume(&completed.reference()),
        Err(GoalError::InvalidTransition { .. })
    ));
    assert!(matches!(
        state.pause(&completed.reference()),
        Err(GoalError::InvalidTransition { .. })
    ));
    assert!(matches!(
        state.complete(&completed.reference()),
        Err(GoalError::InvalidTransition { .. })
    ));
}

#[test]
fn block_records_a_reason_and_disarms() {
    let (mut state, goal) = state_with_goal();
    let reason = BlockReason::new("needs-input", "the user must choose a branch").expect("valid");

    let blocked = state
        .block(&goal.reference(), reason.clone())
        .expect("blocked");

    assert_eq!(blocked.phase, Phase::Blocked);
    assert_eq!(blocked.blocked_reason, Some(reason));
    assert_eq!(state.activation(), Activation::Disarmed);
}

#[test]
fn a_block_with_an_empty_message_is_refused() {
    // A goal that stops for a reason nobody can read is worse than one that keeps trying.
    let (mut state, goal) = state_with_goal();
    let blank = BlockReason {
        code: "x".to_owned(),
        message: "  ".to_owned(),
    };

    let error = state.block(&goal.reference(), blank).expect_err("blank");

    assert!(matches!(error, GoalError::EmptyBlockReason));
}

#[test]
fn unblocking_clears_the_reason() {
    // A stale reason would show on a goal that is no longer blocked.
    let (mut state, goal) = state_with_goal();
    let blocked = state
        .block(&goal.reference(), BlockReason::new("x", "why").unwrap())
        .unwrap();
    let resumed = state.resume(&blocked.reference()).unwrap();

    assert!(resumed.blocked_reason.is_none());
}

#[test]
fn only_an_active_goal_can_be_blocked() {
    let (mut state, goal) = state_with_goal();
    let paused = state.pause(&goal.reference()).unwrap();

    assert!(matches!(
        state.block(&paused.reference(), BlockReason::new("x", "why").unwrap()),
        Err(GoalError::InvalidTransition { .. })
    ));
}

#[test]
fn a_completed_goal_cannot_be_edited() {
    // Editing a finished goal would let it mean something the user never approved.
    let (mut state, goal) = state_with_goal();
    let completed = state.complete(&goal.reference()).unwrap();

    assert!(matches!(
        state.edit(
            &completed.reference(),
            Some("new objective".to_owned()),
            None
        ),
        Err(GoalError::InvalidTransition { .. })
    ));
}

// ---- revisions --------------------------------------------------------------------------------

#[test]
fn every_mutation_increments_the_revision() {
    let (mut state, goal) = state_with_goal();
    assert_eq!(goal.revision, 1);

    let paused = state.pause(&goal.reference()).unwrap();
    assert_eq!(paused.revision, 2);

    let resumed = state.resume(&paused.reference()).unwrap();
    assert_eq!(resumed.revision, 3);
}

#[test]
fn a_stale_revision_is_refused() {
    // Two clients can hold a view and both edit; without compare-and-set the second write wins and
    // the first client's change vanishes with no indication.
    let (mut state, goal) = state_with_goal();
    let original = goal.reference();
    let _ = state.pause(&original).unwrap();

    let error = state.resume(&original).expect_err("stale");

    let GoalError::StaleRevision {
        expected, actual, ..
    } = error
    else {
        panic!("expected StaleRevision, got {error:?}");
    };
    assert_eq!(expected, 1);
    assert_eq!(actual, 2);
}

#[test]
fn a_reference_to_another_goal_is_refused() {
    let (mut state, _) = state_with_goal();
    let other = GoalRef {
        id: GoalId::new("other").unwrap(),
        revision: 1,
    };

    assert!(matches!(
        state.pause(&other),
        Err(GoalError::NotCurrent { .. })
    ));
}

#[test]
fn the_free_check_names_the_stale_revision() {
    let (_, goal) = state_with_goal();
    let stale = GoalRef {
        id: goal.id.clone(),
        revision: 99,
    };

    let error = girder_core::check_goal_reference(&goal, &stale).expect_err("stale");
    assert!(matches!(
        error,
        GoalError::StaleRevision { expected: 99, .. }
    ));
}

// ---- rounds -----------------------------------------------------------------------------------

#[test]
fn a_round_increments_the_counter() {
    let (mut state, goal) = state_with_goal();

    let after = state.start_round(2000).expect("round");

    assert_eq!(after.rounds_started, 1);
    assert!(!after.rounds_exhausted() || after.max_rounds == 1);
    let _ = goal;
}

#[test]
fn the_round_budget_stops_continuation() {
    // A goal whose objective is unreachable would otherwise continue forever, spending tokens every
    // round with no way to notice.
    let mut state = GoalState::new();
    state.create(id(), "obj", Some(2), 1000).unwrap();

    state.start_round(2000).unwrap();
    state.start_round(3000).unwrap();
    let goal = state.current().unwrap();
    assert!(goal.rounds_exhausted());

    let error = state.start_round(4000).expect_err("exhausted");
    assert!(matches!(
        error,
        GoalError::InvalidTransition {
            operation: "continue",
            ..
        }
    ));
}

#[test]
fn a_paused_goal_cannot_start_a_round() {
    let (mut state, goal) = state_with_goal();
    state.pause(&goal.reference()).unwrap();

    assert!(matches!(
        state.start_round(2000),
        Err(GoalError::InvalidTransition { .. })
    ));
}

#[test]
fn a_disarmed_goal_cannot_start_a_round() {
    // The restart case: active on disk, disarmed in this process.
    let (_, goal) = state_with_goal();
    let mut restored = GoalState::new();
    restored.restore(goal);

    assert!(matches!(
        restored.start_round(2000),
        Err(GoalError::InvalidTransition { .. })
    ));
}

// ---- clearing ---------------------------------------------------------------------------------

#[test]
fn clearing_removes_the_goal_and_disarms() {
    let (mut state, _) = state_with_goal();

    let cleared = state.clear();

    assert!(cleared.is_some());
    assert!(state.current().is_none());
    assert_eq!(state.activation(), Activation::Disarmed);
}

#[test]
fn clearing_an_empty_state_returns_nothing() {
    let mut state = GoalState::new();
    assert!(state.clear().is_none());
}

// ---- phase predicates --------------------------------------------------------------------------

#[test]
fn phase_predicates_match_the_table() {
    assert!(Phase::Active.is_resumable());
    assert!(Phase::Paused.is_resumable());
    assert!(Phase::Blocked.is_resumable());
    assert!(!Phase::Complete.is_resumable());

    assert!(Phase::Active.is_open());
    assert!(Phase::Blocked.is_open());
    assert!(!Phase::Complete.is_open());
}

#[test]
fn phase_wire_names_match_upstream() {
    assert_eq!(Phase::Active.as_str(), "active");
    assert_eq!(Phase::Paused.as_str(), "paused");
    assert_eq!(Phase::Blocked.as_str(), "blocked");
    assert_eq!(Phase::Complete.as_str(), "complete");
}
