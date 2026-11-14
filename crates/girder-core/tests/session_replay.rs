//! Tests for comparing and replaying session logs.
//!
//! The properties pinned here are the ones a comparison feature is built on. A fork is a *prefix*
//! relationship, so the interesting cases are the asymmetric ones: one log is a prefix of the other,
//! and the difference is a tail rather than an edit. The prefix relation is tested from both
//! directions because "a is a prefix of b" and "b is a prefix of a" are different answers and a
//! symmetric implementation would pass only one of them.
//!
//! Divergence is tested mid-turn as well as between turns, because that is what a fork taken at the
//! end of a turn and then extended on both sides actually looks like: turn 3 opened in both logs, so
//! the last shared event belongs to turn 3 even though the turn itself is not shared.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/session_replay.rs"]
mod session_replay;

use girder_core::{SessionEvent, TurnEndReason};
use session_replay::{Diff, Snapshot, diff, divergence_turn, replay_until, snapshot};

/// A turn's opening event.
fn turn_start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

/// The prompt that opens a turn, with a text unique to `(turn, label)`.
fn prompt(turn: u64, label: &str) -> SessionEvent {
    SessionEvent::user_text(
        &format!("msg-u{turn}-{label}"),
        format!("ask {turn} {label}"),
    )
}

/// The reply that closes a turn's exchange, with a text unique to `(turn, label)`.
fn reply(turn: u64, label: &str) -> SessionEvent {
    SessionEvent::assistant_text(
        &format!("msg-a{turn}-{label}"),
        format!("answer {turn} {label}"),
        "test",
    )
}

/// A turn's closing event.
fn turn_end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    }
}

/// Appends one whole turn: start, prompt, reply, end.
fn push_turn(events: &mut Vec<SessionEvent>, turn: u64) {
    events.push(turn_start(turn));
    events.push(prompt(turn, "main"));
    events.push(reply(turn, "main"));
    events.push(turn_end(turn));
}

/// A log of `turns` complete turns, four events each.
fn log(turns: u64) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    for turn in 1..=turns {
        push_turn(&mut events, turn);
    }
    events
}

// ---- diff --------------------------------------------------------------------------------------

#[test]
fn identical_logs_share_everything() {
    let events = log(3);

    assert_eq!(
        diff(&events, &events),
        Diff {
            common_prefix: events.len(),
            only_a: 0,
            only_b: 0,
        }
    );
}

#[test]
fn an_empty_log_against_an_empty_log_is_empty() {
    assert_eq!(
        diff(&[], &[]),
        Diff {
            common_prefix: 0,
            only_a: 0,
            only_b: 0,
        }
    );
}

#[test]
fn a_log_with_events_against_an_empty_log_differs_at_zero() {
    let events = log(1);

    assert_eq!(
        diff(&events, &[]),
        Diff {
            common_prefix: 0,
            only_a: events.len(),
            only_b: 0,
        }
    );
}

#[test]
fn a_shorter_log_that_is_a_prefix_reports_no_extra_events_on_its_side() {
    // The fork shape, from the fork's side: the parent continues, the fork stops.
    let parent = log(3);
    let fork = log(2);
    let parent_tail = parent.iter().skip(fork.len()).cloned().collect::<Vec<_>>();

    let found = diff(&fork, &parent);
    assert_eq!(found.common_prefix, fork.len());
    assert_eq!(found.only_a, 0, "a fork adds nothing past the fork point");
    assert_eq!(found.only_b, parent_tail.len());
}

#[test]
fn a_longer_log_that_extends_a_prefix_reports_no_extra_events_on_the_other_side() {
    // The same relationship from the parent's side. A symmetric implementation would pass only one
    // of these two tests, which is why both exist.
    let parent = log(3);
    let fork = log(2);

    let found = diff(&parent, &fork);
    assert_eq!(found.common_prefix, fork.len());
    assert_eq!(found.only_a, parent.len() - fork.len());
    assert_eq!(found.only_b, 0, "the fork has nothing the parent lacks");
}

#[test]
fn both_tails_are_reported_when_both_logs_continued_differently() {
    // Forked after turn 1, then turn 2 was written differently on each side.
    let mut left = log(1);
    push_turn(&mut left, 2);
    let mut right = log(1);
    right.push(turn_start(2));
    right.push(prompt(2, "other"));

    let found = diff(&left, &right);
    assert_eq!(
        found.common_prefix, 5,
        "turn 1 and turn 2's opening are shared; the prompts differ"
    );
    assert_eq!(found.only_a, 3, "left's prompt, reply, and turn/end");
    assert_eq!(found.only_b, 1, "right's differently worded prompt");
}

#[test]
fn a_difference_in_the_middle_ends_the_prefix_there() {
    // Positional, not set-like: the shared events after the changed one do not count. That is the
    // deliberate cost of a prefix rule, and it is what makes the answer cheap.
    let events = log(1);
    let mut changed = events.clone();
    changed[1] = prompt(1, "rewritten");
    let found = diff(&events, &changed);
    assert_eq!(found.common_prefix, 1, "only turn/start is shared");
    assert_eq!(found.only_a, 3);
    assert_eq!(found.only_b, 3);

    let identical_tail = &events[2..];
    let changed_tail = &changed[2..];
    assert_eq!(
        identical_tail, changed_tail,
        "the events themselves never converged, only the prefix rule stops early"
    );
}

// ---- divergence_turn ---------------------------------------------------------------------------

#[test]
fn divergence_turn_is_none_without_shared_history() {
    let empty: [SessionEvent; 0] = [];
    assert_eq!(divergence_turn(&[], &[]), None);
    assert_eq!(divergence_turn(&empty, &log(1)), None);

    // A shared prefix that names no turn has no turn to report: inventing turn 0 would be
    // indistinguishable from "no shared history".
    let title = SessionEvent::SessionTitle {
        title: "t".to_owned(),
        message_seqs: Vec::new(),
        source: "model".to_owned(),
    };
    assert_eq!(
        divergence_turn(std::slice::from_ref(&title), std::slice::from_ref(&title)),
        None
    );
}

#[test]
fn divergence_turn_is_the_turn_the_shared_prefix_ends_in() {
    // Diverge after turn 2 closed: the last shared event is turn 2's `turn/end`.
    let parent = log(3);
    let fork = log(2);
    assert_eq!(divergence_turn(&fork, &parent), Some(2));
    assert_eq!(
        divergence_turn(&parent, &fork),
        Some(2),
        "which log is passed first must not change the answer"
    );
}

#[test]
fn divergence_turn_is_reported_inside_a_turn_when_the_split_is_mid_turn() {
    // Forked after turn 1, then both opened turn 2 and wrote different prompts. The last shared
    // event is turn 2's `turn/start`, so the answer is turn 2 even though turn 2 is not shared.
    let mut left = log(1);
    left.push(turn_start(2));
    left.push(prompt(2, "left"));
    let mut right = log(1);
    right.push(turn_start(2));
    right.push(prompt(2, "right"));

    assert_eq!(divergence_turn(&left, &right), Some(2));
    assert_eq!(
        diff(&left, &right).common_prefix,
        5,
        "turn 1 plus both starts"
    );
}

#[test]
fn divergence_turn_of_identical_logs_is_their_final_turn() {
    // The rule is "where the shared content ends", so identical logs answer with their last turn.
    // Telling them apart from a divergence is `Diff`'s job, where both tails are zero.
    let events = log(2);
    assert_eq!(divergence_turn(&events, &events), Some(2));
    assert_eq!(diff(&events, &events).only_a, 0);
}

#[test]
fn divergence_turn_skips_turnless_events_at_the_end_of_the_shared_prefix() {
    // A `session/title` can be written after a turn closes, so the last shared event may carry no
    // turn while the turn before it does.
    let mut left = log(2);
    left.push(SessionEvent::SessionTitle {
        title: "left".to_owned(),
        message_seqs: vec![1],
        source: "model".to_owned(),
    });
    let mut right = log(2);
    right.push(SessionEvent::SessionTitle {
        title: "right".to_owned(),
        message_seqs: vec![1],
        source: "model".to_owned(),
    });

    assert_eq!(
        divergence_turn(&left, &right),
        Some(2),
        "the title differs, but the turns before it agree"
    );
}

// ---- replay_until ------------------------------------------------------------------------------

#[test]
fn replay_until_stops_at_the_condition_and_includes_it() {
    let events = log(2);

    let replayed = replay_until(&events, 0, |event| {
        matches!(event, SessionEvent::TurnEnd { .. })
    });
    assert_eq!(
        replayed.len(),
        4,
        "turn 1's four events, ending on its turn/end"
    );
    assert_eq!(replayed, events[..4]);
}

#[test]
fn replay_until_continues_to_the_next_stopping_point() {
    let events = log(2);

    let replayed = replay_until(&events, 4, |event| {
        matches!(event, SessionEvent::TurnEnd { .. })
    });
    assert_eq!(replayed, events[4..], "from turn 2's start to its end");
}

#[test]
fn replay_until_from_the_middle_of_a_turn_keeps_the_whole_remainder() {
    let events = log(1);

    let replayed = replay_until(&events, 2, |event| {
        matches!(event, SessionEvent::TurnEnd { .. })
    });
    assert_eq!(replayed, events[2..], "the reply and the closing event");
}

#[test]
fn replay_until_without_a_match_returns_everything_after_from() {
    let events = log(2);

    let replayed = replay_until(&events, 0, |_| false);
    assert_eq!(replayed, events);

    let replayed = replay_until(&events, 6, |_| false);
    assert_eq!(replayed, events[6..]);
}

#[test]
fn replay_until_that_matches_immediately_returns_one_event() {
    let events = log(1);

    let replayed = replay_until(&events, 0, |_| true);
    assert_eq!(replayed, events[..1]);
}

#[test]
fn replay_until_at_or_past_the_end_is_empty() {
    let events = log(1);

    // A fork that was taken at the end of its parent has nothing of its own to replay, and a stale
    // cursor past the end is the same situation. Neither is an error worth a channel.
    assert!(replay_until(&events, events.len(), |_| true).is_empty());
    assert!(replay_until(&events, events.len() + 10, |_| false).is_empty());
    assert!(replay_until(&[], 0, |_| true).is_empty());
}

#[test]
fn replay_until_from_a_divergence_point_replays_only_the_forked_tail() {
    let parent = log(3);
    let fork = log(2);

    let point = diff(&fork, &parent).common_prefix;
    let tail = replay_until(&parent, point, |event| {
        matches!(event, SessionEvent::TurnEnd { .. })
    });

    assert_eq!(
        tail,
        parent[point..],
        "the parent's turn 3, and nothing shared"
    );
    assert_eq!(
        divergence_turn(&fork, &parent),
        Some(2),
        "and the tail is what turn 3 did"
    );
}

// ---- snapshot ----------------------------------------------------------------------------------

#[test]
fn snapshot_of_an_empty_log_is_all_zero() {
    assert_eq!(
        snapshot(&[]),
        Snapshot {
            events: 0,
            last_turn: None,
            last_seq: 0,
        }
    );
}

#[test]
fn snapshot_summarises_length_turn_and_sequence() {
    let events = log(2);

    assert_eq!(
        snapshot(&events),
        Snapshot {
            events: 8,
            last_turn: Some(2),
            last_seq: 8,
        }
    );
}

#[test]
fn snapshot_reports_the_turn_in_flight_rather_than_the_last_closed_one() {
    // A caller watching for progress wants to see turn 3 start, not wait for turn 2's close to be
    // the last thing it can learn about.
    let mut events = log(2);
    events.push(turn_start(3));

    let found = snapshot(&events);
    assert_eq!(found.events, 9);
    assert_eq!(found.last_turn, Some(3));
    assert_eq!(found.last_seq, 9);
}

#[test]
fn snapshot_reports_no_turn_for_a_log_that_names_none() {
    let events = vec![SessionEvent::SessionTitle {
        title: "t".to_owned(),
        message_seqs: Vec::new(),
        source: "model".to_owned(),
    }];

    let found = snapshot(&events);
    assert_eq!(found.events, 1);
    assert_eq!(found.last_turn, None);
    assert_eq!(found.last_seq, 1);
}

#[test]
fn snapshots_that_differ_imply_logs_that_differ() {
    let parent = log(2);
    let fork = log(3);

    let before = snapshot(&parent);
    let after = snapshot(&fork);
    assert_ne!(before, after);
    assert_eq!(before.events, diff(&parent, &fork).common_prefix);
}
