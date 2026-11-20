//! Per-call resource resolution: which resources one call actually touches.
//!
//! # What upstream carries, measured
//!
//! `packages/api/remote-access` is 1,073 lines over 7 production sources and exports 22 symbols of
//! its own — 6 classes or functions and 16 `type`/`interface` declarations — plus 5 protocol types
//! it re-exports from `typert-protocol`. Its deciding code groups into five families, of which this
//! crate covers the first two only partially:
//!
//! | Family | Upstream source | Lines | What it decides |
//! | --- | --- | --- | --- |
//! | Roles and bindings | `types.ts` | 142 (whole file) | A role names a resource kind and the actions it allows there; a binding joins one role to one resource, optionally behind a condition |
//! | Conditions | `types.ts` (`RemoteAccessCondition`) | ~20 | A binding may refuse one action on its own resource, given a concrete call's validated arguments |
//! | Decisions | `index.ts` (`decide`, `evaluate`, `explain`, `allows`) | ~120 | Allowed only when *every* listed resource has a binding whose role lists the action and whose condition allows; a policy failure is contained, and the reason is the first refusing condition's, else `explain()`, else `no-allow` |
//! | `whenAllowed` | `client/when-allowed.ts` | 85 (whole file) | Install a callback's effects while one access answer is true, dispose them in reverse when it turns false, reinstall on the next true |
//! | Mount and snapshot | `client/mount.ts`, `client/state.ts`, `snapshot.ts`, `index.ts` (`follow`, `snapshotFor`, `grantsFor`, `AccessFollower`) | ~700 | The client service over a Peer's snapshot, one baseline then `upsert`/`remove` frames, and the injected page baseline |
//!
//! The resources those decisions range over are **not** resolved by this package: upstream
//! `packages/api/gateway/src/access.ts` (179 lines) does it in `callResources` and `eventResources`,
//! from Typert-validated arguments, before any lookup or Context is activated.
//!
//! # What this crate already covers
//!
//! | Local item | File | Upstream family it answers |
//! | --- | --- | --- |
//! | [`Access`], [`AccessDecision`], [`Policy`], [`AccessRequest`] | `access.rs` | The decision vocabulary: an action class, a reason-carrying answer, and the trait a deployment swaps |
//! | [`DenyAll`], [`ReadOnly`], [`AllowList`] | `access.rs` | Three whole-policy shortcuts, keyed on the peer name or on read-versus-write |
//! | [`Declaration`] | `access.rs` | Part of the static half of roles: one action class and a resource list |
//! | `RemoteRegistry::declare`, `authorise` | `remote.rs` | Decisions consulted before a handler runs, with a per-method declaration |
//! | `resources_of` | `api_remotes_gap.rs` | `eventResources` for the *forwardable-event table*, from positional values |
//!
//! So decisions and the static declaration exist, and **per-call resolution does not**.
//!
//! # The gap this module closes
//!
//! [`Declaration`] names `Resource::any_of("session")` — an empty id — and
//! `RemoteRegistry::authorise` passes exactly that to every policy, so a policy asked "may this peer
//! call `session.delete`" sees `session:` (any) on every call and cannot tell `delete(session-a)`
//! from `delete(session-b)`. Every real policy therefore has to allow or refuse the whole method,
//! which is the hole this module removes.
//!
//! [`MethodAccessResolver`] ports upstream's `callResources`, plus the bridge from resolved ids to
//! the request a [`Policy`] is asked about. Five rules are load-bearing and are ported verbatim:
//!
//! 1. **Later sources replace earlier ones for a kind while keeping the kind's position.** The
//!    sources are a kind's registered default declaration, then the Context receiver identity and
//!    every lookup parameter, then the method's own reader — last answer wins per kind, and the
//!    order of the kinds is the order the first source mentioned them.
//! 2. **A method's reader receives positional arguments**, built from the declared parameters in
//!    call order plus the cancellation slot, because upstream's readers are written against the
//!    source signature.
//! 3. **A malformed answer is `invalid`, never empty.** A declaration that yields an array, an id
//!    that is not a non-empty string, a reader that returns an empty list: the whole call is
//!    unresolvable, and upstream denies it with `resource-invalid` rather than authorizing it
//!    against nothing.
//! 4. **The default field falls back to the first object argument**, which is what lets a resource
//!    be named inside a nested request object.
//! 5. **No sources at all means `global`**, the one reserved kind that carries no id.
//!
//! # Why the resolved resources do not go through `Declaration`
//!
//! They cannot: [`AccessRequest`] is a fixed four-field struct whose `resources` are
//! [`AccessResource`], and an `AccessResource` names one `(kind, id)`. That is exactly the shape a
//! per-call decision needs, so [`CallRequest::to_access_request`] fills it and the mounted
//! [`Policy`] sees real ids instead of a wildcard. What local `AccessRequest` cannot carry is the
//! call's arguments, so [`CallRequest`] keeps them beside the resources for a policy that wants
//! upstream's condition half; adding a field to `access.rs` is the integrator's call, not this
//! module's. [`CallRequest::decide`] is the end-to-end path: resolve, then ask, then act on the
//! answer — with the refusal reason preserved.
//!
//! # What this module deliberately does not do
//!
//! [`MethodAccessResolver::resolve_event`] adapts to the existing local forwardable-event table
//! instead of declaring per-event readers. Upstream's `eventResources` takes readers named by each
//! event entry, so one event may name several kinds; the local table is a fixed function of
//! `(event, args)` and is the authoritative declaration this build has. `undefined` from that table
//! is not an empty answer but an invalid one, because an event carries *every* resource it
//! declares — a notification that lost its resources must reach no non-owner.
//!
//! # Compilation note
//!
//! The crate root does not declare this module yet; the integrator adds `pub mod remote_access_gap;`
//! and the `pub use`. Library items are spelled `girder_core::…` because the test binary compiles
//! this exact file by `#[path]`, where a bare `crate::` would name the *test* crate. The one
//! exception is [`typert_gap`](crate::typert::registry), which is private to the library: it is spelled
//! `crate::typert::registry`, so it resolves to the library module there and to the sibling module the
//! test includes by the same path.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use girder_core::{Access, AccessDecision, AccessRequest, AccessResource, Resource, resources_of};
use serde_json::Value;

use crate::typert::registry::{
    GLOBAL_RESOURCE_KIND, InvocationDescriptor, Receiver, ResourceDeclaration, ResourceIdentity,
    ResourceResolveInput, TypertRegistry,
};

/// The reason upstream reports when a call's resources cannot be resolved.
///
/// A reason rather than a distinct error variant, so it travels the same path as every other
/// refusal: a client already reads `reason` to decide what to tell its operator, and a second
/// plumbing route for one value would be a second thing to get wrong.
pub const RESOURCE_INVALID: &str = "resource-invalid";

/// The reason upstream reports when no binding allows a request.
///
/// Unused inside this module — a decision's reason comes from the policy or from a condition — but
/// carried because it is the other half of the refusal vocabulary a client branches on, and a
/// caller writing its own `explain()` should not have to invent the spelling.
#[allow(dead_code)]
pub const NO_ALLOW: &str = "no-allow";

/// One resource a call touches.
///
/// Deliberately not [`AccessResource`]: that type builds a wildcard out of an empty id, and a
/// wildcard is precisely what this module exists to stop producing. A `ResolvedResource` always
/// names one concrete identity — or the reserved global kind, which carries none.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolvedResource {
    /// The resource kind: `session`, `extension`, `workspace`, or the reserved `global`.
    pub kind: String,
    /// Which one. `None` only for the global kind.
    pub id: Option<String>,
}

impl ResolvedResource {
    /// The global resource: the only one that names no identity.
    #[must_use]
    pub fn global() -> Self {
        Self {
            kind: GLOBAL_RESOURCE_KIND.to_owned(),
            id: None,
        }
    }

    /// One identity of one kind.
    #[must_use]
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: Some(id.into()),
        }
    }

    /// Whether this is the reserved global resource.
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.kind == GLOBAL_RESOURCE_KIND && self.id.is_none()
    }

    /// The identity, when this resource names one.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Converts to the resource an [`AccessRequest`] carries.
    ///
    /// The global kind keeps its empty id: no real resource has one, so a policy comparing ids
    /// cannot confuse "the whole host" with "one session without a name".
    #[must_use]
    pub fn to_access_resource(&self) -> AccessResource {
        AccessResource::new(self.kind.clone(), self.id.clone().unwrap_or_default())
    }
}

impl fmt::Display for ResolvedResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            Some(id) => write!(formatter, "{}:{id}", self.kind),
            None => formatter.write_str(&self.kind),
        }
    }
}

/// What one resolution attempt answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The resources the call touches: never empty, one entry per id, ids non-empty.
    Resources(Vec<ResolvedResource>),
    /// The resources could not be named, so the call must be refused as `resource-invalid`.
    Invalid,
}

impl Resolved {
    /// Whether resolution succeeded.
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        matches!(self, Self::Resources(_))
    }

    /// The resources, when resolution succeeded.
    #[must_use]
    pub fn resources(&self) -> Option<&[ResolvedResource]> {
        match self {
            Self::Resources(resources) => Some(resources),
            Self::Invalid => None,
        }
    }

    /// The refusal reason this answer implies, mirroring the Gateway's deny for a failed resolve.
    ///
    /// A resolved call has no reason of its own — its policy decides — so `None` here rather than an
    /// empty string keeps "nothing to say" distinct from "refused with no stated reason", which is a
    /// distinction an audit log depends on.
    #[must_use]
    pub fn refusal(&self) -> Option<&'static str> {
        match self {
            Self::Resources(_) => None,
            Self::Invalid => Some(RESOURCE_INVALID),
        }
    }
}

/// The ids one reader named: one, several, or explicitly none.
///
/// Several is legitimate for a reader — one call may touch a set of sessions — but an empty list is
/// not: a reader that ran and found nothing has contradicted itself, which upstream calls `invalid`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceIds {
    /// Exactly one identity.
    One(String),
    /// Several identities.
    Many(Vec<String>),
}

impl ResourceIds {
    /// The ids, in the order the reader named them.
    #[must_use]
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(id) => vec![id],
            Self::Many(ids) => ids,
        }
    }
}

impl From<String> for ResourceIds {
    fn from(id: String) -> Self {
        Self::One(id)
    }
}

impl From<&str> for ResourceIds {
    fn from(id: &str) -> Self {
        Self::One(id.to_owned())
    }
}

/// What a method's own resource reader answers for one call.
///
/// Mirrors upstream's `TypertResourceReader`: the arguments are *positional*, built from the
/// descriptor's parameter order, so a reader written against the source signature works unchanged.
/// `None` is upstream's `undefined` — this reader cannot name a resource here — which leaves the
/// kind absent from the call. A reader that *did* run and named ids must name at least one usable
/// id; upstream reads an empty list as `invalid`, and so does this module.
pub type ResourceReader = Arc<dyn Fn(&[Value]) -> Option<ResourceIds> + Send + Sync>;

/// One kind's answer while the sources are being merged.
enum Source {
    /// The kind is not touched by this call.
    Absent,
    /// At least one usable id, already validated.
    Ids(Vec<String>),
    /// A source failed, so the call is unresolvable.
    Invalid,
}

/// Whether one id is usable as a resource identity.
///
/// Empty is refused rather than normalized: an empty id is how [`Declaration`] spells "any", and
/// accepting one here would reintroduce the wildcard this module removes.
fn usable(id: &str) -> bool {
    !id.is_empty()
}

/// Appends one kind's ids, reporting whether the call can still be authorized.
fn append_kind(resources: &mut Vec<ResolvedResource>, kind: &str, source: &Source) -> bool {
    match source {
        Source::Invalid => false,
        Source::Absent => true,
        Source::Ids(ids) => {
            let mut added = 0;
            for id in ids {
                if !usable(id) {
                    return false;
                }
                resources.push(ResolvedResource::new(kind.to_owned(), id.clone()));
                added += 1;
            }
            added > 0
        }
    }
}

/// Which resource the local forwardable-event table named for one entry.
fn from_forwarded(resource: &Resource) -> Option<ResolvedResource> {
    match resource.id() {
        Some(id) if usable(id) => Some(ResolvedResource::new(resource.kind(), id.to_owned())),
        // An id that is present but empty is a malformed declaration, not an absent one.
        Some(_) => None,
        // The local table's `global` resource is the one case a resource legitimately has no id.
        None => Some(ResolvedResource {
            kind: resource.kind().to_owned(),
            id: None,
        }),
    }
}

/// The resources one forwarded event touches, from the local forwardable-event declaration.
///
/// This is upstream's `eventResources` over the table this build already keeps. An error from that
/// table — an event this build does not forward — and a malformed id both mean the notification
/// cannot be authorized, so both answer [`Resolved::Invalid`]: a frame whose resources are unknown
/// must reach no non-owner, and treating it as global would broadcast it to everyone.
#[must_use]
pub fn resolve_event_resources(event: &str, args: &[Value]) -> Resolved {
    let Ok(resources) = resources_of(event, args) else {
        return Resolved::Invalid;
    };
    let mut resolved = Vec::with_capacity(resources.len());
    for resource in &resources {
        let Some(mapped) = from_forwarded(resource) else {
            return Resolved::Invalid;
        };
        resolved.push(mapped);
    }
    // No declaration means the global resource alone, which is what the local table answers for an
    // event that names none; an empty *declared* list would be a table bug, not a global event.
    if resolved.is_empty() {
        return Resolved::Resources(vec![ResolvedResource::global()]);
    }
    Resolved::Resources(resolved)
}

/// Resolves the resources of calls and events for one registry.
///
/// Holds no state beyond the registry it is asked about, so it cannot drift from the declarations
/// currently registered and is as cheap to build as the reference it wraps.
pub struct MethodAccessResolver<'a> {
    typert: &'a TypertRegistry,
}

impl<'a> MethodAccessResolver<'a> {
    /// Resolves against one registry.
    #[must_use]
    pub fn new(typert: &'a TypertRegistry) -> Self {
        Self { typert }
    }

    /// The resources one call touches, from its descriptor and validated arguments.
    ///
    /// Sources are merged per kind in this order, each later one replacing the earlier answer for
    /// its kind while keeping the kind's position: the kind's registered default declaration, then
    /// the Context receiver identity and every lookup parameter, then the method's own reader.
    ///
    /// # Errors
    ///
    /// `Resolved::Invalid` — never an empty list — when a default declaration yields an array, a
    /// reader returns an empty list or an unusable id, or any id is not a non-empty string.
    #[must_use]
    pub fn resolve_call(
        &self,
        descriptor: &InvocationDescriptor,
        readers: &BTreeMap<String, ResourceReader>,
        args: &BTreeMap<String, Value>,
        signal: Option<&Value>,
    ) -> Resolved {
        let positional = positional_args(descriptor, args, signal);
        let mut sources: Vec<(String, Source)> = Vec::new();

        for entry in self.typert.resource_entries() {
            let source = declaration_source(&entry.declaration, descriptor, args);
            replace(&mut sources, &entry.kind, source);
        }
        for (kind, ids) in self.lookup_sources(descriptor, args) {
            replace(&mut sources, &kind, Source::Ids(ids));
        }
        for (kind, reader) in readers {
            let source = reader_source(reader, &positional);
            replace(&mut sources, kind, source);
        }

        let mut resources = Vec::new();
        for (kind, source) in &sources {
            if !append_kind(&mut resources, kind, source) {
                return Resolved::Invalid;
            }
        }
        if resources.is_empty() {
            return Resolved::Resources(vec![ResolvedResource::global()]);
        }
        Resolved::Resources(resources)
    }

    /// The request a policy should be asked about for one call.
    ///
    /// This is the step upstream keeps implicit: the resources a decision ranges over are resolved
    /// from the call's own arguments, so a policy can be a per-call rule rather than a per-method
    /// table lookup.
    ///
    /// # Errors
    ///
    /// Returns [`Refused`] carrying [`RESOURCE_INVALID`] when the resources cannot be named. The
    /// caller must refuse *before* asking any policy, which is upstream's order: access precedes
    /// lookup resolution, so a refused call activates nothing.
    pub fn request(
        &self,
        descriptor: &InvocationDescriptor,
        readers: &BTreeMap<String, ResourceReader>,
        peer: &str,
        access: Access,
        args: &BTreeMap<String, Value>,
        signal: Option<&Value>,
    ) -> Result<CallRequest, Refused> {
        let resolved = self.resolve_call(descriptor, readers, args, signal);
        let Some(resources) = resolved.resources() else {
            return Err(Refused::new(RESOURCE_INVALID));
        };
        Ok(CallRequest {
            peer: peer.to_owned(),
            method: format!("{}/{}", descriptor.namespace, descriptor.method),
            access,
            resources: resources.to_vec(),
            args: args.clone(),
        })
    }

    /// The resource ids named by the Context receiver identity and by lookup parameters.
    ///
    /// The Context identity comes first, then lookup parameters in call order, matching upstream's
    /// `lookupResources`. An optional lookup the call omits names no id; the Context identity is
    /// required and is therefore always read when its adapter declares a kind.
    fn lookup_sources(
        &self,
        descriptor: &InvocationDescriptor,
        args: &BTreeMap<String, Value>,
    ) -> Vec<(String, Vec<String>)> {
        let mut ids: Vec<(String, Vec<String>)> = Vec::new();

        if let Receiver::Context { context, wire } = &descriptor.receiver {
            if let Some(adapter) = self.typert.host_context(context) {
                if let (Some(kind), Some(value)) = (adapter.resource(), args.get(wire)) {
                    add_id(&mut ids, kind, id_of(value));
                }
            }
        }
        for parameter in &descriptor.parameters {
            let Some(key) = parameter.lookup_key() else {
                continue;
            };
            let Some(provider) = self.typert.lookup(key) else {
                continue;
            };
            let Some(kind) = provider.resource() else {
                continue;
            };
            let Some(value) = args.get(&parameter.wire) else {
                continue;
            };
            add_id(&mut ids, kind, id_of(value));
        }
        ids
    }
}

impl fmt::Debug for MethodAccessResolver<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MethodAccessResolver").finish()
    }
}

/// Records one kind's answer, replacing an earlier one and keeping the kind's position.
fn replace(sources: &mut Vec<(String, Source)>, kind: &str, source: Source) {
    if let Some(slot) = sources.iter_mut().find(|(existing, _)| existing == kind) {
        slot.1 = source;
    } else {
        sources.push((kind.to_owned(), source));
    }
}

/// Appends one id to a kind's list, keeping kinds in first-mention order.
fn add_id(ids: &mut Vec<(String, Vec<String>)>, kind: &str, id: String) {
    if let Some(slot) = ids.iter_mut().find(|(existing, _)| existing == kind) {
        slot.1.push(id);
    } else {
        ids.push((kind.to_owned(), vec![id]));
    }
}

/// One kind's default declaration, or absence when it names nothing for this call.
///
/// A field-based declaration is read here rather than through the registry's own `extract` so the
/// nested-object fallback survives: a field that is not a declared parameter names a field of the
/// first object argument, which is how a resource carried inside a request object is found.
fn declaration_source(
    declaration: &ResourceDeclaration,
    descriptor: &InvocationDescriptor,
    args: &BTreeMap<String, Value>,
) -> Source {
    let answer = match declaration {
        ResourceDeclaration::Field(field) => field_identity(field, descriptor, args),
        ResourceDeclaration::Resolver(_) => declaration.extract(&ResourceResolveInput::new(
            args.clone(),
            wire_pairs(descriptor),
        )),
    };
    match answer {
        ResourceIdentity::Identity(id) => {
            if usable(&id) {
                Source::Ids(vec![id])
            } else {
                Source::Invalid
            }
        }
        ResourceIdentity::Absent => Source::Absent,
        // An array where one identity belongs is upstream's rejection: authorizing against the
        // first element would be a hole dressed as a policy.
        ResourceIdentity::Many(_) => Source::Invalid,
    }
}

/// The identity one field declaration names, with upstream's nested-object fallback.
///
/// A field that is a declared parameter is read from the top level, exactly as the wire carries it.
/// Otherwise it names a field of the first *object* argument: that is where a resource carried
/// inside a request object lives, and reading only the top level would silently resolve such a call
/// to global. The first object argument decides — if it lacks the field, the call names nothing,
/// which is upstream's rule rather than a scan of later arguments.
fn field_identity(
    field: &str,
    descriptor: &InvocationDescriptor,
    args: &BTreeMap<String, Value>,
) -> ResourceIdentity {
    if descriptor
        .parameters
        .iter()
        .any(|parameter| parameter.wire == field)
    {
        return identity_of(args.get(field));
    }
    for parameter in &descriptor.parameters {
        let Some(value) = args.get(&parameter.wire) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        return identity_of(value.get(field));
    }
    ResourceIdentity::Absent
}

/// One value as a resource identity: absent, or the string an id has to be.
///
/// A non-string scalar is rendered rather than refused, because a generated descriptor may key a
/// resource by a number; null is absence, and an object or array is not an identity at all and is
/// refused downstream by [`usable`].
fn identity_of(value: Option<&Value>) -> ResourceIdentity {
    match value {
        None | Some(Value::Null) => ResourceIdentity::Absent,
        Some(Value::String(id)) => ResourceIdentity::Identity(id.clone()),
        Some(other) => ResourceIdentity::Identity(other.to_string()),
    }
}

/// One method reader's answer as a merged source.
fn reader_source(reader: &ResourceReader, positional: &[Value]) -> Source {
    match reader(positional) {
        None => Source::Absent,
        Some(ids) => {
            let ids = ids.into_vec();
            if ids.is_empty() || !ids.iter().all(|id| usable(id)) {
                Source::Invalid
            } else {
                Source::Ids(ids)
            }
        }
    }
}

/// The request a policy is asked about, with the resources already resolved.
///
/// Distinct from [`AccessRequest`] only by keeping the call's arguments, which local `AccessRequest`
/// has nowhere to put. Both travel together, so a policy sees `session:abc-1` rather than
/// `session:` and, when it wants it, the argument that named it.
#[derive(Clone, Debug, PartialEq)]
pub struct CallRequest {
    /// Who is asking.
    pub peer: String,
    /// The endpoint the call names, as `namespace/method` — upstream's `RemoteAccessAction` spelling.
    pub method: String,
    /// What the call does.
    pub access: Access,
    /// The concrete resources it touches, resolved from its arguments.
    pub resources: Vec<ResolvedResource>,
    /// The call's validated arguments, for a policy that answers per call rather than per method.
    pub args: BTreeMap<String, Value>,
}

impl CallRequest {
    /// Projects to the [`AccessRequest`] a mounted [`Policy`] receives.
    ///
    /// The arguments are dropped here — that type has no field for them — which is why
    /// [`CallRequest::decide`] passes them alongside instead of through it.
    #[must_use]
    pub fn to_access_request(&self) -> AccessRequest {
        AccessRequest {
            peer: self.peer.clone(),
            method: self.method.clone(),
            access: self.access,
            resources: self
                .resources
                .iter()
                .map(ResolvedResource::to_access_resource)
                .collect(),
        }
    }

    /// Asks one policy about this call.
    ///
    /// The single call site that both resolves and decides, which is what makes the per-call
    /// guarantee testable: the policy receives ids, not a wildcard.
    #[must_use]
    pub fn decide(&self, policy: &dyn girder_core::Policy) -> AccessDecision {
        policy.decide(&self.to_access_request())
    }
}

/// A refusal that happens before any policy is consulted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// Why, in the vocabulary a client branches on.
    pub reason: String,
}

impl Refused {
    /// Builds a refusal.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "access refused: {}", self.reason)
    }
}

impl std::error::Error for Refused {}

/// The wire identity of one argument, as a string.
///
/// A JSON string is used verbatim; any other scalar is rendered, because a generated descriptor may
/// key a resource by a number. An object, array, or null is not an identity and yields the empty
/// string, which [`usable`] then refuses.
fn id_of(value: &Value) -> String {
    match value {
        Value::String(id) => id.clone(),
        Value::Null | Value::Array(_) | Value::Object(_) => String::new(),
        other => other.to_string(),
    }
}

/// The positional arguments one call's resource reader receives.
///
/// Declared parameters in call order, then the cancellation slot when the descriptor takes one —
/// the shape upstream builds because its readers are written against the source signature. A
/// parameter the call omitted is `null`, which keeps positions aligned rather than shifting every
/// later argument left.
fn positional_args(
    descriptor: &InvocationDescriptor,
    args: &BTreeMap<String, Value>,
    signal: Option<&Value>,
) -> Vec<Value> {
    let mut positional = Vec::with_capacity(descriptor.parameters.len() + 1);
    for parameter in &descriptor.parameters {
        positional.push(args.get(&parameter.wire).cloned().unwrap_or(Value::Null));
    }
    if descriptor.cancellation.is_some() {
        positional.push(signal.cloned().unwrap_or(Value::Null));
    }
    positional
}

/// The declared parameters as `(name, wire)` pairs, so a resolver can match either.
fn wire_pairs(descriptor: &InvocationDescriptor) -> Vec<(String, String)> {
    descriptor
        .parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), parameter.wire.clone()))
        .collect()
}

/// The kinds one resolved call touches, without duplicates.
///
/// Separate from [`Resolved`] because the caller that wants it is usually asking "did this call
/// cross a kind boundary", which is a policy question rather than a resolution one.
#[must_use]
pub fn kinds_of(resources: &[ResolvedResource]) -> BTreeSet<&str> {
    resources
        .iter()
        .map(|resource| resource.kind.as_str())
        .collect()
}

/// Whether every resolved resource satisfies `allowed`.
///
/// Upstream's decision rule in one function: a request is allowed only when it is allowed on
/// **every** listed resource, so a call touching two sessions is refused when either is out of
/// reach. A policy that spelled the quantifier itself would eventually write `any` and hand out a
/// partial grant.
#[must_use]
pub fn all_allowed(
    resources: &[ResolvedResource],
    mut allowed: impl FnMut(&ResolvedResource) -> bool,
) -> bool {
    !resources.is_empty() && resources.iter().all(&mut allowed)
}

/// A reader that computes ids from positional arguments.
#[must_use]
pub fn reader<F>(read: F) -> ResourceReader
where
    F: Fn(&[Value]) -> Option<ResourceIds> + Send + Sync + 'static,
{
    Arc::new(read)
}

/// A declaration that computes its default identity from a call.
#[must_use]
pub fn declaration<F>(resolve: F) -> ResourceDeclaration
where
    F: Fn(&ResourceResolveInput) -> ResourceIdentity + Send + Sync + 'static,
{
    ResourceDeclaration::resolver(resolve)
}
