//! Tests for the error taxonomy.
//!
//! The claim this module makes is narrow and testable: *one* condition — "that id names no
//! session" — has to reach a client as *one* category, however the method that discovered it chose
//! to report it. So the central tests here are not about wording; they reproduce the exact four
//! shapes a `DirectoryCatalog` produces for one absent id (`Ok(false)`, `UNKNOWN_METHOD`,
//! `INVALID_ARGUMENTS`, `FAILED`) and assert that all four collapse onto `NotFound`.
//!
//! That is paired with the opposite property, which is just as important: a genuine argument error
//! and a genuine method typo must *not* be absorbed into `NotFound`. A taxonomy that answers
//! "missing" too eagerly is worse than the status quo, because it turns a request the caller can
//! fix into a phantom "create it" branch — so the negative cases below are as load-bearing as the
//! positive ones.
//!
//! `looks_missing` is pinned at its boundaries: the crate's own wording (`no session '<id>'`) is
//! in, the ambiguous `no session id given` is out, and a bare `error` is out.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against the taxonomy as written even before the crate root wires it in. It resolves
//! `girder_core::…` through the alias the library declares for itself, which is why the same source
//! builds here.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/util/error_taxonomy.rs"]
pub mod error_taxonomy;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use error_taxonomy::{Category, categorize, categorize_removal, looks_missing, tag};
use girder_core::{
    DirectoryCatalog, ProjectionRegistry, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog,
    register_session_state_method,
};

/// A fresh scratch tree with its session directory, isolated per test and per process.
///
/// The same shape the error suite uses: nothing is pre-seeded, so "the catalog found nothing" is a
/// property of the fixture rather than something each test has to remember to arrange.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("girder-taxonomy-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("sessions")).expect("scratch tree");
    root
}

/// The session directory inside a scratch tree.
fn sessions(root: &Path) -> PathBuf {
    root.join("sessions")
}

/// A catalog over the scratch tree's session directory.
fn directory_catalog(root: &Path) -> DirectoryCatalog {
    DirectoryCatalog::new(sessions(root))
}

/// Runs `session.state` against a catalog over `root`, the way a client reaches it.
async fn state(root: &Path, id: &str) -> Result<serde_json::Value, RemoteError> {
    let catalog: Arc<dyn SessionCatalog> = Arc::new(directory_catalog(root));
    let projections = Arc::new(ProjectionRegistry::new());
    let mut registry = RemoteRegistry::new();
    register_session_state_method(&mut registry, &catalog, &projections);
    registry
        .dispatch(
            "session",
            "state",
            RemoteCall {
                args: serde_json::json!({ "sessionId": id }),
                peer: "test".to_owned(),
            },
        )
        .await
}

/// The id a `NotFound` names, or `None` for any other category.
fn missing_id(category: &Category) -> Option<&str> {
    match category {
        Category::NotFound { id, .. } => Some(id.as_str()),
        _ => None,
    }
}

// ---- the core property: four shapes, one category ----------------------------------------------

#[tokio::test]
async fn delete_reporting_false_is_not_found() {
    let root = scratch("delete");
    let catalog = directory_catalog(&root);
    let removed = catalog.delete("nope").await.expect("no error for absence");

    // `delete` reports absence through its success value, so there is no `RemoteError` to classify:
    // the taxonomy has to accept the flag itself or this case is simply unrepresentable.
    assert!(
        !removed,
        "the fixture guarantees nothing was there to delete"
    );
    let category =
        categorize_removal("session", "nope", removed).expect("removing nothing is an absence");

    assert_eq!(tag(&category), "NOT_FOUND");
    assert_eq!(missing_id(&category), Some("nope"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn rename_reporting_unknown_method_is_not_found() {
    let root = scratch("rename");
    let catalog = directory_catalog(&root);

    let error = catalog
        .rename("nope", "label")
        .await
        .expect_err("renaming a missing session fails");
    // The premise: the variant says "you misspelled the method", which is not what happened.
    assert_eq!(error.code(), "UNKNOWN_METHOD");

    let category = categorize("session", "rename", &error);
    assert_eq!(
        tag(&category),
        "NOT_FOUND",
        "a missing session must not be reported as a method typo: {error}"
    );
    assert_eq!(missing_id(&category), Some("nope"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn fork_reporting_invalid_arguments_is_not_found() {
    let root = scratch("fork");
    let catalog = directory_catalog(&root);

    let error = catalog
        .fork("nope", None)
        .await
        .expect_err("forking a missing session fails");
    // The premise: the request was well formed, so this code points at the caller's own bug.
    assert_eq!(error.code(), "INVALID_ARGUMENTS");

    let category = categorize("session", "fork", &error);
    assert_eq!(
        tag(&category),
        "NOT_FOUND",
        "a missing source session is an absence, not a malformed request: {error}"
    );
    assert_eq!(missing_id(&category), Some("nope"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn state_reporting_failed_is_not_found() {
    let root = scratch("state");
    let error = state(&root, "nope").await.expect_err("state refuses");
    // The premise: the fallback bucket is where an absence hides when nothing else fits.
    assert_eq!(error.code(), "FAILED");

    let category = categorize("session", "state", &error);
    assert_eq!(
        tag(&category),
        "NOT_FOUND",
        "the fallback bucket must still classify as an absence: {error}"
    );
    assert_eq!(missing_id(&category), Some("nope"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn one_missing_id_arrives_as_one_category_from_all_four_methods() {
    // This is the reason the module exists. Four methods, four wire shapes, one fact -- and after
    // classification, one tag and one id, so a client can branch on the answer instead of on
    // whichever method happened to be called.
    let root = scratch("same-id");
    let catalog = directory_catalog(&root);

    let removed = catalog.delete("ghost").await.expect("no error");
    let deleted = categorize_removal("session", "ghost", removed).expect("absence");

    let renamed = catalog.rename("ghost", "label").await.expect_err("missing");
    let forked = catalog.fork("ghost", None).await.expect_err("missing");
    let described = state(&root, "ghost").await.expect_err("missing");

    let categories = [
        deleted,
        categorize("session", "rename", &renamed),
        categorize("session", "fork", &forked),
        categorize("session", "state", &described),
    ];

    let tags: Vec<&str> = categories.iter().map(tag).collect();
    assert_eq!(
        tags,
        ["NOT_FOUND", "NOT_FOUND", "NOT_FOUND", "NOT_FOUND"],
        "the spread is the bug; the convergence is the fix"
    );
    let ids: Vec<Option<&str>> = categories.iter().map(missing_id).collect();
    assert_eq!(
        ids,
        [Some("ghost"), Some("ghost"), Some("ghost"), Some("ghost")],
        "and every branch names the same absent id"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- the negative cases: absence must not absorb real mistakes --------------------------------

#[test]
fn a_genuine_argument_error_stays_an_argument_error() {
    // A blank title is the caller's mistake and is fixable by changing the request. Classifying it
    // as `NotFound` would send the caller to create a session that already exists.
    let error = RemoteError::InvalidArguments {
        namespace: "session".to_owned(),
        method: "rename".to_owned(),
        message: "a title cannot be blank".to_owned(),
    };
    let category = categorize("session", "rename", &error);
    assert_eq!(tag(&category), "INVALID_ARGUMENTS");
    assert_eq!(
        category,
        Category::InvalidArguments {
            detail: "a title cannot be blank".to_owned()
        }
    );
}

#[test]
fn a_rejected_id_is_an_argument_error_rather_than_a_missing_session() {
    // `validate_id`'s wording names the id but never says it is absent, so the conservative reader
    // keeps it as a rejected request. This is the case an over-eager string match would get wrong.
    let error = RemoteError::InvalidArguments {
        namespace: "session".to_owned(),
        method: "id".to_owned(),
        message: "session id `../etc/passwd` contains a path separator".to_owned(),
    };
    let category = categorize("session", "read", &error);
    assert_eq!(
        tag(&category),
        "INVALID_ARGUMENTS",
        "a hostile id is refused, not missing"
    );
}

#[tokio::test]
async fn a_real_method_typo_stays_unknown_method() {
    // The registry, not a hand-built variant: this is what a client gets for `session.nope`.
    let registry = RemoteRegistry::new();
    let error = registry
        .dispatch(
            "session",
            "nope",
            RemoteCall {
                args: serde_json::Value::Null,
                peer: "test".to_owned(),
            },
        )
        .await
        .expect_err("nothing is registered under that name");
    assert_eq!(error.code(), "UNKNOWN_METHOD");

    let category = categorize("session", "nope", &error);
    assert_eq!(
        tag(&category),
        "UNKNOWN_METHOD",
        "a method that does not exist must not be read as a session that does not: {error}"
    );
    assert_eq!(missing_id(&category), None);
}

#[test]
fn a_method_name_that_only_looks_like_an_id_extension_is_not_not_found() {
    // The synthesized-id shape is `rename '<id>'`. A typo that happens to extend the requested
    // name without a quoted id is still a typo, and this is the guard that keeps the two apart.
    let error = RemoteError::UnknownMethod {
        namespace: "session".to_owned(),
        method: "renameNow".to_owned(),
    };
    let category = categorize("session", "rename", &error);
    assert_eq!(tag(&category), "UNKNOWN_METHOD");
}

#[test]
fn a_refusal_is_its_own_category() {
    let error = RemoteError::Refused {
        namespace: "workspace".to_owned(),
        method: "write".to_owned(),
        reason: "read-only peer".to_owned(),
    };
    let category = categorize("workspace", "write", &error);
    assert_eq!(tag(&category), "REFUSED");
    assert_eq!(
        category,
        Category::Refused {
            rule: "read-only peer".to_owned()
        }
    );
}

#[test]
fn a_gap_in_the_build_is_a_failure_rather_than_a_typo() {
    // `NotImplemented` means the call is spelled correctly and this build lacks it. Sending an
    // operator after a spelling mistake would waste the one lead the error gives them.
    let error = RemoteError::NotImplemented {
        namespace: "session".to_owned(),
        method: "archive".to_owned(),
    };
    let category = categorize("session", "archive", &error);
    assert_eq!(tag(&category), "FAILED");
    assert!(matches!(category, Category::Failed { .. }));
}

#[test]
fn a_failure_that_says_nothing_about_absence_stays_a_failure() {
    let error = RemoteError::Failed("connection reset by peer".to_owned());
    let category = categorize("session", "state", &error);
    assert_eq!(
        tag(&category),
        "FAILED",
        "a transient fault is retryable and must not invite a create"
    );
}

// ---- `looks_missing` at its boundaries --------------------------------------------------------

#[test]
fn looks_missing_accepts_only_wording_that_states_absence() {
    for message in [
        "No such file or directory (os error 2)",
        "session `x` does not exist",
        "doesn't exist",
        "not found",
        "cannot find the log",
        "could not find `x`",
        "no session `x` is known to this deployment",
        "no session named `x`",
        "unknown session `x`",
        "NO SUCH SESSION",
    ] {
        assert!(looks_missing(message), "should read as missing: {message}");
    }
}

#[test]
fn looks_missing_rejects_ambiguity_and_generic_failures() {
    for message in [
        "",
        "error",
        "failed",
        "invalid arguments",
        "a title cannot be blank",
        // The trap: an absent *argument*, not an absent session.
        "no session id given",
        "`sessionId` is required",
        "session id `../x` contains a path separator",
        "connection reset by peer",
        "permission denied",
    ] {
        assert!(
            !looks_missing(message),
            "should not read as missing: {message}"
        );
    }
}

#[test]
fn looks_missing_is_case_insensitive() {
    assert!(looks_missing("NOT FOUND"));
    assert!(looks_missing("No Such Session"));
}

// ---- `tag` is an API --------------------------------------------------------------------------

#[test]
fn tag_strings_are_stable() {
    // A client branches on these exact strings. Changing one is a breaking change, so they are
    // asserted literally rather than by shape.
    let categories = [
        (
            Category::NotFound {
                what: "session".to_owned(),
                id: "x".to_owned(),
            },
            "NOT_FOUND",
        ),
        (
            Category::InvalidArguments {
                detail: "d".to_owned(),
            },
            "INVALID_ARGUMENTS",
        ),
        (
            Category::UnknownMethod {
                namespace: "session".to_owned(),
                method: "nope".to_owned(),
            },
            "UNKNOWN_METHOD",
        ),
        (
            Category::Refused {
                rule: "r".to_owned(),
            },
            "REFUSED",
        ),
        (
            Category::Failed {
                detail: "d".to_owned(),
            },
            "FAILED",
        ),
    ];

    for (category, expected) in categories {
        assert_eq!(tag(&category), expected);
    }
}

#[test]
fn a_successful_removal_has_no_category() {
    // Nothing went wrong, so there is nothing to classify: an `Option` keeps the caller from
    // inventing a "success" tag that a client would then have to special-case.
    assert_eq!(categorize_removal("session", "x", true), None);
}

#[test]
fn a_satisfied_rename_is_not_treated_as_missing() {
    // The counterpart of the rename test: the same method, a session that exists, and therefore no
    // absence to classify.
    let error = RemoteError::Failed("disk full".to_owned());
    let category = categorize("session", "rename", &error);
    assert_eq!(missing_id(&category), None);
}

#[test]
fn a_missing_resource_names_the_namespace_as_what_was_missing() {
    let category = categorize(
        "session",
        "state",
        &RemoteError::Failed("no session `x`".to_owned()),
    );
    let Category::NotFound { what, .. } = category else {
        panic!("expected an absence");
    };
    assert_eq!(what, "session");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod util {
    pub use super::error_taxonomy as error_taxonomy;
}
