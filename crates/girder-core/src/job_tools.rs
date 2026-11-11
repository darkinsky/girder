//! The job tools: `job_start`, `job_list`, `job_output`, and `job_kill`.
//!
//! [`crate::jobs`] owns the lifecycle; these four functions are the model-facing surface over it, and
//! they exist for one reason: a tool call blocks, a job does not. `bash` returns when its command
//! exits, so a dev server can never be started with it. `job_start` returns an id instead, and the
//! model reads the output later while the conversation continues.
//!
//! # Trade-offs
//!
//! **`job_output` returns the retained tail, not a delta.** These tools expose an id and a wait bound
//! and nothing else, so there is no cursor to resume from and every call re-reads what is still in
//! the ring. [`crate::jobs::OutputRing::read_from`] already takes an absolute byte offset, so an
//! incremental read is an argument away; until then the tool says when the ring has dropped bytes,
//! because a caller that took a re-read for continuity would silently lose the middle of a log.
//!
//! Two bounds shorten a read and they are reported separately: the ring's cap drops the oldest bytes
//! for good, and [`MAX_OUTPUT_BYTES`] drops what does not fit in one result but leaves it in the
//! ring. With a ring no larger than [`MAX_OUTPUT_BYTES`] the first bound is reached first, so the
//! second only appears when a producer configured a wide ring; a caller that treated the two as one
//! message could not tell "ask again with a bigger window" from "it is gone".
//!
//! **A job has no deadline.** [`crate::subprocess::RunningProcess`] has no timeout of its own, so the
//! `timeout` on [`SpawnOptions`] is not enforced on this path. The value below is set high and
//! explicitly rather than left at the tool-call default, so that a producer which *does* start
//! honouring it cannot kill a dev server after two minutes. Stopping a runaway job is `job_kill`'s
//! job, and that is a deliberate division: a background job that a clock can end is not a background
//! job.
//!
//! **The child gets a baseline environment, not the harness's.** A command the model wrote must not
//! read a credential the harness was launched with, so the environment is cleared and rebuilt from
//! [`crate::shell::Environment::baseline`] — `PATH`, `HOME`, the locale, `TMPDIR`. The `bash` tool is
//! treated the same way; a job that inherited everything would be the hole in that policy.
//!
//! **`job_kill` reports `stopping`, not `killed`.** A process group needs time to die, and a caller
//! told "killed" would start a second job against a port the first still holds. The registry settles
//! the job to `killed` once the group is gone, and `job_output` with a wait bound is how a caller
//! waits for that.
//!
//! **A read-only session may not start a job.** A background shell command is still a shell command,
//! and refusing it after the fact is not refusing: the check runs before anything is registered.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::jobs::{JobError, JobId, JobRegistry, spawn_job};
use crate::shell::SandboxMode;
use crate::subprocess::SpawnOptions;
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// The producer kind recorded for a job started by these tools.
///
/// One kind rather than one per command: [`JobRegistry::live_of_kind`] exists so a caller can ask
/// "is another job holding this resource", and a kind that varied per command would make that
/// question unanswerable.
const KIND: &str = "shell";

/// How much of a job's output one `job_output` call returns.
///
/// Mirrors the shell bound: far above what a model can usefully read and far below a context window,
/// so a chatty dev server is truncated with a count rather than flooding the turn.
const MAX_OUTPUT_BYTES: usize = crate::shell::DEFAULT_MAX_BYTES;

/// The longest `job_output` will wait for a job to settle.
///
/// A tool call holds the turn open. A model that wants to watch a job for an hour should poll across
/// turns instead of pinning one call, so the bound is a ceiling rather than a default.
const MAX_WAIT_MS: u64 = 30_000;

/// How often a waiting `job_output` re-reads the registry.
///
/// Polling rather than a notification channel: the registry is the single source of truth for a job's
/// status, and a second channel would be a second thing to keep in step. Short enough that a settle
/// feels immediate, long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How much of the command line a job's label keeps.
///
/// The label is for a human reading `job_list`; the whole command is in the output, and a label that
/// repeated a thousand-character script would push every other job off the line.
const MAX_LABEL_CHARS: usize = 80;

/// The deadline handed to a job's producer, in seconds.
///
/// A day. See the module note: this is not enforced today, and stating it explicitly is what keeps a
/// future producer from applying the two-minute tool-call default to a dev server.
const JOB_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;

/// Hands out job ids.
///
/// Process id plus a monotonic counter plus a timestamp: two harness processes writing to one registry
/// cannot collide on the counter, and a restarted process cannot reuse an id a reader still holds a
/// saved position for.
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Builds the `job_start` tool.
///
/// The one of the four that creates something: it registers the job and returns the id. The spawned
/// task outlives this call by design — that is the difference between this and `bash` — so the
/// handle is dropped rather than awaited.
#[must_use]
pub fn job_start_tool(jobs: Arc<JobRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "job_start",
        "Starts a command in the background and returns its job id",
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run in the background"
                }
            },
            "required": ["command"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let jobs = Arc::clone(&jobs);
            Box::pin(async move { Ok(start(&args, &jobs, &ctx)) })
        }),
    )
}

/// Builds the `job_list` tool.
///
/// Every job in the registry, not only the live ones: a caller that saw only running jobs could not
/// tell a job that finished from one that was never started, and would start it again.
#[must_use]
pub fn job_list_tool(jobs: Arc<JobRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "job_list",
        "Lists background jobs and their statuses",
        json!({ "type": "object", "properties": {} }),
        Arc::new(move |_args: Value, _ctx: ToolRunContext| {
            let jobs = Arc::clone(&jobs);
            Box::pin(async move { Ok(list(&jobs)) })
        }),
    )
}

/// Builds the `job_output` tool.
///
/// Reads what a job has written so far, optionally waiting for it to settle. Reading does not disturb
/// the job: the ring is read-only here, so a model may poll it between other tool calls.
#[must_use]
pub fn job_output_tool(jobs: Arc<JobRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "job_output",
        "Reads a background job's output, optionally waiting for it to finish",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id returned by job_start" },
                "wait_ms": {
                    "type": "number",
                    "description": "How long to wait for the job to finish before answering, in milliseconds"
                }
            },
            "required": ["id"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let jobs = Arc::clone(&jobs);
            Box::pin(async move { output(&args, &jobs, &ctx).await })
        }),
    )
}

/// Builds the `job_kill` tool.
///
/// Stops a job, and reports `stopping` rather than pretending the process is already gone.
#[must_use]
pub fn job_kill_tool(jobs: Arc<JobRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "job_kill",
        "Stops a background job",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id returned by job_start" }
            },
            "required": ["id"]
        }),
        Arc::new(move |args: Value, _ctx: ToolRunContext| {
            let jobs = Arc::clone(&jobs);
            Box::pin(async move { Ok(kill(&args, &jobs)) })
        }),
    )
}

/// Registers and spawns a job for `command`.
///
/// Synchronous because everything it calls is: the work itself runs on the spawned task, and the
/// caller's only job is to hand back the id.
fn start(args: &Value, registry: &Arc<JobRegistry>, ctx: &ToolRunContext) -> ToolResult {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    if command.is_empty() {
        return ToolResult::error("`command` is required");
    }

    // A spawn into a turn that is already cancelled would leave a process nobody is waiting for and
    // an id nobody will ever read.
    if ctx.cancel.is_cancelled() {
        return ToolResult::error("cancelled before the job started");
    }

    // A background shell command is still a shell command, so it answers to the same policy as
    // `bash`. Checking before the spawn is the whole point: an approval after the process exists is
    // not an approval.
    if let Some(permissions) = &ctx.permissions
        && permissions.knobs().sandbox == SandboxMode::ReadOnly
    {
        return ToolResult::error(
            "this session is read-only, so a background command may not start",
        );
    }

    let Some(id) = next_id() else {
        return ToolResult::error("could not allocate a job id");
    };

    let options = spawn_options(&command, ctx.workspace.as_deref());
    match spawn_job(
        Arc::clone(registry),
        &id,
        KIND,
        label_for(&command),
        options,
    ) {
        // The handle is dropped: `spawn_job` owns the task, and dropping a `JoinHandle` detaches
        // rather than aborts, which is exactly the lifetime a background job should have.
        Ok(_handle) => ToolResult::text(format!(
            "started `{id}` in the background\nread it with job_output id `{id}`"
        )),
        Err(error) => ToolResult::error(format!("cannot start a job: {error}")),
    }
}

/// Renders every registered job as one line each.
fn list(registry: &JobRegistry) -> ToolResult {
    let ids = registry.ids();
    if ids.is_empty() {
        return ToolResult::text("no jobs");
    }

    let mut out = String::new();
    let _ = writeln!(out, "{} job(s)", ids.len());
    for id in ids {
        // A job can vanish between listing its id and reading it only if the registry were replaced
        // underneath, which it cannot be; reporting it rather than skipping keeps the count honest
        // either way.
        let job = match registry.get(&id) {
            Ok(job) => job,
            Err(error) => {
                let _ = writeln!(out, "- {id} [unknown] {error}");
                continue;
            }
        };
        let _ = write!(
            out,
            "- {} [{}] {}: {}",
            job.id,
            job.status.as_str(),
            job.kind,
            job.label
        );
        if let Some(code) = job.code {
            let _ = write!(out, " (exit {code})");
        }
        let _ = writeln!(out);
    }
    ToolResult::text(out)
}

/// Asks the registry to stop a job.
fn kill(args: &Value, registry: &JobRegistry) -> ToolResult {
    let Some(id) = id_arg(args) else {
        return ToolResult::error("`id` is required");
    };

    match registry.request_stop(&id) {
        Ok(job) => ToolResult::text(format!(
            "asked `{}` to stop; it is `{}` until its process group exits",
            job.id,
            job.status.as_str()
        )),
        // A job that already finished is not a failure: the caller wanted it gone, and it is.
        Err(JobError::AlreadySettled { id, status }) => {
            ToolResult::text(format!("`{id}` is already {status}; nothing to stop"))
        }
        Err(error) => ToolResult::error(format!("cannot stop `{id}`: {error}")),
    }
}

/// Reads a job's output, waiting for settlement when asked.
async fn output(
    args: &Value,
    registry: &JobRegistry,
    ctx: &ToolRunContext,
) -> Result<ToolResult, crate::tools::ToolError> {
    let Some(id) = id_arg(args) else {
        return Ok(ToolResult::error("`id` is required"));
    };
    let wait_ms = args
        .get("wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_WAIT_MS);

    let mut job = match registry.get(&id) {
        Ok(job) => job,
        Err(error) => return Ok(ToolResult::error(error.to_string())),
    };

    // Wait for a terminal state only. Returning as soon as a byte arrives would make a caller that
    // asked for the finished output pay for a second call, and the caller cannot know which arrival
    // is the last one.
    let mut cancelled = false;
    if wait_ms > 0 && !job.is_settled() {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                () = ctx.cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                () = tokio::time::sleep(remaining.min(POLL_INTERVAL)) => {}
            }
            job = match registry.get(&id) {
                Ok(job) => job,
                Err(error) => return Ok(ToolResult::error(error.to_string())),
            };
            if job.is_settled() {
                break;
            }
        }
    }

    Ok(ToolResult::text(render(&job, cancelled)))
}

/// Renders one job's status line, its retained output, and every reason the output is incomplete.
///
/// The notes exist for the same reason: a caller that could not tell a truncated log from a short one
/// would draw conclusions the evidence does not support. "Still running", "the ring dropped the
/// head", "this result omitted bytes", and "the wait was cancelled" each mean a different thing about
/// the text that follows, and a single "truncated" flag would collapse them into one.
fn render(job: &crate::jobs::Job, cancelled: bool) -> String {
    let mut out = String::new();
    let _ = write!(out, "{}: {}", job.id, job.status.as_str());
    if let Some(code) = job.code {
        let _ = write!(out, " (exit {code})");
    }
    let _ = writeln!(out);
    if !job.is_settled() {
        let _ = writeln!(out, "(still running; this is its output so far)");
    }
    if job.output.head_offset() > 0 {
        let _ = writeln!(
            out,
            "({} bytes at the head were dropped by the output ring)",
            job.output.head_offset()
        );
    }

    let (body, omitted) = tail(&job.output.text(), MAX_OUTPUT_BYTES);
    if omitted > 0 {
        let _ = writeln!(
            out,
            "({omitted} bytes omitted; showing the most recent {MAX_OUTPUT_BYTES})"
        );
    }
    if cancelled {
        // Said explicitly: the wait ended, the job did not.
        let _ = writeln!(out, "(the wait was cancelled; the job is unaffected)");
    }

    if body.is_empty() {
        let _ = writeln!(out, "(no output yet)");
    } else {
        out.push_str("---\n");
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Returns the last `max` bytes of `text` and how many bytes were dropped.
///
/// A byte cut can land inside a multi-byte character, which would panic on slicing; the boundary is
/// advanced to the next character instead, so the return value is always valid UTF-8.
fn tail(text: &str, max: usize) -> (String, usize) {
    if text.len() <= max {
        return (text.to_owned(), 0);
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_owned(), start)
}

/// Reads the required `id` argument.
fn id_arg(args: &Value) -> Option<String> {
    let id = args.get("id").and_then(Value::as_str)?.trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_owned())
}

/// Builds the spawn request for a background command.
///
/// The environment is cleared and rebuilt from the baseline allowlist rather than inherited: an
/// inherited environment is how a harness credential reaches a model-authored command.
fn spawn_options(command: &str, cwd: Option<&Path>) -> SpawnOptions {
    let shell = if crate::exec::has_program("bash") {
        "bash"
    } else {
        "sh"
    };
    let mut options = SpawnOptions::new(shell, vec!["-c".to_owned(), command.to_owned()])
        .with_timeout(Duration::from_secs(JOB_TIMEOUT_SECONDS))
        .with_cleared_environment();

    // `vars_os` rather than `vars`: a single non-UTF-8 variable would make `vars` panic, and an
    // environment is exactly the place where one turns up.
    let parent: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            let value = value.into_string().ok()?;
            Some((name, value))
        })
        .collect();
    for (name, value) in crate::shell::Environment::baseline().resolve(&parent) {
        options = options.with_env(name, value);
    }

    if let Some(cwd) = cwd {
        options = options.with_cwd(cwd);
    }
    options
}

/// Shortens a command line to something a `job_list` line can carry.
fn label_for(command: &str) -> String {
    if command.chars().count() <= MAX_LABEL_CHARS {
        return command.to_owned();
    }
    let mut label: String = command.chars().take(MAX_LABEL_CHARS).collect();
    label.push_str("...");
    label
}

/// Allocates an id unique to this process and this moment.
///
/// `None` only for an empty string, which the format cannot produce; the caller reports it instead of
/// pretending an id exists.
fn next_id() -> Option<JobId> {
    let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    JobId::new(format!("job-{}-{nanos}-{sequence}", std::process::id()))
}

/// Registers every job tool against one registry.
///
/// A convenience so a host does not repeat four registrations and risk registering three: the four tools are useless
/// apart, since starting a job without being able to read it is worse than not starting one.
pub fn register_job_tools(tools: &crate::tools::ToolRegistry, jobs: Arc<JobRegistry>) {
    tools.register(job_start_tool(Arc::clone(&jobs)));
    tools.register(job_list_tool(Arc::clone(&jobs)));
    tools.register(job_output_tool(Arc::clone(&jobs)));
    tools.register(job_kill_tool(jobs));
}
