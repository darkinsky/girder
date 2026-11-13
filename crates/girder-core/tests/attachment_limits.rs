//! Tests for attachment admission policy (`attachment_limits`).
//!
//! The properties that matter: a prefix entry must admit every type beneath it, the reported reason
//! must be the one that is worth acting on, and the limits must include their own boundary value.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the checker as written even before the crate root wires it in. That is also why nothing
//! here reaches for the rest of `girder_core`: the only dependency is the module itself.

#[path = "../src/attachment_limits.rs"]
mod attachment_limits;

use attachment_limits::{Policy, RejectReason, Verdict, check, is_image};

/// One mebibyte, the unit the default limits are written in.
const MIB: u64 = 1024 * 1024;

/// A policy that admits images and PDFs, up to 4 MiB, four per session.
fn policy() -> Policy {
    Policy {
        max_bytes: 4 * MIB,
        allowed_types: vec![
            "image/".to_owned(),
            "application/pdf".to_owned(),
            "text/plain".to_owned(),
        ],
        max_per_session: 4,
    }
}

#[test]
fn accepts_a_file_within_every_limit() {
    assert_eq!(
        check(&policy(), "photo.png", "image/png", 1024, 0),
        Verdict::Accept
    );
}

#[test]
fn accepts_exactly_at_the_size_limit() {
    // The limit names the largest permitted file, so the boundary must be admitted; an off-by-one
    // here would silently shrink every deployment's configured cap by a byte.
    let policy = policy();
    assert_eq!(
        check(&policy, "photo.png", "image/png", policy.max_bytes, 0),
        Verdict::Accept
    );
}

#[test]
fn rejects_a_file_one_byte_over_the_limit() {
    let policy = policy();
    assert_eq!(
        check(&policy, "photo.png", "image/png", policy.max_bytes + 1, 0),
        Verdict::Reject(RejectReason::TooLarge {
            limit: policy.max_bytes,
            got: policy.max_bytes + 1,
        })
    );
}

#[test]
fn rejects_a_type_outside_the_allowlist() {
    assert_eq!(
        check(&policy(), "download.exe", "application/x-msdownload", 10, 0),
        Verdict::Reject(RejectReason::TypeNotAllowed {
            mime: "application/x-msdownload".to_owned(),
        })
    );
}

#[test]
fn prefix_entry_matches_any_subtype() {
    // The core of the feature: one `image/` entry must cover every image subtype without naming
    // each one, or every new format would need a policy edit to be storable.
    let policy = policy();
    for mime in [
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/gif",
        "image/avif",
    ] {
        assert_eq!(
            check(&policy, "picture", mime, 512, 0),
            Verdict::Accept,
            "`{mime}` should match the `image/` prefix"
        );
    }
}

#[test]
fn prefix_entry_does_not_match_a_different_top_level_type() {
    let policy = policy();
    for mime in ["video/mp4", "application/zip", "audiox/mpeg"] {
        assert!(
            matches!(
                check(&policy, "file", mime, 512, 0),
                Verdict::Reject(RejectReason::TypeNotAllowed { .. })
            ),
            "`{mime}` must not match the `image/` prefix"
        );
    }
}

#[test]
fn exact_entry_does_not_admit_siblings() {
    let policy = policy();
    assert_eq!(
        check(&policy, "doc.pdf", "application/pdf", 512, 0),
        Verdict::Accept
    );
    // `application/pdf` is an exact entry, so it must not widen to the whole `application/` family.
    assert!(matches!(
        check(&policy, "doc.zip", "application/zip", 512, 0),
        Verdict::Reject(RejectReason::TypeNotAllowed { .. })
    ));
}

#[test]
fn empty_allowlist_admits_every_type() {
    let policy = Policy {
        max_bytes: MIB,
        allowed_types: Vec::new(),
        max_per_session: 1,
    };
    // Including the case with no declared type at all: with no allowlist there is nothing to
    // disagree with.
    for mime in ["image/png", "application/x-msdownload", ""] {
        assert_eq!(
            check(&policy, "file", mime, 10, 0),
            Verdict::Accept,
            "empty allowlist should admit `{mime}`"
        );
    }
}

#[test]
fn blank_allowlist_entry_matches_nothing() {
    // A blank entry is a configuration mistake. Reading it as "allow all" would turn a typo into a
    // wide-open policy, so it must fail closed.
    let policy = Policy {
        max_bytes: MIB,
        allowed_types: vec![String::new(), "  ".to_owned()],
        max_per_session: 1,
    };
    assert!(matches!(
        check(&policy, "file", "image/png", 10, 0),
        Verdict::Reject(RejectReason::TypeNotAllowed { .. })
    ));
}

#[test]
fn rejects_when_the_session_is_full() {
    let policy = policy();
    assert_eq!(
        check(
            &policy,
            "photo.png",
            "image/png",
            10,
            policy.max_per_session
        ),
        Verdict::Reject(RejectReason::SessionFull {
            limit: policy.max_per_session,
        })
    );
    // One below the limit still has room.
    assert_eq!(
        check(
            &policy,
            "photo.png",
            "image/png",
            10,
            policy.max_per_session - 1
        ),
        Verdict::Accept
    );
}

#[test]
fn allows_a_session_with_no_attachments_when_the_limit_is_one() {
    let policy = Policy {
        max_bytes: MIB,
        allowed_types: Vec::new(),
        max_per_session: 1,
    };
    assert_eq!(check(&policy, "a", "image/png", 1, 0), Verdict::Accept);
    assert!(matches!(
        check(&policy, "a", "image/png", 1, 1),
        Verdict::Reject(RejectReason::SessionFull { .. })
    ));
}

#[test]
fn rejects_a_missing_or_blank_name() {
    for name in ["", " ", "\t\n"] {
        assert_eq!(
            check(&policy(), name, "image/png", 10, 0),
            Verdict::Reject(RejectReason::MissingName),
            "name {name:?} should count as missing"
        );
    }
}

#[test]
fn name_is_not_trimmed_out_of_a_real_name() {
    // Surrounding whitespace is stripped before judging emptiness, but a name with content stays
    // acceptable.
    assert_eq!(
        check(&policy(), "  photo.png  ", "image/png", 10, 0),
        Verdict::Accept
    );
}

#[test]
fn type_is_reported_before_size_for_a_huge_rejected_file() {
    // The ordering decision, pinned. A 100 GB `.exe` is refused for its type: compressing it changes
    // nothing, so "too large" would send the user to shrink a file that can never be stored. It
    // also stops the size limit from being probed with formats that are banned anyway.
    let verdict = check(
        &policy(),
        "huge.exe",
        "application/x-msdownload",
        100 * 1024 * 1024 * 1024,
        0,
    );
    assert_eq!(
        verdict,
        Verdict::Reject(RejectReason::TypeNotAllowed {
            mime: "application/x-msdownload".to_owned(),
        }),
        "an oversized disallowed type must report the unfixable reason"
    );
}

#[test]
fn size_is_reported_before_session_capacity() {
    // Size precedes capacity because capacity can be fixed by deleting an attachment, so the user
    // should only hear it when deleting would actually help. A full session with an oversized file
    // is still "too large".
    assert!(matches!(
        check(&policy(), "photo.png", "image/png", 50 * MIB, 4),
        Verdict::Reject(RejectReason::TooLarge { .. })
    ));
}

#[test]
fn missing_name_is_reported_before_everything_else() {
    // A malformed request is not a policy violation; reporting the name first keeps the message
    // about the request itself.
    assert_eq!(
        check(&policy(), "", "application/x-msdownload", 50 * MIB, 4),
        Verdict::Reject(RejectReason::MissingName)
    );
}

#[test]
fn media_type_parameters_and_case_are_ignored() {
    // Senders disagree on casing and attach parameters freely; judging `Image/PNG; foo=bar` as a
    // different type from `image/png` would reject legitimate uploads.
    let policy = policy();
    assert_eq!(check(&policy, "a.png", "Image/PNG", 10, 0), Verdict::Accept);
    assert_eq!(
        check(&policy, "a.txt", "text/plain; charset=utf-8", 10, 0),
        Verdict::Accept
    );
    assert_eq!(
        check(&policy, "a.png", "  image/png  ", 10, 0),
        Verdict::Accept
    );
}

#[test]
fn rejects_an_undeclared_type_when_an_allowlist_exists() {
    // No declared type cannot be matched by any entry, and admitting it would let a caller skip the
    // allowlist by sending an empty string.
    assert_eq!(
        check(&policy(), "a", "", 10, 0),
        Verdict::Reject(RejectReason::TypeNotAllowed {
            mime: String::new(),
        })
    );
}

#[test]
fn is_image_recognizes_image_types() {
    for mime in ["image/png", "image/jpeg", "image/webp", "image/svg+xml"] {
        assert!(is_image(mime), "`{mime}` should be an image");
    }
}

#[test]
fn is_image_folds_case_and_parameters() {
    assert!(is_image("IMAGE/PNG"));
    assert!(is_image("image/png; charset=binary"));
    assert!(is_image("  image/gif  "));
}

#[test]
fn is_image_rejects_non_images() {
    for mime in [
        "application/pdf",
        "text/plain",
        "video/mp4",
        "",
        "image",      // no subtype separator: not a media type at all
        "imagex/png", // a longer top-level name must not match
        "notimage/png",
    ] {
        assert!(!is_image(mime), "`{mime}` should not be an image");
    }
}

#[test]
fn rejections_explain_what_is_allowed() {
    // The message must name the allowlist; a bare "type not allowed" leaves the user guessing and
    // they resend the same file.
    let policy = policy();
    let reason = RejectReason::TypeNotAllowed {
        mime: "application/zip".to_owned(),
    };
    let message = reason.explain(&policy);
    assert!(message.contains("application/zip"), "got: {message}");
    assert!(message.contains("image/"), "got: {message}");
    assert!(message.contains("application/pdf"), "got: {message}");
}

#[test]
fn size_rejection_explains_both_numbers() {
    let reason = RejectReason::TooLarge {
        limit: 4 * MIB,
        got: 8 * MIB,
    };
    let message = reason.explain(&policy());
    assert!(message.contains("4.0 MiB"), "got: {message}");
    assert!(message.contains("8.0 MiB"), "got: {message}");
    // The exact byte counts stay in the message: a user comparing against a disk quota needs them.
    assert!(message.contains(&(4 * MIB).to_string()), "got: {message}");
    assert!(message.contains(&(8 * MIB).to_string()), "got: {message}");
}

#[test]
fn missing_name_and_full_session_explain_themselves() {
    let policy = policy();
    let missing = RejectReason::MissingName.explain(&policy);
    assert!(missing.contains("name"), "got: {missing}");

    let full = RejectReason::SessionFull {
        limit: policy.max_per_session,
    }
    .explain(&policy);
    assert!(full.contains("delete one"), "got: {full}");
    assert!(
        full.contains(&policy.max_per_session.to_string()),
        "got: {full}"
    );
}

#[test]
fn explanation_handles_an_empty_or_blank_allowlist() {
    // The "allow everything" policy still needs a truthful sentence; "allowed: " with nothing after
    // it would read as a bug.
    let open = Policy {
        max_bytes: MIB,
        allowed_types: Vec::new(),
        max_per_session: 1,
    };
    let reason = RejectReason::TooLarge { limit: 1, got: 2 };
    assert!(reason.explain(&open).contains("2 bytes"));

    let blank = Policy {
        max_bytes: MIB,
        allowed_types: vec![String::new()],
        max_per_session: 1,
    };
    let reason = RejectReason::TypeNotAllowed {
        mime: "image/png".to_owned(),
    };
    let message = reason.explain(&blank);
    assert!(message.contains("no media type"), "got: {message}");
}

#[test]
fn check_does_not_count_attachments_itself() {
    // `existing` is the caller's number. The same inputs with a different count must give different
    // verdicts; a hidden counter would ignore the argument and these two would agree.
    let policy = policy();
    assert_eq!(check(&policy, "a.png", "image/png", 10, 0), Verdict::Accept);
    assert!(matches!(
        check(&policy, "a.png", "image/png", 10, 99),
        Verdict::Reject(RejectReason::SessionFull { limit }) if limit == policy.max_per_session
    ));
}
