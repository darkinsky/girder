//! Tests for the pre-execution policy on model-written code.
//!
//! The interesting boundary is not "allowed versus refused" — that is a match on a list — but the
//! arithmetic between a request's numbers and the policy's ceiling, which is where the clamp lives.
//! Most tests therefore assert on the *effective* limits in an `Allow`, not merely on the fact that
//! the request passed.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from
//! `lib.rs` is a separate change. When it is exported, this can become
//! `use girder_core::ptc_policy::{Decision, ExecRequest, Limits, Policy};`.

#[path = "../src/ptc_policy.rs"]
mod ptc_policy;

use std::path::Path;

use ptc_policy::{Decision, ExecRequest, Limits, Policy};

/// A policy with one language per runtime the seam might target, so a test that varies only the
/// language can fail for the reason it names.
fn policy() -> Policy {
    Policy {
        languages: vec![
            "python".to_owned(),
            "javascript".to_owned(),
            "shell".to_owned(),
        ],
        default_memory_mb: 512,
        default_timeout_secs: 30,
        max_code_bytes: 1024,
    }
}

/// A workspace root that exists only as a path — the policy never touches the filesystem.
fn workspace() -> &'static Path {
    Path::new("/tmp/girder-ptc-workspace")
}

/// A request with a short program and no opinion about its limits.
fn request(language: &str, code: &str) -> ExecRequest {
    ExecRequest {
        code: code.to_owned(),
        language: language.to_owned(),
        memory_mb: None,
        timeout_secs: None,
    }
}

/// The limits of an `Allow`, panicking with the refusal reason otherwise so a failure is readable.
fn allowed(decision: Decision) -> Limits {
    match decision {
        Decision::Allow(limits) => limits,
        Decision::Refuse(reason) => panic!("expected an allow, was refused: {reason}"),
    }
}

/// The reason of a `Refuse`, panicking with the limits otherwise.
fn refused(decision: Decision) -> String {
    match decision {
        Decision::Refuse(reason) => reason,
        Decision::Allow(limits) => {
            panic!("expected a refusal, was allowed with {limits:?}")
        }
    }
}

#[test]
fn every_enabled_language_is_allowed() {
    for language in ["python", "javascript", "shell"] {
        let decision = policy().check(&request(language, "print(1)"), workspace());
        assert!(
            decision.is_allowed(),
            "`{language}` is in the whitelist but was refused: {decision:?}"
        );
    }
}

#[test]
fn the_language_match_ignores_case_and_surrounding_space() {
    let decision = policy().check(&request("  Python ", "print(1)"), workspace());
    assert!(
        decision.is_allowed(),
        "`  Python ` was refused: {decision:?}"
    );
}

#[test]
fn an_unknown_language_is_refused() {
    let reason = refused(policy().check(&request("ruby", "puts 1"), workspace()));
    assert!(
        reason.contains("ruby") && reason.contains("python"),
        "the refusal should name the language and the enabled ones, was: {reason}"
    );
}

#[test]
fn a_language_that_only_differs_by_a_trailing_character_is_refused() {
    // `pytho` is one edit away from an enabled language, which is exactly the case a fuzzy
    // comparison would have to get wrong to be worth having. There is no fuzzy comparison.
    assert!(
        !policy()
            .check(&request("pytho", "print(1)"), workspace())
            .is_allowed()
    );
}

#[test]
fn an_empty_language_is_refused_even_when_the_list_has_an_empty_entry() {
    // A policy whose config produced a blank entry must not thereby admit a request with no
    // language: the blank entry is trimmed away in comparison, and the request is refused first.
    let mut policy = policy();
    policy.languages.push(String::new());
    let reason = refused(policy.check(&request("", "print(1)"), workspace()));
    assert!(
        reason.contains("no language"),
        "the reason should be about the missing language, was: {reason}"
    );
}

#[test]
fn a_policy_that_enables_nothing_admits_nothing() {
    let mut policy = policy();
    policy.languages.clear();
    let reason = refused(policy.check(&request("python", "print(1)"), workspace()));
    assert!(
        reason.contains("(none)"),
        "an empty whitelist should render as `(none)`, was: {reason}"
    );
}

#[test]
fn code_over_the_byte_ceiling_is_refused() {
    let policy = policy();
    let oversized = "a".repeat(policy.max_code_bytes + 1);
    let reason = refused(policy.check(&request("python", &oversized), workspace()));
    assert!(
        reason.contains(&(policy.max_code_bytes + 1).to_string()),
        "the refusal should report the actual size, was: {reason}"
    );
}

#[test]
fn code_exactly_at_the_byte_ceiling_is_allowed() {
    let policy = policy();
    let at_limit = "a".repeat(policy.max_code_bytes);
    assert!(
        policy
            .check(&request("python", &at_limit), workspace())
            .is_allowed(),
        "a program of exactly max_code_bytes should be admitted"
    );
}

#[test]
fn the_ceiling_counts_bytes_rather_than_characters() {
    // Three bytes per character: 400 characters is 1,200 bytes, over a 1,024-byte ceiling. A
    // character-count check would admit this, which is why the check is on `len()`.
    let policy = policy();
    let multibyte = "\u{4e2d}".repeat(400);
    assert!(multibyte.len() > policy.max_code_bytes);
    assert!(multibyte.chars().count() < policy.max_code_bytes);
    assert!(
        !policy
            .check(&request("python", &multibyte), workspace())
            .is_allowed()
    );
}

#[test]
fn a_huge_requested_memory_limit_is_clamped_rather_than_refused() {
    // The core behaviour of this module. Asking for more than the deployment allows is not an
    // attack, so the answer is the ceiling, not a refusal.
    let mut request = request("python", "print(1)");
    request.memory_mb = Some(999_999);
    let limits = allowed(policy().check(&request, workspace()));
    assert_eq!(limits.memory_mb, 512);
}

#[test]
fn a_huge_requested_timeout_is_clamped_rather_than_refused() {
    let mut request = request("python", "print(1)");
    request.timeout_secs = Some(86_400);
    let limits = allowed(policy().check(&request, workspace()));
    assert_eq!(limits.timeout_secs, 30);
}

#[test]
fn a_request_below_the_ceiling_keeps_its_own_number() {
    // Clamping is a ceiling, not a normalisation: a caller spending less than it is allowed keeps
    // its figure, so a tight request cannot be quietly widened.
    let mut request = request("python", "print(1)");
    request.memory_mb = Some(64);
    request.timeout_secs = Some(5);
    let limits = allowed(policy().check(&request, workspace()));
    assert_eq!(limits.memory_mb, 64);
    assert_eq!(limits.timeout_secs, 5);
}

#[test]
fn defaults_apply_when_a_request_expresses_no_opinion() {
    let limits = allowed(policy().check(&request("python", "print(1)"), workspace()));
    assert_eq!(limits.memory_mb, 512);
    assert_eq!(limits.timeout_secs, 30);
}

#[test]
fn a_zero_requested_limit_is_refused_rather_than_clamped() {
    // Zero is the one value on the axis that is not a request for "less": nothing can run under it.
    let mut request = request("python", "print(1)");
    request.timeout_secs = Some(0);
    let reason = refused(policy().check(&request, workspace()));
    assert!(
        reason.contains("zero"),
        "the reason should name the zero, was: {reason}"
    );
}

#[test]
fn a_zero_policy_ceiling_refuses_every_request() {
    let mut policy = policy();
    policy.default_memory_mb = 0;
    let reason = refused(policy.check(&request("python", "print(1)"), workspace()));
    assert!(
        reason.contains("policy"),
        "the reason should blame the policy rather than the request, was: {reason}"
    );
}

#[test]
fn empty_code_is_allowed() {
    // Harmless, so refusing it would be a refusal of nothing.
    assert!(
        policy()
            .check(&request("python", ""), workspace())
            .is_allowed()
    );
}

#[test]
fn the_workspace_is_the_only_allowed_path() {
    let limits = allowed(policy().check(&request("python", "print(1)"), workspace()));
    assert_eq!(limits.allowed_paths, vec![workspace().to_path_buf()]);
}

#[test]
fn an_empty_workspace_is_refused() {
    // An empty path can resolve to the current directory in several APIs, which is not what the
    // caller meant to allow.
    let reason = refused(policy().check(&request("python", "print(1)"), Path::new("")));
    assert!(
        reason.contains("workspace"),
        "the reason should name the workspace, was: {reason}"
    );
}

#[test]
fn the_reported_allow_is_the_effective_plan() {
    // A caller should be able to read the answer as "what will happen", not "what I typed": the
    // clamp and the default both show up in the same payload.
    let mut request = request("python", "print(1)");
    request.memory_mb = Some(1_000_000);
    let decision = policy().check(&request, workspace());
    assert_eq!(
        decision,
        Decision::Allow(Limits {
            memory_mb: 512,
            timeout_secs: 30,
            allowed_paths: vec![workspace().to_path_buf()],
        })
    );
}
