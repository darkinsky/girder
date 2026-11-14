//! Per-call time budgets for tool invocations.
//!
//! The loop bounds a turn from the outside and [`tool_limits`](crate::tool_limits) bounds how many times a tool may
//! run, but neither bounds *how long one call may take*. A `bash` call that never returns therefore holds the whole
//! dispatch path open: the token in [`dispatch`](crate::agent_loop::dispatch) is checked when a tool starts and when
//! the loop regains control, and a tool that never yields never gives the loop that chance. The missing piece is the
//! arithmetic of "this call has been running too long", which is what this module supplies.
//!
//! # Why the clock is injected
//!
//! Every entry point takes `now_ms` instead of reading [`Instant::now`](std::time::Instant). The deadline is then a
//! pure function of the inputs, so a test can assert the boundary — exactly at the deadline counts as expired — with
//! no sleeps and no tolerance window. A module that read the clock itself could only be tested by waiting, and a
//! test that waits is either slow or flaky.
//!
//! # Why timeouts are per tool
//!
//! One number cannot serve both ends of the range. Thirty seconds is generous for `read` and far too short for
//! `bash`, where a build is a legitimate minute of work; a single global ceiling therefore either abandons builds or
//! lets a hung `read` occupy the turn. [`Policy::overrides`] states the exceptions, and the default covers the tools
//! nobody thought about.
//!
//! # Why `Report` exists
//!
//! Cancelling is not always the safe direction. A write that is half done, or a transaction that has been submitted,
//! leaves a worse state when the future is dropped mid-flight than when it is allowed to finish. For those tools the
//! policy says [`Policy::cancel_on_timeout`] is `false`, and the verdict after the deadline is
//! [`Verdict::Report`]: the caller records that the call overran and lets it complete, instead of trading a slow call
//! for a corrupt one.
//!
//! # What this module does not do
//!
//! It runs no tool, holds no timer, and never touches a token. [`Watch::verdict`] answers a question about a clock
//! reading; acting on the answer — racing the future against a timeout, cancelling the token, or only logging the
//! overrun — belongs to the caller that owns the execution.

use std::collections::BTreeMap;

/// How long tools may run before the caller treats the call as overrunning.
///
/// The default is deliberately one value for every tool rather than a table of guesses: a tool this module has never
/// heard of still needs a bound, and a bound that silently applies only to known names would be no bound at all for
/// the plugin-supplied tool that actually hangs.
///
/// Overrides may be longer or shorter than the default. They are not clamped to it, because the point of an override
/// is to disagree with it: `bash` needs minutes and `read` needs seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The timeout, in milliseconds, for a tool with no override.
    pub default_ms: u64,
    /// Per-tool timeouts in milliseconds, keyed by tool name. An entry replaces the default for that tool.
    pub overrides: BTreeMap<String, u64>,
    /// Whether an expired call should be cancelled rather than merely reported.
    ///
    /// `true` is right for a tool that only reads or starts a fresh subprocess. `false` is right for one whose
    /// interruption leaves a half-written file or a submitted transaction, where letting the call finish is the
    /// cheaper mistake.
    pub cancel_on_timeout: bool,
}

impl Policy {
    /// The timeout used when a policy states none explicitly.
    ///
    /// Thirty seconds is long enough for a `read`, an `ls`, or a web fetch, and short enough that a hung call does
    /// not stall a turn for a noticeable time. The expensive tools are exactly the ones a deployment is expected to
    /// override, which is why this number is a starting point rather than a claim about every tool.
    pub const DEFAULT_MS: u64 = 30_000;

    /// A policy with a stated default and cancellation choice, and no overrides.
    #[must_use]
    pub fn new(default_ms: u64, cancel_on_timeout: bool) -> Self {
        Self {
            default_ms,
            overrides: BTreeMap::new(),
            cancel_on_timeout,
        }
    }

    /// Adds or replaces the timeout for one tool.
    ///
    /// Takes the whole policy by value and returns it so a policy can be built in one expression; a builder that
    /// mutated in place would need a `&mut` the caller rarely has.
    #[must_use]
    pub fn with_override(mut self, tool: impl Into<String>, timeout_ms: u64) -> Self {
        self.overrides.insert(tool.into(), timeout_ms);
        self
    }

    /// The timeout, in milliseconds, that applies to `tool`.
    ///
    /// A missing key falls back to [`Policy::default_ms`]; that is the normal case, not an error, because a policy
    /// is written before the full set of tool names is known.
    #[must_use]
    pub fn timeout_for(&self, tool: &str) -> u64 {
        self.overrides.get(tool).copied().unwrap_or(self.default_ms)
    }
}

impl Default for Policy {
    /// The default policy: [`Policy::DEFAULT_MS`] for every tool, and cancel on expiry.
    ///
    /// Cancelling is the safer default because the common tools are read-only or spawn their own process; a tool
    /// that must not be interrupted has to say so with `cancel_on_timeout: false` rather than inherit a guess.
    fn default() -> Self {
        Self::new(Self::DEFAULT_MS, true)
    }
}

/// The timing of one tool call.
///
/// Constructed with [`Watch::start`], which fixes both the start and the timeout, so later questions need only a
/// clock reading. Copying the timeout in at start rather than re-reading the policy keeps the two answers consistent:
/// a policy edited mid-call cannot retroactively move a deadline that has already been reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watch {
    /// The tool being timed, kept so a report can name it without the caller threading it alongside the watch.
    tool: String,
    /// When the call started, in the caller's millisecond clock.
    started_ms: u64,
    /// How long this call may run, in milliseconds.
    timeout_ms: u64,
    /// Whether an expired call may be cancelled.
    cancel_on_timeout: bool,
}

impl Watch {
    /// Starts timing `tool` against `policy` at `now_ms`.
    #[must_use]
    pub fn start(tool: &str, policy: &Policy, now_ms: u64) -> Self {
        Self {
            tool: tool.to_owned(),
            started_ms: now_ms,
            timeout_ms: policy.timeout_for(tool),
            cancel_on_timeout: policy.cancel_on_timeout,
        }
    }

    /// The tool this watch times.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// How long this call may run, in milliseconds, fixed when the watch started.
    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// The moment the call expires, in the caller's millisecond clock.
    ///
    /// Saturating rather than wrapping: a very long timeout near `u64::MAX` must describe "effectively never" and
    /// not roll over into an already-expired deadline, which would cancel a call the policy meant to allow.
    #[must_use]
    pub fn deadline_ms(&self) -> u64 {
        self.started_ms.saturating_add(self.timeout_ms)
    }

    /// Whether the call has expired at `now_ms`.
    ///
    /// `>=`, not `>`, because the deadline is an inclusive instant: a call whose timeout is exactly reached has used
    /// all of its budget, and a strict comparison would grant one extra millisecond at every boundary. A timeout of
    /// zero is therefore expired the moment it starts, which is the honest reading of "no time allowed".
    #[must_use]
    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms >= self.deadline_ms()
    }

    /// How many milliseconds remain at `now_ms`, or `0` once the deadline has passed.
    ///
    /// Saturating subtraction is what makes the expired case `0`: going negative would need a signed type, and a
    /// caller reporting "3000 ms over" is not what this number is for — [`Watch::expired`] answers that question.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.deadline_ms().saturating_sub(now_ms)
    }

    /// What the caller should do about the call at `now_ms`.
    ///
    /// The three answers keep the policy's choice in one place: a call inside its budget is
    /// [`Verdict::Running`], and an expired call is [`Verdict::Cancel`] or [`Verdict::Report`] depending on the
    /// policy. A caller that only needs the boolean can use [`Watch::expired`], but using this method means the
    /// `cancel_on_timeout: false` decision is made once, here, instead of at every call site.
    #[must_use]
    pub fn verdict(&self, now_ms: u64) -> Verdict {
        if !self.expired(now_ms) {
            return Verdict::Running;
        }
        if self.cancel_on_timeout {
            Verdict::Cancel
        } else {
            Verdict::Report
        }
    }
}

/// What to do about a tool call at a given moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The call is still inside its budget.
    Running,
    /// The call has overrun and should be cancelled.
    Cancel,
    /// The call has overrun, but the policy forbids cancelling it: let it finish and record that it ran long.
    Report,
}

impl Verdict {
    /// Whether the call is still within its budget.
    #[must_use]
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    /// Whether the call has overrun, whether or not it may be cancelled.
    ///
    /// Separate from the [`Verdict::Cancel`] check because a report-worthy overrun still needs logging; a caller that
    /// only asked "should I cancel" would let those calls overrun silently.
    #[must_use]
    pub fn is_expired(self) -> bool {
        !self.is_running()
    }

    /// Whether cancelling the call is the policy's choice.
    #[must_use]
    pub fn should_cancel(self) -> bool {
        matches!(self, Self::Cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::{Policy, Verdict, Watch};

    #[test]
    fn a_tool_without_an_override_uses_the_default() {
        let policy = Policy::new(1_000, true);
        assert_eq!(policy.timeout_for("bash"), 1_000);
        assert_eq!(policy.timeout_for("read"), 1_000);
    }

    #[test]
    fn an_override_wins_over_the_default_in_both_directions() {
        let policy = Policy::new(30_000, true)
            .with_override("bash", 600_000)
            .with_override("read", 5_000);
        assert_eq!(policy.timeout_for("bash"), 600_000);
        assert_eq!(policy.timeout_for("read"), 5_000);
        assert_eq!(policy.timeout_for("glob"), 30_000);
    }

    #[test]
    fn the_deadline_is_the_start_plus_the_timeout() {
        let policy = Policy::new(1_500, true);
        let watch = Watch::start("bash", &policy, 10_000);
        assert_eq!(watch.deadline_ms(), 11_500);
        assert_eq!(watch.timeout_ms(), 1_500);
        assert_eq!(watch.tool(), "bash");
    }

    #[test]
    fn a_call_is_expired_exactly_at_its_deadline() {
        let watch = Watch::start("read", &Policy::new(1_000, true), 0);
        assert!(!watch.expired(999));
        assert!(watch.expired(1_000));
        assert!(watch.expired(1_001));
    }

    #[test]
    fn remaining_counts_down_and_floors_at_zero() {
        let watch = Watch::start("read", &Policy::new(1_000, true), 0);
        assert_eq!(watch.remaining_ms(0), 1_000);
        assert_eq!(watch.remaining_ms(400), 600);
        assert_eq!(watch.remaining_ms(1_000), 0);
        assert_eq!(watch.remaining_ms(5_000), 0);
    }

    #[test]
    fn a_call_inside_its_budget_is_running() {
        let watch = Watch::start("bash", &Policy::new(1_000, true), 0);
        assert_eq!(watch.verdict(999), Verdict::Running);
        assert!(watch.verdict(999).is_running());
    }

    #[test]
    fn an_expired_call_is_cancelled_when_the_policy_allows_it() {
        let watch = Watch::start("bash", &Policy::new(1_000, true), 0);
        assert_eq!(watch.verdict(1_000), Verdict::Cancel);
        assert!(watch.verdict(1_000).should_cancel());
        assert!(watch.verdict(1_000).is_expired());
    }

    #[test]
    fn an_expired_call_is_only_reported_when_cancelling_is_forbidden() {
        // The case this policy exists for: dropping a half-finished write would leave a worse state than waiting.
        let policy = Policy::new(1_000, false);
        let watch = Watch::start("write", &policy, 0);
        assert_eq!(watch.verdict(1_000), Verdict::Report);
        assert!(!watch.verdict(1_000).should_cancel());
        assert!(watch.verdict(1_000).is_expired());
    }

    #[test]
    fn a_zero_timeout_is_expired_at_once() {
        let watch = Watch::start("read", &Policy::new(0, true), 42);
        assert!(watch.expired(42));
        assert_eq!(watch.remaining_ms(42), 0);
        assert_eq!(watch.verdict(42), Verdict::Cancel);
    }

    #[test]
    fn a_huge_timeout_saturates_instead_of_wrapping() {
        let watch = Watch::start("bash", &Policy::new(u64::MAX, true), 10);
        assert_eq!(watch.deadline_ms(), u64::MAX);
        assert!(!watch.expired(u64::MAX - 1));
    }

    #[test]
    fn an_empty_override_table_falls_back_to_the_default() {
        let policy = Policy {
            default_ms: 2_000,
            overrides: std::collections::BTreeMap::new(),
            cancel_on_timeout: false,
        };
        assert_eq!(policy.timeout_for("anything"), 2_000);
        assert_eq!(Watch::start("anything", &policy, 0).deadline_ms(), 2_000);
    }

    #[test]
    fn the_default_policy_cancels_and_uses_the_stated_default() {
        let policy = Policy::default();
        assert_eq!(policy.default_ms, Policy::DEFAULT_MS);
        assert!(policy.cancel_on_timeout);
        assert!(policy.overrides.is_empty());
        assert_eq!(policy.timeout_for("bash"), Policy::DEFAULT_MS);
    }
}
