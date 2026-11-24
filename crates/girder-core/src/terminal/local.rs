//! A [`Shell`] backed by a real child process, and the [`TerminalLauncher`] that starts it.
//!
//! This is what makes the terminal tools usable on a host that has a shell: the crate's
//! [`crate::terminal`] module owns session bookkeeping and [`crate::terminal::tool`] owns the tool
//! surface, but neither owns a process. This module is that missing layer, and nothing else.
//!
//! # Unix runs a pty
//!
//! On Unix the child is given a pseudo-terminal: stdin, stdout, and stderr are the slave, the
//! parent reads and writes the master, and [`Shell::resize`] updates the kernel window size and
//! sends `SIGWINCH`. Programs that call `isatty` see a terminal. A host that is not Unix still gets
//! three pipes, with the limits that implies: no window size, no echo, and no job control.
//!
//! # Why `sh` without `-i`
//!
//! An interactive shell on a pipe prints a prompt to stderr for every command and, on some systems,
//! warns about job control it cannot offer. That noise goes into the transcript the model reads,
//! and buys nothing: state persists because it is one process reading many lines, not because the
//! shell was told it is interactive. So the shell runs plain, and commands are fed line by line.
//!
//! # Why stderr is merged into the read stream
//!
//! A pty merges them too — a person sees both in one window interleaved. Keeping them apart would
//! let a caller read stdout forever and never learn why a command failed, and [`Shell::read`] has no
//! way to say which channel a chunk came from. Errors are part of what a terminal printed.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use girder_core::{BoxFut, Shell, TerminalError, TerminalLaunch, TerminalLauncher, TerminalStatus};

/// The largest chunk one [`Shell::read`] returns.
///
/// One chunk per call, because the caller decides when to read again; returning everything that
/// arrived would let one call hold an arbitrarily large answer, which is the opposite of what the
/// wait bound is for.
const CHUNK: usize = 4096;

/// How long [`Shell::close`] waits at each step before escalating.
///
/// Long enough for a shell at a prompt to notice EOF and leave, short enough that closing a session
/// is not something a turn waits on.
const CLOSE_GRACE: Duration = Duration::from_millis(200);

/// How often [`Shell::close`] checks whether the process it signalled has gone.
///
/// A poll rather than an awaited `wait()` because the handle is behind a synchronous mutex: holding
/// it across an await would make the returned future non-`Send`, which [`BoxFut`] does not allow.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How hard [`Shell::close`] asks a process to stop.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// Ask it to leave, so a process with cleanup can run it.
    Graceful,
    /// Force it, because the ask was ignored.
    Forced,
}

/// One chunk that was ready to read, and which pipe it came from.
enum Ready {
    /// The child's standard output.
    Stdout(std::io::Result<usize>),
    /// The child's standard error.
    Stderr(std::io::Result<usize>),
}

/// A shell running as a child process.
///
/// On Unix the child is attached to a pty. Elsewhere it is attached to pipes.
///
/// Construct it with [`ProcessShell::start`], or let [`LocalLauncher`] do it as part of mounting the
/// terminal tools. The trait methods are the interface; everything here is a process plus the pipes
/// to it.
///
/// Dropping the shell kills the child (`kill_on_drop`), but [`Shell::close`] is the correct way to
/// end a session: it stops the whole process group and lets a shell exit on its own first.
pub struct ProcessShell {
    /// Everything the `'static` futures of the trait methods need.
    ///
    /// An `Arc` rather than plain fields because [`Shell`] returns `BoxFut`, whose futures outlive
    /// the `&self` borrow they were created from; each method clones this and moves it in.
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ProcessShell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessShell")
            .field("id", &self.inner.id)
            .field("alive", &self.inner.is_alive())
            .finish_non_exhaustive()
    }
}

impl ProcessShell {
    /// Starts the shell `spec` names, or the host default when it names none.
    ///
    /// The program is a path or a name looked up on `PATH`; it is not a command line, so a value
    /// containing spaces is treated as one file name and will fail to start. That keeps the launch
    /// free of a shell-quoting layer whose rules nothing here could enforce.
    ///
    /// The environment is inherited rather than cleared: a terminal is somebody's shell, and a
    /// shell without `PATH`, `HOME`, or the session's variables is not the shell they asked for.
    ///
    /// # Errors
    ///
    /// Returns a reason naming the program when it cannot be started: missing, not executable, or
    /// with a working directory that does not exist. The reason is meant to be shown to a model, and
    /// does not repeat the session id — the tool wraps it as "cannot start terminal `<id>`" once.
    pub fn start(spec: &TerminalLaunch) -> Result<Self, String> {
        #[cfg(unix)]
        {
            Self::start_pty(spec)
        }
        #[cfg(not(unix))]
        {
            Self::start_pipes(spec)
        }
    }

    /// Starts the shell on three pipes.
    ///
    /// Used where a pty is not available. The size is exported as `COLUMNS` and `LINES` because
    /// there is no terminal device to query.
    #[cfg(not(unix))]
    fn start_pipes(spec: &TerminalLaunch) -> Result<Self, String> {
        let program = spec.shell.clone().unwrap_or_else(default_shell);
        let mut command = Command::new(&program);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command
            .env("COLUMNS", spec.size.columns.to_string())
            .env("LINES", spec.size.rows.to_string());

        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start `{program}`: {error}"))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        Ok(Self {
            inner: Arc::new(Inner {
                id: spec.id.clone(),
                stdin: tokio::sync::Mutex::new(stdin),
                streams: tokio::sync::Mutex::new(Streams { stdout, stderr }),
                state: Mutex::new(ChildState {
                    child,
                    pid,
                    exit: ExitState::Running,
                }),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Starts the shell on a pty.
    ///
    /// The slave becomes the child's controlling terminal. The parent keeps the master.
    #[cfg(unix)]
    fn start_pty(spec: &TerminalLaunch) -> Result<Self, String> {
        use std::os::fd::AsRawFd;

        let program = spec.shell.clone().unwrap_or_else(default_shell);
        let (master, slave) = super::pty_io::open_pty(spec.size.columns, spec.size.rows)
            .map_err(|error| format!("could not open a pty for `{program}`: {error}"))?;
        let reader = master
            .try_clone()
            .map_err(|error| format!("could not open a pty for `{program}`: {error}"))?;
        let slave_fd = slave.as_raw_fd();
        let master_fd = master.as_raw_fd();

        let mut command = Command::new(&program);
        command.kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command
            .env("TERM", "xterm-256color")
            .env("COLUMNS", spec.size.columns.to_string())
            .env("LINES", spec.size.rows.to_string());
        super::pty_io::prepare_child(&mut command, slave_fd, master_fd);

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Err(format!("could not start `{program}`: {error}"));
            }
        };
        drop(slave);
        let pid = child.id();
        let (chunks_tx, chunks_rx) = tokio::sync::mpsc::channel(32);
        std::thread::Builder::new()
            .name("girder-pty".to_owned())
            .spawn(move || read_pty(reader, chunks_tx))
            .map_err(|error| format!("could not read the pty for `{program}`: {error}"))?;

        Ok(Self {
            inner: Arc::new(Inner {
                id: spec.id.clone(),
                stdin: tokio::sync::Mutex::new(None),
                streams: tokio::sync::Mutex::new(Streams {
                    stdout: None,
                    stderr: None,
                }),
                pty: Some(PtyMaster {
                    master: Mutex::new(master),
                    chunks: tokio::sync::Mutex::new(chunks_rx),
                }),
                state: Mutex::new(ChildState {
                    child,
                    pid,
                    exit: ExitState::Running,
                }),
                closed: AtomicBool::new(false),
            }),
        })
    }
}

impl Shell for ProcessShell {
    fn write(&self, input: &str) -> BoxFut<Result<(), TerminalError>> {
        let inner = Arc::clone(&self.inner);
        let input = input.to_owned();
        Box::pin(async move { inner.write(&input).await })
    }

    fn read(&self, wait: Duration) -> BoxFut<Result<Option<String>, TerminalError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            // The caller's bound, not ours: a tool call is holding a turn open, and "nothing arrived
            // yet" is the normal answer for a shell sitting at a prompt.
            match tokio::time::timeout(wait, inner.next_chunk()).await {
                // A chunk arrived.
                Ok(Some(text)) => Ok(Some(text)),
                // Both pipes are closed: no output can ever arrive again. Reported as "nothing" by
                // the same `None` the timeout uses, because the caller distinguishes the two cases
                // with `is_alive`, which is the cheaper and more reliable question.
                Ok(None) => Ok(None),
                // The bound expired with the streams still open.
                Err(_elapsed) => Ok(None),
            }
        })
    }

    fn close(&self) -> BoxFut<Result<(), TerminalError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            inner.close().await;
            Ok(())
        })
    }

    fn resize(&self, columns: u16, rows: u16) -> BoxFut<Result<(), TerminalError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.resize(columns, rows) })
    }

    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    fn exit_code(&self) -> Option<i32> {
        self.inner.exit_code()
    }
}

/// The state a running shell shares with the futures its trait methods return.
///
/// One struct behind one [`Arc`] so a method clones a single handle instead of four, and so the
/// parts that must agree — the child, its pid, and the exit it was observed to have — stay together
/// under one lock.
struct Inner {
    /// The session id, used only to build the errors the trait makes us report.
    id: String,
    /// Where commands are written.
    ///
    /// A `tokio` mutex because the guard is held across `write_all().await`, and an async-aware
    /// guard is the only kind that may cross an await point in a `Send` future. Closing sets it to
    /// `None`, which drops the pipe: a late write then fails instead of signalling a process the
    /// caller believes is gone.
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    /// The child's two output pipes, read together.
    streams: tokio::sync::Mutex<Streams>,
    /// The pty master, when this shell was started on Unix.
    #[cfg(unix)]
    pty: Option<PtyMaster>,
    /// The child and what is known about its exit.
    ///
    /// A `std` mutex because [`Shell::is_alive`] is synchronous and must not wait behind a read that
    /// is holding the streams lock.
    state: Mutex<ChildState>,
    /// Set when [`Inner::close`] starts, so a later write cannot succeed against a dead terminal.
    closed: AtomicBool,
}

/// The child's output pipes, in the shape the pump needs.
struct Streams {
    /// Standard output; `None` once it reached end of file.
    stdout: Option<ChildStdout>,
    /// Standard error; `None` once it reached end of file.
    stderr: Option<ChildStderr>,
}

/// What is known about a child's exit.
///
/// An enum rather than an `Option<Option<i32>>`: "still running" and "exited without a code" are
/// different answers, and a nested option would make them indistinguishable at the call site that
/// has to report one of them to a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitState {
    /// No exit has been observed, so the child may still be running.
    Running,
    /// The child was seen to be gone; the code is `None` when a signal ended it.
    Exited(Option<i32>),
}

/// The child and the exit it was observed to have.
struct ChildState {
    /// The process.
    child: Child,
    /// Its pid, kept because the group is signalled by pid and `Child::id` stops answering once the
    /// child is reaped.
    pid: Option<u32>,
    /// The exit, once one was observed.
    exit: ExitState,
}

/// The parent's end of a pty, plus the chunks a reader thread has already pulled off it.
#[cfg(unix)]
struct PtyMaster {
    /// Writes and window-size changes.
    master: Mutex<std::fs::File>,
    /// Output the reader thread has copied off the master.
    chunks: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Option<Vec<u8>>>>,
}

#[cfg(unix)]
impl PtyMaster {
    /// Writes `bytes` without holding the async runtime inside the kernel write.
    async fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut file = self
            .master
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_clone()?;
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            file.write_all(&bytes)
        })
        .await
        .map_err(|error| std::io::Error::other(error))?
    }
}

/// Copies pty output onto a channel until the slave hangs up.
#[cfg(unix)]
fn read_pty(mut reader: std::fs::File, chunks: tokio::sync::mpsc::Sender<Option<Vec<u8>>>) {
    use std::io::Read;
    let mut buffer = [0u8; CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = chunks.blocking_send(None);
                break;
            }
            Ok(count) => {
                if chunks
                    .blocking_send(Some(buffer[..count].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                let _ = chunks.blocking_send(None);
                break;
            }
        }
    }
}

impl Inner {
    /// Writes `input` to the shell's stdin.
    async fn write(&self, input: &str) -> Result<(), TerminalError> {
        if self.closed.load(Ordering::Relaxed) || !self.is_alive() {
            return Err(self.not_running());
        }
        #[cfg(unix)]
        if let Some(pty) = &self.pty {
            return pty
                .write_all(input.as_bytes())
                .await
                .map_err(|_| self.not_running());
        }
        let mut stdin = self.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(self.not_running());
        };
        // `write_all` may wait when the child is not reading; that is backpressure, not a hang: the
        // input is queued in the pipe rather than dropped.
        pipe.write_all(input.as_bytes())
            .await
            .map_err(|_error| self.not_running())?;
        pipe.flush().await.map_err(|_error| self.not_running())?;
        Ok(())
    }

    /// Changes the pty's window size and tells the foreground group.
    fn resize(&self, columns: u16, rows: u16) -> Result<(), TerminalError> {
        #[cfg(unix)]
        if let Some(pty) = &self.pty {
            let file = pty.master.lock().unwrap_or_else(PoisonError::into_inner);
            super::pty_io::set_window_size(&file, columns, rows).map_err(|_| self.not_running())?;
            self.signal_window();
            return Ok(());
        }
        #[cfg(not(unix))]
        let _ = (columns, rows);
        Ok(())
    }

    /// Tells the child's process group that the window changed.
    #[cfg(unix)]
    fn signal_window(&self) {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let Some(pid) = self.pid().and_then(|pid| i32::try_from(pid).ok()) else {
            return;
        };
        let _ = kill(Pid::from_raw(-pid), Signal::SIGWINCH);
    }

    /// Returns the next chunk either pipe produced, or `None` when both are closed.
    async fn next_chunk(&self) -> Option<String> {
        #[cfg(unix)]
        if let Some(pty) = &self.pty {
            let mut chunks = pty.chunks.lock().await;
            return chunks.recv().await.flatten().map(|bytes| text(&bytes));
        }
        let mut guard = self.streams.lock().await;
        // Rebinding through one deref is what lets the two fields be borrowed at once: reached
        // through the guard, each field access would re-borrow `*guard` mutably and the pair would
        // conflict.
        let streams = &mut *guard;
        loop {
            // Two buffers: `select!` cannot mutably borrow one buffer for two readers.
            let mut out_buffer = [0u8; CHUNK];
            let mut err_buffer = [0u8; CHUNK];
            let ready = match (streams.stdout.as_mut(), streams.stderr.as_mut()) {
                // Reading both at once is what stops a chatty stderr from starving stdout, and why
                // a child that fills the stderr pipe cannot deadlock against a caller that only
                // ever asks for stdout. `AsyncReadExt::read` is cancel-safe, so the loser of the
                // race loses no bytes.
                (Some(out), Some(err)) => tokio::select! {
                    result = out.read(&mut out_buffer) => Ready::Stdout(result),
                    result = err.read(&mut err_buffer) => Ready::Stderr(result),
                },
                (Some(out), None) => Ready::Stdout(out.read(&mut out_buffer).await),
                (None, Some(err)) => Ready::Stderr(err.read(&mut err_buffer).await),
                // Nothing left to read, and no future chunk can arrive.
                (None, None) => return None,
            };

            match ready {
                // A closed pipe is dropped from the set rather than polled forever: a pending read
                // on end-of-file returns immediately, which would spin this loop.
                Ready::Stdout(Ok(0)) => streams.stdout = None,
                Ready::Stderr(Ok(0)) => streams.stderr = None,
                Ready::Stdout(Ok(n)) => return Some(text(&out_buffer[..n])),
                Ready::Stderr(Ok(n)) => return Some(text(&err_buffer[..n])),
                // A read error is treated as end of that stream. There is no error a caller could act
                // on — `TerminalError` has no transport variant — and reporting the failure as output
                // would put invented text in the transcript.
                Ready::Stdout(Err(_error)) => streams.stdout = None,
                Ready::Stderr(Err(_error)) => streams.stderr = None,
            }
        }
    }

    /// Ends the session, taking the whole process group with it.
    async fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        #[cfg(unix)]
        if let Some(pty) = &self.pty {
            // Ctrl-D is EOF at a prompt. Closing the master instead would hang the terminal up
            // before the shell could exit on its own.
            let _ = pty.write_all(&[0x04]).await;
        }
        #[cfg(not(unix))]
        {
            let mut stdin = self.stdin.lock().await;
            let _taken = stdin.take();
        }
        #[cfg(unix)]
        if self.pty.is_none() {
            let mut stdin = self.stdin.lock().await;
            let _taken = stdin.take();
        }
        // Give the shell a chance to leave on its own so its exit code is a real one rather than a
        // signal. The group is swept afterwards either way: a shell that exits on EOF does not take
        // its background children with it, and those children are still holding the pipes.
        let _ = self.wait_gone(CLOSE_GRACE).await;

        // Something may still be running: `sleep`, a server, or a background job that outlived its
        // shell. Ask, then force.
        if !self.group_alive() {
            return;
        }
        self.signal(Stop::Graceful);
        if self.wait_group_gone(CLOSE_GRACE).await {
            return;
        }
        self.signal(Stop::Forced);
        // Bounded on purpose: a process that will not die must not hold the closing call open.
        // `kill_on_drop` reaps the shell when it is dropped, so nothing is leaked past that.
        self.wait_group_gone(CLOSE_GRACE).await;
    }

    /// Whether the child is still running.
    fn is_alive(&self) -> bool {
        self.observe_exit() == ExitState::Running
    }

    /// The code the child exited with, once it is gone.
    fn exit_code(&self) -> Option<i32> {
        match self.observe_exit() {
            ExitState::Running => None,
            ExitState::Exited(code) => code,
        }
    }

    /// Observes whether the child has exited, reaping it if it has.
    ///
    /// The answer is cached: reaping happens once, so a second observation must not have to
    /// re-derive a status that no longer exists.
    fn observe_exit(&self) -> ExitState {
        let mut state = self.lock_state();
        if state.exit != ExitState::Running {
            return state.exit;
        }
        match state.child.try_wait() {
            Ok(Some(status)) => {
                state.exit = ExitState::Exited(status.code());
                state.exit
            }
            // A probe that failed says nothing about liveness, and inventing death from it would
            // report a working terminal as gone. Assuming it is alive matches the trait's own
            // default, which is the answer that changes nothing.
            Ok(None) | Err(_) => ExitState::Running,
        }
    }

    /// Sends `stop` to the child's process group.
    fn signal(&self, stop: Stop) {
        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;

            let Some(pid) = self
                .lock_state()
                .pid
                .and_then(|pid| i32::try_from(pid).ok())
            else {
                // No pid means no group to signal: the child is already reaped, or its id did not
                // fit, which cannot happen for a real pid.
                return;
            };
            let signal = match stop {
                Stop::Graceful => Signal::SIGTERM,
                Stop::Forced => Signal::SIGKILL,
            };
            // Negative pid addresses the group. A shell with job control puts background
            // commands in other groups of the same session, so those are signalled too.
            let _ = kill(Pid::from_raw(-pid), signal);
            for member in session_members(pid) {
                let _ = kill(Pid::from_raw(member), signal);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = stop;
            // No process groups to signal here; killing the child is the whole of what is portable.
            let mut state = self.lock_state();
            let _ = state.child.start_kill();
        }
    }

    /// Waits up to `within` for the child to be gone, reaping it if it leaves.
    async fn wait_gone(&self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if !self.is_alive() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Whether any process in the child's group is still alive.
    ///
    /// The group rather than the child, because a shell that exited can leave children in the group
    /// it created — a backgrounded job, a pipeline whose reader is gone. Those are what "closed"
    /// has to mean, and a check on the shell alone would report a terminal as closed while its
    /// command was still running.
    ///
    /// `kill(-pgid, 0)` delivers nothing and asks only whether the group has a member. The group id
    /// cannot be recycled while the group has members, so this cannot mistake a later process for
    /// the one it is watching.
    fn group_alive(&self) -> bool {
        #[cfg(unix)]
        {
            use nix::errno::Errno;
            use nix::sys::signal::kill;
            use nix::unistd::Pid;

            // A reaped child still keeps its group id reserved for as long as the group has
            // members, so the pid stays the right name for the group even after `try_wait`.
            let Some(pid) = self.pid() else {
                return self.is_alive();
            };
            let Some(pid) = i32::try_from(pid).ok() else {
                return self.is_alive();
            };
            let group = match kill(Pid::from_raw(-pid), None) {
                // A group with any member answers to the null signal. `EPERM` means it exists but
                // belongs to somebody else, which still counts as alive for deciding whether to
                // signal it.
                Ok(()) | Err(Errno::EPERM) => true,
                Err(_) => false,
            };
            group
                || session_members(pid)
                    .iter()
                    .any(|member| kill(Pid::from_raw(*member), None).is_ok())
        }
        #[cfg(not(unix))]
        {
            self.is_alive()
        }
    }

    /// Waits up to `within` for the whole group to be gone.
    async fn wait_group_gone(&self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            // Reap the shell on every pass: a zombie leader would otherwise keep the group's entry
            // alive and make the wait run to its bound.
            let _ = self.observe_exit();
            if !self.group_alive() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// The pid the group was created under.
    fn pid(&self) -> Option<u32> {
        self.lock_state().pid
    }

    /// The lock, recovering from a poisoned mutex.
    ///
    /// Nothing under these locks can panic, so poisoning would be a bug in this module, and the
    /// alternative — failing every later call — would turn one lost wakeup into a dead session.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, ChildState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The error a transport failure is reported as.
    ///
    /// [`TerminalError`] has no transport variant, so a broken pipe is reported as a session that is
    /// no longer running. That is accurate for the only ordinary cause — the shell exited and closed
    /// its stdin — and claiming a spawn failure would be worse.
    fn not_running(&self) -> TerminalError {
        TerminalError::NotRunning {
            id: self.id.clone(),
            status: TerminalStatus::Exited { code: None }.as_str(),
        }
    }
}

/// Pids that share the session led by `leader`, excluding the leader itself.
///
/// Job control puts background commands in other process groups of that session. `/proc` is the
/// list Linux already keeps. A host without it contributes nothing, and the process-group signal
/// remains the whole of the sweep.
#[cfg(unix)]
fn session_members(leader: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|pid| *pid != leader && session_of(*pid) == Some(leader))
        .collect()
}

/// The session id recorded for `pid`, when `/proc` has one.
#[cfg(unix)]
fn session_of(pid: i32) -> Option<i32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` can contain spaces and parentheses. The session id is the fourth field after the
    // closing parenthesis of that name.
    let rest = text.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let _parent = fields.next()?;
    let _group = fields.next()?;
    fields.next()?.parse().ok()
}

/// The default program for a shell a launch does not name.
///
/// `$SHELL` first, because that is the shell the person running the harness chose, then `sh`, which
/// POSIX guarantees. An empty or whitespace-only value is ignored rather than attempted.
fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .map(|shell| shell.trim().to_owned())
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "sh".to_owned())
}

/// Decodes one chunk of output.
///
/// Lossy rather than strict: a program may print a byte sequence that is not UTF-8, and failing the
/// read would turn a mojibake character into a dead session.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Starts [`ProcessShell`]s on the host this process runs on.
///
/// This is the launcher to mount in [`crate::terminal::tool`] in place of `UnavailableLauncher`: it
/// gives the model a terminal that really runs commands. On Unix that terminal is a pty.
///
/// The fallback shell is captured when the launcher is built, so a session started later behaves
/// like the environment the harness was started in rather than like whatever `$SHELL` says by then.
#[derive(Clone, Debug)]
pub struct LocalLauncher {
    /// The program run when a launch names no shell.
    shell: String,
}

impl LocalLauncher {
    /// Builds a launcher that runs the host's default shell.
    #[must_use]
    pub fn new() -> Self {
        Self::with_shell(default_shell())
    }

    /// Builds a launcher that runs `shell` when a launch names none.
    ///
    /// Useful for pinning the shell to something known (`bash`, `/bin/sh`) instead of inheriting the
    /// ambient `$SHELL`, which makes a deployment's behaviour independent of who started it.
    #[must_use]
    pub fn with_shell(shell: impl Into<String>) -> Self {
        Self {
            shell: shell.into(),
        }
    }
}

impl Default for LocalLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalLauncher for LocalLauncher {
    fn launch(&self, spec: TerminalLaunch) -> BoxFut<Result<Arc<dyn Shell>, String>> {
        // The launcher's shell is a fallback, not an override: a launch that names a program gets
        // that program, because the caller asked for it.
        let mut spec = spec;
        if spec.shell.is_none() {
            spec.shell = Some(self.shell.clone());
        }
        Box::pin(async move {
            let shell = ProcessShell::start(&spec)?;
            Ok(Arc::new(shell) as Arc<dyn Shell>)
        })
    }
}
