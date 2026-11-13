//! Tests for the `OpenAPI` emitter.
//!
//! The assertions are about a *document contract*, so they check what a consumer reads rather than
//! how the emitter is written: the version string decides which schema dialect a tool applies, the
//! path shape decides which operation a generated client calls, and a `$ref` either resolves inside
//! the document or silently validates as "anything". The two that would otherwise ship broken are
//! the version (3.0 rejects the `"type": [..., "null"]` unions this crate emits) and the reference
//! prefix (`#/$defs/…` no longer exists once the definitions move to `components.schemas`).
//!
//! The module is included by path rather than through a crate re-export, so these tests compile
//! against it as written before the crate root declares it. It names its dependencies through
//! `girder_core::…`, which resolves here because the library aliases itself under that name.

#[path = "../src/openapi.rs"]
mod openapi;

use girder_core::{Catalogue, Field, Method, ObjectSchema, Parameter, Schema};
use serde_json::Value;

use openapi::{Info, document};

/// Builds the metadata most tests use.
fn info() -> Info {
    Info::new("Girder Remote API", "1.0.0")
}

/// Follows a local `$ref` in `document` and returns what it points at.
///
/// Only the local form is handled, which is the only form the emitter produces: a reference that is
/// not local would be a contract with a document this test never sees.
fn resolve<'a>(document: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    document.pointer(pointer)
}

// ---- version and metadata ---------------------------------------------------------------------

#[test]
fn the_document_declares_openapi_3_1() {
    // 3.0 is not an alternative: its Schema Object admits one string in `type`, and this crate
    // renders a nullable value as `"type": ["string", "null"]`, which a 3.0 consumer rejects.
    let json = document(&Catalogue::new(1), info());

    assert_eq!(json["openapi"], "3.1.0");
}

#[test]
fn the_schema_dialect_is_named_explicitly() {
    // Without this key a consumer falls back to the OpenAPI base dialect, which is 2020-12 *plus*
    // keywords the embedded schemas are not written against.
    let json = document(&Catalogue::new(1), info());

    assert_eq!(
        json["jsonSchemaDialect"],
        "https://json-schema.org/draft/2020-12/schema"
    );
}

#[test]
fn info_carries_the_title_and_version() {
    let json = document(&Catalogue::new(1), Info::new("Girder Remote API", "2.3.4"));

    assert_eq!(json["info"]["title"], "Girder Remote API");
    assert_eq!(json["info"]["version"], "2.3.4");
    assert!(
        json["info"].get("description").is_none(),
        "an absent description is omitted, not sent as null"
    );
}

#[test]
fn info_description_is_kept_when_supplied() {
    let json = document(
        &Catalogue::new(1),
        info().with_description("Every method a Girder server serves."),
    );

    assert_eq!(
        json["info"]["description"],
        "Every method a Girder server serves."
    );
}

#[test]
fn the_wire_revision_travels_beside_the_api_version() {
    // `info.version` is the API version the caller advertises; the wire revision is a different
    // fact and is only readable where the schema emitter put it.
    let json = document(&Catalogue::new(7), info());

    assert_eq!(json["x-girder-revision"], 7);
}

// ---- the empty catalogue ----------------------------------------------------------------------

#[test]
fn an_empty_catalogue_is_still_a_valid_document() {
    let json = document(&Catalogue::new(0), info());

    assert_eq!(json["openapi"], "3.1.0");
    assert_eq!(json["paths"], serde_json::json!({}));
    assert_eq!(json["components"]["schemas"], serde_json::json!({}));
}

#[test]
fn an_empty_catalogue_keeps_the_paths_key() {
    // `paths` is optional in 3.1, but omitting it would make a consumer distinguish "no methods"
    // from "a document that forgot to describe them".
    let json = document(&Catalogue::new(0), info());

    assert!(json.get("paths").is_some());
}

// ---- one method -------------------------------------------------------------------------------

/// A catalogue with one `session.list` method and one named type.
fn one_method() -> Catalogue {
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "SessionSummary",
        vec![Field::required("id", Schema::String)],
    ));
    catalogue.add(
        Method::new("session", "list")
            .with_parameter(Parameter::required("sessionId", Schema::String))
            .with_parameter(Parameter::optional("limit", Schema::Int64))
            .with_docs("Lists sessions.")
            .returning(Schema::Array(Box::new(Schema::Ref(
                "SessionSummary".to_owned(),
            )))),
    );
    catalogue
}

#[test]
fn a_dotted_method_becomes_a_slash_path() {
    let json = document(&one_method(), info());

    let paths = json["paths"]
        .as_object()
        .map(|paths| paths.keys().cloned().collect::<Vec<_>>());

    assert_eq!(paths, Some(vec!["/session/list".to_owned()]));
}

#[test]
fn every_method_is_published_as_a_post() {
    // `POST` for a read too: the arguments are one JSON object, and a `GET` would have to flatten
    // them into query parameters, losing nested shapes, arrays and `null`.
    let json = document(&one_method(), info());

    assert!(json["paths"]["/session/list"].get("post").is_some());
}

#[test]
fn the_operation_id_is_the_qualified_method_name() {
    let json = document(&one_method(), info());

    assert_eq!(
        json["paths"]["/session/list"]["post"]["operationId"],
        "session.list"
    );
}

#[test]
fn the_namespace_becomes_the_tag() {
    let json = document(&one_method(), info());

    assert_eq!(
        json["paths"]["/session/list"]["post"]["tags"],
        serde_json::json!(["session"])
    );
}

#[test]
fn method_documentation_becomes_the_operation_description() {
    let json = document(&one_method(), info());

    assert_eq!(
        json["paths"]["/session/list"]["post"]["description"],
        "Lists sessions."
    );
}

#[test]
fn a_parameter_is_described_in_the_request_body() {
    let json = document(&one_method(), info());
    let reference = json["paths"]["/session/list"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"]["$ref"]
        .as_str()
        .map(str::to_owned);
    let body = reference
        .as_deref()
        .and_then(|reference| resolve(&json, reference));

    assert_eq!(
        body.map(|body| &body["properties"]["sessionId"]),
        Some(&serde_json::json!({"type": "string"}))
    );
    assert_eq!(
        body.map(|body| &body["required"]),
        Some(&serde_json::json!(["sessionId"])),
        "only the required parameter is listed"
    );
    assert!(
        body.is_some_and(|body| body["properties"].get("limit").is_some()),
        "an optional parameter is still described"
    );
}

#[test]
fn a_body_is_required_when_a_parameter_is() {
    let json = document(&one_method(), info());

    assert_eq!(
        json["paths"]["/session/list"]["post"]["requestBody"]["required"],
        true
    );
}

#[test]
fn a_body_is_not_required_when_no_parameter_is() {
    let mut catalogue = Catalogue::new(1);
    catalogue.add(
        Method::new("session", "names")
            .with_parameter(Parameter::optional("prefix", Schema::String)),
    );

    let json = document(&catalogue, info());

    assert_eq!(
        json["paths"]["/session/names"]["post"]["requestBody"]["required"],
        false
    );
}

#[test]
fn a_method_with_no_parameters_still_has_a_body_schema() {
    let mut catalogue = Catalogue::new(1);
    catalogue.add(Method::new("session", "count"));

    let json = document(&catalogue, info());
    let reference = json["paths"]["/session/count"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"]["$ref"]
        .as_str()
        .map(str::to_owned);
    let body = reference
        .as_deref()
        .and_then(|reference| resolve(&json, reference));

    assert_eq!(
        body.map(|body| &body["type"]),
        Some(&serde_json::json!("object"))
    );
    assert_eq!(
        json["paths"]["/session/count"]["post"]["requestBody"]["required"],
        false
    );
}

#[test]
fn the_response_describes_the_return_value() {
    let json = document(&one_method(), info());
    let schema = &json["paths"]["/session/list"]["post"]["responses"]["200"]["content"]["application/json"]
        ["schema"];

    assert_eq!(schema["type"], "array");
    assert_eq!(
        schema["items"]["$ref"], "#/components/schemas/SessionSummary",
        "the reference has to point at where the definitions actually live"
    );
}

#[test]
fn a_failure_response_carries_no_schema() {
    // The set of error codes grows with the method set; a schema listing today's codes would
    // reject tomorrow's.
    let json = document(&one_method(), info());

    assert!(json["paths"]["/session/list"]["post"]["responses"]["default"].is_object());
    assert!(
        json["paths"]["/session/list"]["post"]["responses"]["default"]
            .get("content")
            .is_none()
    );
}

// ---- components -------------------------------------------------------------------------------

#[test]
fn a_named_type_lands_in_components_schemas() {
    let json = document(&one_method(), info());

    assert_eq!(
        json["components"]["schemas"]["SessionSummary"]["properties"]["id"],
        serde_json::json!({"type": "string"})
    );
}

#[test]
fn a_named_type_no_method_mentions_is_still_published() {
    // A type is part of the contract whether or not a method body currently exercises it; a client
    // that has to construct one needs its definition.
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "Unused",
        vec![Field::required("id", Schema::String)],
    ));
    catalogue.add(Method::new("session", "noop"));

    let json = document(&catalogue, info());

    assert!(json["components"]["schemas"].get("Unused").is_some());
}

#[test]
fn the_request_body_is_a_component_of_its_own() {
    let json = document(&one_method(), info());

    assert_eq!(
        json["components"]["schemas"]["session.list.Input"]["properties"]["sessionId"],
        serde_json::json!({"type": "string"})
    );
}

#[test]
fn every_reference_in_the_document_resolves() {
    // The failure this guards against is a `$ref` left pointing at `#/$defs/…` after the
    // definitions moved: it parses, resolves to nothing, and validates as "anything".
    let mut catalogue = one_method();
    catalogue.add(
        Method::new("session", "page").with_parameter(Parameter::required(
            "cursor",
            Schema::Ref("SessionSummary".to_owned()),
        )),
    );

    let json = document(&catalogue, info());
    let references = collect_refs(&json);

    assert!(!references.is_empty());
    for reference in references {
        assert!(
            reference.starts_with("#/components/schemas/"),
            "{reference}"
        );
        assert!(
            resolve(&json, &reference).is_some(),
            "unresolved reference {reference}"
        );
    }
}

#[test]
fn no_reference_points_at_the_old_defs_location() {
    let json = document(&one_method(), info());

    assert!(
        !serde_json::to_string(&json)
            .unwrap_or_default()
            .contains("$defs"),
        "the $defs block moved, so nothing may still name it"
    );
}

#[test]
fn a_nullable_parameter_keeps_its_type_union() {
    // The reason this document is 3.1: the union is draft 2020-12 syntax and has no 3.0 spelling.
    let mut catalogue = Catalogue::new(1);
    catalogue.add(
        Method::new("session", "find").with_parameter(Parameter::required(
            "since",
            Schema::Optional(Box::new(Schema::String)),
        )),
    );

    let json = document(&catalogue, info());
    let schema = &json["components"]["schemas"]["session.find.Input"]["properties"]["since"];

    assert_eq!(schema["type"], serde_json::json!(["string", "null"]));
}

#[test]
fn a_recursive_type_is_referenced_rather_than_expanded() {
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "Node",
        vec![
            Field::required("id", Schema::String),
            Field::optional(
                "next",
                Schema::Optional(Box::new(Schema::Ref("Node".to_owned()))),
            ),
        ],
    ));
    catalogue.add(Method::new("tree", "root").returning(Schema::Ref("Node".to_owned())));

    let json = document(&catalogue, info());
    let node = &json["components"]["schemas"]["Node"];

    assert_eq!(
        node["properties"]["next"]["anyOf"][0]["$ref"],
        "#/components/schemas/Node"
    );
    assert_eq!(node["required"], serde_json::json!(["id"]));
}

#[test]
fn a_component_name_needing_pointer_escaping_still_resolves() {
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "a/b",
        vec![Field::required("id", Schema::String)],
    ));
    catalogue.add(
        Method::new("x", "y")
            .with_parameter(Parameter::required("v", Schema::Ref("a/b".to_owned()))),
    );

    let json = document(&catalogue, info());
    let reference = json["components"]["schemas"]["x.y.Input"]["properties"]["v"]["$ref"]
        .as_str()
        .map(str::to_owned);

    assert_eq!(reference.as_deref(), Some("#/components/schemas/a~1b"));
    assert!(
        reference
            .as_deref()
            .and_then(|reference| resolve(&json, reference))
            .is_some()
    );
}

#[test]
fn an_unresolvable_reference_is_reported_rather_than_dropped() {
    let mut catalogue = Catalogue::new(1);
    catalogue.add(
        Method::new("x", "y")
            .with_parameter(Parameter::required("v", Schema::Ref("Missing".to_owned()))),
    );

    let json = document(&catalogue, info());

    assert_eq!(
        json["x-girder-unresolved-refs"],
        serde_json::json!(["Missing"])
    );
}

// ---- several namespaces -----------------------------------------------------------------------

#[test]
fn methods_from_several_namespaces_get_their_own_paths() {
    let mut catalogue = Catalogue::new(1);
    catalogue.add(Method::new("session", "list"));
    catalogue.add(Method::new("session", "get"));
    catalogue.add(Method::new("job", "cancel"));
    catalogue.add(Method::new("workspace", "files"));

    let json = document(&catalogue, info());
    let mut paths: Vec<String> = json["paths"]
        .as_object()
        .map(|paths| paths.keys().cloned().collect())
        .unwrap_or_default();
    paths.sort();

    assert_eq!(
        paths,
        vec![
            "/job/cancel".to_owned(),
            "/session/get".to_owned(),
            "/session/list".to_owned(),
            "/workspace/files".to_owned(),
        ]
    );
    // Two methods in one namespace must not share a tag *or* a path: the path is what a client
    // calls, and a collision would silently merge two operations.
    assert_eq!(
        json["paths"]["/session/get"]["post"]["operationId"],
        "session.get"
    );
    assert_eq!(
        json["paths"]["/job/cancel"]["post"]["tags"],
        serde_json::json!(["job"])
    );
}

#[test]
fn every_method_produces_exactly_one_request_component() {
    let mut catalogue = Catalogue::new(1);
    catalogue.add(Method::new("a", "one"));
    catalogue.add(Method::new("b", "two"));

    let json = document(&catalogue, info());

    assert!(json["components"]["schemas"].get("a.one.Input").is_some());
    assert!(json["components"]["schemas"].get("b.two.Input").is_some());
}

#[test]
fn a_namespace_cannot_produce_a_doubled_separator() {
    let mut catalogue = Catalogue::new(1);
    catalogue.add(Method::new("/session/", "/list"));

    let json = document(&catalogue, info());

    assert!(json["paths"].get("/session/list").is_some());
}

// ---- the transport hint -----------------------------------------------------------------------

#[test]
fn the_invoke_route_is_published_as_an_extension() {
    // This build serves every method through one envelope route, so a client that wants to talk to
    // this server as it is needs the address the paths do not spell.
    let json = document(&Catalogue::new(1), info());

    assert_eq!(json["x-girder-invoke-path"], "/api/remote.invoke");
}

/// Collects every `$ref` string in a document.
fn collect_refs(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    walk_refs(value, &mut found);
    found
}

/// Walks a value, recording each `$ref`.
fn walk_refs(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "$ref"
                    && let Some(reference) = value.as_str()
                {
                    found.push(reference.to_owned());
                }
                walk_refs(value, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_refs(item, found);
            }
        }
        _ => {}
    }
}
