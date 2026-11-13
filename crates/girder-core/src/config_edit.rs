//! A declared-schema layer that makes `settings.describe` answerable over a flat key/value store.
//!
//! [`crate::settings_rpc`] refuses `settings.describe`, and the reason it gives is sound: a flat
//! key/value store has no schema, no layers, and no revision to report, so a namespace view built
//! from it would be a view a client tries to rehydrate from fields that do not exist. But that
//! refusal answers for the *store* when the question is about the *deployment*. The shape of a
//! deployment's settings is something the deployment knows: it wrote the code that reads
//! `provider.model`, it knows that value is a string, and it knows that `provider.apiKey` must
//! never be echoed back.
//!
//! This module is where those facts are stated. It is a registry of declarations, not a second
//! settings surface: [`SchemaRegistry::describe`] renders the declarations together with the values
//! a [`SettingsStore`] currently holds, and [`SchemaRegistry::validate`] is the write-side
//! counterpart a host can call before it stores a value. Nothing here registers a Remote method and
//! nothing here writes to the store; wiring is the integrator's decision.
//!
//! # The store key is `namespace.key`
//!
//! A declaration carries a namespace and a key separately, because a client groups a form by
//! namespace, while a [`SettingsStore`] addresses one flat value per key. The two are joined as
//! `{namespace}.{key}` (see [`SettingSpec::store_key`]), the same dotted convention
//! [`crate::api_config::SettingView`] already uses for `provider.model`. Joining is a stated rule
//! rather than a guess, so `describe` reads exactly the value a client will later write.
//!
//! # Secrets are withheld rather than filtered
//!
//! A declaration marked [`secret`](SettingSpec::secret) must never have its value leave the
//! process. That is enforced structurally, not by a filter that runs afterwards: the secret branch
//! of the projection asks the store *whether* a value exists and then reports a constant
//! ([`REDACTED`]) or `null`. The stored value is never cloned into the response, never formatted,
//! and never handed to a field that is later serialized. The declared default is dropped for the
//! same reason — a default secret is as sensitive as a stored one, and echoing it would leak the
//! one value a deployment wrote down instead of one it kept in a vault.
//!
//! The response still reports `configured`, because "a secret is set" is information a form needs
//! to render "replace" rather than "add". It is the *value* that is withheld, not the fact that
//! something is stored there.
//!
//! # Undeclared keys are hidden from `describe` and refused by `validate`
//!
//! A store is a shared flat map. It may hold keys from an earlier release, or keys another
//! subsystem wrote for itself — including credential material the settings surface never owned.
//! Echoing such a key would publish a value no declaration marked secret, and rendering it would
//! offer an edit with no type to check. So `describe` reports only declared settings and returns a
//! count of what it hid, which is enough for an operator to see drift without the names or values
//! themselves becoming a second, unreviewed description surface.
//!
//! `validate` refuses undeclared keys for the matching reason: if the registry cannot describe a
//! key, it cannot vouch for what a write to it means, and accepting one would let a typo
//! (`provider.modle`) create a setting nothing reads. A deployment that wants a key editable
//! declares it; refusing is the honest half of the same policy that hides it.
//!
//! # Validation checks JSON types, not the whole schema
//!
//! The declared schema is a JSON Schema fragment, carried verbatim to the client so the form can
//! render it. Server-side, only the `type` keyword is enforced: `string`, `number`, `integer`,
//! `boolean`, `null`, `array`, and `object`, including the list form JSON Schema allows. A fraction
//! of JSON Schema is implemented on purpose — a second full validator in this crate would be a
//! second source of truth, and the client is already the one rendering the form. The one thing a
//! write path must never get wrong is storing a number where the reader expects a string, and that
//! is exactly what this checks.
//!
//! A schema with no `type` makes no claim, so nothing is checked. An unrecognised type name (a
//! deployment may extend schemas with UI-only types such as `color`) also skips the check rather
//! than rejecting every value: refusing a write the deployment's own reader accepts would be worse
//! than the missing check, and the missing check is visible in the declaration itself.
//!
//! Error messages name the namespace, the key, the declared types, and the JSON kind of the value.
//! They never include the value, because a refusal can reach a log and a value may be secret.
//!
//! # Wire shape
//!
//! [`SchemaRegistry::describe`] answers with namespaces in name order and settings in key order, so
//! two runs over the same registry serialize identically and a diff of the output is meaningful:
//!
//! ```text
//! {
//!   "undeclared": 0,                     // stored keys no declaration covers; values are not sent
//!   "namespaces": [
//!     {
//!       "namespace": "provider",
//!       "settings": [
//!         {
//!           "key": "model",
//!           "store_key": "provider.model",
//!           "schema": {"type": "string"},
//!           "default": "deepseek-chat",  // omitted when none was declared, or when secret
//!           "secret": false,
//!           "description": "The model to call.",
//!           "value": "deepseek-chat",    // the resolved value: stored, else the default
//!           "configured": false          // whether the store itself holds this key
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

use serde_json::{Map, Value, json};

use crate::settings_rpc::SettingsStore;

// ---- surface constants ------------------------------------------------------------------------

/// The value reported in place of a secret.
///
/// A constant rather than an omitted field, so a client can tell "a value is stored, and it is not
/// yours to read" from "nothing is stored" (`null`) without a second flag. It is exported because a
/// form that compares against it should not hard-code the literal.
pub const REDACTED: &str = "***";

// ---- declarations -----------------------------------------------------------------------------

/// One declared setting: its shape, its default, and whether its value may be shown.
///
/// The fields are public so a deployment declares settings with a struct literal, which keeps the
/// declaration next to the code that reads the value. Every field is plain data; nothing here
/// validates the schema fragment, because the fragment is the deployment's description of itself
/// and this crate only interprets the `type` keyword.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingSpec {
    /// The namespace the setting belongs to, such as `provider`.
    pub namespace: String,
    /// The key within the namespace, such as `model`.
    pub key: String,
    /// The type description, as a JSON Schema fragment.
    ///
    /// Carried to the client verbatim so a form can render a field for it. Only its `type` keyword
    /// is enforced by [`SchemaRegistry::validate`].
    pub schema: Value,
    /// The value to assume when the store holds none.
    ///
    /// Ignored for a [`secret`](Self::secret) setting: the default is not echoed and not resolved,
    /// because a default is a value the deployment wrote down in code and may be as sensitive as a
    /// stored one.
    pub default: Option<Value>,
    /// Whether the value must never be echoed to a caller.
    ///
    /// When true, [`SchemaRegistry::describe`] reports [`REDACTED`] if the store holds a value and
    /// `null` otherwise. The value is checked for presence only.
    pub secret: bool,
    /// What the setting means, for the form's label.
    pub description: String,
}

impl SettingSpec {
    /// The key this setting is stored under: `{namespace}.{key}`.
    ///
    /// Exposed rather than kept private, because a host that writes a value outside
    /// [`SchemaRegistry::validate`] still has to address it with the same key `describe` read.
    #[must_use]
    pub fn store_key(&self) -> String {
        format!("{}.{}", self.namespace, self.key)
    }
}

// ---- the registry -----------------------------------------------------------------------------

/// The declared settings, grouped by namespace and kept in name order.
///
/// Name order is why the inner and outer maps are ordered ones: a `describe` response is a document
/// a human reads and diffs, and a hash-ordered one would reorder between releases for reasons the
/// reader cannot see.
///
/// Duplicate declarations are refused by [`declare`](Self::declare) rather than overwriting: two
/// declarations of one key would make the rendered form depend on declaration order, and the second
/// one would silently win for every read afterwards.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaRegistry {
    namespaces: BTreeMap<String, BTreeMap<String, SettingSpec>>,
}

impl SchemaRegistry {
    /// Creates a registry with nothing declared.
    ///
    /// [`describe`](Self::describe) on an empty registry is a valid response (`namespaces: []`) that
    /// still counts the keys it is hiding, so a deployment that has not declared anything sees why
    /// its settings page is empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares one setting.
    ///
    /// # Errors
    ///
    /// - [`SchemaError::InvalidDeclaration`] when the namespace or the key cannot address a
    ///   setting, or when they would make [`describe`](Self::describe) read a path the deployment
    ///   did not name.
    /// - [`SchemaError::DuplicateDeclaration`] when this namespace and key are already declared. A
    ///   namespace and a key are the identity; the same key may be declared in two namespaces,
    ///   because those are two settings.
    pub fn declare(&mut self, spec: SettingSpec) -> Result<(), SchemaError> {
        if let Some(reason) = name_problem(&spec.namespace) {
            return Err(SchemaError::InvalidDeclaration {
                namespace: spec.namespace,
                key: spec.key,
                reason,
            });
        }
        if let Some(reason) = name_problem(&spec.key) {
            return Err(SchemaError::InvalidDeclaration {
                namespace: spec.namespace,
                key: spec.key,
                reason,
            });
        }

        // The entry API is used rather than `contains_key` followed by `insert` so the duplicate
        // check and the insertion speak about one lookup, and the error can carry the declaration
        // that was refused.
        let namespace = spec.namespace.clone();
        let key = spec.key.clone();
        let slot = self
            .namespaces
            .entry(spec.namespace.clone())
            .or_default()
            .entry(spec.key.clone());
        match slot {
            Entry::Occupied(_) => Err(SchemaError::DuplicateDeclaration { namespace, key }),
            Entry::Vacant(vacant) => {
                vacant.insert(spec);
                Ok(())
            }
        }
    }

    /// Looks a declaration up by namespace and key.
    ///
    /// `None` means this registry does not describe that setting, which is also the answer
    /// [`validate`](Self::validate) turns into a refusal.
    #[must_use]
    pub fn get(&self, namespace: &str, key: &str) -> Option<&SettingSpec> {
        self.namespaces.get(namespace)?.get(key)
    }

    /// How many settings are declared, across every namespace.
    #[must_use]
    pub fn len(&self) -> usize {
        self.namespaces.values().map(BTreeMap::len).sum()
    }

    /// Whether nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.namespaces.values().all(BTreeMap::is_empty)
    }

    /// Builds the `describe` response: every declared setting with its resolved value.
    ///
    /// The resolved value is what the store holds, else the declared default, else `null`. A secret
    /// is never resolved: it reports [`REDACTED`] when the store holds something and `null` when it
    /// does not, and its declared default is omitted from the response entirely.
    ///
    /// Nothing in the response is fabricated. `configured` is a fact about the store, `undeclared`
    /// is a count of what was withheld, and a namespace with no declarations is absent rather than
    /// present with an empty list, because that is what the registry knows.
    #[must_use]
    pub fn describe(&self, store: &dyn SettingsStore) -> Value {
        let stored: BTreeMap<String, Value> = store.list().into_iter().collect();
        let declared: BTreeSet<String> = self
            .namespaces
            .values()
            .flat_map(|specs| specs.values())
            .map(SettingSpec::store_key)
            .collect();
        // A count rather than a list: an undeclared key may be one another subsystem wrote for
        // itself, and its name can be as telling as its value. The count tells an operator that the
        // store holds more than this registry describes without publishing what.
        let undeclared = stored.keys().filter(|key| !declared.contains(*key)).count();

        let mut namespaces = Vec::with_capacity(self.namespaces.len());
        for (namespace, specs) in &self.namespaces {
            let settings = specs
                .values()
                .map(|spec| project(spec, stored.get(&spec.store_key())))
                .collect::<Vec<_>>();
            namespaces.push(json!({ "namespace": namespace, "settings": settings }));
        }

        json!({ "undeclared": undeclared, "namespaces": namespaces })
    }

    /// Checks one value against its declaration's type, before it is written.
    ///
    /// # Errors
    ///
    /// - [`SchemaError::Undeclared`] when no declaration covers this namespace and key. The registry
    ///   cannot vouch for a key it does not describe, so it refuses instead of passing the value
    ///   through.
    /// - [`SchemaError::TypeMismatch`] when the value is not one of the types the schema declares.
    ///   The message names the JSON kind of the value, never the value itself.
    ///
    /// A declaration whose schema has no `type`, or a type name this build does not know, is
    /// accepted unchecked; see the module documentation for why.
    pub fn validate(&self, namespace: &str, key: &str, value: &Value) -> Result<(), SchemaError> {
        let Some(spec) = self.get(namespace, key) else {
            return Err(SchemaError::Undeclared {
                namespace: namespace.to_owned(),
                key: key.to_owned(),
            });
        };
        let accepted = declared_types(&spec.schema);
        if accepted.is_empty() {
            return Ok(());
        }
        if accepted.iter().any(|name| type_accepts(name, value)) {
            return Ok(());
        }
        Err(SchemaError::TypeMismatch {
            namespace: namespace.to_owned(),
            key: key.to_owned(),
            expected: accepted.join(" or "),
            actual: json_kind(value),
        })
    }
}

// ---- projection -------------------------------------------------------------------------------

/// Renders one declaration and the value the store holds for it.
///
/// Written as a single function so the secret case is one branch a reader can inspect, rather than
/// a later pass over fields that might miss one. In the secret branch `stored` is used only through
/// [`Option::is_some`]; it is never cloned, formatted, or moved into the response.
fn project(spec: &SettingSpec, stored: Option<&Value>) -> Value {
    let configured = stored.is_some();
    let mut entry = Map::new();
    entry.insert("key".to_owned(), Value::String(spec.key.clone()));
    entry.insert("store_key".to_owned(), Value::String(spec.store_key()));
    entry.insert("schema".to_owned(), spec.schema.clone());
    entry.insert("secret".to_owned(), Value::Bool(spec.secret));
    entry.insert(
        "description".to_owned(),
        Value::String(spec.description.clone()),
    );
    entry.insert("configured".to_owned(), Value::Bool(configured));

    let value = if spec.secret {
        // SECURITY: presence only. The stored value does not reach this expression, so no edit to
        // the surrounding code can start echoing it by accident.
        if configured {
            Value::String(REDACTED.to_owned())
        } else {
            Value::Null
        }
    } else {
        stored
            .cloned()
            .or_else(|| spec.default.clone())
            .unwrap_or(Value::Null)
    };
    entry.insert("value".to_owned(), value);

    // A default reaches the client only for a value the client may read. For a secret it is dropped
    // rather than redacted, because "there is a default secret" is itself a hint worth withholding.
    if !spec.secret {
        if let Some(default) = &spec.default {
            entry.insert("default".to_owned(), default.clone());
        }
    }

    Value::Object(entry)
}

// ---- name rules -------------------------------------------------------------------------------

/// Why `name` cannot be a namespace or a key, or `None` when it can.
///
/// These restate [`crate::settings_rpc`]'s key rules rather than calling them, because that module
/// keeps its validator private. They matter here for a different reason than at the write boundary:
/// [`SchemaRegistry::describe`] builds a store key out of a namespace and a key, so a declaration
/// containing a path separator or `..` would aim a read at something the deployment did not name.
/// A dot is allowed, since dots are this crate's own convention for a flat namespace.
fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("it is empty, which addresses nothing");
    }
    if name.trim() != name {
        return Some("it has leading or trailing whitespace, so two names would look alike");
    }
    if name.contains("..") {
        return Some(
            "it contains `..`, which a file-backed store may resolve as a parent directory",
        );
    }
    if name.contains('/') || name.contains('\\') {
        return Some("it contains a path separator, and a setting name is not a path");
    }
    if name.chars().any(char::is_control) {
        return Some("it contains a control character, which no log or terminal can show");
    }
    None
}

// ---- type checks ------------------------------------------------------------------------------

/// The type names a schema fragment accepts, or an empty list when it makes no type claim.
///
/// JSON Schema allows `type` to be one name or a list of names; both forms are read, so a
/// declaration copied from a schema that uses the list form is not silently unchecked.
fn declared_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Whether `value` is of the JSON type called `type_name`.
///
/// An unrecognised name is accepted: it is a vocabulary this build does not implement (a deployment
/// may extend a schema with a UI-only type), and refusing every value under it would reject writes
/// the deployment's own reader accepts. The missing check is visible in the declaration, which is
/// the fragment the client is told to render.
fn type_accepts(type_name: &str, value: &Value) -> bool {
    match type_name {
        "string" => value.is_string(),
        "number" => value.is_number(),
        // A JSON number with no fractional part is an integer under JSON Schema, and a client that
        // serialises `3` as `3.0` must not be refused for the wire form it chose.
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value
                    .as_f64()
                    .is_some_and(|number| number.fract().abs() < f64::EPSILON)
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

/// The JSON kind of a value, for an error message that must not repeat the value.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---- errors -----------------------------------------------------------------------------------

/// Why a declaration or a value was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    /// The namespace or key cannot address a setting.
    #[error("`{namespace}.{key}` cannot be declared: {reason}")]
    InvalidDeclaration {
        /// The namespace that was being declared.
        namespace: String,
        /// The key that was being declared.
        key: String,
        /// The rule that was broken, phrased to be shown to a caller.
        reason: &'static str,
    },

    /// A setting with this namespace and key is already declared.
    #[error("`{namespace}.{key}` is already declared, and a key has one declaration")]
    DuplicateDeclaration {
        /// The namespace that was declared twice.
        namespace: String,
        /// The key that was declared twice.
        key: String,
    },

    /// No declaration covers this namespace and key, so there is nothing to check against.
    #[error("`{namespace}.{key}` is not declared, so this registry cannot validate it")]
    Undeclared {
        /// The namespace that was looked up.
        namespace: String,
        /// The key that was looked up.
        key: String,
    },

    /// The value is not one of the types the declaration accepts.
    ///
    /// The variant carries the JSON *kind* of the value, never the value, because a refusal may be
    /// logged and the value may be a secret.
    #[error("`{namespace}.{key}` is declared as {expected}, but the value is {actual}")]
    TypeMismatch {
        /// The namespace of the declaration.
        namespace: String,
        /// The key of the declaration.
        key: String,
        /// The accepted type names, joined for display.
        expected: String,
        /// The JSON kind of the value that was supplied.
        actual: &'static str,
    },
}
