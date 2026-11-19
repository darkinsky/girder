//! `session.wait`: the stopping decision, ported as a pure function.
//!
//! # What upstream has
//!
//! `packages/api/session-controller/src/state/controller.ts` implements `wait`, which observes a
//! Session until one of four things is true and then answers with the reason. The loop is
//! transport-shaped -- it holds a listener, a deadline and an async iterator -- but the *decision*
//! inside it is one small function:
//!
//! ```typescript
//! function computeWaitAction(state: SessionState): 'input-required' | 'continue' | 'idle' | 'check-idle' {
//!   if (state.pendingInput.length > 0) return 'input-required'
//!   if (state.running) return 'continue'
//!   if (!state.active) return 'idle'
//!   return 'check-idle'
//! }
//! ```
//!
//! That is the part worth porting exactly, and it is here. The order matters and is not
//! interchangeable: a session that is both running and waiting for input must report
//! `input-required`, because the run cannot progress and the person is what it is waiting on.
//! Checking `running` first would make `wait` block on a run that is itself blocked.
//!
//! # What is not here
//!
//! The observation loop. It needs a live listener over a session's event stream, which is the
//! `SessionState` source `session.state` already reads; this module answers *what should happen next*
//! for a state, and a caller that has a state source drives it. Splitting it this way is what makes
//! the decision testable without a socket.

use serde_json::{Value, json};

/// What `wait` should do after seeing one state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitAction {
    /// Stop: the session is waiting for the person, not for the agent.
    InputRequired,
    /// Keep watching: a run is in progress.
    Continue,
    /// Stop: nothing is running and the session is not open.
    Idle,
    /// Stop when the driver reports idle, which needs a second reading.
    CheckIdle,
}

impl WaitAction {
    /// The wire spelling, matching upstream's discriminator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InputRequired => "input-required",
            Self::Continue => "continue",
            Self::Idle => "idle",
            Self::CheckIdle => "check-idle",
        }
    }

    /// Whether this action ends the wait.
    ///
    /// `CheckIdle` does not: upstream's loop asks the driver whether it is idle and only then
    /// decides, so treating it as terminal would report a stop the driver never confirmed.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::InputRequired | Self::Idle)
    }
}

/// The three facts the decision reads.
///
/// A struct rather than the full `SessionStateValue` because these are the only fields the rule
/// consults, and naming them makes the rule's inputs obvious to a reader who has never seen the
/// session state type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitFacts {
    /// Whether anything is waiting for the person.
    pub pending_input: bool,
    /// Whether a run is in progress.
    pub running: bool,
    /// Whether the session is open at all.
    pub active: bool,
}

/// Decides what `wait` should do next.
///
/// The order is upstream's and is load-bearing: `pending_input` before `running`, because a run
/// blocked on a question cannot progress and `wait` must say so rather than time out.
#[must_use]
pub fn wait_action(facts: WaitFacts) -> WaitAction {
    if facts.pending_input {
        return WaitAction::InputRequired;
    }
    if facts.running {
        return WaitAction::Continue;
    }
    if !facts.active {
        return WaitAction::Idle;
    }
    WaitAction::CheckIdle
}

/// Reads the three facts from a session state the way `session.state` renders it.
///
/// `active` is derived rather than read: upstream's `SessionState.active` is true while the session
/// has a live driver, and this build's state carries the same information as the absence of an
/// `interrupted` flag on a session that has events. A session with no events has never run, so it is
/// not active.
#[must_use]
pub fn facts_of(state: &crate::session_state_rpc::SessionStateValue) -> WaitFacts {
    WaitFacts {
        // This build's state has no `pendingInput` field yet; a state that reports `interrupted` is
        // waiting for the person to decide whether to continue, which is the same requirement.
        pending_input: state.interrupted,
        running: state.running,
        active: state.events > 0,
    }
}

/// `session.wait`'s value for one stopping point.
#[must_use]
pub fn wait_value(
    action: WaitAction,
    state: &crate::session_state_rpc::SessionStateValue,
) -> Value {
    json!({
        "reason": action.as_str(),
        "state": state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_question_wins_over_a_running_agent() {
        // The order upstream chose, and the only order that terminates: a run blocked on a question
        // cannot finish on its own, so reporting `continue` would wait forever.
        let action = wait_action(WaitFacts {
            pending_input: true,
            running: true,
            active: true,
        });
        assert_eq!(action, WaitAction::InputRequired);
        assert!(action.is_terminal());
    }

    #[test]
    fn a_running_agent_continues() {
        let action = wait_action(WaitFacts {
            pending_input: false,
            running: true,
            active: true,
        });
        assert_eq!(action, WaitAction::Continue);
        assert!(!action.is_terminal());
    }

    #[test]
    fn an_inactive_session_is_idle() {
        let action = wait_action(WaitFacts {
            pending_input: false,
            running: false,
            active: false,
        });
        assert_eq!(action, WaitAction::Idle);
        assert!(action.is_terminal());
    }

    #[test]
    fn an_open_quiet_session_checks_with_the_driver() {
        // `check-idle` is not terminal: upstream asks the driver and only then decides, so ending
        // here would report a stop nothing confirmed.
        let action = wait_action(WaitFacts {
            pending_input: false,
            running: false,
            active: true,
        });
        assert_eq!(action, WaitAction::CheckIdle);
        assert!(!action.is_terminal());
    }

    #[test]
    fn the_wire_spellings_are_upstreams() {
        assert_eq!(WaitAction::InputRequired.as_str(), "input-required");
        assert_eq!(WaitAction::Continue.as_str(), "continue");
        assert_eq!(WaitAction::Idle.as_str(), "idle");
        assert_eq!(WaitAction::CheckIdle.as_str(), "check-idle");
    }
}
