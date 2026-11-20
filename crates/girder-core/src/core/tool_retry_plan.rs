//! Deciding what to do *after* a tool call has already failed.
//!
//! Three modules already cover the three halves of this problem, and none of them answers the
//! question that connects them:
//!
//! - [`crate::Watch`] measures a call against its budget and reports [`crate::WatchdogVerdict`]
//!   once the deadline passes. It answers *has this call overrun*, not *should it run again*.
//! - [`crate::core::tool_retry`] classifies a failure and says whether that class is worth another
//!   attempt. Its own documentation names the gap this module closes: its failure enum has no
//!   timeout variant, so a timeout and a deterministic internal error land in the same class and
//!   neither can be opted back in.
//! - [`crate::util::backoff`] computes how long retry *n* waits. It takes a status code or nothing at
//!   all; it has no notion of a tool, a side effect, or an uncertain outcome.
//!
//! So the composition was missing: an expired watchdog verdict has to become a classified failure,
//! that failure has to be judged against what repeating the call would *repeat*, and only then does
//! backoff have anything to say. This module is that junction, and it is a pure function of its
//! inputs so the policy can be tested without a clock, a tool, or a loop.
//!
//! # Why a timeout is the dangerous failure
//!
//! Every other failure tells you where things stand. A parse error means nothing ran. A reported
//! error means the tool ran and stopped. A timeout means the deadline passed while the body was
//! still executing — and the process that would have told you the outcome was the one that got
//! dropped. The call may have finished, half-finished, or not started; all three look identical
//! from the outside.
//!
//! That uncertainty is not a property of the failure alone. It becomes harmless or catastrophic
//! depending on what the call would do a second time, which is why [`plan`] takes `idempotent`
//! alongside the failure instead of deciding on the failure kind alone:
//!
//! - **Idempotent + timeout → retry.** A `read` that ran twice returns the same bytes. The only
//!   cost of being wrong is one wasted syscall, so retrying is strictly better than spending a
//!   human's attention on it.
//! - **Non-idempotent + timeout → ask.** A `write` that is retried may append, truncate, or apply
//!   its effect twice; a `write` that is abandoned loses work the user can see. Neither branch is
//!   safe to take automatically, and the module cannot tell which one the world is in. Only a
//!   human — or a caller that can inspect the effect — can.
//!
//! # Why an explicit failure is retried either way
//!
//! [`Kind::Error`] is defined as a failure the tool *reported*, which is the tool saying its body
//! did not complete. Nothing partial was left behind, so there is nothing for a retry to repeat:
//! idempotency does not enter into it. This is deliberately narrower than "the call failed
//! somehow" — a failure the tool could not report is a timeout, and timeouts take the other path.
//!
//! # Why bad arguments are never retried
//!
//! [`Kind::BadArguments`] is not a transient condition: the arguments are fixed text, and a retry
//! resends the same fixed text. The answer cannot change, so every attempt after the first is a
//! guaranteed waste of the caller's budget. Note that this is a statement about *resent* calls, not
//! about the model's next turn — [`crate::core::tool_retry`] correctly retries a schema violation when
//! the model is expected to rewrite the arguments, and that remains true; what this module refuses
//! is repeating the identical call.
//!
//! # What this module does not do
//!
//! It does not sleep, dispatch, or cancel anything, and it never reads a clock. It returns a
//! [`Plan`] and the caller executes it — which is also why the jittered draw is not taken here:
//! the delay has to be the same number when it is logged and when it is slept, and a decision
//! layer that sampled an RNG could not be asserted on at all.

use ::girder_core::BackoffStrategy;

/// The wait schedule for a retried tool call.
///
/// Jitter is disabled on purpose. [`crate::util::backoff`] jitters to stop a herd of clients that failed
/// together from retrying together, which is a property of many clients against one server; a tool
/// retry is one agent against its own filesystem, where the dispersion buys nothing and the cost is
/// a delay that cannot be asserted on. A caller that does want jitter can add its own draw before
/// sleeping — it owns the sleep, and the `after_ms` of [`Plan::Retry`] is the number it should
/// shift.
///
/// The base is the same order as [`BackoffStrategy::transient`]: a tool that failed is usually
/// ready to succeed on the next attempt, and a long wait only makes the turn feel stuck.
const RETRY: BackoffStrategy = BackoffStrategy {
    base_ms: 500,
    multiplier: 2,
    max_ms: 10_000,
    jitter: 0.0,
};

/// The draw passed for a jitter-free strategy.
///
/// Exactly `0.5` is the value that leaves the computed delay undisplaced, so the constant documents
/// the intent better than a bare literal at the call site would.
const NO_DISPLACEMENT: f64 = 0.5;

/// What to do after one failed attempt.
///
/// Each variant is an instruction the caller can execute without re-deriving anything: [`Plan::Retry`]
/// carries the wait, and the two refusals carry the sentence to show, because a caller that has to
/// recompute the reason at the call site tends to recompute it slightly differently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Run the same call again after `after_ms`.
    ///
    /// The call is unchanged. A retry that is meant to carry *corrected* arguments is not this
    /// variant — that is a new call, and [`crate::core::tool_retry`] owns the message that asks the model
    /// for it.
    Retry {
        /// How long to wait first, in milliseconds. Never zero: an immediate re-dispatch of a call
        /// that just failed is a busy loop when the failure is not instantaneous.
        after_ms: u64,
        /// The attempt number the retry *will be*, one-based.
        ///
        /// The next attempt rather than the failed one, because the caller's natural use is
        /// `failure.attempt + 1` when it builds the next [`Failure`], and an off-by-one here would
        /// silently buy one extra attempt in a loop that trusts this field.
        attempt: u32,
    },
    /// Do not retry; report the failure to the model as the call's result.
    ///
    /// The error is a fact about this attempt and the model can act on it in its next turn. Handing
    /// it over is the right end for a failure that a repeat cannot change.
    GiveUp {
        /// Why, in a sentence suitable for a log line or a model-facing message.
        why: String,
    },
    /// Do not retry, and do not report to the model yet: a human has to look at the world first.
    ///
    /// Distinct from [`Plan::GiveUp`] because the two produce opposite next turns. A `GiveUp` is an
    /// error the model may respond to by calling the tool again, which is exactly what must not
    /// happen when the previous call may have left a half-applied effect. An `Ask` is a stop.
    Ask {
        /// Why, naming the tool and the state that is unknown, so the human is not asked to
        /// reconstruct the situation from a log.
        why: String,
    },
}

/// One failed attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The tool the model asked for, as the registry spells it.
    pub tool: String,
    /// Which attempt this was, counting from one.
    ///
    /// One-based like [`crate::core::tool_retry::Policy::should_retry`], and for the same reason: the
    /// first failure of a call is attempt `1`, and a caller that counts from zero would get a
    /// budget one attempt longer than it configured.
    pub attempt: u32,
    /// What went wrong.
    pub kind: Kind,
}

impl Failure {
    /// Builds one failed attempt.
    #[must_use]
    pub fn new(tool: impl Into<String>, attempt: u32, kind: Kind) -> Self {
        Self {
            tool: tool.into(),
            attempt,
            kind,
        }
    }
}

/// Why an attempt failed.
///
/// The three variants are separated by *what the failure proves about the world*, which is the only
/// distinction the retry decision needs. Two failures that prove the same thing would be one
/// variant here even if they arrived through different code paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The call overran its deadline.
    ///
    /// The body may have run to completion, run partway, or never started; a cancelled future
    /// reports nothing, so no observation distinguishes the three. This is the variant that makes
    /// [`plan`] consult `idempotent`.
    Timeout,
    /// The tool ran and reported a failure, so its effect did not complete.
    ///
    /// Covers the environment faults a tool can name — a missing file, an unreachable host, a
    /// subprocess that exited non-zero. A failure the tool could not report because it was
    /// interrupted is a [`Kind::Timeout`], not this.
    Error {
        /// The tool's message, passed through unchanged. The retry decision does not inspect it:
        /// classifying this text would mean every tool's wording leaks into the policy.
        message: String,
    },
    /// The arguments were rejected before the tool ran, or the tool rejected them on sight.
    ///
    /// Nothing executed, and the arguments will be byte-identical on a resend.
    BadArguments,
}

impl Kind {
    /// Stable code for logs and metrics.
    ///
    /// Kept separate from the `Debug` rendering so a log parser is not coupled to derive output,
    /// and separate from the model-facing text so renaming a message never breaks a dashboard.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Timeout => "TIMEOUT",
            Self::Error { .. } => "ERROR",
            Self::BadArguments => "BAD_ARGUMENTS",
        }
    }
}

/// Decides the next step after `failure`.
///
/// `idempotent` says whether repeating the call repeats a side effect; it is supplied by the caller
/// rather than looked up here because the answer is a property of the tool registry this layer does
/// not own, and a caller that knows the tool should not have to go through a name table to say so.
/// `max_attempts` is the total budget including the attempt that just failed, so `1` means "this
/// call gets exactly one try" and every failure is final.
///
/// `max_attempts` bounds *automatic* retries only. It cannot convert an unknown state into a known
/// one, so an exhausted budget on a non-idempotent timeout still yields [`Plan::Ask`]: reporting
/// [`Plan::GiveUp`] there would hand the model an error it may answer by resending the very call
/// whose effect is in doubt.
///
/// The rules, in the order they are applied:
///
/// 1. [`Kind::BadArguments`] — [`Plan::GiveUp`]. The arguments are fixed text; a resend fails
///    identically, so neither the idempotency of the tool nor the remaining budget is relevant.
/// 2. [`Kind::Timeout`] with `idempotent == false` — [`Plan::Ask`], before the budget is consulted.
///    The call may have left a partial effect, and both available automatic branches are wrong.
/// 3. An attempt that has used up `max_attempts` (or that is `0`, which no real failure should
///    report) — [`Plan::GiveUp`]. Nothing remains to spend.
/// 4. Otherwise — [`Plan::Retry`]. Either the tool is idempotent and the repeat is harmless, or the
///    failure was reported and therefore left nothing to repeat.
#[must_use]
pub fn plan(failure: &Failure, idempotent: bool, max_attempts: u32) -> Plan {
    let tool = failure.tool.as_str();

    // Rule 1: an argument error is a property of text that a retry resends verbatim. It is placed
    // first so that no other rule can accidentally override it with a retry.
    if failure.kind == Kind::BadArguments {
        return Plan::GiveUp {
            why: format!(
                "`{tool}` rejected its arguments; a retry would resend exactly the same arguments \
                 and be rejected the same way, so the failure is reported as-is"
            ),
        };
    }

    // Rule 2: the only case where the module genuinely does not know what the world looks like.
    // Answered before the budget because `max_attempts` is a limit on spending, not evidence.
    let uncertain = failure.kind == Kind::Timeout && !idempotent;
    if uncertain {
        return Plan::Ask {
            why: format!(
                "`{tool}` timed out on attempt {}, and repeating it is not safe: a non-idempotent \
                 tool may have already applied part or all of its effect, so a retry could apply it \
                 twice while reporting the error could let the model re-run it blindly. Check what \
                 the call changed before deciding.",
                failure.attempt
            ),
        };
    }

    // Rule 3: the budget is spent. `attempt == 0` is not a real failure and is refused rather than
    // read as "no attempts yet", which would grant a retry on an undefined attempt number.
    if failure.attempt == 0 || failure.attempt >= max_attempts {
        return Plan::GiveUp {
            why: exhausted_why(failure, max_attempts),
        };
    }

    // Rule 4: nothing above objected, so a repeat cannot make things worse.
    let after_ms = RETRY.delay_ms(failure.attempt, NO_DISPLACEMENT);
    Plan::Retry {
        after_ms,
        attempt: failure.attempt.saturating_add(1),
    }
}

/// Builds the [`Plan::GiveUp`] reason for a spent attempt budget.
///
/// Separate so the two ways into rule 3 — a real budget and a nonsensical attempt number — explain
/// themselves differently; a single format string would have to say something true of neither.
fn exhausted_why(failure: &Failure, max_attempts: u32) -> String {
    let tool = failure.tool.as_str();
    let kind = failure.kind.code();
    if failure.attempt == 0 {
        return format!(
            "`{tool}` reported failure {kind} on attempt 0, which is not a real attempt number; \
             refusing to retry a failure whose position in the sequence is undefined"
        );
    }
    let detail = match &failure.kind {
        Kind::Error { message } if !message.trim().is_empty() => format!(" It reported: {message}"),
        Kind::Error { .. } => " It reported no message.".to_owned(),
        Kind::Timeout | Kind::BadArguments => String::new(),
    };
    format!(
        "`{tool}` failed with {kind} on attempt {} of {max_attempts}, so the attempt budget is \
         spent.{detail}",
        failure.attempt
    )
}
