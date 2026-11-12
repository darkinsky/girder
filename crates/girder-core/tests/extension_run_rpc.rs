//! End-to-end tests for `extension.activate`, `extension.deactivate`, and `extension.status`.
//!
//! These drive the real lifecycle: a real temporary script, started by [`ShellRuntime`] as an actual
//! process, stopped by `deactivate`, and observed through the same `RemoteCall` a socket would produce.
//! A test double for the runtime would prove the handlers call *something*; only a process proves that
//! "running" means a process exists and "stopped" means it does not.
//!
//! The properties under test, in the order they matter:
//!
//! - **define → approve → activate → status running → deactivate → status stopped** — the round trip, with
//!   the process's own pid as evidence at both ends
//! - **an unapproved definition cannot be activated** — the registry's `NotApproved` reaches the caller
//!   unsoftened; the RPC layer is not a second approval path
//! - **a definition with an unmet `requires` cannot be activated** — with no services configured, an
//!   activation is refused and says which service was missing
//! - **an unknown id is an error on every method that takes one** — activate, deactivate, and status, so a
//!   miss-shaped success cannot be produced by any of them
//! - **a read-only peer may read a status and may not start anything** — the declarations are correct, not
//!   merely present
//!
//! Isolation: every test builds its own registry, its own runtime, and its own temporary directory named
//! after the test and this process, so a test binary running in parallel cannot observe another's pid file.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile against
//! the methods as written even before the crate root wires them in. It spells its own paths through
//! `girder_core::`, the alias the library declares for itself, so the same source builds here and inside the
//! crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/extension_run_rpc.rs"]
mod extension_run_rpc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use girder_core::{
    ExtensionDefinition, ExtensionId, ExtensionRegistry, ExtensionRuntime,
    ExtensionShellRuntime as ShellRuntime, ReadOnly, RemoteCall, RemoteError, RemoteRegistry,
};
use serde_json::{Value, json};

use extension_run_rpc::{
    ACTIVATE_METHOD, DEACTIVATE_METHOD, NAMESPACE, STATUS_METHOD, register_extension_run_methods,
    register_extension_run_methods_with_services,
};

// ---- fixtures ---------------------------------------------------------------------------------

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A Remote registry with the three methods registered, against the default (empty) service list.
fn registry(extensions: &Arc<ExtensionRegistry>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    let runtime = runtime();
    register_extension_run_methods(&mut registry, extensions, &runtime);
    registry
}

/// The shell runtime every test drives, as the trait object the registration takes.
fn runtime() -> Arc<dyn ExtensionRuntime> {
    Arc::new(ShellRuntime::new())
}

/// A directory owned by one test, named after it and this process.
///
/// Cleaned up first: a previous run killed by a timeout leaves the directory behind, and a marker file in
/// it would make the next run observe a process that never started.
fn sandbox(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("girder-ext-run-rpc-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create sandbox");
    dir
}

/// A definition that runs `source`.
fn definition(id: &str, source: impl Into<String>) -> ExtensionDefinition {
    ExtensionDefinition {
        id: ExtensionId::new(id).expect("valid id"),
        name: format!("Extension {id}"),
        description: format!("the {id} extension"),
        source: source.into(),
        requires: Vec::new(),
        approved_by: None,
    }
}

/// A source that writes its pid and stays alive until signalled.
///
/// `exec sleep` so the process the runtime signals is the process that sleeps: a shell waiting on a child
/// would be killed while the child survived, and the stop assertion would not prove anything.
fn long_running(dir: &Path) -> String {
    format!("echo $$ > {}; exec sleep 30", pid_file(dir).display())
}

fn pid_file(dir: &Path) -> PathBuf {
    dir.join("pid")
}

/// Waits for the pid file to hold a usable pid.
async fn wait_for_pid(dir: &Path) -> u32 {
    for _ in 0..300 {
        if let Ok(text) = std::fs::read_to_string(pid_file(dir)) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the extension never reported a pid in {}",
        pid_file(dir).display()
    );
}

/// Whether a process is gone.
///
/// `kill(pid, 0)` asks the kernel without sending anything: `ESRCH` means no such process.
fn is_gone(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err()
}

/// Polls until `check` holds, so a test does not depend on how fast a signal is delivered.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..300 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Dispatches one call and unwraps the value, so the assertions stay about the wire shape.
async fn dispatch(
    registry: &RemoteRegistry,
    method: &str,
    args: Value,
) -> Result<Value, RemoteError> {
    registry.dispatch(NAMESPACE, method, call(args)).await
}

// ---- the round trip ---------------------------------------------------------------------------

#[tokio::test]
async fn an_approved_extension_can_be_started_observed_and_stopped() {
    let dir = sandbox("round-trip");
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions
        .define(definition("e1", long_running(&dir)))
        .expect("defined");
    extensions.approve("e1", "the operator").expect("approved");
    let registry = registry(&extensions);

    let started = dispatch(&registry, ACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect("activated");
    assert_eq!(started["id"], "e1");
    assert_eq!(started["status"], "running");

    // The status the method reports is the registry's, and the process the runtime started is alive.
    let status = dispatch(&registry, STATUS_METHOD, json!({"id": "e1"}))
        .await
        .expect("status");
    assert_eq!(status["status"], "running");
    assert_eq!(status["approvedBy"], "the operator");
    let pid = wait_for_pid(&dir).await;
    assert!(!is_gone(pid), "the extension should be running");

    let stopped = dispatch(&registry, DEACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect("deactivated");
    assert_eq!(stopped["id"], "e1");
    assert_eq!(stopped["status"], "stopped");

    let status = dispatch(&registry, STATUS_METHOD, json!({"id": "e1"}))
        .await
        .expect("status");
    assert_eq!(status["status"], "stopped");
    // `stopped` is not a failure, so nothing should have explained it.
    assert!(status.get("reason").is_none(), "{status}");
    eventually("the process to be gone", || is_gone(pid)).await;
}

#[tokio::test]
async fn deactivating_an_extension_that_is_not_running_succeeds() {
    // The caller asked for a state, and the state holds. Reporting a failure for an already-stopped
    // extension would make a retried request look like a broken one.
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions
        .define(definition("idle", "sleep 30"))
        .expect("defined");
    extensions.approve("idle", "human").expect("approved");

    let value = dispatch(
        &registry(&extensions),
        DEACTIVATE_METHOD,
        json!({"id": "idle"}),
    )
    .await
    .expect("deactivated");
    assert_eq!(value["status"], "stopped");
}

// ---- the two gates ----------------------------------------------------------------------------

#[tokio::test]
async fn activating_an_unapproved_extension_is_refused() {
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions
        .define(definition("e1", "sleep 30"))
        .expect("defined");

    let error = dispatch(&registry(&extensions), ACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect_err("unapproved");
    let message = error.to_string();
    assert!(message.contains("has not been approved"), "{message}");

    // Nothing started, and the status never became running.
    let status = dispatch(&registry(&extensions), STATUS_METHOD, json!({"id": "e1"}))
        .await
        .expect("status");
    assert_eq!(status["status"], "stopped");
    assert!(status["approvedBy"].is_null());
}

#[tokio::test]
async fn activating_an_extension_with_an_unmet_requirement_is_refused() {
    // The default registration supplies no services, so any `requires` is missing. The refusal must name
    // the service instead of starting an extension that would fail at its first call.
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions
        .define(definition("e1", "sleep 30").requiring("database"))
        .expect("defined");
    extensions.approve("e1", "human").expect("approved");

    let error = dispatch(&registry(&extensions), ACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect_err("missing services");
    let message = error.to_string();
    assert!(message.contains("database"), "{message}");

    // The registry records the failure with its reason, so a client can explain it later.
    let status = dispatch(&registry(&extensions), STATUS_METHOD, json!({"id": "e1"}))
        .await
        .expect("status");
    assert_eq!(status["status"], "failed");
    let reason = status["reason"].as_str().expect("a reason");
    assert!(reason.contains("database"), "{reason}");
}

#[tokio::test]
async fn a_declared_service_that_is_available_lets_the_extension_run() {
    // The other half of the previous test: with the service supplied at registration, the same definition
    // activates. Without this, "refused" could be proved by a check that never passes.
    let dir = sandbox("services");
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions
        .define(definition("e1", long_running(&dir)).requiring("database"))
        .expect("defined");
    extensions.approve("e1", "human").expect("approved");

    let mut registry = RemoteRegistry::new();
    register_extension_run_methods_with_services(
        &mut registry,
        &extensions,
        &runtime(),
        &["database".to_owned()],
    );

    let value = dispatch(&registry, ACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect("activated");
    assert_eq!(value["status"], "running");
    let pid = wait_for_pid(&dir).await;

    let stopped = dispatch(&registry, DEACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect("deactivated");
    assert_eq!(stopped["status"], "stopped");
    eventually("the process to be gone", || is_gone(pid)).await;
}

// ---- unknown ids ------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_id_is_an_error_on_every_method_that_takes_one() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let registry = registry(&extensions);

    for method in [ACTIVATE_METHOD, DEACTIVATE_METHOD, STATUS_METHOD] {
        let error = dispatch(&registry, method, json!({"id": "nope"}))
            .await
            .expect_err(method);
        assert!(
            matches!(error, RemoteError::InvalidArguments { .. }),
            "{method}"
        );
        assert!(error.to_string().contains("nope"), "{method}: {error}");
    }
}

#[tokio::test]
async fn a_missing_id_is_a_protocol_error() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let registry = registry(&extensions);

    for method in [ACTIVATE_METHOD, DEACTIVATE_METHOD, STATUS_METHOD] {
        let error = dispatch(&registry, method, json!({}))
            .await
            .expect_err(method);
        assert!(
            matches!(error, RemoteError::InvalidArguments { .. }),
            "{method}"
        );
    }

    // A blank id is not an id: it could never name a definition, so it is refused as an argument error
    // rather than reported as something that used to exist.
    let error = dispatch(&registry, ACTIVATE_METHOD, json!({"id": "   "}))
        .await
        .expect_err("blank id");
    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
}

// ---- access policy ----------------------------------------------------------------------------

#[tokio::test]
async fn a_read_only_peer_may_read_a_status_and_may_not_activate() {
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions
        .define(definition("e1", "sleep 30"))
        .expect("defined");
    extensions.approve("e1", "human").expect("approved");

    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    let runtime = runtime();
    register_extension_run_methods(&mut registry, &extensions, &runtime);

    // `status` is a read, so the policy lets it through.
    let value = dispatch(&registry, STATUS_METHOD, json!({"id": "e1"}))
        .await
        .expect("status is readable");
    assert_eq!(value["status"], "stopped");

    // Starting code with the harness's authority is a write, so the policy refuses it — and the refusal
    // carries the policy's reason, not a generic failure.
    for method in [ACTIVATE_METHOD, DEACTIVATE_METHOD] {
        let error = dispatch(&registry, method, json!({"id": "e1"}))
            .await
            .expect_err(method);
        assert!(matches!(error, RemoteError::Refused { .. }), "{method}");
        assert_eq!(error.code(), "REFUSED");
    }

    // The refusals changed nothing.
    assert_eq!(
        extensions
            .status("e1")
            .map(|status| status.as_str().to_owned()),
        Some("stopped".to_owned())
    );
}

// ---- a start that fails -----------------------------------------------------------------------

#[tokio::test]
async fn a_source_that_cannot_run_is_reported_as_a_failed_start() {
    // An approval is not a promise that the code works. The runtime's own failure must reach the caller
    // instead of leaving a "running" extension that was dead before the call returned.
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions
        .define(definition("broken", "exit 3"))
        .expect("defined");
    extensions.approve("broken", "human").expect("approved");

    let error = dispatch(
        &registry(&extensions),
        ACTIVATE_METHOD,
        json!({"id": "broken"}),
    )
    .await
    .expect_err("failed start");
    assert!(error.to_string().contains("exited with code 3"), "{error}");

    let status = dispatch(
        &registry(&extensions),
        STATUS_METHOD,
        json!({"id": "broken"}),
    )
    .await
    .expect("status");
    assert_eq!(status["status"], "failed");
    assert!(status["reason"].as_str().is_some(), "{status}");
}

#[tokio::test]
async fn activating_twice_in_a_row_is_refused() {
    // "Start it" for something already started is ambiguous: the caller may have wanted a restart. The
    // registry refuses rather than silently doing either, and the refusal must survive the RPC layer.
    let dir = sandbox("twice");
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions
        .define(definition("e1", long_running(&dir)))
        .expect("defined");
    extensions.approve("e1", "human").expect("approved");
    let registry = registry(&extensions);

    dispatch(&registry, ACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect("activated");
    let pid = wait_for_pid(&dir).await;

    let error = dispatch(&registry, ACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect_err("already running");
    assert!(error.to_string().contains("already running"), "{error}");

    // Still the one process from the first call, and still stoppable.
    assert!(!is_gone(pid), "the first start should be untouched");
    dispatch(&registry, DEACTIVATE_METHOD, json!({"id": "e1"}))
        .await
        .expect("deactivated");
    eventually("the process to be gone", || is_gone(pid)).await;
}

// ---- registration -----------------------------------------------------------------------------

#[tokio::test]
async fn registration_adds_three_methods_and_declares_them() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let registry = registry(&extensions);

    let names = registry.names();
    for method in [ACTIVATE_METHOD, DEACTIVATE_METHOD, STATUS_METHOD] {
        assert!(
            names
                .iter()
                .any(|(namespace, name)| namespace == NAMESPACE && name == method),
            "{method} was not registered: {names:?}"
        );
    }

    // The declarations take effect through the policy, which the read-only test above checks. Here the
    // point is narrower: adding these methods did not disturb a registry that already had the list and
    // approve methods on it.
    let mut shared = RemoteRegistry::new();
    girder_core::register_extension_methods(&mut shared, &extensions);
    let before = shared.len();
    let runtime = runtime();
    register_extension_run_methods(&mut shared, &extensions, &runtime);
    assert_eq!(shared.len(), before + 3);
}
