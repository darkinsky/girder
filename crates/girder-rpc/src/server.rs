//! The WebSocket transport: `/api/remote.mux`, one connection generation at a time.
//!
//! The server accepts the mux socket, sends `ready`, forwards host events as `emit` frames, and
//! accepts `RemoteEventResult` uplinks. It also forwards waterfalls the *host* initiates to a client
//! and waits for the answer — the mechanism upstream uses for approvals and user questions.
//!
//! # Why the connection is a generation, not a socket
//!
//! Upstream calls each established connection a "generation" and stamps every frame with its
//! `clientId`. That exists because a reconnect is not a resumption: the old socket is gone, any
//! waterfall it was answering is unanswerable, and the client must be told which generation its
//! results belong to. So a connection object here is created per socket, its pending waterfalls are
//! drained on close, and its id is echoed in every frame.
//!
//! # Reconnection
//!
//! The acceptance criterion is "state is consistent after reconnect". State here is: the client gets
//! a new generation id, the old pending waterfalls are abandoned, and the event stream resumes from
//! the new connection. Nothing accumulates across generations — which is what makes it consistent
//! rather than merely reconnected.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bounded_transport::{FrameKind, Outbound, SendOutcome};
use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use girder_core::EventBus;
use girder_core::api::gateway::{
    Connection, ConnectionOutcome, Credential, Peer, PeerDirectory, Rejection,
};
use tokio_util::sync::CancellationToken;

use crate::{
    DownlinkFrame, PendingWaterfalls, REMOTE_STREAM_MUX_PATH, RemoteEventId, RemoteEventOutcome,
    RemoteEventRejection, RemoteEventResult, SessionState,
};

/// The request header naming the API Peer a caller speaks for, in strict mode.
///
/// A header rather than the wire's own `peer` field: a body field is written by whoever sent the
/// body, so admitting on it would make the identity a claim again. This mirrors upstream, where the
/// credential is the socket's handshake, not the request payload.
///
/// A deployment that terminates a proxy or a browser session in front of this server is the one that
/// sets this header; nothing in this crate mints a credential.
pub const PEER_HEADER: &str = "x-girder-peer";

/// Counts connection generations across the process, so two clients never share an id.
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// What a waterfall layer returns to the host after asking the client.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientAnswer {
    /// The client delegated back to the host chain.
    Next,
    /// The client replaced the value.
    Value(serde_json::Value),
    /// The client refused.
    Rejected(RemoteEventRejection),
}

/// A failure raised by the transport.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The socket closed before the client answered.
    #[error("connection closed while awaiting client answer")]
    Closed,

    /// The client's result did not match the pending waterfall.
    #[error("unexpected result `{event_id}`: {reason}")]
    UnexpectedResult {
        /// The id the client sent.
        event_id: RemoteEventId,
        /// Why it was rejected.
        reason: String,
    },
}

/// A handle the host uses to push frames to one connected client.
///
/// Cloneable and `Send`, so a waterfall layer or an event forwarder can hold one without owning the
/// socket.
#[derive(Clone)]
pub struct ClientHandle {
    /// This generation's identity.
    pub client_id: String,
    outbound: Outbound,
    /// Resolutions for waterfalls awaiting a client answer.
    answers: Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<RemoteEventId, tokio::sync::oneshot::Sender<ClientAnswer>>,
        >,
    >,
    /// Waterfalls delivered and not yet answered, for diagnostics and disconnect cleanup.
    pending: Arc<tokio::sync::Mutex<PendingWaterfalls>>,
    next_id: Arc<AtomicU64>,
}

impl std::fmt::Debug for ClientHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientHandle")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl ClientHandle {
    /// Builds a handle over an existing outbound channel.
    ///
    /// For a caller that owns the write half itself — a test, or a future transport that is not a
    /// WebSocket. The production path constructs the same struct inside [`serve`].
    #[must_use]
    pub fn test_handle(client_id: impl Into<String>, outbound: Outbound) -> Self {
        Self {
            client_id: client_id.into(),
            outbound,
            answers: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            pending: Arc::new(tokio::sync::Mutex::new(PendingWaterfalls::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Sends a frame, reporting whether the client is still connected.
    pub fn send(&self, frame: DownlinkFrame) -> bool {
        // A full queue drops a notification and keeps the connection. A dropped `ready` or
        // `waterfall` is different: the client cannot join the generation, or the host waits on an
        // answer that will never arrive. Those are reported as a closed send so `ask` fails the
        // call instead of parking it.
        let kind = FrameKind::of(&frame);
        match self.outbound.send(frame) {
            SendOutcome::Queued => true,
            SendOutcome::Closed => false,
            SendOutcome::Dropped => !matches!(kind, FrameKind::Ready | FrameKind::Waterfall),
        }
    }

    /// Asks the client to resolve a waterfall, awaiting its answer.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::Closed`] when the connection drops first. That is not an error the
    /// caller can retry: the generation is gone, so the correct response is to fall back to the
    /// host's own behaviour rather than wait forever.
    pub async fn ask(
        &self,
        event: impl Into<String>,
        agent_id: impl Into<String>,
        request: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
    ) -> Result<ClientAnswer, TransportError> {
        let event = event.into();
        let value = self.next_id.fetch_add(1, Ordering::Relaxed);
        let event_id = format!("{}-{value}", self.client_id);

        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut answers = self.answers.lock().await;
            answers.insert(event_id.clone(), tx);
        }
        {
            let mut pending = self.pending.lock().await;
            pending.insert(event_id.clone(), event.clone());
        }

        let frame = DownlinkFrame::Waterfall {
            event,
            event_id: event_id.clone(),
            agent_id: agent_id.into(),
            request,
        };
        if !self.send(frame) {
            self.forget(&event_id).await;
            return Err(TransportError::Closed);
        }

        tokio::select! {
            () = cancel.cancelled() => {
                // Tell the client to stop working on it, then abandon.
                let _ = self.send(DownlinkFrame::Cancel { event_id: event_id.clone() });
                self.forget(&event_id).await;
                Err(TransportError::Closed)
            }
            answer = rx => {
                self.forget(&event_id).await;
                answer.map_err(|_| TransportError::Closed)
            }
        }
    }

    /// Number of waterfalls awaiting an answer.
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }

    async fn forget(&self, event_id: &str) {
        self.answers.lock().await.remove(event_id);
        self.pending.lock().await.take(event_id);
    }

    /// Applies one uplink result, returning it to the waiting [`ClientHandle::ask`].
    async fn deliver(&self, result: RemoteEventResult) -> Result<(), TransportError> {
        if result.client_id != self.client_id {
            return Err(TransportError::UnexpectedResult {
                event_id: result.event_id,
                reason: format!(
                    "generation mismatch (expected `{}`, got `{}`)",
                    self.client_id, result.client_id
                ),
            });
        }

        let Some(sender) = self.answers.lock().await.remove(&result.event_id) else {
            return Err(TransportError::UnexpectedResult {
                event_id: result.event_id,
                reason: "no pending waterfall with this id".to_owned(),
            });
        };
        self.pending.lock().await.take(&result.event_id);

        let answer = match result.outcome {
            RemoteEventOutcome::Next => ClientAnswer::Next,
            RemoteEventOutcome::Result { value } => {
                ClientAnswer::Value(value.unwrap_or(serde_json::Value::Null))
            }
            RemoteEventOutcome::Rejected { error } => ClientAnswer::Rejected(error),
        };
        // The receiver may have been dropped by a cancellation; that is not an error to report.
        let _ = sender.send(answer);
        Ok(())
    }
}

/// The RPC server: a router plus the state a connection needs.
#[derive(Clone)]
pub struct RpcServer {
    bus: Arc<EventBus>,
    /// Events forwarded to every client. Empty means none: defaulting to everything would leak host
    /// internals to any connected socket.
    watch: Arc<Vec<&'static str>>,
    /// Remote methods, when the caller supplied them.
    remote: Option<Arc<girder_core::RemoteRegistry>>,
    /// The admitted Peers, when this server verifies identity. `None` keeps the legacy path, where
    /// the request's own `peer` string is carried unverified — see [`RpcServer::with_peer_directory`]
    /// for why that switch is a builder and not a default.
    directory: Option<Arc<PeerDirectory>>,
    /// Origins outside loopback that may reach a strict server, spelled as a browser sends them.
    trusted_origins: Arc<Vec<String>>,
    /// Whether a trusted request with no credential speaks for the local owner.
    local_owner: bool,
    /// Optional browser frontend served from the router's fallback seat.
    static_frontend: Option<girder_core::host::webserver::StaticConfig>,
    /// Optional browser-session authenticator for the frontend index.
    browser_auth: Option<Arc<crate::BrowserAuth>>,
    /// Waterfall results shared by the WebSocket event stream and `/api` RPC.
    event_pending: crate::mux::PendingEvents,
    /// gzip level and minimum body size, when this listener compresses responses.
    gzip: Option<(u8, usize)>,
    /// PEM certificate chain and private key, when this listener speaks HTTPS.
    tls: Option<(String, String)>,
}

impl RpcServer {
    /// Creates a server over the given event bus, forwarding nothing.
    #[must_use]
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            bus,
            watch: Arc::new(Vec::new()),
            remote: None,
            directory: None,
            trusted_origins: Arc::new(Vec::new()),
            local_owner: false,
            static_frontend: None,
            browser_auth: None,
            event_pending: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
            gzip: None,
            tls: None,
        }
    }

    /// Sets the events forwarded to each connected client.
    #[must_use]
    pub fn watching(mut self, events: Vec<&'static str>) -> Self {
        self.watch = Arc::new(events);
        self
    }

    /// Installs the Remote method registry that the invoke endpoint dispatches to.
    #[must_use]
    pub fn with_remote(mut self, remote: Arc<girder_core::RemoteRegistry>) -> Self {
        self.remote = Some(remote);
        self
    }

    /// Installs a [`PeerDirectory`], which turns on verified identity.
    ///
    /// Without this, the server behaves exactly as it did before: `request.peer` is copied into the
    /// call and nothing checks it. With it, every request must be admitted by the directory first,
    /// the admitted [`Peer`] supplies the call's identity, and the wire's `peer` field can only
    /// agree or be refused — it cannot promote anyone. The switch is a builder rather than a default
    /// because the unverified mode is what every existing client and test speaks; flipping the
    /// default would have been a protocol change disguised as a security fix.
    ///
    /// See [`RpcServer::trusting_origins`] and [`RpcServer::allowing_local_owner`] for the two
    /// decisions a deployment still has to make.
    #[must_use]
    pub fn with_peer_directory(mut self, directory: Arc<PeerDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Allows these origins through the Host/Origin fence, in addition to loopback.
    ///
    /// A browser sends `Origin` on every cross-origin request, and the fence is what turns a page
    /// that rebounded onto this port into a 403. Loopback origins are trusted without configuration
    /// because a page served from `127.0.0.1` *is* this host; anything else — a GUI served from a
    /// LAN address, a reverse proxy — has to be named here. Entries are compared against the
    /// `Origin` header exactly (case-insensitively, trailing `/` ignored), because guessing at
    /// normalisation is how an allowlist stops being one.
    #[must_use]
    pub fn trusting_origins(mut self, origins: Vec<String>) -> Self {
        self.trusted_origins = Arc::new(origins);
        self
    }

    /// Lets a trusted credential-less request speak as the local owner.
    ///
    /// This is the browser case: a page served from this host has no API credential, and upstream
    /// answers it with the shared owner Peer after checking its cookie. This crate does not
    /// implement cookies, so it cannot tell that page from any other local client — hence off by
    /// default. A deployment whose only clients are local (a desktop app, a loopback-bound GUI) can
    /// turn it on deliberately; one behind a proxy should set a credential header instead.
    #[must_use]
    pub fn allowing_local_owner(mut self, allow: bool) -> Self {
        self.local_owner = allow;
        self
    }

    /// Serves a built frontend from the router's fallback seat.
    ///
    /// Named API routes still win over the fallback. The configured dist root is validated by
    /// [`girder_core::host::webserver::serve_static`] for every request, so a browser cannot turn
    /// the SPA entry point into a directory traversal primitive.
    #[must_use]
    pub fn with_static_frontend(
        mut self,
        config: girder_core::host::webserver::StaticConfig,
    ) -> Self {
        self.static_frontend = Some(config);
        self
    }

    /// Protects the frontend index with the browser launch-token exchange.
    #[must_use]
    pub fn with_browser_auth(mut self, auth: Arc<crate::BrowserAuth>) -> Self {
        self.browser_auth = Some(auth);
        self
    }

    /// Compresses eligible HTTP responses when the client accepts gzip.
    ///
    /// `level` is the DEFLATE level, 0 through 9. `threshold` is the smallest body that is worth
    /// compressing. WebSocket upgrades stay uncompressed.
    #[must_use]
    pub fn with_gzip(mut self, level: u8, threshold: usize) -> Self {
        self.gzip = Some((level, threshold));
        self
    }

    /// Serves this listener over HTTPS with a PEM certificate chain and its private key.
    #[must_use]
    pub fn with_tls(mut self, cert_pem: impl Into<String>, key_pem: impl Into<String>) -> Self {
        self.tls = Some((cert_pem.into(), key_pem.into()));
        self
    }

    /// Builds the axum router.
    ///
    /// The mux route is upstream's exact path; a second WebSocket path would let a client connect to
    /// something that is not the mux and then fail confusingly. The invoke endpoint is HTTP because
    /// upstream's unary calls are, and because a method result is not a stream.
    pub fn router(self) -> Router {
        let mut router = Router::new().route(REMOTE_STREAM_MUX_PATH, get(upgrade));
        if self.remote.is_some() {
            router = router
                .route(INVOKE_PATH, axum::routing::post(invoke))
                .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BYTES));
        }
        // The browser Connection transport uses one envelope for every `/api/<endpoint>` call.
        // Keep the older `remote.invoke` shape mounted for existing callers, but reserve the
        // endpoint namespace so `$events/result` and generated Remote methods share one contract.
        router = router
            .route("/api/{*endpoint}", axum::routing::post(rpc_call))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BYTES));
        if self.browser_auth.is_some() {
            router = router.route("/connect", get(connect_operator));
        }
        if self.static_frontend.is_some() {
            router = router.fallback(static_frontend);
        }
        if let Some((level, threshold)) = self.gzip {
            router = router.layer(axum::middleware::from_fn(move |request, next| {
                gzip_http_response(level, threshold, request, next)
            }));
        }
        router.with_state(self)
    }
}

/// Buffers one finished HTTP response and gzips it when the client and the body both allow it.
async fn gzip_http_response(
    level: u8,
    threshold: usize,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let accept = request
        .headers()
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let response = next.run(request).await;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS
        || response
            .headers()
            .contains_key(axum::http::header::CONTENT_ENCODING)
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_range = parts
        .headers
        .get(axum::http::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024 * 1024).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "response exceeded the gzip buffer",
        )
            .into_response();
    };
    let decision = girder_core::host::webserver::apply_gzip(
        &accept,
        content_type.as_deref(),
        content_range.as_deref(),
        bytes.to_vec(),
        level,
        threshold,
    );
    if decision.content_encoding.is_some() {
        parts.headers.insert(
            axum::http::header::CONTENT_ENCODING,
            axum::http::HeaderValue::from_static("gzip"),
        );
        parts.headers.insert(
            axum::http::header::VARY,
            axum::http::HeaderValue::from_static("Accept-Encoding"),
        );
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    }
    Response::from_parts(parts, axum::body::Body::from(decision.body))
}

/// The largest request body the invoke endpoint accepts, and the largest WebSocket message.
///
/// A tool call carries its arguments inline. Four mebibytes covers a source-file `write` and still
/// refuses a hostile body before it is parsed. The same number is the WebSocket frame ceiling and
/// the per-connection [`girder_core::Quota::max_request_bytes`].
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// How many downlink frames one connection may have queued.
///
/// Past this the producer drops the frame and counts it. `ready` and `waterfall` are treated as a
/// failed send so the host does not wait on a frame the socket will never write.
const OUTBOUND_CAPACITY: usize = 256;

/// The admission policy for one WebSocket.
///
/// The bucket allows a burst of one second and then refills at the same rate. The reader is the
/// only caller, so the in-flight ceiling bounds work that is still inside `deliver` or a subscribe
/// dispatch, not messages sitting in a process-wide queue.
fn connection_quota() -> girder_core::Quota {
    girder_core::Quota {
        in_flight: 32,
        per_second: 256,
        max_request_bytes: MAX_REQUEST_BYTES,
    }
}

/// Handles the envelope used by the upstream Connection RPC transport.
async fn rpc_call(
    State(server): State<RpcServer>,
    headers: HeaderMap,
    axum::extract::Path(endpoint): axum::extract::Path<String>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    if endpoint == "remote.invoke" && server.remote.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(object) = body.as_object() else {
        return rpc_error_response(
            "",
            "gateway/bad-request",
            "client-request must be an object",
        );
    };
    let rpc_id = object
        .get("rpcId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let method = object
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if object.get("type").and_then(serde_json::Value::as_str) != Some("client-request")
        || rpc_id.is_empty()
        || method != endpoint
        || !object.contains_key("payload")
    {
        return rpc_error_response(
            rpc_id,
            "gateway/bad-request",
            "invalid client-request envelope",
        );
    }
    let payload = object
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    if endpoint == girder_core::api::gateway::EVENTS_RESULT_ENDPOINT {
        if let Some(directory) = &server.directory {
            let peer = match admit(directory, &server, &headers) {
                Ok(peer) => peer,
                Err(rejection) => {
                    return rpc_error_response(
                        rpc_id,
                        "gateway/access-denied",
                        &format!("{rejection:?}"),
                    );
                }
            };
            if !peer.principal().is_owner() {
                return rpc_error_response(
                    rpc_id,
                    "gateway/invocation-unavailable",
                    "only the owner Peer may answer forwarded Remote events",
                );
            }
        }
        return match crate::mux::settle_result(&server.event_pending, &payload) {
            Ok(()) => rpc_success_response(rpc_id, serde_json::Value::Null),
            Err(error) => rpc_error_response(rpc_id, "gateway/protocol", &error),
        };
    }

    let Some(remote) = &server.remote else {
        return rpc_error_response(
            rpc_id,
            "gateway/service-unavailable",
            "this server exposes no Remote methods",
        );
    };
    let (namespace, method_name) = match endpoint.split_once('/') {
        Some((namespace, method)) if !namespace.is_empty() && !method.is_empty() => {
            (namespace, method)
        }
        _ => {
            return rpc_error_response(
                rpc_id,
                "gateway/bad-request",
                "Remote endpoint must be namespace/method",
            );
        }
    };
    let args = crate::mux::business_args(payload);
    let call = match &server.directory {
        None => girder_core::RemoteCall {
            args,
            peer: String::new(),
        },
        Some(directory) => {
            let peer = match admit(directory, &server, &headers) {
                Ok(peer) => peer,
                Err(rejection) => {
                    return rpc_error_response(
                        rpc_id,
                        "gateway/access-denied",
                        &format!("{rejection:?}"),
                    );
                }
            };
            match girder_core::api::gateway::admitted_call(&peer, args) {
                Ok(call) => call,
                Err(error) => {
                    return rpc_error_response(rpc_id, "gateway/access-denied", &error.to_string());
                }
            }
        }
    };
    match remote.dispatch(namespace, method_name, call).await {
        Ok(value) => rpc_success_response(rpc_id, value),
        Err(error) => rpc_error_response(rpc_id, error.code(), &error.to_string()),
    }
}

fn rpc_success_response(rpc_id: &str, value: serde_json::Value) -> Response {
    axum::Json(serde_json::json!({
        "type": "server-response",
        "rpcId": rpc_id,
        "result": {"ok": true, "value": value},
    }))
    .into_response()
}

fn rpc_error_response(rpc_id: &str, code: &str, message: &str) -> Response {
    axum::Json(serde_json::json!({
        "type": "server-response",
        "rpcId": rpc_id,
        "result": {
            "ok": false,
            "error": {"code": code, "message": message, "details": {}},
        },
    }))
    .into_response()
}

/// Answers a frontend fallback request through the same static-serving contract used by the host
/// implementation. The index gets the readiness marker and a relative base so a dist bundle works
/// at both `/` and a mounted path.
async fn static_frontend(
    State(server): State<RpcServer>,
    request: axum::extract::Request,
) -> Response {
    let Some(config) = server.static_frontend.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(auth) = server.browser_auth.as_ref()
        && is_static_index_path(config, request.uri().path())
    {
        let target = request
            .uri()
            .path_and_query()
            .map_or("/", axum::http::uri::PathAndQuery::as_str);
        match auth.authorize_index(
            request.method(),
            target,
            request.headers(),
            false,
            now_millis(),
        ) {
            crate::IndexAuth::Serve => {}
            crate::IndexAuth::Redirect {
                location,
                set_cookie,
            } => {
                let mut response = Response::new(axum::body::Body::empty());
                *response.status_mut() = StatusCode::SEE_OTHER;
                let headers = response.headers_mut();
                headers.insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                headers.insert(
                    axum::http::header::LOCATION,
                    axum::http::HeaderValue::from_static(location),
                );
                if let Ok(value) = set_cookie.parse() {
                    headers.insert(axum::http::header::SET_COOKIE, value);
                }
                return response;
            }
            crate::IndexAuth::Unauthorized { body } => {
                let mut response = Response::new(axum::body::Body::from(body));
                *response.status_mut() = StatusCode::UNAUTHORIZED;
                let headers = response.headers_mut();
                headers.insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
                );
                return response;
            }
        }
    }
    let if_none_match = request
        .headers()
        .get("if-none-match")
        .and_then(|value| value.to_str().ok());
    let static_request = girder_core::host::webserver::StaticRequest {
        method: request.method().as_str(),
        path: request.uri().path(),
        if_none_match,
    };
    let render_index = |html: &str| {
        let rendered = girder_core::host::webserver::render_index_injections(html, &[]);
        rendered.replacen("<head>", "<head><base href=\"./\">", 1)
    };
    match girder_core::host::webserver::serve_static(&static_request, config, &render_index) {
        Ok(static_response) => static_response_response(static_response),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            error.to_string(),
        )
            .into_response(),
    }
}

fn is_static_index_path(config: &girder_core::host::webserver::StaticConfig, path: &str) -> bool {
    if path == "/" {
        return true;
    }
    let Ok(relative) = config.dist_index.strip_prefix(&config.dist_root) else {
        return false;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    format!("/{relative}") == path
}

async fn connect_operator(
    State(server): State<RpcServer>,
    request: axum::extract::Request,
) -> Response {
    let Some(auth) = server.browser_auth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !connect_is_trusted(request.headers(), &server.trusted_origins) {
        return plain(StatusCode::FORBIDDEN, "forbidden");
    }
    let Some(directory) = server.directory.as_ref() else {
        return plain(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    if directory.is_disposed() {
        return plain(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let target = request
        .uri()
        .path_and_query()
        .map_or("/connect", axum::http::uri::PathAndQuery::as_str);
    match auth.authorize_connect(
        request.method(),
        target,
        request.headers(),
        false,
        now_millis(),
        |interactions| {
            directory
                .publish(girder_core::api::gateway::Principal::owner(), interactions)
                .id()
                .as_str()
                .to_owned()
        },
    ) {
        crate::ConnectAuth::Redirect {
            location,
            set_cookie,
            ..
        } => {
            let mut response = Response::new(axum::body::Body::empty());
            *response.status_mut() = StatusCode::FOUND;
            let headers = response.headers_mut();
            headers.insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            headers.insert(
                axum::http::header::LOCATION,
                axum::http::HeaderValue::from_static(location),
            );
            headers.insert(
                axum::http::header::REFERRER_POLICY,
                axum::http::HeaderValue::from_static("no-referrer"),
            );
            if let Ok(value) = set_cookie.parse() {
                headers.insert(axum::http::header::SET_COOKIE, value);
            }
            response
        }
        crate::ConnectAuth::BadRequest { body } => plain(StatusCode::BAD_REQUEST, body),
        crate::ConnectAuth::Unauthorized { body } => plain(StatusCode::UNAUTHORIZED, body),
    }
}

fn plain(status: StatusCode, body: impl Into<axum::body::Body>) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn connect_is_trusted(headers: &HeaderMap, trusted_origins: &[String]) -> bool {
    let Some(host) = headers.get("host").and_then(|value| value.to_str().ok()) else {
        return false;
    };
    if headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        == Some("cross-site")
    {
        return false;
    }
    let hostname = hostname_of(host);
    let named = trusted_origins.iter().any(|allowed| {
        allowed.eq_ignore_ascii_case(host) || allowed.eq_ignore_ascii_case(hostname)
    });
    if !hostname.eq_ignore_ascii_case("localhost")
        && !girder_core::is_loopback_host(hostname)
        && !named
    {
        return false;
    }
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let Ok(origin_url) = url::Url::parse(origin) else {
        return false;
    };
    if origin_url.scheme() != "http" && origin_url.scheme() != "https" {
        return false;
    }
    let Some(origin_host) = origin_url.host_str() else {
        return false;
    };
    let origin_authority = match origin_url.port() {
        Some(port) => format!("{origin_host}:{port}"),
        None => origin_host.to_owned(),
    };
    origin_authority.eq_ignore_ascii_case(host)
        || origin_host.eq_ignore_ascii_case(hostname_of(host))
}

fn hostname_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split_once(']').map_or(authority, |(host, _)| host);
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

/// Converts the pure static response into an axum response without dropping cache validators.
fn static_response_response(
    static_response: girder_core::host::webserver::StaticResponse,
) -> Response {
    let mut response = Response::new(axum::body::Body::from(static_response.body));
    *response.status_mut() =
        StatusCode::from_u16(static_response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = response.headers_mut();
    if let Some(content_type) = static_response.content_type {
        if let Ok(value) = content_type.parse() {
            headers.insert(axum::http::header::CONTENT_TYPE, value);
        }
    }
    if let Some(cache_control) = static_response.cache_control {
        if let Ok(value) = cache_control.parse() {
            headers.insert(axum::http::header::CACHE_CONTROL, value);
        }
    }
    if let Some(etag) = static_response.etag {
        if let Ok(value) = etag.parse() {
            headers.insert(axum::http::header::ETAG, value);
        }
    }
    if let Some(allow) = static_response.allow {
        if let Ok(value) = allow.parse() {
            headers.insert(axum::http::header::ALLOW, value);
        }
    }
    response
}

/// HTTP endpoint for unary Remote calls.
///
/// Upstream carries these over the same mux as logical streams. A separate POST is simpler and
/// equivalent for a unary call: there is no stream to multiplex.
pub const INVOKE_PATH: &str = "/api/remote.invoke";

/// Handles one unary Remote call.
///
/// # Two modes, one handler
///
/// Without a [`PeerDirectory`] this is the legacy path: `request.peer` is copied into the call and
/// carried unverified, which is what every client of this crate speaks today. With a directory the
/// request is admitted first, the admitted [`Peer`] builds the call through
/// [`admitted_call`](girder_core::api::gateway::admitted_call), and `request.peer` is only ever
/// compared — never used. The two modes are one handler so a deployment cannot end up running a
/// different code path than the one its tests exercised.
async fn invoke(
    State(server): State<RpcServer>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<crate::InvokeRemoteRequest>,
) -> Response {
    use axum::http::StatusCode;

    let Some(remote) = &server.remote else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(crate::InvokeResponse::Error {
                error: crate::RemoteEventRejection {
                    name: "NotFound".to_owned(),
                    message: "this server exposes no Remote methods".to_owned(),
                    code: Some("NOT_IMPLEMENTED".to_owned()),
                    details: None,
                },
            }),
        )
            .into_response();
    };

    // The claim, if there is one. `""` means "no claim" because the request type requires the field
    // and this crate does not own that type's serde attributes.
    let claimed = Some(request.peer.as_str()).filter(|peer| !peer.is_empty());

    let call = match &server.directory {
        None => girder_core::RemoteCall {
            args: serde_json::Value::Object(request.args),
            peer: request.peer,
        },
        Some(directory) => {
            let peer = match admit(directory, &server, &headers) {
                Ok(peer) => peer,
                Err(rejection) => {
                    return rejection_response(
                        status_of(rejection),
                        format!(
                            "the request was not admitted to `{}.{}`: {rejection:?}",
                            request.namespace, request.method
                        ),
                    );
                }
            };
            // Only now, with an identity the transport chose, is the wire field worth looking at:
            // it must agree. A disagreement is refused rather than resolved in the Peer's favour,
            // because the caller believes it is someone else and silently serving the request would
            // leave that belief intact.
            if let Some(claimed) = claimed {
                if claimed != peer.id().as_str() {
                    return rejection_response(
                        StatusCode::FORBIDDEN,
                        format!(
                            "the request claims peer `{claimed}` but was admitted as `{}`",
                            peer.id()
                        ),
                    );
                }
            }
            match girder_core::api::gateway::admitted_call(
                &peer,
                serde_json::Value::Object(request.args),
            ) {
                Ok(call) => call,
                Err(error) => {
                    return rejection_response(StatusCode::FORBIDDEN, error.to_string());
                }
            }
        }
    };

    let response = match remote
        .dispatch(&request.namespace, &request.method, call)
        .await
    {
        Ok(value) => crate::InvokeResponse::Ok { value },
        Err(error) => crate::InvokeResponse::Error {
            error: crate::RemoteEventRejection {
                name: "RemoteError".to_owned(),
                message: error.to_string(),
                code: Some(error.code().to_owned()),
                details: None,
            },
        },
    };

    // A method that ran and reported a domain failure is a successful HTTP exchange: the transport
    // did its job. A 500 would make a client retry a call that already has an answer.
    (StatusCode::OK, axum::Json(response)).into_response()
}

/// A JSON refusal, so a strict-mode client sees the same envelope as any other answer.
///
/// The status carries the admission outcome and the body carries the reason: a client that only
/// checks the status still learns *that* it was refused, and one that reads the body learns why.
fn rejection_response(status: axum::http::StatusCode, message: String) -> Response {
    (
        status,
        axum::Json(crate::InvokeResponse::Error {
            error: crate::RemoteEventRejection {
                name: "AccessDenied".to_owned(),
                message,
                code: Some("remote-access/denied".to_owned()),
                details: None,
            },
        }),
    )
        .into_response()
}

/// Admits one request, or reports that it was refused.
///
/// The two inputs [`PeerDirectory::admit`] takes are decided here in the order upstream decides
/// them: the Host/Origin fence, then the credential. The credential is a header, never the body: the
/// body's `peer` is a claim, and admitting on it would restore exactly what this path removes. The
/// caller compares the claim afterwards, once an identity exists to compare it against.
fn admit(
    directory: &PeerDirectory,
    server: &RpcServer,
    headers: &HeaderMap,
) -> Result<Peer, Rejection> {
    let trusted = origin_is_trusted(headers, &server.trusted_origins);
    let presented = presented_credential(server, headers);
    let credential = match &presented {
        PresentedCredential::Browser => Credential::Browser,
        PresentedCredential::Api(id) => Credential::Api(id),
        PresentedCredential::None => Credential::None,
    };
    directory.admit(trusted, credential).into()
}

enum PresentedCredential {
    Browser,
    Api(String),
    None,
}

/// The credential this request actually carries.
///
/// A mounted browser authenticator reads the signed cookie and ignores `x-girder-peer`: that header
/// is a claim, and accepting it beside a cookie would let a caller pick an operator it does not
/// hold. Without the authenticator, the header remains the deployment credential the existing
/// clients send.
fn presented_credential(server: &RpcServer, headers: &HeaderMap) -> PresentedCredential {
    if let Some(auth) = &server.browser_auth {
        let now = now_millis();
        if auth.is_authenticated(headers, false, now) {
            return PresentedCredential::Browser;
        }
        if let Some(id) = auth.authenticate_api(headers, false, now) {
            return PresentedCredential::Api(id);
        }
        if server.local_owner {
            return PresentedCredential::Browser;
        }
        return PresentedCredential::None;
    }
    match headers
        .get(PEER_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        Some(id) => PresentedCredential::Api(id.to_owned()),
        None if server.local_owner => PresentedCredential::Browser,
        None => PresentedCredential::None,
    }
}

/// Whether the request passed the Host/Origin fence.
///
/// A missing `Origin` is trusted: browsers attach it to cross-origin POSTs, so its absence means the
/// caller is not a page — and a page is the only thing that can be rebound onto this port. A present
/// `Origin` is trusted when it is loopback (a page served by this host) or when the deployment named
/// it in [`RpcServer::trusting_origins`]. Everything else, including the literal `null` a sandboxed
/// frame sends, is refused.
fn origin_is_trusted(headers: &HeaderMap, trusted_origins: &[String]) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let trimmed = origin.trim_end_matches('/');
    if trusted_origins
        .iter()
        .any(|allowed| allowed.trim_end_matches('/').eq_ignore_ascii_case(trimmed))
    {
        return true;
    }
    let Some((_scheme, authority)) = trimmed.split_once("://") else {
        return false;
    };
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _port)| host);
    // `localhost` is a name, not an address, so the literal parser does not accept it — but it is
    // the spelling a page served from this machine most often sends, and refusing it would make the
    // loopback rule unusable in practice.
    host.eq_ignore_ascii_case("localhost") || girder_core::is_loopback_host(host)
}

/// Rewrites one uplink control to speak for the admitted Peer, or refuses it.
///
/// [`SubscriptionHub`](crate::SubscriptionHub) reads the identity from the request it is handed, and
/// that type belongs to the protocol layer rather than this one. Rather than teaching the hub about
/// Peers, the transport admits the control here and then *replaces* the claimed identity with the
/// admitted one: after this function returns, there is no path by which a subscription can speak for
/// a Peer other than the socket's.
///
/// `None` means the control was refused and a [`SubscribeResult`](crate::DownlinkFrame::SubscribeResult)
/// with `ok: false` has already been sent. The socket stays open — one bad request must not cost the
/// client its session, which is the same rule the hub applies to its own refusals.
///
/// A `subscribe` that names no Peer is accepted: the identity it would otherwise carry is the
/// connection's, and requiring a redundant echo would break clients that never sent one.
fn admit_control(
    peer: &Peer,
    handle: &ClientHandle,
    control: crate::SubscriptionControl,
) -> Option<crate::SubscriptionControl> {
    let crate::SubscriptionControl::Subscribe(mut request) = control else {
        // `unsubscribe` carries an id and nothing else, so there is no identity to check.
        return Some(control);
    };
    let claimed = request.peer.clone();
    let refused = if !claimed.is_empty() && claimed.as_str() != peer.id().as_str() {
        Some(format!(
            "this connection was admitted as `{}`, not `{claimed}`",
            peer.id()
        ))
    } else {
        peer.assert_live(&request.qualified())
            .err()
            .map(|error| error.to_string())
    };
    if let Some(reason) = refused {
        handle.send(crate::DownlinkFrame::SubscribeResult {
            id: request.id,
            ok: false,
            error: Some(reason),
        });
        return None;
    }
    peer.id().as_str().clone_into(&mut request.peer);
    Some(crate::SubscriptionControl::Subscribe(request))
}

/// Admits one socket, returning its [`Connection`] or the status to refuse the upgrade with.
///
/// The upgrade is refused *before* the socket is established: a client that was never admitted must
/// not observe a `ready` frame, because `ready` is the server saying "you are connected".
fn admit_socket(
    server: &RpcServer,
    headers: &HeaderMap,
) -> Result<Option<Connection>, axum::http::StatusCode> {
    let Some(directory) = &server.directory else {
        return Ok(None);
    };
    let trusted = origin_is_trusted(headers, &server.trusted_origins);
    let presented = presented_credential(server, headers);
    let credential = match &presented {
        PresentedCredential::Browser => Credential::Browser,
        PresentedCredential::Api(id) => Credential::Api(id),
        PresentedCredential::None => Credential::None,
    };
    match ConnectionOutcome::into(directory.admit_connection(trusted, credential)) {
        Ok(connection) => Ok(Some(connection)),
        Err(rejection) => Err(status_of(rejection)),
    }
}

/// The HTTP status a refusal is written with.
fn status_of(rejection: Rejection) -> axum::http::StatusCode {
    axum::http::StatusCode::from_u16(rejection.status())
        .unwrap_or(axum::http::StatusCode::FORBIDDEN)
}

async fn upgrade(
    ws: WebSocketUpgrade,
    State(server): State<RpcServer>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let connection = match admit_socket(&server, &headers) {
        Ok(connection) => connection,
        Err(status) => return status.into_response(),
    };
    // The upstream stream mux is the protocol on this path. The old event generation remains
    // available only for callers that explicitly opt into it while local compatibility consumers
    // migrate; browser, Node, and other clients all get the standard mux by default.
    let legacy = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .any(|part| part == "legacy=1");
    ws.max_message_size(MAX_REQUEST_BYTES)
        .max_frame_size(MAX_REQUEST_BYTES)
        .on_upgrade(move |socket| async move {
            if !legacy {
                crate::mux::serve(
                    socket,
                    Arc::clone(&server.bus),
                    Arc::clone(&server.watch),
                    server.remote.clone(),
                    connection,
                    Arc::clone(&server.event_pending),
                )
                .await;
            } else {
                let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
                let client_id = format!("gen-{generation}");
                let state = Arc::new(SessionState::new(client_id, Arc::clone(&server.bus)));
                serve_full(
                    socket,
                    state,
                    server.watch.as_ref().clone(),
                    server.remote.clone(),
                    connection,
                )
                .await;
            }
        })
        .into_response()
}

/// Runs one connection to completion.
///
/// Split from the axum handler so a test can drive it over a duplex stream without a real socket.
/// Without a Remote registry there is nothing to subscribe to, so this is [`serve_full`] with the
/// subscription path turned off.
pub async fn serve(socket: WebSocket, state: Arc<SessionState>, watch: Vec<&'static str>) {
    serve_full(socket, state, watch, None, None).await;
}

/// Runs one connection with an optional subscription hub and an optional admitted Peer.
///
/// One reader loop serves both uplinks because the socket is the same: dispatching on `type` costs a
/// field read, while a second reader would need the socket split and would race for the frames.
///
/// # The Peer is the connection's, not the frame's
///
/// `connection` is `Some` exactly when this server was built with a [`PeerDirectory`]
/// *and* the socket was admitted. When it is, every subscription the socket opens speaks for that
/// Peer: [`admitted_call`] builds the call, a disposed Peer
/// stops the poller, and the `peer` field on each `subscribe` frame is refused if it disagrees. When
/// it is `None` the legacy behaviour is unchanged — the frame's own `peer` string is carried.
async fn serve_full(
    socket: WebSocket,
    state: Arc<SessionState>,
    watch: Vec<&'static str>,
    remote: Option<Arc<girder_core::RemoteRegistry>>,
    connection: Option<Connection>,
) {
    use futures::stream::StreamExt as _;

    let (outbound_tx, mut outbound_rx) = Outbound::new(OUTBOUND_CAPACITY);
    let (mut sink, mut stream) = socket.split();

    let handle = ClientHandle {
        client_id: state.client_id.clone(),
        outbound: outbound_tx.clone(),
        answers: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        // Shared, not copied: the same map the session state drains on close.
        pending: Arc::clone(&state.pending),
        next_id: Arc::new(AtomicU64::new(1)),
    };

    // The Peer this socket speaks for. A clone is kept for the teardown so "the socket closed" and
    // "the Peer is gone" stay one event; `SubscriptionHub` gets its own clone, which shares the same
    // lifetime because every `Peer` clone shares one.
    let peer = connection
        .as_ref()
        .map(|connection| connection.peer().clone());

    // Built before the first frame is written so a client that subscribes immediately after `ready`
    // finds a hub rather than a race. Absent when the server exposes no methods: a subscription to a
    // method nobody registered would only ever fail.
    let hub = remote.map(|remote| {
        Arc::new(crate::SubscriptionHub::new(
            remote,
            outbound_tx,
            crate::DEFAULT_SUBSCRIPTION_LIMIT,
        ))
    });

    // Subscribe before the first frame is written, so no dispatch is missed between the client
    // seeing `ready` and the forwarder starting.
    let forwarder = forward_events(handle.clone(), &state.bus, watch);

    // The first frame on every generation is `ready`: a client must not send a result before it
    // knows which generation it belongs to.
    handle.send(state.ready());

    // Pump outbound frames onto the socket.
    let writer = {
        use futures::SinkExt as _;
        tokio::spawn(async move {
            while let Some(frame) = outbound_rx.recv().await {
                let Ok(text) = crate::encode_downlink(&frame) else {
                    continue;
                };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        })
    };

    // Read uplinks until the socket closes. One limiter per socket: a client that spends its
    // quota refuses only itself.
    {
        use futures::StreamExt as _;
        let mut limiter = girder_core::Limiter::new(connection_quota(), std::time::Instant::now());
        while let Some(message) = stream.next().await {
            let Ok(message) = message else { break };
            let Message::Text(text) = message else {
                continue;
            };
            let permit = match limiter.admit(std::time::Instant::now(), text.len()) {
                Ok(permit) => permit,
                Err(refusal) => {
                    tracing::debug!(%refusal, "refusing uplink");
                    continue;
                }
            };
            // A subscription control is recognised by its `type` before the result parse: both are
            // tagged, and attempting the result shape first would log every `subscribe` as garbage.
            if let Ok(control) = serde_json::from_str::<crate::SubscriptionControl>(&text) {
                let control = match peer.as_ref() {
                    Some(peer) => admit_control(peer, &handle, control),
                    None => Some(control),
                };
                if let (Some(control), Some(hub)) = (control, hub.as_ref()) {
                    hub.handle(control);
                }
                limiter.release(permit);
                continue;
            }
            let Ok(result) = serde_json::from_str::<RemoteEventResult>(&text) else {
                // An unparseable uplink is not fatal: the client may be speaking a newer protocol.
                // Dropping the frame keeps the generation alive for everything else.
                tracing::debug!(frame = %text, "ignoring unparseable uplink frame");
                limiter.release(permit);
                continue;
            };
            if let Err(error) = handle.deliver(result).await {
                tracing::debug!(%error, "ignoring uplink result");
            }
            limiter.release(permit);
        }
    }

    // Disconnect: nothing can answer the outstanding waterfalls, so they are abandoned rather than
    // leaked into the next generation. Subscriptions are stopped for the same reason — a poller that
    // outlived its socket would keep calling methods nobody can read the answers to.
    if let Some(hub) = hub.as_ref() {
        hub.shutdown(&crate::EndReason::Failed {
            reason: "the connection closed".to_owned(),
        });
    }
    let abandoned = state.pending.lock().await.drain();
    if abandoned > 0 {
        tracing::debug!(abandoned, "connection closed with pending waterfalls");
    }
    // The Peer was the socket's, so the socket ending disposes it. Done after the hub shutdown so a
    // poller that was mid-call is aborted before its Peer is refused, rather than racing it.
    //
    // `release` retires a published Peer from its directory as well as disposing it, which is
    // `gateway_gap`'s rule: a Peer's lifetime is a socket's lifetime, and an operator identity that
    // came back after its socket closed would be a new identity anyway. A deployment that expects
    // reconnects under one credential publishes a fresh Peer per connection; this crate does not
    // mint or recycle them.
    if let Some(connection) = connection {
        connection.release();
    }
    writer.abort();
    forwarder.abort();
}

/// Serves the mux endpoint on `listener` until `cancel` fires.
///
/// # Errors
///
/// Returns the underlying I/O error when the listener fails.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    bus: Arc<EventBus>,
    watch: Vec<&'static str>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    serve_with(listener, bus, watch, None, cancel).await
}

/// Serves the mux endpoint and the invoke endpoint, exposing `remote` methods.
///
/// # Errors
///
/// Returns the underlying I/O error when the listener fails.
pub async fn serve_with(
    listener: tokio::net::TcpListener,
    bus: Arc<EventBus>,
    watch: Vec<&'static str>,
    remote: Option<Arc<girder_core::RemoteRegistry>>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    serve_with_options(
        listener, bus, watch, remote, None, None, None, None, None, cancel,
    )
    .await
}

/// Serves with peer identity enforced.
///
/// The separate entry point rather than a flag on [`serve_with`]: turning admission on changes what
/// a client must send, and a deployment that gets it half-right has connections that fail in ways
/// the caller cannot diagnose from a boolean. A caller that wants admission calls this one and knows
/// it did.
///
/// # Errors
///
/// The listener's error, as [`serve_with`] reports it.
pub async fn serve_with_peers(
    listener: tokio::net::TcpListener,
    bus: Arc<EventBus>,
    watch: Vec<&'static str>,
    remote: Option<Arc<girder_core::RemoteRegistry>>,
    directory: Arc<girder_core::api::gateway::PeerDirectory>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    serve_with_options(
        listener,
        bus,
        watch,
        remote,
        Some(directory),
        None,
        None,
        None,
        None,
        cancel,
    )
    .await
}

/// Serves the mux, Remote methods, an optional admitted-peer directory, and an optional built
/// frontend from one listener.
///
/// This is the composition entry point used by the web profile. The older [`serve_with`] and
/// [`serve_with_peers`] functions remain as convenience wrappers for deployments without a dist
/// directory.
pub async fn serve_with_options(
    listener: tokio::net::TcpListener,
    bus: Arc<EventBus>,
    watch: Vec<&'static str>,
    remote: Option<Arc<girder_core::RemoteRegistry>>,
    directory: Option<Arc<girder_core::api::gateway::PeerDirectory>>,
    static_frontend: Option<girder_core::host::webserver::StaticConfig>,
    browser_auth: Option<Arc<crate::BrowserAuth>>,
    gzip: Option<(u8, usize)>,
    tls: Option<(String, String)>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let mut server = RpcServer::new(bus).watching(watch);
    if let Some(remote) = remote {
        server = server.with_remote(remote);
    }
    if let Some(directory) = directory {
        server = server.with_peer_directory(directory);
    }
    if let Some(static_frontend) = static_frontend {
        server = server.with_static_frontend(static_frontend);
    }
    if let Some(browser_auth) = browser_auth {
        server = server.with_browser_auth(browser_auth);
    }
    if let Some((level, threshold)) = gzip {
        server = server.with_gzip(level, threshold);
    }
    if let Some((cert_pem, key_pem)) = tls {
        server = server.with_tls(cert_pem, key_pem);
    }
    serve_server(listener, server, cancel).await
}

/// Runs one assembled server to graceful shutdown.
async fn serve_server(
    listener: tokio::net::TcpListener,
    server: RpcServer,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let tls = server.tls.clone();
    let router = server.router();
    if let Some((cert_pem, key_pem)) = tls {
        let listener = crate::tls::TlsListener::new(listener, &cert_pem, &key_pem)?;
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
    } else {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
    }
}

/// Forwards bus dispatches to a client as `emit` frames.
///
/// A push subscription, not a poll: the bus hands over every dispatch of the watched events with its
/// arguments. The earlier version polled `active_events` every 50 ms, which reported only *which
/// names had listeners* — losing the arguments entirely and unable to represent the same event
/// firing twice. That was the wrong shape, not just an inefficient one.
///
/// `events` selects what the client may see. An empty list means nothing is forwarded: the
/// projection of "which events a client is allowed to observe" is a decision of the caller, and
/// defaulting to everything would leak host internals to any connected socket.
pub fn forward_events(
    handle: ClientHandle,
    bus: &EventBus,
    events: Vec<&'static str>,
) -> tokio::task::JoinHandle<()> {
    // Subscribe *before* yielding to the runtime. Spawning first and subscribing inside the task
    // leaves a window in which a dispatch is missed — the caller has no way to know when the
    // subscription became live, so it would have to sleep, which is not an API.
    let mut observed = if events.is_empty() {
        None
    } else {
        Some(bus.observe(Some(events)))
    };

    tokio::spawn(async move {
        let Some(observed) = observed.as_mut() else {
            return;
        };
        while let Some(event) = observed.recv().await {
            let args = match event.args {
                // The wire carries positional arguments; a single-object payload stays one argument
                // rather than being flattened, so a client's decoder sees the same shape the host
                // dispatched.
                serde_json::Value::Array(items) => items,
                other => vec![other],
            };
            if !handle.send(DownlinkFrame::Emit {
                event: event.name.to_owned(),
                args,
            }) {
                // The client is gone; stop watching.
                break;
            }
            // A slow client is bounded by the bus's observer queue: when it fills, the observer is
            // dropped and `recv` returns `None`, ending this loop. Buffering without bound would
            // turn one stalled socket into unbounded memory.
        }
    })
}
