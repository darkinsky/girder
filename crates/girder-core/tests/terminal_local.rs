//! Tests for [`ProcessShell`] and [`LocalLauncher`], against real child processes.
//!
//! These are the only tests in the suite that drive a shell for real, and they have to be: the
//! properties under test — output arriving from a pipe, an exit being observed, a close actually
//! killing — are properties of the operating system's process handling, and a fake would prove only
//! that the fake works.
//!
//! # What the platform gives and what it does not
//!
//! There is no pty here, so nothing prints a prompt and nothing echoes input. Every assertion is
//! therefore about output a program *printed*, and the transcripts are matched by substring rather
//! than by exact bytes: chunks arrive split wherever the kernel happened to cut them, and pinning an
//! exact chunk boundary would make the tests fail on a machine with a different pipe buffer.

use std::sync::Arc;
use std::time::Duration;

use girder_core::{
    LocalLauncher, ProcessShell, Shell, TerminalError, TerminalLaunch, TerminalLauncher,
    TerminalSize,
};

/// How long a read waits before reporting that nothing arrived.
///
/// A second, not the tool's 500 ms: these tests run on machines that may be loaded, and a read that
/// timed out too eagerly would fail on scheduling jitter rather than on a defect.
const WAIT: Duration = Duration::from_secs(1);

/// How many reads a test is willing to spend collecting output before giving up.
///
/// A command's output may arrive in several chunks, and how many is a property of the kernel's pipe
/// buffer, not of the implementation. A bound rather than a loop: a shell that never answers must
/// fail the test instead of hanging it.
const MAX_READS: usize = 50;

/// Builds a launch request for `id` in the given directory.
fn launch(id: &str, cwd: &std::path::Path) -> TerminalLaunch {
    TerminalLaunch::new(id)
        .expect("a non-blank id")
        .in_directory(cwd)
        .with_size(TerminalSize::headless())
}

/// Reads until `needle` appears, or panics after [`MAX_READS`].
async fn read_until(shell: &dyn Shell, needle: &str) -> String {
    let mut collected = String::new();
    for _ in 0..MAX_READS {
        if let Some(chunk) = shell.read(WAIT).await.expect("read must not fail") {
            collected.push_str(&chunk);
            if collected.contains(needle) {
                return collected;
            }
        }
    }
    panic!("`{needle}` was not printed; collected: {collected:?}");
}

/// Polls `condition` for up to a second.
///
/// An exit is asynchronous: the process is signalled, then reaped, and only then is `is_alive`
/// false. A test that asserted immediately would be asserting on scheduling, not on behaviour.
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Whether a pid names a process that has not exited.
///
/// `kill(pid, 0)` is the portable probe: it delivers no signal and only asks whether the process
/// exists. A zombie counts as existing — it has not been reaped — but the shell here is reaped by
/// the runtime before this is asked, so a `true` means a live process.
fn process_exists(pid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // `EPERM` means the process exists and belongs to somebody else, which is still "exists" -- so both of those
    // answers are `true` and only a genuine "no such process" is `false`.
    matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
}

// ---- running commands -------------------------------------------------------------------------

#[tokio::test]
async fn a_written_command_prints_its_output() {
    // The property the whole module exists for: input reaches the shell and output comes back.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");
    assert!(shell.is_alive(), "a shell that just started must be alive");

    shell.write("echo hello\n").await.expect("wrote");

    let collected = read_until(&shell, "hello").await;
    assert!(
        collected.contains("hello"),
        "the shell's output must contain what it printed: {collected:?}"
    );
}

#[tokio::test]
async fn a_shell_keeps_its_state_between_commands() {
    // Why a terminal exists rather than a one-shot command: the second command sees the first's
    // working directory. A pipe-backed shell gets this from being one long-lived process.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");

    shell.write("cd /\n").await.expect("wrote cd");
    shell.write("pwd\n").await.expect("wrote pwd");

    let collected = read_until(&shell, "/").await;
    assert!(
        collected.lines().any(|line| line.trim() == "/"),
        "the shell must still be in `/`, which only a surviving process can report: {collected:?}"
    );
    assert!(
        shell.is_alive(),
        "changing directory must not end the shell"
    );
}

#[tokio::test]
async fn a_failing_command_is_readable_and_does_not_end_the_shell() {
    // stderr is merged into the read stream on purpose: a caller that saw only stdout would never
    // learn why a command failed. The error must also not take the shell down with it.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");

    shell
        .write("command_that_does_not_exist_girder\n")
        .await
        .expect("wrote");

    let collected = read_until(&shell, "not found").await;
    assert!(
        collected.contains("not found"),
        "the shell's stderr must be part of what a read returns: {collected:?}"
    );
    assert!(
        shell.is_alive(),
        "a command that failed must not end the shell that ran it"
    );
    assert_eq!(
        shell.exit_code(),
        None,
        "a live shell must not report an exit code"
    );
}

#[tokio::test]
async fn a_read_with_nothing_to_read_returns_none() {
    // The bound is the whole contract: a shell at a prompt has nothing to say, and the call must
    // come back rather than wait for output that may not arrive for hours.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");

    // Drain anything the shell printed on startup first, so the next read is a genuinely idle one.
    while shell.read(WAIT).await.expect("read").is_some() {}

    let started = std::time::Instant::now();
    let chunk = shell.read(Duration::from_millis(150)).await.expect("read");
    let waited = started.elapsed();

    assert_eq!(chunk, None, "an idle shell must report no output");
    assert!(
        waited < Duration::from_secs(2),
        "a read must return near its bound rather than block: waited {waited:?}"
    );
}

// ---- exiting on its own -----------------------------------------------------------------------

#[tokio::test]
async fn an_exit_ends_the_shell_and_reports_its_code() {
    // `exit` is how a person ends a shell, and nothing calls `close`, so the shell has to notice by
    // itself — which is what `is_alive` and `exit_code` are for.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");

    shell.write("exit\n").await.expect("wrote exit");

    assert!(
        eventually(|| !shell.is_alive()).await,
        "a shell that was told to exit must be observed as gone"
    );
    assert_eq!(
        shell.exit_code(),
        Some(0),
        "a clean exit must report its code, not `None`"
    );
}

#[tokio::test]
async fn a_non_zero_exit_reports_the_code_it_chose() {
    // A code other than zero must survive the round trip: reporting every exit as `Some(0)` would
    // hide a failing command behind a clean one.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");

    shell.write("exit 3\n").await.expect("wrote exit");

    assert!(
        eventually(|| !shell.is_alive()).await,
        "the shell must be observed as gone"
    );
    assert_eq!(shell.exit_code(), Some(3));
}

#[tokio::test]
async fn output_printed_before_an_exit_is_still_readable() {
    // Noticing the exit must not discard what the shell printed on its way out: a Read after an exit
    // is how the result of the last command is collected.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");

    shell.write("echo last words\n").await.expect("wrote");
    shell.write("exit\n").await.expect("wrote exit");

    assert!(
        eventually(|| !shell.is_alive()).await,
        "the shell must be observed as gone"
    );

    // What is left is whatever the pipe still holds, which must still include the last command.
    let mut collected = String::new();
    while let Some(chunk) = shell.read(WAIT).await.expect("read") {
        collected.push_str(&chunk);
    }
    assert!(
        collected.contains("last words"),
        "output printed before the exit must still be readable: {collected:?}"
    );
}

// ---- closing ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_close_ends_a_shell_sitting_at_a_prompt() {
    // The common case: nothing is running, so closing succeeds by letting the shell see EOF.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");
    assert!(shell.is_alive());

    shell.close().await.expect("closed");

    assert!(
        eventually(|| !shell.is_alive()).await,
        "a closed shell must not still be running"
    );
}

#[tokio::test]
async fn a_close_kills_the_command_the_shell_is_running() {
    // The case that matters: `sleep` runs in the shell, so nothing on stdin can end it and the
    // close has to signal it. A close that only dropped the pipes would leave the command running
    // with no reader for as long as it chose.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");
    shell.write("sleep 137\n").await.expect("wrote sleep");

    // Let the shell reach the command before closing, so the test closes a busy shell rather than a
    // prompt. A short pause rather than a race: the state under test is "busy", not "busy by now".
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(shell.is_alive(), "the shell must still be running `sleep`");

    shell.close().await.expect("closed");

    assert!(
        eventually(|| !shell.is_alive()).await,
        "closing a busy shell must kill the command, not wait for it"
    );
}

#[tokio::test]
async fn a_close_takes_the_command_with_it_not_just_the_shell() {
    // The property a kill that reaches only the shell would fail: `sleep` is a separate process,
    // and killing `sh` alone would leave it running (reparented to init) until it finished. It is
    // found by the pid the shell itself reported, so the check cannot match an unrelated process.
    let dir = std::env::temp_dir();
    let pid_file = dir.join(format!("girder-terminal-child-{}.pid", std::process::id()));
    let _ignored = std::fs::remove_file(&pid_file);

    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");
    // `$!` is the background job's pid. Backgrounded rather than foreground so the shell prints it
    // immediately instead of waiting for the command; it lands in the same process group either way,
    // which is the group the close has to signal.
    shell
        .write(&format!("sleep 137 & echo $! > {}\n", pid_file.display()))
        .await
        .expect("wrote");

    // Wait for the shell to report the pid, which also proves the command actually started.
    let mut child = None;
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                child = Some(pid);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let child = child.expect("the shell must report the background child's pid");
    assert!(
        process_exists(child),
        "the child `sleep` must be running before the close, or the test proves nothing"
    );

    shell.close().await.expect("closed");

    assert!(
        eventually(|| !process_exists(child)).await,
        "closing the terminal must kill the command too, not only the shell: pid {child} survived"
    );
    let _ignored = std::fs::remove_file(&pid_file);
}

#[tokio::test]
async fn a_closed_shell_refuses_further_input() {
    // Writing to a shell the caller closed must be an error rather than a write into a pipe whose
    // reader is gone, which would look like a command that was accepted and never ran.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");
    shell.close().await.expect("closed");

    let error = shell
        .write("echo ignored\n")
        .await
        .expect_err("a closed shell must refuse input");

    assert!(
        matches!(error, TerminalError::NotRunning { .. }),
        "a closed shell must report why it refused: {error:?}"
    );
}

#[tokio::test]
async fn a_close_is_idempotent() {
    // Closing twice is what a session registry does when a caller closes an already-exited terminal,
    // and it must not fail on a process that is already reaped.
    let dir = std::env::temp_dir();
    let shell = ProcessShell::start(&launch("t1", &dir)).expect("started");

    shell.close().await.expect("first close");
    shell.close().await.expect("second close");
}

// ---- failing to start --------------------------------------------------------------------------

#[tokio::test]
async fn starting_a_shell_that_does_not_exist_fails_with_a_reason() {
    // A missing shell must fail at launch, naming the program, rather than hand back a session that
    // answers nothing forever.
    let dir = std::env::temp_dir();
    let spec = launch("t1", &dir).with_shell("girder-no-such-shell-anywhere");

    let error = ProcessShell::start(&spec).expect_err("must not start");

    assert!(
        error.contains("girder-no-such-shell-anywhere"),
        "the reason must name the program that could not start: {error}"
    );
}

#[tokio::test]
async fn starting_in_a_directory_that_does_not_exist_fails() {
    // A working directory that is gone is a launch failure, not a shell that starts in `/` and
    // silently operates somewhere the caller did not ask for.
    let dir = std::env::temp_dir().join("girder-no-such-directory-anywhere");
    let spec = launch("t1", &dir);

    let error = ProcessShell::start(&spec).expect_err("must not start");

    assert!(!error.is_empty(), "a failed launch must say why: {error:?}");
}

// ---- the launcher --------------------------------------------------------------------------------

#[tokio::test]
async fn a_launch_naming_no_shell_gets_the_launchers_default() {
    // The launcher's shell is the fallback the deployment chose, so a launch that names nothing
    // still gets a terminal instead of a refusal.
    let dir = std::env::temp_dir();
    let launcher = LocalLauncher::with_shell("sh");

    let shell = launcher.launch(launch("t1", &dir)).await.expect("launched");

    assert!(shell.is_alive(), "the launched shell must be running");
    shell
        .write("echo from-the-launcher\n")
        .await
        .expect("wrote");
    let collected = read_until(shell.as_ref(), "from-the-launcher").await;
    assert!(collected.contains("from-the-launcher"));
    shell.close().await.expect("closed");
}

#[tokio::test]
async fn a_launch_naming_a_shell_overrides_the_launchers_default() {
    // A caller that asked for a specific program must get it: a launcher that always substituted its
    // own would make the `shell` field of a launch a lie.
    let dir = std::env::temp_dir();
    let launcher = LocalLauncher::with_shell("girder-no-such-shell-anywhere");
    let spec = launch("t1", &dir).with_shell("sh");

    let shell = launcher.launch(spec).await.expect("launched");

    assert!(
        shell.is_alive(),
        "the requested shell must be the one started"
    );
    shell.close().await.expect("closed");
}

#[tokio::test]
async fn a_launch_of_a_missing_shell_fails_through_the_launcher() {
    // The launcher reports the reason instead of panicking: it is called from inside a turn, where a
    // panic would take down the session rather than fail one tool call.
    let dir = std::env::temp_dir();
    let launcher = LocalLauncher::with_shell("sh");
    let spec = launch("t1", &dir).with_shell("girder-no-such-shell-anywhere");

    // Matched rather than `expect_err`, which would require `Arc<dyn Shell>` to be `Debug`.
    let error = match launcher.launch(spec).await {
        Ok(_shell) => panic!("a missing shell must not launch"),
        Err(error) => error,
    };

    assert!(
        error.contains("girder-no-such-shell-anywhere"),
        "the failure must name the program: {error}"
    );
}

#[tokio::test]
async fn the_default_launcher_uses_a_shell_that_exists() {
    // `new()` picks `$SHELL` or `sh`; the assertion is that the choice actually starts, because a
    // `$SHELL` pointing at something missing must not make every terminal fail.
    let dir = std::env::temp_dir();
    let launcher = LocalLauncher::new();

    let shell = launcher
        .launch(launch("t1", &dir))
        .await
        .expect("the default shell must start");
    assert!(shell.is_alive());
    shell.close().await.expect("closed");
}

#[tokio::test]
async fn a_launcher_can_be_shared_across_sessions() {
    // A deployment mounts one launcher for every session, so it is held as an `Arc<dyn …>`; this
    // pins that two launches from the same value are independent processes.
    let dir = std::env::temp_dir();
    let launcher: Arc<dyn TerminalLauncher> = Arc::new(LocalLauncher::with_shell("sh"));

    let first = launcher.launch(launch("t1", &dir)).await.expect("first");
    let second = launcher.launch(launch("t2", &dir)).await.expect("second");

    first.write("echo first\n").await.expect("wrote");
    second.write("echo second\n").await.expect("wrote");

    let first_output = read_until(first.as_ref(), "first").await;
    let second_output = read_until(second.as_ref(), "second").await;
    assert!(first_output.contains("first"));
    assert!(second_output.contains("second"));
    assert!(
        !first_output.contains("second"),
        "two sessions must not share one process: {first_output:?}"
    );

    first.close().await.expect("closed");
    second.close().await.expect("closed");
}
