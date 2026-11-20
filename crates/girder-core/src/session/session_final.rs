//! The incremental session-log contribution for official `DeepSeek` requests.
//!
//! Corresponds to upstream `packages/session/session-log-deepseek` (396 source lines: `index.ts`
//! 291, `types.ts` 105). This was the largest *completely* unported package in `packages/session`
//! after [`crate::session::migration`], [`crate::session::telemetry`], and [`crate::session::extra`]:
//! `grep` over `crates/` found no `dsh_session_log`, no `delivery-accepted` producer (only the
//! migration tables *name* the event type), and no acceptance watermark anywhere.
//!
//! # What the package does
//!
//! An ordinary conversation request to the official `DeepSeek` API can carry one extra field,
//! `dsh_session_log`, describing the part of the session log the server has not acknowledged yet.
//! The contribution is incremental by construction: the accepted prefix is folded out of the log
//! itself (the `session-log-deepseek/delivery-accepted` event), so a restart resends only what a
//! 2xx response may not have covered, without a second store to keep in sync.
//!
//! Three decisions in that package are easy to get subtly wrong, and they are the reason this port
//! exists rather than a thin type alias:
//!
//! 1. **The field is bounded by serialized bytes, not by event count.** A request uploads the
//!    longest pending *prefix* that fits `maxBytes`, and the arithmetic that decides "fits" is exact
//!    — the field's own envelope, one comma per additional event, and the decimal digits of
//!    `throughSeq`. Off by one and a request either exceeds the limit or drops an event it could
//!    have carried. [`fit_prefix`] performs that arithmetic and [`SessionLogExtension`] states it.
//! 2. **The watermark is evidence and is validated as such.** An acceptance event whose format
//!    version is not a safe non-negative integer, whose `throughSeq` is not a safe non-negative
//!    integer, whose `sessionId` is empty, or whose watermark is not strictly *below* the event's
//!    own position, is malformed and refuses the upload rather than silently resetting the cursor
//!    to `-1` and resending the whole session. [`accepted_through`] is that check.
//! 3. **A request either carries the field *and* its acceptance callback, or neither.** An
//!    exclusion predicate suppresses both, which is what stops an auxiliary request (a title
//!    generation, a compaction summary) from claiming an acceptance it never made.
//!
//! # What is ported, and what is deliberately not
//!
//! Ported: the wire projection ([`WireHeader`], [`PendingEvent`], [`wire_event`]), the byte
//! accounting ([`json_bytes`], [`SessionLogExtension::base_field_bytes`], [`fit_prefix`]), the
//! watermark fold ([`AcceptanceFold`], [`accepted_through`]), the exclusion registry
//! ([`Exclusions`]), and the prepare decision itself ([`Uploader::prepare`]) with all five
//! outcomes.
//!
//! Not ported, and deliberately:
//!
//! - **The Cordis service registration** (`ctx.deepseekLlmApiExtensions.register`). This crate has
//!   no request-extension registry, so [`Uploader::prepare`] *returns* the field value and the
//!   caller decides what to do with it. That is also what makes every branch testable without a
//!   context.
//! - **The `WeakMap`-keyed acceptance cache.** [`SessionLogCursor`] is the same cache with the
//!   identity written down: one cursor per session, holding the scanned prefix length and the
//!   watermark. A cursor is data, not hidden state, so a test can assert that a second scan reads
//!   only the appended events.
//! - **The `Volatile` config wrapper and `ctx.logger`.** [`Uploader::set_enabled`] is the volatile
//!   read, and a blocked upload is reported as [`Preparation::Blocked`] carrying the byte count the
//!   logger would have quoted; [`BlockWarning`] is the "log each block once" rule.
//! - **The unconditional `ctx.effect` disposer.** [`Uploader::register_exclusion`] returns a handle
//!   and [`Uploader::remove_exclusion`] takes it back; that is the same contract without a fiber.
//!
//! # Where the shapes came from
//!
//! ```text
//! field   {"version":1,"sessionFormatVersion":4,"session":{…},"afterSeq":41,"throughSeq":43,
//!          "events":[{"type":"user/message","seq":42,"time":9,"data":{…},"surfaceOp":"append"}]}
//! accept  {"type":"session-log-deepseek/delivery-accepted",
//!          "data":{"sessionId":"s1","sessionFormatVersion":4,"throughSeq":43}}
//! ```
//!
//! This file is also compiled by `tests/session_final_gap.rs` through `#[path]`, so every import
//! below goes through the crate root's public re-exports: in that build `crate::` names the test
//! crate, and a `crate::…` path would fail to resolve.

// The crate root has not been wired to this module yet, so in a library build nothing here has a
// caller and `dead_code` would report a whole public surface as waste. The allow says that plainly
// rather than leaving a reader to guess; the test target exercises every item below.
#![allow(dead_code)]

use std::fmt;

use girder_core::{SessionEvent, SurfaceOp, is_surface_eligible_type};
use serde_json::{Map, Value};

/// The request-extension key the field is carried under.
pub const EXTENSION_KEY: &str = "dsh_session_log";

/// The only field version this build writes.
pub const EXTENSION_VERSION: u64 = 1;

/// The log event that records one accepted delivery.
pub const ACCEPTANCE_EVENT_TYPE: &str = "session-log-deepseek/delivery-accepted";

/// The default `maxBytes`: 8 MiB, matching upstream's schema default.
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// `Number.MAX_SAFE_INTEGER`, the largest integer a `JavaScript` peer compares exactly.
///
/// Both the acceptance format version and the watermark are checked against it: a value above it
/// round-trips through a `double` and can compare equal to a neighbour, which would let a watermark
/// advance past an event it never acknowledged.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// The event type whose payload names the delivery watermark.
const SESSION_ID_FIELD: &str = "sessionId";
/// The payload field naming the generation an acceptance was made in.
const FORMAT_VERSION_FIELD: &str = "sessionFormatVersion";
/// The payload field carrying the acknowledged watermark.
const THROUGH_SEQ_FIELD: &str = "throughSeq";

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// Why a configuration was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A zero or absent byte budget admits no event at all, so the field could never be prepared.
    ZeroMaxBytes,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMaxBytes => {
                write!(
                    formatter,
                    "session-log-deepseek: maxBytes must be at least 1"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// The contribution's execution controls.
///
/// `enabled` is the volatile half: upstream reads it *per request*, so a deployment can stop
/// uploading without rebuilding the service. [`Uploader::set_enabled`] is that read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Whether to contribute the field at all.
    pub enabled: bool,
    /// Largest serialized field, in `UTF-8` bytes, that one request may carry.
    pub max_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl Config {
    /// Validates a byte budget.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ZeroMaxBytes`] when `max_bytes` is zero: with no budget no prefix ever
    /// fits, so every request would be blocked forever and the session would never upload.
    pub fn new(enabled: bool, max_bytes: usize) -> Result<Self, ConfigError> {
        if max_bytes == 0 {
            return Err(ConfigError::ZeroMaxBytes);
        }
        Ok(Self { enabled, max_bytes })
    }
}

// ---------------------------------------------------------------------------------------------
// Header projection
// ---------------------------------------------------------------------------------------------

/// The header facts the wire header is projected from.
///
/// A struct of primitives rather than a borrow of the storage header, for the same reason
/// [`crate::SessionFacts`] is: the projection is then a pure function that can be tested without a
/// file, and this module does not have to be wired into the crate root to compile.
///
/// `version` is the raw stored number rather than a [`girder_core::FormatVersion`], because a log
/// written by a newer harness declares a number this build has no variant for — and the request
/// field has to report it, not refuse it. `sessionFormatVersion` is exactly that number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderFacts {
    /// The format generation the log is written in, as stored.
    pub version: u64,
    /// The session identity.
    pub id: String,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at: u64,
    /// The project directory, when the header recorded one.
    pub cwd: Option<String>,
    /// The fork parent, when this session was seeded from another.
    pub parent_session: Option<String>,
    /// Whether this session inherited a prefix from its parent.
    pub is_seeded: bool,
    /// How many leading events came from the parent. Meaningful only when `is_seeded`.
    pub inherited_event_count: u64,
    /// Who created the session when it was not a client, copied verbatim.
    ///
    /// Kept as a string rather than an enum because the storage layer is what restricts it to its
    /// one released value; a second closed enum here would be a second place to update.
    pub origin: Option<String>,
    /// How far below the root agent this session runs.
    pub delegation_depth: u64,
    /// The agent preset the session runs under.
    pub agent_preset: Option<String>,
}

impl HeaderFacts {
    /// A minimal, unseeded header — the shape every ordinary session has.
    #[must_use]
    pub fn new(version: u64, id: impl Into<String>, created_at: u64) -> Self {
        Self {
            version,
            id: id.into(),
            created_at,
            cwd: None,
            parent_session: None,
            is_seeded: false,
            inherited_event_count: 0,
            origin: None,
            delegation_depth: 0,
            agent_preset: None,
        }
    }
}

/// The session header as raw request fields.
///
/// Every field here is a `JSON` primitive on purpose: the request wire is read by a peer that shares
/// no types with this build, so a branded id or an enum would have to be unwrapped somewhere, and
/// doing it once here is what keeps the unwrapping auditable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireHeader {
    /// The format generation, as `sessionFormatVersion` on the field.
    pub version: u64,
    /// The session id.
    pub id: String,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at: u64,
    /// The project directory, when one was recorded.
    pub cwd: Option<String>,
    /// The fork parent, when this session was seeded from another.
    pub parent_session: Option<String>,
    /// The exact inherited prefix length. Absent for an unseeded session.
    pub seed_length: Option<u64>,
    /// Who created the session when it was not a client.
    pub origin: Option<String>,
    /// How far below the root agent this session runs.
    pub delegation_depth: Option<u64>,
    /// The agent preset the session runs under.
    pub agent_preset: Option<String>,
}

impl WireHeader {
    /// Projects the header facts onto the request wire.
    ///
    /// `seedLength` is emitted **only** for a seeded session: an unseeded session has no inherited
    /// prefix, and writing `0` would tell a receiver that a cut exists and is empty. That is the
    /// difference between "this log starts here" and "the first zero events of this log came from
    /// somewhere else", and only one of them is true.
    ///
    /// `delegationDepth` is always emitted here, unlike upstream's "when defined": this build's
    /// storage header makes the key mandatory, so a missing value is not a state a log can be in.
    #[must_use]
    pub fn from_facts(facts: &HeaderFacts) -> Self {
        Self {
            version: facts.version,
            id: facts.id.clone(),
            created_at: facts.created_at,
            cwd: facts.cwd.clone(),
            parent_session: facts.parent_session.clone(),
            seed_length: if facts.is_seeded {
                Some(facts.inherited_event_count)
            } else {
                None
            },
            origin: facts.origin.clone(),
            delegation_depth: Some(facts.delegation_depth),
            agent_preset: facts.agent_preset.clone(),
        }
    }

    /// The header as the `JSON` object the field carries.
    ///
    /// Absent optional fields are omitted rather than written as `null`: a `null` `cwd` and an
    /// absent one mean the same thing to a reader, and only one of them is a value this build
    /// writes.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("version".to_owned(), Value::from(self.version));
        map.insert("id".to_owned(), Value::String(self.id.clone()));
        map.insert("createdAt".to_owned(), Value::from(self.created_at));
        if let Some(cwd) = &self.cwd {
            map.insert("cwd".to_owned(), Value::String(cwd.clone()));
        }
        if let Some(parent) = &self.parent_session {
            map.insert("parentSession".to_owned(), Value::String(parent.clone()));
        }
        if let Some(seed_length) = self.seed_length {
            map.insert("seedLength".to_owned(), Value::from(seed_length));
        }
        if let Some(origin) = &self.origin {
            map.insert("origin".to_owned(), Value::String(origin.clone()));
        }
        if let Some(depth) = self.delegation_depth {
            map.insert("delegationDepth".to_owned(), Value::from(depth));
        }
        if let Some(preset) = &self.agent_preset {
            map.insert("agentPreset".to_owned(), Value::String(preset.clone()));
        }
        Value::Object(map)
    }
}

// ---------------------------------------------------------------------------------------------
// Event projection
// ---------------------------------------------------------------------------------------------

/// How a pending event sits on the model-visible surface.
///
/// The three variants are the three kinds of row a log can hold, and they are separated here because
/// they have *different* wire rules rather than because the shapes differ:
///
/// - a current event carries a typed [`SurfaceOp`];
/// - a log-only event carries none, and is refused if it claims one;
/// - a restored record this build does not model carries `surfaceOp`/`sourceEventSeqs` as opaque
///   `JSON`, which must survive a re-upload unexamined rather than be dropped.
#[derive(Clone, Debug, PartialEq)]
pub enum Placement {
    /// A current event that stands on the surface.
    Surface {
        /// How it entered: at the tail, or shadowing a range.
        op: SurfaceOp,
        /// Earlier log positions it was derived from.
        sources: Vec<u64>,
    },
    /// A current event with no surface presence.
    LogOnly,
    /// A restored record this build does not model, kept opaque.
    Opaque {
        /// Whatever placement the writer recorded, unexamined.
        surface_op: Option<Value>,
        /// Whatever provenance the writer recorded, unexamined.
        sources: Option<Value>,
    },
}

/// One canonical log event on its way to the request wire.
///
/// Carries the placement separately from the payload for the same reason
/// [`girder_core::SurfaceEnvelope`] does: the payload is the event's own data and is uploaded
/// untouched, while the placement is the harness's statement about it.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingEvent {
    /// The event's `type` tag.
    pub type_name: String,
    /// One-based position in the log.
    pub seq: u64,
    /// Wall-clock time in milliseconds since the Unix epoch.
    pub time: u64,
    /// The payload, without its `type` tag.
    pub data: Value,
    /// Whether a reader that does not know this type may skip it.
    pub ignorable: bool,
    /// Where the event stands on the surface.
    pub placement: Placement,
}

impl PendingEvent {
    /// Builds the wire event for a current event at an explicit placement.
    ///
    /// # Errors
    ///
    /// Returns [`EmitError::Unserializable`] when the event's payload cannot be rendered as `JSON`.
    pub fn current(
        event: &SessionEvent,
        seq: u64,
        time: u64,
        placement: Placement,
    ) -> Result<Self, EmitError> {
        Ok(Self {
            type_name: event.type_name().to_owned(),
            seq,
            time,
            data: payload(event, seq)?,
            ignorable: false,
            placement,
        })
    }

    /// Builds the wire event for an event that appends to the surface tail.
    ///
    /// A surface-eligible type gets [`Placement::Surface`] with [`SurfaceOp::Append`] and no
    /// provenance, which is what every ordinary prompt, reply, and tool result is; everything else
    /// is log-only. That covers all but the replacement case, which has to be stated explicitly
    /// because a replacement's provenance is exactly what a reader checks.
    ///
    /// # Errors
    ///
    /// Returns [`EmitError::Unserializable`] when the event's payload cannot be rendered as `JSON`.
    pub fn append(event: &SessionEvent, seq: u64, time: u64) -> Result<Self, EmitError> {
        let placement = if is_surface_eligible_type(event.type_name()) {
            Placement::Surface {
                op: SurfaceOp::Append,
                sources: Vec::new(),
            }
        } else {
            Placement::LogOnly
        };
        Self::current(event, seq, time, placement)
    }

    /// Builds the wire event for a restored record this build does not model.
    ///
    /// Such a record is by definition `ignorable` — a log that carried a *required* unknown event
    /// must not upload at all, because the reader could not reconstruct the session — and its
    /// placement travels opaque.
    #[must_use]
    pub fn opaque(type_name: impl Into<String>, seq: u64, time: u64, data: Value) -> Self {
        Self {
            type_name: type_name.into(),
            seq,
            time,
            data,
            ignorable: true,
            placement: Placement::Opaque {
                surface_op: None,
                sources: None,
            },
        }
    }

    /// The log-only companion for a known non-surface event.
    #[must_use]
    pub fn log_only(type_name: impl Into<String>, seq: u64, time: u64, data: Value) -> Self {
        Self {
            type_name: type_name.into(),
            seq,
            time,
            data,
            ignorable: false,
            placement: Placement::LogOnly,
        }
    }
}

/// The payload of a session event, without its `type` tag.
fn payload(event: &SessionEvent, seq: u64) -> Result<Value, EmitError> {
    let mut value = serde_json::to_value(event).map_err(|_| EmitError::Unserializable { seq })?;
    if let Some(object) = value.as_object_mut() {
        object.remove("type");
    }
    Ok(value)
}

/// Why an event could not be projected onto the request wire.
///
/// Each variant is a log shape the *writer* got wrong rather than something a receiver can repair,
/// so the upload is refused: uploading a field that misstates a placement tells the server the
/// session's surface is different from the one this build folded, and the server has no way to
/// disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmitError {
    /// A surface-eligible event carried no placement. Every one of them must.
    MissingSurfaceOp {
        /// The event's type.
        type_name: String,
        /// The event's position.
        seq: u64,
    },
    /// A non-surface event carried a placement.
    UnexpectedSurfaceOp {
        /// The event's type.
        type_name: String,
        /// The event's position.
        seq: u64,
    },
    /// An event carried provenance but cannot cite any.
    UnexpectedSources {
        /// The event's type.
        type_name: String,
        /// The event's position.
        seq: u64,
    },
    /// An opaque record was marked non-ignorable, so it is a required event this build cannot read.
    OpaqueNotIgnorable {
        /// The event's type.
        type_name: String,
        /// The event's position.
        seq: u64,
    },
    /// The event's payload could not be rendered as `JSON`.
    Unserializable {
        /// The event's position.
        seq: u64,
    },
}

impl fmt::Display for EmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSurfaceOp { type_name, seq } => write!(
                formatter,
                "session-log-deepseek: surface event \"{type_name}\" at seq {seq} carries no surface op"
            ),
            Self::UnexpectedSurfaceOp { type_name, seq } => write!(
                formatter,
                "session-log-deepseek: event \"{type_name}\" at seq {seq} is not surface-eligible and cannot carry a surface op"
            ),
            Self::UnexpectedSources { type_name, seq } => write!(
                formatter,
                "session-log-deepseek: event \"{type_name}\" at seq {seq} cannot cite source events"
            ),
            Self::OpaqueNotIgnorable { type_name, seq } => write!(
                formatter,
                "session-log-deepseek: event \"{type_name}\" at seq {seq} is opaque but not ignorable"
            ),
            Self::Unserializable { seq } => write!(
                formatter,
                "session-log-deepseek: event at seq {seq} cannot be serialized into a dsh_session_log field"
            ),
        }
    }
}

impl std::error::Error for EmitError {}

/// The wire shape of one surface mutation: `'append'`, or a replacement range.
#[must_use]
pub fn wire_surface_op(op: SurfaceOp) -> Value {
    match op {
        SurfaceOp::Append => Value::String("append".to_owned()),
        SurfaceOp::Replace { start, end } => {
            let mut map = Map::new();
            map.insert("op".to_owned(), Value::String("replace".to_owned()));
            map.insert("startSeq".to_owned(), Value::from(start));
            map.insert("endSeq".to_owned(), Value::from(end));
            Value::Object(map)
        }
    }
}

/// Projects one pending event onto the request wire.
///
/// The rules are upstream's, and each one is a way for the server's view to diverge from this
/// build's:
///
/// - **A surface event must state its placement**, and the placement is emitted as `surfaceOp`.
/// - **`assistant/message` never cites sources.** It is the one surface event that is not *derived*
///   from earlier nodes — it is the model's answer — so upstream's type forbids the field, and a
///   value that carried one is refused rather than silently stripped.
/// - **Provenance is emitted only when non-empty.** `[]` and "absent" both say "derived from
///   nothing"; only absent is what upstream writes.
/// - **A log-only event must not claim a placement**, because a receiver would fold it into the
///   conversation surface that the sender did not.
/// - **An opaque record keeps whatever it carried**, unexamined: this build cannot judge a
///   placement whose event type it does not know, and dropping it would rewrite the history the
///   record is evidence of.
///
/// # Errors
///
/// Returns [`EmitError`] for any of the five shapes above that a writer can produce but a reader
/// must not accept.
pub fn wire_event(event: &PendingEvent) -> Result<Value, EmitError> {
    let mut map = Map::new();
    map.insert("type".to_owned(), Value::String(event.type_name.clone()));
    map.insert("seq".to_owned(), Value::from(event.seq));
    map.insert("time".to_owned(), Value::from(event.time));
    map.insert("data".to_owned(), event.data.clone());
    if event.ignorable {
        map.insert("ignorable".to_owned(), Value::Bool(true));
    }

    match &event.placement {
        Placement::Surface { op, sources } => {
            if !is_surface_eligible_type(&event.type_name) {
                return Err(EmitError::UnexpectedSurfaceOp {
                    type_name: event.type_name.clone(),
                    seq: event.seq,
                });
            }
            map.insert("surfaceOp".to_owned(), wire_surface_op(*op));
            if !sources.is_empty() {
                if event.type_name == "assistant/message" {
                    return Err(EmitError::UnexpectedSources {
                        type_name: event.type_name.clone(),
                        seq: event.seq,
                    });
                }
                map.insert(
                    "sourceEventSeqs".to_owned(),
                    Value::Array(sources.iter().map(|seq| Value::from(*seq)).collect()),
                );
            }
        }
        Placement::LogOnly => {
            // A surface-eligible type must state where it stands, or the receiver folds it into a
            // conversation surface this build never built.
            if is_surface_eligible_type(&event.type_name) {
                return Err(EmitError::MissingSurfaceOp {
                    type_name: event.type_name.clone(),
                    seq: event.seq,
                });
            }
            // An unknown *required* type is deliberately still uploaded. Refusing it here would
            // make the acceptance marker (`session-log-deepseek/delivery-accepted`, which this
            // crate has no `SessionEvent` variant for) impossible to forward, and this layer's job
            // is faithful transport, not vocabulary policing: `validate_stored_records` is what
            // refuses a required event the reader cannot fold, *before* it is loaded.
        }
        Placement::Opaque {
            surface_op,
            sources,
        } => {
            if !event.ignorable {
                return Err(EmitError::OpaqueNotIgnorable {
                    type_name: event.type_name.clone(),
                    seq: event.seq,
                });
            }
            if let Some(op) = surface_op {
                map.insert("surfaceOp".to_owned(), op.clone());
            }
            if let Some(sources) = sources {
                map.insert("sourceEventSeqs".to_owned(), sources.clone());
            }
        }
    }

    Ok(Value::Object(map))
}

// ---------------------------------------------------------------------------------------------
// Byte accounting
// ---------------------------------------------------------------------------------------------

/// The `UTF-8` length of one value's `JSON` text, as the request body encodes it.
///
/// `None` stands for upstream's `Infinity`: a value that cannot be serialized fits inside no limit,
/// so the caller treats it as "exceeds every budget" rather than as zero bytes.
#[must_use]
pub fn json_bytes(value: &Value) -> Option<usize> {
    serde_json::to_string(value).ok().map(|text| text.len())
}

/// The number of decimal digits in `value`'s text.
///
/// This is the part of the byte accounting that is easy to miss: `throughSeq` sits *before* the
/// events in the field, and its own digits grow with the session. Computing `1` for every watermark
/// would over-admit a two-thousand-event session by three bytes per request.
#[must_use]
pub const fn decimal_digits(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// The field one request carries.
///
/// `afterSeq` can be `-1`: a session whose log holds no acceptance event has acknowledged nothing,
/// and `-1` is how that is stated without a sentinel meaning "unknown".
#[derive(Clone, Debug, PartialEq)]
pub struct SessionLogExtension {
    /// The field version; always [`EXTENSION_VERSION`].
    pub version: u64,
    /// The session format generation the events are written in.
    pub session_format_version: u64,
    /// The session header.
    pub session: WireHeader,
    /// The highest acknowledged sequence, or `-1`.
    pub after_seq: i64,
    /// The highest sequence this field represents.
    pub through_seq: u64,
    /// The wired events, contiguous and in log order.
    pub events: Vec<Value>,
}

impl SessionLogExtension {
    /// The field as the `JSON` object the request carries.
    #[must_use]
    pub fn to_value(&self) -> Value {
        self.render(self.through_seq, &self.events)
    }

    /// The field with a substituted watermark and event list.
    fn render(&self, through_seq: u64, events: &[Value]) -> Value {
        let mut map = Map::new();
        map.insert("version".to_owned(), Value::from(self.version));
        map.insert(
            "sessionFormatVersion".to_owned(),
            Value::from(self.session_format_version),
        );
        map.insert("session".to_owned(), self.session.to_value());
        map.insert("afterSeq".to_owned(), Value::from(self.after_seq));
        map.insert("throughSeq".to_owned(), Value::from(through_seq));
        map.insert("events".to_owned(), Value::Array(events.to_vec()));
        Value::Object(map)
    }

    /// The field's byte length *before* any event and before the watermark's digits.
    ///
    /// This is the anchor [`fit_prefix`] adds to, and the reason it exists is that the field cannot
    /// be measured by serializing each part and summing: the envelope's own keys, the brackets, and
    /// the `afterSeq` digits are all part of the budget and none of them belong to an event. The
    /// probe writes `throughSeq` as `0` — one digit — and subtracts it.
    #[must_use]
    pub fn base_field_bytes(&self) -> Option<usize> {
        json_bytes(&self.render(0, &[])).map(|bytes| bytes.saturating_sub(1))
    }
}

/// The longest pending prefix that fits one field.
#[derive(Clone, Debug, PartialEq)]
pub struct PrefixFit {
    /// The wired events, in log order.
    pub events: Vec<Value>,
    /// The highest sequence the prefix covers.
    pub through_seq: u64,
    /// The complete byte length of the field this prefix produces, watermark digits included.
    pub field_bytes: usize,
}

/// A prefix that could not be admitted, and what it would have cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedUpload {
    /// The first event that did not fit.
    pub seq: u64,
    /// The bytes the field would have needed, or `None` when the event cannot be serialized.
    pub bytes: Option<usize>,
}

/// The outcome of measuring a pending tail against a byte budget.
#[derive(Clone, Debug, PartialEq)]
pub enum FitOutcome {
    /// Nothing was pending, so there is nothing to contribute.
    Empty,
    /// A prefix fits; upload it.
    Fit(PrefixFit),
    /// Not even the first event fits, so the whole session stays where it is.
    Blocked(BlockedUpload),
}

/// Measures the longest pending prefix that fits `max_bytes`.
///
/// The arithmetic mirrors upstream exactly, and it is exact rather than approximate: the field's
/// length is the envelope plus one comma per event after the first, plus each event's own text, plus
/// the decimal digits of the watermark. Each candidate is measured *before* it is admitted, and the
/// first candidate that does not fit ends the scan — later events are never smaller than the one
/// that stopped it in the only case that matters (a single oversized event), and reordering is not
/// allowed because a prefix has to stay contiguous for the server to fold it.
///
/// # Errors
///
/// Returns [`EmitError`] when an event cannot be projected onto the wire; a field that misstates one
/// event is not a field this build will send.
pub fn fit_prefix(
    base_bytes: usize,
    pending: &[PendingEvent],
    max_bytes: usize,
) -> Result<FitOutcome, EmitError> {
    if pending.is_empty() {
        return Ok(FitOutcome::Empty);
    }
    let mut bytes = base_bytes;
    let mut events: Vec<Value> = Vec::new();
    let mut last_seq: Option<u64> = None;

    for event in pending {
        let wire = wire_event(event)?;
        let comma = usize::from(!events.is_empty());
        let total = match json_bytes(&wire) {
            Some(size) => bytes
                .checked_add(comma)
                .and_then(|sum| sum.checked_add(size)),
            None => None,
        };
        let candidate = total.and_then(|sum| sum.checked_add(decimal_digits(event.seq)));
        let Some(candidate) = candidate else {
            break;
        };
        if candidate > max_bytes {
            // A prefix has to stay contiguous, so this event ends the scan: skipping it and
            // admitting a later one would describe a log with a hole in it. Whatever did fit is
            // still uploaded, which is what keeps a large event from blocking the session forever.
            break;
        }
        // `candidate` was built from `total`, so `total` is `Some` here.
        if let Some(total) = total {
            bytes = total;
            last_seq = Some(event.seq);
            events.push(wire);
        }
    }

    let Some(through_seq) = last_seq else {
        // Nothing fit, so the session stays where it is and the caller is told what blocked it.
        let first = pending.first().map_or(0, |event| event.seq);
        let bytes = pending
            .first()
            .and_then(|event| wire_event(event).ok())
            .and_then(|wire| json_bytes(&wire))
            .and_then(|size| {
                base_bytes
                    .checked_add(size)
                    .and_then(|sum| sum.checked_add(decimal_digits(first)))
            });
        return Ok(FitOutcome::Blocked(BlockedUpload { seq: first, bytes }));
    };
    Ok(FitOutcome::Fit(PrefixFit {
        events,
        through_seq,
        field_bytes: bytes.saturating_add(decimal_digits(through_seq)),
    }))
}

// ---------------------------------------------------------------------------------------------
// Acceptance watermark
// ---------------------------------------------------------------------------------------------

/// Why an acceptance event could not be read as evidence.
///
/// Every variant refuses the upload. The alternative — treating a malformed watermark as `-1` —
/// resends an entire session on every request, and treating it as "as far as the log goes" claims
/// acknowledgements the server never made. Refusing is the only option that neither wastes the
/// budget nor lies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatermarkError {
    /// A cached cursor claims more events than the log now holds, so the log lost history.
    MissingEvent {
        /// The index that could not be read.
        index: usize,
        /// The length the cursor had captured.
        captured_length: usize,
    },
    /// The recorded format version is not a safe non-negative integer.
    MalformedFormatVersion {
        /// The acceptance event's own position.
        seq: u64,
    },
    /// The recorded watermark is not a safe non-negative integer, or an id is missing.
    MalformedWatermark {
        /// The acceptance event's own position.
        seq: u64,
    },
    /// The watermark does not precede the event that records it.
    WatermarkNotBeforeEvent {
        /// The acceptance event's own position.
        seq: u64,
    },
    /// The log's positions are not contiguous, so a field would describe an event it omits.
    NonContiguous {
        /// The position the walk expected.
        expected: u64,
        /// The position it found.
        actual: u64,
    },
}

impl fmt::Display for WatermarkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEvent {
                index,
                captured_length,
            } => write!(
                formatter,
                "session-log-deepseek: missing event {index} below captured length {captured_length}"
            ),
            Self::MalformedFormatVersion { seq } => write!(
                formatter,
                "session-log-deepseek: malformed acceptance format version at seq {seq}"
            ),
            Self::MalformedWatermark { seq } => write!(
                formatter,
                "session-log-deepseek: malformed acceptance watermark at seq {seq}"
            ),
            Self::WatermarkNotBeforeEvent { seq } => write!(
                formatter,
                "session-log-deepseek: acceptance watermark at seq {seq} does not precede it"
            ),
            Self::NonContiguous { expected, actual } => write!(
                formatter,
                "session-log-deepseek: expected event {expected} but the log continues at {actual}"
            ),
        }
    }
}

impl std::error::Error for WatermarkError {}

/// The acceptance fold's cache: how far it scanned, and what it found.
///
/// Upstream keeps this in a `WeakMap` keyed by session, which is invisible state. Writing it down as
/// a value is what lets a caller keep one per session explicitly and lets a test assert that a
/// second scan reads only the appended events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptanceFold {
    /// How many leading events have already been folded.
    pub scanned_events: usize,
    /// The highest acknowledged sequence, or `-1`.
    pub through_seq: i64,
}

impl Default for AcceptanceFold {
    fn default() -> Self {
        Self::new()
    }
}

impl AcceptanceFold {
    /// A fold that has read nothing and acknowledges nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            scanned_events: 0,
            through_seq: -1,
        }
    }

    /// Whether the fold has acknowledged anything.
    #[must_use]
    pub const fn has_acknowledgement(&self) -> bool {
        self.through_seq >= 0
    }
}

/// Folds one acceptance payload into `through` when it belongs to this session and generation.
///
/// The checks run in upstream's order, and the order is load-bearing: format version, then the
/// format of the watermark and the id, then the lineage filters. A malformed watermark is therefore
/// reported even when it belongs to another session, which is what makes a corrupt log visible
/// instead of silently ignored.
fn fold_acceptance(
    event: &PendingEvent,
    header: &HeaderFacts,
    through: i64,
) -> Result<i64, WatermarkError> {
    let Some(object) = event.data.as_object() else {
        return Err(WatermarkError::MalformedWatermark { seq: event.seq });
    };

    // `?? 0`: an absent generation means the version-zero lane, which is a real historical value
    // rather than an error.
    let format_version = match object.get(FORMAT_VERSION_FIELD) {
        None => 0,
        Some(value) => match value.as_u64() {
            Some(version) if version <= MAX_SAFE_INTEGER => version,
            _ => {
                return Err(WatermarkError::MalformedFormatVersion { seq: event.seq });
            }
        },
    };
    if format_version != header.version {
        return Ok(through);
    }

    let Some(session_id) = object.get(SESSION_ID_FIELD).and_then(Value::as_str) else {
        return Err(WatermarkError::MalformedWatermark { seq: event.seq });
    };
    let Some(watermark) = object.get(THROUGH_SEQ_FIELD).and_then(Value::as_u64) else {
        return Err(WatermarkError::MalformedWatermark { seq: event.seq });
    };
    if session_id.is_empty() || watermark > MAX_SAFE_INTEGER {
        return Err(WatermarkError::MalformedWatermark { seq: event.seq });
    }
    // A marker cannot acknowledge itself or anything after it: the event that records an
    // acknowledgement is appended *after* the events it acknowledges.
    if watermark >= event.seq {
        return Err(WatermarkError::WatermarkNotBeforeEvent { seq: event.seq });
    }
    if session_id != header.id {
        return Ok(through);
    }

    let watermark = i64::try_from(watermark).unwrap_or(i64::MAX);
    Ok(through.max(watermark))
}

/// The highest sequence this session's log has acknowledged, or `-1`.
///
/// # Errors
///
/// Returns [`WatermarkError`] for a malformed acceptance event, and for a cursor that captured more
/// events than the log still holds — the one case where a *deleted* log prefix would otherwise look
/// like a clean acknowledgement.
pub fn accepted_through(
    events: &[PendingEvent],
    header: &HeaderFacts,
    fold: &mut AcceptanceFold,
) -> Result<i64, WatermarkError> {
    if fold.scanned_events > events.len() {
        return Err(WatermarkError::MissingEvent {
            index: events.len(),
            captured_length: fold.scanned_events,
        });
    }
    let mut through = fold.through_seq;
    // The position the walk expects next. On a rescan this is the last already-folded event, so the
    // seam between the cached prefix and the new tail is checked too; on a fresh scan it is `0`,
    // which is what makes the crate's one-based sequence the only accepted start.
    let mut previous = fold
        .scanned_events
        .checked_sub(1)
        .and_then(|index| events.get(index))
        .map_or(0, |event| event.seq);
    for event in &events[fold.scanned_events..] {
        // A pending tail is a contiguous run of positions: `afterSeq + 1..=throughSeq` is what a
        // receiver folds, so a hole would make the field describe one event it does not carry.
        if event.seq != previous.saturating_add(1) {
            return Err(WatermarkError::NonContiguous {
                expected: previous.saturating_add(1),
                actual: event.seq,
            });
        }
        previous = event.seq;
        if event.type_name != ACCEPTANCE_EVENT_TYPE {
            continue;
        }
        through = fold_acceptance(event, header, through)?;
    }
    fold.scanned_events = events.len();
    fold.through_seq = through;
    Ok(through)
}

/// The event a successful delivery appends.
///
/// Kept as a value rather than appended directly for the same reason the field is returned rather
/// than sent: this module has no session writer. The caller appends [`Self::data`] under
/// [`ACCEPTANCE_EVENT_TYPE`] once the endpoint has answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptanceEvent {
    /// The session the accepted delivery carried.
    pub session_id: String,
    /// The generation it was made in.
    pub session_format_version: u64,
    /// The last event the accepted request included.
    pub through_seq: u64,
}

impl AcceptanceEvent {
    /// The event's payload.
    #[must_use]
    pub fn data(&self) -> Value {
        let mut map = Map::new();
        map.insert(
            SESSION_ID_FIELD.to_owned(),
            Value::String(self.session_id.clone()),
        );
        map.insert(
            FORMAT_VERSION_FIELD.to_owned(),
            Value::from(self.session_format_version),
        );
        map.insert(THROUGH_SEQ_FIELD.to_owned(), Value::from(self.through_seq));
        Value::Object(map)
    }
}

// ---------------------------------------------------------------------------------------------
// Exclusions
// ---------------------------------------------------------------------------------------------

/// Why an exclusion predicate refused to answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExclusionError {
    /// What the predicate reported.
    pub message: String,
}

impl ExclusionError {
    /// Wraps a predicate's failure.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ExclusionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "session-log-deepseek exclusion: {}",
            self.message
        )
    }
}

impl std::error::Error for ExclusionError {}

/// The identity of one upload attempt, as an exclusion predicate sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadRequest<'a> {
    /// The session whose log would be contributed.
    pub session_id: &'a str,
    /// The auxiliary request classification, absent for ordinary conversation requests.
    pub purpose: Option<&'a str>,
}

/// A registered exclusion predicate.
type Predicate = Box<dyn Fn(&UploadRequest<'_>) -> Result<bool, ExclusionError>>;

/// A registered predicate's handle.
///
/// Opaque in practice — the value is only meaningful to the registry that issued it — but the field
/// is public so a caller can keep one in its own struct without a constructor, and so a test can
/// present a handle the registry never issued to prove removal is not a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExclusionHandle(pub u64);

/// The exclusion registry.
///
/// A predicate returns `true` to suppress the upload. Suppression is total — the field *and* its
/// acceptance callback — because a field sent without its callback would be resent forever, and a
/// callback registered without its field would acknowledge events the server never received.
///
/// A predicate that fails fails the *contribution*, not the request: the caller omits the field and
/// reports the error. That is upstream's `registerExclusion` contract, and it is the safe direction
/// — an auxiliary request is better off without a log field than with a guessed one.
#[derive(Default)]
pub struct Exclusions {
    entries: Vec<(ExclusionHandle, Predicate)>,
    next: u64,
}

impl fmt::Debug for Exclusions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Exclusions")
            .field("entries", &self.entries.len())
            .field("next", &self.next)
            .finish()
    }
}

impl Exclusions {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a predicate, in evaluation order.
    pub fn register(
        &mut self,
        exclude: impl Fn(&UploadRequest<'_>) -> Result<bool, ExclusionError> + 'static,
    ) -> ExclusionHandle {
        let handle = ExclusionHandle(self.next);
        self.next += 1;
        self.entries.push((handle, Box::new(exclude)));
        handle
    }

    /// Removes a predicate.
    ///
    /// # Errors
    ///
    /// Returns `false` when the handle was already removed. Upstream's disposer is idempotent too:
    /// calling it twice is not a program error, because a caller may unload a plugin that another
    /// caller already tore down.
    pub fn remove(&mut self, handle: ExclusionHandle) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(entry, _)| *entry != handle);
        self.entries.len() != before
    }

    /// How many predicates are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no predicate is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether any predicate suppresses this request.
    ///
    /// # Errors
    ///
    /// Returns the first failing predicate's error. Registration order is evaluation order, so the
    /// error a caller sees is deterministic.
    pub fn excludes(&self, request: &UploadRequest<'_>) -> Result<bool, ExclusionError> {
        for (_, exclude) in &self.entries {
            if exclude(request)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Whether a block has already been reported.
///
/// Upstream warns once per blocked sequence: a session whose next event is too large would
/// otherwise warn on every single request for the rest of its life. The dedupe is per session, which
/// is why it lives beside [`AcceptanceFold`] in [`SessionLogCursor`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockWarning {
    last: Option<u64>,
}

impl BlockWarning {
    /// A reporter that has reported nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Whether a block at `seq` should be reported now.
    pub fn should_log(&mut self, seq: u64) -> bool {
        if self.last == Some(seq) {
            return false;
        }
        self.last = Some(seq);
        true
    }

    /// The sequence last reported, if any.
    #[must_use]
    pub const fn last(&self) -> Option<u64> {
        self.last
    }
}

// ---------------------------------------------------------------------------------------------
// The prepare decision
// ---------------------------------------------------------------------------------------------

/// One session's upload state: the acceptance cache and the block report.
///
/// Upstream keys both by session in `WeakMap`s. Making them one struct means a caller cannot keep
/// one and forget the other, and the lifetime is the caller's to state — a cursor dropped with the
/// session is what upstream's weak keys approximate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionLogCursor {
    /// The acceptance fold.
    pub fold: AcceptanceFold,
    /// The block report.
    pub warning: BlockWarning,
}

impl SessionLogCursor {
    /// A cursor for a session whose log has not been scanned.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fold: AcceptanceFold::new(),
            warning: BlockWarning::new(),
        }
    }
}

/// A field ready to be placed on a request.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedUpload {
    /// The field itself.
    pub value: SessionLogExtension,
}

impl PreparedUpload {
    /// The field's `JSON` form.
    #[must_use]
    pub fn to_value(&self) -> Value {
        self.value.to_value()
    }

    /// The event to append once the endpoint accepts the delivery.
    #[must_use]
    pub fn acceptance(&self) -> AcceptanceEvent {
        AcceptanceEvent {
            session_id: self.value.session.id.clone(),
            session_format_version: self.value.session_format_version,
            through_seq: self.value.through_seq,
        }
    }

    /// The highest sequence the field covers.
    #[must_use]
    pub const fn through_seq(&self) -> u64 {
        self.value.through_seq
    }
}

/// What one prepare attempt produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Preparation {
    /// The contribution is switched off.
    Disabled,
    /// A predicate suppressed this request.
    Excluded,
    /// Everything up to the end of the log is already acknowledged.
    Absent,
    /// The next event does not fit the byte budget.
    Blocked(BlockedUpload),
    /// A field to place on the request.
    Ready(PreparedUpload),
}

/// Why a prepare attempt could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrepareError {
    /// A predicate failed.
    Exclusion(ExclusionError),
    /// The log's acceptance events are malformed.
    Watermark(WatermarkError),
    /// An event cannot be projected onto the wire.
    Emit(EmitError),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exclusion(error) => error.fmt(formatter),
            Self::Watermark(error) => error.fmt(formatter),
            Self::Emit(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PrepareError {}

impl From<WatermarkError> for PrepareError {
    fn from(error: WatermarkError) -> Self {
        Self::Watermark(error)
    }
}

impl From<EmitError> for PrepareError {
    fn from(error: EmitError) -> Self {
        Self::Emit(error)
    }
}

impl From<ExclusionError> for PrepareError {
    fn from(error: ExclusionError) -> Self {
        Self::Exclusion(error)
    }
}

/// The request-extension contributor.
///
/// Holds the two things that outlive a session — the volatile switch and the exclusion registry —
/// and takes the per-session state as an argument, which is the one structural difference from
/// upstream's service object. Everything else is a straight port of `prepare`.
#[derive(Debug)]
pub struct Uploader {
    config: Config,
    exclusions: Exclusions,
}

impl Uploader {
    /// Builds a contributor.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            exclusions: Exclusions::new(),
        }
    }

    /// The current controls.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Reads the volatile switch, as upstream does before every contribution.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
    }

    /// Registers an exclusion predicate.
    pub fn register_exclusion(
        &mut self,
        exclude: impl Fn(&UploadRequest<'_>) -> Result<bool, ExclusionError> + 'static,
    ) -> ExclusionHandle {
        self.exclusions.register(exclude)
    }

    /// Removes an exclusion predicate.
    pub fn remove_exclusion(&mut self, handle: ExclusionHandle) -> bool {
        self.exclusions.remove(handle)
    }

    /// The registry, for a caller that wants to inspect it.
    #[must_use]
    pub const fn exclusions(&self) -> &Exclusions {
        &self.exclusions
    }

    /// Prepares this request's contribution.
    ///
    /// The order is upstream's, and every step before the last can answer without touching the
    /// byte budget:
    ///
    /// 1. the volatile switch;
    /// 2. the exclusions, which see the request's identity but not its log;
    /// 3. the acceptance fold, which validates the log's evidence;
    /// 4. the pending tail, which is everything after the acknowledged watermark;
    /// 5. the prefix that fits.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareError`] when a predicate fails, an acceptance event is malformed, or an
    /// event cannot be projected.
    pub fn prepare(
        &self,
        cursor: &mut SessionLogCursor,
        header: &HeaderFacts,
        events: &[PendingEvent],
        request: &UploadRequest<'_>,
    ) -> Result<Preparation, PrepareError> {
        if !self.config.enabled {
            return Ok(Preparation::Disabled);
        }
        if self.exclusions.excludes(request)? {
            return Ok(Preparation::Excluded);
        }
        let after_seq = accepted_through(events, header, &mut cursor.fold)?;
        // `after_seq` is at least `-1`, so the first pending index is at most `0`.
        let start = usize::try_from(after_seq.saturating_add(1)).unwrap_or(usize::MAX);
        let pending = events.get(start..).unwrap_or(&[]);
        if pending.is_empty() {
            return Ok(Preparation::Absent);
        }

        let extension = SessionLogExtension {
            version: EXTENSION_VERSION,
            session_format_version: header.version,
            session: WireHeader::from_facts(header),
            after_seq,
            through_seq: 0,
            events: Vec::new(),
        };
        let Some(base) = extension.base_field_bytes() else {
            let seq = pending.first().map_or(0, |event| event.seq);
            return Err(PrepareError::Emit(EmitError::Unserializable { seq }));
        };
        match fit_prefix(base, pending, self.config.max_bytes)? {
            FitOutcome::Empty => Ok(Preparation::Absent),
            FitOutcome::Blocked(blocked) => Ok(Preparation::Blocked(blocked)),
            FitOutcome::Fit(fit) => Ok(Preparation::Ready(PreparedUpload {
                value: SessionLogExtension {
                    through_seq: fit.through_seq,
                    events: fit.events,
                    ..extension
                },
            })),
        }
    }
}
