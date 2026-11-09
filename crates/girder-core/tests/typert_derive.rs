//! Tests for `#[derive(Typert)]`.
//!
//! The hand-written schema in `tests/typert.rs` is the standard: the macro must produce the same
//! thing. A macro that disagreed with a considered design would be the macro's fault, so these tests
//! compare against explicit expectations rather than against whatever the macro emits.

// Every struct in this file exists to be described by the derive; nothing reads its fields.
#![allow(dead_code, reason = "test structs exist for the derive, not for use")]

use girder_core::{Field, ObjectSchema, Schema, Typert};

/// The hand-written reference: what the macro must produce.
///
/// The type's own `///` documentation is part of it: the derive carries doc comments into the
/// schema, so a reference that omitted them would be describing a different thing.
fn reference() -> ObjectSchema {
    let mut schema = ObjectSchema::new(
        "SessionSummary",
        vec![
            Field::required("id", Schema::String).with_docs("stable identity"),
            Field::required("turns", Schema::Int64),
            Field::optional("title", Schema::Optional(Box::new(Schema::String))),
            Field::required(
                "journal",
                Schema::Array(Box::new(Schema::Ref("JournalEntry".to_owned()))),
            )
            .added_in(2),
        ],
    );
    schema.docs = Some("The same shape, produced by the macro.".to_owned());
    schema
}

/// The same shape, produced by the macro.
#[derive(Typert)]
#[typert(name = "SessionSummary")]
struct SessionSummary {
    /// stable identity
    id: String,
    turns: u64,
    title: Option<String>,
    #[typert(added_in = 2)]
    journal: Vec<JournalEntry>,
}

#[allow(dead_code)]
struct JournalEntry {
    text: String,
}

// ---- equivalence with the hand-written schema -------------------------------------------------

#[test]
fn the_derive_matches_the_hand_written_schema() {
    // The property the whole exercise rests on.
    assert_eq!(SessionSummary::typert_schema(), reference());
}

#[test]
fn the_rendered_json_matches() {
    // Equality of the struct is not enough: the JSON is what a client actually receives.
    assert_eq!(
        SessionSummary::typert_schema().to_json(),
        reference().to_json()
    );
}

#[test]
fn the_derive_respects_revisions() {
    let schema = SessionSummary::typert_schema();

    assert_eq!(schema.required_at(1), vec!["id", "turns"]);
    assert_eq!(schema.required_at(2), vec!["id", "turns", "journal"]);
}

// ---- individual mappings ----------------------------------------------------------------------

#[derive(Typert)]
struct Primitives {
    text: String,
    flag: bool,
    count: u64,
    signed: i32,
    ratio: f64,
    anything: serde_json::Value,
}

#[test]
fn primitives_map_to_their_json_types() {
    let schema = Primitives::typert_schema();
    let json = schema.to_json();

    assert_eq!(json["properties"]["text"]["type"], "string");
    assert_eq!(json["properties"]["flag"]["type"], "boolean");
    assert_eq!(json["properties"]["count"]["type"], "integer");
    assert_eq!(json["properties"]["signed"]["type"], "integer");
    assert_eq!(json["properties"]["ratio"]["type"], "number");
    assert!(
        json["properties"]["anything"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    );
}

#[derive(Typert)]
struct Containers {
    items: Vec<String>,
    maybe: Option<u32>,
    nested: Option<Vec<String>>,
}

#[test]
fn containers_map_to_arrays_and_nullables() {
    let json = Containers::typert_schema().to_json();

    assert_eq!(json["properties"]["items"]["type"], "array");
    assert_eq!(json["properties"]["items"]["items"]["type"], "string");
    assert_eq!(json["properties"]["maybe"]["nullable"], true);
    assert_eq!(json["properties"]["nested"]["type"], "array");
    assert_eq!(json["properties"]["nested"]["nullable"], true);
}

#[test]
fn an_option_field_is_optional_even_without_the_attribute() {
    // An `Option` that still claimed `required` would make a client send null rather than omitting.
    let schema = Containers::typert_schema();

    assert_eq!(
        schema.required_at(0),
        vec!["items"],
        "nested and maybe are optional"
    );
}

#[derive(Typert)]
struct Renamed {
    #[typert(rename = "wire_name")]
    rust_name: String,
}

#[test]
fn a_rename_changes_the_wire_name() {
    let json = Renamed::typert_schema().to_json();

    assert!(json["properties"]["wire_name"].is_object());
    assert!(json["properties"]["rust_name"].is_null());
}

#[derive(Typert)]
struct ExplicitOptional {
    #[typert(optional)]
    maybe_absent: String,
}

#[test]
fn an_explicit_optional_is_omittable() {
    // A field the server always sets but a client may omit — an older client, typically.
    let schema = ExplicitOptional::typert_schema();

    assert!(schema.required_at(0).is_empty());
}

#[derive(Typert)]
#[typert(added_in = 3)]
struct AddedLater {
    id: String,
}

#[test]
fn a_type_can_record_its_own_revision() {
    let schema = AddedLater::typert_schema();

    assert_eq!(schema.added_in, Some(3));
}

#[derive(Typert)]
/// A documented type.
struct Documented {
    /// a documented field
    value: String,
}

#[test]
fn documentation_reaches_the_schema() {
    let json = Documented::typert_schema().to_json();

    assert_eq!(json["description"], "A documented type.");
    assert_eq!(
        json["properties"]["value"]["description"],
        "a documented field"
    );
}

#[derive(Typert)]
struct NamedType {
    entry: JournalEntry,
}

#[test]
fn an_unknown_type_becomes_a_reference() {
    // A reference rather than an inline copy: one definition in one place.
    let json = NamedType::typert_schema().to_json();

    assert_eq!(json["properties"]["entry"]["$ref"], "#/$defs/JournalEntry");
}

#[test]
fn the_default_name_is_the_rust_name() {
    // No `#[typert(name)]`, so the struct's own name is used.
    assert_eq!(Primitives::typert_schema().name, "Primitives");
}

#[derive(Typert)]
struct Empty {}

#[test]
fn an_empty_struct_renders_an_empty_object() {
    let json = Empty::typert_schema().to_json();

    assert_eq!(json["type"], "object");
    assert!(
        json["properties"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    );
}

// ---- discriminated unions ---------------------------------------------------------------------

/// A union of two shapes, with one variant carrying fields.
#[derive(Typert)]
enum Outcome {
    Completed,
    Failed {
        reason: String,
        #[typert(added_in = 2)]
        code: Option<u32>,
    },
}

#[test]
fn an_enum_becomes_a_discriminated_union() {
    let schema = Outcome::typert_schema();

    let Schema::OneOf(variants) = schema else {
        panic!("an enum must be a union");
    };
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0].tag, "completed");
    assert_eq!(variants[1].tag, "failed");
}

#[test]
fn a_unit_variant_has_only_its_tag() {
    let Schema::OneOf(variants) = Outcome::typert_schema() else {
        panic!("expected a union");
    };
    let Schema::Object(object) = &variants[0].schema else {
        panic!("expected an object");
    };

    assert!(object.fields.is_empty(), "a unit variant carries no fields");
}

#[test]
fn a_struct_variant_carries_its_fields() {
    let Schema::OneOf(variants) = Outcome::typert_schema() else {
        panic!("expected a union");
    };
    let Schema::Object(object) = &variants[1].schema else {
        panic!("expected an object");
    };

    assert_eq!(object.fields.len(), 2);
    assert_eq!(object.fields[0].name, "reason");
    assert!(object.fields[0].required);
    assert_eq!(
        object.required_at(1),
        vec!["reason"],
        "code arrived in revision 2"
    );
    assert_eq!(object.required_at(2), vec!["reason"]);
}

#[test]
fn the_rendered_union_constrains_each_tag() {
    // The property a client relies on: narrowing on `kind` needs no other check.
    let json = Outcome::typert_schema().to_json();

    assert_eq!(json["oneOf"][0]["properties"]["kind"]["const"], "completed");
    assert_eq!(json["oneOf"][1]["properties"]["kind"]["const"], "failed");
}

#[test]
fn a_variant_tag_is_kebab_case_of_the_rust_name() {
    // Matching the event tags this codebase already writes: a client reads `turn-end`.
    let Schema::OneOf(variants) = Outcome::typert_schema() else {
        panic!("expected a union");
    };

    assert_eq!(variants[1].tag, "failed");
}

#[derive(Typert)]
enum RenamedEnum {
    #[typert(rename = "wire-tag")]
    Variant,
}

#[test]
fn a_rename_overrides_the_default_tag() {
    let Schema::OneOf(variants) = RenamedEnum::typert_schema() else {
        panic!("expected a union");
    };

    assert_eq!(variants[0].tag, "wire-tag");
}

#[derive(Typert)]
/// An enum with documentation.
enum DocumentedEnum {
    /// The only branch.
    Only,
}

#[test]
fn a_variant_registers_its_branch() {
    // Each variant renders as a named object so a `$ref` target is legible.
    let Schema::OneOf(variants) = DocumentedEnum::typert_schema() else {
        panic!("expected a union");
    };
    let Schema::Object(object) = &variants[0].schema else {
        panic!("expected an object");
    };

    assert_eq!(object.name, "DocumentedEnumOnly");
    assert_eq!(object.docs.as_deref(), Some("The only branch."));
}

#[test]
fn an_enum_and_a_struct_never_share_a_schema_shape() {
    // A struct is an ObjectSchema; an enum is a Schema::OneOf. Keeping them apart is why the two
    // expansions are separate functions rather than branches of one.
    let struct_schema: ObjectSchema = SessionSummary::typert_schema();
    let enum_schema: Schema = Outcome::typert_schema();

    assert!(matches!(enum_schema, Schema::OneOf(_)));
    assert_eq!(struct_schema.name, "SessionSummary");
}
