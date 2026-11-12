//! Tests for the API key shape checks.
//!
//! Two things are under test and they are different questions. [`check`] must *name* every defect
//! it can see, with a hint that tells the operator what to do rather than merely what is wrong.
//! [`normalize`] must repair exactly the defects that have one plausible reading, and must leave
//! everything else alone — including keys that were already fine.
//!
//! The second property is the one worth stating explicitly: a normalizer that is too eager turns a
//! loud configuration error into a silent authentication failure. The last few tests exist to pin
//! that boundary, because it is easy to widen by accident.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the checker as written even before the crate root wires it in. That is also why no test
// here reaches for anything else in `girder_core`: the only dependency is the module itself.
#[path = "../src/api_key.rs"]
mod api_key;

use api_key::{KeyCheck, MIN_KEY_LEN, check, normalize};

/// A key shaped like the ones providers issue: long enough to clear every length check.
const GOOD: &str = "sk-abcdefghijklmnopqrstuvwxyz0123456789";

/// Asserts the verdict is `Suspect` and returns its two fields.
fn suspect(raw: &str) -> (String, String) {
    match check(raw) {
        KeyCheck::Suspect { problem, hint } => (problem, hint),
        KeyCheck::Ok => panic!("expected a suspect verdict for {raw:?}"),
    }
}

/// Asserts both that the verdict is `Suspect` and that its hint says something to do.
fn suspect_with_hint(raw: &str) -> (String, String) {
    let (problem, hint) = suspect(raw);
    assert!(!problem.is_empty(), "empty problem for {raw:?}");
    assert!(!hint.is_empty(), "empty hint for {raw:?}");
    (problem, hint)
}

#[test]
fn a_well_formed_key_is_ok() {
    assert_eq!(check(GOOD), KeyCheck::Ok);
    assert!(check(GOOD).is_ok());
}

#[test]
fn a_key_of_exactly_the_minimum_length_is_ok() {
    assert!(check(&"a".repeat(MIN_KEY_LEN)).is_ok());
}

#[test]
fn an_empty_key_is_suspect() {
    let (problem, _) = suspect_with_hint("");
    assert!(problem.contains("empty"), "{problem}");
}

#[test]
fn an_all_whitespace_key_is_suspect() {
    let (problem, _) = suspect_with_hint(" \t\r\n ");
    assert!(problem.contains("whitespace"), "{problem}");
}

#[test]
fn a_bearer_prefixed_key_is_suspect() {
    let (problem, hint) = suspect_with_hint(&format!("Bearer {GOOD}"));
    assert!(problem.contains("Bearer"), "{problem}");
    // The hint must say what to keep, not only what to remove.
    assert!(hint.contains("Girder adds the scheme"), "{hint}");
}

#[test]
fn a_bearer_prefix_in_another_case_is_still_suspect() {
    let (problem, _) = suspect_with_hint(&format!("bearer {GOOD}"));
    assert!(problem.contains("Bearer"), "{problem}");
}

#[test]
fn a_double_quoted_key_is_suspect() {
    let (problem, hint) = suspect_with_hint(&format!("\"{GOOD}\""));
    assert!(problem.contains("quotes"), "{problem}");
    assert!(hint.contains("JSON"), "{hint}");
}

#[test]
fn a_single_quoted_key_is_suspect() {
    let (problem, _) = suspect_with_hint(&format!("'{GOOD}'"));
    assert!(problem.contains("quotes"), "{problem}");
}

#[test]
fn a_backtick_quoted_key_is_suspect() {
    let (problem, _) = suspect_with_hint(&format!("`{GOOD}`"));
    assert!(problem.contains("quotes"), "{problem}");
}

#[test]
fn a_surrounding_whitespace_key_is_suspect() {
    let (problem, _) = suspect_with_hint(&format!("  {GOOD}  "));
    assert!(problem.contains("whitespace"), "{problem}");
}

#[test]
fn a_trailing_newline_is_suspect() {
    // The most common instance of the whitespace defect: a value read from a file that ended with
    // a newline, or echoed from a terminal.
    let (problem, _) = suspect_with_hint(&format!("{GOOD}\n"));
    assert!(problem.contains("whitespace"), "{problem}");
}

#[test]
fn a_non_ascii_key_is_suspect() {
    let (problem, hint) = suspect_with_hint("sk-abcdefghijklmnop\u{2019}qrstuvwxyz");
    assert!(problem.contains("non-ASCII"), "{problem}");
    // The offset locates the offending character in the operator's own file.
    assert!(problem.contains("byte offset"), "{problem}");
    assert!(hint.contains("plain ASCII"), "{hint}");
}

#[test]
fn a_non_breaking_space_inside_a_key_is_a_non_ascii_defect() {
    // `str::trim` does not touch an interior character, so this can only be reported as content.
    let (problem, _) = suspect_with_hint("sk-abc\u{a0}defghijklmnop");
    assert!(problem.contains("non-ASCII"), "{problem}");
}

#[test]
fn a_too_short_key_is_suspect() {
    let short = "sk-12345";
    assert!(short.len() < MIN_KEY_LEN);
    let (problem, hint) = suspect_with_hint(short);
    assert!(problem.contains("characters long"), "{problem}");
    assert!(hint.contains("placeholder"), "{hint}");
}

#[test]
fn a_key_one_character_under_the_floor_is_suspect() {
    let under = "a".repeat(MIN_KEY_LEN - 1);
    assert!(!check(&under).is_ok());
}

#[test]
fn every_suspect_verdict_carries_both_fields() {
    let cases: [&str; 7] = [
        "",
        "   ",
        "\"short\"",
        "Bearer sk-1234",
        " padded ",
        "sk-abcdefghij\u{2019}",
        "short",
    ];
    for raw in cases {
        let _ = suspect_with_hint(raw);
    }
}

#[test]
fn normalize_repairs_surrounding_whitespace() {
    let raw = format!("  {GOOD} \n");
    assert_eq!(normalize(&raw), GOOD);
    assert_eq!(check(&normalize(&raw)), KeyCheck::Ok);
}

#[test]
fn normalize_repairs_a_bearer_prefix() {
    let raw = format!("Bearer {GOOD}");
    assert_eq!(normalize(&raw), GOOD);
    assert_eq!(check(&normalize(&raw)), KeyCheck::Ok);
}

#[test]
fn normalize_repairs_wrapping_quotes() {
    let raw = format!("\"{GOOD}\"");
    assert_eq!(normalize(&raw), GOOD);
    assert_eq!(check(&normalize(&raw)), KeyCheck::Ok);
}

#[test]
fn normalize_repairs_a_value_copied_out_of_json() {
    // The composed defect: the quotes and the newline that followed the value in the file.
    let raw = format!("\"{GOOD}\"\n");
    assert_eq!(normalize(&raw), GOOD);
    assert_eq!(check(&normalize(&raw)), KeyCheck::Ok);
}

#[test]
fn normalize_repairs_a_padded_bearer_header() {
    let raw = format!("  Bearer {GOOD}\r\n");
    assert_eq!(normalize(&raw), GOOD);
    assert_eq!(check(&normalize(&raw)), KeyCheck::Ok);
}

#[test]
fn normalize_does_not_disturb_a_good_key() {
    assert_eq!(normalize(GOOD), GOOD);
    assert_eq!(check(&normalize(GOOD)), KeyCheck::Ok);
}

#[test]
fn normalize_does_not_repair_an_inner_defect() {
    // Repair is not "try things until it looks legal". A key that is genuinely wrong comes back
    // unchanged, so the caller's `check` still reports it instead of authenticating with a
    // silently edited credential.
    let broken = "sk-abcdefghijklmnop\u{2019}qrstuvwxyz";
    assert_eq!(normalize(broken), broken);
    assert!(!check(&normalize(broken)).is_ok());
}

#[test]
fn normalize_does_not_repair_a_short_key() {
    assert_eq!(normalize("sk-12345"), "sk-12345");
    assert!(!check(&normalize("sk-12345")).is_ok());
}

#[test]
fn normalize_is_idempotent() {
    for raw in [GOOD, "", "  ", "Bearer x", "\"x\"", " x ", "sk-12345"] {
        let once = normalize(raw);
        assert_eq!(normalize(&once), once, "not idempotent for {raw:?}");
    }
}

#[test]
fn normalize_terminates_on_degenerate_input() {
    // The repair loop runs to a fixed point; these inputs are the ones most likely to shrink
    // without ever settling if the loop were written carelessly.
    for raw in ["\"", "'", "`", "Bearer ", "\"\"", "''", "``"] {
        let _ = normalize(raw);
    }
}
