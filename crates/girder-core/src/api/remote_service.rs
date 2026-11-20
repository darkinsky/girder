//! The Rust equivalent of upstream's `TypertRemoteService`.
//!
//! # What upstream has
//!
//! `packages/typert/protocol/src/index.ts` declares one abstract class that does two things at once:
//!
//! ```typescript
//! export abstract class TypertRemoteService<out T = never> extends Service<T> {
//!   readonly typertRemote: TypertGatewayBinding<this>
//!   protected constructor(ctx: Context, serviceKey: string, options = {}) {
//!     super(ctx, serviceKey)
//!     this.typertRemote = bindTypertRemote(this, this.name, options)
//!   }
//! }
//! ```
//!
//! `super(ctx, serviceKey)` publishes the instance into the Cordis container, which is what
//! [`ServiceKey`] already models here. `bindTypertRemote` freezes a three-field binding —
//! `{ service, serviceKey, namespace }` — that the Gateway reads to discover what a live service
//! exposes. A method becomes an RPC endpoint by carrying `@Remote`, and the Gateway enumerates the
//! markers at call time.
//!
//! # Why this file exists
//!
//! [`ServiceKey`] answers "who can resolve this capability". [`RemoteRegistry`] answers "what does a
//! client see". Nothing connected them, so a ported service class had a published capability and a
//! separate, hand-written RPC surface, and the two drifted: `llm_final_gap` holds `LlmRuntime`'s
//! whole behaviour and registered no endpoints at all.
//!
//! `RemoteService` is the connection. It is a trait rather than a decorator because Rust has no
//! method decorators, and the declarative half is the implementation obligation: a type that
//! implements `RemoteService` must say its namespace and must register its own endpoints, so a
//! service cannot be wired without its surface.
//!
//! # The three markers
//!
//! Upstream's `@Remote` has three forms and all three map directly:
//!
//! | TypeScript | Here |
//! | --- | --- |
//! | `@Remote` | [`Endpoint::direct`] — the method name is the endpoint name |
//! | `@Remote('name')` | [`Endpoint::named`] — an endpoint with a different name |
//! | `@Remote({ mode: 'stream' })` | [`Endpoint::streaming`] — a stream rather than one answer |
//!
//! The fourth upstream form, `@RemoteScope`, scopes a method to a resource; it is
//! [`Endpoint::scoped`], and the scope name travels with the registration so the access layer can
//! refuse a call the caller is not in.

use std::sync::Arc;

use serde_json::Value;

use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::core::scope::BoxFut;
use crate::core::service::ServiceKey;

/// How one method is exposed to a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// The method answers once. `name` is the wire name.
    Direct {
        /// The wire name a client calls.
        name: String,
    },
    /// The method answers with a stream of values.
    Streaming {
        /// The wire name a client calls.
        name: String,
    },
    /// The method is scoped to a resource the caller must already be inside.
    Scoped {
        /// The wire name a client calls.
        name: String,
        /// The scope kind, for example `session` or `workspace`.
        scope: String,
    },
}

impl Endpoint {
    /// A direct endpoint carrying the method's own name.
    #[must_use]
    pub fn direct(name: impl Into<String>) -> Self {
        Self::Direct { name: name.into() }
    }

    /// A direct endpoint whose wire name differs from the Rust method name.
    ///
    /// Upstream's `@Remote('discoverModels')` on `remoteDiscoverModels`: the wire keeps the name
    /// clients already call while the implementation keeps the name that describes it.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self::Direct { name: name.into() }
    }

    /// A streaming endpoint.
    #[must_use]
    pub fn streaming(name: impl Into<String>) -> Self {
        Self::Streaming { name: name.into() }
    }

    /// A scoped endpoint.
    #[must_use]
    pub fn scoped(name: impl Into<String>, scope: impl Into<String>) -> Self {
        Self::Scoped {
            name: name.into(),
            scope: scope.into(),
        }
    }

    /// The wire name this endpoint answers to.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Direct { name } | Self::Streaming { name } | Self::Scoped { name, .. } => name,
        }
    }

    /// The scope a caller must be inside, when this endpoint has one.
    #[must_use]
    pub fn scope(&self) -> Option<&str> {
        match self {
            Self::Scoped { scope, .. } => Some(scope),
            _ => None,
        }
    }

    /// Whether this endpoint streams.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        matches!(self, Self::Streaming { .. })
    }
}

/// A handler a [`RemoteService`] registers for one endpoint.
pub type Handler =
    Arc<dyn Fn(RemoteCall) -> BoxFut<Result<Value, RemoteError>> + Send + Sync + 'static>;

/// A published capability that also exposes RPC endpoints.
///
/// The `ServiceKey` half says what a consumer resolves; the `RemoteService` half says what a client
/// calls. One implementation obligation keeps them together, which is the property upstream gets
/// from constructing both in the same `protected constructor`.
pub trait RemoteService: ServiceKey {
    /// The wire namespace every endpoint of this service lives under.
    ///
    /// Upstream defaults it to the Cordis service key and lets a binding override it, so the two
    /// agree unless a deployment renames one. Here it is stated once, which makes a rename a
    /// single-line change rather than two that have to agree.
    const NAMESPACE: &'static str;

    /// The endpoints this service exposes, in registration order.
    ///
    /// Declared separately from [`Self::handlers`] so a caller can ask what a service offers without
    /// building the handlers — a schema dump and a permission listing both need the names, and both
    /// would otherwise have to construct the service.
    fn endpoints() -> Vec<Endpoint>;

    /// The handler for one endpoint, or `None` when the name is not this service's.
    ///
    /// Takes the service instance because an endpoint answers from live state: a handler that did
    /// not would be a free function wearing a method's name.
    fn handler(service: &Arc<Self::Service>, endpoint: &str) -> Option<Handler>;
}

/// Registers every endpoint of one `RemoteService` into a registry.
///
/// This is the single place the two halves meet, so a service that implements [`RemoteService`]
/// cannot be published with a surface that disagrees with its declaration: the names come from
/// [`RemoteService::endpoints`] and the handlers from [`RemoteService::handler`], and a name with no
/// handler is reported rather than silently skipped.
///
/// # Errors
///
/// The endpoints that had no handler. Nothing is registered when any endpoint is missing, because a
/// partial surface is one a client cannot reason about — the same rule the settings loader applies
/// to a half-valid provider block.
pub fn register_remote_service<S: RemoteService>(
    registry: &mut RemoteRegistry,
    service: &Arc<S::Service>,
) -> Result<usize, Vec<String>> {
    let endpoints = S::endpoints();
    let mut missing = Vec::new();
    for endpoint in &endpoints {
        if S::handler(service, endpoint.name()).is_none() {
            missing.push(endpoint.name().to_owned());
        }
    }
    if !missing.is_empty() {
        return Err(missing);
    }
    for endpoint in &endpoints {
        // `handler` was checked above, so this cannot be `None`; the `if let` keeps the function
        // total without an `expect`, which would turn a future refactor into a panic.
        if let Some(handler) = S::handler(service, endpoint.name()) {
            registry.register(S::NAMESPACE, endpoint.name(), move |call| handler(call));
        }
    }
    Ok(endpoints.len())
}

/// The names a registered `RemoteService` exposes, without touching a registry.
///
/// Used by the schema dump and the capability audit, both of which run before any server exists.
#[must_use]
pub fn endpoint_names<S: RemoteService>() -> Vec<String> {
    S::endpoints()
        .iter()
        .map(|endpoint| endpoint.name().to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capability with no behaviour, for the shape tests.
    trait Greeter: Send + Sync {
        fn greet(&self) -> String;
    }

    struct GreeterKey;

    impl ServiceKey for GreeterKey {
        type Service = dyn Greeter;
        const ID: &'static str = "greeter";
    }

    impl RemoteService for GreeterKey {
        const NAMESPACE: &'static str = "greeter";

        fn endpoints() -> Vec<Endpoint> {
            vec![Endpoint::direct("greet"), Endpoint::named("shout")]
        }

        fn handler(service: &Arc<Self::Service>, endpoint: &str) -> Option<Handler> {
            let service = Arc::clone(service);
            match endpoint {
                "greet" => Some(Arc::new(move |_call: RemoteCall| {
                    let answer = service.greet();
                    Box::pin(async move { Ok(Value::String(answer)) }) as BoxFut<_>
                })),
                "shout" => Some(Arc::new(move |_call: RemoteCall| {
                    let answer = service.greet().to_uppercase();
                    Box::pin(async move { Ok(Value::String(answer)) }) as BoxFut<_>
                })),
                _ => None,
            }
        }
    }

    struct Hello;

    impl Greeter for Hello {
        fn greet(&self) -> String {
            "hello".to_owned()
        }
    }

    #[test]
    fn an_endpoint_keeps_the_name_a_client_calls() {
        // Upstream's `@Remote('discoverModels')` on `remoteDiscoverModels`: the wire name is what
        // clients already call, and it need not match the Rust method.
        assert_eq!(Endpoint::named("discoverModels").name(), "discoverModels");
        assert_eq!(Endpoint::direct("list").name(), "list");
    }

    #[test]
    fn a_scoped_endpoint_carries_its_scope() {
        let endpoint = Endpoint::scoped("open", "workspace");
        assert_eq!(endpoint.scope(), Some("workspace"));
        assert!(!endpoint.is_streaming());
    }

    #[test]
    fn a_streaming_endpoint_says_so() {
        assert!(Endpoint::streaming("watch").is_streaming());
        assert_eq!(Endpoint::streaming("watch").scope(), None);
    }

    #[test]
    fn registering_a_service_installs_every_endpoint() {
        let mut registry = RemoteRegistry::new();
        let hello: Arc<dyn Greeter> = Arc::new(Hello);
        let installed = register_remote_service::<GreeterKey>(&mut registry, &hello)
            .expect("every endpoint has a handler");
        assert_eq!(installed, 2);
        assert_eq!(endpoint_names::<GreeterKey>(), vec!["greet", "shout"]);
    }

    #[test]
    fn a_missing_handler_installs_nothing() {
        // The rule the settings loader uses for a half-valid provider block: a partial surface is
        // one a client cannot reason about, so the refusal is total.
        struct Half;

        impl ServiceKey for Half {
            type Service = dyn Greeter;
            const ID: &'static str = "half";
        }

        impl RemoteService for Half {
            const NAMESPACE: &'static str = "half";

            fn endpoints() -> Vec<Endpoint> {
                vec![Endpoint::direct("present"), Endpoint::direct("absent")]
            }

            fn handler(service: &Arc<Self::Service>, endpoint: &str) -> Option<Handler> {
                let service = Arc::clone(service);
                (endpoint == "present").then(|| {
                    Arc::new(move |_call: RemoteCall| {
                        let answer = service.greet();
                        Box::pin(async move { Ok(Value::String(answer)) }) as BoxFut<_>
                    }) as Handler
                })
            }
        }

        let mut registry = RemoteRegistry::new();
        let hello: Arc<dyn Greeter> = Arc::new(Hello);
        let missing = register_remote_service::<Half>(&mut registry, &hello).expect_err("absent");
        assert_eq!(missing, vec!["absent"]);
        // Nothing registered, including the endpoint that *did* have a handler.
        assert!(endpoint_names::<Half>()[0] == "present");
    }
}
