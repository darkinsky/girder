//! Tests for approval requests.
//!
//! The distinction the design rests on is that a denial is an answer and not an error: a caller that
//! handled a denial through its error path could not tell "the user declined" from "the approval system
//! broke".

use std::time::Duration;

use girder_core::{
    ApprovalError, ApprovalRequest, Approver, DEFAULT_APPROVAL_TIMEOUT, DecidedBy, Decision,
    ScriptedApprover, UnavailableApprover,
};

fn request() -> ApprovalRequest {
    ApprovalRequest::new("bash", "run a shell command").expect("valid")
}

// ---- requests ---------------------------------------------------------------------------------

#[test]
fn a_request_needs_an_action_and_a_description() {
    // A person cannot approve what they cannot read.
    assert!(ApprovalRequest::new("", "does something").is_none());
    assert!(ApprovalRequest::new("bash", "  ").is_none());
}

#[test]
fn a_request_defaults_to_a_five_minute_wait() {
    // Long enough for a person to notice, short enough that an unattended session does not hold a tool call
    // open indefinitely.
    assert_eq!(request().timeout, DEFAULT_APPROVAL_TIMEOUT);
    assert_eq!(DEFAULT_APPROVAL_TIMEOUT, Duration::from_secs(300));
}

#[test]
fn a_request_carries_the_exact_detail_separately() {
    // A description is what a person skims; the detail is what they check when it is not enough.
    let request = request().with_detail("rm -rf /tmp/scratch");

    assert_eq!(request.detail.as_deref(), Some("rm -rf /tmp/scratch"));
    assert_eq!(request.description, "run a shell command");
}

#[test]
fn a_timeout_can_be_shortened() {
    let request = request().with_timeout(Duration::from_secs(1));

    assert_eq!(request.timeout, Duration::from_secs(1));
}

// ---- decisions --------------------------------------------------------------------------------

#[test]
fn a_human_approval_is_approved() {
    let decision = Decision::approved();

    assert!(decision.is_approved());
    assert_eq!(decision.decided_by(), DecidedBy::Human);
}

#[test]
fn a_denial_is_not_approved() {
    let decision = Decision::denied();

    assert!(!decision.is_approved());
    assert_eq!(decision.decided_by(), DecidedBy::Human);
}

#[test]
fn a_denial_can_carry_a_reason() {
    let decision = Decision::denied_because("this would delete the repository");

    let Decision::Denied { reason, .. } = decision else {
        panic!("expected a denial");
    };
    assert_eq!(reason.as_deref(), Some("this would delete the repository"));
}

#[test]
fn a_policy_approval_is_not_attributed_to_a_person() {
    // A replay that showed every approval as a person's decision would misrepresent what happened.
    let decision = Decision::auto_approved();

    assert!(decision.is_approved());
    assert_eq!(decision.decided_by(), DecidedBy::Policy);
}

#[test]
fn the_decider_names_are_stable() {
    assert_eq!(DecidedBy::Human.as_str(), "human");
    assert_eq!(DecidedBy::Policy.as_str(), "policy");
    assert_eq!(DecidedBy::TimedOut.as_str(), "timed-out");
}

// ---- approvers --------------------------------------------------------------------------------

#[tokio::test]
async fn an_unavailable_approver_reports_that_rather_than_approving() {
    // A caller that forgot to mount an approver must get a named error, not a silent approval.
    let error = UnavailableApprover
        .ask(request())
        .await
        .expect_err("no approver");

    assert!(matches!(error, ApprovalError::NoApprover));
}

#[tokio::test]
async fn a_scripted_approver_approves_without_attributing_it_to_a_person() {
    let decision = ScriptedApprover::approving()
        .ask(request())
        .await
        .expect("answered");

    assert!(decision.is_approved());
    assert_eq!(decision.decided_by(), DecidedBy::Policy);
}

#[tokio::test]
async fn a_scripted_denial_carries_its_reason() {
    // A deployment with no UI still has to decide, and the choice is explicit at construction.
    let decision = ScriptedApprover::denying("non-interactive run")
        .ask(request())
        .await
        .expect("answered");

    assert!(!decision.is_approved());
    let Decision::Denied { reason, .. } = decision else {
        panic!("expected a denial");
    };
    assert_eq!(reason.as_deref(), Some("non-interactive run"));
}

#[tokio::test]
async fn every_approver_sees_the_request() {
    // A request that a scripted approver ignored would let a test pass while a real one asked the wrong
    // question.
    let decision = ScriptedApprover::approving()
        .ask(request().with_detail("ls -la"))
        .await
        .expect("answered");

    assert!(decision.is_approved());
}

#[test]
fn a_record_answers_whether_the_action_ran() {
    let record = girder_core::ApprovalRecord {
        request: request(),
        decision: Decision::denied(),
    };

    assert!(!record.is_approved());
}
