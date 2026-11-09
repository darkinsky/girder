//! Tests for the provider registry and catalogue (M3 of the v2 plan).
//!
//! A second provider needs two things beyond its own request path: a name that does not collide with
//! the first, and a declared window so compaction can decide before sending rather than after a
//! rejection.

use std::sync::Arc;

use girder_core::{
    Modality, ModelInfo, MountedProvider, ProviderRoute, Providers, Route, RouteError,
    RouteRegistry, model_fits, window_of,
};
use tokio_util::sync::CancellationToken;

// ---- the catalogue ----------------------------------------------------------------------------

#[test]
fn a_model_declares_its_limits() {
    let model = ModelInfo::text("deepseek-chat", 64_000, 8_192);

    assert_eq!(window_of(&model), Some(64_000));
    assert_eq!(model.max_tokens, Some(8_192));
    assert_eq!(model.input_modalities, vec![Modality::Text]);
    assert!(model.supports_tools);
    assert!(!model.supports_reasoning);
}

#[test]
fn an_unknown_window_cannot_answer_whether_something_fits() {
    // The load-bearing distinction: compaction must not compact on a guess, and must not refuse a
    // request the model would accept.
    let model = ModelInfo::text("m", 100, 10).with_unknown_window();

    assert_eq!(window_of(&model), None);
    assert_eq!(
        model_fits(&model, 1_000_000),
        None,
        "cannot claim it does not fit"
    );
    assert_eq!(model_fits(&model, 1), None, "cannot claim it fits either");
}

#[test]
fn a_known_window_answers_fit_at_the_boundary() {
    let model = ModelInfo::text("m", 100, 10);

    assert_eq!(model_fits(&model, 99), Some(true));
    assert_eq!(
        model_fits(&model, 100),
        Some(true),
        "the boundary itself fits"
    );
    assert_eq!(model_fits(&model, 101), Some(false));
}

#[test]
fn a_model_can_declare_images_and_reasoning() {
    let model = ModelInfo::text("m", 100, 10).with_images().with_reasoning();

    assert!(model.input_modalities.contains(&Modality::Image));
    assert!(model.supports_reasoning);
}

#[test]
fn adding_images_twice_is_idempotent() {
    let model = ModelInfo::text("m", 100, 10).with_images().with_images();
    assert_eq!(
        model
            .input_modalities
            .iter()
            .filter(|m| **m == Modality::Image)
            .count(),
        1
    );
}

// ---- routes -----------------------------------------------------------------------------------

#[test]
fn the_first_model_becomes_the_default() {
    let route = Route::new("a", "p")
        .with_model(ModelInfo::text("m1", 100, 10))
        .with_model(ModelInfo::text("m2", 200, 20));

    assert_eq!(route.default().map(|m| m.id.as_str()), Some("m1"));
}

#[test]
fn an_explicit_default_wins() {
    let route = Route::new("a", "p")
        .with_model(ModelInfo::text("m1", 100, 10))
        .with_model(ModelInfo::text("m2", 200, 20))
        .with_default("m2");

    assert_eq!(route.default().map(|m| m.id.as_str()), Some("m2"));
}

#[test]
fn a_route_with_no_models_has_no_default() {
    assert!(Route::new("a", "p").default().is_none());
}

// ---- the registry -----------------------------------------------------------------------------

#[test]
fn two_mounts_of_one_provider_do_not_shadow_each_other() {
    // A personal key and an organization key are two routes, not one.
    let mut registry = RouteRegistry::new();
    registry
        .register(
            Route::new("deepseek-personal", "deepseek").with_model(ModelInfo::text("m", 100, 10)),
        )
        .unwrap();
    registry
        .register(Route::new("deepseek-org", "deepseek").with_model(ModelInfo::text("m", 100, 10)))
        .unwrap();

    assert_eq!(registry.len(), 2);
    assert_eq!(
        registry.get("deepseek-personal").unwrap().provider,
        "deepseek"
    );
    assert_eq!(registry.get("deepseek-org").unwrap().provider, "deepseek");
}

#[test]
fn a_duplicate_name_is_refused() {
    // Replacing silently would leave a recorded route name pointing at a different provider after a
    // config reload.
    let mut registry = RouteRegistry::new();
    registry.register(Route::new("a", "p")).unwrap();

    let error = registry.register(Route::new("a", "other")).unwrap_err();

    let RouteError::Duplicate { name } = error else {
        panic!("expected Duplicate, got {error:?}");
    };
    assert_eq!(name, "a");
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
fn resolve_refuses_a_model_the_route_does_not_serve() {
    let mut registry = RouteRegistry::new();
    registry
        .register(Route::new("a", "p").with_model(ModelInfo::text("m1", 100, 10)))
        .unwrap();

    let error = registry.resolve("a", Some("nope")).unwrap_err();

    let RouteError::UnknownModel { route, model } = error else {
        panic!("expected UnknownModel, got {error:?}");
    };
    assert_eq!((route.as_str(), model.as_str()), ("a", "nope"));
}

#[test]
fn an_unknown_route_is_named_in_the_error() {
    let registry = RouteRegistry::new();
    let error = registry.get("missing").unwrap_err();

    let RouteError::UnknownRoute { name } = error else {
        panic!("expected UnknownRoute");
    };
    assert_eq!(name, "missing");
}

#[test]
fn route_names_are_sorted() {
    // A selector lists them, and an order that changes between runs makes the UI jump.
    let mut registry = RouteRegistry::new();
    for name in ["zeta", "alpha", "mid"] {
        registry.register(Route::new(name, "p")).unwrap();
    }

    assert_eq!(registry.names(), vec!["alpha", "mid", "zeta"]);
}

#[test]
fn the_catalogue_lists_every_model_with_its_route() {
    let mut registry = RouteRegistry::new();
    registry
        .register(
            Route::new("a", "p")
                .with_model(ModelInfo::text("m1", 100, 10))
                .with_model(ModelInfo::text("m2", 200, 20)),
        )
        .unwrap();
    registry
        .register(Route::new("b", "p").with_model(ModelInfo::text("m3", 300, 30)))
        .unwrap();

    let catalogue = registry.catalogue();

    assert_eq!(catalogue.len(), 3);
    assert!(
        catalogue
            .iter()
            .any(|(route, model)| route == "a" && model.id == "m2")
    );
    assert!(
        catalogue
            .iter()
            .any(|(route, model)| route == "b" && model.id == "m3")
    );
}

// ---- the callable side ------------------------------------------------------------------------

/// A provider that never streams; enough to prove registration pairs the two halves.
struct Silent;

impl girder_core::LlmProvider for Silent {
    fn stream(
        &self,
        _request: girder_core::LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, girder_core::LlmError>> {
        Box::pin(async { Err(girder_core::LlmError::Provider("silent".to_owned())) })
    }
}

#[test]
fn a_registered_route_is_callable() {
    let mut providers = Providers::new();
    let route = Route::new("test", "silent").with_model(ModelInfo::text("m", 100, 10));
    let mounted = Arc::new(MountedProvider::new("test", Arc::new(Silent)));

    providers.register(route, mounted).expect("registered");

    assert!(providers.provider("test").is_ok());
    assert_eq!(providers.catalogue().len(), 1);
}

#[test]
fn registering_twice_refuses_the_second() {
    let mut providers = Providers::new();
    let make = || {
        (
            Route::new("dup", "silent").with_model(ModelInfo::text("m", 100, 10)),
            Arc::new(MountedProvider::new("dup", Arc::new(Silent))) as Arc<dyn ProviderRoute>,
        )
    };
    let (route, provider) = make();
    providers.register(route, provider).unwrap();

    let (route, provider) = make();
    let error = providers.register(route, provider).unwrap_err();

    assert!(matches!(error, RouteError::Duplicate { .. }));
}

#[test]
fn a_mounted_provider_reports_its_mount_name() {
    // The name lives on the adapter, which is what lets one provider be mounted twice.
    let mounted = MountedProvider::new("my-name", Arc::new(Silent));
    assert_eq!(mounted.name(), "my-name");
}

#[test]
fn the_deepseek_route_advertises_a_window() {
    // A catalogue that disagrees with the provider would make compaction plan against a window the
    // provider does not have.
    let route = girder_core::deepseek_route("ds", "deepseek-chat");

    assert_eq!(route.provider, "deepseek");
    assert_eq!(
        route.default().map(|m| m.id.as_str()),
        Some("deepseek-chat")
    );
    assert!(route.default().unwrap().context_window.is_some());
}
