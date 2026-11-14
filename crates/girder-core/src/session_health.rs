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

/// Diagnoses one session log.
///
/// One pass, no I/O, no allocation beyond the returned issues: the caller holds the events already,
/// so this is cheap enough to run over every log a deployment can read.
///
/// The result is ordered by [`Issue::at`], so it reads in log order rather than in the order the
/// checks happened to fire.
#[must_use]
pub fn diagnose(events: &[SessionEvent]) -> Vec<Issue> {
    let mut issues: Vec<Issue> = Vec::new();
    let mut turn: Option<OpenTurn> = None;
    let mut step: Option<OpenStep> = None;
    let mut calls: BTreeMap<String, OpenCall> = BTreeMap::new();

    for (at, event) in events.iter().enumerate() {
        match event {
            SessionEvent::TurnStart { turn: number } => {
                // Taken rather than borrowed so the arms can open the next turn while inspecting the
                // previous one; every arm but the last puts it back.
                match turn.take() {
                    // The writer has produced two `turn/start` records for one boundary before (see
                    // `event_index`), so this is a hint rather than a fault: splitting the turn here
                    // would report one turn as two.
                    Some(open) if open.number == *number => {
                        issues.push(Issue {
                            at,
                            kind: Kind::OutOfOrder,
                            severity: Severity::Info,
                            detail: format!(
                                "`turn/start` for turn {number} repeats the turn already open at {}",
                                open.at
                            ),
                        });
                        turn = Some(open);
                    }
                    // A session cannot run two turns at once, so this is the first evidence that the
                    // previous turn never wrote its `turn/end`.
                    Some(open) => {
                        issues.push(Issue {
                            at: open.at,
                            kind: Kind::UnclosedTurn,
                            severity: Severity::Warning,
                            detail: format!(
                                "turn {} opened here never wrote `turn/end`; turn {number} started at \
                                 {at} instead",
                                open.number
                            ),
                        });
                        close_turn(&mut step, &mut calls, at, &mut issues);
                        turn = Some(OpenTurn {
                            number: *number,
                            at,
                        });
                    }
                    None => {
                        turn = Some(OpenTurn {
                            number: *number,
                            at,
                        })
                    }
                }
            }
            SessionEvent::TurnEnd { turn: number, .. } => match turn.take() {
                // The ordinary close. Whatever the turn left open can never be answered now, because
                // the results would be written before this record existed.
                Some(open) if open.number == *number => {
                    close_turn(&mut step, &mut calls, at, &mut issues);
                }
                // An end naming another turn is not evidence about the open one, so the open turn is
                // kept: closing it here would invent a boundary the log never recorded.
                Some(open) => {
                    issues.push(Issue {
                        at,
                        kind: Kind::OutOfOrder,
                        severity: Severity::Error,
                        detail: format!(
                            "`turn/end` for turn {number} while turn {} opened at {} is the open one; \
                             the end closes a turn this log never started",
                            open.number, open.at
                        ),
                    });
                    turn = Some(open);
                }
                None => issues.push(Issue {
                    at,
                    kind: Kind::OutOfOrder,
                    severity: Severity::Error,
                    detail: format!("`turn/end` for turn {number} with no `turn/start` before it"),
                }),
            },
            SessionEvent::StepStart {
                turn: number,
                step: index,
            } => {
                if let Some(open) = step.take() {
                    issues.push(Issue {
                        at: open.at,
                        kind: Kind::UnclosedStep,
                        severity: Severity::Warning,
                        detail: format!(
                            "step {}/{} opened here never wrote `step/end`; step {index} of turn \
                             {number} started at {at} instead",
                            open.turn, open.step
                        ),
                    });
                }
                // The step is recorded rather than refused: it can still be closed, and a log whose
                // turn boundary was trimmed is readable apart from this record.
                if turn.is_none() {
                    issues.push(Issue {
                        at,
                        kind: Kind::OutOfOrder,
                        severity: Severity::Warning,
                        detail: format!(
                            "`step/start` for turn {number} step {index} while no turn is open"
                        ),
                    });
                }
                step = Some(OpenStep {
                    turn: *number,
                    step: *index,
                    at,
                });
            }
            SessionEvent::StepEnd {
                turn: number,
                step: index,
            } => match step.take() {
                Some(open) if open.turn == *number && open.step == *index => {}
                Some(open) => {
                    issues.push(Issue {
                        at,
                        kind: Kind::OutOfOrder,
                        severity: Severity::Error,
                        detail: format!(
                            "`step/end` for turn {number} step {index} does not close step {}/{} \
                             opened at {}",
                            open.turn, open.step, open.at
                        ),
                    });
                    step = Some(open);
                }
                None => issues.push(Issue {
                    at,
                    kind: Kind::OutOfOrder,
                    severity: Severity::Error,
                    detail: format!(
                        "`step/end` for turn {number} step {index} with no matching `step/start`"
                    ),
                }),
            },
            SessionEvent::ToolCall { id, name, .. } => {
                let key = id.as_str().to_owned();
                match calls.get_mut(&key) {
                    Some(open) => {
                        open.unanswered = open.unanswered.saturating_add(1);
                        issues.push(Issue {
                            at,
                            kind: Kind::OutOfOrder,
                            severity: Severity::Warning,
                            detail: format!(
                                "tool call `{key}` ({name}) repeats the id of the unanswered call at \
                                 {}; one result cannot answer both",
                                open.at
                            ),
                        });
                    }
                    None => {
                        calls.insert(key, OpenCall { at, unanswered: 1 });
                    }
                }
            }
            // A result answers exactly one call, so a repeated id is counted down rather than
            // removed: two calls that share an id and one result leaves one of them unanswered, and
            // dropping the whole entry would hide it.
            SessionEvent::ToolResult { id, .. } => {
                let key = id.as_str();
                let settled = calls.get_mut(key).map(|open| {
                    open.unanswered = open.unanswered.saturating_sub(1);
                    open.unanswered == 0
                });
                match settled {
                    // `remove` after the borrow rather than inside the closure: the entry is gone
                    // only once nothing is left to answer.
                    Some(true) => {
                        calls.remove(key);
                    }
                    Some(false) => {}
                    // A result with no open call is the one shape nothing normal produces: the call
                    // is always written before the tool runs.
                    None => issues.push(Issue {
                        at,
                        kind: Kind::OrphanResult,
                        severity: Severity::Error,
                        detail: format!(
                            "tool result for `{key}` with no unanswered `tool/call` for that id \
                             before it"
                        ),
                    }),
                }
            }
            _ => {}
        }
    }

    // The tail. Nothing after this position can answer anything, so every open item is a hint: the
    // session may simply still be running.
    if let Some(open) = step.take() {
        issues.push(Issue {
            at: open.at,
            kind: Kind::UnclosedStep,
            severity: Severity::Info,
            detail: format!(
                "step {}/{} is still open at the end of the log; a live session between `step/start` \
                 and `step/end` looks exactly like this",
                open.turn, open.step
            ),
        });
    }
    if let Some(open) = turn.take() {
        issues.push(Issue {
            at: open.at,
            kind: Kind::UnclosedTurn,
            severity: Severity::Info,
            detail: format!(
                "turn {} is still open at the end of the log; it may be the turn running now",
                open.number
            ),
        });
    }
    for (id, open) in calls {
        issues.push(Issue {
            at: open.at,
            kind: Kind::UnansweredCall,
            severity: Severity::Info,
            detail: format!(
                "tool call `{id}` has no result yet{}; a session waiting on a tool looks like this",
                shared_id_note(open.unanswered)
            ),
        });
    }

    // Position order, stably: issues pushed while closing a turn point back at the record that was
    // left dangling, so pushing order is not log order.
    issues.sort_by_key(|issue| issue.at);
    issues
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

/// Closes the open turn and reports what it leaves dangling.
///
/// Both callers are cases where the turn is over — its own `turn/end`, or a later `turn/start` that
/// proves the end was never written — so everything still open inside it is a warning rather than a
/// hint: no result or step end can arrive for a turn that is finished.
///
/// `at` is the position of the record that ended the turn, used only in the detail so the reader can
/// see what the turn was overtaken by.
fn close_turn(
    step: &mut Option<OpenStep>,
    calls: &mut BTreeMap<String, OpenCall>,
    at: usize,
    issues: &mut Vec<Issue>,
) {
    if let Some(open) = step.take() {
        issues.push(Issue {
            at: open.at,
            kind: Kind::UnclosedStep,
            severity: Severity::Warning,
            detail: format!(
                "step {}/{} opened here never wrote `step/end` before the turn ended at {at}",
                open.turn, open.step
            ),
        });
    }

    // `take` so the caller's map is left empty: these calls belong to the turn being closed, and
    // carrying them into the next turn would attribute an old failure to a new one.
    for (id, open) in std::mem::take(calls) {
        issues.push(Issue {
            at: open.at,
            kind: Kind::UnansweredCall,
            severity: Severity::Warning,
            detail: format!(
                "tool call `{id}` has no result and its turn ended at {at}{}; a result written after \
                 this would be an orphan",
                shared_id_note(open.unanswered)
            ),
        });
    }
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
