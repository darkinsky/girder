//! Provider registry and model catalogue.
//!
//! The `llm` package is 15k lines because it holds the message model, the assembler, the retry
//! policy, and the catalogue. What a *second* provider needs is the part that lets one exist beside
//! another, and that is what this module carries.
//!
//! # Why a catalogue rather than asking the provider
//!
//! A request that exceeds a model's window is rejected by the provider after a round trip, and the
//! rejection is a generic "context length exceeded" with no number in it. Knowing the window up front
//! is what lets compaction decide *before* sending. A provider that reports its own catalogue on
//! every request would pay a round trip per decision.
//!
//! # Why routes rather than providers
//!
//! One provider can serve several models, and one deployment can mount the same provider twice with
//! different credentials — a personal key and an organization key. A route is the addressable unit:
//! `(provider, model)` resolved through a name the caller chose. The registry keys on that name, so
//! two mounts of one provider are two routes and neither shadows the other.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::scope::BoxFut;

/// What a model can accept as input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modality {
    /// Text.
    Text,
    /// Images.
    Image,
}

/// One model a route serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// The id a request names.
    pub id: String,
    /// Human-readable name, for a selector.
    pub display_name: String,
    /// The largest context the model accepts, in tokens.
    ///
    /// `None` when unknown. Compaction must treat unknown as "cannot predict" rather than as zero:
    /// assuming a number would compact needlessly or refuse a request the model would accept.
    pub context_window: Option<u64>,
    /// The largest output it produces, in tokens.
    pub max_tokens: Option<u64>,
    /// Accepted input types, when disclosed.
    pub input_modalities: Vec<Modality>,
    /// Whether the model can call tools.
    pub supports_tools: bool,
    /// Whether the model accepts a reasoning effort setting.
    pub supports_reasoning: bool,
}

impl ModelInfo {
    /// A text-only model with a declared window.
    #[must_use]
    pub fn text(id: impl Into<String>, context_window: u64, max_tokens: u64) -> Self {
        let id = id.into();
        Self {
            display_name: id.clone(),
            id,
            context_window: Some(context_window),
            max_tokens: Some(max_tokens),
            input_modalities: vec![Modality::Text],
            supports_tools: true,
            supports_reasoning: false,
        }
    }

    /// Declares an unreadable window.
    ///
    /// Compaction then cannot predict, and must not compact on a guess.
    #[must_use]
    pub fn with_unknown_window(mut self) -> Self {
        self.context_window = None;
        self
    }

    /// Allows image input.
    #[must_use]
    pub fn with_images(mut self) -> Self {
        if !self.input_modalities.contains(&Modality::Image) {
            self.input_modalities.push(Modality::Image);
        }
        self
    }

    /// Declares reasoning-effort support.
    #[must_use]
    pub fn with_reasoning(mut self) -> Self {
        self.supports_reasoning = true;
        self
    }
}

/// A registered provider route.
#[derive(Clone, Debug)]
pub struct Route {
    /// The name a caller uses to select this route.
    ///
    /// A caller-chosen name rather than the provider's id: two mounts of one provider need two names,
    /// and the name is what appears in a log.
    pub name: String,
    /// The provider's own identifier.
    pub provider: String,
    /// Human-readable provider name.
    pub display_name: String,
    /// The models this route serves.
    pub models: Vec<ModelInfo>,
    /// The model used when a request does not name one.
    pub default_model: Option<String>,
}

impl Route {
    /// Builds a route.
    #[must_use]
    pub fn new(name: impl Into<String>, provider: impl Into<String>) -> Self {
        let provider = provider.into();
        Self {
            name: name.into(),
            display_name: provider.clone(),
            provider,
            models: Vec::new(),
            default_model: None,
        }
    }

    /// Adds a model.
    #[must_use]
    pub fn with_model(mut self, model: ModelInfo) -> Self {
        if self.default_model.is_none() {
            self.default_model = Some(model.id.clone());
        }
        self.models.push(model);
        self
    }

    /// Sets the display name.
    #[must_use]
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = name.into();
        self
    }

    /// Sets the default model.
    #[must_use]
    pub fn with_default(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Finds a model by id.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|model| model.id == id)
    }

    /// The model a request without an explicit one uses.
    #[must_use]
    pub fn default(&self) -> Option<&ModelInfo> {
        self.default_model
            .as_deref()
            .and_then(|id| self.model(id))
            .or_else(|| self.models.first())
    }
}

/// Why a route lookup failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouteError {
    /// No route has that name.
    #[error("no provider route named `{name}`")]
    UnknownRoute {
        /// The name asked for.
        name: String,
    },

    /// The route exists but does not serve that model.
    #[error("route `{route}` does not serve model `{model}`")]
    UnknownModel {
        /// The route.
        route: String,
        /// The model.
        model: String,
    },

    /// A route was registered twice.
    #[error("a route named `{name}` is already registered")]
    Duplicate {
        /// The name.
        name: String,
    },
}

/// The set of mounted routes.
#[derive(Clone, Debug, Default)]
pub struct RouteRegistry {
    routes: BTreeMap<String, Route>,
}

impl RouteRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a route.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::Duplicate`] when the name is taken. Replacing a route silently would
    /// leave a caller's recorded route name pointing at a different provider after a config reload.
    pub fn register(&mut self, route: Route) -> Result<(), RouteError> {
        if self.routes.contains_key(&route.name) {
            return Err(RouteError::Duplicate {
                name: route.name.clone(),
            });
        }
        self.routes.insert(route.name.clone(), route);
        Ok(())
    }

    /// Looks up a route by name.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::UnknownRoute`] when nothing is registered under that name.
    pub fn get(&self, name: &str) -> Result<&Route, RouteError> {
        self.routes
            .get(name)
            .ok_or_else(|| RouteError::UnknownRoute {
                name: name.to_owned(),
            })
    }

    /// Resolves a route and a model, filling in the default model when none is named.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::UnknownRoute`] or [`RouteError::UnknownModel`].
    pub fn resolve(
        &self,
        name: &str,
        model: Option<&str>,
    ) -> Result<(&Route, &ModelInfo), RouteError> {
        let route = self.get(name)?;
        let model = match model {
            Some(id) => route.model(id).ok_or_else(|| RouteError::UnknownModel {
                route: name.to_owned(),
                model: id.to_owned(),
            })?,
            None => route.default().ok_or_else(|| RouteError::UnknownModel {
                route: name.to_owned(),
                model: "(default)".to_owned(),
            })?,
        };
        Ok((route, model))
    }

    /// The registered route names, sorted.
    ///
    /// Sorted because a selector lists them, and an order that changes between runs makes the UI
    /// jump.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.routes.keys().cloned().collect()
    }

    /// How many routes are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether no routes are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Every model across every route, as `(route, model)` pairs.
    #[must_use]
    pub fn catalogue(&self) -> Vec<(String, ModelInfo)> {
        self.routes
            .values()
            .flat_map(|route| {
                route
                    .models
                    .iter()
                    .map(move |model| (route.name.clone(), model.clone()))
            })
            .collect()
    }
}

/// The context window to plan against, or `None` when it is unknown.
///
/// A free function rather than a method because the caller's decision is the interesting part:
/// compaction must treat an unknown window as "cannot predict" and proceed without compacting, not
/// as zero and compact everything.
#[must_use]
pub fn window_of(model: &ModelInfo) -> Option<u64> {
    model.context_window
}

/// Whether a measured request fits a model's window.
///
/// `None` when the window is unknown: not fitting is a claim that requires knowing the window, and
/// answering `false` for an unknown one would refuse every request to a model whose catalogue is
/// silent.
#[must_use]
pub fn fits(model: &ModelInfo, measured: u64) -> Option<bool> {
    model.context_window.map(|window| measured <= window)
}

/// The provider-facing seam: one mounted route's request path.
///
/// Separate from the catalogue so a provider that cannot enumerate its models still serves requests;
/// the registry holds what is known and this holds what can run.
pub trait ProviderRoute: Send + Sync {
    /// The route's name.
    fn name(&self) -> &str;

    /// Streams a request.
    fn stream(
        &self,
        request: crate::llm::LlmRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFut<Result<crate::llm::ChunkStream, crate::llm::LlmError>>;
}

/// A registry of callable routes, alongside the catalogue.
#[derive(Default)]
pub struct Providers {
    catalogue: RouteRegistry,
    routes: BTreeMap<String, Arc<dyn ProviderRoute>>,
}

impl Providers {
    /// Creates an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers both the catalogue entry and the callable route.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::Duplicate`] when either half is already present.
    pub fn register(
        &mut self,
        route: Route,
        provider: Arc<dyn ProviderRoute>,
    ) -> Result<(), RouteError> {
        if self.routes.contains_key(&route.name) {
            return Err(RouteError::Duplicate {
                name: route.name.clone(),
            });
        }
        let name = route.name.clone();
        self.catalogue.register(route)?;
        self.routes.insert(name, provider);
        Ok(())
    }

    /// The catalogue.
    #[must_use]
    pub fn catalogue(&self) -> &RouteRegistry {
        &self.catalogue
    }

    /// Looks up a callable route.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::UnknownRoute`] when nothing is registered under that name — which
    /// includes a route registered in the catalogue without a provider, a state the two-step
    /// registration in [`Providers::register`] prevents.
    pub fn provider(&self, name: &str) -> Result<Arc<dyn ProviderRoute>, RouteError> {
        self.routes
            .get(name)
            .cloned()
            .ok_or_else(|| RouteError::UnknownRoute {
                name: name.to_owned(),
            })
    }
}

/// Adapts any [`crate::llm::LlmProvider`] into a callable route.
///
/// An adapter rather than making `LlmProvider` extend `ProviderRoute`: the provider trait is the
/// request path and should not grow a name method that only the registry needs. This also lets a
/// provider be mounted under two names — a personal key and an organization key — since the name
/// lives on the adapter, not the provider.
pub struct MountedProvider {
    name: String,
    inner: Arc<dyn crate::llm::LlmProvider>,
}

impl MountedProvider {
    /// Mounts `inner` under `name`.
    #[must_use]
    pub fn new(name: impl Into<String>, inner: Arc<dyn crate::llm::LlmProvider>) -> Self {
        Self {
            name: name.into(),
            inner,
        }
    }
}

impl std::fmt::Debug for MountedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountedProvider")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ProviderRoute for MountedProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn stream(
        &self,
        request: crate::llm::LlmRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFut<Result<crate::llm::ChunkStream, crate::llm::LlmError>> {
        self.inner.stream(request, cancel)
    }
}

/// The catalogue entry a `DeepSeek` mount advertises.
///
/// Kept beside the adapter so a route's declared limits and its request path are defined together: a
/// catalogue that disagrees with the provider would make compaction plan against a window the
/// provider does not have.
#[must_use]
pub fn deepseek_route(name: impl Into<String>, model: impl Into<String>) -> Route {
    let model = model.into();
    Route::new(name, "deepseek")
        .with_display_name("DeepSeek")
        // The first model becomes the default, so a request that names none still resolves.
        .with_model(ModelInfo::text(model, 64_000, 8_192))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duplicate_route_is_refused() {
        // Replacing silently would leave a recorded route name pointing at a different provider
        // after a config reload.
        let mut registry = RouteRegistry::new();
        registry.register(Route::new("a", "p")).unwrap();

        let error = registry.register(Route::new("a", "other")).unwrap_err();

        assert!(matches!(error, RouteError::Duplicate { .. }));
    }

    #[test]
    fn the_first_model_becomes_the_default() {
        let route = Route::new("a", "p")
            .with_model(ModelInfo::text("m1", 100, 10))
            .with_model(ModelInfo::text("m2", 200, 20));

        assert_eq!(route.default().map(|m| m.id.as_str()), Some("m1"));
    }

    #[test]
    fn resolve_fills_in_the_default_model() {
        let mut registry = RouteRegistry::new();
        registry
            .register(Route::new("a", "p").with_model(ModelInfo::text("m1", 100, 10)))
            .unwrap();

        let (route, model) = registry.resolve("a", None).unwrap();

        assert_eq!(route.name, "a");
        assert_eq!(model.id, "m1");
    }

    #[test]
    fn an_unknown_window_is_not_zero() {
        // Compaction must treat it as "cannot predict", not compact on a guess.
        let model = ModelInfo::text("m", 100, 10).with_unknown_window();

        assert_eq!(window_of(&model), None);
        assert_eq!(
            fits(&model, 1_000_000),
            None,
            "cannot claim it does not fit"
        );
    }

    #[test]
    fn a_known_window_answers_fit() {
        let model = ModelInfo::text("m", 100, 10);

        assert_eq!(fits(&model, 100), Some(true));
        assert_eq!(fits(&model, 101), Some(false));
    }
}
