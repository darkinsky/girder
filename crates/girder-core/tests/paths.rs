//! Tests for user-data roots (M1 of the v2 plan).
//!
//! Precedence is explicit-then-environment-then-default, and the interesting cases are the ones
//! upstream learned the hard way: a blank environment override, a `~` that must expand against a
//! supplied home rather than the process's, and a path that tries to escape its root.

use girder_core::{
    AGENTS_HOME_DISPLAY, HARNESS_HOME_DISPLAY, RootEnvironment, RootError, abbreviate_home,
    agents_home, expand_home, harness_home, join_within,
};
use std::path::{Path, PathBuf};

fn env() -> RootEnvironment {
    RootEnvironment::default().with_home("/home/user")
}

#[test]
fn the_default_root_is_under_the_home() {
    let root = harness_home(None, &env()).expect("home is known");

    assert_eq!(root.path, PathBuf::from("/home/user/.girder"));
    assert_eq!(root.display, HARNESS_HOME_DISPLAY);
}

#[test]
fn an_explicit_path_wins_over_the_environment() {
    let environment = env().with_var("GIRDER_HOME", "/from/env");
    let root = harness_home(Some("/explicit"), &environment).expect("resolved");

    assert_eq!(root.path, PathBuf::from("/explicit"));
    assert_eq!(root.display, "$GIRDER_HOME");
}

#[test]
fn the_environment_wins_over_the_default() {
    let environment = env().with_var("GIRDER_HOME", "/from/env");
    let root = harness_home(None, &environment).expect("resolved");

    assert_eq!(root.path, PathBuf::from("/from/env"));
    assert_eq!(
        root.display, "$GIRDER_HOME",
        "a configured root is labelled by its variable"
    );
}

#[test]
fn a_blank_environment_value_counts_as_unset() {
    // Upstream's lesson: a blank override must not resolve the root to the working directory.
    let environment = env().with_var("GIRDER_HOME", "   ");
    let root = harness_home(None, &environment).expect("resolved");

    assert_eq!(root.path, PathBuf::from("/home/user/.girder"));
    assert_eq!(root.display, HARNESS_HOME_DISPLAY);
}

#[test]
fn a_blank_explicit_path_falls_through() {
    let environment = env().with_var("GIRDER_HOME", "/from/env");
    let root = harness_home(Some("  "), &environment).expect("resolved");

    assert_eq!(
        root.path,
        PathBuf::from("/from/env"),
        "blank explicit is not a choice"
    );
}

#[test]
fn a_tilde_expands_against_the_supplied_home() {
    // Not against the process's home: a test must be able to pin it, and a caller may supply a
    // different one.
    let root = harness_home(Some("~/custom"), &env()).expect("resolved");
    assert_eq!(root.path, PathBuf::from("/home/user/custom"));

    let bare = harness_home(Some("~"), &env()).expect("resolved");
    assert_eq!(bare.path, PathBuf::from("/home/user"));
}

#[test]
fn a_relative_path_is_made_absolute() {
    // A relative configured root would otherwise depend on the process's working directory.
    let root = harness_home(Some("relative/dir"), &env()).expect("resolved");

    assert!(root.path.is_absolute(), "{}", root.path.display());
    assert!(root.path.ends_with("relative/dir"));
}

#[test]
fn an_unknown_home_is_an_error_not_a_fallback() {
    // A harness that cannot find the home must fail loudly: the alternative scatters state into
    // whatever directory it started in.
    let error = harness_home(None, &RootEnvironment::default()).expect_err("no home");

    assert!(matches!(error, RootError::NoHome { .. }));
    assert!(
        error.to_string().contains("GIRDER_HOME"),
        "the message should name the override"
    );
}

#[test]
fn the_agents_home_uses_its_own_default_and_variable() {
    let root = agents_home(None, &env()).expect("resolved");
    assert_eq!(root.path, PathBuf::from("/home/user/.agents"));
    assert_eq!(root.display, AGENTS_HOME_DISPLAY);

    let overridden = agents_home(None, &env().with_var("GIRDER_AGENTS_HOME", "/agents")).unwrap();
    assert_eq!(overridden.path, PathBuf::from("/agents"));
    assert_eq!(overridden.display, "$GIRDER_AGENTS_HOME");
}

#[test]
fn expand_home_handles_the_supported_prefixes() {
    // The function reads the process home, so the assertions are relative to it.
    let Some(home) = girder_core::home_dir() else {
        return;
    };
    assert_eq!(expand_home("~"), home);
    assert_eq!(expand_home("~/x/y"), home.join("x/y"));
    // No supported prefix: unchanged, rather than invented.
    assert_eq!(
        expand_home("/absolute/path"),
        PathBuf::from("/absolute/path")
    );
    assert_eq!(expand_home("relative"), PathBuf::from("relative"));
    // A tilde in the middle is not a prefix.
    assert_eq!(expand_home("a/~/b"), PathBuf::from("a/~/b"));
}

#[test]
fn abbreviate_replaces_the_home_with_a_tilde() {
    let home = Path::new("/home/user");

    assert_eq!(
        abbreviate_home(Path::new("/home/user/.girder/x"), Some(home)),
        "~/.girder/x"
    );
    assert_eq!(abbreviate_home(home, Some(home)), "~");
    assert_eq!(
        abbreviate_home(Path::new("/elsewhere/x"), Some(home)),
        "/elsewhere/x",
        "a path outside the home is unchanged, not made relative to something arbitrary"
    );
    assert_eq!(abbreviate_home(Path::new("/x"), None), "/x");
}

#[test]
fn a_path_under_the_root_is_accepted() {
    let root = Path::new("/data/workspace");
    assert_eq!(
        join_within(root, "a/b.txt").unwrap(),
        PathBuf::from("/data/workspace/a/b.txt")
    );
    // A `..` that stays inside is fine.
    assert_eq!(
        join_within(root, "a/../b.txt").unwrap(),
        PathBuf::from("/data/workspace/b.txt")
    );
}

#[test]
fn a_path_escaping_the_root_is_refused() {
    // A tool that takes a path from a model must not be talked into ../../etc/passwd.
    let root = Path::new("/data/workspace");

    for attempt in ["../escape", "a/../../escape", "../../../etc/passwd"] {
        let error = join_within(root, attempt).expect_err(attempt);
        assert!(matches!(error, RootError::Escapes { .. }), "{attempt}");
    }
}

#[test]
fn an_absolute_path_cannot_be_used_to_escape() {
    // `root.join("/etc/passwd")` yields the absolute path in Rust, which would silently leave the
    // root; the check must catch it.
    let root = Path::new("/data/workspace");
    let error = join_within(root, "/etc/passwd").expect_err("absolute path escapes");

    assert!(matches!(error, RootError::Escapes { .. }));
}
