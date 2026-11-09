//! Tests for the type catalogue (the first step of the typert decision).
//!
//! The schema shape is being decided by evidence here rather than by what a macro happens to emit
//! later. Two properties matter most: `Option` must render as nullable rather than as a bare type,
//! and a field added in a later revision must be omittable by an older client.

use girder_core::{Catalogue, Field, Method, ObjectSchema, Parameter, Schema, Variant};

// ---- primitives -------------------------------------------------------------------------------

#[test]
fn primitives_render_as_their_json_types() {
    assert_eq!(Schema::String.to_json()["type"], "string");
    assert_eq!(Schema::Bool.to_json()["type"], "boolean");
    assert_eq!(Schema::Int64.to_json()["type"], "integer");
    assert_eq!(Schema::Int64.to_json()["format"], "int64");
    assert_eq!(Schema::Float64.to_json()["type"], "number");
}

#[test]
fn an_opaque_value_renders_as_an_empty_schema() {
    // An empty JSON Schema accepts anything, which is exactly what "opaque" means.
    let json = Schema::Any.to_json();

    assert!(json.as_object().is_some_and(serde_json::Map::is_empty));
}

#[test]
fn an_enum_renders_its_values() {
    let json = Schema::Enum(vec!["active".to_owned(), "paused".to_owned()]).to_json();

    assert_eq!(json["type"], "string");
    assert_eq!(json["enum"][0], "active");
    assert_eq!(json["enum"][1], "paused");
}

// ---- containers -------------------------------------------------------------------------------

#[test]
fn an_array_renders_its_element_schema() {
    let json = Schema::Array(Box::new(Schema::String)).to_json();

    assert_eq!(json["type"], "array");
    assert_eq!(json["items"]["type"], "string");
}

#[test]
fn a_map_renders_additional_properties() {
    let json = Schema::Map(Box::new(Schema::Int64)).to_json();

    assert_eq!(json["type"], "object");
    assert_eq!(json["additionalProperties"]["type"], "integer");
}

#[test]
fn an_optional_renders_as_nullable() {
    // A client that did not know Option accepts null would send an empty string instead.
    let json = Schema::Optional(Box::new(Schema::String)).to_json();

    assert_eq!(json["type"], "string");
    assert_eq!(json["nullable"], true);
}

#[test]
fn a_nested_optional_stays_nullable() {
    let json = Schema::Optional(Box::new(Schema::Array(Box::new(Schema::String)))).to_json();

    assert_eq!(json["type"], "array");
    assert_eq!(json["nullable"], true);
}

#[test]
fn a_reference_points_at_the_definitions_block() {
    // A reference rather than an inline copy: inlining a type used five times gives five chances to
    // disagree, and a recursive type cannot be inlined at all.
    let json = Schema::Ref("SessionSummary".to_owned()).to_json();

    assert_eq!(json["$ref"], "#/$defs/SessionSummary");
}

// ---- discriminated unions ---------------------------------------------------------------------

#[test]
fn a_one_of_renders_each_branch_with_its_discriminator() {
    // The discriminator is fixed per branch, so a client can narrow without checking.
    let schema = Schema::OneOf(vec![
        Variant {
            tag: "completed".to_owned(),
            schema: Schema::Object(ObjectSchema::new(
                "Completed",
                vec![Field::required("turn", Schema::Int64)],
            )),
        },
        Variant {
            tag: "failed".to_owned(),
            schema: Schema::Object(ObjectSchema::new(
                "Failed",
                vec![Field::required("reason", Schema::String)],
            )),
        },
    ]);

    let json = schema.to_json();

    assert!(json["oneOf"].is_array());
    assert_eq!(json["oneOf"][0]["properties"]["kind"]["const"], "completed");
    assert_eq!(json["oneOf"][1]["properties"]["kind"]["const"], "failed");
}

// ---- objects ----------------------------------------------------------------------------------

#[test]
fn an_object_lists_its_properties_and_required_fields() {
    let object = ObjectSchema::new(
        "SessionSummary",
        vec![
            Field::required("id", Schema::String),
            Field::optional("title", Schema::Optional(Box::new(Schema::String))),
        ],
    );

    let json = object.to_json();

    assert_eq!(json["title"], "SessionSummary");
    assert_eq!(json["properties"]["id"]["type"], "string");
    assert_eq!(json["required"][0], "id");
    assert_eq!(json["required"].as_array().map(Vec::len), Some(1));
}

#[test]
fn a_field_documents_itself_in_its_schema() {
    let object = ObjectSchema::new(
        "T",
        vec![Field::required("id", Schema::String).with_docs("stable identity")],
    );

    let json = object.to_json();
    assert_eq!(json["properties"]["id"]["description"], "stable identity");
}

#[test]
fn an_object_documents_itself() {
    let mut object = ObjectSchema::new("T", vec![]);
    object.docs = Some("a summary".to_owned());

    assert_eq!(object.to_json()["description"], "a summary");
}

// ---- revisions --------------------------------------------------------------------------------

#[test]
fn a_field_added_later_is_omittable_by_an_older_client() {
    // The property JSON Schema cannot express. An old client cannot know to send a field that did
    // not exist, and a schema that demanded it would make every old client fail.
    let field = Field::required("journal", Schema::String).added_in(2);

    assert!(!field.omittable_at(2), "at revision 2 it is required");
    assert!(
        field.omittable_at(1),
        "at revision 1 the client cannot know it"
    );
    assert!(field.omittable_at(0));
}

#[test]
fn a_field_from_revision_one_is_required_at_every_later_revision() {
    let field = Field::required("id", Schema::String).added_in(1);

    assert!(!field.omittable_at(1));
    assert!(!field.omittable_at(5));
}

#[test]
fn an_optional_field_is_omittable_at_every_revision() {
    let field = Field::optional("title", Schema::String);

    assert!(field.omittable_at(0));
    assert!(field.omittable_at(9));
}

#[test]
fn an_object_reports_different_required_sets_per_revision() {
    // The whole point of tracking revisions: one type, two answers.
    let object = ObjectSchema::new(
        "T",
        vec![
            Field::required("id", Schema::String),
            Field::required("journal", Schema::String).added_in(2),
        ],
    );

    assert_eq!(object.required_at(1), vec!["id"]);
    assert_eq!(object.required_at(2), vec!["id", "journal"]);
}

#[test]
fn rendering_at_a_revision_uses_that_revisions_required_set() {
    let object = ObjectSchema::new(
        "T",
        vec![
            Field::required("id", Schema::String),
            Field::required("journal", Schema::String).added_in(2),
        ],
    );

    let old = object.to_json_at(1);
    let new = object.to_json_at(2);

    assert_eq!(old["required"].as_array().map(Vec::len), Some(1));
    assert_eq!(new["required"].as_array().map(Vec::len), Some(2));
}

// ---- methods ----------------------------------------------------------------------------------

#[test]
fn a_method_reports_its_qualified_name() {
    let method = Method::new("session", "create");

    assert_eq!(method.qualified(), "session.create");
}

#[test]
fn a_method_renders_its_input_schema_and_output() {
    let method = Method::new("session", "page")
        .with_parameter(Parameter::required("limit", Schema::Int64))
        .with_parameter(Parameter::optional("after", Schema::String))
        .returning(Schema::Array(Box::new(Schema::Ref(
            "SessionSummary".to_owned(),
        ))))
        .with_docs("pages a session's events");

    let json = method.to_json();

    assert_eq!(json["name"], "session.page");
    assert_eq!(json["input"]["properties"]["limit"]["type"], "integer");
    assert_eq!(json["input"]["required"][0], "limit");
    assert_eq!(json["input"]["required"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["output"]["type"], "array");
    assert_eq!(json["description"], "pages a session's events");
}

#[test]
fn a_parameter_documents_itself() {
    let method = Method::new("n", "m")
        .with_parameter(Parameter::required("limit", Schema::Int64).with_docs("how many"));
    let json = method.to_json();

    assert_eq!(
        json["input"]["properties"]["limit"]["description"],
        "how many"
    );
}

#[test]
fn a_method_with_no_parameters_still_renders_an_object_input() {
    // A client that special-cased "no input" would fail on the first method that later gained one.
    let json = Method::new("n", "m").to_json();

    assert_eq!(json["input"]["type"], "object");
    assert_eq!(json["input"]["required"].as_array().map(Vec::len), Some(0));
}

// ---- the catalogue ----------------------------------------------------------------------------

#[test]
fn a_catalogue_holds_types_and_methods() {
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "SessionSummary",
        vec![Field::required("id", Schema::String)],
    ));
    catalogue
        .add(Method::new("session", "list").returning(Schema::Ref("SessionSummary".to_owned())));

    let json = catalogue.to_json();

    assert_eq!(json["revision"], 1);
    assert!(json["$defs"]["SessionSummary"].is_object());
    assert_eq!(json["methods"][0]["name"], "session.list");
}

#[test]
fn a_catalogue_renders_types_at_its_own_revision() {
    // A client asking for revision 1 must not be told to send a revision-2 field.
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "T",
        vec![
            Field::required("id", Schema::String),
            Field::required("journal", Schema::String).added_in(2),
        ],
    ));

    let json = catalogue.to_json();

    assert_eq!(
        json["$defs"]["T"]["required"].as_array().map(Vec::len),
        Some(1)
    );
}

#[test]
fn an_empty_catalogue_names_its_revision() {
    let json = Catalogue::new(3).to_json();

    assert_eq!(json["revision"], 3);
    assert!(json["methods"].as_array().is_some_and(Vec::is_empty));
}

#[test]
fn the_catalogue_is_valid_json_end_to_end() {
    // A client parses this; malformed JSON would fail there rather than here.
    let mut catalogue = Catalogue::new(1);
    catalogue.define(ObjectSchema::new(
        "T",
        vec![Field::required("id", Schema::String)],
    ));
    catalogue.add(Method::new("n", "m").returning(Schema::Ref("T".to_owned())));

    let text = serde_json::to_string(&catalogue.to_json()).expect("serializes");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("parses");

    assert_eq!(parsed["revision"], 1);
}
