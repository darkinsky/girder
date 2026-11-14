//! Diagnosing whether one session log is coherent as a conversation.
//!
//! # Why this is not already answered
//!
//! Everything the crate knows about a log's condition today is about *something else*:
//!
//! - `session_recovery` answers a question about **bytes**. It walks zstd frames, finds the point at
//!   which the file stops decoding, and decides whether the tail can be cut away. A log that decodes
//!   perfectly but has a tool result with no call to answer is a `Recovery::Healthy` verdict there,
//!   because the bytes are fine and the *conversation* is not.
//! - `session_stats` answers **how much** a conversation did. It counts `unanswered_tool_calls`, but
//!   a count is not a verdict: one unanswered call is an ordinary session waiting on a slow tool,
//!   and the same count on a finished session is a lost result. Nothing in the count says which.
//! - `compaction::pairing_balanced_at` answers whether a *cut point* is legal for compaction. It is
//!   a predicate over one boundary, not a diagnosis of a whole log.
//!
//! So the question "is this session broken, and how badly" had no answer inside the crate. The answer
//! an operator was left with was the one below.
//!
//! # Why finding every broken session was a manual job
//!
//! Before this module a deployment that wanted to find its damaged conversations had to decode every
//! log, run `SessionStats::of` on each, and then eyeball `unanswered_tool_calls == 0` — which is not a
//! test for damage at all, since it is normal for the session being watched right now. It also cannot
//! see the failures that matter most: an orphan result leaves the counter at zero, an unclosed step
//! leaves no trace, and a `turn/end` before its `turn/start` is invisible to a fold that only counts.
//! There was no per-session verdict, no severity, and nothing to sort by. [`diagnose`], [`is_healthy`],
//! and [`score`] are that missing per-session verdict, so a sweep is a loop over logs plus a filter
//! instead of a manual reading of each one.
//!
//! # Why the tail is judged differently from the middle
//!
//! The single idea this module is built on: **an incomplete tail is normal, an incomplete middle is
//! not.** A session that is running right now ends in the middle of a turn, with a step open and a
//! tool call that has not been answered yet — that is what a healthy live session looks like, and
//! flagging it would make every active conversation look damaged. But the same shape *before* later
//! records is damage: once a `turn/end` has been written, no result can arrive for a call that the
//! turn left unanswered, and once a new `turn/start` has been written, the previous turn can never be
//! closed. So the same structural fact is a hint at the end of the log and a warning before it.
//!
//! This is also why [`Severity::Info`] never lowers [`score`] and never makes [`is_healthy`] false.
//! "This session is waiting for a tool" and "this session died waiting for a tool" are the same
//! evidence; a score that punished the first would rank every live session as the worst one.
//!
//! # What counts as an error
//!
//! A tool result that no call asked for. It cannot be produced by a slow tool, a cancelled turn, or a
//! process that died: the call is always written before the tool runs. It means the log lost the call
//! (or two logs were spliced), and a replay that respected the result would show the model answering
//! something it never asked. That is a conversation a reader cannot trust, so it is [`Severity::Error`].

use std::collections::BTreeMap;

// Named through `::girder_core` rather than `crate::` so this file compiles both as a module of the
// library and, by `tests/session_health.rs`, as a module of the test crate; `extern crate self as
// girder_core` in the crate root is what makes the path resolve from inside the library too.
use ::girder_core::SessionEvent;

/// Points one [`Severity::Warning`] takes off [`score`].
///
/// Public because a caller that sorts sessions needs to know how the number was reached, and because
/// changing the weight is a behaviour change that should be a visible edit rather than a magic number.
pub const WARNING_PENALTY: u8 = 5;

/// Points one [`Severity::Error`] takes off [`score`].
///
/// Five times a warning: one error is a conversation a reader cannot trust, while one warning is a
/// structural oddity that a replay can still survive.
pub const ERROR_PENALTY: u8 = 25;

/// What a log with nothing wrong scores.
const FULL_SCORE: u8 = 100;

/// A health problem found in a session log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// Index of the event the issue is about.
    ///
    /// The event that identifies the problem, not the record that revealed it: an unanswered call is
    /// reported at the `tool/call` that was never answered, even though the reason it is a warning is
    /// a later `turn/end`. A reader following the index lands on the thing to look at.
    pub at: usize,

    /// What kind of problem it is.
    pub kind: Kind,

    /// How much it matters.
    pub severity: Severity,

    /// What was seen, in the form an operator can act on.
    ///
    /// Carries the ids, turn numbers, and positions involved rather than only a category, because the
    /// caller of [`diagnose`] has the category in [`Issue::kind`] already and needs the specifics.
    pub detail: String,
}

/// The kinds of problem [`diagnose`] can find.
///
/// Deliberately a closed list of structural facts, not a taxonomy of causes: this module reports what
/// the log contradicts, and deciding why (a crash, a cancelled turn, a spliced file) is the reader's
/// question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub enum Kind {
    /// A tool call with no result.
    ///
    /// Usually normal. See the module docs: at the end of the log it is a hint, and before a closed
    /// turn it is a warning.
    UnansweredCall,

    /// A tool result with no call.
    ///
    /// Always [`Severity::Error`]. A result cannot precede the call that asked for it, so this is
    /// evidence the log lost a record rather than evidence of a tool that was slow.
    OrphanResult,

    /// A turn that never closed.
    UnclosedTurn,

    /// A step that never closed.
    UnclosedStep,

    /// An event whose position contradicts another event's, e.g. `turn/end` before `turn/start`.
    OutOfOrder,
}

impl Kind {
    /// The kind's name, for log lines and metric labels.
    ///
    /// A stable spelling rather than a `Debug` rendering: `Debug` output is a developer convenience
    /// that may be reformatted at any time, and these strings end up in dashboards.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnansweredCall => "unanswered_call",
            Self::OrphanResult => "orphan_result",
            Self::UnclosedTurn => "unclosed_turn",
            Self::UnclosedStep => "unclosed_step",
            Self::OutOfOrder => "out_of_order",
        }
    }
}

/// How much an [`Issue`] matters.
///
/// Ordered, so the worst issue in a list is its maximum: `Info < Warning < Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub enum Severity {
    /// A hint that can be ignored.
    ///
    /// The normal shape of a session that is still running, and the reason [`is_healthy`] exists
    /// instead of "the issue list is empty".
    Info,

    /// Worth looking at.
    ///
    /// The log contradicts itself in a way a replay can survive: a turn that ended with a call still
    /// open, a step a later step overtook.
    Warning,

    /// The session cannot be trusted as a conversation.
    Error,
}

impl Severity {
    /// The severity's name, for log lines and metric labels.
    ///
    /// Stable for the same reason as [`Kind::as_str`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// Points this severity takes off [`score`].
    ///
    /// Zero for [`Severity::Info`] rather than a small number: a live session waiting on a tool is
    /// indistinguishable here from one that died waiting, and a score that charged for the first
    /// would sort every active conversation below every idle one.
    #[must_use]
    pub fn weight(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Warning => WARNING_PENALTY,
            Self::Error => ERROR_PENALTY,
        }
    }
}

/// A turn opened by `turn/start` and not yet closed.
struct OpenTurn {
    /// The number the log carried, used to match the close against the open.
    number: u64,

    /// Where it opened, so an issue about it points at the `turn/start`.
    at: usize,
}

/// A step opened by `step/start` and not yet closed.
struct OpenStep {
    /// Turn the step belongs to, as the log recorded it.
    turn: u64,

    /// Step number within the turn.
    step: u64,

    /// Where it opened.
    at: usize,
}

/// A tool call with no result yet.
struct OpenCall {
    /// Where the call was recorded.
    at: usize,

    /// How many unanswered calls share this id.
    ///
    /// Normally 1. Counted rather than overwritten because a duplicated id is worth reporting once
    /// with the number attached, and a plain map insert would silently drop the earlier call site.
    unanswered: u32,
}

/// The state of a log walk: what is open at the current position.
///
/// A struct rather than four locals in [`diagnose`] because the three record handlers all need to
/// close things, and passing four mutable references to a free function was less readable than the
/// state it was mutating. The fields are private: an `Issue` is the only thing a caller sees.
#[derive(Default)]
struct Walker {
    /// The turn being accumulated, if one is open.
    turn: Option<OpenTurn>,

    /// The step being accumulated, if one is open.
    step: Option<OpenStep>,

    /// Tool calls with no result yet, keyed by id.
    ///
    /// Ordered rather than hashed so the report is deterministic: two runs over one log must produce
    /// the same list, or a test that compares issues would flake and a diff between two sweeps would
    /// show churn that is not there.
    calls: BTreeMap<String, OpenCall>,

    /// What has been found so far.
    issues: Vec<Issue>,
}

impl Walker {
    /// The record at `at` opened a turn.
    fn turn_started(&mut self, at: usize, number: u64) {
        match self.turn.take() {
            // The writer has produced two `turn/start` records for one boundary before (see
            // `event_index`), so this is a hint rather than a fault: splitting the turn here would
            // report one turn as two.
            Some(open) if open.number == number => {
                self.issue(
                    at,
                    Kind::OutOfOrder,
                    Severity::Info,
                    format!(
                        "`turn/start` for turn {number} repeats the turn already open at {}",
                        open.at
                    ),
                );
                self.turn = Some(open);
            }
            // A session cannot run two turns at once, so this is the first evidence that the previous
            // turn never wrote its `turn/end`.
            Some(open) => {
                self.issue(
                    open.at,
                    Kind::UnclosedTurn,
                    Severity::Warning,
                    format!(
                        "turn {} opened here never wrote `turn/end`; turn {number} started at {at} \
                         instead",
                        open.number
                    ),
                );
                self.close_turn(at);
                self.turn = Some(OpenTurn { number, at });
            }
            None => self.turn = Some(OpenTurn { number, at }),
        }
    }

    /// The record at `at` closed a turn.
    fn turn_ended(&mut self, at: usize, number: u64) {
        match self.turn.take() {
            // The ordinary close. Whatever the turn left open can never be answered now, because the
            // results would have been written before this record existed.
            Some(open) if open.number == number => self.close_turn(at),
            // An end naming another turn is not evidence about the open one, so the open turn is kept:
            // closing it here would invent a boundary the log never recorded.
            Some(open) => {
                self.issue(
                    at,
                    Kind::OutOfOrder,
                    Severity::Error,
                    format!(
                        "`turn/end` for turn {number} while turn {} opened at {} is the open one; the \
                         end closes a turn this log never started",
                        open.number, open.at
                    ),
                );
                self.turn = Some(open);
            }
            None => self.issue(
                at,
                Kind::OutOfOrder,
                Severity::Error,
                format!("`turn/end` for turn {number} with no `turn/start` before it"),
            ),
        }
    }

    /// The record at `at` opened a step.
    fn step_started(&mut self, at: usize, number: u64, index: u64) {
        if let Some(open) = self.step.take() {
            self.issue(
                open.at,
                Kind::UnclosedStep,
                Severity::Warning,
                format!(
                    "step {}/{} opened here never wrote `step/end`; step {index} of turn {number} \
                     started at {at} instead",
                    open.turn, open.step
                ),
            );
        }
        // A step outside any turn is unusual but readable, so it is recorded rather than refused: it
        // can still be closed, and treating it as a fault would turn a trimmed log into an unusable
        // one.
        if self.turn.is_none() {
            self.issue(
                at,
                Kind::OutOfOrder,
                Severity::Warning,
                format!("`step/start` for turn {number} step {index} while no turn is open"),
            );
        }
        self.step = Some(OpenStep {
            turn: number,
            step: index,
            at,
        });
    }

    /// The record at `at` closed a step.
    fn step_ended(&mut self, at: usize, number: u64, index: u64) {
        match self.step.take() {
            Some(open) if open.turn == number && open.step == index => {}
            Some(open) => {
                self.issue(
                    at,
                    Kind::OutOfOrder,
                    Severity::Error,
                    format!(
                        "`step/end` for turn {number} step {index} does not close step {}/{} opened \
                         at {}",
                        open.turn, open.step, open.at
                    ),
                );
                self.step = Some(open);
            }
            None => self.issue(
                at,
                Kind::OutOfOrder,
                Severity::Error,
                format!("`step/end` for turn {number} step {index} with no matching `step/start`"),
            ),
        }
    }

    /// The record at `at` asked for a tool.
    fn called(&mut self, at: usize, id: &str, name: &str) {
        // Resolved before the issue is pushed rather than inside the match: pushing needs the whole
        // walker, which cannot overlap a borrow of the entry that describes the duplicate.
        let first = self.calls.get_mut(id).map(|open| {
            open.unanswered = open.unanswered.saturating_add(1);
            open.at
        });
        match first {
            Some(first) => self.issue(
                at,
                Kind::OutOfOrder,
                Severity::Warning,
                format!(
                    "tool call `{id}` ({name}) repeats the id of the unanswered call at {first}; one \
                     result cannot answer both"
                ),
            ),
            None => {
                self.calls.insert(id.to_owned(), OpenCall { at, unanswered: 1 });
            }
        }
    }

    /// The record at `at` answered a tool.
    fn answered(&mut self, at: usize, id: &str) {
        let settled = self.calls.get_mut(id).map(|open| {
            open.unanswered = open.unanswered.saturating_sub(1);
            open.unanswered == 0
        });
        match settled {
            // Removed only once nothing is left to answer: a repeated id is two calls, and one result
            // satisfies one of them.
            Some(true) => {
                self.calls.remove(id);
            }
            Some(false) => {}
            // A result with no open call is the one shape nothing normal produces: the call is always
            // written before the tool runs.
            None => self.issue(
                at,
                Kind::OrphanResult,
                Severity::Error,
                format!(
                    "tool result for `{id}` with no unanswered `tool/call` for that id before it"
                ),
            ),
        }
    }

    /// Closes whatever the finished turn left open and reports it.
    ///
    /// Both callers are cases where the turn is over -- its own `turn/end`, or a later `turn/start`
    /// that proves the end was never written -- so everything still open inside it is a warning
    /// rather than a hint: no result or step end can arrive for a turn that is finished.
    fn close_turn(&mut self, at: usize) {
        if let Some(open) = self.step.take() {
            self.issue(
                open.at,
                Kind::UnclosedStep,
                Severity::Warning,
                format!(
                    "step {}/{} opened here never wrote `step/end` before the turn ended at {at}",
                    open.turn, open.step
                ),
            );
        }
        // Taken so the calls of a closed turn are not carried into the next one, which would attribute
        // an old failure to a new turn.
        for (id, open) in std::mem::take(&mut self.calls) {
            self.issue(
                open.at,
                Kind::UnansweredCall,
                Severity::Warning,
                format!(
                    "tool call `{id}` has no result and its turn ended at {at}{}; a result written \
                     after this would be an orphan",
                    shared_id_note(open.unanswered)
                ),
            );
        }
    }

    /// Reports what is still open at the end of the log.
    ///
    /// Nothing after this position can answer anything, so every item here is a hint: the session may
    /// simply still be running.
    fn finish(mut self) -> Vec<Issue> {
        if let Some(open) = self.step.take() {
            self.issue(
                open.at,
                Kind::UnclosedStep,
                Severity::Info,
                format!(
                    "step {}/{} is still open at the end of the log; a live session between \
                     `step/start` and `step/end` looks exactly like this",
                    open.turn, open.step
                ),
            );
        }
        if let Some(open) = self.turn.take() {
            self.issue(
                open.at,
                Kind::UnclosedTurn,
                Severity::Info,
                format!(
                    "turn {} is still open at the end of the log; it may be the turn running now",
                    open.number
                ),
            );
        }
        for (id, open) in std::mem::take(&mut self.calls) {
            self.issue(
                open.at,
                Kind::UnansweredCall,
                Severity::Info,
                format!(
                    "tool call `{id}` has no result yet{}; a session waiting on a tool looks like \
                     this",
                    shared_id_note(open.unanswered)
                ),
            );
        }

        // Position order, stably: issues pushed while closing a turn point back at the record that was
        // left dangling, so pushing order is not log order.
        self.issues.sort_by_key(|issue| issue.at);
        self.issues
    }

    /// Pushes one issue.
    fn issue(&mut self, at: usize, kind: Kind, severity: Severity, detail: String) {
        self.issues.push(Issue {
            at,
            kind,
            severity,
            detail,
        });
    }
}

/// Diagnoses one session log.
///
/// One pass, no I/O, and no allocation beyond the returned issues: the caller holds the events already,
/// so this is cheap enough to run over every log a deployment can read.
///
/// The result is ordered by [`Issue::at`], so it reads in log order rather than in the order the checks
/// happened to fire.
#[must_use]
pub fn diagnose(events: &[SessionEvent]) -> Vec<Issue> {
    let mut walker = Walker::default();

    for (at, event) in events.iter().enumerate() {
        match event {
            SessionEvent::TurnStart { turn } => walker.turn_started(at, *turn),
            SessionEvent::TurnEnd { turn, .. } => walker.turn_ended(at, *turn),
            SessionEvent::StepStart { turn, step } => walker.step_started(at, *turn, *step),
            SessionEvent::StepEnd { turn, step } => walker.step_ended(at, *turn, *step),
            SessionEvent::ToolCall { id, name, .. } => walker.called(at, id.as_str(), name),
            SessionEvent::ToolResult { id, .. } => walker.answered(at, id.as_str()),
            // Every other variant -- messages, accounting, the selection records -- is content rather
            // than structure, so it cannot contradict the boundaries and is not the subject here.
            _ => {}
        }
    }

    walker.finish()
}

/// Reports whether a log is healthy.
///
/// Healthy means no issue above [`Severity::Info`]: a session waiting on a tool, or a turn still
/// running, is healthy even though [`diagnose`] returns issues for it. "No issues at all" is the
/// wrong question — it would call every live session unhealthy.
///
/// Implemented over [`diagnose`] rather than as a second scan so the two answers cannot disagree.
#[must_use]
pub fn is_healthy(events: &[SessionEvent]) -> bool {
    diagnose(events)
        .iter()
        .all(|issue| issue.severity == Severity::Info)
}

/// Rates a log from 0 to 100.
///
/// [`FULL_SCORE`] minus each issue's [`Severity::weight`], floored at zero with saturating
/// arithmetic — twenty errors and four errors both score 0, because the difference between them is
/// not something a ranking of broken sessions needs to preserve.
///
/// The number exists so a deployment can *sort*: a sweep over every log produces a list whose head is
/// the worst session, instead of a list that has to be read.
#[must_use]
pub fn score(events: &[SessionEvent]) -> u8 {
    diagnose(events).iter().fold(FULL_SCORE, |total, issue| {
        total.saturating_sub(issue.severity.weight())
    })
}

/// A parenthetical for a call id that several unanswered calls share, or nothing when only one does.
///
/// A free function rather than a method because [`OpenCall`] is private and its count is an
/// implementation detail; the sentence belongs where it is read.
fn shared_id_note(unanswered: u32) -> String {
    if unanswered > 1 {
        format!(" ({unanswered} unanswered calls share this id)")
    } else {
        String::new()
    }
}
