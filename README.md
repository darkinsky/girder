# Girder

A Rust rewrite of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) (`dsh`), the
open-source agent harness from DeepSeek AI.

A girder is the main beam that carries a structure: it holds the parts up without being any of
them. That is the job of a harness — accept contributions from plugins, resolve them for
consumers, and stay out of the way.

> **Status: early development.** Only the plugin registry exists so far. Nothing here is usable as
> an agent yet, and the API will change without notice.

## Design

The upstream harness is built on *everything is a plugin*: every capability — the model client, the
shell executor, the session store — is contributed by a plugin, and plugins reach each other by
resolving services from a shared context instead of importing concrete implementations.

`girder-core` is the seed of that model:

- **`Service`** — a capability a plugin publishes under a stable string id such as `llm.provider`.
  Ids are strings rather than type ids so separately compiled crates agree on the key.
- **`Plugin`** — a unit of contribution, applied once at startup.
- **`Registry`** — owns the applied plugin set and resolves published services. A second provider
  claiming the same id is a registration error, never a silent replacement.

## Quick start

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

```rust
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
    Ok(())
}
```

## Layout

```
crates/
  girder-core/   service ids, plugin lifecycle, registry
```

## Roadmap

- [x] Plugin registry and typed service resolution
- [ ] Service lifecycle and dependency-ordered teardown
- [ ] LLM provider seam
- [ ] Shell execution seam
- [ ] Session persistence
- [ ] Agent loop
- [ ] CLI and Web front ends

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
