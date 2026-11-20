//! The forwarded-Host-event face of upstream `packages/api/remotes`.
//!
//! Upstream `packages/api/remotes` is a two-face assembly: a **Host** entry
//! (`src/index.ts`, `src/remote-events.ts`, `src/types.ts`) and a **Client** entry
//! (`src/client/index.ts`). The Client entry is bookkeeping — mount thirty-one generated
//! contributions through `ctx.remote.$mount` and unwind them in reverse — and this crate already
//! has that mechanism ([`Scope`](girder_core::Scope) disposes in reverse registration order), so
//! the Client face is not what this module ports.
//!
//! The Host face is the part with no local counterpart at all, and it holds every decision worth
//! reproducing:
//!
//! 1. [`FORWARDED_EVENTS`] — the *allowlist* of Host events a Client may receive, and the delivery
//!    mode of each: `emit` (one-way notification) or `waterfall` (Agent-scoped, answerable). It is
//!    the legal key set of `ctx.remote.$on`, and the single control point over what crosses.
//! 2. [`resources_of`] — which resources a notification touches, so a non-owner Peer receives it
//!    only when access control allows it. A notification with no readers touches the built-in
//!    `global` resource; a reader that cannot name its resource makes the notification invalid
//!    rather than global.
//! 3. [`EventQueue`] — the pull-driven queue that bridges synchronous Host listeners to the
//!    asynchronous stream the Gateway consumes, one queue per Client stream, plus its end
//!    semantics ([`EventQueue::end`] rejects every buffered invocation, and a cancel discards the
//!    buffered notifications so a revoked Peer receives none of them).
//! 4. [`forward_waterfall`] — the Agent-scoped waterfall bridge: delegate to the following Host
//!    listener when there is no carrier Agent, refuse a request that does not carry its Agent
//!    directly, and otherwise hand the Client a [`WaterfallTicket`](WaterfallTicket) to settle.
//!
//! # What this module deliberately does not port
//!
//! Everything Cordis-shaped: `ctx.on`, `ctx.effect`, `ctx.waterfall`, `ctx.typertGateway`, the
//! `Events` declaration merges, `carrierKeyOf`, and the `WeakMap`/`Fiber` ownership that ties a
//! listener's life to a plugin. This crate's [`EventBus`](girder_core::EventBus) owns listener
//! registration and removal, and a module that pretended to own Cordis ownership as well would be
//! a second, worse implementation of it.
//!
//! The wire types (`TypertRemoteEventFrame`, `TypertRemoteEventInvocation`) are also not ported as
//! types: this crate's transport already defines its own downlink frames
//! (`girder_rpc::DownlinkFrame`). What is ported is the *shape* those frames must have, as
//! [`Frame::to_wire`] and [`WaterfallSlot::to_wire`], so a transport can build them without
//! re-deriving the field names.
//!
//! # Why the queue is unbounded here, when the rest of this crate is not
//!
//! Upstream leaves [`EventQueue`] unbounded on purpose: it is the bridge between one synchronous
//! listener and one stream, and the bound lives one layer up, in the Gateway's per-Client queue.
//! This crate has the same split — the bound is [`EventBus::observe`](girder_core::EventBus::observe)'s
//! 4096-item observer queue, which drops an observer that falls behind — so adding a second,
//! smaller bound here would turn a slow Client into *two* independent loss points, each with its
//! own accounting. [`EventQueue::len`] is exposed so the layer that owns the bound can enforce it.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};
use tokio::sync::{Notify, oneshot};

use girder_core::is_lossless;

/// How one allowlisted Host event reaches a Client.
///
/// Upstream spells these `mode: 'emit'` and `mode: 'waterfall'` in the allowlist array. The split
/// is not cosmetic: an `emit` entry can be broadcast to every Client that access control admits,
/// while a `waterfall` entry is a pending question whose answer has to travel back before the Host
/// listener continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// A one-way notification. Listener return values are ignored and nothing travels back.
    Emit,
    /// An Agent-scoped waterfall: the Client may answer, or delegate to the next Host listener.
    Waterfall,
}

impl Delivery {
    /// The upstream spelling, for diagnostics and for a manifest a Client can read.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Emit => "emit",
            Self::Waterfall => "waterfall",
        }
    }
}

/// Reads the resource identity a notification's arguments carry, for one resource kind.
///
/// Called with the event's arguments as positional parameters, matching upstream's
/// `TypertResourceReader`. `None` means the reader could not name its resource — upstream throws in
/// that case and the notification becomes *invalid* rather than global, so an event that promised
/// to name a Session never silently reaches a Peer that was not authorized for one.
pub type ResourceReader = fn(&[Value]) -> Option<String>;

/// One entry of the forwarded-Host-event allowlist.
#[derive(Clone, Copy, Debug)]
pub struct ForwardedEvent {
    name: &'static str,
    delivery: Delivery,
    readers: &'static [(&'static str, ResourceReader)],
}

impl ForwardedEvent {
    /// The Host Cordis event name, spelled exactly as the Host dispatches it.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// How this event travels.
    #[must_use]
    pub fn delivery(&self) -> Delivery {
        self.delivery
    }

    /// Whether this event declares any resource reader.
    ///
    /// An event with none touches the built-in `global` resource, which is what makes a
    /// payload-free invalidation event (`schedule/changed`) reachable by every admitted Client.
    #[must_use]
    pub fn reads_resources(&self) -> bool {
        !self.readers.is_empty()
    }
}

/// The resource kind a notification with no readers touches.
pub const GLOBAL_RESOURCE_KIND: &str = "global";

/// The one home of this application's forwarded-Host-event allowlist.
///
/// Order is upstream's order, and it is load-bearing in one respect only: `Discovery` of "which
/// key can a Client subscribe to" is derived from this list, so an entry added here becomes
/// subscribable with no second declaration to keep in step. Entries are otherwise independent.
///
/// Upstream types this as `as const satisfies readonly TypertForwardableEventEntry[]`, which
/// statically rejects an `emit` entry that names a waterfall event and vice versa. Rust has no such
/// constraint at the declaration site; [`validate_allowlist`] is the runtime stand-in, and
/// [`Delivery::Waterfall`] entries carrying resource readers is refused because upstream's
/// waterfall wire frame has no `resources` slot.
pub const FORWARDED_EVENTS: &[ForwardedEvent] = &[
    ForwardedEvent {
        name: "agent-preset/selected",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "approval/request",
        delivery: Delivery::Waterfall,
        readers: &[],
    },
    ForwardedEvent {
        name: "api-session/activity",
        delivery: Delivery::Emit,
        readers: &[(SESSION_RESOURCE_KIND, session_id_argument)],
    },
    ForwardedEvent {
        name: "api-session/added",
        delivery: Delivery::Emit,
        readers: &[(SESSION_RESOURCE_KIND, session_summary_argument)],
    },
    ForwardedEvent {
        name: "api-session/error",
        delivery: Delivery::Emit,
        readers: &[(SESSION_RESOURCE_KIND, session_id_argument)],
    },
    ForwardedEvent {
        name: "api-session/removed",
        delivery: Delivery::Emit,
        readers: &[(SESSION_RESOURCE_KIND, session_id_argument)],
    },
    ForwardedEvent {
        name: "api-session/status",
        delivery: Delivery::Emit,
        readers: &[(SESSION_RESOURCE_KIND, session_id_argument)],
    },
    ForwardedEvent {
        name: "commands/change",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "deepseek-account/session-expired",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "deepseek-account/model-sign-in-required",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "credentials/record-updated",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "credentials/reference-updated",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "goal/activation-changed",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "cordis/request-run",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "cordis/request-run-resolved",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "cordis/dynamic-package",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "cordis/dynamic-retract",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "cordis/inspect-query",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "cordis/inspect-query-resolved",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "llm/adapters-updated",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "llm/models-updated",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "permission-presets/catalog-changed",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "plugin-manager/changed",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "plugin-manager/install-log",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "plugin-manager/install-state",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "plugin-manager/update-state",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "plugin-market/changed",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "settings/document-updated",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "schedule/changed",
        delivery: Delivery::Emit,
        readers: &[],
    },
    ForwardedEvent {
        name: "user-questions/request",
        delivery: Delivery::Waterfall,
        readers: &[],
    },
    ForwardedEvent {
        name: "web/providers-changed",
        delivery: Delivery::Emit,
        readers: &[],
    },
];

/// The resource kind the Session-tagged notifications declare.
pub const SESSION_RESOURCE_KIND: &str = "session";

/// The allowlisted entries, by name.
///
/// Linear search over thirty-one entries rather than a map: a lookup is one dispatch, and a `Map`
/// would need a lazy initializer or a `phf` dependency for a table this size.
#[must_use]
pub fn forwarded_event(name: &str) -> Option<&'static ForwardedEvent> {
    FORWARDED_EVENTS.iter().find(|entry| entry.name == name)
}

/// Whether a Client may subscribe to `name`.
///
/// The consumer-side name of [`forwarded_event`]'s answer: `ctx.remote.$on` accepts exactly these
/// keys and nothing else, which is why the check is a function rather than a comment.
#[must_use]
pub fn is_forwardable(name: &str) -> bool {
    forwarded_event(name).is_some()
}

/// The allowlisted names, in upstream order.
#[must_use]
pub fn forwarded_event_names() -> Vec<&'static str> {
    FORWARDED_EVENTS.iter().map(|entry| entry.name).collect()
}

/// A reason the allowlist itself is malformed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AllowlistError {
    /// Two entries claim the same name, so `forwarded_event` would silently pick the first.
    #[error("forwarded host event `{event}` is allowlisted more than once")]
    Duplicate {
        /// The repeated name.
        event: &'static str,
    },
    /// A waterfall entry declares resource readers, which its wire frame cannot carry.
    #[error("forwarded host event `{event}` is a waterfall and cannot declare resource readers")]
    WaterfallWithResources {
        /// The offending name.
        event: &'static str,
    },
}

/// Checks the properties upstream's `satisfies` clause enforces at compile time.
///
/// Called from a test rather than at startup: the allowlist is a `const`, so a mistake in it is a
/// programming error that must fail the build's test suite, not a deployment condition a running
/// process should report and then serve anyway.
///
/// # Errors
///
/// Returns [`AllowlistError`] for a duplicate name or a waterfall entry with resource readers.
pub fn validate_allowlist() -> Result<(), AllowlistError> {
    let mut seen: Vec<&'static str> = Vec::with_capacity(FORWARDED_EVENTS.len());
    for entry in FORWARDED_EVENTS {
        if seen.contains(&entry.name) {
            return Err(AllowlistError::Duplicate { event: entry.name });
        }
        if entry.delivery == Delivery::Waterfall && !entry.readers.is_empty() {
            return Err(AllowlistError::WaterfallWithResources { event: entry.name });
        }
        seen.push(entry.name);
    }
    Ok(())
}

/// One resource a forwarded notification touches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    kind: &'static str,
    id: Option<String>,
}

impl Resource {
    /// The built-in resource a notification with no readers touches.
    #[must_use]
    pub fn global() -> Self {
        Self {
            kind: GLOBAL_RESOURCE_KIND,
            id: None,
        }
    }

    /// A named resource. An empty id is not a resource and is refused by [`resources_of`].
    #[must_use]
    pub fn new(kind: &'static str, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: Some(id.into()),
        }
    }

    /// The resource kind: `session`, `workspace`, or `global`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The identity, or `None` for the global resource.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Whether this is the global resource.
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.id.is_none()
    }

    /// The wire shape: `{"kind": "session", "id": "..."}`, or `{"kind": "global"}`.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        let mut object = Map::new();
        object.insert("kind".to_owned(), Value::String(self.kind.to_owned()));
        if let Some(id) = &self.id {
            object.insert("id".to_owned(), Value::String(id.clone()));
        }
        Value::Object(object)
    }
}

/// A reason a value could not be forwarded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForwardError {
    /// The event is not on the allowlist, so no Client may receive it.
    #[error("forwarded host event `{event}` is not allowlisted")]
    UnknownEvent {
        /// The rejected name.
        event: String,
    },

    /// The event exists but is delivered the other way.
    #[error("forwarded host event `{event}` is not a {expected} event")]
    WrongDelivery {
        /// The rejected name.
        event: String,
        /// The delivery the caller needed.
        expected: &'static str,
    },

    /// One positional argument would not survive the JSON round trip.
    ///
    /// Upstream refuses the whole event rather than dropping the argument: a Client that rendered
    /// a notification with a missing argument would show a lie, and one that received a truncated
    /// value would compute from it.
    #[error("forwarded host event `{event}` argument {index} is not lossless JSON data")]
    NotLossless {
        /// The event whose argument was rejected.
        event: String,
        /// Zero-based position of the offending argument.
        index: usize,
    },

    /// A reader could not name a non-empty resource id.
    #[error("forwarded host event `{event}` names no valid resources")]
    InvalidResources {
        /// The event whose resources could not be resolved.
        event: String,
    },

    /// A scoped waterfall request did not carry the carrier Agent.
    ///
    /// Upstream throws a `TypeError` here rather than delegating: silently falling through to the
    /// next Host listener would apply an answer nobody authorized.
    #[error("forwarded scoped event `{event}` must carry its Agent directly")]
    MissingAgent {
        /// The event whose request was refused.
        event: String,
    },
}

/// Which resources one forwarded notification touches.
///
/// Upstream's `eventResources`. Three outcomes, and the third is the important one:
///
/// | case | result |
/// |---|---|
/// | entry declares no readers | the `global` resource alone |
/// | every reader names a non-empty id | the touched resources |
/// | a reader returns nothing, or an empty id | invalid — the notification reaches no non-owner |
///
/// Invalid is not the same as global. Treating it as global would deliver a Session-scoped
/// notification to a Peer with no access to that Session, which is the one mistake this function
/// exists to make impossible.
///
/// # Errors
///
/// Returns [`ForwardError::UnknownEvent`] for an event off the allowlist and
/// [`ForwardError::InvalidResources`] for a reader that cannot name its resource.
pub fn resources_of(event: &str, args: &[Value]) -> Result<Vec<Resource>, ForwardError> {
    let entry = forwarded_event(event).ok_or_else(|| ForwardError::UnknownEvent {
        event: event.to_owned(),
    })?;
    if entry.readers.is_empty() {
        return Ok(vec![Resource::global()]);
    }
    let mut resources = Vec::with_capacity(entry.readers.len());
    for (kind, reader) in entry.readers {
        let id = reader(args).ok_or_else(|| ForwardError::InvalidResources {
            event: event.to_owned(),
        })?;
        resources.push(Resource::new(kind, id));
    }
    Ok(resources)
}

/// Reads a non-empty id out of a value, or `None`.
///
/// Empty is rejected as well as absent: an empty resource id names nothing, and a Peer authorized
/// for "no Session" is authorized for every Session.
fn id_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// `api-session/activity`, `/error`, `/removed`, `/status`: the first argument is the Session id.
fn session_id_argument(args: &[Value]) -> Option<String> {
    id_of(args.first()?)
}

/// `api-session/added`: the first argument is a summary whose `sessionId` field is the id.
fn session_summary_argument(args: &[Value]) -> Option<String> {
    args.first()?.get("sessionId").and_then(id_of)
}

/// Checks one notification's arguments for lossless JSON.
///
/// Upstream's `assertJsonArgs`. It walks every argument before enqueueing, because a value that
/// only fails at serialization time would fail once per Client — and after the notification was
/// already accepted, so its loss would look like a Client bug rather than a Host one.
///
/// The check is [`is_lossless`](girder_core::is_lossless), which is *stricter* than upstream's
/// `isJsonValue` in exactly one respect: it also refuses an integer outside ±2^53, because this
/// host produces `u64` sequence numbers and revisions that a JavaScript `number` cannot hold. A
/// payload that passes upstream's check and fails this one is one a browser would silently
/// corrupt, so the extra refusal is the point rather than a divergence to apologize for.
///
/// # Errors
///
/// Returns [`ForwardError::UnknownEvent`] for an event off the allowlist and
/// [`ForwardError::NotLossless`] for the first argument that would not survive the round trip.
pub fn validate_args(event: &str, args: &[Value]) -> Result<(), ForwardError> {
    if !is_forwardable(event) {
        return Err(ForwardError::UnknownEvent {
            event: event.to_owned(),
        });
    }
    for (index, arg) in args.iter().enumerate() {
        if !is_lossless(arg) {
            return Err(ForwardError::NotLossless {
                event: event.to_owned(),
                index,
            });
        }
    }
    Ok(())
}

/// One forwarded Host notification: the event name and its positional arguments.
///
/// A frame is immutable and carries no resource identities: they are derived from
/// [`resources_of`] at broadcast time, from the same arguments, so a frame cannot claim resources
/// its payload does not name.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    event: &'static str,
    args: Vec<Value>,
}

impl Frame {
    /// The Host Cordis event name.
    #[must_use]
    pub fn event(&self) -> &'static str {
        self.event
    }

    /// The positional arguments, in dispatch order.
    #[must_use]
    pub fn args(&self) -> &[Value] {
        &self.args
    }

    /// The wire shape upstream's `RemoteEventEmitFrame` uses.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        let mut object = Map::new();
        object.insert("type".to_owned(), Value::String("emit".to_owned()));
        object.insert("event".to_owned(), Value::String(self.event.to_owned()));
        object.insert("args".to_owned(), Value::Array(self.args.clone()));
        Value::Object(object)
    }
}

/// How a Client answered a forwarded waterfall.
///
/// Upstream carries this over *two* callbacks: `resolve({kind:'result'|'next'})` and
/// `reject(reason)`. One Rust channel with three variants is the same algebra with the difference
/// between "the Client declined" and "the stream died" kept visible — collapsing them would make a
/// transport failure look like a deliberate delegation, and the Host listener would then run its
/// next layer as though a human had chosen to.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The Client answered with a value, which becomes the waterfall's result.
    Result(Value),

    /// The Client declined, so the Host calls the next listener in the chain.
    Next,

    /// The invocation was abandoned: its Agent Context was released, the stream was cancelled, or
    /// the transport failed. The Host listener must not be resumed.
    Rejected {
        /// Why, when a reason was available.
        reason: Option<String>,
    },
}

impl Outcome {
    /// Whether this outcome delegates to the next Host listener.
    #[must_use]
    pub fn is_next(&self) -> bool {
        matches!(self, Self::Next)
    }

    /// Whether this outcome abandons the invocation.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }

    /// The answered value, when there is one.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        match self {
            Self::Result(value) => Some(value),
            Self::Next | Self::Rejected { .. } => None,
        }
    }
}

/// One Agent-scoped waterfall invocation waiting for a Client answer.
///
/// The slot owns its reply channel, and [`WaterfallSlot::settle`] consumes it. That is the
/// type-level form of upstream's "a chain advances monotonically": a second answer to the same
/// invocation cannot be sent, because there is nothing left to send it with.
#[derive(Debug)]
pub struct WaterfallSlot {
    event: &'static str,
    agent_id: String,
    request: Value,
    reply: oneshot::Sender<Outcome>,
}

impl PartialEq for WaterfallSlot {
    /// Compares the facts a consumer can see, not the reply channel.
    ///
    /// The channel has no identity worth comparing -- two slots carrying the same invocation are the same fact even
    /// though their senders are different objects -- and a derived `PartialEq` could not have compiled.
    fn eq(&self, other: &Self) -> bool {
        self.event == other.event
            && self.agent_id == other.agent_id
            && self.request == other.request
    }
}

impl WaterfallSlot {
    /// The Host Cordis event name.
    #[must_use]
    pub fn event(&self) -> &'static str {
        self.event
    }

    /// Identity of the Agent that carries the scoped invocation.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The projected request: the caller's JSON fields with `agent` and `signal` removed.
    #[must_use]
    pub fn request(&self) -> &Value {
        &self.request
    }

    /// The wire shape upstream's `RemoteEventWaterfallFrame` uses.
    ///
    /// `event_id` is minted by the transport, not here: upstream's Gateway assigns transport ids
    /// so a reconnecting Client cannot collide with an id this layer chose.
    #[must_use]
    pub fn to_wire(&self, event_id: &str) -> Value {
        let mut object = Map::new();
        object.insert("type".to_owned(), Value::String("waterfall".to_owned()));
        object.insert("event".to_owned(), Value::String(self.event.to_owned()));
        object.insert("eventId".to_owned(), Value::String(event_id.to_owned()));
        object.insert("agentId".to_owned(), Value::String(self.agent_id.clone()));
        object.insert("request".to_owned(), self.request.clone());
        Value::Object(object)
    }

    /// Settles the invocation, returning whether anyone was still waiting.
    ///
    /// `false` means the Host listener gave up first — the queue ended, or its Agent Context was
    /// released — which is not an error for the *Client*, only a fact the caller may log.
    #[must_use]
    pub fn settle(self, outcome: Outcome) -> bool {
        self.reply.send(outcome).is_ok()
    }

    /// Answers with a value.
    #[must_use]
    pub fn resolve(self, value: Value) -> bool {
        self.settle(Outcome::Result(value))
    }

    /// Declines, delegating to the next Host listener.
    #[must_use]
    pub fn delegate(self) -> bool {
        self.settle(Outcome::Next)
    }

    /// Abandons the invocation with a reason.
    #[must_use]
    pub fn reject(self, reason: Option<String>) -> bool {
        self.settle(Outcome::Rejected { reason })
    }
}

/// One item a Client's event stream may yield.

#[derive(Debug, PartialEq)]
pub enum Dispatch {
    /// A one-way notification.
    Emit(Frame),
    /// A pending waterfall the Client may answer.
    Waterfall(WaterfallSlot),
}

impl Dispatch {
    /// The Host Cordis event name.
    #[must_use]
    pub fn event(&self) -> &'static str {
        match self {
            Self::Emit(frame) => frame.event(),
            Self::Waterfall(slot) => slot.event(),
        }
    }

    /// Whether this is a one-way notification.
    #[must_use]
    pub fn is_emit(&self) -> bool {
        matches!(self, Self::Emit(_))
    }

    /// Whether this is a pending waterfall.
    #[must_use]
    pub fn is_waterfall(&self) -> bool {
        matches!(self, Self::Waterfall(_))
    }

    /// Takes the notification out, if this is one.
    #[must_use]
    pub fn into_emit(self) -> Option<Frame> {
        match self {
            Self::Emit(frame) => Some(frame),
            Self::Waterfall(_) => None,
        }
    }

    /// Takes the waterfall slot out, if this is one.
    #[must_use]
    pub fn into_waterfall(self) -> Option<WaterfallSlot> {
        match self {
            Self::Waterfall(slot) => Some(slot),
            Self::Emit(_) => None,
        }
    }
}

/// The result of offering one notification to a queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    /// It was accepted.
    Accepted,
    /// The queue already ended, so nothing was accepted.
    ///
    /// Not an error, and not a dropped notification to report: a listener that outlives the stream
    /// it feeds is the normal consequence of a Client disconnecting, and a fresh Client attaches
    /// new listeners. Upstream returns `false` here for the same reason.
    Ended,
}

/// One Client stream's queue: synchronous pushes in, asynchronous pulls out.
///
/// Upstream's `RemoteEventQueue`. One instance per Client stream, never shared: two Clients that
/// shared a queue would each receive a random half of the notifications, because a queue hands each
/// item to exactly one consumer.
#[derive(Debug)]
pub struct EventQueue {
    inner: Mutex<Inner>,
    ready: Notify,
}

#[derive(Debug, Default)]
struct Inner {
    buffered: VecDeque<Dispatch>,
    ended: bool,
    reason: Option<String>,
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl EventQueue {
    /// Creates an empty queue that has not ended.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            ready: Notify::new(),
        }
    }

    /// Cheap projection for a `Debug` render; exposed so a caller can log backlog depth.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // No user code runs under this lock — a rejected slot's `send` only moves a value — so a
        // panic elsewhere cannot have left the queue half-updated. Propagating the poison would
        // turn an unrelated panic into a permanent outage of the stream.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the oldest item, if one is waiting.
    ///
    /// The consumer side of [`Self::push`]. A queue that could only be filled would be a queue whose contents nobody
    /// could read, and a caller asserting on what a stream produced needs to take items out one at a time.
    pub fn try_pop_emit(&self) -> Option<Dispatch> {
        let mut inner = self.lock();
        if inner.buffered.is_empty() {
            return None;
        }
        inner.buffered.pop_front()
    }

    /// How many items are waiting for the consumer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().buffered.len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the queue has ended.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.lock().ended
    }

    /// Why it ended, when a reason was recorded.
    #[must_use]
    pub fn end_reason(&self) -> Option<String> {
        self.lock().reason.clone()
    }

    /// Offers one notification.
    ///
    /// Validates before enqueueing, so a queue never holds something it would refuse to send: an
    /// event off the allowlist, an argument that is not lossless JSON, or a waterfall name offered
    /// as a notification.
    ///
    /// # Errors
    ///
    /// Returns [`ForwardError`] for a rejected event or argument.
    pub fn push(&self, event: &str, args: Vec<Value>) -> Result<Push, ForwardError> {
        let entry = forwarded_event(event).ok_or_else(|| ForwardError::UnknownEvent {
            event: event.to_owned(),
        })?;
        if entry.delivery != Delivery::Emit {
            return Err(ForwardError::WrongDelivery {
                event: event.to_owned(),
                expected: Delivery::Emit.as_str(),
            });
        }
        validate_args(event, &args)?;
        Ok(self.push_dispatch(Dispatch::Emit(Frame {
            event: entry.name,
            args,
        })))
    }

    /// Ends the stream, rejecting every invocation still buffered.
    ///
    /// Two different fates, both deliberate. A buffered *notification* is discarded, because
    /// upstream ends the stream exactly when the Client's carrier is gone or revoked and "a revoked
    /// Peer receives none of them" is the requirement — delivering the backlog would hand a revoked
    /// Client the events it was just cut off from. A buffered *invocation* is rejected rather than
    /// discarded, because its Host listener is parked on an answer: dropping the slot would leave
    /// the waterfall unresolved, and the Host would hang instead of failing.
    ///
    /// Idempotent: the first call's reason wins, and a later call changes nothing.
    pub fn end(&self, reason: Option<String>) {
        // Kept because the reason is both recorded on the queue and handed to every rejected slot, and the first of those
        // moves it.
        let for_slots = reason.clone();
        let rejected: Vec<WaterfallSlot> = {
            let mut inner = self.lock();
            if inner.ended {
                return;
            }
            inner.ended = true;
            inner.reason = reason;
            inner
                .buffered
                .drain(..)
                .filter_map(Dispatch::into_waterfall)
                .collect()
        };
        // Outside the lock: `settle` runs no user code today, but a future reply payload could, and
        // a queue's mutex must never be held across code the caller controls.
        for slot in rejected {
            // The reply cannot fail: the ticket's receiver is alive for as long as the Host
            // listener is parked on it, and if it is not, the ticket already reads a closed channel
            // as a rejection.
            let _ = slot.reject(for_slots.clone());
        }
        // `notify_waiters` rather than `notify_one`: every waiter has to learn about the end, not
        // just the next one in line.
        self.ready.notify_waiters();
    }

    /// Ends the stream for a source-level reason: cancellation, or an unexpected end.
    ///
    /// Upstream's `remoteEventSourceEndReason`: a cancelled source reports *its own* reason, because
    /// that reason is the one the Client's pending waterfalls must be rejected with; a source that
    /// ended without being cancelled reports a fixed message, because nothing else explains it.
    pub fn end_from(&self, end: &SourceEnd) {
        self.end(end.reason());
    }

    /// Waits for the next item, or `None` once the queue has ended.
    ///
    /// `None` is the end of the stream, not a lull: a live Client's stream is idle most of the time
    /// and nothing ends it but [`EventQueue::end`]. A consumer that treated the first quiet moment
    /// as terminal would disconnect at once.
    ///
    /// Named `recv` rather than `next` so it does not read as [`Iterator::next`]: this is an
    /// asynchronous pull, and an iterator that blocked would be a different primitive.
    ///
    /// Takes `&self` although a queue hands each item to exactly one consumer, for the same reason
    /// [`BoundedQueue`](girder_core::BoundedQueue) lets a clone receive: two concurrent calls are
    /// memory-safe and simply split the stream between them. A design that needs one consumer owns
    /// one handle and passes [`EventQueue`] by reference to the producers.
    pub async fn recv(&self) -> Option<Dispatch> {
        loop {
            // Registered and enabled before the queue is inspected, so a push that lands between
            // the check and the await cannot be lost: `Notify` stores at most one permit for a
            // waiter that has not arrived yet.
            let mut waiting = std::pin::pin!(self.ready.notified());
            waiting.as_mut().enable();

            {
                let mut inner = self.lock();
                if let Some(item) = inner.buffered.pop_front() {
                    return Some(item);
                }
                if inner.ended {
                    return None;
                }
            }

            waiting.await;
        }
    }

    /// Enqueues a dispatch, reporting whether the queue had already ended.
    fn push_dispatch(&self, dispatch: Dispatch) -> Push {
        {
            let mut inner = self.lock();
            if inner.ended {
                // The dispatch is dropped here. A waterfall slot dropped this way leaves its
                // receiver with a closed channel, which `WaterfallTicket::outcome` reports as a
                // rejection rather than a hang.
                return Push::Ended;
            }
            inner.buffered.push_back(dispatch);
        }
        // Exactly one item is available, so wake exactly one consumer: `notify_waiters` would be a
        // thundering herd on a queue that has one legitimate reader.
        self.ready.notify_one();
        Push::Accepted
    }

    /// Enqueues a waterfall slot, or hands it back when the queue has ended.
    fn push_slot(&self, slot: WaterfallSlot) -> Result<(), WaterfallSlot> {
        {
            let mut inner = self.lock();
            if inner.ended {
                return Err(slot);
            }
            inner.buffered.push_back(Dispatch::Waterfall(slot));
        }
        self.ready.notify_one();
        Ok(())
    }
}

/// Why a forwarded-event source stopped.
///
/// Upstream's `remoteEventSourceEndReason`. The distinction matters to a pending waterfall: a
/// cancellation's reason is what the Client's parked listener must be rejected with, while an
/// unexpected end has no reason to pass on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceEnd {
    /// The source was cancelled; the reason is whatever the cancellation carried.
    Cancelled(Option<String>),
    /// The source ended without being cancelled.
    Unexpected,
}

impl SourceEnd {
    /// The message to reject pending invocations with, if there is one.
    ///
    /// A bare cancellation with no reason yields `None` rather than an invented message: a caller
    /// that cancelled without explaining itself did not say why, and fabricating a reason would put
    /// words in its mouth in the Client's error display.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Cancelled(reason) => reason.clone(),
            Self::Unexpected => Some(UNEXPECTED_SOURCE_END.to_owned()),
        }
    }

    /// Whether this was a cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }
}

/// The message an unexpected end reports.
pub const UNEXPECTED_SOURCE_END: &str = "api-remotes: forwarded Remote event source ended";

/// Classifies how a source stopped.
///
/// `cancelled` is whether the shared stream signal was aborted; `reason` is that signal's reason,
/// when it had one. Upstream reads both off one `AbortSignal`; they are separate parameters here
/// because this crate's cancellation is a [`tokio_util::sync::CancellationToken`] plus an optional
/// message, and collapsing them into one value at the call site would hide which of the two the
/// caller actually had.
#[must_use]
pub fn source_end_reason(cancelled: bool, reason: Option<String>) -> SourceEnd {
    if cancelled {
        SourceEnd::Cancelled(reason)
    } else {
        SourceEnd::Unexpected
    }
}

/// A projected waterfall request: the Agent that carries it, and the JSON the Client may see.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedRequest {
    agent_id: String,
    request: Value,
}

impl ProjectedRequest {
    /// Identity of the Agent that carries the invocation.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The JSON fields a Client may see: the caller's, minus `agent` and `signal`.
    #[must_use]
    pub fn request(&self) -> &Value {
        &self.request
    }
}

/// The field a scoped waterfall request must carry its Agent in.
pub const AGENT_FIELD: &str = "agent";

/// The field a scoped waterfall request carries its cancellation in, and which is never forwarded.
///
/// Upstream strips it because it is an `AbortSignal` rather than JSON data. This build strips it
/// for the same reason a wire protocol would: it names a transport object, and a Client that
/// received it would receive something it could not use.
pub const SIGNAL_FIELD: &str = "signal";

/// Projects one scoped waterfall request into the JSON a Client may see.
///
/// Upstream's `projectRemoteEventRequest`. Four rules, each of which rejects rather than repairs:
///
/// 1. The request must be a plain object.
/// 2. It must carry `agent`, and that must be the carrier identity the listener was invoked with.
///    Upstream compares object identity against `carrierKeyOf(this)`; with no live Agent objects
///    here, the identity is the Agent id.
/// 3. Every remaining field must be lossless JSON.
/// 4. `agent` and `signal` are removed from the projection.
///
/// # Errors
///
/// Returns [`ForwardError::MissingAgent`] when `agent` is absent, empty, or not the expected
/// carrier, and [`ForwardError::NotLossless`] when a field would not survive the JSON round trip.
pub fn project_request(
    event: &str,
    carrier: &str,
    request: &Value,
) -> Result<ProjectedRequest, ForwardError> {
    let object = request
        .as_object()
        .ok_or_else(|| ForwardError::MissingAgent {
            event: event.to_owned(),
        })?;
    let agent_id =
        object
            .get(AGENT_FIELD)
            .and_then(id_of)
            .ok_or_else(|| ForwardError::MissingAgent {
                event: event.to_owned(),
            })?;
    if agent_id != carrier {
        return Err(ForwardError::MissingAgent {
            event: event.to_owned(),
        });
    }
    let mut projected = Map::new();
    for (index, (key, value)) in object.iter().enumerate() {
        if key == AGENT_FIELD || key == SIGNAL_FIELD {
            continue;
        }
        if !is_lossless(value) {
            // Upstream reports the offending *parameter*, not a field path; the index keeps the
            // message faithful to that without inventing a JSON-pointer feature.
            return Err(ForwardError::NotLossless {
                event: event.to_owned(),
                index,
            });
        }
        projected.insert(key.clone(), value.clone());
    }
    Ok(ProjectedRequest {
        agent_id,
        request: Value::Object(projected),
    })
}

/// A waterfall handed to a Client, waiting for [`WaterfallTicket::outcome`].
#[derive(Debug)]
pub struct WaterfallTicket {
    event: &'static str,
    receiver: oneshot::Receiver<Outcome>,
}

impl WaterfallTicket {
    /// The Host Cordis event name.
    #[must_use]
    pub fn event(&self) -> &'static str {
        self.event
    }

    /// Waits for the Client's answer.
    ///
    /// A closed channel — the slot was dropped without settling — becomes
    /// [`Outcome::Rejected`] rather than an error: from the Host listener's point of view "nobody
    /// answered and nobody is going to" is exactly the abandonment `Rejected` describes, and a
    /// caller forced to handle a third case would be handling the same case twice.
    #[must_use]
    pub async fn outcome(self) -> Outcome {
        match self.receiver.await {
            Ok(outcome) => outcome,
            Err(_) => Outcome::Rejected {
                reason: Some(ABANDONED_INVOCATION.to_owned()),
            },
        }
    }
}

/// The reason a ticket reports when its slot vanished without settling.
pub const ABANDONED_INVOCATION: &str = "api-remotes: forwarded Remote event invocation was dropped";

/// Builds a ticket that is already settled, without touching any queue.
fn settled(event: &'static str, outcome: Outcome) -> WaterfallTicket {
    let (reply, receiver) = oneshot::channel();
    // The receiver is returned in the same expression, so it cannot have been dropped first.
    let _ = reply.send(outcome);
    WaterfallTicket { event, receiver }
}

/// Bridges one Agent-scoped waterfall listener through a Client stream.
///
/// Upstream's `forwardWaterfall` plus the `waterfall` arm of its listener body, in the order the
/// listener applies them:
///
/// | condition | result |
/// |---|---|
/// | event is not allowlisted | [`ForwardError::UnknownEvent`] |
/// | event is an `emit` entry | [`ForwardError::WrongDelivery`] |
/// | no carrier Agent | [`Outcome::Next`], and nothing is queued |
/// | request does not carry the carrier Agent | [`ForwardError::MissingAgent`] |
/// | queue already ended | [`Outcome::Next`], so the Host chain continues |
/// | otherwise | the invocation is queued, and the ticket waits for the Client |
///
/// The "no carrier" and "queue ended" cases both delegate to the next Host listener rather than
/// failing, and that is not a convenience: upstream's listener runs inside a waterfall, where
/// returning without calling `next()` would *consume* the chain and change Host behaviour merely
/// because no Client was attached.
///
/// # Errors
///
/// Returns [`ForwardError`] for a non-waterfall event or a request without its carrier Agent.
pub fn forward_waterfall(
    queue: &EventQueue,
    event: &str,
    carrier: Option<&str>,
    request: &Value,
) -> Result<WaterfallTicket, ForwardError> {
    let entry = forwarded_event(event).ok_or_else(|| ForwardError::UnknownEvent {
        event: event.to_owned(),
    })?;
    if entry.delivery != Delivery::Waterfall {
        return Err(ForwardError::WrongDelivery {
            event: event.to_owned(),
            expected: Delivery::Waterfall.as_str(),
        });
    }
    let Some(carrier) = carrier else {
        return Ok(settled(entry.name, Outcome::Next));
    };
    let projected = project_request(event, carrier, request)?;
    let (reply, receiver) = oneshot::channel();
    let slot = WaterfallSlot {
        event: entry.name,
        agent_id: projected.agent_id,
        request: projected.request,
        reply,
    };
    match queue.push_slot(slot) {
        Ok(()) => Ok(WaterfallTicket {
            event: entry.name,
            receiver,
        }),
        // The slot was handed back rather than queued, and the receiver built above is dropped with
        // it; the ticket this returns owns its own channel.
        Err(_) => Ok(settled(entry.name, Outcome::Next)),
    }
}
