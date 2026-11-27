//! The `userQuestions.*` Remote methods: asking a person from outside the process.
//!
//! [`crate::interaction::questions`] owns the domain — what a question is ([`Question`] and its [`QuestionShape`]),
//! what an answer is ([`Answer`]), how an answer is validated against the shape a question declared,
//! and who produced it ([`AnsweredBy`]). What it deliberately does not own is *reach*: its
//! [`Responder`](crate::Responder) seam waits inside the process that asked, and nothing exported a
//! question to a client or accepted an answer from one. The tool form of asking existed; the RPC form
//! did not. This module is the entrance.
//!
//! # Upstream's surface, and what this build serves
//!
//! Upstream's `userQuestions` namespace is generated from two decorators in
//! `packages/interaction/user-questions/src/index.ts`, and only two:
//!
//! | upstream method | upstream shape | here |
//! |---|---|---|
//! | `userQuestions.answer(agentId, callId, answer)` | `boolean` | [`ANSWER_METHOD`] — serves an answer, returns the recorded reply |
//! | `userQuestions.attachWait(sessionId, callId, signal)` | `stream` of `{remainingMs}` | [`ATTACH_WAIT_METHOD`] — one remaining-time frame, then open until the question settles |
//!
//! Upstream *lists* pending questions not through a method but through a Session projection
//! (`faceOf('userQuestions')` → `{active, settled}`, see `AskUserQuestionItem` and
//! `UserQuestionProjectionView`). [`crate::interaction::questions_projection`] folds that view
//! from the log. [`LIST_METHOD`]
//! is added: without it a client could answer a question it had no way to discover. [`CANCEL_METHOD`]
//! is added for the same reason from the other side — see below.
//!
//! `attachWait` is the foreground countdown. The client reads one `{remainingMs}` frame and then
//! waits for the stream to end. No matching open question produces no frames, which the client
//! reads as "this call is not a timed wait".
//!
//! # Why `cancel` exists when upstream has no such method
//!
//! Upstream cancels by *closing the panel*: the waterfall's `signal` aborts, the Host listener is
//! released, and the tool call ends. An RPC client has no such signal — it disconnects, or it
//! navigates away, and the process on the other side keeps waiting. Without a cancel method the
//! only exit from a pending question is the timeout, which for a question with
//! [`NoDefault`](crate::interaction::questions::Question::default) is an error rather than an answer. So cancel
//! is the wire spelling of "the panel closed", and it settles the waiter with
//! [`AskError::Cancelled`] rather than leaving it parked.
//!
//! # Answering an answered question is an error, and the first answer wins
//!
//! The two defensible behaviours are idempotent success (return what was recorded) and explicit
//! refusal. This module refuses, and the reason is that the two cases are not the same event:
//!
//! - Idempotency is right for a retried `create` — the caller wanted one thing to exist and it does.
//! - A second `answer` is a *different* answer from a *different* moment. Either the client is
//!   showing a stale card after the question was already settled (the person is answering something
//!   that is no longer being asked), or two clients are open on one question and the second person's
//!   choice is about to be discarded. Reporting success in both cases tells a person their click
//!   counted when it did not, and the discarded answer is exactly the information a caller would need
//!   to notice the duplicate surface.
//!
//! So `answer` on a settled question fails with the state it actually reached — `answered`,
//! `cancelled`, or `expired` — and never replaces the recorded reply. The same holds for `cancel`.
//!
//! The refusal is decided by a compare-and-set inside [`QuestionStore::transition`], not by a read
//! followed by a write: two answers arriving at once both see an open question if the check is a
//! separate step, and both would then write. The store performs the check and the change under one
//! lock, so exactly one of any number of concurrent answers is recorded. A test races eight threads
//! at one question and asserts that exactly one wins.
//!
//! # The queue is the seam, and it does not lie about waiting
//!
//! [`QuestionQueue`] is what a caller holds: it records a question, waits for one of the three
//! settling events, and reports which happened. Storage is [`QuestionStore`]; the default
//! [`MemoryQuestionStore`] is a process-local map and is **lost on restart**, which is the honest
//! description of a deployment that has not supplied durable storage.
//!
//! The wait is a `oneshot` per question plus a re-read of the record after the waiter is registered.
//! The re-read is what makes the handshake safe: a settle either lands before the registration (and
//! its result is already visible in the record) or after (and finds the waiter). Both paths take the
//! waiter table's lock before touching the record, in that order, so the check is atomic with
//! respect to a settle and neither path can deadlock against the other.
//!
//! A question that nothing settles and whose question declared no default is marked [`Expired`] when
//! its wait elapses, so an answer arriving afterwards is refused with `expired` rather than being
//! recorded against a wait that no longer exists. With a default, the expiry records
//! [`Reply::defaulted`] — `by: "default"`, never `"human"` — because a caller that treated a timeout
//! as consent is the failure this crate's `questions` module exists to prevent.
//!
//! ## What this module does not do
//!
//! It is not a [`Responder`](crate::Responder): it does not decide what a timeout *means* to the
//! caller. [`AskError`] reports the three ways a wait can end without a person answering, and the
//! host that mounts `ask_user` maps that to its own policy. Mapping them onto
//! [`QuestionError::NoResponder`](crate::QuestionError) here would have been one line, and it would
//! have told a caller "this deployment has no way to ask" when the deployment had one and the wait
//! expired — a wrong named error is worse than a refusal to guess.
//!
//! Nothing here delivers, schedules, or retries anything, and no method blocks on a client: the
//! handlers are synchronous, so `list`, `answer`, and `cancel` return at once and only
//! [`QuestionQueue::ask`] waits, on the side that asked.
//!
//! # Why each method declares its own access
//!
//! `list` is a read. `answer` is a write: it changes the record and releases whoever was waiting.
//! `cancel` is `destructive`, by the definition in [`crate::guard::access`] — a cancelled question cannot
//! be restored, and the caller that was waiting has already been told it will never be answered;
//! re-asking means a new question with a new id. Declaring it `write` would hide that irreversibility
//! from a policy that distinguishes the two.
//!
//! # Wire shapes
//!
//! ```text
//! userQuestions.list   {sessionId?, state?, limit?, offset?} -> {items, shown, total, limit, offset}
//! userQuestions.answer {id, answer: {selected, custom?}}     -> {id, state, answer, by}
//! userQuestions.cancel {id, reason?}                         -> {id, state, cancelled, reason}
//! ```
//!
//! One list item, and the record `answer`/`cancel` return, is shaped like upstream's
//! `AskUserQuestionItem` where the two agree:
//!
//! ```text
//! {
//!   "id": "q-…", "sessionId": …, "callId": …,
//!   "state": "open" | "answered" | "cancelled" | "expired",
//!   "shape": {"kind": "text"|"choice"|"multiple"|"confirm", "label": …, …},
//!   "context": …, "timeoutMs": 300000, "hasDefault": false, "createdAt": 1700000000000,
//!   "answer": {"selected": [...], "custom": …},   // only when answered
//!   "by": "human" | "default",                    // only when answered
//!   "reason": …                                   // only when cancelled with a reason
//! }
//! ```
//!
//! An answer travels as upstream's `AskUserQuestionAnswerItem` — `selected` holds option *values*
//! (upstream holds labels; [`QuestionOption`] carries both, and this build keys on `value` because a
//! caller that matched on a label would break when the wording changed) and `custom` holds free
//! text. A `confirm` question publishes two options, `yes` and `no`, so a client renders buttons
//! rather than guessing that a boolean-shaped answer is wanted; [`Answer::Confirmed`] is what those
//! two decode to.
//!
//! Optional members are `null` rather than omitted, so a client reads one shape for every item.
//!
//! # Why the paths go through `girder_core::`
//!
//! The crate aliases itself as `girder_core` (`extern crate self as girder_core`), and the
//! integration test includes this file by path because wiring the module into `lib.rs` belongs to a
//! different change. Spelling the paths through the crate root keeps one copy of them working from
//! inside the crate and from the test crate, and it restricts this module to the names the crate
//! actually exports — [`QuestionShape`] and [`QuestionOption`] rather than the private module's
//! `Shape` and `Option_`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use girder_core::{
    Answer, BoxFut, Declaration, Question, QuestionError, QuestionOption, QuestionReply,
    QuestionShape, RemoteCall, RemoteError, RemoteRegistry, now_millis,
};

// ---- surface constants ------------------------------------------------------------------------

/// The namespace every method here is served under.
///
/// Upstream's spelling, camel case, because a Typert-generated client asks for exactly this string
/// and a namespace differing in case would be a typo a client could not work around.
pub const NAMESPACE: &str = "userQuestions";

/// Lists questions, filtered and paged.
pub const LIST_METHOD: &str = "list";

/// Answers one question.
pub const ANSWER_METHOD: &str = "answer";

/// Cancels one question.
pub const CANCEL_METHOD: &str = "cancel";

/// One frame of the countdown a client shows while a question is open.
pub const ATTACH_WAIT_METHOD: &str = "attachWait";

/// Upstream methods in this namespace that this build does not serve.
///
/// Empty: every upstream method in this namespace is registered.
pub const UNSERVED_METHODS: &[&str] = &[];

/// The resource kind these methods touch, as reported to an access policy.
const RESOURCE_KIND: &str = "question";

/// The largest page a listing may ask for.
///
/// Refused rather than clamped above this: a client that asked for 10,000 and silently received 500
/// would believe it had read everything.
pub const MAX_LIST_LIMIT: u32 = 500;

/// The page size a listing gets when it asks for none.
pub const DEFAULT_LIST_LIMIT: u32 = 100;

/// The `selected` value a `confirm` question treats as yes.
pub const CONFIRM_YES: &str = "yes";

/// The `selected` value a `confirm` question treats as no.
pub const CONFIRM_NO: &str = "no";

/// The reason reported when a question was cancelled without one.
const NO_REASON: &str = "cancelled without a reason";

/// The state word a record reports while it can still take an answer.
const STATE_OPEN: &str = "open";

/// The state word a record reports once someone answered it.
const STATE_ANSWERED: &str = "answered";

/// The state word a record reports once it was cancelled.
const STATE_CANCELLED: &str = "cancelled";

/// The state word a record reports once its wait ran out with no default.
const STATE_EXPIRED: &str = "expired";

// ---- the record -------------------------------------------------------------------------------

/// Where one question stands.
///
/// A state, not a pair of flags: the three ways a wait ends are exclusive, and a record that could be
/// both answered and cancelled would let an answer be read from a question the person was no longer
/// being asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuestionState {
    /// Nothing has settled it; an answer and a cancel are both still possible.
    Open,
    /// Someone answered it. The reply records whether that was a person or the default.
    Answered(QuestionReply),
    /// The wait was released without an answer.
    Cancelled {
        /// Why, when the caller said.
        reason: Option<String>,
    },
    /// The wait ran out and the question declared no default.
    ///
    /// Distinct from [`Self::Cancelled`] because nothing asked for it: a client that sees `expired`
    /// knows its own countdown is what ended the question.
    Expired,
}

impl QuestionState {
    /// The wire word for this state.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => STATE_OPEN,
            Self::Answered(_) => STATE_ANSWERED,
            Self::Cancelled { .. } => STATE_CANCELLED,
            Self::Expired => STATE_EXPIRED,
        }
    }

    /// Whether this record can still take an answer.
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }

    /// The reason a cancel recorded, when it recorded one.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Cancelled { reason } => reason.as_deref(),
            _ => None,
        }
    }
}

/// One question a client may answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingQuestion {
    /// The id `answer` and `cancel` take.
    pub id: String,
    /// What is being asked, and how long the asker will wait.
    pub question: Question,
    /// The session that asked, when the asker named one.
    ///
    /// Carried so a client can route the question to the conversation it belongs to. Not part of the
    /// question: a question is the same question whichever session asked it.
    pub session_id: Option<String>,
    /// The tool call that asked, when the asker named one.
    pub call_id: Option<String>,
    /// When the record was created, in milliseconds since the epoch.
    pub created_at: u64,
    /// Where it stands.
    pub state: QuestionState,
}

impl PendingQuestion {
    /// Builds an open record.
    #[must_use]
    pub fn new(id: impl Into<String>, question: Question) -> Self {
        Self {
            id: id.into(),
            question,
            session_id: None,
            call_id: None,
            created_at: now_millis(),
            state: QuestionState::Open,
        }
    }

    /// Attributes it to a session and a tool call.
    #[must_use]
    pub fn attributed_to(mut self, session_id: Option<String>, call_id: Option<String>) -> Self {
        self.session_id = session_id;
        self.call_id = call_id;
        self
    }
}

// ---- storage seam -----------------------------------------------------------------------------

/// Why a store could not record a question or a change to one.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QuestionStoreError {
    /// No record holds that id.
    #[error("no question `{id}`")]
    NotFound {
        /// The id the caller sent.
        id: String,
    },

    /// The record is not open, and only an open record can be settled.
    ///
    /// The state travels with the error so a client learns *what* happened — answered, cancelled, or
    /// expired — rather than only that its call was late.
    #[error("question `{id}` is already `{state}`")]
    AlreadySettled {
        /// The id the caller sent.
        id: String,
        /// What the record had reached.
        state: String,
    },

    /// No storage is configured, and the message says what a host must supply.
    #[error("question storage is unavailable: {0}")]
    Unavailable(String),

    /// The storage exists and failed.
    ///
    /// Separate from [`Self::Unavailable`] because the two need different responses: a missing
    /// backend is an operator's configuration to fix, a failed write is worth retrying.
    ///
    /// Not constructed by the store in this module — it exists for the implementation a host
    /// supplies, and [`store_error`] already maps every store error to the wire, so removing it would
    /// leave a real store with no way to report "try again" as opposed to "nothing is configured".
    #[allow(
        dead_code,
        reason = "the variant is part of the trait's contract for host-supplied stores"
    )]
    #[error("question storage failed: {0}")]
    Backend(String),
}

/// Where questions are kept.
///
/// Every method is `&self` and the trait is `Send + Sync`, because a handler is called from many
/// requests at once: a store that needed `&mut self` would force the registry to serialise every
/// question call. Implementations provide their own interior mutability.
///
/// [`Self::transition`] is the whole state machine, on purpose. A store that exposed `set_state`
/// instead would let a caller skip the "only an open record may settle" rule, and that rule is what
/// makes a second answer an error rather than an overwrite.
///
/// No method here waits. Waiting belongs to [`QuestionQueue`], because a store that blocked would
/// hold its own lock across an await and turn one slow client into a stalled listing.
pub trait QuestionStore: Send + Sync {
    /// Records a question that is not yet settled.
    ///
    /// An `Err` means nothing was recorded, so an asker must not start waiting.
    fn insert(&self, pending: PendingQuestion) -> Result<(), QuestionStoreError>;

    /// Every record, in an order the store chooses.
    ///
    /// The order is not a contract: a listing sorts by creation time itself, because a store that is
    /// not a sorted map cannot promise that order cheaply.
    fn list(&self) -> Vec<PendingQuestion>;

    /// One record, or `None` when no record holds that id.
    fn get(&self, id: &str) -> Option<PendingQuestion>;

    /// Moves an open record to `to`, refusing one that already settled.
    ///
    /// The check and the change are one step under one lock; that is the whole reason this is a
    /// single method rather than a read and a write. `Ok` carries the record as stored — the caller
    /// receives what was recorded, not what it sent.
    ///
    /// # Errors
    ///
    /// Returns [`QuestionStoreError::NotFound`] for an unknown id and
    /// [`QuestionStoreError::AlreadySettled`] for a record that is not open.
    fn transition(
        &self,
        id: &str,
        to: QuestionState,
    ) -> Result<PendingQuestion, QuestionStoreError>;
}

/// A store that keeps records in this process's memory.
///
/// **Lost on restart.** It is the store for tests and for a host that wants the surface to work
/// before it has durable storage, not a durable record: a question that survived a restart with no
/// process waiting on it could only be answered into nothing.
#[derive(Debug, Default)]
pub struct MemoryQuestionStore {
    records: Mutex<BTreeMap<String, PendingQuestion>>,
}

impl MemoryQuestionStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many records are held, settled or not.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // Poisoning is recovered rather than propagated: a panic while holding this map leaves a table
    // of records, not a half-written one, and refusing every later call would turn one panic into a
    // permanently unusable question surface.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, PendingQuestion>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl QuestionStore for MemoryQuestionStore {
    fn insert(&self, pending: PendingQuestion) -> Result<(), QuestionStoreError> {
        // Insert, not insert-if-absent: the id is minted immediately before this call and the queue
        // has already refused an empty one, so an overwrite here could only be an id collision, and
        // overwriting reuses the slot rather than leaving a record nobody can list.
        self.lock().insert(pending.id.clone(), pending);
        Ok(())
    }

    fn list(&self) -> Vec<PendingQuestion> {
        self.lock().values().cloned().collect()
    }

    fn get(&self, id: &str) -> Option<PendingQuestion> {
        self.lock().get(id).cloned()
    }

    fn transition(
        &self,
        id: &str,
        to: QuestionState,
    ) -> Result<PendingQuestion, QuestionStoreError> {
        let mut records = self.lock();
        let Some(record) = records.get_mut(id) else {
            return Err(QuestionStoreError::NotFound { id: id.to_owned() });
        };
        // The compare-and-set. A `set_state` that skipped this would let the second of two concurrent
        // answers replace the first, which is the one thing the module documentation promises cannot
        // happen.
        if !record.state.is_open() {
            return Err(QuestionStoreError::AlreadySettled {
                id: id.to_owned(),
                state: record.state.as_str().to_owned(),
            });
        }
        record.state = to;
        Ok(record.clone())
    }
}

/// The store a deployment gets when it configures none.
///
/// Every write is refused and the reason names what is missing, following
/// [`crate::session::lifecycle_rpc::UnavailableArchiveStore`]: the alternative — silently keeping the
/// record in memory — would report a state the next call cannot confirm, and a client would show a
/// question that vanished on restart.
#[derive(Debug)]
pub struct UnavailableQuestionStore {
    reason: String,
}

impl UnavailableQuestionStore {
    /// Creates a store that refuses with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The reason the default registration would use.
    ///
    /// It names the object a host must supply, because a message that only said "not supported"
    /// would leave an operator guessing which half to wire.
    #[must_use]
    pub fn default_reason() -> Self {
        Self::new(
            "no QuestionStore was supplied; this module ships only an in-process store whose records \
             vanish on restart, so a host must pass its own Arc<dyn QuestionStore> to a \
             QuestionQueue",
        )
    }

    /// The reason every refusal carries.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl QuestionStore for UnavailableQuestionStore {
    fn insert(&self, _pending: PendingQuestion) -> Result<(), QuestionStoreError> {
        Err(QuestionStoreError::Unavailable(self.reason.clone()))
    }

    fn list(&self) -> Vec<PendingQuestion> {
        // Empty rather than a refusal, because the signature has no error channel. Consistent with
        // the writes: this store holds nothing, so nothing is what it has.
        Vec::new()
    }

    fn get(&self, _id: &str) -> Option<PendingQuestion> {
        None
    }

    fn transition(
        &self,
        _id: &str,
        _to: QuestionState,
    ) -> Result<PendingQuestion, QuestionStoreError> {
        Err(QuestionStoreError::Unavailable(self.reason.clone()))
    }
}

// ---- the waiting side -------------------------------------------------------------------------

/// Why a wait for an answer ended without one.
///
/// Separate from [`QuestionError`] because these are not validation failures of a question: they are
/// the three ways a *wait* can end, and the host decides what each means to the caller that asked.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AskError {
    /// The wait elapsed and the question declared no default.
    #[error(
        "question `{id}` expired before anyone answered, and it declared no default to take instead"
    )]
    TimedOut {
        /// The record's id.
        id: String,
    },

    /// Someone cancelled the question while the asker was waiting.
    #[error("question `{id}` was cancelled: {reason}")]
    Cancelled {
        /// The record's id.
        id: String,
        /// Why, or [`NO_REASON`] when the caller said nothing.
        reason: String,
    },

    /// The record settled in a state this wait cannot turn into a reply.
    ///
    /// Unreachable through the paths in this module — a wait is only ever released by an answer, a
    /// cancel, or its own expiry — and reported rather than asserted so a store that invented a
    /// fourth state fails a call instead of panicking a request path.
    #[error("question `{id}` settled as `{state}`, which is not an answer")]
    Unexpected {
        /// The record's id.
        id: String,
        /// The state a store reported.
        state: String,
    },

    /// The store refused or failed.
    #[error(transparent)]
    Store(#[from] QuestionStoreError),
}

/// One question as an asker hands it over.
///
/// A struct rather than four arguments to [`QuestionQueue::ask`]: the question is the part that
/// matters, and the two identities are optional attribution a caller either has or does not.
#[derive(Clone, Debug)]
pub struct AskRequest {
    /// What to ask.
    pub question: Question,
    /// The session asking, when there is one.
    pub session_id: Option<String>,
    /// The tool call asking, when there is one.
    pub call_id: Option<String>,
}

impl AskRequest {
    /// Wraps a question with no attribution.
    #[must_use]
    pub fn new(question: Question) -> Self {
        Self {
            question,
            session_id: None,
            call_id: None,
        }
    }

    /// Attributes it to a session and a tool call.
    #[must_use]
    pub fn attributed_to(mut self, session_id: Option<String>, call_id: Option<String>) -> Self {
        self.session_id = session_id;
        self.call_id = call_id;
        self
    }
}

impl From<Question> for AskRequest {
    fn from(question: Question) -> Self {
        Self::new(question)
    }
}

/// A store of pending questions plus the waiters parked on them.
///
/// The RPC methods and the asking side share one of these: `answer` settles a record and releases
/// whoever is in [`Self::ask`], which is what makes the two halves one surface rather than two
/// tables that happen to describe similar things.
pub struct QuestionQueue {
    store: Arc<dyn QuestionStore>,
    /// One entry per waiting question id.
    ///
    /// A `Vec` rather than a single sender because a wait is not exclusive: two callers may legitimately
    /// watch one question (a retry, a second process), and whichever settles it must release both.
    waiters: Mutex<BTreeMap<String, Vec<oneshot::Sender<QuestionState>>>>,
    /// Wakes `attachWait` and `session.wait` after a question settles. The record remains the state.
    changed: std::sync::Arc<tokio::sync::Notify>,
}

// Hand-written rather than derived: the store is a trait object, and a `Debug` render that printed
// only the waiter count would be a snapshot a reader could mistake for the records themselves.
impl std::fmt::Debug for QuestionQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionQueue")
            .field("records", &self.store.list().len())
            .field("waiting", &self.lock_waiters().len())
            .finish_non_exhaustive()
    }
}

impl QuestionQueue {
    /// Creates a queue over `store`.
    #[must_use]
    pub fn new(store: Arc<dyn QuestionStore>) -> Self {
        Self {
            store,
            waiters: Mutex::new(BTreeMap::new()),
            changed: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Resolves after the next successful settle.
    ///
    /// Enable the future before reading the queue. A settle that lands after that registration
    /// wakes it; one that landed earlier is visible in the read.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }

    /// The same signal [`Self::notified`] waits on, shared with session observers.
    #[must_use]
    pub fn activity(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.changed)
    }

    /// Creates a queue over a fresh in-memory store.
    ///
    /// The convenience for a host that has no durable storage yet, and what the tests use. The store
    /// is lost on restart — see [`MemoryQuestionStore`].
    #[must_use]
    pub fn in_memory() -> Self {
        Self::new(Arc::new(MemoryQuestionStore::new()))
    }

    /// The store behind this queue.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn QuestionStore> {
        &self.store
    }

    /// Records a question and waits for it to settle.
    ///
    /// The wait is `question.timeout`. A question with a default settles with
    /// [`QuestionReply::defaulted`] when that elapses; one without is marked
    /// [`QuestionState::Expired`] and reports [`AskError::TimedOut`], because a question whose answer
    /// changes what happens must not be defaulted silently.
    ///
    /// Cancelling the future (dropping it) leaves the record open: nothing here infers intent from a
    /// dropped waiter, and the caller that wants the record released calls [`Self::cancel`] — which
    /// is also what a client disconnecting does over RPC.
    ///
    /// # Errors
    ///
    /// Returns [`AskError::Store`] when the store refuses, [`AskError::Cancelled`] when the question
    /// was cancelled while waiting, and [`AskError::TimedOut`] when the wait elapsed with no default.
    pub async fn ask(&self, request: AskRequest) -> Result<QuestionReply, AskError> {
        let AskRequest {
            question,
            session_id,
            call_id,
        } = request;
        let id = mint_id();
        let record =
            PendingQuestion::new(id.clone(), question.clone()).attributed_to(session_id, call_id);
        self.store.insert(record)?;

        let (sender, receiver) = oneshot::channel();
        // Registered and then re-checked, in that order, under the waiter lock: a settle either
        // happened before this point — and the record already says so — or it happens afterwards and
        // finds the sender. Reading the record first and registering second would leave a window in
        // which an answer is recorded and the waiter never hears about it.
        {
            let mut waiters = self.lock_waiters();
            waiters.entry(id.clone()).or_default().push(sender);
            if let Some(record) = self.store.get(&id) {
                if !record.state.is_open() {
                    // Settled while this call was registering: its `settle` already drained the
                    // waiter list, so the sender just pushed is the only one here and the recorded
                    // state is the outcome. Taken from the record rather than from a channel that
                    // will never carry a value.
                    waiters.remove(&id);
                    return completion(&id, record.state);
                }
            } else {
                waiters.remove(&id);
                return Err(AskError::Store(QuestionStoreError::NotFound { id }));
            }
        }

        match tokio::time::timeout(question.timeout, receiver).await {
            Ok(Ok(state)) => completion(&id, state),
            // The sender was dropped without sending: only the queue itself removes a sender, and it
            // settles the record before it does, so this means the store lost the record. Reported
            // rather than retried, because retrying would mint a second question for one ask.
            Ok(Err(_)) => Err(AskError::Store(QuestionStoreError::NotFound { id })),
            Err(_elapsed) => self.expire(id, question),
        }
    }

    /// [`Self::ask`] as a `'static` future, for a host mounting a
    /// [`Responder`](crate::Responder).
    ///
    /// `ask` borrows the queue, and a `Responder::ask` must return a future that outlives its own
    /// call. Cloning the `Arc` into the future is the whole difference, so it lives here rather than
    /// in every host's adapter.
    pub fn ask_owned(
        self: Arc<Self>,
        request: AskRequest,
    ) -> BoxFut<Result<QuestionReply, AskError>> {
        Box::pin(async move { self.ask(request).await })
    }

    /// Answers one question.
    ///
    /// The reply is recorded as given; validating it against the question's shape is the caller's
    /// step, because the wire form is decoded there and a store that also validated would be a second
    /// copy of the rule. What this method decides is the race: exactly one reply is recorded, and a
    /// second call reports [`QuestionStoreError::AlreadySettled`] with the state that won.
    ///
    /// # Errors
    ///
    /// Returns [`QuestionStoreError::NotFound`] for an unknown id and
    /// [`QuestionStoreError::AlreadySettled`] when the record is not open.
    pub fn answer(
        &self,
        id: &str,
        reply: QuestionReply,
    ) -> Result<PendingQuestion, QuestionStoreError> {
        self.settle(id, QuestionState::Answered(reply))
    }

    /// Cancels one question, releasing whoever was waiting.
    ///
    /// Cancelling a settled question is refused rather than ignored: a cancel that reported success
    /// for an already-answered question would tell the caller it stopped something that had already
    /// happened.
    ///
    /// # Errors
    ///
    /// Returns [`QuestionStoreError::NotFound`] for an unknown id and
    /// [`QuestionStoreError::AlreadySettled`] when the record is not open.
    pub fn cancel(
        &self,
        id: &str,
        reason: Option<String>,
    ) -> Result<PendingQuestion, QuestionStoreError> {
        self.settle(id, QuestionState::Cancelled { reason })
    }

    /// Every record, in the order they were created.
    ///
    /// Sorted by creation time and then by id, not left to the store: a page that reordered between
    /// calls would make `offset` return rows the previous page already showed.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingQuestion> {
        let mut records = self.store.list();
        records.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        records
    }

    /// One record, if it exists.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<PendingQuestion> {
        self.store.get(id)
    }

    /// The store's lock over the waiter table.
    // Poisoning is recovered, as in `MemoryQuestionStore::lock`: the table holds senders, and a
    // panic elsewhere must not make every later answer fail.
    fn lock_waiters(
        &self,
    ) -> MutexGuard<'_, BTreeMap<String, Vec<oneshot::Sender<QuestionState>>>> {
        self.waiters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Moves a record and releases its waiters, under the waiter table's lock.
    ///
    /// The waiter lock is taken before the store is touched, and the queue's own registration does
    /// the same in the same order, so a registration is never interleaved with a settle: see the
    /// module documentation's note on the handshake.
    fn settle(&self, id: &str, to: QuestionState) -> Result<PendingQuestion, QuestionStoreError> {
        let mut waiters = self.lock_waiters();
        let record = self.store.transition(id, to)?;
        for sender in waiters.remove(id).unwrap_or_default() {
            // The receiver may be gone because its `ask` future was dropped or its timeout already
            // fired. Either way nothing is waiting for this value, and failing the settle over it
            // would refuse an answer that is legitimately recorded.
            let _ = sender.send(record.state.clone());
        }
        drop(waiters);
        self.changed.notify_waiters();
        Ok(record)
    }

    /// Ends a wait whose timeout elapsed.
    ///
    /// A question with a default settles with it, recorded as a default rather than as a person's
    /// answer. One without is marked [`QuestionState::Expired`] so a later answer is refused instead
    /// of being recorded against a wait that no longer exists.
    fn expire(&self, id: String, question: Question) -> Result<QuestionReply, AskError> {
        let to = match question.default {
            Some(default) => QuestionState::Answered(QuestionReply::defaulted(default)),
            None => QuestionState::Expired,
        };
        match self.settle(&id, to) {
            Ok(record) => completion(&id, record.state),
            // Someone settled it in the instant the timeout fired: their settlement is the outcome,
            // and their answer is a better result than the default this path was about to record. The
            // record is re-read for it, because the conflict error carries the state's name rather
            // than the reply itself.
            Err(QuestionStoreError::AlreadySettled { .. }) => match self.store.get(&id) {
                Some(record) => completion(&id, record.state),
                None => Err(AskError::Store(QuestionStoreError::NotFound { id })),
            },
            Err(error) => Err(AskError::Store(error)),
        }
    }
}

/// Turns a settled state into what an asker receives.
fn completion(id: &str, state: QuestionState) -> Result<QuestionReply, AskError> {
    match state {
        QuestionState::Answered(reply) => Ok(reply),
        QuestionState::Cancelled { reason } => Err(AskError::Cancelled {
            id: id.to_owned(),
            reason: reason.unwrap_or_else(|| NO_REASON.to_owned()),
        }),
        QuestionState::Expired => Err(AskError::TimedOut { id: id.to_owned() }),
        QuestionState::Open => Err(AskError::Unexpected {
            id: id.to_owned(),
            state: STATE_OPEN.to_owned(),
        }),
    }
}

/// Mints an id no other call in this process can mint.
///
/// A timestamp, the process id, and a process-wide counter: the timestamp keeps a log roughly
/// chronological, the counter is what actually guarantees uniqueness inside the process (two calls in
/// one millisecond), and the process id keeps two processes sharing a log apart. No randomness,
/// because none is needed for an id that is compared, never guessed.
fn mint_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("q-{:x}-{:x}-{sequence:x}", now_millis(), std::process::id())
}

// ---- registration -----------------------------------------------------------------------------

/// Registers the `userQuestions.*` methods against `questions`.
///
/// Declarations are registered beside each handler so the policy judges a call before it runs; the
/// queue is cloned into each closure, so a host that keeps its own `Arc` clone can use the same
/// object as the asking side ([`QuestionQueue::ask`]) and the RPC surface at once — which is the
/// point of the module.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the owned Arc is the host contract: a caller that kept only a temporary queue would \
              otherwise have to name a binding solely to lend it, and the three handlers need one \
              clone each anyway"
)]
pub fn register_question_methods(registry: &mut RemoteRegistry, questions: Arc<QuestionQueue>) {
    registry.declare(NAMESPACE, LIST_METHOD, Declaration::read(RESOURCE_KIND));
    registry.declare(NAMESPACE, ANSWER_METHOD, Declaration::write(RESOURCE_KIND));
    // A cancelled question cannot be restored: the record keeps the fact, and the caller that was
    // waiting has already been told it will never be answered. Re-asking is a new question with a new
    // id, which is why this is `destructive` and not `write`.
    registry.declare(
        NAMESPACE,
        CANCEL_METHOD,
        Declaration::destructive(RESOURCE_KIND),
    );

    let list_queue = Arc::clone(&questions);
    registry.register(NAMESPACE, LIST_METHOD, move |call: RemoteCall| {
        let queue = Arc::clone(&list_queue);
        Box::pin(async move { list(&call.args, &queue) })
    });

    let answer_queue = Arc::clone(&questions);
    registry.register(NAMESPACE, ANSWER_METHOD, move |call: RemoteCall| {
        let queue = Arc::clone(&answer_queue);
        Box::pin(async move { answer(&call.args, &queue) })
    });

    let cancel_queue = Arc::clone(&questions);
    registry.register(NAMESPACE, CANCEL_METHOD, move |call: RemoteCall| {
        let queue = Arc::clone(&cancel_queue);
        Box::pin(async move { cancel(&call.args, &queue) })
    });

    registry.declare(
        NAMESPACE,
        ATTACH_WAIT_METHOD,
        Declaration::read(RESOURCE_KIND),
    );
    let wait_queue = Arc::clone(&questions);
    registry.register_stream(NAMESPACE, ATTACH_WAIT_METHOD, move |open| {
        let queue = Arc::clone(&wait_queue);
        Box::pin(async move { follow_attach_wait(open, queue).await })
    });
}

async fn follow_attach_wait(open: girder_core::StreamOpen, queue: Arc<QuestionQueue>) {
    let Some(call_id) = argument(&open.call.args, "callId") else {
        let _ = open.fail(invalid(ATTACH_WAIT_METHOD, "`callId` is required"));
        return;
    };
    let session =
        argument(&open.call.args, "sessionId").or_else(|| argument(&open.call.args, "agentId"));
    let Some(record) = matching_open(&queue, call_id, session) else {
        return;
    };
    if !open.push(json!({ "remainingMs": remaining_ms(&record) })) {
        return;
    }
    loop {
        let notified = queue.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if matching_open(&queue, call_id, session).is_none() {
            return;
        }
        tokio::select! {
            () = open.cancel.cancelled() => return,
            incoming = open.uplink.recv() => {
                if !matches!(incoming, Ok(Some(_))) {
                    return;
                }
            }
            () = notified => {}
        }
    }
}

fn argument<'a>(args: &'a Value, field: &str) -> Option<&'a str> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn matching_open(
    queue: &QuestionQueue,
    call_id: &str,
    session: Option<&str>,
) -> Option<PendingQuestion> {
    queue.pending().into_iter().find(|record| {
        record.state.is_open()
            && record.call_id.as_deref() == Some(call_id)
            && session.is_none_or(|id| record.session_id.as_deref() == Some(id))
    })
}

fn remaining_ms(record: &PendingQuestion) -> u64 {
    let budget = u64::try_from(record.question.timeout.as_millis()).unwrap_or(u64::MAX);
    budget.saturating_sub(now_millis().saturating_sub(record.created_at))
}

// ---- requests ---------------------------------------------------------------------------------

/// The body of `userQuestions.list`.
#[derive(Debug, Deserialize)]
struct ListRequest {
    /// Restricts the listing to one session's questions.
    #[serde(default, rename = "sessionId", alias = "session_id")]
    session_id: Option<String>,
    /// `open`, `answered`, `cancelled`, or `expired`. Defaults to `open`: the listing exists to show
    /// what can still be answered.
    #[serde(default)]
    state: Option<String>,
    /// Page size. Refused rather than clamped when out of range.
    #[serde(default)]
    limit: Option<u32>,
    /// Records to skip after sorting.
    #[serde(default)]
    offset: Option<u32>,
}

/// The body of `userQuestions.answer`.
#[derive(Debug, Deserialize)]
struct AnswerRequest {
    /// The question's id.
    #[serde(default)]
    id: Option<String>,
    /// The answer, in upstream's `AskUserQuestionAnswerItem` form.
    #[serde(default)]
    answer: Option<AnswerItemRequest>,
}

/// One answer, shaped as upstream's `AskUserQuestionAnswerItem`.
///
/// `id` is not read from here even though upstream carries it: the method already takes the id as an
/// argument, and accepting a second copy would leave two ids that could disagree about which record
/// is being answered — and the tie-break would be arbitrary.
#[derive(Debug, Default, Deserialize)]
struct AnswerItemRequest {
    /// The option values chosen.
    #[serde(default)]
    selected: Vec<String>,
    /// Free text, for an `Other` answer or a text question.
    #[serde(default)]
    custom: Option<String>,
}

/// The body of `userQuestions.cancel`.
#[derive(Debug, Deserialize)]
struct CancelRequest {
    /// The question's id.
    #[serde(default)]
    id: Option<String>,
    /// Why it is being cancelled, recorded so a later reader knows what ended it.
    #[serde(default)]
    reason: Option<String>,
}

// ---- handlers ---------------------------------------------------------------------------------
//
// The handlers are plain functions, not `async fn`s: every queue call is synchronous, so there is
// nothing to await, and clippy's `unused_async` is right that the keyword would be noise. The futures
// lie one layer up, in the registration closures, which is the only place `RemoteMethod` requires one.

/// Runs one `userQuestions.list`.
fn list(args: &Value, queue: &Arc<QuestionQueue>) -> Result<Value, RemoteError> {
    let request: ListRequest = parse_request(args, LIST_METHOD)?;
    let limit = request.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(invalid(
            LIST_METHOD,
            format!("`limit` must be between 1 and {MAX_LIST_LIMIT}, got {limit}"),
        ));
    }
    let offset = request.offset.unwrap_or(0);
    let wanted = match request.state.as_deref().map(str::trim) {
        None | Some("") => STATE_OPEN,
        Some(state) => match state {
            STATE_OPEN | STATE_ANSWERED | STATE_CANCELLED | STATE_EXPIRED => state,
            other => {
                return Err(invalid(
                    LIST_METHOD,
                    format!(
                        "`state` must be `{STATE_OPEN}`, `{STATE_ANSWERED}`, `{STATE_CANCELLED}`, \
                         or `{STATE_EXPIRED}`, got `{other}`"
                    ),
                ));
            }
        },
    };

    let records: Vec<PendingQuestion> = queue
        .pending()
        .into_iter()
        .filter(|record| record.state.as_str() == wanted)
        .filter(|record| {
            request
                .session_id
                .as_deref()
                .is_none_or(|session| record.session_id.as_deref() == Some(session))
        })
        .collect();

    let total = u32::try_from(records.len()).unwrap_or(u32::MAX);
    let items: Vec<Value> = records
        .iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(entry)
        .collect();

    Ok(json!({
        "items": items,
        "shown": items.len(),
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
}

/// Runs one `userQuestions.answer`.
fn answer(args: &Value, queue: &Arc<QuestionQueue>) -> Result<Value, RemoteError> {
    let request: AnswerRequest = parse_request(args, ANSWER_METHOD)?;
    let id = require_id(request.id.as_deref(), ANSWER_METHOD)?;
    let item = request.answer.ok_or_else(|| {
        invalid(
            ANSWER_METHOD,
            "`answer` is required: an object of `selected` option values and an optional `custom` text",
        )
    })?;

    let Some(record) = queue.get(&id) else {
        return Err(no_question(ANSWER_METHOD, &id));
    };
    // Checked before the answer is even looked at, so a stale card is told what happened to its
    // question rather than what was wrong with the answer typed into it. This read is not what makes
    // the race safe -- `QuestionQueue::answer` performs the compare-and-set -- it is what makes the
    // *message* right for the common case of a card that outlived its question.
    if !record.state.is_open() {
        return Err(store_error(
            ANSWER_METHOD,
            &QuestionStoreError::AlreadySettled {
                id: record.id.clone(),
                state: record.state.as_str().to_owned(),
            },
        ));
    }
    // Decoded and validated against the shape the question declared, before the record is touched: a
    // refused answer must not settle anything, and `QuestionShape::accepts` is the one rule every
    // question validates by.
    let decoded = decode_answer(&record.question.shape, &item, ANSWER_METHOD)?;

    let settled = queue
        .answer(&id, QuestionReply::human(decoded))
        .map_err(|error| store_error(ANSWER_METHOD, &error))?;

    // The response reports what was *recorded*, read back from the settled record rather than from
    // the request: a concurrent answer that won the race would otherwise be echoed as if this call
    // had recorded it, and the two replies could differ.
    match settled.state {
        QuestionState::Answered(reply) => Ok(json!({
            "id": settled.id,
            // The state describes the record; `by` describes who answered. A separate word per
            // author would be a second axis a client has to learn for no new information.
            "state": STATE_ANSWERED,
            "answer": answer_item(&reply.answer),
            "by": reply.by.as_str(),
        })),
        other => Err(RemoteError::Failed(format!(
            "`{NAMESPACE}.{ANSWER_METHOD}`: question `{id}` was recorded as `{}` after being answered",
            other.as_str()
        ))),
    }
}

/// Runs one `userQuestions.cancel`.
fn cancel(args: &Value, queue: &Arc<QuestionQueue>) -> Result<Value, RemoteError> {
    let request: CancelRequest = parse_request(args, CANCEL_METHOD)?;
    let id = require_id(request.id.as_deref(), CANCEL_METHOD)?;

    let settled = queue
        .cancel(&id, request.reason.clone())
        .map_err(|error| store_error(CANCEL_METHOD, &error))?;

    match settled.state {
        QuestionState::Cancelled { reason } => Ok(json!({
            "id": settled.id,
            "state": STATE_CANCELLED,
            "cancelled": true,
            "reason": reason,
        })),
        other => Err(RemoteError::Failed(format!(
            "`{NAMESPACE}.{CANCEL_METHOD}`: question `{id}` was recorded as `{}` after being \
             cancelled",
            other.as_str()
        ))),
    }
}

// ---- answer decoding --------------------------------------------------------------------------

/// Decodes one wire answer against the shape its question declared.
///
/// Every branch ends in [`QuestionShape::accepts`], the crate's single validation rule: a decoder
/// that validated by itself would be a second copy of the rule, and the copy is what would drift —
/// accepting an answer the question never offered. The decode step only decides *which* [`Answer`]
/// variant the item means.
fn decode_answer(
    shape: &QuestionShape,
    item: &AnswerItemRequest,
    method: &str,
) -> Result<Answer, RemoteError> {
    let answer = match shape {
        QuestionShape::Text { .. } => {
            // `custom` is where a text answer belongs; a single `selected` entry is accepted because
            // a client with a text field and no custom concept puts the typed value there. Several
            // selections are refused rather than joined: a text question has one value, and a silent
            // join would invent a separator the asker never chose.
            if let Some(text) = &item.custom {
                Answer::Text(text.clone())
            } else if let Some(value) = single(&item.selected) {
                Answer::Text(value.to_owned())
            } else if item.selected.is_empty() {
                Answer::Text(String::new())
            } else {
                return Err(invalid(
                    method,
                    format!(
                        "a text question takes one value in `custom`, got {} selections",
                        item.selected.len()
                    ),
                ));
            }
        }
        QuestionShape::Choice { .. } => {
            if let Some(custom) = item
                .custom
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                // A choice answers with a value the question offered. Accepting free text here would
                // let a client answer an option list with something not on it and call that an
                // answer, which the question's own shape would then reject anyway with a worse
                // message.
                return Err(invalid(
                    method,
                    format!(
                        "this question offers choices, so `{custom}` is not an answer; send one of `selected`"
                    ),
                ));
            }
            let Some(value) = single(&item.selected) else {
                return Err(invalid(
                    method,
                    format!(
                        "a single-choice question takes exactly one `selected` value, got {}",
                        item.selected.len()
                    ),
                ));
            };
            Answer::Chosen(value.to_owned())
        }
        QuestionShape::Multiple { .. } => {
            if let Some(custom) = item
                .custom
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                return Err(invalid(
                    method,
                    format!(
                        "this question offers choices, so `{custom}` is not an answer; send them in `selected`"
                    ),
                ));
            }
            Answer::ChosenMany(item.selected.clone())
        }
        QuestionShape::Confirm { .. } => {
            let Some(value) = single(&item.selected) else {
                return Err(invalid(
                    method,
                    format!(
                        "a confirmation takes exactly one `selected` value, `{CONFIRM_YES}` or \
                         `{CONFIRM_NO}`, got {}",
                        item.selected.len()
                    ),
                ));
            };
            match value.trim().to_lowercase().as_str() {
                CONFIRM_YES => Answer::Confirmed(true),
                CONFIRM_NO => Answer::Confirmed(false),
                other => {
                    return Err(invalid(
                        method,
                        format!(
                            "`{other}` is not a confirmation value: send `{CONFIRM_YES}` or \
                             `{CONFIRM_NO}`"
                        ),
                    ));
                }
            }
        }
    };

    // The shared rule. `QuestionError` already distinguishes "not one of the options", "too few",
    // "too many", and "blank where a value is required", so its message travels as the argument
    // error instead of being restated here.
    shape
        .accepts(&answer)
        .map_err(|error: QuestionError| invalid(method, error.to_string()))?;
    Ok(answer)
}

/// The one element of `values`, when there is exactly one.
fn single(values: &[String]) -> Option<&str> {
    match values {
        [value] => Some(value.as_str()),
        _ => None,
    }
}

// ---- response pieces --------------------------------------------------------------------------

/// One record as a listing reports it.
fn entry(record: &PendingQuestion) -> Value {
    let mut value = json!({
        "id": record.id,
        "sessionId": record.session_id,
        "callId": record.call_id,
        "state": record.state.as_str(),
        "shape": shape_json(&record.question.shape),
        "context": record.question.context,
        // Milliseconds, because a client counts down in whatever unit its timer uses and a
        // pre-formatted duration would be a string it has to parse back.
        "timeoutMs": u64::try_from(record.question.timeout.as_millis()).unwrap_or(u64::MAX),
        // Whether a timeout takes the default, so a client can tell "nobody answered and the caller
        // chose X" from "nobody answered and the call will fail".
        "hasDefault": record.question.default.is_some(),
        "createdAt": record.created_at,
    });
    if let QuestionState::Answered(reply) = &record.state {
        // The recorded answer and its author, on the same record: a client that showed an answer
        // without `by` would present a default as a person's decision.
        if let Some(map) = value.as_object_mut() {
            map.insert("answer".to_owned(), answer_item(&reply.answer));
            map.insert("by".to_owned(), json!(reply.by.as_str()));
        }
    }
    if let QuestionState::Cancelled {
        reason: Some(reason),
    } = &record.state
    {
        if let Some(map) = value.as_object_mut() {
            map.insert("reason".to_owned(), json!(reason));
        }
    }
    value
}

/// The wire form of a shape, as a client needs it to render a control before anything is typed.
fn shape_json(shape: &QuestionShape) -> Value {
    match shape {
        QuestionShape::Text { label, optional } => json!({
            "kind": "text",
            "label": label,
            "optional": optional,
        }),
        QuestionShape::Choice { label, options } => json!({
            "kind": "choice",
            "label": label,
            "options": options.iter().map(option_json).collect::<Vec<Value>>(),
        }),
        QuestionShape::Multiple {
            label,
            options,
            min,
            max,
        } => json!({
            "kind": "multiple",
            "label": label,
            "options": options.iter().map(option_json).collect::<Vec<Value>>(),
            // Published because a client that offered every box and accepted none would produce an
            // answer this build rejects, one round trip later.
            "min": min,
            "max": max,
        }),
        QuestionShape::Confirm { label } => json!({
            "kind": "confirm",
            "label": label,
            // A confirmation has no option list of its own, so it is published with the two values
            // `decode_answer` accepts. Without them a client would have to guess the encoding of a
            // boolean answer.
            "options": [
                {"value": CONFIRM_YES, "label": "Yes", "detail": Value::Null},
                {"value": CONFIRM_NO, "label": "No", "detail": Value::Null},
            ],
        }),
    }
}

/// The wire form of one option.
///
/// `value` and `label` both travel: the value is what comes back in an answer and the label is what a
/// person reads, and a client given only one of them would either show an identifier or send human
/// wording as the answer.
fn option_json(option: &QuestionOption) -> Value {
    json!({
        "value": option.value,
        "label": option.label,
        "detail": option.detail,
    })
}

/// The wire form of a recorded answer, as upstream's `AskUserQuestionAnswerItem`.
fn answer_item(answer: &Answer) -> Value {
    match answer {
        Answer::Text(text) => json!({"selected": [], "custom": text}),
        Answer::Chosen(value) => json!({"selected": [value], "custom": Value::Null}),
        Answer::ChosenMany(values) => json!({"selected": values, "custom": Value::Null}),
        Answer::Confirmed(yes) => {
            let value = if *yes { CONFIRM_YES } else { CONFIRM_NO };
            json!({"selected": [value], "custom": Value::Null})
        }
    }
}

// ---- helpers ----------------------------------------------------------------------------------

/// Parses one method's arguments into its request type.
fn parse_request<T>(args: &Value, method: &str) -> Result<T, RemoteError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(args.clone()).map_err(|error| RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: error.to_string(),
    })
}

/// Extracts an id, refusing a missing or blank one.
///
/// Blank is refused rather than looked up: an empty id is a client bug, and reporting it as "no such
/// question" would send the caller looking for a question instead of a field.
fn require_id(id: Option<&str>, method: &str) -> Result<String, RemoteError> {
    match id.map(str::trim) {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(invalid(method, "`id` is required")),
    }
}

/// An invalid-argument error for `method`.
fn invalid(method: &str, message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: message.into(),
    }
}

/// The error an unknown id reports.
fn no_question(method: &str, id: &str) -> RemoteError {
    invalid(method, format!("no question `{id}`"))
}

/// Turns a store failure into an RPC failure that names what went wrong.
///
/// A settled record is [`RemoteError::InvalidArguments`] rather than [`RemoteError::Failed`]: the
/// caller's request was well formed but the state it assumed is gone, which is the distinction that
/// tells a client to refresh its view rather than to retry the same call. A store that is missing or
/// broken is `Failed`, because there the deployment is what is wrong and a retry may succeed.
fn store_error(method: &str, error: &QuestionStoreError) -> RemoteError {
    match error {
        QuestionStoreError::NotFound { id } => no_question(method, id),
        QuestionStoreError::AlreadySettled { id, state } => invalid(
            method,
            format!(
                "question `{id}` is already `{state}`: a second answer would replace an answer that \
                 was already recorded, so it is refused"
            ),
        ),
        QuestionStoreError::Unavailable(reason) | QuestionStoreError::Backend(reason) => {
            RemoteError::Failed(format!("`{NAMESPACE}.{method}`: {reason}"))
        }
    }
}
