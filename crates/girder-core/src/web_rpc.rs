//! `web.listProviders`: the search and fetch providers this deployment registered.
//!
//! # What upstream has
//!
//! `packages/web/web/src/index.ts` marks one method:
//!
//! ```typescript
//! @Remote
//! listProviders(): WebProviderList {
//!   return {
//!     search: [...this.searchProviders.values()].map(providerInfo),
//!     fetch: [...this.fetchProviders.values()].map(providerInfo),
//!   }
//! }
//! ```
//!
//! Two lists rather than one, because the two capabilities are separate: a deployment may have a
//! fetch endpoint and no search index, and a picker that merged them would offer a search that
//! cannot run.
//!
//! Availability does not filter. A provider that is registered but unreachable still appears, which
//! is what lets a picker show it as disabled with a reason rather than omitting it and leaving a
//! person to wonder where it went.
//!
//! # What this port has
//!
//! [`crate::web::EndpointProvider`] is a search or fetch provider with a name and an endpoint. This
//! module reads a registered set out; it decides nothing.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::remote::{RemoteCall, RemoteRegistry};
use crate::web::EndpointProvider;

/// The namespace this method registers under, as upstream spells it.
pub const NAMESPACE: &str = "web";

/// The one endpoint.
pub const LIST_PROVIDERS_METHOD: &str = "listProviders";

/// The providers a deployment registered, split by capability.
///
/// Separate vectors rather than one with a `kind` field: upstream's shape has two keys, and a
/// client written against it reads `list.search` and `list.fetch`.
#[derive(Default)]
pub struct WebProviders {
    /// Providers that answer a search query.
    pub search: Vec<EndpointProvider>,
    /// Providers that fetch one URL.
    pub fetch: Vec<EndpointProvider>,
}

impl WebProviders {
    /// An empty set, which is what a deployment with no web configuration has.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The set this deployment's environment names.
    ///
    /// `GIRDER_FETCH_ENDPOINT` is the variable the web tools already read, so the provider list and
    /// the tools cannot disagree about what is configured. A search endpoint has no such variable in
    /// this build, so that list is empty -- reported as empty rather than hidden.
    #[must_use]
    pub fn from_environment() -> Self {
        let mut providers = Self::new();
        if let Ok(endpoint) = std::env::var("GIRDER_FETCH_ENDPOINT") {
            if !endpoint.is_empty() {
                providers.push_fetch(EndpointProvider {
                    name: "fetch".to_owned(),
                    endpoint,
                });
            }
        }
        providers
    }

    /// Adds a search provider.
    pub fn push_search(&mut self, provider: EndpointProvider) {
        self.search.push(provider);
    }

    /// Adds a fetch provider.
    pub fn push_fetch(&mut self, provider: EndpointProvider) {
        self.fetch.push(provider);
    }
}

/// Renders one provider the way upstream's `providerInfo` is shaped.
///
/// `description` is omitted when there is none, matching the spread-of-undefined upstream uses. The
/// `id` is the provider's name, because upstream keys its map by name and reads it back into the
/// same field.
fn provider_info(provider: &EndpointProvider) -> Value {
    json!({
        "id": provider.name,
        "title": provider.name,
        // The endpoint is this build's own field: upstream's providers carry a description, and a
        // deployment here configures a URL. Reporting it lets a picker show what it will call.
        "endpoint": provider.endpoint,
    })
}

/// Registers `web.listProviders`.
pub fn register_web_methods(registry: &mut RemoteRegistry, providers: &Arc<WebProviders>) {
    let providers = Arc::clone(providers);
    registry.register(
        NAMESPACE,
        LIST_PROVIDERS_METHOD,
        move |_call: RemoteCall| {
            let providers = Arc::clone(&providers);
            Box::pin(async move {
                Ok(json!({
                    "search": providers.search.iter().map(provider_info).collect::<Vec<_>>(),
                    "fetch": providers.fetch.iter().map(provider_info).collect::<Vec<_>>(),
                }))
            })
        },
    );
    // A read: the provider set is configuration and nothing here writes it.
    registry.declare(
        NAMESPACE,
        LIST_PROVIDERS_METHOD,
        crate::access::Declaration::read("webProvider"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_capabilities_stay_in_their_own_lists() {
        // A deployment may have a fetch endpoint and no search index; merging them would offer a
        // search that cannot run.
        let mut providers = WebProviders::new();
        providers.push_fetch(EndpointProvider {
            name: "fetch".to_owned(),
            endpoint: "https://fetch.example".to_owned(),
        });
        assert!(providers.search.is_empty());
        assert_eq!(providers.fetch.len(), 1);
    }

    #[test]
    fn a_provider_reports_the_endpoint_it_will_call() {
        let info = provider_info(&EndpointProvider {
            name: "search".to_owned(),
            endpoint: "https://search.example".to_owned(),
        });
        assert_eq!(info["id"], "search");
        assert_eq!(info["endpoint"], "https://search.example");
    }
}
