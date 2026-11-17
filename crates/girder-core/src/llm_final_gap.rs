//! The LLM runtime: adapter registration, model-info normalization, and the prepared-call contract.
//!
//! Upstream path: `packages/llm/llm/src/index.ts` — 797 lines, the largest single file in the nine
//! `packages/llm` subpackages, and the only one whose absence leaves *thirteen* declared failure
//! codes with no producer.
//!
//! # Why this module exists
//!
//! This crate already has a great deal of the LLM surface, and every piece of it is a *leaf*: a
//! message model, an assembler, a retry policy, a token fold, a serializer, a file-id plan. What it
//! did not have is the thing those leaves hang from. A caller that wants to "ask provider `p` for
//! model `m` under these controls" has had to reach into a provider directly, which is why:
//!
//! | consumer that already exists | what it needs | local status before this module |
//! |---|---|---|
//! | [`crate::agent::ModelSelection`] | a route that is actually registered | nothing to check it against |
//! | [`crate::agent_loop`] | a resolved model + defaulted controls | calls `LlmProvider::stream` raw |
//! | [`crate::request_header_gap::HeaderConfig`] | a producer | built only by hand, in tests |
//! | [`crate::request_header_gap::AdapterDefaults`] | the two flags decided from a real resolution | never set outside a test |
//! | `crate::session_title_llm_gap::Prepared::reasoning_effort` | a model's declared efforts | upstream calls `ctx.llm.prepareCall`; here the effort had no source |
//! | [`crate::providers::RouteRegistry`] | provider *adapters* beside route *names* | routes cannot register an adapter |
//!
//! The last row is the sharpest. [`crate::providers`] keys a registry by route *name* — a
//! deployment-level alias — and answers `(route, model) -> ModelInfo`. Upstream's `LlmRuntime` keys
//! by **provider id** and answers `provider -> adapter`, which is a different question: a route is a
//! name a user chose, a provider is a thing that can stream. Two mounts of one provider are two
//! routes and one adapter; that arithmetic is impossible on the current type, which is why this
//! module registers adapters rather than routes and leaves routes to `providers`.
//!
//! # The evidence that this is the missing half
//!
//! [`crate::llm_extra_gap::FAILURE_CODES`] is a 52-entry table copied from upstream's
//! `LLM_FAILURE_CODES`, and thirteen of its entries were, before this module, **strings nobody
//! raised**:
//!
//! ```text
//! INVALID_ADAPTER   DUPLICATE_ADAPTER   REGISTRATION_DISPOSED
//! INVALID_DIRECTORY DUPLICATE_DIRECTORY INVALID_DISCOVERY
//! NO_DISCOVERY      NO_ADAPTER          INVALID_CATALOG
//! INVALID_MODEL_INFO INVALID_MODEL_CONTEXT INVALID_MODEL_MAX_TOKENS
//! INVALID_MODEL_REASONING UNSUPPORTED_REASONING_EFFORT INVALID_PREPARED_CALL
//! ```
//!
//! A code table is a *contract with the code that raises it*. Fifteen codes with no raiser meant the
//! retry policy in [`crate::retry`] could be told `UNSUPPORTED_REASONING_EFFORT` by nothing, and a
//! UI keying a message off `NO_ADAPTER` had no path that produced one. Every variant of
//! [`RuntimeError`] below names exactly one of those codes, and [`RuntimeError::code`] returns the
//! string from the same table — so the table and the producers cannot drift apart silently.
//!
//! # The four invariants this module is
//!
//! Each is a rule that a caller cannot check for itself, which is why it belongs here rather than in
//! the caller:
//!
//! 1. **Registration is all-or-nothing.** [`Runtime::register_adapter`] and
//!    [`Runtime::replace_routes`] validate *every* provider name before touching the table, so a
//!    registration that fails halfway cannot leave a provider half-mounted. A route table with one
//!    arm registered and another rejected is worse than either outcome, because a caller that
//!    retried would get `DUPLICATE_ADAPTER` for the arm that worked.
//! 2. **An adapter's answer is normalized, never trusted.** [`Runtime::resolve_model_info`] refuses
//!    a model whose id or provider moved, whose name is blank, whose window is zero, or whose
//!    reasoning default names an effort it does not offer. An adapter is third-party code; a
//!    plausible-looking wrong answer is the failure mode that reaches the user as a bad request
//!    instead of a clear one.
//! 3. **A materialized default is recorded as materialized.** Upstream tracks this with two `true`
//!    flags ([`AdapterDefaults`]) so a UI can distinguish "the user asked for 256k" from "the
//!    adapter supplied 256k". Filling the number without the flag is the bug that makes a later
//!    settings page offer to "restore" a value nobody chose.
//! 4. **A prepared call is dispatched once, with the config it was prepared under.** Both halves
//!    matter: dispatching twice double-bills a stream, and dispatching with an edited config sends
//!    controls the retry policy and the header record never saw.
//!
//! # What is deliberately not here
//!
//! - **No bus, no fiber, no `ctx.effect`.** Upstream's `LlmRuntime extends TypertRemoteService`, so
//!   registration is scoped to a fiber and disposal is automatic. Here registration returns a
//!   [`AdapterHandle`] and [`Runtime::dispose_adapter`] takes it back — the same contract with the
//!   lifetime written down, which is also what makes "a disposed handle cannot be replaced" a
//!   testable value rather than a closure over a mutable flag.
//! - **No `WeakSet` for the agent-loop marker.** Upstream's `markAgentLoopRequest` tags one frozen
//!   request object. A Rust `LlmRequest` is a value, not an identity, so the marker would have to
//!   become a field — which is a change to [`crate::llm::LlmRequest`], not a gap in this module.
//!   [`crate::content_projection_gap::TOOL_UPDATE_GAP`] is the same shape of omission for the same
//!   reason.
//! - **No `async`.** Every rule above is a decision over values. The two places upstream awaits —
//!   `adapter.listModels` and `adapter.prepareCall`, both I/O — arrive here as *answers*
//!   ([`Runtime::normalize_catalog`] and [`Runtime::resolve_model_info`]) that this module then
//!   judges. That split is what makes each refusal reachable from a unit test.
//! - **No badge.** Upstream's `LlmModelInfo.badge` is a selector decoration whose text is
//!   `LocalizedText` (a string *or* a locale map). That whole negotiation already lives in
//!   [`crate::deepseek_files_gap`] as `ModelBadge`/`ListedBadge`, where the listing that declares it
//!   is decoded; copying it here would give a client two sources for one decoration.
//! - **No remote/RPC surface.** Turning these into `llm.*` methods is wiring, and wiring is the
//!   integrator's: this crate declares the module and re-exports it.
//!
//! # Where the shapes came from
//!
//! ```text
//! ProviderInfo        <- LlmProviderInfo
//! ModelInfo           <- LlmModelInfo (minus badge, see above)
//! ResolvedModelInfo   <- LlmResolvedModelInfo
//! ModelContext        <- LlmModelContext
//! ModelReasoningInfo  <- LlmModelReasoningInfo
//! ReasoningEffortInfo <- LlmReasoningEffortInfo
//! InputModality       <- ModelModality
//! SystemPromptUpdate  <- SystemPromptUpdate        ('in-history')
//! ToolUpdate          <- ToolUpdate                ('in-history' | 'addition-only')
//! CallControls        <- LlmCallControls
//! LlmCallConfig       <- LlmCallConfig
//! ConfigureCall       <- ConfigureCall
//! AdapterDefaults     <- LlmCallConfigAdapterDefaults  (reused, not copied)
//! ConfigurableProvider<- LlmConfigurableProvider
//! NativeAuth          <- LlmNativeAuth
//! DiscoveryRequest    <- LlmModelDiscoveryRequest
//! DiscoveredModel     <- LlmDiscoveredModel
//! Adapter             <- LlmAdapter (minus the stream, which is the provider's)
//! AdapterHandle       <- AdapterRegistrationHandle
//! DirectoryHandle     <- DirectoryRegistrationHandle
//! PreparedCall        <- PreparedLlmCall (minus the stream)
//! Runtime             <- LlmRuntime (minus the Cordis service)
//! RuntimeError        <- LlmError, one variant per raised code
//! ```

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use girder_core::request_header_gap::AdapterDefaults;
use girder_core::{MAX_SAFE_INTEGER, RetryPolicy};

// =============================================================================================
// Vocabulary
// =============================================================================================

/// How a provider identifies itself to a selector.
///
/// `id` is checked against the key it was registered under, because a selector that offered an id
/// the router does not know would produce a choice that cannot be dispatched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderInfo {
    /// The provider id, which must equal the registration key.
    pub id: String,
    /// Human-readable name; must not be blank.
    pub name: String,
}

impl ProviderInfo {
    /// An identity whose display name is its id, which is the least-surprising default.
    #[must_use]
    pub fn named(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
        }
    }
}

/// What a model accepts as input — upstream `ModelModality`.
///
/// Named `InputModality` rather than `Modality` to leave the crate's existing
/// [`crate::providers::Modality`] unambiguous: that one describes a route's capability in the
/// routing table, this one describes what an adapter *answered* when asked. They agree today and are
/// deliberately separate types, because one is a deployment fact and the other is third-party input
/// that has to be validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputModality {
    /// Text.
    Text,
    /// Images.
    Image,
}

impl InputModality {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
        }
    }

    /// Reads the wire spelling; an unknown modality reads as `None` rather than as text.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "text" => Some(Self::Text),
            "image" => Some(Self::Image),
            _ => None,
        }
    }
}

/// The context window an adapter reports — upstream `LlmModelContext`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelContext {
    /// The largest context the model accepts, in tokens. Must be positive.
    pub context_window: u64,
}

/// One selectable reasoning effort — upstream `LlmReasoningEffortInfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasoningEffortInfo {
    /// The id dispatch sends.
    pub id: String,
    /// Human-readable name; must not be blank.
    pub name: String,
    /// Explanation shown beside the choice, when the adapter supplied one.
    pub description: Option<String>,
}

/// The reasoning efforts one model offers — upstream `LlmModelReasoningInfo`.
///
/// `efforts` is refused when empty: an adapter that says "this model reasons" and then names no
/// level has told the caller nothing it can dispatch, and upstream throws for exactly that reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelReasoningInfo {
    /// The offered levels, in display order.
    pub efforts: Vec<ReasoningEffortInfo>,
    /// The level chosen when the caller names none; must be one of `efforts`.
    pub default_effort: Option<String>,
}

impl ModelReasoningInfo {
    /// Whether `effort` is one of the offered levels.
    #[must_use]
    pub fn offers(&self, effort: &str) -> bool {
        self.efforts.iter().any(|known| known.id == effort)
    }
}

/// How a later system prompt reaches the model — upstream `SystemPromptUpdate`.
///
/// One variant, because upstream types the field as the literal `'in-history'`: the *absence* of the
/// field is the other state, and an enum with one variant plus `Option` states it exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemPromptUpdate {
    /// A later system prompt becomes a turn in the history.
    InHistory,
}

impl SystemPromptUpdate {
    /// The wire spelling.
    ///
    /// An associated function on the value rather than on the type, for symmetry with
    /// [`ToolUpdate::as_str`], which genuinely branches. The single-variant shape is upstream's: the
    /// field is the literal `'in-history'`, so absence — not another variant — is the other state.
    #[must_use]
    #[allow(
        clippy::unused_self,
        reason = "kept as a method so both update modes are spelled the same way at every call site"
    )]
    pub const fn as_str(self) -> &'static str {
        "in-history"
    }
}

/// How a tool-list change reaches the model — upstream `ToolUpdate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolUpdate {
    /// Tool changes ride the history.
    InHistory,
    /// Mid-conversation additions are allowed.
    AdditionOnly,
}

impl ToolUpdate {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InHistory => "in-history",
            Self::AdditionOnly => "addition-only",
        }
    }
}

/// A model as an adapter lists it — upstream `LlmModelInfo`.
///
/// `input_modalities` is `Option` rather than a defaulted vector so that "the adapter did not say"
/// stays distinguishable from "the adapter said text only": the first defers to the route's own
/// declaration, the second overrides it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// The provider that serves it; must equal the provider it was listed under.
    pub provider: String,
    /// The id a request names; must not be blank and must be unique within the listing.
    pub id: String,
    /// Human-readable name; must not be blank.
    pub name: String,
    /// Description shown in a selector, when the adapter supplied one.
    pub description: Option<String>,
    /// Accepted input types, when disclosed.
    pub input_modalities: Option<Vec<InputModality>>,
}

impl ModelInfo {
    /// A text-capable model with this id, named after the id.
    #[must_use]
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            provider: provider.into(),
            name: id.clone(),
            id,
            description: None,
            input_modalities: None,
        }
    }

    /// Sets the display name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Declares the accepted input types.
    #[must_use]
    pub fn with_inputs(mut self, inputs: Vec<InputModality>) -> Self {
        self.input_modalities = Some(inputs);
        self
    }
}

/// A model with the facts a call is resolved against — upstream `LlmResolvedModelInfo`.
///
/// The identity half is nested rather than repeated, so a caller cannot build a "resolved" value
/// whose id disagrees with its name-carrying twin. Three facts beyond identity matter:
/// `default_max_tokens` (materialized when the caller named none), `context` (what compaction
/// measures against), and `reasoning` (what an effort is validated against).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModelInfo {
    /// Provider, id, name, description, modalities.
    pub info: ModelInfo,
    /// The context window, when the adapter knows it.
    pub context: Option<ModelContext>,
    /// The per-request output cap this model supplies when the caller names none.
    pub default_max_tokens: Option<u64>,
    /// The reasoning efforts this model offers, when it reasons.
    pub reasoning: Option<ModelReasoningInfo>,
    /// How a later system prompt reaches it, when the adapter declared a mode.
    pub system_prompt_update: Option<SystemPromptUpdate>,
    /// How a tool-list change reaches it, when the adapter declared a mode.
    pub tool_update: Option<ToolUpdate>,
}

impl ResolvedModelInfo {
    /// A model known by id alone, with every optional fact absent.
    #[must_use]
    pub fn bare(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            info: ModelInfo::new(provider, model),
            context: None,
            default_max_tokens: None,
            reasoning: None,
            system_prompt_update: None,
            tool_update: None,
        }
    }

    /// The provider id.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.info.provider
    }

    /// The model id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// Declares the context window.
    #[must_use]
    pub const fn with_context(mut self, context_window: u64) -> Self {
        self.context = Some(ModelContext { context_window });
        self
    }

    /// Declares the default output cap.
    #[must_use]
    pub const fn with_default_max_tokens(mut self, max_tokens: u64) -> Self {
        self.default_max_tokens = Some(max_tokens);
        self
    }

    /// Declares the reasoning efforts.
    #[must_use]
    pub fn with_reasoning(mut self, reasoning: ModelReasoningInfo) -> Self {
        self.reasoning = Some(reasoning);
        self
    }
}

// =============================================================================================
// Call configuration
// =============================================================================================

/// The sampling controls a caller may propose — upstream `LlmCallControls`.
///
/// Temperature is `f32` to match [`crate::request_header_gap::HeaderConfig`], which is where a
/// request's controls are recorded: one precision for one number, so a header written from a
/// resolved config compares equal to the config it came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallControls {
    /// The adapter-owned reasoning effort, when one was chosen.
    pub reasoning_effort: Option<String>,
    /// Sampling temperature, when the caller proposed one.
    pub temperature: Option<f32>,
    /// The output-token cap, when the caller proposed one.
    pub max_tokens: Option<u32>,
    /// Stop sequences, when the caller proposed any.
    ///
    /// `None` and `Some([])` are different: absence is silence, an empty list is an explicit "no
    /// stop sequences". Upstream's `callConfigEquals` draws the same line.
    pub stop: Option<Vec<String>>,
}

/// A route plus the controls it runs under — upstream `LlmCallConfig`.
///
/// Complete by construction: provider and model are required, so "a config with no route" is not
/// representable and [`Runtime::resolve_call_config`] never has to answer for one.
#[derive(Clone, Debug, PartialEq)]
pub struct LlmCallConfig {
    /// The registered provider route.
    pub provider: String,
    /// The provider-owned model id.
    pub model: String,
    /// The adapter-owned reasoning effort, when one was chosen.
    pub reasoning_effort: Option<String>,
    /// Sampling temperature, when proposed.
    pub temperature: Option<f32>,
    /// The output-token cap, when proposed.
    pub max_tokens: Option<u32>,
    /// Stop sequences, when proposed.
    pub stop: Option<Vec<String>>,
}

impl LlmCallConfig {
    /// A config naming a route with every control unset.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
            stop: None,
        }
    }

    /// Sets the reasoning effort.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Sets the output cap.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the sampling temperature.
    #[must_use]
    pub const fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// The controls half, without the route.
    #[must_use]
    pub fn controls(&self) -> CallControls {
        CallControls {
            reasoning_effort: self.reasoning_effort.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            stop: self.stop.clone(),
        }
    }

    /// This config with a different controls half, keeping the route.
    #[must_use]
    pub fn with_controls(&self, controls: CallControls) -> Self {
        Self {
            provider: self.provider.clone(),
            model: self.model.clone(),
            reasoning_effort: controls.reasoning_effort,
            temperature: controls.temperature,
            max_tokens: controls.max_tokens,
            stop: controls.stop,
        }
    }
}

/// A caller's last look at the controls before dispatch — upstream `ConfigureCall`.
///
/// The signature carries the resolved model deliberately: "choose the reasoning effort this model
/// offers" is the one decision that cannot be made before the model is resolved, and passing the
/// controls alone would force the callback to resolve the model a second time.
pub type ConfigureCall<'a> =
    dyn Fn(&CallControls, &ResolvedModelInfo) -> CallControls + Send + Sync + 'a;

/// Whether two call configs describe the same request — upstream `callConfigEquals`.
///
/// Every control is compared, not just the route: a change of temperature is as much a different
/// request as a change of model, and a dispatcher that only compared provider and model would send
/// a stream under controls the retry policy never priced.
///
/// Temperature compares with IEEE equality on purpose. `NaN` is therefore always a change (a `NaN`
/// temperature cannot be dispatched, and reporting it unchanged would keep a bad request alive), and
/// `-0.0` equals `0.0` (the sign of zero is not a sampling decision). That is exactly what upstream's
/// `!==` does.
#[must_use]
#[allow(
    clippy::float_cmp,
    reason = "IEEE equality is the specified behaviour: NaN is always a change, -0.0 equals 0.0"
)]
pub fn call_config_equals(left: &LlmCallConfig, right: &LlmCallConfig) -> bool {
    left.provider == right.provider
        && left.model == right.model
        && left.reasoning_effort == right.reasoning_effort
        && left.temperature == right.temperature
        && left.max_tokens == right.max_tokens
        && match (&left.stop, &right.stop) {
            (None, None) => true,
            (Some(left), Some(right)) => left == right,
            (None, Some(_)) | (Some(_), None) => false,
        }
}

// =============================================================================================
// Errors
// =============================================================================================

/// A refused runtime operation, naming the code that a retry policy or a UI keys on.
///
/// One variant per code, so [`Self::code`] is total and a caller that matches on the reason also
/// gets the code. The alternative — a string code plus a free-form message — is what upstream does
/// and what makes a mis-cased code a silent no-op in a policy's `retryableCodes` list.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuntimeError {
    /// A registration named no providers.
    #[error("an adapter must register at least one provider")]
    EmptyAdapterRegistration,

    /// A registration named a provider whose name is blank.
    #[error("adapter provider names must be non-empty")]
    EmptyProviderName,

    /// The adapter answered with metadata that does not describe the provider it was asked about.
    #[error(
        "adapter metadata for provider {provider:?} must preserve its id and have a non-empty name"
    )]
    InvalidAdapterMetadata {
        /// The provider the metadata was supposed to describe.
        provider: String,
    },

    /// The provider already has an adapter that this registration does not own.
    #[error("an adapter for provider {provider:?} is already registered")]
    DuplicateAdapter {
        /// The provider already taken.
        provider: String,
    },

    /// A registration was disposed and then asked to change its routes.
    #[error("a disposed adapter registration cannot replace its routes")]
    DisposedAdapterRegistration,

    /// A configurable-provider registration was disposed and then asked to change its entries.
    #[error("this configurable-provider registration was disposed")]
    DisposedDirectoryRegistration,

    /// A configurable-provider entry is missing one of its three required names.
    #[error("configurable providers need a non-empty provider, displayName, and settingsNs")]
    InvalidDirectoryEntry,

    /// A configurable-provider entry has an empty settings-path segment.
    #[error("configurable provider {provider:?} has an empty settingsPath segment")]
    EmptySettingsPathSegment {
        /// The provider at fault.
        provider: String,
    },

    /// A configurable-provider registration declared no entries.
    #[error("a configurable-provider registration must declare at least one provider")]
    EmptyDirectoryRegistration,

    /// The configurable provider is already declared by a registration this one does not own.
    #[error("configurable provider {provider:?} is already declared")]
    DuplicateDirectoryEntry {
        /// The provider already declared.
        provider: String,
    },

    /// A discovery registration named an empty settings namespace.
    #[error("model discovery needs a non-empty settings namespace")]
    EmptyDiscoveryNamespace,

    /// That namespace already has a discovery this registration does not own.
    #[error("model discovery for {settings_ns:?} is already registered")]
    DuplicateDiscovery {
        /// The namespace already taken.
        settings_ns: String,
    },

    /// No discovery is registered for the namespace.
    #[error("no model discovery is registered for {settings_ns:?}")]
    NoDiscovery {
        /// The namespace asked about.
        settings_ns: String,
    },

    /// A discovery request named neither a provider route nor a base URL, so there is nothing to ask.
    #[error("model discovery needs a provider route or a baseURL")]
    DiscoveryWithoutRoute,

    /// No adapter is registered for the provider.
    #[error("no adapter registered for provider {provider:?}")]
    NoAdapter {
        /// The provider asked for.
        provider: String,
    },

    /// The adapter returned exact-model metadata that does not describe the model asked about.
    #[error(
        "adapter returned invalid exact model metadata for provider {provider:?} model {model:?}"
    )]
    InvalidModelInfo {
        /// The provider asked about.
        provider: String,
        /// The model asked about.
        model: String,
    },

    /// The adapter returned a context window that is not a positive integer.
    #[error("adapter returned invalid context metadata for provider {provider:?} model {model:?}")]
    InvalidModelContext {
        /// The provider asked about.
        provider: String,
        /// The model asked about.
        model: String,
    },

    /// The adapter returned a default output cap that is not a positive safe integer.
    #[error("adapter returned invalid default maxTokens for provider {provider:?} model {model:?}")]
    InvalidModelMaxTokens {
        /// The provider asked about.
        provider: String,
        /// The model asked about.
        model: String,
    },

    /// The adapter returned reasoning metadata that cannot be offered or defaulted to.
    #[error(
        "adapter returned invalid reasoning metadata for provider {provider:?} model {model:?}"
    )]
    InvalidModelReasoning {
        /// The provider asked about.
        provider: String,
        /// The model asked about.
        model: String,
    },

    /// The requested (or defaulted) effort is not one this model offers.
    #[error("provider {provider:?} model {model:?} does not support reasoning effort {effort:?}")]
    UnsupportedReasoningEffort {
        /// The provider.
        provider: String,
        /// The model.
        model: String,
        /// The effort that was asked for.
        effort: String,
    },

    /// The adapter's catalog listing held a row that does not describe the provider it is under.
    #[error("adapter returned invalid or duplicate model metadata for provider {provider:?}")]
    InvalidCatalog {
        /// The provider whose listing held the row.
        provider: String,
    },

    /// A prepared call was dispatched more than once.
    #[error("a prepared LLM call can only be dispatched once")]
    PreparedCallAlreadyDispatched,

    /// A prepared call was dispatched with a config other than the one it was prepared under.
    #[error("prepared LLM call config changed before adapter dispatch")]
    PreparedCallConfigChanged,
}

impl RuntimeError {
    /// The failure code this refusal raises.
    ///
    /// Every string here is an entry of [`crate::llm_extra_gap::FAILURE_CODES`]; the test suite
    /// asserts that rather than trusting this comment, because a code outside the table would be
    /// invisible to a retry policy and to the settings surface that lists the choices.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::EmptyAdapterRegistration
            | Self::EmptyProviderName
            | Self::InvalidAdapterMetadata { .. } => "INVALID_ADAPTER",
            Self::DuplicateAdapter { .. } => "DUPLICATE_ADAPTER",
            Self::DisposedAdapterRegistration | Self::DisposedDirectoryRegistration => {
                "REGISTRATION_DISPOSED"
            }
            Self::InvalidDirectoryEntry
            | Self::EmptySettingsPathSegment { .. }
            | Self::EmptyDirectoryRegistration => "INVALID_DIRECTORY",
            Self::DuplicateDirectoryEntry { .. } => "DUPLICATE_DIRECTORY",
            Self::EmptyDiscoveryNamespace | Self::DiscoveryWithoutRoute => "INVALID_DISCOVERY",
            Self::DuplicateDiscovery { .. } => "DUPLICATE_DISCOVERY",
            Self::NoDiscovery { .. } => "NO_DISCOVERY",
            Self::NoAdapter { .. } => "NO_ADAPTER",
            Self::InvalidModelInfo { .. } => "INVALID_MODEL_INFO",
            Self::InvalidModelContext { .. } => "INVALID_MODEL_CONTEXT",
            Self::InvalidModelMaxTokens { .. } => "INVALID_MODEL_MAX_TOKENS",
            Self::InvalidModelReasoning { .. } => "INVALID_MODEL_REASONING",
            Self::UnsupportedReasoningEffort { .. } => "UNSUPPORTED_REASONING_EFFORT",
            Self::InvalidCatalog { .. } => "INVALID_CATALOG",
            Self::PreparedCallAlreadyDispatched | Self::PreparedCallConfigChanged => {
                "INVALID_PREPARED_CALL"
            }
        }
    }
}

// =============================================================================================
// Adapters
// =============================================================================================

/// A source of model responses, as the runtime needs to see it — upstream `LlmAdapter`.
///
/// The streaming half is deliberately absent. In this crate a stream is
/// [`crate::llm::LlmProvider::stream`], which takes a [`crate::llm::LlmRequest`] and produces
/// [`crate::llm::ChunkStream`]; re-declaring it here would give every provider two entry points and
/// no way to keep them in agreement. What this trait carries is the *metadata* a call is resolved
/// against, which is the part the runtime owns.
///
/// Every method has a default that is correct for an adapter which knows nothing but its ids, so an
/// implementation that only streams is three lines. Defaults are also what makes the validation
/// meaningful: a default that upstream would have to write by hand is a default nobody forgets to
/// make consistent.
pub trait Adapter: Send + Sync {
    /// How this adapter describes `provider`.
    ///
    /// The default answers the identity, which registers cleanly. An adapter with a marketing name
    /// overrides it; one that answers a *different* id is refused at registration, because a
    /// selector keyed on the id would then offer a route the runtime cannot dispatch.
    fn provider_info(&self, provider: &str) -> ProviderInfo {
        ProviderInfo::named(provider)
    }

    /// The retry policy this provider runs under, when the adapter owns one.
    ///
    /// `None` defers to [`RetryPolicy::default`], which is what a provider with no opinion should
    /// get: the runtime's default and a hand-written duplicate of it are the same numbers today and
    /// would not stay that way.
    fn provider_retry_policy(&self, _provider: &str) -> Option<RetryPolicy> {
        None
    }

    /// The exact facts of one model, as the adapter knows them.
    ///
    /// Returned *unvalidated*: [`Runtime::resolve_model_info`] is the judge. An adapter that had to
    /// validate its own answer would be the only thing standing between third-party metadata and a
    /// bad request, which is the arrangement upstream avoids for the same reason.
    fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<ResolvedModelInfo, RuntimeError> {
        Ok(ResolvedModelInfo::bare(provider, model))
    }
}

/// One registered provider: its adapter, identity, and policy.
#[derive(Clone)]
struct Registration {
    adapter: Arc<dyn Adapter>,
    info: ProviderInfo,
    retry_policy: RetryPolicy,
}

/// A live adapter registration — upstream `AdapterRegistrationHandle`.
///
/// Opaque and owned by the runtime: disposal and route replacement both go through it, so the set of
/// providers a handle owns is the runtime's record rather than something the caller re-states and
/// gets wrong.
pub struct AdapterHandle {
    providers: Vec<String>,
    adapter: Arc<dyn Adapter>,
    disposed: bool,
}

impl std::fmt::Debug for AdapterHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdapterHandle")
            .field("providers", &self.providers)
            .field("disposed", &self.disposed)
            .finish_non_exhaustive()
    }
}

impl AdapterHandle {
    /// The providers this handle currently routes.
    ///
    /// Ordered, so a caller comparing two registrations is comparing routes rather than a hash
    /// iteration order. Empty after disposal.
    #[must_use]
    pub fn providers(&self) -> &[String] {
        &self.providers
    }

    /// Whether the registration has been disposed.
    #[must_use]
    pub const fn is_disposed(&self) -> bool {
        self.disposed
    }
}

/// A configurable provider a settings surface may render — upstream `LlmConfigurableProvider`.
///
/// The path members are vectors rather than `&str`s because a provider's settings location is a
/// *path*: `["models", "deepseek"]` names a nested settings object, and a caller that had to join
/// and re-split it would get `/` inside a segment wrong eventually.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigurableProvider {
    /// The provider id this entry configures.
    pub provider: String,
    /// Human-readable name for the settings page.
    pub display_name: String,
    /// The settings namespace the entry reads.
    pub settings_ns: String,
    /// Where in that namespace the entry lives; every segment must be non-empty.
    pub settings_path: Vec<String>,
    /// Whether this build declares the provider itself rather than reading it from a plugin.
    pub declared: bool,
    /// The base URL a caller starts from, when there is a canonical one.
    pub default_base_url: Option<String>,
    /// Environment variables that may carry the base URL, in precedence order.
    pub base_url_env: Option<Vec<String>>,
    /// The native sign-in this provider supports, when it supports one.
    pub native_auth: Option<NativeAuth>,
    /// A configuration problem to show beside the entry, when it has one.
    pub error: Option<String>,
}

impl ConfigurableProvider {
    /// An entry naming a provider and the namespace that configures it.
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        display_name: impl Into<String>,
        settings_ns: impl Into<String>,
        settings_path: Vec<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            display_name: display_name.into(),
            settings_ns: settings_ns.into(),
            settings_path,
            declared: false,
            default_base_url: None,
            base_url_env: None,
            native_auth: None,
            error: None,
        }
    }
}

/// How a provider's own credentials are obtained — upstream `LlmNativeAuth`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeAuth {
    /// Environment variables that may carry the credential, in precedence order.
    pub env: Vec<String>,
    /// Whether the provider supports an OAuth sign-in.
    pub oauth: bool,
}

/// A live configurable-provider registration — upstream `DirectoryRegistrationHandle`.
pub struct DirectoryHandle {
    providers: Vec<String>,
    disposed: bool,
}

impl std::fmt::Debug for DirectoryHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DirectoryHandle")
            .field("providers", &self.providers)
            .field("disposed", &self.disposed)
            .finish()
    }
}

impl DirectoryHandle {
    /// The providers this registration declares.
    #[must_use]
    pub fn providers(&self) -> &[String] {
        &self.providers
    }

    /// Whether the registration has been disposed.
    #[must_use]
    pub const fn is_disposed(&self) -> bool {
        self.disposed
    }
}

/// What a discovery call may name.
///
/// Both route members are optional because a user building a draft has neither yet, which is why
/// [`Runtime::discover_models`] requires *one of* them rather than both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryRequest {
    /// The provider route, when the draft names one.
    pub provider: Option<String>,
    /// The endpoint, when the draft names one instead.
    pub base_url: Option<String>,
    /// The wire protocol to interrogate.
    pub api: Option<String>,
    /// The credential to send, when one is available.
    pub api_key: Option<String>,
}

impl DiscoveryRequest {
    /// Whether the request names somewhere to ask.
    #[must_use]
    pub fn has_route(&self) -> bool {
        self.provider.as_ref().is_some_and(|name| !name.is_empty())
            || self.base_url.as_ref().is_some_and(|url| !url.is_empty())
    }
}

/// One model a listing disclosed — upstream `LlmDiscoveredModel`.
///
/// Deliberately not [`crate::provider_catalog_gap::DiscoveredModel`]: that type requires a name and
/// is the *result* of reading a wire listing, while this one is what a provider's discovery call
/// answered and may omit everything but the id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// The id a request names.
    pub id: String,
    /// A human-readable name, when the provider supplied one.
    pub name: Option<String>,
    /// The context window, when disclosed.
    pub context_window: Option<u64>,
    /// The output cap, when disclosed.
    pub max_tokens: Option<u64>,
    /// Accepted input types, when disclosed.
    pub input_modalities: Option<Vec<InputModality>>,
}

impl DiscoveredModel {
    /// A model known by id alone.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }
}

/// A live discovery registration.
pub struct DiscoveryHandle {
    settings_ns: String,
    disposed: bool,
}

impl std::fmt::Debug for DiscoveryHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiscoveryHandle")
            .field("settings_ns", &self.settings_ns)
            .field("disposed", &self.disposed)
            .finish()
    }
}

impl DiscoveryHandle {
    /// The namespace this registration answers for.
    #[must_use]
    pub fn settings_ns(&self) -> &str {
        &self.settings_ns
    }

    /// Whether the registration has been disposed.
    #[must_use]
    pub const fn is_disposed(&self) -> bool {
        self.disposed
    }
}

// =============================================================================================
// The prepared call
// =============================================================================================

/// A resolved call that has not been dispatched — upstream `PreparedLlmCall`.
///
/// Prepared in two senses. First, every decision that could be wrong *before* a request goes out has
/// already been made and checked: the model is normalized, the output cap is materialized, the
/// effort is one the model offers. Second, the record of what was materialized is kept
/// ([`Self::adapter_defaults`]), so a header written from this call says which numbers the adapter
/// supplied rather than implying the user chose them.
///
/// The dispatch guard is the one piece of mutable state: [`Self::begin_dispatch`] takes `&mut self`
/// and refuses a second call and a changed config. Making the guard a method rather than a flag a
/// caller could set means a caller cannot forget it — there is no other way to obtain the config
/// that a stream is supposed to be sent with.
#[derive(Clone, Debug)]
pub struct PreparedCall {
    config: LlmCallConfig,
    retry_policy: RetryPolicy,
    context: Option<ModelContext>,
    input_modalities: Option<Vec<InputModality>>,
    system_prompt_update: Option<SystemPromptUpdate>,
    tool_update: Option<ToolUpdate>,
    adapter_defaults: AdapterDefaults,
    dispatched: bool,
}

impl PreparedCall {
    /// The config every dispatch must present.
    #[must_use]
    pub fn config(&self) -> &LlmCallConfig {
        &self.config
    }

    /// The policy the caller's retry loop should run under.
    ///
    /// Resolved once here rather than read per attempt: an adapter whose policy changes between
    /// attempts would make the retry counter's arithmetic describe two policies at once.
    #[must_use]
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }

    /// The context window, when the model knows it.
    #[must_use]
    pub fn context(&self) -> Option<ModelContext> {
        self.context
    }

    /// The accepted input types, when the model disclosed them.
    #[must_use]
    pub fn input_modalities(&self) -> Option<&[InputModality]> {
        self.input_modalities.as_deref()
    }

    /// How a later system prompt reaches the model, when the adapter declared a mode.
    #[must_use]
    pub fn system_prompt_update(&self) -> Option<SystemPromptUpdate> {
        self.system_prompt_update
    }

    /// How a tool-list change reaches the model, when the adapter declared a mode.
    #[must_use]
    pub fn tool_update(&self) -> Option<ToolUpdate> {
        self.tool_update
    }

    /// Which control fields the adapter materialized rather than the caller proposing.
    #[must_use]
    pub fn adapter_defaults(&self) -> AdapterDefaults {
        self.adapter_defaults
    }

    /// Whether a dispatch has already happened.
    #[must_use]
    pub const fn is_dispatched(&self) -> bool {
        self.dispatched
    }

    /// Claims the single dispatch this call is allowed.
    ///
    /// The caller passes the config it is about to send with, and both halves of the contract are
    /// enforced here: a second call is refused, and a config that differs from the prepared one in
    /// any field — temperature and stop included — is refused.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::PreparedCallAlreadyDispatched`] on the second and every later call, and
    /// [`RuntimeError::PreparedCallConfigChanged`] when `config` is not the prepared one. Both raise
    /// `INVALID_PREPARED_CALL`, because a caller's response to either is the same: do not send.
    pub fn begin_dispatch(&mut self, config: &LlmCallConfig) -> Result<(), RuntimeError> {
        if self.dispatched {
            return Err(RuntimeError::PreparedCallAlreadyDispatched);
        }
        if !call_config_equals(config, &self.config) {
            return Err(RuntimeError::PreparedCallConfigChanged);
        }
        self.dispatched = true;
        Ok(())
    }
}

// =============================================================================================
// The runtime
// =============================================================================================

/// Provider adapters, the configurable-provider directory, and model discovery.
///
/// One value holding three tables rather than three types, because the three are read together: a
/// settings page asks "which providers can I configure" and "which have an adapter" in one render,
/// and a discovery that answered for a provider with no adapter is a fact about the same table.
///
/// Every table is a [`BTreeMap`], so iteration is sorted by id rather than by insertion order. That
/// is a deliberate deviation from upstream's `Map`: a registration whose order leaked into a
/// selector would make two processes with identical configuration render differently, and there is
/// no meaningful ordering to preserve.
#[derive(Default)]
pub struct Runtime {
    adapters: BTreeMap<String, Registration>,
    directory: BTreeMap<String, ConfigurableProvider>,
    discoveries: BTreeSet<String>,
    revision: u64,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("adapters", &self.adapters.keys().collect::<Vec<_>>())
            .field("directory", &self.directory.keys().collect::<Vec<_>>())
            .field("discoveries", &self.discoveries)
            .field("revision", &self.revision)
            .finish()
    }
}

impl Runtime {
    /// An empty runtime: no adapters, no directory entries, no discovery.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many route-changing commits have happened — upstream's `llm/adapters-updated` event,
    /// as a counter.
    ///
    /// A counter rather than a subscription list because a listener in this crate is a task, and a
    /// task needs a bus. The counter is the part a listener needs from the event ("did anything
    /// change since I last looked"), and it is the part a test can assert exactly.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    // -- adapters ---------------------------------------------------------------------------

    /// Registers an adapter for its providers — upstream `registerAdapter`.
    ///
    /// All-or-nothing: every provider name is validated, and every route is built, before the table
    /// is touched. A half-applied registration is the one outcome that cannot be retried (the arm
    /// that succeeded now answers `DUPLICATE_ADAPTER`), so it is the outcome this refuses to reach.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::EmptyAdapterRegistration`], [`RuntimeError::EmptyProviderName`],
    /// [`RuntimeError::InvalidAdapterMetadata`], and [`RuntimeError::DuplicateAdapter`].
    pub fn register_adapter(
        &mut self,
        providers: &[&str],
        adapter: Arc<dyn Adapter>,
    ) -> Result<AdapterHandle, RuntimeError> {
        let registrations = self.prepare_routes(providers, &adapter, &BTreeSet::new())?;
        let mut handle = AdapterHandle {
            providers: Vec::new(),
            adapter,
            disposed: false,
        };
        self.commit_routes(&mut handle, registrations);
        Ok(handle)
    }

    /// Changes which providers a live registration routes — upstream `handle.replace`.
    ///
    /// The same all-or-nothing rule as registration, and it is the rule that matters more here: a
    /// caller replacing `[a, b]` with `[a, c]` where `c` is taken must keep routing `b`, not end up
    /// routing `a` and neither of the others.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::DisposedAdapterRegistration`] once disposed, and every error
    /// [`Self::register_adapter`] raises. A failed replacement leaves the previous routes intact.
    pub fn replace_routes(
        &mut self,
        handle: &mut AdapterHandle,
        providers: &[&str],
    ) -> Result<(), RuntimeError> {
        if handle.disposed {
            return Err(RuntimeError::DisposedAdapterRegistration);
        }
        let owned: BTreeSet<String> = handle.providers.iter().cloned().collect();
        let registrations = self.prepare_routes(providers, &handle.adapter, &owned)?;
        self.commit_routes(handle, registrations);
        Ok(())
    }

    /// Releases every route a registration owns.
    ///
    /// Idempotent: disposing twice is the same as disposing once, because a caller's teardown path
    /// may legitimately run twice and an error there has no action attached to it.
    pub fn dispose_adapter(&mut self, handle: &mut AdapterHandle) {
        if handle.disposed {
            return;
        }
        for provider in &handle.providers {
            self.adapters.remove(provider);
        }
        handle.providers.clear();
        handle.disposed = true;
        self.bump();
    }

    /// Every registered provider, sorted by id — upstream `listProviders`.
    #[must_use]
    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        self.adapters
            .values()
            .map(|registration| registration.info.clone())
            .collect()
    }

    /// The retry policy a provider runs under — upstream `providerRetryPolicy`.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::NoAdapter`] when nothing is registered for it.
    pub fn provider_retry_policy(&self, provider: &str) -> Result<RetryPolicy, RuntimeError> {
        Ok(self.registration(provider)?.retry_policy.clone())
    }

    /// The exact facts of one model, normalized — upstream `resolveModelInfo`.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::NoAdapter`], whatever the adapter returned, and every refusal
    /// [`Self::normalize_model_info`] raises.
    pub fn resolve_model_info(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<ResolvedModelInfo, RuntimeError> {
        let registration = self.registration(provider)?;
        let resolved = registration.adapter.resolve_model(provider, model)?;
        Self::normalize_model_info(provider, model, resolved)
    }

    /// Normalizes one adapter answer — the validation ladder of upstream's `normalizeModelInfo`.
    ///
    /// A static function rather than a method because it needs nothing from the table, which is what
    /// makes every refusal reachable without a registration.
    ///
    /// The checks that survive typing are the *relational* and *numeric* ones. Upstream also
    /// re-checks `typeof name === 'string'` and that a mode is one of two spellings; here those are
    /// [`String`] and an enum, so a value that could fail them is not constructible and re-checking
    /// would be dead code. What cannot be typed away, and is therefore checked:
    ///
    /// - the answer describes **the provider and model that were asked about** (an adapter serving
    ///   two models can answer with the wrong one, and the id is what a route dispatch keys on),
    /// - the name is non-blank,
    /// - the context window is positive (zero makes compaction divide by nothing),
    /// - the default output cap is a positive safe integer (a value above 2^53 arriving as `f64`
    ///   would not survive the round trip),
    /// - the reasoning efforts are non-empty, have non-blank ids and names, have **unique** ids, and
    ///   the default names one of them (a duplicate id makes two choices indistinguishable; a
    ///   default outside the set is an effort that can never be dispatched).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidModelInfo`], [`RuntimeError::InvalidModelContext`],
    /// [`RuntimeError::InvalidModelMaxTokens`], or [`RuntimeError::InvalidModelReasoning`].
    pub fn normalize_model_info(
        provider: &str,
        model: &str,
        resolved: ResolvedModelInfo,
    ) -> Result<ResolvedModelInfo, RuntimeError> {
        let opaque = || RuntimeError::InvalidModelInfo {
            provider: provider.to_owned(),
            model: model.to_owned(),
        };
        if resolved.info.provider != provider || resolved.info.id != model {
            return Err(opaque());
        }
        if resolved.info.name.trim().is_empty() {
            return Err(opaque());
        }
        if let Some(context) = resolved.context {
            if context.context_window == 0 {
                return Err(RuntimeError::InvalidModelContext {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                });
            }
        }
        if let Some(max_tokens) = resolved.default_max_tokens {
            if max_tokens == 0 || max_tokens > MAX_SAFE_INTEGER {
                return Err(RuntimeError::InvalidModelMaxTokens {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                });
            }
        }
        if let Some(reasoning) = &resolved.reasoning {
            Self::check_reasoning(provider, model, reasoning)?;
        }
        Ok(resolved)
    }

    /// Validates a catalog listing — upstream's `listModels` normalization.
    ///
    /// Each row must describe the provider it was listed under, carry a non-blank id and name, and
    /// the ids must be unique. A row that fails is a **refusal, not a skip**: upstream throws
    /// `INVALID_CATALOG` because a listing with one bad row is not a shorter listing, it is an
    /// adapter that disagrees with itself, and dropping the row would hide that.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidCatalog`] naming `provider`.
    pub fn normalize_catalog(
        provider: &str,
        models: &[ModelInfo],
    ) -> Result<Vec<ModelInfo>, RuntimeError> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for model in models {
            if model.provider != provider
                || model.id.is_empty()
                || model.name.is_empty()
                || !seen.insert(model.id.as_str())
            {
                return Err(RuntimeError::InvalidCatalog {
                    provider: provider.to_owned(),
                });
            }
        }
        Ok(models.to_vec())
    }

    // -- call resolution --------------------------------------------------------------------

    /// A config with its defaults materialized and its effort validated —
    /// upstream `resolveCallConfig`.
    ///
    /// # Errors
    ///
    /// Every refusal [`Self::resolve_model_info`] raises, plus
    /// [`RuntimeError::UnsupportedReasoningEffort`].
    pub fn resolve_call_config(
        &self,
        config: &LlmCallConfig,
    ) -> Result<LlmCallConfig, RuntimeError> {
        let info = self.resolve_model_info(&config.provider, &config.model)?;
        Ok(Self::resolve_call_with_info(config, &info).config)
    }

    /// Resolves a call against an already-normalized model — upstream `resolveCallWithInfo`.
    ///
    /// The two rules it applies are worth stating because both are silent when wrong:
    ///
    /// 1. **An output cap the caller omitted is filled from the model.** A model that declares a
    ///    default and receives no cap is sent at whatever the provider guesses, which is a number
    ///    nobody in this process chose.
    /// 2. **A reasoning effort is checked against the model's own list.** A model that reasons
    ///    rejects an effort it does not offer; a model that does *not* reason rejects any effort at
    ///    all, including one the caller believed was universal. Both are the same refusal here
    ///    because the caller's fix is the same: pick another effort or none.
    ///
    /// A defaulted effort is written *into* the returned config so that the header recorded for the
    /// request names what was actually sent, and so that a dispatch equality check compares the same
    /// value the adapter will see.
    #[must_use]
    pub fn resolve_call_with_info(
        config: &LlmCallConfig,
        info: &ResolvedModelInfo,
    ) -> ResolvedCall {
        let max_tokens = config.max_tokens.or_else(|| {
            info.default_max_tokens
                .and_then(|tokens| u32::try_from(tokens).ok())
        });
        let requested = config.reasoning_effort.as_deref();
        let mut resolved = config.clone();
        resolved.max_tokens = max_tokens;
        match &info.reasoning {
            None => {
                // A model with no reasoning block does not reason at all, so *any* requested effort
                // is unsupported. Leaving it on the config would send a field the provider ignores,
                // and the header would record a choice that had no effect.
                if let Some(effort) = requested {
                    return ResolvedCall {
                        config: resolved,
                        context: info.context,
                        model_info: info.clone(),
                        unsupported: Some(RuntimeError::UnsupportedReasoningEffort {
                            provider: config.provider.clone(),
                            model: config.model.clone(),
                            effort: effort.to_owned(),
                        }),
                    };
                }
            }
            Some(reasoning) => {
                let effective = requested
                    .map(ToOwned::to_owned)
                    .or_else(|| reasoning.default_effort.clone());
                if let Some(effort) = &effective {
                    if !reasoning.offers(effort) {
                        return ResolvedCall {
                            config: resolved,
                            context: info.context,
                            model_info: info.clone(),
                            unsupported: Some(RuntimeError::UnsupportedReasoningEffort {
                                provider: config.provider.clone(),
                                model: config.model.clone(),
                                effort: effort.clone(),
                            }),
                        };
                    }
                    resolved.reasoning_effort = Some(effort.clone());
                }
            }
        }
        ResolvedCall {
            config: resolved,
            context: info.context,
            model_info: info.clone(),
            unsupported: None,
        }
    }

    /// Prepares a call for dispatch — upstream `prepareCall` without the stream.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::NoAdapter`], every refusal [`Self::resolve_model_info`] raises, and
    /// [`RuntimeError::UnsupportedReasoningEffort`].
    pub fn prepare_call(&self, config: &LlmCallConfig) -> Result<PreparedCall, RuntimeError> {
        self.prepare_call_with(config, None)
    }

    /// Prepares a call, letting the caller adjust the controls once the model is known.
    ///
    /// The callback runs **after** normalization and **before** resolution, which is the only order
    /// that lets it pick an effort from the model's own list. It is handed the controls only — not
    /// the route — because a callback that could change `provider` or `model` would be able to
    /// silently dispatch to a route the caller never selected.
    ///
    /// The adapter-default flags are computed from the config *the callback returned*: an effort the
    /// callback chose is the callback's, not the adapter's, and calling it a materialized default
    /// would blame the adapter for a decision made in this process.
    ///
    /// # Errors
    ///
    /// As [`Self::prepare_call`].
    pub fn prepare_call_with(
        &self,
        config: &LlmCallConfig,
        configure: Option<&ConfigureCall<'_>>,
    ) -> Result<PreparedCall, RuntimeError> {
        let registration = self.registration(&config.provider)?;
        let resolved = registration
            .adapter
            .resolve_model(&config.provider, &config.model)?;
        let model_info = Self::normalize_model_info(&config.provider, &config.model, resolved)?;
        let proposed = match configure {
            None => config.clone(),
            Some(configure) => config.with_controls(configure(&config.controls(), &model_info)),
        };
        let resolved_call = Self::resolve_call_with_info(&proposed, &model_info);
        if let Some(error) = resolved_call.unsupported {
            return Err(error);
        }
        let resolved_config = resolved_call.config;
        Ok(PreparedCall {
            adapter_defaults: AdapterDefaults {
                reasoning_effort: proposed.reasoning_effort.is_none()
                    && resolved_config.reasoning_effort.is_some(),
                max_tokens: proposed.max_tokens.is_none() && resolved_config.max_tokens.is_some(),
            },
            retry_policy: registration.retry_policy.clone(),
            context: resolved_call.context,
            input_modalities: model_info.info.input_modalities.clone(),
            system_prompt_update: model_info.system_prompt_update,
            tool_update: model_info.tool_update,
            config: resolved_config,
            dispatched: false,
        })
    }

    // -- configurable providers -------------------------------------------------------------

    /// Declares the providers a settings surface may configure — upstream
    /// `registerConfigurableProviders`.
    ///
    /// Takes the entries by value. Upstream has to deep-detach the nested arrays so a caller that
    /// mutates what it passed cannot mutate the registry; ownership makes that automatic, and the
    /// same all-or-nothing rule as adapter registration applies before anything is committed.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::EmptyDirectoryRegistration`], [`RuntimeError::InvalidDirectoryEntry`],
    /// [`RuntimeError::EmptySettingsPathSegment`], and [`RuntimeError::DuplicateDirectoryEntry`].
    pub fn register_configurable_providers(
        &mut self,
        entries: Vec<ConfigurableProvider>,
    ) -> Result<DirectoryHandle, RuntimeError> {
        let mut handle = DirectoryHandle {
            providers: Vec::new(),
            disposed: false,
        };
        let detached = self.prepare_directory(entries, &BTreeSet::new())?;
        self.commit_directory(&mut handle, detached);
        Ok(handle)
    }

    /// Replaces the entries a live registration declares.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::DisposedDirectoryRegistration`], and every error
    /// [`Self::register_configurable_providers`] raises. A failed replacement leaves the previous
    /// entries intact.
    pub fn replace_configurable_providers(
        &mut self,
        handle: &mut DirectoryHandle,
        entries: Vec<ConfigurableProvider>,
    ) -> Result<(), RuntimeError> {
        if handle.disposed {
            return Err(RuntimeError::DisposedDirectoryRegistration);
        }
        let owned: BTreeSet<String> = handle.providers.iter().cloned().collect();
        let detached = self.prepare_directory(entries, &owned)?;
        self.commit_directory(handle, detached);
        Ok(())
    }

    /// Releases every entry a registration declared. Idempotent.
    pub fn dispose_directory(&mut self, handle: &mut DirectoryHandle) {
        if handle.disposed {
            return;
        }
        for provider in &handle.providers {
            self.directory.remove(provider);
        }
        handle.providers.clear();
        handle.disposed = true;
        self.bump();
    }

    /// Every declared provider, sorted by id — upstream `listConfigurableProviders`.
    #[must_use]
    pub fn list_configurable_providers(&self) -> Vec<ConfigurableProvider> {
        self.directory.values().cloned().collect()
    }

    // -- discovery --------------------------------------------------------------------------

    /// Registers a discovery provider for a settings namespace — upstream `registerModelDiscovery`.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::EmptyDiscoveryNamespace`] and [`RuntimeError::DuplicateDiscovery`].
    pub fn register_model_discovery(
        &mut self,
        settings_ns: &str,
    ) -> Result<DiscoveryHandle, RuntimeError> {
        if settings_ns.is_empty() {
            return Err(RuntimeError::EmptyDiscoveryNamespace);
        }
        if self.discoveries.contains(settings_ns) {
            return Err(RuntimeError::DuplicateDiscovery {
                settings_ns: settings_ns.to_owned(),
            });
        }
        self.discoveries.insert(settings_ns.to_owned());
        Ok(DiscoveryHandle {
            settings_ns: settings_ns.to_owned(),
            disposed: false,
        })
    }

    /// Releases a discovery registration. Idempotent.
    pub fn dispose_model_discovery(&mut self, handle: &mut DiscoveryHandle) {
        if handle.disposed {
            return;
        }
        self.discoveries.remove(&handle.settings_ns);
        handle.disposed = true;
        self.bump();
    }

    /// Normalizes what a discovery call answered — upstream `discoverModels`'s tail.
    ///
    /// The lookups and the request validation happen here; the call itself is I/O and therefore the
    /// caller's, which is why the answers arrive as a slice. The tail is the part with the rules:
    /// a row with a blank id or a repeated one is **dropped rather than refused** (unlike a
    /// catalog), because a discovery answer is a suggestion list — one unusable row should not deny
    /// the user the rest of a working endpoint's models.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::NoDiscovery`] when the namespace has no registration, and
    /// [`RuntimeError::DiscoveryWithoutRoute`] when the request names nowhere to ask.
    pub fn discover_models(
        &self,
        settings_ns: &str,
        request: &DiscoveryRequest,
        discovered: &[DiscoveredModel],
    ) -> Result<Vec<DiscoveredModel>, RuntimeError> {
        if !self.discoveries.contains(settings_ns) {
            return Err(RuntimeError::NoDiscovery {
                settings_ns: settings_ns.to_owned(),
            });
        }
        if !request.has_route() {
            return Err(RuntimeError::DiscoveryWithoutRoute);
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut models = Vec::with_capacity(discovered.len());
        for model in discovered {
            if model.id.is_empty() || !seen.insert(model.id.as_str()) {
                continue;
            }
            models.push(model.clone());
        }
        Ok(models)
    }

    // -- internals --------------------------------------------------------------------------

    /// The registration for a provider, or the refusal that names it.
    fn registration(&self, provider: &str) -> Result<&Registration, RuntimeError> {
        self.adapters
            .get(provider)
            .ok_or_else(|| RuntimeError::NoAdapter {
                provider: provider.to_owned(),
            })
    }

    /// Builds the routes a registration would install, without touching the table.
    ///
    /// `owned` holds the providers the calling registration already routes: those may be
    /// re-registered, everything else may not. Validating here rather than during the commit is what
    /// makes the commit infallible, and therefore atomic.
    fn prepare_routes(
        &self,
        providers: &[&str],
        adapter: &Arc<dyn Adapter>,
        owned: &BTreeSet<String>,
    ) -> Result<Vec<(String, Registration)>, RuntimeError> {
        if providers.is_empty() {
            return Err(RuntimeError::EmptyAdapterRegistration);
        }
        let mut unique: BTreeSet<&str> = BTreeSet::new();
        let mut prepared = Vec::with_capacity(providers.len());
        for provider in providers {
            if provider.is_empty() {
                return Err(RuntimeError::EmptyProviderName);
            }
            if !unique.insert(provider)
                || (self.adapters.contains_key(*provider) && !owned.contains(*provider))
            {
                return Err(RuntimeError::DuplicateAdapter {
                    provider: (*provider).to_owned(),
                });
            }
            let info = adapter.provider_info(provider);
            if info.id != *provider || info.name.trim().is_empty() {
                return Err(RuntimeError::InvalidAdapterMetadata {
                    provider: (*provider).to_owned(),
                });
            }
            let retry_policy = adapter.provider_retry_policy(provider).unwrap_or_default();
            prepared.push((
                (*provider).to_owned(),
                Registration {
                    adapter: Arc::clone(adapter),
                    info,
                    retry_policy,
                },
            ));
        }
        Ok(prepared)
    }

    /// Installs a prepared route set, replacing whatever the handle owned. Infallible.
    fn commit_routes(
        &mut self,
        handle: &mut AdapterHandle,
        registrations: Vec<(String, Registration)>,
    ) {
        for provider in &handle.providers {
            self.adapters.remove(provider);
        }
        handle.providers.clear();
        for (provider, registration) in registrations {
            self.adapters.insert(provider.clone(), registration);
            handle.providers.push(provider);
        }
        self.bump();
    }

    /// Validates and detaches the entries a directory registration would install.
    fn prepare_directory(
        &self,
        entries: Vec<ConfigurableProvider>,
        owned: &BTreeSet<String>,
    ) -> Result<Vec<ConfigurableProvider>, RuntimeError> {
        if entries.is_empty() {
            return Err(RuntimeError::EmptyDirectoryRegistration);
        }
        let mut unique: BTreeSet<String> = BTreeSet::new();
        let mut prepared = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.provider.is_empty()
                || entry.display_name.is_empty()
                || entry.settings_ns.is_empty()
            {
                return Err(RuntimeError::InvalidDirectoryEntry);
            }
            if entry.settings_path.iter().any(String::is_empty) {
                return Err(RuntimeError::EmptySettingsPathSegment {
                    provider: entry.provider,
                });
            }
            let taken =
                self.directory.contains_key(&entry.provider) && !owned.contains(&entry.provider);
            if taken || !unique.insert(entry.provider.clone()) {
                return Err(RuntimeError::DuplicateDirectoryEntry {
                    provider: entry.provider,
                });
            }
            prepared.push(entry);
        }
        Ok(prepared)
    }

    /// Installs a prepared entry set, replacing whatever the handle owned. Infallible.
    fn commit_directory(
        &mut self,
        handle: &mut DirectoryHandle,
        entries: Vec<ConfigurableProvider>,
    ) {
        for provider in &handle.providers {
            self.directory.remove(provider);
        }
        handle.providers.clear();
        for entry in entries {
            handle.providers.push(entry.provider.clone());
            self.directory.insert(entry.provider.clone(), entry);
        }
        self.bump();
    }

    /// The reasoning-metadata half of [`Self::normalize_model_info`].
    fn check_reasoning(
        provider: &str,
        model: &str,
        reasoning: &ModelReasoningInfo,
    ) -> Result<(), RuntimeError> {
        let refuse = || RuntimeError::InvalidModelReasoning {
            provider: provider.to_owned(),
            model: model.to_owned(),
        };
        if reasoning.efforts.is_empty() {
            return Err(refuse());
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for effort in &reasoning.efforts {
            if effort.id.is_empty() || effort.name.trim().is_empty() || !seen.insert(&effort.id) {
                return Err(refuse());
            }
        }
        if let Some(default) = &reasoning.default_effort {
            if !seen.contains(default.as_str()) {
                return Err(refuse());
            }
        }
        Ok(())
    }

    /// Records a committed change.
    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// A call resolved against a model, or the reason its effort cannot be dispatched.
///
/// Returned rather than a bare `Result` because the resolution has already produced two facts a
/// caller wants even on failure: the config with its output cap materialized, and the model's
/// context. A caller reporting "this model does not offer `max`" can then name the efforts it *does*
/// offer without resolving the model a second time.
#[derive(Clone, Debug)]
pub struct ResolvedCall {
    /// The config with the caller's omissions filled in.
    pub config: LlmCallConfig,
    /// The model's context window, when known.
    pub context: Option<ModelContext>,
    /// The normalized model the call resolved against.
    pub model_info: ResolvedModelInfo,
    /// Why the effort cannot be dispatched, when it cannot.
    pub unsupported: Option<RuntimeError>,
}

impl ResolvedCall {
    /// The refusal, if the resolution produced one.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnsupportedReasoningEffort`] when the effort is not offerable.
    pub fn into_result(self) -> Result<Self, RuntimeError> {
        match self.unsupported {
            Some(error) => Err(error),
            None => Ok(self),
        }
    }
}
