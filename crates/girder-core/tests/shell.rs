//! Tests for the shell seam.
//!
//! The two properties worth pinning are about leakage and defaults: a child must not inherit the
//! harness's own secrets, and a resolved specification must record what actually ran.

use std::collections::BTreeMap;
use std::time::Duration;

use girder_core::{
    DEFAULT_MAX_BYTES, DEFAULT_TIMEOUT, ENV_PREFIX, Program, SandboxMode, ShellEnvironment,
    ShellRequest, TextStrategy, split_words, which,
};

// ---- the request ------------------------------------------------------------------------------

#[test]
fn a_request_defaults_to_a_posix_shell() {
    let spec = ShellRequest::new("echo hi").resolve(SandboxMode::WorkspaceWrite);

    assert_eq!(
        spec.program,
        Program::Posix {
            shell: "sh".to_owned()
        },
        "sh exists on every POSIX system; bash-isms would fail on a minimal image"
    );
}

#[test]
fn an_omitted_timeout_resolves_to_the_default() {
    let spec = ShellRequest::new("sleep 1").resolve(SandboxMode::Full);

    assert_eq!(spec.timeout, DEFAULT_TIMEOUT);
}

#[test]
fn an_explicit_timeout_survives() {
    let spec = ShellRequest::new("x")
        .with_timeout(Duration::from_secs(5))
        .resolve(SandboxMode::Full);

    assert_eq!(spec.timeout, Duration::from_secs(5));
}

#[test]
fn the_default_retention_keeps_both_ends() {
    // A shell tool wants the command's echo at the start and its result at the end; the middle is what a
    // reader skips.
    let spec = ShellRequest::new("x").resolve(SandboxMode::Full);

    assert_eq!(
        spec.retention,
        TextStrategy::HeadTail {
            head_bytes: DEFAULT_MAX_BYTES / 2,
            tail_bytes: DEFAULT_MAX_BYTES / 2,
        }
    );
}

#[test]
fn a_directory_is_carried_into_the_specification() {
    let spec = ShellRequest::new("x")
        .in_directory("/tmp")
        .resolve(SandboxMode::Full);

    assert_eq!(spec.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
}

#[test]
fn the_sandbox_mode_is_recorded() {
    for mode in [
        SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite,
        SandboxMode::Full,
    ] {
        let spec = ShellRequest::new("x").resolve(mode);
        assert_eq!(spec.sandbox, mode);
    }
}

#[test]
fn the_sandbox_modes_have_wire_names() {
    assert_eq!(SandboxMode::ReadOnly.as_str(), "read-only");
    assert_eq!(SandboxMode::WorkspaceWrite.as_str(), "workspace-write");
    assert_eq!(SandboxMode::Full.as_str(), "danger-full-access");
}

// ---- the environment --------------------------------------------------------------------------

#[test]
fn the_baseline_passes_only_what_a_command_needs() {
    // Deliberately short: a child inherits its parent's environment, which includes whatever secrets the
    // parent was started with.
    let environment = ShellEnvironment::baseline();

    assert!(environment.is_passed("PATH"));
    assert!(environment.is_passed("HOME"));
    assert!(!environment.is_passed("DEEPSEEK_API_KEY"));
    assert!(!environment.is_passed("AWS_SECRET_ACCESS_KEY"));
}

#[test]
fn an_empty_environment_contributes_nothing() {
    let environment = ShellEnvironment::empty();
    let parent = BTreeMap::from([("PATH".to_owned(), "/bin".to_owned())]);

    assert!(environment.resolve(&parent).is_empty());
}

#[test]
fn a_passed_variable_resolves_from_the_parent() {
    let environment = ShellEnvironment::empty().pass("PATH");
    let parent = BTreeMap::from([("PATH".to_owned(), "/usr/bin".to_owned())]);

    let resolved = environment.resolve(&parent);

    assert_eq!(resolved.get("PATH").map(String::as_str), Some("/usr/bin"));
}

#[test]
fn a_passed_variable_the_parent_lacks_contributes_nothing() {
    // An empty value would be indistinguishable from a variable the parent did not set, and a command
    // that checked for presence would get the wrong answer.
    let environment = ShellEnvironment::empty().pass("MISSING");

    assert!(
        !environment
            .resolve(&BTreeMap::new())
            .contains_key("MISSING")
    );
}

#[test]
fn an_explicit_value_wins_over_the_parent() {
    let environment = ShellEnvironment::empty()
        .pass("PATH")
        .set("PATH", "/custom");
    let parent = BTreeMap::from([("PATH".to_owned(), "/usr/bin".to_owned())]);

    assert_eq!(
        environment.resolve(&parent).get("PATH").map(String::as_str),
        Some("/custom")
    );
}

#[test]
fn the_environment_prefix_is_an_environment_name() {
    assert!(ENV_PREFIX.ends_with('_'));
    assert!(
        ENV_PREFIX
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_')
    );
}

// ---- programs ---------------------------------------------------------------------------------

#[test]
fn a_direct_program_carries_its_arguments() {
    let program = Program::direct("echo", vec!["hi".to_owned()]);

    let Program::Direct { program, args } = program else {
        panic!("expected a direct program");
    };
    assert_eq!(program, "echo");
    assert_eq!(args, vec!["hi"]);
}

// ---- word splitting ---------------------------------------------------------------------------

#[test]
fn a_simple_command_splits_on_whitespace() {
    assert_eq!(split_words("echo hi"), vec!["echo", "hi"]);
    assert_eq!(split_words("  echo   hi  "), vec!["echo", "hi"]);
}

#[test]
fn a_quoted_word_stays_one_word() {
    assert_eq!(
        split_words(r#"echo "hello world""#),
        vec!["echo", "hello world"]
    );
    assert_eq!(
        split_words("echo 'hello world'"),
        vec!["echo", "hello world"]
    );
}

#[test]
fn an_escaped_space_does_not_split() {
    assert_eq!(split_words(r"echo a\ b"), vec!["echo", "a b"]);
}

#[test]
fn an_empty_command_splits_to_nothing() {
    assert!(split_words("").is_empty());
    assert!(split_words("   ").is_empty());
}

#[test]
fn a_shell_operator_stays_inside_its_word() {
    // Not a shell parser: `|` and `>` are ordinary characters here, which is exactly why a caller with a
    // compound command must fall back to the POSIX shell instead of guessing.
    assert_eq!(
        split_words("echo a | grep b"),
        vec!["echo", "a", "|", "grep", "b"]
    );
    assert_eq!(split_words("echo a>b"), vec!["echo", "a>b"]);
}

#[test]
fn a_single_quote_keeps_a_backslash_literal() {
    // Inside single quotes a backslash is not an escape, which is a real difference from double quotes.
    assert_eq!(split_words(r"echo 'a\b'"), vec!["echo", r"a\b"]);
}

// ---- finding a program ------------------------------------------------------------------------

#[test]
fn a_present_program_is_found() {
    assert!(which("sh").is_some());
}

#[test]
fn a_missing_program_is_not_found() {
    assert!(which("girder-no-such-program-xyz").is_none());
}

#[test]
fn an_absolute_path_is_checked_directly() {
    assert!(which("/bin/sh").is_some() || which("/usr/bin/sh").is_some());
    assert!(which("/nonexistent/program").is_none());
}
