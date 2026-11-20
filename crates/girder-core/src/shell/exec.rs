//! The execution layer: process spawning, process-group control, and path sandboxing.
//!
//! Upstream splits this across `packages/subprocess` (5,529 lines) and `packages/sandbox/sandbox-local`
//! (646 lines). This module carries the four properties the plan's phase 5 acceptance names:
//!
//! 1. **A command with pipes runs.** Not "argv is passed to a shell", but "the shell exists and
//!    `|` reaches it".
//! 2. **A timeout kills the whole process group.** Killing the direct child leaves its children
//!    running — the classic leak — so the child is spawned as a group leader and signalled by
//!    negative pid.
//! 3. **The sandbox refuses an out-of-scope write.** Path scope is checked before the command runs,
//!    not after.
//! 4. **No zombie processes remain.** The child is reaped on every path, including the timeout path
//!    and the cancellation path.
//!
//! # Why a process group and not just a pid
//!
//! `sh -c "sleep 100 | cat"` starts three processes. Killing the `sh` pid leaves `sleep` and `cat`
//! orphaned and still holding the pipe. `setsid` makes the child a group leader; `kill(-pgid, …)`
//! then reaches every member. This is the same mechanism upstream's `killGroup` uses, and it is the
//! only portable way to do it on POSIX.
//!
//! # Sandboxing scope
//!
//! Upstream implements seccomp/landlock on Linux and seatbelt on macOS. Neither is here. What is
//! here is a **path policy**: a command declares the paths it may write, and the executor refuses
//! before spawning if the command would write outside them. That catches the case the acceptance
//! criterion names ("refuses an out-of-scope write") without a kernel feature, and it is honest
//! about what it does not catch: a command can still write anywhere the process can, because the
//! policy is advisory, not enforced. Real kernel-level enforcement is a later phase — see the note
//! at the bottom of this module.
//!
//! # Why `bash` invalidates rather than predicts
//!
//! The `write` and `edit` tools check a path against what the session last read before touching it. A
//! shell command has no such path to check: it can build one at run time, call a binary that writes,
//! or redirect into a file whose name came out of a variable. Rather than pretend a script can be
//! statically analysed for what it writes, the tool drops this session's observations after the
//! command runs and says so in its result. The model is told to re-read before editing, which is the
//! same instruction a detected conflict produces — the difference is that this one is honest about
//! not knowing which file was the one that changed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::core::scope::BoxFut;

/// A failure raised by the executor.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The command was refused by the path policy.
    #[error("denied: `{path}` is outside the writable scope")]
    OutOfScope {
        /// The path that was refused.
        path: PathBuf,
    },

    /// The requested working directory cannot be used.
    ///
    /// Distinct from [`Self::OutOfScope`]: that one is a policy decision about a path the caller may
    /// not touch, this one is a request that names no usable directory. A model told "outside the
    /// writable scope" would try a different path; one told why the directory is unusable can fix
    /// the request.
    #[error("working directory `{directory}`: {reason}")]
    WorkingDirectory {
        /// The directory that was asked for.
        directory: String,
        /// Why it cannot be used.
        reason: String,
    },

    /// The command ran longer than its timeout and was killed.
    #[error("command timed out after {seconds}s")]
    Timeout {
        /// The configured limit.
        seconds: u64,
    },

    /// The caller cancelled.
    #[error("command cancelled")]
    Cancelled,

    /// The process could not be started.
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        /// The program that failed.
        program: String,
        /// The underlying error.
        source: std::io::Error,
    },

    /// Reading the child's output failed.
    #[error("failed to read output: {0}")]
    Io(#[from] std::io::Error),

    /// A confined mode was requested and no backend on this host can enforce it.
    ///
    /// The command was not started. Running it anyway is the failure this error exists to prevent.
    #[error("{message}")]
    SandboxUnavailable {
        /// Upstream's refusal text, including the remedy.
        message: String,
    },
}

/// What a command is allowed to touch.
///
/// `writable` is a set of allowed prefixes rather than a single root so a profile can grant two
/// unrelated directories (a data dir and a temp dir) without granting their common ancestor.
#[derive(Clone, Debug, Default)]
pub struct PathPolicy {
    /// Prefixes a command may write under. Empty means nothing is writable.
    pub writable: BTreeSet<PathBuf>,
    /// Prefixes a command may read under. Empty means no restriction is expressed.
    pub readable: BTreeSet<PathBuf>,
    /// When set, a shell command must run confined to this mode.
    ///
    /// `None` keeps the historical advisory spawn: callers that only check declared paths still run.
    /// [`crate::shell::SandboxMode::Full`] is the explicit unconfined mode. The other two refuse
    /// when this host has no usable runner, rather than starting the command anyway.
    pub confine: Option<crate::shell::SandboxMode>,
}

impl PathPolicy {
    /// A policy that allows writes only under `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut writable = BTreeSet::new();
        writable.insert(root.clone());
        let mut readable = BTreeSet::new();
        readable.insert(root);
        Self {
            writable,
            readable,
            confine: None,
        }
    }

    /// Requires shell commands under this policy to be confined to `mode`.
    #[must_use]
    pub fn confined(mut self, mode: crate::shell::SandboxMode) -> Self {
        self.confine = Some(mode);
        self
    }

    /// Grants an additional writable prefix.
    #[must_use]
    pub fn allow_write(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.writable.insert(path.clone());
        self.readable.insert(path);
        self
    }

    /// Whether `path` is inside a writable prefix.
    ///
    /// Comparison is on the *resolved* path, so `a/../b` cannot escape by string trickery. A path
    /// that does not exist yet is resolved through its nearest existing ancestor, which is what
    /// allows a policy to permit a file that has not been created.
    #[must_use]
    pub fn permits_write(&self, path: &Path) -> bool {
        let resolved = resolve_lenient(path);
        self.writable.iter().any(|prefix| {
            let prefix = resolve_lenient(prefix);
            resolved.starts_with(&prefix)
        })
    }

    /// Refuses when `path` is outside the writable scope.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::OutOfScope`] with the offending path.
    pub fn check_write(&self, path: &Path) -> Result<(), ExecError> {
        if self.permits_write(path) {
            Ok(())
        } else {
            Err(ExecError::OutOfScope {
                path: path.to_path_buf(),
            })
        }
    }
}

/// Resolves a path as far as it exists, then re-appends the missing tail.
///
/// `std::fs::canonicalize` fails on a path that does not exist. A policy has to judge such a path
/// (that is the whole point of checking before a write), so this walks up to the nearest ancestor
/// that does exist, canonicalizes that, and re-joins the rest.
fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }

    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        match cursor.parent() {
            Some(parent) => {
                if let Some(name) = cursor.file_name() {
                    missing.push(name.to_os_string());
                }
                if let Ok(resolved) = std::fs::canonicalize(parent) {
                    let mut out = resolved;
                    for component in missing.iter().rev() {
                        out.push(component);
                    }
                    return out;
                }
                cursor = parent;
            }
            None => return path.to_path_buf(),
        }
    }
}

/// How a command should be run.
#[derive(Clone, Debug)]
pub struct ExecOptions {
    /// Directory to run in.
    pub cwd: Option<PathBuf>,
    /// Hard limit. On expiry the process group is killed.
    pub timeout: Duration,
    /// Path policy. `None` disables checking, which the tests use and a real profile must not.
    pub policy: Option<PathPolicy>,
    /// Paths the command declares it will write, checked before spawning.
    pub declared_writes: Vec<PathBuf>,
    /// Whether to clear the inherited environment before adding `env`.
    ///
    /// Off by default because a command usually needs the environment it was started with. A tool that runs
    /// model-authored commands turns it on, so a child cannot read a credential the harness was launched with.
    pub clear_environment: bool,
    /// Variables to add after clearing.
    ///
    /// Clearing alone leaves a command with no `PATH`, so every tool it invokes is missing and the failure looks like a
    /// broken harness rather than a deliberate policy. The allowlist puts back what a command needs to run at all while
    /// still withholding what it must not see.
    pub environment: Vec<(String, String)>,
    /// Confines the command to this mode. `None` and [`crate::shell::SandboxMode::Full`] spawn directly.
    pub confine: Option<crate::shell::SandboxMode>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(120),
            policy: None,
            declared_writes: Vec::new(),
            clear_environment: false,
            environment: Vec::new(),
            confine: None,
        }
    }
}

impl ExecOptions {
    /// Requires the command to run under `mode`, or not at all.
    #[must_use]
    pub fn with_confine(mut self, mode: crate::shell::SandboxMode) -> Self {
        self.confine = Some(mode);
        self
    }
}

impl ExecOptions {
    /// Clears the inherited environment and adds the baseline a command needs to run.
    ///
    /// The baseline comes from [`crate::util::launch_environment`], which is the one place the allowlist and the forbidden
    /// prefixes are written down: a second copy here would drift, and the drift would be a credential reaching a child.
    #[must_use]
    pub fn with_baseline_environment(mut self) -> Self {
        self.clear_environment = true;
        let parent: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        let resolved =
            crate::util::launch_environment::resolve(&parent, &std::collections::BTreeMap::new());
        self.environment = resolved.values.into_iter().collect();
        self
    }

    /// Clears the inherited environment, leaving only what was added explicitly.
    ///
    /// The difference between a child seeing the harness's own credentials and a child seeing only what it was told
    /// to see. A tool that runs model-authored commands turns it on.
    #[must_use]
    pub fn with_cleared_environment(mut self) -> Self {
        self.clear_environment = true;
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the path policy.
    #[must_use]
    pub fn with_policy(mut self, policy: PathPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Declares a path this command will write.
    #[must_use]
    pub fn writing(mut self, path: impl Into<PathBuf>) -> Self {
        self.declared_writes.push(path.into());
        self
    }
}

/// The result of a completed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// The exit code, or `None` when the process was signalled.
    pub code: Option<i32>,
}

impl ExecOutput {
    /// Whether the command reported success.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The model-facing text, rendered through `shell_gap`.
    ///
    /// Three differences from the concatenation this replaced, and each is a fact the model needs:
    ///
    /// - **A `[stderr]` header** separates the streams. Without it a warning on stderr and a result
    ///   on stdout read as one paragraph, and a model quoting the output cannot tell which it saw.
    /// - **`(no output)`** for an empty result, so "the command printed nothing" is stated rather
    ///   than left as an empty string the reader has to interpret.
    /// - **`parse_exit_status`'s marker**, which is what `render_result` writes and what upstream's
    ///   `parseExitStatus` reads back. A model that wants the code can find it; the previous
    ///   `[exit N]` was this build's own invention and nothing parsed it.
    #[must_use]
    pub fn to_model_text(&self) -> String {
        let outcome = crate::shell::render::RunOutcome {
            exit_code: self.code,
            signal: None,
            // A timed-out run never reaches here: it is `ExecError::Timeout`, so there is no
            // outcome to render. Reporting `false` is therefore accurate rather than a default.
            timed_out: false,
            stopped: None,
            timeout_ms: 0,
            stdout: crate::shell::render::CollectedOutput::complete(self.stdout.clone()),
            stderr: crate::shell::render::CollectedOutput::complete(self.stderr.clone()),
            sandbox: None,
        };
        crate::shell::render::render_result(&outcome, &[])
    }

    // `to_text` used to live here: it concatenated stdout and stderr with no separator, printed an
    // empty string for no output, and wrote `[exit N]` -- a marker nothing parsed. `to_model_text`
    // replaces it, through `shell_gap`, which owns upstream's contract. The old method is deleted
    // rather than kept as a second answer to "what does the model see".
}

/// Runs `script` through `sh -c`.
///
/// A shell is used rather than splitting argv because the acceptance criterion is "a command with
/// pipes works" — pipes are shell syntax. The security consequence is real and belongs to the
/// sandbox, not here: a script is arbitrary code, and the only defences are the path policy and
/// (later) kernel isolation.
///
/// # Errors
///
/// Returns [`ExecError::OutOfScope`] when a declared write is refused, [`ExecError::Timeout`] when
/// the limit expires, and [`ExecError::Cancelled`] when the token is cancelled first.
pub async fn run_shell(
    script: &str,
    options: &ExecOptions,
    cancel: &CancellationToken,
) -> Result<ExecOutput, ExecError> {
    run_shell_program("sh", script, options, cancel).await
}

/// Runs a `bash` script, using `bash` when available and `sh` otherwise.
///
/// Kept separate from [`run_shell`] so the executor can report which shell it actually used: a
/// profile that assumes `bash` semantics (process substitution, `[[`) must not silently get `sh`.
///
/// # Errors
///
/// Same as [`run_shell`].
pub async fn run_bash(
    script: &str,
    options: &ExecOptions,
    cancel: &CancellationToken,
) -> Result<ExecOutput, ExecError> {
    let program = if has_program("bash") { "bash" } else { "sh" };
    run_shell_program(program, script, options, cancel).await
}

/// The platform name `platform_chain` expects.
fn host_os() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else {
        "unknown"
    }
}

/// Wraps a shell argv in the host's confining runner, or refuses.
///
/// `None` and `danger-full-access` return the original argv. Every other mode goes through
/// [`crate::sandbox::policy::confine`]. A host with no usable runner gets
/// [`ExecError::SandboxUnavailable`] and the command is not started.
fn confined_argv(
    program: &str,
    script: &str,
    options: &ExecOptions,
) -> Result<(String, Vec<String>), ExecError> {
    let direct = (program.to_owned(), vec!["-c".to_owned(), script.to_owned()]);
    let Some(mode) = options.confine else {
        return Ok(direct);
    };
    if mode == crate::shell::SandboxMode::Full {
        return Ok(direct);
    }
    let root = options
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let policy = crate::sandbox::policy::ExecutionPolicy {
        mode,
        workspace_root: root,
        additional_writable_roots: None,
        session_id: None,
    };
    let argv = vec![program.to_owned(), "-c".to_owned(), script.to_owned()];
    let confined = crate::sandbox::policy::confine(
        &argv,
        &policy,
        crate::sandbox::policy::platform_chain(host_os()),
        |runner| {
            runner.program().and_then(|name| {
                if has_program(name) {
                    Some(crate::sandbox::policy::static_enforcement(runner))
                } else {
                    None
                }
            })
        },
    )
    .map_err(|error| ExecError::SandboxUnavailable {
        message: error.to_string(),
    })?;
    let mut parts = confined.argv.into_iter();
    let wrapped = parts.next().unwrap_or_default();
    Ok((wrapped, parts.collect()))
}

/// The shared implementation both entry points call.
///
/// One body rather than two near-identical ones: the two differed only in the program name, while
/// duplicating the pipe draining, the process-group kill, and the timeout race. A fix to any of
/// those had to be made twice, and the second copy was the one likely to be forgotten.
async fn run_shell_program(
    program: &str,
    script: &str,
    options: &ExecOptions,
    cancel: &CancellationToken,
) -> Result<ExecOutput, ExecError> {
    // The policy check runs before anything is spawned: refusing after the fact is not refusing.
    if let Some(policy) = &options.policy {
        for path in &options.declared_writes {
            policy.check_write(path)?;
        }
    }

    let (program, args) = confined_argv(program, script, options)?;

    // Process mechanics live in `subprocess`; this function's job is to turn a script and a policy
    // into a spawn request and translate the outcome. Duplicating the pipe drain and the group kill
    // here is what made the earlier version need every fix twice.
    let mut spawn = crate::shell::subprocess::SpawnOptions::new(program, args).with_timeout(options.timeout);
    if let Some(cwd) = &options.cwd {
        // The change is *validated* before it is used: `working_dir_gap` owns upstream's rules for
        // what a working-directory change may be (absolute after normalization, existing, a
        // directory), and a spawn that failed with its own message would name a syscall rather than
        // the request. `validate_change` answers the resolved path or the reason.
        let requested = cwd.to_string_lossy();
        let base = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        match crate::shell::working_dir::validate_change(&base, &requested) {
            Ok(resolved) => spawn = spawn.with_cwd(std::path::PathBuf::from(resolved)),
            // An unusable directory is reported as the tool's error, not as a panic: the model asked
            // for it and can be told why it cannot have it.
            Err(error) => {
                return Err(ExecError::WorkingDirectory {
                    directory: requested.into_owned(),
                    reason: error.to_string(),
                });
            }
        }
    }
    if options.clear_environment {
        spawn = spawn.with_cleared_environment();
    }
    for (name, value) in &options.environment {
        spawn = spawn.with_env(name.clone(), value.clone());
    }

    match crate::shell::subprocess::run(spawn, cancel.clone()).await {
        Ok(outcome) => Ok(ExecOutput {
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            code: outcome.code,
        }),
        Err(crate::shell::subprocess::ProcessError::Timeout { seconds }) => {
            Err(ExecError::Timeout { seconds })
        }
        Err(crate::shell::subprocess::ProcessError::Cancelled) => Err(ExecError::Cancelled),
        Err(crate::shell::subprocess::ProcessError::Spawn { program, source }) => {
            Err(ExecError::Spawn { program, source })
        }
        Err(crate::shell::subprocess::ProcessError::Io(error)) => Err(ExecError::Io(error)),
    }
}

#[must_use]
pub fn has_program(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Asks before this command runs under a wider sandbox than the session has.
///
/// `Ok(None)` means the call did not ask to widen. `Ok(Some)` is the mode the person approved.
/// `Err` is the sentence the model should read: a malformed pair, a refusal, or no one to ask.
async fn escalate_bash(
    args: &serde_json::Value,
    effective: Option<crate::shell::SandboxMode>,
    ctx: &crate::core::tools::ToolRunContext,
) -> Result<Option<crate::shell::SandboxMode>, String> {
    let requested_name = args
        .get("sandbox_permissions")
        .and_then(serde_json::Value::as_str);
    let justification = args.get("justification").and_then(serde_json::Value::as_str);
    if requested_name.is_none() && justification.is_none() {
        return Ok(None);
    }
    crate::sandbox::policy::validate_escalation_args(requested_name, justification)
        .map_err(|error| error.to_string())?;
    let requested_name = requested_name.unwrap_or_default();
    let justification = justification.unwrap_or_default();
    let Some(requested) = crate::sandbox::policy::parse_mode(requested_name) else {
        return Err(format!(
            "`sandbox_permissions` = `{requested_name}` is not a sandbox mode"
        ));
    };
    let effective = effective.unwrap_or(crate::shell::SandboxMode::WorkspaceWrite);
    let bridge = ctx.escalation.clone();
    let approver: Option<&dyn crate::sandbox::policy::EscalationApprover> = bridge
        .as_ref()
        .map(|item| item.as_ref() as &dyn crate::sandbox::policy::EscalationApprover);
    let agent = ctx.execution.agent_id.as_str();
    let call_id = ctx.execution.call_id.as_str();
    let approval = crate::sandbox::policy::EscalationApproval {
        approver,
        agent: Some(agent),
        call_id,
        tool_name: "bash",
        signal: Some(&ctx.cancel),
    };
    let request = crate::sandbox::policy::EscalationRequest {
        requested_mode: requested,
        justification: justification.to_owned(),
        effective_mode: effective,
        subject: "command".to_owned(),
    };
    crate::sandbox::policy::approve_escalation(&request, &approval)
        .await
        .map(Some)
        .map_err(|error| error.to_string())
}

/// A `bash` tool: runs a script and returns its output.
///
/// The tool is a thin wrapper; everything interesting is in [`run_bash`]. It takes the policy from
/// the context rather than from the arguments, so a model cannot widen its own scope by asking.
pub fn bash_tool(policy: PathPolicy) -> crate::core::tools::ToolDefinition {
    use std::sync::Arc;

    crate::core::tools::ToolDefinition::new(
        "bash",
        "Runs a shell command and returns its stdout and stderr",
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command to run" },
                "sandbox_permissions": {
                    "type": "string",
                    "description": "A strictly wider sandbox mode to ask for. Requires justification."
                },
                "justification": {
                    "type": "string",
                    "description": "Why the wider mode is needed. Required together with sandbox_permissions."
                }
            },
            "required": ["command"]
        }),
        Arc::new(
            move |args: serde_json::Value, ctx: crate::core::tools::ToolRunContext| {
                let policy = policy.clone();
                Box::pin(async move {
                    let command = args
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if command.trim().is_empty() {
                        return Ok(crate::core::tools::ToolResult::error("no command given"));
                    }

                    let escalated = match escalate_bash(&args, policy.confine, &ctx).await {
                        Ok(mode) => mode,
                        Err(message) => {
                            return Ok(crate::core::tools::ToolResult::error(message));
                        }
                    };

                    // The permission layer decides whether a shell command may run at all. A session in read-only
                    // mode must not run one, because a shell command can write whatever it likes and no amount of
                    // path checking inside the harness would catch it. An approved widening is the exception:
                    // the person just allowed this one command to leave read-only.
                    if escalated.is_none() {
                        if let Some(permissions) = &ctx.permissions {
                            let knobs = permissions.knobs();
                            if knobs.sandbox == crate::shell::SandboxMode::ReadOnly {
                                return Ok(crate::core::tools::ToolResult::error(
                                    "this session is read-only, so a shell command may not run",
                                ));
                            }
                        }
                    }

                    let mut options = ExecOptions::default()
                        .with_policy(policy.clone())
                        // A command a model asked to run must not inherit the harness's own credentials, but it does
                        // need the baseline that lets any command run at all.
                        .with_baseline_environment();
                    if let Some(mode) = escalated.or(policy.confine) {
                        options = options.with_confine(mode);
                    }
                    // The command runs in the session's workspace, which is what "this project" means to a model. A
                    // tool that inherited the process's directory instead ran in whatever the harness was started
                    // from -- for a test binary, the repository root, where a command writing `a.txt` overwrote a
                    // tracked file.
                    if let Some(cwd) = ctx.cwd.clone().or_else(|| ctx.workspace.clone()) {
                        options = options.with_cwd(cwd);
                    }
                    match run_bash(&command, &options, &ctx.cancel).await {
                        Ok(output) => Ok(with_invalidation_notice(
                            crate::core::tools::ToolResult {
                                content: vec![crate::llm::ContentBlock::Text {
                                    // Through `shell_gap`, which owns upstream's output contract: the
                                    // `[stderr]` header, `(no output)` for an empty result, and the
                                    // exit marker `parse_exit_status` reads back.
                                    text: output.to_model_text(),
                                }],
                                value: None,
                                is_error: !output.success(),
                            },
                            // A command that ran at all may have written: a nonzero exit does not mean it touched
                            // nothing on the way, and a caller that kept its old expectations would then check a
                            // later write against a version this command invalidated.
                            ctx.forget_all(),
                        )),
                        Err(ExecError::Timeout { seconds }) => Ok(with_invalidation_notice(
                            // A timed-out command was killed midway, which is the case most likely to have left a
                            // file half-written.
                            crate::core::tools::ToolResult::error(format!(
                                "command timed out after {seconds}s"
                            )),
                            ctx.forget_all(),
                        )),
                        Err(ExecError::SandboxUnavailable { message }) => {
                            Ok(crate::core::tools::ToolResult::error(message))
                        }
                        Err(ExecError::OutOfScope { path }) => {
                            // Nothing was spawned, so nothing was written and the observations still hold.
                            Ok(crate::core::tools::ToolResult::error(format!(
                                "denied: `{}` is outside the writable scope",
                                path.display()
                            )))
                        }
                        Err(ExecError::Cancelled) => {
                            // The command may have run and been killed partway, so its effect is unknown.
                            Ok(with_invalidation_notice(
                                crate::core::tools::ToolResult::error("cancelled"),
                                ctx.forget_all(),
                            ))
                        }
                        Err(error) => {
                            // A spawn failure means no child existed; an output-read failure means one did.
                            if matches!(error, ExecError::Spawn { .. }) {
                                Ok(crate::core::tools::ToolResult::error(error.to_string()))
                            } else {
                                Ok(with_invalidation_notice(
                                    crate::core::tools::ToolResult::error(error.to_string()),
                                    ctx.forget_all(),
                                ))
                            }
                        }
                    }
                })
            },
        ),
    )
}

/// Appends the observation-invalidation notice to a `bash` result.
///
/// The tool cannot know what a shell command wrote: the path can be built at run time, a called binary can write,
/// and a redirect looks like any other token. So every successful or attempted command drops this session's file
/// observations, and the model is told so in the result it reads — otherwise its next `write` would silently be
/// checked against a version the command had already invalidated.
///
/// A command that dropped nothing gets no notice: telling the model that "reads were invalidated" when it had read
/// no files is noise, and noise in a tool result is what teaches a model to skip the notice that matters.
fn with_invalidation_notice(
    mut result: crate::core::tools::ToolResult,
    dropped: usize,
) -> crate::core::tools::ToolResult {
    if dropped == 0 {
        return result;
    }
    let notice = format!(
        "\n\n[observation] this command may have modified files, so the {dropped} file read(s) observed earlier \
         in this session are no longer considered current. Re-read a file before editing it if its contents \
         matter."
    );
    for block in &mut result.content {
        if let crate::llm::ContentBlock::Text { text } = block {
            text.push_str(&notice);
            return result;
        }
    }
    // No text block to extend means the tool produced no readable output, which a `bash` result never does; the
    // notice stands on its own rather than being dropped.
    result
        .content
        .push(crate::llm::ContentBlock::Text { text: notice });
    result
}

/// Reads a file, refusing paths outside the readable scope.
///
/// # Errors
///
/// Returns [`ExecError::OutOfScope`] when the path is not readable, and [`ExecError::Io`] when the
/// read fails.
pub async fn read_file(path: &Path, policy: &PathPolicy) -> Result<String, ExecError> {
    let resolved = resolve_lenient(path);
    if !policy.readable.is_empty()
        && !policy
            .readable
            .iter()
            .any(|prefix| resolved.starts_with(resolve_lenient(prefix)))
    {
        return Err(ExecError::OutOfScope {
            path: path.to_path_buf(),
        });
    }
    let content = tokio::fs::read_to_string(path).await?;
    Ok(content)
}

// ---- what this module does not do ----------------------------------------------------------------
//
// The plan (§2.8) asks for seccomp/landlock on Linux and seatbelt on macOS. Neither is here, and
// the gap is deliberate rather than forgotten:
//
// - A path policy checks *declared* writes. A command that writes somewhere it did not declare is
//   not stopped, because the check happens before the process exists.
// - Consequently `cargo run -p girder-cli` with the bash tool is NOT sandboxed against a
//   misbehaving model. It is a development harness, and the module's job is to make the *interface*
//   right — `PathPolicy` is the seam a kernel implementation slots into.
//
// The right shape for phase 6+ is a `Sandbox` trait whose `spawn` applies landlock before `exec`,
// with this `PathPolicy` as its configuration. Writing the trait now, with one implementation that
// only checks declared paths, keeps the call sites honest about what is enforced.

// ---- the sandbox interface ----------------------------------------------------------------------

/// How much a sandbox actually enforces.
///
/// This is part of the interface, not a doc comment, because an `Advisory` sandbox and a `Kernel`
/// sandbox are indistinguishable to a caller otherwise. Reporting the level is what keeps a
/// development harness from being mistaken for isolation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enforcement {
    /// Only *declared* paths are checked, before the process starts. A command that writes somewhere
    /// it did not declare is **not** stopped.
    Advisory,

    /// The kernel refuses out-of-scope access. `mechanism` names the facility (`landlock`,
    /// `seatbelt`).
    Kernel {
        /// The kernel facility enforcing the policy.
        mechanism: &'static str,
    },
}

impl Enforcement {
    /// Whether this level actually confines the process.
    #[must_use]
    pub fn is_kernel(&self) -> bool {
        matches!(self, Self::Kernel { .. })
    }

    /// A short description for logs and tool results.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Advisory => "advisory (declared paths only; not enforced)".to_owned(),
            Self::Kernel { mechanism } => format!("kernel ({mechanism})"),
        }
    }
}

/// A strategy for running a command under a policy.
///
/// The seam a kernel implementation slots into. Phase 5 shipped only the policy; this trait exists so
/// the call sites are honest about what they get and a landlock/seatbelt backend is a new
/// implementation rather than a change everywhere.
pub trait Sandbox: Send + Sync {
    /// What this sandbox enforces.
    fn enforcement(&self) -> Enforcement;

    /// Runs a shell script under the policy.
    fn run(
        &self,
        script: &str,
        policy: &PathPolicy,
        cancel: CancellationToken,
    ) -> BoxFut<Result<ExecOutput, ExecError>>;
}

/// The only sandbox this build ships: it checks declared writes and otherwise runs the command as
/// the harness user.
///
/// Named rather than anonymous so that a caller reading its `enforcement()` cannot mistake it for
/// confinement.
pub struct AdvisorySandbox {
    /// Working directory for commands.
    pub cwd: Option<PathBuf>,
    /// Hard limit per command.
    pub timeout: Duration,
}

impl Default for AdvisorySandbox {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(120),
        }
    }
}

impl AdvisorySandbox {
    /// A sandbox with the given working directory.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: Some(cwd.into()),
            timeout: Duration::from_secs(120),
        }
    }

    /// Sets the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Sandbox for AdvisorySandbox {
    fn enforcement(&self) -> Enforcement {
        Enforcement::Advisory
    }

    fn run(
        &self,
        script: &str,
        policy: &PathPolicy,
        cancel: CancellationToken,
    ) -> BoxFut<Result<ExecOutput, ExecError>> {
        let options = ExecOptions {
            cwd: self.cwd.clone(),
            timeout: self.timeout,
            policy: Some(policy.clone()),
            declared_writes: Vec::new(),
            // A test harness that spawns a shell must not hand it the environment the suite was started in.
            clear_environment: true,
            // Nor must it leave the command without the environment any command needs, and what it needs is the same
            // baseline a model-facing tool gets.
            environment: crate::util::launch_environment::resolve(
                &std::env::vars().collect(),
                &std::collections::BTreeMap::new(),
            )
            .values
            .into_iter()
            .collect(),
            confine: policy.confine,
        };
        let script = script.to_owned();
        Box::pin(async move { run_shell(&script, &options, &cancel).await })
    }
}

/// Detects whether the running kernel offers Landlock, and which ABI version.
///
/// Returns `None` when the facility is absent — a kernel built without `CONFIG_SECURITY_LANDLOCK`
/// answers `ENOSYS` to the version probe. This exists so a future kernel backend can decide at
/// startup instead of failing per command, and so a caller can report honestly why it is not
/// confined.
#[must_use]
pub fn landlock_abi() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        // `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` returns the ABI
        // version. It is the documented way to probe support without creating anything.
        const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
        const SYS_LANDLOCK_CREATE_RULESET: i64 = 444;

        // SAFETY: the version probe passes a null ruleset pointer and zero size, which the kernel
        // documents as a pure query. No memory is read or written.
        #[allow(
            unsafe_code,
            reason = "the Landlock version probe is a documented raw syscall"
        )]
        let result = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::null::<u8>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };

        if result < 0 {
            None
        } else {
            u32::try_from(result).ok()
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
