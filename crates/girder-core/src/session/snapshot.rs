//! The snapshot layer of upstream `packages/api/session-controller`: the two observable values a
//! client UI subscribes to, and the state machines that build them.
//!
//! # Where this comes from
//!
//! Upstream splits this capability across two large files plus two small ones:
//!
//! - `src/client/sessions/session.ts` (1,055 lines) — one client Session: its event window, its
//!   lifecycle bits, its local prompt echoes, and `buildSnapshot`/`subscribe`/`getSnapshot`.
//! - `src/client/sessions/manager.ts` (857 lines) — the instance cluster and the session list:
//!   baseline/delta control-frame dispatch, the engagement ledger, the list mutation log, the
//!   single-flighted list pull, and `buildListSnapshot`.
//! - `src/client/contract/snapshot.ts` (94 lines) — `SessionSnapshot`'s field set.
//! - `src/client/ordered-baseline.ts` (43 lines) — the reference-order merge, already ported as
//!   [`girder_core::session::controller::merge_ordered_baseline`]; this module calls it.
//!
//! Two sibling modules already ported the pieces these files sit on: `session_controller_gap` has
//! the history cut, the cursor algebra, and the lineage flattening; `session_engine_gap` has
//! [`ProjectionValueStore`] and [`Notifier`]. Neither had a snapshot builder, so nothing in this
//! crate could answer "what does the client render right now". This module adds that, and it
//! re-implements neither: every projection value goes through the existing store, every publication
//! through the existing notifier, every ordering decision through the existing baseline merge and
//! lineage walk.
//!
//! # The two snapshots, and why they are separate objects
//!
//! [`SessionSnapshot`] is small, hot, and per session: a UI framework's external-store binding calls
//! `snapshot()` on every render of every component watching that session. [`ListSnapshot`] is cold
//! and wide: it holds every row, every row's lineage depth, and every attached session's projection
//! values. Merging them would either make the list rebuild on every running-bit flip of one session,
//! or make per-session reads pay for a lineage walk. Upstream keeps them separate and this module
//! follows: two caches, two notifiers, no shared write path.
//!
//! # Rules that are load-bearing, and why
//!
//! - **A snapshot is rebuilt lazily, and only once per batch.** Writes mark dirty; a read while
//!   dirty rebuilds synchronously; a publication rebuilds before notifying. Without this a frame
//!   storm (a control baseline for two hundred sessions) would rebuild two hundred snapshots for a
//!   repaint that never happened. [`Notifier`] already encodes the batching; this module supplies
//!   the rebuild closure.
//! - **Blank only lowers with help.** [`SessionView::handle_blank`] refuses to re-blank a session
//!   that has a prompt attempt or is currently running: the list's `blank` is a durability question
//!   ("has a turn ever started"), while the composer's is a display question, and a stale list row
//!   must not re-hide a conversation the user is looking at.
//! - **Running clears blank; a stopped turn does not restore it.** A blank session that starts
//!   running is no longer blank — it has a turn. A non-blank session that stops running does not
//!   become blank, because its first prompt is still durably there.
//! - **`awaiting_first_turn` is two flags, not one.** A session is engaging only between the
//!   accepted prompt and the first durable `turn/start`; observing running clears the flag even if
//!   the turn event never arrives, because the Host admitted the prompt and moved on.
//! - **Engagement outlives instantiation.** The cluster remembers which identities were accepted or
//!   seen running, across Session object replacement and across list rows disappearing, because a
//!   prompt can be accepted by a Session object the cluster has already replaced. Forgetting early
//!   re-blanks a conversation mid-turn.
//! - **A pull's completion is merged with, not overwritten by, the mutations that arrived during
//!   it.** A list response is a baseline snapshot of the Host's ledger taken before the frames that
//!   landed while it was in flight. Taking it as truth would undo an addition, a rename, or a
//!   removal the user just watched happen — so the established order is preserved and every
//!   recorded mutation is replayed on top.
//! - **`phase` is monotone; `state` is the failure axis.** `pending → ready` fires on the first
//!   successful pull and never steps back, so an empty row set means "nothing arrived" until a pull
//!   proves otherwise. Pull *failures* land in `state`/`error`, which is a different question and
//!   does not imply the client has no rows.
//! - **An unchanged row set reuses its allocation.** Every rebuild re-derives every row; when the
//!   derived rows are equal to the previous rebuild's, the previous `Arc<Vec<ListEntry>>` is handed
//!   back unchanged. That is what keeps a memoised row component from re-rendering on every Host
//!   frame — in Rust the row *is* the shared object, so reuse of the array is reuse of the rows.
//! - **A control baseline is a consistent cut; a per-event frame is not.** The baseline seeds every
//!   key and clears what it omits; a frame carries only the keys that changed and clears nothing.
//!   Confusing the two either resurrects removed capabilities or drops live ones.
//! - **A cached list block never outranks a sequenced row.** The list may carry a zero-I/O view of
//!   the persisted checkpoint for sessions nobody opened. Its watermark is not comparable with this
//!   connection's sequence space, so it only fills keys no sequenced row holds.
//! - **A prompt echo is published only after the Host accepted it.** The echo exists so the user
//!   sees their message before durable admission; publishing it earlier would show a message the
//!   Host rejected, and retiring it before acceptance would make a failed send vanish silently.
//!
//! # What this module deliberately does not do
//!
//! - **No transport.** Every method takes the data a Remote call would have returned. The
//!   single-flight list pull is modelled as [`ListEngine::begin_list_refresh`] plus
//!   [`ListEngine::complete_list_refresh`] around a caller-owned future, and a projection read as a
//!   ticket plus its outcome, which is what makes the fencing testable without a socket.
//! - **No inbox correlation.** Upstream settles a prompt echo against the `inbox` projection and
//!   durable `user/message` / `agent/inbox/spliced` events. That reconciliation is the largest
//!   remaining piece of `session.ts` and is not in this module; the echo ledger here publishes on
//!   acceptance and retires on an explicit observation, and every entry point names the upstream
//!   call it corresponds to.
//! - **No event window.** The window's pages, its prepend/replace algebra, and the assistant-stream
//!   folding live in `contract/events.ts` and `sessions/assistant-stream.ts`; this module keeps only
//!   the *snapshot bits* that window ownership implies (`has_more`, `history_was_partial`,
//!   `history_load_status`, the first-turn latch).
//! - **No React bindings.** A subscriber is a `Fn()`; the framework calls it.
//!
//! # Module-include note
//!
//! Both this file and its test are included by path from `tests/session_snapshot_gap.rs`, so every
//! reference to the library goes through `girder_core::…` rather than `crate::…`: inside a
//! `#[path]` module, `crate::` names the *test* crate.

// The crate root has not been wired to this module yet, so in a library build nothing here has a
// caller and `dead_code` would report a whole public surface as waste. The allow says that plainly
// rather than leaving a reader to guess; the test target exercises every item below.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use serde_json::Value;

use girder_core::api::gateway_client::RemoteFailure;
use girder_core::session::controller::{LineageRow, flatten_lineage, merge_ordered_baseline};
use girder_core::{
    Notifier, ProjectionListenerId as ListenerId, ProjectionValueStore, Publish, SeqCursor,
};

/// Locks a mutex, treating poisoning as a stale writer rather than a fatal state.
///
/// A poisoned snapshot mutex means some thread panicked mid-write. The value it guards is a set of
/// booleans plus a cached snapshot: the next writer overwrites all of it, and a reader would rather
/// render the last consistent snapshot than abort the process. This is the choice
/// [`girder_core::ProjectionValueStore`] already makes for its own rows.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Shared wire bits
// ---------------------------------------------------------------------------------------------

/// The watermark a projection block is consistent with.
///
/// Upstream's `SessionSeqCursor` is a branded `number` whose only unusual value is `-1`, which the
/// transport uses for "this cut is before the first event" (an empty log). `u64` cannot carry that
/// sentinel, and folding it into `0` would be wrong in one direction: an event at seq `0` is a real
/// event, so a value seeded at `-1` must yield to one seeded at `0`, while two values seeded at `0`
/// must tie. The variant keeps the distinction explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watermark {
    /// A real cut: the block is consistent with this event position.
    At(SeqCursor),
    /// Upstream's `-1`: the block describes a log with no events yet.
    BeforeFirstEvent,
}

impl Watermark {
    /// Reads one wire watermark, refusing values upstream cannot produce.
    ///
    /// `-1` is the empty-cut sentinel; every non-negative integer is a real cut; anything below is
    /// rejected rather than clamped, because a silently clamped watermark would make a corrupt frame
    /// look valid and lose whichever value it carried.
    #[must_use]
    pub fn from_wire(value: i64) -> Option<Self> {
        if value == -1 {
            return Some(Self::BeforeFirstEvent);
        }
        u64::try_from(value).ok().map(Self::At)
    }

    /// The comparable sequence number, absent for the empty-cut sentinel.
    #[must_use]
    pub fn seq(self) -> Option<SeqCursor> {
        match self {
            Self::At(seq) => Some(seq),
            Self::BeforeFirstEvent => None,
        }
    }
}

/// Seeds one projection store from a baseline block, returning the keys that lost their value.
///
/// For a real cut this is exactly [`ProjectionValueStore::seed`]: sweep cached rows, apply the
/// carried keys under higher-seq-wins, then clear every key the cut omits unless a newer frame
/// superseded the cut. That entire precedence rule already lives in the store, and duplicating it
/// here is the two-sources-of-truth bug this port exists to avoid.
///
/// The empty-cut sentinel needs one different step, because no `u64` watermark sits below a real
/// event. Upstream seeds those keys at `-1`, so any real frame outranks them; the faithful modelling
/// available through the store's public API is to seed them as *cached* rows, which also yield to
/// every sequenced row. One consequence is documented rather than hidden: a later cached **list**
/// block could overwrite a sentinel-seeded key, which upstream would resist. That needs an attached
/// session with an empty log to also be listed header-only from the persisted checkpoint, a
/// combination the wire protocol does not produce.
pub fn seed_baseline(
    store: &ProjectionValueStore,
    watermark: Watermark,
    values: &BTreeMap<String, Value>,
) -> Vec<String> {
    match watermark {
        Watermark::At(seq) => store.seed(seq, values),
        Watermark::BeforeFirstEvent => seed_before_first_event(store, values),
    }
}

/// The empty-cut branch of [`seed_baseline`].
fn seed_before_first_event(
    store: &ProjectionValueStore,
    values: &BTreeMap<String, Value>,
) -> Vec<String> {
    // Upstream sweeps every cached row first, whatever the cut. Sequenced rows survive: their seq is
    // above the sentinel.
    let current = store.values();
    let sequenced: Vec<(String, Value, SeqCursor)> = current
        .keys()
        .filter_map(|key| {
            let seq = store.seq_of(key)?;
            Some((key.clone(), current.get(key)?.clone(), seq))
        })
        .collect();
    let mut swept: Vec<String> = current
        .keys()
        .filter(|key| store.is_cached(key))
        .cloned()
        .collect();
    if !swept.is_empty() {
        store.clear();
        for (key, value, seq) in sequenced {
            store.apply(&key, value, seq);
        }
    }

    // Carried keys land only where no sequenced row holds them: the store's own cached rule.
    let absent: BTreeMap<String, Value> = values
        .iter()
        .filter(|(key, _)| store.seq_of(key).is_none())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    store.apply_cached(&absent);

    // A carried key was re-seeded, so reporting it as removed would make a caller drop it.
    swept.retain(|key| !values.contains_key(key));
    swept.sort_unstable();
    swept.dedup();
    swept
}

// ---------------------------------------------------------------------------------------------
// Session snapshot
// ---------------------------------------------------------------------------------------------

/// History-open lifecycle of one session's event window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpenState {
    /// Nothing has been requested yet.
    #[default]
    Cold,
    /// The first page is in flight.
    Loading,
    /// A window is installed.
    Open,
    /// The open failed; `open_error` explains it.
    Error,
}

/// Status of the history requests behind the event window.
///
/// `pending-consumption` exists because a page can arrive while the window is being replaced: it
/// means "a page is held, not yet published". It is deliberately not a loading state — the spinner
/// must stop — which is why it is not folded into [`Self::Loading`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryLoadStatus {
    /// No request outstanding.
    #[default]
    Idle,
    /// An ordinary older-page or jump request is in flight.
    Loading,
    /// A page landed while another consumption owns the window.
    PendingConsumption,
    /// The last request failed.
    Error,
}

/// Which verb produced a [`PromptError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptOp {
    /// A prompt submission.
    Send,
    /// A cancel request.
    Stop,
}

/// A send or stop failure surfaced through the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptError {
    /// The verb that failed.
    pub op: PromptOp,
    /// The Host or transport failure.
    pub failure: RemoteFailure,
}

/// A subagent child's browsing mode.
///
/// `unknown` is a real value, not a missing one: a listed child header may omit its mode, and the
/// address is then re-typed from the child's own `subagent` projection once that is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentMode {
    /// A child that answers once.
    OneShot,
    /// A child that can be prompted again.
    Continuable,
    /// Mode not yet resolved.
    Unknown,
}

impl SubagentMode {
    /// Reads the wire spelling, absent for a value this build does not know.
    ///
    /// An unrecognized mode becomes "absent" rather than an error: the address is still usable for
    /// navigation, and refusing it would hide a child the Host listed.
    #[must_use]
    pub fn from_wire(value: Option<&str>) -> Option<Self> {
        match value? {
            "one-shot" => Some(Self::OneShot),
            "continuable" => Some(Self::Continuable),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneShot => "one-shot",
            Self::Continuable => "continuable",
            Self::Unknown => "unknown",
        }
    }
}

/// A durable parent/child browsing address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentAddress {
    /// The parent session.
    pub parent_session_id: String,
    /// The child session.
    pub child_session_id: String,
    /// The child's mode as far as the client knows it.
    pub mode: SubagentMode,
}

/// Where a local prompt echo is displayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The transcript: an idle session accepts the prompt as the next turn.
    Transcript,
    /// The queue dock: a running session appends it to pending work.
    Queued,
    /// The steering slot: a running session interrupts its current step.
    Steering,
}

/// One image a submission echo displays before durable admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingImage {
    /// Browser-owned preview URL; the echo never owns its lifecycle.
    pub preview_url: String,
    /// Display name, when the file had one.
    pub name: Option<String>,
    /// Intrinsic pixel width, when the submitter probed it.
    pub width: Option<u32>,
    /// Intrinsic pixel height, when the submitter probed it.
    pub height: Option<u32>,
}

/// One attachment of a submission echo, in prompt order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingAttachment {
    /// A browser-local image preview.
    Image(PendingImage),
    /// A durable file reference.
    File {
        /// Content-addressed storage id.
        attachment_id: String,
        /// Sanitized display name.
        name: String,
        /// Exact byte length.
        bytes: u64,
    },
}

/// One local prompt echo.
///
/// Client memory only: a reload or a reconnect rebuilds the conversation from durable events, so
/// this type never needs a wire form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSubmission {
    /// The prompt RPC identity, echoed back by the durable `user/message` as `rpcId`.
    pub request_id: String,
    /// Surface fixed at submission time.
    pub placement: Placement,
    /// Client wall-clock milliseconds when the submission began.
    pub time_ms: u64,
    /// Prompt text exactly as it will be sent.
    pub text: String,
    /// Ordered attachments matching the prompt's.
    pub attachments: Vec<PendingAttachment>,
}

/// One client session's immutable, observable state.
///
/// Upstream's `SessionSnapshot`, minus the fields the event stream owns and with the notification
/// callbacks lifted out of the value: a Rust snapshot is a value a subscriber reads, not a pair of
/// handles. The flags are a bag because the composer, the history pane, the error strip, and the
/// title bar each read a different one; grouping them into sub-structs would make every consumer
/// destructure state it does not use.
#[allow(clippy::struct_excessive_bools)] // One flag per independent surface (see above).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// The session identity.
    pub session_id: String,
    /// Accepted echoes not yet observed as durable.
    pub pending_submissions: Vec<PendingSubmission>,
    /// Whether an Agent for this session is running a turn.
    pub running: bool,
    /// The subagent address, once known.
    pub subagent: Option<SubagentAddress>,
    /// Whether the Host reported the session removed.
    pub removed: bool,
    /// History-open lifecycle.
    pub open_state: OpenState,
    /// Why the open failed, when it did.
    pub open_error: Option<RemoteFailure>,
    /// Whether older history remains below the window.
    pub has_more: bool,
    /// Whether this client ever exposed an incomplete window; sticky across replacements.
    pub history_was_partial: bool,
    /// Whether a history request is in flight, including a turn jump.
    pub loading_older: bool,
    /// The status behind `loading_older`.
    pub history_load_status: HistoryLoadStatus,
    /// The last send/stop failure, cleared by the next attempt.
    pub prompt_error: Option<PromptError>,
    /// Whether the session has no turn yet and is still reusable.
    pub blank: bool,
    /// The last live Agent failure carrying no turn position.
    pub last_agent_error: Option<String>,
    /// Whether a prompt call began on this object; sticky.
    pub prompt_attempted: bool,
    /// Whether an accepted first prompt has not yet reached a durable `turn/start`.
    pub awaiting_first_turn: bool,
}

/// One echo's settlement state.
#[derive(Clone, Copy, Debug)]
struct Settlement {
    accepted: bool,
    admitted: bool,
    retiring: bool,
}

/// The mutable half of one session's snapshot, guarded as a unit.
///
/// The booleans are independent facts the composer, the history pane, and the error strip each
/// read: a running session can still be engaging, a removed one still has an open error. They are
/// not exclusive states that could collapse into one enum, and grouping them would make every
/// accessor take a sub-struct it does not use.
#[allow(clippy::struct_excessive_bools)] // See above: independent bits, not exclusive states.
#[derive(Debug)]
struct ViewState {
    running: bool,
    address: Option<SubagentAddress>,
    removed: bool,
    open_state: OpenState,
    open_error: Option<RemoteFailure>,
    has_more: bool,
    history_was_partial: bool,
    history_load_status: HistoryLoadStatus,
    prompt_error: Option<PromptError>,
    blank: bool,
    last_agent_error: Option<String>,
    prompt_attempted: bool,
    first_prompt_pending_turn: bool,
    /// Every registered echo, insertion-ordered.
    echoes: Vec<PendingSubmission>,
    /// Echo identities whose settlement is still tracked; absent means retired.
    settlements: BTreeMap<String, Settlement>,
    /// The cached snapshot the notifier rebuilds and readers clone.
    cache: SessionSnapshot,
}

/// The outcome of [`SessionView::configure_subagent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigureOutcome {
    /// Whether the address actually changed.
    pub changed: bool,
    /// Whether an already-open window must be rebuilt through the new route.
    pub resync: bool,
}

/// The snapshot half of one client Session.
///
/// Upstream keeps this state on the `Session` class next to the event stream. This type is that
/// state plus the observable cache, so a caller can wire it to a stream (or to a test) without
/// inheriting the stream's I/O. The event window itself is outside this module: its pages, its
/// prepend/replace algebra, and its assistant-stream folding are a different capability.
pub struct SessionView {
    session_id: String,
    state: Arc<Mutex<ViewState>>,
    projections: Arc<ProjectionValueStore>,
    notifier: Arc<Notifier>,
}

impl std::fmt::Debug for SessionView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.state);
        formatter
            .debug_struct("SessionView")
            .field("session_id", &self.session_id)
            .field("running", &state.running)
            .field("blank", &state.blank)
            .field("removed", &state.removed)
            .field("open_state", &state.open_state)
            .field("echoes", &state.echoes.len())
            .finish_non_exhaustive()
    }
}

impl SessionView {
    /// Builds the snapshot state for one session.
    ///
    /// The projection store is passed in rather than created here because upstream's manager owns
    /// it: a store outlives the object, so control frames land before the session is instantiated
    /// and a list row reads a cold title without opening anything. A caller without a manager passes
    /// a fresh [`ProjectionValueStore`].
    #[must_use]
    pub fn new(session_id: impl Into<String>, projections: Arc<ProjectionValueStore>) -> Self {
        let session_id = session_id.into();
        let initial = SessionSnapshot {
            session_id: session_id.clone(),
            pending_submissions: Vec::new(),
            running: false,
            subagent: None,
            removed: false,
            open_state: OpenState::Cold,
            open_error: None,
            has_more: false,
            history_was_partial: false,
            loading_older: false,
            history_load_status: HistoryLoadStatus::Idle,
            prompt_error: None,
            blank: true,
            last_agent_error: None,
            prompt_attempted: false,
            awaiting_first_turn: false,
        };
        let state = Arc::new(Mutex::new(ViewState {
            running: false,
            address: None,
            removed: false,
            open_state: OpenState::Cold,
            open_error: None,
            has_more: false,
            history_was_partial: false,
            history_load_status: HistoryLoadStatus::Idle,
            prompt_error: None,
            blank: true,
            last_agent_error: None,
            prompt_attempted: false,
            first_prompt_pending_turn: false,
            echoes: Vec::new(),
            settlements: BTreeMap::new(),
            cache: initial,
        }));
        let rebuild_id = session_id.clone();
        let rebuild_state = Arc::clone(&state);
        let rebuild_projections = Arc::clone(&projections);
        let notifier = Arc::new(Notifier::new(move || {
            let mut guarded = lock(&rebuild_state);
            guarded.cache = build_session_snapshot(&rebuild_id, &guarded, &rebuild_projections);
        }));
        Self {
            session_id,
            state,
            projections,
            notifier,
        }
    }

    /// The session identity.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The projection store this session reads.
    #[must_use]
    pub fn projections(&self) -> Arc<ProjectionValueStore> {
        Arc::clone(&self.projections)
    }

    /// Subscribes to snapshot changes.
    ///
    /// The token unsubscribes, exactly as upstream's returned function does; it stays valid after
    /// the session is dropped, where unsubscribing is a no-op.
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> ListenerId {
        self.notifier.subscribe(listener)
    }

    /// Removes one subscription.
    pub fn unsubscribe(&self, id: ListenerId) {
        self.notifier.unsubscribe(id);
    }

    /// How many listeners are subscribed.
    #[must_use]
    pub fn listener_count(&self) -> usize {
        self.notifier.listener_count()
    }

    /// Delivers a pending publication.
    ///
    /// A caller with a render loop calls this from its frame callback; one without calls it at the
    /// end of the task that wrote. Public because "three writes, one publication" is the property
    /// batching exists for, and it cannot be asserted from outside otherwise.
    pub fn flush(&self) -> Publish {
        self.notifier.flush()
    }

    /// The current snapshot, rebuilt first when dirty.
    #[must_use]
    pub fn snapshot(&self) -> SessionSnapshot {
        self.notifier.ensure_fresh();
        lock(&self.state).cache.clone()
    }

    /// Records an Agent running-state change.
    ///
    /// Running clears blank (a started turn is not a blank session) and retires the "awaiting first
    /// turn" flag: once the Agent runs, the engaging phase is over whether or not the durable
    /// `turn/start` has been read yet.
    pub fn handle_running(&self, running: bool) -> bool {
        let changed = {
            let mut state = lock(&self.state);
            let mut changed = false;
            if running && state.blank {
                state.blank = false;
                changed = true;
            }
            if running {
                state.first_prompt_pending_turn = false;
            }
            if state.running != running {
                state.running = running;
                changed = true;
            }
            changed
        };
        self.mark(changed);
        changed
    }

    /// Applies the manager's display blank, reconciled with the list-metadata projection.
    ///
    /// Two refusals matter. A projection that says `blank: false` wins over a stale list row,
    /// because the projection is the Host's own metadata and the row may predate it. And a session
    /// with a prompt attempt or a running Agent cannot be re-blanked: the list answers "is there
    /// durable history", while the composer is asking "should the user keep looking at this", and a
    /// stale row must not hide a conversation that is happening.
    pub fn handle_blank(&self, blank: bool) -> bool {
        let metadata = metadata_of(&self.projections).0;
        let blank = blank && metadata != Some(false);
        let changed = {
            let mut state = lock(&self.state);
            if blank == state.blank {
                return false;
            }
            if blank && (state.prompt_attempted || state.running) {
                return false;
            }
            state.blank = blank;
            true
        };
        self.mark(changed);
        changed
    }

    /// Records that the Host reported the session removed.
    ///
    /// The resident instance is kept: its stream still has to be torn down by its owner, and the
    /// snapshot is the only place that fact is visible.
    pub fn handle_removed(&self) -> bool {
        let changed = {
            let mut state = lock(&self.state);
            if state.removed {
                false
            } else {
                state.removed = true;
                true
            }
        };
        self.mark(changed);
        changed
    }

    /// Records a live Agent failure carrying no turn position.
    pub fn handle_agent_error(&self, message: impl Into<String>) -> bool {
        lock(&self.state).last_agent_error = Some(message.into());
        self.mark(true);
        true
    }

    /// Installs or clears the catalog-discovered subagent address.
    ///
    /// A changed address invalidates an already-open window: the same identity read through a
    /// different route is a different log. A cold window is not rebuilt, because there is nothing to
    /// rebuild.
    pub fn configure_subagent(&self, address: Option<SubagentAddress>) -> ConfigureOutcome {
        let (changed, resync) = {
            let mut state = lock(&self.state);
            let changed = state.address != address;
            state.address = address;
            (changed, changed && state.open_state != OpenState::Cold)
        };
        self.mark(changed);
        ConfigureOutcome { changed, resync }
    }

    /// Starts a first open: the window is loading and any previous error is cleared.
    pub fn begin_open(&self) -> bool {
        {
            let mut state = lock(&self.state);
            state.open_state = OpenState::Loading;
            state.open_error = None;
        }
        self.mark(true);
        true
    }

    /// Records a successful open.
    pub fn opened(&self) -> bool {
        let changed = {
            let mut state = lock(&self.state);
            if state.open_state == OpenState::Open {
                false
            } else {
                state.open_state = OpenState::Open;
                state.open_error = None;
                true
            }
        };
        self.mark(changed);
        changed
    }

    /// Records a failed open, keeping the failure for the error surface.
    pub fn open_failed(&self, failure: RemoteFailure) -> bool {
        {
            let mut state = lock(&self.state);
            state.open_state = OpenState::Error;
            state.open_error = Some(failure);
        }
        self.mark(true);
        true
    }

    /// Sets the history-request status behind `loading_older`.
    pub fn set_history_load_status(&self, status: HistoryLoadStatus) -> bool {
        let changed = {
            let mut state = lock(&self.state);
            if state.history_load_status == status {
                false
            } else {
                state.history_load_status = status;
                true
            }
        };
        self.mark(changed);
        changed
    }

    /// Records a replacement window.
    ///
    /// `has_turn_start` is whether the installed window contains a durable `turn/start`: a first
    /// prompt observed there is no longer awaiting anything, even if running was never reported.
    pub fn note_window_replaced(&self, has_more: bool, has_turn_start: bool) -> bool {
        {
            let mut state = lock(&self.state);
            state.has_more = has_more;
            state.history_was_partial |= has_more;
            if has_turn_start {
                state.first_prompt_pending_turn = false;
            }
        }
        self.mark(true);
        true
    }

    /// Records a prepended older page.
    pub fn note_window_prepended(&self, has_more: bool) -> bool {
        {
            let mut state = lock(&self.state);
            state.has_more = has_more;
            state.history_was_partial |= has_more;
        }
        self.mark(true);
        true
    }

    /// Begins a prompt attempt: the error surfaces clear and the engaging phase may start.
    ///
    /// Upstream does this before its first `await`, so a `turn/start` observed ahead of the receipt
    /// can clear the engaging flag without racing the send path.
    pub fn begin_prompt(&self) -> bool {
        {
            let mut state = lock(&self.state);
            state.prompt_error = None;
            state.last_agent_error = None;
            if state.blank && !state.first_prompt_pending_turn {
                state.first_prompt_pending_turn = true;
            }
        }
        self.mark(true);
        true
    }

    /// Applies the Host's acceptance of one prompt (upstream `acceptPrompt`).
    ///
    /// Acceptance is what publishes an echo and moves a blank session to engaging. The settlement
    /// latch makes the second proof — a receipt and an inbox row, whichever lands second — a no-op.
    /// Returns whether this call changed anything.
    pub fn accept_prompt(&self, request_id: Option<&str>) -> bool {
        let changed = {
            let mut state = lock(&self.state);
            let mut changed = false;
            if let Some(id) = request_id
                && let Some(settlement) = state.settlements.get_mut(id)
                && !settlement.accepted
            {
                settlement.accepted = true;
                changed = true;
            }
            state.prompt_attempted = true;
            // Direct assignment on purpose: acceptance is the one write that clears blank without
            // consulting the prompt-attempt guard, because it is the Host's own verdict.
            if state.blank {
                state.blank = false;
                changed = true;
            }
            changed
        };
        self.mark(changed);
        changed
    }

    /// Records a rejected or failed prompt: the error surfaces, and the engaging phase ends.
    ///
    /// A first prompt that never reached admission leaves the session blank and reusable, which is
    /// exactly why the engaging flag is a separate bit from the blank bit.
    pub fn reject_prompt(&self, failure: RemoteFailure) -> bool {
        {
            let mut state = lock(&self.state);
            state.first_prompt_pending_turn = false;
            state.prompt_error = Some(PromptError {
                op: PromptOp::Send,
                failure,
            });
        }
        self.mark(true);
        true
    }

    /// Records a failed cancel request, which shares the error strip's slot.
    pub fn fail_stop(&self, failure: RemoteFailure) -> bool {
        {
            let mut state = lock(&self.state);
            state.prompt_error = Some(PromptError {
                op: PromptOp::Stop,
                failure,
            });
        }
        self.mark(true);
        true
    }

    /// Registers one local prompt echo (upstream `beginSubmission`) and returns its placement.
    ///
    /// The placement is derived once, at submission time: a later inbox acceptance must not move a
    /// transcript echo into the queue dock, because the user already saw it where it was.
    pub fn begin_submission(
        &self,
        request_id: impl Into<String>,
        mode: PromptOp,
        text: impl Into<String>,
        attachments: Vec<PendingAttachment>,
        now_ms: u64,
    ) -> Placement {
        let request_id = request_id.into();
        let placement = {
            let mut state = lock(&self.state);
            let placement = if state.running {
                if mode == PromptOp::Stop {
                    Placement::Steering
                } else {
                    Placement::Queued
                }
            } else {
                Placement::Transcript
            };
            state.echoes.push(PendingSubmission {
                request_id: request_id.clone(),
                placement,
                time_ms: now_ms,
                text: text.into(),
                attachments,
            });
            state.settlements.insert(
                request_id,
                Settlement {
                    accepted: false,
                    admitted: false,
                    retiring: false,
                },
            );
            placement
        };
        self.mark(true);
        placement
    }

    /// Records that one echo reached durable admission.
    ///
    /// Upstream then retires the echo once the inbox claim watermark proves no stale queue row can
    /// reappear. That watermark read is the inbox correlation this module leaves out, so admission is
    /// recorded here and the caller retires through [`SessionView::retire_submission`].
    pub fn note_submission_admitted(&self, request_id: &str) -> bool {
        let mut state = lock(&self.state);
        let Some(settlement) = state.settlements.get_mut(request_id) else {
            return false;
        };
        if settlement.retiring {
            return false;
        }
        settlement.admitted = true;
        true
    }

    /// Retires one echo exactly once (upstream `finishSubmission`).
    ///
    /// `observed` distinguishes "the durable message arrived" from "the Host refused". A refusal
    /// cannot retire an echo that already reached admission — the message exists, and telling its
    /// owner to restore the local preview would duplicate an attachment — so that order returns
    /// `false` instead of retiring.
    pub fn retire_submission(&self, request_id: &str, observed: bool) -> bool {
        let retired = {
            let mut state = lock(&self.state);
            let Some(settlement) = state.settlements.get_mut(request_id) else {
                return false;
            };
            if settlement.retiring || (!observed && settlement.admitted) {
                return false;
            }
            settlement.retiring = true;
            state.settlements.remove(request_id);
            state.echoes.retain(|echo| echo.request_id != request_id);
            true
        };
        self.mark(retired);
        retired
    }

    /// Retires one echo as failed (upstream's registered `abandon` path).
    pub fn abandon_submission(&self, request_id: &str) -> bool {
        self.retire_submission(request_id, false)
    }

    fn mark(&self, changed: bool) {
        if changed {
            self.notifier.mark_dirty();
        }
    }
}

/// Builds one immutable session snapshot from the mutable bits.
fn build_session_snapshot(
    session_id: &str,
    state: &ViewState,
    projections: &ProjectionValueStore,
) -> SessionSnapshot {
    let values = projections.values();
    // A catalogued child listed with `mode: unknown` is re-typed from its own `subagent`
    // projection, which the Host computes once the child's descriptor has been read.
    let subagent = state.address.as_ref().map(|address| {
        if address.mode == SubagentMode::Unknown
            && let Some(mode) = identity_mode(&values)
        {
            return SubagentAddress {
                parent_session_id: address.parent_session_id.clone(),
                child_session_id: address.child_session_id.clone(),
                mode,
            };
        }
        address.clone()
    });
    let accepted: Vec<PendingSubmission> = state
        .echoes
        .iter()
        .filter(|echo| {
            state
                .settlements
                .get(&echo.request_id)
                .is_some_and(|settlement| settlement.accepted)
        })
        .cloned()
        .collect();
    SessionSnapshot {
        session_id: session_id.to_owned(),
        pending_submissions: accepted,
        running: state.running,
        subagent,
        removed: state.removed,
        open_state: state.open_state,
        open_error: state.open_error.clone(),
        has_more: state.has_more,
        history_was_partial: state.history_was_partial,
        loading_older: state.history_load_status == HistoryLoadStatus::Loading,
        history_load_status: state.history_load_status,
        prompt_error: state.prompt_error.clone(),
        blank: state.blank,
        last_agent_error: state.last_agent_error.clone(),
        prompt_attempted: state.prompt_attempted,
        // The flag is set before the receipt; the session leaves blank only once the Host accepted.
        awaiting_first_turn: state.first_prompt_pending_turn && !state.blank,
    }
}

/// The `blank` and `lastPromptAt` fields of the `sessionListMetadata` projection.
fn metadata_of(projections: &ProjectionValueStore) -> (Option<bool>, Option<u64>) {
    let Some(metadata) = projections.value("sessionListMetadata") else {
        return (None, None);
    };
    (
        metadata.get("blank").and_then(Value::as_bool),
        metadata.get("lastPromptAt").and_then(Value::as_u64),
    )
}

/// Reads the `blank` field of one projection value map.
fn metadata_blank(values: &BTreeMap<String, Value>) -> Option<bool> {
    values
        .get("sessionListMetadata")
        .and_then(|value| value.get("blank"))
        .and_then(Value::as_bool)
}

/// Reads `lastPromptAt` from one projection value map.
fn metadata_last_prompt_at(values: &BTreeMap<String, Value>) -> Option<u64> {
    values
        .get("sessionListMetadata")
        .and_then(|value| value.get("lastPromptAt"))
        .and_then(Value::as_u64)
}

/// Reads the mode from the `subagent` identity projection.
fn identity_mode(values: &BTreeMap<String, Value>) -> Option<SubagentMode> {
    SubagentMode::from_wire(values.get("subagent")?.get("mode")?.as_str())
}

// ---------------------------------------------------------------------------------------------
// Session list
// ---------------------------------------------------------------------------------------------

/// Arrival lifecycle of the list, orthogonal to its pull-activity axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListPhase {
    /// No successful pull yet, so an empty row set means "nothing arrived".
    #[default]
    Pending,
    /// At least one pull landed. Monotone: never steps back.
    Ready,
}

/// Pull-activity of the list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListState {
    /// No pull outstanding and the last one succeeded.
    #[default]
    Idle,
    /// A pull is in flight.
    Loading,
    /// The last pull failed; the snapshot's `error` explains it.
    Error,
}

/// How a projection block reached the client, which is also its precedence class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintKind {
    /// The Host's live registry computed it; the watermark is comparable with frames.
    Sequenced,
    /// A view of the persisted checkpoint; the watermark is not comparable.
    Cached,
}

/// One list row's projection block.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectionHints {
    /// Which sequence space the watermark belongs to.
    pub kind: HintKind,
    /// The producer's watermark in that space.
    pub watermark: Watermark,
    /// Summary values present in the block; omitted keys are unknown, not absent.
    pub values: BTreeMap<String, Value>,
}

/// Lifecycle of one session's explicit projection read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProjectionState {
    /// No read yet.
    #[default]
    Idle,
    /// A read is in flight.
    Loading,
    /// A read landed.
    Ready,
    /// The session needs a format migration before its projections can be computed.
    MigrationRequired,
    /// The read failed.
    Error,
}

/// One session's projection values plus the lifecycle of their explicit read.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectionSnapshot {
    /// Every value the store currently holds.
    pub values: BTreeMap<String, Value>,
    /// Lifecycle of the explicit read.
    pub state: ProjectionState,
    /// Why the read failed, when it did.
    pub error: Option<RemoteFailure>,
}

/// Persistence format classification of one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatStatus {
    /// The session is on the current format.
    Current,
    /// The session needs a migration before it can be opened.
    MigrationRequired,
}

/// One Host list row, before lineage and projections are folded in.
///
/// The booleans are the row's independent facts — a running session can be blank, an available
/// Agent can be idle — so they are not mutually exclusive states that could collapse into one enum.
#[allow(clippy::struct_excessive_bools)] // See above: independent facts, not exclusive states.
#[derive(Clone, Debug, PartialEq)]
pub struct ListSummary {
    /// Session identity.
    pub session_id: String,
    /// Host-reported modification time, milliseconds.
    pub updated_at: u64,
    /// Whether a turn is running.
    pub running: bool,
    /// Whether no turn ever started.
    pub blank: bool,
    /// Whether the session currently owns a live Agent.
    pub agent_available: bool,
    /// The parent, for a subagent child.
    pub parent_session_id: Option<String>,
    /// Whether this row is a subagent child rather than an ordinary fork.
    pub origin_subagent: bool,
    /// Working directory, when known.
    pub cwd: Option<String>,
    /// Persistence format classification.
    pub format_status: Option<FormatStatus>,
    /// The row's summary projection block, when the listing carried one.
    pub projections: Option<ProjectionHints>,
}

/// One flattened list row, ready to render.
#[derive(Clone, Debug, PartialEq)]
pub struct ListEntry {
    /// Session identity.
    pub session_id: String,
    /// Latest title: the summary's own, else the title projection.
    pub title: Option<String>,
    /// Modification time, corrected by the metadata projection.
    pub updated_at: u64,
    /// Whether a turn is running.
    pub running: bool,
    /// Whether the session is still blank and reusable; lists hide these.
    pub blank: bool,
    /// The parent, for a subagent child.
    pub parent_session_id: Option<String>,
    /// Whether this row is a subagent child.
    pub origin_subagent: bool,
    /// Persistence format classification.
    pub format_status: Option<FormatStatus>,
    /// Working directory, when known.
    pub cwd: Option<String>,
    /// The row's summary projection values, never the full projection set.
    pub projection_values: Option<BTreeMap<String, Value>>,
    /// Lineage indent depth: a root is 0.
    pub depth: usize,
}

/// The immutable session-list snapshot a list view renders.
///
/// `items` is shared rather than cloned: an unchanged row set hands back the same allocation, which
/// is this crate's form of upstream's stable array identity.
#[derive(Clone, Debug, PartialEq)]
pub struct ListSnapshot {
    /// Render-order rows.
    pub items: Arc<Vec<ListEntry>>,
    /// Pull activity.
    pub state: ListState,
    /// Arrival lifecycle.
    pub phase: ListPhase,
    /// Why the last pull failed, when it did.
    pub error: Option<RemoteFailure>,
    /// Every attached session's projection values and read lifecycle.
    pub projections_by_session: BTreeMap<String, ProjectionSnapshot>,
}

impl ListSnapshot {
    /// The rows in render order.
    #[must_use]
    pub fn items(&self) -> &[ListEntry] {
        &self.items
    }
}

/// One mutation of the list, recorded so a pull in flight can replay it.
///
/// The log exists because a pull's response predates the frames that landed while it was in flight.
/// Recording the mutation's *effect* rather than re-deriving it from the summary keeps a removal, a
/// status flip, and an engagement distinguishable when replayed.
#[derive(Clone, Debug, PartialEq)]
pub enum ListMutation {
    /// A Host advertisement or refresh of one row.
    Upsert(ListSummary),
    /// A locally minted row (create or fork) that only fills missing metadata.
    Placeholder(ListSummary),
    /// The row left the list.
    Remove {
        /// Session identity.
        session_id: String,
    },
    /// An Agent running-state change.
    Status {
        /// Session identity.
        session_id: String,
        /// Current running state.
        running: bool,
        /// Whether an Agent is available.
        agent_available: bool,
    },
    /// A user-authored durable message moved the row's recency.
    Activity {
        /// Session identity.
        session_id: String,
        /// The message's timestamp.
        updated_at: u64,
    },
    /// A local first-send flipped the row out of blank.
    Engaged {
        /// Session identity.
        session_id: String,
    },
}

/// What one baseline frame carries: a complete per-session projection cut.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ControlBaseline {
    /// Every session's block, by identity.
    pub projections: BTreeMap<String, (Watermark, BTreeMap<String, Value>)>,
}

/// One live per-event projection change.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectionUpdate {
    /// Session whose projections changed.
    pub session_id: String,
    /// The committed event's seq, which is every change's watermark.
    pub seq: SeqCursor,
    /// One entry per changed key; unlike a baseline, absence means unchanged.
    pub changes: Vec<(String, Value)>,
}

/// One frame of the Host-wide live control stream.
///
/// Every generation starts with exactly one baseline. A delta frame removes nothing, which is the
/// single most important difference from the baseline.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlFrame {
    /// A complete cut for every attached session.
    Baseline(ControlBaseline),
    /// The keys one committed event changed.
    Projection(ProjectionUpdate),
}

/// A ticket proving which list pull a completion belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListTicket {
    generation: u64,
}

/// A ticket proving which projection read a completion belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionTicket {
    /// Session being read.
    pub session_id: String,
    generation: u64,
}

/// What one `session.projections` read returned.
#[derive(Clone, Debug, PartialEq)]
pub enum ProjectionRead {
    /// A live registry cut with a comparable watermark.
    Sequenced {
        /// The cut.
        watermark: Watermark,
        /// The values at that cut.
        values: BTreeMap<String, Value>,
    },
    /// A checkpoint view for a session that needs a migration.
    MigrationRequired {
        /// The checkpoint's values.
        values: BTreeMap<String, Value>,
    },
    /// The Host has no projections for that session yet.
    Absent,
    /// The read failed.
    Failed(RemoteFailure),
}

/// What one `session.list` pull returned.
#[derive(Clone, Debug, PartialEq)]
pub struct ListRead {
    /// The current rows.
    pub items: Vec<ListSummary>,
}

/// The work a reconnect implies, since this module performs no I/O of its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconnectPlan {
    /// Identities whose explicit projection read must be retried.
    pub refresh_projections: BTreeSet<String>,
}

/// One list row after its projection values and latest title are folded in.
///
/// The tuple is named because it flows through two closures; a reader should not have to count
/// `Option`s to know which field is which.
type MergedRow = (ListSummary, Option<String>, Option<BTreeMap<String, Value>>);

/// The mutable cluster state, guarded as a unit.
struct ManagerState {
    sessions: BTreeMap<String, Arc<SessionView>>,
    addresses: BTreeMap<String, SubagentAddress>,
    stores: BTreeMap<String, Arc<ProjectionValueStore>>,
    /// Identities accepted or observed running; retained across instance replacement.
    engaged: BTreeSet<String>,
    summaries: Vec<ListSummary>,
    list_state: ListState,
    list_phase: ListPhase,
    list_error: Option<RemoteFailure>,
    /// The in-flight pull's ticket and mutation log; the ticket's identity fences completion.
    list_inflight: Option<ListTicket>,
    list_mutations: Option<Vec<ListMutation>>,
    projection_loads: BTreeMap<String, (ProjectionState, Option<RemoteFailure>)>,
    /// In-flight reads: the ticket's generation and the values the store held when it began.
    ///
    /// The starting values are captured here, not at completion: upstream snapshots
    /// `store.values()` before awaiting and uses it to decide whether an `Absent` answer may clear
    /// the store. Capturing at completion would compare the store with itself and always clear.
    projection_inflight: BTreeMap<String, (u64, BTreeMap<String, Value>)>,
    /// The cached rows, shared with every snapshot built while they stay equal.
    items_cache: Option<Arc<Vec<ListEntry>>>,
    cache: ListSnapshot,
    disposed: bool,
    generation: u64,
    /// Weak so the store subscriptions can mark the list dirty without a reference cycle.
    notifier: Weak<Notifier>,
}

/// The instance cluster and the session list.
///
/// Upstream's `SessionManager`. It owns one [`SessionView`] per resident identity, one
/// [`ProjectionValueStore`] per identity ever advertised (a store outlives instantiation so a cold
/// title can render without opening anything), and the list snapshot those feed.
pub struct ListEngine {
    state: Arc<Mutex<ManagerState>>,
    notifier: Arc<Notifier>,
}

impl std::fmt::Debug for ListEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.state);
        formatter
            .debug_struct("ListEngine")
            .field("sessions", &state.sessions.len())
            .field("summaries", &state.summaries.len())
            .field("stores", &state.stores.len())
            .field("phase", &state.list_phase)
            .field("state", &state.list_state)
            .finish_non_exhaustive()
    }
}

impl ListEngine {
    /// Builds an empty cluster.
    #[must_use]
    pub fn new() -> Self {
        let initial = ListSnapshot {
            items: Arc::new(Vec::new()),
            state: ListState::Idle,
            phase: ListPhase::Pending,
            error: None,
            projections_by_session: BTreeMap::new(),
        };
        let state = Arc::new(Mutex::new(ManagerState {
            sessions: BTreeMap::new(),
            addresses: BTreeMap::new(),
            stores: BTreeMap::new(),
            engaged: BTreeSet::new(),
            summaries: Vec::new(),
            list_state: ListState::Idle,
            list_phase: ListPhase::Pending,
            list_error: None,
            list_inflight: None,
            list_mutations: None,
            projection_loads: BTreeMap::new(),
            projection_inflight: BTreeMap::new(),
            items_cache: None,
            cache: initial,
            disposed: false,
            generation: 0,
            notifier: Weak::new(),
        }));
        let weak_state: Weak<Mutex<ManagerState>> = Arc::downgrade(&state);
        let notifier = Arc::new(Notifier::new(move || {
            if let Some(state) = weak_state.upgrade() {
                let mut guard = lock(&state);
                guard.cache = build_list_snapshot(&mut guard);
            }
        }));
        lock(&state).notifier = Arc::downgrade(&notifier);
        Self { state, notifier }
    }

    /// Subscribes to list changes.
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> ListenerId {
        self.notifier.subscribe(listener)
    }

    /// Removes one subscription.
    pub fn unsubscribe(&self, id: ListenerId) {
        self.notifier.unsubscribe(id);
    }

    /// How many listeners are subscribed.
    #[must_use]
    pub fn listener_count(&self) -> usize {
        self.notifier.listener_count()
    }

    /// Delivers every pending publication: the projection stores first, then the list.
    ///
    /// Stores come first because a store change can correct a row's blank bit, and the list
    /// publication is what surfaces the corrected row. A caller with a render loop calls this from
    /// its frame callback.
    pub fn flush(&self) -> Publish {
        let stores: Vec<Arc<ProjectionValueStore>> =
            lock(&self.state).stores.values().cloned().collect();
        for store in stores {
            let _ = store.flush();
        }
        self.notifier.flush()
    }

    /// The current list snapshot, rebuilt first when dirty.
    #[must_use]
    pub fn snapshot(&self) -> ListSnapshot {
        self.notifier.ensure_fresh();
        lock(&self.state).cache.clone()
    }

    /// The resident instance for one identity, constructed on demand.
    ///
    /// Lazy on purpose: constructing a Session must not open its history — the caller decides when
    /// to open — and a list-only identity never needs an instance at all. A new instance adopts the
    /// list's current blank and running bits, so the list and the snapshot agree whichever arrived
    /// first.
    pub fn get(&self, session_id: &str) -> Arc<SessionView> {
        let mut state = lock(&self.state);
        if let Some(existing) = state.sessions.get(session_id) {
            return Arc::clone(existing);
        }
        let store = Self::projection_store_locked(&self.state, &mut state, session_id);
        let view = Arc::new(SessionView::new(session_id, store));
        if let Some(summary) = state
            .summaries
            .iter()
            .find(|row| row.session_id == session_id)
        {
            let blank = summary.blank && !state.engaged.contains(session_id);
            let running = summary.running;
            view.handle_blank(blank);
            view.handle_running(running);
        } else {
            {
                // A catalogued child exists only after its delegated session has durable history,
                // even though child rows carry no `blank` bit of their own.
                let catalogued = state
                    .addresses
                    .get(session_id)
                    .and_then(|address| state.stores.get(&address.parent_session_id))
                    .and_then(|store| catalogued_entry(store, session_id))
                    .is_some();
                if catalogued {
                    view.handle_blank(false);
                    view.handle_running(false);
                }
            }
        }
        state
            .sessions
            .insert(session_id.to_owned(), Arc::clone(&view));
        view
    }

    /// Whether an instance is resident for one identity.
    #[must_use]
    pub fn is_resident(&self, session_id: &str) -> bool {
        lock(&self.state).sessions.contains_key(session_id)
    }

    /// Withdraws one exact instance before its owner runs teardown.
    ///
    /// `expected` is compared by identity: a replacement instance is left untouched, which is what
    /// keeps an old stream's teardown from dropping a new one.
    pub fn drop_session(
        &self,
        session_id: &str,
        expected: &Arc<SessionView>,
    ) -> Option<Arc<SessionView>> {
        let mut state = lock(&self.state);
        let resident = state.sessions.get(session_id)?;
        if !Arc::ptr_eq(resident, expected) {
            return None;
        }
        let dropped = state.sessions.remove(session_id)?;
        state.addresses.remove(session_id);
        let retained = retained_ids(&state);
        if !retained.contains(session_id) {
            state.engaged.remove(session_id);
        }
        Some(dropped)
    }

    /// Stops the cluster and hands back every resident instance for its owner to dispose.
    ///
    /// In-flight reads are fenced by bumping the generation, so a late completion is ignored rather
    /// than resurrecting a torn-down cluster.
    pub fn dispose(&self) -> Vec<Arc<SessionView>> {
        let mut state = lock(&self.state);
        state.disposed = true;
        state.list_mutations = None;
        state.list_inflight = None;
        state.engaged.clear();
        state.projection_inflight.clear();
        state.projection_loads.clear();
        state.generation = state.generation.wrapping_add(1);
        let residents: Vec<Arc<SessionView>> = state.sessions.values().cloned().collect();
        state.sessions.clear();
        state.addresses.clear();
        residents
    }

    /// Resolves an acquisition target (upstream `resolveTarget`).
    ///
    /// An unknown identity is refused rather than materialised: `retain` is a promise that the
    /// identity exists, and silently creating a session object for a typo would make every later
    /// call fail somewhere else.
    ///
    /// # Errors
    ///
    /// Returns an error naming the identity when no summary, instance, or address proves it exists.
    pub fn resolve_target(
        &self,
        target: &str,
        address: Option<SubagentAddress>,
    ) -> Result<String, String> {
        let mut state = lock(&self.state);
        let known = state.sessions.contains_key(target)
            || state.summaries.iter().any(|row| row.session_id == target)
            || address.is_some()
            || state.addresses.contains_key(target);
        if !known {
            return Err(format!("sessions.retain: unknown session {target}"));
        }
        if let Some(address) = address {
            state.addresses.insert(target.to_owned(), address.clone());
            if let Some(view) = state.sessions.get(target) {
                view.configure_subagent(Some(address));
            }
        }
        Ok(target.to_owned())
    }

    /// Resolves a browsing address without granting transport authority.
    ///
    /// Three sources, in order: a retained address, then a catalogued child (which is *not* retained
    /// — breadcrumbs must not make future routing decisions out of a listing), then a listed summary
    /// whose mode is unknown because a header-only row does not carry one.
    #[must_use]
    pub fn subagent_address(&self, session_id: &str) -> Option<SubagentAddress> {
        let state = lock(&self.state);
        if let Some(address) = state.addresses.get(session_id) {
            return Some(address.clone());
        }
        for (parent_session_id, store) in &state.stores {
            if let Some(address) = catalogued_address(parent_session_id, store, session_id) {
                return Some(address);
            }
        }
        let summary = state
            .summaries
            .iter()
            .find(|row| row.session_id == session_id)?;
        if !summary.origin_subagent {
            return None;
        }
        Some(SubagentAddress {
            parent_session_id: summary.parent_session_id.clone()?,
            child_session_id: session_id.to_owned(),
            mode: SubagentMode::Unknown,
        })
    }

    /// Applies a complete control baseline or one live per-event frame.
    ///
    /// Returns the keys a baseline cleared, which is the caller-visible signal that a capability
    /// disappeared. A delta frame never clears anything.
    pub fn handle_control_frame(&self, frame: &ControlFrame) -> Vec<String> {
        let removed = match frame {
            ControlFrame::Baseline(baseline) => {
                let mut state = lock(&self.state);
                let mut removed = Vec::new();
                for (session_id, (watermark, values)) in &baseline.projections {
                    let store = Self::projection_store_locked(&self.state, &mut state, session_id);
                    removed.extend(seed_baseline(&store, *watermark, values));
                }
                removed
            }
            ControlFrame::Projection(update) => {
                let mut state = lock(&self.state);
                let store =
                    Self::projection_store_locked(&self.state, &mut state, &update.session_id);
                for (key, value) in &update.changes {
                    store.apply(key, value.clone(), update.seq);
                }
                Vec::new()
            }
        };
        self.notifier.mark_dirty();
        removed
    }

    /// Applies one Session-list addition forwarded through the Host event stream.
    pub fn handle_session_added(&self, summary: ListSummary) {
        let mut state = lock(&self.state);
        if !state.disposed && summary.running {
            state.engaged.insert(summary.session_id.clone());
        }
        let blank = effective_blank(&state, &summary);
        if let Some(view) = state.sessions.get(&summary.session_id) {
            view.handle_blank(blank);
        }
        if let Some(block) = summary.projections.clone() {
            let session_id = summary.session_id.clone();
            Self::apply_list_block_locked(&self.state, &mut state, &session_id, &block);
        }
        record_mutation_locked(&mut state, &ListMutation::Upsert(summary));
        drop(state);
        self.notifier.mark_dirty();
    }

    /// Applies one Session removal forwarded through the Host event stream.
    ///
    /// A durable subagent child is not removed from the list: its parent's catalog is the authority
    /// on whether it exists, and a child can be re-read after its Agent goes away. Only its status is
    /// lowered. Everything else loses the row, its address, and — when nothing else holds it — its
    /// projection store.
    pub fn handle_session_removed(&self, session_id: &str) {
        let mut state = lock(&self.state);
        let durable = state.addresses.contains_key(session_id)
            || state
                .summaries
                .iter()
                .any(|row| row.session_id == session_id && row.origin_subagent);
        if durable {
            record_mutation_locked(
                &mut state,
                &ListMutation::Status {
                    session_id: session_id.to_owned(),
                    running: false,
                    agent_available: false,
                },
            );
            if let Some(view) = state.sessions.get(session_id) {
                view.handle_running(false);
            }
        } else {
            record_mutation_locked(
                &mut state,
                &ListMutation::Remove {
                    session_id: session_id.to_owned(),
                },
            );
            if let Some(view) = state.sessions.get(session_id) {
                view.handle_removed();
            }
        }
        let catalog_held = state
            .stores
            .get(session_id)
            .and_then(|store| store.value("subagentCatalog"))
            .and_then(|value| value.as_array().map(|items| !items.is_empty()))
            .unwrap_or(false);
        if !durable && !catalog_held {
            state.stores.remove(session_id);
        }
        let retained = retained_ids(&state);
        if !retained.contains(session_id) {
            state.engaged.remove(session_id);
        }
        state.projection_inflight.remove(session_id);
        state.projection_loads.remove(session_id);
        drop(state);
        self.notifier.mark_dirty();
    }

    /// Applies one live Agent running-state change.
    pub fn handle_session_status(&self, session_id: &str, running: bool) {
        let mut state = lock(&self.state);
        if !state.disposed && running {
            state.engaged.insert(session_id.to_owned());
        }
        if let Some(view) = state.sessions.get(session_id) {
            view.handle_running(running);
        }
        record_mutation_locked(
            &mut state,
            &ListMutation::Status {
                session_id: session_id.to_owned(),
                running,
                agent_available: true,
            },
        );
        drop(state);
        self.notifier.mark_dirty();
    }

    /// Advances list activity from one user-authored durable message.
    pub fn handle_session_activity(&self, session_id: &str, updated_at: u64) {
        let mut state = lock(&self.state);
        record_mutation_locked(
            &mut state,
            &ListMutation::Activity {
                session_id: session_id.to_owned(),
                updated_at,
            },
        );
        drop(state);
        self.notifier.mark_dirty();
    }

    /// Surfaces one live Agent failure on an already-materialised session.
    pub fn handle_session_error(&self, session_id: &str, message: impl Into<String>) -> bool {
        let changed = {
            let mut state = lock(&self.state);
            let Some(view) = state.sessions.get(session_id).cloned() else {
                return false;
            };
            let _ = &mut state;
            drop(state);
            view.handle_agent_error(message)
        };
        self.notifier.mark_dirty();
        changed
    }

    /// Repairs a re-established Host-event generation (upstream `handleConnected`).
    ///
    /// Every projection cut belongs to the previous connection, so the stores are cleared rather
    /// than kept: a value whose watermark came from another connection cannot be compared with this
    /// one's. The caller then re-pulls the list and re-reads the projections of every parent it
    /// knows, which is what the returned plan names.
    pub fn handle_connected(&self) -> ReconnectPlan {
        let mut state = lock(&self.state);
        for store in state.stores.values() {
            store.clear();
        }
        state.list_mutations = None;
        state.list_inflight = None;
        state.generation = state.generation.wrapping_add(1);
        state.projection_inflight.clear();
        state.projection_loads.clear();
        let mut parents: BTreeSet<String> = BTreeSet::new();
        for session_id in state.sessions.keys() {
            if let Some(address) = state.addresses.get(session_id) {
                parents.insert(address.parent_session_id.clone());
            }
        }
        for (parent_session_id, store) in &state.stores {
            if store.contains("subagentCatalog") {
                parents.insert(parent_session_id.clone());
            }
        }
        drop(state);
        self.notifier.mark_dirty();
        ReconnectPlan {
            refresh_projections: parents,
        }
    }

    /// Starts one list pull, or returns `None` when one is already in flight.
    ///
    /// Single-flighting is not an optimisation here: two concurrent pulls would each install their
    /// own baseline, and the later one — which may have been taken earlier — would win. Returning the
    /// same ticket to a second caller is what makes a double-invocation harmless.
    pub fn begin_list_refresh(&self) -> Option<ListTicket> {
        let mut state = lock(&self.state);
        if state.list_inflight.is_some() {
            return None;
        }
        state.generation = state.generation.wrapping_add(1);
        let ticket = ListTicket {
            generation: state.generation,
        };
        state.list_state = ListState::Loading;
        state.list_error = None;
        state.list_inflight = Some(ticket);
        state.list_mutations = Some(Vec::new());
        drop(state);
        self.notifier.mark_dirty();
        Some(ticket)
    }

    /// Completes one list pull, replaying every mutation recorded while it was in flight.
    ///
    /// Returns whether the result was applied: `false` means the pull was fenced by a reconnect or a
    /// disposal, and the caller must not treat its rows as current.
    #[allow(clippy::too_many_lines)] // The pull's completion is one atomic reconciliation.
    pub fn complete_list_refresh(
        &self,
        ticket: ListTicket,
        result: Result<ListRead, RemoteFailure>,
    ) -> bool {
        let mut state = lock(&self.state);
        if state.list_inflight != Some(ticket) {
            return false;
        }
        let mutations = state.list_mutations.take().unwrap_or_default();
        state.list_inflight = None;
        match result {
            Ok(read) => {
                // The established order survives a pull: the response's own order only decides where
                // identities the client has never shown belong.
                let baseline: Vec<ListSummary> = if state.list_phase == ListPhase::Pending {
                    read.items.clone()
                } else {
                    merge_ordered_baseline(&state.summaries, &read.items, |row| {
                        row.session_id.clone()
                    })
                };
                // A removal supersedes running observed in the pull, including after re-addition.
                let removed_since_pull: BTreeSet<&str> = mutations
                    .iter()
                    .filter_map(|mutation| match mutation {
                        ListMutation::Remove { session_id } => Some(session_id.as_str()),
                        _ => None,
                    })
                    .collect();
                for row in &baseline {
                    if row.running && !removed_since_pull.contains(row.session_id.as_str()) {
                        state.engaged.insert(row.session_id.clone());
                    }
                }
                let mut summaries = baseline;
                for mutation in &mutations {
                    apply_mutation(&mut summaries, mutation);
                }
                state.summaries = summaries;
                // A full list can drop identities without a removal frame.
                let retained = retained_ids(&state);
                state.engaged.retain(|id| retained.contains(id));
                state.list_state = ListState::Idle;
                state.list_phase = ListPhase::Ready;
                state.list_error = None;
                // Resident instances and list rows share one display blank, reconciled with the
                // metadata projection.
                let updates: Vec<(Arc<SessionView>, bool, bool)> = state
                    .summaries
                    .iter()
                    .filter_map(|row| {
                        let view = state.sessions.get(&row.session_id)?;
                        Some((Arc::clone(view), effective_blank(&state, row), row.running))
                    })
                    .collect();
                for (view, blank, running) in updates {
                    view.handle_blank(blank);
                    view.handle_running(running);
                }
                // Land each row's projection block in its store: a cold title surfaces without
                // opening the session. A block is partial, so an absent key never clears.
                let blocks: Vec<(String, ProjectionHints)> = read
                    .items
                    .iter()
                    .filter_map(|row| Some((row.session_id.clone(), row.projections.clone()?)))
                    .collect();
                for (session_id, block) in blocks {
                    Self::apply_list_block_locked(&self.state, &mut state, &session_id, &block);
                }
            }
            Err(failure) => {
                state.list_state = ListState::Error;
                state.list_error = Some(failure);
                // `phase` is monotone: a failed pull proves nothing about arrival.
            }
        }
        drop(state);
        self.notifier.mark_dirty();
        true
    }

    /// Abandons an in-flight pull (a reconnect or a disposal).
    pub fn cancel_list_refresh(&self) {
        let mut state = lock(&self.state);
        state.list_mutations = None;
        state.list_inflight = None;
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        self.notifier.mark_dirty();
    }

    /// Starts one explicit projection read without opening the conversation.
    ///
    /// Returns `None` when a read is already in flight (its result is shared) or when the previous
    /// read already succeeded — a ready row needs no re-read, and re-reading it on every parent
    /// refresh is what turns a breadcrumb render into a request storm.
    pub fn begin_projection_read(&self, session_id: &str) -> Option<ProjectionTicket> {
        let mut state = lock(&self.state);
        if state.projection_inflight.contains_key(session_id) {
            return None;
        }
        if state
            .projection_loads
            .get(session_id)
            .is_some_and(|(load, _)| *load == ProjectionState::Ready)
        {
            return None;
        }
        let store = Self::projection_store_locked(&self.state, &mut state, session_id);
        let initial = store.values();
        let _ = store;
        state.generation = state.generation.wrapping_add(1);
        let ticket = ProjectionTicket {
            session_id: session_id.to_owned(),
            generation: state.generation,
        };
        state
            .projection_inflight
            .insert(session_id.to_owned(), (ticket.generation, initial));
        state
            .projection_loads
            .insert(session_id.to_owned(), (ProjectionState::Loading, None));
        drop(state);
        self.notifier.mark_dirty();
        Some(ticket)
    }

    /// Completes one explicit projection read.
    ///
    /// A read that lands after the connection changed is dropped: its values belong to a generation
    /// whose watermarks are not comparable with the current one, so applying them would pin a stale
    /// value above every live frame.
    pub fn complete_projection_read(&self, ticket: ProjectionTicket, read: ProjectionRead) -> bool {
        let mut state = lock(&self.state);
        let Some((generation, before)) = state.projection_inflight.get(&ticket.session_id) else {
            return false;
        };
        if *generation != ticket.generation {
            return false;
        }
        let before = before.clone();
        state.projection_inflight.remove(&ticket.session_id);
        let store = Self::projection_store_locked(&self.state, &mut state, &ticket.session_id);
        let load = match read {
            ProjectionRead::Sequenced { watermark, values } => {
                seed_baseline(&store, watermark, &values);
                (ProjectionState::Ready, None)
            }
            ProjectionRead::MigrationRequired { values } => {
                store.apply_cached(&values);
                (ProjectionState::MigrationRequired, None)
            }
            ProjectionRead::Absent => {
                // A later frame proves existence independently of an earlier missing read, so the
                // store is only cleared when the read found nothing *and* nothing arrived since.
                if store.values() == before {
                    store.clear();
                }
                (ProjectionState::Ready, None)
            }
            ProjectionRead::Failed(failure) => (ProjectionState::Error, Some(failure)),
        };
        state.projection_loads.insert(ticket.session_id, load);
        drop(state);
        self.notifier.mark_dirty();
        true
    }

    /// Applies the accepted title of a rename without opening the session.
    ///
    /// Writing the projection from the response settles the row immediately; the control frame that
    /// follows is a no-op replay under the store's higher-seq-wins rule.
    pub fn apply_title(&self, session_id: &str, title: &str, seq: SeqCursor) {
        let mut state = lock(&self.state);
        let store = Self::projection_store_locked(&self.state, &mut state, session_id);
        store.apply("title", Value::String(title.to_owned()), seq);
        drop(state);
        self.notifier.mark_dirty();
    }

    /// Merges a locally minted row (create or fork) into the list immediately.
    ///
    /// A created session is blank by definition — entity birth precedes the first message — and the
    /// row appears now rather than at the next pull, so the caller can prompt it without waiting.
    pub fn add_placeholder(&self, session_id: &str, cwd: Option<String>, now_ms: u64) {
        let mut state = lock(&self.state);
        record_mutation_locked(
            &mut state,
            &ListMutation::Placeholder(ListSummary {
                session_id: session_id.to_owned(),
                updated_at: now_ms,
                running: false,
                blank: true,
                agent_available: true,
                parent_session_id: None,
                origin_subagent: false,
                cwd,
                format_status: None,
                projections: None,
            }),
        );
        drop(state);
        self.notifier.mark_dirty();
    }

    /// Records one identity as engaged (upstream's `onEngaged` mirror).
    ///
    /// The list mirrors the sender's local first-send flip so a session surfaces — lists filter on
    /// blank — before any Host frame lands. An identity already on the ledger needs no second row
    /// mutation, and one the list does not retain is ignored: the engagement belongs to a session
    /// nobody is listing.
    pub fn engage(&self, session_id: &str) -> bool {
        let mut state = lock(&self.state);
        if state.disposed {
            return false;
        }
        let retained = retained_ids(&state);
        if !retained.contains(session_id) {
            return false;
        }
        if !state.engaged.insert(session_id.to_owned()) {
            return false;
        }
        record_mutation_locked(
            &mut state,
            &ListMutation::Engaged {
                session_id: session_id.to_owned(),
            },
        );
        if let Some(view) = state.sessions.get(session_id) {
            view.handle_blank(false);
        }
        drop(state);
        self.notifier.mark_dirty();
        true
    }

    /// Every projection value the cluster holds for one identity, even a never-instantiated one.
    #[must_use]
    pub fn projection_values(&self, session_id: &str) -> Option<BTreeMap<String, Value>> {
        lock(&self.state)
            .stores
            .get(session_id)
            .map(|store| store.values())
    }

    /// The unit's store for one identity, created on demand.
    fn projection_store_locked(
        engine: &Arc<Mutex<ManagerState>>,
        state: &mut ManagerState,
        session_id: &str,
    ) -> Arc<ProjectionValueStore> {
        if let Some(store) = state.stores.get(session_id) {
            return Arc::clone(store);
        }
        let store = Arc::new(ProjectionValueStore::new());
        let weak_state: Weak<Mutex<ManagerState>> = Arc::downgrade(engine);
        let weak_notifier = state.notifier.clone();
        let session = session_id.to_owned();
        store.subscribe_any(move || {
            let Some(state) = weak_state.upgrade() else {
                return;
            };
            // Newer history or control metadata corrects a resident Session's stale list hint. The
            // guard is dropped before touching the view: `handle_blank` reads the same store.
            let (blank, view) = {
                let guard = lock(&state);
                let blank = guard
                    .stores
                    .get(&session)
                    .and_then(|held| metadata_blank(&held.values()));
                (blank, guard.sessions.get(&session).cloned())
            };
            if blank == Some(false)
                && let Some(view) = view
            {
                view.handle_blank(false);
            }
            if let Some(notifier) = weak_notifier.upgrade() {
                notifier.mark_dirty();
            }
        });
        state
            .stores
            .insert(session_id.to_owned(), Arc::clone(&store));
        store
    }

    /// Applies one list-surface projection block by the sequence space it declares.
    ///
    /// A `sequenced` block lands under higher-seq-wins against that session's baselines and frames.
    /// A `cached` block was viewed from the persisted checkpoint by a header-only listing: its
    /// watermark is not comparable with this connection's seqs, so it only fills keys no sequenced
    /// row holds.
    fn apply_list_block_locked(
        engine: &Arc<Mutex<ManagerState>>,
        state: &mut ManagerState,
        session_id: &str,
        block: &ProjectionHints,
    ) {
        let store = Self::projection_store_locked(engine, state, session_id);
        match block.kind {
            HintKind::Sequenced => {
                let Watermark::At(seq) = block.watermark else {
                    // A `sequenced` block with the empty-cut sentinel carries values consistent with
                    // an empty log: they outrank nothing this connection computed.
                    store.apply_cached(&block.values);
                    return;
                };
                for (key, value) in &block.values {
                    store.apply(key, value.clone(), seq);
                }
            }
            HintKind::Cached => {
                store.apply_cached(&block.values);
            }
        }
    }
}

impl Default for ListEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// The catalogued entry for one child, if the parent's projection lists it.
fn catalogued_entry(store: &ProjectionValueStore, child_session_id: &str) -> Option<Value> {
    store
        .value("subagentCatalog")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(child_session_id))
        .cloned()
}

/// A browsing address derived from a parent's catalog, refusing an `external` child.
///
/// An external child is delegated *outside* this process, so reading it through the parent's history
/// route would show the wrong log. Upstream excludes it from address resolution for exactly that
/// reason.
fn catalogued_address(
    parent_session_id: &str,
    store: &ProjectionValueStore,
    child_session_id: &str,
) -> Option<SubagentAddress> {
    let entry = catalogued_entry(store, child_session_id)?;
    let mode = SubagentMode::from_wire(entry.get("mode").and_then(Value::as_str))?;
    if mode == SubagentMode::Unknown {
        return None;
    }
    Some(SubagentAddress {
        parent_session_id: parent_session_id.to_owned(),
        child_session_id: child_session_id.to_owned(),
        mode,
    })
}

/// The blank a row displays, after the engagement ledger is consulted.
fn effective_blank(state: &ManagerState, summary: &ListSummary) -> bool {
    summary.blank && !state.engaged.contains(&summary.session_id)
}

/// Identities a row or an engagement may still belong to.
fn retained_ids(state: &ManagerState) -> BTreeSet<String> {
    let mut retained: BTreeSet<String> = state
        .summaries
        .iter()
        .map(|row| row.session_id.clone())
        .collect();
    retained.extend(state.sessions.keys().cloned());
    retained.extend(state.addresses.keys().cloned());
    retained
}

/// Applies one mutation and retains it for the in-flight pull to replay.
///
/// The mutation is taken by reference rather than by value: the caller usually keeps constructing
/// more of them, and the only thing this needs is to read it twice (log once, apply in place).
fn record_mutation_locked(state: &mut ManagerState, mutation: &ListMutation) {
    if state.disposed {
        return;
    }
    if let Some(log) = state.list_mutations.as_mut() {
        log.push(mutation.clone());
    }
    apply_mutation(&mut state.summaries, mutation);
}

/// Applies one mutation to a row set without deriving display order.
fn apply_mutation(summaries: &mut Vec<ListSummary>, mutation: &ListMutation) {
    match mutation {
        ListMutation::Upsert(next) => upsert_row(summaries, next, true),
        ListMutation::Placeholder(next) => upsert_row(summaries, next, false),
        ListMutation::Remove { session_id } => {
            summaries.retain(|row| row.session_id != *session_id);
        }
        ListMutation::Status {
            session_id,
            running,
            agent_available,
        } => {
            for row in summaries
                .iter_mut()
                .filter(|row| row.session_id == *session_id)
            {
                // Running converts display state; it never establishes durable history.
                let needs = row.running != *running
                    || row.agent_available != *agent_available
                    || (*running && row.blank);
                if needs {
                    row.running = *running;
                    row.agent_available = *agent_available;
                    row.blank = row.blank && !*running;
                }
            }
        }
        ListMutation::Activity {
            session_id,
            updated_at,
        } => {
            for row in summaries
                .iter_mut()
                .filter(|row| row.session_id == *session_id)
            {
                if *updated_at > row.updated_at {
                    row.updated_at = *updated_at;
                }
            }
        }
        ListMutation::Engaged { session_id } => {
            for row in summaries
                .iter_mut()
                .filter(|row| row.session_id == *session_id)
            {
                row.blank = false;
            }
        }
    }
}

/// Merges one upsert or placeholder row.
///
/// A placeholder only fills metadata: it is a locally minted identity, so it cannot overrule a live
/// running bit the Host already reported. Blank only lowers, so a stale `true` racing a local first
/// send never re-hides a row that has already surfaced.
fn upsert_row(summaries: &mut Vec<ListSummary>, next: &ListSummary, live: bool) {
    let Some(existing) = summaries
        .iter_mut()
        .find(|row| row.session_id == next.session_id)
    else {
        summaries.insert(0, next.clone());
        return;
    };
    existing.blank = existing.blank && next.blank;
    if live {
        existing.agent_available = next.agent_available;
        existing.running = next.running;
        if next.format_status.is_some() {
            existing.format_status = next.format_status;
        }
    }
    if existing.cwd.is_none() {
        existing.cwd.clone_from(&next.cwd);
    }
    if existing.parent_session_id.is_none() {
        existing
            .parent_session_id
            .clone_from(&next.parent_session_id);
    }
    if !existing.origin_subagent && next.origin_subagent {
        existing.origin_subagent = true;
    }
}

/// Builds one list snapshot, sharing the previous row allocation when nothing changed.
fn build_list_snapshot(state: &mut ManagerState) -> ListSnapshot {
    // Fold each row's projections in, then reconcile the durability blank with the metadata
    // projection. The store map is borrowed for this block only, so the caches below can be written.
    let merged: Vec<MergedRow> = {
        let stores = &state.stores;
        let engaged = &state.engaged;
        state
            .summaries
            .iter()
            .map(|summary| {
                let values = stores.get(&summary.session_id).map(|store| store.values());
                let mut row = summary.clone();
                let (metadata, last_prompt_at) = match &values {
                    Some(values) => (metadata_blank(values), metadata_last_prompt_at(values)),
                    None => (None, None),
                };
                // List rows read the generic `title` projection key; there is no title frame.
                let title = values
                    .as_ref()
                    .and_then(|values| values.get("title"))
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned);
                row.updated_at = row.updated_at.max(last_prompt_at.unwrap_or(0));
                // The metadata projection refines the durability bit into a display bit.
                row.blank =
                    row.blank && !engaged.contains(&row.session_id) && metadata != Some(false);
                (row, title, values)
            })
            .collect()
    };

    let lineage: Vec<LineageRow<'_>> = merged
        .iter()
        .map(|(row, _, _)| LineageRow {
            session_id: &row.session_id,
            parent_session_id: row.parent_session_id.as_deref(),
            subagent: row.origin_subagent,
        })
        .collect();
    let flat = flatten_lineage(&lineage);

    let items: Vec<ListEntry> = flat
        .iter()
        .filter_map(|entry| {
            let (row, title, values) = merged.get(entry.index)?;
            Some(ListEntry {
                session_id: row.session_id.clone(),
                title: title.clone(),
                updated_at: row.updated_at,
                running: row.running,
                blank: row.blank,
                parent_session_id: row.parent_session_id.clone(),
                origin_subagent: row.origin_subagent,
                format_status: row.format_status,
                cwd: row.cwd.clone(),
                projection_values: values.clone(),
                depth: entry.depth,
            })
        })
        .collect();

    // Reference stability: an unchanged row set keeps the previous allocation, so a memoised row
    // component sees the same shared rows on every rebuild that changed only a projection elsewhere.
    let items = match &state.items_cache {
        Some(previous) if **previous == items => Arc::clone(previous),
        _ => {
            let fresh = Arc::new(items);
            state.items_cache = Some(Arc::clone(&fresh));
            fresh
        }
    };

    let projections_by_session: BTreeMap<String, ProjectionSnapshot> = state
        .stores
        .iter()
        .map(|(session_id, store)| {
            let (load, error) = state
                .projection_loads
                .get(session_id)
                .cloned()
                .unwrap_or((ProjectionState::Idle, None));
            (
                session_id.clone(),
                ProjectionSnapshot {
                    values: store.values(),
                    state: load,
                    error,
                },
            )
        })
        .collect();

    ListSnapshot {
        items,
        state: state.list_state,
        phase: state.list_phase,
        error: state.list_error.clone(),
        projections_by_session,
    }
}
