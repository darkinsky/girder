//! A [`Runtime`] that starts an extension as a shell command.
//!
//! The registry in [`girder_core::extensions`] owns the lifecycle — approval, service checks,
//! status — and deliberately knows nothing about how an extension is executed. This module is the
//! missing other half: the thing that turns an approved [`Definition`] into a process, and a
//! [`Run`] back into a way to end it.
//!
//! # What `Definition::source` actually is
//!
//! `source` is documented as *text*: "stored as text rather than as a path: a path ties the
//! definition to one host's filesystem, and a definition that a client submitted must be
//! inspectable before it runs." Nothing in this crate gives that text a syntax. There is no parser,
//! no loader, and the RPC surface can only list and approve — so the only interpretation this build
//! can offer without inventing an extension language is the POSIX shell. [`ShellRuntime`] therefore
//! runs `source` as the script of a `sh -c` command line.
//!
//! That is a real execution rather than a simulation: a definition whose source is executable text
//! starts a process, the process is a process-group leader so stopping it also stops what it
//! started, and stopping is what `deactivate` does. The cost is that a source written as *prose*
//! does not work, and the two comment markers reach the shell very differently. A `#`-only source
//! is the dangerous one: `sh` accepts it and exits 0, so an extension that did nothing would look
//! exactly like one that worked. A `//`-only source fails loudly instead — the shell reads it as an
//! absolute path and reports "not found" — which is a correct failure, but a confusing one to
//! report for what was plainly a description. [`assert_runnable`] refuses both before a human is
//! asked to approve them, and [`NoopRuntime`] is for a deployment that wants the definition
//! lifecycle without execution — documented as proving nothing about the extension.
//!
//! # What "running" can and cannot mean here
//!
//! `activate` reports a start, not a health check. This runtime waits briefly (the *start grace*)
//! for a command that fails immediately — `sh -c 'no-such-program'` spawns a shell successfully and
//! only then exits 127 — so a command that cannot run is an activation error instead of a
//! "running" extension that was already dead. A process that dies *after* the grace is not
//! reported at all: [`Run`] carries a stopper and nothing else, so there is no channel from a
//! process's exit status back to [`Status`]. An extension that crashes later keeps its `Running`
//! status until somebody deactivates it. That is a limit of the extension API, not of this module,
//! and it is why the grace is a knob rather than a fixed value.
//!
//! # Output
//!
//! Both pipes are drained, because a pipe nobody reads fills at 64 KiB and blocks the writer — an
//! extension that logged would otherwise freeze. Standard output is discarded, and standard error
//! is kept only as a small tail used to explain a start failure: a [`Run`] has nowhere to report a
//! running extension's output, so buffering an unbounded amount of it would be a leak.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, ChildStderr, Command};
use tokio_util::sync::CancellationToken;

// The crate's own items are named through `::girder_core` rather than through `crate::` so that this
// file can be compiled both as a module of the library and, by `tests/extension_runtime.rs`, as a
// module of the test crate before `lib.rs` re-exports it. `extern crate self as girder_core` in
// `lib.rs` is what makes that path resolve from inside the crate — the same trick the derive
// macros rely on.
use ::girder_core::{
    BoxFut, ExtensionDefinition as Definition, ExtensionError, ExtensionRun as Run,
    ExtensionRuntime as Runtime,
};

/// How long an activation waits to see whether the command fails at once.
///
/// Long enough that a shell reporting "not found" is always caught, short enough that activating a
/// long-running extension is not noticeably delayed. The wait only happens on the failure path: a
/// process that is still alive when this expires is reported as started immediately afterwards.
const DEFAULT_START_GRACE: Duration = Duration::from_millis(250);

/// How much of a failed start's standard error is kept for the error message.
const DEFAULT_STDERR_TAIL: usize = 4096;

/// How long a stopped process is given to exit after `SIGTERM` before it is killed outright.
///
/// A process that ignores `SIGTERM` would otherwise survive `deactivate`; a process that needs to
/// clean up would otherwise be killed on the spot.
const STOP_GRACE: Duration = Duration::from_millis(500);

/// Starts an extension by running its source as a shell command line.
///
/// See the module documentation for what `source` is taken to mean, and for the limits that
/// interpretation carries.
#[derive(Clone, Debug)]
pub struct ShellRuntime {
    /// The shell that interprets `source`.
    shell: String,
    /// The directory the command runs in, when the caller wants one.
    cwd: Option<PathBuf>,
    /// How long a start waits for an immediate exit.
    start_grace: Duration,
    /// How much of a failed start's standard error is kept.
    stderr_tail: usize,
}

impl ShellRuntime {
    /// Builds a runtime that runs sources with `sh`.
    ///
    /// `sh` rather than `bash`: it exists on every POSIX system, and a definition that needed
    /// bash-isms would fail on a minimal image in a way that looks like the harness is broken.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shell: "sh".to_owned(),
            cwd: None,
            start_grace: DEFAULT_START_GRACE,
            stderr_tail: DEFAULT_STDERR_TAIL,
        }
    }

    /// Runs sources with a different shell.
    #[must_use]
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = shell.into();
        self
    }

    /// Runs sources in `cwd`.
    ///
    /// A definition has no working directory of its own: it is text, and text does not carry one.
    /// The deployment that activates it decides, which is also why this is a property of the runtime
    /// rather than of the definition.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Changes how long a start waits for the command to fail immediately.
    ///
    /// A value of zero disables the check, which means a source that cannot run is reported as
    /// running until it is deactivated. Because such an extension also stops nothing, that is only
    /// sensible for a caller that watches the process itself.
    #[must_use]
    pub fn with_start_grace(mut self, grace: Duration) -> Self {
        self.start_grace = grace;
        self
    }

    /// Starts the command and decides whether the start succeeded.
    ///
    /// Everything the run needs after this returns is either owned by the returned [`Run`] or by the
    /// task reaping the child; nothing borrows `self`, which is what [`BoxFut`] being `'static`
    /// requires.
    async fn launch(&self, definition: &Definition) -> Result<Run, ExtensionError> {
        let id = definition.id.clone();

        let mut command = Command::new(&self.shell);
        command
            .arg("-c")
            .arg(&definition.source)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A `Run` dropped without being stopped must not leave the extension alive: the registry
            // stops a run on `deactivate` and `remove`, but a process that outlived both would be
            // unstoppable by anything.
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        // The parent's environment holds whatever credentials the harness was launched with, and an
        // extension is code somebody added, not the harness itself. Clearing first and then adding
        // the allowlist is what keeps a token from reaching it merely because it was started here.
        command.env_clear();
        for (name, value) in child_environment() {
            command.env(name, value);
        }
        #[cfg(unix)]
        {
            // The child becomes its own process-group leader so that stopping it also stops what it
            // started: a shell that ran a pipeline leaves children holding the pipes, and killing
            // only the shell would leave them running with no reader.
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|error| ExtensionError::LoadFailed {
                id: id.to_string(),
                reason: format!("cannot run `{}`: {error}", self.shell),
            })?;
        let pid = child.id();

        // Drain both pipes while the process runs. Waiting instead would deadlock the extension —
        // and this runtime with it — as soon as it produced more than a pipe buffer of output.
        if let Some(mut stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await;
            });
        }
        let stderr_task = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(drain_tail(stderr, self.stderr_tail)));

        // `sh -c 'no-such-program'` spawns successfully and only then exits 127, so "the spawn
        // worked" is not "the extension started". The grace is the only way to tell them apart
        // without inventing a readiness protocol the source has no way to implement.
        match tokio::time::timeout(self.start_grace, child.wait()).await {
            Ok(status) => {
                let status = status.map_err(|error| ExtensionError::LoadFailed {
                    id: id.to_string(),
                    reason: format!("cannot wait for the extension: {error}"),
                })?;
                let tail = collect_tail(stderr_task).await;
                if status.success() {
                    // A one-shot that finished inside the grace did start; it simply had nothing
                    // left to do. Reporting it as a failure would reject a `seed-the-database`
                    // style definition, so it is reported as started with a stopper that stops
                    // nothing.
                    return Ok(Run {
                        id,
                        stop: Box::new(|| {}),
                    });
                }
                return Err(ExtensionError::LoadFailed {
                    id: id.to_string(),
                    reason: exit_reason(status.code(), &tail),
                });
            }
            // Still alive: the start is real. The reaper below owns the child from here, so its exit
            // is collected even though nothing waits for it.
            Err(_elapsed) => {}
        }

        let signal = Arc::new(StopSignal::new(pid));
        tokio::spawn(reap(child, Arc::clone(&signal)));
        let stopper = Arc::clone(&signal);
        Ok(Run {
            id,
            stop: Box::new(move || stopper.request()),
        })
    }
}

impl Default for ShellRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime for ShellRuntime {
    fn start(
        &self,
        definition: &Definition,
        _available: &[String],
    ) -> BoxFut<Result<Run, ExtensionError>> {
        // `available` is not used: the registry checks the declared services before it calls the
        // runtime, and doing it again here would give the rule two implementations that could
        // disagree. The runtime's job is to run the source, not to police which services exist.
        let settings = self.clone();
        let definition = definition.clone();
        Box::pin(async move { settings.launch(&definition).await })
    }
}

/// A runtime that records activation without running anything.
///
/// # Why this exists
///
/// Not every definition is executable. A definition whose `source` is a description — the `// source`
/// placeholder, a link to a repository, a comment explaining what the extension would do — has no
/// command for [`ShellRuntime`] to run, and inventing one would be worse than admitting there is
/// none. This runtime keeps the lifecycle usable for those definitions: `activate` succeeds, the
/// status moves to `Running`, `deactivate` moves it back.
///
/// # Limitations
///
/// * It does not run the extension. Nothing is registered, no tool appears, no process exists.
/// * `Running` therefore means "activation was recorded", not "the extension works". A definition
///   activated this way has been approved and does nothing, which is a safe default but never a
///   test of the extension.
/// * It cannot fail, so it can never report a broken definition.
///
/// A deployment that wants extensions to have an effect must use [`ShellRuntime`], or a runtime that
/// understands whatever language its definitions are written in.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRuntime;

impl NoopRuntime {
    /// Builds the inert runtime.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Runtime for NoopRuntime {
    fn start(
        &self,
        definition: &Definition,
        _available: &[String],
    ) -> BoxFut<Result<Run, ExtensionError>> {
        let id = definition.id.clone();
        Box::pin(async move {
            Ok(Run {
                id,
                stop: Box::new(|| {}),
            })
        })
    }
}

/// Checks that a definition is something [`ShellRuntime`] could actually run.
///
/// # Why this is separate from `start`
///
/// `start` is only reachable through [`Registry::activate`](::girder_core::ExtensionRegistry::activate),
/// which means a caller can spend an approval prompt — and a human's attention — on a definition
/// that could never run. This answers the question first, so a client can refuse to ask.
///
/// It checks only what can be known without starting anything: that the source is not blank, that it
/// is not a comment-only script under either marker the two callers use (`#` and `//`), and that the
/// shell which would interpret it exists. It deliberately does not try to parse the script: `sh` is
/// the only thing that knows what `sh` accepts, and a second parser would disagree with it. A source
/// that *looks* like prose but is not comment-only is therefore accepted here and reported as a
/// failed start by `start`, which is the right division — this check refuses the cases that would
/// otherwise be reported as *successes*, and leaves the rest to the shell.
///
/// # Errors
///
/// Returns [`ExtensionError::LoadFailed`] with the id and a reason when the definition could not be
/// run by this runtime.
pub fn assert_runnable(definition: &Definition) -> Result<(), ExtensionError> {
    let id = definition.id.to_string();
    let source = definition.source.trim();
    if source.is_empty() || !has_statement(source) {
        return Err(ExtensionError::LoadFailed {
            id,
            reason: "the source is a description rather than a command; use a runtime that \
                     understands it, or NoopRuntime to record the activation without running it"
                .to_owned(),
        });
    }
    // The shell `ShellRuntime::new` would use, so the check cannot drift from what `start` does.
    let shell = ShellRuntime::new().shell;
    if !::girder_core::has_program(&shell) {
        // A missing shell would otherwise surface as a spawn error per activation, which reads as
        // "this extension is broken" rather than "this deployment cannot run any extension".
        return Err(ExtensionError::LoadFailed {
            id,
            reason: format!("cannot run the source: `{shell}` is not on PATH"),
        });
    }
    Ok(())
}

/// Whether a script contains at least one line that is not blank and not a comment.
///
/// A source made only of comment lines is prose that happens to start with a comment marker, which is
/// the shape a description takes. The two markers fail differently, and both are worth refusing
/// before a human is asked:
///
/// * `#` is a real shell comment, so `sh` accepts the script and exits 0. Without this check an
///   extension that did nothing would be indistinguishable from one that worked — the dangerous
///   case, because nothing reports it.
/// * `//` is not a shell comment, so the line runs as a command and fails with "not found". That is
///   correctly reported as a failed start, but as a *command* failure, which misdescribes a
///   definition that was plainly a description.
///
/// `//` is recognised because a JSON field holding prose is where it comes from, whatever the shell
/// thinks of it.
fn has_statement(source: &str) -> bool {
    source
        .lines()
        .map(str::trim)
        .any(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("//"))
}

/// The one piece of state a running extension shares with the caller that stops it.
///
/// A [`Run`]'s stopper is synchronous and cannot await, while ending a process properly needs a
/// grace period and an escalation. Splitting it this way lets `deactivate` return with the request
/// made, and a task own the waiting.
struct StopSignal {
    /// The child's pid, which is also its process-group id when a group was requested.
    ///
    /// Cleared by the reaper once the child has been collected: after that the pid identifies
    /// nothing this runtime owns, and the operating system is free to hand it to an unrelated
    /// process. Signalling a stale pid would then kill somebody else's process group.
    pid: std::sync::atomic::AtomicI32,
    /// Wakes the reaper when a stop is requested.
    cancel: CancellationToken,
}

impl StopSignal {
    /// Records the child's pid.
    fn new(pid: Option<u32>) -> Self {
        Self {
            pid: std::sync::atomic::AtomicI32::new(pid.map_or(0, |pid| {
                // A pid fits in `i32` on every platform with POSIX; anything larger is not a pid we
                // could signal anyway, and 0 means "nothing to signal".
                i32::try_from(pid).unwrap_or(0)
            })),
            cancel: CancellationToken::new(),
        }
    }

    /// The pid, or `None` when the child has already been collected.
    fn pid(&self) -> Option<u32> {
        let pid = self.pid.load(std::sync::atomic::Ordering::SeqCst);
        u32::try_from(pid).ok().filter(|pid| *pid != 0)
    }

    /// Forgets the pid, so a later stop cannot signal a reused one.
    fn release(&self) {
        self.pid.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// Asks the process to stop. Safe to call more than once.
    fn request(&self) {
        // The signal is sent here rather than from the reaper so that stopping is not merely queued:
        // `deactivate` reports `Stopped` as soon as this returns, and a caller that checked the
        // process immediately afterwards should find it already signalled.
        #[cfg(unix)]
        if let Some(pid) = self.pid() {
            signal_group(pid, nix::sys::signal::Signal::SIGTERM);
        }
        // Off unix there is no portable way to signal from a synchronous closure, so the reaper
        // does the killing when the token fires.
        self.cancel.cancel();
    }
}

/// Waits for the child, escalating to `SIGKILL` when a requested stop is not obeyed.
async fn reap(mut child: Child, stop: Arc<StopSignal>) {
    let waited = tokio::select! {
        result = child.wait() => Some(result),
        () = stop.cancel.cancelled() => None,
    };
    // `Some` means the child was collected, whether or not the wait itself reported an error: after
    // either, the pid is no longer ours and the kernel may hand it to an unrelated process. Treating
    // an errored wait as "still running" would signal that stranger's group.
    if let Some(waited) = waited {
        stop.release();
        // An errored wait is not acted on: the child is gone either way, and there is no caller left
        // to report to (`activate` has already returned). Releasing the pid is what matters.
        let _ = waited;
        return;
    }

    #[cfg(not(unix))]
    let _ = child.start_kill();
    if tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        // A process that ignored `SIGTERM` would otherwise keep the extension alive after the
        // registry already reported it stopped.
        #[cfg(unix)]
        if let Some(pid) = stop.pid() {
            signal_group(pid, nix::sys::signal::Signal::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = child.start_kill();
        // Bounded so a wedged process cannot keep this task alive forever; dropping the child is the
        // backstop, because `kill_on_drop` is set.
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    stop.release();
}

/// Signals the process group led by `pid`.
#[cfg(unix)]
fn signal_group(pid: u32, signal: nix::sys::signal::Signal) {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // A negative pid names the group whose id is `pid`, and `process_group(0)` made the child that
    // group's leader. Signalling the child alone would leave its children holding the pipes.
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let _ = kill(Pid::from_raw(-pid), signal);
}

/// Reads a child's standard error to the end, keeping only the last `limit` bytes.
///
/// Reading to the end is what keeps the pipe from filling; keeping a tail rather than everything is
/// what keeps a chatty failure from turning into unbounded memory.
async fn drain_tail(mut stderr: ChildStderr, limit: usize) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                tail.extend_from_slice(&buffer[..read]);
                if tail.len() > limit {
                    let excess = tail.len().saturating_sub(limit);
                    tail.drain(..excess);
                }
            }
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// Collects the drained standard error, bounded so a stuck reader cannot stall activation.
async fn collect_tail(handle: Option<tokio::task::JoinHandle<String>>) -> String {
    let Some(handle) = handle else {
        return String::new();
    };
    if let Ok(Ok(tail)) = tokio::time::timeout(Duration::from_secs(2), handle).await {
        tail
    } else {
        // No tail is better than a hang: the failure is reported with whatever the exit status says.
        String::new()
    }
}

/// Renders why a start failed.
fn exit_reason(code: Option<i32>, tail: &str) -> String {
    let how = match code {
        Some(code) => format!("exited with code {code}"),
        // A signalled child has no exit code, and reporting "code 0" or "code -1" would mislead.
        None => "was killed by a signal".to_owned(),
    };
    let detail = tail.trim();
    if detail.is_empty() {
        format!("the extension {how} before it was running")
    } else {
        format!("the extension {how} before it was running: {detail}")
    }
}

/// The environment an extension's process receives.
///
/// Built from the parent's environment through the same allowlist a shell command already gets:
/// an extension runs with the harness's authority, but it does not get to inherit whatever
/// credentials the harness was launched with just because it was started from inside it.
fn child_environment() -> BTreeMap<String, String> {
    let parent: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| {
            // A non-UTF-8 variable cannot be passed as a `String`; dropping it is better than
            // making every extension fail to start because one unrelated variable was unusual.
            Some((name.into_string().ok()?, value.into_string().ok()?))
        })
        .collect();
    ::girder_core::resolve_launch_environment(&parent, &BTreeMap::new()).values
}
