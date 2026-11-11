//! The terminal tools: `terminal_start`, `terminal_send`, and `terminal_read`.
//!
//! [`crate::terminal`] owns sessions: a shell that survives between calls, a bounded transcript, and a
//! registry of both. What it does not own is a *handle a model can hold*. An id is only useful if some
//! tool mints it, another writes to it, and a third reads from it, so this module is the smallest
//! surface that makes a session addressable by a model.
//!
//! # Why three tools rather than one tool with an `action` parameter
//!
//! An action union forces every field into one schema and makes each field meaningful for only one
//! action, which is how a model ends up passing `input` to a read. Separate tools let each schema
//! state exactly what its call needs, and the tool name the model picks is itself the decision.
//!
//! # Why `close` is a parameter of `terminal_send`
//!
//! Closing is addressed by the same id, has no payload, and produces nothing a model needs to read.
//! A fourth tool would take a slot in every schema list the model is shown for one boolean, and the
//! verb `terminal_send` already owns "make this terminal do something". The trade-off is that a
//! closing call still names `terminal_send`; the description and the receipt both say `closed` so the
//! transcript is unambiguous.
//!
//! # Why a launcher trait instead of constructing the shell here
//!
//! A [`Shell`] is a real process, a pty, or a remote transport. Nothing in this crate owns one yet,
//! and building a fake here would hand a model a terminal that prints nothing and exits never.
//! Construction is therefore a dependency ([`TerminalLauncher`]) the deployment supplies: a host with
//! a real shell passes its factory, and a host without one mounts [`UnavailableLauncher`], which
//! refuses with a message naming what is missing. A refusal the model can read is worth more than a
//! session that looks alive and answers nothing.
//!
//! # Why the wait bound belongs to the tool
//!
//! A tool call holds the whole turn open. [`crate::terminal::Session::read`] waits as long as it is
//! told to, so a model that asked for an hour would pin one call for an hour and see nothing else.
//! The bound is clamped here, where the cost of a long wait is known, and the receipt says how long
//! it waited so a second call can ask for more.
//!
//! # Why the transcript is not returned by default
//!
//! `terminal_read` returns what arrived *since the last read*, because that is the question a model
//! following a command is asking. Re-sending the whole scrollback would make the answer grow with the
//! session instead of with the work; `full: true` asks for it explicitly, and the byte count that was
//! dropped is reported so "the beginning is gone" is visible rather than inferred.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::scope::BoxFut;
use crate::shell::SandboxMode;
use crate::terminal::{Launch, Registry as TerminalRegistry, Session, Shell, Size, TerminalError};
use crate::tools::{ToolDefinition, ToolRegistry, ToolResult, ToolRunContext};

/// How long `terminal_read` waits by default.
///
/// Long enough for a command that was just sent to print its first line, short enough that a turn
/// that reads an idle prompt gets its answer back before a person notices the pause.
pub const DEFAULT_READ_WAIT_MS: u64 = 500;

/// The longest any tool here waits for output.
///
/// A model that needs to watch a long build should poll across turns; one call that waits ten minutes
/// is a call nothing else in the session can happen during.
pub const MAX_WAIT_MS: u64 = 30_000;

/// How many ids the allocator tries before giving up.
///
/// A bound rather than an open loop: a registry that somehow holds every candidate id must produce an
/// error, not spin until the turn is cancelled.
const ID_ATTEMPTS: u64 = 1024;

/// The source of generated ids.
///
/// Process-wide rather than per tool: ids only have to be unique within a registry, and a counter that
/// reset for every tool would mint the same id twice the moment two terminals are mounted, which is
/// exactly the collision the check in [`next_free_id`] would then have to absorb.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Builds the shells the terminal tools run.
///
/// The crate has no local [`Shell`] implementation, so a tool cannot construct one itself without
/// inventing a process layer that belongs to a deployment, not to this library. A deployment supplies
/// the factory; [`UnavailableLauncher`] is what a deployment without one mounts.
///
/// The returned future is `Send + 'static`, matching [`crate::terminal::Shell`]: the launch happens
/// inside a turn, which may move between threads.
pub trait TerminalLauncher: Send + Sync {
    /// Starts a shell for `spec`.
    ///
    /// The [`Launch`] carries the id, working directory, size, and transcript bound, so an
    /// implementation does not have to consult the registry to know what it is starting.
    ///
    /// Returns `Err` with a model-readable reason when no shell can be started. The reason must not
    /// repeat the id prefix: the tool wraps it as "cannot start terminal `id`", once.
    fn launch(&self, spec: Launch) -> BoxFut<Result<Arc<dyn Shell>, String>>;
}

/// A launcher that refuses every request, with a reason.
///
/// Mounting the terminal tools without a shell implementation must fail in one visible place. A tool
/// that quietly returned an id for a shell that was never started would leave the model reading an
/// empty transcript and concluding the command printed nothing.
#[derive(Clone, Debug)]
pub struct UnavailableLauncher {
    reason: String,
}

impl UnavailableLauncher {
    /// Refuses every launch with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Refuses because this build has no [`Shell`] implementation to start.
    ///
    /// The message names the missing piece rather than saying "unavailable": whoever reads it in a
    /// transcript needs to know that a wiring step was skipped, not that a device is busy.
    #[must_use]
    pub fn no_shell() -> Self {
        Self::new(
            "no `Shell` implementation is wired into this deployment, so no terminal can be started; \
             the host must supply a `TerminalLauncher`",
        )
    }
}

impl Default for UnavailableLauncher {
    fn default() -> Self {
        Self::no_shell()
    }
}

impl TerminalLauncher for UnavailableLauncher {
    fn launch(&self, _spec: Launch) -> BoxFut<Result<Arc<dyn Shell>, String>> {
        let reason = self.reason.clone();
        // An `async` block rather than a ready future because the trait returns `BoxFut`; the refusal
        // itself needs no await point.
        Box::pin(async move { Err(reason) })
    }
}

/// Builds the tool that starts a terminal and returns its id.
///
/// The id is the only handle the model has, so it is allocated before anything else can fail and is
/// reported in the receipt even when the model supplied one.
#[must_use]
pub fn terminal_start_tool(
    registry: Arc<TerminalRegistry>,
    launcher: Arc<dyn TerminalLauncher>,
) -> ToolDefinition {
    ToolDefinition::new(
        "terminal_start",
        "Starts a persistent terminal and returns its id. Unlike a one-shot command, the shell keeps \
         its working directory and environment between calls, so a second command sees what the \
         first did. Send input with `terminal_send` and read output with `terminal_read`.",
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Name for the terminal. Omit for a generated one."
                },
                "cwd": {
                    "type": "string",
                    "description": "Directory to start in; a relative path is resolved against the session workspace."
                },
                "shell": {
                    "type": "string",
                    "description": "Shell to run, such as `bash`. Omit for the host default."
                },
                "columns": {
                    "type": "integer",
                    "description": "Width in columns. Defaults to 80."
                },
                "rows": {
                    "type": "integer",
                    "description": "Height in rows. Defaults to 24."
                },
                "max_bytes": {
                    "type": "integer",
                    "description": "Transcript bytes to retain. Defaults to 256 KiB."
                }
            }
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            let launcher = Arc::clone(&launcher);
            Box::pin(async move { Ok(start(&registry, launcher.as_ref(), &args, &ctx).await) })
        }),
    )
}

/// Builds the tool that writes input to a terminal, or closes it.
#[must_use]
pub fn terminal_send_tool(registry: Arc<TerminalRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "terminal_send",
        "Types input into a running terminal, as if it had been typed at the prompt, and presses \
         Enter unless `submit` is false. Pass `close: true` to end the terminal instead; a closed \
         terminal's id is released. This call does not return output: use `terminal_read` after it.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The terminal's id." },
                "input": {
                    "type": "string",
                    "description": "Text to type. Required unless `close` is true."
                },
                "submit": {
                    "type": "boolean",
                    "description": "Press Enter after the input. Defaults to true."
                },
                "close": {
                    "type": "boolean",
                    "description": "Close the terminal instead of typing. Defaults to false."
                }
            },
            "required": ["id"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move { Ok(send(&registry, &args, &ctx).await) })
        }),
    )
}

/// Builds the tool that returns what a terminal printed.
#[must_use]
pub fn terminal_read_tool(registry: Arc<TerminalRegistry>) -> ToolDefinition {
    ToolDefinition::new(
        "terminal_read",
        "Returns what a terminal printed since the last read, waiting up to `wait_ms` for output. \
         An empty answer means nothing was printed in that window, not that the terminal is gone; \
         the status is reported. Pass `full: true` for the whole retained transcript.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The terminal's id." },
                "wait_ms": {
                    "type": "integer",
                    "description": "How long to wait for output, up to 30000. Defaults to 500."
                },
                "full": {
                    "type": "boolean",
                    "description": "Return the whole retained transcript instead of new output."
                }
            },
            "required": ["id"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move { Ok(read(&registry, &args, &ctx).await) })
        }),
    )
}

/// Registers all three terminal tools over one registry.
///
/// One call so a host cannot mount `terminal_send` without `terminal_read`: half a surface is worse
/// than none, because the model has then been told a terminal exists and cannot see what it printed.
pub fn register_terminal_tools(
    tools: &ToolRegistry,
    registry: Arc<TerminalRegistry>,
    launcher: Arc<dyn TerminalLauncher>,
) {
    tools.register(terminal_start_tool(Arc::clone(&registry), launcher));
    tools.register(terminal_send_tool(Arc::clone(&registry)));
    tools.register(terminal_read_tool(registry));
}

/// Starts a terminal and answers with its id.
async fn start(
    registry: &TerminalRegistry,
    launcher: &dyn TerminalLauncher,
    args: &Value,
    ctx: &ToolRunContext,
) -> ToolResult {
    // Starting a shell is a write-shaped act: the model gets a process it can type into. Refusing
    // before it exists is the only refusal that leaves nothing running.
    if read_only(ctx) {
        return ToolResult::error("this session is read-only, so a terminal may not start");
    }
    if ctx.cancel.is_cancelled() {
        return ToolResult::error("cancelled before the terminal started");
    }

    let spec = match launch_spec(registry, args, ctx.workspace.as_deref()) {
        Ok(spec) => spec,
        Err(message) => return ToolResult::error(message),
    };
    let id = spec.id.clone();
    let size = spec.size;
    let max_bytes = spec.max_bytes;

    let attempt = tokio::select! {
        () = ctx.cancel.cancelled() => {
            return ToolResult::error("cancelled while the terminal was starting");
        }
        result = launcher.launch(spec.clone()) => result,
    };
    let shell = match attempt {
        Ok(shell) => shell,
        // The launcher's reason is the deployment's own words; repeating them verbatim is the only
        // way the person who wired it can see what was refused.
        Err(reason) => return ToolResult::error(format!("cannot start terminal `{id}`: {reason}")),
    };

    let session = Arc::new(Session::new(id.clone(), shell, size, max_bytes));
    // `add` is the authority on collisions, but it drops the session on failure, which leaves a
    // process nobody can close. The id was already checked in `launch_spec`, so a failure here is a
    // race between two starts, and reporting it is all that is left.
    if registry.add(session).is_err() {
        return ToolResult::error(format!(
            "terminal `{id}` was started by another call; call `terminal_send` to close it, or use \
             the id it reports"
        ));
    }

    ToolResult::text(receipt(&id, &spec, size))
}

/// The text `terminal_start` answers with.
fn receipt(id: &str, spec: &Launch, size: Size) -> String {
    let mut out = format!("started terminal `{id}` at {}x{}", size.columns, size.rows);
    if let Some(shell) = &spec.shell {
        // `write!` into a `String` cannot fail; discarding the result keeps the lint quiet without an
        // unwrap path that could never be taken.
        let _ = write!(out, " running `{shell}`");
    }
    if let Some(cwd) = &spec.cwd {
        let _ = write!(out, " in `{}`", cwd.display());
    }
    out.push_str(
        "; type into it with `terminal_send` and read what it printed with `terminal_read`",
    );
    out
}

/// Parses `terminal_start`'s arguments into a launch request.
///
/// Split out from [`start`] so every validation failure is a plain `Err(String)` the tool turns into a
/// model-facing result, and so nothing is launched until the whole request is known to be buildable.
fn launch_spec(
    registry: &TerminalRegistry,
    args: &Value,
    workspace: Option<&Path>,
) -> Result<Launch, String> {
    let id = match optional_string(args, "id")? {
        Some(id) if !id.trim().is_empty() => {
            if registry.get(&id).is_some() {
                return Err(format!(
                    "terminal `{id}` already exists; close it with `terminal_send` and `close: true`, \
                     or omit `id` to get a fresh one"
                ));
            }
            id
        }
        Some(_) => {
            return Err("`id` is blank; a terminal needs a name it can be addressed by".into());
        }
        None => {
            next_free_id(registry).ok_or_else(|| "could not allocate a terminal id".to_owned())?
        }
    };

    // `Launch::new` rejects a blank id, which the branch above already ruled out; the error is kept
    // rather than unwrapped so a later change to either check cannot become a panic.
    let mut spec =
        Launch::new(id.clone()).ok_or_else(|| format!("`{id}` is not a usable terminal id"))?;

    if let Some(shell) = optional_string(args, "shell")? {
        spec = spec.with_shell(shell);
    }
    if let Some(cwd) = optional_string(args, "cwd")? {
        // A relative directory is resolved against the session workspace rather than the harness's,
        // because the model means "inside the project it is working in".
        let path = PathBuf::from(cwd);
        let path = match workspace {
            Some(root) if path.is_relative() => root.join(path),
            _ => path,
        };
        spec = spec.in_directory(path);
    }

    let columns = positive_u16(args, "columns")?;
    let rows = positive_u16(args, "rows")?;
    let headless = Size::headless();
    let size = match (columns, rows) {
        (None, None) => headless,
        (columns, rows) => Size::new(
            columns.unwrap_or(headless.columns),
            rows.unwrap_or(headless.rows),
        )
        .ok_or_else(|| "`columns` and `rows` must both be at least 1".to_owned())?,
    };
    spec = spec.with_size(size);

    if let Some(max_bytes) = args.get("max_bytes") {
        let Some(bytes) = max_bytes.as_u64() else {
            return Err("`max_bytes` must be a non-negative integer".into());
        };
        // A bound wider than the address space is a bound that means "unlimited", so clamping is a
        // faithful reading of the request rather than a silent truncation.
        spec = spec.with_max_bytes(usize::try_from(bytes).unwrap_or(usize::MAX));
    }

    Ok(spec)
}

/// Allocates an id no session in `registry` holds.
fn next_free_id(registry: &TerminalRegistry) -> Option<String> {
    for _ in 0..ID_ATTEMPTS {
        let candidate = format!("terminal-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        if registry.get(&candidate).is_none() {
            return Some(candidate);
        }
    }
    None
}

/// Writes to a terminal, or closes it.
async fn send(registry: &TerminalRegistry, args: &Value, ctx: &ToolRunContext) -> ToolResult {
    let id = match required_id(args) {
        Ok(id) => id,
        Err(message) => return ToolResult::error(message),
    };
    let Some(session) = registry.get(&id) else {
        return unknown(&id);
    };
    let closing = args.get("close").and_then(Value::as_bool).unwrap_or(false);

    if ctx.cancel.is_cancelled() {
        return ToolResult::error("cancelled before anything was sent");
    }

    let input = match optional_string(args, "input") {
        Ok(input) => input,
        Err(message) => return ToolResult::error(message),
    };
    if input.is_some() && read_only(ctx) {
        return ToolResult::error(
            "this session is read-only, so typed input would be a write; only `close: true` is allowed",
        );
    }

    if let Some(text) = input {
        let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(true);
        // A model that typed a command almost always means to run it. Pressing Enter is the default
        // so a forgotten newline does not look like a shell that ignored the instruction; a caller
        // that means to send a partial line says `submit: false`.
        let mut payload = text;
        if submit && !payload.ends_with('\n') {
            payload.push('\n');
        }
        if let Err(error) = session.send(&payload).await {
            return ToolResult::error(describe(&error));
        }
        if !closing {
            return ToolResult::text(format!(
                "sent {} bytes to terminal `{id}`; call `terminal_read` to see what it printed",
                payload.len()
            ));
        }
    }

    if !closing {
        return ToolResult::error(
            "`input` is required unless `close` is true; there is nothing to send",
        );
    }

    // Removal rather than `Session::close`: a closed session left in the registry would keep an id the
    // model cannot reuse and hold its transcript for as long as the process lives.
    match registry.remove(&id).await {
        Ok(()) => ToolResult::text(format!("closed terminal `{id}`")),
        // The session was present a moment ago, so a removal that finds nothing is a concurrent close;
        // saying so is better than reporting success for a call that did nothing.
        Err(error) => ToolResult::error(describe(&error)),
    }
}

/// Returns what a terminal printed, or the retained transcript.
async fn read(registry: &TerminalRegistry, args: &Value, ctx: &ToolRunContext) -> ToolResult {
    let id = match required_id(args) {
        Ok(id) => id,
        Err(message) => return ToolResult::error(message),
    };
    let Some(session) = registry.get(&id) else {
        return unknown(&id);
    };
    let wait = duration_arg(args, "wait_ms", DEFAULT_READ_WAIT_MS);

    // Checked before the wait rather than only inside the `select`: a call that arrives already
    // cancelled must not race a shell that happens to answer instantly, or the answer becomes
    // whichever future `select` polled first.
    if ctx.cancel.is_cancelled() {
        return ToolResult::error("cancelled while waiting for terminal output");
    }

    // A cancelled wait abandons the pending read. Nothing is lost by that: the transcript already holds
    // every chunk the shell delivered, and the next call reads from wherever this one stopped.
    let chunk = tokio::select! {
        () = ctx.cancel.cancelled() => {
            return ToolResult::error("cancelled while waiting for terminal output");
        }
        result = session.read(wait) => result,
    };
    let chunk = match chunk {
        Ok(chunk) => chunk,
        Err(error) => return ToolResult::error(describe(&error)),
    };

    let status = session.status();
    if args.get("full").and_then(Value::as_bool).unwrap_or(false) {
        return full_text(&session, &id, &status);
    }

    match chunk {
        // An empty chunk is the shell saying "nothing"; rendering it as output would look like a
        // successful command that printed a blank line.
        Some(chunk) if !chunk.is_empty() => ToolResult::text(chunk),
        _ => ToolResult::text(format!(
            "no output from terminal `{id}` within {} ms; it is {} ({} bytes retained). Call again \
             with a larger `wait_ms` if a command is still printing.",
            wait.as_millis(),
            status.as_str(),
            session.transcript().len()
        )),
    }
}

/// Renders the whole retained transcript, with what it dropped.
fn full_text(session: &Session, id: &str, status: &crate::terminal::Status) -> ToolResult {
    let transcript = session.transcript();
    let omitted = session.omitted();
    let mut out = format!(
        "terminal `{id}` is {}; {} bytes retained",
        status.as_str(),
        transcript.len()
    );
    if let Some(bytes) = omitted.count()
        && bytes > 0
    {
        let _ = write!(out, ", {bytes} older bytes dropped");
    }
    if transcript.is_empty() {
        out.push_str(" (nothing has been printed)");
        return ToolResult::text(out);
    }
    out.push('\n');
    out.push_str(&transcript);
    ToolResult::text(out)
}

/// The answer for an id no session holds.
///
/// The tool name is named in the message because the model's next move is a different tool call, and a
/// bare "unknown id" leaves it guessing which one.
fn unknown(id: &str) -> ToolResult {
    ToolResult::error(format!(
        "no terminal `{id}`; start one with `terminal_start`, or check `terminal_send`'s receipt for \
         the id it closed"
    ))
}

/// Renders a terminal failure for the model.
fn describe(error: &TerminalError) -> String {
    match error {
        TerminalError::Unknown { id } => format!(
            "no terminal `{id}`; it may have been closed by an earlier call, so start a new one with \
             `terminal_start`"
        ),
        other => other.to_string(),
    }
}

/// Whether the session may not write.
fn read_only(ctx: &ToolRunContext) -> bool {
    ctx.permissions
        .as_ref()
        .is_some_and(|permissions| permissions.knobs().sandbox == SandboxMode::ReadOnly)
}

/// Reads the required `id` argument.
fn required_id(args: &Value) -> Result<String, String> {
    match args.get("id").and_then(Value::as_str) {
        Some(id) if !id.trim().is_empty() => Ok(id.to_owned()),
        Some(_) => Err("`id` is blank".into()),
        None => Err("`id` is required".into()),
    }
}

/// Reads an optional string argument, rejecting a value of the wrong type.
fn optional_string(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

/// Reads an optional positive column or row count.
fn positive_u16(args: &Value, key: &str) -> Result<Option<u16>, String> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let Some(number) = value.as_u64() else {
        return Err(format!("`{key}` must be a positive integer"));
    };
    match u16::try_from(number) {
        Ok(number) if number > 0 => Ok(Some(number)),
        _ => Err(format!("`{key}` must be between 1 and {}", u16::MAX)),
    }
}

/// Reads a wait bound in milliseconds, clamped to [`MAX_WAIT_MS`].
fn duration_arg(args: &Value, key: &str, default_ms: u64) -> Duration {
    let millis = args
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(default_ms)
        .min(MAX_WAIT_MS);
    Duration::from_millis(millis)
}
