//! Tests for the tool-retry decision layer.
//!
//! The module is wired into the crate by the integrator, so this test loads the source file
//! directly; that keeps the test runnable against `src/tool_retry.rs` alone.
//!
//! What matters here is the decision, not the wording: which failures are worth another attempt,
//! that a typo in a configured retryable list is caught instead of silently matching nothing, that
//! the attempt budget bounds the loop, and that every corrective message tells the model what to
//! change rather than only that something was wrong.

#[path = "../src/tool_retry.rs"]
mod tool_retry;

use tool_retry::{Failure, FailureReason, Policy, PolicyError, corrective_message};

fn malformed() -> Failure {
    Failure::new(
        "read_file",
        FailureReason::MalformedJson {
            detail: "expected `,` or `}` at line 1 column 24".to_owned(),
        },
    )
}

fn schema() -> Failure {
    Failure::new(
        "read_file",
        FailureReason::SchemaViolation {
            message: "args.offset must be an integer, found a string".to_owned(),
            remedy: "pass `offset` as an integer, for example 3".to_owned(),
        },
    )
}

fn tool_error() -> Failure {
    Failure::new(
        "read_file",
        FailureReason::ToolError {
            message: "no such file: src/nope.rs".to_owned(),
        },
    )
}

// ---- is_retryable per failure kind -----------------------------------------------------------

#[test]
fn malformed_json_is_retryable() {
    // Nothing ran and the arguments are still in front of the model, so a corrected call can work.
    assert!(malformed().reason.is_retryable());
    assert!(malformed().is_retryable());
}

#[test]
fn a_schema_violation_is_retryable() {
    assert!(schema().reason.is_retryable());
    assert!(schema().is_retryable());
}

#[test]
fn a_tool_error_is_not_retryable() {
    // Resending the same call reproduces a deterministic internal failure.
    assert!(!tool_error().reason.is_retryable());
    assert!(!tool_error().is_retryable());
}

#[test]
fn every_kind_reports_a_stable_code() {
    assert_eq!(malformed().reason.code(), "MALFORMED_JSON");
    assert_eq!(schema().reason.code(), "SCHEMA_VIOLATION");
    assert_eq!(tool_error().reason.code(), "TOOL_ERROR");
}

// ---- the string table ------------------------------------------------------------------------

#[test]
fn every_code_in_the_table_is_known_and_only_those_are() {
    for code in FailureReason::CODES {
        assert!(
            FailureReason::is_known_code(code),
            "`{code}` is in CODES but not recognised"
        );
    }
    assert!(!FailureReason::is_known_code("SCHEMA_VIOLATON"));
    assert!(!FailureReason::is_known_code("schema_violation"));
    assert!(!FailureReason::is_known_code(""));
}

#[test]
fn only_the_argument_kinds_are_retryable_codes() {
    assert!(FailureReason::is_retryable_code("MALFORMED_JSON"));
    assert!(FailureReason::is_retryable_code("SCHEMA_VIOLATION"));
    assert!(!FailureReason::is_retryable_code("TOOL_ERROR"));
}

// ---- typo detection: the core of the string-list design --------------------------------------

#[test]
fn a_misspelled_retryable_code_is_reported() {
    // This is the failure the string list trades for configurability: the name matches nothing, so
    // the retry it was meant to enable never happens. Validation is what turns it into an error.
    let policy = Policy {
        max_retries: 3,
        retryable: vec!["MALFORMED_JSON", "SCHEMA_VIOLATON"],
    };

    let error = policy.validate().expect_err("the typo must be reported");
    assert_eq!(error.unknown(), ["SCHEMA_VIOLATON".to_owned()]);
    assert!(error.ineffective().is_empty());
}

#[test]
fn the_typo_error_names_the_valid_kinds() {
    // A message that only says "invalid" leaves the reader guessing at the spelling.
    let policy = Policy {
        max_retries: 1,
        retryable: vec!["MALFORMED_JSONN"],
    };

    let text = policy
        .validate()
        .expect_err("the typo must be reported")
        .to_string();
    assert!(text.contains("MALFORMED_JSONN"), "got: {text}");
    assert!(text.contains("MALFORMED_JSON`"), "got: {text}");
    assert!(text.contains("SCHEMA_VIOLATION`"), "got: {text}");
    assert!(text.contains("TOOL_ERROR`"), "got: {text}");
}

#[test]
fn listing_a_never_retryable_kind_is_reported_as_ineffective() {
    // Known, spelled correctly, and still a no-op: worth saying out loud for the same reason.
    let policy = Policy {
        max_retries: 2,
        retryable: vec!["TOOL_ERROR"],
    };

    let error = policy
        .validate()
        .expect_err("ineffective entries are reported");
    assert!(error.unknown().is_empty());
    assert_eq!(error.ineffective(), ["TOOL_ERROR".to_owned()]);
    assert!(error.to_string().contains("no effect"));
}

#[test]
fn both_problems_are_reported_together() {
    let policy = Policy {
        max_retries: 2,
        retryable: vec!["NOPE", "TOOL_ERROR"],
    };

    let text = policy
        .validate()
        .expect_err("both are reported")
        .to_string();
    assert!(text.contains("NOPE"), "got: {text}");
    assert!(text.contains("TOOL_ERROR"), "got: {text}");
}

#[test]
fn the_policy_error_exposes_its_parts() {
    // Only the validator builds a `PolicyError`, so the accessors are exercised through it: a caller
    // that wants to log or route the problem needs the entries back, not just the rendered text.
    let policy = Policy {
        max_retries: 2,
        retryable: vec!["MALFORMED_JSONN", "TOOL_ERROR"],
    };

    let error: PolicyError = policy.validate().expect_err("both entries are problems");
    assert_eq!(error.unknown(), ["MALFORMED_JSONN".to_owned()]);
    assert_eq!(error.ineffective(), ["TOOL_ERROR".to_owned()]);
}

// ---- the empty policy ------------------------------------------------------------------------

#[test]
fn the_empty_policy_is_valid_and_never_retries() {
    // Turning retries off is a configuration, not an error.
    let policy = Policy::none();
    policy.validate().expect("an empty list is valid");
    assert_eq!(policy.max_retries, 0);
    assert!(!policy.should_retry(&malformed(), 1));
    assert!(!policy.should_retry(&schema(), 1));
}

#[test]
fn a_policy_with_a_budget_and_no_kinds_still_retries_nothing() {
    // A positive budget with nothing to spend it on is useless but not invalid.
    let policy = Policy {
        max_retries: 5,
        retryable: Vec::new(),
    };
    policy.validate().expect("an empty list is valid");
    assert!(!policy.should_retry(&malformed(), 1));
}

// ---- attempt counting ------------------------------------------------------------------------

#[test]
fn the_recommended_policy_allows_the_model_to_fix_its_arguments() {
    let policy = Policy::recommended();
    policy.validate().expect("the shipped policy must be valid");

    assert!(policy.should_retry(&malformed(), 1));
    assert!(policy.should_retry(&schema(), 1));
    assert!(policy.should_retry(&schema(), 2));
    // Out of budget.
    assert!(!policy.should_retry(&schema(), 3));
    // Never worth it, whatever the budget.
    assert!(!policy.should_retry(&tool_error(), 1));
}

#[test]
fn the_budget_bounds_the_number_of_attempts() {
    let policy = Policy {
        max_retries: 3,
        retryable: vec!["MALFORMED_JSON"],
    };

    for attempt in 1..=3 {
        assert!(
            policy.should_retry(&malformed(), attempt),
            "attempt {attempt}"
        );
    }
    for attempt in 4..10 {
        assert!(
            !policy.should_retry(&malformed(), attempt),
            "attempt {attempt}"
        );
    }
}

#[test]
fn attempt_zero_is_not_a_failure_so_it_is_not_retried() {
    let policy = Policy {
        max_retries: 10,
        retryable: vec!["MALFORMED_JSON"],
    };
    assert!(!policy.should_retry(&malformed(), 0));
}

#[test]
fn a_kind_outside_the_policy_is_not_retried_even_when_retryable() {
    // The failure is worth another attempt on its own merits, but this policy does not cover it.
    let policy = Policy {
        max_retries: 5,
        retryable: vec!["MALFORMED_JSON"],
    };
    assert!(policy.should_retry(&malformed(), 1));
    assert!(!policy.should_retry(&schema(), 1));
}

#[test]
fn an_unvalidated_typo_matches_nothing_rather_than_panicking() {
    // `should_retry` runs on the failure path and must stay total; the typo is caught by `validate`.
    let policy = Policy {
        max_retries: 3,
        retryable: vec!["SCHEMA_VIOLATON"],
    };
    assert!(!policy.should_retry(&schema(), 1));
}

// ---- corrective messages ---------------------------------------------------------------------

#[test]
fn a_malformed_json_message_gives_the_position_and_the_usual_causes() {
    let message = corrective_message(&malformed());

    assert!(message.contains("read_file"), "got: {message}");
    assert!(message.contains("not valid JSON"), "got: {message}");
    assert!(message.contains("line 1, column 24"), "got: {message}");
    // Concrete edits, not just "fix your JSON".
    assert!(message.contains("Fix:"), "got: {message}");
    assert!(message.contains("missing closing quote"), "got: {message}");
}

#[test]
fn a_malformed_json_message_without_a_position_still_guides() {
    let failure = Failure::new(
        "grep",
        FailureReason::MalformedJson {
            detail: "EOF while parsing a value".to_owned(),
        },
    );
    let message = corrective_message(&failure);

    assert!(message.contains("no position"), "got: {message}");
    assert!(message.contains("Fix:"), "got: {message}");
    assert!(!message.contains("line 0"), "got: {message}");
}

#[test]
fn a_schema_message_carries_the_remedy_verbatim() {
    let message = corrective_message(&schema());

    assert!(message.contains("read_file"), "got: {message}");
    assert!(
        message.contains("args.offset must be an integer"),
        "got: {message}"
    );
    assert!(
        message.contains("pass `offset` as an integer, for example 3"),
        "got: {message}"
    );
    assert!(message.contains("Fix:"), "got: {message}");
}

#[test]
fn a_tool_error_message_says_retrying_will_not_help() {
    let message = corrective_message(&tool_error());

    assert!(message.contains("read_file"), "got: {message}");
    assert!(message.contains("src/nope.rs"), "got: {message}");
    assert!(
        message.contains("not an argument problem"),
        "got: {message}"
    );
    // It must still offer a direction rather than only refusing.
    assert!(message.contains("transient"), "got: {message}");
}

#[test]
fn every_message_names_its_tool() {
    for failure in [malformed(), schema(), tool_error()] {
        let message = corrective_message(&failure);
        assert!(
            message.contains(failure.tool.as_str()),
            "message did not name `{}`: {message}",
            failure.tool
        );
    }
}

#[test]
fn every_message_ends_with_an_instruction() {
    // The observed failure mode of a bare error message is a resend; each message must ask for a
    // change instead of reporting and stopping.
    for failure in [malformed(), schema(), tool_error()] {
        let message = corrective_message(&failure);
        assert!(
            message.contains("Fix:") || message.contains("Read the message"),
            "message gives no direction: {message}"
        );
    }
}

// ---- parser position extraction --------------------------------------------------------------

#[test]
fn a_position_is_read_from_the_parser_detail() {
    let failure = Failure::new(
        "read_file",
        FailureReason::MalformedJson {
            detail: "key must be a string at line 12 column 7".to_owned(),
        },
    );
    assert!(corrective_message(&failure).contains("line 12, column 7"));
}

#[test]
fn a_large_position_does_not_overflow() {
    let failure = Failure::new(
        "read_file",
        FailureReason::MalformedJson {
            detail: "trailing characters at line 999999 column 5".to_owned(),
        },
    );
    assert!(corrective_message(&failure).contains("line 999999, column 5"));
}
