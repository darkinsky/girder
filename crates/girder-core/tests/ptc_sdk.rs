//! Tests for `ptc_sdk_gap`: the two PTC SDK projections — JSON Schema to Python and TypeScript type
//! text, and the `tools:sdk` prompt sections built from them.
//!
//! The cases are the upstream ones, restated where the upstream suites state them
//! (`tool-executor-ptc/tests/py-types.spec.ts`, `tool-executor-ptc/tests/ts-types.spec.ts`), plus
//! the ones whose *reason* is the property under test rather than a recorded output: a renderer that
//! never throws, a declaration that precedes its reference, an import line that lists only what ran.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}`.

#[path = "../src/ptc/sdk.rs"]
pub mod ptc_sdk_gap;

use serde_json::{Value, json};

use ptc_sdk_gap::{
    MAX_CLASS_NAME_BASE, MAX_LIST_NESTING, ProgramFormat, ProseTarget, SdkTool, SubsetError,
    TypingSymbol, camel_case, cap_class_name_base, describe, is_bare_identifier,
    is_python_reserved, is_supported, json_schema_to_py, json_schema_to_ts, python_literal, quote,
    render_tools_sdk, render_tools_sdk_py, typescript_literal, validate_subset,
};

// ============================================================================================
// Helpers
// ============================================================================================

/// The rendered section's `python` code fence contents, with the fixed instructions removed.
fn python_declaration(text: &str) -> String {
    let start = text.find("```python\n").expect("a python code fence") + "```python\n".len();
    let end = text.rfind("\n```").expect("a closing fence");
    text[start..end].to_owned()
}

/// The rendered section's `ts` code fence contents, with the fixed instructions removed.
fn ts_declaration(text: &str) -> String {
    let start = text.find("```ts\n").expect("a ts code fence") + "```ts\n".len();
    let end = text.rfind("\n```").expect("a closing fence");
    text[start..end].to_owned()
}

/// A tool with one required `command: string` argument, like upstream's `bash` fixture.
fn bash_tool() -> SdkTool {
    SdkTool::new(
        "bash",
        "Run a shell command.",
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"],
        }),
        json!({ "type": "string" }),
    )
}

// ============================================================================================
// The enforced subset
// ============================================================================================

#[test]
fn the_subset_accepts_every_construct_the_renderers_support() {
    let accepted = [
        json!({ "type": "string" }),
        json!({ "type": "number" }),
        json!({ "type": "integer" }),
        json!({ "type": "boolean" }),
        json!({ "type": "null" }),
        json!({ "type": "array" }),
        json!({ "type": "array", "items": { "type": "number" } }),
        json!({ "type": "object" }),
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "object", "additionalProperties": false }),
        json!({ "oneOf": [{ "type": "string" }, { "type": "null" }] }),
        json!({ "type": "string", "enum": ["a", "b"] }),
        json!({ "type": "integer", "const": 2 }),
        json!({ "type": "null", "const": null }),
        // Annotations are ignored for validation and may sit on any node.
        json!({ "type": "string", "description": "d", "title": "t", "default": 1, "examples": [] }),
        // An annotation-only schema is the unconstrained-JSON form.
        json!({ "description": "anything" }),
        json!({}),
    ];
    for schema in accepted {
        assert!(
            is_supported(&schema),
            "should be supported: {schema}\n{:#?}",
            validate_subset(&schema)
        );
    }
}

#[test]
fn the_subset_rejects_every_keyword_and_placement_it_documents() {
    let rejected = [
        // Keywords outside the subset, including the ones a Remote schema would legitimately use.
        json!({ "type": "string", "$ref": "#/$defs/x" }),
        json!({ "type": "string", "format": "date" }),
        json!({ "type": "number", "minimum": 0 }),
        json!({ "type": "string", "minLength": 1 }),
        // A misplaced constraint keyword.
        json!({ "type": "string", "properties": {} }),
        json!({ "type": "object", "items": { "type": "string" } }),
        json!({ "type": "array", "properties": {} }),
        // A keyword with no type or oneOf to attach to.
        json!({ "properties": {} }),
        json!({ "required": [] }),
        // type and oneOf are exclusive, and oneOf may not carry constraint siblings.
        json!({ "type": "string", "oneOf": [{ "type": "string" }, { "type": "null" }] }),
        json!({
            "oneOf": [{ "type": "string" }, { "type": "null" }],
            "description": "ok but",
            "properties": {},
        }),
        // A oneOf needs two branches; a type array is unsupported.
        json!({ "oneOf": [] }),
        json!({ "oneOf": [{ "type": "string" }] }),
        json!({ "type": ["string", "null"] }),
        json!({ "type": "int" }),
        // A required name that no property declares can never be satisfied.
        json!({ "type": "object", "properties": {}, "required": ["missing"] }),
        json!({ "type": "object", "required": [7] }),
        // A literal that does not match its type, or an empty enum.
        json!({ "type": "string", "enum": [1, 2] }),
        json!({ "type": "string", "enum": [] }),
        json!({ "type": "string", "const": 3 }),
        json!({ "type": "string", "enum": ["a"], "const": "b" }),
        // Not a schema at all.
        json!(null),
        json!(42),
        json!("schema"),
        json!([]),
    ];
    for schema in rejected {
        assert!(!is_supported(&schema), "should be rejected: {schema}");
    }
}

#[test]
fn a_subset_error_names_every_violation_with_its_path() {
    let schema = json!({
        "type": "object",
        "properties": {
            "bad": { "type": "string", "$ref": "x" },
            "worse": { "type": "number", "minimum": 0 },
        },
        "required": ["missing"],
    });
    let error = validate_subset(&schema).expect_err("two keywords and one required name");
    assert!(error.violations.len() >= 3, "{:#?}", error.violations);
    let joined = error.violations.join(" ");
    assert!(joined.contains("schema.properties.bad.$ref"), "{joined}");
    assert!(
        joined.contains("schema.properties.worse.minimum"),
        "{joined}"
    );
    assert!(joined.contains("\"missing\""), "{joined}");
    assert_eq!(
        error.to_string(),
        format!("unsupported JSON schema: {}", error.violations.join("; "))
    );
    // The error is a real `Error`, so a caller can box it rather than formatting it.
    let boxed: Box<dyn std::error::Error> = Box::new(SubsetError::new(vec!["x".to_owned()]));
    assert!(boxed.to_string().contains('x'));
}

#[test]
fn an_integer_literal_accepts_a_whole_float_and_rejects_a_fraction() {
    assert!(is_supported(&json!({ "type": "integer", "const": 2 })));
    assert!(is_supported(&json!({ "type": "integer", "enum": [2.0] })));
    assert!(!is_supported(&json!({ "type": "integer", "const": 2.5 })));
    // `number` takes both, since a JSON number is one type on the wire.
    assert!(is_supported(&json!({ "type": "number", "const": 2.5 })));
    assert!(is_supported(&json!({ "type": "number", "const": 2 })));
}

#[test]
fn a_schema_nested_past_the_walk_bound_is_refused_rather_than_overflowing() {
    let mut schema = json!({ "type": "string" });
    for _ in 0..(ptc_sdk_gap::MAX_SCHEMA_DEPTH + 2) {
        schema = json!({ "type": "array", "items": schema });
    }
    let error = validate_subset(&schema).expect_err("too deep");
    assert!(
        error
            .violations
            .iter()
            .any(|violation| violation.contains("nests deeper")),
        "{:#?}",
        error.violations
    );
}

// ============================================================================================
// Agreement with the crate's other schema vocabulary
// ============================================================================================

#[test]
fn this_subsets_type_names_are_the_ones_tool_contracts_enforces() {
    // The two gates answer different questions — `tool_contracts` validates arguments against a
    // schema, this module validates a schema against the grammar the SDK text can express — but
    // they must agree on what the seven `type` spellings ARE. A spelling this module accepted and
    // `tool_contracts` rejected (or vice versa) would mean a tool whose arguments validate but
    // whose generated types say nothing, or the reverse.
    let samples = [
        ("object", json!({})),
        ("array", json!([])),
        ("string", json!("x")),
        ("number", json!(1.5)),
        ("integer", json!(1)),
        ("boolean", json!(true)),
        ("null", json!(null)),
    ];
    for (type_name, sample) in samples {
        let schema = json!({ "type": type_name });
        assert!(
            is_supported(&schema),
            "{type_name} should be in this subset"
        );
        let violations = girder_core::validate_tool_arguments(&schema, &sample);
        assert!(
            violations.is_ok(),
            "tool_contracts should accept a {type_name} sample: {violations:?}"
        );
    }
}

#[test]
fn a_type_name_neither_vocabulary_knows_is_refused_by_both() {
    // The drift this pins is the interesting direction: a name invented on one side and not the
    // other. Both must refuse it, so an authoring mistake cannot pass one gate and fail the next.
    for type_name in ["int", "float", "dict", "list", "str", "bool", "any"] {
        let schema = json!({ "type": type_name });
        assert!(
            !is_supported(&schema),
            "{type_name} should not be in this subset"
        );
        let violations =
            girder_core::validate_tool_arguments(&schema, &json!({})).expect_err(type_name);
        assert!(
            violations.to_string().contains("unsupported type"),
            "tool_contracts should call {type_name} unsupported: {violations}"
        );
    }
}

#[test]
fn an_object_shape_means_the_same_thing_to_both_gates() {
    // `required` and `additionalProperties` are the two keywords whose MEANING the two gates share,
    // so a schema accepted by this module has to be enforced the same way by `tool_contracts`.
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "command": { "type": "string" }, "limit": { "type": "integer" } },
        "required": ["command"],
    });
    assert!(is_supported(&schema));
    // A call satisfying the schema validates.
    assert!(girder_core::validate_tool_arguments(&schema, &json!({ "command": "ls" })).is_ok());
    // A missing required field does not.
    assert!(girder_core::validate_tool_arguments(&schema, &json!({ "limit": 1 })).is_err());
    // An undeclared key does not, which is what `additionalProperties: false` means on both sides.
    assert!(
        girder_core::validate_tool_arguments(&schema, &json!({ "command": "ls", "extra": 1 }))
            .is_err()
    );
}

// ============================================================================================
// JSON Schema → Python
// ============================================================================================

#[test]
fn json_schema_to_py_maps_every_supported_construct() {
    let cases: [(Value, &str); 13] = [
        (json!({ "type": "string" }), "str"),
        (json!({ "type": "number" }), "float"),
        (json!({ "type": "integer" }), "int"),
        (json!({ "type": "boolean" }), "bool"),
        (json!({ "type": "null" }), "None"),
        (
            json!({ "type": "string", "enum": ["a", "b"] }),
            r#"Literal["a", "b"]"#,
        ),
        (
            json!({ "type": "string", "const": "fixed" }),
            r#"Literal["fixed"]"#,
        ),
        (json!({ "type": "boolean", "const": true }), "Literal[True]"),
        (json!({ "type": "number", "const": 1.5 }), "Literal[1.5]"),
        (
            json!({ "type": "boolean", "enum": [false] }),
            "Literal[False]",
        ),
        (
            json!({ "type": "array", "items": { "type": "number" } }),
            "list[float]",
        ),
        (
            json!({ "type": "array", "items": { "type": "string", "enum": ["x", "y"] } }),
            r#"list[Literal["x", "y"]]"#,
        ),
        (json!({ "type": "array" }), "list[Any]"),
    ];
    for (schema, expected) in cases {
        assert_eq!(json_schema_to_py(&schema), expected, "{schema}");
    }
}

#[test]
fn json_schema_to_py_degrades_an_object_to_a_plain_dict() {
    // Naming a `TypedDict` needs a render context to declare into, and the context-free entry point
    // has none of its own — so every object shape here is opaque, including one WITH properties.
    for schema in [
        json!({ "type": "object" }),
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "object", "properties": { "x": { "type": "string" } } }),
    ] {
        assert_eq!(json_schema_to_py(&schema), "dict[str, Any]", "{schema}");
    }
}

#[test]
fn json_schema_to_py_is_total_over_hostile_input() {
    let hostile = [
        json!(null),
        json!(42),
        json!("string-schema"),
        json!({}),
        json!({ "oneOf": 7 }),
        json!({ "oneOf": [] }),
        json!({ "oneOf": [{ "type": "string" }] }),
        json!({ "$ref": "#/defs/x" }),
        json!({ "type": "object", "properties": 7 }),
        json!({ "type": "string", "enum": [1, 2] }),
        json!({ "type": "string", "enum": [] }),
        Value::Null,
    ];
    for schema in hostile {
        assert_eq!(json_schema_to_py(&schema), "Any", "{schema}");
    }
    assert_eq!(
        json_schema_to_py(&json!({ "oneOf": [{ "type": "string" }, { "type": "null" }] })),
        "str | None"
    );
}

#[test]
fn json_schema_to_py_renders_a_union_of_objects_without_names() {
    // Both branches are context-free, so each degrades independently rather than sharing a name.
    let schema = json!({
        "oneOf": [
            { "type": "object", "properties": { "a": { "type": "string" } } },
            { "type": "object", "properties": { "b": { "type": "number" } } },
        ]
    });
    assert_eq!(
        json_schema_to_py(&schema),
        "dict[str, Any] | dict[str, Any]"
    );
}

#[test]
fn json_schema_to_py_caps_list_nesting_at_the_interpreter_bracket_limit() {
    // Exactly at the cap every level is still emitted, so the innermost type is exact.
    let mut at_cap = json!({ "type": "string" });
    for _ in 0..MAX_LIST_NESTING {
        at_cap = json!({ "type": "array", "items": at_cap });
    }
    let rendered = json_schema_to_py(&at_cap);
    assert_eq!(rendered.matches("list[").count(), MAX_LIST_NESTING);
    assert!(rendered.starts_with("list[list["));
    assert!(rendered.ends_with(&format!("list[str{}", "]".repeat(MAX_LIST_NESTING))));

    // One level deeper and the extra `list[` would pass the bracket limit: that innermost node
    // degrades to `Any`, while every enclosing level still renders.
    let past_cap = json!({ "type": "array", "items": at_cap });
    let rendered = json_schema_to_py(&past_cap);
    assert_eq!(rendered.matches("list[").count(), MAX_LIST_NESTING);
    assert!(rendered.ends_with(&format!("list[Any{}", "]".repeat(MAX_LIST_NESTING))));
}

#[test]
fn json_schema_to_py_escapes_a_literal_so_it_stays_parseable() {
    // The two code points `CPython` refuses anywhere in source: NUL, and the C0 controls.
    assert_eq!(
        json_schema_to_py(&json!({ "type": "string", "const": "a\u{0}b" })),
        r#"Literal["a\u0000b"]"#
    );
    // A quote and a backslash would otherwise close (or eat) the literal early.
    assert_eq!(
        json_schema_to_py(&json!({ "type": "string", "const": "a\"b\\c" })),
        r#"Literal["a\"b\\c"]"#
    );
    // A newline would end the line before its terminator.
    assert_eq!(
        json_schema_to_py(&json!({ "type": "string", "const": "a\nb" })),
        r#"Literal["a\nb"]"#
    );
    // NEL and the line/paragraph separators are legal raw in a Python literal: they split a string
    // at run time but do not end a physical line.
    let rendered = json_schema_to_py(&json!({ "type": "string", "const": "a\u{85}b\u{2028}c" }));
    assert!(rendered.contains('\u{85}'), "{rendered}");
    assert!(rendered.contains('\u{2028}'), "{rendered}");
}

#[test]
fn json_schema_to_py_keeps_a_beyond_safe_integer_exact() {
    // `-2^60` has no exact `f64` neighbour in the digits a shortest-round-trip print would give, so
    // the integer path is the only one that names the value the schema declared.
    let schema = json!({ "type": "integer", "const": -1_152_921_504_606_846_976_i64 });
    assert_eq!(json_schema_to_py(&schema), "Literal[-1152921504606846976]");
    assert_eq!(python_literal(&json!(7)), "7");
    assert_eq!(python_literal(&json!(1.5)), "1.5");
    // A whole float keeps a float spelling, since `Literal[1]` would be a different value.
    assert_eq!(python_literal(&json!(1.0)), "1.0");
}

// ============================================================================================
// JSON Schema → TypeScript
// ============================================================================================

#[test]
fn json_schema_to_ts_maps_every_supported_construct() {
    let cases: [(Value, &str); 15] = [
        (json!({ "type": "string" }), "string"),
        (json!({ "type": "number" }), "number"),
        (json!({ "type": "integer" }), "number"),
        (json!({ "type": "boolean" }), "boolean"),
        (json!({ "type": "null" }), "null"),
        (
            json!({ "type": "string", "enum": ["a", "b"] }),
            r#""a" | "b""#,
        ),
        (json!({ "type": "number", "enum": [1, 2] }), "1 | 2"),
        (json!({ "type": "integer", "const": 2 }), "2"),
        (json!({ "type": "boolean", "const": true }), "true"),
        (json!({ "type": "null", "const": null }), "null"),
        (
            json!({ "type": "string", "enum": ["a", "b"], "const": "a" }),
            r#""a""#,
        ),
        (
            json!({ "oneOf": [{ "type": "string" }, { "type": "null" }] }),
            "string | null",
        ),
        (
            json!({ "type": "array", "items": { "type": "number" } }),
            "number[]",
        ),
        (
            json!({ "type": "array", "items": { "type": "string", "enum": ["x", "y"] } }),
            r#"("x" | "y")[]"#,
        ),
        (json!({ "type": "array" }), "JsonValue[]"),
    ];
    for (schema, expected) in cases {
        assert_eq!(json_schema_to_ts(&schema, 0), expected, "{schema}");
    }
}

#[test]
fn json_schema_to_ts_renders_an_empty_object_by_its_openness() {
    assert_eq!(
        json_schema_to_ts(&json!({ "type": "object" }), 0),
        "Record<string, JsonValue>"
    );
    assert_eq!(
        json_schema_to_ts(&json!({ "type": "object", "properties": {} }), 0),
        "Record<string, JsonValue>"
    );
    assert_eq!(
        json_schema_to_ts(
            &json!({ "type": "object", "additionalProperties": false }),
            0
        ),
        "Record<string, never>"
    );
    assert_eq!(
        json_schema_to_ts(
            &json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            0
        ),
        "Record<string, never>"
    );
}

#[test]
fn json_schema_to_ts_renders_required_and_optional_keys() {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "id": { "type": "integer" }, "label": { "type": "string" } },
        "required": ["id"],
    });
    assert_eq!(
        json_schema_to_ts(&schema, 0),
        ["{", "  id: number;", "  label?: string;", "}"].join("\n")
    );
}

#[test]
fn json_schema_to_ts_nests_objects_and_marks_openness_with_an_intersection() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Absolute file path" },
            "limit": { "type": "number" },
            "opts": {
                "type": "object",
                "additionalProperties": true,
                "properties": { "deep": { "type": "boolean" } },
                "required": ["deep"],
            },
        },
        "required": ["path"],
    });
    // Members are in key order: this workspace's `serde_json` sorts map keys. Upstream renders in
    // JSON insertion order, a difference that cannot lose a field — see the module documentation.
    assert_eq!(
        json_schema_to_ts(&schema, 0),
        [
            "{",
            "  limit?: number;",
            "  opts?: {",
            "    deep: boolean;",
            "  } & Record<string, JsonValue>;",
            "  /** Absolute file path */",
            "  path: string;",
            "} & Record<string, JsonValue>",
        ]
        .join("\n")
    );
}

#[test]
fn json_schema_to_ts_quotes_a_key_that_is_not_an_identifier() {
    let schema = json!({
        "type": "object",
        "properties": {
            "my-key": { "type": "string" },
            "2nd": { "type": "string" },
            "$ref": { "type": "string" },
        },
    });
    let rendered = json_schema_to_ts(&schema, 0);
    // A name with no identifier spelling keeps its exact bytes as a quoted key, so the emitted key
    // IS the JSON key the model has to pass.
    assert!(rendered.contains(r#""my-key"?: string;"#), "{rendered}");
    assert!(rendered.contains(r#""2nd"?: string;"#), "{rendered}");
    // `$` is a legal identifier start in TypeScript, so that one stays bare.
    assert!(rendered.contains("$ref?: string;"), "{rendered}");
}

#[test]
fn json_schema_to_ts_escapes_a_comment_closer_in_a_description() {
    let schema = json!({
        "type": "object",
        "properties": { "glob": { "type": "string", "description": "a pattern like packages/*/tool-*/ over here" } },
    });
    let rendered = json_schema_to_ts(&schema, 0);
    assert!(!rendered.contains("tool-*/ over"), "{rendered}");
    assert!(rendered.contains(r"tool-*\/ over"), "{rendered}");
}

#[test]
fn json_schema_to_ts_is_total_over_hostile_input() {
    for schema in [
        json!({ "oneOf": [] }),
        json!({ "oneOf": [{ "type": "string" }] }),
        json!({ "type": "object", "properties": 7 }),
        json!({ "type": "object", "properties": { "bad": { "$ref": "x" } } }),
        json!({ "type": "object", "properties": { "bad": { "$ref": "x" } }, "required": ["bad"] }),
        json!({ "type": "string", "enum": [1, 2] }),
        json!({ "type": "string", "enum": [] }),
        json!({ "type": "object", "properties": { "a": { "type": "string" } }, "required": [7] }),
        json!({ "type": "object", "properties": { "weird": 42 } }),
        json!(null),
        json!(42),
    ] {
        assert_eq!(json_schema_to_ts(&schema, 0), "unknown", "{schema}");
    }
    // An unconstrained schema is the open-JSON form rather than an error.
    assert_eq!(json_schema_to_ts(&json!({}), 0), "JsonValue");
}

#[test]
fn a_deeply_nested_union_renders_linearly() {
    let depth = 500;
    let mut schema = json!({ "type": "string" });
    for _ in 0..depth {
        schema = json!({ "oneOf": [schema, { "type": "null" }] });
    }
    let rendered = json_schema_to_ts(&schema, 0);
    assert_eq!(rendered, "string".to_owned() + &" | null".repeat(depth));
}

#[test]
fn a_deeply_nested_array_stops_at_the_bracket_limit_without_overflowing() {
    let mut schema = json!({ "type": "string" });
    for _ in 0..(MAX_LIST_NESTING + 40) {
        schema = json!({ "type": "array", "items": schema });
    }
    // Neither walk panics on the way down, and each stops at its own degradation point.
    let python = json_schema_to_py(&schema);
    assert_eq!(python.matches("list[").count(), MAX_LIST_NESTING);
    assert!(python.ends_with(&format!("list[Any{}", "]".repeat(MAX_LIST_NESTING))));
    // TypeScript has no bracket limit — nothing in its grammar bounds nesting — so the whole chain
    // renders, all the way down to the innermost `string`.
    let typescript = json_schema_to_ts(&schema, 0);
    assert_eq!(typescript.matches("[]").count(), MAX_LIST_NESTING + 40);
    assert!(typescript.starts_with("string[]"), "{}", &typescript[..12]);
}

// ============================================================================================
// Identifiers, class names, prose and literals
// ============================================================================================

#[test]
fn bare_identifiers_follow_the_python_grammar() {
    for name in ["path", "_private", "a1", "路径", "field_2"] {
        assert!(is_bare_identifier(name), "{name}");
    }
    for name in ["my-tool", "1st", "", "a b", "my.mcp", "$ref"] {
        assert!(!is_bare_identifier(name), "{name}");
    }
}

#[test]
fn hard_keywords_are_reserved_and_soft_ones_are_not() {
    for name in ["class", "lambda", "async", "None", "True", "__debug__"] {
        assert!(is_python_reserved(name), "{name}");
    }
    // Each soft keyword is special in exactly one syntactic position, so it is usable here.
    for name in ["match", "case", "type", "_"] {
        assert!(!is_python_reserved(name), "{name}");
    }
}

#[test]
fn camel_case_splits_and_prefixes_a_head_that_cannot_start_an_identifier() {
    assert_eq!(camel_case("bash"), "Bash");
    assert_eq!(camel_case("my-mcp.tool"), "MyMcpTool");
    assert_eq!(camel_case("read_image"), "ReadImage");
    // A digit head has no spelling as an identifier, so the prefix makes one.
    assert_eq!(camel_case("1st-tool"), "Tool1stTool");
    // Unicode survives rather than collapsing to the bare prefix.
    assert_eq!(camel_case("路径"), "路径");
    assert_eq!(camel_case(""), "Tool");
}

#[test]
fn class_name_bases_are_capped_by_character_count() {
    let long = "A".repeat(MAX_CLASS_NAME_BASE + 40);
    assert_eq!(
        cap_class_name_base(&long).chars().count(),
        MAX_CLASS_NAME_BASE
    );
    // An astral character is never split, because the cap counts characters rather than UTF-16 units.
    let astral: String = std::iter::repeat_n('\u{1D400}', MAX_CLASS_NAME_BASE + 5).collect();
    let capped = cap_class_name_base(&astral);
    assert_eq!(capped.chars().count(), MAX_CLASS_NAME_BASE);
    assert!(capped.chars().all(|character| character == '\u{1D400}'));
}

#[test]
fn descriptions_collapse_to_one_line_and_escape_what_cannot_be_emitted() {
    assert_eq!(
        describe(Some("  two\n  lines  "), ProseTarget::Python).as_deref(),
        Some("two lines")
    );
    // A description that documents nothing is absent, not an empty docstring.
    assert_eq!(describe(Some("   "), ProseTarget::Python), None);
    assert_eq!(describe(None, ProseTarget::Python), None);
    // NUL cannot appear in Python source at all.
    assert_eq!(
        describe(Some("nul\u{0}here"), ProseTarget::Python).as_deref(),
        Some("nul\\x00here")
    );
    // NEL is not ECMAScript whitespace, so it survives the collapse and is then escaped as a C1
    // control — the boundary is the category, not the single NUL case.
    assert_eq!(
        describe(Some("a\u{85}b"), ProseTarget::Python).as_deref(),
        Some("a\\x85b")
    );
    // The TypeScript flavor only has to protect a comment closer.
    assert_eq!(
        describe(Some("a */ b"), ProseTarget::TypeScript).as_deref(),
        Some("a */ b")
    );
}

#[test]
fn quotes_escape_only_the_overlap_of_both_languages() {
    assert_eq!(quote("plain"), r#""plain""#);
    assert_eq!(quote("a\"b"), r#""a\"b""#);
    assert_eq!(quote("a\\b"), r#""a\\b""#);
    assert_eq!(quote("a\nb\tc"), r#""a\nb\tc""#);
    assert_eq!(quote("\u{0}\u{1f}"), r#""\u0000\u001f""#);
    // DEL and the C1 controls are legal raw in both, so they stay readable.
    assert_eq!(quote("\u{7f}\u{85}"), "\"\u{7f}\u{85}\"");
}

#[test]
fn the_two_flavors_render_scalar_literals_differently_where_the_languages_do() {
    assert_eq!(python_literal(&json!(true)), "True");
    assert_eq!(typescript_literal(&json!(true)), "true");
    assert_eq!(python_literal(&json!(null)), "None");
    assert_eq!(typescript_literal(&json!(null)), "null");
    assert_eq!(python_literal(&json!("x")), r#""x""#);
    assert_eq!(typescript_literal(&json!("x")), r#""x""#);
}

#[test]
fn typing_symbols_render_in_the_fixed_import_order() {
    assert_eq!(TypingSymbol::Any.as_str(), "Any");
    assert_eq!(TypingSymbol::Literal.as_str(), "Literal");
    assert_eq!(TypingSymbol::NotRequired.as_str(), "NotRequired");
    assert_eq!(TypingSymbol::Protocol.as_str(), "Protocol");
    assert_eq!(TypingSymbol::TypedDict.as_str(), "TypedDict");
}

// ============================================================================================
// renderToolsSdkPy
// ============================================================================================

#[test]
fn every_tool_arg_and_output_object_is_a_named_typed_dict() {
    let text = render_tools_sdk_py(&[bash_tool()]);
    let declaration = python_declaration(&text);
    // The argument object is a named TypedDict, not an opaque dict.
    assert!(
        declaration.contains("class BashArgs(TypedDict):"),
        "{declaration}"
    );
    assert!(declaration.contains("    command: str"), "{declaration}");
    assert!(
        declaration.contains("async def bash(self, args: BashArgs) -> str:"),
        "{declaration}"
    );
    assert!(
        declaration.contains("class Tools(Protocol):"),
        "{declaration}"
    );
    assert!(
        declaration.contains("class ToolCallError(Exception):"),
        "{declaration}"
    );
    assert!(declaration.contains("tools: Tools"), "{declaration}");
}

#[test]
fn the_python_section_carries_the_contract_the_model_relies_on() {
    let text = render_tools_sdk_py(&[bash_tool()]);
    // Both halves of the binding boundary: `tools`/`ToolCallError` are bound, the TypedDicts are
    // not. Dropping either half turns a correct contract into a wrong one.
    assert!(text.contains(
        "exactly two of the names declared below are bound: `tools` and `ToolCallError`"
    ));
    assert!(text.contains("never `FooArgs(field=1)`, which raises `NameError`"));
    assert!(text.contains("MAY overlap under `asyncio.gather`"));
    assert!(text.contains("lossless JSON"));
    assert!(text.contains("```python"));
    // The program comes before its description, in the argument sentence.
    assert!(text.contains("`code`, the program to execute, and `description`"));
}

#[test]
fn a_required_field_is_plain_and_an_optional_one_is_not_required() {
    let tool = SdkTool::new(
        "search",
        "Search for text.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for." },
                "limit": { "type": "number", "description": "Max results." },
            },
            "required": ["query"],
        }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        declaration.contains("class SearchArgs(TypedDict):"),
        "{declaration}"
    );
    assert!(
        declaration.contains("    # What to search for."),
        "{declaration}"
    );
    assert!(declaration.contains("    query: str"), "{declaration}");
    assert!(declaration.contains("    # Max results."), "{declaration}");
    assert!(
        declaration.contains("    limit: NotRequired[float]"),
        "{declaration}"
    );
    // The import line lists exactly what ran: no `Any` here, since every type is concrete.
    assert!(
        declaration.contains("from typing import NotRequired, Protocol, TypedDict"),
        "{declaration}"
    );
}

#[test]
fn a_missing_required_array_makes_every_field_optional() {
    let tool = SdkTool::new(
        "all_optional",
        "No required array.",
        json!({ "type": "object", "properties": { "flag": { "type": "boolean" } } }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        declaration.contains("    flag: NotRequired[bool]"),
        "{declaration}"
    );
}

#[test]
fn an_enum_property_renders_as_a_literal_field_and_imports_literal() {
    let tool = SdkTool::new(
        "mode_tool",
        "Pick a mode.",
        json!({
            "type": "object",
            "properties": { "mode": { "type": "string", "enum": ["fast", "slow"] } },
            "required": ["mode"],
        }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        declaration.contains("class ModeToolArgs(TypedDict):"),
        "{declaration}"
    );
    assert!(
        declaration.contains(r#"    mode: Literal["fast", "slow"]"#),
        "{declaration}"
    );
    assert!(
        declaration.contains("from typing import Literal, Protocol, TypedDict"),
        "{declaration}"
    );
}

#[test]
fn a_nested_object_is_its_own_class_declared_before_its_parent() {
    let tool = SdkTool::new(
        "workflow",
        "Run a workflow.",
        json!({
            "type": "object",
            "properties": {
                "meta": {
                    "type": "object",
                    "additionalProperties": false,
                    "description": "Identity block.",
                    "properties": {
                        "name": { "type": "string", "description": "Short name." },
                        "phases": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": { "title": { "type": "string" } },
                                "required": ["title"],
                            },
                        },
                    },
                    "required": ["name"],
                },
            },
            "required": ["meta"],
        }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    let outer = declaration
        .find("class WorkflowArgsMeta(TypedDict):")
        .expect("outer class");
    let inner = declaration
        .find("class WorkflowArgsMetaPhases(TypedDict):")
        .expect("inner class");
    let parent = declaration
        .find("class WorkflowArgs(TypedDict):")
        .expect("parent class");
    // Declaration order equals reference order: a nested class precedes the class naming it.
    assert!(inner < outer, "inner before outer:\n{declaration}");
    assert!(outer < parent, "outer before parent:\n{declaration}");
    assert!(declaration.contains("    name: str"), "{declaration}");
    assert!(
        declaration.contains("    phases: NotRequired[list[WorkflowArgsMetaPhases]]"),
        "{declaration}"
    );
    assert!(declaration.contains("    title: str"), "{declaration}");
}

#[test]
fn a_one_of_of_objects_names_both_branches() {
    let tool = SdkTool::new(
        "union_tool",
        "Two shapes.",
        json!({
            "type": "object",
            "properties": {
                "value": {
                    "oneOf": [
                        { "type": "object", "additionalProperties": false, "properties": { "a": { "type": "string" } } },
                        { "type": "object", "additionalProperties": false, "properties": { "b": { "type": "number" } } },
                    ],
                },
            },
            "required": ["value"],
        }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        declaration.contains("class UnionToolArgsValue1(TypedDict):"),
        "{declaration}"
    );
    assert!(
        declaration.contains("class UnionToolArgsValue2(TypedDict):"),
        "{declaration}"
    );
    assert!(
        declaration.contains("    value: UnionToolArgsValue1 | UnionToolArgsValue2"),
        "{declaration}"
    );
}

#[test]
fn two_tools_that_camel_case_alike_get_distinct_class_names() {
    let first = SdkTool::new(
        "a-b",
        "First.",
        json!({ "type": "object", "properties": { "x": { "type": "string" } }, "required": ["x"] }),
        json!({ "type": "string" }),
    );
    let second = SdkTool::new(
        "a_b",
        "Second.",
        json!({ "type": "object", "properties": { "y": { "type": "string" } }, "required": ["y"] }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[first, second]));
    // Both CamelCase to `AB`, so the second takes a counter suffix rather than colliding.
    assert!(
        declaration.contains("class ABArgs(TypedDict):"),
        "{declaration}"
    );
    assert!(
        declaration.contains("class ABArgs2(TypedDict):"),
        "{declaration}"
    );
}

#[test]
fn a_reserved_or_exotic_tool_name_routes_to_subscript_access() {
    let exotic = SdkTool::new(
        "my-mcp.tool",
        "Exotic name.",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "string" }),
    );
    let reserved = SdkTool::new(
        "class",
        "Uses a reserved Python word.",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[exotic, reserved]));
    // Empty-property objects keep the opaque dict, since there is nothing to name.
    assert!(
        declaration.contains(r#"# tools["my-mcp.tool"](args: dict[str, Any]) -> str"#),
        "{declaration}"
    );
    assert!(
        declaration.contains(r#"# tools["class"](args: dict[str, Any]) -> str"#),
        "{declaration}"
    );
}

#[test]
fn an_underscore_leading_tool_name_also_routes_to_subscript_access() {
    // `_foo` IS a legal attribute; it is routed here because the forms that break split three ways.
    let tool = SdkTool::new(
        "_private",
        "Private.",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        declaration.contains(r#"# tools["_private"]"#),
        "{declaration}"
    );
}

#[test]
fn a_subscript_only_tool_set_still_gets_a_pass_body() {
    // A class body of only comments fails to parse, so `pass` is required.
    let tool = SdkTool::new(
        "x-1",
        "Only subscript.",
        json!({}),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        declaration.contains("class Tools(Protocol):\n    pass"),
        "{declaration}"
    );
}

#[test]
fn an_empty_tool_set_renders_a_minimal_declaration() {
    let declaration = python_declaration(&render_tools_sdk_py(&[]));
    assert_eq!(
        declaration,
        [
            "from typing import Protocol",
            "",
            "class ToolCallError(Exception):",
            "    toolName: str",
            "",
            "class Tools(Protocol):",
            "    pass",
            "",
            "tools: Tools",
        ]
        .join("\n")
    );
}

#[test]
fn a_docstring_is_the_first_statement_of_its_own_method() {
    let described = SdkTool::new(
        "weird",
        "A described tool.",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[described]));
    // The docstring follows its `async def`, not the class header and not the previous method.
    assert!(
        declaration.contains("    async def weird(self, args: dict[str, Any]) -> str:\n        \"\"\"A described tool.\"\"\""),
        "{declaration}"
    );
}

#[test]
fn a_description_less_method_gets_an_ellipsis_body() {
    let tool = SdkTool::new(
        "bare",
        "",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        declaration.contains("    async def bare(self, args: dict[str, Any]) -> str: ..."),
        "{declaration}"
    );
}

#[test]
fn a_docstring_escapes_quotes_and_backslashes() {
    let tool = SdkTool::new(
        "tricky",
        "ends with \" and \\",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        declaration.contains(r#"        """ends with \" and \\""""#),
        "{declaration}"
    );
}

#[test]
fn an_unprintable_character_in_a_description_is_escaped() {
    let tool = SdkTool::new(
        "control",
        "before\u{0}after",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "string" }),
    );
    let text = render_tools_sdk_py(&[tool]);
    // No raw NUL anywhere: CPython refuses source containing one, in a docstring or a comment.
    assert!(!text.contains('\u{0}'), "{text}");
    // The docstring path escapes the control, then doubles the backslash into a Python source escape,
    // so the text the reader sees is the escape rather than an invisible byte.
    assert!(text.contains(r#""""before\\x00after""""#), "{text}");
}

#[test]
fn an_object_whose_field_cannot_be_declared_degrades_whole() {
    let tool = SdkTool::new(
        "hostile",
        "Bad field name.",
        json!({
            "type": "object",
            "properties": { "not-a-name": { "type": "string" } },
            "required": ["not-a-name"],
        }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    // The whole object degrades, rather than describing some fields and not others.
    assert!(
        !declaration.contains("class HostileArgs(TypedDict):"),
        "{declaration}"
    );
    assert!(
        declaration.contains("async def hostile(self, args: dict[str, Any]) -> str:"),
        "{declaration}"
    );
}

#[test]
fn a_field_that_would_be_name_mangled_degrades_whole() {
    // `__token` would be declared as `_ClassName__token` inside class syntax, describing a
    // different JSON key than the registered schema.
    let tool = SdkTool::new(
        "mangled",
        "Name-mangling probe.",
        json!({ "type": "object", "properties": { "__token": { "type": "string" } }, "required": ["__token"] }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(
        !declaration.contains("class MangledArgs(TypedDict):"),
        "{declaration}"
    );
    // A dunder is NOT mangled, so the same shape with `__x__` keeps its named class.
    let dunder = SdkTool::new(
        "ok",
        "Dunder probe.",
        json!({ "type": "object", "properties": { "__x__": { "type": "string" } }, "required": ["__x__"] }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[dunder]));
    assert!(
        declaration.contains("class OkArgs(TypedDict):"),
        "{declaration}"
    );
}

#[test]
fn a_non_ascii_field_name_keeps_its_class_name_derived_from_it() {
    let tool = SdkTool::new(
        "localized",
        "Non-ASCII field.",
        json!({
            "type": "object",
            "properties": { "路径": { "type": "string", "description": "文件路径" } },
            "required": ["路径"],
        }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(declaration.contains("    # 文件路径"), "{declaration}");
    assert!(declaration.contains("    路径: str"), "{declaration}");
}

#[test]
fn a_soft_keyword_field_name_stays_a_typed_dict_field() {
    let tool = SdkTool::new(
        "soft",
        "Soft keyword field.",
        json!({ "type": "object", "properties": { "type": { "type": "string" } }, "required": ["type"] }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    assert!(declaration.contains("    type: str"), "{declaration}");
}

#[test]
fn an_open_object_states_its_openness_in_band() {
    let open_tool = SdkTool::new(
        "open_tool",
        "Open object.",
        json!({ "type": "object", "properties": { "a": { "type": "string" } }, "required": ["a"] }),
        json!({ "type": "string" }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[open_tool]));
    assert!(
        declaration.contains("# Additional keys beyond those declared are allowed."),
        "{declaration}"
    );
    // A closed object says nothing extra, and a closed EMPTY one declares an empty class.
    let closed = SdkTool::new(
        "closed_tool",
        "Closed empty object.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "a": { "type": "string" } },
            "required": ["a"],
        }),
        json!({ "type": "string" }),
    );
    let closed_declaration = python_declaration(&render_tools_sdk_py(&[closed]));
    assert!(
        !closed_declaration.contains("Additional keys"),
        "{closed_declaration}"
    );
    let empty_closed = SdkTool::new(
        "empty_closed",
        "No keys accepted.",
        json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        json!({ "type": "string" }),
    );
    let empty_declaration = python_declaration(&render_tools_sdk_py(&[empty_closed]));
    assert!(
        empty_declaration.contains("class EmptyClosedArgs(TypedDict):\n    pass"),
        "{empty_declaration}"
    );
    assert!(
        !empty_declaration.contains("Additional keys"),
        "{empty_declaration}"
    );
}

#[test]
fn the_python_section_orders_tools_lexicographically_and_is_deterministic() {
    let first = SdkTool::new("zzz", "Last.", json!({}), json!({ "type": "string" }));
    let second = SdkTool::new("aaa", "First.", json!({}), json!({ "type": "string" }));
    let forward = render_tools_sdk_py(&[first.clone(), second.clone()]);
    let reversed = render_tools_sdk_py(&[second.clone(), first.clone()]);
    assert_eq!(forward, reversed);
    let zzz = forward.find("async def zzz").expect("zzz");
    let aaa = forward.find("async def aaa").expect("aaa");
    assert!(aaa < zzz, "aaa before zzz");
}

#[test]
fn a_malformed_argument_schema_degrades_only_that_position() {
    let tool = SdkTool::new(
        "1st-tool",
        "Hostile-shape probe.",
        // Malformed: a `$ref` is outside the subset, so the validator rejects the schema whole and
        // the args position degrades.
        json!({ "type": "object", "properties": { "field": { "type": "string", "$ref": "x" } } }),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"],
        }),
    );
    let declaration = python_declaration(&render_tools_sdk_py(&[tool]));
    // The output still names a class, so one bad schema does not take the whole tool with it.
    assert!(
        declaration.contains(r#"# tools["1st-tool"](args: Any) -> Tool1stToolOutput"#),
        "{declaration}"
    );
    assert!(
        declaration.contains("class Tool1stToolOutput(TypedDict):"),
        "{declaration}"
    );
    assert!(declaration.contains("    ok: bool"), "{declaration}");
}

// ============================================================================================
// renderToolsSdk (TypeScript)
// ============================================================================================

#[test]
fn the_typescript_section_declares_both_maps_and_the_callable_binding() {
    let declaration = ts_declaration(&render_tools_sdk(
        &[bash_tool()],
        ProgramFormat::FunctionBody,
    ));
    assert!(
        declaration.contains("interface ToolArgsMap {"),
        "{declaration}"
    );
    assert!(
        declaration.contains("interface ToolOutputMap {"),
        "{declaration}"
    );
    assert!(declaration.contains("  bash: {"), "{declaration}");
    assert!(
        declaration.contains("type ToolName = keyof ToolOutputMap"),
        "{declaration}"
    );
    assert!(
        declaration.contains("declare const tools: {"),
        "{declaration}"
    );
    assert!(
        declaration.contains("declare class ToolCallError extends Error {"),
        "{declaration}"
    );
    assert!(
        declaration.contains("declare class Image {"),
        "{declaration}"
    );
    assert!(
        declaration.contains("type JsonValue = null | boolean | number | string"),
        "{declaration}"
    );
}

#[test]
fn the_typescript_section_carries_the_contract_the_model_relies_on() {
    let text = render_tools_sdk(&[bash_tool()], ProgramFormat::FunctionBody);
    assert!(text.contains("erasable syntax only"));
    assert!(text.contains("MAY overlap under `Promise.all`"));
    assert!(text.contains("lossless JSON"));
    assert!(text.contains("`inspect(...values)`"));
    // Any other wrapper describes the program rather than a function body.
    let other = render_tools_sdk(&[bash_tool()], ProgramFormat::Other);
    assert!(other.contains("`code`, the program to execute,"));
    assert!(!other.contains("erasable syntax only"));
}

#[test]
fn the_typescript_section_overrides_the_two_image_returning_tools() {
    let read_image = SdkTool::new(
        "read_image",
        "Read an image.",
        json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        json!({ "type": "string" }),
    );
    let cua = SdkTool::new(
        "cua",
        "Drive a browser.",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "string" }),
    );
    let declaration = ts_declaration(&render_tools_sdk(
        &[read_image, cua],
        ProgramFormat::FunctionBody,
    ));
    // The declared JSON schema would be a lie for these two, so `Image` names the real value.
    assert!(
        declaration.contains("  read_image: Image;"),
        "{declaration}"
    );
    assert!(
        declaration.contains("  cua: Image | string;"),
        "{declaration}"
    );
}

#[test]
fn the_bash_example_appears_only_when_its_literals_satisfy_the_schema() {
    // `command` is required and accepts "pwd", so the example is shown.
    let accepted = render_tools_sdk(&[bash_tool()], ProgramFormat::FunctionBody);
    assert!(
        accepted.contains("inspect(await tools.bash({ command: 'pwd' }))"),
        "{accepted}"
    );

    // A `description` that a required field demands is included in the example.
    let with_description = SdkTool::new(
        "bash",
        "Run a shell command.",
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" }, "description": { "type": "string" } },
            "required": ["command", "description"],
        }),
        json!({ "type": "string" }),
    );
    let text = render_tools_sdk(&[with_description], ProgramFormat::FunctionBody);
    assert!(
        text.contains(
            "inspect(await tools.bash({ description: 'Show current directory', command: 'pwd' }))"
        ),
        "{text}"
    );

    // A schema whose literals would reject the example omits it rather than teaching a bad call.
    let rejected = SdkTool::new(
        "bash",
        "Run a shell command.",
        json!({
            "type": "object",
            "properties": { "command": { "type": "string", "const": "ls" } },
            "required": ["command"],
        }),
        json!({ "type": "string" }),
    );
    let text = render_tools_sdk(&[rejected], ProgramFormat::FunctionBody);
    assert!(!text.contains("inspect(await tools.bash("), "{text}");

    // No `bash` tool at all means no example.
    let other = SdkTool::new("other", "Other.", json!({}), json!({ "type": "string" }));
    let text = render_tools_sdk(&[other], ProgramFormat::FunctionBody);
    assert!(!text.contains("inspect(await tools.bash("), "{text}");
}

#[test]
fn the_typescript_section_is_deterministic_and_sorted() {
    let first = SdkTool::new("zzz", "Last.", json!({}), json!({ "type": "string" }));
    let second = SdkTool::new("aaa", "First.", json!({}), json!({ "type": "string" }));
    let forward = render_tools_sdk(
        &[first.clone(), second.clone()],
        ProgramFormat::FunctionBody,
    );
    let reversed = render_tools_sdk(&[second, first], ProgramFormat::FunctionBody);
    assert_eq!(forward, reversed);
    let aaa = forward.find("  aaa:").expect("aaa");
    let zzz = forward.find("  zzz:").expect("zzz");
    assert!(aaa < zzz, "aaa before zzz");
}

#[test]
fn an_empty_tool_set_renders_empty_maps() {
    let declaration = ts_declaration(&render_tools_sdk(&[], ProgramFormat::FunctionBody));
    assert!(
        declaration.contains("interface ToolArgsMap {}"),
        "{declaration}"
    );
    assert!(
        declaration.contains("interface ToolOutputMap {}"),
        "{declaration}"
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod ptc {
    pub use super::ptc_sdk_gap as sdk;
}
