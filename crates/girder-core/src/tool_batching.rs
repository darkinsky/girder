//! How a batch of tool calls from one assistant step is executed.
//!
//! One model turn can request several tool calls at once. Running them one after another wastes the
//! wall-clock time of every read, and running them all at once corrupts state: two `write` calls that
//! start from the same file content both produce a full replacement, and the second to finish wins,
//! silently discarding the first edit. The loser is not reported as an error — it returns success —
//! so the loss surfaces only much later, as a missing change somebody has to redo. That hazard, and
//! nothing else, is the reason this module exists: it decides *before* any call starts whether the
//! batch may overlap.
//!
//! # The decision
//!
//! The classification is deliberately coarse — one answer for the whole batch rather than one per
//! call — because a coarse answer is easy to audit, cannot interleave in the wrong order, and
//! errs toward correctness:
//!
//! | batch content                 | strategy     | why                                              |
//! |-------------------------------|--------------|--------------------------------------------------|
//! | empty                         | `Sequential` | no work; no reason to name a concurrency policy  |
//! | any destructive call          | `Sequential` | cannot be undone, so it must not race anything   |
//! | every call read-only          | `Parallel`   | reads do not affect each other                   |
//! | any write, and nothing else   | `Sequential` | writes can clobber each other                    |
//! | mixed reads and writes        | `Sequential` | the write may depend on a read it would race     |
//!
//! A mixed batch is the case that costs the most and is still worth it: `read a` followed by
//! `write b` is only correct in that order when the write consumed the read's result, and the
//! scheduler cannot see data flow. Sequencing the whole batch is the only answer that is correct
//! under both readings.
//!
//! Empty batches return [`Strategy::Sequential`] rather than `Parallel`. No call runs either way, so
//! the choice cannot change behaviour; it is `Sequential` so that the answer never claims a
//! concurrency policy that was not chosen on evidence. `Parallel` here would make a caller that
//! reports "batch ran in parallel" assert something about a batch that did not exist.
//!
//! # Why the flags are declarations, not guesses
//!
//! [`ToolCallInfo::read_only`] and [`ToolCallInfo::destructive`] must be set by the tool itself, from
//! what it actually does. A guess from the tool's name is wrong in both directions: a `shell` call
//! that runs `ls` is a read and a `shell` call that runs `rm -rf` is destructive, and no name tells
//! them apart. Callers that do not know the answer must leave both flags `false`; the batch then
//! runs sequentially, which is the only safe default.
//!
//! # Failure propagation
//!
//! The strategy governs scheduling; failure handling is described per variant of [`Strategy`].
//! `Sequential` and `Parallel` both record a failing call's error and continue, because one failed
//! read should not cancel the other five calls the model asked for. `FailFast` exists for a batch
//! whose later calls are only meaningful after the earlier ones succeeded.
//!
//! # What this layer does not do
//!
//! Upstream's scheduler (`packages/core/tools/src/batch.ts` in the reference tree) is finer-grained
//! than this baseline in three ways, none of which are implemented here:
//!
//! * **Per-call classification.** Upstream asks each call whether it is concurrency-safe, so a batch
//!   like `read, read, write, read` becomes parallel, barrier, parallel rather than all-sequential.
//!   Its unclassified default is exclusive (fail closed), the same instinct as this module.
//! * **A bounded rolling pool.** Parallel calls start as slots free up, capped by a configurable
//!   `maxParallelToolCalls` (default 10), so a batch of fifty reads does not open fifty sockets.
//! * **Abort and reclassification.** A pending call can be re-classified as exclusive before it
//!   starts (a registry change), and abort drains started calls while synthesizing error results for
//!   the calls that never started, so the provider still sees one result per `tool_use`.
//!
//! This module also does not model the ordering rule that results are committed to the transcript in
//! model order even when bodies overlap: that belongs to the executor that has the session in hand,
//! not to the policy that chose the strategy.

/// One tool call, as the scheduling decision needs to see it.
///
/// Both flags are properties of what the call will *do*, not of which tool it names; see the module
/// documentation for why a name cannot supply them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallInfo {
    /// The tool name, kept for diagnostics rather than consulted by [`choose`].
    pub name: String,
    /// Whether the call only observes state.
    ///
    /// A read-only call is safe to overlap with any other read-only call. It is ignored when
    /// [`Self::destructive`] is set: a call that claims both is treated as destructive.
    pub read_only: bool,
    /// Whether the call changes something that cannot be undone.
    ///
    /// Destructive calls always force [`Strategy::Sequential`], even when they also claim to be
    /// read-only. The conservative reading of a contradiction is the one that loses no data.
    pub destructive: bool,
}

impl ToolCallInfo {
    /// Builds a call that touches state that can be changed: the safe default for an unknown tool.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            read_only: false,
            destructive: false,
        }
    }

    /// Builds a call that only observes state, such as a file read or a directory listing.
    #[must_use]
    pub fn read_only(name: impl Into<String>) -> Self {
        Self {
            read_only: true,
            ..Self::new(name)
        }
    }

    /// Builds a call that changes state reversibly, such as writing a file.
    #[must_use]
    pub fn write(name: impl Into<String>) -> Self {
        Self::new(name)
    }

    /// Builds a call that changes state irreversibly, such as deleting a path.
    #[must_use]
    pub fn destructive(name: impl Into<String>) -> Self {
        Self {
            destructive: true,
            ..Self::new(name)
        }
    }

    /// Whether this call is safe to overlap with another one, in the terms of the batch decision.
    ///
    /// Destructive and write calls are both excluded: the first because its effect cannot be undone,
    /// the second because two of them can overwrite each other's output.
    #[must_use]
    pub fn is_parallel_safe(&self) -> bool {
        self.read_only && !self.destructive
    }
}

/// How a batch of tool calls is executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// One at a time, in the order the model asked for them.
    ///
    /// A failing call records its error and the batch continues with the next call: the model asked
    /// for all of them, and one failed read says nothing about the others.
    Sequential,
    /// All started at once, waiting for every one of them to finish.
    ///
    /// A failing call records its error; the others still settle. Results must still be delivered in
    /// model order even when the bodies finish out of order — the provider rejects a transcript whose
    /// `tool_result` blocks do not answer the preceding `tool_use` blocks in order.
    Parallel,
    /// One at a time, stopping at the first failure.
    ///
    /// The remaining calls do not run. Each of them still needs a result in the transcript, because
    /// every `tool_use` must be answered; a caller using this strategy is responsible for writing a
    /// synthetic "not executed" error for the calls it skipped.
    FailFast,
}

impl Strategy {
    /// Whether calls may run at the same time.
    #[must_use]
    pub const fn is_parallel(self) -> bool {
        matches!(self, Self::Parallel)
    }

    /// Whether the first failure ends the batch.
    #[must_use]
    pub const fn stops_on_first_failure(self) -> bool {
        matches!(self, Self::FailFast)
    }

    /// A stable name for logs and for tests that compare readable output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel => "parallel",
            Self::FailFast => "fail-fast",
        }
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Chooses the strategy for one batch of calls requested together.
///
/// The rules, in the order they are applied:
///
/// 1. An empty batch is [`Strategy::Sequential`] — see the module documentation for why.
/// 2. Any destructive call makes the batch [`Strategy::Sequential`].
/// 3. A batch whose every call is read-only may run [`Strategy::Parallel`].
/// 4. Anything else — at least one write, mixed with reads or not — is [`Strategy::Sequential`].
///
/// [`Strategy::FailFast`] is never chosen automatically. It is an assertion that a batch's calls are
/// ordered dependencies of each other, which only the caller that built the batch can know.
#[must_use]
pub fn choose(calls: &[ToolCallInfo]) -> Strategy {
    if calls.is_empty() {
        return Strategy::Sequential;
    }
    if calls.iter().any(|call| call.destructive) {
        return Strategy::Sequential;
    }
    if calls.iter().all(ToolCallInfo::is_parallel_safe) {
        return Strategy::Parallel;
    }
    Strategy::Sequential
}
