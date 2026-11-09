//! Acceptance tests for migration-plan phase 5 (§2.8).
//!
//! The plan names four criteria:
//!
//! 1. A `bash` command with pipes runs.
//! 2. A timeout kills the whole process group.
//! 3. The sandbox refuses an out-of-scope write.
//! 4. No zombie processes remain.
//!
//! Criterion 2 is the one that fails silently if implemented naively: killing the direct child
//! leaves its grandchildren running. So the test asserts on the *grandchild*, not on the shell.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use girder_core::{
    ExecError, ExecOptions, PathPolicy, ToolCallId, has_program, read_file, run_bash, run_shell,
};
use tokio_util::sync::CancellationToken;

/// A scratch directory under the target dir, unique per test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-exec-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

// ---- 1. a command with pipes runs ---------------------------------------------------------------

#[tokio::test]
async fn a_command_with_a_pipe_runs() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(20));

    let output = run_bash("echo hello | tr a-z A-Z", &options, &cancel)
        .await
        .expect("pipes must work");

    assert!(output.success(), "exit: {:?}", output.code);
    assert_eq!(output.stdout.trim(), "HELLO");
}

#[tokio::test]
async fn a_multi_stage_pipeline_runs() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(20));

    let output = run_bash("printf 'c\\na\\nb\\n' | sort | head -2", &options, &cancel)
        .await
        .unwrap();

    assert_eq!(output.stdout, "a\nb\n");
}

#[tokio::test]
async fn stderr_is_captured_separately_from_stdout() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(20));

    let output = run_bash("echo out; echo err >&2", &options, &cancel)
        .await
        .unwrap();

    assert_eq!(output.stdout.trim(), "out");
    assert_eq!(output.stderr.trim(), "err");
    assert!(output.success());
}

#[tokio::test]
async fn a_nonzero_exit_is_reported_not_raised() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(20));

    let output = run_bash("exit 3", &options, &cancel).await.unwrap();

    assert_eq!(output.code, Some(3));
    assert!(!output.success());
    assert!(output.to_text().contains("[exit 3]"));
}

#[tokio::test]
async fn the_working_directory_is_respected() {
    let dir = scratch("cwd");
    std::fs::write(dir.join("marker.txt"), "here").unwrap();
    let cancel = CancellationToken::new();
    let options = ExecOptions::default()
        .with_cwd(&dir)
        .with_timeout(Duration::from_secs(20));

    let output = run_bash("cat marker.txt", &options, &cancel).await.unwrap();

    assert_eq!(
        output.stdout.trim(),
        "here",
        "the command ran in {}",
        dir.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn large_output_does_not_deadlock() {
    // A pipe whose reader is not pumped fills and blocks the child. This is the failure mode that
    // makes a naive spawn-and-wait hang forever on a big result.
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(30));

    let output = run_bash("seq 1 20000", &options, &cancel).await.unwrap();

    assert!(output.success());
    let lines: Vec<&str> = output.stdout.lines().collect();
    assert_eq!(lines.len(), 20000, "all output must be drained");
    assert_eq!(lines.last(), Some(&"20000"));
}

// ---- 2. a timeout kills the whole process group -------------------------------------------------

/// Whether a pid is still alive.
fn alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        // Signal 0 performs the permission and existence checks without delivering anything.
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[tokio::test]
async fn a_timeout_kills_the_whole_process_group() {
    // The shell spawns a grandchild. Killing only the shell leaves the grandchild alive, which is
    // the leak this criterion exists to catch. The grandchild writes its pid to a file so the test
    // can check it after the timeout.
    let dir = scratch("timeout");
    let pidfile = dir.join("grandchild.pid");
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_millis(600));

    let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
    let started = Instant::now();
    let error = run_bash(&script, &options, &cancel)
        .await
        .expect_err("must time out");
    let elapsed = started.elapsed();

    assert!(
        matches!(error, ExecError::Timeout { .. }),
        "expected Timeout, got {error:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the timeout must not wait for the full sleep: {elapsed:?}"
    );

    let grandchild: i32 = std::fs::read_to_string(&pidfile)
        .expect("the grandchild must have written its pid")
        .trim()
        .parse()
        .expect("pid is a number");

    // Give the signal a moment to be delivered and reaped.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !alive(grandchild),
        "the grandchild (pid {grandchild}) survived the process-group kill"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_command_that_finishes_before_the_timeout_is_not_killed() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(30));

    let output = run_bash("echo done", &options, &cancel).await.unwrap();

    assert!(output.success());
    assert_eq!(output.stdout.trim(), "done");
}

// ---- 3. the sandbox refuses an out-of-scope write ------------------------------------------------

#[tokio::test]
async fn a_declared_write_outside_the_scope_is_refused_before_spawning() {
    let allowed = scratch("scope-allowed");
    let outside = scratch("scope-outside");
    let policy = PathPolicy::new(&allowed);
    let cancel = CancellationToken::new();
    let options = ExecOptions::default()
        .with_policy(policy)
        .with_timeout(Duration::from_secs(10))
        .writing(outside.join("escape.txt"));

    let error = run_bash("touch anything", &options, &cancel)
        .await
        .expect_err("an out-of-scope write must be refused");

    let ExecError::OutOfScope { path } = &error else {
        panic!("expected OutOfScope, got {error:?}");
    };
    assert!(path.ends_with("escape.txt"));
    assert!(
        error.to_string().contains("outside the writable scope"),
        "{error}"
    );

    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn a_declared_write_inside_the_scope_runs() {
    let allowed = scratch("scope-inside");
    let target = allowed.join("ok.txt");
    let policy = PathPolicy::new(&allowed);
    let cancel = CancellationToken::new();
    let options = ExecOptions::default()
        .with_policy(policy)
        .with_timeout(Duration::from_secs(10))
        .writing(&target);

    let output = run_bash(
        &format!("echo written > {}", target.display()),
        &options,
        &cancel,
    )
    .await
    .expect("an in-scope write must run");

    assert!(output.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap().trim(), "written");

    let _ = std::fs::remove_dir_all(&allowed);
}

#[tokio::test]
async fn traversal_out_of_the_scope_is_refused() {
    // `a/../b` must not escape by string comparison. The policy resolves both sides.
    let allowed = scratch("traversal");
    let policy = PathPolicy::new(&allowed);

    let sneaky = allowed.join("..").join("escape.txt");
    assert!(
        !policy.permits_write(&sneaky),
        ".. must not escape the scope"
    );
    assert!(policy.check_write(&sneaky).is_err());

    let _ = std::fs::remove_dir_all(&allowed);
}

#[tokio::test]
async fn a_nonexistent_path_under_the_scope_is_permitted() {
    // The check runs *before* the write, so the target cannot exist yet. A policy that only handled
    // existing paths would refuse every create.
    let allowed = scratch("nonexistent");
    let policy = PathPolicy::new(&allowed);
    let future = allowed.join("deeper").join("yet").join("new.txt");

    assert!(
        policy.permits_write(&future),
        "a create target must be judged"
    );

    let _ = std::fs::remove_dir_all(&allowed);
}

#[tokio::test]
async fn reading_outside_the_readable_scope_is_refused() {
    let allowed = scratch("read-scope");
    let policy = PathPolicy::new(&allowed);

    let error = read_file(std::path::Path::new("/etc/passwd"), &policy)
        .await
        .expect_err("reading outside the scope must fail");
    assert!(matches!(error, ExecError::OutOfScope { .. }));

    let _ = std::fs::remove_dir_all(&allowed);
}

#[tokio::test]
async fn reading_inside_the_scope_succeeds() {
    let allowed = scratch("read-inside");
    let path = allowed.join("data.txt");
    std::fs::write(&path, "content").unwrap();
    let policy = PathPolicy::new(&allowed);

    let content = read_file(&path, &policy).await.unwrap();

    assert_eq!(content, "content");
    let _ = std::fs::remove_dir_all(&allowed);
}

// ---- 4. no zombies ------------------------------------------------------------------------------

#[tokio::test]
async fn a_completed_command_leaves_no_zombie() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(20));

    // Run several so a leak would accumulate.
    for _ in 0..5 {
        let output = run_bash("true", &options, &cancel).await.unwrap();
        assert!(output.success());
    }

    // If a child were left unreaped it would appear as a zombie of this process, but this test
    // process has no such child by construction: `tokio::process::Child::wait` reaps. The assertion
    // that matters is that the loop above completes without hanging, plus the one below.
    assert_eq!(zombie_children(), 0, "no zombie children may remain");
}

#[tokio::test]
async fn a_killed_command_leaves_no_zombie() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_millis(200));

    for _ in 0..3 {
        let _ = run_bash("sleep 30", &options, &cancel).await;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(zombie_children(), 0, "a killed child must still be reaped");
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
            // Format: `pid (comm) state ppid ...`. `comm` may contain spaces, so split after the
            // final ')'.
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

// ---- cancellation -----------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_kills_the_running_command() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(60));
    let token = cancel.clone();

    let running = tokio::spawn(async move { run_bash("sleep 30", &options, &token).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    cancel.cancel();

    let result = running.await.unwrap();
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(ExecError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "cancellation must not wait for the command: {elapsed:?}"
    );
}

// ---- shell selection ---------------------------------------------------------------------------

#[tokio::test]
async fn run_shell_uses_sh_and_works() {
    let cancel = CancellationToken::new();
    let options = ExecOptions::default().with_timeout(Duration::from_secs(20));

    let output = run_shell("echo from-sh | cat", &options, &cancel)
        .await
        .unwrap();

    assert_eq!(output.stdout.trim(), "from-sh");
}

#[test]
fn has_program_finds_a_known_binary() {
    assert!(has_program("sh"), "sh must exist on a POSIX host");
    assert!(!has_program("definitely-not-a-real-program-xyz"));
}

// ---- the bash tool ------------------------------------------------------------------------------

#[tokio::test]
async fn the_bash_tool_runs_a_command_and_reports_success() {
    let dir = scratch("tool-ok");
    let tool = girder_core::bash_tool(PathPolicy::new(&dir));
    let context = girder_core::ToolRunContext {
        execution: girder_core::ToolExecution {
            call_id: ToolCallId::new("c1").expect("valid test id"),
            name: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "echo tool-ran" }).to_string(),
            agent_id: girder_core::AgentId::new("a".to_owned()).expect("valid test id"),
        },
        cancel: CancellationToken::new(),
    };

    let result = (tool.execute)(serde_json::json!({ "command": "echo tool-ran" }), context)
        .await
        .unwrap();

    assert!(!result.is_error);
    assert!(result.to_text().contains("tool-ran"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_bash_tool_reports_a_nonzero_exit_as_an_error() {
    let dir = scratch("tool-fail");
    let tool = girder_core::bash_tool(PathPolicy::new(&dir));
    let context = girder_core::ToolRunContext {
        execution: girder_core::ToolExecution {
            call_id: ToolCallId::new("c1").expect("valid test id"),
            name: "bash".to_owned(),
            arguments: "{}".to_owned(),
            agent_id: girder_core::AgentId::new("a".to_owned()).expect("valid test id"),
        },
        cancel: CancellationToken::new(),
    };

    let result = (tool.execute)(serde_json::json!({ "command": "exit 7" }), context)
        .await
        .unwrap();

    assert!(
        result.is_error,
        "a nonzero exit is a failure the model must see"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- a tool result helper used above -------------------------------------------------------------

/// Renders a tool result's text, for assertions.
trait ToText {
    fn to_text(&self) -> String;
}

impl ToText for girder_core::ToolResult {
    fn to_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                girder_core::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}
