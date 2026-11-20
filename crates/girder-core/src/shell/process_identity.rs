//! Process identity and process-table inspection: which process a pid *is*, and whether it is
//! still that process before it is signalled.
//!
//! # Where this comes from
//!
//! Upstream `packages/subprocess` is 6,875 lines of production TypeScript across 28 files. Its
//! capabilities fall into eight categories:
//!
//! 1. **Service definition and vocabulary** — `subprocess/src/{types,error,index,control}.ts`:
//!    the spawn spec, `CollectedOutput`, the error codes, and [`scrubbedParentEnv`].
//! 2. **Local spawn and managed binding** — `subprocess-local/src/{spawn,managed-owner,index}.ts`:
//!    spawn, group kill, and the owner that reaps.
//! 3. **Bounded output collection** — `subprocess-local/src/output.ts`: a tail-keeping collector
//!    with a spill file and offset-addressed reads.
//! 4. **Process-table inspection** — `subprocess-local/src/{process-inspector,windows-inspector}.ts`
//!    (906 lines): `/proc/<pid>/stat` parsing, PID-plus-start-time identity, tree and session
//!    queries, liveness, foreground groups, and the fence a signal takes.
//! 5. **Linux containment** — `linux-scope.ts`, `linux-scope-selector.ts`, `linux-execve.ts`
//!    (919 lines): a transient systemd scope, probed and selected before use.
//! 6. **Privileged runner protocol** — `runner-protocol.ts`, `runner-launch.ts`, `spawn-runner.ts`,
//!    `bin.ts`, `control-spawn.ts` (1,194 lines).
//! 7. **Terminal (PTY) primitive** — `terminal.ts`, `shell-activity.ts` (610 lines).
//! 8. **Windows process control** — `win32-process/src/*` and `windows-job.ts` (~1,180 lines): job
//!    objects over koffi FFI.
//!
//! # What the local tree already covers
//!
//! - `subprocess.rs` — category 2 (spawn, group-leader, `SIGTERM`→`SIGKILL`, concurrent drain,
//!   reaping on every path including timeout and cancellation).
//! - `exec.rs` — the shell executor over that seam, plus a path policy.
//! - `process_group.rs` — post-exit containment: what to do when a *successful* command left a
//!   descendant behind, plus a `/proc` scan for group membership.
//! - `mcp_subprocess.rs` — the MCP stdio child.
//! - `launch_environment.rs` — category 1's scrub, as an allowlist of ambient names plus a
//!   forbidden-prefix table.
//! - `spill.rs`, `spill_policy.rs`, `retention.rs`, `terminal_buffer.rs` — most of category 3,
//!   including a bounded offset-addressed buffer.
//! - `terminal_local.rs` — category 7, explicitly and honestly not a PTY.
//!
//! # The two gaps, and why this one
//!
//! **Category 4 is the total gap.** `process_group.rs` walks `/proc` for one question only — which
//! pids share a group id — and it answers that from `/proc/<pid>/stat` fields `state` and `pgrp`,
//! discarding everything else on the line. Nothing in the local tree parses a start time, so nothing
//! can answer "is this pid still *that* process". That is the question the whole category exists
//! for, and it is a correctness question rather than a feature: a pid is recycled, so a signal
//! aimed at a remembered pid can land on an unrelated process — potentially another harness, or a
//! user's editor.
//!
//! The local consequences are visible today: `crate::shell::subprocess::kill_tree` signals `-pid` after the leader
//! was reaped, and `GroupGuard` signals a remembered group id after a grace period, and neither can
//! tell a recycled id from the original. `process_group.rs` says as much in its own module docs
//! ("the group id must not be one we share") and mitigates the one case it can, but it has no
//! start-time fence to mitigate the rest with.
//!
//! **Category 3 has a real remainder** — `subprocess.rs` drains with `read_to_end`, so a command
//! that writes gigabytes is bounded by nothing — but `retention.rs` already supplies a streaming
//! bounded tail and `terminal_buffer.rs` already supplies offset reads, so the remainder is narrow
//! and it is a policy question rather than a missing primitive.
//!
//! **Categories 5, 7 and 8 are deliberately unported locally**, each with a stated reason in the
//! module that would own it: a systemd scope needs a user manager and a `systemctl` round trip, a
//! PTY needs `unsafe` and a dependency, and Windows has no `/proc`. Porting them here would be a
//! second, worse implementation of a decision already made and documented.
//!
//! So this module ports **category 4**, the one with no local counterpart at all.
//!
//! # What is ported, symbol by symbol
//!
//! | Upstream (`process-inspector.ts`) | Here |
//! | --- | --- |
//! | `parseProcStat` | [`parse_proc_stat`] |
//! | `quiescent` | [`quiescent`] |
//! | `linuxProcessGroupLiveMember` | [`group_member`] |
//! | `ProcessIdentity` | [`ProcessIdentity`] |
//! | `ProcessSnapshot` (`tree`, `session`, `alive`, `complete`) | [`Snapshot`] |
//! | `ProcessInspector.snapshot` | [`Snapshot::observe`] |
//! | `ProcessInspector.isAlive` | [`is_alive`] |
//! | `ProcessInspector.foregroundPgid` | [`foreground_group`] |
//! | `ProcessInspector.signalProcess` | [`signal_process`] |
//! | `ProcessInspector.signalGroup` | [`signal_group`] |
//! | `ProcessInspectorInternals` | [`Probe`] and [`HostProbe`] |
//! | `SubprocessTerminalSignal` | [`Signal`] |
//!
//! # Deliberately not ported
//!
//! - **`isStdinWaiting`** (upstream, ~200 lines). It decides whether a foreground group is blocked
//!   reading the terminal by reading `/proc/<pid>/task/<tid>/syscall` and then decoding
//!   `poll`/`select`/`epoll` argument memory through `/proc/<pid>/mem`, against a per-architecture
//!   syscall table. Beyond needing `unsafe`-adjacent memory reads, it is only meaningful for a
//!   PTY-backed shell — and this crate has no PTY (see `terminal_local.rs`), so the answer would
//!   have no reader.
//! - **The macOS `ps` table and the Windows Toolhelp32 table.** The local execution layer is POSIX
//!   and Linux-shaped already (`/proc` is the mechanism, as `process_group.rs` states), so a
//!   second platform's row source would be untestable here.
//! - **`createProcessInspector`'s platform dispatch**, for the same reason: [`HostProbe`] is the
//!   Linux binding, and a caller that needs another platform implements [`Probe`].
//!
//! # The fence, and why it cannot be a cached observation
//!
//! [`is_alive`] reads the process's own stat line immediately before the signal. An observation
//! taken earlier in the same round cannot stand in for it: a snapshot preserves the *original*
//! pid-to-start-time pairing, so a recycled pid still matches its row and would take a signal meant
//! for the process that exited. [`Snapshot::alive`] is therefore the right question for batch
//! filtering and the wrong one for signalling, which is exactly the split upstream draws.

/// One signal the terminal and process primitives can deliver.
///
/// Upstream: `SubprocessTerminalSignal` in `packages/subprocess/subprocess/src/types.ts`, kept
/// member-identical to `TerminalSignal` in `@deepseek-ai/dsh-terminal`. The set is closed on
/// purpose: a harness has no business sending `SIGSEGV` to a child it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Signal {
    /// `SIGINT` — the interrupt a `Ctrl-C` would send.
    Interrupt,
    /// `SIGTERM` — the polite stop every teardown starts with.
    Terminate,
    /// `SIGKILL` — the stop that cannot be caught.
    Kill,
    /// `SIGTSTP` — the interactive stop (`Ctrl-Z`).
    Stop,
    /// `SIGHUP` — the hangup a closed terminal sends.
    Hangup,
}

impl Signal {
    /// The signal's network-facing name, as upstream spells it.
    ///
    /// Upper case with the `SIG` prefix because the name crosses into wire frames and logs, where
    /// Node's own spelling is what a reader expects.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Kill => "SIGKILL",
            Self::Stop => "SIGTSTP",
            Self::Hangup => "SIGHUP",
        }
    }

    /// The `nix` signal this maps to.
    ///
    /// Public so a [`Probe`] implementation on another substrate can deliver the same set.
    #[must_use]
    pub const fn to_nix(self) -> nix::sys::signal::Signal {
        match self {
            Self::Interrupt => nix::sys::signal::Signal::SIGINT,
            Self::Terminate => nix::sys::signal::Signal::SIGTERM,
            Self::Kill => nix::sys::signal::Signal::SIGKILL,
            Self::Stop => nix::sys::signal::Signal::SIGTSTP,
            Self::Hangup => nix::sys::signal::Signal::SIGHUP,
        }
    }
}

impl std::fmt::Display for Signal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A pid together with the identity of the process that held it.
///
/// Upstream: `ProcessIdentity` in `packages/subprocess/subprocess-local/src/process-inspector.ts`.
///
/// The pair is the point. A pid alone names a slot, and the operating system reuses slots; the start
/// identity names the occupant. Comparing both is the only way to say "this is still the process I
/// spawned" rather than "some process now has that number".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessIdentity {
    /// The process id.
    pub pid: u32,
    /// The process's start identity, in the source's own units.
    ///
    /// On Linux this is `/proc/<pid>/stat`'s `starttime` field (in clock ticks since boot); on a
    /// platform whose table reports a human-readable start it is that text. It is compared for
    /// equality only and never parsed here, which is what lets one type carry both.
    pub started: String,
}

impl ProcessIdentity {
    /// Pairs a pid with its start identity.
    #[must_use]
    pub fn new(pid: u32, started: impl Into<String>) -> Self {
        Self {
            pid,
            started: started.into(),
        }
    }
}

/// The fields of one `/proc/<pid>/stat` line this module reads.
///
/// Upstream: `ProcStat` (`interface ProcStat`, private to `process-inspector.ts`) and its parser
/// `parseProcStat`, which upstream exports for tests. The struct is public here for the same reason:
/// a caller that already holds a stat line should not re-read the file to ask one more question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcStat {
    /// The process id, from the field before `(`.
    pub pid: u32,
    /// The parent process id (`ppid`).
    pub parent_pid: u32,
    /// The process group id (`pgrp`).
    pub group: u32,
    /// The session id.
    pub session: u32,
    /// The single-letter state: `R`, `S`, `D`, `T`, `t`, `Z`, `X`, or `x`.
    pub state: char,
    /// The controlling terminal's device number (`tty_nr`), signed as `/proc` renders it.
    ///
    /// Zero when the process has no controlling terminal.
    pub terminal: i32,
    /// The foreground process group of that terminal (`tpgid`), or a non-positive value when none.
    pub foreground_group: i32,
    /// The start identity: `starttime` on Linux, in clock ticks since boot.
    pub started: String,
}

impl ProcStat {
    /// This line's identity, in the form a fence compares.
    #[must_use]
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity::new(self.pid, self.started.clone())
    }
}

/// Parses one `/proc/<pid>/stat` line, or reports that it is malformed.
///
/// Upstream: `parseProcStat` in `packages/subprocess/subprocess-local/src/process-inspector.ts`.
///
/// # Why the `comm` field is skipped by parenthesis, not by splitting
///
/// The second field is the executable's name in parentheses, and the kernel does not escape it: a
/// process can rename itself to `a b) c` and every field after it would shift under a plain split.
/// The fields are therefore taken from after the *last* `)`, which is the one the kernel wrote.
///
/// # Where this is stricter than upstream
///
/// Upstream's numeric check is `Number.isSafeInteger`, and `Number('')` and `Number(' ')` are both
/// `0` — so a truncated line parses as pid 0 with a valid-looking start identity, and a fence built
/// on it would match every nameless row. Here a field that does not parse is a `None`, and a blank
/// start identity is a `None` as well: an empty `started` would compare equal to every other empty
/// one and silently defeat the fence this module exists for.
#[must_use]
pub fn parse_proc_stat(text: &str) -> Option<ProcStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    // `(comm)` must be preceded by the pid and followed by at least the state field.
    if open == 0 || close <= open || close + 1 >= text.len() {
        return None;
    }
    let pid = text.get(..open)?.trim().parse::<u32>().ok()?;
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();

    let state = rest.first()?.chars().next()?;
    // A one-letter state is the kernel's contract; anything else means the split is off.
    if rest.first()?.chars().count() != 1 {
        return None;
    }
    let parent_pid = rest.get(1)?.parse::<u32>().ok()?;
    let group = rest.get(2)?.parse::<u32>().ok()?;
    let session = rest.get(3)?.parse::<u32>().ok()?;
    let terminal = rest.get(4)?.parse::<i32>().ok()?;
    let foreground_group = rest.get(5)?.parse::<i32>().ok()?;
    // Fields 7 through 21 sit between `tpgid` and `starttime`; only the last of them is wanted.
    let started = *rest.get(19)?;
    if started.is_empty() {
        return None;
    }
    Some(ProcStat {
        pid,
        parent_pid,
        group,
        session,
        state,
        terminal,
        foreground_group,
        started: started.to_owned(),
    })
}

/// Whether a process state means the process is already past running.
///
/// Upstream: `quiescent` in `packages/subprocess/subprocess-local/src/process-inspector.ts`.
///
/// A zombie (`Z`) has exited and is waiting to be reaped; it keeps its pid, its group and its
/// session, so every membership question answers "present" for it. It cannot be signalled into doing
/// anything, and counting it as alive makes a teardown report a survivor that does not exist. `X`
/// and `x` are the kernel's "dead" states, which are only observable on a kernel built to expose
/// them.
#[must_use]
pub const fn quiescent(state: char) -> bool {
    matches!(state, 'Z' | 'X' | 'x')
}

/// The operating-system operations the inspector needs, injected so they can be faked.
///
/// Upstream: `ProcessInspectorInternals` in
/// `packages/subprocess/subprocess-local/src/process-inspector.ts`. Upstream carries ten operations
/// (`readFile`, `readDir`, `readLink`, `stat`, `open`, `read`, `close`, `exec`, `kill`, and the
/// default binding); the three here are the ones the ported subset calls. Shrinking the surface is
/// deliberate — an operation no ported function calls is an operation a test has to fake for nothing.
pub trait Probe {
    /// Reads a file to a string, or reports that it is unreadable.
    ///
    /// Unreadable is the ordinary race, not a failure: a process can exit between the directory read
    /// that named it and the open that would have described it.
    fn read(&self, path: &str) -> Option<String>;

    /// Lists a directory's numeric entries, or reports that it is unreadable.
    ///
    /// `/proc` holds names that are not pids (`self`, `mounts`, `sys`); returning the numeric ones
    /// only is what upstream's `numericEntries` does, and doing it here keeps every caller from
    /// repeating the filter.
    fn entries(&self, path: &str) -> Option<Vec<u32>>;

    /// Delivers `signal` to `pid`.
    ///
    /// A negative `pid` names a process group, which is how [`signal_group`] reaches one. Delivery
    /// failures are the implementation's to ignore: a signal that finds nothing has already achieved
    /// what it wanted.
    fn signal(&self, pid: i32, signal: Signal);
}

/// The real host: `/proc` for reads, `kill(2)` for delivery.
///
/// Upstream: `DEFAULT_INTERNALS` in `packages/subprocess/subprocess-local/src/process-inspector.ts`,
/// reduced to the same three operations [`Probe`] declares.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostProbe;

impl Probe for HostProbe {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn entries(&self, path: &str) -> Option<Vec<u32>> {
        let directory = std::fs::read_dir(path).ok()?;
        let mut pids: Vec<u32> = directory
            // An entry that vanishes between the read and the name lookup is skipped, not fatal:
            // this is a live table, and a process exiting mid-walk is the normal case.
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .collect();
        // Sorted so two observations of an unchanged table agree, which makes a test that compares
        // them report a real difference rather than a directory order.
        pids.sort_unstable();
        Some(pids)
    }

    fn signal(&self, pid: i32, signal: Signal) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal.to_nix());
    }
}

/// Why a process-table observation could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InspectError {
    /// The process table could not be enumerated at all.
    ///
    /// Distinct from a row that could not be read: that is a race, and it is reported by
    /// [`Snapshot::complete`] instead. This is "no question can be answered", which is why it is an
    /// error rather than a completeness flag.
    #[error("the process table is unreadable, so no membership question can be answered")]
    Unreadable,
}

/// One row of a process-table observation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    pid: u32,
    parent_pid: u32,
    started: String,
    /// Absent where the platform's table omits session ids (upstream's macOS rows).
    session: Option<u32>,
    /// Absent where the platform's table omits a state column; a table without one can only report
    /// presence, never liveness.
    state: Option<char>,
}

/// One step of the tree walk: descend into a pid, or emit it once its children are done.
///
/// Declared beside the walk rather than inside it so the function body stays statements: an item
/// declared mid-body reads as if it only exists from that point, which it does not.
enum Frame {
    /// Descend into this pid.
    Enter(u32),
    /// Emit this pid, after its children.
    Leave(u32),
}

/// One observation of the process table, shared by every question asked of it.
///
/// Upstream: `ProcessSnapshot` / `PosixProcessSnapshot` in
/// `packages/subprocess/subprocess-local/src/process-inspector.ts`.
///
/// The table is read once, on construction. That is what keeps a readiness poll's cost independent
/// of how many descendants the running command spawned: a tree walk asks one in-memory question per
/// pid rather than one `/proc` read per pid.
///
/// It answers what the table showed, which is what batch filtering wants and what signalling must
/// not use — see [`Snapshot::alive`] and [`is_alive`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    rows: Vec<Row>,
    complete: bool,
}

impl Snapshot {
    /// Reads the process table once.
    ///
    /// Upstream: `ProcessInspector.snapshot` in
    /// `packages/subprocess/subprocess-local/src/process-inspector.ts`.
    ///
    /// # Errors
    ///
    /// Returns [`InspectError::Unreadable`] when the table cannot be enumerated. A row that cannot be
    /// read is not an error — it is the ordinary exit race, and it clears [`Snapshot::complete`].
    pub fn observe<P>(probe: &P) -> Result<Self, InspectError>
    where
        P: Probe + ?Sized,
    {
        let pids = probe.entries("/proc").ok_or(InspectError::Unreadable)?;
        let mut rows = Vec::with_capacity(pids.len());
        let mut complete = true;
        for pid in pids {
            match stat_of(probe, pid) {
                Some(stat) => rows.push(Row {
                    pid: stat.pid,
                    parent_pid: stat.parent_pid,
                    started: stat.started,
                    session: Some(stat.session),
                    state: Some(stat.state),
                }),
                // A process can exit between the directory read and this open; the failure is
                // expected, and it makes the observation incomplete rather than wrong.
                None => complete = false,
            }
        }
        Ok(Self { rows, complete })
    }

    /// Whether the enumeration omitted no unreadable row.
    ///
    /// Upstream: `ProcessSnapshot.complete`. A caller that needs an exhaustive answer — "no member
    /// of this group is left" — must check this, because an incomplete table can only say "I did not
    /// see one".
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.complete
    }

    /// How many rows the observation holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the observation holds no rows at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The root and its transitive descendants, children before parents.
    ///
    /// Upstream: `ProcessSnapshot.tree` / `processTree`. The order is post-order so a caller killing
    /// in iteration order never kills a parent before the child that would be re-parented to init by
    /// its death. An unknown root yields nothing, and a table whose parent links form a cycle yields
    /// each pid once rather than looping — the `visited` set upstream keeps for the same reason.
    #[must_use]
    pub fn tree(&self, root_pid: u32) -> Vec<ProcessIdentity> {
        // One pass to build the parent index, then an explicit stack for the walk: a process tree can
        // be deeper than a comfortable recursion, and an iteration bound is easier to see than a
        // recursion bound.
        let mut children: std::collections::BTreeMap<u32, Vec<u32>> =
            std::collections::BTreeMap::new();
        let mut identities: std::collections::BTreeMap<u32, ProcessIdentity> =
            std::collections::BTreeMap::new();
        for row in &self.rows {
            children.entry(row.parent_pid).or_default().push(row.pid);
            identities.insert(row.pid, ProcessIdentity::new(row.pid, row.started.clone()));
        }
        if !identities.contains_key(&root_pid) {
            return Vec::new();
        }

        let mut visited: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let mut order: Vec<ProcessIdentity> = Vec::new();
        let mut stack = vec![Frame::Enter(root_pid)];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Enter(pid) => {
                    // A pid reached twice (a cycle, or a table with duplicate rows) is emitted once.
                    if !visited.insert(pid) {
                        continue;
                    }
                    stack.push(Frame::Leave(pid));
                    // Reversed so the first child is the first one entered, matching the table order
                    // the recursive form would have used.
                    if let Some(list) = children.get(&pid) {
                        for child in list.iter().rev() {
                            stack.push(Frame::Enter(*child));
                        }
                    }
                }
                Frame::Leave(pid) => {
                    if let Some(identity) = identities.get(&pid) {
                        order.push(identity.clone());
                    }
                }
            }
        }
        order
    }

    /// The observed members of one POSIX session.
    ///
    /// Upstream: `ProcessSnapshot.session`. Empty where the platform's table omits session ids,
    /// which is why the row field is optional rather than defaulted to zero.
    #[must_use]
    pub fn session(&self, session_id: u32) -> Vec<ProcessIdentity> {
        self.rows
            .iter()
            .filter(|row| row.session == Some(session_id))
            .map(|row| ProcessIdentity::new(row.pid, row.started.clone()))
            .collect()
    }

    /// Whether the exact identity was a non-quiescent process when the table was read.
    ///
    /// Upstream: `ProcessSnapshot.alive`. It matches the pid *and* the start identity, so a recycled
    /// pid is not the process that was observed. This is a batch question: a signal must use
    /// [`is_alive`], which re-reads.
    #[must_use]
    pub fn alive(&self, identity: &ProcessIdentity) -> bool {
        self.rows
            .iter()
            .find(|row| row.pid == identity.pid)
            .is_some_and(|row| {
                // A table with no state column can only report presence; one with a state column
                // must also show the process is not already past running.
                row.started == identity.started && !row.state.is_some_and(quiescent)
            })
    }
}

/// Reads and parses one process's stat line.
///
/// Upstream: `readLinuxStat` in `packages/subprocess/subprocess-local/src/process-inspector.ts`.
#[must_use]
pub fn stat_of<P>(probe: &P, pid: u32) -> Option<ProcStat>
where
    P: Probe + ?Sized,
{
    parse_proc_stat(&probe.read(&format!("/proc/{pid}/stat"))?)
}

/// Whether `identity` names a process that is running *now*.
///
/// Upstream: `ProcessInspector.isAlive` in
/// `packages/subprocess/subprocess-local/src/process-inspector.ts`.
///
/// This reads the narrowest source the platform offers — one stat line — rather than a whole table,
/// so a signalling round can re-check every target without paying for a scan. It is also the fence a
/// signal takes, which is why it re-reads instead of trusting an earlier [`Snapshot`].
#[must_use]
pub fn is_alive<P>(probe: &P, identity: &ProcessIdentity) -> bool
where
    P: Probe + ?Sized,
{
    stat_of(probe, identity.pid)
        .is_some_and(|stat| stat.started == identity.started && !quiescent(stat.state))
}

/// What a group-membership question proved.
///
/// Upstream: the tri-state return of `linuxProcessGroupLiveMember` in
/// `packages/subprocess/subprocess-local/src/process-inspector.ts` — `number | false | undefined`.
/// The three cases are kept apart because they call for three different decisions: [`Live`] means
/// there is something to signal, [`AllQuiescent`] means the group is provably done, and
/// [`Unobservable`] means the caller has to fall back to its own knowledge.
///
/// [`Live`]: Self::Live
/// [`AllQuiescent`]: Self::AllQuiescent
/// [`Unobservable`]: Self::Unobservable
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupLiveness {
    /// A member is executing; its pid.
    Live(u32),
    /// The group has entries and every one of them is zombie or dead.
    AllQuiescent,
    /// The table could not prove either outcome: nothing matched, or `/proc` was unreadable.
    Unobservable,
}

/// Finds an executing member of a process group.
///
/// Upstream: `linuxProcessGroupLiveMember` in
/// `packages/subprocess/subprocess-local/src/process-inspector.ts`, including its `knownMember`
/// fast path: a still-executing member returned by an earlier call is confirmed from its own stat
/// entry without enumerating `/proc`, so repeated polls of a long-lived group cost one file read
/// instead of one per host process. That matters because this is the question a teardown ladder asks
/// between rungs.
#[must_use]
pub fn group_member<P>(probe: &P, group: u32, known: Option<u32>) -> GroupLiveness
where
    P: Probe + ?Sized,
{
    if let Some(known) = known {
        if let Some(stat) = stat_of(probe, known) {
            if stat.group == group && !quiescent(stat.state) {
                return GroupLiveness::Live(known);
            }
        }
    }
    let Some(pids) = probe.entries("/proc") else {
        return GroupLiveness::Unobservable;
    };
    let mut matched = false;
    for pid in pids {
        let Some(stat) = stat_of(probe, pid) else {
            continue;
        };
        if stat.group != group {
            continue;
        }
        matched = true;
        if !quiescent(stat.state) {
            return GroupLiveness::Live(pid);
        }
    }
    if matched {
        GroupLiveness::AllQuiescent
    } else {
        GroupLiveness::Unobservable
    }
}

/// The foreground process group of `pid`'s controlling terminal, when it has one.
///
/// Upstream: `ProcessInspector.foregroundPgid` (`LinuxProcessInspector.foregroundPgid`). A
/// non-positive `tpgid` means "no foreground group", which is not the same fact as "the shell has no
/// terminal": both make the question unanswerable here, and `None` covers them because no caller can
/// act differently on the distinction.
#[must_use]
pub fn foreground_group<P>(probe: &P, pid: u32) -> Option<u32>
where
    P: Probe + ?Sized,
{
    let raw = stat_of(probe, pid)?.foreground_group;
    // `u32::try_from` rejects the negative "no foreground group" encoding, and the filter drops zero.
    u32::try_from(raw).ok().filter(|group| *group > 0)
}

/// Signals one exact process, fenced against pid reuse.
///
/// Upstream: `ProcessInspector.signalProcess` /
/// `PosixProcessInspector.signalProcess` in
/// `packages/subprocess/subprocess-local/src/process-inspector.ts`.
///
/// The fence reads current state immediately before the signal, which is the whole reason this
/// function exists: an observation taken earlier in the same round cannot stand in for it, because
/// that observation preserves the original pid-to-start-time pairing and a recycled pid would still
/// match it.
///
/// Returns whether a signal was delivered — `false` when the identity is no longer that process, so
/// a caller can record "it was already gone" rather than "I killed it".
pub fn signal_process<P>(probe: &P, identity: &ProcessIdentity, signal: Signal) -> bool
where
    P: Probe + ?Sized,
{
    // A zero pid means the process group of the caller, and a negative one addresses a group; neither
    // is a single process, and neither is something this function should be asked to aim.
    if identity.pid == 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(identity.pid) else {
        return false;
    };
    if !is_alive(probe, identity) {
        return false;
    }
    probe.signal(pid, signal);
    true
}

/// Signals a whole process group.
///
/// Upstream: `ProcessInspector.signalGroup` (and the `killGroup` it is built on) in
/// `packages/subprocess/subprocess-local/src/process-inspector.ts`.
///
/// The negative pid is the point: a positive one would signal only the leader and leave the
/// descendants the group exists to reach.
///
/// # A deliberate deviation
///
/// Upstream signals whatever group id it is handed. This refuses group `0` and group `1`, because
/// group `0` is the caller's own group — signalling it would kill the harness — and group `1` is
/// init's. `process_group.rs` keeps the same guard for the same reason. Returns whether a signal was
/// delivered.
pub fn signal_group<P>(probe: &P, group: u32, signal: Signal) -> bool
where
    P: Probe + ?Sized,
{
    if group <= 1 {
        return false;
    }
    let Ok(pgid) = i32::try_from(group) else {
        return false;
    };
    probe.signal(-pgid, signal);
    true
}
