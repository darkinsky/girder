//! Budgets: counting what a session has used against what it may use.
//!
//! A turn can cost money, tokens, and time. A budget is the ceiling on one of those, and this module owns the
//! counting: what was spent, what remains, and whether the next step fits.
//!
//! # Why a budget reports rather than enforces
//!
//! Enforcement belongs where the action happens — a token budget cannot stop a request already in flight. What a
//! caller needs from here is the number *before* it acts, and the decision about what to do when the ceiling is near
//! is the caller's. A budget that threw would make every caller handle an error it could have avoided by asking
//! first.
//!
//! # Why the remaining amount is not clamped at zero
//!
//! Overshoot is real and worth seeing. A budget that reported zero for any overshoot would hide that a single
//! request cost twice the ceiling, which is exactly the fact a person investigating an expensive session needs.
//!
//! # Why a budget can be unlimited
//!
//! A deployment that does not charge does not want a ceiling, and a limit of `u64::MAX` would be a number a reader
//! mistakes for one. [`Ceiling`] says "no limit" as a state, so a summary can print "unlimited" rather than a
//! fifteen-digit number.

/// A ceiling on one quantity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ceiling {
    /// No limit.
    ///
    /// The default: a deployment that does not charge should not acquire a ceiling because a field was omitted.
    #[default]
    Unlimited,
    /// At most this much.
    AtMost(u64),
}

impl Ceiling {
    /// Whether a further amount fits within the ceiling given what was already used.
    #[must_use]
    pub fn allows(&self, used: u64, additional: u64) -> bool {
        match self {
            Self::Unlimited => true,
            Self::AtMost(limit) => used.saturating_add(additional) <= *limit,
        }
    }

    /// The limit, when there is one.
    #[must_use]
    pub fn limit(&self) -> Option<u64> {
        match self {
            Self::Unlimited => None,
            Self::AtMost(limit) => Some(*limit),
        }
    }
}

/// One budget's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    limit: Ceiling,
    used: u64,
}

impl Budget {
    /// A budget with no ceiling.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// A budget with a ceiling.
    #[must_use]
    pub fn at_most(limit: u64) -> Self {
        Self {
            limit: Ceiling::AtMost(limit),
            used: 0,
        }
    }

    /// Records an amount as spent.
    ///
    /// Saturating rather than wrapping: a budget that overflowed would report a small number after a huge spend,
    /// which is the opposite of what a ceiling is for.
    pub fn spend(&mut self, amount: u64) {
        self.used = self.used.saturating_add(amount);
    }

    /// Records an amount as returned, for a spend that was reversed.
    ///
    /// Saturating at zero: a refund larger than the spend is a caller bug, and a budget that went negative would
    /// report that it had room it does not have.
    pub fn refund(&mut self, amount: u64) {
        self.used = self.used.saturating_sub(amount);
    }

    /// How much was spent.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used
    }

    /// How much remains.
    ///
    /// Not clamped at zero: overshoot is real, and a budget that hid it would conceal that one request cost twice the
    /// ceiling.
    #[must_use]
    pub fn remaining(&self) -> Option<i64> {
        match self.limit {
            Ceiling::Unlimited => None,
            Ceiling::AtMost(limit) => {
                let used = i64::try_from(self.used).unwrap_or(i64::MAX);
                let limit = i64::try_from(limit).unwrap_or(i64::MAX);
                Some(limit - used)
            }
        }
    }

    /// Whether another amount fits.
    #[must_use]
    pub fn allows(&self, additional: u64) -> bool {
        self.limit.allows(self.used, additional)
    }

    /// Whether the ceiling has been reached.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        !self.allows(1)
    }

    /// The ceiling.
    #[must_use]
    pub fn ceiling(&self) -> Ceiling {
        self.limit
    }
}

/// The budgets a session tracks.
///
/// Named rather than generic so a caller cannot confuse a token count with a byte count: the two are both `u64` and
/// both budgets, and mixing them would produce a spend that looks right and is not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Session {
    /// Tokens.
    pub tokens: Budget,
    /// Bytes of tool output.
    pub tool_bytes: Budget,
    /// Wall-clock milliseconds.
    pub milliseconds: Budget,
    /// Money in the smallest unit the deployment charges in.
    pub cost: Budget,
}

impl Session {
    /// A session with the given ceilings.
    #[must_use]
    pub fn with(
        tokens: Ceiling,
        tool_bytes: Ceiling,
        milliseconds: Ceiling,
        cost: Ceiling,
    ) -> Self {
        Self {
            tokens: Budget {
                limit: tokens,
                used: 0,
            },
            tool_bytes: Budget {
                limit: tool_bytes,
                used: 0,
            },
            milliseconds: Budget {
                limit: milliseconds,
                used: 0,
            },
            cost: Budget {
                limit: cost,
                used: 0,
            },
        }
    }

    /// Records one model call.
    pub fn record_tokens(&mut self, tokens: u64) {
        self.tokens.spend(tokens);
    }

    /// Records one tool result.
    pub fn record_tool_output(&mut self, bytes: u64) {
        self.tool_bytes.spend(bytes);
    }

    /// Records elapsed time.
    pub fn record_elapsed(&mut self, milliseconds: u64) {
        self.milliseconds.spend(milliseconds);
    }

    /// Records a charge.
    pub fn record_cost(&mut self, amount: u64) {
        self.cost.spend(amount);
    }

    /// Whether every ceiling still has room for `tokens` more.
    ///
    /// The question a caller asks before a model call: it is the one spend that cannot be undone, and a caller that
    /// discovered the ceiling afterwards has already paid.
    #[must_use]
    pub fn can_spend_tokens(&self, tokens: u64) -> bool {
        self.tokens.allows(tokens)
    }

    /// Which budgets are exhausted.
    #[must_use]
    pub fn exhausted(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        for (name, budget) in [
            ("tokens", self.tokens),
            ("tool_bytes", self.tool_bytes),
            ("milliseconds", self.milliseconds),
            ("cost", self.cost),
        ] {
            if budget.is_exhausted() {
                names.push(name);
            }
        }
        names
    }
}
