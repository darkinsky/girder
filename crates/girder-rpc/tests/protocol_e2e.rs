//! End-to-end tests for the HTTP half of the RPC protocol: real sockets, real requests, real bodies.
//!
//! The existing suites stop at the edges of this: `transport.rs` drives the WebSocket mux, `bounded_transport.rs`
//! and `subscription(s).rs` drive the queueing and fan-out, `web_page.rs` fetches the probe page. What none of them
//! does is put a request on a socket and assert the *wire* shape a browser or a TypeScript client would branch on:
//! the HTTP status, the JSON envelope, and the error `code`.
//!
//! That gap matters because the envelope is the contract. `girder-cli` and the probe page are separate programs
//! that read this JSON; a field renamed or an error that turns into a 500 breaks them without breaking any
//! in-process test.
//!
//! # Why a hand-written HTTP client
//!
//! There is no HTTP client in this crate's dependency set, and the assertions below need nothing a client library
//! provides: one request, `Connection: close`, read to EOF, parse. `web_page.rs` made the same choice for the same
//! reason — a test should not add a dependency to read a few hundred bytes. Every request still crosses a real
//! `TcpListener`, so a routing, header or status mistake is still caught; only the convenience layer is missing.
//!
//! # What each test pins down
//!
//! | Test | Contract |
//! |---|---|
//! | success envelope | `{"status":"ok","value":...}` over HTTP 200 |
//! | domain failure | `{"status":"error","error":{...}}` over HTTP 200, not a 5xx |
//! | unknown method | `code == "UNKNOWN_METHOD"` |
//! | declared gap | `code == "NOT_IMPLEMENTED"`, distinct from an unknown name |
//! | bad arguments | `code == "INVALID_ARGUMENTS"` from a real method |
//! | malformed JSON | transport rejection, no panic, server keeps serving |
//! | oversized body | rejected by the body limit rather than buffered |
//! | concurrency | responses are matched to their own requests |
//! | probe page | `GET /` is HTML and names the invoke endpoint |
//! | no registry | the invoke route is absent rather than silently accepting |

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use girder_core::{
    EventBus, RemoteCall, RemoteError, RemoteRegistry, api_config::WorkspaceRegistry,
};
use girder_rpc::{INVOKE_PATH, WEB_INDEX_PATH};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

/// axum's default request-body ceiling, which is the bound that actually applies to `INVOKE_PATH`.
///
/// Named rather than inlined because the oversized-body test derives its fixture from it: the fixture has to sit just
/// past the limit so the refusal is caused by the limit and not by the kernel buffer.
/// The limit the router sets, mirrored here so the fixture can exceed it.
///
/// Kept in step with `server::MAX_REQUEST_BYTES`. Before that constant existed the bound was axum's default of 2 MiB and
/// nothing in the tree configured it, so a deployment tuning a request size changed nothing.
const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// How many calls the concurrency test issues, so the assertion message and the loop cannot disagree.
const CALLS: usize = 16;

// ---- the HTTP client ----------------------------------------------------------------------------

/// The parts of a response an assertion can look at.
///
/// Header names are lowercased on parse so a lookup does not have to guess at `Content-Type` versus
/// `content-type`; HTTP header names are case insensitive and a server is free to send either.
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    /// One response header, by case-insensitive name.
    fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body parsed as JSON, with the raw body in the panic message.
    ///
    /// A failed parse is a protocol failure, so the message must show what actually arrived — an error that only
    /// says "invalid JSON" would hide the one fact needed to fix it.
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("expected a JSON body ({error}): {:?}", self.body))
    }
}

/// Sends one HTTP/1.1 request and returns the parsed response.
///
/// `Connection: close` rather than keep-alive: the response is read to EOF, so no framing code is needed beyond
/// splitting the header block from the body. axum answers with a `Content-Length`, which is the only framing used.
async fn send(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        // `write!` rather than `push_str(&format!(..))`: one allocation instead of two, per header.
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("\r\n");

    let mut request = head.into_bytes();
    request.extend_from_slice(body);

    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("the server must be listening");
    let (mut reader, mut writer) = stream.split();

    // Write and read concurrently. A server that answers before the whole body is written — which is exactly what a
    // body-limit rejection does — would otherwise leave the writer blocked on a full socket buffer while the reader
    // waits for a response that has already been sent.
    let write = async move {
        // Deliberately *not* followed by `shutdown()`. hyper's HTTP/1 server reads a client half-close as the
        // connection being gone and abandons the in-flight request without answering, so a method slower than the
        // round trip would return nothing — which is exactly what made the slow half of the concurrency test fail
        // while the fast half passed. The server ends the exchange instead, by closing its side after the response
        // because the request carried `Connection: close`.
        //
        // Ignored: a rejection can close the connection mid-body, and that write error is expected. The response, not
        // the write, is what the assertion is about.
        let _ = writer.write_all(&request).await;
        let _ = writer.flush().await;
    };
    let read = async move {
        let mut raw = Vec::new();
        reader
            .read_to_end(&mut raw)
            .await
            .expect("the server must close the connection after the response");
        raw
    };
    let ((), raw) = tokio::join!(write, read);

    parse_response(&raw)
}

/// Splits a raw HTTP/1.1 response into status, headers and body.
fn parse_response(raw: &[u8]) -> HttpResponse {
    let boundary = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response must terminate its header block with a blank line");
    let head = std::str::from_utf8(&raw[..boundary]).expect("response headers must be UTF-8");
    let body = String::from_utf8_lossy(&raw[boundary + 4..]).into_owned();

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .expect("a response must start with a status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("expected `HTTP/1.1 <code>`, got {status_line:?}"));

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();

    HttpResponse {
        status,
        headers,
        body,
    }
}

// ---- the server under test ----------------------------------------------------------------------

/// A server on an OS-assigned port, plus the token that stops it.
///
/// Port 0 rather than a fixed one: tests run in parallel and in the same process as every other suite, so a fixed
/// port is a flake waiting for a second run.
struct TestServer {
    address: SocketAddr,
    cancel: CancellationToken,
}

impl TestServer {
    /// One unary Remote call, as the probe page and any real client send it.
    async fn invoke(&self, namespace: &str, method: &str, args: Value) -> HttpResponse {
        let body = json!({
            "namespace": namespace,
            "method": method,
            "args": args,
            "peer": "test-peer",
        })
        .to_string();
        send(
            self.address,
            "POST",
            INVOKE_PATH,
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        )
        .await
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Stops the accept loop when the test ends, so a later test in the same process cannot inherit a listener from an
        // earlier one. The runtime would reap it anyway; cancelling makes the shutdown deterministic.
        self.cancel.cancel();
    }
}

/// Starts a server over `remote`, or with no Remote surface when `remote` is `None`.
///
/// Goes through the public `serve_with` so the test exercises the same router and graceful-shutdown path production
/// uses; a test that assembled its own `Router` would pass even if `serve_with` forgot to mount a route.
async fn start(remote: Option<Arc<RemoteRegistry>>) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let address = listener.local_addr().expect("the listener has an address");
    let cancel = CancellationToken::new();
    let stop = cancel.clone();

    tokio::spawn(async move {
        let _ = girder_rpc::serve_with(
            listener,
            Arc::new(EventBus::new()),
            Vec::new(),
            remote,
            stop,
        )
        .await;
    });

    TestServer { address, cancel }
}

/// A registry with test methods plus the real `workspace.*` surface.
///
/// The product surface is registered through its own function, not reimplemented: the point of the
/// `INVALID_ARGUMENTS` test is that a *real* handler's argument failure projects to that code on the wire.
fn test_registry() -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();

    // Echoes one named argument, so a response can be tied to the request that caused it.
    registry.register("test", "echo", |call: RemoteCall| {
        Box::pin(async move { Ok(call.args.get("value").cloned().unwrap_or(Value::Null)) })
    });

    // Sleeps before answering. Concurrency can only be observed if a second request has time to be dispatched while
    // the first is still in flight; an instant method would serialise trivially and prove nothing.
    registry.register("test", "slowEcho", |call: RemoteCall| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            Ok(call.args.get("value").cloned().unwrap_or(Value::Null))
        })
    });

    // Reports a domain failure, for the error envelope.
    registry.register("test", "fail", |_call: RemoteCall| {
        Box::pin(async {
            Err(RemoteError::Failed(
                "the method could not do the thing".to_owned(),
            ))
        })
    });

    // A name the build admits it does not implement. Dispatch must answer `NOT_IMPLEMENTED` for it rather than
    // `UNKNOWN_METHOD`, which is the difference between "this build is old" and "you have a typo".
    registry.declare_unimplemented("test", &["ghost"]);

    let workspaces = Arc::new(WorkspaceRegistry::new());
    girder_core::register_workspace_methods(&mut registry, &workspaces);

    registry
}

/// Metadata a client branches on, pulled out of an error envelope.
///
/// Returned as a tuple rather than the raw `Value` so every assertion names the field it depends on; a test that
/// poked at `body["error"]["code"]` inline would panic with a type error the first time the shape changed, which is
/// less useful than the assertion that was meant to run.
fn error_parts(response: &HttpResponse) -> (String, String, String) {
    let body = response.json();
    assert_eq!(
        body["status"], "error",
        "a failed call must be tagged `error`, got {}",
        response.body
    );
    let error = &body["error"];
    (
        error["name"]
            .as_str()
            .unwrap_or_else(|| panic!("`error.name` must be a string: {}", response.body))
            .to_owned(),
        error["code"]
            .as_str()
            .unwrap_or_else(|| panic!("`error.code` must be a string: {}", response.body))
            .to_owned(),
        error["message"]
            .as_str()
            .unwrap_or_else(|| panic!("`error.message` must be a string: {}", response.body))
            .to_owned(),
    )
}

// ---- success shape ------------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_invoke_returns_the_ok_envelope() {
    // The shape a client parses on every call: `{"status":"ok","value":...}`. The tag is asserted rather than just the
    // value because a client that deserialises into a sum type reads `status` first and fails outright if it drifts.
    let server = start(Some(Arc::new(test_registry()))).await;

    let response = server
        .invoke("test", "echo", json!({ "value": "hello" }))
        .await;

    // HTTP 200 is the only status a successful dispatch may use: a client that checks the status before the body would
    // otherwise have to special-case success.
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "a JSON envelope must be labelled as JSON"
    );

    let body = response.json();
    assert_eq!(
        body["status"], "ok",
        "unexpected envelope: {}",
        response.body
    );
    assert_eq!(body["value"], json!("hello"));
    // No error key on success: a client that checks `if body.error` would otherwise see a stale field.
    assert!(
        body.get("error").is_none(),
        "an ok envelope must not carry an error: {}",
        response.body
    );
}

#[tokio::test]
async fn a_method_failure_returns_the_error_envelope_over_http_200() {
    // A method that ran and reported a domain failure is a successful HTTP exchange. This is the single most
    // load-bearing status decision in the design: a 5xx would make an HTTP client's retry layer re-run a call that
    // already has a definite answer.
    let server = start(Some(Arc::new(test_registry()))).await;

    let response = server.invoke("test", "fail", json!({})).await;

    assert_eq!(
        response.status, 200,
        "a domain failure is not a transport failure: {}",
        response.body
    );

    let (name, code, message) = error_parts(&response);
    // `name` is the error *class* the wire uses, not the specific code; a client needs both, and today they disagree in
    // specificity ("RemoteError" here, "FAILED"/"UNKNOWN_METHOD" in `code`).
    assert_eq!(name, "RemoteError");
    assert_eq!(code, "FAILED");
    assert!(
        message.contains("could not do the thing"),
        "the message must carry the method's own explanation, got {message:?}"
    );
}

// ---- error codes --------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_method_reports_the_unknown_method_code() {
    // `UNKNOWN_METHOD` is what tells a client author they have a typo. Any other code would send them looking at the
    // server. Both halves are asserted: the code (machine) and the message (human, and it must name what was asked for
    // so a log line is enough to diagnose it).
    let server = start(Some(Arc::new(test_registry()))).await;

    let response = server
        .invoke("no.such.namespace", "nothing", json!({}))
        .await;

    assert_eq!(response.status, 200, "body: {}", response.body);
    let (_, code, message) = error_parts(&response);
    assert_eq!(code, "UNKNOWN_METHOD", "body: {}", response.body);
    assert!(
        message.contains("no.such.namespace.nothing"),
        "the message must name the method that was not found, got {message:?}"
    );
}

#[tokio::test]
async fn a_declared_gap_is_not_reported_as_an_unknown_method() {
    // The registry records methods the upstream API defines but this build lacks. Dispatch consults that list before
    // answering, so the two codes must differ on the wire — if they collapsed, a client could not tell "upgrade the
    // server" from "fix your call", which is the whole reason the list exists.
    let server = start(Some(Arc::new(test_registry()))).await;

    let response = server.invoke("test", "ghost", json!({})).await;

    assert_eq!(response.status, 200, "body: {}", response.body);
    let (_, code, message) = error_parts(&response);
    assert_eq!(code, "NOT_IMPLEMENTED", "body: {}", response.body);
    assert_ne!(
        code, "UNKNOWN_METHOD",
        "a declared gap must not look like a typo"
    );
    assert!(message.contains("test.ghost"), "got {message:?}");
}

#[tokio::test]
async fn invalid_arguments_report_the_invalid_arguments_code() {
    // Driven through the *real* `workspace.create` handler rather than a stub: the assertion is that a product
    // method's own argument check survives the projection to the wire. `workspace.create` requires `path`, so an
    // empty argument object is refused by the handler, not by the dispatcher.
    let server = start(Some(Arc::new(test_registry()))).await;

    let response = server.invoke("workspace", "create", json!({})).await;

    assert_eq!(response.status, 200, "body: {}", response.body);
    let (_, code, message) = error_parts(&response);
    assert_eq!(code, "INVALID_ARGUMENTS", "body: {}", response.body);
    assert!(
        message.contains("path"),
        "the message must name the missing field so a client can fix it, got {message:?}"
    );
    // Distinct from every other failure: a client that saw FAILED would retry a call that can never succeed.
    assert_ne!(code, "FAILED");
}

// ---- transport-level rejections -----------------------------------------------------------------

#[tokio::test]
async fn malformed_json_is_rejected_and_the_server_keeps_serving() {
    // Three ways a request can be wrong before dispatch is ever reached, all of which must be refused rather than
    // panic a handler task: unparseable JSON, JSON of the wrong shape, and a missing required field.
    let server = start(Some(Arc::new(test_registry()))).await;

    let cases: [(&str, &str); 3] = [
        // Truncated: a client that crashed mid-write.
        ("truncated", r#"{"namespace": "test", "method": "echo""#),
        // Valid JSON, wrong type.
        ("array", r#"["test", "echo"]"#),
        // Valid JSON, right object, but no `peer`: serde rejects the whole request rather than defaulting an identity.
        (
            "missing peer",
            r#"{"namespace": "test", "method": "echo", "args": {}}"#,
        ),
    ];

    for (label, body) in cases {
        let response = send(
            server.address,
            "POST",
            INVOKE_PATH,
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        )
        .await;

        assert!(
            (400..500).contains(&response.status),
            "{label}: a malformed request must be a client error, got {} ({})",
            response.status,
            response.body
        );
        // Not a 5xx: a handler panic or an unwrap would surface here, and the point of this test is that neither
        // happens.
        assert_ne!(response.status, 500, "{label}: {}", response.body);
        // The rejection is axum's, not the envelope: this is a real gap for clients, which must handle two error
        // shapes on one endpoint. Asserted so the shape is pinned rather than assumed.
        assert_ne!(
            response.header("content-type"),
            Some("application/json"),
            "{label}: a transport rejection is not the JSON envelope today; if that changes, the client contract did"
        );
    }

    // The server must still answer. A panic in a handler would have killed only that connection's task, so the
    // recovery check is a *new* request on a *new* connection — which is also what a client would do after a 400.
    let response = server
        .invoke("test", "echo", json!({ "value": "still alive" }))
        .await;
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_eq!(response.json()["value"], json!("still alive"));
}

#[tokio::test]
async fn a_request_larger_than_the_body_limit_is_rejected() {
    // The server must bound the body it buffers. Before `MAX_REQUEST_BYTES` was wired, the bound that applied was axum's
    // default of 2 MiB -- an accidental ceiling rather than a configured one. The test asserts the behaviour a client
    // sees: a refusal, not a hung connection and not a 200.
    let server = start(Some(Arc::new(test_registry()))).await;

    // Just over the limit, and only just: the server stops reading at the limit, and every byte still in the
    // socket after that is a byte the peer may reset on. A body far above the limit would test the kernel's buffer
    // rather than the server's bound.
    let oversized = "x".repeat(BODY_LIMIT + 64 * 1024);
    let body = json!({
        "namespace": "test",
        "method": "echo",
        "args": { "value": oversized },
        "peer": "test-peer",
    })
    .to_string();
    assert!(body.len() > BODY_LIMIT, "the fixture must exceed the limit");

    let response = send(
        server.address,
        "POST",
        INVOKE_PATH,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
    .await;

    // 413 Payload Too Large. A 200 here would mean the limit did not apply; a 400 would be a lie about why it failed.
    assert_eq!(
        response.status,
        413,
        "an oversized body must be refused, got {} ({})",
        response.status,
        &response.body[..response.body.len().min(200)]
    );
    assert!(
        !response.body.contains("\"status\":\"ok\""),
        "a refused request must not carry a success envelope"
    );

    // The refusal is per request, not a crash: the next call on a fresh connection must still work.
    let response = server
        .invoke("test", "echo", json!({ "value": "after" }))
        .await;
    assert_eq!(response.status, 200, "body: {}", response.body);
}

// ---- concurrency --------------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_invokes_keep_their_own_responses() {
    // The failure this guards against is a response routed to the wrong request — the kind of bug that only appears
    // under load and that in-process dispatch tests cannot see, because they never share a socket or a connection
    // pool. Each request carries a unique value and each response is checked against the value *it* sent.
    let server = start(Some(Arc::new(test_registry()))).await;

    let address = server.address;
    let mut handles = Vec::with_capacity(CALLS);

    for index in 0..CALLS {
        handles.push(tokio::spawn(async move {
            let value = format!("request-{index}");
            let body = json!({
                "namespace": "test",
                // Half slow, half fast: the slow ones are still in flight when the fast ones answer, which is what
                // makes a crossed response observable instead of merely possible.
                "method": if index % 2 == 0 { "slowEcho" } else { "echo" },
                "args": { "value": value },
                "peer": "test-peer",
            })
            .to_string();
            let response = send(
                address,
                "POST",
                INVOKE_PATH,
                &[("Content-Type", "application/json")],
                body.as_bytes(),
            )
            .await;
            (index, value, response)
        }));
    }

    let mut answered = 0;
    for handle in handles {
        let (index, value, response) = handle.await.expect("the request task must not panic");
        assert_eq!(response.status, 200, "call {index}: {}", response.body);
        let body = response.json();
        assert_eq!(body["status"], "ok", "call {index}: {}", response.body);
        assert_eq!(
            body["value"], value,
            "call {index} received another call's answer: {}",
            response.body
        );
        answered += 1;
    }

    assert_eq!(answered, CALLS, "every call must be answered");
}

// ---- the page, and the route that only exists with a registry ------------------------------------

#[tokio::test]
async fn the_root_path_serves_the_probe_page() {
    // The page is served by the same process as the API so a browser has no second origin to configure. Asserted over
    // the socket, and against the same HTML the crate exports: a routing mistake that served something else with a
    // 200 would pass a status-only check.
    let server = start(Some(Arc::new(test_registry()))).await;

    let response = send(server.address, "GET", WEB_INDEX_PATH, &[], b"").await;

    assert_eq!(response.status, 200, "body: {}", response.body);
    let content_type = response.header("content-type").unwrap_or_else(|| {
        panic!(
            "the page must declare a content type: {:?}",
            response.headers
        )
    });
    assert!(
        content_type.starts_with("text/html"),
        "a browser needs HTML, got {content_type:?}"
    );
    assert!(
        response
            .body
            .to_ascii_lowercase()
            .starts_with("<!doctype html>"),
        "the body must be the page, got {:?}",
        &response.body[..response.body.len().min(80)]
    );
    // The page's buttons post to the same path this suite posts to; if the two drifted, every button would 404.
    assert!(response.body.contains(INVOKE_PATH));
    assert_eq!(response.body, girder_rpc::INDEX_HTML);
}

#[tokio::test]
async fn the_invoke_route_is_absent_without_a_remote_registry() {
    // `RpcServer::router` mounts `INVOKE_PATH` only when a registry is present. The handler's own "this server exposes
    // no Remote methods" branch therefore cannot be reached through HTTP — the request never gets that far. Asserted
    // as a 404 so the real behaviour is pinned: a client that POSTs to a server without Remote sees axum's empty 404,
    // not a structured error telling it why.
    let server = start(None).await;

    let response = send(
        server.address,
        "POST",
        INVOKE_PATH,
        &[("Content-Type", "application/json")],
        br#"{"namespace":"test","method":"echo","args":{},"peer":"p"}"#,
    )
    .await;

    assert_eq!(
        response.status, 404,
        "unmounted route must be a 404, got {} ({})",
        response.status, response.body
    );
    assert!(
        response.body.is_empty(),
        "axum's fallback answers with an empty body, so a client has nothing to parse: {:?}",
        response.body
    );

    // The probe page, by contrast, is mounted unconditionally: it is how an operator checks the process is up before
    // any registry exists.
    let page = send(server.address, "GET", WEB_INDEX_PATH, &[], b"").await;
    assert_eq!(page.status, 200, "body: {}", page.body);
}

// ---- what the wire does not check ---------------------------------------------------------------

/// `peer` is carried, never verified -- and that is recorded, not forgotten.
///
/// `docs/phase6-scope.md` says it plainly: any client may claim to be any peer, and authentication is a deployment
/// concern rather than something this layer does. The test exists so the limitation cannot change silently: if someone
/// later adds peer checking, this fails and the document has to move with it. A security property that lives only in
/// prose is one that drifts.
#[tokio::test]
async fn a_claimed_peer_identity_is_not_verified() {
    let server = start(Some(Arc::new(test_registry()))).await;
    let body = json!({
        "namespace": "test",
        "method": "echo",
        // Two unrelated identities: if the server checked, at most one of these could be true.
        "args": { "value": "first-claim" },
        "peer": "tenant-a",
    })
    .to_string();
    let response = send(
        server.address,
        "POST",
        INVOKE_PATH,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
    .await;
    assert_eq!(response.status, 200, "the call is served, not refused");

    // The same call under a different claimed identity is served just as readily.
    let body = json!({
        "namespace": "test",
        "method": "echo",
        "args": { "value": "second-claim" },
        "peer": "tenant-b",
    })
    .to_string();
    let response = send(
        server.address,
        "POST",
        INVOKE_PATH,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
    .await;
    assert_eq!(response.status, 200);
    assert!(
        response.body.contains("second-claim"),
        "the second identity reached the handler too: {}",
        response.body
    );
}
