//! `llm.*`: the three endpoints upstream marks `@Remote` on `LlmRuntime`.
//!
//! Before this, `llm::llm_final::Runtime` held the whole runtime and registered no endpoint, so a
//! client could not ask a running harness which providers it has. The property under test is that
//! the service's declared surface and its handlers agree, and that a call reaches the live runtime.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use girder_core::llm::llm_final::Runtime;
use girder_core::{RemoteCall, RemoteError, RemoteRegistry};
use serde_json::{Value, json};

/// A registry with `llm.*` registered.
fn registry() -> (RemoteRegistry, usize) {
    let mut registry = RemoteRegistry::new();
    let runtime = Arc::new(Runtime::new());
    let installed = girder_core::register_llm_remote(&mut registry, &runtime)
        .expect("every endpoint has a handler");
    (registry, installed)
}

/// Invokes one `llm` endpoint.
fn call(method: &str, args: Value) -> Result<Value, RemoteError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (registry, _) = registry();
    runtime.block_on(registry.dispatch(
        "llm",
        method,
        RemoteCall {
            args,
            peer: "test".to_owned(),
        },
    ))
}

#[test]
fn the_surface_is_the_three_upstream_marks() {
    // `packages/llm/llm/src/index.ts` marks exactly three methods, and one of them renames the wire.
    let mut names = girder_core::endpoint_names::<girder_core::LlmRuntimeKey>();
    names.sort();
    assert_eq!(
        names,
        vec![
            "discoverModels".to_owned(),
            "listConfigurableProviders".to_owned(),
            "listProviders".to_owned(),
        ]
    );
}

#[test]
fn registering_installs_every_endpoint() {
    let (_, installed) = registry();
    assert_eq!(installed, 3);
}

#[test]
fn list_providers_answers_from_the_live_runtime() {
    // An empty runtime answers with an empty list, which is a *success*: "I have no providers" is
    // different from "I could not tell you".
    let answer = call("listProviders", json!({})).expect("dispatched");
    assert_eq!(answer, json!([]));
}

#[test]
fn list_configurable_providers_answers() {
    let answer = call("listConfigurableProviders", json!({})).expect("dispatched");
    assert_eq!(answer, json!([]));
}

#[test]
fn discover_models_without_a_registered_namespace_is_not_implemented() {
    // The distinction a client branches on: a deployment with no discovery for that namespace is a
    // capability it does not have, not a typo in the method name.
    let error = call("discoverModels", json!({ "settingsNs": "absent" }))
        .expect_err("no discovery registered");
    assert!(
        matches!(error, RemoteError::NotImplemented { .. }),
        "a missing discovery transport is not a bad argument: {error:?}"
    );
}

#[test]
fn an_unknown_endpoint_is_unknown_rather_than_silent() {
    let error = call("noSuchThing", json!({})).expect_err("not an endpoint");
    assert!(
        matches!(error, RemoteError::UnknownMethod { .. }),
        "{error:?}"
    );
}

#[test]
fn a_missing_settings_namespace_is_treated_as_empty() {
    // Upstream requires the argument; absent here it is the empty string, which no registration
    // claims, so the answer is the same refusal rather than a panic.
    let error = call("discoverModels", json!({})).expect_err("empty namespace");
    assert!(
        matches!(error, RemoteError::NotImplemented { .. }),
        "{error:?}"
    );
}
