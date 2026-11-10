//! Tests for asking a person to approve an extension.
//!
//! The gap this closes: `approve` recorded a signature that nothing had asked for, so an extension could be approved
//! by a caller that never described it to anyone.

use girder_core::{
    Decision, ExtensionDefinition, ExtensionError, ExtensionId, ExtensionRegistry,
    ScriptedApprover, UnavailableApprover,
};

fn id(value: &str) -> ExtensionId {
    ExtensionId::new(value).expect("valid")
}

fn definition(name: &str) -> ExtensionDefinition {
    ExtensionDefinition {
        id: id(name),
        name: name.to_owned(),
        description: format!("the {name} extension"),
        source: "test".to_owned(),
        requires: Vec::new(),
        approved_by: None,
    }
}

/// A runtime that refuses to start anything, so a test can reach the gates before it.
struct Refusing;

impl girder_core::ExtensionRuntime for Refusing {
    fn start(
        &self,
        _definition: &ExtensionDefinition,
        _available: &[String],
    ) -> girder_core::BoxFut<Result<girder_core::ExtensionRun, ExtensionError>> {
        Box::pin(async {
            Err(ExtensionError::NotApproved {
                id: "refusing".to_owned(),
            })
        })
    }
}

fn registry(name: &str) -> ExtensionRegistry {
    let registry = ExtensionRegistry::new();
    registry.define(definition(name)).expect("defined");
    registry
}

#[tokio::test]
async fn an_approving_approver_approves_the_extension() {
    let registry = registry("a");
    let approver = ScriptedApprover::approving();

    let decision = registry
        .request_approval("a", &approver)
        .await
        .expect("approved");

    assert!(decision.is_approved());
    assert!(
        registry
            .definition("a")
            .expect("present")
            .approved_by
            .is_some()
    );
}

#[tokio::test]
async fn a_denying_approver_leaves_the_extension_unapproved() {
    // A refusal is a decision about this extension, not a failure of the approval system.
    let registry = registry("a");
    let approver = ScriptedApprover::denying("not in this session");

    let error = registry
        .request_approval("a", &approver)
        .await
        .expect_err("denied");

    assert!(matches!(error, ExtensionError::Denied { .. }));
    assert!(
        registry
            .definition("a")
            .expect("present")
            .approved_by
            .is_none(),
        "a denial must not record a signature"
    );
}

#[tokio::test]
async fn an_unavailable_approver_is_distinct_from_a_denial() {
    // Nobody was asked, versus somebody said no: a caller reports them differently.
    let registry = registry("a");

    let error = registry
        .request_approval("a", &UnavailableApprover)
        .await
        .expect_err("no approver");

    assert!(matches!(error, ExtensionError::ApprovalFailed { .. }));
    assert!(!matches!(error, ExtensionError::Denied { .. }));
}

#[tokio::test]
async fn an_unapproved_extension_cannot_be_activated() {
    // The order in `activate` is approval first, so an extension nobody approved never reaches the runtime.
    let registry = registry("a");
    let runtime = Refusing;

    let error = registry
        .activate("a", &[], &runtime)
        .await
        .expect_err("not approved");

    assert!(matches!(error, ExtensionError::NotApproved { .. }));
}

#[tokio::test]
async fn an_approval_lets_activation_reach_the_runtime() {
    // The refuser's own error carries a different id, which is how this proves the approval gate let it through
    // rather than the refusal happening earlier.
    let registry = registry("a");
    registry
        .request_approval("a", &ScriptedApprover::approving())
        .await
        .expect("approved");

    let error = registry
        .activate("a", &[], &Refusing)
        .await
        .expect_err("gate");

    let ExtensionError::NotApproved { id } = &error else {
        panic!("expected the runtime's refusal, got {error:?}");
    };
    assert_eq!(
        id, "refusing",
        "the runtime was reached, so approval passed"
    );
}

#[tokio::test]
async fn requesting_approval_for_an_unknown_extension_is_an_error() {
    let registry = ExtensionRegistry::new();

    let error = registry
        .request_approval("nope", &ScriptedApprover::approving())
        .await
        .expect_err("unknown");

    assert!(matches!(error, ExtensionError::NotFound { .. }));
}

#[tokio::test]
async fn the_recorded_approver_names_who_decided() {
    // A replay that showed a policy approval as a person's decision would misrepresent what happened.
    let registry = registry("a");

    registry
        .request_approval("a", &ScriptedApprover::approving())
        .await
        .expect("approved");

    let by = registry
        .definition("a")
        .expect("present")
        .approved_by
        .expect("recorded");
    assert!(
        by.starts_with("policy"),
        "the scripted approver is a policy: {by}"
    );
}

#[test]
fn the_decision_type_distinguishes_approval_from_refusal() {
    assert!(Decision::approved().is_approved());
    assert!(!Decision::denied().is_approved());
}
