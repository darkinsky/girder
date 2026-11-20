//! `schemastery`'s resolution semantics: what a value becomes, and why one is refused.
//!
//! Compiled by `#[path]` because the crate-root wiring belongs to the integrator.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../src/settings/config_schema.rs"]
pub mod config_schema_gap;

use config_schema_gap::{Kind, Schema, ValidationError, resolve};
use serde_json::{Value, json};

// ---- absence -----------------------------------------------------------------------------------

#[test]
fn an_absent_value_with_a_default_resolves_to_it() {
    let schema = Schema::number().with_default(json!(42));
    assert_eq!(resolve(&Value::Null, &schema).unwrap(), json!(42));
}

#[test]
fn an_absent_required_value_is_refused() {
    let schema = Schema::string().required();
    let error = resolve(&Value::Null, &schema).expect_err("required");
    assert_eq!(error.message, "missing required value");
}

#[test]
fn an_absent_optional_value_without_a_default_stays_absent() {
    let schema = Schema::string();
    assert_eq!(resolve(&Value::Null, &schema).unwrap(), json!(null));
}

#[test]
fn a_required_schema_with_a_default_uses_the_default_rather_than_failing() {
    // `required` decides what to do when there is *no* default. With one, absence resolves.
    let schema = Schema::string().required().with_default(json!("fallback"));
    assert_eq!(resolve(&Value::Null, &schema).unwrap(), json!("fallback"));
}

// ---- primitives --------------------------------------------------------------------------------

#[test]
fn a_string_accepts_a_string_and_refuses_everything_else() {
    let schema = Schema::string();
    assert_eq!(resolve(&json!("hi"), &schema).unwrap(), json!("hi"));
    for wrong in [json!(1), json!(true), json!([]), json!({})] {
        let error = resolve(&wrong, &schema).expect_err("not a string");
        assert_eq!(error.message, "must be a string", "{wrong}");
    }
}

#[test]
fn a_number_accepts_integers_and_floats_alike() {
    // JSON has one numeric type; refusing `1.0` where `1` is meant would be a distinction the
    // format does not make.
    let schema = Schema::number();
    assert_eq!(resolve(&json!(1), &schema).unwrap(), json!(1));
    assert_eq!(resolve(&json!(1.5), &schema).unwrap(), json!(1.5));
    assert!(resolve(&json!("1"), &schema).is_err());
}

#[test]
fn a_natural_refuses_a_negative_or_a_fraction() {
    let schema = Schema::natural();
    assert_eq!(resolve(&json!(0), &schema).unwrap(), json!(0));
    assert_eq!(resolve(&json!(7), &schema).unwrap(), json!(7));
    assert!(resolve(&json!(-1), &schema).is_err());
    assert!(resolve(&json!(1.5), &schema).is_err());
}

#[test]
fn a_percent_refuses_a_value_outside_zero_to_one() {
    let schema = Schema::of(Kind::Percent);
    assert_eq!(resolve(&json!(0.0), &schema).unwrap(), json!(0.0));
    assert_eq!(resolve(&json!(1.0), &schema).unwrap(), json!(1.0));
    assert!(resolve(&json!(1.01), &schema).is_err());
    assert!(resolve(&json!(-0.01), &schema).is_err());
}

#[test]
fn a_boolean_refuses_a_truthy_lookalike() {
    // JavaScript would coerce `1` and `"true"`; a validator that did so could not report what the
    // file actually said.
    let schema = Schema::boolean();
    assert!(resolve(&json!(true), &schema).is_ok());
    for wrong in [json!(1), json!("true"), json!(0)] {
        assert!(resolve(&wrong, &schema).is_err(), "{wrong}");
    }
}

#[test]
fn a_constant_accepts_only_itself() {
    let schema = Schema::constant(json!("dark"));
    assert_eq!(resolve(&json!("dark"), &schema).unwrap(), json!("dark"));
    let error = resolve(&json!("light"), &schema).expect_err("not the constant");
    assert!(error.message.contains("dark"), "{}", error.message);
}

// ---- bounds ------------------------------------------------------------------------------------

#[test]
fn a_string_bound_counts_characters_not_bytes() {
    // A bound that counted bytes would refuse a three-character Chinese string against a bound of 3.
    let schema = Schema::string().min(2.0).max(3.0);
    assert_eq!(resolve(&json!("ab"), &schema).unwrap(), json!("ab"));
    assert_eq!(resolve(&json!("中文啊"), &schema).unwrap(), json!("中文啊"));
    assert!(resolve(&json!("a"), &schema).is_err());
    assert!(resolve(&json!("abcd"), &schema).is_err());
}

#[test]
fn a_number_bound_is_inclusive_on_both_ends() {
    let schema = Schema::number().min(1.0).max(10.0);
    assert!(resolve(&json!(1), &schema).is_ok());
    assert!(resolve(&json!(10), &schema).is_ok());
    assert!(resolve(&json!(0.5), &schema).is_err());
    assert!(resolve(&json!(10.5), &schema).is_err());
}

#[test]
fn an_array_bound_counts_items() {
    let schema = Schema::array(Schema::number()).min(1.0).max(2.0);
    assert!(resolve(&json!([1]), &schema).is_ok());
    assert!(resolve(&json!([1, 2]), &schema).is_ok());
    assert!(resolve(&json!([]), &schema).is_err());
    assert!(resolve(&json!([1, 2, 3]), &schema).is_err());
}

// ---- nesting -----------------------------------------------------------------------------------

#[test]
fn an_array_resolves_every_member_and_names_the_failing_index() {
    let schema = Schema::array(Schema::number());
    assert_eq!(
        resolve(&json!([1, 2, 3]), &schema).unwrap(),
        json!([1, 2, 3])
    );
    let error = resolve(&json!([1, "two"]), &schema).expect_err("second member");
    assert_eq!(
        error.path, "1",
        "the path names the position, not the array"
    );
}

#[test]
fn a_dict_resolves_declared_entries_and_passes_undeclared_ones_through() {
    let schema = Schema::dict([("count", Schema::number()), ("name", Schema::string())]);
    let resolved = resolve(&json!({ "count": 2, "name": "x", "extra": true }), &schema).unwrap();
    assert_eq!(resolved["count"], json!(2));
    assert_eq!(
        resolved["extra"],
        json!(true),
        "an open object keeps what it does not declare"
    );
}

#[test]
fn a_closed_dict_refuses_an_undeclared_key_and_names_it() {
    let schema = Schema::dict([("count", Schema::number())]).closed();
    let error = resolve(&json!({ "count": 1, "surprise": 2 }), &schema).expect_err("extra key");
    assert_eq!(error.path, "surprise");
    assert!(error.message.contains("not a declared key"));
}

#[test]
fn a_nested_failure_reports_the_full_path() {
    let schema = Schema::dict([(
        "outer",
        Schema::dict([("inner", Schema::array(Schema::number()))]),
    )]);
    let error = resolve(&json!({ "outer": { "inner": [1, "x"] } }), &schema).expect_err("deep");
    assert_eq!(error.path, "outer.inner.1", "every level is named");
}

#[test]
fn an_omitted_optional_entry_is_not_written_back_as_null() {
    // Writing `null` for an absent optional makes "not configured" indistinguishable from
    // "configured to null", which a settings reader cannot resolve.
    let schema = Schema::dict([("present", Schema::number()), ("absent", Schema::string())]);
    let resolved = resolve(&json!({ "present": 1 }), &schema).unwrap();
    assert!(resolved.get("absent").is_none(), "{resolved}");
}

#[test]
fn a_dict_refuses_a_non_object() {
    let schema = Schema::dict([("count", Schema::number())]);
    assert!(resolve(&json!([1]), &schema).is_err());
    assert!(resolve(&json!("x"), &schema).is_err());
}

// ---- composition -------------------------------------------------------------------------------

#[test]
fn a_tuple_requires_exactly_the_declared_length() {
    let schema = Schema::tuple([Schema::string(), Schema::number()]);
    assert_eq!(resolve(&json!(["a", 1]), &schema).unwrap(), json!(["a", 1]));
    let error = resolve(&json!(["a"]), &schema).expect_err("too short");
    assert!(error.message.contains("2 items"));
}

#[test]
fn a_union_takes_the_first_matching_alternative() {
    let schema = Schema::union([Schema::string(), Schema::number()]);
    assert_eq!(resolve(&json!("x"), &schema).unwrap(), json!("x"));
    assert_eq!(resolve(&json!(1), &schema).unwrap(), json!(1));
    let error = resolve(&json!(true), &schema).expect_err("neither");
    assert!(error.message.contains("2 alternatives"));
}

#[test]
fn a_union_returns_the_resolved_value_so_a_default_is_not_lost() {
    // A union that matched a part but returned the *input* would drop a default the part filled in.
    let schema = Schema::union([
        Schema::number().required(),
        Schema::string().with_default(json!("filled")),
    ]);
    assert_eq!(resolve(&Value::Null, &schema).unwrap(), json!("filled"));
}

#[test]
fn an_intersect_narrows_rather_than_merges() {
    // Every part sees the same value; the last part's answer wins. An intersect that merged objects
    // would let two parts each contribute half a value, which is not what "all must match" means.
    let narrow = Schema::number().min(5.0);
    let schema = Schema::intersect([Schema::number(), narrow]);
    assert!(resolve(&json!(7), &schema).is_ok());
    assert!(
        resolve(&json!(3), &schema).is_err(),
        "the narrowing part still applies"
    );
}

#[test]
fn an_intersect_refuses_when_any_part_refuses() {
    let schema = Schema::intersect([Schema::string(), Schema::string().min(3.0)]);
    assert!(resolve(&json!("abc"), &schema).is_ok());
    assert!(resolve(&json!("ab"), &schema).is_err());
}

// ---- loose -------------------------------------------------------------------------------------

#[test]
fn a_loose_schema_substitutes_its_default_for_a_failure() {
    let schema = Schema::number().loose().with_default(json!(0));
    assert_eq!(
        resolve(&json!("not a number"), &schema).unwrap(),
        json!(0),
        "a loose section degrades instead of failing the whole document"
    );
}

#[test]
fn a_loose_schema_with_no_default_resolves_to_null_rather_than_failing() {
    let schema = Schema::number().loose();
    assert_eq!(resolve(&json!("nope"), &schema).unwrap(), json!(null));
}

#[test]
fn a_strict_schema_reports_instead_of_degrading() {
    let schema = Schema::number();
    assert!(resolve(&json!("nope"), &schema).is_err());
}

// ---- the error surface -------------------------------------------------------------------------

#[test]
fn an_error_names_its_path_and_displays_them_together() {
    let error = ValidationError::new("a.b", "must be a string");
    assert_eq!(error.to_string(), "a.b: must be a string");
    let root = ValidationError::new("", "missing required value");
    assert_eq!(root.to_string(), "missing required value");
}

// ---- the shapes upstream does not have ----------------------------------------------------------

#[test]
fn never_refuses_every_value() {
    let schema = Schema::of(Kind::Never);
    for value in [json!(null), json!(0), json!(""), json!([])] {
        assert!(resolve(&value, &schema).is_err(), "{value}");
    }
}

#[test]
fn any_accepts_everything_and_copies_it() {
    let schema = Schema::any();
    for value in [json!(null), json!(1), json!({"a": [1]})] {
        assert_eq!(resolve(&value, &schema).unwrap(), value);
    }
}

#[test]
fn a_list_requires_an_array_without_inspecting_members() {
    let schema = Schema::of(Kind::List);
    assert!(resolve(&json!([1, "x", null]), &schema).is_ok());
    assert!(resolve(&json!({}), &schema).is_err());
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod settings {
    pub use super::config_schema_gap as config_schema;
}
