//! Tests for the API version gate.
//!
//! Three properties carry the design, and each is tested from both sides because each is easy to
//! satisfy vacuously:
//!
//! 1. **A garbage token is refused, not defaulted.** The failure mode worth pinning is the
//!    permissive one: a parser that reads `2.0` or ` 2 ` as two has not failed loudly, it has
//!    accepted a second spelling for a version, and every token-keyed cache or log line is then
//!    subtly wrong. The malformed tests are therefore about what is *rejected*, and the accepted
//!    spellings are pinned exactly.
//!
//! 2. **A version newer than the build is refused and says so.** `TooNew` is the reason this module
//!    exists: the alternative is a silent clamp, which produces a client whose calls half work.
//!    The tests check the variant, both numbers in it, and — critically — that it is not
//!    `Malformed`, because collapsing the two would throw away the one message that tells the
//!    client which side to fix.
//!
//! 3. **The window is half-open and the removed bound is inclusive.** "Removed in v3" must mean v3
//!    does not serve it; an off-by-one there hides a retirement for a whole release. `since` is
//!    tested the same way, at the boundary and one below.
//!
//! The deprecation tests exist because a warning without a replacement is not a warning — it is a
//! shrug. Every `Deprecated` assertion checks `use_instead` is non-empty and names the successor.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the checker as written even before the crate root wires it in.
#[path = "../src/api/version.rs"]
pub mod api_version;

use api_version::{Compatibility, Deprecation, Requirement, Version, VersionError, check};

/// The version every "current" assertion is written against.
///
/// Spelled as a number rather than read from `CURRENT` wherever the test is about the constant's
/// value: comparing `CURRENT` to `CURRENT` would pass for any value, including one that silently
/// fell to 1 and made two of `check`'s outcomes unreachable.
const CURRENT_NUMBER: u32 = 2;

/// A requirement served from v1 with no changes.
fn since_v1() -> Requirement {
    Requirement::new(v1())
}

/// Version 1, for tests that must name a version older than the current one.
fn v1() -> Version {
    match Version::parse("v1") {
        Ok(version) => version,
        Err(error) => panic!("v1 must parse: {error}"),
    }
}

/// Version 2, the current one.
fn v2() -> Version {
    match Version::parse("2") {
        Ok(version) => version,
        Err(error) => panic!("2 must parse: {error}"),
    }
}

// ---- parsing ----------------------------------------------------------------------------------

#[test]
fn the_current_version_parses_its_own_number() {
    let text = Version::CURRENT.as_u32().to_string();
    assert_eq!(Version::parse(&text), Ok(Version::CURRENT));
}

#[test]
fn the_current_version_parses_its_own_token() {
    assert_eq!(
        Version::parse(&format!("v{CURRENT_NUMBER}")),
        Ok(Version::CURRENT)
    );
}

#[test]
fn the_current_version_is_two() {
    // Not a tautology: this is the number `check`'s window logic and the compile-time `assert!` in
    // the module both rest on.
    assert_eq!(Version::CURRENT.as_u32(), CURRENT_NUMBER);
}

#[test]
fn a_bare_number_parses() {
    assert_eq!(Version::parse("1"), Ok(v1()));
    assert_eq!(Version::parse("2"), Ok(v2()));
}

#[test]
fn a_prefixed_token_parses() {
    assert_eq!(Version::parse("v1"), Ok(v1()));
    assert_eq!(Version::parse("v2"), Ok(v2()));
}

#[test]
fn the_number_round_trips_through_the_token() {
    for number in 1..=CURRENT_NUMBER {
        let version = Version::parse(&number.to_string());
        assert!(version.is_ok(), "v{number} must parse: {version:?}");
        if let Ok(version) = version {
            assert_eq!(version.as_u32(), number);
            assert_eq!(Version::parse(&version.to_string()), Ok(version));
        }
    }
}

#[test]
fn the_token_form_is_the_wire_form() {
    assert_eq!(v1().to_string(), "v1");
    assert_eq!(Version::CURRENT.to_string(), "v2");
}

#[test]
fn versions_order_by_their_number() {
    // Monotonicity is the property the whole module assumes; a `Version` whose ordering disagreed
    // with its number would make every window comparison meaningless.
    assert!(v1() < v2());
    assert!(v2() <= Version::CURRENT);
    assert_eq!(v1().cmp(&v1()), std::cmp::Ordering::Equal);
}

// ---- garbage is refused -----------------------------------------------------------------------

#[test]
fn an_empty_token_is_empty_not_malformed() {
    // The distinction matters: an empty value is a missing field, a malformed one is a bad guess.
    assert_eq!(Version::parse(""), Err(VersionError::Empty));
}

#[test]
fn garbage_tokens_are_refused() {
    for text in [
        "v", "V1", "vV1", "version1", "1a", "1 ", " 1", "\t2", "1.0", "2.0", "0x2", "+2", "-2",
        "two", "v 2", "2\n", "１",
    ] {
        let result = Version::parse(text);
        assert!(
            matches!(result, Err(VersionError::Malformed { .. })),
            "{text:?} must be refused as malformed, got {result:?}"
        );
    }
}

#[test]
fn a_refused_token_is_quoted_back_verbatim() {
    let result = Version::parse("version2");
    match result {
        Err(VersionError::Malformed { text }) => assert_eq!(text, "version2"),
        other => panic!("expected a malformed error carrying the token, got {other:?}"),
    }
}

#[test]
fn a_leading_zero_is_not_a_second_spelling() {
    // `02` and `2` would be the same version with two tokens; only the plain form is allowed.
    for text in ["0", "00", "01", "v01", "v0"] {
        let result = Version::parse(text);
        assert!(
            matches!(result, Err(VersionError::Malformed { .. })),
            "{text:?} must be refused, got {result:?}"
        );
    }
}

#[test]
fn zero_is_not_a_version() {
    assert_eq!(
        Version::parse("0"),
        Err(VersionError::Malformed {
            text: "0".to_owned()
        })
    );
}

#[test]
fn a_number_too_wide_for_the_type_is_malformed_not_too_new() {
    // Truncating it to report `TooNew` would report a number the client never sent.
    let text = "999999999999999999999999";
    let result = Version::parse(text);
    assert!(
        matches!(result, Err(VersionError::Malformed { .. })),
        "an oversized token must be malformed, got {result:?}"
    );
}

#[test]
fn the_error_messages_are_readable() {
    // These strings are what an operator sees; a `Debug` dump would be a defect in a user-facing
    // error, so each variant is rendered and checked for the thing a reader needs from it.
    let empty = VersionError::Empty.to_string();
    assert!(empty.contains("empty"), "{empty}");

    let malformed = VersionError::Malformed {
        text: "vX".to_owned(),
    }
    .to_string();
    assert!(
        malformed.contains("vX"),
        "the message must quote the token: {malformed}"
    );

    let too_new = VersionError::TooNew {
        found: 9,
        supported: CURRENT_NUMBER,
    }
    .to_string();
    assert!(too_new.contains('9'), "{too_new}");
    assert!(too_new.contains(&CURRENT_NUMBER.to_string()), "{too_new}");
}

// ---- too new is refused -----------------------------------------------------------------------

#[test]
fn a_version_newer_than_the_build_is_too_new() {
    let result = Version::parse("v3");
    match result {
        Err(VersionError::TooNew { found, supported }) => {
            assert_eq!(found, 3);
            assert_eq!(supported, Version::CURRENT.as_u32());
        }
        other => panic!("v3 must be refused as too new, got {other:?}"),
    }
}

#[test]
fn too_new_is_not_reported_as_malformed() {
    // The whole point of the variant: the caller must be able to tell "upgrade the server" from
    // "the token was nonsense".
    let result = Version::parse(&(CURRENT_NUMBER + 1).to_string());
    assert!(
        !matches!(result, Err(VersionError::Malformed { .. })),
        "a newer well-formed version must not be malformed, got {result:?}"
    );
}

#[test]
fn far_future_versions_are_too_new() {
    for number in [CURRENT_NUMBER + 1, 7, 100, u32::MAX] {
        let result = Version::parse(&number.to_string());
        match result {
            Err(VersionError::TooNew { found, supported }) => {
                assert_eq!(found, number);
                assert_eq!(supported, CURRENT_NUMBER);
            }
            other => panic!("v{number} must be too new, got {other:?}"),
        }
    }
}

#[test]
fn the_too_new_error_names_both_versions() {
    let result = Version::parse("v9");
    let message = match result {
        Err(error) => error.to_string(),
        Ok(version) => panic!("v9 must be refused, got {version}"),
    };
    assert!(message.contains("v9"), "{message}");
    assert!(message.contains("v2"), "{message}");
    // A client reading this must learn that the fix is the server's side or its own, not the token.
    assert!(
        message.contains("upgrade") || message.contains("newer"),
        "{message}"
    );
}

// ---- availability -----------------------------------------------------------------------------

#[test]
fn a_method_inside_its_window_is_supported() {
    assert_eq!(
        check(&since_v1(), v1(), "session.list"),
        Compatibility::Supported
    );
    assert_eq!(
        check(&since_v1(), v2(), "session.list"),
        Compatibility::Supported
    );
}

#[test]
fn a_method_at_its_since_boundary_is_supported() {
    // The boundary is inclusive on the lower end: "since v2" means v2 has it.
    let requirement = Requirement::new(v2());
    assert_eq!(
        check(&requirement, v2(), "goal.get"),
        Compatibility::Supported
    );
}

#[test]
fn a_method_before_its_since_boundary_is_not_in_version() {
    let requirement = Requirement::new(v2());
    match check(&requirement, v1(), "goal.get") {
        Compatibility::NotInVersion {
            method,
            available_from,
        } => {
            assert_eq!(method, "goal.get");
            assert_eq!(available_from, v2());
        }
        other => panic!("expected not-in-version before the since bound, got {other:?}"),
    }
}

#[test]
fn a_removed_method_is_not_in_version_at_its_removal() {
    // Inclusive on the upper end: "removed in v2" means v2 does not have it.
    let requirement = Requirement {
        removed: Some(v2()),
        ..since_v1()
    };
    match check(&requirement, v2(), "session.legacy") {
        Compatibility::NotInVersion {
            method,
            available_from,
        } => {
            assert_eq!(method, "session.legacy");
            assert_eq!(available_from, v1());
        }
        other => panic!("expected not-in-version at the removed bound, got {other:?}"),
    }
}

#[test]
fn a_removed_method_is_supported_one_version_before_removal() {
    let requirement = Requirement {
        removed: Some(v2()),
        ..since_v1()
    };
    assert_eq!(
        check(&requirement, v1(), "session.legacy"),
        Compatibility::Supported
    );
}

#[test]
fn a_method_outside_its_window_is_not_in_version_at_both_ends() {
    // A window narrower than the whole range exists so both refusals are reachable at once.
    let requirement = Requirement {
        removed: Some(v2()),
        ..since_v1()
    };
    assert!(matches!(
        check(&requirement, v1(), "session.legacy"),
        Compatibility::Supported
    ));
    assert!(matches!(
        check(&requirement, v2(), "session.legacy"),
        Compatibility::NotInVersion { .. }
    ));
}

#[test]
fn the_not_in_version_error_names_the_method_it_was_asked_about() {
    // The method name is carried rather than reconstructed by the caller, so the message a client
    // shows is about the call it made.
    let requirement = Requirement::new(v2());
    match check(&requirement, v1(), "workflow.start") {
        Compatibility::NotInVersion { method, .. } => assert_eq!(method, "workflow.start"),
        other => panic!("expected not-in-version, got {other:?}"),
    }
}

// ---- deprecation ------------------------------------------------------------------------------

#[test]
fn a_deprecated_method_names_its_replacement() {
    let requirement = since_v1().with_deprecation(Deprecation {
        since: v1(),
        use_instead: "session.listPage".to_owned(),
    });
    match check(&requirement, v1(), "session.list") {
        Compatibility::Deprecated { since, use_instead } => {
            assert_eq!(since, v1());
            assert!(
                !use_instead.is_empty(),
                "a deprecation must name a replacement"
            );
            assert_eq!(use_instead, "session.listPage");
        }
        other => panic!("expected a deprecation with a replacement, got {other:?}"),
    }
}

#[test]
fn a_deprecation_does_not_fire_before_its_since() {
    // A warning that arrives before the behavior changed would tell the caller to migrate away from
    // a call that still works as documented.
    let requirement = since_v1().with_deprecation(Deprecation {
        since: v2(),
        use_instead: "session.listPage".to_owned(),
    });
    assert_eq!(
        check(&requirement, v1(), "session.list"),
        Compatibility::Supported
    );
    assert!(matches!(
        check(&requirement, v2(), "session.list"),
        Compatibility::Deprecated { .. }
    ));
}

#[test]
fn a_retired_method_is_refused_rather_than_deprecated() {
    // Availability is decided before deprecation: a method that is gone cannot also be a warning.
    let requirement = Requirement {
        removed: Some(v2()),
        ..since_v1().with_deprecation(Deprecation {
            since: v1(),
            use_instead: "session.listPage".to_owned(),
        })
    };
    assert!(matches!(
        check(&requirement, v1(), "session.list"),
        Compatibility::Deprecated { .. }
    ));
    assert!(matches!(
        check(&requirement, v2(), "session.list"),
        Compatibility::NotInVersion { .. }
    ));
}

#[test]
fn a_deprecation_carries_the_since_it_began_at() {
    let requirement = since_v1().with_deprecation(Deprecation {
        since: v2(),
        use_instead: "goal.getV2".to_owned(),
    });
    match check(&requirement, v2(), "goal.get") {
        Compatibility::Deprecated { since, .. } => assert_eq!(since, v2()),
        other => panic!("expected the deprecation's own since, got {other:?}"),
    }
}

// ---- the requirement type ---------------------------------------------------------------------

#[test]
fn a_new_requirement_has_no_removal_and_no_deprecation() {
    let requirement = Requirement::new(v1());
    assert_eq!(requirement.since, v1());
    assert_eq!(requirement.removed, None);
    assert_eq!(requirement.deprecated, None);
}

#[test]
fn a_requirement_without_a_deprecation_never_reports_one() {
    for version in [v1(), v2()] {
        assert_eq!(
            check(&since_v1(), version, "session.list"),
            Compatibility::Supported
        );
    }
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod api {
    pub use super::api_version as version;
}
