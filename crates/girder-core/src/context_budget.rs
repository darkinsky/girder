//! A pre-flight context budget: does one request fit the window once output is paid for?
//!
//! Upstream answers this in `compaction-basic`'s `resolveCompactSpec`, which subtracts
//! `reservedCompletionTokens` from the adapter-declared `contextWindow` before any pressure budget
//! is computed, and refuses a configuration where that subtraction leaves nothing. This module is
//! the same arithmetic reduced to the single question worth asking immediately before a request
//! leaves the process: *given what this request will carry, is there room left?*
//!
//! # Why this is not `compaction::needs_compaction`
//!
//! They answer different questions at different moments, and neither subsumes the other:
//!
//! - [`needs_compaction`](crate::needs_compaction) is a *pressure* signal. It compares a measured
//!   conversation against a fraction of the raw window and says "start compacting soon". It has no
//!   notion of how much of that window is reserved for output, so it never reports the case that
//!   matters most: a request that is comfortably under the raw window and still over what the input
//!   may actually occupy.
//! - This module is a *gate*. It prices one concrete request by component (system prompt, messages,
//!   tool schemas) and compares it against `window - reserve_output - margin`. A caller that gets
//!   [`Decision::Over`] knows exactly how many tokens it must shed, which is the number a compaction
//!   step needs in order to stop cutting.
//!
//! [`compaction_hook`](crate::compaction_hook) sits above both: its [`CompactionPolicy`] decides
//! *where* a history may be cut, in message counts, because that layer holds no tokenizer. Feeding
//! [`Decision::Over`] into that policy is the intended composition — this module produces the
//! target, the policy produces the cut.
//!
//! # Why the estimate is split into three components
//!
//! A request's cost is assembled from parts with different owners and different lifetimes: the
//! system prompt is rewritten per turn, the message history grows, and tool schemas are fixed but
//! large. Reporting them separately is what lets a caller react correctly — dropping tool schemas is
//! a different decision from summarising history — and a single opaque total would force it to guess
//! which one blew the budget.
//!
//! # Why a margin is part of the budget, not an afterthought
//!
//! Every token count here is an estimate (see [`token_meter`](crate::token_meter), whose character
//! heuristic undercounts CJK). An estimate compared against an exact limit has no defence against
//! its own error, so the error allowance has to be inside the arithmetic or the gate is decorative.
//! A request that lands exactly on the allowance is one estimate away from being rejected by the
//! provider mid-generation, which is the expensive way to find out.

/// One request's context budget.
///
/// The fields are absolute token counts rather than ratios because the decision at this layer is
/// absolute: a provider rejects a request for a fixed number of tokens, not for a percentage. The
/// ratios belong to the pressure policy in [`compaction`](crate::compaction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// The largest prompt-plus-completion total the model accepts.
    pub window: u64,
    /// Tokens held back for the model's own output.
    pub reserve_output: u64,
    /// Safety margin absorbing the error in the token estimate.
    pub margin: u64,
}

impl Budget {
    /// A budget from its three parts.
    ///
    /// Construction does not validate that the parts are coherent: a [`Budget`] whose
    /// `reserve_output` exceeds its `window` is a misconfiguration the caller should reject, but it
    /// is representable here so that [`input_allowance`](Self::input_allowance) can report the
    /// arithmetic consequence instead of this constructor panicking on it.
    #[must_use]
    pub const fn new(window: u64, reserve_output: u64, margin: u64) -> Self {
        Self {
            window,
            reserve_output,
            margin,
        }
    }

    /// The tokens actually available to the input.
    ///
    /// Output is subtracted first because generation happens *after* the request is accepted: a
    /// budget that spent the whole window on input would leave the model no room to answer, and the
    /// request would be accepted and then truncated mid-sentence. The margin is subtracted second,
    /// from what input may claim, because it guards the input estimate — the output reservation is a
    /// configured number, not an estimate, so it needs no allowance of its own.
    ///
    /// Saturating rather than wrapping: when the reservation meets or exceeds the window the answer
    /// is zero, never a colossal number produced by underflow. A caller that sees zero here has a
    /// configuration to fix, and it should see that plainly rather than as a wrapped value.
    #[must_use]
    pub const fn input_allowance(&self) -> u64 {
        self.window
            .saturating_sub(self.reserve_output)
            .saturating_sub(self.margin)
    }
}

/// What one request is estimated to cost, by component.
///
/// Every field is a token count from whichever estimator the caller uses. They are summed
/// saturating, so assembling an enormous request yields a huge number rather than a small one — the
/// failure mode a wrapping sum would produce, and the one that would silently let an oversized
/// request through this gate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// The system prompt.
    pub system: u64,
    /// The conversation messages, including tool results.
    pub messages: u64,
    /// The tool schemas offered to the model.
    pub tools: u64,
}

impl Usage {
    /// A usage from its three components.
    #[must_use]
    pub const fn new(system: u64, messages: u64, tools: u64) -> Self {
        Self {
            system,
            messages,
            tools,
        }
    }

    /// The total estimated cost of the request.
    ///
    /// Saturating at [`u64::MAX`]: a total that wrapped would compare as small against the
    /// allowance and admit a request that cannot possibly fit.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.system
            .saturating_add(self.messages)
            .saturating_add(self.tools)
    }
}

/// Whether a request fits, and by how much it does not.
///
/// The excess is carried as data rather than as a bare `false` because the caller's next move
/// depends on it: compacting by a guessed amount either fails again or throws away context that was
/// affordable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The request fits the input allowance and may be sent.
    Fits,
    /// The request does not fit; this many tokens must be shed first.
    Over {
        /// Tokens by which the estimate exceeds the input allowance.
        excess: u64,
    },
}

/// Decides whether one request fits its budget.
///
/// The comparison is against [`Budget::input_allowance`], not against [`Budget::window`]: the
/// window includes the output the model has yet to write, and a request measured against the whole
/// window would be admitted with no room to answer. Comparing against the allowance is what makes
/// [`Decision::Over`] mean "shed tokens before sending" rather than "the provider will object".
///
/// An empty usage against a zero allowance is [`Decision::Fits`]: the check compares cost to
/// allowance, and a request carrying nothing has no cost to shed. A zero allowance because the
/// window is smaller than the reservation is a configuration error for the caller to refuse, and
/// this function reports it through the allowance rather than by panicking on it.
#[must_use]
pub fn check(budget: &Budget, usage: &Usage) -> Decision {
    let allowance = budget.input_allowance();
    let total = usage.total();
    if total <= allowance {
        Decision::Fits
    } else {
        Decision::Over {
            excess: total - allowance,
        }
    }
}
