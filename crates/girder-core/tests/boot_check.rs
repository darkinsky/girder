//! Tests for the startup check.
//!
//! The distinction that matters: a finding that stops the server and one that does not. Reporting a missing provider as
//! fatal would take away the offline path, and reporting a missing workspace as a note would let every file tool fail
//! one call at a time with a message about a path rather than about the deployment.

use std::path::PathBuf;

use girder_core::{Deployment, Severity, boot_is_fatal, boot_report, check_deployment};

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-boot-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// A deployment with everything working.
fn healthy(workspace: &std::path::Path) -> Deployment {
    Deployment {
        provider: true,
        sessions_root: Some(workspace.join("sessions")),
        workspace: Some(workspace.to_path_buf()),
        terminal: true,
        desktop: true,
        shell: true,
    }
}

#[test]
fn a_healthy_deployment_has_nothing_to_report() {
    let dir = scratch("healthy");

    let findings = check_deployment(&healthy(&dir));

    // The sessions directory does not exist yet, which is a note about a fresh deployment rather than a problem.
    assert!(
        findings
            .iter()
            .all(|finding| finding.severity == Severity::Note),
        "{findings:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_workspace_is_fatal() {
    // Every file tool would fail one call at a time with a message about a path rather than about the deployment.
    let dir = scratch("workspace");
    let mut deployment = healthy(&dir);
    deployment.workspace = Some(dir.join("does-not-exist"));

    let findings = check_deployment(&deployment);

    assert!(boot_is_fatal(&findings), "{findings:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_provider_is_not_fatal() {
    // The offline path drives the real tools without a model, and a deployment using it should not need a dummy key.
    let dir = scratch("provider");
    let mut deployment = healthy(&dir);
    deployment.provider = false;

    let findings = check_deployment(&deployment);

    assert!(!boot_is_fatal(&findings), "{findings:?}");
    assert!(
        findings
            .iter()
            .any(|finding| finding.subject.contains("provider")
                && finding.severity == Severity::Degraded),
        "{findings:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_sessions_root_that_is_a_file_is_fatal() {
    let dir = scratch("sessions-file");
    let mut deployment = healthy(&dir);
    std::fs::write(dir.join("sessions"), "not a directory").expect("written");
    deployment.sessions_root = Some(dir.join("sessions"));

    let findings = check_deployment(&deployment);

    assert!(boot_is_fatal(&findings), "{findings:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_sessions_root_is_only_a_note() {
    // The store creates it on first write, so a fresh deployment must not be refused.
    let dir = scratch("sessions-absent");
    let deployment = healthy(&dir);

    let findings = check_deployment(&deployment);

    assert!(!boot_is_fatal(&findings));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_shell_degrades_the_command_tools() {
    let dir = scratch("shell");
    let mut deployment = healthy(&dir);
    deployment.shell = false;

    let findings = check_deployment(&deployment);

    assert!(
        !boot_is_fatal(&findings),
        "the rest of the server still works"
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding.subject.contains("shell")),
        "{findings:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_desktop_is_a_note_rather_than_a_problem() {
    // Normal on a server: the path handoff refuses, and nothing else depends on it.
    let dir = scratch("desktop");
    let mut deployment = healthy(&dir);
    deployment.desktop = false;

    let findings = check_deployment(&deployment);

    let desktop: Vec<_> = findings
        .iter()
        .filter(|finding| finding.subject.contains("no desktop"))
        .collect();
    assert_eq!(desktop.len(), 1, "{findings:?}");
    assert_eq!(desktop[0].severity, Severity::Note);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_report_leads_with_the_fatal_finding() {
    // A caller reading the report stops at the first line that says the server cannot run.
    let dir = scratch("order");
    let mut deployment = healthy(&dir);
    deployment.workspace = Some(dir.join("absent"));
    deployment.provider = false;
    deployment.shell = false;

    let findings = check_deployment(&deployment);
    let text = boot_report(&findings);

    let fatal_at = text.find("fatal:").expect("a fatal line");
    let degraded_at = text.find("degraded:").expect("a degraded line");
    assert!(fatal_at < degraded_at, "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_finding_says_what_to_do() {
    // A report that says only what is wrong leaves the reader to work out the remedy, which is the part they did not
    // know.
    let dir = scratch("remedy");
    let mut deployment = healthy(&dir);
    deployment.provider = false;
    deployment.shell = false;
    deployment.terminal = false;
    deployment.desktop = false;

    for finding in check_deployment(&deployment) {
        assert!(!finding.remedy.trim().is_empty(), "{finding:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_report_is_stable_between_runs() {
    // Two reports of one deployment must not differ in order, or a caller diffing them sees noise.
    let dir = scratch("stable");
    let mut deployment = healthy(&dir);
    deployment.provider = false;
    deployment.shell = false;

    let first = boot_report(&check_deployment(&deployment));
    let second = boot_report(&check_deployment(&deployment));

    assert_eq!(first, second);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_directory_may_still_be_created() {
    let dir = scratch("may-be");
    let absent = dir.join("x");

    // Missing answers true: it may be about to be created, and treating absent as broken would report every fresh
    // deployment as fatal.
    assert!(girder_core::may_be_a_directory(&absent));
    assert!(girder_core::may_be_a_directory(&dir));
    std::fs::write(&absent, "file").expect("written");
    assert!(!girder_core::may_be_a_directory(&absent));
    let _ = std::fs::remove_dir_all(&dir);
}
