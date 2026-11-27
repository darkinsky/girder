//! The `session.state` Remote method: one read that returns everything a client renders.
//!
//! Opening a session costs several round trips today — `session.page` for the cursor, the catalog for
//! the label, `session.projections` for the folded views — and a client that issues them one after
//! another renders a screen stitched together from several moments. Those moments can disagree: a turn
//! that ends between the page read and the projection read leaves the counts and the folded state
//! describing different logs, and the mismatch shows up as a UI that briefly shows a turn as open
//! after it closed.
//!
//! This method answers once, from one read of the log, so every field in the answer describes the same
//! snapshot and a client can render without a second request.
//!
//! # What upstream's `state` returns, and where this answer differs
//!
//! Upstream's `session.state` (in `session-controller`) is a *passive control* read, not a render
//! snapshot. Its `SessionState` is `{ cursor, active, running, queue, pendingInput }`: where the log
//! ends, whether an agent is attached, whether it is executing, what the inbox holds, and which
//! replies a pending Remote request waits for. It carries no title, no projection view, and no event
//! count, because upstream's client gets those from a separate `follow` opening snapshot.
//!
//! This build has no registry mapping a session id to a live [`crate::Agent`], and no gateway of
//! pending Remote events, so three of upstream's five fields cannot be answered honestly here. The
//! field set below is the union this build *can* answer: the cursor and `running` from upstream, plus
//! the event count, title, and projection view that upstream's clients read from elsewhere.
//!
//! What is deliberately absent, rather than sent as a placeholder:
//!
//! - **`active`** — "an agent is attached". Nothing here maps a session id to a live agent
//!   ([`crate::AgentRuntime`] tracks cancellation tokens, not handles), so every answer would be
//!   `false`, including for a session whose turn is running in this very process. A client that
//!   branched on it would take the cold path while output streamed.
//! - **`queue` / `pendingInput`** — pending input lives in a live agent's in-memory inbox
//!   ([`crate::api::inbox`]), which a cold read cannot reconstruct; [`crate::session::queue_rpc`] documents
//!   the same gap for `updateQueue`. This build also has no gateway tracking pending Remote requests.
//!
//! One name differs from upstream. Upstream calls the cursor `cursor`; this build calls it `lastSeq`,
//! which is what [`crate::session::resume_rpc`] and `session.follow` already report. A client comparing
//! this snapshot against its follow cursor therefore does so without translating between two names for
//! one number — and the whole point of this method is that those numbers agree.
//!
//! # Why an unknown session is an error
//!
//! A snapshot answering zero events, no title, and an empty projection view is exactly what a *new*
//! session looks like. A caller that mistyped an id — or whose session was deleted on another host —
//! would render a fresh blank conversation instead of a failure, and the next prompt would land in it.
//! Upstream refuses this with `session/not-found`; [`RemoteError`] here has no not-found variant, so
//! the answer is `Failed` with the id in the message, as [`crate::session::resume_rpc`] does.
//!
//! # Why the declaration is a read
//!
//! The method writes nothing: it reads the log, the catalog's stored title, and the projection map. It
//! must stay callable under a read-only policy — a mirror that can show a session but not prompt into
//! it is a supported deployment, and declaring a write would refuse it the one call it exists for.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use girder_core::{
    Declaration, ProjectionRegistry, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog,
    SessionEvent,
};

/// The namespace `session.state` lives under.
pub const NAMESPACE: &str = "session";

/// The method name within [`NAMESPACE`], spelled as upstream spells it.
pub const METHOD: &str = "state";

/// How many sessions one control baseline names.
///
/// A poll walks the catalog. Past this the frame says the host is larger than the page, rather than
/// reading every log on every tick.
const CONTROL_SESSION_LIMIT: usize = 64;

/// The resource kind this method declares to an access policy.
///
/// Reading one session is a read of that session and nothing else, so a policy can grant this method
/// to a peer without granting anything the peer did not already need to render a screen.
const RESOURCE_KIND: &str = "session";

/// The request a client sends.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateRequest {
    /// The session to read.
    session_id: String,
}

/// The complete state of one session at the moment it was read.
///
/// Every field comes from one read of the log, which is what makes the snapshot consistent: a count
/// that disagrees with the view it is sent beside is worse than a stale count, because a client
/// renders the two together and cannot tell which one is wrong.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateValue {
    /// Identity, echoed so a reader multiplexing several reads can pair an answer with its question
    /// without relying on request order.
    pub session_id: String,
    /// How many durable events the log holds.
    pub events: u64,
    /// The sequence number of the last event.
    ///
    /// One-based, matching `session.follow` and `session.resume`; an empty log has none, so this is
    /// `0` rather than `1`. Upstream names this same number `cursor`.
    pub last_seq: u64,
    /// The label the catalog holds for the session, when a person gave it one.
    ///
    /// `None` means nobody renamed it. This is the *stored label*, not a title derived from the
    /// conversation: the log's own `session/title` event is visible through the `title` projection,
    /// and a client that wants a derived label for an untitled session can compute one with
    /// [`crate::derive_title`] from contents it already has. Filling this field with a guess would make
    /// a session nobody renamed indistinguishable from one that was.
    pub title: Option<String>,
    /// The request header in force, when the log has one.
    ///
    /// `crate::llm::request_header::request_header_of` reads it: the last `request/header` event, in
    /// canonical form. A client showing "what model produced this" reads it here rather than
    /// re-folding the log, and a session that never had a request built for it answers `null` --
    /// which is different from a header this build cannot parse, counted by
    /// `crate::llm::request_header::unparsed`.
    #[serde(default)]
    pub request_header: Option<serde_json::Value>,
    /// Every registered projection's folded state, as the host currently holds it.
    ///
    /// Read, never folded here: the projection state belongs to the host, and replaying *this*
    /// session's log into the shared registry to refresh it would overwrite whatever session the host
    /// folded last, because [`ProjectionRegistry`] is process-wide rather than per session. An empty
    /// map is therefore a normal answer on a server that has folded nothing yet.
    pub projections: serde_json::Map<String, serde_json::Value>,
    /// Whether a turn is executing for this session right now.
    ///
    /// Asked of a [`LiveTurns`] source rather than derived from the log, because "executing" is a fact
    /// about the process, not about the file. The three-argument constructor installs a source that
    /// always answers `false`; see [`register_session_state_method`].
    pub running: bool,
    /// Whether the log ends inside a turn.
    ///
    /// The writing process stopped between `turn/start` and `turn/end` — a crash, a kill, a power loss.
    /// Reported rather than repaired, for the reason [`crate::session::resume_rpc`] gives: this build has
    /// no event that can carry "outcome unknown", and appending a fabricated closer would hand the model
    /// an observation that never happened.
    pub interrupted: bool,
    /// The last assistant text in the log, when there is any.
    ///
    /// `None` covers two states a client renders differently: no assistant message at all, and a final message that
    /// carried only tool calls. Distinguishing them needs the log, which is why this comes from
    /// [`crate::final_text`] rather than from a live buffer that a cold read would not have.
    pub last_text: Option<String>,
    /// Which turn the last assistant message belonged to.
    ///
    /// A number rather than a re-derivation: a client showing "the answer to turn 3" needs the turn, and counting
    /// boundaries itself is how two counts start disagreeing.
    pub last_turn: Option<u64>,
    /// What the conversation did, in counts.
    ///
    /// A client showing "12 turns, 40 steps, 3 failed tool calls" reads this instead of walking the log it already
    /// asked for, which is the same numbers computed twice.
    pub counts: girder_core::SessionStats,
    /// Characters written by each kind of producer.
    pub attribution: girder_core::ContentAttribution,
    /// What each turn did, one line each.
    pub timeline: Vec<girder_core::TimelineSegment>,
    /// Per-tool counts, heaviest first.
    ///
    /// Heaviest by calls rather than by bytes: a tool called once that returned a megabyte is an interesting outlier, but
    /// a list sorted by a rare tool's result size would put it above the tool the model reaches for every turn.
    pub tool_usage: Vec<girder_core::ToolStats>,
    /// What the conversation came to, in one value.
    ///
    /// The counts say how much work happened, the timeline says what each turn did, and neither says what the last answer
    /// was -- which is the question someone returning to a long conversation asks first.
    pub digest: girder_core::Digest,
    /// Where this session forked from, when its id records it.
    ///
    /// Reconstructed from the id rather than read from the log, because a fork writes no edge: it copies a prefix and
    /// nothing else. `None` means this session was not forked, or was created by a build that named it differently.
    pub forked_from: Option<ForkOrigin>,
    /// The sessions that forked from this one.
    ///
    /// Children by id, which is what a client drawing a tree needs and what no field in this session's own log holds.
    pub forks: Vec<String>,
    /// Sessions this one references, was continued from, or discusses the same work as.
    ///
    /// Separate from `forks` because a fork is lineage and these are not: `session_branch` enforces one parent and no
    /// cycles, which is right for ancestry and wrong for a graph of "this one came up before".
    pub related: Vec<RelatedSession>,
    /// The turn still running, when the log ends inside one.
    ///
    /// Separate from the segments rather than inferred from the last one: a turn can be unfinished and still have a
    /// complete summary, and a client that treated "has a summary" as "is done" would hide the live row.
    pub open_turn: Option<u64>,
}

/// Where "a turn is executing" is found.
///
/// A trait rather than a bare `fn` so a host can pass its runtime, a map, or a test double without this
/// module depending on any of them. The source installed by [`register_session_state_method`] answers
/// `false` for everything — the honest answer for a build with no live agent table, and the wrong one
/// for a build that has one, which is why [`register_session_state_method_with`] exists.
pub trait LiveTurns: Send + Sync {
    /// Whether `session_id` has a turn in flight.
    fn is_running(&self, session_id: &str) -> bool;

    /// Wakes after [`Self::is_running`] may have changed.
    ///
    /// `None` means the source cannot say when it changes, so the observer keeps a short poll.
    fn changes(&self) -> Option<Arc<tokio::sync::Notify>> {
        None
    }
}

/// A closure is a source too, so a host whose lookup is one line need not write a type.
impl<F> LiveTurns for F
where
    F: Fn(&str) -> bool + Send + Sync + 'static,
{
    fn is_running(&self, session_id: &str) -> bool {
        self(session_id)
    }
}

/// The build's own answer: nothing is running, because this module holds no live agents.
///
/// Named rather than inlined so the documentation on [`register_session_state_method`] can point at
/// the thing a host would replace, and so a caller reading `running: false` has somewhere to look.
#[derive(Debug, Default)]
pub struct NoLiveTurns;

impl LiveTurns for NoLiveTurns {
    fn is_running(&self, _session_id: &str) -> bool {
        false
    }
}

/// Pending Remote input for one session, named the way `session.wait` classifies it.
///
/// The kinds are `approval`, `question`, and `other`. A session with none returns an empty list.
/// The default registration has no source, so every session answers empty.
pub trait PendingInputs: Send + Sync {
    /// Kinds currently waiting on `session_id`, without duplicates.
    fn kinds(&self, session_id: &str) -> Vec<String>;

    /// Wakes after [`Self::kinds`] may have changed.
    fn changes(&self) -> Option<Arc<tokio::sync::Notify>> {
        None
    }
}

impl<F> PendingInputs for F
where
    F: Fn(&str) -> Vec<String> + Send + Sync + 'static,
{
    fn kinds(&self, session_id: &str) -> Vec<String> {
        self(session_id)
    }
}

/// The source used when a host has not connected questions or approvals.
#[derive(Debug, Default)]
pub struct NoPendingInputs;

impl PendingInputs for NoPendingInputs {
    fn kinds(&self, _session_id: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Registers `session.state` with no live-turn source.
///
/// The signature a host with no session-to-agent table can satisfy. One field is therefore limited,
/// and the limit is stated here rather than discovered in a UI:
///
/// **`running` is always `false`.** A host that keeps live agents passes
/// [`register_session_state_method_with`] a source backed by [`crate::AgentRuntime::is_running`]. A
/// client polling the three-argument version during a turn would otherwise see a session that looks
/// idle while output streams.
pub fn register_session_state_method(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    projections: &Arc<ProjectionRegistry>,
) {
    // Bound to a trait object first: `&Arc::new(NoLiveTurns)` does not coerce behind a shared
    // reference, and the named binding is also what the doc above points a host at.
    let turns: Arc<dyn LiveTurns> = Arc::new(NoLiveTurns);
    let pending: Arc<dyn PendingInputs> = Arc::new(NoPendingInputs);
    let queues = no_queues();
    register_session_state_method_observing(
        registry,
        catalog,
        projections,
        &turns,
        &pending,
        &queues,
    );
}

/// Registers `session.state`, asking `turns` whether a turn is in flight.
///
/// A separate seam rather than a [`crate::AgentRuntime`] parameter because the two are not the same
/// capability: a test double, a host that tracks turns in its own table, and a deployment with no
/// runtime at all all want to answer this question without constructing a runtime to do it.
pub fn register_session_state_method_with(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    projections: &Arc<ProjectionRegistry>,
    turns: &Arc<dyn LiveTurns>,
) {
    let pending: Arc<dyn PendingInputs> = Arc::new(NoPendingInputs);
    let queues = no_queues();
    register_session_state_method_observing(
        registry,
        catalog,
        projections,
        turns,
        &pending,
        &queues,
    );
}

/// Registers the session state methods and reads pending input from `pending`.
///
/// [`register_session_state_method_with`] is this function with an empty pending source. A host
/// that holds open questions passes the queue here, so `session.wait` can stop with
/// `input-required` while a question is still open.
pub fn register_session_state_method_observing(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    projections: &Arc<ProjectionRegistry>,
    turns: &Arc<dyn LiveTurns>,
    pending: &Arc<dyn PendingInputs>,
    queues: &Arc<dyn girder_core::QueueAgentSource>,
) {
    // One owned clone per registration, because each closure takes ownership of what it captures.
    // Named by endpoint rather than by index: five handlers read these and a `_2` suffix would not
    // say which one it belonged to.
    let state_catalog = Arc::clone(catalog);
    let state_projections = Arc::clone(projections);
    let state_turns: Arc<dyn LiveTurns> = Arc::clone(turns);
    let wait_catalog = Arc::clone(catalog);
    let wait_turns: Arc<dyn LiveTurns> = Arc::clone(turns);
    let wait_pending = Arc::clone(pending);
    let wait_queues = Arc::clone(queues);
    let event_catalog = Arc::clone(catalog);
    let projection_registry = Arc::clone(projections);
    let projection_catalog = Arc::clone(catalog);
    let observe_state_catalog = Arc::clone(catalog);
    let observe_state_projections = Arc::clone(projections);
    let observe_state_turns: Arc<dyn LiveTurns> = Arc::clone(turns);

    // Declared beside the handler and before it, so the policy judges the call the handler would take.
    // A read: nothing below writes, and a read-only mirror must still be able to render a session.
    registry.declare(NAMESPACE, METHOD, Declaration::read(RESOURCE_KIND));

    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let catalog = Arc::clone(&state_catalog);
        let projections = Arc::clone(&state_projections);
        let turns = Arc::clone(&state_turns);
        Box::pin(async move { state(&call.args, &catalog, &projections, &turns, None).await })
    });

    // `session.wait` observes until the session is idle, needs input, or the deadline lands.
    // The stopping rule is `crate::session::wait::wait_action`. The answer is the passive state
    // plus the assistant messages inside the requested window.
    registry.declare(NAMESPACE, "wait", Declaration::read(RESOURCE_KIND));
    registry.register(NAMESPACE, "wait", move |call: RemoteCall| {
        let catalog = Arc::clone(&wait_catalog);
        let turns = Arc::clone(&wait_turns);
        let pending = Arc::clone(&wait_pending);
        let queues = Arc::clone(&wait_queues);
        Box::pin(async move { session_wait(&call.args, &catalog, &turns, &pending, &queues).await })
    });

    // The three `observe*` endpoints keep unary compatibility handlers and also register live
    // stream openers below, so both transport generations can reach the same state source.
    registry.register(NAMESPACE, "observeEvent", move |call: RemoteCall| {
        let catalog = Arc::clone(&event_catalog);
        Box::pin(
            async move { girder_core::session::observe::observe_event(&call.args, &catalog).await },
        )
    });
    let event_stream_catalog = Arc::clone(catalog);
    registry.register_stream(NAMESPACE, "observeEvent", move |open| {
        let catalog = Arc::clone(&event_stream_catalog);
        Box::pin(async move { observe_event_stream(open, catalog).await })
    });

    registry.register(NAMESPACE, "observeProjections", move |call: RemoteCall| {
        let projections = Arc::clone(&projection_registry);
        let catalog = Arc::clone(&projection_catalog);
        Box::pin(async move {
            girder_core::session::observe::observe_projections(&call.args, &catalog, &projections)
                .await
        })
    });
    let projection_stream_catalog = Arc::clone(catalog);
    let projection_stream_registry = Arc::clone(projections);
    registry.register_stream(NAMESPACE, "observeProjections", move |open| {
        let catalog = Arc::clone(&projection_stream_catalog);
        let projections = Arc::clone(&projection_stream_registry);
        Box::pin(async move { observe_projections_stream(open, catalog, projections).await })
    });

    registry.register(NAMESPACE, "observeState", move |call: RemoteCall| {
        let catalog = Arc::clone(&observe_state_catalog);
        let projections = Arc::clone(&observe_state_projections);
        let turns = Arc::clone(&observe_state_turns);
        Box::pin(async move {
            let state = state(&call.args, &catalog, &projections, &turns, None).await?;
            Ok(girder_core::session::observe::observe_state(&state))
        })
    });
    let state_stream_catalog = Arc::clone(catalog);
    let state_stream_turns: Arc<dyn LiveTurns> = Arc::clone(turns);
    let state_stream_pending = Arc::clone(pending);
    let state_stream_queues = Arc::clone(queues);
    registry.register_stream(NAMESPACE, "observeState", move |open| {
        let catalog = Arc::clone(&state_stream_catalog);
        let turns = Arc::clone(&state_stream_turns);
        let pending = Arc::clone(&state_stream_pending);
        let queues = Arc::clone(&state_stream_queues);
        Box::pin(async move { observe_state_stream(open, catalog, turns, pending, queues).await })
    });

    // `session.control`: one baseline, then one projection frame per session whose keys changed.
    // `{ type: "baseline", value: { projections: { [sessionId]: { asOfSeq, values } } } }`
    // `{ type: "projection", sessionId, seq, changes: [{ key, val }] }`.
    // A burst between polls collapses to the net diff at the later seq.
    let control_catalog = Arc::clone(catalog);
    let control_projections = Arc::clone(projections);
    registry.register(NAMESPACE, "control", move |_call: RemoteCall| {
        let catalog = Arc::clone(&control_catalog);
        let projections = Arc::clone(&control_projections);
        Box::pin(async move {
            let listed = catalog.list().await;
            let mut blocks = serde_json::Map::new();
            for record in listed.into_iter().take(CONTROL_SESSION_LIMIT) {
                let Some(full) = catalog.read(&record.id).await else {
                    continue;
                };
                let projected = projections
                    .project(&full.events)
                    .with_header_directory(catalog.working_directory(&full.id).as_deref());
                blocks.insert(
                    full.id,
                    serde_json::json!({
                        "asOfSeq": projected.as_of,
                        "values": projected.values,
                    }),
                );
            }
            Ok(serde_json::json!({
                "type": "baseline",
                "value": { "projections": blocks },
                "done": false,
            }))
        })
    });
    let control_stream_catalog = Arc::clone(catalog);
    let control_stream_projections = Arc::clone(projections);
    registry.register_stream(NAMESPACE, "control", move |open| {
        let catalog = Arc::clone(&control_stream_catalog);
        let projections = Arc::clone(&control_stream_projections);
        Box::pin(async move {
            // One cursor per session: the number of events already folded into this generation.
            // The opening baseline covers that prefix. Each later event is its own frame.
            let mut cursors: BTreeMap<String, usize> = BTreeMap::new();
            let mut opened = false;
            loop {
                let wake = catalog.changes();
                let notified = wake.as_ref().map(|wake| wake.notified());
                tokio::pin!(notified);
                if let Some(waiting) = notified.as_mut().as_pin_mut() {
                    waiting.enable();
                }
                let listed = catalog.list().await;
                if !opened {
                    let mut blocks = serde_json::Map::new();
                    for record in listed.into_iter().take(CONTROL_SESSION_LIMIT) {
                        let Some(full) = catalog.read(&record.id).await else {
                            continue;
                        };
                        let projected = projections
                            .project(&full.events)
                            .with_header_directory(catalog.working_directory(&full.id).as_deref());
                        blocks.insert(
                            full.id.clone(),
                            serde_json::json!({
                                "asOfSeq": projected.as_of,
                                "values": projected.values,
                            }),
                        );
                        cursors.insert(full.id, full.events.len());
                    }
                    if !open.push(serde_json::json!({
                        "type": "baseline",
                        "value": { "projections": Value::Object(blocks) },
                    })) {
                        return;
                    }
                    opened = true;
                } else if !push_control_diffs(&open, &catalog, &projections, &listed, &mut cursors)
                    .await
                {
                    return;
                }
                if !wait_armed(&open, notified.as_mut().as_pin_mut()).await {
                    return;
                }
            }
        })
    });
}

/// Push one projection frame per new event that changed a visible key.
///
/// Returns false when the stream closed while a frame was being written.
async fn push_control_diffs(
    open: &girder_core::StreamOpen,
    catalog: &Arc<dyn SessionCatalog>,
    projections: &Arc<ProjectionRegistry>,
    listed: &[girder_core::SessionRecord],
    cursors: &mut BTreeMap<String, usize>,
) -> bool {
    for record in listed.iter().take(CONTROL_SESSION_LIMIT) {
        let Some(full) = catalog.read(&record.id).await else {
            continue;
        };
        let start = cursors.get(&full.id).copied().unwrap_or(0);
        if start < full.events.len() {
            for index in start..full.events.len() {
                let seq = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
                let directory = catalog.working_directory(&full.id);
                let before = projections
                    .project(&full.events[..index])
                    .with_header_directory(directory.as_deref());
                let after = projections
                    .project(&full.events[..=index])
                    .with_header_directory(directory.as_deref());
                let changes = changed_projection_keys(&before.values, &after.values);
                if changes.is_empty() {
                    continue;
                }
                if !open.push(serde_json::json!({
                    "type": "projection",
                    "sessionId": full.id,
                    "seq": seq,
                    "changes": changes,
                })) {
                    return false;
                }
            }
        }
        cursors.insert(full.id, full.events.len());
    }
    true
}

async fn observe_event_stream(open: girder_core::StreamOpen, catalog: Arc<dyn SessionCatalog>) {
    let method = "observeEvent";
    let session_id = match girder_core::api::api_final::session_id_of(&open.call.args, method) {
        Ok(id) => id,
        Err(error) => {
            let _ = open.fail(error);
            return;
        }
    };
    // Readiness does not prove the session exists. A page can subscribe before
    // create finishes; events that land afterwards are the ones this stream reports.
    let mut cursor = match catalog.read(&session_id).await {
        Some(record) => u64::try_from(record.events.len()).unwrap_or(u64::MAX),
        None => 0,
    };
    if !open.push(serde_json::json!({ "type": "ready" })) {
        return;
    }
    loop {
        let wake = catalog.changes();
        let notified = wake.as_ref().map(|wake| wake.notified());
        tokio::pin!(notified);
        if let Some(waiting) = notified.as_mut().as_pin_mut() {
            waiting.enable();
        }
        let Some(record) = catalog.read(&session_id).await else {
            if !wait_armed(&open, notified.as_mut().as_pin_mut()).await {
                return;
            }
            continue;
        };
        let start = usize::try_from(cursor).unwrap_or(usize::MAX);
        if start < record.events.len() {
            for (offset, event) in record.events[start..].iter().enumerate() {
                let seq = cursor
                    .saturating_add(u64::try_from(offset).unwrap_or(u64::MAX))
                    .saturating_add(1);
                let Some(frame) = girder_core::api::remote::session_wire_record(event, seq) else {
                    continue;
                };
                if !open.push(frame) {
                    return;
                }
            }
            cursor = u64::try_from(record.events.len()).unwrap_or(u64::MAX);
            continue;
        }
        if !wait_armed(&open, notified.as_mut().as_pin_mut()).await {
            return;
        }
    }
}

async fn observe_projections_stream(
    open: girder_core::StreamOpen,
    catalog: Arc<dyn SessionCatalog>,
    projections: Arc<ProjectionRegistry>,
) {
    let method = "observeProjections";
    let session_id = match girder_core::api::api_final::session_id_of(&open.call.args, method) {
        Ok(id) => id,
        Err(error) => {
            let _ = open.fail(error);
            return;
        }
    };
    // The opening `ready` is only "the subscription is live". A missing session
    // stays open until it appears, the same way `observeEvent` does.
    let mut cursor = match catalog.read(&session_id).await {
        Some(record) => u64::try_from(record.events.len()).unwrap_or(u64::MAX),
        None => 0,
    };
    if !open.push(serde_json::json!({ "type": "ready" })) {
        return;
    }
    loop {
        let wake = catalog.changes();
        let notified = wake.as_ref().map(|wake| wake.notified());
        tokio::pin!(notified);
        if let Some(waiting) = notified.as_mut().as_pin_mut() {
            waiting.enable();
        }
        let Some(record) = catalog.read(&session_id).await else {
            if !wait_armed(&open, notified.as_mut().as_pin_mut()).await {
                return;
            }
            continue;
        };
        let start = usize::try_from(cursor).unwrap_or(usize::MAX);
        if start < record.events.len() {
            for index in start..record.events.len() {
                let seq = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
                let directory = catalog.working_directory(&session_id);
                let before = projections
                    .project(&record.events[..index])
                    .with_header_directory(directory.as_deref());
                let after = projections
                    .project(&record.events[..=index])
                    .with_header_directory(directory.as_deref());
                let changes = changed_projection_keys(&before.values, &after.values);
                if changes.is_empty() {
                    continue;
                }
                if !open.push(serde_json::json!({
                    "type": "projection",
                    "seq": seq,
                    "changes": changes,
                })) {
                    return;
                }
            }
            cursor = u64::try_from(record.events.len()).unwrap_or(u64::MAX);
            continue;
        }
        if !wait_armed(&open, notified.as_mut().as_pin_mut()).await {
            return;
        }
    }
}

/// Park until the catalog may have changed, or until the caller closes the stream.
///
/// `waiting` is enabled before the caller reads. A directory catalog wakes it from the file
/// watch. A catalog without one falls back to a 50ms poll. Returns false when the stream should
/// stop.
async fn wait_armed(
    open: &girder_core::StreamOpen,
    waiting: Option<std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>>,
) -> bool {
    tokio::select! {
        () = open.cancel.cancelled() => false,
        incoming = open.uplink.recv() => matches!(incoming, Ok(Some(_))),
        () = async {
            if let Some(waiting) = waiting {
                waiting.await;
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => true,
    }
}

async fn observe_state_stream(
    open: girder_core::StreamOpen,
    catalog: Arc<dyn SessionCatalog>,
    turns: Arc<dyn LiveTurns>,
    pending: Arc<dyn PendingInputs>,
    queues: Arc<dyn girder_core::QueueAgentSource>,
) {
    let method = "observeState";
    let session_id = match girder_core::api::api_final::session_id_of(&open.call.args, method) {
        Ok(id) => id,
        Err(error) => {
            let _ = open.fail(error);
            return;
        }
    };
    let mut last = match passive_state(&session_id, &catalog, &turns, &pending, &queues).await {
        Ok(value) => value,
        Err(error) => {
            let _ = open.fail(error);
            return;
        }
    };
    if !open.push(last.clone()) {
        return;
    }
    loop {
        let (sources, poll) = activity_sources(
            catalog.as_ref(),
            turns.as_ref(),
            pending.as_ref(),
            queues.as_ref(),
        );
        let first = sources[0].as_ref().map(|wake| wake.notified());
        let second = sources[1].as_ref().map(|wake| wake.notified());
        let third = sources[2].as_ref().map(|wake| wake.notified());
        let fourth = sources[3].as_ref().map(|wake| wake.notified());
        tokio::pin!(first, second, third, fourth);
        if let Some(waiting) = first.as_mut().as_pin_mut() {
            waiting.enable();
        }
        if let Some(waiting) = second.as_mut().as_pin_mut() {
            waiting.enable();
        }
        if let Some(waiting) = third.as_mut().as_pin_mut() {
            waiting.enable();
        }
        if let Some(waiting) = fourth.as_mut().as_pin_mut() {
            waiting.enable();
        }
        let current = match passive_state(&session_id, &catalog, &turns, &pending, &queues).await {
            Ok(value) => value,
            Err(_) => return,
        };
        if current != last {
            last = current.clone();
            if !open.push(current) {
                return;
            }
        }
        tokio::select! {
            () = open.cancel.cancelled() => return,
            incoming = open.uplink.recv() => {
                if !matches!(incoming, Ok(Some(_))) {
                    return;
                }
            }
            () = await_sources(
                first.as_mut().as_pin_mut(),
                second.as_mut().as_pin_mut(),
                third.as_mut().as_pin_mut(),
                fourth.as_mut().as_pin_mut(),
                poll.then_some(std::time::Duration::from_millis(50)),
            ) => {}
        }
    }
}

async fn await_sources(
    first: Option<std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>>,
    second: Option<std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>>,
    third: Option<std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>>,
    fourth: Option<std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>>,
    limit: Option<std::time::Duration>,
) {
    tokio::select! {
        () = async { if let Some(waiting) = first { waiting.await } else { std::future::pending().await } } => {}
        () = async { if let Some(waiting) = second { waiting.await } else { std::future::pending().await } } => {}
        () = async { if let Some(waiting) = third { waiting.await } else { std::future::pending().await } } => {}
        () = async { if let Some(waiting) = fourth { waiting.await } else { std::future::pending().await } } => {}
        () = async { if let Some(limit) = limit { tokio::time::sleep(limit).await } else { std::future::pending().await } } => {}
    }
}

fn activity_sources(
    catalog: &dyn SessionCatalog,
    turns: &dyn LiveTurns,
    pending: &dyn PendingInputs,
    queues: &dyn girder_core::QueueAgentSource,
) -> ([Option<Arc<tokio::sync::Notify>>; 4], bool) {
    let sources = [
        catalog.changes(),
        turns.changes(),
        pending.changes(),
        queues.changes(),
    ];
    let poll = sources.iter().any(Option::is_none);
    (sources, poll)
}

/// Default `session.wait` deadline when the caller omits `timeoutMs`.
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Largest delay a timer in this protocol accepts. Matches upstream `MAX_TIMER_DELAY_MS`.
const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

/// Largest integer JSON can round-trip. `afterSeq` and `maxMessages` must stay inside it.
const MAX_SAFE_INTEGER: i64 = (1_i64 << 53) - 1;

/// How many assistant records `session.wait` returns when `maxMessages` is omitted.
const DEFAULT_MAX_MESSAGES: usize = 50;

/// `session.wait`: sample until the session is idle, needs input, or the deadline lands.
///
/// A running turn keeps the call open. Sources that can signal a change wake the wait; a source
/// that cannot still falls back to a 50ms sample. The returned `state` is
/// the passive snapshot, and `output.records` are assistant messages after `afterSeq` through that
/// snapshot's cursor, newest first.
async fn session_wait(
    args: &Value,
    catalog: &Arc<dyn SessionCatalog>,
    turns: &Arc<dyn LiveTurns>,
    pending: &Arc<dyn PendingInputs>,
    queues: &Arc<dyn girder_core::QueueAgentSource>,
) -> Result<Value, RemoteError> {
    let bounds = wait_bounds(args)?;
    let started = std::time::Instant::now();
    loop {
        let (sources, poll) = activity_sources(
            catalog.as_ref(),
            turns.as_ref(),
            pending.as_ref(),
            queues.as_ref(),
        );
        let first = sources[0].as_ref().map(|wake| wake.notified());
        let second = sources[1].as_ref().map(|wake| wake.notified());
        let third = sources[2].as_ref().map(|wake| wake.notified());
        let fourth = sources[3].as_ref().map(|wake| wake.notified());
        tokio::pin!(first, second, third, fourth);
        if let Some(waiting) = first.as_mut().as_pin_mut() {
            waiting.enable();
        }
        if let Some(waiting) = second.as_mut().as_pin_mut() {
            waiting.enable();
        }
        if let Some(waiting) = third.as_mut().as_pin_mut() {
            waiting.enable();
        }
        if let Some(waiting) = fourth.as_mut().as_pin_mut() {
            waiting.enable();
        }
        let state = passive_state(&bounds.session_id, catalog, turns, pending, queues).await?;
        if started.elapsed() >= bounds.timeout {
            return finish_wait("timeout", &state, catalog, &bounds).await;
        }
        let needs_input = state
            .get("pendingInput")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        let running = state
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let active = state
            .get("active")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let action =
            girder_core::session::wait::wait_action(girder_core::session::wait::WaitFacts {
                pending_input: needs_input,
                running,
                active,
            });
        match action {
            girder_core::session::wait::WaitAction::InputRequired => {
                return finish_wait("input-required", &state, catalog, &bounds).await;
            }
            girder_core::session::wait::WaitAction::Idle
            | girder_core::session::wait::WaitAction::CheckIdle => {
                return finish_wait("idle", &state, catalog, &bounds).await;
            }
            girder_core::session::wait::WaitAction::Continue => {
                let remaining = bounds.timeout.saturating_sub(started.elapsed());
                let limit = if poll {
                    remaining.min(std::time::Duration::from_millis(50))
                } else {
                    remaining
                };
                await_sources(
                    first.as_mut().as_pin_mut(),
                    second.as_mut().as_pin_mut(),
                    third.as_mut().as_pin_mut(),
                    fourth.as_mut().as_pin_mut(),
                    Some(limit),
                )
                .await;
            }
        }
    }
}

struct WaitBounds {
    session_id: String,
    timeout: std::time::Duration,
    after_seq: i64,
    max_messages: usize,
}

fn wait_bounds(args: &Value) -> Result<WaitBounds, RemoteError> {
    let session_id = args
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if session_id.is_empty() {
        return Err(invalid_wait("`sessionId` is required".to_owned()));
    }
    let after_seq = match args.get("afterSeq") {
        None => -1,
        Some(value) => after_seq_of(value)?,
    };
    let max_messages = match args.get("maxMessages") {
        None => DEFAULT_MAX_MESSAGES,
        Some(value) => positive_count(value, "maxMessages")?,
    };
    let timeout = match args.get("timeoutMs") {
        None => std::time::Duration::from_millis(DEFAULT_WAIT_TIMEOUT_MS),
        Some(value) => wait_timeout(value)?,
    };
    Ok(WaitBounds {
        session_id,
        timeout,
        after_seq,
        max_messages,
    })
}

fn after_seq_of(value: &Value) -> Result<i64, RemoteError> {
    let Some(seq) = value.as_i64() else {
        return Err(invalid_wait(
            "afterSeq must be an integer greater than or equal to -1".to_owned(),
        ));
    };
    if seq < -1 || seq > MAX_SAFE_INTEGER {
        return Err(invalid_wait(
            "afterSeq must be an integer greater than or equal to -1".to_owned(),
        ));
    }
    Ok(seq)
}

fn positive_count(value: &Value, name: &str) -> Result<usize, RemoteError> {
    let Some(count) = value.as_u64() else {
        return Err(invalid_wait(format!(
            "{name} must be a positive safe integer"
        )));
    };
    if count == 0 || count > u64::try_from(MAX_SAFE_INTEGER).unwrap_or(u64::MAX) {
        return Err(invalid_wait(format!(
            "{name} must be a positive safe integer"
        )));
    }
    usize::try_from(count)
        .map_err(|_| invalid_wait(format!("{name} must be a positive safe integer")))
}

fn wait_timeout(value: &Value) -> Result<std::time::Duration, RemoteError> {
    let Some(ms) = value.as_u64() else {
        return Err(invalid_wait(
            "timeoutMs must be a positive supported timer delay".to_owned(),
        ));
    };
    if ms == 0 || ms > MAX_TIMER_DELAY_MS {
        return Err(invalid_wait(
            "timeoutMs must be a positive supported timer delay".to_owned(),
        ));
    }
    Ok(std::time::Duration::from_millis(ms))
}

fn invalid_wait(message: String) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: "wait".to_owned(),
        message,
    }
}

async fn finish_wait(
    reason: &str,
    state: &Value,
    catalog: &Arc<dyn SessionCatalog>,
    bounds: &WaitBounds,
) -> Result<Value, RemoteError> {
    let through = state.get("cursor").and_then(Value::as_i64).unwrap_or(-1);
    let Some(record) = catalog.read(&bounds.session_id).await else {
        return Err(RemoteError::Failed(format!(
            "no session `{}` is known to this deployment",
            bounds.session_id
        )));
    };
    let output = assistant_output(
        &record.events,
        bounds.after_seq,
        through,
        bounds.max_messages,
    )?;
    Ok(serde_json::json!({
        "reason": reason,
        "state": state,
        "output": output,
    }))
}

/// Assistant appends after `after_seq` through `through_seq`, newest first.
///
/// Sequence numbers here are 0-based, matching upstream `session.state`. `after_seq` is exclusive,
/// so `-1` includes the whole log.
fn assistant_output(
    events: &[SessionEvent],
    after_seq: i64,
    through_seq: i64,
    max_messages: usize,
) -> Result<Value, RemoteError> {
    let covered = i64::try_from(events.len())
        .ok()
        .and_then(|length| length.checked_sub(1))
        .unwrap_or(-1);
    if through_seq > covered {
        return Err(RemoteError::Failed(
            "gateway/internal: Session history no longer covers the wait cursor".to_owned(),
        ));
    }
    let mut records = Vec::new();
    let mut truncated = false;
    let mut seq = through_seq;
    while seq > after_seq {
        if seq >= 0
            && let Some(event) = events.get(usize::try_from(seq).unwrap_or(usize::MAX))
            && matches!(event, SessionEvent::AssistantMessage { .. })
        {
            if records.len() == max_messages {
                truncated = true;
                break;
            }
            if let Some(record) = girder_core::api::remote::session_wire_record(
                event,
                u64::try_from(seq).unwrap_or(0),
            ) {
                records.push(record);
            }
        }
        seq -= 1;
    }
    Ok(serde_json::json!({
        "records": records,
        "truncated": truncated,
    }))
}

/// The passive state a client reads from `session.observeState`.
///
/// `cursor` is the inclusive 0-based last event sequence, and an empty log is `-1`.
/// `active` is true while a turn is running or a live agent still holds the inbox. `queue` is
/// that inbox — next-turn items as `queued`, next-step user input as `steering`, anything else
/// as `context` — an empty list while a turn is running with nothing queued, and null when the
/// session is idle. `pendingInput` lists the kinds [`PendingInputs`] reports for this session.
async fn passive_state(
    session_id: &str,
    catalog: &Arc<dyn SessionCatalog>,
    turns: &Arc<dyn LiveTurns>,
    pending: &Arc<dyn PendingInputs>,
    queues: &Arc<dyn girder_core::QueueAgentSource>,
) -> Result<Value, RemoteError> {
    let Some(record) = catalog.read(session_id).await else {
        return Err(RemoteError::Failed(format!(
            "no session `{session_id}` is known to this deployment"
        )));
    };
    let cursor = i64::try_from(record.events.len())
        .ok()
        .and_then(|length| length.checked_sub(1))
        .unwrap_or(-1);
    let running = turns.is_running(session_id);
    let agent = queues.agent(session_id);
    Ok(serde_json::json!({
        "cursor": cursor,
        "active": running || agent.is_some(),
        "running": running,
        "queue": queue_snapshot(agent.as_deref(), running),
        "pendingInput": pending.kinds(session_id),
    }))
}

/// A host that has not connected a live-agent table.
fn no_queues() -> Arc<dyn girder_core::QueueAgentSource> {
    Arc::new(|_: &str| None)
}

/// The inbox a client renders, in the order upstream reports it: next turn, then next step.
fn queue_snapshot(agent: Option<&girder_core::Agent>, running: bool) -> Value {
    let Some(agent) = agent else {
        return if running {
            Value::Array(Vec::new())
        } else {
            Value::Null
        };
    };
    let inbox = agent.inbox_state();
    if inbox.is_empty() {
        return if running {
            Value::Array(Vec::new())
        } else {
            Value::Null
        };
    }
    let mut items = Vec::new();
    for message in inbox.next_turn() {
        items.push(queue_item(message, "queued"));
    }
    for message in inbox.next_step() {
        let placement = match &message.source {
            girder_core::MessageSource::User { .. } => "steering",
            _ => "context",
        };
        items.push(queue_item(message, placement));
    }
    Value::Array(items)
}

fn queue_item(message: &girder_core::ChatMessage, placement: &str) -> Value {
    serde_json::json!({
        "id": message.id.as_str(),
        "placement": placement,
        "content": message.content,
    })
}

/// Keys whose folded value changed, in key order.
fn changed_projection_keys(
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
) -> Vec<Value> {
    after
        .iter()
        .filter_map(|(key, val)| {
            if before.get(key) == Some(val) {
                None
            } else {
                Some(serde_json::json!({ "key": key, "val": val }))
            }
        })
        .collect()
}

/// Runs one `session.state` call.
async fn state(
    args: &Value,
    catalog: &Arc<dyn SessionCatalog>,
    projections: &Arc<ProjectionRegistry>,
    turns: &Arc<dyn LiveTurns>,
    links: Option<&girder_core::SessionLinkGraph>,
) -> Result<Value, RemoteError> {
    let request: StateRequest =
        serde_json::from_value(args.clone()).map_err(|error| invalid(error.to_string()))?;

    let session_id = request.session_id.trim();
    if session_id.is_empty() {
        // A blank id is a client bug, not a missing session: answering "no such session ``" would
        // blame the deployment for a request that never named one.
        return Err(invalid("`sessionId` is required".to_owned()));
    }

    let Some(record) = catalog.read(session_id).await else {
        // Not an empty success: see the module docs — a snapshot of a session that does not exist is
        // indistinguishable from a snapshot of a brand-new one, and a caller would prompt into it.
        return Err(RemoteError::Failed(format!(
            "no session `{session_id}` is known to this deployment"
        )));
    };

    // Read after the log, and the failure mode is stated rather than hidden: a rename that lands
    // between the two reads leaves a label from the moment after the events, which is a smaller
    // inconsistency than the one this method exists to remove (several round trips, several moments),
    // and a title sidecar is not part of the log so there is no single read that covers both.
    let title = catalog.title(session_id).await;
    // The links touching this session, read from the graph the registry holds. A registry with no graph answers empty
    // rather than failing: a deployment that never recorded a link has none, which is not an error.
    let links = links.map(|graph| {
        graph
            .related(session_id)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    });
    // The ids alone, not the records: `list` returns events and a session's own children need nothing but their names.
    let directory_ids: Vec<String> = catalog
        .list()
        .await
        .into_iter()
        .map(|record| record.id)
        .collect();

    // One count, used for both fields, so they cannot disagree. `try_from` rather than a cast: a log
    // longer than `u64::MAX` events cannot exist, and saturating is still the honest answer if the
    // arithmetic ever runs on a narrower platform.
    let events = u64::try_from(record.events.len()).unwrap_or(u64::MAX);

    // Folded from this session's log, not from the registry's stored map. The registry holds the
    // definitions; its stored map is whatever the last caller wrote, which is another session when
    // two are open. The sequence matches `last_seq`: one-based, empty is 0.
    let projected = projections
        .project(&record.events)
        .with_header_directory(catalog.working_directory(session_id).as_deref());
    let mut folded = serde_json::Map::new();
    for (key, value) in projected.values {
        folded.insert(key, value);
    }

    let value = SessionStateValue {
        session_id: session_id.to_owned(),
        events,
        // One-based: the event at index `i` has seq `i + 1`, so the last seq of `events` events is
        // `events`, and an empty log has `0`.
        last_seq: events,
        title,
        projections: folded,
        running: turns.is_running(session_id),
        interrupted: ends_mid_turn(&record.events),
        // The last thing the model said, as text. Taken from the log rather than from a live buffer so a cold read
        // answers it: a client opening a conversation shows the final answer whether or not the turn that produced it
        // is still running, and a turn that only called tools has no text at all -- which is a different state from one
        // that has not answered yet.
        request_header: girder_core::llm::request_header::request_header_of(&record.events)
            .and_then(|header| serde_json::to_value(&header).ok()),
        last_text: girder_core::final_text(&record.events),
        last_turn: girder_core::final_output(&record.events).map(|output| output.turn),
        // Counts rather than durations: the log records no timestamps, so every duration a richer stats layer would
        // report is not derivable from it. Reporting one as zero would say the model was instant rather than that
        // nothing measured it.
        counts: girder_core::SessionStats::of(&record.events),
        // Per tool, because the totals say how much work happened and this says which tools did it. A deployment asking
        // "is the shell slow or the build" needs the second, and `SessionStats` keeps only the first.
        tool_usage: girder_core::tool_usage(&record.events),
        digest: girder_core::digest_session(&record.events),
        // The directory's own ids are the only record of the relationship, so it is read here rather than asked of the
        // log. A missing parent directory leaves both fields empty rather than failing the call.
        forked_from: origin_of(&record.id),
        // Every id in the directory, listed on each call: the relationship is not stored, so it has to be derived, and
        // the alternative is a second source of truth that could disagree with the fork's own naming. A deployment with
        // thousands of sessions should record the edge instead -- `crate::session::branch::Forest` is what that would store.
        forks: children_of(&record.id, &directory_ids),
        related: related_of(&record.id, links.as_deref()),
        // How much of the conversation each producer wrote, in characters. The message count says how many turns
        // happened; this says who filled the context, and they are different questions -- one long model answer and one
        // short question is a conversation where the model wrote almost all of it.
        attribution: girder_core::ContentAttribution::of(&messages_of(&record.events)),
        // One line per turn, so a client rendering a sidebar does not fold the log a second time to find out what
        // happened. The `open` marker is separate from the segments because a turn can be both unfinished and have a
        // complete summary -- guessing from an empty one would report a live turn as a blank.
        timeline: girder_core::session_outline(&record.events),
        open_turn: girder_core::open_turn(&record.events),
    };

    serde_json::to_value(value).map_err(|error| RemoteError::Failed(error.to_string()))
}

/// Every message the log carries, in order.
///
/// Only the message-bearing events are collected: a step boundary or a tool result describes the conversation without
/// being part of what either side wrote, and counting them would attribute protocol noise to a producer.
fn messages_of(events: &[SessionEvent]) -> Vec<girder_core::ChatMessage> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::UserMessage { message }
            | SessionEvent::AssistantMessage { message, .. }
            | SessionEvent::DeveloperMessage { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Builds an `INVALID_ARGUMENTS` error for this method.
fn invalid(message: String) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: METHOD.to_owned(),
        message,
    }
}

/// Whether the log's last turn boundary is a start with no matching end.
///
/// The test is "a `turn/start` came after the last `turn/end`", not "the final event is a
/// `turn/start`": the loop writes a start per model round and other records in between, so the final
/// record of a cut-off turn is usually a message or a tool call.
///
/// A log scan rather than a read of the `turnBoundary` projection, even though that projection holds
/// this answer: the projection only knows about a log the host has folded, and a cold read — the case
/// this method exists for — must answer without one. The same fold appears in
/// [`crate::session::resume_rpc`]; sharing it would need a new public helper in a file this module may
/// not edit, and ten lines that must agree are the smaller cost.
fn ends_mid_turn(events: &[SessionEvent]) -> bool {
    // Delegated, not reimplemented. This function and `crate::session::timeline::open_turn` answered the same question with two
    // different rules -- this one ignored the turn number, the other compared it -- so a log whose `turn/end` named the
    // wrong turn made `interrupted` and `openTurn` contradict each other. `turn_closure` is now the one implementation.
    girder_core::turns_interrupted(events)
}

/// Where a session forked from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkOrigin {
    /// The session it was forked from.
    pub parent: String,
    /// When, from the id's stamp.
    pub at_ms: u64,
}

/// The parent recorded in an id, when the id is the shape a fork produces.
///
/// Delegates to `session_branch`, which owns the parsing: two implementations of "what a fork id looks like" would drift,
/// and the one in this file would be the one nobody tested.
fn origin_of(id: &str) -> Option<ForkOrigin> {
    let link = girder_core::links_from_ids(std::slice::from_ref(&id.to_owned()))
        .into_iter()
        .next()?;
    Some(ForkOrigin {
        parent: link.parent,
        at_ms: link.at_ms,
    })
}

/// The sessions whose id names `id` as their parent.
///
/// Through the forest rather than by filtering links here: the forest is what drops a link whose parent is absent and
/// what refuses one that would close a loop, and a caller doing its own filtering would reintroduce both problems.
fn children_of(id: &str, ids: &[String]) -> Vec<String> {
    girder_core::forest_from_ids(ids)
        .children(id)
        .into_iter()
        .map(|link| link.child.clone())
        .collect()
}

/// One session this one is linked to, from the viewer's side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedSession {
    /// The other session.
    pub session: String,
    /// Whether this session is the source or the target of the link.
    pub direction: String,
    /// What the link says.
    pub kind: String,
    /// An optional note.
    pub note: Option<String>,
}

/// The links touching `session`, rendered from that session's point of view.
///
/// The direction is resolved here rather than in the client: a link stores one `from` and one `to`, and a client that had
/// to work out which end it was holding would get it wrong for exactly one of the two.
fn related_of(
    session: &str,
    links: Option<&[girder_core::CrossSessionLink]>,
) -> Vec<RelatedSession> {
    let Some(links) = links else {
        return Vec::new();
    };
    links
        .iter()
        .filter_map(|link| {
            let (other, direction) = if link.from == session {
                (&link.to, "outgoing")
            } else if link.to == session {
                (&link.from, "incoming")
            } else {
                return None;
            };
            Some(RelatedSession {
                session: other.clone(),
                direction: direction.to_owned(),
                kind: format!("{:?}", link.kind).to_lowercase(),
                note: link.note.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod related_tests {
    use super::*;

    fn link(from: &str, to: &str) -> girder_core::CrossSessionLink {
        girder_core::CrossSessionLink {
            from: from.to_owned(),
            to: to.to_owned(),
            kind: girder_core::SessionLinkKind::References,
            at_ms: 1,
            note: Some("note".to_owned()),
        }
    }

    #[test]
    fn a_link_is_outgoing_from_its_source_and_incoming_at_its_target() {
        // The client should not have to work out which end it is holding: it gets one wrong of the two.
        let links = vec![link("a", "b")];

        let from_a = related_of("a", Some(&links));
        let from_b = related_of("b", Some(&links));

        assert_eq!(from_a[0].session, "b");
        assert_eq!(from_a[0].direction, "outgoing");
        assert_eq!(from_b[0].session, "a");
        assert_eq!(from_b[0].direction, "incoming");
    }

    #[test]
    fn a_session_not_in_the_link_gets_nothing() {
        let links = vec![link("a", "b")];

        assert!(related_of("c", Some(&links)).is_empty());
        assert!(related_of("a", None).is_empty());
    }

    #[test]
    fn the_kind_is_rendered_lowercase_for_a_client() {
        let links = vec![link("a", "b")];

        assert_eq!(related_of("a", Some(&links))[0].kind, "references");
    }
}
