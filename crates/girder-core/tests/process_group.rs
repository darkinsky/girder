//! Tests for process-group containment.
//!
//! Every test drives real processes. A test double for `kill` would prove only that a function was
//! called; the property that matters is that a pid the harness no longer has a handle on is gone
//! from the process table afterwards.
//!
//! The properties, in the order they matter:
//!
//! - **a background child is gone when the command returns** — the core case. `sh -c "sleep 999 &"`
//!   exits immediately and leaves a survivor, which is exactly what `subprocess::run` strands today.
//! - **that survivor does not delay the caller** — it held the stdout pipe open, so a drain that
//!   ran before the group kill would block for the survivor's lifetime.
//! - **a timeout reports itself and kills the group**, not just the leader
//! - **a plain command still works**, so containment did not break the ordinary path
//! - **a missing program is an error, not a panic**
//! - **`any_alive` tracks the group**, and a guard that was never armed — or was handed the
//!   harness's own group — reports nothing and kills nothing
//!
//! These tests go through the crate's public surface — the `girder_core` root re-exports — rather
//! than including the module by path, so they fail if the crate stops exporting it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

use std::time::{Duration, Instant};

use girder_core::{GroupError, GroupGuard, run_isolated};

/// Whether `pid` is a *running* process.
///
/// Signal zero alone is not enough: a child that has been killed but not yet `wait`ed for is a
/// zombie, and the kernel still accepts a signal for it. The `/proc` state field is what separates
/// "exited, awaiting reaping" from "still working", and it is the same distinction `any_alive`
/// makes. `EPERM` still means the process is there.
fn alive(pid: i32) -> bool {
    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
        return false;
    }
    running(pid)
}

/// Whether `pid` is in a state other than exited, from `/proc/<pid>/stat`.
fn running(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rfind(')')
        .and_then(|close| stat[close + 1..].split_whitespace().next())
        .is_some_and(|state| state != "Z")
}

/// Parses the pid a test command printed.
fn pid_of(stdout: &[u8]) -> i32 {
    String::from_utf8_lossy(stdout)
        .trim()
        .parse()
        .expect("the fixture prints its pid")
}

/// Waits for `pid` to disappear, bounded, so a test can assert absence without racing the kernel.
fn gone_within(pid: i32, limit: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    !alive(pid)
}

#[tokio::test]
async fn a_plain_command_runs_and_reports_output() {
    let output = run_isolated(
        "sh",
        &["-c", "echo hello; echo oops >&2"],
        Duration::from_secs(10),
    )
    .await
    .expect("the command runs");

    assert!(output.status.success(), "status: {:?}", output.status);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "oops");
}

#[tokio::test]
async fn a_failing_command_reports_its_status_without_killing_the_caller() {
    let output = run_isolated("sh", &["-c", "exit 3"], Duration::from_secs(10))
        .await
        .expect("a non-zero exit is an outcome, not an error");

    assert_eq!(output.status.code(), Some(3));
}

#[tokio::test]
async fn a_background_child_is_killed_when_the_command_returns() {
    // The command exits at once; the `sleep` does not. Without the group kill this pid outlives the
    // call and the harness has no handle on it.
    let started = Instant::now();
    let output = run_isolated(
        "sh",
        &["-c", "sleep 999 & echo $!"],
        Duration::from_secs(30),
    )
    .await
    .expect("the command runs");
    let elapsed = started.elapsed();
    let background = pid_of(&output.stdout);

    assert!(output.status.success(), "status: {:?}", output.status);
    assert!(
        gone_within(background, Duration::from_secs(5)),
        "background pid {background} survived the group kill"
    );
    // The survivor held the stdout pipe open. Had the drain run before the kill, this call would
    // have waited for `sleep 999` and hit the 30s timeout instead of returning.
    assert!(
        elapsed < Duration::from_secs(10),
        "the call took {elapsed:?}, so the drain blocked on the background child"
    );
}

#[tokio::test]
async fn a_background_pipeline_is_killed_too() {
    // Two levels: the shell backgrounds a pipeline, so the survivor is not the shell's direct child
    // in the sense a `kill(pid)` would reach.
    let output = run_isolated(
        "sh",
        &["-c", "sh -c 'sleep 999 | cat' & echo $!"],
        Duration::from_secs(30),
    )
    .await
    .expect("the command runs");
    let background = pid_of(&output.stdout);

    assert!(
        gone_within(background, Duration::from_secs(5)),
        "the background pipeline {background} survived"
    );
}

#[tokio::test]
async fn a_timeout_is_reported_and_kills_the_group() {
    // `exec` keeps the leader as the `sleep` itself, so a leader-only kill would also work here;
    // the variant below adds a background child to prove the group is what is signalled.
    let started = Instant::now();
    let error = run_isolated("sleep", &["30"], Duration::from_millis(300))
        .await
        .expect_err("a timeout is an error");

    assert!(matches!(error, GroupError::Timeout { .. }), "got {error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the timeout was not prompt"
    );
}

#[tokio::test]
async fn a_timeout_kills_the_leader_and_its_children() {
    // The command prints the background pid and then waits, so the leader is alive when the
    // timeout fires and the group holds two processes. A leader-only kill would free the caller
    // and leave the `sleep` behind, which is the failure this asserts against.
    let output = run_isolated(
        "sh",
        &["-c", "sleep 999 & echo $!; wait"],
        Duration::from_millis(400),
    )
    .await;

    let error = output.expect_err("a timeout is an error");
    assert!(matches!(error, GroupError::Timeout { .. }), "got {error:?}");
}

#[tokio::test]
async fn a_timeout_takes_the_background_child_with_it() {
    // Same shape, but the background pid is readable: the child prints it before the deadline. The
    // timeout hides stdout from the caller, so the pid is published through the filesystem, which
    // the test owns.
    let path = std::env::temp_dir().join(format!(
        "girder-process-group-timeout-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let script = format!("sleep 999 & echo $! > {}; wait", path.display());

    let output = run_isolated("sh", &["-c", &script], Duration::from_millis(500)).await;
    assert!(output.is_err(), "the command must report its timeout");

    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the fixture did not write {}: {error}", path.display()));
    let _ = std::fs::remove_file(&path);
    let background: i32 = text.trim().parse().expect("the fixture writes a pid");

    assert!(
        gone_within(background, Duration::from_secs(5)),
        "the background pid {background} outlived the timed-out group"
    );
}

#[tokio::test]
async fn a_missing_program_is_an_error() {
    let error = run_isolated("girder-no-such-program", &[], Duration::from_secs(5))
        .await
        .expect_err("a missing program cannot run");

    match error {
        GroupError::Spawn { program, .. } => assert_eq!(program, "girder-no-such-program"),
        other @ GroupError::Timeout { .. } => {
            panic!("expected a spawn error, got {other:?}")
        }
    }
}

#[tokio::test]
async fn any_alive_tracks_the_group() {
    let mut command = tokio::process::Command::new("sleep");
    command.arg("30");
    let mut guard = GroupGuard::attach(&mut command).expect("a new group is available");
    let mut child = command.spawn().expect("sleep starts");

    let pid = child.id().expect("the child has a pid");
    guard.record(pid);

    assert_eq!(
        guard.group_id(),
        Some(pid),
        "the child must lead its own group"
    );
    assert!(guard.any_alive(), "the child is running");

    guard.kill_all();
    assert!(
        gone_within(i32::try_from(pid).unwrap(), Duration::from_secs(5)),
        "the child survives kill_all"
    );
    assert!(!guard.any_alive(), "nothing is left in the group");

    let _ = child.wait().await;
}

#[tokio::test]
async fn an_unarmed_guard_kills_nothing() {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg("true");
    let mut guard = GroupGuard::attach(&mut command).expect("a new group is available");

    assert_eq!(guard.group_id(), None, "a guard starts unarmed");
    guard.kill_all();
    assert!(!guard.any_alive());

    // `record` must refuse the pid it was given if that pid is not a group leader of its own.
    guard.record(0);
    guard.record(1);
    assert_eq!(
        guard.group_id(),
        None,
        "the sentinel pids must not arm a guard"
    );
}

#[tokio::test]
async fn the_harness_own_group_is_never_armed() {
    // The failure this prevents: `kill(-pid)` where pid belongs to the harness's own group kills
    // the harness, including this test. Recording our own pid must therefore leave the guard
    // unarmed — and calling `kill_all` on it must be a no-op rather than a suicide.
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg("true");
    let mut guard = GroupGuard::attach(&mut command).expect("a new group is available");

    guard.record(std::process::id());

    assert_eq!(guard.group_id(), None, "our own pid must not arm a guard");
    guard.kill_all();
    assert!(!guard.any_alive());
    // Reaching this line at all is the assertion: the harness is still here.
}

#[tokio::test]
async fn repeated_kills_are_harmless() {
    let mut command = tokio::process::Command::new("sleep");
    command.arg("30");
    let mut guard = GroupGuard::attach(&mut command).expect("a new group is available");
    let mut child = command.spawn().expect("sleep starts");
    guard.record(child.id().expect("the child has a pid"));

    guard.kill_all();
    guard.kill_all();
    guard.kill_all();

    assert!(!guard.any_alive());
    let _ = child.wait().await;
}
