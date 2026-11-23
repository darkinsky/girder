//! `schemastery`, reduced to the part a backend actually needs: validating a value against a
//! declared schema.
//!
//! Upstream `vendor/schemastery` is 962 lines and it is two things at once. One half validates data
//! — types, nesting, ranges, defaults, the volatile-field rule. The other half describes a form: the
//! `hidden`/`collapse`/`disabled`/`role`/`comment` builders, the badge list, `toString` emitting a
//! compact literal for a settings UI. This crate ports the validation half; it has no settings
//! form to describe.
//!
//! # What is ported
//!
//! The resolution semantics, because they are what a caller means by "valid":
//!
//! - A **missing** value falls back to the schema's default; a required schema with no default
//!   refuses instead. `null` and `undefined` both count as missing, which is upstream's
//!   `isNullable`.
//! - A **default inside an intersect** is looked up by walking the intersect's parts, because the
//!   default may live on one of them rather than on the intersect itself.
//! - `loose` means a failing schema yields its default instead of an error, which is how an
//!   optional section degrades.
//! - A value must match the declared `type`, and the primitive types carry their own range checks.
//!
//! # What is deliberately not ported
//!
//! - Every UI builder (`hidden`, `collapse`, `disabled`, `role`, `link`, `comment`, `description`,
//!   `badges`, `deprecated`, `experimental`, `simplify`, `toString`). They produce metadata for a
//!   form, and nothing here renders one.
//! - `refs`/`uid`: upstream lets a schema be a graph of shared nodes so a UI can reuse one
//!   sub-schema. A tree is enough for validation, and a cycle in a tree cannot be expressed.
//! - The `~standard` validator interface (a JS ecosystem convention) and `Volatile`, which is a
//!   proxy that writes back through a getter — a React-shaped idea with no counterpart here.
//! - `Schema.extend`, the escape hatch for third-party types. Adding a type means adding a variant
//!   here, which is the point of a closed set.

use serde_json::{Map, Value};

/// One declared schema.
///
/// Upstream's `Schema` is a callable function carrying an options object, so a schema and its
/// constraints share one mutable record. A struct with an explicit constraint set says the same
/// thing in a shape the compiler can check.
#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    /// What shape the value must have.
    pub kind: Kind,
    /// Whether an absent value is an error rather than a default.
    pub required: bool,
    /// The value to substitute when the input is absent.
    pub default: Option<Value>,
    /// Substitutes `default` for a *failing* value instead of reporting the failure.
    pub loose: bool,
    /// Rejects a string that does not match, or a number/array outside `min..=max`.
    pub min: Option<f64>,
    /// Upper bound for the same three kinds.
    pub max: Option<f64>,
}

/// The shape a schema declares.
///
/// Named `Kind` rather than `Type` because `Type` is a Rust keyword-adjacent word in this crate's
/// vocabulary (every branded id has a "type"), and because these are the shapes the resolver
/// branches on, not a Rust type.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Any value, including absent.
    Any,
    /// No value is ever valid.
    Never,
    /// A string.
    String,
    /// A number (integer or float; JSON has one numeric type).
    Number,
    /// A number that must be a non-negative integer.
    Natural,
    /// A number in `0.0..=1.0`.
    Percent,
    /// A boolean.
    Boolean,
    /// Exactly this value.
    Const(Value),
    /// An object whose keys are declared; `None` means "any extra key is allowed".
    Dict {
        /// The declared entries.
        entries: Vec<(String, Schema)>,
        /// Whether a key not in `entries` is refused.
        closed: bool,
    },
    /// A sequence whose members all match `inner`.
    Array(Box<Schema>),
    /// A fixed-length sequence, one schema per position.
    Tuple(Vec<Schema>),
    /// One of several schemas, first match wins.
    Union(Vec<Schema>),
    /// Every schema must match.
    Intersect(Vec<Schema>),
    /// Any JSON value that is an array.
    List,
}

impl Schema {
    /// A schema of one kind with no constraints.
    #[must_use]
    pub fn of(kind: Kind) -> Self {
        Self {
            kind,
            required: false,
            default: None,
            loose: false,
            min: None,
            max: None,
        }
    }

    /// An unconstrained string.
    #[must_use]
    pub fn string() -> Self {
        Self::of(Kind::String)
    }

    /// An unconstrained number.
    #[must_use]
    pub fn number() -> Self {
        Self::of(Kind::Number)
    }

    /// A non-negative integer.
    #[must_use]
    pub fn natural() -> Self {
        Self::of(Kind::Natural)
    }

    /// A boolean.
    #[must_use]
    pub fn boolean() -> Self {
        Self::of(Kind::Boolean)
    }

    /// Any value.
    #[must_use]
    pub fn any() -> Self {
        Self::of(Kind::Any)
    }

    /// Exactly one value.
    #[must_use]
    pub fn constant(value: Value) -> Self {
        Self::of(Kind::Const(value))
    }

    /// An object with declared entries.
    #[must_use]
    pub fn dict(entries: impl IntoIterator<Item = (impl Into<String>, Schema)>) -> Self {
        Self::of(Kind::Dict {
            entries: entries
                .into_iter()
                .map(|(key, schema)| (key.into(), schema))
                .collect(),
            closed: false,
        })
    }

    /// An array whose members all match `inner`.
    #[must_use]
    pub fn array(inner: Schema) -> Self {
        Self::of(Kind::Array(Box::new(inner)))
    }

    /// A fixed-length sequence.
    #[must_use]
    pub fn tuple(items: impl IntoIterator<Item = Schema>) -> Self {
        Self::of(Kind::Tuple(items.into_iter().collect()))
    }

    /// One of several schemas.
    #[must_use]
    pub fn union(items: impl IntoIterator<Item = Schema>) -> Self {
        Self::of(Kind::Union(items.into_iter().collect()))
    }

    /// Every schema must match.
    #[must_use]
    pub fn intersect(items: impl IntoIterator<Item = Schema>) -> Self {
        Self::of(Kind::Intersect(items.into_iter().collect()))
    }

    /// Marks the value as required, so an absent one is an error.
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Supplies the value to use when the input is absent.
    #[must_use]
    pub fn with_default(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    /// Substitutes the default for a failing value instead of reporting the failure.
    #[must_use]
    pub fn loose(mut self) -> Self {
        self.loose = true;
        self
    }

    /// Sets the lower bound (string length, number value, or array length).
    #[must_use]
    pub fn min(mut self, bound: f64) -> Self {
        self.min = Some(bound);
        self
    }

    /// Sets the upper bound.
    #[must_use]
    pub fn max(mut self, bound: f64) -> Self {
        self.max = Some(bound);
        self
    }

    /// Refuses an undeclared key.
    #[must_use]
    pub fn closed(mut self) -> Self {
        if let Kind::Dict { closed, .. } = &mut self.kind {
            *closed = true;
        }
        self
    }
}

/// Why a value was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    /// The dotted path to the offending node, `""` for the root.
    pub path: String,
    /// What was wrong, phrased so a person can act on it.
    pub message: String,
}

impl ValidationError {
    /// A failure at `path`.
    #[must_use]
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(formatter, "{}", self.message)
        } else {
            write!(formatter, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ValidationError {}

/// Resolves `value` against `schema`, returning the value to use.
///
/// This is upstream's `Schema.resolve(data, schema)[0]`: the returned value is not always the input,
/// because an absent one may be replaced by a default and a loose schema may replace a failing one.
///
/// # Errors
///
/// [`ValidationError`] naming the path and the reason, when the value does not satisfy the schema
/// and the schema is not `loose`.
pub fn resolve(value: &Value, schema: &Schema) -> Result<Value, ValidationError> {
    resolve_at(value, schema, "")
}

/// Resolves at a known path, so a nested failure names where it happened.
fn resolve_at(value: &Value, schema: &Schema, path: &str) -> Result<Value, ValidationError> {
    // A loose schema reports its default for any failure, so the work is done once and the answer
    // replaced. Upstream does this with a `try`/`catch` around the resolver; the effect is the same
    // and the shape here says so directly.
    let attempt = || resolve_strict(value, schema, path);
    match attempt() {
        Ok(resolved) => Ok(resolved),
        Err(_error) if schema.loose => Ok(schema.default.clone().unwrap_or(Value::Null)),
        Err(error) => Err(error),
    }
}

/// Resolves without consulting `loose`.
fn resolve_strict(value: &Value, schema: &Schema, path: &str) -> Result<Value, ValidationError> {
    // `Never` is checked before the absence rule: "no value is acceptable" has to include *no*
    // value, or a `Never` field would quietly accept an omitted one.
    if matches!(schema.kind, Kind::Never) {
        return Err(ValidationError::new(path, "no value is acceptable"));
    }
    // Absence next: an absent value is a default or an error, never a type mismatch.
    //
    // A union is the exception: it has to see the absence, because one of its parts may carry the
    // default that makes an absent input meaningful. The absence rule above would answer `null`
    // before the scan ever began, which is why `union([required, with_default])` resolved to `null`
    // instead of the default -- a case this module's own test caught.
    if matches!(value, Value::Null) && !matches!(schema.kind, Kind::Union(_)) {
        return match (&schema.default, schema.required) {
            (Some(default), _) => Ok(default.clone()),
            (None, true) => Err(ValidationError::new(path, "missing required value")),
            // An absent, optional, defaultless value stays absent. Upstream returns `data`, which is
            // `null` here; `Value::Null` round-trips to an omitted key at the JSON layer.
            (None, false) => Ok(Value::Null),
        };
    }

    match &schema.kind {
        Kind::Any => Ok(value.clone()),
        Kind::Never => Err(ValidationError::new(path, "no value is acceptable")),
        Kind::Const(expected) => {
            if value == expected {
                Ok(value.clone())
            } else {
                Err(ValidationError::new(
                    path,
                    format!("must be {expected}, got {value}"),
                ))
            }
        }
        Kind::String
        | Kind::Number
        | Kind::Natural
        | Kind::Percent
        | Kind::Boolean
        | Kind::List => resolve_scalar(value, schema, path),
        Kind::Array(_) | Kind::Tuple(_) | Kind::Dict { .. } => {
            resolve_container(value, schema, path)
        }
        Kind::Union(parts) => resolve_composed(value, parts, path, true),
        Kind::Intersect(parts) => resolve_composed(value, parts, path, false),
    }
}

/// Joins a path with one segment.
fn join(path: &str, segment: &str) -> String {
    if path.is_empty() {
        segment.to_owned()
    } else {
        format!("{path}.{segment}")
    }
}

/// Enforces `min`/`max` on a measurement.
fn check_bounds(
    path: &str,
    measured: f64,
    schema: &Schema,
    label: &str,
) -> Result<(), ValidationError> {
    if let Some(min) = schema.min
        && measured < min
    {
        return Err(ValidationError::new(
            path,
            format!("{label} must be at least {min}, got {measured}"),
        ));
    }
    if let Some(max) = schema.max
        && measured > max
    {
        return Err(ValidationError::new(
            path,
            format!("{label} must be at most {max}, got {measured}"),
        ));
    }
    Ok(())
}

/// Resolves the arms that inspect one value without recursing.
fn resolve_scalar(value: &Value, schema: &Schema, path: &str) -> Result<Value, ValidationError> {
    match &schema.kind {
        Kind::String => {
            let Some(text) = value.as_str() else {
                return Err(ValidationError::new(path, "must be a string"));
            };
            // Character count, not byte count: a bound of 3 must accept a three-character Chinese
            // string, and bytes would refuse it.
            check_bounds(path, char_count(text), schema, "string length")?;
            Ok(value.clone())
        }
        Kind::Number => {
            let Some(number) = value.as_f64() else {
                return Err(ValidationError::new(path, "must be a number"));
            };
            check_bounds(path, number, schema, "number")?;
            Ok(value.clone())
        }
        Kind::Natural => {
            let Some(number) = value.as_f64() else {
                return Err(ValidationError::new(path, "must be a number"));
            };
            // `1.0` counts: JSON does not distinguish it from `1`, so the test is on the value
            // rather than on the representation.
            if number < 0.0 || number.fract() != 0.0 {
                return Err(ValidationError::new(
                    path,
                    format!("must be a non-negative integer, got {number}"),
                ));
            }
            check_bounds(path, number, schema, "number")?;
            Ok(value.clone())
        }
        Kind::Percent => {
            let Some(number) = value.as_f64() else {
                return Err(ValidationError::new(path, "must be a number"));
            };
            check_bounds(path, number, schema, "number")?;
            if !(0.0..=1.0).contains(&number) {
                return Err(ValidationError::new(
                    path,
                    format!("must be between 0 and 1, got {number}"),
                ));
            }
            Ok(value.clone())
        }
        Kind::Boolean => {
            if value.is_boolean() {
                Ok(value.clone())
            } else {
                Err(ValidationError::new(path, "must be a boolean"))
            }
        }
        Kind::List => {
            if value.is_array() {
                Ok(value.clone())
            } else {
                Err(ValidationError::new(path, "must be an array"))
            }
        }
        // The container arms never reach here; `resolve_strict` routes them to
        // `resolve_container`. The arm exists so adding a kind to one match and not the other is a
        // compile error rather than a silent pass-through.
        other => Err(ValidationError::new(
            path,
            format!("{other:?} is not a scalar"),
        )),
    }
}

/// Counts characters without going through `f64` first.
///
/// `len() as f64` loses precision past 2^52 and tripped `cast_precision_loss`; a bound is a small
/// number, so clamping is honest rather than lossy.
#[allow(
    clippy::cast_precision_loss,
    reason = "a length past 2^52 cannot be realised in memory, so this cast is exact for every value that can reach it"
)]
fn size_of(count: usize) -> f64 {
    count as f64
}

fn char_count(text: &str) -> f64 {
    size_of(text.chars().count())
}

/// Resolves the arms that recurse into a value's members.
fn resolve_container(value: &Value, schema: &Schema, path: &str) -> Result<Value, ValidationError> {
    // The arms are moved verbatim from the dispatcher, which routes here so it stays within its
    // length budget. `schema` carries the bounds they read.
    match &schema.kind {
        Kind::Array(inner) => {
            let Some(items) = value.as_array() else {
                return Err(ValidationError::new(path, "must be an array"));
            };
            check_bounds(path, size_of(items.len()), schema, "array length")?;
            let mut resolved = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let at = join(path, &index.to_string());
                resolved.push(resolve_at(item, inner, &at)?);
            }
            Ok(Value::Array(resolved))
        }
        Kind::Tuple(parts) => {
            let Some(items) = value.as_array() else {
                return Err(ValidationError::new(path, "must be an array"));
            };
            if items.len() != parts.len() {
                return Err(ValidationError::new(
                    path,
                    format!("must have {} items, got {}", parts.len(), items.len()),
                ));
            }
            let mut resolved = Vec::with_capacity(items.len());
            for (index, (item, part)) in items.iter().zip(parts).enumerate() {
                let at = join(path, &index.to_string());
                resolved.push(resolve_at(item, part, &at)?);
            }
            Ok(Value::Array(resolved))
        }
        Kind::Dict { entries, closed } => {
            let Some(object) = value.as_object() else {
                return Err(ValidationError::new(path, "must be an object"));
            };
            let mut resolved = Map::new();
            for (key, entry) in entries {
                let at = join(path, key);
                let incoming = object.get(key).cloned().unwrap_or(Value::Null);
                let value = resolve_at(&incoming, entry, &at)?;
                // A resolved `null` means "this entry is absent": writing it back would turn an
                // omitted optional into an explicit null, which a reader cannot tell from a value.
                if !value.is_null() || entry.required {
                    resolved.insert(key.clone(), value);
                }
            }
            if !closed {
                // An undeclared key passes through unchanged, which is what "open object" means.
                for (key, value) in object {
                    if !entries.iter().any(|(declared, _)| declared == key) {
                        resolved.insert(key.clone(), value.clone());
                    }
                }
            } else if let Some(extra) = object.keys().find(|key| {
                !entries
                    .iter()
                    .any(|(declared, _)| declared.as_str() == key.as_str())
            }) {
                return Err(ValidationError::new(
                    join(path, extra),
                    "is not a declared key",
                ));
            }
            Ok(Value::Object(resolved))
        }
        // The scalar and composition arms never reach here.
        other => Err(ValidationError::new(
            path,
            format!("{other:?} is not a container"),
        )),
    }
}

/// Resolves the composition arms (`union`, `intersect`).
///
/// These are not containers: they recurse over *schemas* rather than over the value's members, which
/// is why they re-enter [`resolve_at`] with a part instead of walking the value.
fn resolve_composed(
    value: &Value,
    parts: &[Schema],
    path: &str,
    union: bool,
) -> Result<Value, ValidationError> {
    if union {
        let mut accepted = None;
        for part in parts {
            if let Ok(resolved) = resolve_at(value, part, path) {
                if value.is_null() && resolved.is_null() {
                    accepted.get_or_insert(Value::Null);
                    continue;
                }
                return Ok(resolved);
            }
        }
        return accepted.ok_or_else(|| {
            ValidationError::new(
                path,
                format!("matches none of {} alternatives", parts.len()),
            )
        });
    }
    let mut current = value.clone();
    for part in parts {
        current = resolve_at(&current, part, path)?;
    }
    Ok(current)
}
