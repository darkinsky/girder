//! The life of an MCP server *process*: whether it is up, why it went down, and whether to bring it back.
//!
//! The other MCP modules each own one layer of the connection and stop at its edge.
//! [`crate::mcp::client`] says so in its own module docs — "this layer owns no process" — and builds the
//! `initialize`, `tools/list`, and `tools/call` payloads as values. [`crate::mcp::stdio`] converts bytes
//! on a pipe into whole messages and back. [`crate::mcp`] declares the [`crate::mcp::Transport`] trait
//! and names tools as `server__tool`. None of them spawns, watches, or reaps a child, and the one
//! `impl Transport` in the tree today refuses every call.
//!
//! That leaves one question nobody answers: a server process that dies between two calls is not
//! visible in any of them. The pipe reaches EOF, the pending call fails with a transport error, and the
//! registry still reports the server as ready because readiness is a fact it recorded once at startup.
//! This module is the missing record: which processes are supposed to be up, how long they took to
//! handshake, why the last one ended, and whether the fleet is allowed to start it again.
//!
//! # Why this is a record and not a supervisor
//!
//! Spawning is a deployment concern — a pipe, a socket, an SSH channel — and the trait that will do it
//! is still being implemented. A supervisor written now would have to invent that interface and then be
//! rewritten around it. A record does not: it is a state machine over events a supervisor *will* have,
//! so the policy that matters (restart crashes, not clean exits) is settled and tested before the
//! process plumbing exists. Every entry point takes the current time as a parameter rather than reading
//! a clock, so a test can drive a slow handshake or a three-strike restart loop exactly.
//!
//! # Why only crashes are restarted
//!
//! The restart policy is the whole point of the module, and it is deliberately narrow:
//!
//! - A **crash** is a fault nobody chose, so a retry is the ordinary response. Bounded by
//!   [`Lifecycle::should_restart`], because a server that crashes on every start is a configuration
//!   bug, and retrying it forever burns a core and floods the log without ever succeeding.
//! - A **clean exit** is the server saying it is done. Restarting it would re-run work it finished and
//!   would turn a one-shot server into an unintended daemon.
//! - A **kill** is the harness's own decision — a startup timeout, a shutdown, a broken pipe during
//!   teardown. Restarting it would resurrect exactly the process that was just declared unfit, which is
//!   the one outcome a timeout exists to prevent.
//!
//! # Why startup time is recorded at all
//!
//! A slow handshake is invisible until it is measured: the server is "up", calls work, and the only
//! symptom is that every session starts a little later. Recording the last handshake's duration per
//! server turns that into a number an operator can compare, and makes a server that regressed from 200 ms
//! to eight seconds visible before it is blamed on the harness.

use std::collections::BTreeMap;

/// A state in the life of one MCP server process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Spawned, handshake not finished.
    Started,
    /// Handshake finished; the server can answer calls.
    Ready,
    /// Gone.
    Exited {
        /// The status the operating system reported, when it reported one.
        ///
        /// `None` for a process that died from a signal: a signal death has no exit status, and
        /// substituting a number (the shell's `128 + n` convention, or `-1`) would make it
        /// indistinguishable from a process that genuinely exited with that code.
        code: Option<i32>,
        /// Why it ended.
        reason: ExitReason,
    },
}

impl State {
    /// Whether the server can answer a call right now.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Whether the process is gone.
    #[must_use]
    pub fn is_exited(&self) -> bool {
        matches!(self, Self::Exited { .. })
    }

    /// The wire name, for logs and status lines.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ready => "ready",
            Self::Exited { .. } => "exited",
        }
    }
}

/// Why a server process ended.
///
/// Kept as a cause and not a status code because the restart decision depends on *who* ended the
/// process, and an exit code cannot express that: a server we killed and a server that crashed may both
/// be reaped with the same status, and only the actor that killed it knows which happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// The server finished on its own.
    Clean,
    /// The server died without being asked to.
    Crashed,
    /// We ended it — a timeout, a shutdown, a teardown.
    Killed,
}

impl ExitReason {
    /// The wire name, for logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Crashed => "crashed",
            Self::Killed => "killed",
        }
    }
}

/// One MCP process's lifecycle record.
///
/// Holds the server's name, where it currently is, when the last start and handshake happened, and how
/// many restarts have been spent on it. The record is driven by the caller: [`Lifecycle::started`],
/// [`Lifecycle::ready`], and [`Lifecycle::exited`] are the three events a supervisor observes, and each
/// takes the observation time so the record never reads a clock of its own.
///
/// A new record is in [`State::Started`] rather than a fourth "configured" state: [`crate::mcp::Status`]
/// already models "configured, not started" for servers the registry knows about, and a fleet is the set
/// of processes the deployment intends to run. The distinction that matters here — spawned but not yet
/// handshaken — is the one the timestamp marks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lifecycle {
    server: String,
    state: State,
    started_at_ms: Option<u64>,
    ready_at_ms: Option<u64>,
    exited_at_ms: Option<u64>,
    restarts: u32,
}

impl Lifecycle {
    /// Creates a record for `server`, before its first start is observed.
    #[must_use]
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            state: State::Started,
            started_at_ms: None,
            ready_at_ms: None,
            exited_at_ms: None,
            restarts: 0,
        }
    }

    /// The server this record is about.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Records that the process was spawned at `now_ms`.
    ///
    /// Also the entry point after a restart: it clears the previous handshake and exit, so the record
    /// describes the process that is running now rather than the one it replaced. Keeping the old
    /// timestamps would let [`Lifecycle::startup_ms`] report a handshake from a process that no longer
    /// exists.
    pub fn started(&mut self, now_ms: u64) {
        self.state = State::Started;
        self.started_at_ms = Some(now_ms);
        self.ready_at_ms = None;
        self.exited_at_ms = None;
    }

    /// Records that the handshake finished at `now_ms`.
    ///
    /// Ignored unless the process is currently [`State::Started`]. A handshake reported after the exit
    /// is a late reply from a pipe that has already been reaped — a reader draining what the dead
    /// process wrote — and marking the server ready on it would advertise a process that cannot answer.
    pub fn ready(&mut self, now_ms: u64) {
        if self.state != State::Started {
            return;
        }
        self.state = State::Ready;
        self.ready_at_ms = Some(now_ms);
    }

    /// Records that the process exited at `now_ms` with `code`, for `reason`.
    ///
    /// The first exit report wins. A supervisor that waits on a child can race a kill with a reaper, and
    /// letting the later report overwrite the earlier one would relabel a kill as a clean exit (or the
    /// reverse), which is precisely the distinction [`Lifecycle::should_restart`] reads.
    pub fn exited(&mut self, now_ms: u64, code: Option<i32>, reason: ExitReason) {
        if self.state.is_exited() {
            return;
        }
        self.state = State::Exited { code, reason };
        self.exited_at_ms = Some(now_ms);
    }

    /// Where the process is now.
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Whether the server can answer a call right now.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    /// How many restarts this record has spent, counted by [`Fleet::restart`].
    #[must_use]
    pub fn restarts(&self) -> u32 {
        self.restarts
    }

    /// Milliseconds from the last observed start to the handshake that followed it.
    ///
    /// `None` when no handshake has completed since the last start, which includes a server that is
    /// still handshaking and one that died before it finished — the cases where a duration would be
    /// either wrong or impossible to measure.
    ///
    /// Saturating because a caller reporting a monotonic clock across a restart can hand back a time
    /// that went backwards; a zero is a truthful "too fast to see", a panic or a wrapped
    /// eighteen-quintillion-millisecond startup is not.
    #[must_use]
    pub fn startup_ms(&self) -> Option<u64> {
        match (self.started_at_ms, self.ready_at_ms) {
            (Some(started), Some(ready)) => Some(ready.saturating_sub(started)),
            _ => None,
        }
    }

    /// Whether this record may be restarted given a budget of `max_restarts` attempts.
    ///
    /// This is the whole policy, in one predicate: only a crash qualifies, and only while the budget
    /// lasts. A clean exit and a kill answer `false` regardless of the budget, because the budget is
    /// about how often a fault may be retried, not permission to overturn a decision.
    #[must_use]
    pub fn should_restart(&self, max_restarts: u32) -> bool {
        matches!(
            &self.state,
            State::Exited {
                reason: ExitReason::Crashed,
                ..
            }
        ) && self.restarts < max_restarts
    }

    /// When the last start was observed, for diagnostics that need an absolute time.
    #[must_use]
    pub fn started_at_ms(&self) -> Option<u64> {
        self.started_at_ms
    }

    /// When the last exit was observed, for diagnostics that need an absolute time.
    #[must_use]
    pub fn exited_at_ms(&self) -> Option<u64> {
        self.exited_at_ms
    }

    /// Spends one restart and moves the record back to [`State::Started`].
    ///
    /// Private to the module so the counter cannot be advanced without the state change that must
    /// accompany it: a record whose counter moved but whose state stayed `Exited` would report a
    /// restart that never happened, and the budget would be spent on nothing.
    fn spend_restart(&mut self, now_ms: u64) {
        self.restarts = self.restarts.saturating_add(1);
        self.started(now_ms);
    }
}

/// What can go wrong when asking a fleet to restart a server.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FleetError {
    /// No record for this server name.
    #[error("unknown MCP server `{server}`")]
    Unknown {
        /// The name that was asked for.
        server: String,
    },
    /// The fleet will not start this server again.
    ///
    /// Two conditions share this error because both mean the same thing to a caller — nothing was
    /// started — and neither is retryable: either the restart budget is spent (`restarts` equals the
    /// fleet's limit), or the last exit does not warrant a restart at all (a clean exit or a kill).
    /// Splitting them would tempt a caller into retrying the second kind.
    #[error("MCP server `{server}` may not be restarted (restarts: {restarts})")]
    Exhausted {
        /// The server that was refused.
        server: String,
        /// How many restarts were already recorded for it.
        restarts: u32,
    },
}

/// The servers a deployment runs, and the restart budget they share as a policy.
///
/// The budget is a property of the fleet rather than of a server so every server is held to the same
/// limit: a per-server number invites a configuration where the one server that cannot start is the one
/// configured to retry forever.
///
/// Ordered by name so that [`Fleet::crashed`] and [`Fleet::names`] are stable between calls; a fleet
/// summary that reorders itself is hard to diff between two log lines.
#[derive(Clone, Debug)]
pub struct Fleet {
    max_restarts: u32,
    servers: BTreeMap<String, Lifecycle>,
}

impl Fleet {
    /// Creates an empty fleet whose servers may each be restarted `max_restarts` times.
    ///
    /// Zero is a valid budget and means "never restart": a deployment that would rather fail a session
    /// than run a server twice should not have to express that as a special case.
    #[must_use]
    pub fn new(max_restarts: u32) -> Self {
        Self {
            max_restarts,
            servers: BTreeMap::new(),
        }
    }

    /// The restart budget every server in this fleet is held to.
    #[must_use]
    pub fn max_restarts(&self) -> u32 {
        self.max_restarts
    }

    /// Adds a record, replacing any record with the same server name.
    ///
    /// Replacing rather than refusing: a caller that rebuilds a record from configuration is describing
    /// the same server, and keeping the stale copy would leave the fleet reporting a process that no
    /// longer exists. The replacement starts with a fresh restart budget, which is the honest reading of
    /// "the deployment was reconfigured".
    pub fn add(&mut self, lifecycle: Lifecycle) {
        self.servers.insert(lifecycle.server.clone(), lifecycle);
    }

    /// One server's record.
    #[must_use]
    pub fn get(&self, server: &str) -> Option<&Lifecycle> {
        self.servers.get(server)
    }

    /// The server names, ordered.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }

    /// How many servers this fleet knows about.
    #[must_use]
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// Whether the fleet knows about no servers at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// How many servers can answer a call right now.
    ///
    /// Only [`State::Ready`] counts: a server that is handshaking has no catalogue yet, and one that
    /// exited cannot answer, so counting either would advertise capacity the fleet does not have.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.servers
            .values()
            .filter(|lifecycle| lifecycle.is_ready())
            .count()
    }

    /// The servers currently sitting in a crash, ordered by name.
    ///
    /// "In a crash" is the current state, not the history: once a crashed server is restarted it leaves
    /// this list, so the list answers "what needs attention now" rather than "what ever went wrong".
    #[must_use]
    pub fn crashed(&self) -> Vec<&str> {
        self.servers
            .values()
            .filter(|lifecycle| {
                matches!(
                    lifecycle.state(),
                    State::Exited {
                        reason: ExitReason::Crashed,
                        ..
                    }
                )
            })
            .map(|lifecycle| lifecycle.server.as_str())
            .collect()
    }

    /// Restarts one crashed server, spending one attempt of its budget.
    ///
    /// The restart is recorded before it is returned as successful: the record moves to
    /// [`State::Started`] with `now_ms` as its start, so a subsequent [`Lifecycle::exited`] describes the
    /// new process. The counter advances here and nowhere else, which is what makes the budget
    /// unbypassable — a caller cannot start a server again without passing through this check.
    ///
    /// # Errors
    ///
    /// Returns [`FleetError::Unknown`] when the fleet has no record for `server`, and
    /// [`FleetError::Exhausted`] when [`Lifecycle::should_restart`] refuses — the budget is spent, or the
    /// last exit was a clean exit or a kill rather than a crash.
    pub fn restart(&mut self, server: &str, now_ms: u64) -> Result<(), FleetError> {
        let max_restarts = self.max_restarts;
        let lifecycle = self
            .servers
            .get_mut(server)
            .ok_or_else(|| FleetError::Unknown {
                server: server.to_owned(),
            })?;
        if !lifecycle.should_restart(max_restarts) {
            return Err(FleetError::Exhausted {
                server: server.to_owned(),
                restarts: lifecycle.restarts,
            });
        }
        lifecycle.spend_restart(now_ms);
        Ok(())
    }
}
