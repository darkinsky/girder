//! The event window and Assistant-stream fold of upstream `packages/api/session-controller`: the
//! contiguous durable-plus-transient window a conversation renders, and the state machine that
//! decides when a live attempt's transient rows become a durable settlement.
//!
//! # Upstream capability classification
//!
//! `src/client/contract/events.ts` (205 lines) is one capability with four layers, not several
//! capabilities: an **observable contiguous window over `SessionEventLike` entries** with a
//! synchronous delta algebra.
//!
//! 1. **Entry vocabulary.** `AssistantLiveChunkEvent` (a client-only `assistant/live-chunk` row whose
//!    fractional `seq` orders it between durable seqs), `SessionEventLike` (`SessionEvent` or that
//!    live row), and `SessionEventLikeEntry`/`SessionLiveEventEntry`/`SessionTransientEventEntry`
//!    (the `{type:'event'}` versus `{type:'transient'}` discriminator).
//!    `SessionAssistantSettlementEntry` narrows a durable entry to the two Assistant events allowed
//!    to supersede an attempt.
//! 2. **Rope algebra.** `EventWindowLeaf`/`EventWindowConcat`, `leaf`, `concat`, `materialize` (an
//!    explicit-stack in-order walk), and `windowSnapshot` (a lazy `entries` getter).
//! 3. **Delta algebra.** `SessionEventChange` with four kinds — `replace`, `prepend`, `append`, and
//!    `settle-assistant` — each naming exactly the entries a mutation introduced.
//! 4. **The source.** `MutableSessionEventSource`: `getSnapshot`, `subscribe`, `replace`, `prepend`,
//!    `append`, `settleAssistant`, and the private `publish` that bumps `revision` and notifies
//!    synchronously.
//!
//! A second upstream file, `src/client/sessions/assistant-stream.ts` (229 lines), is what makes part
//! 4 usable: `ClientAssistantStream` consumes the process-local `start`/`chunk`/`end` frames plus
//! durable entries and returns a publication decision
//! (`publish`/`settlement`/`abandonment`/`transient`/`rebaseline`/`undefined`).
//!
//! # What this crate already covers
//!
//! Three sibling modules absorbed everything around these files, and none ported them:
//!
//! - [`girder_core::session_controller_gap`] — history paging, the tri-state cursor, address
//!   resolution, `merge_ordered_baseline`, `flatten_lineage`.
//! - [`girder_core::session_engine_gap`] — `ProjectionValueStore`, `Notifier`.
//! - [`girder_core::session_snapshot_gap`] — `SessionView`/`ListEngine`. It deliberately keeps only
//!   the *snapshot bits* window ownership implies (`has_more`, `history_was_partial`,
//!   `history_load_status`, the first-turn latch) and documents the window itself as out of scope.
//!
//! So layers 1–4 above and the fold were absent: nothing in this crate could answer "what is in the
//! conversation window right now, what changed, and is the live attempt still only a stream?".
//!
//! # Which remaining gap this closes, and which it cannot
//!
//! The subagent-224 report named two remaining gaps. They are not equally reachable from a new file.
//!
//! - **Inbox correlation** — settle a prompt echo against the `inbox` projection and the durable
//!   `user/message` / `agent/inbox/spliced` events. `agent/inbox/spliced` has **no variant** in this
//!   crate's [`girder_core::SessionEvent`], and that enum lives in the off-limits `session.rs`. The
//!   splice payload (`target`, `start`, `removedCount`, `inserted`, `outcome`) therefore cannot be
//!   decoded from a real session log here at all, and a borrow-only view would still have to guess
//!   at rows this crate cannot produce. This module does not fake it — see the port report.
//! - **Window algebra plus Assistant folding** — fully expressible with types that already exist
//!   ([`girder_core::ContentBlock`], [`girder_core::StreamChunk`], [`girder_core::SessionId`],
//!   [`girder_core::ToolCallId`]) and needing no change to the event vocabulary. That is this module.
//!
//! # Rules that are load-bearing, and why
//!
//! - **A live row has no durable seq, and an order key that keeps it inside its gap.** Upstream
//!   orders live rows by `durableCursor + 1 - 1/(n+1)`, a fraction strictly inside the gap below the
//!   next durable seq. [`SessionEntry::order_key`] encodes that position as `(floor, 1, rank)` — an
//!   integer triple that compares exactly like the fraction without floating point, and that cannot
//!   be mistaken for a durable position (durable rows are `(seq, 0, 0)`).
//! - **A settlement replaces the attempt's live rows, and only then splices itself in.** Dropping the
//!   live rows without inserting the durable message makes the answer vanish; inserting before
//!   filtering leaves a duplicate. [`EventWindow::settle_assistant`] filters first and then splices
//!   by durable position, reproducing what a replay of the settled log would render — the property
//!   upstream asserts as `settled.order === replay.order`.
//! - **A settlement for a foreign attempt is not a settlement.** Upstream treats an
//!   `assistant/message` as a settlement only when `surfaceOp === 'append'`, its `seq` is past
//!   `startedAfterSeq`, and its turn/step match the open attempt. Any other message is an ordinary
//!   durable publish, because a replayed or forked older message shares the shape but not the
//!   attempt.
//! - **A successful message keeps its live rows until the owning step ends.** The step's closing
//!   frames are laid out relative to the live attempt, so retiring early would change the layout
//!   mid-step. Interrupted messages, failed attempts, and abandonment retire immediately, because
//!   nothing will arrive to consume them.
//! - **A gap in the dense chunk index forces a rebaseline, not a guess.** A dropped frame silently
//!   corrupts the assembled stream; the only honest recovery is to drop the transient picture and
//!   re-open.
//! - **A controller never notifies while holding its own lock.** Listeners are cloned out of the
//!   table first, so a listener may read the window it is subscribed to and may unsubscribe itself
//!   without deadlocking or poisoning the batch.
//!
//! # Module-include note
//!
//! Both this file and its test are compiled by path (`#[path = "../src/session_window_gap.rs"]` from
//! `tests/session_window_gap.rs`) because the crate root's wiring is a separate change. Inside a
//! `#[path]` module `crate::` names the *test* crate, so every library reference here is spelled
//! `girder_core::…`.

#![allow(
    dead_code,
    reason = "the crate root's module wiring is a separate change; everything here is reached from \
              the by-path test target until then"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use girder_core::{ContentBlock, SessionId, StreamChunk, ToolCallId};

/// Locks a mutex, treating poisoning as a stale writer rather than a fatal state.
///
/// A poisoned window mutex means some thread panicked mid-write. The value it guards is one window
/// plus its derived bookkeeping; the next mutation replaces it wholesale, and a reader would rather
/// render the last consistent window than abort the process. This is the choice
/// [`girder_core::ProjectionValueStore`] already makes for its own rows.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Entry vocabulary (upstream: the types at the top of `contract/events.ts`)
// ---------------------------------------------------------------------------------------------

/// The gap value a live row carries before the window has placed it.
///
/// It is `u64::MAX` rather than `0` because `0` is a real floor — the gap before the first durable
/// event — and a replacement that treated every unplaced row as belonging there would render the live
/// tail before the whole conversation. Sorting an unplaced row last, then reassigning it to the
/// window's actual live tail, is upstream's behaviour: transient rows are reconstructed above
/// `durableCursor`, not before seq 1.
pub const UNPLACED_GAP: u64 = u64::MAX;

/// One attempt's identity, as the process-local Assistant stream knows it.
///
/// Upstream brands this as `LlmAttemptId`. It stays a plain [`String`] here for the same reason the
/// snapshot layer keys settlements by string: the id is minted per process and never crosses a
/// durable boundary in this crate, so a brand would add a validation rule with no producer.
pub type AttemptId = String;

/// Which durable event kind settles an Assistant attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettlementKind {
    /// `assistant/message`: a committed answer.
    #[serde(rename = "assistant/message")]
    Message,
    /// `assistant/attempt`: a durable attempt record with no committed message.
    #[serde(rename = "assistant/attempt")]
    Attempt,
}

impl SettlementKind {
    /// The event's `type` tag, as it appears in the durable log.
    #[must_use]
    pub fn type_name(self) -> &'static str {
        match self {
            Self::Message => "assistant/message",
            Self::Attempt => "assistant/attempt",
        }
    }
}

/// A durable Assistant event that atomically supersedes one attempt's transient rows.
///
/// Upstream's `SessionAssistantSettlementEntry` is an entry union narrowed to `assistant/message` or
/// `assistant/attempt`; only those two event types may settle a live attempt. This type is the
/// borrowed view the task allows: it describes the settlement by the fields the fold needs and never
/// invents a [`girder_core::SessionEvent`] variant to carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantSettlement {
    /// Durable position the settlement occupies.
    pub seq: u64,
    /// The attempt it settles.
    pub attempt_id: AttemptId,
    /// Turn the attempt belongs to.
    pub turn: u64,
    /// Step the attempt belongs to.
    pub step: u64,
    /// Which durable event kind settles the attempt.
    pub kind: SettlementKind,
    /// For `assistant/message`, whether the surface operation was `append`.
    ///
    /// A message surfaced by any other operation (a replacement, a fork) is not the settlement of
    /// *this* attempt, and treating it as one would splice an older answer into the live position.
    pub surface_append: bool,
    /// For `assistant/message`, whether the message is marked interrupted.
    ///
    /// An interrupted message is durable but carries no completed answer, so upstream retires the
    /// live rows immediately instead of waiting for the step to close.
    pub interrupted: bool,
}

impl AssistantSettlement {
    /// A settlement carried by an `assistant/message` event.
    #[must_use]
    pub fn message(seq: u64, attempt_id: impl Into<AttemptId>, turn: u64, step: u64) -> Self {
        Self {
            seq,
            attempt_id: attempt_id.into(),
            turn,
            step,
            kind: SettlementKind::Message,
            surface_append: true,
            interrupted: false,
        }
    }

    /// A settlement carried by an `assistant/attempt` event.
    #[must_use]
    pub fn attempt(seq: u64, attempt_id: impl Into<AttemptId>, turn: u64, step: u64) -> Self {
        Self {
            seq,
            attempt_id: attempt_id.into(),
            turn,
            step,
            kind: SettlementKind::Attempt,
            surface_append: true,
            interrupted: false,
        }
    }

    /// Marks the settlement as a non-`append` surface operation.
    #[must_use]
    pub fn with_surface_append(mut self, surface_append: bool) -> Self {
        self.surface_append = surface_append;
        self
    }

    /// Marks the settlement as an interrupted message.
    #[must_use]
    pub fn with_interrupted(mut self, interrupted: bool) -> Self {
        self.interrupted = interrupted;
        self
    }

    /// Whether this event is the settlement of `attempt`.
    ///
    /// The four conditions are upstream's `attemptForSettlement`: the attempt must be open, a
    /// message must have been surfaced by `append`, the seq must be past the attempt's
    /// `startedAfterSeq`, and the turn/step must match.
    #[must_use]
    pub fn settles(&self, attempt: &ActiveAttempt) -> bool {
        if self.kind == SettlementKind::Message && !self.surface_append {
            return false;
        }
        self.seq > attempt.started_after_seq
            && self.turn == attempt.turn
            && self.step == attempt.step
    }
}

/// A durable Session event, reduced to what the window orders and settles on.
///
/// The window does not own the full [`girder_core::SessionEvent`] vocabulary; it owns the *position*
/// and *settlement shape* of each row, which is all a window may assume about a log it did not
/// produce.
#[derive(Clone, Debug, PartialEq)]
pub struct DurableEntry {
    /// Durable position, one-based like every sequence in this crate.
    pub seq: u64,
    /// The event's `type` tag (`turn/start`, `assistant/message`, …).
    pub type_name: String,
    /// The durable row as JSON, when the caller has it.
    pub data: Option<Value>,
    /// The settlement this row carries, when it is an Assistant settlement.
    pub settlement: Option<AssistantSettlement>,
}

impl DurableEntry {
    /// An ordinary durable row with no settlement semantics.
    #[must_use]
    pub fn new(seq: u64, type_name: impl Into<String>) -> Self {
        Self {
            seq,
            type_name: type_name.into(),
            data: None,
            settlement: None,
        }
    }

    /// A durable row that settles an attempt.
    #[must_use]
    pub fn settling(settlement: AssistantSettlement) -> Self {
        Self {
            seq: settlement.seq,
            type_name: settlement.kind.type_name().to_owned(),
            data: None,
            settlement: Some(settlement),
        }
    }

    /// Attaches the durable row's JSON payload.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// The client-only live chunk row: upstream's `assistant/live-chunk`.
///
/// Upstream gives the row a fractional `seq` so it sorts *between* durable seqs. The fraction is a
/// presentation device, not an identity, so this type never stores it: [`SessionEntry`] records the
/// gap instead, and [`SessionEntry::order_key`] derives the position.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveChunk {
    /// The attempt producing the chunk.
    pub attempt_id: AttemptId,
    /// Turn the attempt belongs to.
    pub turn: u64,
    /// Step the attempt belongs to.
    pub step: u64,
    /// Original Session timestamp.
    pub time: u64,
    /// The raw chunk, exactly as it arrived.
    pub chunk: StreamChunk,
}

impl LiveChunk {
    /// A live chunk row.
    #[must_use]
    pub fn new(
        attempt_id: impl Into<AttemptId>,
        turn: u64,
        step: u64,
        time: u64,
        chunk: StreamChunk,
    ) -> Self {
        Self {
            attempt_id: attempt_id.into(),
            turn,
            step,
            time,
            chunk,
        }
    }
}

/// One entry of the conversation window.
///
/// Upstream's `SessionEventLikeEntry` discriminates on `type: 'event' | 'transient'`. The durable
/// side additionally keeps the owning session, because a window in this crate is addressable: the
/// same log can be open twice through different routes, and a snapshot without its session identity
/// cannot be reconciled with the store keyed by it.
///
/// A live row records the *gap* it fills: `floor` is the durable seq it sits above, and `rank` its
/// position among the live rows of that gap. The floor is what keeps two gaps from interleaving —
/// without it, two live rows at rank 0 in different gaps would compare equal.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEntry {
    /// A durable Session event.
    Event {
        /// The session whose log holds it.
        session: SessionId,
        /// The durable row.
        durable: DurableEntry,
    },
    /// A client-only live chunk, ordered inside the gap above `floor`.
    Transient {
        /// The session the attempt belongs to.
        session: SessionId,
        /// The live row.
        live: LiveChunk,
        /// Durable seq this row sits above; `0` means before the first durable event.
        floor: u64,
        /// Position among the live rows of this gap, from 0.
        rank: u64,
    },
}

impl SessionEntry {
    /// A durable entry.
    #[must_use]
    pub fn event(session: SessionId, durable: DurableEntry) -> Self {
        Self::Event { session, durable }
    }

    /// A live row with no gap assigned yet.
    ///
    /// The window resolves it onto the live tail — the gap above its highest durable seq — which is
    /// where upstream reconstructs transient rows on a replacement
    /// (`durableCursor + 1 - 1/(n+1)`). Use [`SessionEntry::in_gap`] when the row belongs to an
    /// older, already-paged gap.
    #[must_use]
    pub fn live(session: SessionId, live: LiveChunk) -> Self {
        Self::Transient {
            session,
            live,
            floor: UNPLACED_GAP,
            rank: 0,
        }
    }

    /// A live row belonging to the gap above `floor`.
    #[must_use]
    pub fn in_gap(session: SessionId, live: LiveChunk, floor: u64) -> Self {
        Self::Transient {
            session,
            live,
            floor,
            rank: 0,
        }
    }

    /// The entry's transport discriminator: `"event"` or `"transient"`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Event { .. } => "event",
            Self::Transient { .. } => "transient",
        }
    }

    /// The session the entry belongs to.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        match self {
            Self::Event { session, .. } | Self::Transient { session, .. } => session,
        }
    }

    /// The durable position, for durable entries only.
    ///
    /// A live row deliberately has none: it has not been written, and answering a number here would
    /// let a caller fold it into a durable cursor.
    #[must_use]
    pub fn durable_seq(&self) -> Option<u64> {
        match self {
            Self::Event { durable, .. } => Some(durable.seq),
            Self::Transient { .. } => None,
        }
    }

    /// The total order of the window.
    ///
    /// Durable rows sort by seq alone. A live row sorts immediately *after* the durable row at its
    /// floor and before the next durable seq, which is exactly where upstream's
    /// `base + 1 - 1/(n+1)` fraction places it: `(floor, 1, rank)` sits after `(floor, 0, 0)` and
    /// before `(floor + 1, 0, 0)`.
    ///
    /// An unplaced row ([`UNPLACED_GAP`]) orders as if it sat above `u64::MAX`, so it lands last until
    /// [`place`] resolves it onto the live tail. That resolution is what makes a replacement
    /// reconstruct transient rows in upstream's position instead of before every durable entry.
    #[must_use]
    pub fn order_key(&self) -> (u64, u8, u64) {
        match self {
            Self::Event { durable, .. } => (durable.seq, 0, 0),
            Self::Transient { floor, rank, .. } => (*floor, 1, *rank),
        }
    }

    /// The attempt a row belongs to, for retirement.
    #[must_use]
    pub fn attempt_id(&self) -> Option<&AttemptId> {
        match self {
            Self::Event { durable, .. } => durable
                .settlement
                .as_ref()
                .map(|settlement| &settlement.attempt_id),
            Self::Transient { live, .. } => Some(&live.attempt_id),
        }
    }

    /// The live chunk, when this is a transient row.
    #[must_use]
    pub fn live_chunk(&self) -> Option<&LiveChunk> {
        match self {
            Self::Transient { live, .. } => Some(live),
            Self::Event { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Delta algebra (upstream: `SessionEventChange`)
// ---------------------------------------------------------------------------------------------

/// The exact delta that produced the latest window revision.
///
/// Upstream's four kinds are preserved one-for-one. Entries are carried by value rather than by
/// reference so a listener may keep the delta after the window moves on without cloning the window.
#[derive(Clone, Debug, PartialEq)]
pub enum EventWindowChange {
    /// The complete window was replaced.
    Replace {
        /// The new window.
        entries: Vec<SessionEntry>,
    },
    /// One older contiguous page was prepended.
    Prepend {
        /// The newly loaded older entries.
        entries: Vec<SessionEntry>,
    },
    /// One contiguous live entry was appended.
    Append {
        /// The appended entry.
        entry: SessionEntry,
    },
    /// One attempt's transient rows were replaced by its committed settlement.
    SettleAssistant {
        /// The attempt whose live rows are gone.
        attempt_id: AttemptId,
        /// The durable settlement, when there was one.
        entry: Option<SessionEntry>,
    },
}

impl EventWindowChange {
    /// The change's `kind` tag, as upstream spells it (`settle-assistant` is kebab-case).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Replace { .. } => "replace",
            Self::Prepend { .. } => "prepend",
            Self::Append { .. } => "append",
            Self::SettleAssistant { .. } => "settle-assistant",
        }
    }

    /// Every entry the delta introduced, in window order.
    #[must_use]
    pub fn introduced(&self) -> Vec<&SessionEntry> {
        match self {
            Self::Replace { entries } | Self::Prepend { entries } => entries.iter().collect(),
            Self::Append { entry } => vec![entry],
            Self::SettleAssistant { entry, .. } => entry.iter().collect(),
        }
    }

    /// How many entries the delta introduced.
    #[must_use]
    pub fn introduced_len(&self) -> usize {
        match self {
            Self::Replace { entries } | Self::Prepend { entries } => entries.len(),
            Self::Append { .. } => 1,
            Self::SettleAssistant { entry, .. } => usize::from(entry.is_some()),
        }
    }
}

/// The current contiguous window and its latest synchronous delta.
#[derive(Clone, Debug, PartialEq)]
pub struct EventWindowSnapshot {
    /// Every entry, in window order.
    pub entries: Vec<SessionEntry>,
    /// Whether older history remains below the window.
    pub has_more: bool,
    /// Monotone revision, bumped once per accepted mutation.
    pub revision: u64,
    /// The delta that produced this revision.
    pub change: EventWindowChange,
}

// ---------------------------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------------------------

/// One subscribed listener.
///
/// `Arc` rather than `Box` so a publication can clone the table's handles under the lock, release
/// it, and only then run user code. A plain `Box` could only be called while the lock was held,
/// which would deadlock the first listener that reads the window it watches.
pub type Listener = Arc<dyn Fn() + Send + Sync>;

/// Mutable state behind the window's mutex.
#[derive(Debug)]
struct WindowState {
    entries: Vec<SessionEntry>,
    has_more: bool,
    revision: u64,
    change: EventWindowChange,
    /// Whether the first window ever installed was incomplete; sticky across replacements.
    history_was_partial: bool,
    /// Durable seqs already present, so a redelivered frame is a no-op.
    durable: BTreeSet<u64>,
}

/// An observable, contiguous, durable-plus-transient Session window.
///
/// Upstream's `MutableSessionEventSource` is a rope (`leaf`/`concat`) with a lazily materialised
/// `entries` getter, so prepending two hundred pages does not copy the tail each time. This port
/// keeps the algebra and drops the rope: a window here is materialised eagerly, because every
/// consumer in this crate reads the whole window at once and real windows are paged in tens of
/// entries. What is preserved is everything a view layer can observe — the order, the revision, the
/// delta, and the synchronous publication.
pub struct EventWindow {
    inner: Arc<Mutex<WindowState>>,
    listeners: Arc<Mutex<BTreeMap<u64, Listener>>>,
    next_listener: Arc<Mutex<u64>>,
}

impl std::fmt::Debug for EventWindow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.inner);
        formatter
            .debug_struct("EventWindow")
            .field("entries", &state.entries.len())
            .field("has_more", &state.has_more)
            .field("revision", &state.revision)
            .field("history_was_partial", &state.history_was_partial)
            .field("change", &state.change.kind())
            .field("listeners", &lock(&self.listeners).len())
            .field("next_listener", &lock(&self.next_listener))
            .finish()
    }
}

impl Default for EventWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl EventWindow {
    /// An empty window at revision 0 with a `replace` delta over no entries.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(WindowState {
                entries: Vec::new(),
                has_more: false,
                revision: 0,
                change: EventWindowChange::Replace {
                    entries: Vec::new(),
                },
                history_was_partial: false,
                durable: BTreeSet::new(),
            })),
            listeners: Arc::new(Mutex::new(BTreeMap::new())),
            next_listener: Arc::new(Mutex::new(0)),
        }
    }

    /// Subscribes to synchronous publication.
    ///
    /// The listener runs inside the mutating call, after the new snapshot is installed — upstream's
    /// `notifySubscribers` contract. It is called with no window lock held, so it may read
    /// [`EventWindow::snapshot`]; it must not mutate the window it is listening to.
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> u64 {
        let mut next = lock(&self.next_listener);
        let id = *next;
        *next += 1;
        drop(next);
        lock(&self.listeners).insert(id, Arc::new(listener));
        id
    }

    /// Removes a subscription.
    pub fn unsubscribe(&self, id: u64) {
        lock(&self.listeners).remove(&id);
    }

    /// The number of live subscriptions.
    #[must_use]
    pub fn listener_count(&self) -> usize {
        lock(&self.listeners).len()
    }

    /// The cached snapshot.
    #[must_use]
    pub fn snapshot(&self) -> EventWindowSnapshot {
        let state = lock(&self.inner);
        EventWindowSnapshot {
            entries: state.entries.clone(),
            has_more: state.has_more,
            revision: state.revision,
            change: state.change.clone(),
        }
    }

    /// The current entries, without the delta.
    #[must_use]
    pub fn entries(&self) -> Vec<SessionEntry> {
        lock(&self.inner).entries.clone()
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.inner).entries.len()
    }

    /// Whether the window holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        lock(&self.inner).entries.is_empty()
    }

    /// The current revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        lock(&self.inner).revision
    }

    /// Whether older history remains below the window.
    #[must_use]
    pub fn has_more(&self) -> bool {
        lock(&self.inner).has_more
    }

    /// Whether this client ever exposed an incomplete window; sticky across replacements.
    #[must_use]
    pub fn history_was_partial(&self) -> bool {
        lock(&self.inner).history_was_partial
    }

    /// The durable seqs currently in the window, ascending.
    #[must_use]
    pub fn durable_seqs(&self) -> Vec<u64> {
        let state = lock(&self.inner);
        state
            .entries
            .iter()
            .filter_map(SessionEntry::durable_seq)
            .collect()
    }

    /// The highest durable seq in the window, or `None` when it holds none.
    #[must_use]
    pub fn durable_cursor(&self) -> Option<u64> {
        lock(&self.inner)
            .entries
            .iter()
            .filter_map(SessionEntry::durable_seq)
            .max()
    }

    /// Replaces the complete contiguous window.
    ///
    /// Entries are re-ordered by [`SessionEntry::order_key`], live rows are re-based onto the gap
    /// they now occupy, and durable rows the window already holds are dropped. A replacement resets
    /// the gap bookkeeping, because the live rows of a previous window are gone with it.
    pub fn replace(&self, entries: Vec<SessionEntry>, has_more: bool) {
        let mut state = lock(&self.inner);
        state.durable.clear();
        let mut kept = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(seq) = entry.durable_seq() {
                if !state.durable.insert(seq) {
                    continue;
                }
            }
            kept.push(entry);
        }
        place(&mut kept);
        state.entries = kept;
        state.has_more = has_more;
        if has_more {
            state.history_was_partial = true;
        }
        state.change = EventWindowChange::Replace {
            entries: state.entries.clone(),
        };
        state.revision += 1;
        drop(state);
        publish(&self.listeners);
    }

    /// Prepends one older contiguous page.
    ///
    /// `has_more` describes the window *after* the prepend, not the page: a page that reaches the
    /// log start clears it. Durable rows the window already holds are dropped, so a retried page is
    /// idempotent rather than duplicated.
    pub fn prepend(&self, entries: Vec<SessionEntry>, has_more: bool) {
        let mut state = lock(&self.inner);
        let mut added = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(seq) = entry.durable_seq() {
                if !state.durable.insert(seq) {
                    continue;
                }
            }
            added.push(entry);
        }
        let mut combined = added.clone();
        combined.append(&mut state.entries);
        place(&mut combined);
        state.entries = combined;
        state.has_more = has_more;
        if has_more {
            state.history_was_partial = true;
        }
        state.change = EventWindowChange::Prepend { entries: added };
        state.revision += 1;
        drop(state);
        publish(&self.listeners);
    }

    /// Appends one entry.
    ///
    /// Returns `false` without publishing when the entry is a durable row the window already holds:
    /// a reconnect can redeliver a frame, and replaying it must be a no-op rather than a duplicate.
    pub fn append(&self, entry: SessionEntry) -> bool {
        let mut state = lock(&self.inner);
        if let Some(seq) = entry.durable_seq() {
            if !state.durable.insert(seq) {
                return false;
            }
        }
        state.entries.push(entry.clone());
        let mut combined = std::mem::take(&mut state.entries);
        place(&mut combined);
        state.entries = combined;
        state.change = EventWindowChange::Append { entry };
        state.revision += 1;
        drop(state);
        publish(&self.listeners);
        true
    }

    /// Appends one live chunk into the gap above the newest durable event.
    ///
    /// This is the only correct place for a live row: the Host is still writing `seq`-ordered
    /// events, and a chunk that arrived after durable seq N and before N+1 belongs between them.
    /// A window with no durable entry yet uses floor 0, which sorts before seq 1.
    pub fn append_live(&self, session: SessionId, live: LiveChunk) -> bool {
        let floor = self.durable_cursor().unwrap_or(0);
        self.append(SessionEntry::Transient {
            session,
            live,
            floor,
            rank: 0,
        })
    }

    /// Replaces one attempt's transient rows with its committed settlement.
    ///
    /// Every live row of `attempt_id` is dropped, then `entry` is spliced in at its durable position
    /// — not appended, because a settlement's seq predates durable rows that arrived while the
    /// stream was live, and appending it would place the answer after the tool results it produced.
    pub fn settle_assistant(&self, attempt_id: &str, entry: Option<SessionEntry>) {
        let mut state = lock(&self.inner);
        state.entries.retain(|candidate| match candidate {
            SessionEntry::Transient { live, .. } => live.attempt_id != attempt_id,
            SessionEntry::Event { .. } => true,
        });
        if let Some(settlement) = &entry {
            if let Some(seq) = settlement.durable_seq() {
                state.durable.insert(seq);
            }
        }
        let inserted = entry.clone();
        if let Some(settlement) = entry {
            let position = settlement.durable_seq().map_or(state.entries.len(), |seq| {
                state
                    .entries
                    .iter()
                    .position(|candidate| candidate.durable_seq().is_some_and(|other| other > seq))
                    .unwrap_or(state.entries.len())
            });
            state.entries.insert(position, settlement);
        }
        let mut combined = std::mem::take(&mut state.entries);
        place(&mut combined);
        state.entries = combined;
        state.change = EventWindowChange::SettleAssistant {
            attempt_id: attempt_id.to_owned(),
            entry: inserted,
        };
        state.revision += 1;
        drop(state);
        publish(&self.listeners);
    }

    /// Retires one attempt's live rows without installing a settlement.
    ///
    /// This is the window-side half of an abandonment: the process-local attempt ended without a
    /// durable row, so its presentation must go and nothing may replace it. A retirement that
    /// removes nothing publishes nothing, so an abandoned attempt whose chunks never became visible
    /// does not bump the revision.
    pub fn retire_attempt(&self, attempt_id: &str) -> usize {
        let mut state = lock(&self.inner);
        let before = state.entries.len();
        state.entries.retain(|candidate| match candidate {
            SessionEntry::Transient { live, .. } => live.attempt_id != attempt_id,
            SessionEntry::Event { .. } => true,
        });
        let removed = before.saturating_sub(state.entries.len());
        if removed == 0 {
            return 0;
        }
        state.change = EventWindowChange::SettleAssistant {
            attempt_id: attempt_id.to_owned(),
            entry: None,
        };
        state.revision += 1;
        drop(state);
        publish(&self.listeners);
        removed
    }
}

/// Sorts a window, resolves unplaced live rows onto the live tail, and re-bases every gap.
///
/// Three steps, in this order:
///
/// 1. **Sort** by [`SessionEntry::order_key`]. Unplaced rows carry [`UNPLACED_GAP`] and land last.
/// 2. **Resolve.** An unplaced row joins the tail gap — the gap above the window's highest durable
///    seq — preserving arrival order among rows that were already in that gap. This is where a
///    replacement's transient rows land, matching upstream's `durableCursor`-anchored reconstruction.
/// 3. **Re-densify.** Every live row's rank is renumbered from 0 within its gap, so a settlement that
///    removed rows from the middle of a gap leaves no hole for a later sort to trip over.
fn place(entries: &mut [SessionEntry]) {
    entries.sort_by_key(SessionEntry::order_key);
    let tail = entries
        .iter()
        .filter_map(SessionEntry::durable_seq)
        .max()
        .unwrap_or(0);
    for entry in entries.iter_mut() {
        if let SessionEntry::Transient { floor, .. } = entry {
            if *floor == UNPLACED_GAP {
                *floor = tail;
            }
        }
    }
    // Resolving may have moved a row across a durable neighbour, so restore the order before ranking.
    entries.sort_by_key(SessionEntry::order_key);
    let mut run: Option<u64> = None;
    let mut rank = 0u64;
    for entry in entries.iter_mut() {
        match entry {
            SessionEntry::Event { .. } => run = None,
            SessionEntry::Transient {
                floor,
                rank: position,
                ..
            } => {
                if run != Some(*floor) {
                    run = Some(*floor);
                    rank = 0;
                }
                *position = rank;
                rank += 1;
            }
        }
    }
}

/// Runs every listener against the window's current state.
///
/// The table is cloned out of the mutex first, so no lock is held while user code runs: a listener
/// may call [`EventWindow::snapshot`] and may [`EventWindow::unsubscribe`] itself. A listener that
/// subscribes during publication is not called in that same batch, matching upstream's
/// `notifySubscribers`, which iterates the set it snapshotted.
fn publish(listeners: &Arc<Mutex<BTreeMap<u64, Listener>>>) {
    let snapshot: Vec<Listener> = {
        let table = lock(listeners);
        table.values().cloned().collect()
    };
    for listener in snapshot {
        listener();
    }
}

// ---------------------------------------------------------------------------------------------
// The Assistant-stream fold (upstream: `src/client/sessions/assistant-stream.ts`)
// ---------------------------------------------------------------------------------------------

/// One process-local Assistant stream frame, as the follow connection delivers it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AssistantStreamFrame {
    /// One attempt began producing chunks.
    Start {
        /// The attempt.
        #[serde(rename = "attemptId")]
        attempt_id: AttemptId,
        /// Durable seq it began after.
        #[serde(rename = "startedAfterSeq")]
        started_after_seq: u64,
        /// Turn it belongs to.
        turn: u64,
        /// Step it belongs to.
        step: u64,
    },
    /// One dense chunk of the open attempt.
    Chunk {
        /// The attempt.
        #[serde(rename = "attemptId")]
        attempt_id: AttemptId,
        /// Dense position, expected in order.
        index: u64,
        /// Original Session timestamp.
        time: u64,
        /// The raw chunk.
        chunk: StreamChunk,
    },
    /// The attempt ended, carrying the durable row that settles it.
    End {
        /// The attempt.
        #[serde(rename = "attemptId")]
        attempt_id: AttemptId,
        /// Number of chunk frames the terminal marker accounts for.
        index: u64,
        /// How it ended.
        outcome: AssistantStreamOutcome,
    },
}

/// How an Assistant attempt ended, from the process-local stream's point of view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AssistantStreamOutcome {
    /// A durable row settled it.
    Committed {
        /// Which durable event kind carries the settlement.
        #[serde(rename = "eventType")]
        event_type: SettlementKind,
        /// Durable position of the settlement.
        seq: u64,
    },
    /// It produced no durable row.
    Abandoned,
}

/// One Web publication decision from the Assistant stream fold.
///
/// Upstream's `ClientAssistantStreamResult`. Upstream's `undefined` is modelled here as
/// [`FoldDecision::Quiet`] rather than `Option`, because "nothing becomes visible" is a decision the
/// caller must handle, and an `Option` invites an `unwrap` that silently drops it.
#[derive(Clone, Debug, PartialEq)]
pub enum FoldDecision {
    /// Publish one durable entry.
    Publish {
        /// The entry.
        entry: SessionEntry,
        /// Retire this attempt's live rows after publishing its step end.
        retire_attempt_id: Option<AttemptId>,
    },
    /// Retire one attempt's live rows in favour of a durable settlement.
    Settlement {
        /// The attempt.
        attempt_id: AttemptId,
        /// The durable entry.
        entry: SessionEntry,
    },
    /// Retire one attempt's live rows with no settlement.
    Abandonment {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// Show one live chunk; nothing durable becomes visible.
    Transient {
        /// The live row.
        entry: SessionEntry,
    },
    /// The transient picture can no longer be trusted; drop it and re-open.
    Rebaseline,
    /// Nothing becomes visible.
    Quiet,
}

impl FoldDecision {
    /// The decision's kind, for assertions and logs.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Publish { .. } => "publish",
            Self::Settlement { .. } => "settlement",
            Self::Abandonment { .. } => "abandonment",
            Self::Transient { .. } => "transient",
            Self::Rebaseline => "rebaseline",
            Self::Quiet => "quiet",
        }
    }

    /// The entry this decision makes visible, when it makes one.
    #[must_use]
    pub fn entry(&self) -> Option<&SessionEntry> {
        match self {
            Self::Publish { entry, .. }
            | Self::Settlement { entry, .. }
            | Self::Transient { entry } => Some(entry),
            Self::Abandonment { .. } | Self::Rebaseline | Self::Quiet => None,
        }
    }
}

/// One attempt the fold is currently tracking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAttempt {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// Durable seq it began after; a settlement at or before this seq is not its settlement.
    pub started_after_seq: u64,
    /// Turn it belongs to.
    pub turn: u64,
    /// Step it belongs to.
    pub step: u64,
    /// Dense position expected for the next chunk frame.
    pub next_index: u64,
}

/// The fold joining transient Assistant frames to one durable v2 settlement.
///
/// Upstream's `ClientAssistantStream`. The state machine's point is that a live attempt's rows are a
/// *presentation* of a stream the Host is also writing durably, and the two must converge on exactly
/// the rows a replay of the settled log would produce.
pub struct ClientAssistantStream {
    session: SessionId,
    active_attempt: Option<ActiveAttempt>,
    retained_attempt: Option<(AttemptId, u64, u64)>,
    pending: BTreeMap<u64, AssistantSettlement>,
    published_seqs: BTreeSet<u64>,
    durable_cursor: Option<u64>,
    transient_in_gap: u64,
}

impl std::fmt::Debug for ClientAssistantStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientAssistantStream")
            .field("session", &self.session)
            .field(
                "active",
                &self.active_attempt.as_ref().map(|a| &a.attempt_id),
            )
            .field("retained", &self.retained_attempt.as_ref().map(|r| &r.0))
            .field("pending", &self.pending.len())
            .field("published", &self.published_seqs.len())
            .field("durable_cursor", &self.durable_cursor)
            .field("transient_in_gap", &self.transient_in_gap)
            .finish()
    }
}

impl ClientAssistantStream {
    /// A fold for one session, with no attempt open.
    #[must_use]
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            active_attempt: None,
            retained_attempt: None,
            pending: BTreeMap::new(),
            published_seqs: BTreeSet::new(),
            durable_cursor: None,
            transient_in_gap: 0,
        }
    }

    /// The session this fold belongs to.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The open attempt, when there is one.
    #[must_use]
    pub fn active_attempt(&self) -> Option<&ActiveAttempt> {
        self.active_attempt.as_ref()
    }

    /// Settlements staged but not yet released.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// The highest durable seq the fold has admitted.
    #[must_use]
    pub fn durable_cursor(&self) -> Option<u64> {
        self.durable_cursor
    }

    /// Replaces the durable window.
    ///
    /// Every staged settlement and every live row is dropped: they belong to the window being
    /// replaced. The published set is reseeded from the new entries so a settlement the replacement
    /// already contains is not published a second time.
    pub fn replace(&mut self, entries: &[SessionEntry]) {
        self.pending.clear();
        self.transient_in_gap = 0;
        self.active_attempt = None;
        self.retained_attempt = None;
        self.published_seqs = entries
            .iter()
            .filter_map(SessionEntry::durable_seq)
            .collect();
        self.durable_cursor = self.published_seqs.iter().next_back().copied();
    }

    /// Stages one durable event, releasing its settlement when the matching attempt is open.
    ///
    /// A durable settlement for the open attempt is *staged*, not published: the stream's own `end`
    /// frame names which seq terminates the attempt, and publishing early would show the answer
    /// before the chunks that led to it were retired.
    pub fn accept_durable(&mut self, entry: SessionEntry) -> FoldDecision {
        let SessionEntry::Event { durable, .. } = &entry else {
            return FoldDecision::Rebaseline;
        };
        let seq = durable.seq;
        self.durable_cursor = Some(self.durable_cursor.map_or(seq, |cursor| cursor.max(seq)));
        self.transient_in_gap = 0;
        let staging = durable
            .settlement
            .as_ref()
            .filter(|settlement| {
                self.active_attempt
                    .as_ref()
                    .is_some_and(|attempt| settlement.settles(attempt))
            })
            .cloned();
        if let Some(settlement) = staging {
            if self.pending.insert(seq, settlement).is_some() {
                // The same settlement arrived twice. Which copy is authoritative cannot be known,
                // so re-open instead of guessing.
                return FoldDecision::Rebaseline;
            }
            return FoldDecision::Quiet;
        }
        self.publish(entry)
    }

    /// Folds one dense transient frame and releases its named durable settlement.
    pub fn accept_frame(&mut self, frame: AssistantStreamFrame) -> FoldDecision {
        match frame {
            AssistantStreamFrame::Start {
                attempt_id,
                started_after_seq,
                turn,
                step,
            } => {
                if self.active_attempt.is_some()
                    || self.retained_attempt.is_some()
                    || !self.pending.is_empty()
                {
                    return FoldDecision::Rebaseline;
                }
                self.pending.clear();
                self.active_attempt = Some(ActiveAttempt {
                    attempt_id,
                    started_after_seq,
                    turn,
                    step,
                    next_index: 0,
                });
                FoldDecision::Quiet
            }
            AssistantStreamFrame::Chunk {
                attempt_id,
                index,
                time,
                chunk,
            } => {
                let Some(attempt) = self.active_attempt.as_mut() else {
                    // A controller mounted after the Host began the attempt has no start frame to
                    // reconstruct. Its durable settlement publishes directly; the transient suffix
                    // is presentation-only and is dropped rather than guessed at.
                    return FoldDecision::Quiet;
                };
                if attempt.attempt_id != attempt_id {
                    return FoldDecision::Quiet;
                }
                if index != attempt.next_index {
                    return FoldDecision::Rebaseline;
                }
                attempt.next_index += 1;
                let turn = attempt.turn;
                let step = attempt.step;
                let floor = self.durable_cursor.unwrap_or(0);
                let rank = self.transient_in_gap;
                self.transient_in_gap += 1;
                FoldDecision::Transient {
                    entry: SessionEntry::Transient {
                        session: self.session.clone(),
                        live: LiveChunk::new(attempt_id, turn, step, time, chunk),
                        floor,
                        rank,
                    },
                }
            }
            AssistantStreamFrame::End {
                attempt_id,
                index,
                outcome,
            } => self.end_attempt(&attempt_id, index, &outcome),
        }
    }

    /// Handles a terminal frame for one attempt.
    fn end_attempt(
        &mut self,
        attempt_id: &str,
        index: u64,
        outcome: &AssistantStreamOutcome,
    ) -> FoldDecision {
        let Some(attempt) = self.active_attempt.take() else {
            return FoldDecision::Quiet;
        };
        if attempt.attempt_id != attempt_id {
            // A terminal frame for someone else's attempt: put ours back and ignore theirs.
            self.active_attempt = Some(attempt);
            return FoldDecision::Quiet;
        }
        if index != attempt.next_index {
            return FoldDecision::Rebaseline;
        }
        match outcome {
            AssistantStreamOutcome::Abandoned => {
                if self.pending.is_empty() {
                    FoldDecision::Abandonment {
                        attempt_id: attempt.attempt_id,
                    }
                } else {
                    // A durable settlement is staged that nothing can release: the attempt is gone
                    // but the log says it committed. The two pictures cannot be reconciled here.
                    FoldDecision::Rebaseline
                }
            }
            AssistantStreamOutcome::Committed { event_type, seq } => {
                if self.published_seqs.contains(seq) {
                    return FoldDecision::Quiet;
                }
                let Some(settlement) = self.pending.get(seq).cloned() else {
                    return FoldDecision::Rebaseline;
                };
                if settlement.kind != *event_type {
                    return FoldDecision::Rebaseline;
                }
                self.pending.remove(seq);
                let entry = self.settlement_entry(&settlement);
                if settlement.kind == SettlementKind::Message && !settlement.interrupted {
                    // The step's closing frames are laid out relative to this attempt, so its live
                    // rows stay until the step ends.
                    self.retained_attempt =
                        Some((attempt.attempt_id.clone(), attempt.turn, attempt.step));
                    return self.publish(entry);
                }
                self.published_seqs.insert(settlement.seq);
                FoldDecision::Settlement {
                    attempt_id: attempt.attempt_id,
                    entry,
                }
            }
        }
    }

    /// Publishes one durable entry, retiring a retained attempt when its step ends.
    fn publish(&mut self, entry: SessionEntry) -> FoldDecision {
        if let Some(seq) = entry.durable_seq() {
            self.published_seqs.insert(seq);
        }
        let retire_attempt_id = match (&self.retained_attempt, &entry) {
            (Some((attempt_id, turn, step)), SessionEntry::Event { durable, .. })
                if durable.type_name == "step/end"
                    && turn_step(durable.data.as_ref()).is_some_and(
                        |(event_turn, event_step)| event_turn == *turn && event_step == *step,
                    ) =>
            {
                let id = attempt_id.clone();
                self.retained_attempt = None;
                Some(id)
            }
            _ => None,
        };
        FoldDecision::Publish {
            entry,
            retire_attempt_id,
        }
    }

    /// Wraps a settlement as a window entry.
    fn settlement_entry(&self, settlement: &AssistantSettlement) -> SessionEntry {
        SessionEntry::Event {
            session: self.session.clone(),
            durable: DurableEntry::settling(settlement.clone()),
        }
    }
}

/// Reads a `{turn, step}` pair out of a durable row's JSON payload.
fn turn_step(data: Option<&Value>) -> Option<(u64, u64)> {
    let data = data?;
    let turn = data.get("turn")?.as_u64()?;
    let step = data.get("step")?.as_u64()?;
    Some((turn, step))
}

// ---------------------------------------------------------------------------------------------
// Folding a stream back into blocks
// ---------------------------------------------------------------------------------------------

/// Why a chunk stream could not be folded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamFoldError {
    /// A delta named a block that no `block-start` introduced.
    MissingIndex {
        /// The block index.
        index: usize,
    },
    /// A `block-end` named a block that was not open, or ended a block twice.
    OutOfOrder {
        /// The block index.
        index: usize,
    },
    /// A tool-call block carried no delta, so it has neither a call id nor arguments.
    IncompleteToolCall {
        /// The block index.
        index: usize,
    },
}

impl std::fmt::Display for StreamFoldError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingIndex { index } => {
                write!(
                    formatter,
                    "chunk delta names block {index}, which was never started"
                )
            }
            Self::OutOfOrder { index } => {
                write!(formatter, "block {index} ended without being open")
            }
            Self::IncompleteToolCall { index } => {
                write!(formatter, "tool-call block {index} ended without any delta")
            }
        }
    }
}

impl std::error::Error for StreamFoldError {}

/// Folds a run of live chunks into the blocks they describe.
///
/// The window holds chunks; a renderer needs blocks. Upstream builds this expansion from the compact
/// `AssistantStreamRecord` form. This port takes the chunks the window already holds, so the fold and
/// the presentation cannot disagree about which chunks exist.
///
/// The rules follow the crate's existing assembly: `text-delta` and `reasoning-delta` append to their
/// block, `tool-call-delta` appends to the call's raw JSON arguments and adopts a name when it carries
/// one, and `block-end` is authoritative — the block it states replaces anything accumulated,
/// because the provider is the last word on what it produced.
///
/// # Errors
///
/// [`StreamFoldError::MissingIndex`] when a delta names a block no `block-start` introduced,
/// [`StreamFoldError::OutOfOrder`] when a block ends without being open, and
/// [`StreamFoldError::IncompleteToolCall`] when a tool-call block closes with no delta at all. Each
/// means the chunk stream is corrupt and none is recoverable by guessing.
pub fn fold_stream<'a>(
    chunks: impl IntoIterator<Item = &'a StreamChunk>,
) -> Result<Vec<ContentBlock>, StreamFoldError> {
    /// What one block index has accumulated so far.
    #[derive(Debug)]
    enum Slot {
        Open {
            block_type: String,
            text: String,
            call: Option<(ToolCallId, String, String)>,
        },
        Closed(ContentBlock),
    }

    let mut slots: BTreeMap<usize, Slot> = BTreeMap::new();
    let mut order: Vec<usize> = Vec::new();
    for chunk in chunks {
        match chunk {
            StreamChunk::BlockStart { index, block_type } => {
                if slots
                    .insert(
                        *index,
                        Slot::Open {
                            block_type: block_type.clone(),
                            text: String::new(),
                            call: None,
                        },
                    )
                    .is_none()
                {
                    order.push(*index);
                }
            }
            StreamChunk::TextDelta { index, text }
            | StreamChunk::ReasoningDelta { index, text } => {
                let Some(Slot::Open { text: buffer, .. }) = slots.get_mut(index) else {
                    return Err(StreamFoldError::MissingIndex { index: *index });
                };
                buffer.push_str(text);
            }
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let Some(Slot::Open { call, .. }) = slots.get_mut(index) else {
                    return Err(StreamFoldError::MissingIndex { index: *index });
                };
                let slot = call.get_or_insert_with(|| (id.clone(), String::new(), String::new()));
                if let Some(name) = name {
                    slot.1.clone_from(name);
                }
                slot.2.push_str(arguments_delta);
            }
            StreamChunk::BlockEnd { index, block } => {
                if !matches!(slots.get(index), Some(Slot::Open { .. })) {
                    return Err(StreamFoldError::OutOfOrder { index: *index });
                }
                slots.insert(*index, Slot::Closed(block.clone()));
            }
            StreamChunk::Usage(_) | StreamChunk::Finish { .. } => {}
        }
    }

    let mut blocks = Vec::with_capacity(order.len());
    for index in order {
        let Some(slot) = slots.remove(&index) else {
            continue;
        };
        match slot {
            Slot::Closed(block) => blocks.push(block),
            Slot::Open {
                block_type,
                text,
                call,
            } => match block_type.as_str() {
                "reasoning" => blocks.push(ContentBlock::Reasoning { text }),
                "tool-call" => {
                    let Some((id, name, arguments)) = call else {
                        return Err(StreamFoldError::IncompleteToolCall { index });
                    };
                    blocks.push(ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                _ => blocks.push(ContentBlock::Text { text }),
            },
        }
    }
    Ok(blocks)
}
