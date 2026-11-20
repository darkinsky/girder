//! The Remote stream **carrier**: wire messages, the bounded uplink inbox, the per-connection
//! stream table, and the heartbeat/liveness rules that own a `/api/remote.mux` socket.
//!
//! Upstream `packages/api/gateway` is 5,691 lines across twelve files in four layers:
//!
//! 1. **`stream-protocol.ts`** — the text frames of the WebSocket mux (`open`/`item`/`end`/`cancel`
//!    up, `item`/`error`/`end` down), their exact-key validation, the `RemoteUplinkWindow` budget,
//!    and the forwarded-event frame shapes.
//! 2. **`stream-server.ts`** — the mux itself: one socket carries many logical streams keyed by
//!    `streamId`, each with a bounded uplink inbox; heartbeat accounting; upgrade rejection.
//! 3. **`index.ts`** — dispatch: endpoint claiming, descriptor resolution, access, Result
//!    envelopes, and the forwarded-event registry.
//! 4. **`client/*`** — the browser half: multiplexing, journal/resnapshot streams, reconnect.
//!
//! This module ports layer 1 and the parts of layer 2 that are decisions rather than `ws` plumbing.
//! Layer 3's event registry is already here as
//! [`api_remotes_gap`](girder_core::api::remotes) — the allowlist, the per-Client queue, the
//! waterfall bridge, and `resources_of` — so it is deliberately **not** repeated: what this module
//! adds is the carrier those events travel on, which nothing local implements. The discovery of
//! that split is the point of the exercise: `api_remotes_gap` answers *what may cross*, this module
//! answers *how a stream crosses*.
//!
//! # Why the inbox is a queue with a byte budget rather than a channel
//!
//! Upstream caps **buffered complete frame bytes**, not item count, because a single `paste` frame
//! and a single keystroke are both one item and differ by five orders of magnitude. A count-based
//! bound would let one paste consume the Host's memory. [`UplinkInbox`] therefore charges every
//! push the byte length of the frame that carried it, and reports
//! [`UplinkViolation::Overflow`] — `gateway/uplink-overflow` on the wire — the moment the *next*
//! frame would exceed the budget. The already-queued items are dropped with the failure, because a
//! consumer that has been told its input stream is broken must not keep reading a prefix of it and
//! believe the rest never arrived.
//!
//! # Why `fail` is not `end`
//!
//! `end` is the Client's half-close: "that is all I will send", and the consumer drains what is
//! buffered. `fail` is the carrier's verdict that the stream is over *for a reason*: it drops the
//! buffer first, so the consumer's next read raises instead of silently returning the tail of a
//! stream whose remainder was thrown away. Upstream calls `fail` from four places — overflow, an
//! item after `end`, a cancel, and the downlink having settled — and getting the drop wrong in any
//! of them is how a stream "succeeds" while losing input.
//!
//! # Why a duplicate `open` closes the socket, and an unknown `streamId` does not
//!
//! The two look symmetric and are not. A frame for a stream this connection no longer owns is
//! *expected*: a stream finishes, its entry leaves the table, and the Client's in-flight frames are
//! still crossing. Dropping them is correct. A duplicate `open` means the Client has two different
//! meanings for one `streamId`; no later frame can be attributed, so the only honest recovery is to
//! fail the physical generation ([`Reception::Duplicate`], close `1008`).
//!
//! # Why heartbeat liveness counts writes, not just Pongs
//!
//! A congested link queues the Pong behind application data, so a server that counted only Pongs
//! would kill a slow-but-healthy client — and it would kill it exactly when it is most expensive to
//! reconnect. Upstream therefore counts three things as activity: a Pong, any received frame, and a
//! downlink write that *did not complete at once* (buffered bytes prove the peer is behind). A write
//! that completes immediately proves nothing and does not count. [`Heartbeat`] models that with a
//! two-phase tick: [`Heartbeat::tick`] arms a verification and [`Heartbeat::verify`] re-checks, so a
//! Pong arriving in between still saves the socket.
//!
//! # What is not ported
//!
//! `ws`, `node:http` upgrades, `AsyncIterable`, `AbortSignal`, and the Cordis `PeerScope` binding.
//! This crate's socket is `girder-rpc`'s `server.rs`, which already owns the upgrade and the Peer;
//! what it lacks is the logical-stream layer above it. Everything here is therefore synchronous and
//! carrier-free: [`ClientMessage`] parses text, [`StreamTable`] routes a parsed frame, and
//! [`disposition`] decides which terminal frame the pump must write.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};

use girder_core::is_lossless;

/// How many heartbeat intervals a socket may miss before it is verified for termination.
///
/// Upstream's `MAX_MISSED_HEARTBEATS`. Two, not one: a single lost Ping/Pong pair on a busy radio
/// link is normal, and terminating on the first miss would turn a hiccup into a reconnect storm.
pub const MAX_MISSED_HEARTBEATS: u32 = 2;

/// The error text a run of `null` occupies in a downlink `item` frame.
///
/// [`frame_overhead_bytes`] subtracts this from a probe frame to find the envelope's own size. It is
/// a constant rather than a literal `4` in two places so the subtraction cannot drift from the
/// frame it was measured on.
const NULL_ITEM_BYTES: u64 = 4;

// ---- wire messages ----------------------------------------------------------------------------

/// A stable, carrier-independent failure shape a Host sends down a logical stream.
///
/// Upstream's `RemoteStreamFailure`. `details` is an object, never an array or a scalar: the
/// Gateway's own fault details are `{ endpoint, field? }`, and a Client that had to re-check the
/// shape of `details` before reading `endpoint` would be one branch away from a crash on a Host it
/// does not control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamFailure {
    /// The stable failure category, e.g. `gateway/uplink-overflow`.
    pub code: String,
    /// A correction-oriented diagnostic that carries no sensitive values.
    pub message: String,
    /// Structured details; empty for a failure that has none.
    pub details: Map<String, Value>,
}

impl StreamFailure {
    /// Builds a failure from its three wire fields.
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

    /// The failure as the JSON object the wire carries.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("code".to_owned(), Value::String(self.code.clone()));
        map.insert("message".to_owned(), Value::String(self.message.clone()));
        map.insert("details".to_owned(), Value::Object(self.details.clone()));
        Value::Object(map)
    }
}

/// One logical-stream request sent by a Client: open, one uplink item, half-close, or cancel.
///
/// `Item`'s `value` is [`Option`] rather than [`Value`] because upstream's frame may *omit* the
/// field entirely and the distinction survives the wire: `{ "type": "item", "streamId": "s" }` is a
/// present-but-undefined item, and `value: null` is a JSON `null`. Both are legal and the inbox
/// hands the consumer `None` and `Some(Value::Null)` respectively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMessage {
    /// Start a logical stream from a Remote endpoint and its decoded payload.
    Open {
        /// The logical-stream id this socket will use for every later frame.
        stream_id: String,
        /// The canonical `<namespace>/<method>` endpoint, or a Gateway-owned stream name.
        endpoint: String,
        /// The carrier payload, already decoded from JSON.
        payload: Value,
    },
    /// One Client-to-Host item of an open stream.
    Item {
        /// The stream the item belongs to.
        stream_id: String,
        /// The item, absent when the Client sent a frame without a `value` field.
        value: Option<Value>,
    },
    /// The Client's half-close: no more items will follow on this stream.
    End {
        /// The stream being half-closed.
        stream_id: String,
    },
    /// Cancel the logical stream; its Host iterator is asked to return.
    Cancel {
        /// The stream being cancelled.
        stream_id: String,
    },
}

impl ClientMessage {
    /// The logical stream this frame belongs to.
    #[must_use]
    pub fn stream_id(&self) -> &str {
        match self {
            Self::Open { stream_id, .. }
            | Self::Item { stream_id, .. }
            | Self::End { stream_id }
            | Self::Cancel { stream_id } => stream_id,
        }
    }

    /// Whether this frame creates a stream rather than addressing one.
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }
}

/// One logical-stream frame sent by a Host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerMessage {
    /// One Host-to-Client item.
    Item {
        /// The stream the item belongs to.
        stream_id: String,
        /// The item, absent when the Host sent a frame without a `value` field.
        value: Option<Value>,
    },
    /// The terminal failure of one stream; the Host will send nothing more for it.
    Error {
        /// The stream that failed.
        stream_id: String,
        /// Why it failed.
        error: StreamFailure,
    },
    /// The normal end of one stream.
    End {
        /// The stream that ended.
        stream_id: String,
    },
}

impl ServerMessage {
    /// The logical stream this frame belongs to.
    #[must_use]
    pub fn stream_id(&self) -> &str {
        match self {
            Self::Item { stream_id, .. }
            | Self::Error { stream_id, .. }
            | Self::End { stream_id } => stream_id,
        }
    }

    /// The frame as the JSON object the wire carries.
    ///
    /// An absent `value` stays absent rather than becoming `null`, which is what makes a round trip
    /// through [`parse_server_message`] reproduce the same value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        match self {
            Self::Item { stream_id, value } => {
                map.insert("type".to_owned(), Value::String("item".to_owned()));
                map.insert("streamId".to_owned(), Value::String(stream_id.clone()));
                if let Some(value) = value {
                    map.insert("value".to_owned(), value.clone());
                }
            }
            Self::End { stream_id } => {
                map.insert("type".to_owned(), Value::String("end".to_owned()));
                map.insert("streamId".to_owned(), Value::String(stream_id.clone()));
            }
            Self::Error { stream_id, error } => {
                map.insert("type".to_owned(), Value::String("error".to_owned()));
                map.insert("streamId".to_owned(), Value::String(stream_id.clone()));
                map.insert("error".to_owned(), error.to_value());
            }
        }
        Value::Object(map)
    }

    /// The frame as the compact JSON text a socket writes.
    ///
    /// Infallible, unlike a `Serializer` call: [`Value`] is already JSON by construction, so the
    /// only thing that could fail is floating-point formatting, which `serde_json` resolves.
    #[must_use]
    pub fn encode(&self) -> String {
        self.to_value().to_string()
    }
}

/// Why a text frame was rejected.
///
/// The variants spell upstream's four diagnostic strings exactly, because a Client logs them and a
/// log line that no longer matches the upstream one is a log line nobody can grep for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The text is not JSON at all.
    NotJson,
    /// The text is valid JSON but not an object.
    NotObject,
    /// An object, but not one of the four Client frames.
    InvalidClientMessage,
    /// An object, but not one of the three Host frames.
    InvalidServerMessage,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson => write!(f, "api gateway: Remote stream message is not JSON"),
            Self::NotObject => write!(f, "api gateway: Remote stream message must be an object"),
            Self::InvalidClientMessage => {
                write!(f, "api gateway: invalid Remote stream client message")
            }
            Self::InvalidServerMessage => {
                write!(f, "api gateway: invalid Remote stream server message")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Parses one Client-to-Host WebSocket text message.
///
/// Validation is by *exact* key set, as upstream's `exactKeys`: a recognised `type` with one extra
/// field is rejected rather than ignored. That strictness is what keeps the protocol extensible —
/// a future field cannot be smuggled into an older Host that would silently drop it — and it is
/// also what makes a misspelled `streamId` a loud failure instead of a frame routed to no stream.
///
/// # Errors
///
/// [`ParseError`] for a non-JSON text, a non-object JSON value, or an object that is not exactly
/// one of [`ClientMessage`]'s four variants. An `item` whose `value` is not lossless JSON
/// (see [`girder_core::is_lossless`]) is rejected here, at the boundary, because it would be
/// corrupted by the very transport that carried it.
pub fn parse_client_message(text: &str) -> Result<ClientMessage, ParseError> {
    let decoded: Value = serde_json::from_str(text).map_err(|_| ParseError::NotJson)?;
    let map = decoded.as_object().ok_or(ParseError::NotObject)?;
    parse_client_record(map)
}

/// Parses one Host-to-Client WebSocket text message.
///
/// # Errors
///
/// [`ParseError`] for a non-JSON text, a non-object JSON value, or an object that is not exactly
/// one of [`ServerMessage`]'s three variants.
pub fn parse_server_message(text: &str) -> Result<ServerMessage, ParseError> {
    let decoded: Value = serde_json::from_str(text).map_err(|_| ParseError::NotJson)?;
    let map = decoded.as_object().ok_or(ParseError::NotObject)?;
    parse_server_record(map)
}

fn parse_client_record(map: &Map<String, Value>) -> Result<ClientMessage, ParseError> {
    let invalid = || ParseError::InvalidClientMessage;
    let kind = map
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    match kind {
        "open" => {
            if !exact_keys(map, &["type", "streamId", "endpoint", "payload"]) {
                return Err(invalid());
            }
            Ok(ClientMessage::Open {
                stream_id: non_empty_string(map, "streamId").ok_or_else(invalid)?,
                endpoint: non_empty_string(map, "endpoint").ok_or_else(invalid)?,
                payload: map.get("payload").cloned().unwrap_or(Value::Null),
            })
        }
        "item" => {
            if !matches!(
                map.len(),
                2 if exact_keys(map, &["type", "streamId"])
            ) && !exact_keys(map, &["type", "streamId", "value"])
            {
                return Err(invalid());
            }
            let stream_id = non_empty_string(map, "streamId").ok_or_else(invalid)?;
            let value = match map.get("value") {
                None => None,
                Some(value) if is_lossless(value) => Some(value.clone()),
                Some(_) => return Err(invalid()),
            };
            Ok(ClientMessage::Item { stream_id, value })
        }
        "end" | "cancel" => {
            if !exact_keys(map, &["type", "streamId"]) {
                return Err(invalid());
            }
            let stream_id = non_empty_string(map, "streamId").ok_or_else(invalid)?;
            Ok(if kind == "end" {
                ClientMessage::End { stream_id }
            } else {
                ClientMessage::Cancel { stream_id }
            })
        }
        _ => Err(invalid()),
    }
}

fn parse_server_record(map: &Map<String, Value>) -> Result<ServerMessage, ParseError> {
    let invalid = || ParseError::InvalidServerMessage;
    let kind = map
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    match kind {
        "item" => {
            if !exact_keys(map, &["type", "streamId"])
                && !exact_keys(map, &["type", "streamId", "value"])
            {
                return Err(invalid());
            }
            Ok(ServerMessage::Item {
                stream_id: non_empty_string(map, "streamId").ok_or_else(invalid)?,
                value: map.get("value").cloned(),
            })
        }
        "end" => {
            if !exact_keys(map, &["type", "streamId"]) {
                return Err(invalid());
            }
            Ok(ServerMessage::End {
                stream_id: non_empty_string(map, "streamId").ok_or_else(invalid)?,
            })
        }
        "error" => {
            if !exact_keys(map, &["type", "streamId", "error"]) {
                return Err(invalid());
            }
            let stream_id = non_empty_string(map, "streamId").ok_or_else(invalid)?;
            let error = map
                .get("error")
                .and_then(Value::as_object)
                .ok_or_else(invalid)?;
            if !exact_keys(error, &["code", "message", "details"]) {
                return Err(invalid());
            }
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            let details = error
                .get("details")
                .and_then(Value::as_object)
                .ok_or_else(invalid)?;
            Ok(ServerMessage::Error {
                stream_id,
                error: StreamFailure::new(code, message, details.clone()),
            })
        }
        _ => Err(invalid()),
    }
}

fn exact_keys(map: &Map<String, Value>, expected: &[&str]) -> bool {
    map.len() == expected.len() && expected.iter().all(|key| map.contains_key(*key))
}

fn non_empty_string(map: &Map<String, Value>, key: &str) -> Option<String> {
    let value = map.get(key)?.as_str()?;
    if value.is_empty() {
        return None;
    }
    Some(value.to_owned())
}

// ---- the uplink budget ------------------------------------------------------------------------

/// The frame budget one logical stream carries into its Host method.
///
/// Upstream's `RemoteUplinkWindow`: `maxBytes` is the whole inbox budget and `frameOverheadBytes`
/// is the envelope a *single* item adds on top of its JSON body, so a Host that wants to report
/// "you may send 8 KiB of payload" can subtract without re-deriving the frame shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UplinkWindow {
    /// Maximum complete encoded frame bytes waiting for the Host consumer.
    pub max_bytes: u64,
    /// UTF-8 envelope bytes added to a compact JSON item with a present `value` field.
    pub frame_overhead_bytes: u64,
}

impl UplinkWindow {
    /// Builds a window from an already-measured overhead.
    #[must_use]
    pub const fn new(max_bytes: u64, frame_overhead_bytes: u64) -> Self {
        Self {
            max_bytes,
            frame_overhead_bytes,
        }
    }

    /// Derives the window for one stream id, measuring the overhead rather than assuming it.
    ///
    /// The overhead is not a constant: the stream id is part of the frame, so a longer id leaves
    /// less room for the item itself. Measuring it is the only way the subtraction stays honest.
    #[must_use]
    pub fn for_stream(stream_id: &str, max_bytes: u64) -> Self {
        Self::new(max_bytes, frame_overhead_bytes(stream_id))
    }

    /// The complete frame bytes one item would occupy in this stream's inbox.
    #[must_use]
    pub fn frame_bytes(&self, value: &Value) -> u64 {
        bytes_of(&value.to_string()).saturating_add(self.frame_overhead_bytes)
    }
}

/// The byte length of one item frame's envelope, for a given stream id.
///
/// Measured by serialising a probe frame whose `value` is `null` and subtracting that `null`, so the
/// escaping of a stream id with quotes or non-ASCII characters is accounted for exactly as
/// `serde_json` will do it on the real frame.
#[must_use]
pub fn frame_overhead_bytes(stream_id: &str) -> u64 {
    let mut probe = Map::new();
    probe.insert("type".to_owned(), Value::String("item".to_owned()));
    probe.insert("streamId".to_owned(), Value::String(stream_id.to_owned()));
    probe.insert("value".to_owned(), Value::Null);
    bytes_of(&Value::Object(probe).to_string()).saturating_sub(NULL_ITEM_BYTES)
}

/// The UTF-8 byte length of a text frame.
///
/// A function rather than `text.len()` at each call site because the *wire* unit is UTF-8 bytes and
/// Rust's `str::len` happens to be the same number: naming it here means a future move to a
/// different encoding changes one place.
#[must_use]
pub fn bytes_of(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

// ---- the inbox --------------------------------------------------------------------------------

/// Why an uplink inbox was failed by the carrier rather than by its consumer.
///
/// Both variants are `RemoteError`s upstream, which is what makes them travel to the Client as a
/// terminal `error` frame instead of a silent cancellation. [`UplinkViolation::code`] is the wire
/// code, so the transport does not have to map the variant again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UplinkViolation {
    /// The Client sent an item after its own `end`.
    ItemAfterEnd {
        /// The endpoint whose stream was violated, for the Client's diagnostic.
        endpoint: String,
    },
    /// The next item would have pushed buffered frame bytes past the budget.
    Overflow {
        /// The endpoint whose stream overflowed.
        endpoint: String,
        /// The budget that was exceeded.
        max_bytes: u64,
    },
}

impl UplinkViolation {
    /// The stable Remote failure code this violation travels as.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ItemAfterEnd { .. } => "gateway/protocol",
            Self::Overflow { .. } => "gateway/uplink-overflow",
        }
    }
}

impl std::fmt::Display for UplinkViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ItemAfterEnd { .. } => {
                write!(f, "api gateway: Remote stream uplink item after end")
            }
            Self::Overflow { max_bytes, .. } => write!(
                f,
                "api gateway: Remote stream uplink exceeded {max_bytes} buffered bytes"
            ),
        }
    }
}

impl UplinkViolation {
    /// The failure as the wire shape a Client receives.
    #[must_use]
    pub fn to_failure(&self) -> StreamFailure {
        let endpoint = match self {
            Self::ItemAfterEnd { endpoint } | Self::Overflow { endpoint, .. } => endpoint.clone(),
        };
        let mut details = Map::new();
        details.insert("endpoint".to_owned(), Value::String(endpoint));
        StreamFailure::new(self.code(), self.to_string(), details)
    }
}

/// Why a consumer's read failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UplinkError {
    /// The carrier failed the stream; the variant says why, and stays stable across re-reads.
    Violation(UplinkViolation),
    /// The stream was closed for a reason that is not a protocol violation: a cancel, a socket
    /// close, or the downlink having settled. Upstream carries these as plain `Error`s, so they are
    /// *not* delivered to the Client as a terminal `error` frame.
    Closed {
        /// What the closer said, for the Host's log.
        reason: String,
    },
}

impl std::fmt::Display for UplinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Violation(violation) => write!(f, "{violation}"),
            Self::Closed { reason } => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for UplinkError {}

/// The result of one non-blocking read of an uplink inbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UplinkPoll {
    /// One item, in the order the Client sent it.
    Item(Value),
    /// Nothing buffered yet, and neither `end` nor `fail` has been called.
    Pending,
    /// The Client half-closed and everything it sent has been consumed.
    Ended,
}

#[derive(Default)]
struct UplinkState {
    queue: VecDeque<(Value, u64)>,
    bytes: u64,
    ended: bool,
    closed: bool,
    failure: Option<UplinkError>,
}

/// The bounded single-consumer uplink queue of one logical stream.
///
/// Shared as an `Arc` between the frame reader (which pushes) and the Host method (which reads), and
/// deliberately *not* tracking "one consumer" the way upstream does: upstream must check because an
/// `AsyncIterable` can be asked for an iterator twice at runtime, while here the pump moves the only
/// handle it has into one read loop, so a second consumer is not expressible.
///
/// # Ordering
///
/// Items come out in push order, and a push that is refused changes nothing: it neither queues the
/// value nor charges its bytes. A refused push may still *fail* the stream (overflow, item after
/// end), and that failure drops everything already queued — see the module docs on `fail` vs `end`.
pub struct UplinkInbox {
    max_bytes: u64,
    endpoint: String,
    state: Mutex<UplinkState>,
}

impl UplinkInbox {
    /// Builds an inbox with a complete-frame byte budget and the endpoint it speaks for.
    #[must_use]
    pub fn new(max_bytes: u64, endpoint: impl Into<String>) -> Self {
        Self {
            max_bytes,
            endpoint: endpoint.into(),
            state: Mutex::new(UplinkState::default()),
        }
    }

    /// The endpoint this inbox belongs to.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The byte budget this inbox enforces.
    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Buffered frame bytes, the number the budget is checked against.
    #[must_use]
    pub fn buffered_bytes(&self) -> u64 {
        self.lock().bytes
    }

    /// Number of buffered items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().queue.len()
    }

    /// Whether nothing is buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().queue.is_empty()
    }

    /// Queues one item, charging the byte length of the frame that carried it.
    ///
    /// Returns `Ok(())` when the item was buffered — and also when the stream has already been
    /// failed or closed, in which case the item is silently dropped, matching upstream: a frame that
    /// arrives after the stream died is not a new violation.
    ///
    /// # Errors
    ///
    /// [`UplinkViolation`] when the item is illegal (after `end`) or would overflow the budget. The
    /// stream is failed in both cases, so a later read raises [`UplinkError::Violation`].
    pub fn push(&self, value: Value, frame_bytes: u64) -> Result<(), UplinkViolation> {
        let mut state = self.lock();
        if state.failure.is_some() || state.closed {
            return Ok(());
        }
        if state.ended {
            let violation = UplinkViolation::ItemAfterEnd {
                endpoint: self.endpoint.clone(),
            };
            fail_locked(&mut state, UplinkError::Violation(violation.clone()));
            return Err(violation);
        }
        if state.bytes.saturating_add(frame_bytes) > self.max_bytes {
            let violation = UplinkViolation::Overflow {
                endpoint: self.endpoint.clone(),
                max_bytes: self.max_bytes,
            };
            fail_locked(&mut state, UplinkError::Violation(violation.clone()));
            return Err(violation);
        }
        state.bytes = state.bytes.saturating_add(frame_bytes);
        state.queue.push_back((value, frame_bytes));
        Ok(())
    }

    /// The Client's half-close. Idempotent, and ignored once the stream has failed.
    pub fn end(&self) {
        let mut state = self.lock();
        if state.failure.is_some() {
            return;
        }
        state.ended = true;
    }

    /// Fails the stream for a non-violation reason, dropping everything buffered.
    ///
    /// Idempotent: the first reason wins, so a cancel after an overflow still reports the overflow.
    pub fn fail(&self, reason: impl Into<String>) {
        let mut state = self.lock();
        fail_locked(
            &mut state,
            UplinkError::Closed {
                reason: reason.into(),
            },
        );
    }

    /// The consumer stopped reading: later items are dropped and a pending read would end.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.queue.clear();
        state.bytes = 0;
    }

    /// Whether the Client has half-closed this stream.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.lock().ended
    }

    /// Whether the stream has been failed or closed.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        let state = self.lock();
        state.closed || state.failure.is_some()
    }

    /// Reads one item without blocking.
    ///
    /// # Errors
    ///
    /// [`UplinkError`] once the stream was failed, including when the buffer was non-empty: a
    /// consumer must not be handed a prefix of a stream whose remainder was discarded.
    pub fn poll(&self) -> Result<UplinkPoll, UplinkError> {
        let mut state = self.lock();
        if state.closed {
            return Ok(UplinkPoll::Ended);
        }
        if let Some((value, bytes)) = state.queue.pop_front() {
            state.bytes = state.bytes.saturating_sub(bytes);
            return Ok(UplinkPoll::Item(value));
        }
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        if state.ended {
            return Ok(UplinkPoll::Ended);
        }
        Ok(UplinkPoll::Pending)
    }

    fn lock(&self) -> MutexGuard<'_, UplinkState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn fail_locked(state: &mut UplinkState, error: UplinkError) {
    if state.failure.is_some() {
        return;
    }
    state.failure = Some(error);
    state.queue.clear();
    state.bytes = 0;
}

// ---- the per-connection stream table ----------------------------------------------------------

/// Why a logical stream's Host iterator stopped.
///
/// The split between [`AbortReason::Remote`] and [`AbortReason::Local`] is the whole reason
/// [`disposition`] exists: a Remote failure must be *delivered* to the Client, while a cancellation
/// must not, because the Client is the one that asked for it and a terminal frame for an abandoned
/// stream would be noise at best and a misattributed error at worst.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AbortReason {
    /// Not aborted: the Host iterator settled on its own.
    NotAborted,
    /// The Gateway failed the stream with a Remote failure the Client must receive.
    Remote(UplinkViolation),
    /// The Client cancelled, the socket closed, or the downlink settled. Silent.
    Local,
}

/// The terminal frame a pump must write after its Host iterator settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Write `{ "type": "error", ... }` with the abort reason's failure.
    Failure,
    /// Write `{ "type": "end" }`.
    End,
    /// Write nothing: the Client cancelled.
    Silent,
}

/// Decides which terminal frame one stream's pump must write.
///
/// Ordering matters and is upstream's: the abort reason wins over the iterator's own outcome,
/// because a cancelled stream that also *happened* to throw must not report the throw — the Client
/// is no longer listening for it, and reporting it would make a cancellation look like a failure.
#[must_use]
pub fn disposition(reason: &AbortReason, failed: bool) -> Disposition {
    match reason {
        AbortReason::Remote(_) => Disposition::Failure,
        AbortReason::Local => Disposition::Silent,
        AbortReason::NotAborted if failed => Disposition::Failure,
        AbortReason::NotAborted => Disposition::End,
    }
}

/// What one received frame did to the connection's stream table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reception {
    /// An `open` created a stream; the caller must now pump it.
    Opened,
    /// The frame reached a live stream.
    Routed,
    /// The frame addressed no live stream — dropped, as upstream drops a finished stream's tail.
    Dropped,
    /// A second `open` for a live `streamId`: the connection must close with `1008`.
    Duplicate,
}

/// One live logical stream: its inbox and why it is (or is not) aborted.
struct ActiveStream {
    inbox: Arc<UplinkInbox>,
    reason: AbortReason,
}

/// The logical streams one WebSocket connection currently owns.
///
/// Upstream's `RemoteStreamMuxConnection.streams`. The table owns the abort bookkeeping so the
/// transport does not have to correlate a violation it returned from `push` with a key in a second
/// map: [`StreamTable::receive`] records it on the stream that caused it, and
/// [`StreamTable::finish`] hands the pump everything it needs to choose a terminal frame.
pub struct StreamTable {
    max_bytes: u64,
    streams: BTreeMap<String, ActiveStream>,
}

impl StreamTable {
    /// Builds a table whose streams each get a `max_bytes` uplink budget.
    #[must_use]
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            streams: BTreeMap::new(),
        }
    }

    /// How many logical streams are live.
    #[must_use]
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    /// Whether no logical stream is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// The inbox of a live stream, for a consumer that reads it outside [`StreamTable::receive`].
    #[must_use]
    pub fn inbox(&self, stream_id: &str) -> Option<Arc<UplinkInbox>> {
        self.streams
            .get(stream_id)
            .map(|stream| Arc::clone(&stream.inbox))
    }

    /// Why a live stream is aborted, or `None` when no such stream is live.
    #[must_use]
    pub fn abort_reason(&self, stream_id: &str) -> Option<&AbortReason> {
        self.streams.get(stream_id).map(|stream| &stream.reason)
    }

    /// Routes one parsed Client frame.
    ///
    /// `frame_bytes` is the UTF-8 length of the text that carried the frame; it is what the inbox
    /// budget is charged, so a caller that reconstructs the message instead of measuring the text
    /// must pass the original length.
    pub fn receive(&mut self, message: &ClientMessage, frame_bytes: u64) -> Reception {
        match message {
            ClientMessage::Open {
                stream_id,
                endpoint,
                ..
            } => {
                if self.streams.contains_key(stream_id) {
                    return Reception::Duplicate;
                }
                self.streams.insert(
                    stream_id.clone(),
                    ActiveStream {
                        inbox: Arc::new(UplinkInbox::new(self.max_bytes, endpoint.clone())),
                        reason: AbortReason::NotAborted,
                    },
                );
                Reception::Opened
            }
            ClientMessage::Item { stream_id, value } => {
                let Some(stream) = self.streams.get_mut(stream_id) else {
                    return Reception::Dropped;
                };
                if let Some(value) = value.clone()
                    && let Err(violation) = stream.inbox.push(value, frame_bytes)
                {
                    stream.reason = AbortReason::Remote(violation);
                }
                Reception::Routed
            }
            ClientMessage::End { stream_id } => {
                let Some(stream) = self.streams.get(stream_id) else {
                    return Reception::Dropped;
                };
                stream.inbox.end();
                Reception::Routed
            }
            ClientMessage::Cancel { stream_id } => {
                let Some(stream) = self.streams.get_mut(stream_id) else {
                    return Reception::Dropped;
                };
                stream.reason = AbortReason::Local;
                stream.inbox.fail("Remote stream cancelled");
                Reception::Routed
            }
        }
    }

    /// Removes one settled stream and reports how its pump must terminate.
    ///
    /// The inbox is failed first, so a Host method still parked on a read is woken with "the
    /// downlink settled" rather than hanging until the socket dies.
    pub fn finish(&mut self, stream_id: &str, failed: bool) -> Option<Completion> {
        let stream = self.streams.remove(stream_id)?;
        stream.inbox.fail("Remote stream ended");
        Some(Completion {
            disposition: disposition(&stream.reason, failed),
            reason: stream.reason,
            inbox: stream.inbox,
        })
    }

    /// Stops every live stream because the socket is going away.
    ///
    /// Each stream is marked [`AbortReason::Local`] — a socket close is not a Gateway failure the
    /// Client could act on, and it may not even have a socket left to receive one on — and its inbox
    /// is failed so no Host reader is left parked.
    pub fn shutdown(&mut self) -> Vec<(String, Arc<UplinkInbox>)> {
        let streams = std::mem::take(&mut self.streams);
        streams
            .into_iter()
            .map(|(id, mut stream)| {
                stream.reason = AbortReason::Local;
                stream.inbox.fail("Remote stream socket closed");
                (id, stream.inbox)
            })
            .collect()
    }
}

/// What [`StreamTable::finish`] reports about one settled stream.
pub struct Completion {
    /// Why the stream stopped.
    pub reason: AbortReason,
    /// The terminal frame the pump must write.
    pub disposition: Disposition,
    /// The stream's inbox, already failed, for a caller that wants the final byte accounting.
    pub inbox: Arc<UplinkInbox>,
}

// ---- heartbeat --------------------------------------------------------------------------------

/// What one heartbeat interval asks the connection to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tick {
    /// Send a Ping and keep the socket.
    Ping,
    /// The peer has missed [`MAX_MISSED_HEARTBEATS`] intervals: verify in a later turn before
    /// terminating, because a Pong may still arrive.
    Verify,
}

/// The liveness counter of one socket.
///
/// Activity is reported by the connection, not derived here: a Pong, any received frame, and a
/// downlink write that had to wait are all [`Heartbeat::alive`]. A write that completed immediately
/// proves nothing about the peer and must not be reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Heartbeat {
    missed: u32,
}

impl Heartbeat {
    /// A fresh socket that has just completed its upgrade.
    #[must_use]
    pub const fn new() -> Self {
        Self { missed: 0 }
    }

    /// The number of intervals missed since the last activity.
    #[must_use]
    pub const fn missed(self) -> u32 {
        self.missed
    }

    /// Records peer activity and clears the miss counter.
    pub const fn alive(&mut self) {
        self.missed = 0;
    }

    /// Advances one interval.
    ///
    /// The first [`MAX_MISSED_HEARTBEATS`] intervals of silence each send a Ping; the interval that
    /// reaches the limit asks for [`Tick::Verify`] instead of a third Ping, because pinging a peer
    /// that has not answered twice is what fills a dead socket's send buffer.
    pub const fn tick(&mut self) -> Tick {
        if self.missed >= MAX_MISSED_HEARTBEATS {
            return Tick::Verify;
        }
        self.missed = self.missed.saturating_add(1);
        Tick::Ping
    }

    /// Re-checks a pending [`Tick::Verify`]; `true` means terminate the socket now.
    ///
    /// The second look is the whole point: between the tick that armed it and this call, a delayed
    /// Pong may have arrived ([`Heartbeat::alive`] resets the counter), and terminating anyway would
    /// punish exactly the congested-but-alive peer the write-based liveness rule was added to keep.
    #[must_use]
    pub const fn verify(self) -> bool {
        self.missed >= MAX_MISSED_HEARTBEATS
    }
}

// ---- upgrade rejection ------------------------------------------------------------------------

/// Why an upgrade to the stream mux was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeRejection {
    /// No authenticated Peer: `401`.
    Unauthorized,
    /// An authenticated Peer that may not open the mux: `403`.
    Forbidden,
    /// A Peer that conflicts with one already admitted: `409`.
    Conflict,
}

impl UpgradeRejection {
    /// The HTTP status line this rejection sends.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::Conflict => 409,
        }
    }

    /// The reason phrase, which is also the lowercased body.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Unauthorized => "Unauthorized",
            Self::Forbidden => "Forbidden",
            Self::Conflict => "Conflict",
        }
    }

    /// The complete HTTP response for the socket, which the caller then ends.
    ///
    /// Written by hand rather than through a framework because the socket has not been transferred
    /// to the WebSocket server yet: the rejection has to be a plain HTTP/1.1 response on the raw
    /// connection, and a `Content-Length` computed from the body is the one thing a Client needs to
    /// not hang.
    #[must_use]
    pub fn response(self) -> String {
        let body = self.reason().to_lowercase();
        format!(
            "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            self.status(),
            self.reason(),
            bytes_of(&body),
            body
        )
    }
}
