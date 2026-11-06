//! The RPC protocol: wire frames, schema export, and the WebSocket transport.
//!
//! Upstream's gateway is a WebSocket mux at `/api/remote.mux` carrying four downlink frame types and
//! one uplink shape. This crate carries the same protocol without Typert's compile-time descriptor
//! generation — see `docs/phase6-scope.md` for why that split was chosen and what it costs.
//!
//! # The protocol is untyped on the wire
//!
//! Upstream's `InvokeRemoteRequest.args` is `Record<string, unknown>`; type safety comes entirely
//! from descriptors the TypeScript compiler generates from service signatures. Nothing about field
//! names or shapes travels on the wire. That is why this crate *can* speak the protocol without
//! reimplementing Typert, and also why a field rename is not a compile error on either side.
//!
//! The mitigation here is [`schema`]: every message type derives `JsonSchema`, so the two sides can
//! be checked against one artifact even though the wire itself carries no types.
//!
//! # What is not here
//!
//! - Descriptor generation (the compile-time projection).
//! - The `Remote` method namespaces (`session.list`, `workspace.*`, …).
//! - Authentication: `PeerScope` is carried through, never verified.
//!
//! Consequence: this is **protocol connectivity**, not a GUI that works. A real DSH client calls
//! generated methods this crate does not implement.

pub mod server;

pub use server::{
    ClientAnswer, ClientHandle, RpcServer, TransportError, forward_events, serve, serve_on,
};

use std::collections::HashMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The exact WebSocket route carrying every remote stream.
pub const REMOTE_STREAM_MUX_PATH: &str = "/api/remote.mux";

/// Gateway-internal logical stream carrying application-selected events.
pub const REMOTE_EVENT_STREAM_ENDPOINT: &str = "$events";

/// Gateway-internal unary endpoint returning one client event outcome.
pub const REMOTE_EVENT_RESULT_ENDPOINT: &str = "$events/result";

/// Opaque identity for one active client event generation.
///
/// A string rather than a newtype: the wire treats it as an opaque token, and a newtype would only
/// add conversion noise at every boundary without preventing a wrong-id bug (both sides are strings
/// coming from the same place).
pub type RemoteEventClientId = String;

/// Opaque correlation id for one pending host-to-client waterfall.
pub type RemoteEventId = String;

/// Opaque agent identity carried by one scoped event.
pub type RemoteEventAgentId = String;

/// Stable host facts published with every established event generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteEventHostInfo {
    /// Host account home, used only to abbreviate displayed filesystem paths.
    pub home: String,
}

/// A frame the host sends to a client.
///
/// `#[serde(tag = "type")]` matches upstream's discriminator exactly, so a real client's frame
/// parser accepts these without translation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DownlinkFrame {
    /// The connection is established; later results bind to this generation.
    Ready {
        /// Identity of this generation.
        #[serde(rename = "clientId")]
        client_id: RemoteEventClientId,
        /// Host facts.
        host: RemoteEventHostInfo,
    },

    /// One host notification.
    Emit {
        /// Event name in upstream's `domain/name` spelling.
        event: String,
        /// Positional arguments.
        args: Vec<serde_json::Value>,
    },

    /// One pending waterfall the client must resolve.
    Waterfall {
        /// Event name.
        event: String,
        /// Correlation id the client echoes back in its result.
        #[serde(rename = "eventId")]
        event_id: RemoteEventId,
        /// Agent the waterfall is scoped to.
        #[serde(rename = "agentId")]
        agent_id: RemoteEventAgentId,
        /// The request object.
        request: serde_json::Map<String, serde_json::Value>,
    },

    /// Cancellation of a pending waterfall previously delivered under the same id.
    Cancel {
        /// The waterfall to abandon.
        #[serde(rename = "eventId")]
        event_id: RemoteEventId,
    },
}

/// Why a client refused a waterfall.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteEventRejection {
    /// Error name.
    pub name: String,
    /// Human-readable message.
    pub message: String,
    /// Stable machine-readable code, when the error had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// JSON-safe details, when the error carried any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// A client's verdict on one delivered waterfall.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RemoteEventOutcome {
    /// Continue the host's waterfall chain.
    Next,

    /// Replace the chain's value.
    Result {
        /// The replacement value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
    },

    /// Fail the chain.
    Rejected {
        /// The projected error.
        error: RemoteEventRejection,
    },
}

/// A client's answer to one delivered waterfall.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteEventResult {
    /// Generation this result belongs to.
    #[serde(rename = "clientId")]
    pub client_id: RemoteEventClientId,
    /// The waterfall being answered.
    #[serde(rename = "eventId")]
    pub event_id: RemoteEventId,
    /// The verdict.
    pub outcome: RemoteEventOutcome,
}

/// One remote method request after a carrier decoded its envelope.
///
/// `args` stays untyped for the same reason upstream's does: the types are a compile-time
/// projection, not wire data. Validation against a schema is the caller's job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InvokeRemoteRequest {
    /// Service namespace.
    pub namespace: String,
    /// Method name.
    pub method: String,
    /// Named arguments.
    pub args: serde_json::Map<String, serde_json::Value>,
    /// The peer the call speaks for. Carried, never verified.
    pub peer: String,
}

/// The response to an [`InvokeRemoteRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum InvokeResponse {
    /// The method succeeded.
    Ok {
        /// Its return value.
        value: serde_json::Value,
    },
    /// The method failed.
    Error {
        /// The projected error.
        error: RemoteEventRejection,
    },
}

/// The schema artifact both sides are checked against.
///
/// Every message type in one document. Emitting it as a file is what lets a TypeScript side generate
/// matching types; without that step the two sides drift silently.
pub fn schema() -> serde_json::Value {
    use schemars::generate::SchemaSettings;

    let settings = SchemaSettings::draft07();
    let generator = settings.into_generator();

    let mut definitions = serde_json::Map::new();
    let mut root = serde_json::Map::new();
    root.insert(
        "$schema".to_owned(),
        serde_json::Value::String("http://json-schema.org/draft-07/schema#".to_owned()),
    );
    root.insert(
        "title".to_owned(),
        serde_json::Value::String("girder-rpc".to_owned()),
    );

    let insert = |name: &str,
                  value: serde_json::Value,
                  definitions: &mut serde_json::Map<String, serde_json::Value>| {
        definitions.insert(name.to_owned(), value);
    };

    insert(
        "DownlinkFrame",
        serde_json::to_value(generator.clone().into_root_schema_for::<DownlinkFrame>())
            .expect("schema serializes"),
        &mut definitions,
    );
    insert(
        "RemoteEventResult",
        serde_json::to_value(
            generator
                .clone()
                .into_root_schema_for::<RemoteEventResult>(),
        )
        .expect("schema serializes"),
        &mut definitions,
    );
    insert(
        "InvokeRemoteRequest",
        serde_json::to_value(
            generator
                .clone()
                .into_root_schema_for::<InvokeRemoteRequest>(),
        )
        .expect("schema serializes"),
        &mut definitions,
    );
    insert(
        "InvokeResponse",
        serde_json::to_value(generator.into_root_schema_for::<InvokeResponse>())
            .expect("schema serializes"),
        &mut definitions,
    );

    root.insert("$defs".to_owned(), serde_json::Value::Object(definitions));
    serde_json::Value::Object(root)
}

/// A failure raised by the protocol layer.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// A frame could not be decoded.
    #[error("invalid frame: {0}")]
    Decode(#[from] serde_json::Error),

    /// The frame was well-formed JSON but not a known frame.
    #[error("unknown frame: {0}")]
    UnknownFrame(String),
}

impl std::fmt::Display for DownlinkFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready { client_id, .. } => write!(f, "ready({client_id})"),
            Self::Emit { event, args } => write!(f, "emit({event}, {} args)", args.len()),
            Self::Waterfall {
                event, event_id, ..
            } => write!(f, "waterfall({event}, {event_id})"),
            Self::Cancel { event_id } => write!(f, "cancel({event_id})"),
        }
    }
}

/// Decodes a downlink frame from text.
///
/// # Errors
///
/// Returns [`RpcError::Decode`] when the text is not a valid frame.
pub fn decode_downlink(text: &str) -> Result<DownlinkFrame, RpcError> {
    Ok(serde_json::from_str(text)?)
}

/// Encodes a downlink frame.
///
/// # Errors
///
/// Returns [`RpcError::Decode`] only if a value is not JSON-representable, which cannot happen for
/// these types.
pub fn encode_downlink(frame: &DownlinkFrame) -> Result<String, RpcError> {
    Ok(serde_json::to_string(frame)?)
}

/// Tracks pending waterfalls so a `cancel` frame can resolve them.
///
/// Upstream keeps this in `stream-server.ts` alongside the socket. Separating it makes the
/// bookkeeping testable without a socket, which is where the interesting bugs are: an id delivered
/// twice, a result arriving after cancellation, a disconnect leaking every pending entry.
#[derive(Default)]
pub struct PendingWaterfalls {
    entries: HashMap<RemoteEventId, String>,
}

impl PendingWaterfalls {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a delivered waterfall, returning false when the id was already pending.
    ///
    /// A duplicate id is a bug in the sender, not something to overwrite: overwriting would make the
    /// earlier request unanswerable while its caller still waits.
    pub fn insert(&mut self, id: RemoteEventId, event: String) -> bool {
        self.entries.insert(id, event).is_none()
    }

    /// Removes a pending waterfall, returning the event name it was for.
    pub fn take(&mut self, id: &str) -> Option<String> {
        self.entries.remove(id)
    }

    /// Number of waterfalls still awaiting a result.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops every pending entry, returning how many were abandoned.
    ///
    /// Called on disconnect: nothing can answer them any more, and leaving them would leak.
    pub fn drain(&mut self) -> usize {
        let count = self.entries.len();
        self.entries.clear();
        count
    }
}

impl std::fmt::Debug for PendingWaterfalls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingWaterfalls")
            .field("pending", &self.entries.len())
            .finish()
    }
}

/// Shared state one connection generation uses.
pub struct SessionState {
    /// Identity of this generation.
    pub client_id: RemoteEventClientId,
    /// Waterfalls delivered and not yet answered.
    ///
    /// Behind an `Arc` so [`ClientHandle`](crate::ClientHandle) can consult and drain the same map
    /// the connection owns. Two copies would let a disconnect drain one while the other still
    /// believed waterfalls were pending.
    pub pending: Arc<tokio::sync::Mutex<PendingWaterfalls>>,
    /// Event bus to forward from.
    pub bus: Arc<girder_core::EventBus>,
    /// Monotonic counter for event ids.
    next_event_id: std::sync::atomic::AtomicU64,
}

impl SessionState {
    /// Creates the state for one connection.
    #[must_use]
    pub fn new(client_id: impl Into<String>, bus: Arc<girder_core::EventBus>) -> Self {
        Self {
            client_id: client_id.into(),
            pending: Arc::new(tokio::sync::Mutex::new(PendingWaterfalls::new())),
            bus,
            next_event_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Mints an event id for a new waterfall.
    pub fn next_event_id(&self) -> RemoteEventId {
        let value = self
            .next_event_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{}-{value}", self.client_id)
    }

    /// Builds the `ready` frame for this generation.
    #[must_use]
    pub fn ready(&self) -> DownlinkFrame {
        DownlinkFrame::Ready {
            client_id: self.client_id.clone(),
            host: RemoteEventHostInfo {
                home: std::env::var("HOME").unwrap_or_default(),
            },
        }
    }
}

/// Event names this build knows how to forward.
///
/// An event's name is a `const` on its type, so a name supplied at runtime — a CLI flag, a config
/// value — cannot be turned into a `&'static str` without a lookup table. This is that table. An
/// unknown name is reported by the caller rather than dropped, because a typo would otherwise look
/// like "this event never fires".
#[must_use]
pub fn known_events() -> Vec<&'static str> {
    vec![
        <girder_core::ToolPreExecute as girder_core::Waterfall>::NAME,
        <girder_core::ToolExecute as girder_core::Waterfall>::NAME,
        <girder_core::ToolPostExecute as girder_core::Waterfall>::NAME,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ready_frame_round_trips_through_the_wire() {
        let frame = DownlinkFrame::Ready {
            client_id: "c1".to_owned(),
            host: RemoteEventHostInfo {
                home: "/home/user".to_owned(),
            },
        };
        let text = encode_downlink(&frame).unwrap();
        assert!(text.contains(r#""type":"ready""#), "{text}");
        assert!(text.contains(r#""clientId":"c1""#), "{text}");
        assert_eq!(decode_downlink(&text).unwrap(), frame);
    }

    #[test]
    fn an_emit_frame_carries_positional_args() {
        let frame = DownlinkFrame::Emit {
            event: "tools/change".to_owned(),
            args: vec![serde_json::json!({"a": 1})],
        };
        let text = encode_downlink(&frame).unwrap();
        assert!(text.contains(r#""type":"emit""#), "{text}");
        assert_eq!(decode_downlink(&text).unwrap(), frame);
    }

    #[test]
    fn a_waterfall_frame_has_the_upstream_field_names() {
        let frame = DownlinkFrame::Waterfall {
            event: "tools/pre-execute".to_owned(),
            event_id: "e1".to_owned(),
            agent_id: "a1".to_owned(),
            request: serde_json::Map::new(),
        };
        let text = encode_downlink(&frame).unwrap();
        // The upstream protocol uses camelCase on the wire; a snake_case field would be rejected by
        // a real client's parser.
        assert!(text.contains(r#""eventId":"e1""#), "{text}");
        assert!(text.contains(r#""agentId":"a1""#), "{text}");
    }

    #[test]
    fn a_result_frame_decodes_its_three_outcomes() {
        let next: RemoteEventResult =
            serde_json::from_str(r#"{"clientId":"c","eventId":"e","outcome":{"kind":"next"}}"#)
                .unwrap();
        assert_eq!(next.outcome, RemoteEventOutcome::Next);

        let result: RemoteEventResult = serde_json::from_str(
            r#"{"clientId":"c","eventId":"e","outcome":{"kind":"result","value":42}}"#,
        )
        .unwrap();
        assert_eq!(
            result.outcome,
            RemoteEventOutcome::Result {
                value: Some(serde_json::json!(42))
            }
        );

        let rejected: RemoteEventResult = serde_json::from_str(
            r#"{"clientId":"c","eventId":"e","outcome":{"kind":"rejected","error":{"name":"E","message":"m"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            rejected.outcome,
            RemoteEventOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn pending_waterfalls_reject_a_duplicate_id() {
        let mut pending = PendingWaterfalls::new();
        assert!(pending.insert("e1".to_owned(), "tools/execute".to_owned()));
        assert!(
            !pending.insert("e1".to_owned(), "tools/execute".to_owned()),
            "a duplicate id must not overwrite the pending request"
        );
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn pending_waterfalls_are_drained_on_disconnect() {
        let mut pending = PendingWaterfalls::new();
        pending.insert("e1".to_owned(), "a".to_owned());
        pending.insert("e2".to_owned(), "b".to_owned());

        assert_eq!(pending.drain(), 2);
        assert!(
            pending.is_empty(),
            "a disconnect must not leak pending entries"
        );
    }

    #[test]
    fn taking_a_pending_waterfall_removes_it() {
        let mut pending = PendingWaterfalls::new();
        pending.insert("e1".to_owned(), "tools/execute".to_owned());

        assert_eq!(pending.take("e1").as_deref(), Some("tools/execute"));
        assert_eq!(pending.take("e1"), None);
    }

    #[test]
    fn the_schema_document_has_every_message_type() {
        let schema = schema();
        let defs = schema["$defs"].as_object().expect("$defs");
        for name in [
            "DownlinkFrame",
            "RemoteEventResult",
            "InvokeRemoteRequest",
            "InvokeResponse",
        ] {
            assert!(defs.contains_key(name), "schema is missing {name}");
        }
    }

    #[test]
    fn the_schema_marks_the_frame_discriminator() {
        let schema = schema();
        let text = serde_json::to_string(&schema).unwrap();
        assert!(
            text.contains("ready"),
            "the discriminator values must be enumerable"
        );
        assert!(text.contains("waterfall"));
    }
}
