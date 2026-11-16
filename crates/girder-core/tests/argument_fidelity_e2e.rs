//! End-to-end fidelity of tool arguments, from the model's bytes to the tool's value.
//!
//! A tool call travels as *text*: the model emits JSON, the dispatcher measures and parses it, the
//! tool receives a `serde_json::Value`, and the result travels back as text again. Each hop is a
//! chance for a value to change shape without anyone noticing — an integer losing its low bit, a
//! grapheme being normalized, a NUL slipping through, a missing key being mistaken for an empty
//! one. None of those failures raise an error; the tool simply acts on a different value than the
//! model wrote, and the transcript looks correct.
//!
//! The tests below therefore assert **specific values**, never "no error occurred". Where the
//! current code does not enforce the property the task expected, the test pins what actually
//! happens and says so in a comment, because a test that asserted the desired behaviour instead
//! would fail against the shipped code and could only be made green by changing the module — which
//! is not this file's job (`argument_limits` is owned elsewhere and must not be edited here).
//!
//! Every test is independent: no shared mutable state, no ordering assumptions.

use serde_json::{Value, json};

use girder_core::{
    ArgumentLimitKind, ArgumentLimits, ArgumentProvenance, ContentBlock, ToolResult,
    ToolRunContext, argument_provenance, check_arguments, complete_from_defaults,
    default_argument_limits, is_text_safe, move_tool, scan_text, validate_tool_arguments,
};

/// `2^53 + 1`, the smallest positive integer a `f64` cannot represent.
///
/// Written with separators because the digits are the whole point: the last `3` is what an
/// `f64` round trip destroys, and `clippy::unreadable_literal` is right to insist the literal be
/// legible.
const ABOVE_F64_EXACT: u64 = 9_007_199_254_740_993;

/// The largest `u64`, which is still exactly representable by `serde_json` but not by `f64`.
const U64_MAX: u64 = 18_446_744_073_709_551_615;

/// The negation of [`ABOVE_F64_EXACT`], as a signed literal.
///
/// A separate constant rather than a cast, so the round-trip test measures the serializer instead
/// of measuring a conversion the test itself performed.
const NEGATIVE_ABOVE_F64_EXACT: i64 = -9_007_199_254_740_993;

/// A JSON array nested `depth` levels deep, as text.
///
/// Built iteratively: a recursive builder in the test itself would overflow the stack before the
/// code under test got a chance to.
fn nested_array(depth: usize) -> String {
    format!("{}{}{}", "[".repeat(depth), "1", "]".repeat(depth))
}

/// An object nested `depth` levels deep, as a `Value`.
fn nested_object_value(depth: usize) -> Value {
    let mut value = json!(1);
    for _ in 0..depth {
        value = json!({ "a": value });
    }
    value
}

/// A schema describing an object nested `depth` levels deep, matching [`nested_object_value`].
fn nested_object_schema(depth: usize) -> Value {
    let mut schema = json!({ "type": "integer" });
    for _ in 0..depth {
        schema = json!({ "type": "object", "properties": { "a": schema } });
    }
    schema
}

/// The text a tool result carries, for asserting on a refusal's wording.
fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---- integers -----------------------------------------------------------------------------------

#[test]
fn an_integer_above_f64_precision_survives_the_limits_check_exactly() {
    // The most important case in this file. `2^53 + 1` is the first integer `f64` cannot hold, so a
    // pipeline that ever routes a number through a float silently turns it into `2^53` — an id, a
    // nanosecond timestamp, or a file offset that is now one off, with no error anywhere.
    let arguments = format!("{{\"id\":{ABOVE_F64_EXACT}}}");

    // The check runs on the raw text and must not alter or reject it: the call is small and legal.
    assert_eq!(
        check_arguments(&default_argument_limits(), &arguments),
        None
    );

    // The value the tool receives must be the integer the model wrote, to the last digit.
    let parsed: Value = serde_json::from_str(&arguments).expect("the arguments are valid JSON");
    assert_eq!(parsed["id"].as_u64(), Some(ABOVE_F64_EXACT));
    assert_eq!(parsed["id"].to_string(), "9007199254740993");
    assert_ne!(
        parsed["id"].to_string(),
        "9007199254740992",
        "the low bit was lost, so the number went through f64"
    );

    // The loss is detectable exactly here: the same number read as a float rounds *down* to
    // `2^53`. Asserting this pins why the integer assertion above has to use `as_u64`, and fails
    // loudly if a future change starts storing the number as a float. The comparison is against
    // `9_007_199_254_740_992`, not against the literal `...993.0` — that literal is itself rounded
    // to the same float, so comparing to it would be comparing two equal values.
    let as_float = parsed["id"]
        .as_f64()
        .expect("every number has a float view");
    // Compared by bit pattern rather than by `==`: exact identity is precisely what is being
    // asserted, and `clippy::float_cmp` is right that ordinary float equality is usually a bug.
    assert_eq!(
        as_float.to_bits(),
        9_007_199_254_740_992.0_f64.to_bits(),
        "the float view of 2^53+1 is 2^53"
    );
    // Rounding the float back into a `Value` does not recover the digits: the round trip ends on
    // `2^53`, which is the "loss" in "lossy". Written as a `Value` comparison so no numeric cast has
    // to appear in the test — a cast would be the very operation whose safety is in question.
    let rounded_back = Value::from(as_float);
    assert_eq!(rounded_back.to_string(), "9007199254740992.0");
    assert_ne!(
        rounded_back, parsed["id"],
        "the float round trip must not reproduce 2^53+1"
    );
    // Worth pinning on its own: `serde_json` keeps integer and float numbers distinct under `==`
    // even when they denote the same mathematical value. So "these two arguments are equal" is not
    // the same statement as "these two arguments denote the same number", and a tool that compares
    // `Value`s will treat `9007199254740992.0` and `9007199254740992` as different inputs.
    assert_ne!(
        Value::from(9_007_199_254_740_992.0_f64),
        Value::from(9_007_199_254_740_992_u64)
    );
}

#[test]
fn the_largest_u64_survives_the_limits_check_exactly() {
    // The other end of the in-range integers: `u64::MAX` is representable by `serde_json`'s integer
    // path but rounds to an even number as a float, so this is the case that catches an
    // implementation that parses numbers as `f64` and casts back.
    let arguments = format!("{{\"mask\":{U64_MAX}}}");
    assert_eq!(
        check_arguments(&default_argument_limits(), &arguments),
        None
    );

    let parsed: Value = serde_json::from_str(&arguments).expect("the arguments are valid JSON");
    assert_eq!(parsed["mask"].as_u64(), Some(U64_MAX));
    assert_eq!(parsed["mask"].to_string(), "18446744073709551615");
}

#[test]
fn an_integer_beyond_u64_keeps_its_digits_under_arbitrary_precision() {
    // The case that decides whether 128-bit ids and nanosecond timestamps survive the trip at all.
    // `girder-core` enables `serde_json/arbitrary_precision`, so a literal past `u64::MAX` is stored
    // as written rather than widened to an `f64`. Without that feature this same input would come
    // back as `1.8446744073709552e+19` — accepted, the right order of magnitude, and the wrong id.
    //
    // The test pins both halves: the digits are preserved, *and* the value still has no `u64` view,
    // so a caller cannot accidentally take the exact-looking path and then truncate.
    let too_big = "18446744073709551616";
    let arguments = format!("{{\"id\":{too_big}}}");
    assert_eq!(
        check_arguments(&default_argument_limits(), &arguments),
        None
    );

    let parsed: Value = serde_json::from_str(&arguments).expect("the arguments are valid JSON");
    assert_eq!(
        parsed["id"].as_u64(),
        None,
        "the value is past u64, so there is no exact integer view through `as_u64`"
    );
    // The digits are kept even though no integer view can hold them: `arbitrary_precision` stores
    // the number as written, so a caller that echoes it back sends what it received.
    assert_eq!(parsed["id"].to_string(), too_big);
}

// ---- unicode ------------------------------------------------------------------------------------

#[test]
fn unicode_arguments_survive_the_limits_check_byte_for_byte() {
    // Three classes of character that a serializer is most likely to mangle: an emoji outside the
    // BMP (a surrogate pair if anything in the chain thinks in UTF-16), CJK (multi-byte UTF-8), and
    // a decomposed `e` + U+0301 (a normalizing step would silently rewrite it as U+00E9).
    let emoji = "\u{1f680}\u{1f30d}"; // rocket, globe
    let cjk = "\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}"; // "Chinese test"
    let decomposed = "e\u{301}"; // e + COMBINING ACUTE, not the precomposed U+00E9
    let arguments = json!({ "emoji": emoji, "cjk": cjk, "combining": decomposed }).to_string();

    assert_eq!(
        check_arguments(&default_argument_limits(), &arguments),
        None
    );

    let parsed: Value = serde_json::from_str(&arguments).expect("the arguments are valid JSON");
    // Byte-for-byte, not merely "renders the same": `assert_eq!` on `&str` compares bytes.
    assert_eq!(parsed["emoji"].as_str(), Some(emoji));
    assert_eq!(parsed["cjk"].as_str(), Some(cjk));
    assert_eq!(parsed["combining"].as_str(), Some(decomposed));

    // Spelled out as byte counts so a normalization or a UTF-16 round trip cannot hide behind a
    // string comparison that happens to look right in a terminal.
    assert_eq!(
        parsed["emoji"].as_str().unwrap().len(),
        8,
        "two 4-byte emoji"
    );
    assert_eq!(parsed["cjk"].as_str().unwrap().len(), 12, "four 3-byte CJK");
    assert_eq!(
        parsed["combining"].as_str().unwrap().len(),
        3,
        "`e` + 2-byte combining mark, not the 2-byte precomposed form"
    );
    assert_ne!(
        parsed["combining"].as_str(),
        Some("\u{e9}"),
        "the value was NFC-normalized, which changes bytes the model chose"
    );
}

// ---- NUL and size refusals ----------------------------------------------------------------------

#[tokio::test]
async fn a_nul_byte_is_refused_with_a_reason_that_names_the_nul() {
    // A NUL cannot travel in a Unix path argument: it truncates the name at the syscall boundary, so
    // a checked string and the path the OS sees would differ. The refusal has to *say* NUL, because
    // "invalid path" would send the model looking for a typo in a name that is byte-correct.
    let dir = std::env::temp_dir().join(format!("girder-fidelity-nul-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch directory exists");

    let arguments = json!({ "source": "a\u{0}b", "destination": "c.txt" });
    let text = arguments.to_string();

    // The size checker sees nothing wrong — a NUL is one byte and the call is tiny. Pinned here
    // because it locates the guard: the rejection below is the *tool*'s, not the checker's.
    assert_eq!(check_arguments(&default_argument_limits(), &text), None);
    // The NUL does survive the JSON round trip, so it is genuinely present when the tool sees it.
    let reparsed: Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(reparsed["source"].as_str(), Some("a\u{0}b"));

    let tool = move_tool(dir.clone());
    let result = (tool.execute)(arguments, ToolRunContext::default())
        .await
        .expect("the tool answers with a result");
    assert!(result.is_error, "a NUL in a path must be refused");
    let message = text_of(&result);
    assert!(
        message.contains("NUL"),
        "the refusal must name the NUL byte, got: {message}"
    );
    assert!(
        message.contains("source"),
        "the refusal must name the offending field, got: {message}"
    );

    // `text_safety` locates the same byte, which is what a caller that reports hazards would use.
    assert!(!is_text_safe("a\u{0}b"));
    let hazards = scan_text("a\u{0}b");
    assert_eq!(hazards.len(), 1);
    assert_eq!(hazards[0].at, 1, "the NUL is the second byte");
    assert_eq!(hazards[0].kind, girder_core::HazardKind::Control);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_oversized_string_is_refused_and_the_report_states_the_limit() {
    // The report has to carry the number: a model told only "too large" has to guess how much to cut,
    // and the guess is what produces a second oversized call.
    let limits = default_argument_limits();
    let oversized = limits.max_string_bytes + 1;
    let arguments = format!("{{\"content\":\"{}\"}}", "a".repeat(oversized));

    let found = check_arguments(&limits, &arguments).expect("the string is over the limit");
    assert_eq!(found.kind, ArgumentLimitKind::StringTooLarge);
    assert_eq!(found.field, "content");
    assert_eq!(found.actual, oversized, "the measured size is exact bytes");
    assert_eq!(found.limit, limits.max_string_bytes);

    // The rendered sentence contains the limit as a decimal, so a reader can act on it without
    // knowing the constant.
    let explained = found.explain();
    assert!(
        explained.contains(&limits.max_string_bytes.to_string()),
        "the message must state the limit, got: {explained}"
    );
    assert!(
        explained.contains(&oversized.to_string()),
        "the message must state the actual size, got: {explained}"
    );

    // One byte under the limit is accepted: the limit names the largest legal size.
    let at_limit = format!(
        "{{\"content\":\"{}\"}}",
        "a".repeat(limits.max_string_bytes)
    );
    assert_eq!(check_arguments(&limits, &at_limit), None);
}

#[test]
fn a_total_that_exceeds_the_limit_is_refused_before_any_string_is_named() {
    // With a small policy so the test allocates kilobytes rather than megabytes. The total must win
    // over the per-string finding: shrinking one field of an oversized call does not make it fit.
    let limits = ArgumentLimits {
        max_string_bytes: 64,
        max_total_bytes: 256,
        max_keys: 8,
    };
    let arguments = format!("{{\"a\":\"{}\"}}", "x".repeat(300));

    let found = check_arguments(&limits, &arguments).expect("the call is over the total limit");
    assert_eq!(found.kind, ArgumentLimitKind::TotalTooLarge);
    assert_eq!(found.actual, arguments.len());
    assert_eq!(found.limit, limits.max_total_bytes);
    assert!(found.field.is_empty(), "the finding is about the call");
}

// ---- nesting ------------------------------------------------------------------------------------

#[test]
fn deep_nesting_is_refused_by_the_parser_instead_of_overflowing_the_stack() {
    // Pinned fact, contrary to the task's premise: `serde_json`'s ceiling is **128** nested
    // containers, not 100. A 100-level argument is therefore *accepted* end to end — the size
    // checker has no depth rule at all, and the parser is happy at 127. The property that matters
    // still holds: nothing recurses without bound, and the refusal is an error, not a stack
    // overflow (a stack overflow aborts the process, so this test could not report one).
    let shallow_enough = nested_array(100);
    assert_eq!(
        check_arguments(&default_argument_limits(), &shallow_enough),
        None,
        "100 levels are under the parser's limit, so the checker has nothing to report"
    );
    assert!(
        serde_json::from_str::<Value>(&shallow_enough).is_ok(),
        "100 levels parse: the boundary is 128, not 100"
    );

    // One past the last accepted level. The checker still says nothing — it does not count depth —
    // so the parser is the only guard, and it refuses with a named error.
    let too_deep = nested_array(200);
    assert_eq!(
        check_arguments(&default_argument_limits(), &too_deep),
        None,
        "the size checker is depth-blind; this is what makes the parser the guard"
    );
    let refused = serde_json::from_str::<Value>(&too_deep);
    let error = refused.expect_err("200 levels exceed the parser's recursion limit");
    assert!(
        error.to_string().contains("recursion limit exceeded"),
        "the refusal must be the bounded-recursion error, got: {error}"
    );

    // The schema walk does have its own depth cap, and reports it instead of recursing. This is the
    // "rejected, not overflowed" evidence for the argument path that does look at structure.
    let schema = nested_object_schema(100);
    let value = nested_object_value(100);
    let violation = validate_tool_arguments(&schema, &value)
        .expect_err("a 100-level schema is past the validator's 64-level cap");
    assert!(
        violation.message.contains("nests deeper than 64 levels"),
        "got: {}",
        violation.message
    );
    // 64 levels are still walked, so the cap is a guard rather than a blanket refusal.
    assert!(validate_tool_arguments(&nested_object_schema(64), &nested_object_value(64)).is_ok());
}

#[test]
fn default_completion_on_deep_arguments_stops_instead_of_recursing() {
    // `argument_infer` fills nested defaults recursively. Its own cap (64) has to hold for a
    // hand-built value that `serde_json` could never have parsed, or the fill is a stack overflow
    // waiting for a caller that builds arguments in code.
    let schema = nested_object_schema(200);
    let mut arguments = nested_object_value(200);

    let completion = complete_from_defaults(&schema, &mut arguments);
    // Nothing to fill: the fixture supplies every level, so the walk only has to stop safely.
    assert!(completion.is_empty(), "no defaults were declared to fill");
    assert_eq!(completion.arguments, arguments);
}

// ---- empty versus absent ------------------------------------------------------------------------

#[test]
fn an_empty_string_and_an_absent_key_are_different_arguments() {
    // `""` is a value the model chose; a missing key is a value it never sent. Collapsing them is
    // how a tool ends up acting on the empty path or the empty query instead of reporting that it
    // was told nothing — the failure surfaces later, somewhere unrelated.
    let schema = json!({
        "type": "object",
        "properties": { "a": { "type": "string" } },
        "required": ["a"]
    });

    // The two calls are not the same value, and nothing normalizes one into the other.
    let empty = json!({ "a": "" });
    let absent = json!({});
    assert_ne!(empty, absent);

    // The validator separates them: the empty string satisfies `type: string`, the absent key does
    // not satisfy `required`.
    assert!(validate_tool_arguments(&schema, &empty).is_ok());
    let missing = validate_tool_arguments(&schema, &absent)
        .expect_err("a required key that is absent must be refused");
    assert!(
        missing.message.contains("required but was not supplied"),
        "got: {}",
        missing.message
    );

    // Provenance reports the same distinction from the other side, which is what lets a message say
    // "the model omitted `a`" rather than "`a` is missing".
    assert_eq!(
        argument_provenance(&schema, &empty),
        vec![("a".to_owned(), ArgumentProvenance::Provided)]
    );
    assert_eq!(
        argument_provenance(&schema, &absent),
        vec![("a".to_owned(), ArgumentProvenance::Absent)]
    );

    // The size checker is value-blind here by design — it measures bytes, not intent — so this
    // pinned fact shows the distinction is *not* its job: both are admissible small calls.
    for arguments in [&empty, &absent] {
        assert_eq!(
            check_arguments(&default_argument_limits(), &arguments.to_string()),
            None
        );
    }
}

#[test]
fn a_required_string_may_be_empty_and_that_is_not_a_missing_argument() {
    // The complement of the test above, stated as its own property: emptiness is not absence, so a
    // schema that only requires *presence* must accept `""`. A validator that treated empty as
    // missing would reject calls the model made deliberately.
    let schema = json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"]
    });
    let result = validate_tool_arguments(&schema, &json!({ "query": "" }));
    assert_eq!(result, Ok(()), "an empty string is still a supplied string");
}

// ---- round trip ---------------------------------------------------------------------------------

#[test]
fn serializing_and_reparsing_arguments_preserves_structure_and_values() {
    // The last hop before a tool runs and the first hop after it returns is a `to_string()` /
    // `from_str()` pair. If that pair is lossy, every value-level guarantee above is undone here.
    let original = json!({
        "integer": 42,
        "big": ABOVE_F64_EXACT,
        // The same magnitude with a sign, written as its own `i64` constant so no cast appears: a
        // cast here would be testing a conversion rather than the round trip under test.
        "negative": NEGATIVE_ABOVE_F64_EXACT,
        "float": 1.5,
        "zero": 0,
        "empty": "",
        "unicode": "caf\u{e9} \u{4e2d}\u{6587} \u{1f680}",
        "decomposed": "e\u{301}",
        "bool": true,
        "null": Value::Null,
        "array": [1, "two", false, Value::Null, []],
        "object": { "nested": { "deeper": [ { "k": "v" } ] } }
    });

    let serialized = original.to_string();
    let reparsed: Value = serde_json::from_str(&serialized).expect("our own output parses");
    assert_eq!(reparsed, original, "structure and every value survived");

    // Equality alone would pass if both sides were lossy in the same way, so spot-check the values
    // that have a known lossy alternative.
    assert_eq!(reparsed["big"].as_u64(), Some(ABOVE_F64_EXACT));
    assert_eq!(
        reparsed["negative"].as_i64(),
        Some(NEGATIVE_ABOVE_F64_EXACT),
        "a negative integer past f64 precision must keep its digits too"
    );
    assert_eq!(reparsed["empty"].as_str(), Some(""));
    assert_eq!(reparsed["decomposed"].as_str(), Some("e\u{301}"));

    // The serialized form is stable: a second round trip is byte-identical, so a value cannot drift
    // a little on each hop.
    let again: Value = serde_json::from_str(&reparsed.to_string()).expect("parses again");
    assert_eq!(again, original);
    assert_eq!(again.to_string(), serialized);
}

#[test]
fn key_order_is_not_preserved_but_the_map_is() {
    // A fidelity fact with no pathological consequence, pinned so it is not mistaken for data loss.
    // `serde_json`'s map is a `BTreeMap` unless `preserve_order` is enabled, so keys come back
    // sorted: a signature computed over the argument text, or a byte-for-byte transcript comparison,
    // would disagree with what the model emitted even though the value is identical.
    let written = json!({ "zebra": 1, "alpha": 2 });
    let reserialized = written.to_string();
    assert_eq!(
        reserialized, "{\"alpha\":2,\"zebra\":1}",
        "keys come back in sorted order, not in the order written"
    );

    // The argument itself is unaffected: order is a property of the text, not of the value.
    let reparsed: Value = serde_json::from_str(&reserialized).expect("parses");
    assert_eq!(reparsed, written);
    assert_eq!(reparsed["zebra"].as_u64(), Some(1));
    assert_eq!(reparsed["alpha"].as_u64(), Some(2));
}
