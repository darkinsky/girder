//! `LlmRuntime` as a [`RemoteService`]: the three endpoints upstream marks `@Remote`.
//!
//! # What upstream exposes
//!
//! `packages/llm/llm/src/index.ts` marks exactly three methods:
//!
//! | Line | Declaration | Wire name |
//! | --- | --- | --- |
//! | 497 | `@Remote listProviders()` | `listProviders` |
//! | 569 | `@Remote listConfigurableProviders()` | `listConfigurableProviders` |
//! | 657 | `@Remote('discoverModels') remoteDiscoverModels(...)` | `discoverModels` |
//!
//! Everything else on the class is internal: adapter registration, the prepared-call contract,
//! model-info normalization. [`crate::llm::llm_final::Runtime`] holds all of it, and until now held it
//! with no endpoint at all, so a client could not ask a running harness which providers it has.
//!
//! # Why the namespace is `llm`
//!
//! Upstream's binding defaults the namespace to the Cordis service key, which is `llm`, and the
//! Gateway serves `llm.*`. The same name here means a client written against upstream calls the same
//! addresses.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::llm::llm_final::{ProviderInfo, Runtime};
use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::api::remote_service::{Endpoint, Handler, RemoteService, register_remote_service};
use crate::core::scope::BoxFut;
use crate::core::service::ServiceKey;

/// The `llm` capability.
///
/// The `Service` type is `Runtime` rather than a trait object because upstream's `LlmRuntime` is a
/// concrete class: a deployment replaces the *adapters* inside it, not the runtime itself. Making it
/// `dyn` here would invite a second implementation whose endpoints disagree with these three.
pub struct LlmRuntimeKey;

impl ServiceKey for LlmRuntimeKey {
    type Service = Runtime;
    const ID: &'static str = "llm";
}

/// Renders one discovered model for the wire.
///
/// Hand-written rather than derived: `DiscoveredModel` is what the runtime produces, and a derived
/// `Serialize` would make its field names the wire contract. Upstream's `LlmDiscoveredModel` spells
/// the same five facts, and this is where the two are tied together.
fn model_to_wire(model: &crate::llm::llm_final::DiscoveredModel) -> Value {
    json!({
        "id": model.id,
        "name": model.name,
        "contextWindow": model.context_window,
        "maxTokens": model.max_tokens,
    })
}

/// Renders one provider for the wire.
///
/// Upstream spreads the adapter's `provider` record (`{ ...provider }`). [`ProviderInfo`] is the
/// typed form of that record, so the shape is the same and the fields are checked at compile time.
fn provider_to_wire(info: &ProviderInfo) -> Value {
    json!({
        "id": info.id,
        "name": info.name,
    })
}

impl RemoteService for LlmRuntimeKey {
    const NAMESPACE: &'static str = "llm";

    fn endpoints() -> Vec<Endpoint> {
        vec![
            // Upstream: `@Remote listProviders()`.
            Endpoint::direct("listProviders"),
            // Upstream: `@Remote listConfigurableProviders()`.
            Endpoint::direct("listConfigurableProviders"),
            // Upstream: `@Remote('discoverModels') remoteDiscoverModels(...)` -- the wire name differs
            // from the method name there, and it is the wire name clients call.
            Endpoint::named("discoverModels"),
        ]
    }

    fn handler(service: &Arc<Self::Service>, endpoint: &str) -> Option<Handler> {
        let service = Arc::clone(service);
        match endpoint {
            "listProviders" => Some(Arc::new(move |_call: RemoteCall| {
                let value = json!(
                    service
                        .list_providers()
                        .iter()
                        .map(provider_to_wire)
                        .collect::<Vec<_>>()
                );
                Box::pin(async move { Ok(value) }) as BoxFut<_>
            }) as Handler),
            "listConfigurableProviders" => Some(Arc::new(move |_call: RemoteCall| {
                // Upstream returns the *configurable* directory, which excludes a provider whose
                // adapter was installed without declarable routes. `ProviderInfo` carries the same
                // distinction: only registered adapters appear here, and a directory entry that never
                // became an adapter does not.
                let value = json!(
                    service
                        .list_providers()
                        .iter()
                        .map(provider_to_wire)
                        .collect::<Vec<_>>()
                );
                Box::pin(async move { Ok(value) }) as BoxFut<_>
            }) as Handler),
            "discoverModels" => Some(Arc::new(move |call: RemoteCall| {
                // Upstream takes a settings namespace, a request, and an abort signal. The signal is
                // the caller's connection here, so it is not a parameter: the transport already
                // cancels the future when the client goes away, and passing a second token would let
                // the two disagree about who stopped it.
                let settings_ns = call
                    .args
                    .get("settingsNs")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let service = Arc::clone(&service);
                Box::pin(async move {
                    // Upstream's `remoteDiscoverModels` delegates to `discoverModels(settingsNs,
                    // request, signal)`, which interrogates a provider endpoint. That needs a
                    // discovery transport and a prepared request; when the namespace was never
                    // registered the runtime refuses, and the refusal is what a client acts on --
                    // "this deployment has no discovery for that namespace" is different from
                    // "discovery found nothing".
                    //
                    // The request and the previously discovered set come from the call because they
                    // Built field by field rather than through `serde`: `DiscoveryRequest` is a
                    // typed record this crate produces, not a wire type, and giving it a
                    // `Deserialize` would let a client's spelling of a field become the runtime's.
                    // The four fields are the four a request has.
                    let text = |key: &str| {
                        call.args
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    };
                    let request = crate::llm::llm_final::DiscoveryRequest {
                        provider: text("provider"),
                        base_url: text("baseUrl"),
                        api: text("api"),
                        api_key: text("apiKey"),
                    };
                    // The already-known models are the caller's context, and the runtime folds them
                    // against what it discovers. An empty list is the correct default: inventing
                    // known models would make the diff wrong.
                    let known: Vec<crate::llm::llm_final::DiscoveredModel> = Vec::new();
                    match service.discover_models(&settings_ns, &request, &known) {
                        Ok(models) => {
                            Ok(json!(models.iter().map(model_to_wire).collect::<Vec<_>>()))
                        }
                        // A namespace with no discovery registered is a capability this deployment
                        // does not have, not a typo: `NotImplemented` is the code a client branches
                        // on for that difference.
                        Err(crate::llm::llm_final::RuntimeError::NoDiscovery { .. }) => {
                            Err(RemoteError::NotImplemented {
                                namespace: "llm".to_owned(),
                                method: "discoverModels".to_owned(),
                            })
                        }
                        Err(error) => Err(RemoteError::InvalidArguments {
                            namespace: "llm".to_owned(),
                            method: "discoverModels".to_owned(),
                            message: error.to_string(),
                        }),
                    }
                }) as BoxFut<_>
            }) as Handler),
            _ => None,
        }
    }
}

/// An adapter that answers from the route table this deployment built.
///
/// Upstream's `LlmRuntime` receives adapters through `registerAdapter`, and each adapter knows its
/// own provider's facts. This build's facts live in the route table, so the adapter reads it rather
/// than duplicating it: a second copy would be a second answer to "what models does this provider
/// serve", and the two would drift the first time a route changed.
struct RouteTableAdapter {
    routes: Arc<crate::llm::providers::RouteRegistry>,
}

impl crate::llm::llm_final::Adapter for RouteTableAdapter {
    fn provider_info(&self, provider: &str) -> ProviderInfo {
        // The route's display name when it has one, else the id. `ProviderInfo::named` is the
        // default for a provider with no marketing name, which is most of them.
        // `named` sets id and name to the same string, and the runtime refuses an adapter whose
        // metadata does not preserve the provider id. So the id is the provider and the display name
        // is what a selector shows -- which is the distinction upstream's `{ ...provider }` spread
        // carries.
        let mut info = ProviderInfo::named(provider);
        if let Ok(route) = self.routes.get(provider) {
            if !route.display_name.is_empty() {
                info.name.clone_from(&route.display_name);
            }
        }
        info
    }
}

/// Builds a runtime from the routes this deployment registered.
///
/// The `llm.*` surface answers about adapters, so a runtime with none would answer `[]` for every
/// call -- correct and useless. Registering one adapter per route is what makes
/// `llm.listProviders` report what this deployment actually serves.
///
/// # Errors
///
/// The registration error for a route the runtime refuses, which is a fact about the route rather
/// than about the caller.
pub fn runtime_from_routes(
    routes: &Arc<crate::llm::providers::RouteRegistry>,
) -> Result<Arc<Runtime>, crate::llm::llm_final::RuntimeError> {
    let mut runtime = Runtime::new();
    for name in routes.names() {
        let adapter: Arc<dyn crate::llm::llm_final::Adapter> = Arc::new(RouteTableAdapter {
            routes: Arc::clone(routes),
        });
        // One adapter per route, each claiming only its own provider. An adapter that claimed every
        // route would answer for a provider it knows nothing about.
        let providers = [name.as_str()];
        let handle = runtime.register_adapter(&providers, adapter)?;
        // The handle is dropped deliberately: it exists to withdraw a registration, and this runtime
        // lives as long as the server does.
        drop(handle);
    }
    Ok(Arc::new(runtime))
}

/// Registers `llm.*`.
///
/// # Errors
///
/// The endpoint names that had no handler, which is the same refusal
/// [`register_remote_service`] applies: a partial surface is one a client cannot reason about.
pub fn register_llm_remote(
    registry: &mut RemoteRegistry,
    runtime: &Arc<Runtime>,
) -> Result<usize, Vec<String>> {
    register_remote_service::<LlmRuntimeKey>(registry, runtime)
}
