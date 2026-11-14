//! Approval rules: deciding, per tool call, whether it may run, is refused, or waits for a person.
//!
//! [`crate::permissions`] answers "what may this session do at all" with one bundle of knobs. That is the
//! right granularity for a user choosing a mode once, and the wrong granularity for the request "from now
//! on, ask me before anything in `bash` deletes something". That request is about *one tool* and *one
//! pattern of arguments*, and there is no way to express it with a switch that has three positions and no
//! knowledge of tool names.
//!
//! This module is that expression, and nothing else: a list of rules, and a pure function from
//! `(tool, arguments)` to a decision. It reads no files, spawns nothing, and holds no locks, so the same
//! rules give the same answer before and after a reload, and a test can pin the answer without a fixture.
//! Recording the decision, showing it to a person and enforcing it belong to the caller.
//!
//! # Why the last matching rule wins
//!
//! Rules are *overlays*, not a first-match table. A person configures "allow every `bash` command" first
//! because that is the baseline, and then "but ask me about `rm`" because that is the exception. Under
//! first-match-wins the second rule would never fire, and the user would have to rewrite the file with the
//! exception above the baseline to make it work at all — an order that reads backwards and that they must
//! maintain forever.
//!
//! Last-match-wins makes appending safe: every new rule means "and additionally, for the case I am about to
//! describe", and the newest sentence in the list is the one that governs. The cost is that appending a
//! *broad* rule after a narrow one widens the policy, which is why [`Decision::matched`] reports the index
//! of the rule that won: a UI can show the list newest-first and mark the live one, so a user who widened
//! their own policy by accident sees it rather than discovering it at the next tool call.
//!
//! # Why matching is a plain substring test, and what that cannot do
//!
//! The arguments arrive as the raw JSON the model produced. A precise rule would have to name a field, a
//! comparison and a value — a query language of its own — and the common request ("`rm` is dangerous") does
//! not need one. A substring over the raw text covers it, needs no configuration vocabulary, and cannot
//! silently disagree with itself.
//!
//! The price is exact and must be stated rather than hidden:
//!
//! * **False positives.** `rm` is contained in `charm`, `confirm`, `format`, `perform` and `uniform`, and a
//!   rule cannot tell a command from a filename, a comment, or an unrelated argument. A rule that matches
//!   `rm` therefore fires on `format`, on a path like `/tmp/confirm.txt`, and on a JSON field named
//!   `confirm` in a tool that never touches the shell. The failure direction is the safe one — a call that
//!   did not need review still waits for a person — but it is noise a user will notice.
//! * **False negatives.** Matching text cannot see through a shell: `r''m`, `\rm`, `RM`, a command read
//!   from a variable or a file, or `rm` split across a `$(...)` are all ways to delete a file without the
//!   substring appearing. This is not a sandbox and must not be presented as one; it is a policy about the
//!   spelling of a request. What actually confines a command is [`crate::shell`]'s sandbox mode and
//!   [`crate::exec`]'s path policy, and this module neither replaces nor strengthens either.
//! * **Case and whitespace are literal.** `RM` does not match `rm`, and `" rm"` does not match `echo rm`.
//!   Normalizing would be a guess about what a user meant, and a guess in a policy is worse than a string
//!   the user can read back and correct.
//! * **The whole argument blob is searched.** A substring in any field counts, including a field that
//!   merely mentions the word — see "false positives" above. A rule author who needs field precision does
//!   not get it here.
//!
//! # Why an unmatched call asks instead of proceeding
//!
//! The default must be the one that fails closed. If no rule matched meant "allow", then a typo in a tool
//! name, a rules file that failed to load, or a tool the user never thought about would each quietly permit
//! the call — and the person who wrote the policy would have no signal that it did not apply. Asking is
//! recoverable and visible: the call stops, a human reads what it wanted to do, and the missing rule gets
//! written. [`Decision::matched`] is `None` on that path so a caller can say "default policy" rather than
//! attributing the decision to a rule that does not exist.

/// What a rule does with a call it matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Run the call without asking.
    Allow,
    /// Refuse the call. The reason is shown to whoever asked, and to the model.
    Deny,
    /// Stop and wait for a person.
    Ask,
}

impl Action {
    /// The wire name, for a config file and for a log line.
    ///
    /// The name is the enum's own spelling so that a rule read back from disk and a rule built in memory
    /// cannot disagree about which action they mean.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// One rule: for this tool and these arguments, take this action, for this reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The tool this rule is about, matched exactly; `*` matches every tool.
    ///
    /// A blank value matches nothing rather than everything: an empty field is a mistake, and reading it as
    /// `*` would silently turn the narrow rule a user believed they wrote into one governing every tool.
    /// A rule that matches nothing falls through to the default, which asks — so the mistake is visible
    /// instead of being absorbed.
    pub tool: String,
    /// Substrings that must all appear in the arguments. Empty means "any arguments".
    ///
    /// All, not any: each extra substring narrows the rule. With "any", adding a substring would *widen* a
    /// rule, so a user tightening their policy would loosen it — the opposite of what the edit reads like.
    /// An empty substring is dropped when the rule is added, because the empty string is contained in every
    /// argument and would quietly turn the rule into an unrestricted one.
    pub contains: Vec<String>,
    /// What to do when the tool and every substring match.
    pub action: Action,
    /// Why the rule exists, in the user's terms. Shown wherever the decision is shown.
    ///
    /// It is not optional and it is not decoration: a prompt that says "approve this call" without saying
    /// which rule asked cannot be answered well, and a denial with no reason cannot be corrected. A blank
    /// reason is replaced when the rule is added rather than accepted, so a caller may rely on
    /// [`Decision::reason`] being non-empty.
    pub reason: String,
}

impl Rule {
    /// Whether this rule governs the call.
    ///
    /// Kept private: [`Rules::decide`] is the only place that may interpret a rule, so the precedence rule
    /// and the matching rule cannot drift apart in a second implementation.
    fn matches(&self, tool: &str, arguments: &str) -> bool {
        if self.tool != "*" && self.tool != tool {
            return false;
        }
        // `all` over an empty list is true, which is exactly "empty means unrestricted".
        self.contains
            .iter()
            .all(|needle| arguments.contains(needle.as_str()))
    }
}

/// What a call is allowed to do, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The action to take.
    pub action: Action,
    /// The index of the winning rule, in the order rules were added, or `None` for the default policy.
    ///
    /// The index rather than a copy of the rule: a caller that wants to show or edit the rule needs to name
    /// it in the list the user actually maintains, and `None` is what lets a UI distinguish "your rule said
    /// this" from "no rule you wrote applies".
    pub matched: Option<usize>,
    /// Why, in one line, non-empty in every case.
    pub reason: String,
}

/// The reason reported when no rule applies.
///
/// A constant rather than a sentence built per call, because a test asserts on it and a log greps for it,
/// and neither should break when the wording moves.
const DEFAULT_REASON: &str =
    "no approval rule matches this call, so the default is to ask a person";

/// A list of rules, evaluated newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    /// In the order rules were added. Read back to front, which is what makes the last one win.
    rules: Vec<Rule>,
}

impl Rules {
    /// An empty rule set.
    ///
    /// Empty means every call takes the default, which asks. A deployment that wants another default wraps
    /// this type instead of starting from a different one, so there is one place where "no rule" is defined.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a rule. Later rules take precedence over earlier ones.
    ///
    /// The rule is normalized as it is added: the tool name is trimmed, an empty substring is dropped, and a
    /// blank reason is replaced. Normalizing here rather than at decision time means the rules a caller reads
    /// back are the rules that will run, and [`Rule`]'s documented guarantees hold for a rule built by a
    /// struct literal as well as one parsed from a config file.
    pub fn add(&mut self, rule: Rule) {
        // Build the normalized rule from the parts rather than reassigning the fields: the trimmed tool name
        // is a fresh string, and writing it back over the original would allocate only to drop the old value.
        let tool = rule.tool.trim();
        let reason = rule.reason.trim();
        let reason = if reason.is_empty() {
            // Never silently invent a justification: say that the rule has none, and say what it does. A
            // reader still learns which rule is asking and what it decided.
            format!(
                "rule for tool `{tool}` requests `{}` and was added without a reason",
                rule.action.as_str()
            )
        } else {
            reason.to_owned()
        };
        self.rules.push(Rule {
            tool: tool.to_owned(),
            // An empty needle is contained in every string, so keeping it would turn a rule the user wrote
            // to narrow the policy into one that matches everything.
            contains: rule
                .contains
                .into_iter()
                .filter(|needle| !needle.is_empty())
                .collect(),
            action: rule.action,
            reason,
        });
    }

    /// Decides one call.
    ///
    /// Iterates newest first, so the rule added last that matches is the one that governs. Stops at the
    /// first match found that way, which is the same thing once precedence is "newest wins".
    #[must_use]
    pub fn decide(&self, tool: &str, arguments: &str) -> Decision {
        for (index, rule) in self.rules.iter().enumerate().rev() {
            if rule.matches(tool, arguments) {
                return Decision {
                    action: rule.action,
                    matched: Some(index),
                    reason: rule.reason.clone(),
                };
            }
        }
        Decision {
            action: Action::Ask,
            matched: None,
            reason: DEFAULT_REASON.to_owned(),
        }
    }

    /// How many rules are configured.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether there are no rules, in which case every call takes the default.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pieces an integration test cannot reach, namely the normalization `add` performs
    //! on a rule before it is stored.

    use super::{Action, Rule, Rules};

    fn rule(tool: &str, contains: &[&str], action: Action, reason: &str) -> Rule {
        Rule {
            tool: tool.to_owned(),
            contains: contains.iter().map(|needle| (*needle).to_owned()).collect(),
            action,
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn an_empty_needle_is_dropped_so_it_cannot_match_everything() {
        // The empty string is contained in every argument blob. A kept empty needle would make an intended
        // "only when rm appears" rule govern every call in the tool.
        let mut rules = Rules::new();
        rules.add(rule("bash", &["rm", ""], Action::Ask, "deletes"));

        assert_eq!(rules.decide("bash", "ls -la").matched, None);
        assert_eq!(rules.decide("bash", "rm a.txt").matched, Some(0));
    }

    #[test]
    fn the_tool_name_is_trimmed_before_it_is_stored() {
        // A pasted `"bash "` would otherwise match nothing, and a rule that silently never fires is the
        // failure mode the default-asks policy exists to make visible rather than hide.
        let mut rules = Rules::new();
        rules.add(rule("  bash  ", &[], Action::Allow, "trusted"));

        assert_eq!(rules.decide("bash", "ls").matched, Some(0));
    }

    #[test]
    fn a_blank_reason_is_replaced_rather_than_stored() {
        // A prompt with no reason cannot be answered well, so a caller may rely on a non-empty reason.
        let mut rules = Rules::new();
        rules.add(rule("bash", &["rm"], Action::Ask, "   "));

        let decision = rules.decide("bash", "rm a.txt");

        assert!(!decision.reason.trim().is_empty());
        assert!(decision.reason.contains("without a reason"));
        // It still names the rule and the action, so the reader learns what happened.
        assert!(decision.reason.contains("bash"));
        assert!(decision.reason.contains("ask"));
    }

    #[test]
    fn an_empty_rule_set_is_empty_and_decides_nothing() {
        let rules = Rules::new();

        assert!(rules.is_empty());
        assert_eq!(rules.len(), 0);
    }

    #[test]
    fn every_action_has_a_wire_name() {
        // The names are what a config file stores, so a rule read back must mean the same action.
        assert_eq!(Action::Allow.as_str(), "allow");
        assert_eq!(Action::Deny.as_str(), "deny");
        assert_eq!(Action::Ask.as_str(), "ask");
    }
}
