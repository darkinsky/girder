//! Per-model call configuration: the limits one model call runs under, and the table that selects
//! them by model name.
//!
//! # Why a table rather than more provider fields
//!
//! `DeepSeekConfig` in `llm_deepseek` holds exactly one `max_tokens` for the whole provider, and no
//! timeout at all: `DeepSeekProvider::new` builds its client with
//! `reqwest::Client::builder().build()`, which leaves the request timeout unset. A deployment that
//! serves a fast chat model beside a slower reasoning model therefore cannot give them different
//! output limits or different deadlines, and a socket that never answers holds the turn until
//! something unrelated notices. Widening `DeepSeekConfig` would not help: the choice is per model,
//! not per process, so it needs a lookup keyed by model name.
//!
//! # Total timeout versus first-token timeout
//!
//! These two are not interchangeable, and the difference is the reason [`CallConfig`] has both.
//!
//! - `timeout` bounds the **whole** request, from dispatch until the last event of the stream. It
//!   has to be generous, because a long, useful answer legitimately takes a while to stream: at a
//!   default of 120 seconds it tolerates a large response.
//! - `first_token_timeout` bounds only the wait **before the first token arrives**. A provider that
//!   has accepted the connection and then stalled usually shows it there: no bytes for tens of
//!   seconds means it is not thinking slowly, it is not working at all.
//!
//! With only a total timeout, a hung model is indistinguishable from a slow one until the total
//! elapses: at 120 seconds that is two minutes of a turn spent waiting for nothing. With a
//! 20-second first-token timeout the same stall is reported in 20 seconds, and the caller can retry
//! or fail over while the answer is still worth having. The total timeout still applies afterwards,
//! so a stream that delivers one token and then hangs is caught as well.
//!
//! A first-token timeout that is not shorter than the total timeout cannot fire, so [`ConfigTable::set`]
//! rejects it instead of storing a value that silently does nothing.
//!
//! # Matching rules
//!
//! Patterns are **globs**, not prefixes, with `*` as the only metacharacter: `*` matches any
//! (possibly empty) run of characters, everything else — including `?` — is literal. A prefix rule
//! is the common case and is written `deepseek-*`; the glob form costs nothing extra and covers the
//! cases a prefix cannot, such as `*-reasoner` or `deepseek-*-pro`.
//!
//! When more than one pattern matches, the most specific one wins:
//!
//! 1. A pattern without any `*` (an exact match) beats every pattern with one. `deepseek-chat` is
//!    more specific about that model than `deepseek-*` can ever be, whatever their lengths.
//! 2. Otherwise the pattern with the most literal (non-`*`) characters wins, so `deepseek-chat-*`
//!    beats `deepseek-*`, which beats `*`.
//! 3. Otherwise — two patterns of equal specificity — the most recently set pattern wins. A config
//!    reload that restates a rule is meant to take effect, and a deterministic tie-break keeps
//!    [`ConfigTable::resolve`] stable regardless of iteration order.
//!
//! A model that matches nothing gets the table's default configuration, so `resolve` never fails;
//! a deployment only describes exceptions.

use std::time::Duration;

/// Configuration for one model call.
///
/// Every field is a ceiling or a default for a single request, not for the process. `None` means
/// "let the provider decide", which is why the optional fields are not filled in here: duplicating
/// the provider's own default in this type would let the two drift apart, and a call that says
/// nothing is better than one that says 8192 because that used to be right.
#[derive(Clone, Debug, PartialEq)]
pub struct CallConfig {
    /// Maximum output tokens.
    ///
    /// `None` defers to the provider's configured default (`DeepSeekConfig::max_tokens`).
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    ///
    /// `None` defers to the provider's default. Sampling is a per-task decision — a summarizer and
    /// a code generator want different values — so this type carries no opinion of its own.
    pub temperature: Option<f32>,
    /// Deadline for the entire request, stream included.
    ///
    /// See the module documentation for how this differs from
    /// [`first_token_timeout`](Self::first_token_timeout).
    pub timeout: Duration,
    /// Deadline for the wait before the first token arrives.
    ///
    /// Must be strictly shorter than [`timeout`](Self::timeout), otherwise it can never fire and
    /// the stall it exists to catch is only reported at the total deadline.
    pub first_token_timeout: Duration,
    /// Additional attempts after the first one fails.
    ///
    /// The total number of attempts is this value plus one, and `0` means no retry. This is the
    /// same convention as `RetryPolicy::max_retries`, so the two layers cannot disagree about what
    /// "5" means — and the worst-case wall-clock cost of a call is `(retries + 1) * timeout`, which
    /// is the product an operator has to reason about when raising either field.
    pub retries: u32,
}

/// Default configuration for a call that no rule describes.
///
/// The numbers are chosen to be defensible without tuning:
///
/// - `timeout` is 120 seconds, the same default `ExecOptions` uses in this crate for a command
///   whose duration nobody knows. It is long enough for a large streamed answer and short enough
///   that a dead connection is noticed within a couple of minutes.
/// - `first_token_timeout` is 20 seconds: a healthy provider starts streaming within a second or
///   two, and 20 seconds still absorbs queueing and a cold start while reporting a stall six times
///   sooner than the total deadline.
/// - `retries` is 5, matching both `RetryPolicy::default().max_retries` and upstream's
///   `DEFAULT_MAX_RETRIES`, so a deployment that sets neither does not end up with two retry
///   budgets that disagree.
/// - `max_tokens` and `temperature` are `None`, leaving both to the provider.
impl Default for CallConfig {
    fn default() -> Self {
        Self {
            max_tokens: None,
            temperature: None,
            timeout: Duration::from_secs(120),
            first_token_timeout: Duration::from_secs(20),
            retries: 5,
        }
    }
}

/// Why a pattern or a configuration was refused.
///
/// All of these are configuration mistakes that would otherwise be discovered at request time, as a
/// timeout that never fires or a rule that can never match.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The pattern was empty, so it could never match a model name.
    #[error("call-config pattern is empty; use `*` to match every model")]
    EmptyPattern,

    /// The first-token timeout was not shorter than the total timeout.
    ///
    /// Storing this would make `first_token_timeout` dead weight: the total deadline always wins,
    /// which is the behaviour the field was added to avoid.
    #[error(
        "first_token_timeout ({first_token_timeout:?}) must be shorter than timeout ({timeout:?})"
    )]
    FirstTokenNotShorter {
        /// The total request timeout.
        timeout: Duration,
        /// The first-token timeout that was at least as long.
        first_token_timeout: Duration,
    },

    /// A timeout was zero.
    ///
    /// Zero means "no time at all", so every call would fail instantly; a deployment that wants no
    /// deadline needs that expressed explicitly, not as an accidental zero.
    #[error("`{field}` must be greater than zero")]
    ZeroTimeout {
        /// Which timeout was zero.
        field: &'static str,
    },

    /// The temperature was NaN or infinite.
    ///
    /// It cannot be sent as JSON, so it would fail later, at serialization, with a message that
    /// points at the request rather than at the configuration.
    #[error("temperature must be a finite number")]
    NonFiniteTemperature,
}

/// One rule: a pattern and the configuration it selects.
#[derive(Clone, Debug)]
struct Rule {
    /// The glob as written, kept for the "most recently set" tie-break and for re-setting.
    pattern: String,
    /// Number of literal characters, the specificity used by rule 2 of the module documentation.
    literals: usize,
    /// Whether the pattern contains a `*`, the exact-match test used by rule 1.
    wildcard: bool,
    /// The configuration this rule selects.
    config: CallConfig,
}

/// A set of rules keyed by model-name pattern, plus the fallback.
///
/// Lookup is linear in the number of rules: a deployment describes a handful of exceptions, and a
/// map keyed by pattern would still have to test every glob, so an ordered list is both simpler and
/// honest about the cost.
#[derive(Clone, Debug)]
pub struct ConfigTable {
    /// The configuration used when no rule matches.
    default: CallConfig,
    /// The rules, in the order they were set.
    rules: Vec<Rule>,
}

impl ConfigTable {
    /// Builds a table with no rules, so every model gets `default`.
    #[must_use]
    pub fn new(default: CallConfig) -> Self {
        Self {
            default,
            rules: Vec::new(),
        }
    }

    /// The fallback configuration.
    #[must_use]
    pub fn default_config(&self) -> &CallConfig {
        &self.default
    }

    /// Number of rules in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether the table holds no rules, so every model gets the default.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Sets the configuration for models matching `pattern`.
    ///
    /// `pattern` is a glob in which `*` matches any run of characters and every other character is
    /// literal; see the module documentation for which rule wins when several match. Setting the
    /// same pattern again replaces the earlier configuration and makes it the most recent, so a
    /// reloaded rule takes effect even when a tie-break decides between it and another pattern.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::EmptyPattern`] for an empty pattern,
    /// [`ConfigError::ZeroTimeout`] when either timeout is zero,
    /// [`ConfigError::FirstTokenNotShorter`] when the first-token timeout is not shorter than the
    /// total one, and [`ConfigError::NonFiniteTemperature`] for a NaN or infinite temperature.
    pub fn set(&mut self, pattern: &str, config: CallConfig) -> Result<(), ConfigError> {
        if pattern.is_empty() {
            return Err(ConfigError::EmptyPattern);
        }
        if config.timeout.is_zero() {
            return Err(ConfigError::ZeroTimeout { field: "timeout" });
        }
        if config.first_token_timeout.is_zero() {
            return Err(ConfigError::ZeroTimeout {
                field: "first_token_timeout",
            });
        }
        if config.first_token_timeout >= config.timeout {
            return Err(ConfigError::FirstTokenNotShorter {
                timeout: config.timeout,
                first_token_timeout: config.first_token_timeout,
            });
        }
        if config.temperature.is_some_and(|value| !value.is_finite()) {
            return Err(ConfigError::NonFiniteTemperature);
        }

        let literals = pattern.bytes().filter(|byte| *byte != b'*').count();
        let wildcard = pattern.contains('*');
        let rule = Rule {
            pattern: pattern.to_owned(),
            literals,
            wildcard,
            config,
        };
        match self
            .rules
            .iter()
            .position(|existing| existing.pattern == pattern)
        {
            // Replacing keeps the list short and moves the rule to "most recent", which is what the
            // third tie-break reads.
            Some(index) => {
                self.rules.remove(index);
                self.rules.push(rule);
            }
            None => self.rules.push(rule),
        }
        Ok(())
    }

    /// The configuration a model should use.
    ///
    /// Falls back to the table's default when nothing matches, so a caller never needs to handle
    /// "unknown model" separately from "known model with no rule".
    #[must_use]
    pub fn resolve(&self, model: &str) -> &CallConfig {
        let mut best: Option<(bool, usize, usize)> = None;
        let mut chosen = &self.default;
        for (index, rule) in self.rules.iter().enumerate() {
            if !glob_matches(&rule.pattern, model) {
                continue;
            }
            // Rule 1: exact beats wildcard. Rule 2: more literals is more specific. Rule 3: later
            // wins, which the index supplies. Tuple comparison is lexicographic, so one key
            // expresses all three without a chain of hand-written comparisons.
            let key = (!rule.wildcard, rule.literals, index);
            if best.is_none_or(|current| key > current) {
                best = Some(key);
                chosen = &rule.config;
            }
        }
        chosen
    }
}

impl Default for ConfigTable {
    /// An empty table holding [`CallConfig::default`].
    fn default() -> Self {
        Self::new(CallConfig::default())
    }
}

/// Whether the glob `pattern` matches `text`.
///
/// Hand-written rather than pulled from a regex crate: the language is one metacharacter, the
/// inputs are short model ids, and the dependency would be larger than the function. The two
/// pointers plus one remembered star give linear-time matching for the patterns this table
/// actually contains, while still allowing backtracking for `deepseek-*-pro`-shaped rules.
///
/// Matching is byte-wise. `*` and every other metacharacter are ASCII, and a multi-byte UTF-8
/// sequence never contains an ASCII byte, so comparing bytes cannot split a character and needs no
/// decoding.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut p, mut t) = (0, 0);
    // Where the last `*` was, and the text position it is currently assumed to have consumed up to.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            // The star consumes one more byte and matching resumes just after it.
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    // Trailing stars may match the empty remainder.
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
