//! Tests for lossless JSON (M1 of the v2 plan).
//!
//! Upstream's check exists because JavaScript silently coerces values JSON cannot carry. Most of
//! that list cannot be expressed in Rust, so what is verified here is the part that remains — plus
//! the hazard Rust has that JavaScript does not: integers above 2^53.

use girder_core::{
    JsonHazard, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER, hazards, is_lossless, normalize,
    number_from_f64, number_from_u64,
};
use serde_json::json;

#[test]
fn ordinary_values_are_lossless() {
    for value in [
        json!(null),
        json!(true),
        json!("text"),
        json!(0),
        json!(-1),
        json!(1.5),
        json!([1, 2, 3]),
        json!({"a": [1, {"b": null}]}),
    ] {
        assert!(is_lossless(&value), "{value} must be lossless");
    }
}

#[test]
fn a_value_cannot_hold_a_non_finite_number() {
    // The premise of the whole module: `serde_json` refuses at construction, so `hazards` never sees
    // one. This is the difference from JavaScript, where `NaN` is a value that silently becomes
    // `null` on the way out.
    assert_eq!(serde_json::Value::from(f64::NAN), json!(null));
    assert_eq!(serde_json::Value::from(f64::INFINITY), json!(null));
    assert!(serde_json::Number::from_f64(f64::NAN).is_none());
    // And so the walk reports nothing for a payload built that way: the loss already happened.
    assert!(is_lossless(&json!({"ratio": f64::NAN})));
}

#[test]
fn the_constructor_refuses_what_the_value_type_already_dropped() {
    // `number_from_f64` is the guard at construction, which is where the check has to live.
    let error = number_from_f64(f64::NAN).expect_err("NaN has no JSON form");
    assert!(matches!(error, JsonHazard::NonFinite { .. }));
    assert!(
        error.to_string().contains("null"),
        "the message must say what happens"
    );
}

#[test]
fn building_a_json_number_from_a_non_finite_float_fails() {
    // The constructor that refuses, versus `Value::from(f64)` which coerces.
    assert!(number_from_f64(f64::NAN).is_err());
    assert!(number_from_f64(f64::INFINITY).is_err());
    assert_eq!(number_from_f64(1.5).unwrap(), json!(1.5));
}

#[test]
fn a_large_integer_is_a_hazard() {
    // The hazard Rust has and JavaScript does not: 64-bit counters and byte sizes exist here, and a
    // JavaScript client parsing 9007199254740993 receives 9007199254740992.
    let too_big = MAX_SAFE_INTEGER + 1;
    let found = hazards(&json!({ "bytes": too_big }));

    assert_eq!(found.len(), 1, "{found:?}");
    let JsonHazard::UnsafeInteger { value } = &found[0] else {
        panic!("expected UnsafeInteger, got {:?}", found[0]);
    };
    assert!(value.contains(&too_big.to_string()));
    assert!(found[0].to_string().contains("2^53"));
}

#[test]
fn the_boundary_integer_is_accepted() {
    // Off-by-one at the limit would reject a legitimate id.
    assert!(is_lossless(&json!(MAX_SAFE_INTEGER)));
    assert!(is_lossless(&json!(MIN_SAFE_INTEGER)));
}

#[test]
fn a_negative_integer_below_the_boundary_is_a_hazard() {
    let found = hazards(&json!(MIN_SAFE_INTEGER - 1));
    assert!(matches!(
        found.first(),
        Some(JsonHazard::UnsafeInteger { .. })
    ));
}

#[test]
fn building_an_unsafe_integer_fails() {
    assert!(number_from_u64(MAX_SAFE_INTEGER + 1).is_err());
    assert_eq!(
        number_from_u64(MAX_SAFE_INTEGER).unwrap(),
        json!(MAX_SAFE_INTEGER)
    );
}

#[test]
fn a_fractional_float_is_not_an_integer_hazard() {
    // Precision of a fractional value is a different question; the check is about integer identity.
    assert!(is_lossless(&json!(1.0e300)));
    assert!(is_lossless(&json!(-0.5)));
}

#[test]
fn every_hazard_is_reported_not_just_the_first() {
    // A payload with two bad integers should report both, or the caller fixes them one at a time.
    let value = json!({
        "a": MAX_SAFE_INTEGER + 1,
        "b": [MAX_SAFE_INTEGER + 2],
        "c": MIN_SAFE_INTEGER - 1,
    });
    let found = hazards(&value);

    assert_eq!(found.len(), 3, "{found:?}");
}

#[test]
fn hazards_are_found_in_nested_structures() {
    let value = json!({
        "outer": {
            "inner": [1, 2, {"deep": MAX_SAFE_INTEGER + 5}]
        }
    });
    assert_eq!(hazards(&value).len(), 1);
}

#[test]
fn negative_zero_survives_in_rust() {
    // JavaScript rejects `-0` because `Object.is(-0, 0)` is false while the wire sends `0`. Rust's
    // serde_json preserves the sign, so there is nothing to reject.
    let value = json!(-0.0f64);
    assert!(is_lossless(&value));
    assert_eq!(value.to_string(), "-0.0");
}

#[test]
fn normalize_makes_two_producers_agree() {
    // Whatever `serde_json` would do to a payload is applied once, here, where it can be compared.
    let value = json!({"a": 1, "b": [1.5, null]});
    assert_eq!(normalize(&value), value);
}

#[test]
fn an_unsafe_integer_survives_rust_and_dies_in_javascript() {
    // The hazard the module exists for: Rust round-trips it exactly, so nothing here notices. A
    // JavaScript client parsing the same text gets a different number.
    let value = json!({"bytes": MAX_SAFE_INTEGER + 1});
    let text = serde_json::to_string(&value).unwrap();
    assert!(text.contains("9007199254740992"), "{text}");

    let reread: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(reread, value, "Rust round-trips it exactly");
    assert!(!is_lossless(&value), "but it must not cross the boundary");
}
