//! Browser-carrier acceptance tests for the upstream Remote stream mux.

use std::sync::Arc;

use futures::{SinkExt as _, StreamExt as _};
use girder_core::api::extra::{ServerMessage, parse_server_message};
use girder_core::{Event as _, EventBus, RemoteError, RemoteRegistry};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest as _},
};
use tokio_util::sync::CancellationToken;

struct Ping;

impl girder_core::Event for Ping {
    const NAME: &'static str = "test/ping";
    type Args = u32;
}

struct CommandsChange;

impl girder_core::Event for CommandsChange {
    const NAME: &'static str = "commands/change";
    type Args = ();
}

struct Approval;

impl girder_core::Event for Approval {
    const NAME: &'static str = "approval/request";
    type Args = serde_json::Value;
}

#[tokio::test]
async fn a_browser_mux_open_returns_an_item_and_end() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let mut registry = RemoteRegistry::new();
    registry.register("probe", "echo", |call| {
        Box::pin(async move { Ok(call.args) })
    });
    let remote = Arc::new(registry);
    let token = cancel.clone();
    tokio::spawn(async move {
        let server = girder_rpc::RpcServer::new(Arc::new(EventBus::new())).with_remote(remote);
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .unwrap();
    });

    let url = format!("ws://{addr}/api/remote.mux");
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("user-agent", "Mozilla/5.0".parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&serde_json::json!({
                "type": "open",
                "streamId": "stream-1",
                "endpoint": "probe/echo",
                "payload": {"value": 7}
            }))
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();

    let first = next_server_message(&mut socket).await;
    assert_eq!(
        first,
        ServerMessage::Item {
            stream_id: "stream-1".to_owned(),
            value: Some(serde_json::json!({"value": 7})),
        }
    );
    assert_eq!(
        next_server_message(&mut socket).await,
        ServerMessage::End {
            stream_id: "stream-1".to_owned()
        }
    );
    socket
        .send(Message::Text(
            serde_json::to_string(&serde_json::json!({
                "type": "open",
                "streamId": "stream-2",
                "endpoint": "probe/echo",
                "payload": {"args": {"value": 8}}
            }))
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        next_server_message(&mut socket).await,
        ServerMessage::Item {
            stream_id: "stream-2".to_owned(),
            value: Some(serde_json::json!({"value": 8})),
        }
    );

    cancel.cancel();
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn a_streaming_remote_yields_each_uplink_item() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let mut registry = RemoteRegistry::new();
    registry.register_stream("probe", "follow", |open| {
        Box::pin(async move {
            if !open.push(serde_json::json!("ready")) {
                return;
            }
            loop {
                tokio::select! {
                    () = open.cancel.cancelled() => return,
                    item = open.uplink.recv() => {
                        match item {
                            Ok(Some(value)) => {
                                if !open.push(value) {
                                    return;
                                }
                            }
                            Ok(None) => return,
                            Err(error) => {
                                let _ = open.fail(RemoteError::Failed(error.to_string()));
                                return;
                            }
                        }
                    }
                }
            }
        })
    });
    let remote = Arc::new(registry);
    let token = cancel.clone();
    tokio::spawn(async move {
        let server = girder_rpc::RpcServer::new(Arc::new(EventBus::new())).with_remote(remote);
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .unwrap();
    });

    let url = format!("ws://{addr}/api/remote.mux");
    let (mut socket, _) = connect_async(url).await.unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&serde_json::json!({
                "type": "open",
                "streamId": "follow-1",
                "endpoint": "probe/follow",
                "payload": {}
            }))
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        next_server_message(&mut socket).await,
        ServerMessage::Item {
            stream_id: "follow-1".to_owned(),
            value: Some(serde_json::json!("ready")),
        }
    );
    socket
        .send(Message::Text(
            serde_json::to_string(&serde_json::json!({
                "type": "item",
                "streamId": "follow-1",
                "value": {"n": 1}
            }))
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        next_server_message(&mut socket).await,
        ServerMessage::Item {
            stream_id: "follow-1".to_owned(),
            value: Some(serde_json::json!({"n": 1})),
        }
    );
    socket
        .send(Message::Text(
            serde_json::to_string(&serde_json::json!({
                "type": "end",
                "streamId": "follow-1"
            }))
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        next_server_message(&mut socket).await,
        ServerMessage::End {
            stream_id: "follow-1".to_owned()
        }
    );

    cancel.cancel();
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn connection_rpc_envelope_dispatches_generated_endpoint() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let mut registry = RemoteRegistry::new();
    registry.register("probe", "echo", |call| {
        Box::pin(async move { Ok(call.args) })
    });
    let remote = Arc::new(registry);
    let token = cancel.clone();
    tokio::spawn(async move {
        let server = girder_rpc::RpcServer::new(Arc::new(EventBus::new())).with_remote(remote);
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .unwrap();
    });

    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": "rpc-1",
        "method": "probe/echo",
        "payload": {"value": 9},
    })
    .to_string();
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(
            format!(
                "POST /api/probe/echo HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let body = response.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        serde_json::json!({
            "type": "server-response",
            "rpcId": "rpc-1",
            "result": {"ok": true, "value": {"value": 9}},
        })
    );

    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": "rpc-2",
        "method": "probe/echo",
        "payload": {"args": {"value": 4}},
    })
    .to_string();
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(
            format!(
                "POST /api/probe/echo HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let body = response.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        serde_json::json!({
            "type": "server-response",
            "rpcId": "rpc-2",
            "result": {"ok": true, "value": {"value": 4}},
        })
    );
    cancel.cancel();
}

#[tokio::test]
async fn the_events_stream_starts_with_ready_and_forwards_allowlisted_events() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let bus = Arc::new(girder_core::EventBus::new());
    let token = cancel.clone();
    let server_bus = Arc::clone(&bus);
    tokio::spawn(async move {
        let server = girder_rpc::RpcServer::new(server_bus).watching(vec![CommandsChange::NAME]);
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .unwrap();
    });

    let url = format!("ws://{addr}/api/remote.mux");
    let (mut socket, _) = connect_async(url).await.unwrap();
    socket
        .send(Message::Text(
            r#"{"type":"open","streamId":"events","endpoint":"$events","payload":{"args":{}}}"#
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        next_server_message(&mut socket).await,
        ServerMessage::Item {
            stream_id: "events".to_owned(),
            value: Some(serde_json::json!({
                "type": "ready",
                "clientId": "events",
                "host": {"home": std::env::var("HOME").unwrap_or_default()},
            })),
        }
    );

    bus.on::<Ping, _>("test", girder_core::Order::Append, |_| {});
    bus.emit::<Ping>(7);
    bus.emit::<CommandsChange>(());
    assert_eq!(
        next_server_message(&mut socket).await,
        ServerMessage::Item {
            stream_id: "events".to_owned(),
            value: Some(serde_json::json!({
                "type": "emit",
                "event": "commands/change",
                "args": [serde_json::Value::Null],
            })),
        }
    );

    cancel.cancel();
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn a_waterfall_event_is_settled_by_events_result() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let bus = Arc::new(girder_core::EventBus::new());
    let token = cancel.clone();
    let server_bus = Arc::clone(&bus);
    tokio::spawn(async move {
        let server = girder_rpc::RpcServer::new(server_bus).watching(vec![Approval::NAME]);
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .unwrap();
    });

    let (mut socket, _) = connect_async(format!("ws://{addr}/api/remote.mux"))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            r#"{"type":"open","streamId":"events","endpoint":"$events","payload":{"args":{}}}"#
                .into(),
        ))
        .await
        .unwrap();
    let _ = next_server_message(&mut socket).await;
    bus.emit::<Approval>(serde_json::json!({"agent": "agent-1", "prompt": "ship?"}));
    assert_eq!(
        next_server_message(&mut socket).await,
        ServerMessage::Item {
            stream_id: "events".to_owned(),
            value: Some(serde_json::json!({
                "type": "waterfall",
                "event": "approval/request",
                "eventId": "events-1",
                "agentId": "agent-1",
                "request": {"prompt": "ship?"},
            })),
        }
    );
    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": "rpc-events-1",
        "method": "$events/result",
        "payload": {
            "args": {
                "clientId": "events",
                "eventId": "events-1",
                "outcome": {"kind": "next"},
            },
        },
    })
    .to_string();
    let mut http = tokio::net::TcpStream::connect(addr).await.unwrap();
    http.write_all(
        format!(
            "POST /api/$events/result HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut response = Vec::new();
    http.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let response_body = response.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(response_body).unwrap(),
        serde_json::json!({
            "type": "server-response",
            "rpcId": "rpc-events-1",
            "result": {"ok": true, "value": null},
        })
    );

    cancel.cancel();
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn connect_publishes_an_operator_and_gates_events_on_interactions() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let directory = Arc::new(girder_core::api::gateway::PeerDirectory::new());
    let auth = Arc::new(girder_rpc::BrowserAuth::new(
        "launch-token",
        [9; 32],
        std::time::Duration::from_secs(60),
    ));
    let token = cancel.clone();
    let server_directory = Arc::clone(&directory);
    let server_auth = Arc::clone(&auth);
    tokio::spawn(async move {
        let server = girder_rpc::RpcServer::new(Arc::new(EventBus::new()))
            .with_peer_directory(server_directory)
            .with_browser_auth(server_auth);
        axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .unwrap();
    });

    let refused = http_get(
        addr,
        &format!("/connect?token=launch-token&interactions=observe"),
    )
    .await;
    assert!(refused.starts_with("HTTP/1.1 400"), "{refused}");
    assert!(directory.is_empty());

    let forbidden = http_get_host(addr, "/connect?token=launch-token", "untrusted.example").await;
    assert!(forbidden.starts_with("HTTP/1.1 403"), "{forbidden}");

    let response = http_get(addr, "/connect?token=launch-token").await;
    assert!(response.contains("302"), "{response}");
    assert!(
        response
            .to_ascii_lowercase()
            .contains("location: ./api/remote.mux")
    );
    let cookie = set_cookie_pair(&response);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("host", format!("{addr}").parse().unwrap());
    headers.insert("cookie", cookie.parse().unwrap());
    let peer_id = auth
        .authenticate_api(&headers, false, now_ms())
        .expect("operator cookie");
    let peer = directory
        .peer(&girder_core::api::gateway::PeerId::new(&peer_id))
        .expect("published operator");
    assert_eq!(
        peer.interactions(),
        girder_core::api::gateway::Interactions::None
    );

    let url = format!("ws://{addr}/api/remote.mux");
    let mut request = url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("cookie", cookie.parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            r#"{"type":"open","streamId":"events","endpoint":"$events","payload":{"args":{}}}"#
                .into(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        next_server_message(&mut socket).await,
        ServerMessage::Error { .. }
    ));
    let _ = socket.close(None).await;

    let answering = http_get(addr, "/connect?token=launch-token&interactions=answer").await;
    let answering_cookie = set_cookie_pair(&answering);
    let mut answering_headers = axum::http::HeaderMap::new();
    answering_headers.insert("host", format!("{addr}").parse().unwrap());
    answering_headers.insert("cookie", answering_cookie.parse().unwrap());
    let answering_id = auth
        .authenticate_api(&answering_headers, false, now_ms())
        .expect("answering operator");
    assert_ne!(answering_id, peer_id);
    assert_eq!(
        directory
            .peer(&girder_core::api::gateway::PeerId::new(answering_id))
            .unwrap()
            .interactions(),
        girder_core::api::gateway::Interactions::Answer
    );

    cancel.cancel();
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    http_get_host(addr, path, &addr.to_string()).await
}

async fn http_get_host(addr: std::net::SocketAddr, path: &str, host: &str) -> String {
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await.unwrap();
    String::from_utf8(response).unwrap()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn set_cookie_pair(response: &str) -> String {
    response
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("set-cookie")
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn next_server_message(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> ServerMessage {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(text) = message {
            return parse_server_message(&text).unwrap();
        }
    }
}
