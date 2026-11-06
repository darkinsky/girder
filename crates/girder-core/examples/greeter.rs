//! Minimal plugin that publishes a service and resolves it back.
//!
//! Mirrors the example in the repository README; keep the two in sync.

use std::sync::Arc;

use girder_core::{Plugin, PluginContext, PluginError, Registry, Service, ServiceId};

struct Greeter;

impl Service for Greeter {
    fn id(&self) -> ServiceId {
        ServiceId::new("greeter")
    }
}

struct GreeterPlugin;

impl Plugin for GreeterPlugin {
    fn name(&self) -> &'static str {
        "greeter"
    }

    fn apply(&self, ctx: &PluginContext) -> Result<(), PluginError> {
        ctx.register(Greeter)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = Registry::new();
    registry.apply(Arc::new(GreeterPlugin))?;

    let service = registry.service(&ServiceId::new("greeter"))?;
    assert!(service.downcast_ref::<Greeter>().is_some());
    println!(
        "resolved `greeter` from {} plugin(s)",
        registry.plugin_count()
    );
    Ok(())
}
