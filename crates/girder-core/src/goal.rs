//! Goals: a durable objective a session keeps working toward.
//!
//! A goal is the difference between "answer this message" and "keep going until this is done". It
//! carries an objective, a lifecycle phase, and a round budget, and every mutation is a revision so
//! two concurrent edits cannot silently overwrite each other.
//!
//! # Phase and activation are two different things
//!
//! | concept | persisted | meaning |
//! |---|---|---|
//! | [`Phase`] | yes | where the goal is in its lifecycle |
//! | [`Activation`] | no | whether *this process* may continue it automatically |
//!
//! Conflating them looks simpler and is wrong. A goal that is `active` on disk was active when the
//! process that wrote it stopped; a fresh process must not resume an objective on its own just
//! because the last one was mid-flight. The user asks to continue, or the goal stays disarmed. That
//! is why `resume` is a human-authorized transition rather than something a restart does.
//!
//! # Why every mutation is a revision
//!
//! Two clients can each hold a view of a goal and both edit it. Without a compare-and-set, the second
//! write wins and the first client's change vanishes with no indication. [`GoalRef`] carries the
//! revision the caller saw, and a mismatch is refused.
//!
//! # The transition table
//!
//! | from | pause | resume | complete | block |
//! |---|---|---|---|---|
//! | active | yes | (already armed: no) | yes | yes |
//! | paused | — | yes | yes | — |
//! | blocked | — | yes | yes | — |
//! | complete | — | — | — | — |
//!
//! A completed goal is terminal: reopening it would make "complete" mean "probably complete".

/// Identifies one goal across its durable revisions.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GoalId(String);

impl GoalId {
    /// Builds an id.
    ///
    /// # Errors
    ///
    /// Returns `None` for an empty id: an id that names nothing cannot identify a goal.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GoalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A compare-and-set identity for one exact revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalRef {
    /// The goal.
    pub id: GoalId,
    /// A positive revision, incremented by every durable mutation.
    pub revision: u64,
}

/// Where a goal is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Being worked on.
    Active,
    /// Stopped, resumable.
    Paused,
    /// Stopped because something concrete blocks progress.
    Blocked,
    /// Done.
    Complete,
}

impl Phase {
    /// The wire name, matching the upstream spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
        }
    }

    /// Whether a goal in this phase can be resumed.
    #[must_use]
    pub fn is_resumable(self) -> bool {
        matches!(self, Self::Active | Self::Paused | Self::Blocked)
    }

    /// Whether a goal in this phase can still reach completion.
    #[must_use]
    pub fn is_open(self) -> bool {
        !matches!(self, Self::Complete)
    }
}

/// Whether the live process may continue an active goal automatically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Activation {
    /// Automatic continuation is allowed.
    Armed,
    /// It is not.
    ///
    /// The default, because a process that has not been told to continue must not.
    #[default]
    Disarmed,
}

/// Why a goal is blocked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockReason {
    /// A stable classification a policy branches on.
    pub code: String,
    /// A non-empty explanation for a human or a model.
    pub message: String,
}

impl BlockReason {
    /// Builds a reason.
    ///
    /// # Errors
    ///
    /// Returns `None` for an empty message: a block with no explanation leaves the user with a goal
    /// that stopped for a reason nobody can read.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Option<Self> {
        let message = message.into();
        if message.trim().is_empty() {
            return None;
        }
        Some(Self {
            code: code.into(),
            message,
        })
    }
}

/// A goal's full durable state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Goal {
    /// Its identity.
    pub id: GoalId,
    /// Its revision; every mutation increments it.
    pub revision: u64,
    /// What the user asked for.
    pub objective: String,
    /// Where it is.
    pub phase: Phase,
    /// Present exactly while `phase` is `Blocked`.
    pub blocked_reason: Option<BlockReason>,
    /// How many continuation rounds it may take.
    pub max_rounds: u32,
    /// How many rounds it has taken.
    pub rounds_started: u32,
    /// When it was created, in milliseconds since the epoch.
    pub created_at: u64,
}

impl Goal {
    /// The compare-and-set identity of this exact revision.
    #[must_use]
    pub fn reference(&self) -> GoalRef {
        GoalRef {
            id: self.id.clone(),
            revision: self.revision,
        }
    }

    /// Whether the round budget is spent.
    #[must_use]
    pub fn rounds_exhausted(&self) -> bool {
        self.rounds_started >= self.max_rounds
    }
}

/// The default round cap when a caller names none.
///
/// A default rather than unlimited: a goal whose objective is unreachable would otherwise continue
/// forever, spending tokens every round with no way to notice.
pub const DEFAULT_MAX_GOAL_ROUNDS: u32 = 25;

/// Why a goal mutation was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GoalError {
    /// The caller's revision is not the current one.
    #[error("goal `{id}` is at revision {actual}, not {expected}")]
    StaleRevision {
        /// The goal.
        id: String,
        /// What the caller saw.
        expected: u64,
        /// What is stored.
        actual: u64,
    },

    /// The transition is not allowed from this phase.
    #[error("goal `{id}` cannot {operation} from {phase}")]
    InvalidTransition {
        /// The goal.
        id: String,
        /// The operation asked for.
        operation: &'static str,
        /// The phase it is in.
        phase: Phase,
    },

    /// The objective was empty.
    #[error("a goal needs an objective")]
    EmptyObjective,

    /// A block needs a readable reason.
    #[error("a blocked goal needs a non-empty reason")]
    EmptyBlockReason,

    /// A resume of an already-active, already-armed goal.
    #[error("goal `{id}` is already active and armed")]
    AlreadyArmed {
        /// The goal.
        id: String,
    },

    /// The operation names a goal that is not current.
    #[error("goal `{id}` is not the current goal")]
    NotCurrent {
        /// The goal.
        id: String,
    },
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The goal state a session holds, with its live activation.
#[derive(Clone, Debug, Default)]
pub struct GoalState {
    current: Option<Goal>,
    activation: Activation,
}

impl GoalState {
    /// Creates an empty state, disarmed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: None,
            activation: Activation::Disarmed,
        }
    }

    /// The current goal, if any.
    #[must_use]
    pub fn current(&self) -> Option<&Goal> {
        self.current.as_ref()
    }

    /// The live activation.
    #[must_use]
    pub fn activation(&self) -> Activation {
        self.activation
    }

    /// Whether automatic continuation may happen now.
    ///
    /// Requires all three: a goal exists, it is active, and it is armed. Any one missing means a
    /// continuation would be wrong — continuing a paused goal ignores the user's pause, and
    /// continuing a disarmed one ignores a restart.
    #[must_use]
    pub fn may_continue(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|goal| goal.phase == Phase::Active && self.activation == Activation::Armed)
    }

    /// Creates a goal, arming it.
    ///
    /// A newly created goal is armed because the user just asked for it. A goal loaded from disk
    /// starts disarmed — see [`GoalState::restore`].
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::EmptyObjective`] for a blank objective, and
    /// [`GoalError::InvalidTransition`] when a goal already exists.
    pub fn create(
        &mut self,
        id: GoalId,
        objective: impl Into<String>,
        max_rounds: Option<u32>,
        now: u64,
    ) -> Result<Goal, GoalError> {
        let objective = objective.into();
        if objective.trim().is_empty() {
            return Err(GoalError::EmptyObjective);
        }
        if let Some(existing) = &self.current {
            if existing.phase.is_open() {
                return Err(GoalError::InvalidTransition {
                    id: existing.id.to_string(),
                    operation: "create",
                    phase: existing.phase,
                });
            }
        }
        let goal = Goal {
            id,
            revision: 1,
            objective,
            phase: Phase::Active,
            blocked_reason: None,
            // A cap of zero would make the goal unable to take a single round, so an explicit zero
            // is treated as the default rather than as a trap.
            max_rounds: max_rounds
                .filter(|cap| *cap > 0)
                .unwrap_or(DEFAULT_MAX_GOAL_ROUNDS),
            rounds_started: 0,
            created_at: now,
        };
        self.current = Some(goal.clone());
        self.activation = Activation::Armed;
        Ok(goal)
    }

    /// Loads a goal from durable state, leaving it disarmed.
    ///
    /// The essential restart rule: the phase survives, the activation does not. A process that
    /// resumed an objective on its own because the previous one was mid-flight would act without
    /// being asked.
    pub fn restore(&mut self, goal: Goal) {
        self.current = Some(goal);
        self.activation = Activation::Disarmed;
    }

    /// The state a session holds.
    #[must_use]
    pub fn goal(&self) -> Option<&Goal> {
        self.current.as_ref()
    }

    fn check(&self, reference: &GoalRef) -> Result<&Goal, GoalError> {
        let goal = self.current.as_ref().ok_or_else(|| GoalError::NotCurrent {
            id: reference.id.to_string(),
        })?;
        if goal.id != reference.id {
            return Err(GoalError::NotCurrent {
                id: reference.id.to_string(),
            });
        }
        if goal.revision != reference.revision {
            return Err(GoalError::StaleRevision {
                id: goal.id.to_string(),
                expected: reference.revision,
                actual: goal.revision,
            });
        }
        Ok(goal)
    }

    fn advance(&mut self, phase: Phase, activation: Activation) -> Goal {
        let goal = self.current.as_mut().expect("checked by the caller");
        goal.revision += 1;
        goal.phase = phase;
        // A reason belongs exactly to a blocked phase; leaving a stale one behind would show the
        // previous reason on a goal that is no longer blocked.
        if phase != Phase::Blocked {
            goal.blocked_reason = None;
        }
        self.activation = activation;
        goal.clone()
    }

    /// Pauses an active goal.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::InvalidTransition`] from any phase but active.
    pub fn pause(&mut self, reference: &GoalRef) -> Result<Goal, GoalError> {
        let phase = self.check(reference)?.phase;
        if phase != Phase::Active {
            return Err(GoalError::InvalidTransition {
                id: reference.id.to_string(),
                operation: "pause",
                phase,
            });
        }
        Ok(self.advance(Phase::Paused, Activation::Disarmed))
    }

    /// Resumes a stopped goal, arming it.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::InvalidTransition`] from a completed phase, and
    /// [`GoalError::AlreadyArmed`] for an active goal that is already armed.
    pub fn resume(&mut self, reference: &GoalRef) -> Result<Goal, GoalError> {
        let phase = self.check(reference)?.phase;
        if !phase.is_resumable() {
            return Err(GoalError::InvalidTransition {
                id: reference.id.to_string(),
                operation: "resume",
                phase,
            });
        }
        // An active, armed goal is already doing what a resume would ask for; recording a no-op
        // revision would make the log claim a change that did not happen.
        if phase == Phase::Active && self.activation == Activation::Armed {
            return Err(GoalError::AlreadyArmed {
                id: reference.id.to_string(),
            });
        }
        Ok(self.advance(Phase::Active, Activation::Armed))
    }

    /// Marks an open goal complete.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::InvalidTransition`] from the complete phase.
    pub fn complete(&mut self, reference: &GoalRef) -> Result<Goal, GoalError> {
        let phase = self.check(reference)?.phase;
        if !phase.is_open() {
            return Err(GoalError::InvalidTransition {
                id: reference.id.to_string(),
                operation: "complete",
                phase,
            });
        }
        Ok(self.advance(Phase::Complete, Activation::Disarmed))
    }

    /// Marks an active goal blocked.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::InvalidTransition`] from any phase but active, and
    /// [`GoalError::EmptyBlockReason`] for an unreadable reason.
    pub fn block(&mut self, reference: &GoalRef, reason: BlockReason) -> Result<Goal, GoalError> {
        let phase = self.check(reference)?.phase;
        if phase != Phase::Active {
            return Err(GoalError::InvalidTransition {
                id: reference.id.to_string(),
                operation: "block",
                phase,
            });
        }
        if reason.message.trim().is_empty() {
            return Err(GoalError::EmptyBlockReason);
        }
        let goal = self.advance(Phase::Blocked, Activation::Disarmed);
        let goal = Goal {
            blocked_reason: Some(reason),
            ..goal
        };
        self.current = Some(goal.clone());
        Ok(goal)
    }

    /// Edits the objective or the round cap.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::EmptyObjective`] for a blank objective, and
    /// [`GoalError::InvalidTransition`] for a completed goal: editing a finished goal would let it
    /// mean something the user never approved.
    pub fn edit(
        &mut self,
        reference: &GoalRef,
        objective: Option<String>,
        max_rounds: Option<u32>,
    ) -> Result<Goal, GoalError> {
        let phase = self.check(reference)?.phase;
        if !phase.is_open() {
            return Err(GoalError::InvalidTransition {
                id: reference.id.to_string(),
                operation: "edit",
                phase,
            });
        }
        if let Some(objective) = &objective {
            if objective.trim().is_empty() {
                return Err(GoalError::EmptyObjective);
            }
        }
        let goal = self.current.as_mut().expect("checked above");
        goal.revision += 1;
        if let Some(objective) = objective {
            goal.objective = objective;
        }
        if let Some(cap) = max_rounds.filter(|cap| *cap > 0) {
            goal.max_rounds = cap;
        }
        Ok(goal.clone())
    }

    /// Records one continuation round.
    ///
    /// # Errors
    ///
    /// Returns [`GoalError::InvalidTransition`] when the goal is not continuable, and
    /// [`GoalError::InvalidTransition`] with `rounds` when the budget is spent.
    pub fn start_round(&mut self, now: u64) -> Result<Goal, GoalError> {
        let _ = now;
        let goal = self.current.as_ref().ok_or_else(|| GoalError::NotCurrent {
            id: "(none)".to_owned(),
        })?;
        if goal.phase != Phase::Active || self.activation != Activation::Armed {
            return Err(GoalError::InvalidTransition {
                id: goal.id.to_string(),
                operation: "continue",
                phase: goal.phase,
            });
        }
        if goal.rounds_exhausted() {
            return Err(GoalError::InvalidTransition {
                id: goal.id.to_string(),
                operation: "continue",
                phase: goal.phase,
            });
        }
        let goal = self.current.as_mut().expect("checked above");
        goal.rounds_started += 1;
        Ok(goal.clone())
    }

    /// Clears the goal entirely.
    ///
    /// Returns what was cleared, so a caller can log it. A completed goal is the normal thing to
    /// clear; an open one is cleared too, because "clear" is the user's explicit discard.
    pub fn clear(&mut self) -> Option<Goal> {
        self.activation = Activation::Disarmed;
        self.current.take()
    }
}

/// Validates that a `GoalRef` matches the goal it names.
///
/// A free function so a caller holding only a reference can check without a mutable state.
///
/// # Errors
///
/// Returns [`GoalError::StaleRevision`] or [`GoalError::NotCurrent`].
pub fn check_reference(goal: &Goal, reference: &GoalRef) -> Result<(), GoalError> {
    if goal.id != reference.id {
        return Err(GoalError::NotCurrent {
            id: reference.id.to_string(),
        });
    }
    if goal.revision != reference.revision {
        return Err(GoalError::StaleRevision {
            id: goal.id.to_string(),
            expected: reference.revision,
            actual: goal.revision,
        });
    }
    Ok(())
}
