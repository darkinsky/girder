//! The `goal.*` Remote methods: a client's way into a session's durable objective.
//!
//! [`crate::goal`] already answers every question about a goal's *lifecycle* — which transitions
//! exist, what a stale revision means, what a block reason has to say — and it answers them with
//! [`GoalState::create`], [`GoalState::pause`], [`GoalState::resume`], [`GoalState::complete`],
//! [`GoalState::block`], [`GoalState::edit`], and [`GoalState::clear`]. What it deliberately does
//! not answer is *how a client reaches any of them*: it has exactly one free function,
//! [`crate::goal::check_reference`], and its state is a value a caller owns rather than a surface a
//! socket can address. So until this module existed, no goal could be created over RPC at all, while
//! upstream advertises a `goal` namespace. This module is that entrance.
//!
//! # This module implements no lifecycle rules of its own
//!
//! Every mutation below calls straight into [`GoalState`] and lets it refuse. That is the whole
//! design: a second copy of the transition table here would disagree with the first one about some
//! edge — pausing a blocked goal, editing a complete one, resuming an already-armed active goal —
//! and the disagreement would surface as a state the domain module considers impossible. The table
//! in [`crate::goal`]'s documentation *is* this module's contract, by delegation.
//!
//! Concretely: [`Phase`], [`GoalRef`], [`GoalError`], [`Activation`] are the domain's types. Nothing
//! here redefines, mirrors, or flattens them.
//!
//! # Upstream's namespace, and what is served here
//!
//! Upstream's `GoalService` declares seven `@Remote` methods:
//!
//! | upstream | here | notes |
//! |---|---|---|
//! | `get` | [`get`] | serves [`GET_METHOD`] and [`LIST_METHOD`] |
//! | `create` | [`create`] | mints an id; arms the goal |
//! | `edit` | [`update`] | served as `update` **and** `edit` |
//! | `pause` | [`pause`] | |
//! | `resume` | [`resume`] | |
//! | `complete` | [`complete`] | |
//! | `clear` | [`clear`] | |
//!
//! `block` is a public method on the upstream service but carries no `@Remote` decorator, so it is
//! not part of the remote surface; [`block`] serves it anyway because a blocked goal is a state the
//! wire can otherwise create (`GoalState::block` exists) and cannot undo except by resuming.
//!
//! The task asked for `list`/`get`/`create`/`update`; all four are present, and `update` is the
//! name asked for while `edit` is the name upstream uses — both spellings reach one handler, so a
//! client written against either build works, and there is no second implementation to drift.
//!
//! `disarm` is **not** served: it clears process-local activation without a durable change, and a
//! remote peer asking a server to stop continuing a goal it never asked it to continue is a
//! distinction this surface has no peer identity to make. A caller that wants that arming gone asks
//! for `pause`, which is durable and visible.
//!
//! # One goal per store, which is what upstream means by "current"
//!
//! Upstream keys a goal by the *live agent* it belongs to. This crate has no agent registry in its
//! goal module: [`GoalState`] holds one optional current goal, and [`GoalState::create`] already
//! refuses a second one while the first is open. So the store below holds a map keyed by goal id,
//! and a second open goal is refused by `GoalState` rather than by this module. The consequence is
//! honest and visible: `list` returns at most one *current* goal plus any completed ones that were
//! not cleared, and an id that is not the current goal answers [`GoalError::NotCurrent`] on every
//! mutation.
//!
//! # Where the store is
//!
//! [`GoalStore`] is the only place a goal is kept, and [`MemoryGoalStore`] is the only
//! implementation shipped. It is **process-local**: a goal is lost on restart, which is exactly the
//! restart rule [`crate::goal`] already encodes (a new process must be *asked* before it continues
//! an objective). A host with durable storage passes its own `Arc<dyn GoalStore>`; nothing in this
//! module reads a file.
//!
//! # Wire shapes
//!
//! ```text
//! goal.list     {}                                   -> {items, total, activated}
//! goal.get      {id?}                                -> goal view, or {goal: null}
//! goal.create   {objective, maxRounds?}              -> goal view
//! goal.update   {id, revision, objective?, maxRounds?} -> goal view   (`edit` is the same)
//! goal.pause    {id, revision}                       -> goal view
//! goal.resume   {id, revision}                       -> goal view
//! goal.complete {id, revision}                       -> goal view
//! goal.block    {id, revision, code, message}        -> goal view
//! goal.clear    {id, revision}                       -> {id, revision, clearedAt}
//! ```
//!
//! A goal view is [`crate::goal::Goal`] plus the live `activation` and the two derived booleans a
//! client would otherwise recompute wrongly:
//!
//! ```text
//! {id, revision, objective, phase, blockedReason?, maxRounds, roundsStarted,
//!  createdAt, activation, resumable, mayContinue}
//! ```
//!
//! `revision` is required on every mutation and is the compare-and-set the domain module insists
//! on. It is **not** defaulted to the current revision: a client that did not read the goal first
//! has not seen the revision it is about to overwrite, and accepting its write would be the silent
//! lost update [`GoalError::StaleRevision`] exists to prevent.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value, json};

// Written as `girder_core::` rather than `crate::` on purpose: the test includes this file by
// `#[path]`, which builds it as a module of the test crate. The library declares
// `extern crate self as girder_core`, so one spelling compiles in both places and the tests exercise
// the source as written instead of a copy.
use girder_core::{
    Activation, Declaration, Goal, GoalError, GoalId, GoalRef, GoalState, Phase, RemoteCall,
    RemoteError, RemoteRegistry, check_goal_reference, now_millis,
};

// ---- surface constants ------------------------------------------------------------------------

/// The namespace every method here is served under.
///
/// Plural, because upstream declares `super(ctx, 'goals')` in `packages/goal/goal/src/index.ts`.
/// The singular spelling is what a client written against upstream would not find.
pub const NAMESPACE: &str = "goals";

/// Creates a goal.
pub const CREATE_METHOD: &str = "create";

/// Lists the goals this store holds.
pub const LIST_METHOD: &str = "list";

/// Reads one goal, or the current one when no id is given.
pub const GET_METHOD: &str = "get";

/// Edits an existing goal's objective or round cap.
///
/// Upstream serves this as `edit`. A `update` alias was registered here and is gone: an endpoint
/// upstream does not serve is one a client written against upstream cannot call, so it is not a
/// superset of the API but a second, incompatible one.
pub const EDIT_METHOD: &str = "edit";

/// Stops a goal without completing it.
pub const PAUSE_METHOD: &str = "pause";

/// Rearms a stopped goal.
pub const RESUME_METHOD: &str = "resume";

/// Marks a goal done.
pub const COMPLETE_METHOD: &str = "complete";

/// Discards the goal.
pub const CLEAR_METHOD: &str = "clear";

/// Every method this module registers.
///
/// Exported rather than left implicit so a listing or a test can assert the surface without calling
/// [`register_goal_methods`] and guessing at the names.
pub const METHODS: &[&str] = &[
    CREATE_METHOD,
    LIST_METHOD,
    GET_METHOD,
    EDIT_METHOD,
    PAUSE_METHOD,
    RESUME_METHOD,
    COMPLETE_METHOD,
    CLEAR_METHOD,
];

/// The resource kind these methods touch, as reported to an access policy.
const RESOURCE_KIND: &str = "goal";

/// Whether this registration can deliver or continue anything on its own.
///
/// `false`, and exported so the fact is checkable rather than only documented: nothing here arms a
/// timer, starts a round, or sends a prompt. `mayContinue` in a response describes what
/// [`GoalState::may_continue`] would allow, not something this module does.
pub const STARTS_ROUNDS: bool = false;

// ---- storage seam -----------------------------------------------------------------------------

/// Why a store could not keep or change a goal.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GoalStoreError {
    /// No storage is configured, and the message says what a host must supply.
    #[error("goal storage is unavailable: {0}")]
    Unavailable(String),

    /// The storage exists and failed.
    ///
    /// Separate from [`Self::Unavailable`] because the two need different answers: a missing backend
    /// is an operator's configuration, a failed write is worth retrying. Not constructed by
    /// [`MemoryGoalStore`] — it exists for a host-supplied implementation, and [`store_failure`]
    /// already maps it to the wire so removing it would leave a real store with no way to say
    /// "try again" as opposed to "nothing is configured".
    #[allow(
        dead_code,
        reason = "the variant is part of the trait's contract for host-supplied stores"
    )]
    #[error("goal storage failed: {0}")]
    Backend(String),
}

/// Where goals are kept.
///
/// Every method takes `&self` and the trait is `Send + Sync`, because a handler runs on many
/// requests at once; a store needing `&mut self` would force the registry to serialise every call.
///
/// The operations are deliberately few and coarse. There is no `mutate(&self, closure)` here even
/// though that would make the handlers shorter: a closure-shaped seam cannot be implemented over a
/// network without moving the state machine to the server, and the domain's compare-and-set is the
/// thing worth preserving.
pub trait GoalStore: Send + Sync {
    /// Stores a goal, overwriting any goal with the same id.
    ///
    /// An `Err` means nothing was stored, so a caller must not report success.
    fn put(&self, goal: Goal) -> Result<(), GoalStoreError>;

    /// Every stored goal, in an order the store chooses.
    fn list(&self) -> Vec<Goal>;

    /// One goal, or `None` when no goal holds that id.
    fn get(&self, id: &str) -> Option<Goal>;

    /// Removes a goal, reporting whether it was there.
    ///
    /// The caller distinguishes the two so a clear of an unknown id can be refused instead of
    /// reported as a success.
    fn remove(&self, id: &str) -> Result<bool, GoalStoreError>;

    /// The process-local activation this store's owner reports.
    ///
    /// Separate from the goal because [`crate::goal`] keeps them separate: a goal that was `active`
    /// on disk was active for a process that has since stopped, and that is not a reason for this
    /// one to continue it.
    fn activation(&self) -> Activation;

    /// Records the live activation after a mutation.
    fn set_activation(&self, activation: Activation);
}

/// A store that keeps goals in this process's memory.
///
/// **Lost on restart.** That is not only a storage limitation, it is the domain's restart rule:
/// [`GoalState::restore`] loads a goal *disarmed*, so a fresh process must be told to continue
/// rather than inferring it from what the previous process was doing.
#[derive(Debug, Default)]
pub struct MemoryGoalStore {
    goals: Mutex<BTreeMap<String, Goal>>,
    activation: Mutex<Activation>,
}

impl MemoryGoalStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many goals are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.goals().len()
    }

    /// Whether nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // Poisoning is recovered rather than propagated: a panic while holding this map leaves a table
    // of records, not a half-written one, and refusing every later call would turn one panic into a
    // permanently unusable goal surface.
    fn goals(&self) -> MutexGuard<'_, BTreeMap<String, Goal>> {
        self.goals.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn activation_guard(&self) -> MutexGuard<'_, Activation> {
        self.activation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl GoalStore for MemoryGoalStore {
    fn put(&self, goal: Goal) -> Result<(), GoalStoreError> {
        self.goals().insert(goal.id.as_str().to_owned(), goal);
        Ok(())
    }

    fn list(&self) -> Vec<Goal> {
        self.goals().values().cloned().collect()
    }

    fn get(&self, id: &str) -> Option<Goal> {
        self.goals().get(id).cloned()
    }

    fn remove(&self, id: &str) -> Result<bool, GoalStoreError> {
        Ok(self.goals().remove(id).is_some())
    }

    fn activation(&self) -> Activation {
        *self.activation_guard()
    }

    fn set_activation(&self, activation: Activation) {
        *self.activation_guard() = activation;
    }
}

/// The store a deployment gets when it configures none.
///
/// Every operation refuses and the reason names what is missing. Reporting the gap beats pretending:
/// a client told "created" against a store that cannot keep it would look at a goal that vanishes on
/// the next call, which reads as data loss rather than as a missing configuration.
#[derive(Debug)]
pub struct UnavailableGoalStore {
    reason: String,
}

impl UnavailableGoalStore {
    /// Creates a store that refuses with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The reason the default registration uses.
    ///
    /// It names the call a host makes to fix it, because a message that only said "not supported"
    /// would leave an operator guessing which half to supply.
    #[must_use]
    pub fn default_reason() -> Self {
        Self::new(
            "no GoalStore was supplied; this module ships only an in-process store whose goals \
             vanish on restart, so a host must pass its own Arc<dyn GoalStore> to \
             register_goal_methods",
        )
    }
}

impl GoalStore for UnavailableGoalStore {
    fn put(&self, _goal: Goal) -> Result<(), GoalStoreError> {
        Err(GoalStoreError::Unavailable(self.reason.clone()))
    }

    fn list(&self) -> Vec<Goal> {
        Vec::new()
    }

    fn get(&self, _id: &str) -> Option<Goal> {
        None
    }

    fn remove(&self, _id: &str) -> Result<bool, GoalStoreError> {
        Err(GoalStoreError::Unavailable(self.reason.clone()))
    }

    fn activation(&self) -> Activation {
        Activation::Disarmed
    }

    fn set_activation(&self, _activation: Activation) {}
}

// ---- registration -----------------------------------------------------------------------------

/// Registers the `goal` namespace's methods.
///
/// Every method is declared, so a mounted access policy judges what it is actually asked to do:
/// `create`, `update`/`edit`, `pause`, `resume`, `complete`, and `block` are writes, `clear` is
/// destructive because the stored goal is discarded rather than marked, and `list`/`get` are reads.
/// Declaring everything `write` would have been shorter and would have refused a read-only peer's
/// listing; declaring nothing would let a read-only peer create goals.
///
/// # Why `goals` is an `Arc<dyn GoalStore>` and not a `GoalState`
///
/// [`GoalState`] is a state machine, not a container: it holds one current goal and has no way to
/// name a second. A socket needs to address a goal by id and survive a restart, so the seam is the
/// store and the state machine is rebuilt per call — see [`state_for`]. Passing a `GoalState`
/// directly would also make the registration shared-`mut` across concurrent requests.
pub fn register_goal_methods(registry: &mut RemoteRegistry, goals: &Arc<dyn GoalStore>) {
    for method in [
        CREATE_METHOD,
        EDIT_METHOD,
        PAUSE_METHOD,
        RESUME_METHOD,
        COMPLETE_METHOD,
    ] {
        registry.declare(NAMESPACE, method, Declaration::write(RESOURCE_KIND));
    }
    // A cleared goal is gone; the store keeps no tombstone, so re-creating it is the only way back
    // and the new goal has a new revision starting from one.
    registry.declare(
        NAMESPACE,
        CLEAR_METHOD,
        Declaration::destructive(RESOURCE_KIND),
    );
    registry.declare(NAMESPACE, LIST_METHOD, Declaration::read(RESOURCE_KIND));
    registry.declare(NAMESPACE, GET_METHOD, Declaration::read(RESOURCE_KIND));

    let create_store = Arc::clone(goals);
    registry.register(NAMESPACE, CREATE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&create_store);
        Box::pin(async move { create(&call.args, &store) })
    });

    let list_store = Arc::clone(goals);
    registry.register(NAMESPACE, LIST_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&list_store);
        // A listing reads every goal and can fail at nothing: the store's `list` has no error to
        // report, so there is no failure for this handler to propagate.
        Box::pin(async move { Ok(list(&call.args, &store)) })
    });

    let get_store = Arc::clone(goals);
    registry.register(NAMESPACE, GET_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&get_store);
        Box::pin(async move { get(&call.args, &store) })
    });

    // `update` and `block` are deliberately not registered: upstream serves `edit` and has no
    // "blocked" transition at all. The functions stay because the state machine has the concepts and
    // their own tests cover them, but an endpoint upstream does not serve would be a second API.

    // The same handler under upstream's spelling, not a forward to the first: forwarding would mean
    // a second `RemoteCall` and a second chance for the two paths to differ.
    let edit_store = Arc::clone(goals);
    registry.register(NAMESPACE, EDIT_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&edit_store);
        Box::pin(async move { update(&call.args, &store) })
    });

    let pause_store = Arc::clone(goals);
    registry.register(NAMESPACE, PAUSE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&pause_store);
        Box::pin(async move { pause(&call.args, &store) })
    });

    let resume_store = Arc::clone(goals);
    registry.register(NAMESPACE, RESUME_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&resume_store);
        Box::pin(async move { resume(&call.args, &store) })
    });

    let complete_store = Arc::clone(goals);
    registry.register(NAMESPACE, COMPLETE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&complete_store);
        Box::pin(async move { complete(&call.args, &store) })
    });

    let clear_store = Arc::clone(goals);
    registry.register(NAMESPACE, CLEAR_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&clear_store);
        Box::pin(async move { clear(&call.args, &store) })
    });
}

// ---- handlers ---------------------------------------------------------------------------------

/// `goal.create`: starts a goal and arms it.
///
/// Upstream's `@Remote('create')`. The id is minted here rather than accepted from the caller:
/// an id a client chooses is an id two clients can choose, and the second `create` would either
/// overwrite the first goal or be refused for a reason the caller cannot act on.
///
/// The objective is validated by [`GoalState::create`], which refuses a blank one with
/// [`GoalError::EmptyObjective`]. A second open goal is refused by the same call with
/// [`GoalError::InvalidTransition`], because upstream's rule is that an existing non-complete goal
/// must be cleared or resumed instead.
fn create(args: &Value, store: &Arc<dyn GoalStore>) -> Result<Value, RemoteError> {
    let objective = required_str(args, "objective", CREATE_METHOD)?;
    // Absent means "the default cap". An explicit zero is passed through and `GoalState::create`
    // treats it as the default rather than as a goal that can take no round at all.
    let max_rounds = optional_u32(args, "maxRounds", CREATE_METHOD)?;

    // Seeded with the store's current goal so `GoalState::create` can apply its own rule — a second
    // open goal is refused, a completed one is replaced. Checking here instead would be a second copy
    // of that rule, and the copy would be the one that drifts.
    let mut state = GoalState::new();
    if let Some(existing) = current_goal(store) {
        state.restore(existing);
    }
    let id = GoalId::new(mint_id()).ok_or_else(|| {
        invalid(
            CREATE_METHOD,
            "minted an empty goal id, which names nothing",
        )
    })?;
    let goal = state
        .create(id, objective, max_rounds, now_millis())
        .map_err(|error| goal_error(CREATE_METHOD, &error))?;
    store
        .put(goal.clone())
        .map_err(|error| store_failure(&error))?;
    store.set_activation(state.activation());
    Ok(view(&goal, state.activation()))
}

/// `goal.list`: every goal this store holds.
///
/// Upstream has no list method — a client reads its live agent's current goal with `get`. This one
/// exists because the task's surface requires a listing, and because a store keyed by id has to be
/// enumerable or a goal that is no longer current becomes unreachable. `activation` describes the
/// store's live arming, which applies to at most the current goal.
fn list(_args: &Value, store: &Arc<dyn GoalStore>) -> Value {
    let activation = store.activation();
    let goals = store.list();
    let items: Vec<Value> = goals
        .iter()
        .map(|goal| view(goal, activation_for(goal, activation)))
        .collect();
    let total = items.len();
    json!({
        "items": items,
        "total": total,
        "activation": activation_name(activation),
    })
}

/// `goal.get`: one goal, or the current one when no id is given.
///
/// Upstream's `@Remote('get')`, keyed by live agent. A peer here has no agent to name, so the id is
/// what addresses a goal, and omitting it means "the current one" — the single goal whose phase is
/// still open, which is the closest thing this surface has to upstream's notion.
///
/// A goal that does not exist is **not** an error: it answers `{"goal": null}`, matching upstream's
/// `GoalView | undefined`. Reading a goal that has not been created yet is the normal state at the
/// start of a session, not a caller mistake, and an error would make every client special-case it.
fn get(args: &Value, store: &Arc<dyn GoalStore>) -> Result<Value, RemoteError> {
    let activation = store.activation();
    let id = optional_str(args, "id", GET_METHOD)?;
    let goal = match id {
        Some(id) => store.get(id),
        None => current_goal(store),
    };
    Ok(match goal {
        Some(goal) => view(&goal, activation_for(&goal, activation)),
        None => json!({ "goal": Value::Null }),
    })
}

/// `goal.update`: edits the objective or the round cap.
///
/// Upstream's `@Remote('edit')`, and served under both spellings. At least one field is required:
/// an edit that changes nothing would still bump the revision, so every other client's compare-and-
/// set would fail for a change nobody made — upstream refuses it with `GOAL_INVALID_EDIT` for the
/// same reason.
///
/// The rest is [`GoalState::edit`]: a completed goal is refused with
/// [`GoalError::InvalidTransition`], a blank objective with [`GoalError::EmptyObjective`], and a
/// stale or foreign revision by [`GoalState::check`] before either.
fn update(args: &Value, store: &Arc<dyn GoalStore>) -> Result<Value, RemoteError> {
    let (reference, mut state) = current(args, store, EDIT_METHOD)?;
    let objective = optional_string(args, "objective", EDIT_METHOD)?;
    let max_rounds = optional_u32(args, "maxRounds", EDIT_METHOD)?;
    if objective.is_none() && max_rounds.is_none() {
        return Err(invalid(
            EDIT_METHOD,
            "an edit requires `objective` and/or `maxRounds`; an empty edit would bump the revision \
             for a change nobody made",
        ));
    }
    let goal = state
        .edit(&reference, objective, max_rounds)
        .map_err(|error| goal_error(EDIT_METHOD, &error))?;
    commit(store, state.activation(), &goal)
}

/// `goal.pause`: stops an active goal and disarms it.
///
/// Upstream's `@Remote('pause')`. Any phase but active is refused by [`GoalState::pause`]; a paused
/// goal is resumable, so this is not a discard.
fn pause(args: &Value, store: &Arc<dyn GoalStore>) -> Result<Value, RemoteError> {
    let (reference, mut state) = current(args, store, PAUSE_METHOD)?;
    let goal = state
        .pause(&reference)
        .map_err(|error| goal_error(PAUSE_METHOD, &error))?;
    commit(store, state.activation(), &goal)
}

/// `goal.resume`: re-arms a stopped goal, or arms an active one that is disarmed.
///
/// Upstream's `@Remote('resume')`. This is the human-authorized edge the domain module's
/// documentation is about: a fresh process loads a goal disarmed, and only an explicit resume makes
/// automatic continuation possible again. [`GoalError::AlreadyArmed`] refuses the no-op, so a resume
/// never records a revision for a change that did not happen.
fn resume(args: &Value, store: &Arc<dyn GoalStore>) -> Result<Value, RemoteError> {
    let (reference, mut state) = current(args, store, RESUME_METHOD)?;
    // A `GoalState` rebuilt by `restore` is always disarmed, but the *process's* arming lives in the
    // store — that is the half of `GoalState::resume`'s precondition this call has to apply, because
    // the state machine it delegates to cannot see it. Re-arming an already-armed active goal would
    // record a revision for a change nobody made, which is precisely what
    // [`GoalError::AlreadyArmed`] refuses.
    // The revision is checked *before* the arming rule: a caller working from a stale view is
    // answering the wrong question, and telling it "already armed" would describe a goal as it is
    // now when the caller is still looking at an older one.
    if let Some(goal) = state.current() {
        check_goal_reference(goal, &reference)
            .map_err(|error| goal_error(RESUME_METHOD, &error))?;
    }
    let armed = store.activation() == Activation::Armed;
    if armed
        && state
            .current()
            .is_some_and(|goal| goal.phase == Phase::Active)
    {
        return Err(goal_error(
            RESUME_METHOD,
            &GoalError::AlreadyArmed {
                id: reference.id.to_string(),
            },
        ));
    }
    let goal = state
        .resume(&reference)
        .map_err(|error| goal_error(RESUME_METHOD, &error))?;
    commit(store, state.activation(), &goal)
}

/// `goal.complete`: marks an open goal done.
///
/// Upstream's `@Remote('complete')`. A completed goal is terminal — [`Phase::is_open`] is false and
/// every later mutation is refused — because reopening one would make "complete" mean "probably
/// complete".
fn complete(args: &Value, store: &Arc<dyn GoalStore>) -> Result<Value, RemoteError> {
    let (reference, mut state) = current(args, store, COMPLETE_METHOD)?;
    let goal = state
        .complete(&reference)
        .map_err(|error| goal_error(COMPLETE_METHOD, &error))?;
    commit(store, state.activation(), &goal)
}

/// `goal.clear`: discards the goal.
///
/// Upstream's `@Remote('clear')` answers the tombstone `{id, revision + 1}` and keeps history.
/// This store keeps no history, so the response reports the revision that was discarded and says
/// so with `cleared: true` rather than inventing a tombstone revision nothing can resolve.
///
/// Unlike the other mutations this one is not guarded by a transition: [`GoalState::clear`]
/// discards whatever is there, because "clear" is the user's explicit discard and refusing it for a
/// completed goal would leave a finished goal that cannot be removed. The revision is still
/// checked, so a client working from a stale view cannot discard a goal it has not seen.
fn clear(args: &Value, store: &Arc<dyn GoalStore>) -> Result<Value, RemoteError> {
    let (reference, mut state) = current(args, store, CLEAR_METHOD)?;
    // `GoalState::clear` takes no reference — it discards whatever is there — so the compare-and-set
    // has to happen here. Without it a client working from a stale view discards a goal it has never
    // seen, which is the lost update the revision exists to prevent, only worse: the record is gone.
    if let Some(goal) = state.current() {
        check_goal_reference(goal, &reference).map_err(|error| goal_error(CLEAR_METHOD, &error))?;
    }
    let goal = state.clear().ok_or_else(|| {
        goal_error(
            CLEAR_METHOD,
            &GoalError::NotCurrent {
                id: reference.id.to_string(),
            },
        )
    })?;
    store
        .remove(goal.id.as_str())
        .map_err(|error| store_failure(&error))?;
    store.set_activation(state.activation());
    Ok(json!({
        "id": goal.id.as_str(),
        "revision": goal.revision,
        "cleared": true,
        "clearedAt": now_millis(),
        "activation": activation_name(state.activation()),
    }))
}

// ---- shared plumbing --------------------------------------------------------------------------

/// The store's current goal: the one whose phase is still open.
///
/// This is the closest this surface has to upstream's "the goal for this agent". [`GoalState::create`]
/// refuses a second open goal, so at most one exists.
fn current_goal(store: &Arc<dyn GoalStore>) -> Option<Goal> {
    store.list().into_iter().find(|goal| goal.phase.is_open())
}

/// Loads the goal a mutation names and rebuilds a [`GoalState`] around it.
///
/// The state machine is reconstructed per call rather than shared: [`GoalState`] is a `&mut self`
/// value with one current goal, while the store is what many concurrent requests can share. Loading
/// through [`GoalState::restore`] also keeps the domain's restart rule in one place — the phase
/// survives, the store's live activation is applied separately by [`commit`].
///
/// An id that names no stored goal is [`GoalError::NotCurrent`], the domain's own answer for "this
/// is not the goal that is current". Mapping it rather than minting a new error keeps one vocabulary
/// for a client to branch on.
fn current(
    args: &Value,
    store: &Arc<dyn GoalStore>,
    method: &str,
) -> Result<(GoalRef, GoalState), RemoteError> {
    let id = required_str(args, "id", method)?;
    let revision = required_u64(args, "revision", method)?;
    let goal = store
        .get(id)
        .ok_or_else(|| goal_error(method, &GoalError::NotCurrent { id: id.to_owned() }))?;
    // The reference is assembled before the state machine checks it so a stale revision is refused
    // by `GoalState::check`, with the same error the domain uses, rather than by a comparison here.
    let reference = GoalRef {
        id: goal.id.clone(),
        revision,
    };
    let mut state = GoalState::new();
    state.restore(goal);
    Ok((reference, state))
}

/// Stores a mutated goal and records the live activation the mutation produced.
///
/// The order is deliberate: the goal is written first, so an activation that says "armed" never
/// describes a phase that was not durably recorded.
fn commit(
    store: &Arc<dyn GoalStore>,
    activation: Activation,
    goal: &Goal,
) -> Result<Value, RemoteError> {
    store
        .put(goal.clone())
        .map_err(|error| store_failure(&error))?;
    store.set_activation(activation);
    Ok(view(goal, activation))
}

/// The activation that governs `goal`.
///
/// Only the current goal can be armed: the store's activation is the live process's, and a goal that
/// is not active cannot be continued whatever the flag says. Reporting `armed` on a completed goal
/// would tell a client automatic continuation was possible when [`GoalState::may_continue`] says
/// otherwise.
fn activation_for(goal: &Goal, activation: Activation) -> Activation {
    if goal.phase == Phase::Active {
        activation
    } else {
        Activation::Disarmed
    }
}

/// The wire name of an activation, matching the domain's spelling.
fn activation_name(activation: Activation) -> &'static str {
    match activation {
        Activation::Armed => "armed",
        Activation::Disarmed => "disarmed",
    }
}

/// Projects one goal for the wire.
///
/// The field names follow upstream's `GoalView` (`maxGoalRounds`, `roundsStarted`, `blockedReason`)
/// with two exceptions, both deliberate:
///
/// - `maxRounds` rather than `maxGoalRounds`, matching [`crate::goal::Goal`]'s own field name and
///   this crate's other surfaces.
/// - `blockedReason` is omitted, not `null`, when the goal is not blocked — a client that
///   distinguishes "absent" from "null" would otherwise mis-render.
///
/// The derived `resumable` and `mayContinue` are computed by [`Phase::is_resumable`] and
/// [`GoalState::may_continue`]'s rule rather than by the client: the rules are the domain's, and a
/// second copy in a client would eventually disagree, with the disagreement showing as a button that
/// offers a resume the server refuses.
fn view(goal: &Goal, activation: Activation) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), json!(goal.id.as_str()));
    object.insert("revision".to_owned(), json!(goal.revision));
    object.insert("objective".to_owned(), json!(goal.objective));
    object.insert("phase".to_owned(), json!(goal.phase.as_str()));
    if let Some(reason) = &goal.blocked_reason {
        object.insert(
            "blockedReason".to_owned(),
            json!({ "code": reason.code, "message": reason.message }),
        );
    }
    object.insert("maxRounds".to_owned(), json!(goal.max_rounds));
    object.insert("roundsStarted".to_owned(), json!(goal.rounds_started));
    object.insert("createdAt".to_owned(), json!(goal.created_at));
    object.insert("activation".to_owned(), json!(activation_name(activation)));
    object.insert("resumable".to_owned(), json!(goal.phase.is_resumable()));
    object.insert(
        "mayContinue".to_owned(),
        json!(goal.phase == Phase::Active && activation == Activation::Armed),
    );
    // Upstream's `GoalView` carries `updatedAt`; this store keeps only `createdAt` because a goal's
    // revision is the mutation counter a client actually uses. Reporting `createdAt` twice under a
    // second name would be a lie about what the store knows.
    Value::Object(object)
}

/// Mints an id no other call in this process can mint.
///
/// A timestamp, the process id, and a process-wide counter, following
/// [`crate::schedule::rpc`]'s `mint_id`: the timestamp keeps a log roughly chronological, the counter
/// is what guarantees uniqueness inside the process, and the process id keeps two processes apart.
/// No randomness, because none is needed for an id that is compared, never guessed.
fn mint_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    format!(
        "goal-{:x}-{:x}-{sequence:x}",
        now_millis(),
        std::process::id()
    )
}

/// Reads a required string argument.
fn required_str<'a>(args: &'a Value, field: &str, method: &str) -> Result<&'a str, RemoteError> {
    args.get(field).and_then(Value::as_str).ok_or_else(|| {
        invalid(
            method,
            format!("`{field}` is required and must be a string"),
        )
    })
}

/// Reads an optional string argument, distinguishing absent from present-but-not-a-string.
fn optional_str<'a>(
    args: &'a Value,
    field: &str,
    method: &str,
) -> Result<Option<&'a str>, RemoteError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid(
            method,
            format!("`{field}` must be a string when present"),
        )),
    }
}

/// Reads an optional string argument as an owned value.
fn optional_string(args: &Value, field: &str, method: &str) -> Result<Option<String>, RemoteError> {
    Ok(optional_str(args, field, method)?.map(ToOwned::to_owned))
}

/// Reads a required non-negative integer argument.
fn required_u64(args: &Value, field: &str, method: &str) -> Result<u64, RemoteError> {
    args.get(field).and_then(Value::as_u64).ok_or_else(|| {
        // The revision is what makes the mutation a compare-and-set. A default here would let a
        // caller that never read the goal overwrite it, which is the lost update the revision
        // exists to prevent — so a missing one is refused rather than assumed.
        invalid(
            method,
            format!("`{field}` is required and must be a non-negative integer"),
        )
    })
}

/// Reads an optional integer argument, narrowing it to `u32`.
///
/// A value above [`u32::MAX`] is refused rather than wrapped: a round cap that wrapped would be a
/// cap far below what the caller asked for, and the goal would stop early for a reason nothing in
/// the record explains.
fn optional_u32(args: &Value, field: &str, method: &str) -> Result<Option<u32>, RemoteError> {
    let Some(value) = args.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let number = value.as_u64().ok_or_else(|| {
        invalid(
            method,
            format!("`{field}` must be a non-negative integer when present"),
        )
    })?;
    u32::try_from(number).map(Some).map_err(|_| {
        invalid(
            method,
            format!("`{field}` is {number}, above the maximum of {}", u32::MAX),
        )
    })
}

/// Maps the domain's own error to the wire.
///
/// One function for all of them because the domain already has the vocabulary a client branches on.
/// The mapping is not a flattening to `FAILED`: [`GoalError::StaleRevision`] and
/// [`GoalError::InvalidTransition`] both reach the caller as `INVALID_ARGUMENTS`, with the domain's
/// message intact, because both mean "this call cannot succeed as sent" — the caller changes the
/// revision or the operation, and a retry with the same arguments is not worth making.
fn goal_error(method: &str, error: &GoalError) -> RemoteError {
    invalid(method, error.to_string())
}

/// Maps a store failure to the wire.
///
/// [`GoalStoreError::Unavailable`] becomes `FAILED` rather than `INVALID_ARGUMENTS`: the arguments
/// were fine and the deployment is not, so a client must not tell its user they asked for something
/// wrong. The message names the missing piece, which is the only thing an operator can act on.
fn store_failure(error: &GoalStoreError) -> RemoteError {
    // Every store failure is `FAILED`, and that is the point: the caller did nothing wrong, so
    // `INVALID_ARGUMENTS` would send it looking for a mistake in its request when the deployment is
    // what needs fixing. The variants stay distinct in the type because a host-supplied store needs
    // the distinction — "retry" and "configure storage" are different operator actions — and the
    // message carries which one it was.
    RemoteError::Failed(error.to_string())
}

/// Builds an `INVALID_ARGUMENTS` error for `method`.
fn invalid(method: &str, message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: message.into(),
    }
}
