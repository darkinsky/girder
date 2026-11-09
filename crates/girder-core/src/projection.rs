//! Session projections: state folded from an event log.
//!
//! A session log is append-only. Almost everything a caller wants to *read* — the current goal, the
//! pending inbox, the model selection, the title — is not in any single event: it is what you get by
//! replaying all of them. A projection is that replay, made explicit and reusable.
//!
//! # Why a projection rather than a scan
//!
//! The naive alternative is to scan the log whenever something is needed. That works until the log is
//! large and the read is frequent, which is exactly a UI's access pattern. A projection folds once
//! per appended event and answers reads from the result, so the cost is per-write rather than
//! per-read.
//!
//! # Why a projection must be a pure fold
//!
//! `apply` takes the previous state and one event and returns the next state. No I/O, no clock, no
//! lookups. That constraint is what makes a projection testable by feeding it a list, and what makes
//! a cached projection and a fresh replay agree — the property everything else depends on.
//!
//! # Why `init` is separate from the first `apply`
//!
//! An empty log has a state: an empty inbox, no goal. Without `init` a caller would have to guess
//! what "no events" looks like, and two callers would guess differently.

use std::collections::BTreeMap;
use std::sync::Arc;

/// The sequence number of one event in a log.
///
/// 1-based, matching `SessionEvent` ordering. A projection that recorded positions would use these;
/// nothing in this module needs them, but a definition's `apply` receives them so a projection can
/// detect a gap.
pub type Seq = u64;

/// A fold from events to state.
///
/// Implementors are cheap value types: a projection holds no I/O handles, no locks, and no clock. A
/// fold that needed any of those could not be replayed deterministically, and a projection whose
/// replay differs from its live behaviour is worse than none.
pub trait Projection: Send + Sync {
    /// The projection's key, which is also its field name in a view.
    fn key(&self) -> &'static str;

    /// The state of an empty log.
    ///
    /// A JSON value rather than an associated type: projections are heterogeneous, and the caller
    /// that reads them is a UI that renders JSON. A typed state would force every consumer to know
    /// every definition.
    fn init(&self) -> serde_json::Value;

    /// Folds one event into the state.
    ///
    /// Returns the state unchanged for an event this projection does not care about. That is the
    /// common case: a log holds every kind of event, and each projection reads a few.
    fn apply(
        &self,
        state: serde_json::Value,
        event: &crate::session::SessionEvent,
        seq: Seq,
    ) -> serde_json::Value;

    /// Whether this projection appears in a session summary.
    ///
    /// False for a projection whose value is large and rarely listed — a thread of messages, a full
    /// transcript. A summary that included them would be as expensive as the log itself.
    fn in_summary(&self) -> bool {
        true
    }
}

/// Why a projection operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProjectionError {
    /// A key is registered twice.
    #[error("a projection named `{key}` is already registered")]
    Duplicate {
        /// The key.
        key: String,
    },

    /// A fold produced a state that is not JSON, which means the definition is broken.
    #[error("projection `{key}` produced a non-JSON state")]
    NonJson {
        /// The key.
        key: String,
    },
}

/// The registered projections and the state folded so far.
#[derive(Default)]
pub struct ProjectionRegistry {
    definitions: Vec<Arc<dyn Projection>>,
    /// Folded state per key, kept in step with the log so a read does not replay.
    state: std::sync::Mutex<BTreeMap<String, serde_json::Value>>,
    /// The last sequence number folded, per key.
    ///
    /// Per key rather than global: registering a projection mid-session means it starts from the
    /// events after registration, and a global cursor would make it silently skip or double-apply.
    folded_to: std::sync::Mutex<BTreeMap<String, Seq>>,
}

impl std::fmt::Debug for ProjectionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionRegistry")
            .field("definitions", &self.definitions.len())
            .finish_non_exhaustive()
    }
}

impl ProjectionRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a projection, initialising its state.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::Duplicate`] when the key is taken. Replacing silently would leave
    /// a UI reading one definition's state under another's name.
    pub fn register(&mut self, projection: Arc<dyn Projection>) -> Result<(), ProjectionError> {
        let key = projection.key().to_owned();
        if self
            .definitions
            .iter()
            .any(|existing| existing.key() == key)
        {
            return Err(ProjectionError::Duplicate { key });
        }
        let initial = projection.init();
        self.definitions.push(projection);
        self.lock_state().insert(key.clone(), initial);
        self.lock_cursor().insert(key, 0);
        Ok(())
    }

    /// The registered keys, sorted.
    ///
    /// Sorted because a view is rendered from them, and an order that changes between runs makes the
    /// UI jump.
    #[must_use]
    pub fn keys(&self) -> Vec<&'static str> {
        let mut keys: Vec<&'static str> = self.definitions.iter().map(|d| d.key()).collect();
        keys.sort_unstable();
        keys
    }

    /// Folds one event into every projection that has not seen it.
    ///
    /// An event at or below a projection's cursor is skipped, which makes this idempotent: replaying
    /// a log twice, or receiving a duplicate, does not double-apply. That matters because the caller
    /// that feeds events is a subscription, and a subscription can deliver the same event twice.
    pub fn observe(&self, event: &crate::session::SessionEvent, seq: Seq) {
        let mut state = self.lock_state();
        let mut cursor = self.lock_cursor();
        for definition in &self.definitions {
            let key = definition.key().to_owned();
            let last = cursor.get(&key).copied().unwrap_or(0);
            if seq <= last {
                continue;
            }
            let current = state
                .get(&key)
                .cloned()
                .unwrap_or_else(|| definition.init());
            let next = definition.apply(current, event, seq);
            state.insert(key.clone(), next);
            cursor.insert(key, seq);
        }
    }

    /// Reads one projection's state.
    #[must_use]
    pub fn read(&self, key: &str) -> Option<serde_json::Value> {
        self.lock_state().get(key).cloned()
    }

    /// The summary view: every projection that declares itself part of one.
    #[must_use]
    pub fn summary(&self) -> BTreeMap<String, serde_json::Value> {
        let state = self.lock_state();
        self.definitions
            .iter()
            .filter(|definition| definition.in_summary())
            .filter_map(|definition| {
                state
                    .get(definition.key())
                    .map(|value| (definition.key().to_owned(), value.clone()))
            })
            .collect()
    }

    /// Every projection's state, for a full view.
    #[must_use]
    pub fn view(&self) -> BTreeMap<String, serde_json::Value> {
        self.lock_state().clone()
    }

    /// Replays a log from scratch, replacing every projection's state.
    ///
    /// The operation a resume performs. It resets the cursors first so the replay is total rather
    /// than a continuation: a resumed session must see a state built from the whole log, not from
    /// whatever the previous process folded.
    pub fn replay(&self, events: &[(Seq, crate::session::SessionEvent)]) {
        {
            let mut state = self.lock_state();
            let mut cursor = self.lock_cursor();
            for definition in &self.definitions {
                let key = definition.key().to_owned();
                state.insert(key.clone(), definition.init());
                cursor.insert(key, 0);
            }
        }
        for (seq, event) in events {
            self.observe(event, *seq);
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, serde_json::Value>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_cursor(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Seq>> {
        self.folded_to
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Shared registry handle.
pub type ProjectionHandle = Arc<ProjectionRegistry>;

/// A projection over the turn boundaries in a log.
///
/// The reference definition: small enough to read in one screen, and it exercises the shape every
/// other projection uses — ignore what you do not need, fold what you do.
pub struct TurnBoundary;

impl Projection for TurnBoundary {
    fn key(&self) -> &'static str {
        "turnBoundary"
    }

    fn init(&self) -> serde_json::Value {
        serde_json::json!({
            "openTurnStartSeq": serde_json::Value::Null,
            "lastStepStartSeq": serde_json::Value::Null,
            "lastTurn": 0,
        })
    }

    fn apply(
        &self,
        mut state: serde_json::Value,
        event: &crate::session::SessionEvent,
        seq: Seq,
    ) -> serde_json::Value {
        use crate::session::SessionEvent;
        if let Some(object) = state.as_object_mut() {
            match event {
                SessionEvent::TurnStart { turn } => {
                    object.insert("openTurnStartSeq".to_owned(), serde_json::json!(seq));
                    object.insert("lastTurn".to_owned(), serde_json::json!(turn));
                }
                SessionEvent::TurnEnd { .. } => {
                    // Null between turns: an open turn that has ended is not open.
                    object.insert("openTurnStartSeq".to_owned(), serde_json::Value::Null);
                }
                SessionEvent::StepStart { .. } => {
                    object.insert("lastStepStartSeq".to_owned(), serde_json::json!(seq));
                }
                _ => {}
            }
        }
        state
    }
}

/// A projection over the model selection a session has used.
pub struct ModelSelectionProjection;

impl Projection for ModelSelectionProjection {
    fn key(&self) -> &'static str {
        "modelSelection"
    }

    fn init(&self) -> serde_json::Value {
        serde_json::json!({
            "lastUsed": serde_json::Value::Null,
        })
    }

    fn apply(
        &self,
        mut state: serde_json::Value,
        event: &crate::session::SessionEvent,
        _seq: Seq,
    ) -> serde_json::Value {
        if let (crate::session::SessionEvent::ModelSelection { selection }, Some(object)) =
            (event, state.as_object_mut())
        {
            object.insert("lastUsed".to_owned(), serde_json::json!(selection));
        }
        state
    }
}

/// A projection over the session's title.
pub struct TitleProjection;

impl Projection for TitleProjection {
    fn key(&self) -> &'static str {
        "title"
    }

    fn init(&self) -> serde_json::Value {
        serde_json::json!({ "title": serde_json::Value::Null, "source": serde_json::Value::Null })
    }

    fn apply(
        &self,
        mut state: serde_json::Value,
        event: &crate::session::SessionEvent,
        _seq: Seq,
    ) -> serde_json::Value {
        if let (crate::session::SessionEvent::SessionTitle { title, source, .. }, Some(object)) =
            (event, state.as_object_mut())
        {
            object.insert("title".to_owned(), serde_json::json!(title));
            object.insert("source".to_owned(), serde_json::json!(source));
        }
        state
    }
}

// ---- the projection-aware session -------------------------------------------------------------

/// A session wrapper that keeps projections in step with the log.
///
/// A wrapper rather than a field on every session: there are already several `Session` implementations
/// (an in-memory one, a file store), and adding projection awareness to each would make every one of
/// them responsible for a concern none of them owns. A caller that wants projections wraps its session
/// once.
///
/// # Why the index is tracked here
///
/// `ProjectionRegistry::observe` is idempotent per sequence number, but nothing assigns them: the log
/// does not carry a sequence, because the position *is* the sequence. This wrapper counts appends, so
/// the first event is 1 and a duplicate delivery is recognisable.
pub struct ProjectedSession {
    inner: Arc<dyn crate::session::Session>,
    projections: Arc<ProjectionRegistry>,
    /// The sequence number of the last appended event.
    ///
    /// A counter rather than the log length, because reading the length per append would be a second
    /// I/O per write.
    cursor: std::sync::atomic::AtomicU64,
}

impl ProjectedSession {
    /// Wraps a session, folding its existing log first.
    ///
    /// Replaying at construction is what makes a resumed session correct: the projections reflect the
    /// whole log rather than only what happens after this process started.
    ///
    /// # Errors
    ///
    /// Returns the session's replay error. A wrapper whose projections disagree with the log is worse
    /// than no wrapper, so a failed replay refuses to construct.
    pub async fn attach(
        inner: Arc<dyn crate::session::Session>,
        projections: Arc<ProjectionRegistry>,
    ) -> Result<Self, crate::session::SessionError> {
        let events = inner.replay().await?;
        let numbered: Vec<(Seq, crate::session::SessionEvent)> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| (u64::try_from(index).unwrap_or(0) + 1, event))
            .collect();
        let cursor = numbered.last().map_or(0, |(seq, _)| *seq);
        projections.replay(&numbered);
        Ok(Self {
            inner,
            projections,
            cursor: std::sync::atomic::AtomicU64::new(cursor),
        })
    }

    /// The projections this wrapper maintains.
    #[must_use]
    pub fn projections(&self) -> &Arc<ProjectionRegistry> {
        &self.projections
    }

    /// The session being wrapped.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn crate::session::Session> {
        &self.inner
    }

    /// How many events have been folded.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Appends an event and folds it.
    ///
    /// A separate method rather than an `impl Session`, deliberately: folding must happen **after**
    /// the write succeeds. Providing `Session` would let a caller call `append` through the trait and
    /// bypass the fold, and the two would silently diverge.
    ///
    /// # Errors
    ///
    /// Returns the session's error. The projection is **not** advanced on failure, so a caller that
    /// retries does not fold an event twice.
    pub async fn append(
        &self,
        event: crate::session::SessionEvent,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append(event.clone()).await?;
        let seq = self
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.projections.observe(&event, seq);
        Ok(())
    }

    /// Appends several events in order.
    ///
    /// # Errors
    ///
    /// Returns the first failure; events before it are already written and folded, which is the same
    /// partial-application a caller would see appending one at a time.
    pub async fn append_all(
        &self,
        events: impl IntoIterator<Item = crate::session::SessionEvent>,
    ) -> Result<(), crate::session::SessionError> {
        for event in events {
            self.append(event).await?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ProjectedSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The folded state is deliberately not printed: a projection can hold a transcript, and a
        // debug line should name it rather than dump it. The keys and the cursor are what a caller
        // actually needs from this type.
        f.debug_struct("ProjectedSession")
            .field("inner", &"<session>")
            .field("projections", &self.projections.keys())
            .field("cursor", &self.cursor())
            .finish()
    }
}
