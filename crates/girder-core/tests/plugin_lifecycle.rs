//! Tests for the plugin lifecycle record.
//!
//! Three properties carry the weight here, and each is tested as a property rather than only on the
//! path that motivated it. *Discovery is not enough to run*: `Discovered -> Running` must be refused,
//! because the approval is the fact that makes running legitimate. *Removal is final*: no pair with
//! `Removed` on the left may pass, including the pairs nobody would think to try, which is why the
//! sweep enumerates every state rather than listing the plausible attempts. *A failure is not a dead
//! end*: retrying is the ordinary response to a missing dependency, so `Failed` must reach
//! `Starting` again — while `Failed -> Running` stays refused, because a retry that skipped its own
//! start would record a success nobody observed.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from `lib.rs`
//! is a separate change. When it is exported, this can become
//! `use girder_core::plugin_lifecycle::{allows, Cause, History, State, Transition};`.

#[path = "../src/plugin_lifecycle.rs"]
mod plugin_lifecycle;

use plugin_lifecycle::{Cause, History, State, Transition, TransitionError, allows};

/// The reason carried by the `Failed` fixture.
///
/// Fixed, so a sweep that compares states compares the states it means to. The table may not depend
/// on *which* failure occurred, and [`the_reason_does_not_change_the_verdict`] varies it.
const FAILED_REASON: &str = "needs [\"db\"]";

/// The `Failed` state, with the fixture reason.
fn failed() -> State {
    State::Failed {
        reason: FAILED_REASON.to_owned(),
    }
}

/// The `Failed` state, with its own reason.
fn failed_because(reason: &str) -> State {
    State::Failed {
        reason: reason.to_owned(),
    }
}

/// Every state, once, for exhaustive sweeps.
fn all_states() -> Vec<State> {
    vec![
        State::Discovered,
        State::Approved,
        State::Starting,
        State::Running,
        State::Stopped,
        failed(),
        State::Removed,
    ]
}

/// A transition with the usual "operator started it" cause; the table is keyed on states, so the
/// cause cannot change a verdict.
fn transition(from: State, to: State) -> Transition {
    Transition {
        from,
        to,
        cause: Cause::OperatorStarted,
    }
}

/// Applies one step, taking `from` from the record — what a real caller does.
fn advance(history: &mut History, to: State, cause: Cause) -> Result<(), TransitionError> {
    let from = history.current().clone();
    history.apply(Transition { from, to, cause })
}

/// A record that walked `path` from a fresh discovery.
fn walked(path: &[State]) -> History {
    let mut history = History::new();
    for to in path {
        advance(&mut history, to.clone(), Cause::OperatorStarted)
            .expect("path is legal by construction");
    }
    history
}

/// A record whose current state is `target`, reached by the shortest legal path.
///
/// A record cannot be conjured into an arbitrary state — reaching `Running` without `Starting` is
/// exactly what the table forbids — so a test that wants to start somewhere has to walk there.
fn path_to(target: &State) -> History {
    let path: &[State] = match target {
        State::Discovered => &[],
        State::Approved => &[State::Approved],
        State::Starting => &[State::Approved, State::Starting],
        State::Running => &[State::Approved, State::Starting, State::Running],
        State::Stopped => &[
            State::Approved,
            State::Starting,
            State::Running,
            State::Stopped,
        ],
        State::Failed { .. } => &[State::Approved, State::Starting, failed()],
        State::Removed => &[State::Removed],
    };
    walked(path)
}

/// A short, reason-free name for a state, so the expected table can be written as pairs of words.
fn name(state: &State) -> &'static str {
    match state {
        State::Discovered => "discovered",
        State::Approved => "approved",
        State::Starting => "starting",
        State::Running => "running",
        State::Stopped => "stopped",
        State::Failed { .. } => "failed",
        State::Removed => "removed",
    }
}

/// The expected table, written out as the documented pairs.
///
/// Deliberately a second statement of the same rules, spelled as data rather than as a `match`: the
/// point of the sweep below is to catch the two disagreeing, so it may not derive its answer from
/// `allows`, and a copy of the implementation's shape would only catch typos in that shape.
const ALLOWED: &[(&str, &str)] = &[
    ("discovered", "approved"),
    ("discovered", "removed"),
    ("approved", "starting"),
    ("approved", "removed"),
    ("starting", "running"),
    ("starting", "failed"),
    ("running", "stopped"),
    ("running", "failed"),
    ("stopped", "starting"),
    ("stopped", "removed"),
    ("failed", "starting"),
    ("failed", "removed"),
];

/// Whether the documented table permits `from -> to`.
fn expected(from: &State, to: &State) -> bool {
    ALLOWED.contains(&(name(from), name(to)))
}

// ---- the table, exhaustively -------------------------------------------------------------------

#[test]
fn table_matches_the_documented_rules_for_every_pair() {
    let states = all_states();
    for from in &states {
        for to in &states {
            assert_eq!(
                allows(from, to),
                expected(from, to),
                "table disagrees with the documented rules for {from} -> {to}"
            );
        }
    }
}

#[test]
fn the_table_is_not_trivially_permissive_or_trivially_closed() {
    // Guards the sweep above: if `allows` returned one value for everything, the sweep would pass
    // by matching an equally broken `expected`. The number of permitted pairs is pinned instead.
    let states = all_states();
    let permitted = states
        .iter()
        .flat_map(|from| states.iter().map(move |to| (from, to)))
        .filter(|(from, to)| allows(from, to))
        .count();
    assert_eq!(
        permitted, 12,
        "the table should permit exactly twelve pairs"
    );
}

#[test]
fn the_reason_does_not_change_the_verdict() {
    let with_fixture = failed();
    let with_other = failed_because("disk full");
    for to in all_states() {
        assert_eq!(
            allows(&with_fixture, &to),
            allows(&with_other, &to),
            "the verdict out of failed must not depend on the reason, for {to}"
        );
    }
}

#[test]
fn the_cause_never_changes_the_verdict() {
    // The table is keyed on states, so no label may talk a record into a move it would otherwise
    // refuse — or out of one it would otherwise allow.
    let causes = [
        Cause::OperatorApproved,
        Cause::OperatorStarted,
        Cause::OperatorStopped,
        Cause::OperatorRemoved,
        Cause::StartSucceeded,
        Cause::StartFailed("boom".to_owned()),
        Cause::DependencyLost("db".to_owned()),
    ];
    let states = all_states();
    for from in &states {
        for to in &states {
            let verdict = allows(from, to);
            for cause in &causes {
                let mut history = path_to(from);
                let result = history.apply(Transition {
                    from: from.clone(),
                    to: to.clone(),
                    cause: cause.clone(),
                });
                assert_eq!(
                    result.is_ok(),
                    verdict,
                    "{from} -> {to} under {cause:?} disagreed with the table"
                );
            }
        }
    }
}

// ---- the normal path ---------------------------------------------------------------------------

#[test]
fn discovery_to_running_takes_approval_and_start() {
    let mut history = History::new();
    assert_eq!(history.current(), &State::Discovered);

    advance(&mut history, State::Approved, Cause::OperatorApproved).expect("legal");
    assert_eq!(history.current(), &State::Approved);

    advance(&mut history, State::Starting, Cause::OperatorStarted).expect("legal");
    assert_eq!(history.current(), &State::Starting);

    advance(&mut history, State::Running, Cause::StartSucceeded).expect("legal");
    assert_eq!(history.current(), &State::Running);

    advance(&mut history, State::Stopped, Cause::OperatorStopped).expect("legal");
    assert_eq!(history.current(), &State::Stopped);

    // A stopped plugin is restartable: stopping is not removal.
    advance(&mut history, State::Starting, Cause::OperatorStarted).expect("legal");
    advance(&mut history, State::Running, Cause::StartSucceeded).expect("legal");
    advance(&mut history, State::Stopped, Cause::OperatorStopped).expect("legal");
    advance(&mut history, State::Removed, Cause::OperatorRemoved).expect("legal");

    assert_eq!(history.current(), &State::Removed);
    assert!(history.current().is_terminal());
    assert_eq!(
        history.transitions().len(),
        8,
        "every accepted transition is kept"
    );
}

#[test]
fn skipping_approval_is_refused() {
    let mut history = History::new();
    let error = history
        .apply(Transition {
            from: State::Discovered,
            to: State::Running,
            cause: Cause::OperatorStarted,
        })
        .expect_err("a plugin may not run before it is approved");

    assert_eq!(
        error,
        TransitionError::Illegal {
            from: "discovered".to_owned(),
            to: "running".to_owned(),
        }
    );
    // The refusal changed nothing: an operator may still approve afterwards.
    assert_eq!(history.current(), &State::Discovered);
    assert!(history.transitions().is_empty());
}

#[test]
fn skipping_the_start_is_refused_even_from_an_approved_plugin() {
    let mut history = walked(&[State::Approved]);
    let error = history
        .apply(Transition {
            from: State::Approved,
            to: State::Running,
            cause: Cause::StartSucceeded,
        })
        .expect_err("approval alone does not make a plugin run");

    assert_eq!(
        error,
        TransitionError::Illegal {
            from: "approved".to_owned(),
            to: "running".to_owned(),
        }
    );
    assert_eq!(history.transitions().len(), 1);
    assert_eq!(history.current(), &State::Approved);
}

#[test]
fn skipping_straight_to_stopped_from_approved_is_refused() {
    let mut history = walked(&[State::Approved]);
    assert!(
        history
            .apply(Transition {
                from: State::Approved,
                to: State::Stopped,
                cause: Cause::OperatorStopped,
            })
            .is_err(),
        "nothing ran, so nothing stopped"
    );
    assert_eq!(history.current(), &State::Approved);
}

// ---- removal is final --------------------------------------------------------------------------

#[test]
fn removed_is_terminal_for_every_destination() {
    for to in all_states() {
        assert!(
            !allows(&State::Removed, &to),
            "removed must not move to {to}"
        );
    }
    assert!(State::Removed.is_terminal());
}

#[test]
fn removed_is_terminal_through_the_record_and_not_only_the_predicate() {
    let mut history = walked(&[State::Removed]);
    assert_eq!(history.current(), &State::Removed);

    for to in all_states() {
        let error = history
            .apply(Transition {
                from: State::Removed,
                to: to.clone(),
                cause: Cause::OperatorApproved,
            })
            .expect_err("removal is final");
        assert_eq!(
            error,
            TransitionError::Illegal {
                from: "removed".to_owned(),
                to: to.to_string(),
            }
        );
    }
    // Refusals left the record alone, so the single removal is still the only transition.
    assert_eq!(history.transitions().len(), 1);
}

#[test]
fn only_removed_is_terminal() {
    for state in all_states() {
        let terminal = state.is_terminal();
        let has_successor = all_states().iter().any(|to| allows(&state, to));
        assert_eq!(
            terminal, !has_successor,
            "{state} reports terminal={terminal} but has_successor={has_successor}"
        );
    }
}

#[test]
fn a_running_plugin_is_stopped_before_removal() {
    let mut history = walked(&[State::Approved, State::Starting, State::Running]);
    let error = history
        .apply(transition(State::Running, State::Removed))
        .expect_err("removing a running plugin leaves the shutdown unrecorded, so stop first");

    assert_eq!(
        error,
        TransitionError::Illegal {
            from: "running".to_owned(),
            to: "removed".to_owned(),
        }
    );
    assert_eq!(history.current(), &State::Running);
}

// ---- failure and retry -------------------------------------------------------------------------

#[test]
fn a_failure_can_be_retried_through_starting() {
    let mut history = walked(&[State::Approved, State::Starting, failed()]);
    assert_eq!(history.current(), &failed());

    advance(&mut history, State::Starting, Cause::OperatorStarted)
        .expect("a retry is legal, and it goes through starting");
    advance(&mut history, State::Running, Cause::StartSucceeded).expect("the retry succeeded");

    assert_eq!(history.current(), &State::Running);

    let states: Vec<String> = history
        .transitions()
        .iter()
        .map(|step| step.to.to_string())
        .collect();
    assert_eq!(
        states,
        vec![
            "approved",
            "starting",
            "failed (needs [\"db\"])",
            "starting",
            "running"
        ],
        "the log shows the failed attempt rather than hiding it behind the success"
    );
}

#[test]
fn a_retry_that_fails_again_keeps_its_own_reason() {
    let first = failed_because("connection refused");
    let second = failed_because("timed out");
    let mut history = walked(&[State::Approved, State::Starting, first.clone()]);

    advance(&mut history, State::Starting, Cause::OperatorStarted).expect("a retry is legal");
    advance(
        &mut history,
        second.clone(),
        Cause::StartFailed("timed out".to_owned()),
    )
    .expect("a failed retry is legal");

    assert_eq!(history.current(), &second);
    // Both failures are still in the log, so the second does not erase the first.
    let failures = history
        .transitions()
        .iter()
        .filter(|step| matches!(step.to, State::Failed { .. }))
        .count();
    assert_eq!(failures, 2);
}

#[test]
fn failed_to_running_is_refused() {
    let mut history = walked(&[State::Approved, State::Starting, failed()]);

    let error = history
        .apply(Transition {
            from: failed(),
            to: State::Running,
            cause: Cause::StartSucceeded,
        })
        .expect_err("a retry must pass through starting so the attempt is recorded");

    assert_eq!(
        error,
        TransitionError::Illegal {
            from: "failed (needs [\"db\"])".to_owned(),
            to: "running".to_owned(),
        }
    );
    assert!(!allows(&failed_because("anything"), &State::Running));
}

#[test]
fn failed_can_be_removed_without_being_retried() {
    let mut history = walked(&[State::Approved, State::Starting, failed()]);
    advance(&mut history, State::Removed, Cause::OperatorRemoved)
        .expect("an operator may give up on a broken plugin");
    assert_eq!(history.current(), &State::Removed);
}

// ---- a lost dependency -------------------------------------------------------------------------

#[test]
fn a_lost_dependency_fails_the_plugin_rather_than_stopping_it() {
    let mut history = walked(&[State::Approved, State::Starting, State::Running]);

    advance(
        &mut history,
        failed_because("dependency `db` disappeared"),
        Cause::DependencyLost("db".to_owned()),
    )
    .expect("losing a dependency is a failure, which is legal from running");

    assert_eq!(
        history.current(),
        &failed_because("dependency `db` disappeared"),
        "a lost dependency is not a normal stop, and must not be reported as one"
    );
    assert_ne!(history.current(), &State::Stopped);
    // A normal stop stays legal from running; the two outcomes are distinguished by the caller's
    // cause and by the state it records, not by one of the moves being forbidden.
    assert!(allows(&State::Running, &State::Stopped));
}

#[test]
fn dependency_loss_is_recorded_as_the_cause_with_its_name() {
    let mut history = walked(&[State::Approved, State::Starting, State::Running]);
    advance(
        &mut history,
        failed_because("dependency `db` disappeared"),
        Cause::DependencyLost("db".to_owned()),
    )
    .expect("legal");

    let last = history.transitions().last().expect("a transition exists");
    assert_eq!(
        last.cause,
        Cause::DependencyLost("db".to_owned()),
        "the name of the lost dependency survives in the log"
    );
}

// ---- the record --------------------------------------------------------------------------------

#[test]
fn history_keeps_every_transition_in_order() {
    let history = walked(&[
        State::Approved,
        State::Starting,
        State::Running,
        State::Stopped,
        State::Starting,
        State::Running,
        State::Stopped,
        State::Removed,
    ]);

    assert_eq!(history.transitions().len(), 8);
    assert_eq!(history.current(), &State::Removed);
    // Each transition's `to` is the next one's `from`: the log is a path, not a set.
    for pair in history.transitions().windows(2) {
        assert_eq!(pair[0].to, pair[1].from);
    }
    assert_eq!(
        history.transitions().first().map(|step| &step.from),
        Some(&State::Discovered)
    );
    assert_eq!(
        history.transitions().last().map(|step| &step.to),
        Some(&State::Removed)
    );
}

#[test]
fn a_fresh_record_is_discovered_and_empty() {
    let history = History::new();
    assert_eq!(history.current(), &State::Discovered);
    assert!(history.transitions().is_empty());
    assert_eq!(History::default().current(), &State::Discovered);
    assert!(History::default().transitions().is_empty());
}

#[test]
fn a_stale_caller_is_refused_without_touching_the_record() {
    let mut history = walked(&[State::Approved, State::Starting, State::Running]);

    // A caller that still believes the plugin is approved asks to start it again.
    let error = history
        .apply(Transition {
            from: State::Approved,
            to: State::Starting,
            cause: Cause::OperatorStarted,
        })
        .expect_err("the record is running, not approved");

    assert_eq!(
        error,
        TransitionError::Illegal {
            from: "approved".to_owned(),
            to: "starting".to_owned(),
        }
    );
    assert_eq!(history.current(), &State::Running);
    assert_eq!(history.transitions().len(), 3);
}

#[test]
fn cloning_a_history_does_not_share_its_future() {
    // `History` is a value: a clone taken before an edit must not observe the edit, otherwise a
    // caller could branch on a snapshot and be surprised. Cheap to state, easy to regress.
    let mut history = walked(&[State::Approved]);
    let snapshot = history.clone();
    advance(&mut history, State::Starting, Cause::OperatorStarted).expect("legal");

    assert_eq!(snapshot.current(), &State::Approved);
    assert_eq!(history.current(), &State::Starting);
    assert_eq!(snapshot.transitions().len(), 1);
}

// ---- error messages ----------------------------------------------------------------------------

#[test]
fn the_error_message_names_both_states() {
    let mut history = History::new();
    let error = history
        .apply(Transition {
            from: State::Discovered,
            to: State::Running,
            cause: Cause::OperatorStarted,
        })
        .expect_err("illegal");

    assert_eq!(
        error.to_string(),
        "illegal transition: discovered -> running"
    );
    assert!(
        error.to_string().contains("discovered"),
        "the source state is named"
    );
    assert!(
        error.to_string().contains("running"),
        "the destination state is named"
    );
}

#[test]
fn the_error_message_names_a_refused_intermediate_step() {
    let mut history = walked(&[State::Approved, State::Starting]);
    let error = history
        .apply(Transition {
            from: State::Starting,
            to: State::Removed,
            cause: Cause::OperatorRemoved,
        })
        .expect_err("a starting plugin cannot be removed");

    assert_eq!(error.to_string(), "illegal transition: starting -> removed");
    assert_eq!(history.current(), &State::Starting);
}

#[test]
fn the_error_message_renders_the_reason_of_a_failed_source() {
    // The message is built from `Display`, so a `Failed` source carries *why* it was in that state
    // — which is the only part of the message a reader cannot infer from the destination.
    let mut history = walked(&[State::Approved, State::Starting, failed()]);
    let error = history
        .apply(Transition {
            from: failed(),
            to: State::Approved,
            cause: Cause::OperatorApproved,
        })
        .expect_err("an approval is not something a failed plugin needs");

    assert_eq!(
        error.to_string(),
        "illegal transition: failed (needs [\"db\"]) -> approved"
    );
}

#[test]
fn a_failed_plugin_may_be_removed_without_a_retry() {
    let mut history = walked(&[State::Approved, State::Starting, failed()]);
    advance(&mut history, State::Removed, Cause::OperatorRemoved)
        .expect("giving up on a broken plugin is legal");
    assert_eq!(history.current(), &State::Removed);
}

#[test]
fn errors_compare_by_their_states() {
    let left = TransitionError::Illegal {
        from: "discovered".to_owned(),
        to: "running".to_owned(),
    };
    let right = TransitionError::Illegal {
        from: "discovered".to_owned(),
        to: "running".to_owned(),
    };
    let other = TransitionError::Illegal {
        from: "approved".to_owned(),
        to: "running".to_owned(),
    };
    assert_eq!(left, right);
    assert_ne!(left, other);
}

// ---- display -----------------------------------------------------------------------------------

#[test]
fn states_render_readably_including_the_failure_reason() {
    assert_eq!(State::Discovered.to_string(), "discovered");
    assert_eq!(State::Approved.to_string(), "approved");
    assert_eq!(State::Starting.to_string(), "starting");
    assert_eq!(State::Running.to_string(), "running");
    assert_eq!(State::Stopped.to_string(), "stopped");
    assert_eq!(State::Removed.to_string(), "removed");
    assert_eq!(failed_because("boom").to_string(), "failed (boom)");
}
