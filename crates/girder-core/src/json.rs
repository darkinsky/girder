//! Lossless JSON for values crossing a transport boundary.
//!
//! Upstream's `isRemoteJsonValue` answers one question: does this value survive JSON transport
//! unchanged? It exists because JavaScript has values JSON cannot carry — `NaN`, `Infinity`, `-0`,
//! circular references, sparse arrays, `Date`, `Map` — and all of them are *silently* coerced rather
//! than rejected.
//!
//! # What changes in Rust
//!
//! Most of that list is not expressible here, and one case changes shape entirely:
//!
//! | hazard | JavaScript | Rust |
//! |---|---|---|
//! | `NaN` / `Infinity` | rejected by the check | **cannot exist in a `Value`**: `Number::from_f64` returns `None` and `Value::from(NaN)` is `Null` |
//! | `-0` becomes `0` | rejected | preserved by `serde_json`, not a hazard |
//! | circular reference | rejected | not constructible |
//! | sparse array | rejected | not constructible |
//! | `Date` / `Map` / class instance | rejected | not constructible |
//! | **integer above 2^53 loses precision** | impossible (every number is f64) | **the remaining hazard** |
//!
//! So this module checks exactly one thing about values that already exist, plus provides the
//! constructors that refuse the bad ones before they become values.
//!
//! The integer case is the one that matters in practice: the migration carries 64-bit counters, byte
//! sizes, and timestamps. `9007199254740993` as `u64` round-trips fine through `serde_json`, but the
//! moment a JavaScript client parses it, it becomes `9007199254740992`. The check refuses it at the
//! boundary rather than letting the two sides disagree.
//!
//! # Why `hazards` cannot report a non-finite number
//!
//! It takes a `Value`, and a `Value` cannot hold one. The guard against non-finite floats therefore
//! lives in [`number_from_f64`], at construction, not in the walk. A caller with a raw `f64` must go
//! through that constructor rather than `Value::from`, which coerces silently.

use serde_json::Value;

/// The largest integer a JavaScript `number` represents exactly.
///
/// `2^53 - 1`. Beyond this, `f64` cannot distinguish consecutive integers, so a value that survives
/// Rust's own round trip can still be corrupted by any JavaScript client.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// The most negative integer a JavaScript `number` represents exactly.
pub const MIN_SAFE_INTEGER: i64 = -9_007_199_254_740_991;

/// A reason a value cannot cross JSON transport unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsonHazard {
    /// A float that JSON cannot represent; it would become `null`.
    ///
    /// Reported by [`number_from_f64`], never by [`hazards`]: a `Value` cannot hold one.
    NonFinite {
        /// The value, formatted, so the error names what was wrong.
        value: String,
    },

    /// An integer outside the range JavaScript represents exactly.
    UnsafeInteger {
        /// The offending value, formatted.
        value: String,
    },
}

impl std::fmt::Display for JsonHazard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFinite { value } => {
                write!(
                    f,
                    "`{value}` is not representable in JSON and would become `null`"
                )
            }
            Self::UnsafeInteger { value } => write!(
                f,
                "`{value}` exceeds 2^53 and would lose precision in a JavaScript client"
            ),
        }
    }
}

/// Checks whether `value` crosses JSON transport unchanged.
///
/// Returns every hazard found, not only the first: a payload with two bad numbers should report
/// both, or the caller fixes them one boundary error at a time.
#[must_use]
pub fn hazards(value: &Value) -> Vec<JsonHazard> {
    let mut found = Vec::new();
    visit(value, &mut found);
    found
}

/// Whether `value` crosses JSON transport unchanged.
#[must_use]
pub fn is_lossless(value: &Value) -> bool {
    hazards(value).is_empty()
}

fn visit(value: &Value, found: &mut Vec<JsonHazard>) {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => {}
        Value::Number(number) => {
            // No non-finite check here: `serde_json::Number` cannot hold one. The guard for a raw
            // float is `number_from_f64`, at construction.
            //
            // An integer is unsafe only when JavaScript cannot represent it exactly; a float that
            // happens to be integral in range is fine, and a fractional float is fine at any
            // magnitude within f64.
            if number.is_u64() {
                if let Some(raw) = number.as_u64() {
                    if raw > MAX_SAFE_INTEGER {
                        found.push(JsonHazard::UnsafeInteger {
                            value: raw.to_string(),
                        });
                    }
                }
            } else if number.is_i64() {
                if let Some(raw) = number.as_i64() {
                    if raw < MIN_SAFE_INTEGER {
                        found.push(JsonHazard::UnsafeInteger {
                            value: raw.to_string(),
                        });
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                visit(item, found);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                visit(item, found);
            }
        }
    }
}

/// Builds a JSON number from a float, refusing the ones JSON cannot carry.
///
/// The constructor to use instead of `Value::from(f64)`, which turns `NaN` into `null` without a
/// word.
///
/// # Errors
///
/// Returns [`JsonHazard::NonFinite`] for `NaN` and both infinities.
pub fn number_from_f64(value: f64) -> Result<Value, JsonHazard> {
    if !value.is_finite() {
        return Err(JsonHazard::NonFinite {
            value: format!("{value}"),
        });
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or(JsonHazard::NonFinite {
            value: format!("{value}"),
        })
}

/// Builds a JSON number from a `u64`, refusing anything a JavaScript client would corrupt.
///
/// # Errors
///
/// Returns [`JsonHazard::UnsafeInteger`] above [`MAX_SAFE_INTEGER`].
pub fn number_from_u64(value: u64) -> Result<Value, JsonHazard> {
    if value > MAX_SAFE_INTEGER {
        return Err(JsonHazard::UnsafeInteger {
            value: value.to_string(),
        });
    }
    Ok(Value::Number(serde_json::Number::from(value)))
}

/// Normalizes a value into its JSON round trip, so two producers agree on what was sent.
///
/// Useful as a last step before handing a payload to a transport: whatever `serde_json` would do to
/// it is applied once, here, where the caller can compare input and output.
#[must_use]
pub fn normalize(value: &Value) -> Value {
    // `serde_json` round-trips through text; doing it explicitly makes the normalization visible
    // rather than an implicit consequence of the transport.
    serde_json::from_str(&value.to_string()).unwrap_or(Value::Null)
}
