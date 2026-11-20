//! Tests for the MCP process lifecycle record and its restart policy.
//!
//! The record exists to answer one question the other MCP layers cannot: a server process died — is the
//! fleet allowed to start it again? Every test here is a statement about that answer. The three exit
//! reasons are tested as three independent properties, because the policy is defined by the pair
//! "reason, restart" and a test that only exercised the crash path would pass with a `should_restart`
//! that returned `true` unconditionally. The budget is tested to exhaustion, because the interesting
//! failure of a restart budget is not that it permits one restart but that it eventually stops.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from `lib.rs` is
//! a separate change. When it is exported, this can become
//! `use girder_core::mcp::process::{ExitReason, Fleet, FleetError, Lifecycle, State};`.

#[path = "../src/mcp/process.rs"]
pub mod mcp_process;

use mcp_process::{ExitReason, Fleet, FleetError, Lifecycle, State};

/// A server that started at t=0 and completed its handshake at t=100.
fn ready_server(name: &str) -> Lifecycle {
    let mut lifecycle = Lifecycle::new(name);
    lifecycle.started(0);
    lifecycle.ready(100);
    lifecycle
}

/// A server that crashed at t=200 after a successful handshake.
fn crashed_server(name: &str) -> Lifecycle {
    let mut lifecycle = ready_server(name);
    lifecycle.exited(200, Some(1), ExitReason::Crashed);
    lifecycle
}

/// Records a crash for a server already in the fleet.
///
/// The fleet owns its records, so a crash observed between two restarts is expressed by taking the
/// record, applying the event, and putting it back — which is also what a supervisor does when it
/// reaps a child it started.
fn record_crash(fleet: &mut Fleet, server: &str, now_ms: u64) {
    let mut lifecycle = match fleet.get(server) {
        Some(lifecycle) => lifecycle.clone(),
        None => panic!("`{server}` is not in the fleet"),
    };
    lifecycle.exited(now_ms, Some(1), ExitReason::Crashed);
    fleet.add(lifecycle);
}

#[test]
fn a_new_record_is_started_and_not_ready() {
    let lifecycle = Lifecycle::new("search");
    assert_eq!(lifecycle.server(), "search");
    assert_eq!(lifecycle.state(), &State::Started);
    assert!(!lifecycle.is_ready());
    assert_eq!(lifecycle.restarts(), 0);
}

#[test]
fn start_then_ready_then_exit_walks_the_whole_lifecycle() {
    let mut lifecycle = Lifecycle::new("search");
    lifecycle.started(0);
    assert_eq!(lifecycle.state(), &State::Started);

    lifecycle.ready(250);
    assert_eq!(lifecycle.state(), &State::Ready);
    assert!(lifecycle.is_ready());
    assert_eq!(lifecycle.startup_ms(), Some(250));

    lifecycle.exited(1_000, Some(0), ExitReason::Clean);
    assert_eq!(
        lifecycle.state(),
        &State::Exited {
            code: Some(0),
            reason: ExitReason::Clean,
        }
    );
    assert!(!lifecycle.is_ready());
    assert_eq!(lifecycle.exited_at_ms(), Some(1_000));
}

#[test]
fn a_signal_death_keeps_its_absent_status_code() {
    // A process killed by a signal has no exit status; a fabricated one would be indistinguishable from
    // a process that really exited with that number.
    let mut lifecycle = ready_server("search");
    lifecycle.exited(300, None, ExitReason::Crashed);
    assert_eq!(
        lifecycle.state(),
        &State::Exited {
            code: None,
            reason: ExitReason::Crashed,
        }
    );
}

#[test]
fn a_crash_is_restarted() {
    // The core case: a fault nobody chose is the one thing a retry is for.
    let lifecycle = crashed_server("search");
    assert!(lifecycle.should_restart(3));

    let mut fleet = Fleet::new(3);
    fleet.add(lifecycle);
    assert_eq!(fleet.restart("search", 500), Ok(()));

    let lifecycle = fleet.get("search").expect("the server is in the fleet");
    assert_eq!(lifecycle.state(), &State::Started);
    assert_eq!(lifecycle.restarts(), 1);
    assert_eq!(lifecycle.started_at_ms(), Some(500));
    assert!(!lifecycle.is_ready());
    assert!(fleet.crashed().is_empty());
}

#[test]
fn a_clean_exit_is_not_restarted() {
    // A server that finished is done. Restarting it re-runs completed work and turns a one-shot server
    // into a daemon nobody asked for.
    let mut lifecycle = ready_server("search");
    lifecycle.exited(200, Some(0), ExitReason::Clean);
    assert!(!lifecycle.should_restart(10));

    let mut fleet = Fleet::new(10);
    fleet.add(lifecycle);
    assert_eq!(
        fleet.restart("search", 300),
        Err(FleetError::Exhausted {
            server: "search".to_owned(),
            restarts: 0,
        })
    );
    assert_eq!(
        fleet.get("search").expect("still present").state(),
        &State::Exited {
            code: Some(0),
            reason: ExitReason::Clean,
        }
    );
}

#[test]
fn a_kill_is_not_restarted() {
    // We killed it — a startup timeout, a shutdown, a teardown. Restarting it resurrects the process
    // that decision was made about.
    let mut lifecycle = ready_server("search");
    lifecycle.exited(200, Some(143), ExitReason::Killed);
    assert!(!lifecycle.should_restart(10));

    let mut fleet = Fleet::new(10);
    fleet.add(lifecycle);
    assert_eq!(
        fleet.restart("search", 300),
        Err(FleetError::Exhausted {
            server: "search".to_owned(),
            restarts: 0,
        })
    );
    assert_eq!(fleet.get("search").expect("still present").restarts(), 0);
}

#[test]
fn the_restart_budget_runs_out() {
    // A server that crashes on every start must not be restarted forever: two allowed restarts means a
    // third crash is final, and the error reports how much was spent.
    let mut fleet = Fleet::new(2);
    fleet.add(crashed_server("flaky"));

    assert_eq!(fleet.restart("flaky", 1_000), Ok(()));
    assert_eq!(
        fleet.restart("flaky", 1_100),
        Err(FleetError::Exhausted {
            server: "flaky".to_owned(),
            restarts: 1,
        }),
        "a restart in progress is not a crash"
    );

    // The restarted process crashes again, twice, and the budget is spent after the second.
    record_crash(&mut fleet, "flaky", 1_200);
    assert_eq!(fleet.restart("flaky", 1_300), Ok(()));

    record_crash(&mut fleet, "flaky", 1_400);
    assert_eq!(
        fleet.restart("flaky", 1_500),
        Err(FleetError::Exhausted {
            server: "flaky".to_owned(),
            restarts: 2,
        })
    );
    assert_eq!(
        fleet.get("flaky").expect("still present").restarts(),
        2,
        "a refused restart must not spend budget"
    );
}

#[test]
fn a_zero_budget_never_restarts() {
    // "Never restart" must be expressible as a number rather than as a special case.
    let mut fleet = Fleet::new(0);
    fleet.add(crashed_server("search"));
    assert_eq!(
        fleet.restart("search", 300),
        Err(FleetError::Exhausted {
            server: "search".to_owned(),
            restarts: 0,
        })
    );
}

#[test]
fn startup_ms_measures_the_handshake_and_refuses_to_invent_one() {
    let mut lifecycle = Lifecycle::new("slow");
    assert_eq!(lifecycle.startup_ms(), None, "nothing has started yet");

    lifecycle.started(1_000);
    assert_eq!(
        lifecycle.startup_ms(),
        None,
        "still handshaking is not a duration"
    );

    lifecycle.ready(9_000);
    assert_eq!(lifecycle.startup_ms(), Some(8_000));

    // A restart clears the previous handshake: the old duration describes a process that is gone.
    lifecycle.exited(9_500, Some(1), ExitReason::Crashed);
    lifecycle.started(10_000);
    assert_eq!(lifecycle.startup_ms(), None);
    lifecycle.ready(10_050);
    assert_eq!(lifecycle.startup_ms(), Some(50));
}

#[test]
fn startup_ms_saturates_when_the_clock_goes_backwards() {
    let mut lifecycle = Lifecycle::new("skewed");
    lifecycle.started(5_000);
    lifecycle.ready(4_000);
    assert_eq!(lifecycle.startup_ms(), Some(0));
}

#[test]
fn a_late_handshake_after_an_exit_is_ignored() {
    // A reader draining a dead process's pipe can hand back an `initialize` reply after the exit was
    // observed. Marking the server ready on it would advertise a process that cannot answer.
    let mut lifecycle = ready_server("search");
    lifecycle.exited(200, Some(1), ExitReason::Crashed);
    lifecycle.ready(250);
    assert_eq!(
        lifecycle.state(),
        &State::Exited {
            code: Some(1),
            reason: ExitReason::Crashed,
        }
    );
}

#[test]
fn the_first_exit_report_wins() {
    // A kill and a reaper can race. The first report is the one that observed the cause; a later one
    // would relabel a deliberate kill as a crash and make the server restartable.
    let mut lifecycle = ready_server("search");
    lifecycle.exited(200, Some(143), ExitReason::Killed);
    lifecycle.exited(201, Some(1), ExitReason::Crashed);
    assert!(!lifecycle.should_restart(5));
    assert_eq!(
        lifecycle.state(),
        &State::Exited {
            code: Some(143),
            reason: ExitReason::Killed,
        }
    );
}

#[test]
fn the_fleet_aggregates_states() {
    let mut fleet = Fleet::new(3);
    assert!(fleet.is_empty());
    assert_eq!(fleet.max_restarts(), 3);

    fleet.add(ready_server("alpha"));
    fleet.add(crashed_server("beta"));
    let mut handshaking = Lifecycle::new("gamma");
    handshaking.started(0);
    fleet.add(handshaking);
    let mut stopped = ready_server("delta");
    stopped.exited(200, Some(0), ExitReason::Clean);
    fleet.add(stopped);

    assert_eq!(fleet.len(), 4);
    assert!(!fleet.is_empty());
    assert_eq!(fleet.names(), vec!["alpha", "beta", "delta", "gamma"]);
    assert_eq!(
        fleet.ready_count(),
        1,
        "handshaking, crashed, and cleanly exited servers are not capacity"
    );
    assert_eq!(fleet.crashed(), vec!["beta"]);
}

#[test]
fn adding_a_server_twice_replaces_its_record() {
    let mut fleet = Fleet::new(3);
    fleet.add(ready_server("search"));
    let mut replacement = Lifecycle::new("search");
    replacement.started(900);
    fleet.add(replacement);

    assert_eq!(fleet.len(), 1);
    assert_eq!(
        fleet.get("search").expect("present").state(),
        &State::Started,
        "the record describes the process running now"
    );
}

#[test]
fn an_unknown_server_is_an_error_and_nothing_is_invented() {
    let mut fleet = Fleet::new(3);
    fleet.add(ready_server("search"));

    assert_eq!(
        fleet.restart("missing", 100),
        Err(FleetError::Unknown {
            server: "missing".to_owned(),
        })
    );
    assert!(fleet.get("missing").is_none());
    assert_eq!(fleet.len(), 1);
    assert_eq!(fleet.ready_count(), 1, "the fleet is untouched");
}

#[test]
fn exit_reasons_have_stable_names_for_logs() {
    // The names appear in operator-facing logs; renaming one silently would break a log search.
    assert_eq!(ExitReason::Clean.as_str(), "clean");
    assert_eq!(ExitReason::Crashed.as_str(), "crashed");
    assert_eq!(ExitReason::Killed.as_str(), "killed");
    assert_eq!(State::Started.as_str(), "started");
    assert_eq!(State::Ready.as_str(), "ready");
    assert_eq!(
        State::Exited {
            code: None,
            reason: ExitReason::Clean
        }
        .as_str(),
        "exited"
    );
}

#[test]
fn the_fleet_error_names_the_server_it_refused() {
    // A restart failure has to say *which* server, because a fleet restarts many and the caller only
    // learns that one of them was not started.
    let unknown = FleetError::Unknown {
        server: "search".to_owned(),
    };
    assert!(unknown.to_string().contains("search"));
    let exhausted = FleetError::Exhausted {
        server: "flaky".to_owned(),
        restarts: 2,
    };
    assert!(exhausted.to_string().contains("flaky"));
    assert!(exhausted.to_string().contains('2'));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod mcp {
    pub use super::mcp_process as process;
}
