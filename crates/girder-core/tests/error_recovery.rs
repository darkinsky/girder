//! Tests for the recovery-suggestion table.
//!
//! Two properties matter more than the wording of any single suggestion. The first is that a
//! permission failure *escalates*: the interesting claim of the module is that some failures have
//! no local fix, and a table that answered "retry with a different path" there would send a model
//! round a loop it cannot exit. The second is that an unrecognised error produces *nothing*: a
//! fabricated hint is worse than silence, because the model cannot tell an inference from an
//! observation.
//!
//! The rest of the cases pin the mapping the module documents — which fault gets which kind, and
//! that the tool name changes the advice rather than merely decorating it.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the policy as written even before the crate root wires it in. It resolves `girder_core::…`
// through the alias the library declares for itself, which is why the same source builds here.
#[path = "../src/error_recovery.rs"]
mod error_recovery;

use error_recovery::{Kind, Suggestion, suggest};

/// Collects the kinds of every action, in order.
fn kinds(suggestion: &Suggestion) -> Vec<Kind> {
    suggestion
        .actions
        .iter()
        .map(|action| action.kind)
        .collect()
}

/// Whether any action has `kind`.
fn has(suggestion: &Suggestion, kind: Kind) -> bool {
    suggestion.actions.iter().any(|action| action.kind == kind)
}

#[test]
fn missing_file_is_investigated_before_it_is_retried() {
    // The raw `std::io::Error` text this crate's file tools forward verbatim.
    let suggestion = suggest(
        "read",
        "cannot read `src/nope.rs`: No such file or directory (os error 2)",
    );

    assert_eq!(
        suggestion.actions.first().map(|action| action.kind),
        Some(Kind::Investigate),
        "a missing file must be looked for before any second guess: {:?}",
        suggestion.actions
    );
    // The look must name a *different* tool, or it is just the failed call again.
    let first = &suggestion.actions.first().expect("an action").describe;
    assert!(first.contains("glob"), "{first}");
    assert!(
        !suggestion
            .actions
            .iter()
            .any(|action| action.describe.starts_with("Run `read`"))
    );
}

#[test]
fn missing_file_also_says_what_to_do_when_it_is_not_found() {
    // A `not found` that the search confirms is an escalation, not a licence to create the file.
    let suggestion = suggest("read", "`docs/guide.md` does not exist");
    assert!(has(&suggestion, Kind::Escalate), "{:?}", suggestion.actions);
    assert!(
        has(&suggestion, Kind::Investigate),
        "{:?}",
        suggestion.actions
    );
}

#[test]
fn permission_denied_escalates_and_never_suggests_a_retry() {
    // The core claim: permission is not the model's to grant, so every move is `Escalate`.
    for error in [
        "Permission denied (os error 13)",
        "cannot write `Cargo.lock`: permission denied",
        "Operation not permitted",
        "read-only file system",
        "approval rejected by the user",
    ] {
        let suggestion = suggest("write", error);
        assert!(
            !suggestion.actions.is_empty(),
            "`{error}` must produce advice"
        );
        assert!(
            suggestion
                .actions
                .iter()
                .all(|action| action.kind == Kind::Escalate),
            "`{error}` must escalate only, got {:?}",
            kinds(&suggestion)
        );
        // No action may invite another tool call against the refused path.
        for action in &suggestion.actions {
            let text = action.describe.to_lowercase();
            assert!(
                !text.contains("call `write` again") && !text.contains("retry"),
                "`{error}` suggested a retry: {text}"
            );
        }
    }
}

#[test]
fn permission_denied_is_not_mistaken_for_a_missing_file() {
    // `Operation not permitted` and `No such file` share no branch, but both mention the path: a
    // table that classified on the path rather than the cause would confuse them.
    let denied = suggest("read", "`/root/.ssh/id_rsa`: Permission denied");
    assert!(
        denied
            .actions
            .iter()
            .all(|action| action.kind == Kind::Escalate),
        "{:?}",
        kinds(&denied)
    );

    let missing = suggest("read", "`/root/.ssh/id_rsa`: No such file or directory");
    assert!(has(&missing, Kind::Investigate), "{:?}", kinds(&missing));
    assert_eq!(kinds(&denied), {
        let mut expected = kinds(&denied).clone();
        expected.dedup();
        expected
    });
}

#[test]
fn a_directory_given_as_a_file_switches_tools() {
    let suggestion = suggest("read", "`src/session` is a directory, not a file");
    assert!(
        has(&suggestion, Kind::UseOtherTool),
        "{:?}",
        suggestion.actions
    );
    let other = suggestion
        .actions
        .iter()
        .find(|action| action.kind == Kind::UseOtherTool)
        .expect("a tool switch");
    assert!(other.describe.contains("glob"), "{}", other.describe);
}

#[test]
fn is_a_directory_raw_errno_also_switches_tools() {
    let suggestion = suggest("read", "cannot read `target`: Is a directory (os error 21)");
    assert!(
        has(&suggestion, Kind::UseOtherTool),
        "{:?}",
        suggestion.actions
    );
}

#[test]
fn a_missing_directory_component_is_investigated() {
    let suggestion = suggest("write", "`a/b/c.rs`: Not a directory (os error 20)");
    assert!(
        has(&suggestion, Kind::Investigate),
        "{:?}",
        suggestion.actions
    );
    assert!(
        has(&suggestion, Kind::ChangeArgument),
        "{:?}",
        suggestion.actions
    );
}

#[test]
fn unknown_errors_suggest_nothing() {
    // The core claim about honesty: the module may not invent a recovery for text it cannot place.
    for error in [
        "segmentation fault",
        "the model returned an empty completion",
        "internal error: assertion failed",
        "flurbulator desynchronised",
        "",
        "   ",
    ] {
        let suggestion = suggest("bash", error);
        assert!(
            suggestion.actions.is_empty(),
            "`{error}` produced fabricated advice: {:?}",
            suggestion.actions
        );
        // The problem text is preserved rather than paraphrased.
        assert_eq!(suggestion.problem, error.trim(), "for `{error}`");
    }
}

#[test]
fn unknown_error_still_names_the_problem_it_was_given() {
    let suggestion = suggest("bash", "  kubelet said no  ");
    assert!(suggestion.actions.is_empty());
    assert_eq!(suggestion.problem, "kubelet said no");
}

#[test]
fn every_suggestion_has_a_non_empty_description() {
    // Every recognised failure, so a new branch with an empty string is caught here rather than by
    // a model reading a blank instruction.
    let errors = [
        "No such file or directory",
        "Permission denied",
        "Is a directory",
        "Not a directory",
        "not valid UTF-8",
        "too large",
        "changed since it was read",
        "`path` is required",
        "command not found",
        "timed out",
        "no space left on device",
        "ambiguous match: more than one match",
    ];
    let tools = [
        "read",
        "write",
        "edit",
        "glob",
        "grep",
        "bash",
        "apply_patch",
        "",
    ];

    for tool in tools {
        for error in errors {
            let suggestion = suggest(tool, error);
            assert!(
                !suggestion.actions.is_empty(),
                "`{tool}` / `{error}` produced no advice"
            );
            assert!(
                !suggestion.problem.trim().is_empty(),
                "`{tool}` / `{error}` has an empty problem"
            );
            for action in &suggestion.actions {
                assert!(
                    !action.describe.trim().is_empty(),
                    "`{tool}` / `{error}` has an empty action"
                );
                assert!(
                    action.describe.contains(' '),
                    "`{tool}` / `{error}` action is not a sentence: {}",
                    action.describe
                );
            }
        }
    }
}

#[test]
fn the_tool_name_changes_the_advice() {
    // A `glob` that cannot find its start directory is not fixed by running `glob` again; the
    // recommendation must name a different tool than the one that failed.
    let read = suggest("read", "No such file or directory");
    let glob = suggest("glob", "No such file or directory");
    assert_ne!(read, glob, "the tool must affect the suggestion");

    let read_first = &read.actions.first().expect("an action").describe;
    let glob_first = &glob.actions.first().expect("an action").describe;
    assert!(read_first.contains("`glob`"), "{read_first}");
    assert!(
        !glob_first.contains("`glob`"),
        "a failed `glob` must not be told to run `glob` again: {glob_first}"
    );
    assert!(glob_first.contains("`bash`"), "{glob_first}");
}

#[test]
fn the_tool_name_appears_in_the_problem() {
    let suggestion = suggest("apply_patch", "Permission denied");
    assert!(
        suggestion.problem.contains("apply_patch"),
        "{}",
        suggestion.problem
    );
}

#[test]
fn an_unnamed_tool_still_reads_as_a_sentence() {
    // An empty name must not produce "`` is missing", which reads as a typo.
    for tool in ["", "   "] {
        let suggestion = suggest(tool, "No such file or directory");
        assert!(!suggestion.problem.contains("``"), "{}", suggestion.problem);
        assert!(
            suggestion.problem.starts_with("the tool"),
            "{}",
            suggestion.problem
        );

        let denied = suggest(tool, "Permission denied");
        assert!(denied.problem.contains("the tool"), "{}", denied.problem);
    }
}

#[test]
fn empty_input_is_handled_without_panicking() {
    let suggestion = suggest("", "");
    assert!(suggestion.actions.is_empty());
    assert!(suggestion.problem.is_empty());

    // Whitespace around a known error is tolerated, because tools indent their messages.
    let padded = suggest("read", "  \n  No such file or directory\n");
    assert!(has(&padded, Kind::Investigate), "{:?}", padded.actions);
}

#[test]
fn matching_is_case_insensitive() {
    for error in [
        "no such file or directory",
        "NO SUCH FILE OR DIRECTORY",
        "No Such File Or Directory",
    ] {
        let suggestion = suggest("read", error);
        assert!(has(&suggestion, Kind::Investigate), "`{error}` not matched");
    }
}

#[test]
fn stale_write_is_a_reread_not_a_force() {
    let suggestion = suggest(
        "write",
        "`src/main.rs` changed since it was read; re-read it before writing",
    );
    assert_eq!(
        suggestion.actions.first().map(|action| action.kind),
        Some(Kind::Investigate),
        "{:?}",
        suggestion.actions
    );
    let first = &suggestion.actions.first().expect("an action").describe;
    assert!(first.contains("read"), "{first}");
}

#[test]
fn a_large_file_is_read_by_range() {
    let suggestion = suggest(
        "read",
        "`logs/big.log` is 73400320 bytes, over the 524288-byte limit",
    );
    assert!(
        has(&suggestion, Kind::ChangeArgument),
        "{:?}",
        suggestion.actions
    );
    let change = suggestion
        .actions
        .iter()
        .find(|action| action.kind == Kind::ChangeArgument)
        .expect("a range suggestion");
    assert!(change.describe.contains("offset"), "{}", change.describe);
    assert!(change.describe.contains("limit"), "{}", change.describe);
}

#[test]
fn a_missing_argument_is_a_resend_not_an_escalation() {
    let suggestion = suggest("read", "`path` is required");
    assert_eq!(
        kinds(&suggestion),
        vec![Kind::ChangeArgument, Kind::Investigate]
    );
    assert!(
        !has(&suggestion, Kind::Escalate),
        "{:?}",
        suggestion.actions
    );
}

#[test]
fn a_missing_program_names_the_lookup() {
    let suggestion = suggest("bash", "bash: line 1: rg: command not found");
    assert!(
        has(&suggestion, Kind::Investigate),
        "{:?}",
        suggestion.actions
    );
    assert!(has(&suggestion, Kind::Escalate), "{:?}", suggestion.actions);
}

#[test]
fn a_timeout_narrows_rather_than_repeating() {
    let suggestion = suggest("bash", "the command timed out after 120000ms");
    assert!(
        suggestion
            .actions
            .iter()
            .all(|action| action.kind != Kind::Escalate)
    );
    assert!(
        suggestion
            .actions
            .iter()
            .all(|action| !action.describe.to_lowercase().contains("retry the same")),
        "{:?}",
        suggestion.actions
    );
}

#[test]
fn a_full_device_escalates() {
    let suggestion = suggest("write", "No space left on device (os error 28)");
    assert!(has(&suggestion, Kind::Escalate), "{:?}", suggestion.actions);
    assert!(
        !has(&suggestion, Kind::ChangeArgument),
        "{:?}",
        suggestion.actions
    );
}

#[test]
fn suggestions_are_ordered_by_how_cheap_they_are() {
    // Investigate first, escalate last: the cheap look comes before the expensive decision.
    let suggestion = suggest("read", "No such file or directory");
    let order = kinds(&suggestion);
    let investigate = order.iter().position(|kind| *kind == Kind::Investigate);
    let escalate = order.iter().position(|kind| *kind == Kind::Escalate);
    assert!(investigate < escalate, "{order:?}");
}

#[test]
fn the_same_input_suggests_the_same_thing() {
    // A pure function: no clock, no environment, so a caller may cache or snapshot the answer.
    let first = suggest("read", "Permission denied");
    let second = suggest("read", "Permission denied");
    assert_eq!(first, second);
}
