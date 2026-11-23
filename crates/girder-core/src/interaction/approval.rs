//! Approval requests: asking a person to confirm one action.
//!
//! A permission preset decides *whether* to ask. This is the asking: a request names what is about to
//! happen, a person answers, and the answer is recorded so a replay can explain why the action ran.
//!
//! # Why the request carries the action rather than a pointer to it
//!
//! A person deciding must see what they are approving. A request that referenced a pending action by id
//! would force the UI to fetch it, and a UI that showed a stale description would be asking the person to
//! approve something else.
//!
//! # Why the answer is durable
//!
//! An approval that existed only in memory would leave a replay unable to explain why a write happened. The
//! answer is therefore a value with who decided and when, and it is written beside the action rather than
//! instead of it.
//!
//! # Why a denial is not an error
//!
//! A person saying no is a normal outcome, not a failure. Modelling it as an error would make every caller
//! handle a rejection through its error path, and the difference between "the user declined" and "the
//! approval system broke" would be lost.

use std::time::Duration;

/// What a person is being asked about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The tool or command that wants to run.
    pub action: String,
    /// What it would do, in one line.
    pub description: String,
    /// The exact detail — a command line, a path — for a person to judge.
    ///
    /// Separate from `description` because the two serve different readers: a description is what a person
    /// skims, and the detail is what they check when the description is not enough.
    pub detail: Option<String>,
    /// How long the asker will wait.
    pub timeout: Duration,
}

/// The default wait for an answer.
///
/// Five minutes: long enough for a person to notice and read, short enough that an unattended session does
/// not hold a tool call open indefinitely.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

impl Request {
    /// Builds a request.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank action or description: a person cannot approve what they cannot read.
    #[must_use]
    pub fn new(action: impl Into<String>, description: impl Into<String>) -> Option<Self> {
        let action = action.into();
        let description = description.into();
        if action.trim().is_empty() || description.trim().is_empty() {
            return None;
        }
        Some(Self {
            action,
            description,
            detail: None,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Attaches the exact detail.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Bounds the wait.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Who answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecidedBy {
    /// A person, interactively.
    Human,
    /// A policy that auto-approved without asking.
    ///
    /// Recorded rather than left as `Human`: a replay that showed every approval as a person's decision
    /// would misrepresent what happened.
    Policy,
    /// The wait expired with no answer.
    ///
    /// A decision, not an absence: the action did not run, and a log needs to say why.
    TimedOut,
}

impl DecidedBy {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Policy => "policy",
            Self::TimedOut => "timed-out",
        }
    }
}

/// What a person answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run it.
    Approved {
        /// Who decided.
        by: DecidedBy,
        /// An optional note, when a person added one.
        note: Option<String>,
    },
    /// Do not run it.
    Denied {
        /// Who decided.
        by: DecidedBy,
        /// Why, when the decider said.
        reason: Option<String>,
    },
}

impl Decision {
    /// A human approval.
    #[must_use]
    pub fn approved() -> Self {
        Self::Approved {
            by: DecidedBy::Human,
            note: None,
        }
    }

    /// A human denial.
    #[must_use]
    pub fn denied() -> Self {
        Self::Denied {
            by: DecidedBy::Human,
            reason: None,
        }
    }

    /// A denial with a reason.
    #[must_use]
    pub fn denied_because(reason: impl Into<String>) -> Self {
        Self::Denied {
            by: DecidedBy::Human,
            reason: Some(reason.into()),
        }
    }

    /// A policy approval that did not ask.
    #[must_use]
    pub fn auto_approved() -> Self {
        Self::Approved {
            by: DecidedBy::Policy,
            note: None,
        }
    }

    /// Whether the action may run.
    ///
    /// The single question a caller asks. Everything else about a decision is for the log.
    #[must_use]
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved { .. })
    }

    /// Who decided.
    #[must_use]
    pub fn decided_by(&self) -> DecidedBy {
        match self {
            Self::Approved { by, .. } | Self::Denied { by, .. } => *by,
        }
    }
}

/// Why an approval could not be obtained.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApprovalError {
    /// Nobody is available to ask.
    ///
    /// Distinct from a denial: a denial is a decision, and this is the absence of one because the
    /// deployment has no interactive surface.
    #[error("no approver is available in this deployment")]
    NoApprover,

    /// The request itself was unusable.
    #[error("the approval request is incomplete: {reason}")]
    Invalid {
        /// Why.
        reason: String,
    },
}

/// The seam a deployment implements to ask a person.
///
/// Async and returning a [`Decision`] rather than a `Result`: a denial is an answer, not a failure. Only a
/// deployment that cannot ask at all produces an error.
pub trait Approver: Send + Sync {
    /// Asks, and waits no longer than the request's timeout.
    fn ask(&self, request: Request) -> crate::core::scope::BoxFut<Result<Decision, ApprovalError>>;
}

/// An approver that answers from a fixed script, for tests and for a non-interactive run.
///
/// A deployment with no UI still has to decide, and the honest options are "approve everything" and "deny
/// everything". This makes that choice explicit at construction rather than implicit in a default.
pub struct Scripted {
    decision: Decision,
}

impl Scripted {
    /// An approver that approves everything.
    #[must_use]
    pub fn approving() -> Self {
        Self {
            decision: Decision::auto_approved(),
        }
    }

    /// An approver that denies everything, with a reason.
    #[must_use]
    pub fn denying(reason: impl Into<String>) -> Self {
        Self {
            decision: Decision::Denied {
                by: DecidedBy::Policy,
                reason: Some(reason.into()),
            },
        }
    }
}

impl Approver for Scripted {
    fn ask(
        &self,
        _request: Request,
    ) -> crate::core::scope::BoxFut<Result<Decision, ApprovalError>> {
        let decision = self.decision.clone();
        Box::pin(async move { Ok(decision) })
    }
}

/// An approver that refuses to ask.
///
/// The default for a deployment with no interactive surface: a caller that forgot to mount an approver gets
/// a named error rather than a silent approval.
#[derive(Debug, Default)]
pub struct Unavailable;

impl Approver for Unavailable {
    fn ask(
        &self,
        _request: Request,
    ) -> crate::core::scope::BoxFut<Result<Decision, ApprovalError>> {
        Box::pin(async { Err(ApprovalError::NoApprover) })
    }
}

/// An approver that asks the person at the terminal.
///
/// A `workspace-write` deployment's rule is that a command or a file change waits for a person. This
/// is that wait: the question goes to stderr, the answer comes from stdin, and anything other than
/// an explicit yes is a denial. A pipe is not a person, so a non-terminal stdin denies rather than
/// blocking a batch job on a read that will never be answered.
#[derive(Debug, Default)]
pub struct StdinApprover;

impl StdinApprover {
    /// Builds the terminal approver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Approver for StdinApprover {
    fn ask(&self, request: Request) -> crate::core::scope::BoxFut<Result<Decision, ApprovalError>> {
        Box::pin(async move {
            let timeout = request.timeout;
            let answered = tokio::task::spawn_blocking(move || answer_on_stdin(&request));
            match tokio::time::timeout(timeout, answered).await {
                Ok(Ok(decision)) => Ok(decision),
                Ok(Err(_)) => Ok(Decision::denied_because(
                    "the approval prompt could not be read",
                )),
                Err(_) => Ok(Decision::Denied {
                    by: DecidedBy::TimedOut,
                    reason: Some("the approval prompt timed out".to_owned()),
                }),
            }
        })
    }
}

/// Reads one yes/no answer. `y` and `yes` approve; every other line, including a closed stdin, denies.
fn answer_on_stdin(request: &Request) -> Decision {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Decision::denied_because("stdin is not a terminal, so nobody can approve this");
    }
    eprintln!();
    eprintln!("[approval] {}", request.action);
    eprintln!("  {}", request.description);
    if let Some(detail) = &request.detail {
        eprintln!("  {detail}");
    }
    eprint!("allow this? [y/N] ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => Decision::denied_because("stdin closed before an answer"),
        Ok(_) if is_yes(line.trim()) => Decision::approved(),
        Ok(_) => Decision::denied(),
        Err(error) => Decision::denied_because(format!("could not read the answer: {error}")),
    }
}

/// Whether a typed answer is consent.
fn is_yes(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

#[cfg(test)]
mod tests {
    use super::is_yes;

    #[test]
    fn only_an_explicit_yes_is_consent() {
        assert!(is_yes("y"));
        assert!(is_yes("Y"));
        assert!(is_yes("yes"));
        assert!(is_yes("YES"));
        assert!(!is_yes(""));
        assert!(!is_yes("n"));
        assert!(!is_yes("no"));
        assert!(!is_yes("yep"));
    }
}

/// One recorded decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// What was asked.
    pub request: Request,
    /// What was decided.
    pub decision: Decision,
}

impl Record {
    /// Whether the action ran.
    #[must_use]
    pub fn is_approved(&self) -> bool {
        self.decision.is_approved()
    }
}
