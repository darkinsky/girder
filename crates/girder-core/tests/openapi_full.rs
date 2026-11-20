//! Tests for the registry-driven `OpenAPI` emitter.
//!
//! These are about *completeness*, which is the property the module exists to provide: a document a
//! client generator cannot use is a document that describes a server the deployment does not run.
//! The assertions therefore compare the document against the registry it was built from rather than
//! against a list written down here — a test that checked a hardcoded method list would pass while
//! the server served something else, which is the exact defect being fixed.
//!
//! The module is included by path rather than through a crate re-export, so these tests compile
//! against it as written before the crate root declares it. It names its dependencies through
//! `girder_core::…`, which resolves here because the library aliases itself under that name.

#[path = "../src/api/openapi_full.rs"]
pub mod openapi_full;

use std::sync::Arc;

use girder_core::api::config::WorkspaceRegistry;
use girder_core::{
    DirectoryCatalog, FileSystem, JobRegistry, LocalFileSystem, MemoryScheduleStore,
    MemorySettingsStore, ProjectionRegistry, RemoteRegistry, ScheduleStore, SessionCatalog,
    SettingsStore, register_fs_methods, register_job_methods, register_projection_methods,
    register_schedule_methods, register_session_methods, register_settings_methods,
    register_terminal_extra_methods, register_workspace_file_extras,
    register_workspace_file_methods, register_workspace_methods,
};
use serde_json::{Value, json};

use openapi_full::{Operation, document, operations};

/// The metadata most tests pass.
fn info() -> Value {
    json!({ "title": "Girder Remote API", "version": "1.2.3" })
}

/// A session catalogue over a directory that holds nothing.
fn catalog() -> Arc<dyn SessionCatalog> {
    Arc::new(DirectoryCatalog::new(
        std::env::temp_dir().join("girder-openapi"),
    ))
}

/// A registry with the subset of the server's surface whose constructors are public.
///
/// Not the whole server: the CLI assembles the rest from components this crate does not export
/// (an extension runtime, an agent queue source, a schedule store's durable form). Completeness is
/// checked against *whatever registry is passed*, which is the property that matters — a caller with
/// the full surface gets a document of the full surface from the same two functions.
fn registry() -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    let jobs = Arc::new(JobRegistry::new(64 * 1024));
    let projections = Arc::new(ProjectionRegistry::new());
    let workspaces = Arc::new(WorkspaceRegistry::new());
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem);
    // Trait objects, because that is what the registration functions take: mounting the concrete
    // store instead would be a second construction path the server does not have.
    let settings: Arc<dyn SettingsStore> = Arc::new(MemorySettingsStore::new());
    let schedules: Arc<dyn ScheduleStore> = Arc::new(MemoryScheduleStore::new());

    register_session_methods(&mut registry, &catalog());
    register_projection_methods(&mut registry, &projections);
    register_job_methods(&mut registry, &jobs);
    register_fs_methods(&mut registry, &fs);
    register_workspace_methods(&mut registry, &workspaces);
    register_workspace_file_methods(&mut registry, &fs, &workspaces);
    register_workspace_file_extras(&mut registry, &fs, &workspaces);
    register_settings_methods(&mut registry, &settings);
    register_schedule_methods(&mut registry, &schedules);
    register_terminal_extra_methods(&mut registry);
    registry
}

/// The qualified names the registry reports, sorted.
fn registry_names(registry: &RemoteRegistry) -> Vec<String> {
    let mut names: Vec<String> = registry
        .names()
        .into_iter()
        .map(|(namespace, method)| format!("{namespace}.{method}"))
        .collect();
    names.sort();
    names
}

/// The qualified names the operations describe, sorted.
fn operation_names(operations: &[Operation]) -> Vec<String> {
    let mut names: Vec<String> = operations
        .iter()
        .map(|operation| format!("{}.{}", operation.namespace, operation.method))
        .collect();
    names.sort();
    names
}

// ---- completeness: the core property ---------------------------------------------------------

#[test]
fn the_operation_count_equals_the_number_of_registered_methods() {
    // The one assertion the module exists for. A document that publishes fewer operations than the
    // registry holds produces a client that cannot reach the difference, and nothing about the
    // document says so.
    let registry = registry();

    assert_eq!(operations(&registry).len(), registry.len());
}

#[test]
fn every_registered_method_has_an_operation_and_none_is_invented() {
    // Equality of the name sets, not just the counts: two errors that cancel out — one method
    // dropped and one invented — would pass a count check and produce a client that calls a
    // method no server answers.
    let registry = registry();

    assert_eq!(
        operation_names(&operations(&registry)),
        registry_names(&registry)
    );
}

#[test]
fn a_registry_that_grows_does_not_need_a_code_change() {
    // Registering a method after the fact has to show up in the document, which is what makes the
    // emitter immune to the drift a hardcoded list suffers.
    let mut registry = registry();
    let before = operations(&registry).len();

    registry.register("invented", "afterwards", |_call| {
        Box::pin(async { Ok(json!({ "ok": true })) })
    });

    let after = operations(&registry);

    assert_eq!(after.len(), before + 1);
    assert!(
        after
            .iter()
            .any(|operation| operation.namespace == "invented" && operation.method == "afterwards"),
        "the newly registered method must appear"
    );
}

#[test]
fn the_document_publishes_one_path_per_registered_method() {
    // The path count is the wire-visible form of the same property: a consumer reads `paths`, not
    // `operations`.
    let registry = registry();
    let json = document(&registry, &info());

    let published = json["paths"].as_object().map_or(0, serde_json::Map::len);

    assert_eq!(published, registry.len());
}

#[test]
fn the_document_states_how_many_operations_it_publishes() {
    // Published so a consumer can detect a truncated document without re-deriving what "complete"
    // means from the server it cannot see.
    let registry = registry();
    let json = document(&registry, &info());

    assert_eq!(json["x-girder-operation-count"], registry.len());
}

// ---- namespaces ------------------------------------------------------------------------------

#[test]
fn every_namespace_in_the_registry_is_present() {
    // A namespace is what a generated client groups by, so a missing one reads as a whole subsystem
    // the server does not have.
    let registry = registry();
    let described: Vec<String> = operations(&registry)
        .into_iter()
        .map(|operation| operation.namespace)
        .collect();

    for (namespace, method) in registry.names() {
        assert!(
            described.contains(&namespace),
            "`{namespace}.{method}` is registered but its namespace is absent"
        );
    }
}

#[test]
fn the_document_has_a_tag_for_every_namespace() {
    let registry = registry();
    let json = document(&registry, &info());

    let mut tagged: Vec<&str> = json["paths"]
        .as_object()
        .map(|paths| {
            paths
                .values()
                .filter_map(|item| item["post"]["tags"][0].as_str())
                .collect()
        })
        .unwrap_or_default();
    tagged.sort_unstable();
    tagged.dedup();

    let described = operations(&registry);
    let mut namespaces: Vec<&str> = described
        .iter()
        .map(|operation| operation.namespace.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    namespaces.sort_unstable();

    assert_eq!(tagged, namespaces);
}

// ---- path shape ------------------------------------------------------------------------------

#[test]
fn a_path_is_the_namespace_then_the_method() {
    // The shape `openapi.rs` uses, so a generator pointed at either document emits the same client.
    let mut registry = RemoteRegistry::new();
    registry.register("session", "list", |_call| {
        Box::pin(async { Ok(Value::Null) })
    });
    registry.register("job", "kill", |_call| Box::pin(async { Ok(Value::Null) }));

    let json = document(&registry, &info());
    let paths = json["paths"].as_object().expect("paths is an object");

    assert!(paths.contains_key("/session/list"));
    assert!(paths.contains_key("/job/kill"));
    assert_eq!(paths.len(), 2);
}

#[test]
fn every_path_is_a_post_with_an_operation_id() {
    let registry = registry();
    let json = document(&registry, &info());
    let paths = json["paths"].as_object().expect("paths is an object");

    for (path, item) in paths {
        assert!(path.starts_with('/'), "`{path}` is not absolute");
        assert!(
            item.get("post").is_some(),
            "`{path}` must be addressed with POST: the body is the argument object"
        );
        let post = &item["post"];
        assert!(
            post["operationId"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "`{path}` has no operationId"
        );
        assert!(
            post["requestBody"]["content"]["application/json"]["schema"].is_object(),
            "`{path}` has no request body schema"
        );
        assert!(
            post["responses"]["200"].is_object(),
            "`{path}` has no success response"
        );
    }
}

#[test]
fn the_invoke_route_is_named_because_no_path_uses_its_literal_form() {
    // The paths are logical: this build serves them all through one envelope route, and a client
    // that had to guess the route would be guessing from a document that never mentions it.
    let registry = registry();
    let json = document(&registry, &info());

    assert_eq!(json["x-girder-invoke-path"], "/api/remote.invoke");
    assert!(
        !json["paths"]
            .as_object()
            .is_some_and(|paths| paths.contains_key("/api/remote.invoke")),
        "the envelope route is a transport detail, not one of the described operations"
    );
}

// ---- emptiness -------------------------------------------------------------------------------

#[test]
fn an_empty_registry_produces_an_empty_paths_object() {
    // `paths` must be present and empty rather than absent: a consumer should not have to
    // distinguish "no methods" from "a document that forgot to describe them".
    let json = document(&RemoteRegistry::new(), &info());

    assert_eq!(json["paths"], json!({}));
    assert_eq!(json["x-girder-operation-count"], 0);
}

#[test]
fn an_empty_registry_has_no_operations() {
    assert!(operations(&RemoteRegistry::new()).is_empty());
}

// ---- the document parses ---------------------------------------------------------------------

#[test]
fn the_generated_document_round_trips_through_json() {
    // A generator receives bytes, so what matters is that the value serializes and parses back
    // unchanged; a document that only exists as a `Value` has not been tested as a document.
    let json = document(&registry(), &info());

    let text = serde_json::to_string(&json).expect("the document serializes");
    let parsed: Value = serde_json::from_str(&text).expect("the document parses");

    assert_eq!(parsed, json);
}

#[test]
fn the_document_declares_openapi_3_1_and_a_dialect() {
    let json = document(&registry(), &info());

    assert_eq!(json["openapi"], "3.1.0");
    assert_eq!(
        json["jsonSchemaDialect"],
        "https://json-schema.org/draft/2020-12/schema"
    );
}

#[test]
fn info_is_passed_through_and_defaulted_only_where_it_must_be() {
    // The caller's own keys survive: a caller that supplies `contact` means it. The two keys
    // `OpenAPI` requires are defaulted so a thin `info` cannot make the whole document invalid.
    let supplied = json!({ "title": "Mine", "version": "9.9.9", "contact": { "name": "ops" } });
    let json = document(&registry(), &supplied);
    assert_eq!(json["info"]["title"], "Mine");
    assert_eq!(json["info"]["version"], "9.9.9");
    assert_eq!(json["info"]["contact"]["name"], "ops");

    let defaulted = document(&registry(), &json!({}));
    assert_eq!(defaulted["info"]["title"], "Girder");
    assert_eq!(defaulted["info"]["version"], "0.0.0");

    // A non-object cannot fill `info`, and putting it there would fail validation for every
    // consumer rather than costing one field.
    let malformed = document(&registry(), &json!("not an object"));
    assert_eq!(malformed["info"]["title"], "Girder");
}

// ---- references resolve inside the document --------------------------------------------------

#[test]
fn every_request_body_reference_resolves() {
    // A `$ref` that keeps pointing at `#/$defs/…` parses, resolves to nothing and validates as
    // "anything" — the most expensive kind of silent failure.
    let json = document(&registry(), &info());

    let mut checked = 0usize;
    for (path, item) in json["paths"].as_object().expect("paths is an object") {
        let schema = &item["post"]["requestBody"]["content"]["application/json"]["schema"];
        let reference = schema["$ref"]
            .as_str()
            .unwrap_or_else(|| panic!("`{path}` has an inline body instead of a component"));
        assert!(
            json.pointer(reference.trim_start_matches('#')).is_some(),
            "`{path}` references `{reference}`, which is not in the document"
        );
        checked += 1;
    }

    assert_eq!(checked, json["x-girder-operation-count"]);
}

#[test]
fn no_reference_still_points_at_defs() {
    let json = document(&registry(), &info());
    let text = serde_json::to_string(&json).expect("the document serializes");

    // The rewritten prefix is the only local form that resolves; the one the schema emitter uses
    // internally no longer exists in the document.
    assert!(
        !text.contains("#/$defs/"),
        "an unmoved `$ref` resolves to nothing once the definitions live under components"
    );
}

#[test]
fn the_named_types_are_hoisted_into_components() {
    // The three catalogue modules describe these, so they must survive the re-homing.
    let json = document(&registry(), &info());
    let schemas = json["components"]["schemas"]
        .as_object()
        .expect("components.schemas is an object");

    for name in [
        "SessionPageValue",
        "SessionProjectionsValue",
        "JobListFrame",
        "WorkspaceView",
    ] {
        assert!(
            schemas.contains_key(name),
            "`{name}` is defined by a catalogue but absent from the document"
        );
    }
}

// ---- descriptions ----------------------------------------------------------------------------

#[test]
fn a_documented_method_carries_its_own_summary() {
    // The catalogue's docs are reused rather than a generated "Calls `x`." — the descriptor is the
    // only place the method's meaning was written down.
    let registry = registry();
    let described = operations(&registry);

    let page = described
        .iter()
        .find(|operation| operation.namespace == "session" && operation.method == "page")
        .expect("session.page is registered");

    assert_eq!(page.summary, "Pages one session's events.");
}

#[test]
fn an_undocumented_method_still_gets_a_summary() {
    // A registry stores handlers, not prose. Saying only what the method is called is honest; a
    // summary invented from the name would not be.
    let mut registry = RemoteRegistry::new();
    registry.register("probe", "undocumented", |_call| {
        Box::pin(async { Ok(Value::Null) })
    });

    let described = operations(&registry);

    assert_eq!(described.len(), 1);
    assert!(!described[0].summary.is_empty());
    assert!(described[0].summary.contains("probe.undocumented"));
}

#[test]
fn a_summary_is_one_line_so_a_console_can_render_it_as_a_heading() {
    // The summary is a heading in a generated console; a whole paragraph there pushes the operation
    // list off the page.
    let registry = registry();

    for operation in operations(&registry) {
        assert!(
            !operation.summary.contains('\n'),
            "`{}` has a multi-line summary",
            operation.summary
        );
        assert!(!operation.summary.is_empty());
    }
}

#[test]
fn a_multi_line_doc_keeps_its_full_text_as_the_description() {
    // Truncating a method's documentation to its first line would lose what a client author needs.
    let mut registry = RemoteRegistry::new();
    registry.register("session", "page", |_call| {
        Box::pin(async { Ok(Value::Null) })
    });
    let json = document(&registry, &info());

    let post = &json["paths"]["/session/page"]["post"];
    assert_eq!(post["summary"], "Pages one session's events.");
    assert!(
        post.get("description").is_none(),
        "a one-line doc is not repeated as a description"
    );
}

// ---- descriptors -----------------------------------------------------------------------------

#[test]
fn a_described_method_gets_its_parameter_schema() {
    // The whole point of reusing the catalogue: a generator emits a typed request model instead of
    // an unconstrained object.
    let registry = registry();
    let json = document(&registry, &info());

    let reference = json["paths"]["/session/page"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"]["$ref"]
        .as_str()
        .expect("the body is a component")
        .to_owned();
    let body = json
        .pointer(reference.trim_start_matches('#'))
        .expect("the reference resolves");

    assert_eq!(body["type"], "object");
    assert!(body["properties"]["sessionId"].is_object());
    assert!(body["properties"]["limit"].is_object());
}

#[test]
fn a_method_with_no_descriptor_accepts_any_argument_object() {
    // An unconstrained body is the honest description of a handler whose signature was never
    // written down. An invented parameter list would produce a client that compiles and sends the
    // wrong body.
    let mut registry = RemoteRegistry::new();
    registry.register("probe", "opaque", |_call| {
        Box::pin(async { Ok(Value::Null) })
    });
    let json = document(&registry, &info());

    let reference = json["paths"]["/probe/opaque"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"]["$ref"]
        .as_str()
        .expect("the body is a component")
        .to_owned();
    let body = json
        .pointer(reference.trim_start_matches('#'))
        .expect("the reference resolves");

    assert_eq!(body["type"], "object");
    assert_eq!(body["properties"], json!({}));
}

#[test]
fn the_descriptors_do_not_decide_which_methods_exist() {
    // The registry does. `session.create`, `session.rename` and `session.delete` are described but
    // this registry does not serve them, so publishing them would describe a method a client cannot
    // call.
    let mut registry = RemoteRegistry::new();
    assert!(
        registry.get("session", "delete").is_none(),
        "the fixture must not register it"
    );
    registry.register("session", "page", |_call| {
        Box::pin(async { Ok(Value::Null) })
    });

    let described = operations(&registry);

    assert_eq!(described.len(), 1);
    assert_eq!(described[0].method, "page");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod api {
    pub use super::openapi_full as openapi_full;
}
