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

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use girder_core::EventBus;
use girder_core::gateway_gap::{
    Connection, ConnectionOutcome, Credential, Peer, PeerDirectory, Rejection,
};
use tokio::sync::mpsc;
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
    outbound: mpsc::UnboundedSender<DownlinkFrame>,
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
    pub fn test_handle(
        client_id: impl Into<String>,
        outbound: mpsc::UnboundedSender<DownlinkFrame>,
    ) -> Self {
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
        self.outbound.send(frame).is_ok()
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

    /// Builds the axum router.
    ///
    /// The mux route is upstream's exact path; a second WebSocket path would let a client connect to
    /// something that is not the mux and then fail confusingly. The invoke endpoint is HTTP because
    /// upstream's unary calls are, and because a method result is not a stream.
    pub fn router(self) -> Router {
        let mut router = Router::new()
            .route(REMOTE_STREAM_MUX_PATH, get(upgrade))
            // The probe page. Served from the same process as the API so a browser has no second
            // origin to configure, and so opening it proves the server is running.
            .route(crate::WEB_INDEX_PATH, get(web_index));
        if self.remote.is_some() {
            // The bound the server actually applied before this line was axum's default of 2 MiB -- an accidental ceiling
            // rather than a configured one. `rpc_limits` documents a `max_request_bytes` and has no caller in this crate,
            // so a deployment tuning it changed nothing.
            router = router
                .route(INVOKE_PATH, axum::routing::post(invoke))
                .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BYTES));
        }
        router.with_state(self)
    }
}

/// The largest request body the invoke endpoint accepts.
///
/// Chosen to match what `rpc_limits::Quota::default` describes. A tool call carries its arguments inline and the largest
/// one this build produces is a `write` of a source file, so a megabyte is generous for the protocol and small enough that
/// a hostile body is refused before it is parsed.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// Serves the embedded probe page.
///
/// A literal HTML response rather than a file: no build step, no path traversal check, and nothing
/// to keep in sync with a bundler.
async fn web_index() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        crate::INDEX_HTML,
    )
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
/// [`admitted_call`](girder_core::gateway_gap::admitted_call), and `request.peer` is only ever
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
            match girder_core::gateway_gap::admitted_call(
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
    let credential = match headers
        .get(PEER_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        Some(id) => Credential::Api(id),
        // A credential-less request is the browser case. It is only allowed to be the owner when the
        // deployment said so; otherwise `Credential::None` makes the directory answer 401 rather
        // than guessing.
        None if server.local_owner => Credential::Browser,
        None => Credential::None,
    };
    directory.admit(trusted, credential).into()
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
    let credential = match headers
        .get(PEER_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        Some(id) => Credential::Api(id),
        None if server.local_owner => Credential::Browser,
        None => Credential::None,
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
) -> Response {
    let connection = match admit_socket(&server, &headers) {
        Ok(connection) => connection,
        Err(status) => return status.into_response(),
    };
    ws.on_upgrade(move |socket| async move {
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

    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<DownlinkFrame>();
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

    // Read uplinks until the socket closes.
    {
        use futures::StreamExt as _;
        while let Some(message) = stream.next().await {
            let Ok(message) = message else { break };
            let Message::Text(text) = message else {
                continue;
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
                continue;
            }
            let Ok(result) = serde_json::from_str::<RemoteEventResult>(&text) else {
                // An unparseable uplink is not fatal: the client may be speaking a newer protocol.
                // Dropping the frame keeps the generation alive for everything else.
                tracing::debug!(frame = %text, "ignoring unparseable uplink frame");
                continue;
            };
            if let Err(error) = handle.deliver(result).await {
                tracing::debug!(%error, "ignoring uplink result");
            }
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
    let mut server = RpcServer::new(bus).watching(watch);
    if let Some(remote) = remote {
        server = server.with_remote(remote);
    }
    serve_server(listener, server, cancel).await
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
    directory: Arc<girder_core::gateway_gap::PeerDirectory>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let mut server = RpcServer::new(bus)
        .watching(watch)
        .with_peer_directory(directory);
    if let Some(remote) = remote {
        server = server.with_remote(remote);
    }
    serve_server(listener, server, cancel).await
}

/// Runs one assembled server to graceful shutdown.
async fn serve_server(
    listener: tokio::net::TcpListener,
    server: RpcServer,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    axum::serve(listener, server.router())
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
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
