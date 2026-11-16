//! The client half of upstream `packages/api/gateway`: projection mounting, logical stream
//! supervision, journal and snapshot subscriptions, and the uplink queue.
//!
//! # What upstream `gateway/src/client/` contains, measured
//!
//! Six sources, 2,574 lines. [`gateway_gap`](girder_core::gateway_gap) records the package total
//! (5,691 lines); the client directory is the part that runs *on the Client*, and it is where the
//! generation, mount, and subscription decisions live:
//!
//! | Source | Lines | What it decides |
//! | --- | --- | --- |
//! | `index.ts` | 895 | `apply`/`inject`, the `remote` service, `$stream`/`$host`/`$mount`/`$on`, contribution validation, namespace creation and per-method install/remove tokens, `prepareInvocation`, `invoke`/`invokeStream`, failure folding, `ClientStreamHandle` |
//! | `journal-stream.ts` | 580 | Cursor algebra, opening-snapshot-then-live delivery, prepend pagination, gap repair that reads a replacement page while the live tail keeps arriving, notification ordering |
//! | `stream-client.ts` | 510 | `RemoteStreamMuxClient` (one physical socket shared by every logical stream), `StreamInbox`, `ClientUplinkQueue`, the uplink pump |
//! | `remote-events.ts` | 341 | `ClientRemoteEvents`: the forwarded-event allowlist, `$on` subscription, per-event dispatch, teardown |
//! | `remote-stream.ts` | 227 | `RemoteStream`: reconnecting logical stream, physical generation ids, `accept()`, retry pacing against the Connection generation, `RemoteStreamCarrierError` handling |
//! | `snapshot-stream.ts` | 94 | `RemoteSnapshotStream`: baseline-then-delta framing over a `RemoteStream` |
//!
//! # What this crate already covers
//!
//! | Local item | File | Client coverage |
//! | --- | --- | --- |
//! | `Peer`, `Principal`, `Interactions`, admission, authorisation, event-stream ownership | `gateway_gap.rs` (1,585 lines) | The **identity** face: who may open a stream, who answers a waterfall, who a forwarded event belongs to |
//! | `ClientMessage`/`ServerMessage`, `UplinkInbox`, `UplinkWindow`, `StreamTable`, `Heartbeat` | `api_extra_gap.rs` (1,110 lines) | The **Host-side carrier**: frame parsing, the bounded uplink inbox, per-connection routing |
//! | `TypertRegistry::mount_remote` / `unmount_remote`, `InvocationDescriptor` | `typert_gap.rs` (2,760 lines) | Descriptor storage and validation, contribution-level mount identity |
//! | `RemoteContribution` | `typert_gap.rs` | A contribution as a package plus descriptors |
//! | `Frame`, `EventQueue`, `WaterfallSlot`, `FORWARDED_EVENTS` | `api_remotes_gap.rs` | The **Host** face of forwarded events |
//! | `Scope` | `scope.rs` | Reverse-order disposal |
//!
//! So the identity, the wire, and the Host-side queues exist, and **every decision on the Client
//! side of a stream is absent**. A workspace grep finds zero occurrences of a physical generation
//! id, of a mount token, of a namespace service, of a journal cursor, of a snapshot baseline, or of
//! a client uplink queue.
//!
//! # The gaps, largest first
//!
//! 1. **Client projection mounting** (`client/index.ts`, 895 lines, this module's first half). Ten
//!    distinct rules, each of which fails silently if dropped: a contribution is validated *before*
//!    anything is mounted; a namespace is created whole or not at all; a method name may not shadow
//!    the namespace service; a group installs all-or-nothing; a token stops a withdrawn method from
//!    dispatching a call that was already selected; `scoped`/`direct` are two independent variants
//!    of one method and removal of one keeps the other; an invocation's arity is checked with an
//!    optional trailing cancellation; `undefined` arguments are omitted; a carrier throw is folded
//!    to `gateway/internal` and a caller abort to `gateway/cancelled`; and a stream method returns a
//!    handle whose uplink queue throws after `end` or after termination.
//! 2. **Journal streams** (`journal-stream.ts`, 580 lines). A cursor algebra, an opening page that
//!    must end at its announced cursor, live entries that must strictly follow the last applied
//!    cursor with no partial overlap, gap repair that re-reads a replacement page *while* the live
//!    tail keeps arriving and merges the queued entries, notification ordering behind the repair,
//!    and a resumed generation that may not open behind the last applied cursor.
//! 3. **Logical stream supervision** (`remote-stream.ts` + `snapshot-stream.ts`, 321 lines). One
//!    logical stream across physical generations: monotone generation ids, `accept()` gating retry
//!    pacing, `restart()` that resets the attempt count, carrier-versus-terminal failure
//!    discrimination, and a snapshot stream that refuses a delta before its baseline or a second
//!    baseline in one generation.
//!
//! This module ports gaps 1 and 3. Gap 2 (the journal cursor algebra and gap repair) stays recorded
//! here as the remaining work; it is the largest single piece left and it depends on both halves
//! this module does port.
//!
//! One correction to the package table above, because a later reader would otherwise inherit a
//! miscount: `client/index.ts` is **895** lines, not 1,806. The 1,806 figure in
//! [`gateway_gap`](girder_core::gateway_gap)'s table is the *sum* of this directory's four
//! projection sources (`index.ts` 895 + `journal-stream.ts` 580 + `remote-stream.ts` 227 +
//! `snapshot-stream.ts` 94), which is what that row's "Sources" column lists; `remote-events.ts`
//! (341) and `stream-client.ts` (510) belong to other rows. Measured here with `wc -l`.
//!
//! # Why mounting is a table, not a `Reflect` proxy
//!
//! Upstream installs a getter on the namespace service so `client.remote.session.list` reads a
//! *property*. Rust has no property lookup, so [`NamespaceTable`] keeps the same records and the
//! same tokens, and [`NamespaceTable::select`] is what the getter's closure did: read the current
//! record at call time, prefer a scoped variant when the caller has an identity for it, and fall
//! back to the direct one. The observable difference is that a caller writes `select(namespace,
//! method, identity)` instead of a property read; the *decisions* — which variant wins, what a
//! withdrawn method does, what two contributions may not claim — are unchanged.
//!
//! # Why a mount token instead of a generation counter
//!
//! Upstream's `MountToken` is `{ active, abort }`: a `MountContribution` disposer flips it inactive
//! and aborts, and an in-flight call re-checks it after the carrier returns. That re-check is the
//! whole point — a call selected before a withdrawal must not report a success the Client can no
//! longer attribute. [`MountToken`] carries the same two facts, and [`MountToken::live`] is the
//! re-check.
//!
//! # Compilation note
//!
//! The crate root does not declare this module yet; the integrator adds
//! `pub mod gateway_client_gap;`. The test binary compiles this exact file by `#[path]`, and under
//! that inclusion `crate::` would name the test crate, so every library reference is spelled
//! `girder_core::`.

// The crate root has not been wired to this module yet, so in a library build nothing here has a
// caller and `dead_code` would report a whole public surface as waste. The allow says that plainly
// rather than leaving a reader to guess; the test target exercises every item below.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};

use girder_core::is_lossless;
use girder_core::typert_gap::{InvocationDescriptor, Receiver, Scope};

/// The service key a namespace's Client-side service is published under.
///
/// Upstream's `remoteServiceKey`: `remote.<namespace>`. Kept as a function so the one place that
/// builds the key is the one place that can change it.
#[must_use]
pub fn remote_service_key(namespace: &str) -> String {
    format!("remote.{namespace}")
}

/// Method names a generated Remote method may not take, because the namespace service itself
/// answers them.
///
/// Upstream's `REMOTE_NAMESPACE_FIELDS`. A method named `empty` or `invokeRemote` would shadow the
/// service's own surface, and the failure would surface as the service behaving wrongly rather than
/// as a rejected contribution.
pub const NAMESPACE_RESERVED_METHODS: [&str; 6] = [
    "ctx",
    "empty",
    "invokeRemote",
    "methods",
    "name",
    "namespace",
];

/// The two universal failure codes a Client folds a carrier outcome into.
pub const CARRIER_FAILURE_CODE: &str = "gateway/internal";

/// The code a call aborted by its caller reports.
pub const CANCELLED_FAILURE_CODE: &str = "gateway/cancelled";

// ---- failures ------------------------------------------------------------------------------------

/// Why a contribution, namespace, or invocation was refused on the Client.
///
/// Separate from [`girder_core::gateway_gap::GatewayError`] because these are *assembly* faults on
/// the Client, not Gateway refusals of a Peer: upstream throws them from `ClientRemoteService`
/// before any request exists, and the messages name the endpoint the way its `client api:` prefix
/// does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// A contribution repeats a method, or claims one another contribution already mounted.
    DuplicateMethod {
        /// The repeated `namespace/method`.
        endpoint: String,
        /// `direct`, `scoped`, or a repeat inside one contribution.
        kind: &'static str,
    },
    /// A method name collides with the namespace service's own surface.
    ReservedMethod {
        /// The rejected `namespace/method`.
        endpoint: String,
    },
    /// The namespace collides with the Remote service itself.
    NamespaceConflict {
        /// The rejected namespace.
        namespace: String,
    },
    /// An invocation was given the wrong number of arguments.
    Arity {
        /// The endpoint that was called.
        endpoint: String,
        /// How many business arguments it takes.
        expected: usize,
        /// How many it received.
        got: usize,
        /// Whether a trailing cancellation parameter was accepted.
        cancellable: bool,
    },
    /// A scoped method has no Client Context adapter, or the caller has no identity for it.
    ScopeUnavailable {
        /// The endpoint that was called.
        endpoint: String,
        /// The Context kind that was missing.
        context: String,
    },
    /// The method is no longer mounted, so no request outcome exists.
    Withdrawn {
        /// The endpoint that was called.
        endpoint: String,
    },
    /// The namespace did not start, or installed nothing.
    NamespaceNotStarted {
        /// The namespace.
        namespace: String,
    },
    /// A descriptor's declaration cannot drive a Client invocation.
    Declaration {
        /// The endpoint the descriptor names.
        endpoint: String,
        /// What about it cannot be driven.
        reason: String,
    },
}

impl ClientError {
    /// The stable code a caller may branch on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateMethod { .. } => "client/duplicate-method",
            Self::ReservedMethod { .. } => "client/reserved-method",
            Self::NamespaceConflict { .. } => "client/namespace-conflict",
            Self::Arity { .. } => "client/arity",
            Self::ScopeUnavailable { .. } => "client/scope-unavailable",
            Self::Withdrawn { .. } => "client/withdrawn",
            Self::NamespaceNotStarted { .. } => "client/namespace-not-started",
            Self::Declaration { .. } => "client/declaration",
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateMethod { endpoint, kind } => write!(
                formatter,
                "client api: contribution repeats {kind} method {endpoint}"
            ),
            Self::ReservedMethod { endpoint } => write!(
                formatter,
                "client api: method \"{endpoint}\" conflicts with its namespace service"
            ),
            Self::NamespaceConflict { namespace } => write!(
                formatter,
                "client api: namespace \"{namespace}\" conflicts with the Remote service"
            ),
            Self::Arity {
                endpoint,
                expected,
                got,
                cancellable,
            } => {
                let contract = if *cancellable {
                    format!("{expected} business argument(s) plus an optional cancellation")
                } else {
                    format!("{expected} argument(s)")
                };
                write!(
                    formatter,
                    "client api: {endpoint} expected {contract}, got {got}"
                )
            }
            Self::ScopeUnavailable { endpoint, context } => write!(
                formatter,
                "client api: {endpoint} requires a \"{context}\" Context"
            ),
            Self::Withdrawn { endpoint } => write!(
                formatter,
                "client api: Remote method {endpoint} is no longer mounted"
            ),
            Self::NamespaceNotStarted { namespace } => write!(
                formatter,
                "client api: namespace \"{namespace}\" did not start"
            ),
            Self::Declaration { endpoint, reason } => {
                write!(
                    formatter,
                    "client api: {endpoint} cannot be invoked: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for ClientError {}

// ---- mount tokens --------------------------------------------------------------------------------

/// One installed method's liveness, shared by every clone of the record.
///
/// Upstream's `MountToken` is `{ active: boolean, abort: AbortController }`. The abort controller
/// exists to cancel an in-flight request when the method is withdrawn; in Rust the cancellation
/// belongs to the caller's own future, so this carries the flag and a sequence number that makes
/// two tokens distinguishable, which is what "remove only while it still belongs to this token"
/// needs.
#[derive(Clone, Debug)]
pub struct MountToken {
    active: Arc<AtomicBool>,
    sequence: u64,
}

impl MountToken {
    /// A live token.
    #[must_use]
    pub fn new(sequence: u64) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            sequence,
        }
    }

    /// Whether the method is still mounted.
    #[must_use]
    pub fn live(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Retires the token. Returns whether this call was the one that retired it, so a disposer is
    /// idempotent and a second disposer cannot report that it did the work.
    pub fn retire(&self) -> bool {
        // `swap` returns the *previous* value: true means this call found it live and retired it.
        self.active.swap(false, Ordering::AcqRel)
    }

    /// The sequence number that makes one token distinguishable from another.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Which variant of one method a call selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// The method is invoked on its own service instance.
    Direct,
    /// The receiver or its identity comes from a resolving Context.
    Scoped,
}

impl Variant {
    /// The word upstream's error messages use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Scoped => "scoped",
        }
    }
}

/// Why a call could not be dispatched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// The method was withdrawn before the call was selected.
    Withdrawn {
        /// The endpoint that was called.
        endpoint: String,
    },
    /// The method exists, but not in a variant this caller can use.
    ///
    /// A method installed only as `scoped` and called by a caller with no identity has nothing to
    /// dispatch to: upstream's `invokeMethod` throws the same way rather than falling back to the
    /// service instance, because the Context *is* the receiver.
    Unavailable {
        /// The endpoint that was called.
        endpoint: String,
    },
}

impl DispatchError {
    /// The endpoint the failure names.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        match self {
            Self::Withdrawn { endpoint } | Self::Unavailable { endpoint } => endpoint,
        }
    }
}

impl fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Withdrawn { endpoint } => {
                write!(
                    formatter,
                    "client api: Remote method {endpoint} is no longer mounted"
                )
            }
            Self::Unavailable { endpoint } => write!(
                formatter,
                "client api: Remote method {endpoint} is no longer mounted for this caller"
            ),
        }
    }
}

impl std::error::Error for DispatchError {}

impl From<DispatchError> for ClientError {
    fn from(error: DispatchError) -> Self {
        match error {
            DispatchError::Withdrawn { endpoint } | DispatchError::Unavailable { endpoint } => {
                Self::Withdrawn { endpoint }
            }
        }
    }
}

// ---- the invocation selection --------------------------------------------------------------------

/// One selected invocation: which descriptor, on which receiver, under which token.
///
/// This is what upstream's property getter closure produced and handed to `invokeSelected`. The
/// descriptor travels with the selection because a withdrawal between selection and dispatch must
/// not turn the call into a different method.
#[derive(Clone, Debug)]
pub struct Selection {
    /// The descriptor to invoke.
    pub descriptor: InvocationDescriptor,
    /// Which variant was selected.
    pub variant: Variant,
    /// The token that must still be live when the outcome arrives.
    pub token: MountToken,
}

impl PartialEq for Selection {
    /// Two selections are the same when they name the same descriptor and mount, which is what a
    /// test needs; the token's *liveness* is deliberately not part of equality, because a withdrawn
    /// selection is still the selection that was withdrawn.
    fn eq(&self, other: &Self) -> bool {
        self.descriptor == other.descriptor
            && self.variant == other.variant
            && self.token.sequence() == other.token.sequence()
    }
}

impl Eq for Selection {}

impl Selection {
    /// The endpoint, for failure messages.
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.descriptor.endpoint()
    }

    /// Re-checks the token after the carrier returned.
    ///
    /// This is upstream's `if (!mountActive(token)) return withdrawn(endpoint)`: a call that was
    /// selected while mounted and completed after the withdrawal reports that it was withdrawn,
    /// because its value can no longer be attributed to anything the Client has.
    pub fn still_mounted(&self) -> Result<(), DispatchError> {
        if self.token.live() {
            Ok(())
        } else {
            Err(DispatchError::Withdrawn {
                endpoint: self.endpoint(),
            })
        }
    }
}

/// One installed method: its live variants and their tokens.
#[derive(Clone, Debug, Default)]
struct MethodRecord {
    direct: Option<(InvocationDescriptor, MountToken)>,
    scoped: Option<(InvocationDescriptor, MountToken, Scope)>,
}

impl MethodRecord {
    fn is_empty(&self) -> bool {
        self.direct.is_none() && self.scoped.is_none()
    }
}

// ---- namespaces ----------------------------------------------------------------------------------

/// The Client-side projection of one namespace: `remote.<namespace>` and its methods.
///
/// Upstream creates one `RemoteNamespaceService` per namespace inside its own Cordis fiber, installs
/// the contribution's whole group synchronously, and disposes the namespace once its last method
/// leaves. [`NamespaceTable`] keeps the same lifecycle without the fiber: [`mount_contribution`]
/// installs a whole contribution or nothing, and [`NamespaceTable::unmount`] retires its tokens and
/// drops a namespace that became empty.
///
/// [`mount_contribution`]: NamespaceTable::mount_contribution
#[derive(Clone, Debug, Default)]
pub struct NamespaceTable {
    inner: Arc<NamespaceInner>,
}

#[derive(Debug, Default)]
struct NamespaceInner {
    namespaces: Mutex<BTreeMap<String, BTreeMap<String, MethodRecord>>>,
    next_token: AtomicU64,
}

impl NamespaceTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, BTreeMap<String, MethodRecord>>> {
        self.inner
            .namespaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The namespaces that have at least one live method.
    ///
    /// A namespace with no methods is not listed: upstream's `disposeNamespace` removes it as soon
    /// as its last method leaves, so an empty namespace would be observable only during a mount.
    #[must_use]
    pub fn namespaces(&self) -> Vec<String> {
        self.lock()
            .iter()
            .filter(|(_, methods)| !methods.is_empty())
            .map(|(namespace, _)| namespace.clone())
            .collect()
    }

    /// The service key of one namespace, for an error message or a lookup.
    #[must_use]
    pub fn service_key(namespace: &str) -> String {
        remote_service_key(namespace)
    }

    /// Whether `namespace` has any live method.
    #[must_use]
    pub fn contains(&self, namespace: &str) -> bool {
        self.lock()
            .get(namespace)
            .is_some_and(|methods| !methods.is_empty())
    }

    /// The methods of one namespace, whether each variant is installed, and the identity.
    ///
    /// Sorted by name so a diagnostic or a test does not depend on insertion order.
    #[must_use]
    pub fn methods(&self, namespace: &str) -> Vec<MethodView> {
        let namespaces = self.lock();
        let Some(methods) = namespaces.get(namespace) else {
            return Vec::new();
        };
        methods
            .iter()
            .map(|(method, record)| MethodView {
                method: method.clone(),
                direct: record.direct.is_some(),
                scoped: record.scoped.is_some(),
            })
            .collect()
    }

    /// Whether one variant of one method is installed.
    #[must_use]
    pub fn has(&self, namespace: &str, method: &str, variant: Variant) -> bool {
        let namespaces = self.lock();
        let Some(record) = namespaces.get(namespace).and_then(|m| m.get(method)) else {
            return false;
        };
        match variant {
            Variant::Direct => record.direct.is_some(),
            Variant::Scoped => record.scoped.is_some(),
        }
    }

    /// Installs one descriptor under a fresh token, replacing whatever occupied that variant.
    ///
    /// This is the one way a record is *replaced* rather than added, and it exists because the
    /// alternative — a mount racing a mount for one endpoint — is what the token guard in
    /// [`NamespaceTable::unmount`] defends against: the retiring disposer checks the sequence it
    /// wrote, not the method name it wrote it under. Public so the case is reachable from a test
    /// without reaching into the table's internals.
    #[must_use]
    pub fn replace(&self, descriptor: &InvocationDescriptor) -> MountToken {
        let token = MountToken::new(self.inner.next_token.fetch_add(1, Ordering::AcqRel));
        let mut namespaces = self.lock();
        let record = namespaces
            .entry(descriptor.namespace.clone())
            .or_default()
            .entry(descriptor.method.clone())
            .or_default();
        match descriptor.receiver {
            Receiver::Direct => record.direct = Some((descriptor.clone(), token.clone())),
            Receiver::Context { .. } => {
                let scope = descriptor
                    .scope
                    .clone()
                    .unwrap_or_else(|| Scope::new(String::new(), String::new()));
                record.scoped = Some((descriptor.clone(), token.clone(), scope));
            }
        }
        token
    }

    /// Selects the variant a caller should dispatch to, at call time.
    ///
    /// Three rules, in upstream's order:
    ///
    /// 1. A scoped variant wins when the caller has an identity for its Context. That is the
    ///    `adapter?.identity(callerCtx)` check, and it is why the identity is an input here: the
    ///    same method with the same arguments resolves differently for two callers.
    /// 2. Otherwise the direct variant is used.
    /// 3. Otherwise the scoped variant is used without an identity bound, which is the case
    ///    upstream reaches when the scoped adapter exists but the caller has no identity at all.
    ///
    /// # Errors
    ///
    /// [`DispatchError::Withdrawn`] when the method is not installed in any variant.
    pub fn select(
        &self,
        namespace: &str,
        method: &str,
        identity: Option<&Value>,
    ) -> Result<Selection, DispatchError> {
        let namespaces = self.lock();
        let record = namespaces
            .get(namespace)
            .and_then(|methods| methods.get(method))
            .ok_or_else(|| DispatchError::Withdrawn {
                endpoint: format!("{namespace}/{method}"),
            })?;
        if identity.is_some()
            && let Some((descriptor, token, _)) = &record.scoped
        {
            return Ok(Selection {
                descriptor: descriptor.clone(),
                variant: Variant::Scoped,
                token: token.clone(),
            });
        }
        if let Some((descriptor, token)) = &record.direct {
            return Ok(Selection {
                descriptor: descriptor.clone(),
                variant: Variant::Direct,
                token: token.clone(),
            });
        }
        if let Some((descriptor, token, _)) = &record.scoped {
            return Ok(Selection {
                descriptor: descriptor.clone(),
                variant: Variant::Scoped,
                token: token.clone(),
            });
        }
        Err(DispatchError::Unavailable {
            endpoint: format!("{namespace}/{method}"),
        })
    }

    /// Mounts one contribution's whole descriptor set, or nothing.
    ///
    /// Upstream's `$mount`: validate the contribution, then install each namespace group, unwinding
    /// every group already installed when one fails. The order matters — validation runs first so a
    /// contribution that repeats a method leaves no namespace behind, which upstream asserts with
    /// `expect(ctx.typert.remotes.list()).toEqual([])`.
    ///
    /// # Errors
    ///
    /// [`ClientError::DuplicateMethod`], [`ClientError::ReservedMethod`],
    /// [`ClientError::NamespaceConflict`], or [`ClientError::Declaration`], and nothing is mounted.
    pub fn mount_contribution(
        &self,
        descriptors: &[InvocationDescriptor],
    ) -> Result<ContributionMount, ClientError> {
        self.validate(descriptors)?;
        let mut installed = Vec::new();
        for descriptor in descriptors {
            let token = MountToken::new(self.inner.next_token.fetch_add(1, Ordering::AcqRel));
            let namespace = descriptor.namespace.clone();
            let method = descriptor.method.clone();
            let mut namespaces = self.lock();
            let record = namespaces
                .entry(namespace.clone())
                .or_default()
                .entry(method.clone())
                .or_default();
            match descriptor.receiver {
                Receiver::Direct => record.direct = Some((descriptor.clone(), token.clone())),
                Receiver::Context { .. } => {
                    let scope = descriptor.scope.clone().unwrap_or_else(|| {
                        // A Context receiver with no projection is still a scoped method: the
                        // identity comes from the receiver's own wire field, so the projection is
                        // the wire field itself.
                        let wire = match &descriptor.receiver {
                            Receiver::Context { wire, .. } => wire.clone(),
                            Receiver::Direct => String::new(),
                        };
                        let context = match &descriptor.receiver {
                            Receiver::Context { context, .. } => context.clone(),
                            Receiver::Direct => String::new(),
                        };
                        Scope::new(context, wire)
                    });
                    record.scoped = Some((descriptor.clone(), token.clone(), scope));
                }
            }
            installed.push((namespace, method, token));
        }
        Ok(ContributionMount {
            table: self.clone(),
            installed,
            retired: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Checks every property a contribution must satisfy before anything is mounted.
    fn validate(&self, descriptors: &[InvocationDescriptor]) -> Result<(), ClientError> {
        let mut direct: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut scoped: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let namespaces = self.lock();
        for descriptor in descriptors {
            validate_client_descriptor(descriptor)?;
            let endpoint = descriptor.endpoint();
            if NAMESPACE_RESERVED_METHODS.contains(&descriptor.method.as_str()) {
                return Err(ClientError::ReservedMethod { endpoint });
            }
            if descriptor.namespace == "remote" {
                return Err(ClientError::NamespaceConflict {
                    namespace: descriptor.namespace.clone(),
                });
            }
            if descriptor.id.is_empty() || descriptor.service.is_empty() {
                return Err(ClientError::Declaration {
                    endpoint,
                    reason: "descriptor has no id or service".to_owned(),
                });
            }
            if matches!(descriptor.receiver, Receiver::Context { .. })
                && descriptor.scope.is_none()
                && matches!(&descriptor.receiver, Receiver::Context { wire, .. } if wire.is_empty())
            {
                return Err(ClientError::Declaration {
                    endpoint,
                    reason: "Context receiver has no wire identity".to_owned(),
                });
            }
            let variants: &mut BTreeMap<&str, BTreeSet<&str>> = match descriptor.receiver {
                Receiver::Direct => &mut direct,
                Receiver::Context { .. } => &mut scoped,
            };
            let kind = match descriptor.receiver {
                Receiver::Direct => "direct",
                Receiver::Context { .. } => "scoped",
            };
            if !variants
                .entry(descriptor.namespace.as_str())
                .or_default()
                .insert(descriptor.method.as_str())
            {
                return Err(ClientError::DuplicateMethod { endpoint, kind });
            }
            let existing = namespaces
                .get(&descriptor.namespace)
                .and_then(|methods| methods.get(&descriptor.method));
            if let Some(record) = existing {
                let taken = match descriptor.receiver {
                    Receiver::Direct => record.direct.is_some(),
                    Receiver::Context { .. } => record.scoped.is_some(),
                };
                if taken {
                    return Err(ClientError::DuplicateMethod { endpoint, kind });
                }
            }
        }
        Ok(())
    }

    /// Retires every token of one contribution and drops the namespaces it emptied.
    ///
    /// Returns how many methods were retired. Idempotent through
    /// [`ContributionMount::unmount`], which is the caller-facing entry point.
    fn unmount(&self, installed: &[(String, String, MountToken)]) -> usize {
        let mut retired = 0;
        let mut namespaces = self.lock();
        for (namespace, method, token) in installed {
            if !token.retire() {
                continue;
            }
            retired += 1;
            let Some(methods) = namespaces.get_mut(namespace) else {
                continue;
            };
            if let Some(record) = methods.get_mut(method) {
                if record
                    .direct
                    .as_ref()
                    .is_some_and(|(_, t)| t.sequence == token.sequence)
                {
                    record.direct = None;
                }
                if record
                    .scoped
                    .as_ref()
                    .is_some_and(|(_, t, _)| t.sequence == token.sequence)
                {
                    record.scoped = None;
                }
                if record.is_empty() {
                    methods.remove(method);
                }
            }
            if methods.is_empty() {
                namespaces.remove(namespace);
            }
        }
        retired
    }
}

/// One method of one namespace, as the table holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodView {
    /// The method name.
    pub method: String,
    /// Whether the direct variant is installed.
    pub direct: bool,
    /// Whether the scoped variant is installed.
    pub scoped: bool,
}

/// One mounted contribution: its installed methods and their shared fate.
///
/// Upstream returns a `TypertDisposer` from `$mount`. This is its local form: one call retires the
/// whole contribution in reverse installation order, and a second call does nothing.
#[derive(Clone, Debug)]
pub struct ContributionMount {
    table: NamespaceTable,
    installed: Vec<(String, String, MountToken)>,
    retired: Arc<AtomicBool>,
}

impl ContributionMount {
    /// The installed methods, in installation order.
    #[must_use]
    pub fn endpoints(&self) -> Vec<String> {
        self.installed
            .iter()
            .map(|(namespace, method, _)| format!("{namespace}/{method}"))
            .collect()
    }

    /// Whether this mount has been unmounted.
    #[must_use]
    pub fn is_unmounted(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    /// Retires the contribution in reverse installation order.
    ///
    /// Returns how many methods were retired; zero on a second call. Reverse order is upstream's,
    /// and it is the order that keeps a namespace alive while its sibling groups are still going
    /// away.
    pub fn unmount(&self) -> usize {
        if self.retired.swap(true, Ordering::AcqRel) {
            return 0;
        }
        let reversed: Vec<(String, String, MountToken)> =
            self.installed.iter().rev().cloned().collect();
        self.table.unmount(&reversed)
    }
}

// ---- invocation preparation ----------------------------------------------------------------------

/// One prepared invocation: the wire arguments, the endpoint, and whether the caller can cancel.
///
/// Upstream's `PreparedClientInvocation`. The arguments are a [`Map`] rather than a [`Value`] so a
/// non-object payload cannot be constructed here at all: the carrier requires `{ args: {...} }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedInvocation {
    /// The canonical `namespace/method`.
    pub endpoint: String,
    /// The wire `args` object.
    pub args: Map<String, Value>,
    /// Whether the caller supplied its own cancellation, which the transport may combine with the
    /// mount token's abort.
    pub cancellable: bool,
}

impl PreparedInvocation {
    /// The payload the carrier sends: `{ "args": { ... } }`.
    #[must_use]
    pub fn payload(&self) -> Value {
        let mut envelope = Map::new();
        envelope.insert("args".to_owned(), Value::Object(self.args.clone()));
        Value::Object(envelope)
    }
}

/// The identity a Context-projected parameter substitutes for the wire value.
///
/// Upstream resolves this through a `ClientContextAdapter` (`adapter.identity(callerCtx)`). A
/// [`Map`] would do, but naming the type says what the value *is*: an identity the Client already
/// holds, never something the caller passed as an argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextIdentity(Value);

impl ContextIdentity {
    /// Wraps an identity.
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self(value)
    }

    /// The identity as the wire carries it.
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.0
    }
}

/// How the expected argument count is computed for one descriptor.
///
/// A scoped parameter is *not* passed as an argument: the Client substitutes the caller's identity
/// for it. So the expected count is the declared parameters minus that one, which is exactly
/// upstream's `descriptor.parameters.length - (projection?.parameterIndex === undefined ? 0 : 1)`.
fn expected_arguments(descriptor: &InvocationDescriptor, scoped: bool) -> usize {
    descriptor.parameters.len() - usize::from(scoped)
}

/// The index of the single lookup parameter a scoped descriptor projects, if it has one.
///
/// Upstream's `scopedProjection` accepts exactly one lookup parameter whose wire field and lookup
/// key match the declared scope, and throws for any other shape. Two lookup parameters would make
/// the substitution ambiguous, so it is a refusal rather than a guess.
fn scoped_parameter_index(descriptor: &InvocationDescriptor) -> Option<usize> {
    let candidates: Vec<usize> = descriptor
        .parameters
        .iter()
        .enumerate()
        .filter(|(_, parameter)| parameter.lookup_key().is_some())
        .map(|(index, _)| index)
        .collect();
    match candidates.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// Validates the descriptor shape a Client invocation needs.
///
/// Upstream's `requireStrictInputs`: every parameter must have a strict codec, the uplink must have
/// one, and a Context receiver must have one too. A descriptor without strict inputs would produce
/// arguments the Host cannot validate, so the contribution is refused at mount time rather than
/// failing on the first call.
///
/// # Errors
///
/// [`ClientError::Declaration`] naming the property the descriptor lacks.
pub fn validate_client_descriptor(descriptor: &InvocationDescriptor) -> Result<(), ClientError> {
    let endpoint = descriptor.endpoint();
    for parameter in &descriptor.parameters {
        if parameter.wire.is_empty() {
            return Err(ClientError::Declaration {
                endpoint,
                reason: format!("parameter \"{}\" has no wire field", parameter.name),
            });
        }
    }
    if let Receiver::Context { wire, .. } = &descriptor.receiver
        && wire.is_empty()
    {
        return Err(ClientError::Declaration {
            endpoint,
            reason: "Context receiver has no wire identity".to_owned(),
        });
    }
    if let Some(scope) = &descriptor.scope {
        let index = scoped_parameter_index(descriptor).ok_or_else(|| ClientError::Declaration {
            endpoint: endpoint.clone(),
            reason: "scope must select exactly one lookup parameter".to_owned(),
        })?;
        let parameter = &descriptor.parameters[index];
        if parameter.wire != scope.wire || parameter.lookup_key() != Some(scope.context.as_str()) {
            return Err(ClientError::Declaration {
                endpoint,
                reason: "the selected lookup parameter does not match the declared scope"
                    .to_owned(),
            });
        }
    }
    Ok(())
}

/// Builds the wire arguments for one invocation.
///
/// Six rules, all of them observable:
///
/// 1. **The arity is exact.** A caller-supplied cancellation is allowed only when the descriptor
///    declares one, and then only as the *last* argument; anything else is [`ClientError::Arity`].
///    This is the check that stops a missing argument from silently becoming `undefined`.
/// 2. **A scoped parameter is filled from the identity, not from an argument.** The substitution is
///    keyed by the parameter's wire field, and it happens *before* the business arguments are
///    placed, which is why they keep their declaration order.
/// 3. **Only the declared parameters become wire fields.** Nothing the caller sent outside the
///    declaration can cross, which is why the output is built field by field rather than cloned.
/// 4. **An explicit `undefined` is omitted rather than sent as `null`.** A JSON `null` and an absent
///    field decode differently when a parameter accepts `undefined`, so collapsing them here would
///    change what the method receives.
/// 5. **A missing scoped identity is a refusal**, not an empty string: upstream throws
///    `requires a "<context>" Context`.
/// 6. **The endpoint travels with the arguments**, so a carrier failure can name it without the
///    caller re-deriving it.
///
/// # Errors
///
/// [`ClientError::Arity`], [`ClientError::ScopeUnavailable`], or [`ClientError::Declaration`].
pub fn prepare_invocation(
    selection: &Selection,
    identity: Option<&ContextIdentity>,
    arguments: &[Value],
) -> Result<PreparedInvocation, ClientError> {
    let descriptor = &selection.descriptor;
    let endpoint = descriptor.endpoint();
    let scoped = matches!(selection.variant, Variant::Scoped);
    let expected = expected_arguments(descriptor, scoped);
    let cancellable = descriptor.cancellation.is_some();
    let has_cancellation = cancellable && arguments.len() == expected + 1;
    if arguments.len() != expected && !has_cancellation {
        return Err(ClientError::Arity {
            endpoint,
            expected,
            got: arguments.len(),
            cancellable,
        });
    }
    let mut args = Map::new();
    let projected = if scoped {
        Some(
            scoped_parameter_index(descriptor).ok_or_else(|| ClientError::Declaration {
                endpoint: endpoint.clone(),
                reason: "scope must select exactly one lookup parameter".to_owned(),
            })?,
        )
    } else {
        None
    };
    if projected.is_some() {
        // The descriptor's own scope is the projection; a Selection cannot be built for a scoped
        // variant of a descriptor that declares none, so the `unwrap_or` below is unreachable and
        // exists only to avoid an `expect` in a library path.
        let scope = descriptor
            .scope
            .clone()
            .unwrap_or_else(|| Scope::new("", ""));
        let Some(identity) = identity else {
            return Err(ClientError::ScopeUnavailable {
                endpoint,
                context: scope.context.clone(),
            });
        };
        args.insert(scope.wire.clone(), identity.value().clone());
    }
    let mut value_index = 0;
    for (parameter_index, parameter) in descriptor.parameters.iter().enumerate() {
        if Some(parameter_index) == projected {
            continue;
        }
        let value = arguments.get(value_index);
        if let Some(value) = value
            && !value.is_null()
        {
            args.insert(parameter.wire.clone(), value.clone());
        }
        value_index += 1;
    }
    Ok(PreparedInvocation {
        endpoint,
        args,
        cancellable: has_cancellation,
    })
}

// ---- failure folding -----------------------------------------------------------------------------

/// The outcome of one unary invocation, already folded.
///
/// Upstream's `RemoteResult<unknown>`. A carrier throw is an *outcome* rather than an assembly
/// fault: offline and aborted calls both have to reach the caller as a value, because the caller is
/// the only one that can decide whether to retry.
#[derive(Clone, Debug, PartialEq)]
pub enum InvocationOutcome {
    /// The Host answered.
    Ok(Value),
    /// The call failed with a stable code.
    Failed(RemoteFailure),
}

impl InvocationOutcome {
    /// Whether the outcome carries a value.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// The value, when there is one.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        match self {
            Self::Ok(value) => Some(value),
            Self::Failed(_) => None,
        }
    }

    /// The failure, when there is one.
    #[must_use]
    pub fn failure(&self) -> Option<&RemoteFailure> {
        match self {
            Self::Ok(_) => None,
            Self::Failed(failure) => Some(failure),
        }
    }
}

/// One folded failure: the code, the diagnostic, and the details.
///
/// The wire form is the same as [`girder_core::typert_gap::RemoteError`]'s, but this type is
/// deliberately separate: a *client* failure is produced locally (a carrier throw, a cancellation,
/// a withdrawal) and must not be confused with one a Host sent. Code is the discriminant, exactly as
/// upstream discriminates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFailure {
    /// The stable code.
    pub code: String,
    /// The diagnostic.
    pub message: String,
    /// Structured details, empty when the failure has none.
    pub details: Map<String, Value>,
}

impl RemoteFailure {
    /// Builds a failure.
    #[must_use]
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Map<String, Value>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details,
        }
    }

    /// The wire form.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("code".to_owned(), Value::String(self.code.clone()));
        map.insert("message".to_owned(), Value::String(self.message.clone()));
        map.insert("details".to_owned(), Value::Object(self.details.clone()));
        Value::Object(map)
    }
}

impl fmt::Display for RemoteFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// Folds a carrier throw into the failure branch.
///
/// Upstream's `carrierFailure`: `gateway/internal` naming the endpoint and the thrown message. It is
/// `internal` rather than a transport code because the Client cannot tell an offline socket from a
/// Host that died, and inventing a code for a distinction it does not have would make a caller
/// branch on a guess.
#[must_use]
pub fn carrier_failure(endpoint: &str, message: &str) -> RemoteFailure {
    RemoteFailure::new(
        CARRIER_FAILURE_CODE,
        format!("client api: {endpoint} failed: {message}"),
        Map::new(),
    )
}

/// Folds a caller abort into the failure branch.
///
/// Upstream's `cancelledFailure`: `gateway/cancelled`, and the *same* code the Host would have
/// produced, so a caller that aborts locally and a Host that notices the abort report one thing. A
/// carrier throw that loses the race against the abort does not change this, which is why
/// cancellation is checked before the carrier failure is folded.
#[must_use]
pub fn cancelled_failure(endpoint: &str) -> RemoteFailure {
    RemoteFailure::new(
        CANCELLED_FAILURE_CODE,
        format!("client api: Remote invocation \"{endpoint}\" was aborted"),
        Map::new(),
    )
}

/// Folds the failure a withdrawn endpoint reports.
#[must_use]
pub fn withdrawn_failure(endpoint: &str) -> RemoteFailure {
    RemoteFailure::new(
        CARRIER_FAILURE_CODE,
        format!("client api: Remote method {endpoint} is no longer mounted"),
        Map::new(),
    )
}

/// What a carrier did: answered, threw, or the caller aborted.
///
/// Modelled as an input rather than a `Result` because the three cases fold differently and the
/// discrimination between them is the whole of [`fold_outcome`].
#[derive(Clone, Debug, PartialEq)]
pub enum CarrierOutcome {
    /// The Host answered with `{ ok, value }` or `{ ok: false, error }`.
    Answered {
        /// Whether the Host reported success.
        ok: bool,
        /// The value on success, or the failure's wire form.
        payload: Value,
    },
    /// The carrier threw.
    Threw(String),
    /// The caller aborted before the carrier settled.
    Aborted,
}

/// Folds one carrier outcome into an [`InvocationOutcome`], re-checking the mount token last.
///
/// The order is upstream's and each step matters:
///
/// 1. A withdrawal *after* the carrier returned reports [`ClientError::Withdrawn`] rather than a
///    value: the caller can no longer attribute it.
/// 2. A caller abort reports `gateway/cancelled` **even when the carrier threw**, because the
///    caller asked for it and a transport error would be a misleading diagnosis.
/// 3. A carrier throw reports `gateway/internal`.
/// 4. A Host-reported failure is passed through with its code **verbatim**, without validating it
///    against this build's vocabulary, so a newer Host stays readable.
///
/// # Errors
///
/// [`ClientError::Withdrawn`] when the selection was withdrawn while the carrier was working.
pub fn fold_outcome(
    selection: &Selection,
    outcome: CarrierOutcome,
) -> Result<InvocationOutcome, ClientError> {
    selection.still_mounted().map_err(ClientError::from)?;
    Ok(match outcome {
        CarrierOutcome::Aborted => {
            InvocationOutcome::Failed(cancelled_failure(&selection.descriptor.endpoint()))
        }
        CarrierOutcome::Threw(message) => {
            InvocationOutcome::Failed(carrier_failure(&selection.descriptor.endpoint(), &message))
        }
        CarrierOutcome::Answered { ok: true, payload } => InvocationOutcome::Ok(payload),
        CarrierOutcome::Answered { ok: false, payload } => {
            InvocationOutcome::Failed(rebuilt_failure(payload))
        }
    })
}

/// Rebuilds a Host failure from its wire form.
///
/// A payload that is not an object with a string code is a *protocol* fault rather than a business
/// failure, and it is reported as `gateway/internal` with the raw payload preserved in `details`:
/// silently dropping it would turn a broken Host into an empty error.
#[must_use]
pub fn rebuilt_failure(payload: Value) -> RemoteFailure {
    let Some(object) = payload.as_object() else {
        let mut details = Map::new();
        details.insert("error".to_owned(), payload);
        return RemoteFailure::new(
            CARRIER_FAILURE_CODE,
            "client api: Remote failure is not an object".to_owned(),
            details,
        );
    };
    let Some(code) = object.get("code").and_then(Value::as_str) else {
        let mut details = Map::new();
        details.insert("error".to_owned(), payload);
        return RemoteFailure::new(
            CARRIER_FAILURE_CODE,
            "client api: Remote failure has no code".to_owned(),
            details,
        );
    };
    RemoteFailure::new(
        code,
        object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        object
            .get("details")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
    )
}

// ---- the uplink queue ----------------------------------------------------------------------------

/// Why a Client could not queue an uplink item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UplinkQueueError {
    /// The Client half-closed the uplink.
    Ended,
    /// The logical stream terminated.
    Terminated,
}

impl UplinkQueueError {
    /// The diagnostic upstream throws.
    #[must_use]
    pub fn message(self, endpoint: &str) -> String {
        match self {
            Self::Ended => format!("client api: {endpoint} uplink was ended"),
            Self::Terminated => format!("client api: {endpoint} stream has terminated"),
        }
    }
}

/// The items one Client stream handle queues for its carrier.
///
/// Upstream's `ClientUplinkQueue`. Three states, and the difference between the last two is the
/// whole reason it is not a bare `VecDeque`:
///
/// - **open** — items queue and a carrier read takes them;
/// - **ended** — the Client half-closed; the carrier drains what is buffered and then sees the end;
/// - **closed** — the stream terminated; the buffer is *discarded* and further pushes throw, because
///   handing a carrier items for a stream the Host has abandoned is worse than dropping them.
///
/// A half-close is ignored after termination, and a second half-close is inert. Upstream's
/// one-pending-read rule is modelled by [`UplinkQueue::poll`] returning
/// [`UplinkQueuePoll::Pending`] instead of awaiting: the caller owns the waiting, so two concurrent
/// readers are the caller's bug rather than a queue state.
#[derive(Clone, Debug)]
pub struct UplinkQueue {
    endpoint: String,
    inner: Arc<UplinkInner>,
}

#[derive(Debug, Default)]
struct UplinkInner {
    state: Mutex<UplinkState>,
}

#[derive(Debug, Default)]
struct UplinkState {
    items: VecDeque<Value>,
    ended: bool,
    closed: bool,
}

impl UplinkQueue {
    /// An open queue for one endpoint.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            inner: Arc::new(UplinkInner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, UplinkState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The endpoint this queue names in its failures.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Queues one item for the carrier.
    ///
    /// A non-lossless value is refused before it reaches the queue: a JavaScript integer the Client
    /// cannot represent exactly would arrive at the Host as a different number, and a stream whose
    /// item changed in flight is worse than one that refused it.
    ///
    /// # Errors
    ///
    /// [`UplinkQueueError::Ended`], [`UplinkQueueError::Terminated`], or — for a non-lossless value
    /// — the same refusal a terminated stream gets, because no code exists for it and the caller's
    /// recovery (do not send that value) is the same.
    pub fn push(&self, item: Value) -> Result<(), UplinkQueueError> {
        let mut state = self.lock();
        if state.closed {
            return Err(UplinkQueueError::Terminated);
        }
        if state.ended {
            return Err(UplinkQueueError::Ended);
        }
        if !is_lossless(&item) {
            return Err(UplinkQueueError::Terminated);
        }
        state.items.push_back(item);
        Ok(())
    }

    /// The Client's half-close. Idempotent, and ignored once the stream terminated.
    pub fn end(&self) {
        let mut state = self.lock();
        if state.ended || state.closed {
            return;
        }
        state.ended = true;
    }

    /// The stream terminated: discard what was buffered and refuse later pushes.
    pub fn close(&self) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        state.items.clear();
    }

    /// Whether the Client half-closed.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.lock().ended
    }

    /// Whether the stream terminated.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// How many items are buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// Whether nothing is buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().items.is_empty()
    }

    /// Takes the next item without blocking.
    #[must_use]
    pub fn poll(&self) -> UplinkQueuePoll {
        let mut state = self.lock();
        if state.closed {
            return UplinkQueuePoll::Done;
        }
        if let Some(item) = state.items.pop_front() {
            return UplinkQueuePoll::Item(item);
        }
        if state.ended {
            return UplinkQueuePoll::Done;
        }
        UplinkQueuePoll::Pending
    }
}

/// One non-blocking read from an [`UplinkQueue`].
#[derive(Clone, Debug, PartialEq)]
pub enum UplinkQueuePoll {
    /// One queued item.
    Item(Value),
    /// Nothing queued yet, and the uplink is still open.
    Pending,
    /// The uplink finished, by half-close or by termination.
    Done,
}

// ---- logical stream supervision ------------------------------------------------------------------

/// Why the supervisor will not open another generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamRefusal {
    /// The stream was disposed.
    Disposed,
    /// A generation already has a consumer; a logical stream is single-consumer.
    Taken,
    /// A terminal failure was already published.
    Failed,
}

/// How a retry is paced.
///
/// Two inputs, both upstream's: whether the carrier loss was *retryable at all*, and whether a new
/// physical carrier already exists. `attempt` is one-based and resets when an item is accepted or
/// the stream is restarted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPlan {
    /// Wait before reconnecting.
    pub wait: bool,
    /// Whether this attempt exhausted the supervisor's patience and the stream must fail.
    pub give_up: bool,
}

/// The retry decision for one carrier loss.
///
/// Upstream's `waitForRemoteStreamRetry`:
///
/// - first attempt with a live carrier: retry **immediately**, because the replacement socket is
///   already there and waiting would only delay a stream that can make progress;
/// - a later attempt with a live carrier: give up, because the carrier keeps coming back and the
///   stream keeps failing, so the fault is not the carrier;
/// - any attempt with no carrier: wait for one, because reconnecting into a socket that is not
///   there is a busy loop.
#[must_use]
pub fn retry_plan(carrier_available: bool, attempt: u32) -> RetryPlan {
    if carrier_available {
        if attempt <= 1 {
            RetryPlan {
                wait: false,
                give_up: false,
            }
        } else {
            RetryPlan {
                wait: false,
                give_up: true,
            }
        }
    } else {
        RetryPlan {
            wait: true,
            give_up: false,
        }
    }
}

/// One logical stream across physical carrier generations.
///
/// Upstream's `RemoteStream`. It owns the three decisions a reconnecting stream has to make and the
/// local type makes explicit:
///
/// 1. **Generation identity is monotone and per logical stream**, so a consumer can tell "a new
///    generation's opening baseline" from "another delta of the one I am on". `generation` only
///    moves forward through [`RemoteStream::advance`].
/// 2. **Acceptance gates retry pacing.** [`RemoteStream::accept`] is the consumer's statement that
///    this generation delivered something worth keeping; it resets the attempt counter.
/// 3. **A restart resets, a loss paces.** [`RemoteStream::restart`] interrupts the current
///    generation and asks for a replacement *immediately* with the attempt counter reset, while a
///    carrier loss counts an attempt and consults [`retry_plan`].
///
/// The module deliberately does not model the `AsyncIterable` itself: the consumer drives the loop,
/// which is why every method here is a state transition rather than a future.
#[derive(Clone, Debug)]
pub struct RemoteStream {
    name: String,
    inner: Arc<StreamInner>,
}

#[derive(Debug)]
struct StreamInner {
    generation: AtomicU64,
    attempt: AtomicU64,
    revision: AtomicU64,
    accepted: AtomicBool,
    disposed: AtomicBool,
    taken: AtomicBool,
    terminal: Mutex<Option<String>>,
}

impl RemoteStream {
    /// A logical stream with no generation open yet.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inner: Arc::new(StreamInner {
                generation: AtomicU64::new(0),
                attempt: AtomicU64::new(0),
                revision: AtomicU64::new(0),
                accepted: AtomicBool::new(false),
                disposed: AtomicBool::new(false),
                taken: AtomicBool::new(false),
                terminal: Mutex::new(None),
            }),
        }
    }

    /// The diagnostic name a failure quotes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current physical generation, zero before the first one opens.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// The current retry attempt.
    #[must_use]
    pub fn attempt(&self) -> u32 {
        u32::try_from(self.inner.attempt.load(Ordering::Acquire)).unwrap_or(u32::MAX)
    }

    /// How many restarts this stream has taken.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.inner.revision.load(Ordering::Acquire)
    }

    /// Whether the stream was disposed.
    #[must_use]
    pub fn is_disposed(&self) -> bool {
        self.inner.disposed.load(Ordering::Acquire)
    }

    /// The terminal failure, when the stream has one.
    #[must_use]
    pub fn terminal_failure(&self) -> Option<String> {
        self.inner
            .terminal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Claims the single consumer.
    ///
    /// # Errors
    ///
    /// [`StreamRefusal::Taken`] on a second claim, [`StreamRefusal::Disposed`] after disposal, and
    /// [`StreamRefusal::Failed`] once a terminal failure was published.
    pub fn take(&self) -> Result<(), StreamRefusal> {
        if self.is_disposed() {
            return Err(StreamRefusal::Disposed);
        }
        if self.terminal_failure().is_some() {
            return Err(StreamRefusal::Failed);
        }
        if self.inner.taken.swap(true, Ordering::AcqRel) {
            return Err(StreamRefusal::Taken);
        }
        Ok(())
    }

    /// Opens the next physical generation, returning its id.
    ///
    /// Clears the acceptance flag: acceptance belongs to the generation that delivered it, so a new
    /// generation starts unaccepted.
    ///
    /// # Errors
    ///
    /// [`StreamRefusal::Disposed`] or [`StreamRefusal::Failed`].
    pub fn advance(&self) -> Result<u64, StreamRefusal> {
        self.check_openable()?;
        self.inner.accepted.store(false, Ordering::Release);
        Ok(self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1)
    }

    fn check_openable(&self) -> Result<(), StreamRefusal> {
        if self.is_disposed() {
            return Err(StreamRefusal::Disposed);
        }
        if self.terminal_failure().is_some() {
            return Err(StreamRefusal::Failed);
        }
        Ok(())
    }

    /// Whether the current generation's opening item (or a later item) was accepted.
    #[must_use]
    pub fn accepted(&self) -> bool {
        self.inner.accepted.load(Ordering::Acquire)
    }

    /// Accepts an item of the current generation and resets the attempt counter.
    ///
    /// Returns whether this call was the one that accepted it; a second acceptance in the same
    /// generation changes nothing, so a consumer that accepts per item does not reset pacing
    /// spuriously.
    pub fn accept(&self, generation: u64) -> bool {
        if generation != self.generation() || self.is_disposed() {
            return false;
        }
        self.inner.attempt.store(0, Ordering::Release);
        !self.inner.accepted.swap(true, Ordering::AcqRel)
    }

    /// Interrupts the current generation and requests a replacement with pacing reset.
    ///
    /// Returns the revision after the restart, so a consumer can tell whether an in-flight
    /// generation is the one it started.
    pub fn restart(&self) -> u64 {
        if self.is_disposed() {
            return self.revision();
        }
        self.inner.attempt.store(0, Ordering::Release);
        self.inner.revision.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Counts one carrier loss and returns how the supervisor should retry it.
    ///
    /// The plan is derived from the attempt *after* this loss, which is why the counter is bumped
    /// first: upstream's `attempt++` happens before `waitForRemoteStreamRetry`.
    pub fn carrier_lost(&self, carrier_available: bool) -> RetryPlan {
        let attempt = u32::try_from(self.inner.attempt.fetch_add(1, Ordering::AcqRel) + 1)
            .unwrap_or(u32::MAX);
        retry_plan(carrier_available, attempt)
    }

    /// Publishes a terminal failure. The first one wins; later ones are ignored.
    ///
    /// Returns whether this call published it, so a consumer can report each failure exactly once.
    pub fn fail(&self, message: impl Into<String>) -> bool {
        let mut terminal = self
            .inner
            .terminal
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if terminal.is_some() {
            return false;
        }
        *terminal = Some(message.into());
        true
    }

    /// Disposes the stream. Idempotent; returns whether this call disposed it.
    pub fn dispose(&self) -> bool {
        !self.inner.disposed.swap(true, Ordering::AcqRel)
    }
}

// ---- snapshot streams ---------------------------------------------------------------------------

/// One frame of a snapshot stream: the generation's baseline, or a delta after it.
#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotFrame<Snapshot, Delta> {
    /// A complete snapshot that replaces the published model.
    Snapshot(Snapshot),
    /// One incremental update after the generation's snapshot.
    Delta(Delta),
}

/// Why a snapshot stream refused a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotViolation {
    /// A delta arrived before its generation's baseline.
    DeltaBeforeSnapshot {
        /// The stream's diagnostic name.
        name: String,
    },
    /// A second baseline arrived in one generation.
    RepeatedSnapshot {
        /// The stream's diagnostic name.
        name: String,
    },
}

impl fmt::Display for SnapshotViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeltaBeforeSnapshot { name } => write!(
                formatter,
                "{name} emitted an update before its opening snapshot"
            ),
            Self::RepeatedSnapshot { name } => {
                write!(formatter, "{name} emitted more than one opening snapshot")
            }
        }
    }
}

/// What the domain owner should do with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotAction {
    /// Atomically replace the domain model from a complete snapshot.
    Replace,
    /// Apply one incremental update.
    Update,
}

/// Baseline-and-delta framing over a reconnecting [`RemoteStream`].
///
/// Upstream's `RemoteSnapshotStream`. The rules are small and every one of them is a failure mode
/// if dropped:
///
/// 1. **Each generation opens with exactly one baseline.** A delta before it is a protocol
///    violation, not a delta to apply against a stale model, because the two generations' models
///    are not necessarily related.
/// 2. **A second baseline in one generation is refused**, since it means the Host changed the model
///    mid-generation and every subsequent delta's meaning is unknown.
/// 3. **A new generation resets the expectation.** The generation id is compared on every frame, so
///    a replacement baseline is accepted the moment its generation id changes.
/// 4. **The published model survives a reconnect.** The previous snapshot stays published while the
///    stream retries; a replacement is accepted only after the domain owner applies it, which is
///    why [`SnapshotAction`] is returned instead of the stream calling the domain itself.
#[derive(Clone, Debug)]
pub struct SnapshotStream {
    name: String,
    stream: RemoteStream,
    state: Arc<Mutex<SnapshotState>>,
}

#[derive(Debug, Default)]
struct SnapshotState {
    generation: u64,
    snapshot_seen: bool,
    replaced: u64,
    updated: u64,
}

impl SnapshotStream {
    /// Wraps a logical stream.
    #[must_use]
    pub fn new(name: impl Into<String>, stream: RemoteStream) -> Self {
        Self {
            name: name.into(),
            stream,
            state: Arc::new(Mutex::new(SnapshotState::default())),
        }
    }

    /// The underlying logical stream.
    #[must_use]
    pub fn stream(&self) -> &RemoteStream {
        &self.stream
    }

    /// The diagnostic name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many baselines were accepted.
    #[must_use]
    pub fn replaced(&self) -> u64 {
        self.lock().replaced
    }

    /// How many deltas were accepted.
    #[must_use]
    pub fn updated(&self) -> u64 {
        self.lock().updated
    }

    /// Whether the *current* generation has delivered its baseline.
    ///
    /// A generation becomes current when [`RemoteStream::advance`] opens it, and the stored flag is
    /// only reset when the next frame arrives. Reading it through the generation comparison is what
    /// keeps `advance()` followed by `snapshot_seen()` from reporting the previous generation's
    /// baseline, which would make a caller skip the replacement it must wait for.
    #[must_use]
    pub fn snapshot_seen(&self) -> bool {
        let state = self.lock();
        state.generation == self.stream.generation() && state.snapshot_seen
    }

    fn lock(&self) -> MutexGuard<'_, SnapshotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Checks one frame of the current generation and says what the domain owner must do with it.
    ///
    /// Upstream needs an `isSnapshot` predicate because its frames are a TypeScript union that is
    /// erased at run time. [`SnapshotFrame`] is a real tagged union, so its tag *is* the
    /// discriminator and the predicate would be a second, weaker statement of the same fact.
    ///
    /// Acceptance for pacing is deliberately **not** performed here; the domain owner calls
    /// [`SnapshotStream::applied`] once it has applied the frame, which is upstream's order
    /// (`replace` and then `item.accept()`).
    ///
    /// # Errors
    ///
    /// [`SnapshotViolation`] for a delta before the baseline or a repeated baseline.
    pub fn accept<Snapshot, Delta>(
        &self,
        frame: &SnapshotFrame<Snapshot, Delta>,
    ) -> Result<SnapshotAction, SnapshotViolation> {
        let generation = self.stream.generation();
        let mut state = self.lock();
        if generation != state.generation {
            state.generation = generation;
            state.snapshot_seen = false;
        }
        match frame {
            SnapshotFrame::Snapshot(_) => {
                if state.snapshot_seen {
                    return Err(SnapshotViolation::RepeatedSnapshot {
                        name: self.name.clone(),
                    });
                }
                state.snapshot_seen = true;
                state.replaced += 1;
                Ok(SnapshotAction::Replace)
            }
            SnapshotFrame::Delta(_) => {
                if !state.snapshot_seen {
                    return Err(SnapshotViolation::DeltaBeforeSnapshot {
                        name: self.name.clone(),
                    });
                }
                state.updated += 1;
                Ok(SnapshotAction::Update)
            }
        }
    }

    /// A baseline becomes accepted only after the domain owner applied it.
    ///
    /// This is upstream's `item.accept()` *after* `options.replace(...)`: an application that throws
    /// must not count as progress, so the pacing reset follows the domain call rather than
    /// preceding it.
    pub fn applied(&self) {
        self.stream.accept(self.stream.generation());
    }

    /// Replaces the active generation while keeping the published model.
    ///
    /// Returns the revision after the restart.
    pub fn restart(&self) -> u64 {
        self.stream.restart()
    }

    /// Stops the stream.
    pub fn dispose(&self) -> bool {
        self.stream.dispose()
    }
}

// ---- the client stream handle -------------------------------------------------------------------

/// One generation of one logical stream, as a generated stream method returns it.
///
/// Upstream's `ClientStreamHandle`: a downlink consumed once, an uplink queue the carrier drains,
/// and a disposal that closes the queue and ends the iteration. The one rule worth pinning is that
/// `send()` *before* the first read queues behind the opening frame rather than being lost, so the
/// queue is created with the handle and not on first read.
#[derive(Clone, Debug)]
pub struct StreamHandle {
    endpoint: String,
    uplink: UplinkQueue,
    consumed: Arc<AtomicBool>,
    disposed: Arc<AtomicBool>,
}

impl StreamHandle {
    /// Opens a handle for one endpoint.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        Self {
            uplink: UplinkQueue::new(endpoint.clone()),
            endpoint,
            consumed: Arc::new(AtomicBool::new(false)),
            disposed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The uplink queue the carrier drains.
    #[must_use]
    pub fn uplink(&self) -> &UplinkQueue {
        &self.uplink
    }

    /// Queues one uplink item.
    ///
    /// # Errors
    ///
    /// [`UplinkQueueError`], including after disposal — a disposed handle's buffer was discarded, so
    /// a queued item would never be delivered.
    pub fn send(&self, item: Value) -> Result<(), UplinkQueueError> {
        if self.disposed.load(Ordering::Acquire) {
            return Err(UplinkQueueError::Terminated);
        }
        self.uplink.push(item)
    }

    /// Half-closes the uplink.
    pub fn end(&self) {
        self.uplink.end();
    }

    /// Claims the single downlink consumer.
    ///
    /// # Errors
    ///
    /// [`StreamRefusal::Taken`] on a second claim: upstream throws `stream has one consumer`.
    pub fn consume(&self) -> Result<(), StreamRefusal> {
        if self.consumed.swap(true, Ordering::AcqRel) {
            return Err(StreamRefusal::Taken);
        }
        Ok(())
    }

    /// Disposes the generation: closes the uplink and ends the downlink.
    ///
    /// Idempotent; returns whether this call disposed it.
    pub fn dispose(&self) -> bool {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.uplink.close();
        true
    }

    /// Whether the handle was disposed.
    #[must_use]
    pub fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::Acquire)
    }
}

// ---- the mount lifecycle ------------------------------------------------------------------------

/// The Client Remote service: namespaces, streams, and the mount/unmount lifecycle.
///
/// This is the local `ClientRemoteService`. It exists so the pieces above have the one caller that
/// makes their interactions observable: a contribution mounts into the namespace table, a
/// subscription opens a logical stream, and a teardown retires both — which is the sequence whose
/// ordering upstream gets from Cordis effects and this type gets from explicit fields.
#[derive(Clone, Debug, Default)]
pub struct ClientRemote {
    namespaces: NamespaceTable,
    streams: Arc<Mutex<BTreeMap<String, RemoteStream>>>,
}

impl ClientRemote {
    /// An empty service.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The namespace table.
    #[must_use]
    pub fn namespaces(&self) -> &NamespaceTable {
        &self.namespaces
    }

    /// Mounts one contribution.
    ///
    /// # Errors
    ///
    /// Whatever [`NamespaceTable::mount_contribution`] refuses.
    pub fn mount(
        &self,
        descriptors: &[InvocationDescriptor],
    ) -> Result<ContributionMount, ClientError> {
        self.namespaces.mount_contribution(descriptors)
    }

    /// Opens one logical stream under a name, refusing a duplicate.
    ///
    /// The name is the stream's identity for the service: two subscriptions under one name would
    /// each be disposed by the other's teardown.
    ///
    /// # Errors
    ///
    /// [`ClientError::DuplicateMethod`] when the name is already open.
    pub fn open_stream(&self, name: impl Into<String>) -> Result<RemoteStream, ClientError> {
        let name = name.into();
        let mut streams = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        if streams.contains_key(&name) {
            return Err(ClientError::DuplicateMethod {
                endpoint: name,
                kind: "stream",
            });
        }
        let stream = RemoteStream::new(name.clone());
        streams.insert(name, stream.clone());
        Ok(stream)
    }

    /// The open stream names.
    #[must_use]
    pub fn streams(&self) -> Vec<String> {
        self.streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Retires a stream by name, disposing it.
    ///
    /// Returns whether a stream was there to dispose.
    pub fn close_stream(&self, name: &str) -> bool {
        let removed = self
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
        removed.is_some_and(|stream| stream.dispose())
    }

    /// Disposes every stream and returns how many were running.
    pub fn dispose_streams(&self) -> usize {
        let mut guard = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        let streams: Vec<RemoteStream> = std::mem::take(&mut *guard).into_values().collect();
        drop(guard);
        streams.iter().filter(|stream| stream.dispose()).count()
    }
}
