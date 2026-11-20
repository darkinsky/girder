//! `session.initializeDefaultModel`: pick a default model when the deployment has none.
//!
//! # What upstream does
//!
//! `packages/api/session-controller/src/index.ts` implements it as three steps:
//!
//! ```typescript
//! const provider = 'deepseek-account'
//! if (await hasProviderApiKey(this.ctx)) return          // already configured: nothing to do
//! const catalog = await buildModelCatalog(this.ctx)
//! const model = catalog.groups.find(g => g.id === provider)?.models[0]
//! if (model === undefined) throw new RemoteError('session/provider-models-unavailable', ...)
//! await this.ctx.agentDefaultModel.saveSelection({ provider, model: model.id, ... })
//! ```
//!
//! Two properties are worth naming because they are easy to get wrong:
//!
//! 1. **It is idempotent against a configured deployment.** A deployment that already has a key has
//!    a default model by definition, and this method returns *without* touching it. A version that
//!    always wrote would overwrite an operator's choice on every start.
//! 2. **A deployment with no models is an error, not a no-op.** `provider-models-unavailable` is
//!    what a client acts on; silently succeeding would leave a session that cannot run and no
//!    explanation for why.
//!
//! # What this port decides differently
//!
//! Upstream hard-codes `'deepseek-account'` because its catalogue always has that group. This build's
//! route table is whatever the deployment registered, so the provider is a parameter with that
//! default: a deployment that configured one route still gets a default, and one that configured
//! several picks the first rather than a group it may not have.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::llm::providers::RouteRegistry;

/// The provider upstream prefers when it is present.
pub const PREFERRED_PROVIDER: &str = "deepseek-account";

/// One model selection: what a client needs to know to run a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// The route that will serve the request.
    pub provider: String,
    /// The model id.
    pub model: String,
    /// The model's default reasoning effort, when it declares one.
    pub reasoning_effort: Option<String>,
}

impl Selection {
    /// Renders the selection for the wire.
    ///
    /// `reasoningEffort` is omitted rather than nulled when the model declares none: a client that
    /// read a null would have to know that null means "no default" rather than "not stated", and
    /// upstream's spread-of-undefined does the same thing.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        let mut value = json!({
            "provider": self.provider,
            "model": self.model,
        });
        if let Some(effort) = &self.reasoning_effort {
            value["reasoningEffort"] = json!(effort);
        }
        value
    }
}

/// Why a default could not be chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultModelError {
    /// No route in the catalogue serves any model.
    ///
    /// Upstream's `session/provider-models-unavailable`: the deployment is misconfigured in a way a
    /// person has to fix, so it is reported rather than swallowed.
    NoModels {
        /// The provider that was searched for first.
        preferred: String,
    },
}

impl std::fmt::Display for DefaultModelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoModels { preferred } => write!(
                formatter,
                "no provider serves a model (looked for `{preferred}` first); \
                 a session cannot be created until one does"
            ),
        }
    }
}

/// Whether a deployment already has a usable selection.
///
/// `hasProviderApiKey` upstream, reduced to the fact that matters here: this build's routes are
/// registered only when a credential resolved, so a non-empty catalogue *is* "configured". Kept as a
/// named function rather than an inline `is_empty` so the two spellings cannot drift.
#[must_use]
pub fn is_configured(routes: &Arc<RouteRegistry>) -> bool {
    routes
        .names()
        .iter()
        .any(|name| routes.get(name).is_ok_and(|route| !route.models.is_empty()))
}

/// Chooses the default selection, preferring `preferred` and falling back to the first route.
///
/// # Errors
///
/// [`DefaultModelError::NoModels`] when nothing in the catalogue serves a model.
pub fn choose_selection(
    routes: &Arc<RouteRegistry>,
    preferred: &str,
) -> Result<Selection, DefaultModelError> {
    // The preferred provider first, because upstream does: a deployment that has the group it names
    // should not get a different model just because another route registered earlier.
    if let Ok(route) = routes.get(preferred) {
        if let Some(model) = route.models.first() {
            return Ok(Selection {
                provider: route.name.clone(),
                model: model.id.clone(),
                // `None` because `ModelInfo` does not model a default effort: upstream reads
                // `model.reasoning?.defaultEffort`, and this catalogue records only whether a model
                // reasons at all. Emitting a value here would be inventing one; leaving it out is
                // what upstream does when the model declares none.
                reasoning_effort: None,
            });
        }
    }
    // Otherwise the first route with a model, in the catalogue's own order so two runs agree.
    for name in routes.names() {
        let Ok(route) = routes.get(&name) else {
            continue;
        };
        if let Some(model) = route.models.first() {
            return Ok(Selection {
                provider: route.name.clone(),
                model: model.id.clone(),
                // `None` because `ModelInfo` does not model a default effort: upstream reads
                // `model.reasoning?.defaultEffort`, and this catalogue records only whether a model
                // reasons at all. Emitting a value here would be inventing one; leaving it out is
                // what upstream does when the model declares none.
                reasoning_effort: None,
            });
        }
    }
    Err(DefaultModelError::NoModels {
        preferred: preferred.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::providers::{ModelInfo, Route};

    /// A registry with one route serving one model.
    fn one_model(route: &str, model: &str) -> Arc<RouteRegistry> {
        let mut routes = RouteRegistry::new();
        let entry = Route::new(route, "test").with_model(ModelInfo::text(model, 128_000, 8_192));
        routes.register(entry).expect("registered");
        Arc::new(routes)
    }

    #[test]
    fn an_empty_catalogue_is_an_error_not_a_no_op() {
        let routes = Arc::new(RouteRegistry::new());
        let error = choose_selection(&routes, PREFERRED_PROVIDER).expect_err("no models");
        assert_eq!(
            error,
            DefaultModelError::NoModels {
                preferred: PREFERRED_PROVIDER.to_owned()
            }
        );
        assert!(!is_configured(&routes));
    }

    #[test]
    fn the_preferred_provider_wins_when_it_exists() {
        let mut routes = RouteRegistry::new();
        routes
            .register(Route::new("other", "test").with_model(ModelInfo::text(
                "other-model",
                128_000,
                8_192,
            )))
            .expect("registered");
        routes
            .register(
                Route::new("deepseek-account", "test")
                    .with_model(ModelInfo::text("chosen", 128_000, 8_192)),
            )
            .expect("registered");
        let routes = Arc::new(routes);
        let selection = choose_selection(&routes, PREFERRED_PROVIDER).expect("chosen");
        assert_eq!(selection.provider, "deepseek-account");
        assert_eq!(selection.model, "chosen");
    }

    #[test]
    fn another_provider_is_used_when_the_preferred_one_is_absent() {
        // A deployment that configured one route still gets a default rather than an error.
        let routes = one_model("only-route", "only-model");
        let selection = choose_selection(&routes, PREFERRED_PROVIDER).expect("chosen");
        assert_eq!(selection.provider, "only-route");
        assert_eq!(selection.model, "only-model");
    }

    #[test]
    fn a_configured_deployment_is_recognised() {
        assert!(is_configured(&one_model("r", "m")));
    }

    #[test]
    fn a_route_with_no_models_is_not_a_configuration() {
        let mut routes = RouteRegistry::new();
        routes
            .register(Route::new("empty", "test"))
            .expect("registered");
        let routes = Arc::new(routes);
        assert!(!is_configured(&routes));
        assert!(choose_selection(&routes, PREFERRED_PROVIDER).is_err());
    }

    #[test]
    fn the_wire_omits_an_effort_the_model_does_not_declare() {
        let selection = Selection {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            reasoning_effort: None,
        };
        let wire = selection.to_wire();
        assert!(wire.get("reasoningEffort").is_none(), "{wire}");
        assert_eq!(wire["provider"], "p");
    }
}
