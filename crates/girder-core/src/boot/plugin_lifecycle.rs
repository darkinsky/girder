//! Plugin lifecycle: the states a plugin moves through, and which moves are permitted.
//!
//! [`crate::extensions::Registry`] already answers "is this extension running?". That answer is
//! deliberately small — three statuses, one of them a failure — because a registry that also had to
//! explain *how* a plugin arrived somewhere would carry audit policy into the code that starts it.
//! This module is the other half: a record of the path, so an operator can ask "why is this plugin
//! not running?" and get an answer better than "stopped".
//!
//! # The transition table
//!
//! | from | to | cause |
//! |---|---|---|
//! | `Discovered` | `Approved` | `OperatorApproved` |
//! | `Discovered` | `Removed` | `OperatorRemoved` |
//! | `Approved` | `Starting` | `OperatorStarted` |
//! | `Approved` | `Removed` | `OperatorRemoved` |
//! | `Starting` | `Running` | `StartSucceeded` |
//! | `Starting` | `Failed` | `StartFailed` |
//! | `Running` | `Stopped` | `OperatorStopped` |
//! | `Running` | `Failed` | `DependencyLost` |
//! | `Stopped` | `Starting` | `OperatorStarted` |
//! | `Stopped` | `Removed` | `OperatorRemoved` |
//! | `Failed` | `Starting` | `OperatorStarted` |
//! | `Failed` | `Removed` | `OperatorRemoved` |
//!
//! Everything else is refused. The table is stated here as prose *and* encoded in [`allows`], and a
//! test drives all forty-nine state pairs through both to keep the two from drifting apart.
//!
//! # Why the table is keyed on states and not on causes
//!
//! [`allows`] takes two states, so a caller cannot smuggle `Running -> Stopped` past the record by
//! labelling it `DependencyLost`. That leaves the reverse risk: the table permits
//! `Running -> Stopped`, and a caller could label a dependency loss as an operator stop. Causes are
//! therefore recorded, not policed — they are evidence for a human reading the log, and a machine
//! that lies in the log is a bug in the machine, not something a four-line predicate can prevent.
//! The mapping each cause is *meant* to produce is the table above, and the tests exercise it.
//!
//! # Why `Failed` is not a dead end, but `Failed -> Running` is still refused
//!
//! A retry must be possible: a plugin whose dependency was missing is exactly the plugin an operator
//! retries after installing it. But a retry has to pass through [`State::Starting`], for two
//! reasons. It records that a second attempt happened, so the log shows `failed, starting, running`
//! rather than a failure that silently turned into success. And it puts the retry's own failure
//! where it belongs: a retry that fails again lands in `Failed` again, with its own reason, instead
//! of overwriting the first one or being mistaken for one long outage.
//!
//! # Why a lost dependency lands in `Failed` and not in `Stopped`
//!
//! [`State::Stopped`] means the plugin ended because someone decided it should — the normal outcome
//! of a normal stop, after which an operator may restart it and expect the same result. A plugin
//! torn down because a service it needs vanished did not stop normally, and reporting it as stopped
//! invites a restart that will fail the same way while hiding the reason. `Failed` carries that
//! reason, which is the whole difference between the two states.
//!
//! # Why `Removed` is terminal
//!
//! Removal is the one operation that discards the definition, so there is nothing left to approve,
//! start, or stop. Every pair with `Removed` on the left is refused before the table is consulted,
//! rather than relying on no arm happening to match: a state that can only be left through a hole
//! someone adds later is not terminal, it is merely terminal today.
//!
//! # What is deliberately not here
//!
//! The record does not start, stop, or inspect a plugin; [`History`] is a value, not a handle. That
//! keeps it usable in a test and in a replay of a persisted log, and keeps the decision "may this
//! move happen?" in one predicate that both a caller and an auditor can run.
//!
//! # Why the surface is allowed to look unused
//!
//! A host drives a plugin through these states; this crate only describes the path. Until the
//! re-export in `lib.rs` has a caller, `dead_code` would otherwise report a library surface as
//! waste. The allow says that plainly instead of leaving a reader to wonder whether it is unused.

use std::fmt;

/// A plugin's lifecycle state.
///
/// Ordered by how far along the path a plugin is, not by declaration: the interesting question is
/// which states a given one may move to, and naming them here does not decide that — [`allows`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Discovered but not approved.
    ///
    /// A definition exists and nothing has run it. It is not "stopped": stopping implies a start
    /// that a human authorised, and this one has not been authorised yet.
    Discovered,
    /// Approved, not running.
    Approved,
    /// Starting.
    ///
    /// A separate state because a start is not instantaneous and may not finish at all; a plugin
    /// that is mid-start has no settled outcome to report.
    Starting,
    /// Running.
    Running,
    /// Stopped normally.
    Stopped,
    /// Start-up failed.
    Failed {
        /// Why it failed.
        ///
        /// Carried in the state rather than beside it so a reader of the final state learns the
        /// reason without searching back through the transitions.
        reason: String,
    },
    /// Removed.
    ///
    /// Terminal: see [`State::is_terminal`] and the module documentation.
    Removed,
}

impl State {
    /// Whether no transition may leave this state.
    ///
    /// Only removal is final. A failed plugin is not terminal — it can be retried — and neither is
    /// a stopped one.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Removed)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovered => f.write_str("discovered"),
            Self::Approved => f.write_str("approved"),
            Self::Starting => f.write_str("starting"),
            Self::Running => f.write_str("running"),
            Self::Stopped => f.write_str("stopped"),
            Self::Failed { reason } => write!(f, "failed ({reason})"),
            Self::Removed => f.write_str("removed"),
        }
    }
}

/// What triggered a transition.
///
/// Kept with the transition rather than derived from the pair of states, because
/// `Running -> Failed` can only be one cause while `Stopped -> Starting` can be a first start or a
/// retry, and the log is read to recover that difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cause {
    /// An operator approved it.
    OperatorApproved,
    /// An operator started it.
    OperatorStarted,
    /// An operator stopped it.
    OperatorStopped,
    /// An operator removed it.
    ///
    /// Distinct from [`Cause::OperatorStopped`] even though a removal also ends a run: the log is
    /// asked whether a plugin was set aside or merely halted, and one cause for both makes that
    /// question unanswerable. Without this variant no transition could legally reach
    /// [`State::Removed`], which would make its terminality untestable.
    OperatorRemoved,
    /// Start-up succeeded.
    StartSucceeded,
    /// Start-up failed, with the reason.
    StartFailed(String),
    /// A service it needed disappeared, named here.
    DependencyLost(String),
}

/// One transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    /// The state the plugin was in.
    ///
    /// Asserted by the caller and checked against the record: see [`History::apply`].
    pub from: State,
    /// The state it moved to.
    pub to: State,
    /// Who or what caused the move.
    pub cause: Cause,
}

/// Checks whether one transition is permitted.
///
/// This is the single statement of the table in the module documentation; [`History::apply`] calls
/// it rather than duplicating the rules, so a caller can ask in advance without holding a record.
///
/// `Failed -> Running` is false on purpose: a retry re-enters [`State::Starting`], which records the
/// attempt and gives its failure somewhere to land. `Running -> Removed` is false on purpose too —
/// stop first, so the log shows the shutdown rather than a removal that happens to have included
/// one. And `Removed` on the left is always false: removal is final.
#[must_use]
pub fn allows(from: &State, to: &State) -> bool {
    // Removal discards the definition, so there is nothing left to move. Checked here rather than
    // left to a missing arm, so a later arm cannot quietly make `Removed` non-terminal.
    if from.is_terminal() {
        return false;
    }
    // Grouped by source state, so one arm reads as one row of the table in the module docs.
    matches!(
        (from, to),
        (State::Discovered, State::Approved | State::Removed)
            | (
                State::Approved | State::Stopped | State::Failed { .. },
                State::Starting | State::Removed
            )
            | (State::Starting, State::Running | State::Failed { .. })
            | (State::Running, State::Stopped | State::Failed { .. })
    )
}

/// One plugin's lifecycle record.
///
/// Starts at [`State::Discovered`] and keeps every accepted transition. The current state is the
/// last transition's destination; both are stored so a reader does not have to replay the list to
/// answer the common question.
#[derive(Clone, Debug)]
pub struct History {
    current: State,
    transitions: Vec<Transition>,
}

impl History {
    /// Creates a record for a freshly discovered plugin.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: State::Discovered,
            transitions: Vec::new(),
        }
    }

    /// Applies a transition.
    ///
    /// The `from` the caller states is checked against the record before the table is consulted, so
    /// a stale caller — one that believed the plugin was somewhere it no longer is — is refused
    /// instead of having its mistaken view written into the log. A refused transition changes
    /// nothing: neither the state nor the list.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError::Illegal`] when the stated `from` disagrees with the record, or
    /// when [`allows`] refuses the pair.
    pub fn apply(&mut self, transition: Transition) -> Result<(), TransitionError> {
        if transition.from != self.current {
            return Err(TransitionError::Illegal {
                from: transition.from.to_string(),
                to: transition.to.to_string(),
            });
        }
        if !allows(&self.current, &transition.to) {
            return Err(TransitionError::Illegal {
                from: self.current.to_string(),
                to: transition.to.to_string(),
            });
        }
        self.current = transition.to.clone();
        self.transitions.push(transition);
        Ok(())
    }

    /// The current state.
    #[must_use]
    pub fn current(&self) -> &State {
        &self.current
    }

    /// Every transition, oldest first.
    #[must_use]
    pub fn transitions(&self) -> &[Transition] {
        &self.transitions
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

/// A transition the record refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// The transition is not permitted.
    ///
    /// Both states are named, so the message says where the plugin was and where it was asked to
    /// go. Blaming only the destination would leave a reader unable to tell a skipped state from a
    /// state the plugin had already left.
    #[error("illegal transition: {from} -> {to}")]
    Illegal {
        /// The state the plugin was in, as reported in the message.
        from: String,
        /// The state it was asked to move to.
        to: String,
    },
}
