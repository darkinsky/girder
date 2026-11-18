//! `DeepSeek` options: the validation ladder and defaults of `config.ts`.
//!
//! Upstream paths: `packages/llm/llm-deepseek/src/config.ts` (340 lines),
//! `listing-config.ts` (74), `llm-deepseek-account/src/config.ts` (97), and
//! `llm-deepseek-api-key/src/config.ts` (61).
//!
//! # What existed here before this module
//!
//! [`girder_core::deepseek_serialize_gap`] serializes a Messages request and owns
//! [`ReasoningEffort`]; [`girder_core::deepseek_files_gap`] owns the Files API decisions and the
//! *constants* those decisions compare against, plus [`FilePolicy`] for the expiry triple.
//! [`girder_core::llm_deepseek`] holds a flat `DeepSeekConfig` whose only defaults are
//! `max_tokens: 8192` and model `deepseek-chat`.
//!
//! What none of them had is the *resolve step*: the one place that re-judges every default and every
//! cross-field bound after a settings update that bypassed schema validation. Grepping the crate for
//! `imageOffloadByteQuantum`, `maxRequestFilesBytes`, or `adapter_options` returned nothing — the
//! fields below existed as loose constants, or not at all.
//!
//! # The 18 fields of `config.ts` `Config`
//!
//! Defaults and bounds are transcribed from `deepSeekConfigFields` and `resolveAdapterOptions`;
//! "bound" is the resolver's check, which is the one that survives a programmatic construction.
//!
//! | # | field | type | default | bound |
//! |---|---|---|---|---|
//! | 1 | `baseURL` | string | `https://api.deepseek.com/anthropic`, or `$DEEPSEEK_BASE_URL` | http(s) root, no credentials/query/fragment |
//! | 2 | `thinking` | `enabled │ disabled` | unset | `disabled` forces effort `off` or unset |
//! | 3 | `reasoningEffort` | `off │ low │ high │ max` | unset (adapter reads `high`) | see #2 |
//! | 4 | `maxTokens` | integer | `256_000` | positive safe integer |
//! | 5 | `defaultContextWindow` | integer | `1_000_000` | positive integer |
//! | 6 | `models` | catalog | `DEFAULT_MODELS` | per-entry rules below; ids unique |
//! | 7 | `streamIdleTimeoutMs` | number | `300_000` | finite, `0 < x ≤ MAX_TIMER_DELAY_MS` |
//! | 8 | `maxRequestFilesBytes` | integer | `128 MiB` | positive safe integer |
//! | 9 | `maxInlineRequestImageBytes` | integer | `20 MiB` | positive safe integer |
//! | 10 | `maxImagesPerRequest` | integer | `600` | positive safe integer |
//! | 11 | `imageOffloadByteQuantum` | integer | `64 MiB` | positive safe integer, `≤ #8` |
//! | 12 | `inlineImageOffloadByteQuantum` | integer | `10 MiB` | positive safe integer, `≤ #9` |
//! | 13 | `imageOffloadCountQuantum` | integer | `20` | positive safe integer, `≤ #10` |
//! | 14 | `filesApiTimeoutMs` | number | `60_000` | finite, `0 < x ≤ MAX_TIMER_DELAY_MS` |
//! | 15 | `fileExpiresAfterSeconds` | integer | `604_800` | integer in `3600 ..= 2_592_000` |
//! | 16 | `fileRefreshMarginSeconds` | integer | `3_600` | integer, `0 ≤ x < #15` |
//! | 17 | `fileQuotaCleanupBatch` | integer | `100` | integer in `1 ..= 1_000` |
//! | 18 | `retryPolicy` | policy | normal, 5 retries, 5 codes, 500 ms, 10 s, 0.1 jitter | see [`resolve_retry_policy`] |
//!
//! Four more members arrive from the route packages:
//!
//! | # | field | route | default | bound |
//! |---|---|---|---|---|
//! | 19 | `modelsTimeoutMs` | account, api-key | `5_000` | finite, `0 < x ≤ MAX_TIMER_DELAY_MS` |
//! | 20 | `modelsMaxAgeMs` | account, api-key | `600_000` | non-negative safe integer |
//! | 21 | `modelOverrides` | account | `{}` | non-empty id; no `id` member; catalog rules per stand-in entry |
//! | 22 | `apiKeyEnv` | api-key | `DEEPSEEK_API_KEY` | `/^[A-Za-z_][A-Za-z0-9_]*$/` |
//!
//! # Cross-field constraints, which no field bound expresses
//!
//! 1. `thinking == disabled` ⇒ `reasoningEffort` unset or `off`.
//! 2. `imageOffloadByteQuantum ≤ maxRequestFilesBytes` — a removal step larger than the bound it
//!    repairs would make one offload a no-op.
//! 3. `inlineImageOffloadByteQuantum ≤ maxInlineRequestImageBytes` — the same rule for the base64
//!    fallback, whose bound is much smaller.
//! 4. `imageOffloadCountQuantum ≤ maxImagesPerRequest` — the same rule for the image count.
//! 5. `fileRefreshMarginSeconds < fileExpiresAfterSeconds` — a margin at or beyond the lifetime
//!    marks every fresh upload stale on arrival.
//! 6. A catalog model may declare `imagePixelBudget`/`imageMaxBytes` only with `image` input.
//! 7. A catalog `systemPromptUpdate` may only be `in-history`; `toolUpdate` only `in-history` or
//!    `addition-only`. Upstream carries `leading-only` as a *runtime* mode and refuses it in
//!    configuration.
//! 8. `retryPolicy.backoff.initialDelayMs ≤ maxDelayMs`.
//! 9. `thinking` and `reasoningEffort` are one policy, not two settings.
//! 10. The account route refuses a retired `models` setting; the API-key route follows the official
//!     `/models` listing only for the built-in catalog at the official origin.
//!
//! # Deliberate deviations
//!
//! - **`CatalogModel` carries no `description`.** That type belongs to
//!   [`girder_core::deepseek_files_gap`], which does not model the field; [`CatalogSpec`] parses and
//!   validates a `description` so a settings file is judged exactly as upstream judges it, and the
//!   resolved [`CatalogModel`] drops it. The consequence: two catalogs that differ only in a
//!   description resolve equal. Nothing downstream selects on a description.
//! - **Typed enums where upstream validates strings.** `Modality`, `PixelBudget`,
//!   `SystemPromptMode`, and `ToolUpdateMode` come from the Files module. A JSON payload carrying
//!   `inputModalities: ['audio']` is refused while parsing, with upstream's message; a *typed*
//!   construction cannot express it. The same holds for `imageDetail` on a model and `id` inside an
//!   override: both are detected on the JSON path, where a settings file can carry them, and are
//!   unrepresentable on the typed path — stated at the functions that check them.
//! - **Unknown members are refused where the schema would have.** [`Options::from_json`] and the
//!   catalog/override/retry parsers reject a member they do not know with
//!   `{owner}: unknown key "…"`, mirroring `validateKeys` in upstream's `retry-policy.ts` rather
//!   than inventing a rule the reference lacks. A programmatic caller cannot pass one.
//! - **`modelsMaxAgeMs` stays `f64` until resolution, and `modelsTimeoutMs` always does.** Upstream
//!   accepts the number and refuses `-1` and `1.5` at resolve time with one message; a `u64` field
//!   would have to refuse them at parse time and could not hold the negative case at all.
//! - **No `URL` library.** [`origin_of`] and [`model_listing_url`] use the small parser below:
//!   scheme case, credentials, query, fragment, host case, and default ports are reproduced; IDN
//!   host folding and dot-segment path normalization are not, and neither affects an origin. A base
//!   URL that fails to parse is reported with upstream's one base-URL message rather than the
//!   `TypeError` the WHATWG parser raises.
//! - **`isBuiltInCatalog`'s `deepEqualJson` is structural equality here.** Both sides are
//!   normalized entries, so field order and absent-versus-`None` cannot differ.
//! - **`reasoningEffort` has no resolve-time default.** Upstream passes the value through and the
//!   adapter reads `high`; storing `High` in a snapshot would claim an effort nobody configured.
//!   [`DEFAULT_REASONING_EFFORT`] names the adapter-side value and
//!   [`RequestDefaults::effort_or_default`] applies it where a request is assembled.
#![allow(
    clippy::doc_markdown,
    reason = "provider, setting, and endpoint names are the subject of nearly every paragraph"
)]
#![allow(
    clippy::too_many_lines,
    reason = "the resolve ladder is one ordered sequence; splitting it would hide the order that \
              decides which of two simultaneous mistakes a caller is told about"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::{Map, Value};

use girder_core::llm_extra_gap::{DEFAULT_RETRYABLE, MAX_TIMER_DELAY_MS};

// Re-exported because they appear in this module's public signatures: a caller that resolves a
// catalog or an override should not have to name two modules to describe one value.
pub use girder_core::deepseek_files_gap::{CatalogModel, FilePolicy, Modality, ModelOverride};
pub use girder_core::deepseek_files_gap::{PixelBudget, SystemPromptMode, ToolUpdateMode};
pub use girder_core::deepseek_serialize_gap::ReasoningEffort;

// The defaults this module resolves *to* are the ones the Files module compares *against*. Naming
// them from one place is what makes a drift a compile error here rather than two opinions of one
// bound in production.
pub use girder_core::deepseek_files_gap::{
    DEFAULT_FILE_EXPIRY_SECONDS, DEFAULT_FILE_QUOTA_CLEANUP_BATCH,
    DEFAULT_FILE_REFRESH_MARGIN_SECONDS, DEFAULT_FILES_API_TIMEOUT_MS,
    DEFAULT_IMAGE_OFFLOAD_BYTE_QUANTUM, DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM,
    DEFAULT_INLINE_IMAGE_OFFLOAD_BYTE_QUANTUM, DEFAULT_MAX_IMAGES_PER_REQUEST,
    DEFAULT_MAX_INLINE_REQUEST_IMAGE_BYTES, DEFAULT_MAX_REQUEST_FILES_BYTES,
    DEFAULT_REQUEST_IMAGE_MAX_BYTES, MAX_FILE_EXPIRY_SECONDS, MIN_FILE_EXPIRY_SECONDS,
};

// =============================================================================================
// Published defaults and limits
// =============================================================================================

/// The public Messages endpoint — upstream `PUBLIC_BASE_URL`.
pub const PUBLIC_BASE_URL: &str = "https://api.deepseek.com/anthropic";

/// The environment variable naming this provider's endpoint, honored only from trusted layers.
pub const BASE_URL_ENV: &str = "DEEPSEEK_BASE_URL";

/// Default per-request output cap — upstream `DEFAULT_MAX_TOKENS`.
pub const DEFAULT_MAX_TOKENS: u64 = 256_000;

/// Default context capacity used when a model declares no exact value.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;

/// Default idle bound for one outstanding stream read, in milliseconds.
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: f64 = 300_000.0;

/// The effort the adapter reads when configuration names none.
///
/// Documentation rather than resolution: [`resolve_options`] passes `reasoningEffort` through
/// untouched, exactly as upstream's resolver does.
pub const DEFAULT_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::High;

/// `Number.MAX_SAFE_INTEGER`: the largest integer a JavaScript number holds exactly.
///
/// Every `!Number.isSafeInteger` in the ladder is a comparison against this, and a Rust `u64` can
/// represent values it refuses, so the bound has to be restated rather than implied by the type.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// [`MAX_SAFE_INTEGER`] as the `f64` the JSON path compares against before narrowing.
const MAX_SAFE_INTEGER_F64: f64 = 9_007_199_254_740_991.0;

/// [`MAX_TIMER_DELAY_MS`] as the `f64` a timeout is compared in.
const MAX_TIMER_DELAY_MS_F64: f64 = 2_147_483_647.0;

/// [`DEFAULT_FILES_API_TIMEOUT_MS`] as the `f64` a timeout is compared in.
const DEFAULT_FILES_API_TIMEOUT_MS_F64: f64 = 60_000.0;

/// Default listing deadline — upstream `DEFAULT_MODELS_TIMEOUT_MS`.
pub const DEFAULT_MODELS_TIMEOUT_MS: f64 = 5_000.0;

/// Default listing refresh interval — upstream `DEFAULT_MODELS_MAX_AGE_MS`.
pub const DEFAULT_MODELS_MAX_AGE_MS: u64 = 600_000;

/// Default credential reference of the API-key route.
pub const DEFAULT_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// Default maximum eligible retries — upstream `DEFAULT_MAX_RETRIES`.
pub const DEFAULT_MAX_RETRIES: u64 = 5;

/// Default delay before the first retry, in milliseconds.
pub const DEFAULT_INITIAL_DELAY_MS: f64 = 500.0;

/// Default ceiling for any single delay, in milliseconds.
pub const DEFAULT_MAX_DELAY_MS: f64 = 10_000.0;

/// Default symmetric jitter ratio around one.
pub const DEFAULT_JITTER_RATIO: f64 = 0.1;

/// The catalog every unconfigured route advertises — upstream `DEFAULT_MODELS`, normalized.
///
/// Normalized the way [`resolve_models`] normalizes a supplied catalog: the vision entry carries
/// the per-image byte target its omission would resolve to, and the text-only entry carries no image
/// limits. That is what lets [`is_built_in_catalog`] be a comparison rather than a special case.
#[must_use]
pub fn built_in_models() -> Vec<CatalogModel> {
    vec![
        CatalogModel {
            id: "deepseek-flash".to_owned(),
            name: Some("DeepSeek-V4.1-Flash".to_owned()),
            input_modalities: vec![Modality::Text, Modality::Image],
            context_window: Some(1_048_576),
            max_output_tokens: None,
            system_prompt_update: Some(SystemPromptMode::InHistory),
            tool_update: Some(ToolUpdateMode::AdditionOnly),
            image_pixel_budget: None,
            image_max_bytes: Some(DEFAULT_REQUEST_IMAGE_MAX_BYTES),
            vision: true,
        },
        CatalogModel {
            id: "deepseek-v4-pro".to_owned(),
            name: Some("DeepSeek-V4-Pro".to_owned()),
            input_modalities: vec![Modality::Text],
            context_window: Some(1_048_576),
            max_output_tokens: None,
            system_prompt_update: None,
            tool_update: None,
            image_pixel_budget: None,
            image_max_bytes: None,
            vision: false,
        },
    ]
}

// =============================================================================================
// Errors
// =============================================================================================

/// One refusal from a resolve step, carrying upstream's message verbatim.
///
/// The message is the contract: upstream's tests assert on it (`/imageOffloadByteQuantum must not
/// exceed maxRequestFilesBytes/`), and a caller that matched only a variant would still have to
/// format the same sentence for a user. [`OptionsError::message`] is therefore the primary accessor
/// and [`Display`](fmt::Display) prints it unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionsError {
    message: String,
}

impl OptionsError {
    /// Builds a refusal from its complete message.
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The refusal message, upstream-shaped and already owner-prefixed.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for OptionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OptionsError {}

/// Builds the unknown-member refusal upstream's `validateKeys` emits.
fn unknown_key(owner: &str, key: &str) -> OptionsError {
    OptionsError::new(format!("{owner}: unknown key \"{key}\""))
}

// =============================================================================================
// Shared vocabulary
// =============================================================================================

/// The deployment thinking policy — upstream `thinking`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thinking {
    /// Thinking is offered; a request may still ask for `off`.
    Enabled,
    /// Every conversation request is limited to `off`.
    Disabled,
}

impl Thinking {
    /// The wire spelling, which a settings form writes back.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    /// Parses the wire spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// Request defaults applied to every call — upstream `RequestDefaults`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestDefaults {
    /// Deployment thinking policy, when one is configured.
    pub thinking: Option<Thinking>,
    /// Default effort, when one is configured.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl RequestDefaults {
    /// The effort a request assembly should use, applying the adapter-side default.
    ///
    /// The one place [`DEFAULT_REASONING_EFFORT`] is applied — never at resolve time, because a
    /// snapshot that stored `High` for an unconfigured route would claim a setting nobody chose.
    #[must_use]
    pub fn effort_or_default(self) -> ReasoningEffort {
        self.reasoning_effort.unwrap_or(DEFAULT_REASONING_EFFORT)
    }

    /// Whether a request may carry evidence of thinking at all.
    #[must_use]
    pub fn thinking_enabled(self) -> bool {
        self.thinking != Some(Thinking::Disabled)
    }
}

// =============================================================================================
// Retry policy — upstream `retry-policy.ts`
// =============================================================================================

/// Bounded exponential backoff with symmetric jitter — upstream `BackoffConfig`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BackoffConfig {
    /// Initial local delay, in milliseconds.
    pub initial_delay_ms: Option<f64>,
    /// Ceiling for any single delay, in milliseconds.
    pub max_delay_ms: Option<f64>,
    /// Symmetric jitter ratio around one.
    pub jitter_ratio: Option<f64>,
}

/// A provider-owned retry policy — upstream `RetryPolicyConfig`.
///
/// `always` carries neither `maxRetries` nor `retryableCodes`: a settings file's `always` object may
/// still carry them (upstream's `ALWAYS_POLICY_KEYS` equals the normal set, so a mode switch does
/// not strand the old values) and they are ignored. A typed value that never carried them cannot
/// wrongly apply them, and [`RetryPolicyConfig::from_json`] drops them with the same result.
#[derive(Clone, Debug, PartialEq)]
pub enum RetryPolicyConfig {
    /// Retry only configured transient failure codes.
    Normal {
        /// Maximum eligible retries after the first request.
        max_retries: Option<u64>,
        /// Accepted failure codes.
        retryable_codes: Option<Vec<String>>,
        /// Backoff configuration.
        backoff: Option<BackoffConfig>,
    },
    /// Retry every model-request failure until success, cancellation, or disposal.
    Always {
        /// Backoff configuration.
        backoff: Option<BackoffConfig>,
    },
}

/// The two retry modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryMode {
    /// Bounded, code-filtered retries.
    Normal,
    /// Unbounded retries.
    Always,
}

impl RetryMode {
    /// The wire spelling, which `retryPolicyKey` serializes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Always => "always",
        }
    }
}

/// A fully resolved policy — upstream `ResolvedRetryPolicy`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedRetryPolicy {
    /// Which arm this policy is.
    pub mode: RetryMode,
    /// Maximum retries, present only in [`RetryMode::Normal`].
    pub max_retries: Option<u64>,
    /// Eligible codes; empty and ignored in [`RetryMode::Always`].
    pub retryable_codes: Vec<String>,
    /// Delay before the first retry, in milliseconds.
    pub initial_delay_ms: f64,
    /// Ceiling for any single delay, in milliseconds.
    pub max_delay_ms: f64,
    /// Symmetric jitter ratio around one.
    pub jitter_ratio: f64,
}

impl Default for ResolvedRetryPolicy {
    fn default() -> Self {
        Self {
            mode: RetryMode::Normal,
            max_retries: Some(DEFAULT_MAX_RETRIES),
            retryable_codes: DEFAULT_RETRYABLE
                .iter()
                .map(|code| (*code).to_owned())
                .collect(),
            initial_delay_ms: DEFAULT_INITIAL_DELAY_MS,
            max_delay_ms: DEFAULT_MAX_DELAY_MS,
            jitter_ratio: DEFAULT_JITTER_RATIO,
        }
    }
}

/// The members upstream's `validateKeys` accepts on a policy.
const RETRY_POLICY_KEYS: [&str; 4] = ["mode", "maxRetries", "retryableCodes", "backoff"];

/// The members of one backoff object.
const BACKOFF_KEYS: [&str; 3] = ["initialDelayMs", "maxDelayMs", "jitterRatio"];

impl RetryPolicyConfig {
    /// Reads one policy from a settings object.
    ///
    /// `maxRetries` and `retryableCodes` are accepted in either mode and dropped from `always`,
    /// which is what upstream's shared key set means; an unknown member is refused with the shape
    /// `validateKeys` uses.
    ///
    /// # Errors
    ///
    /// Returns [`OptionsError`] naming the first unusable member, prefixed with `path`.
    pub fn from_json(config: &Map<String, Value>, path: &str) -> Result<Self, OptionsError> {
        for key in config.keys() {
            if !RETRY_POLICY_KEYS.contains(&key.as_str()) {
                return Err(unknown_key(path, key));
            }
        }
        let Some(mode) = config.get("mode").and_then(Value::as_str) else {
            return Err(OptionsError::new(format!(
                "{path}.mode must be \"normal\" or \"always\""
            )));
        };
        let backoff = match config.get("backoff") {
            None | Some(Value::Null) => None,
            Some(value) => Some(parse_backoff(value, &format!("{path}.backoff"))?),
        };
        match mode {
            "normal" => {
                let max_retries = match config.get("maxRetries") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(integer_member(
                        value,
                        &format!("{path}.maxRetries must be a non-negative safe integer"),
                    )?),
                };
                let retryable_codes = match config.get("retryableCodes") {
                    None | Some(Value::Null) => None,
                    Some(Value::Array(rows)) => {
                        let mut codes = Vec::with_capacity(rows.len());
                        for row in rows {
                            let Some(text) = row.as_str() else {
                                return Err(OptionsError::new(format!(
                                    "{path}.retryableCodes must contain only non-empty strings"
                                )));
                            };
                            codes.push(text.to_owned());
                        }
                        Some(codes)
                    }
                    Some(_) => {
                        return Err(OptionsError::new(format!(
                            "{path}.retryableCodes must contain only non-empty strings"
                        )));
                    }
                };
                Ok(Self::Normal {
                    max_retries,
                    retryable_codes,
                    backoff,
                })
            }
            "always" => Ok(Self::Always { backoff }),
            _ => Err(OptionsError::new(format!(
                "{path}.mode must be \"normal\" or \"always\""
            ))),
        }
    }
}

/// Reads one backoff object.
fn parse_backoff(value: &Value, path: &str) -> Result<BackoffConfig, OptionsError> {
    let Value::Object(members) = value else {
        return Err(OptionsError::new(format!(
            "{path}.initialDelayMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )));
    };
    for key in members.keys() {
        if !BACKOFF_KEYS.contains(&key.as_str()) {
            return Err(unknown_key(path, key));
        }
    }
    let number = |key: &str, message: &str| -> Result<Option<f64>, OptionsError> {
        match members.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_f64()
                .map(Some)
                .ok_or_else(|| OptionsError::new(message.to_owned())),
        }
    };
    Ok(BackoffConfig {
        initial_delay_ms: number(
            "initialDelayMs",
            &format!(
                "{path}.initialDelayMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
            ),
        )?,
        max_delay_ms: number(
            "maxDelayMs",
            &format!(
                "{path}.maxDelayMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
            ),
        )?,
        jitter_ratio: number(
            "jitterRatio",
            &format!("{path}.jitterRatio must be between 0 and 1"),
        )?,
    })
}

/// Validates, defaults, and detaches one provider-owned retry policy — upstream
/// `resolveRetryPolicy`.
///
/// # Errors
///
/// Returns [`OptionsError`] for a policy outside its bounds, prefixed with `path`.
pub fn resolve_retry_policy(
    config: Option<&RetryPolicyConfig>,
    path: &str,
) -> Result<ResolvedRetryPolicy, OptionsError> {
    let Some(config) = config else {
        return Ok(ResolvedRetryPolicy::default());
    };
    match config {
        RetryPolicyConfig::Normal {
            max_retries,
            retryable_codes,
            backoff,
        } => {
            let max_retries = max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
            if max_retries > MAX_SAFE_INTEGER {
                return Err(OptionsError::new(format!(
                    "{path}.maxRetries must be a non-negative safe integer"
                )));
            }
            let codes: Vec<String> = retryable_codes.as_ref().map_or_else(
                || {
                    DEFAULT_RETRYABLE
                        .iter()
                        .map(|code| (*code).to_owned())
                        .collect()
                },
                Clone::clone,
            );
            if codes.is_empty() {
                return Err(OptionsError::new(format!(
                    "{path}.retryableCodes must not be empty"
                )));
            }
            if codes.iter().any(String::is_empty) {
                return Err(OptionsError::new(format!(
                    "{path}.retryableCodes must contain only non-empty strings"
                )));
            }
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            for code in &codes {
                if !seen.insert(code.as_str()) {
                    return Err(OptionsError::new(format!(
                        "{path}.retryableCodes must not contain duplicates"
                    )));
                }
            }
            let (initial_delay_ms, max_delay_ms, jitter_ratio) =
                resolve_backoff(*backoff, &format!("{path}.backoff"))?;
            Ok(ResolvedRetryPolicy {
                mode: RetryMode::Normal,
                max_retries: Some(max_retries),
                retryable_codes: codes,
                initial_delay_ms,
                max_delay_ms,
                jitter_ratio,
            })
        }
        RetryPolicyConfig::Always { backoff } => {
            let (initial_delay_ms, max_delay_ms, jitter_ratio) =
                resolve_backoff(*backoff, &format!("{path}.backoff"))?;
            Ok(ResolvedRetryPolicy {
                mode: RetryMode::Always,
                max_retries: None,
                retryable_codes: Vec::new(),
                initial_delay_ms,
                max_delay_ms,
                jitter_ratio,
            })
        }
    }
}

/// Validates and defaults one backoff triple.
fn resolve_backoff(
    config: Option<BackoffConfig>,
    path: &str,
) -> Result<(f64, f64, f64), OptionsError> {
    let config = config.unwrap_or_default();
    let initial_delay_ms = config.initial_delay_ms.unwrap_or(DEFAULT_INITIAL_DELAY_MS);
    let max_delay_ms = config.max_delay_ms.unwrap_or(DEFAULT_MAX_DELAY_MS);
    let jitter_ratio = config.jitter_ratio.unwrap_or(DEFAULT_JITTER_RATIO);
    if !initial_delay_ms.is_finite()
        || initial_delay_ms <= 0.0
        || initial_delay_ms > MAX_TIMER_DELAY_MS_F64
    {
        return Err(OptionsError::new(format!(
            "{path}.initialDelayMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )));
    }
    if !max_delay_ms.is_finite() || max_delay_ms <= 0.0 || max_delay_ms > MAX_TIMER_DELAY_MS_F64 {
        return Err(OptionsError::new(format!(
            "{path}.maxDelayMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )));
    }
    if initial_delay_ms > max_delay_ms {
        return Err(OptionsError::new(format!(
            "{path}.initialDelayMs must be less than or equal to maxDelayMs"
        )));
    }
    if !jitter_ratio.is_finite() || !(0.0..=1.0).contains(&jitter_ratio) {
        return Err(OptionsError::new(format!(
            "{path}.jitterRatio must be between 0 and 1"
        )));
    }
    Ok((initial_delay_ms, max_delay_ms, jitter_ratio))
}

// =============================================================================================
// Catalog resolution — upstream `resolveModels`
// =============================================================================================

/// The members a catalog entry and a per-model override share — upstream `catalogModelFields`.
///
/// `None` means "declared absent", which is *not* the same as a defaulted value: [`resolve_models`]
/// injects `image_max_bytes` for a vision entry, while an override keeps only what its author wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogSpec {
    /// Wire model id.
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Selector detail — parsed and validated, then dropped: [`CatalogModel`] has no such field.
    pub description: Option<String>,
    /// Combined request/response context.
    pub context_window: Option<u64>,
    /// Per-request output cap.
    pub max_output_tokens: Option<u64>,
    /// Accepted modalities; `None` means the entry declared none.
    pub input_modalities: Option<Vec<Modality>>,
    /// Pixel budget, either an explicit cap or the low-detail preset.
    pub image_pixel_budget: Option<PixelBudget>,
    /// Encoded-byte target.
    pub image_max_bytes: Option<u64>,
    /// Messages system-prompt mode.
    pub system_prompt_update: Option<SystemPromptMode>,
    /// Messages tool-update mode.
    pub tool_update: Option<ToolUpdateMode>,
    /// Whether the object carried the retired `imageDetail` member.
    ///
    /// A settings file written against an older release can still carry it and a typed
    /// [`CatalogModel`] cannot, so this flag is the only way the JSON path can reproduce upstream's
    /// refusal instead of ignoring a member the author meant.
    pub legacy_image_detail: bool,
}

impl CatalogSpec {
    /// A spec declaring just an id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }
}

/// Resolves, validates, and detaches the advisory catalog — upstream `resolveModels`.
///
/// `None` resolves to [`built_in_models`]. Every rule upstream applies to an entry is applied here,
/// including the normalization that makes a vision entry carry its byte target and a text-only
/// entry carry no image limits at all.
///
/// # Errors
///
/// Returns [`OptionsError`] naming the first unusable entry or duplicate id.
pub fn resolve_models(models: Option<&[CatalogModel]>) -> Result<Vec<CatalogModel>, OptionsError> {
    let Some(entries) = models else {
        return Ok(built_in_models());
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let spec = CatalogSpec {
            id: entry.id.clone(),
            name: entry.name.clone(),
            description: None,
            context_window: entry.context_window,
            max_output_tokens: entry.max_output_tokens,
            input_modalities: Some(entry.input_modalities.clone()),
            image_pixel_budget: entry.image_pixel_budget,
            image_max_bytes: entry.image_max_bytes,
            system_prompt_update: entry.system_prompt_update,
            tool_update: entry.tool_update,
            legacy_image_detail: false,
        };
        let model = resolve_catalog_spec(&spec, false)?;
        if !seen.insert(model.id.clone()) {
            return Err(OptionsError::new(format!(
                "llm-deepseek: duplicate catalog model \"{}\"",
                model.id
            )));
        }
        resolved.push(model);
    }
    Ok(resolved)
}

/// Validates one catalog entry and returns its normalized form.
///
/// `stand_in_vision` is the account route's rule: an override is validated against an
/// image-capable stand-in entry, because the entry it will layer over is only known once a listing
/// loads.
///
/// # Errors
///
/// Returns [`OptionsError`] for a rule the entry breaks.
pub fn resolve_catalog_spec(
    spec: &CatalogSpec,
    stand_in_vision: bool,
) -> Result<CatalogModel, OptionsError> {
    if spec.legacy_image_detail {
        return Err(OptionsError::new(
            "llm-deepseek: catalog model imageDetail is no longer supported; use imagePixelBudget",
        ));
    }
    if spec.id.is_empty() {
        return Err(OptionsError::new(
            "llm-deepseek: catalog model ids must be non-empty",
        ));
    }
    let label = format!("llm-deepseek: catalog model \"{}\"", spec.id);
    if spec.name.as_ref().is_some_and(String::is_empty) {
        return Err(OptionsError::new(format!("{label} has an empty name")));
    }
    if spec
        .context_window
        .is_some_and(|window| window == 0 || window > MAX_SAFE_INTEGER)
    {
        return Err(OptionsError::new(format!(
            "{label} contextWindow must be a positive integer"
        )));
    }
    if spec
        .max_output_tokens
        .is_some_and(|max_tokens| max_tokens == 0 || max_tokens > MAX_SAFE_INTEGER)
    {
        return Err(OptionsError::new(format!(
            "{label} maxTokens must be a positive integer"
        )));
    }
    let modalities = spec.input_modalities.clone().unwrap_or_else(|| {
        if stand_in_vision {
            vec![Modality::Text, Modality::Image]
        } else {
            vec![Modality::Text]
        }
    });
    if modalities.is_empty() {
        return Err(OptionsError::new(format!(
            "{label} inputModalities must not be empty"
        )));
    }
    let mut distinct: BTreeSet<Modality> = BTreeSet::new();
    for modality in &modalities {
        if !distinct.insert(*modality) {
            return Err(OptionsError::new(format!(
                "{label} inputModalities must not contain duplicates"
            )));
        }
    }
    let has_image = modalities.contains(&Modality::Image);
    if !has_image && (spec.image_pixel_budget.is_some() || spec.image_max_bytes.is_some()) {
        return Err(OptionsError::new(format!(
            "llm-deepseek: text-only catalog model \"{}\" cannot declare image request limits",
            spec.id
        )));
    }
    if spec
        .image_pixel_budget
        .is_some_and(|budget| matches!(budget, PixelBudget::Pixels(pixels) if pixels == 0 || pixels > MAX_SAFE_INTEGER))
    {
        return Err(OptionsError::new(format!(
            "{label} imagePixelBudget must be \"low\" or a positive safe integer"
        )));
    }
    if spec
        .image_max_bytes
        .is_some_and(|bytes| bytes == 0 || bytes > MAX_SAFE_INTEGER)
    {
        return Err(OptionsError::new(format!(
            "{label} imageMaxBytes must be a positive safe integer"
        )));
    }
    if spec.system_prompt_update == Some(SystemPromptMode::LeadingOnly) {
        return Err(OptionsError::new(format!(
            "{label} systemPromptUpdate must be \"in-history\" when present"
        )));
    }
    if spec.tool_update == Some(ToolUpdateMode::LeadingOnly) {
        return Err(OptionsError::new(format!(
            "{label} toolUpdate must be \"in-history\" or \"addition-only\" when present"
        )));
    }
    Ok(CatalogModel {
        id: spec.id.clone(),
        name: spec.name.clone(),
        input_modalities: modalities,
        context_window: spec.context_window,
        max_output_tokens: spec.max_output_tokens,
        system_prompt_update: spec.system_prompt_update,
        tool_update: spec.tool_update,
        image_pixel_budget: if has_image {
            spec.image_pixel_budget
        } else {
            None
        },
        image_max_bytes: if has_image {
            Some(
                spec.image_max_bytes
                    .unwrap_or(DEFAULT_REQUEST_IMAGE_MAX_BYTES),
            )
        } else {
            None
        },
        vision: has_image,
    })
}

/// Whether a resolved catalog is the built-in one — upstream `isBuiltInCatalog`.
///
/// The comparison is against [`built_in_models`] rather than against the constant, so a catalog that
/// merely restates the defaults by hand still counts as built-in — what upstream's "resolve the
/// default catalog and deep-compare" does, and what lets the API-key route follow the official
/// listing for a deployment that cloned the catalog into its settings.
#[must_use]
pub fn is_built_in_catalog(models: &[CatalogModel]) -> bool {
    let built_in = built_in_models();
    models == built_in.as_slice()
}

// =============================================================================================
// Base URL and origin — upstream `new URL(baseURL)` and `modelListingURL`
// =============================================================================================

/// The two schemes a Messages root may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    /// `http:`
    Http,
    /// `https:`
    Https,
}

impl Scheme {
    /// Parses a scheme, case-insensitively.
    fn parse(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("http") {
            Some(Self::Http)
        } else if text.eq_ignore_ascii_case("https") {
            Some(Self::Https)
        } else {
            None
        }
    }

    /// The scheme spelling a serialized origin uses.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    /// The port the scheme assumes when none is written.
    const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

/// One parsed origin: scheme, host, and a non-default port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    /// `http` or `https`.
    scheme: Scheme,
    /// Lowercased host.
    host: String,
    /// Explicit port, or `None` when the scheme's default applies.
    port: Option<u16>,
}

impl Origin {
    /// The serialized origin, as `URL.origin` spells it: default ports are dropped.
    ///
    /// URL parsing lowercases the host and drops a port equal to the scheme's default, so
    /// `https://API.example.com:443/x` and `https://api.example.com/x` share one origin.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self.port {
            Some(port) => format!("{}://{}:{port}", self.scheme.as_str(), self.host),
            None => format!("{}://{}", self.scheme.as_str(), self.host),
        }
    }
}

/// The scheme/authority split of a base URL, plus whatever follows the authority.
///
/// Returns `None` for anything the ladder refuses: a non-HTTP scheme, an authority with credentials,
/// or a query/fragment anywhere.
fn parse_origin(url: &str) -> Option<(Origin, &str)> {
    let (scheme_text, rest) = url.split_once("://")?;
    let scheme = Scheme::parse(scheme_text)?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let after = &rest[authority_end..];
    // Credentials, query, and fragment are refusals, not things to strip. `@` is checked over the
    // whole authority because user information precedes the host and its port.
    if authority.is_empty() || rest.contains('@') || rest.contains('?') || rest.contains('#') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port_text)) => {
            if port_text.is_empty() || !port_text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let port = u16::try_from(port_text.parse::<u32>().ok()?).ok()?;
            (host, Some(port))
        }
        None => (authority, None),
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) if port == scheme.default_port() => None,
        other => other,
    };
    Some((
        Origin {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        },
        after,
    ))
}

/// Refuses anything but an HTTP(S) root — the `baseURL` rule of the ladder.
///
/// # Errors
///
/// Returns [`OptionsError`] with upstream's one base-URL message.
pub fn validate_base_url(base_url: &str) -> Result<(), OptionsError> {
    parse_origin(base_url).map_or_else(
        || {
            Err(OptionsError::new(
                "llm-deepseek: Messages baseURL must be an HTTP(S) root without credentials, query, or fragment",
            ))
        },
        |_| Ok(()),
    )
}

/// The `/models` listing endpoint of a Messages root — upstream `modelListingURL`.
///
/// `/models` sits at the origin of the base URL, not under its path: the route's credential is all
/// the listing needs, and a deployment path such as `/anthropic` is not part of the listing's
/// address. That is also why the API-key route can insist the two origins match.
///
/// # Errors
///
/// Returns [`OptionsError`] when `base_url` is not an HTTP(S) root.
pub fn model_listing_url(base_url: &str) -> Result<String, OptionsError> {
    let (origin, _) = parse_origin(base_url).ok_or_else(invalid_base_url)?;
    Ok(format!("{}/models", origin.as_str()))
}

/// The origin of a base URL, for comparing one endpoint with another.
///
/// # Errors
///
/// Returns [`OptionsError`] when `base_url` is not an HTTP(S) root.
pub fn origin_of(base_url: &str) -> Result<Origin, OptionsError> {
    parse_origin(base_url)
        .map(|(origin, _)| origin)
        .ok_or_else(invalid_base_url)
}

/// Upstream's one base-URL refusal.
fn invalid_base_url() -> OptionsError {
    OptionsError::new(
        "llm-deepseek: Messages baseURL must be an HTTP(S) root without credentials, query, or fragment",
    )
}

// =============================================================================================
// The protocol resolve step — upstream `resolveAdapterOptions`
// =============================================================================================

/// Plain protocol options accepted by [`resolve_options`] — upstream `Options`.
///
/// Every member is optional, and `None` means "not configured", which is what lets one struct serve
/// both a settings file and a programmatic construction. Integer members are `u64` and timeouts are
/// `f64`; the checks that must *see* `1.5` in order to reject it run in [`Options::from_json`],
/// which owns the JSON value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    /// Messages endpoint root.
    pub base_url: Option<String>,
    /// Deployment thinking policy.
    pub thinking: Option<Thinking>,
    /// Default thinking effort.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Default per-request output cap.
    pub max_tokens: Option<u64>,
    /// Context capacity for a model that declares none.
    pub default_context_window: Option<u64>,
    /// Advisory catalog; `None` resolves to the built-in one.
    pub models: Option<Vec<CatalogModel>>,
    /// Stream idle bound, in milliseconds.
    pub stream_idle_timeout_ms: Option<f64>,
    /// File-referenced image byte bound.
    pub max_request_files_bytes: Option<u64>,
    /// Inline base64 image byte bound.
    pub max_inline_request_image_bytes: Option<u64>,
    /// Represented image count bound.
    pub max_images_per_request: Option<u64>,
    /// Raw-byte removal step.
    pub image_offload_byte_quantum: Option<u64>,
    /// Inline-byte removal step.
    pub inline_image_offload_byte_quantum: Option<u64>,
    /// Image-count removal step.
    pub image_offload_count_quantum: Option<u64>,
    /// Files API resolution deadline, in milliseconds.
    pub files_api_timeout_ms: Option<f64>,
    /// Upload lifetime, in seconds.
    pub file_expires_after_seconds: Option<u64>,
    /// Proactive refresh window, in seconds.
    pub file_refresh_margin_seconds: Option<u64>,
    /// Quota-recovery deletion batch.
    pub file_quota_cleanup_batch: Option<u64>,
    /// Provider-owned retry policy.
    pub retry_policy: Option<RetryPolicyConfig>,
}

/// Resolved protocol settings — upstream `DeepSeekConnectionOptions`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedOptions {
    /// Messages endpoint root, unchanged.
    pub base_url: String,
    /// Request defaults.
    pub defaults: RequestDefaults,
    /// Default per-request output cap.
    pub max_tokens: u64,
    /// Context capacity for a model that declares none.
    pub default_context_window: u64,
    /// Normalized advisory catalog.
    pub models: Vec<CatalogModel>,
    /// Stream idle bound, in milliseconds.
    pub stream_idle_timeout_ms: f64,
    /// File-referenced image byte bound.
    pub max_request_files_bytes: u64,
    /// Inline base64 image byte bound.
    pub max_inline_request_image_bytes: u64,
    /// Represented image count bound.
    pub max_images_per_request: u64,
    /// Raw-byte removal step.
    pub image_offload_byte_quantum: u64,
    /// Inline-byte removal step.
    pub inline_image_offload_byte_quantum: u64,
    /// Image-count removal step.
    pub image_offload_count_quantum: u64,
    /// Files API resolution deadline, in milliseconds.
    pub files_api_timeout_ms: f64,
    /// Expiry, refresh, and quota-recovery policy.
    pub file_policy: FilePolicy,
    /// Fully resolved retry policy.
    pub retry_policy: ResolvedRetryPolicy,
}

/// The members `config.ts`'s `Config` declares, for the JSON path's unknown-key refusal.
const PROTOCOL_KEYS: [&str; 18] = [
    "baseURL",
    "thinking",
    "reasoningEffort",
    "maxTokens",
    "defaultContextWindow",
    "models",
    "streamIdleTimeoutMs",
    "maxRequestFilesBytes",
    "maxInlineRequestImageBytes",
    "maxImagesPerRequest",
    "imageOffloadByteQuantum",
    "inlineImageOffloadByteQuantum",
    "imageOffloadCountQuantum",
    "filesApiTimeoutMs",
    "fileExpiresAfterSeconds",
    "fileRefreshMarginSeconds",
    "fileQuotaCleanupBatch",
    "retryPolicy",
];

impl Options {
    /// Reads protocol options from a settings object.
    ///
    /// Two members are refused *here* rather than by a field rule, because a settings file can carry
    /// what a typed construction cannot: `protocol`, with upstream's "not configurable" message
    /// (checked first, as the resolver checks it first), and any unknown member, with
    /// `llm-deepseek: unknown key "…"`.
    ///
    /// # Errors
    ///
    /// Returns [`OptionsError`] naming the first unusable member.
    pub fn from_json(config: &Map<String, Value>) -> Result<Self, OptionsError> {
        Self::from_json_with(config, &[])
    }

    /// Reads protocol options, tolerating the route-specific members `extra_keys` names.
    ///
    /// The account and API-key routes embed these fields in one settings object with their own
    /// additions, so their parser must not report `modelsTimeoutMs` as unknown when this one runs.
    fn from_json_with(
        config: &Map<String, Value>,
        extra_keys: &[&str],
    ) -> Result<Self, OptionsError> {
        if config.contains_key("protocol") {
            return Err(OptionsError::new(
                "llm-deepseek: protocol is not configurable; remove it and use a Messages-compatible baseURL",
            ));
        }
        for key in config.keys() {
            if !PROTOCOL_KEYS.contains(&key.as_str()) && !extra_keys.contains(&key.as_str()) {
                return Err(unknown_key("llm-deepseek", key));
            }
        }
        Ok(Self {
            base_url: string_member(config, "baseURL")?,
            thinking: match config.get("thinking") {
                None | Some(Value::Null) => None,
                Some(value) => value
                    .as_str()
                    .and_then(Thinking::parse)
                    .map(Some)
                    .ok_or_else(|| {
                        OptionsError::new("llm-deepseek: thinking must be \"enabled\" or \"disabled\"")
                    })?,
            },
            reasoning_effort: match config.get("reasoningEffort") {
                None | Some(Value::Null) => None,
                Some(value) => value
                    .as_str()
                    .and_then(ReasoningEffort::parse)
                    .map(Some)
                    .ok_or_else(|| {
                        OptionsError::new(
                            "llm-deepseek: reasoningEffort must be \"off\", \"low\", \"high\", or \"max\"",
                        )
                    })?,
            },
            max_tokens: integral_member(
                config.get("maxTokens"),
                "llm-deepseek: maxTokens must be a positive safe integer",
            )?,
            default_context_window: integral_member(
                config.get("defaultContextWindow"),
                "llm-deepseek: defaultContextWindow must be a positive integer",
            )?,
            models: match config.get("models") {
                None | Some(Value::Null) => None,
                Some(Value::Array(rows)) => {
                    let mut resolved = Vec::with_capacity(rows.len());
                    for row in rows {
                        resolved.push(resolve_catalog_spec(&catalog_spec_from_json(row)?, false)?);
                    }
                    Some(resolved)
                }
                Some(_) => {
                    return Err(OptionsError::new(
                        "llm-deepseek: models must be an array of catalog entries",
                    ));
                }
            },
            stream_idle_timeout_ms: number_member(config.get("streamIdleTimeoutMs"), "streamIdleTimeoutMs")?,
            max_request_files_bytes: integral_member(
                config.get("maxRequestFilesBytes"),
                "llm-deepseek: maxRequestFilesBytes must be a positive safe integer",
            )?,
            max_inline_request_image_bytes: integral_member(
                config.get("maxInlineRequestImageBytes"),
                "llm-deepseek: maxInlineRequestImageBytes must be a positive safe integer",
            )?,
            max_images_per_request: integral_member(
                config.get("maxImagesPerRequest"),
                "llm-deepseek: maxImagesPerRequest must be a positive safe integer",
            )?,
            image_offload_byte_quantum: integral_member(
                config.get("imageOffloadByteQuantum"),
                "llm-deepseek: imageOffloadByteQuantum must be a positive safe integer",
            )?,
            inline_image_offload_byte_quantum: integral_member(
                config.get("inlineImageOffloadByteQuantum"),
                "llm-deepseek: inlineImageOffloadByteQuantum must be a positive safe integer",
            )?,
            image_offload_count_quantum: integral_member(
                config.get("imageOffloadCountQuantum"),
                "llm-deepseek: imageOffloadCountQuantum must be a positive safe integer",
            )?,
            files_api_timeout_ms: number_member(config.get("filesApiTimeoutMs"), "filesApiTimeoutMs")?,
            file_expires_after_seconds: integral_member(
                config.get("fileExpiresAfterSeconds"),
                &format!(
                    "llm-deepseek: fileExpiresAfterSeconds must be an integer from {MIN_FILE_EXPIRY_SECONDS} through {MAX_FILE_EXPIRY_SECONDS}"
                ),
            )?,
            file_refresh_margin_seconds: integral_member(
                config.get("fileRefreshMarginSeconds"),
                "llm-deepseek: fileRefreshMarginSeconds must be a non-negative integer below fileExpiresAfterSeconds",
            )?,
            file_quota_cleanup_batch: integral_member(
                config.get("fileQuotaCleanupBatch"),
                "llm-deepseek: fileQuotaCleanupBatch must be an integer from 1 through 1000",
            )?,
            retry_policy: match config.get("retryPolicy") {
                None | Some(Value::Null) => None,
                Some(Value::Object(members)) => Some(RetryPolicyConfig::from_json(
                    members,
                    "llm-deepseek: retryPolicy",
                )?),
                Some(_) => {
                    return Err(OptionsError::new(
                        "llm-deepseek: retryPolicy must be an object",
                    ));
                }
            },
        })
    }
}

/// An environment the resolver may read the endpoint from.
///
/// Upstream takes a `LaunchEnvironmentSnapshot` whose layers are searched most-trusted first. This
/// is the same contract as a trait rather than a struct, so a caller can hand over the launcher's
/// snapshot — including its "an empty value is still a value" behaviour, which is why the endpoint
/// can be present and empty here, and then fail the base-URL rule rather than the fallback.
pub trait LaunchEnvironment {
    /// Resolves one name across every layer, most trusted first.
    fn get(&self, name: &str) -> Option<String>;
}

/// An empty environment: no layer supplies anything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEnvironment;

impl LaunchEnvironment for NoEnvironment {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}

/// Resolves, defaults, and validates one protocol configuration.
///
/// The order is upstream's, which matters when a configuration is wrong twice: `thinking` and
/// `reasoningEffort`, then each numeric field in declaration order with its cross-field companion
/// immediately after it, then the base URL, then the catalog, then the retry policy.
///
/// # Errors
///
/// Returns [`OptionsError`] for the first rule the configuration breaks.
pub fn resolve_options(
    config: &Options,
    environment: &dyn LaunchEnvironment,
) -> Result<ResolvedOptions, OptionsError> {
    if config.thinking == Some(Thinking::Disabled)
        && config
            .reasoning_effort
            .is_some_and(|effort| effort != ReasoningEffort::Off)
    {
        return Err(OptionsError::new(
            "llm-deepseek: only reasoningEffort \"off\" can be configured when thinking is disabled",
        ));
    }
    if config
        .default_context_window
        .is_some_and(|window| window == 0 || window > MAX_SAFE_INTEGER)
    {
        return Err(OptionsError::new(
            "llm-deepseek: defaultContextWindow must be a positive integer",
        ));
    }
    if config
        .max_tokens
        .is_some_and(|max_tokens| max_tokens == 0 || max_tokens > MAX_SAFE_INTEGER)
    {
        return Err(OptionsError::new(
            "llm-deepseek: maxTokens must be a positive safe integer",
        ));
    }
    let stream_idle_timeout_ms = config
        .stream_idle_timeout_ms
        .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_MS);
    if !valid_timeout(stream_idle_timeout_ms) {
        return Err(OptionsError::new(format!(
            "llm-deepseek: streamIdleTimeoutMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )));
    }
    let max_request_files_bytes = config
        .max_request_files_bytes
        .unwrap_or(DEFAULT_MAX_REQUEST_FILES_BYTES);
    if !safe_positive(max_request_files_bytes) {
        return Err(OptionsError::new(
            "llm-deepseek: maxRequestFilesBytes must be a positive safe integer",
        ));
    }
    let max_inline_request_image_bytes = config
        .max_inline_request_image_bytes
        .unwrap_or(DEFAULT_MAX_INLINE_REQUEST_IMAGE_BYTES);
    if !safe_positive(max_inline_request_image_bytes) {
        return Err(OptionsError::new(
            "llm-deepseek: maxInlineRequestImageBytes must be a positive safe integer",
        ));
    }
    let max_images_per_request = config
        .max_images_per_request
        .unwrap_or(DEFAULT_MAX_IMAGES_PER_REQUEST as u64);
    if !safe_positive(max_images_per_request) {
        return Err(OptionsError::new(
            "llm-deepseek: maxImagesPerRequest must be a positive safe integer",
        ));
    }
    let image_offload_byte_quantum = config
        .image_offload_byte_quantum
        .unwrap_or(DEFAULT_IMAGE_OFFLOAD_BYTE_QUANTUM);
    if !safe_positive(image_offload_byte_quantum) {
        return Err(OptionsError::new(
            "llm-deepseek: imageOffloadByteQuantum must be a positive safe integer",
        ));
    }
    if image_offload_byte_quantum > max_request_files_bytes {
        return Err(OptionsError::new(
            "llm-deepseek: imageOffloadByteQuantum must not exceed maxRequestFilesBytes",
        ));
    }
    let inline_image_offload_byte_quantum = config
        .inline_image_offload_byte_quantum
        .unwrap_or(DEFAULT_INLINE_IMAGE_OFFLOAD_BYTE_QUANTUM);
    if !safe_positive(inline_image_offload_byte_quantum) {
        return Err(OptionsError::new(
            "llm-deepseek: inlineImageOffloadByteQuantum must be a positive safe integer",
        ));
    }
    if inline_image_offload_byte_quantum > max_inline_request_image_bytes {
        return Err(OptionsError::new(
            "llm-deepseek: inlineImageOffloadByteQuantum must not exceed maxInlineRequestImageBytes",
        ));
    }
    let image_offload_count_quantum = config
        .image_offload_count_quantum
        .unwrap_or(DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM as u64);
    if !safe_positive(image_offload_count_quantum) {
        return Err(OptionsError::new(
            "llm-deepseek: imageOffloadCountQuantum must be a positive safe integer",
        ));
    }
    if image_offload_count_quantum > max_images_per_request {
        return Err(OptionsError::new(
            "llm-deepseek: imageOffloadCountQuantum must not exceed maxImagesPerRequest",
        ));
    }
    let files_api_timeout_ms = config
        .files_api_timeout_ms
        .unwrap_or(DEFAULT_FILES_API_TIMEOUT_MS_F64);
    if !valid_timeout(files_api_timeout_ms) {
        return Err(OptionsError::new(format!(
            "llm-deepseek: filesApiTimeoutMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )));
    }
    let file_expires_after_seconds = config
        .file_expires_after_seconds
        .unwrap_or(DEFAULT_FILE_EXPIRY_SECONDS);
    if !(MIN_FILE_EXPIRY_SECONDS..=MAX_FILE_EXPIRY_SECONDS).contains(&file_expires_after_seconds) {
        return Err(OptionsError::new(format!(
            "llm-deepseek: fileExpiresAfterSeconds must be an integer from {MIN_FILE_EXPIRY_SECONDS} through {MAX_FILE_EXPIRY_SECONDS}"
        )));
    }
    let file_refresh_margin_seconds = config
        .file_refresh_margin_seconds
        .unwrap_or(DEFAULT_FILE_REFRESH_MARGIN_SECONDS);
    if file_refresh_margin_seconds >= file_expires_after_seconds {
        return Err(OptionsError::new(
            "llm-deepseek: fileRefreshMarginSeconds must be a non-negative integer below fileExpiresAfterSeconds",
        ));
    }
    let file_quota_cleanup_batch = config
        .file_quota_cleanup_batch
        .unwrap_or(u64::from(DEFAULT_FILE_QUOTA_CLEANUP_BATCH));
    if !(1..=1_000).contains(&file_quota_cleanup_batch) {
        return Err(OptionsError::new(
            "llm-deepseek: fileQuotaCleanupBatch must be an integer from 1 through 1000",
        ));
    }
    let base_url = config
        .base_url
        .clone()
        .or_else(|| environment.get(BASE_URL_ENV))
        .unwrap_or_else(|| PUBLIC_BASE_URL.to_owned());
    validate_base_url(&base_url)?;
    let models = resolve_models(config.models.as_deref())?;
    let retry_policy =
        resolve_retry_policy(config.retry_policy.as_ref(), "llm-deepseek: retryPolicy")?;
    Ok(ResolvedOptions {
        base_url,
        defaults: RequestDefaults {
            thinking: config.thinking,
            reasoning_effort: config.reasoning_effort,
        },
        max_tokens: config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        default_context_window: config
            .default_context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW),
        models,
        stream_idle_timeout_ms,
        max_request_files_bytes,
        max_inline_request_image_bytes,
        max_images_per_request,
        image_offload_byte_quantum,
        inline_image_offload_byte_quantum,
        image_offload_count_quantum,
        files_api_timeout_ms,
        file_policy: FilePolicy {
            expires_after_seconds: file_expires_after_seconds,
            refresh_margin_seconds: file_refresh_margin_seconds,
            quota_cleanup_batch: u32::try_from(file_quota_cleanup_batch).unwrap_or(u32::MAX),
        },
        retry_policy,
    })
}

/// Whether a timeout is positive, finite, and within the timer ceiling.
fn valid_timeout(value: f64) -> bool {
    value.is_finite() && value > 0.0 && value <= MAX_TIMER_DELAY_MS_F64
}

/// Whether a value is a positive integer a JavaScript number holds exactly.
fn safe_positive(value: u64) -> bool {
    value != 0 && value <= MAX_SAFE_INTEGER
}

// =============================================================================================
// JSON helpers
// =============================================================================================

/// Reads an optional string member.
fn string_member(config: &Map<String, Value>, key: &str) -> Result<Option<String>, OptionsError> {
    match config.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(OptionsError::new(format!(
            "llm-deepseek: {key} must be a string"
        ))),
    }
}

/// Reads an optional finite-number member, leaving its bounds to the ladder.
fn number_member(value: Option<&Value>, key: &str) -> Result<Option<f64>, OptionsError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_f64().map(Some).ok_or_else(|| {
            OptionsError::new(format!("llm-deepseek: {key} must be a finite number"))
        }),
    }
}

/// Reads one optional JSON value as an integral member.
///
/// The *message* is the ladder's, deliberately: upstream reaches these refusals through
/// `!Number.isSafeInteger`, which refuses `1.5` with the same sentence as `0`. Refusing a
/// non-integer earlier with a different message would break the contract the tests assert.
fn integral_member(value: Option<&Value>, message: &str) -> Result<Option<u64>, OptionsError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => integer_member(value, message).map(Some),
    }
}

/// Reads one JSON value as a non-negative integer within [`MAX_SAFE_INTEGER`].
fn integer_member(value: &Value, message: &str) -> Result<u64, OptionsError> {
    let Some(number) = value.as_f64() else {
        return Err(OptionsError::new(message));
    };
    if !number.is_finite()
        || number.fract() != 0.0
        || !(0.0..=MAX_SAFE_INTEGER_F64).contains(&number)
    {
        return Err(OptionsError::new(message));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is integral, non-negative, and bounded by 2^53-1, which u64 holds exactly"
    )]
    let narrowed = number as u64;
    Ok(narrowed)
}

/// Reads one catalog entry from a settings object.
fn catalog_spec_from_json(value: &Value) -> Result<CatalogSpec, OptionsError> {
    const KEYS: [&str; 10] = [
        "id",
        "name",
        "description",
        "contextWindow",
        "maxTokens",
        "inputModalities",
        "imagePixelBudget",
        "imageMaxBytes",
        "systemPromptUpdate",
        "toolUpdate",
    ];

    let Value::Object(members) = value else {
        return Err(OptionsError::new(
            "llm-deepseek: catalog model must be an object",
        ));
    };
    for key in members.keys() {
        if key == "imageDetail" {
            return Err(OptionsError::new(
                "llm-deepseek: catalog model imageDetail is no longer supported; use imagePixelBudget",
            ));
        }
        if !KEYS.contains(&key.as_str()) {
            return Err(unknown_key("llm-deepseek: catalog model", key));
        }
    }
    let id = match members.get("id") {
        Some(Value::String(text)) => text.clone(),
        Some(_) => {
            return Err(OptionsError::new(
                "llm-deepseek: catalog model id must be a string",
            ));
        }
        None => String::new(),
    };
    let label = format!("llm-deepseek: catalog model \"{id}\"");
    Ok(CatalogSpec {
        id,
        name: opt_string(members, "name", &label)?,
        description: opt_string(members, "description", &label)?,
        context_window: opt_integer(
            members.get("contextWindow"),
            &format!("{label} contextWindow must be a positive integer"),
        )?,
        max_output_tokens: opt_integer(
            members.get("maxTokens"),
            &format!("{label} maxTokens must be a positive integer"),
        )?,
        input_modalities: match members.get("inputModalities") {
            None | Some(Value::Null) => None,
            Some(Value::Array(rows)) => Some(modalities_of(rows, &label)?),
            Some(_) => {
                return Err(OptionsError::new(format!(
                    "{label} inputModalities must be an array"
                )));
            }
        },
        image_pixel_budget: match members.get("imagePixelBudget") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) if text == "low" => Some(PixelBudget::Low),
            Some(value) => Some(PixelBudget::Pixels(integer_member(
                value,
                &format!("{label} imagePixelBudget must be \"low\" or a positive safe integer"),
            )?)),
        },
        image_max_bytes: opt_integer(
            members.get("imageMaxBytes"),
            &format!("{label} imageMaxBytes must be a positive safe integer"),
        )?,
        system_prompt_update: match members.get("systemPromptUpdate") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) if text == "in-history" => Some(SystemPromptMode::InHistory),
            Some(Value::String(text)) if text == "leading-only" => {
                Some(SystemPromptMode::LeadingOnly)
            }
            Some(_) => {
                return Err(OptionsError::new(format!(
                    "{label} systemPromptUpdate must be \"in-history\" when present"
                )));
            }
        },
        tool_update: match members.get("toolUpdate") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) if text == "in-history" => Some(ToolUpdateMode::InHistory),
            Some(Value::String(text)) if text == "addition-only" => {
                Some(ToolUpdateMode::AdditionOnly)
            }
            Some(Value::String(text)) if text == "leading-only" => {
                Some(ToolUpdateMode::LeadingOnly)
            }
            Some(_) => {
                return Err(OptionsError::new(format!(
                    "{label} toolUpdate must be \"in-history\" or \"addition-only\" when present"
                )));
            }
        },
        legacy_image_detail: false,
    })
}

/// Reads a modality array, refusing anything outside the two known spellings.
fn modalities_of(rows: &[Value], label: &str) -> Result<Vec<Modality>, OptionsError> {
    let mut modalities = Vec::with_capacity(rows.len());
    for row in rows {
        let modality = row.as_str().and_then(|text| match text {
            "text" => Some(Modality::Text),
            "image" => Some(Modality::Image),
            _ => None,
        });
        let Some(modality) = modality else {
            return Err(OptionsError::new(format!(
                "{label} inputModalities must contain only \"text\" and \"image\""
            )));
        };
        modalities.push(modality);
    }
    Ok(modalities)
}

/// Reads an optional string member of a catalog entry.
fn opt_string(
    members: &Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<Option<String>, OptionsError> {
    match members.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(OptionsError::new(format!("{label} {key} must be a string"))),
    }
}

/// Reads an optional integer member of a catalog entry.
fn opt_integer(value: Option<&Value>, message: &str) -> Result<Option<u64>, OptionsError> {
    integral_member(value, message)
}

// =============================================================================================
// Listing timing — upstream `listing-config.ts`
// =============================================================================================

/// Plain listing options — upstream `ModelListingOptions`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ListingOptions {
    /// Deadline of one listing request.
    pub models_timeout_ms: Option<f64>,
    /// Refresh interval after first use.
    pub models_max_age_ms: Option<f64>,
}

/// Validated listing cache timing — upstream `ListingTiming`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListingTiming {
    /// Deadline of one listing request, in milliseconds.
    pub timeout_ms: f64,
    /// Age after which the next access refreshes the listing.
    pub max_age_ms: u64,
}

/// Listing endpoint and timing — upstream `ListingSettings`.
#[derive(Clone, Debug, PartialEq)]
pub struct ListingSettings {
    /// `/models` at the Messages origin.
    pub url: String,
    /// Validated timeout.
    pub timeout_ms: f64,
    /// Validated refresh age.
    pub max_age_ms: u64,
}

/// Validates listing timing — upstream `resolveListingTiming`.
///
/// `owner` prefixes each refusal with the route that owns the setting (`llm-deepseek-account` or
/// `llm-deepseek-api-key`), because both routes read the same resolver and a message naming only the
/// field would leave a reader guessing which entry to edit.
///
/// # Errors
///
/// Returns [`OptionsError`] for a timeout outside `(0, MAX_TIMER_DELAY_MS]` or an age that is not a
/// non-negative safe integer.
pub fn resolve_listing_timing(
    options: &ListingOptions,
    owner: &str,
) -> Result<ListingTiming, OptionsError> {
    let timeout_ms = options
        .models_timeout_ms
        .unwrap_or(DEFAULT_MODELS_TIMEOUT_MS);
    if !valid_timeout(timeout_ms) {
        return Err(OptionsError::new(format!(
            "{owner}: modelsTimeoutMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )));
    }
    let max_age_ms = match options.models_max_age_ms {
        None => DEFAULT_MODELS_MAX_AGE_MS,
        Some(number)
            if number.is_finite()
                && number.fract() == 0.0
                && (0.0..=MAX_SAFE_INTEGER_F64).contains(&number) =>
        {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the value is integral, non-negative, and bounded by 2^53-1"
            )]
            let narrowed = number as u64;
            narrowed
        }
        Some(_) => {
            return Err(OptionsError::new(format!(
                "{owner}: modelsMaxAgeMs must be a non-negative safe integer"
            )));
        }
    };
    Ok(ListingTiming {
        timeout_ms,
        max_age_ms,
    })
}

// =============================================================================================
// Account route — upstream `llm-deepseek-account/config.ts`
// =============================================================================================

/// Plain account options — upstream `AccountOptions`.
///
/// `models` is absent from the struct and present in [`AccountOptions::from_json`] as a refusal: the
/// account `/models` listing supplies the catalog, and a settings file that still carries the retired
/// member must fail loudly rather than be silently ignored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountOptions {
    /// Protocol options, without a catalog.
    pub protocol: Options,
    /// Listing timing.
    pub listing: ListingOptions,
    /// Declared overrides, keyed by model id, as their raw objects.
    pub model_overrides: BTreeMap<String, Map<String, Value>>,
}

/// One validated account configuration generation — upstream `ResolvedAccountOptions`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedAccountOptions {
    /// Messages settings; the request path replaces `models` with the account catalog.
    pub protocol: ResolvedOptions,
    /// Listing endpoint and cache timing.
    pub catalog: ListingSettings,
    /// Overrides by model id, holding only the fields each one declares.
    pub model_overrides: BTreeMap<String, ModelOverride>,
}

/// The members the account route adds to the protocol ones.
const ACCOUNT_KEYS: [&str; 3] = ["modelsTimeoutMs", "modelsMaxAgeMs", "modelOverrides"];

impl AccountOptions {
    /// Reads account options from a settings object.
    ///
    /// # Errors
    ///
    /// Returns [`OptionsError`] naming the first unusable member; `models` is refused with the
    /// account route's retirement message.
    pub fn from_json(config: &Map<String, Value>) -> Result<Self, OptionsError> {
        if config.contains_key("models") {
            return Err(OptionsError::new(
                "llm-deepseek-account: models is no longer configurable; the account /models \
                 listing supplies the catalog. Remove models from this entry and adjust single \
                 models with modelOverrides.<model id>",
            ));
        }
        for key in config.keys() {
            if !ACCOUNT_KEYS.contains(&key.as_str()) && !PROTOCOL_KEYS.contains(&key.as_str()) {
                return Err(unknown_key("llm-deepseek-account", key));
            }
        }
        Ok(Self {
            protocol: Options::from_json_with(config, &ACCOUNT_KEYS)?,
            listing: ListingOptions {
                models_timeout_ms: number_member(config.get("modelsTimeoutMs"), "modelsTimeoutMs")?,
                models_max_age_ms: number_member(config.get("modelsMaxAgeMs"), "modelsMaxAgeMs")?,
            },
            model_overrides: match config.get("modelOverrides") {
                None | Some(Value::Null) => BTreeMap::new(),
                Some(Value::Object(entries)) => {
                    let mut overrides = BTreeMap::new();
                    for (id, value) in entries {
                        let Value::Object(members) = value else {
                            return Err(OptionsError::new(format!(
                                "llm-deepseek-account: modelOverrides.{id} must be an object"
                            )));
                        };
                        overrides.insert(id.clone(), members.clone());
                    }
                    overrides
                }
                Some(_) => {
                    return Err(OptionsError::new(
                        "llm-deepseek-account: modelOverrides must be an object",
                    ));
                }
            },
        })
    }
}

/// Resolves one account configuration — upstream `resolveAccountOptions`.
///
/// # Errors
///
/// Returns [`OptionsError`] from the listing timing, the protocol ladder, or one override.
pub fn resolve_account_options(
    options: &AccountOptions,
    environment: &dyn LaunchEnvironment,
) -> Result<ResolvedAccountOptions, OptionsError> {
    let timing = resolve_listing_timing(&options.listing, "llm-deepseek-account")?;
    let protocol = resolve_options(&options.protocol, environment)?;
    let url = model_listing_url(&protocol.base_url)?;
    let mut overrides = BTreeMap::new();
    for (id, declared) in &options.model_overrides {
        overrides.insert(id.clone(), resolve_override(id, declared)?);
    }
    Ok(ResolvedAccountOptions {
        catalog: ListingSettings {
            url,
            timeout_ms: timing.timeout_ms,
            max_age_ms: timing.max_age_ms,
        },
        protocol,
        model_overrides: overrides,
    })
}

/// Validates one override and keeps the fields it declares — upstream `resolveOverride`.
///
/// The stand-in entry is why an override declaring `imageMaxBytes` on a text-only model is refused
/// even though the model it will layer over is only known once a listing loads: the override is
/// judged as if it were a complete image-capable entry, the strictest thing it could become.
///
/// An entry that is `null` or an empty array means "inherit", not "declare", so it is dropped before
/// validation — which is what makes `inputModalities: []` leave the base list alone.
///
/// # Errors
///
/// Returns [`OptionsError`] with the account route's prefix; the underlying catalog refusal is
/// preserved inside it, as upstream preserves it as `cause`.
pub fn resolve_override(
    id: &str,
    declared: &Map<String, Value>,
) -> Result<ModelOverride, OptionsError> {
    if id.is_empty() {
        return Err(OptionsError::new(
            "llm-deepseek-account: modelOverrides keys must be non-empty model ids",
        ));
    }
    if declared.contains_key("id") {
        return Err(OptionsError::new(format!(
            "llm-deepseek-account: modelOverrides.{id} sets \"id\", which is its key"
        )));
    }
    let declared = strip_inherited(declared);
    let spec = override_spec(id, &declared).map_err(|error| override_invalid(id, &error))?;
    resolve_catalog_spec(&spec, true).map_err(|error| override_invalid(id, &error))?;
    Ok(ModelOverride {
        name: spec.name,
        input_modalities: spec.input_modalities,
        context_window: spec.context_window,
        max_output_tokens: spec.max_output_tokens,
        system_prompt_update: spec.system_prompt_update,
        tool_update: spec.tool_update,
        image_pixel_budget: spec.image_pixel_budget,
        image_max_bytes: spec.image_max_bytes,
    })
}

/// The refusal upstream builds around a rejected override, preserving the inner message.
fn override_invalid(id: &str, inner: &OptionsError) -> OptionsError {
    OptionsError::new(format!(
        "llm-deepseek-account: modelOverrides.{id} is invalid: {}",
        inner.message()
    ))
}

/// Drops the members that mean "inherit".
fn strip_inherited(declared: &Map<String, Value>) -> Map<String, Value> {
    let mut kept = Map::new();
    for (key, value) in declared {
        if value.is_null() {
            continue;
        }
        if let Value::Array(rows) = value {
            if rows.is_empty() {
                continue;
            }
        }
        kept.insert(key.clone(), value.clone());
    }
    kept
}

/// Builds the catalog spec of one override, keeping only the members it declares.
fn override_spec(id: &str, declared: &Map<String, Value>) -> Result<CatalogSpec, OptionsError> {
    let label = format!("llm-deepseek: catalog model \"{id}\"");
    let mut spec = CatalogSpec::new(id);
    for key in declared.keys() {
        match key.as_str() {
            "name" => spec.name = opt_string(declared, "name", &label)?,
            "description" => spec.description = opt_string(declared, "description", &label)?,
            "contextWindow" => {
                spec.context_window = opt_integer(
                    declared.get("contextWindow"),
                    &format!("{label} contextWindow must be a positive integer"),
                )?;
            }
            "maxTokens" => {
                spec.max_output_tokens = opt_integer(
                    declared.get("maxTokens"),
                    &format!("{label} maxTokens must be a positive integer"),
                )?;
            }
            "inputModalities" => {
                let Some(Value::Array(rows)) = declared.get("inputModalities") else {
                    return Err(OptionsError::new(format!(
                        "{label} inputModalities must be an array"
                    )));
                };
                spec.input_modalities = Some(modalities_of(rows, &label)?);
            }
            "imagePixelBudget" => {
                spec.image_pixel_budget = match declared.get("imagePixelBudget") {
                    Some(Value::String(text)) if text == "low" => Some(PixelBudget::Low),
                    Some(value) => Some(PixelBudget::Pixels(integer_member(
                        value,
                        &format!(
                            "{label} imagePixelBudget must be \"low\" or a positive safe integer"
                        ),
                    )?)),
                    None => None,
                };
            }
            "imageMaxBytes" => {
                spec.image_max_bytes = opt_integer(
                    declared.get("imageMaxBytes"),
                    &format!("{label} imageMaxBytes must be a positive safe integer"),
                )?;
            }
            "systemPromptUpdate" => {
                spec.system_prompt_update = match declared.get("systemPromptUpdate") {
                    Some(Value::String(text)) if text == "in-history" => {
                        Some(SystemPromptMode::InHistory)
                    }
                    Some(Value::String(text)) if text == "leading-only" => {
                        Some(SystemPromptMode::LeadingOnly)
                    }
                    _ => {
                        return Err(OptionsError::new(format!(
                            "{label} systemPromptUpdate must be \"in-history\" when present"
                        )));
                    }
                };
            }
            "toolUpdate" => {
                spec.tool_update = match declared.get("toolUpdate") {
                    Some(Value::String(text)) if text == "in-history" => {
                        Some(ToolUpdateMode::InHistory)
                    }
                    Some(Value::String(text)) if text == "addition-only" => {
                        Some(ToolUpdateMode::AdditionOnly)
                    }
                    Some(Value::String(text)) if text == "leading-only" => {
                        Some(ToolUpdateMode::LeadingOnly)
                    }
                    _ => {
                        return Err(OptionsError::new(format!(
                            "{label} toolUpdate must be \"in-history\" or \"addition-only\" when present"
                        )));
                    }
                };
            }
            other => return Err(unknown_key("llm-deepseek-account: modelOverrides", other)),
        }
    }
    Ok(spec)
}

// =============================================================================================
// API-key route — upstream `llm-deepseek-api-key/config.ts`
// =============================================================================================

/// Plain API-key route options — upstream `Options`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApiKeyOptions {
    /// Protocol options.
    pub protocol: Options,
    /// Listing timing.
    pub listing: ListingOptions,
    /// Credential reference; `None` takes [`DEFAULT_API_KEY_ENV`].
    pub api_key_env: Option<String>,
}

/// One resolved API-key configuration — upstream `ResolvedDeepSeekOptions`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedApiKeyOptions {
    /// Messages settings.
    pub protocol: ResolvedOptions,
    /// The credential reference this generation uses.
    pub api_key_env: String,
    /// The official listing this generation follows, absent otherwise.
    pub listing: Option<ListingSettings>,
}

/// The members the API-key route adds to the protocol ones.
const API_KEY_KEYS: [&str; 3] = ["modelsTimeoutMs", "modelsMaxAgeMs", "apiKeyEnv"];

impl ApiKeyOptions {
    /// Reads API-key route options from a settings object.
    ///
    /// # Errors
    ///
    /// Returns [`OptionsError`] naming the first unusable member.
    pub fn from_json(config: &Map<String, Value>) -> Result<Self, OptionsError> {
        for key in config.keys() {
            if !API_KEY_KEYS.contains(&key.as_str()) && !PROTOCOL_KEYS.contains(&key.as_str()) {
                return Err(unknown_key("llm-deepseek-api-key", key));
            }
        }
        Ok(Self {
            protocol: Options::from_json_with(config, &API_KEY_KEYS)?,
            listing: ListingOptions {
                models_timeout_ms: number_member(config.get("modelsTimeoutMs"), "modelsTimeoutMs")?,
                models_max_age_ms: number_member(config.get("modelsMaxAgeMs"), "modelsMaxAgeMs")?,
            },
            api_key_env: string_member(config, "apiKeyEnv")?,
        })
    }
}

/// Whether a name is a POSIX identifier upstream accepts as a credential reference.
///
/// `/^[A-Za-z_][A-Za-z0-9_]*$/` — the grammar `credentialRef` enforces, restated because this module
/// resolves the reference and the credential store is a different seam.
#[must_use]
pub fn is_credential_ref_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Resolves one API-key configuration — upstream `resolveAdapterOptions`.
///
/// The listing is followed only when both halves of upstream's condition hold: the endpoint's origin
/// is the official one *and* the catalog is the built-in one. Either half alone is a deployment that
/// has said something about where its models come from, and a listing fetched over the other would
/// overwrite it.
///
/// # Errors
///
/// Returns [`OptionsError`] from the listing timing, the protocol ladder, or a credential reference
/// outside the POSIX-identifier grammar.
pub fn resolve_api_key_options(
    options: &ApiKeyOptions,
    environment: &dyn LaunchEnvironment,
) -> Result<ResolvedApiKeyOptions, OptionsError> {
    let timing = resolve_listing_timing(&options.listing, "llm-deepseek-api-key")?;
    let protocol = resolve_options(&options.protocol, environment)?;
    let api_key_env = options
        .api_key_env
        .clone()
        .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned());
    if !is_credential_ref_name(&api_key_env) {
        return Err(OptionsError::new(format!(
            "credential ref \"{api_key_env}\" must match /^[A-Za-z_][A-Za-z0-9_]*$/"
        )));
    }
    let official = origin_of(PUBLIC_BASE_URL)?;
    let listed = origin_of(&protocol.base_url)? == official
        && is_built_in_catalog(protocol.models.as_slice());
    let listing = if listed {
        Some(ListingSettings {
            url: model_listing_url(&protocol.base_url)?,
            timeout_ms: timing.timeout_ms,
            max_age_ms: timing.max_age_ms,
        })
    } else {
        None
    };
    Ok(ResolvedApiKeyOptions {
        protocol,
        api_key_env,
        listing,
    })
}

// =============================================================================================
// Unit tests
// =============================================================================================

/// The wire spellings of the enums this module resolves.
///
/// Exposed as inherent methods, and exercised here so a driver that reports a policy or a thinking
/// mode writes the same two words the settings file used.
#[cfg(test)]
mod tests {
    // The integration test in `tests/deepseek_options_gap.rs` carries the transcribed upstream
    // vectors; these two cover helpers whose behaviour a resolved value hides.
    use super::*;

    #[test]
    fn wire_spellings_round_trip() {
        for (mode, text) in [(RetryMode::Normal, "normal"), (RetryMode::Always, "always")] {
            assert_eq!(mode.as_str(), text);
        }
        for (thinking, text) in [
            (Thinking::Enabled, "enabled"),
            (Thinking::Disabled, "disabled"),
        ] {
            assert_eq!(thinking.as_str(), text);
            assert_eq!(Thinking::parse(text), Some(thinking));
        }
    }

    #[test]
    fn credential_refs_are_posix_identifiers() {
        assert!(is_credential_ref_name("DEEPSEEK_API_KEY"));
        assert!(is_credential_ref_name("_"));
        assert!(is_credential_ref_name("a1"));
        assert!(!is_credential_ref_name(""));
        assert!(!is_credential_ref_name("1A"));
        assert!(!is_credential_ref_name("A-B"));
        assert!(!is_credential_ref_name("A B"));
    }

    #[test]
    fn origins_drop_default_ports_and_lowercase_hosts() {
        assert_eq!(
            origin_of("https://API.Example.com:443/x")
                .expect("a valid root")
                .as_str(),
            "https://api.example.com"
        );
        assert_eq!(
            origin_of("http://127.0.0.1:9/anthropic")
                .expect("a valid root")
                .as_str(),
            "http://127.0.0.1:9"
        );
        assert!(origin_of("ftp://x").is_err());
        assert!(origin_of("https://user:pw@x/y").is_err());
        assert!(origin_of("https://x/y?z=1").is_err());
        assert!(origin_of("https://x/y#z").is_err());
    }
}
