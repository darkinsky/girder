//! The pi-ai runtime gap: stop-reason mapping, request-context conversion, and prefix binding.
//!
//! # What upstream has
//!
//! `packages/llm` is 9,951 effective lines across nine packages. The pi-ai adapter is the largest
//! family inside it, and three of its modules are pure transformations with no I/O:
//!
//! | module | lines | capability |
//! |---|---|---|
//! | `llm-pi-ai/src/stream.ts` | 254 | pi-ai terminal events into harness `FinishReason`/`TokenUsage`: usage folding, overflow detection, failure classification, chunk translation |
//! | `llm-pi-ai/src/context.ts` | 411 | harness request history into pi-ai's `Context` vocabulary: system-prompt splitting, mid-conversation system reminders, developer tool updates, tool-result name recovery, image budget targets |
//! | `llm-pi-ai/src/prefix-binding.ts` | 110 | the `SHA-256` chain that decides whether replayed Anthropic signed thinking still belongs to the request prefix it was generated under |
//!
//! # What this crate already covered
//!
//! - [`crate::llm::extra`] carries the failure-code vocabulary (`PI_AI_ERROR`, `AUTH`,
//!   `CONTEXT_WINDOW_EXCEEDED`, `EMPTY_RESPONSE`, …) and the harness-side overflow/quota
//!   classifiers. That is upstream `packages/llm/llm/src/error.ts`, not the adapter.
//! - [`crate::llm::stream`] carries the *durable* half of pi-ai: `PiStopReason`,
//!   `PiReplayResponse`, `pi_replay_state` and `read_pi_replay`. It validates what was stored; it
//!   never maps a live response onto a harness finish reason.
//! - [`crate::llm::attempt`] assembles chunks into blocks and timing; [`crate::llm::image_tokens`] prices
//!   images under `DeepSeek`'s patch-grid budget, which is a *different* geometry problem from
//!   pi-ai's square-root pixel budget.
//! - [`crate::session::content_projection`] renders the model-facing image/file prose and computes the
//!   offload counts that gate a request.
//!
//! # The gap, and why these three functions are the largest pure part of it
//!
//! Nothing local answered three questions the adapter answers on every call:
//!
//! 1. **Why did this response stop, and what should the loop do about it?** `mapStopReason` folds
//!    two independent overflow detectors (pi-ai's 25 provider wordings plus the harness's own
//!    classifier), a zero-block `stop`, a zero-output `length` that filled the window, and pi-ai's
//!    two transient terminal states (`pending`, `deferred`) into one routing decision. Each of
//!    those is invisible when wrong: an overflow reported as a plain failure burns the retry budget
//!    on a request that can only succeed after compaction, and an empty `stop` reported as success
//!    ends the turn with nothing for the loop to act on.
//! 2. **What does the model actually receive?** `toPiContext` decides where the system prompt ends
//!    and the history begins, turns later system text into a `<system-reminder>` block when the
//!    provider cannot take a mid-conversation system message, resolves developer tool changes
//!    against the declared tool set, and recovers tool-result names from the preceding assistant
//!    turn. All of that is prose the model reads; a dropped `\n\n` or an unescaped closing tag is a
//!    behavioural change nothing reports.
//! 3. **May this signed thinking block be replayed?** Anthropic binds a signed thinking block to
//!    the exact request prefix it was generated under. Compaction or tool-result pruning rewrites
//!    that prefix, and replaying the block then makes the endpoint reject the *whole* request. The
//!    digest chain is what stands between a pruned history and a hard `400`.
//!
//! The remaining parts of the three files are impure by construction: `toStreamChunks` is an async
//! generator, and the image path of `toPiContext` reads attachment bytes through a durable store.
//! Those are deliberately not ported here — see the last section.
//!
//! # What is ported
//!
//! - [`map_usage`] and [`UsageCounts`] — pi-ai's counters with the two cache fields *present only
//!   when non-zero*, because pi-ai reports zeros rather than absence and a zero cache read must not
//!   become "cache read supported, zero tokens used".
//! - [`is_context_overflow`] — pi-ai's three overflow cases, including the 25-word pattern table.
//! - [`classify_pi_ai_error`] — the leading-status-gated failure classification.
//! - [`map_stop_reason`] — the routing decision, as [`PiFinish`].
//! - [`request_image_target`] — pi-ai's square-root pixel budget, as distinct from
//!   [`crate::llm::image_tokens::request_dimensions`]'s patch grid.
//! - [`to_pi_context`] — the synchronous, text-only conversion, refusing images exactly where
//!   upstream's synchronous overload refuses them.
//! - [`foreign_assistant`] — the provider-neutral assistant message a request replays when its
//!   durable replay state is unusable.
//! - [`chain_digest`], [`open_tool_round_assistant`], [`bind_thinking_to_prefix`] — the prefix chain.
//!
//! # Deliberate differences from upstream
//!
//! Two, both forced by the language, and both named where they are visible:
//!
//! - **The digest seed differs.** Upstream's seed is `dsh-llm-pi-ai/prefix-binding/v1`; this port
//!   uses [`DIGEST_SEED`]'s `girder-…` spelling. Upstream's own contract says that changing the
//!   digest input or its serialization requires a new seed, and this port serializes object members
//!   in sorted order (see [`json_text`]) so the digest cannot depend on `serde_json`'s map backend.
//!   A digest written by upstream therefore never matches, which invalidates every recorded
//!   signature exactly once — the safe direction, and the one upstream prescribes.
//! - **Object identity becomes message index.** Upstream keys recorded digests by the
//!   `AssistantMessage` object; Rust keys them by position in the converted context ([`PrefixDigests`]).
//!   Binding never reorders the list, so the index is the same fact.
//!
//! The digest itself is not re-implemented: [`crate::session::extra::digest_hex`] is the crate's
//! existing `SHA-256`, and this module calls it. The crate has no hash dependency by design.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value};

// Re-exported rather than imported privately: the codes are the routing half of every value this
// module returns, so a caller that matches on them can name the same paths this module uses. These
// are [`crate::llm::extra`]'s constants, not copies.
pub use girder_core::llm::extra::{
    ABORTED, AUTH, CONTEXT_WINDOW_EXCEEDED, EMPTY_RESPONSE, INVALID_REQUEST, QUOTA, RATE_LIMIT,
    SERVER, TIMEOUT, TRANSPORT,
};
use girder_core::llm::extra::{is_context_window_exceeded, is_quota_exceeded};
use girder_core::{ContentBlock, FinishReason, ImageDimensions, TokenUsage};

use crate::session::extra::digest_hex;

/// Canonical code: a pi-ai failure this module could not classify further.
///
/// Spelled here because [`crate::llm::extra`] publishes constants only for the codes its own
/// classifiers produce; this one is adapter-side. A test asserts it is a member of
/// [`crate::llm::extra::FAILURE_CODES`], so the two cannot drift apart silently.
pub const PI_AI_ERROR: &str = "PI_AI_ERROR";

/// Canonical code: the request carries content pi-ai cannot represent.
///
/// The same note as [`PI_AI_ERROR`] applies.
pub const UNSUPPORTED_CONTENT: &str = "UNSUPPORTED_CONTENT";

// ---------------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------------

/// The wire protocol whose thinking signatures bind to the request prefix.
///
/// Only Anthropic Messages enforces the check; every other protocol records no digest and replays
/// its signed thinking unconditionally, so [`bind_thinking_to_prefix`] is the identity on them.
pub const PREFIX_BOUND_API: &str = "anthropic-messages";

/// Seed of every digest chain.
///
/// The `girder-` prefix is not cosmetic: it makes a digest from this build unable to collide with
/// one written by upstream, which is what upstream's own "change the serialization, change the seed"
/// rule requires. See the module docs.
pub const DIGEST_SEED: &str = "girder-llm-pi-ai/prefix-binding/v1";

/// The synthetic `api` of an assistant message that carries no usable replay state.
///
/// Deliberately never equal to a catalog API, so the message is treated as foreign even when its
/// durable source names the provider and model of the request being made.
pub const FOREIGN_API: &str = "dsh-foreign";

/// The placeholder tool name for a tool result with no preceding tool call to name it.
///
/// A result whose call was pruned still has to be sent: the protocol requires one result per call,
/// and dropping it changes the conversation. `unknown` keeps the message and says the name is lost.
pub const UNKNOWN_TOOL_NAME: &str = "unknown";

/// What a tool result whose blocks produced no text is sent as.
///
/// An empty string is a legal but meaningless tool result; providers differ on whether they accept
/// it, so upstream substitutes a sentence that says the tool produced nothing.
pub const EMPTY_TOOL_OUTPUT: &str = "(no output)";

// ---------------------------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------------------------

/// Cumulative token accounting as pi-ai reports it.
///
/// The two cache counters are plain zeros when the provider did not report them; pi-ai's own type
/// is not optional, so the "absent" fact survives only in [`map_usage`]'s output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PiUsage {
    /// Prompt tokens.
    pub input: u64,
    /// Completion tokens, with reasoning already folded in.
    pub output: u64,
    /// Prompt tokens served from the provider's cache.
    pub cache_read: u64,
    /// Prompt tokens written into the provider's cache.
    pub cache_write: u64,
    /// The provider's own total, which is not always `input + output`.
    pub total: u64,
}

/// Harness token counts, with the two cache fields present only when the provider reported them.
///
/// `Option` rather than a sentinel is the point of the mapping: downstream cost and cache-hit
/// arithmetic branches on whether a value exists, and a `0` that means "not reported" is
/// indistinguishable from a `0` that means "reported, empty cache".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageCounts {
    /// Prompt tokens.
    pub input: u64,
    /// Completion tokens.
    pub output: u64,
    /// The provider's exact total, preserved rather than recomputed.
    pub total: u64,
    /// Cache-read tokens, when non-zero.
    pub cache_read: Option<u64>,
    /// Cache-write tokens, when non-zero.
    pub cache_write: Option<u64>,
}

impl UsageCounts {
    /// Projects onto the crate's coarser [`TokenUsage`], which has one `cached` field.
    ///
    /// Lossy on purpose and documented as such: `TokenUsage` cannot distinguish a cache *read*
    /// (billed at a discount) from a cache *write* (billed at a premium), so this folds both into
    /// `cached`. Callers that price a request must use [`UsageCounts`] instead.
    #[must_use]
    pub fn to_token_usage(self) -> TokenUsage {
        TokenUsage {
            input: self.input,
            output: self.output,
            cached: self
                .cache_read
                .unwrap_or(0)
                .saturating_add(self.cache_write.unwrap_or(0)),
        }
    }
}

/// Maps pi-ai usage onto harness counts, keeping the provider's exact total.
///
/// Upstream `mapUsage`.
#[must_use]
pub fn map_usage(usage: &PiUsage) -> UsageCounts {
    UsageCounts {
        input: usage.input,
        output: usage.output,
        total: usage.total,
        cache_read: nonzero(usage.cache_read),
        cache_write: nonzero(usage.cache_write),
    }
}

/// `Some(value)` for a positive count, `None` for the zero pi-ai reports in place of absence.
fn nonzero(value: u64) -> Option<u64> {
    if value > 0 { Some(value) } else { None }
}

// ---------------------------------------------------------------------------------------------
// Stop reasons
// ---------------------------------------------------------------------------------------------

/// pi-ai's full terminal-state domain, including the two states the durable replay reader refuses.
///
/// Deliberately not [`crate::llm::stream::PiStopReason`]: that enum is the *stored* domain and
/// rejects `pending`/`deferred` because a message that ended in one of them cannot be reconstructed.
/// A live response, however, can end in either, and [`map_stop_reason`] has to report it — so this
/// enum is a superset and the conversion is explicit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PiStop {
    /// A durable handle was returned instead of an answer.
    Pending,
    /// The model finished its turn.
    #[default]
    Stop,
    /// The model hit its output limit.
    Length,
    /// The model asked for tools.
    ToolUse,
    /// The provider reported a failure.
    Error,
    /// The caller cancelled.
    Aborted,
    /// The provider deferred the response.
    Deferred,
}

impl PiStop {
    /// The wire tag.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolUse => "toolUse",
            Self::Error => "error",
            Self::Aborted => "aborted",
            Self::Deferred => "deferred",
        }
    }

    /// Parses a wire tag, accepting all seven values.
    #[must_use]
    pub fn parse(tag: &str) -> Option<Self> {
        match tag {
            "pending" => Some(Self::Pending),
            "stop" => Some(Self::Stop),
            "length" => Some(Self::Length),
            "toolUse" => Some(Self::ToolUse),
            "error" => Some(Self::Error),
            "aborted" => Some(Self::Aborted),
            "deferred" => Some(Self::Deferred),
            _ => None,
        }
    }
}

/// One terminal pi-ai response, reduced to what the finish mapping reads.
///
/// A local shape rather than a pi-ai message type: the mapping needs six facts, and every pi-ai
/// message field it does not read (ids, timestamps, signatures) would have to be fabricated by
/// every caller otherwise.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PiResponse {
    /// The terminal state.
    pub stop: PiStop,
    /// The provider's failure text, when it produced one.
    pub error_message: Option<String>,
    /// How many content blocks the response produced.
    pub content_len: usize,
    /// The provider-reported model, used in the failure prose.
    pub model: String,
    /// The provider route, consulted by the Cerebras body-less overflow rule.
    pub provider: String,
    /// The response's usage, for the two usage-based overflow cases.
    pub usage: PiUsage,
}

/// A failure a caller can route on: a stable code plus the provider's own words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiFailure {
    /// Provider-neutral routing code, one of [`crate::llm::extra::FAILURE_CODES`].
    pub code: &'static str,
    /// Human-readable failure.
    pub message: String,
}

/// What the loop should do about one terminal response.
///
/// [`FinishReason`] already says *how* the response ended; `failure` adds the code that decides
/// whether to retry, compact, re-authenticate, or surface the error. They travel together because
/// a `FinishReason::Error` without its code is exactly the state that makes a retry policy guess.
#[derive(Clone, Debug, PartialEq)]
pub struct PiFinish {
    /// The terminal reason, in the crate's vocabulary.
    pub reason: FinishReason,
    /// The routing code, when the outcome is a failure.
    pub failure: Option<PiFailure>,
}

impl PiFinish {
    /// A non-failure outcome.
    fn settled(reason: FinishReason) -> Self {
        Self {
            reason,
            failure: None,
        }
    }

    /// A failure carrying its routing code.
    ///
    /// `FinishReason::Error` carries the message itself, so the same text appears in both places:
    /// the reason is what a log records, the failure is what a policy reads.
    fn failed(code: &'static str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            reason: FinishReason::Error {
                message: message.clone(),
            },
            failure: Some(PiFailure { code, message }),
        }
    }
}

/// Maps a terminal pi-ai response onto the harness finish reason and its routing code.
///
/// Upstream `mapStopReason`. The order of the checks is the decision:
///
/// 1. **Overflow first, whatever the stop reason.** An overflow surfaces as `error` on most
///    providers, as `stop` on providers that accept the request and quietly exceed the window, and
///    as `length` with zero output on providers that truncate the input to fit. All three are
///    `CONTEXT_WINDOW_EXCEEDED`, which the loop answers by compacting rather than retrying.
/// 2. **A `stop` with no blocks is a failure, not an empty success.** A provider that completes
///    with nothing to act on has produced no durable content; reporting `stop` ends the turn.
/// 3. **`pending` and `deferred` are non-retryable errors**, because resending cannot turn a
///    durable handle into an answer.
#[must_use]
pub fn map_stop_reason(response: &PiResponse, context_window: Option<u64>) -> PiFinish {
    let pi_ai_overflow = is_context_overflow(response, context_window);
    let harness_overflow = response.stop == PiStop::Error
        && response
            .error_message
            .as_deref()
            .is_some_and(is_context_window_exceeded);
    if pi_ai_overflow || harness_overflow {
        let message = response.error_message.clone().unwrap_or_else(|| {
            format!(
                "pi-ai detected context overflow for model \"{}\"",
                response.model
            )
        });
        return PiFinish::failed(CONTEXT_WINDOW_EXCEEDED, message);
    }

    match response.stop {
        PiStop::Stop => {
            if response.content_len == 0 {
                return PiFinish::failed(
                    EMPTY_RESPONSE,
                    format!(
                        "model \"{}\" returned a completed response with no content",
                        response.model
                    ),
                );
            }
            PiFinish::settled(FinishReason::Stop)
        }
        // Upstream calls this `max-tokens`; the crate's `FinishReason::Length` is the same fact
        // under the name its own providers already use.
        PiStop::Length => PiFinish::settled(FinishReason::Length),
        PiStop::ToolUse => PiFinish::settled(FinishReason::ToolCalls),
        PiStop::Pending => PiFinish::failed(
            PI_AI_ERROR,
            format!(
                "pi-ai stream for model \"{}\" ended pending",
                response.model
            ),
        ),
        PiStop::Deferred => PiFinish::failed(
            PI_AI_ERROR,
            format!(
                "pi-ai deferred response for model \"{}\" is not supported",
                response.model
            ),
        ),
        PiStop::Aborted => {
            let message = response
                .error_message
                .clone()
                .unwrap_or_else(|| "pi-ai stream aborted".to_owned());
            PiFinish {
                reason: FinishReason::Aborted,
                failure: Some(PiFailure {
                    code: ABORTED,
                    message,
                }),
            }
        }
        PiStop::Error => {
            let text = response
                .error_message
                .clone()
                .unwrap_or_else(|| "pi-ai stream error".to_owned());
            PiFinish::failed(classify_pi_ai_error(&text), text)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------------------------

/// Maps one provider failure string onto a routing code.
///
/// Upstream `classifyPiAiError`. Two structural decisions carry the whole function:
///
/// - **A leading HTTP status wins over anything later in the text.** pi-ai renders an SDK failure
///   as `<status> <body>`, and the body can quote another hop's status — a gateway `500` embedding
///   its upstream's `401`. Reading `401` out of that body reports an authentication failure for a
///   server fault. So when the message *starts* with a status, only that status is read.
/// - **The order is by specificity, not by likelihood.** `413` (a request body over a size cap) is
///   answered before `400`, and both before `5xx`; a stream truncated before the terminal event
///   (`stream ended before message_stop`) is a transport failure rather than a model failure,
///   because the connection dropped mid-response.
///
/// The quota check reads the whole message rather than the status source: quota wording is prose,
/// and providers say it in a `429`, a `400`, or a `200`-shaped error body.
#[must_use]
pub fn classify_pi_ai_error(message: &str) -> &'static str {
    let lower = message.to_lowercase();
    let status_source = leading_http_status(message).unwrap_or(&lower);

    if contains_delimited(status_source, "401") || contains_delimited(status_source, "403") {
        return AUTH;
    }
    if is_quota_exceeded(message) {
        return QUOTA;
    }
    if contains_delimited(status_source, "429") || matches_optional_char(&lower, "rate", "limit") {
        return RATE_LIMIT;
    }
    // A rejected body (gateway or provider size cap): resending the same request cannot succeed,
    // so it is invalid rather than transient. `413` is checked before `400` because a gateway may
    // report both in one line.
    if contains_delimited(status_source, "413")
        || lower.contains("failed to buffer the request body")
            && lower.contains("length limit exceeded")
        || lower.contains("payload too large")
        || lower.contains("request body too large")
    {
        return INVALID_REQUEST;
    }
    if contains_delimited(status_source, "400")
        || matches_optional_char(&lower, "invalid", "request")
    {
        return INVALID_REQUEST;
    }
    if has_delimited_status(status_source, '5') {
        return SERVER;
    }
    if lower.contains("timeout") || lower.contains("timed out") || lower.contains("time out") {
        return TIMEOUT;
    }
    if lower.contains("stream ended before") || lower.contains("stream ended without") {
        return TRANSPORT;
    }
    if contains_any(
        &lower,
        &["network", "connection", "socket", "fetch", "econn"],
    ) || contains_any(
        &lower,
        &[
            "other side closed",
            "http2 request did not get a response",
            "websocket closed unexpectedly",
        ],
    ) || contains_delimited(&lower, "terminated")
        || lower.contains("premature close")
    {
        return TRANSPORT;
    }
    PI_AI_ERROR
}

/// The status a message leads with, when it leads with one.
///
/// Upstream's `/^[45]\d\d\b/`: three characters forming a `4xx` or `5xx` status at the very start,
/// followed by a non-word character (or the end of the message).
fn leading_http_status(message: &str) -> Option<&str> {
    let mut characters = message.char_indices();
    let (_, first) = characters.next()?;
    if first != '4' && first != '5' {
        return None;
    }
    let mut end = first.len_utf8();
    for _ in 0..2 {
        let (offset, character) = characters.next()?;
        if !character.is_ascii_digit() {
            return None;
        }
        end = offset + character.len_utf8();
    }
    let boundary_ok = message[end..]
        .chars()
        .next()
        .is_none_or(|character| !is_word(character));
    if boundary_ok {
        Some(&message[..end])
    } else {
        None
    }
}

/// Whether any three-digit run of the given hundreds digit appears delimited.
fn has_delimited_status(text: &str, hundreds: char) -> bool {
    let mut previous_is_word = false;
    let characters: Vec<char> = text.chars().collect();
    for (index, character) in characters.iter().enumerate() {
        let is_candidate = *character == hundreds
            && !previous_is_word
            && characters.get(index + 1).is_some_and(char::is_ascii_digit)
            && characters.get(index + 2).is_some_and(char::is_ascii_digit)
            && characters.get(index + 3).is_none_or(|next| !is_word(*next));
        if is_candidate {
            return true;
        }
        previous_is_word = is_word(*character);
    }
    false
}

/// Whether `needle` appears in `haystack` with non-word characters (or the ends) around it.
fn contains_delimited(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(found) = haystack[start..].find(needle) {
        let begin = start + found;
        let end = begin + needle.len();
        let before_ok = haystack[..begin]
            .chars()
            .next_back()
            .is_none_or(|character| !is_word(character));
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|character| !is_word(character));
        if before_ok && after_ok {
            return true;
        }
        start = begin + needle.len().max(1);
    }
    false
}

/// Whether `first` is followed by at most one arbitrary character and then `second`.
///
/// Upstream writes `/rate.?limit/i` and `/invalid.?request/i`; `.` does not match a newline, so the
/// single intervening character may be anything but a line break.
fn matches_optional_char(haystack: &str, first: &str, second: &str) -> bool {
    let mut start = 0;
    while let Some(found) = haystack[start..].find(first) {
        let after_first = start + found + first.len();
        if haystack[after_first..].starts_with(second) {
            return true;
        }
        let mut characters = haystack[after_first..].chars();
        if let Some(middle) = characters.next() {
            if middle != '\n' && characters.as_str().starts_with(second) {
                return true;
            }
        }
        start = start + found + first.len().max(1);
    }
    false
}

/// Whether any of `needles` appears in `haystack`.
fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// Whether a character counts as a regular-expression word character.
fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

// ---------------------------------------------------------------------------------------------
// Context overflow
// ---------------------------------------------------------------------------------------------

/// Whether a terminal response represents a context-window overflow.
///
/// Upstream `isContextOverflow` from `@earendil-works/pi-ai/utils/overflow`, which is what
/// [`map_stop_reason`] consults *before* the stop reason. Three cases:
///
/// 1. **Error text.** A `stopReason` of `error` whose message matches one of
///    [`OVERFLOW_PATTERNS`], unless it also matches [`NON_OVERFLOW_PATTERNS`] — Bedrock words a
///    throttling failure as "Too many tokens, please wait", which the generic pattern would
///    otherwise read as overflow.
/// 2. **Silent overflow.** The provider accepted the request and reported success, but the prompt
///    alone (input plus cache read) exceeded the window.
/// 3. **Truncating overflow.** The provider cut the input to fit the window and had no room left to
///    generate, so it reports `length` with zero output. The window is compared at 99% because the
///    provider's own accounting is approximate.
#[must_use]
pub fn is_context_overflow(response: &PiResponse, context_window: Option<u64>) -> bool {
    if response.stop == PiStop::Error {
        if let Some(message) = response.error_message.as_deref() {
            if !is_non_overflow_text(message) {
                if is_overflow_text(message) {
                    return true;
                }
                if response.provider == "cerebras" && is_cerebras_bodyless_overflow(message) {
                    return true;
                }
            }
        }
    }

    let prompt_tokens = response
        .usage
        .input
        .saturating_add(response.usage.cache_read);
    if let Some(window) = context_window {
        if response.stop == PiStop::Stop && prompt_tokens > window {
            return true;
        }
        if response.stop == PiStop::Length
            && response.usage.output == 0
            // `input >= window * 0.99`, in integers so the boundary is exact.
            && u128::from(prompt_tokens) * 100 >= u128::from(window) * 99
        {
            return true;
        }
    }
    false
}

/// Whether `message` matches any of pi-ai's overflow wordings.
#[must_use]
pub fn is_overflow_text(message: &str) -> bool {
    let lower = message.to_lowercase();
    OVERFLOW_PATTERNS.iter().any(|pattern| pattern(&lower))
}

/// Whether `message` matches a known *non*-overflow wording that would otherwise look like one.
///
/// Each entry exists because the generic overflow patterns are deliberately broad: "too many
/// tokens" is how Bedrock words a throttle, and answering a throttle with compaction is a wasted
/// turn.
#[must_use]
pub fn is_non_overflow_text(message: &str) -> bool {
    let lower = message.to_lowercase();
    NON_OVERFLOW_PATTERNS.iter().any(|pattern| pattern(&lower))
}

/// Cerebras reports a size rejection as a bare status line with no body.
fn is_cerebras_bodyless_overflow(lower: &str) -> bool {
    let trimmed = lower.trim_start();
    let body = trimmed
        .strip_prefix("400")
        .or_else(|| trimmed.strip_prefix("413"))
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix("status code"))
        .map(str::trim_start)
        .or_else(|| {
            trimmed
                .strip_prefix("400")
                .or_else(|| trimmed.strip_prefix("413"))
                .map(str::trim_start)
        });
    body.is_some_and(|rest| rest.starts_with("(no body)"))
}

/// The 25 provider wordings that mean "the request does not fit".
///
/// Kept as a table of predicates rather than one long condition so a provider can be added — or a
/// wording shown to fire on the wrong message — without reading the other 24. The order is
/// pi-ai's, and [`OVERFLOW_PATTERN_COUNT`] makes an accidental deletion visible to the tests.
pub const OVERFLOW_PATTERNS: [fn(&str) -> bool; 25] = [
    // Anthropic, and z.ai's token overflow. `prompt (?:is )?too long`.
    prompt_too_long,
    // z.ai CN endpoint.
    |text| text.contains("prompt exceeds max length"),
    // Anthropic request byte-size overflow, HTTP 413.
    |text| text.contains("request_too_large"),
    // Amazon Bedrock.
    |text| text.contains("input is too long for requested model"),
    // OpenAI Completions and Responses.
    |text| text.contains("exceeds the context window"),
    // OpenAI-compatible proxies such as LiteLLM.
    exceeds_maximum_context_length,
    // Google Gemini.
    |text| between(text, "input token count", "exceeds the maximum"),
    // xAI Grok.
    |text| followed_by_digits(text, "maximum prompt length is"),
    // Groq.
    |text| text.contains("reduce the length of the messages"),
    // OpenRouter and most of its backends.
    |text| {
        split_around(text, "maximum context length is", "tokens")
            .is_some_and(|(gap, _)| is_numeric(gap.trim()))
    },
    // OpenRouter, Poolside.
    |text| {
        split_around(text, "maximum allowed input length of", "token")
            .is_some_and(|(gap, rest)| is_numeric(gap.trim()) || rest.starts_with('s'))
    },
    // Together AI.
    together_input_longer_than_context,
    // GitHub Copilot.
    |text| followed_by_digits(text, "exceeds the limit of"),
    // llama.cpp server.
    |text| text.contains("exceeds the available context size"),
    // LM Studio.
    |text| text.contains("greater than the context length"),
    // MiniMax.
    |text| text.contains("context window exceeds limit"),
    // Kimi For Coding.
    |text| text.contains("exceeded model token limit"),
    // Mistral.
    |text| followed_by_digits(text, "too large for model with"),
    // DS4 server.
    |text| {
        split_around(text, "prompt has", "tokens").is_some_and(|(gap, rest)| {
            is_numeric(gap.trim()) && rest.contains("configured context size")
        })
    },
    // z.ai surfaces a non-standard finish reason as error text.
    |text| text.contains("model_context_window_exceeded"),
    // Ollama's explicit overflow error. Must be read before the broader "prompt too long".
    |text| {
        text.contains("prompt too long")
            && split_around(text, "exceeded", "context length").is_some()
    },
    // DashScope and Qwen Token Plan.
    |text| text.contains("range of input length should be"),
    // Generic fallbacks.
    context_length_exceeded_parts,
    |text| text.contains("too many tokens"),
    |text| text.contains("token limit exceeded"),
];

/// How many wordings [`OVERFLOW_PATTERNS`] carries.
pub const OVERFLOW_PATTERN_COUNT: usize = OVERFLOW_PATTERNS.len();

/// The three wordings that disqualify an otherwise-overflow-looking message.
pub const NON_OVERFLOW_PATTERNS: [fn(&str) -> bool; 3] = [
    // AWS Bedrock's human-readable prefixes.
    |text| text.starts_with("throttling error:") || text.starts_with("service unavailable:"),
    // Generic rate limiting.
    |text| text.contains("rate limit"),
    // Generic HTTP 429 wording.
    |text| text.contains("too many requests"),
];

/// `exceeds [the] [model's] maximum context length [of N tokens | (N)]`.
fn exceeds_maximum_context_length(text: &str) -> bool {
    let Some((gap, rest)) = split_around(text, "exceeds", "maximum context length") else {
        return false;
    };
    let gap_ok = matches!(
        gap,
        "" | " "
            | " the "
            | " model's "
            | " model\u{2019}s "
            | " the model's "
            | " the model\u{2019}s "
    );
    if !gap_ok {
        return false;
    }
    let trimmed = rest.trim_start();
    if let Some(inner) = trimmed.strip_prefix('(') {
        let digits: String = inner
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ',')
            .collect();
        return is_numeric(digits.trim_end_matches(',')) && inner[digits.len()..].starts_with(')');
    }
    // `rest` was already left-trimmed, so the literal leading space of " of " is gone.
    match trimmed.strip_prefix("of ") {
        Some(after) => {
            let digits: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .collect();
            is_numeric(digits.trim_end_matches(',')) && after[digits.len()..].contains("token")
        }
        None => false,
    }
}

/// `input (N tokens) is longer than the model's context length (N tokens)`.
fn together_input_longer_than_context(text: &str) -> bool {
    let Some((count, rest)) = split_around(text, "input (", " tokens)") else {
        return false;
    };
    is_numeric(count.trim())
        && rest.contains("is longer than the model")
        && rest.contains("context length (")
}

/// `context[_ ]length[_ ]exceeded`, where each separator is one underscore or space.
fn context_length_exceeded_parts(text: &str) -> bool {
    ["context", "context_", "context "].iter().any(|head| {
        ["length_exceeded", "length exceeded"]
            .iter()
            .any(|tail| text.contains(&format!("{head}{tail}")))
    })
}

/// Whether `needle` is immediately followed by a run of digits.
fn followed_by_digits(text: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(index) = text[start..].find(needle) {
        let after = start + index + needle.len();
        // The upstream regexes spell the separator literally (`length is \d+`), so whitespace
        // between the phrase and the number is part of the wording rather than an accident.
        if text[after..]
            .trim_start_matches(|character: char| character.is_ascii_whitespace())
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
        {
            return true;
        }
        start = after.max(start + 1);
    }
    false
}

/// `prompt (?:is )?too long`.
fn prompt_too_long(text: &str) -> bool {
    text.contains("prompt too long") || text.contains("prompt is too long")
}

/// Whether `before` appears with `after` somewhere later in the text.
fn between(text: &str, before: &str, after: &str) -> bool {
    split_around(text, before, after).is_some()
}

/// Splits `text` at the first `before`, returning the gap up to `after` and everything past it.
///
/// An empty `after` yields the remainder as the gap, which is what
/// [`followed_by_digits`]-style checks want.
fn split_around<'a>(text: &'a str, before: &str, after: &str) -> Option<(&'a str, &'a str)> {
    let start = text.find(before)?;
    let rest = &text[start + before.len()..];
    if after.is_empty() {
        return Some((rest, ""));
    }
    let end = rest.find(after)?;
    Some((&rest[..end], &rest[end + after.len()..]))
}

/// Whether `text` is a non-empty run of digits, optionally with comma group separators.
fn is_numeric(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_digit() || character == ',')
        && text.chars().any(|character| character.is_ascii_digit())
}

// ---------------------------------------------------------------------------------------------
// Image targets
// ---------------------------------------------------------------------------------------------

/// Where one request image is sent: dimensions under a pixel budget, plus a byte target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageTarget {
    /// Dimensions the request asks the provider to decode.
    pub dimensions: ImageDimensions,
    /// Encoded-byte target for the sent image.
    pub max_bytes: u64,
}

/// Resolves one source image's request target under a route's pixel and byte budgets.
///
/// Upstream `requestImageTarget` composing `requestImageDimensions`. This is *not*
/// [`crate::llm::image_tokens::request_dimensions`]: that function implements `DeepSeek`'s patch-grid token
/// budget, which pads to 28-pixel patches and caps a token count. pi-ai's budget is a plain
/// total-pixel ceiling with a square-root scale and an integer long-edge search, and the two produce
/// different geometry for the same source. Using the wrong one silently sends an image the route
/// did not price.
///
/// The search is load-bearing: `floor`/`round` after a square-root scale can still land one pixel
/// over the budget, so the long edge is decremented until the area fits. A result that is exact only
/// up to rounding is rejected by a provider that enforces the cap.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel counts are far below 2^53, and every cast is of a non-negative scaled dimension"
)]
pub fn request_image_target(
    width: u64,
    height: u64,
    max_pixels: u64,
    max_bytes: u64,
) -> ImageTarget {
    let dimensions = if width == 0 || height == 0 {
        ImageDimensions::new(0, 0)
    } else {
        project_dimensions(width, height, max_pixels)
    };
    ImageTarget {
        dimensions,
        max_bytes,
    }
}

/// The aspect-preserving integer dimensions pi-ai asks for under a total-pixel budget.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel counts are far below 2^53, and every cast is of a non-negative scaled dimension"
)]
fn project_dimensions(width: u64, height: u64, max_pixels: u64) -> ImageDimensions {
    let area = width as f64 * height as f64;
    let scale = f64::from(1_u8).min((max_pixels as f64 / area).sqrt());
    if scale >= 1.0 {
        return ImageDimensions::new(
            u32::try_from(width).unwrap_or(u32::MAX),
            u32::try_from(height).unwrap_or(u32::MAX),
        );
    }
    let (long_source, short_source, width_is_long) = if width >= height {
        (width, height, true)
    } else {
        (height, width, false)
    };
    let _ = long_source;
    let mut projected_long = ((if width_is_long { width } else { height }) as f64 * scale)
        .floor()
        .max(1.0) as u64;
    let mut projected_short = ((projected_long as f64 * short_source as f64
        / width.max(height) as f64)
        .round()
        .max(1.0)) as u64;
    while projected_long * projected_short > max_pixels && projected_long > 1 {
        projected_long -= 1;
        projected_short = ((projected_long as f64 * short_source as f64 / width.max(height) as f64)
            .round()
            .max(1.0)) as u64;
    }
    let (sent_width, sent_height) = if width_is_long {
        (projected_long, projected_short)
    } else {
        (projected_short, projected_long)
    };
    ImageDimensions::new(
        u32::try_from(sent_width).unwrap_or(u32::MAX),
        u32::try_from(sent_height).unwrap_or(u32::MAX),
    )
}

// ---------------------------------------------------------------------------------------------
// pi-ai context vocabulary
// ---------------------------------------------------------------------------------------------

/// One declared tool, as pi-ai receives it.
#[derive(Clone, Debug, PartialEq)]
pub struct PiTool {
    /// Tool name.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
}

/// One assistant content block, in pi-ai's own vocabulary.
///
/// The three variants carry *different* signature fields because they are different artifacts: a
/// thinking signature is the opaque payload Anthropic binds to a request prefix, a text signature is
/// a separate provider attestation, and a thought signature belongs to a tool call. Flattening them
/// into one `signature` field would let a replay send the wrong one.
#[derive(Clone, Debug, PartialEq)]
pub enum PiAssistantBlock {
    /// Visible text.
    Text {
        /// The text.
        text: String,
        /// The provider's text signature, when it issued one.
        signature: Option<String>,
    },
    /// Reasoning, hidden from the user.
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// The opaque signature that binds this block to its request prefix.
        signature: Option<String>,
        /// Whether the provider redacted the payload and stored it in the signature.
        redacted: Option<bool>,
    },
    /// A tool invocation, with its arguments already parsed the way pi-ai requires them.
    ToolCall {
        /// Provider-issued call id.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed argument object; `{}` when the model's JSON was not an object.
        arguments: Value,
        /// Provider attestation for this call.
        thought_signature: Option<String>,
        /// Namespace for a dynamically loaded tool.
        namespace: Option<String>,
    },
}

impl PiAssistantBlock {
    /// Whether this block carries a signature or redacted payload that binds it to a request prefix.
    ///
    /// Upstream `signed`. Redacted thinking counts even without a signature, because a redacted
    /// payload *is* the signature — the provider stored the encrypted content there.
    #[must_use]
    pub fn is_signed(&self) -> bool {
        match self {
            Self::Thinking {
                signature,
                redacted,
                ..
            } => {
                *redacted == Some(true) || signature.as_ref().is_some_and(|value| !value.is_empty())
            }
            Self::Text { .. } | Self::ToolCall { .. } => false,
        }
    }
}

/// One assistant message in pi-ai's vocabulary.
#[derive(Clone, Debug, PartialEq)]
pub struct PiAssistant {
    /// Content blocks, in stream order.
    pub content: Vec<PiAssistantBlock>,
    /// The wire protocol that produced it.
    pub api: String,
    /// The provider route.
    pub provider: String,
    /// The requested model identity.
    pub model: String,
    /// The model the provider said it was, when that differs.
    pub response_model: Option<String>,
    /// The provider-issued response id.
    pub response_id: Option<String>,
    /// The provider-native thinking effort.
    pub provider_thinking_level: Option<String>,
    /// Usage. Always zero for a reconstructed history message.
    pub usage: PiUsage,
    /// The terminal state.
    pub stop: PiStop,
    /// Unix milliseconds.
    pub timestamp: u64,
}

/// A system message's content: a bare string, or text blocks.
///
/// Two shapes because upstream produces two: a native mid-conversation system message is a string,
/// while a developer update is a list of text blocks. The difference survives into the request
/// prefix digest, so flattening it would make two different requests share a digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PiSystemContent {
    /// A single string.
    Text(String),
    /// Text blocks.
    Blocks(Vec<String>),
}

/// One message in pi-ai's `Context` vocabulary.
#[derive(Clone, Debug, PartialEq)]
pub enum PiMessage {
    /// Instructions, possibly carrying tool-set changes.
    System {
        /// The instruction text.
        content: PiSystemContent,
        /// Unix milliseconds.
        timestamp: u64,
        /// Tools that become available at this point.
        tools_added: Vec<PiTool>,
        /// Names of tools that stop being available at this point.
        tools_removed: Vec<String>,
    },
    /// A user turn, always text on the synchronous path.
    User {
        /// The text.
        text: String,
        /// Unix milliseconds.
        timestamp: u64,
    },
    /// A model turn.
    Assistant(PiAssistant),
    /// A tool's answer to one call.
    ToolResult {
        /// The call this answers.
        tool_call_id: String,
        /// The tool's name, recovered from the preceding assistant turn when possible.
        tool_name: String,
        /// Text blocks.
        content: Vec<String>,
        /// Whether the tool failed.
        is_error: bool,
        /// Unix milliseconds.
        timestamp: u64,
    },
}

impl PiMessage {
    /// The role tag.
    #[must_use]
    pub fn role_tag(&self) -> &'static str {
        match self {
            Self::System { .. } => "system",
            Self::User { .. } => "user",
            Self::Assistant(_) => "assistant",
            Self::ToolResult { .. } => "toolResult",
        }
    }
}

/// A whole pi-ai request context.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PiContext {
    /// The initial system prompt. Absent sends no system prompt at all.
    pub system_prompt: Option<String>,
    /// The history.
    pub messages: Vec<PiMessage>,
    /// Declared tools. Empty means the field is omitted from the request.
    pub tools: Vec<PiTool>,
}

// ---------------------------------------------------------------------------------------------
// Request conversion
// ---------------------------------------------------------------------------------------------

/// Who authored one request message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiRequestRole {
    /// The system prompt.
    System,
    /// Harness-authored instructions, which may change the tool set.
    Developer,
    /// The person.
    User,
    /// The model.
    Assistant,
    /// A tool's result.
    Tool,
}

impl PiRequestRole {
    /// The role tag, for failure prose.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// One message of the request being converted.
///
/// A local shape rather than [`girder_core::Message`]: that type's role domain has no `system` or
/// `developer`, and both exist in the request (a leading system message is the system prompt, and a
/// developer message is what carries a mid-conversation tool change). `tool_call_id` and `is_error`
/// are hoisted out of the content because that is how this conversion reads them.
#[derive(Clone, Debug, PartialEq)]
pub struct PiRequestMessage {
    /// Who authored it.
    pub role: PiRequestRole,
    /// Its blocks.
    pub content: Vec<ContentBlock>,
    /// For a tool message, the call it answers.
    pub tool_call_id: Option<String>,
    /// For a tool message, whether the tool failed.
    pub is_error: bool,
    /// For a developer message, tools this update makes available.
    pub tools_added: Vec<String>,
    /// For a developer message, tools this update withdraws.
    pub tools_removed: Vec<String>,
}

impl PiRequestMessage {
    /// A message with no tool correlation and no tool-set change.
    #[must_use]
    pub fn new(role: PiRequestRole, content: Vec<ContentBlock>) -> Self {
        Self {
            role,
            content,
            tool_call_id: None,
            is_error: false,
            tools_added: Vec::new(),
            tools_removed: Vec::new(),
        }
    }

    /// A tool result.
    #[must_use]
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: PiRequestRole::Tool,
            content: vec![ContentBlock::Text { text: text.into() }],
            tool_call_id: Some(call_id.into()),
            is_error: false,
            tools_added: Vec::new(),
            tools_removed: Vec::new(),
        }
    }

    /// Marks a tool result as a failure.
    #[must_use]
    pub fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// A developer update adding and withdrawing tools.
    #[must_use]
    pub fn developer(text: impl Into<String>, added: Vec<String>, removed: Vec<String>) -> Self {
        Self {
            role: PiRequestRole::Developer,
            content: vec![ContentBlock::Text { text: text.into() }],
            tool_call_id: None,
            is_error: false,
            tools_added: added,
            tools_removed: removed,
        }
    }
}

/// One tool declaration in the request, minus pi-ai's own filtering.
#[derive(Clone, Debug, PartialEq)]
pub struct PiToolDeclaration {
    /// Tool name.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
    /// Whether the tool's declaration is withheld until its owning group is activated.
    ///
    /// A deferred tool is *not* sent: sending it would let the model call a capability the turn has
    /// not made available, which the harness then has to refuse.
    pub defer_loading: bool,
}

impl PiToolDeclaration {
    /// A tool that is sent whenever the request declares it.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            defer_loading: false,
        }
    }

    /// Withholds this declaration until its group is activated.
    #[must_use]
    pub fn deferred(mut self, defer_loading: bool) -> Self {
        self.defer_loading = defer_loading;
        self
    }
}

/// A request, in the shape this conversion reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PiRequest {
    /// Request-level system text, which precedes the leading system messages.
    pub system: Option<String>,
    /// The conversation.
    pub messages: Vec<PiRequestMessage>,
    /// Declared tools.
    pub tools: Vec<PiToolDeclaration>,
}

impl PiRequest {
    /// A request with no system text and no tools.
    #[must_use]
    pub fn new(messages: Vec<PiRequestMessage>) -> Self {
        Self {
            system: None,
            messages,
            tools: Vec::new(),
        }
    }

    /// Sets the request-level system text.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets the declared tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<PiToolDeclaration>) -> Self {
        self.tools = tools;
        self
    }
}

/// A request this build cannot convert into pi-ai's vocabulary.
///
/// Carries a failure code from [`crate::llm::extra::FAILURE_CODES`] rather than a variant per
/// cause, because the code is what a caller routes on and the message is what a person reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextError {
    /// Provider-neutral routing code.
    pub code: &'static str,
    /// Human-readable description.
    pub message: String,
}

impl ContextError {
    /// Builds an error.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ContextError {}

/// The request split into pi-ai's `systemPrompt` slot and the history that becomes `messages`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemPromptSplit {
    /// Text for pi-ai's `systemPrompt`; `None` sends no system prompt.
    pub system_prompt: Option<String>,
    /// Index of the first history message, past the consumed leading system messages.
    pub history_start: usize,
}

/// Combines request-level text and leading system history into the initial prompt.
///
/// Upstream `splitSystemPrompt`. Three details are load-bearing: pieces are joined with a blank line
/// (`"\n\n"`), empty pieces are dropped so an absent request-level prompt does not leave a leading
/// blank line, and only the *leading run* of system messages is consumed — a system message after a
/// user turn is a mid-conversation instruction, not part of the prompt.
#[must_use]
pub fn split_system_prompt(request: &PiRequest) -> SystemPromptSplit {
    let mut end = 0;
    for message in &request.messages {
        if message.role != PiRequestRole::System {
            break;
        }
        end += 1;
    }
    let mut owned: Vec<String> = Vec::new();
    if let Some(system) = request.system.clone() {
        owned.push(system);
    }
    for message in request.messages.iter().take(end) {
        owned.push(flatten_text(&message.content));
    }
    let joined = owned
        .into_iter()
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<String>>()
        .join("\n\n");
    SystemPromptSplit {
        system_prompt: if joined.is_empty() {
            None
        } else {
            Some(joined)
        },
        history_start: end,
    }
}

/// Joins the text blocks of one message, ignoring every other block kind.
///
/// Upstream `flattenText`. The filter is exact: an image or file block contributes nothing, which on
/// the text-only path is how a file handle silently disappears — see [`to_pi_context`]'s note.
#[must_use]
pub fn flatten_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Rejects misplaced tool-change blocks and unsupported image roles before anything is converted.
///
/// Upstream `assertSupportedHistory`. Each refusal names a request that has no faithful pi-ai
/// representation, so converting it anyway would send a message the model reads differently from
/// what the author wrote.
///
/// # Errors
///
/// [`UNSUPPORTED_CONTENT`] for a tool change outside a developer message, an image in a role that
/// cannot carry one, or a non-text system message.
pub fn assert_supported_history(messages: &[PiRequestMessage]) -> Result<(), ContextError> {
    for message in messages {
        let has_tool_change = !message.tools_added.is_empty() || !message.tools_removed.is_empty();
        if message.role != PiRequestRole::Developer && has_tool_change {
            return Err(ContextError::new(
                UNSUPPORTED_CONTENT,
                "Tool-change blocks require developer role",
            ));
        }
        let carries_image =
            girder_core::session::content_projection::content_has_image(&message.content);
        if message.role != PiRequestRole::User
            && message.role != PiRequestRole::Tool
            && carries_image
        {
            return Err(ContextError::new(
                UNSUPPORTED_CONTENT,
                format!(
                    "pi-ai cannot represent an image in an in-history {} message",
                    message.role.tag()
                ),
            ));
        }
        if message.role == PiRequestRole::System
            && message
                .content
                .iter()
                .any(|block| !matches!(block, ContentBlock::Text { .. }))
        {
            return Err(ContextError::new(
                UNSUPPORTED_CONTENT,
                "pi-ai cannot represent non-text system messages",
            ));
        }
    }
    Ok(())
}

/// Parses the declared tools that are actually sent.
///
/// Upstream `toolsOf`. Returns an empty vector both when nothing is declared and when everything is
/// deferred; [`PiContext`] then omits the field either way, which is why the two cases need no
/// distinction here.
#[must_use]
pub fn sent_tools(request: &PiRequest) -> Vec<PiTool> {
    request
        .tools
        .iter()
        .filter(|tool| !tool.defer_loading)
        .map(|tool| PiTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        })
        .collect()
}

/// Converts harness request history into a pi-ai `Context`, text only.
///
/// Upstream's synchronous `toPiContext` overload. It is synchronous precisely because it does not
/// touch the attachment store: an image anywhere in the history is a failure rather than an
/// omission, since silently dropping it would send a request the person did not ask for.
///
/// # What the model ends up reading
///
/// - The leading system messages and the request-level system text become one prompt, joined with a
///   blank line.
/// - A later system message becomes either a native system message (when the model supports one) or
///   a `<system-reminder>`-wrapped **user** message. That wrap is visible to the model, so the
///   closing tag is escaped: unescaped, the text could close the wrapper early and promote its
///   remainder to instructions.
/// - A developer message becomes one system message carrying its text plus the tool declarations it
///   enables and the names it withdraws.
/// - A tool result carries the *name* recovered from the preceding assistant turn, which the
///   protocol requires and the durable history does not store.
///
/// # Deliberate fidelity note
///
/// A file block in a user message contributes nothing: upstream's flattening keeps only text blocks,
/// so a file's handle text is not sent on this path. That is upstream behaviour, reproduced here
/// rather than improved, because the image path is where file handles are rendered.
///
/// # Errors
///
/// [`ContextError`] with `UNSUPPORTED_CONTENT` for every case [`assert_supported_history`] refuses
/// and for any image at all, or with `INVALID_REQUEST` for a developer update naming a tool the
/// request never declared.
pub fn to_pi_context(
    request: &PiRequest,
    supports_mid_convo_system_messages: bool,
) -> Result<PiContext, ContextError> {
    assert_supported_history(&request.messages)?;

    let split = split_system_prompt(request);
    let mut tool_names: BTreeMap<String, String> = BTreeMap::new();
    let mut messages: Vec<PiMessage> = Vec::new();

    for message in request.messages.iter().skip(split.history_start) {
        if girder_core::session::content_projection::content_has_image(&message.content) {
            return Err(ContextError::new(
                UNSUPPORTED_CONTENT,
                "pi-ai image conversion requires the durable attachment service",
            ));
        }
        match message.role {
            PiRequestRole::Developer => {
                if let Some(system) = developer_message(message, request)? {
                    messages.push(system);
                }
            }
            PiRequestRole::System => {
                if let Some(system) = system_message(message, supports_mid_convo_system_messages) {
                    messages.push(system);
                }
            }
            PiRequestRole::Assistant => {
                let assistant = foreign_assistant(&message.content, "", "")?;
                for block in &assistant.content {
                    if let PiAssistantBlock::ToolCall { id, name, .. } = block {
                        tool_names.insert(id.clone(), name.clone());
                    }
                }
                messages.push(PiMessage::Assistant(assistant));
            }
            PiRequestRole::Tool => {
                messages.push(tool_result_message(message, &tool_names));
            }
            PiRequestRole::User => {
                messages.push(PiMessage::User {
                    text: flatten_text(&message.content),
                    timestamp: 0,
                });
            }
        }
    }

    Ok(PiContext {
        system_prompt: split.system_prompt,
        messages,
        tools: sent_tools(request),
    })
}

/// Builds the system message a developer update becomes, or nothing when it carries no change.
fn developer_message(
    message: &PiRequestMessage,
    request: &PiRequest,
) -> Result<Option<PiMessage>, ContextError> {
    let mut blocks: Vec<String> = Vec::new();
    let mut added: Vec<PiTool> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    blocks.push(text.clone());
                }
            }
            // A developer message may carry only text; its tool changes live in the two fields.
            other => {
                return Err(ContextError::new(
                    UNSUPPORTED_CONTENT,
                    format!(
                        "pi-ai cannot represent developer content {}",
                        other.type_name()
                    ),
                ));
            }
        }
    }
    for name in &message.tools_added {
        let declaration = request
            .tools
            .iter()
            .find(|tool| tool.name == *name)
            .ok_or_else(|| {
                ContextError::new(
                    INVALID_REQUEST,
                    format!("pi-ai tool update references undeclared tool \"{name}\""),
                )
            })?;
        added.push(PiTool {
            name: declaration.name.clone(),
            description: declaration.description.clone(),
            parameters: declaration.parameters.clone(),
        });
    }
    for name in &message.tools_removed {
        removed.push(name.clone());
    }
    if blocks.is_empty() && added.is_empty() && removed.is_empty() {
        return Ok(None);
    }
    Ok(Some(PiMessage::System {
        content: PiSystemContent::Blocks(blocks),
        timestamp: 0,
        tools_added: added,
        tools_removed: removed,
    }))
}

/// Builds the message a mid-conversation system message becomes, or nothing when it is empty.
///
/// The reminder wrap is the fallback for a model that cannot take a system message mid-conversation.
/// Escaping is applied to the *closing* tag only, and by replacing the literal text anywhere in the
/// body — a body that writes the tag twice is escaped twice, which is correct.
fn system_message(
    message: &PiRequestMessage,
    supports_mid_convo_system_messages: bool,
) -> Option<PiMessage> {
    let content = flatten_text(&message.content);
    if content.is_empty() {
        return None;
    }
    if supports_mid_convo_system_messages {
        return Some(PiMessage::System {
            content: PiSystemContent::Text(content),
            timestamp: 0,
            tools_added: Vec::new(),
            tools_removed: Vec::new(),
        });
    }
    let reminder = content.replace("</system-reminder>", "<\\/system-reminder>");
    Some(PiMessage::User {
        text: format!("<system-reminder>\n{reminder}\n</system-reminder>"),
        timestamp: 0,
    })
}

/// Builds one tool result, recovering the tool's name from the preceding assistant turn.
fn tool_result_message(
    message: &PiRequestMessage,
    tool_names: &BTreeMap<String, String>,
) -> PiMessage {
    let call_id = message.tool_call_id.clone().unwrap_or_default();
    let text = flatten_text(&message.content);
    let content = vec![if text.is_empty() {
        EMPTY_TOOL_OUTPUT.to_owned()
    } else {
        text
    }];
    PiMessage::ToolResult {
        tool_name: tool_names
            .get(&call_id)
            .cloned()
            .unwrap_or_else(|| UNKNOWN_TOOL_NAME.to_owned()),
        tool_call_id: call_id,
        content,
        is_error: message.is_error,
        timestamp: 0,
    }
}

/// Converts provider-neutral blocks into an assistant message that claims no replay state.
///
/// Upstream `foreignAssistant`. Two choices make the degrade safe:
///
/// - `api` is [`FOREIGN_API`], never a catalog API, so the message can never be mistaken for
///   same-model replay even when its durable source names the request's own provider and model.
/// - The terminal state is *derived* from the content — `toolUse` when any call is present,
///   otherwise `stop` — because a reconstructed history message has no provider verdict to report.
///
/// The arguments are re-parsed from the model's raw JSON, because pi-ai expects a parsed object. A
/// raw string that is not a JSON object becomes `{}` rather than a failure: the model's malformed
/// call is worth replaying as a call with no arguments, and refusing would make the whole history
/// unconvertible.
///
/// # Errors
///
/// [`UNSUPPORTED_CONTENT`] for an image block, which a chat history cannot represent as model
/// output.
pub fn foreign_assistant(
    content: &[ContentBlock],
    provider: &str,
    model: &str,
) -> Result<PiAssistant, ContextError> {
    let mut blocks: Vec<PiAssistantBlock> = Vec::with_capacity(content.len());
    for block in content {
        match block {
            ContentBlock::Text { text } => blocks.push(PiAssistantBlock::Text {
                text: text.clone(),
                signature: None,
            }),
            ContentBlock::Reasoning { text } => blocks.push(PiAssistantBlock::Thinking {
                thinking: text.clone(),
                signature: None,
                redacted: None,
            }),
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => blocks.push(PiAssistantBlock::ToolCall {
                id: id.as_str().to_owned(),
                name: name.clone(),
                arguments: parse_arguments(arguments),
                thought_signature: None,
                namespace: None,
            }),
            ContentBlock::Image { .. } => {
                return Err(ContextError::new(
                    UNSUPPORTED_CONTENT,
                    "pi-ai chat history cannot represent structured assistant image output",
                ));
            }
            // A file block is a reference, not model output; it contributes nothing.
            ContentBlock::File { .. } => {}
        }
    }
    let stop = if blocks
        .iter()
        .any(|block| matches!(block, PiAssistantBlock::ToolCall { .. }))
    {
        PiStop::ToolUse
    } else {
        PiStop::Stop
    };
    Ok(PiAssistant {
        content: blocks,
        api: FOREIGN_API.to_owned(),
        provider: provider.to_owned(),
        model: model.to_owned(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        usage: PiUsage::default(),
        stop,
        timestamp: 0,
    })
}

/// Parses a model's raw tool-call arguments into pi-ai's object form.
///
/// Upstream `parseArguments`: anything that is not a JSON object — malformed text, an array, a bare
/// number — becomes `{}`. Object is the only shape a tool call can have.
#[must_use]
pub fn parse_arguments(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) if value.is_object() => value,
        _ => Value::Object(Map::new()),
    }
}

// ---------------------------------------------------------------------------------------------
// Prefix binding
// ---------------------------------------------------------------------------------------------

/// Recorded request-prefix digests of reconstructed assistant messages.
///
/// Keyed by the message's index in the context being bound, which is upstream's object identity
/// under a language where messages are values. Binding never reorders the list, so the index names
/// the same message across the conversion that recorded the digest and the request that consumes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrefixDigests(pub BTreeMap<usize, String>);

impl PrefixDigests {
    /// An empty record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one message's digest.
    pub fn record(&mut self, index: usize, digest: impl Into<String>) {
        self.0.insert(index, digest.into());
    }

    /// The digest recorded for one message, if any.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        self.0.get(&index).map(String::as_str)
    }

    /// How many digests are recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A context to send, plus the digest that must be recorded with its response.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundContext {
    /// Context to send; equal to the input when nothing was removed.
    pub context: PiContext,
    /// Digest of the sent context, recorded with the response; `None` for other protocols.
    pub digest: Option<String>,
    /// How many messages had their signed thinking removed.
    pub omitted: usize,
}

/// Extends a digest chain by one value.
///
/// Upstream `chain`: `sha256(previous ‖ 0x00 ‖ JSON.stringify(value))`. The `NUL` separator is what
/// makes the chain unambiguous — without it, a previous digest ending in `a` chained with a value
/// starting with `b` would hash the same bytes as one ending in `ab` chained with something empty.
///
/// The hashing is [`crate::session::extra::digest_hex`], the crate's one `SHA-256`; this module
/// adds no second copy.
#[must_use]
pub fn chain_digest(previous: &str, value: &Value) -> String {
    let mut input = String::with_capacity(previous.len() + 1 + 32);
    input.push_str(previous);
    input.push('\0');
    push_json(&mut input, value);
    digest_hex(input.as_bytes())
}

/// Renders a JSON value the way `JSON.stringify` does, with object members sorted by key.
///
/// Sorting is the one deliberate difference: `serde_json`'s map is sorted by default and unsorted
/// when any crate in the graph enables `preserve_order`, and a digest that depends on that feature
/// unification would silently invalidate itself whenever an unrelated dependency changed. Sorting
/// explicitly makes the rendering a property of this build rather than of the feature graph.
///
/// Escaping is delegated to `serde_json` so there is no second escaper here; its output for a string
/// is the same text `JSON.stringify` produces.
#[must_use]
pub fn json_text(value: &Value) -> String {
    let mut out = String::new();
    push_json(&mut out, value);
    out
}

/// Appends one entity's rendering.
fn push_json(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => {
            // Serializing a `&str` yields its quoted, escaped JSON form.
            if let Ok(rendered) = serde_json::to_string(text) {
                out.push_str(&rendered);
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_json(out, item);
            }
            out.push(']');
        }
        Value::Object(members) => {
            let mut keys: Vec<&String> = members.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if let Ok(rendered) = serde_json::to_string(key) {
                    out.push_str(&rendered);
                }
                out.push(':');
                if let Some(member) = members.get(key) {
                    push_json(out, member);
                }
            }
            out.push('}');
        }
    }
}

/// The index of the assistant message whose tool round the request continues, or `None`.
///
/// Upstream `openToolRoundAssistant`. Scans back over a run of tool results; a run that ends at an
/// assistant message means the request is continuing *that* round, which matters because a manual
/// (budget-based) thinking configuration requires the round's assistant message to start with a
/// thinking block — removing it would make the request invalid rather than merely stale.
#[must_use]
pub fn open_tool_round_assistant(messages: &[PiMessage]) -> Option<usize> {
    let mut results = 0_usize;
    for (index, message) in messages.iter().enumerate().rev() {
        match message {
            PiMessage::Assistant(_) => {
                return if results > 0 { Some(index) } else { None };
            }
            PiMessage::ToolResult { .. } => results += 1,
            _ => return None,
        }
    }
    None
}

/// An assistant message with its signed thinking blocks removed.
fn without_signed_thinking(assistant: &PiAssistant) -> PiAssistant {
    let mut copied = assistant.clone();
    copied.content.retain(|block| !block.is_signed());
    copied
}

/// Whether an assistant message has any signed thinking to bind.
fn has_signed_thinking(assistant: &PiAssistant) -> bool {
    assistant.content.iter().any(PiAssistantBlock::is_signed)
}

/// Removes replayed signed thinking whose request prefix changed, and reports the digest to record.
///
/// Upstream `bindThinkingToPrefix`. The reasoning in one sentence: Anthropic can bind a signed
/// thinking block to the prefix it was generated under, and a request that replays a block from a
/// *different* prefix is rejected as a whole — so a block that cannot be proven to match must be
/// dropped, not risked.
///
/// The chain hashes the context as it is *sent*: the system prompt and tools first, then each
/// message in order, with a message's already-stripped form feeding the chain when it was stripped.
/// That is why a second request over the same history computes the same digests as the first.
///
/// # Which digest gets recorded
///
/// The digest a response stores is the chain value **at the point that response is about to be
/// appended** — the prefix it was generated from. So when the message at index `i` is checked, its
/// recorded digest is compared against the chain of messages `..i`, and [`BoundContext::digest`]
/// after the last message is the value to record alongside *this* request's answer. Recording the
/// post-message digest instead would make a history never validate against itself.
///
/// Upstream reports omissions through a callback; this returns the count in [`BoundContext::omitted`],
/// which makes "called once with a non-zero count" a value the caller cannot forget to read.
#[must_use]
pub fn bind_thinking_to_prefix(
    context: &PiContext,
    api: &str,
    prefix_digests: &PrefixDigests,
    manual_thinking: bool,
) -> BoundContext {
    if api != PREFIX_BOUND_API {
        return BoundContext {
            context: context.clone(),
            digest: None,
            omitted: 0,
        };
    }
    let kept = if manual_thinking {
        open_tool_round_assistant(&context.messages)
    } else {
        None
    };

    let mut head = Map::new();
    head.insert(
        "systemPrompt".to_owned(),
        match &context.system_prompt {
            Some(prompt) => Value::String(prompt.clone()),
            None => Value::Null,
        },
    );
    head.insert(
        "tools".to_owned(),
        if context.tools.is_empty() {
            Value::Null
        } else {
            Value::Array(context.tools.iter().map(tool_json).collect())
        },
    );
    let mut digest = chain_digest(DIGEST_SEED, &Value::Object(head));

    let mut messages: Option<Vec<PiMessage>> = None;
    let mut omitted = 0_usize;
    for (index, original) in context.messages.iter().enumerate() {
        let mut message = original.clone();
        if let PiMessage::Assistant(assistant) = &message {
            let stale = assistant.api == PREFIX_BOUND_API
                && Some(index) != kept
                && prefix_digests.get(index) != Some(digest.as_str())
                && has_signed_thinking(assistant);
            if stale {
                message = PiMessage::Assistant(without_signed_thinking(assistant));
                let copy = messages.get_or_insert_with(|| context.messages.clone());
                if let Some(slot) = copy.get_mut(index) {
                    *slot = message.clone();
                }
                omitted += 1;
            }
        }
        digest = chain_digest(&digest, &message_json(&message));
    }

    match messages {
        None => BoundContext {
            context: context.clone(),
            digest: Some(digest),
            omitted: 0,
        },
        Some(replaced) => BoundContext {
            context: PiContext {
                system_prompt: context.system_prompt.clone(),
                messages: replaced,
                tools: context.tools.clone(),
            },
            digest: Some(digest),
            omitted,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// pi-ai JSON projection
// ---------------------------------------------------------------------------------------------

/// Renders one tool declaration.
#[must_use]
pub fn tool_json(tool: &PiTool) -> Value {
    let mut members = Map::new();
    members.insert("name".to_owned(), Value::String(tool.name.clone()));
    members.insert(
        "description".to_owned(),
        Value::String(tool.description.clone()),
    );
    members.insert("parameters".to_owned(), tool.parameters.clone());
    Value::Object(members)
}

/// Renders one message the way pi-ai serializes it.
///
/// Only the fields pi-ai's wire shape carries, and only when they exist: an absent optional member is
/// absent, not `null`, because the digest and the provider's view both distinguish the two.
#[must_use]
pub fn message_json(message: &PiMessage) -> Value {
    let mut members = Map::new();
    match message {
        PiMessage::System {
            content,
            timestamp,
            tools_added,
            tools_removed,
        } => {
            members.insert("role".to_owned(), Value::String("system".to_owned()));
            members.insert(
                "content".to_owned(),
                match content {
                    PiSystemContent::Text(text) => Value::String(text.clone()),
                    PiSystemContent::Blocks(blocks) => Value::Array(
                        blocks
                            .iter()
                            .map(|text| text_block(text))
                            .collect::<Vec<Value>>(),
                    ),
                },
            );
            members.insert("timestamp".to_owned(), Value::from(*timestamp));
            if !tools_added.is_empty() {
                members.insert(
                    "toolsAdded".to_owned(),
                    Value::Array(tools_added.iter().map(tool_json).collect()),
                );
            }
            if !tools_removed.is_empty() {
                members.insert(
                    "toolsRemoved".to_owned(),
                    Value::Array(
                        tools_removed
                            .iter()
                            .map(|name| {
                                let mut removed = Map::new();
                                removed.insert("name".to_owned(), Value::String(name.clone()));
                                Value::Object(removed)
                            })
                            .collect(),
                    ),
                );
            }
        }
        PiMessage::User { text, timestamp } => {
            members.insert("role".to_owned(), Value::String("user".to_owned()));
            members.insert("content".to_owned(), Value::String(text.clone()));
            members.insert("timestamp".to_owned(), Value::from(*timestamp));
        }
        PiMessage::Assistant(assistant) => return assistant_json(assistant),
        PiMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            is_error,
            timestamp,
        } => {
            members.insert("role".to_owned(), Value::String("toolResult".to_owned()));
            members.insert("toolCallId".to_owned(), Value::String(tool_call_id.clone()));
            members.insert("toolName".to_owned(), Value::String(tool_name.clone()));
            members.insert(
                "content".to_owned(),
                Value::Array(content.iter().map(|text| text_block(text)).collect()),
            );
            members.insert("isError".to_owned(), Value::Bool(*is_error));
            members.insert("timestamp".to_owned(), Value::from(*timestamp));
        }
    }
    Value::Object(members)
}

/// Renders one assistant message.
#[must_use]
pub fn assistant_json(assistant: &PiAssistant) -> Value {
    let mut members = Map::new();
    members.insert("role".to_owned(), Value::String("assistant".to_owned()));
    members.insert(
        "content".to_owned(),
        Value::Array(assistant.content.iter().map(block_json).collect()),
    );
    members.insert("api".to_owned(), Value::String(assistant.api.clone()));
    members.insert(
        "provider".to_owned(),
        Value::String(assistant.provider.clone()),
    );
    members.insert("model".to_owned(), Value::String(assistant.model.clone()));
    for (key, value) in [
        ("responseModel", assistant.response_model.as_ref()),
        ("responseId", assistant.response_id.as_ref()),
        (
            "providerThinkingLevel",
            assistant.provider_thinking_level.as_ref(),
        ),
    ] {
        if let Some(value) = value {
            members.insert(key.to_owned(), Value::String(value.clone()));
        }
    }
    members.insert("usage".to_owned(), usage_json(&assistant.usage));
    members.insert(
        "stopReason".to_owned(),
        Value::String(assistant.stop.tag().to_owned()),
    );
    members.insert("timestamp".to_owned(), Value::from(assistant.timestamp));
    Value::Object(members)
}

/// Renders one assistant content block.
#[must_use]
pub fn block_json(block: &PiAssistantBlock) -> Value {
    let mut members = Map::new();
    match block {
        PiAssistantBlock::Text { text, signature } => {
            members.insert("type".to_owned(), Value::String("text".to_owned()));
            members.insert("text".to_owned(), Value::String(text.clone()));
            if let Some(signature) = signature {
                members.insert("textSignature".to_owned(), Value::String(signature.clone()));
            }
        }
        PiAssistantBlock::Thinking {
            thinking,
            signature,
            redacted,
        } => {
            members.insert("type".to_owned(), Value::String("thinking".to_owned()));
            members.insert("thinking".to_owned(), Value::String(thinking.clone()));
            if let Some(signature) = signature {
                members.insert(
                    "thinkingSignature".to_owned(),
                    Value::String(signature.clone()),
                );
            }
            if let Some(redacted) = redacted {
                members.insert("redacted".to_owned(), Value::Bool(*redacted));
            }
        }
        PiAssistantBlock::ToolCall {
            id,
            name,
            arguments,
            thought_signature,
            namespace,
        } => {
            members.insert("type".to_owned(), Value::String("toolCall".to_owned()));
            members.insert("id".to_owned(), Value::String(id.clone()));
            members.insert("name".to_owned(), Value::String(name.clone()));
            members.insert("arguments".to_owned(), arguments.clone());
            if let Some(signature) = thought_signature {
                members.insert(
                    "thoughtSignature".to_owned(),
                    Value::String(signature.clone()),
                );
            }
            if let Some(namespace) = namespace {
                members.insert("namespace".to_owned(), Value::String(namespace.clone()));
            }
        }
    }
    Value::Object(members)
}

/// Renders one usage record.
///
/// `cost` is written as zeros because a reconstructed history message was never billed: the field is
/// part of pi-ai's shape, not a measured value here.
#[must_use]
pub fn usage_json(usage: &PiUsage) -> Value {
    let mut cost = Map::new();
    for key in ["input", "output", "cacheRead", "cacheWrite", "total"] {
        cost.insert(key.to_owned(), Value::from(0_u8));
    }
    let mut members = Map::new();
    members.insert("input".to_owned(), Value::from(usage.input));
    members.insert("output".to_owned(), Value::from(usage.output));
    members.insert("cacheRead".to_owned(), Value::from(usage.cache_read));
    members.insert("cacheWrite".to_owned(), Value::from(usage.cache_write));
    members.insert("totalTokens".to_owned(), Value::from(usage.total));
    members.insert("cost".to_owned(), Value::Object(cost));
    Value::Object(members)
}

/// One text block.
fn text_block(text: &str) -> Value {
    let mut members = Map::new();
    members.insert("type".to_owned(), Value::String("text".to_owned()));
    members.insert("text".to_owned(), Value::String(text.to_owned()));
    Value::Object(members)
}
