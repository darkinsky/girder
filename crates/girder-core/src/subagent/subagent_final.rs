//! The model-facing delegation surface: child LLM route authorization, preset selection, the
//! description and prompt-section wording, and route discovery.
//!
//! # What the measurement said
//!
//! Upstream `packages/subagent` is 6,791 effective lines (11,077 raw) across 43 source files in
//! eleven packages, plus 64 lines of package directories that hold no source at all. Measured per
//! package, the local crate's coverage is *not* proportional to size, and the two places where a
//! large block is covered and a small one is not are the interesting ones:
//!
//! | package | effective lines | local counterpart |
//! | --- | --- | --- |
//! | `subagent` | 3,090 | partial: [`crate::subagent`], [`crate::subagent::depth`], [`crate::subagent::pool`], [`crate::subagent::interrupt`], [`crate::subagent::extra`], [`crate::subagent::tools`] |
//! | `subagent-codex` | 1,040 | decision layer only: [`crate::subagent::deep`] (`wire.ts`); `run.ts` + `index.ts` absent |
//! | `tool-subagent` | 705 | **absent**: this module |
//! | `subagent-claude-code` | 695 | absent (transport-shaped) |
//! | `subagent-acp` | 498 | absent (transport-shaped) |
//! | `subagent-dsh-sdk` | 384 | absent (transport-shaped) |
//! | `tool-subagent-control` | 307 | partial: list/wait in [`crate::subagent::tools`]; `send_message` has no channel |
//! | `subagent-fork-in-process` | 41 | absent (a 41-line shim over a fork path this crate has) |
//! | `subagent-spawn-in-process` | 31 | absent (same) |
//! | `tool-subagent-report` | 0 | nothing to port |
//! | `subagent-in-process-driver` | 0 | nothing to port |
//!
//! # The gap this module closes
//!
//! `packages/subagent/tool-subagent` is 705 effective lines and has **zero** local coverage: a
//! grep for every one of its exported symbols (`modelRouteKey`, `assertAllowedModelRoutes`,
//! `requestedAgentOptions`, `assertAllowedModelSelection`, `hasConfiguredLlmSelection`,
//! `preflightChildLlmRoute`, `hasDelegationModelRequest`, `SubagentPresetSelection`,
//! `allowedAgentPresets`, `list_subagent_models`, `modelSelectionPolicy`) returned no hits before
//! this file. Only the *capability flag* `Capabilities::agent_preset` existed, and nothing ever
//! produced an [`AgentOptions`] for a child at all — [`crate::subagent::delegate`] runs every child
//! on the parent's route with no way to select, authorize, or refuse a different one.
//!
//! That block is pure decision logic, which is exactly the part a port can get wrong silently:
//!
//! 1. **Authorization** (`model-selection.ts`): an explicit `provider`/`model`/`reasoning_effort`
//!    triple from model-facing JSON is validated, then checked against a per-Session allowlist.
//!    Omitted policy means *only the parent's route* — not "anything". The distinction matters
//!    because the tool's arguments are model output, and a missing policy is the shipped default.
//! 2. **Preflight** (`preflightChildLlmRoute`): before a child is created, the effective route is
//!    resolved through the live adapter, and the effort inherits from the parent only when the
//!    route did not change. A changed route that silently inherited the parent's effort would run
//!    the child at an effort its model may not advertise.
//! 3. **Presets** (`preset-selection.ts`): a deployment default plus an allowlist; a provider that
//!    inherits the parent's conversation cannot honor an independent preset, so the *provider* is
//!    refused rather than the request being quietly downgraded.
//! 4. **Wording** (`tool-subagent/src/index.ts`): four conditionally concatenated fragments. Each
//!    one states something a model will act on, and each one is false in a different configuration
//!    (a fork told "it does not see this conversation" restates everything; a non-continuable
//!    backend told "use `send_message`" offers a verb that does not exist).
//! 5. **Discovery** (`list-models.ts`): three modes over the same allowlist, where the *error*
//!    strings are what make a wrong route correctable by the model.
//!
//! # What stays out
//!
//! The Cordis service wiring (`Service.init`, `ctx.on('agent/created')`, projection registration,
//! `ctx.effect` disposal) is not decision logic and is not ported here; this module carries the
//! policy state machine those hooks drive ([`apply_policy_event`], [`policy_for_child`],
//! [`policy_for_new_top_level`]) with the event-shaped inputs spelled out, so the wiring binds to
//! pure functions instead of embedding them.

use std::collections::BTreeSet;

use girder_core::{AgentOptions, SubagentCapabilities};

/// The separator inside a route identity.
///
/// A NUL byte cannot appear in a provider or model id that a downstream transport can carry, so a
/// key built from it cannot collide: `("ab", "c")` and `("a", "bc")` stay distinct, which a `"/"` or
/// `":"` separator would not guarantee. [`AllowedModelRoute::new`] rejects a field containing it so
/// the property holds for every route this module will ever compare.
const KEY_SEPARATOR: char = '\0';

/// One exact child LLM route a user setting authorizes.
///
/// Construction validates, because an empty provider or model id is not a route — it is the state a
/// fallback chain produces when it has one half and not the other, and such a request cannot be
/// routed at all.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AllowedModelRoute {
    provider: String,
    model: String,
}

impl AllowedModelRoute {
    /// Builds a route from a provider and model id.
    ///
    /// # Errors
    ///
    /// [`RouteError::EmptyProvider`] or [`RouteError::EmptyModel`] for a blank id, and
    /// [`RouteError::Separator`] when a field contains the identity separator (which would let two
    /// different routes share a key).
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Result<Self, RouteError> {
        let provider = provider.into();
        let model = model.into();
        if provider.is_empty() {
            return Err(RouteError::EmptyProvider);
        }
        if model.is_empty() {
            return Err(RouteError::EmptyModel);
        }
        if provider.contains(KEY_SEPARATOR) {
            return Err(RouteError::Separator { field: "provider" });
        }
        if model.contains(KEY_SEPARATOR) {
            return Err(RouteError::Separator { field: "model" });
        }
        Ok(Self { provider, model })
    }

    /// The registered provider id.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The provider-owned model id.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Stable identity for this pair.
    ///
    /// The upstream `modelRouteKey` shape: provider, separator, model. Equality of two routes is
    /// field equality, which this key agrees with by construction.
    #[must_use]
    pub fn key(&self) -> String {
        let mut key = String::with_capacity(self.provider.len() + self.model.len() + 1);
        key.push_str(&self.provider);
        key.push(KEY_SEPARATOR);
        key.push_str(&self.model);
        key
    }

    /// `provider/model`, for a human or model-facing sentence.
    #[must_use]
    pub fn display(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// Why a route list, a route value, or a selection was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouteError {
    /// The value was not an array of routes.
    #[error("subagent model selection requires an array of routes")]
    NotAnArray,
    /// The entry at `index` was not an object with two non-empty string fields.
    #[error("subagent model selection requires non-empty provider and model ids (entry {index})")]
    Malformed {
        /// Which entry, counted from zero.
        index: usize,
    },
    /// A provider id was empty.
    #[error("subagent model selection requires a non-empty provider id")]
    EmptyProvider,
    /// A model id was empty.
    #[error("subagent model selection requires a non-empty model id")]
    EmptyModel,
    /// A field contained the identity separator.
    #[error("subagent model selection cannot carry a NUL byte in the {field}")]
    Separator {
        /// `provider` or `model`.
        field: &'static str,
    },
    /// A reasoning effort id was empty.
    #[error("child LLM `reasoning_effort` must be non-empty")]
    EmptyEffort,
    /// A field had the wrong JSON type.
    #[error("child LLM `{field}` must be a string")]
    WrongType {
        /// The offending field.
        field: &'static str,
    },
    /// The same route appeared twice.
    #[error("subagent model selection repeats route \"{route}\"")]
    Repeated {
        /// The repeated route, rendered `provider/model`.
        route: String,
    },
    /// A policy is being recorded with no route at all.
    #[error("subagent/model-selection-policy requires at least one route")]
    EmptyPolicy,
    /// A deployment enabled selection without naming any route to select.
    #[error("enabled subagent model selection requires at least one allowed model")]
    EnabledWithoutRoutes,
    /// The route is not authorized for this Session.
    #[error("child LLM route \"{route}\" is not allowed for this Session")]
    NotAllowed {
        /// The refused route, rendered `provider/model`.
        route: String,
        /// Whether the refusal came from the default (parent-route-only) policy.
        parent_only: bool,
    },
}

impl RouteError {
    /// The sentence to show a model, including the guidance a bare refusal omits.
    ///
    /// The parent-only suffix is what turns "not allowed" into something correctable: a model that
    /// does not know the Session never opted in will keep retrying the same route.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NotAllowed {
                route,
                parent_only: true,
            } => format!(
                "child LLM route \"{route}\" is not allowed for this Session; without a recorded \
                 model-selection policy, explicit selection is limited to the parent route"
            ),
            other => other.to_string(),
        }
    }
}

/// Validates a parsed route list: no repeated pair.
///
/// The upstream assertion at the durable/configuration boundary. Empty ids are already refused by
/// [`AllowedModelRoute::new`], so this is exactly the duplicate check.
///
/// # Errors
///
/// [`RouteError::Repeated`] naming the first duplicate, in input order.
pub fn assert_allowed_routes(routes: &[AllowedModelRoute]) -> Result<(), RouteError> {
    let mut seen = BTreeSet::new();
    for route in routes {
        if !seen.insert(route.key()) {
            return Err(RouteError::Repeated {
                route: route.display(),
            });
        }
    }
    Ok(())
}

/// Reads a route array from untrusted JSON, rejecting every shape the upstream assertion rejects.
///
/// This is the tool-argument and setting boundary: model output and a user document both arrive as
/// JSON, so both are checked here rather than trusted. Extra keys on an entry are ignored, matching
/// the upstream assertion (the stricter projection schema is applied by
/// [`ModelSelectionPolicy::recorded`]'s caller, which has nothing else to check).
///
/// # Errors
///
/// [`RouteError::NotAnArray`], [`RouteError::Malformed`] for an entry that is not an object with two
/// non-empty string fields, or [`RouteError::Repeated`].
pub fn parse_routes(value: &serde_json::Value) -> Result<Vec<AllowedModelRoute>, RouteError> {
    let entries = value.as_array().ok_or(RouteError::NotAnArray)?;
    let mut routes = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let provider = entry.get("provider").and_then(serde_json::Value::as_str);
        let model = entry.get("model").and_then(serde_json::Value::as_str);
        let (Some(provider), Some(model)) = (provider, model) else {
            return Err(RouteError::Malformed { index });
        };
        if provider.is_empty() || model.is_empty() {
            return Err(RouteError::Malformed { index });
        }
        let route = AllowedModelRoute::new(provider, model)
            // An embedded NUL is a malformed entry at this boundary, not a separate class.
            .map_err(|_| RouteError::Malformed { index })?;
        routes.push(route);
    }
    assert_allowed_routes(&routes)?;
    Ok(routes)
}

/// The exact routes a Session authorized, or nothing.
///
/// `None` (an omitted policy) is not "unrestricted": see [`assert_allowed_selection`]. It means the
/// Session recorded no opt-in, so explicit selection is limited to the route the parent is already
/// running.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelSelectionPolicy {
    routes: Vec<AllowedModelRoute>,
}

impl ModelSelectionPolicy {
    /// A policy naming `routes`.
    ///
    /// An empty policy is representable because a deployment can be configured with selection off;
    /// a *recorded* policy additionally requires at least one route ([`Self::recorded`]), because a
    /// Session that opted in with nothing to select would be an opt-in that cannot be honored.
    ///
    /// # Errors
    ///
    /// [`RouteError::Repeated`] for a duplicate pair.
    pub fn new(routes: Vec<AllowedModelRoute>) -> Result<Self, RouteError> {
        assert_allowed_routes(&routes)?;
        Ok(Self { routes })
    }

    /// A policy being recorded into a Session: at least one route.
    ///
    /// # Errors
    ///
    /// [`RouteError::EmptyPolicy`] when no route is named — a Session that opted in with nothing to
    /// select would be an opt-in that cannot be honored — or [`RouteError::Repeated`].
    pub fn recorded(routes: Vec<AllowedModelRoute>) -> Result<Self, RouteError> {
        if routes.is_empty() {
            return Err(RouteError::EmptyPolicy);
        }
        Self::new(routes)
    }

    /// The authorized routes, in recorded order.
    #[must_use]
    pub fn routes(&self) -> &[AllowedModelRoute] {
        &self.routes
    }

    /// Whether `route` is authorized.
    #[must_use]
    pub fn allows(&self, route: &AllowedModelRoute) -> bool {
        self.routes.iter().any(|candidate| candidate == route)
    }
}

/// One durable policy event, as the Session log stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelSelectionEvent {
    /// `subagent/model-selection-policy` with its allowlist.
    RecordPolicy {
        /// The routes the Session may select explicitly.
        allowed_models: Vec<AllowedModelRoute>,
    },
}

/// Folds one policy event into the projection state.
///
/// The recorded policy is **first-wins**: a second `RecordPolicy` is ignored rather than replacing
/// the first. Upstream's projection returns the state unchanged once it is non-null, and the reason
/// is authorization: a later event could widen an allowlist after the Session's first model request,
/// which would let an agent grant itself routes mid-session.
///
/// # Errors
///
/// [`RouteError::Malformed`] for an empty list, [`RouteError::Repeated`] for a duplicate.
pub fn apply_policy_event(
    state: Option<&ModelSelectionPolicy>,
    event: &ModelSelectionEvent,
) -> Result<Option<ModelSelectionPolicy>, RouteError> {
    if state.is_some() {
        return Ok(state.cloned());
    }
    match event {
        ModelSelectionEvent::RecordPolicy { allowed_models } => Ok(Some(
            ModelSelectionPolicy::recorded(allowed_models.clone())?,
        )),
    }
}

/// The policy a Session inherits from its parent, when it records none of its own.
///
/// A child inherits its parent's allowlist rather than the deployment default: the parent already
/// passed whatever check produced its own policy, and re-read the setting for every child would let
/// a mid-session setting change reach children of a Session that never opted in.
#[must_use]
pub fn policy_for_child(parent: Option<&ModelSelectionPolicy>) -> Option<ModelSelectionPolicy> {
    parent.cloned()
}

/// The policy a fresh top-level Session records, if the deployment default applies.
///
/// `seed_ended` is whether the Session's first event is `session/end-seed`. Both the enable flag and
/// that condition gate the default: a seeded Session deliberately starts from an existing transcript
/// rather than a clean opt-in, and a resumed Session (`first_live_seq != 0`) keeps whatever its log
/// already recorded — which is why the resume check belongs to the caller, who is also the only party
/// that can read the log.
#[must_use]
pub fn policy_for_new_top_level(
    settings: &ModelSelectionSettings,
    seed_ended: bool,
) -> Option<ModelSelectionPolicy> {
    if !settings.enabled() || seed_ended {
        return None;
    }
    Some(ModelSelectionPolicy {
        routes: settings.allowed_models().to_vec(),
    })
}

/// The stored user preference for model-selectable delegation.
///
/// `enabled` with no routes is refused at construction rather than at the first delegation: an
/// enabled setting with an empty allowlist would record nothing, and a user who opted in deserves to
/// learn at configuration time that the opt-in cannot be honored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSelectionSettings {
    enabled: bool,
    allowed_models: Vec<AllowedModelRoute>,
}

impl ModelSelectionSettings {
    /// Builds a setting.
    ///
    /// # Errors
    ///
    /// [`RouteError::Repeated`] for a duplicate route, [`RouteError::EnabledWithoutRoutes`] when
    /// `enabled` is true and no route is named.
    pub fn new(enabled: bool, allowed_models: Vec<AllowedModelRoute>) -> Result<Self, RouteError> {
        if enabled && allowed_models.is_empty() {
            return Err(RouteError::EnabledWithoutRoutes);
        }
        assert_allowed_routes(&allowed_models)?;
        Ok(Self {
            enabled,
            allowed_models,
        })
    }

    /// Whether new top-level Sessions opt in.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The routes new top-level Sessions may select.
    #[must_use]
    pub fn allowed_models(&self) -> &[AllowedModelRoute] {
        &self.allowed_models
    }
}

/// What a tool call explicitly asked for, read from model-facing JSON.
///
/// All three fields are optional and their *presence* matters as much as their value: an explicit
/// empty string is a malformed request, while an omitted effort on a changed route means "the
/// selected model's default", not "the parent's effort".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DelegationModelRequest {
    /// Requested provider id.
    pub provider: Option<String>,
    /// Requested model id.
    pub model: Option<String>,
    /// Requested reasoning effort id.
    pub reasoning_effort: Option<String>,
}

impl DelegationModelRequest {
    /// An empty request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the provider.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Sets the model.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets the reasoning effort.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Reads the three fields from tool-call JSON.
    ///
    /// # Errors
    ///
    /// [`RouteError::WrongType`] when a present field is not a string; [`RouteError::EmptyProvider`],
    /// [`RouteError::EmptyModel`] or [`RouteError::EmptyEffort`] when it is an empty string, which is
    /// refused because a model that sends `""` meant "unset" and the two must not be conflated.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, RouteError> {
        let mut request = Self::new();
        for (field, slot) in [
            ("provider", &mut request.provider),
            ("model", &mut request.model),
            ("reasoning_effort", &mut request.reasoning_effort),
        ] {
            match value.get(field) {
                None | Some(serde_json::Value::Null) => {}
                Some(serde_json::Value::String(text)) => {
                    if text.is_empty() {
                        return Err(match field {
                            "provider" => RouteError::EmptyProvider,
                            "model" => RouteError::EmptyModel,
                            _ => RouteError::EmptyEffort,
                        });
                    }
                    *slot = Some(text.clone());
                }
                Some(_) => return Err(RouteError::WrongType { field }),
            }
        }
        Ok(request)
    }

    /// Whether the call names any child LLM value at all.
    ///
    /// This is upstream's `hasDelegationModelRequest`, and it is the switch between "the child
    /// inherits the configured defaults" and "the child runs somewhere the caller chose". A request
    /// with nothing set must not be authorized, because there is nothing to authorize.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.provider.is_some() || self.model.is_some() || self.reasoning_effort.is_some()
    }
}

/// Whether configured child defaults name any LLM value.
///
/// Configured defaults are a deployment decision, not model output, but they still have to be
/// resolved through the adapter before a child exists, so this decides whether preflight runs even
/// when the model asked for nothing.
#[must_use]
pub fn has_configured_llm_selection(options: Option<&AgentOptions>) -> bool {
    options.is_some_and(|options| {
        options.provider.is_some() || options.model.is_some() || options.reasoning_effort.is_some()
    })
}

/// Whether this delegation needs its route resolved before a child is created.
#[must_use]
pub fn requires_route_preflight(
    request: &DelegationModelRequest,
    configured: Option<&AgentOptions>,
) -> bool {
    request.has_selection() || has_configured_llm_selection(configured)
}

/// Enforces Session authorization for one complete, explicitly selected route.
///
/// # Errors
///
/// [`RouteError::NotAllowed`] when the route is not the parent's (no policy recorded) or not in the
/// recorded allowlist. The error names the route so the model can correct it.
pub fn assert_allowed_selection(
    policy: Option<&ModelSelectionPolicy>,
    parent_route: Option<&AllowedModelRoute>,
    route: &AllowedModelRoute,
) -> Result<(), RouteError> {
    let allowed = match policy {
        Some(policy) => policy.allows(route),
        None => parent_route.is_some_and(|parent| parent == route),
    };
    if allowed {
        return Ok(());
    }
    Err(RouteError::NotAllowed {
        route: route.display(),
        parent_only: policy.is_none(),
    })
}

/// Why an explicit selection could not be turned into child options.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectionError {
    /// The tool instance does not permit model-facing selection.
    #[error("child model selection is disabled for this tool instance")]
    Disabled,
    /// Only one of provider and model was supplied.
    #[error("child LLM `provider` and `model` must be supplied together")]
    NeedsBothHalves,
    /// An effort was supplied with no route.
    #[error(
        "child LLM `reasoning_effort` requires both `provider` and `model`; supply both or omit all three fields"
    )]
    EffortNeedsRoute,
    /// A supplied field was empty.
    #[error("child LLM `{field}` must be non-empty")]
    EmptyField {
        /// The offending field.
        field: &'static str,
    },
    /// The route was not authorized.
    #[error(transparent)]
    Route(#[from] RouteError),
}

/// Turns an explicit request into the child options it names, authorized against the policy.
///
/// The layering is upstream's: an absent request returns the configured defaults unchanged (so an
/// omitted value keeps meaning "child-loop default"); a present request replaces provider and model
/// and takes the effort *explicitly*, including explicitly clearing it.
///
/// # Errors
///
/// [`SelectionError::Disabled`] when the tool instance forbids selection,
/// [`SelectionError::NeedsBothHalves`] / [`SelectionError::EffortNeedsRoute`] for a partial request,
/// [`SelectionError::EmptyField`] for an empty one, and [`SelectionError::Route`] when authorization
/// fails.
pub fn requested_child_options(
    configured: Option<&AgentOptions>,
    request: &DelegationModelRequest,
    enabled: bool,
    policy: Option<&ModelSelectionPolicy>,
    parent_route: Option<&AllowedModelRoute>,
) -> Result<Option<AgentOptions>, SelectionError> {
    if !request.has_selection() {
        return Ok(configured.cloned());
    }
    if !enabled {
        return Err(SelectionError::Disabled);
    }
    for (field, value) in [
        ("provider", request.provider.as_deref()),
        ("model", request.model.as_deref()),
        ("reasoning_effort", request.reasoning_effort.as_deref()),
    ] {
        if value.is_some_and(str::is_empty) {
            return Err(SelectionError::EmptyField { field });
        }
    }
    let (Some(provider), Some(model)) = (request.provider.as_deref(), request.model.as_deref())
    else {
        return Err(if request.reasoning_effort.is_none() {
            SelectionError::NeedsBothHalves
        } else {
            SelectionError::EffortNeedsRoute
        });
    };
    let route = AllowedModelRoute::new(provider, model)?;
    assert_allowed_selection(policy, parent_route, &route)?;

    let mut options = configured.cloned().unwrap_or_default();
    options.provider = Some(provider.to_owned());
    options.model = Some(model.to_owned());
    // An explicit `None` here clears a compatible parent effort during child-option merging; it is
    // deliberately not "keep the configured effort".
    options
        .reasoning_effort
        .clone_from(&request.reasoning_effort);
    Ok(Some(options))
}

/// Merges a provider's own route defaults under the tool instance's configured options.
///
/// Field-wise, with the instance winning: upstream spreads `providerRouteDefaults` then
/// `config.agentOptions`, which is a per-field overwrite because both are partial objects. The merge
/// only happens when a route must be resolved; otherwise the provider's defaults are left to the
/// provider.
#[must_use]
pub fn merge_child_defaults(
    provider_defaults: Option<&AgentOptions>,
    configured: Option<&AgentOptions>,
    requires_preflight: bool,
) -> Option<AgentOptions> {
    if !requires_preflight {
        return configured.cloned();
    }
    match (provider_defaults, configured) {
        (None, configured) => configured.cloned(),
        (Some(defaults), None) => Some(defaults.clone()),
        (Some(defaults), Some(configured)) => Some(AgentOptions {
            provider: configured
                .provider
                .clone()
                .or_else(|| defaults.provider.clone()),
            model: configured.model.clone().or_else(|| defaults.model.clone()),
            reasoning_effort: configured
                .reasoning_effort
                .clone()
                .or_else(|| defaults.reasoning_effort.clone()),
            max_tokens: configured.max_tokens.or(defaults.max_tokens),
        }),
    }
}

/// The route a child would actually run on, as the adapter is asked to resolve it.
///
/// Named fields because the adapter call takes exactly these three and nothing else; a struct that
/// could carry a fourth field would invite passing a value the adapter does not accept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRoute {
    /// Effective provider.
    pub provider: String,
    /// Effective model.
    pub model: String,
    /// Effective effort, when one applies.
    pub reasoning_effort: Option<String>,
}

/// Resolves the effective child route before the child is created.
///
/// The effort rule is the subtle one. An omitted effort inherits the parent's **only when the route
/// did not change** and the caller permits inheritance; a changed route falls back to the selected
/// model's own default. Inheriting an effort across a route change would ask a model for an effort
/// it may not advertise, and the failure would surface as a provider error mid-run rather than as a
/// refusal here.
///
/// # Errors
///
/// [`SelectionError::Route`] wrapping [`RouteError::EmptyProvider`] / [`RouteError::EmptyModel`]
/// when neither the request nor the parent supplies both halves — there is no route to resolve.
pub fn preflight_route(
    parent: &AgentOptions,
    requested: Option<&AgentOptions>,
    inherit_parent_effort: bool,
) -> Result<CallRoute, SelectionError> {
    let provider = requested
        .and_then(|options| options.provider.as_deref())
        .or(parent.provider.as_deref());
    let model = requested
        .and_then(|options| options.model.as_deref())
        .or(parent.model.as_deref());
    let (Some(provider), Some(model)) = (provider, model) else {
        return Err(SelectionError::Route(RouteError::EmptyProvider));
    };
    let route_changed =
        Some(provider) != parent.provider.as_deref() || Some(model) != parent.model.as_deref();
    let reasoning_effort = requested
        .and_then(|options| options.reasoning_effort.clone())
        .or_else(|| {
            if inherit_parent_effort && !route_changed {
                parent.reasoning_effort.clone()
            } else {
                None
            }
        });
    Ok(CallRoute {
        provider: provider.to_owned(),
        model: model.to_owned(),
        reasoning_effort,
    })
}

/// Deployment-owned preset choices for one delegation tool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresetSelectionConfig {
    /// Default independent preset; omission inherits the parent's.
    pub agent_preset: Option<String>,
    /// Non-empty allowlist exposing `agent_preset` to the model; omission disables selection.
    pub allowed_agent_presets: Option<Vec<String>>,
}

impl PresetSelectionConfig {
    /// No preset configured.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the default preset.
    #[must_use]
    pub fn with_default(mut self, preset: impl Into<String>) -> Self {
        self.agent_preset = Some(preset.into());
        self
    }

    /// Sets the allowlist.
    #[must_use]
    pub fn with_allowed(mut self, presets: Vec<String>) -> Self {
        self.allowed_agent_presets = Some(presets);
        self
    }
}

/// Untrusted preset selection from tool JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresetSelectionRequest {
    /// The requested preset id, as it arrived.
    pub agent_preset: Option<serde_json::Value>,
}

/// Why a preset configuration or selection was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PresetError {
    /// The allowlist names the same preset twice.
    #[error("tool-subagent: allowedAgentPresets contains duplicate preset ids")]
    DuplicateId {
        /// The duplicated id.
        id: String,
    },
    /// The default is not in the allowlist.
    #[error("tool-subagent: agentPreset must be included in allowedAgentPresets")]
    DefaultNotAllowed {
        /// The default that is missing from the allowlist.
        id: String,
    },
    /// The provider cannot run an independent preset.
    #[error(
        "tool-subagent: provider \"{name}\" does not support independent agent preset selection"
    )]
    ProviderUnsupported {
        /// The provider name.
        name: String,
    },
    /// The model asked for a preset while the tool exposes none.
    #[error("agent_preset selection is disabled for this tool instance")]
    Disabled,
    /// The requested id is not in the allowlist.
    #[error("agent_preset must be one of: {choices}")]
    NotInChoices {
        /// The comma-separated allowlist.
        choices: String,
    },
}

/// Validates configured preset choices and resolves model input against the allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetSelection {
    config: PresetSelectionConfig,
}

impl PresetSelection {
    /// Builds a selection, validating the configuration eagerly.
    ///
    /// # Errors
    ///
    /// [`PresetError::DuplicateId`] for a repeated allowlist entry, [`PresetError::DefaultNotAllowed`]
    /// when the default is not in the allowlist. Validate at mount time rather than at resolve time:
    /// a misconfigured deployment should fail to load, not fail every delegation.
    pub fn new(config: PresetSelectionConfig) -> Result<Self, PresetError> {
        if let Some(allowed) = &config.allowed_agent_presets {
            let mut seen = BTreeSet::new();
            for id in allowed {
                if !seen.insert(id.as_str()) {
                    return Err(PresetError::DuplicateId { id: id.clone() });
                }
            }
            if let Some(default) = &config.agent_preset {
                if !allowed.contains(default) {
                    return Err(PresetError::DefaultNotAllowed {
                        id: default.clone(),
                    });
                }
            }
        }
        Ok(Self { config })
    }

    /// Whether this tool requires the preset service and an independently composing provider.
    #[must_use]
    pub fn configured(&self) -> bool {
        self.config.agent_preset.is_some() || self.config.allowed_agent_presets.is_some()
    }

    /// Preset ids advertised in the model-facing parameter, when selection is exposed.
    #[must_use]
    pub fn choices(&self) -> Option<&[String]> {
        self.config.allowed_agent_presets.as_deref()
    }

    /// Model-visible selection and default guidance.
    #[must_use]
    pub fn description(&self) -> String {
        let fallback = match &self.config.agent_preset {
            Some(preset) => format!("Omit to use {preset}."),
            None => "Omit to inherit the parent agent's current preset.".to_owned(),
        };
        format!("Agent preset for this child. {fallback}")
    }

    /// Rejects preset selection for a provider that cannot honor it.
    ///
    /// A provider that inherits the parent's conversation cannot run an independent preset at all —
    /// its child *is* the parent's context — and one that declares no `agent_preset` capability
    /// would silently ignore the selection. Both are refused at mount time.
    ///
    /// # Errors
    ///
    /// [`PresetError::ProviderUnsupported`].
    pub fn assert_provider(
        &self,
        name: &str,
        capabilities: SubagentCapabilities,
        inherits_parent_context: bool,
    ) -> Result<(), PresetError> {
        if self.configured() && (!capabilities.agent_preset || inherits_parent_context) {
            return Err(PresetError::ProviderUnsupported {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    /// Validates model input and applies the deployment default.
    ///
    /// # Errors
    ///
    /// [`PresetError::Disabled`] when a preset was requested without an allowlist, or
    /// [`PresetError::NotInChoices`] for a value outside it. A non-string is also
    /// [`PresetError::NotInChoices`], because the only correct values are strings in the list.
    pub fn resolve(&self, request: &PresetSelectionRequest) -> Result<Option<String>, PresetError> {
        let Some(value) = &request.agent_preset else {
            return Ok(self.config.agent_preset.clone());
        };
        let Some(choices) = self.config.allowed_agent_presets.as_ref() else {
            return Err(PresetError::Disabled);
        };
        let Some(id) = value.as_str() else {
            return Err(PresetError::NotInChoices {
                choices: choices.join(", "),
            });
        };
        if !choices.iter().any(|choice| choice == id) {
            return Err(PresetError::NotInChoices {
                choices: choices.join(", "),
            });
        }
        Ok(Some(id.to_owned()))
    }
}

/// The model-facing description of one delegation tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationWording {
    /// The tool description.
    pub description: String,
    /// The `prompt` parameter description.
    pub prompt_description: String,
}

/// Wording that tells the truth about what the child can see.
///
/// A fork inherits the conversation, so telling the model the child "does not see this
/// conversation" makes it restate context the child already has — wasted tokens and a diluted task.
/// A fresh child sees nothing, so telling the model to "build on the conversation" makes it refer to
/// turns the child cannot read. The provider's descriptor is the only source for which is true.
#[must_use]
pub fn provider_wording(inherits_conversation: bool) -> DelegationWording {
    if inherits_conversation {
        DelegationWording {
            description: "Delegate a task to a subagent that inherits this conversation: a child \
                          agent seeded with all completed turns so far (it does not see the current \
                          in-flight turn). Use this when the subtask builds on this conversation's \
                          context — a follow-up analysis, a review, a continuation — without \
                          consuming this conversation's context for the work itself. You receive its \
                          result, not its intermediate steps."
                .to_owned(),
            prompt_description: "The task for the subagent. It already sees this conversation's \
                                 completed turns, so build on them freely and state only what is new."
                .to_owned(),
        }
    } else {
        DelegationWording {
            description:
                "Delegate a self-contained task to a subagent (a separate agent that works \
                          in its own context) to offload focused, independent work — research, a \
                          scoped implementation, an analysis — so it does not consume this \
                          conversation's context. The subagent returns its result, not its \
                          intermediate steps."
                    .to_owned(),
            prompt_description: "The complete, self-contained task for the subagent. It does not \
                                 share this conversation's context, so include everything it needs."
                .to_owned(),
        }
    }
}

/// The conditional guidance appended to the description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four independent facts about the backend, not a state machine"
)]
pub struct DescriptionOptions {
    /// Whether the selected backend accepts follow-up messages.
    pub continuable: bool,
    /// Whether the tool instance exposes model selection.
    pub model_selection: bool,
    /// Whether the backend supplies its own child route defaults.
    pub route_defaults: bool,
    /// Whether the backend seeds the child with the parent's turns.
    pub inherits_conversation: bool,
}

/// Renders the full tool description, including the conditional fragments.
///
/// Each fragment is gated on a fact about the selected backend. The selection sentence differs on
/// `route_defaults` because the fallback differs: a backend with its own defaults falls back to
/// those, not to the parent's values, and a model told the wrong one will omit fields expecting the
/// wrong result.
#[must_use]
pub fn delegation_description(options: DescriptionOptions) -> String {
    let mut text = provider_wording(options.inherits_conversation).description;
    text.push_str(
        " This tool starts an independently managed subagent and immediately returns its name. If \
         you are still online when it finishes, the runtime automatically delivers its completion \
         notice. The notice includes the final answer unless the child sent you a message just \
         before finishing.",
    );
    text.push_str(if options.continuable {
        " Use `send_message` to steer it while running or continue its conversation after it \
         finishes."
    } else {
        " This backend does not accept follow-up messages."
    });
    if options.model_selection {
        text.push_str(if options.route_defaults {
            " Omit `provider`, `model`, and `reasoning_effort` to use child defaults, falling back \
             to this backend's route defaults."
        } else {
            " Omit `provider`, `model`, and `reasoning_effort` to use child defaults, falling back \
             to compatible parent values."
        });
        if options.inherits_conversation {
            text.push_str(
                " Changing the route can prevent provider-side reuse of the inherited conversation \
                 prefix.",
            );
        }
    }
    text
}

/// Renders the delegation guidance for the system prompt, or nothing when this tool is not visible.
///
/// `visible` must be the tools whose definition is the one currently registered for the scope, which
/// is why the caller passes names rather than reading a global list: a scoped override leaves two
/// definitions alive for one name, and only one of them is the tool the model can call. The section
/// renders only when `tool_name` sorts first among the visible names, so two instances of the same
/// tool never emit the guidance twice.
#[must_use]
pub fn delegation_prompt_section(tool_name: &str, visible: &[String]) -> String {
    let mut names: Vec<&str> = visible.iter().map(String::as_str).collect();
    names.sort_unstable();
    if names.first() != Some(&tool_name) {
        return String::new();
    }
    let rendered = names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(" or ");
    format!(
        "Start independent delegations with {rendered} together in one assistant message and \
         continue useful work while they run. When only waiting for their results remains, end your \
         turn instead of sleeping or polling; the next result starts a new turn. Collect the \
         subagent results required to complete the task before giving the user a final answer.\n\n\
         Bash, formatters, code generators, and scripts can overwrite concurrent edits without \
         conflict detection; coordinate their changes explicitly. The agent integrating the work \
         must review the final diff and run relevant tests."
    )
}

/// Renders the depth-context sentence, or nothing while the agent may still delegate.
///
/// At the ceiling the sentence names the current depth, the maximum, and the remaining levels (zero)
/// — three facts the model needs to stop rather than retry — and repeats the tool name so the
/// instruction survives a context that also mentions sibling delegation tools.
#[must_use]
pub fn delegation_depth_context(tool_name: &str, depth: u32, max: u32) -> Option<String> {
    if depth < max {
        return None;
    }
    Some(format!(
        "Delegation depth for `{tool_name}`: current {depth}, maximum {max}, remaining levels 0. Do \
         not call `{tool_name}` at this depth; carry out the work directly."
    ))
}

/// One reasoning effort a model advertises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasoningEffortInfo {
    /// The effort id the selection names.
    pub id: String,
    /// The display name.
    pub name: String,
    /// Optional explanation.
    pub description: Option<String>,
    /// Whether this is the model's default effort.
    pub default: bool,
}

/// One model a provider advertises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// The model id.
    pub id: String,
    /// The display name.
    pub name: String,
    /// Optional explanation.
    pub description: Option<String>,
    /// The efforts this model advertises, in presentation order.
    pub efforts: Vec<ReasoningEffortInfo>,
}

/// One registered LLM provider and the models this module knows it advertises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderInfo {
    /// The provider id routes name.
    pub id: String,
    /// The display name.
    pub name: String,
    /// Models, in presentation order.
    pub models: Vec<ModelInfo>,
}

/// What a discovery call asks for: providers, a provider's models, or one model's efforts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryRequest {
    /// Authorized providers.
    Providers,
    /// One provider's authorized models.
    Models {
        /// The provider id.
        provider: String,
    },
    /// One authorized model and its reasoning efforts.
    Efforts {
        /// The provider id.
        provider: String,
        /// The exact model id.
        model: String,
    },
}

/// Reads the two optional discovery parameters into a request.
///
/// # Errors
///
/// [`DiscoveryError::ModelRequiresProvider`] when `model` arrives without `provider`,
/// [`DiscoveryError::EmptyProvider`] / [`DiscoveryError::EmptyModel`] for an explicit empty string,
/// and [`DiscoveryError::Malformed`] for a non-string value.
pub fn parse_discovery_request(
    provider: Option<&serde_json::Value>,
    model: Option<&serde_json::Value>,
) -> Result<DiscoveryRequest, DiscoveryError> {
    let provider = string_field(provider, "provider")?;
    let model = string_field(model, "model")?;
    match (provider, model) {
        (None, None) => Ok(DiscoveryRequest::Providers),
        (Some(provider), None) => Ok(DiscoveryRequest::Models { provider }),
        (None, Some(_)) => Err(DiscoveryError::ModelRequiresProvider),
        (Some(provider), Some(model)) => Ok(DiscoveryRequest::Efforts { provider, model }),
    }
}

/// Reads one optional string field, refusing an explicit empty string.
fn string_field(
    value: Option<&serde_json::Value>,
    field: &'static str,
) -> Result<Option<String>, DiscoveryError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) if text.is_empty() => Err(match field {
            "provider" => DiscoveryError::EmptyProvider,
            _ => DiscoveryError::EmptyModel,
        }),
        Some(serde_json::Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(DiscoveryError::Malformed),
    }
}

/// Why a discovery call could not answer.
///
/// These strings are the tool's corrective surface: a model that gets
/// [`DiscoveryError::ProviderNotRegistered`] back is told the available providers; the alternative —
/// an empty answer — leaves it guessing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiscoveryError {
    /// `model` arrived with no `provider`.
    #[error("`model` requires `provider`")]
    ModelRequiresProvider,
    /// An explicit empty provider id.
    #[error("`provider` must be non-empty")]
    EmptyProvider,
    /// An explicit empty model id.
    #[error("`model` must be non-empty")]
    EmptyModel,
    /// A parameter was not a string.
    #[error("`provider` and `model` must be strings")]
    Malformed,
    /// The provider id is not registered.
    #[error("LLM provider \"{id}\" is not registered; available providers: {available}")]
    ProviderNotRegistered {
        /// The requested id.
        id: String,
        /// The authorized providers, comma separated, or `(none)`.
        available: String,
    },
    /// The provider is registered but no recorded route names it.
    #[error("LLM provider \"{id}\" is not allowed for this Session")]
    ProviderNotAllowed {
        /// The requested id.
        id: String,
    },
    /// The exact route is not authorized.
    #[error("child LLM route \"{route}\" is not allowed for this Session")]
    RouteNotAllowed {
        /// The refused route.
        route: String,
    },
    /// There is no allowlist at all.
    #[error("cannot discover child LLM routes without a parent provider and model")]
    NoEffectiveRoute,
}

impl From<RouteError> for DiscoveryError {
    fn from(error: RouteError) -> Self {
        match error {
            RouteError::NotAllowed { route, .. } => Self::RouteNotAllowed { route },
            _ => Self::NoEffectiveRoute,
        }
    }
}

/// The provider catalog discovery answers from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    providers: Vec<ProviderInfo>,
}

impl ModelCatalog {
    /// Builds a catalog.
    #[must_use]
    pub fn new(providers: Vec<ProviderInfo>) -> Self {
        Self { providers }
    }

    /// The authorized policy when a Session recorded none: the parent's route, and nothing else.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::NoEffectiveRoute`] when the parent has no complete route — discovery cannot
    /// describe what the child could run on if the parent itself does not name a model.
    pub fn policy_for_parent(
        parent_provider: Option<&str>,
        parent_model: Option<&str>,
    ) -> Result<ModelSelectionPolicy, DiscoveryError> {
        let (Some(provider), Some(model)) = (parent_provider, parent_model) else {
            return Err(DiscoveryError::NoEffectiveRoute);
        };
        ModelSelectionPolicy::new(vec![AllowedModelRoute::new(provider, model)?])
            .map_err(DiscoveryError::from)
    }

    /// Answers one discovery request against a policy.
    ///
    /// Three modes over one allowlist: an unauthorized provider is an error even when it is
    /// registered, because advertising it would offer a route the following delegation refuses.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::ProviderNotRegistered`], [`DiscoveryError::ProviderNotAllowed`],
    /// [`DiscoveryError::RouteNotAllowed`].
    pub fn discover(
        &self,
        policy: &ModelSelectionPolicy,
        selection_enabled: bool,
        request: &DiscoveryRequest,
    ) -> Result<String, DiscoveryError> {
        match request {
            DiscoveryRequest::Providers => Ok(self.providers_answer(policy)),
            DiscoveryRequest::Models { provider } => {
                self.models_answer(policy, selection_enabled, provider)
            }
            DiscoveryRequest::Efforts { provider, model } => {
                self.efforts_answer(policy, provider, model)
            }
        }
    }

    /// The authorized providers, one `id — name` line each.
    ///
    /// Filtered by the policy rather than by the catalog: advertising a provider the following
    /// delegation would refuse sends the model from one error into another.
    fn providers_answer(&self, policy: &ModelSelectionPolicy) -> String {
        let allowed: Vec<&ProviderInfo> = self
            .providers
            .iter()
            .filter(|provider| {
                policy
                    .routes()
                    .iter()
                    .any(|route| route.provider() == provider.id)
            })
            .collect();
        if allowed.is_empty() {
            return "(no LLM providers)".to_owned();
        }
        allowed
            .iter()
            .map(|provider| format!("{} — {}", provider.id, provider.name))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One provider's models: the authorized subset with an opt-in, or exactly the recorded route
    /// without one.
    fn models_answer(
        &self,
        policy: &ModelSelectionPolicy,
        selection_enabled: bool,
        provider: &str,
    ) -> Result<String, DiscoveryError> {
        let allowed_routes: Vec<&AllowedModelRoute> = policy
            .routes()
            .iter()
            .filter(|route| route.provider() == provider)
            .collect();
        let Some(first) = allowed_routes.first() else {
            return Err(DiscoveryError::ProviderNotAllowed {
                id: provider.to_owned(),
            });
        };
        let info = self.registered(policy, provider)?;
        let models: Vec<&ModelInfo> = if selection_enabled {
            info.models
                .iter()
                .filter(|model| allowed_routes.iter().any(|route| route.model() == model.id))
                .collect()
        } else {
            // Without an opt-in the only route that exists is the parent's, and the advertised list
            // may not contain it; resolve that one route instead of pretending nothing is available.
            info.models
                .iter()
                .find(|model| model.id == first.model())
                .into_iter()
                .collect()
        };
        if models.is_empty() {
            return Ok(format!("(no advertised models for {})", info.id));
        }
        Ok(models
            .iter()
            .map(|model| model_line(&info.id, model))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// One authorized model and its advertised efforts, with the default marked.
    fn efforts_answer(
        &self,
        policy: &ModelSelectionPolicy,
        provider: &str,
        model: &str,
    ) -> Result<String, DiscoveryError> {
        let refused = || DiscoveryError::RouteNotAllowed {
            route: format!("{provider}/{model}"),
        };
        if !policy
            .routes()
            .iter()
            .any(|route| route.provider() == provider && route.model() == model)
        {
            return Err(refused());
        }
        let info = self.registered(policy, provider)?;
        let model_info = info
            .models
            .iter()
            .find(|candidate| candidate.id == model)
            .ok_or_else(refused)?;
        let efforts = model_info
            .efforts
            .iter()
            .map(|effort| {
                let suffix = effort
                    .description
                    .as_deref()
                    .map_or_else(String::new, |description| format!(": {description}"));
                format!(
                    "{}{} — {}{}",
                    effort.id,
                    if effort.default { " (default)" } else { "" },
                    effort.name,
                    suffix
                )
            })
            .collect::<Vec<_>>();
        let efforts = if efforts.is_empty() {
            "(no advertised reasoning efforts)".to_owned()
        } else {
            efforts.join("\n")
        };
        Ok(format!(
            "{}\nReasoning efforts:\n{efforts}",
            model_line(&info.id, model_info)
        ))
    }

    /// Looks up a registered provider, with a model-correctable diagnostic.
    ///
    /// The available list is filtered by the policy, not by the catalog: naming a provider the
    /// Session may not use would send the model from one refusal into another.
    fn registered(
        &self,
        policy: &ModelSelectionPolicy,
        provider: &str,
    ) -> Result<&ProviderInfo, DiscoveryError> {
        if let Some(info) = self.providers.iter().find(|info| info.id == provider) {
            return Ok(info);
        }
        let available = self
            .providers
            .iter()
            .filter(|info| {
                policy
                    .routes()
                    .iter()
                    .any(|route| route.provider() == info.id)
            })
            .map(|info| info.id.clone())
            .collect::<Vec<_>>();
        Err(DiscoveryError::ProviderNotRegistered {
            id: provider.to_owned(),
            available: if available.is_empty() {
                "(none)".to_owned()
            } else {
                available.join(", ")
            },
        })
    }
}

/// Renders `provider/model — name`, with the optional description after a colon.
fn model_line(provider: &str, model: &ModelInfo) -> String {
    let suffix = model
        .description
        .as_deref()
        .map_or_else(String::new, |description| format!(": {description}"));
    format!("{provider}/{} — {}{suffix}", model.id, model.name)
}
