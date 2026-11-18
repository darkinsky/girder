//! The dialect-neutral hook protocol: matcher selection, output decoding, and outcome merging.
//!
//! # What upstream has
//!
//! `packages/experimental/hook-protocol` is 749 production lines and one of the few packages of that
//! size whose absence leaves no trace at all: it has **zero** named symbols in this crate —
//! `matchesMatcher`, `matcherDiagnostic`, `parseHookOutput`, `mergeHookOutputs`,
//! `selectHookGroups`, `summarizeStderr`, `DEFAULT_HOOK_TIMEOUT_MS` and `DEFAULT_STDERR_SUMMARY_MAX_CHARS`
//! all miss. Two bridges consume it (`hooks-claude-code`, 488 lines; `hooks-codex`, 414 lines), and
//! both are absent too. What *is* already here is only the durable half of the contract:
//! [`crate::migration_steps_gap`] declares the exact field set of `hook/invoked` and `hook/result`
//! (required `turn`/`point`/`dialect`/`handlerId`, optional `matcher`; required
//! `decision`/`durationMs`, optional `exitCode`/`stderrSummary`). So a bridge could not be written
//! that satisfied the schema, and nothing could produce a payload to check it with.
//!
//! # Why this protocol and not a bridge
//!
//! The three bridges' *dialect-specific* halves are payload construction (which JSON a `PreToolUse`
//! hook receives), environment rules, and the mapping from a neutral decision onto each
//! extension-point's typed decision. None of that can be verified here: it needs a live tool call in
//! a live turn, and it needs the Claude Code / Codex CLIs, which this machine does not have. What
//! *can* be verified is the part both bridges share, and that is this module:
//!
//! - **which hooks run** — the matcher dialect (`Claude` treats a word-and-pipe pattern as literal
//!   alternatives, `Codex` as a regex) and the match-all sentinels;
//! - **what a hook said** — exit code, stdout JSON, and stderr folded into one neutral
//!   [`HookOutput`], including the event-name guard that discards a block emitted for the wrong event;
//! - **what several hooks together said** — the `deny > ask > allow` fold;
//! - **what is recorded** — the `hook/invoked` / `hook/result` payloads, field for field.
//!
//! The four are pure functions over strings and JSON, so every rule below is pinned by a test rather
//! than by a subprocess. Splitting the protocol from the bridge is also what upstream did, for the
//! same reason: the two dialects disagree about payloads and agree about this.
//!
//! # The regex subset, stated up front
//!
//! There is no regular-expression engine in this crate and this module does not add one (a new
//! dependency is not a wiring decision this module may make). Matching therefore delegates to
//! [`girder_core::check_pattern`], the anchored/unanchored subset already used for JSON Schema
//! `pattern`: literals, character classes, `.`, and `*`/`+`, with `^`/`$` anchors. On top of it this
//! module adds **top-level alternation**, splitting on unescaped `|` and requiring every alternative
//! to be individually valid — the shape `Edit|Write` needs, which is the one that actually appears in
//! hook configs.
//!
//! The consequence is stated rather than hidden: a pattern outside that subset (`\d`, `\w`, `(…)`,
//! `?`, `{n,m}`, lookarounds) is **invalid**, which means [`matcher_diagnostic`] reports it at config
//! time and [`matches_matcher`] treats it as a non-match. Upstream's engine would run those
//! patterns. A bridge that silently accepted a pattern it could not evaluate would be worse: the
//! hook would never fire and nothing would say why. Reporting it as invalid is the failure mode a
//! config author can act on.
//!
//! # What is deliberately not ported
//!
//! - **`runHook`.** It is `runHook`'s shell plumbing (credential scrub, process groups, cancellation,
//!   timeout enforcement) that this crate already has in [`crate::shell`]; re-running a command here
//!   would be a second executor. [`hook_stdin`] and [`resolve_timeout_ms`] are the two decisions the
//!   runner makes *before* handing the command to a shell, and those are here.
//! - **`createDetachedRuns`.** Quiescence tracking over detached hook processes, which needs the
//!   process owner. Not a pure rule.
//! - **`appendHookInvoked`/`appendHookResult` as mutators.** A `Session` here has its own append path
//!   and event types; these are the *payload builders* ([`hook_invoked_event`],
//!   [`hook_result_event`]), which is the half whose field set is a contract and therefore testable.

use std::collections::BTreeMap;
use std::fmt;

use girder_core::PatternOutcome;
use girder_core::check_pattern;
use serde_json::{Map, Value, json};

// =============================================================================================
// Constants — the tables both bridges share
// =============================================================================================

/// The hook points the Claude Code bridge supports, in its own order.
///
/// Order is not cosmetic: it is the order a config's events are visited, so it decides the order of
/// `skipped` entries and of the parsed map. `Setup` and any future CC event are ignored rather than
/// refused, which is what allows a settings file written for a newer CC to boot.
pub const CLAUDE_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SubagentStart",
    "SubagentStop",
];

/// The five hook points the Codex bridge supports.
///
/// A strict subset of [`CLAUDE_EVENTS`] minus the subagent pair: Codex has no subagent hook points,
/// so a config naming them is ignored instead of being half-honoured.
pub const CODEX_EVENTS: [&str; 5] = [
    "PreToolUse",
    "PostToolUse",
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
];

/// The per-hook timeout both bridges apply when the config names none.
///
/// Ten minutes. Long enough for a hook that runs a build, short enough that a wedged hook cannot hold
/// a turn open indefinitely. It lives here, once, so the two bridges' `defaultTimeoutMs` defaults
/// cannot drift apart on it.
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 600_000;

/// The default character cap on a recorded `hook/result.stderrSummary`.
///
/// Five hundred characters: enough to carry a block reason, small enough that a hook which prints a
/// stack trace on every call does not turn the session log into its own transcript.
pub const DEFAULT_STDERR_SUMMARY_MAX_CHARS: usize = 500;

/// The exit code a hook uses to signal a blocking failure.
///
/// `2` is the only code with a decision attached: every other non-zero exit is a non-blocking error,
/// which is why a hook that fails to spawn must not be reported as a block.
const BLOCKING_EXIT_CODE: i32 = 2;

/// The events whose matcher field is discarded because the event has no matcher subject.
///
/// `UserPromptSubmit` and `Stop` fire on a prompt and on a turn end, neither of which is a tool name
/// or a session source, so a matcher there could only ever be a typo. Discarding it keeps a copied
/// `hooks.json` from silently filtering an event that cannot be filtered.
const UNMATCHED_EVENTS: [&str; 2] = ["UserPromptSubmit", "Stop"];

// =============================================================================================
// Vocabulary
// =============================================================================================

/// The bridge a hook belongs to, and the matcher dialect that bridge uses.
///
/// One enum rather than two because upstream's `HookDialect` and `MatcherMode` have exactly the same
/// members (`'claude-code' | 'codex'`) and the same spelling on the wire: the dialect *is* the matcher
/// mode. Two types would be two chances to pass the wrong one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// The Claude Code bridge: literal word-and-pipe matchers, `${CLAUDE_PLUGIN_ROOT}` substitution.
    ClaudeCode,
    /// The Codex bridge: every matcher is a regex, and no substitution is applied.
    Codex,
}

impl Dialect {
    /// The dialect's wire spelling, which is also the `hook/invoked.dialect` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// Whether a word-and-pipe matcher is literal alternation rather than a regex.
    ///
    /// True only for [`Self::ClaudeCode`]. This single predicate is the whole reason the two dialects
    /// need separate matcher handling.
    #[must_use]
    pub const fn treats_word_pattern_as_literal(self) -> bool {
        matches!(self, Self::ClaudeCode)
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One configured command hook — the `{type: 'command', command, timeout?}` shape both dialects share.
///
/// Non-command hook types (Claude Code's `prompt`, `agent`, `http`) never reach this struct: the
/// parsers record them as [`SkippedHook`] instead, because a prompt hook is a model call with no
/// process to run and pretending otherwise would hang a turn waiting for an exit code that cannot come.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandHook {
    /// The shell command line to run.
    pub command: String,
    /// Per-hook timeout in *seconds* (the wire unit), as the config wrote it.
    ///
    /// Kept as `f64` because that is what a JSON number is: rounding here would silently change a
    /// configured timeout, and [`resolve_timeout_ms`] is the one place the unit conversion happens.
    pub timeout_sec: Option<f64>,
}

/// One matcher group: a `matcher` pattern plus the command hooks it selects.
///
/// `matcher` is `None` for the three match-all spellings (absent, `""`, `"*"`). The distinction
/// between `Some("")` and `None` is preserved so [`matcher_diagnostic`] and
/// [`hook_invoked_event`] report what the config actually wrote.
#[derive(Clone, Debug, PartialEq)]
pub struct MatcherGroup {
    /// The configured pattern, when the config wrote one.
    pub matcher: Option<String>,
    /// The command hooks that run when the pattern matches, in config order.
    pub hooks: Vec<CommandHook>,
}

/// A hook entry a parser refused to run, kept so a bridge can warn about it.
///
/// Upstream's two bridges report different fields here (Claude Code the hook `type`, Codex a
/// `reason`); this is the union in the shape that prints: the event, and one sentence saying why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedHook {
    /// The event the entry was configured under.
    pub event: String,
    /// Why it was skipped, phrased for a log line.
    pub reason: String,
}

/// A parsed hook config: runnable groups per event, plus what was skipped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedConfig {
    /// Runnable groups, keyed by event name. An event with no runnable group is absent rather than
    /// present-and-empty, so a bridge can test membership without looking at the vector.
    pub groups: BTreeMap<String, Vec<MatcherGroup>>,
    /// Entries that were parsed and deliberately not run.
    pub skipped: Vec<SkippedHook>,
}

/// The `command` string substitutions a Claude Code config applies at parse time.
///
/// Codex applies none, which is why this is a parameter of the Claude parser alone. An unset field
/// leaves its token verbatim, so a config that mentions `${CLAUDE_PLUGIN_ROOT}` outside a plugin
/// still runs and the hook's own shell reports the unexpanded name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubstitutionVars<'a> {
    /// Replaces `${CLAUDE_PLUGIN_ROOT}`.
    pub plugin_root: Option<&'a str>,
    /// Replaces `${CLAUDE_PROJECT_DIR}`.
    pub project_dir: Option<&'a str>,
}

/// Why a hook config could not be accepted.
///
/// One variant's worth of shape — a message — because there is exactly one failure: a runnable group
/// whose matcher is not a pattern this build can evaluate. Accepting the config and never firing the
/// hook would be the silent alternative, and a hook that never fires is indistinguishable from one
/// whose command did nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// The diagnostic, already naming the event.
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// What one hook process said, folded into a dialect-neutral shape.
///
/// Every field past the three stream fields is optional because a hook may exercise any subset; the
/// bridge decides which are meaningful for its hook point. `stdout` stays verbatim and trimmed
/// because a clean exit may carry *plain* text that the bridge renders (Claude Code) or injects as
/// context (Codex) — the structured half is a bonus, not the only channel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HookOutput {
    /// The process exit code, or `None` when the hook could not be run at all.
    ///
    /// `None` rather than a synthetic non-zero code: a hook that never started produced no decision,
    /// and inventing an exit code would make an infrastructure fault look like a hook that said no.
    pub exit_code: Option<i32>,
    /// Trimmed stderr, verbatim.
    pub stderr: String,
    /// Trimmed stdout, verbatim.
    pub stdout: String,
    /// `false` means the hook asked to halt; `None` means it said nothing about continuing.
    pub continue_: Option<bool>,
    /// The human-readable reason shown when `continue_` is `false`.
    pub stop_reason: Option<String>,
    /// The neutral decision a hook expressed, if any.
    pub decision: Option<Decision>,
    /// The reason or explanation accompanying [`Self::decision`].
    pub reason: Option<String>,
    /// The event a `hookSpecificOutput` block claimed, when it claimed one.
    ///
    /// Recorded even when it does *not* match the firing event: the log should show what a malformed
    /// block claimed, which is the only clue that a copied hook is on the wrong event.
    pub hook_event_name: Option<String>,
    /// Extra context to inject into the next model request.
    pub additional_context: Option<String>,
    /// A warning to surface to the user.
    pub system_message: Option<String>,
    /// A tool-input rewrite the hook requested. Parsed, not honoured: input rewrite is a bridge
    /// feature this build does not have, and silently applying it would change a tool call the model
    /// made.
    pub updated_input: Option<Map<String, Value>>,
}

/// The permission decision a single hook expressed.
///
/// Five members rather than three because the two reference protocols keep two channels: the legacy
/// top-level `decision` (`approve`/`block`) and `hookSpecificOutput.permissionDecision`
/// (`allow`/`deny`/`ask`). Keeping them distinct is what lets [`merge_hook_outputs`] rank them
/// (`block` folds to deny, `approve` to allow) without losing what the hook actually wrote — the
/// recorded `hook/result.decision` carries the raw spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The top-level legacy `approve`.
    Approve,
    /// The `hookSpecificOutput.permissionDecision` `allow`.
    Allow,
    /// The top-level legacy `block`.
    Block,
    /// The `hookSpecificOutput.permissionDecision` `deny`.
    Deny,
    /// The `hookSpecificOutput.permissionDecision` `ask`.
    Ask,
}

impl Decision {
    /// The wire spelling, which is what `hook/result.decision` records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Allow => "allow",
            Self::Block => "block",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }

    /// The precedence rank: `deny`/`block` outrank `ask`, which outranks `allow`/`approve`.
    ///
    /// The number itself is private to the fold; only the ordering is a contract.
    #[must_use]
    const fn rank(self) -> u8 {
        match self {
            Self::Deny | Self::Block => 3,
            Self::Ask => 2,
            Self::Approve | Self::Allow => 1,
        }
    }
}

/// The single decision a hook point resolves to after merging every matched hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergedDecision {
    /// At least one hook permitted the action and none asked or objected.
    Allow,
    /// The strictest expression was a request for confirmation.
    Ask,
    /// At least one hook forbade the action.
    Deny,
    /// No hook expressed a decision — the caller treats this as "nothing to say".
    None,
}

impl MergedDecision {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
            Self::None => "none",
        }
    }
}

/// The folded outcome of every hook that matched one point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedOutcome {
    /// The most restrictive decision across all hooks.
    pub decision: MergedDecision,
    /// The winning rank's reasons, joined with a blank line.
    pub reason: Option<String>,
    /// Whether any hook asked to halt.
    pub stop: bool,
    /// The *first* halting hook's `stop_reason`.
    pub stop_reason: Option<String>,
    /// Every hook's `additional_context`, in hook order.
    pub additional_context: Vec<String>,
    /// Every hook's `system_message`, in hook order.
    pub system_messages: Vec<String>,
}

// =============================================================================================
// Matching
// =============================================================================================

/// True for the three match-all spellings: absent, empty, and `"*"`.
///
/// A single predicate because all three mean the same thing to both dialects, and a config author
/// writing `"*"` and one writing nothing should not get different behaviour.
#[must_use]
pub fn is_match_all(matcher: Option<&str>) -> bool {
    matches!(matcher, None | Some("" | "*"))
}

/// True for a pattern made only of ASCII word characters and `|` — the Claude Code literal shape.
///
/// This is the regex-versus-literal discriminator: Claude Code documents `Bash` as *the tool named
/// Bash*, not as `/Bash/`, so a pure word pattern must not be compiled as a regex. Empty is excluded
/// by the emptiness check that precedes every use, because `^[A-Za-z0-9_|]+$` requires one character.
#[must_use]
pub fn is_claude_literal(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '|'
        })
}

/// Splits a pattern on unescaped `|`, so `a\|b` stays one alternative.
///
/// Byte slicing is safe because both delimiters are ASCII, so every split point is a character
/// boundary. A trailing lone backslash is carried into the last alternative and rejected there by the
/// pattern parser, which is the same place an unbalanced escape is reported.
fn alternatives(pattern: &str) -> Vec<&str> {
    let bytes = pattern.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = index.saturating_add(2),
            b'|' => {
                parts.push(pattern.get(start..index).unwrap_or_default());
                index = index.saturating_add(1);
                start = index;
            }
            _ => index = index.saturating_add(1),
        }
    }
    parts.push(pattern.get(start..).unwrap_or_default());
    parts
}

/// Whether every alternative of `pattern` is inside the supported subset.
fn pattern_is_valid(pattern: &str) -> bool {
    alternatives(pattern).iter().all(|alternative| {
        !matches!(
            check_pattern(alternative, ""),
            PatternOutcome::BadPattern { .. }
        )
    })
}

/// Whether `matcher` selects `query` under the given dialect.
///
/// Match-all sentinels select everything. A Claude Code word-and-pipe pattern is exact-match
/// alternation, so `Bash` does **not** select `BashOutput`; every other pattern is an unanchored
/// regex, so Codex's `Bash` *does*. An invalid pattern is a non-match rather than a panic: a hook
/// matcher comes from a config file, and a config file must not be able to abort a turn.
#[must_use]
pub fn matches_matcher(matcher: Option<&str>, query: &str, dialect: Dialect) -> bool {
    if is_match_all(matcher) {
        return true;
    }
    let Some(pattern) = matcher else {
        return true;
    };
    if dialect.treats_word_pattern_as_literal() && is_claude_literal(pattern) {
        return pattern.split('|').any(|alternative| alternative == query);
    }
    let mut any_hit = false;
    for alternative in alternatives(pattern) {
        match check_pattern(alternative, query) {
            PatternOutcome::Ok => any_hit = true,
            PatternOutcome::NoMatch { .. } => {}
            // A pattern outside the subset cannot be shown *not* to match, so refuse it outright
            // rather than returning a match from the alternatives that happened to parse.
            PatternOutcome::BadPattern { .. } => return false,
        }
    }
    any_hit
}

/// Validates one matcher before a bridge accepts its config, returning a stable diagnostic.
///
/// The string is upstream's, character for character — `invalid <dialect> regex matcher "<pattern>"`
/// — because a bridge logs it and a person greps for it. The pattern is quoted as JSON, so a pattern
/// containing a quote stays readable instead of splitting the message.
#[must_use]
pub fn matcher_diagnostic(matcher: Option<&str>, dialect: Dialect) -> Option<String> {
    let pattern = matcher?;
    if is_match_all(Some(pattern)) {
        return None;
    }
    if dialect.treats_word_pattern_as_literal() && is_claude_literal(pattern) {
        return None;
    }
    if pattern_is_valid(pattern) {
        return None;
    }
    Some(format!(
        "invalid {} regex matcher {}",
        dialect.as_str(),
        json_string(pattern)
    ))
}

/// Selects configured groups that have command hooks and match the event subject.
///
/// Order is config order and the returned groups are the caller's own values (upstream returns the
/// original references), so a bridge can correlate a `hook/invoked` record with the group that
/// produced it. Groups with no hooks are dropped even when their matcher matches: a group whose
/// commands were all skipped runs nothing, and "matched but empty" is not a run.
#[must_use]
pub fn select_hook_groups<'a>(
    groups: Option<&'a [MatcherGroup]>,
    query: &str,
    dialect: Dialect,
) -> Vec<&'a MatcherGroup> {
    groups.map_or_else(Vec::new, |groups| {
        groups
            .iter()
            .filter(|group| {
                !group.hooks.is_empty() && matches_matcher(group.matcher.as_deref(), query, dialect)
            })
            .collect()
    })
}

// =============================================================================================
// Config parsing
// =============================================================================================

/// Parses a Claude Code hook config — either a settings object with a `hooks` key or the bare event map.
///
/// Malformed entries are ignored rather than failing boot, which is what keeps one bad row in a
/// user's settings file from bricking the harness. The single exception is a runnable group whose
/// matcher cannot be evaluated: that throws, because the alternative is a hook that silently never
/// fires. `Setup` and any event outside [`CLAUDE_EVENTS`] are ignored before their groups are
/// inspected, so a config written for a newer Claude Code still loads.
///
/// # Errors
///
/// Returns [`ConfigError`] naming the event when a group's matcher is not a supported pattern.
pub fn parse_claude_config(
    raw: &Value,
    vars: &SubstitutionVars<'_>,
) -> Result<ParsedConfig, ConfigError> {
    parse_config(raw, &CLAUDE_EVENTS, Dialect::ClaudeCode, vars)
}

/// Parses a Codex hook config — a `{hooks: …}` wrapper or the bare event map.
///
/// Codex performs no command substitution and additionally skips `async: true` commands, which are
/// recorded in [`ParsedConfig::skipped`] rather than run: a Codex async hook outlives the turn that
/// started it, and this build has no owner for a process that outlives its turn.
///
/// # Errors
///
/// Returns [`ConfigError`] naming the event when a group's matcher is not a supported pattern.
pub fn parse_codex_config(raw: &Value) -> Result<ParsedConfig, ConfigError> {
    parse_config(
        raw,
        &CODEX_EVENTS,
        Dialect::Codex,
        &SubstitutionVars::default(),
    )
}

/// Parses one dialect's config, which differs only in event table, substitution, and skip rules.
fn parse_config(
    raw: &Value,
    events: &[&str],
    dialect: Dialect,
    vars: &SubstitutionVars<'_>,
) -> Result<ParsedConfig, ConfigError> {
    let Some(root) = as_object(raw) else {
        return Ok(ParsedConfig::default());
    };
    // `{ hooks: { … } }` and the bare event map are both accepted, because the same file is read as a
    // settings fragment and as a standalone `hooks.json`.
    let hooks = root.get("hooks").and_then(as_object).unwrap_or(root);
    let mut parsed = ParsedConfig::default();
    for event in events {
        // Absent or not-an-array: nothing to run and nothing to report.
        let Some(groups) = hooks.get(*event).and_then(Value::as_array) else {
            continue;
        };
        let mut runnable = Vec::new();
        for raw_group in groups {
            if let Some(group) = parse_group(raw_group, event, dialect, vars, &mut parsed.skipped)?
            {
                runnable.push(group);
            }
        }
        if !runnable.is_empty() {
            parsed.groups.insert((*event).to_owned(), runnable);
        }
    }
    Ok(parsed)
}

/// Parses one matcher group, or `None` when it has nothing runnable.
///
/// Returning `Option` rather than an empty group is what makes upstream's
/// `if (commands.length === 0) continue` visible: a group with only skipped hooks must not be
/// inserted, because an inserted empty group would be indistinguishable from a configured no-op.
fn parse_group(
    raw_group: &Value,
    event: &str,
    dialect: Dialect,
    vars: &SubstitutionVars<'_>,
    skipped: &mut Vec<SkippedHook>,
) -> Result<Option<MatcherGroup>, ConfigError> {
    let Some(group) = as_object(raw_group) else {
        return Ok(None);
    };
    let Some(raw_hooks) = group.get("hooks").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut hooks = Vec::new();
    for raw_hook in raw_hooks {
        if let Some(hook) = parse_hook(raw_hook, event, dialect, vars, skipped) {
            hooks.push(hook);
        }
    }
    if hooks.is_empty() {
        return Ok(None);
    }
    // The matcher is discarded for an event with no matcher subject, so a copied config cannot
    // filter an event that has nothing to filter on.
    let matcher = if UNMATCHED_EVENTS.contains(&event) {
        None
    } else {
        group.get("matcher").and_then(Value::as_str)
    };
    if let Some(diagnostic) = matcher_diagnostic(matcher, dialect) {
        return Err(ConfigError {
            message: format!("{diagnostic} on event {}", json_string(event)),
        });
    }
    Ok(Some(MatcherGroup {
        matcher: matcher.map(str::to_owned),
        hooks,
    }))
}

/// Parses one hook entry, or `None` when it is malformed or must be skipped.
fn parse_hook(
    raw_hook: &Value,
    event: &str,
    dialect: Dialect,
    vars: &SubstitutionVars<'_>,
    skipped: &mut Vec<SkippedHook>,
) -> Option<CommandHook> {
    let hook = as_object(raw_hook)?;
    // A missing `type` is `command`: both reference formats default it that way.
    let kind = hook
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("command");
    if kind != "command" {
        skipped.push(SkippedHook {
            event: event.to_owned(),
            reason: format!("unsupported {} hook", json_string(kind)),
        });
        return None;
    }
    if dialect == Dialect::Codex && hook.get("async").and_then(Value::as_bool) == Some(true) {
        skipped.push(SkippedHook {
            event: event.to_owned(),
            reason: "async hook".to_owned(),
        });
        return None;
    }
    let command = hook.get("command").and_then(Value::as_str)?;
    let command = if dialect == Dialect::ClaudeCode {
        substitute_command(command, vars)
    } else {
        command.to_owned()
    };
    // Claude Code spells the per-hook timeout `timeout`; Codex accepts `timeout` or its `timeoutSec`
    // alias. Both are seconds.
    let timeout_sec = hook
        .get("timeout")
        .or_else(|| {
            if dialect == Dialect::Codex {
                hook.get("timeoutSec")
            } else {
                None
            }
        })
        .and_then(Value::as_f64);
    Some(CommandHook {
        command,
        timeout_sec,
    })
}

/// Applies `${CLAUDE_PLUGIN_ROOT}` / `${CLAUDE_PROJECT_DIR}` substitution to a command.
///
/// A token whose variable is unset stays verbatim, so the hook's own shell reports the unexpanded
/// name instead of running a command with an empty path. Every occurrence is replaced, because a
/// command may name the same root twice.
#[must_use]
pub fn substitute_command(command: &str, vars: &SubstitutionVars<'_>) -> String {
    let mut out = command.to_owned();
    if let Some(root) = vars.plugin_root {
        out = out.replace("${CLAUDE_PLUGIN_ROOT}", root);
    }
    if let Some(project) = vars.project_dir {
        out = out.replace("${CLAUDE_PROJECT_DIR}", project);
    }
    out
}

// =============================================================================================
// Decoding
// =============================================================================================

/// Decodes one hook's process outcome into a dialect-neutral [`HookOutput`].
///
/// The rules, in the order they apply:
///
/// 1. `exit_code == 2` is a block, with stderr (trimmed) as its reason. Structured stdout is
///    *ignored* on that path: stderr is authoritative, so a hook cannot block and also claim
///    `approve` in its stdout.
/// 2. Structured stdout is read only on a clean exit, and only when the trimmed text starts with
///    `{`. Anything else is plain output, which the bridge may still render or inject.
/// 3. Malformed JSON on a clean exit is lenient — no structured fields, no error — because the
///    reference engines treat it as plain text and a hook that prints a log line must not fail a turn.
/// 4. The legacy top-level `decision` is `approve`/`block` only. `allow`/`deny`/`ask` there is
///    invalid and ignored, because those belong to `hookSpecificOutput.permissionDecision`.
/// 5. A `hookSpecificOutput` block applies only when its `hookEventName` matches `expected_event_name`
///    (or when no expectation is given). A block for the wrong event, or with no discriminator, has its
///    event-scoped fields discarded — a stray `PreToolUse` deny must not deny a `Stop`.
///
/// This function is total: every input produces an output, because a hook is untrusted input.
#[must_use]
pub fn parse_hook_output(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    expected_event_name: Option<&str>,
) -> HookOutput {
    let trimmed_stderr = stderr.trim();
    let trimmed_stdout = stdout.trim();
    let mut output = HookOutput {
        exit_code,
        stderr: trimmed_stderr.to_owned(),
        stdout: trimmed_stdout.to_owned(),
        ..HookOutput::default()
    };
    if exit_code == Some(BLOCKING_EXIT_CODE) {
        output.decision = Some(Decision::Block);
        if !trimmed_stderr.is_empty() {
            output.reason = Some(trimmed_stderr.to_owned());
        }
    }
    if exit_code == Some(0) && trimmed_stdout.starts_with('{') {
        if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(trimmed_stdout) {
            apply_structured(&mut output, &parsed, expected_event_name);
        }
    }
    output
}

/// Folds a structured-stdout object into `output`, gated on the firing event.
fn apply_structured(
    output: &mut HookOutput,
    parsed: &Map<String, Value>,
    expected_event_name: Option<&str>,
) {
    if let Some(cont) = parsed.get("continue").and_then(Value::as_bool) {
        output.continue_ = Some(cont);
    }
    if let Some(stop_reason) = string_field(parsed, "stopReason") {
        output.stop_reason = Some(stop_reason.to_owned());
    }
    if let Some(system_message) = string_field(parsed, "systemMessage") {
        output.system_message = Some(system_message.to_owned());
    }
    // The legacy channel: `approve`/`block` only.
    if let Some(decision) = string_field(parsed, "decision").and_then(|value| match value {
        "approve" => Some(Decision::Approve),
        "block" => Some(Decision::Block),
        _ => None,
    }) {
        output.decision = Some(decision);
    }
    if let Some(reason) = string_field(parsed, "reason") {
        output.reason = Some(reason.to_owned());
    }

    let Some(Value::Object(specific)) = parsed.get("hookSpecificOutput") else {
        return;
    };
    let event_name = string_field(specific, "hookEventName");
    // Always record the discriminator, even on a mismatch: the log is the only place a copied hook's
    // wrong event name is visible.
    if let Some(event_name) = event_name {
        output.hook_event_name = Some(event_name.to_owned());
    }
    if let Some(expected) = expected_event_name {
        if event_name != Some(expected) {
            // A missing discriminator is as malformed as a mismatched one under the keyed schema.
            return;
        }
    }
    if let Some(decision) =
        string_field(specific, "permissionDecision").and_then(|value| match value {
            "allow" => Some(Decision::Allow),
            "deny" => Some(Decision::Deny),
            "ask" => Some(Decision::Ask),
            _ => None,
        })
    {
        output.decision = Some(decision);
    }
    if let Some(reason) = string_field(specific, "permissionDecisionReason") {
        output.reason = Some(reason.to_owned());
    }
    if let Some(context) = string_field(specific, "additionalContext") {
        output.additional_context = Some(context.to_owned());
    }
    if let Some(Value::Object(updated)) = specific.get("updatedInput") {
        output.updated_input = Some(updated.clone());
    }
}

// =============================================================================================
// Merging
// =============================================================================================

/// Folds every matched hook's output into one most-restrictive outcome.
///
/// Three rules, and each exists because the naive version silently weakens a policy:
///
/// - **`deny > ask > allow`.** Order-independent, so a config's ordering cannot change the answer.
///   `block` folds to deny and `approve` to allow, because the two channels express the same intent.
/// - **only the winning rank's reasons survive.** A hook that *allowed* the call must not have its
///   explanation attached to a deny: the reason a caller reads has to be the reason the outcome
///   happened. Same-rank reasons are joined with a blank line, because two independent objections are
///   two paragraphs.
/// - **the first halt wins.** `stop` is sticky and keeps the first halting hook's `stop_reason`;
///   a later hook's halt reason describes a halt that never took effect.
///
/// Context and warnings accumulate in hook order instead of being folded, because a bridge decides
/// how to inject them and dropping all but one would lose configuration the user wrote.
#[must_use]
pub fn merge_hook_outputs(outputs: &[HookOutput]) -> MergedOutcome {
    let mut max_rank = 0u8;
    // Kept per rank so that only the winning rank's objections can surface.
    let mut reasons: BTreeMap<u8, Vec<String>> = BTreeMap::new();
    let mut stop = false;
    let mut stop_reason: Option<String> = None;
    let mut additional_context = Vec::new();
    let mut system_messages = Vec::new();

    for output in outputs {
        let rank = output.decision.map_or(0, Decision::rank);
        max_rank = max_rank.max(rank);
        if let (3 | 2, Some(reason)) = (rank, output.reason.as_deref()) {
            if !reason.is_empty() {
                reasons.entry(rank).or_default().push(reason.to_owned());
            }
        }
        if output.continue_ == Some(false) && !stop {
            stop = true;
            if output.stop_reason.is_some() {
                stop_reason.clone_from(&output.stop_reason);
            }
        }
        if let Some(context) = output.additional_context.as_deref() {
            if !context.is_empty() {
                additional_context.push(context.to_owned());
            }
        }
        if let Some(message) = output.system_message.as_deref() {
            if !message.is_empty() {
                system_messages.push(message.to_owned());
            }
        }
    }

    let decision = match max_rank {
        3 => MergedDecision::Deny,
        2 => MergedDecision::Ask,
        1 => MergedDecision::Allow,
        _ => MergedDecision::None,
    };
    let reason = reasons
        .remove(&max_rank)
        .filter(|reasons| !reasons.is_empty())
        .map(|reasons| reasons.join("\n\n"));
    MergedOutcome {
        decision,
        reason,
        stop,
        stop_reason,
        additional_context,
        system_messages,
    }
}

// =============================================================================================
// Durable records
// =============================================================================================

/// Truncates a hook's stderr for a `hook/result.stderrSummary`.
///
/// Trimming happens before the cut so a padded message is not reported as "short enough". The cap is
/// *inclusive* — exactly `max_chars` characters are kept verbatim — because an off-by-one here would
/// append an ellipsis to a summary that lost nothing, and a reader cannot tell that from a real cut.
#[must_use]
pub fn summarize_stderr(stderr: &str, max_chars: usize) -> Option<String> {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= max_chars {
        return Some(trimmed.to_owned());
    }
    let mut kept: String = trimmed.chars().take(max_chars).collect();
    kept.push('…');
    Some(kept)
}

/// Builds the `hook/invoked` payload for one run.
///
/// Field for field what [`crate::migration_steps_gap`] already declares: `turn`, `point`, `dialect`,
/// `handlerId` required, `matcher` present only when the selecting group had one. An absent matcher is
/// omitted rather than written as `null`, because the schema's optional set is a closed set and a null
/// is a value.
#[must_use]
pub fn hook_invoked_event(
    turn: u64,
    point: &str,
    dialect: Dialect,
    handler_id: &str,
    matcher: Option<&str>,
) -> Value {
    let mut payload = json!({
        "turn": turn,
        "point": point,
        "dialect": dialect.as_str(),
        "handlerId": handler_id,
    });
    if let (Some(object), Some(matcher)) = (payload.as_object_mut(), matcher) {
        object.insert("matcher".to_owned(), Value::String(matcher.to_owned()));
    }
    payload
}

/// Builds the `hook/result` payload paired with [`hook_invoked_event`].
///
/// The recorded decision is the decoded decision, else `stop` when the hook asked to halt, else
/// `pass` — so every result says something a reader can act on, even when the hook said nothing.
/// An absent exit code and blank stderr are omitted for the same reason `matcher` is.
#[must_use]
pub fn hook_result_event(
    turn: u64,
    point: &str,
    handler_id: &str,
    output: &HookOutput,
    stderr_summary_max_chars: usize,
    duration_ms: u64,
) -> Value {
    let decision = output.decision.map_or_else(
        || {
            if output.continue_ == Some(false) {
                "stop"
            } else {
                "pass"
            }
        },
        Decision::as_str,
    );
    let mut payload = json!({
        "turn": turn,
        "point": point,
        "handlerId": handler_id,
        "decision": decision,
        "durationMs": duration_ms,
    });
    let Some(object) = payload.as_object_mut() else {
        return payload;
    };
    if let Some(exit_code) = output.exit_code {
        object.insert("exitCode".to_owned(), json!(exit_code));
    }
    if let Some(summary) = summarize_stderr(&output.stderr, stderr_summary_max_chars) {
        object.insert("stderrSummary".to_owned(), Value::String(summary));
    }
    payload
}

// =============================================================================================
// Runner decisions
// =============================================================================================

/// The timeout a hook runs under: its own `timeoutSec`, else the bridge's default.
///
/// Upstream multiplies by 1000 with JavaScript number semantics; this saturates at zero for a negative
/// value and truncates a fractional millisecond, so a config saying `0.001` yields `1`. Saturation
/// rather than a panic because this is config data arriving from a file.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the product is clamped into [0, u64::MAX] before the cast, and a timeout is a \
              wall-clock duration, not an exact quantity"
)]
pub fn resolve_timeout_ms(timeout_sec: Option<f64>, default_ms: u64) -> u64 {
    timeout_sec.map_or(default_ms, |seconds| {
        let millis = seconds * 1000.0;
        if !millis.is_finite() || millis <= 0.0 {
            return 0;
        }
        // 2^63 is exactly representable and far above any real timeout, so it is the saturation
        // point rather than a comparison against `u64::MAX as f64`, which would itself lose
        // precision on the way in.
        if millis >= 9_223_372_036_854_775_808.0 {
            return u64::MAX;
        }
        millis as u64
    })
}

/// The stdin text one hook receives: the JSON payload, plus a trailing newline when the dialect wants it.
///
/// Claude Code reads a newline-terminated payload and Codex does not, which is why this is a flag
/// rather than a constant: a `read -r` in a hook script would otherwise return a partial line.
#[must_use]
pub fn hook_stdin(payload: &Value, trailing_newline: bool) -> String {
    let text = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_owned());
    if trailing_newline {
        format!("{text}\n")
    } else {
        text
    }
}

// =============================================================================================
// JSON helpers
// =============================================================================================

/// A plain (non-null, non-array) object, else `None`.
fn as_object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

/// A string field of a parsed object, else `None`.
fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

/// `text` as a JSON string literal, which is how the diagnostic quotes a pattern or an event name.
fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
}
