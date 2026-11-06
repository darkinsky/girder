//! The plugin unit of contribution.

use std::sync::Arc;

use crate::registry::RegistryInner;
use crate::service::Service;

/// A failure raised while a plugin was being applied.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The plugin tried to register a service id that already has a provider.
    #[error("service `{id}` is already registered by `{existing}`; `{plugin}` cannot claim it")]
    DuplicateService {
        /// The contested service id.
        id: String,
        /// The plugin that already owns the id.
        existing: String,
        /// The plugin attempting the duplicate registration.
        plugin: String,
    },
}

/// The handle a plugin uses to contribute to the harness.
///
/// The context is the only channel through which a plugin may reach shared state. Everything a
/// plugin registers is recorded here, so the registry can dispose of it later without relying on
/// the plugin to implement cleanup.
pub struct PluginContext {
    /// Name of the plugin currently being applied, used in diagnostics.
    plugin: &'static str,
    pub(crate) inner: Arc<RegistryInner>,
}

impl PluginContext {
    pub(crate) fn new(plugin: &'static str, inner: Arc<RegistryInner>) -> Self {
        Self { plugin, inner }
    }

    /// Publishes a service under its declared [`Service::id`].
    ///
    /// The registration is disposed automatically when the registry shuts down; a plugin must
    /// not try to unregister manually.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::DuplicateService`] when another plugin already supplies the same
    /// id. Claiming an id silently would make resolution order-dependent, so the conflict is
    /// reported to the plugin that lost the race.
    pub fn register<S: Service + 'static>(&self, service: S) -> Result<(), PluginError> {
        let id = service.id();
        let mut services = self.inner.lock();
        if let Some((existing, _)) = services.get_key_value(&id) {
            return Err(PluginError::DuplicateService {
                id: id.to_string(),
                existing: existing.to_string(),
                plugin: self.plugin.to_owned(),
            });
        }
        services.insert(id, Arc::new(service));
        Ok(())
    }

    /// Returns the name of the plugin being applied.
    #[must_use]
    pub fn plugin(&self) -> &'static str {
        self.plugin
    }
}

/// A unit of contribution to the harness.
///
/// A plugin is applied once, at startup, and is not removed while the harness runs. It declares
/// its name for diagnostics and contributes services through the [`PluginContext`].
pub trait Plugin: Send + Sync {
    /// A stable, lowercase name used in logs and duplicate-registration errors.
    fn name(&self) -> &'static str;

    /// Contributes this plugin's services.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when a contribution conflicts with an already-applied plugin.
    /// Registration performs no rollback: a plugin that fails midway leaves its earlier services
    /// in place, and the registry is considered failed and must be discarded, not reused.
    fn apply(&self, ctx: &PluginContext) -> Result<(), PluginError>;
}
