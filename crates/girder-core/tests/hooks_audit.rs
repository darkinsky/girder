//! Tests for the dialect-neutral hook protocol: matching, decoding, merging, and durable records.
//!
//! `packages/experimental/hook-protocol` (749 lines) had **zero** named symbols in this crate. The
//! bridges that consume it (`hooks-claude-code`, `hooks-codex`) are absent as well, so the whole
//! "a configured command hook can veto or annotate a tool call" feature was missing — not merely
//! unwired. This module ports the half both bridges share, and the properties under test are the ones
//! that decide whether a policy holds:
//!
//! - **the dialect decides what a word pattern means.** Claude Code's `Bash` is *the tool named
//!   Bash*; Codex's `Bash` is `/Bash/` and therefore matches `BashOutput`. Getting this backwards
//!   either narrows a hook that was meant to be broad or widens one that was meant to be exact, and
//!   neither shows up as an error.
//! - **an unevaluable matcher is reported, never silently accepted.** A pattern outside the supported
//!   subset must be a config error, because the alternative is a hook that never fires and says
//!   nothing about why.
//! - **exit 2 blocks and nothing else does.** A hook that fails to spawn, or exits 1, must not be
//!   able to deny a tool call — an infrastructure fault is not a decision.
//! - **the event-name guard is load-bearing.** A `hookSpecificOutput` block emitted for a *different*
//!   event must not be able to deny the event that is firing, and the same goes for a block with no
//!   discriminator at all.
//! - **the fold is order-independent and keeps only the winning reason.** `deny > ask > allow`, and a
//!   reason attached to a losing rank must never be shown as the reason for the outcome.
//! - **the recorded payloads match the schema the crate already declares** — `hook/invoked` and
//!   `hook/result` field sets in [`girder_core`]'s migration table, including which keys are *absent*.
//!
//! The module is included by path rather than through the crate root, because the crate root is
//! wired up separately. It spells its paths through `girder_core::`, which is the alias the library
//! declares for itself, so the same source builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::float_cmp,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/hooks/audit.rs"]
pub mod audit_gap;

use audit_gap::{
    CLAUDE_EVENTS, CODEX_EVENTS, CommandHook, DEFAULT_HOOK_TIMEOUT_MS,
    DEFAULT_STDERR_SUMMARY_MAX_CHARS, Decision, Dialect, HookOutput, MatcherGroup, MergedDecision,
    SubstitutionVars, hook_invoked_event, hook_result_event, hook_stdin, is_claude_literal,
    is_match_all, matcher_diagnostic, matches_matcher, merge_hook_outputs, parse_claude_config,
    parse_codex_config, parse_hook_output, resolve_timeout_ms, select_hook_groups,
    substitute_command, summarize_stderr,
};
use serde_json::json;

// ---- fixtures ----------------------------------------------------------------------------------

/// A hook output with the three stream fields defaulted, matching upstream's test helper.
fn out(over: HookOutput) -> HookOutput {
    over
}

/// An exit-0 output that only carries `decision`.
fn decided(decision: Decision) -> HookOutput {
    HookOutput {
        exit_code: Some(0),
        decision: Some(decision),
        ..HookOutput::default()
    }
}

// =============================================================================================
// Matching — the match-all sentinels
// =============================================================================================

#[test]
fn match_all_sentinels_are_absent_empty_and_star() {
    assert!(is_match_all(None));
    assert!(is_match_all(Some("")));
    assert!(is_match_all(Some("*")));
    // `**` is not a sentinel: it is a pattern that matches anything containing `*`, and collapsing it
    // into "match all" would hide a typo.
    assert!(!is_match_all(Some("**")));
    assert!(!is_match_all(Some(" ")));
}

#[test]
fn every_match_all_sentinel_selects_everything_in_both_dialects() {
    for dialect in [Dialect::ClaudeCode, Dialect::Codex] {
        assert!(matches_matcher(None, "Bash", dialect));
        assert!(matches_matcher(Some(""), "anything", dialect));
        assert!(matches_matcher(Some("*"), "whatever", dialect));
    }
}

// =============================================================================================
// Matching — the Claude dialect's literal-or-regex rule
// =============================================================================================

#[test]
fn claude_word_pattern_is_a_literal_exact_match_not_a_substring() {
    // The whole point: `Bash` must not select `BashOutput`, which a regex would.
    assert!(matches_matcher(Some("Bash"), "Bash", Dialect::ClaudeCode));
    assert!(!matches_matcher(
        Some("Bash"),
        "BashOutput",
        Dialect::ClaudeCode
    ));
}

#[test]
fn claude_pipe_pattern_is_literal_alternation() {
    assert!(matches_matcher(
        Some("Edit|Write"),
        "Edit",
        Dialect::ClaudeCode
    ));
    assert!(matches_matcher(
        Some("Edit|Write"),
        "Write",
        Dialect::ClaudeCode
    ));
    assert!(!matches_matcher(
        Some("Edit|Write"),
        "Read",
        Dialect::ClaudeCode
    ));
    // Exact per alternative, still not a substring.
    assert!(!matches_matcher(
        Some("Edit|Write"),
        "EditFile",
        Dialect::ClaudeCode
    ));
}

#[test]
fn claude_non_word_pattern_falls_through_to_unanchored_regex() {
    assert!(matches_matcher(Some("^Bash$"), "Bash", Dialect::ClaudeCode));
    assert!(matches_matcher(
        Some("Bash.*"),
        "BashOutput",
        Dialect::ClaudeCode
    ));
    assert!(matches_matcher(
        Some(".*\\.ts$"),
        "foo.ts",
        Dialect::ClaudeCode
    ));
    assert!(!matches_matcher(
        Some(".*\\.ts$"),
        "foo.js",
        Dialect::ClaudeCode
    ));
}

#[test]
fn claude_literal_shape_is_word_chars_and_pipe_only() {
    assert!(is_claude_literal("Bash"));
    assert!(is_claude_literal("Edit|Write"));
    assert!(is_claude_literal("tool_1"));
    assert!(!is_claude_literal(""));
    assert!(!is_claude_literal("Bash.*"));
    assert!(!is_claude_literal("^Bash$"));
    assert!(!is_claude_literal("Bash Edit"));
}

// =============================================================================================
// Matching — the Codex dialect has no literal fast path
// =============================================================================================

#[test]
fn codex_word_pattern_is_an_unanchored_regex() {
    assert!(matches_matcher(Some("Bash"), "Bash", Dialect::Codex));
    // Codex has NO literal fast path: `Bash` is `/Bash/`, so it DOES match a substring.
    assert!(matches_matcher(Some("Bash"), "BashOutput", Dialect::Codex));
}

#[test]
fn codex_regex_alternation_and_anchors_work() {
    assert!(matches_matcher(Some("Edit|Write"), "Edit", Dialect::Codex));
    assert!(matches_matcher(Some("^Bash$"), "Bash", Dialect::Codex));
    assert!(!matches_matcher(
        Some("^Bash$"),
        "BashOutput",
        Dialect::Codex
    ));
}

#[test]
fn the_same_pattern_selects_differently_per_dialect() {
    // The single most consequential divergence, asserted side by side.
    assert!(!matches_matcher(
        Some("Bash"),
        "BashOutput",
        Dialect::ClaudeCode
    ));
    assert!(matches_matcher(Some("Bash"), "BashOutput", Dialect::Codex));
}

// =============================================================================================
// Matching — an invalid pattern is a non-match, never a panic
// =============================================================================================

#[test]
fn an_invalid_pattern_matches_nothing_and_never_panics() {
    // `(` is not the Claude literal charset, so it reaches the regex path and is invalid there.
    assert!(!matches_matcher(Some("("), "x", Dialect::ClaudeCode));
    assert!(!matches_matcher(Some("["), "x", Dialect::Codex));
    // A malformed one of several alternatives poisons the whole pattern rather than letting the
    // well-formed alternatives answer: a partially-evaluated matcher is a silent widening.
    assert!(!matches_matcher(Some("Bash|("), "Bash", Dialect::Codex));
}

#[test]
fn escaped_pipe_is_one_alternative_not_two() {
    // `a\|b` must not split, or a pattern meant to match a literal pipe would match two words.
    assert!(!matches_matcher(Some("a\\|b"), "a", Dialect::Codex));
    assert!(matches_matcher(Some("a\\|b"), "a|b", Dialect::Codex));
}

// =============================================================================================
// Config-time diagnostics
// =============================================================================================

#[test]
fn matcher_diagnostic_accepts_sentinels_literals_and_valid_regexes() {
    assert!(matcher_diagnostic(None, Dialect::ClaudeCode).is_none());
    assert!(matcher_diagnostic(Some(""), Dialect::Codex).is_none());
    assert!(matcher_diagnostic(Some("*"), Dialect::Codex).is_none());
    assert!(matcher_diagnostic(Some("Edit|Write"), Dialect::ClaudeCode).is_none());
    assert!(matcher_diagnostic(Some("^Bash$"), Dialect::ClaudeCode).is_none());
    assert!(matcher_diagnostic(Some("Edit|Write"), Dialect::Codex).is_none());
}

#[test]
fn matcher_diagnostic_is_upstreams_stable_string() {
    assert_eq!(
        matcher_diagnostic(Some("("), Dialect::ClaudeCode).as_deref(),
        Some(r#"invalid claude-code regex matcher "(""#)
    );
    assert_eq!(
        matcher_diagnostic(Some("["), Dialect::Codex).as_deref(),
        Some(r#"invalid codex regex matcher "[""#)
    );
}

// =============================================================================================
// Group selection
// =============================================================================================

#[test]
fn absent_and_empty_hook_points_select_no_groups() {
    for dialect in [Dialect::ClaudeCode, Dialect::Codex] {
        assert!(select_hook_groups(None, "Bash", dialect).is_empty());
        assert!(select_hook_groups(Some(&[]), "Bash", dialect).is_empty());
    }
}

#[test]
fn selection_keeps_config_order_and_drops_hookless_groups() {
    let groups = vec![
        MatcherGroup {
            matcher: Some("*".to_owned()),
            hooks: Vec::new(),
        },
        MatcherGroup {
            matcher: Some("*".to_owned()),
            hooks: vec![CommandHook {
                command: "first".to_owned(),
                timeout_sec: None,
            }],
        },
        MatcherGroup {
            matcher: Some("Read".to_owned()),
            hooks: vec![CommandHook {
                command: "unmatched".to_owned(),
                timeout_sec: None,
            }],
        },
        MatcherGroup {
            matcher: Some("^Bash$".to_owned()),
            hooks: vec![
                CommandHook {
                    command: "second".to_owned(),
                    timeout_sec: None,
                },
                CommandHook {
                    command: "third".to_owned(),
                    timeout_sec: None,
                },
            ],
        },
    ];
    let selected = select_hook_groups(Some(&groups), "Bash", Dialect::ClaudeCode);
    let commands: Vec<&str> = selected
        .iter()
        .flat_map(|group| group.hooks.iter().map(|hook| hook.command.as_str()))
        .collect();
    // The hookless `*` group runs nothing, so it is not selected even though its matcher matched.
    assert_eq!(commands, ["first", "second", "third"]);
    // The original list is untouched: selection borrows rather than edits.
    assert_eq!(groups.len(), 4);
}

#[test]
fn an_empty_hook_list_is_not_a_run() {
    // A group whose only hook was skipped parses to no hooks; selecting it would report a hook run
    // that never happened.
    let groups = vec![MatcherGroup {
        matcher: Some("Bash".to_owned()),
        hooks: Vec::new(),
    }];
    assert!(select_hook_groups(Some(&groups), "Bash", Dialect::Codex).is_empty());
}

// =============================================================================================
// Constants — the two dialect tables
// =============================================================================================

#[test]
fn codex_events_are_a_strict_subset_of_claude_events() {
    for event in CODEX_EVENTS {
        assert!(
            CLAUDE_EVENTS.contains(&event),
            "Codex event {event} is not a Claude Code event"
        );
    }
    // The subagent pair is Claude-only: Codex has no subagent hook point, so honouring it would be a
    // hook that never fires.
    assert!(CLAUDE_EVENTS.contains(&"SubagentStart"));
    assert!(!CODEX_EVENTS.contains(&"SubagentStart"));
    assert!(!CODEX_EVENTS.contains(&"SubagentStop"));
    // `Setup` is in neither: it is a Claude Code settings event this build does not run.
    assert!(!CLAUDE_EVENTS.contains(&"Setup"));
}

#[test]
fn the_shared_defaults_are_upstreams_numbers() {
    assert_eq!(DEFAULT_HOOK_TIMEOUT_MS, 600_000);
    assert_eq!(DEFAULT_STDERR_SUMMARY_MAX_CHARS, 500);
    assert_eq!(DEFAULT_HOOK_TIMEOUT_MS / 1000, 600);
}

// =============================================================================================
// Config parsing — Claude Code
// =============================================================================================

#[test]
fn claude_config_accepts_a_wrapped_settings_object_and_a_bare_event_map() {
    let wrapped = json!({
        "hooks": {
            "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "x.sh" }] }]
        }
    });
    let bare = json!({
        "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "x.sh" }] }]
    });
    let from_wrapped = parse_claude_config(&wrapped, &SubstitutionVars::default()).unwrap();
    let from_bare = parse_claude_config(&bare, &SubstitutionVars::default()).unwrap();
    assert_eq!(from_wrapped.groups, from_bare.groups);
    let groups = &from_wrapped.groups["PreToolUse"];
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].matcher.as_deref(), Some("Bash"));
    assert_eq!(groups[0].hooks[0].command, "x.sh");
}

#[test]
fn claude_config_ignores_unsupported_events_before_parsing_their_groups() {
    // `Setup` is not a supported point, so its malformed matcher must not fail the whole config.
    let raw = json!({
        "Setup": [{ "matcher": "(", "hooks": [{ "type": "command", "command": "ignored.sh" }] }],
        "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "kept.sh" }] }],
    });
    let parsed = parse_claude_config(&raw, &SubstitutionVars::default()).unwrap();
    assert!(!parsed.groups.contains_key("Setup"));
    assert_eq!(parsed.groups["PreToolUse"][0].hooks[0].command, "kept.sh");
}

#[test]
fn claude_config_rejects_a_runnable_group_with_an_invalid_matcher() {
    let raw = json!({
        "PreToolUse": [{ "matcher": "(", "hooks": [{ "type": "command", "command": "x.sh" }] }],
    });
    let error = parse_claude_config(&raw, &SubstitutionVars::default()).unwrap_err();
    assert_eq!(
        error.message,
        r#"invalid claude-code regex matcher "(" on event "PreToolUse""#
    );
}

#[test]
fn claude_config_records_non_command_hooks_as_skipped() {
    let raw = json!({
        "PreToolUse": [
            { "matcher": "Bash", "hooks": [{ "type": "prompt", "command": "ignored" }] },
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": "runs.sh" }] },
        ],
    });
    let parsed = parse_claude_config(&raw, &SubstitutionVars::default()).unwrap();
    assert_eq!(parsed.skipped.len(), 1);
    assert_eq!(parsed.skipped[0].event, "PreToolUse");
    assert_eq!(parsed.skipped[0].reason, r#"unsupported "prompt" hook"#);
    // The command group still runs; one unusable entry does not disable its siblings.
    assert_eq!(parsed.groups["PreToolUse"].len(), 1);
}

#[test]
fn claude_config_discards_the_matcher_on_events_with_no_matcher_subject() {
    let raw = json!({
        "UserPromptSubmit": [{ "matcher": "(", "hooks": [{ "type": "command", "command": "prompt.sh" }] }],
        "Stop": [{ "matcher": "[", "hooks": [{ "type": "command", "command": "stop.sh" }] }],
    });
    let parsed = parse_claude_config(&raw, &SubstitutionVars::default()).unwrap();
    // A matcher on these events can only be a typo, so it is discarded *before* validation and the
    // invalid pattern therefore cannot fail the config.
    assert!(parsed.groups["UserPromptSubmit"][0].matcher.is_none());
    assert!(parsed.groups["Stop"][0].matcher.is_none());
}

#[test]
fn claude_config_applies_substitutions_to_every_occurrence() {
    let raw = json!({
        "PreToolUse": [{
            "matcher": "Bash",
            "hooks": [{
                "type": "command",
                "command": "${CLAUDE_PLUGIN_ROOT}/a.sh ${CLAUDE_PLUGIN_ROOT}/b.sh ${CLAUDE_PROJECT_DIR}",
            }],
        }],
    });
    let vars = SubstitutionVars {
        plugin_root: Some("/plugin"),
        project_dir: Some("/project"),
    };
    let parsed = parse_claude_config(&raw, &vars).unwrap();
    assert_eq!(
        parsed.groups["PreToolUse"][0].hooks[0].command,
        "/plugin/a.sh /plugin/b.sh /project"
    );
}

#[test]
fn an_unset_substitution_variable_leaves_its_token_verbatim() {
    // The hook's own shell then reports the unexpanded name instead of running an empty path.
    assert_eq!(
        substitute_command("${CLAUDE_PLUGIN_ROOT}/x.sh", &SubstitutionVars::default()),
        "${CLAUDE_PLUGIN_ROOT}/x.sh"
    );
    assert_eq!(
        substitute_command(
            "${CLAUDE_PROJECT_DIR}/x.sh",
            &SubstitutionVars {
                plugin_root: Some("/plugin"),
                project_dir: None,
            }
        ),
        "${CLAUDE_PROJECT_DIR}/x.sh"
    );
}

#[test]
fn claude_config_reads_the_timeout_in_seconds() {
    let raw = json!({
        "PreToolUse": [{
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": "slow.sh", "timeout": 30 }],
        }],
    });
    let parsed = parse_claude_config(&raw, &SubstitutionVars::default()).unwrap();
    assert_eq!(
        parsed.groups["PreToolUse"][0].hooks[0].timeout_sec,
        Some(30.0)
    );
}

// =============================================================================================
// Config parsing — Codex
// =============================================================================================

#[test]
fn codex_config_skips_async_hooks() {
    let raw = json!({
        "PreToolUse": [{
            "matcher": "Bash",
            "hooks": [
                { "type": "command", "command": "async.sh", "async": true },
                { "type": "command", "command": "sync.sh" },
            ],
        }],
    });
    let parsed = parse_codex_config(&raw).unwrap();
    assert_eq!(parsed.skipped.len(), 1);
    assert_eq!(parsed.skipped[0].reason, "async hook");
    assert_eq!(parsed.groups["PreToolUse"][0].hooks[0].command, "sync.sh");
}

#[test]
fn codex_config_applies_no_substitution() {
    let raw = json!({
        "PreToolUse": [{
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/x.sh" }],
        }],
    });
    let parsed = parse_codex_config(&raw).unwrap();
    assert_eq!(
        parsed.groups["PreToolUse"][0].hooks[0].command,
        "${CLAUDE_PLUGIN_ROOT}/x.sh"
    );
}

#[test]
fn codex_config_accepts_the_timeout_sec_alias() {
    let raw = json!({
        "PreToolUse": [{
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": "x.sh", "timeoutSec": 5 }],
        }],
    });
    let parsed = parse_codex_config(&raw).unwrap();
    assert_eq!(
        parsed.groups["PreToolUse"][0].hooks[0].timeout_sec,
        Some(5.0)
    );
}

#[test]
fn codex_config_rejects_an_invalid_matcher_with_its_own_dialect_name() {
    let raw = json!({
        "PreToolUse": [{ "matcher": "[", "hooks": [{ "type": "command", "command": "s.sh" }] }],
    });
    let error = parse_codex_config(&raw).unwrap_err();
    assert_eq!(
        error.message,
        r#"invalid codex regex matcher "[" on event "PreToolUse""#
    );
}

#[test]
fn a_config_that_is_not_an_object_yields_nothing_rather_than_failing() {
    for raw in [json!(null), json!([]), json!("nope")] {
        let parsed = parse_codex_config(&raw).unwrap();
        assert!(parsed.groups.is_empty());
        assert!(parsed.skipped.is_empty());
    }
}

#[test]
fn a_missing_command_is_ignored_and_does_not_produce_an_empty_group() {
    let raw = json!({
        "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command" }] }],
    });
    let parsed = parse_codex_config(&raw).unwrap();
    assert!(!parsed.groups.contains_key("PreToolUse"));
    assert!(parsed.skipped.is_empty());
}

// =============================================================================================
// Decoding — exit-code semantics
// =============================================================================================

#[test]
fn exit_zero_with_no_stdout_is_neutral() {
    let output = parse_hook_output(Some(0), "", "", None);
    assert_eq!(output.exit_code, Some(0));
    assert!(output.decision.is_none());
    assert!(output.continue_.is_none());
}

#[test]
fn exit_two_blocks_and_stderr_becomes_the_reason() {
    let output = parse_hook_output(Some(2), "", "this command is not allowed", None);
    assert_eq!(output.decision, Some(Decision::Block));
    assert_eq!(
        output.reason.as_deref(),
        Some("this command is not allowed")
    );
    assert_eq!(output.stderr, "this command is not allowed");
}

#[test]
fn exit_two_with_blank_stderr_still_blocks_with_no_reason() {
    let output = parse_hook_output(Some(2), "", "   ", None);
    assert_eq!(output.decision, Some(Decision::Block));
    assert!(output.reason.is_none());
}

#[test]
fn any_other_non_zero_exit_is_a_non_blocking_error() {
    let output = parse_hook_output(Some(1), "", "some warning", None);
    assert_eq!(output.exit_code, Some(1));
    assert!(output.decision.is_none());
    assert_eq!(output.stderr, "some warning");
}

#[test]
fn an_absent_exit_code_carries_no_decision() {
    // A hook that never started produced no decision; inventing a block would let an infrastructure
    // fault deny a tool call.
    let output = parse_hook_output(None, "", "spawn failed: ENOENT", None);
    assert!(output.exit_code.is_none());
    assert!(output.decision.is_none());
    assert_eq!(output.stderr, "spawn failed: ENOENT");
}

#[test]
fn structured_stdout_is_ignored_on_a_blocking_run() {
    // exit 2 forces a block regardless of what stdout claims, so stderr is authoritative.
    let output = parse_hook_output(Some(2), r#"{"decision":"approve"}"#, "blocked", None);
    assert_eq!(output.decision, Some(Decision::Block));
    assert_eq!(output.reason.as_deref(), Some("blocked"));
}

// =============================================================================================
// Decoding — structured stdout
// =============================================================================================

#[test]
fn top_level_continue_stop_reason_and_system_message_are_parsed() {
    let output = parse_hook_output(
        Some(0),
        r#"{"continue":false,"stopReason":"budget exceeded","systemMessage":"heads up"}"#,
        "",
        None,
    );
    assert_eq!(output.continue_, Some(false));
    assert_eq!(output.stop_reason.as_deref(), Some("budget exceeded"));
    assert_eq!(output.system_message.as_deref(), Some("heads up"));
}

#[test]
fn the_legacy_top_level_decision_accepts_only_approve_and_block() {
    let block = parse_hook_output(Some(0), r#"{"decision":"block","reason":"nope"}"#, "", None);
    assert_eq!(block.decision, Some(Decision::Block));
    assert_eq!(block.reason.as_deref(), Some("nope"));
    let approve = parse_hook_output(Some(0), r#"{"decision":"approve"}"#, "", None);
    assert_eq!(approve.decision, Some(Decision::Approve));
}

#[test]
fn allow_deny_and_ask_are_invalid_at_the_top_level_and_ignored() {
    // Both reference schemas restrict the legacy channel to approve/block; allow/deny/ask belong to
    // `hookSpecificOutput.permissionDecision`. Accepting them here would let an out-of-band
    // `{"decision":"deny"}` deny a call the schema says it cannot deny.
    for value in ["deny", "allow", "ask"] {
        let output = parse_hook_output(Some(0), &format!(r#"{{"decision":"{value}"}}"#), "", None);
        assert!(
            output.decision.is_none(),
            "{value} was accepted at top level"
        );
    }
}

#[test]
fn an_unknown_decision_string_is_ignored_not_coerced() {
    let output = parse_hook_output(Some(0), r#"{"decision":"maybe"}"#, "", None);
    assert!(output.decision.is_none());
}

#[test]
fn the_hook_specific_discriminator_is_recorded() {
    let output = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"}}"#,
        "",
        None,
    );
    assert_eq!(output.hook_event_name.as_deref(), Some("PreToolUse"));
    assert_eq!(output.decision, Some(Decision::Deny));
}

#[test]
fn the_permission_decision_overrides_the_legacy_top_level_decision() {
    let output = parse_hook_output(
        Some(0),
        r#"{"decision":"approve","hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"denied by policy"}}"#,
        "",
        None,
    );
    assert_eq!(output.decision, Some(Decision::Deny));
    assert_eq!(output.reason.as_deref(), Some("denied by policy"));
}

#[test]
fn allow_and_ask_are_read_from_the_permission_decision_channel() {
    let allow = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
        "",
        None,
    );
    assert_eq!(allow.decision, Some(Decision::Allow));
    let ask = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#,
        "",
        None,
    );
    assert_eq!(ask.decision, Some(Decision::Ask));
}

#[test]
fn additional_context_and_updated_input_are_parsed() {
    let output = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"additionalContext":"remember X","updatedInput":{"command":"safe"}}}"#,
        "",
        None,
    );
    assert_eq!(output.additional_context.as_deref(), Some("remember X"));
    // Parsed, not honoured: the field is carried so a bridge can warn about it.
    assert_eq!(
        output.updated_input,
        Some(
            json!({"command":"safe"})
                .as_object()
                .cloned()
                .expect("an object")
        )
    );
}

// =============================================================================================
// Decoding — the event-name guard
// =============================================================================================

#[test]
fn a_mismatched_block_has_its_event_scoped_fields_discarded() {
    // A PreToolUse block emitted on a Stop hook must not deny the Stop.
    let output = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no","additionalContext":"x","updatedInput":{"command":"y"}}}"#,
        "",
        Some("Stop"),
    );
    // The discriminator is still recorded: the log is the only place a copied hook's wrong event shows.
    assert_eq!(output.hook_event_name.as_deref(), Some("PreToolUse"));
    assert!(output.decision.is_none());
    assert!(output.reason.is_none());
    assert!(output.additional_context.is_none());
    assert!(output.updated_input.is_none());
}

#[test]
fn a_discriminatorless_block_is_discarded_when_an_event_is_expected() {
    // Under the keyed schema a missing discriminator is as malformed as a mismatched one.
    let output = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"permissionDecision":"deny","additionalContext":"x"}}"#,
        "",
        Some("Stop"),
    );
    assert!(output.hook_event_name.is_none());
    assert!(output.decision.is_none());
    assert!(output.additional_context.is_none());
}

#[test]
fn a_matching_block_applies() {
    let output = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","additionalContext":"x"}}"#,
        "",
        Some("PreToolUse"),
    );
    assert_eq!(output.decision, Some(Decision::Deny));
    assert_eq!(output.additional_context.as_deref(), Some("x"));
}

#[test]
fn opting_out_of_the_guard_applies_any_block_as_is() {
    let matched = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"}}"#,
        "",
        None,
    );
    assert_eq!(matched.decision, Some(Decision::Deny));
    let bare = parse_hook_output(
        Some(0),
        r#"{"hookSpecificOutput":{"permissionDecision":"deny"}}"#,
        "",
        None,
    );
    assert_eq!(bare.decision, Some(Decision::Deny));
}

#[test]
fn a_mismatched_block_does_not_discard_the_event_agnostic_top_level_fields() {
    // Only the per-event block is scoped; `decision`/`continue`/`stopReason` are not.
    let output = parse_hook_output(
        Some(0),
        r#"{"decision":"block","reason":"top","continue":false,"stopReason":"halt","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#,
        "",
        Some("Stop"),
    );
    assert_eq!(output.decision, Some(Decision::Block));
    assert_eq!(output.reason.as_deref(), Some("top"));
    assert_eq!(output.continue_, Some(false));
    assert_eq!(output.stop_reason.as_deref(), Some("halt"));
}

// =============================================================================================
// Decoding — leniency
// =============================================================================================

#[test]
fn malformed_json_on_a_clean_exit_is_lenient() {
    let output = parse_hook_output(Some(0), "{ not valid json", "", None);
    assert!(output.decision.is_none());
    assert!(output.continue_.is_none());
    // The raw text is still available to the bridge as plain output.
    assert_eq!(output.stdout, "{ not valid json");
}

#[test]
fn non_object_stdout_on_a_clean_exit_is_plain_output() {
    let output = parse_hook_output(Some(0), "just some text output", "", None);
    assert!(output.decision.is_none());
    assert_eq!(output.stdout, "just some text output");
    // A JSON array starts with `[`, so it is not even attempted.
    let array = parse_hook_output(Some(0), "[1,2,3]", "", None);
    assert!(array.decision.is_none());
}

#[test]
fn raw_stdout_is_preserved_trimmed_alongside_parsed_fields() {
    let output = parse_hook_output(Some(0), "  {\"decision\":\"block\"}  \n", "", None);
    assert_eq!(output.stdout, r#"{"decision":"block"}"#);
    assert_eq!(output.decision, Some(Decision::Block));
    assert_eq!(parse_hook_output(Some(0), "", "", None).stdout, "");
}

// =============================================================================================
// Merging
// =============================================================================================

#[test]
fn an_empty_merge_is_neutral() {
    let merged = merge_hook_outputs(&[]);
    assert_eq!(merged.decision, MergedDecision::None);
    assert!(!merged.stop);
    assert!(merged.additional_context.is_empty());
    assert!(merged.system_messages.is_empty());
    assert!(merged.reason.is_none());
}

#[test]
fn approve_and_allow_both_fold_to_allow() {
    assert_eq!(
        merge_hook_outputs(&[decided(Decision::Allow)]).decision,
        MergedDecision::Allow
    );
    assert_eq!(
        merge_hook_outputs(&[decided(Decision::Approve)]).decision,
        MergedDecision::Allow
    );
}

#[test]
fn deny_beats_ask_beats_allow_regardless_of_order() {
    assert_eq!(
        merge_hook_outputs(&[decided(Decision::Allow), decided(Decision::Ask)]).decision,
        MergedDecision::Ask
    );
    assert_eq!(
        merge_hook_outputs(&[decided(Decision::Ask), decided(Decision::Deny)]).decision,
        MergedDecision::Deny
    );
    assert_eq!(
        merge_hook_outputs(&[decided(Decision::Deny), decided(Decision::Allow)]).decision,
        MergedDecision::Deny
    );
    // block folds to deny.
    assert_eq!(
        merge_hook_outputs(&[decided(Decision::Allow), decided(Decision::Block)]).decision,
        MergedDecision::Deny
    );
}

#[test]
fn no_decision_anywhere_is_none() {
    assert_eq!(
        merge_hook_outputs(&[out(HookOutput::default()), out(HookOutput::default())]).decision,
        MergedDecision::None
    );
}

#[test]
fn block_and_deny_reasons_are_joined_with_a_blank_line() {
    let merged = merge_hook_outputs(&[
        out(HookOutput {
            decision: Some(Decision::Deny),
            reason: Some("first objection".to_owned()),
            ..HookOutput::default()
        }),
        // An allow's reason is NOT collected: it explains the losing side.
        out(HookOutput {
            decision: Some(Decision::Allow),
            reason: Some("this allow reason is NOT collected".to_owned()),
            ..HookOutput::default()
        }),
        out(HookOutput {
            decision: Some(Decision::Block),
            reason: Some("second objection".to_owned()),
            ..HookOutput::default()
        }),
    ]);
    assert_eq!(
        merged.reason.as_deref(),
        Some("first objection\n\nsecond objection")
    );
}

#[test]
fn no_reason_surfaces_when_nothing_blocked() {
    let merged = merge_hook_outputs(&[out(HookOutput {
        decision: Some(Decision::Allow),
        ..HookOutput::default()
    })]);
    assert!(merged.reason.is_none());
}

#[test]
fn the_winning_asks_reason_surfaces_when_ask_wins() {
    let merged = merge_hook_outputs(&[
        out(HookOutput {
            decision: Some(Decision::Allow),
            reason: Some("allow reason — not surfaced".to_owned()),
            ..HookOutput::default()
        }),
        out(HookOutput {
            decision: Some(Decision::Ask),
            reason: Some("needs approval".to_owned()),
            ..HookOutput::default()
        }),
    ]);
    assert_eq!(merged.decision, MergedDecision::Ask);
    assert_eq!(merged.reason.as_deref(), Some("needs approval"));
}

#[test]
fn ask_reasons_are_dropped_once_deny_wins() {
    // The reason a caller reads has to be the reason the outcome happened.
    let merged = merge_hook_outputs(&[
        out(HookOutput {
            decision: Some(Decision::Ask),
            reason: Some("ask reason — not surfaced once deny wins".to_owned()),
            ..HookOutput::default()
        }),
        out(HookOutput {
            decision: Some(Decision::Deny),
            reason: Some("the real objection".to_owned()),
            ..HookOutput::default()
        }),
    ]);
    assert_eq!(merged.decision, MergedDecision::Deny);
    assert_eq!(merged.reason.as_deref(), Some("the real objection"));
}

#[test]
fn an_empty_reason_contributes_nothing() {
    let merged = merge_hook_outputs(&[out(HookOutput {
        decision: Some(Decision::Deny),
        reason: Some(String::new()),
        ..HookOutput::default()
    })]);
    assert!(merged.reason.is_none());
}

#[test]
fn stop_is_sticky_on_the_first_halting_hook() {
    let merged = merge_hook_outputs(&[
        out(HookOutput {
            continue_: Some(true),
            ..HookOutput::default()
        }),
        out(HookOutput {
            continue_: Some(false),
            stop_reason: Some("halt now".to_owned()),
            ..HookOutput::default()
        }),
        out(HookOutput {
            continue_: Some(false),
            stop_reason: Some("second halt — ignored".to_owned()),
            ..HookOutput::default()
        }),
    ]);
    assert!(merged.stop);
    // The second halt never took effect, so its reason describes nothing.
    assert_eq!(merged.stop_reason.as_deref(), Some("halt now"));
}

#[test]
fn no_stop_when_every_hook_continues() {
    let merged = merge_hook_outputs(&[
        out(HookOutput {
            continue_: Some(true),
            ..HookOutput::default()
        }),
        out(HookOutput::default()),
    ]);
    assert!(!merged.stop);
    assert!(merged.stop_reason.is_none());
}

#[test]
fn a_halt_with_no_stop_reason_stops_with_no_reason() {
    let merged = merge_hook_outputs(&[out(HookOutput {
        continue_: Some(false),
        ..HookOutput::default()
    })]);
    assert!(merged.stop);
    assert!(merged.stop_reason.is_none());
}

#[test]
fn context_and_system_messages_accumulate_in_hook_order_skipping_empties() {
    let merged = merge_hook_outputs(&[
        out(HookOutput {
            additional_context: Some("ctx-A".to_owned()),
            system_message: Some("warn-A".to_owned()),
            ..HookOutput::default()
        }),
        out(HookOutput {
            additional_context: Some(String::new()),
            system_message: Some(String::new()),
            ..HookOutput::default()
        }),
        out(HookOutput {
            additional_context: Some("ctx-B".to_owned()),
            ..HookOutput::default()
        }),
        out(HookOutput {
            system_message: Some("warn-B".to_owned()),
            ..HookOutput::default()
        }),
    ]);
    assert_eq!(merged.additional_context, ["ctx-A", "ctx-B"]);
    assert_eq!(merged.system_messages, ["warn-A", "warn-B"]);
}

// =============================================================================================
// Durable records — schema conformance
// =============================================================================================

#[test]
fn hook_invoked_carries_its_required_fields_and_the_matcher() {
    let payload = hook_invoked_event(1, "PreToolUse", Dialect::ClaudeCode, "h1", Some("Bash"));
    assert_eq!(
        payload,
        json!({
            "turn": 1,
            "point": "PreToolUse",
            "dialect": "claude-code",
            "handlerId": "h1",
            "matcher": "Bash",
        })
    );
}

#[test]
fn hook_invoked_omits_an_absent_matcher_rather_than_writing_null() {
    // The schema's optional set is closed and a null is a value, so an absent matcher is an absent key.
    let payload = hook_invoked_event(2, "Stop", Dialect::Codex, "h2", None);
    assert_eq!(
        payload,
        json!({ "turn": 2, "point": "Stop", "dialect": "codex", "handlerId": "h2" })
    );
    assert!(payload.get("matcher").is_none());
}

#[test]
fn hook_result_derives_decision_exit_code_and_summary() {
    let output = HookOutput {
        exit_code: Some(2),
        stderr: "blocked".to_owned(),
        decision: Some(Decision::Deny),
        ..HookOutput::default()
    };
    let payload = hook_result_event(1, "PreToolUse", "h1", &output, 500, 5);
    assert_eq!(
        payload,
        json!({
            "turn": 1,
            "point": "PreToolUse",
            "handlerId": "h1",
            "decision": "deny",
            "exitCode": 2,
            "stderrSummary": "blocked",
            "durationMs": 5,
        })
    );
}

#[test]
fn hook_result_omits_an_absent_exit_code_and_blank_stderr() {
    let output = HookOutput {
        exit_code: None,
        decision: Some(Decision::Allow),
        ..HookOutput::default()
    };
    let payload = hook_result_event(1, "Stop", "h3", &output, 500, 5);
    assert!(payload.get("exitCode").is_none());
    assert!(payload.get("stderrSummary").is_none());
    assert_eq!(payload["decision"], json!("allow"));
}

#[test]
fn the_recorded_decision_falls_back_to_stop_then_pass() {
    let halted = hook_result_event(
        1,
        "Stop",
        "halt",
        &HookOutput {
            continue_: Some(false),
            ..HookOutput::default()
        },
        500,
        5,
    );
    assert_eq!(halted["decision"], json!("stop"));
    let noop = hook_result_event(1, "Stop", "noop", &HookOutput::default(), 500, 5);
    assert_eq!(noop["decision"], json!("pass"));
    // An explicit decision wins over the continue:false fallback.
    let both = hook_result_event(
        1,
        "Stop",
        "both",
        &HookOutput {
            continue_: Some(false),
            decision: Some(Decision::Block),
            ..HookOutput::default()
        },
        500,
        5,
    );
    assert_eq!(both["decision"], json!("block"));
}

#[test]
fn the_recorded_field_sets_match_the_declared_event_schema() {
    // The crate already declares these sets in `migration_steps_gap`; a payload that grew a key would
    // fail that schema at persistence time, so the check belongs here too.
    let invoked = hook_invoked_event(1, "PreToolUse", Dialect::ClaudeCode, "h1", Some("Bash"));
    let invoked_keys: Vec<&str> = invoked
        .as_object()
        .map(|object| object.keys().map(String::as_str).collect())
        .unwrap_or_default();
    for required in ["turn", "point", "dialect", "handlerId"] {
        assert!(invoked_keys.contains(&required));
    }
    assert!(invoked_keys.contains(&"matcher"));

    let result = hook_result_event(
        1,
        "PreToolUse",
        "h1",
        &HookOutput {
            exit_code: Some(2),
            stderr: "x".to_owned(),
            ..HookOutput::default()
        },
        500,
        5,
    );
    let result_keys: Vec<&str> = result
        .as_object()
        .map(|object| object.keys().map(String::as_str).collect())
        .unwrap_or_default();
    for required in ["turn", "point", "handlerId", "decision", "durationMs"] {
        assert!(result_keys.contains(&required));
    }
    for optional in ["exitCode", "stderrSummary"] {
        assert!(result_keys.contains(&optional));
    }
}

// =============================================================================================
// stderr summaries
// =============================================================================================

#[test]
fn a_blank_stderr_has_no_summary() {
    assert!(summarize_stderr("", 500).is_none());
    assert!(summarize_stderr("  \n\t ", 500).is_none());
}

#[test]
fn a_summary_at_or_under_the_cap_passes_through_trimmed() {
    assert_eq!(
        summarize_stderr("  blocked: bad tool  ", 500).as_deref(),
        Some("blocked: bad tool")
    );
    assert_eq!(summarize_stderr("abc", 3).as_deref(), Some("abc"));
}

#[test]
fn a_summary_past_the_cap_is_cut_with_an_ellipsis() {
    assert_eq!(summarize_stderr("abcdef", 4).as_deref(), Some("abcd…"));
    assert_eq!(
        summarize_stderr(&"x".repeat(600), 500).as_deref(),
        Some(format!("{}…", "x".repeat(500)).as_str())
    );
}

#[test]
fn the_cap_is_inclusive_so_a_500_character_summary_is_kept_verbatim() {
    // An off-by-one here appends an ellipsis to a summary that lost nothing, and a reader cannot tell
    // that from a real cut.
    let exact = "y".repeat(500);
    assert_eq!(
        summarize_stderr(&exact, 500).as_deref(),
        Some(exact.as_str())
    );
}

#[test]
fn the_cap_counts_characters_not_bytes() {
    // A CJK character is three bytes; a byte cap would cut a 500-character summary to ~166.
    let cjk = "中".repeat(500);
    assert_eq!(summarize_stderr(&cjk, 500), Some(cjk.clone()));
    let over = "中".repeat(501);
    let summary = summarize_stderr(&over, 500).expect("a summary");
    assert_eq!(summary.chars().count(), 501);
    assert!(summary.ends_with('…'));
}

// =============================================================================================
// Runner decisions
// =============================================================================================

#[test]
fn a_hook_with_no_timeout_uses_the_default() {
    assert_eq!(resolve_timeout_ms(None, 600_000), 600_000);
    assert_eq!(resolve_timeout_ms(None, DEFAULT_HOOK_TIMEOUT_MS), 600_000);
}

#[test]
fn a_configured_timeout_is_in_seconds_and_overrides_the_default() {
    assert_eq!(resolve_timeout_ms(Some(30.0), 600_000), 30_000);
    assert_eq!(resolve_timeout_ms(Some(1.5), 600_000), 1_500);
}

#[test]
fn a_nonsensical_timeout_saturates_instead_of_panicking() {
    // Config data arrives from a file, so a negative or non-finite value must not abort the turn.
    assert_eq!(resolve_timeout_ms(Some(-1.0), 600_000), 0);
    assert_eq!(resolve_timeout_ms(Some(0.0), 600_000), 0);
    assert_eq!(resolve_timeout_ms(Some(f64::NAN), 600_000), 0);
    assert_eq!(resolve_timeout_ms(Some(f64::INFINITY), 600_000), 0);
}

#[test]
fn stdin_framing_honours_the_dialect_newline() {
    let payload = json!({ "tool": "Bash" });
    assert_eq!(hook_stdin(&payload, true), "{\"tool\":\"Bash\"}\n");
    assert_eq!(hook_stdin(&payload, false), "{\"tool\":\"Bash\"}");
}

// =============================================================================================
// Vocabulary
// =============================================================================================

#[test]
fn dialect_and_decision_spellings_are_the_wire_values() {
    assert_eq!(Dialect::ClaudeCode.as_str(), "claude-code");
    assert_eq!(Dialect::Codex.as_str(), "codex");
    assert_eq!(Dialect::ClaudeCode.to_string(), "claude-code");
    // Only the Claude dialect has a literal fast path.
    assert!(Dialect::ClaudeCode.treats_word_pattern_as_literal());
    assert!(!Dialect::Codex.treats_word_pattern_as_literal());

    assert_eq!(Decision::Approve.as_str(), "approve");
    assert_eq!(Decision::Allow.as_str(), "allow");
    assert_eq!(Decision::Block.as_str(), "block");
    assert_eq!(Decision::Deny.as_str(), "deny");
    assert_eq!(Decision::Ask.as_str(), "ask");

    assert_eq!(MergedDecision::Allow.as_str(), "allow");
    assert_eq!(MergedDecision::Ask.as_str(), "ask");
    assert_eq!(MergedDecision::Deny.as_str(), "deny");
    assert_eq!(MergedDecision::None.as_str(), "none");
}

// =============================================================================================
// End to end: a config selects hooks, the outputs merge, the records persist
// =============================================================================================

#[test]
fn a_configured_deny_reaches_the_merged_outcome_and_the_record() {
    let config = parse_codex_config(&json!({
        "PreToolUse": [
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": "deny.sh" }] },
            { "matcher": "Read", "hooks": [{ "type": "command", "command": "unused.sh" }] },
        ],
    }))
    .expect("a valid config");

    let groups = select_hook_groups(
        config.groups.get("PreToolUse").map(Vec::as_slice),
        "Bash",
        Dialect::Codex,
    );
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].hooks[0].command, "deny.sh");

    // The selected hook blocks.
    let output = parse_hook_output(Some(2), "", "no shell for you", Some("PreToolUse"));
    let merged = merge_hook_outputs(std::slice::from_ref(&output));
    assert_eq!(merged.decision, MergedDecision::Deny);
    assert_eq!(merged.reason.as_deref(), Some("no shell for you"));

    // And the pair the bridge would persist correlates by handler id.
    let invoked = hook_invoked_event(3, "PreToolUse", Dialect::Codex, "deny.sh", Some("Bash"));
    let result = hook_result_event(3, "PreToolUse", "deny.sh", &output, 500, 12);
    assert_eq!(invoked["handlerId"], result["handlerId"]);
    assert_eq!(invoked["turn"], result["turn"]);
    assert_eq!(result["decision"], json!("block"));
    assert_eq!(result["exitCode"], json!(2));
}

#[test]
fn a_hookless_selection_merges_to_none_and_still_records_nothing_meaningful() {
    // The empty path: no hook matched, so the point has no opinion and the caller proceeds. `None` is
    // distinguishable from `Allow` on purpose — "nobody objected" is not "someone approved".
    let merged = merge_hook_outputs(&[]);
    assert_eq!(merged.decision, MergedDecision::None);
    assert!(!merged.stop);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod hooks {
    pub use super::audit_gap as audit;
}
