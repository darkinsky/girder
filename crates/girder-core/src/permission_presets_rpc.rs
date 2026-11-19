//! `permissionPresets.catalog`: every selectable preset, in contribution order.
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
//! [`crate::permissions::Catalogue`] already models a built-in set, availability, and ordered
//! entries. This module reads it out; it does not decide anything.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::permissions::Catalogue;
use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};

/// The namespace this method registers under, as upstream spells it.
pub const NAMESPACE: &str = "permissionPresets";

/// The one endpoint.
pub const CATALOG_METHOD: &str = "catalog";

/// Renders one entry the way upstream's `PresetOption` is shaped.
///
/// `description` is omitted when a preset has none, matching the spread-of-undefined upstream uses:
/// an empty string would render as a blank line in a selector, which reads as a preset whose
/// explanation is missing rather than one that needs none.
fn option_of(entry: &crate::permissions::Entry) -> Value {
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
) {
    let catalogue = Arc::clone(catalogue);
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
        crate::access::Declaration::read("permissionPreset"),
    );
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
/// Upstream's composition default. Named here rather than inline because two callers (this and a
/// session's first turn) must agree.
pub const DEFAULT_PRESET: &str = "default";

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
    fn an_option_omits_an_empty_description() {
        let mut catalogue = Catalogue::builtin();
        let first = catalogue.entries().into_iter().next().expect("one entry");
        let option = option_of(&first);
        if first.preset.description.is_empty() {
            assert!(option.get("description").is_none(), "{option}");
        }
        // Availability always travels: a selector needs it to explain a disabled knob.
        assert!(option.get("available").is_some(), "{option}");
    }
}
