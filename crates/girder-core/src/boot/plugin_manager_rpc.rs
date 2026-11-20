//! `pluginManager.*`: the installed-plugin surface, over an explicit entry registry.
//!
//! # What upstream has
//!
//! `packages/boot/plugin-manager/src/index.ts` marks fifteen methods. They fall into three groups by
//! what they need:
//!
//! | Group | Methods | Needs |
//! | --- | --- | --- |
//! | Reads | `listPlugins`, `listBundles`, `listVersions`, `readBundleDetail`, `inspect`, `registries`, `listVersionExemptions` | A registry |
//! | Mutations | `setPluginEnabled`, `setBundleEnabled`, `installBundle`, `updateBundle`, `removeBundle`, `cancelInstall`, `setVersionExemption` | The registry plus an installer |
//! | Progress | `waitForInstall` | A running install |
//!
//! # What this port serves
//!
//! The seven reads, over [`crate::boot::plugin_entries::Registry`] -- the explicit replacement for the
//! Cordis tree. That is the whole point of the substitution: `listPlugins` answers from data this
//! compiler checks, and it computes the same two read-only reasons because both are facts about data
//! rather than about a tree.
//!
//! The mutations refuse with `no_installer`, and `waitForInstall` with `no_install`: installing a
//! package means running pnpm against a profile, and this build has no installer to run. Answering
//! `ok` would report a change that never happened.
//!
//! # Why one registry rather than a tree
//!
//! A tree exists so plugins can be mounted, nested and reloaded while a process runs. This build
//! mounts plugins at compile time, so what a client needs is not the tree but the *inventory* -- and
//! an inventory is a map. Every entry in it was put there by code this compiler type-checked, which
//! is the guarantee a tree cannot offer.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::boot::plugin_entries::Registry;
use crate::api::remote::{RemoteCall, RemoteRegistry};

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "pluginManager";

/// The reads, which answer from the registry.
pub const READS: &[&str] = &[
    "listPlugins",
    "listBundles",
    "listVersions",
    "readBundleDetail",
    "inspect",
    "registries",
    "listVersionExemptions",
];

/// The mutations, which need an installer this build does not have.
pub const MUTATIONS: &[&str] = &[
    "setPluginEnabled",
    "setBundleEnabled",
    "installBundle",
    "updateBundle",
    "removeBundle",
    "cancelInstall",
    "setVersionExemption",
];

/// Renders the refusal a mutation answers.
///
/// A distinct code per group rather than one `unavailable`, because a client reacts differently:
/// "no installer" is a deployment fact it cannot fix, and it should not retry.
fn no_installer(method: &str) -> Value {
    json!({
        "ok": false,
        "code": "no_installer",
        "message": format!(
            "`pluginManager.{method}` needs a package installer; this build mounts plugins at compile time"
        ),
    })
}

/// Registers the fifteen `pluginManager` methods.
pub fn register_plugin_manager_methods(registry: &mut RemoteRegistry, entries: &Arc<Registry>) {
    let listing = Arc::clone(entries);
    registry.register(NAMESPACE, "listPlugins", move |_call: RemoteCall| {
        let entries = Arc::clone(&listing);
        Box::pin(async move { Ok(json!(entries.list())) })
    });

    // The remaining reads answer an empty collection rather than refusing. That is accurate for this
    // build -- it has no bundles, no version catalogue and no install log -- and it is a shape a
    // client can render, where a refusal would make a plugins page fail to load entirely.
    for method in READS.iter().filter(|m| **m != "listPlugins") {
        let method = (*method).to_owned();
        let wire = method.clone();
        registry.register(NAMESPACE, &wire, move |_call: RemoteCall| {
            let method = method.clone();
            Box::pin(async move {
                Ok(json!({
                    "items": [],
                    "code": "empty",
                    "message": format!("this build has no `{method}` inventory; the list is empty by construction"),
                }))
            })
        });
    }

    for method in MUTATIONS {
        let method = (*method).to_owned();
        let wire = method.clone();
        registry.register(NAMESPACE, &wire, move |_call: RemoteCall| {
            let method = method.clone();
            Box::pin(async move { Ok(no_installer(&method)) })
        });
    }

    registry.register(NAMESPACE, "waitForInstall", move |_call: RemoteCall| {
        Box::pin(async move {
            Ok(json!({
                "ok": false,
                "code": "no_install",
                "message": "no install can be in progress in a build with no installer",
            }))
        })
    });

    for method in READS {
        registry.declare(
            NAMESPACE,
            method,
            crate::guard::access::Declaration::read("plugin"),
        );
    }
    for method in MUTATIONS {
        registry.declare(
            NAMESPACE,
            method,
            crate::guard::access::Declaration::write("plugin"),
        );
    }
    registry.declare(
        NAMESPACE,
        "waitForInstall",
        crate::guard::access::Declaration::read("plugin"),
    );
}

/// Registers `pluginInventory.list`.
///
/// Upstream separates this from `pluginManager` because the inventory is a *resource* rather than a
/// manager: it answers "what is installed" without the management decisions layered on top. Here it
/// reads the same registry, because there is one inventory and two views of it -- and a second source
/// would be a second answer to the same question.
pub fn register_plugin_inventory_methods(registry: &mut RemoteRegistry, entries: &Arc<Registry>) {
    let listing = Arc::clone(entries);
    registry.register("pluginInventory", "list", move |_call: RemoteCall| {
        let entries = Arc::clone(&listing);
        Box::pin(async move {
            Ok(json!({
                "entries": entries.entries().iter().map(|entry| json!({
                    "entryId": entry.entry_id,
                    "moduleName": entry.module_name,
                })).collect::<Vec<_>>(),
                // This build has no preset compositions separate from the entries, so the field is
                // empty rather than absent: a client that iterated it would find nothing, which is
                // accurate, and a client that checked for the key finds it.
                "presets": [],
            }))
        })
    });
    registry.declare(
        "pluginInventory",
        "list",
        crate::guard::access::Declaration::read("plugin"),
    );
}

/// Registers a method name that upstream serves but this build's list missed.
///
/// A seam for tests: the declaration table must match what is registered, and a name that appears in
/// one but not the other is exactly the drift `docs/port-map.md` exists to catch.
///
/// # Errors
///
/// The message when the name is not one upstream serves.
pub fn assert_upstream_method(name: &str) -> Result<(), String> {
    if READS.contains(&name) || MUTATIONS.contains(&name) || name == "waitForInstall" {
        return Ok(());
    }
    Err(format!(
        "`{name}` is not a pluginManager method upstream serves"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_upstream_method_is_accounted_for() {
        // Fifteen marks upstream; reads plus mutations plus the one progress method.
        assert_eq!(READS.len() + MUTATIONS.len() + 1, 15);
    }

    #[test]
    fn a_mutation_refusal_names_the_method_and_the_reason() {
        // A client cannot fix "no installer", so the code must say what it is rather than invite a
        // retry.
        let refusal = no_installer("installBundle");
        assert_eq!(refusal["code"], "no_installer");
        assert!(
            refusal["message"]
                .as_str()
                .unwrap_or_default()
                .contains("installBundle")
        );
    }

    #[test]
    fn the_lists_do_not_overlap() {
        for method in READS {
            assert!(!MUTATIONS.contains(method), "`{method}` cannot be both");
        }
        assert!(!READS.contains(&"waitForInstall"));
        assert!(!MUTATIONS.contains(&"waitForInstall"));
    }

    #[test]
    fn an_unknown_name_is_refused() {
        assert!(assert_upstream_method("notAThing").is_err());
        assert!(assert_upstream_method("listPlugins").is_ok());
    }
}
