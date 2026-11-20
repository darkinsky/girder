//! The Typert runtime registry: the part of `packages/typert` that is *not* the RPC protocol.
//!
//! # Why this gap
//!
//! Both candidate packages were measured before choosing (production `src`, tests excluded):
//!
//! | package | lines | what it is |
//! |---|---|---|
//! | `packages/typert` | 11,328 | 3,471 `analyzer` + 1,234 `emitter` + 1,069 `cordis-catalog` compiler; 957 protocol types; 817 runtime registry; 507 loader; 1,840 small modules |
//! | `packages/util` | 7,916 | 38 unrelated leaf utilities (`time` 1,654, `native-command` 797, `http-proxy` 711, `plugin-detail` 472, …) |
//!
//! `packages/util` *looks* smaller but is far better covered locally: `atomic_write.rs`, `brand.rs`,
//! `chunked_list.rs`, `code_language.rs`, `home_paths.rs`, `launch_environment.rs`, `time.rs`,
//! `workspace_path.rs`, `retention.rs` and `json.rs` already port eleven of the substantial
//! sub-packages, and `util_gap.rs` adds `PartialArguments` from `values` on top. What remains there
//! is either *inherently Node* (`http-proxy` installs a patched `http` module, `native-command`
//! shells out to `explorer.exe`/`xdg-open`, `lazy-require` wraps `createRequire`) or
//! desktop-console plumbing with no Rust consumer.
//!
//! `packages/typert` is 653 (`typert.rs`) + 470 (`typert_json.rs`) lines locally — a *data model*
//! ([`girder_core::Schema`], [`girder_core::Method`], [`girder_core::Catalogue`]) and two emitters.
//! It has no *runtime* at all.
//!
//! # Is Typert the same protocol as `girder-rpc`?
//!
//! No, and the distinction is the whole reason this module exists. `girder-rpc` owns the **wire**:
//! frames, a request/response envelope, backpressure. Typert owns the **definition** of what may
//! cross that wire and the *lifecycle* of those definitions:
//!
//! - generation: `analyzer` (3,471 lines) reads TypeScript with the TypeScript compiler and derives
//!   invocation descriptors and Zod schemas. A Rust proc macro sees type *tokens*, not a type graph,
//!   so that half is not a gap to fill by porting — `tests/typert_derive.rs` pins what the local
//!   macro can recover.
//! - **runtime registry**: `registry/src/service.ts` (817 lines) keeps the generated artifacts —
//!   schema factories, package reflection models, local and Remote invocation descriptors, lookup
//!   providers, Host/Client Context adapters, resource kinds — indexed by stable keys, with *atomic
//!   contribution registration*, owner-scoped withdrawal, and change notification.
//! - protocol predicates: the `RemoteError` structural marker, the principal projection, the
//!   endpoint-segment grammar, the reserved `global` resource kind.
//!
//! The registry is the largest *portable* piece with no local counterpart, and the piece the rest of
//! the migration needs: without it a `#[remote]` descriptor exists but nothing looks one up by
//! endpoint, nothing rejects a duplicate endpoint, and nothing withdraws a plugin's contributions
//! when it unloads.
//!
//! # What a Rust registry must decide differently
//!
//! 1. **What a schema factory is.** Upstream stores `create: () => z.ZodType` and caches the
//!    materialized Zod schema. Rust has no Zod, and this crate's schema currency is
//!    [`girder_core::Schema`]. A factory here builds an [`ObjectSchema`], and the JSON Schema
//!    projection uses the *cached* value, so a factory runs at most once per registration — the
//!    property upstream asserts with `expect(create).toHaveBeenCalledOnce()`.
//! 2. **What "the registry's lifetime" means.** Upstream's `hasSeen` keeps a withdrawn endpoint
//!    visible for the life of the Cordis service, and a lookup key may never change its wire
//!    declaration during that life. In Rust the registry *is* the lifetime, so both are fields of
//!    [`TypertRegistry`].
//! 3. **What a resolver returns.** A `TypertLookupProvider.resolve` may return a value or a
//!    promise. A Rust [`LookupProvider`] returns a [`BoxFut`]: ready on first poll, but never
//!    forced to be synchronous, which is the honest translation of
//!    `T | undefined | Promise<T | undefined>`.
//!
//! # Failure containment
//!
//! Upstream reports a failing change listener to `ctx.logger.warn` and keeps notifying the rest.
//! There is no logger here, so [`TypertRegistry::take_warnings`] drains them: a host that never
//! looks loses nothing, and a test asserts containment without a logging mock.
//!
//! # Why `girder_core::` inside the module
//!
//! `typert_gap` is not declared in `lib.rs` yet — wiring belongs to a separate change. The test
//! compiles this exact source by `#[path]`, and under that inclusion `crate::` names the test
//! binary, so every path out of the module goes through the crate root's public re-exports.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use girder_core::{BoxFut, ObjectSchema};

// ---- identity keys ------------------------------------------------------------------------------

/// The separator between a package and a name inside a global key.
pub const KEY_SEPARATOR: char = '#';

/// The separator between a namespace and a method inside an endpoint.
pub const ENDPOINT_SEPARATOR: char = '/';

/// The built-in identity-less resource kind, reserved against declaration.
pub const GLOBAL_RESOURCE_KIND: &str = "global";

/// The universal Remote failure codes upstream declares in the protocol package itself.
///
/// Every domain merges more beside the code that throws them, so this is a *floor*, not a closed
/// set: [`RemoteErrorCode::Other`] carries a code this build does not know, which is what a client
/// receiving an unknown code must be able to represent.
pub const UNIVERSAL_ERROR_CODES: [&str; 3] = [
    "gateway/bad-request",
    "gateway/cancelled",
    "gateway/internal",
];

/// Compose the global key of one generated schema: `<package>#<name>`.
///
/// Upstream's `typertKey`. Nothing is validated here — registration is the rejecting boundary,
/// because a key built on the read path must be comparable with one built on the write path and a
/// throwing helper would be unusable for a lookup.
#[must_use]
pub fn schema_key(package: &str, name: &str) -> String {
    format!("{package}{KEY_SEPARATOR}{name}")
}

/// Compose the identity of one package-face model: `<package>#<face>`.
#[must_use]
pub fn package_key(package: &str, face: Face) -> String {
    format!("{package}{KEY_SEPARATOR}{}", face.as_str())
}

/// Compose the endpoint of one invocation: `<namespace>/<method>`.
#[must_use]
pub fn endpoint_of(namespace: &str, method: &str) -> String {
    format!("{namespace}{ENDPOINT_SEPARATOR}{method}")
}

// ---- names and faces ----------------------------------------------------------------------------

/// Which independently compiled side produced a contribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Face {
    /// The runtime that owns the resources.
    Host,
    /// The runtime that consumes them across a carrier.
    Client,
}

impl Face {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Client => "client",
        }
    }

    /// Parses the wire spelling.
    ///
    /// # Errors
    ///
    /// Returns [`TypertError::Face`] for anything other than `host` or `client`.
    pub fn parse(value: &str) -> Result<Self, TypertError> {
        match value {
            "host" => Ok(Self::Host),
            "client" => Ok(Self::Client),
            other => Err(TypertError::Face {
                value: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for Face {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

// ---- the endpoint grammar -----------------------------------------------------------------------

/// The characters an RPC endpoint segment may contain. Upstream `TYPERT_REMOTE_SEGMENT_PATTERN`.
const SEGMENT_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_$.-";

/// Tests one generated Remote name against the connection endpoint grammar.
///
/// Upstream's `isTypertRemoteSegment`: a namespace, method, lookup key or Context key must survive
/// the shared carrier unchanged. `.` and `..` pass the character test and are still refused,
/// because a path-shaped carrier resolves them to a different resource.
#[must_use]
pub fn is_remote_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.bytes().all(|byte| SEGMENT_CHARS.contains(&byte))
}

// ---- errors -------------------------------------------------------------------------------------

/// A reason a definition, provider, or contribution is refused.
///
/// Every variant names the *subject* the way upstream's messages do (`invocation id`, `namespace`,
/// …) because the message is what a generated-artifact author reads from a build log, and a code
/// that said only "invalid descriptor" would not point at the field that is wrong.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TypertError {
    /// A required identity or type symbol is the empty string.
    #[error("typert: invalid {subject} — must be nonempty")]
    Empty {
        /// The field that failed.
        subject: &'static str,
    },
    /// A name contains a character the carrier cannot transport, or is `.`/`..`.
    #[error(
        "typert: invalid {subject} \"{value}\" — must contain only RPC endpoint segment characters"
    )]
    Segment {
        /// The field that failed.
        subject: &'static str,
        /// The rejected value.
        value: String,
    },
    /// A non-segment identity contains the key separator.
    #[error("typert: invalid {subject} \"{value}\" — must be nonempty and must not contain \"#\"")]
    Key {
        /// The field that failed.
        subject: &'static str,
        /// The rejected value.
        value: String,
    },
    /// A contribution names a face that does not exist.
    #[error("typert: invalid face \"{value}\" — expected \"host\" or \"client\"")]
    Face {
        /// The rejected value.
        value: String,
    },
    /// An endpoint is already claimed.
    #[error("typert: {kind} endpoint \"{endpoint}\" is already registered")]
    DuplicateEndpoint {
        /// Which registry (`local` or `remote`).
        kind: &'static str,
        /// The repeated endpoint.
        endpoint: String,
    },
    /// An invocation id is already claimed.
    #[error("typert: {kind} invocation id \"{id}\" is already registered")]
    DuplicateId {
        /// Which registry.
        kind: &'static str,
        /// The repeated id.
        id: String,
    },
    /// Two parameters claim the same wire field, so one value would silently win.
    #[error("typert: invocation \"{id}\" repeats wire field \"{wire}\"")]
    RepeatedWireField {
        /// The invocation.
        id: String,
        /// The repeated wire field.
        wire: String,
    },
    /// A lookup parameter declares no lookup key, so it could never resolve.
    #[error("typert: invocation \"{id}\" lookup parameter \"{name}\" has no lookup key")]
    MissingLookup {
        /// The invocation.
        id: String,
        /// The parameter.
        name: String,
    },
    /// A lookup parameter claims it accepts an absent wire field.
    #[error("typert: invocation \"{id}\" lookup parameter \"{name}\" cannot accept undefined")]
    LookupAcceptsUndefined {
        /// The invocation.
        id: String,
        /// The parameter.
        name: String,
    },
    /// A scope projection does not select the invocation's only lookup parameter.
    #[error(
        "typert: invocation \"{id}\" scope wire \"{wire}\" must select its only lookup parameter"
    )]
    ScopeSelection {
        /// The invocation.
        id: String,
        /// The declared scope wire field.
        wire: String,
    },
    /// A Context receiver declared a direct scope projection, which has no receiver to project.
    #[error(
        "typert: invocation \"{id}\" Context receiver cannot declare a direct scope projection"
    )]
    ContextScope {
        /// The invocation.
        id: String,
    },
    /// The cancellation parameter is not the reserved final `signal`.
    #[error("typert: invocation \"{id}\" cancellation parameter must be \"signal\"")]
    Cancellation {
        /// The invocation.
        id: String,
    },
    /// A lookup provider is already registered under its key.
    #[error("typert: lookup \"{key}\" is already registered")]
    LookupRegistered {
        /// The key.
        key: String,
    },
    /// A lookup provider changed its wire declaration during the registry's lifetime.
    #[error("typert: lookup \"{key}\" changed its wire declaration during this registry lifetime")]
    LookupRedeclared {
        /// The key.
        key: String,
    },
    /// A lookup resolver is already configured for its key.
    #[error("typert: lookup \"{key}\" resolver is already configured")]
    LookupConfigured {
        /// The key.
        key: String,
    },
    /// A Context adapter is already registered for its key.
    #[error("typert: {kind} provider \"{key}\" is already registered")]
    ContextRegistered {
        /// `host-context` or `client-context`.
        kind: &'static str,
        /// The key.
        key: String,
    },
    /// A Host Context resolver is already configured for its key.
    #[error("typert: host-context \"{key}\" resolver is already configured")]
    ContextConfigured {
        /// The key.
        key: String,
    },
    /// A resource kind is declared twice.
    #[error("typert: resource kind \"{kind}\" is already registered")]
    ResourceRegistered {
        /// The kind.
        kind: String,
    },
    /// A domain tried to redefine the built-in `global` kind.
    #[error("typert: resource kind \"global\" is reserved for the built-in global resource")]
    GlobalReserved,
    /// A package face is already registered.
    #[error("typert: package face \"{key}\" is already registered")]
    PackageRegistered {
        /// The package-face key.
        key: String,
    },
    /// A schema is already registered under its key.
    #[error("typert: schema \"{key}\" is already registered")]
    SchemaRegistered {
        /// The schema key.
        key: String,
    },
    /// A Remote package is already mounted.
    #[error("typert: Remote package \"{package}\" is already registered")]
    RemotePackage {
        /// The package.
        package: String,
    },
    /// A schema key is not `<package>#<name>`.
    #[error("typert: invalid schema key \"{key}\" — expected \"<package>#<name>\"")]
    InvalidSchemaKey {
        /// The key.
        key: String,
    },
    /// The package is registered but contributes no schema of that name.
    #[error(
        "typert: cannot resolve \"{key}\" — package \"{package}\" is registered but contributes no schema named \"{name}\""
    )]
    UnknownSchema {
        /// The key.
        key: String,
        /// The package part.
        package: String,
        /// The name part.
        name: String,
    },
    /// No contribution registered that package at all.
    #[error(
        "typert: cannot resolve \"{key}\" — package \"{package}\" has no registered contribution"
    )]
    UnknownPackage {
        /// The key.
        key: String,
        /// The package part.
        package: String,
    },
}

// ---- RemoteError --------------------------------------------------------------------------------

/// A stable Remote failure code.
///
/// Upstream's `RemoteErrorCode` is a merge-extensible key union; a Rust enum cannot be merged into
/// from another crate, so the vocabulary keeps the three universal codes as variants and carries
/// everything else as [`RemoteErrorCode::Other`]. Discrimination is always by code, never by type,
/// which is exactly what that variant makes possible.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RemoteErrorCode {
    /// Owner-side business validation refused the request.
    BadRequest,
    /// The carrier or the backend cancelled the call.
    Cancelled,
    /// Carrier, dispatch, or unclassified Host failure.
    Internal,
    /// A code this build does not declare.
    Other(String),
}

impl RemoteErrorCode {
    /// Parses the wire spelling of a code.
    #[must_use]
    pub fn parse(code: &str) -> Self {
        match code {
            "gateway/bad-request" => Self::BadRequest,
            "gateway/cancelled" => Self::Cancelled,
            "gateway/internal" => Self::Internal,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::BadRequest => UNIVERSAL_ERROR_CODES[0],
            Self::Cancelled => UNIVERSAL_ERROR_CODES[1],
            Self::Internal => UNIVERSAL_ERROR_CODES[2],
            Self::Other(code) => code,
        }
    }

    /// Whether this is one of the universal codes rather than a domain declaration.
    #[must_use]
    pub fn is_universal(&self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

impl fmt::Display for RemoteErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The structural marker every `RemoteError` carries.
///
/// Upstream identifies one across realms by this field plus a string `code`, never by
/// `instanceof`. In Rust the type already identifies it, but the marker still matters at a
/// boundary: a value decoded from a peer's JSON has the marker and no type, and
/// [`remote_error_of`] is what turns one into a failure.
pub const REMOTE_ERROR_MARKER: &str = "isDSHRemoteError";

/// One Remote call failure: a stable code, a human diagnostic, and code-typed details.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteError {
    /// The stable failure code.
    pub code: String,
    /// The human diagnostic, carried across the wire.
    pub message: String,
    /// The structured payload, typed on the receiving side by `code`.
    #[serde(default)]
    pub details: Value,
}

impl RemoteError {
    /// Builds a failure from a code, a message, and its details.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>, details: Value) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details,
        }
    }

    /// Builds the universal `gateway/bad-request` failure carrying codec issues.
    #[must_use]
    pub fn bad_request(message: impl Into<String>, issues: &[Value]) -> Self {
        Self::new(
            RemoteErrorCode::BadRequest.as_str(),
            message,
            serde_json::json!({ "issues": issues }),
        )
    }

    /// The code, parsed into this build's vocabulary.
    #[must_use]
    pub fn parsed_code(&self) -> RemoteErrorCode {
        RemoteErrorCode::parse(&self.code)
    }

    /// The typed details of a `gateway/bad-request`.
    ///
    /// `Some(vec![])` when the failure carries no issue list at all, and `None` when the details are
    /// *not* the shape that code declares. The mismatch is reported rather than defaulted: a client
    /// that assumed `[]` for a malformed `issues` field would report a validation failure with no
    /// issues, which is a lie about what the server said.
    #[must_use]
    pub fn issues(&self) -> Option<Vec<Value>> {
        if self.parsed_code() != RemoteErrorCode::BadRequest {
            return None;
        }
        match self.details.get("issues") {
            None => Some(Vec::new()),
            Some(Value::Array(issues)) => Some(issues.clone()),
            Some(_) => None,
        }
    }

    /// The wire form, always carrying the marker.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            REMOTE_ERROR_MARKER: true,
            "name": "RemoteError",
            "code": self.code,
            "message": self.message,
            "details": self.details,
        })
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RemoteError {}

/// Structurally identifies a Remote failure thrown across a module, realm, or wire.
///
/// Upstream's `remoteErrorOf`. The test is the marker plus a *string* code: an `Error` from another
/// realm fails `instanceof`, and a value that merely looks like one is not a failure at all.
#[must_use]
pub fn remote_error_of(value: &Value) -> Option<RemoteError> {
    let object = value.as_object()?;
    if object.get(REMOTE_ERROR_MARKER).and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let code = object.get("code")?.as_str()?.to_owned();
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let details = object.get("details").cloned().unwrap_or(Value::Null);
    Some(RemoteError {
        code,
        message,
        details,
    })
}

/// The identity a Remote-call producer records on durable output.
///
/// Upstream's `principalOf`: the caller's principal when it is not the owner, and `None` for the
/// owner and for trusted in-process code outside a Remote call. An owner is `None` rather than
/// `Some(owner)` because the owner needs no attribution — recording it would stamp every local
/// write with the identity of the only user there is.
#[must_use]
pub fn principal_of(principal: Option<&Value>) -> Option<&Value> {
    match principal {
        Some(Value::Object(map)) if map.get("kind").and_then(Value::as_str) == Some("owner") => {
            None
        }
        other => other,
    }
}

// ---- invocation descriptors ---------------------------------------------------------------------

/// Whether a method answers once or yields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// One result.
    #[default]
    Unary,
    /// Every yielded item crosses as the Host produced it.
    Stream,
}

/// Where one parameter's value comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParameterSource {
    /// A JSON value carried by the wire `args` object.
    Json,
    /// A Host object resolved from a wire identity by a lookup provider.
    Lookup(String),
}

/// One ordered business parameter of a Remote invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationParameter {
    /// The source-level name.
    pub name: String,
    /// The required wire field name.
    pub wire: String,
    /// Where the value comes from.
    pub source: ParameterSource,
    /// The source documentation, when the parameter had any.
    pub description: Option<String>,
    /// Whether a missing wire field decodes to "absent" for an explicitly declared `T | undefined`.
    ///
    /// Only meaningful for a JSON parameter: a lookup identity has no absent form on the wire.
    pub accepts_undefined: bool,
}

impl InvocationParameter {
    /// A JSON parameter whose wire field has the same name.
    #[must_use]
    pub fn json(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            wire: name.clone(),
            name,
            source: ParameterSource::Json,
            description: None,
            accepts_undefined: false,
        }
    }

    /// A lookup parameter: the wire carries an identity, not the object.
    #[must_use]
    pub fn lookup(
        name: impl Into<String>,
        wire: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            wire: wire.into(),
            source: ParameterSource::Lookup(key.into()),
            description: None,
            accepts_undefined: false,
        }
    }

    /// Overrides the wire field name.
    #[must_use]
    pub fn on_wire(mut self, wire: impl Into<String>) -> Self {
        self.wire = wire.into();
        self
    }

    /// Adds documentation.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Marks a JSON parameter as accepting an absent wire field.
    #[must_use]
    pub fn accepting_undefined(mut self) -> Self {
        self.accepts_undefined = true;
        self
    }

    /// The lookup key, when this is a lookup parameter.
    #[must_use]
    pub fn lookup_key(&self) -> Option<&str> {
        match &self.source {
            ParameterSource::Lookup(key) => Some(key),
            ParameterSource::Json => None,
        }
    }
}

/// Which receiver a Host method is invoked on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Receiver {
    /// The service instance named by the descriptor, reached through the Remote namespace.
    Direct,
    /// A Context resolved from a wire identity, which supplies the receiver instead.
    Context {
        /// The Context key.
        context: String,
        /// The wire field carrying the identity.
        wire: String,
    },
}

/// An optional projection of one lookup parameter onto a consuming Context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The Context kind whose Client adapter supplies the identity.
    pub context: String,
    /// The lookup parameter wire field replaced by the Context identity.
    pub wire: String,
}

impl Scope {
    /// Builds a scope projection.
    #[must_use]
    pub fn new(context: impl Into<String>, wire: impl Into<String>) -> Self {
        Self {
            context: context.into(),
            wire: wire.into(),
        }
    }
}

/// A carrier-independent description of one exported method invocation.
///
/// This is the *runtime* shape upstream generates and the registry stores. It is deliberately not
/// [`girder_core::Method`]: that type models the type-level signature a schema emitter wants, while
/// this one models the *invocation* a carrier performs — which receiver, which parameters come from
/// the wire, whether the answer streams, how the result is typed. A descriptor can name a method
/// whose schema is contributed by a different package, so neither type converts into the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationDescriptor {
    /// Globally stable generated identity.
    pub id: String,
    /// The service key owning the method.
    pub service: String,
    /// The wire namespace.
    pub namespace: String,
    /// The public instance method name.
    pub method: String,
    /// The service member invoked when the exported name is an alias.
    pub implementation: Option<String>,
    /// Unary or stream.
    pub mode: Mode,
    /// Receiver selection.
    pub receiver: Receiver,
    /// Optional consuming-Context projection.
    pub scope: Option<Scope>,
    /// Ordered business parameters.
    pub parameters: Vec<InvocationParameter>,
    /// The type symbol of the uplink item, when the method reads one.
    pub uplink: Option<String>,
    /// The reserved final Host parameter that carries transport cancellation, when the method takes
    /// one.
    ///
    /// A name rather than a flag: upstream validates that the parameter is the reserved `signal`,
    /// and a `bool` could not express a generated descriptor that invented another name.
    pub cancellation: Option<String>,
    /// The type symbol of the unary result or of each stream item.
    pub result: String,
    /// Source-owned method documentation.
    pub description: Option<String>,
}

impl InvocationDescriptor {
    /// Builds a direct, unary descriptor with no parameters.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        service: impl Into<String>,
        namespace: impl Into<String>,
        method: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            service: service.into(),
            namespace: namespace.into(),
            method: method.into(),
            implementation: None,
            mode: Mode::Unary,
            receiver: Receiver::Direct,
            scope: None,
            parameters: Vec::new(),
            uplink: None,
            cancellation: None,
            result: String::new(),
            description: None,
        }
    }

    /// The canonical endpoint, `<namespace>/<method>`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        endpoint_of(&self.namespace, &self.method)
    }

    /// Adds a parameter.
    #[must_use]
    pub fn with_parameter(mut self, parameter: InvocationParameter) -> Self {
        self.parameters.push(parameter);
        self
    }

    /// Sets the result type symbol.
    #[must_use]
    pub fn returning(mut self, symbol: impl Into<String>) -> Self {
        self.result = symbol.into();
        self
    }

    /// Marks the method as streaming.
    #[must_use]
    pub fn streaming(mut self) -> Self {
        self.mode = Mode::Stream;
        self
    }

    /// Declares the uplink item type.
    #[must_use]
    pub fn with_uplink(mut self, symbol: impl Into<String>) -> Self {
        self.uplink = Some(symbol.into());
        self
    }

    /// Injects the reserved `signal` cancellation parameter after the business parameters.
    #[must_use]
    pub fn with_cancellation(mut self) -> Self {
        self.cancellation = Some("signal".to_owned());
        self
    }

    /// Injects a named cancellation parameter. Only `signal` validates; this exists so a decoded
    /// descriptor that names something else is rejected rather than silently accepted.
    #[must_use]
    pub fn with_cancellation_parameter(mut self, parameter: impl Into<String>) -> Self {
        self.cancellation = Some(parameter.into());
        self
    }

    /// Receives on a Context resolved from `wire`.
    #[must_use]
    pub fn on_context(mut self, context: impl Into<String>, wire: impl Into<String>) -> Self {
        self.receiver = Receiver::Context {
            context: context.into(),
            wire: wire.into(),
        };
        self
    }

    /// Projects the only lookup parameter onto a consuming Context.
    #[must_use]
    pub fn with_scope(mut self, scope: Scope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Sets the invoked member when the exported name is an alias.
    #[must_use]
    pub fn implementing(mut self, member: impl Into<String>) -> Self {
        self.implementation = Some(member.into());
        self
    }

    /// Adds documentation.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The wire fields the carrier will see, in declaration order, then the Context identity.
    ///
    /// The Context identity is last because upstream's `NamedInput` merges it into the same strict
    /// object *after* the business parameters; this is the order its generated schema declares.
    #[must_use]
    pub fn wire_fields(&self) -> Vec<&str> {
        let mut fields: Vec<&str> = self.parameters.iter().map(|p| p.wire.as_str()).collect();
        if let Receiver::Context { wire, .. } = &self.receiver {
            fields.push(wire.as_str());
        }
        fields
    }

    /// The wire fields a caller must send, in the same order.
    #[must_use]
    pub fn required_wire_fields(&self) -> Vec<&str> {
        let mut fields: Vec<&str> = self
            .parameters
            .iter()
            .filter(|parameter| !parameter.accepts_undefined)
            .map(|parameter| parameter.wire.as_str())
            .collect();
        if let Receiver::Context { wire, .. } = &self.receiver {
            fields.push(wire.as_str());
        }
        fields
    }
}

/// Checks every property upstream's `validateInvocation` enforces.
///
/// Run by both registration paths before anything is stored, so a rejected contribution leaves no
/// trace — the property upstream asserts with `expect(ctx.typert.remotes.list()).toEqual([])`.
///
/// # Errors
///
/// Returns the first [`TypertError`] the descriptor violates.
pub fn validate_invocation(descriptor: &InvocationDescriptor) -> Result<(), TypertError> {
    if descriptor.id.is_empty() {
        return Err(TypertError::Empty {
            subject: "invocation id",
        });
    }
    if descriptor.service.is_empty() || descriptor.service.contains(KEY_SEPARATOR) {
        return Err(TypertError::Key {
            subject: "invocation service key",
            value: descriptor.service.clone(),
        });
    }
    validate_wire_name("namespace", &descriptor.namespace)?;
    validate_wire_name("method", &descriptor.method)?;
    if let Some(implementation) = &descriptor.implementation {
        validate_wire_name("implementation method", implementation)?;
    }
    if descriptor.result.is_empty() {
        return Err(TypertError::Empty {
            subject: "result type symbol",
        });
    }
    if descriptor.uplink.as_deref() == Some("") {
        return Err(TypertError::Empty {
            subject: "uplink type symbol",
        });
    }

    let mut wires: BTreeSet<&str> = BTreeSet::new();
    let mut lookups = Vec::new();
    for parameter in &descriptor.parameters {
        validate_wire_name("parameter name", &parameter.name)?;
        validate_wire_name("parameter wire field", &parameter.wire)?;
        if !wires.insert(parameter.wire.as_str()) {
            return Err(TypertError::RepeatedWireField {
                id: descriptor.id.clone(),
                wire: parameter.wire.clone(),
            });
        }
        if let Some(key) = parameter.lookup_key() {
            if parameter.accepts_undefined {
                return Err(TypertError::LookupAcceptsUndefined {
                    id: descriptor.id.clone(),
                    name: parameter.name.clone(),
                });
            }
            if key.is_empty() {
                return Err(TypertError::MissingLookup {
                    id: descriptor.id.clone(),
                    name: parameter.name.clone(),
                });
            }
            if !is_remote_segment(key) {
                return Err(TypertError::Segment {
                    subject: "lookup key",
                    value: key.to_owned(),
                });
            }
            lookups.push(parameter);
        }
    }

    if let Some(scope) = &descriptor.scope {
        if !matches!(descriptor.receiver, Receiver::Direct) {
            return Err(TypertError::ContextScope {
                id: descriptor.id.clone(),
            });
        }
        validate_segment("scope Context key", &scope.context)?;
        validate_wire_name("scope wire field", &scope.wire)?;
        let only = (lookups.len() == 1).then(|| lookups[0]);
        let selects = only.is_some_and(|parameter| {
            parameter.wire == scope.wire && parameter.lookup_key() == Some(scope.context.as_str())
        });
        if !selects {
            return Err(TypertError::ScopeSelection {
                id: descriptor.id.clone(),
                wire: scope.wire.clone(),
            });
        }
    }

    if let Receiver::Context { context, wire } = &descriptor.receiver {
        validate_segment("Context key", context)?;
        validate_wire_name("Context wire field", wire)?;
        if wires.contains(wire.as_str()) {
            return Err(TypertError::RepeatedWireField {
                id: descriptor.id.clone(),
                wire: wire.clone(),
            });
        }
    }

    // The cancellation parameter is the reserved final `signal`: a client generates its call site
    // from that name, so a descriptor that invented another one would produce a call the Host never
    // receives.
    if let Some(parameter) = &descriptor.cancellation
        && parameter != "signal"
    {
        return Err(TypertError::Cancellation {
            id: descriptor.id.clone(),
        });
    }

    Ok(())
}

/// Rejects a nonempty identity that contains the key separator.
fn validate_segment(subject: &'static str, value: &str) -> Result<(), TypertError> {
    if value.is_empty() || value.contains(KEY_SEPARATOR) {
        return Err(TypertError::Key {
            subject,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Rejects a name the carrier cannot transport.
fn validate_wire_name(subject: &'static str, value: &str) -> Result<(), TypertError> {
    if !is_remote_segment(value) {
        return Err(TypertError::Segment {
            subject,
            value: value.to_owned(),
        });
    }
    Ok(())
}

// ---- lookup providers ---------------------------------------------------------------------------

/// The resolution policy one lookup provider applies to a validated wire identity.
pub type LookupResolver = Arc<dyn Fn(&Value) -> BoxFut<Option<Value>> + Send + Sync>;

/// The resolution policy one Host Context adapter applies to a validated wire identity.
pub type ContextResolver = Arc<dyn Fn(&Value) -> BoxFut<Option<String>> + Send + Sync>;

/// Reads the wire identity a live Client Context handle represents.
pub type ClientIdentityReader = Arc<dyn Fn(&str) -> Option<Value> + Send + Sync>;

/// Resolves a validated wire identity to a live Client Context handle.
pub type ClientContextResolver = Arc<dyn Fn(&Value) -> Option<String> + Send + Sync>;

/// The single value a resource resolver extracts from a call, when it touches one.
pub type ResourceResolver = Arc<dyn Fn(&ResourceResolveInput) -> ResourceIdentity + Send + Sync>;

/// One declared Host object lookup: how a wire identity replaces a source parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupDeclaration {
    /// The lookup key.
    pub key: String,
    /// The source parameter name the weak parser recognizes.
    pub parameter: String,
    /// The wire field replacing the Host object parameter.
    pub wire: String,
    /// The canonical Host type symbol used by strict generation.
    pub host_type_symbol: String,
    /// The canonical wire type symbol used by strict generation.
    pub wire_type_symbol: String,
}

impl LookupDeclaration {
    /// Builds a declaration.
    #[must_use]
    pub fn new(
        key: impl Into<String>,
        parameter: impl Into<String>,
        wire: impl Into<String>,
        host_type_symbol: impl Into<String>,
        wire_type_symbol: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            parameter: parameter.into(),
            wire: wire.into(),
            host_type_symbol: host_type_symbol.into(),
            wire_type_symbol: wire_type_symbol.into(),
        }
    }

    /// Whether two declarations describe the same wire meaning, ignoring the key they hang off.
    #[must_use]
    pub fn same_wire_meaning(&self, other: &Self) -> bool {
        self.parameter == other.parameter
            && self.wire == other.wire
            && self.host_type_symbol == other.host_type_symbol
            && self.wire_type_symbol == other.wire_type_symbol
    }
}

/// A live lookup provider: its declaration, the resource kind it names, and its resolver.
pub struct LookupProvider {
    declaration: LookupDeclaration,
    resource: Option<String>,
    resolve: LookupResolver,
}

impl LookupProvider {
    /// Builds a provider from a declaration and a synchronous resolver.
    ///
    /// A synchronous resolver is the common case — the wire identity is already in memory — and
    /// wrapping it is what makes the asynchronous form cost nothing at the call site.
    #[must_use]
    pub fn new<F>(declaration: LookupDeclaration, resolve: F) -> Self
    where
        F: Fn(&Value) -> Option<Value> + Send + Sync + 'static,
    {
        Self {
            declaration,
            resource: None,
            resolve: Arc::new(move |id: &Value| {
                let resolved = resolve(id);
                Box::pin(std::future::ready(resolved))
            }),
        }
    }

    /// Builds a provider from an asynchronous resolver.
    #[must_use]
    pub fn with_async_resolver<F>(declaration: LookupDeclaration, resolve: F) -> Self
    where
        F: Fn(&Value) -> BoxFut<Option<Value>> + Send + Sync + 'static,
    {
        Self {
            declaration,
            resource: None,
            resolve: Arc::new(resolve),
        }
    }

    /// Names the resource kind the wire identity belongs to.
    #[must_use]
    pub fn naming_resource(mut self, kind: impl Into<String>) -> Self {
        self.resource = Some(kind.into());
        self
    }

    /// The declaration.
    #[must_use]
    pub fn declaration(&self) -> &LookupDeclaration {
        &self.declaration
    }

    /// The resource kind, when the identity names one.
    #[must_use]
    pub fn resource(&self) -> Option<&str> {
        self.resource.as_deref()
    }

    /// Resolves one wire identity.
    #[must_use]
    pub fn resolve(&self, id: &Value) -> BoxFut<Option<Value>> {
        (self.resolve)(id)
    }
}

impl fmt::Debug for LookupProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LookupProvider")
            .field("declaration", &self.declaration)
            .field("resource", &self.resource)
            .field("resolve", &"closure")
            .finish()
    }
}

/// A borrow of one live lookup provider.
pub struct LookupProviderView<'a> {
    declaration: &'a LookupDeclaration,
    resource: Option<&'a str>,
    resolve: &'a LookupProvider,
}

impl LookupProviderView<'_> {
    /// The declaration.
    #[must_use]
    pub fn declaration(&self) -> &LookupDeclaration {
        self.declaration
    }

    /// The resource kind the identity names.
    #[must_use]
    pub fn resource(&self) -> Option<&str> {
        self.resource
    }

    /// Resolves one wire identity.
    #[must_use]
    pub fn resolve(&self, id: &Value) -> BoxFut<Option<Value>> {
        self.resolve.resolve(id)
    }
}

impl fmt::Debug for LookupProviderView<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LookupProviderView")
            .field("declaration", self.declaration)
            .field("resource", &self.resource)
            .field("resolve", &"closure")
            .finish()
    }
}

// ---- Context adapters ---------------------------------------------------------------------------

/// A Host-side wire-to-Context adapter.
#[derive(Clone)]
pub struct HostContextAdapter {
    /// The wire field carrying the Context identity.
    pub wire: String,
    /// The canonical wire type symbol.
    pub wire_type_symbol: String,
    /// The resource kind the identity names.
    pub resource: Option<String>,
    /// Resolves one validated identity to a live Context handle.
    pub resolve: ContextResolver,
}

impl HostContextAdapter {
    /// Builds an adapter with a synchronous resolver.
    #[must_use]
    pub fn new<F>(wire: impl Into<String>, wire_type_symbol: impl Into<String>, resolve: F) -> Self
    where
        F: Fn(&Value) -> Option<String> + Send + Sync + 'static,
    {
        Self {
            wire: wire.into(),
            wire_type_symbol: wire_type_symbol.into(),
            resource: None,
            resolve: Arc::new(move |id: &Value| {
                let resolved = resolve(id);
                Box::pin(std::future::ready(resolved))
            }),
        }
    }

    /// Names the resource kind the identity belongs to.
    #[must_use]
    pub fn naming_resource(mut self, kind: impl Into<String>) -> Self {
        self.resource = Some(kind.into());
        self
    }
}

impl fmt::Debug for HostContextAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostContextAdapter")
            .field("wire", &self.wire)
            .field("wire_type_symbol", &self.wire_type_symbol)
            .field("resource", &self.resource)
            .field("resolve", &"closure")
            .finish()
    }
}

/// A borrow of one live Host Context adapter.
pub struct HostContextAdapterView<'a> {
    wire: &'a str,
    wire_type_symbol: &'a str,
    resource: Option<&'a str>,
    resolve: &'a HostContextAdapter,
}

impl HostContextAdapterView<'_> {
    /// The wire field carrying the identity.
    #[must_use]
    pub fn wire(&self) -> &str {
        self.wire
    }

    /// The canonical wire type symbol.
    #[must_use]
    pub fn wire_type_symbol(&self) -> &str {
        self.wire_type_symbol
    }

    /// The resource kind the identity names.
    #[must_use]
    pub fn resource(&self) -> Option<&str> {
        self.resource
    }

    /// Resolves one validated identity.
    #[must_use]
    pub fn resolve(&self, id: &Value) -> BoxFut<Option<String>> {
        (self.resolve.resolve)(id)
    }
}

impl fmt::Debug for HostContextAdapterView<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostContextAdapterView")
            .field("wire", &self.wire)
            .field("wire_type_symbol", &self.wire_type_symbol)
            .field("resource", &self.resource)
            .field("resolve", &"closure")
            .finish()
    }
}

/// A Client-side bidirectional Context adapter.
///
/// `identity` reads the wire identity out of a live handle and `resolve` turns an identity back into
/// one. Both are synchronous because a Client invocation cannot await: upstream's
/// `TypertClientContextAdapter` returns a borrowed or invocation-owned value, never a promise.
#[derive(Clone)]
pub struct ClientContextAdapter {
    /// Reads the identity a live Client handle represents.
    pub identity: ClientIdentityReader,
    /// Resolves one validated identity for a single invocation.
    pub resolve: ClientContextResolver,
}

impl ClientContextAdapter {
    /// Builds an adapter from its two directions.
    #[must_use]
    pub fn new<I, R>(identity: I, resolve: R) -> Self
    where
        I: Fn(&str) -> Option<Value> + Send + Sync + 'static,
        R: Fn(&Value) -> Option<String> + Send + Sync + 'static,
    {
        Self {
            identity: Arc::new(identity),
            resolve: Arc::new(resolve),
        }
    }
}

impl fmt::Debug for ClientContextAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientContextAdapter")
            .field("identity", &"closure")
            .field("resolve", &"closure")
            .finish()
    }
}

// ---- resource declarations ----------------------------------------------------------------------

/// The input one resource resolver reads: validated wire values and the declared parameters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceResolveInput {
    /// Wire field to its validated value, including lookup and Context identities.
    pub args: BTreeMap<String, Value>,
    /// Declared parameters in call order, as `(name, wire)`, so a resolver can match either.
    pub parameters: Vec<(String, String)>,
}

impl ResourceResolveInput {
    /// Builds an input from wire values and `(name, wire)` pairs.
    #[must_use]
    pub fn new(args: BTreeMap<String, Value>, parameters: Vec<(String, String)>) -> Self {
        Self { args, parameters }
    }

    /// Reads a wire value.
    #[must_use]
    pub fn value(&self, wire: &str) -> Option<&Value> {
        self.args.get(wire)
    }
}

/// What a resource resolver answers for one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceIdentity {
    /// The call touches one identity of the kind.
    Identity(String),
    /// The call touches no resource of this kind.
    Absent,
    /// The call touches more than one identity, which a single-resource call cannot name.
    ///
    /// Upstream treats an array here as a rejection: a call that touches several resources would
    /// otherwise be authorized against the first, which is a hole rather than a policy.
    Many(Vec<String>),
}

impl ResourceIdentity {
    /// The single identity, when exactly one was named.
    #[must_use]
    pub fn single(&self) -> Option<&str> {
        match self {
            Self::Identity(id) => Some(id),
            Self::Absent | Self::Many(_) => None,
        }
    }

    /// Whether the call touches no resource of this kind.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

/// Default identity extraction for one resource kind.
#[derive(Clone)]
pub enum ResourceDeclaration {
    /// Read the identity from one wire field.
    Field(String),
    /// Compute the identity from the whole call.
    Resolver(ResourceResolver),
}

impl ResourceDeclaration {
    /// Declares a field-based extraction.
    #[must_use]
    pub fn field(field: impl Into<String>) -> Self {
        Self::Field(field.into())
    }

    /// Declares a resolver-based extraction.
    #[must_use]
    pub fn resolver<F>(resolve: F) -> Self
    where
        F: Fn(&ResourceResolveInput) -> ResourceIdentity + Send + Sync + 'static,
    {
        Self::Resolver(Arc::new(resolve))
    }

    /// Extracts the identity for one call.
    #[must_use]
    pub fn extract(&self, input: &ResourceResolveInput) -> ResourceIdentity {
        match self {
            Self::Field(field) => match input.value(field) {
                None | Some(Value::Null) => ResourceIdentity::Absent,
                Some(Value::String(id)) => ResourceIdentity::Identity(id.clone()),
                Some(other) => ResourceIdentity::Identity(other.to_string()),
            },
            Self::Resolver(resolve) => resolve(input),
        }
    }
}

/// Two declarations are equal when they extract the same way.
///
/// A resolver variant is a closure, compared by `Arc` identity: two `Arc`s share a resolver only if
/// one was cloned from the other, which is the only equality that can be decided without calling
/// them. A field variant compares by name, which is the whole of what it means.
impl PartialEq for ResourceDeclaration {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Field(left), Self::Field(right)) => left == right,
            (Self::Resolver(left), Self::Resolver(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl fmt::Debug for ResourceDeclaration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field(field) => formatter.debug_tuple("Field").field(field).finish(),
            Self::Resolver(_) => formatter.write_str("Resolver(..)"),
        }
    }
}

/// One registered resource kind with its extraction declaration.
#[derive(Clone, Debug)]
pub struct ResourceEntry {
    /// The kind the owning domain contributed.
    pub kind: String,
    /// How a call's identity is extracted.
    pub declaration: ResourceDeclaration,
}

impl PartialEq for ResourceEntry {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.declaration == other.declaration
    }
}

// ---- change notification ------------------------------------------------------------------------

/// Which registry changed.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    /// A current-environment invocation definition.
    #[default]
    Local,
    /// A consumer-selected Remote definition.
    Remote,
    /// A Host object lookup provider.
    Lookup,
    /// A Host Context adapter.
    HostContext,
    /// A Client Context adapter.
    ClientContext,
    /// A resource kind's default extraction.
    Resource,
}

impl ChangeKind {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Lookup => "lookup",
            Self::HostContext => "host-context",
            Self::ClientContext => "client-context",
            Self::Resource => "resource",
        }
    }
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One notification emitted after a registry changes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RegistryChange {
    /// Which registry.
    pub kind: ChangeKind,
    /// The key that changed.
    pub key: String,
}

impl RegistryChange {
    /// Builds a change.
    #[must_use]
    pub fn new(kind: ChangeKind, key: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
        }
    }
}

impl fmt::Display for RegistryChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.kind, self.key)
    }
}

/// A synchronous contained observer.
pub type ChangeListener = Arc<dyn Fn(&RegistryChange) + Send + Sync>;

/// A subscription's identity, so it can be withdrawn exactly.
pub type SubscriptionId = u64;

/// Delivers one change to every listener, containing each failure.
///
/// Upstream catches a thrown listener error, logs it, and continues. Rust has no thrown error, so
/// the failure mode is a panic, caught here — an observer that took the registry down with it would
/// turn a diagnostic bug into a data-loss bug. The report goes to `warnings` rather than a logger.
fn notify(
    listeners: &[(SubscriptionId, ChangeListener)],
    change: &RegistryChange,
    warnings: &mut Vec<String>,
) {
    for (_, listener) in listeners {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(change)));
        if outcome.is_err() {
            warnings.push(format!("typert: {change} observer failed"));
        }
    }
}

// ---- generated schema and package models --------------------------------------------------------

/// Runtime reflection metadata for one Cordis service.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceModel {
    /// The Cordis service key.
    pub key: String,
    /// The exported name.
    pub export_name: String,
    /// The public member signatures.
    pub members: Vec<String>,
}

/// Runtime reflection metadata for one Cordis event.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventModel {
    /// The event name.
    pub name: String,
    /// `emit` or `waterfall`.
    pub mode: Option<String>,
    /// The listener signature.
    pub signature: String,
}

/// Generated business reflection for one package on one face.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageModel {
    /// The services the package exposes.
    pub services: Vec<ServiceModel>,
    /// The events the package emits.
    pub events: Vec<EventModel>,
    /// The explicitly exported reference objects.
    pub objects: Vec<String>,
}

/// One live package model plus its stable identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRecord {
    /// The package.
    pub package: String,
    /// The face that produced it.
    pub face: Face,
    /// The `<package>#<face>` identity.
    pub key: String,
    /// The generated reflection.
    pub model: PackageModel,
}

/// One live schema plus its contribution identity.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaRecordView {
    /// The schema export name.
    pub name: String,
    /// The materialized schema.
    pub schema: Arc<ObjectSchema>,
    /// The contributing package.
    pub package: String,
    /// The producing face.
    pub face: Face,
    /// The global `<package>#<name>` key.
    pub key: String,
}

/// A restriction on schema enumeration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaFilter {
    /// Only schemas from this package.
    pub package: Option<String>,
    /// Only schemas from this face.
    pub face: Option<Face>,
}

impl SchemaFilter {
    /// Matches one record.
    #[must_use]
    pub fn matches(&self, record: &SchemaRecordView) -> bool {
        self.package.as_ref().is_none_or(|p| p == &record.package)
            && self.face.is_none_or(|f| f == record.face)
    }
}

/// A restriction on package-model enumeration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageFilter {
    /// Only this package.
    pub package: Option<String>,
    /// Only this face.
    pub face: Option<Face>,
}

impl PackageFilter {
    /// Matches one record.
    #[must_use]
    pub fn matches(&self, record: &PackageRecord) -> bool {
        self.package.as_ref().is_none_or(|p| p == &record.package)
            && self.face.is_none_or(|f| f == record.face)
    }
}

// ---- contributions ------------------------------------------------------------------------------

/// A schema factory: called at most once, when the schema is first materialized.
pub type SchemaFactory = Box<dyn FnOnce() -> ObjectSchema + Send + Sync>;

/// One intended package contribution, validated as a whole and then committed atomically.
pub struct Contribution {
    package: String,
    face: Face,
    model: PackageModel,
    schemas: Vec<(String, SchemaFactory)>,
    invocations: Vec<InvocationDescriptor>,
}

impl Contribution {
    /// Starts an empty Host contribution for one package.
    #[must_use]
    pub fn new(package: impl Into<String>) -> Self {
        Self {
            package: package.into(),
            face: Face::Host,
            model: PackageModel::default(),
            schemas: Vec::new(),
            invocations: Vec::new(),
        }
    }

    /// Marks the contribution as produced by another face.
    #[must_use]
    pub fn on_face(mut self, face: Face) -> Self {
        self.face = face;
        self
    }

    /// Sets the generated reflection.
    #[must_use]
    pub fn with_model(mut self, model: PackageModel) -> Self {
        self.model = model;
        self
    }

    /// Adds one schema factory. It is not called until the schema is first resolved.
    #[must_use]
    pub fn with_schema<F>(mut self, name: impl Into<String>, factory: F) -> Self
    where
        F: FnOnce() -> ObjectSchema + Send + Sync + 'static,
    {
        self.schemas.push((name.into(), Box::new(factory)));
        self
    }

    /// Adds one local invocation descriptor.
    #[must_use]
    pub fn with_invocation(mut self, descriptor: InvocationDescriptor) -> Self {
        self.invocations.push(descriptor);
        self
    }

    /// The package name.
    #[must_use]
    pub fn package(&self) -> &str {
        &self.package
    }
}

impl fmt::Debug for Contribution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Contribution")
            .field("package", &self.package)
            .field("face", &self.face)
            .field("model", &self.model)
            .field("schemas", &self.schemas.len())
            .field("invocations", &self.invocations)
            .finish()
    }
}

/// One Remote package's descriptors, mounted and withdrawn as a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteContribution {
    /// The package the descriptors were generated from.
    pub package: String,
    /// Its descriptors.
    pub descriptors: Vec<InvocationDescriptor>,
}

impl RemoteContribution {
    /// Builds a Remote contribution.
    #[must_use]
    pub fn new(package: impl Into<String>, descriptors: Vec<InvocationDescriptor>) -> Self {
        Self {
            package: package.into(),
            descriptors,
        }
    }
}

/// The id `register` returns; it names the owner a contribution's records belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContributionId {
    owner: u64,
}

/// The id `mount_remote` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteMountId {
    owner: u64,
}

/// The key a lookup provider was registered under.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LookupId {
    key: String,
}

impl LookupId {
    /// The key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// The kind and key of one Context adapter.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextId {
    kind: ChangeKind,
    key: String,
}

impl ContextId {
    /// The key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Which registry it belongs to.
    #[must_use]
    pub fn kind(&self) -> ChangeKind {
        self.kind
    }
}

/// The kind a resource declaration was registered for.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceId {
    kind: String,
}

impl ResourceId {
    /// The kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }
}

// ---- the registry -------------------------------------------------------------------------------

/// An owner identity: one call to `register` or `mount_remote`.
type Owner = u64;

/// One descriptor registry entry.
#[derive(Clone)]
struct DescriptorEntry {
    descriptor: InvocationDescriptor,
    owner: Owner,
}

/// The engine behind both the local and the Remote descriptor registries.
///
/// Both sides share one implementation for the reason upstream does: the invariants are identical
/// (unique endpoint, unique id, owner-scoped withdrawal, registration-order listing), and two copies
/// would drift on whichever one is exercised less. `BTreeMap` rather than a hash map because
/// registration order *is* the iteration order, which upstream relies on and a hash map would not
/// preserve.
#[derive(Default)]
struct DescriptorStore {
    kind: ChangeKind,
    entries: BTreeMap<String, DescriptorEntry>,
    ids: BTreeMap<String, String>,
    /// Endpoints that have existed at any point, for `has_seen`.
    history: BTreeSet<String>,
    /// Public changes, already emitted through the registry's own listener list.
    changes: Vec<RegistryChange>,
}

impl DescriptorStore {
    fn new(kind: ChangeKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    /// Checks a batch without storing any of it.
    ///
    /// The batch is checked as a whole because a contribution that half-registers is worse than one
    /// that does not register: the caller gets an error *and* a partial surface it cannot withdraw.
    fn validate(&self, descriptors: &[InvocationDescriptor]) -> Result<(), TypertError> {
        let mut endpoints = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for descriptor in descriptors {
            validate_invocation(descriptor)?;
            let endpoint = descriptor.endpoint();
            if !endpoints.insert(endpoint.clone()) || self.entries.contains_key(&endpoint) {
                return Err(TypertError::DuplicateEndpoint {
                    kind: self.kind.as_str(),
                    endpoint,
                });
            }
            if !ids.insert(descriptor.id.clone()) || self.ids.contains_key(&descriptor.id) {
                return Err(TypertError::DuplicateId {
                    kind: self.kind.as_str(),
                    id: descriptor.id.clone(),
                });
            }
        }
        Ok(())
    }

    fn commit(&mut self, owner: Owner, descriptors: &[InvocationDescriptor]) {
        for descriptor in descriptors {
            let endpoint = descriptor.endpoint();
            self.ids.insert(descriptor.id.clone(), endpoint.clone());
            self.history.insert(endpoint.clone());
            self.entries.insert(
                endpoint,
                DescriptorEntry {
                    descriptor: descriptor.clone(),
                    owner,
                },
            );
        }
        for descriptor in descriptors {
            self.changes
                .push(RegistryChange::new(self.kind, descriptor.endpoint()));
        }
    }

    fn withdraw(&mut self, owner: Owner, descriptors: &[InvocationDescriptor]) {
        for descriptor in descriptors {
            let endpoint = descriptor.endpoint();
            // An entry another owner replaced is not this contribution's to remove. Duplicate
            // registration is rejected, so this can only matter if a caller withdrew out of order.
            if self.entries.get(&endpoint).map(|entry| entry.owner) != Some(owner) {
                continue;
            }
            self.entries.remove(&endpoint);
            if self.ids.get(&descriptor.id).map(String::as_str) == Some(endpoint.as_str()) {
                self.ids.remove(&descriptor.id);
            }
            self.changes.push(RegistryChange::new(self.kind, endpoint));
        }
    }

    fn get(&self, endpoint: &str) -> Option<&InvocationDescriptor> {
        self.entries.get(endpoint).map(|entry| &entry.descriptor)
    }

    fn has_seen(&self, endpoint: &str) -> bool {
        self.history.contains(endpoint)
    }

    fn list(&self) -> Vec<InvocationDescriptor> {
        self.entries
            .values()
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    fn owned_by(&self, owner: Owner) -> Vec<InvocationDescriptor> {
        self.entries
            .values()
            .filter(|entry| entry.owner == owner)
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    /// Drains the changes recorded since the last drain.
    fn drain(&mut self) -> Vec<RegistryChange> {
        std::mem::take(&mut self.changes)
    }
}

/// The registry of generated schemas, package reflection, invocations, and dependency providers.
///
/// One registry is one Cordis service instance: the lifetime upstream's `TypertRegistry` spans, and
/// the lifetime `has_seen` and the lookup-declaration freeze are measured against.
pub struct TypertRegistry {
    schemas: BTreeMap<String, SchemaRecord>,
    /// Schema key to the owner that contributed it.
    schema_owners: BTreeMap<String, Owner>,
    packages: BTreeMap<String, PackageRecord>,
    /// Package-face key to the owner that contributed it.
    package_owners: BTreeMap<String, Owner>,
    local: DescriptorStore,
    remote: DescriptorStore,
    lookups: BTreeMap<String, LookupProvider>,
    lookup_resolvers: BTreeMap<String, LookupResolver>,
    /// Declarations that survive a withdrawal, so a later registration cannot change the wire
    /// meaning of a key that other packages already generated against.
    lookup_definitions: BTreeMap<String, LookupDeclaration>,
    host_contexts: BTreeMap<String, HostContextAdapter>,
    host_context_resolvers: BTreeMap<String, ContextResolver>,
    client_contexts: BTreeMap<String, ClientContextAdapter>,
    resources: BTreeMap<String, ResourceDeclaration>,
    /// Mounted Remote packages, with the owner that mounted each.
    remote_packages: BTreeMap<String, Owner>,
    listeners: Vec<(SubscriptionId, ChangeListener)>,
    local_listeners: Vec<(SubscriptionId, ChangeListener)>,
    remote_listeners: Vec<(SubscriptionId, ChangeListener)>,
    next_subscription: SubscriptionId,
    next_owner: Owner,
    warnings: Mutex<Vec<String>>,
}

impl Default for TypertRegistry {
    fn default() -> Self {
        Self {
            schemas: BTreeMap::new(),
            schema_owners: BTreeMap::new(),
            packages: BTreeMap::new(),
            package_owners: BTreeMap::new(),
            local: DescriptorStore::new(ChangeKind::Local),
            remote: DescriptorStore::new(ChangeKind::Remote),
            lookups: BTreeMap::new(),
            lookup_resolvers: BTreeMap::new(),
            lookup_definitions: BTreeMap::new(),
            host_contexts: BTreeMap::new(),
            host_context_resolvers: BTreeMap::new(),
            client_contexts: BTreeMap::new(),
            resources: BTreeMap::new(),
            remote_packages: BTreeMap::new(),
            listeners: Vec::new(),
            local_listeners: Vec::new(),
            remote_listeners: Vec::new(),
            next_subscription: 0,
            next_owner: 0,
            warnings: Mutex::new(Vec::new()),
        }
    }
}

impl TypertRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // ---- contributions --------------------------------------------------------------------------

    /// Registers one generated contribution atomically.
    ///
    /// A duplicate package face, a duplicate schema key, a malformed identity, or an invalid
    /// descriptor rejects the *whole* batch, so a rejected contribution leaves no package record,
    /// no schema, and no invocation behind.
    ///
    /// # Errors
    ///
    /// Returns the first [`TypertError`] the contribution violates, and stores nothing.
    pub fn register(&mut self, contribution: Contribution) -> Result<ContributionId, TypertError> {
        validate_segment("package name", &contribution.package)?;
        let face_key = package_key(&contribution.package, contribution.face);
        if self.packages.contains_key(&face_key) {
            return Err(TypertError::PackageRegistered { key: face_key });
        }

        // Every schema key is resolved and checked against the registry *and* the batch before any
        // factory is stored: ownership of an `FnOnce` cannot be given back.
        let mut names = Vec::with_capacity(contribution.schemas.len());
        for (name, _) in &contribution.schemas {
            validate_segment("schema name", name)?;
            let key = schema_key(&contribution.package, name);
            if names
                .iter()
                .any(|(_, existing): &(String, String)| *existing == key)
                || self.schemas.contains_key(&key)
            {
                return Err(TypertError::SchemaRegistered { key });
            }
            names.push((name.clone(), key));
        }

        self.local.validate(&contribution.invocations)?;

        let owner = self.owner();
        self.packages.insert(
            face_key.clone(),
            PackageRecord {
                package: contribution.package.clone(),
                face: contribution.face,
                key: face_key.clone(),
                model: contribution.model,
            },
        );
        self.package_owners.insert(face_key, owner);

        for ((_, factory), (name, key)) in contribution.schemas.into_iter().zip(names) {
            self.schemas.insert(
                key.clone(),
                SchemaRecord {
                    name,
                    package: contribution.package.clone(),
                    face: contribution.face,
                    key: key.clone(),
                    factory: Some(factory),
                    value: None,
                },
            );
            self.schema_owners.insert(key, owner);
        }

        self.local.commit(owner, &contribution.invocations);
        self.publish_local();
        Ok(ContributionId { owner })
    }

    /// Withdraws one contribution through the exact id `register` returned.
    ///
    /// Nothing else can be withdrawn by it: each record is removed only while it still belongs to
    /// that owner, so a stale id cannot remove a later contribution.
    pub fn withdraw(&mut self, id: ContributionId) {
        let schemas: Vec<String> = self
            .schema_owners
            .iter()
            .filter(|(_, owner)| **owner == id.owner)
            .map(|(key, _)| key.clone())
            .collect();
        for key in schemas {
            self.schema_owners.remove(&key);
            self.schemas.remove(&key);
        }
        let packages: Vec<String> = self
            .package_owners
            .iter()
            .filter(|(_, owner)| **owner == id.owner)
            .map(|(key, _)| key.clone())
            .collect();
        for key in packages {
            self.package_owners.remove(&key);
            self.packages.remove(&key);
        }
        let descriptors = self.local.owned_by(id.owner);
        self.local.withdraw(id.owner, &descriptors);
        self.publish_local();
    }

    // ---- schemas --------------------------------------------------------------------------------

    /// Looks up one schema by `<package>#<name>`.
    ///
    /// Materializes the factory on first access: registering a schema costs nothing, and a schema
    /// that is never asked for is never built.
    pub fn get(&mut self, key: &str) -> Option<SchemaRecordView> {
        let record = self.schemas.get_mut(key)?;
        Some(materialize(record))
    }

    /// Resolves one required schema.
    ///
    /// The three failures are distinct on purpose, because they tell a client different things: a
    /// malformed key is the caller's bug, a registered package without that schema is a version
    /// mismatch, and an unregistered package is a missing contribution.
    ///
    /// # Errors
    ///
    /// Returns [`TypertError::InvalidSchemaKey`], [`TypertError::UnknownSchema`], or
    /// [`TypertError::UnknownPackage`].
    pub fn resolve(&mut self, key: &str) -> Result<SchemaRecordView, TypertError> {
        let known = self.schemas.contains_key(key);
        if known {
            // The probe above cannot hand out the borrow, so the record is fetched again.
            return match self.schemas.get_mut(key) {
                Some(record) => Ok(materialize(record)),
                None => Err(TypertError::InvalidSchemaKey {
                    key: key.to_owned(),
                }),
            };
        }
        let (package, name) = key
            .split_once(KEY_SEPARATOR)
            .filter(|(package, name)| !package.is_empty() && !name.is_empty())
            .ok_or_else(|| TypertError::InvalidSchemaKey {
                key: key.to_owned(),
            })?;
        if self
            .packages
            .values()
            .any(|record| record.package == package)
        {
            return Err(TypertError::UnknownSchema {
                key: key.to_owned(),
                package: package.to_owned(),
                name: name.to_owned(),
            });
        }
        Err(TypertError::UnknownPackage {
            key: key.to_owned(),
            package: package.to_owned(),
        })
    }

    /// Enumerates live schemas in registration order.
    pub fn list(&mut self, filter: &SchemaFilter) -> Vec<SchemaRecordView> {
        let keys: Vec<String> = self.schemas.keys().cloned().collect();
        let mut matched = Vec::new();
        for key in keys {
            let Some(record) = self.schemas.get_mut(&key) else {
                continue;
            };
            let view = materialize(record);
            if filter.matches(&view) {
                matched.push(view);
            }
        }
        matched
    }

    /// Looks up generated reflection for one package face.
    #[must_use]
    pub fn get_package(&self, package: &str, face: Face) -> Option<&PackageRecord> {
        self.packages.get(&package_key(package, face))
    }

    /// Enumerates generated package reflection in registration order.
    #[must_use]
    pub fn list_packages(&self, filter: &PackageFilter) -> Vec<PackageRecord> {
        self.packages
            .values()
            .filter(|record| filter.matches(record))
            .cloned()
            .collect()
    }

    /// Projects one live schema to a standalone JSON Schema document.
    ///
    /// The projection is deliberately *not* cached, and it is a *document*: the named type is
    /// hoisted into `$defs` and the root is an empty schema that accepts anything, which is what a
    /// bundle looks like. Projecting the named type in place would inline it and leave its `$ref`s
    /// dangling, and caching the document would share one mutable value between every caller —
    /// upstream's test asserts two projections are different values.
    ///
    /// # Errors
    ///
    /// Returns whatever [`TypertRegistry::resolve`] returns.
    pub fn json_schema_for(&mut self, key: &str) -> Result<Value, TypertError> {
        let record = self.resolve(key)?;
        let name = record.schema.name.clone();
        let mut catalogue = girder_core::Catalogue::new(0);
        let mut schema = (*record.schema).clone();
        // A definition needs a nonempty name; a schema contributed without one is projected under
        // its key's name part, which is what a client would have called it anyway.
        if schema.name.is_empty() {
            schema.name = name;
        }
        catalogue.define(schema);
        Ok(girder_core::catalogue_to_json_schema(&catalogue))
    }

    // ---- local invocations ----------------------------------------------------------------------

    /// Reads one local invocation by endpoint.
    #[must_use]
    pub fn local(&self, endpoint: &str) -> Option<&InvocationDescriptor> {
        self.local.get(endpoint)
    }

    /// Reports whether a local descriptor has existed at any point in this registry's lifetime.
    ///
    /// A withdrawn endpoint stays visible, which is what lets a client distinguish "this build never
    /// had that method" from "the plugin that owned it unloaded".
    #[must_use]
    pub fn has_seen(&self, endpoint: &str) -> bool {
        self.local.has_seen(endpoint)
    }

    /// Lists local descriptors in registration order.
    #[must_use]
    pub fn local_invocations(&self) -> Vec<InvocationDescriptor> {
        self.local.list()
    }

    // ---- Remote invocations ---------------------------------------------------------------------

    /// Mounts one Remote contribution.
    ///
    /// # Errors
    ///
    /// Returns [`TypertError::RemotePackage`] for a second mount of the same package, or the first
    /// descriptor violation — in either case nothing is mounted.
    pub fn mount_remote(
        &mut self,
        contribution: &RemoteContribution,
    ) -> Result<RemoteMountId, TypertError> {
        validate_segment("Remote package name", &contribution.package)?;
        if self.remote_packages.contains_key(&contribution.package) {
            return Err(TypertError::RemotePackage {
                package: contribution.package.clone(),
            });
        }
        self.remote.validate(&contribution.descriptors)?;
        let owner = self.owner();
        self.remote_packages
            .insert(contribution.package.clone(), owner);
        self.remote.commit(owner, &contribution.descriptors);
        self.publish_remote();
        Ok(RemoteMountId { owner })
    }

    /// Withdraws one mounted Remote contribution.
    ///
    /// The package name is released only for the owner that mounted it, so a withdrawal of a stale
    /// id cannot unmount a later mount of the same package.
    pub fn unmount_remote(&mut self, id: RemoteMountId) {
        let descriptors = self.remote.owned_by(id.owner);
        self.remote.withdraw(id.owner, &descriptors);
        let packages: Vec<String> = self
            .remote_packages
            .iter()
            .filter(|(_, owner)| **owner == id.owner)
            .map(|(package, _)| package.clone())
            .collect();
        for package in packages {
            self.remote_packages.remove(&package);
        }
        self.publish_remote();
    }

    /// Reads one Remote descriptor by endpoint.
    #[must_use]
    pub fn remote(&self, endpoint: &str) -> Option<&InvocationDescriptor> {
        self.remote.get(endpoint)
    }

    /// Lists Remote descriptors in registration order.
    #[must_use]
    pub fn remote_invocations(&self) -> Vec<InvocationDescriptor> {
        self.remote.list()
    }

    /// The Remote packages currently mounted, in name order.
    #[must_use]
    pub fn remote_packages(&self) -> Vec<String> {
        self.remote_packages.keys().cloned().collect()
    }

    // ---- lookup providers -----------------------------------------------------------------------

    /// Registers one Host object lookup provider.
    ///
    /// # Errors
    ///
    /// Returns a validation error, [`TypertError::LookupRegistered`] for a duplicate key, or
    /// [`TypertError::LookupRedeclared`] when the key was registered earlier in this registry's
    /// lifetime with a different wire declaration.
    pub fn register_lookup(&mut self, provider: LookupProvider) -> Result<LookupId, TypertError> {
        let declaration = provider.declaration().clone();
        validate_segment("lookup key", &declaration.key)?;
        validate_segment("lookup parameter", &declaration.parameter)?;
        validate_wire_name("lookup wire field", &declaration.wire)?;
        if declaration.host_type_symbol.is_empty() {
            return Err(TypertError::Empty {
                subject: "lookup Host type symbol",
            });
        }
        if declaration.wire_type_symbol.is_empty() {
            return Err(TypertError::Empty {
                subject: "lookup wire type symbol",
            });
        }
        if self.lookups.contains_key(&declaration.key) {
            return Err(TypertError::LookupRegistered {
                key: declaration.key.clone(),
            });
        }
        if let Some(known) = self.lookup_definitions.get(&declaration.key)
            && !known.same_wire_meaning(&declaration)
        {
            return Err(TypertError::LookupRedeclared {
                key: declaration.key.clone(),
            });
        }
        let key = declaration.key.clone();
        self.lookup_definitions.insert(key.clone(), declaration);
        self.lookups.insert(key.clone(), provider);
        self.emit(ChangeKind::Lookup, &key);
        Ok(LookupId { key })
    }

    /// Withdraws one lookup provider.
    ///
    /// The *definition* stays: the wire meaning of a key outlives the provider that served it, which
    /// is the difference between "no provider right now" and "this key now means something else".
    pub fn withdraw_lookup(&mut self, id: &LookupId) {
        if self.lookups.remove(&id.key).is_some() {
            self.emit(ChangeKind::Lookup, &id.key);
        }
    }

    /// Reads one lookup provider.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<LookupProviderView<'_>> {
        let provider = self.lookups.get(key)?;
        Some(LookupProviderView {
            declaration: provider.declaration(),
            resource: provider.resource(),
            resolve: provider,
        })
    }

    /// The keys with a live provider, in registration order.
    #[must_use]
    pub fn lookup_keys(&self) -> Vec<String> {
        self.lookups.keys().cloned().collect()
    }

    /// Every stable lookup declaration, including one whose provider has withdrawn.
    #[must_use]
    pub fn lookup_definitions(&self) -> Vec<&LookupDeclaration> {
        self.lookup_definitions.values().collect()
    }

    /// Replaces one lookup provider's default resolution policy.
    ///
    /// Kept separate from registration because the composition root owns the policy while the domain
    /// package owns the declaration, and the two load in either order.
    ///
    /// # Errors
    ///
    /// Returns [`TypertError::LookupConfigured`] when a resolver is already configured.
    pub fn configure_lookup<F>(&mut self, key: &str, resolve: F) -> Result<LookupId, TypertError>
    where
        F: Fn(&Value) -> BoxFut<Option<Value>> + Send + Sync + 'static,
    {
        validate_segment("lookup key", key)?;
        if self.lookup_resolvers.contains_key(key) {
            return Err(TypertError::LookupConfigured {
                key: key.to_owned(),
            });
        }
        self.lookup_resolvers
            .insert(key.to_owned(), Arc::new(resolve));
        self.emit(ChangeKind::Lookup, key);
        Ok(LookupId {
            key: key.to_owned(),
        })
    }

    /// Resolves one identity through the configured policy, falling back to the provider's default.
    ///
    /// Configured policy wins over the provider default because that is what "composition-owned
    /// replacement" means; a key with neither is a registry error rather than an empty answer, so a
    /// caller cannot mistake "not configured" for "the object does not exist".
    ///
    /// # Errors
    ///
    /// Returns [`TypertError::LookupRegistered`] when no provider and no resolver exist for the key.
    pub fn resolve_lookup(
        &self,
        key: &str,
        id: &Value,
    ) -> Result<BoxFut<Option<Value>>, TypertError> {
        if let Some(resolver) = self.lookup_resolvers.get(key) {
            return Ok(resolver(id));
        }
        let provider = self
            .lookups
            .get(key)
            .ok_or_else(|| TypertError::LookupRegistered {
                key: key.to_owned(),
            })?;
        Ok(provider.resolve(id))
    }

    // ---- Context adapters -----------------------------------------------------------------------

    /// Registers one Host Context adapter.
    ///
    /// # Errors
    ///
    /// Returns a validation error or [`TypertError::ContextRegistered`].
    pub fn register_host_context(
        &mut self,
        key: &str,
        adapter: HostContextAdapter,
    ) -> Result<ContextId, TypertError> {
        validate_segment("Context key", key)?;
        validate_wire_name("Context wire field", &adapter.wire)?;
        if adapter.wire_type_symbol.is_empty() {
            return Err(TypertError::Empty {
                subject: "Context wire type symbol",
            });
        }
        if self.host_contexts.contains_key(key) {
            return Err(TypertError::ContextRegistered {
                kind: ChangeKind::HostContext.as_str(),
                key: key.to_owned(),
            });
        }
        self.host_contexts.insert(key.to_owned(), adapter);
        self.emit(ChangeKind::HostContext, key);
        Ok(ContextId {
            kind: ChangeKind::HostContext,
            key: key.to_owned(),
        })
    }

    /// Registers one Client Context adapter.
    ///
    /// # Errors
    ///
    /// Returns a validation error or [`TypertError::ContextRegistered`].
    pub fn register_client_context(
        &mut self,
        key: &str,
        adapter: ClientContextAdapter,
    ) -> Result<ContextId, TypertError> {
        validate_segment("Context key", key)?;
        if self.client_contexts.contains_key(key) {
            return Err(TypertError::ContextRegistered {
                kind: ChangeKind::ClientContext.as_str(),
                key: key.to_owned(),
            });
        }
        self.client_contexts.insert(key.to_owned(), adapter);
        self.emit(ChangeKind::ClientContext, key);
        Ok(ContextId {
            kind: ChangeKind::ClientContext,
            key: key.to_owned(),
        })
    }

    /// Withdraws one Context adapter.
    pub fn withdraw_context(&mut self, id: &ContextId) {
        let removed = match id.kind {
            ChangeKind::HostContext => self.host_contexts.remove(&id.key).is_some(),
            ChangeKind::ClientContext => self.client_contexts.remove(&id.key).is_some(),
            _ => false,
        };
        if removed {
            self.emit(id.kind, &id.key);
        }
    }

    /// Reads one Host Context adapter.
    #[must_use]
    pub fn host_context(&self, key: &str) -> Option<HostContextAdapterView<'_>> {
        let adapter = self.host_contexts.get(key)?;
        Some(HostContextAdapterView {
            wire: &adapter.wire,
            wire_type_symbol: &adapter.wire_type_symbol,
            resource: adapter.resource.as_deref(),
            resolve: adapter,
        })
    }

    /// Reads one Client Context adapter.
    #[must_use]
    pub fn client_context(&self, key: &str) -> Option<&ClientContextAdapter> {
        self.client_contexts.get(key)
    }

    /// Replaces one Host Context adapter's default resolution policy.
    ///
    /// # Errors
    ///
    /// Returns [`TypertError::ContextConfigured`] when a resolver is already configured.
    pub fn configure_host_context<F>(
        &mut self,
        key: &str,
        resolve: F,
    ) -> Result<ContextId, TypertError>
    where
        F: Fn(&Value) -> BoxFut<Option<String>> + Send + Sync + 'static,
    {
        validate_segment("Context key", key)?;
        if self.host_context_resolvers.contains_key(key) {
            return Err(TypertError::ContextConfigured {
                key: key.to_owned(),
            });
        }
        self.host_context_resolvers
            .insert(key.to_owned(), Arc::new(resolve));
        self.emit(ChangeKind::HostContext, key);
        Ok(ContextId {
            kind: ChangeKind::HostContext,
            key: key.to_owned(),
        })
    }

    /// Resolves one Host Context identity through the configured policy, falling back to the adapter.
    ///
    /// # Errors
    ///
    /// Returns [`TypertError::ContextRegistered`] when no adapter and no resolver exist for the key.
    pub fn resolve_host_context(
        &self,
        key: &str,
        id: &Value,
    ) -> Result<BoxFut<Option<String>>, TypertError> {
        if let Some(resolver) = self.host_context_resolvers.get(key) {
            return Ok(resolver(id));
        }
        let adapter =
            self.host_contexts
                .get(key)
                .ok_or_else(|| TypertError::ContextRegistered {
                    kind: ChangeKind::HostContext.as_str(),
                    key: key.to_owned(),
                })?;
        Ok((adapter.resolve)(id))
    }

    // ---- resources ------------------------------------------------------------------------------

    /// Registers one resource kind's default identity extraction.
    ///
    /// # Errors
    ///
    /// Returns [`TypertError::GlobalReserved`] for `global`, a validation error, or
    /// [`TypertError::ResourceRegistered`].
    pub fn register_resource(
        &mut self,
        kind: &str,
        declaration: ResourceDeclaration,
    ) -> Result<ResourceId, TypertError> {
        validate_segment("resource kind", kind)?;
        if kind == GLOBAL_RESOURCE_KIND {
            return Err(TypertError::GlobalReserved);
        }
        if self.resources.contains_key(kind) {
            return Err(TypertError::ResourceRegistered {
                kind: kind.to_owned(),
            });
        }
        if let ResourceDeclaration::Field(field) = &declaration {
            validate_wire_name("resource field", field)?;
        }
        self.resources.insert(kind.to_owned(), declaration);
        self.emit(ChangeKind::Resource, kind);
        Ok(ResourceId {
            kind: kind.to_owned(),
        })
    }

    /// Withdraws one resource kind.
    pub fn withdraw_resource(&mut self, id: &ResourceId) {
        if self.resources.remove(&id.kind).is_some() {
            self.emit(ChangeKind::Resource, &id.kind);
        }
    }

    /// Reads one resource kind's declaration.
    #[must_use]
    pub fn resource(&self, kind: &str) -> Option<&ResourceDeclaration> {
        self.resources.get(kind)
    }

    /// Every registered kind with its declaration, in registration order.
    #[must_use]
    pub fn resource_entries(&self) -> Vec<ResourceEntry> {
        self.resources
            .iter()
            .map(|(kind, declaration)| ResourceEntry {
                kind: kind.clone(),
                declaration: declaration.clone(),
            })
            .collect()
    }

    // ---- change notification --------------------------------------------------------------------

    /// Observes later changes on the local descriptor registry.
    pub fn subscribe_local(&mut self, listener: ChangeListener) -> SubscriptionId {
        let id = self.mint_subscription();
        self.local_listeners.push((id, listener));
        id
    }

    /// Observes later changes on the Remote descriptor registry.
    pub fn subscribe_remote(&mut self, listener: ChangeListener) -> SubscriptionId {
        let id = self.mint_subscription();
        self.remote_listeners.push((id, listener));
        id
    }

    /// Observes later changes on every provider registry.
    pub fn subscribe(&mut self, listener: ChangeListener) -> SubscriptionId {
        let id = self.mint_subscription();
        self.listeners.push((id, listener));
        id
    }

    /// Withdraws one subscription from whichever registry it belongs to.
    ///
    /// A subscription id is a registry-local number, and the three id spaces are one sequence: a
    /// separate sequence per list would let one id name two subscriptions, and a caller holds an id
    /// and nothing else.
    pub fn unsubscribe(&mut self, id: SubscriptionId) {
        self.local_listeners
            .retain(|(candidate, _)| *candidate != id);
        self.remote_listeners
            .retain(|(candidate, _)| *candidate != id);
        self.listeners.retain(|(candidate, _)| *candidate != id);
    }

    /// Drains the warnings raised by failing listeners.
    ///
    /// Upstream writes these to `ctx.logger.warn` and keeps going; here they are values, so a test
    /// can assert containment and a host can log them wherever it likes.
    #[must_use]
    pub fn take_warnings(&self) -> Vec<String> {
        let mut warnings = lock(&self.warnings);
        std::mem::take(&mut *warnings)
    }

    // ---- internals ------------------------------------------------------------------------------

    /// Delivers the pending local-descriptor changes to the local subscribers.
    fn publish_local(&mut self) {
        let changes = self.local.drain();
        if changes.is_empty() {
            return;
        }
        let listeners = self.local_listeners.clone();
        for change in changes {
            notify(&listeners, &change, &mut lock(&self.warnings));
        }
    }

    /// Delivers the pending Remote-descriptor changes to the Remote subscribers.
    fn publish_remote(&mut self) {
        let changes = self.remote.drain();
        if changes.is_empty() {
            return;
        }
        let listeners = self.remote_listeners.clone();
        for change in changes {
            notify(&listeners, &change, &mut lock(&self.warnings));
        }
    }

    fn mint_subscription(&mut self) -> SubscriptionId {
        self.next_subscription += 1;
        self.next_subscription
    }

    fn owner(&mut self) -> Owner {
        self.next_owner += 1;
        self.next_owner
    }

    fn emit(&mut self, kind: ChangeKind, key: &str) {
        let change = RegistryChange::new(kind, key);
        let listeners = self.listeners.clone();
        notify(&listeners, &change, &mut lock(&self.warnings));
    }
}

/// One contributed schema, materialized at most once.
struct SchemaRecord {
    name: String,
    package: String,
    face: Face,
    key: String,
    /// `None` after the factory ran; the materialized value lives in `value`.
    factory: Option<SchemaFactory>,
    value: Option<Arc<ObjectSchema>>,
}

/// Materializes a schema record, running its factory at most once.
///
/// A factory is `FnOnce` and cannot be called twice, so the result is stored: the second access is
/// free rather than a second call, which is the property upstream's `toHaveBeenCalledOnce` asserts.
fn materialize(record: &mut SchemaRecord) -> SchemaRecordView {
    let schema = match (&record.value, record.factory.take()) {
        (Some(schema), _) => Arc::clone(schema),
        (None, Some(factory)) => {
            let schema = Arc::new(factory());
            record.value = Some(Arc::clone(&schema));
            schema
        }
        // Unreachable: the value is written whenever the factory is taken, and the factory is only
        // taken once. Returning an empty schema keeps the failure local if that ever changes.
        (None, None) => Arc::new(ObjectSchema::default()),
    };
    SchemaRecordView {
        name: record.name.clone(),
        schema,
        package: record.package.clone(),
        face: record.face,
        key: record.key.clone(),
    }
}

/// Locks a mutex, recovering from a poisoned one.
///
/// Poisoning means a listener panicked inside `notify`; the vector stays structurally valid, and the
/// panic has already been reported to the caller that caught it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
