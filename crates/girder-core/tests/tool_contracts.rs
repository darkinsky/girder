//! Tests for tool-call contract validation.
//!
//! Two properties carry the weight here. The first is that the check actually looks at the schema:
//! a validator that returns `Ok(())` for everything passes every "valid arguments" test, so the
//! load-bearing tests are the negative ones, one per JSON type, each asserting the *path* of the
//! offending value and not merely that something failed.
//!
//! The second is the remedy. An error that only says "must be a string" makes the reader infer the
//! fix; a model that infers badly re-sends the same call. So every test that expects a violation
//! also asserts that `remedy` is non-empty and that it is not a restatement of the message.
//!
//! The module is included by path rather than through the crate root, so these tests compile against
//! the module as written even before the crate root declares it. That is also why nothing here
//! touches the rest of `girder_core`.

#[path = "../src/tool_contracts.rs"]
mod tool_contracts;

use serde_json::{Value, json};
use tool_contracts::{FailureKind, MAX_DEPTH, Violation, validate_arguments, violations};

/// Builds a JSON value from a literal, keeping the tests readable.
fn value(text: &str) -> Value {
    serde_json::from_str(text).expect("test literals are valid JSON")
}

/// The first violation for `schema`/`args`, failing the test when there is none.
fn violation(schema: &Value, args: &Value) -> Violation {
    let found = violations(schema, args);
    assert!(
        !found.is_empty(),
        "expected a violation for {args} against {schema}"
    );
    found
        .into_iter()
        .next()
        .unwrap_or_else(|| Violation::new(FailureKind::ToolFailed, "", ""))
}

/// Asserts every violation for this call names a usable fix and the right kind.
fn assert_invalid_arguments(schema: &Value, args: &Value) -> Violation {
    let first = violation(schema, args);
    assert_eq!(
        first.kind,
        FailureKind::InvalidArguments,
        "wrong kind for {args}: {first}"
    );
    assert_non_empty_remedy(&first);
    assert!(validate_arguments(schema, args).is_err());
    first
}

/// Asserts a remedy exists and is not just a restatement of the message.
fn assert_non_empty_remedy(violation: &Violation) {
    assert!(
        !violation.remedy.trim().is_empty(),
        "violation has no remedy: {violation}"
    );
    assert_ne!(
        violation.remedy.trim(),
        violation.message.trim(),
        "remedy restates the message instead of saying what to change"
    );
}

// ---- required fields ----------------------------------------------------------------------------

#[test]
fn missing_required_field_is_reported_with_its_path() {
    let schema = json!({
        "type": "object",
        "properties": { "path": { "type": "string" }, "limit": { "type": "number" } },
        "required": ["path", "limit"]
    });
    let found = assert_invalid_arguments(&schema, &json!({ "path": "src/lib.rs" }));
    assert!(
        found.message.contains("args.limit"),
        "message should name the missing field: {}",
        found.message
    );
    assert!(found.message.contains("required"), "{}", found.message);
    assert!(
        found.remedy.contains("limit"),
        "remedy should name the field to add: {}",
        found.remedy
    );
}

#[test]
fn every_missing_required_field_is_reported_at_once() {
    let schema = json!({
        "type": "object",
        "properties": { "a": {}, "b": {}, "c": {} },
        "required": ["a", "b", "c"]
    });
    let found = violations(&schema, &json!({}));
    assert_eq!(found.len(), 3, "{found:?}");
    for violation in &found {
        assert_non_empty_remedy(violation);
    }
}

#[test]
fn a_present_but_null_field_satisfies_required() {
    // JSON Schema distinguishes "absent" from "null": `required` asks only that the key exists, and
    // the `type` on that property is what rejects `null`.
    let schema = json!({ "type": "object", "properties": { "a": {} }, "required": ["a"] });
    assert!(validate_arguments(&schema, &json!({ "a": null })).is_ok());
}

// ---- type mismatches, one per type --------------------------------------------------------------

#[test]
fn string_field_rejects_a_number() {
    let schema = json!({ "type": "object", "properties": { "path": { "type": "string" } } });
    let found = assert_invalid_arguments(&schema, &json!({ "path": 7 }));
    assert!(found.message.contains("args.path"), "{}", found.message);
    assert!(found.message.contains("string"), "{}", found.message);
    assert!(found.message.contains("number"), "{}", found.message);
    assert!(found.remedy.contains("string"), "{}", found.remedy);
}

#[test]
fn number_field_rejects_a_string() {
    let schema = json!({ "type": "object", "properties": { "limit": { "type": "number" } } });
    let found = assert_invalid_arguments(&schema, &json!({ "limit": "10" }));
    assert!(found.message.contains("args.limit"), "{}", found.message);
    assert!(
        found.message.contains("must be a number"),
        "{}",
        found.message
    );
}

#[test]
fn integer_field_rejects_a_fraction() {
    let schema = json!({ "type": "object", "properties": { "offset": { "type": "integer" } } });
    let found = assert_invalid_arguments(&schema, &json!({ "offset": 1.5 }));
    assert!(found.message.contains("integer"), "{}", found.message);
}

#[test]
fn integer_field_accepts_an_integral_float() {
    // JSON has one number type; a producer that writes `3.0` means three.
    let schema = json!({ "type": "object", "properties": { "offset": { "type": "integer" } } });
    assert!(validate_arguments(&schema, &json!({ "offset": 3.0 })).is_ok());
    assert!(validate_arguments(&schema, &json!({ "offset": 3 })).is_ok());
    assert!(validate_arguments(&schema, &json!({ "offset": -3 })).is_ok());
}

#[test]
fn boolean_field_rejects_a_string() {
    let schema = json!({ "type": "object", "properties": { "force": { "type": "boolean" } } });
    let found = assert_invalid_arguments(&schema, &json!({ "force": "true" }));
    assert!(found.message.contains("args.force"), "{}", found.message);
    assert!(found.message.contains("boolean"), "{}", found.message);
}

#[test]
fn object_field_rejects_an_array() {
    let schema = json!({
        "type": "object",
        "properties": { "options": { "type": "object", "properties": { "depth": { "type": "number" } } } }
    });
    let found = assert_invalid_arguments(&schema, &json!({ "options": [1, 2] }));
    assert!(found.message.contains("args.options"), "{}", found.message);
    assert!(
        found.message.contains("must be an object"),
        "{}",
        found.message
    );
}

#[test]
fn array_field_rejects_an_object() {
    let schema = json!({ "type": "object", "properties": { "patterns": { "type": "array" } } });
    let found = assert_invalid_arguments(&schema, &json!({ "patterns": { "a": 1 } }));
    assert!(found.message.contains("args.patterns"), "{}", found.message);
    assert!(
        found.message.contains("must be an array"),
        "{}",
        found.message
    );
}

#[test]
fn null_field_rejects_a_string() {
    let schema = json!({ "type": "object", "properties": { "marker": { "type": "null" } } });
    let found = assert_invalid_arguments(&schema, &json!({ "marker": "null" }));
    assert!(found.message.contains("must be null"), "{}", found.message);
}

#[test]
fn a_root_value_of_the_wrong_type_is_rejected() {
    let schema = json!({ "type": "object", "required": ["path"] });
    let found = assert_invalid_arguments(&schema, &json!("not an object"));
    assert!(found.message.contains("args"), "{}", found.message);
}

// ---- nested paths and containers ----------------------------------------------------------------

#[test]
fn a_nested_violation_reports_the_full_path() {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "array",
                "items": { "type": "object", "properties": { "limit": { "type": "number" } } }
            }
        }
    });
    let found = assert_invalid_arguments(
        &schema,
        &json!({ "query": [ { "limit": 5 }, { "limit": "5" } ] }),
    );
    assert!(
        found.message.contains("args.query[1].limit"),
        "path should locate the element: {}",
        found.message
    );
}

#[test]
fn an_unusual_key_is_quoted_in_the_path() {
    let schema = json!({
        "type": "object",
        "properties": { "a-b": { "type": "number" } }
    });
    let found = assert_invalid_arguments(&schema, &json!({ "a-b": "text" }));
    assert!(
        found.message.contains("args[\"a-b\"]"),
        "non-identifier keys must be quoted: {}",
        found.message
    );
}

#[test]
fn additional_properties_false_rejects_undeclared_keys() {
    let schema = json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "additionalProperties": false
    });
    let found = assert_invalid_arguments(&schema, &json!({ "path": "a", "extra": 1 }));
    assert!(found.message.contains("args.extra"), "{}", found.message);
    assert!(found.remedy.contains("extra"), "{}", found.remedy);
}

#[test]
fn additional_properties_absent_allows_extra_keys() {
    let schema = json!({ "type": "object", "properties": { "path": { "type": "string" } } });
    assert!(validate_arguments(&schema, &json!({ "path": "a", "extra": 1 })).is_ok());
    let open = json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "additionalProperties": true
    });
    assert!(validate_arguments(&open, &json!({ "path": "a", "extra": 1 })).is_ok());
}

#[test]
fn enum_rejects_a_value_outside_the_list() {
    let schema = json!({
        "type": "object",
        "properties": { "mode": { "type": "string", "enum": ["fast", "safe"] } }
    });
    let found = assert_invalid_arguments(&schema, &json!({ "mode": "turbo" }));
    assert!(found.message.contains("fast"), "{}", found.message);
    assert!(found.remedy.contains("fast"), "{}", found.remedy);
    assert!(validate_arguments(&schema, &json!({ "mode": "safe" })).is_ok());
}

#[test]
fn a_long_enum_is_summarized_rather_than_pasted() {
    let allowed: Vec<String> = (0..20).map(|index| format!("\"rule-{index}\"")).collect();
    let schema = value(&format!(
        r#"{{ "type": "object", "properties": {{ "rule": {{ "type": "string", "enum": [{}] }} }} }}"#,
        allowed.join(", ")
    ));
    let found = assert_invalid_arguments(&schema, &json!({ "rule": "missing" }));
    assert!(found.message.contains("and 12 more"), "{}", found.message);
}

#[test]
fn const_rejects_another_value() {
    let schema = json!({
        "type": "object",
        "properties": { "version": { "type": "string", "const": "v2" } }
    });
    let found = assert_invalid_arguments(&schema, &json!({ "version": "v1" }));
    assert!(found.message.contains("v2"), "{}", found.message);
    assert!(validate_arguments(&schema, &json!({ "version": "v2" })).is_ok());
}

// ---- schemas that accept everything -------------------------------------------------------------

#[test]
fn an_empty_schema_accepts_any_arguments() {
    let empty = json!({});
    for candidate in [
        json!(null),
        json!(true),
        json!(0),
        json!("text"),
        json!([1, 2, 3]),
        json!({ "anything": { "nested": [true, null] } }),
    ] {
        assert!(
            validate_arguments(&empty, &candidate).is_ok(),
            "empty schema should accept {candidate}"
        );
    }
}

#[test]
fn an_annotation_only_schema_accepts_any_arguments() {
    let annotated = json!({ "description": "no constraints", "title": "Anything" });
    assert!(validate_arguments(&annotated, &json!({ "x": 1 })).is_ok());
    assert!(validate_arguments(&annotated, &json!(42)).is_ok());
}

#[test]
fn a_true_boolean_schema_accepts_any_arguments() {
    assert!(validate_arguments(&json!(true), &json!({ "x": 1 })).is_ok());
}

#[test]
fn a_false_boolean_schema_accepts_nothing() {
    let found = violation(&json!(false), &json!({}));
    assert_eq!(found.kind, FailureKind::ToolFailed);
    assert_non_empty_remedy(&found);
}

// ---- valid arguments ----------------------------------------------------------------------------

#[test]
fn valid_arguments_pass_a_realistic_tool_schema() {
    // Shape taken from the file-read tool in this crate.
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "offset": { "type": "number" },
            "limit": { "type": "number" }
        },
        "required": ["path"]
    });
    assert!(validate_arguments(&schema, &json!({ "path": "src/lib.rs" })).is_ok());
    assert!(
        validate_arguments(
            &schema,
            &json!({ "path": "src/lib.rs", "offset": 10, "limit": 20 })
        )
        .is_ok()
    );
    assert!(violations(&schema, &json!({ "path": "", "offset": 0 })).is_empty());
}

// ---- unknown and malformed schemas must not panic -----------------------------------------------

#[test]
fn an_unknown_type_is_a_schema_error_not_a_panic() {
    let schema = json!({ "type": "file" });
    let found = violation(&schema, &json!({}));
    assert_eq!(found.kind, FailureKind::ToolFailed);
    assert!(found.message.contains("file"), "{}", found.message);
    assert!(
        found.remedy.contains("schema"),
        "the remedy should point at the definition: {}",
        found.remedy
    );
}

#[test]
fn a_non_string_type_is_a_schema_error() {
    let found = violation(&json!({ "type": 7 }), &json!(1));
    assert_eq!(found.kind, FailureKind::ToolFailed);
    assert_non_empty_remedy(&found);
}

#[test]
fn a_malformed_schema_node_is_a_schema_error() {
    for schema in [json!("string"), json!(7), json!([1, 2])] {
        let found = violation(&schema, &json!("anything"));
        assert_eq!(found.kind, FailureKind::ToolFailed, "for {schema}");
        assert_non_empty_remedy(&found);
    }
}

#[test]
fn a_malformed_required_clause_is_a_schema_error() {
    let found = violation(&json!({ "type": "object", "required": "path" }), &json!({}));
    assert_eq!(found.kind, FailureKind::ToolFailed);
    assert_non_empty_remedy(&found);
}

#[test]
fn a_non_string_required_entry_is_a_schema_error() {
    let found = violation(&json!({ "type": "object", "required": [7] }), &json!({}));
    assert_eq!(found.kind, FailureKind::ToolFailed);
    assert_non_empty_remedy(&found);
}

#[test]
fn a_malformed_enum_clause_is_a_schema_error() {
    let found = violation(&json!({ "type": "string", "enum": "fast" }), &json!("fast"));
    assert_eq!(found.kind, FailureKind::ToolFailed);
    assert_non_empty_remedy(&found);
}

#[test]
fn tuple_form_items_is_a_schema_error() {
    let schema = json!({ "type": "array", "items": [{ "type": "string" }] });
    let found = violation(&schema, &json!(["a"]));
    assert_eq!(found.kind, FailureKind::ToolFailed);
    assert_non_empty_remedy(&found);
}

#[test]
fn a_deeply_nested_value_stops_at_the_cap_instead_of_overflowing() {
    let schema = json!({
        "type": "object",
        "properties": { "next": { "type": "object", "properties": { "next": { "type": "object" } } } }
    });
    // A value nested far past the cap, with a schema that keeps descending one level at a time.
    let mut deep_schema = json!({ "type": "object" });
    let mut deep_value = json!({ "x": 1 });
    for _ in 0..(MAX_DEPTH + 5) {
        deep_schema = json!({ "type": "object", "properties": { "x": deep_schema } });
        deep_value = json!({ "x": deep_value });
    }
    // Reaching the cap is a schema error, and either way nothing may panic.
    let found = violations(&deep_schema, &deep_value);
    assert!(!found.is_empty(), "a value past the cap should be reported");
    for violation in &found {
        assert_eq!(violation.kind, FailureKind::ToolFailed);
        assert_non_empty_remedy(violation);
    }
    // The shallow parts of the same tree still validate.
    let _ = violations(&schema, &json!({ "next": { "next": {} } }));
}

// ---- kinds and their codes ----------------------------------------------------------------------

#[test]
fn kinds_round_trip_through_their_codes() {
    for kind in FailureKind::ALL {
        assert_eq!(FailureKind::from_code(kind.code()), Some(kind));
        assert_eq!(kind.to_string(), kind.code());
    }
}

#[test]
fn codes_are_unique_and_uppercase() {
    let mut seen = std::collections::BTreeSet::new();
    for kind in FailureKind::ALL {
        let code = kind.code();
        assert!(seen.insert(code), "duplicate code {code}");
        assert_eq!(code, code.to_uppercase(), "code {code} should be uppercase");
        assert!(!code.contains(' '), "code {code} should be a single token");
    }
    assert_eq!(seen.len(), FailureKind::ALL.len());
}

#[test]
fn an_unknown_code_decodes_to_none() {
    assert_eq!(FailureKind::from_code("INVALID_ARGS"), None);
    assert_eq!(FailureKind::from_code(""), None);
    assert_eq!(FailureKind::from_code("invalid_arguments"), None);
}

#[test]
fn a_violation_renders_its_remedy() {
    let violation = Violation::new(FailureKind::NotFound, "no such file", "check the path");
    let rendered = violation.to_string();
    assert!(rendered.contains("NOT_FOUND"), "{rendered}");
    assert!(rendered.contains("no such file"), "{rendered}");
    assert!(rendered.contains("check the path"), "{rendered}");
}
