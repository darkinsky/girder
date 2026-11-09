//! Tests for the subprocess seam (M2 of the v2 plan).
//!
//! Three failure modes are what this seam exists to prevent, and each has a test here: orphaned
//! grandchildren, pipe deadlock, and zombies. Those are the ones that fail silently in production.

use std::time::{Duration, Instant};

use girder_core::{ProcessError, SpawnOptions, run_process, spawn_process};
use tokio_util::sync::CancellationToken;

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

fn sh(script: &str) -> SpawnOptions {
    SpawnOptions::new("sh", vec!["-c".to_owned(), script.to_owned()])
}

// ---- basic execution --------------------------------------------------------------------------

#[tokio::test]
async fn a_command_runs_and_reports_its_output() {
    let outcome = run_process(sh("echo hello"), cancel()).await.expect("ran");

    assert!(outcome.success());
    assert_eq!(outcome.stdout.trim(), "hello");
}

#[tokio::test]
async fn stderr_is_captured_separately() {
    let outcome = run_process(sh("echo out; echo err >&2"), cancel())
        .await
        .unwrap();

    assert_eq!(outcome.stdout.trim(), "out");
    assert_eq!(outcome.stderr.trim(), "err");
}

#[tokio::test]
async fn a_nonzero_exit_is_reported_not_raised() {
    // The process ran; its exit code is data, not a failure of the seam.
    let outcome = run_process(sh("exit 3"), cancel()).await.unwrap();

    assert_eq!(outcome.code, Some(3));
    assert!(!outcome.success());
    assert!(outcome.to_text().contains("[exit 3]"));
}

#[tokio::test]
async fn stderr_survives_into_the_rendered_text() {
    // A failing command usually says why on stderr; dropping it leaves an exit code and no reason.
    let outcome = run_process(sh("echo 'disk full' >&2; exit 1"), cancel())
        .await
        .unwrap();

    assert!(outcome.to_text().contains("disk full"));
}

#[tokio::test]
async fn the_working_directory_is_respected() {
    let dir = std::env::temp_dir().join(format!("girder-sp-cwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("marker"), "here").unwrap();

    let outcome = run_process(sh("cat marker").with_cwd(&dir), cancel())
        .await
        .unwrap();

    assert_eq!(outcome.stdout.trim(), "here");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_environment_variable_can_be_added() {
    let options = sh("echo $GIRDER_TEST_VAR").with_env("GIRDER_TEST_VAR", "injected");
    let outcome = run_process(options, cancel()).await.unwrap();

    assert_eq!(outcome.stdout.trim(), "injected");
}

#[tokio::test]
async fn a_missing_program_is_a_spawn_error() {
    let options = SpawnOptions::new("girder-no-such-program-xyz", vec![]);
    let error = run_process(options, cancel())
        .await
        .expect_err("cannot spawn");

    let ProcessError::Spawn { program, .. } = error else {
        panic!("expected Spawn, got {error:?}");
    };
    assert_eq!(program, "girder-no-such-program-xyz");
}

// ---- failure mode 2: pipe deadlock ------------------------------------------------------------

#[tokio::test]
async fn a_large_output_does_not_deadlock() {
    // A pipe not drained concurrently fills at 64 KiB and blocks the writer, which then never exits.
    // 20,000 lines is far above any buffer.
    let outcome = run_process(
        sh("seq 1 20000").with_timeout(Duration::from_secs(30)),
        cancel(),
    )
    .await
    .expect("must not deadlock");

    assert!(outcome.success());
    assert_eq!(outcome.stdout.lines().count(), 20000);
}

#[tokio::test]
async fn output_larger_than_the_pipe_buffer_on_stderr_also_works() {
    // The same hazard on the other stream; a reader that only pumps stdout would hang here.
    let outcome = run_process(
        sh("seq 1 10000 >&2").with_timeout(Duration::from_secs(30)),
        cancel(),
    )
    .await
    .expect("must not deadlock");

    assert_eq!(outcome.stderr.lines().count(), 10000);
}

// ---- failure mode 1: orphaned grandchildren ---------------------------------------------------

#[tokio::test]
async fn a_timeout_kills_the_whole_process_group() {
    // The grandchild must die too. This is the property that a naive `child.kill()` misses, and it
    // fails silently: the shell exits and the orphan keeps running.
    let dir = std::env::temp_dir().join(format!("girder-sp-group-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("grandchild.pid");

    let started = Instant::now();
    let error = run_process(
        sh(&format!("sleep 30 & echo $! > {}; wait", pidfile.display()))
            .with_timeout(Duration::from_millis(600)),
        cancel(),
    )
    .await
    .expect_err("must time out");

    assert!(matches!(error, ProcessError::Timeout { .. }), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "must not wait for the sleep"
    );

    let grandchild: i32 = std::fs::read_to_string(&pidfile)
        .expect("the grandchild wrote its pid")
        .trim()
        .parse()
        .expect("pid");
    tokio::time::sleep(Duration::from_millis(300)).await;

    #[cfg(unix)]
    {
        let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_ok();
        assert!(!alive, "the grandchild survived the group kill");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cancelling_kills_the_running_command() {
    let token = CancellationToken::new();
    let cloner = token.clone();
    let started = Instant::now();

    let running = tokio::spawn(async move {
        run_process(sh("sleep 30").with_timeout(Duration::from_secs(60)), cloner).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    token.cancel();

    let result = running.await.unwrap();
    assert!(matches!(result, Err(ProcessError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn a_command_that_finishes_before_the_timeout_is_not_killed() {
    let outcome = run_process(
        sh("echo done").with_timeout(Duration::from_secs(30)),
        cancel(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.stdout.trim(), "done");
}

// ---- failure mode 3: zombies ------------------------------------------------------------------

#[tokio::test]
async fn a_completed_command_leaves_no_zombie() {
    for _ in 0..5 {
        run_process(sh("true"), cancel()).await.unwrap();
    }
    assert_eq!(zombie_children(), 0);
}

#[tokio::test]
async fn a_killed_command_leaves_no_zombie() {
    for _ in 0..3 {
        let _ = run_process(
            sh("sleep 30").with_timeout(Duration::from_millis(200)),
            cancel(),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(zombie_children(), 0);
}

/// Counts this process's zombie children by reading `/proc`.
fn zombie_children() -> usize {
    #[cfg(target_os = "linux")]
    {
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return 0;
        };
        let mut count = 0;
        for entry in entries.flatten() {
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // `pid (comm) state ppid`; `comm` may contain spaces, so split after the final ')'.
            let Some(after) = stat.rfind(')').map(|index| &stat[index + 2..]) else {
                continue;
            };
            let mut fields = after.split_whitespace();
            let Some(state) = fields.next() else { continue };
            let Some(ppid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
                continue;
            };
            if ppid == me && state == "Z" {
                count += 1;
            }
        }
        count
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

// ---- the spawn alias --------------------------------------------------------------------------

#[tokio::test]
async fn the_boxed_spawn_form_works_too() {
    // The seam is async-first; `spawn` returns a boxed future for callers that store it.
    let outcome = spawn_process(sh("echo boxed"), cancel()).await.unwrap();
    assert_eq!(outcome.stdout.trim(), "boxed");
}

#[test]
fn has_program_finds_sh() {
    assert!(girder_core::has_program_in_path("sh"));
    assert!(!girder_core::has_program_in_path(
        "girder-no-such-program-xyz"
    ));
}
