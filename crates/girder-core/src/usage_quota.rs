//! A spend ceiling over a *moving* time window: how many tokens a deployment may use per hour, per day,
//! per billing period.
//!
//! [`crate::limits::Ceiling`] answers a different question. It is a ceiling for one session and it never
//! forgets: once a session has spent its tokens, they stay spent for the life of that session. That is
//! the right model for "this conversation may cost at most X", and the wrong one for "this deployment may
//! cost at most X per day", because the second must *forgive* yesterday's spend at midnight. A ceiling
//! that never forgets would stop a long-lived process after its first busy day and never let it work
//! again.
//!
//! A quota is therefore a ceiling plus a window, and the window is the part that carries the difficulty.
//!
//! # Why the window is sliding, and why records are dropped rather than flagged
//!
//! The tempting implementation is to keep every record and mark the old ones as expired, summing only the
//! live ones. That is a memory leak with a plausible story attached: a process that runs for a month and
//! records one usage per model call accumulates millions of dead entries, each of which is still walked
//! on every `totals` call. The cost grows with uptime rather than with traffic, which is the worst kind
//! of growth because it looks fine in a short test.
//!
//! So [`Tracker::record`] *removes* records that have fallen out of the window. What remains is bounded by
//! how many calls happen inside one window, which is a number the quota itself bounds. A day-long quota on
//! a busy deployment holds a day of calls and no more; the same deployment a month later holds the same
//! amount.
//!
//! # Why input and output are counted separately
//!
//! They are not the same product. Providers publish one rate for prompt tokens and a higher one for
//! completion tokens, routinely by a factor of three or more, so a single combined ceiling cannot express
//! "at most a million tokens" without silently pricing a million output tokens the same as a million
//! input ones. Counting them apart keeps the numbers a caller needs to apply either rate, and keeps a
//! verbose model from spending a quiet one's budget.
//!
//! # Why an over-quota record is still recorded
//!
//! A tracker that refused the record once the ceiling was passed would report a client that retried a
//! thousand times as one that stopped at the ceiling. The spend is real — the provider billed it — and the
//! number is the evidence for what to do next: raise the ceiling, or stop the client. Refusing the record
//! would also make the verdict permanent in a way that hides the cause, since every later call would see
//! the same "at the ceiling" total regardless of how much more it spent.
//!
//! The record therefore always lands, and the verdict is computed *after* it. What the caller does with a
//! refusal is the caller's decision, exactly as with [`crate::tool_limits`].
//!
//! # Why `window_ms == 0` means "no window" while `max_*_tokens == 0` means "none allowed"
//!
//! The two zeroes mean opposite things on purpose, because the two fields are asking different questions.
//! A zero window is a *configuration* — "count everything this process has ever done" — and `u64::MAX`
//! would be a number a reader mistakes for one. A zero token ceiling is a *ceiling*, and reading it as
//! unlimited would invert a hard stop into an open door; a deployment staging a rollout may genuinely want
//! "record, but allow nothing".
//!
//! # What this module does not do
//!
//! It does not read clocks. Every entry point takes `now_ms`, so the window arithmetic is testable without
//! sleeping and the caller decides whose clock is authoritative. It also knows nothing about prices:
//! [`Usage`] carries token counts, which is what the two independent limits are stated in. A cost ceiling
//! needs a rate per model, and this build has no pricing table — see
//! [`crate::token_meter::price_for`] for why one is deliberately absent.

use std::collections::VecDeque;
use std::fmt;

/// A spend ceiling over a moving window.
///
/// Both token ceilings are "at most", so a ceiling of `100` allows a total of exactly `100` and is passed
/// by the record that brings the total to `101`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    /// How long a record stays in the window, in milliseconds.
    ///
    /// `0` means no window: records accumulate for the life of the tracker. That is an intended
    /// configuration for a process whose lifetime is the billing period, not an error — see the module
    /// documentation for why it is the opposite convention from the token ceilings below.
    pub window_ms: u64,
    /// Most input (prompt) tokens allowed inside one window.
    ///
    /// `0` is a hard stop rather than "unlimited".
    pub max_input_tokens: u64,
    /// Most output (completion) tokens allowed inside one window.
    ///
    /// `0` is a hard stop rather than "unlimited".
    pub max_output_tokens: u64,
}

impl Quota {
    /// A quota with the window and both ceilings stated.
    #[must_use]
    pub const fn new(window_ms: u64, max_input_tokens: u64, max_output_tokens: u64) -> Self {
        Self {
            window_ms,
            max_input_tokens,
            max_output_tokens,
        }
    }

    /// A quota that never refuses and accumulates for the life of the tracker.
    ///
    /// The tracking still happens: a deployment that does not enforce a quota may still want
    /// [`Tracker::totals`] for a report, and a tracker that stopped counting would answer `0` for a
    /// process that has been serving for a week.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self::new(0, u64::MAX, u64::MAX)
    }

    /// Whether the quota has a window at all.
    #[must_use]
    pub const fn is_windowed(&self) -> bool {
        self.window_ms != 0
    }

    /// Whether a record made at `at_ms` has fallen out of the window as of `now_ms`.
    ///
    /// A record is live while it is strictly younger than the window, so a one-second window opened at
    /// `0` holds a record made at `999` and drops it at `1000`. Expiry is measured from the record's own
    /// timestamp rather than from a bucket boundary, which is what "sliding" means: there is no instant at
    /// which the whole total resets and a client can spend a full quota on either side of it.
    ///
    /// A `now_ms` earlier than `at_ms` is treated as "just now" by saturating subtraction. Callers are
    /// expected to pass a non-decreasing clock, but a clock that steps backwards must not resurrect a
    /// record as much younger than it is nor panic; keeping the record is the harmless choice.
    #[must_use]
    fn is_expired(&self, at_ms: u64, now_ms: u64) -> bool {
        self.is_windowed() && now_ms.saturating_sub(at_ms) >= self.window_ms
    }
}

impl Default for Quota {
    /// No window and no ceilings.
    ///
    /// The default has to be permissive for the same reason [`crate::limits::Ceiling`] defaults to
    /// unlimited: a deployment that does not charge should not acquire a spend ceiling because a field
    /// was omitted.
    fn default() -> Self {
        Self::unlimited()
    }
}

/// One measurement of what a call used.
///
/// A record rather than a running total, because the total depends on when it is asked for: the same
/// tracker answers differently one window later, and only the individual timestamps can say which
/// records still count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// When the usage happened, in milliseconds on the caller's clock.
    ///
    /// Milliseconds since an arbitrary origin, not a UTC instant: the tracker only ever subtracts two of
    /// these, so a monotonic clock is the honest source and a wall clock that jumps would distort a window
    /// for one call.
    pub at_ms: u64,
    /// Prompt tokens the call used.
    pub input_tokens: u64,
    /// Completion tokens the call used.
    pub output_tokens: u64,
}

impl Usage {
    /// One usage at `at_ms` in both directions.
    #[must_use]
    pub const fn new(at_ms: u64, input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            at_ms,
            input_tokens,
            output_tokens,
        }
    }
}

/// A sum of live usage, in both directions.
///
/// Named fields rather than a tuple so a caller cannot swap the two token counts, and a type rather than
/// two `u64`s so [`Tracker::remaining`] can return the same shape as [`Tracker::totals`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Prompt tokens summed over the live records.
    pub input_tokens: u64,
    /// Completion tokens summed over the live records.
    pub output_tokens: u64,
}

impl Totals {
    /// Nothing used, which is also the starting point of a fresh window.
    ///
    /// A constant rather than only [`Default`] because the interesting comparisons in a caller read better
    /// against a named zero, and because it is what a tracker with an empty window reports.
    pub const ZERO: Self = Self::new(0, 0);

    /// Both directions set.
    #[must_use]
    pub const fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }
}

/// What the tracker says about a record that just landed.
///
/// The verdict names which ceiling was passed and by what, because the two call for different responses:
/// an input overrun usually means the context grew, and an output overrun usually means the model rambled,
/// so a caller told "over quota" alone has to look the numbers up to find out which one to attack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Still inside both ceilings.
    Within,
    /// The input ceiling is passed.
    InputExceeded {
        /// Input tokens now in the window, including the record just made.
        used: u64,
        /// The ceiling they passed.
        max: u64,
    },
    /// The output ceiling is passed.
    OutputExceeded {
        /// Output tokens now in the window, including the record just made.
        used: u64,
        /// The ceiling they passed.
        max: u64,
    },
    /// Both ceilings are passed.
    BothExceeded {
        /// Input tokens now in the window.
        input: u64,
        /// Output tokens now in the window.
        output: u64,
    },
}

impl Verdict {
    /// Whether both ceilings are still satisfied.
    #[must_use]
    pub const fn is_within(&self) -> bool {
        matches!(self, Self::Within)
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Within => formatter.write_str("within quota"),
            Self::InputExceeded { used, max } => {
                write!(formatter, "input tokens over quota ({used} of {max})")
            }
            Self::OutputExceeded { used, max } => {
                write!(formatter, "output tokens over quota ({used} of {max})")
            }
            Self::BothExceeded { input, output } => {
                write!(
                    formatter,
                    "both token counts over quota ({input} in, {output} out)"
                )
            }
        }
    }
}

/// A quota and the usage still inside its window.
///
/// One tracker belongs to one spending subject — a deployment, a tenant, a session whose quota must
/// survive compaction. Which subject that is decides how the tracker is shared, and that decision is the
/// caller's: this type owns a `VecDeque` and no lock.
#[derive(Clone, Debug)]
pub struct Tracker {
    quota: Quota,
    /// Live records, oldest first in the common case.
    ///
    /// A deque because the steady-state operation is "drop from the front as records age out", which is
    /// `O(1)` there and `O(n)` in a `Vec`. It is not *assumed* to be sorted, though: a caller may replay a
    /// batch of usages whose timestamps interleave, so pruning filters the whole deque rather than popping
    /// the front. The cost is one walk per record over a deque the quota bounds, which is cheaper than the
    /// sorting a strict front-pop would require.
    entries: VecDeque<Usage>,
}

impl Tracker {
    /// A tracker with nothing spent.
    #[must_use]
    pub const fn new(quota: Quota) -> Self {
        Self {
            quota,
            entries: VecDeque::new(),
        }
    }

    /// The quota being tracked.
    #[must_use]
    pub const fn quota(&self) -> Quota {
        self.quota
    }

    /// Records one usage and says whether the quota still holds.
    ///
    /// The record lands whether or not it fits, so a client that keeps calling after a refusal keeps
    /// growing the total; see the module documentation for why the alternative hides the very spend it is
    /// supposed to bound.
    ///
    /// Expired records are discarded before the new one is counted, so the verdict reflects the window
    /// the caller asked about rather than a stale one.
    pub fn record(&mut self, usage: Usage) -> Verdict {
        self.prune(usage.at_ms);
        self.entries.push_back(usage);
        // Summed from the deque instead of accumulated in a field: a running total cannot be un-spent when
        // a record ages out, and recomputing keeps the number equal to what `totals` would report for the
        // same instant, which is the property a caller reading both relies on.
        let totals = self.sum_live(usage.at_ms);
        self.verdict_for(totals)
    }

    /// The totals inside the window as of `now_ms`.
    ///
    /// Takes `&self` and filters, rather than pruning: a caller asking a question should not change the
    /// answer the next caller gets, and a read that mutated would make `totals` and `remaining` order
    /// dependent. Memory is bounded by [`Tracker::record`], which is the one path that grows it.
    #[must_use]
    pub fn totals(&self, now_ms: u64) -> Totals {
        self.sum_live(now_ms)
    }

    /// How much of each ceiling is left as of `now_ms`.
    ///
    /// Clamped at zero, unlike [`crate::limits::Budget::remaining`], and the difference is deliberate: that
    /// method returns an `i64` because a negative remainder is information about a single overspending
    /// session, while this returns a `u64` capacity — the number of tokens that may still be recorded
    /// before the ceiling is passed. There is no such thing as a negative capacity. An overshoot is still
    /// visible, just where it belongs: compare [`Tracker::totals`] against [`Quota::max_input_tokens`].
    #[must_use]
    pub fn remaining(&self, now_ms: u64) -> Totals {
        let totals = self.sum_live(now_ms);
        Totals::new(
            self.quota
                .max_input_tokens
                .saturating_sub(totals.input_tokens),
            self.quota
                .max_output_tokens
                .saturating_sub(totals.output_tokens),
        )
    }

    /// How many records are physically held right now.
    ///
    /// Exposed because the interesting property is a memory one: records leave the deque when a later
    /// record prunes them, so this number tracks the window rather than the process's uptime. It can
    /// exceed the live count between two records — [`Tracker::totals`] ignores expired entries without
    /// removing them — which is why the live count is what the totals report and this is what bounds memory.
    #[must_use]
    pub fn records(&self) -> usize {
        self.entries.len()
    }

    /// Drops every record that has fallen out of the window as of `now_ms`.
    ///
    /// With no window there is nothing to expire, so the deque only grows; that is the documented meaning of
    /// `window_ms == 0`, and skipping the walk keeps an unwindowed tracker from paying `O(n)` per record for
    /// a retain that could not remove anything.
    fn prune(&mut self, now_ms: u64) {
        if !self.quota.is_windowed() {
            return;
        }
        let quota = self.quota;
        self.entries
            .retain(|usage| !quota.is_expired(usage.at_ms, now_ms));
    }

    /// Sums the records the window still holds, without mutating.
    ///
    /// Saturating: a total that overflowed would report a small number after an enormous spend, which is
    /// the opposite of what a quota is for.
    fn sum_live(&self, now_ms: u64) -> Totals {
        let quota = self.quota;
        let mut totals = Totals::default();
        for usage in &self.entries {
            if quota.is_expired(usage.at_ms, now_ms) {
                continue;
            }
            totals.input_tokens = totals.input_tokens.saturating_add(usage.input_tokens);
            totals.output_tokens = totals.output_tokens.saturating_add(usage.output_tokens);
        }
        totals
    }

    /// Compares live totals against the ceilings.
    ///
    /// `>` rather than `>=` so both ceilings mean "at most": a ceiling of `100` allows a total of exactly
    /// `100` and is passed by the record that brings it to `101`. Stopping *on* the ceiling would make
    /// "at most 100" refuse the hundredth token, which is a bug a reader only finds by running it.
    fn verdict_for(&self, totals: Totals) -> Verdict {
        let input_over = totals.input_tokens > self.quota.max_input_tokens;
        let output_over = totals.output_tokens > self.quota.max_output_tokens;
        match (input_over, output_over) {
            (false, false) => Verdict::Within,
            (true, false) => Verdict::InputExceeded {
                used: totals.input_tokens,
                max: self.quota.max_input_tokens,
            },
            (false, true) => Verdict::OutputExceeded {
                used: totals.output_tokens,
                max: self.quota.max_output_tokens,
            },
            (true, true) => Verdict::BothExceeded {
                input: totals.input_tokens,
                output: totals.output_tokens,
            },
        }
    }
}

impl Default for Tracker {
    /// A tracker with an unlimited quota, for the same reason [`Quota`] defaults that way.
    fn default() -> Self {
        Self::new(Quota::default())
    }
}
