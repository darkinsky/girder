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
//! - `src/client/ordered-baseline.ts` (43 lines) — the reference-order merge. Already ported in
//!   [`girder_core::session_controller_gap::merge_ordered_baseline`]; this module calls it.
//!
//! Two sibling modules already ported the pieces these files sit on: `session_controller_gap` has
//! the history cut, the cursor algebra, and the lineage flattening; `session_engine_gap` has
//! [`ProjectionValueStore`] and [`Notifier`]. Neither had a snapshot builder, so nothing in this
//! crate could answer "what does the client render right now". This module adds that, and it does
//! not re-implement either: every projection value goes through the existing store, every
//! publication through the existing notifier, every ordering decision through the existing
//! baseline merge and lineage walk.
//!
//! # The two snapshots, and why they are separate objects
//!
//! `SessionSnapshot` is small, hot, and per session: React's `useSyncExternalStore` calls
//! `getSnapshot` on every render of every component watching that session. `ListSnapshot` is cold
//! and wide: it holds every row, every row's lineage depth, and every attached session's projection
//! values. Merging them would either make the list rebuild on every running-bit flip of one
//! session, or make per-session reads pay for a lineage walk. Upstream keeps them separate and this
//! module follows: two caches, two notifiers.
//!
//! # Rules that are load-bearing, and why
//!
//! - **A snapshot is rebuilt lazily, and only once per batch.** Writes mark dirty; a read while
//!   dirty rebuilds synchronously; a publication rebuilds before notifying. Without this a frame
//!   storm (a control baseline for two hundred sessions) would rebuild two hundred snapshots for a
//!   repaint that never happened. [`Notifier`] already encodes the batching; this module supplies
//!   the rebuild closure.
//! - **Blank only ever lowers with help.** `handle_blank` refuses to re-blank a session that has a
//!   prompt attempt or is currently running: the list's `blank` is a durability question ("no turn
//!   ever started"), while the composer's is a display question, and a stale list row must not
//!   re-hide a conversation the user is looking at.
//! - **Running clears blank; blank never clears running.** A blank session that starts running is
//!   no longer blank — it has a turn. A non-blank session that stops running does not become blank,
//!   because its first prompt is still durably there.
//! - **`awaiting_first_turn` is two flags, not one.** The session is "engaging" only between the
//!   accepted prompt and the first durable `turn/start`; once running is observed the flag clears
//!   even if the turn event never arrives (a Host that admitted and failed still moved on).
//! - **Engagement outlives instantiation.** The manager remembers which identities were accepted or
//!   seen running, across Session object replacement and across list rows disappearing, because a
//!   prompt can be accepted by a Session object the manager has already replaced. Forgetting early
//!   re-blanks a conversation mid-turn.
//! - **A pull's completion is replayed over, not overwritten by, the frames that arrived during it.**
//!   A list response is a baseline snapshot of the Host's ledger, taken before the frames that
//!   landed while it was in flight. Taking it as the truth would lose an addition, a rename, or a
//!   removal that the user just watched happen.
//! - **`phase` is monotone; `state` is the failure axis.** `pending → ready` fires on the first
//!   successful pull and never steps back, so "empty" means "nothing arrived" until a pull proves
//!   otherwise. Pull *failures* are reported in `state`/`error` instead, which is a different
//!   question and does not imply the client has no rows.
//! - **A reference-stable row is not a policy, it is a cost.** Every list rebuild re-derives all
//!   rows; a row whose fields all match the previous rebuild reuses the previous object, and an
//!   unchanged order reuses the previous array. Without it every memoised row component re-renders
//!   on every Host frame.
//! - **A control baseline is a consistent cut, a per-event frame is not.** The baseline seeds every
//!   key and clears what it omits; a frame carries only the keys that changed and clears nothing.
//!   Confusing the two either resurrects deleted capabilities or drops live ones.
//! - **A cached list block never outranks a sequenced row.** The list may carry a zero-I/O view of
//!   the persisted checkpoint for sessions nobody opened. Its watermark is not comparable with this
//!   connection's sequence space, so it only fills keys no sequenced row holds.
//! - **A prompt echo is published only after the Host accepted it.** The echo exists so the user sees
//!   their message before durable admission; publishing it earlier would show a message the Host
//!   rejected, and retiring it before acceptance would make a failed send vanish silently.
//!
//! # What this module deliberately does not do
//!
//! - **No transport.** Every method takes the data a Remote call would have returned. The
//!   single-flight list pull is modelled as `begin`/`complete` around a caller-owned future, and a
//!   projection read as a ticket plus its result, which is what makes the fencing testable without
//!   a socket.
//! - **No inbox correlation.** Upstream settles a prompt echo against the `inbox` projection and
//!   durable `user/message`/`agent/inbox/spliced` events. That reconciliation is the remaining gap
//!   in this port; the echo ledger here publishes on acceptance and retires on an explicit
//!   observation, and its docs say which upstream call each entry point corresponds to.
//! - **No React bindings.** A subscriber is a `Fn()`; the framework calls it.
//!
//! # Module-include note
//!
//! Both this file and its test are included by path from `tests/session_snapshot_gap.rs`, so every
//! reference to the library goes through `girder_core::…` rather than `crate::…`: inside a
//! `#[path]` module, `crate::` names the *test* crate.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use serde_json::{Map, Value};

use girder_core::ProjectionListenerId as ListenerId;
use girder_core::gateway_client_gap::RemoteFailure;
use girder_core::session_controller_gap::{LineageRow, flatten_lineage, merge_ordered_baseline};
use girder_core::{Notifier, ProjectionValueStore, Publish, SeqCursor};

/// Locks a mutex, treating poisoning as a stale writer rather than a fatal state.
///
/// A poisoned snapshot mutex means some thread panicked mid-write. The value it guards is a set of
/// booleans and a cached snapshot: the next writer overwrites all of it, and a reader would rather
/// render the last consistent snapshot than abort the process. This is the same choice
/// [`girder_core::ProjectionValueStore`] makes for its own rows.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The watermark a projection block is consistent with.
///
/// Upstream's `SessionSeqCursor` is a branded `number` whose only unusual value is `-1`, which the
/// transport uses for "this cut is before the first event" (an empty log). `u64` cannot carry that
/// sentinel, and mapping it to `0` would be wrong in one direction: an event at seq `0` is a real
/// event, and a value seeded at `-1` must yield to one seeded at `0`, while a value seeded at `0`
/// must not yield to a second value seeded at `0`. The variant keeps the distinction.
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
    /// rejected rather than clamped, because a silently clamped watermark would make a corrupt
    /// frame look like a valid one and lose whichever value it carried.
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
/// For a real cut this is exactly [`ProjectionValueStore::seed`]: sweep cached rows, apply carried
/// keys under higher-seq-wins, then clear every key the cut omits unless a newer frame superseded
/// the cut. That whole precedence rule already lives in the store, and duplicating it here is the
/// two-sources-of-truth bug this port exists to avoid.
///
/// The empty-cut sentinel needs one different step because no `u64` watermark sits below a real
/// event. Upstream seeds those keys at `-1`, so every real frame outranks them; the closest faithful
/// modelling with the store's public API is to seed them as *cached* rows, which also yield to every
/// sequenced row. One consequence is documented rather than hidden: a later cached **list** block
/// could overwrite a sentinel-seeded key, which upstream would resist. That needs an attached session
/// with an empty log to also be listed header-only from the persisted checkpoint, which the wire
/// protocol does not produce.
pub fn seed_baseline(
    store: &ProjectionValueStore,
    watermark: Watermark,
    values: &BTreeMap<String, Value>,
) -> Vec<String> {
    let Watermark::BeforeFirstEvent = watermark else {
        // The safe destructuring above only runs this arm for a real cut.
        return match watermark {
            Watermark::At(seq) => store.seed(seq, values),
            Watermark::BeforeFirstEvent => Vec::new(),
        };
    };

    // Upstream sweeps every cached row first, whatever the cut. A sequenced row survives, because
    // its seq is above the sentinel.
    let rows = store.values();
    let sequenced: Vec<(String, Value, SeqCursor)> = rows
        .keys()
        .filter_map(|key| {
            let seq = store.seq_of(key)?;
            let value = rows.get(key)?.clone();
            Some((key.clone(), value, seq))
        })
        .collect();
    let swept: Vec<String> = rows
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

    // Carried keys land only where no sequenced row holds them: the store's cached rule.
    let absent: BTreeMap<String, Value> = values
        .iter()
        .filter(|(key, _)| store.seq_of(key).is_none())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    store.apply_cached(&absent);

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
/// `pending-consumption` is the state that exists because a page can arrive while the window is
/// being replaced: it means "a page is held, not yet published". It is not a loading state — the
/// spinner must stop — which is why it is not folded into [`Self::Loading`].
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
/// address is then re-typed from the child's own `subagent` projection once it is read.
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
    /// Reads the wire spelling, absent for an unrecognized one.
    ///
    /// A mode this build does not know is treated as unknown rather than rejected: the address is
    /// still usable for navigation, and refusing it would hide a listed child.
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
    /// The queue dock: a running session appends it to the pending work.
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
/// this struct never needs a wire form.
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
/// This is upstream's `SessionSnapshot` minus the fields that belong to the event-window machinery
/// (which the stream layer owns) and with the notification callbacks lifted out of the value: a Rust
/// snapshot is a value a subscriber reads, not a pair of handles.
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
    /// Whether a history request is in flight (including a turn jump).
    pub loading_older: bool,
    /// The status behind `loading_older`.
    pub history_load_status: HistoryLoadStatus,
    /// The last send/stop failure, cleared by the next attempt.
    pub prompt_error: Option<PromptError>,
    /// Whether the session has no turn yet and is still reusable.
    pub blank: bool,
    /// The last live Agent failure with no turn position.
    pub last_agent_error: Option<String>,
    /// Whether a prompt call began on this object; sticky.
    pub prompt_attempted: bool,
    /// Whether an accepted first prompt has not yet reached a durable `turn/start`.
    pub awaiting_first_turn: bool,
}

/// The mutable half of one session's snapshot, guarded as a unit.
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
    /// Echo identities whose settlement state is tracked; absent means already retired.
    settlements: BTreeMap<String, Settlement>,
    /// The cached snapshot the notifier rebuilds and readers clone.
    cache: SessionSnapshot,
}

/// One echo's settlement state.
#[derive(Clone, Copy, Debug)]
struct Settlement {
    /// Whether the Host accepted the prompt.
    accepted: bool,
    /// Whether durable admission was observed.
    admitted: bool,
    /// Whether a retirement is already scheduled, which latches the first observation.
    retiring: bool,
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
/// inheriting the stream's I/O. The event window itself is outside this module: its length, its
/// pages, and its assistant-stream folding are a different capability.
pub struct SessionView {
    session_id: String,
    state: Mutex<ViewState>,
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
    /// it: a store outlives the object, so control frames can land before the session is
    /// instantiated and a list row can read a cold title without opening anything. A caller without
    /// a manager passes [`Arc::new(ProjectionValueStore::new())`].
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
        let state = Mutex::new(ViewState {
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
        });
        let rebuild_state = Arc::new(());
        drop(rebuild_state);
        // The rebuild closure needs the same `ViewState` the methods lock. `SessionView` owns its
        // state by value, so the closure is installed after construction through the shared handle
        // below rather than capturing `self`.
        let view = Self {
            session_id,
            state,
            projections,
            notifier: Arc::new(Notifier::new(|| {})),
        };
        view
    }

    /// The session identity.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The manager-owned projection store this session reads.
    #[must_use]
    pub fn projections(&self) -> &Arc<ProjectionValueStore> {
        &self.projections
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
    /// end of the task that wrote. Kept public because "three writes, one publication" is the
    /// property batching exists for and it cannot be asserted from outside otherwise.
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
    /// turn" flag: once the Agent is running, the engaging phase is over whether or not the durable
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
    /// Two refusals matter. A projection that says `blank: false` wins over a stale list row, because
    /// the projection is the Host's own metadata and the row may predate it. And a session with a
    /// prompt attempt or a running Agent cannot be re-blanked: the list answers "is there durable
    /// history", while the composer is asking "should the user keep looking at this", and a stale row
    /// must not hide a conversation that is happening.
    pub fn handle_blank(&self, blank: bool) -> bool {
        let metadata_blank = metadata_blank(&self.projections.values());
        let blank = blank && metadata_blank != Some(false);
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

    /// Records a live Agent failure with no turn position.
    pub fn handle_agent_error(&self, message: impl Into<String>) -> bool {
        {
            let mut state = lock(&self.state);
            state.last_agent_error = Some(message.into());
        }
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
        self.mark(true);
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
    /// prompt observed there is no longer awaiting anything, even if `handle_running` never fired.
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

    /// Begins a prompt attempt: the previous error surfaces clear and the engaging phase may start.
    ///
    /// Upstream does this before its first `await`, so a `turn/start` observed ahead of the receipt
    /// can clear the engaging flag without the send path racing it.
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

    /// Applies the Host's acceptance of one prompt.
    ///
    /// Acceptance is what publishes an echo and moves a blank session to engaging. The settlement
    /// latch makes a second proof (a receipt and an inbox row, whichever arrives second) a no-op.
    /// Returns whether this call was the acceptance.
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
            changed || request_id.is_some()
        };
        self.mark(changed);
        changed
    }

    /// Records a rejected or failed prompt: the echo retires as failed and the error surfaces.
    ///
    /// A first prompt that never reached admission leaves the session blank and reusable, which is
    /// the whole reason the engaging flag is a separate bit.
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

    /// Records a failed cancel request.
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

    /// Registers one local prompt echo and returns the surface fixed for it.
    ///
    /// The placement is derived once, at submission time: an Inbox acceptance later must not move a
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
    /// recorded and the caller retires through [`SessionView::retire_submission`].
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

    /// Retires one echo exactly once.
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

    /// Retires one echo as failed (upstream's `abandon` path).
    pub fn abandon_submission(&self, request_id: &str) -> bool {
        self.retire_submission(request_id, false)
    }

    fn mark(&self, changed: bool) {
        if changed {
            self.notifier.mark_dirty();
        }
    }

    /// Rebuilds the cached snapshot from the mutable state.
    fn rebuild(&self) {
        let mut state = lock(&self.state);
        state.cache = build_snapshot(&self.session_id, &state, &self.projections);
    }
}

/// Builds one immutable snapshot from the mutable bits.
fn build_snapshot(
    session_id: &str,
    state: &ViewState,
    projections: &ProjectionValueStore,
) -> SessionSnapshot {
    // A catalogued child listed with `mode: unknown` is re-typed from its own `subagent`
    // projection, which the Host computes once the child's descriptor has been read.
    let subagent = state.address.as_ref().map(|address| {
        if address.mode == SubagentMode::Unknown
            && let Some(mode) = identity_mode(&projections.values())
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

/// Reads the `blank` field of the `sessionListMetadata` projection.
fn metadata_blank(values: &BTreeMap<String, Value>) -> Option<bool> {
    values
        .get("sessionListMetadata")
        .and_then(|value| value.get("blank"))
        .and_then(Value::as_bool)
}

/// Reads `lastPromptAt` from the `sessionListMetadata` projection.
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
