//! The `session.updateQueue` Remote method: changing input that is still waiting.
//!
//! A prompt does not always run the moment it arrives. Input typed while a turn is running lands in
//! a pending queue and is consumed by the next turn or the next step, and until then the client that
//! typed it can take it back or reword it. Upstream answers that with one method and three actions —
//! `edit`, `remove`, and `steer` — and this module registers the same method over the queues this
//! build already has ([`crate::inbox`]).
//!
//! # Why the item is addressed by id rather than by position
//!
//! A position is a statement about a moment: the third pending message is only the third while
//! nothing else is queued, drained, or withdrawn. A client that held a position across a render, a
//! click, and a network round trip would be editing whatever had moved into that slot. The message id
//! is stable for the item's whole life, so it is what the wire carries and what this layer resolves.
//!
//! # What the queue layer actually offers, and what this module therefore does
//!
//! [`crate::inbox::Inbox`] is position-based end to end: [`Inbox::next_turn`] and [`Inbox::next_step`]
//! hand out slices, and [`Inbox::apply`] takes a [`Splice`] whose address is `(target, start)`.
//! There is no `find(id)` and no `replace(id, ..)`. So locating an item is a scan performed here,
//! and the mutation is a one-element splice at the index the scan found. Three consequences are
//! stated plainly because a caller will meet them:
//!
//! 1. **Locate and mutate are two lock acquisitions, not one.** The scan reads
//!    [`Agent::inbox_state`], which clones; the splice then takes the lock again. The only writer that
//!    can move an index in between is a whole-queue drain (`take_turn`/`take_step`), which empties
//!    the queue and turns the splice into an honest "not pending" rather than a silent edit of the
//!    wrong entry. [`AgentQueueMap`]-backed agents have no other single-item remover. Because "the
//!    only writer happens to drain everything" is a property of today's call sites rather than of the
//!    type, the mutation also verifies the entry it replaced and undoes itself when it was not the
//!    one asked for — see [`replace_pending`].
//! 2. **The edit is not durable.** Upstream records a queue mutation as an `agent/inbox/spliced`
//!    event. [`crate::session::SessionEvent`] has no such variant, so this module has nothing to
//!    append to: the change lives in the live agent's memory and a restart loses it. Inventing an
//!    event here would put a shape in every session log that no reader understands, so the gap is
//!    reported instead (see [`QueueError::NoLiveAgent`]) and left for the log layer to close.
//! 3. **`steer` is refused, not emulated.** Steering hands a message *into* a turn that is already
//!    running. This build has no channel for that — [`crate::subagent_tools`] declines it for the
//!    same reason — and editing the pending queue instead would deliver the text as a new turn, which
//!    is a different promise to the user. [`QueueError::SteerUnavailable`] says so.
//!
//! # Why an item that has started is refused, and how it is told apart
//!
//! `edit` and `remove` are defined over input that has *not* begun executing. Once the loop has
//! taken a message, its text is on the model-visible surface; changing it afterwards would rewrite
//! what the model was already told and leave the log disagreeing with its own history. Such an item
//! is gone from the inbox, and "gone" has two causes that a client must be able to tell apart: never
//! queued under that id, or already consumed. The second is answered from the durable log — a
//! consumed message was appended as a [`SessionEvent::UserMessage`] — which is why this module needs
//! the [`SessionCatalog`] as well as the queues.
//!
//! # Why the seams are parameters
//!
//! The crate has no registry mapping a session id to its live [`Agent`]: [`crate::AgentRuntime`]
//! tracks cancellation tokens for running turns, not handles. [`QueueAgentSource`] is that missing
//! lookup, and [`AgentQueueMap`] is a working one for hosts that keep their own map. Both are passed
//! in at registration, like every other `register_*` function here: the host decides which
//! capabilities exist, and a module that reached for global state would decide for it — with the
//! additional defect that two registries in one process would answer for different sets of sessions.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use serde_json::{Value, json};

use girder_core::{
    Access, Agent, ChatMessage, ContentBlock, Declaration, Inbox, InboxError, InboxTarget,
    RemoteCall, RemoteError, RemoteRegistry, SessionCatalog, SessionEvent, Splice,
};

/// The namespace `updateQueue` lives under.
pub const NAMESPACE: &str = "session";

/// The method name within [`NAMESPACE`], spelled as upstream spells it.
pub const METHOD: &str = "updateQueue";

/// The resource kind this method declares to an access policy.
///
/// A queue mutation changes state, so it declares a write. Without a declaration the policy would
/// treat the call as a read of `session` and a read-only mirror would be able to rewrite a user's
/// pending prompt — the decision would be made by an absent declaration rather than by the operator.
const RESOURCE_KIND: &str = "session";

/// Where the live pending queues are found.
///
/// A trait rather than a concrete map because the host owns agent lifetime: a deployment that keeps
/// agents in its own table should not have to copy them into a second one here. A trait also keeps
/// the "no agent for this session" case an ordinary answer instead of an error to be swallowed.
pub trait QueueAgentSource: Send + Sync {
    /// The live agent holding `session_id`'s pending queues, when one exists.
    ///
    /// `None` means no agent is live. It does not mean the session is unknown — this build cannot
    /// reconstruct a queue from a log, so a cold session has no pending input at all.
    fn agent(&self, session_id: &str) -> Option<Arc<Agent>>;
}

/// A closure is a source too, so a host with one line of lookup need not write a type.
impl<F> QueueAgentSource for F
where
    F: Fn(&str) -> Option<Arc<Agent>> + Send + Sync + 'static,
{
    fn agent(&self, session_id: &str) -> Option<Arc<Agent>> {
        self(session_id)
    }
}

/// The session-id to live-agent map this crate otherwise lacks.
///
/// Generic over nothing on purpose: it is a table, and a table with policy in it would be a second
/// place where "may this peer edit this session" is decided.
#[derive(Debug, Default)]
pub struct AgentQueueMap {
    agents: Mutex<BTreeMap<String, Arc<Agent>>>,
}

impl AgentQueueMap {
    /// Creates an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the agent that holds `session_id`'s queue.
    ///
    /// A later agent for the same session replaces the earlier one: a resumed session has a new
    /// agent, and keeping the stale handle would route edits into a queue nobody drains.
    pub fn attach(&self, session_id: impl Into<String>, agent: Arc<Agent>) {
        self.lock().insert(session_id.into(), agent);
    }

    /// Forgets a session's agent.
    ///
    /// Returns the handle it held, so a caller shutting a session down can tell "there was one" from
    /// "there was not" without a second lookup.
    pub fn detach(&self, session_id: &str) -> Option<Arc<Agent>> {
        self.lock().remove(session_id)
    }

    /// How many sessions have a live agent.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no session has a live agent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    // Poisoning is recovered rather than propagated: a panic while holding this map leaves a table
    // of handles, not a half-written one, and refusing every later call would turn one panic into a
    // permanently unusable queue surface.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<Agent>>> {
        self.agents.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl QueueAgentSource for AgentQueueMap {
    fn agent(&self, session_id: &str) -> Option<Arc<Agent>> {
        self.lock().get(session_id).cloned()
    }
}

/// The arguments of `session.updateQueue`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateQueueRequest {
    /// The session whose queue is being changed.
    session_id: String,
    /// The pending message's id, which is stable for the item's whole life.
    item_id: String,
    /// What to do to it.
    action: QueueAction,
}

/// What a caller wants done to one pending item.
///
/// Tagged by `kind` exactly as upstream is, so a generated client sends the shape it already has. The
/// enum is closed: an unknown `kind` is a parse error naming the variants, which is better than
/// accepting it and doing nothing.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum QueueAction {
    /// Replace the item's content; its id, role, and provenance stay.
    Edit {
        /// Replacement blocks. Parsed as [`ContentBlock`] rather than as text-only, so a non-text
        /// block reaches validation and is refused with a message about the block it sent, instead of
        /// failing as an unreadable argument object.
        content: Vec<ContentBlock>,
    },
    /// Withdraw the item.
    Remove,
    /// Hand the item to the running turn. Refused here; see the module docs.
    Steer,
}

/// Why a queue mutation was not applied.
///
/// A typed error rather than a formatted string so the two callers that differ — the RPC layer, which
/// maps it to a wire error, and a test, which asserts on the reason — read the same value.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QueueError {
    /// The request named no session.
    #[error("`sessionId` is required")]
    MissingSessionId,

    /// The request named no item.
    #[error("`itemId` is required")]
    MissingItemId,

    /// No session has that id.
    #[error("no session `{session_id}` is known to this deployment")]
    UnknownSession {
        /// The id that was asked for.
        session_id: String,
    },

    /// The session exists but has no live agent, so it has no pending queue.
    ///
    /// A refusal rather than a silent success: this build keeps pending input in memory only, and a
    /// client told "accepted" would believe an edit had landed that nothing holds.
    #[error(
        "session `{session_id}` has no live agent, and this build keeps pending input in memory only, so it has nothing queued to change"
    )]
    NoLiveAgent {
        /// The session that was asked for.
        session_id: String,
    },

    /// No pending item has that id, and no record shows it was consumed.
    #[error("no queued item `{item_id}` is pending in this session")]
    NotPending {
        /// The id that was asked for.
        item_id: String,
    },

    /// The item has already begun executing, so it is no longer a queue edit.
    #[error(
        "`{item_id}` has already started executing; changing it after the model has seen it is steering, which this build has no channel for"
    )]
    AlreadyStarted {
        /// The id that was asked for.
        item_id: String,
    },

    /// Steering was requested and this build cannot do it.
    #[error(
        "steering is unavailable in this build: there is no channel that injects a message into a turn that is already running"
    )]
    SteerUnavailable,

    /// The replacement content held a block that is not text.
    #[error("queue edits accept text content only; the request carried a `{kind}` block")]
    EditNotText {
        /// The offending block's type tag.
        kind: &'static str,
    },

    /// The replacement content was empty or entirely whitespace.
    #[error("queue edit content must include non-whitespace text")]
    EditBlank,

    /// The inbox refused the splice, or the entry it landed on was not the requested one.
    ///
    /// The second case cannot be raised by [`Inbox::apply`]: it validates the position, not the
    /// identity at that position. It is raised here because this module addresses an item by id and
    /// has to prove the id is what it changed.
    #[error("the pending queue could not be changed: {reason}")]
    Inbox {
        /// What the queue layer reported.
        reason: String,
    },
}

impl QueueError {
    /// Maps the reason onto a wire error.
    ///
    /// Arguments that could never be satisfied become `InvalidArguments`, which is what a client can
    /// fix; everything else becomes `Failed`. The registry's error enum has no domain codes, so a
    /// client cannot branch on "not pending" against "steer unavailable" except by reading the
    /// message — stated here because a client author would otherwise assume it can.
    fn into_remote(self) -> RemoteError {
        match self {
            Self::MissingSessionId
            | Self::MissingItemId
            | Self::EditNotText { .. }
            | Self::EditBlank => RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: METHOD.to_owned(),
                message: self.to_string(),
            },
            other => RemoteError::Failed(other.to_string()),
        }
    }
}

/// Registers `session.updateQueue`.
///
/// Takes the catalog as well as the queue source: the catalog is what tells "already started" from
/// "never queued" (see the module docs), and a caller that supplied only the queues would get a
/// method that cannot answer the question the client is asking.
///
/// The arguments are references and are cloned into the closure, following the other `register_*`
/// functions: the host keeps ownership of both seams and can clone the registry afterwards.
pub fn register_session_queue_methods(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    queues: &Arc<dyn QueueAgentSource>,
) {
    let catalog = Arc::clone(catalog);
    let queues = Arc::clone(queues);

    // Declared before the handler, so the policy judges a call the handler would otherwise accept.
    // A mutation of a user's pending prompt is a write to that session, not a read of it.
    registry.declare(NAMESPACE, METHOD, Declaration::write(RESOURCE_KIND));

    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let catalog = Arc::clone(&catalog);
        let queues = Arc::clone(&queues);
        Box::pin(async move { update_queue(&call.args, &catalog, &queues).await })
    });
}

/// Runs one `session.updateQueue` call.
async fn update_queue(
    args: &Value,
    catalog: &Arc<dyn SessionCatalog>,
    queues: &Arc<dyn QueueAgentSource>,
) -> Result<Value, RemoteError> {
    let request: UpdateQueueRequest =
        serde_json::from_value(args.clone()).map_err(|error| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: METHOD.to_owned(),
            message: error.to_string(),
        })?;

    if request.session_id.trim().is_empty() {
        return Err(QueueError::MissingSessionId.into_remote());
    }
    if request.item_id.trim().is_empty() {
        return Err(QueueError::MissingItemId.into_remote());
    }

    // Content is validated before anything is looked up, matching upstream's order: a request that
    // could never be applied should not first spend a log read deciding where it would go.
    if let QueueAction::Edit { content } = &request.action {
        validate_edit(content).map_err(QueueError::into_remote)?;
    }

    let Some(agent) = queues.agent(&request.session_id) else {
        // Two different answers, because they ask for different things from the caller. An unknown
        // session is a bad id; a known session with no agent is this build's inability to keep a
        // pending queue across a restart, and reporting it as "unknown session" would send the caller
        // looking for a typo that is not there.
        let known = catalog.read(&request.session_id).await.is_some();
        return Err(if known {
            QueueError::NoLiveAgent {
                session_id: request.session_id,
            }
        } else {
            QueueError::UnknownSession {
                session_id: request.session_id,
            }
        }
        .into_remote());
    };

    let pending = agent.inbox_state();
    let Some((target, index)) = locate(&pending, &request.item_id) else {
        // Not in a queue: either it was never there or the loop has already taken it. The log is the
        // only witness to the second, and the two cases differ for the user — one is a stale UI, the
        // other is a message they can no longer change.
        let started = started(catalog, &request.session_id, &request.item_id).await;
        return Err(if started {
            QueueError::AlreadyStarted {
                item_id: request.item_id,
            }
        } else {
            QueueError::NotPending {
                item_id: request.item_id,
            }
        }
        .into_remote());
    };

    match request.action {
        QueueAction::Edit { content } => {
            // The item's identity, role, and provenance are kept; only the blocks change. Upstream
            // spreads the original message for the same reason: an edit rewords what the user is
            // about to say, it does not turn their message into a different producer's.
            let Some(original) = queue_of(&pending, target).get(index) else {
                // The scan and this lookup read the same snapshot, so an absent entry here would be a
                // defect in `locate` rather than a race against another writer.
                return Err(QueueError::NotPending {
                    item_id: request.item_id,
                }
                .into_remote());
            };
            let replacement = ChatMessage::new(
                original.id.clone(),
                original.role,
                original.source.clone(),
                content,
            );
            replace_pending(&agent, target, index, &request.item_id, Some(replacement))
                .map_err(QueueError::into_remote)?;
        }
        QueueAction::Remove => {
            replace_pending(&agent, target, index, &request.item_id, None)
                .map_err(QueueError::into_remote)?;
        }
        QueueAction::Steer => return Err(QueueError::SteerUnavailable.into_remote()),
    }

    // Upstream's receipt: an acknowledgement, not a copy of the queue. A client that rendered the
    // changed item would be rendering the content it just sent, and one that needed the queue would
    // read it through the session projection instead of inferring it from this reply.
    Ok(json!({ "accepted": true }))
}

/// Enforces upstream's content rules for an edit.
///
/// Text-only, and at least one non-whitespace character. Both rules exist for the same reason: the
/// queue holds *prompts*, and a prompt made of an image reference or of spaces is not something the
/// model can be asked to act on — upstream rejects the same two cases.
fn validate_edit(content: &[ContentBlock]) -> Result<(), QueueError> {
    if let Some(block) = content
        .iter()
        .find(|block| !matches!(block, ContentBlock::Text { .. }))
    {
        return Err(QueueError::EditNotText {
            kind: block.type_name(),
        });
    }
    if !content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text { text } if !text.trim().is_empty()))
    {
        return Err(QueueError::EditBlank);
    }
    Ok(())
}

/// Finds `item_id` in a queue snapshot.
///
/// Next-turn is searched first, matching upstream. Order is otherwise immaterial: [`Inbox`]'s
/// invariant forbids one id appearing in both queues, so at most one branch can match.
fn locate(inbox: &Inbox, item_id: &str) -> Option<(InboxTarget, usize)> {
    [InboxTarget::NextTurn, InboxTarget::NextStep]
        .into_iter()
        .find_map(|target| {
            queue_of(inbox, target)
                .iter()
                .position(|message| message.id.as_str() == item_id)
                .map(|index| (target, index))
        })
}

/// One queue of a snapshot.
///
/// [`Inbox`] exposes the queues as slices and nothing else, so a target has to be turned into a slice
/// before it can be indexed. Kept in one place so `locate` and the mutation cannot disagree about
/// which slice a target names.
fn queue_of(inbox: &Inbox, target: InboxTarget) -> &[ChatMessage] {
    match target {
        InboxTarget::NextTurn => inbox.next_turn(),
        InboxTarget::NextStep => inbox.next_step(),
    }
}

/// Whether the durable log shows the item was already consumed.
///
/// A consumed pending message is appended as a user message when the loop picks it up, so its id
/// appearing in the log is the evidence that it has begun executing. A read failure is reported as
/// "not started": the alternative would claim the message was already seen on the strength of a log
/// that could not be read, and the caller's next action differs between the two.
async fn started(catalog: &Arc<dyn SessionCatalog>, session_id: &str, item_id: &str) -> bool {
    let Some(record) = catalog.read(session_id).await else {
        return false;
    };
    record.events.iter().any(|event| match event {
        SessionEvent::UserMessage { message } => message.id.as_str() == item_id,
        _ => false,
    })
}

/// Replaces or removes one entry, and proves it changed the entry that was asked for.
///
/// Two things happen here that [`Inbox::apply`] cannot do for this caller:
///
/// - **Identity is verified.** The splice names a position; this module promised the client an id. If
///   another writer drained the queue between the scan and the splice, the position could address a
///   different message. So the state returned by [`Agent::splice_inbox`] — the inbox as it was before
///   the splice — is checked at that index, and a mismatch is undone with the inverse splice and
///   reported as "not pending". Undo is best effort by nature; if even that fails the error says so,
///   because a queue left in a state this layer did not choose must not be reported as success.
/// - **The mutation is one splice**, so it is atomic with respect to the inbox's own validation
///   (range and duplicate checks) and a refused mutation leaves the queue untouched.
///
/// `replacement: None` removes; `Some` replaces in place.
fn replace_pending(
    agent: &Agent,
    target: InboxTarget,
    index: usize,
    item_id: &str,
    replacement: Option<ChatMessage>,
) -> Result<(), QueueError> {
    let inserted: Vec<ChatMessage> = replacement.into_iter().collect();
    let removed = 1;
    let splice = Splice {
        target,
        start: index,
        removed,
        inserted: inserted.clone(),
    };

    let previous = agent
        .splice_inbox(&splice)
        .map_err(|error| QueueError::Inbox {
            reason: error.to_string(),
        })?;

    let replaced = queue_of(&previous, target).get(index);
    if replaced.is_some_and(|message| message.id.as_str() == item_id) {
        return Ok(());
    }

    // Undo with the exact entries that were there. `previous` is the whole queue as it stood, so the
    // inverse is "remove what was inserted, put back what was displaced".
    let restore = Splice {
        target,
        start: index,
        removed: inserted.len(),
        inserted: queue_of(&previous, target)
            .get(index)
            .cloned()
            .into_iter()
            .collect(),
    };
    if let Err(error) = agent.splice_inbox(&restore) {
        return Err(QueueError::Inbox {
            reason: format!(
                "`{item_id}` was not at the located position and the change could not be undone: {error}"
            ),
        });
    }

    Err(QueueError::NotPending {
        item_id: item_id.to_owned(),
    })
}

/// Renders an [`InboxError`] for a caller that only has its text.
///
/// Kept so the `From` conversion used by tests and diagnostics cannot drift from what
/// [`replace_pending`] reports: both spell the queue layer's reason the same way.
impl From<InboxError> for QueueError {
    fn from(error: InboxError) -> Self {
        Self::Inbox {
            reason: error.to_string(),
        }
    }
}

/// Re-exported so a host or a test can name the access kind this method declares without importing
/// `girder_core::Access` and guessing which one applies.
///
/// The value is a write, and it is public because a deployment's own policy may want to compare
/// against it rather than re-derive it from the registration.
pub const DECLARED_ACCESS: Access = Access::Write;
