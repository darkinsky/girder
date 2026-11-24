//! Peer identity and admission: the part of upstream `packages/api/gateway` that this crate carries
//! as a bare `String`.
//!
//! # What upstream `packages/api/gateway` contains, measured
//!
//! Twelve production sources, 5,691 lines (16 test files add 11,659 more), exporting 25 symbols. The
//! package is the *connection and peer-identity* face of the api layer, and its deciding code groups
//! into six families:
//!
//! | Family | Sources | Lines | What it decides |
//! | --- | --- | --- | --- |
//! | Unary dispatch | `index.ts` (`invokeRpc`, `prepareInvocation`, `dispatchRpc`) | 1,749 (whole file) | Endpoint resolution, exact arguments, validation, **peer liveness**, owner bypass, invocation construction |
//! | Client projection | `client/index.ts`, `remote-stream.ts`, `journal-stream.ts`, `snapshot-stream.ts` | 1,806 | Mounting generated `remote.<namespace>` services, journal/snapshot subscriptions, uplink queues |
//! | Stream mux and wire protocol | `stream-protocol.ts`, `stream-server.ts`, `client/stream-client.ts` | 1,388 | `$events` routing, ready frames, uplink windows, heartbeats, **socket bound to its admitted peer** |
//! | Forwarded Remote events | `client/remote-events.ts`, `index.ts` (`openRemoteEvents`, `broadcastRemoteEvent`, `startRemoteEvent`) | ~640 | Allowlist, per-peer resource check on delivery, **waterfall answering reserved to the owner**, result ownership |
//! | Access and resources | `access.ts` | 179 | `callResources` / `eventResources`, resource kinds, lookups, Context adapters, invalid → deny |
//! | Error vocabulary | `remote-error-codes.ts`, `types.ts` | 238 | The 20 `gateway/*` codes and their `{ endpoint, field }` details |
//!
//! The peer *type* lives one package upstream, in `packages/client/connection` (18 files, 2,751 lines):
//! `Peer` fixes `principal` at construction, owns a disposal signal, and carries `interactions`. The
//! Gateway consumes it but never mints it — `PeerScope` is 61 occurrences across the 12 gateway
//! sources, `principal` 8.
//!
//! # What this build already covers, measured
//!
//! | Local item | File | Peer coverage |
//! | --- | --- | --- |
//! | `RemoteCall { args, peer }` | `remote.rs` (2,275 lines) | 4 lines: the field, its doc ("carried, never verified"), and two constructions |
//! | `RemoteRegistry::authorise` | `remote.rs` | Builds `crate::guard::access::Request { peer, .. }` and asks a mounted `Policy` before the handler runs |
//! | `Policy`, `AllowList`, `ReadOnly`, `DenyAll`, `Declaration` | `access.rs` (233 lines) | 20 lines: the peer is a `String` in `Request`, compared for set membership |
//! | `InvokeRemoteRequest { peer }`, `SubscribeRequest { peer }` | `girder-rpc` (10 peer lines) | Deserialized straight from the request body |
//! | `invoke` handler | `girder-rpc/src/server.rs` (line 324) | Copies `request.peer` into `RemoteCall` unchanged |
//!
//! So of the six families above, **access decisions** are covered as a policy layer over a peer
//! *name*, and everything that makes the name trustworthy is absent. A workspace-wide grep finds
//! **zero** occurrences of `interactions`, of a peer lifetime / `peer-disposed` failure, of an owner
//! bypass, of event-stream ownership binding, and of any Host/Origin fence in `girder-rpc`.
//!
//! # The gaps, largest first
//!
//! 1. **The peer is a wire field with no principal, no lifetime, and no admission** (this module).
//!    A client writes `{"peer": "..."}` into the JSON body and the server dispatches for that name; it
//!    cannot be a different peer than it says, it cannot be dead, and every call it makes is
//!    indistinguishable from the local owner's. Upstream makes `principal` immutable at Peer
//!    construction (`PeerScope.principal`: "no wire field can set it"), refuses a disposed Peer before
//!    reading its payload, reserves waterfall answering to the owner, and binds each event stream to
//!    the Peer that opened it.
//! 2. **Per-call resource resolution** (`access.ts`, 179 lines). [`Declaration`](girder_core::Declaration)
//!    names static resources, so a policy sees `session:any` for every call; upstream derives the
//!    touched ids from validated arguments, treats a malformed reader as `resource-invalid`, and
//!    decides before any lookup activates.
//! 3. **Forwarded-event delivery admission**. `eventResources` plus the per-client `decide` in
//!    `broadcastRemoteEvent` and the revocation of buffered events for a Peer that lost its rights
//!    have no local counterpart at all: `api_remotes_gap` forwards frames and stops there.
//!
//! This module ports gap 1. Gaps 2 and 3 stay recorded here as the remaining work.
//!
//! # What "verify" means when the field is a string
//!
//! Verification cannot mean comparing the string to a secret: there is no secret on the wire, and
//! inventing one would be a second authentication scheme competing with the transport's. It means the
//! peer is decided by the *carrier* — the code that admitted the connection — and travels to
//! [`RemoteCall`](girder_core::RemoteCall) from there. [`admitted_call`] is that move: it takes the
//! admitted [`Peer`], never a claimed peer name, and refuses a peer that has been disposed. The wire's
//! own `peer` field is dropped, not trusted, and [`RESERVED_IDENTITY_FIELDS`] makes a payload that
//! spells an identity a refusal rather than a silently ignored field.
//!
//! # Why this module does not depend on `girder-rpc`
//!
//! The admission decision belongs under the transport, not inside it: `girder-rpc` should be able to
//! hand `admit` the flags it observed (a trusted Host/Origin, a credential) and receive a [`Peer`], or
//! a status it writes back. Keeping the decision here means the same code serves the WebSocket mux,
//! the `/api/remote.invoke` POST, and an in-process caller such as a test, which is exactly the split
//! upstream has between `packages/client/connection` and `packages/api/gateway`.
//!
//! # Compilation note
//!
//! The crate root does not declare this module yet; the integrator adds `pub mod gateway_gap;`. The
//! test binary compiles this exact file by `#[path]`, and under that inclusion `crate::` would name the
//! test crate, so every library reference is spelled `girder_core::`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

/// The `kind` of the principal the local user owns the Host as.
pub const OWNER_KIND: &str = "owner";

/// Gateway-internal logical stream carrying application-selected Host events.
///
/// A method namespace may not shadow it: [`route`] resolves this before splitting the endpoint, which
/// is what upstream's `isGatewayEndpoint` check buys.
pub const EVENTS_ENDPOINT: &str = "$events";

/// Gateway-internal unary endpoint returning one Client event outcome.
pub const EVENTS_RESULT_ENDPOINT: &str = "$events/result";

/// Top-level argument names that would otherwise read as an identity.
///
/// Upstream refuses an argument its descriptor does not declare, and `principal` is never declared.
/// Naming both here makes the refusal explicit: a payload that says who it is has misunderstood the
/// transport, and silently ignoring the field is how a caller ends up believing it worked.
pub const RESERVED_IDENTITY_FIELDS: [&str; 2] = ["peer", "principal"];

/// Every failure code this module can produce.
///
/// The strings are upstream's, not paraphrases: a Client branches on them, so a local spelling of a
/// remote one is a bug a Client cannot detect.
pub const ERROR_CODES: [&str; 8] = [
    "gateway/ambiguous-endpoint",
    "gateway/arguments-invalid",
    "gateway/input-invalid",
    "gateway/interactions-forbidden",
    "gateway/invocation-unavailable",
    "gateway/peer-disposed",
    "gateway/protocol",
    "remote-access/denied",
];

// ---- failures ------------------------------------------------------------------------------------

/// A Gateway-infrastructure failure: something wrong with the call rather than with the business
/// method it named.
///
/// Carries the endpoint because upstream's `TypertGatewayFaultDetails` does (`{ endpoint, field? }`),
/// and because a Client that receives one needs to know which call failed without guessing from the
/// payload it sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayError {
    /// The Peer was disposed — by its owner's teardown, or by the socket that carried it.
    ///
    /// Refused before the payload is read: a disposed Peer's arguments are not worth validating, and
    /// upstream's `assertPeerLive` runs at the top of `remoteRequest` for exactly that reason.
    PeerDisposed {
        /// The endpoint that was attempted.
        endpoint: String,
    },
    /// The Peer may not take part in forwarded human interactions.
    ///
    /// `interactions: none` is what an API operator gets by default: it can call methods, and it must
    /// not answer a question the Host asked a human.
    InteractionsForbidden {
        /// The endpoint that was attempted.
        endpoint: String,
    },
    /// Only an owner Peer answers forwarded events, or the named event stream does not exist.
    InvocationUnavailable {
        /// The endpoint that was attempted.
        endpoint: String,
    },
    /// The arguments did not match the shape the endpoint requires.
    ArgumentsInvalid {
        /// The endpoint that was attempted.
        endpoint: String,
        /// The offending wire field.
        field: String,
    },
    /// A codec rejected the arguments before any name or policy was consulted.
    InputInvalid {
        /// The endpoint that was attempted.
        endpoint: String,
    },
    /// The endpoint did not split into exactly one `namespace/method` pair.
    AmbiguousEndpoint {
        /// The endpoint that was attempted.
        endpoint: String,
    },
    /// A wire message contradicted the protocol.
    Protocol {
        /// The endpoint that was attempted.
        endpoint: String,
        /// What was wrong.
        message: String,
    },
    /// The access policy refused the call, or no policy was mounted to allow it.
    ///
    /// Upstream's code for this is `remote-access/denied`, distinct from `gateway/*` because it comes
    /// from the authorization layer rather than from the gateway.
    Refused {
        /// The endpoint that was attempted.
        endpoint: String,
        /// Why, in words a caller can show its operator.
        reason: String,
    },
}

impl GatewayError {
    /// The stable wire code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PeerDisposed { .. } => "gateway/peer-disposed",
            Self::InteractionsForbidden { .. } => "gateway/interactions-forbidden",
            Self::InvocationUnavailable { .. } => "gateway/invocation-unavailable",
            Self::ArgumentsInvalid { .. } => "gateway/arguments-invalid",
            Self::InputInvalid { .. } => "gateway/input-invalid",
            Self::AmbiguousEndpoint { .. } => "gateway/ambiguous-endpoint",
            Self::Protocol { .. } => "gateway/protocol",
            Self::Refused { .. } => "remote-access/denied",
        }
    }

    /// The endpoint the failure names.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        match self {
            Self::PeerDisposed { endpoint }
            | Self::InteractionsForbidden { endpoint }
            | Self::InvocationUnavailable { endpoint }
            | Self::ArgumentsInvalid { endpoint, .. }
            | Self::InputInvalid { endpoint }
            | Self::AmbiguousEndpoint { endpoint }
            | Self::Protocol { endpoint, .. }
            | Self::Refused { endpoint, .. } => endpoint,
        }
    }

    /// The offending wire field, when the failure is field-specific.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::ArgumentsInvalid { field, .. } => Some(field),
            _ => None,
        }
    }

    /// The `TypertGatewayFaultDetails`-shaped details object.
    ///
    /// `field` is omitted rather than sent as `null` for the same reason the rest of this crate omits
    /// unknown optionals: a Client that distinguishes absent from null would mis-render it.
    #[must_use]
    pub fn details(&self) -> Value {
        match self.field() {
            Some(field) => json!({ "endpoint": self.endpoint(), "field": field }),
            None => json!({ "endpoint": self.endpoint() }),
        }
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PeerDisposed { endpoint } => {
                write!(formatter, "{endpoint}: the calling Peer has been disposed")
            }
            Self::InteractionsForbidden { endpoint } => write!(
                formatter,
                "{endpoint}: forwarded Remote events require interactions=answer"
            ),
            Self::InvocationUnavailable { endpoint } => write!(
                formatter,
                "{endpoint}: only owner Peers answer forwarded Remote events"
            ),
            Self::ArgumentsInvalid { endpoint, field } => {
                write!(formatter, "{endpoint}: invalid wire field `{field}`")
            }
            Self::InputInvalid { endpoint } => {
                write!(
                    formatter,
                    "{endpoint}: the arguments were rejected by their codec"
                )
            }
            Self::AmbiguousEndpoint { endpoint } => {
                write!(formatter, "invalid Remote endpoint {endpoint:?}")
            }
            Self::Protocol { endpoint, message } => write!(formatter, "{endpoint}: {message}"),
            Self::Refused { endpoint, reason } => {
                write!(formatter, "{endpoint}: access denied ({reason})")
            }
        }
    }
}

impl std::error::Error for GatewayError {}

/// Whether `code` is one of [`ERROR_CODES`].
#[must_use]
pub fn is_gateway_error_code(code: &str) -> bool {
    ERROR_CODES.contains(&code)
}

// ---- endpoints -----------------------------------------------------------------------------------

/// A Gateway-owned endpoint, resolved before any namespace lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayEndpoint {
    /// The forwarded-event stream.
    Events,
    /// The unary endpoint a Client answers a forwarded event on.
    EventsResult,
}

/// What one endpoint string addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// A Gateway-owned endpoint that shadows method lookup.
    Gateway(GatewayEndpoint),
    /// An ordinary `namespace/method` pair.
    Method {
        /// Namespace of the receiving service.
        namespace: String,
        /// Exported method name.
        method: String,
    },
}

/// Whether `endpoint` is Gateway-owned.
///
/// Checked before [`route`] splits on `/`, because [`EVENTS_RESULT_ENDPOINT`] contains one and would
/// otherwise resolve to a `$events.result` method — a method no service may implement.
#[must_use]
pub fn is_gateway_endpoint(endpoint: &str) -> bool {
    endpoint == EVENTS_ENDPOINT || endpoint == EVENTS_RESULT_ENDPOINT
}

/// Resolves one endpoint string to its route.
pub fn route(endpoint: &str) -> Result<Route, GatewayError> {
    match endpoint {
        EVENTS_ENDPOINT => return Ok(Route::Gateway(GatewayEndpoint::Events)),
        EVENTS_RESULT_ENDPOINT => return Ok(Route::Gateway(GatewayEndpoint::EventsResult)),
        _ => {}
    }
    let segments: Vec<&str> = endpoint.split('/').collect();
    match segments.as_slice() {
        [namespace, method] if !namespace.is_empty() && !method.is_empty() => Ok(Route::Method {
            namespace: (*namespace).to_owned(),
            method: (*method).to_owned(),
        }),
        _ => Err(GatewayError::AmbiguousEndpoint {
            endpoint: endpoint.to_owned(),
        }),
    }
}

/// The canonical endpoint for a namespace and method.
#[must_use]
pub fn endpoint_of(namespace: &str, method: &str) -> String {
    format!("{namespace}/{method}")
}

// ---- principals ----------------------------------------------------------------------------------

/// The caller identity one Peer speaks for, fixed when the Peer is created.
///
/// Upstream's `Principal` is merge-extensible JSON with a literal `kind` discriminant; a Rust enum
/// cannot be extended by a downstream crate, so this carries the discriminant and one subject instead.
/// That is enough for every decision the Gateway makes — owner or not — while keeping the word
/// `kind` at the wire boundary, where a Client sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    kind: String,
    subject: Option<String>,
}

impl Principal {
    /// The local user who owns this Host.
    #[must_use]
    pub fn owner() -> Self {
        Self {
            kind: OWNER_KIND.to_owned(),
            subject: None,
        }
    }

    /// A non-owner caller, named by its kind and one subject.
    #[must_use]
    pub fn named(kind: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            subject: Some(subject.into()),
        }
    }

    /// The discriminant.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The subject, when the kind names one.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// Whether this is the Host's owner.
    #[must_use]
    pub fn is_owner(&self) -> bool {
        self.kind == OWNER_KIND
    }

    /// The wire form: `{ "kind": ..., "subject": ... }`, subject omitted when absent.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        match self.subject() {
            Some(subject) => json!({ "kind": self.kind, "subject": subject }),
            None => json!({ "kind": self.kind }),
        }
    }

    /// Reads a principal a Host declared *for* a Client — for example in a test fixture.
    ///
    /// This is deliberately not a wire decoder for request payloads: an `owner` claim is refused
    /// rather than honoured, because a value the caller wrote cannot promote the caller. Upstream
    /// states the same rule as a type comment — `principal`: "Caller identity fixed at creation by
    /// in-process Host code; no wire field can set it."
    pub fn from_wire(endpoint: &str, value: &Value) -> Result<Self, GatewayError> {
        let map = value
            .as_object()
            .ok_or_else(|| GatewayError::ArgumentsInvalid {
                endpoint: endpoint.to_owned(),
                field: "principal".to_owned(),
            })?;
        let kind = map
            .get("kind")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .ok_or_else(|| GatewayError::ArgumentsInvalid {
                endpoint: endpoint.to_owned(),
                field: "principal".to_owned(),
            })?;
        if kind == OWNER_KIND {
            return Err(GatewayError::ArgumentsInvalid {
                endpoint: endpoint.to_owned(),
                field: "principal".to_owned(),
            });
        }
        // `subject` is the local spelling; otherwise the first string field, which is how upstream's
        // per-driver entries (`{ kind: 'test-guest', guest: 'x' }`) carry their subject.
        let subject = map
            .get("subject")
            .and_then(Value::as_str)
            .or_else(|| {
                map.iter()
                    .find(|(key, value)| key.as_str() != "kind" && value.is_string())
                    .and_then(|(_, value)| value.as_str())
            })
            .map(str::to_owned);
        Ok(Self {
            kind: kind.to_owned(),
            subject,
        })
    }
}

/// Whether a Peer takes part in forwarded human interactions.
///
/// Two states, not a boolean, because the asymmetric default matters: an in-process Peer defaults to
/// [`Interactions::Answer`], while an API operator obtained through `/connect` defaults to
/// [`Interactions::None`]. Answering a Host question on behalf of a machine is a capability, and one
/// that must be asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Interactions {
    /// May open a forwarded-event stream and answer a waterfall.
    #[default]
    Answer,
    /// May call methods only.
    None,
}

impl Interactions {
    /// Whether the Peer may open a forwarded-event stream at all.
    #[must_use]
    pub fn allows_events(self) -> bool {
        matches!(self, Self::Answer)
    }

    /// Parses the two legal wire spellings.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "answer" => Some(Self::Answer),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Reads the `interactions` query declaration of a `/connect` request.
    ///
    /// At most one declaration is legal, and its absence means [`Interactions::None`] — the API
    /// operator default. Upstream refuses a repeated or unknown declaration with a 400 rather than
    /// guessing, because the difference is whether a machine may answer for a human.
    pub fn from_declarations(values: &[&str], endpoint: &str) -> Result<Self, GatewayError> {
        match values {
            [] => Ok(Self::None),
            [only] => Self::parse(only).ok_or_else(|| GatewayError::Protocol {
                endpoint: endpoint.to_owned(),
                message: "interactions must be declared once as answer or none".to_owned(),
            }),
            _ => Err(GatewayError::Protocol {
                endpoint: endpoint.to_owned(),
                message: "interactions must be declared once as answer or none".to_owned(),
            }),
        }
    }
}

// ---- peers ---------------------------------------------------------------------------------------

/// A stable identity for one party admitted to this Host.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(String);

impl PeerId {
    /// Names a peer.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identity as the wire spells it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for PeerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PeerId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

/// The disposal flag a Peer's clones share.
///
/// Separate from [`Peer`] so a clone cannot fork the lifetime: upstream's `Peer` hangs its signal on
/// the scope that created it, and disposing through any handle ends all of them.
#[derive(Debug)]
struct Lifetime {
    disposed: AtomicBool,
}

/// One party admitted to this Host, as the Gateway sees it.
///
/// This is the local `PeerScope`: an identity, an immutable principal, an interaction capability, and a
/// lifetime. It is `Clone` so a transport can hand one to each logical stream, and every clone shares
/// one lifetime — disposal by any of them disposes all, which is what makes "the socket closed" and
/// "the Peer is gone" the same event.
#[derive(Clone, Debug)]
pub struct Peer {
    id: PeerId,
    principal: Principal,
    interactions: Interactions,
    lifetime: Arc<Lifetime>,
}

impl Peer {
    /// Creates a live Peer, defaulting to [`Interactions::Answer`] as upstream's `PeerOptions` does.
    #[must_use]
    pub fn new(id: PeerId, principal: Principal) -> Self {
        Self {
            id,
            principal,
            interactions: Interactions::Answer,
            lifetime: Arc::new(Lifetime {
                disposed: AtomicBool::new(false),
            }),
        }
    }

    /// Sets the interaction capability.
    #[must_use]
    pub fn with_interactions(mut self, interactions: Interactions) -> Self {
        self.interactions = interactions;
        self
    }

    /// The identity.
    #[must_use]
    pub fn id(&self) -> &PeerId {
        &self.id
    }

    /// The identity it speaks for.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// What it may do beyond calling methods.
    #[must_use]
    pub fn interactions(&self) -> Interactions {
        self.interactions
    }

    /// Whether the Peer has not been disposed.
    #[must_use]
    pub fn is_live(&self) -> bool {
        !self.lifetime.disposed.load(Ordering::Acquire)
    }

    /// Refuses a disposed Peer, naming the endpoint it tried.
    ///
    /// Upstream calls this `assertPeerLive` and runs it before reading a payload, before resolving a
    /// descriptor, and again per stream, so a race with disposal fails closed at every layer.
    pub fn assert_live(&self, endpoint: &str) -> Result<(), GatewayError> {
        if self.is_live() {
            Ok(())
        } else {
            Err(GatewayError::PeerDisposed {
                endpoint: endpoint.to_owned(),
            })
        }
    }

    /// Refuses a Peer that may not take part in forwarded interactions.
    pub fn require_interactions(&self, endpoint: &str) -> Result<(), GatewayError> {
        if self.interactions.allows_events() {
            Ok(())
        } else {
            Err(GatewayError::InteractionsForbidden {
                endpoint: endpoint.to_owned(),
            })
        }
    }

    /// Disposes the Peer and every clone of it.
    ///
    /// Returns whether this call was the one that disposed it, so a teardown path can be idempotent
    /// without a second flag.
    pub fn dispose(&self) -> bool {
        !self.lifetime.disposed.swap(true, Ordering::AcqRel)
    }
}

/// Refuses a payload that spells an identity in its arguments.
///
/// A top-level [`RESERVED_IDENTITY_FIELDS`] key is an error rather than an ignored field: the caller
/// believes it is setting something, and quietly dropping the belief is worse than refusing it.
pub fn assert_no_identity_fields(endpoint: &str, args: &Value) -> Result<(), GatewayError> {
    let Some(map) = args.as_object() else {
        return Ok(());
    };
    for field in RESERVED_IDENTITY_FIELDS {
        if map.contains_key(field) {
            return Err(GatewayError::ArgumentsInvalid {
                endpoint: endpoint.to_owned(),
                field: field.to_owned(),
            });
        }
    }
    Ok(())
}

/// Builds the [`RemoteCall`](girder_core::RemoteCall) for an admitted Peer.
///
/// This is where a peer string stops being a claim. The call's `peer` comes from the [`Peer`] the
/// transport admitted, and no argument can change it; a disposed Peer is refused before the arguments
/// are looked at. A transport that used to copy `request.peer` into the call calls this instead and
/// drops the wire field.
pub fn admitted_call(peer: &Peer, args: Value) -> Result<girder_core::RemoteCall, GatewayError> {
    let endpoint = "*";
    peer.assert_live(endpoint)?;
    assert_no_identity_fields(endpoint, &args)?;
    Ok(girder_core::RemoteCall {
        args,
        peer: peer.id().as_str().to_owned(),
    })
}

// ---- authorisation -------------------------------------------------------------------------------

/// One call's authorisation question, in the terms the local policy layer understands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorisationRequest {
    /// The method as `namespace.method`.
    pub method: String,
    /// What it does.
    pub access: girder_core::Access,
    /// What it touches.
    pub resources: Vec<girder_core::AccessResource>,
}

impl AuthorisationRequest {
    /// Builds a request.
    #[must_use]
    pub fn new(
        method: impl Into<String>,
        access: girder_core::Access,
        resources: Vec<girder_core::AccessResource>,
    ) -> Self {
        Self {
            method: method.into(),
            access,
            resources,
        }
    }

    /// A read of one resource kind.
    #[must_use]
    pub fn read(kind: impl Into<String>, method: impl Into<String>) -> Self {
        Self::new(
            method,
            girder_core::Access::Read,
            vec![girder_core::AccessResource::any_of(kind)],
        )
    }

    /// A write of one resource kind.
    #[must_use]
    pub fn write(kind: impl Into<String>, method: impl Into<String>) -> Self {
        Self::new(
            method,
            girder_core::Access::Write,
            vec![girder_core::AccessResource::any_of(kind)],
        )
    }

    /// An irreversible change of one resource kind.
    #[must_use]
    pub fn destructive(kind: impl Into<String>, method: impl Into<String>) -> Self {
        Self::new(
            method,
            girder_core::Access::Destructive,
            vec![girder_core::AccessResource::any_of(kind)],
        )
    }
}

/// Decides whether `peer` may make `request`.
///
/// Three properties, each of which upstream has and the local `remote.rs` path does not:
///
/// 1. **The owner is not checked.** Upstream's `prepareInvocation` skips resource resolution and the
///    policy entirely for `principal.kind === 'owner'`. That is not a shortcut: the owner is the
///    process's own user, and asking a policy about them would make every local call depend on how a
///    deployment configured a remote one.
/// 2. **A non-owner needs a mounted policy.** `None` is a refusal, not an allow. This is the deliberate
///    opposite of [`RemoteRegistry`](girder_core::RemoteRegistry), whose default is permissive because
///    it serves one local client; a directory that has admitted a *second* peer has already left that
///    case behind.
/// 3. **The peer name handed to the policy comes from the admitted Peer**, never from the payload.
pub fn authorise(
    peer: &Peer,
    request: &AuthorisationRequest,
    policy: Option<&dyn girder_core::Policy>,
) -> Result<(), GatewayError> {
    let endpoint = request.method.clone();
    peer.assert_live(&endpoint)?;
    if peer.principal().is_owner() {
        return Ok(());
    }
    let Some(policy) = policy else {
        return Err(GatewayError::Refused {
            endpoint,
            reason: "no access policy is mounted, so a non-owner Peer cannot be authorised"
                .to_owned(),
        });
    };
    let local = girder_core::AccessRequest {
        peer: peer.id().as_str().to_owned(),
        method: request.method.clone(),
        access: request.access,
        resources: request.resources.clone(),
    };
    match policy.decide(&local) {
        girder_core::AccessDecision::Allow => Ok(()),
        girder_core::AccessDecision::Refuse { reason } => {
            Err(GatewayError::Refused { endpoint, reason })
        }
    }
}

// ---- admission -----------------------------------------------------------------------------------

/// What a request carried to prove it belongs to a Peer.
///
/// The variants are the three cases `rpc-host.ts`'s `admit` distinguishes: a browser with its cookie,
/// an API operator with a published id, and nothing at all. Whether the request passed the Host/Origin
/// fence is a separate input, because that fence protects the listener and the credential protects the
/// identity — upstream refuses them with different statuses for that reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Credential<'a> {
    /// The shared browser cookie.
    Browser,
    /// An API operator id.
    Api(&'a str),
    /// No credential.
    None,
}

/// Why an admission was refused.
///
/// The status is part of the type rather than a comment because the callers write it into a response:
/// 403 for an untrusted Host, 401 for an unknown credential, 409 for a second socket claiming one Peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The request was not authenticated.
    Unauthorized,
    /// The request came from an untrusted Host or Origin.
    Forbidden,
    /// Another socket already owns this Peer.
    Conflict,
}

impl Rejection {
    /// The HTTP status a carrier writes back.
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::Conflict => 409,
        }
    }
}

/// The result of admitting one request.
#[derive(Clone, Debug)]
pub enum Admission {
    /// A live Peer the request speaks for.
    Admitted(Peer),
    /// A refusal the carrier writes back.
    Rejected(Rejection),
}

/// The result of admitting one socket.
#[derive(Clone, Debug)]
pub enum ConnectionOutcome {
    /// The socket owns the Peer until [`Connection::release`].
    Admitted(Connection),
    /// A refusal the carrier writes back.
    Rejected(Rejection),
}

impl From<Admission> for Result<Peer, Rejection> {
    fn from(admission: Admission) -> Self {
        match admission {
            Admission::Admitted(peer) => Ok(peer),
            Admission::Rejected(rejection) => Err(rejection),
        }
    }
}

impl From<ConnectionOutcome> for Result<Connection, Rejection> {
    fn from(outcome: ConnectionOutcome) -> Self {
        match outcome {
            ConnectionOutcome::Admitted(connection) => Ok(connection),
            ConnectionOutcome::Rejected(rejection) => Err(rejection),
        }
    }
}

/// A socket's exclusive claim on one Peer.
///
/// Exclusive because a Peer's lifetime is a socket's lifetime: two sockets admitting one Peer would
/// each dispose it when the other was still using it, and the second is refused with 409 rather than
/// sharing.
#[derive(Clone, Debug)]
pub struct Connection {
    peer: Peer,
    directory: PeerDirectory,
}

impl Connection {
    /// The Peer the socket speaks for.
    #[must_use]
    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// Gives up the claim and disposes the Peer.
    ///
    /// Idempotent: the second call finds no record and disposes nothing, mirroring upstream's
    /// `release` returning one shared completion.
    pub fn release(&self) -> bool {
        self.directory.retire(self.peer.id())
    }
}

/// The table of Peers this Host has admitted.
///
/// Upstream's `rpc-host.ts` keeps the same three things: one shared browser Peer, a map of published
/// API Peers with an `ownsSocket` flag, and a disposed flag that turns every admission into a 401. The
/// names here are local; the decisions are not.
#[derive(Clone, Debug)]
pub struct PeerDirectory {
    inner: Arc<DirectoryInner>,
}

#[derive(Debug)]
struct DirectoryInner {
    browser: Peer,
    peers: Mutex<BTreeMap<String, Record>>,
    grants: Mutex<BTreeMap<String, Vec<AccessGrant>>>,
    access_changed: Arc<tokio::sync::Notify>,
    next: AtomicU64,
    disposed: AtomicBool,
}

/// One resource grant held by a non-owner peer.
///
/// Actions are stored sorted so a snapshot can compare two generations without treating order as a
/// change. An absent id is a kind-wide grant; a present id names one resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessGrant {
    /// Resource kind, such as `session` or `global`.
    pub kind: String,
    /// Resource identity. `None` grants the kind as a whole.
    pub id: Option<String>,
    /// Endpoint actions, spelled `namespace/method` and sorted.
    pub actions: Vec<String>,
}

impl AccessGrant {
    /// Builds a grant and normalizes its action list.
    #[must_use]
    pub fn new(
        kind: impl Into<String>,
        id: Option<String>,
        actions: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let mut actions: Vec<String> = actions.into_iter().map(Into::into).collect();
        actions.sort();
        actions.dedup();
        Self {
            kind: kind.into(),
            id: id.filter(|id| !id.is_empty()),
            actions,
        }
    }

    /// The `remoteAccess.follow` wire object.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        let mut resource = serde_json::Map::new();
        resource.insert("kind".to_owned(), Value::String(self.kind.clone()));
        if let Some(id) = &self.id {
            resource.insert("id".to_owned(), Value::String(id.clone()));
        }
        json!({ "resource": resource, "actions": self.actions })
    }
}

#[derive(Debug)]
struct Record {
    peer: Peer,
    owns_socket: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PeerDirectory {
    /// A directory serving one shared browser Peer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DirectoryInner {
                browser: Peer::new(PeerId::new("browser"), Principal::owner()),
                peers: Mutex::new(BTreeMap::new()),
                grants: Mutex::new(BTreeMap::new()),
                access_changed: Arc::new(tokio::sync::Notify::new()),
                next: AtomicU64::new(1),
                disposed: AtomicBool::new(false),
            }),
        }
    }

    /// The shared owner Peer every browser request speaks for.
    #[must_use]
    pub fn browser(&self) -> Peer {
        self.inner.browser.clone()
    }

    /// Whether the directory has been disposed.
    #[must_use]
    pub fn is_disposed(&self) -> bool {
        self.inner.disposed.load(Ordering::Acquire)
    }

    /// Publishes a Peer an API credential can name, returning it.
    pub fn publish(&self, principal: Principal, interactions: Interactions) -> Peer {
        let sequence = self.inner.next.fetch_add(1, Ordering::AcqRel);
        let peer = Peer::new(PeerId::new(format!("peer-{sequence}")), principal)
            .with_interactions(interactions);
        lock(&self.inner.peers).insert(
            peer.id().as_str().to_owned(),
            Record {
                peer: peer.clone(),
                owns_socket: false,
            },
        );
        peer
    }

    /// Replaces the grants held by one peer and wakes `remoteAccess.follow`.
    pub fn set_grants(&self, id: &PeerId, grants: Vec<AccessGrant>) {
        lock(&self.inner.grants).insert(id.as_str().to_owned(), grants);
        self.inner.access_changed.notify_waiters();
    }

    /// The grants currently held by one peer.
    #[must_use]
    pub fn grants(&self, id: &PeerId) -> Vec<AccessGrant> {
        lock(&self.inner.grants)
            .get(id.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// Wakes when any peer's grants are replaced.
    #[must_use]
    pub fn access_changed(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.inner.access_changed)
    }

    /// Looks a published Peer up by identity.
    #[must_use]
    pub fn peer(&self, id: &PeerId) -> Option<Peer> {
        lock(&self.inner.peers)
            .get(id.as_str())
            .map(|record| record.peer.clone())
    }

    /// How many Peers are published.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.inner.peers).len()
    }

    /// Whether no Peer is published.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Admits one request, in upstream's order: trust, then disposal, then credential.
    ///
    /// Trust first is what makes a rebound page's cross-site request a 403 even before its cookie is
    /// examined; disposal next turns every later admission into the 401 a closing Host should send.
    #[must_use]
    pub fn admit(&self, trusted: bool, credential: Credential<'_>) -> Admission {
        if !trusted {
            return Admission::Rejected(Rejection::Forbidden);
        }
        if self.is_disposed() {
            return Admission::Rejected(Rejection::Unauthorized);
        }
        match credential {
            Credential::Browser => Admission::Admitted(self.inner.browser.clone()),
            Credential::Api(id) => match lock(&self.inner.peers).get(id) {
                Some(record) => Admission::Admitted(record.peer.clone()),
                None => Admission::Rejected(Rejection::Unauthorized),
            },
            Credential::None => Admission::Rejected(Rejection::Unauthorized),
        }
    }

    /// Admits one socket, claiming its Peer exclusively.
    #[must_use]
    pub fn admit_connection(&self, trusted: bool, credential: Credential<'_>) -> ConnectionOutcome {
        let peer = match self.admit(trusted, credential) {
            Admission::Admitted(peer) => peer,
            Admission::Rejected(rejection) => return ConnectionOutcome::Rejected(rejection),
        };
        // The browser Peer is shared with unary calls and is not claimed by a socket; an API Peer is.
        if peer.id() == self.inner.browser.id() {
            return ConnectionOutcome::Admitted(Connection {
                peer,
                directory: self.clone(),
            });
        }
        let mut peers = lock(&self.inner.peers);
        let Some(record) = peers.get_mut(peer.id().as_str()) else {
            return ConnectionOutcome::Rejected(Rejection::Unauthorized);
        };
        if record.owns_socket {
            return ConnectionOutcome::Rejected(Rejection::Conflict);
        }
        record.owns_socket = true;
        drop(peers);
        ConnectionOutcome::Admitted(Connection {
            peer,
            directory: self.clone(),
        })
    }

    /// Disposes every Peer and refuses every later admission.
    ///
    /// Returns whether this call was the one that disposed the directory.
    pub fn dispose(&self) -> bool {
        if self.inner.disposed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner.browser.dispose();
        for record in lock(&self.inner.peers).values() {
            record.peer.dispose();
        }
        true
    }

    /// Removes one Peer, disposing it and freeing its socket claim.
    fn retire(&self, id: &PeerId) -> bool {
        let Some(record) = lock(&self.inner.peers).remove(id.as_str()) else {
            return false;
        };
        record.peer.dispose();
        true
    }
}

impl Default for PeerDirectory {
    fn default() -> Self {
        Self::new()
    }
}

// ---- forwarded-event streams ---------------------------------------------------------------------

/// One opened client event generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedStream {
    /// The generation identity later results are bound to.
    pub client_id: String,
    /// Whether this stream's Peer may answer a waterfall.
    pub answers: bool,
}

/// The live forwarded-event streams, one per admitted client generation.
///
/// Two bindings, both of which upstream has and the local forwarding path does not:
///
/// - a stream is opened *by* a Peer and belongs to it, so a result naming someone else's generation is
///   refused rather than accepted for whichever peer asked;
/// - only an owner Peer answers a waterfall. A non-owner stream is a subscriber: it receives the
///   broadcasts its resources allow and nothing more.
#[derive(Clone, Debug)]
pub struct EventStreams {
    inner: Arc<StreamInner>,
}

#[derive(Debug)]
struct StreamInner {
    clients: Mutex<BTreeMap<String, ClientRecord>>,
    next: AtomicU64,
}

#[derive(Debug)]
struct ClientRecord {
    peer: PeerId,
    answers: bool,
}

impl EventStreams {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StreamInner {
                clients: Mutex::new(BTreeMap::new()),
                next: AtomicU64::new(1),
            }),
        }
    }

    /// Opens one generation for `peer`.
    ///
    /// The payload is the empty standard Remote envelope: a Gateway-owned stream reads no uplink, and
    /// upstream releases the carrier's iterator immediately rather than buffering it. Anything else in
    /// the payload is refused, because accepting and ignoring it would let a caller believe a
    /// subscription option took effect.
    pub fn open(
        &self,
        peer: &Peer,
        endpoint: &str,
        payload: &Value,
    ) -> Result<OpenedStream, GatewayError> {
        peer.assert_live(endpoint)?;
        peer.require_interactions(endpoint)?;
        assert_only_empty_args(endpoint, payload)?;
        let answers = peer.principal().is_owner();
        let mut clients = lock(&self.inner.clients);
        loop {
            let sequence = self.inner.next.fetch_add(1, Ordering::AcqRel);
            let client_id = format!("stream-{sequence}");
            if clients.contains_key(&client_id) {
                continue;
            }
            clients.insert(
                client_id.clone(),
                ClientRecord {
                    peer: peer.id().clone(),
                    answers,
                },
            );
            drop(clients);
            return Ok(OpenedStream { client_id, answers });
        }
    }

    /// Whether `peer` may answer a result for `client_id`.
    ///
    /// The order is upstream's: live, then interactions, then owner, then existence, then ownership.
    /// A guest therefore learns it may not answer at all before it learns the generation is not its
    /// own — the same information an attacker would otherwise probe for.
    pub fn assert_answers(
        &self,
        peer: &Peer,
        client_id: &str,
        endpoint: &str,
    ) -> Result<bool, GatewayError> {
        peer.assert_live(endpoint)?;
        peer.require_interactions(endpoint)?;
        if !peer.principal().is_owner() {
            return Err(GatewayError::InvocationUnavailable {
                endpoint: endpoint.to_owned(),
            });
        }
        let clients = lock(&self.inner.clients);
        let Some(record) = clients.get(client_id) else {
            return Err(GatewayError::Protocol {
                endpoint: endpoint.to_owned(),
                message: format!(
                    "Remote event result identifies no active event stream (`{client_id}`)"
                ),
            });
        };
        if record.peer != *peer.id() {
            return Err(GatewayError::InteractionsForbidden {
                endpoint: endpoint.to_owned(),
            });
        }
        Ok(record.answers)
    }

    /// Whether `client_id` is an open generation, and whether it may answer.
    #[must_use]
    pub fn answers(&self, client_id: &str) -> Option<bool> {
        lock(&self.inner.clients)
            .get(client_id)
            .map(|record| record.answers)
    }

    /// The Peer that owns `client_id`.
    #[must_use]
    pub fn owner_of(&self, client_id: &str) -> Option<PeerId> {
        lock(&self.inner.clients)
            .get(client_id)
            .map(|record| record.peer.clone())
    }

    /// Drops every generation belonging to `peer`, returning how many were dropped.
    ///
    /// This is the local form of the upstream rule that a revoked Peer's buffered events are discarded
    /// while other streams keep running: the queue belongs to the generation, so removing the
    /// generation removes what was buffered for it.
    pub fn revoke(&self, peer: &PeerId) -> usize {
        let mut clients = lock(&self.inner.clients);
        let before = clients.len();
        clients.retain(|_, record| record.peer != *peer);
        before - clients.len()
    }

    /// How many generations are open.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.inner.clients).len()
    }

    /// Whether no generation is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for EventStreams {
    fn default() -> Self {
        Self::new()
    }
}

/// Refuses a payload that is not exactly `{ "args": {} }`.
fn assert_only_empty_args(endpoint: &str, payload: &Value) -> Result<(), GatewayError> {
    let Some(map) = payload.as_object() else {
        return Err(GatewayError::ArgumentsInvalid {
            endpoint: endpoint.to_owned(),
            field: "args".to_owned(),
        });
    };
    let args = match map.len() {
        1 => map.get("args"),
        _ => None,
    };
    match args.and_then(Value::as_object) {
        Some(args) if args.is_empty() => Ok(()),
        _ => Err(GatewayError::ArgumentsInvalid {
            endpoint: endpoint.to_owned(),
            field: "args".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Credential, EVENTS_ENDPOINT, GatewayError, Interactions, Peer, PeerDirectory, PeerId,
        Principal, RESERVED_IDENTITY_FIELDS, admitted_call, assert_no_identity_fields, authorise,
        endpoint_of, route,
    };
    use serde_json::json;

    fn guest() -> Peer {
        Peer::new(
            PeerId::new("guest-1"),
            Principal::named("test-guest", "spec"),
        )
    }

    #[test]
    fn the_owner_is_recognised_and_carries_no_subject() {
        let owner = Principal::owner();
        assert!(owner.is_owner());
        assert_eq!(owner.kind(), "owner");
        assert_eq!(owner.subject(), None);
    }

    #[test]
    fn a_wire_owner_claim_is_refused() {
        let claimed = json!({ "kind": "owner" });
        assert_eq!(
            Principal::from_wire(EVENTS_ENDPOINT, &claimed),
            Err(GatewayError::ArgumentsInvalid {
                endpoint: EVENTS_ENDPOINT.to_owned(),
                field: "principal".to_owned(),
            })
        );
    }

    #[test]
    fn a_named_principal_reads_its_subject_from_the_extra_field() {
        let principal = Principal::from_wire(
            EVENTS_ENDPOINT,
            &json!({
                "kind": "test-guest",
                "guest": "session-controller",
            }),
        )
        .expect("a non-owner principal is readable");
        assert_eq!(
            principal,
            Principal::named("test-guest", "session-controller")
        );
    }

    #[test]
    fn a_named_principal_round_trips_through_its_wire_form() {
        let principal = Principal::named("test-guest", "spec");
        let wire = principal.to_wire();
        assert_eq!(wire, json!({ "kind": "test-guest", "subject": "spec" }));
        assert_eq!(
            Principal::from_wire(EVENTS_ENDPOINT, &wire),
            Ok(Principal::named("test-guest", "spec"))
        );
    }

    #[test]
    fn identity_fields_in_arguments_are_refused_rather_than_ignored() {
        for field in RESERVED_IDENTITY_FIELDS {
            let mut args = serde_json::Map::new();
            args.insert(field.to_owned(), json!("owner"));
            let error = assert_no_identity_fields("session/list", &json!(args))
                .expect_err("an identity field is refused");
            assert_eq!(error.field(), Some(field));
        }
        assert!(assert_no_identity_fields("session/list", &json!({ "sessionId": "s1" })).is_ok());
    }

    #[test]
    fn an_admitted_call_takes_its_peer_from_the_peer_not_the_payload() {
        let directory = PeerDirectory::new();
        let peer = directory.browser();
        let call =
            admitted_call(&peer, json!({ "sessionId": "s1" })).expect("a live peer builds a call");
        assert_eq!(call.peer, "browser");
        assert_eq!(call.args, json!({ "sessionId": "s1" }));
    }

    #[test]
    fn a_disposed_peer_builds_no_call() {
        let peer = guest();
        assert!(peer.dispose());
        let Err(error) = admitted_call(&peer, json!({})) else {
            panic!("a disposed peer is refused")
        };
        assert_eq!(error.code(), "gateway/peer-disposed");
    }

    #[test]
    fn disposing_one_clone_disposes_every_clone() {
        let peer = guest();
        let clone = peer.clone();
        assert!(clone.dispose());
        assert!(!peer.is_live());
        assert!(!clone.dispose());
    }

    #[test]
    fn the_events_route_wins_over_the_method_split() {
        assert_eq!(
            route("$events/result").expect("a gateway endpoint"),
            super::Route::Gateway(super::GatewayEndpoint::EventsResult)
        );
        assert_eq!(
            route("session/list").expect("a method"),
            super::Route::Method {
                namespace: "session".to_owned(),
                method: "list".to_owned(),
            }
        );
        assert_eq!(
            route("session/list/extra")
                .expect_err("three segments are ambiguous")
                .code(),
            "gateway/ambiguous-endpoint"
        );
        assert_eq!(endpoint_of("session", "list"), "session/list");
    }

    #[test]
    fn the_interactions_declaration_defaults_to_none_for_an_api_operator() {
        assert_eq!(
            Interactions::from_declarations(&[], "/connect"),
            Ok(Interactions::None)
        );
        assert_eq!(
            Interactions::from_declarations(&["answer"], "/connect"),
            Ok(Interactions::Answer)
        );
        assert_eq!(
            Interactions::from_declarations(&["answer", "none"], "/connect")
                .expect_err("a repeat is refused")
                .code(),
            "gateway/protocol"
        );
        assert_eq!(Interactions::default(), Interactions::Answer);
    }

    #[test]
    fn a_peer_without_interactions_may_not_open_a_stream() {
        let streams = super::EventStreams::new();
        let peer = guest().with_interactions(Interactions::None);
        let error = streams
            .open(&peer, EVENTS_ENDPOINT, &json!({ "args": {} }))
            .expect_err("interactions=none is refused");
        assert_eq!(error.code(), "gateway/interactions-forbidden");
        assert!(streams.is_empty());
    }

    #[test]
    fn an_untrusted_request_is_forbidden_before_its_credential_is_read() {
        let directory = PeerDirectory::new();
        assert!(matches!(
            directory.admit(false, Credential::Browser),
            super::Admission::Rejected(super::Rejection::Forbidden)
        ));
    }

    #[test]
    fn a_missing_or_unknown_credential_is_unauthorized() {
        let directory = PeerDirectory::new();
        assert!(matches!(
            directory.admit(true, Credential::None),
            super::Admission::Rejected(super::Rejection::Unauthorized)
        ));
        assert!(matches!(
            directory.admit(true, Credential::Api("peer-404")),
            super::Admission::Rejected(super::Rejection::Unauthorized)
        ));
    }

    #[test]
    fn the_browser_credential_shares_one_owner_peer() {
        let directory = PeerDirectory::new();
        let first = match directory.admit(true, Credential::Browser) {
            super::Admission::Admitted(peer) => peer,
            other @ super::Admission::Rejected(_) => {
                panic!("expected an admission, got {other:?}")
            }
        };
        let second = match directory.admit(true, Credential::Browser) {
            super::Admission::Admitted(peer) => peer,
            other @ super::Admission::Rejected(_) => {
                panic!("expected an admission, got {other:?}")
            }
        };
        assert_eq!(first.id(), second.id());
        assert!(first.principal().is_owner());
    }

    #[test]
    fn a_published_peer_is_admitted_by_its_credential() {
        let directory = PeerDirectory::new();
        let published =
            directory.publish(Principal::named("test-guest", "api"), Interactions::None);
        let admitted = match directory.admit(true, Credential::Api(published.id().as_str())) {
            super::Admission::Admitted(peer) => peer,
            other @ super::Admission::Rejected(_) => {
                panic!("expected an admission, got {other:?}")
            }
        };
        assert_eq!(admitted.id(), published.id());
        assert_eq!(admitted.interactions(), Interactions::None);
    }

    #[test]
    fn a_disposed_directory_refuses_every_admission() {
        let directory = PeerDirectory::new();
        assert!(directory.dispose());
        assert!(!directory.dispose());
        assert!(matches!(
            directory.admit(true, Credential::Browser),
            super::Admission::Rejected(super::Rejection::Unauthorized)
        ));
    }

    #[test]
    fn a_second_socket_on_one_peer_conflicts_and_release_frees_it() {
        let directory = PeerDirectory::new();
        let published =
            directory.publish(Principal::named("test-guest", "api"), Interactions::None);
        let id = published.id().as_str().to_owned();
        let connection = match directory.admit_connection(true, Credential::Api(&id)) {
            super::ConnectionOutcome::Admitted(connection) => connection,
            other @ super::ConnectionOutcome::Rejected(_) => {
                panic!("expected a connection, got {other:?}")
            }
        };
        assert!(matches!(
            directory.admit_connection(true, Credential::Api(&id)),
            super::ConnectionOutcome::Rejected(super::Rejection::Conflict)
        ));
        assert_eq!(super::Rejection::Conflict.status(), 409);
        assert!(connection.release());
        assert!(!connection.release());
        assert!(!connection.peer().is_live());
        // Release removes the record as well as disposing the Peer, so the credential names nothing
        // afterwards: a closed socket's id is not a reusable ticket.
        assert!(matches!(
            directory.admit_connection(true, Credential::Api(&id)),
            super::ConnectionOutcome::Rejected(super::Rejection::Unauthorized)
        ));
    }

    #[test]
    fn the_browser_peer_is_shared_and_not_claimed_by_a_socket() {
        let directory = PeerDirectory::new();
        let first = directory.admit_connection(true, Credential::Browser);
        let second = directory.admit_connection(true, Credential::Browser);
        assert!(matches!(first, super::ConnectionOutcome::Admitted(_)));
        assert!(matches!(second, super::ConnectionOutcome::Admitted(_)));
    }

    #[test]
    fn the_owner_bypasses_even_a_policy_that_refuses_everyone() {
        let peer = PeerDirectory::new().browser();
        let policy = girder_core::DenyAll;
        assert_eq!(
            authorise(
                &peer,
                &super::AuthorisationRequest::read("session", "session.list"),
                Some(&policy)
            ),
            Ok(())
        );
    }

    #[test]
    fn a_non_owner_without_a_policy_is_refused_rather_than_allowed() {
        let peer = guest();
        let error = authorise(
            &peer,
            &super::AuthorisationRequest::read("session", "session.list"),
            None,
        )
        .expect_err("no policy fails closed for a non-owner");
        assert_eq!(error.code(), "remote-access/denied");
    }

    #[test]
    fn an_allowlisted_peer_is_admitted_and_its_name_reaches_the_policy() {
        let peer = guest();
        let policy = girder_core::AllowList::new().allowing("guest-1");
        assert_eq!(
            authorise(
                &peer,
                &super::AuthorisationRequest::read("session", "session.list"),
                Some(&policy)
            ),
            Ok(())
        );
        let refusal = authorise(
            &peer,
            &super::AuthorisationRequest::read("session", "session.list"),
            Some(&girder_core::AllowList::new()),
        )
        .expect_err("an absent peer is refused");
        assert!(refusal.to_string().contains("guest-1"), "{refusal}");
    }

    #[test]
    fn a_read_only_policy_refuses_a_non_owner_write_but_allows_a_read() {
        let peer = guest();
        let policy = girder_core::ReadOnly;
        assert_eq!(
            authorise(
                &peer,
                &super::AuthorisationRequest::read("session", "session.list"),
                Some(&policy)
            ),
            Ok(())
        );
        assert_eq!(
            authorise(
                &peer,
                &super::AuthorisationRequest::write("session", "session.rename"),
                Some(&policy)
            )
            .expect_err("a write is refused")
            .code(),
            "remote-access/denied"
        );
    }

    #[test]
    fn a_disposed_peer_is_refused_before_the_policy_is_consulted() {
        let peer = guest();
        peer.dispose();
        let error = authorise(
            &peer,
            &super::AuthorisationRequest::read("session", "session.list"),
            Some(&girder_core::DenyAll),
        )
        .expect_err("disposal is reported first");
        assert_eq!(error.code(), "gateway/peer-disposed");
    }
}
