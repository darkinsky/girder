//! Tests for the shell tool's result contract: rendering, marker parsing, and sandbox
//! classification.
//!
//! The properties pinned here are the ones the upstream suites pin, transcribed from
//! `packages/shell/shell/tests/render.spec.ts` and `packages/shell/tool-bash/tests/tools.spec.ts`:
//!
//! - **Round-trip.** Every status [`render_result`] can emit must be recoverable by
//!   [`parse_exit_status`], because replay keeps the rendered text and nothing else.
//! - **Marker boundaries.** A marker needs a leading newline and the end of the text, so output that
//!   merely ends in marker-like text is output. This is the case a naive `ends_with` gets wrong.
//! - **First-cause facts.** A timeout, an outside stop, and a signal death are separate facts that
//!   compose into separate markers, and a nonzero exit is reported rather than errored.
//! - **Classification.** A denial needs a nonzero exit *and* a dialect match; a runner failure needs a
//!   fatal line that survives informational exclusion. Exit status alone proves neither.
//!
//! Included by path rather than through the crate's re-exports, so these tests compile against the
//! module as written even before the crate root wires it in. That is why the module under test reaches
//! [`SandboxMode`] through `girder_core::` rather than `crate::`: under `#[path]` inclusion, `crate::`
//! names *this test binary*, which has no such item.

#[path = "../src/shell/render.rs"]
pub mod shell_gap;

use girder_core::SandboxMode;
use shell_gap::{
    Channel, Chunk, CollectedOutput, ParsedExit, RunOutcome, RunnerFailureRule, SandboxEnforcement,
    ShellSandboxInfo, classify_denial, classify_runner_failure, escalation_hint_marker,
    matches_signature, parse_exit_status, render_job_read, render_promoted, render_result,
    ring_delta, runner_failure_notice, sandbox_denial_marker, stream_text,
};

/// A clean run with the given output.
fn clean(stdout: &str, stderr: &str) -> RunOutcome {
    RunOutcome {
        exit_code: Some(0),
        stdout: CollectedOutput::complete(stdout),
        stderr: CollectedOutput::complete(stderr),
        ..RunOutcome::default()
    }
}

/// A run that exited non-zero.
fn failed(code: i32, stdout: &str, stderr: &str) -> RunOutcome {
    RunOutcome {
        exit_code: Some(code),
        stdout: CollectedOutput::complete(stdout),
        stderr: CollectedOutput::complete(stderr),
        ..RunOutcome::default()
    }
}

// ---- rendering: the body ---------------------------------------------------------------

#[test]
fn silent_command_renders_the_no_output_placeholder() {
    assert_eq!(render_result(&clean("", ""), &[]), "(no output)");
}

#[test]
fn stderr_becomes_a_marked_section_after_stdout() {
    // stdout already ends with a newline, so exactly one separates the sections.
    assert_eq!(
        render_result(&clean("out\n", "err\n"), &[]),
        "out\n[stderr]\nerr\n"
    );
}

#[test]
fn a_missing_newline_before_the_stderr_section_is_supplied() {
    assert_eq!(
        render_result(&clean("out", "err"), &[]),
        "out\n[stderr]\nerr"
    );
}

#[test]
fn stderr_alone_still_gets_its_section_marker() {
    assert_eq!(render_result(&clean("", "err\n"), &[]), "[stderr]\nerr\n");
}

#[test]
fn a_clean_exit_adds_no_marker() {
    assert_eq!(render_result(&clean("hi\n", ""), &[]), "hi\n");
}

#[test]
fn truncation_names_the_spill_file_or_says_unavailable() {
    let mut bounded = CollectedOutput::truncated_tail("tail\n", Some("/spill/out.log".to_owned()));
    assert_eq!(
        stream_text(&bounded),
        "tail\n\n[output truncated; full output: /spill/out.log]"
    );
    bounded.spill_path = None;
    assert_eq!(
        stream_text(&bounded),
        "tail\n\n[output truncated; full output: (unavailable)]"
    );
}

#[test]
fn a_truncated_stream_renders_its_notice_inside_the_result() {
    let mut outcome = clean("tail\n", "");
    outcome.stdout = CollectedOutput::truncated_tail("tail\n", Some("/spill/out.log".to_owned()));
    assert_eq!(
        render_result(&outcome, &[]),
        "tail\n\n[output truncated; full output: /spill/out.log]"
    );
}

// ---- rendering: the markers ------------------------------------------------------------

#[test]
fn a_nonzero_exit_is_reported_as_a_marker_not_an_error() {
    assert_eq!(
        render_result(&failed(3, "oops\n", ""), &[]),
        "oops\n[exit code: 3]"
    );
}

#[test]
fn a_timeout_after_a_trapped_sigterm_still_reports_the_interruption() {
    // The process exited 0 because it trapped SIGTERM, but the deadline is what cut it short.
    let outcome = RunOutcome {
        timed_out: true,
        timeout_ms: 100,
        ..clean("", "")
    };
    assert_eq!(
        render_result(&outcome, &[]),
        "(no output)\n[timed out after 100ms]"
    );
}

#[test]
fn a_signal_death_reports_the_signal_instead_of_an_exit_code() {
    let outcome = RunOutcome {
        exit_code: None,
        signal: Some("SIGKILL".to_owned()),
        ..clean("", "")
    };
    assert_eq!(
        render_result(&outcome, &[]),
        "(no output)\n[killed by signal: SIGKILL]"
    );
}

#[test]
fn timeout_and_signal_compose_in_that_order() {
    let outcome = RunOutcome {
        exit_code: None,
        signal: Some("SIGTERM".to_owned()),
        timed_out: true,
        timeout_ms: 100,
        ..clean("", "")
    };
    assert_eq!(
        render_result(&outcome, &[]),
        "(no output)\n[timed out after 100ms]\n[killed by signal: SIGTERM]"
    );
}

#[test]
fn an_outside_stop_explains_itself_before_the_signal() {
    let outcome = RunOutcome {
        exit_code: None,
        signal: Some("SIGTERM".to_owned()),
        stopped: Some("cancelled by the user".to_owned()),
        ..clean("", "")
    };
    assert_eq!(
        render_result(&outcome, &[]),
        "(no output)\n[stopped: cancelled by the user]\n[killed by signal: SIGTERM]"
    );
}

#[test]
fn preparation_expiry_renders_the_literal_null_exit_code() {
    // No process ever existed, so there is neither a code nor a signal — and upstream renders `null`.
    let outcome = RunOutcome {
        exit_code: None,
        timed_out: true,
        timeout_ms: 100,
        ..clean("", "")
    };
    assert_eq!(
        render_result(&outcome, &[]),
        "(no output)\n[timed out after 100ms]\n[exit code: null]"
    );
}

#[test]
fn a_denial_marker_names_the_mode_and_the_hint_follows_only_when_advertised() {
    let outcome = RunOutcome {
        exit_code: Some(1),
        sandbox: Some(ShellSandboxInfo::denied(SandboxMode::ReadOnly)),
        ..clean("", "touch: cannot touch 'x': Read-only file system\n")
    };
    let denied = render_result(&outcome, &[]);
    assert!(denied.contains("[sandbox: file access denied under read-only mode]"));
    assert!(!denied.contains("escalation available"));
    // The exit marker stays last, because the parse anchors there.
    assert!(denied.ends_with("[exit code: 1]"));

    let escalated = render_result(&outcome, &[SandboxMode::WorkspaceWrite]);
    assert!(escalated.contains("escalation available — retry this exact command once"));
    assert!(escalated.ends_with("[exit code: 1]"));
}

#[test]
fn the_marker_spellings_match_the_upstream_contract() {
    assert_eq!(
        sandbox_denial_marker(SandboxMode::WorkspaceWrite),
        "[sandbox: file access denied under workspace-write mode]"
    );
    assert_eq!(
        sandbox_denial_marker(SandboxMode::Full),
        "[sandbox: file access denied under danger-full-access mode]"
    );
    assert!(escalation_hint_marker("command").contains("sandbox_permissions"));
    assert!(
        runner_failure_notice(SandboxMode::ReadOnly)
            .contains("the sandbox runner itself failed under read-only mode")
    );
}

// ---- the marker grammar: parsing -------------------------------------------------------

#[test]
fn a_clean_result_parses_with_its_body_verbatim() {
    let parsed = parse_exit_status("hi\n\n");
    assert_eq!(parsed.body, "hi\n\n");
    assert_eq!(parsed.exit, ParsedExit::Code(0));

    let empty = parse_exit_status("");
    assert_eq!(empty.body, "");
    assert_eq!(empty.exit, ParsedExit::Code(0));
}

#[test]
fn a_nonzero_marker_is_stripped_from_the_body() {
    let parsed = parse_exit_status("oops\n[exit code: 3]");
    assert_eq!(parsed.body, "oops");
    assert_eq!(parsed.exit, ParsedExit::Code(3));
}

#[test]
fn marker_like_output_without_a_leading_newline_is_output() {
    // The whole body is one line, so there is no marker to consume.
    let parsed = parse_exit_status("[exit code: 5]");
    assert_eq!(parsed.body, "[exit code: 5]");
    assert_eq!(parsed.exit, ParsedExit::Code(0));

    let signal = parse_exit_status("[killed by signal: SIGKILL]");
    assert_eq!(signal.body, "[killed by signal: SIGKILL]");
    assert_eq!(signal.exit, ParsedExit::Code(0));
}

#[test]
fn a_signal_marker_wins_over_any_nonzero_marker() {
    let parsed = parse_exit_status("gone\n[killed by signal: SIGKILL]");
    assert_eq!(parsed.body, "gone");
    assert_eq!(parsed.exit, ParsedExit::Signal("SIGKILL".to_owned()));
}

#[test]
fn markers_no_pill_shows_stay_in_the_body() {
    let parsed = parse_exit_status("slow\n[timed out after 100ms]\n[exit code: 143]");
    assert_eq!(parsed.body, "slow\n[timed out after 100ms]");
    assert_eq!(parsed.exit, ParsedExit::Code(143));
}

#[test]
fn a_marker_that_does_not_end_the_text_is_output() {
    let parsed = parse_exit_status("a\n[exit code: 3]\nmore");
    assert_eq!(parsed.body, "a\n[exit code: 3]\nmore");
    assert_eq!(parsed.exit, ParsedExit::Code(0));
}

#[test]
fn a_marker_with_no_digits_is_not_a_marker() {
    let parsed = parse_exit_status("x\n[exit code: ]");
    assert_eq!(parsed.body, "x\n[exit code: ]");
    assert_eq!(parsed.exit, ParsedExit::Code(0));
}

#[test]
fn an_exit_code_too_large_for_i32_is_output() {
    // The renderer cannot produce this marker, so it cannot be one.
    let parsed = parse_exit_status("x\n[exit code: 99999999999999]");
    assert_eq!(parsed.body, "x\n[exit code: 99999999999999]");
    assert_eq!(parsed.exit, ParsedExit::Code(0));
}

#[test]
fn an_empty_signal_name_is_not_a_marker() {
    let parsed = parse_exit_status("x\n[killed by signal: ]");
    assert_eq!(parsed.body, "x\n[killed by signal: ]");
    assert_eq!(parsed.exit, ParsedExit::Code(0));
}

#[test]
fn every_rendered_status_round_trips_through_the_parse() {
    // The contract that matters: whatever the renderer emits, the parser recovers.
    let cases = [
        (clean("hi\n", ""), ParsedExit::Code(0)),
        (failed(1, "oops\n", ""), ParsedExit::Code(1)),
        (failed(143, "slow\n", ""), ParsedExit::Code(143)),
        (clean("(no output)", ""), ParsedExit::Code(0)),
        (
            RunOutcome {
                exit_code: None,
                signal: Some("SIGKILL".to_owned()),
                ..clean("", "")
            },
            ParsedExit::Signal("SIGKILL".to_owned()),
        ),
    ];
    for (outcome, expected) in cases {
        let rendered = render_result(&outcome, &[]);
        let parsed = parse_exit_status(&rendered);
        assert_eq!(parsed.exit, expected, "round trip failed for {rendered:?}");
    }
}

#[test]
fn a_timed_out_render_round_trips_to_its_exit_code_with_the_timeout_left_in_the_body() {
    let outcome = RunOutcome {
        timed_out: true,
        timeout_ms: 100,
        exit_code: Some(143),
        ..clean("slow\n", "")
    };
    let rendered = render_result(&outcome, &[]);
    let parsed = parse_exit_status(&rendered);
    assert_eq!(parsed.exit, ParsedExit::Code(143));
    assert!(parsed.body.contains("[timed out after 100ms]"));
    assert!(!parsed.body.contains("[exit code:"));
}

// ---- promoted calls and job reads ------------------------------------------------------

#[test]
fn a_promoted_call_names_the_job_it_became() {
    assert_eq!(
        render_promoted("job-7", 1000, "working\n"),
        "working\n[still running after 1000ms; moved to background job job-7]"
    );
}

#[test]
fn a_promoted_call_with_no_output_yet_still_names_the_job() {
    assert_eq!(
        render_promoted("job-7", 1000, ""),
        "[still running after 1000ms; moved to background job job-7]"
    );
}

#[test]
fn a_promoted_call_supplies_the_newline_before_its_marker() {
    assert_eq!(
        render_promoted("job-7", 1000, "working"),
        "working\n[still running after 1000ms; moved to background job job-7]"
    );
}

#[test]
fn a_lossy_job_read_names_every_spill_file() {
    let both = render_job_read(
        "out\n",
        true,
        &["/spill/out.log".to_owned(), "/spill/err.log".to_owned()],
        None,
        &[],
    );
    assert_eq!(
        both,
        "out\n[some output was dropped from memory; full output: /spill/out.log, /spill/err.log]"
    );

    let none = render_job_read("out\n", true, &[], None, &[]);
    assert_eq!(
        none,
        "out\n[some output was dropped from memory; full output: (unavailable)]"
    );
}

#[test]
fn a_lossless_job_read_without_sandbox_facts_is_the_delta_verbatim() {
    assert_eq!(render_job_read("out\n", false, &[], None, &[]), "out\n");
}

#[test]
fn a_broken_runner_outranks_a_denial_in_a_job_read() {
    // The command never ran, so a denial-shaped line is a symptom, not a policy decision.
    let mut sandbox = ShellSandboxInfo::runner_failed(SandboxMode::WorkspaceWrite);
    sandbox.denied = true;
    let rendered = render_job_read("", false, &[], Some(&sandbox), &[SandboxMode::Full]);
    assert!(rendered.contains("the sandbox runner itself failed under workspace-write mode"));
    assert!(!rendered.contains("file access denied"));
    assert!(!rendered.contains("escalation available"));
}

#[test]
fn a_denied_job_read_adds_the_hint_only_when_advertised() {
    let sandbox = ShellSandboxInfo::denied(SandboxMode::ReadOnly);
    let plain = render_job_read("x", false, &[], Some(&sandbox), &[]);
    assert_eq!(
        plain,
        "x\n[sandbox: file access denied under read-only mode]"
    );

    let hinted = render_job_read(
        "x",
        false,
        &[],
        Some(&sandbox),
        &[SandboxMode::WorkspaceWrite],
    );
    assert!(hinted.contains("escalation available"));
}

#[test]
fn a_job_read_delta_that_already_ends_in_a_newline_gets_no_extra_one() {
    let sandbox = ShellSandboxInfo::denied(SandboxMode::ReadOnly);
    let rendered = render_job_read("x\n", false, &[], Some(&sandbox), &[]);
    assert_eq!(
        rendered,
        "x\n[sandbox: file access denied under read-only mode]"
    );
}

#[test]
fn ring_delta_sections_stdout_then_stderr() {
    // The ring's interleaving is a buffering artefact; the rendering is sectioned.
    let chunks = [
        Chunk::stdout("a\n"),
        Chunk::stderr("e1\n"),
        Chunk::stdout("b\n"),
        Chunk::stderr("e2\n"),
    ];
    assert_eq!(ring_delta(&chunks), "a\nb\n[stderr]\ne1\ne2\n");
}

#[test]
fn ring_delta_with_only_stderr_still_marks_the_section() {
    assert_eq!(ring_delta(&[Chunk::stderr("err\n")]), "[stderr]\nerr\n");
}

#[test]
fn ring_delta_with_no_stderr_is_plain_stdout() {
    assert_eq!(
        ring_delta(&[Chunk::stdout("a\n"), Chunk::stdout("b")]),
        "a\nb"
    );
}

#[test]
fn ring_delta_supplies_the_separator_a_partial_stdout_line_needs() {
    assert_eq!(
        ring_delta(&[Chunk::stdout("abc"), Chunk::stderr("err")]),
        "abc\n[stderr]\nerr"
    );
}

#[test]
fn ring_delta_of_nothing_is_empty() {
    assert_eq!(ring_delta(&[]), "");
}

// ---- classification: denials -----------------------------------------------------------

#[test]
fn a_denial_needs_a_nonzero_exit_and_a_dialect_match() {
    let signatures = ["read-only file system"];
    assert!(matches_signature(
        Some(1),
        "touch: cannot touch 'x': Read-only file system",
        &signatures
    ));
    // The same stderr on a clean exit is not a denial: the command succeeded.
    assert!(!matches_signature(
        Some(0),
        "touch: cannot touch 'x': Read-only file system",
        &signatures
    ));
    // A signal death is not a denial either.
    assert!(!matches_signature(
        None,
        "touch: cannot touch 'x': Read-only file system",
        &signatures
    ));
    // A non-zero exit with unrelated stderr is a command failure, not a policy decision.
    assert!(!matches_signature(
        Some(1),
        "command not found: nope",
        &signatures
    ));
}

#[test]
fn signature_matching_is_case_insensitive_on_both_sides() {
    assert!(matches_signature(
        Some(2),
        "PERMISSION DENIED",
        &["permission denied"]
    ));
    assert!(matches_signature(
        Some(2),
        "permission denied",
        &["PERMISSION DENIED"]
    ));
}

#[test]
fn an_empty_signature_list_classifies_nothing() {
    assert!(!matches_signature(Some(1), "read-only file system", &[]));
}

#[test]
fn classify_denial_is_the_signature_match_over_a_settled_run() {
    let stderr = "sandbox: EROFS: read-only file system";
    assert!(classify_denial(Some(1), stderr, &["erofs"]));
    assert!(!classify_denial(Some(1), stderr, &["eacces"]));
}

// ---- classification: runner failures ---------------------------------------------------

/// A rule matching the bwrap "no permissions" failure, with one informational line excluded.
fn bwrap_rule() -> RunnerFailureRule {
    RunnerFailureRule {
        allowed_exit_codes: Some(vec![1]),
        fatal_signatures: vec!["bwrap: ".to_owned(), "no permissions".to_owned()],
        informational_lines: vec!["bwrap: setting up uid map".to_owned()],
    }
}

#[test]
fn a_fatal_line_identifies_a_broken_runner() {
    let stderr = "bwrap: setting up uid map\nbwrap: Can't mount proc on /newroot/proc: No such file or directory\n";
    let matched = classify_runner_failure(Some(1), stderr, &[bwrap_rule()]);
    let detail = matched.map(|matched| matched.detail);
    assert_eq!(
        detail.as_deref(),
        Some("bwrap: Can't mount proc on /newroot/proc: No such file or directory")
    );
}

#[test]
fn an_informational_line_alone_does_not_prove_a_runner_failure() {
    // The runner said only its ordinary setup chatter, so a failing command is a command failure.
    let matched = classify_runner_failure(Some(1), "bwrap: setting up uid map\n", &[bwrap_rule()]);
    assert!(matched.is_none());
}

#[test]
fn exit_status_alone_never_proves_a_runner_failure() {
    // Even stderr that matches nothing at all cannot match: exit 0 and a signal are both excluded.
    let rule = bwrap_rule();
    assert!(
        classify_runner_failure(Some(0), "bwrap: boom\n", std::slice::from_ref(&rule)).is_none()
    );
    assert!(classify_runner_failure(None, "bwrap: boom\n", &[rule]).is_none());
}

#[test]
fn a_rule_exit_code_gate_excludes_other_codes() {
    let matched = classify_runner_failure(Some(2), "bwrap: boom\n", &[bwrap_rule()]);
    assert!(matched.is_none());
}

#[test]
fn a_rule_without_an_exit_code_gate_matches_any_nonzero_exit() {
    let rule = RunnerFailureRule {
        fatal_signatures: vec!["bwrap: ".to_owned()],
        ..RunnerFailureRule::default()
    };
    assert!(classify_runner_failure(Some(7), "bwrap: boom\n", &[rule]).is_some());
}

#[test]
fn a_blank_signature_carries_no_evidence_but_leaves_valid_ones_active() {
    let rule = RunnerFailureRule {
        fatal_signatures: vec!["   ".to_owned(), "real failure".to_owned()],
        ..RunnerFailureRule::default()
    };
    // The blank signature must not match every line.
    assert!(
        classify_runner_failure(Some(1), "ordinary line\n", std::slice::from_ref(&rule)).is_none()
    );
    assert!(classify_runner_failure(Some(1), "a real failure\n", &[rule]).is_some());
}

#[test]
fn the_reported_detail_is_the_fatal_line_not_the_informational_one_that_preceded_it() {
    let rule = RunnerFailureRule {
        fatal_signatures: vec!["fatal".to_owned()],
        informational_lines: vec!["note: benign".to_owned()],
        ..RunnerFailureRule::default()
    };
    let stderr = "note: benign\nfatal: the runner broke\nfatal: trailing\n";
    let matched = classify_runner_failure(Some(3), stderr, &[rule]);
    assert_eq!(
        matched.map(|matched| matched.detail).as_deref(),
        Some("fatal: the runner broke")
    );
}

#[test]
fn information_exclusion_is_exact_full_line_equality() {
    let rule = RunnerFailureRule {
        fatal_signatures: vec!["benign".to_owned()],
        informational_lines: vec!["benign line".to_owned()],
        ..RunnerFailureRule::default()
    };
    // The line carries more than the informational text, so it is not excluded.
    assert!(classify_runner_failure(Some(1), "benign line and more\n", &[rule]).is_some());
}

#[test]
fn rules_are_tried_in_order() {
    let first = RunnerFailureRule {
        fatal_signatures: vec!["first".to_owned()],
        ..RunnerFailureRule::default()
    };
    let second = RunnerFailureRule {
        fatal_signatures: vec!["second".to_owned()],
        ..RunnerFailureRule::default()
    };
    let matched = classify_runner_failure(Some(1), "second\nfirst\n", &[first, second]);
    assert_eq!(
        matched.map(|matched| matched.detail).as_deref(),
        Some("first")
    );
}

#[test]
fn carriage_returns_do_not_hide_a_fatal_line() {
    // Windows-style line endings reach the classifier unchanged; the line is still identified.
    let rule = RunnerFailureRule {
        informational_lines: vec!["benign".to_owned()],
        fatal_signatures: vec!["boom".to_owned()],
        ..RunnerFailureRule::default()
    };
    let matched = classify_runner_failure(Some(1), "benign\r\nboom goes the runner\r\n", &[rule]);
    assert_eq!(
        matched.map(|matched| matched.detail).as_deref(),
        Some("boom goes the runner")
    );
}

#[test]
fn the_registry_defaults_describe_an_unconfined_run() {
    let info = ShellSandboxInfo::confined(SandboxMode::WorkspaceWrite);
    assert_eq!(info.mode, SandboxMode::WorkspaceWrite);
    assert!(!info.denied);
    assert!(!info.runner_failed);
    assert_eq!(info.enforcement, None);

    let full = ShellSandboxInfo {
        enforcement: Some(SandboxEnforcement::Full),
        ..info.clone()
    };
    assert_eq!(full.enforcement, Some(SandboxEnforcement::Full));

    let partial = ShellSandboxInfo {
        enforcement: Some(SandboxEnforcement::Partial),
        ..info
    };
    // A caller needing an absolute boundary must be able to tell these apart.
    assert_ne!(full.enforcement, partial.enforcement);
}

#[test]
fn a_parsed_status_exposes_its_code_only_for_a_numeric_exit() {
    assert_eq!(ParsedExit::Code(3).code(), Some(3));
    // A signal death has no exit code to report.
    assert_eq!(ParsedExit::Signal("SIGKILL".to_owned()).code(), None);
}

#[test]
fn channel_round_trips_through_a_chunk() {
    let chunk = Chunk::stderr("e");
    assert_eq!(chunk.channel, Channel::Stderr);
    assert_eq!(Chunk::stdout("o").channel, Channel::Stdout);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod shell {
    pub use super::shell_gap as render;
}
