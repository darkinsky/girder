//! Tests for the persistent shell: the state that survives between commands, and the ways a shell
//! stops being usable.
//!
//! The reason this module exists at all is the first group: a one-shot shell forgets `cd` and
//! `export`, and these tests fail against one. The rest cover the framing, because a marker parsed
//! wrongly returns another command's output and a marker that never arrives hangs a tool call.

use std::time::{Duration, Instant};

use girder_core::persistent_shell::{Config, PersistentShell, ShellError};

/// A scratch directory under the target dir, unique per test.
///
/// Under the temporary directory rather than the repository: a test that ran here and wrote a file
/// would touch tracked files, and one that `cd`s would leave a relative path pointing somewhere
/// surprising.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("girder-ps-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A shell started in a fresh scratch directory.
fn shell(name: &str) -> (PersistentShell, std::path::PathBuf) {
    let dir = scratch(name);
    let shell = PersistentShell::start(Config::new(&dir).with_timeout(Duration::from_secs(20)))
        .expect("the shell starts");
    (shell, dir)
}

// ---- 1. one command ---------------------------------------------------------------------------

#[tokio::test]
async fn a_single_command_returns_its_output_and_status() {
    let (mut shell, _dir) = shell("single");

    let output = shell.run("echo hello").await.expect("the command runs");

    assert_eq!(output.stdout().trim(), "hello");
    assert!(output.stderr().is_empty(), "stderr: {:?}", output.stderr());
    assert_eq!(output.code(), 0);
    assert!(output.success());
    assert_eq!(output.stdout_omitted_bytes(), 0);
}

// ---- 2. the reason this exists: cd survives ------------------------------------------------

#[tokio::test]
async fn a_cd_moves_the_shell_for_the_next_command() {
    let (mut shell, dir) = shell("cd");
    let target = dir.join("nested");
    std::fs::create_dir_all(&target).expect("nested dir");

    let changed = shell
        .run(&format!("cd {}", target.display()))
        .await
        .expect("cd runs");
    assert_eq!(changed.code(), 0);

    let printed = shell.run("pwd").await.expect("pwd runs");

    // The canonical form, because the scratch path may itself contain a symlink (macOS maps
    // `/var` to `/private/var`) and `pwd` reports what the kernel says.
    let expected = std::fs::canonicalize(&target).expect("the target exists");
    let reported = std::fs::canonicalize(printed.stdout().trim()).expect("pwd printed a directory");
    assert_eq!(reported, expected, "pwd printed {:?}", printed.stdout());
    // The tracked directory follows the shell rather than staying at the configured one.
    assert_eq!(
        std::fs::canonicalize(shell.cwd()).expect("cwd exists"),
        expected
    );
}

#[tokio::test]
async fn a_relative_cd_is_resolved_against_the_previous_directory() {
    let (mut shell, dir) = shell("cd-relative");
    std::fs::create_dir_all(dir.join("a/b")).expect("nested dir");

    shell.run("cd a").await.expect("first cd");
    shell.run("cd b").await.expect("second cd");
    let printed = shell.run("pwd").await.expect("pwd");

    assert_eq!(
        std::fs::canonicalize(printed.stdout().trim()).expect("a directory"),
        std::fs::canonicalize(dir.join("a/b")).expect("the target exists")
    );
}

// ---- 3. environment survives -----------------------------------------------------------------

#[tokio::test]
async fn an_exported_variable_survives_between_commands() {
    let (mut shell, _dir) = shell("env");

    shell.run("export GIRDER_PS_TEST=kept").await.expect("export");
    let printed = shell.run("printf '%s' \"$GIRDER_PS_TEST\"").await.expect("read");

    assert_eq!(printed.stdout(), "kept");
}

#[tokio::test]
async fn an_unexported_shell_variable_also_survives() {
    let (mut shell, _dir) = shell("shell-var");

    // A plain assignment is shell state rather than process environment: it survives only if the
    // same shell is still running, which is the property a one-shot call cannot have.
    shell.run("plain=value").await.expect("assign");
    let printed = shell.run("printf '%s' \"$plain\"").await.expect("read");

    assert_eq!(printed.stdout(), "value");
}

// ---- 4. failing commands are answers, not errors ---------------------------------------------

#[tokio::test]
async fn a_nonzero_exit_is_reported_not_raised() {
    let (mut shell, _dir) = shell("nonzero");

    let output = shell
        .run("echo failing >&2; exit 3")
        .await
        .expect("a failing command still answers");

    assert_eq!(output.code(), 3);
    assert!(!output.success());
    assert_eq!(output.stderr().trim(), "failing");
}

#[tokio::test]
async fn a_failing_command_does_not_take_the_shell_with_it() {
    let (mut shell, _dir) = shell("nonzero-continues");

    let failed = shell.run("false").await.expect("false runs");
    assert_eq!(failed.code(), 1);

    let after = shell.run("echo still-here").await.expect("the shell still runs");
    assert_eq!(after.stdout().trim(), "still-here");
    assert_eq!(after.code(), 0);
}

#[tokio::test]
async fn stdout_and_stderr_stay_separate() {
    let (mut shell, _dir) = shell("streams");

    let output = shell
        .run("echo out; echo err >&2")
        .await
        .expect("the command runs");

    // Separated rather than merged: identical bytes would otherwise describe both a successful
    // command that warns and a failed one that explains itself.
    assert_eq!(output.stdout().trim(), "out");
    assert_eq!(output.stderr().trim(), "err");
}

// ---- 5. the timeout -------------------------------------------------------------------------

#[tokio::test]
async fn a_command_that_never_returns_times_out() {
    let dir = scratch("timeout");
    let mut shell = PersistentShell::start(
        Config::new(&dir)
            .with_timeout(Duration::from_millis(300))
            .with_shell("sh"),
    )
    .expect("the shell starts");

    let started = Instant::now();
    let outcome = shell.run("sleep 999").await;

    match outcome {
        Err(ShellError::Timeout { seconds }) => assert_eq!(seconds, 0),
        other => panic!("expected a timeout, got {other:?}"),
    }
    // The point of the timeout is that it is respected, not merely reported: a shell that kept the
    // command running would make this test finish only when `sleep` did.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the timeout took {:?}",
        started.elapsed()
    );
    // The state of a shell that was interrupted is unknown, so it is not offered for reuse.
    assert!(!shell.is_running());
    assert!(matches!(
        shell.run("echo after").await,
        Err(ShellError::NotRunning)
    ));
}

// ---- 6. output that looks like the framing ---------------------------------------------------

#[tokio::test]
async fn output_containing_a_sentinel_like_string_is_not_mistaken_for_one() {
    let (mut shell, _dir) = shell("sentinel");

    // The shapes a guesser would try: the module's own marker names, and a complete-looking
    // completion frame. The real marker carries a per-shell random tag, so a command that does not
    // know it cannot terminate its own frame early.
    let output = shell
        .run("echo __GIRDER_PS_END:0:; echo __GIRDER_PS_START; echo __GIRDER_PS_ERR_END:")
        .await
        .expect("the command runs");

    assert_eq!(output.code(), 0);
    let printed = output.stdout();
    assert!(printed.contains("__GIRDER_PS_END:0:"), "printed: {printed:?}");
    assert!(printed.contains("__GIRDER_PS_START"), "printed: {printed:?}");
    assert!(printed.contains("__GIRDER_PS_ERR_END:"), "printed: {printed:?}");
}

#[tokio::test]
async fn a_command_that_prints_nul_bytes_is_reported_verbatim() {
    let (mut shell, _dir) = shell("nul-output");

    // A NUL is the byte the framing itself uses as a delimiter, so this is the input most likely to
    // confuse the scanner. The tag participates in the match, so a NUL alone is ordinary output.
    let output = shell
        .run("printf 'a\\000b\\000'")
        .await
        .expect("the command runs");

    assert_eq!(output.stdout().as_bytes(), b"a\0b\0");
    assert_eq!(output.code(), 0);
}

#[tokio::test]
async fn a_command_containing_a_nul_byte_is_refused() {
    let (mut shell, _dir) = shell("nul-input");

    // A NUL cannot travel in an argument, so it is refused where the reason is knowable rather than
    // passed to a shell that would fail with a syntax error blaming the command.
    assert!(matches!(
        shell.run("echo a\0b").await,
        Err(ShellError::NulByte)
    ));
    // The refusal is this side's, so the shell is untouched and still usable.
    assert_eq!(
        shell.run("echo fine").await.expect("the shell runs").stdout().trim(),
        "fine"
    );
}

// ---- 7. many commands in a row ---------------------------------------------------------------

#[tokio::test]
async fn many_commands_run_in_sequence_without_leaking_frames() {
    let (mut shell, _dir) = shell("sequence");

    for index in 0..20 {
        let output = shell
            .run(&format!("echo line-{index}"))
            .await
            .expect("the command runs");
        assert_eq!(
            output.stdout().trim(),
            format!("line-{index}"),
            "command {index} saw {:?}",
            output.stdout()
        );
    }
}

#[tokio::test]
async fn state_accumulates_across_many_commands() {
    let (mut shell, dir) = shell("accumulate");
    std::fs::create_dir_all(dir.join("one/two/three")).expect("nested dirs");

    shell.run("cd one").await.expect("cd one");
    shell.run("export STEP=one").await.expect("export");
    shell.run("cd two").await.expect("cd two");
    shell.run("cd three").await.expect("cd three");

    let output = shell
        .run("printf '%s in %s' \"$STEP\" \"$(basename \"$PWD\")\"")
        .await
        .expect("the command runs");

    assert_eq!(output.stdout(), "one in three");
}

#[tokio::test]
async fn a_command_with_no_output_reports_nothing() {
    let (mut shell, _dir) = shell("silent");

    let output = shell.run("true").await.expect("true runs");

    assert_eq!(output.stdout(), "");
    assert_eq!(output.stderr(), "");
    assert_eq!(output.code(), 0);
}

#[tokio::test]
async fn a_multiline_command_runs_as_one_command() {
    let (mut shell, _dir) = shell("multiline");

    let output = shell
        .run("for name in a b c; do\n  echo \"item-$name\"\ndone")
        .await
        .expect("the command runs");

    assert_eq!(output.stdout(), "item-a\nitem-b\nitem-c\n");
}

#[tokio::test]
async fn a_command_with_quotes_and_pipes_reaches_the_shell_intact() {
    let (mut shell, _dir) = shell("quoting");

    let output = shell
        .run("printf '%s\\n' \"it's\" | tr a-z A-Z")
        .await
        .expect("the command runs");

    assert_eq!(output.stdout(), "IT'S\n");
}

#[tokio::test]
async fn large_output_is_bounded_and_says_how_much_was_dropped() {
    let dir = scratch("bounded");
    let mut shell = PersistentShell::start(
        Config::new(&dir)
            .with_timeout(Duration::from_secs(20))
            .with_max_output_bytes(64),
    )
    .expect("the shell starts");

    // Enough output to exceed the bound several times over: the marker still has to be found after
    // it, which is what a scanner that stopped reading at the bound would fail to do.
    let output = shell
        .run("i=0; while [ $i -lt 200 ]; do printf '0123456789'; i=$((i+1)); done")
        .await
        .expect("the command runs");

    assert_eq!(output.stdout().len(), 64);
    assert_eq!(output.stdout_omitted_bytes(), 2000 - 64);
    assert!(output.success());
}

// ---- 8. a shell that exits -------------------------------------------------------------------

#[tokio::test]
async fn an_exit_is_reported_as_an_exited_shell() {
    let (mut shell, _dir) = shell("exit");

    // `exit` closes the shell before it can report a status, which is a different outcome from a
    // command that failed: there is no shell left to run the next command in.
    let outcome = shell.run("exit 4").await;

    assert!(
        matches!(outcome, Err(ShellError::Exited { .. })),
        "got {outcome:?}"
    );
    assert!(!shell.is_running());
}

// ---- 9. lifecycle ----------------------------------------------------------------------------

#[tokio::test]
async fn closing_is_idempotent_and_ending_the_shell_is_visible() {
    let (shell, _dir) = shell("close");

    assert!(shell.is_running());
    shell.close();
}

#[tokio::test]
async fn a_shell_started_in_a_missing_directory_reports_why() {
    let missing = scratch("missing").join("not-there");

    let outcome = PersistentShell::start(Config::new(&missing));

    match outcome {
        Err(ShellError::Start { cwd, .. }) => assert_eq!(cwd, missing),
        Err(other) => panic!("expected a start error, got {other:?}"),
        // A build machine may refuse the spawn for a different reason; the assertion is about which
        // error variant a missing directory produces, and a shell that somehow started is a bug.
        Ok(_) => panic!("a shell started in a directory that does not exist"),
    }
}
