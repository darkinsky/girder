//! Process-group containment for commands that may leave descendants behind.
//!
//! [`crate::subprocess`] already spawns a child as a process-group leader and kills `-pgid` on
//! timeout, so a pipeline such as `sh -c "sleep 100 | cat"` is torn down as a unit. What it does
//! not do is guarantee that the whole group is gone *after a command that exited successfully*.
//! Two gaps follow from that, and both are demonstrated by this module's test file:
//!
//! 1. **A background child survives a successful command.** `sh -c "sleep 999 &"` exits at once,
//!    while the `sleep` keeps the group alive. The normal-completion path never signals anything,
//!    so that process is left running, owned by the harness.
//! 2. **Draining to EOF blocks on that survivor.** The pipe stays open in the background child, so
//!    a read after the leader was reaped returns only when the survivor exits. Measured with the
//!    same spawn shape `subprocess::run` uses, `sleep 3 &` delayed the drain by 3.00s: the command
//!    "finished" in 2.6ms and the caller still waited three seconds. With `sleep 999 &` that wait
//!    is unbounded, and it lands on whatever timeout the caller wrapped around the whole call.
//!
//! This module closes both by making group membership explicit and by tearing the group down
//! before the drain can block.
//!
//! # Why a group and not a tree walk
//!
//! Reconstructing a process tree from `/proc` loses processes that re-parent to init, and it races
//! with `fork`. A process group is a kernel-maintained set: `kill(-pgid, …)` reaches every member,
//! including ones whose intermediate parents have already exited. The price is that the group id
//! must not be one we share — see [`GroupGuard::attach`].
//!
//! # Signal escalation
//!
//! SIGTERM first so a well-behaved process can clean up, then SIGKILL after a short grace period.
//! Escalation is unconditional rather than conditional on a second membership observation: a
//! process can be mid-exit when observed and still running when the caller returns, and a SIGKILL
//! that finds nothing has already achieved what it wanted.
//!
//! # Linux containment, for contrast
//!
//! Upstream reaches for a transient systemd scope when the user manager answers
//! (`systemd-run --user --scope --collect`, killed with `systemctl --user kill --kill-who=all`),
//! and falls back to signalling a spawn group when it does not. A cgroup is strictly stronger than
//! a process group: it also holds double-forks and a `setsid()` inside the command. It needs a user
//! manager and a `systemctl` round trip per teardown. This build uses the process group, which is
//! the same fallback upstream keeps, and does not claim cgroup strength: a command that calls
//! `setsid()` escapes this containment, exactly as it escapes upstream's fallback.
//!
//! # Portability
//!
//! POSIX only, like the rest of the execution layer: group signalling and `/proc` inspection are
//! the mechanism, and neither exists on Windows. [`crate::subprocess`] makes the same tradeoff for
//! the same reason, rather than shipping a Windows tree kill it cannot test.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

/// How long a group gets to exit after `SIGTERM` before it is `SIGKILL`ed.
///
/// Long enough for a shell to run an `EXIT` trap and for a `sleep` to notice the signal, short
/// enough that a command ignoring SIGTERM cannot stall a timeout by more than this.
const GRACE: Duration = Duration::from_millis(50);

/// How long the final output drain may take once the group has been killed.
///
/// The drain is expected to finish immediately, because every writer in the group is dead by then.
/// The bound exists for the one writer that is not in the group: a descendant that escaped through
/// `setsid()` and still holds the pipe. Waiting on it forever would reintroduce the hang this
/// module exists to remove, so such a survivor is abandoned with whatever output arrived.
const DRAIN: Duration = Duration::from_secs(5);

/// How long [`GroupGuard::record`] waits for the child to become a group leader.
///
/// The parent can return from `spawn` before the child has run its `setpgid`, so the group id is
/// polled rather than read once. The window is a few scheduler ticks; the bound keeps a pathological
/// child from stalling the caller.
const ESTABLISH: Duration = Duration::from_millis(200);

/// How `record` found the child's process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Establishment {
    /// The child ran `setpgid(0, 0)`; its pid is a group id of its own.
    Leader,
    /// The child is gone or a zombie, so the group it led is already empty.
    Departed,
    /// The child is alive but still in another group: `setpgid` failed or was overridden.
    Shared,
}

/// A failure raised while running a command in its own process group.
#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    /// The program could not be started, isolated, or reaped.
    #[error("failed to run `{program}`: {source}")]
    Spawn {
        /// The program that failed.
        program: String,
        /// The underlying error.
        source: std::io::Error,
    },

    /// The command exceeded its timeout and its group was killed.
    #[error("`{program}` timed out after {seconds}s; its process group was killed")]
    Timeout {
        /// The program that timed out.
        program: String,
        /// The configured limit, in seconds.
        seconds: u64,
    },
}

/// A freshly created process group, owned by one command.
///
/// The guard exists so that a group id is never signalled by accident. The id is the child's pid,
/// and the child is a group *leader* only because it was spawned with `setpgid(0, 0)`; had that
/// failed, the child would still be in the harness's group and `kill(-pid, …)` would signal the
/// harness itself. [`GroupGuard::attach`] therefore captures the harness's own group id, and
/// [`GroupGuard::record`] refuses to arm the guard when the child did not leave it.
///
/// Cloning copies the verified group id; both copies signal the same group, so a clone is only
/// useful for moving the kill onto a blocking thread.
#[derive(Debug, Clone)]
pub struct GroupGuard {
    /// The verified group id, or `None` when the child could not be isolated. `None` disables
    /// killing entirely, which is the safe direction: no group is better than the wrong one.
    group: Option<u32>,
    /// The harness's own process-group id, captured at [`GroupGuard::attach`] time.
    own_group: u32,
}

impl GroupGuard {
    /// Places `command` in a new process group and returns a guard for it.
    ///
    /// The command must not set its own `process_group` afterwards: the guard's kills use the
    /// recorded pid as a group id, and a caller-chosen group would make that a signal to strangers.
    ///
    /// # Errors
    ///
    /// Returns the io error when the current process group cannot be read. Without it the guard
    /// cannot tell its own group from the child's, and must not signal anything at all.
    pub fn attach(command: &mut Command) -> std::io::Result<Self> {
        let own_group = own_group_id()?;
        // `0` does not mean "no group": the standard spawn contract reads it as "put the child in
        // a new group whose id is its own pid", which is the leader `kill(-pid)` needs.
        command.process_group(0);
        Ok(Self {
            group: None,
            own_group,
        })
    }

    /// Records a spawned child's pid so its group can be signalled later.
    ///
    /// The pid is verified before it is trusted. A pid that is still in the harness's group is
    /// what a failed `setpgid` looks like from here, and that group is the one id that must never
    /// be negated; the guard is left unarmed in that case, so a caller passing the wrong number
    /// disables cleanup instead of aiming a signal at the harness.
    ///
    /// Verification polls, so it yields: a caller on an async runtime should run it inside
    /// [`tokio::task::spawn_blocking`] rather than block a reactor thread for up to [`ESTABLISH`].
    pub fn record(&mut self, pid: u32) {
        if pid <= 1 || pid == self.own_group {
            return;
        }
        match establish(pid) {
            // `Departed` is recorded too: the group is empty, `kill(-pid)` is a no-op, and letting
            // the caller proceed keeps a fast-exiting command (the common case) working. The pid
            // cannot be recycled into a *group leader* inside this microsecond window.
            Establishment::Leader | Establishment::Departed => self.group = Some(pid),
            Establishment::Shared => {}
        }
    }

    /// The verified group id, or `None` when the child could not be isolated.
    #[must_use]
    pub fn group_id(&self) -> Option<u32> {
        self.group
    }

    /// Kills the whole group: SIGTERM, a short grace period, then SIGKILL.
    ///
    /// Safe to call repeatedly and safe to call on an unarmed guard, which does nothing. Delivery
    /// failures are ignored because the end state worth wanting is "no such group", and that is
    /// exactly what a failed delivery to an empty group reports.
    pub fn kill_all(&self) {
        let Some(group) = self.group else {
            return;
        };
        send(group, nix::sys::signal::Signal::SIGTERM);
        std::thread::sleep(GRACE);
        send(group, nix::sys::signal::Signal::SIGKILL);
    }

    /// Whether any member of the group is still running.
    ///
    /// Membership is read from `/proc` rather than inferred from the recorded pid, so a descendant
    /// forked after [`Self::record`] still counts. A process we may not signal still exists and is
    /// reported: the question is "is anyone left", not "may I kill them".
    #[must_use]
    pub fn any_alive(&self) -> bool {
        self.group
            .is_some_and(|group| !group_members(group, self.own_group).is_empty())
    }
}

/// Runs one command in its own process group and collects its output.
///
/// On exit — whether the command succeeded, failed, or timed out — the whole group is killed before
/// the pipes are drained. That order is what makes a background child harmless: killing it closes
/// its copies of the pipe write ends, so the drain sees EOF instead of waiting for a survivor.
///
/// # Errors
///
/// Returns [`GroupError::Timeout`] when `timeout` expires, after killing the group, and
/// [`GroupError::Spawn`] when the program cannot be started, cannot be isolated into its own
/// group, or cannot be reaped.
pub async fn run_isolated(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output, GroupError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A child dropped without being waited for must not outlive its caller.
        .kill_on_drop(true);

    let guard = GroupGuard::attach(&mut command).map_err(|source| spawn_error(program, source))?;
    let mut child = command
        .spawn()
        .map_err(|source| spawn_error(program, source))?;

    // `id()` is `None` only once the child has been reaped, which cannot happen before the guard
    // runs; take the pid now, because the guard needs it before the pipes are moved out.
    let Some(pid) = child.id() else {
        // Unreachable in practice, but a panic here would abort the harness over a state the
        // runtime is allowed to produce. Reap the child instead of leaking it.
        let _ = child.start_kill();
        let status = child
            .wait()
            .await
            .map_err(|source| spawn_error(program, source))?;
        return Ok(std::process::Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    };

    // Verification polls for up to `ESTABLISH`, so it belongs on a blocking thread: the harness
    // shares one reactor across every session, and a sub-second stall there delays unrelated work.
    let guard = tokio::task::spawn_blocking(move || {
        let mut guard = guard;
        guard.record(pid);
        guard
    })
    .await
    .map_err(|source| spawn_error(program, std::io::Error::other(source)))?;

    if guard.group_id().is_none() {
        // Without a verified group this function is a worse `subprocess::run`: it could not clean
        // up its own descendants. Refuse instead of pretending, but do not leak the child.
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(GroupError::Spawn {
            program: program.to_owned(),
            source: std::io::Error::other(
                "the spawned child did not become a process-group leader, so its group was not contained",
            ),
        });
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // Drain both pipes while waiting. A pipe whose reader is not pumped fills at 64 KiB and blocks
    // the writer, so waiting first would deadlock a command with enough output.
    let mut stdout_task = Some(tokio::spawn(drain(stdout_pipe)));
    let mut stderr_task = Some(tokio::spawn(drain(stderr_pipe)));

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    // The wait future borrows `child` for as long as the `select!` lives, so the timer branch must
    // not touch the child itself. It records which side won and the work happens after the borrow
    // ends: reaping still needs `child`, and that is only available once the select is over.
    let finished = tokio::select! {
        biased;
        () = &mut deadline => None,
        status = child.wait() => Some(status),
    };

    let status = if let Some(status) = finished {
        status.map_err(|source| spawn_error(program, source))?
    } else {
        // The group, not just the leader: the leader may have exited already, or may have spawned the very
        // background work that a leader-only kill would strand.
        //
        // `kill_all` sleeps through the `SIGTERM` grace period, so it runs on a blocking thread: the harness
        // shares one reactor, and a stall there delays unrelated sessions.
        kill_off_thread(&guard).await;
        let _ = tokio::time::timeout(DRAIN, child.wait()).await;
        abort(stdout_task.take(), stderr_task.take()).await;
        return Err(GroupError::Timeout {
            program: program.to_owned(),
            seconds: timeout.as_secs(),
        });
    };

    // The leader is reaped, but a successful command can still leave descendants in the group —
    // and those descendants hold the pipe write ends open. Kill them before draining, or the drain
    // below waits for their natural exit. Skipped when the group is already empty, so the common
    // case does not pay the grace period.
    if guard.any_alive() {
        kill_off_thread(&guard).await;
    }

    Ok(std::process::Output {
        status,
        stdout: collect(stdout_task.take()).await,
        stderr: collect(stderr_task.take()).await,
    })
}

/// Runs [`GroupGuard::kill_all`] on a blocking thread.
///
/// The guard is cloned rather than moved so the caller keeps its handle: `kill_all` needs only the
/// group id, and the grace period is a sleep that must not occupy a reactor thread.
async fn kill_off_thread(guard: &GroupGuard) {
    let guard = guard.clone();
    // A panicking blocking task would leave the group alive; the clone makes that impossible in
    // practice, and a failed join is still not worth propagating from a teardown path.
    let _ = tokio::task::spawn_blocking(move || guard.kill_all()).await;
}

/// Reads a pipe to EOF; `None` yields no bytes.
async fn drain<R>(pipe: Option<R>) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buffer).await;
    }
    buffer
}

/// Awaits one drain task, bounded by [`DRAIN`].
///
/// A task exceeding the bound belongs to a writer outside the group, which nothing here can kill;
/// abandoning it keeps the caller from hanging on a process it does not own.
async fn collect(task: Option<tokio::task::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    let Some(task) = task else {
        return Vec::new();
    };
    match tokio::time::timeout(DRAIN, task).await {
        Ok(Ok(buffer)) => buffer,
        // The task was aborted or panicked; there is no output to report either way.
        Ok(Err(_cancelled)) => Vec::new(),
        Err(_timed_out) => Vec::new(),
    }
}

/// Aborts both drain tasks and waits for them, so no task outlives the call.
async fn abort(
    stdout: Option<tokio::task::JoinHandle<Vec<u8>>>,
    stderr: Option<tokio::task::JoinHandle<Vec<u8>>>,
) {
    for task in [stdout, stderr].into_iter().flatten() {
        task.abort();
        let _ = task.await;
    }
}

/// Wraps a spawn or reap failure with the program it belongs to.
fn spawn_error(program: &str, source: std::io::Error) -> GroupError {
    GroupError::Spawn {
        program: program.to_owned(),
        source,
    }
}

/// The harness's own process-group id.
fn own_group_id() -> std::io::Result<u32> {
    nix::unistd::getpgrp()
        .as_raw()
        .try_into()
        .map_err(|_| std::io::Error::other("the current process group id does not fit in u32"))
}

/// Classifies `pid`'s process group, polling until the child's `setpgid` has landed or
/// [`ESTABLISH`] expires.
///
/// The poll is a bounded spin with a 1ms sleep: `spawn` returns as soon as the kernel has created
/// the child, which can be before the child has run `setpgid`. Reading once would intermittently
/// report the harness's group and disable a guard that is in fact correct.
fn establish(pid: u32) -> Establishment {
    let Ok(raw) = i32::try_from(pid) else {
        return Establishment::Shared;
    };
    let started = Instant::now();
    loop {
        match nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(raw))) {
            Ok(group) if u32::try_from(group.as_raw()).ok() == Some(pid) => {
                return Establishment::Leader;
            }
            // Alive but grouped elsewhere, or unreadable. A zombie that never became a leader is
            // dead; treating it as departed lets a fast-exiting command through.
            Ok(_) => {
                if departed(pid) {
                    return Establishment::Departed;
                }
            }
            Err(nix::errno::Errno::ESRCH) => return Establishment::Departed,
            Err(_) => {}
        }
        if started.elapsed() >= ESTABLISH {
            return Establishment::Shared;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Whether `pid` is a zombie, read from its `/proc` state field.
///
/// A zombie keeps its pid and its group until it is reaped, so `getpgid` still answers for it; the
/// state is what distinguishes "exited, waiting for `wait`" from "alive in the wrong group".
fn departed(pid: u32) -> bool {
    let path = format!("/proc/{pid}/stat");
    let Ok(stat) = std::fs::read_to_string(path) else {
        // The process vanished between the `getpgid` call and this read.
        return true;
    };
    let Some(tail) = stat.rfind(')').map(|close| &stat[close + 1..]) else {
        return false;
    };
    tail.split_whitespace().next() == Some("Z")
}

/// Every live member of `group`, read from `/proc`.
///
/// `/proc/<pid>/stat` is used instead of probing pid ranges with signal zero because a group can
/// contain any pid, and scanning would be both slow and racy. The `comm` field can contain spaces
/// and parentheses, so the group id is taken from the fields after the *last* `)` rather than by
/// splitting the whole line.
fn group_members(group: u32, own_group: u32) -> Vec<u32> {
    // Never report on our own group, whatever was recorded: it is not the child's, and a caller
    // that reached here is better served by "nothing to kill" than by a survey of the harness.
    if group == own_group || group <= 1 {
        return Vec::new();
    }
    let mut members = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return members;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // A process can exit between the directory read and this open; the failure is expected.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(tail) = stat.rfind(')').map(|close| &stat[close + 1..]) else {
            continue;
        };
        // After `comm`: `state`, then `ppid`, then `pgrp`. The third field is the group.
        let mut fields = tail.split_whitespace();
        let (Some(state), Some(_ppid), Some(pgrp)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // A zombie keeps both its pid and its group until it is reaped, but it holds no pipes and
        // cannot be signalled into doing anything. Counting it would make `any_alive` report a
        // survivor whenever a killed child has not been waited for yet.
        if state == "Z" {
            continue;
        }
        if pgrp.parse::<u32>() == Ok(group) {
            members.push(pid);
        }
    }
    members
}

/// Delivers `signal` to the whole group, ignoring delivery failures.
///
/// The negative pid is the whole point: a positive one would signal only the leader and leave the
/// descendants this module exists to reap. Every case that could aim it at the wrong group — an
/// unverified pid, the harness's own group — is rejected before this function is reached.
fn send(group: u32, signal: nix::sys::signal::Signal) {
    let Ok(raw) = i32::try_from(group) else {
        return;
    };
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(-raw), signal);
}
