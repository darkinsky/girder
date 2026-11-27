//! The upstream-compatible logical Remote stream mux.
//!
//! The legacy event socket carries `ready`/`emit`/`waterfall` frames. The browser Remote client
//! uses the independent stream protocol instead: one WebSocket owns many logical streams, each
//! opened with `open` and terminated with `end` or `error`. This module keeps that carrier separate
//! from the legacy interaction path while the registry grows stream-aware handlers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use girder_core::api::extra::{
    ClientMessage, Heartbeat, ServerMessage, StreamFailure, StreamTable, UplinkViolation,
    parse_client_message,
};
use girder_core::api::gateway::Peer;
use girder_core::{EventBus, ForwardDelivery, RemoteCall, RemoteError, RemoteRegistry, StreamOpen};
use tokio::sync::mpsc;

use girder_core::api::gateway::Connection;

const STREAM_INBOX_BYTES: u64 = 4 * 1024 * 1024;
/// Upstream's default WebSocket heartbeat interval.
///
/// `Heartbeat` performs the two missed-ping ticks and a verification tick, so a silent peer is
/// closed after roughly three intervals while ordinary Pong/frame activity resets the counter.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

enum Outbound {
    Frame(ServerMessage),
    Ping,
    Pong(Vec<u8>),
    Close { code: u16, reason: &'static str },
}

enum Done {
    Stream(String),
}

struct Running {
    task: tokio::task::JoinHandle<()>,
    cancel: tokio_util::sync::CancellationToken,
}

/// One waterfall waiting for a client answer.
///
/// `client_id` is the generation that currently holds it. `None` means the last generation
/// disconnected before answering, so the next answering `$events` stream can deliver the same frame.
pub(crate) struct PendingEvent {
    client_id: Option<String>,
    frame: serde_json::Value,
}

pub(crate) type PendingEvents = Arc<Mutex<BTreeMap<String, PendingEvent>>>;

/// Serves one browser-compatible Remote stream connection.
///
/// The registry currently exposes unary handlers, so an `open` dispatches one handler and maps its
/// value to a one-item logical stream. The carrier is already stream-shaped: adding true async
/// iterators later changes only the opener, not framing, cancellation, heartbeat, or backpressure.
pub async fn serve(
    socket: WebSocket,
    bus: Arc<EventBus>,
    watch: Arc<Vec<&'static str>>,
    remote: Option<Arc<RemoteRegistry>>,
    connection: Option<Connection>,
    pending: PendingEvents,
) {
    let peer = connection
        .as_ref()
        .map(|connection| connection.peer().clone());
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(256);
    let mut writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            let wire = match message {
                Outbound::Frame(frame) => Message::Text(frame.encode().into()),
                Outbound::Ping => Message::Ping(Vec::new().into()),
                Outbound::Pong(payload) => Message::Pong(payload.into()),
                Outbound::Close { code, reason } => Message::Close(Some(CloseFrame {
                    code,
                    reason: reason.into(),
                })),
            };
            let close = matches!(wire, Message::Close(_));
            if sink.send(wire).await.is_err() || close {
                break;
            }
        }
    });

    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Done>();
    let mut table = StreamTable::new(STREAM_INBOX_BYTES);
    let mut tasks: BTreeMap<String, Running> = BTreeMap::new();
    let mut heartbeat = Heartbeat::new();
    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    ticker.tick().await;

    'connection: loop {
        tokio::select! {
            message = stream.next() => {
                if peer_gone(peer.as_ref()) {
                    let _ = out_tx.send(Outbound::Close {
                        code: 1001,
                        reason: "peer left",
                    }).await;
                    break;
                }
                let Some(message) = message else { break };
                let Ok(message) = message else { break };
                match message {
                    Message::Text(text) => {
                        let text = text.as_str();
                        heartbeat.alive();
                        let parsed = match parse_client_message(text) {
                            Ok(message) => message,
                            Err(_) => {
                                let _ = out_tx.send(Outbound::Close {
                                    code: 1008,
                                    reason: "invalid Remote stream request",
                                }).await;
                                break;
                            }
                        };
                        if !receive(
                            &mut table,
                            &mut tasks,
                            &out_tx,
                            &done_tx,
                            parsed,
                            remote.as_ref(),
                            peer.as_ref(),
                            &bus,
                            &watch,
                            &pending,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        let _ = out_tx.send(Outbound::Close {
                            code: 1003,
                            reason: "text messages required",
                        }).await;
                        break;
                    }
                    Message::Pong(_) => heartbeat.alive(),
                    Message::Ping(payload) => {
                        heartbeat.alive();
                        let _ = out_tx.send(Outbound::Pong(payload.to_vec())).await;
                    }
                    Message::Close(_) => break,
                }
            }
            done = done_rx.recv() => {
                if let Some(Done::Stream(stream_id)) = done {
                    tasks.remove(&stream_id);
                    if let Some(completion) = table.finish(&stream_id, false) {
                        if let Some(violation) = remote_violation(&completion.reason) {
                            let _ = out_tx.send(Outbound::Frame(ServerMessage::Error {
                                stream_id,
                                error: violation.to_failure(),
                            })).await;
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                if peer_gone(peer.as_ref()) {
                    let _ = out_tx.send(Outbound::Close {
                        code: 1001,
                        reason: "peer left",
                    }).await;
                    break 'connection;
                }
                match heartbeat.tick() {
                    girder_core::api::extra::Tick::Ping => {
                        if out_tx.send(Outbound::Ping).await.is_err() {
                            break 'connection;
                        }
                    }
                    girder_core::api::extra::Tick::Verify => {
                        if heartbeat.verify() {
                            let _ = out_tx.send(Outbound::Close {
                                code: 1001,
                                reason: "Remote stream heartbeat timeout",
                            }).await;
                            break 'connection;
                        }
                        heartbeat.alive();
                    }
                }
            }
        }
    }

    for (_, running) in tasks {
        running.cancel.cancel();
        running.task.abort();
    }
    if let Some(connection) = connection {
        connection.release();
    }
    drop(out_tx);
    if tokio::time::timeout(std::time::Duration::from_secs(1), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}

async fn receive(
    table: &mut StreamTable,
    tasks: &mut BTreeMap<String, Running>,
    out_tx: &mpsc::Sender<Outbound>,
    done_tx: &mpsc::UnboundedSender<Done>,
    message: ClientMessage,
    remote: Option<&Arc<RemoteRegistry>>,
    peer: Option<&Peer>,
    bus: &Arc<EventBus>,
    watch: &[&'static str],
    pending: &PendingEvents,
) -> bool {
    let stream_id = message.stream_id().to_owned();
    if matches!(message, ClientMessage::Open { .. }) {
        if table.receive(&message, 0) == girder_core::api::extra::Reception::Duplicate {
            let _ = out_tx
                .send(Outbound::Close {
                    code: 1008,
                    reason: "duplicate Remote stream id",
                })
                .await;
            return false;
        }
        let ClientMessage::Open {
            endpoint, payload, ..
        } = message
        else {
            unreachable!("the open branch established this shape");
        };
        if endpoint == "$events" {
            if peer.is_some_and(|peer| peer.require_interactions("$events").is_err()) {
                send_failure(
                    out_tx,
                    &stream_id,
                    "REFUSED",
                    "Remote event interactions are unavailable",
                    "$events",
                )
                .await;
                let _ = done_tx.send(Done::Stream(stream_id));
                return true;
            }
            if payload != serde_json::json!({"args": {}}) {
                send_failure(
                    out_tx,
                    &stream_id,
                    "INVALID_ARGUMENTS",
                    "Remote event stream expects args: {}",
                    "$events",
                )
                .await;
                let _ = done_tx.send(Done::Stream(stream_id));
                return true;
            }
            let selected = event_names(watch);
            let mut observed = bus.observe(Some(selected));
            let client_id = next_event_client_id();
            let wire_stream_id = stream_id.clone();
            let owner = peer.is_none_or(|peer| peer.principal().is_owner());
            let out_tx = out_tx.clone();
            let done_tx = done_tx.clone();
            let pending = Arc::clone(pending);
            let task = tokio::spawn(async move {
                let _detach = DetachClient {
                    pending: Arc::clone(&pending),
                    client_id: client_id.clone(),
                };
                let ready = serde_json::json!({
                    "type": "ready",
                    "clientId": client_id,
                    "host": {
                        "home": std::env::var("HOME").unwrap_or_default(),
                    },
                });
                if out_tx
                    .send(Outbound::Frame(ServerMessage::Item {
                        stream_id: wire_stream_id.clone(),
                        value: Some(ready),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
                if owner {
                    for frame in replay_detached(&pending, &client_id) {
                        if out_tx
                            .send(Outbound::Frame(ServerMessage::Item {
                                stream_id: wire_stream_id.clone(),
                                value: Some(frame),
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                let mut sequence = 0_u64;
                while let Some(event) = observed.recv().await {
                    let Some(value) =
                        forward_event(&pending, &client_id, &mut sequence, &event, owner)
                    else {
                        continue;
                    };
                    if out_tx
                        .send(Outbound::Frame(ServerMessage::Item {
                            stream_id: wire_stream_id.clone(),
                            value: Some(value),
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = done_tx.send(Done::Stream(wire_stream_id));
            });
            tasks.insert(
                stream_id,
                Running {
                    task,
                    cancel: tokio_util::sync::CancellationToken::new(),
                },
            );
            return true;
        }
        if endpoint == "$events/result" {
            match settle_result(pending, &payload) {
                Ok(()) => {
                    let _ = out_tx
                        .send(Outbound::Frame(ServerMessage::Item {
                            stream_id: stream_id.clone(),
                            value: Some(serde_json::Value::Null),
                        }))
                        .await;
                    let _ = out_tx
                        .send(Outbound::Frame(ServerMessage::End {
                            stream_id: stream_id.clone(),
                        }))
                        .await;
                }
                Err(message) => {
                    send_failure(
                        out_tx,
                        &stream_id,
                        "gateway/protocol",
                        &message,
                        "$events/result",
                    )
                    .await;
                }
            }
            let _ = table.finish(&stream_id, false);
            return true;
        }
        let Some(remote) = remote.cloned() else {
            send_failure(
                out_tx,
                &stream_id,
                "NOT_IMPLEMENTED",
                "Remote registry is unavailable",
                &endpoint,
            )
            .await;
            let _ = done_tx.send(Done::Stream(stream_id));
            return true;
        };
        let Some((namespace, method)) = endpoint.split_once('/') else {
            send_failure(
                out_tx,
                &stream_id,
                "INVALID_ARGUMENTS",
                "Remote endpoint must be namespace/method",
                &endpoint,
            )
            .await;
            let _ = done_tx.send(Done::Stream(stream_id));
            return true;
        };
        let namespace = namespace.to_owned();
        let method = method.to_owned();
        let stream_id_for_task = stream_id.clone();
        let out_tx = out_tx.clone();
        let done_tx = done_tx.clone();
        let call = RemoteCall {
            args: business_args(payload),
            peer: peer.map_or_else(String::new, |peer| peer.id().as_str().to_owned()),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = if remote.is_streaming(&namespace, &method) {
            let Some(uplink) = table.inbox(&stream_id) else {
                send_failure(
                    &out_tx,
                    &stream_id,
                    "NOT_IMPLEMENTED",
                    "Remote stream inbox is unavailable",
                    &endpoint,
                )
                .await;
                let _ = done_tx.send(Done::Stream(stream_id));
                return true;
            };
            match remote.stream_method(&namespace, &method, &call) {
                Ok(handler) => {
                    let cancel_for_open = cancel.clone();
                    tokio::spawn(async move {
                        let (open, mut items) =
                            StreamOpen::pair(call, uplink, cancel_for_open.clone());
                        let producer = tokio::spawn(async move {
                            handler(open).await;
                        });
                        loop {
                            tokio::select! {
                                () = cancel_for_open.cancelled() => {
                                    producer.abort();
                                    return;
                                }
                                item = items.recv() => {
                                    match item {
                                        Some(Ok(value)) => {
                                            if out_tx
                                                .send(Outbound::Frame(ServerMessage::Item {
                                                    stream_id: stream_id_for_task.clone(),
                                                    value: Some(value),
                                                }))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Some(Err(error)) => {
                                            send_remote_error(
                                                &out_tx,
                                                &stream_id_for_task,
                                                error,
                                                &namespace,
                                                &method,
                                            )
                                            .await;
                                            producer.abort();
                                            let _ = done_tx.send(Done::Stream(stream_id_for_task));
                                            return;
                                        }
                                        None => {
                                            let _ = out_tx
                                                .send(Outbound::Frame(ServerMessage::End {
                                                    stream_id: stream_id_for_task.clone(),
                                                }))
                                                .await;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        producer.abort();
                        let _ = done_tx.send(Done::Stream(stream_id_for_task));
                    })
                }
                Err(error) => {
                    send_remote_error(&out_tx, &stream_id, error, &namespace, &method).await;
                    let _ = done_tx.send(Done::Stream(stream_id));
                    return true;
                }
            }
        } else {
            tokio::spawn(async move {
                match remote.dispatch(&namespace, &method, call).await {
                    Ok(value) => {
                        let _ = out_tx
                            .send(Outbound::Frame(ServerMessage::Item {
                                stream_id: stream_id_for_task.clone(),
                                value: Some(value),
                            }))
                            .await;
                        let _ = out_tx
                            .send(Outbound::Frame(ServerMessage::End {
                                stream_id: stream_id_for_task.clone(),
                            }))
                            .await;
                    }
                    Err(error) => {
                        send_remote_error(&out_tx, &stream_id_for_task, error, &namespace, &method)
                            .await;
                    }
                }
                let _ = done_tx.send(Done::Stream(stream_id_for_task));
            })
        };
        tasks.insert(stream_id, Running { task, cancel });
        return true;
    }

    let reception = table.receive(
        &message,
        serde_json::to_string(&message_to_value(&message)).map_or(0, |text| text.len() as u64),
    );
    if matches!(message, ClientMessage::Cancel { .. }) {
        if let Some(running) = tasks.remove(&stream_id) {
            running.cancel.cancel();
            running.task.abort();
        }
        let _ = table.finish(&stream_id, false);
        return true;
    }
    let violation = table.abort_reason(&stream_id).and_then(|reason| {
        if let girder_core::api::extra::AbortReason::Remote(violation) = reason {
            Some(violation.to_failure())
        } else {
            None
        }
    });
    if let Some(error) = violation {
        if let Some(running) = tasks.remove(&stream_id) {
            running.cancel.cancel();
            running.task.abort();
        }
        let _ = out_tx
            .send(Outbound::Frame(ServerMessage::Error {
                stream_id: stream_id.clone(),
                error,
            }))
            .await;
        let _ = table.finish(&stream_id, false);
        return true;
    }
    match reception {
        girder_core::api::extra::Reception::Routed
        | girder_core::api::extra::Reception::Dropped => true,
        girder_core::api::extra::Reception::Duplicate
        | girder_core::api::extra::Reception::Opened => true,
    }
}

/// The method arguments inside a Gateway Remote payload.
///
/// The browser sends `{ args: { ...fields } }` and the host reads the inner object. A payload that
/// is already the argument object stays as it is, so an older caller that opened a stream with the
/// fields at the top level still reaches the method.
pub(crate) fn business_args(payload: serde_json::Value) -> serde_json::Value {
    let Some(object) = payload.as_object() else {
        return payload;
    };
    if object.len() == 1
        && let Some(args) = object.get("args").filter(|value| value.is_object())
    {
        return args.clone();
    }
    payload
}

fn event_names(watch: &[&'static str]) -> Vec<&'static str> {
    let allowlisted = girder_core::forwarded_event_names();
    let selected: Vec<&'static str> = allowlisted
        .iter()
        .copied()
        .filter(|name| watch.contains(name))
        .collect();
    selected
}

fn forward_event(
    pending: &PendingEvents,
    client_id: &str,
    sequence: &mut u64,
    event: &girder_core::ObservedEvent,
    owner: bool,
) -> Option<serde_json::Value> {
    let entry = girder_core::forwarded_event(event.name)?;
    let args = match &event.args {
        serde_json::Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    girder_core::validate_args(event.name, &args).ok()?;
    let resources = girder_core::resources_of(event.name, &args).ok()?;
    if !owner && resources.iter().any(|resource| !resource.is_global()) {
        return None;
    }
    match entry.delivery() {
        ForwardDelivery::Emit => Some(serde_json::json!({
            "type": "emit",
            "event": event.name,
            "args": args,
        })),
        ForwardDelivery::Waterfall => {
            let request = args.first()?;
            let agent = request.get(girder_core::AGENT_FIELD)?.as_str()?;
            let projected = girder_core::project_request(event.name, agent, request).ok()?;
            *sequence = sequence.saturating_add(1);
            let event_id = format!("{client_id}-{sequence}");
            let frame = serde_json::json!({
                "type": "waterfall",
                "event": event.name,
                "eventId": event_id,
                "agentId": projected.agent_id(),
                "request": projected.request(),
            });
            pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(
                    event_id,
                    PendingEvent {
                        client_id: Some(client_id.to_owned()),
                        frame: frame.clone(),
                    },
                );
            Some(frame)
        }
    }
}

pub(crate) fn settle_result(
    pending: &PendingEvents,
    payload: &serde_json::Value,
) -> Result<(), String> {
    let args = payload
        .get("args")
        .ok_or_else(|| "Remote event result requires args".to_owned())?;
    let client_id = args
        .get("clientId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Remote event result requires clientId".to_owned())?;
    let event_id = args
        .get("eventId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Remote event result requires eventId".to_owned())?;
    let kind = args
        .get("outcome")
        .and_then(|outcome| outcome.get("kind"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Remote event result requires an outcome".to_owned())?;
    if !matches!(kind, "next" | "result" | "rejected") {
        return Err("Remote event result outcome is not recognised".to_owned());
    }
    let mut pending = pending.lock().unwrap_or_else(|error| error.into_inner());
    let Some(record) = pending.get(event_id) else {
        // Settlement and reconnect can race. A result for an event that already
        // finished is an idempotent success.
        return Ok(());
    };
    if record.client_id.as_deref() != Some(client_id) {
        // The frame now belongs to a newer generation. The old result must not
        // remove it or fail the call.
        return Ok(());
    }
    pending.remove(event_id);
    Ok(())
}

fn peer_gone(peer: Option<&Peer>) -> bool {
    peer.is_some_and(|peer| !peer.is_live())
}

fn next_event_client_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "events-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

struct DetachClient {
    pending: PendingEvents,
    client_id: String,
}

impl Drop for DetachClient {
    fn drop(&mut self) {
        detach_client(&self.pending, &self.client_id);
    }
}

fn detach_client(pending: &PendingEvents, client_id: &str) {
    for record in pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values_mut()
    {
        if record.client_id.as_deref() == Some(client_id) {
            record.client_id = None;
        }
    }
}

fn replay_detached(pending: &PendingEvents, client_id: &str) -> Vec<serde_json::Value> {
    let mut frames = Vec::new();
    for record in pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values_mut()
    {
        if record.client_id.is_none() {
            record.client_id = Some(client_id.to_owned());
            frames.push(record.frame.clone());
        }
    }
    frames
}

fn message_to_value(message: &ClientMessage) -> serde_json::Value {
    match message {
        ClientMessage::Item { stream_id, value } => {
            let mut map = serde_json::Map::new();
            map.insert(
                "type".to_owned(),
                serde_json::Value::String("item".to_owned()),
            );
            map.insert(
                "streamId".to_owned(),
                serde_json::Value::String(stream_id.clone()),
            );
            if let Some(value) = value {
                map.insert("value".to_owned(), value.clone());
            }
            serde_json::Value::Object(map)
        }
        ClientMessage::End { stream_id } => {
            serde_json::json!({"type": "end", "streamId": stream_id})
        }
        ClientMessage::Cancel { stream_id } => {
            serde_json::json!({"type": "cancel", "streamId": stream_id})
        }
        ClientMessage::Open { .. } => serde_json::Value::Null,
    }
}

async fn send_failure(
    out_tx: &mpsc::Sender<Outbound>,
    stream_id: &str,
    code: &str,
    message: &str,
    endpoint: &str,
) {
    let mut details = serde_json::Map::new();
    details.insert(
        "endpoint".to_owned(),
        serde_json::Value::String(endpoint.to_owned()),
    );
    let _ = out_tx
        .send(Outbound::Frame(ServerMessage::Error {
            stream_id: stream_id.to_owned(),
            error: StreamFailure::new(code, message, details),
        }))
        .await;
}

async fn send_remote_error(
    out_tx: &mpsc::Sender<Outbound>,
    stream_id: &str,
    error: RemoteError,
    namespace: &str,
    method: &str,
) {
    let mut details = serde_json::Map::new();
    details.insert(
        "endpoint".to_owned(),
        serde_json::Value::String(format!("{namespace}/{method}")),
    );
    let _ = out_tx
        .send(Outbound::Frame(ServerMessage::Error {
            stream_id: stream_id.to_owned(),
            error: StreamFailure::new(error.wire_code(), error.to_string(), details),
        }))
        .await;
}

fn remote_violation(reason: &girder_core::api::extra::AbortReason) -> Option<&UplinkViolation> {
    match reason {
        girder_core::api::extra::AbortReason::Remote(violation) => Some(violation),
        girder_core::api::extra::AbortReason::NotAborted
        | girder_core::api::extra::AbortReason::Local => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingEvent, PendingEvents, detach_client, replay_detached, settle_result};

    fn pending_with(id: &str, client_id: Option<&str>) -> (String, PendingEvent) {
        let frame = serde_json::json!({
            "type": "waterfall",
            "event": "approval/request",
            "eventId": id,
            "agentId": "agent-1",
            "request": {"prompt": "ship?"},
        });
        (
            id.to_owned(),
            PendingEvent {
                client_id: client_id.map(str::to_owned),
                frame,
            },
        )
    }

    #[test]
    fn a_disconnected_generation_releases_its_waterfall_for_the_next_one() {
        let (first_id, first) = pending_with("events-1-1", Some("events-1"));
        let (other_id, other) = pending_with("events-2-1", Some("events-2"));
        let pending: PendingEvents =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::from([
                (first_id, first),
                (other_id, other),
            ])));

        detach_client(&pending, "events-1");
        let replayed = replay_detached(&pending, "events-3");
        settle_result(
            &pending,
            &serde_json::json!({
                "args": {
                    "clientId": "events-1",
                    "eventId": "events-1-1",
                    "outcome": {"kind": "next"},
                }
            }),
        )
        .expect("a superseded generation is ignored");
        assert!(
            pending
                .lock()
                .expect("pending lock")
                .contains_key("events-1-1"),
            "the old result must leave the replayed waterfall in place"
        );

        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0]["eventId"], "events-1-1");
        settle_result(
            &pending,
            &serde_json::json!({
                "args": {
                    "clientId": "events-3",
                    "eventId": "events-1-1",
                    "outcome": {"kind": "next"},
                }
            }),
        )
        .expect("the new generation answers the replayed waterfall");
        let remaining = pending.lock().expect("pending lock");
        assert!(!remaining.contains_key("events-1-1"));
        assert!(remaining.contains_key("events-2-1"));
    }
}
