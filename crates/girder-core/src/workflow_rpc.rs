//! The `workflow.*` Remote methods: observing and steering orchestration runs from a client.
//!
//! [`crate::workflow`] already answers everything about a run's *state*: [`WorkflowRun`] holds its
//! status, its phases, its agents and its result, and [`WorkflowRegistry`] owns the table they live
//! in. What it does not answer is *how a client reaches that table*, because it is a library value
//! rather than a surface. `workflow_tool` gives a model two tools over the same table; a browser or
//! an operator console has no tool registry, and before this module it had no namespace either.
//! This module is that entrance.
//!
//! # What upstream actually has, and why this is not a port
//!
//! Upstream `packages/workflow` defines **no `@Remote` decorator at all** — a search of the package
//! for `@Remote('…')` finds nothing. Its surface is three separate things:
//!
//! 1. a Cordis **service**, `workflowEngine.start(request)`, returning a live [`WorkflowRun`] handle
//!    the *holder* owns (`cancel()`, `dispose()`, and a `result` promise that never rejects);
//! 2. a set of **lifecycle events** — `workflow/start`, `workflow/phase`, `workflow/log`,
//!    `workflow/agent-start`, `workflow/agent-end`, `workflow/end` — which are emission-only, so an
//!    observer never gains control;
//! 3. a **model-facing tool** (`tool-workflow`) that starts a run from inside an agent turn.
//!
//! So there is no upstream wire contract to copy field for field, and inventing one that claimed
//! to be a port would be the worse choice: a client author would look for upstream's guarantee —
//! that a live run is holder-owned and cancellable out of band — and find a request/response
//! surface wearing its name. What this module does instead is take the operations the local domain
//! can actually honour, name them the way every other namespace in this crate is named, and state
//! the difference.
//!
//! | this module | upstream counterpart | what it is |
//! |---|---|---|
//! | `workflow.list` | — (the tool's `workflow_status` listing) | every run the registry holds |
//! | `workflow.get` | — (the tool's `workflow_status` by id) | one run, with phases and agents |
//! | `workflow.start` | `workflowEngine.start` | mints an id, starts a run, returns at once |
//! | `workflow.cancel` | `WorkflowRun.cancel()` | stops a run and records the status |
//!
//! The lifecycle events have no counterpart here on purpose: [`crate::remote`] dispatches one value
//! per call and has no stream mode, and `job.follow` already made the same decision — a
//! stream-shaped method answered as request/response is a different contract under the same name.
//! A client polls `workflow.get` instead, which always answers with the run's current state.
//!
//! # Why `start` returns before the script does
//!
//! [`WorkflowRegistry::start`] awaits the engine to completion and returns the finished run. That is the
//! right shape for a tool call — the model wants the value the script produced — but the wrong one
//! for a client, for two reasons. A run can be long, so a call that returned only at the end would
//! leave the caller with no id, nothing to poll, and a connection that looks hung; and a caller that
//! never learns the id cannot cancel what it started. So `start` mints the id, hands the domain's
//! future to a task, and returns the id immediately. Everything a caller needs to follow the run is
//! reachable through `workflow.get`.
//!
//! The first `yield_now` after the spawn is not decoration: [`WorkflowRegistry::start`] records the run
//! `Running` on its first poll, before it awaits the engine, so one yield is enough for the record
//! to exist and for `start`'s response to describe the run rather than guess at it. If the task has
//! not been polled yet the response says `pending`, which is true: accepted, not yet begun.
//!
//! # Why cancellation is real here and a no-op in the tool
//!
//! `workflow_tool` documents that its cancellation is best-effort, because it can only drop the
//! future it is awaiting. This module can do better, and does: it keeps the spawned task's
//! [`JoinHandle`] keyed by run id, so `cancel` aborts that task. Aborting drops the domain's future
//! at its next await point, which means the engine is never polled again and — importantly — the
//! final `Completed` write that [`WorkflowRegistry::start`] performs after the engine returns never happens.
//! Without the abort, a cancelled run would be silently overwritten back to `completed` when its
//! script finished, which is the worst possible answer to a cancellation request.
//!
//! What abort still cannot do is stop work an engine placed on tasks of its own. That is the same
//! limitation the tool reports, and the response says so in `interrupted` rather than implying the
//! script was stopped.
//!
//! # Why a run cannot be restarted, and why an id may be supplied
//!
//! The domain's [`WorkflowStatus`] has terminal states, and a finished run's phases, agents and result are
//! the record of what happened. Starting again *under the same id* would overwrite that record with
//! a run that did different work — the evidence would be gone and nothing would say so. So `start`
//! accepts an optional `id` (a caller that mints its own, or retries with the id it lost the reply
//! for) and refuses one the registry already holds, in either direction: `completed`/`cancelled`/
//! `failed` because a finished run is history, and `pending`/`running` because the id is in use.
//! A caller that wants another run mints another id, which is what omitting `id` does.
//!
//! # Wire shapes
//!
//! ```text
//! workflow.list    {status?, activeOnly?, limit?, offset?}  -> {items, shown, total, limit, offset}
//! workflow.get     {id}                                     -> run
//! workflow.start   {script, title, arguments?, id?}         -> run (or {id, title, status: "pending"})
//! workflow.cancel  {id}                                     -> {id, status, interrupted}
//! ```
//!
//! A `run` is `{id, title, status, terminal, reason, startedAt, currentPhase, phases, agents,
//! finishedAgents, agentCount, result}`. `status` is one of `pending`, `running`, `completed`,
//! `cancelled`, `failed` — the domain's own [`WorkflowStatus`], so a client branches on the same
//! names the Rust code does. `reason` carries the failure text and is `null` otherwise; it is
//! present rather than omitted so a typed client is not handed a field that appears and disappears.
//! A `phase` is `{name, startedAt, endedAt, running}` and an `agent` is `{label, prompt, output,
//! result, finishedAt, done}`.
//!
//! # Arguments are strings, and that is a real limit
//!
//! Upstream passes `args` as arbitrary JSON. The local [`WorkflowStartRequest`] carries
//! `BTreeMap<String, String>`, and widening it would mean changing [`crate::workflow`], which this
//! module does not do. A non-string argument is therefore refused by the parser with a message
//! naming the field, rather than being stringified behind the caller's back: a script that received
//! `"3"` where it expected `3` would make a type error the caller cannot see.
//!
//! # Error mapping
//!
//! - an unknown id, a blank field, a malformed argument, a refused restart and a refusal to cancel
//!   a finished run all become `INVALID_ARGUMENTS` with the reason in the message — each means
//!   "this call cannot succeed as sent", and a retry with the same arguments is not worth making;
//! - a run that failed reports `status: "failed"` in its own projection rather than an error, since
//!   the run exists and its record is the answer; `start` only reports `FAILED` when it cannot mint
//!   an id at all;
//! - the domain's [`WorkflowError`] is surfaced as the run's `reason`, not as a transport
//!   failure, because the caller asked to observe a run and the run is what it should see.
//!
//! The paths below are spelled `girder_core::…` because the integration test includes this file
//! directly; the crate aliases itself by that name, so one spelling works inside and outside it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};

use girder_core::{
    Declaration, RemoteCall, RemoteError, RemoteRegistry, RunId, WorkflowError, WorkflowRegistry,
    WorkflowRun, WorkflowStartRequest, WorkflowStatus,
};

// ---- surface constants ------------------------------------------------------------------------

/// The namespace every method here is served under.
pub const NAMESPACE: &str = "workflow";

/// Lists runs, newest first within the registry's own id order.
pub const LIST_METHOD: &str = "list";

/// Reads one run.
pub const GET_METHOD: &str = "get";

/// Starts a run.
pub const START_METHOD: &str = "start";

/// Cancels a run.
pub const CANCEL_METHOD: &str = "cancel";

/// The resource kind the declarations name.
///
/// One kind for the whole namespace rather than one per run: a policy that had to name a run id
/// would have to be rewritten for every run, and a policy that allowed "some runs" is not a thing
/// an operator can write down.
const RESOURCE_KIND: &str = "workflow";

/// The largest page a listing will return.
///
/// Refused rather than clamped when exceeded, for the reason `schedule` gives: a client that asked
/// for 10,000 rows and silently received 500 would believe it had read everything.
pub const MAX_LIST_LIMIT: u32 = 500;

/// The page size a listing gets when it asks for none.
pub const DEFAULT_LIST_LIMIT: u32 = 100;

/// Every status name a listing may filter by.
const STATUS_NAMES: [&str; 5] = ["pending", "running", "completed", "cancelled", "failed"];

/// Makes two run ids allocated in the same millisecond distinct.
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// The spawned tasks this deployment may still abort, keyed by run id.
///
/// A [`tokio::task::JoinHandle`] rather than an abort handle of our own because aborting is the
/// whole operation: dropping the domain's future is what stops an awaiting engine, and keeping the
/// handle is the only way to reach it after `start` has returned.
type Running = Arc<Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>>;

// ---- registration -----------------------------------------------------------------------------

/// Registers the `workflow.*` methods over `workflows`.
///
/// Declarations are registered beside each handler so the policy judges a call before it runs. The
/// registry is captured rather than accepted per call: a caller must not choose which table its run
/// is recorded in, or a `get` would look at a table the run was never written to.
///
/// `start` and `cancel` declare `write`; neither is `destructive`, because a cancelled or finished
/// run remains in the table and can still be read — nothing is thrown away.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the Arc is cloned into a method closure, so ownership is the point"
)]
pub fn register_workflow_methods(registry: &mut RemoteRegistry, workflows: Arc<WorkflowRegistry>) {
    registry.declare(NAMESPACE, LIST_METHOD, Declaration::read(RESOURCE_KIND));
    registry.declare(NAMESPACE, GET_METHOD, Declaration::read(RESOURCE_KIND));
    registry.declare(NAMESPACE, START_METHOD, Declaration::write(RESOURCE_KIND));
    registry.declare(NAMESPACE, CANCEL_METHOD, Declaration::write(RESOURCE_KIND));

    let running: Running = Arc::new(Mutex::new(BTreeMap::new()));

    let list_workflows = Arc::clone(&workflows);
    registry.register(NAMESPACE, LIST_METHOD, move |call: RemoteCall| {
        let workflows = Arc::clone(&list_workflows);
        Box::pin(async move { list(&call.args, &workflows) })
    });

    let get_workflows = Arc::clone(&workflows);
    registry.register(NAMESPACE, GET_METHOD, move |call: RemoteCall| {
        let workflows = Arc::clone(&get_workflows);
        Box::pin(async move { get(&call.args, &workflows) })
    });

    let start_workflows = Arc::clone(&workflows);
    let start_running = Arc::clone(&running);
    registry.register(NAMESPACE, START_METHOD, move |call: RemoteCall| {
        let workflows = Arc::clone(&start_workflows);
        let running = Arc::clone(&start_running);
        Box::pin(async move { start(&call.args, &workflows, &running).await })
    });

    let cancel_workflows = Arc::clone(&workflows);
    let cancel_running = Arc::clone(&running);
    registry.register(NAMESPACE, CANCEL_METHOD, move |call: RemoteCall| {
        let workflows = Arc::clone(&cancel_workflows);
        let running = Arc::clone(&cancel_running);
        Box::pin(async move { cancel(&call.args, &workflows, &running) })
    });
}

// ---- requests ---------------------------------------------------------------------------------

/// The body of `workflow.list`.
#[derive(Debug, Default, Deserialize)]
struct ListRequest {
    /// Only runs whose status is exactly this name.
    status: Option<String>,
    /// Only runs that have not finished.
    #[serde(rename = "activeOnly")]
    active_only: Option<bool>,
    /// How many rows to return.
    limit: Option<u32>,
    /// How many rows to skip.
    offset: Option<u32>,
}

/// The body of `workflow.get` and `workflow.cancel`.
#[derive(Debug, Default, Deserialize)]
struct IdRequest {
    /// The run.
    id: Option<String>,
}

/// The body of `workflow.start`.
#[derive(Debug, Default, Deserialize)]
struct StartRequestBody {
    /// The script.
    script: Option<String>,
    /// A title for a listing.
    title: Option<String>,
    /// What the script may name.
    ///
    /// Parsed as JSON values rather than as strings so a non-string argument can be refused with a
    /// message naming the field; `serde`'s own "invalid type" text for a map value does not say which
    /// key was wrong.
    arguments: Option<BTreeMap<String, Value>>,
    /// An id the caller minted, refused when the registry already holds it.
    id: Option<String>,
}

// ---- handlers ---------------------------------------------------------------------------------

/// Reads one run.
fn get(args: &Value, workflows: &Arc<WorkflowRegistry>) -> Result<Value, RemoteError> {
    let request: IdRequest = parse_request(args, GET_METHOD)?;
    let id = require_id(&request, GET_METHOD)?;
    match workflows.get(&id) {
        Some(run) => Ok(project(&run)),
        None => Err(unknown_run(&id, workflows, GET_METHOD)),
    }
}

/// Lists runs, filtered and paged.
fn list(args: &Value, workflows: &Arc<WorkflowRegistry>) -> Result<Value, RemoteError> {
    let request: ListRequest = parse_request(args, LIST_METHOD)?;
    let limit = request.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(invalid(
            LIST_METHOD,
            format!("`limit` must be between 1 and {MAX_LIST_LIMIT}, got {limit}"),
        ));
    }
    let offset = request.offset.unwrap_or(0);

    // Validated rather than ignored: a caller that misspells a status would otherwise receive every
    // run and conclude the filter was applied and matched everything.
    let wanted = match request.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(name) => {
            if !STATUS_NAMES.contains(&name) {
                return Err(invalid(
                    LIST_METHOD,
                    format!(
                        "`status` must be one of {}, got `{name}`",
                        STATUS_NAMES.join(", ")
                    ),
                ));
            }
            Some(name.to_owned())
        }
    };
    let active_only = request.active_only.unwrap_or(false);

    let selected: Vec<WorkflowRun> = workflows
        .ids()
        .into_iter()
        // A run cannot vanish between listing its id and reading it, since both are one registry;
        // reporting a gap by omission would make `total` describe a different set than `items`.
        .filter_map(|id| workflows.get(&id))
        .filter(|run| !active_only || !run.status.is_terminal())
        .filter(|run| {
            wanted
                .as_deref()
                .is_none_or(|name| run.status.as_str() == name)
        })
        .collect();

    let total = selected.len();
    let items: Vec<Value> = selected
        .iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(summary)
        .collect();

    Ok(json!({
        "items": items,
        "shown": items.len(),
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
}

/// Starts a run and returns as soon as it has been accepted.
///
/// The `yield_now` below is the whole reason the response can describe a run: the spawned task's
/// first poll is what writes the `Running` record, and a yield is what lets that poll happen before
/// this function answers. See the module documentation for why the task is kept in `running`.
async fn start(
    args: &Value,
    workflows: &Arc<WorkflowRegistry>,
    running: &Running,
) -> Result<Value, RemoteError> {
    let request: StartRequestBody = parse_request(args, START_METHOD)?;
    let script = request.script.unwrap_or_default();
    let title = request.title.unwrap_or_default();

    // `StartRequest::new` is the domain's own rule about what a startable run is, so a blank script
    // or title is refused by the same code that refuses it for the tool, not by a second copy here.
    let Some(mut domain) = WorkflowStartRequest::new(script, title.clone()) else {
        return Err(invalid(
            START_METHOD,
            "`script` and `title` must both be non-blank: a run with no script does nothing, and one \
             with no title cannot be listed",
        ));
    };
    for (name, value) in request.arguments.unwrap_or_default() {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid(
                START_METHOD,
                "an argument name must not be blank; a script could not read a value it cannot name",
            ));
        }
        let Some(text) = value.as_str() else {
            return Err(invalid(
                START_METHOD,
                format!(
                    "argument `{name}` must be a string, got {}; the local start request carries \
                     string arguments only, and stringifying it here would hide a type error from \
                     the script",
                    type_name(&value)
                ),
            ));
        };
        domain = domain.with_argument(name.to_owned(), text);
    }

    let id = match request.id.as_deref().map(str::trim) {
        Some("") => {
            return Err(invalid(
                START_METHOD,
                "`id` is blank; omit it to have one minted, or pass the id you minted",
            ));
        }
        Some(raw) => RunId::new(raw).ok_or_else(|| {
            invalid(
                START_METHOD,
                "`id` is blank; omit it to have one minted, or pass the id you minted",
            )
        })?,
        None => mint_id().ok_or_else(|| {
            RemoteError::Failed("could not allocate a workflow run id".to_owned())
        })?,
    };

    // The state-transition rule, in the one place a caller can reach the table's write half: a run
    // already under this id is never restarted, whichever side of "finished" it is on.
    if let Some(existing) = workflows.get(id.as_str()) {
        let situation = if existing.status.is_terminal() {
            format!(
                "is already {}, and a finished run is a record of what happened rather than a slot \
                 to reuse",
                existing.status.as_str()
            )
        } else {
            format!(
                "is already {}, and this deployment will not run two runs under one id; use \
                 `workflow.get` to observe it",
                existing.status.as_str()
            )
        };
        return Err(invalid(
            START_METHOD,
            format!("workflow run `{id}` {situation}; omit `id` to start a new run"),
        ));
    }

    let key = id.to_string();
    let own_workflows = Arc::clone(workflows);

    // Pruned before the insert so the table stays bounded by live runs. A handle whose task has
    // finished can no longer abort anything, and `cancel` would find it only to learn that from the
    // domain's own terminal check.
    prune(running);

    let task = tokio::spawn(async move {
        // The outcome needs no handling here: `start` records a failed run in the table before it
        // returns the error, so the reason reaches the caller through `workflow.get` either way.
        let _ = own_workflows.start(id, domain, now_millis()).await;
    });
    {
        let mut handles = lock_handles(running);
        handles.insert(key.clone(), task);
    }

    // One yield: enough for the spawned task's first poll, which is what inserts the run.
    tokio::task::yield_now().await;

    Ok(match await_recorded(workflows, &key).await {
        Some(run) => project(&run),
        None => json!({
            "id": key,
            "title": title,
            "status": WorkflowStatus::Pending.as_str(),
            "terminal": false,
            "accepted": true,
        }),
    })
}

/// Waits, briefly, for a spawned run to appear in the table.
///
/// [`WorkflowRegistry::start`] inserts the `Running` record on its first poll, so this is normally one
/// yield. The loop rather than a single yield is what makes the response describe the run instead of
/// guessing: a scheduler that ran something else first would otherwise make the same call report
/// `pending` on one host and `running` on another. The bound keeps a broken engine from turning a
/// start into a hang, and the fallback is a truthful `pending`.
async fn await_recorded(workflows: &WorkflowRegistry, key: &str) -> Option<WorkflowRun> {
    for _ in 0..16 {
        if let Some(run) = workflows.get(key) {
            return Some(run);
        }
        tokio::task::yield_now().await;
    }
    None
}

/// Cancels a run.
fn cancel(
    args: &Value,
    workflows: &Arc<WorkflowRegistry>,
    running: &Running,
) -> Result<Value, RemoteError> {
    let request: IdRequest = parse_request(args, CANCEL_METHOD)?;
    let id = require_id(&request, CANCEL_METHOD)?;

    // The id is checked against the table before the handle table is touched, so a typo aborts
    // nothing and reports what a caller can act on — the ids that do exist.
    if workflows.get(&id).is_none() {
        return Err(unknown_run(&id, workflows, CANCEL_METHOD));
    }

    // Aborted before the status is recorded, so the domain's terminal check judges a run that is
    // still in flight. The other order would abort a task whose status had just been written, and
    // the write is the very thing the abort exists to prevent.
    let interrupted = abort(&id, running);

    match workflows.cancel(&id) {
        Ok(status) => Ok(json!({
            "id": id,
            "status": status.as_str(),
            "interrupted": interrupted,
        })),
        Err(error @ WorkflowError::AlreadyFinished { .. }) => {
            Err(invalid(CANCEL_METHOD, error.to_string()))
        }
        // `Unknown` cannot happen: the id was found above and nothing removes runs from the table.
        // Reported rather than assumed away, because a second read is cheap and a silent success for
        // a run nobody holds is exactly the shape this module refuses elsewhere.
        Err(error) => Err(invalid(CANCEL_METHOD, error.to_string())),
    }
}

// ---- the handle table -------------------------------------------------------------------------

/// Forgets every handle whose task has finished.
fn prune(running: &Running) {
    let mut handles = lock_handles(running);
    handles.retain(|_, handle| !handle.is_finished());
}

/// Aborts one run's task, reporting whether there was still something to abort.
///
/// `false` is not a failure: a run whose script finished without yielding has already written its
/// terminal status, and the domain's own refusal to cancel it is what the caller sees instead.
fn abort(id: &str, running: &Running) -> bool {
    let mut handles = lock_handles(running);
    let Some(handle) = handles.remove(id) else {
        return false;
    };
    if handle.is_finished() {
        return false;
    }
    handle.abort();
    true
}

/// Locks the handle table.
///
/// A poisoned lock is recovered rather than propagated: the map holds join handles, which no panic
/// can leave half-updated, and refusing every later call because one task panicked would turn one
/// broken run into a broken namespace.
fn lock_handles(
    running: &Running,
) -> MutexGuard<'_, BTreeMap<String, tokio::task::JoinHandle<()>>> {
    running.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- projection -------------------------------------------------------------------------------

/// Projects one run for the wire, with everything a client renders.
fn project(run: &WorkflowRun) -> Value {
    let phases: Vec<Value> = run
        .phases
        .iter()
        .map(|phase| {
            json!({
                "name": phase.name,
                "startedAt": phase.started_at,
                "endedAt": phase.ended_at,
                "running": phase.ended_at.is_none(),
            })
        })
        .collect();
    let agents: Vec<Value> = run
        .agents
        .values()
        .map(|agent| {
            json!({
                "label": agent.label,
                "prompt": agent.prompt,
                "output": agent.output,
                "result": agent.result,
                "finishedAt": agent.finished_at,
                "done": agent.finished_at.is_some(),
            })
        })
        .collect();

    json!({
        "id": run.id.as_str(),
        "title": run.title,
        "status": run.status.as_str(),
        "terminal": run.status.is_terminal(),
        // The failure text, in the status itself. A client that saw `failed` with no reason would
        // have to ask a second question to learn the only thing worth showing.
        "reason": failure_reason(&run.status),
        "startedAt": run.started_at,
        "currentPhase": run.current_phase().map(|phase| phase.name.clone()),
        "phases": phases,
        "agents": agents,
        "finishedAgents": run.finished_agents(),
        "agentCount": run.agents.len(),
        // `null` rather than omitted: a completed run whose script returned nothing is not the same
        // as a run that has not produced a value yet, and the status already tells the two apart.
        "result": run.result,
    })
}

/// Projects one run for a listing, without the fields a table does not render.
///
/// A listing of twenty runs that each carried their agents' full output would be a listing nobody
/// scrolls; the summary is what a row shows, and `workflow.get` is how a client asks for the rest.
fn summary(run: &WorkflowRun) -> Value {
    json!({
        "id": run.id.as_str(),
        "title": run.title,
        "status": run.status.as_str(),
        "terminal": run.status.is_terminal(),
        "startedAt": run.started_at,
        "currentPhase": run.current_phase().map(|phase| phase.name.clone()),
        "phaseCount": run.phases.len(),
        "agentCount": run.agents.len(),
        "finishedAgents": run.finished_agents(),
    })
}

/// The domain's failure text, or `null` when the status carries none.
fn failure_reason(status: &WorkflowStatus) -> Option<&str> {
    match status {
        WorkflowStatus::Failed { reason } => Some(reason.as_str()),
        _ => None,
    }
}

/// Names a JSON value's kind, for an error that has to say what was wrong.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

// ---- helpers ----------------------------------------------------------------------------------

/// Mints an id no other call in this process can mint.
///
/// A timestamp, the process id, and a process-wide counter: the timestamp keeps a log roughly
/// chronological, the counter is what actually guarantees uniqueness inside the process, and the
/// process id keeps two processes writing to one shared table apart.
fn mint_id() -> Option<RunId> {
    let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    RunId::new(format!(
        "workflow-{}-{}-{sequence}",
        std::process::id(),
        now_millis()
    ))
}

/// Milliseconds since the epoch, or zero when the clock is before it.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Parses a method's arguments, naming the method when it fails.
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
/// run" would send the caller looking for a run instead of a field.
fn require_id(request: &IdRequest, method: &str) -> Result<String, RemoteError> {
    match request.id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(invalid(method, "`id` is required")),
    }
}

/// Builds the error for an id no run holds.
///
/// The known ids are part of the message because the usual cause is a mistyped or truncated id, and
/// the fix is visible only when the real ones are.
fn unknown_run(id: &str, workflows: &WorkflowRegistry, method: &str) -> RemoteError {
    let known = workflows.ids();
    let message = if known.is_empty() {
        format!("no workflow run `{id}`; there are no runs")
    } else {
        format!("no workflow run `{id}`; known runs: {}", known.join(", "))
    };
    invalid(method, message)
}

/// Builds an `INVALID_ARGUMENTS` error for `method`.
fn invalid(method: &str, message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: message.into(),
    }
}
