//! Tests for the `DeepSeek` options ladder: the 18 `config.ts` fields, the cross-field constraints,
//! the two route packages' extra members, and the listing origin rule.
//!
//! The reference vectors are transcriptions of
//! `packages/llm/llm-deepseek/tests/{runtime,built-in-catalog}.spec.ts`,
//! `packages/llm/llm-deepseek-account/tests/config.spec.ts`, and
//! `packages/llm/llm-deepseek-api-key/tests/{config,listing}.spec.ts`, which is why every refusal
//! test names the upstream case it comes from. Where a case cannot be transcribed — upstream's
//! schema rejects a value before the resolver ever sees it, and this build's JSON parser is the only
//! schema — the deviation is stated in the test that covers the neighbouring behaviour.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `deepseek_options_gap`; adding `pub mod
//! deepseek_options_gap;` and its `pub use` belongs to the integrator. Compiling the module into
//! this test binary directly keeps the tests independent of that wiring and leaves the crate root
//! untouched, which is why the imports below name `girder_core::` rather than `crate::`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "tests assert by panicking; a failed unwrap is a failed test, and the per-rule tables \
              are clearer as long cases than as dozens of near-identical functions"
)]

#[path = "../src/llm/deepseek_options.rs"]
pub mod deepseek_options_gap;

use std::collections::BTreeMap;

use deepseek_options_gap::{
    AccountOptions, ApiKeyOptions, BackoffConfig, CatalogModel, CatalogSpec, DEFAULT_API_KEY_ENV,
    DEFAULT_CONTEXT_WINDOW, DEFAULT_FILE_EXPIRY_SECONDS, DEFAULT_FILE_QUOTA_CLEANUP_BATCH,
    DEFAULT_FILE_REFRESH_MARGIN_SECONDS, DEFAULT_FILES_API_TIMEOUT_MS,
    DEFAULT_IMAGE_OFFLOAD_BYTE_QUANTUM, DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM,
    DEFAULT_INITIAL_DELAY_MS, DEFAULT_INLINE_IMAGE_OFFLOAD_BYTE_QUANTUM, DEFAULT_JITTER_RATIO,
    DEFAULT_MAX_DELAY_MS, DEFAULT_MAX_IMAGES_PER_REQUEST, DEFAULT_MAX_INLINE_REQUEST_IMAGE_BYTES,
    DEFAULT_MAX_REQUEST_FILES_BYTES, DEFAULT_MAX_RETRIES, DEFAULT_MAX_TOKENS,
    DEFAULT_MODELS_MAX_AGE_MS, DEFAULT_MODELS_TIMEOUT_MS, DEFAULT_REASONING_EFFORT,
    DEFAULT_STREAM_IDLE_TIMEOUT_MS, FilePolicy, LaunchEnvironment, ListingOptions,
    MAX_SAFE_INTEGER, Modality, ModelOverride, NoEnvironment, Options, PUBLIC_BASE_URL,
    PixelBudget, ReasoningEffort, RequestDefaults, ResolvedApiKeyOptions as ApiKeyResolved,
    ResolvedOptions, ResolvedRetryPolicy, RetryMode, RetryPolicyConfig, SystemPromptMode, Thinking,
    ToolUpdateMode, built_in_models, is_built_in_catalog, is_credential_ref_name,
    model_listing_url, origin_of, resolve_account_options, resolve_api_key_options,
    resolve_catalog_spec, resolve_listing_timing, resolve_models, resolve_options,
    resolve_override, resolve_retry_policy, validate_base_url,
};
use girder_core::llm::extra::MAX_TIMER_DELAY_MS;
use serde_json::{Map, Value, json};

// =============================================================================================
// Harness
// =============================================================================================

/// A fixed environment, so a test never reads the process one.
struct Environment(BTreeMap<String, String>);

impl Environment {
    fn empty() -> Self {
        Self(BTreeMap::new())
    }

    fn with(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }
}

impl LaunchEnvironment for Environment {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// Parses a settings object, then resolves it.
fn options(value: Value) -> Options {
    let Value::Object(members) = value else {
        panic!("a settings object is an object");
    };
    Options::from_json(&members).expect("these settings parse")
}

/// Resolves a settings object and returns the refusal.
fn refusal(value: Value) -> String {
    let Value::Object(members) = value else {
        panic!("a settings object is an object");
    };
    match Options::from_json(&members) {
        Err(error) => error.message().to_owned(),
        Ok(parsed) => match resolve_options(&parsed, &Environment::empty()) {
            Err(error) => error.message().to_owned(),
            Ok(resolved) => panic!("expected a refusal, resolved {:?}", resolved.base_url),
        },
    }
}

/// Resolves a settings object successfully.
fn accepted(value: Value) -> ResolvedOptions {
    let parsed = options(value);
    resolve_options(&parsed, &Environment::empty()).expect("these settings resolve")
}

/// [`MAX_TIMER_DELAY_MS`] as the `f64` a timeout field holds.
fn timer_ceiling() -> f64 {
    // 2_147_483_647 is far below 2^52, so f64 represents it exactly.
    #[allow(clippy::cast_precision_loss, reason = "exactly representable")]
    let ceiling = MAX_TIMER_DELAY_MS as f64;
    ceiling
}

/// A catalog entry as JSON, merged onto `{ "id": … }`.
fn entry(id: &str, extra: Value) -> Value {
    let Value::Object(mut object) = extra else {
        panic!("extra fields are an object");
    };
    object.insert("id".to_owned(), Value::from(id));
    Value::Object(object)
}

/// A number just past `Number.MAX_SAFE_INTEGER`, as a JSON number.
///
/// `json!(2^53)` would be a `u64` literal; the ladder's refusal hinges on the *JavaScript* bound, so
/// the vector has to be the float a settings file would carry after `JSON.parse`.
fn past_max_safe_integer() -> Value {
    // 2^53 is exactly representable, and the point of the vector is that it is one past the bound.
    #[allow(clippy::cast_precision_loss, reason = "2^53 is exactly representable")]
    let past = MAX_SAFE_INTEGER as f64 + 1.0;
    json!(past)
}

/// Whether two floating-point values are the same number.
///
/// `clippy::float_cmp` refuses `assert_eq!` on `f64` because a computed value rarely lands exactly.
/// These are literals carried through defaults, so exactness is the property under test — and the
/// comparison states that intent rather than silencing the lint at each site.
fn same(left: f64, right: f64) -> bool {
    left.to_bits() == right.to_bits()
}

// =============================================================================================
// Defaults — every field, so a changed default fails here rather than silently in a caller
// =============================================================================================

#[test]
fn an_empty_configuration_takes_every_documented_default() {
    let resolved = accepted(json!({}));
    assert_eq!(resolved.base_url, PUBLIC_BASE_URL);
    assert_eq!(resolved.defaults, RequestDefaults::default());
    assert_eq!(resolved.max_tokens, DEFAULT_MAX_TOKENS);
    assert_eq!(resolved.default_context_window, DEFAULT_CONTEXT_WINDOW);
    assert!(same(
        resolved.stream_idle_timeout_ms,
        DEFAULT_STREAM_IDLE_TIMEOUT_MS
    ));
    assert_eq!(
        resolved.max_request_files_bytes,
        DEFAULT_MAX_REQUEST_FILES_BYTES
    );
    assert_eq!(
        resolved.max_inline_request_image_bytes,
        DEFAULT_MAX_INLINE_REQUEST_IMAGE_BYTES
    );
    assert_eq!(
        resolved.max_images_per_request,
        DEFAULT_MAX_IMAGES_PER_REQUEST as u64
    );
    assert_eq!(
        resolved.image_offload_byte_quantum,
        DEFAULT_IMAGE_OFFLOAD_BYTE_QUANTUM
    );
    assert_eq!(
        resolved.inline_image_offload_byte_quantum,
        DEFAULT_INLINE_IMAGE_OFFLOAD_BYTE_QUANTUM
    );
    assert_eq!(
        resolved.image_offload_count_quantum,
        DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM as u64
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "60_000 is exactly representable"
    )]
    let files_api_timeout_ms = DEFAULT_FILES_API_TIMEOUT_MS as f64;
    assert!(same(resolved.files_api_timeout_ms, files_api_timeout_ms));
    // These four are the values `deepseek_files_gap` publishes as constants; a default that drifted
    // from them would give the resolver and the upload driver two opinions of one bound.
    assert_eq!(
        resolved.file_policy,
        FilePolicy {
            expires_after_seconds: DEFAULT_FILE_EXPIRY_SECONDS,
            refresh_margin_seconds: DEFAULT_FILE_REFRESH_MARGIN_SECONDS,
            quota_cleanup_batch: DEFAULT_FILE_QUOTA_CLEANUP_BATCH,
        }
    );
    assert_eq!(resolved.retry_policy, ResolvedRetryPolicy::default());
    assert_eq!(resolved.models, built_in_models());
}

#[test]
fn the_default_retry_policy_is_normal_with_five_retries() {
    // Upstream `resolveRetryPolicy(undefined, …)`: normal, five retries, the five built-in codes,
    // 500 ms → 10 s with 0.1 jitter.
    let policy = ResolvedRetryPolicy::default();
    assert_eq!(policy.mode, RetryMode::Normal);
    assert_eq!(policy.max_retries, Some(DEFAULT_MAX_RETRIES));
    assert_eq!(
        policy.retryable_codes,
        vec![
            "EMPTY_RESPONSE".to_owned(),
            "RATE_LIMIT".to_owned(),
            "SERVER".to_owned(),
            "TIMEOUT".to_owned(),
            "TRANSPORT".to_owned(),
        ]
    );
    assert!((policy.initial_delay_ms - DEFAULT_INITIAL_DELAY_MS).abs() < f64::EPSILON);
    assert!((policy.max_delay_ms - DEFAULT_MAX_DELAY_MS).abs() < f64::EPSILON);
    assert!((policy.jitter_ratio - DEFAULT_JITTER_RATIO).abs() < f64::EPSILON);
}

#[test]
fn reasoning_effort_has_no_resolve_time_default_but_the_adapter_has_one() {
    // Upstream passes `reasoningEffort` through and the adapter reads `high`. A resolver that
    // invented `High` would make a snapshot claim a setting nobody configured.
    let resolved = accepted(json!({}));
    assert_eq!(resolved.defaults.reasoning_effort, None);
    assert_eq!(
        resolved.defaults.effort_or_default(),
        DEFAULT_REASONING_EFFORT
    );
    assert_eq!(DEFAULT_REASONING_EFFORT, ReasoningEffort::High);
    assert!(resolved.defaults.thinking_enabled());
}

#[test]
fn the_built_in_catalog_is_the_two_official_models() {
    let models = built_in_models();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "deepseek-flash");
    assert_eq!(models[0].name.as_deref(), Some("DeepSeek-V4.1-Flash"));
    assert_eq!(models[0].context_window, Some(1_048_576));
    assert_eq!(
        models[0].input_modalities,
        vec![Modality::Text, Modality::Image]
    );
    assert_eq!(
        models[0].system_prompt_update,
        Some(SystemPromptMode::InHistory)
    );
    assert_eq!(models[0].tool_update, Some(ToolUpdateMode::AdditionOnly));
    assert_eq!(models[0].image_max_bytes, Some(2 * 1024 * 1024));
    assert!(models[0].vision);
    assert_eq!(models[1].id, "deepseek-v4-pro");
    assert_eq!(models[1].input_modalities, vec![Modality::Text]);
    assert_eq!(models[1].system_prompt_update, None);
    assert_eq!(models[1].image_max_bytes, None);
    assert!(!models[1].vision);
}

// =============================================================================================
// baseURL — upstream `runtime.spec.ts` and `api-key/tests/config.spec.ts`
// =============================================================================================

#[test]
fn base_url_falls_back_from_config_to_environment_to_public_api() {
    let configured = json!({ "baseURL": "http://127.0.0.1:9/anthropic" });
    let resolved = resolve_options(
        &options(configured),
        &Environment::with(&[("DEEPSEEK_BASE_URL", "http://env:1")]),
    )
    .expect("explicit config wins over the environment");
    assert_eq!(resolved.base_url, "http://127.0.0.1:9/anthropic");

    let resolved = resolve_options(
        &options(json!({})),
        &Environment::with(&[("DEEPSEEK_BASE_URL", "http://env:1")]),
    )
    .expect("the environment supplies the endpoint");
    assert_eq!(resolved.base_url, "http://env:1");

    let resolved =
        resolve_options(&options(json!({})), &NoEnvironment).expect("the public API is last");
    assert_eq!(resolved.base_url, PUBLIC_BASE_URL);
}

#[test]
fn an_empty_endpoint_from_the_environment_still_resolves_and_then_fails_the_rule() {
    // Upstream's snapshot distinguishes "absent" from "present and empty"; an empty endpoint is a
    // value, so the fallback does not replace it and the base-URL rule refuses it instead.
    let error = resolve_options(
        &options(json!({})),
        &Environment::with(&[("DEEPSEEK_BASE_URL", "")]),
    )
    .expect_err("an empty endpoint is not a usable root");
    assert_eq!(
        error.message(),
        "llm-deepseek: Messages baseURL must be an HTTP(S) root without credentials, query, or fragment"
    );
}

#[test]
fn base_url_must_be_an_http_root() {
    for unusable in [
        "ftp://example.com/anthropic",
        "https://user:pw@example.com/anthropic",
        "https://example.com/anthropic?x=1",
        "https://example.com/anthropic#frag",
        "not a url",
    ] {
        let error = validate_base_url(unusable).expect_err("this is not an HTTP(S) root");
        assert_eq!(
            error.message(),
            "llm-deepseek: Messages baseURL must be an HTTP(S) root without credentials, query, or fragment",
            "{unusable}"
        );
    }
    validate_base_url("http://127.0.0.1:1").expect("plain http is allowed");
    validate_base_url("https://api.deepseek.com/anthropic").expect("the public root is allowed");
}

#[test]
fn a_base_url_may_keep_its_deployment_path() {
    // Upstream leaves the configured path alone; only the listing URL resolves to the origin.
    let resolved = accepted(json!({ "baseURL": "https://gateway.example/openai/v1" }));
    assert_eq!(resolved.base_url, "https://gateway.example/openai/v1");
    assert_eq!(
        model_listing_url(&resolved.base_url).expect("a valid root"),
        "https://gateway.example/models"
    );
}

// =============================================================================================
// thinking × reasoningEffort
// =============================================================================================

#[test]
fn disabled_thinking_admits_only_the_off_effort() {
    assert_eq!(
        refusal(json!({ "thinking": "disabled", "reasoningEffort": "low" })),
        "llm-deepseek: only reasoningEffort \"off\" can be configured when thinking is disabled"
    );
    assert_eq!(
        refusal(json!({ "thinking": "disabled", "reasoningEffort": "high" })),
        "llm-deepseek: only reasoningEffort \"off\" can be configured when thinking is disabled"
    );
    accepted(json!({ "thinking": "disabled", "reasoningEffort": "off" }));
    accepted(json!({ "thinking": "disabled" }));
    accepted(json!({ "thinking": "enabled", "reasoningEffort": "max" }));
}

#[test]
fn a_thinking_policy_and_an_effort_are_both_carried_into_the_defaults() {
    let resolved = accepted(json!({ "thinking": "enabled", "reasoningEffort": "max" }));
    assert_eq!(resolved.defaults.thinking, Some(Thinking::Enabled));
    assert_eq!(
        resolved.defaults.reasoning_effort,
        Some(ReasoningEffort::Max)
    );
    assert!(resolved.defaults.thinking_enabled());

    let resolved = accepted(json!({ "thinking": "disabled" }));
    assert!(!resolved.defaults.thinking_enabled());
    assert_eq!(resolved.defaults.effort_or_default(), ReasoningEffort::High);
}

#[test]
fn an_unknown_effort_or_policy_is_refused_while_parsing() {
    let Value::Object(members) = json!({ "reasoningEffort": "medium" }) else {
        panic!("object");
    };
    let error = Options::from_json(&members).expect_err("medium is not an effort");
    assert_eq!(
        error.message(),
        "llm-deepseek: reasoningEffort must be \"off\", \"low\", \"high\", or \"max\""
    );

    let Value::Object(members) = json!({ "thinking": "sometimes" }) else {
        panic!("object");
    };
    let error = Options::from_json(&members).expect_err("not a thinking policy");
    assert_eq!(
        error.message(),
        "llm-deepseek: thinking must be \"enabled\" or \"disabled\""
    );
}

// =============================================================================================
// The numeric ladder — upstream `runtime.spec.ts` `it.each` tables
// =============================================================================================

#[test]
fn every_field_refuses_the_values_upstream_refuses() {
    // Transcribed from `runtime.spec.ts`'s two `it.each` tables; each row is (field, value, message).
    let rows: [(&str, Value, &str); 28] = [
        (
            "maxImagesPerRequest",
            json!(0),
            "llm-deepseek: maxImagesPerRequest must be a positive safe integer",
        ),
        (
            "maxImagesPerRequest",
            json!(1.5),
            "llm-deepseek: maxImagesPerRequest must be a positive safe integer",
        ),
        (
            "imageOffloadByteQuantum",
            json!(0),
            "llm-deepseek: imageOffloadByteQuantum must be a positive safe integer",
        ),
        (
            "imageOffloadByteQuantum",
            past_max_safe_integer(),
            "llm-deepseek: imageOffloadByteQuantum must be a positive safe integer",
        ),
        (
            "inlineImageOffloadByteQuantum",
            json!(0),
            "llm-deepseek: inlineImageOffloadByteQuantum must be a positive safe integer",
        ),
        (
            "inlineImageOffloadByteQuantum",
            past_max_safe_integer(),
            "llm-deepseek: inlineImageOffloadByteQuantum must be a positive safe integer",
        ),
        (
            "imageOffloadCountQuantum",
            json!(0),
            "llm-deepseek: imageOffloadCountQuantum must be a positive safe integer",
        ),
        (
            "imageOffloadCountQuantum",
            json!(1.5),
            "llm-deepseek: imageOffloadCountQuantum must be a positive safe integer",
        ),
        (
            "fileExpiresAfterSeconds",
            json!(3_599),
            "llm-deepseek: fileExpiresAfterSeconds must be an integer from 3600 through 2592000",
        ),
        (
            "fileExpiresAfterSeconds",
            json!(2_592_001),
            "llm-deepseek: fileExpiresAfterSeconds must be an integer from 3600 through 2592000",
        ),
        (
            "fileRefreshMarginSeconds",
            json!(-1),
            "llm-deepseek: fileRefreshMarginSeconds must be a non-negative integer below fileExpiresAfterSeconds",
        ),
        (
            "fileRefreshMarginSeconds",
            json!(604_800),
            "llm-deepseek: fileRefreshMarginSeconds must be a non-negative integer below fileExpiresAfterSeconds",
        ),
        (
            "fileQuotaCleanupBatch",
            json!(0),
            "llm-deepseek: fileQuotaCleanupBatch must be an integer from 1 through 1000",
        ),
        (
            "fileQuotaCleanupBatch",
            json!(1_001),
            "llm-deepseek: fileQuotaCleanupBatch must be an integer from 1 through 1000",
        ),
        (
            "maxRequestFilesBytes",
            json!(0),
            "llm-deepseek: maxRequestFilesBytes must be a positive safe integer",
        ),
        (
            "maxRequestFilesBytes",
            json!(1.5),
            "llm-deepseek: maxRequestFilesBytes must be a positive safe integer",
        ),
        (
            "maxRequestFilesBytes",
            past_max_safe_integer(),
            "llm-deepseek: maxRequestFilesBytes must be a positive safe integer",
        ),
        (
            "maxInlineRequestImageBytes",
            json!(0),
            "llm-deepseek: maxInlineRequestImageBytes must be a positive safe integer",
        ),
        (
            "maxInlineRequestImageBytes",
            json!(1.5),
            "llm-deepseek: maxInlineRequestImageBytes must be a positive safe integer",
        ),
        (
            "maxInlineRequestImageBytes",
            past_max_safe_integer(),
            "llm-deepseek: maxInlineRequestImageBytes must be a positive safe integer",
        ),
        (
            "defaultContextWindow",
            json!(0),
            "llm-deepseek: defaultContextWindow must be a positive integer",
        ),
        (
            "defaultContextWindow",
            json!(1.5),
            "llm-deepseek: defaultContextWindow must be a positive integer",
        ),
        (
            "maxTokens",
            json!(0),
            "llm-deepseek: maxTokens must be a positive safe integer",
        ),
        (
            "maxTokens",
            json!(1.5),
            "llm-deepseek: maxTokens must be a positive safe integer",
        ),
        (
            "maxTokens",
            past_max_safe_integer(),
            "llm-deepseek: maxTokens must be a positive safe integer",
        ),
        ("streamIdleTimeoutMs", json!(0), ""),
        ("filesApiTimeoutMs", json!(MAX_TIMER_DELAY_MS + 1), ""),
        (
            "fileExpiresAfterSeconds",
            json!(3_600.5),
            "llm-deepseek: fileExpiresAfterSeconds must be an integer from 3600 through 2592000",
        ),
    ];

    for (field, value, message) in rows {
        let actual = refusal(json!({ field: value.clone() }));
        if message.is_empty() {
            // The two timeout messages embed the ceiling; assert the stable prefix and the ceiling.
            assert!(
                actual.starts_with(&format!(
                    "llm-deepseek: {field} must be a positive finite number no greater than "
                )) && actual.ends_with(&MAX_TIMER_DELAY_MS.to_string()),
                "{field}={value}: {actual}"
            );
        } else {
            assert_eq!(actual, message, "{field}={value}");
        }
    }
}

#[test]
fn a_timeout_above_the_timer_ceiling_is_refused_with_the_ceiling_in_the_message() {
    assert_eq!(
        refusal(json!({ "streamIdleTimeoutMs": MAX_TIMER_DELAY_MS + 1 })),
        format!(
            "llm-deepseek: streamIdleTimeoutMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )
    );
    assert_eq!(
        refusal(json!({ "filesApiTimeoutMs": 0 })),
        format!(
            "llm-deepseek: filesApiTimeoutMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )
    );
    // The ceiling itself is accepted, which is what makes the bound inclusive.
    accepted(json!({ "streamIdleTimeoutMs": MAX_TIMER_DELAY_MS }));
}

// =============================================================================================
// The three cross-field quanta — the constraints the report called out
// =============================================================================================

#[test]
fn an_offload_quantum_may_not_exceed_the_bound_it_repairs() {
    // Upstream `runtime.spec.ts`: 'rejects offload quanta larger than their request bounds'.
    assert_eq!(
        refusal(json!({ "maxRequestFilesBytes": 10, "imageOffloadByteQuantum": 11 })),
        "llm-deepseek: imageOffloadByteQuantum must not exceed maxRequestFilesBytes"
    );
    assert_eq!(
        refusal(json!({ "maxInlineRequestImageBytes": 10, "inlineImageOffloadByteQuantum": 11 })),
        "llm-deepseek: inlineImageOffloadByteQuantum must not exceed maxInlineRequestImageBytes"
    );
    assert_eq!(
        refusal(json!({ "maxImagesPerRequest": 10, "imageOffloadCountQuantum": 11 })),
        "llm-deepseek: imageOffloadCountQuantum must not exceed maxImagesPerRequest"
    );
}

#[test]
fn a_quantum_equal_to_its_bound_is_accepted() {
    // The comparison is `>` upstream, so equality is legal and repairs in one step.
    let resolved = accepted(json!({
        "maxRequestFilesBytes": 10,
        "imageOffloadByteQuantum": 10,
        "maxInlineRequestImageBytes": 10,
        "inlineImageOffloadByteQuantum": 10,
        "maxImagesPerRequest": 10,
        "imageOffloadCountQuantum": 10,
    }));
    assert_eq!(resolved.image_offload_byte_quantum, 10);
    assert_eq!(resolved.max_request_files_bytes, 10);
    assert_eq!(resolved.image_offload_count_quantum, 10);
}

#[test]
fn the_bound_is_judged_before_its_quantum() {
    // Both are wrong; the ladder names the field it reaches first, which is the bound.
    assert_eq!(
        refusal(json!({ "maxRequestFilesBytes": 0, "imageOffloadByteQuantum": 0 })),
        "llm-deepseek: maxRequestFilesBytes must be a positive safe integer"
    );
}

#[test]
fn a_refresh_margin_must_stay_below_the_lifetime() {
    assert_eq!(
        refusal(json!({ "fileExpiresAfterSeconds": 3_600, "fileRefreshMarginSeconds": 3_600 })),
        "llm-deepseek: fileRefreshMarginSeconds must be a non-negative integer below fileExpiresAfterSeconds"
    );
    let resolved =
        accepted(json!({ "fileExpiresAfterSeconds": 3_600, "fileRefreshMarginSeconds": 3_599 }));
    assert_eq!(resolved.file_policy.expires_after_seconds, 3_600);
    assert_eq!(resolved.file_policy.refresh_margin_seconds, 3_599);
    // Upstream's schema admits zero here and the resolver keeps it: a deployment may refresh on
    // every access.
    accepted(json!({ "fileRefreshMarginSeconds": 0 }));
}

// =============================================================================================
// The catalog — upstream `runtime.spec.ts`, `built-in-catalog.spec.ts`
// =============================================================================================

#[test]
fn the_built_in_catalog_is_recognized_however_it_was_supplied() {
    // Upstream `built-in-catalog.spec.ts`: 'recognizes the built-in catalog however it was supplied'.
    assert!(is_built_in_catalog(accepted(json!({})).models.as_slice()));
    assert!(is_built_in_catalog(&built_in_models()));
    // A catalog restated by hand from JSON resolves to the same normalized entries, so it is still
    // built-in: the comparison is structural, exactly as upstream's `deepEqualJson` is.
    let restated = accepted(json!({ "models": [
        entry("deepseek-flash", json!({
            "name": "DeepSeek-V4.1-Flash",
            "contextWindow": 1_048_576,
            "inputModalities": ["text", "image"],
            "systemPromptUpdate": "in-history",
            "toolUpdate": "addition-only",
        })),
        entry("deepseek-v4-pro", json!({
            "name": "DeepSeek-V4-Pro",
            "contextWindow": 1_048_576,
        })),
    ] }));
    assert!(is_built_in_catalog(restated.models.as_slice()));
}

#[test]
fn any_other_catalog_is_customized() {
    // Upstream: 'treats any other catalog as customized'.
    let [flash, pro] = built_in_models().try_into().expect("two entries");
    assert!(!is_built_in_catalog(std::slice::from_ref(&flash)));
    let mut widened = flash.clone();
    widened.context_window = Some(524_288);
    assert!(!is_built_in_catalog(&[widened, pro.clone()]));
    assert!(!is_built_in_catalog(&[pro, flash]));
    assert!(!is_built_in_catalog(&[]));
}

#[test]
fn an_empty_catalog_is_legal() {
    // Upstream `runtime.spec.ts`: 'allows an explicit empty model catalog'.
    let resolved = accepted(json!({ "models": [] }));
    assert!(resolved.models.is_empty());
    assert!(!is_built_in_catalog(&resolved.models));
}

#[test]
fn every_invalid_catalog_entry_is_refused() {
    // Upstream `runtime.spec.ts` `invalidModels` and `invalidProgrammaticModalities`, plus the
    // per-field tables; the typed path covers what the JSON path cannot express.
    let rows: [(Value, &str); 16] = [
        (
            json!([entry("", json!({}))]),
            "llm-deepseek: catalog model ids must be non-empty",
        ),
        (
            json!([entry("m", json!({ "name": "" }))]),
            "llm-deepseek: catalog model \"m\" has an empty name",
        ),
        (
            json!([entry("m", json!({ "contextWindow": 0 }))]),
            "llm-deepseek: catalog model \"m\" contextWindow must be a positive integer",
        ),
        (
            json!([entry("m", json!({ "contextWindow": 1.5 }))]),
            "llm-deepseek: catalog model \"m\" contextWindow must be a positive integer",
        ),
        (
            json!([entry("m", json!({ "maxTokens": 0 }))]),
            "llm-deepseek: catalog model \"m\" maxTokens must be a positive integer",
        ),
        (
            json!([entry("m", json!({ "inputModalities": [] }))]),
            "llm-deepseek: catalog model \"m\" inputModalities must not be empty",
        ),
        (
            json!([entry("m", json!({ "inputModalities": ["text", "text"] }))]),
            "llm-deepseek: catalog model \"m\" inputModalities must not contain duplicates",
        ),
        (
            json!([entry("m", json!({ "inputModalities": ["audio"] }))]),
            "llm-deepseek: catalog model \"m\" inputModalities must contain only \"text\" and \"image\"",
        ),
        (
            json!([entry("m", json!({})), entry("m", json!({}))]),
            "llm-deepseek: duplicate catalog model \"m\"",
        ),
        (
            json!([entry(
                "vision",
                json!({ "inputModalities": ["image"], "imageDetail": "low" })
            )]),
            "llm-deepseek: catalog model imageDetail is no longer supported; use imagePixelBudget",
        ),
        (
            json!([entry(
                "text-only",
                json!({ "inputModalities": ["text"], "imagePixelBudget": 1 })
            )]),
            "llm-deepseek: text-only catalog model \"text-only\" cannot declare image request limits",
        ),
        (
            json!([entry(
                "vision",
                json!({ "inputModalities": ["image"], "imagePixelBudget": 0 })
            )]),
            "llm-deepseek: catalog model \"vision\" imagePixelBudget must be \"low\" or a positive safe integer",
        ),
        (
            json!([entry(
                "vision",
                json!({ "inputModalities": ["image"], "imagePixelBudget": "auto" })
            )]),
            "llm-deepseek: catalog model \"vision\" imagePixelBudget must be \"low\" or a positive safe integer",
        ),
        (
            json!([entry(
                "vision",
                json!({ "inputModalities": ["image"], "imageMaxBytes": 0 })
            )]),
            "llm-deepseek: catalog model \"vision\" imageMaxBytes must be a positive safe integer",
        ),
        (
            json!([entry("bogus", json!({ "systemPromptUpdate": "leading" }))]),
            "llm-deepseek: catalog model \"bogus\" systemPromptUpdate must be \"in-history\" when present",
        ),
        (
            json!([entry("m", json!({ "unknownField": 1 }))]),
            "llm-deepseek: catalog model: unknown key \"unknownField\"",
        ),
    ];
    for (models, message) in rows {
        assert_eq!(
            refusal(json!({ "models": models.clone() })),
            message,
            "{models}"
        );
    }
}

#[test]
fn a_text_only_entry_drops_the_image_limits_and_a_vision_entry_gains_a_byte_target() {
    // Upstream `resolveModels`: the normalization is what makes a vision entry carry
    // `imageMaxBytes: DEFAULT_REQUEST_IMAGE_MAX_BYTES` while a text-only entry carries nothing.
    let resolved = accepted(json!({ "models": [
        entry("vision", json!({ "inputModalities": ["text", "image"] })),
        entry("plain", json!({})),
        entry("budgeted", json!({ "inputModalities": ["text", "image"], "imagePixelBudget": "low", "imageMaxBytes": 7 })),
    ] }));
    assert_eq!(resolved.models[0].image_max_bytes, Some(2 * 1024 * 1024));
    assert_eq!(resolved.models[0].image_pixel_budget, None);
    assert!(resolved.models[0].vision);
    assert_eq!(resolved.models[1].image_max_bytes, None);
    assert_eq!(resolved.models[1].input_modalities, vec![Modality::Text]);
    assert_eq!(
        resolved.models[2].image_pixel_budget,
        Some(PixelBudget::Low)
    );
    assert_eq!(resolved.models[2].image_max_bytes, Some(7));
}

#[test]
fn a_typed_entry_is_normalized_the_same_way_as_a_parsed_one() {
    // The typed path skips JSON entirely; `resolve_models` must still inject and drop.
    let mut vision = CatalogSpec::new("vision");
    vision.input_modalities = Some(vec![Modality::Text, Modality::Image]);
    let model = resolve_catalog_spec(&vision, false).expect("valid");
    assert_eq!(model.image_max_bytes, Some(2 * 1024 * 1024));
    assert!(model.vision);

    let typed = vec![CatalogModel {
        id: "plain".to_owned(),
        name: None,
        input_modalities: vec![Modality::Text],
        context_window: None,
        max_output_tokens: None,
        system_prompt_update: None,
        tool_update: None,
        image_pixel_budget: None,
        image_max_bytes: None,
        vision: false,
    }];
    let resolved = resolve_models(Some(&typed)).expect("valid");
    assert_eq!(resolved, typed);
    assert_eq!(
        resolve_models(None).expect("the default"),
        built_in_models()
    );
}

#[test]
fn a_pixel_budget_may_be_the_low_preset_or_a_positive_cap() {
    let resolved = accepted(json!({ "models": [
        entry("low", json!({ "inputModalities": ["image"], "imagePixelBudget": "low" })),
        entry("cap", json!({ "inputModalities": ["image"], "imagePixelBudget": 262_144 })),
    ] }));
    assert_eq!(
        resolved.models[0].image_pixel_budget,
        Some(PixelBudget::Low)
    );
    assert_eq!(
        resolved.models[1].image_pixel_budget,
        Some(PixelBudget::Pixels(262_144))
    );
}

#[test]
fn a_leading_only_update_mode_is_a_runtime_mode_the_schema_refuses() {
    // Upstream carries `leading-only` at runtime (`deepseek_files_gap` models it) but configuration
    // admits only `in-history` for a prompt update and `in-history`/`addition-only` for tools.
    assert_eq!(
        refusal(json!({ "models": [entry("m", json!({ "toolUpdate": "leading-only" }))] })),
        "llm-deepseek: catalog model \"m\" toolUpdate must be \"in-history\" or \"addition-only\" when present"
    );
    // Reaching the same state through the typed path is refused identically, which is what keeps
    // the two paths one rule.
    let mut spec = CatalogSpec::new("m");
    spec.system_prompt_update = Some(SystemPromptMode::LeadingOnly);
    let error = resolve_catalog_spec(&spec, false).expect_err("leading-only is refused");
    assert_eq!(
        error.message(),
        "llm-deepseek: catalog model \"m\" systemPromptUpdate must be \"in-history\" when present"
    );
}

// =============================================================================================
// The retired `protocol` member and unknown members
// =============================================================================================

#[test]
fn the_retired_protocol_member_is_refused_first() {
    // Upstream checks `Object.hasOwn(config, 'protocol')` before every other rule, and the message
    // tells the author what to do instead.
    let Value::Object(members) = json!({ "protocol": "openai", "maxTokens": 0 }) else {
        panic!("object");
    };
    let error = Options::from_json(&members).expect_err("protocol is not configurable");
    assert_eq!(
        error.message(),
        "llm-deepseek: protocol is not configurable; remove it and use a Messages-compatible baseURL"
    );
}

#[test]
fn an_unknown_member_is_refused_rather_than_ignored() {
    let Value::Object(members) = json!({ "maxTokens": 10, "mystery": 1 }) else {
        panic!("object");
    };
    let error = Options::from_json(&members).expect_err("mystery is not a protocol field");
    assert_eq!(error.message(), "llm-deepseek: unknown key \"mystery\"");
}

// =============================================================================================
// retryPolicy — upstream `retry-policy.ts`
// =============================================================================================

#[test]
fn a_normal_policy_resolves_its_declared_members() {
    let resolved = accepted(json!({ "retryPolicy": {
        "mode": "normal",
        "maxRetries": 2,
        "retryableCodes": ["RATE_LIMIT", "SERVER"],
        "backoff": { "initialDelayMs": 100, "maxDelayMs": 2_000, "jitterRatio": 0.5 },
    } }));
    assert_eq!(resolved.retry_policy.mode, RetryMode::Normal);
    assert_eq!(resolved.retry_policy.max_retries, Some(2));
    assert_eq!(
        resolved.retry_policy.retryable_codes,
        vec!["RATE_LIMIT", "SERVER"]
    );
    assert!((resolved.retry_policy.initial_delay_ms - 100.0).abs() < f64::EPSILON);
    assert!((resolved.retry_policy.max_delay_ms - 2_000.0).abs() < f64::EPSILON);
    assert!((resolved.retry_policy.jitter_ratio - 0.5).abs() < f64::EPSILON);
}

#[test]
fn an_always_policy_has_no_retry_bound_and_ignores_the_normal_members() {
    // Upstream's `ALWAYS_POLICY_KEYS` equals the normal set, so a mode switch does not strand the
    // old values; they are accepted and ignored.
    let resolved = accepted(json!({ "retryPolicy": {
        "mode": "always",
        "maxRetries": 9,
        "retryableCodes": ["SERVER"],
        "backoff": { "initialDelayMs": 250 },
    } }));
    assert_eq!(resolved.retry_policy.mode, RetryMode::Always);
    assert_eq!(resolved.retry_policy.max_retries, None);
    assert!(resolved.retry_policy.retryable_codes.is_empty());
    assert!((resolved.retry_policy.initial_delay_ms - 250.0).abs() < f64::EPSILON);
    // The untouched members still take their defaults.
    assert!((resolved.retry_policy.max_delay_ms - DEFAULT_MAX_DELAY_MS).abs() < f64::EPSILON);
}

#[test]
fn every_unusable_retry_policy_is_refused_with_its_path() {
    let rows: [(Value, &str); 9] = [
        (
            json!({ "mode": "sometimes" }),
            "llm-deepseek: retryPolicy.mode must be \"normal\" or \"always\"",
        ),
        (
            json!({ "mode": "normal", "maxRetries": -1 }),
            "llm-deepseek: retryPolicy.maxRetries must be a non-negative safe integer",
        ),
        (
            json!({ "mode": "normal", "maxRetries": 1.5 }),
            "llm-deepseek: retryPolicy.maxRetries must be a non-negative safe integer",
        ),
        (
            json!({ "mode": "normal", "retryableCodes": [] }),
            "llm-deepseek: retryPolicy.retryableCodes must not be empty",
        ),
        (
            json!({ "mode": "normal", "retryableCodes": ["SERVER", "SERVER"] }),
            "llm-deepseek: retryPolicy.retryableCodes must not contain duplicates",
        ),
        (
            json!({ "mode": "normal", "retryableCodes": [""] }),
            "llm-deepseek: retryPolicy.retryableCodes must contain only non-empty strings",
        ),
        (
            json!({ "mode": "normal", "backoff": { "initialDelayMs": 5_000, "maxDelayMs": 1_000 } }),
            "llm-deepseek: retryPolicy.backoff.initialDelayMs must be less than or equal to maxDelayMs",
        ),
        (
            json!({ "mode": "normal", "backoff": { "jitterRatio": 1.5 } }),
            "llm-deepseek: retryPolicy.backoff.jitterRatio must be between 0 and 1",
        ),
        (
            json!({ "mode": "normal", "mystery": 1 }),
            "llm-deepseek: retryPolicy: unknown key \"mystery\"",
        ),
    ];
    for (policy, message) in rows {
        assert_eq!(
            refusal(json!({ "retryPolicy": policy.clone() })),
            message,
            "{policy}"
        );
    }
}

#[test]
fn a_backoff_delay_above_the_timer_ceiling_is_refused() {
    assert_eq!(
        refusal(json!({ "retryPolicy": { "mode": "always", "backoff": {
            "maxDelayMs": MAX_TIMER_DELAY_MS + 1,
        } } })),
        format!(
            "llm-deepseek: retryPolicy.backoff.maxDelayMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        )
    );
}

#[test]
fn a_typed_policy_resolves_the_same_way() {
    let typed = RetryPolicyConfig::Normal {
        max_retries: None,
        retryable_codes: None,
        backoff: Some(BackoffConfig {
            initial_delay_ms: None,
            max_delay_ms: Some(2_000.0),
            jitter_ratio: Some(0.0),
        }),
    };
    let resolved = resolve_retry_policy(Some(&typed), "llm-deepseek: retryPolicy").expect("valid");
    assert_eq!(resolved.max_retries, Some(DEFAULT_MAX_RETRIES));
    assert_eq!(
        resolved.retryable_codes,
        ResolvedRetryPolicy::default().retryable_codes
    );
    assert!((resolved.jitter_ratio - 0.0).abs() < f64::EPSILON);
    assert_eq!(
        resolve_retry_policy(None, "llm-deepseek: retryPolicy").expect("the default"),
        ResolvedRetryPolicy::default()
    );
}

// =============================================================================================
// Listing timing — upstream `llm-deepseek-account/tests/config.spec.ts`
// =============================================================================================

#[test]
fn listing_timing_takes_the_documented_defaults() {
    let timing = resolve_listing_timing(&ListingOptions::default(), "llm-deepseek-account")
        .expect("the defaults are valid");
    assert!((timing.timeout_ms - DEFAULT_MODELS_TIMEOUT_MS).abs() < f64::EPSILON);
    assert_eq!(timing.max_age_ms, DEFAULT_MODELS_MAX_AGE_MS);
}

#[test]
fn listing_timing_refuses_the_values_upstream_refuses() {
    // Upstream account `config.spec.ts`'s `it.each` table, then the API-key route's own two rows.
    let owner = "llm-deepseek-account";
    let rows: [(ListingOptions, &str); 4] = [
        (
            ListingOptions {
                models_timeout_ms: Some(0.0),
                models_max_age_ms: None,
            },
            "llm-deepseek-account: modelsTimeoutMs must be a positive finite number",
        ),
        (
            ListingOptions {
                models_timeout_ms: Some(timer_ceiling() + 1.0),
                models_max_age_ms: None,
            },
            "llm-deepseek-account: modelsTimeoutMs must be a positive finite number",
        ),
        (
            ListingOptions {
                models_timeout_ms: None,
                models_max_age_ms: Some(-1.0),
            },
            "llm-deepseek-account: modelsMaxAgeMs must be a non-negative safe integer",
        ),
        (
            ListingOptions {
                models_timeout_ms: None,
                models_max_age_ms: Some(1.5),
            },
            "llm-deepseek-account: modelsMaxAgeMs must be a non-negative safe integer",
        ),
    ];
    for (options, prefix) in rows {
        let error = resolve_listing_timing(&options, owner).expect_err("outside its bounds");
        assert!(
            error.message().starts_with(prefix),
            "{options:?}: {}",
            error.message()
        );
    }
}

#[test]
fn the_owner_prefix_names_the_route_that_owns_the_setting() {
    let unusable = ListingOptions {
        models_timeout_ms: Some(0.0),
        models_max_age_ms: None,
    };
    assert!(
        resolve_listing_timing(&unusable, "llm-deepseek-api-key")
            .expect_err("zero is not a deadline")
            .message()
            .starts_with("llm-deepseek-api-key: modelsTimeoutMs")
    );
    // A zero age is legal: it refreshes on every access and disables automatic refresh.
    let timing = resolve_listing_timing(
        &ListingOptions {
            models_timeout_ms: None,
            models_max_age_ms: Some(0.0),
        },
        "llm-deepseek-account",
    )
    .expect("zero is a legal age");
    assert_eq!(timing.max_age_ms, 0);
}

// =============================================================================================
// The account route — upstream `llm-deepseek-account/tests/config.spec.ts`
// =============================================================================================

/// Parses an account settings object.
fn account_refusal(value: Value) -> String {
    let Value::Object(members) = value else {
        panic!("an account settings object is an object");
    };
    match AccountOptions::from_json(&members) {
        Err(error) => error.message().to_owned(),
        Ok(parsed) => match resolve_account_options(&parsed, &Environment::empty()) {
            Err(error) => error.message().to_owned(),
            Ok(_) => panic!("expected a refusal"),
        },
    }
}

#[test]
fn the_account_route_derives_the_listing_endpoint_from_the_messages_origin() {
    // Upstream: `resolveAccountOptions({}).catalog` is the official listing with the defaults.
    let Value::Object(members) = json!({}) else {
        panic!("object");
    };
    let parsed = AccountOptions::from_json(&members).expect("empty is valid");
    let resolved = resolve_account_options(&parsed, &Environment::empty()).expect("valid");
    assert_eq!(resolved.catalog.url, "https://api.deepseek.com/models");
    assert!((resolved.catalog.timeout_ms - 5_000.0).abs() < f64::EPSILON);
    assert_eq!(resolved.catalog.max_age_ms, 600_000);

    let Value::Object(members) = json!({ "baseURL": "http://127.0.0.1:9/anthropic" }) else {
        panic!("object");
    };
    let parsed = AccountOptions::from_json(&members).expect("valid");
    let resolved = resolve_account_options(&parsed, &Environment::empty()).expect("valid");
    assert_eq!(resolved.catalog.url, "http://127.0.0.1:9/models");
}

#[test]
fn the_account_route_refuses_the_retired_models_setting() {
    // Upstream: 'passes a retired models setting through to the resolver, which rejects it'.
    let message = account_refusal(json!({ "models": [] }));
    assert!(
        message.starts_with("llm-deepseek-account: models is no longer configurable"),
        "{message}"
    );
    assert!(message.contains("modelOverrides.<model id>"), "{message}");
}

#[test]
fn every_account_refusal_matches_its_upstream_message() {
    let rows: [(Value, &str); 5] = [
        (
            json!({ "modelsTimeoutMs": 0 }),
            "llm-deepseek-account: modelsTimeoutMs must be a positive finite number",
        ),
        (
            json!({ "modelsMaxAgeMs": -1 }),
            "llm-deepseek-account: modelsMaxAgeMs must be a non-negative safe integer",
        ),
        (
            json!({ "modelOverrides": { "": {} } }),
            "llm-deepseek-account: modelOverrides keys must be non-empty model ids",
        ),
        (
            json!({ "modelOverrides": { "deepseek-flash": { "id": "other" } } }),
            "llm-deepseek-account: modelOverrides.deepseek-flash sets \"id\", which is its key",
        ),
        (
            json!({ "modelOverrides": { "deepseek-flash": { "contextWindow": 0 } } }),
            "llm-deepseek-account: modelOverrides.deepseek-flash is invalid: llm-deepseek: catalog model \"deepseek-flash\" contextWindow must be a positive integer",
        ),
    ];
    for (settings, expected) in rows {
        let actual = account_refusal(settings.clone());
        assert!(
            actual.starts_with(expected) || actual == expected,
            "{settings}: {actual}"
        );
    }
}

#[test]
fn an_override_on_a_text_only_model_cannot_declare_image_limits() {
    // The stand-in entry is image-capable, so the refusal comes from the override's own modality
    // list — which is exactly what upstream's stand-in is for.
    assert_eq!(
        account_refusal(json!({ "modelOverrides": {
            "deepseek-flash": { "inputModalities": ["text"], "imageMaxBytes": 1 },
        } })),
        "llm-deepseek-account: modelOverrides.deepseek-flash is invalid: llm-deepseek: text-only catalog model \"deepseek-flash\" cannot declare image request limits"
    );
}

#[test]
fn an_override_keeps_the_fields_it_declares_and_drops_inherited_empty_lists() {
    // Upstream: 'keeps the fields an override declares and drops inherited empty lists'.
    let Value::Object(members) = json!({ "modelOverrides": {
        "deepseek-flash": { "contextWindow": 524_288, "imagePixelBudget": "low", "inputModalities": [] },
    } }) else {
        panic!("object");
    };
    let parsed = AccountOptions::from_json(&members).expect("valid");
    let resolved = resolve_account_options(&parsed, &Environment::empty()).expect("valid");
    let override_fields = resolved
        .model_overrides
        .get("deepseek-flash")
        .expect("the declared override");
    assert_eq!(override_fields.context_window, Some(524_288));
    assert_eq!(override_fields.image_pixel_budget, Some(PixelBudget::Low));
    // The empty list meant "inherit", so it is absent rather than an empty declaration.
    assert_eq!(override_fields.input_modalities, None);
    assert_eq!(override_fields.name, None);
    assert_eq!(override_fields.image_max_bytes, None);
}

#[test]
fn an_override_that_declares_nothing_is_valid_and_empty() {
    // Upstream's `modelOverrides: {}` default and a bare entry both resolve; the entry is an empty
    // `ModelOverride`, which layers nothing.
    let empty = resolve_override("m", &Map::new()).expect("an empty override is legal");
    assert_eq!(empty, ModelOverride::default());
}

#[test]
fn an_unknown_override_member_is_refused() {
    let Value::Object(members) = json!({ "modelOverrides": { "m": { "mystery": 1 } } }) else {
        panic!("object");
    };
    let parsed = AccountOptions::from_json(&members).expect("the object parses");
    let error = resolve_account_options(&parsed, &Environment::empty())
        .expect_err("mystery is not an override field");
    assert_eq!(
        error.message(),
        "llm-deepseek-account: modelOverrides.m is invalid: llm-deepseek-account: modelOverrides: unknown key \"mystery\""
    );
}

#[test]
fn the_account_route_never_accepts_a_catalog() {
    // A retired `models` member parses to a refusal, so a resolved account generation cannot carry
    // one — the struct has no field for it.
    let Value::Object(members) = json!({ "modelsTimeoutMs": 7 }) else {
        panic!("object");
    };
    let parsed = AccountOptions::from_json(&members).expect("valid");
    assert_eq!(parsed.protocol.models, None);
    assert!((parsed.listing.models_timeout_ms.expect("declared") - 7.0).abs() < f64::EPSILON);
}

// =============================================================================================
// The API-key route — upstream `llm-deepseek-api-key/tests/{config,listing}.spec.ts`
// =============================================================================================

/// Resolves an API-key settings object.
fn api_key_resolved(value: Value) -> ApiKeyResolved {
    let Value::Object(members) = value else {
        panic!("object");
    };
    let parsed = ApiKeyOptions::from_json(&members).expect("these settings parse");
    resolve_api_key_options(&parsed, &Environment::empty()).expect("these settings resolve")
}

/// Resolves an API-key settings object and returns the refusal.
fn api_key_refusal(value: Value) -> String {
    let Value::Object(members) = value else {
        panic!("object");
    };
    match ApiKeyOptions::from_json(&members) {
        Err(error) => error.message().to_owned(),
        Ok(parsed) => match resolve_api_key_options(&parsed, &Environment::empty()) {
            Err(error) => error.message().to_owned(),
            Ok(_) => panic!("expected a refusal"),
        },
    }
}

#[test]
fn the_api_key_route_follows_the_official_listing_for_the_built_in_catalog() {
    // Upstream `config.spec.ts`: 'follows the official listing while the catalog is the built-in one'.
    let listing = api_key_resolved(json!({}))
        .listing
        .expect("the default route follows the official listing");
    assert_eq!(listing.url, "https://api.deepseek.com/models");
    assert!((listing.timeout_ms - 5_000.0).abs() < f64::EPSILON);
    assert_eq!(listing.max_age_ms, 600_000);

    // A deployment path on the official host keeps the same origin.
    let listing = api_key_resolved(json!({
        "baseURL": "https://api.deepseek.com/anthropic/v1",
        "modelsTimeoutMs": 7,
        "modelsMaxAgeMs": 0,
    }))
    .listing
    .expect("still the official origin");
    assert_eq!(listing.url, "https://api.deepseek.com/models");
    assert!((listing.timeout_ms - 7.0).abs() < f64::EPSILON);
    assert_eq!(listing.max_age_ms, 0);
}

#[test]
fn the_api_key_route_keeps_its_own_catalog_for_any_other_endpoint_or_catalog() {
    // Upstream: 'keeps its own catalog for %j' — a custom catalog, an empty one, a gateway host, and
    // plain http on the official host all withhold the listing.
    for settings in [
        json!({ "models": [entry("custom", json!({}))] }),
        json!({ "models": [] }),
        json!({ "baseURL": "https://gateway.example/anthropic" }),
        json!({ "baseURL": "http://api.deepseek.com/anthropic" }),
    ] {
        let resolved = api_key_resolved(settings.clone());
        assert!(
            resolved.listing.is_none(),
            "{settings} should not follow the listing"
        );
    }
}

#[test]
fn the_api_key_route_refuses_its_own_two_timing_rows() {
    // Upstream: the API-key route's `it.each`, where the owner prefix is the route's own name.
    for (settings, prefix) in [
        (
            json!({ "models": [entry("custom", json!({}))], "modelsTimeoutMs": 0 }),
            "llm-deepseek-api-key: modelsTimeoutMs must be a positive finite number",
        ),
        (
            json!({ "models": [entry("custom", json!({}))], "modelsMaxAgeMs": 1.5 }),
            "llm-deepseek-api-key: modelsMaxAgeMs must be a non-negative safe integer",
        ),
    ] {
        let message = api_key_refusal(settings.clone());
        assert!(message.starts_with(prefix), "{settings}: {message}");
    }
}

#[test]
fn the_api_key_route_defaults_the_credential_reference() {
    let resolved = api_key_resolved(json!({}));
    assert_eq!(resolved.api_key_env, DEFAULT_API_KEY_ENV);
    assert!(is_credential_ref_name(&resolved.api_key_env));

    let resolved = api_key_resolved(json!({ "apiKeyEnv": "MY_DEEPSEEK_KEY" }));
    assert_eq!(resolved.api_key_env, "MY_DEEPSEEK_KEY");
}

#[test]
fn an_unusable_credential_reference_is_refused() {
    for unusable in ["", "1KEY", "MY-KEY", "MY KEY"] {
        let message = api_key_refusal(json!({ "apiKeyEnv": unusable }));
        assert!(
            message.starts_with("credential ref \""),
            "{unusable}: {message}"
        );
    }
}

// =============================================================================================
// Origins — the whole point of the API-key rule
// =============================================================================================

#[test]
fn an_origin_ignores_the_path_and_the_default_port_and_the_host_case() {
    let tables: [(&str, &str); 5] = [
        (
            "https://api.deepseek.com/anthropic",
            "https://api.deepseek.com",
        ),
        (
            "https://api.deepseek.com:443/anthropic/v1",
            "https://api.deepseek.com",
        ),
        (
            "https://API.DeepSeek.com/anthropic",
            "https://api.deepseek.com",
        ),
        ("http://127.0.0.1:9/anthropic", "http://127.0.0.1:9"),
        ("http://127.0.0.1:80", "http://127.0.0.1"),
    ];
    for (base, expected) in tables {
        assert_eq!(
            origin_of(base).expect("a valid root").as_str(),
            expected,
            "{base}"
        );
    }
}

#[test]
fn the_listing_url_is_always_at_the_origin_of_the_messages_root() {
    // Upstream `listing.ts`: `new URL('/models', new URL(baseURL).origin)`.
    assert_eq!(
        model_listing_url("https://api.deepseek.com/anthropic").expect("valid"),
        "https://api.deepseek.com/models"
    );
    assert_eq!(
        model_listing_url("http://127.0.0.1:9/anthropic").expect("valid"),
        "http://127.0.0.1:9/models"
    );
    assert!(model_listing_url("ftp://x").is_err());
}

// =============================================================================================
// The default file policy is the one the Files module publishes
// =============================================================================================

#[test]
fn the_resolved_file_policy_is_the_files_module_s_default() {
    // The two modules must not drift: `llm::deepseek_files::FilePolicy::default()` is what the upload
    // driver falls back to when nothing was configured, and this resolver's defaults must equal it.
    let resolved = accepted(json!({}));
    assert_eq!(resolved.file_policy, FilePolicy::default());
    assert!(resolved.file_policy.expiry_is_supported());
    // The refresh window is one hour of a seven-day lifetime, which the resolver's cross-field rule
    // (`margin < lifetime`) and upstream's `FilePolicy` both accept.
    assert!(
        resolved.file_policy.refresh_margin_seconds < resolved.file_policy.expires_after_seconds
    );
    assert_eq!(MAX_SAFE_INTEGER, 9_007_199_254_740_991);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod llm {
    pub use super::deepseek_options_gap as deepseek_options;
}
