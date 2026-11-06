//! Plugin registry and service resolution for the Girder agent harness.
//!
//! Girder is a Rust rewrite of `DeepSeek Harness`. The upstream TypeScript harness is built on
//! "everything is a plugin": every capability is contributed by a plugin, and plugins obtain
//! each other's capabilities by resolving services from a shared context rather than by
//! importing concrete implementations.
//!
//! This crate is the seed of that model. It provides:
//!
//! - [`Plugin`], the unit of contribution.
//! - [`Registry`], which owns the applied plugin set and resolves the services they publish.
//! - [`Service`], the marker implemented by each capability a plugin publishes.
//!
//! Services are type-erased at the registry and downcast by the consumer that declared them,
//! so a provider can be swapped without recompiling its consumers.

mod plugin;
mod registry;
mod service;

pub use plugin::{Plugin, PluginContext, PluginError};
pub use registry::{Registry, RegistryError};
pub use service::{Service, ServiceId};
