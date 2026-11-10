//! Tests for the session Remote surface.
//!
//! This is the first subsystem where the whole chain runs: a Rust type becomes a schema, the schema
//! becomes a catalogue, and the catalogue becomes a TypeScript module a browser can use. Each link is
//! asserted, because a break in any of them produces a client that compiles and then fails.

use girder_core::session_api::{
    ModelCatalogue, ModelChoice, SessionCancelRequest, SessionCancelValue, SessionCreateValue,
    SessionPageRequest, SessionProjectionsRequest, SessionProjectionsValue, SessionRenameRequest,
    SessionRenameValue, session_catalogue, session_methods,
};
use girder_core::{Schema, to_typescript};

// ---- the wire types ---------------------------------------------------------------------------

#[test]
fn a_page_request_describes_its_fields() {
    let schema = SessionPageRequest::typert_schema();

    assert_eq!(schema.name, "SessionPageRequest");
    assert_eq!(schema.required_at(1), vec!["session_id", "limit"]);
    assert_eq!(
        schema.fields[2].schema,
        Schema::Optional(Box::new(Schema::Int64)),
        "after_seq is how a client continues"
    );
}

#[test]
fn a_cancel_request_makes_keep_inbox_optional() {
    // A client that had to send it would have to guess, and the default is to keep the input.
    let schema = SessionCancelRequest::typert_schema();

    assert_eq!(schema.required_at(1), vec!["session_id"]);
}

#[test]
fn a_cancel_value_has_one_field() {
    let schema = SessionCancelValue::typert_schema();

    assert_eq!(schema.fields.len(), 1);
    assert_eq!(schema.fields[0].name, "accepted");
    assert_eq!(schema.fields[0].schema, Schema::Bool);
}

#[test]
fn a_projection_value_is_a_map() {
    // A projection is registered by name; a field per projection would need changing whenever one is
    // added.
    let schema = SessionProjectionsValue::typert_schema();
    let field = &schema.fields[0];

    assert_eq!(field.name, "projections");
    // A `Map` whose values are opaque: the keys are projection names this layer does not enumerate,
    // and each value is that projection's folded state.
    assert_eq!(field.schema, Schema::Map(Box::new(Schema::Any)));
}

// ---- the method list --------------------------------------------------------------------------

#[test]
fn every_method_is_namespaced_under_session() {
    for method in session_methods() {
        assert_eq!(method.namespace, "session", "{}", method.name);
    }
}

#[test]
fn the_methods_cover_the_documented_surface() {
    let names: Vec<String> = session_methods().into_iter().map(|m| m.name).collect();

    assert_eq!(
        names,
        vec![
            "create",
            "page",
            "projections",
            "cancel",
            "rename",
            "modelCatalog"
        ]
    );
}

#[test]
fn a_page_method_requires_a_session_and_a_limit() {
    let page = session_methods()
        .into_iter()
        .find(|method| method.name == "page")
        .expect("page exists");

    assert_eq!(page.parameters[0].name, "sessionId");
    assert!(page.parameters[0].required);
    assert_eq!(page.parameters[1].name, "limit");
    assert!(page.parameters[1].required);
    assert!(
        !page.parameters[2].required,
        "afterSeq is how a client continues"
    );
}

#[test]
fn a_method_returns_a_reference_to_its_value_type() {
    let page = session_methods()
        .into_iter()
        .find(|method| method.name == "page")
        .expect("page exists");

    assert_eq!(page.returns, Schema::Ref("SessionPageValue".to_owned()));
}

#[test]
fn a_parameterless_method_has_an_empty_input() {
    let catalogue_method = session_methods()
        .into_iter()
        .find(|method| method.name == "modelCatalog")
        .expect("modelCatalog exists");

    assert!(catalogue_method.parameters.is_empty());
}

// ---- the catalogue ----------------------------------------------------------------------------

#[test]
fn the_catalogue_files_every_type_its_methods_reference() {
    // A `$ref` to a type the catalogue does not define is a client that cannot resolve it.
    let catalogue = session_catalogue();

    for method in &catalogue.methods {
        if let Schema::Ref(name) = &method.returns {
            assert!(
                catalogue.types.contains_key(name),
                "method `{}` returns an undefined type `{name}`",
                method.qualified()
            );
        }
    }
}

#[test]
fn the_catalogue_holds_every_wire_type() {
    let catalogue = session_catalogue();

    for name in [
        "SessionPageRequest",
        "SessionPageValue",
        "SessionProjectionsRequest",
        "SessionProjectionsValue",
        "SessionCancelRequest",
        "SessionCancelValue",
        "SessionRenameRequest",
        "SessionRenameValue",
        "SessionCreateRequest",
        "SessionCreateValue",
        "ModelChoice",
        "ModelCatalogue",
    ] {
        assert!(catalogue.types.contains_key(name), "missing {name}");
    }
}

#[test]
fn the_catalogue_renders_as_json() {
    let json = session_catalogue().to_json();

    assert_eq!(json["revision"], 1);
    assert!(json["$defs"]["SessionPageValue"].is_object());
    assert_eq!(json["methods"].as_array().map(Vec::len), Some(6));
}

#[test]
fn a_type_referenced_by_name_resolves_in_the_definitions() {
    // The client resolves `#/$defs/SessionPageValue`; an absent key is a runtime failure.
    let catalogue = session_catalogue();
    let json = catalogue.to_json();

    for name in catalogue.types.keys() {
        assert!(
            !json["$defs"][name].is_null(),
            "`{name}` is filed but not rendered"
        );
    }
}

// ---- the TypeScript client --------------------------------------------------------------------

#[test]
fn the_client_module_defines_every_type() {
    let output = to_typescript(&session_catalogue());

    for name in ["SessionPageValue", "SessionCancelValue", "ModelCatalogue"] {
        assert!(
            output.contains(&format!("export const {name}Schema")),
            "the client is missing {name}:\n{output}"
        );
    }
}

#[test]
fn the_client_module_lists_every_method() {
    let output = to_typescript(&session_catalogue());

    for method in [
        "session.create",
        "session.page",
        "session.cancel",
        "session.modelCatalog",
    ] {
        assert!(
            output.contains(&format!("'{method}': {{")),
            "the client is missing {method}:\n{output}"
        );
    }
}

#[test]
fn the_client_output_has_balanced_braces() {
    // A file the client cannot parse runs nothing at all.
    let output = to_typescript(&session_catalogue());

    assert_eq!(
        output.chars().filter(|c| *c == '{').count(),
        output.chars().filter(|c| *c == '}').count()
    );
}

#[test]
fn the_client_ids_are_plain_strings_on_the_wire() {
    // A branded identifier is a Rust-side guarantee; JSON carries a string, and the schema says so
    // rather than pretending the brand survives.
    let output = to_typescript(&session_catalogue());

    assert!(output.contains("session_id: z.string()"), "{output}");
}

// ---- the model catalogue ----------------------------------------------------------------------

#[test]
fn a_model_choice_declares_an_optional_window() {
    // A model whose window is unknown must not claim zero.
    let schema = ModelChoice::typert_schema();
    let window = schema
        .fields
        .iter()
        .find(|field| field.name == "context_window")
        .expect("context_window exists");

    assert_eq!(window.schema, Schema::Optional(Box::new(Schema::Int64)));
}

#[test]
fn a_model_catalogue_is_a_list_of_choices() {
    let schema = ModelCatalogue::typert_schema();

    assert_eq!(
        schema.fields[0].schema,
        Schema::Array(Box::new(Schema::Ref("ModelChoice".to_owned())))
    );
}

#[test]
fn the_create_value_carries_an_id_and_a_title() {
    let schema = SessionCreateValue::typert_schema();
    let names: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();

    assert_eq!(names, vec!["session_id", "title"]);
}

#[test]
fn a_rename_request_carries_the_new_title() {
    let schema = SessionRenameRequest::typert_schema();

    assert_eq!(schema.required_at(1), vec!["session_id", "title"]);
}

#[test]
fn a_rename_value_returns_the_stored_title() {
    // Trimmed, so a client does not have to guess what was stored.
    let schema = SessionRenameValue::typert_schema();

    assert_eq!(schema.fields[0].name, "title");
}

#[test]
fn a_projection_request_asks_for_one_session() {
    let schema = SessionProjectionsRequest::typert_schema();

    assert_eq!(schema.required_at(1), vec!["session_id"]);
}
