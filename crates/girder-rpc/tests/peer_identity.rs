//! Peer identity at the transport boundary.
//!
//! `gateway_gap` implements the whole admission model — an immutable `Principal`, a `Peer` with a
//! shared lifetime, and [`admitted_call`] as the one function that can build a `RemoteCall`'s
//! identity. What it cannot do on its own is prove that the **RPC layer uses it**: this crate could
//! keep copying `request.peer` into the call and every `gateway_gap` test would still pass.
//!
//! These tests are that proof, and they are written against the distinction the change turns on:
//!
//! - **legacy mode** (no `PeerDirectory`) carries the wire's `peer` unchanged. Every existing client
//!   and test speaks this, so it stays the default and its behaviour is pinned here;
//! - **strict mode** (`with_peer_directory`) admits the caller first and then *compares* the wire
//!   field with the identity it admitted. A claim can agree or be refused; it can never create an
//!   identity.
//!
//! The interesting cases are the ones where a claim is *plausible*. "Some garbage string is refused"
//! would pass even if the server ignored the field entirely; "a caller presenting a valid credential
//! but claiming a different peer is refused" only passes if the two are really being cross-checked.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use girder_core::EventBus;
use girder_core::gateway_gap::{Interactions, PeerDirectory, Principal, RESERVED_IDENTITY_FIELDS};
use girder_rpc::server::PEER_HEADER;
use girder_rpc::{DownlinkFrame, INVOKE_PATH, RpcServer, SubscriptionFrame, decode_downlink};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::{ClientRequestBuilder, Message};
use tokio_util::sync::CancellationToken;

// ---- the registry the tests dispatch to --------------------------------------------------------

/// A registry with one method that reports the identity it was actually given.
///
/// Returning `call.peer` is the whole point: the assertion is about what the *handler* saw, not about
/// what the response envelope says. A server that admitted a Peer and then still passed the wire
/// string through would fail these tests.
fn registry(calls: Arc<AtomicUsize>) -> Arc<girder_core::RemoteRegistry> {
    let mut registry = girder_core::RemoteRegistry::new();
    let counter = Arc::clone(&calls);
    registry.register("test", "echo", move |call: girder_core::RemoteCall| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(serde_json::json!({ "peer": call.peer, "args": call.args }))
        })
    });
    // A subscribable method so the socket path can be exercised too.
    registry.register("job", "follow", move |call: girder_core::RemoteCall| {
        let counter = Arc::clone(&calls);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(serde_json::json!({ "peer": call.peer }))
        })
    });
    Arc::new(registry)
}

// ---- servers ------------------------------------------------------------------------------------

/// A running test server: its address, the handle that stops it, and how often its methods ran.
///
/// The counter belongs to the server rather than the module because these tests run concurrently:
/// a shared static would let one test observe another's calls.
struct Server {
    address: std::net::SocketAddr,
    cancel: CancellationToken,
    calls: Arc<AtomicUsize>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Server {
    fn mux(&self) -> String {
        format!("ws://{}/api/remote.mux", self.address)
    }

    /// How many times a method on this server actually ran.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

async fn start(build: impl FnOnce(RpcServer) -> RpcServer) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let server =
        build(RpcServer::new(Arc::new(EventBus::new())).with_remote(registry(Arc::clone(&calls))));
    tokio::spawn(async move {
        let _ = axum::serve(listener, server.router())
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await;
    });
    Server {
        address,
        cancel,
        calls,
    }
}

/// A directory holding one published operator Peer, returning it with the directory.
///
/// `publish` is how a Host declares a machine identity; the returned `Peer`'s id is the credential a
/// client presents. Nothing about it is guessable from the request, which is what makes it a
/// credential rather than a claim.
fn directory_with_operator() -> (Arc<PeerDirectory>, String) {
    let directory = PeerDirectory::new();
    let peer = directory.publish(Principal::named("operator", "ci"), Interactions::Answer);
    let id = peer.id().as_str().to_owned();
    (Arc::new(directory), id)
}

// ---- HTTP ---------------------------------------------------------------------------------------

/// One HTTP response as the client sees it.
struct Response {
    status: u16,
    body: String,
}

impl Response {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("body must be JSON ({error}): {}", self.body))
    }
}

/// Posts one invoke request with the given extra headers. Hand-rolled to keep the test dependency-free.
async fn post(server: &Server, headers: &[(&str, &str)], body: &serde_json::Value) -> Response {
    let authority = server.address.to_string();
    let mut stream = tokio::net::TcpStream::connect(authority.as_str())
        .await
        .unwrap();
    let payload = body.to_string();
    let mut request = format!(
        "POST {INVOKE_PATH} HTTP/1.1\r\nHost: {authority}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        payload.len()
    );
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(&payload);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").expect("a complete response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    Response {
        status,
        body: body.to_owned(),
    }
}

/// The invoke body for one `test.echo` call claiming `peer`.
fn invoke_body(peer: &str) -> serde_json::Value {
    serde_json::json!({
        "namespace": "test",
        "method": "echo",
        "args": { "value": 1 },
        "peer": peer,
    })
}

// ---- legacy mode: unchanged, and still unverified ------------------------------------------------

/// Without a directory the wire's `peer` is carried, exactly as before this change.
///
/// This is the backward-compatibility contract: a deployment that never calls
/// `with_peer_directory` cannot observe a difference, including the fact that the identity is not
/// verified. Pinned because the doc comment on `InvokeRemoteRequest.peer` says "carried, never
/// verified", and that sentence has to stay true for the mode it describes.
#[tokio::test]
async fn legacy_mode_carries_the_claimed_peer_unverified() {
    let server = start(|server| server).await;

    let first = post(&server, &[], &invoke_body("tenant-a")).await;
    assert_eq!(first.status, 200, "{}", first.body);
    assert_eq!(first.json()["value"]["peer"], "tenant-a");

    // A second, unrelated identity is served just as readily: nothing was checked.
    let second = post(&server, &[], &invoke_body("tenant-b")).await;
    assert_eq!(second.status, 200, "{}", second.body);
    assert_eq!(second.json()["value"]["peer"], "tenant-b");

    // An empty claim is still a claim: the legacy path copies it through rather than inventing one.
    let empty = post(&server, &[], &invoke_body("")).await;
    assert_eq!(empty.status, 200, "{}", empty.body);
    assert_eq!(empty.json()["value"]["peer"], "");

    // And a header the strict path would honour means nothing here, which is the whole difference.
    let with_header = post(
        &server,
        &[(PEER_HEADER, "peer-1")],
        &invoke_body("tenant-c"),
    )
    .await;
    assert_eq!(with_header.json()["value"]["peer"], "tenant-c");
}

// ---- strict mode: admission --------------------------------------------------------------------

/// A strict server refuses a request carrying no credential at all.
///
/// 401 rather than 403: the Host/Origin fence passed (no `Origin` means not a page), and what is
/// missing is proof of identity. A client can fix this by presenting a credential; it could not fix
/// a 403 caused by where it was served from.
#[tokio::test]
async fn strict_mode_refuses_an_uncredentialed_request() {
    let (directory, _credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(directory)).await;

    let response = post(&server, &[], &invoke_body("tenant-a")).await;
    assert_eq!(response.status, 401, "{}", response.body);
    let body = response.json();
    assert_eq!(body["status"], "error");
    assert_eq!(body["error"]["code"], "remote-access/denied");
}

/// An unknown credential names no Peer, and the refusal says which status applies.
#[tokio::test]
async fn strict_mode_refuses_an_unknown_credential() {
    let (directory, _credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(directory)).await;

    let response = post(
        &server,
        &[(PEER_HEADER, "peer-404")],
        &invoke_body("peer-404"),
    )
    .await;
    assert_eq!(response.status, 401, "{}", response.body);
    assert_eq!(response.json()["error"]["code"], "remote-access/denied");
}

/// A valid credential is admitted, and the handler sees the Peer the directory chose.
#[tokio::test]
async fn strict_mode_admits_a_known_credential() {
    let (directory, credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(directory)).await;

    let response = post(
        &server,
        &[(PEER_HEADER, &credential)],
        &invoke_body(&credential),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.json()["value"]["peer"], credential);
    assert_eq!(server.calls(), 1, "the method ran once");
}

/// A request that claims no identity is admitted on its credential alone.
///
/// The wire field is required by the request type, so `""` is how a client says "I am not claiming to
/// be anyone". Accepting it matters for the same reason refusing a disagreement does: the *credential*
/// is the identity, and a client that does not restate it is not thereby suspect.
#[tokio::test]
async fn strict_mode_accepts_a_request_that_claims_no_identity() {
    let (directory, credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(directory)).await;

    let response = post(
        &server,
        &[(PEER_HEADER, credential.as_str())],
        &invoke_body(""),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        response.json()["value"]["peer"],
        credential,
        "the admitted Peer is the identity, not the empty claim"
    );
}

/// Disposing the directory turns every later admission into a refusal.
///
/// This is what makes "the Host is shutting down" observable to a client instead of it seeing
/// timeouts: `PeerDirectory::dispose` refuses rather than serving from a stale table.
#[tokio::test]
async fn strict_mode_refuses_everything_after_the_directory_is_disposed() {
    let (directory, credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(Arc::clone(&directory))).await;

    let before = post(
        &server,
        &[(PEER_HEADER, credential.as_str())],
        &invoke_body(&credential),
    )
    .await;
    assert_eq!(before.status, 200, "{}", before.body);

    directory.dispose();

    let after = post(
        &server,
        &[(PEER_HEADER, credential.as_str())],
        &invoke_body(&credential),
    )
    .await;
    assert_eq!(after.status, 401, "{}", after.body);
    assert_eq!(after.json()["error"]["code"], "remote-access/denied");
}

/// The security property: a valid credential cannot speak for a *different* Peer, even when the
/// claim it makes is itself a published identity.
///
/// This is the case a naive implementation misses. Admitting on the header and then *overwriting*
/// the call's peer with the header value would pass every other test in this file, because the
/// admitted identity and the claim would coincide by construction. Here they deliberately do not,
/// and the request must be refused rather than served as either one.
#[tokio::test]
async fn strict_mode_refuses_a_claim_that_disagrees_with_the_credential() {
    let directory = PeerDirectory::new();
    // Two real, published operators: both credentials are valid, so nothing here is "unknown".
    let first = directory.publish(Principal::named("operator", "a"), Interactions::Answer);
    let second = directory.publish(Principal::named("operator", "b"), Interactions::Answer);
    let server = start(|server| server.with_peer_directory(Arc::new(directory))).await;

    let response = post(
        &server,
        &[(PEER_HEADER, first.id().as_str())],
        &invoke_body(second.id().as_str()),
    )
    .await;

    assert_eq!(response.status, 403, "{}", response.body);
    assert_eq!(response.json()["error"]["code"], "remote-access/denied");
    assert_eq!(
        server.calls(),
        0,
        "a refused call must not reach the handler"
    );
}

/// An identity spelled in `args` is refused, not silently dropped.
///
/// `gateway_gap::admitted_call` refuses a payload carrying `peer` or `principal` at the top level.
/// The property that matters at this layer is that the refusal happens *before* dispatch: a handler
/// that ran and ignored the field would leave the caller believing it had set something.
#[tokio::test]
async fn strict_mode_refuses_an_identity_smuggled_in_the_arguments() {
    let (directory, credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(directory)).await;

    for field in RESERVED_IDENTITY_FIELDS {
        let body = serde_json::json!({
            "namespace": "test",
            "method": "echo",
            "args": { "value": 1, *field: "tenant-a" },
            "peer": credential,
        });
        let response = post(&server, &[(PEER_HEADER, &credential)], &body).await;
        assert_eq!(response.status, 403, "{field}: {}", response.body);
        assert_eq!(
            response.json()["error"]["code"],
            "remote-access/denied",
            "{field}"
        );
    }
    assert_eq!(
        server.calls(),
        0,
        "a refused call must not reach the handler"
    );
}

// ---- strict mode: the fence ----------------------------------------------------------------------

/// A foreign Origin is refused with 403 even when the credential is valid.
///
/// Order matters and is why the status differs from the 401 above: trust is decided before the
/// credential, so a rebound page learns nothing about whether its guessed credential was good.
#[tokio::test]
async fn strict_mode_refuses_a_foreign_origin_before_checking_the_credential() {
    let (directory, credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(directory)).await;

    let response = post(
        &server,
        &[
            (PEER_HEADER, &credential),
            ("origin", "https://evil.example"),
        ],
        &invoke_body(&credential),
    )
    .await;
    assert_eq!(response.status, 403, "{}", response.body);

    // The same credential with no Origin is served, so the 403 above is the fence and not the
    // credential.
    let accepted = post(
        &server,
        &[(PEER_HEADER, &credential)],
        &invoke_body(&credential),
    )
    .await;
    assert_eq!(accepted.status, 200, "{}", accepted.body);
}

/// A loopback Origin passes without configuration, a named one passes when configured.
#[tokio::test]
async fn strict_mode_trusts_loopback_and_configured_origins() {
    let (directory, credential) = directory_with_operator();
    let server = start(|server| {
        server
            .with_peer_directory(directory)
            .trusting_origins(vec!["http://gui.internal:8080".to_owned()])
    })
    .await;

    for origin in [
        "http://127.0.0.1:8080",
        "http://localhost:3000",
        "http://gui.internal:8080",
    ] {
        let response = post(
            &server,
            &[(PEER_HEADER, &credential), ("origin", origin)],
            &invoke_body(&credential),
        )
        .await;
        assert_eq!(response.status, 200, "{origin}: {}", response.body);
    }
}

// ---- strict mode: the socket path ----------------------------------------------------------------

/// A socket with no credential is refused at the upgrade, so no `ready` frame is ever sent.
#[tokio::test]
async fn strict_mode_refuses_an_uncredentialed_socket() {
    let (directory, _credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(directory)).await;

    let result = tokio_tungstenite::connect_async(server.mux()).await;
    assert!(result.is_err(), "an unadmitted socket must not be upgraded");
}

/// A subscription on an admitted socket speaks for the connection's Peer.
#[tokio::test]
async fn strict_mode_polls_a_subscription_as_the_admitted_peer() {
    let (directory, credential) = directory_with_operator();
    let server = start(|server| server.with_peer_directory(directory)).await;

    let request = ClientRequestBuilder::new(server.mux().parse().unwrap())
        .with_header(PEER_HEADER, credential.as_str());
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("an admitted socket connects");

    // `ready` first: the generation identity precedes any frame that could reference it.
    let ready = next_frame(&mut socket).await;
    assert!(matches!(ready, DownlinkFrame::Ready { .. }), "{ready:?}");

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "subscribe",
                "id": "s1",
                "namespace": "job",
                "method": "follow",
                "args": {},
                // The claim names someone else. The connection's Peer is what must be used.
                "peer": "tenant-elsewhere",
                "intervalMs": 30,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // The poller never starts: the claim was refused, which is the stream-side half of the check.
    let frame = next_frame(&mut socket).await;
    let DownlinkFrame::SubscribeResult { id, ok, error } = frame else {
        panic!("expected a subscribe result, got {frame:?}");
    };
    assert_eq!(id, "s1");
    assert!(!ok, "a disagreeing claim must be refused");
    assert!(
        error.unwrap_or_default().contains("admitted as"),
        "the refusal must say which identity the socket has"
    );

    // And an agreeing claim is accepted and polled as that Peer.
    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "subscribe",
                "id": "s2",
                "namespace": "job",
                "method": "follow",
                "args": {},
                "peer": credential,
                "intervalMs": 30,
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let frame = next_frame(&mut socket).await;
    let DownlinkFrame::SubscribeResult { id, ok, .. } = frame else {
        panic!("expected a subscribe result, got {frame:?}");
    };
    assert_eq!(id, "s2");
    assert!(ok, "an agreeing claim opens the subscription");

    let frame = next_frame(&mut socket).await;
    let DownlinkFrame::Subscription {
        frame: SubscriptionFrame::Value { value, .. },
    } = frame
    else {
        panic!("expected a value, got {frame:?}");
    };
    assert_eq!(
        value["peer"], credential,
        "the poller used the admitted Peer"
    );

    let _ = socket.close(None).await;
}

/// Reads the next downlink frame, with a bound so a missing frame fails instead of hanging.
async fn next_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> DownlinkFrame {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a frame must arrive")
            .expect("the socket must stay open")
            .expect("the frame must not be an error");
        if let Message::Text(text) = message {
            return decode_downlink(&text).expect("the server must send valid frames");
        }
    }
}
