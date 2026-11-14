//! Per-tool budgets inside one turn: how many times a tool may run, and how many failures in a row it may make.
//!
//! The loop already bounds a turn from the outside. [`LoopContext::max_turns`](crate::agent_loop::LoopContext)
//! decides how many turns run, the token [`Ceiling`](crate::limits::Ceiling) decides what a turn may cost, and
//! [`crate::subagent::Depth`] decides how far delegation may go. None of those bounds *what a single turn does*: one
//! turn is a chain of steps, each step may request a batch of tool calls, and nothing on that path counts them. A
//! model that decides to re-run `bash` until the build passes can spend a turn — and most of the context window —
//! on the same call fifty times, and the loop reports it as one long but otherwise normal turn. The budget here is
//! the missing counter.
//!
//! # Why the count is per tool
//!
//! A total across all tools cannot distinguish the two cases that matter. Forty `read` calls on forty files is a
//! model doing its job; forty `bash` calls is a model thrashing, because each one costs a process, a timeout window,
//! and a result that lands in the window. A global counter has to sit above the expensive case to catch it, and then
//! it refuses the cheap one too. Counting per tool lets the ceiling sit where the expensive tool needs it while the
//! chatty one spends what it needs.
//!
//! # Why failures are counted in a row
//!
//! A tool that fails is the one case where another attempt is sometimes right: a corrected argument, a file that now
//! exists. What is never right is the same failure five times, which is what a model does when it cannot see why the
//! call failed — it resends it. Counting *consecutive* failures, rather than all of them, is what keeps a long turn
//! legitimate: a `bash` that fails once, succeeds, fails once, succeeds is working, and any total would eventually
//! stop it. A success therefore clears the run, and the same tool may fail again afterwards.
//!
//! # Why the verdict carries a message
//!
//! A refusal that the model cannot read is a refusal it will retry. The stop is only useful if the model is told
//! both that it stopped and what to do instead, which is why [`Verdict`] is an enum with [`Verdict::explain`]
//! rather than a `bool`: "too many calls" alone invites one more call with different whitespace, while "finish the
//! work another way" is a change of approach.
//!
//! # What this module does not decide
//!
//! It runs nothing and knows no tool's name in advance. [`Tracker::record`] is called by the caller that just
//! executed (or is about to execute) a call, and the caller owns the consequence: skipping the call, answering the
//! model with the explanation, or ending the turn. Keeping the decision separate is what makes the arithmetic — and
//! the wording — testable without a loop, a provider, or a session.

use std::collections::BTreeMap;
use std::fmt;

/// How many calls one tool may make in a turn, and how many failures in a row it may make.
///
/// Both numbers are "at most", so a ceiling of `3` allows three and refuses the fourth. `0` is therefore a hard
/// stop rather than an "off" switch: a tool that may not be called at all is a configuration a deployment might
/// want for one turn, and reading `0` as unlimited would silently invert it. [`Limit::unlimited`] is the way to
/// say "no ceiling".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    /// Most calls allowed in one turn, per tool.
    pub max_calls: u32,
    /// Most consecutive failures allowed, per tool.
    pub max_consecutive_failures: u32,
}

impl Limit {
    /// A budget with both ceilings stated.
    #[must_use]
    pub const fn new(max_calls: u32, max_consecutive_failures: u32) -> Self {
        Self {
            max_calls,
            max_consecutive_failures,
        }
    }

    /// A budget that never refuses.
    ///
    /// `u32::MAX` rather than an `Option`, because the tracking still happens: a deployment that does not enforce
    /// the budget may still want [`Tracker::calls`] for a report, and a tracker that stopped counting would answer
    /// `0` for a tool that ran fifty times.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self::new(u32::MAX, u32::MAX)
    }
}

impl Default for Limit {
    /// The same ceilings for every tool, which is the honest default at this layer.
    ///
    /// The tracker cannot know which of two tool names is the expensive one, so one number has to cover both:
    /// forty is above what a reading loop needs and low enough that forty shells is caught. A deployment that knows
    /// its tools should state its own numbers; per-tool limits would need a configuration shape this module does not
    /// have.
    fn default() -> Self {
        Self::new(40, 5)
    }
}

/// What one tool has done so far this turn.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    /// Calls recorded, including ones a verdict refused.
    calls: u32,
    /// Failures since this tool's last success.
    consecutive_failures: u32,
}

/// What a call may do, as judged by the tracker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The call is within budget.
    Allow,
    /// This tool has been called too many times this turn.
    TooManyCalls {
        /// The tool that is out of budget.
        tool: String,
        /// The ceiling it passed.
        limit: u32,
    },
    /// This tool has failed too many times in a row.
    TooManyFailures {
        /// The tool that keeps failing.
        tool: String,
        /// How many failures in a row it has now made.
        failures: u32,
        /// The ceiling that run passed.
        limit: u32,
    },
}

impl Verdict {
    /// Whether the call may proceed.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The text to hand the model when it is not allowed to continue.
    ///
    /// Written as an instruction rather than a status because the reader is a model that has to choose its next
    /// call: a message naming what to do instead is the difference between a change of approach and one more
    /// identical attempt. [`Verdict::Allow`] also has text, so a caller can log the decision without matching on the
    /// variant first.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Self::Allow => "Within budget. Continue with the current approach.".to_owned(),
            Self::TooManyCalls { tool, limit } => format!(
                "`{tool}` has reached its limit of {limit} calls for this turn. Finish the work another way: use a \
                 different tool, make one broader call instead of {limit} narrow ones, or work from the results you \
                 already have. Calling `{tool}` again will be refused."
            ),
            Self::TooManyFailures {
                tool,
                failures,
                limit,
            } => format!(
                "`{tool}` has failed {failures} times in a row, which is the limit of {limit}. Repeating the same \
                 call is unlikely to succeed: change what you are asking for, use a different tool, or tell the user \
                 what is blocking you instead of retrying."
            ),
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allow => formatter.write_str("allow"),
            Self::TooManyCalls { tool, limit } => {
                write!(formatter, "{tool}: too many calls (limit {limit})")
            }
            Self::TooManyFailures {
                tool,
                failures,
                limit,
            } => write!(
                formatter,
                "{tool}: too many consecutive failures ({failures}, limit {limit})"
            ),
        }
    }
}

/// A budget and what each tool has spent against it this turn.
///
/// One tracker belongs to one turn. [`Tracker::reset`] at the start of the next one is not optional bookkeeping: a
/// turn's budget exists because a turn is the unit a model is asked to finish in, and a tracker that outlived its
/// turn would stop a session at its first busy turn and never let it work again.
#[derive(Clone, Debug)]
pub struct Tracker {
    limits: Limit,
    counts: BTreeMap<String, Counts>,
}

impl Tracker {
    /// A tracker with nothing spent.
    #[must_use]
    pub const fn new(limits: Limit) -> Self {
        Self {
            limits,
            counts: BTreeMap::new(),
        }
    }

    /// Records one call and says whether it may proceed.
    ///
    /// The attempt is counted whether or not it is allowed. The number then answers "how many times did the model
    /// ask for this tool", which is the fact a person diagnosing a runaway turn needs; a count that stopped at the
    /// ceiling would report a model that asked forty times as one that asked five. A caller that checks the verdict
    /// *before* executing still gets a stable answer: the refusal does not reset, so every later attempt is refused
    /// too.
    ///
    /// The call ceiling is checked before the failure ceiling. Once no further call of this tool is allowed, the
    /// failure run is moot, and a model told both would have to guess which limit it is actually against.
    pub fn record(&mut self, tool: &str, succeeded: bool) -> Verdict {
        let counts = self.counts.entry(tool.to_owned()).or_default();
        counts.calls = counts.calls.saturating_add(1);
        if succeeded {
            counts.consecutive_failures = 0;
        } else {
            counts.consecutive_failures = counts.consecutive_failures.saturating_add(1);
        }

        // Read before the borrow of `tool` is needed again: `counts` borrows the map that `self.limits` does not.
        let calls = counts.calls;
        let failures = counts.consecutive_failures;

        if calls > self.limits.max_calls {
            return Verdict::TooManyCalls {
                tool: tool.to_owned(),
                limit: self.limits.max_calls,
            };
        }
        // `>` rather than `>=` so both fields mean "at most": a ceiling of three allows three calls and three
        // failures, and refuses the fourth. Stopping *on* the third would make the two ceilings disagree about what
        // a limit of three means, which is a bug a reader has to run the code to discover.
        if failures > self.limits.max_consecutive_failures {
            return Verdict::TooManyFailures {
                tool: tool.to_owned(),
                failures,
                limit: self.limits.max_consecutive_failures,
            };
        }
        Verdict::Allow
    }

    /// Clears every count, for the start of a new turn.
    ///
    /// Everything goes, including the counts of tools that never came near a ceiling. A partial reset would make the
    /// budget depend on the history of previous turns, which is exactly the state a turn boundary is meant to drop.
    pub fn reset(&mut self) {
        self.counts.clear();
    }

    /// How many times `tool` was called this turn, including calls a verdict refused.
    #[must_use]
    pub fn calls(&self, tool: &str) -> u32 {
        self.counts.get(tool).map_or(0, |counts| counts.calls)
    }

    /// How many failures in a row `tool` has made since its last success.
    #[must_use]
    pub fn consecutive_failures(&self, tool: &str) -> u32 {
        self.counts
            .get(tool)
            .map_or(0, |counts| counts.consecutive_failures)
    }
}
