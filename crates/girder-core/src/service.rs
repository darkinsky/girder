//! Typed service keys and the type-erased service table.
//!
//! Upstream Cordis resolves services through a `Proxy` on `ctx`: reading `ctx.tools` looks the
//! name up in a table and returns whatever object was published under it, with no check that the
//! consumer and the provider agree on the type. Rust has no such proxy, and the plan discards
//! dynamic property reads entirely (see migration-plan §1.2 第三层). The replacement is a key
//! that carries its service type:
//!
//! ```
//! use std::sync::Arc;
//!
//! use girder_core::ServiceKey;
//!
//! pub trait Greeter: Send + Sync {
//!     fn greet(&self) -> String;
//! }
//!
//! pub struct GreeterKey;
//!
//! impl ServiceKey for GreeterKey {
//!     type Service = dyn Greeter;
//!     const ID: &'static str = "greeter";
//! }
//! ```
//!
//! A key may name an unsized service (`dyn Trait`), so a provider and a consumer can be compiled
//! against the trait alone. Storage still has to be `Sized` because the table is type-erased to
//! `Arc<dyn Any>`; publishing therefore boxes the `Arc` itself, and resolution unwraps it. The
//! cost is one extra pointer indirection per lookup, paid once when a consumer resolves.

use std::any::Any;
use std::sync::Arc;

/// A statically named capability that a plugin publishes and other plugins consume.
///
/// The associated type is the contract: a consumer that resolves [`GreeterKey`] gets exactly
/// `Arc<dyn Greeter>`, so a provider cannot accidentally publish an unrelated type under the same
/// id. Two implementations of [`ServiceKey`] with the same [`ID`](ServiceKey::ID) are *not*
/// interchangeable; the id exists for diagnostics and for runtime replacement, and the registry
/// rejects a second provider of the same id.
pub trait ServiceKey {
    /// The service contract, usually a `dyn Trait` so providers stay swappable.
    type Service: ?Sized + Send + Sync + 'static;

    /// Stable, lowercase identifier used in errors and, later, in config.
    const ID: &'static str;
}

/// Wraps an `Arc` in something `Sized` so it can live in an `Arc<dyn Any>`.
///
/// `Arc<dyn Trait>` cannot be coerced to `Arc<dyn Any>`: unsizing only removes the outer pointer's
/// metadata, and trait upcasting between unrelated traits is not allowed. Holding the `Arc` in a
/// concrete struct sidesteps that — `ArcHolder<S>` is `Sized` for every `S`, including `dyn Trait`.
struct ArcHolder<S: ?Sized>(Arc<S>);

/// A published service, detached from its type until a consumer claims it.
pub(crate) type ServiceObject = Arc<dyn Any + Send + Sync>;

/// One row of the table: the service plus the plugin that owns it, for diagnostics.
pub(crate) struct Entry {
    /// Name of the plugin that published the service, reported in conflict errors.
    pub(crate) plugin: &'static str,
    object: ServiceObject,
}

impl Entry {
    /// Publishes `service` on behalf of `plugin`.
    pub(crate) fn new<S>(plugin: &'static str, service: Arc<S>) -> Self
    where
        S: ?Sized + Send + Sync + 'static,
    {
        Self {
            plugin,
            object: Arc::new(ArcHolder(service)),
        }
    }

    /// Recovers the typed service.
    ///
    /// Returns `None` only when the table row was written by a key with a different associated
    /// type — a case the registry cannot construct, since the only writer is
    /// [`Entry::new`] called through a concrete `K`.
    pub(crate) fn get<K: ServiceKey>(&self) -> Option<Arc<K::Service>> {
        let holder = self
            .object
            .clone()
            .downcast::<ArcHolder<K::Service>>()
            .ok()?;
        Some(Arc::clone(&holder.0))
    }
}
