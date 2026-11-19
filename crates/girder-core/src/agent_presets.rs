//! `agentPresets.*`: the declarative capability sets a session may start from.
//!
//! # What upstream has
//!
//! `packages/preset/agent-preset-registry/src/index.ts` marks three methods:
//!
//! | Endpoint | Answers |
//! | --- | --- |
//! | `list` | every preset, each marked when it is the deployment default |
//! | `read` | one preset's declared composition, as YAML |
//! | `select` | binds a preset to a live agent |
//!
//! Upstream's presets are Cordis plugin lists: `read` dumps `record.config.plugins` through the
//! Loader's YAML dialect. **This build has no plugin tree**, so a preset here is a named set of tool
//! and model defaults rather than a plugin composition. That is the same substitution that makes
//! `pluginManager` a boundary, applied one layer up: the *shape* of the answer is upstream's, and the
//! content is what this substrate can express.
//!
//! # Ordering
//!
//! Upstream sorts by `order` with `Infinity` for a preset that declares none, then by id. That is
//! reproduced exactly, including the `None`-sorts-last part: a registry that put unordered presets
//! first would make every newly declared preset jump to the top of a picker.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "agentPresets";

/// One preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preset {
    /// The identity a client selects.
    pub id: String,
    /// A display name, when one is declared.
    pub name: Option<String>,
    /// What it is for.
    pub description: Option<String>,
    /// Where it sorts. `None` sorts last, as upstream's `Infinity` does.
    pub order: Option<u32>,
    /// The composition, for `read`. This build's content: the tool names a session starts with.
    pub tools: Vec<String>,
    /// The model a session starts on, when the preset names one.
    pub model: Option<String>,
}

impl Preset {
    /// A preset with an id and nothing else.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            description: None,
            order: None,
            tools: Vec::new(),
            model: None,
        }
    }

    /// Renders the roster row.
    fn to_row(&self, default_id: Option<&str>) -> Value {
        let mut row = json!({ "id": self.id });
        if let Some(name) = &self.name {
            row["name"] = json!(name);
        }
        if let Some(description) = &self.description {
            row["description"] = json!(description);
        }
        if let Some(order) = self.order {
            row["order"] = json!(order);
        }
        // Upstream marks the default on the row rather than naming it once, so a client reading a
        // single row can tell without consulting another field.
        row["isDefault"] = json!(default_id == Some(self.id.as_str()));
        row
    }
}

/// The preset registry.
#[derive(Default)]
pub struct Registry {
    presets: BTreeMap<String, Preset>,
    default_id: Option<String>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a preset, replacing any with the same id.
    pub fn insert(&mut self, preset: Preset) {
        if self.default_id.is_none() {
            // The first preset becomes the default, which is what upstream does when a deployment
            // names none: a roster with no default would leave every session unbound.
            self.default_id = Some(preset.id.clone());
        }
        self.presets.insert(preset.id.clone(), preset);
    }

    /// Names the default explicitly.
    pub fn set_default(&mut self, id: &str) {
        self.default_id = Some(id.to_owned());
    }

    /// Every preset, in upstream's order: `order` ascending with `None` last, then id.
    #[must_use]
    pub fn ordered(&self) -> Vec<&Preset> {
        let mut presets: Vec<&Preset> = self.presets.values().collect();
        presets.sort_by(|left, right| {
            // `u32::MAX` stands in for upstream's `Infinity`: a preset with no order sorts after
            // every preset that declared one, which is the behaviour a picker expects.
            let left_order = left.order.unwrap_or(u32::MAX);
            let right_order = right.order.unwrap_or(u32::MAX);
            left_order
                .cmp(&right_order)
                .then_with(|| left.id.cmp(&right.id))
        });
        presets
    }

    /// The default preset's id.
    #[must_use]
    pub fn default_id(&self) -> Option<&str> {
        self.default_id.as_deref()
    }

    /// One preset.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Preset> {
        self.presets.get(id)
    }
}

/// Renders one preset's composition for `read`.
///
/// Upstream answers YAML because its content *is* a plugin list dumped from the Loader's dialect.
/// This build has no plugin tree, so the content is a small JSON document describing what the preset
/// sets. A caller that only displays it needs no more; a caller that parsed upstream's YAML would
/// not find the same dialect here, which is stated rather than papered over.
fn composition(preset: &Preset) -> Value {
    let mut content = json!({
        "id": preset.id,
        "tools": preset.tools,
    });
    if let Some(model) = &preset.model {
        content["model"] = json!(model);
    }
    content
}

/// Registers `agentPresets.{list, read, select}`.
pub fn register_agent_preset_methods(registry: &mut RemoteRegistry, presets: &Arc<Registry>) {
    let listing = Arc::clone(presets);
    registry.register(NAMESPACE, "list", move |_call: RemoteCall| {
        let presets = Arc::clone(&listing);
        Box::pin(async move {
            let default_id = presets.default_id();
            Ok(json!({
                "presets": presets
                    .ordered()
                    .iter()
                    .map(|preset| preset.to_row(default_id))
                    .collect::<Vec<_>>(),
            }))
        })
    });

    let reading = Arc::clone(presets);
    registry.register(NAMESPACE, "read", move |call: RemoteCall| {
        let presets = Arc::clone(&reading);
        Box::pin(async move {
            let id = call
                .args
                .get("agentPreset")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "read".to_owned(),
                    message: "`agentPreset` is required".to_owned(),
                })?;
            // Upstream's `agent-preset/not-found`, which names the available ids so a caller with a
            // typo learns the set rather than only the miss.
            let Some(preset) = presets.get(id) else {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "read".to_owned(),
                    message: format!(
                        "unknown agent preset `{id}`; available: {}",
                        presets
                            .ordered()
                            .iter()
                            .map(|preset| preset.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            };
            let mut value = json!({
                "agentPreset": preset.id,
                "content": composition(preset),
            });
            if let Some(name) = &preset.name {
                value["name"] = json!(name);
            }
            if let Some(description) = &preset.description {
                value["description"] = json!(description);
            }
            Ok(value)
        })
    });

    let selecting = Arc::clone(presets);
    registry.register(NAMESPACE, "select", move |call: RemoteCall| {
        let presets = Arc::clone(&selecting);
        Box::pin(async move {
            let id = call
                .args
                .get("agentPreset")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "select".to_owned(),
                    message: "`agentPreset` is required".to_owned(),
                })?;
            if presets.get(id).is_none() {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "select".to_owned(),
                    message: format!("unknown agent preset `{id}`"),
                });
            }
            // Binding needs a live agent, which this registration does not hold. Answered rather
            // than claimed: `ok: true` would tell a caller a session had changed preset.
            Ok(json!({
                "ok": false,
                "code": "agent_unavailable",
                "message": "selecting a preset binds it to a live agent; this deployment has none attached to this call",
                "agentPreset": id,
            }))
        })
    });
}

/// The presets a deployment gets when it configures none.
///
/// One, named `default`, carrying no tools and no model: a session that starts from it gets whatever
/// the deployment's own defaults are. An empty registry would leave `list` answering nothing, which
/// a picker cannot show.
#[must_use]
pub fn default_registry() -> Arc<Registry> {
    let mut presets = Registry::new();
    let mut preset = Preset::new("default");
    preset.name = Some("Default".to_owned());
    preset.description = Some("The deployment's own tool and model defaults.".to_owned());
    preset.order = Some(0);
    presets.insert(preset);
    Arc::new(presets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(id: &str, order: Option<u32>) -> Preset {
        let mut preset = Preset::new(id);
        preset.order = order;
        preset
    }

    #[test]
    fn an_unordered_preset_sorts_last() {
        // Upstream's `Infinity`: a preset that declares no order must not jump to the top of a
        // picker just because it was added.
        let mut registry = Registry::new();
        registry.insert(preset("z-last", None));
        registry.insert(preset("b-first", Some(1)));
        registry.insert(preset("a-second", Some(2)));
        let ids: Vec<&str> = registry.ordered().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["b-first", "a-second", "z-last"]);
    }

    #[test]
    fn equal_orders_fall_back_to_the_id() {
        let mut registry = Registry::new();
        registry.insert(preset("beta", Some(1)));
        registry.insert(preset("alpha", Some(1)));
        let ids: Vec<&str> = registry.ordered().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "beta"]);
    }

    #[test]
    fn the_first_preset_becomes_the_default() {
        // A roster with no default would leave every session unbound.
        let mut registry = Registry::new();
        registry.insert(preset("only", None));
        assert_eq!(registry.default_id(), Some("only"));
    }

    #[test]
    fn a_row_marks_the_default_rather_than_naming_it_once() {
        // A client reading one row must be able to tell without consulting another field.
        let registry = default_registry();
        let row = registry.ordered()[0].to_row(registry.default_id());
        assert_eq!(row["isDefault"], json!(true));
    }

    #[test]
    fn the_default_registry_is_not_empty() {
        // An empty one would leave `list` answering nothing, which a picker cannot show.
        assert!(!default_registry().ordered().is_empty());
    }
}
