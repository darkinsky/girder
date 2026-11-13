//! A shell that lives across commands, so `cd` and exported variables survive.
//!
//! A one-shot `sh -c` call starts a fresh process per command, which is why `cd /tmp` followed by
//! `pwd` prints the original directory: the directory was changed in a process that had already
//! exited. This module keeps the alternative — one `sh` process for the lifetime of the
//! [`PersistentShell`] — fed one script per command over a pipe and read back over pipes.
//!
//! # How one command is separated from the next
//!
//! Because stdin is a pipe rather than a terminal, the shell prints no prompt and echoes nothing
//! back, so the framing only has to cover what the shell itself prints. Each command is wrapped:
//!
//! ```text
//! <NUL>tag__GIRDER_PS_START<NUL>                              stdout: the body starts here
//! <NUL>tag__GIRDER_PS_ERR_START<NUL>                          stderr: same, on the error stream
//! eval -- '<command>' </dev/null
//! <NUL>tag__GIRDER_PS_END:<status>:<cwd hex><NUL>             stdout: body ends, payload
//! <NUL>tag__GIRDER_PS_ERR_END:<NUL>                           stderr: body ends
//! ```
//!
//! `<status>` is `$?` captured immediately after the `eval`, so it is the command's exit status and
//! not a later helper's. The working directory is reported by the shell rather than tracked here,
//! because only the shell knows where `cd` went — including into a directory this process could not
//! have predicted. It is hex-encoded because a path may contain the newline a line-oriented frame
//! would use as its delimiter.
//!
//! The leading `NUL` is a byte a shell's own output almost never contains at a marker boundary, and
//! it keeps a body that ends mid-token from running into the marker. `printf` writes the markers
//! with `\000` escapes, so no literal NUL ever travels through the script text itself.
//!
//! # Why the tag is random
//!
//! A fixed marker could be printed by the command it is supposed to terminate: `echo __GIRDER_PS_END`
//! is ordinary output, and treating it as completion would truncate the result and return a wrong
//! exit code. The tag is therefore unique per shell (process id, clock, and a counter, hashed) and
//! is *never* exported into the shell's environment, so a command cannot learn it and fabricate it.
//! Uniqueness is what makes a first match correct, which is what keeps the scanner's memory bounded.
//!
//! # Why stderr is kept separate
//!
//! Merging would be simpler and wrong: a command that succeeds while warning on stderr and one that
//! fails with a message would produce identical bytes, and a caller that classified failures by
//! output text could not tell them apart. Exit status already separates the two, so the streams are
//! reported separately and the caller can decide. The cost is that the interleaving of the two
//! streams is not preserved, which is a property neither stream promises anyway once it is a pipe.
//!
//! # Why a timeout kills the shell
//!
//! A shell that has been interrupted mid-command has an unknown state: a half-written file, a child
//! still running, an input line partially consumed. Reusing it would attribute that state to the
//! next command, so a timeout terminates the shell and the next [`PersistentShell::run`] reports
//! [`ShellError::NotRunning`] rather than guessing. The same applies to a shell that exits on its
//! own (`exit`, `exec`, or a `set -e` failure), which is reported as [`ShellError::Exited`].
//!
//! # What state is *not* preserved
//!
//! - A foreground child that reads stdin blocks until the timeout instead of seeing end of input:
//!   the shell's own stdin is the command stream, and separating the two would need a terminal.
//! - Output from a background process started by an earlier command may arrive during a later one
//!   and be attributed to it; there is no per-process framing to recover the original owner.
//! - `set -e`, `set -x`, and `umask` changes persist, exactly as they do in a real shell.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::launch_environment;

/// The default limit on one command, matching the one-shot shell's.
///
/// A command that never returns would otherwise hold a tool call open for the session's life.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The default bound on each output stream of one command.
///
/// 64 KiB: far above what a model can usefully read and far below a context window, so a chatty
/// command is truncated rather than refused.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// How long the stderr frame is waited for after the stdout frame has arrived.
///
/// The shell writes the two markers back to back, so the normal gap is microseconds. A stderr that
/// never delivers its marker — a background process holding the pipe open, for instance — must not
/// turn a completed command into a timeout, so the wait is bounded separately.
const STDERR_GRACE: Duration = Duration::from_secs(2);

/// How much is read from a pipe at a time.
///
/// The scanner keeps the unparsed tail of each chunk so a marker split across two reads is still
/// found; a larger chunk costs memory without improving that, and a smaller one costs syscalls.
const READ_CHUNK: usize = 8 * 1024;

/// The stdout start marker's suffix; the tag precedes it.
const STDOUT_START: &str = "__GIRDER_PS_START";

/// The stdout end marker's suffix, ending with the `:` that introduces the payload.
const STDOUT_END: &str = "__GIRDER_PS_END:";

/// The stderr start marker's suffix.
const STDERR_START: &str = "__GIRDER_PS_ERR_START";

/// The stderr end marker's suffix, ending with the `:` that introduces an empty payload.
const STDERR_END: &str = "__GIRDER_PS_ERR_END:";

/// How a shell is configured before it starts.
///
/// A resolved value rather than a set of arguments: the working directory and the environment are
/// decided once, before the process exists, and the same configuration describes what the shell
/// will run with without asking the process afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The shell to run.
    shell: String,
    /// The directory the shell starts in.
    cwd: PathBuf,
    /// The variables the shell starts with, already resolved from the parent.
    environment: BTreeMap<String, String>,
    /// The limit applied to each command.
    timeout: Duration,
    /// The bound applied to each output stream of each command.
    max_output_bytes: usize,
}

impl Config {
    /// Configures a shell that starts in `cwd`, holding the baseline environment.
    ///
    /// The baseline is the launcher's allowlist rather than the whole parent environment: the
    /// harness's own credentials must not reach a command a model asked to run, and a shell that
    /// inherited everything would leak them to every command it ran.
    ///
    /// `sh` rather than `bash` by default, for the same reason the one-shot shell makes that choice:
    /// it exists on every POSIX system, and a shell that needed bash-isms would fail on a minimal
    /// image in a way that looks like the harness is broken. [`with_shell`](Self::with_shell) covers
    /// the deployments that want bash.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let parent: BTreeMap<String, String> = std::env::vars().collect();
        let environment = launch_environment::resolve(&parent, &BTreeMap::new()).values;

        Self {
            shell: "sh".to_owned(),
            cwd: cwd.into(),
            environment,
            timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Runs a different shell program.
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = shell.into();
        self
    }

    /// Replaces the environment the shell starts with.
    pub fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = environment;
        self
    }

    /// Bounds one command.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Bounds each output stream of one command.
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// The shell program.
    pub fn shell(&self) -> &str {
        &self.shell
    }

    /// The directory the shell starts in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The environment the shell starts with.
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    /// The limit applied to each command.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The bound applied to each output stream.
    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

/// What one command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// What the command wrote to stdout, with the framing removed.
    stdout: String,
    /// What the command wrote to stderr, with the framing removed.
    stderr: String,
    /// The exit status the shell reported.
    code: i32,
    /// Bytes of stdout dropped because the output bound was reached.
    stdout_omitted: u64,
    /// Bytes of stderr dropped because the output bound was reached.
    stderr_omitted: u64,
}

impl Output {
    /// The command's standard output.
    ///
    /// Empty rather than absent when the command wrote nothing, so a caller does not distinguish
    /// "wrote nothing" from "the stream was not captured".
    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    /// The command's standard error.
    pub fn stderr(&self) -> &str {
        &self.stderr
    }

    /// The exit status.
    pub fn code(&self) -> i32 {
        self.code
    }

    /// Whether the command exited successfully.
    pub fn success(&self) -> bool {
        self.code == 0
    }

    /// How many bytes of stdout the output bound dropped from the end.
    ///
    /// Non-zero means [`stdout`](Self::stdout) is a prefix rather than the whole stream. The head is
    /// kept rather than the tail because a compiler's first error is the one that explains the rest.
    pub fn stdout_omitted_bytes(&self) -> u64 {
        self.stdout_omitted
    }

    /// How many bytes of stderr the output bound dropped from the end.
    pub fn stderr_omitted_bytes(&self) -> u64 {
        self.stderr_omitted
    }
}

/// A failure of the shell itself, as opposed to a command that exited non-zero.
///
/// A command that fails is a successful [`PersistentShell::run`] with a non-zero
/// [`Output::code`]: the shell did what it was asked. These variants are reserved for the cases
/// where the shell could not deliver an answer at all.
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    /// The shell process could not be started.
    #[error("failed to start `{shell}` in `{cwd}`: {source}")]
    Start {
        /// The program that was asked for.
        shell: String,
        /// The directory it was asked to start in.
        cwd: PathBuf,
        /// Why the spawn failed.
        source: std::io::Error,
    },
    /// The command did not finish within the configured limit; the shell was terminated.
    #[error("the command did not finish within {seconds} seconds; the shell was terminated")]
    Timeout {
        /// The limit that expired, in seconds.
        seconds: u64,
    },
    /// The shell exited before reporting the command's status.
    ///
    /// An `exit`, an `exec`, or a `set -e` failure inside the command closes the shell, and so does a
    /// signal. The shell is no longer usable and the next run reports [`ShellError::NotRunning`].
    #[error("the shell exited before reporting a status (code {code:?})")]
    Exited {
        /// The exit code, when the shell was reaped in time to report it.
        code: Option<i32>,
    },
    /// The framing the shell is supposed to print was missing or malformed.
    #[error("the shell's completion marker was unusable: {detail}")]
    Protocol {
        /// What was wrong.
        detail: String,
    },
    /// The command contained a NUL byte, which a shell cannot receive.
    #[error("the command contains a NUL byte, which a shell cannot receive")]
    NulByte,
    /// The shell had already been closed, or was terminated by an earlier failure.
    #[error("the shell is not running; start a new one")]
    NotRunning,
    /// Talking to the shell failed.
    #[error("failed to talk to the shell: {0}")]
    Io(#[from] std::io::Error),
}

/// A shell that keeps its working directory, environment, and shell variables between commands.
///
/// One long-running shell process, not one process per command: `cd`, `export`, and shell functions
/// are state of that process, and a process that exits cannot carry them. See the module
/// documentation for the framing and its limits.
pub struct PersistentShell {
    /// The shell process, while it is believed to be alive.
    child: Option<Child>,
    /// Its process group id, which equals its pid because it leads its own group.
    pid: Option<u32>,
    /// The write end of the command stream.
    stdin: Option<ChildStdin>,
    /// Chunks of the shell's stdout, in arrival order.
    stdout_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    /// Chunks of the shell's stderr, in arrival order.
    stderr_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    /// The task draining stdout; kept so it can be aborted if a grandchild holds the pipe open.
    stdout_task: Option<JoinHandle<()>>,
    /// The task draining stderr.
    stderr_task: Option<JoinHandle<()>>,
    /// The frame markers for this shell.
    markers: Markers,
    /// Where the shell last said it was.
    cwd: PathBuf,
    /// The limit applied to each command.
    timeout: Duration,
    /// The bound applied to each output stream.
    max_output_bytes: usize,
    /// Whether the shell has been terminated.
    closed: bool,
}

impl PersistentShell {
    /// Starts a shell, leaving it ready for its first command.
    ///
    /// The shell reads its command stream from a pipe rather than a terminal, so it prints no prompt
    /// and echoes nothing back; the only thing read from its output is what the commands and the
    /// framing print.
    ///
    /// # Runtime
    ///
    /// Must be called from a Tokio runtime context: the process is spawned through `tokio::process`
    /// and its pipes are drained by tasks, both of which need a reactor.
    pub fn start(config: Config) -> Result<Self, ShellError> {
        let Config {
            shell,
            cwd,
            environment,
            timeout,
            max_output_bytes,
        } = config;

        let mut command = Command::new(&shell);
        command
            // `-s` reads the program from stdin, which is the whole point: the process stays alive
            // between commands, so its state does.
            .arg("-s")
            .current_dir(&cwd)
            // Cleared before adding: a shell that inherited the harness's environment would hand
            // every command whatever credentials the harness was started with.
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A dropped shell must not outlive its owner: a panic or an early return would otherwise
            // leave a shell process holding a tool call's pipes.
            .kill_on_drop(true);
        for (name, value) in &environment {
            command.env(name, value);
        }
        lead_own_process_group(&mut command);

        let mut child = command.spawn().map_err(|source| ShellError::Start {
            shell: shell.clone(),
            cwd: cwd.clone(),
            source,
        })?;
        let pid = child.id();

        let stdin = child.stdin.take().ok_or_else(|| {
            ShellError::Start {
                shell: shell.clone(),
                cwd: cwd.clone(),
                source: std::io::Error::other("the shell's stdin pipe was not created"),
            }
        })?;
        let stdout = child.stdout.take().ok_or_else(|| ShellError::Start {
            shell: shell.clone(),
            cwd: cwd.clone(),
            source: std::io::Error::other("the shell's stdout pipe was not created"),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| ShellError::Start {
            shell: shell.clone(),
            cwd: cwd.clone(),
            source: std::io::Error::other("the shell's stderr pipe was not created"),
        })?;

        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();

        Ok(Self {
            child: Some(child),
            pid,
            stdin: Some(stdin),
            stdout_rx: Some(stdout_rx),
            stderr_rx: Some(stderr_rx),
            stdout_task: Some(tokio::spawn(drain(stdout, stdout_tx))),
            stderr_task: Some(tokio::spawn(drain(stderr, stderr_tx))),
            markers: Markers::new(),
            cwd,
            timeout,
            max_output_bytes,
            closed: false,
        })
    }

    /// Runs one command and returns what it produced.
    ///
    /// The command runs in the directory the previous command left the shell in, with the previous
    /// command's variables, which is the difference from a one-shot shell.
    ///
    /// # Errors
    ///
    /// A command that exits non-zero is a successful return with a non-zero [`Output::code`]. The
    /// errors are the ones where no answer exists: a timeout (the shell is then terminated), a shell
    /// that exited on its own, a framing failure, and a shell that is no longer running.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the shell in an unknown state — the command has been written and
    /// may still be running — so a caller that cancels should [`close`](Self::close) the shell rather
    /// than reuse it.
    pub async fn run(&mut self, command: &str) -> Result<Output, ShellError> {
        // A NUL byte cannot travel in an argument, and this command travels inside one: rejecting it
        // here says why, instead of letting the shell fail with a syntax error that blames the user.
        if command.contains('\0') {
            return Err(ShellError::NulByte);
        }
        if self.closed {
            return Err(ShellError::NotRunning);
        }

        // Output that arrived after the previous command's markers cannot be told apart from this
        // command's, so it is dropped rather than attributed to a command that did not produce it.
        discard_pending(&mut self.stdout_rx);
        discard_pending(&mut self.stderr_rx);

        let script = self.markers.script(command);
        self.write(&script).await?;

        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut stdout_frame = Frame::new(self.max_output_bytes);
        let mut stderr_frame = Frame::new(self.max_output_bytes);
        // After stdout's marker arrives, stderr's is given a short, separate deadline so a command
        // that did finish is not turned into a timeout by a stderr that never terminates.
        let mut grace: Option<tokio::time::Instant> = None;

        let stop = loop {
            if stdout_frame.finished() && stderr_frame.finished() {
                break Stop::Complete;
            }
            let until = grace.map_or(deadline, |grace| grace.min(deadline));
            tokio::select! {
                () = tokio::time::sleep_until(until) => {
                    break if stdout_frame.finished() { Stop::StderrGrace } else { Stop::Timeout };
                }
                chunk = next(&mut self.stdout_rx) => {
                    let Some(chunk) = chunk else { break Stop::Exited };
                    stdout_frame.push(&chunk, &self.markers.stdout_start, &self.markers.stdout_end);
                }
                chunk = next(&mut self.stderr_rx) => {
                    let Some(chunk) = chunk else { break Stop::Exited };
                    stderr_frame.push(&chunk, &self.markers.stderr_start, &self.markers.stderr_end);
                }
            }
            if stdout_frame.finished() && grace.is_none() && !stderr_frame.finished() {
                grace = Some(tokio::time::Instant::now() + STDERR_GRACE);
            }
        };

        match stop {
            Stop::Complete | Stop::StderrGrace => {}
            Stop::Timeout => {
                let seconds = self.timeout.as_secs();
                self.terminate();
                return Err(ShellError::Timeout { seconds });
            }
            Stop::Exited => {
                let code = self.exit_code();
                self.terminate();
                return Err(ShellError::Exited { code });
            }
        }

        let Some(payload) = stdout_frame.payload() else {
            let detail = "the shell printed no completion marker".to_owned();
            return Err(ShellError::Protocol { detail });
        };
        let (code, cwd) = parse_payload(payload)?;
        // The shell is the authority on where it is: `cd` may have gone somewhere this process could
        // not have predicted, so the reported directory replaces the remembered one.
        if let Some(cwd) = cwd {
            self.cwd = cwd;
        }

        let (stderr, stderr_omitted) = stderr_frame.finish();
        let (stdout, stdout_omitted) = stdout_frame.finish();

        Ok(Output {
            stdout,
            stderr,
            code,
            stdout_omitted,
            stderr_omitted,
        })
    }

    /// The directory the shell is in, as of the last command it ran.
    ///
    /// Taken from the shell's own report rather than from this process, because only the shell knows
    /// where a `cd` went. Until the first command finishes this is the configured directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Whether the shell is still usable.
    pub fn is_running(&self) -> bool {
        !self.closed
    }

    /// Terminates the shell and releases its pipes and tasks.
    ///
    /// Consuming rather than borrowing so the shell cannot be used afterwards: a closed shell is not
    /// an empty one, and a caller that closed it meant to be done with its state.
    pub fn close(mut self) {
        self.terminate();
    }

    /// Writes one script to the shell's command stream.
    async fn write(&mut self, script: &str) -> Result<(), ShellError> {
        let outcome = {
            let Some(stdin) = self.stdin.as_mut() else {
                return Err(ShellError::NotRunning);
            };
            match stdin.write_all(script.as_bytes()).await {
                // Flushed explicitly: a shell reading from a pipe may otherwise wait for more input
                // while the command sits in a buffer this side never handed over.
                Ok(()) => stdin.flush().await,
                Err(error) => Err(error),
            }
        };

        match outcome {
            Ok(()) => Ok(()),
            Err(error) => {
                // A failed write means the pipe is gone, so the shell is gone with it. Marking it
                // closed here keeps the next call from reporting an unrelated timeout.
                self.terminate();
                Err(ShellError::Io(error))
            }
        }
    }

    /// The shell's exit code, when it can be reaped without blocking.
    fn exit_code(&mut self) -> Option<i32> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => status.code(),
            _ => None,
        }
    }

    /// Stops the shell and everything it started. Idempotent.
    fn terminate(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;

        // Dropping stdin is the polite shutdown: the shell sees end of input and exits once its
        // current command returns. The kill that follows is what covers a shell that will not.
        self.stdin.take();
        kill_group(self.pid);
        if let Some(task) = self.stdout_task.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        if let Some(mut child) = self.child.take() {
            // Covers the non-unix path, where no process group was set, and races with the group
            // kill harmlessly: a signal to a process that is already gone changes nothing.
            let _ = child.start_kill();
        }
    }
}

impl Drop for PersistentShell {
    fn drop(&mut self) {
        // Without this, a shell whose owner forgot to call `close` would keep running with the pipes
        // of a tool call that has already returned.
        self.terminate();
    }
}

/// Why the read loop stopped.
enum Stop {
    /// Both frames arrived.
    Complete,
    /// The stdout frame arrived and stderr's did not within its grace period.
    StderrGrace,
    /// No complete stdout frame arrived before the deadline.
    Timeout,
    /// A pipe closed, so the shell exited.
    Exited,
}

/// The markers one shell uses, derived from a tag no command can guess.
struct Markers {
    /// The random per-shell tag, as hex digits.
    tag: String,
    /// `<NUL>tag__GIRDER_PS_START<NUL>`.
    stdout_start: Vec<u8>,
    /// `<NUL>tag__GIRDER_PS_END:` — the payload and a `NUL` follow on the wire.
    stdout_end: Vec<u8>,
    /// `<NUL>tag__GIRDER_PS_ERR_START<NUL>`.
    stderr_start: Vec<u8>,
    /// `<NUL>tag__GIRDER_PS_ERR_END:` — an empty payload and a `NUL` follow.
    stderr_end: Vec<u8>,
}

impl Markers {
    /// Builds the markers for one shell.
    fn new() -> Self {
        let tag = unique_tag();
        Self {
            stdout_start: marker(&tag, STDOUT_START, true),
            stdout_end: marker(&tag, STDOUT_END, false),
            stderr_start: marker(&tag, STDERR_START, true),
            stderr_end: marker(&tag, STDERR_END, false),
            tag,
        }
    }

    /// The script that runs `command` inside the framing.
    ///
    /// One physical line per statement: the shell reads the program incrementally from a pipe, and
    /// keeping statements whole means a line is never half-parsed when the next is written.
    fn script(&self, command: &str) -> String {
        format!(
            concat!(
                "printf '\\000{tag}{stdout_start}\\000'\n",
                "printf '\\000{tag}{stderr_start}\\000' >&2\n",
                "eval -- {command} </dev/null\n",
                "__girder_ps_status=$?\n",
                "__girder_ps_cwd=$(printf '%s' \"$PWD\" | od -An -v -tx1 | tr -d ' \\n')\n",
                "printf '\\000{tag}{stdout_end}%s:%s\\000' \
                 \"$__girder_ps_status\" \"$__girder_ps_cwd\"\n",
                "printf '\\000{tag}{stderr_end}\\000' >&2\n",
            ),
            tag = self.tag,
            stdout_start = STDOUT_START,
            stderr_start = STDERR_START,
            stdout_end = STDOUT_END,
            stderr_end = STDERR_END,
            // Quoted so the command is one word of `eval`: spaces, quotes, redirections, and
            // newlines all survive as the command wrote them, and `eval` still interprets them.
            command = quote(command),
        )
    }
}

/// Builds one marker: a leading NUL, the tag, the suffix, and optionally a trailing NUL.
///
/// Built here rather than pasted in two places so the bytes searched for and the bytes printed are
/// the same by construction; a divergence between them would look like a hang until the timeout.
fn marker(tag: &str, suffix: &str, trailing_nul: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(tag.len() + suffix.len() + 2);
    bytes.push(0);
    bytes.extend_from_slice(tag.as_bytes());
    bytes.extend_from_slice(suffix.as_bytes());
    if trailing_nul {
        bytes.push(0);
    }
    bytes
}

/// A tag that no two shells share and no command can predict.
///
/// Process id and clock separate two shells in different processes; the counter separates two
/// started in the same nanosecond by the same process. Hashing them keeps the tag to hex digits, so
/// it is safe inside a `printf` format and cannot be read as a quoting problem by the shell.
fn unique_tag() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash as _, Hasher as _};
    use std::sync::atomic::{AtomicU64, Ordering};

    static STARTED: AtomicU64 = AtomicU64::new(0);

    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
        .hash(&mut hasher);
    STARTED.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);

    format!("{:016x}", hasher.finish())
}

/// Quotes `command` as one shell word.
///
/// Single quotes rather than the shell's `$'...'` form: single quoting is POSIX and needs no escape
/// processing, so a backslash or a `%` stays literal. The one character that cannot appear inside
/// quotes — `'` — is closed, escaped, and reopened.
fn quote(command: &str) -> String {
    format!("'{}'", command.replace('\'', "'\\''"))
}

/// Splits a completion payload into the exit code and the reported directory.
fn parse_payload(payload: &str) -> Result<(i32, Option<PathBuf>), ShellError> {
    let (code, cwd) = payload.split_once(':').ok_or_else(|| ShellError::Protocol {
        detail: format!("the completion payload `{payload}` has no `:`"),
    })?;
    let code = code
        .trim()
        .parse::<i32>()
        .map_err(|_| ShellError::Protocol {
            detail: format!("the completion payload `{payload}` has no exit status"),
        })?;

    // A directory that cannot be decoded leaves the remembered one in place: the status is what the
    // caller needs, and a decoded path is only tracking for `cwd()`.
    Ok((code, decode_hex_path(cwd)))
}

/// Decodes the hex-encoded working directory the shell reported.
fn decode_hex_path(hex: &str) -> Option<PathBuf> {
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }

    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(pair, 16).ok()?);
    }

    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Accumulates one stream of one command, stopping at that stream's end marker.
///
/// Two buffers rather than one: `body` is what the caller sees and is bounded, while `window` holds
/// only the bytes that could still be part of a marker. Without the split, finding a marker would
/// require keeping the whole stream, and a command with unbounded output would exhaust memory before
/// its marker arrived.
struct Frame {
    /// The retained output.
    body: Vec<u8>,
    /// Bytes dropped because the bound was reached.
    omitted: u64,
    /// Received bytes not yet classified: a possible partial marker, or a body prefix.
    window: Vec<u8>,
    /// Whether the start marker has been seen.
    started: bool,
    /// The payload of the end marker, once it has arrived.
    payload: Option<String>,
    /// How much output is retained.
    max: usize,
}

impl Frame {
    /// A frame that retains at most `max` bytes.
    fn new(max: usize) -> Self {
        Self {
            body: Vec::new(),
            omitted: 0,
            window: Vec::new(),
            started: false,
            payload: None,
            max,
        }
    }

    /// Whether the end marker arrived.
    fn finished(&self) -> bool {
        self.payload.is_some()
    }

    /// The end marker's payload, when it has arrived.
    fn payload(&self) -> Option<&str> {
        self.payload.as_deref()
    }

    /// Consumes the frame into its retained output and its dropped-byte count.
    fn finish(self) -> (String, u64) {
        (String::from_utf8_lossy(&self.body).into_owned(), self.omitted)
    }

    /// Classifies one chunk of the stream.
    ///
    /// The start marker's prefix is dropped rather than reported: anything before it belongs to the
    /// previous command (or to a background process) and attributing it here would mislead the caller
    /// about which command produced it.
    fn push(&mut self, chunk: &[u8], start: &[u8], end: &[u8]) {
        if self.payload.is_some() {
            return;
        }
        self.window.extend_from_slice(chunk);

        loop {
            if !self.started {
                let Some(at) = find(&self.window, start) else {
                    // Keep only a tail that could be the beginning of the marker; the rest is output
                    // from before this command, which this frame does not own.
                    keep_tail(&mut self.window, start.len().saturating_sub(1));
                    return;
                };
                self.started = true;
                self.window.drain(..at + start.len());
            }

            let Some(at) = find(&self.window, end) else {
                // No marker yet: everything except a possible partial marker is body. The retained
                // tail is one byte shorter than the marker, which is the longest prefix of it that
                // could still be completed by the next chunk.
                let keep = end.len().saturating_sub(1);
                if self.window.len() > keep {
                    let split = self.window.len() - keep;
                    self.append(split);
                }
                return;
            };

            // Indices are resolved against the window as it is now: `append` drains from the front,
            // so a payload offset computed after it would address the wrong bytes.
            let payload_start = at + end.len();
            match self.window[payload_start..].iter().position(|byte| *byte == 0) {
                Some(length) => {
                    let payload = String::from_utf8_lossy(
                        &self.window[payload_start..payload_start + length],
                    )
                    .into_owned();
                    self.append(at);
                    self.window.clear();
                    self.payload = Some(payload);
                    return;
                }
                None => {
                    // The marker prefix arrived but its NUL-terminated payload has not. The body
                    // before it is final, so it is committed while the rest waits.
                    self.append(at);
                    return;
                }
            }
        }
    }

    /// Moves the first `length` bytes of the window into the retained body.
    fn append(&mut self, length: usize) {
        let chunk = &self.window[..length];
        let room = self.max.saturating_sub(self.body.len());
        let take = chunk.len().min(room);
        self.body.extend_from_slice(&chunk[..take]);
        self.omitted += u64::try_from(chunk.len() - take).unwrap_or(u64::MAX);
        self.window.drain(..length);
    }
}

/// Finds `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Truncates `window` to its last `keep` bytes.
fn keep_tail(window: &mut Vec<u8>, keep: usize) {
    if window.len() > keep {
        let drop = window.len() - keep;
        window.drain(..drop);
    }
}

/// Drops output that arrived while no command was running.
fn discard_pending(receiver: &mut Option<mpsc::UnboundedReceiver<Vec<u8>>>) {
    let Some(receiver) = receiver.as_mut() else {
        return;
    };
    while receiver.try_recv().is_ok() {}
}

/// Waits for the next chunk, reporting a closed channel as `None`.
async fn next(receiver: &mut Option<mpsc::UnboundedReceiver<Vec<u8>>>) -> Option<Vec<u8>> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => None,
    }
}

/// Reads a pipe to its end, forwarding each chunk.
async fn drain(
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    sender: mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buffer).await {
            // End of stream and a read error are the same outcome here: the shell is gone, and the
            // consumer learns that from the closed channel rather than from an error code.
            Ok(0) | Err(_) => return,
            Ok(read) => {
                let Some(chunk) = buffer.get(..read) else {
                    return;
                };
                if sender.send(chunk.to_vec()).is_err() {
                    // The shell was closed while this task was reading.
                    return;
                }
            }
        }
    }
}

/// Makes the shell lead its own process group.
///
/// A timeout kills the group, and the group must be the shell's rather than the harness's: signalling
/// `-pid` where `pid` is not a group leader would signal whatever group it belongs to, which is the
/// harness itself.
fn lead_own_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

/// Kills the shell and everything it started.
fn kill_group(pid: Option<u32>) {
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
        // SIGKILL without a preceding SIGTERM: the shell's state is being discarded either way, so a
        // graceful stage would only delay the tool call, and a shell that traps or ignores SIGTERM
        // would need the SIGKILL regardless.
        let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}
