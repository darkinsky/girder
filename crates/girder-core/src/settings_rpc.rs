//! The `settings.*` Remote methods: reading and writing configuration values by key.
//!
//! Upstream serves settings from `packages/api/settings-controller`, which registers **eight**
//! `@Remote` methods across two namespaces: five on `settings` (`describe`, `update`, `replace`,
//! `mutate`, `openSettingsDocument`) and three on `credentials` (`describe`, `set`, `unset`). None
//! of them existed here, so a client that opened a configuration page had nothing to call.
//!
//! This module closes the part of that gap the crate can honour. It is a key/value surface over
//! [`SettingsStore`]: one stored value per key, with the read/write split the access layer needs.
//!
//! # What is implemented, and what upstream method each one answers
//!
//! | here | upstream | note |
//! |---|---|---|
//! | `settings.get` | — | upstream has no per-key read; it describes whole namespaces |
//! | `settings.set` | — | same: upstream writes a namespace's user section, not one key |
//! | `settings.list` | — | the keys this store holds, for a client that renders a flat list |
//! | `settings.delete` | — | the removal half of `set` |
//! | `settings.update` | `update(ns, patch, expectedRevision)` | shallow merge into the stored object |
//! | `settings.replace` | `replace(ns, section, expectedRevision)` | wholesale replacement of it |
//! | `settings.mutate` | `mutate(ns, ops, expectedRevision)` | path-addressed `set`/`unset` edits |
//!
//! # What is **not** implemented, and why
//!
//! - `settings.describe` — upstream answers with one redacted view per namespace carrying the
//!   serialized `schemastery` schema, the resolved layered value, the composition base layer, the
//!   secret slots, and a revision. A key/value store has no schema, no layers, and no revision, so
//!   the honest answer is "not implemented" rather than a view whose `schema` a client would try to
//!   rehydrate. A client that renders its own form from [`list`] does not need it.
//! - `settings.openSettingsDocument` — upstream materializes the provider-owned settings *document*
//!   and opens it in a native text editor. There is no document behind this store and no editor
//!   integration in this crate, so there is nothing to open.
//! - The three `credentials.*` methods — a different namespace and a different seam (secret
//!   references, not settings values). They belong with [`crate::credentials`], not here.
//!
//! Declaring a method implemented that is not would be worse than the gap: a client would render a
//! form from fields that do not exist and write values nothing reads. The gaps are recorded through
//! [`RemoteRegistry::declare_unimplemented`], so a caller gets `NOT_IMPLEMENTED` naming the method
//! instead of a transport-level 404.
//!
//! # Storage is a seam, and the default one is volatile
//!
//! [`SettingsStore`] is the only place a value is kept. Two implementations ship:
//!
//! - [`MemorySettingsStore`] — a process-local map. **Every value is lost when the process exits.**
//!   It exists so the methods are testable and so a host can accept writes before it has durable
//!   storage; it is not persistence, and nothing here writes a file.
//! - [`UnavailableSettingsStore`] — refuses every write and reports that storage is missing, which
//!   is what [`register_settings_methods`] installs when the caller supplies nothing. Reporting the
//!   gap beats pretending: a client told "saved" against a store that cannot keep the value would
//!   show a setting that vanishes on the next read.
//!
//! A host with a database or a config file passes its own implementation; no method here assumes
//! the map, and nothing in this module reads or writes a path.
//!
//! # Why a key is validated rather than stored as sent
//!
//! A settings key reaches a store that a host may back with a file, a directory, or a column name.
//! `../../etc/passwd`, `a/b`, and the empty string are all plausible-looking keys that a
//! filesystem-backed store would resolve somewhere the caller did not name, so they are refused at
//! the boundary by [`validate_key`] with a message naming the rule:
//!
//! | refused | why |
//! |---|---|
//! | empty, or only whitespace | nothing to address; also the shape a missing field takes |
//! | leading or trailing whitespace | `"a "` and `"a"` would be two keys nothing distinguishes |
//! | contains `..` | a store may read it as a parent directory |
//! | contains `/` or `\` | a store may read it as a path |
//! | contains a control character | unprintable in every log and terminal that has to show it |
//! | longer than [`MAX_KEY_LEN`] | a key is a name, not a document |
//!
//! Dotted keys such as `provider.model` are deliberately allowed: they are this crate's own
//! convention for a flat namespace (see [`crate::api_config::SettingView`]), and a dot inside a
//! segment is not a path traversal.
//!
//! # Why each method declares its own access
//!
//! `get` and `list` declare `read`; `set`, `update`, `replace`, and `mutate` declare `write`;
//! `delete` declares `destructive`, because a removed value cannot be restored — only re-created
//! from whatever the caller still remembers. Declaring everything `write` would be one line shorter
//! and would break a read-only mirror twice: its reads would be refused, and the refusal would name
//! the wrong reason. A method with no declaration is treated as a read of its namespace, which for
//! `set` would let a read-only peer write.
//!
//! # Optimistic concurrency is refused rather than ignored
//!
//! Upstream's three write modes take an `expectedRevision` and fail a stale writer instead of
//! overwriting a concurrent change. [`SettingsStore`] keeps no revisions, so a supplied
//! `expectedRevision` is refused with a message saying so. Silently ignoring it would be the one
//! failure a client cannot see: it would believe a conflict was detected when the last writer won.
//!
//! # Responses omit what the store cannot answer
//!
//! Upstream's write modes answer with a namespace view (`schema`, `value`, `base`, `user`,
//! `applies`, `secrets`, `revision`). These answer with `{ns, value}`, and the per-key methods with
//! `{key, value}`. A field that was fabricated — a `revision` of `0` on a store that has none —
//! would be read as a fact by the next client, so it is absent instead.
//!
//! # Wire shapes
//!
//! ```text
//! settings.get      {key}                           -> {key, value}
//! settings.set      {key, value}                    -> {key, value}
//! settings.list     {}                              -> {items: [{key, value}], total}
//! settings.delete   {key}                           -> {key, deleted}
//! settings.update   {ns, patch, expectedRevision?}  -> {ns, value}
//! settings.replace  {ns, section, expectedRevision?} -> {ns, value}
//! settings.mutate   {ns, ops, expectedRevision?}    -> {ns, value}
//! ```
//!
//! A read of a key that was never set is an error naming the key, not a miss-shaped success
//! (`{value: null}`). The crate makes the same choice in [`crate::schedule_rpc`], for the same
//! reason: a client that forgets to check a "found" flag would render an absent value as a null one,
//! and `null` is a value a setting may legitimately hold.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use girder_core::{Declaration, RemoteCall, RemoteError, RemoteRegistry};

// ---- surface constants ------------------------------------------------------------------------

/// The namespace every method here is served under.
pub const NAMESPACE: &str = "settings";

/// Reads one key.
pub const GET_METHOD: &str = "get";

/// Writes one key.
pub const SET_METHOD: &str = "set";

/// Lists every key.
pub const LIST_METHOD: &str = "list";

/// Removes one key.
pub const DELETE_METHOD: &str = "delete";

/// Merges a patch into one namespace's stored object.
pub const UPDATE_METHOD: &str = "update";

/// Replaces one namespace's stored object wholesale.
pub const REPLACE_METHOD: &str = "replace";

/// Applies path-addressed edits to one namespace's stored object.
pub const MUTATE_METHOD: &str = "mutate";

/// Describes every namespace for a configuration page. Not implemented; see the module docs.
pub const DESCRIBE_METHOD: &str = "describe";

/// Opens the provider-owned settings document in a native editor. Not implemented; see the module
/// docs.
pub const OPEN_DOCUMENT_METHOD: &str = "openSettingsDocument";

/// The upstream methods this build records as gaps.
///
/// Exported rather than inlined into the registration so a list builder can report the gap without
/// duplicating the names, and so a test can assert the gap is exactly these two.
pub const UNIMPLEMENTED: &[&str] = &[DESCRIBE_METHOD, OPEN_DOCUMENT_METHOD];

/// The resource kind these methods touch, as reported to an access policy.
const RESOURCE_KIND: &str = "settings";

/// The longest key this build accepts, in bytes.
///
/// A bound rather than no bound: a key becomes a map entry, a database column, or a file name
/// depending on the store, and an unbounded one lets a single call allocate past what any of those
/// can hold.
pub const MAX_KEY_LEN: usize = 256;

/// The deepest path `mutate` accepts, in segments.
///
/// The path walk is recursive, so the depth is a stack bound as well as a sanity bound: a caller
/// sending ten thousand segments would otherwise be choosing how much stack this server uses.
pub const MAX_PATH_DEPTH: usize = 32;

// ---- key validation ---------------------------------------------------------------------------

/// Checks that one key is usable, answering it unchanged when it is.
///
/// The rules are the table in the module documentation. They are checked here, at the boundary,
/// rather than inside a store, so every store — including one a host supplies later — receives a key
/// that is safe to use as a file name, a directory entry, or a column.
fn validate_key(key: &str) -> Result<&str, String> {
    if key.is_empty() {
        return Err("a settings key must not be empty".to_owned());
    }
    if key.trim().is_empty() {
        return Err(format!("`{key}` is only whitespace, which addresses nothing"));
    }
    if key.trim() != key {
        return Err(format!(
            "`{key}` has leading or trailing whitespace, which a store may keep or strip, making \
             it two keys nothing distinguishes"
        ));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(format!(
            "a settings key must be at most {MAX_KEY_LEN} bytes, got {}",
            key.len()
        ));
    }
    // `..` anywhere, not just as a whole segment: a store that is not a POSIX filesystem may still
    // normalise it, and treating `a..b` and `a/b` differently is a distinction no caller needs.
    if key.contains("..") {
        return Err(format!(
            "`{key}` contains `..`, which a file-backed store may resolve as a parent directory"
        ));
    }
    if key == "." {
        return Err("`.` is the current directory, not a settings key".to_owned());
    }
    if key.contains('/') || key.contains('\\') {
        return Err(format!(
            "`{key}` contains a path separator; a settings key names one value, never a path"
        ));
    }
    if key.chars().any(char::is_control) {
        return Err(format!(
            "`{key}` contains a control character, which no log or terminal can show"
        ));
    }
    Ok(key)
}

// ---- storage seam -----------------------------------------------------------------------------

/// Where settings values are kept.
///
/// Every method is `&self` and the trait is `Send + Sync`, because a handler is called from many
/// requests at once: a store that needed `&mut self` would force the registry to serialise every
/// settings call. Implementations provide their own interior mutability.
///
/// The seam is deliberately value-shaped rather than namespace-shaped: a store knows how to keep a
/// value under a key and nothing about merging, patching, or revisions. Those live in the handlers
/// below, so every store gets the same semantics.
pub trait SettingsStore: Send + Sync {
    /// Reads one key, or `None` when nothing is stored under it.
    fn get(&self, key: &str) -> Option<Value>;

    /// Writes one key.
    ///
    /// An `Err` means nothing was stored, so a caller must not report success. The message is shown
    /// to the caller, so it should say what the storage said.
    fn set(&self, key: &str, value: Value) -> Result<(), String>;

    /// Every stored key with its value, in an order the store chooses.
    ///
    /// The order is not a contract: [`list`] sorts, because a store that is not a sorted map cannot
    /// promise a stable order cheaply, and a page that reordered between calls would show rows
    /// twice.
    fn list(&self) -> Vec<(String, Value)>;

    /// Removes one key.
    ///
    /// `Ok(true)` when it was there and is now gone, `Ok(false)` when nothing held it. The caller
    /// distinguishes the two so a delete of an unknown key can be refused rather than reported as a
    /// success.
    ///
    /// The default refuses instead of doing nothing: a store written against an earlier version of
    /// this trait keeps compiling, and its `delete` fails with a message that names the gap rather
    /// than reporting a removal that never happened.
    fn remove(&self, key: &str) -> Result<bool, String> {
        Err(format!(
            "this store keeps no per-key removal, so `{key}` cannot be deleted"
        ))
    }
}

/// A store that keeps values in this process's memory.
///
/// **Lost on restart.** The map lives in the process, so every value disappears when the process
/// exits. It is the store for tests and for a host that wants the surface to work before it has
/// durable storage — not persistence. Nothing in this module writes the map anywhere.
#[derive(Debug, Default)]
pub struct MemorySettingsStore {
    values: Mutex<BTreeMap<String, Value>>,
}

impl MemorySettingsStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many keys are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // Poisoning is recovered rather than propagated: a panic while holding this map leaves a table
    // of values, not a half-written one, and refusing every later call would turn one panic into a
    // permanently unusable settings surface.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Value>> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SettingsStore for MemorySettingsStore {
    fn get(&self, key: &str) -> Option<Value> {
        self.lock().get(key).cloned()
    }

    fn set(&self, key: &str, value: Value) -> Result<(), String> {
        self.lock().insert(key.to_owned(), value);
        Ok(())
    }

    fn list(&self) -> Vec<(String, Value)> {
        self.lock()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn remove(&self, key: &str) -> Result<bool, String> {
        Ok(self.lock().remove(key).is_some())
    }
}

/// The store a deployment gets when it configures none.
///
/// Every write is refused and the reason names what is missing, following
/// [`crate::schedule_rpc::UnavailableScheduleStore`]: the alternative — silently keeping the value
/// in memory — would report a state the next call cannot confirm.
#[derive(Debug)]
pub struct UnavailableSettingsStore {
    reason: String,
}

impl UnavailableSettingsStore {
    /// Creates a store that refuses with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The reason the default registration uses.
    ///
    /// It names the call a host makes to fix it, because a message that only said "not supported"
    /// would leave an operator guessing which half to supply.
    #[must_use]
    pub fn default_reason() -> Self {
        Self::new(
            "no SettingsStore was supplied; this module ships only an in-process store whose values \
             vanish on restart, so a host must pass its own Arc<dyn SettingsStore> to \
             register_settings_methods",
        )
    }
}

impl SettingsStore for UnavailableSettingsStore {
    fn get(&self, _key: &str) -> Option<Value> {
        // `None` rather than a refusal, because the signature has no error channel. The refusal a
        // caller actually sees comes from the write path or from the read handler's miss message,
        // both of which carry this reason.
        None
    }

    fn set(&self, _key: &str, _value: Value) -> Result<(), String> {
        Err(self.reason.clone())
    }

    fn list(&self) -> Vec<(String, Value)> {
        // Empty for the same reason as `get`: this store holds nothing, so nothing is what it has.
        Vec::new()
    }

    fn remove(&self, _key: &str) -> Result<bool, String> {
        Err(self.reason.clone())
    }
}

// ---- registration -----------------------------------------------------------------------------

/// Registers the `settings.*` methods against `store`, and records the upstream gaps.
///
/// Declarations are registered beside each handler so the policy judges a call before it runs; the
/// store is cloned into each closure so the host keeps ownership of it and the registry stays the
/// only thing this function mutates.
pub fn register_settings_methods(registry: &mut RemoteRegistry, store: &Arc<dyn SettingsStore>) {
    registry.declare(NAMESPACE, GET_METHOD, Declaration::read(RESOURCE_KIND));
    registry.declare(NAMESPACE, LIST_METHOD, Declaration::read(RESOURCE_KIND));
    registry.declare(NAMESPACE, SET_METHOD, Declaration::write(RESOURCE_KIND));
    registry.declare(NAMESPACE, UPDATE_METHOD, Declaration::write(RESOURCE_KIND));
    registry.declare(NAMESPACE, REPLACE_METHOD, Declaration::write(RESOURCE_KIND));
    registry.declare(NAMESPACE, MUTATE_METHOD, Declaration::write(RESOURCE_KIND));
    // A deleted value is gone; the store keeps no tombstone, so the only way back is for the caller
    // to write it again from what it remembers. That is `destructive` by the definition in
    // `crate::access`.
    registry.declare(
        NAMESPACE,
        DELETE_METHOD,
        Declaration::destructive(RESOURCE_KIND),
    );

    registry.declare_unimplemented(NAMESPACE, UNIMPLEMENTED);

    let get_store = Arc::clone(store);
    registry.register(NAMESPACE, GET_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&get_store);
        Box::pin(async move { get(&call.args, &store) })
    });

    let set_store = Arc::clone(store);
    registry.register(NAMESPACE, SET_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&set_store);
        Box::pin(async move { set(&call.args, &store) })
    });

    let list_store = Arc::clone(store);
    registry.register(NAMESPACE, LIST_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&list_store);
        Box::pin(async move { list(&store) })
    });

    let delete_store = Arc::clone(store);
    registry.register(NAMESPACE, DELETE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&delete_store);
        Box::pin(async move { delete(&call.args, &store) })
    });

    let update_store = Arc::clone(store);
    registry.register(NAMESPACE, UPDATE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&update_store);
        Box::pin(async move { update(&call.args, &store) })
    });

    let replace_store = Arc::clone(store);
    registry.register(NAMESPACE, REPLACE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&replace_store);
        Box::pin(async move { replace(&call.args, &store) })
    });

    let mutate_store = Arc::clone(store);
    registry.register(NAMESPACE, MUTATE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&mutate_store);
        Box::pin(async move { mutate(&call.args, &store) })
    });
}

// ---- requests ---------------------------------------------------------------------------------

/// A body carrying only a key.
#[derive(Debug, Deserialize)]
struct KeyRequest {
    /// The key.
    #[serde(default)]
    key: Option<String>,
}

/// The body of `settings.set`.
///
/// `value` is not an `Option`: an absent one is a parse error naming the field, which is what the
/// caller needs, while an explicit `null` is a value a setting may legitimately hold.
#[derive(Debug, Deserialize)]
struct SetRequest {
    /// The key.
    #[serde(default)]
    key: Option<String>,
    /// The value.
    value: Value,
}

/// The body of `settings.update`, `settings.replace`, and `settings.mutate`.
///
/// One struct for three methods because the three differ only in what they do with a section, and
/// three copies of `ns` plus `expectedRevision` would be three chances for the spelling to drift.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteRequest {
    /// The namespace, which is a key in this store.
    #[serde(default)]
    ns: Option<String>,
    /// The fields to merge into the stored section; used by `update`.
    #[serde(default)]
    patch: Option<Map<String, Value>>,
    /// The complete replacement section; used by `replace`.
    #[serde(default)]
    section: Option<Map<String, Value>>,
    /// The path-addressed edits; used by `mutate`.
    #[serde(default)]
    ops: Option<Vec<Value>>,
    /// The revision the caller read.
    ///
    /// Refused when present: this store keeps no revisions, so a stale write cannot be detected.
    /// See the module documentation.
    #[serde(default)]
    expected_revision: Option<u64>,
}

/// One path-addressed edit, in upstream's `SettingsPathOpView` shape.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum PathOp {
    /// Writes `value` at `path`, creating intermediate objects.
    Set {
        /// Segments from the section root; empty addresses the root itself.
        path: Vec<String>,
        /// What to write.
        value: Value,
    },
    /// Removes whatever is at `path`.
    Unset {
        /// Segments from the section root; empty addresses the root itself.
        path: Vec<String>,
    },
}

// ---- handlers ---------------------------------------------------------------------------------
//
// The handlers below are plain functions, not `async fn`s: every store call is synchronous, so there
// is nothing to await, and clippy's `unused_async` is right that the keyword would be noise. The
// futures lie one layer up, in the registration closures, which is the only place the
// `RemoteMethod` type requires one.

/// Runs one `settings.get`.
fn get(args: &Value, store: &Arc<dyn SettingsStore>) -> Result<Value, RemoteError> {
    let request: KeyRequest = parse_request(args, GET_METHOD)?;
    let key = require_key(&request, GET_METHOD)?;
    let Some(value) = store.get(&key) else {
        return Err(missing(GET_METHOD, &key));
    };
    Ok(json!({"key": key, "value": value}))
}

/// Runs one `settings.set`.
fn set(args: &Value, store: &Arc<dyn SettingsStore>) -> Result<Value, RemoteError> {
    let request: SetRequest = parse_request(args, SET_METHOD)?;
    let key = require_key_from(request.key.as_deref(), SET_METHOD)?;
    store
        .set(&key, request.value.clone())
        .map_err(|error| store_failure(SET_METHOD, &key, &error))?;
    Ok(json!({"key": key, "value": request.value}))
}

/// Runs one `settings.list`.
///
/// Nothing is paged: a settings surface is expected to be small, and a page a client forgot to follow
/// would hide a key it then reports as unset. If a deployment ever holds more than a form's worth,
/// the bound belongs to the store that knows its own size.
fn list(store: &Arc<dyn SettingsStore>) -> Result<Value, RemoteError> {
    // Sorted here rather than by the store: the trait promises no order, and a listing that
    // reordered between two calls would move rows under a reader.
    let mut entries = store.list();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    let items: Vec<Value> = entries
        .into_iter()
        .map(|(key, value)| json!({"key": key, "value": value}))
        .collect();
    Ok(json!({"items": items, "total": items.len()}))
}

/// Runs one `settings.delete`.
fn delete(args: &Value, store: &Arc<dyn SettingsStore>) -> Result<Value, RemoteError> {
    let request: KeyRequest = parse_request(args, DELETE_METHOD)?;
    let key = require_key(&request, DELETE_METHOD)?;

    // Existence is checked before the removal so a delete of a key nobody holds is refused. A store
    // that reports `false` here and one that reports `true` for a key that never existed are
    // indistinguishable from the response, so the check is the only place that can be honest.
    if store.get(&key).is_none() {
        return Err(missing(DELETE_METHOD, &key));
    }
    let removed = store
        .remove(&key)
        .map_err(|error| store_failure(DELETE_METHOD, &key, &error))?;
    if !removed {
        // The key was read a moment ago and is gone now: another caller deleted it first. Reported
        // rather than swallowed, because the two requests meant different things.
        return Err(missing(DELETE_METHOD, &key));
    }
    Ok(json!({"key": key, "deleted": true}))
}

/// Runs one `settings.update`.
fn update(args: &Value, store: &Arc<dyn SettingsStore>) -> Result<Value, RemoteError> {
    let request: WriteRequest = parse_request(args, UPDATE_METHOD)?;
    let ns = require_namespace(request.ns.as_deref(), UPDATE_METHOD)?;
    refuse_revision(request.expected_revision, UPDATE_METHOD)?;
    let Some(patch) = request.patch else {
        return Err(invalid(UPDATE_METHOD, "`patch` is required"));
    };

    let mut current = match store.get(&ns) {
        None => Map::new(),
        Some(Value::Object(map)) => map,
        Some(other) => return Err(not_a_section(UPDATE_METHOD, &ns, &other)),
    };
    // Shallow merge, which is what upstream's `update` means by "merge a patch into one namespace's
    // stored user section": a nested object in the patch replaces the stored one rather than being
    // merged into it. A deep merge would be a second, unstated rule about arrays and nulls.
    for (field, value) in patch {
        current.insert(field, value);
    }
    store_section(store, UPDATE_METHOD, &ns, Value::Object(current))
}

/// Runs one `settings.replace`.
fn replace(args: &Value, store: &Arc<dyn SettingsStore>) -> Result<Value, RemoteError> {
    let request: WriteRequest = parse_request(args, REPLACE_METHOD)?;
    let ns = require_namespace(request.ns.as_deref(), REPLACE_METHOD)?;
    refuse_revision(request.expected_revision, REPLACE_METHOD)?;
    let Some(section) = request.section else {
        return Err(invalid(REPLACE_METHOD, "`section` is required"));
    };
    store_section(store, REPLACE_METHOD, &ns, Value::Object(section))
}

/// Runs one `settings.mutate`.
fn mutate(args: &Value, store: &Arc<dyn SettingsStore>) -> Result<Value, RemoteError> {
    let request: WriteRequest = parse_request(args, MUTATE_METHOD)?;
    let ns = require_namespace(request.ns.as_deref(), MUTATE_METHOD)?;
    refuse_revision(request.expected_revision, MUTATE_METHOD)?;
    let Some(raw_ops) = request.ops else {
        return Err(invalid(MUTATE_METHOD, "`ops` is required"));
    };

    let mut section = match store.get(&ns) {
        None => Value::Object(Map::new()),
        Some(value) => value,
    };
    // The edits are parsed here rather than into `WriteRequest` directly so a malformed op is
    // reported against `mutate` with the op's position, which is what a caller needs to find it.
    for (index, raw) in raw_ops.into_iter().enumerate() {
        let op: PathOp = serde_json::from_value(raw).map_err(|error| {
            invalid(
                MUTATE_METHOD,
                format!("`ops[{index}]` is not a path edit: {error}"),
            )
        })?;
        apply_op(&mut section, &op)
            .map_err(|message| invalid(MUTATE_METHOD, format!("`ops[{index}]`: {message}")))?;
    }
    store_section(store, MUTATE_METHOD, &ns, section)
}

// ---- path edits -------------------------------------------------------------------------------

/// Applies one path-addressed edit to `section` in place.
///
/// The walk is recursive rather than a loop of `&mut` cursors, because each step hands the rest of
/// the path to a narrower borrow: a loop would need to reborrow the cursor it is assigning to.
fn apply_op(section: &mut Value, op: &PathOp) -> Result<(), String> {
    match op {
        PathOp::Set { path, value } => {
            validate_path(path)?;
            set_at(section, path, value.clone())
        }
        PathOp::Unset { path } => {
            validate_path(path)?;
            unset_at(section, path)
        }
    }
}

/// Writes `value` at `path`, creating intermediate objects.
fn set_at(target: &mut Value, path: &[String], value: Value) -> Result<(), String> {
    let Some((head, rest)) = path.split_first() else {
        // The empty path addresses the section root: the section itself is replaced.
        *target = value;
        return Ok(());
    };
    let Some(map) = target.as_object_mut() else {
        return Err(format!(
            "`{}` is not an object, so `{head}` cannot be written into it",
            describe(path, head)
        ));
    };
    // Created rather than refused, matching upstream's "creating intermediate objects": a client
    // setting `a.b.c` on an empty section means the three-level object it named.
    let child = map
        .entry(head.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    set_at(child, rest, value)
}

/// Removes whatever is at `path`.
fn unset_at(target: &mut Value, path: &[String]) -> Result<(), String> {
    let Some((head, rest)) = path.split_first() else {
        // Removing the root empties the section rather than deleting it: the namespace keeps
        // existing, so a later write does not have to know it once did.
        *target = Value::Object(Map::new());
        return Ok(());
    };
    let Some(map) = target.as_object_mut() else {
        return Err(format!(
            "`{}` is not an object, so `{head}` cannot be removed from it",
            describe(path, head)
        ));
    };
    if rest.is_empty() {
        map.remove(head);
        return Ok(());
    }
    match map.get_mut(head) {
        Some(child) => unset_at(child, rest),
        // Absent is not an error: `unset` asks for a state, and that state already holds. Reporting
        // a miss would force every caller to read the section before removing from it.
        None => Ok(()),
    }
}

/// Checks one path's segments and depth.
///
/// Each segment goes through [`validate_key`], because a segment becomes a JSON object key that a
/// store may later use as a name: the same traversal and separator rules apply one level down.
fn validate_path(path: &[String]) -> Result<(), String> {
    if path.len() > MAX_PATH_DEPTH {
        return Err(format!(
            "a path may be at most {MAX_PATH_DEPTH} segments deep, got {}",
            path.len()
        ));
    }
    for segment in path {
        validate_key(segment).map_err(|message| format!("path segment {message}"))?;
    }
    Ok(())
}

/// Names the object a path step could not enter, for the message.
fn describe(path: &[String], head: &str) -> String {
    let prefix = path
        .iter()
        .take_while(|segment| segment.as_str() != head)
        .cloned()
        .collect::<Vec<String>>()
        .join(".");
    if prefix.is_empty() {
        "the section root".to_owned()
    } else {
        prefix
    }
}

// ---- helpers ----------------------------------------------------------------------------------

/// Parses one method's arguments into its request type.
fn parse_request<T>(args: &Value, method: &str) -> Result<T, RemoteError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(args.clone()).map_err(|error| RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: error.to_string(),
    })
}

/// Extracts `key`, refusing a missing, blank, or unusable one.
fn require_key(request: &KeyRequest, method: &str) -> Result<String, RemoteError> {
    require_key_from(request.key.as_deref(), method)
}

/// Validates a raw key, reporting which rule it broke.
///
/// A blank key is reported as missing rather than as invalid: an empty string is a client bug, and
/// telling it "no such setting" would send the caller looking for a value instead of a field.
fn require_key_from(raw: Option<&str>, method: &str) -> Result<String, RemoteError> {
    match raw {
        Some(key) if !key.trim().is_empty() => validate_key(key)
            .map(str::to_owned)
            .map_err(|message| invalid(method, message)),
        _ => Err(invalid(method, "`key` is required")),
    }
}

/// Validates a namespace, which is a key in this store.
fn require_namespace(raw: Option<&str>, method: &str) -> Result<String, RemoteError> {
    match raw {
        Some(ns) if !ns.trim().is_empty() => {
            validate_key(ns).map(str::to_owned).map_err(|message| {
                invalid(method, format!("`ns` is not a usable namespace: {message}"))
            })
        }
        _ => Err(invalid(method, "`ns` is required")),
    }
}

/// Refuses a supplied `expectedRevision`.
///
/// Ignoring it would be the one failure a client cannot see: it would believe a stale write was
/// detected when the last writer simply won.
fn refuse_revision(expected: Option<u64>, method: &str) -> Result<(), RemoteError> {
    match expected {
        None => Ok(()),
        Some(revision) => Err(invalid(
            method,
            format!(
                "`expectedRevision` ({revision}) cannot be honoured: this store keeps no revisions, \
                 so a stale write cannot be detected. Omit it to write unconditionally, and do not \
                 send it again until the store reports one."
            ),
        )),
    }
}

/// Stores a section and answers the shape every write mode returns.
fn store_section(
    store: &Arc<dyn SettingsStore>,
    method: &str,
    ns: &str,
    value: Value,
) -> Result<Value, RemoteError> {
    store
        .set(ns, value.clone())
        .map_err(|error| store_failure(method, ns, &error))?;
    Ok(json!({"ns": ns, "value": value}))
}

/// The error a stored value reports when a write mode needed an object and found something else.
fn not_a_section(method: &str, ns: &str, value: &Value) -> RemoteError {
    invalid(
        method,
        format!(
            "`{ns}` holds a {}, not an object, so it cannot be patched or mutated; use `{SET_METHOD}` \
             to replace the whole value",
            type_name(value)
        ),
    )
}

/// The JSON type of one value, for a message.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The error a read or delete of an unknown key reports.
fn missing(method: &str, key: &str) -> RemoteError {
    invalid(method, format!("no setting `{key}`"))
}

/// An invalid-argument error for `method`.
fn invalid(method: &str, message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: message.into(),
    }
}

/// Turns a store failure into an RPC failure that says what the store said.
///
/// `Failed` rather than `InvalidArguments`: the caller's request was fine and the deployment is what
/// is missing, which is the distinction a client needs to stop retrying for the first case and keep
/// trying for the second.
fn store_failure(method: &str, key: &str, error: &str) -> RemoteError {
    RemoteError::Failed(format!(
        "`{NAMESPACE}.{method}` could not keep `{key}`: {error}"
    ))
}
