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
use axum::response::IntoResponse;
use axum::routing::get;
use girder_core::EventBus;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    DownlinkFrame, PendingWaterfalls, REMOTE_STREAM_MUX_PATH, RemoteEventId, RemoteEventOutcome,
    RemoteEventRejection, RemoteEventResult, SessionState,
};

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
}

impl RpcServer {
    /// Creates a server over the given event bus, forwarding nothing.
    #[must_use]
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            bus,
            watch: Arc::new(Vec::new()),
            remote: None,
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

    /// Builds the axum router.
    ///
    /// The mux route is upstream's exact path; a second WebSocket path would let a client connect to
    /// something that is not the mux and then fail confusingly. The invoke endpoint is HTTP because
    /// upstream's unary calls are, and because a method result is not a stream.
    pub fn router(self) -> Router {
        let mut router = Router::new().route(REMOTE_STREAM_MUX_PATH, get(upgrade));
        if self.remote.is_some() {
            router = router.route(INVOKE_PATH, axum::routing::post(invoke));
        }
        router.with_state(self)
    }
}

/// HTTP endpoint for unary Remote calls.
///
/// Upstream carries these over the same mux as logical streams. A separate POST is simpler and
/// equivalent for a unary call: there is no stream to multiplex.
pub const INVOKE_PATH: &str = "/api/remote.invoke";

/// Handles one unary Remote call.
async fn invoke(
    State(server): State<RpcServer>,
    axum::Json(request): axum::Json<crate::InvokeRemoteRequest>,
) -> impl IntoResponse {
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
        );
    };

    let call = girder_core::RemoteCall {
        args: serde_json::Value::Object(request.args),
        peer: request.peer,
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
    (StatusCode::OK, axum::Json(response))
}

async fn upgrade(ws: WebSocketUpgrade, State(server): State<RpcServer>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        let client_id = format!("gen-{generation}");
        let state = Arc::new(SessionState::new(client_id, Arc::clone(&server.bus)));
        serve(socket, state, server.watch.as_ref().clone()).await;
    })
}

/// Runs one connection to completion.
///
/// Split from the axum handler so a test can drive it over a duplex stream without a real socket.
pub async fn serve(socket: WebSocket, state: Arc<SessionState>, watch: Vec<&'static str>) {
    use futures::stream::StreamExt as _;

    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<DownlinkFrame>();
    let (mut sink, mut stream) = socket.split();

    let handle = ClientHandle {
        client_id: state.client_id.clone(),
        outbound: outbound_tx,
        answers: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        // Shared, not copied: the same map the session state drains on close.
        pending: Arc::clone(&state.pending),
        next_id: Arc::new(AtomicU64::new(1)),
    };

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
    // leaked into the next generation.
    let abandoned = state.pending.lock().await.drain();
    if abandoned > 0 {
        tracing::debug!(abandoned, "connection closed with pending waterfalls");
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
        }
    })
}
