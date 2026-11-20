//! Tests for `core_final_gap`: the author schema DSL, the enforced-subset gate, and the value walk.
//!
//! The cases are upstream's, restated where its suites state them
//! (`packages/core/tools/tests/schema.spec.ts`, `packages/core/tools/tests/json-schema.spec.ts`),
//! plus the ones whose *reason* is the property under test rather than a recorded output.
//!
//! Every property below is a way for a schema pipeline to look correct while being wrong:
//!
//! - **the three layers stay distinct.** Compiling an author node and then validating a *value*
//!   against it are different languages; a test that only compiles would not notice the walk reading
//!   an author-only keyword, and a test that only walks would not notice the compiler emitting a
//!   schema the gate refuses.
//! - **`required: true` is hoisted, not copied.** The author spells requiredness per property; the
//!   grammar spells it per object. A compiler that left `required: true` on the property would produce
//!   a schema outside the subset, and one that dropped it would silently make every argument optional.
//! - **openness is explicit.** An object node without `additionalProperties` is an author error, so no
//!   nested object can acquire JSON Schema's open default by omission.
//! - **`oneOf` matches exactly one.** "At least one" is the easy misreading and it accepts a value the
//!   author explicitly disambiguated.
//! - **the root label convention holds.** An empty path names the argument root, so a tool's first
//!   diagnostic reads `missing required property "path"` — not `"arguments.path"`, which a model
//!   reading tool arguments would not recognise.
//! - **detaching is checked.** A projection that copies parameters must refuse an integer a
//!   JavaScript client would corrupt; reading in place must still work, because nothing is copied.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, so every
//! reference to the library goes through `girder_core::{...}`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic,
    // The four compile helpers take the spec by value so a call site can pass a `json!` literal
    // directly; taking a reference would put an `&` in front of every one of the ~40 cases for no
    // gain, since the helpers hand the value to a function that only reads it.
    clippy::needless_pass_by_value
)]

#[path = "../src/core/surface_tail.rs"]
pub mod core_final_gap;

use serde_json::{Value, json};

use core_final_gap::{
    INVALID_ARGS, MAX_SCHEMA_DEPTH, ProjectedToolSchema, ToolSchemaSource, UNSUPPORTED_SCHEMA,
    arg_violations, assert_object_schema, assert_supported_schema, check_args,
    compile_parameter_schema, compile_value_schema, project_tool_schema, validate_schema_value,
    validate_schema_value_at,
};

// ============================================================================================
// Helpers
// ============================================================================================

/// Compiles one author value schema, expecting success.
fn value_spec(spec: Value) -> Value {
    compile_value_schema(&spec).expect("the spec compiles")
}

/// Compiles an implicit parameter map, expecting success.
fn param_spec(spec: Value) -> Value {
    compile_parameter_schema(&spec).expect("the parameter map compiles")
}

/// The violations a value schema is refused with.
fn value_spec_violations(spec: Value) -> Vec<String> {
    compile_value_schema(&spec)
        .expect_err("the spec must be refused")
        .violations
}

/// The violations a parameter map is refused with.
fn param_violations(spec: Value) -> Vec<String> {
    compile_parameter_schema(&spec)
        .expect_err("the parameter map must be refused")
        .violations
}

// ============================================================================================
// The author DSL: value roots
// ============================================================================================

#[test]
fn every_value_root_compiles_and_the_author_json_node_becomes_annotation_only() {
    // The one place the DSL is not a subset of the grammar: `type: 'json'` has no raw spelling, so it
    // compiles to a node with no `type` — which the subset reads as unconstrained JSON.
    assert_eq!(
        value_spec(json!({"type": "string"})),
        json!({"type": "string"})
    );
    assert_eq!(
        value_spec(json!({"type": "number"})),
        json!({"type": "number"})
    );
    assert_eq!(
        value_spec(json!({"type": "integer"})),
        json!({"type": "integer"})
    );
    assert_eq!(
        value_spec(json!({"type": "boolean"})),
        json!({"type": "boolean"})
    );
    assert_eq!(value_spec(json!({"type": "null"})), json!({"type": "null"}));
    assert_eq!(
        value_spec(json!({"type": "array", "items": {"type": "json"}})),
        json!({"type": "array", "items": {}})
    );
    assert_eq!(
        value_spec(
            json!({"type": "json", "description": "anything", "title": "Any JSON",
            "default": null, "examples": [{"nested": true}]})
        ),
        json!({"description": "anything", "title": "Any JSON", "default": null,
            "examples": [{"nested": true}]}),
        "the author node keeps its annotations and loses only its type"
    );
    assert_eq!(
        value_spec(json!({"oneOf": [{"type": "string"}, {"type": "null"}]})),
        json!({"oneOf": [{"type": "string"}, {"type": "null"}]})
    );
}

#[test]
fn scalar_literals_are_copied_for_the_gate_to_check() {
    assert_eq!(
        value_spec(json!({"type": "string", "enum": ["a", "b"], "const": "a"})),
        json!({"type": "string", "enum": ["a", "b"], "const": "a"})
    );
    // A type-incorrect literal is not the compiler's error: the message names the grammar, because
    // that is the rule the author actually broke and the one the SDK renderers also rely on.
    assert_eq!(
        value_spec_violations(json!({"type": "number", "enum": ["1"]})),
        vec![String::from(
            "schema.enum must be a non-empty array of number values"
        )]
    );
    assert_eq!(
        value_spec_violations(json!({"type": "integer", "const": 1.5})),
        vec![String::from("schema.const must be a integer value")]
    );
    assert_eq!(
        value_spec_violations(json!({"type": "string", "enum": ["a"], "const": "b"})),
        vec![String::from(
            "schema.const must be one of schema.enum when both are declared"
        )]
    );
    assert_eq!(
        value_spec_violations(json!({"type": "string", "enum": "a"})),
        vec![String::from(
            "schema.enum must be a non-empty array of scalar values"
        )]
    );
}

#[test]
fn an_object_node_must_state_its_openness() {
    // The silent-behaviour-change guard: an object without `additionalProperties` is refused rather
    // than inheriting JSON Schema's open default.
    assert_eq!(
        value_spec_violations(json!({"type": "object"})),
        vec![String::from(
            "schema.additionalProperties must be explicitly true or false"
        )]
    );
    assert_eq!(
        value_spec_violations(json!({"type": "object", "additionalProperties": "yes"})),
        vec![String::from(
            "schema.additionalProperties must be explicitly true or false"
        )]
    );
    assert_eq!(
        value_spec(json!({"type": "object", "additionalProperties": false})),
        json!({"type": "object", "additionalProperties": false})
    );
    assert_eq!(
        value_spec(json!({"type": "object", "additionalProperties": true, "properties": {}})),
        json!({"type": "object", "additionalProperties": true, "properties": {}})
    );
}

#[test]
fn the_author_vocabulary_is_closed_per_node_kind() {
    // Each node kind admits only the keywords it can honour, so a typo is an author error instead of
    // a keyword that compiles and is then ignored.
    assert_eq!(
        value_spec_violations(json!({"type": "string", "extra": true})),
        vec![String::from(
            "schema.extra is not supported by the value schema DSL"
        )]
    );
    assert_eq!(
        value_spec_violations(json!({"type": "array", "items": {}})),
        vec![String::from(
            "schema.items.type must be string/number/integer/boolean/null/array/object/json, or use oneOf"
        )],
        "an empty node is not a value schema: the DSL has no annotation-only spelling"
    );
    assert_eq!(
        value_spec_violations(json!({"type": "array", "items": 42})),
        vec![String::from("schema.items must be a value schema object")]
    );
    assert_eq!(
        value_spec_violations(
            json!({"type": "string", "oneOf": [{"type": "string"}, {"type": "null"}]})
        ),
        vec![String::from("schema cannot declare both type and oneOf")]
    );
    assert_eq!(
        value_spec_violations(json!({"type": "tuple"})),
        vec![String::from(
            "schema.type must be string/number/integer/boolean/null/array/object/json, or use oneOf"
        )]
    );
    assert_eq!(
        value_spec_violations(json!({})),
        vec![String::from(
            "schema.type must be string/number/integer/boolean/null/array/object/json, or use oneOf"
        )]
    );
    assert_eq!(
        value_spec_violations(json!(null)),
        vec![String::from("schema must be a value schema object")]
    );
    // A one-branch union is compiled and then refused by the gate: upstream's compiler checks that
    // `oneOf` is an array but not how long it is, so the grammar owns the minimum.
    assert_eq!(
        value_spec_violations(json!({"oneOf": [{"type": "string"}]})),
        vec![String::from(
            "schema.oneOf must be an array of at least two schemas"
        )]
    );
    // A non-array is an author error, because it cannot be compiled at all.
    assert_eq!(
        value_spec_violations(json!({"oneOf": "not-an-array"})),
        vec![String::from(
            "schema.oneOf must be an array of at least two value schemas"
        )]
    );
    // `required` belongs to a property, not to a value root: a root has no enclosing object.
    assert_eq!(
        value_spec_violations(json!({"type": "string", "required": true})),
        vec![String::from(
            "schema.required is not supported by the value schema DSL"
        )]
    );
}

// ============================================================================================
// The author DSL: the implicit parameter root
// ============================================================================================

#[test]
fn requiredness_is_hoisted_from_property_to_object() {
    // The DSL spells it per property, the grammar per object. Both levels are asserted, because a
    // compiler can get exactly one of them right.
    let schema = param_spec(json!({
        "closed": {
            "type": "object",
            "additionalProperties": false,
            "required": true,
            "properties": {"id": {"type": "integer", "required": true}},
        },
        "open": {"type": "object", "additionalProperties": true},
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "closed": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"id": {"type": "integer"}},
                    "required": ["id"],
                },
                "open": {"type": "object", "additionalProperties": true},
            },
            "required": ["closed"],
        })
    );
    // The implicit root stays open — the property that makes an undeclared key merely undeclared
    // rather than rejected — and the hoisted `required` lists names sorted, because the local map is
    // a `BTreeMap` where upstream's is insertion-ordered.
    assert_eq!(schema.get("additionalProperties"), None);
}

#[test]
fn the_root_openness_survives_when_only_required_properties_exist() {
    let schema = param_spec(json!({"path": {"type": "string", "required": true}}));
    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        })
    );
    // An empty map compiles to an open object with no `required`: a tool that takes no arguments.
    assert_eq!(
        param_spec(json!({})),
        json!({"type": "object", "properties": {}})
    );
}

#[test]
fn requiredness_must_be_true_or_absent() {
    assert_eq!(
        param_violations(json!({"value": {"type": "string", "required": false}})),
        vec![String::from(
            "parameters.value.required must be true when present"
        )]
    );
    assert_eq!(
        param_violations(json!({"value": {"type": "string", "required": "yes"}})),
        vec![String::from(
            "parameters.value.required must be true when present"
        )]
    );
    assert_eq!(
        param_violations(json!({"bad": 42})),
        vec![String::from("parameters.bad must be a value schema object")]
    );
    assert_eq!(
        param_violations(json!(null)),
        vec![String::from(
            "parameters must be an object of value schemas"
        )]
    );
}

#[test]
fn a_compiled_parameter_root_is_always_inside_the_subset() {
    // The compiler gates its own output, so a compiled schema can never be one the SDK renderers
    // would refuse. This is the invariant that keeps the three layers from drifting.
    for spec in [
        json!({}),
        json!({"a": {"type": "json"}}),
        json!({"a": {"type": "array", "items": {"type": "string"}}}),
        json!({"a": {"oneOf": [{"type": "string"}, {"type": "integer"}]}}),
        json!({"a": {"type": "object", "additionalProperties": false,
            "properties": {"b": {"type": "null"}}}}),
    ] {
        let schema = param_spec(spec.clone());
        assert_supported_schema(&schema).unwrap_or_else(|error| {
            panic!("{spec} compiled to a schema outside the subset: {error}")
        });
    }
}

// ============================================================================================
// The subset gate
// ============================================================================================

#[test]
fn the_gate_accepts_every_supported_node() {
    for schema in [
        json!({"type": "string"}),
        json!({"type": "object", "properties": {"free": {}}}),
        json!({"type": "array", "items": {"type": "string"}}),
        json!({"oneOf": [{"type": "string"}, {"type": "number"}]}),
        json!({"description": "any JSON", "title": "JSON"}),
        json!({}),
    ] {
        assert_supported_schema(&schema).unwrap_or_else(|error| panic!("{schema}: {error}"));
    }
}

#[test]
fn the_gate_collects_every_violation_in_walk_order() {
    let error = assert_supported_schema(&json!({
        "type": "object",
        "pattern": "x",
        "properties": {"a": {"type": "weird"}, "b": {"type": "string", "minimum": 1}},
    }))
    .expect_err("three independent problems");
    assert_eq!(error.code, UNSUPPORTED_SCHEMA);
    assert_eq!(error.violations.len(), 3, "{:?}", error.violations);
    // The report is one sentence, joined the way upstream joins it.
    assert!(error.message().starts_with("unsupported JSON schema: "));
    assert!(error.message().contains("; "));
}

#[test]
fn the_gate_rejects_keywords_the_harness_cannot_enforce() {
    for keyword in [
        "anyOf",
        "allOf",
        "not",
        "pattern",
        "minimum",
        "maxLength",
        "$ref",
    ] {
        let schema = json!({"type": "object", "additionalProperties": true,
            "properties": {}, keyword: []});
        let error = assert_supported_schema(&schema).expect_err(keyword);
        assert!(
            error.violations.iter().any(|violation| violation
                .contains(&format!("schema.{keyword} is not a supported keyword"))),
            "{keyword}: {:?}",
            error.violations
        );
    }
    // A keyword on the wrong type is refused rather than accepted and ignored.
    let error = assert_supported_schema(&json!({"type": "object", "items": {}}))
        .expect_err("items on an object");
    assert_eq!(
        error.violations,
        vec![String::from(
            "schema.items is not supported on type \"object\""
        )]
    );
}

#[test]
fn an_object_root_is_required_only_where_the_consumer_needs_one() {
    assert_eq!(
        assert_object_schema(&json!({"type": "object", "additionalProperties": true})),
        Ok(())
    );
    // A grammar-valid but non-object root fails with the single root violation, naming why.
    for schema in [
        json!({}),
        json!({"type": "string"}),
        json!({"oneOf": [{"type": "string"}, {"type": "null"}]}),
    ] {
        let error = assert_object_schema(&schema).expect_err("not object-rooted");
        assert_eq!(
            error.violations,
            vec![String::from(
                "schema.type must be \"object\" (structured output is object-rooted)"
            )]
        );
    }
    // When both are wrong, the grammar violations come first: fixing them is the prerequisite.
    let error =
        assert_object_schema(&json!({"type": "string", "pattern": "x"})).expect_err("both wrong");
    assert!(error.violations[0].contains("is not a supported keyword"));
}

#[test]
fn the_gate_refuses_a_schema_deeper_than_the_walk_can_descend() {
    // Both walks are recursive here, so depth is capped rather than allowed to overflow the stack.
    let mut schema = json!({"type": "string"});
    for _ in 0..=MAX_SCHEMA_DEPTH {
        schema = json!({"oneOf": [schema, {"type": "null"}]});
    }
    let error = assert_supported_schema(&schema).expect_err("too deep");
    assert!(
        error
            .violations
            .iter()
            .any(|violation| violation.contains("nests deeper than")),
        "{:?}",
        error.violations
    );
}

// ============================================================================================
// The value walk: scalars
// ============================================================================================

#[test]
fn each_type_accepts_only_its_own_values() {
    let cases: [(Value, Value, &str); 8] = [
        (json!({"type": "string"}), json!("x"), ""),
        (json!({"type": "number"}), json!(1.5), ""),
        (json!({"type": "integer"}), json!(2), ""),
        (json!({"type": "boolean"}), json!(true), ""),
        (json!({"type": "null"}), json!(null), ""),
        (
            json!({"type": "string"}),
            json!(1),
            "\"value\" must be a string",
        ),
        (
            json!({"type": "number"}),
            json!("1"),
            "\"value\" must be a number",
        ),
        (json!({"type": "null"}), json!(0), "\"value\" must be null"),
    ];
    for (schema, value, expected) in cases {
        let found = validate_schema_value(&schema, &value);
        if expected.is_empty() {
            assert!(found.is_empty(), "{schema} / {value}: {found:?}");
        } else {
            assert_eq!(found, vec![String::from(expected)], "{schema} / {value}");
        }
    }
}

#[test]
fn an_integer_is_a_number_with_no_fractional_part() {
    // `1.0` is an integer in JavaScript, and a JSON parse of `1.0` produces it; an author who wrote
    // `integer` must accept it.
    assert!(validate_schema_value(&json!({"type": "integer"}), &json!(1.0)).is_empty());
    assert_eq!(
        validate_schema_value(&json!({"type": "integer"}), &json!(1.5)),
        vec![String::from("\"value\" must be an integer")]
    );
    assert_eq!(
        validate_schema_value(&json!({"type": "integer"}), &json!("1")),
        vec![String::from("\"value\" must be an integer")]
    );
}

#[test]
fn scalar_literals_are_enforced_and_enum_short_circuits() {
    let schema = json!({"type": "string", "enum": ["a", "b"], "const": "a"});
    assert!(validate_schema_value(&schema, &json!("a")).is_empty());
    assert_eq!(
        validate_schema_value(&schema, &json!("c")),
        vec![String::from("\"value\" must be one of [\"a\",\"b\"]")]
    );
    // `b` is inside the enum but wrong for the const; only the const mismatch is reported.
    assert_eq!(
        validate_schema_value(&schema, &json!("b")),
        vec![String::from("\"value\" must be \"a\"")]
    );
    // Outside the enum, the enum is the diagnostic and the const is not also reported: two failures
    // for one value would describe a shape the value was never meant to have.
    let both = json!({"type": "string", "enum": ["a"], "const": "a"});
    assert_eq!(
        validate_schema_value(&both, &json!("z")),
        vec![String::from("\"value\" must be one of [\"a\"]")]
    );
}

#[test]
fn numbers_compare_by_value_not_by_text_form() {
    // `1` and `1.0` are the same number in JavaScript while `serde_json` keeps their text apart, so
    // an author's `const: 1` must accept a model that sent `1.0`.
    let schema = json!({"type": "number", "const": 1});
    assert!(validate_schema_value(&schema, &json!(1.0)).is_empty());
    assert!(
        validate_schema_value(&json!({"type": "number", "enum": [1, 2]}), &json!(2.0)).is_empty()
    );
    assert_eq!(
        validate_schema_value(&json!({"type": "number", "const": 1}), &json!(2)),
        vec![String::from("\"value\" must be 1")]
    );
}

// ============================================================================================
// The value walk: containers
// ============================================================================================

#[test]
fn requiredness_nested_values_and_openness_are_reported_together() {
    let schema = json!({
        "type": "object",
        "properties": {
            "file": {"type": "string"},
            "nested": {
                "type": "object",
                "properties": {"line": {"type": "integer"}},
                "required": ["line"],
                "additionalProperties": false,
            },
        },
        "required": ["file"],
    });
    assert!(
        validate_schema_value(
            &schema,
            &json!({"file": "a", "extra": [1], "nested": {"line": 2}})
        )
        .is_empty()
    );
    assert_eq!(
        validate_schema_value(&schema, &json!({"nested": {"line": 1}})),
        vec![String::from("missing required property \"value.file\"")]
    );
    // Both the type failure and the missing nested name are reported, in walk order: declared
    // properties before the required names of the objects they contain, which is upstream's order.
    assert_eq!(
        validate_schema_value(&schema, &json!({"file": 1, "nested": {}})),
        vec![
            String::from("\"value.file\" must be a string"),
            String::from("missing required property \"value.nested.line\""),
        ]
    );
    assert_eq!(
        validate_schema_value(
            &schema,
            &json!({"file": "a", "nested": {"line": 1, "extra": true}})
        ),
        vec![String::from(
            "\"value.nested.extra\" is not a declared property (additionalProperties: false)"
        )]
    );
    assert_eq!(
        validate_schema_value(&schema, &json!("x")),
        vec![String::from("\"value\" must be an object")]
    );
}

#[test]
fn arrays_are_validated_per_index() {
    let schema = json!({"type": "array", "items": {"type": "integer"}});
    assert!(validate_schema_value(&schema, &json!([1, 2])).is_empty());
    assert_eq!(
        validate_schema_value(&schema, &json!([1, 1.5])),
        vec![String::from("\"value[1]\" must be an integer")]
    );
    assert_eq!(
        validate_schema_value(&schema, &json!("x")),
        vec![String::from("\"value\" must be an array")]
    );
    // No `items` accepts any element: the array's shape is all that was declared.
    assert!(validate_schema_value(&json!({"type": "array"}), &json!([1, "x", null])).is_empty());
}

#[test]
fn one_of_requires_exactly_one_match() {
    let disjoint = json!({"oneOf": [{"type": "string"}, {"type": "number"}]});
    assert!(validate_schema_value(&disjoint, &json!("x")).is_empty());
    assert_eq!(
        validate_schema_value(&disjoint, &json!(null)),
        vec![String::from(
            "\"value\" must match exactly one oneOf branch (matched 0)"
        )]
    );
    // The misreading to guard: "at least one" accepts a value the author disambiguated.
    let overlap = json!({"oneOf": [{"type": "number"}, {"type": "integer"}]});
    assert_eq!(
        validate_schema_value(&overlap, &json!(1)),
        vec![String::from(
            "\"value\" must match exactly one oneOf branch (matched 2)"
        )]
    );
    assert!(validate_schema_value(&overlap, &json!(1.5)).is_empty());
    // A branch's own diagnostics never surface: only its verdict matters.
    let nested = json!({"oneOf": [
        {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
        {"type": "null"},
    ]});
    assert_eq!(
        validate_schema_value(&nested, &json!({"a": 1})),
        vec![String::from(
            "\"value\" must match exactly one oneOf branch (matched 0)"
        )]
    );
}

#[test]
fn an_unconstrained_schema_accepts_any_json_value() {
    // Upstream additionally requires lossless JSON here; every `Value` already is, which is the
    // documented deviation.
    for value in [
        json!(null),
        json!(true),
        json!(1),
        json!("x"),
        json!([1]),
        json!({"x": null}),
    ] {
        assert!(
            validate_schema_value(&json!({}), &value).is_empty(),
            "{value}"
        );
    }
}

#[test]
fn the_walk_reports_rather_than_descends_past_the_cap() {
    let mut schema = json!({"type": "string"});
    let mut value = json!("leaf");
    for _ in 0..=MAX_SCHEMA_DEPTH {
        schema = json!({"type": "array", "items": schema});
        value = Value::Array(vec![value]);
    }
    let found = validate_schema_value(&schema, &value);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("nests deeper than"), "{found:?}");
    // The cap is a report, not a panic and not a silent pass.
    assert!(found[0].contains("was not checked"));
}

// ============================================================================================
// Tool arguments: the root label convention
// ============================================================================================

#[test]
fn argument_diagnostics_name_the_parameter_directly() {
    let spec = json!({
        "path": {"type": "string", "required": true},
        "offset": {"type": "integer"},
    });
    assert!(
        arg_violations(&spec, &json!({"path": "/tmp/x"}))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        arg_violations(&spec, &json!({})).unwrap(),
        vec![String::from("missing required property \"path\"")]
    );
    assert_eq!(
        arg_violations(&spec, &json!({"path": "/tmp/x", "offset": "3"})).unwrap(),
        vec![String::from("\"offset\" must be an integer")]
    );
    // An undeclared key is merely undeclared: the implicit root is open.
    assert!(
        arg_violations(&spec, &json!({"path": "/tmp/x", "unknown": 1}))
            .unwrap()
            .is_empty()
    );
    // The explicit root label still applies to nested paths.
    assert_eq!(
        arg_violations(
            &json!({"opts": {"type": "object", "additionalProperties": false,
                "properties": {"deep": {"type": "string"}}}}),
            &json!({"opts": {"deep": 1}}),
        )
        .unwrap(),
        vec![String::from("\"opts.deep\" must be a string")]
    );
}

#[test]
fn checking_arguments_raises_invalid_args_with_every_violation() {
    let spec = json!({
        "a": {"type": "string", "required": true},
        "b": {"type": "integer", "required": true},
    });
    assert_eq!(check_args(&spec, &json!({"a": "x", "b": 2})), Ok(()));
    let error = check_args(&spec, &json!({})).expect_err("both are missing");
    assert_eq!(error.code, INVALID_ARGS);
    assert_eq!(
        error.violations,
        vec![
            String::from("missing required property \"a\""),
            String::from("missing required property \"b\""),
        ]
    );
    assert!(error.message().starts_with("invalid arguments: "));
}

#[test]
fn a_malformed_spec_is_an_author_error_not_an_argument_error() {
    // The distinction matters: a caller that treated this as `INVALID_ARGS` would tell the model to
    // fix arguments that could never be valid.
    let error = arg_violations(
        &json!({"x": {"type": "string", "required": false}}),
        &json!({}),
    )
    .expect_err("malformed spec");
    assert_eq!(error.code, UNSUPPORTED_SCHEMA);
    let error =
        check_args(&json!({"x": {"type": "weird"}}), &json!({})).expect_err("malformed spec");
    assert_eq!(error.code, UNSUPPORTED_SCHEMA);
}

#[test]
fn the_root_label_can_be_chosen_by_the_caller() {
    let schema = json!({"type": "object", "properties": {"x": {"type": "string"}},
        "additionalProperties": false});
    assert_eq!(
        validate_schema_value_at(&schema, &json!({"x": 1}), "args"),
        vec![String::from("\"args.x\" must be a string")]
    );
    // The empty label is the argument convention: the property name stands alone.
    assert_eq!(
        validate_schema_value_at(&schema, &json!({"x": 1}), ""),
        vec![String::from("\"x\" must be a string")]
    );
    // And naming a non-existent root still reads sensibly.
    assert_eq!(
        validate_schema_value_at(&schema, &json!("x"), ""),
        vec![String::from("\"arguments\" must be an object")]
    );
}

// ============================================================================================
// Schema projection
// ============================================================================================

#[test]
fn projection_strips_the_execution_half_and_keeps_the_flag() {
    let parameters = json!({"type": "object", "properties": {}});
    let source = ToolSchemaSource {
        name: "bash",
        description: "Run a shell command.",
        parameters: &parameters,
        defer_loading: true,
    };
    let projected: ProjectedToolSchema = project_tool_schema(&source, true).expect("lossless");
    assert_eq!(projected.name, "bash");
    assert_eq!(projected.description, "Run a shell command.");
    assert_eq!(projected.parameters, parameters);
    assert!(projected.defer_loading, "the flag is reported, not dropped");
}

#[test]
fn detaching_refuses_parameters_a_javascript_client_would_corrupt() {
    // Detaching is where the lossless check belongs: a snapshot that silently corrupted an integer
    // above 2^53 would advertise a schema the tool never had.
    let parameters = json!({"type": "object", "default": 9_007_199_254_740_993_u64});
    let source = ToolSchemaSource {
        name: "big",
        description: "Carries an unsafe integer.",
        parameters: &parameters,
        defer_loading: false,
    };
    let error = project_tool_schema(&source, true).expect_err("unsafe integer");
    assert_eq!(
        error.violations,
        vec![String::from(
            "tool \"big\" parameters must be lossless JSON before schema projection"
        )]
    );
    // Reading in place copies nothing, so nothing can be lost: upstream's shape exactly.
    assert!(project_tool_schema(&source, false).is_ok());
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod core {
    pub use super::core_final_gap as surface_tail;
}
