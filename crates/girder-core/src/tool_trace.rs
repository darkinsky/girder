//! Phase-level timing for one tool call: which stage the time went to.
//!
//! A tool call that took five seconds is one number, and that number does not say what to fix. The
//! five seconds are spread over stages that have nothing in common: the model emitting the
//! arguments, the harness checking them, a permission decision that may be waiting on a human, the
//! tool body itself, and the post-processing that trims or spills the result. A slow executor and a
//! slow approval queue both read as "five seconds" and need opposite remedies — one is fixed in the
//! tool, the other in the deployment's approval policy — so a trace that cannot separate the two
//! cannot be acted on.
//!
//! [`Phase`] is that separation, [`Trace`] is one call's record of it, and [`summarize`] folds a
//! batch of traces into the question a deployment actually asks: which stage is *repeatedly* slow,
//! rather than which single call was slow.
//!
//! # The clock is injected, not read
//!
//! Nothing here calls [`std::time::Instant::now`]. [`Trace::record`] takes a duration the caller
//! already measured, so the measurement seam is visible at every recording site and a test asserts
//! exact numbers instead of a range. This is the same rule [`crate::trace_span`] follows and for the
//! same reason: a module that read the clock itself could only be tested by sleeping, and a test
//! that sleeps is a test that fails on a loaded machine. It also keeps every function here pure,
//! which is what makes [`summarize`] a fold rather than a sampler.
//!
//! The durations are wall-clock milliseconds as the caller measured them. They are not CPU time, and
//! they deliberately do not subtract time spent waiting on a lock or on the network: "where did the
//! five seconds go" is a wall-clock question, because that is the time the user waited.
//!
//! # `Done` is a stage, and it is not the total
//!
//! [`Phase::Done`] records the hand-off — the result is in the conversation and the next request
//! will carry it — and it has its own small duration like every other stage. It is *not* the call's
//! total, and [`Trace::total_ms`] never special-cases it:
//!
//! - If `Done` carried the total, every total would be double-counted by the sum of the other
//!   phases, and the invariant that makes these numbers auditable — `total_ms` equals the sum of
//!   the recorded durations — would be false.
//! - [`Trace::slowest`] would be able to answer "the slowest stage is the whole call", which is not
//!   an answer.
//! - A trace with and a trace without `Done` could no longer be compared by their totals.
//!
//! What `Done` buys is the difference between *the tool returned* and *the result reached the
//! model*. A call whose `Executing` closed but which has no `Done` is stuck in the harness, not in
//! the tool, and that is exactly the failure a plain duration hides. [`Trace::is_done`] is that
//! question, and an unfinished trace is kept rather than discarded: the call that never finished is
//! the interesting one.
//!
//! # The same phase may be recorded more than once
//!
//! `phases` is an ordered list of `(phase, duration)` pairs, not a map. A retried tool body records
//! `Executing` twice, and both durations happened, so both are kept and both are summed. Folding
//! them into a map at record time would silently report a retried call as a single fast one.
//!
//! # Two different "slowest" questions
//!
//! [`Trace::slowest`] answers about one call: the largest recorded duration. [`Summary::slowest_phase`]
//! answers about a batch, and it is computed by **summing each phase across every call** before
//! comparing, not by taking the largest single duration seen anywhere. The distinction is the whole
//! point of the aggregate: a phase that costs 100ms in every one of twenty calls dominates the
//! session's wall time while never appearing as a single spike, and a report that ranked single
//! spikes would never mention it. One 3s outlier is a fact about one call; 20×100ms is a fact about
//! the system, and only the sum surfaces the second.
//!
//! # Ties go to the earlier phase
//!
//! Both reductions keep the phase that comes first in lifecycle order — the order [`Phase`]
//! declares its variants in — when two are equal. A tie is not rare (a call with one phase per
//! stage and no dominating one), and picking whichever the iterator happened to visit first would
//! make two runs of the same input disagree. "The earlier stage" is at least a rule a reader can
//! state: the time was already going before the later stage began.
//!
//! # Relationship to the other measurement modules
//!
//! - [`crate::trace_span`] nests spans inside *one request*. It answers "how long did this tool
//!   take, and what contained it". A trace answers "on what", inside a single call. The two compose:
//!   a `tool:bash` span can carry a trace as attributes, but neither replaces the other.
//! - [`crate::tool_usage`] and [`crate::SessionStats`] count calls and failures from the session
//!   log, and carry no durations at all because [`crate::SessionEvent`] has no timestamps. A trace
//!   is collected live, in the process doing the work, so the two read different sources.
//! - [`crate::tool_watchdog`] holds a *budget*, not a measurement: it says when to stop, not where
//!   the time went. A call that hit its deadline has the timeout as its `Executing` duration and
//!   nothing that says whether the time was spent working or blocked.
//!
//! # Wiring
//!
//! The crate root does not declare this module yet; declaring it and recording the phases inside
//! `agent_loop`'s dispatch belong to the integrator. Until then the tests include this file by path,
//! which is also why the examples above are fenced as `text`: the module path they would have to
//! name is the integrator's choice, and a doctest that guessed it would break on the wrong one.

use std::collections::BTreeMap;

/// One stage of a tool call.
///
/// The variants are declared in the order a call passes through them, and the derived
/// [`Ord`] follows that order. [`summarize`] and [`Trace::slowest`] rely on it to break ties
/// deterministically, so the declaration order is part of the contract rather than cosmetics.
///
/// The stages are cut where the *remedy* changes: a slow `Generating` is a model or provider
/// problem, a slow `Deciding` is a policy or human problem, and a slow `Executing` is a tool
/// problem. A finer split inside one of those would produce numbers that always move together and
/// therefore answer nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// The model is generating the arguments for the call.
    ///
    /// Measured around the model call that produced them, since the argument text arrives as part of
    /// the streamed response. One response can carry several calls, so this stage is shared between
    /// them and the caller decides how to attribute it; this module records whatever it is given and
    /// does not pretend the split is exact.
    Generating,
    /// The arguments are being validated before the body sees them.
    ///
    /// The size limits and the JSON parse both live here. They are cheap on a good call and are the
    /// whole cost of a bad one — a hundred-megabyte argument string is rejected without being parsed
    /// — which is why they get a stage instead of being folded into execution.
    Validating,
    /// Permission is being decided: rules, the pre-execute hooks, and any human approval.
    ///
    /// This is separated from `Executing` because it is the stage most likely to dominate a call and
    /// the least likely to be the tool's fault: a five-second approval wait and a five-second tool
    /// are the same number in a total and completely different findings. It is also the stage whose
    /// cost a deployment can usually remove with a rule.
    Deciding,
    /// The tool body is actually running.
    ///
    /// Wrapped in whatever deadline the caller enforces. A call that hit that deadline reports the
    /// deadline here, which says the call was stopped but not whether it was working or blocked.
    Executing,
    /// The result is being prepared for the model: sanitising, trimming to the budget, spilling, and
    /// appending whatever the harness adds to a failure.
    ///
    /// Cheap for a small result and expensive for a large one, and entirely the harness's cost — a
    /// tool is not slow because its output had to be trimmed. Keeping it separate is what makes that
    /// visible.
    PostProcessing,
    /// The result has been handed back and the call is over.
    ///
    /// Carries its own small hand-off duration (appending to the session, pushing the message) and is
    /// a marker rather than a total; see the module documentation for why the total is not recorded
    /// here.
    Done,
}

/// One tool call's phase timings.
///
/// The fields are public because a trace is data: a test builds one by hand to pin an exact shape,
/// and a consumer that renders or logs a trace walks it without an accessor for every field. The
/// accessors that exist ([`Trace::total_ms`], [`Trace::slowest`], [`Trace::is_done`]) are reductions,
/// not field reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    /// The provider-issued call id.
    ///
    /// Kept verbatim so a trace can be joined against the session log, where the same id names the
    /// `tool/call` and `tool/result` events. A trace whose id matches nothing in the log is a trace
    /// from a different session or a log that was pruned; the id is what makes that checkable.
    pub call_id: String,
    /// The tool's name, as the model wrote it.
    ///
    /// Not normalised, for the same reason [`crate::tool_usage`] does not normalise it: two spellings
    /// of one tool are two tools to the registry, and folding them here would report a breakdown no
    /// single tool has.
    pub tool: String,
    /// The phases in the order they were recorded, with their measured durations in milliseconds.
    ///
    /// A `Vec` rather than a map because order and repetition are both facts: order is how the call
    /// actually progressed, and a retried stage genuinely happened twice.
    pub phases: Vec<(Phase, u64)>,
    /// Whether the call failed, as the tool or the harness reported it.
    ///
    /// Set by the caller rather than inferred from `phases`: a failure is a verdict on the result,
    /// and the timings do not contain it. A denied call, a timed-out call and a body that returned an
    /// error are all failures here; which one it was is visible in the phases and in the result text.
    pub failed: bool,
}

impl Trace {
    /// Starts an empty trace for one call.
    ///
    /// `failed` starts false because nothing has failed yet, and phases are recorded as they happen;
    /// a trace therefore describes a call that is still running, which is why nothing here requires
    /// [`Phase::Done`] to be present.
    pub fn new(call_id: impl Into<String>, tool: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            phases: Vec::new(),
            failed: false,
        }
    }

    /// Appends one measured stage.
    ///
    /// Appending rather than replacing is deliberate: the same phase recorded twice means the stage
    /// ran twice (a retried body, a second approval round), and both durations were paid. Replacing
    /// would hide the retry in the one place it is cheapest to notice.
    ///
    /// The duration is the caller's measurement in milliseconds. This module never reads the clock,
    /// so a duration of `0` means the caller measured no elapsed time, not that the stage was skipped;
    /// a skipped stage is simply not recorded.
    pub fn record(&mut self, phase: Phase, duration_ms: u64) {
        self.phases.push((phase, duration_ms));
    }

    /// The sum of every recorded phase, in milliseconds.
    ///
    /// Saturating: a caller that measured something absurd gets a total that is still a number, and
    /// an overflow would panic in debug and wrap in release, which is the worst pair of behaviours
    /// for a diagnostic. The result is exactly the sum of [`Trace::phases`], never a separately
    /// measured wall time — those two can differ (time between stages is in neither) and conflating
    /// them would make the breakdown stop adding up to the total it is broken out of.
    #[must_use]
    pub fn total_ms(&self) -> u64 {
        self.phases
            .iter()
            .fold(0u64, |sum, &(_, ms)| sum.saturating_add(ms))
    }

    /// The single largest recorded duration, or `None` when nothing was recorded.
    ///
    /// Scans every entry rather than looking at the first or the last: the stages are not sorted by
    /// cost, and the whole purpose of the type is to answer this question. When two entries are
    /// equal the earlier recorded one wins, so the answer is a function of the input alone.
    ///
    /// This reports one occurrence. A phase recorded twice at 40ms each is reported as 40ms here even
    /// though it cost 80ms in total — use [`Trace::total_ms`] for what the call cost and
    /// [`summarize`] for what a phase costs across calls.
    #[must_use]
    pub fn slowest(&self) -> Option<(Phase, u64)> {
        let mut best: Option<(Phase, u64)> = None;
        for &(phase, ms) in &self.phases {
            let replace = match best {
                Some((_, best_ms)) => ms > best_ms,
                None => true,
            };
            if replace {
                best = Some((phase, ms));
            }
        }
        best
    }

    /// Whether the call reached [`Phase::Done`].
    ///
    /// An unfinished trace is kept rather than dropped, because the call that never reached `Done` is
    /// the one worth looking at: it says the harness stopped somewhere between the tool returning and
    /// the result reaching the conversation. A trace with no `Done` is not empty and is not a failure —
    /// it is a call whose last stage is whatever it recorded last.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.phases.iter().any(|&(phase, _)| phase == Phase::Done)
    }
}

/// An aggregate over a batch of traces.
///
/// This is the report a deployment reads, as opposed to the per-call record a single trace is. Its
/// fields are deliberately four numbers: a summary that listed every phase would be a table, and the
/// question it exists to answer fits in one line — how much time, in how many calls, where, and how
/// often it went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// How many calls were summarized, including empty and unfinished traces.
    ///
    /// A trace is one call, so this is the number of traces. An empty trace still counts: the call
    /// happened, and only its breakdown is unknown.
    pub calls: usize,
    /// The sum of every trace's [`Trace::total_ms`].
    ///
    /// A sum and not a wall-clock span, for the same reason the per-trace total is: phases can run in
    /// parallel across calls, so the sum can exceed the time the batch took, and that is the honest
    /// number for "how much work-time went into these calls".
    pub total_ms: u64,
    /// The phase with the largest **total** duration across all traces, or `None` when there is
    /// nothing recorded.
    ///
    /// Aggregated by summing each phase over every call before comparing, so repeated moderate cost
    /// outranks a single spike — see the module documentation. Ties keep the earlier phase in
    /// lifecycle order.
    pub slowest_phase: Option<(Phase, u64)>,
    /// How many of the summarized traces have [`Trace::failed`] set.
    ///
    /// Counted independently of the timings: a failed call still contributes its durations to
    /// [`Summary::total_ms`], because a failure that took four seconds cost four seconds. Excluding
    /// failures from the totals would make a batch of timeouts look fast.
    pub failures: usize,
}

/// Folds a batch of traces into one summary.
///
/// The reduction is a fold over the traces and nothing else: no clock, no I/O, so the same input
/// always produces the same summary and a test can pin it exactly. Phases are accumulated in a
/// [`BTreeMap`], whose iteration order is the variants' declaration order, which is what makes the
/// tie-break in [`Summary::slowest_phase`] deterministic.
///
/// An empty slice yields an all-zero summary with `slowest_phase: None` rather than a zero-duration
/// phase: "no call recorded anything" and "some phase took no measurable time" are different facts,
/// and reporting the second for the first would put a stage name in a report about no calls.
#[must_use]
pub fn summarize(traces: &[Trace]) -> Summary {
    let mut total_ms = 0u64;
    let mut failures = 0usize;
    let mut per_phase: BTreeMap<Phase, u64> = BTreeMap::new();

    for trace in traces {
        total_ms = total_ms.saturating_add(trace.total_ms());
        if trace.failed {
            failures += 1;
        }
        for &(phase, ms) in &trace.phases {
            let slot = per_phase.entry(phase).or_default();
            *slot = slot.saturating_add(ms);
        }
    }

    let mut slowest_phase: Option<(Phase, u64)> = None;
    for (phase, ms) in per_phase {
        let replace = match slowest_phase {
            Some((_, best_ms)) => ms > best_ms,
            None => true,
        };
        if replace {
            slowest_phase = Some((phase, ms));
        }
    }

    Summary {
        calls: traces.len(),
        total_ms,
        slowest_phase,
        failures,
    }
}
