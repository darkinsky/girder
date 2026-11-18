//! The `apply` assembly of the pi-ai adapter: profile resolution, serviceability, and the pure half of
//! plugin registration.
//!
//! Upstream: `packages/llm/llm-pi-ai/src/config.ts` (551 lines) and `src/index.ts` (346 lines) — 897
//! lines that an earlier round reported as "the missing `apply` wiring". This module checks that
//! report against the two files, separates what a plugin host owns from what is arithmetic, and
//! implements everything in the arithmetic half.
//!
//! # `config.ts`, capability by capability
//!
//! | Upstream | Lines | What it does | Portability |
//! | --- | --- | --- | --- |
//! | `DEFAULT_*` constants | 47–81 | eight adapter-owned defaults | **pure data** |
//! | `PiAiProviderProfile` / `ResolvedPiAiProviderProfile` / `Config` / `Options` | 104–261 | the profile shape and its resolved form | **pure types** |
//! | `thinkingBudgets`, `chatTemplateKwarg`, `compatProfile`, `reasoningEfforts`, `modelFields`, `modelProfile`, `modelOverride`, `profile`, `Config` | 263–385 | `schemastery` schemas: defaults, steps, and `volatile()` wrappers | needs `schemastery` |
//! | `assertServiceable` | 395–399 | narrow a config to the profiles that *changed*, re-resolve those | **pure** |
//! | `rejectRemovedFields` | 402–417 | refuse `provider` / `maxRetries` / `maxRetryDelayMs` with the replacement named | **pure** |
//! | `assertValidHeaders` | 420–431 | refuse a header `Fetch` cannot put on a request | **pure** |
//! | `resolveProfiles` | 441–551 | default every scalar, materialize each route's catalog, build its provider | **pure given an injected catalog** |
//!
//! The `schemastery` row is the only one that is genuinely not portable, and what it contributes is
//! *defaults and type checks* — both of which a Rust type carries for free, except for the handful of
//! bounds the schema expresses as `.min(1)` / `.natural()` / `.step(1)`. Those are re-asserted in
//! [`resolve_profiles`], which is where upstream asserts them too: the schema's `streamIdleTimeoutMs`
//! upper bound is checked twice upstream (once by `max(MAX_TIMER_DELAY_MS)`, once by hand in
//! resolution), so the hand check is the load-bearing one.
//!
//! `resolveProfiles` reads three things from outside its own file: the installed catalog
//! (`resolveRouteModels`), the provider constructor (`buildProvider`), and two library helpers
//! (`credentialRef`, `resolveRetryPolicy`). None of them is Cordis, none is network, and all three
//! already exist in this build — [`crate::provider_catalog_gap::resolve_route_models_with`],
//! [`crate::provider_catalog_gap::SUPPORTED_PROTOCOLS`], and
//! [`crate::deepseek_options_gap::resolve_retry_policy`]. So it is a pure function of
//! `(profiles, installed catalog)`, and that is how it is implemented here.
//!
//! # `index.ts`, capability by capability
//!
//! | Upstream | Lines | What it does | Cordis? |
//! | --- | --- | --- | --- |
//! | `name` / `inject` | 95–96 | plugin identity and the `llm` injection | declarations |
//! | `llm-pi-ai/request-headers` event | 98–115 | a waterfall the adapter runs per request | **Cordis** |
//! | `registrationFacts` | 124–135 | the per-route facts the registry captures | **pure** |
//! | `directoryEntries` | 148–171 | the configurable-provider directory: installed ∪ declared | **pure** |
//! | `apply` | 174–346 | the whole mount | **Cordis** |
//! | `profiles()` memo | 188–196 | raw snapshot → resolved map, memoized on identity | **pure** |
//! | `internal/config` listener | 197–206 | `assertServiceable` over the candidate config | **Cordis** (`ctx.on`) |
//! | `resolveApiKey` | 208–231 | `ctx.get('credentials')` / launch environment lookup | **Cordis** seam; the *decision* is pure |
//! | `new PiAiAdapter` | 237–257 | the request path onto pi-ai | pi-ai runtime |
//! | `registerPiAiFlows` injection | 263 | interactive OAuth/API-key sign-in flows | **Cordis** + network |
//! | `ensureDirectory` | 270–285 | `registerConfigurableProviders` / `replace` | **Cordis** |
//! | `storedDiscoveryProfile` | 287–297 | the stored half of a discovery request | **pure** |
//! | `registerModelDiscovery` | 304–307 | endpoint interrogation | **Cordis** + network |
//! | `ensureRegistrationFacts` | 314–337 | register / replace the route set | **Cordis**; the *decision* is pure |
//! | `loader/volatile-update` listener | 339–345 | re-run the two `ensure*` on a live config change | **Cordis** |
//!
//! So of `index.ts`'s twelve real behaviours, three are pure and nine are the plugin host. The
//! important consequence, and the reason `registerPiAiFlows` cannot be ported wholesale: it does not
//! compute a flow, it *registers* one into `ctx.authorization`, and the flow body runs an OAuth
//! conversation through pi-ai's `models.login`. Both halves are the host's. What is portable out of
//! it is the *gate* — a provider is offered a sign-in only when its id can address a credential
//! record — which is [`can_address_credential_record`].
//!
//! # What this build already covered, and what it did not
//!
//! [`crate::provider_catalog_gap`] (2,300 lines, 44 public items) covers the config.ts *inputs*: the
//! provider directory, the vocabulary and compat gates, `resolveRouteModels` and its precedence
//! chain, plus the discovery parsers. [`crate::pi_ai_runtime_gap`] (2,300 lines, 44 public items)
//! covers the adapter's request path: stop-reason mapping, context conversion, image offload, and
//! prefix binding.
//!
//! What neither covers is everything *between* them, which is exactly the 897 lines above:
//!
//! 1. **The profile shape itself.** A route profile had no Rust type at all — no
//!    `PiAiProviderProfile`, so no place to state a `streamIdleTimeoutMs` or a `retryPolicy`, and no
//!    way to reach the two modules above from configuration.
//! 2. **Resolution.** Every adapter-owned default (`streamIdleTimeoutMs`, the five image bounds, the
//!    quantum capped at `maxRequestImages`), every structural refusal (empty `baseURL`, empty
//!    `displayName`, a header `Fetch` cannot carry, a removed pre-release field), and the
//!    strict-vs-deferred split that lets a *stored* route survive a catalog change while a *write*
//!    rejects it.
//! 3. **`assertServiceable`'s narrowing.** The subtle rule that only profiles which *changed* are
//!    re-resolved, so a route broken by a catalog upgrade stays editable and does not block edits to
//!    a route that is fine. Getting this wrong turns one stale route into an unopenable settings
//!    page.
//! 4. **The registration/directory decisions.** `registrationFacts` + `ensureRegistrationFacts` and
//!    `directoryEntries` + `ensureDirectory` are four decisions wrapped around four host calls. The
//!    decisions are: which facts make a re-register necessary, when a bare mount stays dormant, and
//!    which routes the directory must show. A port that skipped them would re-register on every
//!    keystroke or never re-register at all.
//!
//! # What this module implements
//!
//! - [`PiAiProviderProfile`] and [`providers_from_json`]: the profile shape plus its JSON reader,
//!   including [`reject_removed_fields`] and the "providers is a dict, not an array" refusal.
//! - [`resolve_profiles`] / [`resolve_profiles_from_json`]: upstream's `resolveProfiles`, strict and
//!   deferred, over an injected [`InstalledRoutes`].
//! - [`assert_serviceable`]: upstream's `assertServiceable`, including its narrowing.
//! - [`build_provider`]: `buildProvider`'s decision — reuse the installed catalog provider, or build
//!   from the protocol table — as a pure [`PiProvider`] descriptor. The pi-ai `Provider` object
//!   itself is the runtime's, and stays out.
//! - [`registration_facts`] / [`plan_registration`] / [`directory_entries`] /
//!   [`stored_discovery_profile`]: the pure halves of `ensureRegistrationFacts`, `ensureDirectory`,
//!   `directoryEntries`, and `storedDiscoveryProfile`.
//! - [`resolve_route_credential`]: the decision inside `resolveApiKey`, split from the Cordis lookup
//!   that feeds it. The split is the point: a miss must fail loud with `MISSING_CREDENTIAL`, because
//!   handing pi-ai `None` would let it pick up an unrelated ambient key and bill another tenant.
//! - [`apply_hooks`] / [`config_hooks`]: the two classification tables above, as data, so a test can
//!   pin that nothing which is pure has been left unimplemented and nothing which is Cordis has been
//!   pretended portable.
//!
//! # What this module deliberately does not implement
//!
//! - `registerPiAiFlows` (and `login.ts`'s `relay` / `restate` / `loginMethods`): a flow body is an
//!   OAuth conversation registered into `ctx.authorization`. Only its gate —
//!   [`can_address_credential_record`] — is portable.
//! - `ctx.inject` / `ctx.on` / `ctx.effect` / `ctx.waterfall` and the two registration handles: the
//!   host. [`plan_registration`] returns the *action* those calls would take.
//! - The pi-ai `Provider` object and `createProvider`, [`crate::provider_catalog_gap`]'s boundary.
//! - `Config = z.object(...)`: a schema is `schemastery`'s; [`providers_from_json`] is the reader and
//!   [`resolve_profiles`] is the validator, which is what the schema's defaults and bounds amount to.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value};

use girder_core::deepseek_options_gap::{
    ResolvedRetryPolicy, RetryPolicyConfig, is_credential_ref_name, resolve_retry_policy,
};
use girder_core::llm_extra_gap::MAX_TIMER_DELAY_MS;
use girder_core::provider_catalog_gap::{
    CatalogError, CatalogModel, CompatProfile, DEFAULT_CONTEXT_WINDOW, DEFAULT_INPUT,
    DEFAULT_MAX_TOKENS, InstalledCatalog, Modality, ModelFields, ModelOverride, ModelProfile,
    ProviderDefaults, ReasoningEfforts, RouteCatalogRequest, SUPPORTED_PROTOCOLS, ThinkingLevel,
    Validation, provider_defaults_of, resolve_route_models_with,
};

// =============================================================================================
// Adapter-owned defaults (upstream config.ts:47–81)
// =============================================================================================

/// Default maximum idle interval while an adapter stream read is outstanding.
///
/// Reached only by waiting: a provider that accepts the request and then says nothing. Five minutes
/// is long enough for a 128k-token prefill and short enough that a wedged connection does not hold a
/// turn until the process ends.
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;

/// Default request-level bound on base64-encoded image payload.
///
/// Every image in history is re-encoded into every request body, so an unbounded conversation
/// eventually exceeds a gateway's request-size cap and the session can never complete another
/// request. 20 MiB admits fifteen 1 MiB request versions after base64 expansion and reserves
/// capacity for the system prompt, history, tools, and JSON.
pub const DEFAULT_MAX_REQUEST_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// Default request-level bound on retained image occurrences.
///
/// Azure `OpenAI`'s documented limit of 50 images per GPT-4o and GPT-4.1 request; routes whose
/// provider accepts more raise it.
pub const DEFAULT_MAX_REQUEST_IMAGES: u64 = 50;

/// Default number of oldest occurrences one count overflow offloads.
///
/// One offload per ten new images rather than one per image: a saturated route rewrites its request
/// prefix once per quantum instead of once per attachment.
pub const DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM: u64 = 10;

/// Default total-pixel budget, which preserves the complete 2048px normalized attachment.
pub const DEFAULT_REQUEST_IMAGE_PIXEL_BUDGET: u64 = 2048 * 2048;

/// Default raw encoded-byte target before inline base64 expansion.
pub const DEFAULT_REQUEST_IMAGE_MAX_BYTES: u64 = 1024 * 1024;

/// The settings namespace a bare mount registers under (upstream `NS`).
pub const NS: &str = "llm-pi-ai";

// =============================================================================================
// Portability classification, as data
// =============================================================================================

/// What stands between a piece of upstream and a port of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Portability {
    /// Implements as a Rust function: no host, no network, no third-party binding.
    Pure,
    /// The behaviour is a `schemastery` schema — defaults and bounds that a Rust type and
    /// [`resolve_profiles`] carry instead.
    Schema,
    /// Requires the Cordis plugin host: `inject`, `on`, `effect`, `waterfall`, or a registration
    /// handle the host owns.
    Cordis,
    /// Requires the pi-ai runtime object (`Provider`, `Models`, `createProvider`, `models.login`).
    PiAiRuntime,
    /// Requires network IO (endpoint interrogation, OAuth conversations).
    ///
    /// No row in either table below is network-only: `discoverModels` is classified through the Cordis
    /// handle that calls it. The variant exists so a later table can classify a *parser* separately
    /// from its transport, which is the split [`crate::provider_catalog_gap`] already makes.
    #[allow(
        dead_code,
        reason = "no row of the two current tables is network-only; the variant is what a later row would use to separate a parser from its transport"
    )]
    Network,
}

impl Portability {
    /// The label used by the classification tables.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Schema => "schemastery",
            Self::Cordis => "cordis",
            Self::PiAiRuntime => "pi-ai runtime",
            Self::Network => "network",
        }
    }

    /// Whether a port can implement it as a plain function.
    ///
    /// [`Self::Schema`] is excluded deliberately: a schema is `schemastery`'s object, and what a port
    /// reproduces is the *behaviour* it encodes — defaults and bounds — inside
    /// [`resolve_profiles`], not the schema itself. Counting it as portable would hide that the
    /// translation happened somewhere.
    #[must_use]
    pub const fn is_portable(self) -> bool {
        matches!(self, Self::Pure)
    }
}

impl fmt::Display for Portability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Every capability of upstream `config.ts`, with what a port needs to reproduce it.
///
/// Ordered as the file is, so the table reads like the module it classifies.
pub const CONFIG_HOOKS: &[(&str, Portability)] = &[
    ("DEFAULT_* constants", Portability::Pure),
    (
        "PiAiProviderProfile / ResolvedPiAiProviderProfile / Config / Options",
        Portability::Pure,
    ),
    ("schemastery schemas", Portability::Schema),
    ("assertServiceable", Portability::Pure),
    ("rejectRemovedFields", Portability::Pure),
    ("assertValidHeaders", Portability::Pure),
    ("resolveProfiles", Portability::Pure),
];

/// Every capability of upstream `index.ts`, with what a port needs to reproduce it.
///
/// `apply` is listed once even though it is the file: its twelve behaviours appear individually
/// below it, which is where the port happens or does not.
pub const INDEX_HOOKS: &[(&str, Portability)] = &[
    ("name / inject declarations", Portability::Pure),
    ("llm-pi-ai/request-headers event", Portability::Cordis),
    ("apply", Portability::Cordis),
    ("registrationFacts", Portability::Pure),
    ("directoryEntries", Portability::Pure),
    ("profiles memo", Portability::Pure),
    ("internal/config listener", Portability::Cordis),
    ("resolveApiKey", Portability::Cordis),
    ("PiAiAdapter construction", Portability::PiAiRuntime),
    ("registerPiAiFlows injection", Portability::Cordis),
    ("ensureDirectory", Portability::Cordis),
    ("storedDiscoveryProfile", Portability::Pure),
    ("registerModelDiscovery", Portability::Cordis),
    ("ensureRegistrationFacts", Portability::Cordis),
    ("loader/volatile-update listener", Portability::Cordis),
];

/// `config.ts`'s classification table.
#[must_use]
pub fn config_hooks() -> &'static [(&'static str, Portability)] {
    CONFIG_HOOKS
}

/// `index.ts`'s classification table.
#[must_use]
pub fn apply_hooks() -> &'static [(&'static str, Portability)] {
    INDEX_HOOKS
}

/// One named hook's classification, from either table.
#[must_use]
pub fn portability_of(hook: &str) -> Option<Portability> {
    CONFIG_HOOKS
        .iter()
        .chain(INDEX_HOOKS)
        .find(|(name, _)| *name == hook)
        .map(|(_, portability)| *portability)
}

// =============================================================================================
// Profile vocabulary
// =============================================================================================

/// Prompt-cache retention preference — upstream `CacheRetention`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheRetention {
    /// Do not ask the provider to cache.
    None,
    /// The provider's default lifetime.
    Short,
    /// The long-retention tier, where the provider sells one.
    Long,
}

impl CacheRetention {
    /// The wire spelling upstream's schema accepts.
    ///
    /// Paired with [`CacheRetention::parse`]: a round-trip helper is what a diagnostic or a settings
    /// form reads, and leaving it out would make the two halves of one enum asymmetric.
    #[allow(
        dead_code,
        reason = "the wire spelling is the other half of `parse`; callers that render a profile are outside this module"
    )]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Short => "short",
            Self::Long => "long",
        }
    }

    /// Reads a wire spelling, or `None` when it names no retention.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "none" => Some(Self::None),
            "short" => Some(Self::Short),
            "long" => Some(Self::Long),
            _ => None,
        }
    }
}

/// Streaming transport preference — upstream `Transport`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Server-sent events.
    Sse,
    /// A plain WebSocket.
    Websocket,
    /// A WebSocket that reuses its connection across requests.
    WebsocketCached,
    /// Let the provider decide.
    Auto,
}

impl Transport {
    /// The wire spelling upstream's schema accepts.
    ///
    /// Paired with [`Transport::parse`] for the same reason as [`CacheRetention::as_str`].
    #[allow(
        dead_code,
        reason = "the wire spelling is the other half of `parse`; callers that render a profile are outside this module"
    )]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sse => "sse",
            Self::Websocket => "websocket",
            Self::WebsocketCached => "websocket-cached",
            Self::Auto => "auto",
        }
    }

    /// Reads a wire spelling, or `None` when it names no transport.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "sse" => Some(Self::Sse),
            "websocket" => Some(Self::Websocket),
            "websocket-cached" => Some(Self::WebsocketCached),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// Token budgets used by reasoning providers that support them — upstream `ThinkingBudgets`.
///
/// Every member is optional because the provider takes whichever ones it reads; an absent member
/// keeps pi-ai's own default for that level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThinkingBudgets {
    /// Budget for the `minimal` level.
    pub minimal: Option<u64>,
    /// Budget for the `low` level.
    pub low: Option<u64>,
    /// Budget for the `medium` level.
    pub medium: Option<u64>,
    /// Budget for the `high` level.
    pub high: Option<u64>,
}

/// Every member upstream's `thinkingBudgets` schema declares.
const THINKING_BUDGET_KEYS: [&str; 4] = ["minimal", "low", "medium", "high"];

/// A profile's thinking budgets, as the levels dispatch reads them.
impl ThinkingBudgets {
    /// Reads a budget block, refusing an unknown member.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] for an unknown member or a negative/non-integer budget.
    pub fn from_json(provider: &str, value: &Value) -> Result<Self, CatalogError> {
        let object = value.as_object().ok_or_else(|| {
            CatalogError::new(
                provider,
                "must set thinkingBudgets to an object of token budgets",
            )
        })?;
        for key in object.keys() {
            if !THINKING_BUDGET_KEYS.contains(&key.as_str()) {
                return Err(CatalogError::new(
                    provider,
                    format!(
                        "sets thinkingBudgets.{key}, which is not one of {}",
                        THINKING_BUDGET_KEYS.join(", ")
                    ),
                ));
            }
        }
        Ok(Self {
            minimal: budget_member(provider, object, "minimal")?,
            low: budget_member(provider, object, "low")?,
            medium: budget_member(provider, object, "medium")?,
            high: budget_member(provider, object, "high")?,
        })
    }
}

fn budget_member(
    provider: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, CatalogError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            CatalogError::new(
                provider,
                format!("sets thinkingBudgets.{key}, which must be a non-negative integer"),
            )
        }),
    }
}

// =============================================================================================
// The profile shape (upstream config.ts:104–208)
// =============================================================================================

/// The members upstream's `profile` schema declares.
const PROFILE_KEYS: [&str; 24] = [
    "apiKeyEnv",
    "displayName",
    "api",
    "baseURL",
    "models",
    "modelOverrides",
    "compat",
    "defaultContextWindow",
    "defaultMaxTokens",
    "defaultInput",
    "headers",
    "reasoning",
    "thinkingBudgets",
    "cacheRetention",
    "transport",
    "timeoutMs",
    "websocketConnectTimeoutMs",
    "streamIdleTimeoutMs",
    "maxRequestImageBytes",
    "maxRequestImages",
    "imageOffloadCountQuantum",
    "requestImagePixelBudget",
    "requestImageMaxBytes",
    "retryPolicy",
];

/// The fields a `models` entry and a `modelOverrides` value share.
const MODEL_FIELD_KEYS: [&str; 6] = [
    "name",
    "contextWindow",
    "maxTokens",
    "input",
    "reasoningEfforts",
    "compat",
];

/// Configuration for one pi-ai provider route; the `providers` map key *is* the route.
///
/// Every member is optional because every one of them is a statement about a route rather than a
/// requirement on it: a route that states nothing but its credential still serves the installed
/// catalog exactly as pi-ai ships it. Which is also why `Option` is load-bearing rather than
/// stylistic here — `None` means "this layer says nothing", which is different from a stated `0` or
/// an empty list, and [`resolve_profiles`] is where the two are told apart.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PiAiProviderProfile {
    /// Credential reference (environment-variable name) resolved per request.
    pub api_key_env: Option<String>,
    /// Name shown by configuration surfaces; defaults to the route key.
    pub display_name: Option<String>,
    /// Wire protocol every model on this route speaks.
    ///
    /// Omission keeps each installed catalog model's own protocol, which is why a catalog route needs
    /// no protocol at all; a route the catalog does not ship must name one.
    pub api: Option<String>,
    /// Endpoint for this route's models; defaults to the installed catalog's endpoint.
    pub base_url: Option<String>,
    /// This route's model catalog; omission serves the installed catalog unchanged.
    pub models: Option<Vec<ModelProfile>>,
    /// Installed-catalog customizations by model id.
    ///
    /// Only meaningful on a catalog route with no `models` list; an override beside a list, on a route
    /// the catalog does not ship, or naming a model the catalog does not describe is refused rather
    /// than skipped — by [`crate::provider_catalog_gap::resolve_route_models_with`], which owns that
    /// rule.
    pub model_overrides: BTreeMap<String, ModelOverride>,
    /// Switches defaulting every model on this route whose protocol declares them.
    pub compat: Option<CompatProfile>,
    /// Context capacity for a listed model neither the entry nor the catalog sizes.
    pub default_context_window: Option<u64>,
    /// Output capability for a listed model neither the entry nor the catalog sizes.
    pub default_max_tokens: Option<u64>,
    /// Request modalities for a listed model neither the entry nor the catalog declares.
    ///
    /// A fallback like the two capacities above, never a narrowing: unlike an entry's list this one
    /// may not be empty, because nothing sits below it to answer instead.
    pub default_input: Option<Vec<Modality>>,
    /// Provider request headers, validated when the profile resolves.
    pub headers: BTreeMap<String, String>,
    /// Provider-neutral pi-ai reasoning level.
    pub reasoning: Option<ThinkingLevel>,
    /// Token budgets used by reasoning providers that support them.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Prompt-cache retention preference.
    pub cache_retention: Option<CacheRetention>,
    /// Streaming transport preference.
    pub transport: Option<Transport>,
    /// HTTP/provider SDK timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// WebSocket connection timeout in milliseconds.
    pub websocket_connect_timeout_ms: Option<u64>,
    /// Maximum provider idle time while one stream read is outstanding.
    pub stream_idle_timeout_ms: Option<f64>,
    /// Maximum base64-encoded image payload per request.
    pub max_request_image_bytes: Option<u64>,
    /// Maximum retained image occurrences per request.
    pub max_request_images: Option<u64>,
    /// Oldest occurrences offloaded per count overflow.
    pub image_offload_count_quantum: Option<u64>,
    /// Total-pixel budget for each deterministic inline request version.
    pub request_image_pixel_budget: Option<u64>,
    /// Raw encoded-byte target for each deterministic inline request version.
    pub request_image_max_bytes: Option<u64>,
    /// Provider-owned model-request retry policy.
    pub retry_policy: Option<RetryPolicyConfig>,
}

/// Refuse the pre-release profile fields, naming each one's replacement.
///
/// The fields are refused rather than ignored for the reason a typo is: `maxRetries` on a pi-ai
/// profile looks like it configures retries and configures nothing. A route that kept it would retry
/// on the pi-ai layer's schedule while the operator watched a knob that did nothing.
///
/// # Errors
///
/// Returns [`CatalogError`] naming the field and where it moved.
pub fn reject_removed_fields(
    provider: &str,
    object: &Map<String, Value>,
) -> Result<(), CatalogError> {
    if object.contains_key("provider") {
        return Err(CatalogError::new(
            provider,
            "sets \"provider\", which moved to the providers dict key",
        ));
    }
    if object.contains_key("maxRetries") || object.contains_key("maxRetryDelayMs") {
        return Err(CatalogError::new(
            provider,
            "sets maxRetries or maxRetryDelayMs, which were removed; \
             compose agent recovery with dsh-llm-retry",
        ));
    }
    Ok(())
}

/// Reads every provider profile from a configuration value.
///
/// # Errors
///
/// Returns [`CatalogError`] when `providers` is an array (upstream's shape before the dict), when one
/// profile is not an object, or when any profile states a removed or unknown member. The route key
/// is empty for an array or non-object refusal, because the fault is the shape rather than one
/// route's.
pub fn providers_from_json(
    value: &Value,
) -> Result<BTreeMap<String, PiAiProviderProfile>, CatalogError> {
    let Some(object) = value.as_object() else {
        if value.is_array() {
            return Err(CatalogError::new(
                "",
                "providers must be a dict keyed by provider route, not an array of profiles",
            ));
        }
        return Err(CatalogError::new(
            "",
            "must set providers to a dict keyed by provider route",
        ));
    };
    let mut profiles = BTreeMap::new();
    for (provider, profile) in object {
        profiles.insert(
            provider.clone(),
            PiAiProviderProfile::from_json(provider, profile)?,
        );
    }
    Ok(profiles)
}

impl PiAiProviderProfile {
    /// Reads one route's profile.
    ///
    /// Unknown members are refused, which is the half of upstream's behaviour `schemastery` provided
    /// and a plain struct would otherwise lose silently: a `streamIdleTimoutMs` typo must not resolve
    /// to the default while looking configured.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] for a removed field, an unknown member, or a member whose type this
    /// shape cannot carry.
    pub fn from_json(provider: &str, value: &Value) -> Result<Self, CatalogError> {
        let object = value.as_object().ok_or_else(|| {
            CatalogError::new(provider, "must be an object stating the route's profile")
        })?;
        reject_removed_fields(provider, object)?;
        for key in object.keys() {
            if !PROFILE_KEYS.contains(&key.as_str()) {
                return Err(CatalogError::new(
                    provider,
                    format!(
                        "has an unknown key {key:?}; the profile fields are {}",
                        PROFILE_KEYS.join(", ")
                    ),
                ));
            }
        }
        Ok(Self {
            api_key_env: string_member(provider, object, "apiKeyEnv")?,
            display_name: string_member(provider, object, "displayName")?,
            api: string_member(provider, object, "api")?,
            base_url: string_member(provider, object, "baseURL")?,
            models: match object.get("models") {
                None | Some(Value::Null) => None,
                Some(value) => Some(parse_models(provider, value)?),
            },
            model_overrides: parse_model_overrides(provider, object.get("modelOverrides"))?,
            compat: match object.get("compat") {
                None | Some(Value::Null) => None,
                Some(value) => Some(CompatProfile::from_json(provider, "route", value)?),
            },
            default_context_window: integer_member(provider, object, "defaultContextWindow")?,
            default_max_tokens: integer_member(provider, object, "defaultMaxTokens")?,
            default_input: match object.get("defaultInput") {
                None | Some(Value::Null) => None,
                Some(value) => Some(parse_modalities(provider, "defaultInput", value)?),
            },
            headers: parse_headers(provider, object.get("headers"))?,
            reasoning: match object.get("reasoning") {
                None | Some(Value::Null) => None,
                Some(value) => Some(parse_reasoning_level(provider, value)?),
            },
            thinking_budgets: match object.get("thinkingBudgets") {
                None | Some(Value::Null) => None,
                Some(value) => Some(ThinkingBudgets::from_json(provider, value)?),
            },
            cache_retention: match object.get("cacheRetention") {
                None | Some(Value::Null) => None,
                Some(value) => Some(parse_cache_retention(provider, value)?),
            },
            transport: match object.get("transport") {
                None | Some(Value::Null) => None,
                Some(value) => Some(parse_transport(provider, value)?),
            },
            timeout_ms: integer_member(provider, object, "timeoutMs")?,
            websocket_connect_timeout_ms: integer_member(
                provider,
                object,
                "websocketConnectTimeoutMs",
            )?,
            stream_idle_timeout_ms: number_member(provider, object, "streamIdleTimeoutMs")?,
            max_request_image_bytes: integer_member(provider, object, "maxRequestImageBytes")?,
            max_request_images: integer_member(provider, object, "maxRequestImages")?,
            image_offload_count_quantum: integer_member(
                provider,
                object,
                "imageOffloadCountQuantum",
            )?,
            request_image_pixel_budget: integer_member(
                provider,
                object,
                "requestImagePixelBudget",
            )?,
            request_image_max_bytes: integer_member(provider, object, "requestImageMaxBytes")?,
            retry_policy: match object.get("retryPolicy") {
                None | Some(Value::Null) => None,
                Some(value) => {
                    let Some(block) = value.as_object() else {
                        return Err(CatalogError::new(
                            provider,
                            "must set retryPolicy to an object",
                        ));
                    };
                    Some(read_retry_policy(provider, block)?)
                }
            },
        })
    }
}

/// Reads a `retryPolicy` block, supplying `mode` when the block leaves it out.
///
/// Upstream's `normalPolicySchema` declares `mode: z.const('normal').required()`, which schemastery
/// reads as "required of a *judged* object but defaulted for a missing member" — a block stating only
/// `maxRetries` is accepted upstream and means normal mode. [`RetryPolicyConfig::from_json`] is the
/// stricter reader (a missing `mode` is an error, which is right for a function that only parses), so
/// the default is applied here rather than by loosening that reader for every caller.
fn read_retry_policy(
    provider: &str,
    block: &Map<String, Value>,
) -> Result<RetryPolicyConfig, CatalogError> {
    let path = format!("llm-pi-ai: provider \"{provider}\" retryPolicy");
    if block.contains_key("mode") {
        return RetryPolicyConfig::from_json(block, &path)
            .map_err(|error| CatalogError::new(provider, error.message()));
    }
    let mut with_mode = block.clone();
    with_mode.insert("mode".to_owned(), Value::String("normal".to_owned()));
    RetryPolicyConfig::from_json(&with_mode, &path)
        .map_err(|error| CatalogError::new(provider, error.message()))
}

fn string_member(
    provider: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, CatalogError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(CatalogError::new(
            provider,
            format!("sets {key}, which must be a string"),
        )),
    }
}

fn integer_member(
    provider: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, CatalogError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            CatalogError::new(
                provider,
                format!("sets {key}, which must be a non-negative integer"),
            )
        }),
    }
}

fn number_member(
    provider: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<f64>, CatalogError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_f64().map(Some).ok_or_else(|| {
            CatalogError::new(provider, format!("sets {key}, which must be a number"))
        }),
    }
}

fn parse_reasoning_level(provider: &str, value: &Value) -> Result<ThinkingLevel, CatalogError> {
    value
        .as_str()
        .and_then(ThinkingLevel::parse)
        .ok_or_else(|| {
            CatalogError::new(
                provider,
                "sets reasoning, which must be one of off, minimal, low, medium, high, xhigh, max",
            )
        })
}

fn parse_cache_retention(provider: &str, value: &Value) -> Result<CacheRetention, CatalogError> {
    value
        .as_str()
        .and_then(CacheRetention::parse)
        .ok_or_else(|| {
            CatalogError::new(
                provider,
                "sets cacheRetention, which must be one of none, short, long",
            )
        })
}

fn parse_transport(provider: &str, value: &Value) -> Result<Transport, CatalogError> {
    value.as_str().and_then(Transport::parse).ok_or_else(|| {
        CatalogError::new(
            provider,
            "sets transport, which must be one of sse, websocket, websocket-cached, auto",
        )
    })
}

fn parse_modalities(
    provider: &str,
    site: &str,
    value: &Value,
) -> Result<Vec<Modality>, CatalogError> {
    let Some(entries) = value.as_array() else {
        return Err(CatalogError::new(
            provider,
            format!("sets {site}, which must be an array of modalities"),
        ));
    };
    let mut modalities = Vec::with_capacity(entries.len());
    for entry in entries {
        let parsed = entry.as_str().and_then(Modality::parse);
        match parsed {
            Some(modality) => modalities.push(modality),
            None => {
                return Err(CatalogError::new(
                    provider,
                    format!("sets {site}, which names a modality that is not text or image"),
                ));
            }
        }
    }
    Ok(modalities)
}

fn parse_headers(
    provider: &str,
    value: Option<&Value>,
) -> Result<BTreeMap<String, String>, CatalogError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    let Some(object) = value.as_object() else {
        return Err(CatalogError::new(
            provider,
            "must set headers to a dict of header names to values",
        ));
    };
    let mut headers = BTreeMap::new();
    for (name, value) in object {
        let Some(text) = value.as_str() else {
            return Err(CatalogError::new(
                provider,
                format!("sets header {name:?}, whose value must be a string"),
            ));
        };
        headers.insert(name.clone(), text.to_owned());
    }
    Ok(headers)
}

fn parse_models(provider: &str, value: &Value) -> Result<Vec<ModelProfile>, CatalogError> {
    let Some(entries) = value.as_array() else {
        return Err(CatalogError::new(
            provider,
            "must set models to an array of model entries",
        ));
    };
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(object) = entry.as_object() else {
            return Err(CatalogError::new(
                provider,
                "has a models entry that is not an object",
            ));
        };
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            return Err(CatalogError::new(
                provider,
                "has a models entry with no string id",
            ));
        };
        for key in object.keys() {
            if key != "id" && !MODEL_FIELD_KEYS.contains(&key.as_str()) {
                return Err(CatalogError::new(
                    provider,
                    format!(
                        "has a models entry for \"{id}\" with an unknown key {key:?}; \
                         an entry states id and {}",
                        MODEL_FIELD_KEYS.join(", ")
                    ),
                ));
            }
        }
        models.push(ModelProfile::new(id).with_fields(parse_model_fields(
            provider,
            object,
            &format!("models entry \"{id}\""),
        )?));
    }
    Ok(models)
}

fn parse_model_overrides(
    provider: &str,
    value: Option<&Value>,
) -> Result<BTreeMap<String, ModelOverride>, CatalogError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    let Some(object) = value.as_object() else {
        return Err(CatalogError::new(
            provider,
            "must set modelOverrides to a dict keyed by model id",
        ));
    };
    let mut overrides = BTreeMap::new();
    for (id, entry) in object {
        let Some(fields) = entry.as_object() else {
            return Err(CatalogError::new(
                provider,
                format!("has a modelOverrides entry {id:?} that is not an object"),
            ));
        };
        // The id lives in the dict key; a value carrying its own would quietly rename the model it
        // meant to customize. Upstream guards this at the schema boundary, which passes unknown keys
        // through; here the reader is that boundary.
        if fields.contains_key("id") {
            return Err(CatalogError::new(
                provider,
                format!("modelOverrides entry \"{id}\" sets \"id\", which is the dict key"),
            ));
        }
        for key in fields.keys() {
            if !MODEL_FIELD_KEYS.contains(&key.as_str()) {
                return Err(CatalogError::new(
                    provider,
                    format!(
                        "has a modelOverrides entry for \"{id}\" with an unknown key {key:?}; \
                         an override states {}",
                        MODEL_FIELD_KEYS.join(", ")
                    ),
                ));
            }
        }
        overrides.insert(
            id.clone(),
            parse_model_fields(provider, fields, &format!("modelOverrides entry \"{id}\""))?,
        );
    }
    Ok(overrides)
}

fn parse_model_fields(
    provider: &str,
    object: &Map<String, Value>,
    site: &str,
) -> Result<ModelFields, CatalogError> {
    Ok(ModelFields {
        name: string_member(provider, object, "name")?,
        context_window: positive_member(provider, object, site, "contextWindow")?,
        max_tokens: positive_member(provider, object, site, "maxTokens")?,
        input: match object.get("input") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let modalities = parse_modalities(provider, "input", value)?;
                // An empty list is "no answer here", which is what an absent array means too: the
                // route's `defaultInput`, then the catalog, still answer below it.
                if modalities.is_empty() {
                    None
                } else {
                    Some(modalities)
                }
            }
        },
        reasoning_efforts: match object.get("reasoningEfforts") {
            None | Some(Value::Null) => None,
            Some(value) => Some(parse_reasoning_efforts(provider, site, value)?),
        },
        compat: match object.get("compat") {
            None | Some(Value::Null) => None,
            Some(value) => Some(CompatProfile::from_json(provider, site, value)?),
        },
    })
}

fn positive_member(
    provider: &str,
    object: &Map<String, Value>,
    site: &str,
    key: &str,
) -> Result<Option<u64>, CatalogError> {
    match integer_member(provider, object, key)? {
        None => Ok(None),
        Some(0) => Err(CatalogError::new(
            provider,
            format!("sets {site} {key} to 0, which must be at least 1"),
        )),
        Some(value) => Ok(Some(value)),
    }
}

/// Reads `reasoningEfforts`: `false` disables reasoning, an object maps offered levels to wire
/// spellings.
fn parse_reasoning_efforts(
    provider: &str,
    site: &str,
    value: &Value,
) -> Result<ReasoningEfforts, CatalogError> {
    if value == &Value::Bool(false) {
        return Ok(ReasoningEfforts::Disabled);
    }
    let Some(object) = value.as_object() else {
        return Err(CatalogError::new(
            provider,
            format!("sets {site} reasoningEfforts, which must be false or a dict of levels"),
        ));
    };
    if object.is_empty() {
        return Err(CatalogError::new(
            provider,
            format!(
                "sets {site} reasoningEfforts to an empty dict, which declares nothing; \
                 name the offered levels or set it to false"
            ),
        ));
    }
    let mut levels = BTreeMap::new();
    for (key, entry) in object {
        let Some(level) = ThinkingLevel::parse(key) else {
            return Err(CatalogError::new(
                provider,
                format!(
                    "sets {site} reasoningEfforts.{key}, which names no thinking level; \
                     the levels are off, minimal, low, medium, high, xhigh, max"
                ),
            ));
        };
        match entry {
            Value::Null => {
                // The valueless key survives only for `off`: "supported, send nothing". Every other
                // level's absence *is* the parameter's absence, so a declared level with no spelling
                // would be written but not applied.
                if level != ThinkingLevel::Off {
                    return Err(CatalogError::new(
                        provider,
                        format!(
                            "sets {site} reasoningEfforts.{key} with no wire value; \
                             only off may leave its value empty"
                        ),
                    ));
                }
                levels.insert(level, None);
            }
            Value::String(spelling) => {
                levels.insert(level, Some(spelling.clone()));
            }
            _ => {
                return Err(CatalogError::new(
                    provider,
                    format!(
                        "sets {site} reasoningEfforts.{key}, whose value must be a string or empty"
                    ),
                ));
            }
        }
    }
    Ok(ReasoningEfforts::Levels(levels))
}

// =============================================================================================
// The installed catalog, injected
// =============================================================================================

/// One provider pi-ai ships, as configuration resolution reads it.
///
/// Three facts, all of which upstream reads from pi-ai's static registry: the provider's models, its
/// own endpoint, and the protocol its models speak by default. Injecting them is what keeps
/// [`resolve_profiles`] a function rather than a mount.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InstalledRoute {
    /// The models this provider ships, keyed by id.
    pub models: InstalledCatalog,
    /// The provider's own endpoint, which a route inherits when it states none.
    pub base_url: Option<String>,
    /// The protocol this provider's models speak when a route does not override it.
    pub api: String,
    /// Whether the provider offers an interactive `OAuth` flow, which selects the login methods a
    /// sign-in surface offers.
    pub oauth: bool,
}

/// Every provider the installed catalog ships, keyed by provider id.
pub type InstalledRoutes = BTreeMap<String, InstalledRoute>;

/// The installed provider a route names, or nothing when pi-ai does not ship it.
#[must_use]
pub fn installed_provider<'routes>(
    routes: &'routes InstalledRoutes,
    provider: &str,
) -> Option<&'routes InstalledRoute> {
    routes.get(provider)
}

// =============================================================================================
// The resolved profile (upstream config.ts:210–248)
// =============================================================================================

/// A validated profile with its route stamped and every adapter-owned default resolved.
///
/// The resolved form exists so the request path never re-reads configuration: every `Option` above
/// that had a default is a bare value here, and every diagnostic that resolution produced is carried
/// beside the models rather than thrown away.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPiAiProviderProfile {
    /// Harness route key and the `Models` collection key (the configuration dict key).
    pub provider: String,
    /// Resolved display name for selectors and configuration surfaces.
    pub display_name: String,
    /// Validated credential reference, when one is configured.
    ///
    /// A plain `String` rather than the credentials seam's branded type: this module refuses the
    /// reference when its grammar is wrong (see [`is_credential_ref_name`]), which is the whole of
    /// what the brand would add, and the brand lives behind a seam this pure function does not have.
    pub api_key_env: Option<String>,
    /// Wire protocol override, when the profile names one.
    pub api: Option<String>,
    /// Endpoint override, when the profile names one.
    pub base_url: Option<String>,
    /// Context capacity used for models neither configuration nor the catalog sizes.
    pub default_context_window: u64,
    /// Output capability used for models neither configuration nor the catalog sizes.
    pub default_max_tokens: u64,
    /// Modalities used for models neither configuration nor the catalog declares.
    pub default_input: Vec<Modality>,
    /// Provider request headers, already validated.
    pub headers: BTreeMap<String, String>,
    /// Provider-neutral reasoning level.
    pub reasoning: Option<ThinkingLevel>,
    /// Token budgets for reasoning providers that read them.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Prompt-cache retention preference.
    pub cache_retention: Option<CacheRetention>,
    /// Streaming transport preference.
    pub transport: Option<Transport>,
    /// HTTP/provider SDK timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// WebSocket connection timeout in milliseconds.
    pub websocket_connect_timeout_ms: Option<u64>,
    /// Positive finite provider-idle interval after defaulting.
    pub stream_idle_timeout_ms: f64,
    /// Positive request-level base64 image payload bound after defaulting.
    pub max_request_image_bytes: u64,
    /// Positive request-level retained image count bound after defaulting.
    pub max_request_images: u64,
    /// Positive count-overflow offload step, at most [`Self::max_request_images`], after defaulting.
    pub image_offload_count_quantum: u64,
    /// Positive total-pixel request-version budget after defaulting.
    pub request_image_pixel_budget: u64,
    /// Positive raw request-version byte target after defaulting.
    pub request_image_max_bytes: u64,
    /// Immutable retry policy captured with this provider route.
    pub retry_policy: ResolvedRetryPolicy,
    /// The provider containing this route's serviceable models.
    ///
    /// Absent when a stored route cannot be constructed — a deferred read keeps the profile editable
    /// rather than deleting a route the operator can still repair.
    pub pi_provider: Option<PiProvider>,
    /// First model diagnostic, or the route failure when no model diagnostic is available.
    pub catalog_error: Option<String>,
    /// Per-model failures reported before attempting a request.
    pub model_errors: BTreeMap<String, String>,
    /// The route's materialized models, in configuration order.
    ///
    /// Empty for a deferred read whose catalog could not materialize, which is the same state
    /// [`Self::pi_provider`] reports as absent.
    pub models: Vec<CatalogModel>,
    /// Per-request output caps this profile explicitly configured, by model id.
    ///
    /// Separate from a model's `max_tokens`, which is its output *capability*: materializing a
    /// capability as a request default would start capping every request at a number nobody picked.
    pub configured_max_tokens: BTreeMap<String, u64>,
}

impl ResolvedPiAiProviderProfile {
    /// Whether this route has at least one serviceable model.
    ///
    /// Presence of the provider, not absence of a diagnostic: a deferred read records each unusable
    /// model in [`Self::catalog_error`] *beside* the models that still serve, so a route that lost one
    /// of thirty is serviceable and must keep serving. Only a route whose catalog could not
    /// materialize at all has no provider.
    #[must_use]
    pub fn is_serviceable(&self) -> bool {
        self.pi_provider.is_some()
    }

    /// The facts the adapter registry captures for this route.
    #[must_use]
    pub fn registration_fact(&self) -> RegistrationFact {
        RegistrationFact {
            provider: self.provider.clone(),
            display_name: self.display_name.clone(),
            retry_policy: self.retry_policy.clone(),
        }
    }
}

// =============================================================================================
// Provider construction (upstream provider.ts:193, reduced to its decision)
// =============================================================================================

/// The pi-ai provider one route registers, as the facts a pure function can decide.
///
/// Upstream's `buildProvider` returns a live `Provider` because a `Provider` is the thing `Models`
/// registers. Everything in that object except the protocol implementation is a copy of the route's
/// identity, which is what this type carries. The object itself is the runtime's — see the module
/// documentation's `PiAiRuntime` column.
#[derive(Clone, Debug, PartialEq)]
pub struct PiProvider {
    /// Route key; also the `Models` collection key and each model's `provider`.
    pub id: String,
    /// Display name for selectors and status labels.
    pub name: String,
    /// Provider-level endpoint, which is display metadata: requests follow each model's own.
    pub base_url: Option<String>,
    /// The protocol this route's models speak.
    pub api: String,
    /// Whether this route reuses the installed catalog provider rather than a fresh one.
    ///
    /// True is not an optimization: a catalog provider owns API implementations this build cannot
    /// reconstruct (Bedrock loads its Smithy module through a separate entry point), so rebuilding it
    /// from parts would silently narrow which providers work. A route that keeps its catalog protocol
    /// therefore must reuse it, and only an explicit protocol override may build a new one.
    pub reuses_catalog_provider: bool,
    /// Whether the profile names a credential, which only decides whether the harness adds its own
    /// api-key method to a catalog provider that offers none.
    pub names_credential: bool,
    /// The route's materialized models, in configuration order.
    ///
    /// Carried on the provider rather than beside it because it is what the provider *serves*: a
    /// `Provider` whose `getModels` returned anything else would be serving a catalog nobody resolved.
    pub models: Vec<CatalogModel>,
}

/// Builds the provider for one resolved route.
///
/// # Errors
///
/// Returns [`CatalogError`] when the route names a wire protocol this build cannot serve. Upstream
/// interpolates the absent protocol as `undefined` in that message, which is reachable only for a
/// route the catalog does not ship and that names no protocol; the spelling is kept so the two
/// diagnostics are recognizable as the same fault.
pub fn build_provider(
    provider: &str,
    display_name: &str,
    api: Option<&str>,
    base_url: Option<&str>,
    names_credential: bool,
    installed: Option<&InstalledRoute>,
    models: Vec<CatalogModel>,
) -> Result<PiProvider, CatalogError> {
    if let Some(installed) = installed {
        if api.is_none() {
            return Ok(PiProvider {
                id: provider.to_owned(),
                name: display_name.to_owned(),
                base_url: base_url
                    .or(installed.base_url.as_deref())
                    .map(str::to_owned),
                api: installed.api.clone(),
                reuses_catalog_provider: true,
                names_credential,
                models,
            });
        }
    }
    let named = api.unwrap_or("undefined");
    if !SUPPORTED_PROTOCOLS.contains(&named) {
        return Err(CatalogError::new(
            provider,
            format!(
                "names api \"{named}\", which this build cannot serve; supported protocols are {}",
                SUPPORTED_PROTOCOLS.join(", ")
            ),
        ));
    }
    Ok(PiProvider {
        id: provider.to_owned(),
        name: display_name.to_owned(),
        base_url: base_url.map(str::to_owned),
        api: named.to_owned(),
        reuses_catalog_provider: false,
        names_credential,
        models,
    })
}

// =============================================================================================
// Header validation (upstream config.ts:419–431)
// =============================================================================================

/// Rejects a profile header `Fetch` cannot put on a provider request.
///
/// Upstream asks `new Headers([[name, value]])` and lets it throw. The two questions that throw
/// answers are checkable: the name must be an HTTP token, and the value must be field-content — no
/// CR, LF, or NUL, and nothing above `U+00FF`, which is what `Headers` refuses as unrepresentable as
/// bytes rather than as a control character.
///
/// # Errors
///
/// Returns [`CatalogError`] naming the header and what about it cannot be sent.
pub fn assert_valid_headers(
    provider: &str,
    headers: &BTreeMap<String, String>,
) -> Result<(), CatalogError> {
    for (name, value) in headers {
        if !is_header_name(name) || !is_header_value(value) {
            return Err(CatalogError::new(
                provider,
                format!(
                    "header \"{name}\" is not valid for Fetch; use a valid HTTP field name and a \
                     single-line value representable as bytes"
                ),
            ));
        }
    }
    Ok(())
}

/// Whether one name is an HTTP field name (an RFC 9110 `token`).
#[must_use]
pub fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Whether one value is single-line field content `Fetch` can carry.
#[must_use]
pub fn is_header_value(value: &str) -> bool {
    value.chars().all(|character| {
        // HTAB is the one control character field-content admits.
        character == '\t'
            || ('\u{20}'..='\u{7e}').contains(&character)
            // Latin-1 is representable as bytes, which is `Headers`' limit; no provider needs it, but
            // refusing it is a different decision from refusing a control character.
            || ('\u{a0}'..='\u{ff}').contains(&character)
    })
}

// =============================================================================================
// Resolution (upstream config.ts:441–551)
// =============================================================================================

/// Resolves scalar defaults and materializes each route's serviceable models.
///
/// Deferred catalog validation retains diagnostics without deleting configured routes, which is what
/// a stored-read wants: a route whose catalog changed under it must stay visible and editable. An
/// omitted map resolves to the empty, dormant route set.
///
/// # Errors
///
/// Returns [`CatalogError`] for a scalar or structural fault in either mode, and for the first
/// unserviceable route under [`Validation::Strict`].
#[allow(
    clippy::too_many_lines,
    reason = "upstream's resolveProfiles is one pass of six checks and a resolution per route; splitting it would hide the order the checks run in, which is the contract"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "the two casts are millisecond counts bounded by MAX_TIMER_DELAY_MS, far inside f64's exact integer range"
)]
pub fn resolve_profiles(
    providers: &BTreeMap<String, PiAiProviderProfile>,
    installed: &InstalledRoutes,
    validation: Validation,
) -> Result<BTreeMap<String, ResolvedPiAiProviderProfile>, CatalogError> {
    let mut resolved = BTreeMap::new();
    for (key, source) in providers {
        let provider = key.as_str();
        if provider.is_empty() {
            return Err(CatalogError::new(
                provider,
                "is empty; the providers dict is keyed by provider route, so every key must name one",
            ));
        }
        if source.base_url.as_ref().is_some_and(String::is_empty) {
            return Err(CatalogError::new(provider, "has an empty baseURL"));
        }
        if source.display_name.as_ref().is_some_and(String::is_empty) {
            return Err(CatalogError::new(provider, "has an empty displayName"));
        }
        assert_valid_headers(provider, &source.headers)?;
        // The casts are exact: both values are far below `f64`'s 2^53 integer range, so a millisecond
        // count round-trips through `f64` without loss. The type is `f64` at all because upstream
        // admits a fractional idle interval.
        let stream_idle_timeout_ms = source
            .stream_idle_timeout_ms
            .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_MS as f64);
        if !stream_idle_timeout_ms.is_finite()
            || stream_idle_timeout_ms <= 0.0
            || stream_idle_timeout_ms > MAX_TIMER_DELAY_MS as f64
        {
            return Err(CatalogError::new(
                provider,
                format!(
                    "streamIdleTimeoutMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
                ),
            ));
        }
        let max_request_image_bytes = source
            .max_request_image_bytes
            .unwrap_or(DEFAULT_MAX_REQUEST_IMAGE_BYTES);
        if max_request_image_bytes == 0 {
            return Err(CatalogError::new(
                provider,
                "maxRequestImageBytes must be a positive integer",
            ));
        }
        let max_request_images = source
            .max_request_images
            .unwrap_or(DEFAULT_MAX_REQUEST_IMAGES);
        if max_request_images == 0 {
            return Err(CatalogError::new(
                provider,
                "maxRequestImages must be a positive safe integer",
            ));
        }
        let image_offload_count_quantum = source
            .image_offload_count_quantum
            .unwrap_or_else(|| DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM.min(max_request_images));
        if image_offload_count_quantum == 0 || image_offload_count_quantum > max_request_images {
            return Err(CatalogError::new(
                provider,
                "imageOffloadCountQuantum must be a positive safe integer no greater than maxRequestImages",
            ));
        }
        let request_image_pixel_budget = source
            .request_image_pixel_budget
            .unwrap_or(DEFAULT_REQUEST_IMAGE_PIXEL_BUDGET);
        if request_image_pixel_budget == 0 {
            return Err(CatalogError::new(
                provider,
                "requestImagePixelBudget must be a positive safe integer",
            ));
        }
        let request_image_max_bytes = source
            .request_image_max_bytes
            .unwrap_or(DEFAULT_REQUEST_IMAGE_MAX_BYTES);
        if request_image_max_bytes == 0 {
            return Err(CatalogError::new(
                provider,
                "requestImageMaxBytes must be a positive safe integer",
            ));
        }
        // Detached from the configuration because a model's `input` is mutable downstream. An empty
        // list here is always one someone typed — the schema's default covers the absent key — and
        // unlike an entry's list nothing below it can answer instead, so it is refused rather than
        // read as "no answer".
        let default_input = source
            .default_input
            .clone()
            .unwrap_or_else(|| DEFAULT_INPUT.to_vec());
        if default_input.is_empty() {
            return Err(CatalogError::new(
                provider,
                "defaultInput must name at least one modality",
            ));
        }
        let api_key_env = match source.api_key_env.as_deref() {
            None => None,
            Some(reference) if is_credential_ref_name(reference) => Some(reference.to_owned()),
            Some(reference) => {
                return Err(CatalogError::new(
                    provider,
                    format!(
                        "apiKeyEnv \"{reference}\" must be an environment-variable name matching \
                         /^[A-Za-z_][A-Za-z0-9_]*$/"
                    ),
                ));
            }
        };
        // The route key, not the installed provider's own name: the directory has always shown route
        // keys, and a catalog route must not silently rename itself on every configuration surface
        // just because it gained a profile.
        let display_name = source
            .display_name
            .clone()
            .unwrap_or_else(|| provider.to_owned());
        let installed_route = installed_provider(installed, provider);
        let mut request = RouteCatalogRequest::new(provider);
        request.api.clone_from(&source.api);
        request.base_url.clone_from(&source.base_url);
        request.models.clone_from(&source.models);
        request.model_overrides.clone_from(&source.model_overrides);
        request.compat.clone_from(&source.compat);
        request.default_input.clone_from(&default_input);
        request.default_context_window = source
            .default_context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        request.default_max_tokens = source.default_max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let empty_catalog = InstalledCatalog::new();
        // A catalog fault is caught rather than propagated only in deferred mode, and when it is
        // caught the provider is *not* built: upstream's try block covers both, so a route whose
        // catalog could not materialize has nothing to serve even if its protocol is fine.
        let (models, model_errors, configured_max_tokens, catalog_error, catalog_resolved) =
            match resolve_route_models_with(
                &request,
                installed_route.map_or(&empty_catalog, |route| &route.models),
                installed_route.and_then(|route| route.base_url.as_deref()),
                validation,
            ) {
                Ok(catalog) => {
                    let first = catalog.model_errors.values().next().cloned();
                    (
                        catalog.models,
                        catalog.model_errors,
                        catalog.configured_max_tokens,
                        first,
                        true,
                    )
                }
                Err(error) if validation == Validation::Deferred => (
                    Vec::new(),
                    BTreeMap::new(),
                    BTreeMap::new(),
                    Some(error.to_string()),
                    false,
                ),
                Err(error) => return Err(error),
            };
        let pi_provider = if catalog_resolved {
            match build_provider(
                provider,
                &display_name,
                source.api.as_deref(),
                source.base_url.as_deref(),
                api_key_env.is_some(),
                installed_route,
                models,
            ) {
                Ok(built) => Some(built),
                // Unreachable by construction — a route that materialized a catalog named a protocol
                // the table serves, or kept its catalog's protocol — but a deferred read must not turn
                // an unexpected refusal into a hard failure, so it becomes a diagnostic like any other.
                Err(error) if validation == Validation::Deferred => {
                    resolved.insert(
                        provider.to_owned(),
                        assemble(
                            provider,
                            source,
                            ResolvedInputs {
                                display_name,
                                api_key_env,
                                default_input,
                                stream_idle_timeout_ms,
                                max_request_image_bytes,
                                max_request_images,
                                image_offload_count_quantum,
                                request_image_pixel_budget,
                                request_image_max_bytes,
                                catalog_error: Some(error.to_string()),
                                model_errors,
                                configured_max_tokens,
                                pi_provider: None,
                                models: Vec::new(),
                            },
                        )?,
                    );
                    continue;
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let built_models = pi_provider
            .as_ref()
            .map(|built| built.models.clone())
            .unwrap_or_default();
        resolved.insert(
            provider.to_owned(),
            assemble(
                provider,
                source,
                ResolvedInputs {
                    display_name,
                    api_key_env,
                    default_input,
                    stream_idle_timeout_ms,
                    max_request_image_bytes,
                    max_request_images,
                    image_offload_count_quantum,
                    request_image_pixel_budget,
                    request_image_max_bytes,
                    catalog_error,
                    model_errors,
                    configured_max_tokens,
                    pi_provider,
                    models: built_models,
                },
            )?,
        );
    }
    Ok(resolved)
}

/// The per-route values resolution computes before assembly.
struct ResolvedInputs {
    display_name: String,
    api_key_env: Option<String>,
    default_input: Vec<Modality>,
    stream_idle_timeout_ms: f64,
    max_request_image_bytes: u64,
    max_request_images: u64,
    image_offload_count_quantum: u64,
    request_image_pixel_budget: u64,
    request_image_max_bytes: u64,
    catalog_error: Option<String>,
    model_errors: BTreeMap<String, String>,
    configured_max_tokens: BTreeMap<String, u64>,
    pi_provider: Option<PiProvider>,
    models: Vec<CatalogModel>,
}

/// Assembles one resolved profile, resolving its retry policy last.
///
/// The retry policy is the one member whose resolution can fail after every earlier check passed, which
/// is why it is resolved here rather than in the caller: upstream resolves it inside the same object
/// literal that builds the profile, so its refusal is the same refusal.
fn assemble(
    provider: &str,
    source: &PiAiProviderProfile,
    inputs: ResolvedInputs,
) -> Result<ResolvedPiAiProviderProfile, CatalogError> {
    let retry_policy = resolve_retry_policy(
        source.retry_policy.as_ref(),
        &format!("llm-pi-ai: provider \"{provider}\" retryPolicy"),
    )
    .map_err(|error| CatalogError::new(provider, error.to_string()))?;
    Ok(ResolvedPiAiProviderProfile {
        provider: provider.to_owned(),
        display_name: inputs.display_name,
        api_key_env: inputs.api_key_env,
        api: source.api.clone(),
        base_url: source.base_url.clone(),
        default_context_window: source
            .default_context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW),
        default_max_tokens: source.default_max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        default_input: inputs.default_input,
        headers: source.headers.clone(),
        reasoning: source.reasoning,
        thinking_budgets: source.thinking_budgets,
        cache_retention: source.cache_retention,
        transport: source.transport,
        timeout_ms: source.timeout_ms,
        websocket_connect_timeout_ms: source.websocket_connect_timeout_ms,
        stream_idle_timeout_ms: inputs.stream_idle_timeout_ms,
        max_request_image_bytes: inputs.max_request_image_bytes,
        max_request_images: inputs.max_request_images,
        image_offload_count_quantum: inputs.image_offload_count_quantum,
        request_image_pixel_budget: inputs.request_image_pixel_budget,
        request_image_max_bytes: inputs.request_image_max_bytes,
        retry_policy,
        pi_provider: inputs.pi_provider,
        catalog_error: inputs.catalog_error,
        model_errors: inputs.model_errors,
        configured_max_tokens: inputs.configured_max_tokens,
        models: inputs.models,
    })
}

/// Reads a configuration value and resolves it, which is upstream's `Config` schema followed by
/// `apply`'s memo.
///
/// # Errors
///
/// Returns [`CatalogError`] from [`providers_from_json`] or [`resolve_profiles`].
pub fn resolve_profiles_from_json(
    providers: &Value,
    installed: &InstalledRoutes,
    validation: Validation,
) -> Result<BTreeMap<String, ResolvedPiAiProviderProfile>, CatalogError> {
    resolve_profiles(&providers_from_json(providers)?, installed, validation)
}

// =============================================================================================
// Serviceability (upstream config.ts:387–399)
// =============================================================================================

/// Rejects new or changed provider profiles that cannot be served.
///
/// The narrowing is the whole function. Unchanged stored profiles may need repair after a catalog
/// upgrade and do not block edits to *another* provider, so only profiles whose configuration text
/// differs from the previous resolved section are re-resolved; a removed profile is not checked at
/// all, because nothing will serve it.
///
/// # Errors
///
/// Returns [`CatalogError`] naming the route and the entry that cannot be served.
pub fn assert_serviceable(
    config: &BTreeMap<String, PiAiProviderProfile>,
    previous: Option<&BTreeMap<String, PiAiProviderProfile>>,
    installed: &InstalledRoutes,
) -> Result<(), CatalogError> {
    let mut changed = BTreeMap::new();
    for (provider, profile) in config {
        let unchanged = previous
            .and_then(|previous| previous.get(provider))
            .is_some_and(|before| before == profile);
        if !unchanged {
            changed.insert(provider.clone(), profile.clone());
        }
    }
    resolve_profiles(&changed, installed, Validation::Strict)?;
    Ok(())
}

// =============================================================================================
// The pure half of `apply` (upstream index.ts:124–337, minus the host calls)
// =============================================================================================

/// The registry facts one route contributes.
///
/// The registry captures these per route, so a change to any of them must re-register. `display_name`
/// rides along because the registry hands it to every selector: a rename that did not re-register
/// would leave the old label showing until some unrelated fact happened to change.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistrationFact {
    /// The route key.
    pub provider: String,
    /// The label selectors show.
    pub display_name: String,
    /// The policy captured at registration.
    pub retry_policy: ResolvedRetryPolicy,
}

/// The per-route facts the adapter registry captured, sorted by provider.
///
/// Sorted because a settings document that merely reorders its keys is not a route change, and
/// re-registering on a reorder would swap a live route set for an identical one.
#[must_use]
pub fn registration_facts(
    profiles: &BTreeMap<String, ResolvedPiAiProviderProfile>,
) -> Vec<RegistrationFact> {
    profiles
        .values()
        .map(ResolvedPiAiProviderProfile::registration_fact)
        .collect()
}

/// What one pass of `ensureRegistrationFacts` must do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationAction {
    /// The captured facts already match; nothing moves.
    Unchanged,
    /// A bare mount with zero routes: stay dormant, but record the (empty) facts.
    Dormant,
    /// Register the route set for the first time.
    Register,
    /// Swap the route set of an existing registration.
    Replace,
}

/// What the registry currently holds.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistrationState {
    /// The route set the registry captured.
    pub routes: Vec<String>,
    /// The facts it captured with it.
    pub facts: Vec<RegistrationFact>,
}

/// The decision `ensureRegistrationFacts` makes, and the route set to apply.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistrationPlan {
    /// What the caller must do with the registry.
    pub action: RegistrationAction,
    /// The route set to register or replace, in configuration order.
    pub routes: Vec<String>,
    /// The facts to record once the registry holds `routes`.
    pub facts: Vec<RegistrationFact>,
}

/// Decides one pass of route registration.
///
/// The order of the two tests is upstream's and matters: facts are compared *first*, so an empty
/// route set on a naked mount records its facts without registering, and an empty route set on an
/// existing registration replaces itself with nothing rather than leaving the old routes serving.
#[must_use]
pub fn plan_registration(
    profiles: &BTreeMap<String, ResolvedPiAiProviderProfile>,
    state: Option<&RegistrationState>,
) -> RegistrationPlan {
    let facts = registration_facts(profiles);
    let routes: Vec<String> = profiles.keys().cloned().collect();
    let action = match state {
        Some(state) if state.facts == facts => RegistrationAction::Unchanged,
        Some(_) => RegistrationAction::Replace,
        None if routes.is_empty() => RegistrationAction::Dormant,
        None => RegistrationAction::Register,
    };
    RegistrationPlan {
        action,
        routes,
        facts,
    }
}

/// One entry of the configurable-provider directory.
///
/// A hand-declared route has no catalog entry, so without the union of installed and declared routes
/// it would have no settings address and configuration surfaces could neither show nor edit it. A
/// catalog route also carries its catalog endpoint and native authentication, which a settings editor
/// shows while the profile leaves the endpoint or the credential reference unset.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryEntry {
    /// The route key.
    pub provider: String,
    /// The label selectors show.
    pub display_name: String,
    /// The settings namespace the profile lives under.
    pub settings_ns: String,
    /// The settings path, always `["providers", <route>]`.
    pub settings_path: (String, String),
    /// Whether the route exists only because configuration declared it.
    ///
    /// Membership of the *installed catalog*, not of the settings document: narrowing a shipped
    /// provider's models stores a profile too, and that route is still one pi-ai knows.
    pub declared: bool,
    /// The provider's own credential variables and endpoint, when the directory records it.
    pub defaults: ProviderDefaults,
    /// The route's catalog diagnostic, when its profile carries one.
    pub error: Option<String>,
}

/// The directory entries for the current profiles: installed catalog order, declared routes last.
///
/// # Errors
///
/// Never; the signature is fallible only through [`provider_defaults_of`]'s absence of errors, which
/// is why this returns a plain vector.
#[must_use]
pub fn directory_entries(
    profiles: &BTreeMap<String, ResolvedPiAiProviderProfile>,
    installed: &InstalledRoutes,
    settings_ns: &str,
) -> Vec<DirectoryEntry> {
    fn declare(
        entries: &mut Vec<DirectoryEntry>,
        installed: &InstalledRoutes,
        settings_ns: &str,
        provider: &str,
        display_name: &str,
        error: Option<&str>,
    ) {
        let catalog = installed.get(provider);
        let entry = DirectoryEntry {
            provider: provider.to_owned(),
            display_name: display_name.to_owned(),
            settings_ns: settings_ns.to_owned(),
            settings_path: ("providers".to_owned(), provider.to_owned()),
            declared: catalog.is_none(),
            defaults: provider_defaults_of(
                provider,
                catalog.and_then(|route| route.base_url.as_deref()),
                catalog.is_some_and(|route| route.oauth),
            ),
            error: error.map(str::to_owned),
        };
        // Replacing in place rather than pushing: a profile for an installed route must keep the
        // catalog's position, or the directory would reorder itself every time a route gained a
        // profile — and the two facts that identify an entry (route key, settings path) are the
        // same ones, so "the same entry with a new label" is what this is.
        match entries
            .iter_mut()
            .find(|existing| existing.provider == provider)
        {
            Some(existing) => *existing = entry,
            None => entries.push(entry),
        }
    }
    let mut entries = Vec::new();
    // The *installed catalog*, not the generated provider-data table: a route pi-ai ships under an id
    // the table does not record still has models to offer, and the table's 42 entries are the ones
    // that additionally carry endpoint and env-var metadata.
    for provider in installed.keys() {
        declare(
            &mut entries,
            installed,
            settings_ns,
            provider,
            provider,
            None,
        );
    }
    for (provider, profile) in profiles {
        declare(
            &mut entries,
            installed,
            settings_ns,
            provider,
            &profile.display_name,
            profile.catalog_error.as_deref(),
        );
    }
    entries
}

/// The stored half of one discovery request.
///
/// Discovery interrogates an endpoint over a draft, so the request does not carry the stored
/// credential or the deployment-owned headers: the curated UI accepts neither. An already-configured
/// route supplies both inside the host, which is what this type hands it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDiscoveryProfile {
    /// The route's configured headers.
    pub headers: BTreeMap<String, String>,
    /// The credential reference the host must resolve.
    pub api_key_env: Option<String>,
}

/// The stored discovery profile for one route, when it has one.
#[must_use]
pub fn stored_discovery_profile(
    profiles: &BTreeMap<String, ResolvedPiAiProviderProfile>,
    provider: Option<&str>,
) -> Option<StoredDiscoveryProfile> {
    let profile = profiles.get(provider?)?;
    Some(StoredDiscoveryProfile {
        headers: profile.headers.clone(),
        api_key_env: profile.api_key_env.clone(),
    })
}

// =============================================================================================
// Credentials (upstream index.ts:208–231, the decision rather than the lookup)
// =============================================================================================

/// Canonical code: the profile names a credential the seam does not hold.
pub const MISSING_CREDENTIAL: &str = "MISSING_CREDENTIAL";

/// Canonical code: a credential was found but no request can carry it.
pub const INVALID_CREDENTIAL: &str = "INVALID_CREDENTIAL";

/// Why one route's credential is unusable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialFault {
    /// The canonical code, one of [`MISSING_CREDENTIAL`] or [`INVALID_CREDENTIAL`].
    pub code: &'static str,
    /// The user-visible message, which must name no secret.
    pub message: String,
}

impl fmt::Display for CredentialFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CredentialFault {}

/// Decides what one route's credential resolution means.
///
/// `reference` is the profile's `apiKeyEnv` and `resolved` whatever the seam found for it — the
/// Cordis `ctx.credentials` lookup, or the launching environment in a composition that mounts no
/// credentials seam. Splitting the decision from the lookup is what makes the two failures
/// distinguishable in a test: only a profile that names no credential at all defers to pi-ai's own
/// ambient discovery, and once one is named a miss must fail loud. Handing pi-ai `None` would let it
/// pick up an unrelated ambient key (`OPENAI_API_KEY` and friends), billing another tenant for a
/// request the deployment meant to authenticate differently.
///
/// # Errors
///
/// Returns [`CredentialFault`] with [`MISSING_CREDENTIAL`] when a named reference did not resolve (or
/// resolved to nothing), and with [`INVALID_CREDENTIAL`] when the value found is blank or carries a
/// character no HTTP header can.
pub fn resolve_route_credential(
    reference: Option<&str>,
    resolved: Option<&str>,
    provider: &str,
) -> Result<Option<String>, CredentialFault> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    // A miss and an empty hit are the same fault: something was configured and nothing is there.
    if let Some(value) = resolved.filter(|value| !value.is_empty()) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(CredentialFault {
                code: INVALID_CREDENTIAL,
                message: format!(
                    "{NS}: the API key resolved from {reference} is blank; set {reference} to the raw \
                     key (the web Models page writes it) or export it in the launching environment"
                ),
            });
        }
        if !trimmed.chars().all(is_legal_api_key_char) {
            return Err(CredentialFault {
                code: INVALID_CREDENTIAL,
                message: format!(
                    "{NS}: the API key resolved from {reference} contains characters no HTTP header \
                     can carry; set {reference} to the raw key alone (the web Models page writes it)"
                ),
            });
        }
        return Ok(Some(trimmed.to_owned()));
    }
    Err(CredentialFault {
        code: MISSING_CREDENTIAL,
        message: format!(
            "{NS}: no credential for provider route \"{provider}\"; its profile resolves \
             {reference}, which is not set — store {reference} through the credentials service \
             (the web Models page writes it) or export it, and remove apiKeyEnv only if this \
             provider should authenticate from pi-ai's own environment discovery"
        ),
    })
}

/// Whether one character may appear in an API key.
///
/// `^[\x21-\x7E]+$`: printable ASCII, space excluded. That is a transport invariant rather than one
/// provider's policy — `Fetch` refuses to build a header from anything else — and Latin-1 is excluded
/// deliberately, because admitting it would trade a local explained refusal for an opaque 401.
#[must_use]
pub fn is_legal_api_key_char(character: char) -> bool {
    ('\u{21}'..='\u{7e}').contains(&character)
}

/// Whether one provider id can address a credential record.
///
/// This is the gate `registerPiAiFlows` applies before offering a sign-in: a credential key is
/// `<scope>/<id>` with both halves matching `^[a-z][a-z0-9-]*$`, so a future catalog provider with a
/// dotted or uppercase id would throw while addressing its record and take the whole mount down.
/// The flows themselves are the host's; the gate is not.
#[must_use]
pub fn can_address_credential_record(provider_id: &str) -> bool {
    let mut bytes = provider_id.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The credential record key for one provider route — upstream `recordKeyFor`.
///
/// # Errors
///
/// Returns `None` when either half cannot be a credential key segment; see
/// [`can_address_credential_record`].
#[must_use]
pub fn record_key_for(provider_id: &str) -> Option<String> {
    if can_address_credential_record(NS) && can_address_credential_record(provider_id) {
        Some(format!("{NS}/{provider_id}"))
    } else {
        None
    }
}

/// The providers the installed catalog ships that offer a sign-in, in directory order.
///
/// This is `registerPiAiFlows`'s *domain* — which providers would be offered — with the registration
/// itself left to the host. A provider whose id cannot address a credential record is excluded
/// rather than registered and failing at the first prompt.
#[must_use]
pub fn login_offerable_providers(installed: &InstalledRoutes) -> Vec<&str> {
    installed
        .iter()
        .filter(|(_, route)| route.oauth)
        .map(|(provider, _)| provider.as_str())
        .filter(|provider| can_address_credential_record(provider))
        .collect()
}
