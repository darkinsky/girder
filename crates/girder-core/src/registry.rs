//! Plugin lifecycle and the service table.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::plugin::{Plugin, PluginContext, PluginError};
use crate::service::ServiceId;

/// A published service, type-erased at the table.
pub(crate) type ServiceObject = Arc<dyn Any + Send + Sync>;

/// Services published so far, keyed by id.
pub(crate) type ServiceTable = BTreeMap<ServiceId, ServiceObject>;

/// State shared between the registry and every context it hands out.
pub(crate) struct RegistryInner {
    pub(crate) services: Mutex<ServiceTable>,
}

impl RegistryInner {
    /// Locks the service table.
    ///
    /// A poisoned lock means a plugin panicked while holding it. The table only ever holds owned
    /// `Arc`s, so it is still structurally sound and the surviving entries remain valid; the panic
    /// itself has already been reported to whoever applied that plugin.
    pub(crate) fn lock(&self) -> MutexGuard<'_, ServiceTable> {
        self.services.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A failure raised while assembling or querying the plugin set.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A plugin failed to apply.
    #[error("plugin `{plugin}` failed: {source}")]
    Plugin {
        /// The plugin that failed.
        plugin: &'static str,
        /// The failure it reported.
        source: PluginError,
    },
    /// No applied plugin supplies the requested service.
    #[error("no plugin supplies service `{id}`")]
    Missing {
        /// The requested service id.
        id: String,
    },
}

/// Owns the applied plugin set and the services they published.
pub struct Registry {
    inner: Arc<RegistryInner>,
    plugins: Vec<Arc<dyn Plugin>>,
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                services: Mutex::new(BTreeMap::new()),
            }),
            plugins: Vec::new(),
        }
    }

    /// Applies one plugin.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Plugin`] when the plugin reports a conflict. The registry must not
    /// be used afterwards: contributions applied before the failure are not rolled back.
    pub fn apply(&mut self, plugin: Arc<dyn Plugin>) -> Result<(), RegistryError> {
        let name = plugin.name();
        let ctx = PluginContext::new(name, Arc::clone(&self.inner));
        plugin.apply(&ctx).map_err(|source| RegistryError::Plugin {
            plugin: name,
            source,
        })?;
        self.plugins.push(plugin);
        Ok(())
    }

    /// Resolves a published service by id.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Missing`] when no applied plugin supplies `id`.
    pub fn service(&self, id: &ServiceId) -> Result<ServiceObject, RegistryError> {
        let services = self.inner.lock();
        services
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::Missing { id: id.to_string() })
    }

    /// Returns the number of applied plugins.
    #[must_use]
    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Service, ServiceId};

    struct Greeter;

    impl Service for Greeter {
        fn id(&self) -> ServiceId {
            ServiceId::new("test.greeter")
        }
    }

    struct GreeterPlugin;

    impl Plugin for GreeterPlugin {
        fn name(&self) -> &'static str {
            "test-greeter"
        }

        fn apply(&self, ctx: &PluginContext) -> Result<(), PluginError> {
            ctx.register(Greeter)
        }
    }

    #[test]
    fn applied_plugin_publishes_its_service() {
        let mut registry = Registry::new();
        registry.apply(Arc::new(GreeterPlugin)).unwrap();
        assert_eq!(registry.plugin_count(), 1);
        let service = registry.service(&ServiceId::new("test.greeter")).unwrap();
        assert!(service.downcast_ref::<Greeter>().is_some());
    }

    #[test]
    fn resolving_an_unpublished_service_reports_the_missing_id() {
        let registry = Registry::new();
        let err = registry
            .service(&ServiceId::new("test.absent"))
            .unwrap_err();
        assert_eq!(err.to_string(), "no plugin supplies service `test.absent`");
    }

    #[test]
    fn a_second_provider_for_the_same_id_is_rejected() {
        let mut registry = Registry::new();
        registry.apply(Arc::new(GreeterPlugin)).unwrap();
        let err = registry.apply(Arc::new(GreeterPlugin)).unwrap_err();
        assert!(err.to_string().contains("already registered"), "{err}");
        assert_eq!(registry.plugin_count(), 1);
    }
}
