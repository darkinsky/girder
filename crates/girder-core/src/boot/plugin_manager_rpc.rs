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
//! | Enablement | `setPluginEnabled`, `setBundleEnabled` | The registry. A saved plugin switch waits for the next start |
//! | Mutations | `installBundle`, `updateBundle`, `removeBundle`, `cancelInstall` | An installer |
//! | Progress | `waitForInstall` | A running install |
//!
//! # What this port serves
//!
//! The seven reads, over [`crate::boot::plugin_entries::Registry`] -- the explicit replacement for the
//! Cordis tree. That is the whole point of the substitution: `listPlugins` answers from data this
//! compiler checks, and it computes the same two read-only reasons because both are facts about data
//! rather than about a tree.
//!
//! `setPluginEnabled` records the choice and `listPlugins` reports it. `setVersionExemption` writes
//! `compatibility.json` beside that record, and `listVersionExemptions` reads it back. This build
//! cannot reload a plugin in place, so a change that was saved answers `restart-required`.
//! `setBundleEnabled` answers `not-bundle`: there is no bundle catalogue to select. The install
//! mutations refuse with `no_installer`, and `waitForInstall` with `no_install`. Installing a
//! package means running a package manager, and this build has none. Answering `ok` would report a
//! change that never happened.
//!
//! # Why one registry rather than a tree
//!
//! A tree exists so plugins can be mounted, nested and reloaded while a process runs. This build
//! mounts plugins at compile time, so what a client needs is not the tree but the *inventory* -- and
//! an inventory is a map. Every entry in it was put there by code this compiler type-checked, which
//! is the guarantee a tree cannot offer.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteRegistry};
use crate::boot::plugin_entries::Registry;

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

/// Package operations. Enablement is not one of them.
const INSTALLS: &[&str] = &[
    "installBundle",
    "updateBundle",
    "removeBundle",
    "cancelInstall",
];

/// Renders the refusal a mutation answers.
///
/// A distinct code per group rather than one `unavailable`, because a client reacts differently:
/// "no installer" is a deployment fact it cannot fix, and it should not retry.
fn required_text<'a>(
    args: &'a Value,
    field: &str,
    method: &str,
) -> Result<&'a str, crate::api::remote::RemoteError> {
    match args.get(field).and_then(Value::as_str).map(str::trim) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(crate::api::remote::RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: format!("`{field}` is required"),
        }),
    }
}

fn required_bool(
    args: &Value,
    field: &str,
    method: &str,
) -> Result<bool, crate::api::remote::RemoteError> {
    args.get(field).and_then(Value::as_bool).ok_or_else(|| {
        crate::api::remote::RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: format!("`{field}` is required"),
        }
    })
}

fn inspect_spec(entries: &Registry, spec: &str) -> Value {
    let parsed = match crate::boot::install::parse_install_spec(spec) {
        Ok(parsed) => parsed,
        Err(error) => {
            return json!({
                "status": "refused",
                "problem": "invalid-spec",
                "reason": error.reason,
            });
        }
    };
    match parsed {
        crate::boot::install::ParsedSpec::Git { host, .. } => json!({
            "status": "accepted",
            "kind": "git",
            "bundle": null,
            "registry": null,
            "host": host,
        }),
        crate::boot::install::ParsedSpec::Tarball { path, host, .. } => {
            if path
                .as_deref()
                .is_some_and(|path| !std::path::Path::new(path).exists())
            {
                return json!({
                    "status": "refused",
                    "problem": "not-a-package",
                    "reason": "the tarball does not exist",
                });
            }
            let mut answer = json!({
                "status": "accepted",
                "kind": "tarball",
                "bundle": null,
                "registry": null,
            });
            if let Some(host) = host {
                answer["host"] = json!(host);
            }
            answer
        }
        crate::boot::install::ParsedSpec::Path { path, .. } => inspect_path(entries, &path),
        crate::boot::install::ParsedSpec::Registry { name, .. } => {
            if entries
                .entries()
                .iter()
                .any(|entry| entry.module_name == name)
            {
                json!({
                    "status": "refused",
                    "problem": "already-installed",
                    "reason": format!("{name} is already installed"),
                })
            } else {
                json!({
                    "status": "refused",
                    "problem": "network",
                    "reason": "this build does not query a package registry",
                    "registries": [null],
                })
            }
        }
    }
}

fn inspect_path(entries: &Registry, path: &str) -> Value {
    let directory = std::path::Path::new(path);
    if !directory.exists() {
        return json!({
            "status": "refused",
            "problem": "not-a-package",
            "reason": "the path does not exist",
        });
    }
    let manifest = directory.join("package.json");
    if std::fs::metadata(&manifest).is_ok_and(|metadata| metadata.len() > 1_048_576) {
        return json!({
            "status": "refused",
            "problem": "not-a-package",
            "reason": "no readable package.json at the path: the manifest is larger than 1MiB",
        });
    }
    let text = match std::fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(error) => {
            return json!({
                "status": "refused",
                "problem": "not-a-package",
                "reason": format!("no readable package.json at the path: {error}"),
            });
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "status": "refused",
                "problem": "not-a-package",
                "reason": format!("no readable package.json at the path: {error}"),
            });
        }
    };
    let Some(name) = value.get("name").and_then(Value::as_str) else {
        return json!({
            "status": "refused",
            "problem": "not-a-package",
            "reason": "the package.json names no package",
        });
    };
    if entries
        .entries()
        .iter()
        .any(|entry| entry.module_name == name)
    {
        return json!({
            "status": "refused",
            "problem": "already-installed",
            "reason": format!("{name} is already installed"),
        });
    }
    let bundle = value
        .get("dsh")
        .and_then(|dsh| dsh.get("bundle"))
        .and_then(Value::as_object)
        .is_some();
    if !bundle {
        return json!({
            "status": "refused",
            "problem": "not-a-bundle",
            "reason": format!("{name} declares no dsh.bundle"),
        });
    }
    let mut answer = json!({
        "status": "accepted",
        "kind": "path",
        "name": name,
        "bundle": true,
        "registry": null,
    });
    if let Some(version) = value.get("version").and_then(Value::as_str) {
        answer["version"] = json!(version);
    }
    if let Some(description) = value
        .get("description")
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty())
    {
        answer["description"] = json!(description);
    }
    answer
}

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

    let exemptions = Arc::clone(entries);
    registry.register(
        NAMESPACE,
        "listVersionExemptions",
        move |_call: RemoteCall| {
            let entries = Arc::clone(&exemptions);
            Box::pin(async move { Ok(entries.version_exemptions()) })
        },
    );
    // An empty array is the inventory a plugins page iterates. An object would make that page fail
    // while trying to read bundle names.
    registry.register(NAMESPACE, "listBundles", move |_call: RemoteCall| {
        Box::pin(async move { Ok(json!([])) })
    });
    // Nothing is configured, so the first registry is the one the package manager would name itself
    // and no fallback has been read.
    registry.register(NAMESPACE, "registries", move |_call: RemoteCall| {
        Box::pin(async move {
            Ok(json!({
                "registry": null,
                "fallbackRegistries": [],
                "resolved": null,
            }))
        })
    });
    registry.register(NAMESPACE, "listVersions", move |call: RemoteCall| {
        Box::pin(async move {
            let name = required_text(&call.args, "name", "listVersions")?;
            Ok(json!({
                "status": "refused",
                "problem": "not-updatable",
                "reason": format!("{name} is not a third-party package installed in this profile"),
                "attempts": [],
            }))
        })
    });
    registry.register(NAMESPACE, "readBundleDetail", move |call: RemoteCall| {
        Box::pin(async move {
            let name = required_text(&call.args, "name", "readBundleDetail")?;
            let _language = required_text(&call.args, "language", "readBundleDetail")?;
            Err(crate::api::remote::RemoteError::Failed(format!(
                "No such bundle: {name}"
            )))
        })
    });
    let inspecting = Arc::clone(entries);
    registry.register(NAMESPACE, "inspect", move |call: RemoteCall| {
        let entries = Arc::clone(&inspecting);
        Box::pin(async move {
            let spec = required_text(&call.args, "spec", "inspect")?;
            Ok(inspect_spec(&entries, spec))
        })
    });

    let toggling = Arc::clone(entries);
    registry.register(NAMESPACE, "setPluginEnabled", move |call: RemoteCall| {
        let entries = Arc::clone(&toggling);
        Box::pin(async move {
            let id = required_text(&call.args, "id", "setPluginEnabled")?;
            let enabled = required_bool(&call.args, "enabled", "setPluginEnabled")?;
            Ok(entries.set_plugin_enabled(id, enabled))
        })
    });
    let exempting = Arc::clone(entries);
    registry.register(NAMESPACE, "setVersionExemption", move |call: RemoteCall| {
        let entries = Arc::clone(&exempting);
        Box::pin(async move {
            let package_version =
                required_text(&call.args, "packageVersion", "setVersionExemption")?;
            let runtime_version =
                required_text(&call.args, "runtimeVersion", "setVersionExemption")?;
            let enabled = required_bool(&call.args, "enabled", "setVersionExemption")?;
            let accept_risk = match call.args.get("acceptRisk") {
                None => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => {
                    return Err(crate::api::remote::RemoteError::InvalidArguments {
                        namespace: NAMESPACE.to_owned(),
                        method: "setVersionExemption".to_owned(),
                        message: "`acceptRisk` must be a boolean".to_owned(),
                    });
                }
            };
            Ok(entries.set_version_exemption(
                package_version,
                runtime_version,
                enabled,
                accept_risk,
            ))
        })
    });
    registry.register(NAMESPACE, "setBundleEnabled", move |call: RemoteCall| {
        Box::pin(async move {
            let name = required_text(&call.args, "name", "setBundleEnabled")?;
            let enabled = required_bool(&call.args, "enabled", "setBundleEnabled")?;
            Ok(json!({
                "changed": false,
                "application": "failed",
                "stage": "enable",
                "target": name,
                "enabled": enabled,
                "error": { "code": "not-bundle" },
            }))
        })
    });

    for method in INSTALLS {
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
            let listed = entries.list();
            Ok(json!({
                // The manager is registered beside this inventory, which is the fact the plugins
                // page reads before it asks for bundles.
                "managementAvailable": true,
                "entries": listed.iter().map(|entry| json!({
                    "entryId": entry["entryId"],
                    "moduleName": entry["moduleName"],
                    "enabled": entry["enabled"],
                    "fiberPhase": null,
                })).collect::<Vec<_>>(),
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
/// one but not the other is a drift between the declaration and the registration.
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

    #[tokio::test]
    async fn set_plugin_enabled_reports_the_saved_choice() {
        use crate::api::remote::{RemoteCall, RemoteRegistry};
        use crate::boot::plugin_entries::{Entry, INCLUDE_ROOT, ProfileRow, Registry};

        let mut entries = Registry::new();
        entries.insert(Entry {
            entry_id: "extra".to_owned(),
            module_name: "some-plugin".to_owned(),
            options_id: "some-plugin".to_owned(),
        });
        entries.push_row(ProfileRow {
            id: "some-plugin".to_owned(),
            name: "some-plugin".to_owned(),
            parent: INCLUDE_ROOT.to_owned(),
        });
        let entries = Arc::new(entries);
        let mut registry = RemoteRegistry::new();
        register_plugin_manager_methods(&mut registry, &entries);
        let disabled = registry
            .dispatch(
                NAMESPACE,
                "setPluginEnabled",
                RemoteCall {
                    args: json!({"id": "extra", "enabled": false}),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("disabled");
        assert_eq!(disabled["application"], "restart-required");
        assert_eq!(disabled["changed"], true);
        let listed = registry
            .dispatch(
                NAMESPACE,
                "listPlugins",
                RemoteCall {
                    args: json!({}),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("list");
        assert_eq!(listed[0]["enabled"], false);

        let bundle = registry
            .dispatch(
                NAMESPACE,
                "setBundleEnabled",
                RemoteCall {
                    args: json!({"name": "extra", "enabled": true}),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("bundle");
        assert_eq!(bundle["error"]["code"], "not-bundle");
    }

    #[tokio::test]
    async fn the_plugin_page_reads_an_array_of_bundles() {
        use crate::api::remote::{RemoteCall, RemoteRegistry};

        let entries = Arc::new(Registry::new());
        let mut registry = RemoteRegistry::new();
        register_plugin_manager_methods(&mut registry, &entries);
        register_plugin_inventory_methods(&mut registry, &entries);
        let call = || RemoteCall {
            args: json!({}),
            peer: "test".to_owned(),
        };
        let inventory = registry
            .dispatch("pluginInventory", "list", call())
            .await
            .expect("inventory");
        assert_eq!(inventory["managementAvailable"], true);
        assert!(inventory["entries"].as_array().expect("entries").is_empty());
        let bundles = registry
            .dispatch(NAMESPACE, "listBundles", call())
            .await
            .expect("bundles");
        assert!(bundles.as_array().expect("bundle array").is_empty());
        let registries = registry
            .dispatch(NAMESPACE, "registries", call())
            .await
            .expect("registries");
        assert!(registries["registry"].is_null());
        assert!(
            registries["fallbackRegistries"]
                .as_array()
                .expect("fallbacks")
                .is_empty()
        );
        let versions = registry
            .dispatch(
                NAMESPACE,
                "listVersions",
                RemoteCall {
                    args: json!({"name": "some-plugin"}),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("versions");
        assert_eq!(versions["status"], "refused");
        assert_eq!(versions["problem"], "not-updatable");
        let inspected = registry
            .dispatch(
                NAMESPACE,
                "inspect",
                RemoteCall {
                    args: json!({"spec": "https://github.com/a/b.git"}),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("inspect");
        assert_eq!(inspected["status"], "accepted");
        assert_eq!(inspected["kind"], "git");
        assert_eq!(inspected["host"], "github.com");
        let detail = registry
            .dispatch(
                NAMESPACE,
                "readBundleDetail",
                RemoteCall {
                    args: json!({"name": "missing", "language": "en"}),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect_err("missing bundle");
        assert!(
            detail.to_string().contains("No such bundle: missing"),
            "{detail}"
        );
    }
}
