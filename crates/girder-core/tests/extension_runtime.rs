//! Tests for the shell runtime that puts an approved extension into a process.
//!
//! Each test uses its own temporary directory, process id, and counter, because the runtime starts
//! real processes: two tests sharing a pid file would observe each other's start or stop and pass
//! for the wrong reason.

use std::path::{Path, PathBuf};
use std::time::Duration;

use girder_core::{
    BoxFut, ExtensionDefinition as Definition, ExtensionError, ExtensionId, ExtensionRegistry,
    ExtensionRun as Run, ExtensionRuntime as Runtime, ExtensionShellRuntime as ShellRuntime,
    ExtensionStatus,
};

use girder_core::{ExtensionNoopRuntime as NoopRuntime, assert_runnable};

/// A directory owned by one test, named after it and this process.
///
/// Cleaned up first: a previous run killed by a timeout leaves the directory behind, and a marker
/// file in it would make the next run pass before anything happened.
fn sandbox(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("girder-ext-runtime-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create sandbox");
    dir
}

fn definition(id: &str, source: impl Into<String>) -> Definition {
    Definition {
        id: ExtensionId::new(id).expect("valid id"),
        name: format!("Extension {id}"),
        description: format!("the {id} extension"),
        source: source.into(),
        requires: Vec::new(),
        approved_by: None,
    }
}

/// A source that starts, writes its pid, and stays alive until signalled.
///
/// `exec sleep` so the process the runtime signalled is the process that sleeps: a shell waiting on
/// a child would be killed while the child survived, and the pause test would not prove anything.
fn long_running(dir: &Path, seconds: u64) -> String {
    format!(
        "echo $$ > {pid}; exec sleep {seconds}",
        pid = dir.join("pid").display()
    )
}

fn pid_file(dir: &Path) -> PathBuf {
    dir.join("pid")
}

/// Waits for a file to hold a usable pid, or reports that it never did.
async fn wait_for_pid(dir: &Path) -> u32 {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(pid_file(dir)) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the extension never reported a pid in {}", dir.display());
}

/// Whether a process is gone.
///
/// `kill(pid, 0)` asks the kernel without sending anything: `ESRCH` means no such process. A zombie
/// still answers, which is not what these tests care about — they care that it is not running.
fn is_gone(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err()
}

/// Polls until `check` holds, so a test does not depend on how fast a signal is delivered.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---- a successful start -----------------------------------------------------------------------

#[tokio::test]
async fn activating_a_runnable_extension_moves_it_to_running() {
    let dir = sandbox("start");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", long_running(&dir, 30)))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let status = registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect("started");

    assert_eq!(status, ExtensionStatus::Running);
    assert_eq!(
        registry.status("e1"),
        Some(ExtensionStatus::Running),
        "the registry must record what the runtime reported"
    );
    assert_eq!(registry.running(), vec!["e1"]);

    let pid = wait_for_pid(&dir).await;
    assert!(!is_gone(pid), "the extension is supposed to be alive");

    registry.deactivate("e1").expect("stopped");
    eventually("the process to exit", || is_gone(pid)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_source_that_finishes_at_once_counts_as_started() {
    // A definition such as `seed-the-database` has nothing left to do once it exits. Refusing it
    // would make a legitimate extension impossible to activate.
    let dir = sandbox("oneshot");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition(
            "e1",
            format!("echo done > {}", dir.join("done").display()),
        ))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let status = registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect("started");

    assert_eq!(status, ExtensionStatus::Running);
    assert!(
        dir.join("done").exists(),
        "the source must actually have run"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- stopping ---------------------------------------------------------------------------------

#[tokio::test]
async fn deactivating_stops_the_process_and_marks_it_stopped() {
    let dir = sandbox("stop");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", long_running(&dir, 30)))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");
    let runtime = ShellRuntime::new().with_start_grace(Duration::from_millis(100));

    registry
        .activate("e1", &[], &runtime)
        .await
        .expect("started");
    let pid = wait_for_pid(&dir).await;

    let status = registry.deactivate("e1").expect("stopped");

    assert_eq!(status, ExtensionStatus::Stopped);
    assert_eq!(registry.status("e1"), Some(ExtensionStatus::Stopped));
    assert!(registry.running().is_empty());
    eventually("the process to exit", || is_gone(pid)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stopping_twice_is_harmless() {
    // `Run::stop` is not `FnOnce`, and nothing in the registry promises to call it once.
    let dir = sandbox("stop-twice");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", long_running(&dir, 30)))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect("started");
    let pid = wait_for_pid(&dir).await;

    registry.deactivate("e1").expect("stopped");
    registry.deactivate("e1").expect("stopped again");

    assert_eq!(registry.status("e1"), Some(ExtensionStatus::Stopped));
    eventually("the process to exit", || is_gone(pid)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn removing_a_running_extension_stops_its_process() {
    let dir = sandbox("remove");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", long_running(&dir, 30)))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect("started");
    let pid = wait_for_pid(&dir).await;

    registry.remove("e1").expect("removed");

    eventually("the process to exit", || is_gone(pid)).await;
    assert!(registry.ids().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- a failed start ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_program_fails_activation_and_marks_it_failed() {
    // `sh -c` spawns successfully and only then exits 127, so this passes only because the runtime
    // checks whether the command survived its grace period.
    let registry = ExtensionRegistry::new();
    registry
        .define(definition(
            "e1",
            "girder-definitely-not-a-program-0123456789 --go",
        ))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let error = registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect_err("the program does not exist");

    let ExtensionError::LoadFailed { id, reason } = &error else {
        panic!("expected LoadFailed, got {error:?}");
    };
    assert_eq!(id, "e1");
    assert!(
        reason.contains("127"),
        "the exit code must be reported: {reason}"
    );

    let status = registry.status("e1").expect("status");
    assert!(
        matches!(status, ExtensionStatus::Failed { .. }),
        "expected Failed, got {status:?}"
    );
    assert_eq!(status.as_str(), "failed");
    assert!(registry.running().is_empty());
}

#[tokio::test]
async fn a_nonzero_exit_fails_activation() {
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", "echo 'no config file' >&2; exit 3"))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let error = registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect_err("exited nonzero");

    let ExtensionError::LoadFailed { reason, .. } = &error else {
        panic!("expected LoadFailed, got {error:?}");
    };
    // stderr is what explains a failure; an exit code alone leaves a reader with no reason.
    assert!(
        reason.contains("no config file"),
        "the failure must carry stderr: {reason}"
    );
    assert!(matches!(
        registry.status("e1"),
        Some(ExtensionStatus::Failed { .. })
    ));
}

#[tokio::test]
async fn a_failed_extension_can_be_activated_again_once_fixed() {
    // A definition is durable while a run is not, so a transient failure must not be permanent.
    let dir = sandbox("retry");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", "exit 1"))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect_err("failed");
    assert!(matches!(
        registry.status("e1"),
        Some(ExtensionStatus::Failed { .. })
    ));

    // The registry has no way to replace a definition, so this exercises the status transition:
    // a definition that fails and then succeeds must reach `Running` from `Failed`.
    registry.remove("e1").expect("removed");
    registry
        .define(definition("e1", long_running(&dir, 30)))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let status = registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect("started");

    assert_eq!(status, ExtensionStatus::Running);
    registry.deactivate("e1").expect("stopped");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- runtime configuration --------------------------------------------------------------------

#[tokio::test]
async fn the_runtime_runs_a_source_in_its_configured_directory() {
    // A definition is text and carries no working directory, so the deployment that activates it
    // decides where it runs.
    let dir = sandbox("cwd");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", "cat marker"))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");
    std::fs::write(dir.join("marker"), "from the workspace").expect("write marker");

    // The source would fail here rather than in the directory, which is the point: a wrong `cwd`
    // must be an activation error, not an extension that silently reads the wrong file.
    let status = registry
        .activate("e1", &[], &ShellRuntime::new().with_cwd(dir.clone()))
        .await
        .expect("started");

    assert_eq!(status, ExtensionStatus::Running);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_runtime_can_use_another_shell() {
    // A definition may need bashisms; `sh` is the default because it exists everywhere, not because
    // it is the only choice.
    let dir = sandbox("shell");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition(
            "e1",
            format!("echo $$ > {}", pid_file(&dir).display()),
        ))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let status = registry
        .activate("e1", &[], &ShellRuntime::new().with_shell("bash"))
        .await
        .expect("started");

    assert_eq!(status, ExtensionStatus::Running);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the approval gate ------------------------------------------------------------------------

#[tokio::test]
async fn an_unapproved_extension_is_refused_before_anything_runs() {
    let dir = sandbox("unapproved");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition(
            "e1",
            format!("touch {}", dir.join("ran").display()),
        ))
        .expect("defined");

    let error = registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect_err("not approved");

    assert!(matches!(error, ExtensionError::NotApproved { .. }));
    assert!(
        !dir.join("ran").exists(),
        "the source must not have run: approval is what makes running it legitimate"
    );
    assert_eq!(registry.status("e1"), Some(ExtensionStatus::Stopped));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the service gate -------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_service_is_refused_before_anything_runs() {
    let dir = sandbox("missing-service");
    let registry = ExtensionRegistry::new();
    registry
        .define(
            definition("e1", format!("touch {}", dir.join("ran").display()))
                .requiring("tools")
                .requiring("events"),
        )
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let available = vec!["events".to_owned()];
    let error = registry
        .activate("e1", &available, &ShellRuntime::new())
        .await
        .expect_err("missing service");

    let ExtensionError::MissingServices { missing, .. } = error else {
        panic!("expected MissingServices, got {error:?}");
    };
    assert_eq!(missing, vec!["tools"]);
    assert!(!dir.join("ran").exists(), "nothing was started");
    assert!(matches!(
        registry.status("e1"),
        Some(ExtensionStatus::Failed { .. })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_satisfied_service_lets_the_extension_start() {
    let dir = sandbox("service-ok");
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", long_running(&dir, 30)).requiring("tools"))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let available = vec!["tools".to_owned()];
    let status = registry
        .activate("e1", &available, &ShellRuntime::new())
        .await
        .expect("started");

    assert_eq!(status, ExtensionStatus::Running);
    registry.deactivate("e1").expect("stopped");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the pre-flight check ----------------------------------------------------------------------

#[test]
fn a_command_is_runnable() {
    // The case a client may activate: the source is something a shell can execute.
    assert_runnable(&definition("e1", "echo hello")).expect("a command is runnable");
    // Comments are fine alongside a statement; only a comment-only source is a description.
    assert_runnable(&definition("e1", "# why this exists\necho hello"))
        .expect("a comment above a command is runnable");
}

#[test]
fn a_description_is_not_runnable() {
    // `sh` accepts a comment-only source and exits 0, so without this check an extension that does
    // nothing would be indistinguishable from one that worked. `// source` is a path to the shell,
    // not a comment, so it is caught by `start` rather than here — but it is prose all the same,
    // and a client that asked before prompting a human would want to know.
    let error = assert_runnable(&definition("e1", "# only a comment\n\n   "))
        .expect_err("a comment-only source");
    let ExtensionError::LoadFailed { id, reason } = &error else {
        panic!("expected LoadFailed, got {error:?}");
    };
    assert_eq!(id, "e1");
    assert!(
        reason.contains("description"),
        "the reason must name the problem: {reason}"
    );

    let error = assert_runnable(&definition("e2", "#   \n#")).expect_err("another description");
    let ExtensionError::LoadFailed { reason, .. } = &error else {
        panic!("expected LoadFailed, got {error:?}");
    };
    assert!(reason.contains("description"), "{reason}");
}

#[test]
fn the_check_rejects_a_definition_the_registry_would_otherwise_prompt_for() {
    // Why this is a separate function: a client can refuse before a human is asked, so an approval
    // prompt is never spent on a definition that could not run.
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", "# a description of what it would do"))
        .expect("defined");
    let stored = registry.definition("e1").expect("present");

    assert!(assert_runnable(&stored).is_err());
    assert!(
        registry
            .status("e1")
            .is_some_and(|status| !status.is_running()),
        "checking must not change the lifecycle"
    );
}

#[tokio::test]
async fn a_description_activated_anyway_is_a_reported_failure() {
    // Not every client runs the pre-flight check. `// source` is a path to the shell, so the process
    // fails and the registry says so — which is the safe outcome, unlike a `#` comment that exits 0.
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", "// source"))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let error = registry
        .activate("e1", &[], &ShellRuntime::new())
        .await
        .expect_err("a path that does not exist");

    assert!(matches!(error, ExtensionError::LoadFailed { .. }));
    assert!(matches!(
        registry.status("e1"),
        Some(ExtensionStatus::Failed { .. })
    ));
}

// ---- the inert runtime ------------------------------------------------------------------------

#[tokio::test]
async fn the_noop_runtime_records_activation_without_running_anything() {
    // Its documented limit: `Running` means the activation was recorded, not that anything works.
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", "// a description, not a command"))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    let status = registry
        .activate("e1", &[], &NoopRuntime::new())
        .await
        .expect("started");

    assert_eq!(status, ExtensionStatus::Running);
    assert_eq!(
        registry.deactivate("e1").expect("stopped"),
        ExtensionStatus::Stopped
    );
}

// ---- the trait is usable through a boxed trait object ------------------------------------------

#[tokio::test]
async fn a_runtime_can_be_held_as_a_trait_object() {
    // The registry takes `&dyn Runtime`, so a deployment stores one behind an `Arc`; the shell
    // runtime must be usable that way or wiring it into a server would not compile.
    let dir = sandbox("trait-object");
    let runtime: std::sync::Arc<dyn Runtime> = std::sync::Arc::new(ShellRuntime::new());
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1", long_running(&dir, 30)))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");

    registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect("started");

    assert_eq!(registry.status("e1"), Some(ExtensionStatus::Running));
    registry.deactivate("e1").expect("stopped");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A runtime that always fails, to prove the registry's error path is exercised through the trait
/// rather than only through the built-in runtimes.
struct Broken;

impl Runtime for Broken {
    fn start(
        &self,
        definition: &Definition,
        _available: &[String],
    ) -> BoxFut<Result<Run, ExtensionError>> {
        let id = definition.id.to_string();
        Box::pin(async move {
            Err(ExtensionError::LoadFailed {
                id,
                reason: "synthetic".to_owned(),
            })
        })
    }
}

#[tokio::test]
async fn a_runtime_error_marks_the_extension_failed() {
    let registry = ExtensionRegistry::new();
    registry.define(definition("e1", "// x")).expect("defined");
    registry.approve("e1", "human").expect("approved");

    let error = registry
        .activate("e1", &[], &Broken)
        .await
        .expect_err("failed");

    assert!(matches!(error, ExtensionError::LoadFailed { .. }));
    assert!(matches!(
        registry.status("e1"),
        Some(ExtensionStatus::Failed { .. })
    ));
}
