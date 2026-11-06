//! The plan's acceptance example: register one service, resolve it back, run teardown.
//!
//! Run with `cargo run -p girder-core --example greeter`.

use std::sync::Arc;

use girder_core::{BoxFut, Registry, ServiceKey};

/// The contract a plugin publishes and consumers depend on.
trait Greeter: Send + Sync {
    /// Greets `name`.
    fn greet(&self, name: &str) -> String;
}

/// The key consumers type-check against. `ID` is what the registry stores under.
struct GreeterKey;

impl ServiceKey for GreeterKey {
    type Service = dyn Greeter;
    const ID: &'static str = "greeter";
}

/// One provider of [`Greeter`].
struct English;

impl Greeter for English {
    fn greet(&self, name: &str) -> String {
        format!("hello, {name}")
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let registry = Registry::new();

    // A plugin publishes through a context tagged with its own name, so conflicts are attributable.
    let plugin = registry.context("greeter-plugin");
    plugin
        .register::<GreeterKey>(Arc::new(English))
        .expect("no conflict");

    // Teardown is registered with the contribution, not implemented by the plugin.
    plugin.effect("greeter shutdown", || -> BoxFut<()> {
        Box::pin(async {
            println!("  [teardown] greeter released");
        })
    });

    // A consumer sees only the key and the trait.
    let greeter: Arc<dyn Greeter> = registry.get::<GreeterKey>().expect("greeter is published");
    println!("{}", greeter.greet("girder"));

    registry.dispose().await;
    println!("disposed; {} services remain", registry.len());
}
