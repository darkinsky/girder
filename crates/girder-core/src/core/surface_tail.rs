//! The tool-schema surface: the author-facing schema DSL, the subset gate, and the value walk.
//!
//! Upstream `packages/core/tools` is the largest pure-code subpackage in `packages/core` (2,361
//! effective lines), and its two schema files are the largest part of it that needs no Cordis
//! context at all: `json-schema.ts` (656 lines) and `schema.ts` (646 lines). This module ports both
//! halves that the local crate was missing.
//!
//! # What upstream actually does, in three layers
//!
//! | layer | upstream | here |
//! |---|---|---|
//! | author DSL | `valueSchemaSpecToJsonSchema`, `parameterSchemaSpecToJsonSchema` | [`compile_value_schema`], [`compile_parameter_schema`] |
//! | subset gate | `assertSupportedJsonSchema`, `assertObjectJsonSchema` | [`assert_supported_schema`], [`assert_object_schema`] |
//! | value walk | `validateJsonSchemaValue` | [`validate_schema_value`], [`validate_schema_value_at`] |
//!
//! They are three *different* languages and conflating them is the mistake the layering prevents:
//!
//! - The **author DSL** is what a tool author writes. It has one keyword the raw schema does not
//!   (`type: 'json'`), it spells requiredness as a per-property `required: true`, and it demands an
//!   explicit openness decision on every object.
//! - The **subset** is the only grammar the rest of the harness (prompt rendering, generated PTC
//!   SDK types, subagent output schemas) promises to understand. A schema outside it is refused at
//!   definition time rather than ignored at call time.
//! - The **value walk** is what runs against model-generated arguments, and it is total: it returns
//!   path-qualified diagnostics instead of throwing.
//!
//! # Why `required: true` becomes `required: [...]`
//!
//! The author DSL puts requiredness next to the property, because that is where an author thinks
//! about it — "this parameter is not optional". JSON Schema puts it on the *object*, as a list of
//! names. Compilation is therefore what hoists `required: true` from each property into the
//! enclosing object's `required` array, and the implicit parameter root is just the outermost case
//! of the same rule. [`compile_parameter_schema`] additionally keeps the root **open** (no
//! `additionalProperties: false`), while every explicit object node must state its own openness:
//! an accidental JSON Schema default on a nested object is the silent behaviour change the explicit
//! requirement exists to prevent.
//!
//! # What the local crate already had, and what this does not duplicate
//!
//! - [`girder_core::validate_tool_arguments`] (`tool_contracts.rs`) already validates arguments
//!   against a *raw* schema, with standardized [`girder_core::FailureKind`] violations that carry a
//!   remedy. It deliberately refuses `oneOf`, and it labels its root `args`.
//! - [`girder_core::ptc::sdk::validate_subset`] already implements the **subset grammar walk**.
//!   This module calls it rather than growing a third copy: [`assert_supported_schema`] is that walk
//!   plus upstream's error shape, and [`assert_object_schema`] adds the object-root guard. Every
//!   message this module reports about a malformed schema is therefore that walk's wording.
//!
//! What is genuinely new here is the DSL compiler and the *exactly-one* `oneOf` value semantics,
//! plus upstream's diagnostic root convention (an empty root path names a property directly, so the
//! first violation of a tool call reads `missing required property "path"` rather than
//! `missing required property "args.path"`).
//!
//! # Deviations, all caused by Rust having no JavaScript realm
//!
//! Upstream's walk spends real effort on hazards that cannot exist here, and each one leaves a
//! documented gap rather than a silent behaviour change:
//!
//! - **Realm and prototype forgery.** `isPlainJsonRecord`, `isJsonSchemaRecord`,
//!   `isPlainJsonArray`, the intrinsic-constructor probes and the `Proxy` traps all ask "is this a
//!   plain value from any JavaScript realm". A `serde_json::Value` is plain by construction, so
//!   those checks reduce to their accept branch.
//! - **Lossless JSON.** `default`/`examples` annotations, object and array values, and unconstrained
//!   roots are all checked with `isJsonValue` upstream. A `Value` cannot hold `undefined`, `NaN`,
//!   `-0`, a sparse array, a `Date`, a circular reference or a throwing getter, so those checks are
//!   dropped. The one losslessness hazard Rust *does* have — an integer above 2^53 — is
//!   [`girder_core::is_lossless`], and [`project_tool_schema`] applies it.
//! - **Cycles.** Not constructible in a `Value`; upstream's `seen` set becomes unreachable.
//! - **Key order.** Upstream preserves JavaScript insertion order, so `required` lists names in
//!   declaration order. The local map is a `BTreeMap`, so `required` lists them sorted, and
//!   `additionalProperties: false` reports undeclared keys sorted. Order is the only difference and
//!   it is asserted below so it cannot drift unnoticed.
//! - **Stack safety.** Upstream walks both trees with explicit task stacks, so a 5,000-deep union is
//!   fine there. Both walks here are recursive and capped at [`MAX_SCHEMA_DEPTH`], which is the same
//!   cap the shared subset walk uses: past it a schema is *refused* (compiling and asserting) or a
//!   value is *reported* (validating) rather than overflowing the stack.
//! - **A forged schema node.** Upstream throws (`assertNever`) when a schema node declares a type
//!   outside the subset, because a validated schema cannot contain one. Here the value walk reports
//!   a violation instead, and a non-object node constrains nothing — which is exactly what
//!   upstream's `Object.hasOwn` reads do with one. Neither is reachable through
//!   [`check_args`], which refuses the schema first.

use serde_json::{Map, Number, Value};

use girder_core::is_lossless;
use girder_core::ptc::sdk::validate_subset;

/// The deepest schema or value nesting either walk descends.
///
/// `serde_json` refuses to parse more than 128 nested containers, so a deeper tree can only be built
/// programmatically. The cap exists because both walks are recursive: past it a schema is refused or
/// a value is reported, so a hand-built tree cannot overflow the stack. The subset walk
/// ([`girder_core::ptc::sdk::validate_subset`]) applies the same number.
pub const MAX_SCHEMA_DEPTH: usize = 512;

/// The error code upstream's `JsonSchemaError` carries.
pub const UNSUPPORTED_SCHEMA: &str = "UNSUPPORTED_SCHEMA";

/// The error code upstream's `ToolArgsError` carries.
pub const INVALID_ARGS: &str = "INVALID_ARGS";

/// The annotation keywords every author node may carry.
///
/// They are copied through compilation verbatim and ignored by validation; the subset walk still
/// checks that `description` and `title` are strings.
const ANNOTATION_KEYWORDS: [&str; 4] = ["description", "title", "default", "examples"];

/// The seven type names the enforced subset accepts.
const SCHEMA_TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// The scalar types, which are the ones that accept `enum`/`const`.
const SCALAR_TYPES: [&str; 5] = ["string", "number", "integer", "boolean", "null"];

/// A schema or argument failure, as upstream reports it.
///
/// Upstream has two error classes with two codes; the code is the part a caller branches on, so both
/// shapes live in one type here. Every `violations` entry is a complete diagnostic sentence fragment,
/// joined with `"; "` — the same joining upstream does, because a model that receives all violations
/// at once needs one turn fewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaError {
    /// The stable code: [`UNSUPPORTED_SCHEMA`] or [`INVALID_ARGS`].
    pub code: &'static str,
    /// Every diagnostic, in walk order. Never empty.
    pub violations: Vec<String>,
}

impl SchemaError {
    /// Builds an error from a code and its violations.
    #[must_use]
    pub fn new(code: &'static str, violations: Vec<String>) -> Self {
        Self { code, violations }
    }

    /// The one-sentence report, matching upstream's error messages.
    #[must_use]
    pub fn message(&self) -> String {
        let headline = if self.code == INVALID_ARGS {
            "invalid arguments:"
        } else {
            "unsupported JSON schema:"
        };
        format!("{headline} {}", self.violations.join("; "))
    }
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for SchemaError {}

// ============================================================================================
// Layer one: the author-facing schema DSL
// ============================================================================================

/// Compiles one author value schema into the enforced raw subset.
///
/// This is upstream's `valueSchemaSpecToJsonSchema`: the author-only `type: 'json'` node becomes an
/// **annotation-only** node (which the subset reads as "any JSON value"), `required: true` is refused
/// at this level because a value root has no enclosing object to hoist it into, and the result is
/// then gated by [`assert_supported_schema`].
///
/// # Errors
///
/// Returns [`UNSUPPORTED_SCHEMA`] with one violation for the first author error — upstream's compiler
/// throws on the first, so this does not collect them — or with every subset violation when the
/// compiled node is outside the grammar (a type-incorrect `enum`, a one-branch `oneOf`).
pub fn compile_value_schema(spec: &Value) -> Result<Value, SchemaError> {
    let compiled = compile_value(spec, "schema", false, 0).map_err(author_error)?;
    assert_supported_schema(&compiled)?;
    Ok(compiled)
}

/// Compiles an implicit parameter map into an object-rooted raw schema.
///
/// This is upstream's `parameterSchemaSpecToJsonSchema`: the map *is* the root object, so it stays
/// open (no `additionalProperties: false`), and a property marked `required: true` is hoisted into
/// the root's `required` array.
///
/// # Errors
///
/// Returns [`UNSUPPORTED_SCHEMA`] with one violation for the first author error, or with every
/// subset violation when the compiled root is outside the grammar.
pub fn compile_parameter_schema(spec: &Value) -> Result<Value, SchemaError> {
    let (properties, required) =
        compile_property_map(spec, "parameters", 0).map_err(author_error)?;
    let mut schema = Map::new();
    schema.insert("type".to_owned(), Value::String("object".to_owned()));
    schema.insert("properties".to_owned(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_owned(), string_array(required));
    }
    let schema = Value::Object(schema);
    assert_supported_schema(&schema)?;
    Ok(schema)
}

/// Folds one author error into the error shape, as upstream's `authorError` does.
fn author_error(message: String) -> SchemaError {
    SchemaError::new(UNSUPPORTED_SCHEMA, vec![message])
}

/// Compiles one author value node.
///
/// `allow_required` is true only for a node that is a property of a property map: that is the one
/// place author requiredness is legal, and it is exactly where upstream threads the same flag.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per JSON Schema keyword, in upstream's declaration order; splitting it would scatter the DSL's rules and make a reader diffing against `json-schema.ts` jump between functions"
)]
fn compile_value(
    input: &Value,
    path: &str,
    allow_required: bool,
    depth: usize,
) -> Result<Value, String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(format!(
            "{path} nests deeper than {MAX_SCHEMA_DEPTH} levels, which this compiler refuses"
        ));
    }
    let record = input
        .as_object()
        .ok_or_else(|| format!("{path} must be a value schema object"))?;
    let mut author_keys: Vec<&str> = ANNOTATION_KEYWORDS.to_vec();
    if allow_required {
        author_keys.push("required");
    }

    if record.contains_key("oneOf") {
        let mut allowed = author_keys.clone();
        allowed.extend(["oneOf", "type"]);
        assert_author_keys(record, path, &allowed)?;
        // A union has no room for a sibling `type`; the subset would reject the pair anyway, but the
        // author error names the mistake instead of the grammar.
        if record.contains_key("type") {
            return Err(format!("{path} cannot declare both type and oneOf"));
        }
        let branches = record
            .get("oneOf")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                format!("{path}.oneOf must be an array of at least two value schemas")
            })?;
        let mut node = Map::new();
        copy_annotations(record, &mut node);
        let mut compiled = Vec::with_capacity(branches.len());
        for (index, branch) in branches.iter().enumerate() {
            compiled.push(compile_value(
                branch,
                &format!("{path}.oneOf[{index}]"),
                false,
                depth + 1,
            )?);
        }
        node.insert("oneOf".to_owned(), Value::Array(compiled));
        return Ok(Value::Object(node));
    }

    match record.get("type").and_then(Value::as_str) {
        // The author-only node: no `type` survives, so the subset reads the result as unconstrained
        // JSON. This is the one place the DSL is not a subset of the grammar.
        Some("json") => {
            let mut allowed = author_keys.clone();
            allowed.push("type");
            assert_author_keys(record, path, &allowed)?;
            let mut node = Map::new();
            copy_annotations(record, &mut node);
            Ok(Value::Object(node))
        }
        Some("object") => {
            let mut allowed = author_keys.clone();
            allowed.extend(["type", "properties", "additionalProperties"]);
            assert_author_keys(record, path, &allowed)?;
            // Openness is mandatory: an implicit JSON Schema default would silently start accepting
            // (or rejecting) keys the author never considered.
            let Some(Value::Bool(additional)) = record.get("additionalProperties") else {
                return Err(format!(
                    "{path}.additionalProperties must be explicitly true or false"
                ));
            };
            let additional = *additional;
            let mut node = Map::new();
            node.insert("type".to_owned(), Value::String("object".to_owned()));
            copy_annotations(record, &mut node);
            node.insert("additionalProperties".to_owned(), Value::Bool(additional));
            if let Some(properties) = record.get("properties") {
                let (compiled, required) =
                    compile_property_map(properties, &format!("{path}.properties"), depth + 1)?;
                attach_property_map(&mut node, compiled, required);
            }
            Ok(Value::Object(node))
        }
        Some("array") => {
            let mut allowed = author_keys.clone();
            allowed.extend(["type", "items"]);
            assert_author_keys(record, path, &allowed)?;
            let mut node = Map::new();
            node.insert("type".to_owned(), Value::String("array".to_owned()));
            copy_annotations(record, &mut node);
            if let Some(items) = record.get("items") {
                node.insert(
                    "items".to_owned(),
                    compile_value(items, &format!("{path}.items"), false, depth + 1)?,
                );
            }
            Ok(Value::Object(node))
        }
        Some(name) if SCALAR_TYPES.contains(&name) => {
            let mut allowed = author_keys.clone();
            allowed.extend(["type", "enum", "const"]);
            assert_author_keys(record, path, &allowed)?;
            let mut node = Map::new();
            node.insert("type".to_owned(), Value::String(name.to_owned()));
            copy_annotations(record, &mut node);
            // The literal list is copied as written; type-correctness is the subset gate's job, so
            // `{type: 'number', enum: ['1']}` fails with "must be a non-empty array of number values"
            // rather than with a DSL message.
            if let Some(values) = record.get("enum") {
                if !values.is_array() {
                    return Err(format!(
                        "{path}.enum must be a non-empty array of scalar values"
                    ));
                }
                node.insert("enum".to_owned(), values.clone());
            }
            if let Some(constant) = record.get("const") {
                node.insert("const".to_owned(), constant.clone());
            }
            Ok(Value::Object(node))
        }
        _ => Err(format!(
            "{path}.type must be string/number/integer/boolean/null/array/object/json, or use oneOf"
        )),
    }
}

/// Compiles one implicit property map, collecting per-property requiredness.
fn compile_property_map(
    input: &Value,
    path: &str,
    depth: usize,
) -> Result<(Map<String, Value>, Vec<String>), String> {
    let record = input
        .as_object()
        .ok_or_else(|| format!("{path} must be an object of value schemas"))?;
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (key, property) in record {
        let property_path = format!("{path}.{key}");
        let declared = property
            .as_object()
            .ok_or_else(|| format!("{property_path} must be a value schema object"))?;
        match declared.get("required") {
            None => {}
            Some(Value::Bool(true)) => required.push(key.clone()),
            Some(_) => {
                return Err(format!(
                    "{property_path}.required must be true when present"
                ));
            }
        }
        properties.insert(
            key.clone(),
            compile_value(property, &property_path, true, depth + 1)?,
        );
    }
    Ok((properties, required))
}

/// Installs a compiled property map on a containing object node.
fn attach_property_map(
    node: &mut Map<String, Value>,
    properties: Map<String, Value>,
    required: Vec<String>,
) {
    node.insert("properties".to_owned(), Value::Object(properties));
    if !required.is_empty() {
        node.insert("required".to_owned(), string_array(required));
    }
}

/// Copies every annotation the author wrote, verbatim.
fn copy_annotations(source: &Map<String, Value>, target: &mut Map<String, Value>) {
    for key in ANNOTATION_KEYWORDS {
        if let Some(value) = source.get(key) {
            target.insert(key.to_owned(), value.clone());
        }
    }
}

/// Rejects a key outside one node's declared author vocabulary.
fn assert_author_keys(
    record: &Map<String, Value>,
    path: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for key in record.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "{path}.{key} is not supported by the value schema DSL"
            ));
        }
    }
    Ok(())
}

/// Builds a JSON array of names.
fn string_array(names: Vec<String>) -> Value {
    Value::Array(names.into_iter().map(Value::String).collect())
}

// ============================================================================================
// Layer two: the enforced subset gate
// ============================================================================================

/// Asserts that a raw schema uses only the enforced subset.
///
/// This is upstream's `assertSupportedJsonSchema`. The grammar walk itself is the shared one
/// ([`girder_core::ptc::sdk::validate_subset`]), so the wording of every grammar diagnostic has a
/// single source; what this adds is upstream's error shape and the guarantee that the *same* schema
/// the PTC SDK renderers accept is the one this crate compiles to.
///
/// An annotation-only node is accepted, because that is the standard spelling of "any JSON value".
///
/// # Errors
///
/// Returns [`UNSUPPORTED_SCHEMA`] naming every offending path.
pub fn assert_supported_schema(schema: &Value) -> Result<(), SchemaError> {
    match validate_subset(schema) {
        Ok(()) => Ok(()),
        Err(error) => Err(SchemaError::new(UNSUPPORTED_SCHEMA, error.violations)),
    }
}

/// Asserts the subset **and** an object root, for consumers that need a record.
///
/// Structured output — subagent results, workflow results — is object-rooted upstream, and the guard
/// lives at the consumer rather than in the grammar because a tool *output* schema may legitimately
/// be a scalar or a union. An object-rooted failure is reported even when the grammar is fine, and
/// when both are wrong the grammar violations come first: fixing the grammar is the prerequisite.
///
/// # Errors
///
/// Returns [`UNSUPPORTED_SCHEMA`] with the grammar violations, or with the single root violation
/// `schema.type must be "object" (structured output is object-rooted)`.
pub fn assert_object_schema(schema: &Value) -> Result<(), SchemaError> {
    assert_supported_schema(schema)?;
    let is_object_root = schema
        .as_object()
        .is_some_and(|record| record.get("type").and_then(Value::as_str) == Some("object"));
    if is_object_root {
        Ok(())
    } else {
        Err(author_error(String::from(
            "schema.type must be \"object\" (structured output is object-rooted)",
        )))
    }
}

// ============================================================================================
// Layer three: the value walk
// ============================================================================================

/// Validates a value against an asserted raw schema, labelling the root `value`.
///
/// This is upstream's `validateJsonSchemaValue` with its default path.
#[must_use]
pub fn validate_schema_value(schema: &Value, value: &Value) -> Vec<String> {
    validate_schema_value_at(schema, value, "value")
}

/// Validates a value against an asserted raw schema at an explicit root label.
///
/// The walk is *total*: every diagnostic is returned, in walk order, and an empty result means the
/// value is valid. Order is deterministic — required names in declaration order, then properties in
/// schema order, then undeclared keys — because a model that fixes the first problem should see the
/// one its fix most likely caused next.
///
/// The empty root label is upstream's parameter convention: `""` names a property directly, so a
/// tool call's first failure reads `missing required property "path"`.
#[must_use]
pub fn validate_schema_value_at(schema: &Value, value: &Value, path: &str) -> Vec<String> {
    let mut found = Vec::new();
    check_value(schema, value, path, 0, &mut found);
    found
}

/// Walks one schema/value pair, appending every diagnostic.
fn check_value(node: &Value, value: &Value, path: &str, depth: usize, found: &mut Vec<String>) {
    if depth > MAX_SCHEMA_DEPTH {
        found.push(format!(
            "\"{}\" nests deeper than {MAX_SCHEMA_DEPTH} levels and was not checked",
            diagnostic_path(path)
        ));
        return;
    }
    // A non-object node constrains nothing, which is what upstream's own-property reads do with one;
    // [`check_args`] refuses such a schema before a value ever reaches this walk.
    let Some(record) = node.as_object() else {
        return;
    };

    if let Some(one_of) = record.get("oneOf") {
        // A union branch contributes only its verdict: its own diagnostics would describe a shape the
        // value was never required to have.
        let mut matches = 0_usize;
        for branch in one_of.as_array().map_or(&[][..], Vec::as_slice) {
            let mut branch_found = Vec::new();
            check_value(branch, value, path, depth + 1, &mut branch_found);
            if branch_found.is_empty() {
                matches += 1;
            }
        }
        if matches != 1 {
            found.push(format!(
                "\"{}\" must match exactly one oneOf branch (matched {matches})",
                diagnostic_path(path)
            ));
        }
        return;
    }

    let Some(declared) = record.get("type") else {
        // Unconstrained: any JSON value is valid. Upstream additionally requires the value to be
        // lossless JSON; every `Value` already is.
        return;
    };
    let Some(type_name) = declared.as_str().filter(|name| SCHEMA_TYPES.contains(name)) else {
        found.push(format!(
            "\"{}\" was not checked: the schema node's type is outside the enforced subset",
            diagnostic_path(path)
        ));
        return;
    };

    match type_name {
        "object" => check_object_value(record, value, path, depth, found),
        "array" => {
            let Some(items) = value.as_array() else {
                found.push(format!("\"{}\" must be an array", diagnostic_path(path)));
                return;
            };
            if let Some(item_schema) = record.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check_value(
                        item_schema,
                        item,
                        &format!("{path}[{index}]"),
                        depth + 1,
                        found,
                    );
                }
            }
        }
        "string" => {
            if value.is_string() {
                check_scalar_value(record, value, path, found);
            } else {
                found.push(format!("\"{}\" must be a string", diagnostic_path(path)));
            }
        }
        "number" => {
            if value.is_number() {
                check_scalar_value(record, value, path, found);
            } else {
                found.push(format!("\"{}\" must be a number", diagnostic_path(path)));
            }
        }
        "integer" => {
            if is_integer_value(value) {
                check_scalar_value(record, value, path, found);
            } else {
                found.push(format!("\"{}\" must be an integer", diagnostic_path(path)));
            }
        }
        "boolean" => {
            if value.is_boolean() {
                check_scalar_value(record, value, path, found);
            } else {
                found.push(format!("\"{}\" must be a boolean", diagnostic_path(path)));
            }
        }
        _ => {
            if value.is_null() {
                check_scalar_value(record, value, path, found);
            } else {
                found.push(format!("\"{}\" must be null", diagnostic_path(path)));
            }
        }
    }
}

/// Validates an object node: requiredness, declared properties, then openness.
fn check_object_value(
    record: &Map<String, Value>,
    value: &Value,
    path: &str,
    depth: usize,
    found: &mut Vec<String>,
) {
    let Some(object) = value.as_object() else {
        found.push(format!("\"{}\" must be an object", diagnostic_path(path)));
        return;
    };
    let properties = record.get("properties").and_then(Value::as_object);
    let mut reported = Vec::new();
    if let Some(required) = record.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                reported.push(format!(
                    "missing required property \"{}\"",
                    property_path(path, name)
                ));
            }
        }
    }
    if let Some(properties) = properties {
        for (name, child_schema) in properties {
            if let Some(child) = object.get(name) {
                check_value(
                    child_schema,
                    child,
                    &property_path(path, name),
                    depth + 1,
                    &mut reported,
                );
            }
        }
    }
    // Undeclared keys are reported last: a required or declared-property failure is the more
    // actionable one, and upstream orders them the same way.
    if record.get("additionalProperties") == Some(&Value::Bool(false)) {
        for name in object.keys() {
            if !properties.is_some_and(|properties| properties.contains_key(name)) {
                reported.push(format!(
                    "\"{}\" is not a declared property (additionalProperties: false)",
                    property_path(path, name)
                ));
            }
        }
    }
    found.append(&mut reported);
}

/// Validates a scalar node's `enum`/`const` after its primitive type check.
///
/// `enum` short-circuits: when the value is outside the list, that is the diagnostic, and also
/// reporting a `const` mismatch would describe two failures for one value.
fn check_scalar_value(
    record: &Map<String, Value>,
    value: &Value,
    path: &str,
    found: &mut Vec<String>,
) {
    if let Some(allowed) = record.get("enum") {
        let entries = allowed.as_array().map_or(&[][..], Vec::as_slice);
        if !entries.iter().any(|entry| json_equal(entry, value)) {
            found.push(format!(
                "\"{}\" must be one of {}",
                diagnostic_path(path),
                compact(allowed)
            ));
            return;
        }
    }
    if let Some(constant) = record.get("const") {
        if !json_equal(constant, value) {
            found.push(format!(
                "\"{}\" must be {}",
                diagnostic_path(path),
                compact(constant)
            ));
        }
    }
}

/// The root-aware diagnostic label: an empty path names the argument root.
fn diagnostic_path(path: &str) -> &str {
    if path.is_empty() { "arguments" } else { path }
}

/// Appends one object property without a leading dot at an implicit root.
fn property_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// Whether a value is an integer in JavaScript's sense.
///
/// `1.0` is an integer: `Number.isInteger(1.0)` is true, and a `JSON` parse of `1.0` produces that
/// value. `serde_json` keeps the decimal point, so the fractional part is what decides.
fn is_integer_value(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number.is_i64()
                || number.is_u64()
                || number.as_f64().is_some_and(|float| float.fract() == 0.0)
        }
        _ => false,
    }
}

/// Compares two JSON values as JavaScript's `===` would for the scalars `enum`/`const` accept.
///
/// The one place the comparison differs from structural equality: `1` and `1.0` are the same number
/// in JavaScript, while `serde_json` with `arbitrary_precision` keeps their text apart and compares
/// numbers as text. An author who writes `enum: [1]` must accept a model that sent `1.0`.
fn json_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => numbers_equal(a, b),
        _ => left == right,
    }
}

/// Compares two JSON numbers by value, not by their text form.
///
/// Exact equality is the point: JSON has no notion of "close enough", and a tolerance would accept a
/// value the author did not write. `-0.0` and `0.0` are the same number in `JSON`/JavaScript, and
/// `as_f64` on a parsed `-0` yields `-0.0`, so the comparison below must be numeric rather than by
/// bit pattern.
#[allow(clippy::float_cmp)]
fn numbers_equal(left: &Number, right: &Number) -> bool {
    if let (Some(a), Some(b)) = (left.as_i64(), right.as_i64()) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (left.as_u64(), right.as_u64()) {
        return a == b;
    }
    match (left.as_f64(), right.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Renders a value as compact JSON for a diagnostic, as upstream's `JSON.stringify` does.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("null"))
}

// ============================================================================================
// The tool-call surface built on all three layers
// ============================================================================================

/// Validates model-generated arguments against an author parameter spec.
///
/// This is upstream's `validateArgs`: compile the implicit parameter root, then walk the value with
/// the empty root label so diagnostics name the parameter directly.
///
/// # Errors
///
/// Returns [`UNSUPPORTED_SCHEMA`] when the *spec* is malformed. A malformed spec is an author bug, so
/// it is never reported as an argument problem.
pub fn arg_violations(spec: &Value, args: &Value) -> Result<Vec<String>, SchemaError> {
    let schema = compile_parameter_schema(spec)?;
    Ok(validate_schema_value_at(&schema, args, ""))
}

/// Checks arguments, raising [`INVALID_ARGS`] when they do not match.
///
/// This is what upstream's `defineTool` runs before every `execute`, and the reason `execute` can
/// trust its typed arguments.
///
/// # Errors
///
/// Returns [`INVALID_ARGS`] with every violation when the arguments do not match, or
/// [`UNSUPPORTED_SCHEMA`] when the spec itself is malformed.
pub fn check_args(spec: &Value, args: &Value) -> Result<(), SchemaError> {
    let violations = arg_violations(spec, args)?;
    if violations.is_empty() {
        Ok(())
    } else {
        Err(SchemaError::new(INVALID_ARGS, violations))
    }
}

/// The model-facing half of a tool definition, as schema projection needs it.
///
/// A view rather than a borrow of [`girder_core::ToolDefinition`] because the projection is also
/// applied to definitions whose executor half is not a local `ToolBody` — the PTC transport, for
/// instance, is resolved lazily from a runtime. `defer_loading` has no field in
/// [`girder_core::ToolSchema`] yet; carrying it here is what lets a caller see whether the flag
/// would have been emitted instead of silently dropping it.
#[derive(Clone, Copy, Debug)]
pub struct ToolSchemaSource<'a> {
    /// Tool name the model calls.
    pub name: &'a str,
    /// Description shown to the model.
    pub description: &'a str,
    /// JSON Schema for the arguments.
    pub parameters: &'a Value,
    /// Whether the definition asked to be loaded on demand.
    pub defer_loading: bool,
}

/// What schema projection produces: the model-facing fields, and nothing callable.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedToolSchema {
    /// Tool name the model calls.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
    /// Whether the definition asked to be loaded on demand.
    pub defer_loading: bool,
}

/// Strips execution and presentation from a definition, leaving the model-facing schema.
///
/// This is upstream's `projectToolSchema`. `detach_parameters` is the difference between *reading*
/// the schema and taking a *snapshot* of it: a caller that hands the projection to a registry, a
/// cache, or a prompt section must detach, because a later mutation of the definition's parameters
/// would otherwise change a schema that was already emitted.
///
/// Detaching is also where the lossless check belongs: a snapshot that silently corrupts an integer
/// above 2^53 would advertise a schema the tool never had. Reading without detaching skips the check
/// (nothing is copied, so nothing can be lost) — upstream's shape exactly.
///
/// # Errors
///
/// Returns [`UNSUPPORTED_SCHEMA`] when the parameters are not lossless JSON while detaching.
pub fn project_tool_schema(
    source: &ToolSchemaSource<'_>,
    detach_parameters: bool,
) -> Result<ProjectedToolSchema, SchemaError> {
    let parameters = if detach_parameters {
        if !is_lossless(source.parameters) {
            return Err(author_error(format!(
                "tool \"{}\" parameters must be lossless JSON before schema projection",
                source.name
            )));
        }
        source.parameters.clone()
    } else {
        source.parameters.clone()
    };
    Ok(ProjectedToolSchema {
        name: source.name.to_owned(),
        description: source.description.to_owned(),
        parameters,
        defer_loading: source.defer_loading,
    })
}
