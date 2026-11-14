//! Tests for approval rules.
//!
//! The property the design rests on is *overlay* precedence: a rule added later governs over one added
//! earlier, so "allow every `bash` command" followed by "ask me about `rm`" asks rather than allows. A
//! first-match table would make the second rule dead, and the user would have to keep their file ordered
//! backwards forever.
//!
//! The second property is that an unmatched call asks. A policy file that failed to load, or a rule whose
//! tool name has a typo, must not fall through to "run it".

use girder_core::{
    ApprovalAction as Action, ApprovalDecision as Decision, ApprovalRule as Rule,
    ApprovalRules as Rules,
};

/// A rule, spelled out, so a test reads as the configuration a user would write.
fn rule(tool: &str, contains: &[&str], action: Action, reason: &str) -> Rule {
    Rule {
        tool: tool.to_owned(),
        contains: contains.iter().map(|needle| (*needle).to_owned()).collect(),
        action,
        reason: reason.to_owned(),
    }
}

/// The scenario from the product request: trust the shell, except for deletion.
fn trust_shell_but_ask_about_rm() -> Rules {
    let mut rules = Rules::new();
    rules.add(rule(
        "bash",
        &[],
        Action::Allow,
        "the shell is trusted here",
    ));
    rules.add(rule(
        "bash",
        &["rm"],
        Action::Ask,
        "`rm` deletes files and cannot be undone",
    ));
    rules
}

// ---- one rule ---------------------------------------------------------------------------------

#[test]
fn a_matching_rule_decides_the_call() {
    let mut rules = Rules::new();
    rules.add(rule("bash", &["rm"], Action::Deny, "deletion is forbidden"));

    let decision = rules.decide("bash", "rm -rf build");

    assert_eq!(decision.action, Action::Deny);
    assert_eq!(decision.matched, Some(0));
    assert_eq!(decision.reason, "deletion is forbidden");
}

#[test]
fn a_rule_for_another_tool_does_not_apply() {
    // The tool name is exact, because a rule written for `bash` was written by someone thinking about a
    // shell, not about every tool whose arguments happen to contain the same text.
    let mut rules = Rules::new();
    rules.add(rule("bash", &["rm"], Action::Deny, "deletion is forbidden"));

    let decision = rules.decide("write_file", "rm");

    assert_eq!(decision.action, Action::Ask, "an unmatched call asks");
    assert_eq!(decision.matched, None);
}

#[test]
fn a_rule_matches_only_when_every_substring_appears() {
    // Multiple `contains` entries are combined with AND: each one narrows the rule. OR would mean that
    // adding a substring widens a policy, so an edit that reads like a tightening would loosen it.
    let mut rules = Rules::new();
    rules.add(rule(
        "bash",
        &["git", "push"],
        Action::Ask,
        "pushing publishes work other people can see",
    ));

    // Both present, in either order: the rule is about content, not about where the words sit.
    assert_eq!(
        rules.decide("bash", "git push origin main").action,
        Action::Ask
    );
    assert_eq!(rules.decide("bash", "git -C repo push").action, Action::Ask);

    // Only one of the two: not a match, so the default applies.
    assert_eq!(rules.decide("bash", "git status").matched, None);
    assert_eq!(rules.decide("bash", "push").matched, None);
}

// ---- precedence: the rule added last wins -----------------------------------------------------

#[test]
fn a_later_rule_overrides_an_earlier_one_narrower_than_it() {
    // The core scenario. Under first-match-wins the `rm` rule would never fire.
    let rules = trust_shell_but_ask_about_rm();

    let deleted = rules.decide("bash", "rm -rf target");
    let other = rules.decide("bash", "cargo build");

    assert_eq!(deleted.action, Action::Ask);
    assert_eq!(
        deleted.matched,
        Some(1),
        "the `rm` rule is the one that won"
    );
    assert_eq!(other.action, Action::Allow);
    assert_eq!(
        other.matched,
        Some(0),
        "a call the `rm` rule does not match falls back to the earlier rule"
    );
}

#[test]
fn a_later_rule_overrides_an_earlier_one_even_when_it_is_broader() {
    // Appending is not always narrowing. It is still the last word, because the newest sentence in the list
    // is the one the user just wrote, and an override that sometimes ignored the latest edit would be
    // unpredictable. `matched` names the winner so a UI can show what widened the policy.
    let mut rules = Rules::new();
    rules.add(rule("bash", &["rm"], Action::Deny, "deletion is forbidden"));
    rules.add(rule(
        "bash",
        &[],
        Action::Allow,
        "the shell is trusted here",
    ));

    let decision = rules.decide("bash", "rm -rf build");

    assert_eq!(decision.action, Action::Allow);
    assert_eq!(decision.matched, Some(1));
}

#[test]
fn the_last_matching_rule_wins_among_many() {
    // Three rules that all match: the newest of the matching ones decides, not the newest overall and not
    // the first. This is what makes the answer independent of how many irrelevant rules follow.
    let mut rules = Rules::new();
    rules.add(rule("bash", &["rm"], Action::Allow, "first"));
    rules.add(rule("bash", &[], Action::Deny, "second"));
    rules.add(rule("bash", &["rm"], Action::Ask, "third"));
    rules.add(rule("bash", &["ls"], Action::Allow, "does not match"));

    let decision = rules.decide("bash", "rm a.txt");

    assert_eq!(decision.action, Action::Ask);
    assert_eq!(decision.matched, Some(2));
    assert_eq!(decision.reason, "third");
}

#[test]
fn a_newer_tool_wildcard_overrides_an_older_tool_specific_rule() {
    // Precedence is about the list, not about specificity: a later `*` rule covers a tool that an earlier
    // exact rule named. Specificity ordering would make the answer depend on a comparison the user cannot
    // see, and there is no total order over "how specific is this rule" that a person can predict.
    let mut rules = Rules::new();
    rules.add(rule("bash", &[], Action::Deny, "no shell"));
    rules.add(rule("*", &[], Action::Allow, "trusted session"));

    assert_eq!(rules.decide("bash", "ls").matched, Some(1));
    assert_eq!(rules.decide("read_file", "a.txt").matched, Some(1));
}

// ---- wildcards and unrestricted `contains` ----------------------------------------------------

#[test]
fn a_star_tool_rule_governs_every_tool() {
    let mut rules = Rules::new();
    rules.add(rule("*", &["secret"], Action::Deny, "secrets are not read"));

    assert_eq!(rules.decide("read_file", "secret.txt").matched, Some(0));
    assert_eq!(rules.decide("bash", "cat secret.txt").matched, Some(0));
    // Still subject to its `contains`, so it is not an unconditional allow.
    assert_eq!(rules.decide("bash", "ls").matched, None);
}

#[test]
fn an_empty_contains_list_matches_any_arguments() {
    // Empty means "this tool, whatever the arguments", including the empty argument string.
    let mut rules = Rules::new();
    rules.add(rule(
        "bash",
        &[],
        Action::Allow,
        "the shell is trusted here",
    ));

    assert_eq!(rules.decide("bash", "").matched, Some(0));
    assert_eq!(
        rules.decide("bash", r#"{"command":"make -j8"}"#).matched,
        Some(0)
    );
}

#[test]
fn a_star_tool_with_empty_contains_is_an_unconditional_rule() {
    let mut rules = Rules::new();
    rules.add(rule("*", &[], Action::Deny, "this session permits nothing"));

    assert_eq!(rules.decide("bash", "ls").action, Action::Deny);
    assert_eq!(rules.decide("read_file", "a.txt").action, Action::Deny);
}

// ---- the default when nothing matches ---------------------------------------------------------

#[test]
fn an_empty_rule_set_asks() {
    // Asking is the recoverable default, and the visible one: a rules file that failed to load, or a tool
    // name with a typo, stops the call instead of quietly permitting it.
    let rules = Rules::new();

    let decision = rules.decide("bash", "rm -rf /");

    assert_eq!(decision.action, Action::Ask);
    assert_eq!(
        decision.matched, None,
        "no rule may be blamed for a default"
    );
    assert!(
        !decision.reason.trim().is_empty(),
        "a default prompt must still say why it is asking"
    );
}

#[test]
fn a_blank_tool_name_matches_nothing_rather_than_everything() {
    // An empty field is a mistake. Reading it as `*` would turn a narrow rule into one governing every
    // tool, so it falls through to the default instead.
    let mut rules = Rules::new();
    rules.add(rule("", &[], Action::Allow, "meant to be narrow"));

    assert_eq!(rules.decide("bash", "ls").matched, None);
    assert_eq!(rules.decide("bash", "ls").action, Action::Ask);
}

// ---- the decision carries a reason ------------------------------------------------------------

#[test]
fn every_decision_has_a_non_empty_reason() {
    // The approval UI displays this. A blank reason would ask a person to approve something with no
    // explanation, so the guarantee holds for both paths: a matched rule and the default.
    let cases: [(&str, &str); 3] = [
        ("bash", "rm -rf build"),
        ("bash", "cargo build"),
        ("read_file", "a.txt"),
    ];
    let rules = trust_shell_but_ask_about_rm();

    for (tool, arguments) in cases {
        let decision: Decision = rules.decide(tool, arguments);
        assert!(
            !decision.reason.trim().is_empty(),
            "`{tool} {arguments}` produced no reason"
        );
    }
}

#[test]
fn the_reason_is_the_matching_rules_own_reason() {
    // The prompt has to explain *why this* call is being questioned, so the reason travels with the rule
    // rather than being generated from the tool name.
    let rules = trust_shell_but_ask_about_rm();

    assert_eq!(
        rules.decide("bash", "rm -rf build").reason,
        "`rm` deletes files and cannot be undone"
    );
    assert_eq!(
        rules.decide("bash", "cargo build").reason,
        "the shell is trusted here"
    );
}

// ---- the substring limitation, pinned as behaviour --------------------------------------------

#[test]
fn a_substring_rule_also_fires_on_words_that_merely_contain_it() {
    // Documented, not fixed: `format` contains `rm`, so a rule about `rm` also stops `format`. The failure
    // direction is the safe one — a call that needed no review waits for a person — and the alternative, a
    // regex or a field query, is a configuration language this module deliberately does not have.
    let rules = trust_shell_but_ask_about_rm();

    for harmless in [
        "cargo fmt --check",
        "df --format=json",
        "ls /tmp/confirm.txt",
    ] {
        // `cargo fmt --check` has no `rm`; the other two do. Assert the exact split so a change to the
        // matching rule cannot quietly redefine what "contains" means.
        let expected = if harmless.contains("rm") {
            Action::Ask
        } else {
            Action::Allow
        };
        assert_eq!(
            rules.decide("bash", harmless).action,
            expected,
            "`{harmless}`"
        );
    }

    assert_eq!(
        rules.decide("bash", "format --help").action,
        Action::Ask,
        "`format` contains `rm`"
    );
}

#[test]
fn matching_is_literal_about_case() {
    // Not normalized: a policy the user can read back verbatim is easier to trust than one that guesses
    // what they meant by `RM`.
    let mut rules = Rules::new();
    rules.add(rule("bash", &["rm"], Action::Ask, "deletion"));

    assert_eq!(rules.decide("bash", "RM -rf build").matched, None);
}

// ---- the rule set reports what it holds --------------------------------------------------------

#[test]
fn the_rule_count_grows_with_each_rule_added() {
    let mut rules = Rules::new();
    assert!(rules.is_empty());

    rules.add(rule("bash", &[], Action::Allow, "trusted"));
    rules.add(rule("*", &["secret"], Action::Deny, "secrets"));

    assert_eq!(rules.len(), 2);
    assert!(!rules.is_empty());
}
