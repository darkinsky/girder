//! Tests for the pattern, enum, and length checks applied to a tool's string schema keywords.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against `src/pattern_check.rs` as written even before the crate root wires it in. That is
//! also why nothing here reaches for the rest of `girder_core`: the only dependency is the module
//! itself.
//!
//! Four properties carry the design, and the rest of the cases pin the edges around them:
//!
//! - A **character class** is the common spelling in a tool schema, so `^[a-z]+$` must accept `abc`
//!   and reject `abc1`, `ABC`, and `abc-def`.
//! - **Anchoring is the schema author's choice**: without `^` and `$` a match may sit anywhere in
//!   the value, and a checker that quietly anchors everything would accept and reject the right
//!   shapes for the wrong reason.
//! - An **invalid pattern is refused** as `BadPattern`, never as a pass. A schema typo that
//!   silently validates everything is the one outcome worse than no check at all.
//! - **Characters, not bytes**: a CJK value is three bytes per character, and both a class test and
//!   a length bound must count it as one.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the checker as written even before the crate root wires it in.
#[path = "../src/pattern_check.rs"]
mod pattern_check;

use pattern_check::{Outcome, check, check_enum, check_length};

/// Asserts `value` matches `pattern`, quoting the outcome on failure.
#[track_caller]
fn assert_matches(pattern: &str, value: &str) {
    match check(pattern, value) {
        Outcome::Ok => {}
        other => panic!("expected `{value}` to match `{pattern}`, got {other}"),
    }
}

/// Asserts `value` does not match a *valid* `pattern`.
///
/// The distinction from [`BadPattern`] is the point: a test that only asserted "not Ok" would pass
/// when the pattern was rejected outright, which is exactly the bug this suite exists to catch.
///
/// [`BadPattern`]: Outcome::BadPattern
#[track_caller]
fn assert_no_match(pattern: &str, value: &str) {
    match check(pattern, value) {
        Outcome::NoMatch { .. } => {}
        Outcome::Ok => panic!("expected `{value}` not to match `{pattern}`, but it did"),
        Outcome::BadPattern { detail, .. } => panic!(
            "expected `{value}` to be checked against `{pattern}`, but the pattern was rejected: \
             {detail}"
        ),
    }
}

/// Asserts `pattern` is rejected as unsupported, returning the detail for a message check.
#[track_caller]
fn assert_bad_pattern(pattern: &str) -> String {
    match check(pattern, "anything") {
        Outcome::BadPattern { detail, .. } => detail,
        other => panic!("expected `{pattern}` to be rejected as unsupported, got {other}"),
    }
}

// ---- literal matching --------------------------------------------------------------------------

#[test]
fn an_exact_literal_matches_only_itself() {
    assert_matches("abc", "abc");
    assert_no_match("abc", "abd");
    assert_no_match("abc", "ab");
}

#[test]
fn an_unanchored_literal_matches_inside_the_value() {
    // JSON Schema patterns are unanchored, so the literal may sit anywhere.
    assert_matches("abc", "xxabcxx");
}

#[test]
fn a_literal_pattern_reports_both_sides_on_a_miss() {
    let outcome = check("abc", "xyz");
    assert_eq!(
        outcome,
        Outcome::NoMatch {
            pattern: "abc".to_owned(),
            value: "xyz".to_owned(),
        }
    );
}

#[test]
fn the_display_forms_name_the_offending_side() {
    assert_eq!(check("abc", "abc").to_string(), "matches");
    let miss = check("abc", "xyz").to_string();
    assert!(miss.contains("xyz") && miss.contains("abc"), "got {miss}");
    let bad = check("[", "xyz").to_string();
    assert!(bad.contains("not a supported pattern"), "got {bad}");
}

// ---- character classes (core) ------------------------------------------------------------------

#[test]
fn a_lowercase_class_accepts_only_lowercase() {
    assert_matches("^[a-z]+$", "abc");
    assert_no_match("^[a-z]+$", "abc1");
    assert_no_match("^[a-z]+$", "ABC");
    assert_no_match("^[a-z]+$", "abc-def");
    assert_no_match("^[a-z]+$", "");
}

#[test]
fn an_uppercase_class_accepts_only_uppercase() {
    assert_matches("^[A-Z]+$", "ABC");
    assert_no_match("^[A-Z]+$", "ABc");
    // `[` and `Z` are ASCII neighbours, so a sloppy range test is caught here.
    assert_no_match("^[A-Z]+$", "[");
}

#[test]
fn a_digit_class_accepts_only_digits() {
    assert_matches("^[0-9]+$", "0123456789");
    assert_no_match("^[0-9]+$", "12a");
    assert_no_match("^[0-9]+$", "/");
    // The byte after `9` is `:`, which naive range arithmetic would admit.
    assert_no_match("^[0-9]+$", ":");
}

#[test]
fn a_class_combines_ranges_and_single_members() {
    assert_matches("^[a-z0-9_]+$", "my_tool_1");
    assert_no_match("^[a-z0-9_]+$", "my-tool");
    assert_no_match("^[a-z0-9_]+$", "my tool");
}

#[test]
fn a_negated_class_rejects_its_members() {
    assert_matches("^[^0-9]+$", "abc");
    assert_no_match("^[^0-9]+$", "ab3");
}

#[test]
fn a_trailing_hyphen_in_a_class_is_a_literal() {
    // `[a-z-]` is a range plus a literal hyphen, not a reversed range.
    assert_matches("^[a-z-]+$", "ab-c");
    assert_no_match("^[a-z-]+$", "ab_c");
}

#[test]
fn an_escaped_bracket_inside_a_class_is_a_literal() {
    assert_matches("^[a\\]]+$", "a]a");
    assert_no_match("^[a\\]]+$", "ab");
}

// ---- `.*` and `.+` ----------------------------------------------------------------------------

#[test]
fn dot_star_matches_the_empty_string() {
    assert_matches("^.*$", "");
    assert_matches("^.*$", "anything at all");
}

#[test]
fn dot_plus_requires_at_least_one_character() {
    assert_matches("^.+$", "a");
    assert_no_match("^.+$", "");
}

#[test]
fn dot_star_bridges_in_the_middle_of_a_pattern() {
    assert_matches("^ab.*yz$", "abyz");
    assert_matches("^ab.*yz$", "ab in the middle yz");
    assert_no_match("^ab.*yz$", "ab yzq");
    assert_no_match("^ab.*yz$", "ybz");
}

#[test]
fn a_second_dot_star_still_terminates() {
    // Two unconstrained repetitions over a value that almost matches is where a backtracking
    // implementation goes exponential; this one must simply finish.
    let value = "a".repeat(64);
    assert_no_match("^a.*a.*a.*b$", &value);
    assert_matches("^a.*a.*a.*a$", &value);
}

#[test]
fn a_repeated_class_consumes_several_characters() {
    assert_matches("^[a-z]*$", "");
    assert_matches("^[a-z]*$", "abc");
    assert_no_match("^[a-z]*$", "ab1");
    assert_matches("^[0-9]+[a-z]+$", "123abc");
    assert_no_match("^[0-9]+[a-z]+$", "123");
    // The trailing digits fail the end anchor, which is the anchor doing its job.
    assert_no_match("^[0-9]+[a-z]+$", "123abc456");
    // Unanchored, the same atoms match the middle of that value.
    assert_matches("[0-9]+[a-z]+", "123abc456");
}

// ---- anchor differences (core) ----------------------------------------------------------------

#[test]
fn without_anchors_a_match_may_start_anywhere() {
    assert_matches("[a-z]+", "9abc9");
    assert_matches("[a-z]+", "abc");
}

#[test]
fn a_start_anchor_forbids_a_prefix() {
    assert_matches("^[a-z]+", "abc9");
    assert_no_match("^[a-z]+", "9abc");
}

#[test]
fn an_end_anchor_forbids_a_suffix() {
    assert_matches("[a-z]+$", "9abc");
    assert_no_match("[a-z]+$", "abc9");
}

#[test]
fn both_anchors_require_the_whole_value() {
    assert_matches("^[a-z]+$", "abc");
    assert_no_match("^[a-z]+$", "9abc");
    assert_no_match("^[a-z]+$", "abc9");
    assert_no_match("^[a-z]+$", "9abc9");
}

#[test]
fn a_bare_caret_anchors_only_the_start() {
    // `^a` on `ab` matches because `$` is absent.
    assert_matches("^a", "ab");
    assert_no_match("^a", "ba");
}

#[test]
fn an_anchor_only_pattern_is_accepted() {
    assert_matches("^$", "");
    assert_no_match("^$", "x");
}

#[test]
fn a_path_like_value_is_rejected_by_an_identifier_pattern() {
    // The case the module exists for: a tool declaring `^[a-z][a-z0-9_]*$` for a name field.
    let name = "^[a-z][a-z0-9_]*$";
    assert_matches(name, "read_file");
    assert_no_match(name, "../../etc/passwd");
    assert_no_match(name, "/etc/passwd");
    assert_no_match(name, "ReadFile");
    assert_no_match(name, "");
    assert_no_match(name, "1read");
}

#[test]
fn the_other_anchor_enforces_a_suffix() {
    assert_matches("\\.rs$", "lib.rs");
    assert_no_match("\\.rs$", "lib.py");
    assert_no_match("\\.rs$", "lib.rs.bak");
}

// ---- invalid patterns are refused (core) -------------------------------------------------------

#[test]
fn an_unclosed_class_is_rejected() {
    let detail = assert_bad_pattern("[a-z");
    assert!(detail.contains("never closed"), "got {detail}");
}

#[test]
fn an_empty_class_is_rejected() {
    let detail = assert_bad_pattern("^[]$");
    assert!(detail.contains("empty"), "got {detail}");
}

#[test]
fn a_reversed_range_is_rejected() {
    let detail = assert_bad_pattern("[z-a]");
    assert!(detail.contains("reversed"), "got {detail}");
}

#[test]
fn a_trailing_backslash_is_rejected() {
    let detail = assert_bad_pattern("abc\\");
    assert!(detail.contains("trailing"), "got {detail}");
}

#[test]
fn shorthand_classes_are_rejected_rather_than_misread() {
    // Silently treating `\d` as a literal `d` is the worst possible answer.
    for pattern in ["^\\d+$", "^\\w+$", "^\\s+$", "\\D"] {
        let detail = assert_bad_pattern(pattern);
        assert!(detail.contains("shorthand"), "{pattern}: got {detail}");
    }
}

#[test]
fn unsupported_quantifiers_are_rejected() {
    assert!(assert_bad_pattern("^a?$").contains("`?`"));
    assert!(assert_bad_pattern("^a{2,4}$").contains("counted repetition"));
}

#[test]
fn alternation_and_groups_are_rejected() {
    assert!(assert_bad_pattern("^(a|b)$").contains("groups"));
    assert!(assert_bad_pattern("^a|b$").contains("alternation"));
}

#[test]
fn a_misplaced_anchor_is_rejected() {
    // `^` mid-pattern and `$` mid-pattern are almost always a schema bug.
    assert!(assert_bad_pattern("^a^").contains("anchors the start"));
    assert!(assert_bad_pattern("^a$b").contains("anchors the end"));
}

#[test]
fn a_dangling_quantifier_is_rejected() {
    assert!(assert_bad_pattern("^*").contains("nothing to repeat"));
    assert!(assert_bad_pattern("^+").contains("nothing to repeat"));
}

#[test]
fn a_doubled_quantifier_is_rejected() {
    assert!(assert_bad_pattern("^a**$").contains("two quantifiers"));
    assert!(assert_bad_pattern("^a+*$").contains("two quantifiers"));
}

#[test]
fn a_bad_pattern_reports_itself_not_a_match() {
    let outcome = check("[a-z", "abc");
    assert!(
        matches!(outcome, Outcome::BadPattern { .. }),
        "a malformed pattern must never read as a pass, got {outcome}"
    );
}

#[test]
fn a_bad_pattern_keeps_the_source_text() {
    let outcome = check("[z-a]", "z");
    match outcome {
        Outcome::BadPattern { pattern, .. } => assert_eq!(pattern, "[z-a]"),
        other => panic!("expected BadPattern, got {other}"),
    }
}

// ---- Chinese counted as characters ------------------------------------------------------------

#[test]
fn a_class_of_chinese_characters_matches_by_character() {
    assert_matches("^[一-龥]+$", "中文测试");
    assert_no_match("^[一-龥]+$", "中文abc");
    assert_no_match("^[一-龥]+$", "");
}

#[test]
fn a_dot_matches_one_chinese_character_not_three_bytes() {
    assert_matches("^.$", "中");
    assert_no_match("^.$", "中文");
    assert_matches("^..$", "中文");
}

#[test]
fn a_literal_chinese_string_matches() {
    assert_matches("^读文件$", "读文件");
    assert_no_match("^读文件$", "写文件");
}

#[test]
fn length_counts_chinese_characters_not_bytes() {
    // Three characters, nine bytes: a byte-based check would call this too long for `max: 4`.
    assert_eq!(check_length(None, Some(4), "中文测"), None);
    assert_eq!(check_length(Some(3), None, "中文测"), None);
    // And a byte-based minimum would wrongly accept a single character as three.
    assert!(check_length(Some(3), None, "中").is_some());
}

// ---- enum checks ------------------------------------------------------------------------------

#[test]
fn an_enum_accepts_a_listed_value() {
    assert!(check_enum(&["read", "write"], "read"));
    assert!(check_enum(&["read", "write"], "write"));
}

#[test]
fn an_enum_rejects_an_unlisted_value() {
    assert!(!check_enum(&["read", "write"], "delete"));
    assert!(!check_enum(&["read", "write"], "Read"));
    assert!(!check_enum(&["read", "write"], ""));
}

#[test]
fn an_enum_is_an_exact_comparison() {
    assert!(!check_enum(&["read"], "read "));
    assert!(!check_enum(&["read"], " rea"));
}

#[test]
fn an_empty_enum_accepts_nothing() {
    assert!(!check_enum(&[], "read"));
    assert!(!check_enum(&[], ""));
}

#[test]
fn an_enum_of_chinese_values_compares_exactly() {
    assert!(check_enum(&["读", "写"], "读"));
    assert!(!check_enum(&["读", "写"], "读 "));
}

// ---- length checks ----------------------------------------------------------------------------

#[test]
fn no_bounds_always_passes() {
    assert_eq!(check_length(None, None, ""), None);
    assert_eq!(check_length(None, None, "anything"), None);
}

#[test]
fn a_minimum_rejects_a_shorter_value() {
    assert_eq!(check_length(Some(3), None, "abc"), None);
    assert_eq!(check_length(Some(3), None, "abcd"), None);
    let message = check_length(Some(3), None, "ab").expect("two characters is shorter than three");
    assert!(
        message.contains('2') && message.contains('3'),
        "got {message}"
    );
}

#[test]
fn a_maximum_rejects_a_longer_value() {
    assert_eq!(check_length(None, Some(3), "abc"), None);
    assert_eq!(check_length(None, Some(3), ""), None);
    let message =
        check_length(None, Some(3), "abcd").expect("four characters is longer than three");
    assert!(
        message.contains('4') && message.contains('3'),
        "got {message}"
    );
}

#[test]
fn both_bounds_accept_a_value_inside_the_range() {
    assert_eq!(check_length(Some(2), Some(4), "abc"), None);
    assert!(check_length(Some(2), Some(4), "a").is_some());
    assert!(check_length(Some(2), Some(4), "abcde").is_some());
}

#[test]
fn a_zero_minimum_accepts_the_empty_string() {
    assert_eq!(check_length(Some(0), None, ""), None);
}

#[test]
fn a_zero_maximum_accepts_only_the_empty_string() {
    assert_eq!(check_length(None, Some(0), ""), None);
    assert!(check_length(None, Some(0), "a").is_some());
}

#[test]
fn an_impossible_range_still_reports_the_violated_bound() {
    // `min > max` is a schema bug the caller owns; the check still answers the question asked.
    assert!(check_length(Some(5), Some(1), "abc").is_some());
}

#[test]
fn a_very_long_value_is_measured_without_overflowing() {
    let value = "a".repeat(10_000);
    assert_eq!(check_length(None, Some(10_000), &value), None);
    assert!(check_length(None, Some(9_999), &value).is_some());
}

// ---- escapes ----------------------------------------------------------------------------------

#[test]
fn an_escaped_dot_is_a_literal() {
    assert_matches("^\\.$", ".");
    assert_no_match("^\\.$", "a");
    // Without the escape the dot is `Any` and accepts `a` as well.
    assert_matches("^.$", "a");
}

#[test]
fn an_escaped_dollar_is_a_literal() {
    assert_matches("^\\$$", "$");
    assert_no_match("^\\$$", "a");
}

#[test]
fn an_escaped_caret_is_a_literal() {
    assert_matches("^a\\^b$", "a^b");
    assert_no_match("^a\\^b$", "ab");
}

#[test]
fn an_escaped_backslash_is_a_literal() {
    assert_matches("^a\\\\b$", "a\\b");
    assert_no_match("^a\\\\b$", "ab");
}

#[test]
fn an_escaped_star_is_a_literal() {
    assert_matches("^a\\*$", "a*");
    // The escaped star is a literal, so it does not repeat the `a`.
    assert_no_match("^a\\*$", "aa*");
    assert_no_match("^a\\*$", "aaa");
}

#[test]
fn a_quoted_file_name_with_an_extension_is_matched_by_an_escaped_dot() {
    assert_matches("^[a-z]+\\.rs$", "lib.rs");
    assert_no_match("^[a-z]+\\.rs$", "libXrs");
    assert_no_match("^[a-z]+\\.rs$", "lib.rs.bak");
}
