//! The two PTC SDK projections: one tool registry rendered twice, as the Python and the TypeScript
//! text a model programs against.
//!
//! Upstream `packages/core/tool-executor-ptc/src` holds 2,476 lines. `py-types.ts` (817) and
//! `ts-types.ts` (335) are the part of it with no Cordis dependency at all: with the PTC executor
//! selected, the native tool schemas are *omitted* from the request, so this generated text is the
//! model's only source for each tool's argument names, required fields, types, descriptions and
//! canonical output type. Nothing here executes model code; both functions are pure projections of
//! input to text, which is why they port on their own.
//!
//! # What the projections answer
//!
//! Both files do the same two things.
//!
//! 1. Translate one raw JSON Schema node into a type expression in the target language.
//!    [`json_schema_to_py`] maps `string`/`number`/`integer`/`boolean`/`null` to
//!    `str`/`float`/`int`/`bool`/`None`, `array` to `list[T]`, `oneOf` to an `A | B` union, and a
//!    scalar `const`/`enum` to a `Literal[...]`. [`json_schema_to_ts`] makes the same translation
//!    into `string`/`number`/`boolean`/`null`, `T[]`, `A | B`, and literal unions.
//! 2. Render the whole `tools:sdk` prompt section: fixed usage instructions, then one declaration
//!    per tool, sorted lexicographically so an unchanged registry produces byte-identical text.
//!    [`render_tools_sdk_py`] additionally *names* object shapes — one `TypedDict` per argument,
//!    output and nested object — so the shape survives into the program instead of collapsing to
//!    `dict[str, Any]`.
//!
//! # Which vocabulary this module reads
//!
//! Upstream pins the input to `JsonSchemaNode` from `packages/core/tools/src/json-schema.ts`, and
//! gates both renderers on `assertSupportedJsonSchema`, whose accepted subset is exactly:
//!
//! | keyword | accepted alongside |
//! |---|---|
//! | `type` | one of `object`/`array`/`string`/`number`/`integer`/`boolean`/`null` |
//! | `oneOf` | two or more branches; none of the constraint keywords beside it |
//! | `properties`, `required`, `additionalProperties` | `type: object` only |
//! | `items` | `type: array` only |
//! | `enum`, `const` | scalar `type`s only |
//! | `description`, `title`, `default`, `examples` | any node, annotations only |
//!
//! [`SubsetError`] restates that grammar and [`validate_subset`] enforces it. It has to be a
//! restatement rather than a call into an existing table, because the three candidate vocabularies
//! in this crate answer different questions:
//!
//! - [`crate::tool_contracts`] enforces a *wider* grammar — it also accepts `$ref`, `format`, the
//!   numeric bounds and the string constraints — and it validates *arguments against a schema*
//!   rather than a schema against a grammar. Reading a schema through it would mean emitting a
//!   type expression for keywords the PTC text has no spelling for.
//! - [`crate::typert::Schema`] describes the Remote surface (revisions, `added_in`, nullable
//!   widening). It is a schema *constructor*: it produces JSON Schema, and it does not parse it.
//! - `rpc_schema` and `tool_schema_export` both project a registry *outward* to MCP/`OpenAI`/
//!   `OpenAPI` shapes, without inspecting a schema's keywords at all.
//!
//! What the new table buys is the gate itself: upstream rejects a schema outside the subset at
//! registration time, so a caller that never validated could otherwise render a `TypedDict` from a
//! `$ref` its walk silently skipped. The test suite pins the agreement of this table with
//! [`crate::tool_contracts`] on every construct both of them understand, so the two cannot drift
//! into disagreeing about what `type: object` means.
//!
//! `serde_json::Map` is key-sorted in this workspace (`preserve_order` is not enabled), so object
//! fields render in key order. Upstream renders in JSON insertion order; the difference cannot lose
//! a field, and this module documents it rather than fighting it.
//!
//! # Why totals, not errors
//!
//! Both renderers are *total* in upstream: an unsupported or malformed node degrades to `Any`
//! (Python) or `unknown` (TypeScript) and the render continues. Prompt assembly cannot fail because
//! one tool registered a schema outside the subset — the model then sees one untyped argument
//! instead of no SDK at all. [`json_schema_to_py`] and [`json_schema_to_ts`] therefore degrade and
//! say so, while [`validate_subset`] is exposed separately for a caller that wants to know *why* a
//! tool went untyped.
//!
//! # The two bounds this walk has that upstream does not
//!
//! Upstream's walks are explicit-stack, so a 5,000-deep `oneOf` chain renders without touching the
//! JavaScript call stack. Rust's are recursive and bounded instead: [`MAX_SCHEMA_DEPTH`] caps
//! validation, and [`MAX_RENDER_DEPTH`] caps rendering to `Any`/`unknown`. A schema deeper than
//! that degrades rather than overflowing the stack — a visibly untyped tool beats a killed process.
//!
//! # What an exact Python projection needs and this cannot have
//!
//! The Python flavor's identifier rule is `str.isidentifier()` on the *target* interpreter's
//! Unicode tables plus `NFKC` stability, and its class names are `NFKC`-normalized. Girder has no
//! `unicode-xid`/`unicode-normalization` dependency and this change adds none, so
//! [`is_bare_identifier`] is an explicit ASCII-plus-non-ASCII approximation: an ASCII
//! `XID_Start`/`XID_Continue` name, or a name whose characters are all letters or digits by Rust's
//! tables, may go bare; everything else takes the subscript or `dict[str, Any]` path.
//!
//! The practical loss is the `NFKC` *equivalence* cases (`ﬁeld` normalizing to `field`, Hangul jamo
//! composing across a join) and code points newer than Rust's tables: those may be emitted bare
//! where upstream would have routed them, so the generated block could disagree with one
//! interpreter. That is a bounded readability risk in advisory prompt text, not a correctness one —
//! every field stays reachable through the subscript path either way. Given the choice between a
//! wrong private `XID_Start` table and an honest approximation, this module takes the second and
//! says so.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Map, Value};

/// The deepest `list[…]` nesting emitted into one Python annotation.
///
/// `CPython`'s tokenizer rejects a logical line with more than 200 simultaneously open brackets
/// (`MAXLEVEL`), and an annotation adds up to two of its own (`Literal[`, plus the enclosing `(` of
/// an argument list). 180 leaves headroom; past it the chain degrades to `Any`, which is an
/// unusable annotation either way but *valid* Python. The TypeScript flavor needs no counterpart:
/// nothing in the TypeScript grammar bounds nesting.
pub const MAX_LIST_NESTING: usize = 180;

/// The cap on a Python class-name base, keeping each emitted name linear in schema depth.
///
/// Child class names derive from their parent's allocated name, so an unbounded schema of
/// single-field objects would grow each name by one segment per level and the sum of all names
/// quadratically. Counted in `char`s rather than UTF-16 code units, so it can never split a code
/// point — upstream's `slice(0, n)` can, and has to drop a straddling surrogate half afterwards.
pub const MAX_CLASS_NAME_BASE: usize = 120;

/// The deepest schema tree [`validate_subset`] walks before refusing to go further.
///
/// Validation is recursive here where upstream's is explicit-stack, so this is the bound that keeps
/// a hostile schema from overflowing the stack. It is far past any real tool's schema: a
/// 190-deep array chain is the deepest shape the tests exercise.
pub const MAX_SCHEMA_DEPTH: usize = 512;

/// The TypeScript alias every open object and every unconstrained node references.
pub const JSON_VALUE_ALIAS: &str = "JsonValue";

/// Python hard keywords, plus `__debug__`.
///
/// Soft keywords (`match`, `case`, `type`, `_`) are deliberately absent: each is special in exactly
/// one syntactic position — a statement head, a `match` clause head, or a pattern — so `type: str`
/// as a field and `async def match(...)` as a method are both legal, and including them would
/// degrade common search and regex fields for nothing. `__debug__` is not a keyword, but `CPython`
/// refuses to assign it at compile time, which is what a class-syntax field, a parameter and a
/// keyword argument all are.
const PYTHON_RESERVED: [&str; 36] = [
    "False",
    "None",
    "True",
    "and",
    "as",
    "assert",
    "async",
    "await",
    "break",
    "class",
    "continue",
    "def",
    "del",
    "elif",
    "else",
    "except",
    "finally",
    "for",
    "from",
    "global",
    "if",
    "import",
    "in",
    "is",
    "lambda",
    "nonlocal",
    "not",
    "or",
    "pass",
    "raise",
    "return",
    "try",
    "while",
    "with",
    "yield",
    "__debug__",
];

/// One `typing` symbol a render may pull in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TypingSymbol {
    /// `Any`, for an unconstrained or degraded node.
    Any,
    /// `Literal`, for a scalar `const`/`enum`.
    Literal,
    /// `NotRequired`, for a field absent from `required`.
    NotRequired,
    /// `Protocol`, for the `Tools` interface. Always present.
    Protocol,
    /// `TypedDict`, for a named object shape.
    TypedDict,
}

impl TypingSymbol {
    /// The symbol as it appears in the `typing` import line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Any => "Any",
            Self::Literal => "Literal",
            Self::NotRequired => "NotRequired",
            Self::Protocol => "Protocol",
            Self::TypedDict => "TypedDict",
        }
    }
}

/// The lexicographic `typing` import order, matching upstream's `TYPING_ORDER`.
const TYPING_ORDER: [TypingSymbol; 5] = [
    TypingSymbol::Any,
    TypingSymbol::Literal,
    TypingSymbol::NotRequired,
    TypingSymbol::Protocol,
    TypingSymbol::TypedDict,
];

// ============================================================================================
// The enforced subset
// ============================================================================================

/// Why a raw schema falls outside the subset the SDK text can express.
///
/// Upstream collects *every* violation and throws them together, because an author fixing one at a
/// time needs one round trip per problem. [`validate_subset`] keeps that shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubsetError {
    /// Every offending path, in walk order. Never empty.
    pub violations: Vec<String>,
}

impl SubsetError {
    /// Builds an error from its violations.
    #[must_use]
    pub fn new(violations: Vec<String>) -> Self {
        Self { violations }
    }

    /// The whole report as one sentence, matching upstream's `JsonSchemaError` message.
    #[must_use]
    pub fn message(&self) -> String {
        format!("unsupported JSON schema: {}", self.violations.join("; "))
    }
}

impl std::fmt::Display for SubsetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for SubsetError {}

/// The keywords that constrain a value, as opposed to annotating one.
const CONSTRAINT_KEYWORDS: [&str; 8] = [
    "type",
    "oneOf",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
];

/// Keywords that only annotate, and never change the rendered type.
const ANNOTATION_KEYWORDS: [&str; 4] = ["description", "title", "default", "examples"];

/// The keywords that are invalid beside `oneOf`: a union has no place for them.
const ONE_OF_SIBLING_KEYWORDS: [&str; 6] = [
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
];

/// The seven type names the subset accepts.
const SCHEMA_TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// The quoted list of accepted type names, for diagnostics.
const SCHEMA_TYPES_LIST: &str = "object/array/string/number/integer/boolean/null";

/// Checks a raw schema against the enforced subset, collecting every violation.
///
/// This is upstream's `assertSupportedJsonSchema` as a `Result` rather than a throw, and it is the
/// gate both renderers apply before they walk. `serde_json::Value` cannot hold a circular reference
/// or a non-JSON scalar, so the realm and lossless-JSON checks upstream needs do not exist here;
/// what remains is the grammar.
///
/// # Errors
///
/// Returns a [`SubsetError`] naming every offending path when the schema uses a keyword outside the
/// subset, places a keyword on the wrong `type`, or declares `required` names that are not in
/// `properties`.
pub fn validate_subset(schema: &Value) -> Result<(), SubsetError> {
    let mut violations = Vec::new();
    check_node(schema, "schema", 0, &mut violations);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(SubsetError::new(violations))
    }
}

/// Whether `schema` would render to a type expression rather than degrade to `Any`/`unknown`.
#[must_use]
pub fn is_supported(schema: &Value) -> bool {
    validate_subset(schema).is_ok()
}

/// Walks one raw node, appending every violation.
fn check_node(node: &Value, path: &str, depth: usize, violations: &mut Vec<String>) {
    if depth > MAX_SCHEMA_DEPTH {
        violations.push(format!(
            "{path} nests deeper than {MAX_SCHEMA_DEPTH} levels, which the SDK text cannot express"
        ));
        return;
    }
    let Some(record) = node.as_object() else {
        violations.push(format!("{path} must be a schema object"));
        return;
    };
    for key in record.keys() {
        if CONSTRAINT_KEYWORDS.contains(&key.as_str())
            || ANNOTATION_KEYWORDS.contains(&key.as_str())
        {
            continue;
        }
        violations.push(format!(
            "{path}.{key} is not a supported keyword (subset: type/oneOf/properties/required/\
             additionalProperties/items/enum/const + annotations)"
        ));
    }
    if record
        .get("description")
        .is_some_and(|value| !value.is_string())
    {
        violations.push(format!("{path}.description must be a string"));
    }
    if record.get("title").is_some_and(|value| !value.is_string()) {
        violations.push(format!("{path}.title must be a string"));
    }
    let has_type = record.contains_key("type");
    let has_one_of = record.contains_key("oneOf");
    if has_type && has_one_of {
        violations.push(format!("{path} cannot declare both type and oneOf"));
        return;
    }
    if !has_type && !has_one_of {
        for key in ONE_OF_SIBLING_KEYWORDS {
            if record.contains_key(key) {
                violations.push(format!("{path}.{key} requires type or oneOf"));
            }
        }
        return;
    }
    if has_one_of {
        check_one_of(record, path, depth, violations);
        return;
    }
    let Some(declared) = record.get("type") else {
        return;
    };
    let Some(type_name) = declared.as_str() else {
        violations.push(if declared.is_array() {
            format!("{path}.type must be a single type string (type arrays are not supported)")
        } else {
            format!("{path}.type must be one of {SCHEMA_TYPES_LIST}")
        });
        return;
    };
    if !SCHEMA_TYPES.contains(&type_name) {
        violations.push(format!("{path}.type must be one of {SCHEMA_TYPES_LIST}"));
        return;
    }
    check_keywords_against_type(record, type_name, path, violations);
    match type_name {
        "object" => check_object(record, path, depth, violations),
        "array" => {
            if let Some(items) = record.get("items") {
                check_node(items, &format!("{path}.items"), depth + 1, violations);
            }
        }
        _ => check_scalar(record, type_name, path, violations),
    }
}

/// Validates the `oneOf` branches and the keywords that may not sit beside them.
fn check_one_of(
    record: &Map<String, Value>,
    path: &str,
    depth: usize,
    violations: &mut Vec<String>,
) {
    for key in ONE_OF_SIBLING_KEYWORDS {
        if record.contains_key(key) {
            violations.push(format!("{path}.{key} is not supported beside oneOf"));
        }
    }
    match record.get("oneOf").and_then(Value::as_array) {
        Some(branches) if branches.len() >= 2 => {
            for (index, branch) in branches.iter().enumerate() {
                check_node(
                    branch,
                    &format!("{path}.oneOf[{index}]"),
                    depth + 1,
                    violations,
                );
            }
        }
        _ => violations.push(format!(
            "{path}.oneOf must be an array of at least two schemas"
        )),
    }
}

/// Rejects a keyword that is only valid on another `type`.
fn check_keywords_against_type(
    record: &Map<String, Value>,
    type_name: &str,
    path: &str,
    violations: &mut Vec<String>,
) {
    let allowed_for: [(&str, &[&str]); 6] = [
        ("properties", &["object"]),
        ("required", &["object"]),
        ("additionalProperties", &["object"]),
        ("items", &["array"]),
        ("enum", &["string", "number", "integer", "boolean", "null"]),
        ("const", &["string", "number", "integer", "boolean", "null"]),
    ];
    for (key, types) in allowed_for {
        if record.contains_key(key) && !types.contains(&type_name) {
            violations.push(format!(
                "{path}.{key} is not supported on type \"{type_name}\""
            ));
        }
    }
}

/// Validates an `object` node: its property schemas, `required`, and `additionalProperties`.
fn check_object(
    record: &Map<String, Value>,
    path: &str,
    depth: usize,
    violations: &mut Vec<String>,
) {
    let declared = match record.get("properties") {
        None => None,
        Some(Value::Object(properties)) => {
            for (name, child) in properties {
                check_node(
                    child,
                    &format!("{path}.properties.{name}"),
                    depth + 1,
                    violations,
                );
            }
            Some(properties)
        }
        Some(_) => {
            violations.push(format!("{path}.properties must be an object of schemas"));
            None
        }
    };
    check_object_tail(record, path, declared, violations);
}

/// Validates the `required`/`additionalProperties` half of an `object` node.
///
/// `required` is checked against the declared properties because a name that is not in `properties`
/// can never be satisfied, and it is the one authoring mistake that silently makes a tool
/// uncallable while still rendering a type expression.
fn check_object_tail(
    record: &Map<String, Value>,
    path: &str,
    properties: Option<&Map<String, Value>>,
    violations: &mut Vec<String>,
) {
    if let Some(required) = record.get("required") {
        match required.as_array() {
            None => violations.push(format!("{path}.required must be an array of strings")),
            Some(names) if names.iter().any(|name| !name.is_string()) => {
                violations.push(format!("{path}.required must be an array of strings"));
            }
            Some(names) => {
                for name in names.iter().filter_map(Value::as_str) {
                    let declared =
                        properties.is_some_and(|properties| properties.contains_key(name));
                    if !declared {
                        violations.push(format!(
                            "{path}.required names \"{name}\" which is not in properties"
                        ));
                    }
                }
            }
        }
    }
    if record
        .get("additionalProperties")
        .is_some_and(|value| !value.is_boolean())
    {
        violations.push(format!("{path}.additionalProperties must be a boolean"));
    }
}

/// Validates a scalar node's `enum`/`const` against its declared `type`.
fn check_scalar(
    record: &Map<String, Value>,
    type_name: &str,
    path: &str,
    violations: &mut Vec<String>,
) {
    let allowed = record.get("enum").and_then(Value::as_array);
    let enum_valid = allowed.is_some_and(|values| {
        !values.is_empty() && values.iter().all(|value| scalar_matches(type_name, value))
    });
    if record.contains_key("enum") && !enum_valid {
        violations.push(format!(
            "{path}.enum must be a non-empty array of {type_name} values"
        ));
    }
    let Some(constant) = record.get("const") else {
        return;
    };
    if !scalar_matches(type_name, constant) {
        violations.push(format!("{path}.const must be a {type_name} value"));
        return;
    }
    if let Some(allowed) = allowed
        && enum_valid
        && !allowed.contains(constant)
    {
        violations.push(format!(
            "{path}.const must be one of {path}.enum when both are declared"
        ));
    }
}

/// Whether a scalar JSON value is valid for one declared `type`.
///
/// `integer` accepts a number with no fractional part, as JavaScript's `Number.isInteger` does on
/// the value a JSON parse produced, so `1.0` is an integer here and that is upstream's reading too.
fn scalar_matches(type_name: &str, value: &Value) -> bool {
    match type_name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => match value {
            Value::Number(number) => {
                number.is_i64()
                    || number.is_u64()
                    || number.as_f64().is_some_and(|float| float.fract() == 0.0)
            }
            _ => false,
        },
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

// ============================================================================================
// Prose, quoting and literals
// ============================================================================================

/// Which language's escaping a description needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProseTarget {
    /// A Python docstring or `#` comment: control characters are escaped as `\xNN`.
    Python,
    /// A JavaScript block comment: `*/` is escaped as `*\/`.
    TypeScript,
}

/// Whether a character is whitespace by `ECMAScript`'s `\s` definition.
///
/// Not Rust's `char::is_whitespace`, which uses the `White_Space` property and therefore includes
/// NEL (U+0085) — a character `ECMAScript` does not fold. Upstream's collapse keeps NEL, escapes it
/// as `\x85`, and this module matches that rather than the property table.
fn is_ecmascript_space(character: char) -> bool {
    matches!(character as u32,
        0x0009..=0x000d
        | 0x0020
        | 0x00a0
        | 0x1680
        | 0x2000..=0x200a
        | 0x2028
        | 0x2029
        | 0x202f
        | 0x205f
        | 0x3000
        | 0xfeff
    )
}

/// Collapses a description to one line for `target`, or `None` when it documents nothing.
///
/// A description that collapses to nothing — empty, or whitespace only — is `None`: it documents the
/// node no better than an absent one, and emitting it would leave an empty `"""` docstring or a bare
/// `#   ` line in the SDK.
///
/// Python escaping covers the C0 controls, DEL and the C1 controls, because `CPython` refuses NUL
/// anywhere in source and the rest are at best invisible. The boundary is the category: `\xNN`
/// addresses U+0000 to U+00FF, so one form covers `Cc` exactly. `char` already excludes unpaired
/// surrogates, so upstream's second escape class — a surrogate that makes the source impossible to
/// UTF-8-encode — cannot reach here.
#[must_use]
pub fn describe(description: Option<&str>, target: ProseTarget) -> Option<String> {
    let raw = description?;
    let mut collapsed = String::with_capacity(raw.len());
    let mut pending_space = false;
    for character in raw.chars() {
        if is_ecmascript_space(character) {
            pending_space = !collapsed.is_empty();
            continue;
        }
        if pending_space {
            collapsed.push(' ');
            pending_space = false;
        }
        match target {
            ProseTarget::Python if is_python_control(character) => {
                let _ = write!(collapsed, "\\x{:02x}", character as u32);
            }
            _ => collapsed.push(character),
        }
    }
    if collapsed.is_empty() {
        return None;
    }
    Some(collapsed)
}

/// Whether `CPython` treats a code point as a control character it should not see raw.
///
/// The C0 range minus the characters the whitespace collapse already removed, plus DEL and the C1
/// block.
fn is_python_control(character: char) -> bool {
    let code = character as u32;
    code <= 0x0008 || (0x000e..=0x001f).contains(&code) || (0x007f..=0x009f).contains(&code)
}

/// Renders a one-line `JSDoc` block for a description, or no lines when there is none.
///
/// The `*/` escape happens on the pair rather than per character, because only the pair closes the
/// comment: a bare `*` is harmless.
fn ts_doc_lines(description: Option<&str>, indent: usize) -> Vec<String> {
    let Some(collapsed) = describe(description, ProseTarget::TypeScript) else {
        return Vec::new();
    };
    let escaped = collapsed.replace("*/", "*\\/");
    vec![format!("{}/** {escaped} */", ts_pad(indent))]
}

/// Renders a one-line docstring for a description, or no lines when there is none.
///
/// Backslashes are doubled first and every quote is escaped, so a description ending in `"` or an
/// odd backslash cannot merge with or escape the closing triple quote.
fn py_doc_lines(description: Option<&str>, indent: usize) -> Vec<String> {
    let Some(collapsed) = describe(description, ProseTarget::Python) else {
        return Vec::new();
    };
    let escaped = collapsed.replace('\\', "\\\\").replace('"', "\\\"");
    vec![format!("{}\"\"\"{escaped}\"\"\"", py_pad(indent))]
}

/// `indent` levels of Python indentation, four spaces per level (PEP 8).
fn py_pad(indent: usize) -> String {
    "    ".repeat(indent)
}

/// `indent` levels of TypeScript indentation, two spaces per level.
fn ts_pad(indent: usize) -> String {
    "  ".repeat(indent)
}

/// Renders a string as a double-quoted literal both languages read back identically.
///
/// The escape set is the overlap of Python's and JavaScript's: a bare `"` would close the literal
/// early, a trailing odd backslash would eat the closing quote, and a bare LF/CR would end the line
/// before its terminator. Escaping `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t` and the remaining C0
/// controls as `\uXXXX` keeps the text parseable and the decoded value equal to the schema's.
///
/// DEL, the C1 controls (NEL among them), and U+2028/U+2029 deliberately pass through raw: all are
/// legal in a Python string literal and in a TypeScript one alike, and none ends a physical line.
#[must_use]
pub fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\u{0008}' => quoted.push_str("\\b"),
            '\u{000c}' => quoted.push_str("\\f"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            control if (control as u32) < 0x0020 => {
                let _ = write!(quoted, "\\u{:04x}", control as u32);
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Renders one validated scalar as Python literal text: `True`/`False`, a quoted string, or the
/// exact digits of a number.
///
/// Digits rather than `f64` formatting, because Python integers are arbitrary-precision: the emitted
/// digits *are* the value the model programs against. For `-2^60`, shortest-round-trip formatting of
/// the double names an integer no double holds, so the `i64`/`u64` path is the exact one. A float
/// keeps its decimal text, gaining `.0` when nothing else marks it as a float.
///
/// `null` cannot reach the `const`/`enum` path on a non-`null` type — the subset rejects it — and a
/// `null`-typed node renders `None` at the type level instead.
#[must_use]
pub fn python_literal(value: &Value) -> String {
    match value {
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        Value::String(text) => quote(text),
        Value::Number(number) => python_number(number),
        // A validated `const`/`enum` member is one of the above; anything else cannot reach here.
        other => other.to_string(),
    }
}

/// Renders a JSON number as Python literal text.
fn python_number(number: &serde_json::Number) -> String {
    if let Some(integer) = number.as_i64() {
        return integer.to_string();
    }
    if let Some(integer) = number.as_u64() {
        return integer.to_string();
    }
    let text = number.to_string();
    if text.contains('.') || text.contains('e') || text.contains('E') {
        text
    } else {
        format!("{text}.0")
    }
}

/// Renders one validated scalar as a TypeScript literal, which is its JSON text.
///
/// The TypeScript flavor needs no exactness work: its literal is re-read by a JavaScript parser into
/// the same double, so the shortest text that round-trips is the value.
#[must_use]
pub fn typescript_literal(value: &Value) -> String {
    match value {
        Value::String(text) => quote(text),
        other => other.to_string(),
    }
}

// ============================================================================================
// Python identifiers and class names
// ============================================================================================

/// Whether a name can be emitted as a bare Python identifier.
///
/// ASCII names follow the grammar exactly. Non-ASCII names are accepted when every character is a
/// letter or digit by Rust's Unicode tables (plus `_`) and the first is a letter rather than a
/// combining mark: `路径` is as legal a field name as `path`, and rejecting it would degrade the
/// whole enclosing object — dropping every field's name, requiredness and type, information whose
/// only source under PTC execution is this text.
///
/// `NFKC` stability is not tested; see the module documentation for why that is a documented
/// approximation rather than a correctness bug.
#[must_use]
pub fn is_bare_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    is_identifier_head(first) && characters.all(is_identifier_continue)
}

/// Whether a character may start a Python identifier, by this module's approximation.
fn is_identifier_head(character: char) -> bool {
    if character.is_ascii() {
        return character.is_ascii_alphabetic() || character == '_';
    }
    character.is_alphabetic() && !is_combining_mark(character)
}

/// Whether a character may continue a Python identifier, by this module's approximation.
fn is_identifier_continue(character: char) -> bool {
    if character.is_ascii() {
        return character.is_ascii_alphanumeric() || character == '_';
    }
    character.is_alphanumeric() || character == '_' || is_combining_mark(character)
}

/// Whether a character is a combining mark, which `XID_Start` excludes and `XID_Continue` allows.
///
/// `char::is_alphabetic` is true for some combining marks, so a name starting with one would
/// otherwise look like it starts with a letter. These are the common combining blocks rather than a
/// complete `Mn`/`Mc` table; a false negative degrades a name's rendering, never its reachability.
fn is_combining_mark(character: char) -> bool {
    matches!(character as u32,
        0x0300..=0x036f      // Combining Diacritical Marks
        | 0x0483..=0x0489    // Cyrillic combining
        | 0x0591..=0x05bd    // Hebrew points
        | 0x0610..=0x061a    // Arabic marks
        | 0x064b..=0x065f    // Arabic marks
        | 0x0e31 | 0x0e34..=0x0e3a | 0x0e47..=0x0e4e // Thai marks
        | 0x0eb1 | 0x0eb4..=0x0ebc | 0x0ec8..=0x0ecd // Lao marks
        | 0x1ab0..=0x1aff    // Combining Diacritical Marks Extended
        | 0x1dc0..=0x1dff    // Combining Diacritical Marks Supplement
        | 0x20d0..=0x20f0    // Combining Diacritical Marks for Symbols
        | 0x3099..=0x309a    // Kana voicing marks
        | 0xfe20..=0xfe2f    // Combining Half Marks
    )
}

/// Whether a tool or field name is a Python hard keyword.
#[must_use]
pub fn is_python_reserved(name: &str) -> bool {
    PYTHON_RESERVED.contains(&name)
}

/// Whether a name may be declared as a class-syntax `TypedDict` field.
///
/// Three conditions with three different failures: a non-identifier has no class-syntax spelling, a
/// hard keyword is a `SyntaxError` in a class body, and a leading-double-underscore non-dunder name
/// (`__token`) is NAME-MANGLED inside class syntax (`_ClassName__token`), so the declaration would
/// describe a different JSON key than the registered schema. A dunder is not mangled and stays.
fn is_declarable_field(name: &str) -> bool {
    // A dunder is not mangled, so it keeps its name; any other double-underscore head does not.
    let mangled = name.starts_with("__") && !name.ends_with("__");
    is_bare_identifier(name) && !is_python_reserved(name) && !mangled
}

/// Whether a tool name may be declared as a `Tools` method rather than a subscript comment.
///
/// `_foo` is a legal attribute but is routed to the subscript form anyway, because the forms that
/// break split three ways — a non-dunder `__token` name-mangles at the CALL site, a dunder that
/// exists on `object`/`type` (`__class__`, `__doc__`) resolves before `__getattr__` ever runs, and
/// implicit special-method lookup skips the hook entirely. One rule over the whole family costs
/// nothing; a per-form rule would have to enumerate them.
fn is_declarable_method(name: &str) -> bool {
    is_bare_identifier(name) && !is_python_reserved(name) && !name.starts_with('_')
}

/// `CamelCase`s a name into a Python class-name segment.
///
/// Non-identifier characters and `_` split words, each word is capitalized, and a joined head that
/// cannot start an identifier takes a `Tool` prefix. Unicode survives: a `路径` field yields a
/// `路径`-based class name rather than collapsing to the bare prefix — as upstream's coercion of a
/// combining-mark head shows, the prefix decision has to happen before the name is joined.
#[must_use]
pub fn camel_case(raw: &str) -> String {
    let mut joined = String::new();
    let mut word = String::new();
    for character in raw.chars().chain(std::iter::once('\u{0}')) {
        if character != '\u{0}' && is_identifier_continue(character) && character != '_' {
            word.push(character);
            continue;
        }
        if word.is_empty() {
            continue;
        }
        let mut characters = word.chars();
        if let Some(first) = characters.next() {
            joined.extend(first.to_uppercase());
            joined.push_str(characters.as_str());
        }
        word.clear();
    }
    match joined.chars().next() {
        Some(first) if is_identifier_head(first) => joined,
        _ => format!("Tool{joined}"),
    }
}

/// Caps a class-name base so each emitted name stays linear in schema depth.
///
/// Counted in `char`s, which is also why no code point is ever split: upstream's cap counts UTF-16
/// code units and has to drop a straddling surrogate half afterwards.
#[must_use]
pub fn cap_class_name_base(base: &str) -> String {
    base.chars().take(MAX_CLASS_NAME_BASE).collect()
}

/// Appends a child segment to a parent class-name base, capping at every step.
///
/// Capping at propagation rather than only at allocation keeps each level constant-time: a deep
/// chain would otherwise carry an ever-growing string down the tree and re-materialize it at every
/// level.
fn child_class_name(base: &str, segment: &str) -> String {
    cap_class_name_base(&format!("{base}{segment}"))
}

// ============================================================================================
// JSON Schema → Python
// ============================================================================================

/// The state one Python render accumulates: declarations, taken names, and `typing` symbols.
///
/// Threaded through the walk so a nested object's declaration reaches the output before the class
/// that refers to it, which is what "declaration order equals reference order" requires.
#[derive(Debug, Default)]
struct RenderState {
    /// Already-emitted `TypedDict` declarations, in emission order.
    classes: Vec<String>,
    /// Every class name taken, so a collision is suffixed rather than emitted twice.
    used_class_names: BTreeSet<String>,
    /// The next suffix to try per capped base, making allocation amortized constant.
    next_class_counter: BTreeMap<String, usize>,
    /// The `typing` symbols the render actually used.
    typing: BTreeSet<TypingSymbol>,
}

impl RenderState {
    /// A state whose import line will at least list `Protocol`.
    fn new() -> Self {
        let mut state = Self::default();
        state.need(TypingSymbol::Protocol);
        state
    }

    /// Records that a symbol is needed in the `typing` import line.
    fn need(&mut self, symbol: TypingSymbol) {
        self.typing.insert(symbol);
    }

    /// Reserves a unique class name from `base`, suffixing `2`, `3`, … on collision.
    fn allocate_class_name(&mut self, base: &str) -> String {
        let capped = cap_class_name_base(base);
        if self.used_class_names.insert(capped.clone()) {
            return capped;
        }
        let mut counter = self.next_class_counter.get(&capped).copied().unwrap_or(2);
        loop {
            let candidate = format!("{capped}{counter}");
            counter += 1;
            if self.used_class_names.insert(candidate.clone()) {
                self.next_class_counter.insert(capped, counter);
                return candidate;
            }
        }
    }

    /// The `from typing import …` line, listing exactly the symbols the render used.
    fn typing_imports(&self) -> String {
        let symbols: Vec<&str> = TYPING_ORDER
            .iter()
            .filter(|symbol| self.typing.contains(symbol))
            .map(|symbol| symbol.as_str())
            .collect();
        format!("from typing import {}", symbols.join(", "))
    }
}

/// Where an object node may declare a class, or the fact that it may not.
///
/// A `TypedDict` has to be *named*, and a name only exists inside a render that has somewhere to
/// declare it. The context-free entry point has no such place, so an object there is opaque — and
/// making that a variant rather than an empty string keeps the distinction explicit instead of
/// hanging it on a sentinel a caller could accidentally pass.
#[derive(Clone, Copy, Debug)]
enum Naming<'a> {
    /// A named render: `class_name` is the base this object would take.
    Named(&'a str),
    /// A context-free render: an object has nowhere to declare a class, so it degrades.
    Anonymous,
}

impl<'a> Naming<'a> {
    /// The base this node would take, or `None` when nothing may be declared.
    fn base(self) -> Option<&'a str> {
        match self {
            Self::Named(name) => Some(name),
            Self::Anonymous => None,
        }
    }
}

/// One node's Python rendering context.
#[derive(Clone, Copy, Debug)]
struct PyContext<'a> {
    /// Where an object node here may declare a class.
    naming: Naming<'a>,
    /// Open `list[` brackets enclosing this node, for [`MAX_LIST_NESTING`].
    list_depth: usize,
}

/// Maps one raw JSON Schema node to a context-free Python type expression.
///
/// This is upstream's `jsonSchemaToPy`. An object with properties degrades to `dict[str, Any]`,
/// because naming a `TypedDict` needs a render context to declare into, which only
/// [`render_tools_sdk_py`] supplies. Unsupported or malformed input returns `Any`, never an error.
#[must_use]
pub fn json_schema_to_py(schema: &Value) -> String {
    if validate_subset(schema).is_err() {
        return "Any".to_owned();
    }
    let mut state = RenderState::default();
    let context = PyContext {
        naming: Naming::Anonymous,
        list_depth: 0,
    };
    render_type(schema, context, &mut state)
}

/// Maps one node to a Python type expression, naming object shapes into `state`.
fn render_type(schema: &Value, context: PyContext<'_>, state: &mut RenderState) -> String {
    let Some(node) = schema.as_object() else {
        state.need(TypingSymbol::Any);
        return "Any".to_owned();
    };
    if let Some(branches) = node.get("oneOf").and_then(Value::as_array) {
        // `list[A | B]` needs no parentheses in Python, and a union opens no bracket of its own, so
        // the branches inherit the enclosing depth unchanged. Union LENGTH is deliberately uncapped:
        // a long `A | B | …` chain is grammatically valid at any length, and this text is never
        // compiled — only parsed by a reader — while a cap would retire the linear-time pin.
        let mut parts = Vec::with_capacity(branches.len());
        for (index, branch) in branches.iter().enumerate() {
            let segment = (index + 1).to_string();
            let naming = match context.naming.base() {
                Some(base) => Naming::Named(&child_class_name(base, &segment)),
                None => Naming::Anonymous,
            };
            parts.push(render_type(
                branch,
                PyContext {
                    naming,
                    list_depth: context.list_depth,
                },
                state,
            ));
        }
        return parts.join(" | ");
    }
    let Some(type_name) = node.get("type").and_then(Value::as_str) else {
        state.need(TypingSymbol::Any);
        return "Any".to_owned();
    };
    match type_name {
        "string" => constrained_scalar(node, "str", state),
        "number" => constrained_scalar(node, "float", state),
        "integer" => constrained_scalar(node, "int", state),
        "boolean" => constrained_scalar(node, "bool", state),
        "null" => "None".to_owned(),
        "array" => render_array(node, context, state),
        "object" => render_object(node, context, state),
        _ => {
            state.need(TypingSymbol::Any);
            "Any".to_owned()
        }
    }
}

/// Renders an `array` node as `list[T]`.
fn render_array(
    node: &Map<String, Value>,
    context: PyContext<'_>,
    state: &mut RenderState,
) -> String {
    let Some(items) = node.get("items") else {
        state.need(TypingSymbol::Any);
        return "list[Any]".to_owned();
    };
    if context.list_depth >= MAX_LIST_NESTING {
        // Another `list[` would push the annotation past `CPython`'s open-bracket limit and make the
        // whole SDK block unparseable, so the chain degrades here — an unusable annotation either
        // way, and this one is valid Python.
        state.need(TypingSymbol::Any);
        return "Any".to_owned();
    }
    let item = render_type(
        items,
        PyContext {
            naming: context.naming,
            list_depth: context.list_depth + 1,
        },
        state,
    );
    format!("list[{item}]")
}

/// Renders an `object` node, as a named `TypedDict` when it can be and `dict[str, Any]` when not.
fn render_object(
    node: &Map<String, Value>,
    context: PyContext<'_>,
    state: &mut RenderState,
) -> String {
    let empty = Map::new();
    let properties = match node.get("properties") {
        Some(Value::Object(properties)) => properties,
        Some(_) => {
            state.need(TypingSymbol::Any);
            return "Any".to_owned();
        }
        None => &empty,
    };
    // No naming context means there is nowhere to declare a `TypedDict`, so the object degrades. A
    // field name with no class-syntax spelling is inexpressible, so such an object degrades WHOLE
    // rather than describing some fields and not others — and either way a degraded object must not
    // leave behind a class declaration the caller has no way to receive.
    let Some(base) = context.naming.base() else {
        state.need(TypingSymbol::Any);
        return "dict[str, Any]".to_owned();
    };
    if !properties.keys().all(|name| is_declarable_field(name)) {
        state.need(TypingSymbol::Any);
        return "dict[str, Any]".to_owned();
    }
    let open = node.get("additionalProperties") != Some(&Value::Bool(false));
    // An OPEN empty object is any dict; a CLOSED empty object declares an empty `TypedDict` so "no
    // keys accepted" survives into the SDK.
    if properties.is_empty() && open {
        state.need(TypingSymbol::Any);
        return "dict[str, Any]".to_owned();
    }
    let name = state.allocate_class_name(base);
    state.need(TypingSymbol::TypedDict);
    let required: BTreeSet<&str> = node
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut lines = vec![format!("class {name}(TypedDict):")];
    for (field, field_schema) in properties {
        if let Some(description) = field_schema
            .as_object()
            .and_then(|schema| schema.get("description"))
            .and_then(Value::as_str)
        {
            if let Some(description) = describe(Some(description), ProseTarget::Python) {
                lines.push(format!("{}# {description}", py_pad(1)));
            }
        }
        let segment = camel_case(field);
        let field_type = render_type(
            field_schema,
            PyContext {
                naming: Naming::Named(&child_class_name(&name, &segment)),
                // A field annotation is its own logical line, so nesting restarts — at 1, reserving
                // the bracket an optional field's `NotRequired[…]` wraps around it.
                list_depth: 1,
            },
            state,
        );
        if required.contains(field.as_str()) {
            lines.push(format!("{}{field}: {field_type}", py_pad(1)));
        } else {
            state.need(TypingSymbol::NotRequired);
            lines.push(format!("{}{field}: NotRequired[{field_type}]", py_pad(1)));
        }
    }
    // `TypedDict` syntax cannot express openness, so an open object states it in-band: with the
    // native schemas omitted under PTC execution, this line is the model's only signal that keys
    // beyond the declared ones are accepted.
    if open {
        lines.push(format!(
            "{}# Additional keys beyond those declared are allowed.",
            py_pad(1)
        ));
    }
    // A closed empty object still needs a class body (`pass`) to be valid Python; the declared
    // emptiness is the information.
    if lines.len() == 1 {
        lines.push(format!("{}pass", py_pad(1)));
    }
    state.classes.push(lines.join("\n"));
    name
}

/// Renders a validated scalar `const`/`enum` as `Literal[…]`, falling back to the broad type.
///
/// This deviates from PEP 586, which restricts `Literal` parameters to `int`/`bool`/`str`/`bytes`/
/// enum/`None`: a non-integral number emits a float literal a strict checker would reject. The stub
/// is advisory prompt text, only required to parse, and keeping the exact value communicates the
/// constraint to the model.
fn constrained_scalar(node: &Map<String, Value>, broad: &str, state: &mut RenderState) -> String {
    if let Some(constant) = node.get("const") {
        state.need(TypingSymbol::Literal);
        return format!("Literal[{}]", python_literal(constant));
    }
    if let Some(allowed) = node.get("enum").and_then(Value::as_array) {
        state.need(TypingSymbol::Literal);
        let rendered: Vec<String> = allowed.iter().map(python_literal).collect();
        return format!("Literal[{}]", rendered.join(", "));
    }
    broad.to_owned()
}

// ============================================================================================
// JSON Schema → TypeScript
// ============================================================================================

/// Maps one raw JSON Schema node to a TypeScript type literal.
///
/// This is upstream's `jsonSchemaToTs`. Object nodes render as inline type literals with optional
/// markers, an array of a union parenthesizes its element (`("x" | "y")[]`), and an open object
/// intersects `Record<string, JsonValue>`. Unsupported or malformed input returns `unknown`, never
/// an error. `indent` is the indentation level the rendered object members start at.
#[must_use]
pub fn json_schema_to_ts(schema: &Value, indent: usize) -> String {
    if validate_subset(schema).is_err() {
        return "unknown".to_owned();
    }
    render_ts(schema, indent)
}

/// Renders one node as TypeScript.
fn render_ts(schema: &Value, indent: usize) -> String {
    let Some(node) = schema.as_object() else {
        return "unknown".to_owned();
    };
    if let Some(branches) = node.get("oneOf").and_then(Value::as_array) {
        let rendered: Vec<String> = branches
            .iter()
            .map(|branch| render_ts(branch, indent))
            .collect();
        return rendered.join(" | ");
    }
    let Some(type_name) = node.get("type").and_then(Value::as_str) else {
        return JSON_VALUE_ALIAS.to_owned();
    };
    match type_name {
        "string" | "number" | "integer" | "boolean" | "null" => {
            ts_constrained_scalar(node, type_name)
        }
        "array" => {
            let Some(items) = node.get("items") else {
                return format!("{JSON_VALUE_ALIAS}[]");
            };
            let element = render_ts(items, indent);
            // A union or intersection element must be parenthesized: `"x" | "y"[]` would otherwise
            // mean `"x" | ("y"[])`.
            if element.contains('|') || element.contains('&') {
                format!("({element})[]")
            } else {
                format!("{element}[]")
            }
        }
        "object" => render_ts_object(node, indent),
        _ => "unknown".to_owned(),
    }
}

/// Renders an `object` node as a TypeScript type literal.
fn render_ts_object(node: &Map<String, Value>, indent: usize) -> String {
    let empty = Map::new();
    let properties = match node.get("properties") {
        Some(Value::Object(properties)) => properties,
        Some(_) => return "unknown".to_owned(),
        None => &empty,
    };
    let open = node.get("additionalProperties") != Some(&Value::Bool(false));
    if properties.is_empty() {
        return if open {
            format!("Record<string, {JSON_VALUE_ALIAS}>")
        } else {
            "Record<string, never>".to_owned()
        };
    }
    let required: BTreeSet<&str> = node
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut parts = vec!["{".to_owned()];
    for (name, property) in properties {
        let description = property
            .as_object()
            .and_then(|schema| schema.get("description"))
            .and_then(Value::as_str);
        parts.extend(ts_doc_lines(description, indent + 1));
        let property_type = render_ts(property, indent + 1);
        let optional = if required.contains(name.as_str()) {
            ""
        } else {
            "?"
        };
        parts.push(format!(
            "{}{}{optional}: {property_type};",
            ts_pad(indent + 1),
            render_key(name)
        ));
    }
    parts.push(format!("{}}}", ts_pad(indent)));
    let declared = parts.join("\n");
    if open {
        format!("{declared} & Record<string, {JSON_VALUE_ALIAS}>")
    } else {
        declared
    }
}

/// Whether a name is a valid bare TypeScript identifier; anything else is quoted.
fn is_ts_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    characters
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '$')
}

/// Renders an object key: bare when it is a valid identifier, quoted otherwise.
///
/// Every name stays reachable and nothing is aliased, which matters because the emitted key IS the
/// JSON key the model has to pass.
fn render_key(name: &str) -> String {
    if is_ts_identifier(name) {
        name.to_owned()
    } else {
        quote(name)
    }
}

/// Renders a validated scalar `const`/`enum` as a TypeScript literal type.
fn ts_constrained_scalar(node: &Map<String, Value>, type_name: &str) -> String {
    if let Some(constant) = node.get("const") {
        return typescript_literal(constant);
    }
    if let Some(allowed) = node.get("enum").and_then(Value::as_array) {
        let rendered: Vec<String> = allowed.iter().map(typescript_literal).collect();
        return rendered.join(" | ");
    }
    // `integer` is `number`: JavaScript has one number type, and a distinct spelling would be a
    // distinction the program cannot observe.
    match type_name {
        "integer" | "number" => "number".to_owned(),
        "string" => "string".to_owned(),
        "boolean" => "boolean".to_owned(),
        _ => "null".to_owned(),
    }
}

// ============================================================================================
// The `tools:sdk` sections
// ============================================================================================

/// One tool's SDK projection: the model-facing schema plus the tool's canonical output schema.
///
/// Upstream's `ToolSdkSchema` adds exactly one field to the registry's `ToolSchema` — the validated
/// canonical value the tool returns — because a generated method signature needs a return type and
/// the argument schema alone cannot supply it.
#[derive(Clone, Debug, PartialEq)]
pub struct SdkTool {
    /// The tool's name, as the model calls it.
    pub name: String,
    /// The tool's description, as the model reads it.
    pub description: String,
    /// The argument schema, as registered.
    pub parameters: Value,
    /// The canonical value the tool binding returns.
    pub output: Value,
}

impl SdkTool {
    /// Builds a projection from the fields a registry already holds plus the output schema.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        output: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            output,
        }
    }
}

/// The tools in lexicographic name order — the order both flavors emit them in.
fn tools_sorted(tools: &[SdkTool]) -> Vec<&SdkTool> {
    let mut sorted: Vec<&SdkTool> = tools.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    sorted
}

/// The fixed model-facing usage contract rendered above the Python declarations.
const PY_SDK_INSTRUCTIONS: &str = "## Writing code for run_code

`run_code` takes two required arguments: `code`, the program to execute, and `description`, a short summary of what the program does. At run time exactly two of the names declared below are bound: `tools` and `ToolCallError`. Everything else is a STATIC STUB describing argument and return types — in particular the `TypedDict` classes do NOT exist at run time, so build arguments as plain `dict`/`list` JSON values: `await tools.name({\"field\": 1})`, never `FooArgs(field=1)`, which raises `NameError`. Inside the program:

- Call tools as `await tools.name(args)` — subscript access for exotic, reserved, or underscore-leading names: `await tools[\"my-tool\"](args)`. Every call resolves to the tool's typed canonical JSON value (each method's return type below). Tool arguments must be lossless JSON.
- A FAILED tool call raises `ToolCallError`, whose `toolName` identifies the failed tool and whose message is human-readable — wrap in `try/except` to handle and continue.
- Independent read-only calls MAY overlap under `asyncio.gather` (safe calls run concurrently; mutating calls run alone, in submission order). Sequence dependent work with `await`.
- Only values emitted as program output enter the conversation. A successful tool result containing an image is attached after the run so you can inspect it on the next step; every other intermediate result stays out of the conversation, so extract just what you need.

The available tools:";

/// The error class both declarations name, so a program can catch a failed call.
const PY_TOOL_CALL_ERROR: &str = "class ToolCallError(Exception):\n    toolName: str";

/// The syntax a generated TypeScript program is expected to use.
///
/// Upstream's `PtcProgramFormat` has four values (`function-body`, `repl`, `script`,
/// `scoped-repl`), but only two instruction variants exist and only `function-body` is distinguished
/// by the text: the other three differ in how a runtime *wraps* the program, which is a runtime
/// concern rather than an SDK-text one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramFormat {
    /// A function body, run type-stripped: erasable syntax only.
    FunctionBody,
    /// Any other wrapper: a `repl`, a `script`, or a scoped `repl`.
    Other,
}

/// Renders the TypeScript flavor's argument and call instructions for one program format.
fn ts_sdk_instructions(program_format: ProgramFormat) -> String {
    let program_description = match program_format {
        ProgramFormat::FunctionBody => {
            "the body of an async TypeScript function (erasable syntax \
                                        only — no `enum` or namespaces; type annotations are \
                                        advisory, the code runs type-stripped)"
        }
        ProgramFormat::Other => "the program to execute",
    };
    format!(
        "## Writing code for run_code\n\n`run_code` takes two required arguments: `code`, \
         {program_description}, and `description`, a short summary of what the program does. The \
         declarations below are SDK bindings for this program. A declaration does not make its \
         name a directly callable tool; only names supplied as separate tool schemas may be \
         called directly."
    )
}

/// Renders the TypeScript program's execution and output instructions.
fn ts_program_instructions() -> String {
    "Inside the program:\n\n- Call tools as `await tools.name(args)` — quoted access for exotic \
     names: `tools[\"my-tool\"](args)`. Every call resolves to its declared value, which may be an \
     Image. Tool arguments must be lossless JSON.\n- A FAILED tool call rejects with \
     `ToolCallError`, whose `toolName` identifies the failed tool and whose `message` is \
     human-readable — `try/catch` it to handle and continue.\n- Independent read-only calls MAY \
     overlap under `Promise.all` (safe calls run concurrently; mutating calls run alone, in \
     submission order). Sequence dependent work with `await`.\n- Show selected values and images \
     with `inspect(...values)`; every other intermediate result stays out of the conversation, so \
     extract just what you need.\n\nProgram-only SDK bindings:"
        .to_owned()
}

/// Whether one string schema accepts the literal a doc example uses for it.
fn accepts_example_string(schema: Option<&Value>, value: &str) -> bool {
    let Some(schema) = schema.and_then(Value::as_object) else {
        return false;
    };
    if schema.get("type").and_then(Value::as_str) != Some("string") {
        return false;
    }
    let constant_accepts = schema
        .get("const")
        .is_none_or(|constant| constant.as_str() == Some(value));
    let enum_accepts = schema
        .get("enum")
        .and_then(Value::as_array)
        .is_none_or(|allowed| allowed.iter().any(|entry| entry.as_str() == Some(value)));
    constant_accepts && enum_accepts
}

/// Renders the `bash` example, but only when its literal arguments satisfy the current schema.
///
/// A prompt that shows a call the declared types reject teaches the model to make a call that
/// fails, so the example is omitted rather than emitted wrong.
fn render_bash_example(tools: &[&SdkTool]) -> String {
    let Some(bash) = tools.iter().find(|tool| tool.name == "bash") else {
        return String::new();
    };
    let Some(parameters) = bash.parameters.as_object() else {
        return String::new();
    };
    if parameters.get("type").and_then(Value::as_str) != Some("object") {
        return String::new();
    }
    let required: Vec<&str> = parameters
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if required
        .iter()
        .any(|name| *name != "command" && *name != "description")
    {
        return String::new();
    }
    let properties = parameters.get("properties").and_then(Value::as_object);
    let command = properties.and_then(|properties| properties.get("command"));
    if !accepts_example_string(command, "pwd") {
        return String::new();
    }
    let needs_description = required.contains(&"description");
    let description_schema = properties.and_then(|properties| properties.get("description"));
    if needs_description && !accepts_example_string(description_schema, "Show current directory") {
        return String::new();
    }
    let description = if needs_description {
        "description: 'Show current directory', "
    } else {
        ""
    };
    let program = format!("inspect(await tools.bash({{ {description}command: 'pwd' }}))");
    format!(
        " When no separate `bash` schema is supplied, invoke a declared `bash` binding inside \
         `run_code`:\n\n`run_code({{ code: \"{program}\", description: \"Show current directory\" }})`"
    )
}

/// Renders one tool's schema into the named position, degrading the whole node when it is outside
/// the subset.
///
/// This is the gate upstream applies inside `renderType`: a malformed schema is rejected before any
/// text is emitted for it, so one bad tool loses its own types and nothing else. Checking here rather
/// than at every nested node keeps the walk linear — the whole subtree is validated once, then
/// trusted, exactly as upstream's does after its single `assertSupportedJsonSchema`.
fn render_tool_schema(schema: &Value, class_name: &str, state: &mut RenderState) -> String {
    if !is_supported(schema) {
        state.need(TypingSymbol::Any);
        return "Any".to_owned();
    }
    render_type(
        schema,
        PyContext {
            naming: Naming::Named(class_name),
            list_depth: 0,
        },
        state,
    )
}

/// Renders the full `tools:sdk` prompt section under `runtime.language === 'python'`.
///
/// The fixed usage instructions, then one named `TypedDict` per tool argument or output object (and
/// per nested object), then one awaitable method per visible tool on a `Tools` protocol — typed
/// arguments in, the tool's canonical output value out — and a `tools: Tools` singleton the model
/// calls into.
///
/// Deterministic: tools are emitted in lexicographic name order, class declarations precede the
/// protocol, and nested classes precede the parent that references them, so an unchanged tool set
/// produces byte-identical text. The `typing` import line lists exactly the symbols the render used.
#[must_use]
pub fn render_tools_sdk_py(tools: &[SdkTool]) -> String {
    let sorted = tools_sorted(tools);
    let mut state = RenderState::new();
    let mut members: Vec<String> = Vec::new();
    let mut statements = 0;
    for tool in &sorted {
        let base = camel_case(&tool.name);
        let args_class = format!("{base}Args");
        let output_class = format!("{base}Output");
        let args = render_tool_schema(&tool.parameters, &args_class, &mut state);
        let output = render_tool_schema(&tool.output, &output_class, &mut state);
        if is_declarable_method(&tool.name) {
            // A docstring only documents its method when it is the FIRST statement of that method's
            // body. Emitted before the `async def` it would become the `Tools` class docstring (for
            // the first tool) or a dead expression (for every later one), leaving every method
            // undocumented — and under PTC execution this SDK is the model's only description of
            // what a tool does. A docstring is a complete body, so the `...` stub is only for the
            // description-less case.
            let doc = py_doc_lines(Some(&tool.description), 2);
            if doc.is_empty() {
                members.push(format!(
                    "{}async def {}(self, args: {args}) -> {output}: ...",
                    py_pad(1),
                    tool.name
                ));
            } else {
                members.push(format!(
                    "{}async def {}(self, args: {args}) -> {output}:",
                    py_pad(1),
                    tool.name
                ));
                members.extend(doc);
            }
            statements += 1;
        } else {
            // Not reachable as `tools.name`; the model reaches it via `tools[name]`. The stub lists
            // it as a subscript comment, referencing the named `TypedDict`s too, so a reader sees
            // what is accessible; runtime resolution goes through the proxy's `__getitem__`.
            members.push(format!(
                "{}# tools[{}](args: {args}) -> {output}",
                py_pad(1),
                quote(&tool.name)
            ));
            if let Some(description) = describe(Some(&tool.description), ProseTarget::Python) {
                members.push(format!("{}#   {description}", py_pad(1)));
            }
        }
    }
    // Subscript entries are COMMENTS, not statements: a class body of only comments fails to parse,
    // so `pass` is required whenever no method was emitted — including the subscript-only tool set.
    let mut body_lines = Vec::new();
    if statements == 0 {
        body_lines.push(format!("{}pass", py_pad(1)));
    }
    body_lines.extend(members);
    let class_block = if state.classes.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", state.classes.join("\n\n"))
    };
    let declaration = format!(
        "{}\n\n{PY_TOOL_CALL_ERROR}\n\n{class_block}class Tools(Protocol):\n{}\n\ntools: Tools",
        state.typing_imports(),
        body_lines.join("\n")
    );
    format!("{PY_SDK_INSTRUCTIONS}\n\n```python\n{declaration}\n```")
}

/// Renders the full `tools:sdk` prompt section for a TypeScript program.
///
/// The fixed usage instructions, then a `declare const tools` declaration whose `ToolArgsMap` and
/// `ToolOutputMap` are keyed by tool name — so a call's argument type and return type are the ones
/// the tool registered, with no second place for them to drift.
///
/// Two return types are overridden rather than projected: `read_image` returns an image attachment,
/// and `cua` may return either an image or its declared value. Their declared JSON schema would be a
/// lie the model programs against, so the declaration names `Image` instead.
#[must_use]
pub fn render_tools_sdk(tools: &[SdkTool], program_format: ProgramFormat) -> String {
    let sorted = tools_sorted(tools);
    let mut args_members: Vec<String> = Vec::new();
    let mut output_members: Vec<String> = Vec::new();
    for tool in &sorted {
        args_members.extend(ts_doc_lines(Some(&tool.description), 1));
        args_members.push(format!(
            "{}{}: {};",
            ts_pad(1),
            render_key(&tool.name),
            json_schema_to_ts(&tool.parameters, 1)
        ));
        let declared_output = json_schema_to_ts(&tool.output, 1);
        let output = match tool.name.as_str() {
            "read_image" => "Image".to_owned(),
            "cua" => format!("Image | {declared_output}"),
            _ => declared_output,
        };
        output_members.push(format!(
            "{}{}: {output};",
            ts_pad(1),
            render_key(&tool.name)
        ));
    }
    let args_map = render_interface("ToolArgsMap", &args_members);
    let output_map = render_interface("ToolOutputMap", &output_members);
    let tool_call_error = [
        "declare class ToolCallError extends Error {",
        "  readonly name: \"ToolCallError\";",
        "  readonly toolName: ToolName;",
        "}",
    ]
    .join("\n");
    let tools_binding = [
        "declare const tools: {",
        "  [K in ToolName]: (args: ToolArgsMap[K]) => Promise<ToolOutputMap[K]>;",
        "}",
    ]
    .join("\n");
    let declaration = [
        "declare class Image { private constructor(); readonly attachmentId: string; readonly \
         path: string; readonly width: number; readonly height: number; }"
            .to_owned(),
        "declare function inspect(...values: unknown[]): void;".to_owned(),
        args_map,
        output_map,
        "type ToolName = keyof ToolOutputMap".to_owned(),
        tool_call_error,
        tools_binding,
    ]
    .join("\n\n");
    let json_value = format!(
        "type {JSON_VALUE_ALIAS} = null | boolean | number | string | {JSON_VALUE_ALIAS}[] | {{ \
         [key: string]: {JSON_VALUE_ALIAS} }}"
    );
    let instructions = ts_sdk_instructions(program_format);
    let program_instructions = ts_program_instructions();
    let bash_example = render_bash_example(&sorted);
    format!(
        "{instructions}{bash_example}\n\n{program_instructions}\n\n```ts\n{json_value}\n\n\
         {declaration}\n```"
    )
}

/// Renders one `interface` with the given members, or an empty body when there are none.
fn render_interface(name: &str, members: &[String]) -> String {
    if members.is_empty() {
        return format!("interface {name} {{}}");
    }
    format!("interface {name} {{\n{}\n}}", members.join("\n"))
}
