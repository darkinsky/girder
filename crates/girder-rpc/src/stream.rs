//! Subscriptions: a client asking for a series of values rather than one.
//!
//! Some Remote methods cannot answer once. `session.observeEvent`, `job.follow`, and
//! `session.follow` produce values over time, and a client that had to poll would either miss events
//! between polls or spend a round trip per interval.
//!
//! # Why the host drives the pace
//!
//! A subscription belongs to the host, not the client. The host knows when something changed, so it
//! sends a frame then; a client-paced stream would need the client to know when to ask, which is the
//! same knowledge the host has and the client does not.
//!
//! # Polling, not events (a documented gap)
//!
//! The transport is real — frames travel the mux socket, a client can open and close a subscription,
//! and an end is visible — but the *source* of values is a timer, not a notification. Each
//! subscription re-invokes its method every [`DEFAULT_POLL_INTERVAL`] and forwards whatever came
//! back. That is not the design this module wants; it is what the layers below can express today:
//!
//! - A Remote method is `Fn(RemoteCall) -> BoxFut<Result<Value>>` — one call in, one value out. It
//!   has no way to hand back a series, so the host has nothing to await between values.
//! - Nothing in the registry marks a method as streaming, so dispatch cannot tell `job.follow` from
//!   `job.list`. [`SUBSCRIBABLE_METHODS`] is that knowledge, held here, and it is a list of names
//!   rather than a property of the handlers.
//! - The event bus (`girder_core::EventBus`) can notify the host, and the mux already forwards emits,
//!   but there is no mapping from a bus dispatch to "which subscription wants it": the arguments of
//!   `agent/turn` are not the return value of `session.observeEvent`.
//!
//! Making this event-driven needs one change in the method layer, which this crate does not own: a
//! streaming flag on the descriptor plus a handler that returns a receiver. Until then, polling is the
//! honest implementation, and the costs are visible in the protocol rather than hidden: a value can
//! repeat between polls, a burst of changes can be collapsed into one, and a client's view can lag by
//! up to one interval. `seq` is what lets a client detect the second case; the first case is why
//! [`SubscriptionHub`] does not forward an unchanged result — "nothing changed" is not a value.
//!
//! # The protocol in one screen
//!
//! ```text
//! client -> host  {"type":"subscribe","id":"s1","namespace":"job","method":"follow",
//!                  "args":{"jobId":"j1"},"peer":"web","intervalMs":250}
//! host -> client  {"type":"subscribeResult","id":"s1","ok":true}
//! host -> client  {"type":"subscription","frame":{"type":"value","subscriptionId":"s1",
//!                  "method":"job.follow","value":{},"seq":1}}
//! client -> host  {"type":"unsubscribe","id":"s1"}
//! host -> client  {"type":"subscription","frame":{"type":"end","subscriptionId":"s1",
//!                  "reason":{"kind":"unsubscribed"}}}
//! ```
//!
//! `subscribeResult` exists because a refusal has no frame to carry: a rejected subscription has no
//! series to end, and reusing `end` for one would make the client decide whether `end` meant "the
//! subscription you opened finished" or "there was never one". A separate ack keeps both cases
//! unambiguous, and it arrives before any value because it is written in the same step that opens
//! the subscription.
//!
//! # Why the id is client-chosen
//!
//! A client opening two subscriptions to the same method must be able to tell the frames apart. A
//! host-assigned id would need a round trip before the first value, and the client would have to
//! buffer values that arrived before it learned its own id.
//!
//! # Why a closed subscription is a frame rather than silence
//!
//! Silence is indistinguishable from a broken connection. A client waiting for a value that will
//! never come has no way to decide between "nothing happened" and "the stream is gone", so the host
//! says which.

use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Identifies one subscription, chosen by the client.
///
/// Client-chosen so a client can name it before the first value arrives; two subscriptions to one
/// method would otherwise be indistinguishable.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct SubscriptionId(pub String);

impl SubscriptionId {
    /// Builds an id.
    ///
    /// # Errors
    ///
    /// Returns `None` for an empty id: it would name every subscription at once.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a subscription ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EndReason {
    /// The host finished producing values.
    Completed,
    /// The client asked to stop.
    Unsubscribed,
    /// The host could not continue.
    Failed {
        /// Why.
        reason: String,
    },
}

/// One frame a subscription produces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubscriptionFrame {
    /// A value.
    Value {
        /// Which subscription.
        #[serde(rename = "subscriptionId")]
        subscription_id: SubscriptionId,
        /// The method that produced it, for a client that multiplexes several.
        method: String,
        /// The value.
        value: serde_json::Value,
        /// Where in the series this value sits.
        ///
        /// A sequence the client can compare, so a gap is detectable: a client that cannot tell
        /// frame 3 from frame 5 would render a missing update as continuity.
        seq: u64,
    },

    /// The subscription has ended.
    End {
        /// Which subscription.
        #[serde(rename = "subscriptionId")]
        subscription_id: SubscriptionId,
        /// Why.
        reason: EndReason,
    },
}

/// A subscription the host is producing for.
///
/// Not a stream type: the host calls [`Subscription::next`] when it has a value, so the pacing is the
/// host's. A `Stream` would work too and would need `futures` in the public API of a crate that
/// otherwise does not use it.
pub struct Subscription {
    id: SubscriptionId,
    method: String,
    seq: u64,
    ended: bool,
}

impl Subscription {
    /// Opens a subscription.
    #[must_use]
    pub fn new(id: SubscriptionId, method: impl Into<String>) -> Self {
        Self {
            id,
            method: method.into(),
            seq: 0,
            ended: false,
        }
    }

    /// Its id.
    #[must_use]
    pub fn id(&self) -> &SubscriptionId {
        &self.id
    }

    /// The method it describes.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Whether it has ended.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// Produces the next value.
    ///
    /// Returns `None` once the subscription has ended, so a host that keeps calling after a close
    /// produces no further frames rather than resurrecting it.
    pub fn next(&mut self, value: serde_json::Value) -> Option<SubscriptionFrame> {
        if self.ended {
            return None;
        }
        self.seq += 1;
        Some(SubscriptionFrame::Value {
            subscription_id: self.id.clone(),
            method: self.method.clone(),
            value,
            seq: self.seq,
        })
    }

    /// Ends the subscription, producing the closing frame.
    ///
    /// Returns `None` for a second call: a client that received one close and then another would not
    /// know which to believe.
    pub fn end(&mut self, reason: EndReason) -> Option<SubscriptionFrame> {
        if self.ended {
            return None;
        }
        self.ended = true;
        Some(SubscriptionFrame::End {
            subscription_id: self.id.clone(),
            reason,
        })
    }
}

/// Why a subscription could not be opened.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SubscribeError {
    /// The method is not one a client may subscribe to.
    #[error("`{method}` is not subscribable")]
    NotSubscribable {
        /// The method.
        method: String,
    },

    /// A subscription with that id is already open.
    #[error("subscription `{id}` is already open")]
    Duplicate {
        /// The id.
        id: String,
    },

    /// The client sent too many subscriptions.
    #[error("a client may hold at most {limit} subscriptions")]
    TooMany {
        /// The limit.
        limit: usize,
    },
}

/// The subscriptions one client holds.
///
/// A `Mutex` rather than a channel: the host holds this beside the socket and reads it once per
/// frame, so the contention is nil and the invariant — one entry per id — is easier to hold.
#[derive(Default)]
pub struct Subscriptions {
    open: std::sync::Mutex<std::collections::BTreeMap<String, Subscription>>,
    limit: usize,
}

impl std::fmt::Debug for Subscriptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscriptions")
            .field("open", &self.ids())
            .field("limit", &self.limit)
            .finish()
    }
}

impl Subscriptions {
    /// Creates a set bounded by `limit`.
    ///
    /// A bound because an unbounded set is a way for one client to exhaust the server: each open
    /// subscription holds a slot that the host writes to.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            open: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            limit: limit.max(1),
        }
    }

    /// Opens a subscription.
    ///
    /// # Errors
    ///
    /// Returns [`SubscribeError::Duplicate`] for a taken id and [`SubscribeError::TooMany`] past the
    /// limit.
    pub fn open(
        &self,
        id: SubscriptionId,
        method: impl Into<String>,
    ) -> Result<(), SubscribeError> {
        let mut open = self.lock();
        if open.contains_key(id.as_str()) {
            return Err(SubscribeError::Duplicate { id: id.to_string() });
        }
        if open.len() >= self.limit {
            return Err(SubscribeError::TooMany { limit: self.limit });
        }
        let method = method.into();
        open.insert(id.to_string(), Subscription::new(id, method));
        Ok(())
    }

    /// Produces a value for `id`.
    ///
    /// Returns `None` for an unknown or ended subscription, so a stray write produces no frame rather
    /// than resurrecting a closed one.
    pub fn push(&self, id: &str, value: serde_json::Value) -> Option<SubscriptionFrame> {
        self.lock().get_mut(id)?.next(value)
    }

    /// Ends one subscription.
    pub fn close(&self, id: &str, reason: EndReason) -> Option<SubscriptionFrame> {
        self.lock().get_mut(id)?.end(reason)
    }

    /// The open subscription ids, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// How many are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether none are open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, Subscription>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The methods a client may subscribe to.
///
/// A fixed list rather than "any method": a subscription holds a server-side slot, and allowing one to
/// any unary method would let a client open thousands of slots that never produce anything.
pub const SUBSCRIBABLE_METHODS: &[&str] = &[
    "session.observeEvent",
    "session.observeProjections",
    "session.observeState",
    "session.follow",
    "job.follow",
];

/// How often a subscription re-invokes its method when the client asks for no interval.
///
/// Half a second is a compromise: fast enough that a watch feels live, slow enough that eight
/// subscriptions of a `job.follow` do not turn one idle connection into a busy loop.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The fastest interval a client may ask for.
///
/// A floor because the interval is the client's to choose and the cost is the host's to pay: a
/// `intervalMs` of zero would otherwise become a tight loop against the method registry.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The slowest interval a client may ask for.
///
/// A ceiling rather than an error: a client asking for an hour still means "watch this", and clamping
/// keeps it receiving something.
pub const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How many subscriptions one connection may hold at once.
///
/// The same bound [`Subscriptions::new`] enforces; named here so the transport and a test agree.
pub const DEFAULT_SUBSCRIPTION_LIMIT: usize = 8;

/// Whether a method can be subscribed to.
#[must_use]
pub fn is_subscribable(namespace: &str, method: &str) -> bool {
    let qualified = format!("{namespace}.{method}");
    SUBSCRIBABLE_METHODS.contains(&qualified.as_str())
}

/// Whether a value is the last one its method will produce.
///
/// The convention is a `"done": true` member, checked here rather than in each method because a
/// pollable method has no other way to say "stop calling me": its return type is a value, not an
/// outcome. A method that never sets it is simply polled until the client unsubscribes.
#[must_use]
pub fn is_terminal(value: &serde_json::Value) -> bool {
    value
        .get("done")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// One `subscribe` request.
///
/// `args` is the same map [`InvokeRemoteRequest`](crate::InvokeRemoteRequest) carries: a subscription
/// is the same call repeated, so it takes the same arguments and reuses the same dispatch path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeRequest {
    /// The id the client chose for this subscription.
    pub id: String,
    /// Service namespace.
    pub namespace: String,
    /// Method name.
    pub method: String,
    /// Named arguments, forwarded unchanged on every poll.
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
    /// The peer the calls speak for. Carried, never verified.
    #[serde(default)]
    pub peer: String,
    /// How often to poll, in milliseconds. Clamped on both ends; absent means the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}

impl SubscribeRequest {
    /// The method as a client names it.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.namespace, self.method)
    }

    /// The poll interval, clamped to what the host will serve.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval_ms.map_or(DEFAULT_POLL_INTERVAL, |millis| {
            Duration::from_millis(millis).clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL)
        })
    }
}

/// The uplink frames a client sends to drive subscriptions.
///
/// Tagged by `type` like every other frame on this socket, so a transport can tell a subscription
/// control from an [`RemoteEventResult`](crate::RemoteEventResult) by one field rather than by
/// attempting both parses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubscriptionControl {
    /// Open a subscription.
    Subscribe(SubscribeRequest),

    /// Close the subscription with this id.
    Unsubscribe {
        /// Which subscription.
        id: String,
    },
}

/// One running poller, kept so a close can stop it.
struct Poller {
    task: tokio::task::JoinHandle<()>,
}

/// Turns `subscribe` requests into a stream of frames.
///
/// Transport-agnostic on purpose: it writes [`DownlinkFrame`](crate::DownlinkFrame) values into the
/// channel the socket writer already drains, so the runtime is testable without a socket and a second
/// transport reuses it unchanged.
///
/// # Why the pollers are tasks and not one loop
///
/// Two subscriptions to different methods with different intervals cannot share a single sleep: one
/// slow method would delay the other. A task per subscription also makes a close immediate — aborting
/// one task cannot disturb its neighbours.
pub struct SubscriptionHub {
    remote: Arc<girder_core::RemoteRegistry>,
    outbound: tokio::sync::mpsc::UnboundedSender<crate::DownlinkFrame>,
    subscriptions: Arc<Subscriptions>,
    pollers: std::sync::Mutex<std::collections::BTreeMap<String, Poller>>,
}

impl std::fmt::Debug for SubscriptionHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionHub")
            .field("ids", &self.ids())
            .finish_non_exhaustive()
    }
}

impl SubscriptionHub {
    /// Builds a hub over a method registry and the connection's outbound channel.
    #[must_use]
    pub fn new(
        remote: Arc<girder_core::RemoteRegistry>,
        outbound: tokio::sync::mpsc::UnboundedSender<crate::DownlinkFrame>,
        limit: usize,
    ) -> Self {
        Self {
            remote,
            outbound,
            subscriptions: Arc::new(Subscriptions::new(limit)),
            pollers: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// The ids currently open.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.subscriptions.ids()
    }

    /// How many subscriptions are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.subscriptions.len()
    }

    /// Whether none are open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
    }

    /// Applies one uplink control frame.
    ///
    /// Returns nothing on purpose: a refusal is a frame to the client, not an error the transport
    /// should act on. The socket stays open either way, because one bad request must not cost the
    /// client its whole session.
    pub fn handle(&self, control: SubscriptionControl) {
        match control {
            SubscriptionControl::Subscribe(request) => self.open(&request),
            SubscriptionControl::Unsubscribe { id } => {
                self.close(&id, EndReason::Unsubscribed);
            }
        }
    }

    /// Opens one subscription, acking either way.
    ///
    /// The checks happen in the order a client can fix them: a malformed id, then a method that may
    /// not be subscribed to, then the connection's limit. Reporting the limit first would send a
    /// client to close something when the real problem was its method name.
    pub fn open(&self, request: &SubscribeRequest) {
        let Some(id) = SubscriptionId::new(request.id.clone()) else {
            self.refuse(&request.id, "a subscription id must not be empty");
            return;
        };

        if !is_subscribable(&request.namespace, &request.method) {
            self.refuse(
                &request.id,
                &format!(
                    "`{}` is not subscribable; subscribable methods are: {}",
                    request.qualified(),
                    SUBSCRIBABLE_METHODS.join(", ")
                ),
            );
            return;
        }

        if let Err(error) = self.subscriptions.open(id.clone(), request.qualified()) {
            self.refuse(&request.id, &error.to_string());
            return;
        }

        // The ack precedes the first value because both are written through the same channel, and
        // the poller's first tick is after one interval.
        self.send(crate::DownlinkFrame::SubscribeResult {
            id: request.id.clone(),
            ok: true,
            error: None,
        });

        let poller = Poller {
            task: self.spawn_poller(id, request),
        };
        if let Some(replaced) = self.lock_pollers().insert(request.id.clone(), poller) {
            // A duplicate id is refused above, so this is unreachable; aborting anyway keeps the
            // invariant true if a future check is relaxed.
            replaced.task.abort();
        }
    }

    /// Closes one subscription and stops its poller.
    ///
    /// Closing the entry before aborting the task is what makes "no frames after unsubscribe" hold
    /// under a race: a poller that had already awaited a result finds the entry ended when it tries
    /// to push, and drops the frame instead of delivering it after the end.
    pub fn close(&self, id: &str, reason: EndReason) {
        let ended = self.subscriptions.close(id, reason);
        if let Some(poller) = self.lock_pollers().remove(id) {
            poller.task.abort();
        }
        if let Some(frame) = ended {
            self.send(crate::DownlinkFrame::Subscription { frame });
        }
    }

    /// Ends every subscription, for a disconnect.
    ///
    /// No frames are delivered — the socket is gone — but the pollers must still stop: a task per
    /// subscription that outlived its connection would keep calling methods for a client that can no
    /// longer read the answers.
    pub fn shutdown(&self, reason: &EndReason) {
        for id in self.subscriptions.ids() {
            self.subscriptions.close(&id, reason.clone());
            if let Some(poller) = self.lock_pollers().remove(&id) {
                poller.task.abort();
            }
        }
    }

    /// Spawns the loop that re-invokes `request`'s method.
    fn spawn_poller(
        &self,
        id: SubscriptionId,
        request: &SubscribeRequest,
    ) -> tokio::task::JoinHandle<()> {
        let remote = Arc::clone(&self.remote);
        let outbound = self.outbound.clone();
        let subscriptions = Arc::clone(&self.subscriptions);

        let namespace = request.namespace.clone();
        let method = request.method.clone();
        let args = serde_json::Value::Object(request.args.clone());
        let peer = request.peer.clone();
        let interval = request.interval();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The default bursts missed ticks to catch up after a stall; a subscription must not.
            // Rendering three stale values back to back would look like a burst of changes.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            // The previous value is why a poll does not spam: re-invoking a method that has nothing
            // new to report would otherwise repeat the same frame every interval, and a client would
            // re-render an unchanged view forever.
            let mut previous: Option<serde_json::Value> = None;

            loop {
                // The first tick of a tokio interval is immediate, so the loop's first pass is the
                // initial value and later passes are the polls. Waiting before the first call would
                // make a client stare at an empty watch for one interval after being told it opened.
                ticker.tick().await;

                let call = girder_core::RemoteCall {
                    args: args.clone(),
                    peer: peer.clone(),
                };
                let result = remote.dispatch(&namespace, &method, call).await;

                let value = match result {
                    Ok(value) => value,
                    Err(error) => {
                        // A method that failed cannot be polled again: retrying would repeat the
                        // same failure on a timer. The client is told why instead.
                        if let Some(frame) = subscriptions.close(
                            id.as_str(),
                            EndReason::Failed {
                                reason: error.to_string(),
                            },
                        ) {
                            let _ = outbound.send(crate::DownlinkFrame::Subscription { frame });
                        }
                        return;
                    }
                };

                let terminal = is_terminal(&value);
                if previous.as_ref() != Some(&value) {
                    previous = Some(value.clone());
                    let Some(frame) = subscriptions.push(id.as_str(), value) else {
                        // The subscription ended underneath us (an unsubscribe, or a disconnect).
                        return;
                    };
                    if outbound
                        .send(crate::DownlinkFrame::Subscription { frame })
                        .is_err()
                    {
                        // The connection is gone; stop polling rather than call methods into nothing.
                        return;
                    }
                }

                if terminal {
                    if let Some(frame) = subscriptions.close(id.as_str(), EndReason::Completed) {
                        let _ = outbound.send(crate::DownlinkFrame::Subscription { frame });
                    }
                    return;
                }
            }
        })
    }

    /// Tells the client a subscription was refused.
    fn refuse(&self, id: &str, reason: &str) {
        self.send(crate::DownlinkFrame::SubscribeResult {
            id: id.to_owned(),
            ok: false,
            error: Some(reason.to_owned()),
        });
    }

    /// Writes one frame to the connection.
    fn send(&self, frame: crate::DownlinkFrame) {
        // A closed channel means the client is gone; the connection reader observes that separately,
        // so there is nothing to report here.
        let _ = self.outbound.send(frame);
    }

    fn lock_pollers(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, Poller>> {
        self.pollers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
