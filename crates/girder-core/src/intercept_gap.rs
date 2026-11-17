//! Service intercept config: a per-service stack of config layers along a context chain.
//!
//! Upstream Cordis lets a plugin add config for a *service it does not own*. `ctx.intercept(name,
//! config)` returns a child context, and every plugin loaded below that child sees its service
//! named `name` configured with `config` merged over whatever its ancestors asked for:
//!
//! ```text
//! intercept(name, config) {                 // context.ts
//!   const intercept = Object.create(this[symbols.intercept])
//!   intercept[name] = config
//!   return this.extend({ [symbols.intercept]: intercept })
//! }
//!
//! [symbols.resolveConfig](base?, head?) {   // service.ts
//!   let intercept = this.ctx[Context.intercept]
//!   const configs = []
//!   while (this.name in intercept) {
//!     if (Object.hasOwn(intercept, this.name)) configs.unshift(intercept[this.name])
//!     intercept = Object.getPrototypeOf(intercept)
//!   }
//!   if (base) configs.unshift(base)
//!   if (head) configs.push(head)
//!   return this.Config?.merge ? this.Config.merge(...configs) : Object.assign({}, ...configs)
//! }
//! ```
//!
//! Three rules fall out of that, and they are the whole of the feature:
//!
//! 1. **Ancestor entries apply first.** The chain is walked from the context upward and each entry
//!    is `unshift`ed, so the finished list runs root-most to nearest.
//! 2. **`base` is prepended, `head` appended.** They bracket every intercept, so a service can
//!    supply defaults and a caller-supplied override on either side of the intercept band.
//! 3. **Merge is shallow and last-wins per key** — `Object.assign({}, ...configs)` — unless the
//!    service declares a static `Config.merge`, in which case that function receives the whole
//!    list at once (which is how a service makes arrays concatenate instead of replace).
//!
//! A fourth rule is implicit in `Object.create`: the parent context is untouched. Only the subtree
//! below the returned child sees the new entry, and a sibling branch never does.
//!
//! # What is and is not portable
//!
//! Portable, and implemented here:
//!
//! - the chain walk, the root-first ordering, and `base`/`head` at the two ends;
//! - shallow per-key merge with later layers winning;
//! - the `Config.merge` hook, as a caller-supplied closure ([`Intercepts::resolve_config_with`]);
//! - parent/child and sibling invisibility.
//!
//! Not portable: the *storage*. Upstream keeps one map object per context and finds ancestors by
//! JavaScript's prototype chain. Rust has no prototype chain, so a node here owns a parent pointer
//! and is shared by [`Arc`]. The observable behaviour — walk up, own entries only, nearest last,
//! parent unmodified — is the same; the mechanism is not, which is why this is a new type rather
//! than a change to `registry.rs`.
//!
//! Also not portable: `if (base)` is JavaScript truthiness, so `0`, `''`, `false`, `null`, and
//! `undefined` all skip the layer. Rust makes absence explicit with `Option`, which is what the
//! `base`/`head` parameters are; an *empty* [`Config`] is still a layer (it is `{}`, which is
//! truthy in JavaScript) but contributes no keys to a shallow merge.
//!
//! # Isolation
//!
//! Upstream's `isolate()` and `intercept()` are independent maps: isolating a name does **not** cut
//! its intercept chain, because `resolveConfig` walks the intercept chain regardless of labels.
//! [`Intercepts::isolate`] is therefore a local extension, not a port. It exists because the other
//! half of the property this crate wants — *a child cannot see the parent's configuration for an
//! isolated name* — cannot otherwise be stated, and because it is exactly the shape
//! `Registry::isolate` already has: an own entry that shadows the inherited one instead of layering
//! over it. Wiring note for the integrator: map `Registry::isolate` to `Intercepts::isolate` only
//! where the two axes are *wanted* to move together; upstream would keep them apart.
//!
//! # Wiring
//!
//! This module is deliberately self-contained: it imports only `std`, [`Arc`], and
//! [`serde_json`], and never names `Registry`, `RegistryContext`, or `config::Entry`. That is what
//! lets `tests/intercept_gap.rs` compile the exact file by `#[path]` before the crate root
//! declares it, and it is what leaves the decision of where intercepts are stored — a
//! `RegistryContext` field, a fiber field, or a boot-time side table — to whoever wires it.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

/// One service's config as a flat key/value mapping.
///
/// A [`Config`] is a JSON object because that is what `Object.assign` and `Config.merge` operate
/// on upstream. Non-object values are rejected at the boundary by
/// [`Config::try_from_object`] rather than silently coerced: JavaScript would spread a string into
/// `{'0': 'a', '1': 'b', …}`, which no caller has ever wanted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// Keys in sorted order, so iteration and serialization are deterministic. Upstream's
    /// `Object.assign` keeps insertion order, which nothing depends on: every consumer reads keys
    /// by name.
    entries: BTreeMap<String, Value>,
}

impl Config {
    /// Creates an empty config.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a config from key/value pairs, later pairs winning on a repeated key.
    pub fn from_pairs<I, K>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        let mut config = Self::new();
        for (key, value) in pairs {
            config.insert(key, value);
        }
        config
    }

    /// Acquires a config from a JSON value, requiring an object.
    ///
    /// # Errors
    ///
    /// Returns [`InterceptError::NotAnObject`] naming the JSON type that was found instead.
    pub fn try_from_object(value: Value) -> Result<Self, InterceptError> {
        match value {
            Value::Object(map) => Ok(Self {
                entries: map.into_iter().collect(),
            }),
            other => Err(InterceptError::NotAnObject {
                found: json_type_name(&other),
            }),
        }
    }

    /// The value stored under `key`, when this config sets it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.get(key)
    }

    /// Sets `key`, returning the value it displaced, if any.
    ///
    /// Overwriting is the point: a later layer replaces a key it names and leaves keys it does not
    /// name alone, which is the whole of shallow merge.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.entries.insert(key.into(), value)
    }

    /// The number of keys this config sets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when this config sets no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The keys this config sets, in sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Consumes this config, yielding the underlying mapping.
    #[must_use]
    pub fn into_map(self) -> BTreeMap<String, Value> {
        self.entries
    }

    /// Merges `layers` left to right, later layers winning per key.
    ///
    /// This is `Object.assign({}, ...layers)`: shallow, and a named key is *replaced*, never
    /// recursively combined. A nested object in a later layer therefore discards every sibling key
    /// the earlier layer had in it, which is upstream's behaviour and the reason a service that
    /// wants depth declares a `Config.merge` and uses [`Intercepts::resolve_config_with`].
    #[must_use]
    pub fn merge_shallow(layers: &[&Self]) -> Self {
        let mut merged = Self::new();
        for layer in layers {
            for (key, value) in &layer.entries {
                merged.entries.insert(key.clone(), value.clone());
            }
        }
        merged
    }
}

impl FromIterator<(String, Value)> for Config {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        let mut config = Self::new();
        config.entries.extend(iter);
        config
    }
}

/// A rejected intercept config.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InterceptError {
    /// The config was valid JSON but not an object, so it has no keys to merge.
    #[error("intercept config must be a JSON object, found {found}")]
    NotAnObject {
        /// The JSON type that was supplied — `null`, `array`, `string`, `number`, or `boolean`.
        found: &'static str,
    },
}

/// The JSON type name used in [`InterceptError::NotAnObject`].
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// What a node contributes to the chain.
#[derive(Debug)]
enum Link {
    /// The root: contributes nothing.
    Root,
    /// One config layer for one service name.
    Intercept { name: String, config: Config },
    /// A shadow that hides every ancestor layer for one service name.
    Isolated { name: String },
}

/// One context's contribution, plus the parent it inherits from.
///
/// Nodes are immutable and shared by [`Arc`], so deriving a child never touches its parent — the
/// property `Object.create` gives upstream for free.
#[derive(Debug)]
struct Node {
    /// The context this one extends, or `None` at the root.
    parent: Option<Arc<Node>>,
    /// What this context adds.
    link: Link,
}

/// A handle onto a context's intercept chain.
///
/// [`Intercepts::root`] is the root context: it has no entries. [`Intercepts::intercept`] and
/// [`Intercepts::isolate`] each derive a child that adds one contribution, exactly like
/// `ctx.intercept(...)` returns `ctx.extend(...)`.
///
/// Clones share the same node, so two branches derived from one clone are the same scope — the
/// analogue of `Registry::isolate(name, Some(label))` joining two subtrees. Two branches derived
/// from *different* calls are separate scopes even when they were derived from the same parent,
/// because each call allocates a node.
#[derive(Clone, Debug)]
pub struct Intercepts {
    node: Arc<Node>,
}

impl Default for Intercepts {
    fn default() -> Self {
        Self::root()
    }
}

impl PartialEq for Intercepts {
    /// Compares scope identity, not content: two handles are equal when they point at the same
    /// node, which is what "these two subtrees share a scope" means.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.node, &other.node)
    }
}

impl Eq for Intercepts {}

impl Intercepts {
    /// Creates the root of a chain: no entries, no parent.
    #[must_use]
    pub fn root() -> Self {
        Self {
            node: Arc::new(Node {
                parent: None,
                link: Link::Root,
            }),
        }
    }

    /// Derives a child that adds `config` as a layer for service `name`.
    ///
    /// The parent is not modified, and nothing outside the returned child's subtree sees the
    /// layer. Calling this twice on the same chain stacks the layers in call order — the second
    /// call's node is nearer, so it wins on a shared key.
    #[must_use]
    pub fn intercept(&self, name: impl Into<String>, config: Config) -> Self {
        Self {
            node: Arc::new(Node {
                parent: Some(Arc::clone(&self.node)),
                link: Link::Intercept {
                    name: name.into(),
                    config,
                },
            }),
        }
    }

    /// Derives a child in which `name`'s ancestor layers are invisible.
    ///
    /// This is the config-side analogue of `Registry::isolate`: an own entry shadows the inherited
    /// one instead of layering over it, so a subtree can be configured from scratch without
    /// inheriting an ancestor's mandate. It is **not** an upstream behaviour — see the module
    /// documentation — and it is per name, so sibling services keep inheriting normally.
    ///
    /// Layers added below the returned child are still visible; only the ancestors' are hidden. A
    /// second call to `isolate` for the same name below the first starts from nothing again.
    #[must_use]
    pub fn isolate(&self, name: impl Into<String>) -> Self {
        Self {
            node: Arc::new(Node {
                parent: Some(Arc::clone(&self.node)),
                link: Link::Isolated { name: name.into() },
            }),
        }
    }

    /// The layers that apply to `name`, root-most first, with `base` prepended and `head`
    /// appended.
    ///
    /// This is the list `resolveConfig` builds before it merges: the shape both
    /// [`Intercepts::resolve_config`] and a custom merge closure receive.
    #[must_use]
    pub fn layers<'a>(
        &'a self,
        name: &str,
        base: Option<&'a Config>,
        head: Option<&'a Config>,
    ) -> Vec<&'a Config> {
        let mut layers = Vec::new();
        // Walk root-most first, which is the order entries must be merged in.
        for node in self.nodes_from_root() {
            match &node.link {
                Link::Intercept {
                    name: own_name,
                    config,
                } if own_name == name => layers.push(config),
                // A shadow truncates everything gathered from further up. Walking root-first
                // means a nearer shadow clears the ancestor layers but keeps any layer added
                // between the shadow and the reader.
                Link::Isolated { name: own_name } if own_name == name => layers.clear(),
                // The root contributes nothing, and an entry for another service is not this
                // service's business.
                Link::Root | Link::Intercept { .. } | Link::Isolated { .. } => {}
            }
        }
        if let Some(base) = base {
            layers.insert(0, base);
        }
        if let Some(head) = head {
            layers.push(head);
        }
        layers
    }

    /// Resolves `name`'s config: `base`, then every intercept root-first, then `head`, merged
    /// shallowly.
    ///
    /// With no intercept in scope the result is `base` merged with `head` — or an empty config
    /// when neither is given, which is what a service with no intercept sees upstream.
    #[must_use]
    pub fn resolve_config(
        &self,
        name: &str,
        base: Option<&Config>,
        head: Option<&Config>,
    ) -> Config {
        Config::merge_shallow(&self.layers(name, base, head))
    }

    /// Resolves `name`'s config with a service-declared merge, mirroring static `Config.merge`.
    ///
    /// The closure receives every layer once, root-first with `base` at the front and `head` at the
    /// back, and returns whatever the service's merge produces. Upstream spreads the list into
    /// `Config.merge(...configs)`, so a service that concatenates arrays or merges deeply gets the
    /// same inputs here.
    pub fn resolve_config_with<T>(
        &self,
        name: &str,
        base: Option<&Config>,
        head: Option<&Config>,
        merge: impl FnOnce(&[&Config]) -> T,
    ) -> T {
        merge(&self.layers(name, base, head))
    }

    /// This chain's nodes, root first.
    fn nodes_from_root(&self) -> Vec<&Node> {
        let mut nodes = Vec::new();
        let mut cursor = Some(self.node.as_ref());
        while let Some(node) = cursor {
            nodes.push(node);
            cursor = node.parent.as_deref();
        }
        nodes.reverse();
        nodes
    }
}
