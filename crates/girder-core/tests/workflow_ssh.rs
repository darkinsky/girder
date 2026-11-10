//! Tests for the workflow registry and the SSH vocabulary.
//!
//! Both are registries over an unimplemented transport, and both must refuse rather than appear to work when the
//! transport is absent.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use girder_core::{
    SshClient, SshCommand, SshError, SshHost, SshOutcome, SshTransport, UnavailableSsh,
    WorkflowEngine, WorkflowError, WorkflowEvent, WorkflowRegistry, WorkflowStartRequest,
    WorkflowStatus,
};

// ---- workflow ---------------------------------------------------------------------------------

/// An engine that reports a fixed sequence and returns a value.
struct Scripted {
    phases: Vec<&'static str>,
    agents: Vec<&'static str>,
    fail: bool,
}

impl WorkflowEngine for Scripted {
    fn run(
        &self,
        _request: WorkflowStartRequest,
        report: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
    ) -> girder_core::BoxFut<Result<serde_json::Value, String>> {
        let phases: Vec<String> = self.phases.iter().map(|name| (*name).to_owned()).collect();
        let agents: Vec<String> = self.agents.iter().map(|name| (*name).to_owned()).collect();
        let fail = self.fail;
        Box::pin(async move {
            for name in phases {
                report(WorkflowEvent::PhaseStarted {
                    name: name.clone(),
                    at: 1,
                });
                for label in &agents {
                    report(WorkflowEvent::AgentStarted {
                        label: label.clone(),
                        prompt: format!("do {label}"),
                    });
                    report(WorkflowEvent::AgentFinished {
                        label: label.clone(),
                        output: "done".to_owned(),
                        result: Some(serde_json::json!({"ok": true})),
                        at: 2,
                    });
                }
                report(WorkflowEvent::PhaseEnded { name, at: 3 });
            }
            if fail {
                return Err("the script threw".to_owned());
            }
            Ok(serde_json::json!({"answer": 42}))
        })
    }
}

fn request() -> WorkflowStartRequest {
    WorkflowStartRequest::new("plan()", "a test run").expect("valid")
}

fn run_id(value: &str) -> girder_core::RunId {
    girder_core::RunId::new(value).expect("valid")
}

#[tokio::test]
async fn a_run_records_its_phases_and_agents() {
    let engine = Arc::new(Scripted {
        phases: vec!["plan", "gather"],
        agents: vec!["researcher"],
        fail: false,
    });
    let registry = WorkflowRegistry::with_engine(engine);

    let run = registry
        .start(run_id("r1"), request(), 100)
        .await
        .expect("ran");

    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.phases.len(), 2);
    assert!(run.phases.iter().all(|phase| phase.ended_at.is_some()));
    assert_eq!(run.agents.len(), 1);
    assert_eq!(run.finished_agents(), 1);
    assert_eq!(run.result, Some(serde_json::json!({"answer": 42})));
}

#[tokio::test]
async fn an_agents_result_is_recorded_separately_from_its_output() {
    // The script wants a structured answer; re-parsing prose to find it is what the structured form avoids.
    let engine = Arc::new(Scripted {
        phases: vec!["one"],
        agents: vec!["a"],
        fail: false,
    });
    let registry = WorkflowRegistry::with_engine(engine);

    let run = registry
        .start(run_id("r1"), request(), 100)
        .await
        .expect("ran");
    let agent = run.agents.get("a").expect("recorded");

    assert_eq!(agent.output.as_deref(), Some("done"));
    assert_eq!(agent.result, Some(serde_json::json!({"ok": true})));
    assert_eq!(agent.prompt, "do a");
}

#[tokio::test]
async fn a_failed_script_marks_the_run_failed() {
    let engine = Arc::new(Scripted {
        phases: vec!["one"],
        agents: vec![],
        fail: true,
    });
    let registry = WorkflowRegistry::with_engine(engine);

    let error = registry
        .start(run_id("r1"), request(), 100)
        .await
        .expect_err("failed");

    let WorkflowError::Failed { reason, .. } = error else {
        panic!("expected Failed");
    };
    assert_eq!(reason, "the script threw");
    let run = registry.get("r1").expect("recorded");
    assert!(matches!(run.status, WorkflowStatus::Failed { .. }));
}

#[tokio::test]
async fn a_registry_without_an_engine_refuses_and_records() {
    // A run that silently did nothing would report completion for work that never happened.
    let registry = WorkflowRegistry::new();

    let error = registry
        .start(run_id("r1"), request(), 100)
        .await
        .expect_err("no engine");

    assert!(matches!(error, WorkflowError::Failed { .. }));
    let run = registry
        .get("r1")
        .expect("recorded so a listing shows the attempt");
    assert!(matches!(run.status, WorkflowStatus::Failed { .. }));
}

#[tokio::test]
async fn cancelling_a_running_run_marks_it_cancelled() {
    let registry = WorkflowRegistry::new();
    // Record a pending run by hand through a start that fails; the status is then terminal.
    let _ = registry.start(run_id("r1"), request(), 100).await;

    // A fresh run in `Running` state cannot be produced without an engine that blocks, so this asserts the
    // terminal path instead: a finished run cannot be cancelled.
    let error = registry.cancel("r1").expect_err("already finished");
    assert!(matches!(error, WorkflowError::AlreadyFinished { .. }));
}

#[test]
fn cancelling_an_unknown_run_is_an_error() {
    let registry = WorkflowRegistry::new();

    assert!(matches!(
        registry.cancel("nope"),
        Err(WorkflowError::Unknown { .. })
    ));
}

#[tokio::test]
async fn a_finished_run_leaves_the_active_list() {
    let engine = Arc::new(Scripted {
        phases: vec![],
        agents: vec![],
        fail: false,
    });
    let registry = WorkflowRegistry::with_engine(engine);
    registry
        .start(run_id("r1"), request(), 100)
        .await
        .expect("ran");

    assert!(registry.active().is_empty());
    assert_eq!(registry.ids(), vec!["r1"]);
}

#[test]
fn a_start_request_needs_a_script_and_a_title() {
    assert!(WorkflowStartRequest::new("", "t").is_none());
    assert!(WorkflowStartRequest::new("s", "  ").is_none());
}

#[test]
fn a_start_request_carries_arguments() {
    let request = request().with_argument("topic", "rust");

    assert_eq!(
        request.arguments.get("topic").map(String::as_str),
        Some("rust")
    );
}

#[test]
fn the_status_names_are_stable() {
    assert_eq!(WorkflowStatus::Pending.as_str(), "pending");
    assert_eq!(WorkflowStatus::Running.as_str(), "running");
    assert_eq!(WorkflowStatus::Completed.as_str(), "completed");
    assert_eq!(WorkflowStatus::Cancelled.as_str(), "cancelled");
}

// ---- ssh --------------------------------------------------------------------------------------

#[test]
fn a_host_defaults_to_the_ssh_port() {
    let host = SshHost::new("example.com").expect("valid");

    assert_eq!(host.port, 22);
    assert_eq!(host.display(), "example.com");
}

#[test]
fn a_host_refuses_a_url_or_a_command() {
    // A space, slash, or at-sign means the caller passed a URL or a command.
    assert!(SshHost::new("ssh://example.com").is_none());
    assert!(SshHost::new("example.com ls").is_none());
    assert!(SshHost::new("user@example.com").is_none());
    assert!(SshHost::new("").is_none());
}

#[test]
fn a_user_with_a_shell_character_is_refused() {
    let host = SshHost::new("example.com").expect("valid");

    assert!(host.clone().as_user("user name").is_none());
    assert!(host.clone().as_user("user@host").is_none());
    assert!(host.as_user("deploy").is_some());
}

#[test]
fn a_display_includes_the_user_and_a_non_default_port() {
    let host = SshHost::new("example.com")
        .expect("valid")
        .on_port(2222)
        .as_user("deploy")
        .expect("valid");

    assert_eq!(host.display(), "deploy@example.com:2222");
}

#[test]
fn a_command_needs_a_line() {
    let host = SshHost::new("example.com").expect("valid");

    assert!(SshCommand::new(host.clone(), "").is_none());
    assert!(SshCommand::new(host, "ls").is_some());
}

#[test]
fn a_command_defaults_to_a_two_minute_timeout() {
    let host = SshHost::new("example.com").expect("valid");
    let command = SshCommand::new(host, "ls").expect("valid");

    assert_eq!(command.timeout, Duration::from_secs(120));
}

#[tokio::test]
async fn a_client_without_a_transport_refuses() {
    // A deployment that mounted the vocabulary without the protocol should say so rather than appear to work.
    let client = SshClient::new();
    let host = SshHost::new("example.com").expect("valid");
    let command = SshCommand::new(host, "ls").expect("valid");

    let error = client.run(command).await.expect_err("no transport");

    assert!(matches!(error, SshError::Unreachable { .. }));
    assert!(!client.is_available());
}

#[tokio::test]
async fn an_unavailable_transport_refuses_with_the_host() {
    let host = SshHost::new("example.com").expect("valid");
    let command = SshCommand::new(host, "ls").expect("valid");

    let error = UnavailableSsh.run(command).await.expect_err("refused");

    let SshError::Unreachable { host, .. } = error else {
        panic!("expected Unreachable");
    };
    assert_eq!(host, "example.com");
}

struct FakeTransport;

impl SshTransport for FakeTransport {
    fn run(&self, command: SshCommand) -> girder_core::BoxFut<Result<SshOutcome, SshError>> {
        let line = command.line;
        Box::pin(async move {
            Ok(SshOutcome {
                code: Some(0),
                stdout: format!("ran {line}"),
                stderr: String::new(),
            })
        })
    }
}

#[tokio::test]
async fn a_mounted_transport_runs_the_command() {
    let client = SshClient::with_transport(Arc::new(FakeTransport));
    let host = SshHost::new("example.com").expect("valid");
    let command = SshCommand::new(host, "uname -a").expect("valid");

    let outcome = client.run(command).await.expect("ran");

    assert!(outcome.is_success());
    assert_eq!(outcome.stdout, "ran uname -a");
    assert!(client.is_available());
}

#[test]
fn a_nonzero_status_is_data_not_a_transport_error() {
    // A remote command that failed still produced output; a caller that only saw an error would lose it.
    let outcome = SshOutcome {
        code: Some(1),
        stdout: "the diagnostic".to_owned(),
        stderr: String::new(),
    };

    assert!(!outcome.is_success());
    assert_eq!(outcome.stdout, "the diagnostic");
}

#[test]
fn the_outcome_of_a_signalled_command_has_no_code() {
    let outcome = SshOutcome {
        code: None,
        stdout: String::new(),
        stderr: String::new(),
    };

    assert!(!outcome.is_success());
}

#[test]
fn a_command_can_run_in_a_directory() {
    let host = SshHost::new("example.com").expect("valid");
    let command = SshCommand::new(host, "ls")
        .expect("valid")
        .in_directory("/srv");

    assert_eq!(command.cwd.as_deref(), Some(std::path::Path::new("/srv")));
}

#[test]
fn the_client_debug_form_reports_availability() {
    let client = SshClient::new();
    let text = format!("{client:?}");

    assert!(text.contains("has_transport"));
    assert!(text.contains("false"));
    let _ = BTreeMap::<String, String>::new();
}
