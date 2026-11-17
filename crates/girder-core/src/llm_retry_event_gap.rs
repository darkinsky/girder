//! The two durable retry events: their payload shape, its validation, and the chain rules.
//!
//! Upstream path: `packages/llm/llm-retry` (318 lines across `index.ts`, `types.ts`, `brand.ts`).
//!
//! # What `llm-retry` is, in full
//!
//! The package is a policy **executor**, not a policy. It registers a `sessionProjections`
//! projector under the key `llmRetry`, then intercepts `agent/request-error` and decides whether a
//! failed model request is retried. Its 318 lines carry seven distinct responsibilities:
//!
//! | # | responsibility | where |
//! |---|---|---|
//! | 1 | an empty config that *refuses* a misplaced `retryPolicy` key | `validateConfig` |
//! | 2 | the `llmRetry` projection: `(provider, policyKey)` → `{retry, retryId}` | `apply`'s `register` |
//! | 3 | a cancellable delay that aborts with the turn or the plugin lifetime | `cancellableDelay` |
//! | 4 | the retry decision: `always` consults downstream first, `normal` filters by code and count | `recover` |
//! | 5 | the local backoff arithmetic and the provider's `providerRetryAfterMs` override | `localDelay`, `recover` |
//! | 6 | the policy key that names one chain, and the state key that names one counter | `retryPolicyKey`, `retryStateKey` |
//! | 7 | the two durable events, written before the wait and after it | `backoff` |
//!
//! # What this crate already covers, and what it did not
//!
//! Responsibilities 1, 3, 4, and 5 are present: [`crate::retry`] carries the policy, the code set,
//! the backoff arithmetic and the jitter (273 lines, with [`crate::retry::RetryPolicy::delay_for`]
//! and [`crate::retry::Backoff::jittered`] mirroring `localDelay`), and
//! [`crate::retrying_provider`] executes it against a provider. [`crate::llm_extra_gap`] carries the
//! failure facts and their validation.
//!
//! Responsibilities 2, 6, and 7 were absent, and 7 was the loud one. Nothing in this crate ever
//! wrote either event, and nothing could: there was no type for either payload, and no rule for the
//! two strings (`policyKey`, and the `retryId` it shares) that make a chain readable. The
//! consequences are not cosmetic, because **readers already exist on the other side of the gap**:
//!
//! - `crate::session_migration` refuses to call an `llm/retry` row v4 unless it carries `retryId`,
//!   and its v0 edge *mints* one (`legacy-retry:<session>:<seq>`) purely so a migrated row has the
//!   member this crate never wrote.
//! - `crate::session_migration::RELEASED_V3_EVENT_TYPES` lists both `llm/retry` and
//!   `llm/retry-started`, so the v3 inventory knows the second event exists;
//!   `CURRENT_EVENT_TYPES`, `crate::session_extra_gap::KNOWN_EVENT_TYPES` and
//!   `crate::session_client_gap`'s type table do not. A log carrying a bare `llm/retry-started` is
//!   therefore refused as an unknown required type unless its writer marked it ignorable.
//! - `crate::session::SessionEvent::LlmRetry` claims the same tag with a *different* shape —
//!   `{attempt, reason}` — which no upstream harness has ever written.
//! - `crate::token_projection_gap` folds `SessionEvent::LlmRetry`, because a retry reopens the
//!   billing slot; `crate::session_transcript` drops it; `crate::session_digest` and
//!   `crate::session_timeline` skip it.
//!
//! So the tag has consumers and no producer. This module is the producer: the payload types, their
//! exact member sets, their validation, and the chain arithmetic that decides what the next
//! `retryId` and `retry` number are.
//!
//! # The shapes
//!
//! `llm/retry` is a discriminated union on `mode`:
//!
//! ```json
//! {"retryId":"…","turn":1,"step":1,"provider":"mock","mode":"normal","policyKey":"…",
//!  "retry":1,"maxRetries":2,"delayMs":500,
//!  "failure":{"message":"busy","code":"RATE_LIMIT","status":429}}
//! {"retryId":"…","turn":1,"step":1,"provider":"mock","mode":"always","policyKey":"…",
//!  "retry":1,"delayMs":750,"failure":{"message":"provider busy","code":"RATE_LIMIT","status":429}}
//! ```
//!
//! `maxRetries` is **required** in `normal` and **forbidden** in `always` — the union is not a
//! suggestion, and a payload carrying both is refused.
//!
//! `llm/retry-started` is the transition written after the wait succeeds and before the next attempt
//! begins, and it is deliberately minimal: it repeats only the identity and coordinates, never the
//! policy, so a reader cannot mistake it for a second schedule.
//!
//! ```json
//! {"retryId":"…","turn":1,"step":1,"retry":1}
//! ```
//!
//! # Why `retryId` and `policyKey` are the load-bearing pair
//!
//! `policyKey` is a canonical serialization of the whole resolved policy, so a provider whose
//! policy is reconfigured mid-session produces a *different* key and therefore a *different* chain
//! — which is exactly what upstream's replacement test asserts. `retryId` is minted once per chain
//! and carried by every attempt in it, which is what lets `llm/retry-started` find its schedule
//! without repeating `provider` or `policyKey`. [`policy_key`] reproduces `retryPolicyKey` byte for
//! byte, including JavaScript's number formatting, because a key that differs by `"500"` versus
//! `"500.0"` would silently split one chain into two.
//!
//! # What this module deliberately does not do
//!
//! - **It does not mint random ids.** Upstream's mint is `randomUUID()`; a pure module cannot own
//!   randomness. [`Chain::retry_id`] takes the freshly minted candidate as an argument and applies
//!   upstream's reuse rule, so the random source stays with the caller and the rule stays testable.
//! - **It does not write to a session.** Appending is the session store's job; this produces the
//!   [`serde_json::Value`] a writer appends.
//! - **It does not decide whether to retry.** [`crate::retry::RetryPolicy::should_retry`] already
//!   does, and [`Chain::should_schedule`] only adds the one rule the projection's counter imposes:
//!   `normal` stops at `maxRetries` *counted over the chain*, which is a different question from
//!   "is this code retryable".

// The crate root does not declare this module; the integrator owns the line and its `pub use`.
// Until then nothing in a library build has a caller, so the whole surface would read as waste. The
// test target exercises every item below.
#![allow(dead_code)]

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use girder_core::MAX_SAFE_INTEGER;
use girder_core::llm_extra_gap::{MAX_TIMER_DELAY_MS, ProviderError};

/// The largest `delayMs` a payload may carry, as a JSON number.
///
/// `setTimeout` clamps a delay to a signed 32-bit millisecond count, so a larger value in a durable
/// record describes a wait that could never have happened. Kept as an `f64` because `delayMs` is a
/// number rather than an integer: jittered backoff legitimately produces `500.0000000000001`, and
/// rounding it would misreport the wait that was actually scheduled. Derived from
/// [`MAX_TIMER_DELAY_MS`] rather than restated, so the two cannot drift.
#[allow(
    clippy::cast_precision_loss,
    reason = "the value is 2^31-1, far below 2^52, so f64 represents it exactly"
)]
pub const MAX_DELAY_MS: f64 = MAX_TIMER_DELAY_MS as f64;

/// The stable identity shared by every attempt in one retry chain.
///
/// Upstream's `RetryId` is a [`Branded`] string with **no validation at all** — `RetryId(id)` is a
/// cast. The non-emptiness is enforced where it matters instead, by [`validate`], which is the same
/// division of labour the payload validator upstream uses (`nonEmptyString(data['retryId'], …)`).
///
/// Diverging from [`crate::Id`] is deliberate: this value is minted by the retry executor rather
/// than supplied by a caller, and rejecting a `randomUUID()`-shaped value would be rejecting `Id`'s
/// own `:`-and-alphanumeric alphabet for no gain.
///
/// [`Branded`]: https://www.typescriptlang.org/docs/handbook/2/nominal-typing.html
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RetryId(String);

impl RetryId {
    /// Wraps a caller-minted identity.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RetryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for RetryId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RetryId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

/// Which retry policy produced an event.
///
/// Deliberately narrower than [`crate::retry::RetryMode`], which also carries the retryable code
/// list. The payload never spells the codes out — they are folded into `policyKey` — so a wider
/// enum here would invent a member the wire format does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetryMode {
    /// Bounded: at most `maxRetries` retries per chain.
    Normal,
    /// Unbounded: retries until success, cancellation, or disposal.
    Always,
}

impl RetryMode {
    /// The wire spelling, which is also the `mode` member of the policy key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Always => "always",
        }
    }

    /// Reads the wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "normal" => Some(Self::Normal),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

/// A resolved policy, as the two events and the policy key need it.
///
/// Not [`crate::retry::RetryPolicy`]: that type's `Normal` mode carries the *codes* while this one
/// carries `maxRetries`, and its delay bounds are [`std::time::Duration`]s rather than the
/// millisecond numbers the wire format and `setTimeout` speak. Converting is the integrator's job;
/// keeping the payload honest about millimetres is this module's.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPolicy {
    /// The mode.
    pub mode: RetryMode,
    /// Eligible failure codes. Ignored when the mode is [`RetryMode::Always`], exactly as upstream's
    /// `retryPolicyKey` ignores them.
    pub retryable_codes: Vec<String>,
    /// Maximum retries per chain, when bounded.
    pub max_retries: Option<u32>,
    /// Delay before the first retry, in milliseconds.
    pub initial_delay_ms: f64,
    /// Ceiling for any single delay, in milliseconds.
    pub max_delay_ms: f64,
    /// Symmetric jitter ratio around one, in `[0, 1]`.
    pub jitter_ratio: f64,
}

impl Default for ResolvedPolicy {
    /// The resolved defaults `resolveRetryPolicy(undefined, …)` produces.
    fn default() -> Self {
        Self {
            mode: RetryMode::Normal,
            retryable_codes: girder_core::llm_extra_gap::DEFAULT_RETRYABLE
                .iter()
                .map(|code| (*code).to_owned())
                .collect(),
            max_retries: Some(5),
            initial_delay_ms: 500.0,
            max_delay_ms: 10_000.0,
            jitter_ratio: 0.1,
        }
    }
}

/// The `llm/retry` payload: one scheduled attempt, recorded before its cancellable wait.
///
/// One struct rather than an enum, because every member but `max_retries` is shared and a reader
/// that only wants `retry` should not have to match twice. The discriminated union survives in
/// [`Self::validate`], which enforces `normal ⇒ max_retries.is_some()` and
/// `always ⇒ max_retries.is_none()`, so a hand-built value cannot smuggle the wrong arm onto the
/// wire.
#[derive(Clone, Debug, PartialEq)]
pub struct RetryEvent {
    /// The chain this attempt belongs to.
    pub retry_id: RetryId,
    /// Which chain this attempt advanced: the four members upstream compares.
    pub chain: ChainKey,
    /// The mode, which selects the arm.
    pub mode: RetryMode,
    /// Which attempt this is, starting at 1.
    pub retry: u64,
    /// The bound, present only in [`RetryMode::Normal`].
    pub max_retries: Option<u32>,
    /// The wait actually scheduled, in milliseconds.
    pub delay_ms: f64,
    /// Why the previous attempt failed.
    pub failure: ProviderError,
}

/// The four members that decide which chain an attempt belongs to.
///
/// Upstream compares exactly these — `['turn', 'step', 'provider', 'policyKey'].every(…)` in
/// `assertRetryChain`, and the same tuple is what `retryStateKey` keys the projection by. Naming the
/// tuple is what makes "a changed policy starts a fresh counter" a property of one comparison rather
/// than four that a caller could forget to extend.
///
/// `retry` deliberately stays out: it is the *contents* of a chain, not its name, and including it
/// would make every attempt its own key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChainKey {
    /// Turn it belongs to.
    pub turn: u64,
    /// Step number within the turn.
    pub step: u64,
    /// The provider route that failed.
    pub provider: String,
    /// The canonical policy key naming the chain.
    pub policy_key: String,
}

impl ChainKey {
    /// The key naming the chain at one place under one policy.
    #[must_use]
    pub fn new(
        turn: u64,
        step: u64,
        provider: impl Into<String>,
        policy_key: impl Into<String>,
    ) -> Self {
        Self {
            turn,
            step,
            provider: provider.into(),
            policy_key: policy_key.into(),
        }
    }
}

impl RetryEvent {
    /// A bounded attempt.
    #[must_use]
    pub fn normal(
        retry_id: RetryId,
        chain: ChainKey,
        retry: u64,
        max_retries: u32,
        delay_ms: f64,
        failure: ProviderError,
    ) -> Self {
        Self {
            retry_id,
            chain,
            mode: RetryMode::Normal,
            retry,
            max_retries: Some(max_retries),
            delay_ms,
            failure,
        }
    }

    /// An unbounded attempt, which must omit `maxRetries`.
    #[must_use]
    pub fn always(
        retry_id: RetryId,
        chain: ChainKey,
        retry: u64,
        delay_ms: f64,
        failure: ProviderError,
    ) -> Self {
        Self {
            retry_id,
            chain,
            mode: RetryMode::Always,
            retry,
            max_retries: None,
            delay_ms,
            failure,
        }
    }

    /// Whether this event may be written at all.
    ///
    /// # Errors
    ///
    /// Returns the first [`PayloadError`] found, so the message names the member to fix. Every rule
    /// here is one `payload-validation.ts` applies to `llm/retry`; none is an invention.
    pub fn validate(&self) -> Result<(), PayloadError> {
        non_empty("retryId", self.retry_id.as_str())?;
        count("llm/retry", "turn", self.chain.turn)?;
        count("llm/retry", "step", self.chain.step)?;
        non_empty("provider", &self.chain.provider)?;
        non_empty("policyKey", &self.chain.policy_key)?;
        positive("retry", self.retry)?;
        match (self.mode, self.max_retries) {
            (RetryMode::Normal, None) => return Err(PayloadError::MaxRetriesMissing),
            (RetryMode::Normal, Some(max_retries)) => {
                positive("maxRetries", u64::from(max_retries))?;
                if self.retry > u64::from(max_retries) {
                    return Err(PayloadError::RetryExceedsMax {
                        retry: self.retry,
                        max_retries: u64::from(max_retries),
                    });
                }
            }
            (RetryMode::Always, Some(_)) => return Err(PayloadError::MaxRetriesInAlways),
            (RetryMode::Always, None) => {}
        }
        delay(self.delay_ms)?;
        validate_failure(&self.failure)
    }

    /// The wire payload.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Self::validate`] returns: an invalid payload is never encoded, because a
    /// writer that emitted one would put a fact in the log that no reader could trust.
    pub fn to_value(&self) -> Result<Value, PayloadError> {
        self.validate()?;
        let mut data = Map::new();
        data.insert("retryId".to_owned(), Value::from(self.retry_id.as_str()));
        data.insert("turn".to_owned(), Value::from(self.chain.turn));
        data.insert("step".to_owned(), Value::from(self.chain.step));
        data.insert(
            "provider".to_owned(),
            Value::from(self.chain.provider.as_str()),
        );
        data.insert("mode".to_owned(), Value::from(self.mode.as_str()));
        data.insert(
            "policyKey".to_owned(),
            Value::from(self.chain.policy_key.as_str()),
        );
        data.insert("retry".to_owned(), Value::from(self.retry));
        if let Some(max_retries) = self.max_retries {
            data.insert("maxRetries".to_owned(), Value::from(max_retries));
        }
        data.insert("delayMs".to_owned(), json_number(self.delay_ms));
        data.insert("failure".to_owned(), failure_to_value(&self.failure));
        Ok(Value::Object(data))
    }

    /// Reads a wire payload, then validates it.
    ///
    /// The member sets are exact: an absent required member and an unknown extra member are both
    /// refusals, which is what stops an `always` payload from carrying `maxRetries` "harmlessly".
    ///
    /// # Errors
    ///
    /// Returns [`PayloadError`] for a shape this crate cannot read, or an invariant it cannot hold.
    pub fn from_value(data: &Value) -> Result<Self, PayloadError> {
        let label = "llm/retry";
        let object = exact(
            data,
            label,
            &[
                "retryId",
                "turn",
                "step",
                "provider",
                "mode",
                "policyKey",
                "retry",
                "delayMs",
                "failure",
            ],
            &["maxRetries"],
        )?;
        let mode_text = string_of(object, "mode", label)?;
        let mode = RetryMode::parse(&mode_text).ok_or_else(|| PayloadError::BadMode {
            label: label.to_owned(),
            found: mode_text,
        })?;
        let event = Self {
            retry_id: RetryId::new(string_of(object, "retryId", label)?),
            chain: ChainKey {
                turn: count_of(object, "turn", label)?,
                step: count_of(object, "step", label)?,
                provider: string_of(object, "provider", label)?,
                policy_key: string_of(object, "policyKey", label)?,
            },
            mode,
            retry: count_of(object, "retry", label)?,
            max_retries: match object.get("maxRetries") {
                Some(value) => Some(
                    u32::try_from(count_of_value(value, "maxRetries", label)?).map_err(|_| {
                        PayloadError::NotCount {
                            label: label.to_owned(),
                            key: "maxRetries".to_owned(),
                        }
                    })?,
                ),
                None => None,
            },
            delay_ms: delay_of(object, "delayMs", label)?,
            failure: failure_from_value(object.get("failure"), label)?,
        };
        event.validate()?;
        Ok(event)
    }
}

/// The `llm/retry-started` payload: the transition written after the wait succeeded.
///
/// Four members and no policy: this event says *an* attempt began, and the schedule it began under
/// is the `llm/retry` row it pairs with. Duplicating `provider` or `policyKey` here would create a
/// second copy that could disagree with the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryStarted {
    /// The chain this attempt belongs to.
    pub retry_id: RetryId,
    /// Turn it belongs to.
    pub turn: u64,
    /// Step number within the turn.
    pub step: u64,
    /// Which attempt this is, starting at 1.
    pub retry: u64,
}

impl RetryStarted {
    /// The event that pairs with `scheduled`.
    ///
    /// The only constructor, because the pairing *is* the invariant: a started event built from
    /// anything but a schedule would be an orphan the validator upstream refuses.
    #[must_use]
    pub fn after(scheduled: &RetryEvent) -> Self {
        Self {
            retry_id: scheduled.retry_id.clone(),
            turn: scheduled.chain.turn,
            step: scheduled.chain.step,
            retry: scheduled.retry,
        }
    }

    /// Whether this event may be written at all.
    ///
    /// # Errors
    ///
    /// Returns the first [`PayloadError`] found.
    pub fn validate(&self) -> Result<(), PayloadError> {
        non_empty("retryId", self.retry_id.as_str())?;
        count("llm/retry-started", "turn", self.turn)?;
        count("llm/retry-started", "step", self.step)?;
        positive("retry", self.retry)
    }

    /// The wire payload.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Self::validate`] returns.
    pub fn to_value(&self) -> Result<Value, PayloadError> {
        self.validate()?;
        let mut data = Map::new();
        data.insert("retryId".to_owned(), Value::from(self.retry_id.as_str()));
        data.insert("turn".to_owned(), Value::from(self.turn));
        data.insert("step".to_owned(), Value::from(self.step));
        data.insert("retry".to_owned(), Value::from(self.retry));
        Ok(Value::Object(data))
    }

    /// Reads a wire payload, then validates it.
    ///
    /// # Errors
    ///
    /// Returns [`PayloadError`] for a shape this crate cannot read, or an invariant it cannot hold.
    pub fn from_value(data: &Value) -> Result<Self, PayloadError> {
        let label = "llm/retry-started";
        let object = exact(data, label, &["retryId", "turn", "step", "retry"], &[])?;
        let event = Self {
            retry_id: RetryId::new(string_of(object, "retryId", label)?),
            turn: count_of(object, "turn", label)?,
            step: count_of(object, "step", label)?,
            retry: count_of(object, "retry", label)?,
        };
        event.validate()?;
        Ok(event)
    }
}

// ---- the two strings that make a chain readable ------------------------------------------------

/// Upstream's `retryPolicyKey`: one canonical string naming one resolved policy.
///
/// `normal` folds in the bound and the *sorted* code list; `always` folds in neither, because the
/// mode alone already says there is no bound and no filtering. The two modes therefore produce keys
/// with different arity, which is correct: an `always` policy and a `normal` one are different
/// chains even when their backoff is identical.
///
/// # Why the JS-compatible number formatting is not pedantry
///
/// The key is compared as a string, by a reader that may be JavaScript. `serde_json` would spell the
/// default backoff `500.0,10000.0,0.1`, and JavaScript spells it `500,10000,0.1`; a mismatch would
/// split every chain in two — silently, because both spellings are valid JSON numbers and neither
/// is an error. [`js_number`] reproduces `JSON.stringify`'s spelling exactly for that reason.
#[must_use]
pub fn policy_key(policy: &ResolvedPolicy) -> String {
    match policy.mode {
        RetryMode::Always => format!(
            "[{},{},{},{}]",
            quoted(RetryMode::Always.as_str()),
            js_number(policy.initial_delay_ms),
            js_number(policy.max_delay_ms),
            js_number(policy.jitter_ratio),
        ),
        RetryMode::Normal => {
            let mut codes: Vec<&str> = policy.retryable_codes.iter().map(String::as_str).collect();
            // `[...codes].sort()` is lexicographic by UTF-16 code unit. Every code this crate knows
            // is uppercase ASCII, where that order and Rust's are the same; sorting here rather than
            // trusting the caller is what makes the key canonical.
            codes.sort_unstable();
            let codes: Vec<String> = codes.into_iter().map(quoted).collect();
            format!(
                "[{},{},[{}],{},{},{}]",
                quoted(RetryMode::Normal.as_str()),
                policy.max_retries.unwrap_or(0),
                codes.join(","),
                js_number(policy.initial_delay_ms),
                js_number(policy.max_delay_ms),
                js_number(policy.jitter_ratio),
            )
        }
    }
}

/// Upstream's `retryStateKey`: the projection key under which one chain's counter lives.
///
/// The provider is part of the key because two routes can share a policy (and therefore a policy
/// key) while failing independently — an outage on one must not advance the other's counter.
#[must_use]
pub fn state_key(provider: &str, policy_key: &str) -> String {
    format!("[{},{}]", quoted(provider), quoted(policy_key))
}

/// `JSON.stringify`'s spelling of a number.
///
/// Three rules distinguish it from Rust's `Display`, and all three are reachable here: a number is
/// written in exponent form outside `(1e-6, 1e21)`, `-0` is written `0`, and a non-finite value is
/// written `null` because JSON has no spelling for it.
#[must_use]
pub fn js_number(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_owned();
    }
    // `to_bits() << 1 == 0` catches both zeros without comparing floats, and `-0` must become `0`
    // rather than `-0`, which is what `JSON.stringify` does.
    if value.to_bits() << 1 == 0 {
        return "0".to_owned();
    }
    let negative = value.is_sign_negative();
    // `LowerExp` yields the shortest round-tripping digits, which is the same digit string ECMA-262
    // asks for; only the layout rules differ.
    let text = format!("{:e}", value.abs());
    let (mantissa, exponent) = text
        .split_once('e')
        .map_or((text.as_str(), "0"), |(mantissa, exponent)| {
            (mantissa, exponent)
        });
    let digits: String = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let length = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    // `n` is ECMA-262's: the position of the decimal point relative to the digit string.
    let point = exponent + 1;

    let body = if length <= point && point <= 21 {
        // Integral, and short enough that JavaScript does not switch to exponent form.
        format!(
            "{digits}{}",
            "0".repeat(usize::try_from(point - length).unwrap_or(0))
        )
    } else if 0 < point && point <= 21 {
        let split = usize::try_from(point).unwrap_or(0);
        format!("{}.{}", &digits[..split], &digits[split..])
    } else if -6 < point && point <= 0 {
        format!(
            "0.{}{digits}",
            "0".repeat(usize::try_from(-point).unwrap_or(0))
        )
    } else {
        let power = point - 1;
        let sign = if power < 0 { '-' } else { '+' };
        let power = power.abs();
        if length == 1 {
            format!("{digits}e{sign}{power}")
        } else {
            format!("{}.{}e{sign}{power}", &digits[..1], &digits[1..])
        }
    };
    if negative { format!("-{body}") } else { body }
}

// ---- chain arithmetic --------------------------------------------------------------------------

/// One turn's retry chain, folded from the events already written.
///
/// This is upstream's `llmRetry` projection (`(provider, policyKey) → {retry, retryId}`) plus the
/// `assertRetryChain` relationship rules, which are two halves of one question: what may the next
/// event in this chain say? Keeping them together is what lets [`Self::admit`] reject a log that a
/// separate counter would have accepted.
#[derive(Clone, Debug, Default)]
pub struct Chain {
    scheduled: Vec<RetryEvent>,
    started: Vec<(RetryId, u64)>,
}

impl Chain {
    /// An empty chain, which is what a turn starts with.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recent attempt scheduled under this exact policy in this exact place.
    ///
    /// All four members are compared because the key is what upstream compares: a changed policy
    /// (different `policyKey`), a changed provider, or a moved step all start a fresh chain, and a
    /// counter that survived any of those would suppress a legitimate retry.
    #[must_use]
    pub fn prior(&self, event: &RetryEvent) -> Option<&RetryEvent> {
        self.scheduled
            .iter()
            .rev()
            .find(|candidate| candidate.chain == event.chain)
    }

    /// How many retries this chain has already scheduled.
    #[must_use]
    pub fn count(&self, event: &RetryEvent) -> u64 {
        self.prior(event).map_or(0, |prior| prior.retry)
    }

    /// The identity the next attempt in this chain must carry.
    ///
    /// Upstream's `previous?.retryId ?? RetryId(randomUUID())`: an attempt that continues a chain
    /// keeps the chain's identity, and one that opens a chain takes the freshly minted candidate.
    /// Minting is taken as an argument so this stays pure.
    ///
    /// # Errors
    ///
    /// Returns [`PayloadError::RetryIdReused`] when this would open a new chain with an identity
    /// another chain already used, which is the one case where the minted candidate is not usable.
    pub fn retry_id(&self, event: &RetryEvent, minted: RetryId) -> Result<RetryId, PayloadError> {
        if let Some(prior) = self.prior(event) {
            return Ok(prior.retry_id.clone());
        }
        if self
            .scheduled
            .iter()
            .any(|candidate| candidate.retry_id == minted)
        {
            return Err(PayloadError::RetryIdReused { retry_id: minted });
        }
        Ok(minted)
    }

    /// Whether this chain may schedule another attempt at all.
    ///
    /// `always` is unbounded by construction. `normal` stops once the chain has reached
    /// `maxRetries` — the projection's counter, not the mode's own arithmetic, which is why the
    /// bound is read from the event rather than from a policy.
    #[must_use]
    pub fn should_schedule(&self, event: &RetryEvent) -> bool {
        match event.max_retries {
            None => true,
            Some(max_retries) => self.count(event) < u64::from(max_retries),
        }
    }

    /// Admits one scheduled attempt, enforcing the chain rules.
    ///
    /// Three rules, all upstream's `assertRetryChain`: the attempt number must continue the chain,
    /// a continuing attempt must keep the chain's `retryId`, and an opening attempt must not borrow
    /// another chain's. A fourth comes from the payload itself: the event must be valid, because a
    /// chain folded over invalid rows proves nothing.
    ///
    /// # Errors
    ///
    /// Returns the first [`PayloadError`] found.
    pub fn admit(&mut self, event: &RetryEvent) -> Result<(), PayloadError> {
        event.validate()?;
        let prior = self.prior(event).cloned();
        let expected = prior.as_ref().map_or(1, |prior| prior.retry + 1);
        if event.retry != expected {
            return Err(PayloadError::RetrySkipsSequence {
                expected,
                found: event.retry,
            });
        }
        match prior {
            Some(prior) => {
                if prior.retry_id != event.retry_id {
                    return Err(PayloadError::RetryIdChanged {
                        expected: prior.retry_id,
                        found: event.retry_id.clone(),
                    });
                }
            }
            None => {
                if self
                    .scheduled
                    .iter()
                    .any(|candidate| candidate.retry_id == event.retry_id)
                {
                    return Err(PayloadError::RetryIdReused {
                        retry_id: event.retry_id.clone(),
                    });
                }
            }
        }
        self.scheduled.push(event.clone());
        Ok(())
    }

    /// Admits one started transition, enforcing that it pairs with a scheduled attempt.
    ///
    /// Upstream's three rules: an orphan is refused, a started event whose coordinates drifted from
    /// its schedule is refused, and a second start for the same attempt is refused (the wait
    /// completed once).
    ///
    /// # Errors
    ///
    /// Returns the first [`PayloadError`] found.
    pub fn admit_started(&mut self, started: &RetryStarted) -> Result<(), PayloadError> {
        started.validate()?;
        let Some(scheduled) = self
            .scheduled
            .iter()
            .find(|candidate| {
                candidate.retry_id == started.retry_id && candidate.retry == started.retry
            })
            .cloned()
        else {
            return Err(PayloadError::OrphanStarted {
                retry_id: started.retry_id.clone(),
                retry: started.retry,
            });
        };
        if scheduled.chain.turn != started.turn || scheduled.chain.step != started.step {
            return Err(PayloadError::StartedCoordinatesChanged {
                scheduled_turn: scheduled.chain.turn,
                scheduled_step: scheduled.chain.step,
                turn: started.turn,
                step: started.step,
            });
        }
        let key = (started.retry_id.clone(), started.retry);
        if self.started.contains(&key) {
            return Err(PayloadError::StartedRepeats {
                retry_id: started.retry_id.clone(),
                retry: started.retry,
            });
        }
        self.started.push(key);
        Ok(())
    }
}

// ---- refusals ----------------------------------------------------------------------------------

/// Why a retry payload is not a valid fact.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PayloadError {
    /// A payload, or one of its members, was not a `JSON` object.
    #[error("`{label}` must be an object")]
    NotAnObject {
        /// What was being read.
        label: String,
    },
    /// A required member was absent.
    #[error("`{label}` requires `{key}`")]
    MissingKey {
        /// What was being read.
        label: String,
        /// The member.
        key: &'static str,
    },
    /// A member the format does not define was present.
    #[error("`{label}` has no member `{key}`")]
    UnknownKey {
        /// What was being read.
        label: String,
        /// The member.
        key: String,
    },
    /// A string member was empty.
    #[error("`{key}` must be a non-empty string")]
    EmptyString {
        /// The member.
        key: &'static str,
    },
    /// A string member was not a string.
    #[error("`{label}` member `{key}` must be a string")]
    NotAString {
        /// What was being read.
        label: String,
        /// The member.
        key: &'static str,
    },
    /// A count was not a non-negative safe integer.
    #[error("`{label}` member `{key}` must be a non-negative safe integer")]
    NotCount {
        /// What was being read.
        label: String,
        /// The member.
        key: String,
    },
    /// A count that must be at least one was zero.
    #[error("`{key}` must be positive")]
    NotPositive {
        /// The member.
        key: &'static str,
    },
    /// `mode` was neither arm.
    #[error("`{label}` member `mode` must be `normal` or `always`, not `{found}`")]
    BadMode {
        /// What was being read.
        label: String,
        /// What was found.
        found: String,
    },
    /// A bounded attempt carried no bound.
    #[error("`normal` mode requires `maxRetries`")]
    MaxRetriesMissing,
    /// The unbounded arm carried a bound.
    #[error("`always` mode must omit `maxRetries`")]
    MaxRetriesInAlways,
    /// The attempt number exceeded the bound it was scheduled under.
    #[error("retry {retry} exceeds maxRetries {max_retries}")]
    RetryExceedsMax {
        /// The attempt number.
        retry: u64,
        /// The bound.
        max_retries: u64,
    },
    /// A delay was negative, `-0`, non-finite, or beyond [`MAX_DELAY_MS`].
    #[error("`delayMs` must be a finite number in [0, 2147483647], not `{found}`")]
    BadDelay {
        /// What was found, formatted.
        found: String,
    },
    /// The failure bundle was not a valid failure fact.
    ///
    /// The name is `violation` rather than `source` on purpose: `thiserror` treats a member named
    /// `source` as a [`std::error::Error`] chain link, and a [`girder_core::llm_extra_gap::Violation`]
    /// is a plain enum with no `Display`, so naming it `source` would demand an impl this crate
    /// would then have to add to a module it does not own.
    #[error("`failure` is not a valid failure: {violation:?}")]
    BadFailure {
        /// The underlying violation.
        violation: girder_core::llm_extra_gap::Violation,
    },
    /// The failure bundle carried a member the retry payload does not define.
    #[error("`failure` has no member `{key}`")]
    UnknownFailureKey {
        /// The member.
        key: String,
    },
    /// The next attempt did not continue its chain.
    #[error("`llm/retry` must use retry {expected}, not {found}")]
    RetrySkipsSequence {
        /// The number the chain requires.
        expected: u64,
        /// What was found.
        found: u64,
    },
    /// A continuing attempt changed its chain's identity.
    #[error("`llm/retry` must preserve retryId `{expected}` across one chain, not `{found}`")]
    RetryIdChanged {
        /// The chain's identity.
        expected: RetryId,
        /// What was found.
        found: RetryId,
    },
    /// A new chain borrowed an identity another chain already used.
    #[error("`llm/retry` reuses retryId `{retry_id}` across policy chains")]
    RetryIdReused {
        /// The borrowed identity.
        retry_id: RetryId,
    },
    /// A started transition had no schedule to pair with.
    #[error("`llm/retry-started` pairs no prior scheduled attempt `{retry_id}` #{retry}")]
    OrphanStarted {
        /// The identity it claimed.
        retry_id: RetryId,
        /// The attempt number it claimed.
        retry: u64,
    },
    /// A started transition moved its schedule's coordinates.
    #[error(
        "`llm/retry-started` changes scheduled coordinates ({scheduled_turn}, {scheduled_step}) to \
         ({turn}, {step})"
    )]
    StartedCoordinatesChanged {
        /// The scheduled turn.
        scheduled_turn: u64,
        /// The scheduled step.
        scheduled_step: u64,
        /// The turn it claimed.
        turn: u64,
        /// The step it claimed.
        step: u64,
    },
    /// A started transition repeated.
    #[error("`llm/retry-started` repeats attempt `{retry_id}` #{retry}")]
    StartedRepeats {
        /// The identity.
        retry_id: RetryId,
        /// The attempt number.
        retry: u64,
    },
}

// ---- field rules -------------------------------------------------------------------------------

fn non_empty(key: &'static str, value: &str) -> Result<(), PayloadError> {
    if value.is_empty() {
        return Err(PayloadError::EmptyString { key });
    }
    Ok(())
}

fn count(label: &str, key: &'static str, value: u64) -> Result<(), PayloadError> {
    if value > MAX_SAFE_INTEGER {
        return Err(PayloadError::NotCount {
            label: label.to_owned(),
            key: key.to_owned(),
        });
    }
    Ok(())
}

fn positive(key: &'static str, value: u64) -> Result<(), PayloadError> {
    if value == 0 {
        return Err(PayloadError::NotPositive { key });
    }
    Ok(())
}

/// A delay must be finite, not `-0`, non-negative, and within the timer range.
///
/// `-0` is refused separately from `0` because JavaScript's `Number.isFinite(-0)` is `true` and
/// `-0 >= 0` is `true`, so the only thing rejecting it upstream is an explicit `Object.is` clause —
/// it is a producer bug, not a rounding artifact, and coercing it would hide the bug.
fn delay(value: f64) -> Result<(), PayloadError> {
    let bad = || PayloadError::BadDelay {
        found: js_number(value),
    };
    if !value.is_finite() {
        return Err(bad());
    }
    // `-0` has the sign bit set and no other bits set.
    if value.to_bits() == (-0.0f64).to_bits() {
        return Err(bad());
    }
    if !(0.0..=MAX_DELAY_MS).contains(&value) {
        return Err(bad());
    }
    Ok(())
}

/// Validates a failure bundle as a retry payload carries it.
///
/// The field rules are [`girder_core::llm_extra_gap::ProviderError::validate`]'s, which is the same
/// `failureSnapshot` validator the rest of this crate already trusts. The one addition is
/// `offloadImages`: [`ProviderError`] can carry it, but an `llm/retry` payload cannot, because the
/// released validator's member set for `failure` is exactly
/// `{message, code, status?, providerRetryAfterMs?, requestId?}`. That count belongs to the
/// `image/offload` flow that reads it.
fn validate_failure(failure: &ProviderError) -> Result<(), PayloadError> {
    if failure.offload_images.is_some() {
        return Err(PayloadError::UnknownFailureKey {
            key: "offloadImages".to_owned(),
        });
    }
    failure
        .validate()
        .map_err(|violation| PayloadError::BadFailure { violation })
}

fn failure_to_value(failure: &ProviderError) -> Value {
    let mut object = Map::new();
    object.insert("message".to_owned(), Value::from(failure.message.as_str()));
    object.insert("code".to_owned(), Value::from(failure.code.as_str()));
    if let Some(status) = failure.status {
        object.insert("status".to_owned(), Value::from(status));
    }
    if let Some(retry_after) = failure.provider_retry_after_ms {
        object.insert("providerRetryAfterMs".to_owned(), Value::from(retry_after));
    }
    if let Some(request_id) = &failure.request_id {
        object.insert("requestId".to_owned(), Value::from(request_id.as_str()));
    }
    Value::Object(object)
}

fn failure_from_value(value: Option<&Value>, label: &str) -> Result<ProviderError, PayloadError> {
    let label = format!("{label} failure");
    let data = value.ok_or_else(|| PayloadError::MissingKey {
        label: label.clone(),
        key: "failure",
    })?;
    let object = exact(
        data,
        &label,
        &["message", "code"],
        &["status", "providerRetryAfterMs", "requestId"],
    )?;
    let status = match object.get("status") {
        Some(status) => Some(
            u16::try_from(count_of_value(status, "status", &label)?).map_err(|_| {
                PayloadError::NotCount {
                    label: label.clone(),
                    key: "status".to_owned(),
                }
            })?,
        ),
        None => None,
    };
    Ok(ProviderError {
        message: string_of(object, "message", &label)?,
        code: string_of(object, "code", &label)?,
        status,
        provider_retry_after_ms: match object.get("providerRetryAfterMs") {
            Some(value) => Some(count_of_value(value, "providerRetryAfterMs", &label)?),
            None => None,
        },
        request_id: match object.get("requestId") {
            Some(_) => Some(string_of(object, "requestId", &label)?),
            None => None,
        },
        offload_images: None,
    })
}

/// The exact member set of one payload.
///
/// Returns `&Map<String, Value>` borrowed from `data`, so every caller reads members through the
/// same checked object rather than re-deriving the shape.
fn exact<'a>(
    data: &'a Value,
    label: &str,
    required: &[&'static str],
    optional: &[&'static str],
) -> Result<&'a Map<String, Value>, PayloadError> {
    let Some(object) = data.as_object() else {
        return Err(PayloadError::NotAnObject {
            label: label.to_owned(),
        });
    };
    for key in required {
        if !object.contains_key(*key) {
            return Err(PayloadError::MissingKey {
                label: label.to_owned(),
                key,
            });
        }
    }
    for key in object.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(PayloadError::UnknownKey {
                label: label.to_owned(),
                key: key.clone(),
            });
        }
    }
    Ok(object)
}

fn string_of(
    object: &Map<String, Value>,
    key: &'static str,
    label: &str,
) -> Result<String, PayloadError> {
    let value = object.get(key).ok_or_else(|| PayloadError::MissingKey {
        label: label.to_owned(),
        key,
    })?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| PayloadError::NotAString {
            label: label.to_owned(),
            key,
        })
}

fn count_of(
    object: &Map<String, Value>,
    key: &'static str,
    label: &str,
) -> Result<u64, PayloadError> {
    let value = object.get(key).ok_or_else(|| PayloadError::MissingKey {
        label: label.to_owned(),
        key,
    })?;
    count_of_value(value, key, label)
}

/// Reads a non-negative safe integer, rejecting `-0`.
///
/// `arbitrary_precision` keeps `-0` as a token while `as_u64` reports `Some(0)`, so the check has to
/// read the token; the same reasoning applies to a `1e3` token, which is an integer to JavaScript but
/// not to a `u64` reader, and which the released validator accepts because `Number.isSafeInteger`
/// accepts it. Both are therefore normalised through `as_f64` when the token is not a plain integer.
fn count_of_value(value: &Value, key: &str, label: &str) -> Result<u64, PayloadError> {
    let bad = || PayloadError::NotCount {
        label: label.to_owned(),
        key: key.to_owned(),
    };
    if is_negative_zero(value) {
        return Err(bad());
    }
    if let Some(count) = value.as_u64() {
        return if count > MAX_SAFE_INTEGER {
            Err(bad())
        } else {
            Ok(count)
        };
    }
    let float = value.as_f64().ok_or_else(bad)?;
    if !float.is_finite() || float < 0.0 || float.fract() != 0.0 {
        return Err(bad());
    }
    if float > max_safe_integer() {
        return Err(bad());
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is finite, integral, non-negative, and at most 2^53 by the checks above"
    )]
    Ok(float as u64)
}

fn delay_of(
    object: &Map<String, Value>,
    key: &'static str,
    label: &str,
) -> Result<f64, PayloadError> {
    let value = object.get(key).ok_or_else(|| PayloadError::MissingKey {
        label: label.to_owned(),
        key,
    })?;
    if is_negative_zero(value) {
        return Err(PayloadError::BadDelay {
            found: "-0".to_owned(),
        });
    }
    value.as_f64().ok_or_else(|| PayloadError::BadDelay {
        found: value.to_string(),
    })
}

/// [`MAX_SAFE_INTEGER`] as the `f64` a comparison needs.
///
/// `2^53 - 1` is not representable in `f64` — the nearest value is `2^53` — so the conversion rounds
/// *up*, which is the safe direction here: it lets `2^53` itself through the comparison, and `2^53` is
/// still an integer JavaScript represents exactly.
#[allow(
    clippy::cast_precision_loss,
    reason = "the value is 2^53-1, whose nearest f64 is 2^53; both are exactly representable integers and the comparison is against that bound, not against a rounded quantity"
)]
fn max_safe_integer() -> f64 {
    MAX_SAFE_INTEGER as f64
}

/// Whether the value is the JSON token `-0`.
fn is_negative_zero(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.to_string() == "-0",
        _ => false,
    }
}

/// A JSON number spelled the way `JSON.stringify` spells it.
///
/// Parsed back through `serde_json` rather than built from an `f64`, because
/// `arbitrary_precision` preserves the token text: `1e+21` stays `1e+21` instead of becoming
/// `1000000000000000000000`.
fn json_number(value: f64) -> Value {
    let text = js_number(value);
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// A JSON string literal for `text`.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
}
