//! The subprocess seam: spawning, streaming, and guaranteed reaping.
//!
//! Upstream's `subprocess` is 6.7k lines because it supports several providers (local, ssh, sandbox)
//! and a control channel. The part that matters for correctness is smaller, and it is what this
//! module carries: how a child is spawned so its whole tree can be killed, how its output is drained
//! without deadlocking, and how it is reaped on every path including the failure paths.
//!
//! # The three failure modes this exists to prevent
//!
//! 1. **Orphaned grandchildren.** `sh -c "sleep 100 | cat"` starts three processes. Killing the `sh`
//!    pid leaves the other two holding the pipe. The child is therefore a process-group leader, and
//!    a kill goes to `-pgid`.
//! 2. **Pipe deadlock.** A pipe whose reader is not pumped fills at 64 KiB and blocks the writer. A
//!    naive wait-then-read hangs forever on a command with enough output.
//! 3. **Zombies.** A child that exits but is not waited for stays as a zombie until the parent does.
//!    Every path here waits, including timeout and cancellation.
//!
//! # Streaming versus collecting
//!
//! Upstream exposes raw `Readable` streams plus a collected view. This build exposes the collected
//! form only, because the only consumer is a tool that shows output after the command finishes.
//! A streaming consumer is a real need — a long `watch` command, an interactive shell — and the seam
//! is shaped so it can be added without changing the spawn path: the drain already happens
//! concurrently, and [`ProcessOutcome`] is what a stream would feed into.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::scope::BoxFut;

/// A failure raised while running a process.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// The program could not be started.
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        /// The program that failed.
        program: String,
        /// The underlying error.
        source: std::io::Error,
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

    /// Reading the child's output failed.
    #[error("failed to read output: {0}")]
    Io(#[from] std::io::Error),
}

/// How to spawn one process.
#[derive(Clone, Debug)]
pub struct SpawnOptions {
    /// Whether to clear the inherited environment before adding `env`.
    ///
    /// Off by default: a command usually needs the environment it was started with. A tool that runs
    /// model-authored commands turns it on, because a child must not inherit a credential the harness was
    /// launched with.
    pub clear_environment: bool,
    /// Program to run.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: Option<std::path::PathBuf>,
    /// Hard limit; on expiry the process group is killed.
    pub timeout: Duration,
    /// Environment variables to add, on top of the inherited set.
    pub env: Vec<(String, String)>,
    /// Whether the child becomes its own process-group leader.
    ///
    /// True for anything a timeout may need to kill. False only when the child must share the
    /// harness's group — which nothing here does, and which makes the group signal dangerous.
    pub new_process_group: bool,
}

impl SpawnOptions {
    /// Spawns `program` with `args` and the default timeout.
    #[must_use]
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            timeout: Duration::from_secs(120),
            env: Vec::new(),
            new_process_group: true,
            clear_environment: false,
        }
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Adds an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Clears the inherited environment, leaving only what was added explicitly.
    ///
    /// Off by default because a command usually needs the environment it was started with. On, it is the
    /// difference between a child seeing the harness's own credentials and a child seeing only what it was told
    /// to see: a command a model asked to run must not inherit a token the harness was launched with.
    #[must_use]
    pub fn with_cleared_environment(mut self) -> Self {
        self.clear_environment = true;
        self
    }
}

/// What a finished process produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessOutcome {
    /// Captured stdout, decoded lossily.
    pub stdout: String,
    /// Captured stderr, decoded lossily.
    pub stderr: String,
    /// The exit code, or `None` when the process was signalled.
    pub code: Option<i32>,
}

impl ProcessOutcome {
    /// Whether the process reported success.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Renders both streams for a tool result.
    ///
    /// stderr is appended rather than discarded: a command that fails usually says why there, and
    /// dropping it leaves the model with an exit code and no reason.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if !self.stdout.is_empty() {
            out.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&self.stderr);
        }
        if let Some(code) = self.code.filter(|code| *code != 0) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("[exit {code}]"));
        }
        out
    }
}

/// Runs one process to completion.
///
/// # Errors
///
/// Returns [`ProcessError::Timeout`] when the limit expires, [`ProcessError::Cancelled`] when the
/// token fires first, and [`ProcessError::Spawn`] when the program cannot start.
pub async fn run(
    options: SpawnOptions,
    cancel: CancellationToken,
) -> Result<ProcessOutcome, ProcessError> {
    let mut command = Command::new(&options.program);
    command
        .args(&options.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A child dropped without being waited for must not outlive its caller.
        .kill_on_drop(true);

    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }
    if options.clear_environment {
        // Before adding our own: `env_clear` after would drop them too.
        command.env_clear();
    }
    if options.clear_environment {
        command.env_clear();
    }
    for (key, value) in &options.env {
        command.env(key, value);
    }

    if options.new_process_group {
        #[cfg(unix)]
        {
            // `setpgid(0, 0)` makes the child a group leader whose id is its own pid. Without it the
            // group id would be the harness's, and signalling it would kill the harness.
            //
            // SAFETY: `pre_exec` runs in the child after fork and before exec, where only
            // async-signal-safe calls are allowed. `setpgid` is one, and it allocates nothing.
            #[allow(
                unsafe_code,
                reason = "pre_exec is the only way to set a process group before exec"
            )]
            unsafe {
                command.pre_exec(|| {
                    nix::unistd::setpgid(
                        nix::unistd::Pid::from_raw(0),
                        nix::unistd::Pid::from_raw(0),
                    )
                    .map_err(std::io::Error::from)?;
                    Ok(())
                });
            }
        }
    }

    let program = options.program.clone();
    let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
        program: program.clone(),
        source,
    })?;
    let pid = child.id();

    let mut stdout = child
        .stdout
        .take()
        .expect("stdout is piped by this function; a change to that must update this line");
    let mut stderr = child
        .stderr
        .take()
        .expect("stderr is piped by this function; a change to that must update this line");

    // Drain both pipes while waiting. A pipe whose reader is not pumped fills at 64 KiB and blocks
    // the writer, so waiting first would deadlock on a command with enough output.
    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer).await;
        buffer
    });
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer).await;
        buffer
    });

    let deadline = tokio::time::sleep(options.timeout);
    tokio::pin!(deadline);

    let status = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            kill_tree(pid);
            reap(child).await;
            abort_all(stdout_task, stderr_task).await;
            return Err(ProcessError::Cancelled);
        }
        () = &mut deadline => {
            kill_tree(pid);
            reap(child).await;
            abort_all(stdout_task, stderr_task).await;
            return Err(ProcessError::Timeout {
                seconds: options.timeout.as_secs(),
            });
        }
        status = child.wait() => status?,
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    Ok(ProcessOutcome {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code: status.code(),
    })
}

/// Kills the process group led by `pid`.
///
/// SIGTERM first so a well-behaved process can clean up, then SIGKILL. The escalation is not
/// optional: a shell that ignores SIGTERM would otherwise survive the timeout. Delivery races are
/// ignored — a kill that finds nothing has already achieved what it wanted.
fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        // A pid is positive and fits in i32 on every platform with POSIX.
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        let group = Pid::from_raw(-pid);
        let _ = kill(group, Signal::SIGTERM);
        std::thread::sleep(Duration::from_millis(50));
        let _ = kill(group, Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        // Windows needs `taskkill /T /F` for a tree; not implemented because the execution layer's
        // acceptance criteria are POSIX-only.
        let _ = pid;
    }
}

/// Waits for the child to be reaped, bounded so a stuck wait cannot hang teardown.
async fn reap(mut child: Child) {
    // `kill_on_drop` already prevents a zombie if this times out; the explicit wait is what
    // collects the exit status on the normal path.
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

/// Aborts both drain tasks and waits for them so no task outlives the call.
async fn abort_all(
    stdout: tokio::task::JoinHandle<Vec<u8>>,
    stderr: tokio::task::JoinHandle<Vec<u8>>,
) {
    stdout.abort();
    stderr.abort();
    let _ = stdout.await;
    let _ = stderr.await;
}

/// Whether a program exists on `PATH`.
#[must_use]
pub fn has_program(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Runs a process, returning a future so the seam is uniform with the async callers.
///
/// A convenience over [`run`] for a caller that already owns an `Arc`-free `SpawnOptions`.
pub fn spawn(
    options: SpawnOptions,
    cancel: CancellationToken,
) -> BoxFut<Result<ProcessOutcome, ProcessError>> {
    Box::pin(async move { run(options, cancel).await })
}

/// A running process whose output is readable while it runs.
///
/// The collecting [`run`] is what a tool wants: it waits and renders one result. A job wants the
/// opposite — the process is still running when the caller reads, so the output has to arrive as it
/// is produced. The two cannot share one implementation, because the collecting form must not return
/// until the child exits while this one must return immediately.
pub struct RunningProcess {
    child: Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    pid: Option<u32>,
}

impl RunningProcess {
    /// Reads the next available chunk of either stream.
    ///
    /// Returns `None` when both streams are closed. Chunks are `(channel, text)`; the caller decides
    /// how to label them, which is what lets a job record stdout and stderr differently.
    pub async fn next_chunk(&mut self) -> Option<(crate::jobs::Channel, String)> {
        use tokio::io::AsyncReadExt as _;

        loop {
            // Two buffers, because `select!` cannot mutably borrow one buffer for two futures.
            // `select` on both readers is what stops a chatty stderr from starving stdout the way a
            // sequential read would.
            let mut out_buffer = [0u8; 4096];
            let mut err_buffer = [0u8; 4096];
            match (self.stdout.as_mut(), self.stderr.as_mut()) {
                (Some(out), Some(err)) => {
                    tokio::select! {
                        result = out.read(&mut out_buffer) => match result {
                            Ok(0) | Err(_) => {
                                self.stdout = None;
                                if self.stdout.is_none() && self.stderr.is_none() {
                                    return None;
                                }
                            }
                            Ok(n) => {
                                return Some((
                                    crate::jobs::Channel::Stdout,
                                    String::from_utf8_lossy(&out_buffer[..n]).into_owned(),
                                ))
                            }
                        },
                        result = err.read(&mut err_buffer) => match result {
                            Ok(0) | Err(_) => {
                                self.stderr = None;
                                if self.stdout.is_none() && self.stderr.is_none() {
                                    return None;
                                }
                            }
                            Ok(n) => {
                                return Some((
                                    crate::jobs::Channel::Stderr,
                                    String::from_utf8_lossy(&err_buffer[..n]).into_owned(),
                                ))
                            }
                        },
                    }
                }
                (Some(out), None) => match out.read(&mut out_buffer).await {
                    Ok(0) | Err(_) => {
                        self.stdout = None;
                        return None;
                    }
                    Ok(n) => {
                        return Some((
                            crate::jobs::Channel::Stdout,
                            String::from_utf8_lossy(&out_buffer[..n]).into_owned(),
                        ));
                    }
                },
                (None, Some(err)) => match err.read(&mut err_buffer).await {
                    Ok(0) | Err(_) => {
                        self.stderr = None;
                        return None;
                    }
                    Ok(n) => {
                        return Some((
                            crate::jobs::Channel::Stderr,
                            String::from_utf8_lossy(&err_buffer[..n]).into_owned(),
                        ));
                    }
                },
                (None, None) => return None,
            }
        }
    }

    /// Waits for the child to exit, returning its code.
    ///
    /// # Errors
    ///
    /// Returns the wait error; a signalled child reports `None` as its code.
    pub async fn wait(mut self) -> Result<Option<i32>, std::io::Error> {
        self.child.wait().await.map(|status| status.code())
    }

    /// Kills the whole process group.
    ///
    /// The group rather than the child: a shell that started a pipeline leaves other processes holding
    /// the pipes, and killing only the shell leaves them running with no reader.
    pub fn terminate(&self) {
        kill_tree(self.pid);
    }
}

/// Spawns a process whose output can be read while it runs.
///
/// # Errors
///
/// Returns [`ProcessError::Spawn`] when the program cannot start.
pub fn start(options: &SpawnOptions) -> Result<RunningProcess, ProcessError> {
    let mut command = Command::new(&options.program);
    command
        .args(&options.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }
    if options.clear_environment {
        // Before adding our own: `env_clear` after would drop them too.
        command.env_clear();
    }
    if options.clear_environment {
        command.env_clear();
    }
    for (key, value) in &options.env {
        command.env(key, value);
    }

    if options.new_process_group {
        #[cfg(unix)]
        {
            // SAFETY: `pre_exec` runs in the child after fork and before exec, where only
            // async-signal-safe calls are allowed. `setpgid` is one, and it allocates nothing.
            #[allow(
                unsafe_code,
                reason = "pre_exec is the only way to set a process group before exec"
            )]
            unsafe {
                command.pre_exec(|| {
                    nix::unistd::setpgid(
                        nix::unistd::Pid::from_raw(0),
                        nix::unistd::Pid::from_raw(0),
                    )
                    .map_err(std::io::Error::from)?;
                    Ok(())
                });
            }
        }
    }

    let program = options.program.clone();
    let mut child = command
        .spawn()
        .map_err(|source| ProcessError::Spawn { program, source })?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    Ok(RunningProcess {
        child,
        stdout,
        stderr,
        pid,
    })
}
