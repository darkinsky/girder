//! Standardized tool-failure kinds and the argument contract check for a tool call.
//!
//! Today [`crate::ToolError`] carries a free-form `message` and three variants; every tool decides
//! for itself what a failure *means*. That is enough to print an error, and not enough to act on
//! one: a caller cannot tell "the model sent a bad argument" (retry with a different call) from
//! "the file is not there" (retry with a different path) from "the permission was refused" (stop
//! and ask), so the only available reaction is to show the text to the model and hope.
//!
//! This module gives a failure a standardized *kind* plus one thing the error text alone never
//! carries: a **remedy**, the change that would make the call succeed. The distinction matters
//! because a model that reads only "invalid arguments" re-sends the same call with different
//! whitespace; a model that reads "add `path`; it is required" fixes the call.
//!
//! # Relationship to upstream error codes
//!
//! Upstream `packages/core/tools/src/error-codes.ts` defines a small set of *persisted contract*
//! strings, and `schema.ts` adds one more:
//!
//! | upstream | where | this module |
//! |---|---|---|
//! | `INVALID_ARGS` | `ToolArgsError.code` | [`FailureKind::InvalidArguments`] |
//! | `APPROVAL_REJECTED` | `error-codes.ts` | [`FailureKind::PermissionDenied`] |
//! | `APPROVAL_CANCELLED` | `error-codes.ts` | [`FailureKind::Cancelled`] |
//! | `APPROVAL_UNAVAILABLE` | `error-codes.ts` | [`FailureKind::ToolFailed`] |
//! | `TOOL_CONTEXT_UNAVAILABLE` | `error-codes.ts` | [`FailureKind::ToolFailed`] |
//! | `ApprovalError` | error *name*, not a code | no counterpart; see below |
//!
//! Three differences are deliberate:
//!
//! - Upstream has no code for "not found" or "timed out". Its executor and file tools report those
//!   as free-form messages, so [`FailureKind::NotFound`] and [`FailureKind::Timeout`] are additions
//!   rather than translations. They are the two failures a caller most often recovers from, which
//!   is why they get kinds of their own.
//! - `APPROVAL_UNAVAILABLE` means *the environment could not ask anyone*, so it is an environment
//!   failure ([`FailureKind::ToolFailed`]) and not a denial. Merging it into
//!   [`FailureKind::PermissionDenied`] would tell the caller to ask for permission it already tried
//!   to ask for.
//! - `ApprovalError` is a JavaScript error *class* carrying a code. Rust needs no class: the kind
//!   is the discriminant and the text is the message. [`FailureKind::code`] is the stable half, and
//!   [`FailureKind::from_code`] reads it back.
//!
//! # Which part of JSON Schema is enforced
//!
//! The full specification is thousands of lines and most of it is unused by the tools in this
//! repository, so this check implements the subset upstream enforces (see its `json-schema.ts`),
//! plus the two literal constraints:
//!
//! - `type`: one of `object`, `array`, `string`, `number`, `integer`, `boolean`, `null`.
//! - `properties` and `required` on objects.
//! - `additionalProperties: false` — undeclared keys are rejected. Absent or `true` follows JSON
//!   Schema's open default.
//! - `items` on arrays, applied to every element.
//! - `enum` and `const`, compared by JSON equality.
//! - A boolean schema: `true` accepts anything, `false` accepts nothing.
//! - Annotation-only schemas (`description`, `title`, `default`, `examples`) with no `type`
//!   constrain nothing, so an empty schema accepts any arguments.
//!
//! Deliberately **not** enforced, because upstream rejects them at definition time rather than
//! validating them at call time, and a half-implementation of a constraint is worse than a clear
//! refusal:
//!
//! - `oneOf`, `anyOf`, `allOf`, `not`, `if`/`then`/`else`.
//! - `$ref` and remote references.
//! - Numeric bounds (`minimum`, `maximum`, `multipleOf`), string shapes (`pattern`, `minLength`,
//!   `format`), array bounds (`minItems`, `maxItems`, `uniqueItems`).
//! - Tuple-form `items` (an array of schemas), and `required ⊆ properties` consistency: a schema
//!   error here surfaces as a violation of kind [`FailureKind::ToolFailed`] when a call is checked,
//!   not when the tool is registered. Validation is the last line of defence; registration-time
//!   checks belong to the registry that owns the schema.
//!
//! # Why every violation carries a remedy
//!
//! A violation is read by the party that can fix it. Without a remedy the reader has to infer the
//! fix from a path and a type name, and the common failure — a model re-sending the same call —
//! shows that inference is unreliable. Each constructor below therefore fills `remedy` with the
//! concrete edit, and a schema-authoring problem says so instead of blaming the caller.

use serde_json::{Map, Value};

/// How deeply a schema and its value may nest before the check stops descending.
///
/// `serde_json` refuses to *parse* more than 128 nested containers, so a deeper tree can only be
/// built programmatically. The cap exists so that a hand-built or future-format value cannot turn
/// recursive validation into a stack overflow; the check reports a violation instead of aborting
/// the process.
pub const MAX_DEPTH: usize = 64;

/// The canonical reason a tool call failed.
///
/// The variants are the distinctions a *caller* can act on, not the places in the code that raise
/// errors. Two failures share a kind whenever the reaction to them is the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailureKind {
    /// The arguments did not match the tool's schema.
    ///
    /// The caller can fix this by changing the call. See [`validate_arguments`].
    InvalidArguments,

    /// Something the tool needed does not exist: a file, a directory, a command, a session.
    ///
    /// Distinct from [`Self::InvalidArguments`]: the call was well-formed and the answer is still
    /// "no". Retrying the identical call cannot help, but naming an existing target can.
    NotFound,

    /// The caller is not allowed to do this.
    ///
    /// Covers a refused approval (`APPROVAL_REJECTED` upstream) and a sandbox or policy denial.
    /// Retrying cannot help until the permission changes.
    PermissionDenied,

    /// The call exceeded its deadline.
    ///
    /// The tool may have made partial progress; the caller cannot assume it had none.
    Timeout,

    /// The tool itself failed.
    ///
    /// The catch-all for environment faults the caller did not cause and cannot fix by changing
    /// the call: a crashed subprocess, an unreadable device, a missing tool context, or approval
    /// being unavailable because no channel could answer.
    ToolFailed,

    /// The call was cancelled before it finished.
    ///
    /// Usually the user interrupted, so the caller should stop rather than retry. Covers
    /// `APPROVAL_CANCELLED` upstream.
    Cancelled,
}

impl FailureKind {
    /// Every kind, in a stable order.
    ///
    /// Public so callers can enumerate the code set (for a metrics label, an exhaustive match in a
    /// test, or a console legend) without duplicating the list.
    pub const ALL: [Self; 6] = [
        Self::InvalidArguments,
        Self::NotFound,
        Self::PermissionDenied,
        Self::Timeout,
        Self::ToolFailed,
        Self::Cancelled,
    ];

    /// The stable, machine-readable code for this kind.
    ///
    /// These strings are a *contract*: logs, metrics labels, and persisted transcripts compare
    /// them, so changing one is a breaking change. The spelling matches upstream only where
    /// upstream has a code; [`Self::InvalidArguments`] is `INVALID_ARGUMENTS` rather than upstream's
    /// `INVALID_ARGS` because the two sets are read by different consumers and a silent near-match
    /// is worse than an explicit table in the module docs.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidArguments => "INVALID_ARGUMENTS",
            Self::NotFound => "NOT_FOUND",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::Timeout => "TIMEOUT",
            Self::ToolFailed => "TOOL_FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }

    /// Reads a code produced by [`Self::code`] back into a kind.
    ///
    /// Returns `None` for an unknown code rather than guessing a nearest match: a caller decoding a
    /// persisted record must be able to tell an older or newer writer from a corrupted one.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.code() == code)
    }
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

/// One way a tool call broke its contract.
///
/// Produced by [`validate_arguments`] and [`violations`]. The pair of `message` and `remedy` is the
/// point of the type: `message` says what is wrong and where, `remedy` says what to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// What kind of failure this is, so a caller can branch without parsing text.
    pub kind: FailureKind,

    /// What is wrong, with the path of the offending argument.
    ///
    /// The path is rooted at `args`, e.g. `` args.query[0].limit ``.
    pub message: String,

    /// The change that would make the call valid.
    ///
    /// Never empty for a violation built by this module. It names the edit (`add the field`,
    /// `pass a string`, `fix the schema`) and, where the schema is at fault, says so, so the reader
    /// does not try to fix an argument that was never going to be accepted.
    pub remedy: String,
}

impl Violation {
    /// Builds a violation from its parts.
    ///
    /// Callers outside this module occasionally know a failure that no schema check can express;
    /// this is the one constructor that takes all three fields, so they are always explicit.
    #[must_use]
    pub fn new(kind: FailureKind, message: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            remedy: remedy.into(),
        }
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}: {} (fix: {})",
            self.kind, self.message, self.remedy
        )
    }
}

/// Checks arguments against a tool's schema, reporting the first problem found.
///
/// Returns `Ok(())` when the arguments satisfy the supported schema subset (see the module docs for
/// exactly which keywords that is). On failure the violation's `remedy` states the change that
/// would make the call valid.
///
/// Only the first violation is returned, because a caller that fixes one problem usually re-checks
/// anyway. Use [`violations`] to report all of them at once, as upstream's validator does.
///
/// # Errors
///
/// Returns a [`Violation`] when a required property is missing, a value does not match its declared
/// `type`, an undeclared property appears under `additionalProperties: false`, an array element
/// does not match `items`, a value falls outside `enum`/`const`, or the schema itself is
/// unsupported or malformed (reported as [`FailureKind::ToolFailed`], since no argument could
/// satisfy it).
pub fn validate_arguments(schema: &Value, args: &Value) -> Result<(), Violation> {
    violations(schema, args)
        .into_iter()
        .next()
        .map_or(Ok(()), Err)
}

/// Checks arguments against a tool's schema, reporting every problem found.
///
/// Upstream's `validateJsonSchemaValue` returns the complete list in schema-walk order, and a model
/// that receives all violations in one message needs one turn fewer than a model told about them
/// one at a time. Order is deterministic: `required` names in declaration order, then properties in
/// schema order (the map is a `BTreeMap`, so also key order), then `additionalProperties`.
#[must_use]
pub fn violations(schema: &Value, args: &Value) -> Vec<Violation> {
    let mut found = Vec::new();
    check(schema, args, "args", 0, &mut found);
    found
}

/// The JSON types this subset understands, spelled as they appear in a schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JsonType {
    Object,
    Array,
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

impl JsonType {
    /// Maps a schema `type` name to a variant; `None` means the schema is unsupported.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "object" => Some(Self::Object),
            "array" => Some(Self::Array),
            "string" => Some(Self::String),
            "number" => Some(Self::Number),
            "integer" => Some(Self::Integer),
            "boolean" => Some(Self::Boolean),
            "null" => Some(Self::Null),
            _ => None,
        }
    }

    /// The article-plus-noun phrase used in "expected ..., found ...".
    ///
    /// This is the schema spelling with an article, so switching on the enum keeps the diagnostic
    /// and the `type` keyword in step: adding a type cannot leave the message describing another.
    const fn noun(self) -> &'static str {
        match self {
            Self::Object => "an object",
            Self::Array => "an array",
            Self::String => "a string",
            Self::Number => "a number",
            Self::Integer => "an integer",
            Self::Boolean => "a boolean",
            Self::Null => "null",
        }
    }
}

/// Walks one schema node against one value, appending every violation to `found`.
fn check(schema: &Value, value: &Value, path: &str, depth: usize, found: &mut Vec<Violation>) {
    if depth > MAX_DEPTH {
        found.push(schema_error(format!(
            "the schema for `{path}` nests deeper than {MAX_DEPTH} levels"
        )));
        return;
    }
    let Some(node) = schema.as_object() else {
        check_non_object_schema(schema, path, found);
        return;
    };
    let declared = match node.get("type") {
        None => None,
        Some(Value::String(name)) => {
            let Some(kind) = JsonType::parse(name) else {
                found.push(schema_error(format!(
                    "the schema for `{path}` declares unsupported type `{name}`"
                )));
                return;
            };
            Some(kind)
        }
        Some(other) => {
            found.push(schema_error(format!(
                "the schema for `{path}` declares `type` as {other}, but it must be a string"
            )));
            return;
        }
    };
    if let Some(kind) = declared
        && !type_matches(kind, value)
    {
        found.push(Violation::new(
            FailureKind::InvalidArguments,
            format!(
                "`{path}` must be {}, found {}",
                kind.noun(),
                describe(value)
            ),
            format!(
                "pass `{path}` as {}, or change the schema for `{path}` if {} is intended",
                kind.noun(),
                describe(value)
            ),
        ));
        return;
    }
    check_literals(node, value, path, found);
    match declared {
        Some(JsonType::Object) => check_object(node, value, path, depth, found),
        Some(JsonType::Array) => check_array(node, value, path, depth, found),
        _ => {}
    }
}

/// Handles a schema that is not an object: boolean schemas, which JSON Schema allows, and
/// malformed ones, which it does not.
fn check_non_object_schema(schema: &Value, path: &str, found: &mut Vec<Violation>) {
    match schema {
        Value::Bool(true) => {}
        Value::Bool(false) => found.push(schema_error(format!(
            "the schema for `{path}` is `false`, which no value satisfies"
        ))),
        other => found.push(schema_error(format!(
            "the schema for `{path}` is {other}, but a schema must be an object or a boolean"
        ))),
    }
}

/// Applies `enum` and `const` to an already type-checked value.
fn check_literals(
    node: &Map<String, Value>,
    value: &Value,
    path: &str,
    found: &mut Vec<Violation>,
) {
    if let Some(constraint) = node.get("const")
        && constraint != value
    {
        found.push(Violation::new(
            FailureKind::InvalidArguments,
            format!("`{path}` must equal {constraint}, found {value}"),
            format!("pass `{path}` as {constraint}"),
        ));
    }
    if let Some(allowed) = node.get("enum") {
        let Some(allowed) = allowed.as_array() else {
            found.push(schema_error(format!(
                "the schema for `{path}` declares `enum` as {}, but it must be an array",
                node.get("enum")
                    .map_or_else(|| "null".to_owned(), ToString::to_string)
            )));
            return;
        };
        if !allowed.contains(value) {
            found.push(Violation::new(
                FailureKind::InvalidArguments,
                format!(
                    "`{path}` must be one of {}, found {value}",
                    render_allowed(allowed)
                ),
                format!(
                    "pass `{path}` as one of {}; these are the only accepted values",
                    render_allowed(allowed)
                ),
            ));
        }
    }
}

/// Renders up to eight allowed values for a diagnostic.
///
/// The cap keeps an error message readable: a tool with a hundred-value enum should not paste the
/// whole list into a failed call, and the first few values are enough to show the caller the shape
/// the tool expects.
fn render_allowed(allowed: &[Value]) -> String {
    const SHOWN: usize = 8;
    let mut rendered: Vec<String> = allowed
        .iter()
        .take(SHOWN)
        .map(ToString::to_string)
        .collect();
    if allowed.len() > SHOWN {
        rendered.push(format!("... and {} more", allowed.len() - SHOWN));
    }
    rendered.join(", ")
}

/// Applies the object keywords: `required`, `properties`, `additionalProperties`.
fn check_object(
    node: &Map<String, Value>,
    value: &Value,
    path: &str,
    depth: usize,
    found: &mut Vec<Violation>,
) {
    let Some(object) = value.as_object() else {
        return;
    };
    check_required(node, object, path, found);
    let declared = match node.get("properties") {
        None => Map::new(),
        Some(Value::Object(properties)) => {
            for (key, child) in properties {
                if let Some(field) = object.get(key) {
                    check(child, field, &property_path(path, key), depth + 1, found);
                }
            }
            properties.clone()
        }
        Some(other) => {
            found.push(schema_error(format!(
                "the schema for `{path}` declares `properties` as {other}, but it must be an object"
            )));
            Map::new()
        }
    };
    if matches!(node.get("additionalProperties"), Some(Value::Bool(false))) {
        for key in object.keys() {
            if !declared.contains_key(key) {
                let field = property_path(path, key);
                found.push(Violation::new(
                    FailureKind::InvalidArguments,
                    format!(
                        "`{field}` is not a declared property of `{path}` \
                         (the schema sets `additionalProperties: false`)"
                    ),
                    format!(
                        "remove `{key}` from the call, or add it to the schema's `properties` if \
                         the tool should accept it"
                    ),
                ));
            }
        }
    }
}

/// Reports each `required` name the call did not supply.
fn check_required(
    node: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    found: &mut Vec<Violation>,
) {
    let Some(required) = node.get("required") else {
        return;
    };
    let Some(names) = required.as_array() else {
        found.push(schema_error(format!(
            "the schema for `{path}` declares `required` as {required}, but it must be an array"
        )));
        return;
    };
    for name in names {
        let Some(name) = name.as_str() else {
            found.push(schema_error(format!(
                "the schema for `{path}` lists the non-string {name} in `required`"
            )));
            continue;
        };
        if !object.contains_key(name) {
            let field = property_path(path, name);
            found.push(Violation::new(
                FailureKind::InvalidArguments,
                format!("`{field}` is required but was not supplied"),
                format!(
                    "add `{name}` to the call; it is listed in the schema's `required`, or remove \
                     it from `required` if the tool can work without it"
                ),
            ));
        }
    }
}

/// Applies the array keyword `items` to every element.
fn check_array(
    node: &Map<String, Value>,
    value: &Value,
    path: &str,
    depth: usize,
    found: &mut Vec<Violation>,
) {
    let Some(array) = value.as_array() else {
        return;
    };
    let Some(items) = node.get("items") else {
        return;
    };
    match items {
        Value::Object(_) | Value::Bool(_) => {
            for (index, entry) in array.iter().enumerate() {
                check(items, entry, &index_path(path, index), depth + 1, found);
            }
        }
        Value::Null => {}
        other => found.push(schema_error(format!(
            "the schema for `{path}` declares tuple-form `items` as {other}; only a single item \
             schema is supported"
        ))),
    }
}

/// Whether `value` satisfies `kind`.
fn type_matches(kind: JsonType, value: &Value) -> bool {
    match kind {
        JsonType::Object => value.is_object(),
        JsonType::Array => value.is_array(),
        JsonType::String => value.is_string(),
        JsonType::Number => value.is_number(),
        JsonType::Integer => is_json_integer(value),
        JsonType::Boolean => value.is_boolean(),
        JsonType::Null => value.is_null(),
    }
}

/// Whether a JSON number is integral.
///
/// `3.0` counts: JSON has one number type, and a producer that writes `3.0` means three. The
/// comparison is against `f64::EPSILON` rather than `0.0` because a direct float equality is a lint
/// and, more importantly, `fract` is exactly zero for every value that reaches here, so a tolerance
/// expresses the intent without changing the answer.
fn is_json_integer(value: &Value) -> bool {
    let Some(number) = value.as_number() else {
        return false;
    };
    if number.is_i64() || number.is_u64() {
        return true;
    }
    number
        .as_f64()
        .is_some_and(|float| float.is_finite() && float.fract().abs() < f64::EPSILON)
}

/// Names the JSON shape of a value, for "expected a string, found a number" diagnostics.
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Appends `.key` for identifier-shaped keys and `["key"]` otherwise, so a path stays unambiguous.
fn property_path(path: &str, key: &str) -> String {
    let identifier = !key.is_empty() && key.chars().all(|ch| ch.is_alphanumeric() || ch == '_');
    if identifier {
        format!("{path}.{key}")
    } else {
        format!("{path}[{key:?}]")
    }
}

/// Appends `[index]` for an array element.
fn index_path(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

/// A violation that blames the schema, not the caller.
///
/// Every schema-authoring problem uses the same remedy wording on purpose: the caller cannot act on
/// it, and a message that invited a retry would send a model into a loop of identical calls.
fn schema_error(message: String) -> Violation {
    Violation::new(
        FailureKind::ToolFailed,
        message,
        "fix the tool definition: this is a schema authoring error, so no argument value can \
         satisfy it",
    )
}
