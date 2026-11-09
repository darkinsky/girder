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
/// A published service's key: its id plus the isolation label it was published under.
///
/// Cordis isolates by shadowing a name in a child context, so the same name can resolve to
/// different implementations in different subtrees. A flat table keyed by id cannot express that;
/// keying by `(id, label)` can, and a lookup with the default label finds the un-isolated entry.
pub(crate) type ServiceTable = BTreeMap<(&'static str, Label), Entry>;

/// An isolation label.
///
/// `0` is the root label: a service published and read without isolation. Any other value comes
/// from [`Registry::isolate`], and two isolated subtrees that share a label share implementations —
/// which is what makes `isolate` useful for a fleet of agents that all want their own `session` but
/// a shared `llm`.
pub type Label = u64;

/// State shared by the registry and every context handle it hands out.
pub(crate) struct RegistryInner {
    pub(crate) services: Mutex<ServiceTable>,
    pub(crate) scope: Scope,
    /// Next label to hand out.
    ///
    /// Starts at 1 because 0 is the root label: a service with no isolation must not collide with
    /// the first isolated subtree.
    pub(crate) next_label: std::sync::atomic::AtomicU64,
}

impl Default for RegistryInner {
    fn default() -> Self {
        Self {
            services: Mutex::new(ServiceTable::new()),
            scope: Scope::default(),
            next_label: std::sync::atomic::AtomicU64::new(1),
        }
    }
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
            isolated: Arc::new(BTreeMap::new()),
        }
    }

    /// Returns a child handle that resolves `id` against its own label.
    ///
    /// A service published through the returned handle is invisible to the parent, and a lookup
    /// through it never sees the parent's provider. Two children that pass the same `label` share
    /// implementations, which is how a group of agents can each own a `session` while sharing one
    /// `llm`.
    ///
    /// The id is a `&'static str` rather than a `ServiceKey` because isolation is decided per name,
    /// not per type: upstream isolates `'llm'`, and a key type would force a generic that the
    /// config layer cannot supply.
    #[must_use]
    pub fn isolate(
        &self,
        context: &RegistryContext,
        id: &'static str,
        label: Option<Label>,
    ) -> RegistryContext {
        let label = label.unwrap_or_else(|| {
            self.inner
                .next_label
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        });
        let mut isolated = (*context.isolated).clone();
        isolated.insert(id, label);
        RegistryContext {
            plugin: context.plugin,
            inner: Arc::clone(&self.inner),
            isolated: Arc::new(isolated),
        }
    }

    /// Resolves a service at the root label.
    pub fn get_at<K: ServiceKey>(&self, label: Label) -> Result<Arc<K::Service>, RegistryError> {
        let services = self.inner.lock();
        let entry = services
            .get(&(K::ID, label))
            .ok_or(RegistryError::Missing { id: K::ID })?;
        entry.get::<K>().ok_or(RegistryError::Missing { id: K::ID })
    }

    /// Resolves a service that a plugin has published.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Missing`] when no plugin publishes `K::ID`.
    pub fn get<K: ServiceKey>(&self) -> Result<Arc<K::Service>, RegistryError> {
        self.get_at::<K>(0)
    }

    /// Returns true when some plugin publishes `K::ID` at the root label.
    #[must_use]
    pub fn contains<K: ServiceKey>(&self) -> bool {
        self.inner.lock().contains_key(&(K::ID, 0))
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
    /// Isolation labels in effect for this handle, by service id.
    ///
    /// Only ids that were explicitly isolated appear; everything else resolves at the root label.
    isolated: Arc<BTreeMap<&'static str, Label>>,
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
        let key = (K::ID, self.label_for(K::ID));
        let mut services = self.inner.lock();
        if let Some(existing) = services.get(&key) {
            return Err(RegistryError::Duplicate {
                id: K::ID,
                existing: existing.plugin,
                plugin: self.plugin,
            });
        }
        services.insert(key, Entry::new(self.plugin, service));
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
            .get(&(K::ID, self.label_for(K::ID)))
            .ok_or(RegistryError::Missing { id: K::ID })?;
        entry.get::<K>().ok_or(RegistryError::Missing { id: K::ID })
    }

    /// The label this handle resolves `id` at.
    #[must_use]
    pub fn label_for(&self, id: &str) -> Label {
        self.isolated.get(id).copied().unwrap_or(0)
    }
}
