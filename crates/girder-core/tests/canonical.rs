//! Tests for the canonical encodings.
//!
//! The tests are weighted toward the two failures that make an identity string worse than useless,
//! because both are silent: an encoding that depends on JSON member order, and an encoding whose
//! field boundaries can be forged by value content. In both cases two *different* inputs collapse
//! into one key, and the caller is handed another request's answer with the shape of a real one.
//!
//! Everything else here pins a distinction that must *not* be normalized away — array order, `1` vs
//! `1.0`, `null` vs an absent member — because the opposite mistake, merging two inputs the caller
//! meant to tell apart, is the same failure approached from the other side.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written even before the crate root wires it in. `canonical` depends on
//! nothing but `serde_json` and the standard library, so it also builds outside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/canonical.rs"]
mod canonical;

use canonical::{Encoder, canonical_json, key_of};
use serde_json::{Value, json};

// ---- JSON: member order ------------------------------------------------------------------------

#[test]
fn object_member_order_is_irrelevant() {
    // The same object written two ways. JSON says members are unordered, so an encoding that
    // distinguishes these two would miss every cache hit whose arguments were serialized by a
    // different tool.
    let left = json!({"alpha": 1, "beta": 2, "gamma": 3});
    let right = json!({"gamma": 3, "alpha": 1, "beta": 2});
    assert_eq!(canonical_json(&left), canonical_json(&right));
}

#[test]
fn nested_object_member_order_is_irrelevant() {
    // The core case: sorting only the top level is the shortcut that looks right and is not. The
    // distinguishing object is two levels down, inside an array, which is where a hand-written
    // encoder stops recursing.
    let left = json!({
        "tools": [
            {"name": "read", "parameters": {"type": "object", "required": ["path"], "path": {"type": "string"}}}
        ],
        "model": "m"
    });
    let right = json!({
        "model": "m",
        "tools": [
            {"parameters": {"path": {"type": "string"}, "required": ["path"], "type": "object"}, "name": "read"}
        ]
    });
    assert_eq!(canonical_json(&left), canonical_json(&right));
}

#[test]
fn a_difference_deep_inside_a_value_is_still_visible() {
    // The other half of recursion: canonicalizing must not flatten distinct trees into one string.
    let left = json!({"a": {"b": {"c": [{"d": 1}]}}});
    let right = json!({"a": {"b": {"c": [{"d": 2}]}}});
    assert_ne!(canonical_json(&left), canonical_json(&right));
}

// ---- JSON: array order -------------------------------------------------------------------------

#[test]
fn array_order_matters() {
    // An array is ordered by definition: `[1, 2]` and `[2, 1]` are different conversations.
    assert_ne!(
        canonical_json(&json!([1, 2])),
        canonical_json(&json!([2, 1]))
    );
    assert_ne!(
        canonical_json(&json!({"tools": ["a", "b"]})),
        canonical_json(&json!({"tools": ["b", "a"]}))
    );
}

#[test]
fn repeated_elements_matter() {
    assert_ne!(canonical_json(&json!([1, 1])), canonical_json(&json!([1])));
    assert_ne!(canonical_json(&json!([])), canonical_json(&json!([null])));
}

// ---- JSON: field boundaries --------------------------------------------------------------------

#[test]
fn the_json_encoding_is_not_delimited() {
    // Pins the shape rather than the exact bytes: a change to a separator-based encoding would
    // still pass every equality test above while making the collisions below possible.
    assert_eq!(canonical_json(&json!({"a": 1})), "o1:k1:an1:1");
    assert_eq!(canonical_json(&json!([])), "a0:");
    assert_eq!(canonical_json(&json!({})), "o0:");
}

#[test]
fn a_string_value_cannot_impersonate_a_boundary() {
    // With a colon separator, `{"a": "1:b"}` and `{"a": "1", "b": ...}` could be made to encode
    // alike. Length prefixes make the boundary positional instead of textual.
    let tricky = json!({"a": "1:b"});
    let plain = json!({"a": "1", "b": ""});
    assert_ne!(canonical_json(&tricky), canonical_json(&plain));
}

#[test]
fn a_key_cannot_impersonate_a_value() {
    // `{"a": "bc"}` and `{"ab": "c"}` differ only in where the key ends.
    assert_ne!(
        canonical_json(&json!({"a": "bc"})),
        canonical_json(&json!({"ab": "c"}))
    );
}

// ---- JSON: scalars -----------------------------------------------------------------------------

#[test]
fn numbers_and_strings_are_distinct() {
    // A model may send `{"offset": 0}` or `{"offset": "0"}`; these are different arguments, and a
    // tool that would fail on one must not be answered with the other's result.
    assert_ne!(canonical_json(&json!(0)), canonical_json(&json!("0")));
    assert_ne!(canonical_json(&json!(true)), canonical_json(&json!("true")));
    assert_ne!(canonical_json(&json!(1)), canonical_json(&json!(true)));
}

#[test]
fn a_number_is_never_normalized_into_another_number() {
    // Deliberately *not* normalized: canonicalization here is about member order only. Merging
    // `1` and `1.0` would be a semantic claim this module is not entitled to make, and it would
    // merge two spellings the caller may have meant to distinguish.
    assert_ne!(canonical_json(&json!(1)), canonical_json(&json!(1.0)));
    assert_ne!(canonical_json(&json!(-1)), canonical_json(&json!(1)));
    assert_ne!(canonical_json(&json!(0)), canonical_json(&json!(-0.0)));
    // Integers beyond 2^53 keep full precision, because the encoding never goes through `f64`.
    assert_ne!(
        canonical_json(&json!(9_007_199_254_740_993_u64)),
        canonical_json(&json!(9_007_199_254_740_992_u64))
    );
}

#[test]
fn unicode_is_preserved_byte_for_byte() {
    // Lengths are counted in bytes and the text is copied verbatim: escaping it would make the
    // encoding locale- and writer-dependent, and truncating at a character count would cut a
    // multi-byte code point.
    let text = canonical_json(&json!({"greeting": "你好，世界 🚀"}));
    assert!(text.contains("你好，世界 🚀"));
    assert_ne!(
        canonical_json(&json!({"greeting": "你好"})),
        canonical_json(&json!({"greeting": "你"})),
        "a shorter prefix of a multi-byte string is a different value"
    );
    // Two different spellings of one grapheme are two different strings; normalizing them is a
    // deliberate non-goal.
    assert_ne!(
        canonical_json(&json!("\u{e9}")),
        canonical_json(&json!("\u{65}\u{301}"))
    );
}

#[test]
fn null_is_not_the_same_as_a_missing_member() {
    // `{"a": null}` and `{}` are different objects: the first says the field is null, the second
    // says nothing about it. An encoder that skips nulls would merge them.
    assert_ne!(
        canonical_json(&json!({"a": null})),
        canonical_json(&json!({}))
    );
    assert_ne!(
        canonical_json(&json!({"a": null})),
        canonical_json(&json!({"a": ""}))
    );
    assert_ne!(
        canonical_json(&json!({"a": null})),
        canonical_json(&json!({"a": 0}))
    );
}

#[test]
fn the_empty_containers_are_all_distinct() {
    let values = [
        json!(null),
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
    ];
    for (index, left) in values.iter().enumerate() {
        for (other, right) in values.iter().enumerate() {
            if index == other {
                assert_eq!(canonical_json(left), canonical_json(right));
            } else {
                assert_ne!(
                    canonical_json(left),
                    canonical_json(right),
                    "{left} and {right} must not encode alike"
                );
            }
        }
    }
}

#[test]
fn canonical_encoding_is_deterministic() {
    // Repeated calls on one value must agree, or a cache would never hit even on itself.
    let value: Value = json!({"b": [1, {"d": null, "c": "x"}], "a": {"z": true, "y": 1.5}});
    let first = canonical_json(&value);
    let second = canonical_json(&value);
    assert_eq!(first, second);
}

// ---- Encoder -----------------------------------------------------------------------------------

#[test]
fn length_prefixes_prevent_flat_field_collisions() {
    // `["ab", "c"]` and `["a", "bc"]` are the canonical collision: a delimiter-based encoder writes
    // both as `ab:c`. The lengths make them different by construction.
    let mut left = Encoder::new();
    left.field("x", "ab").field("y", "c");
    let mut right = Encoder::new();
    right.field("x", "a").field("y", "bc");
    assert_ne!(left.finish(), right.finish());

    // The same collision across the field/value boundary.
    let mut left = Encoder::new();
    left.field("a", "b");
    let mut right = Encoder::new();
    right.field("ab", "");
    assert_ne!(left.finish(), right.finish());
}

#[test]
fn a_field_name_is_part_of_the_encoding() {
    // Two records that differ only in which field holds a value are different records.
    let mut left = Encoder::new();
    left.field("model", "x").field("system", "y");
    let mut right = Encoder::new();
    right.field("model", "y").field("system", "x");
    assert_ne!(left.finish(), right.finish());
}

#[test]
fn a_number_is_not_the_same_field_as_its_text() {
    // A count and a string that spells the count reach the provider differently (one is a limit,
    // the other is content), so they must not share a key.
    let mut numeric = Encoder::new();
    numeric.number("offset", 0);
    let mut textual = Encoder::new();
    textual.field("offset", "0");
    assert_ne!(numeric.finish(), textual.finish());
}

#[test]
fn numbers_are_delimited_from_the_next_field() {
    // Without a terminator, `number("a", 1)` followed by `field("b", "2")` could be read as the
    // single number `12`, or as `1` and `:2`. The `;` makes the end of the digits explicit.
    let mut two_fields = Encoder::new();
    two_fields.number("a", 1).field("b", "2");
    let mut one_field = Encoder::new();
    one_field.number("a", 12);
    assert_ne!(two_fields.finish(), one_field.finish());
}

#[test]
fn an_empty_encoder_is_empty() {
    assert_eq!(Encoder::new().finish(), "");
    assert_eq!(Encoder::default().finish(), "");
}

#[test]
fn the_documented_encoder_encoding_holds() {
    let mut encoder = Encoder::new();
    encoder.field("tool", "read").number("offset", 0);
    assert_eq!(encoder.finish(), "s4:tool4:readn6:offset0;");
}

#[test]
fn a_field_value_may_contain_anything() {
    // Separators, newlines, and non-ASCII text all survive: only the length byte count matters.
    let mut encoder = Encoder::new();
    encoder.field("text", "a:b;c\n你好");
    let rendered = encoder.finish();
    assert!(rendered.contains("a:b;c\n你好"));

    let mut other = Encoder::new();
    other.field("text", "a:b;c\n你");
    assert_ne!(rendered, other.finish());
}

// ---- key_of ------------------------------------------------------------------------------------

#[test]
fn key_of_distinguishes_split_points() {
    // The core property for the list form, and the reason it is not `parts.join(":")`.
    assert_ne!(key_of(&["ab", "c"]), key_of(&["a", "bc"]));
    assert_eq!(key_of(&["ab", "c"]), "2:2:ab1:c");
    assert_eq!(key_of(&["a", "bc"]), "2:1:a2:bc");
    assert_eq!(key_of(&["a"]), "1:1:a");
}

#[test]
fn key_of_makes_the_arity_part_of_the_key() {
    // Lengths alone cannot separate these: the shorter list is a prefix of the longer one.
    assert_ne!(key_of(&["a"]), key_of(&["a", ""]));
    assert_ne!(key_of(&[]), key_of(&[""]));
    assert_eq!(key_of(&[]), "0:");
    assert_eq!(key_of(&["a", ""]), "2:1:a0:");
}

#[test]
fn key_of_is_order_sensitive_and_deterministic() {
    assert_ne!(key_of(&["a", "b"]), key_of(&["b", "a"]));
    assert_eq!(key_of(&["a", "b"]), key_of(&["a", "b"]));
}

#[test]
fn key_of_handles_unicode_by_byte_length() {
    // `你` is three bytes; a character count would record `1` and let the key be read one code
    // point too far.
    assert_eq!(key_of(&["你好"]), "1:6:你好");
    assert_ne!(key_of(&["你"]), key_of(&["你好"]));
}
