//! Tests for `{{variable}}` prompt-template rendering.
//!
//! Three properties carry this module, and each is asserted as a whole output string rather than
//! with `contains`, because a partial match cannot distinguish a correct substitution from one
//! that also leaked extra text:
//!
//! 1. A reference is replaced by exactly its value.
//! 2. An undefined reference is an error, never an empty string. This is the reason the module
//!    exists: a prompt that silently loses a sentence is worse than one that refuses to render.
//! 3. A value is never re-scanned, so `{{...}}` inside a value reaches the output as literal text
//!    and cannot pull in a variable the template never asked for.
//!
//! # Why the module is compiled by path
//!
//! `prompt_template` is not exported from the crate root yet; the wiring is a separate change to
//! `lib.rs`. Compiling the exact source file here keeps these tests runnable and independent of
//! that wiring, while still exercising the same code a consumer will reach through
//! `girder_core::prompt_template`. Nothing is duplicated: this is the real file. The same approach
//! is used by `tests/system_prompt.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/prompt_template.rs"]
mod prompt_template;

use prompt_template::{TemplateError, Variables, render};

/// Builds a `Variables` from pairs, panicking on a name the module rejects.
///
/// A test that has to unwrap every `set` drowns its own assertion, and a bad name here is a bug in
/// the test rather than the subject.
fn variables(pairs: &[(&str, &str)]) -> Variables {
    let mut variables = Variables::new();
    for (name, value) in pairs {
        variables.set(name, value).expect("test variable name");
    }
    variables
}

// ---- the ordinary case ------------------------------------------------------------------------

#[test]
fn substitutes_one_reference() {
    let values = variables(&[("org", "Acme")]);

    assert_eq!(render("For {{org}}.", &values).unwrap(), "For Acme.");
}

#[test]
fn substitutes_several_references() {
    let values = variables(&[
        ("org", "Acme"),
        ("team", "Platform"),
        ("today", "2026-11-14"),
    ]);

    assert_eq!(
        render("{{org}} / {{team}} on {{today}}.", &values).unwrap(),
        "Acme / Platform on 2026-11-14.",
    );
}

#[test]
fn substitutes_a_repeated_reference_every_time() {
    // Repetition is the case where a one-shot `replace` implementation looks correct and is not:
    // every occurrence must be filled, not only the first.
    let values = variables(&[("name", "Ada")]);

    assert_eq!(
        render("{{name}} and {{name}} and {{name}}", &values).unwrap(),
        "Ada and Ada and Ada",
    );
}

#[test]
fn adjacent_references_need_no_separator() {
    let values = variables(&[("a", "1"), ("b", "2")]);

    assert_eq!(render("{{a}}{{b}}", &values).unwrap(), "12");
}

#[test]
fn an_empty_value_renders_as_absence() {
    // An empty value is a deliberate answer, distinct from an undefined variable. A deployment
    // that truly wants nothing there must be able to say so.
    let values = variables(&[("note", "")]);

    assert_eq!(
        render("before{{note}}after", &values).unwrap(),
        "beforeafter"
    );
}

#[test]
fn a_value_is_copied_verbatim() {
    // Newlines, quotes, and punctuation in a value are the value's business, not the renderer's.
    let values = variables(&[("prose", "line one\nline \"two\" — 中文")]);

    assert_eq!(
        render("[{{prose}}]", &values).unwrap(),
        "[line one\nline \"two\" — 中文]",
    );
}

#[test]
fn sets_a_name_twice_replacing_the_value() {
    // Layered configuration: the last writer wins, so defaults can be overridden without the
    // caller tracking which layer supplied the value.
    let mut values = variables(&[("workspace", "/default")]);
    values.set("workspace", "/srv/docs").unwrap();

    assert_eq!(render("{{workspace}}", &values).unwrap(), "/srv/docs");
    assert_eq!(values.names(), vec!["workspace"]);
}

// ---- an undefined variable is an error ---------------------------------------------------------

#[test]
fn undefined_variable_is_an_error_not_an_empty_string() {
    // The central guarantee. Rendering to "" here would produce a prompt that is quietly missing a
    // clause, which is exactly the silent failure this module exists to prevent.
    let values = variables(&[("org", "Acme")]);

    let error = render("For {{team}}.", &values).unwrap_err();

    assert_eq!(
        error,
        TemplateError::Undefined {
            name: "team".to_owned()
        },
    );
}

#[test]
fn an_empty_variable_set_fails_any_reference() {
    let error = render("{{anything}}", &Variables::new()).unwrap_err();

    assert_eq!(
        error,
        TemplateError::Undefined {
            name: "anything".to_owned()
        },
    );
}

#[test]
fn undefined_is_reported_even_when_earlier_references_resolved() {
    // A partial render must not be returned. The first reference resolves; the second does not,
    // and the whole render is a failure.
    let values = variables(&[("a", "1")]);

    assert_eq!(
        render("{{a}} then {{b}}", &values).unwrap_err(),
        TemplateError::Undefined {
            name: "b".to_owned()
        },
    );
}

#[test]
fn an_error_names_the_missing_variable() {
    // The message must identify the variable; "undefined prompt variable" alone forces the reader
    // to re-scan the whole template to find which one.
    let error = render("{{missing}}", &Variables::new()).unwrap_err();

    assert!(
        error.to_string().contains("missing"),
        "error should name the variable: {error}",
    );
}

// ---- brackets that are not references ----------------------------------------------------------

#[test]
fn an_unclosed_bracket_is_an_error() {
    let error = render("shell {{name", &Variables::new()).unwrap_err();

    assert_eq!(error, TemplateError::Unclosed { at: 6 });
}

#[test]
fn an_unclosed_bracket_reports_its_byte_offset() {
    let error = render("abcdef{{x", &Variables::new()).unwrap_err();

    assert_eq!(error, TemplateError::Unclosed { at: 6 });
}

#[test]
fn an_unclosed_bracket_wins_over_an_undefined_name() {
    // `{{name` has a legal name followed by end of input. The template is broken, so the report
    // should be about the missing bracket, not about a variable the author may not have meant to
    // reference at all.
    let values = variables(&[("name", "Ada")]);

    assert_eq!(
        render("{{name", &values).unwrap_err(),
        TemplateError::Unclosed { at: 0 },
    );
}

#[test]
fn a_lone_brace_is_literal_prose() {
    let values = variables(&[("org", "Acme")]);

    // A single `{` never opens anything, and a `}}` outside a reference is just text. Both appear
    // in code samples and JSON snippets that prompts quote.
    assert_eq!(
        render("a { b }} c {{org}}", &values).unwrap(),
        "a { b }} c Acme",
    );
}

// ---- the escape for a literal `{{` -------------------------------------------------------------

#[test]
fn a_doubled_bracket_renders_a_literal_open() {
    // `{{{{` is the documented escape for a literal `{{`.
    assert_eq!(render("{{{{", &Variables::new()).unwrap(), "{{");
    assert_eq!(render("x {{{{ y", &Variables::new()).unwrap(), "x {{ y");
}

#[test]
fn an_escaped_reference_is_not_substituted() {
    // The escape exists so a template can show the syntax. `{{{{name}}` must produce the literal
    // text `{{name}}`, not the value of `name` — otherwise there is no way to write about
    // templates at all.
    let values = variables(&[("name", "Ada")]);

    assert_eq!(render("{{{{name}}", &values).unwrap(), "{{name}}");
}

#[test]
fn an_escape_composes_with_a_following_reference() {
    // `{{{{` immediately followed by a real reference: the escape must consume all four braces and
    // let the next `{{` open normally, so one literal `{{` can sit directly before a value.
    let values = variables(&[("name", "Ada")]);

    assert_eq!(render("{{{{{{{{{{name}}", &values).unwrap(), "{{{{Ada");
}

#[test]
fn an_escape_does_not_hide_a_later_unclosed_bracket() {
    // Escaping one bracket must not swallow the rest of the template: the scan resumes after the
    // escape, so a genuine mistake later is still reported.
    assert_eq!(
        render("{{{{ ok {{broken", &Variables::new()).unwrap_err(),
        TemplateError::Unclosed { at: 8 },
    );
}

// ---- illegal names ----------------------------------------------------------------------------

#[test]
fn rejects_a_name_with_a_space() {
    let values = variables(&[("org", "Acme")]);

    assert_eq!(
        render("{{org id}}", &values).unwrap_err(),
        TemplateError::InvalidName {
            name: "org id".to_owned()
        },
    );
}

#[test]
fn rejects_an_empty_name() {
    // `{{}}` is the reference form with nothing in it: a typo, not prose.
    assert_eq!(
        render("{{}}", &Variables::new()).unwrap_err(),
        TemplateError::InvalidName {
            name: String::new()
        },
    );
}

#[test]
fn rejects_a_name_with_punctuation() {
    for template in ["{{a-b}}", "{{a.b}}", "{{a:b}}", "{{a/b}}"] {
        assert!(
            matches!(
                render(template, &Variables::new()),
                Err(TemplateError::InvalidName { .. }),
            ),
            "{template} should be rejected as an invalid name",
        );
    }
}

#[test]
fn set_rejects_an_invalid_name() {
    // Validation lives at insertion so that a stored name is always referencable, and so the
    // caller that chose the name sees the error.
    let mut values = Variables::new();

    assert_eq!(
        values.set("bad name", "x").unwrap_err(),
        TemplateError::InvalidName {
            name: "bad name".to_owned()
        },
    );
    assert!(values.is_empty(), "a rejected name must not be stored");
}

#[test]
fn set_accepts_letters_digits_and_underscore() {
    let mut values = Variables::new();

    for name in ["a", "A", "_", "x1", "a_b_c", "0", "__"] {
        values.set(name, "v").unwrap();
    }

    assert_eq!(values.len(), 7);
}

#[test]
fn names_are_reported_in_a_stable_order() {
    // Deterministic enumeration keeps error messages and tests diffable between runs.
    let values = variables(&[("zeta", "1"), ("alpha", "2"), ("mid", "3")]);

    assert_eq!(values.names(), vec!["alpha", "mid", "zeta"]);
}

#[test]
fn an_over_long_bad_name_is_truncated_in_the_error() {
    // A forgotten brace can make "the name" the rest of the prompt. The report must stay readable
    // rather than echo the template back.
    let template = ["{{", &" ".repeat(200), "}}"].concat();

    let TemplateError::InvalidName { name } = render(&template, &Variables::new()).unwrap_err()
    else {
        panic!("expected an invalid-name error");
    };

    assert!(name.chars().count() < 200, "name was not truncated: {name}");
    assert!(name.ends_with('…'), "truncation should be visible: {name}");
}

// ---- the empty template -----------------------------------------------------------------------

#[test]
fn an_empty_template_renders_empty() {
    assert_eq!(render("", &Variables::new()).unwrap(), "");
}

#[test]
fn a_template_without_references_is_returned_unchanged() {
    let values = variables(&[("unused", "x")]);
    let template = "You are a documentation agent.\nBe concise.";

    assert_eq!(render(template, &values).unwrap(), template);
}

#[test]
fn whitespace_is_preserved_exactly() {
    let values = variables(&[("v", "V")]);

    assert_eq!(
        render("  {{v}}\n\n\t{{v}}  ", &values).unwrap(),
        "  V\n\n\tV  ",
    );
}

// ---- values are not re-scanned (injection defence) ---------------------------------------------

#[test]
fn a_value_containing_a_reference_is_not_expanded() {
    // The core injection defence. `style` arrives from configuration, and its content must never
    // be able to reach back into the template and resolve `secret`.
    let mut values = variables(&[("style", "{{secret}}")]);
    values.set("secret", "leaked").unwrap();

    assert_eq!(
        render("Style: {{style}}", &values).unwrap(),
        "Style: {{secret}}"
    );
}

#[test]
fn a_value_containing_only_braces_survives_unchanged() {
    let values = variables(&[("v", "{{{{ }} {{")]);

    assert_eq!(render("<{{v}}>", &values).unwrap(), "<{{{{ }} {{>");
}

#[test]
fn a_value_containing_an_undefined_reference_is_not_an_error() {
    // If values were re-scanned, this would fail with `Undefined` — and, worse, whether a template
    // rendered would depend on the content of someone else's data.
    let values = variables(&[("v", "{{nope}}")]);

    assert_eq!(render("{{v}}", &values).unwrap(), "{{nope}}");
}

#[test]
fn expansion_is_not_transitive() {
    // A chain of values must resolve exactly one level: `a` yields `{{b}}` as text, and `b` is
    // never consulted.
    let values = variables(&[("a", "{{b}}"), ("b", "final")]);

    assert_eq!(render("{{a}}", &values).unwrap(), "{{b}}");
}

// ---- the accessors ----------------------------------------------------------------------------

#[test]
fn get_returns_a_set_value_and_none_otherwise() {
    let values = variables(&[("org", "Acme")]);

    assert_eq!(values.get("org"), Some("Acme"));
    assert_eq!(values.get("team"), None);
}

#[test]
fn a_new_set_is_empty() {
    let values = Variables::new();

    assert!(values.is_empty());
    assert_eq!(values.len(), 0);
    assert!(values.names().is_empty());
}

#[test]
fn display_lists_names_but_not_values() {
    // A `Variables` that ends up in a log line should not carry workspace paths or secrets along
    // with it.
    let values = variables(&[("org", "Acme"), ("token", "s3cret")]);

    let shown = values.to_string();

    assert!(shown.contains("org"), "names should be listed: {shown}");
    assert!(shown.contains("token"), "names should be listed: {shown}");
    assert!(!shown.contains("Acme"), "values must not leak: {shown}");
    assert!(!shown.contains("s3cret"), "values must not leak: {shown}");
}

// ---- realistic template -----------------------------------------------------------------------

#[test]
fn renders_a_realistic_deployment_template() {
    // The shape this module is for: one template, per-workspace values, including a git branch
    // that could contain almost anything.
    let values = variables(&[
        ("org", "Acme"),
        ("workspace", "/srv/docs"),
        ("branch", "release/2026-11"),
        ("today", "2026-11-14"),
    ]);

    let template = "You are the documentation agent for {{org}}.\n\
                    Current working directory: \"{{workspace}}\".\n\
                    Current branch: {{branch}}.\n\
                    Current date: {{today}}.";

    assert_eq!(
        render(template, &values).unwrap(),
        "You are the documentation agent for Acme.\n\
         Current working directory: \"/srv/docs\".\n\
         Current branch: release/2026-11.\n\
         Current date: 2026-11-14.",
    );
}
