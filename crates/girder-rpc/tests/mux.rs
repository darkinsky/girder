//! Browser-carrier acceptance tests for the upstream Remote stream mux.

use std::sync::Arc;

use futures::{SinkExt as _, StreamExt as _};
use girder_core::api::extra::{ServerMessage, parse_server_message};
use girder_core::{Event as _, EventBus, RemoteRegistry};
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
