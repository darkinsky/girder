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
//!   ([`crate::inbox`]), which a cold read cannot reconstruct; [`crate::session_queue_rpc`] documents
//!   the same gap for `updateQueue`. This build also has no gateway tracking pending Remote requests.
//!
//! One name differs from upstream. Upstream calls the cursor `cursor`; this build calls it `lastSeq`,
//! which is what [`crate::session_resume_rpc`] and `session.follow` already report. A client comparing
//! this snapshot against its follow cursor therefore does so without translating between two names for
//! one number — and the whole point of this method is that those numbers agree.
//!
//! # Why an unknown session is an error
//!
//! A snapshot answering zero events, no title, and an empty projection view is exactly what a *new*
//! session looks like. A caller that mistyped an id — or whose session was deleted on another host —
//! would render a fresh blank conversation instead of a failure, and the next prompt would land in it.
//! Upstream refuses this with `session/not-found`; [`RemoteError`] here has no not-found variant, so
//! the answer is `Failed` with the id in the message, as [`crate::session_resume_rpc`] does.
//!
//! # Why the declaration is a read
//!
//! The method writes nothing: it reads the log, the catalog's stored title, and the projection map. It
//! must stay callable under a read-only policy — a mirror that can show a session but not prompt into
//! it is a supported deployment, and declaring a write would refuse it the one call it exists for.

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
    /// Reported rather than repaired, for the reason [`crate::session_resume_rpc`] gives: this build has
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
    /// Where this session forked from, when its id records it.
    ///
    /// Reconstructed from the id rather than read from the log, because a fork writes no edge: it copies a prefix and
    /// nothing else. `None` means this session was not forked, or was created by a build that named it differently.
    pub forked_from: Option<ForkOrigin>,
    /// The sessions that forked from this one.
    ///
    /// Children by id, which is what a client drawing a tree needs and what no field in this session's own log holds.
    pub forks: Vec<String>,
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
    register_session_state_method_with(registry, catalog, projections, &turns);
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
    let catalog = Arc::clone(catalog);
    let projections = Arc::clone(projections);
    let turns = Arc::clone(turns);

    // Declared beside the handler and before it, so the policy judges the call the handler would take.
    // A read: nothing below writes, and a read-only mirror must still be able to render a session.
    registry.declare(NAMESPACE, METHOD, Declaration::read(RESOURCE_KIND));

    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let catalog = Arc::clone(&catalog);
        let projections = Arc::clone(&projections);
        let turns = Arc::clone(&turns);
        Box::pin(async move { state(&call.args, &catalog, &projections, &turns).await })
    });
}

/// Runs one `session.state` call.
async fn state(
    args: &Value,
    catalog: &Arc<dyn SessionCatalog>,
    projections: &Arc<ProjectionRegistry>,
    turns: &Arc<dyn LiveTurns>,
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

    let mut folded = serde_json::Map::new();
    for (key, value) in projections.view() {
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
        last_text: girder_core::final_text(&record.events),
        last_turn: girder_core::final_output(&record.events).map(|output| output.turn),
        // Counts rather than durations: the log records no timestamps, so every duration a richer stats layer would
        // report is not derivable from it. Reporting one as zero would say the model was instant rather than that
        // nothing measured it.
        counts: girder_core::SessionStats::of(&record.events),
        // Per tool, because the totals say how much work happened and this says which tools did it. A deployment asking
        // "is the shell slow or the build" needs the second, and `SessionStats` keeps only the first.
        tool_usage: girder_core::tool_usage(&record.events),
        // The directory's own ids are the only record of the relationship, so it is read here rather than asked of the
        // log. A missing parent directory leaves both fields empty rather than failing the call.
        forked_from: origin_of(&record.id),
        // Every id in the directory, listed on each call: the relationship is not stored, so it has to be derived, and
        // the alternative is a second source of truth that could disagree with the fork's own naming. A deployment with
        // thousands of sessions should record the edge instead -- `session_branch::Forest` is what that would store.
        forks: children_of(&record.id, &directory_ids),
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
/// [`crate::session_resume_rpc`]; sharing it would need a new public helper in a file this module may
/// not edit, and ten lines that must agree are the smaller cost.
fn ends_mid_turn(events: &[SessionEvent]) -> bool {
    let mut open = false;
    for event in events {
        match event {
            SessionEvent::TurnStart { .. } => open = true,
            SessionEvent::TurnEnd { .. } => open = false,
            _ => {}
        }
    }
    open
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
