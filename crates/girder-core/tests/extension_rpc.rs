//! Tests for the extension RPC surface.
//!
//! An extension is code a deployment adds, so listing and approving are separate operations: an approval granted as a
//! side effect of activation is one nobody was asked for.

use std::sync::Arc;

use girder_core::{
    ExtensionDefinition, ExtensionId, ExtensionRegistry, RemoteCall, RemoteRegistry,
};
use serde_json::json;

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn registry(extensions: &Arc<ExtensionRegistry>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    girder_core::register_extension_methods(&mut registry, extensions);
    registry
}

fn definition(id: &str) -> ExtensionDefinition {
    ExtensionDefinition {
        id: ExtensionId::new(id).expect("valid"),
        name: id.to_owned(),
        description: format!("the {id} extension"),
        source: "test".to_owned(),
        requires: Vec::new(),
        approved_by: None,
    }
}

#[tokio::test]
async fn list_reports_an_unapproved_extension() {
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions.define(definition("a")).expect("defined");

    let value = registry(&extensions)
        .dispatch("extension", "list", call(json!({})))
        .await
        .expect("listed");

    let listed = value["extensions"].as_array().expect("an array");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "a");
    // A client that showed "installed" as "running" would misreport every extension waiting for approval.
    assert!(listed[0]["approvedBy"].is_null(), "not approved yet");
}

#[tokio::test]
async fn approving_an_extension_records_who_did_it() {
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions.define(definition("a")).expect("defined");

    registry(&extensions)
        .dispatch(
            "extension",
            "approve",
            call(json!({"id": "a", "by": "the operator"})),
        )
        .await
        .expect("approved");

    let value = registry(&extensions)
        .dispatch("extension", "list", call(json!({})))
        .await
        .expect("listed");
    assert_eq!(value["extensions"][0]["approvedBy"], "the operator");
}

#[tokio::test]
async fn approving_without_a_name_records_a_default_rather_than_nothing() {
    // An approval with no attribution is still an approval, and leaving the field empty would make it look unattributed
    // in a way a reader cannot act on.
    let extensions = Arc::new(ExtensionRegistry::new());
    extensions.define(definition("a")).expect("defined");

    registry(&extensions)
        .dispatch("extension", "approve", call(json!({"id": "a"})))
        .await
        .expect("approved");

    let value = registry(&extensions)
        .dispatch("extension", "list", call(json!({})))
        .await
        .expect("listed");
    assert_eq!(value["extensions"][0]["approvedBy"], "client");
}

#[tokio::test]
async fn approving_an_unknown_extension_is_named_in_the_error() {
    let extensions = Arc::new(ExtensionRegistry::new());

    let error = registry(&extensions)
        .dispatch("extension", "approve", call(json!({"id": "nope"})))
        .await
        .expect_err("unknown");

    let girder_core::RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("nope"), "{message}");
}

#[tokio::test]
async fn approving_without_an_id_is_a_protocol_error() {
    let extensions = Arc::new(ExtensionRegistry::new());

    let error = registry(&extensions)
        .dispatch("extension", "approve", call(json!({})))
        .await
        .expect_err("missing id");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
}

#[tokio::test]
async fn list_of_an_empty_registry_is_empty() {
    let extensions = Arc::new(ExtensionRegistry::new());

    let value = registry(&extensions)
        .dispatch("extension", "list", call(json!({})))
        .await
        .expect("listed");

    assert_eq!(value["extensions"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn list_reports_what_an_extension_requires() {
    // A client shows the dependency list, because an extension that cannot start for a missing service is installed
    // and unusable.
    let extensions = Arc::new(ExtensionRegistry::new());
    let mut with_requirement = definition("a");
    with_requirement.requires = vec!["fs".to_owned(), "shell".to_owned()];
    extensions.define(with_requirement).expect("defined");

    let value = registry(&extensions)
        .dispatch("extension", "list", call(json!({})))
        .await
        .expect("listed");

    let requires = value["extensions"][0]["requires"]
        .as_array()
        .expect("an array");
    assert_eq!(requires.len(), 2);
}
