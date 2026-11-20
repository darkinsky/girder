//! The `workflow_run` and `workflow_status` tools: starting an orchestration script and reading the run table.
//!
//! [`crate::workflow::Registry`] is the observation half of a workflow: it records runs, phases and agents. These
//! two tools are the model-facing half. `workflow_run` supplies the execution half — it needs an engine mounted on
//! the registry — and `workflow_status` reads whatever the registry already holds, including runs started by
//! something other than a tool call.
//!
//! # Why `workflow_run` waits for the script to finish
//!
//! A workflow's value is its final result. A tool that returned an id would make the model spend a second call
//! polling for something the first call could have returned, and the model has no way to know how long to wait.
//! The cost is that the tool call is as long as the script; that cost is the caller's choice of script, not a
//! property of this tool.
//!
//! # Why cancellation is best-effort
//!
//! `Registry::start` takes no cancellation parameter and drives the engine to completion inside its own future.
//! The only cancellation this module can offer without changing `workflow.rs` is to stop polling that future and
//! record the run as cancelled. That is real for an engine that awaits (a dropped future is never polled again) and
//! a lie for an engine that spawned work onto its own tasks, which keeps running after the drop. The tool therefore
//! says so in its result instead of claiming the script stopped.
//!
//! # Why the paths below are spelled `girder_core::…`
//!
//! The crate aliases itself as `girder_core` so generated code can name it, and the integration test that covers
//! this module includes the file directly. Spelling the paths through the crate root keeps one copy of them working
//! both inside the crate and from outside it, where `crate::…` would name the test crate.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use girder_core::{
    RunId, ToolDefinition, ToolRegistry, ToolResult, ToolRunContext, WorkflowRegistry, WorkflowRun,
    WorkflowStartRequest, WorkflowStatus,
};

/// How much of an agent's output the status rendering repeats.
///
/// A cap rather than the whole thing: an agent's output can be a transcript, and a status read whose useful line
/// is pushed off the end by one agent's prose is a status read nobody finishes.
const MAX_PREVIEW_CHARS: usize = 400;

/// Makes two run ids allocated in the same millisecond distinct.
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Builds the `workflow_run` tool over `registry`.
///
/// The registry is captured rather than taken as an argument: a model must not choose which table its run is
/// recorded in, or a status read would look at a table the run was never written to.
#[must_use]
pub fn workflow_run_tool(registry: Arc<WorkflowRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "workflow_run",
        "Runs an orchestration workflow script to completion and returns the run's final status, phases, agents \
         and result. This call blocks until the script finishes. Use `workflow_status` to read a run again later \
         or to list runs.",
        json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The workflow script to execute."
                },
                "title": {
                    "type": "string",
                    "description": "A short title for the run, so a listing says what it was for."
                },
                "arguments": {
                    "type": "object",
                    "description": "Values the script may name by its declared inputs.",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["script", "title"],
            "additionalProperties": false
        }),
        Arc::new(move |args: serde_json::Value, ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move { Ok(run(&args, &registry, &ctx).await) })
        }),
    )
}

/// Builds the `workflow_status` tool over `registry`.
#[must_use]
pub fn workflow_status_tool(registry: Arc<WorkflowRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "workflow_status",
        "Reads a workflow run: with `id`, its status, phases, agents and result; without `id`, every run's id and \
         status.",
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The run to read, as returned by `workflow_run`. Omit it to list every run."
                }
            },
            "additionalProperties": false
        }),
        // The context is unneeded by a read: a listing is one locked map walk, so there is nothing in it that
        // could be interrupted to a useful effect.
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move { Ok(status(&args, &registry)) })
        }),
    )
}

/// Registers both workflow tools against one tool registry.
///
/// Registered together because the pair is useless apart: a run nobody can read is as unhelpful as a listing of
/// runs nobody can start.
pub fn register_workflow_tools(tools: &ToolRegistry, registry: Arc<WorkflowRegistry>) {
    tools.register(workflow_run_tool(Arc::clone(&registry)));
    tools.register(workflow_status_tool(registry));
}

/// Validates the arguments, starts one run, and waits for its outcome.
async fn run(
    args: &serde_json::Value,
    registry: &Arc<WorkflowRegistry>,
    ctx: &ToolRunContext,
) -> ToolResult {
    let script = match string_arg(args, "script") {
        Ok(value) => value,
        Err(message) => return ToolResult::error(message),
    };
    let title = match string_arg(args, "title") {
        Ok(value) => value,
        Err(message) => return ToolResult::error(message),
    };
    let Some(mut request) = WorkflowStartRequest::new(script, title) else {
        return ToolResult::error(
            "`script` and `title` must both be non-blank: a run with no script does nothing, and one with no title \
             cannot be listed",
        );
    };
    match arguments(args) {
        Ok(values) => {
            for (name, value) in values {
                request = request.with_argument(name, value);
            }
        }
        Err(message) => return ToolResult::error(message),
    }

    // The id is allocated before the run exists because a failed run is still recorded under it: a caller whose
    // tool call failed must be able to read the run that failed, and the failure text is the only place it can
    // learn the id from.
    let Some(id) = next_run_id() else {
        return ToolResult::error("could not allocate a workflow run id");
    };
    if ctx.cancel.is_cancelled() {
        // Starting now would record a run nobody is waiting for and leave the model with an id it will never
        // read.
        return ToolResult::error("cancelled before the run started");
    }

    let key = id.to_string();
    let started = registry.start(id, request, now_millis());
    tokio::pin!(started);
    tokio::select! {
        outcome = &mut started => match outcome {
            Ok(record) => ToolResult::text(render(&record)),
            Err(error) => match registry.get(&key) {
                // The registry keeps the failed run, so the failure text carries its detail: the bare reason
                // would lose the phases and agents that show where the script stopped.
                Some(record) => ToolResult::error(format!("{error}\n{}", render(&record))),
                None => ToolResult::error(error.to_string()),
            },
        },
        () = ctx.cancel.cancelled() => record_cancellation(registry, &key),
    }
}

/// Records a cancellation for a run whose future this tool is about to drop.
fn record_cancellation(registry: &Arc<WorkflowRegistry>, key: &str) -> ToolResult {
    // `Registry::start` has no cancellation parameter, so the cancellation this tool can perform is the drop of
    // the future that drives the engine plus the status the registry records. See the module documentation for
    // where that is honest and where it is not.
    match registry.cancel(key) {
        Ok(status) => ToolResult::error(format!(
            "cancelled workflow run `{key}` ({}); the script is not interruptible mid-step, so an engine that \
             spawned its own work may still be running it",
            status.as_str()
        )),
        Err(error) => match registry.get(key) {
            // The run finished in the same instant the cancellation arrived. Its result is real, and reporting
            // it as a cancellation would discard work that happened.
            Some(record) => ToolResult::text(format!(
                "{}\n(the caller cancelled as the run finished; this is how it ended)",
                render(&record)
            )),
            None => ToolResult::error(format!(
                "cancelled workflow run `{key}`, which is no longer recorded: {error}"
            )),
        },
    }
}

/// Reads one run by id, or lists every run when no id is given.
#[must_use]
fn status(args: &serde_json::Value, registry: &WorkflowRegistry) -> ToolResult {
    match args.get("id") {
        None | Some(serde_json::Value::Null) => listing(registry),
        Some(value) => {
            let Some(id) = value.as_str() else {
                return ToolResult::error("`id` must be a string");
            };
            let id = id.trim();
            if id.is_empty() {
                // An empty id is a model that meant to list runs but typed the field. Refusing says which call it
                // should have made; silently listing would let it believe it had asked about a run.
                return ToolResult::error("`id` is empty; omit it to list every run");
            }
            if let Some(record) = registry.get(id) {
                return ToolResult::text(render(&record));
            }
            // The known ids are part of the message because the usual cause is a mistyped or truncated id, and
            // the fix is visible only when the real ones are.
            let known = registry.ids();
            if known.is_empty() {
                ToolResult::error(format!("no workflow run `{id}`; there are no runs"))
            } else {
                ToolResult::error(format!(
                    "no workflow run `{id}`; known runs: {}",
                    known.join(", ")
                ))
            }
        }
    }
}

/// Renders every run as one line, so a listing stays scannable as the table grows.
#[must_use]
fn listing(registry: &WorkflowRegistry) -> ToolResult {
    let ids = registry.ids();
    if ids.is_empty() {
        return ToolResult::text("no workflow runs");
    }

    let mut out = format!("{} run(s)", ids.len());
    let running = registry.active();
    if running.is_empty() {
        out.push_str(", none running");
    } else {
        let _ = write!(
            out,
            "; {} still running: {}",
            running.len(),
            running.join(", ")
        );
    }

    for id in ids {
        // A run cannot vanish between listing its id and reading it, since both are one registry; reporting the
        // impossible case rather than skipping it keeps the count in the header honest.
        let Some(record) = registry.get(&id) else {
            let _ = write!(out, "\n- {id} [gone from the registry]");
            continue;
        };
        let _ = write!(
            out,
            "\n- {} [{}] {} ({} phase(s), {}/{} agent(s) finished)",
            record.id,
            status_line(&record.status),
            record.title,
            record.phases.len(),
            record.finished_agents(),
            record.agents.len()
        );
    }
    ToolResult::text(out)
}

/// Renders one run: its status, its phases, its agents, and its result.
#[must_use]
fn render(record: &WorkflowRun) -> String {
    let mut out = format!(
        "run `{}`: {} ({})\nstarted at {} ms since the epoch; {} phase(s), {}/{} agent(s) finished",
        record.id,
        status_line(&record.status),
        record.title,
        record.started_at,
        record.phases.len(),
        record.finished_agents(),
        record.agents.len()
    );

    for phase in &record.phases {
        match phase.ended_at {
            Some(ended) => {
                let _ = write!(
                    out,
                    "\n- phase `{}` {} -> {}",
                    phase.name, phase.started_at, ended
                );
            }
            None => {
                let _ = write!(
                    out,
                    "\n- phase `{}` from {} (running)",
                    phase.name, phase.started_at
                );
            }
        }
    }

    for agent in record.agents.values() {
        let _ = write!(out, "\n- agent `{}`", agent.label);
        match agent.finished_at {
            Some(at) => {
                let _ = write!(out, " finished at {at}");
            }
            None => out.push_str(" still running"),
        }
        // The result before the output: the result is what the script asked for, and the output is the prose it
        // was extracted from.
        if let Some(result) = &agent.result {
            let _ = write!(out, "; result {}", preview(&result.to_string()));
        }
        if let Some(output) = &agent.output {
            let _ = write!(out, "; output {}", preview(output));
        }
    }

    match &record.result {
        Some(result) => {
            let _ = write!(out, "\nresult: {}", pretty_json(result));
        }
        None => out.push_str("\nresult: (none)"),
    }
    out
}

/// A status as one phrase, naming the reason a run failed.
///
/// [`WorkflowStatus::as_str`] is the wire name and drops the reason; a reader asking about a failed run needs the
/// reason, and a second field for it would let the two disagree.
#[must_use]
fn status_line(status: &WorkflowStatus) -> String {
    match status {
        WorkflowStatus::Failed { reason } => format!("failed: {reason}"),
        other => other.as_str().to_owned(),
    }
}

/// Shortens a value for a one-line listing.
///
/// Whitespace is collapsed because agent output is usually multi-line, and a listing whose line breaks are supplied
/// by the agent is a listing that reads as several agents.
#[must_use]
fn preview(text: &str) -> String {
    let mut out = String::new();
    let mut truncated = false;
    let mut after_space = false;
    for (count, ch) in text.chars().enumerate() {
        if count >= MAX_PREVIEW_CHARS {
            truncated = true;
            break;
        }
        if ch.is_whitespace() {
            if !after_space {
                out.push(' ');
                after_space = true;
            }
        } else {
            out.push(ch);
            after_space = false;
        }
    }
    if truncated {
        out.push('\u{2026}');
    }
    out
}

/// Pretty JSON, because a workflow's result is an object a person reads.
///
/// The fallback exists for the pathological value serde cannot render; a status read must not fail over it.
#[must_use]
fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Parses the optional `arguments` object.
fn arguments(args: &serde_json::Value) -> Result<BTreeMap<String, String>, String> {
    let Some(raw) = args.get("arguments") else {
        return Ok(BTreeMap::new());
    };
    // `null` is how a model spells "I have none" when the schema did not force it to omit the field.
    if raw.is_null() {
        return Ok(BTreeMap::new());
    }
    let Some(map) = raw.as_object() else {
        return Err("`arguments` must be an object of string values".to_owned());
    };
    let mut values = BTreeMap::new();
    for (name, value) in map {
        let Some(text) = value.as_str() else {
            return Err(format!(
                "argument `{name}` must be a string; a run passes its inputs as text"
            ));
        };
        values.insert(name.clone(), text.to_owned());
    }
    Ok(values)
}

/// Reads a required string argument.
fn string_arg<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        None => Err(format!("`{key}` is required")),
        Some(value) => value
            .as_str()
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}

/// Allocates a run id that is unique within this process and readable across restarts.
///
/// The pid keeps two processes apart and the millisecond keeps two runs of one process apart; the counter covers
/// two runs started within the same millisecond, which is exactly what a test does.
fn next_run_id() -> Option<RunId> {
    let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    RunId::new(format!(
        "workflow-{}-{}-{sequence}",
        std::process::id(),
        now_millis()
    ))
}

/// Milliseconds since the epoch, or zero when the clock is before it.
///
/// Zero rather than an error: the timestamp is for a reader, and a status read that failed because the host clock
/// is wrong would be unfixable from the model's side.
#[must_use]
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
