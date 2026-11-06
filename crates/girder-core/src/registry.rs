//! The service table and its registration rules.
//!
//! A [`Registry`] owns one table of published services and hands out [`RegistryContext`] handles
//! that plugins use to add to it. The handle is what a plugin is given instead of `ctx`; it can
//! publish services and register effects, and it remembers which plugin it belongs to so that
//! conflicts name both sides.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::scope::Scope;
use crate::service::{Entry, ServiceKey};

/// A failure raised while publishing or resolving a service.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// Two plugins claimed the same service id.
    ///
    /// The error names both plugins because the fix depends on which one is wrong, and the
    /// registry has no way to know.
    #[error(
        "service `{id}` is already registered by plugin `{existing}`; plugin `{plugin}` cannot claim it"
    )]
    Duplicate {
        /// The contested service id.
        id: &'static str,
        /// The plugin that already owns the id.
        existing: &'static str,
        /// The plugin attempting the registration.
        plugin: &'static str,
    },

    /// No plugin publishes the requested service.
    #[error("no plugin publishes service `{id}`")]
    Missing {
        /// The requested service id.
        id: &'static str,
    },
}

/// Published services, keyed by their [`ServiceKey::ID`].
pub(crate) type ServiceTable = BTreeMap<&'static str, Entry>;

/// State shared by the registry and every context handle it hands out.
#[derive(Default)]
pub(crate) struct RegistryInner {
    pub(crate) services: Mutex<ServiceTable>,
    pub(crate) scope: Scope,
}

impl RegistryInner {
    /// Locks the service table, recovering from poisoning.
    ///
    /// Poisoning means a plugin panicked while holding the lock. The table only owns `Arc`s and
    /// `&'static str`s, so it is still structurally sound and remaining entries stay valid.
    pub(crate) fn lock(&self) -> MutexGuard<'_, ServiceTable> {
        self.services.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Owns the published service set.
#[derive(Default)]
pub struct Registry {
    inner: Arc<RegistryInner>,
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a handle for the named plugin to publish through.
    ///
    /// The plugin name is embedded in every error the handle produces. It is advisory — the
    /// registry does not verify that the caller really is that plugin — but it is what makes a
    /// duplicate-registration error actionable.
    #[must_use]
    pub fn context(&self, plugin: &'static str) -> RegistryContext {
        RegistryContext {
            plugin,
            inner: Arc::clone(&self.inner),
        }
    }

    /// Resolves a service that a plugin has published.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Missing`] when no plugin publishes `K::ID`.
    pub fn get<K: ServiceKey>(&self) -> Result<Arc<K::Service>, RegistryError> {
        let services = self.inner.lock();
        let entry = services
            .get(K::ID)
            .ok_or(RegistryError::Missing { id: K::ID })?;
        entry.get::<K>().ok_or(RegistryError::Missing { id: K::ID })
    }

    /// Returns true when some plugin publishes `K::ID`.
    #[must_use]
    pub fn contains<K: ServiceKey>(&self) -> bool {
        self.inner.lock().contains_key(K::ID)
    }

    /// Number of published services.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Returns true when no service has been published.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Runs the disposers of every effect registered through this registry, in reverse order.
    ///
    /// Services are removed only after the disposers run, so a disposer can still resolve the
    /// services it was published alongside — the order Node's fiber uses, where a plugin's own
    /// teardown sees the dependency graph intact.
    pub async fn dispose(&self) {
        self.inner.scope.dispose().await;
        self.inner.lock().clear();
    }
}

/// A plugin's handle to the registry.
///
/// Obtained from [`Registry::context`]. Every method records its contribution in the registry's
/// [`Scope`], so cleanup is the registry's job, not the plugin's.
pub struct RegistryContext {
    plugin: &'static str,
    inner: Arc<RegistryInner>,
}

impl RegistryContext {
    /// The plugin name this handle reports.
    #[must_use]
    pub fn plugin(&self) -> &'static str {
        self.plugin
    }

    /// Publishes a service under `K::ID`.
    ///
    /// The service is passed as an `Arc` rather than by value: the registry shares it out
    /// immediately, and a `dyn Trait` service could not cross the boundary by value anyway. Call
    /// sites still publish concrete values — `ctx.register::<GreeterKey>(Arc::new(English))` — because
    /// `Arc<T>` coerces to `Arc<dyn Trait>` at the call, where both types are known.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Duplicate`] when another plugin already publishes `K::ID`. The id
    /// is not silently replaced: replacement would make resolution depend on plugin apply order,
    /// and upstream observed that bug whenever two plugins claimed the same name.
    pub fn register<K: ServiceKey>(&self, service: Arc<K::Service>) -> Result<(), RegistryError> {
        let mut services = self.inner.lock();
        if let Some(existing) = services.get(K::ID) {
            return Err(RegistryError::Duplicate {
                id: K::ID,
                existing: existing.plugin,
                plugin: self.plugin,
            });
        }
        services.insert(K::ID, Entry::new(self.plugin, service));
        Ok(())
    }

    /// Registers cleanup that runs when the registry is disposed.
    ///
    /// Use this for anything a plugin acquires — listeners, tasks, file handles — so teardown does
    /// not depend on the plugin implementing a trait method correctly.
    pub fn effect<F>(&self, label: &'static str, dispose: F)
    where
        F: FnOnce() -> crate::scope::BoxFut<()> + Send + 'static,
    {
        self.inner.scope.effect(label, dispose);
    }

    /// Resolves a service this or another plugin published.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Missing`] when no plugin publishes `K::ID`.
    pub fn get<K: ServiceKey>(&self) -> Result<Arc<K::Service>, RegistryError> {
        let services = self.inner.lock();
        let entry = services
            .get(K::ID)
            .ok_or(RegistryError::Missing { id: K::ID })?;
        entry.get::<K>().ok_or(RegistryError::Missing { id: K::ID })
    }
}
