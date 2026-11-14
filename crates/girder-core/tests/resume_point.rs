//! Tests for choosing the position a crashed session continues from.
//!
//! The properties pinned here are the ones a plausible implementation gets wrong:
//!
//! - **the last boundary wins, not the first** — a log with three finished turns must resume after
//!   the third. Resuming after the first would re-run tools the user already saw run, which is the
//!   most expensive mistake this module can make and the least visible in a small fixture.
//! - **a turn that died mid-tool-call is discarded whole** — the core crash case. Whether the turn is
//!   redone from the previous boundary or from its own `turn/start`, the resumed prefix must not carry
//!   a `tool/call` without its `tool/result`: no provider accepts that history.
//! - **a log with no boundary is not automatically "start over"** — an open turn whose calls are all
//!   paired resumes at its end, which keeps a prompt that was durable before the model answered.
//! - **`is_safe` checks instead of assuming** — a `TurnBoundary` point over a prefix with an
//!   unanswered call is unsafe. A verification that could not say that would be a restatement of the
//!   reason rather than a check of it.
//! - **`at` is the first event to process** — the tests assert on the event at that index, never on
//!   `at - 1`, because the off-by-one is what makes a resume replay or skip exactly one record.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/resume_point.rs"]
mod resume_point;

use girder_core::{SessionEvent, ToolCallId, TurnEndReason};

use resume_point::{Point, Reason, find, is_safe};

// ---- fixtures ----------------------------------------------------------------------------------

/// A tool call the model asked for.
fn call(id: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(id).expect("valid call id"),
        name: "read".to_owned(),
        arguments: "{}".to_owned(),
    }
}

/// The result a tool produced for `id`.
fn result(id: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(id).expect("valid call id"),
        content: Vec::new(),
        is_error: false,
    }
}

/// One complete step: start, model reply, call, result, end.
fn push_step(events: &mut Vec<SessionEvent>, turn: u64, step: u64, id: &str) {
    events.push(SessionEvent::StepStart { turn, step });
    events.push(SessionEvent::assistant_text(
        &format!("a{turn}-{step}"),
        "working",
        "test",
    ));
    events.push(call(id));
    events.push(result(id));
    events.push(SessionEvent::StepEnd { turn, step });
}

/// One complete turn with one tool-using step.
fn push_turn(events: &mut Vec<SessionEvent>, turn: u64) {
    events.push(SessionEvent::TurnStart { turn });
    events.push(SessionEvent::user_text(&format!("u{turn}"), "go"));
    push_step(events, turn, 1, &format!("call-{turn}"));
    events.push(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    });
}

/// One open turn that has died inside a tool call: start, prompt, step, reply, call, nothing more.
fn push_crashed_turn(events: &mut Vec<SessionEvent>, turn: u64) {
    events.push(SessionEvent::TurnStart { turn });
    events.push(SessionEvent::user_text(&format!("u{turn}"), "go"));
    events.push(SessionEvent::StepStart { turn, step: 1 });
    events.push(SessionEvent::assistant_text(
        &format!("a{turn}"),
        "reading",
        "test",
    ));
    events.push(call(&format!("call-{turn}")));
}

/// A log of `turns` complete turns, eight events each.
fn log(turns: u64) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    for turn in 1..=turns {
        push_turn(&mut events, turn);
    }
    events
}

/// Asserts that the point resumes after the whole log and names a boundary.
fn assert_boundary_at_end(point: Point, events: &[SessionEvent]) {
    assert_eq!(point.at, events.len(), "a finished log resumes at its end");
    assert_eq!(point.reason, Reason::TurnBoundary);
    assert!(is_safe(events, &point), "a boundary is a safe position");
}

// ---- the happy path ----------------------------------------------------------------------------

#[test]
fn a_single_closed_turn_resumes_after_its_end() {
    let events = log(1);

    let point = find(&events);

    assert_boundary_at_end(point, &events);
    assert_eq!(point.turn, 1, "the turn that closed is the one reported");
    // `at` is the first event to process, so it is one past the `turn/end`, not the `turn/end`.
    assert!(matches!(
        events.get(point.at - 1),
        Some(SessionEvent::TurnEnd { turn: 1, .. })
    ));
}

#[test]
fn the_last_boundary_wins() {
    // The core case: three turns finished, and resuming after the first or second would re-run work
    // the conversation already contains.
    let events = log(3);

    let point = find(&events);

    assert_boundary_at_end(point, &events);
    assert_eq!(point.turn, 3, "the newest closed turn is the one kept");
    assert_eq!(
        point.at,
        events.len(),
        "everything after turn 3 was already processed"
    );
}

#[test]
fn a_boundary_before_an_open_turn_resumes_at_that_turns_start() {
    // Turn 2 opened and never closed, after turn 1 closed. The answer is the boundary after turn 1 —
    // and that position is exactly turn 2's `turn/start`, which is what makes the point mean "redo
    // turn 2" without the caller having to find the start itself.
    let mut events = log(1);
    events.push(SessionEvent::TurnStart { turn: 2 });
    events.push(SessionEvent::user_text("u2", "go"));

    let point = find(&events);

    assert_eq!(point.reason, Reason::TurnBoundary);
    assert_eq!(point.turn, 1);
    assert_eq!(point.at, 8, "one past turn 1's `turn/end` at index 7");
    assert!(matches!(
        events.get(point.at),
        Some(SessionEvent::TurnStart { turn: 2 })
    ));
    assert!(is_safe(&events, &point));
}

// ---- the crash cases ---------------------------------------------------------------------------

#[test]
fn a_turn_that_died_after_a_tool_call_is_replayed_from_its_start() {
    // The core crash case, and the reason this module exists: the process died between `tool/call`
    // and `tool/result`, inside the *only* turn in the log. There is no boundary to fall back on, so
    // the whole turn is redone from its `turn/start` and the half-written call is never carried
    // forward — neither invented as a result nor handed to a provider that would reject it.
    let mut events = Vec::new();
    push_crashed_turn(&mut events, 1);

    let point = find(&events);

    assert_eq!(point.reason, Reason::MidTurn { turn: 1 });
    assert_eq!(point.turn, 1);
    assert_eq!(point.at, 0, "the open turn started at the first event");
    assert!(matches!(
        events.get(point.at),
        Some(SessionEvent::TurnStart { turn: 1 })
    ));
    assert!(is_safe(&events, &point), "an empty prefix is always safe");
}

#[test]
fn a_crash_in_a_later_turn_keeps_the_turns_before_it() {
    // The same crash one turn later: turn 1 closed, turn 2 died inside a call. The last boundary is
    // the answer and it is exactly turn 2's start, so the closed turn is kept and the broken one is
    // redone. The tail is *not* resumable, which is the fact this test states with `is_safe`.
    let mut events = log(1);
    push_crashed_turn(&mut events, 2);

    let point = find(&events);

    assert_eq!(point.reason, Reason::TurnBoundary);
    assert_eq!(point.turn, 1);
    assert_eq!(point.at, 8);
    assert!(matches!(
        events.get(point.at),
        Some(SessionEvent::TurnStart { turn: 2 })
    ));
    assert!(is_safe(&events, &point));
    // Resuming at the end of the log instead would carry the unanswered call into the model's
    // history, which is the mistake the boundary exists to prevent.
    assert!(!is_safe(
        &events,
        &Point {
            at: events.len(),
            turn: 2,
            reason: Reason::MidTurn { turn: 2 },
        }
    ));
}

#[test]
fn a_crash_between_two_steps_still_discards_the_whole_turn() {
    // The subtle variant: step 1 finished, so the log looks almost complete, and step 2 died inside a
    // call. The cut still lands on the turn, not on the last complete step — a step boundary is not a
    // transaction boundary, and the turn's remaining work is unknown.
    let mut events = log(1);
    events.push(SessionEvent::TurnStart { turn: 2 });
    events.push(SessionEvent::user_text("u2", "go"));
    push_step(&mut events, 2, 1, "call-2a");
    events.push(SessionEvent::StepStart { turn: 2, step: 2 });
    events.push(call("call-2b"));

    let point = find(&events);

    assert_eq!(point.reason, Reason::TurnBoundary);
    assert_eq!(
        point.at, 8,
        "turn 2 starts after turn 1, so its answered step goes with it"
    );
    assert!(is_safe(&events, &point));
}

#[test]
fn an_open_turn_whose_calls_are_all_answered_resumes_at_the_end() {
    // The turn never closed, but everything in it is complete: a prompt that was durable before the
    // model answered, a call, its result. There is nothing to redo and no boundary to fall back on,
    // so the log is kept whole — which is what stops a restart from asking the user to repeat
    // themselves.
    let mut events = vec![SessionEvent::TurnStart { turn: 1 }];
    events.push(SessionEvent::user_text("u1", "go"));
    push_step(&mut events, 1, 1, "call-1");

    let point = find(&events);

    assert_eq!(point.reason, Reason::CleanCallPair);
    assert_eq!(point.turn, 1, "the open turn is the one named");
    assert_eq!(point.at, events.len(), "nothing is left to replay");
    assert!(is_safe(&events, &point));
}

#[test]
fn a_clean_open_turn_reports_closed_pairs_rather_than_a_boundary() {
    // The two reasons must not be confused: a caller that saw `TurnBoundary` would report a finished
    // conversation, while this log's turn is still open and `session_health` says so too.
    let mut events = vec![SessionEvent::TurnStart { turn: 1 }];
    events.push(SessionEvent::user_text("u1", "go"));

    let point = find(&events);

    assert_eq!(point.reason, Reason::CleanCallPair);
    assert_ne!(point.reason, Reason::TurnBoundary);
    assert_eq!(point.at, 2);
}

// ---- a boundary with damage after it -----------------------------------------------------------

#[test]
fn a_broken_turn_after_a_boundary_still_lands_on_that_boundary() {
    // The two halves of the crash case together: turn 1 closed, turn 2 died inside a call. The answer
    // is the boundary — the position is turn 2's `turn/start` — so the closed turn is kept and the
    // open one is redone. Resuming at the end of the log would carry the unanswered call into the
    // history the provider sees, which is the one history it rejects.
    let mut events = log(1);
    push_crashed_turn(&mut events, 2);

    let point = find(&events);

    assert_eq!(point.reason, Reason::TurnBoundary);
    assert_eq!(point.turn, 1, "the turn that completed is the one reported");
    assert_eq!(point.at, 8, "one past turn 1's `turn/end`");
    assert!(matches!(
        events.get(point.at),
        Some(SessionEvent::TurnStart { turn: 2 })
    ));
    assert!(is_safe(&events, &point));
    assert!(!is_safe(
        &events,
        &Point {
            at: events.len(),
            turn: 2,
            reason: Reason::MidTurn { turn: 2 },
        }
    ));
}

#[test]
fn a_boundary_with_an_orphan_result_after_it_cuts_at_the_boundary() {
    // The other shape after a boundary: a result whose call the log lost. Cutting at the boundary is
    // correct — the closed turns are intact — and the check must reject the prefix that keeps the
    // orphan, which is what makes the two functions disagree deliberately rather than by accident.
    let mut events = log(1);
    events.push(SessionEvent::TurnStart { turn: 2 });
    events.push(result("call-lost"));

    let point = find(&events);

    assert_eq!(point.reason, Reason::TurnBoundary);
    assert_eq!(point.at, 8);
    assert!(is_safe(&events, &point), "the kept prefix ends on turn 1");
    assert!(
        !is_safe(
            &events,
            &Point {
                at: events.len(),
                turn: 2,
                reason: Reason::CleanCallPair,
            }
        ),
        "the orphan result makes the full log unsafe"
    );
}

// ---- no boundary -------------------------------------------------------------------------------

#[test]
fn a_log_with_only_a_prompt_resumes_at_the_end() {
    // No `turn/end` anywhere and no tool traffic at all: the pairing check is vacuously true, so the
    // prompt survives instead of being replayed.
    let mut events = vec![SessionEvent::TurnStart { turn: 1 }];
    events.push(SessionEvent::user_text("u1", "hello"));

    let point = find(&events);

    assert_eq!(point.reason, Reason::CleanCallPair);
    assert_eq!(point.turn, 1);
    assert_eq!(point.at, 2);
}

#[test]
fn a_log_with_no_turn_at_all_is_kept_whole() {
    // Records a session writes before its first prompt — a title, a plan-mode flag — name no turn.
    // There is nothing to redo and nothing to cut back to, so the log is kept and no turn is invented.
    let events = vec![
        SessionEvent::SessionTitle {
            title: "a session".to_owned(),
            message_seqs: Vec::new(),
            source: "human".to_owned(),
        },
        SessionEvent::PlanMode { active: false },
    ];

    let point = find(&events);

    assert_eq!(point.reason, Reason::CleanCallPair);
    assert_eq!(point.turn, 0, "no turn may be invented");
    assert_eq!(point.at, 2);
    assert!(is_safe(&events, &point));
}

#[test]
fn a_turn_less_log_whose_only_call_is_unanswered_starts_from_zero() {
    // A call with no turn around it: no boundary to keep and no `turn/start` to rewind to, so the only
    // position that does not claim a half-written exchange is the beginning.
    let events = vec![SessionEvent::user_text("u1", "go"), call("call-1")];

    let point = find(&events);

    assert_eq!(point.reason, Reason::FromStart);
    assert_eq!(point.at, 0);
    assert_eq!(point.turn, 0);
    assert!(events.get(point.at).is_some());
    assert!(is_safe(&events, &point));
}

// ---- the empty log -----------------------------------------------------------------------------

#[test]
fn an_empty_log_reports_from_start_at_zero() {
    // A brand-new session is this case. "Continue from the first event" and "there are no events" are
    // both true, and the position has to be `0` rather than `len()` for a caller that does
    // `events.get(point.at)` to be looking at the log rather than past it.
    let events: Vec<SessionEvent> = Vec::new();

    let point = find(&events);

    assert_eq!(
        point,
        Point {
            at: 0,
            turn: 0,
            reason: Reason::FromStart,
        }
    );
    assert!(is_safe(&events, &point));
}

#[test]
fn an_empty_log_is_unsafe_at_any_position_past_it() {
    let events: Vec<SessionEvent> = Vec::new();

    assert!(is_safe(
        &events,
        &Point {
            at: 0,
            turn: 0,
            reason: Reason::FromStart,
        }
    ));
    // Anything past the end describes a log that is not this one.
    assert!(!is_safe(
        &events,
        &Point {
            at: 1,
            turn: 1,
            reason: Reason::TurnBoundary,
        }
    ));
}

// ---- is_safe verifies the pairing --------------------------------------------------------------

#[test]
fn is_safe_accepts_a_boundary_and_rejects_a_cut_inside_a_pair() {
    // Safety is about the prefix, not about the reason: a healthy log is safe at its boundaries and
    // unsafe *between* a call and its answer, because the prefix there ends on a request nothing has
    // answered. That is the same property the turn boundary secures for a whole turn.
    let events = log(1);

    // After the `turn/end`.
    assert!(is_safe(
        &events,
        &Point {
            at: events.len(),
            turn: 1,
            reason: Reason::TurnBoundary,
        }
    ));
    // Even inside a healthy turn, a cut between `tool/call` (index 4) and `tool/result` (index 5)
    // leaves the call unanswered.
    assert!(!is_safe(
        &events,
        &Point {
            at: 5,
            turn: 1,
            reason: Reason::FromStart,
        }
    ));
    // One event earlier is before the call, and therefore balanced.
    assert!(is_safe(
        &events,
        &Point {
            at: 4,
            turn: 1,
            reason: Reason::FromStart,
        }
    ));
}

#[test]
fn an_unanswered_call_makes_the_points_after_it_unsafe() {
    // The verification must not be a restatement of the reason: a `TurnBoundary` point placed past an
    // unanswered call is unsafe, and this is exactly the case a naive implementation would allow
    // because the log does contain a `turn/end`.
    let mut events = log(1);
    events.push(SessionEvent::TurnStart { turn: 2 });
    events.push(call("call-2"));

    assert!(!is_safe(
        &events,
        &Point {
            at: events.len(),
            turn: 2,
            reason: Reason::TurnBoundary,
        }
    ));

    // The boundary itself is still balanced: the open call comes after it.
    assert!(is_safe(
        &events,
        &Point {
            at: 8,
            turn: 1,
            reason: Reason::TurnBoundary,
        }
    ));
}

#[test]
fn an_orphan_result_makes_the_prefix_unsafe() {
    // The mirror image: a result whose call the log lost. A resume that respected it would show the
    // model answering a request it never made, so the prefix is not a conversation.
    let mut events = vec![SessionEvent::TurnStart { turn: 1 }];
    events.push(result("call-1"));

    assert!(!is_safe(
        &events,
        &Point {
            at: events.len(),
            turn: 1,
            reason: Reason::CleanCallPair,
        }
    ));
    // The position before the orphan result is fine.
    assert!(is_safe(
        &events,
        &Point {
            at: 1,
            turn: 1,
            reason: Reason::FromStart,
        }
    ));
}

#[test]
fn pairing_is_counted_per_call_id() {
    // Two calls that reuse one id are two requests, and one result answers only one of them. A total
    // balance would call this paired; it is not.
    let mut events = vec![SessionEvent::TurnStart { turn: 1 }];
    events.push(call("same"));
    events.push(call("same"));
    events.push(result("same"));

    assert!(!is_safe(
        &events,
        &Point {
            at: events.len(),
            turn: 1,
            reason: Reason::CleanCallPair,
        }
    ));
    assert_eq!(find(&events).reason, Reason::MidTurn { turn: 1 });

    // The second result settles the second call, and the prefix becomes safe.
    events.push(result("same"));
    assert!(is_safe(
        &events,
        &Point {
            at: events.len(),
            turn: 1,
            reason: Reason::CleanCallPair,
        }
    ));
    assert_eq!(find(&events).reason, Reason::CleanCallPair);
}

#[test]
fn a_position_past_the_end_is_not_safe() {
    // A stale point — from a longer log, or from a different session — names events that are not
    // here, so it cannot be verified and must not be trusted.
    let events = log(1);

    assert!(!is_safe(
        &events,
        &Point {
            at: events.len() + 1,
            turn: 1,
            reason: Reason::TurnBoundary,
        }
    ));
    assert!(is_safe(
        &events,
        &Point {
            at: events.len(),
            turn: 1,
            reason: Reason::TurnBoundary,
        }
    ));
}

// ---- find and is_safe agree where it matters ---------------------------------------------------

#[test]
fn every_point_find_returns_is_safe_and_inside_the_log() {
    // The invariant tying the two functions together over the shapes this module is about. A `find`
    // that returned an unsafe point would make the pair contradictory, and a point past the end would
    // make the slicing caller panic.
    let mut crashed_tail = log(2);
    push_crashed_turn(&mut crashed_tail, 3);

    let mut crashed_only = Vec::new();
    push_crashed_turn(&mut crashed_only, 1);

    let cases = vec![
        Vec::new(),
        log(1),
        log(3),
        vec![SessionEvent::TurnStart { turn: 1 }],
        crashed_tail,
        crashed_only,
        vec![SessionEvent::user_text("u1", "go"), call("call-1")],
    ];

    for events in cases {
        let point = find(&events);
        assert!(
            point.at <= events.len(),
            "a point must be inside its log (at {}, len {})",
            point.at,
            events.len()
        );
        assert!(
            is_safe(&events, &point),
            "find returned an unsafe point: {point:?}"
        );
    }
}

#[test]
fn a_point_lands_where_its_reason_says() {
    // The reason and the position are one claim, not two: a `TurnBoundary` point sits immediately
    // after a `turn/end`, and a `MidTurn` point sits exactly on the `turn/start` of the turn it names.
    let mut events = Vec::new();
    push_crashed_turn(&mut events, 1);
    let mid = find(&events);
    let Reason::MidTurn { turn } = mid.reason else {
        panic!("expected a mid-turn point, got {mid:?}");
    };
    assert!(matches!(
        events.get(mid.at),
        Some(SessionEvent::TurnStart { turn: named }) if *named == turn
    ));

    let closed = log(2);
    let bounded = find(&closed);
    assert!(matches!(
        closed.get(bounded.at - 1),
        Some(SessionEvent::TurnEnd { .. })
    ));
}
