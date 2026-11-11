//! End-to-end tests for subscriptions over the mux socket.
//!
//! The unit tests in `subscriptions.rs` cover the bookkeeping: ids, limits, frame shape. What they
//! cannot cover is whether a client on a real socket ever *receives* anything, which is the property
//! that was missing — the subscription types existed and nothing constructed them.
//!
//! These run a real server, connect a real WebSocket, and speak the wire protocol as a client does.
//! The implementation is poll-driven (see `stream.rs`), so every test sets a short interval and
//! bounds its waits: a test that would hang forever on a missing frame reports a failure instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use girder_core::{RemoteCall, RemoteRegistry};
use girder_rpc::{
    DEFAULT_SUBSCRIPTION_LIMIT, DownlinkFrame, EndReason, SubscriptionFrame, decode_downlink,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Builds a registry with one pollable method that returns an increasing counter.
///
/// The counter is what makes "a frame arrived" mean something: a method returning a constant would
/// pass a test that only counted frames, because the hub could send the same value forever.
fn counting_registry() -> Arc<RemoteRegistry> {
    let mut registry = RemoteRegistry::new();

    // A subscribable method that produces one value and never claims to be done.
    registry.register("job", "follow", |_call: RemoteCall| {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Box::pin(async move {
            let seq = NEXT.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(serde_json::json!({ "at": seq, "text": format!("chunk-{seq}") }))
        })
    });

    // A subscribable method that ends after three values, to pin the `Completed` end frame.
    registry.register("session", "follow", |_call: RemoteCall| {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Box::pin(async move {
            let seq = NEXT.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(serde_json::json!({ "at": seq, "done": seq >= 3 }))
        })
    });

    // A method that fails, to pin the `Failed` end frame.
    registry.register("session", "observeEvent", |_call: RemoteCall| {
        Box::pin(async move {
            Err(girder_core::RemoteError::Failed(
                "the job was deleted".to_owned(),
            ))
        })
    });

    // Unary methods, used to prove a subscription to one is refused.
    registry.register("job", "list", |_call: RemoteCall| {
        Box::pin(async move { Ok(serde_json::json!({ "jobs": [] })) })
    });

    Arc::new(registry)
}

/// Counts how many times a method was invoked, so "unsubscribed stops polling" is observable.
static UNARY_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Starts a server exposing the test registry, returning its WebSocket URL and a cancel handle.
async fn start_server() -> (String, CancellationToken) {
    start_server_with(counting_registry()).await
}

async fn start_server_with(remote: Arc<RemoteRegistry>) -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let cancel = CancellationToken::new();
    let token = cancel.clone();

    tokio::spawn(async move {
        let server =
            girder_rpc::RpcServer::new(Arc::new(girder_core::EventBus::new())).with_remote(remote);
        let _ = axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await;
    });

    (format!("ws://{address}/api/remote.mux"), cancel)
}

/// Connects and reads past the `ready` frame, which every generation starts with.
async fn connect(url: &str) -> Socket {
    let (mut socket, _) = connect_async(url).await.expect("the mux must accept");
    let ready = next_downlink(&mut socket).await;
    assert!(
        matches!(ready, DownlinkFrame::Ready { .. }),
        "the first frame must be `ready`, got {ready:?}"
    );
    socket
}

/// Reads the next downlink frame, skipping nothing: a decode failure is a test failure.
async fn next_downlink(socket: &mut Socket) -> DownlinkFrame {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a frame must arrive within the bound")
            .expect("the socket must stay open")
            .expect("the frame must not be an error");
        if let Message::Text(text) = message {
            return decode_downlink(&text).expect("the server must send valid frames");
        }
    }
}

/// Reads frames until `wanted` accepts one, returning it.
///
/// Needed because frames are not the only thing on this socket — an event forward or a waterfall
/// could interleave — so a test that asserted on "the next frame" would be asserting on ordering it
/// does not own.
async fn wait_for(
    socket: &mut Socket,
    mut wanted: impl FnMut(&DownlinkFrame) -> bool,
) -> DownlinkFrame {
    for _ in 0..40 {
        let frame = next_downlink(socket).await;
        if wanted(&frame) {
            return frame;
        }
    }
    panic!("the wanted frame never arrived");
}

/// Sends one uplink control as text.
async fn send(socket: &mut Socket, value: serde_json::Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("the uplink must send");
}

/// Sends a subscribe request for `namespace.method`.
async fn subscribe(
    socket: &mut Socket,
    id: &str,
    namespace: &str,
    method: &str,
    args: serde_json::Value,
) {
    send(
        socket,
        serde_json::json!({
            "type": "subscribe",
            "id": id,
            "namespace": namespace,
            "method": method,
            "args": args,
            "peer": "test",
            // Short enough that the test finishes quickly, above the floor the host enforces.
            "intervalMs": 30,
        }),
    )
    .await;
}

/// Asserts a subscription ack arrives with `ok` equal to `expected`.
async fn expect_ack(socket: &mut Socket, id: &str, expected: bool) -> Option<String> {
    let frame = wait_for(
        socket,
        |frame| matches!(frame, DownlinkFrame::SubscribeResult { id: got, .. } if got == id),
    )
    .await;

    let DownlinkFrame::SubscribeResult { ok, error, .. } = frame else {
        unreachable!("wait_for returned the frame it matched")
    };
    assert_eq!(ok, expected, "ack for `{id}`: {error:?}");
    error
}

// ---- a value arrives ---------------------------------------------------------------------------

#[tokio::test]
async fn subscribing_to_a_streaming_method_delivers_a_frame() {
    // The property the whole feature exists for: a client that subscribed sees values without
    // calling the method itself.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(&mut socket, "s1", "job", "follow", serde_json::json!({})).await;
    expect_ack(&mut socket, "s1", true).await;

    let frame = wait_for(&mut socket, |frame| {
        matches!(
            frame,
            DownlinkFrame::Subscription {
                frame: SubscriptionFrame::Value { .. }
            }
        )
    })
    .await;

    let DownlinkFrame::Subscription {
        frame:
            SubscriptionFrame::Value {
                subscription_id,
                method,
                value,
                seq,
            },
    } = frame
    else {
        unreachable!("wait_for returned a value frame")
    };

    assert_eq!(subscription_id.as_str(), "s1");
    assert_eq!(
        method, "job.follow",
        "a frame names the method that produced it"
    );
    assert!(
        value.get("at").is_some(),
        "the method's value must be carried: {value}"
    );
    assert_eq!(seq, 1, "the first value of a series is 1");

    socket.close(None).await.ok();
    cancel.cancel();
}

#[tokio::test]
async fn a_subscription_produces_more_than_one_value() {
    // A single frame would also be produced by a unary call. The point of a subscription is a series,
    // so the test asks for two frames from the same id and checks the sequence advances.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(&mut socket, "s1", "job", "follow", serde_json::json!({})).await;
    expect_ack(&mut socket, "s1", true).await;

    let mut sequences = Vec::new();
    while sequences.len() < 2 {
        let frame = wait_for(&mut socket, |frame| {
            matches!(
                frame,
                DownlinkFrame::Subscription {
                    frame: SubscriptionFrame::Value { .. }
                }
            )
        })
        .await;
        let DownlinkFrame::Subscription {
            frame: SubscriptionFrame::Value { seq, .. },
        } = frame
        else {
            unreachable!("wait_for returned a value frame")
        };
        sequences.push(seq);
    }

    assert_eq!(sequences, vec![1, 2], "a series must advance, not repeat");

    socket.close(None).await.ok();
    cancel.cancel();
}

// ---- refusal -----------------------------------------------------------------------------------

#[tokio::test]
async fn subscribing_to_a_unary_method_is_refused_with_a_reason() {
    // Allowing a subscription to any method would let a client hold server slots that never produce
    // anything. The refusal must say which methods are subscribable, or a client author cannot tell
    // a typo from a gap.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(&mut socket, "bad", "job", "list", serde_json::json!({})).await;

    let error = expect_ack(&mut socket, "bad", false)
        .await
        .expect("a refusal must carry a message");

    assert!(
        error.contains("job.list"),
        "the message must name the refused method: {error}"
    );
    assert!(
        error.contains("not subscribable"),
        "the message must say why: {error}"
    );
    assert!(
        error.contains("job.follow"),
        "the message must list what is subscribable: {error}"
    );

    // No frame may arrive for a refused subscription: a client that got a value after a refusal would
    // have no id to attribute it to.
    let next = tokio::time::timeout(Duration::from_millis(300), socket.next()).await;
    assert!(
        next.is_err(),
        "a refused subscription must produce no frames"
    );

    socket.close(None).await.ok();
    cancel.cancel();
}

#[tokio::test]
async fn an_empty_subscription_id_is_refused() {
    // An empty id names every subscription at once, so frames from one would be indistinguishable
    // from frames from another.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(&mut socket, "", "job", "follow", serde_json::json!({})).await;

    let frame = wait_for(&mut socket, |frame| {
        matches!(frame, DownlinkFrame::SubscribeResult { .. })
    })
    .await;
    let DownlinkFrame::SubscribeResult { ok, error, .. } = frame else {
        unreachable!("wait_for returned an ack")
    };

    assert!(!ok);
    assert!(
        error.unwrap_or_default().contains("must not be empty"),
        "the refusal must say what was wrong"
    );

    socket.close(None).await.ok();
    cancel.cancel();
}

#[tokio::test]
async fn the_subscription_limit_is_enforced_over_the_wire() {
    // Each open subscription spawns a poller, so an unbounded set is one client spending the host's
    // CPU. The limit is a transport-level fact, not just a registry one.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    for index in 0..DEFAULT_SUBSCRIPTION_LIMIT {
        let id = format!("s{index}");
        subscribe(&mut socket, &id, "job", "follow", serde_json::json!({})).await;
        expect_ack(&mut socket, &id, true).await;
    }

    subscribe(
        &mut socket,
        "overflow",
        "job",
        "follow",
        serde_json::json!({}),
    )
    .await;
    let error = expect_ack(&mut socket, "overflow", false)
        .await
        .expect("the limit refusal must carry a message");
    assert!(
        error.contains("at most"),
        "the refusal must name the limit: {error}"
    );

    socket.close(None).await.ok();
    cancel.cancel();
}

// ---- unsubscribe -------------------------------------------------------------------------------

#[tokio::test]
async fn unsubscribing_produces_an_end_frame_and_stops_the_stream() {
    // The property: after a close the client hears why, and then nothing. Silence alone would be
    // indistinguishable from a broken connection.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(&mut socket, "s1", "job", "follow", serde_json::json!({})).await;
    expect_ack(&mut socket, "s1", true).await;
    wait_for(&mut socket, |frame| {
        matches!(
            frame,
            DownlinkFrame::Subscription {
                frame: SubscriptionFrame::Value { .. }
            }
        )
    })
    .await;

    send(
        &mut socket,
        serde_json::json!({"type": "unsubscribe", "id": "s1"}),
    )
    .await;

    let frame = wait_for(&mut socket, |frame| {
        matches!(
            frame,
            DownlinkFrame::Subscription {
                frame: SubscriptionFrame::End { .. }
            }
        )
    })
    .await;
    let DownlinkFrame::Subscription {
        frame: SubscriptionFrame::End {
            subscription_id,
            reason,
        },
    } = frame
    else {
        unreachable!("wait_for returned an end frame")
    };
    assert_eq!(subscription_id.as_str(), "s1");
    assert_eq!(reason, EndReason::Unsubscribed);

    // The bound is several poll intervals: a poller that ignored the close would produce at least one
    // more frame in that window.
    let next = tokio::time::timeout(Duration::from_millis(300), socket.next()).await;
    assert!(next.is_err(), "no frame may follow the end frame");

    socket.close(None).await.ok();
    cancel.cancel();
}

// ---- a natural end -----------------------------------------------------------------------------

#[tokio::test]
async fn a_method_that_reports_done_produces_an_end_frame() {
    // A client waiting for a value that will never come has no way to tell "nothing happened" from
    // "the stream is gone", so a method that finished must say so with a frame.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(
        &mut socket,
        "done",
        "session",
        "follow",
        serde_json::json!({}),
    )
    .await;
    expect_ack(&mut socket, "done", true).await;

    let frame = wait_for(&mut socket, |frame| {
        matches!(
            frame,
            DownlinkFrame::Subscription {
                frame: SubscriptionFrame::End { .. }
            }
        )
    })
    .await;
    let DownlinkFrame::Subscription {
        frame: SubscriptionFrame::End { reason, .. },
    } = frame
    else {
        unreachable!("wait_for returned an end frame")
    };

    assert_eq!(
        reason,
        EndReason::Completed,
        "a method that finished is a completion, not a failure"
    );

    let next = tokio::time::timeout(Duration::from_millis(300), socket.next()).await;
    assert!(
        next.is_err(),
        "a completed subscription must not keep polling"
    );

    socket.close(None).await.ok();
    cancel.cancel();
}

#[tokio::test]
async fn a_method_that_fails_ends_the_subscription_with_the_reason() {
    // Retrying a failing method on a timer would repeat the same failure forever, so the first error
    // ends the series and the client is told why.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(
        &mut socket,
        "boom",
        "session",
        "observeEvent",
        serde_json::json!({}),
    )
    .await;
    expect_ack(&mut socket, "boom", true).await;

    let frame = wait_for(&mut socket, |frame| {
        matches!(
            frame,
            DownlinkFrame::Subscription {
                frame: SubscriptionFrame::End { .. }
            }
        )
    })
    .await;
    let DownlinkFrame::Subscription {
        frame: SubscriptionFrame::End { reason, .. },
    } = frame
    else {
        unreachable!("wait_for returned an end frame")
    };

    let EndReason::Failed { reason } = reason else {
        panic!("expected a failure, got {reason:?}");
    };
    assert!(
        reason.contains("the job was deleted"),
        "the end must carry the method's own error: {reason}"
    );

    socket.close(None).await.ok();
    cancel.cancel();
}

// ---- several subscriptions ---------------------------------------------------------------------

#[tokio::test]
async fn two_subscriptions_on_one_socket_stay_distinguishable() {
    // A client multiplexes one socket. If the frames did not carry their id, two watches would merge
    // into one unordered stream and the client could not route them.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(&mut socket, "a", "job", "follow", serde_json::json!({})).await;
    expect_ack(&mut socket, "a", true).await;
    subscribe(&mut socket, "b", "job", "follow", serde_json::json!({})).await;
    expect_ack(&mut socket, "b", true).await;

    let mut seen = std::collections::BTreeSet::new();
    while seen.len() < 2 {
        let frame = wait_for(&mut socket, |frame| {
            matches!(
                frame,
                DownlinkFrame::Subscription {
                    frame: SubscriptionFrame::Value { .. }
                }
            )
        })
        .await;
        let DownlinkFrame::Subscription {
            frame: SubscriptionFrame::Value {
                subscription_id, ..
            },
        } = frame
        else {
            unreachable!("wait_for returned a value frame")
        };
        seen.insert(subscription_id.to_string());
    }

    assert_eq!(
        seen,
        ["a".to_owned(), "b".to_owned()].into_iter().collect(),
        "each subscription's frames must name it"
    );

    socket.close(None).await.ok();
    cancel.cancel();
}

#[tokio::test]
async fn a_disconnect_stops_the_pollers() {
    // A poller that outlived its socket would keep calling methods for a client that cannot read the
    // answers. Observable through the unary counter: the method is invoked once per poll, so a
    // stopped poller leaves the count still.
    let mut registry = RemoteRegistry::new();
    UNARY_CALLS.store(0, Ordering::Relaxed);
    registry.register("job", "follow", |_call: RemoteCall| {
        Box::pin(async move {
            UNARY_CALLS.fetch_add(1, Ordering::Relaxed);
            Ok(serde_json::json!({ "at": UNARY_CALLS.load(Ordering::Relaxed) }))
        })
    });
    let (url, cancel) = start_server_with(Arc::new(registry)).await;
    let mut socket = connect(&url).await;

    subscribe(&mut socket, "s1", "job", "follow", serde_json::json!({})).await;
    expect_ack(&mut socket, "s1", true).await;
    wait_for(&mut socket, |frame| {
        matches!(
            frame,
            DownlinkFrame::Subscription {
                frame: SubscriptionFrame::Value { .. }
            }
        )
    })
    .await;

    socket.close(None).await.ok();

    // Let the server observe the close, then take a baseline and confirm it does not move.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let baseline = UNARY_CALLS.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = UNARY_CALLS.load(Ordering::Relaxed);

    assert_eq!(
        baseline, after,
        "a poller must stop when its connection closes ({baseline} then {after})"
    );

    cancel.cancel();
}

// ---- wire shape --------------------------------------------------------------------------------

#[tokio::test]
async fn a_subscription_frame_is_nested_under_its_own_type() {
    // The envelope is what lets a client dispatch on one field. Pinned as a literal because a serde
    // rename would otherwise only show up in a real client.
    let (url, cancel) = start_server().await;
    let mut socket = connect(&url).await;

    subscribe(&mut socket, "s1", "job", "follow", serde_json::json!({})).await;

    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("a frame must arrive")
        .expect("the socket must stay open")
        .expect("no transport error");
    let Message::Text(text) = message else {
        panic!("the server sends text frames");
    };

    assert!(text.contains(r#""type":"subscribeResult""#), "{text}");

    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("a value must arrive")
        .expect("the socket must stay open")
        .expect("no transport error");
    let Message::Text(text) = message else {
        panic!("the server sends text frames");
    };

    assert!(text.contains(r#""type":"subscription""#), "{text}");
    assert!(text.contains(r#""subscriptionId":"s1""#), "{text}");

    socket.close(None).await.ok();
    cancel.cancel();
}
