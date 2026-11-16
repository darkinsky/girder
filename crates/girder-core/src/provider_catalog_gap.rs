//! Provider directory, compat gates, route-catalog materialization, and model-listing parsing.
//!
//! Upstream: `packages/llm/llm-pi-ai/src/` — 4,638 lines across 16 files. An earlier round reported
//! that package as "multi-provider catalog and discovery, entirely absent locally, needs network and
//! filesystem mechanisms, so it is an integration task". That judgement is half right, and the half
//! it gets wrong is the larger one: most of the package's *bulk* is data and pure functions, and the
//! parts that reach the outside world are the parts this build already has another way.
//!
//! # The package, file by file, with its IO dependency
//!
//! | File | Lines | What it is | IO |
//! | --- | --- | --- | --- |
//! | `provider-data.generated.ts` | 52 | 42 providers: env names, default endpoint, endpoint-env names | **pure data** |
//! | `provider-data.ts` | 19 | `providerDefaultsOf`: derives display metadata from that table | **pure** |
//! | `catalog.ts` | 987 | vocabulary gates, compat gates, `resolveRouteModels` materialization | **pure** |
//! | `discovery.ts` | 418 | listing-URL derivation and the two listing parsers | `fetch` + body read; **parsers pure** |
//! | `models.ts` | 92 | thinking levels from model metadata, transcript-update modes | **pure** |
//! | `prefix-binding.ts` | 110 | digest chain over the replayed prefix | `node:crypto` SHA-256 |
//! | `provider.ts` | 193 | protocol table + pi-ai provider construction | needs pi-ai runtime |
//! | `config.ts` | 551 | `schemastery` config schema, defaults, header validation | needs `schemastery` |
//! | `adapter.ts` | 515 | the request path onto pi-ai | network + pi-ai runtime |
//! | `context.ts` | 411 | message conversion, image offload | pi-ai runtime |
//! | `replay.ts` | 277 | replayed-thinking envelopes | pi-ai runtime |
//! | `stream.ts` | 254 | provider stream events to harness chunks | network stream |
//! | `auth.ts` | 231 | credential store over `~/.config`, `process.env` | **filesystem** |
//! | `login.ts` | 172 | interactive OAuth flows | network + terminal |
//! | `index.ts` | 346 | cordis plugin registration | plugin host |
//! | `types.ts` | 10 | one type alias | type-only |
//!
//! So the report's premise holds for exactly three files — `auth.ts`, `login.ts`, and the network half
//! of `discovery.ts` — plus the five that are thin skins over the `pi-ai` runtime. Everything in the
//! "pure" column above is data, table lookups, string formatting, and one structural merge, and none
//! of it was present in this build.
//!
//! # What this build already had, and what it did not
//!
//! [`crate::providers`] (510 lines) is a route registry: [`crate::Route`], [`crate::ModelInfo`], and
//! the resolution of `(route, model)` to one catalogue entry. It knows about *mounted* routes and
//! nothing about the provider universe they are mounted from — there is no provider directory in it,
//! no endpoint table, no protocol vocabulary. [`crate::model_catalog_extra`] (223 lines) declares
//! what one model can do and filters a catalog by requirement; it takes the catalog as a slice and
//! never asks where a catalog came from.
//!
//! Two concrete gaps follow, and both are pure-data gaps:
//!
//! 1. **There is no provider directory at all.** A route naming `groq` or `together` has to restate
//!    an endpoint, and a deployment cannot discover that `GROQ_API_KEY` is the variable a `groq`
//!    route should read. Upstream carries 42 providers and their environment-variable names.
//! 2. **Nothing materializes a route's catalog.** Upstream merges the installed catalog, a route's
//!    model list, its per-model overrides, and its compat switches field by field with a documented
//!    precedence chain, refusing what cannot be served. This build has the *result* type
//!    ([`crate::ModelInfo`]) and no *producer* — every route is hand-assembled at its mount site,
//!    which is why `deepseek_route` hard-codes `64_000` / `8_192`.
//!
//! # What this module implements
//!
//! - [`provider_data`] / [`provider_defaults_of`]: the 42-provider directory and the derivation of
//!   `nativeAuth`, `defaultBaseURL`, and `baseURLEnv` from it, including the unrecorded-provider
//!   fallback.
//! - The vocabulary gates: [`MODALITIES`], [`THINKING_LEVELS`], [`SUPPORTED_THINKING_FORMATS`],
//!   [`MAX_TOKENS_FIELDS`], [`THINKING_TOKEN_BUDGET_FIELDS`], [`CACHE_CONTROL_FORMATS`],
//!   [`CHAT_TEMPLATE_VARS`], [`SUPPORTED_PROTOCOLS`].
//! - The compat gates: per-protocol `offer`/`withhold` disposition of every upstream compat field,
//!   with [`compat_protocols`], [`offered_compat_fields`], and [`all_offered_compat_fields`].
//! - [`resolve_route_models`]: the whole materialization, strict and deferred, with upstream's
//!   diagnostics.
//! - [`get_supported_thinking_levels`] and [`conversation_updates`] from `models.ts`.
//! - [`listing_url`], [`read_listing`], and [`read_gemini_listing`] from `discovery.ts`: everything
//!   that happens to a model listing *after* the bytes arrive.
//!
//! # What this module deliberately does not do
//!
//! - **No HTTP.** [`read_listing`] takes a parsed JSON value; the request that produced it is the
//!   caller's. `discovery.ts`'s `fetch`, the 4 MiB bounded body read, the abort plumbing, and the
//!   four header shapes (`authorization`, `x-api-key` + `anthropic-version`, `x-goog-api-key`) stay
//!   IO, and this build already owns an HTTP client for its two adapters.
//! - **No filesystem credential store.** `auth.ts` reads `~/.config/...` and `process.env`; that is a
//!   binding, not a translation, and [`provider_data`] already hands the caller the variable names so
//!   it can bind them itself.
//! - **No interactive OAuth.** `login.ts` runs a device-code flow against a terminal and a browser.
//! - **No pi-ai runtime, protocol implementations, or plugin registration.** `adapter.ts`,
//!   `context.ts`, `replay.ts`, `stream.ts`, `provider.ts`, and `index.ts` are translations of a
//!   dependency this port does not have ([`crate::llm_openai`] and [`crate::llm_deepseek`] are this
//!   build's own request paths instead).
//! - **No `prefix-binding.ts`.** It is a SHA-256 chain over pi-ai context objects that only exist if
//!   pi-ai does; the digest binds to a serialization this port does not share, so porting the hash
//!   would produce values nothing could compare against.
//! - **No `schemastery` schema.** `config.ts` is a declarative validation DSL for one TypeScript
//!   library; the *defaults* it declares are here as [`DEFAULT_CONTEXT_WINDOW`],
//!   [`DEFAULT_MAX_TOKENS`], and [`DEFAULT_INPUT`], and the *validation* it performs on profiles is
//!   here as the resolution diagnostics.
//!
//! # Why the installed catalog is a parameter
//!
//! Upstream's `resolveRouteModels` calls `catalogModels(provider)` and `catalogProvider(provider)`,
//! which read pi-ai's statically-linked provider registry — pure lookups, injected through an import.
//! This port has no such registry, so the two facts it supplies — the provider's models by id, and
//! the provider's own endpoint — are parameters ([`InstalledCatalog`]). That is what makes the
//! whole function testable without a single provider installed, and it is also the honest boundary:
//! a caller that *does* have a directory passes it, and a caller that does not can still materialize
//! a hand-declared route.
//!
//! # The precedence chains, stated once
//!
//! Every field of a materialized model resolves through a fixed chain; the whole module is these
//! four chains plus the diagnostics for the cases where a chain cannot finish:
//!
//! | Field | First | Then | Finally |
//! | --- | --- | --- | --- |
//! | `api` | route's `api` | installed entry | route's shared catalog api |
//! | `baseUrl` | route's `baseURL` | installed entry | installed provider |
//! | `contextWindow` / `maxTokens` | entry | installed entry | route's `default*` |
//! | `input` | entry (non-empty) | installed entry | route's `defaultInput` |
//! | `name` | entry | installed entry | the id itself |
//! | `reasoning` + level map | entry's declared efforts | installed entry | non-reasoning |
//! | `compat` | route switches, then entry switches | installed entry | nothing |
//!
//! The last one has the one non-obvious rule in the module: the installed entry's compat travels
//! with the installed entry's *api*, so a route that repoints a catalog model at another protocol
//! does not inherit a compat block written for the old one.
//!
//! # Strict and deferred resolution
//!
//! [`Validation::Strict`] rejects the first unserviceable model, which is what a settings write
//! wants: storing a profile that cannot serve is how a session ends up unable to send anything.
//! [`Validation::Deferred`] retains each model's diagnostic in [`RouteCatalog::model_errors`] and
//! keeps serving the rest, which is what a settings *read* wants: catalog drift after an upgrade
//! must not make the profile unreadable, or unrepairable. A route-level fault — a compat switch no
//! model on the route could read — is fatal in both modes, because no subset of the route makes it
//! correct. So is a route that resolves *no* serviceable model: returning an empty catalog would
//! present as "this provider is configured" until the first request failed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

// =============================================================================================
// Vocabulary
// =============================================================================================

/// One request modality a model may accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modality {
    /// Text input.
    Text,
    /// Image input.
    Image,
}

impl Modality {
    /// The wire spelling upstream uses for this modality.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
        }
    }

    /// Reads a wire spelling, or `None` when it names no modality this port serves.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "text" => Some(Self::Text),
            "image" => Some(Self::Image),
            _ => None,
        }
    }
}

/// Every request modality a profile may declare, in upstream's gate order.
pub const MODALITIES: [Modality; 2] = [Modality::Text, Modality::Image];

/// The modalities assumed for a model neither configuration nor the installed catalog declares.
///
/// Text is the floor every supported protocol certainly carries, so this is the absence of a
/// declaration rather than a guess at the endpoint. The two wrong answers do not cost the same:
/// under-claiming refuses the image before it is attached, naming the model, while over-claiming
/// admits one the provider then rejects mid-turn, after the message is durable.
pub const DEFAULT_INPUT: [Modality; 1] = [Modality::Text];

/// One reasoning level, in upstream's escalation order.
///
/// The declaration order *is* the escalation order: `Ord` is derived, so a `BTreeMap` keyed by this
/// type iterates the levels the way a selector must present them, and no sort call can disagree
/// with the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThinkingLevel {
    /// Reasoning off.
    Off,
    /// The smallest budget providers offer.
    Minimal,
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// The extra-high level pi-ai treats asymmetrically — see [`get_supported_thinking_levels`].
    Xhigh,
    /// The maximum level, equally asymmetric.
    Max,
}

impl ThinkingLevel {
    /// The wire spelling upstream uses for this level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Reads a wire spelling, or `None` when it names no level.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every thinking level a profile may declare, in escalation order.
pub const THINKING_LEVELS: [ThinkingLevel; 7] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Xhigh,
    ThinkingLevel::Max,
];

/// Every reasoning-dispatch wire format a profile may name, most-reached first.
pub const SUPPORTED_THINKING_FORMATS: [&str; 11] = [
    "openai",
    "deepseek",
    "openrouter",
    "together",
    "baseten",
    "zai",
    "qwen",
    "chat-template",
    "qwen-chat-template",
    "string-thinking",
    "ant-ling",
];

/// The output-cap field spellings a profile may name.
pub const MAX_TOKENS_FIELDS: [&str; 2] = ["max_completion_tokens", "max_tokens"];

/// The reasoning-budget field spellings a profile may name.
pub const THINKING_TOKEN_BUDGET_FIELDS: [&str; 3] = [
    "thinking_token_budget",
    "thinking_budget",
    "thinking_budget_tokens",
];

/// The prompt-cache marker conventions a profile may name.
pub const CACHE_CONTROL_FORMATS: [&str; 1] = ["anthropic"];

/// The request-state placeholders a `chat_template_kwargs` value may name.
pub const CHAT_TEMPLATE_VARS: [&str; 3] =
    ["thinking.enabled", "thinking.effort", "thinking.budget"];

/// Every wire protocol a configured route may name, most-reached first.
///
/// The order is upstream's protocol table, and it is load-bearing: a configuration surface presents
/// the first as its default, which is why the protocol a hand-declared gateway most often speaks
/// leads.
pub const SUPPORTED_PROTOCOLS: [&str; 4] = [
    "openai-completions",
    "openai-responses",
    "anthropic-messages",
    "google-generative-ai",
];

/// The protocols whose model listing [`read_listing`] can read.
///
/// `Azure` is absent despite its `OpenAI` lineage — it authenticates with an `api-key` header and
/// requires an `api-version` query — and so is Codex, which authenticates through OAuth. Listing
/// either as if it were plain `OpenAI` would report an authentication failure as a provider with no
/// models.
pub const LISTABLE_PROTOCOLS: [&str; 4] = [
    "anthropic-messages",
    "google-generative-ai",
    "openai-completions",
    "openai-responses",
];

/// The protocol a draft that has not chosen one is probed with.
pub const DEFAULT_PROBE_PROTOCOL: &str = "openai-completions";

/// The `anthropic-version` a Messages listing request must carry.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The largest page `Anthropic`'s public model-listing endpoint accepts.
pub const ANTHROPIC_MODEL_LIMIT: u32 = 1000;

/// The largest page the `Gemini` model-listing endpoint accepts.
pub const GEMINI_MODEL_LIMIT: u32 = 1000;

/// The largest listing reply this build will parse, in bytes.
///
/// The endpoint is whatever URL a caller typed, so the ceiling holds on the bytes actually read
/// rather than on the length the server claims. A truncated listing is not parseable, so overflow
/// rejects instead of truncating.
pub const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Context capacity assumed for a model neither configuration nor the installed catalog sizes.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;

/// Output capability assumed for a model neither configuration nor the installed catalog sizes.
pub const DEFAULT_MAX_TOKENS: u64 = 128_000;

// =============================================================================================
// Provider directory
// =============================================================================================

/// What the generated provider directory records about one provider.
///
/// Endpoints and environment *names* only: no credential is evaluated, and nothing here reads the
/// environment. A caller resolves a name through its own lookup and reports what it found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderData {
    /// Environment variables a credential may arrive in, most-preferred first.
    pub env: &'static [&'static str],
    /// The endpoint this provider serves when a route does not override it.
    pub default_base_url: Option<&'static str>,
    /// Environment variables that may carry the endpoint itself, most-preferred first.
    pub base_url_env: &'static [&'static str],
}

impl ProviderData {
    /// A provider whose endpoint comes only from its environment.
    const fn endpoint_from_env(
        env: &'static [&'static str],
        base_url_env: &'static [&'static str],
    ) -> Self {
        Self {
            env,
            default_base_url: None,
            base_url_env,
        }
    }

    /// A provider with a fixed endpoint and no endpoint override.
    const fn fixed(env: &'static [&'static str], default_base_url: &'static str) -> Self {
        Self {
            env,
            default_base_url: Some(default_base_url),
            base_url_env: &[],
        }
    }

    /// A provider that names a credential but publishes no endpoint this table can state.
    const fn credentials_only(env: &'static [&'static str]) -> Self {
        Self {
            env,
            default_base_url: None,
            base_url_env: &[],
        }
    }
}

/// The provider directory, sorted by id so [`provider_data`] can binary-search it.
///
/// Copied from upstream's `provider-data.generated.ts`, which is itself generated from the installed
/// `pi-ai` catalog. The generation step is not ported — a build with no `pi-ai` installed has
/// nothing to generate from — so this table is the ported *artifact*.
const PROVIDERS: &[(&str, ProviderData)] = &[
    (
        "amazon-bedrock",
        ProviderData::endpoint_from_env(
            &[
                "AWS_BEARER_TOKEN_BEDROCK",
                "AWS_PROFILE",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "AWS_WEB_IDENTITY_TOKEN_FILE",
            ],
            &["AWS_REGION", "AWS_DEFAULT_REGION"],
        ),
    ),
    (
        "ant-ling",
        ProviderData::fixed(&["ANT_LING_API_KEY"], "https://api.ant-ling.com/v1"),
    ),
    (
        "anthropic",
        ProviderData::fixed(
            &[
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_OAUTH_TOKEN",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_FEDERATION_RULE_ID",
                "ANTHROPIC_ORGANIZATION_ID",
                "ANTHROPIC_IDENTITY_TOKEN_FILE",
                "ANTHROPIC_SERVICE_ACCOUNT_ID",
                "ANTHROPIC_WORKSPACE_ID",
            ],
            "https://api.anthropic.com",
        ),
    ),
    (
        "azure",
        ProviderData::endpoint_from_env(
            &["AZURE_OPENAI_API_KEY"],
            &["AZURE_OPENAI_BASE_URL", "AZURE_OPENAI_RESOURCE_NAME"],
        ),
    ),
    (
        "baseten",
        ProviderData::fixed(&["BASETEN_API_KEY"], "https://inference.baseten.co/v1"),
    ),
    (
        "cerebras",
        ProviderData::fixed(&["CEREBRAS_API_KEY"], "https://api.cerebras.ai/v1"),
    ),
    (
        "cloudflare-ai-gateway",
        ProviderData::endpoint_from_env(
            &[
                "CLOUDFLARE_API_KEY",
                "CLOUDFLARE_ACCOUNT_ID",
                "CLOUDFLARE_GATEWAY_ID",
            ],
            &["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"],
        ),
    ),
    (
        "cloudflare-workers-ai",
        ProviderData::endpoint_from_env(
            &["CLOUDFLARE_API_KEY", "CLOUDFLARE_ACCOUNT_ID"],
            &["CLOUDFLARE_ACCOUNT_ID"],
        ),
    ),
    (
        "deepseek",
        ProviderData::fixed(&["DEEPSEEK_API_KEY"], "https://api.deepseek.com"),
    ),
    (
        "fireworks",
        ProviderData::fixed(&["FIREWORKS_API_KEY"], "https://api.fireworks.ai/inference"),
    ),
    (
        "github-copilot",
        ProviderData::fixed(
            &["COPILOT_GITHUB_TOKEN"],
            "https://api.individual.githubcopilot.com",
        ),
    ),
    (
        "google",
        ProviderData::fixed(
            &["GEMINI_API_KEY"],
            "https://generativelanguage.googleapis.com/v1beta",
        ),
    ),
    (
        "google-vertex",
        ProviderData::endpoint_from_env(
            &[
                "GOOGLE_CLOUD_API_KEY",
                "GOOGLE_APPLICATION_CREDENTIALS",
                "GOOGLE_CLOUD_PROJECT",
                "GCLOUD_PROJECT",
                "GOOGLE_CLOUD_LOCATION",
            ],
            &[
                "GOOGLE_CLOUD_PROJECT",
                "GCLOUD_PROJECT",
                "GOOGLE_CLOUD_LOCATION",
            ],
        ),
    ),
    (
        "groq",
        ProviderData::fixed(&["GROQ_API_KEY"], "https://api.groq.com/openai/v1"),
    ),
    (
        "huggingface",
        ProviderData::fixed(&["HF_TOKEN"], "https://router.huggingface.co/v1"),
    ),
    (
        "kimi-coding",
        ProviderData::fixed(&["KIMI_API_KEY"], "https://api.kimi.com/coding/v1"),
    ),
    (
        "meta",
        ProviderData::fixed(&["META_API_KEY"], "https://api.meta.ai/v1"),
    ),
    (
        "minimax",
        ProviderData::fixed(&["MINIMAX_API_KEY"], "https://api.minimax.io/anthropic"),
    ),
    (
        "minimax-cn",
        ProviderData::fixed(
            &["MINIMAX_CN_API_KEY"],
            "https://api.minimaxi.com/anthropic",
        ),
    ),
    (
        "mistral",
        ProviderData::fixed(&["MISTRAL_API_KEY"], "https://api.mistral.ai"),
    ),
    (
        "moonshotai",
        ProviderData::fixed(&["MOONSHOT_API_KEY"], "https://api.moonshot.ai/v1"),
    ),
    (
        "moonshotai-cn",
        ProviderData::fixed(&["MOONSHOT_API_KEY"], "https://api.moonshot.cn/v1"),
    ),
    (
        "nvidia",
        ProviderData::fixed(&["NVIDIA_API_KEY"], "https://integrate.api.nvidia.com/v1"),
    ),
    (
        "openai",
        ProviderData::fixed(&["OPENAI_API_KEY"], "https://api.openai.com/v1"),
    ),
    (
        "openai-codex",
        ProviderData::fixed(&[], "https://chatgpt.com/backend-api"),
    ),
    (
        "opencode",
        ProviderData::credentials_only(&["OPENCODE_API_KEY"]),
    ),
    (
        "opencode-go",
        ProviderData::credentials_only(&["OPENCODE_API_KEY"]),
    ),
    (
        "openrouter",
        ProviderData::fixed(&["OPENROUTER_API_KEY"], "https://openrouter.ai/api/v1"),
    ),
    (
        "qwen-token-plan",
        ProviderData::fixed(
            &["QWEN_TOKEN_PLAN_API_KEY"],
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        ),
    ),
    (
        "qwen-token-plan-cn",
        ProviderData::fixed(
            &["QWEN_TOKEN_PLAN_CN_API_KEY"],
            "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        ),
    ),
    (
        "qwen-token-plan-individual",
        ProviderData::fixed(
            &["QWEN_TOKEN_PLAN_API_KEY"],
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        ),
    ),
    (
        "radius",
        ProviderData::fixed(&["RADIUS_API_KEY"], "https://radius.pi.dev/v1"),
    ),
    (
        "together",
        ProviderData::fixed(&["TOGETHER_API_KEY"], "https://api.together.ai/v1"),
    ),
    (
        "typesafe",
        ProviderData::credentials_only(&["TYPESAFE_API_KEY"]),
    ),
    (
        "vercel-ai-gateway",
        ProviderData::fixed(&["AI_GATEWAY_API_KEY"], "https://ai-gateway.vercel.sh"),
    ),
    (
        "xai",
        ProviderData::fixed(&["XAI_API_KEY"], "https://api.x.ai/v1"),
    ),
    (
        "xiaomi",
        ProviderData::fixed(&["XIAOMI_API_KEY"], "https://api.xiaomimimo.com/v1"),
    ),
    (
        "xiaomi-token-plan-ams",
        ProviderData::fixed(
            &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
            "https://token-plan-ams.xiaomimimo.com/v1",
        ),
    ),
    (
        "xiaomi-token-plan-cn",
        ProviderData::fixed(
            &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
            "https://token-plan-cn.xiaomimimo.com/v1",
        ),
    ),
    (
        "xiaomi-token-plan-sgp",
        ProviderData::fixed(
            &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
            "https://token-plan-sgp.xiaomimimo.com/v1",
        ),
    ),
    (
        "zai",
        ProviderData::fixed(&["ZAI_API_KEY"], "https://api.z.ai/api/coding/paas/v4"),
    ),
    (
        "zai-coding-cn",
        ProviderData::fixed(
            &["ZAI_CODING_CN_API_KEY"],
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ),
    ),
];

/// How many providers the directory records.
pub const PROVIDER_COUNT: usize = PROVIDERS.len();

/// The directory entry for one provider id, or `None` when the directory records none.
///
/// `None` is not a failure: a consumer may install a newer provider catalog than this table was
/// generated from, and an unrecorded provider stays configurable through
/// [`provider_defaults_of`]'s fallback.
#[must_use]
pub fn provider_data(id: &str) -> Option<&'static ProviderData> {
    PROVIDERS
        .binary_search_by(|(known, _)| (*known).cmp(id))
        .ok()
        .map(|index| &PROVIDERS[index].1)
}

/// Every recorded provider id, in directory order.
#[must_use]
pub fn provider_ids() -> Vec<&'static str> {
    PROVIDERS.iter().map(|(id, _)| *id).collect()
}

/// How a provider authenticates natively.
///
/// Names only. Whether the named variables are set is the caller's question — answering it here
/// would make a pure function read the environment, which is exactly the coupling this module
/// exists to avoid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeAuth {
    /// Environment variables a credential may arrive in, most-preferred first.
    pub env: Vec<String>,
    /// Whether the provider also offers an interactive `OAuth` flow.
    pub oauth: bool,
}

/// What a configuration surface may offer for one provider, derived without touching the network.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderDefaults {
    /// The provider's own credential variables, when the directory records the provider.
    pub native_auth: Option<NativeAuth>,
    /// The endpoint to prefill, when anything states one.
    pub default_base_url: Option<String>,
    /// Environment variables that may carry the endpoint instead.
    pub base_url_env: Vec<String>,
}

/// Derives display metadata for one provider from the directory.
///
/// The fallback is the interesting half. A provider the directory does not record — because the
/// consumer installed a newer catalog than this table was generated from — is still configurable:
/// its own `baseUrl` is offered when it is a fixed endpoint rather than a template, since a template
/// such as `https://{account}.example.com` is not something a settings field can prefill. No
/// `nativeAuth` is guessed in that case, because a guessed variable name is worse than none: it
/// looks configured and resolves to nothing.
///
/// `id` is what the directory is keyed by; `catalog_base_url` is the provider's own endpoint from the
/// caller's catalog, when it has one; `has_oauth` says whether the provider offers an interactive
/// `OAuth` flow.
#[must_use]
pub fn provider_defaults_of(
    id: &str,
    catalog_base_url: Option<&str>,
    has_oauth: bool,
) -> ProviderDefaults {
    match provider_data(id) {
        Some(data) => ProviderDefaults {
            native_auth: Some(NativeAuth {
                env: data.env.iter().map(|name| (*name).to_owned()).collect(),
                oauth: has_oauth,
            }),
            default_base_url: data.default_base_url.map(str::to_owned),
            base_url_env: data
                .base_url_env
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        },
        None => ProviderDefaults {
            native_auth: None,
            default_base_url: catalog_base_url
                .filter(|url| !url.contains('{'))
                .map(str::to_owned),
            base_url_env: Vec::new(),
        },
    }
}

// =============================================================================================
// Compat gates
// =============================================================================================

/// Whether a compat field may be configured on a protocol, or is reserved for the vendor catalog.
///
/// `withhold` is the disposition for a field `pi-ai`'s installed catalog already sets for a named
/// vendor. Reaching for one of those on a hand-declared route means configuring a provider that
/// should have been named as a catalog route instead, where the installed entry carries the right
/// value already — so the offer never presents it, and resolution refuses it by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// A profile may set this field on this protocol.
    Offer,
    /// A profile may not; the installed catalog entry owns it.
    Withhold,
}

const COMPLETIONS_GATE: &[(&str, Disposition)] = &[
    ("supportsStore", Disposition::Offer),
    ("supportsDeveloperRole", Disposition::Offer),
    ("supportsReasoningEffort", Disposition::Offer),
    ("supportsUsageInStreaming", Disposition::Offer),
    ("supportsFinishReason", Disposition::Offer),
    ("maxTokensField", Disposition::Offer),
    ("requiresToolResultName", Disposition::Offer),
    ("requiresAssistantAfterToolResult", Disposition::Offer),
    ("requiresThinkingAsText", Disposition::Offer),
    (
        "requiresReasoningContentOnAssistantMessages",
        Disposition::Offer,
    ),
    ("thinkingFormat", Disposition::Offer),
    ("chatTemplateKwargs", Disposition::Offer),
    ("chatTemplateArgs", Disposition::Offer),
    ("supportsThinkingTokenBudget", Disposition::Offer),
    ("thinkingTokenBudgetField", Disposition::Offer),
    ("vllmPriority", Disposition::Offer),
    ("supportsStrictMode", Disposition::Offer),
    ("cacheControlFormat", Disposition::Offer),
    ("supportsLongCacheRetention", Disposition::Offer),
    ("openRouterRouting", Disposition::Withhold),
    ("vercelGatewayRouting", Disposition::Withhold),
    ("zaiToolStream", Disposition::Withhold),
    ("supportsOpenAIGrammarTools", Disposition::Withhold),
    ("sendSessionAffinityHeaders", Disposition::Withhold),
    ("supportsMidConvoSystemMessages", Disposition::Withhold),
    ("supportsMidConvoToolAdditions", Disposition::Withhold),
    ("sessionAffinityFormat", Disposition::Withhold),
];

const RESPONSES_GATE: &[(&str, Disposition)] = &[
    ("supportsDeveloperRole", Disposition::Offer),
    ("supportsMaxOutputTokens", Disposition::Offer),
    ("supportsStrictMode", Disposition::Offer),
    ("supportsLongCacheRetention", Disposition::Offer),
    ("sessionAffinityFormat", Disposition::Withhold),
    ("supportsOpenAIGrammarTools", Disposition::Withhold),
    ("supportsAdditionalTools", Disposition::Withhold),
    ("supportsToolSearch", Disposition::Withhold),
    ("supportsExplicitPromptCacheMode", Disposition::Withhold),
    ("supportsMidConvoSystemMessages", Disposition::Withhold),
    ("supportsMidConvoEffort", Disposition::Withhold),
];

const ANTHROPIC_GATE: &[(&str, Disposition)] = &[
    ("supportsEagerToolInputStreaming", Disposition::Offer),
    ("supportsLongCacheRetention", Disposition::Offer),
    ("supportsCacheControlOnTools", Disposition::Offer),
    ("supportsTemperature", Disposition::Offer),
    ("forceAdaptiveThinking", Disposition::Offer),
    ("allowEmptySignature", Disposition::Offer),
    ("supportsStrictTools", Disposition::Offer),
    ("sendSessionAffinityHeaders", Disposition::Withhold),
    ("sessionAffinityFormat", Disposition::Withhold),
    ("supportsMidConvoSystemMessages", Disposition::Withhold),
    ("supportsMidConvoToolChanges", Disposition::Withhold),
    ("supportsMidConvoEffort", Disposition::Withhold),
    ("allowedFallbackModels", Disposition::Withhold),
];

const BEDROCK_GATE: &[(&str, Disposition)] = &[("supportsStrictMode", Disposition::Offer)];

const MISTRAL_GATE: &[(&str, Disposition)] =
    &[("supportsMidConvoSystemMessages", Disposition::Withhold)];

/// Every protocol a profile may configure, paired with its compat gate.
///
/// Keyed by protocol but grouped by `pi-ai`'s compat *type*: the three Responses protocols share one
/// compat type, so a switch settable on one is settable on all three. Keying by protocol alone would
/// refuse `azure-openai-responses` and `openai-codex-responses` fields their own models declare. The
/// order matters — it is the order diagnostics list protocols in, and the order
/// [`all_offered_compat_fields`] deduplicates in — and it is upstream's table order.
const COMPAT_GATES: &[(&str, &[(&str, Disposition)])] = &[
    ("mistral-conversations", MISTRAL_GATE),
    ("openai-completions", COMPLETIONS_GATE),
    ("openai-responses", RESPONSES_GATE),
    ("azure-openai-responses", RESPONSES_GATE),
    ("openai-codex-responses", RESPONSES_GATE),
    ("anthropic-messages", ANTHROPIC_GATE),
    ("bedrock-converse-stream", BEDROCK_GATE),
];

/// The compat gate of one protocol, or `None` when it takes no configurable compat.
///
/// A lookup by `&str` rather than by a keyed type because a route's `api` is configuration: it may
/// name a protocol `pi-ai` gives no compat type, or none at all.
#[must_use]
pub fn compat_gate(api: &str) -> Option<&'static [(&'static str, Disposition)]> {
    COMPAT_GATES
        .iter()
        .find(|(protocol, _)| *protocol == api)
        .map(|(_, gate)| *gate)
}

/// The protocols offering one compat field, in [`COMPAT_GATES`] order.
///
/// Empty means either a withheld field or a name no upstream compat type declares; a caller that
/// must tell those apart asks [`is_declared_compat_field`].
#[must_use]
pub fn compat_protocols(field: &str) -> Vec<&'static str> {
    COMPAT_GATES
        .iter()
        .filter(|(_, gate)| offers(gate, field))
        .map(|(protocol, _)| *protocol)
        .collect()
}

/// Whether any upstream compat type declares one field, whatever its disposition.
#[must_use]
pub fn is_declared_compat_field(field: &str) -> bool {
    COMPAT_GATES
        .iter()
        .any(|(_, gate)| gate.iter().any(|(name, _)| *name == field))
}

/// The compat fields one protocol offers, in gate order.
#[must_use]
pub fn offered_compat_fields(api: &str) -> Vec<&'static str> {
    match compat_gate(api) {
        Some(gate) => gate
            .iter()
            .filter(|(_, disposition)| *disposition == Disposition::Offer)
            .map(|(field, _)| *field)
            .collect(),
        None => Vec::new(),
    }
}

/// Every offered field name, deduplicated, in [`COMPAT_GATES`] order.
///
/// This is the vocabulary the one protocol-independent diagnostic prints: the name check runs before
/// any protocol resolves, which is what lets it refuse a misspelling on a route whose models would
/// never have reached the protocol that declares the intended field.
#[must_use]
pub fn all_offered_compat_fields() -> Vec<&'static str> {
    let mut seen = BTreeSet::new();
    let mut fields = Vec::new();
    for (_, gate) in COMPAT_GATES {
        for (field, disposition) in *gate {
            if *disposition == Disposition::Offer && seen.insert(*field) {
                fields.push(*field);
            }
        }
    }
    fields
}

fn offers(gate: &[(&str, Disposition)], field: &str) -> bool {
    gate.iter()
        .any(|(name, disposition)| *name == field && *disposition == Disposition::Offer)
}

// =============================================================================================
// Compat values
// =============================================================================================

/// One `chat_template_kwargs` or `chat_template_args` value.
///
/// The `Var` member is `pi-ai`'s placeholder for a value dispatch fills from the request's thinking
/// state, which is what makes a template-driven gateway configurable without restating its template.
#[derive(Clone, Debug, PartialEq)]
pub enum ChatTemplateValue {
    /// A literal string.
    Text(String),
    /// A literal number.
    Number(f64),
    /// A literal boolean.
    Bool(bool),
    /// An explicit null.
    Null,
    /// A placeholder filled from the request's thinking state.
    Var {
        /// One of [`CHAT_TEMPLATE_VARS`].
        name: String,
        /// Whether the whole kwarg is dropped while reasoning is off.
        omit_when_off: bool,
    },
}

/// One configured value of a compat switch.
///
/// Untyped per field on purpose: the gate decides *which* names exist, and the protocol layer
/// decides what each means. Typing every switch would restate `pi-ai`'s compat structs here and
/// drift from them; the one shape with structure is the template-argument map.
#[derive(Clone, Debug, PartialEq)]
pub enum CompatValue {
    /// A boolean switch.
    Bool(bool),
    /// A numeric switch, such as `vllmPriority`.
    Number(f64),
    /// A textual switch, such as `maxTokensField`.
    Text(String),
    /// A template-argument map, for `chatTemplateKwargs` and `chatTemplateArgs`.
    Args(BTreeMap<String, ChatTemplateValue>),
}

impl CompatValue {
    /// Whether this value is the boolean `true`.
    #[must_use]
    pub fn is_true(&self) -> bool {
        matches!(self, Self::Bool(true))
    }
}

/// The compat switches one layer set.
///
/// A map rather than a typed struct because every operation on it here — the gate lookup, the merge,
/// the taker check — is by field name. It is also what makes the "empty block states nothing"
/// contract expressible: a parsed profile with an empty `chatTemplateKwargs` and one that omits the
/// key are the same request, so both end up absent, and neither makes a route look configured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompatProfile {
    fields: BTreeMap<String, CompatValue>,
}

impl CompatProfile {
    /// A profile that states nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether this profile states nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// How many switches are set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// One switch's value.
    #[must_use]
    pub fn get(&self, field: &str) -> Option<&CompatValue> {
        self.fields.get(field)
    }

    /// Whether one switch is set to `true`.
    #[must_use]
    pub fn flag(&self, field: &str) -> bool {
        self.fields.get(field).is_some_and(CompatValue::is_true)
    }

    /// Sets one switch.
    ///
    /// An empty argument map is *removed* rather than stored, because it would send no arguments:
    /// absent and empty are the same request, and storing one would make a route look configured.
    /// No field-name check happens here — see [`CompatProfile::assert_offered`] for why the name is
    /// judged at resolution instead.
    pub fn set(&mut self, field: impl Into<String>, value: CompatValue) {
        let field = field.into();
        if matches!(&value, CompatValue::Args(args) if args.is_empty()) {
            self.fields.remove(&field);
            return;
        }
        self.fields.insert(field, value);
    }

    /// The switches, in ascending field-name order.
    ///
    /// Sorted rather than insertion-ordered because nothing depends on the order — each switch is
    /// independent — and a deterministic order is what lets a diagnostic and a test agree.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &CompatValue)> {
        self.fields
            .iter()
            .map(|(field, value)| (field.as_str(), value))
    }

    /// This profile's switches laid over `base`'s, field by field.
    #[must_use]
    pub fn merged_over(&self, base: &Self) -> Self {
        let mut merged = base.clone();
        for (field, value) in &self.fields {
            merged.fields.insert(field.clone(), value.clone());
        }
        merged
    }

    /// Reads a compat block from configuration JSON.
    ///
    /// Conversions only: a valueless key is refused here because the alternative is carrying a
    /// `null` into a merge that would then overwrite the next layer's value with nothing — the
    /// "written but not applied" outcome this surface exists to refuse. Withheld and unknown *names*
    /// are not refused here; see [`CompatProfile::assert_offered`].
    ///
    /// `provider` is the route key and `site` the configuration site, both for diagnostics; `value`
    /// is the parsed compat block, where `null` states nothing, as an absent key does.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] for a non-mapping block, a valueless switch, or a value this layer
    /// cannot carry.
    pub fn from_json(provider: &str, site: &str, value: &Value) -> Result<Self, CatalogError> {
        let mut profile = Self::empty();
        match value {
            Value::Null => Ok(profile),
            Value::Object(map) => {
                for (field, raw) in map {
                    if raw.is_null() {
                        return Err(CatalogError::new(
                            provider,
                            format!(
                                "{site} sets compat \"{field}\" with no value; give it one, or remove \
                                 the key to leave the field to the next layer — the installed catalog \
                                 entry, then pi-ai's own detection"
                            ),
                        ));
                    }
                    let Some(converted) = compat_value(field, raw) else {
                        return Err(CatalogError::new(
                            provider,
                            format!(
                                "{site} sets compat \"{field}\" to a value this layer cannot carry; a \
                                 switch takes a boolean, a number, or a string, and the two template \
                                 argument fields take a mapping"
                            ),
                        ));
                    };
                    profile.set(field.clone(), converted);
                }
                Ok(profile)
            }
            _ => Err(CatalogError::new(
                provider,
                format!(
                    "{site} states compat as a non-mapping; give it a mapping of switch names to values"
                ),
            )),
        }
    }

    /// Rejects a switch name no protocol offers.
    ///
    /// Runs before any protocol is resolved, so a withheld field or a misspelling fails even on a
    /// route whose models never reach the protocol that would have taken it — the alternative being
    /// the silent drop that let an unreadable switch look applied. Every key is judged, not only the
    /// ones carrying a value, because whatever its value a person wrote it and expects it to do
    /// something. The name is judged before the value for the same reason: a withheld or misspelled
    /// key written bare must be refused for being that name, not sent back for a value it would be
    /// refused with anyway.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] naming the offending key, and for a withheld key the reason it is
    /// withheld.
    pub fn assert_offered(&self, provider: &str, site: &str) -> Result<(), CatalogError> {
        for (field, _) in self.iter() {
            if !compat_protocols(field).is_empty() {
                continue;
            }
            if is_declared_compat_field(field) {
                return Err(CatalogError::new(
                    provider,
                    format!(
                        "{site} sets compat \"{field}\", which is not configurable here: pi-ai's \
                         installed catalog sets it for the vendors that need it, so name that \
                         provider as the route instead"
                    ),
                ));
            }
            return Err(CatalogError::new(
                provider,
                format!(
                    "{site} sets compat \"{field}\", which no wire protocol declares; the \
                     configurable switches are {}",
                    all_offered_compat_fields().join(", ")
                ),
            ));
        }
        Ok(())
    }
}

fn compat_value(field: &str, raw: &Value) -> Option<CompatValue> {
    match field {
        "chatTemplateKwargs" | "chatTemplateArgs" => match raw {
            Value::Object(map) => {
                let mut args = BTreeMap::new();
                for (name, value) in map {
                    args.insert(name.clone(), template_value(value)?);
                }
                Some(CompatValue::Args(args))
            }
            _ => None,
        },
        _ => match raw {
            Value::Bool(value) => Some(CompatValue::Bool(*value)),
            Value::Number(number) => Some(CompatValue::Number(number.as_f64()?)),
            Value::String(text) => Some(CompatValue::Text(text.clone())),
            _ => None,
        },
    }
}

fn template_value(raw: &Value) -> Option<ChatTemplateValue> {
    match raw {
        Value::String(text) => Some(ChatTemplateValue::Text(text.clone())),
        Value::Number(number) => Some(ChatTemplateValue::Number(number.as_f64()?)),
        Value::Bool(value) => Some(ChatTemplateValue::Bool(*value)),
        Value::Null => Some(ChatTemplateValue::Null),
        Value::Object(map) => {
            let name = map.get("$var")?.as_str()?;
            if !CHAT_TEMPLATE_VARS.contains(&name) {
                return None;
            }
            let omit_when_off = match map.get("omitWhenOff") {
                Some(Value::Bool(value)) => *value,
                None | Some(Value::Null) => false,
                Some(_) => return None,
            };
            Some(ChatTemplateValue::Var {
                name: name.to_owned(),
                omit_when_off,
            })
        }
        Value::Array(_) => None,
    }
}

// =============================================================================================
// Model shape
// =============================================================================================

/// Selectable reasoning efforts for one model.
///
/// Keys are the levels the model offers; a value is the wire spelling dispatch sends for that level,
/// and `None` is upstream's valueless key. `Off` alone may leave its value empty — "supported, send
/// nothing" — because for most providers not thinking is the parameter's absence; every other
/// declared level must name a wire value. A level absent from the map is not offered.
#[derive(Clone, Debug, PartialEq)]
pub enum ReasoningEfforts {
    /// A non-reasoning model, which is how a profile strips reasoning from a catalog model its
    /// gateway cannot serve.
    Disabled,
    /// The offered levels and their wire spellings.
    ///
    /// An empty map is refused by resolution: it declares nothing, and it is neither a spelling of
    /// "inherit" nor of "disable". A `reasoningEfforts:` left valueless and an explicit `{}` both
    /// arrive here empty, and both get the same diagnostic — which is why this layer does not need
    /// to distinguish them.
    Levels(BTreeMap<ThinkingLevel, Option<String>>),
}

/// One model a route serves, as materialization produces it.
///
/// The fields are the ones this layer reads or a consumer needs; `pi-ai`'s `Model` carries more, and
/// one omission is deliberate: `cost` is not carried. Upstream copies the installed entry's cost and
/// otherwise substitutes a zero block, and documents that the harness *never* reads `pi-ai`'s cost
/// metadata, so a field no consumer reads would be a field that rots. Provider-specific extras
/// (model headers, reasoning-level spellings this layer does not model) stay with the caller for the
/// same reason: upstream keeps them by spreading the installed entry and never enumerating it, and a
/// port that enumerated them would silently drop whatever `pi-ai` adds next.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogModel {
    /// The id a request names, and the id the catalog is keyed by.
    pub id: String,
    /// Display name for a selector.
    pub name: String,
    /// The wire protocol this model speaks.
    pub api: String,
    /// The provider route key, stamped onto every materialized model.
    pub provider: String,
    /// The endpoint requests go to.
    pub base_url: String,
    /// Accepted request modalities, in declaration order.
    pub input: Vec<Modality>,
    /// The largest combined request and response the model accepts, in tokens.
    pub context_window: u64,
    /// The largest output the model produces, in tokens.
    pub max_tokens: u64,
    /// Whether the model reasons at all.
    pub reasoning: bool,
    /// The level map dispatch reads.
    ///
    /// `Some(map)` maps a level to `Some(spelling)` when it is supported with that wire value, and to
    /// `None` when it is explicitly unsupported; an absent key leaves that level to `pi-ai`'s own
    /// defaulting. Resolution pins every level it can decide precisely because that defaulting is
    /// asymmetric.
    pub thinking_level_map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
    /// Wire-compatibility switches, when any layer set one.
    pub compat: Option<CompatProfile>,
}

/// The reasoning fields one materialized model carries.
struct ResolvedReasoning {
    reasoning: bool,
    thinking_level_map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
}

/// One configured model entry: an id plus the fields it overrides.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelProfile {
    /// Model id sent to the provider and accepted by a request.
    pub id: String,
    /// The fields this entry states.
    pub fields: ModelFields,
}

impl ModelProfile {
    /// An entry that states only its id, so every field resolves from the installed catalog.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            fields: ModelFields::default(),
        }
    }

    /// An entry carrying a set of overridden fields.
    #[must_use]
    pub fn with_fields(mut self, fields: ModelFields) -> Self {
        self.fields = fields;
        self
    }
}

/// The fields a model entry and a model override share; only the id's home differs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelFields {
    /// Display name; defaults to the catalog name, then the id.
    pub name: Option<String>,
    /// Maximum combined request and response context in tokens.
    pub context_window: Option<u64>,
    /// Maximum output tokens.
    ///
    /// Configuring one also makes it this model's per-request default; a value inherited from the
    /// installed catalog, or the route's fallback, is the model's *capability* and never becomes a
    /// request default on its own.
    pub max_tokens: Option<u64>,
    /// Request modalities; absent or empty keeps the next layer's answer.
    pub input: Option<Vec<Modality>>,
    /// Selectable reasoning efforts; absent inherits the installed catalog's capability.
    pub reasoning_efforts: Option<ReasoningEfforts>,
    /// Switches winning over the route's per field.
    pub compat: Option<CompatProfile>,
}

/// A per-model customization of an installed catalog entry.
///
/// The same fields a [`ModelProfile`] may set, with the id living in the request's key. Unlike a
/// model list, overrides leave the rest of the catalog serving untouched, which is what makes
/// "correct one model, keep the other thirty-seven" a three-line edit.
pub type ModelOverride = ModelFields;

/// The route-level facts model materialization reads.
///
/// The two `default*` capacities exist because a model listing that discloses nothing but ids still
/// has to yield a serviceable route, and the fallback is a guess by construction — which is why it is
/// a configurable route field rather than a constant buried in the resolver.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteCatalogRequest {
    /// Provider route key, stamped onto every materialized model.
    pub provider: String,
    /// Wire protocol override; absent defers to each catalog model's own api, then the route's
    /// shared catalog api.
    pub api: Option<String>,
    /// Endpoint override; absent defers to the catalog model, then the catalog provider.
    pub base_url: Option<String>,
    /// Configured catalog; absent means the whole installed catalog for this route.
    pub models: Option<Vec<ModelProfile>>,
    /// Installed-catalog customizations by model id; only meaningful while `models` is absent.
    pub model_overrides: BTreeMap<String, ModelOverride>,
    /// Route-level switches, landing on each model whose protocol declares them.
    pub compat: Option<CompatProfile>,
    /// Context capacity for a model neither the entry nor the catalog sizes.
    pub default_context_window: u64,
    /// Output capability for a model neither the entry nor the catalog sizes.
    pub default_max_tokens: u64,
    /// Modalities for a model neither the entry nor the catalog declares.
    pub default_input: Vec<Modality>,
}

impl RouteCatalogRequest {
    /// A request for one route with upstream's schema defaults.
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            api: None,
            base_url: None,
            models: None,
            model_overrides: BTreeMap::new(),
            compat: None,
            default_context_window: DEFAULT_CONTEXT_WINDOW,
            default_max_tokens: DEFAULT_MAX_TOKENS,
            default_input: DEFAULT_INPUT.to_vec(),
        }
    }
}

/// How strict model resolution is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validation {
    /// Reject the first unserviceable model, which is what a settings *write* wants.
    Strict,
    /// Retain each model's diagnostic and keep the rest, which is what a settings *read* wants.
    Deferred,
}

/// One route's materialized catalog, plus the request caps its profile chose.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteCatalog {
    /// The materialized models, in configuration order.
    pub models: Vec<CatalogModel>,
    /// Models that cannot be resolved, retained as diagnostics during stored-config reads.
    pub model_errors: BTreeMap<String, String>,
    /// Per-request output caps this profile explicitly configured, by model id.
    ///
    /// Separate from [`CatalogModel::max_tokens`] because the two answer different questions: that
    /// field is the model's output *capability*, while a request default is a cap the deployment
    /// chose to send on requests that name none. Materializing a catalog capability as a request
    /// default would start capping every request at a number nobody picked, so only an explicit
    /// configuration lands here.
    pub configured_max_tokens: BTreeMap<String, u64>,
}

// =============================================================================================
// Route materialization
// =============================================================================================

/// An expected configuration failure, naming the route and the key at fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogError {
    provider: String,
    detail: String,
}

impl CatalogError {
    /// Builds a diagnostic for one route.
    #[must_use]
    pub fn new(provider: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            detail: detail.into(),
        }
    }

    /// The route at fault.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// What is wrong, without the route prefix.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "llm-pi-ai: provider \"{}\" {}",
            self.provider, self.detail
        )
    }
}

impl std::error::Error for CatalogError {}

fn invalid(provider: &str, detail: impl Into<String>) -> CatalogError {
    CatalogError::new(provider, detail)
}

/// The installed catalog for one route, keyed by model id.
///
/// Two facts, both of which upstream reads from `pi-ai`'s static registry: the models the provider
/// ships, and the provider's own endpoint. Injecting them is what makes [`resolve_route_models`] a
/// pure function.
pub type InstalledCatalog = BTreeMap<String, CatalogModel>;

/// Materializes one route's catalog, rejecting anything unserviceable.
///
/// A route with no configured `models` serves the installed catalog unchanged, which is what keeps
/// an existing `providers: { deepseek: { apiKeyEnv: … } }` profile working untouched.
///
/// # Errors
///
/// Returns [`CatalogError`] for any model that cannot be materialized; see
/// [`resolve_route_models_with`] for the deferred alternative.
pub fn resolve_route_models(
    request: &RouteCatalogRequest,
    installed: &InstalledCatalog,
    provider_base_url: Option<&str>,
) -> Result<RouteCatalog, CatalogError> {
    resolve_route_models_with(request, installed, provider_base_url, Validation::Strict)
}

/// Materializes one route's catalog, with an explicit strictness.
///
/// # Errors
///
/// Returns [`CatalogError`] for a route-level fault in either mode, for the first unserviceable
/// model under [`Validation::Strict`], and for a route that resolves no serviceable model in either
/// mode.
pub fn resolve_route_models_with(
    request: &RouteCatalogRequest,
    installed: &InstalledCatalog,
    provider_base_url: Option<&str>,
    #[allow(
        dead_code,
        reason = "kept so the resolver can report the mode it ran under; the callers that would read it are the deferred-read tests"
    )]
    validation: Validation,
) -> Result<RouteCatalog, CatalogError> {
    let provider = request.provider.as_str();
    let configured = request.models.as_deref().unwrap_or(&[]);
    let mut model_errors = BTreeMap::new();
    check_overrides(
        request,
        installed,
        configured,
        validation,
        &mut model_errors,
    )?;

    let entries = entries_for(request, installed);
    if entries.is_empty() {
        return Err(invalid(
            provider,
            "resolves no models; the installed catalog does not describe this route, so its models \
             must be listed in configuration",
        ));
    }
    let route_api = shared_catalog_api(installed);
    // Vocabulary before protocols: a withheld or undeclared switch is refused wherever it is
    // written, so it cannot look applied on a route whose models never reach the protocol that
    // would have taken it.
    if let Some(compat) = request.compat.as_ref() {
        compat.assert_offered(provider, "route")?;
    }
    let mut resolver = Resolver {
        request,
        installed,
        provider_base_url,
        route_api: route_api.as_deref(),
        validation,
        seen: BTreeSet::new(),
        configured_max_tokens: BTreeMap::new(),
        model_errors: &mut model_errors,
    };
    let mut models = Vec::with_capacity(entries.len());
    for entry in &entries {
        match resolver.resolve(entry) {
            Ok(model) => models.push(model),
            Err(error) => {
                if validation == Validation::Strict {
                    return Err(error);
                }
                resolver
                    .model_errors
                    .insert(entry.id.clone(), error.to_string());
            }
        }
    }
    let configured_max_tokens = resolver.configured_max_tokens;
    let model_errors = resolver.model_errors.clone();
    // A later duplicate invalidates the id, including an earlier resolved entry.
    let serviceable: Vec<CatalogModel> = models
        .into_iter()
        .filter(|model| !model_errors.contains_key(&model.id))
        .collect();
    if serviceable.is_empty() {
        // A route with no serviceable model has nothing to serve. Returning an empty catalog would
        // present as "this provider is configured" until the first request failed, which is a worse
        // answer than the diagnostic naming what is wrong.
        return Err(invalid(
            provider,
            format!(
                "resolves no serviceable models; {} {} cannot be served, and the diagnostics are {}",
                model_errors.len(),
                if model_errors.len() == 1 {
                    "entry"
                } else {
                    "entries"
                },
                model_errors
                    .values()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        ));
    }
    assert_route_compat_has_a_taker(request, &serviceable)?;
    Ok(RouteCatalog {
        models: serviceable,
        model_errors,
        configured_max_tokens,
    })
}

/// Rejects every unusable `modelOverrides` entry, before any model resolves.
///
/// Writes reject missing referents. Stored overrides retain a diagnostic after catalog removal
/// rather than silently disappearing, which is why the "the catalog does not describe it" branch
/// depends on the mode and the three structural branches do not.
fn check_overrides(
    request: &RouteCatalogRequest,
    installed: &InstalledCatalog,
    configured: &[ModelProfile],
    validation: Validation,
    model_errors: &mut BTreeMap<String, String>,
) -> Result<(), CatalogError> {
    let provider = request.provider.as_str();
    for id in request.model_overrides.keys() {
        if id.is_empty() {
            return Err(invalid(
                provider,
                "has a modelOverrides entry with an empty model id",
            ));
        }
        if installed.is_empty() {
            return Err(invalid(
                provider,
                format!(
                    "sets modelOverrides for \"{id}\", but the installed catalog does not describe \
                     this route; a declared route spells every model out in its models list"
                ),
            ));
        }
        if !configured.is_empty() {
            return Err(invalid(
                provider,
                format!(
                    "sets modelOverrides for \"{id}\" beside a models list; models already replaces \
                     the served catalog, so declare the fields on its entries"
                ),
            ));
        }
        if !installed.contains_key(id) {
            let message = format!(
                "modelOverrides names \"{id}\", which the installed catalog does not describe"
            );
            if validation == Validation::Strict {
                return Err(invalid(provider, message));
            }
            model_errors.insert(id.clone(), CatalogError::new(provider, message).to_string());
        }
    }
    Ok(())
}

/// The entries to materialize: the configured list, or the installed catalog with overrides folded in.
///
/// An override becomes the catalog entry's *configuration*, so everything a model entry may declare
/// — capacities, efforts, compat — resolves through the same path with the same diagnostics and
/// request-default semantics.
fn entries_for(request: &RouteCatalogRequest, installed: &InstalledCatalog) -> Vec<ModelProfile> {
    if let Some(configured) = request.models.as_deref() {
        if !configured.is_empty() {
            return configured.to_vec();
        }
    }
    installed
        .values()
        .map(|model| {
            let fields = request
                .model_overrides
                .get(&model.id)
                .cloned()
                .unwrap_or_default();
            ModelProfile::new(model.id.clone()).with_fields(fields)
        })
        .collect()
}

/// The one wire protocol a route's shipped models agree on.
///
/// This is what lets a deployment add a model the installed catalog has not caught up with without
/// restating the protocol its siblings already use. A route whose shipped models disagree — an
/// `OpenAI`-style catalog spanning Responses and Chat Completions — has no such answer, so a model it
/// does not describe must name its protocol at the route.
fn shared_catalog_api(installed: &InstalledCatalog) -> Option<String> {
    let apis: BTreeSet<&str> = installed.values().map(|model| model.api.as_str()).collect();
    let mut iter = apis.into_iter();
    let first = iter.next()?;
    if iter.next().is_none() {
        Some(first.to_owned())
    } else {
        None
    }
}

/// Rejects a route switch no model on the route could read.
///
/// Per field, not per block: a route may default a switch its completions models take beside one
/// only its anthropic models do, and neither should fail for the other's sake. What is refused is a
/// route default no model on the route could ever read, which is a route that will not behave as
/// written.
fn assert_route_compat_has_a_taker(
    request: &RouteCatalogRequest,
    serviceable: &[CatalogModel],
) -> Result<(), CatalogError> {
    let Some(compat) = request.compat.as_ref() else {
        return Ok(());
    };
    for (field, _) in compat.iter() {
        let takers = compat_protocols(field);
        if serviceable
            .iter()
            .any(|model| takers.contains(&model.api.as_str()))
        {
            continue;
        }
        return Err(invalid(
            request.provider.as_str(),
            format!(
                "sets compat \"{field}\", but no model on the route speaks a protocol that takes \
                 it; it exists on {}",
                takers.join(", ")
            ),
        ));
    }
    Ok(())
}

/// One invocation's mutable state, so the per-model path stays a method rather than a long function
/// with five out-parameters.
struct Resolver<'a> {
    request: &'a RouteCatalogRequest,
    installed: &'a InstalledCatalog,
    provider_base_url: Option<&'a str>,
    route_api: Option<&'a str>,
    validation: Validation,
    seen: BTreeSet<String>,
    configured_max_tokens: BTreeMap<String, u64>,
    model_errors: &'a mut BTreeMap<String, String>,
}

impl Resolver<'_> {
    /// Refuses an entry that cannot name one model, and records a duplicate per the read mode.
    ///
    /// Separate from `resolve` because the two answer different questions -- "is this entry a model at
    /// all" and "what does it materialize to" -- and a caller debugging a bad route wants the first
    /// without the second's work.
    fn admit(&mut self, entry: &ModelProfile) -> Result<(), CatalogError> {
        let provider = self.request.provider.as_str();
        if let Some(compat) = entry.fields.compat.as_ref() {
            compat.assert_offered(provider, &format!("model \"{}\"", entry.id))?;
        }
        if entry.id.is_empty() {
            return Err(invalid(provider, "has a model with an empty id"));
        }
        if self.seen.insert(entry.id.clone()) {
            return Ok(());
        }
        let message = format!("lists model \"{}\" more than once", entry.id);
        // A strict read refuses; a deferred one records the diagnostic and drops the second copy,
        // which is why the mode is read here rather than carried and ignored.
        match self.validation {
            Validation::Strict => Err(invalid(provider, message)),
            Validation::Deferred => {
                self.model_errors.insert(entry.id.clone(), message);
                Err(invalid(provider, "duplicate"))
            }
        }
    }

    fn resolve(&mut self, entry: &ModelProfile) -> Result<CatalogModel, CatalogError> {
        self.admit(entry)?;
        let provider = self.request.provider.as_str();
        let base = self.installed.get(&entry.id);
        let Some(api) = self
            .request
            .api
            .clone()
            .or_else(|| base.map(|model| model.api.clone()))
            .or_else(|| self.route_api.map(str::to_owned))
        else {
            return Err(invalid(
                provider,
                format!(
                    "model \"{}\" needs an api; the installed catalog does not describe it, so set \
                     the route's api to the wire protocol its endpoint speaks",
                    entry.id
                ),
            ));
        };
        let Some(base_url) = self
            .request
            .base_url
            .clone()
            .or_else(|| base.map(|model| model.base_url.clone()))
            .or_else(|| self.provider_base_url.map(str::to_owned))
        else {
            return Err(invalid(
                provider,
                format!(
                    "model \"{}\" needs a baseURL; the installed catalog does not describe this route",
                    entry.id
                ),
            ));
        };
        // Capacities fall back to the route's own defaults. Upstream also rejects a non-integer and a
        // non-positive value; with an unsigned type the only reachable fault is zero.
        let context_window = entry
            .fields
            .context_window
            .or_else(|| base.map(|model| model.context_window))
            .unwrap_or(self.request.default_context_window);
        if context_window == 0 {
            return Err(invalid(
                provider,
                format!(
                    "model \"{}\" contextWindow must be a positive integer",
                    entry.id
                ),
            ));
        }
        let max_tokens = entry
            .fields
            .max_tokens
            .or_else(|| base.map(|model| model.max_tokens))
            .unwrap_or(self.request.default_max_tokens);
        if max_tokens == 0 {
            return Err(invalid(
                provider,
                format!(
                    "model \"{}\" maxTokens must be a positive integer",
                    entry.id
                ),
            ));
        }
        // Only a value the profile named is a deployment choice; the catalog's is the model's
        // capability and stays out of request defaults.
        if let Some(max_tokens) = entry.fields.max_tokens {
            self.configured_max_tokens
                .insert(entry.id.clone(), max_tokens);
        }
        let reasoning = resolve_model_reasoning(provider, entry, base)?;
        let compat = resolve_model_compat(provider, entry, self.request, base, &api)?;
        let input = declared_input(entry.fields.input.as_deref())
            .or_else(|| base.map(|model| model.input.clone()))
            .unwrap_or_else(|| self.request.default_input.clone());
        Ok(CatalogModel {
            id: entry.id.clone(),
            name: entry
                .fields
                .name
                .clone()
                .or_else(|| base.map(|model| model.name.clone()))
                .unwrap_or_else(|| entry.id.clone()),
            api,
            provider: provider.to_owned(),
            base_url,
            input,
            context_window,
            max_tokens,
            reasoning: reasoning.reasoning,
            // The installed entry's map rides along when this layer decides nothing: `pi-ai` never
            // reads it on a non-reasoning model, so stripping it would be a field enumeration with
            // no observable effect.
            thinking_level_map: reasoning
                .thinking_level_map
                .or_else(|| base.and_then(|model| model.thinking_level_map.clone())),
            compat: compat.or_else(|| base.and_then(|model| model.compat.clone())),
        })
    }
}

/// One entry's modality list, or `None` when it states no answer.
///
/// Absent and empty mean the same thing — `[]` describes a model that accepts nothing and could
/// serve no request — which is what makes an entry naming a catalog model without declaring
/// modalities keep the catalog's.
fn declared_input(configured: Option<&[Modality]>) -> Option<Vec<Modality>> {
    configured
        .filter(|list| !list.is_empty())
        .map(<[Modality]>::to_vec)
}

/// Resolves one model's reasoning capability from its declared efforts.
///
/// A declared map translates to `pi-ai`'s level map with every level decided explicitly: declared
/// levels carry their wire spelling, undeclared levels are pinned to "unsupported". Pinning matters
/// because `pi-ai`'s own defaulting is asymmetric — an absent key means supported for the five base
/// levels but unsupported for `xhigh`/`max` — and a profile author should not need to know that. A
/// declared `off` with no value is the one exception: it stays absent from the map, which `pi-ai`
/// reads as "supported, send nothing", the correct dispatch where not thinking is the parameter's
/// absence, while `off` with a value sends that value.
fn resolve_model_reasoning(
    provider: &str,
    entry: &ModelProfile,
    base: Option<&CatalogModel>,
) -> Result<ResolvedReasoning, CatalogError> {
    match &entry.fields.reasoning_efforts {
        None => Ok(ResolvedReasoning {
            reasoning: base.is_some_and(|model| model.reasoning),
            thinking_level_map: None,
        }),
        Some(ReasoningEfforts::Disabled) => Ok(ResolvedReasoning {
            reasoning: false,
            thinking_level_map: None,
        }),
        Some(ReasoningEfforts::Levels(levels)) => resolve_declared_efforts(provider, entry, levels),
    }
}

fn resolve_declared_efforts(
    provider: &str,
    entry: &ModelProfile,
    levels: &BTreeMap<ThinkingLevel, Option<String>>,
) -> Result<ResolvedReasoning, CatalogError> {
    if levels.is_empty() {
        return Err(invalid(
            provider,
            format!(
                "model \"{}\" has an empty reasoningEfforts; declare the offered levels, set false \
                 for a non-reasoning model, or omit the field to keep the installed catalog's \
                 capability",
                entry.id
            ),
        ));
    }
    for level in THINKING_LEVELS {
        let Some(wire) = levels.get(&level) else {
            continue;
        };
        match wire {
            None if level != ThinkingLevel::Off => {
                return Err(invalid(
                    provider,
                    format!(
                        "model \"{}\" reasoningEfforts.{level} needs the wire value dispatch should \
                         send; only \"off\" may leave it empty",
                        entry.id
                    ),
                ));
            }
            Some(wire) if wire.is_empty() => {
                return Err(invalid(
                    provider,
                    format!(
                        "model \"{}\" reasoningEfforts.{level} must not be an empty string",
                        entry.id
                    ),
                ));
            }
            None | Some(_) => {}
        }
    }
    if !levels.keys().any(|level| *level != ThinkingLevel::Off) {
        return Err(invalid(
            provider,
            format!(
                "model \"{}\" reasoningEfforts offers no level beyond \"off\"; declare a thinking \
                 level, or set reasoningEfforts to false for a non-reasoning model",
                entry.id
            ),
        ));
    }
    let mut map = BTreeMap::new();
    for level in THINKING_LEVELS {
        match levels.get(&level) {
            // Undeclared levels are pinned unsupported rather than left to pi-ai's asymmetric
            // defaulting.
            None => {
                map.insert(level, None);
            }
            // Only `off` reaches here with no value, and it is left out: "supported, send nothing".
            Some(None) => {}
            Some(Some(wire)) => {
                map.insert(level, Some(wire.clone()));
            }
        }
    }
    Ok(ResolvedReasoning {
        reasoning: true,
        thinking_level_map: Some(map),
    })
}

/// Resolves one model's compat block from the profile's switches.
///
/// A model switch wins over the route switch field by field; whatever neither sets keeps the
/// installed entry's value, and a field no layer decides falls through to `pi-ai`'s own detection. A
/// model-level switch its protocol does not take fails resolution — about one named model it can
/// only be a mistake — while a route-level one skips past such models, since a route default must
/// stay settable on a route whose models do not all speak one protocol. Every field reaching here is
/// offered by some protocol; [`CompatProfile::assert_offered`] has already refused the rest.
///
/// `Ok(None)` means "this layer set nothing, keep the installed entry's block"; it is not the same as
/// `Some(empty)`, which cannot occur.
fn resolve_model_compat(
    provider: &str,
    entry: &ModelProfile,
    request: &RouteCatalogRequest,
    base: Option<&CatalogModel>,
    api: &str,
) -> Result<Option<CompatProfile>, CatalogError> {
    let gate = compat_gate(api);
    let mut configured = CompatProfile::empty();
    if let (Some(route), Some(gate)) = (request.compat.as_ref(), gate) {
        for (field, value) in route.iter() {
            if offers(gate, field) {
                configured.set(field, value.clone());
            }
        }
    }
    if let Some(model_compat) = entry.fields.compat.as_ref() {
        for (field, value) in model_compat.iter() {
            if !gate.is_some_and(|gate| offers(gate, field)) {
                let offered = offered_compat_fields(api);
                let vocabulary = if offered.is_empty() {
                    "no configurable compat".to_owned()
                } else {
                    offered.join(", ")
                };
                return Err(invalid(
                    provider,
                    format!(
                        "model \"{}\" sets compat \"{field}\", but its api is \"{api}\", which does \
                         not take it; that switch exists on {}, and \"{api}\" offers {vocabulary}",
                        entry.id,
                        compat_protocols(field).join(", ")
                    ),
                ));
            }
            configured.set(field, value.clone());
        }
    }
    if configured.is_empty() {
        return Ok(None);
    }
    // The installed entry's compat matches the entry's OWN api — a route-level `api` repoint (an
    // anthropic catalog served through an OpenAI-compatible gateway) leaves `base.compat` in the
    // other protocol's shape, so it is inherited only while the resolved api still is the entry's. A
    // repointed model starts from `pi-ai`'s URL-derived detection instead, which is what a protocol
    // change means for every other compat field too.
    let inherited = base
        .filter(|model| model.api == api)
        .and_then(|model| model.compat.as_ref());
    Ok(Some(match inherited {
        Some(base) => configured.merged_over(base),
        None => configured,
    }))
}

// =============================================================================================
// Thinking levels and transcript updates
// =============================================================================================

/// Selectable reasoning levels for one model, in escalation order.
///
/// A non-reasoning model offers `off` and nothing else. Otherwise a level is offered when its map
/// entry does not say "unsupported", with `pi-ai`'s asymmetry preserved: `xhigh` and `max` are
/// offered only when the map names them, because their absence means *unsupported* there while it
/// means *supported* for the five base levels.
#[must_use]
pub fn get_supported_thinking_levels(model: &CatalogModel) -> Vec<ThinkingLevel> {
    if !model.reasoning {
        return vec![ThinkingLevel::Off];
    }
    let Some(map) = model.thinking_level_map.as_ref() else {
        // No map at all: every base level is supported and the two asymmetric ones are not.
        return THINKING_LEVELS
            .into_iter()
            .filter(|level| !is_asymmetric(*level))
            .collect();
    };
    THINKING_LEVELS
        .into_iter()
        .filter(|level| match map.get(level) {
            Some(None) => false,
            Some(Some(_)) => true,
            None => !is_asymmetric(*level),
        })
        .collect()
}

/// Whether a level is one `pi-ai` treats asymmetrically — absent means unsupported.
fn is_asymmetric(level: ThinkingLevel) -> bool {
    matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max)
}

/// How a model accepts transcript updates mid-conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConversationUpdates {
    /// Where a system-prompt change may go, or `None` when it needs a new conversation.
    pub system_prompt_update: Option<&'static str>,
    /// How tool definitions may change, or `None` when they may not.
    pub tool_update: Option<&'static str>,
}

/// Resolves transcript update modes from the exact model's protocol capabilities.
///
/// Every flag this reads is *withheld* by the gates above, so none of them can be configured on a
/// profile: they arrive only through the installed catalog entry's compat block, which is where
/// `pi-ai` records what a named vendor actually accepts.
#[must_use]
pub fn conversation_updates(model: &CatalogModel) -> ConversationUpdates {
    let flag = |field: &str| {
        model
            .compat
            .as_ref()
            .is_some_and(|compat| compat.flag(field))
    };
    if !flag("supportsMidConvoSystemMessages") {
        return ConversationUpdates::default();
    }
    let tool_update = match model.api.as_str() {
        "anthropic-messages" if flag("supportsMidConvoToolChanges") => Some("in-history"),
        "openai-completions" if flag("supportsMidConvoToolAdditions") => Some("addition-only"),
        "openai-responses" | "azure-openai-responses" | "openai-codex-responses"
            if flag("supportsAdditionalTools") || flag("supportsToolSearch") =>
        {
            Some("addition-only")
        }
        _ => None,
    };
    ConversationUpdates {
        system_prompt_update: Some("in-history"),
        tool_update,
    }
}

// =============================================================================================
// Model-listing parsing
// =============================================================================================

/// Why an interrogation of an endpoint could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryCode {
    /// The endpoint answered, but not with a listing this build can read.
    Failed,
    /// The protocol has no listing this build can read at all.
    Unsupported,
    /// The key itself is unusable, before any request is built.
    InvalidCredential,
}

/// A discovery failure: a code the caller switches on, and the message a user reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryError {
    /// What kind of failure this is.
    pub code: DiscoveryCode,
    /// The message, naming the URL or the field at fault.
    pub message: String,
}

impl DiscoveryError {
    /// A listing that could not be read.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            code: DiscoveryCode::Failed,
            message: message.into(),
        }
    }

    /// A protocol with no readable listing.
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: DiscoveryCode::Unsupported,
            message: message.into(),
        }
    }

    /// An unusable credential, refused before a request is built.
    #[must_use]
    pub fn invalid_credential(message: impl Into<String>) -> Self {
        Self {
            code: DiscoveryCode::InvalidCredential,
            message: message.into(),
        }
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DiscoveryError {}

/// One model an endpoint advertised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// The id a request names.
    pub id: String,
    /// A human-readable name, defaulted from the id.
    pub name: String,
    /// The context window the listing disclosed, when it disclosed one.
    pub context_window: Option<u64>,
    /// The output cap the listing disclosed, when it disclosed one.
    pub max_tokens: Option<u64>,
}

/// Whether a protocol's listing is one this build can read.
#[must_use]
pub fn is_listable_protocol(api: &str) -> bool {
    LISTABLE_PROTOCOLS.contains(&api)
}

/// The protocol to interrogate, defaulting a draft that has not chosen one.
///
/// Chat Completions is the shape a gateway is overwhelmingly likely to speak, and refusing until the
/// field is filled would withhold the "fetch available models" action from the case it exists for.
#[must_use]
pub fn probe_protocol(api: Option<&str>) -> &str {
    api.unwrap_or(DEFAULT_PROBE_PROTOCOL)
}

/// The listing URL for one endpoint and protocol.
///
/// The base is treated as a prefix rather than a URL to resolve against, so a deployment path such as
/// `https://gateway.example/openai/v1` keeps its segments instead of losing them to URL resolution.
/// `OpenAI` protocols list at `{baseURL}/models`, and `Gemini` at the same path with a page size,
/// because its `baseURL` already carries the API version. `Anthropic` lists at `{root}/v1/models`
/// where the root is the base without trailing slashes and without one trailing `/v1` segment:
/// gateway documentation publishes both spellings of the same root. Only this listing URL normalizes
/// that segment; model requests receive the configured `baseURL` unchanged.
#[must_use]
pub fn listing_url(base_url: &str, api: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if api == "google-generative-ai" {
        return format!("{base}/models?pageSize={GEMINI_MODEL_LIMIT}");
    }
    if api != "anthropic-messages" {
        return format!("{base}/models");
    }
    let root = base.strip_suffix("/v1").unwrap_or(base);
    format!("{root}/v1/models?limit={ANTHROPIC_MODEL_LIMIT}")
}

/// Reads one `OpenAI`-compatible or `Anthropic` model-listing reply.
///
/// The standard `data` array takes precedence when both supported formats are present. An enriched
/// `models` map uses each property key as the endpoint-facing id; its nested `id` is only a fallback
/// for an empty key because gateways may put a canonical model identity there instead of the alias
/// they accept on requests. Only object-valued map entries are models; primitive properties are
/// ignored because they may be directory metadata rather than model records.
///
/// Entries without a usable id are skipped rather than failing the whole interrogation: a single
/// malformed row should not deny the user the rest of a working endpoint's catalog. Missing names
/// fall back to the adopted id so a form receives a complete human-readable row.
///
/// # Errors
///
/// Returns [`DiscoveryError`] with code [`DiscoveryCode::Failed`] when the reply has neither shape.
pub fn read_listing(body: &Value) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let listed: Vec<(Option<&str>, &Value)> = if let Some(Value::Array(rows)) = body.get("data") {
        rows.iter().map(|raw| (None, raw)).collect()
    } else if let Some(Value::Object(map)) = body.get("models") {
        map.iter()
            .filter(|(_, raw)| raw.is_object())
            .map(|(key, raw)| (Some(key.as_str()), raw))
            .collect()
    } else {
        return Err(DiscoveryError::failed(
            "the endpoint's model listing has neither a \"data\" array nor a \"models\" object; \
             enter this provider's models by hand",
        ));
    };
    let mut models = Vec::with_capacity(listed.len());
    for (key, raw) in listed {
        let id = label(&[key, raw.get("id").and_then(Value::as_str)]);
        let Some(id) = id else {
            continue;
        };
        let name = label(&[
            raw.get("name").and_then(Value::as_str),
            raw.get("display_name").and_then(Value::as_str),
            raw.get("displayName").and_then(Value::as_str),
        ])
        .unwrap_or_else(|| id.clone());
        let context_window = capacity(&[
            raw.get("contextWindow"),
            raw.get("context_window"),
            raw.get("context_length"),
            raw.get("max_input_tokens"),
            raw.get("limit").and_then(|limit| limit.get("context")),
        ]);
        let max_tokens = capacity(&[
            raw.get("maxOutputTokens"),
            raw.get("max_output_tokens"),
            raw.get("maxTokens"),
            raw.get("max_tokens"),
            raw.get("limit").and_then(|limit| limit.get("output")),
            raw.get("top_provider")
                .and_then(|provider| provider.get("max_completion_tokens")),
        ]);
        models.push(DiscoveredModel {
            id,
            name,
            context_window,
            max_tokens,
        });
    }
    Ok(models)
}

/// Reads one `Gemini` model-listing reply.
///
/// Each entry's `name` is a resource name such as `models/gemini-2.5-pro`; the request id is that
/// name without the `models/` prefix, because the `Gemini` protocol sends the bare id. Entries that
/// declare generation methods without `generateContent`, such as embedding models, cannot serve a
/// conversation and are skipped. An entry that declares no methods at all is not evidence that it
/// cannot converse, so it is kept.
///
/// # Errors
///
/// Returns [`DiscoveryError`] with code [`DiscoveryCode::Failed`] when there is no `models` array.
pub fn read_gemini_listing(body: &Value) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let Some(Value::Array(listed)) = body.get("models") else {
        return Err(DiscoveryError::failed(
            "the endpoint's model listing has no \"models\" array; enter this provider's models by \
             hand",
        ));
    };
    let mut models = Vec::with_capacity(listed.len());
    for raw in listed {
        if let Some(Value::Array(methods)) = raw.get("supportedGenerationMethods") {
            if !methods
                .iter()
                .any(|method| method.as_str() == Some("generateContent"))
            {
                continue;
            }
        }
        let Some(id) = label(&[raw.get("name").and_then(Value::as_str)])
            .map(|name| name.strip_prefix("models/").unwrap_or(&name).to_owned())
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let name =
            label(&[raw.get("displayName").and_then(Value::as_str)]).unwrap_or_else(|| id.clone());
        models.push(DiscoveredModel {
            id,
            name,
            context_window: capacity(&[raw.get("inputTokenLimit")]),
            max_tokens: capacity(&[raw.get("outputTokenLimit")]),
        });
    }
    Ok(models)
}

/// A non-empty string field, from the first candidate that carries one.
fn label(candidates: &[Option<&str>]) -> Option<String> {
    candidates
        .iter()
        .flatten()
        .find(|candidate| !candidate.is_empty())
        .map(|candidate| (*candidate).to_owned())
}

/// A positive integer field, from the first candidate that carries one.
fn capacity(candidates: &[Option<&Value>]) -> Option<u64> {
    candidates
        .iter()
        .flatten()
        .find_map(|candidate| positive_integer(candidate))
}

/// A positive integer, accepting a whole-valued float because JSON numbers are not typed.
///
/// Upstream asks `Number.isInteger(n) && n > 0`, and `JSON.parse("1.0")` is the integer `1` in
/// JavaScript while `serde_json` keeps it as a float, so both spellings must be accepted here or a
/// listing written by a JavaScript producer would lose a capacity this function is meant to read.
fn positive_integer(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return (number > 0).then_some(number);
    }
    let float = value.as_f64()?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "the value is checked to be a positive whole number that fits u64 before the cast"
    )]
    if float.fract() == 0.0 && float > 0.0 && float <= u64::MAX as f64 {
        Some(float as u64)
    } else {
        None
    }
}
