//! `permissionPresets.catalog` and `permissionPresets.set`.
//!
//! # What upstream has
//!
//! `packages/interaction/permission-presets/src/index.ts` marks one method:
//!
//! ```typescript
//! @Remote('catalog')
//! catalog(): PermissionCatalog {
//!   return {
//!     options: this.names.map(name => this.optionOf(name)),
//!     defaultOptions: Object.keys(this.presets).map(name => this.optionOf(name)),
//!     defaultPreset: this.defaultSettings().defaultPreset,
//!   }
//! }
//! ```
//!
//! Three lists rather than one, and the difference matters to a selector: `options` is everything a
//! person may pick *now*, `defaultOptions` is what this deployment offers as a default for future
//! sessions, and `defaultPreset` is the one already chosen. A UI that showed only `options` would
//! offer a preset as a default that the deployment never intended as one.
//!
//! # What this port has
//!
//! [`crate::credentials::permissions::Catalogue`] already models a built-in set, availability, and ordered
//! entries. This module reads it out; it does not decide anything.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::credentials::permissions::Catalogue;

/// The namespace this method registers under, as upstream spells it.
pub const NAMESPACE: &str = "permissionPresets";

/// The catalog endpoint.
pub const CATALOG_METHOD: &str = "catalog";

/// Renders one entry the way upstream's `PresetOption` is shaped.
///
/// `description` is omitted when a preset has none, matching the spread-of-undefined upstream uses:
/// an empty string would render as a blank line in a selector, which reads as a preset whose
/// explanation is missing rather than one that needs none.
fn option_of(entry: &crate::credentials::permissions::Entry) -> Value {
    let mut option = json!({
        "value": entry.preset.name,
        "name": entry.preset.name,
    });
    if !entry.preset.description.is_empty() {
        option["description"] = json!(entry.preset.description);
    }
    // Availability travels beside the option rather than filtering it out: a selector that hid an
    // unavailable preset could not explain why the knob it implies is off.
    option["available"] = json!(entry.available);
    option
}

/// Registers `permissionPresets.catalog`.
pub fn register_permission_preset_methods(
    registry: &mut RemoteRegistry,
    catalogue: &Arc<Catalogue>,
    default_preset: &str,
    catalog: &Arc<dyn crate::api::remote::SessionCatalog>,
) {
    let catalogue = Arc::clone(catalogue);
    let selecting = Arc::clone(&catalogue);
    let default_preset = default_preset.to_owned();
    registry.register(NAMESPACE, CATALOG_METHOD, move |_call: RemoteCall| {
        let catalogue = Arc::clone(&catalogue);
        let default_preset = default_preset.clone();
        Box::pin(async move {
            let entries = catalogue.entries();
            Ok(json!({
                // Everything a person may pick now, in contribution order.
                "options": entries.iter().map(option_of).collect::<Vec<_>>(),
                // What this deployment offers as a default: the built-in set, which is what a fresh
                // session may start from.
                "defaultOptions": catalogue
                    .available()
                    .iter()
                    .map(|preset| preset.name.clone())
                    .collect::<Vec<_>>(),
                "defaultPreset": default_preset,
            }))
        })
    });
    // A read: the catalogue is process-level and nothing here writes it.
    registry.declare(
        NAMESPACE,
        CATALOG_METHOD,
        crate::guard::access::Declaration::read("permissionPreset"),
    );

    let sessions = Arc::clone(catalog);
    registry.register(NAMESPACE, "set", move |call: RemoteCall| {
        let catalogue = Arc::clone(&selecting);
        let catalog = Arc::clone(&sessions);
        Box::pin(async move { set_preset(&call.args, &catalogue, &catalog).await })
    });
    registry.declare(
        NAMESPACE,
        "set",
        crate::guard::access::Declaration::write("session"),
    );
}

async fn set_preset(
    args: &Value,
    catalogue: &Catalogue,
    catalog: &Arc<dyn crate::api::remote::SessionCatalog>,
) -> Result<Value, RemoteError> {
    let session_id = args
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| preset_invalid("`sessionId` is required".to_owned()))?;
    let preset = args
        .get("preset")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| preset_invalid("`preset` is required".to_owned()))?;
    let Some(entry) = catalogue.get(preset) else {
        return Err(preset_invalid(format!("unknown preset `{preset}`")));
    };
    if !entry.available {
        return Err(preset_invalid(format!(
            "preset `{preset}` is not available"
        )));
    }
    let name = entry.preset.name.clone();
    let sandbox = entry.preset.knobs.sandbox.as_str().to_owned();
    let Some(record) = catalog.read(session_id).await else {
        return Err(preset_invalid(format!(
            "permission: no live session \"{session_id}\""
        )));
    };
    let Some(session) = catalog.open(session_id) else {
        return Err(preset_invalid(format!(
            "permission: no live session \"{session_id}\""
        )));
    };
    let folded = folded_knobs(&record.events);
    if folded.preset.as_deref() != Some(name.as_str()) {
        session
            .append(crate::session::SessionEvent::PermissionPreset {
                preset: name.clone(),
            })
            .await
            .map_err(|error| preset_invalid(error.to_string()))?;
    }
    if folded.sandbox.as_deref() != Some(sandbox.as_str()) {
        session
            .append(crate::session::SessionEvent::SandboxMode {
                mode: sandbox,
                source: None,
            })
            .await
            .map_err(|error| preset_invalid(error.to_string()))?;
    }
    let approval = match entry.preset.knobs.approval {
        crate::credentials::permissions::Approval::Always => "always",
        crate::credentials::permissions::Approval::OutsideWorkspace => "outside-workspace",
        crate::credentials::permissions::Approval::Never => "never",
    };
    if folded.approval.as_deref() != Some(approval) {
        session
            .append(crate::session::SessionEvent::ApprovalPolicy {
                policy: approval.to_owned(),
            })
            .await
            .map_err(|error| preset_invalid(error.to_string()))?;
    }
    let current = match catalog.read(session_id).await {
        Some(record) => current_preset(catalogue, &record.events),
        None => name,
    };
    Ok(json!({ "currentValue": current }))
}

struct FoldedKnobs {
    preset: Option<String>,
    sandbox: Option<String>,
    approval: Option<String>,
}

fn folded_knobs(events: &[crate::session::SessionEvent]) -> FoldedKnobs {
    let mut folded = FoldedKnobs {
        preset: None,
        sandbox: None,
        approval: None,
    };
    for event in events {
        match event {
            crate::session::SessionEvent::PermissionPreset { preset } => {
                folded.preset = Some(preset.clone());
            }
            crate::session::SessionEvent::SandboxMode { mode, .. } => {
                folded.sandbox = Some(mode.clone());
            }
            crate::session::SessionEvent::ApprovalPolicy { policy } => {
                folded.approval = Some(policy.clone());
            }
            _ => {}
        }
    }
    folded
}

fn current_preset(catalogue: &Catalogue, events: &[crate::session::SessionEvent]) -> String {
    let folded = folded_knobs(events);
    if let Some(name) = folded.preset.as_deref()
        && let Some(entry) = catalogue.get(name)
        && (folded.sandbox.is_none()
            || folded.sandbox.as_deref() == Some(entry.preset.knobs.sandbox.as_str()))
        && (folded.approval.is_none()
            || folded.approval.as_deref() == Some(approval_name(entry.preset.knobs.approval)))
    {
        return name.to_owned();
    }
    if let (Some(sandbox), Some(approval)) = (folded.sandbox.as_deref(), folded.approval.as_deref())
    {
        for entry in catalogue.entries() {
            if entry.available
                && entry.preset.knobs.sandbox.as_str() == sandbox
                && approval_name(entry.preset.knobs.approval) == approval
            {
                return entry.preset.name;
            }
        }
    }
    "custom".to_owned()
}

fn approval_name(approval: crate::credentials::permissions::Approval) -> &'static str {
    match approval {
        crate::credentials::permissions::Approval::Always => "always",
        crate::credentials::permissions::Approval::OutsideWorkspace => "outside-workspace",
        crate::credentials::permissions::Approval::Never => "never",
    }
}

fn preset_invalid(message: String) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: "set".to_owned(),
        message,
    }
}

/// The catalogue a deployment gets when it supplies none.
///
/// `Catalogue::builtin` is the same set the approval layer already enforces, so a client reading
/// this and a rule refusing a call cannot disagree about which presets exist.
#[must_use]
pub fn default_catalogue() -> Arc<Catalogue> {
    Arc::new(Catalogue::builtin())
}

/// The preset a fresh session starts from, when the deployment names none.
///
/// This is the builtin `workspace-write` entry. A name outside the catalogue cannot be selected.
pub const DEFAULT_PRESET: &str = "workspace-write";

/// An error for a deployment whose configured default names no preset.
///
/// Checked at registration rather than at call time: a catalogue whose default is unknown would
/// answer a selector with a value nothing can select.
#[must_use]
pub fn validate_default(catalogue: &Catalogue, default_preset: &str) -> Option<RemoteError> {
    if catalogue.get(default_preset).is_some() {
        return None;
    }
    Some(RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: CATALOG_METHOD.to_owned(),
        message: format!(
            "the configured default preset `{default_preset}` is not in the catalogue; \
             available: {}",
            catalogue
                .available()
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_catalogue_is_not_empty() {
        let catalogue = Catalogue::builtin();
        assert!(!catalogue.entries().is_empty());
    }

    #[test]
    fn an_unknown_default_is_reported_with_the_available_names() {
        let catalogue = Catalogue::builtin();
        let error = validate_default(&catalogue, "no-such-preset").expect("refused");
        assert!(
            error.to_string().contains("not in the catalogue"),
            "{error}"
        );
        assert!(error.to_string().contains("available"), "{error}");
    }

    #[test]
    fn the_builtin_default_is_a_selectable_preset() {
        let catalogue = Catalogue::builtin();
        assert!(validate_default(&catalogue, DEFAULT_PRESET).is_none());
    }

    #[test]
    fn an_option_omits_an_empty_description() {
        let catalogue = Catalogue::builtin();
        let first = catalogue.entries().into_iter().next().expect("one entry");
        let option = option_of(&first);
        if first.preset.description.is_empty() {
            assert!(option.get("description").is_none(), "{option}");
        }
        // Availability always travels: a selector needs it to explain a disabled knob.
        assert!(option.get("available").is_some(), "{option}");
    }

    #[tokio::test]
    async fn set_writes_the_preset_into_the_session_log() {
        let dir = std::env::temp_dir().join(format!(
            "girder-preset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let catalog: Arc<dyn crate::api::remote::SessionCatalog> =
            Arc::new(crate::api::remote::DirectoryCatalog::new(&dir));
        catalog.create("s1").await.expect("session");
        let mut registry = RemoteRegistry::new();
        register_permission_preset_methods(
            &mut registry,
            &default_catalogue(),
            "workspace-write",
            &catalog,
        );
        let switched = registry
            .dispatch(
                NAMESPACE,
                "set",
                RemoteCall {
                    args: json!({ "sessionId": "s1", "preset": "workspace-write" }),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("known preset");
        assert_eq!(switched["currentValue"], "workspace-write");
        let record = catalog.read("s1").await.expect("session");
        assert!(record.events.iter().any(|event| matches!(
            event,
            crate::session::SessionEvent::PermissionPreset { preset } if preset == "workspace-write"
        )));
        assert!(record.events.iter().any(|event| matches!(
            event,
            crate::session::SessionEvent::SandboxMode { mode, .. } if mode == "workspace-write"
        )));
        assert!(record.events.iter().any(|event| matches!(
            event,
            crate::session::SessionEvent::ApprovalPolicy { policy } if policy == "outside-workspace"
        )));
        let again = registry
            .dispatch(
                NAMESPACE,
                "set",
                RemoteCall {
                    args: json!({ "sessionId": "s1", "preset": "workspace-write" }),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("same preset");
        assert_eq!(again["currentValue"], "workspace-write");
        let reread = catalog.read("s1").await.expect("session");
        assert_eq!(reread.events.len(), record.events.len());
        let missing = registry
            .dispatch(
                NAMESPACE,
                "set",
                RemoteCall {
                    args: json!({ "sessionId": "absent", "preset": "read-only" }),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect_err("missing session");
        assert!(missing.to_string().contains("no live session"), "{missing}");
        let error = registry
            .dispatch(
                NAMESPACE,
                "set",
                RemoteCall {
                    args: json!({ "sessionId": "s1", "preset": "no-such" }),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect_err("unknown preset");
        assert_eq!(error.code(), "INVALID_ARGUMENTS");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
