//! The `session.resume` Remote method: re-attaching an existing session to this deployment.
//!
//! A client that reopens a conversation after a restart, a reload, or a network drop calls
//! `resume` before it can prompt again. The method answers one question — *is this session usable
//! here, and where does its log end* — and the answer is what the client needs to continue: the
//! event count and the last sequence number are the cursor a follow or history subscription starts
//! from, so a resumed client repeats nothing and misses nothing.
//!
//! # Why "resuming" is a read in this build
//!
//! [`AgentRuntime::prompt`] takes the session as an argument (`Arc<dyn Session>`) rather than
//! looking one up in a table. There is no runtime-side registry of live sessions to re-load into:
//! a session is live for exactly as long as a prompt is running, and the log on disk is the whole
//! of its state. So "resume" is: read the log back, confirm a writable handle can be opened for it,
//! and report where it ends. The write handle check is what keeps the answer a promise: every later
//! prompt opens that same handle, so a catalog that can read a log but not hand out a session would
//! otherwise answer `resumed: true` and then refuse the prompt that follows.
//!
//! # What upstream's resume does that this build does not
//!
//! Upstream's `agents.resume` rebuilds a *live agent*: it repairs a crashed log, restores pending
//! queue input, takes an exclusive write handle, and re-composes the agent's scoped world. The parts
//! of that which are genuinely absent here are listed rather than imitated, because a half-copy
//! would write synthetic records no reader in this build understands:
//!
//! - **Crash repair is not performed.** Upstream appends closers for a turn that was cut off by a
//!   crash: a `tool/result` citing the unanswered call, a `step/end`, a `turn/end` with reason
//!   `interrupted`, and a `session/end-seed` marker. This build has no event that can carry
//!   "outcome unknown" as a coded error, and appending a `tool/result` here would hand the loop a
//!   fabricated observation. Instead the log is *reported* as [`SessionResumeValue::interrupted`]
//!   and left untouched: a read-only answer cannot corrupt a log, and a client that wants to warn
//!   its user can.
//! - **Pending input is not restored.** This build's inbox is in-memory only (the same gap
//!   `session_queue_rpc` documents), so a restart loses queued prompts regardless of what `resume`
//!   reports. Restoring it is therefore not a missing step in this method but a missing durable
//!   store.
//! - **No exclusive write ownership is taken.** Upstream rejects a second resume with
//!   `session/writer-held` because it holds a process-wide write handle. `SessionCatalog` here
//!   exposes no lock, so two hosts may both "resume" the same session; the runtime's per-session
//!   busy check is what prevents two concurrent turns on one host, and nothing prevents it across
//!   hosts.
//! - **No agent composition.** Upstream re-resolves the session's preset, working directory, model
//!   selection, and subagent ownership from the stored header before publishing an agent.
//!   [`SessionRecord`](girder_core::SessionRecord) carries none of those fields here, so there is
//!   nothing to re-compose from; a caller that needs a preset must send it with the next prompt.
//!
//! Stating the gaps is the point: a client that relied on repair would be relying on something
//! that never happened, and silence would read as "nothing to repair".
//!
//! # Why an unknown session is an error
//!
//! Returning an empty success would tell a client that the session exists, is empty, and may be
//! prompted — which is exactly the state a *new* session is in. A caller that mistyped an id, or
//! whose session was deleted on another host, would then prompt into a session that has to be
//! created on demand, and the mistake would surface as a conversation that lost its history.
//!
//! # Why the declaration is a write
//!
//! Resuming itself changes no bytes, but it is the step that puts a session back under a client's
//! hand for the prompts that follow. Declaring a read would let a read-only mirror answer
//! `resumed: true` for a session it can never accept a prompt for — a promise the deployment cannot
//! keep. Refusing the call instead is the failure that a client can act on.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use girder_core::{
    AgentRuntime, Declaration, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog,
    SessionEvent,
};

/// The namespace `session.resume` lives under.
pub const NAMESPACE: &str = "session";

/// The method name within [`NAMESPACE`], spelled as upstream spells it.
pub const METHOD: &str = "resume";

/// The resource kind this method declares to an access policy.
const RESOURCE_KIND: &str = "session";

/// The arguments of `session.resume`.
///
/// Only the id: resuming is a statement about an existing session, and every other field upstream
/// might accept (a preset, a working directory) belongs to the prompt that follows, which is where
/// this build applies it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumeRequest {
    /// Which session to re-attach.
    session_id: String,
}

/// What `session.resume` answers.
///
/// `events` and `last_seq` are the same numbers `session.follow` and `session.page` use: sequence
/// numbers are one-based positions in the log, so a client can feed `lastSeq` straight back as
/// `afterSeq` without arithmetic of its own. Getting that consistent across the three methods is
/// what makes the cursor portable; an off-by-one here would silently duplicate or drop one event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResumeValue {
    /// The session that was resumed, echoed so a client with several in flight can match the reply.
    pub session_id: String,
    /// Always `true` on success; an unusable session is reported as an error instead.
    ///
    /// Present because the shape is what a client branches on, and a client that had to treat a
    /// missing field as success would be guessing at this build's spelling.
    pub resumed: bool,
    /// How many events the log holds, which is also its next one-based sequence number minus one.
    pub events: u64,
    /// The sequence number of the last recorded event, or `0` for a session with no events.
    pub last_seq: u64,
    /// Whether a turn is in flight right now on this host.
    ///
    /// Read from the runtime rather than the log: a turn that is running has usually not written
    /// its `turn/end` yet, so the log cannot tell "about to be appended" from "abandoned".
    pub running: bool,
    /// Whether the log ends inside a turn, as a crash or an abrupt shutdown leaves it.
    ///
    /// Informational only — this build does not repair the log (see the module docs). A client may
    /// show it, and a follow subscription will still replay the events that are there.
    pub interrupted: bool,
}

/// Registers `session.resume`.
///
/// The catalog and the runtime are registration arguments, like every other `register_*` function:
/// the deployment owns its sessions and its running turns, and a module that reached for global
/// state would answer for a different set than the rest of the registry. The runtime is used only
/// for its in-flight map, which is the one piece of resume state this build actually has.
///
/// The declaration is [`Declaration::write`]: see the module docs for why a method that writes
/// nothing still commits the deployment to accepting a prompt afterwards.
pub fn register_session_resume_method(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    runtime: &Arc<AgentRuntime>,
) {
    registry.declare(NAMESPACE, METHOD, Declaration::write(RESOURCE_KIND));
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    let runtime = Arc::clone(runtime);
    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        let runtime = Arc::clone(&runtime);
        Box::pin(async move { resume(&call.args, catalog.as_ref(), runtime.as_ref()).await })
    });
}

/// Runs one `session.resume` call.
async fn resume(
    args: &Value,
    catalog: &dyn SessionCatalog,
    runtime: &AgentRuntime,
) -> Result<Value, RemoteError> {
    let request: ResumeRequest =
        serde_json::from_value(args.clone()).map_err(|error| invalid(error.to_string()))?;

    let session_id = request.session_id.trim();
    if session_id.is_empty() {
        return Err(invalid("`sessionId` is required".to_owned()));
    }

    let Some(record) = catalog.read(session_id).await else {
        // `RemoteError` has no not-found variant, so this is `Failed` with the id in the message: a
        // client can still show which session was missing. Answering `resumed: true` with zero
        // events would be indistinguishable from a fresh session, which is the one reading that
        // would lead a caller to prompt into the wrong conversation.
        return Err(RemoteError::Failed(format!(
            "no session `{session_id}` is known to this deployment"
        )));
    };

    // The existence check above is not enough: a catalog may be able to decode a log while having
    // no writable handle to hand out (a read-only backing store, for instance). The prompt that
    // follows resume opens exactly this handle, so failing here names the real cause instead of
    // letting the next call fail with a message about a missing session.
    if catalog.open(session_id).is_none() {
        return Err(RemoteError::Failed(format!(
            "session `{session_id}` exists but cannot be opened for writing; \
             this deployment cannot accept a prompt for it"
        )));
    }

    let events = u64::try_from(record.events.len()).unwrap_or(u64::MAX);

    let value = SessionResumeValue {
        session_id: session_id.to_owned(),
        resumed: true,
        events,
        // One-based: an event at index `i` has seq `i + 1`, and an empty log has none, so the last
        // seq is `0` rather than `1`.
        last_seq: events,
        running: runtime.is_running(session_id),
        interrupted: ends_mid_turn(&record.events),
    };

    serde_json::to_value(value).map_err(|error| RemoteError::Failed(error.to_string()))
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
/// `turn/start`": the loop writes a start per model round and other records in between, so the
/// final record of a cut-off turn is usually a message or a tool call. A finished turn always ends
/// with `turn/end`, which is what makes the boundary the right thing to look at.
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
