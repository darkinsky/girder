//! The `schedule.*` Remote methods: storing a prompt to be repeated on a cron-regular basis.
//!
//! [`crate::schedule`] already answers the hard question — *when does this fire next* — with
//! [`CronExpression::parse`], [`Rule::next_after`], and [`next_fire`]. What it deliberately does not
//! answer is *where a task is kept*, because it owns no storage: its [`Scheduler`] is a
//! caller-driven value, not a surface a client can reach. So nothing on the RPC surface could create
//! a schedule, and a client had no way in. This module is that entrance: it validates, stores, lists,
//! and removes tasks, and reports each one's next occurrence by asking the rule that already knows.
//!
//! # Cron: this module parses nothing
//!
//! Validation is [`CronExpression::parse`], not a second implementation. A cron parser is a
//! thousand-line component with its own dialect wars (names, `?`, `L`, `W`, `#`, and whether both
//! day fields must match), and a second one here would eventually disagree with the first about
//! some expression — with the disagreement showing up as a task that fires on the wrong day. What
//! the existing parser accepts is what this module accepts:
//!
//! | field | range | supported syntax |
//! |---|---|---|
//! | minute | 0–59 | `*`, lists, ranges, steps |
//! | hour | 0–23 | same |
//! | day-of-month | 1–31 | same |
//! | month | 1–12 | same |
//! | day-of-week | 0–7 (0 and 7 are Sunday) | same |
//!
//! `MON`, `JAN`, `?`, `L`, `W`, and `#` are refused with a message naming the field. Five fields are
//! required; six (a seconds field) or four are refused. A valid expression that can never fire —
//! `0 0 30 2 *` asks for 30 February — is also refused, because a task nobody will ever see run is a
//! caller mistake best reported at the call rather than discovered a year later.
//!
//! # What this module does **not** do: deliver anything
//!
//! **A stored task here is inert.** This module writes the record and computes the next occurrence;
//! it arms no timer, sleeps on nothing, spawns no worker, and delivers no prompt. Nothing in this
//! file can cause a message to arrive. Actually delivering requires a runtime that owns a clock and
//! a destination session — the loop in [`crate::schedule::run_scheduler`] driven by a host that
//! supplies the dispatch — and that integration is out of this module's scope. The consequence is
//! spelled out on the wire: every mutating response carries `"armed": false`, and this registration
//! exposes no method named `run`. A client that shows a task as "scheduled" is reading a stored
//! record, not a promise of delivery.
//!
//! # The store is a seam, and the default one is honestly limited
//!
//! [`ScheduleStore`] is the only place a task is kept. Two implementations ship:
//!
//! - [`MemoryScheduleStore`] — a process-local map. **It is lost on restart**, and its tasks are
//!   inert as above. It exists so the methods are testable and so a host can accept writes before it
//!   has durable storage.
//! - [`UnavailableScheduleStore`] — refuses every write and reports that storage is missing, which
//!   is what [`register_schedule_methods`] installs when the caller supplies nothing. Reporting the
//!   gap beats pretending: a client told "created" against a store that cannot keep it would show a
//!   task that vanishes on the next call.
//!
//! A host with a database passes its own implementation; no method here assumes the map, and
//! nothing in this module reads a file or a directory.
//!
//! # Why an unknown id is an error, not an empty result
//!
//! Upstream answers `view` and `delete` for an unknown id with a non-mutating miss
//! (`{code: "schedule_not_found"}`). This module reports [`RemoteError::InvalidArguments`] instead.
//! A miss-shaped success is a shape a client forgets to check, and a delete that reported success
//! for an id nobody held would tell the caller their task is gone when it never existed. The
//! deviation is visible: the error message names the id.
//!
//! # Why each method declares its own access
//!
//! The mutating methods declare `write`, `delete` declares `destructive` (a deleted task cannot be
//! restored, only recreated), and the read methods declare `read`. Declaring everything `write`
//! would have been one line shorter and would have broken a read-only deployment twice over: its
//! listing would be refused, and its refusal would carry the wrong reason. A method with no
//! declaration at all is treated as a read of its namespace, which for `create` would let a
//! read-only peer write tasks.
//!
//! # Wire shapes
//!
//! ```text
//! schedule.create   {title, prompt, timing?, cron?, time_zone?}  -> {id, nextFireAt, armed}
//! schedule.list     {active?, limit?, offset?}                   -> {items, shown, total, limit, offset}
//! schedule.view     {id}                                         -> entry
//! schedule.get      {id}                                         -> entry  (alias of view)
//! schedule.delete   {id}                                         -> {id, deleted, armed}
//! schedule.pause    {id}                                         -> {id, status, armed}
//! schedule.resume   {id}                                         -> {id, status, armed}
//! ```
//!
//! `timing` is upstream's tagged union, of which three kinds are implemented:
//! `{"kind":"after","after_seconds":N}`, `{"kind":"every","every_seconds":N}`, and
//! `{"kind":"cron","cron":{"expression":"...","time_zone":"UTC"}}`. The `cron` shorthand at the top
//! level (`{title, cron, prompt}`) means the same as the third kind, in UTC unless `time_zone` says
//! otherwise. `at`, `daily`, and `weekly` are **not** implemented — they name a local calendar time,
//! and this crate has no calendar or IANA zone database (see [`Offset`]); a caller who needs them
//! gets a parse error that lists what is accepted rather than a silently shifted firing time.
//! `rrule` is likewise absent: the syntax is a different, larger specification than the five-field
//! grammar this build can validate.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use girder_core::{
    CronExpression, Declaration, Offset, RemoteCall, RemoteError, RemoteRegistry, Rule, Schedule,
    ScheduleId, create_schedule, next_fire, now_millis,
};

// ---- surface constants ------------------------------------------------------------------------

/// The namespace every method here is served under.
pub const NAMESPACE: &str = "schedule";

/// Creates one task.
pub const CREATE_METHOD: &str = "create";

/// Lists tasks, optionally filtered and paged.
pub const LIST_METHOD: &str = "list";

/// Reads one task.
///
/// Upstream spells this `view`; see [`GET_METHOD`] for why both names are served.
pub const VIEW_METHOD: &str = "view";
/// The second spelling of [`VIEW_METHOD`].
///
/// Two names for one handler rather than an alias table elsewhere: a client written against either
/// spelling reaches the same code, and there is no second implementation to drift.
pub const GET_METHOD: &str = "get";

/// Removes one task.
pub const DELETE_METHOD: &str = "delete";

/// Stops a task from firing without removing it.
pub const PAUSE_METHOD: &str = "pause";

/// Lets a paused task resume from its next occurrence.
pub const RESUME_METHOD: &str = "resume";

/// The resource kind these methods touch, as reported to an access policy.
const RESOURCE_KIND: &str = "schedule";

/// Whether this registration arms anything that can deliver a prompt.
///
/// `false`, and exported so the fact is checkable rather than only documented: a stored task is a
/// record, and delivery needs a runtime this module does not have (see the module documentation).
pub const ARMS_TIMERS: bool = false;

/// The largest page a listing may ask for.
///
/// The same bound upstream applies (1..=500). A larger page is refused rather than clamped, because
/// a client that asked for 10,000 and silently got 500 would believe it had read everything.
pub const MAX_LIST_LIMIT: u32 = 500;

/// The page size a listing gets when it asks for none.
pub const DEFAULT_LIST_LIMIT: u32 = 100;

/// The zone a `cron` shorthand is interpreted in when the request names none.
///
/// UTC rather than the host's local zone: a server-side default that followed the machine's clock
/// would move every task when the machine moved, with nothing in the stored record to show why.
pub const DEFAULT_TIME_ZONE: &str = "UTC";

// ---- storage seam -----------------------------------------------------------------------------

/// Why a store could not keep or change a task.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScheduleStoreError {
    /// No storage is configured, and the message says what a host must supply.
    #[error("schedule storage is unavailable: {0}")]
    Unavailable(String),

    /// The storage exists and failed.
    ///
    /// Separate from [`Self::Unavailable`] because the two need different responses: a missing
    /// backend is an operator's configuration to fix, a failed write is worth retrying.
    ///
    /// Not constructed by either store in this module — it exists for the implementation a host
    /// supplies, and [`store_failure`] already maps every error to the wire, so removing it would
    /// leave a real store with no way to report "try again" as opposed to "nothing is configured".
    #[allow(
        dead_code,
        reason = "the variant is part of the trait's contract for host-supplied stores"
    )]
    #[error("schedule storage failed: {0}")]
    Backend(String),
}

/// Where tasks are kept.
///
/// Every method is `&self` and the trait is `Send + Sync`, because a handler is called from many
/// requests at once: a store that needed `&mut self` would force the registry to serialise every
/// schedule call. Implementations provide their own interior mutability.
///
/// The trait is minimal on purpose. It has no "tick", no "due", and no delivery hook: those belong
/// to whoever owns a clock, and putting them here would suggest this module delivers something it
/// does not (see the module documentation).
pub trait ScheduleStore: Send + Sync {
    /// Stores a task.
    ///
    /// An `Err` means nothing was stored, so a caller must not report success.
    fn insert(&self, schedule: Schedule) -> Result<(), ScheduleStoreError>;

    /// Every stored task, in an order the store chooses.
    ///
    /// The order is not a contract: a listing sorts by status and next occurrence itself, because a
    /// store that is not a sorted map cannot promise that order cheaply.
    fn list(&self) -> Vec<Schedule>;

    /// One task, or `None` when no task holds that id.
    fn get(&self, id: &str) -> Option<Schedule>;

    /// Removes a task.
    ///
    /// `Ok(true)` when it was there and is now gone, `Ok(false)` when no task held the id. The
    /// caller distinguishes the two so a delete of an unknown id can be refused rather than
    /// reported as a success.
    fn remove(&self, id: &str) -> Result<bool, ScheduleStoreError>;

    /// Sets the active flag.
    ///
    /// `Ok(None)` when no task holds the id, otherwise the task as stored. Returning the stored task
    /// rather than echoing the request keeps the response honest about what a concurrently-edited
    /// record now says.
    fn set_active(&self, id: &str, active: bool) -> Result<Option<Schedule>, ScheduleStoreError>;
}

/// A store that keeps tasks in this process's memory.
///
/// **Lost on restart, and inert.** Two separate limitations: the map lives in the process, so every
/// task disappears when the process exits; and even while the process lives, nothing in this module
/// delivers the stored prompt, because there is no timer here. It is the store for tests and for a
/// host that wants the surface to work before it has durable storage — not a scheduler.
#[derive(Debug, Default)]
pub struct MemoryScheduleStore {
    tasks: Mutex<BTreeMap<String, Schedule>>,
}

impl MemoryScheduleStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many tasks are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // Poisoning is recovered rather than propagated: a panic while holding this map leaves a table
    // of records, not a half-written one, and refusing every later call would turn one panic into a
    // permanently unusable schedule surface.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Schedule>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ScheduleStore for MemoryScheduleStore {
    fn insert(&self, schedule: Schedule) -> Result<(), ScheduleStoreError> {
        // Insert, not insert-if-absent: the id is minted immediately before this call and the
        // handler has already checked that nobody held it, so an overwrite here could only happen
        // for an id collision across processes -- and overwriting reuses the slot rather than
        // leaving a second task nobody can list.
        self.lock()
            .insert(schedule.id.as_str().to_owned(), schedule);
        Ok(())
    }

    fn list(&self) -> Vec<Schedule> {
        self.lock().values().cloned().collect()
    }

    fn get(&self, id: &str) -> Option<Schedule> {
        self.lock().get(id).cloned()
    }

    fn remove(&self, id: &str) -> Result<bool, ScheduleStoreError> {
        Ok(self.lock().remove(id).is_some())
    }

    fn set_active(&self, id: &str, active: bool) -> Result<Option<Schedule>, ScheduleStoreError> {
        let mut tasks = self.lock();
        let Some(schedule) = tasks.get_mut(id) else {
            return Ok(None);
        };
        schedule.active = active;
        Ok(Some(schedule.clone()))
    }
}

/// The store a deployment gets when it configures none.
///
/// Every write is refused and the reason names what is missing, following
/// [`crate::session_lifecycle_rpc::UnavailableArchiveStore`]: the alternative — silently keeping the
/// record in memory — would report a state the next call cannot confirm.
#[derive(Debug)]
pub struct UnavailableScheduleStore {
    reason: String,
}

impl UnavailableScheduleStore {
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
            "no ScheduleStore was supplied; this module ships only an in-process store whose tasks \
             vanish on restart, so a host must pass its own Arc<dyn ScheduleStore> to \
             register_schedule_methods",
        )
    }
}

impl ScheduleStore for UnavailableScheduleStore {
    fn insert(&self, _schedule: Schedule) -> Result<(), ScheduleStoreError> {
        Err(ScheduleStoreError::Unavailable(self.reason.clone()))
    }

    fn list(&self) -> Vec<Schedule> {
        // Empty rather than a refusal, because the signature has no error channel. Consistent with
        // the writes: this store holds nothing, so nothing is what it has.
        Vec::new()
    }

    fn get(&self, _id: &str) -> Option<Schedule> {
        None
    }

    fn remove(&self, _id: &str) -> Result<bool, ScheduleStoreError> {
        // `Ok(false)` would say "there was nothing to remove", which is true and also useless: the
        // caller learns that this deployment has no storage at all from the refusal.
        Err(ScheduleStoreError::Unavailable(self.reason.clone()))
    }

    fn set_active(&self, _id: &str, _active: bool) -> Result<Option<Schedule>, ScheduleStoreError> {
        Err(ScheduleStoreError::Unavailable(self.reason.clone()))
    }
}

// ---- registration -----------------------------------------------------------------------------

/// Registers the `schedule.*` methods against `store`.
///
/// Declarations are registered beside each handler so the policy judges a call before it runs; the
/// store is cloned into each closure so the host keeps ownership of it and the registry stays the
/// only thing this function mutates.
pub fn register_schedule_methods(registry: &mut RemoteRegistry, store: &Arc<dyn ScheduleStore>) {
    registry.declare(NAMESPACE, CREATE_METHOD, write_declaration());
    registry.declare(NAMESPACE, PAUSE_METHOD, write_declaration());
    registry.declare(NAMESPACE, RESUME_METHOD, write_declaration());
    // A deleted task is gone; the store keeps no tombstone, so re-creating it is the only way back
    // and the new task has a new id. That is `destructive` by the definition in `access`.
    registry.declare(
        NAMESPACE,
        DELETE_METHOD,
        Declaration::destructive(RESOURCE_KIND),
    );
    registry.declare(NAMESPACE, LIST_METHOD, Declaration::read(RESOURCE_KIND));
    registry.declare(NAMESPACE, VIEW_METHOD, Declaration::read(RESOURCE_KIND));
    registry.declare(NAMESPACE, GET_METHOD, Declaration::read(RESOURCE_KIND));

    let create_store = Arc::clone(store);
    registry.register(NAMESPACE, CREATE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&create_store);
        Box::pin(async move { create(&call.args, &store) })
    });

    let list_store = Arc::clone(store);
    registry.register(NAMESPACE, LIST_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&list_store);
        Box::pin(async move { list(&call.args, &store) })
    });

    let view_store = Arc::clone(store);
    registry.register(NAMESPACE, VIEW_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&view_store);
        Box::pin(async move { view(&call.args, &store) })
    });

    // The same handler under the second spelling, not a forward to the first: forwarding would mean
    // a second `RemoteCall` echo and a second chance for the two paths to differ.
    let get_store = Arc::clone(store);
    registry.register(NAMESPACE, GET_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&get_store);
        Box::pin(async move { view(&call.args, &store) })
    });

    let delete_store = Arc::clone(store);
    registry.register(NAMESPACE, DELETE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&delete_store);
        Box::pin(async move { delete(&call.args, &store) })
    });

    let pause_store = Arc::clone(store);
    registry.register(NAMESPACE, PAUSE_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&pause_store);
        Box::pin(async move { set_active(&call.args, &store, false) })
    });

    let resume_store = Arc::clone(store);
    registry.register(NAMESPACE, RESUME_METHOD, move |call: RemoteCall| {
        let store = Arc::clone(&resume_store);
        Box::pin(async move { set_active(&call.args, &store, true) })
    });
}

/// The declaration every mutating method except `delete` reports.
///
/// One function rather than three call sites, so `create`, `pause`, and `resume` cannot end up
/// declaring different accesses for what is the same kind of change.
fn write_declaration() -> Declaration {
    Declaration::write(RESOURCE_KIND)
}

// ---- requests ---------------------------------------------------------------------------------

/// The body of `schedule.create`.
///
/// `title` and `prompt` are not `Option`s: a missing one is reported by the parser with the field's
/// name, which is the message a client needs, and a blank one is reported by [`create_schedule`]
/// with the same wording every other creation path uses.
#[derive(Debug, Deserialize)]
struct CreateRequest {
    /// The task's name, trimmed and non-empty.
    title: String,
    /// The instruction delivered at each occurrence.
    prompt: String,
    /// The timing, in upstream's tagged form. Required unless `cron` is given.
    #[serde(default)]
    timing: Option<TimingRequest>,
    /// A five-field cron expression, as shorthand for `timing.kind == "cron"`.
    #[serde(default)]
    cron: Option<String>,
    /// The zone for the `cron` shorthand. Unused when `timing` is given.
    #[serde(default = "default_time_zone")]
    time_zone: String,
}

/// The timing kinds this build understands.
///
/// Spelled `snake_case` with the same field names as upstream's `ScheduleTiming`, so a client built
/// against the TypeScript types sends a body this parses unchanged for the kinds that are supported.
/// A kind that is not here is a parse error whose message lists the ones that are — which is how
/// `at`, `daily`, `weekly`, and `rrule` report their absence (the module documentation says why).
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TimingRequest {
    /// One shot, this many seconds from now.
    After {
        /// Seconds from the moment of the call.
        after_seconds: u64,
    },
    /// Fixed rate, no shorter than one minute.
    Every {
        /// Seconds between occurrences.
        every_seconds: u64,
    },
    /// A five-field cron expression.
    Cron {
        /// The expression and its zone.
        cron: CronRequest,
    },
}

/// The `cron` member of [`TimingRequest::Cron`], matching upstream's `CronInput`.
#[derive(Debug, Deserialize)]
struct CronRequest {
    /// Five fields: minute hour day-of-month month day-of-week.
    expression: String,
    /// `UTC` or a fixed offset. An IANA name is refused — see [`parse_time_zone`].
    #[serde(default = "default_time_zone")]
    time_zone: String,
}

/// A body carrying only an id.
#[derive(Debug, Deserialize)]
struct IdRequest {
    /// The task's id.
    #[serde(default)]
    id: Option<String>,
}

/// The filter and page of `schedule.list`.
#[derive(Debug, Deserialize)]
struct ListRequest {
    /// `Some(true)` for active tasks only, `Some(false)` for paused ones only, `None` for all.
    #[serde(default)]
    active: Option<bool>,
    /// Page size. Refused rather than clamped when out of range.
    #[serde(default)]
    limit: Option<u32>,
    /// Tasks to skip after sorting.
    #[serde(default)]
    offset: Option<u32>,
}

/// Returns [`DEFAULT_TIME_ZONE`] for serde's `default`.
fn default_time_zone() -> String {
    DEFAULT_TIME_ZONE.to_owned()
}

// ---- handlers ---------------------------------------------------------------------------------
//
// The handlers below are plain functions, not `async fn`s: every store call is synchronous, so there
// is nothing to await, and clippy's `unused_async` is right that the keyword would be noise. The
// futures lie one layer up, in the registration closures, which is the only place the `RemoteMethod`
// type requires one.

/// Runs one `schedule.create`.
fn create(args: &Value, store: &Arc<dyn ScheduleStore>) -> Result<Value, RemoteError> {
    let request: CreateRequest = parse_request(args, CREATE_METHOD)?;
    let now = now_millis();
    let rule = build_rule(&request, now)?;

    // The first occurrence is computed before anything is stored. A rule that never fires would
    // otherwise be accepted, listed, and shown as a task that is waiting for a time that does not
    // exist -- so the impossible cron (`0 0 30 2 *`) and the one-shot in the past are refused here,
    // where the caller can still fix the request.
    let first = rule.next_after(now).map_err(|error| {
        invalid(
            CREATE_METHOD,
            format!("this timing has no future occurrence: {error}"),
        )
    })?;

    let id = mint_id().ok_or_else(|| {
        RemoteError::Failed("cannot mint a non-empty id for the new task".to_owned())
    })?;

    // `create_schedule` owns title and prompt validation, so a blank title is refused with the same
    // message every other creation path produces rather than with a second, drifting copy of the
    // rule.
    let schedule = create_schedule(id, &request.title, &request.prompt, rule)
        .map_err(|error| invalid(CREATE_METHOD, error.to_string()))?;

    let id = schedule.id.as_str().to_owned();
    store
        .insert(schedule)
        .map_err(|error| store_failure(CREATE_METHOD, &error))?;

    Ok(json!({
        "id": id,
        // The instant the rule would next fire. Reported because it is the one thing a caller cannot
        // compute without duplicating the cron logic -- not because anything is armed.
        "nextFireAt": first,
        // Always false: stored, not scheduled. See the module documentation and `ARMS_TIMERS`.
        "armed": ARMS_TIMERS,
    }))
}

/// Runs one `schedule.list`.
fn list(args: &Value, store: &Arc<dyn ScheduleStore>) -> Result<Value, RemoteError> {
    let request: ListRequest = parse_request(args, LIST_METHOD)?;
    let limit = request.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(invalid(
            LIST_METHOD,
            format!("`limit` must be between 1 and {MAX_LIST_LIMIT}, got {limit}"),
        ));
    }
    let offset = request.offset.unwrap_or(0);

    let now = now_millis();
    let mut tasks: Vec<Schedule> = store
        .list()
        .into_iter()
        .filter(|schedule| request.active.is_none_or(|want| schedule.active == want))
        .collect();

    // Active first, then by next occurrence, then by id. Sorted here rather than by the store: the
    // trait promises no order, and a page that reordered between calls would make `offset` return
    // rows the previous page already showed.
    tasks.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| sort_key(a, now).cmp(&sort_key(b, now)))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });

    let total = u32::try_from(tasks.len()).unwrap_or(u32::MAX);
    let items: Vec<Value> = tasks
        .iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(|schedule| entry(schedule, now))
        .collect();

    Ok(json!({
        "items": items,
        "shown": items.len(),
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
}

/// Runs one `schedule.view` (and its `get` spelling).
fn view(args: &Value, store: &Arc<dyn ScheduleStore>) -> Result<Value, RemoteError> {
    let request: IdRequest = parse_request(args, VIEW_METHOD)?;
    let id = require_id(&request, VIEW_METHOD)?;

    // An unknown id is an error rather than a miss-shaped success: see the module documentation.
    let Some(schedule) = store.get(&id) else {
        return Err(no_schedule(VIEW_METHOD, &id));
    };
    Ok(entry(&schedule, now_millis()))
}

/// Runs one `schedule.delete`.
fn delete(args: &Value, store: &Arc<dyn ScheduleStore>) -> Result<Value, RemoteError> {
    let request: IdRequest = parse_request(args, DELETE_METHOD)?;
    let id = require_id(&request, DELETE_METHOD)?;

    // Existence is checked before the removal so a delete of an id nobody holds is refused. A store
    // that reports `false` here and a store that reports `true` for an id that never existed are
    // indistinguishable from the response, so the check is the only place that can be honest.
    if store.get(&id).is_none() {
        return Err(no_schedule(DELETE_METHOD, &id));
    }
    let removed = store
        .remove(&id)
        .map_err(|error| store_failure(DELETE_METHOD, &error))?;
    if !removed {
        // The task was read a moment ago and is gone now: another caller deleted it first. Reported
        // rather than swallowed, because the two requests meant different things.
        return Err(no_schedule(DELETE_METHOD, &id));
    }

    Ok(json!({
        "id": id,
        "deleted": true,
        "armed": ARMS_TIMERS,
    }))
}

/// Runs one `schedule.pause` or `schedule.resume`.
///
/// One function for both because the only difference is the flag; two would be two copies of the
/// existence check and the response shape, and the day one was fixed the other would not be.
fn set_active(
    args: &Value,
    store: &Arc<dyn ScheduleStore>,
    active: bool,
) -> Result<Value, RemoteError> {
    let method = if active { RESUME_METHOD } else { PAUSE_METHOD };
    let request: IdRequest = parse_request(args, method)?;
    let id = require_id(&request, method)?;

    let stored = store
        .set_active(&id, active)
        .map_err(|error| store_failure(method, &error))?;
    let Some(schedule) = stored else {
        return Err(no_schedule(method, &id));
    };

    Ok(json!({
        "id": schedule.id.as_str(),
        "status": status_of(&schedule),
        "armed": ARMS_TIMERS,
    }))
}

// ---- rule construction ------------------------------------------------------------------------

/// Builds the rule for one creation request.
fn build_rule(request: &CreateRequest, now: u64) -> Result<Rule, RemoteError> {
    // Both spellings at once is ambiguous: the top-level `cron` and `timing.cron` could disagree,
    // and picking one would silently ignore what the caller sent.
    let timing = match (&request.timing, &request.cron) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                CREATE_METHOD,
                "supply either `timing` or the `cron` shorthand, not both",
            ));
        }
        (Some(timing), None) => timing,
        (None, Some(expression)) => {
            let expression = expression.clone();
            let time_zone = request.time_zone.clone();
            return cron_rule(&expression, &time_zone);
        }
        (None, None) => {
            return Err(invalid(
                CREATE_METHOD,
                "`timing` is required: one of `after`, `every`, or `cron`",
            ));
        }
    };

    match timing {
        TimingRequest::After { after_seconds } => {
            // The creation instant is the anchor, so the delay is measured from this call rather than
            // from whenever the rule is next evaluated.
            Ok(Rule::after(Duration::from_secs(*after_seconds), now))
        }
        TimingRequest::Every { every_seconds } => {
            Rule::every(Duration::from_secs(*every_seconds), now)
                .map_err(|error| invalid(CREATE_METHOD, error.to_string()))
        }
        TimingRequest::Cron { cron } => cron_rule(&cron.expression, &cron.time_zone),
    }
}

/// Parses and validates a cron expression, refusing one this build cannot honour.
///
/// The parse is [`CronExpression::parse`] — the only cron parser in the crate. Everything below it
/// is zone handling, which is not a grammar question.
fn cron_rule(expression: &str, time_zone: &str) -> Result<Rule, RemoteError> {
    let offset = parse_time_zone(time_zone).map_err(|reason| invalid(CREATE_METHOD, reason))?;
    // Validated here, at the boundary, so an unusable expression is refused with the field that is
    // wrong ("cron field `minute` is invalid: ...") rather than accepted and discovered when it is
    // first evaluated.
    let parsed = CronExpression::parse(expression)
        .map_err(|error| invalid(CREATE_METHOD, format!("`cron` is invalid: {error}")))?;
    Ok(Rule::cron(parsed, offset))
}

/// Parses `UTC`, `Z`, or a fixed `±HH:MM` offset.
///
/// No IANA names. [`Offset`] is a fixed offset by design (`schedule`'s documentation explains that a
/// named zone needs a database that changes when a government does), so `Asia/Shanghai` cannot be
/// resolved here and is refused instead of guessed. Guessing would be the worse failure: a task
/// stored with the wrong offset fires at a time nobody chose, and the record would not show it.
fn parse_time_zone(text: &str) -> Result<Offset, String> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("utc") || trimmed == "Z" {
        return Ok(Offset::UTC);
    }

    let (sign, digits) = match trimmed.split_at_checked(1) {
        Some(("+", rest)) => (1_i64, rest),
        Some(("-", rest)) => (-1_i64, rest),
        _ => {
            return Err(format!(
                "`{text}` is not a usable time zone here: pass `UTC` or a fixed offset such as \
                 `+08:00`; this build has no IANA zone database, so a name like `Asia/Shanghai` \
                 cannot be resolved"
            ));
        }
    };

    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) => (hours, Some(minutes)),
        // `+0800` and `+08` are both accepted because both appear in the wild and neither is
        // ambiguous; a string that is neither shape falls through to the error below.
        None if digits.len() == 4 => (digits.split_at(2).0, Some(digits.split_at(2).1)),
        None => (digits, None),
    };

    let hours: i64 = hours
        .parse()
        .map_err(|_| format!("`{text}` has a non-numeric hour offset"))?;
    let minutes: i64 = match minutes {
        Some(minutes) => minutes
            .parse()
            .map_err(|_| format!("`{text}` has a non-numeric minute offset"))?,
        None => 0,
    };
    // Bounded to a real offset. Every zone in use is within -12:00..=+14:00, and an offset outside
    // that is a typo that would otherwise produce a firing time hours away from what was intended.
    if hours > 14 || minutes > 59 {
        return Err(format!(
            "`{text}` is not a valid offset: hours must be at most 14 and minutes at most 59"
        ));
    }

    Ok(Offset {
        millis: sign * (hours * 3_600_000 + minutes * 60_000),
    })
}

// ---- response pieces --------------------------------------------------------------------------

/// One task as a listing or a read reports it.
fn entry(schedule: &Schedule, now: u64) -> Value {
    json!({
        "id": schedule.id.as_str(),
        "title": schedule.title,
        "prompt": schedule.prompt,
        // The stored spelling of a cron rule, which is `None` for the other rule kinds. Returned as
        // written so a client edits the expression the user typed rather than a normalised form.
        "cron": cron_source(schedule),
        "status": status_of(schedule),
        // `null` when the rule has no next occurrence, which is what a paused or exhausted task
        // looks like. Present rather than omitted so a client can tell "never" from "not sent".
        "nextFireAt": next_fire(schedule, now),
        // Stored, never armed. Repeated on every entry so no single view of a task can imply a
        // timer that does not exist.
        "armed": ARMS_TIMERS,
    })
}

/// The cron expression a task was stored with, if its rule is a cron rule.
fn cron_source(schedule: &Schedule) -> Option<&str> {
    match &schedule.rule {
        Rule::Cron { expression, .. } => Some(expression.source.as_str()),
        _ => None,
    }
}

/// The status word for a task, spelled as upstream spells the union member.
fn status_of(schedule: &Schedule) -> &'static str {
    if schedule.active { "active" } else { "paused" }
}

/// The sort key for a listing: next occurrence, with "never" last.
///
/// `Option`'s ordering puts `None` first, which would float every paused task above the tasks that
/// are actually going to run; mapping to `u64::MAX` puts them where a reader expects an absent date.
fn sort_key(schedule: &Schedule, now: u64) -> u64 {
    next_fire(schedule, now).unwrap_or(u64::MAX)
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

/// Extracts `id`, refusing a missing or blank one.
///
/// Blank is refused rather than looked up: an empty id is a client bug, and reporting it as "no such
/// task" would send the caller looking for a task instead of a field.
fn require_id(request: &IdRequest, method: &str) -> Result<String, RemoteError> {
    match request.id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(invalid(method, "`id` is required")),
    }
}

/// Mints an id no other call in this process can mint.
///
/// A timestamp, the process id, and a process-wide counter: the timestamp keeps a log roughly
/// chronological, the counter is what actually guarantees uniqueness inside the process (two calls
/// in one millisecond), and the process id keeps two processes sharing a directory apart. No
/// randomness, because none is needed for an id that is compared, never guessed.
fn mint_id() -> Option<ScheduleId> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    // `ScheduleId::new` refuses an empty string; this format never produces one, and returning the
    // `Option` rather than asserting keeps a rejected id a reported failure instead of a panic in a
    // request path.
    ScheduleId::new(format!(
        "sched-{:x}-{:x}-{sequence:x}",
        now_millis(),
        std::process::id()
    ))
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
fn no_schedule(method: &str, id: &str) -> RemoteError {
    invalid(method, format!("no schedule `{id}`"))
}

/// Turns a store failure into an RPC failure that says what the store said.
///
/// `Failed` rather than `InvalidArguments`: the caller's request was fine and the deployment is what
/// is missing, which is the distinction a client needs to stop retrying for the first case and keep
/// trying for the second.
fn store_failure(method: &str, error: &ScheduleStoreError) -> RemoteError {
    RemoteError::Failed(format!("`{NAMESPACE}.{method}`: {error}"))
}
