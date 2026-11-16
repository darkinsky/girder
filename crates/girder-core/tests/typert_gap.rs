//! Tests for the Typert runtime registry and its protocol predicates.
//!
//! Corresponds to upstream `packages/typert/registry/tests/typert.spec.ts` (780 lines) and the
//! protocol predicates in `packages/typert/protocol`. The Cordis-lifecycle cases in the upstream
//! spec (plugin fiber dispose, `ctx.effect` withdrawal, `await fiber.dispose()`) have no local
//! counterpart because this crate has no Cordis Context; their local equivalent is the owner-scoped
//! [`TypertRegistry::withdraw`] / [`TypertRegistry::unmount_remote`], which is what is tested here.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than
//! a copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/typert_gap.rs"]
mod typert_gap;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use girder_core::{Field, ObjectSchema, Schema};
use typert_gap::{
    ChangeKind, ClientContextAdapter, Contribution, Face, GLOBAL_RESOURCE_KIND, HostContextAdapter,
    InvocationDescriptor, InvocationParameter, LookupDeclaration, LookupProvider, Mode,
    PackageFilter, PackageModel, Receiver, RegistryChange, RemoteContribution, RemoteError,
    RemoteErrorCode, ResourceDeclaration, ResourceIdentity, ResourceResolveInput, SchemaFilter,
    Scope, TypertError, TypertRegistry, UNIVERSAL_ERROR_CODES, endpoint_of, is_remote_segment,
    package_key, principal_of, remote_error_of, schema_key, validate_invocation,
};

// ---- helpers ------------------------------------------------------------------------------------

/// One `session/page` descriptor, the fixture both upstream and this test use.
fn page() -> InvocationDescriptor {
    InvocationDescriptor::new("@fixture/remote#session/page", "session", "session", "page")
        .with_parameter(InvocationParameter::json("request"))
        .returning("@fixture#Page")
}

/// One JSON-Schema-visible object schema.
fn page_schema() -> ObjectSchema {
    ObjectSchema::new("Page", vec![Field::required("limit", Schema::Int64)])
}

/// A contribution with one schema, one invocation, and one service model entry.
fn tools_contribution() -> Contribution {
    Contribution::new("@deepseek-ai/dsh-tools")
        .with_schema("ToolInput", page_schema)
        .with_invocation(page())
        .with_model(PackageModel {
            services: vec![typert_gap::ServiceModel {
                key: "tools".to_owned(),
                export_name: "ToolRuntime".to_owned(),
                members: vec!["register(definition: ToolDefinition)".to_owned()],
            }],
            events: vec![typert_gap::EventModel {
                name: "tools/change".to_owned(),
                mode: Some("emit".to_owned()),
                signature: "'tools/change'(): void".to_owned(),
            }],
            objects: Vec::new(),
        })
}

// ---- keys ---------------------------------------------------------------------------------------

/// Upstream's three key helpers, asserted against the same literals its spec pins.
#[test]
fn the_key_helpers_compose_upstream_literals() {
    assert_eq!(
        schema_key("@deepseek-ai/dsh-tools", "ToolInput"),
        "@deepseek-ai/dsh-tools#ToolInput"
    );
    assert_eq!(
        package_key("@deepseek-ai/dsh-tools", Face::Host),
        "@deepseek-ai/dsh-tools#host"
    );
    assert_eq!(
        endpoint_of("goals", "create"),
        "goals/create",
        "the endpoint is `<namespace>/<method>`"
    );
    assert_eq!(page().endpoint(), "session/page");
}

/// The endpoint grammar: `.` and `..` are refused even though every character in them is legal.
#[test]
fn the_endpoint_grammar_refuses_dot_segments_and_foreign_characters() {
    for accepted in [
        "create",
        "create-goal",
        "create_goal",
        "$create",
        "a.b",
        "v2",
    ] {
        assert!(
            is_remote_segment(accepted),
            "{accepted} should be transportable"
        );
    }
    for refused in [
        "",
        ".",
        "..",
        "create goal",
        "create#v2",
        "create/goal",
        "café",
    ] {
        assert!(!is_remote_segment(refused), "{refused:?} should be refused");
    }
}

// ---- schemas and package reflection -------------------------------------------------------------

/// Registering separates schemas from package reflection, and both are queryable by key.
#[test]
fn schemas_and_package_reflection_are_registered_and_queried_separately() {
    let mut registry = TypertRegistry::new();
    registry.register(tools_contribution()).unwrap();

    let schema = registry
        .get("@deepseek-ai/dsh-tools#ToolInput")
        .expect("the schema is registered");
    assert_eq!(schema.name, "ToolInput");
    assert_eq!(schema.package, "@deepseek-ai/dsh-tools");
    assert_eq!(schema.face, Face::Host);
    assert_eq!(schema.schema.name, "Page");

    let package = registry
        .get_package("@deepseek-ai/dsh-tools", Face::Host)
        .expect("the package model is registered");
    assert_eq!(package.key, "@deepseek-ai/dsh-tools#host");
    assert_eq!(package.model.services[0].key, "tools");

    assert_eq!(registry.list(&SchemaFilter::default()).len(), 1);
    assert_eq!(registry.list_packages(&PackageFilter::default()).len(), 1);
}

/// The exact disposer removes the schema and the package model, and nothing else.
#[test]
fn the_contribution_id_withdraws_the_schema_and_the_package_model() {
    let mut registry = TypertRegistry::new();
    let other = registry
        .register(Contribution::new("@fixture/other").with_schema("Other", page_schema))
        .unwrap();
    let id = registry.register(tools_contribution()).unwrap();
    assert!(
        registry
            .get_package("@deepseek-ai/dsh-tools", Face::Host)
            .is_some()
    );

    registry.withdraw(id);

    assert!(registry.get("@deepseek-ai/dsh-tools#ToolInput").is_none());
    assert!(
        registry
            .get_package("@deepseek-ai/dsh-tools", Face::Host)
            .is_none()
    );
    assert_eq!(
        registry.list_packages(&PackageFilter::default()).len(),
        1,
        "only this contribution's package face is withdrawn"
    );
    // A withdrawal is owner-scoped: the other contribution is untouched.
    assert!(registry.get("@fixture/other#Other").is_some());
    registry.withdraw(other);
    assert!(registry.get("@fixture/other#Other").is_none());
}

/// A second registration of the same package face is rejected *before* committing, so the first
/// contribution's schema survives with its original factory.
#[test]
fn a_duplicate_package_face_is_rejected_before_committing() {
    let mut registry = TypertRegistry::new();
    registry.register(tools_contribution()).unwrap();

    let error = registry.register(tools_contribution()).unwrap_err();
    assert!(matches!(error, TypertError::PackageRegistered { .. }));
    assert_eq!(
        registry
            .get("@deepseek-ai/dsh-tools#ToolInput")
            .unwrap()
            .schema
            .name,
        "Page"
    );

    let duplicate_batch = Contribution::new("@fixture/duplicate")
        .with_schema("Same", page_schema)
        .with_schema("Same", page_schema);
    let error = registry.register(duplicate_batch).unwrap_err();
    assert!(matches!(error, TypertError::SchemaRegistered { .. }));
    assert!(
        registry
            .get_package("@fixture/duplicate", Face::Host)
            .is_none(),
        "a rejected batch registers nothing"
    );
}

/// Malformed identities are refused, and both registry views filter.
#[test]
fn malformed_contribution_identities_are_refused_and_both_views_filter() {
    let mut registry = TypertRegistry::new();
    registry.register(tools_contribution()).unwrap();

    for package in ["", "bad#package"] {
        let error = registry
            .register(Contribution::new(package).with_schema("Value", page_schema))
            .unwrap_err();
        assert!(
            matches!(error, TypertError::Key { .. }),
            "{package:?} must fail on the package name"
        );
    }

    let error = registry
        .register(Contribution::new("@fixture/schema-name").with_schema("bad#name", page_schema))
        .unwrap_err();
    assert!(matches!(error, TypertError::Key { .. }));

    let absent = SchemaFilter {
        package: Some("@fixture/absent".to_owned()),
        ..SchemaFilter::default()
    };
    assert!(registry.list(&absent).is_empty());
    let client = SchemaFilter {
        face: Some(Face::Client),
        ..SchemaFilter::default()
    };
    assert!(registry.list(&client).is_empty());
    let absent_packages = PackageFilter {
        package: Some("@fixture/absent".to_owned()),
        ..PackageFilter::default()
    };
    assert!(registry.list_packages(&absent_packages).is_empty());
    let client_packages = PackageFilter {
        face: Some(Face::Client),
        ..PackageFilter::default()
    };
    assert!(registry.list_packages(&client_packages).is_empty());
}

/// A schema factory runs at most once, no matter how often the schema is resolved.
#[test]
fn a_schema_factory_runs_at_most_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let mut registry = TypertRegistry::new();
    registry
        .register(
            Contribution::new("@fixture/lazy").with_schema("Lazy", move || {
                counted.fetch_add(1, Ordering::SeqCst);
                page_schema()
            }),
        )
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 0, "registration is lazy");
    let first = registry.resolve("@fixture/lazy#Lazy").unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let second = registry.resolve("@fixture/lazy#Lazy").unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the factory is not called twice"
    );
    assert!(
        Arc::ptr_eq(&first.schema, &second.schema),
        "the cached value is shared"
    );
}

/// Resolving reports three different failures for three different mistakes.
#[test]
fn resolve_distinguishes_a_malformed_key_from_a_missing_schema_and_package() {
    let mut registry = TypertRegistry::new();
    registry.register(tools_contribution()).unwrap();

    let missing = registry
        .resolve("@deepseek-ai/dsh-tools#Missing")
        .unwrap_err();
    assert!(
        matches!(missing, TypertError::UnknownSchema { .. }),
        "{missing}"
    );
    assert!(missing.to_string().contains("contributes no schema named"));

    let unpublished = registry.resolve("@fixture/absent#Value").unwrap_err();
    assert!(
        matches!(unpublished, TypertError::UnknownPackage { .. }),
        "{unpublished}"
    );
    assert!(
        unpublished
            .to_string()
            .contains("has no registered contribution")
    );

    let malformed = registry.resolve("invalid").unwrap_err();
    assert!(matches!(malformed, TypertError::InvalidSchemaKey { .. }));
    assert!(
        malformed
            .to_string()
            .contains("expected \"<package>#<name>\"")
    );

    // A trailing separator is malformed too: the name part is empty.
    let trailing = registry.resolve("@fixture/absent#").unwrap_err();
    assert!(matches!(trailing, TypertError::InvalidSchemaKey { .. }));
}

/// The JSON Schema projection is a standalone document, and every call produces a fresh value.
#[test]
fn the_json_schema_projection_is_a_fresh_standalone_document() {
    let mut registry = TypertRegistry::new();
    registry.register(tools_contribution()).unwrap();

    let document = registry
        .json_schema_for("@deepseek-ai/dsh-tools#ToolInput")
        .unwrap();
    let definitions = document.get("$defs").expect("the named type is hoisted");
    assert_eq!(
        definitions["Page"]["properties"]["limit"]["type"],
        json!("integer")
    );
    assert_eq!(definitions["Page"]["required"], json!(["limit"]));

    // Projecting again after mutating a consumer's copy must not hand back the mutation: the
    // projection is rebuilt from the schema, never cached.
    let mut consumer_copy = document.clone();
    consumer_copy["$defs"]["Page"]["title"] = json!("tampered");
    let again = registry
        .json_schema_for("@deepseek-ai/dsh-tools#ToolInput")
        .unwrap();
    assert_eq!(
        again["$defs"]["Page"].get("title"),
        None,
        "a consumer's mutation cannot leak into the next projection"
    );
}

// ---- local invocations --------------------------------------------------------------------------

/// A contribution registers its local invocations atomically with its reflection.
#[test]
fn local_invocations_register_atomically_with_the_generated_reflection() {
    let mut registry = TypertRegistry::new();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);
    registry.subscribe_local(Arc::new(move |change: &RegistryChange| {
        sink.lock().unwrap().push(change.to_string());
    }));

    assert!(!registry.has_seen("session/page"));
    let id = registry.register(tools_contribution()).unwrap();

    assert_eq!(registry.local("session/page"), Some(&page()));
    assert!(registry.has_seen("session/page"));
    assert_eq!(registry.local_invocations(), vec![page()]);
    assert_eq!(
        *observed.lock().unwrap(),
        vec!["local:session/page".to_owned()]
    );

    registry.withdraw(id);
    assert!(registry.local_invocations().is_empty());
    assert!(
        registry.has_seen("session/page"),
        "a withdrawn endpoint stays visible for the registry's lifetime"
    );
    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            "local:session/page".to_owned(),
            "local:session/page".to_owned()
        ]
    );
}

/// Duplicate endpoints and duplicate ids reject the whole batch, across the local and Remote
/// registries.
#[test]
fn duplicate_endpoints_and_ids_are_rejected_atomically() {
    let mut registry = TypertRegistry::new();
    registry.register(tools_contribution()).unwrap();

    let duplicate_endpoint = registry
        .mount_remote(&RemoteContribution::new(
            "@fixture/duplicate-endpoint",
            vec![
                page().returning("@fixture#First"),
                page().returning("@fixture#Second"),
            ],
        ))
        .unwrap_err();
    assert!(matches!(
        duplicate_endpoint,
        TypertError::DuplicateEndpoint { .. }
    ));

    let duplicate_id = registry
        .mount_remote(&RemoteContribution::new(
            "@fixture/duplicate-id",
            vec![
                InvocationDescriptor::new("@fixture/remote#same", "session", "session", "first")
                    .returning("@fixture#Value"),
                InvocationDescriptor::new("@fixture/remote#same", "session", "session", "second")
                    .returning("@fixture#Value"),
            ],
        ))
        .unwrap_err();
    assert!(matches!(duplicate_id, TypertError::DuplicateId { .. }));

    // The two stores are independent: a Remote mount may claim an endpoint the local registry
    // already serves, because they are different faces of the same name. What must collide is a
    // *local* registration against the local store.
    let collides = registry
        .register(
            Contribution::new("@fixture/existing-endpoint")
                .with_invocation(page().returning("@fixture#Value")),
        )
        .unwrap_err();
    assert!(matches!(collides, TypertError::DuplicateEndpoint { .. }));
    assert!(registry.remote_packages().is_empty());
}

/// An untransportable method name is refused by the same grammar a namespace is.
#[test]
fn an_untransportable_method_name_is_refused() {
    for method in ["create#v2", "create goal", ".", ".."] {
        let descriptor =
            InvocationDescriptor::new("@fixture/remote#goals/create", "goals", "goals", method)
                .returning("@fixture#Value");
        let error = validate_invocation(&descriptor).unwrap_err();
        assert!(
            matches!(error, TypertError::Segment { .. }),
            "{method:?} must fail on the endpoint grammar, got {error}"
        );
        assert!(
            error
                .to_string()
                .contains("RPC endpoint segment characters")
        );
    }
}

// ---- Remote mounts ------------------------------------------------------------------------------

/// A Remote contribution mounts and withdraws exactly, notifying both edges.
#[test]
fn a_remote_contribution_mounts_and_withdraws_exactly() {
    let mut registry = TypertRegistry::new();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);
    registry.subscribe_remote(Arc::new(move |change: &RegistryChange| {
        sink.lock().unwrap().push(change.to_string());
    }));

    let contribution = RemoteContribution::new("@fixture/remote", vec![page()]);
    let id = registry.mount_remote(&contribution).unwrap();

    assert_eq!(registry.remote("session/page"), Some(&page()));
    assert_eq!(
        registry.remote_packages(),
        vec!["@fixture/remote".to_owned()]
    );
    let error = registry.mount_remote(&contribution).unwrap_err();
    assert!(matches!(error, TypertError::RemotePackage { .. }));

    registry.unmount_remote(id);
    assert!(registry.remote_invocations().is_empty());
    assert!(registry.remote_packages().is_empty());
    assert_eq!(
        *observed.lock().unwrap(),
        vec![
            "remote:session/page".to_owned(),
            "remote:session/page".to_owned()
        ]
    );

    // The package name is released, so the same contribution mounts again.
    assert!(
        registry
            .mount_remote(&RemoteContribution::new("@fixture/remote", vec![page()]))
            .is_ok()
    );
}

// ---- scope projection ---------------------------------------------------------------------------

/// Only a direct invocation whose scope selects its single lookup parameter is accepted.
#[test]
fn only_a_direct_scope_selecting_its_unique_lookup_parameter_is_accepted() {
    let scoped = || {
        InvocationDescriptor::new(
            "@fixture/remote#session/scoped",
            "session",
            "session",
            "scoped",
        )
        .with_parameter(InvocationParameter::lookup("agent", "agentId", "fixture"))
        .with_parameter(InvocationParameter::json("request"))
        .with_scope(Scope::new("fixture", "agentId"))
        .returning("@fixture#Value")
    };
    let mut registry = TypertRegistry::new();
    let id = registry
        .mount_remote(&RemoteContribution::new("@fixture/scoped", vec![scoped()]))
        .unwrap();
    assert!(registry.remote("session/scoped").is_some());
    registry.unmount_remote(id);

    let cases: Vec<(InvocationDescriptor, &str)> = vec![
        (
            scoped().on_context("fixture", "scopeId"),
            "Context receiver cannot declare a direct scope projection",
        ),
        (
            scoped().with_scope(Scope::new("fixture", "missingId")),
            "must select its only lookup parameter",
        ),
        (
            scoped().with_parameter(InvocationParameter::lookup("other", "otherId", "fixture")),
            "must select its only lookup parameter",
        ),
        (
            scoped().with_scope(Scope::new("other", "agentId")),
            "must select its only lookup parameter",
        ),
    ];
    for (index, (candidate, message)) in cases.into_iter().enumerate() {
        let error = registry
            .mount_remote(&RemoteContribution::new(
                format!("@fixture/rejected-{index}"),
                vec![candidate],
            ))
            .unwrap_err();
        assert!(
            error.to_string().contains(message),
            "expected {message:?}, got {error}"
        );
    }
    assert!(registry.remote_invocations().is_empty());
}

// ---- lookup and Context providers ---------------------------------------------------------------

/// Lookup and Context providers register, resolve, and withdraw without a domain branch.
#[tokio::test]
async fn lookup_and_context_providers_register_and_resolve() {
    let mut registry = TypertRegistry::new();
    let looked_up = json!({ "id": "agent-1" });
    let resolved = looked_up.clone();
    let lookup = LookupProvider::new(
        LookupDeclaration::new(
            "fixture",
            "agent",
            "agentId",
            "@fixture/agent#Agent",
            "@fixture#SessionId",
        ),
        move |id| (id.as_str() == Some("agent-1")).then(|| resolved.clone()),
    );
    let lookup_id = registry.register_lookup(lookup).unwrap();
    let host_id = registry
        .register_host_context(
            "fixture",
            HostContextAdapter::new("agentId", "@fixture#SessionId", |id| {
                (id.as_str() == Some("agent-1")).then(|| "scoped".to_owned())
            }),
        )
        .unwrap();
    let client_id = registry
        .register_client_context(
            "fixture",
            ClientContextAdapter::new(
                |candidate| (candidate == "scoped").then(|| json!("agent-1")),
                |id| (id.as_str() == Some("agent-1")).then(|| "scoped".to_owned()),
            ),
        )
        .unwrap();

    assert_eq!(
        registry
            .resolve_lookup("fixture", &json!("agent-1"))
            .unwrap()
            .await,
        Some(looked_up)
    );
    assert_eq!(
        registry.lookup_definitions(),
        vec![&LookupDeclaration::new(
            "fixture",
            "agent",
            "agentId",
            "@fixture/agent#Agent",
            "@fixture#SessionId"
        )]
    );
    assert_eq!(
        registry
            .host_context("fixture")
            .unwrap()
            .resolve(&json!("agent-1"))
            .await,
        Some("scoped".to_owned())
    );
    let client = registry.client_context("fixture").unwrap();
    assert_eq!((client.identity)("scoped"), Some(json!("agent-1")));
    assert_eq!(
        (client.resolve)(&json!("agent-1")),
        Some("scoped".to_owned())
    );

    registry.withdraw_context(&client_id);
    registry.withdraw_context(&host_id);
    registry.withdraw_lookup(&lookup_id);
    assert!(registry.lookup_keys().is_empty());
    assert_eq!(
        registry.lookup_definitions().len(),
        1,
        "the declaration outlives the provider"
    );
    assert!(registry.host_context("fixture").is_none());
    assert!(registry.client_context("fixture").is_none());
}

/// A provider resolves through a configured policy when one exists, and the declaration freezes.
#[tokio::test]
async fn a_configured_resolution_policy_replaces_the_provider_default() {
    let mut registry = TypertRegistry::new();
    let original = registry
        .register_lookup(LookupProvider::new(
            LookupDeclaration::new(
                "fixture",
                "agent",
                "agentId",
                "@fixture#Agent",
                "@fixture#AgentId",
            ),
            |_| Some(json!("default")),
        ))
        .unwrap();
    registry
        .configure_lookup("fixture", |_id| {
            Box::pin(std::future::ready(Some(json!("configured"))))
        })
        .unwrap();
    assert_eq!(
        registry
            .resolve_lookup("fixture", &json!("agent-1"))
            .unwrap()
            .await,
        Some(json!("configured"))
    );

    let error = registry
        .configure_lookup("fixture", |_| Box::pin(std::future::ready(None)))
        .unwrap_err();
    assert!(
        matches!(error, TypertError::LookupConfigured { .. }),
        "{error}"
    );

    // A configured policy replaces the resolution, not the declaration. Withdrawing the provider
    // frees the key, and the wire declaration stays frozen for the registry's lifetime.
    registry.withdraw_lookup(&original);
    let id = registry
        .register_lookup(LookupProvider::new(
            LookupDeclaration::new(
                "fixture",
                "agent",
                "agentId",
                "@fixture#Agent",
                "@fixture#AgentId",
            ),
            |_| None,
        ))
        .unwrap();
    registry.withdraw_lookup(&id);
    for changed in [
        LookupDeclaration::new(
            "fixture",
            "session",
            "agentId",
            "@fixture#Agent",
            "@fixture#AgentId",
        ),
        LookupDeclaration::new(
            "fixture",
            "agent",
            "sessionId",
            "@fixture#Agent",
            "@fixture#AgentId",
        ),
        LookupDeclaration::new(
            "fixture",
            "agent",
            "agentId",
            "@fixture#Session",
            "@fixture#AgentId",
        ),
        LookupDeclaration::new(
            "fixture",
            "agent",
            "agentId",
            "@fixture#Agent",
            "@fixture#SessionId",
        ),
    ] {
        let error = registry
            .register_lookup(LookupProvider::new(changed, |_| None))
            .unwrap_err();
        assert!(
            matches!(error, TypertError::LookupRedeclared { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("changed its wire declaration"));
    }
    // An unchanged declaration is accepted again.
    assert!(
        registry
            .register_lookup(LookupProvider::new(
                LookupDeclaration::new(
                    "fixture",
                    "agent",
                    "agentId",
                    "@fixture#Agent",
                    "@fixture#AgentId"
                ),
                |_| None,
            ))
            .is_ok()
    );
}

/// A lookup or Context key with an invalid wire field is refused at registration.
#[test]
fn invalid_provider_declarations_are_refused() {
    let mut registry = TypertRegistry::new();
    let error = registry
        .register_lookup(LookupProvider::new(
            LookupDeclaration::new(
                "bad#key",
                "agent",
                "agentId",
                "@fixture#Agent",
                "@fixture#AgentId",
            ),
            |_| None,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("lookup key"), "{error}");

    let error = registry
        .register_lookup(LookupProvider::new(
            LookupDeclaration::new(
                "fixture",
                "agent",
                "agent/id",
                "@fixture#Agent",
                "@fixture#AgentId",
            ),
            |_| None,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("lookup wire field"), "{error}");

    let error = registry
        .register_lookup(LookupProvider::new(
            LookupDeclaration::new("fixture", "agent", "agentId", "", "@fixture#AgentId"),
            |_| None,
        ))
        .unwrap_err();
    assert!(matches!(error, TypertError::Empty { .. }), "{error}");

    let error = registry
        .register_host_context(
            "fixture",
            HostContextAdapter::new("agent/id", "@fixture#Id", |_| None),
        )
        .unwrap_err();
    assert!(error.to_string().contains("Context wire field"), "{error}");
}

/// A change is published on both edges, duplicate providers are refused, and a disposed
/// subscription observes nothing.
#[test]
fn provider_changes_publish_and_subscriptions_dispose() {
    let mut registry = TypertRegistry::new();
    let changes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let lookup_sink = Arc::clone(&changes);
    let context_sink = Arc::clone(&changes);
    let lookup_subscription = registry.subscribe(Arc::new(move |change: &RegistryChange| {
        if matches!(change.kind, ChangeKind::Lookup) {
            lookup_sink.lock().unwrap().push(change.to_string());
        }
    }));
    let context_subscription = registry.subscribe(Arc::new(move |change: &RegistryChange| {
        if matches!(
            change.kind,
            ChangeKind::HostContext | ChangeKind::ClientContext
        ) {
            context_sink.lock().unwrap().push(change.to_string());
        }
    }));

    let declaration = || {
        LookupDeclaration::new(
            "fixture",
            "agent",
            "agentId",
            "@fixture#Agent",
            "@fixture#AgentId",
        )
    };
    let lookup_id = registry
        .register_lookup(LookupProvider::new(declaration(), |_| None))
        .unwrap();
    let host_id = registry
        .register_host_context(
            "fixture",
            HostContextAdapter::new("agentId", "@fixture#AgentId", |_| None),
        )
        .unwrap();
    let client_id = registry
        .register_client_context("fixture", ClientContextAdapter::new(|_| None, |_| None))
        .unwrap();

    let error = registry
        .register_lookup(LookupProvider::new(declaration(), |_| None))
        .unwrap_err();
    assert!(error.to_string().contains("already registered"), "{error}");
    let error = registry
        .register_host_context(
            "fixture",
            HostContextAdapter::new("agentId", "@x#Y", |_| None),
        )
        .unwrap_err();
    assert!(error.to_string().contains("already registered"), "{error}");
    let error = registry
        .register_client_context("fixture", ClientContextAdapter::new(|_| None, |_| None))
        .unwrap_err();
    assert!(error.to_string().contains("already registered"), "{error}");

    registry.withdraw_lookup(&lookup_id);
    registry.withdraw_context(&host_id);
    registry.withdraw_context(&client_id);
    assert_eq!(
        *changes.lock().unwrap(),
        vec![
            "lookup:fixture".to_owned(),
            "host-context:fixture".to_owned(),
            "client-context:fixture".to_owned(),
            "lookup:fixture".to_owned(),
            "host-context:fixture".to_owned(),
            "client-context:fixture".to_owned(),
        ]
    );

    registry.unsubscribe(lookup_subscription);
    registry.unsubscribe(context_subscription);
    registry
        .register_lookup(LookupProvider::new(declaration(), |_| None))
        .unwrap();
    assert_eq!(
        changes.lock().unwrap().len(),
        6,
        "a disposed subscription sees nothing"
    );
}

// ---- resources ----------------------------------------------------------------------------------

/// One default extraction per kind, withdrawn with its id, and `global` is reserved.
#[test]
fn one_default_extraction_per_kind_is_registered_and_withdrawn() {
    let mut registry = TypertRegistry::new();
    let changes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&changes);
    registry.subscribe(Arc::new(move |change: &RegistryChange| {
        if change.kind == ChangeKind::Resource {
            sink.lock().unwrap().push(change.to_string());
        }
    }));

    let id = registry
        .register_resource("fixture", ResourceDeclaration::field("fixtureId"))
        .unwrap();
    assert_eq!(
        registry.resource("fixture"),
        Some(&ResourceDeclaration::field("fixtureId")),
        "the field declaration is preserved as declared"
    );
    assert_eq!(registry.resource_entries().len(), 1);
    assert_eq!(registry.resource_entries()[0].kind, "fixture");
    let error = registry
        .register_resource("fixture", ResourceDeclaration::field("otherId"))
        .unwrap_err();
    assert!(matches!(error, TypertError::ResourceRegistered { .. }));
    let error = registry
        .register_resource("other", ResourceDeclaration::field(""))
        .unwrap_err();
    assert!(matches!(error, TypertError::Segment { .. }), "{error}");
    let error = registry
        .register_resource("bad#kind", ResourceDeclaration::field("id"))
        .unwrap_err();
    assert!(matches!(error, TypertError::Key { .. }), "{error}");

    registry.withdraw_resource(&id);
    assert!(registry.resource("fixture").is_none());
    assert!(registry.resource_entries().is_empty());
    assert_eq!(
        *changes.lock().unwrap(),
        vec!["resource:fixture".to_owned(), "resource:fixture".to_owned()]
    );

    let reserved = registry
        .register_resource(GLOBAL_RESOURCE_KIND, ResourceDeclaration::field("globalId"))
        .unwrap_err();
    assert!(matches!(reserved, TypertError::GlobalReserved));
    assert!(
        reserved
            .to_string()
            .contains("reserved for the built-in global resource")
    );
    assert!(registry.resource(GLOBAL_RESOURCE_KIND).is_none());
    assert_eq!(
        changes.lock().unwrap().len(),
        2,
        "a refused registration publishes nothing"
    );
}

/// A resolver-based declaration is preserved and computes the identity from the whole call.
#[test]
fn a_resolver_declaration_computes_the_identity_from_the_call() {
    let mut registry = TypertRegistry::new();
    let declaration = ResourceDeclaration::resolver(|input: &ResourceResolveInput| {
        match input.value("agentId") {
            Some(Value::String(id)) => ResourceIdentity::Identity(id.clone()),
            _ => ResourceIdentity::Absent,
        }
    });
    let id = registry.register_resource("fixture", declaration).unwrap();
    let registered = registry.resource("fixture").unwrap();

    let mut args = BTreeMap::new();
    args.insert("agentId".to_owned(), json!("agent-1"));
    let input = ResourceResolveInput::new(args, vec![("agent".to_owned(), "agentId".to_owned())]);
    let identity = registered.extract(&input);
    assert_eq!(identity.single(), Some("agent-1"));

    registry.withdraw_resource(&id);
    assert!(registry.resource("fixture").is_none());
}

/// A field declaration extracts nothing when the field is absent, and a many-identity answer is not
/// silently reduced to the first.
#[test]
fn resource_extraction_reports_absence_and_multiplicity() {
    let declaration = ResourceDeclaration::field("sessionId");
    let mut args = BTreeMap::new();
    assert!(
        declaration
            .extract(&ResourceResolveInput::new(args.clone(), Vec::new()))
            .is_absent()
    );

    args.insert("sessionId".to_owned(), Value::Null);
    assert!(
        declaration
            .extract(&ResourceResolveInput::new(args.clone(), Vec::new()))
            .is_absent()
    );

    args.insert("sessionId".to_owned(), json!("session-1"));
    let input = ResourceResolveInput::new(args, Vec::new());
    assert_eq!(declaration.extract(&input).single(), Some("session-1"));

    let many = ResourceDeclaration::resolver(|_| {
        ResourceIdentity::Many(vec!["a".to_owned(), "b".to_owned()])
    });
    let input = ResourceResolveInput::default();
    assert_eq!(many.extract(&input).single(), None);
    assert!(!many.extract(&input).is_absent());
}

// ---- validation ---------------------------------------------------------------------------------

/// Every invocation boundary upstream validates is validated here.
#[test]
fn every_invocation_boundary_is_validated() {
    let strict = || {
        InvocationDescriptor::new("@fixture/remote#strict", "session", "session", "strict")
            .with_parameter(InvocationParameter::json("request"))
            .implementing("remoteExportCreate")
            .with_cancellation()
            .returning("@fixture#Value")
    };
    let mut registry = TypertRegistry::new();
    let id = registry
        .mount_remote(&RemoteContribution::new("@fixture/strict", vec![strict()]))
        .unwrap();
    registry.unmount_remote(id);

    let streaming = strict().streaming().with_uplink("@fixture#Item");
    let id = registry
        .mount_remote(&RemoteContribution::new("@fixture/uplink", vec![streaming]))
        .unwrap();
    registry.unmount_remote(id);

    let cases: Vec<(InvocationDescriptor, &str)> = vec![
        (
            InvocationDescriptor::new("", "session", "session", "page").returning("@f#V"),
            "invocation id",
        ),
        (
            InvocationDescriptor::new("@f#x", "session", "session", "uplink-broken")
                .streaming()
                .with_uplink("")
                .returning("@f#V"),
            "uplink type symbol",
        ),
        (
            InvocationDescriptor::new("@f#x", "session", "bad/name", "page").returning("@f#V"),
            "namespace",
        ),
        (
            InvocationDescriptor::new("@f#x", "session", "session", "page")
                .implementing("bad/name")
                .returning("@f#V"),
            "implementation method",
        ),
        (
            // The cancellation parameter is the reserved final `signal`; a descriptor that invented
            // another name would generate a call the Host never receives.
            strict().with_cancellation_parameter("abort"),
            "cancellation parameter",
        ),
        (
            strict().with_parameter(InvocationParameter::json("other").on_wire("request")),
            "repeats wire field",
        ),
        (
            InvocationDescriptor::new("@f#x", "session", "session", "page")
                .with_parameter(InvocationParameter::lookup("agent", "agentId", ""))
                .returning("@f#V"),
            "has no lookup key",
        ),
        (
            InvocationDescriptor::new("@f#x", "session", "session", "page")
                .with_parameter(
                    InvocationParameter::lookup("agent", "agentId", "fixture")
                        .accepting_undefined(),
                )
                .returning("@f#V"),
            "cannot accept undefined",
        ),
        (
            InvocationDescriptor::new("@f#x", "session", "session", "page")
                .on_context("fixture", "request")
                .with_parameter(InvocationParameter::json("request"))
                .returning("@f#V"),
            "repeats wire field",
        ),
        (
            InvocationDescriptor::new("@f#x", "session", "session", "page").returning(""),
            "type symbol",
        ),
        (
            InvocationDescriptor::new("@f#x", "bad#service", "session", "page").returning("@f#V"),
            "invocation service key",
        ),
    ];
    for (index, (descriptor, message)) in cases.into_iter().enumerate() {
        let error = registry
            .mount_remote(&RemoteContribution::new(
                format!("@fixture/malformed-{index}"),
                vec![descriptor],
            ))
            .unwrap_err();
        assert!(
            error.to_string().contains(message),
            "expected {message:?}, got {error}"
        );
    }
    assert!(registry.remote_invocations().is_empty());
}

/// A Context receiver's identity field replaces a parameter rather than duplicating one, and the
/// required wire fields keep declaration order with the identity last.
#[test]
fn a_context_receiver_supplies_the_last_wire_field() {
    let descriptor = InvocationDescriptor::new("@f#x", "session", "session", "page")
        .with_parameter(InvocationParameter::json("request"))
        .with_parameter(InvocationParameter::lookup("agent", "agentId", "fixture"))
        .on_context("fixture", "scopeId")
        .returning("@f#V");
    validate_invocation(&descriptor).unwrap();
    assert_eq!(
        descriptor.wire_fields(),
        vec!["request", "agentId", "scopeId"]
    );
    assert_eq!(
        descriptor.required_wire_fields(),
        vec!["request", "agentId", "scopeId"]
    );

    let accepting = descriptor
        .parameters
        .iter()
        .map(|parameter| {
            if parameter.name == "request" {
                parameter.clone().accepting_undefined()
            } else {
                parameter.clone()
            }
        })
        .collect::<Vec<_>>();
    let mut accepting_descriptor = descriptor.clone();
    accepting_descriptor.parameters = accepting;
    assert_eq!(
        accepting_descriptor.required_wire_fields(),
        vec!["agentId", "scopeId"],
        "an absent-accepting parameter is not required"
    );
}

// ---- containment --------------------------------------------------------------------------------

/// A panicking change listener is contained: later listeners still run and a warning is recorded.
#[test]
fn a_panicking_observer_is_contained_and_reported() {
    let mut registry = TypertRegistry::new();
    let observed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&observed);
    registry.subscribe_local(Arc::new(|_: &RegistryChange| panic!("observer failed")));
    registry.subscribe_local(Arc::new(move |_: &RegistryChange| {
        counter.fetch_add(1, Ordering::SeqCst);
    }));

    registry.register(tools_contribution()).unwrap();

    assert_eq!(observed.load(Ordering::SeqCst), 1);
    let warnings = registry.take_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("local:session/page"));
    assert!(warnings[0].contains("observer failed"));
    assert!(registry.take_warnings().is_empty(), "warnings drain");
}

// ---- protocol predicates ------------------------------------------------------------------------

/// The `RemoteError` marker survives the wire, and only the marker plus a string code identifies one.
#[test]
fn the_remote_error_marker_identifies_a_failure_across_the_wire() {
    let error = RemoteError::bad_request("invalid arguments", &[json!({ "path": "limit" })]);
    let wire = error.to_json();
    assert_eq!(wire["isDSHRemoteError"], json!(true));
    assert_eq!(wire["name"], json!("RemoteError"));

    let decoded = remote_error_of(&wire).expect("the marker identifies it");
    assert_eq!(decoded.code, "gateway/bad-request");
    assert_eq!(decoded.parsed_code(), RemoteErrorCode::BadRequest);
    assert_eq!(decoded.issues(), Some(vec![json!({ "path": "limit" })]));

    assert!(remote_error_of(&json!({ "code": "gateway/internal" })).is_none());
    assert!(
        remote_error_of(&json!({ "isDSHRemoteError": true, "message": "no code" })).is_none(),
        "a failure without a code is not identifiable"
    );
    assert!(
        remote_error_of(&json!({ "isDSHRemoteError": false, "code": "gateway/internal" }))
            .is_none()
    );
    assert!(remote_error_of(&json!(null)).is_none());
    assert!(remote_error_of(&json!("gateway/internal")).is_none());
}

/// The three universal codes are the floor of the vocabulary, and an unknown code round-trips.
#[test]
fn the_error_vocabulary_keeps_unknown_codes_representable() {
    assert_eq!(
        UNIVERSAL_ERROR_CODES,
        [
            "gateway/bad-request",
            "gateway/cancelled",
            "gateway/internal"
        ]
    );
    for code in UNIVERSAL_ERROR_CODES {
        let parsed = RemoteErrorCode::parse(code);
        assert!(parsed.is_universal());
        assert_eq!(parsed.as_str(), code);
    }
    let domain = RemoteErrorCode::parse("team/task-rejected");
    assert!(!domain.is_universal());
    assert_eq!(domain.as_str(), "team/task-rejected");
    assert_eq!(
        domain,
        RemoteErrorCode::Other("team/task-rejected".to_owned())
    );

    let error = RemoteError::new("team/task-rejected", "task refused", json!({}));
    assert_eq!(
        error.issues(),
        None,
        "only gateway/bad-request declares issues"
    );

    let malformed = RemoteError::new(
        "gateway/bad-request",
        "invalid arguments",
        json!({ "issues": "not a list" }),
    );
    assert_eq!(
        malformed.issues(),
        None,
        "a malformed payload is reported, not defaulted"
    );
    let empty = RemoteError::new("gateway/bad-request", "invalid", json!({}));
    assert_eq!(empty.issues(), Some(Vec::new()));
}

/// The owner is not recorded as a caller principal; anyone else is.
#[test]
fn the_owner_has_no_principal_and_everyone_else_does() {
    assert_eq!(principal_of(None), None);
    assert_eq!(principal_of(Some(&json!({ "kind": "owner" }))), None);
    let peer = json!({ "kind": "peer", "id": "peer-1" });
    assert_eq!(principal_of(Some(&peer)), Some(&peer));
    assert_eq!(
        principal_of(Some(&json!("unexpected"))),
        Some(&json!("unexpected")),
        "an unknown principal shape is passed through to the authorization layer"
    );
}

/// A streaming descriptor declares its mode; a unary one keeps the default.
#[test]
fn a_streaming_descriptor_declares_its_mode_and_uplink() {
    let streaming = page().streaming().with_uplink("@f#Item");
    assert_eq!(streaming.mode, Mode::Stream);
    assert_eq!(streaming.uplink.as_deref(), Some("@f#Item"));
    assert_eq!(page().mode, Mode::Unary);
    assert_eq!(page().uplink, None);
    assert!(matches!(page().receiver, Receiver::Direct));
}
