//! Tests for the access policy layer.
//!
//! The property that matters is that the policy is consulted before the handler runs, so a method that never thought
//! about access still cannot be reached by a peer the policy refuses. A handler-local check would encode the rule once
//! per method, and the first one somebody forgot would be the hole.

use std::sync::Arc;

use girder_core::{
    Access, AccessDecision, AccessRequest, AccessResource, AllowList, Declaration, DenyAll, Policy,
    ReadOnly, RemoteCall, RemoteError, RemoteRegistry,
};
use serde_json::json;

fn call(peer: &str) -> RemoteCall {
    RemoteCall {
        args: json!({}),
        peer: peer.to_owned(),
    }
}

/// A registry with one read method and one destructive method.
fn registry(policy: Option<Arc<dyn Policy>>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    registry.register("session", "list", |_call| {
        Box::pin(async { Ok(json!({"items": []})) })
    });
    registry.register("session", "delete", |_call| {
        Box::pin(async { Ok(json!({"deleted": true})) })
    });
    registry.declare("session", "list", Declaration::read("session"));
    registry.declare("session", "delete", Declaration::destructive("session"));
    match policy {
        Some(policy) => registry.with_policy(policy),
        None => registry,
    }
}

// ---- no policy --------------------------------------------------------------------------------

#[tokio::test]
async fn without_a_policy_every_call_proceeds() {
    // The default is "none configured", which is what a single-user harness has: a policy that allowed everything
    // would hide the fact that nothing was set up.
    let registry = registry(None);

    assert!(
        registry
            .dispatch("session", "list", call("anyone"))
            .await
            .is_ok()
    );
    assert!(
        registry
            .dispatch("session", "delete", call("anyone"))
            .await
            .is_ok()
    );
}

// ---- deny all ---------------------------------------------------------------------------------

#[tokio::test]
async fn deny_all_refuses_even_a_read() {
    let registry = registry(Some(Arc::new(DenyAll)));

    let error = registry
        .dispatch("session", "list", call("anyone"))
        .await
        .expect_err("refused");

    assert!(matches!(error, RemoteError::Refused { .. }));
}

#[tokio::test]
async fn a_refusal_has_its_own_code() {
    // A client that branches on FAILED would retry; one that sees REFUSED can stop and report.
    let registry = registry(Some(Arc::new(DenyAll)));

    let error = registry
        .dispatch("session", "list", call("anyone"))
        .await
        .expect_err("refused");

    assert_eq!(error.code(), "REFUSED");
}

#[tokio::test]
async fn a_refusal_names_the_method_and_the_reason() {
    // "Refused" alone is unactionable; a caller that knows which method and why can tell its operator.
    let registry = registry(Some(Arc::new(DenyAll)));

    let error = registry
        .dispatch("session", "delete", call("anyone"))
        .await
        .expect_err("refused");

    let text = error.to_string();
    assert!(text.contains("session.delete"), "{text}");
    assert!(text.contains("not permitted"), "{text}");
}

#[tokio::test]
async fn the_handler_never_runs_when_the_policy_refuses() {
    // The whole point of checking before dispatch rather than inside the handler.
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    let mut registry = RemoteRegistry::new();
    registry.register("session", "delete", move |_call| {
        let flag = Arc::clone(&flag);
        Box::pin(async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({}))
        })
    });
    let registry = registry.with_policy(Arc::new(DenyAll));

    let _ = registry.dispatch("session", "delete", call("anyone")).await;

    assert!(
        !ran.load(std::sync::atomic::Ordering::SeqCst),
        "a refused method must not run"
    );
}

// ---- read only --------------------------------------------------------------------------------

#[tokio::test]
async fn read_only_allows_a_read_and_refuses_a_write() {
    let registry = registry(Some(Arc::new(ReadOnly)));

    assert!(
        registry
            .dispatch("session", "list", call("viewer"))
            .await
            .is_ok()
    );
    let error = registry
        .dispatch("session", "delete", call("viewer"))
        .await
        .expect_err("refused");
    assert!(matches!(error, RemoteError::Refused { .. }));
}

#[tokio::test]
async fn an_undeclared_method_is_treated_as_a_read() {
    // A listing the moment a read-only policy was mounted must not be refused: treating an undeclared method as a
    // write would refuse every read that forgot to declare itself.
    let mut registry = RemoteRegistry::new();
    registry.register("session", "page", |_call| {
        Box::pin(async { Ok(json!({"events": []})) })
    });
    let registry = registry.with_policy(Arc::new(ReadOnly));

    assert!(
        registry
            .dispatch("session", "page", call("viewer"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn an_undeclared_method_is_still_refused_by_deny_all() {
    // The permissive default for undeclared methods is a default, not a bypass.
    let mut registry = RemoteRegistry::new();
    registry.register("session", "page", |_call| Box::pin(async { Ok(json!({})) }));
    let registry = registry.with_policy(Arc::new(DenyAll));

    assert!(
        registry
            .dispatch("session", "page", call("viewer"))
            .await
            .is_err()
    );
}

// ---- allow list -------------------------------------------------------------------------------

#[tokio::test]
async fn an_allow_list_permits_a_named_peer() {
    let registry = registry(Some(Arc::new(AllowList::new().allowing("operator"))));

    assert!(
        registry
            .dispatch("session", "delete", call("operator"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn an_allow_list_refuses_another_peer() {
    let registry = registry(Some(Arc::new(AllowList::new().allowing("operator"))));

    let error = registry
        .dispatch("session", "delete", call("stranger"))
        .await
        .expect_err("refused");

    let text = error.to_string();
    assert!(text.contains("stranger"), "{text}");
}

#[tokio::test]
async fn a_public_method_reaches_every_peer() {
    // A harness needs a way to let an unauthenticated client start: the calls it takes to get a name.
    let registry = registry(Some(Arc::new(AllowList::new().public("session.list"))));

    assert!(
        registry
            .dispatch("session", "list", call("stranger"))
            .await
            .is_ok()
    );
    assert!(
        registry
            .dispatch("session", "delete", call("stranger"))
            .await
            .is_err()
    );
}

// ---- the policy itself ------------------------------------------------------------------------

#[test]
fn read_only_refuses_both_write_kinds() {
    let policy = ReadOnly;

    for access in [Access::Write, Access::Destructive] {
        let decision = policy.decide(&AccessRequest {
            peer: "p".to_owned(),
            method: "m".to_owned(),
            access,
            resources: vec![AccessResource::any_of("session")],
        });
        assert!(!decision.is_allowed(), "{access:?} must be refused");
    }
}

#[test]
fn read_only_allows_a_read() {
    let decision = ReadOnly.decide(&AccessRequest {
        peer: "p".to_owned(),
        method: "m".to_owned(),
        access: Access::Read,
        resources: vec![AccessResource::any_of("session")],
    });

    assert!(decision.is_allowed());
    assert_eq!(decision, AccessDecision::Allow);
}

#[test]
fn a_declaration_carries_its_access_and_resources() {
    let declaration = Declaration::destructive("session");

    assert_eq!(declaration.access, Access::Destructive);
    assert_eq!(declaration.resources.len(), 1);
    assert_eq!(declaration.resources[0].kind, "session");
    assert_eq!(declaration.resources[0].id, "", "any of that kind");
}
