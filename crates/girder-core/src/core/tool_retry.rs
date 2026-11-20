//! Retry decisions and corrective feedback for a tool call the model got wrong.
//!
//! Upstream validates model-generated arguments inside the tool body and throws `ToolArgsError`
//! (`INVALID_ARGS`); the executor turns that into a failed tool result, and the text reaches the
//! model unchanged. The retry — if there is one — is the model's own next turn, made blind: it
//! reads "invalid arguments: args.offset must be an integer" and has to guess which edit fixes the
//! call. This crate does the same today: [`crate::Violation`] is produced by the
//! validator and, as of this writing, is not acted on anywhere on the tool path — the JSON parse
//! failure at the top of `run_body` is reported as a one-line string and nothing classifies it.
//!
//! Two things are missing from that picture, and this module supplies both:
//!
//! - A classified failure ([`Failure`] / [`FailureReason`]) so a caller can decide whether another
//!   attempt is worth making at all, instead of treating every failure as equally retryable.
//! - A corrective message ([`corrective_message`]) that names the edit that fixes the call. A model
//!   told *what to change* fixes the call in one turn; a model told only that something was wrong
//!   tends to resend it with different whitespace.
//!
//! Nothing here runs a tool, touches a session, or knows a provider: the module is a pure decision
//! layer so that the retry arithmetic and the message wording can be tested without a loop.
//!
//! # Which failures deserve another attempt
//!
//! The test is *who can fix it*. Malformed JSON and a schema violation are both the model's own
//! writing: nothing ran, the arguments are still in front of it, and one corrected call usually
//! succeeds. A tool-internal error is the environment's, and resending the same call usually
//! reproduces it, so the conservative default is not to retry it (see the gap below for the one
//! exception).
//!
//! # Why the retryable list is strings
//!
//! [`Policy::retryable`] holds strings so a policy can come from configuration — a settings file, an
//! environment variable, a per-agent profile — without this module enumerating every source. The
//! price is that a misspelled name (`"SCHEMA_VIOLATON"`) matches nothing and silently disables the
//! retry it was meant to enable, which is the worst kind of bug: the feature looks configured. The
//! answer is [`Policy::validate`], which reports an unknown spelling *and* lists the valid names, so
//! a caller that validates its configuration at load time turns a silent no-op into a startup
//! error. [`should_retry`](Policy::should_retry) stays total — it never panics and never retries a
//! kind it does not recognise — because the decision runs on the failure path, where a panic would
//! lose the call.
//!
//! # Known gap: a timeout cannot be expressed
//!
//! [`FailureReason::ToolError`] is one class covering every failure raised *inside* a tool, and that
//! is not enough: a timeout is worth one more attempt (the work may simply have been slow) while a
//! deterministic internal error is not. The class cannot separate them, so `is_retryable` answers
//! `false` for both, and no `Policy` can opt a timeout back in. That is the conservative choice, not
//! the right one. The fix is a `Timeout` variant (this crate's `FailureKind` already has one), which
//! this task's agreed shape deliberately omits; until then, a caller that wants to retry timeouts
//! has to classify them before building the [`Failure`].

use std::fmt;

/// One failed call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// Name of the tool the model asked for.
    pub tool: String,
    /// Why the call failed.
    pub reason: FailureReason,
}

impl Failure {
    /// Builds a failure for one tool.
    #[must_use]
    pub fn new(tool: impl Into<String>, reason: FailureReason) -> Self {
        Self {
            tool: tool.into(),
            reason,
        }
    }

    /// Whether another attempt with a corrected call could plausibly succeed.
    ///
    /// Delegates to [`FailureReason::is_retryable`]; a policy also has to allow the kind, which is
    /// what [`Policy::should_retry`] checks.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.reason.is_retryable()
    }
}

/// Why a call failed.
///
/// The three variants follow the three points at which a call can fail, in call order: parsing the
/// model's JSON, checking the parsed arguments against the schema, and running the tool. Which point
/// failed decides who can fix it, which is the only question a retry policy cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// The arguments were not parseable as JSON, so no tool ran.
    MalformedJson {
        /// The parser's message. `serde_json` puts `line N column M` in it, which
        /// [`corrective_message`] lifts into a position the model can act on.
        detail: String,
    },
    /// The arguments parsed but did not satisfy the tool's schema, so no tool ran.
    SchemaViolation {
        /// What is wrong, with the path of the offending argument.
        message: String,
        /// The edit that would make the call valid.
        remedy: String,
    },
    /// The tool ran and raised an error. A timeout is reported here too; see the module docs.
    ToolError {
        /// The tool's message.
        message: String,
    },
}

impl FailureReason {
    /// Stable code for [`FailureReason::MalformedJson`].
    pub const MALFORMED_JSON: &'static str = "MALFORMED_JSON";

    /// Stable code for [`FailureReason::SchemaViolation`].
    pub const SCHEMA_VIOLATION: &'static str = "SCHEMA_VIOLATION";

    /// Stable code for [`FailureReason::ToolError`].
    pub const TOOL_ERROR: &'static str = "TOOL_ERROR";

    /// Every code this module understands, in the order a message should list them.
    pub const CODES: &'static [&'static str] = &[
        Self::MALFORMED_JSON,
        Self::SCHEMA_VIOLATION,
        Self::TOOL_ERROR,
    ];

    /// The stable code for this reason.
    ///
    /// A short, stable string rather than the message: a configured policy is persisted, and a
    /// message wording change must not silently stop a retry.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MalformedJson { .. } => Self::MALFORMED_JSON,
            Self::SchemaViolation { .. } => Self::SCHEMA_VIOLATION,
            Self::ToolError { .. } => Self::TOOL_ERROR,
        }
    }

    /// Whether another attempt could plausibly succeed.
    ///
    /// Argument failures are the model's own writing, so a corrected call is likely to work; a
    /// tool-internal failure is the environment's and resending the same call usually reproduces it.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::MalformedJson { .. } | Self::SchemaViolation { .. } => true,
            Self::ToolError { .. } => false,
        }
    }

    /// Whether `code` names a kind this module knows.
    ///
    /// The string table exists so [`Policy::validate`] can reject a misspelling with the list of
    /// valid names, instead of letting it match nothing.
    #[must_use]
    pub fn is_known_code(code: &str) -> bool {
        Self::CODES.contains(&code)
    }

    /// Whether `code` names a kind that is worth retrying on its own merits.
    ///
    /// Separate from [`Self::is_retryable`] because a policy is validated from a *string*: the check
    /// has to answer for a name that no [`FailureReason`] value exists for.
    #[must_use]
    pub fn is_retryable_code(code: &str) -> bool {
        matches!(code, Self::MALFORMED_JSON | Self::SCHEMA_VIOLATION)
    }
}

/// How many times a failed call may be attempted again, and for which kinds.
///
/// A retry costs a model turn, so the budget is exposed as a plain count rather than a duration
/// schedule: the delay between attempts is the provider's, not this layer's, and a caller that wants
/// backoff already has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// How many times one call may be retried, counting from one. `0` disables retries.
    pub max_retries: u32,
    /// The failure codes worth retrying. Names come from [`FailureReason::CODES`]; a name that is
    /// not in that list is rejected by [`Policy::validate`] rather than silently inert.
    pub retryable: Vec<&'static str>,
}

impl Policy {
    /// A policy that never retries.
    ///
    /// The empty list is deliberate and therefore valid: turning retries off is a configuration a
    /// caller may want, and it must not be reported as an error.
    #[must_use]
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            retryable: Vec::new(),
        }
    }

    /// The default: two attempts at the two failures the model can fix by rewriting its call.
    ///
    /// Two rather than one because a first correction sometimes reveals a second violation behind
    /// the first; two rather than many because a model that has not fixed its arguments after two
    /// corrections is stuck, and more attempts only spend the budget that a different approach needs.
    #[must_use]
    pub fn recommended() -> Self {
        Self {
            max_retries: 2,
            retryable: vec![
                FailureReason::MALFORMED_JSON,
                FailureReason::SCHEMA_VIOLATION,
            ],
        }
    }

    /// Checks that every configured code can actually take effect.
    ///
    /// Two ways a well-meaning entry does nothing: it is not a code this module knows (a
    /// misspelling, or a name from another version), or it is a known code that is never retryable
    /// on its own merits. Both are reported, because a policy that reads right and behaves wrong is
    /// harder to find than one that fails to load.
    ///
    /// A caller loading a policy from configuration should call this and treat an error as a
    /// startup failure; that is the whole point of the string list having a validator.
    ///
    /// # Errors
    ///
    /// Returns a [`PolicyError`] listing the unknown and the ineffective entries; it is empty when
    /// every entry is usable, and an empty `retryable` list is always usable.
    pub fn validate(&self) -> Result<(), PolicyError> {
        let mut unknown = Vec::new();
        let mut ineffective = Vec::new();
        for code in &self.retryable {
            if !FailureReason::is_known_code(code) {
                unknown.push((*code).to_owned());
            } else if !FailureReason::is_retryable_code(code) {
                ineffective.push((*code).to_owned());
            }
        }
        if unknown.is_empty() && ineffective.is_empty() {
            Ok(())
        } else {
            Err(PolicyError {
                unknown,
                ineffective,
            })
        }
    }

    /// Whether the attempt that just failed may be attempted again.
    ///
    /// `attempt` is one-based: the failure of the first attempt passes `1`. An attempt of `0` is not
    /// a failure, so it is never retried. The answer needs the kind to be both worth retrying on its
    /// own merits and allowed by this policy; for a validated policy the two agree, and for an
    /// unvalidated one the stricter answer keeps a deterministic failure from being retried.
    ///
    /// Total by construction: an unrecognised code simply does not match, which is why
    /// [`Policy::validate`] exists to catch the typo before it gets here.
    #[must_use]
    pub fn should_retry(&self, failure: &Failure, attempt: u32) -> bool {
        attempt >= 1
            && attempt <= self.max_retries
            && failure.reason.is_retryable()
            && self.retryable.contains(&failure.reason.code())
    }
}

/// Why a [`Policy`] cannot be used as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyError {
    /// Codes this module does not know, in the order they were configured.
    unknown: Vec<String>,
    /// Known codes that are never retryable, so configuring them has no effect.
    ineffective: Vec<String>,
}

impl PolicyError {
    /// The unrecognised code spellings.
    #[must_use]
    pub fn unknown(&self) -> &[String] {
        &self.unknown
    }

    /// The known codes that can never take effect.
    #[must_use]
    pub fn ineffective(&self) -> &[String] {
        &self.ineffective
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut problems = Vec::new();
        if !self.unknown.is_empty() {
            problems.push(format!(
                "unknown failure kind(s) {} (valid kinds: {})",
                quoted(self.unknown.iter()),
                quoted(FailureReason::CODES.iter())
            ));
        }
        if !self.ineffective.is_empty() {
            problems.push(format!(
                "failure kind(s) {} are never retryable, so listing them has no effect",
                quoted(self.ineffective.iter())
            ));
        }
        write!(formatter, "invalid retry policy: {}", problems.join("; "))
    }
}

impl std::error::Error for PolicyError {}

/// Formats names as a backticked, comma-separated list.
fn quoted<I, S>(names: I) -> String
where
    I: IntoIterator<Item = S>,
    S: fmt::Display,
{
    names
        .into_iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the message that tells the model how to fix its call.
///
/// The message is written for the model, not for a log: it names the tool, states what was wrong in
/// the terms the model can see (an argument path, a parser position), and ends with the edit to
/// make. Every variant closes with an instruction to change something, because the observed failure
/// mode of a bare error message is a resend with the same arguments.
#[must_use]
pub fn corrective_message(failure: &Failure) -> String {
    let tool = failure.tool.as_str();
    match &failure.reason {
        FailureReason::MalformedJson { detail } => {
            let position = parser_position(detail).map_or_else(
                || {
                    "The parser reported no position, so start from the first argument and check \
                     the JSON around it.\n"
                        .to_owned()
                },
                |(line, column)| {
                    format!(
                        "The parser stopped at line {line}, column {column}; the mistake is \
                         usually at or just before that point.\n"
                    )
                },
            );
            format!(
                "`{tool}` was not called: its arguments are not valid JSON.\n\
                 Parser detail: {detail}\n\
                 {position}\
                 Fix: resend the call with one JSON object as its arguments. The usual causes are a \
                 missing or extra comma, a missing closing quote or brace, a trailing comma before \
                 `}}` or `]`, and a double quote inside a string that is not escaped as \\\"."
            )
        }
        FailureReason::SchemaViolation { message, remedy } => format!(
            "`{tool}` was not called: its arguments do not match the tool's schema.\n\
             Problem: {message}\n\
             Fix: {remedy}\n\
             Resend the call with exactly that change; sending the same arguments again will fail \
             the same way."
        ),
        FailureReason::ToolError { message } => format!(
            "`{tool}` ran and failed: {message}\n\
             This is not an argument problem, so resending the same call will most likely fail the \
             same way. Read the message and change what the call depends on — the path, the \
             environment, or the tool's own configuration — and only retry if the failure looks \
             transient (a timeout, or a resource that was busy)."
        ),
    }
}

/// Extracts `line N column M` from a parser message.
///
/// `serde_json` is the parser on this path and its messages end in `at line N column M`, but the
/// detail is opaque text here: the scan finds the last position it can and gives up otherwise, and
/// [`corrective_message`] falls back to a generic hint rather than inventing a position.
fn parser_position(detail: &str) -> Option<(u32, u32)> {
    let after_line = detail
        .rfind("line ")
        .map(|at| &detail[at + "line ".len()..])?;
    let (line, rest) = leading_number(after_line)?;
    let after_column = rest
        .find("column ")
        .map(|at| &rest[at + "column ".len()..])?;
    let (column, _) = leading_number(after_column)?;
    Some((line, column))
}

/// Splits the leading ASCII digits off `text`, returning the number and the remainder.
fn leading_number(text: &str) -> Option<(u32, &str)> {
    let end = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, rest) = text.split_at(end);
    digits.parse().ok().map(|number| (number, rest))
}
