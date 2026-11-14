//! Tests for credential references and the leak redactor.
//!
//! Two properties are under test and only one of them is about catching secrets. The other is the
//! one that decides whether this function is usable at all: text that contains no credential must
//! come back **byte for byte**, because a redactor that rewrites ordinary prose corrupts the very
//! diagnostics it was added to protect, and a caller who notices turns it off. Most of the cases
//! below are there to pin that boundary rather than to find more secrets.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the redactor as written even before the crate root wires it in. That is also why nothing
// here reaches for the rest of `girder_core`.
#[path = "../src/credential_ref.rs"]
mod credential_ref;

use credential_ref::{CredentialRef, KNOWN_PREFIXES, RefError, redact};

// ---- CredentialRef: the grammar ----------------------------------------------------------------

#[test]
fn a_provider_and_key_name_is_accepted() {
    let reference = CredentialRef::new("deepseek/api_key").expect("a well-formed reference");
    assert_eq!(reference.as_str(), "deepseek/api_key");
    assert_eq!(reference.to_string(), "deepseek/api_key");
}

#[test]
fn a_name_with_a_dot_and_a_dash_is_accepted() {
    // Provider names carry dots (a host), key names carry dashes. Both are ordinary, not exotic.
    let reference =
        CredentialRef::new("api.example.com/my-key-2").expect("a well-formed reference");
    assert_eq!(reference.as_str(), "api.example.com/my-key-2");
}

#[test]
fn an_empty_name_is_refused() {
    assert_eq!(CredentialRef::new(""), Err(RefError::Empty));
}

#[test]
fn a_name_without_a_separator_is_refused() {
    assert_eq!(
        CredentialRef::new("deepseek"),
        Err(RefError::MissingSeparator)
    );
}

#[test]
fn a_name_with_an_empty_half_is_refused() {
    // A missing half is a missing half, not a stray character: the caller has to supply a side.
    for name in ["deepseek/", "/api_key", "a//b"] {
        assert_eq!(
            CredentialRef::new(name),
            Err(RefError::MissingSeparator),
            "{name} names a missing half"
        );
    }
}

#[test]
fn a_second_separator_is_an_invalid_character() {
    assert_eq!(
        CredentialRef::new("a/b/c"),
        Err(RefError::InvalidCharacter { character: '/' })
    );
}

#[test]
fn characters_outside_the_grammar_are_refused() {
    for (name, character) in [
        ("a b/c", ' '),
        ("a/b\t", '\t'),
        ("a/b:c", ':'),
        ("a/b@c", '@'),
        ("clé/key", 'é'),
        ("key/naïve", 'ï'),
    ] {
        assert_eq!(
            CredentialRef::new(name),
            Err(RefError::InvalidCharacter { character }),
            "{name} should be refused"
        );
    }
}

#[test]
fn reference_errors_explain_what_to_do() {
    // A message that only repeats the variant name tells an operator nothing.
    let empty = RefError::Empty.to_string();
    assert!(empty.contains("provider/key"), "{empty}");

    let missing = RefError::MissingSeparator.to_string();
    assert!(missing.contains("provider/key"), "{missing}");

    let invalid = RefError::InvalidCharacter { character: ':' }.to_string();
    assert!(invalid.contains("`:`"), "{invalid}");
}

#[test]
fn references_are_comparable_by_name() {
    let one = CredentialRef::new("deepseek/api_key").expect("valid");
    let two = CredentialRef::new("deepseek/api_key").expect("valid");
    let other = CredentialRef::new("openai/api_key").expect("valid");
    assert_eq!(one, two);
    assert_ne!(one, other);
}

// ---- redact: every recognized prefix -----------------------------------------------------------

/// One representative token per known prefix, long enough to clear the body floor.
fn sample_for(prefix: &str) -> String {
    format!("{prefix}abcdefghijklmnopqrstuvwxyz")
}

#[test]
fn every_known_prefix_is_redacted() {
    // Driven off the constant so a prefix added there without a test here cannot pass silently.
    for prefix in KNOWN_PREFIXES {
        let token = sample_for(prefix);
        let text = format!("key is {token}");
        let (output, count) = redact(&text);
        assert_eq!(count, 1, "{prefix} should be recognized");
        assert!(
            !output.contains(&token),
            "{prefix}: the whole token must be gone, got {output}"
        );
        assert!(
            output.contains(prefix),
            "{prefix}: the prefix identifies which key leaked, got {output}"
        );
    }
}

#[test]
fn a_github_personal_access_token_is_redacted() {
    let token = "ghp_0123456789abcdefghijABCDEFGHIJ012345";
    let (output, count) = redact(&format!("export GITHUB_TOKEN={token}"));
    assert_eq!(count, 1);
    assert!(!output.contains(token));
    assert!(
        output.starts_with("export GITHUB_TOKEN=ghp_0123..."),
        "{output}"
    );
}

// ---- redact: Bearer tokens ---------------------------------------------------------------------

#[test]
fn a_bearer_token_is_redacted() {
    // A JWT-shaped token carries no known prefix; the scheme is the only evidence there is.
    let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
    let (output, count) = redact(&format!("Authorization: Bearer {token}"));
    assert_eq!(count, 1);
    assert!(!output.contains(token));
    assert!(
        output.contains("Bearer eyJh..."),
        "the scheme stays readable so the header is still identifiable, got {output}"
    );
}

#[test]
fn the_bearer_scheme_is_matched_case_insensitively() {
    let token = "abcdefghijklmnopqrstuvwxyz";
    for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
        let (output, count) = redact(&format!("{scheme} {token}"));
        assert_eq!(count, 1, "{scheme} should be recognized");
        assert!(!output.contains(token));
    }
}

#[test]
fn a_word_merely_ending_in_bearer_does_not_trigger_redaction() {
    // `NotBearer` is an identifier, not a scheme; the word boundary is what keeps this out.
    let token = "abcdefghijklmnopqrstuvwxyz";
    let text = format!("NotBearer {token}");
    let (output, count) = redact(&text);
    assert_eq!(count, 0, "got {output}");
    assert_eq!(output, text);
}

#[test]
fn bearer_with_no_space_does_not_trigger_redaction() {
    let token = "abcdefghijklmnopqrstuvwxyz";
    let text = format!("Bearer{token}");
    let (output, count) = redact(&text);
    assert_eq!(count, 0, "got {output}");
    assert_eq!(output, text);
}

// ---- redact: the false-positive boundary (the core property) -----------------------------------

#[test]
fn a_plain_long_word_is_not_redacted() {
    // Twenty characters, no prefix, no scheme. This is the case the length floor exists for.
    let text = "the word internationalization should survive";
    let (output, count) = redact(text);
    assert_eq!(count, 0, "got {output}");
    assert_eq!(output, text);
}

#[test]
fn ordinary_technical_text_is_not_redacted() {
    // Text that looks secret-adjacent but carries no anchor: digests, ids, paths, model names.
    let text = "commit 3f786850e387550fdab836ed7e6dc881de23001b, build 0123456789abcdef, \
                file crates/girder-core/src/credential_ref.rs, model deepseek-chat-v3.1";
    let (output, count) = redact(text);
    assert_eq!(count, 0, "got {output}");
    assert_eq!(output, text);
}

#[test]
fn a_prefix_with_too_short_a_body_is_not_redacted() {
    // Documents say `sk-...` and `ghp_xxxx`; neither is a key, and neither may be rewritten.
    for text in [
        "set it to sk-test",
        "the value sk-xxx is a placeholder",
        "paste ghp_abc here",
        "prefix sk- is the OpenAI form",
        "gitlab uses glpat- prefixes",
    ] {
        let (output, count) = redact(text);
        assert_eq!(count, 0, "{text} -> {output}");
        assert_eq!(output, text, "{text}");
    }
}

#[test]
fn a_short_word_after_bearer_is_not_redacted() {
    // `Bearer` is an English word. A short word after it is prose until it is long enough to be a
    // token, and the bearer floor is deliberately higher than the prefixed one.
    let text = "the Bearer of bad news";
    let (output, count) = redact(text);
    assert_eq!(count, 0, "got {output}");
    assert_eq!(output, text);
}

#[test]
fn text_without_credentials_is_returned_byte_for_byte() {
    // Equality of `String` is equality of bytes; this states the contract the caller relies on to
    // detect that nothing was rewritten.
    let text = "nothing sensitive here — not even unicode, punctuation, or a 30-char identifier.";
    let (output, count) = redact(text);
    assert_eq!(count, 0);
    assert_eq!(output, text);
}

#[test]
fn an_empty_string_is_returned_unchanged() {
    assert_eq!(redact(""), (String::new(), 0));
}

// ---- redact: the masked form keeps a handle ----------------------------------------------------

#[test]
fn redaction_keeps_the_leading_characters() {
    // The operator needs to tell *which* key leaked. A blanket `***` cannot, so four characters
    // of the body survive alongside the prefix.
    let (output, count) = redact("sk-abcdefghijklmnop does something");
    assert_eq!(count, 1);
    assert_eq!(output, "sk-abcd... does something");
}

#[test]
fn a_redaction_does_not_leak_the_tail() {
    let token = "sk-0123456789abcdefghijklmnop";
    let (output, _) = redact(token);
    assert!(!output.contains("mnop"), "got {output}");
    assert!(output.len() < token.len() / 2, "got {output}");
}

#[test]
fn redaction_is_idempotent() {
    // Running the redactor over its own output must not consume the reveal prefix; a caller may
    // apply it at more than one layer.
    let once = redact("token sk-abcdefghijklmnop").0;
    let (twice, count) = redact(&once);
    assert_eq!(count, 0, "got {twice}");
    assert_eq!(twice, once);
}

// ---- redact: several credentials ---------------------------------------------------------------

#[test]
fn several_credentials_in_one_text_are_all_redacted() {
    let text = "openai sk-abcdefghijklmnop and github ghp_0123456789abcdef and \
                header Authorization: Bearer abcdefghijklmnopqrstuvwxyz";
    let (output, count) = redact(text);
    assert_eq!(count, 3, "got {output}");
    assert!(!output.contains("sk-abcdefghijklmnop"), "{output}");
    assert!(!output.contains("ghp_0123456789abcdef"), "{output}");
    assert!(!output.contains("abcdefghijklmnopqrstuvwxyz"), "{output}");
    // Everything that is not a credential is still there, in order.
    assert!(
        output.starts_with("openai sk-abcd... and github ghp_0123... and header"),
        "{output}"
    );
}

#[test]
fn adjacent_credentials_on_separate_lines_are_counted_separately() {
    let text = "sk-abcdefghijklmnop\nsk-zyxwvutsrqponmlk";
    let (output, count) = redact(text);
    assert_eq!(count, 2, "got {output}");
    assert_eq!(output, "sk-abcd...\nsk-zyxw...");
}

#[test]
fn a_credential_next_to_punctuation_is_still_found() {
    // Quoted, in JSON, or ending a sentence: the token is delimited by whatever is not a token
    // character, so the surrounding punctuation survives untouched.
    let (output, count) = redact(r#"{"api_key":"sk-abcdefghijklmnop","x":1}"#);
    assert_eq!(count, 1, "got {output}");
    assert_eq!(output, r#"{"api_key":"sk-abcd...","x":1}"#);
}
