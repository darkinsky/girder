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

use serde::{Deserialize, Serialize};

/// Identifies one subscription, chosen by the client.
///
/// Client-chosen so a client can name it before the first value arrives; two subscriptions to one
/// method would otherwise be indistinguishable.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

/// Whether a method can be subscribed to.
#[must_use]
pub fn is_subscribable(namespace: &str, method: &str) -> bool {
    let qualified = format!("{namespace}.{method}");
    SUBSCRIBABLE_METHODS.contains(&qualified.as_str())
}
