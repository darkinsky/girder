//! Tests for the streaming-argument scanner.
//!
//! Upstream `packages/util/values/src/partial-json.ts` splits into two things worth testing
//! separately: what a *finished* view answers, and what a *streaming* view answers while text is
//! still arriving. The interesting failures are all in the second: an escape split across two
//! fragments, a value whose closing bracket has not arrived, a refresh that misses a change because
//! the answer was rounded, a limit that would split a surrogate pair.
//!
//! # Why the module is compiled by path
//!
//! `util_gap` is not exported from the crate root yet; the wiring is a separate change to `lib.rs`.
//! Compiling the exact source file here keeps these tests runnable and independent of that wiring,
//! while still exercising the same code a consumer will reach through `girder_core::util_gap`.
//! Nothing is duplicated: this is the real file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/util_gap.rs"]
mod util_gap;

use serde_json::json;

use util_gap::{PartialArguments, deep_equal_json, is_json_value, utf16_len};

// ---- the value predicates ---------------------------------------------------------------------

#[test]
fn every_json_value_survives_transport_once_it_exists() {
    // Upstream rejects `NaN`, `-0`, cycles, sparse arrays, and foreign prototypes. None of them is
    // constructible here, so the predicate is total. The test pins that it stays total rather than
    // acquiring a false negative for some nested shape.
    let values = [
        json!(null),
        json!(true),
        json!(0),
        json!(-1),
        json!("text"),
        json!([]),
        json!({}),
        json!({"a": [1, {"b": null}], "c": "d"}),
    ];
    for value in values {
        assert!(is_json_value(&value), "{value} should be a JSON value");
    }
}

#[test]
fn structural_equality_ignores_identity_and_respects_nesting() {
    let left = json!({"a": [1, 2], "b": {"c": true}});
    let right = json!({"a": [1, 2], "b": {"c": true}});
    assert!(deep_equal_json(&left, &right));

    // Key order is not identity in JSON, and `serde_json::Value` compares maps structurally.
    assert!(deep_equal_json(
        &json!({"a": 1, "b": 2}),
        &json!({"b": 2, "a": 1})
    ));
    assert!(!deep_equal_json(
        &left,
        &json!({"a": [1, 3], "b": {"c": true}})
    ));
    assert!(!deep_equal_json(&json!([1, 2]), &json!([2, 1])));
    // A type change is a difference even when the rendering looks close.
    assert!(!deep_equal_json(&json!(1), &json!("1")));
    assert!(!deep_equal_json(&json!(null), &json!(false)));
}

#[test]
fn length_is_counted_in_utf16_units_like_the_client_counts() {
    assert_eq!(utf16_len(""), 0);
    assert_eq!(utf16_len("abc"), 3);
    // Outside the BMP: one character, two UTF-16 code units.
    assert_eq!(utf16_len("😀"), 2);
    assert_eq!(utf16_len("a😀b"), 4);
    // A combining sequence is two code points and two units; length is not grapheme count.
    assert_eq!(utf16_len("e\u{301}"), 2);
}

// ---- a finished view --------------------------------------------------------------------------

#[test]
fn a_finished_text_answers_every_reader() {
    let mut view = PartialArguments::from_text(
        r#"{"path": "/tmp/a.rs", "limit": 40, "recursive": true, "none": null, "list": [1, 2]}"#,
    );

    assert!(view.is_sealed());
    assert!(!view.invalid());
    assert_eq!(
        view.keys(),
        vec!["path", "limit", "recursive", "none", "list"]
    );
    assert_eq!(view.text("path").as_deref(), Some("/tmp/a.rs"));
    assert_eq!(view.value("limit"), Some(json!(40)));
    assert_eq!(view.value("recursive"), Some(json!(true)));
    // `null` is a present field with a value, not an absent one.
    assert!(view.has("none"));
    assert_eq!(view.value("none"), Some(json!(null)));
    assert_eq!(view.value("list"), Some(json!([1, 2])));
    // A string has a text reader, not a value reader.
    assert_eq!(view.value("path"), None);
    assert_eq!(view.text("limit"), None);
}

#[test]
fn a_field_that_is_absent_answers_nothing_rather_than_failing() {
    let mut view = PartialArguments::from_text(r#"{"a": 1}"#);

    assert!(!view.has("missing"));
    assert_eq!(view.text("missing"), None);
    assert_eq!(view.value("missing"), None);
    assert_eq!(view.string_length("missing", 1, 0), None);
    assert!(!view.string_exceeds("missing", 0));
    assert!(!view.complete("missing"));
    assert_eq!(view.text_prefix("missing", 5), None);
}

#[test]
fn an_empty_call_is_closed_and_reads_nothing() {
    let mut empty = PartialArguments::empty();
    assert!(empty.is_empty());
    assert!(empty.closed());
    assert!(empty.is_sealed());
    assert_eq!(empty.keys(), Vec::<String>::new());
    assert!(!empty.invalid());

    let mut view = PartialArguments::new();
    assert!(view.is_empty());
    // A streaming view over no text has not closed the object, because it may still arrive.
    assert!(!view.closed());
}

#[test]
fn a_non_object_payload_has_no_fields() {
    // Upstream's `fromObject` treats an array or scalar as having no readable fields, and so does
    // this: a caller indexing an array by name was asking the wrong question either way.
    for payload in [json!([1, 2, 3]), json!("text"), json!(7), json!(null)] {
        let mut view = PartialArguments::from_value(&payload);
        assert!(view.is_empty());
        assert!(!view.has("0"));
        assert_eq!(view.text("0"), None);
    }
}

#[test]
fn a_parsed_value_view_answers_the_same_questions_as_a_text_view() {
    let payload = json!({"path": "a.rs", "count": 3});
    let mut from_value = PartialArguments::from_value(&payload);
    let mut from_text = PartialArguments::from_text(r#"{"path": "a.rs", "count": 3}"#);

    // A parsed payload's key order is the JSON map's, which `serde_json` keeps sorted; the text
    // view keeps first-appearance order. Everything else is identical, which is the point.
    let mut parsed_keys = from_value.keys();
    let mut text_keys = from_text.keys();
    parsed_keys.sort();
    text_keys.sort();
    assert_eq!(parsed_keys, text_keys);
    assert_eq!(from_value.text("path"), from_text.text("path"));
    assert_eq!(from_value.value("count"), from_text.value("count"));
    assert_eq!(
        from_value.string_length("path", 1, 0),
        from_text.string_length("path", 1, 0)
    );
}

// ---- streaming --------------------------------------------------------------------------------

#[test]
fn a_field_appears_when_its_value_begins_not_when_its_key_does() {
    // Upstream documents `has` as "a string opened or another value began", and the scanner opens
    // an entry in `beginValue`. A key with no `:` after it, and a key with a `:` but no value, are
    // therefore both absent.
    let mut view = PartialArguments::new();
    view.append(r#"{"path""#).unwrap();
    assert!(!view.has("path"));
    assert!(view.keys().is_empty());

    view.append(":").unwrap();
    assert!(!view.has("path"));
    assert!(view.keys().is_empty());

    view.append(r#" "a"#).unwrap();
    assert!(view.has("path"));
    assert_eq!(view.keys(), vec!["path"]);
    assert_eq!(view.text("path").as_deref(), Some("a"));
    assert!(!view.complete("path"));
}

#[test]
fn a_streamed_string_answers_a_growing_prefix_and_is_not_complete_until_it_closes() {
    let mut view = PartialArguments::new();
    view.append(r#"{"path": "src/ma"#).unwrap();

    assert_eq!(view.text("path").as_deref(), Some("src/ma"));
    assert_eq!(view.string_length("path", 1, 0), Some(6));
    assert!(!view.complete("path"));
    assert!(!view.closed());

    view.append(r#"in.rs""#).unwrap();
    assert_eq!(view.text("path").as_deref(), Some("src/main.rs"));
    assert!(view.complete("path"));
    // The closes have not arrived yet, so the object is still open.
    assert!(!view.closed());

    view.append("}").unwrap();
    assert!(view.closed());
}

#[test]
fn a_non_ascii_string_is_counted_in_utf16_units_rather_than_bytes() {
    // Upstream has to carry state across a fragment boundary because a JavaScript string can be
    // split between the two halves of a surrogate pair. A Rust `&str` cannot: it is UTF-8, so a
    // character never straddles two fragments. What survives is the *counting* difference, which
    // this pins: `a` + emoji is 3 UTF-16 units but 5 bytes.
    let mut view = PartialArguments::new();
    view.append(r#"{"s": "a"#).unwrap();
    view.append("😀").unwrap();
    view.append(r#""}"#).unwrap();

    assert_eq!(view.text("s").as_deref(), Some("a😀"));
    assert_eq!(view.string_length("s", 1, 0), Some(3));
    assert!(!view.invalid());
}

#[test]
fn an_escape_sequence_split_across_fragments_is_held_back_rather_than_rejected() {
    let mut view = PartialArguments::new();
    // A literal backslash, then the `n` that completes the escape.
    view.append(r#"{"s": "line1\"#).unwrap();
    let partial = view.text("s").unwrap_or_default();
    assert_eq!(partial, "line1");
    assert!(!view.invalid());

    view.append(r#"nline2"}"#).unwrap();
    assert_eq!(view.text("s").as_deref(), Some("line1\nline2"));
    assert!(!view.invalid());
    assert!(view.complete("s"));
}

#[test]
fn the_simple_escapes_decode_to_the_characters_json_names() {
    let mut view = PartialArguments::from_text(r#"{"s": "a\"b\\c\/d\be\ff\ng\rh\ti"}"#);
    assert_eq!(
        view.text("s").as_deref(),
        Some("a\"b\\c/d\u{8}e\u{c}f\ng\rh\ti")
    );
}

#[test]
fn a_unicode_escape_decodes_and_counts_as_one_character() {
    let mut view = PartialArguments::from_text(r#"{"s": "\u00e9\u0041"}"#);
    assert_eq!(view.text("s").as_deref(), Some("éA"));
    assert_eq!(view.string_length("s", 1, 0), Some(2));
    assert!(!view.invalid());
}

#[test]
fn an_undecodable_escape_keeps_the_valid_prefix_and_marks_the_field_incomplete() {
    // Upstream retains the prefix rather than discarding it, so a display showing a Windows path
    // shows the part that decoded.
    let mut view = PartialArguments::from_text(r#"{"s": "ok\qbad"}"#);
    assert_eq!(view.text("s").as_deref(), Some("ok"));
    assert!(view.invalid());
    // A content read found the error, so the field is no longer complete even though its quote
    // arrived.
    assert!(!view.complete("s"));
}

#[test]
fn a_non_string_value_is_unavailable_until_its_bracket_closes() {
    let mut view = PartialArguments::new();
    view.append(r#"{"args": {"a": 1"#).unwrap();

    assert!(view.has("args"));
    assert!(!view.complete("args"));
    // The value is still open, so a reader gets nothing rather than a half-parsed object.
    assert_eq!(view.value("args"), None);

    view.append("}}").unwrap();
    assert!(view.complete("args"));
    assert_eq!(view.value("args"), Some(json!({"a": 1})));
}

#[test]
fn a_scalar_value_completes_at_its_delimiter() {
    let mut view = PartialArguments::new();
    view.append(r#"{"n": 123"#).unwrap();
    assert_eq!(view.value("n"), None);

    view.append(",").unwrap();
    assert_eq!(view.value("n"), Some(json!(123)));
    assert!(view.complete("n"));
}

#[test]
fn a_nested_array_with_strings_braces_and_brackets_parses_as_one_value() {
    let mut view = PartialArguments::from_text(r#"{"v": [1, "}]{", {"k": [true, null]}]}"#);
    assert!(view.complete("v"));
    assert!(!view.invalid());
    assert_eq!(
        view.value("v"),
        Some(json!([1, "}]{", {"k": [true, null]}]))
    );
}

#[test]
fn a_mismatched_nesting_bracket_makes_the_view_invalid() {
    let mut view = PartialArguments::from_text(r#"{"v": [1, 2}"#);
    assert!(view.invalid());
    // Nothing after the error is located, so the field never becomes complete.
    assert!(!view.complete("v"));
}

#[test]
fn a_bad_key_escape_makes_the_view_invalid_rather_than_renaming_the_field() {
    let mut view = PartialArguments::from_text(r#"{"a\q": 1}"#);
    assert!(view.invalid());
    assert!(!view.has("a\\q"));
}

#[test]
fn text_that_is_not_an_object_is_invalid_rather_than_empty() {
    for text in ["[1, 2]", "42", "not json", "  \"s\" "] {
        let mut view = PartialArguments::from_text(text);
        assert!(view.invalid(), "{text:?} should be invalid");
        assert!(view.closed());
        assert!(view.keys().is_empty());
    }

    // Empty text is a different case, and upstream distinguishes it: the scanner never saw a
    // character it could reject, so the view is not invalid — it is simply a sealed view with no
    // fields. Reporting it as invalid would make an empty argument look like a malformed one.
    let mut blank = PartialArguments::from_text("");
    assert!(!blank.invalid());
    assert!(blank.closed());
    assert!(blank.keys().is_empty());
}

#[test]
fn a_trailing_character_after_the_object_is_invalid() {
    let mut view = PartialArguments::from_text(r#"{"a": 1} trailing"#);
    assert!(view.invalid());
    // The fields located before the error are still readable: rejecting the tail must not throw
    // away the answer the display already has.
    assert_eq!(view.value("a"), Some(json!(1)));
}

// ---- limits -----------------------------------------------------------------------------------

#[test]
fn a_length_limit_stops_reading_one_character_past_it() {
    let mut view = PartialArguments::from_text(r#"{"s": "abcdefghij"}"#);

    assert!(view.string_exceeds("s", 5));
    assert!(!view.string_exceeds("s", 10));
    // "At least the limit" is not "exceeds the limit": an exactly-full string is within it.
    assert!(!view.string_exceeds("s", 11));
    // A non-string field exceeds nothing.
    let mut numbers = PartialArguments::from_text(r#"{"n": 5}"#);
    assert!(!numbers.string_exceeds("n", 0));
}

#[test]
fn a_length_limit_counts_utf16_units_so_a_surrogate_pair_counts_twice() {
    let mut view = PartialArguments::from_text(r#"{"s": "😀"}"#);
    // One character, two units: a limit of one is exceeded.
    assert!(view.string_exceeds("s", 1));
    assert!(!view.string_exceeds("s", 2));
}

#[test]
fn a_stepped_length_reports_the_rounded_up_value() {
    let mut view = PartialArguments::from_text(r#"{"s": "1234567890"}"#);

    assert_eq!(view.string_length("s", 1, 0), Some(10));
    // Ten characters in steps of four is three steps.
    assert_eq!(view.string_length("s", 4, 0), Some(3));
    // An offset includes already-counted characters in the total before rounding.
    assert_eq!(view.string_length("s", 4, 2), Some(3));
    assert_eq!(view.string_length("s", 4, 8), Some(5));
    // A step of zero is raised to one rather than dividing by zero.
    assert_eq!(view.string_length("s", 0, 0), Some(10));
}

#[test]
fn a_prefix_is_bounded_and_a_zero_limit_becomes_one() {
    let mut view = PartialArguments::from_text(r#"{"s": "abcdef"}"#);

    assert_eq!(view.text_prefix("s", 3).as_deref(), Some("abc"));
    assert_eq!(view.text_prefix("s", 100).as_deref(), Some("abcdef"));
    assert_eq!(view.text_prefix("s", 0).as_deref(), Some("a"));
}

#[test]
fn a_prefix_limit_never_splits_a_surrogate_pair() {
    // Upstream counts UTF-16 units, so a limit of one lands *inside* an emoji. The prefix stops
    // before the character instead of emitting half a pair, which would not be valid `str`.
    let mut view = PartialArguments::from_text(r#"{"s": "😀x"}"#);
    let prefix = view.text_prefix("s", 1).unwrap_or_default();
    assert!(prefix.is_empty() || prefix == "😀");
}

// ---- change detection -------------------------------------------------------------------------

#[test]
fn a_view_nobody_read_reports_no_change() {
    let mut view = PartialArguments::new();
    view.append(r#"{"a": 1"#).unwrap();
    // Nothing was read, so there is nothing whose answer could have moved.
    assert!(!view.refresh());
}

#[test]
fn a_refresh_reports_a_changed_answer_exactly_once() {
    let mut view = PartialArguments::new();
    view.append(r#"{"path": "ab"#).unwrap();
    assert_eq!(view.text("path").as_deref(), Some("ab"));
    assert!(!view.refresh());

    view.append("c").unwrap();
    assert!(view.refresh());
    assert_eq!(view.text("path").as_deref(), Some("abc"));
    // The baseline advanced, so the same text does not report again.
    assert!(!view.refresh());
}

#[test]
fn a_stepped_length_reports_one_change_per_step_rather_than_per_character() {
    let mut view = PartialArguments::new();
    view.append(r#"{"s": ""#).unwrap();
    // Ask at a granularity of four characters, as a display showing "1.0 KB" would.
    assert_eq!(view.string_length("s", 4, 0), Some(0));
    assert!(!view.refresh());

    // `ceil((length + offset) / step)`: one character in steps of four is already step one, so a
    // display showing "1.0 KB" of a 4 KB step moves once per step, not once per character — the
    // remaining characters inside the same step report nothing.
    view.append("x").unwrap();
    assert!(
        view.refresh(),
        "the first character moves the rounded answer"
    );
    assert_eq!(view.string_length("s", 4, 0), Some(1));

    // Lengths 2, 3 and 4 all round to `ceil(n / 4) == 1`, so none of them is a change.
    for length in 2..=4 {
        view.append("x").unwrap();
        assert!(!view.refresh(), "length {length} is still step one");
        assert_eq!(view.string_length("s", 4, 0), Some(1));
    }
    view.append("x").unwrap();
    assert!(view.refresh(), "length 5 crosses into step two");
    assert_eq!(view.string_length("s", 4, 0), Some(2));
}

#[test]
fn a_completion_read_notices_the_value_closing() {
    let mut view = PartialArguments::new();
    view.append(r#"{"n": 12"#).unwrap();
    assert!(!view.complete("n"));
    assert!(!view.refresh());

    view.append("}").unwrap();
    assert!(view.refresh());
    assert!(view.complete("n"));
}

#[test]
fn a_content_error_that_invalidates_a_completed_field_is_reported_by_refresh() {
    let mut view = PartialArguments::new();
    view.append(r#"{"s": "ok\q""#).unwrap();
    // The closing quote arrived, so the field looked complete before anyone read its content.
    assert!(view.complete("s"));

    // Reading the content discovers the undecodable escape, which makes the field incomplete; the
    // completion read must report that on the next refresh rather than stay at its old answer.
    assert_eq!(view.text("s").as_deref(), Some("ok"));
    assert!(view.refresh());
    assert!(!view.complete("s"));
}

#[test]
fn reading_a_new_field_while_streaming_does_not_report_a_change_by_itself() {
    let mut view = PartialArguments::new();
    view.append(r#"{"a": 1"#).unwrap();
    assert_eq!(view.value("a"), None);
    // A first read establishes a baseline; it is not itself a change.
    assert!(!view.refresh());
}

// ---- append and settle ------------------------------------------------------------------------

#[test]
fn a_sealed_view_refuses_to_grow() {
    let mut view = PartialArguments::from_text(r#"{"a": 1}"#);
    assert_eq!(view.append("x"), Err("cannot append to a sealed view"));
    // The refusal left the view readable and unchanged.
    assert_eq!(view.value("a"), Some(json!(1)));
}

#[test]
fn an_empty_append_is_accepted_and_changes_nothing() {
    let mut view = PartialArguments::new();
    assert_eq!(view.append(""), Ok(()));
    assert!(view.is_empty());
    assert_eq!(view.append(r#"{"a": 1}"#), Ok(()));
    assert_eq!(view.value("a"), Some(json!(1)));
}

#[test]
fn settle_keeps_the_view_when_the_streamed_text_was_exact() {
    let mut view = PartialArguments::new();
    view.append(r#"{"path": "#).unwrap();
    view.append(r#""a.rs"}"#).unwrap();
    let mut settled = view.settle(r#"{"path": "a.rs"}"#);

    assert!(settled.is_sealed());
    assert_eq!(settled.text("path").as_deref(), Some("a.rs"));
    assert_eq!(settled.append("x"), Err("cannot append to a sealed view"));
}

#[test]
fn settle_rebuilds_when_a_fragment_was_wrong() {
    let mut view = PartialArguments::new();
    // A dropped delta: the streamed text does not match the authoritative one.
    view.append(r#"{"path": "x"}"#).unwrap();
    let mut settled = view.settle(r#"{"path": "y"}"#);

    assert!(settled.is_sealed());
    assert_eq!(settled.text("path").as_deref(), Some("y"));
}

#[test]
fn settle_rebuilds_when_the_lengths_differ() {
    let mut view = PartialArguments::new();
    view.append(r#"{"a": 1"#).unwrap();
    let mut settled = view.settle(r#"{"a": 1}"#);
    assert_eq!(settled.value("a"), Some(json!(1)));
}

#[test]
fn settle_of_an_empty_stream_is_an_empty_sealed_view() {
    let view = PartialArguments::new();
    let mut settled = view.settle("");
    assert!(settled.is_sealed());
    assert!(settled.is_empty());
    assert!(!settled.invalid());
}

#[test]
fn settle_of_a_parsed_view_rebuilds_from_the_text() {
    let view = PartialArguments::from_value(&json!({"a": 1}));
    let mut settled = view.settle(r#"{"b": 2}"#);
    assert_eq!(settled.keys(), vec!["b"]);
    assert_eq!(settled.value("b"), Some(json!(2)));
}

// ---- the scanner's memory ---------------------------------------------------------------------

#[test]
fn fragments_are_materialized_across_chunk_boundaries() {
    // The slice has to rejoin a range that spans several fragments; a wrong base offset shows up
    // as a corrupted or truncated string.
    let mut view = PartialArguments::new();
    view.append(r#"{"pa"#).unwrap();
    view.append(r#"th": "one-"#).unwrap();
    view.append("two-").unwrap();
    view.append(r#"three"}"#).unwrap();

    assert_eq!(view.text("path").as_deref(), Some("one-two-three"));
    assert_eq!(view.string_length("path", 1, 0), Some(13));
}

#[test]
fn a_long_string_without_escapes_is_counted_without_being_materialized() {
    let body = "x".repeat(100_000);
    let mut view = PartialArguments::new();
    view.append(r#"{"s": ""#).unwrap();
    view.append(&body).unwrap();

    // Only the length is asked for, so nothing needs to be joined.
    assert_eq!(view.string_length("s", 1, 0), Some(100_000));
    assert!(!view.complete("s"));

    view.append(r#""}"#).unwrap();
    assert!(view.complete("s"));
    assert_eq!(view.text("s").as_deref(), Some(body.as_str()));
}

#[test]
fn a_large_nested_value_is_skipped_and_then_parsed_once() {
    let items: Vec<String> = (0..2_000).map(|i| format!(r#"{{"i": {i}}}"#)).collect();
    let text = format!(r#"{{"items": [{}]}}"#, items.join(","));
    let mut view = PartialArguments::from_text(&text);

    assert!(view.complete("items"));
    let value = view.value("items").expect("the array should parse");
    assert_eq!(value.as_array().map(Vec::len), Some(2_000));
    // A second read answers from the cache rather than reparsing.
    assert_eq!(view.value("items"), Some(value));
}

#[test]
fn several_fields_stream_independently() {
    let mut view = PartialArguments::new();
    view.append(r#"{"a": "one", "b": "#).unwrap();
    assert_eq!(view.text("a").as_deref(), Some("one"));
    assert_eq!(view.value("b"), None);

    view.append(r#"[1, 2], "c": tr"#).unwrap();
    assert_eq!(view.value("b"), Some(json!([1, 2])));
    assert_eq!(view.text("c"), None);

    view.append("ue}").unwrap();
    // `true` is an unquoted scalar, so it is a value rather than text, and it is only readable
    // once the delimiter after it arrived.
    assert_eq!(view.value("c"), Some(json!(true)));
    assert_eq!(view.keys(), vec!["a", "b", "c"]);
    assert!(view.closed());
}

#[test]
fn a_duplicate_key_keeps_its_first_position_and_its_last_value() {
    // JSON parsers disagree about duplicates; what matters here is that the view does not report
    // the same key twice, which would make a row-per-field display render it twice.
    let mut view = PartialArguments::from_text(r#"{"a": 1, "b": 2, "a": 3}"#);
    assert_eq!(view.keys(), vec!["a", "b"]);
    assert_eq!(view.value("a"), Some(json!(3)));
}

#[test]
fn whitespace_around_every_token_is_ignored() {
    let mut view = PartialArguments::from_text("{\n\t\"a\" : \t[ 1 , 2 ]\n , \"b\" : \"x\"\n}");
    assert_eq!(view.value("a"), Some(json!([1, 2])));
    assert_eq!(view.text("b").as_deref(), Some("x"));
    assert!(!view.invalid());
}
