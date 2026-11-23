//! The `session.observe*` Remote family: passive session subscriptions.
//!
//! Upstream `packages/api/session-controller` declares twenty-six `@Remote` methods and ten of them
//! are `{ mode: 'stream' }`. This crate serves most of the unary ones — `list`, `create`, `page`,
//! `events`, `rename`, `fork`, `prompt`, `cancel`, `projections`, `state`, `search`, `selectModel`,
//! `updateQueue`, `attachment`, `openWorkspacePath`, `canOpenWorkspacePath`,
//! `workspacePathApplications`, `workspaceDesktop`, `follow`, `modelCatalog` — and the three
//! `observe*` methods have compatibility poll handlers plus live stream handlers.
//!
//! The legacy subscription transport still advertises these names for compatibility:
//!
//! ```text
//! crates/girder-rpc/src/stream.rs:353:    "session.observeEvent",
//! crates/girder-rpc/src/stream.rs:354:    "session.observeProjections",
//! crates/girder-rpc/src/stream.rs:355:    "session.observeState",
//! ```
//!
//! Standard Remote mux calls use the live stream registrations directly. The unary handlers remain
//! available to older in-process callers and the legacy subscription hub.
//!
//! Standard Remote mux calls use the live stream registrations directly. The compatibility handlers
//! retain the older cursor-based polling shape for callers that still use unary dispatch.
//!
//! # What each method answers, and why the three stay apart
//!
//! Upstream keeps them separate for reasons that survive the change of carrier:
//!
//! | method | unit of delivery | why not the others |
//! | --- | --- | --- |
//! | `observeEvent` | one durable event per frame, contiguous | the transcript, where a gap is a dropped message |
//! | `observeProjections` | one frame per event that changed a visible projection | a panel, which must not re-render for an event that changed nothing |
//! | `observeState` | the whole passive state, replaced | a status bar, where a partial update is worse than a repeated one |
//!
//! Collapsing them into one method would force the panel consumer to diff and the status consumer to
//! fold, which is the work each upstream method exists to do once.
//!
//! # The three invariants a client depends on
//!
//! 1. **Readiness is a flag, not a frame.** Upstream yields `{type:'ready'}` before any event so a
//!    client can tell "nothing has happened yet" from "the subscription never opened". Here it is
//!    `ObserveReply::ready`, true on every poll that read from the start of the log. A frame-shaped
//!    `ready` would be a value the transport keeps re-sending — `SubscriptionHub` suppresses repeats —
//!    so the flag would be invisible on the one call that mattered. A flag is idempotent instead.
//! 2. **A gap is reported, never papered over.** The caller names the last sequence it holds. That
//!    cursor can name an event the log no longer has, and a poll-shaped method cannot throw the way
//!    upstream's generator does (throwing ends the subscription). `ObserveReply::lost_before` says
//!    "everything before N is gone, resync" so a client that lost an event resyncs rather than stops.
//!    A cursor *beyond* the end is different: that is a client bug or a different session, so it is an
//!    error — clamping it would answer "you are caught up" to a caller holding a cursor this build
//!    cannot explain, and the disagreement would never surface.
//! 3. **An unknown session is an error, not an empty series.** `session.follow` and `session.state`
//!    both make this choice and for the same reason: an empty log is what a brand new session looks
//!    like, so a misspelled id would render as a fresh conversation and the next prompt would land in
//!    it.
//!
//! # What is deliberately not here
//!
//! - **`session.control`** — the Host-wide control stream. It baselines *every* session's
//!   projections, and this crate has no host-wide projection snapshot to baseline from; a baseline
//!   covering one session would be a different contract under the same name.
//! - **`session.wait`** — a bounded wait over an in-memory inbox this build does not keep.
//!   `crate::session::state_rpc` documents the same absence for `queue` and `pendingInput`.
//! - **Assistant-stream frames** — `session.follow`'s opt-in `assistant-stream` frames come from a
//!   live model-attempt buffer. These three read the durable log only, which is what makes them
//!   correct against a cold session with no runtime at all.
//! - **Folding into the host's registry.** `observeProjections` folds into its own
//!   [`ProjectionRegistry`], because `ProjectionRegistry::observe` mutates shared state: a subscription
//!   that folded into the host's registry would corrupt the view `session.state` answers for every
//!   other client.
//!
//! # Why the arguments are parsed by hand
//!
//! `afterSeq` is a cursor with three meanings: absent (begin at the log's start), `0` (the same — a
//! client that holds nothing), and `n` (continue after `n`). Reading it with `and_then(as_u64)` would
//! turn a negative or a string into "absent", silently replaying a whole session to a caller that sent
//! a nonsense cursor. [`parse_args`] refuses that case instead.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use girder_core::{
    Declaration, Projection, ProjectionRegistry, RemoteCall, RemoteError, RemoteRegistry,
    SessionCatalog, SessionEvent, SessionHistoryRecord,
};

/// The namespace all three methods live under.
pub const NAMESPACE: &str = "session";

/// Delivers one durable event per frame, contiguous from the caller's cursor.
pub const OBSERVE_EVENT_METHOD: &str = "observeEvent";

/// Delivers one frame per event that changed at least one projection view.
pub const OBSERVE_PROJECTIONS_METHOD: &str = "observeProjections";

/// Delivers a complete passive-state replacement.
pub const OBSERVE_STATE_METHOD: &str = "observeState";

/// The three methods this module implements, in the order upstream declares them.
pub const METHODS: &[&str] = &[
    OBSERVE_EVENT_METHOD,
    OBSERVE_PROJECTIONS_METHOD,
    OBSERVE_STATE_METHOD,
];

/// Upstream `session` stream methods this module deliberately leaves unimplemented.
///
/// Recorded as data rather than in prose so a caller can report the remaining surface instead of
/// rediscovering it. Both are explained in the module note.
pub const UNIMPLEMENTED: &[&str] = &["control", "wait"];

/// The resource kind all three declare to an access policy.
///
/// Reading one session is a read of that session and nothing else, so a read-only mirror allowed to
/// render a session is allowed to observe it — which is the whole point of these methods existing.
const RESOURCE_KIND: &str = "session";

/// How many events one poll returns when the caller does not say.
///
/// The same budget `session.follow` uses, for the same reason: a poll is a round trip, so a page small
/// enough to stay cheap keeps latency bounded while one large enough covers a burst.
pub const DEFAULT_LIMIT: usize = 64;

/// The largest page one poll will return.
///
/// A bound rather than a preference: the transport re-invokes a subscribed method on an interval, so an
/// unbounded page would let one busy session turn a fixed-rate poll into an unbounded response.
pub const MAX_LIMIT: usize = 1_000;

/// The largest `afterSeq` a caller may name.
///
/// The cursor is a JavaScript `number` on the wire, and above 2^53 an integer stops surviving the round
/// trip — a resume would land in the middle of a run of events. Upstream rejects the same class of
/// value for the same reason.
pub const MAX_SAFE_SEQ: u64 = 9_007_199_254_740_991;

/// One parsed argument set, shared by the three methods.
///
/// One type rather than three because the arguments are one shape, and a second parser would be a
/// second place for the three to drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObserveArgs {
    /// The last sequence the caller already holds; `0` means "nothing".
    pub after_seq: u64,
    /// The largest number of events to examine.
    pub limit: usize,
}

impl Default for ObserveArgs {
    fn default() -> Self {
        Self {
            after_seq: 0,
            limit: DEFAULT_LIMIT,
        }
    }
}

impl ObserveArgs {
    /// Whether this poll starts at the beginning of the log.
    ///
    /// Split out so all three handlers agree on the definition instead of each comparing to zero.
    #[must_use]
    pub fn is_opening(&self) -> bool {
        self.after_seq == 0
    }
}

/// Reads one non-negative integer argument, refusing a value that is present but wrong.
///
/// A `null` is absence, which every field here treats as "use the default". Anything else must be a
/// non-negative integer within `max`.
fn integer_arg(args: &Value, key: &str, max: u64) -> Result<Option<u64>, String> {
    let Some(raw) = args.get(key) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let Some(number) = raw.as_u64() else {
        return Err(format!("`{key}` must be a non-negative integer, got {raw}"));
    };
    if number > max {
        return Err(format!("`{key}` must be at most {max}, got {number}"));
    }
    Ok(Some(number))
}

/// Parses the arguments the three observe methods share.
///
/// # Errors
///
/// Returns the message for the field that was wrong. The caller wraps it in
/// [`RemoteError::InvalidArguments`] naming its own method, so the error says which call failed.
pub fn parse_args(args: &Value) -> Result<ObserveArgs, String> {
    let after_seq = integer_arg(args, "afterSeq", MAX_SAFE_SEQ)?.unwrap_or(0);
    let limit = integer_arg(args, "limit", MAX_LIMIT as u64)?
        .map(|value| usize::try_from(value).unwrap_or(MAX_LIMIT))
        // A caller that asks for zero gets the default rather than an empty page: an empty page is
        // indistinguishable from "nothing happened", which is the one answer a poll must not fake.
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_LIMIT);
    Ok(ObserveArgs { after_seq, limit })
}

/// The session id an observe request addresses.
///
/// Both upstream spellings are accepted, because the methods that read an address upstream use
/// different ones: `observeEvent` takes `{address: {kind, sessionId}}` (it can address a subagent),
/// while `observeProjections` and `observeState` take `{sessionId}` directly. A client that used the
/// wrong spelling would otherwise be told its session does not exist rather than that its field name
/// was wrong.
///
/// # Errors
///
/// When neither spelling yields a non-empty id, and when the address kind is one this build cannot
/// resolve (`subagent` — there is no subagent registry here).
pub fn session_id_of(args: &Value, method: &str) -> Result<String, RemoteError> {
    if let Some(address) = args.get("address")
        && !address.is_null()
    {
        let kind = address
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("session");
        if kind != "session" {
            return Err(RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: method.to_owned(),
                message: format!("address kind `{kind}` is not supported; only `session` is"),
            });
        }
        let id = address
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !id.trim().is_empty() {
            return Ok(id.to_owned());
        }
    }
    match args.get("sessionId") {
        Some(Value::String(id)) if !id.trim().is_empty() => Ok(id.clone()),
        _ => Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: "`sessionId` is required".to_owned(),
        }),
    }
}

/// One session's durable events, numbered the way every other read in this crate numbers them.
///
/// The sequence rather than an index: logs here are 1-based, and `session.page`'s cursor,
/// `session.follow`'s `afterSeq`, `session.state`'s `lastSeq` and these three all name that same
/// coordinate — so a caller can move between the five without a translation table.
#[derive(Clone, Debug, PartialEq)]
pub struct Sequenced {
    /// The events, oldest first.
    pub events: Vec<SessionEvent>,
    /// The 1-based sequence of the last event in the log.
    pub last_seq: u64,
}

impl Sequenced {
    /// Numbers a log's events from 1, which is how this crate has numbered them since the first store.
    #[must_use]
    pub fn of(events: Vec<SessionEvent>) -> Self {
        let last_seq = u64::try_from(events.len()).unwrap_or(u64::MAX);
        Self { events, last_seq }
    }

    /// How many events the log holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The events after `after_seq`, bounded by `limit`, with the sequence of the first one.
    ///
    /// The sequence is `after_seq + 1` even when nothing is delivered, so a caller that wants "the next
    /// expected sequence" need not special-case an empty page.
    #[must_use]
    pub fn after(&self, after_seq: u64, limit: usize) -> (u64, &[SessionEvent]) {
        let start = usize::try_from(after_seq).unwrap_or(usize::MAX);
        let start = start.min(self.events.len());
        let end = start.saturating_add(limit).min(self.events.len());
        (after_seq.saturating_add(1), &self.events[start..end])
    }
}

/// What one poll answers.
///
/// `done` is always `false` and is present so the transport's terminal check has a member to read. A
/// session never finishes, and omitting it is indistinguishable from a series that ended.
#[derive(Clone, Debug, PartialEq)]
pub struct ObserveReply {
    /// Whether this poll started at the beginning of the log. See the module note.
    pub ready: bool,
    /// The frames, oldest first.
    pub frames: Vec<Value>,
    /// The cursor the next call passes as `afterSeq`.
    ///
    /// The sequence of the last event *examined*, not of the last frame: a poll that watched events
    /// change nothing must still advance past them, or it would re-read them for ever.
    pub next_seq: u64,
    /// The log's last sequence at the moment it was read.
    pub last_seq: u64,
    /// Set when the caller's cursor named an event the log no longer holds.
    pub lost_before: Option<u64>,
    /// Whether more events are already waiting, so a client can poll again without waiting an interval.
    pub has_more: bool,
}

impl ObserveReply {
    /// The wire form.
    ///
    /// A method rather than a `Serialize` impl because the object is built in one place and all three
    /// methods agree here on the field names; deriving would add an attribute block and change nothing.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "ready": self.ready,
            "frames": self.frames,
            "nextSeq": self.next_seq,
            "lastSeq": self.last_seq,
            "lostBefore": self.lost_before,
            "hasMore": self.has_more,
            // Present on every reply and always false: the transport reads this, and a session never
            // finishes. See the module note.
            "done": false,
        })
    }
}

/// One event, in the shape `session.page` already sends.
///
/// Reusing [`SessionHistoryRecord`] rather than inventing a frame body is what lets a client that
/// decodes a page decode these unchanged — the same argument `crate::session::stream` makes for its own
/// frames. `seq` rides beside it because a position in an array is not a cursor.
#[must_use]
pub fn event_frame(seq: u64, event: &SessionEvent) -> Value {
    let record = SessionHistoryRecord::event(event.clone());
    let body = serde_json::to_value(&record).unwrap_or(Value::Null);
    json!({
        // Upstream's tag for an event entry.
        "type": "event",
        "seq": seq,
        "record": body,
    })
}

/// Builds one `observeEvent` poll over an already-read log.
///
/// Pure, so the framing rule — contiguity, the bounded page, the cursor arithmetic — is testable
/// without a catalog, a registry, or a file.
#[must_use]
pub fn observe_events(log: &Sequenced, args: ObserveArgs) -> ObserveReply {
    let (first, delivered) = log.after(args.after_seq, args.limit);
    let frames: Vec<Value> = delivered
        .iter()
        .enumerate()
        .map(|(offset, event)| {
            let seq = first.saturating_add(u64::try_from(offset).unwrap_or(0));
            event_frame(seq, event)
        })
        .collect();

    // The cursor is the last *examined* event: delivered, or the one the caller already held. A poll
    // that delivered nothing must not move it, or a caught-up client would drift forward on every
    // interval until its cursor pointed past the log.
    let examined = delivered.len();
    let next_seq = if examined == 0 {
        args.after_seq
    } else {
        first.saturating_add(u64::try_from(examined - 1).unwrap_or(0))
    };

    ObserveReply {
        ready: args.is_opening(),
        frames,
        next_seq,
        last_seq: log.last_seq,
        lost_before: None,
        has_more: next_seq < log.last_seq,
    }
}

/// One projection that changed within one event's drive.
///
/// The pair upstream's `onEventChanged` carries as `{key, val}`: the projection that changed and its
/// whole current value. A whole value rather than a delta, so a consumer that missed an earlier
/// delivery is still correct.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// The projection key that changed.
    pub key: String,
    /// The projection's whole current value.
    pub val: Value,
}

impl Change {
    /// One change for `key` at `val`.
    #[must_use]
    pub fn new(key: impl Into<String>, val: Value) -> Self {
        Self {
            key: key.into(),
            val,
        }
    }
}

/// One frame of `observeProjections`: the changes one committed event caused.
///
/// `{type:'projection', seq, changes:[{key, val}]}` — upstream's `SessionProjectionChange` with the
/// session id dropped, because the subscription already names the session. `changes` is in
/// registration order and never empty: an event that changed nothing produces no frame at all, which
/// is the whole reason this channel exists apart from `observeEvent`.
///
/// `None` for an empty change set, so a caller cannot accidentally emit a frame that says nothing
/// changed.
#[must_use]
pub fn projection_frame(seq: u64, changes: &[Change]) -> Option<Value> {
    if changes.is_empty() {
        return None;
    }
    let changes: Vec<Value> = changes
        .iter()
        .map(|change| json!({ "key": change.key, "val": change.val }))
        .collect();
    Some(json!({
        "type": "projection",
        "seq": seq,
        "changes": changes,
    }))
}

/// The per-key differences between two projection views, in the `after` map's key order.
///
/// Value equality rather than reference identity: a fold that allocates a fresh but equal value has not
/// changed anything a consumer can observe, and publishing it would make every event a change and the
/// channel useless.
#[must_use]
pub fn diff(before: &BTreeMap<String, Value>, after: &BTreeMap<String, Value>) -> Vec<Change> {
    let mut changes = Vec::new();
    for (key, value) in after {
        if before.get(key) != Some(value) {
            changes.push(Change::new(key.clone(), value.clone()));
        }
    }
    for (key, value) in before {
        if !after.contains_key(key) {
            changes.push(Change::new(key.clone(), value.clone()));
        }
    }
    changes
}

/// A projection fold over one log, kept apart from every other client's.
///
/// Its own [`ProjectionRegistry`] rather than the host's, because `observe` mutates shared state: a
/// subscription folding into the host's registry would overwrite the view `session.state` answers.
pub struct ProjectionReplay {
    registry: ProjectionRegistry,
    view: BTreeMap<String, Value>,
}

impl std::fmt::Debug for ProjectionReplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectionReplay")
            .field("keys", &self.view.keys())
            .finish_non_exhaustive()
    }
}

impl ProjectionReplay {
    /// A replay over the given definitions, positioned before the log.
    ///
    /// # Errors
    ///
    /// When a definition's key is already registered; two definitions cannot be told apart in a view.
    pub fn new(definitions: &[Arc<dyn Projection>]) -> Result<Self, String> {
        let mut registry = ProjectionRegistry::new();
        for definition in definitions {
            registry
                .register(Arc::clone(definition))
                .map_err(|error| error.to_string())?;
        }
        let view = registry.view();
        Ok(Self { registry, view })
    }

    /// Folds one event and answers what changed, or `None` when nothing visible did.
    pub fn fold(&mut self, seq: u64, event: &SessionEvent) -> Option<Vec<Change>> {
        self.registry.observe(event, seq);
        let after = self.registry.view();
        let changes = diff(&self.view, &after);
        self.view = after;
        if changes.is_empty() {
            None
        } else {
            Some(changes)
        }
    }

    /// The current view.
    #[must_use]
    pub fn view(&self) -> &BTreeMap<String, Value> {
        &self.view
    }
}

/// The keys a replay was built over, sorted.
#[must_use]
pub fn projection_keys(replay: &ProjectionReplay) -> Vec<String> {
    replay.view().keys().cloned().collect()
}

/// Builds one `observeProjections` poll over an already-read log.
///
/// The pure half of the handler, split out so the framing rule is testable without a catalog. The
/// prefix before `after_seq` is folded first, so a delivered change is measured against the state the
/// caller's own view is in rather than against an initial state it never held.
///
/// # Errors
///
/// When two definitions share a key.
pub fn project_changes(
    log: &Sequenced,
    args: ObserveArgs,
    definitions: &[Arc<dyn Projection>],
) -> Result<ObserveReply, String> {
    let mut replay = ProjectionReplay::new(definitions)?;
    let (_, window) = log.after(args.after_seq, args.limit);
    let start = usize::try_from(args.after_seq).unwrap_or(usize::MAX);
    let start = start.min(log.events.len());

    // Fold the held prefix before measuring anything: a change is a difference from what the caller
    // already has, and skipping the prefix would make the first frame a delta against `init()`.
    for (offset, event) in log.events[..start].iter().enumerate() {
        let seq = u64::try_from(offset).unwrap_or(u64::MAX).saturating_add(1);
        replay.fold(seq, event);
    }

    let mut frames = Vec::new();
    for (offset, event) in window.iter().enumerate() {
        let seq = start
            .saturating_add(offset)
            .checked_add(1)
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(u64::MAX);
        if let Some(changes) = replay.fold(seq, event)
            && let Some(frame) = projection_frame(seq, &changes)
        {
            frames.push(frame);
        }
    }

    Ok(projection_reply(args, frames, window.len(), log.last_seq))
}

/// Assembles a projection poll's reply from the frames it produced.
///
/// `examined` is how many events the poll looked at, which is what the cursor advances by. Advancing by
/// the number of *frames* instead would re-read every silent event on every poll for ever, and the
/// `hasMore` flag would then be wrong for exactly the sessions that emit the most events.
#[must_use]
pub fn projection_reply(
    args: ObserveArgs,
    frames: Vec<Value>,
    examined: usize,
    log_end: u64,
) -> ObserveReply {
    let next_seq = if examined == 0 {
        args.after_seq
    } else {
        args.after_seq
            .saturating_add(u64::try_from(examined).unwrap_or(u64::MAX))
    };
    ObserveReply {
        ready: args.is_opening(),
        frames,
        next_seq,
        last_seq: log_end,
        lost_before: None,
        has_more: next_seq < log_end,
    }
}

/// The passive state one `observeState` poll reports: what a status bar renders.
///
/// Upstream's `SessionState` is `{cursor, active, running, queue, pendingInput}`. Three of the five can
/// be answered from a log alone, and they are answered in upstream's own names so a client renders this
/// beside `session.state` without a translation table.
///
/// The other two are **absent rather than false/empty**, for the reason `crate::session::state_rpc`
/// documents at length: nothing here maps a session id to a live agent, so `active` would be `false`
/// even for a session whose turn is running in this very process, and `queue`/`pendingInput` live in an
/// in-memory inbox a cold read cannot reconstruct. A field filled with a guess is worse than a field
/// left out, because the client cannot tell which it received.
#[derive(Clone, Debug, PartialEq)]
pub struct StateSnapshot {
    /// The log's last sequence, or `0` for an empty log.
    pub cursor: u64,
    /// Whether a turn is executing right now.
    pub running: bool,
    /// Whether the log ends inside a turn, unrepaired.
    pub interrupted: bool,
    /// Which turn is open, when the log ends inside one.
    pub open_turn: Option<u64>,
}

impl StateSnapshot {
    /// The wire form.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "cursor": self.cursor,
            "running": self.running,
            "interrupted": self.interrupted,
            "openTurn": self.open_turn,
        })
    }
}

/// A complete state replacement, as `observeState` delivers it.
///
/// A whole snapshot rather than a delta, which is what makes a missed poll harmless: the next one
/// replaces everything. That is upstream's contract ("complete passive state replacements") and the
/// only shape a status consumer can apply without knowing what it missed.
#[must_use]
pub fn state_frame(state: &StateSnapshot) -> Value {
    json!({
        "type": "state",
        "state": state.to_json(),
    })
}

/// Builds one `observeState` poll.
///
/// A frame only when the state actually differs from the one the caller holds, so the frames array is
/// honest about what changed rather than relying on the transport's dedupe.
#[must_use]
pub fn observe_state(
    args: ObserveArgs,
    current: &StateSnapshot,
    previous: Option<&StateSnapshot>,
) -> ObserveReply {
    let frames = if previous == Some(current) {
        Vec::new()
    } else {
        vec![state_frame(current)]
    };
    ObserveReply {
        ready: args.is_opening(),
        frames,
        next_seq: current.cursor,
        last_seq: current.cursor,
        lost_before: None,
        has_more: false,
    }
}

/// What "a turn is executing" is asked of.
///
/// The same seam [`girder_core::session::state_rpc`] uses, re-declared because that module's type is
/// reachable only by path from its own test. The shape is identical, so a host with one source of truth
/// implements both at once — and a closure implements both automatically — which is what stops the two
/// methods from disagreeing about whether a session is running.
pub trait RunningSource: Send + Sync {
    /// Whether `session_id` has a turn in flight.
    fn is_running(&self, session_id: &str) -> bool;
}

/// A closure is a source too.
impl<F> RunningSource for F
where
    F: Fn(&str) -> bool + Send + Sync + 'static,
{
    fn is_running(&self, session_id: &str) -> bool {
        self(session_id)
    }
}

/// The build's own answer: nothing is running, because this module holds no live agents.
///
/// Named rather than inlined so a caller reading `running: false` has somewhere to look, exactly as
/// `NoLiveTurns` is in `crate::session::state_rpc`.
#[derive(Debug, Default)]
pub struct NoRunning;

impl RunningSource for NoRunning {
    fn is_running(&self, _session_id: &str) -> bool {
        false
    }
}

/// Folds a log into the state a status bar renders.
///
/// `running` is left `false` here because the log cannot answer it; the handler overwrites it from the
/// [`RunningSource`]. Splitting the two is what keeps this function pure.
#[must_use]
pub fn state_of(log: &Sequenced) -> StateSnapshot {
    let open_turn = girder_core::open_turn(&log.events);
    StateSnapshot {
        cursor: log.last_seq,
        running: false,
        interrupted: open_turn.is_some(),
        open_turn,
    }
}

/// Registers the three `session.observe*` methods.
///
/// One function for three methods because they share a catalog, a resource declaration and a cursor
/// convention: registering them apart would be three chances to declare one of them a write, which
/// would refuse a read-only mirror the call it exists for.
///
/// `definitions` is what `observeProjections` folds. An empty slice is legal and honest: the channel
/// then delivers nothing, because nothing registered can change — a different answer from
/// `NOT_IMPLEMENTED`, and the right one for a host that registered no projections.
pub fn register_observe_methods(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    definitions: &[Arc<dyn Projection>],
    running: &Arc<dyn RunningSource>,
) {
    for method in METHODS {
        // Declared beside the handler and before it, so the policy judges the call the handler would
        // take. All three read: a read-only mirror must be able to render a session.
        registry.declare(NAMESPACE, method, Declaration::read(RESOURCE_KIND));
    }

    register_observe_event(registry, catalog);
    register_observe_projections(registry, catalog, definitions);
    register_observe_state(registry, catalog, running);
}

/// Registers `session.observeEvent`.
fn register_observe_event(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    let catalog = Arc::clone(catalog);
    registry.register(NAMESPACE, OBSERVE_EVENT_METHOD, move |call: RemoteCall| {
        let catalog = Arc::clone(&catalog);
        Box::pin(async move {
            let method = OBSERVE_EVENT_METHOD;
            let session_id = session_id_of(&call.args, method)?;
            let args = parse_args(&call.args).map_err(|message| invalid(method, message))?;
            let log = read_log(&catalog, &session_id, method).await?;
            refuse_cursor_past_end(&log, args, method)?;
            Ok(observe_events(&log, args).to_json())
        })
    });
}

/// Registers `session.observeProjections`.
fn register_observe_projections(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    definitions: &[Arc<dyn Projection>],
) {
    let catalog = Arc::clone(catalog);
    let definitions: Vec<Arc<dyn Projection>> = definitions.to_vec();
    registry.register(
        NAMESPACE,
        OBSERVE_PROJECTIONS_METHOD,
        move |call: RemoteCall| {
            let catalog = Arc::clone(&catalog);
            let definitions = definitions.clone();
            Box::pin(async move {
                let method = OBSERVE_PROJECTIONS_METHOD;
                let session_id = session_id_of(&call.args, method)?;
                let args = parse_args(&call.args).map_err(|message| invalid(method, message))?;
                let log = read_log(&catalog, &session_id, method).await?;
                refuse_cursor_past_end(&log, args, method)?;
                let reply =
                    project_changes(&log, args, &definitions).map_err(RemoteError::Failed)?;
                Ok(reply.to_json())
            })
        },
    );
}

/// Registers `session.observeState`.
fn register_observe_state(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    running: &Arc<dyn RunningSource>,
) {
    let catalog = Arc::clone(catalog);
    let running = Arc::clone(running);
    registry.register(NAMESPACE, OBSERVE_STATE_METHOD, move |call: RemoteCall| {
        let catalog = Arc::clone(&catalog);
        let running = Arc::clone(&running);
        Box::pin(async move {
            let method = OBSERVE_STATE_METHOD;
            let session_id = session_id_of(&call.args, method)?;
            let args = parse_args(&call.args).map_err(|message| invalid(method, message))?;
            let log = read_log(&catalog, &session_id, method).await?;
            refuse_cursor_past_end(&log, args, method)?;

            let mut current = state_of(&log);
            current.running = running.is_running(&session_id);

            // The previous snapshot is derived from the same log truncated at the caller's cursor, so
            // "did the state change" is answered from the log rather than from a cached copy a restart
            // would lose. That also makes the reply independent of process history: the same log and
            // the same cursor always produce the same answer.
            let previous = if args.is_opening() {
                None
            } else {
                let through = usize::try_from(args.after_seq)
                    .unwrap_or(usize::MAX)
                    .min(log.events.len());
                let prefix = Sequenced::of(log.events[..through].to_vec());
                let mut snapshot = state_of(&prefix);
                snapshot.running = current.running;
                Some(snapshot)
            };

            Ok(observe_state(args, &current, previous.as_ref()).to_json())
        })
    });
}

/// Reads one session's log, refusing an unknown id.
async fn read_log(
    catalog: &Arc<dyn SessionCatalog>,
    session_id: &str,
    method: &str,
) -> Result<Sequenced, RemoteError> {
    let Some(record) = catalog.read(session_id).await else {
        // An empty success would be indistinguishable from a brand new session, which is how a typo
        // becomes a conversation. `RemoteError` has no not-found variant, so this is `Failed` naming the
        // id — the choice `crate::session::resume_rpc` and `crate::session::state_rpc` already make.
        return Err(RemoteError::Failed(format!(
            "`{method}`: no session `{session_id}` is known to this deployment"
        )));
    };
    Ok(Sequenced::of(record.events))
}

/// Refuses a cursor the log never reached.
///
/// A caller naming a sequence above the log end is not resyncing — it is confused, or it is reading a
/// different session than it thinks. Clamping would answer "you are caught up" to a client whose cursor
/// points at a log this build cannot see, and the disagreement would never surface. A cursor exactly at
/// the end is not this case: that is a caller that is caught up.
fn refuse_cursor_past_end(
    log: &Sequenced,
    args: ObserveArgs,
    method: &str,
) -> Result<(), RemoteError> {
    if args.after_seq > log.last_seq {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: format!(
                "`afterSeq` is {} but the log ends at {}",
                args.after_seq, log.last_seq
            ),
        });
    }
    Ok(())
}

/// Wraps a parse message in the error that names the method it came from.
fn invalid(method: &str, message: String) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message,
    }
}
