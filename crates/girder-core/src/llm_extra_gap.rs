//! Provider-neutral LLM failure classification: the code vocabulary and the classifiers behind it.
//!
//! Upstream paths: `packages/llm/llm/src/error.ts` (the code table and the two prose classifiers) and
//! `packages/llm/llm-deepseek/src/transport.ts` (status and prose folded into one code).
//!
//! # Why this module exists
//!
//! This build already had the *shape* of retry — [`crate::RetryPolicy`], [`crate::Backoff`], and the
//! [`crate::FailureCode`] strings — but nothing that produced a code from a provider's answer. The
//! one classifier that did exist, `retrying_provider`'s private `classify_provider`, matches
//! substrings of a rendered message (`"429"`, `"overloaded"`, `"timed out"`) and answers
//! `"PERMANENT"` for everything else. So the three failures a caller most needs to tell apart all
//! collapsed into the same non-retryable bucket:
//!
//! | Provider says | Correct code | Retry? | Why it matters |
//! | --- | --- | --- | --- |
//! | "context length exceeded" | `CONTEXT_WINDOW_EXCEEDED` | never | the answer is to compact, not to wait |
//! | "insufficient_quota" | `QUOTA` | never | the answer is to bill, not to wait |
//! | "max context length is 128000" | `CONTEXT_WINDOW_EXCEEDED` | never | same, worded differently |
//!
//! Both of the first two are *retryable-looking* to a message scan: an exhausted quota arrives as
//! HTTP 429, which `classify_provider` maps to `RATE_LIMIT`, so the loop backs off five times against
//! a balance that will not refill. That is the bug this module closes, and it is why the classifiers
//! here are written against the *specific* wordings rather than against `429`.
//!
//! # Why prose is matched at all, and why narrowly
//!
//! Providers do not agree on a machine-readable field for "your context is too long": OpenAI sends
//! `code: "context_length_exceeded"`, Anthropic sends a `message` only, and gateway proxies forward
//! either as opaque text. HTTP status alone cannot separate it from a malformed request — both are
//! 400 — so the wording is the only evidence available and matching it is unavoidable.
//!
//! The risk of matching prose is the opposite error: a request rejected for an unrelated reason is
//! classified as context overflow, the caller compacts or truncates, and a bug that would have been
//! visible becomes a silent behaviour change. So every pattern here is deliberately narrow, and the
//! narrowness is pinned by the negative cases in the tests: `"input exceeds maximum allowed value"`
//! (a parameter out of range, not a context overrun) and `"context window size must be positive"` (a
//! configuration error) both must come back `false`. The `exceeds` family is the one that needs care
//! — "the input exceeds X" is only about context when X is the context — which is why that pattern
//! requires an explicit `context` token within a bounded distance on both sides.
//!
//! # What is *not* matched
//!
//! Nothing here retries, waits, compacts, or truncates. A `Classification` is evidence a policy acts
//! on; deciding that `CONTEXT_WINDOW_EXCEEDED` means "compact and try once more" belongs to the loop,
//! and deciding that `QUOTA` means "stop and ask the user to pay" belongs to the surface. Keeping the
//! module free of those decisions is what lets both read the same answer.
//!
//! # Why the vocabulary is a table rather than an enum
//!
//! Upstream's `LLM_FAILURE_CODES` is `as const satisfies Readonly<Record<string, { retryChoice?: true }>>`:
//! an open set that a provider plugin can extend, with a closed set of *choices* offered to a retry
//! policy. `HTTP_${number}` is a code that cannot be enumerated at all, so the Rust counterpart has to
//! be a string, and a Rust `enum` would either drop that case or need a catch-all variant that makes
//! every `match` lie. [`FAILURE_CODES`] is therefore the closed part of the vocabulary, [`is_known`]
//! answers membership, and unknown strings are legal — they are simply not offered as policy choices.

use std::collections::BTreeSet;

/// Canonical code: the request was rejected because it exceeds the model's context window.
pub const CONTEXT_WINDOW_EXCEEDED: &str = "CONTEXT_WINDOW_EXCEEDED";

/// Canonical code: the account's quota, balance, or budget is exhausted.
///
/// Deliberately distinct from `RATE_LIMIT`: a rate limit refills on its own, a balance does not, and
/// treating one as the other turns "ask the user to top up" into a five-attempt backoff that cannot
/// succeed.
pub const QUOTA: &str = "QUOTA";

/// Canonical code: an account token is over its first-party quota and can be replenished through the
/// billing page rather than by a new key.
pub const ACCOUNT_QUOTA: &str = "ACCOUNT_QUOTA";

/// Canonical code: the response completed normally and carried no content at all.
///
/// Providers occasionally emit a degenerate completion — a terminal stop with zero blocks — and an
/// empty assistant message silently ends the turn with nothing for the loop to act on. It is safe to
/// retry because the attempt produced nothing durable, which is why it is a default retry choice.
pub const EMPTY_RESPONSE: &str = "EMPTY_RESPONSE";

/// Canonical code: the credential is present but malformed, not absent.
///
/// The fix differs from `MISSING_CREDENTIAL` — correct the stored value rather than supply one — and
/// it fails identically on every attempt, so it is never a retry choice.
pub const INVALID_CREDENTIAL: &str = "INVALID_CREDENTIAL";

/// Canonical code: the provider rejected the credential.
pub const AUTH: &str = "AUTH";

/// Canonical code: the request is malformed for a reason other than size.
pub const INVALID_REQUEST: &str = "INVALID_REQUEST";

/// Canonical code: the provider failed on its own side.
pub const SERVER: &str = "SERVER";

/// Canonical code: the provider is limiting request rate, not account balance.
pub const RATE_LIMIT: &str = "RATE_LIMIT";

/// Canonical code: the stream ended before the provider's terminal event.
pub const STREAM_CLOSED: &str = "STREAM_CLOSED";

/// Canonical code: the stream carried a frame that is not valid for the protocol.
pub const MALFORMED_RESPONSE: &str = "MALFORMED_RESPONSE";

/// Canonical code: the transport failed before or during the request.
pub const TRANSPORT: &str = "TRANSPORT";

/// Canonical code: the request timed out.
pub const TIMEOUT: &str = "TIMEOUT";

/// Canonical code: no provider is registered for the route.
pub const NO_ADAPTER: &str = "NO_ADAPTER";

/// Canonical code: the encoded request would need more of its images offloaded before it fits.
///
/// The failure's `offload_images` names how many more of the *oldest* retained occurrences must go.
pub const IMAGE_OFFLOAD_REQUIRED: &str = "IMAGE_OFFLOAD_REQUIRED";

/// Canonical code: nothing more specific is known.
pub const UNKNOWN: &str = "UNKNOWN";

/// Every failure code the built-in providers and the LLM runtime may raise.
///
/// The list mirrors upstream's `LLM_FAILURE_CODES` key set exactly, in definition order. It is not
/// exhaustive of the codes that may be *observed*: an unclassified provider status becomes
/// `HTTP_<number>`, and a provider plugin may raise a code of its own. Membership therefore answers
/// "does this build know the meaning of this code", never "is this code legal".
pub const FAILURE_CODES: [&str; 52] = [
    "ABORTED",
    "ACCOUNT_QUOTA",
    "ACCOUNT_SIGN_IN_REQUIRED",
    "ACCOUNT_TOKEN_INVALID",
    "AUTH",
    "CLIENT_UPGRADE_REQUIRED",
    "CONTEXT_WINDOW_EXCEEDED",
    "DISCOVERY_FAILED",
    "DISCOVERY_UNSUPPORTED",
    "DUPLICATE_ADAPTER",
    "DUPLICATE_DIRECTORY",
    "DUPLICATE_DISCOVERY",
    "EMPTY_RESPONSE",
    "FILES_API",
    "IMAGE_OFFLOAD_REQUIRED",
    "INVALID_ADAPTER",
    "INVALID_CATALOG",
    "INVALID_CONFIG",
    "INVALID_CREDENTIAL",
    "INVALID_DIRECTORY",
    "INVALID_DISCOVERY",
    "INVALID_MODEL_CONTEXT",
    "INVALID_MODEL_INFO",
    "INVALID_MODEL_MAX_TOKENS",
    "INVALID_MODEL_REASONING",
    "INVALID_PREPARED_CALL",
    "INVALID_REPLAY_STATE",
    "INVALID_REQUEST",
    "INVALID_RESPONSE",
    "MALFORMED_RESPONSE",
    "MISSING_CREDENTIAL",
    "MODEL_SWITCH_COMPACTION_FAILED",
    "MODEL_UNSUPPORTED",
    "NO_ADAPTER",
    "NO_CREDENTIAL_STORE",
    "NO_DISCOVERY",
    "PI_AI_ERROR",
    "QUOTA",
    "RATE_LIMIT",
    "REGISTRATION_DISPOSED",
    "REQUEST_EXTENSION",
    "REQUEST_HEADERS",
    "SERVER",
    "STREAM_CLOSED",
    "TIMEOUT",
    "TRANSPORT",
    "UNKNOWN",
    "UNKNOWN_MODEL",
    "UNSTORABLE_PROVIDER_ID",
    "UNSUPPORTED_CONTENT",
    "UNSUPPORTED_OPTION",
    "UNSUPPORTED_REASONING_EFFORT",
];

/// The codes upstream offers as choices for a retry policy's `retryable_codes`.
///
/// Being a choice does not make a code retried by default: it only means the settings surface lists
/// it. Note that this is a superset of [`DEFAULT_RETRYABLE`] — `MALFORMED_RESPONSE`, `QUOTA`, and
/// `STREAM_CLOSED` are selectable but not selected, because retrying an exhausted balance is exactly
/// the mistake the `QUOTA` code exists to prevent.
pub const RETRY_CODE_CHOICES: [&str; 8] = [
    EMPTY_RESPONSE,
    MALFORMED_RESPONSE,
    QUOTA,
    RATE_LIMIT,
    SERVER,
    STREAM_CLOSED,
    TIMEOUT,
    TRANSPORT,
];

/// The codes a retry policy retries when it is not configured otherwise.
///
/// This is [`crate::RetryPolicy::default`]'s list, kept here as the provider-side statement of the
/// same fact so the two cannot drift apart silently.
pub const DEFAULT_RETRYABLE: [&str; 5] = [
    EMPTY_RESPONSE,
    RATE_LIMIT,
    SERVER,
    TIMEOUT,
    TRANSPORT,
];

/// The largest delay a scheduled timer accepts, in milliseconds.
///
/// A `setTimeout` above this fires immediately on some runtimes; the same ceiling bounds a
/// `retry-after` this build is willing to report, so a provider asking for a week cannot turn into an
/// infinite wait.
pub const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

/// Whether this build knows the meaning of `code`.
#[must_use]
pub fn is_known(code: &str) -> bool {
    FAILURE_CODES.contains(&code)
}

/// Whether `code` may be offered as a retry-policy choice.
#[must_use]
pub fn is_retry_choice(code: &str) -> bool {
    RETRY_CODE_CHOICES.contains(&code)
}

/// Whether a retry policy retries `code` without being configured to.
#[must_use]
pub fn is_default_retryable(code: &str) -> bool {
    DEFAULT_RETRYABLE.contains(&code)
}

/// What one failure's wording says about which non-transient condition it is.
///
/// Both flags are independent rather than an enum because the two classifiers answer two different
/// questions and upstream composes them by precedence, not by exclusion. A detail that reads as both
/// is reported as both, and [`classify_signal`] applies the precedence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Classification {
    /// The request exceeds the model's context window.
    pub context_window_exceeded: bool,
    /// The account's quota, balance, or budget is exhausted.
    pub quota_exceeded: bool,
}

impl Classification {
    /// Whether either condition was recognized.
    #[must_use]
    pub fn any(self) -> bool {
        self.context_window_exceeded || self.quota_exceeded
    }
}

/// Classify one provider detail string.
///
/// `detail` is the provider's error code, type, and message joined with spaces — the shape upstream
/// hands its classifiers. Join them before calling: a classifier that saw only the message would miss
/// OpenAI's `code: "context_length_exceeded"`, which carries the clearest evidence in the payload.
#[must_use]
pub fn classify(detail: &str) -> Classification {
    Classification {
        context_window_exceeded: is_context_window_exceeded(detail),
        quota_exceeded: is_quota_exceeded(detail),
    }
}

/// Whether `detail` identifies a request that exceeds the model's context window.
///
/// The accepted wordings are upstream's, and each exists for a reason:
///
/// - `context_length_exceeded`, `context-window-overflowed` — a structured code with the words in it.
/// - `maximum context length is 128000 tokens` — OpenAI's prose, which names no code.
/// - `input is too long for this model`, `request too large for model context` — request-size wording
///   that ties "too large" to the model.
/// - `input exceeds the model context window limit` — an "exceeds" whose object is the context.
///
/// See the module docs for why the last one is bounded on both sides.
#[must_use]
pub fn is_context_window_exceeded(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    let words = words(&lower);
    structured_overflow(&lower, &words)
        || maximum_context(&lower, &words)
        || too_large_for_context(&lower, &words)
        || too_long_for_model(&lower, &words)
        || exceeds_model_context(&lower, &words)
}

/// Whether `detail` identifies an exhausted account quota rather than a transient rate limit.
///
/// Only terminal wording counts: a quota that was *reached*, a balance that was *depleted*, a budget
/// that ran *out*. `"rate limit reached"` is not matched even though it shares the word "reached",
/// because it refills on its own and retrying is the right answer — that is the distinction this
/// function exists to make, and the reason it is separate from [`crate::FailureCode::RATE_LIMIT`].
#[must_use]
pub fn is_quota_exceeded(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    let words = words(&lower);
    insufficient_quota(&lower, &words)
        || quota_reached(&lower, &words)
        || exceeded_quota(&lower, &words)
        || balance_depleted(&lower, &words)
        || out_of_budget(&lower, &words)
}

/// Everything known about one failed request attempt.
///
/// This is upstream's `LlmFailure`: a detached, serializable bundle, so a failure can be written to a
/// session log and read back with its machine-routing code intact. `code` is a `String` rather than
/// an enum for the reason given in the module docs — `HTTP_<number>` and provider-specific codes are
/// legal members.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderError {
    /// Human-readable failure, in the provider's own words where available.
    pub message: String,
    /// Stable provider-neutral routing code.
    pub code: String,
    /// HTTP status, when the failure had one.
    pub status: Option<u16>,
    /// Provider-requested delay before the next attempt, in milliseconds.
    pub provider_retry_after_ms: Option<u64>,
    /// Opaque provider-issued request identifier, for support escalation.
    pub request_id: Option<String>,
    /// With [`IMAGE_OFFLOAD_REQUIRED`]: how many more retained images must be offloaded.
    pub offload_images: Option<u32>,
}

/// Why a [`ProviderError`] is not a valid failure fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Violation {
    /// The message is empty, so a reader would see a code and nothing to act on.
    EmptyMessage,
    /// The code is empty, so a policy could not route on it.
    EmptyCode,
    /// The status is outside the three-digit range a response can carry.
    StatusOutOfRange,
    /// A retry delay of zero is not a delay; it must be absent or positive.
    NonPositiveRetryAfter,
    /// A request id is present but empty, which is not an identity.
    EmptyRequestId,
    /// An offload count of zero asks for nothing.
    NonPositiveOffload,
}

impl ProviderError {
    /// Builds a failure that carries only a message and a code.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
            ..Self::default()
        }
    }

    /// Checks the invariants a failure fact must hold before it is written to a log.
    ///
    /// The validation is upstream's `failureSnapshot`: a payload that violates it is treated as
    /// unusable and replaced by a generic failure, rather than being trusted because it is shaped
    /// like one. Validating on *write* is what keeps a hostile or buggy provider payload from
    /// becoming a durable fact a later reader has no way to question.
    ///
    /// # Errors
    ///
    /// Returns the first [`Violation`] found, so the message names the field to fix.
    pub fn validate(&self) -> Result<(), Violation> {
        if self.message.is_empty() {
            return Err(Violation::EmptyMessage);
        }
        if self.code.is_empty() {
            return Err(Violation::EmptyCode);
        }
        if let Some(status) = self.status {
            if !(100..=599).contains(&status) {
                return Err(Violation::StatusOutOfRange);
            }
        }
        if self.provider_retry_after_ms == Some(0) {
            return Err(Violation::NonPositiveRetryAfter);
        }
        if self.request_id.as_deref() == Some("") {
            return Err(Violation::EmptyRequestId);
        }
        if self.offload_images == Some(0) {
            return Err(Violation::NonPositiveOffload);
        }
        Ok(())
    }
}

/// The evidence one failed attempt left behind.
#[derive(Clone, Copy, Debug, Default)]
pub struct Signal<'a> {
    /// HTTP status, when the failure preceded or accompanied a response.
    pub status: Option<u16>,
    /// Provider error `type`, for example `rate_limit_error`.
    pub kind: &'a str,
    /// Provider error `code`, for example `context_length_exceeded`.
    pub code: &'a str,
    /// Provider error `message`.
    pub message: &'a str,
    /// Raw `retry-after` header value, if the provider sent one.
    pub retry_after: Option<&'a str>,
    /// Provider-issued request id, from whichever header the provider uses.
    pub request_id: Option<&'a str>,
    /// Current Unix time in milliseconds, used to resolve an HTTP-date `retry-after`.
    pub now_unix_ms: u64,
}

/// Folds a signal into one provider-neutral failure.
///
/// The precedence is upstream's, and its order is load-bearing rather than arbitrary:
///
/// 1. **Authentication first.** A 401 whose body mentions a quota is still an auth failure; telling
///    the user to top up an account whose key is wrong wastes their time.
/// 2. **Quota before rate limit.** An exhausted balance arrives as 429. Testing the status first
///    would classify it as retryable, which is the failure mode this whole module exists to avoid.
/// 3. **Rate limit before context.** A 429 on a context overrun is the provider asking the caller to
///    slow down, and backoff is the correct answer even when the body mentions length.
/// 4. **Context before invalid request.** Both are 400, and only one of them has a remedy.
/// 5. **Server last before the status bucket**, so an unrecognized status still carries its number.
///
/// A status that no rule claims becomes `HTTP_<number>` rather than `SERVER`: an unclassified 418 is a
/// fact, and flattening it into "server error" loses the only routing evidence there was.
#[must_use]
pub fn classify_signal(signal: &Signal<'_>) -> ProviderError {
    let detail = join_detail(signal.kind, signal.code, signal.message);
    let classification = classify(&detail);
    let code = if matches!(signal.status, Some(401 | 403))
        || matches!(signal.kind, "authentication_error" | "permission_error")
    {
        AUTH
    } else if classification.quota_exceeded || signal.status == Some(402) {
        QUOTA
    } else if signal.status == Some(429) || signal.kind == "rate_limit_error" {
        RATE_LIMIT
    } else if classification.context_window_exceeded {
        CONTEXT_WINDOW_EXCEEDED
    } else if matches!(signal.status, Some(400 | 413)) || signal.kind == "invalid_request_error" {
        INVALID_REQUEST
    } else if signal.status.is_some_and(|status| status >= 500)
        || matches!(signal.kind, "api_error" | "overloaded_error")
    {
        SERVER
    } else if let Some(status) = signal.status {
        // A status the rules above did not claim. `format!` rather than a `match` arm per code: the
        // set is every three-digit number, so enumerating it is not possible and not useful.
        return ProviderError {
            code: format!("HTTP_{status}"),
            message: message_of(signal),
            status: Some(status),
            provider_retry_after_ms: retry_after(signal),
            request_id: request_id(signal),
            offload_images: None,
        };
    } else {
        SERVER
    };
    ProviderError {
        code: code.to_owned(),
        message: message_of(signal),
        status: signal.status,
        provider_retry_after_ms: retry_after(signal),
        request_id: request_id(signal),
        offload_images: None,
    }
}

/// Joins the provider's code, type, and message into the one string the classifiers read.
///
/// Public because a caller that already holds the three fields separately should not have to invent
/// the same joining rule, and because the order is part of the contract: the structured `code` comes
/// first so a classifier that matches on a code never depends on the message's phrasing.
#[must_use]
pub fn join_detail(kind: &str, code: &str, message: &str) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    for part in [kind, code, message] {
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts.join(" ")
}

/// Resolves a `retry-after` header value to a delay in milliseconds.
///
/// Both wire forms are accepted, because providers use both and a client that understood only one
/// would either ignore a provider's request to wait or wait forever:
///
/// - **delta-seconds**, with an optional fraction: `120` is two minutes, `0.5` is half a second.
/// - **HTTP-date**, an IMF-fixdate such as `Sun, 06 Nov 1994 08:49:37 GMT`, whose delay is the
///   distance to `now_unix_ms`. A date already in the past is not a delay, so it yields `None`.
///
/// `None` is also the answer for a value that cannot be parsed, for a zero delay, and for a delay that
/// exceeds [`MAX_TIMER_DELAY_MS`] — an unusable instruction is dropped rather than approximated. The
/// caller's own backoff then applies, which is the behaviour that cannot be wrong in either direction.
#[must_use]
pub fn parse_retry_after(value: &str, now_unix_ms: u64) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let delay = delta_seconds_ms(trimmed).or_else(|| {
        let at = http_date_ms(trimmed)?;
        at.checked_sub(now_unix_ms)
    })?;
    if delay == 0 || delay > MAX_TIMER_DELAY_MS {
        return None;
    }
    Some(delay)
}

/// Parses `delta-seconds` — digits with an optional fractional part — into milliseconds.
///
/// Integer arithmetic on the fraction: `0.5` is 500 ms and `1.25` is 1250 ms. Parsing through `f64`
/// would round, and a sub-millisecond remainder is not a unit any timer can honour anyway; digits past
/// the third are truncated rather than rounded, because rounding *up* would sleep longer than the
/// provider asked for.
fn delta_seconds_ms(value: &str) -> Option<u64> {
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (value, None),
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds: u64 = whole.parse().ok()?;
    let millis = match fraction {
        // A trailing `.` with nothing after it is not the grammar the header specifies.
        Some(digits) => {
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut padded = String::with_capacity(3);
            padded.extend(digits.chars().take(3));
            while padded.len() < 3 {
                padded.push('0');
            }
            padded.parse::<u64>().ok()?
        }
        None => 0,
    };
    seconds.checked_mul(1000)?.checked_add(millis)
}

/// Month number for a three-letter English month abbreviation.
fn month_number(name: &str) -> Option<u32> {
    let month = match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    Some(month)
}

/// Whether `year` is a leap year in the Gregorian calendar.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in one month of one year, so `31 Feb` is rejected instead of rolling into March.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since the Unix epoch for a Gregorian civil date.
///
/// Howard Hinnant's `days_from_civil`, which is exact for the whole proleptic Gregorian calendar and
/// involves no table and no iteration. Integer division of negatives is avoided by shifting the era
/// base, which is what makes it correct before 1970 as well.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * i64::from(shifted_month) + 2) / 5 + i64::from(day) - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parses an IMF-fixdate into Unix milliseconds.
///
/// Accepts the fixed-length form HTTP requires, `Sun, 06 Nov 1994 08:49:37 GMT`, and the same without
/// the leading weekday. The two obsolete date forms RFC 9110 still allows are *not* accepted: no
/// current provider sends them, and guessing at a malformed date is worse than falling back to the
/// local backoff.
fn http_date_ms(value: &str) -> Option<u64> {
    // Drop the optional `Sun,` prefix; the weekday is not needed to compute an instant.
    let rest = match value.find(',') {
        Some(index) => &value[index + 1..],
        None => value,
    };
    let mut fields = rest.split_whitespace();
    let day: u32 = fields.next()?.parse().ok()?;
    let month = month_number(fields.next()?)?;
    let year: i64 = fields.next()?.parse().ok()?;
    let time = fields.next()?;
    if fields.next() != Some("GMT") || fields.next().is_some() {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let mut clock = time.split(':');
    let hour: u32 = clock.next()?.parse().ok()?;
    let minute: u32 = clock.next()?.parse().ok()?;
    let second: u32 = clock.next()?.parse().ok()?;
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // A leap second is clamped to :59 rather than rejected: the timestamp is still the right minute,
    // and refusing it would drop a delay the provider meant to grant.
    let second = second.min(59);
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second))?;
    u64::try_from(seconds).ok()?.checked_mul(1000)
}

/// The message a failure reports, falling back to a code-derived sentence when the provider sent none.
fn message_of(signal: &Signal<'_>) -> String {
    if !signal.message.is_empty() {
        return signal.message.to_owned();
    }
    match signal.status {
        Some(status) => format!("provider request failed (HTTP {status})"),
        None => "provider request failed".to_owned(),
    }
}

/// The resolved `retry-after`, or `None` when the header is absent or unusable.
fn retry_after(signal: &Signal<'_>) -> Option<u64> {
    signal
        .retry_after
        .and_then(|value| parse_retry_after(value, signal.now_unix_ms))
}

/// The request id, rejecting an empty one so it is either absent or an identity.
fn request_id(signal: &Signal<'_>) -> Option<String> {
    signal
        .request_id
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------------------------
// Word matching.
//
// The upstream classifiers are regular expressions. Rust has no `regex` dependency here, and adding
// one for five patterns would be a poor trade: the patterns are fixed, the case folding is ASCII, and
// the matcher below is a few dozen lines. It is *not* a regular-expression engine — each pattern is a
// small sequence matcher whose separators are checked explicitly, which is what makes the narrowness
// of the upstream patterns reproducible rather than approximated.
// ---------------------------------------------------------------------------------------------

/// One alphanumeric run in the lowercased detail, with its byte span.
#[derive(Clone, Copy, Debug)]
struct Word<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

/// Splits a lowercased detail into alphanumeric words.
///
/// Punctuation is dropped rather than tokenized, so `context_length_exceeded` yields three words and
/// `context-window-overflowed` yields three more. What the punctuation *was* still matters to three of
/// the patterns, which is why each [`Word`] keeps its span: a separator is checked by slicing the
/// original string between two words, not by looking at the words themselves.
fn words(lower: &str) -> Vec<Word<'_>> {
    let bytes = lower.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_alphanumeric() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphanumeric() {
            index += 1;
        }
        // Both ends are ASCII alphanumeric boundaries, so the slice is valid UTF-8 even when the
        // surrounding text is not ASCII.
        out.push(Word {
            text: &lower[start..index],
            start,
            end: index,
        });
    }
    out
}

/// The single character between two words, when exactly one character separates them.
///
/// Upstream writes `[\s_-]` — one character — so two spaces are *not* a separator there, and this
/// returns `None` for them. Reproducing that exactly is deliberate: a looser rule would match wordings
/// upstream refuses, and the point of a port is that both builds agree about what a given provider
/// error means.
fn single_separator(lower: &str, left: Word<'_>, right: Word<'_>) -> Option<char> {
    if right.start <= left.end {
        return None;
    }
    let mut chars = lower[left.end..right.start].chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(first)
}

/// Whether the gap between two words is one or more characters, each whitespace, `-`, or `_`.
///
/// This is upstream's `[\s_-]+`, including mixed runs like `" _ "`. Used only by the quota
/// classifiers, whose patterns all use the `+` form.
fn separator_run(lower: &str, left: Word<'_>, right: Word<'_>) -> bool {
    right.start > left.end
        && lower[left.end..right.start]
            .chars()
            .all(|c| c.is_whitespace() || c == '-' || c == '_')
}

/// Whether the gap between two words is one or more whitespace characters — upstream's `\s+`.
fn whitespace_run(lower: &str, left: Word<'_>, right: Word<'_>) -> bool {
    right.start > left.end && lower[left.end..right.start].chars().all(char::is_whitespace)
}

/// Whether a single-character separator is one the word-boundary patterns accept: whitespace, `-`, `_`.
fn is_word_separator(c: char) -> bool {
    c.is_whitespace() || c == '-' || c == '_'
}

/// Looks up the word at `index`.
fn at<'a, 'b>(words: &'b [Word<'a>], index: usize) -> Option<Word<'a>> {
    words.get(index).copied()
}

/// `context[-_ ](length|window)[-_ ](exceed(ed|s)?|overflow(ed)?|limit[-_ ]exceeded)`.
///
/// The structured form: a provider code such as `context_length_exceeded` or
/// `context-window-overflowed`. Because the separators are single characters, the prose "context
/// window size must be positive" does not match — `size` is not one of the three predicates.
fn structured_overflow(lower: &str, words: &[Word<'_>]) -> bool {
    for (index, word) in words.iter().enumerate() {
        if word.text != "context" {
            continue;
        }
        let Some(noun) = at(words, index + 1) else {
            continue;
        };
        if !matches!(noun.text, "length" | "window") {
            continue;
        }
        if single_separator(lower, *word, noun).is_none_or(|c| !is_word_separator(c)) {
            continue;
        }
        let Some(predicate) = at(words, index + 2) else {
            continue;
        };
        if single_separator(lower, noun, predicate).is_none_or(|c| !is_word_separator(c)) {
            continue;
        }
        match predicate.text {
            "exceed" | "exceeds" | "exceeded" | "overflow" | "overflowed" => return true,
            // `limit[-_ ]exceeded` is the third alternative and needs one more word.
            "limit" => {
                if let Some(tail) = at(words, index + 3) {
                    if tail.text == "exceeded"
                        && single_separator(lower, predicate, tail)
                            .is_some_and(is_word_separator)
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// `(maximum|max)(\s+(allowed|supported))?\s+context\s+(length|window)`.
///
/// OpenAI's prose: "This model's maximum context length is 128000 tokens". The `maximum`/`max` word
/// must be directly followed by `context` (optionally through `allowed`/`supported`), which is what
/// keeps "input exceeds maximum allowed value" out — neither `context length` nor `context window`
/// follows it.
fn maximum_context(lower: &str, words: &[Word<'_>]) -> bool {
    for (index, word) in words.iter().enumerate() {
        if !matches!(word.text, "maximum" | "max") {
            continue;
        }
        let mut cursor = index + 1;
        if let Some(qualifier) = at(words, cursor) {
            if matches!(qualifier.text, "allowed" | "supported")
                && whitespace_run(lower, *word, qualifier)
            {
                cursor += 1;
            }
        }
        let Some(noun) = at(words, cursor) else {
            continue;
        };
        let Some(previous) = at(words, cursor.wrapping_sub(1)) else {
            continue;
        };
        if noun.text != "context" || !whitespace_run(lower, previous, noun) {
            continue;
        }
        let Some(size) = at(words, cursor + 1) else {
            continue;
        };
        if matches!(size.text, "length" | "window") && whitespace_run(lower, noun, size) {
            return true;
        }
    }
    false
}

/// `(request|prompt|input|messages?)\s+(is\s+|are\s+)?too\s+(large|long)\s+for\s+(this|the)?\s*
/// (model('s)?\s+)?context`.
///
/// The request-size form that names the model *and* its context: "request too large for model
/// context". The context token is required — "input is too long for this model" is matched by
/// [`too_long_for_model`] instead, which is a different pattern upstream keeps separate.
fn too_large_for_context(lower: &str, words: &[Word<'_>]) -> bool {
    for (index, word) in words.iter().enumerate() {
        if !matches!(word.text, "request" | "prompt" | "input" | "message" | "messages") {
            continue;
        }
        let mut cursor = index + 1;
        if let Some(copula) = at(words, cursor) {
            if matches!(copula.text, "is" | "are") && whitespace_run(lower, *word, copula) {
                cursor += 1;
            }
        }
        let Some(too) = at(words, cursor) else {
            continue;
        };
        let Some(previous) = at(words, cursor.wrapping_sub(1)) else {
            continue;
        };
        if too.text != "too" || !whitespace_run(lower, previous, too) {
            continue;
        }
        cursor += 1;
        let Some(size) = at(words, cursor) else {
            continue;
        };
        if !matches!(size.text, "large" | "long") || !whitespace_run(lower, too, size) {
            continue;
        }
        cursor += 1;
        let Some(for_word) = at(words, cursor) else {
            continue;
        };
        if for_word.text != "for" || !whitespace_run(lower, size, for_word) {
            continue;
        }
        cursor += 1;
        if let Some(article) = at(words, cursor) {
            if matches!(article.text, "this" | "the") && whitespace_run(lower, for_word, article) {
                cursor += 1;
            }
        }
        // `model('s)?` consumes an optional possessive `s` as its own word.
        if let Some(model) = at(words, cursor) {
            if model.text == "model" {
                let before = at(words, cursor.wrapping_sub(1));
                if before.is_some_and(|previous| whitespace_run(lower, previous, model)) {
                    cursor += 1;
                    if let Some(possessive) = at(words, cursor) {
                        if possessive.text == "s"
                            && single_separator(lower, model, possessive) == Some('\'')
                        {
                            cursor += 1;
                        }
                    }
                }
            }
        }
        let Some(context) = at(words, cursor) else {
            continue;
        };
        let Some(previous) = at(words, cursor.wrapping_sub(1)) else {
            continue;
        };
        if context.text == "context" && whitespace_run(lower, previous, context) {
            return true;
        }
    }
    false
}

/// `(input|prompt|request)\s+(is\s+)?too\s+(long|large)\s+for\s+(this|the)\s+model`.
///
/// The request-size form with no context word at all: "input is too long for this model". It is a
/// separate pattern upstream because the article before `model` is required here, which is what
/// distinguishes it from "too long for model X" (a name, not a capacity claim).
fn too_long_for_model(lower: &str, words: &[Word<'_>]) -> bool {
    for (index, word) in words.iter().enumerate() {
        if !matches!(word.text, "input" | "prompt" | "request") {
            continue;
        }
        let mut cursor = index + 1;
        if let Some(copula) = at(words, cursor) {
            if copula.text == "is" && whitespace_run(lower, *word, copula) {
                cursor += 1;
            }
        }
        let Some(too) = at(words, cursor) else {
            continue;
        };
        let Some(previous) = at(words, cursor.wrapping_sub(1)) else {
            continue;
        };
        if too.text != "too" || !whitespace_run(lower, previous, too) {
            continue;
        }
        cursor += 1;
        let Some(size) = at(words, cursor) else {
            continue;
        };
        if !matches!(size.text, "long" | "large") || !whitespace_run(lower, too, size) {
            continue;
        }
        cursor += 1;
        let Some(for_word) = at(words, cursor) else {
            continue;
        };
        let Some(previous) = at(words, cursor.wrapping_sub(1)) else {
            continue;
        };
        if for_word.text != "for" || !whitespace_run(lower, previous, for_word) {
            continue;
        }
        cursor += 1;
        let Some(article) = at(words, cursor) else {
            continue;
        };
        let Some(previous) = at(words, cursor.wrapping_sub(1)) else {
            continue;
        };
        if !matches!(article.text, "this" | "the") || !whitespace_run(lower, previous, article) {
            continue;
        }
        cursor += 1;
        let Some(model) = at(words, cursor) else {
            continue;
        };
        let Some(previous) = at(words, cursor.wrapping_sub(1)) else {
            continue;
        };
        if model.text == "model" && whitespace_run(lower, previous, model) {
            return true;
        }
    }
    false
}

/// The largest gap, in characters, that `.{0,40}` allows.
const MAX_GAP_CHARS: usize = 40;

/// Whether the characters between two words are eligible for a `.{0,40}` gap.
///
/// Two conditions, both from the regular expression: `.` does not match a newline, and the gap is
/// bounded. `end` is exclusive and `start` inclusive, so the count is the gap itself.
fn bounded_gap(lower: &str, left: Word<'_>, right: Word<'_>) -> bool {
    if right.start <= left.end {
        return false;
    }
    let gap = &lower[left.end..right.start];
    !gap.contains('\n') && gap.chars().count() <= MAX_GAP_CHARS
}

/// Whether a word is a predicate of the "exceeds" family.
fn is_exceed_word(text: &str) -> bool {
    matches!(text, "exceed" | "exceeds" | "exceeded" | "overflow" | "overflows")
}

/// `(input|prompt|request|messages?).{0,40}(exceed(s|ed)?|overflows?|is larger than).{0,40}
/// (the )?(model('s)? )?context(length|window)?`.
///
/// The pattern that has to earn its narrowness twice: it matches "input exceeds the model context
/// window limit", and it must *not* match "input exceeds maximum allowed value", where the input is a
/// parameter and the bound has nothing to do with context. Requiring a `context` word within forty
/// characters of the predicate is the whole defence, and both sides of the gap are bounded so a
/// message that mentions context far away from the verb is not read as an overflow.
fn exceeds_model_context(lower: &str, words: &[Word<'_>]) -> bool {
    for (index, word) in words.iter().enumerate() {
        if !matches!(word.text, "input" | "prompt" | "request" | "message" | "messages") {
            continue;
        }
        for (offset, candidate) in words.iter().enumerate().skip(index + 1) {
            if !bounded_gap(lower, *word, *candidate) {
                // Every later word is farther away than this one, so the gap only grows.
                break;
            }
            let predicate_end = if is_exceed_word(candidate.text) {
                Some(candidate.end)
            } else if candidate.text == "is" {
                // The `is larger than` alternative is three words; accept it as one predicate.
                let larger = at(words, offset + 1);
                let than = at(words, offset + 2);
                match (larger, than) {
                    (Some(larger), Some(than))
                        if larger.text == "larger" && than.text == "than" =>
                    {
                        Some(than.end)
                    }
                    _ => None,
                }
            } else {
                None
            };
            let Some(predicate_end) = predicate_end else {
                continue;
            };
            let predicate = Word {
                text: candidate.text,
                start: candidate.start,
                end: predicate_end,
            };
            for tail in words.iter().skip(offset) {
                if !bounded_gap(lower, predicate, *tail) {
                    break;
                }
                if tail.text == "context" {
                    return true;
                }
            }
        }
    }
    false
}

/// `insufficient[\s_-]+(quota|balance|credits?)`.
fn insufficient_quota(lower: &str, words: &[Word<'_>]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        word.text == "insufficient"
            && at(words, index + 1).is_some_and(|next| {
                matches!(next.text, "quota" | "balance" | "credit" | "credits")
                    && separator_run(lower, *word, next)
            })
    })
}

/// `(quota|usage[\s_-]+limit)[\s_-]+(exceeded|exhausted|reached)`.
fn quota_reached(lower: &str, words: &[Word<'_>]) -> bool {
    let predicates = ["exceeded", "exhausted", "reached"];
    words.iter().enumerate().any(|(index, word)| {
        let noun_end = if word.text == "quota" {
            Some(*word)
        } else if word.text == "usage" {
            at(words, index + 1)
                .filter(|next| next.text == "limit" && separator_run(lower, *word, *next))
        } else {
            None
        };
        let Some(noun) = noun_end else {
            return false;
        };
        at(words, index + 1)
            .filter(|_| word.text == "quota")
            .or_else(|| at(words, index + 2))
            .is_some_and(|next| {
                predicates.contains(&next.text) && separator_run(lower, noun, next)
            })
    })
}

/// `exceed(ed|s)?[\s_-]+((your|the)[\s_-]+)?(current[\s_-]+)?quota`.
fn exceeded_quota(lower: &str, words: &[Word<'_>]) -> bool {
    for (index, word) in words.iter().enumerate() {
        if !matches!(word.text, "exceed" | "exceeded" | "exceeds") {
            continue;
        }
        let mut cursor = index + 1;
        let mut previous = *word;
        if let Some(owner) = at(words, cursor) {
            if matches!(owner.text, "your" | "the") && separator_run(lower, previous, owner) {
                previous = owner;
                cursor += 1;
            }
        }
        if let Some(current) = at(words, cursor) {
            if current.text == "current" && separator_run(lower, previous, current) {
                previous = current;
                cursor += 1;
            }
        }
        if let Some(quota) = at(words, cursor) {
            if quota.text == "quota" && separator_run(lower, previous, quota) {
                return true;
            }
        }
    }
    false
}

/// `(balance|credits?)[\s_-]+(exhausted|depleted)`.
fn balance_depleted(lower: &str, words: &[Word<'_>]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        matches!(word.text, "balance" | "credit" | "credits")
            && at(words, index + 1).is_some_and(|next| {
                matches!(next.text, "exhausted" | "depleted")
                    && separator_run(lower, *word, next)
            })
    })
}

/// `out[\s_-]+of[\s_-]+(credits?|budget)`.
fn out_of_budget(lower: &str, words: &[Word<'_>]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        if word.text != "out" {
            return false;
        }
        let Some(of_word) = at(words, index + 1) else {
            return false;
        };
        if of_word.text != "of" || !separator_run(lower, *word, of_word) {
            return false;
        }
        at(words, index + 2).is_some_and(|next| {
            matches!(next.text, "credit" | "credits" | "budget")
                && separator_run(lower, of_word, next)
        })
    })
}

/// The set of codes this build knows, for callers that want a collection rather than a predicate.
#[must_use]
pub fn known_codes() -> BTreeSet<&'static str> {
    FAILURE_CODES.into_iter().collect()
}
