//! Tests for the model catalogue and selection.
//!
//! A model picker that listed nothing, or accepted a pair this deployment does not mount, would fail at
//! the next request rather than at the click. Both methods check against the route table for that
//! reason.

use std::sync::Arc;

use girder_core::{
    DirectoryCatalog, ModelInfo, RemoteCall, Route, RouteRegistry, SessionCatalog,
    default_registry, registry_with_routes,
};
use serde_json::json;

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn routes() -> Arc<RouteRegistry> {
    let mut registry = RouteRegistry::new();
    registry
        .register(
            Route::new("ds", "deepseek")
                .with_display_name("DeepSeek")
                .with_model(ModelInfo::text("deepseek-chat", 64_000, 8_192))
                .with_model(ModelInfo::text("deepseek-reasoner", 64_000, 8_192).with_reasoning()),
        )
        .expect("registered");
    Arc::new(registry)
}

fn registry(routes: &Arc<RouteRegistry>) -> girder_core::RemoteRegistry {
    let dir = std::env::temp_dir().join(format!("girder-model-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let runtime = girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
        provider: Arc::new(Silent),
        tools: Arc::new(girder_core::ToolRegistry::new()),
        bus: Arc::new(girder_core::EventBus::new()),
        model: "deepseek-chat".to_owned(),
        max_turns: 1,
    });
    registry_with_routes(&catalog, &runtime, routes)
}

struct Silent;

impl girder_core::LlmProvider for Silent {
    fn stream(
        &self,
        _request: girder_core::LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, girder_core::LlmError>> {
        Box::pin(async { Err(girder_core::LlmError::Cancelled) })
    }
}

// ---- the catalogue ----------------------------------------------------------------------------

#[tokio::test]
async fn the_catalogue_lists_every_route() {
    let registry = registry(&routes());

    let value = registry
        .dispatch("session", "modelCatalog", call(json!({})))
        .await
        .expect("listed");

    let groups = value["groups"].as_array().expect("an array");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["provider"], "ds");
    assert_eq!(groups[0]["displayName"], "DeepSeek");
    assert_eq!(groups[0]["models"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn the_catalogue_reports_a_context_window() {
    // A client shows it; omitting it would make the picker claim the model has no window.
    let registry = registry(&routes());

    let value = registry
        .dispatch("session", "modelCatalog", call(json!({})))
        .await
        .expect("listed");

    assert_eq!(value["groups"][0]["models"][0]["contextWindow"], 64_000);
}

#[tokio::test]
async fn the_catalogue_defaults_to_the_first_routes_first_model() {
    let registry = registry(&routes());

    let value = registry
        .dispatch("session", "modelCatalog", call(json!({})))
        .await
        .expect("listed");

    assert_eq!(value["default"]["provider"], "ds");
    assert_eq!(value["default"]["model"], "deepseek-chat");
}

#[tokio::test]
async fn the_catalogue_lists_only_routable_providers() {
    // A provider with no models cannot serve a request, so offering it would be a dead choice.
    let mut routes = RouteRegistry::new();
    routes
        .register(Route::new("empty", "nothing"))
        .expect("registered");
    routes
        .register(Route::new("ds", "deepseek").with_model(ModelInfo::text("m", 100, 10)))
        .expect("registered");
    let registry = registry(&Arc::new(routes));

    let value = registry
        .dispatch("session", "modelCatalog", call(json!({})))
        .await
        .expect("listed");

    let providers: Vec<&str> = value["routableProviders"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert_eq!(providers, vec!["ds"]);
}

#[tokio::test]
async fn an_empty_route_table_yields_no_default() {
    // An invented pair would be a selection nothing can route.
    let registry = registry(&Arc::new(RouteRegistry::new()));

    let value = registry
        .dispatch("session", "modelCatalog", call(json!({})))
        .await
        .expect("listed");

    assert!(value["default"].is_null());
    assert_eq!(value["groups"].as_array().map(Vec::len), Some(0));
    assert_eq!(value["failures"].as_array().map(Vec::len), Some(0));
}

// ---- selection --------------------------------------------------------------------------------

#[tokio::test]
async fn a_selection_naming_a_mounted_route_is_accepted() {
    let registry = registry(&routes());

    let value = registry
        .dispatch(
            "session",
            "selectModel",
            call(json!({"provider": "ds", "model": "deepseek-reasoner"})),
        )
        .await
        .expect("selected");

    assert_eq!(value["selected"]["provider"], "ds");
    assert_eq!(value["selected"]["model"], "deepseek-reasoner");
}

#[tokio::test]
async fn a_selection_for_an_unmounted_provider_is_refused() {
    // Accepted here, it would fail at the next request, far from the client that sent it.
    let registry = registry(&routes());

    let error = registry
        .dispatch(
            "session",
            "selectModel",
            call(json!({"provider": "nope", "model": "m"})),
        )
        .await
        .expect_err("unmounted provider");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
}

#[tokio::test]
async fn a_selection_for_an_unserved_model_is_refused() {
    let registry = registry(&routes());

    let error = registry
        .dispatch(
            "session",
            "selectModel",
            call(json!({"provider": "ds", "model": "gpt-4"})),
        )
        .await
        .expect_err("unserved model");

    let girder_core::RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("gpt-4"), "{message}");
}

#[tokio::test]
async fn a_partial_selection_is_refused() {
    // A provider without a model has nothing to call.
    let registry = registry(&routes());

    for args in [json!({"provider": "ds"}), json!({"model": "deepseek-chat"})] {
        let error = registry
            .dispatch("session", "selectModel", call(args))
            .await
            .expect_err("partial");
        assert!(matches!(
            error,
            girder_core::RemoteError::InvalidArguments { .. }
        ));
    }
}

#[test]
fn the_plain_registry_does_not_offer_the_model_methods() {
    // A registry built without routes would offer a picker that lists nothing; the plain constructor is
    // left unchanged so a caller that has no routes does not gain a broken capability.
    let dir = std::env::temp_dir().join(format!("girder-model-plain-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let names = default_registry(catalog).names();
    let listed: Vec<String> = names
        .into_iter()
        .map(|(namespace, method)| format!("{namespace}.{method}"))
        .collect();

    assert!(!listed.contains(&"session.modelCatalog".to_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}
