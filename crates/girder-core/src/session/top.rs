//! The top-level Host producers of `packages/api/session-controller` that nothing in this crate
//! wrote: the list-metadata projection, the continuation projection, and the desktop/session gate
//! arithmetic they share.
//!
//! # Where this comes from
//!
//! `packages/api/session-controller/src` is 15 top-level sources (3,723 lines) plus `client/` (4,615),
//! `history/` (660), `state/` (337) and `web/` (78). Seven sibling modules already cover a large part
//! of it: `session_controller_gap` (paging, the cursor algebra, address resolution),
//! `session_engine_gap` (`ProjectionValueStore`, `Notifier`), `session_snapshot_gap`
//! (`SessionView`, `ListEngine`, `ControlFrame`), `session_window_gap` (the event window and the
//! Assistant-stream fold), `session_client_gap` (the stream transport), `session_extra_gap` (storage
//! generations and headers), `session_stats_gap` (the `sessionStats` unit).
//!
//! Reading those seven against the fifteen is what produced this module, and the finding is not a
//! list of missing files. It is one shape, repeated: **the consumer exists and the producer does
//! not.**
//!
//! # The gap, stated as evidence rather than as an impression
//!
//! `session_snapshot_gap` reads the projection key `"sessionListMetadata"` in four places —
//! `metadata_of`, `metadata_blank`, `metadata_last_prompt_at`, and the per-row merge that turns the
//! durability bit into a display bit:
//!
//! ```text
//! $ grep -rn '"sessionListMetadata"' crates/girder-core/src
//! session_snapshot_gap.rs:1031:    let Some(metadata) = projections.value("sessionListMetadata") else {
//! session_snapshot_gap.rs:1043:        .get("sessionListMetadata")
//! session_snapshot_gap.rs:1051:        .get("sessionListMetadata")
//! ```
//!
//! Every one of those is a read. Searching the whole crate for a write — a `Projection` impl, a fold
//! function, a registration — returns nothing: `applySessionListMetadata`, `sessionListMetadata` as a
//! `KEY`, and the projection definition itself are all absent. The tests in
//! `tests/session_snapshot_gap.rs` hand-write the JSON (`json!({ "blank": false, "lastPromptAt": 7 })`)
//! because the repository has no code that could have produced it. A list row's `blank` and
//! `updatedAt` therefore degrade to the durability bit and the header time forever, and the
//! reconciliation branch in `build_list_snapshot` is dead code against any real Host.
//!
//! `continuation.ts` is the same shape with a shorter history: the key `conversationContinuation`
//! has **zero** occurrences in this crate, while upstream's `packages/client/ui-conversation` reads
//! exactly that key to decide whether an empty prompt may continue a failed turn. So there is not yet
//! a consumer here to be starved — but the moment one is ported it will be starved in precisely the
//! way `sessionListMetadata` already is.
//!
//! # What this module produces
//!
//! 1. **`sessionListMetadata`** — [`SessionListMetadata`], its fold
//!    [`apply_session_list_metadata`], and the registerable [`SessionListMetadataProjection`]
//!    (`stateVersion: 1`, `summary: true`). The value is written in the exact shape the existing
//!    readers parse: `{ "blank": bool, "lastPromptAt": number | null }`.
//! 2. **`conversationContinuation`** — [`ContinuationState`], its fold [`fold_continuation`], and the
//!    registerable [`ContinuationProjection`] (`stateVersion: 4`), including the wire projection
//!    upstream keeps separate (`view: state => state.ending`).
//! 3. **The pure half of `list.ts`** that the producers' output is read through:
//!    [`truncate_unicode_code_points`] (the 240-code-point search snippet),
//!    [`normalize_search_query`] (trim, length, NUL), [`SearchAccumulator`] (the authorized, deduped,
//!    bounded, `hasMore`-reporting hit loop) and [`updatedAt`](updated_at) (the `max(createdAt,
//!    lastPromptAt)` correction the list row performs).
//! 4. **`underArchivedSession`** from `archived-session-gate.ts`, the ancestry walk that stops a
//!    model step in an archived Session or any subagent descendant of one.
//!
//! # Why the folds take an explicit input type instead of `&SessionEvent`
//!
//! Upstream folds over `SessionEventMap`, which has 54 tags. [`girder_core::SessionEvent`] has the
//! conversation core — turn, step, message, tool — and deliberately does not carry
//! `assistant/attempt`, the durable attempt record the continuation fold keys on, or the `retry`
//! message source it admits. A fold written against the local enum could not express either, and
//! inventing a variant would put a tag in the log that no writer upstream produces. So the folds take
//! [`ContinuationEvent`] and [`SessionListMetadata`]'s equivalent minimal input, and
//! [`ContinuationEvent::from_session_event`] maps every local event that *does* exist. This is the
//! same bargain [`girder_core::session::window`] makes with `AssistantSettlement`, and
//! [`girder_core::llm::retry_event`] documents the cost of not making it.
//!
//! # Rules that are load-bearing
//!
//! - **Blank only ever lowers on `turn/start`, and is stored with equality, not `min`.** Upstream's
//!   `blank: state.blank && event.type !== 'turn/start'`. A fold that recomputed blankness from the
//!   log would re-blank a session whose compaction dropped its first turn.
//! - **`lastPromptAt` records the person, not every user-role row.** A synthetic injection, a relayed
//!   peer message, and a tool-fed notice are all `role: user`; only `source.kind === 'user'` is a
//!   prompt. Counting the others makes "last activity" report the harness talking to itself.
//! - **An unchanged fold returns the input unchanged.** Upstream compares `blank` and `lastPromptAt`
//!   and returns the original object, which is what keeps a two-hundred-session replay from
//!   reallocating every state on every event. Here that is observable as `apply` returning a value
//!   equal to the input, and [`apply_session_list_metadata`] is written to preserve it.
//! - **A continuation needs three facts, and dropping any one of them turns a failed turn into a
//!   silent retry.** Admitted human work (`admitted`), a failed settlement in the last request step
//!   (`failedAttempt`), and a `turn/end` whose error code is continuable. An eligibility rule that
//!   omitted `admitted` would offer to "continue" a turn nobody started; one that omitted
//!   `failedAttempt` would offer to continue a turn that never asked the model anything.
//! - **`turn/start` resets to empty, and `turn/end` clears on every non-continuable outcome.** The
//!   eligibility is about *the last* turn, so a completed turn must erase an earlier failure rather
//!   than leave a stale seq that a later empty prompt would treat as continuable.
//! - **`step/start` and `assistant/message` clear only the failed-attempt bit, never `admitted`.**
//!   The person's work survives a retry that reached the model; that is what makes the second attempt
//!   continuable if it too fails.
//! - **The continuable failure set is a product policy, not the retry policy.** It is fixed and does
//!   not consult `girder_core::llm::retry`, because an explicit manual continuation is a person's decision
//!   and must not silently change when an operator tunes automatic retry.
//! - **An archived ancestor stops the step even when the intermediate Session is gone.** The walk
//!   falls back to the archived set for a parent it cannot load, because the alternative — treating an
//!   unloadable parent as live — would run a model step inside an archived lineage.
//! - **A fork breaks the line.** Only `origin === 'subagent'` Sessions carry ancestry; a fork of an
//!   archived Session is an independent conversation and must be allowed to run.
//!
//! # Module-include note
//!
//! This file is compiled by path from `tests/session_top_gap.rs`, so every reference to the library
//! goes through `girder_core::…` rather than `crate::…`: inside a `#[path]` module, `crate::` names
//! the *test* crate.

#![allow(
    dead_code,
    reason = "the crate root's module wiring is a separate change; everything here is reached from \
              the by-path test target until then"
)]

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use girder_core::session::stats_rollup::TimeSource;
use girder_core::{MessageSource, Projection, ProjectionSeq, SessionEvent, VersionedProjection};

// ---------------------------------------------------------------------------------------------
// Keys and versions
// ---------------------------------------------------------------------------------------------

/// The projection key whose value the list row's `blank` and `updatedAt` are corrected by.
///
/// Spelled here as a constant because the readers in
/// [`girder_core::session::snapshot`] spell it as a literal, and a producer that invented a
/// different spelling would leave those readers exactly as starved as they are now.
pub const LIST_METADATA_KEY: &str = "sessionListMetadata";

/// Upstream's `stateVersion: 1` for the list-metadata unit.
pub const LIST_METADATA_STATE_VERSION: u64 = 1;

/// The projection key an empty prompt's continuation eligibility is published under.
pub const CONTINUATION_KEY: &str = "conversationContinuation";

/// Upstream's `stateVersion: 4` for the continuation unit.
///
/// Four rather than one because the eligibility rule was tightened three times; a stored row folded
/// under an earlier version answers a different question and must be discarded, which is the whole
/// reason the number exists.
pub const CONTINUATION_STATE_VERSION: u64 = 4;

/// Maximum number of Sessions one search returns.
pub const SESSION_SEARCH_RESULT_LIMIT: usize = 20;

/// Maximum search snippet length in Unicode code points.
pub const SESSION_SEARCH_SNIPPET_MAX_CODE_POINTS: usize = 240;

/// Maximum provider calls one search may spend.
pub const SEARCH_PROVIDER_CALL_LIMIT: u32 = 100;

/// Maximum accepted search query length, in UTF-16 code units as upstream measures it.
pub const SESSION_SEARCH_QUERY_MAX_CHARS: usize = 500;

/// The failure codes an explicit manual continuation may repair.
///
/// A fixed product policy: these are the failures where the person's own retry can plausibly succeed
/// and where nothing about their work was lost. `QUOTA`, `CONTEXT_WINDOW_EXCEEDED`, and every
/// non-retryable code are absent on purpose — offering to continue one of those would retry a request
/// that fails identically every time.
pub const CONTINUABLE_FAILURES: [&str; 6] = [
    "RATE_LIMIT",
    "SERVER",
    "TIMEOUT",
    "TRANSPORT",
    "EMPTY_RESPONSE",
    "STREAM_CLOSED",
];

/// Whether one `turn/end` failure code is repairable by an explicit continuation.
#[must_use]
pub fn is_continuable_failure(code: &str) -> bool {
    CONTINUABLE_FAILURES.contains(&code)
}

// ---------------------------------------------------------------------------------------------
// sessionListMetadata — the producer the list rows already read
// ---------------------------------------------------------------------------------------------

/// Persisted hints used to summarize a Session without activating it.
///
/// Upstream's `SessionListMetadata`. Two facts, and both are needed for a cold list row: `blank` says
/// the conversation has no turn yet (so the row may be reused or hidden), and `lastPromptAt` says
/// when the person last spoke (so `updatedAt` is not merely the header's creation time).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionListMetadata {
    /// Whether the folded prefix contains no `turn/start`.
    pub blank: bool,
    /// Latest human-authored prompt time, in milliseconds since the Unix epoch.
    ///
    /// `None` until the person speaks, and never a stand-in for zero: a prompt at the epoch is not
    /// the same fact as no prompt at all, and a row that rendered "1970" for "never" would be lying.
    pub last_prompt_at: Option<i64>,
}

impl SessionListMetadata {
    /// The empty log's metadata: nothing has started, nobody has spoken.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            blank: true,
            last_prompt_at: None,
        }
    }

    /// Builds a state from both facts, for a restored checkpoint.
    #[must_use]
    pub const fn with(blank: bool, last_prompt_at: Option<i64>) -> Self {
        Self {
            blank,
            last_prompt_at,
        }
    }

    /// The wire shape the list readers parse: `{ "blank": bool, "lastPromptAt": number | null }`.
    ///
    /// `lastPromptAt` is `null` rather than omitted, and rather than `0`. Upstream's reader is
    /// `z.number().nullable()`, so an absent key would fail validation; and `0` is a time, which is
    /// why a negative or absent time is written as `null` and never clamped.
    #[must_use]
    pub fn to_json(self) -> Value {
        json!({
            "blank": self.blank,
            "lastPromptAt": self
                .last_prompt_at
                .filter(|at| *at >= 0)
                .map_or(Value::Null, |at| json!(at)),
        })
    }

    /// Reads the wire shape back.
    ///
    /// A state that does not parse answers `None`, and the caller keeps whatever it held. Silently
    /// substituting [`SessionListMetadata::new`] would re-blank a conversation that had turns, which
    /// is the one direction of error that hides history from the user.
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            blank: object.get("blank")?.as_bool()?,
            last_prompt_at: match object.get("lastPromptAt") {
                None | Some(Value::Null) => None,
                Some(at) => Some(at.as_i64()?),
            },
        })
    }
}

/// Advances the list-metadata projection by one committed event.
///
/// Upstream's `applySessionListMetadata`, unchanged in behaviour: `blank` falls exactly once, on the
/// first `turn/start`, and never rises again; `lastPromptAt` records a `user/message` only when the
/// person produced it.
///
/// `at_ms` is the event's wall-clock time, which this crate's [`SessionEvent`] does not carry. Pass
/// `None` from a source that cannot date the log — the fold then keeps the previous prompt time
/// rather than guessing, so an undated log reports "unknown" instead of a fabricated instant.
#[must_use]
pub fn apply_session_list_metadata(
    state: SessionListMetadata,
    event: &SessionEvent,
    at_ms: Option<i64>,
) -> SessionListMetadata {
    let blank = state.blank && event.type_name() != "turn/start";
    let last_prompt_at = match event {
        SessionEvent::UserMessage { message } if is_human_prompt(&message.source) => {
            at_ms.or(state.last_prompt_at)
        }
        _ => state.last_prompt_at,
    };
    if blank == state.blank && last_prompt_at == state.last_prompt_at {
        return state;
    }
    SessionListMetadata {
        blank,
        last_prompt_at,
    }
}

/// Whether a message source is the person's own prompt.
///
/// The distinction upstream spells `event.data.source.kind === 'user'`. A `relay` is another agent
/// speaking, a `system-prompt` is the harness, and a context form is an injection — all of them arrive
/// as user-role content, and none of them is the person's last word.
fn is_human_prompt(source: &MessageSource) -> bool {
    matches!(source, MessageSource::User { .. })
}

/// The registerable `sessionListMetadata` unit.
///
/// This is the producer whose absence starved `session_snapshot_gap`'s readers. It is a projection over
/// the durable log, so a cold list row can be summarized without opening the Session: exactly the
/// property upstream's name for the key describes.
pub struct SessionListMetadataProjection {
    clock: Arc<dyn TimeSource>,
}

impl SessionListMetadataProjection {
    /// A unit folded over `clock`, which resolves `seq -> time`.
    #[must_use]
    pub fn new(clock: Arc<dyn TimeSource>) -> Self {
        Self { clock }
    }

    /// A unit over a log that carries no clock.
    ///
    /// `blank` is exact; `lastPromptAt` stays absent. Better an absent time than a fabricated one: a
    /// list sorted by a wrong timestamp reorders conversations under the user.
    #[must_use]
    pub fn untimed() -> Self {
        Self {
            clock: Arc::new(girder_core::session::stats_rollup::Untimed),
        }
    }
}

impl std::fmt::Debug for SessionListMetadataProjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionListMetadataProjection")
            .field("key", &LIST_METADATA_KEY)
            .field("state_version", &LIST_METADATA_STATE_VERSION)
            .finish_non_exhaustive()
    }
}

impl Projection for SessionListMetadataProjection {
    fn key(&self) -> &'static str {
        LIST_METADATA_KEY
    }

    fn init(&self) -> Value {
        SessionListMetadata::new().to_json()
    }

    fn apply(&self, state: Value, event: &SessionEvent, seq: ProjectionSeq) -> Value {
        // A state that does not parse is left untouched rather than replaced by `init`: the registry's
        // cursor has already moved past the event, so rewriting would discard a fold that was almost
        // readable and could not be rebuilt.
        let Some(current) = SessionListMetadata::from_json(&state) else {
            return state;
        };
        apply_session_list_metadata(current, event, self.clock.at_ms(seq)).to_json()
    }

    fn in_summary(&self) -> bool {
        // Upstream declares no `summary: false` here, so the value belongs in every list row. That is
        // the point of the unit: a row that omitted it could not render its own blankness.
        true
    }
}

impl VersionedProjection for SessionListMetadataProjection {
    fn state_version(&self) -> u64 {
        LIST_METADATA_STATE_VERSION
    }
}

/// The modification time a list row reports: `max(createdAt, lastPromptAt)`.
///
/// Upstream's `updatedAt` helper. The maximum rather than the later fact alone, because a header's
/// creation time is a real lower bound: a corrupt or clock-skewed `lastPromptAt` in the past must not
/// make a session look older than its own creation.
#[must_use]
pub fn updated_at(created_at: u64, metadata: SessionListMetadata) -> u64 {
    match metadata.last_prompt_at {
        // A negative time is not a time: it cannot raise a header's creation instant, and treating it
        // as an enormous `u64` through a cast would move the session to the top of every list.
        Some(at) => u64::try_from(at).map_or(created_at, |at| created_at.max(at)),
        None => created_at,
    }
}

// ---------------------------------------------------------------------------------------------
// conversationContinuation
// ---------------------------------------------------------------------------------------------

/// Who produced an admitted user-role row, as far as continuation eligibility cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuationSource {
    /// The person, with content.
    User,
    /// The empty message an explicit retry submits.
    ///
    /// Upstream's `source.kind === 'retry'`. It admits a continuation even though it carries no
    /// content, which is the whole reason the rule tests the source and not the text: an empty prompt
    /// from a person is a mistake, and an empty prompt from the retry path is the feature.
    Retry,
    /// Anything else: a relay, an injection, a system prompt, a tool result.
    Other,
}

impl ContinuationSource {
    /// Classifies a local message source.
    ///
    /// [`girder_core::MessageSource`] has no `retry` member, so this build can never produce one from
    /// a durable log; a caller that has the wire value passes [`ContinuationSource::Retry`] directly.
    #[must_use]
    pub fn of(source: &MessageSource) -> Self {
        if is_human_prompt(source) {
            Self::User
        } else {
            Self::Other
        }
    }
}

/// Whether one admitted row joined the surface at the tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuationSurface {
    /// `surfaceOp: 'append'`.
    Append,
    /// A replacement, which shadows history rather than adding to it.
    Replace,
}

/// One durable fact the continuation fold reads.
///
/// Not [`SessionEvent`]: the fold keys on `assistant/attempt`, which this crate's event enum does not
/// carry, and on the `retry` source, which its message model does not carry either. See the module
/// docs for why inventing those tags locally would be worse than an explicit input type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContinuationEvent {
    /// A turn began; eligibility resets to empty.
    TurnStart,
    /// A step began, or an answer was committed: the last attempt did not fail.
    Progressed,
    /// A model attempt was recorded: the last attempt failed.
    FailedAttempt,
    /// A user-role row was admitted to the surface.
    UserMessage {
        /// How it joined the surface.
        surface: ContinuationSurface,
        /// Who produced it.
        source: ContinuationSource,
        /// How many content blocks it carries.
        content_blocks: usize,
    },
    /// A turn ended, carrying the failure code when it failed.
    TurnEnd {
        /// The failure code, when the turn failed. `None` for a completed, aborted, blocked, or
        /// budget-exhausted turn — none of which is repairable by continuing.
        ///
        /// Owned rather than borrowed: the code comes off a durable event, whose lifetime has nothing
        /// to do with the fold's, and a `&'static str` would compile only against a hard-coded table.
        failure_code: Option<String>,
    },
}

impl ContinuationEvent {
    /// Maps one local durable event, when it maps at all.
    ///
    /// The names that exist locally are exact; the two that do not (`assistant/attempt` and the
    /// `retry` source) are absent, and a caller that needs them supplies the corresponding variant.
    /// `surface` must come from the caller because surface placement lives on
    /// [`girder_core::SurfaceEnvelope`] here rather than on the event.
    #[must_use]
    pub fn from_session_event(event: &SessionEvent, surface: ContinuationSurface) -> Option<Self> {
        Some(match event {
            SessionEvent::TurnStart { .. } => Self::TurnStart,
            SessionEvent::StepStart { .. } | SessionEvent::AssistantMessage { .. } => {
                Self::Progressed
            }
            SessionEvent::UserMessage { message } => Self::UserMessage {
                surface,
                source: ContinuationSource::of(&message.source),
                content_blocks: message.content.len(),
            },
            SessionEvent::TurnEnd { reason, .. } => Self::TurnEnd {
                failure_code: match reason {
                    girder_core::TurnEndReason::Failed { code, .. } => Some(code.clone()),
                    girder_core::TurnEndReason::Completed
                    | girder_core::TurnEndReason::Aborted { .. }
                    | girder_core::TurnEndReason::Blocked
                    | girder_core::TurnEndReason::Budget => None,
                },
            },
            _ => return None,
        })
    }
}

/// Recoverable manual-continuation facts over admitted user input and empty retry messages.
///
/// Upstream's `ContinuationState`. The three fields are not independent: `ending` is only ever set
/// when the other two are true, and it is cleared with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuationState {
    /// Whether the turn admitted human work — a person's message, or the retry placeholder.
    pub admitted: bool,
    /// Whether the last request step failed.
    pub failed_attempt: bool,
    /// The eligible `turn/end` seq, or `None` when the last turn is not continuable.
    pub ending: Option<u64>,
}

impl ContinuationState {
    /// The state every turn starts from.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            admitted: false,
            failed_attempt: false,
            ending: None,
        }
    }

    /// Builds a state from all three facts, for a restored checkpoint or a direct assertion.
    ///
    /// `ending` is not validated against the other two here: a stored state is whatever the previous
    /// process wrote, and refusing one whose flags disagree would discard a continuation that may be
    /// legitimate — the fold's own rule is what guarantees they agree for anything it produced.
    #[must_use]
    pub const fn with(ending: Option<u64>, admitted: bool, failed_attempt: bool) -> Self {
        Self {
            admitted,
            failed_attempt,
            ending,
        }
    }

    /// Whether an empty prompt would continue a failed turn.
    #[must_use]
    pub const fn is_continuable(self) -> bool {
        self.ending.is_some()
    }

    /// The internal state as JSON, for a store that keeps the whole fold.
    ///
    /// Distinct from [`ContinuationState::to_wire`]: the wire carries only `ending`, and publishing the
    /// other two would invite a client to re-derive eligibility it cannot get right — the rule depends
    /// on evolutions of `stateVersion` the client has no way to know.
    #[must_use]
    pub fn to_json(self) -> Value {
        json!({
            "admitted": self.admitted,
            "failedAttempt": self.failed_attempt,
            "ending": self.ending.map_or(Value::Null, |seq| json!(seq)),
        })
    }

    /// Reads the internal state back.
    ///
    /// `None` for a value that does not parse, so the caller keeps its previous state instead of
    /// resetting to [`ContinuationState::empty`] and losing an eligible continuation.
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            admitted: object.get("admitted")?.as_bool()?,
            failed_attempt: object.get("failedAttempt")?.as_bool()?,
            ending: match object.get("ending") {
                None | Some(Value::Null) => None,
                Some(seq) => Some(seq.as_u64()?),
            },
        })
    }

    /// The published view: the eligible seq, or `null`.
    ///
    /// Upstream's `wire: { viewSchema: endingSchema, view: state => state.ending }`. One value, because
    /// one value is the answer to the only question a client asks.
    #[must_use]
    pub fn to_wire(self) -> Value {
        self.ending.map_or(Value::Null, |seq| json!(seq))
    }
}

impl Default for ContinuationState {
    fn default() -> Self {
        Self::empty()
    }
}

/// Folds one durable fact into the continuation state.
///
/// Upstream's `continuationProjectionDefinition.apply`, branch for branch. `seq` is the folded event's
/// position, and it is what becomes `ending` — the seq of the `turn/end` that made a continuation
/// possible, which is what a client correlates against the projection's own watermark.
#[must_use]
pub fn fold_continuation(
    state: ContinuationState,
    event: ContinuationEvent,
    seq: u64,
) -> ContinuationState {
    match event {
        // A new turn starts from nothing: whatever an earlier turn admitted or failed is history.
        ContinuationEvent::TurnStart => ContinuationState::empty(),
        // Reaching the model clears the failure. `admitted` survives, because the person's work is
        // still the reason this turn exists.
        ContinuationEvent::Progressed => {
            if state.failed_attempt {
                ContinuationState {
                    failed_attempt: false,
                    ..state
                }
            } else {
                state
            }
        }
        ContinuationEvent::FailedAttempt => {
            if state.failed_attempt {
                state
            } else {
                ContinuationState {
                    failed_attempt: true,
                    ..state
                }
            }
        }
        ContinuationEvent::UserMessage {
            surface,
            source,
            content_blocks,
        } => {
            let admits = surface == ContinuationSurface::Append
                && match source {
                    ContinuationSource::User => content_blocks > 0,
                    ContinuationSource::Retry => content_blocks == 0,
                    ContinuationSource::Other => false,
                };
            if state.admitted || !admits {
                state
            } else {
                ContinuationState {
                    admitted: true,
                    ..state
                }
            }
        }
        ContinuationEvent::TurnEnd { failure_code } => {
            let continuable = failure_code.as_deref().is_some_and(is_continuable_failure)
                && state.admitted
                && state.failed_attempt;
            if continuable {
                ContinuationState {
                    ending: Some(seq),
                    ..ContinuationState::empty()
                }
            } else {
                ContinuationState::empty()
            }
        }
    }
}

/// The registerable `conversationContinuation` unit.
///
/// The fold and the version are the whole of it: the value needs no clock, no I/O, and no other
/// projection, which is why upstream states it in 58 lines and why it was cheap to leave out and
/// expensive to be missing.
pub struct ContinuationProjection;

impl std::fmt::Debug for ContinuationProjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContinuationProjection")
            .field("key", &CONTINUATION_KEY)
            .field("state_version", &CONTINUATION_STATE_VERSION)
            .finish()
    }
}

impl Projection for ContinuationProjection {
    fn key(&self) -> &'static str {
        CONTINUATION_KEY
    }

    fn init(&self) -> Value {
        ContinuationState::empty().to_json()
    }

    fn apply(&self, state: Value, event: &SessionEvent, seq: ProjectionSeq) -> Value {
        let Some(current) = ContinuationState::from_json(&state) else {
            return state;
        };
        // Every locally modelled user message is an append: this crate's surface placement lives on
        // `SurfaceEnvelope`, not on the event, and the declaration reads the event alone. A log whose
        // user message shadowed a range would be reported by the caller through
        // `ContinuationEvent::UserMessage { surface: Replace, .. }` and refused by `fold_continuation`.
        match ContinuationEvent::from_session_event(event, ContinuationSurface::Append) {
            Some(input) => fold_continuation(current, input, seq).to_json(),
            None => state,
        }
    }

    fn in_summary(&self) -> bool {
        // Upstream declares no `summary: false`, so a list row may carry the eligible seq. It is one
        // number and it saves the composer a projection read per rendered row.
        true
    }

    fn wire(&self, state: &Value) -> Value {
        ContinuationState::from_json(state)
            .map(ContinuationState::to_wire)
            .unwrap_or(Value::Null)
    }
}

impl VersionedProjection for ContinuationProjection {
    fn state_version(&self) -> u64 {
        CONTINUATION_STATE_VERSION
    }
}

// ---------------------------------------------------------------------------------------------
// The pure half of `list.ts` the producers are read through
// ---------------------------------------------------------------------------------------------

/// The longest prefix containing at most `maximum` Unicode code points.
///
/// Upstream's `truncateUnicodeCodePoints`. Code points, not bytes and not UTF-16 units: a snippet cut
/// mid-code-point is the mojibake that makes a search result look like log corruption. No ellipsis is
/// appended — the caller decides whether a shortened snippet should say so, and
/// [`girder_core::session::query_rpc`] already has its own spelling of that for the tool path.
#[must_use]
pub fn truncate_unicode_code_points(value: &str, maximum: usize) -> String {
    let mut end = 0;
    for (count, (offset, character)) in value.char_indices().enumerate() {
        if count == maximum {
            return value[..end].to_owned();
        }
        end = offset + character.len_utf8();
    }
    value.to_owned()
}

/// Why a search query was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// The query was empty after trimming.
    Empty,
    /// The query exceeded [`SESSION_SEARCH_QUERY_MAX_CHARS`].
    TooLong {
        /// The trimmed query's length, in UTF-16 code units.
        length: usize,
        /// The permitted maximum.
        limit: usize,
    },
    /// The query contained a NUL, which no provider can index.
    Nul,
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(formatter, "session search query must not be empty"),
            Self::TooLong { length, limit } => write!(
                formatter,
                "session search query must contain at most {limit} UTF-16 code units, got {length}"
            ),
            Self::Nul => write!(formatter, "session search query must not contain NUL"),
        }
    }
}

impl std::error::Error for SearchError {}

/// Trims and validates one search query.
///
/// Upstream's `normalizeSearchQuery`. Trimmed first, because a query of spaces is empty in every way
/// that matters and reporting it as too long would be untrue. Measured in UTF-16 code units, which is
/// what upstream's `String.length` counts and therefore what a client's own limit check agreed to.
///
/// # Errors
///
/// [`SearchError::Empty`], [`SearchError::TooLong`], or [`SearchError::Nul`].
pub fn normalize_search_query(query: &str) -> Result<String, SearchError> {
    let normalized = query.trim();
    if normalized.is_empty() {
        return Err(SearchError::Empty);
    }
    let length: usize = normalized.encode_utf16().count();
    if length > SESSION_SEARCH_QUERY_MAX_CHARS {
        return Err(SearchError::TooLong {
            length,
            limit: SESSION_SEARCH_QUERY_MAX_CHARS,
        });
    }
    if normalized.contains('\0') {
        return Err(SearchError::Nul);
    }
    Ok(normalized.to_owned())
}

/// One Session search result on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchItem {
    /// The matching Session.
    pub session_id: String,
    /// The bounded snippet shown in the row.
    pub snippet: String,
}

/// The bounded search response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchValue {
    /// Accepted results, at most [`SESSION_SEARCH_RESULT_LIMIT`].
    pub items: Vec<SearchItem>,
    /// Whether more authorized results existed than the limit allowed.
    pub has_more: bool,
}

/// What accepting one provider hit produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchVerdict {
    /// The hit became a result.
    Accepted,
    /// The Session was already accepted; a second snippet for one Session is noise.
    Duplicate,
    /// The Session is not visible to this caller.
    Invisible,
    /// The hit is not a current-surface message of an admitted type.
    NotAMessage,
    /// The result budget is already exceeded, so the hit only sets `has_more`.
    OverLimit,
}

/// One provider hit, as far as the authorization filter reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHit<'a> {
    /// The Session the hit belongs to.
    pub header_session_id: &'a str,
    /// The Session named by the best match; must equal the header's.
    pub best_match_session_id: &'a str,
    /// The surface the best match sits on; only `current` is admissible.
    pub best_match_surface: &'a str,
    /// The best match's event type; only `user/message` and `assistant/message` are admissible.
    pub best_match_type: &'a str,
    /// The snippet, before bounding.
    pub snippet: &'a str,
}

/// The authorized, deduplicated, bounded result set one search is building.
///
/// Upstream's `search` loop body, which is where every one of its refusals lives: a hit whose Session
/// is not visible is dropped rather than refused (a provider may index what this caller cannot read);
/// a second hit for one Session is dropped rather than shown twice; and a hit beyond the limit is
/// dropped but still proves `hasMore`.
///
/// `has_more` is `accepted > limit`, not `== limit`: a provider that returned exactly the limit has not
/// shown that anything follows, and reporting "more" would make a client paginate into an empty page.
#[derive(Clone, Debug)]
pub struct SearchAccumulator {
    visible: BTreeSet<String>,
    accepted: Vec<SearchItem>,
    accepted_ids: BTreeSet<String>,
}

impl SearchAccumulator {
    /// A search over the Sessions one caller may read.
    #[must_use]
    pub fn new(visible: BTreeSet<String>) -> Self {
        Self {
            visible,
            accepted: Vec::new(),
            accepted_ids: BTreeSet::new(),
        }
    }

    /// Whether a Session is visible, and therefore whether any of its hits can be authorized.
    #[must_use]
    pub fn is_visible(&self, session_id: &str) -> bool {
        self.visible.contains(session_id)
    }

    /// How many results have been accepted, including the overflow one.
    #[must_use]
    pub fn accepted(&self) -> usize {
        self.accepted.len()
    }

    /// Whether the loop should stop requesting pages.
    ///
    /// True once an overflow result exists or the provider ran out: the caller asks for the next page
    /// only while this is false, which is upstream's `authorized.length > LIMIT || nextCursor ===
    /// undefined` break.
    #[must_use]
    pub fn is_satisfied(&self) -> bool {
        self.accepted.len() > SESSION_SEARCH_RESULT_LIMIT
    }

    /// Offers one provider hit to the result set.
    pub fn accept(&mut self, hit: &SearchHit<'_>) -> SearchVerdict {
        if !self.visible.contains(hit.header_session_id) {
            return SearchVerdict::Invisible;
        }
        // A hit whose best match names a different Session is a provider bug, and admitting it would
        // attribute one conversation's text to another.
        if hit.best_match_session_id != hit.header_session_id
            || hit.best_match_surface != "current"
            || !matches!(hit.best_match_type, "user/message" | "assistant/message")
        {
            return SearchVerdict::NotAMessage;
        }
        if self.accepted_ids.contains(hit.header_session_id) {
            return SearchVerdict::Duplicate;
        }
        if self.accepted.len() > SESSION_SEARCH_RESULT_LIMIT {
            // Past the budget: the extra hit is not shown, but its existence is what `has_more` means.
            return SearchVerdict::OverLimit;
        }
        self.accepted_ids.insert(hit.header_session_id.to_owned());
        self.accepted.push(SearchItem {
            session_id: hit.header_session_id.to_owned(),
            snippet: truncate_unicode_code_points(
                hit.snippet,
                SESSION_SEARCH_SNIPPET_MAX_CODE_POINTS,
            ),
        });
        SearchVerdict::Accepted
    }

    /// The bounded response.
    #[must_use]
    pub fn value(&self) -> SearchValue {
        SearchValue {
            items: self
                .accepted
                .iter()
                .take(SESSION_SEARCH_RESULT_LIMIT)
                .cloned()
                .collect(),
            has_more: self.is_satisfied(),
        }
    }
}

/// Why a search stopped requesting pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchStop {
    /// The provider spent its whole work budget.
    ProviderBudgetExhausted {
        /// The budget that was exceeded.
        limit: u32,
    },
    /// The provider offered more items than the page size it was given.
    OverlongPage {
        /// How many items it returned.
        got: usize,
        /// The page size it was asked for.
        asked: usize,
    },
    /// The provider returned a continuation cursor it had already returned.
    RepeatedCursor,
}

impl std::fmt::Display for SearchStop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderBudgetExhausted { limit } => write!(
                formatter,
                "session search provider exceeded the {limit}-call work budget"
            ),
            Self::OverlongPage { got, asked } => write!(
                formatter,
                "session search provider returned {got} items; maximum is {asked}"
            ),
            Self::RepeatedCursor => {
                write!(
                    formatter,
                    "session search provider repeated a continuation cursor"
                )
            }
        }
    }
}

impl std::error::Error for SearchStop {}

/// The page-request loop's own budget: call count, page bound, and cursor memory.
///
/// Three guards upstream states in one `while` loop, separated here because each one fails for a
/// different reason and a single counter cannot explain which: a provider that never terminates, one
/// that ignores the page size, and one that loops on one cursor are three bugs with three fixes.
#[derive(Clone, Debug)]
pub struct SearchPager {
    page_limit: usize,
    calls: u32,
    seen_cursors: BTreeSet<String>,
    finished: bool,
}

impl SearchPager {
    /// A pager asking for `page_limit` items per call.
    #[must_use]
    pub fn new(page_limit: usize) -> Self {
        Self {
            page_limit,
            calls: 0,
            seen_cursors: BTreeSet::new(),
            finished: false,
        }
    }

    /// The page size the next call must ask for.
    ///
    /// A halving between calls is how upstream recovers from `SESSION_QUERY_INVALID_LIMIT`: the
    /// provider rejected the size, and the only honest response is to ask for less rather than to
    /// retry the same number.
    #[must_use]
    pub const fn page_limit(&self) -> usize {
        self.page_limit
    }

    /// How many provider calls have been charged.
    #[must_use]
    pub const fn calls(&self) -> u32 {
        self.calls
    }

    /// Whether the loop must stop asking for pages.
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    /// Charges one provider call.
    ///
    /// # Errors
    ///
    /// [`SearchStop::ProviderBudgetExhausted`] when the budget is already spent, which is checked
    /// *before* the call so a runaway provider cannot exceed the bound by one.
    pub fn charge_call(&mut self) -> Result<(), SearchStop> {
        if self.calls >= SEARCH_PROVIDER_CALL_LIMIT {
            self.finished = true;
            return Err(SearchStop::ProviderBudgetExhausted {
                limit: SEARCH_PROVIDER_CALL_LIMIT,
            });
        }
        self.calls += 1;
        Ok(())
    }

    /// Checks one page against the size that was requested.
    ///
    /// # Errors
    ///
    /// [`SearchStop::OverlongPage`] when the provider returned more than it was asked for. Reporting
    /// "more" would hide that the page bound — and therefore the result bound — was not honoured.
    pub fn check_page(&self, items: usize) -> Result<(), SearchStop> {
        if items > self.page_limit {
            return Err(SearchStop::OverlongPage {
                got: items,
                asked: self.page_limit,
            });
        }
        Ok(())
    }

    /// Records one continuation cursor.
    ///
    /// # Errors
    ///
    /// [`SearchStop::RepeatedCursor`] when the provider hands back a cursor it already returned, which
    /// would make the loop request the same page forever.
    pub fn observe_cursor(&mut self, cursor: &str) -> Result<(), SearchStop> {
        if !self.seen_cursors.insert(cursor.to_owned()) {
            self.finished = true;
            return Err(SearchStop::RepeatedCursor);
        }
        Ok(())
    }

    /// Halves the page size after the provider refused it.
    ///
    /// Floors at one: a size of zero is a request no provider accepts, so a second refusal at one item
    /// is propagated rather than retried.
    pub fn shrink_page_limit(&mut self) {
        self.page_limit = (self.page_limit / 2).max(1);
    }

    /// Records that the provider ran out of pages.
    pub const fn finish(&mut self) {
        self.finished = true;
    }
}

// ---------------------------------------------------------------------------------------------
// archived-session-gate.ts
// ---------------------------------------------------------------------------------------------

/// The immutable facts the archive walk follows.
///
/// Upstream reads these off `agent.session.header`. A borrowed view rather than a header type, because
/// this crate's [`girder_core`] spells a stored header in `session_extra_gap` with a different field
/// set and a different lifetime posture, and a walk over three fields should not force a migration of
/// either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineageHeader<'a> {
    /// The Session's identity.
    pub id: &'a str,
    /// Whether this Session was spawned as a subagent rather than created by a client.
    pub origin_subagent: bool,
    /// The parent Session, when this one has one.
    pub parent_session: Option<&'a str>,
}

/// Whether an archived Session owns the step an Agent is about to take.
///
/// Upstream's `underArchivedSession`. The walk is over *subagent-origin* ancestry only, because a fork
/// of an archived Session is an independent conversation: it inherited a prefix, not a lifecycle, and
/// blocking it would make unarchiving a parent the only way to use a copy that never belonged to it.
///
/// `lookup` answers the header for a parent id, and `None` is a real answer — the Session is not
/// resident. In that case the parent is checked against `archived` directly rather than assumed live:
/// a released intermediate Session must not become a hole through which a model step reaches an
/// archived ancestor.
///
/// The `visited` guard is what makes a corrupt cyclic lineage terminate instead of looping: the walk
/// stops as soon as it revisits identity, and answers "not archived" because a cycle is a data defect
/// that no archived Session was proven to own.
#[must_use]
pub fn under_archived_session(
    header: LineageHeader<'_>,
    archived: &BTreeSet<String>,
    lookup: impl Fn(&str) -> Option<LineageHeader<'static>>,
) -> bool {
    // Owned because the walk reassigns the current header and the closure's answer has no lifetime
    // relation to the first one. Three fields, so the copy is cheaper than a borrow dance.
    let mut current: (String, bool, Option<String>) = roam(header);
    let mut visited: BTreeSet<String> = BTreeSet::new();
    loop {
        let (id, origin_subagent, parent_session) = current;
        if archived.contains(&id) {
            return true;
        }
        if !visited.insert(id) {
            // A cycle: nothing new can be learned, and no archived Session was reached.
            return false;
        }
        if !origin_subagent {
            return false;
        }
        let Some(parent) = parent_session else {
            return false;
        };
        match lookup(&parent) {
            Some(parent_header) => current = roam(parent_header),
            // The parent is not resident. It is still a Session identity that can be archived, and
            // treating "cannot load" as "not archived" is the one answer that runs a step inside an
            // archived lineage.
            None => return archived.contains(&parent),
        }
    }
}

/// Copies the three walkable facts out of a borrowed header.
fn roam(header: LineageHeader<'_>) -> (String, bool, Option<String>) {
    (
        header.id.to_owned(),
        header.origin_subagent,
        header.parent_session.map(str::to_owned),
    )
}

// ---------------------------------------------------------------------------------------------
// Publication helpers
// ---------------------------------------------------------------------------------------------

/// One projection unit's published wire value, keyed for a control frame.
///
/// The two units above publish differently — the list unit publishes its whole state, the continuation
/// unit publishes only `ending` — and a caller assembling a frame should not have to remember which.
/// This is the table that remembers.
#[must_use]
pub fn publish_wire(key: &str, state: &Value) -> Option<Value> {
    match key {
        LIST_METADATA_KEY => Some(state.clone()),
        CONTINUATION_KEY => ContinuationState::from_json(state).map(ContinuationState::to_wire),
        _ => None,
    }
}

/// The summary block a list row carries, filtered to `summary: true` units.
///
/// Upstream's `summaryKeys()` plus `snapshot(session, keys)`: a row may carry only the units that
/// declare themselves part of a summary, and the block carries the summary view of each — which for
/// the continuation unit is `ending`, not the internal three-field state. Getting this wrong is how a
/// list row would publish `admitted` and let a client re-derive eligibility.
#[must_use]
pub fn summary_block(states: &Map<String, Value>) -> Map<String, Value> {
    states
        .iter()
        .filter_map(|(key, state)| {
            let wire = publish_wire(key, state)?;
            Some((key.clone(), wire))
        })
        .collect()
}
