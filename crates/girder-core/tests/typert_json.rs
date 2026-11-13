//! Tests for the JSON Schema emitter.
//!
//! A generated schema is a contract a *validator* enforces, so the assertions here are about what
//! a validator would accept and reject, not about the shape of the JSON. Two of them catch the
//! failures this emitter exists to avoid: `null` must be expressible as a type union rather than
//! the `nullable` keyword draft 2020-12 does not define, and a nullable field must still leave
//! `required`, because "may be null" and "may be missing" are different promises.
//!
//! The module is included by path rather than through a crate re-export, so these tests compile
//! against it as written before the crate root declares it. It names its dependencies through
//! `girder_core::…`, which resolves here because the library aliases itself under that name.

#[path = "../src/typert_json.rs"]
mod typert_json;

use girder_core::{Catalogue, Field, Method, ObjectSchema, Parameter, Schema, Variant};
use serde_json::Value;

use typert_json::{catalogue_to_json_schema, method_to_json_schema, to_json_schema};

// ---- primitives -------------------------------------------------------------------------------

#[test]
fn a_string_is_typed_as_a_string() {
    assert_eq!(
        to_json_schema(&Schema::String),
        serde_json::json!({"type": "string"})
    );
}

#[test]
fn a_bool_is_typed_as_a_boolean() {
    assert_eq!(
        to_json_schema(&Schema::Bool),
        serde_json::json!({"type": "boolean"})
    );
}

#[test]
fn integers_and_floats_stay_distinct() {
    // `number` would accept a fractional value for a field the server parses as an integer, which
    // is a bug the client can catch before sending it.
    let integer = to_json_schema(&Schema::Int64);
    let float = to_json_schema(&Schema::Float64);

    assert_eq!(integer["type"], "integer");
    assert_eq!(integer["format"], "int64");
    assert_eq!(float["type"], "number");
    assert_eq!(float["format"], "double");
}

#[test]
fn an_opaque_value_is_an_empty_schema() {
    // An empty schema accepts every JSON value, which is exactly what "opaque" means.
    let json = to_json_schema(&Schema::Any);

    assert!(json.as_object().is_some_and(serde_json::Map::is_empty));
}

#[test]
fn an_enum_lists_its_values_under_a_string_type() {
    let json = to_json_schema(&Schema::Enum(vec![
        "active".to_owned(),
        "paused".to_owned(),
    ]));

    assert_eq!(json["type"], "string");
    assert_eq!(json["enum"], serde_json::json!(["active", "paused"]));
}

// ---- containers -------------------------------------------------------------------------------

#[test]
fn an_array_describes_its_elements() {
    let json = to_json_schema(&Schema::Array(Box::new(Schema::String)));

    assert_eq!(json["type"], "array");
    assert_eq!(json["items"]["type"], "string");
}

#[test]
fn an_array_of_objects_keeps_the_element_shape() {
    let json = to_json_schema(&Schema::Array(Box::new(Schema::Object(ObjectSchema::new(
        "Item",
        vec![Field::required("id", Schema::String)],
    )))));

    assert_eq!(json["items"]["properties"]["id"]["type"], "string");
}

#[test]
fn a_map_becomes_additional_properties() {
    let json = to_json_schema(&Schema::Map(Box::new(Schema::Int64)));

    assert_eq!(json["type"], "object");
    assert_eq!(json["additionalProperties"]["type"], "integer");
}

#[test]
fn a_union_fixes_the_discriminator_of_each_branch() {
    let json = to_json_schema(&Schema::OneOf(vec![
        Variant {
            tag: "completed".to_owned(),
            schema: Schema::Object(ObjectSchema::new(
                "Completed",
                vec![Field::required("text", Schema::String)],
            )),
        },
        Variant {
            tag: "failed".to_owned(),
            schema: Schema::Object(ObjectSchema::new(
                "Failed",
                vec![Field::required("reason", Schema::String)],
            )),
        },
    ]));

    assert_eq!(json["oneOf"][0]["properties"]["kind"]["const"], "completed");
    assert_eq!(json["oneOf"][1]["properties"]["kind"]["const"], "failed");
    // `const` narrows only when the field is present, so each branch has to require it.
    assert_eq!(
        json["oneOf"][0]["required"],
        serde_json::json!(["text", "kind"])
    );
}

// ---- absence ----------------------------------------------------------------------------------

#[test]
fn an_optional_is_a_type_union_rather_than_the_nullable_keyword() {
    // `nullable` is OpenAPI 3.0, not draft 2020-12: a validator ignores it and then rejects the
    // `null` the server accepts. This is the whole reason the module exists.
    let json = to_json_schema(&Schema::Optional(Box::new(Schema::String)));

    assert_eq!(json["type"], serde_json::json!(["string", "null"]));
    assert!(json.get("nullable").is_none());
}

#[test]
fn an_optional_object_widens_by_any_of() {
    // There is no single `type` to add `null` to once the inner schema is a `$ref`.
    let json = to_json_schema(&Schema::Optional(Box::new(Schema::Ref("Thing".to_owned()))));

    assert_eq!(json["anyOf"][0]["$ref"], "#/$defs/Thing");
    assert_eq!(json["anyOf"][1]["type"], "null");
}

#[test]
fn a_double_optional_does_not_repeat_null() {
    let json = to_json_schema(&Schema::Optional(Box::new(Schema::Optional(Box::new(
        Schema::String,
    )))));

    assert_eq!(json["type"], serde_json::json!(["string", "null"]));
}

#[test]
fn an_optional_opaque_value_stays_an_empty_schema() {
    // An empty schema already accepts null; widening it would add noise to every passthrough field.
    let json = to_json_schema(&Schema::Optional(Box::new(Schema::Any)));

    assert!(json.as_object().is_some_and(serde_json::Map::is_empty));
}

// ---- objects ----------------------------------------------------------------------------------

#[test]
fn an_object_lists_its_properties_and_required_fields() {
    let object = ObjectSchema::new(
        "SessionSummary",
        vec![
            Field::required("id", Schema::String),
            Field::optional("title", Schema::String),
        ],
    );

    let json = to_json_schema(&Schema::Object(object));

    assert_eq!(json["type"], "object");
    assert_eq!(json["properties"]["id"]["type"], "string");
    assert_eq!(json["required"], serde_json::json!(["id"]));
}

#[test]
fn an_optional_field_is_absent_from_required_but_still_nullable() {
    // "May be omitted" and "may be null" are separate promises; the type keeps the second while
    // `required` records the first.
    let object = ObjectSchema::new(
        "T",
        vec![
            Field::required("id", Schema::String),
            Field::required("title", Schema::Optional(Box::new(Schema::String))),
        ],
    );

    let json = to_json_schema(&Schema::Object(object));

    assert_eq!(json["required"], serde_json::json!(["id"]));
    assert_eq!(
        json["properties"]["title"]["type"],
        serde_json::json!(["string", "null"])
    );
}

#[test]
fn a_field_added_later_is_not_required_on_a_standalone_schema() {
    // A standalone schema describes the newest server, so a field the server does require is
    // required here too: no revision is being emulated.
    let object = ObjectSchema::new(
        "T",
        vec![Field::required("journal", Schema::String).added_in(2)],
    );

    let json = to_json_schema(&Schema::Object(object));
    assert_eq!(json["required"], serde_json::json!(["journal"]));
}

#[test]
fn an_empty_object_is_a_valid_object_schema() {
    let json = to_json_schema(&Schema::Object(ObjectSchema::new("Empty", Vec::new())));

    assert_eq!(json["type"], "object");
    assert_eq!(json["properties"], serde_json::json!({}));
    assert_eq!(json["required"], serde_json::json!([]));
}

#[test]
fn a_nested_object_is_rendered_in_place() {
    let inner = ObjectSchema::new("Inner", vec![Field::required("n", Schema::Int64)]);
    let outer = ObjectSchema::new(
        "Outer",
        vec![Field::required("inner", Schema::Object(inner))],
    );

    let json = to_json_schema(&Schema::Object(outer));

    assert_eq!(
        json["properties"]["inner"]["properties"]["n"]["type"],
        "integer"
    );
    assert_eq!(
        json["properties"]["inner"]["required"],
        serde_json::json!(["n"])
    );
}

#[test]
fn a_field_documents_itself() {
    let object = ObjectSchema::new(
        "T",
        vec![Field::required("id", Schema::String).with_docs("stable identity")],
    );

    let json = to_json_schema(&Schema::Object(object));
    assert_eq!(json["properties"]["id"]["description"], "stable identity");
}

#[test]
fn a_reference_points_at_the_definitions_block() {
    let json = to_json_schema(&Schema::Ref("SessionSummary".to_owned()));

    assert_eq!(json["$ref"], "#/$defs/SessionSummary");
}

// ---- methods ----------------------------------------------------------------------------------

#[test]
fn a_method_body_is_the_root_object() {
    // The root *is* the body: a tool declaration wants the parameters at the top, not nested under
    // an `input` property that the wire format does not have.
    let method = Method::new("session", "page")
        .with_parameter(Parameter::required("limit", Schema::Int64))
        .with_parameter(Parameter::optional(
            "after",
            Schema::Optional(Box::new(Schema::String)),
        ))
        .returning(Schema::Array(Box::new(Schema::Ref(
            "SessionSummary".to_owned(),
        ))))
        .with_docs("One page of sessions.");

    let json = method_to_json_schema(&method);

    assert_eq!(json["type"], "object");
    assert_eq!(json["title"], "session.page");
    assert_eq!(json["description"], "One page of sessions.");
    assert_eq!(json["properties"]["limit"]["type"], "integer");
    assert_eq!(json["required"], serde_json::json!(["limit"]));
    // The return value is not part of the request, so it cannot be a sibling of a property.
    assert_eq!(json["x-girder-output"]["type"], "array");
}

#[test]
fn a_method_with_no_parameters_has_an_empty_body() {
    let json = method_to_json_schema(&Method::new("session", "list"));

    assert_eq!(json["required"], serde_json::json!([]));
    assert_eq!(json["properties"], serde_json::json!({}));
}

#[test]
fn a_method_reports_the_references_it_cannot_resolve() {
    // A `Method` carries type names, not the types themselves; a silent dangling `$ref` would be a
    // schema that validates nothing useful, so the document says so.
    let method = Method::new("session", "get")
        .with_parameter(Parameter::required("id", Schema::Ref("Unknown".to_owned())));

    let json = method_to_json_schema(&method);
    assert_eq!(
        json["x-girder-unresolved-refs"],
        serde_json::json!(["Unknown"])
    );
}

// ---- catalogues -------------------------------------------------------------------------------

/// A catalogue with one referenced type, one unreferenced type and one method.
fn catalogue() -> Catalogue {
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "SessionSummary",
        vec![
            Field::required("id", Schema::String),
            Field::optional("title", Schema::Optional(Box::new(Schema::String))),
            Field::required(
                "parent",
                Schema::Optional(Box::new(Schema::Ref("SessionSummary".to_owned()))),
            ),
        ],
    ));
    // Never referenced by a method, but still part of the contract a client may need.
    catalogue.define(ObjectSchema::new(
        "SessionDetail",
        vec![Field::required(
            "summary",
            Schema::Ref("SessionSummary".to_owned()),
        )],
    ));
    catalogue.add(
        Method::new("session", "page")
            .with_parameter(Parameter::required("limit", Schema::Int64))
            .returning(Schema::Array(Box::new(Schema::Ref(
                "SessionSummary".to_owned(),
            )))),
    );
    catalogue
}

/// Every `$ref` string in a document, at any depth.
fn refs_in(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    match value {
        Value::Object(object) => {
            for (key, inner) in object {
                if key == "$ref" {
                    if let Some(reference) = inner.as_str() {
                        found.push(reference.to_owned());
                    }
                } else {
                    found.extend(refs_in(inner));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                found.extend(refs_in(item));
            }
        }
        _ => {}
    }
    found
}

/// Resolves a `#/$defs/…` pointer against a document, returning whether it exists.
fn resolves(document: &Value, reference: &str) -> bool {
    let Some(name) = reference.strip_prefix("#/$defs/") else {
        return false;
    };
    let name = name.replace("~1", "/").replace("~0", "~");
    document
        .get("$defs")
        .and_then(|defs| defs.get(&name))
        .is_some()
}

#[test]
fn a_catalogue_is_a_document_with_every_type_and_method() {
    let json = catalogue_to_json_schema(&catalogue());

    assert_eq!(
        json["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(json["x-girder-revision"], 1);
    assert_eq!(json["x-girder-methods"][0]["name"], "session.page");
    assert!(json["$defs"]["SessionSummary"].is_object());
    // A type no method mentions is still hoisted: a client may construct it.
    assert!(json["$defs"]["SessionDetail"].is_object());
}

#[test]
fn every_reference_in_a_catalogue_resolves() {
    // The property that makes a multi-type document usable at all. A dangling `$ref` is accepted
    // by every parser and validates nothing.
    let json = catalogue_to_json_schema(&catalogue());
    let refs = refs_in(&json);

    assert!(!refs.is_empty(), "the fixture has references to check");
    for reference in refs {
        assert!(
            resolves(&json, &reference),
            "unresolved reference {reference}"
        );
    }
    assert!(json.get("x-girder-unresolved-refs").is_none());
}

#[test]
fn a_catalogue_renders_requirements_at_its_own_revision() {
    // A catalogue describes a specific wire revision, so a field added in revision 2 is omittable
    // when the catalogue is revision 1 — the property JSON Schema cannot express on its own.
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "T",
        vec![
            Field::required("id", Schema::String),
            Field::required("journal", Schema::String).added_in(2),
        ],
    ));

    let json = catalogue_to_json_schema(&catalogue);
    assert_eq!(json["$defs"]["T"]["required"], serde_json::json!(["id"]));
}

#[test]
fn a_recursive_type_is_defined_once_and_referenced() {
    // Hoisting a type that mentions itself would recurse forever without the open-definition stack.
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

    let json = catalogue_to_json_schema(&catalogue);

    assert_eq!(
        json["$defs"]["Node"]["properties"]["next"]["anyOf"][0]["$ref"],
        "#/$defs/Node"
    );
    assert_eq!(json["$defs"]["Node"]["required"], serde_json::json!(["id"]));
}

#[test]
fn a_reference_to_a_name_that_needs_pointer_escaping_still_resolves() {
    // `/` in a pointer names a nested location; an unescaped name would parse and resolve to
    // nothing.
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "a/b",
        vec![Field::required("id", Schema::String)],
    ));
    catalogue.add(
        Method::new("x", "y")
            .with_parameter(Parameter::required("v", Schema::Ref("a/b".to_owned()))),
    );

    let json = catalogue_to_json_schema(&catalogue);
    let reference = json["x-girder-methods"][0]["input"]["properties"]["v"]["$ref"]
        .as_str()
        .map(str::to_owned);

    assert_eq!(reference.as_deref(), Some("#/$defs/a~1b"));
    assert!(resolves(&json, &reference.unwrap_or_default()));
}

#[test]
fn an_empty_catalogue_is_still_a_valid_document() {
    let json = catalogue_to_json_schema(&Catalogue::new(0));

    assert_eq!(json["x-girder-revision"], 0);
    assert_eq!(json["x-girder-methods"], serde_json::json!([]));
    assert!(
        json.get("$defs").is_none(),
        "nothing to define, nothing emitted"
    );
}
