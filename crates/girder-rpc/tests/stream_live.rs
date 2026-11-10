//! End-to-end test for live event forwarding.
//!
//! The mux and the event bus were both tested separately; nothing tested them together. This runs a
//! real WebSocket client against a real server, dispatches an event, and asserts the frame arrives.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use girder_core::{AgentTurn, EventBus};
use tokio_util::sync::CancellationToken;

/// Starts a server forwarding one event and returns its address.
async fn serve(bus: Arc<EventBus>) -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let watched = vec![<AgentTurn as girder_core::Event>::NAME];

    tokio::spawn(async move {
        let _ = girder_rpc::serve_on(listener, bus, watched, stop).await;
    });

    (address.to_string(), cancel)
}

#[tokio::test]
async fn a_dispatched_event_reaches_a_connected_client() {
    // The property: a browser that connected sees what the host dispatched. Without it a client
    // shows nothing while the model answers.
    let bus = Arc::new(EventBus::new());
    let (address, cancel) = serve(Arc::clone(&bus)).await;

    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/remote.mux"))
            .await
            .expect("connect");

    // The host sends a `ready` frame first; read past it so the assertion is about the event.
    let ready = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("a frame within the bound")
        .expect("a frame")
        .expect("a valid frame");
    let ready = ready.into_text().expect("text");
    assert!(ready.contains("ready"), "{ready}");

    // Dispatch after the client is connected: forwarding is subscribed at connect time, so an event
    // dispatched before would legitimately be missed.
    bus.emit::<AgentTurn>(girder_core::AgentEvent::TextDelta {
        session_id: "s1".to_owned(),
        text: "hello".to_owned(),
    });

    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the event within the bound")
        .expect("a frame")
        .expect("a valid frame");
    let frame = frame.into_text().expect("text");

    assert!(frame.contains("emit"), "{frame}");
    assert!(frame.contains("agent/turn"), "{frame}");
    assert!(frame.contains("hello"), "{frame}");

    socket.close(None).await.ok();
    cancel.cancel();
}

#[tokio::test]
async fn a_client_that_connects_receives_the_ready_frame() {
    // The handshake: a client that did not know its generation could not correlate later frames.
    let bus = Arc::new(EventBus::new());
    let (address, cancel) = serve(bus).await;

    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/remote.mux"))
            .await
            .expect("connect");

    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("a frame within the bound")
        .expect("a frame")
        .expect("a valid frame");
    let frame = frame.into_text().expect("text");

    assert!(frame.contains("ready"), "{frame}");
    assert!(frame.contains("clientId"), "{frame}");

    socket.close(None).await.ok();
    cancel.cancel();
}

#[tokio::test]
async fn a_server_watching_nothing_forwards_no_events() {
    // The filter is what keeps a deployment from flooding a client that asked for nothing. A server
    // started with an empty watch list sends only the handshake.
    let bus = Arc::new(EventBus::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let bus_for_server = Arc::clone(&bus);

    tokio::spawn(async move {
        let _ = girder_rpc::serve_on(listener, bus_for_server, Vec::new(), stop).await;
    });

    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/remote.mux"))
            .await
            .expect("connect");
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.next()).await;

    bus.emit::<AgentTurn>(girder_core::AgentEvent::TextDelta {
        session_id: "s1".to_owned(),
        text: "unwatched".to_owned(),
    });

    let next = tokio::time::timeout(Duration::from_millis(500), socket.next()).await;
    assert!(next.is_err(), "an unwatched server must forward nothing");

    socket.close(None).await.ok();
    cancel.cancel();
}
