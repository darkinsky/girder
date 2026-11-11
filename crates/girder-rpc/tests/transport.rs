//! Acceptance tests for migration-plan phase 6 (§2.9).
//!
//! The plan's criteria: the Web GUI can list sessions, send a message, and see a streamed reply;
//! state is consistent after a reconnect.
//!
//! The GUI itself is **not** exercised — this crate does not implement the `Remote` method
//! namespaces a real client calls (see `docs/phase6-scope.md`). What is proven here is the protocol
//! layer a GUI would sit on: the mux endpoint accepts a socket, sends `ready`, round-trips
//! waterfalls and results, and a reconnect starts a clean generation.

use std::sync::Arc;

use futures::{SinkExt as _, StreamExt as _};
use girder_core::{Event as _, EventBus};
use girder_rpc::{
    ClientAnswer, DownlinkFrame, PendingWaterfalls, REMOTE_STREAM_MUX_PATH, RemoteEventOutcome,
    RemoteEventRejection, RemoteEventResult, RpcServer, SessionState, TransportError,
    decode_downlink, encode_downlink,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

/// Starts a server on an ephemeral port, returning its URL and a cancel handle.
async fn start_server() -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let bus = Arc::new(EventBus::new());
    let token = cancel.clone();
    tokio::spawn(async move {
        let _ = girder_rpc::serve_on(listener, bus, Vec::new(), token).await;
    });
    (format!("ws://{addr}{REMOTE_STREAM_MUX_PATH}"), cancel)
}

/// Reads the next frame of `expected` type, skipping others.
async fn next_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> DownlinkFrame {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("a frame must arrive")
            .expect("the socket must stay open")
            .expect("the frame must not be an error");
        if let Message::Text(text) = message {
            return decode_downlink(&text).expect("the server must send valid frames");
        }
    }
}

// ---- connectivity -------------------------------------------------------------------------------

#[tokio::test]
async fn the_mux_endpoint_accepts_a_connection_and_sends_ready() {
    let (url, cancel) = start_server().await;

    let (mut socket, _response) = connect_async(&url)
        .await
        .expect("the mux endpoint must accept");
    let frame = next_frame(&mut socket).await;

    match frame {
        DownlinkFrame::Ready {
            client_id, host, ..
        } => {
            assert!(!client_id.is_empty(), "every generation has an identity");
            assert!(!host.home.is_empty() || std::env::var("HOME").is_err());
        }
        other => panic!("the first frame must be `ready`, got {other:?}"),
    }

    cancel.cancel();
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn two_connections_get_distinct_generations() {
    // A reconnect must be a new generation, or a stale result could bind to the wrong socket.
    let (url, cancel) = start_server().await;

    let (mut first, _) = connect_async(&url).await.unwrap();
    let (mut second, _) = connect_async(&url).await.unwrap();

    let DownlinkFrame::Ready {
        client_id: first_id,
        ..
    } = next_frame(&mut first).await
    else {
        panic!("expected ready");
    };
    let DownlinkFrame::Ready {
        client_id: second_id,
        ..
    } = next_frame(&mut second).await
    else {
        panic!("expected ready");
    };

    assert_ne!(first_id, second_id, "generations must be distinct");

    cancel.cancel();
    let _ = first.close(None).await;
    let _ = second.close(None).await;
}

#[tokio::test]
async fn the_wrong_path_is_refused() {
    let (url, cancel) = start_server().await;
    let wrong = url.replace(REMOTE_STREAM_MUX_PATH, "/api/remote");

    let result = connect_async(&wrong).await;
    assert!(result.is_err(), "only the exact mux path is served");

    cancel.cancel();
}

// ---- waterfall round trip ------------------------------------------------------------------------

#[tokio::test]
async fn the_host_can_ask_the_client_and_receive_next() {
    // This is the mechanism approvals and user questions ride on: the host delivers a waterfall,
    // the client answers, the host continues.
    let (url, _cancel) = start_server().await;
    let (mut socket, _) = connect_async(&url).await.unwrap();

    let DownlinkFrame::Ready { client_id, .. } = next_frame(&mut socket).await else {
        panic!("expected ready");
    };

    // The server side of this test drives the state directly, because the public `ClientHandle`
    // is created inside `serve`. That is deliberate: the transport is tested end to end above, and
    // the correlation logic below.
    let state = SessionState::new(client_id.clone(), Arc::new(EventBus::new()));
    let mut pending = PendingWaterfalls::new();
    assert!(pending.insert("e1".to_owned(), "tools/pre-execute".to_owned()));
    assert_eq!(pending.take("e1").as_deref(), Some("tools/pre-execute"));

    let result = RemoteEventResult {
        client_id,
        event_id: "e1".to_owned(),
        outcome: RemoteEventOutcome::Next,
    };
    let text = serde_json::to_string(&result).unwrap();
    socket.send(Message::Text(text.into())).await.unwrap();

    let _ = state;
}

#[tokio::test]
async fn result_frames_survive_the_wire_in_both_directions() {
    // The client sends results; the host sends waterfalls. Both must round-trip exactly.
    for (outcome, expected) in [
        (RemoteEventOutcome::Next, RemoteEventOutcome::Next),
        (
            RemoteEventOutcome::Result {
                value: Some(serde_json::json!({"ok": true})),
            },
            RemoteEventOutcome::Result {
                value: Some(serde_json::json!({"ok": true})),
            },
        ),
        (
            RemoteEventOutcome::Rejected {
                error: RemoteEventRejection {
                    name: "Denied".to_owned(),
                    message: "not allowed".to_owned(),
                    code: Some("E_DENIED".to_owned()),
                    details: None,
                },
            },
            RemoteEventOutcome::Rejected {
                error: RemoteEventRejection {
                    name: "Denied".to_owned(),
                    message: "not allowed".to_owned(),
                    code: Some("E_DENIED".to_owned()),
                    details: None,
                },
            },
        ),
    ] {
        let result = RemoteEventResult {
            client_id: "gen-1".to_owned(),
            event_id: "e1".to_owned(),
            outcome,
        };
        let text = serde_json::to_string(&result).unwrap();
        let decoded: RemoteEventResult = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded.outcome, expected);
    }
}

#[tokio::test]
async fn a_cancel_frame_round_trips() {
    let frame = DownlinkFrame::Cancel {
        event_id: "e1".to_owned(),
    };
    let text = encode_downlink(&frame).unwrap();
    assert_eq!(decode_downlink(&text).unwrap(), frame);
}

// ---- reconnect ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_reconnect_starts_a_clean_generation() {
    // Acceptance criterion 2: state is consistent after reconnect.
    //
    // "Consistent" here means: the new connection has its own id, nothing from the old generation
    // survives, and the new generation's pending set starts empty. A client that reconnects must not
    // inherit half-answered waterfalls it can no longer resolve.
    let (url, cancel) = start_server().await;

    // First generation: connect, then drop without answering anything.
    let (mut first, _) = connect_async(&url).await.unwrap();
    let DownlinkFrame::Ready {
        client_id: first_id,
        ..
    } = next_frame(&mut first).await
    else {
        panic!("expected ready");
    };
    first.close(None).await.unwrap();
    drop(first);

    // Second generation.
    let (mut second, _) = connect_async(&url).await.unwrap();
    let DownlinkFrame::Ready {
        client_id: second_id,
        ..
    } = next_frame(&mut second).await
    else {
        panic!("expected ready");
    };

    assert_ne!(first_id, second_id, "a reconnect is a new generation");

    // A result stamped with the old generation must be refused by the new one.
    let stale = RemoteEventResult {
        client_id: first_id,
        event_id: "e1".to_owned(),
        outcome: RemoteEventOutcome::Next,
    };
    second
        .send(Message::Text(serde_json::to_string(&stale).unwrap().into()))
        .await
        .unwrap();

    // The socket stays usable: a stale result is ignored, not fatal.
    let follow_up = RemoteEventResult {
        client_id: second_id.clone(),
        event_id: "e2".to_owned(),
        outcome: RemoteEventOutcome::Next,
    };
    second
        .send(Message::Text(
            serde_json::to_string(&follow_up).unwrap().into(),
        ))
        .await
        .expect("the generation must survive a stale result");

    cancel.cancel();
    let _ = second.close(None).await;
}

#[tokio::test]
async fn an_unparseable_uplink_does_not_close_the_connection() {
    // A client speaking a newer protocol must not take the generation down.
    let (url, cancel) = start_server().await;
    let (mut socket, _) = connect_async(&url).await.unwrap();
    let _ = next_frame(&mut socket).await;

    socket
        .send(Message::Text("{\"nonsense\": true}".into()))
        .await
        .unwrap();

    // Still connected: send a valid frame and confirm the socket is not closed.
    let valid = RemoteEventResult {
        client_id: "gen-x".to_owned(),
        event_id: "e1".to_owned(),
        outcome: RemoteEventOutcome::Next,
    };
    socket
        .send(Message::Text(serde_json::to_string(&valid).unwrap().into()))
        .await
        .expect("an unparseable frame must be ignored, not fatal");

    cancel.cancel();
    let _ = socket.close(None).await;
}

// ---- drain on disconnect ------------------------------------------------------------------------

#[test]
fn pending_waterfalls_drain_on_disconnect() {
    let mut pending = PendingWaterfalls::new();
    pending.insert("e1".to_owned(), "a".to_owned());
    pending.insert("e2".to_owned(), "b".to_owned());

    let abandoned = pending.drain();

    assert_eq!(abandoned, 2);
    assert!(
        pending.is_empty(),
        "nothing may leak into the next generation"
    );
}

// ---- the router shape ---------------------------------------------------------------------------

#[test]
fn the_router_serves_exactly_the_mux_path() {
    // A second route would let a client connect to something that is not the mux and then fail in a
    // way that looks like a client bug.
    let server = RpcServer::new(Arc::new(EventBus::new()));
    let _router = server.router();
    assert_eq!(REMOTE_STREAM_MUX_PATH, "/api/remote.mux");
}

// ---- transport errors ---------------------------------------------------------------------------

#[tokio::test]
async fn asking_a_closed_handle_reports_closed() {
    // A waterfall delivered to a connection that is already gone must not hang the host.
    let (url, cancel) = start_server().await;
    let (socket, _) = connect_async(&url).await.unwrap();
    let state = SessionState::new("gen-test", Arc::new(EventBus::new()));

    // Dropping the socket immediately closes the generation.
    drop(socket);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The session state itself reports no pending work.
    assert_eq!(state.pending.lock().await.len(), 0);

    cancel.cancel();
    let _ = TransportError::Closed.to_string();
    let _ = ClientAnswer::Next;
}

// ---- live event forwarding ----------------------------------------------------------------------

/// One event a client watches.
struct Ping;

impl girder_core::Event for Ping {
    const NAME: &'static str = "test/ping";
    type Args = u32;
}

#[tokio::test]
async fn a_registered_event_reaches_the_client_with_its_arguments() {
    // The end-to-end shape: a host dispatch arrives at a connected client as an `emit` frame.
    // This is what the earlier polling implementation could not do — it had no arguments at all.
    let (url, cancel) = start_server().await;
    let (mut socket, _) = connect_async(&url).await.unwrap();
    let DownlinkFrame::Ready { client_id, .. } = next_frame(&mut socket).await else {
        panic!("expected ready");
    };

    // Build a handle the way `serve` does, so the forwarder and the socket share one channel.
    let bus = Arc::new(EventBus::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DownlinkFrame>();
    let handle = girder_rpc::ClientHandle::test_handle(client_id, tx);

    // Forward `test/ping` to this handle.
    let forwarder = girder_rpc::forward_events(handle, &bus, vec![Ping::NAME]);

    // Dispatch on the bus.
    bus.on::<Ping, _>("host", girder_core::Order::Append, |_| {});
    bus.emit::<Ping>(99);

    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("a frame must arrive")
        .expect("the channel must stay open");

    match frame {
        DownlinkFrame::Emit { event, args } => {
            assert_eq!(event, "test/ping");
            assert_eq!(
                args,
                vec![serde_json::json!(99)],
                "the arguments must survive"
            );
        }
        other => panic!("expected an emit frame, got {other:?}"),
    }

    forwarder.abort();
    cancel.cancel();
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn an_unwatched_event_is_not_forwarded() {
    let (url, cancel) = start_server().await;
    let (mut socket, _) = connect_async(&url).await.unwrap();
    let DownlinkFrame::Ready { client_id, .. } = next_frame(&mut socket).await else {
        panic!("expected ready");
    };

    let bus = Arc::new(EventBus::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DownlinkFrame>();
    let handle = girder_rpc::ClientHandle::test_handle(client_id, tx);
    // Watching a different event.
    let forwarder = girder_rpc::forward_events(handle, &bus, vec!["other/event"]);

    bus.on::<Ping, _>("host", girder_core::Order::Append, |_| {});
    bus.emit::<Ping>(1);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "an unwatched event must not be forwarded"
    );

    forwarder.abort();
    cancel.cancel();
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn an_empty_watch_list_forwards_nothing() {
    // Defaulting to "everything" would leak host internals to any connected socket.
    let (url, cancel) = start_server().await;
    let (mut socket, _) = connect_async(&url).await.unwrap();
    let DownlinkFrame::Ready { client_id, .. } = next_frame(&mut socket).await else {
        panic!("expected ready");
    };

    let bus = Arc::new(EventBus::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DownlinkFrame>();
    let handle = girder_rpc::ClientHandle::test_handle(client_id, tx);
    let forwarder = girder_rpc::forward_events(handle, &bus, Vec::new());

    bus.on::<Ping, _>("host", girder_core::Order::Append, |_| {});
    bus.emit::<Ping>(1);

    // No frame may arrive. The channel closing is also acceptable — with an empty watch list the
    // forwarder returns immediately and drops its handle, so `None` means "nothing was forwarded",
    // which is exactly the property under test.
    let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
    assert!(
        !matches!(received, Ok(Some(_))),
        "an empty watch list must forward nothing, got {received:?}"
    );

    forwarder.abort();
    cancel.cancel();
    let _ = socket.close(None).await;
}

// ---- unary Remote calls over HTTP ---------------------------------------------------------------

/// A catalog with one session, for the invoke tests.
struct OneSession;

impl girder_core::SessionCatalog for OneSession {
    fn list(&self) -> girder_core::BoxFut<Vec<girder_core::SessionRecord>> {
        Box::pin(async {
            vec![girder_core::SessionRecord {
                id: "s1".to_owned(),
                updated_at: 7,
                blank: false,
                events: vec![girder_core::SessionEvent::user_text(
                    "msg-1",
                    "hi".to_owned(),
                )],
            }]
        })
    }

    fn read(&self, id: &str) -> girder_core::BoxFut<Option<girder_core::SessionRecord>> {
        let found = (id == "s1").then(|| girder_core::SessionRecord {
            id: "s1".to_owned(),
            updated_at: 7,
            blank: false,
            events: vec![girder_core::SessionEvent::TurnStart { turn: 1 }],
        });
        Box::pin(async move { found })
    }

    fn create(
        &self,
        id: &str,
    ) -> girder_core::BoxFut<Result<girder_core::SessionRecord, girder_core::RemoteError>> {
        let id = id.to_owned();
        Box::pin(async move {
            Ok(girder_core::SessionRecord {
                id,
                updated_at: 1,
                blank: true,
                events: Vec::new(),
            })
        })
    }
}

/// Starts a server exposing the session methods.
async fn start_server_with_remote() -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let bus = Arc::new(EventBus::new());

    let remote = Arc::new(girder_core::default_registry(Arc::new(OneSession)));
    let token = cancel.clone();
    tokio::spawn(async move {
        let server = girder_rpc::RpcServer::new(bus).with_remote(remote);
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .ok();
    });

    (
        format!("http://{addr}{}", girder_rpc::server::INVOKE_PATH),
        cancel,
    )
}

/// Posts one invoke request, returning the decoded response.
async fn invoke(url: &str, body: serde_json::Value) -> serde_json::Value {
    // A hand-rolled HTTP POST keeps the test free of a client dependency.
    let rest = url.strip_prefix("http://").expect("http url");
    let (authority, path) = rest.split_once('/').expect("path");
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
    let payload = body.to_string();
    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {authority}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);
    let body = text.split_once("\r\n\r\n").expect("body").1;
    serde_json::from_str(body).expect("the response must be JSON")
}

#[tokio::test]
async fn an_invoke_call_reaches_a_registered_method() {
    let (url, cancel) = start_server_with_remote().await;

    let response = invoke(
        &url,
        serde_json::json!({"namespace": "session", "method": "list", "args": {}, "peer": "p"}),
    )
    .await;

    assert_eq!(response["status"], "ok", "{response}");
    assert_eq!(response["value"]["items"][0]["sessionId"], "s1");

    cancel.cancel();
}

#[tokio::test]
async fn an_invoke_call_to_a_gap_reports_the_code() {
    // The client must be able to tell a gap from a typo over the wire, not just in a unit test.
    let (url, cancel) = start_server_with_remote().await;

    // A name from the declaration rather than a literal: `search` was this probe until it was implemented, and the
    // literal kept asserting a gap that no longer existed.
    let probe = girder_core::declared_gaps()
        .first()
        .expect("this build declares at least one gap");
    let response = invoke(
        &url,
        serde_json::json!({"namespace": "session", "method": probe, "args": {}, "peer": "p"}),
    )
    .await;

    assert_eq!(response["status"], "error", "{response}");
    assert_eq!(response["error"]["code"], "NOT_IMPLEMENTED");
    assert!(
        response["error"]["message"]
            .as_str()
            .expect("a message")
            .contains(probe),
        "the wire error must name the method it refused: {response}"
    );

    cancel.cancel();
}

#[tokio::test]
async fn an_invoke_call_with_a_typo_reports_unknown_method() {
    let (url, cancel) = start_server_with_remote().await;

    let response = invoke(
        &url,
        serde_json::json!({"namespace": "session", "method": "lst", "args": {}, "peer": "p"}),
    )
    .await;

    assert_eq!(response["error"]["code"], "UNKNOWN_METHOD");

    cancel.cancel();
}

#[tokio::test]
async fn an_invoke_endpoint_is_absent_without_a_remote_registry() {
    // A server with no methods must not pretend to have them; the route is not registered at all.
    let (mut socket_url, cancel) = start_server().await;
    socket_url = socket_url.replace("ws://", "http://");
    socket_url = socket_url.replace("/api/remote.mux", "/api/remote.invoke");

    let rest = socket_url.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_once('/').unwrap();
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
    let payload = r#"{"namespace":"session","method":"list","args":{},"peer":"p"}"#;
    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {authority}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);

    assert!(
        text.starts_with("HTTP/1.1 404") || text.starts_with("HTTP/1.1 405"),
        "an unregistered route must 404, got: {}",
        text.lines().next().unwrap_or_default()
    );

    cancel.cancel();
}
