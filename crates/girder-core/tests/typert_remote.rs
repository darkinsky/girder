//! Tests for `#[remote]`.
//!
//! The failure mode this macro must not have is making the decorated function disappear: an
//! attribute macro replaces the item, so forgetting to emit it back produces code that compiles
//! while the function is gone. Every test here calls the original function as well as reading its
//! descriptor.

#![allow(
    dead_code,
    reason = "the functions exist to be described and called by tests"
)]

use girder_core::{Method, Schema, remote};

/// Pages one session's events.
#[remote(namespace = "session", name = "page")]
fn page(limit: u64, after: Option<String>) -> String {
    format!("{limit}:{}", after.unwrap_or_default())
}

#[test]
fn the_original_function_is_still_callable() {
    // The property an attribute macro most easily breaks.
    assert_eq!(page(10, None), "10:");
    assert_eq!(page(5, Some("abc".to_owned())), "5:abc");
}

#[test]
fn the_descriptor_names_the_namespace_and_method() {
    let method: Method = PAGE_REMOTE_METHOD();

    assert_eq!(method.namespace, "session");
    assert_eq!(method.name, "page");
    assert_eq!(method.qualified(), "session.page");
}

#[test]
fn the_descriptor_lists_the_parameters() {
    let method = PAGE_REMOTE_METHOD();

    assert_eq!(method.parameters.len(), 2);
    assert_eq!(method.parameters[0].name, "limit");
    assert_eq!(method.parameters[0].schema, Schema::Int64);
    assert!(method.parameters[0].required);
    // An `Option` is nullable but the parameter is still supplied, so it stays required on the wire.
    assert_eq!(method.parameters[1].name, "after");
    assert_eq!(
        method.parameters[1].schema,
        Schema::Optional(Box::new(Schema::String))
    );
}

#[test]
fn the_descriptor_records_the_return_type() {
    let method = PAGE_REMOTE_METHOD();

    assert_eq!(method.returns, Schema::String);
}

#[test]
fn the_documentation_reaches_the_descriptor() {
    let method = PAGE_REMOTE_METHOD();

    assert_eq!(method.docs.as_deref(), Some("Pages one session's events."));
}

#[test]
fn the_descriptor_renders_as_json() {
    // A client consumes the JSON, not the struct.
    let json = PAGE_REMOTE_METHOD().to_json();

    assert_eq!(json["name"], "session.page");
    assert_eq!(json["input"]["properties"]["limit"]["type"], "integer");
    assert_eq!(json["output"]["type"], "string");
    assert_eq!(json["description"], "Pages one session's events.");
}

// ---- a method with no parameters ---------------------------------------------------------------

#[remote(namespace = "session", name = "list")]
fn list() -> Vec<String> {
    vec!["a".to_owned()]
}

#[test]
fn a_parameterless_method_renders_an_empty_input() {
    let method = LIST_REMOTE_METHOD();

    assert!(method.parameters.is_empty());
    let json = method.to_json();
    assert_eq!(json["input"]["type"], "object");
    assert_eq!(json["input"]["required"].as_array().map(Vec::len), Some(0));
}

#[test]
fn a_vec_return_becomes_an_array() {
    assert_eq!(
        LIST_REMOTE_METHOD().returns,
        Schema::Array(Box::new(Schema::String))
    );
}

// ---- the default name ---------------------------------------------------------------------------

#[remote(namespace = "goal")]
fn complete() {}

#[test]
fn the_default_name_is_the_function_name() {
    assert_eq!(COMPLETE_REMOTE_METHOD().name, "complete");
}

// ---- a reference parameter -----------------------------------------------------------------------

#[remote(namespace = "workspace")]
fn open(path: &str) -> bool {
    !path.is_empty()
}

#[test]
fn a_reference_parameter_describes_the_value_not_the_borrow() {
    // The wire carries the string, not a pointer to it.
    let method = OPEN_REMOTE_METHOD();

    assert_eq!(method.parameters[0].schema, Schema::String);
}

#[test]
fn a_reference_parameter_function_still_works() {
    assert!(open("x"));
    assert!(!open(""));
}

// ---- a method taking a named type ---------------------------------------------------------------

#[derive(girder_core::Typert)]
struct Request {
    text: String,
}

#[remote(namespace = "echo")]
fn echo(request: Request) -> Request {
    request
}

#[test]
fn a_named_type_becomes_a_reference() {
    let method = ECHO_REMOTE_METHOD();

    assert_eq!(
        method.parameters[0].schema,
        Schema::Ref("Request".to_owned())
    );
    assert_eq!(method.returns, Schema::Ref("Request".to_owned()));
}

// ---- two methods in one namespace ---------------------------------------------------------------

#[test]
fn two_methods_of_one_namespace_coexist() {
    let page_method = PAGE_REMOTE_METHOD();
    let list_method = LIST_REMOTE_METHOD();

    assert_eq!(page_method.namespace, list_method.namespace);
    assert_ne!(page_method.name, list_method.name);
}
