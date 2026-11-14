//! Tests for tool-argument size limits (`argument_limits`).
//!
//! Two properties carry the weight here. The first is the *order*: a call that is over the total
//! limit must be reported as such, not as "`content` is large", because the second message sends the
//! model to shrink one field of a call that still does not fit. The second is the *field*: a size
//! violation must name the value to shorten, including when it is nested, or the report is a
//! restatement of "too big" with extra steps.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the checker as written even before the crate root wires it in. That is also why nothing
//! here reaches for the rest of `girder_core`: the only dependency is the module itself.

#[path = "../src/argument_limits.rs"]
mod argument_limits;

use argument_limits::{Kind, Limits, check, defaults};

/// A kibibyte, for building inputs by the unit the limits are reasoned about in.
const KIB: usize = 1024;

/// Limits small enough that a test can cross each of them without allocating megabytes.
fn small() -> Limits {
    Limits {
        max_string_bytes: 4 * KIB,
        max_total_bytes: 16 * KIB,
        max_keys: 8,
    }
}

/// A JSON string literal whose *decoded* value is exactly `bytes` bytes.
///
/// The per-string limit is checked against the decoded value, so tests that pass the expected
/// size around must not count the two quotes.
fn string_of(bytes: usize) -> String {
    format!("\"{}\"", "a".repeat(bytes))
}

/// A JSON string literal that is exactly `bytes` bytes *encoded*, quotes included.
///
/// The total limit is checked against the raw argument text, so a boundary test there has to count
/// the punctuation the encoder adds.
fn encoded_string(bytes: usize) -> String {
    format!("\"{}\"", "a".repeat(bytes.saturating_sub(2)))
}

/// A `write`-shaped call whose `content` decodes to `bytes` bytes.
fn write_call(bytes: usize) -> String {
    format!("{{\"path\":\"a.rs\",\"content\":{}}}", string_of(bytes))
}

#[test]
fn a_call_within_every_limit_passes() {
    assert_eq!(
        check(&small(), "{\"path\":\"a.rs\",\"content\":\"fn main() {}\"}"),
        None
    );
}

#[test]
fn the_total_limit_is_reported_before_the_field_that_exceeded_it() {
    // The load-bearing test. Both limits are violated: the string is over `max_string_bytes` and the
    // call is over `max_total_bytes`. Reporting the string would name a field, and shrinking that
    // field alone is not enough to make the call fit, so the total must win.
    let limits = small();
    let arguments = write_call(limits.max_total_bytes + 1);
    assert!(arguments.len() > limits.max_string_bytes);

    let found = check(&limits, &arguments).expect("the call is over the total limit");
    assert_eq!(found.kind, Kind::TotalTooLarge);
    assert_eq!(found.actual, arguments.len());
    assert_eq!(found.limit, limits.max_total_bytes);
    // The finding is about the call, not about a field, so there is no field to name.
    assert!(found.field.is_empty(), "got field: {:?}", found.field);
}

#[test]
fn the_total_limit_can_be_crossed_while_every_string_is_small() {
    // The other direction of the same ordering: many admissible strings add up. This is the shape a
    // per-field check cannot see, and it must still be reported as the total.
    let limits = small();
    let one = string_of(limits.max_string_bytes);
    let arguments = format!("{{\"a\":{one},\"b\":{one},\"c\":{one},\"d\":{one},\"e\":{one}}}");
    // Five strings exactly at the per-string cap: admissible one by one, over the total together.
    assert_eq!(one.len(), limits.max_string_bytes + 2);

    let found = check(&limits, &arguments).expect("the call is over the total limit");
    assert_eq!(found.kind, Kind::TotalTooLarge);
}

#[test]
fn an_oversized_string_names_the_field_it_is_in() {
    let limits = small();
    let content = limits.max_string_bytes + 1;
    let arguments = write_call(content);

    let found = check(&limits, &arguments).expect("`content` is over the string limit");
    assert_eq!(found.kind, Kind::StringTooLarge);
    assert_eq!(found.field, "content");
    assert_eq!(found.actual, content);
    assert_eq!(found.limit, limits.max_string_bytes);
    // The message has to carry the field, or the reader learns nothing it did not already know.
    let message = found.explain();
    assert!(message.contains("content"), "got: {message}");
}

#[test]
fn the_largest_oversized_string_is_the_one_reported() {
    // Two fields are over the limit. The larger one has to shrink the most, so naming it is the
    // repair that actually makes the call fit; naming the first in document order would not be.
    let limits = small();
    let arguments = format!(
        "{{\"first\":{},\"second\":{}}}",
        string_of(limits.max_string_bytes + 10),
        string_of(limits.max_string_bytes + 900)
    );

    let found = check(&limits, &arguments).expect("both strings are over the limit");
    assert_eq!(found.field, "second");
    assert_eq!(found.actual, limits.max_string_bytes + 900);
}

#[test]
fn an_oversized_string_nested_in_an_object_reports_its_path() {
    let limits = small();
    let arguments = format!(
        "{{\"input\":{{\"body\":{}}}}}",
        string_of(limits.max_string_bytes + 1)
    );

    let found = check(&limits, &arguments).expect("a nested string is over the limit");
    assert_eq!(found.kind, Kind::StringTooLarge);
    assert_eq!(found.field, "input.body");
}

#[test]
fn an_oversized_string_nested_in_an_array_reports_its_index() {
    let limits = small();
    let arguments = format!(
        "{{\"files\":[\"ok\",{}]}}",
        string_of(limits.max_string_bytes + 1)
    );

    let found = check(&limits, &arguments).expect("an element is over the limit");
    assert_eq!(found.field, "files[1]");
}

#[test]
fn too_many_top_level_keys_is_reported_as_a_key_count() {
    // Ten thousand tiny keys: each is far below the per-string limit, so only a count can see this.
    let limits = small();
    let pairs: Vec<String> = (0..=limits.max_keys)
        .map(|i| format!("\"k{i}\":1"))
        .collect();
    let arguments = format!("{{{}}}", pairs.join(","));

    let found = check(&limits, &arguments).expect("the object has more keys than allowed");
    assert_eq!(found.kind, Kind::TooManyKeys);
    assert_eq!(found.actual, limits.max_keys + 1);
    assert_eq!(found.limit, limits.max_keys);
    assert!(found.field.is_empty(), "got field: {:?}", found.field);
}

#[test]
fn nested_keys_do_not_count_towards_the_top_level_limit() {
    // Only the top level is counted, by design: counting deeper would be the same walk the string
    // check already performs, and a bulk container in one field is caught by the total instead.
    let limits = small();
    let inner: Vec<String> = (0..32).map(|i| format!("\"k{i}\":1")).collect();
    let arguments = format!("{{\"inner\":{{{}}}}}", inner.join(","));

    assert_eq!(check(&limits, &arguments), None);
}

#[test]
fn a_duplicate_key_counts_once() {
    // `serde_json` keeps the last value for a repeated key, so the map is what the check sees. A
    // textual count of `":` would disagree with the object the tool actually receives.
    let limits = small();
    let pairs: Vec<String> = (0..=limits.max_keys)
        .map(|_| "\"k\":1".to_owned())
        .collect();
    let arguments = format!("{{{}}}", pairs.join(","));

    assert_eq!(check(&limits, &arguments), None);
}

#[test]
fn input_that_is_not_json_is_left_to_the_parser() {
    // There are no fields to measure in a malformed string, and the parse error is a better report
    // than a size finding. The caller that parses owns that message.
    assert_eq!(check(&small(), "{\"content\": "), None);
    assert_eq!(check(&small(), "not json at all"), None);
    assert_eq!(check(&small(), "{]"), None);
}

#[test]
fn empty_arguments_pass() {
    let limits = small();
    assert_eq!(check(&limits, ""), None);
    assert_eq!(check(&limits, "   \n\t "), None);
    assert_eq!(check(&limits, "{}"), None);
}

#[test]
fn a_value_exactly_at_each_limit_is_accepted() {
    // Each limit names the largest permitted size, so the boundary value must be legal. This is the
    // one test that would catch an off-by-one in any of the three comparisons.
    let limits = small();

    let at_string = write_call(limits.max_string_bytes);
    assert!(
        at_string.len() < limits.max_total_bytes,
        "fixture is consistent"
    );
    assert_eq!(check(&limits, &at_string), None);

    // The total boundary needs a configuration where the per-string cap is not the binding one, or
    // a value exactly at the total would also be a string over its own limit.
    let wide = Limits {
        max_string_bytes: limits.max_total_bytes,
        ..limits
    };
    let at_total = encoded_string(wide.max_total_bytes);
    assert_eq!(at_total.len(), wide.max_total_bytes);
    assert_eq!(check(&wide, &at_total), None);

    let keys: Vec<String> = (0..limits.max_keys)
        .map(|i| format!("\"k{i}\":1"))
        .collect();
    let at_keys = format!("{{{}}}", keys.join(","));
    assert_eq!(check(&limits, &at_keys), None);
}

#[test]
fn one_byte_over_each_limit_is_rejected() {
    // The complement of the boundary test: without this, a checker that accepted everything would
    // pass every "exactly at the limit" case.
    let limits = small();

    let over_string = write_call(limits.max_string_bytes + 1);
    assert!(
        over_string.len() < limits.max_total_bytes,
        "fixture is consistent"
    );
    assert_eq!(
        check(&limits, &over_string).map(|found| found.kind),
        Some(Kind::StringTooLarge)
    );

    let wide = Limits {
        max_string_bytes: limits.max_total_bytes,
        ..limits
    };
    let over_total = encoded_string(wide.max_total_bytes + 1);
    assert_eq!(
        check(&wide, &over_total).map(|found| found.kind),
        Some(Kind::TotalTooLarge)
    );

    let keys: Vec<String> = (0..=limits.max_keys)
        .map(|i| format!("\"k{i}\":1"))
        .collect();
    let over_keys = format!("{{{}}}", keys.join(","));
    assert_eq!(
        check(&limits, &over_keys).map(|found| found.kind),
        Some(Kind::TooManyKeys)
    );
}

#[test]
fn a_total_limit_below_the_string_limit_makes_the_total_binding() {
    // The limits are independent, so a configuration where the total is the smaller number is legal.
    // The order is what decides which is reported, and it must not depend on their relative sizes.
    let limits = Limits {
        max_string_bytes: 8 * KIB,
        max_total_bytes: 2 * KIB,
        max_keys: 8,
    };
    let found = check(&limits, &write_call(4 * KIB)).expect("the call is over the total limit");
    assert_eq!(found.kind, Kind::TotalTooLarge);
}

#[test]
fn a_ten_megabyte_content_field_is_refused_by_the_defaults() {
    // The case the module exists for: a model writing a 10 MB file in one call. Under the shipped
    // defaults it is over *both* limits, so the total is what gets reported — the call as a whole
    // does not fit, and telling the model to shorten `content` alone would still leave it unfixable.
    let arguments = write_call(10 * 1024 * 1024);
    let found = check(&defaults(), &arguments).expect("10 MB is over the default limits");
    assert_eq!(found.kind, Kind::TotalTooLarge);
    assert_eq!(found.actual, arguments.len());
}

#[test]
fn a_string_between_the_two_default_limits_is_refused_by_name() {
    // The per-field rule on its own: 2 MiB fits inside the 4 MiB total but not inside the 1 MiB
    // per-string cap, so this is the reachable case where `content` is the right thing to name.
    let arguments = write_call(2 * 1024 * 1024);
    let found = check(&defaults(), &arguments).expect("2 MiB is over the string limit");
    assert_eq!(found.kind, Kind::StringTooLarge);
    assert_eq!(found.field, "content");
    assert!(
        found.explain().contains("content"),
        "got: {}",
        found.explain()
    );
}

#[test]
fn the_defaults_are_ordered_the_way_the_checks_run() {
    // A per-string limit above the total would make every string finding unreachable in a call whose
    // total had to be raised to hold it; the defaults state the intended nesting explicitly.
    let limits = defaults();
    assert!(limits.max_string_bytes < limits.max_total_bytes);
    assert!(limits.max_keys > 0);
    assert!(limits.max_string_bytes > 0);
}

#[test]
fn non_string_values_are_not_measured_against_the_string_limit() {
    // Numbers and booleans have no length in the argument's text beyond their digits, and a huge
    // number is a total-size concern rather than a string one.
    let limits = small();
    assert_eq!(
        check(
            &limits,
            "{\"n\":123456789012345678901234567890,\"b\":true,\"z\":null}"
        ),
        None
    );
}
