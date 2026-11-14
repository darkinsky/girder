//! A positional index over a session log: which events belong to which turn.
//!
//! # Why this exists
//!
//! Paging and replaying a conversation ask the log two questions it cannot answer by itself: "where
//! does turn 5 start and stop" and "which turn is the record at position 812 part of". Both are
//! answered today by scanning, and a scan is not free. [`crate::SessionStore::read_after`] shows the
//! shape of the cost: it decodes the whole log and then *drops* the prefix it was asked to skip, so a
//! client that wants to jump to the fifth turn pays for every event before it. `session.page` has the
//! same property from the other end — it slices `events[start..end]` only after the entire log has
//! been read, so its cursor arithmetic is bounded by how much was decoded, not by what was asked for.
//!
//! A client rendering a turn-only view ("jump to turn 5") therefore needs one thing the store does not
//! expose: a mapping from turn number to a byte-free range of positions, computed once over a slice
//! the caller already holds. That is this module. It does not read the log — it folds one pass over
//! `&[SessionEvent]` into a table, and every later question is a lookup instead of a scan.
//!
//! # What a span means
//!
//! A turn's span is `[start, end)` — the index of its `turn/start` through the index *after* its
//! `turn/end`. Half-open, for the same reason a `Vec` slice is: `end - start` is the number of events
//! in the turn, consecutive turns share a boundary index with no off-by-one to reason about, and an
//! empty range is representable. `turn/end` is part of its own turn, which is why the end is one past
//! it rather than the index of it — a reader that shows "the events of turn 3" must include the record
//! that says turn 3 finished.
//!
//! # Why an unclosed turn is a span, not an omission
//!
//! A turn with `turn/start` and no `turn/end` is not a corrupt log. It is the turn running right now,
//! or one whose process died mid-step. Dropping it would make the live turn disappear at exactly the
//! moment a client is watching it, so it is emitted with `end` at the end of the stream — and marked
//! as open, because "ended at the last event" and "never ended" are otherwise the same pair of
//! numbers. [`Index::open_turn`] is that mark.
//!
//! # Why events outside a turn are not attributed
//!
//! The vocabulary has records that belong to no turn: [`SessionEvent::SessionTitle`], the accounting
//! events, and — in this build — the `user/message` that is written *before* its `turn/start` so it
//! survives a crash before the model answers. Position alone would attribute that prompt to the
//! previous turn, which is the one turn it certainly does not belong to. [`Index::turn_of`] answers
//! `None` for those positions instead of guessing, so a caller that needs attribution can decide
//! what its own surface should show.
//!
//! # Cost
//!
//! [`Index::build`] is one pass, `O(n)` in events and `O(turns)` in memory. Every query is a binary
//! search over at most one entry per turn, so `O(log turns)`; there is no per-query allocation and no
//! rescan of the log.

use std::collections::BTreeMap;

// `girder_core::…` rather than `crate::…` so this file also builds when a test includes it by path;
// the library declares the alias for itself, which is what makes both spellings the same crate.
use girder_core::SessionEvent;

/// One turn's position in the event stream.
///
/// See the module docs for why the range is half-open and why an unclosed turn still gets one. The
/// pair is reported as it appears in the log: [`TurnSpan::turn`] is the number the log carried, not a
/// position in [`Index::turns`], because a forked or resumed log can repeat a number and renumbering
/// here would make a span impossible to match back against the events it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TurnSpan {
    /// Which turn this is.
    pub turn: u64,

    /// First event index of the turn, inclusive.
    ///
    /// The index of the `turn/start` record, which is the boundary a client pages back to when it
    /// jumps to a turn.
    pub start: usize,

    /// One past the last event index of the turn.
    ///
    /// Exclusive, so [`TurnSpan::end`] may equal the log length. A client slicing
    /// `events[span.start..span.end]` gets the turn and nothing of the next one.
    pub end: usize,
}

impl TurnSpan {
    /// How many events the turn contains.
    ///
    /// `end - start` and not `end - start + 1`: the range is half-open, so a turn consisting only of
    /// its `turn/start` and `turn/end` reports 2, which is what a debugger counting records expects.
    #[must_use]
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Whether the turn contains no events at all.
    ///
    /// Only reachable for a hand-built span: [`Index::build`] never emits one, because a span exists
    /// only because a `turn/start` was seen. Present so a caller holding a `TurnSpan` does not have to
    /// spell `span.len() == 0` and trip the lints that flag it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A turn-position table over a session's events.
///
/// Built once per log (or per page of one) and queried many times; see the module docs for the cost
/// and for what an unclosed turn looks like. The type is deliberately not a map of turn number to
/// span: [`Index::turns`] has to be a sequence in log order, so the spans are the storage and the
/// `BTreeMap` is only the by-number lookup built alongside them.
#[derive(Clone, Debug, Default)]
pub struct Index {
    /// Every turn, in log order.
    ///
    /// The order is the log's, not the turn numbers': a forked log can hold turn 3 after turn 7, and a
    /// caller walking [`Index::turns`] is walking what happened.
    turns: Vec<TurnSpan>,

    /// Turn number to position in [`Index::turns`].
    ///
    /// `usize` rather than the span itself, so the ordered vector stays the single copy and the map
    /// stays small enough to rebuild cheaply. The first span recorded for a number wins: a repeated
    /// number means a log that replayed a turn, and the earlier record is the one the events actually
    /// followed.
    by_turn: BTreeMap<u64, usize>,

    /// Position in [`Index::turns`] of the turn that has no `turn/end`.
    ///
    /// At most one turn can be open: a later `turn/start` closes whatever came before it, since a log
    /// cannot run two turns of one session at once. `None` is the ordinary case — a finished log.
    open: Option<usize>,
}

impl Index {
    /// An empty index.
    ///
    /// Same as [`Index::build`] on an empty slice, and the value a consumer starts from before the
    /// log exists: a session with no events has no turns, and a table of zero rows says that without a
    /// placeholder turn.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the table in one pass over the events.
    ///
    /// Records are classified by position, not by the `turn` field they carry: only some variants name
    /// their turn, and the ones that do not ([`SessionEvent::ToolCall`],
    /// [`SessionEvent::AssistantMessage`]) are attributed to the open turn by where they sit. That is
    /// also why a turn is closed by the *next* `turn/start` as well as by its own `turn/end` — the
    /// boundary is the only evidence available when a process died before writing the end.
    ///
    /// A repeated `turn/start` for the turn already open is ignored rather than treated as a new turn:
    /// the log writer has produced two records for one boundary before, and splitting there would
    /// report one turn as two ranges with half the events each.
    #[must_use]
    pub fn build(events: &[SessionEvent]) -> Self {
        let mut turns: Vec<TurnSpan> = Vec::with_capacity(events.len().saturating_div(8));
        // The turn being accumulated. Its `end` starts at the stream end, which is what an unclosed
        // turn keeps; every way of closing it overwrites that with a known boundary.
        let mut open: Option<TurnSpan> = None;

        for (position, event) in events.iter().enumerate() {
            match event {
                SessionEvent::TurnStart { turn } => {
                    if open.as_ref().is_some_and(|span| span.turn == *turn) {
                        continue;
                    }
                    // A turn that never saw its `turn/end` stops here: this boundary is the first
                    // evidence that it did, and the next turn's start is not part of it.
                    if let Some(mut span) = open.take() {
                        span.end = position;
                        turns.push(span);
                    }
                    open = Some(TurnSpan {
                        turn: *turn,
                        start: position,
                        end: events.len(),
                    });
                }
                // Only the turn that is actually open can be closed here, which is why the match
                // carries the test as a guard: a `turn/end` naming another number is a mismatched
                // log, and closing whatever happened to be open would invent a boundary the log
                // never recorded.
                SessionEvent::TurnEnd { turn, .. }
                    if open.as_ref().is_some_and(|span| span.turn == *turn) =>
                {
                    let Some(mut span) = open.take() else {
                        continue;
                    };
                    // `+ 1` because the end is exclusive and this record is the turn's last.
                    span.end = position + 1;
                    turns.push(span);
                }
                _ => {}
            }
        }

        // Whatever is still open ran to the end of the stream and never closed.
        if let Some(span) = open.take() {
            turns.push(span);
        }

        let open = turns.len().checked_sub(1).filter(|last| {
            // A span reaching the end of the stream is only open when the stream did not end with
            // that turn's own `turn/end`. Both look identical in the numbers, which is why the flag
            // is kept here instead of being inferred by a caller from `end == events.len()`.
            turns
                .get(*last)
                .is_some_and(|span| span.end == events.len() && !ended_at(events, span))
        });

        let mut by_turn = BTreeMap::new();
        for (position, span) in turns.iter().enumerate() {
            by_turn.entry(span.turn).or_insert(position);
        }

        Self {
            turns,
            by_turn,
            open,
        }
    }

    /// The span of one turn, by the number the log carried.
    ///
    /// `None` when the log has no such turn. When a number repeats — a fork or a resume replaying a
    /// turn — the first span carrying it is the one returned; [`Index::turns`] is how a caller sees
    /// every occurrence.
    #[must_use]
    pub fn turn_span(&self, turn: u64) -> Option<TurnSpan> {
        self.by_turn
            .get(&turn)
            .and_then(|position| self.turns.get(*position))
            .copied()
    }

    /// Every turn, in log order.
    ///
    /// Unclosed turns are included. This is the rail a client renders when it wants the shape of a
    /// session without any of its events, so the slice is returned by reference rather than built per
    /// call.
    #[must_use]
    pub fn turns(&self) -> &[TurnSpan] {
        &self.turns
    }

    /// The turn that owns the event at `index`.
    ///
    /// `None` for a position outside every turn — before the first `turn/start`, after the last
    /// `turn/end`, past the end of the stream, or in a gap a mismatched log left between two turns.
    /// See the module docs for why those are not attributed to a neighbour.
    #[must_use]
    pub fn turn_of(&self, index: usize) -> Option<u64> {
        // `turns` is ordered and disjoint, so the candidate is the last span that starts at or before
        // `index`; whether the position is inside it is the second test.
        let after = self.turns.partition_point(|span| span.start <= index);
        let span = self.turns.get(after.checked_sub(1)?)?;
        (index < span.end).then_some(span.turn)
    }

    /// How many turns the log contains.
    ///
    /// Turn count, not event count: the caller asking this is sizing a list of turns.
    #[must_use]
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    /// Whether the log holds no turn at all.
    ///
    /// True for an empty log and for one that has not reached its first `turn/start` yet — a session
    /// that was opened and titled but never prompted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// The turn that never closed, if the log has one.
    ///
    /// The turn still in flight, or the one whose writer died. A client renders a spinner on it;
    /// [`TurnSpan`] alone cannot say which span that is, because a turn that ended on the final event
    /// and one that never ended have the same `end`.
    #[must_use]
    pub fn open_turn(&self) -> Option<u64> {
        self.open
            .and_then(|position| self.turns.get(position))
            .map(|span| span.turn)
    }

    /// Whether the named turn is the unclosed one.
    ///
    /// The question a caller holding a `TurnSpan` actually has. False for a turn number the log does
    /// not contain, and false for a closed turn.
    #[must_use]
    pub fn is_open(&self, turn: u64) -> bool {
        self.open_turn() == Some(turn)
    }
}

/// Whether the span ends with its own `turn/end` record.
///
/// Asked only of the final span, to tell "this turn closed on the last event" from "this turn ran out
/// of log". The record at `end - 1` is the only one that can close it, and it must name the same
/// turn: a `turn/end` for some other number does not close this one.
fn ended_at(events: &[SessionEvent], span: &TurnSpan) -> bool {
    let Some(last) = span.end.checked_sub(1) else {
        return false;
    };
    matches!(
        events.get(last),
        Some(SessionEvent::TurnEnd { turn, .. }) if *turn == span.turn
    )
}

#[cfg(test)]
mod tests {
    use girder_core::SessionEvent;

    use super::{Index, TurnSpan};

    /// A minimal two-event turn, so a test can state an exact range.
    fn closed_turn(turn: u64) -> [SessionEvent; 2] {
        [
            SessionEvent::TurnStart { turn },
            SessionEvent::TurnEnd {
                turn,
                reason: girder_core::TurnEndReason::Completed,
            },
        ]
    }

    #[test]
    fn one_closed_turn_spans_its_boundaries() {
        let events = closed_turn(1);
        let index = Index::build(&events);

        assert_eq!(index.len(), 1);
        assert_eq!(
            index.turn_span(1),
            Some(TurnSpan {
                turn: 1,
                start: 0,
                end: 2,
            })
        );
        assert_eq!(index.open_turn(), None);
    }

    #[test]
    fn two_turns_share_a_boundary_index() {
        let mut events = Vec::new();
        events.extend(closed_turn(1));
        events.extend(closed_turn(2));

        let index = Index::build(&events);

        assert_eq!(index.len(), 2);
        assert_eq!(index.turns()[0].end, index.turns()[1].start);
        assert_eq!(index.turns()[1].end, events.len());
    }

    #[test]
    fn a_turn_with_no_end_runs_to_the_end_of_the_stream() {
        let events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::user_text("m1", "go"),
            SessionEvent::StepStart { turn: 1, step: 1 },
        ];

        let index = Index::build(&events);

        let span = index.turn_span(1).expect("the open turn is indexed");
        assert_eq!(span.end, events.len());
        assert_eq!(index.open_turn(), Some(1));
        assert!(index.is_open(1));
    }

    #[test]
    fn a_turn_closed_on_the_final_event_is_not_open() {
        // The distinguishing case: `end == events.len()` is true for both, so the flag cannot be
        // inferred from the range.
        let events = closed_turn(1);
        let index = Index::build(&events);

        let span = index.turn_span(1).expect("indexed");
        assert_eq!(span.end, events.len());
        assert_eq!(index.open_turn(), None);
        assert!(!index.is_open(1));
    }

    #[test]
    fn a_turn_closed_by_a_following_start_is_still_closed() {
        let events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::user_text("m1", "first"),
            SessionEvent::TurnStart { turn: 2 },
        ];

        let index = Index::build(&events);

        assert_eq!(index.turns().len(), 2);
        // The first turn stops at the next boundary and is not the open one.
        assert_eq!(index.turn_span(1).map(|span| span.end), Some(2));
        assert!(!index.is_open(1));
        assert_eq!(index.open_turn(), Some(2));
    }

    #[test]
    fn turn_of_covers_every_position_of_a_turn() {
        let mut events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::user_text("m1", "go"),
            SessionEvent::StepStart { turn: 1, step: 1 },
            SessionEvent::assistant_text("a1", "done", "deepseek"),
        ];
        events.extend(closed_turn(2));

        let index = Index::build(&events);
        let span = index.turn_span(1).expect("indexed");

        // Every index inside the half-open range resolves to the turn, including both boundaries:
        // `turn/start` belongs to turn 1, and so does `turn/end` at `end - 1`.
        for position in span.start..span.end {
            assert_eq!(index.turn_of(position), Some(1), "position {position}");
        }
        // One past the end is the next turn, not this one.
        assert_eq!(index.turn_of(span.end), Some(2));
    }

    #[test]
    fn turn_of_stops_where_the_turn_stops() {
        let events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::TurnEnd {
                turn: 1,
                reason: girder_core::TurnEndReason::Completed,
            },
            SessionEvent::SessionTitle {
                title: "a title".to_owned(),
                message_seqs: vec![1],
                source: "model".to_owned(),
            },
        ];

        let index = Index::build(&events);

        assert_eq!(index.turn_of(0), Some(1));
        assert_eq!(index.turn_of(1), Some(1));
        // The title follows the turn and belongs to none.
        assert_eq!(index.turn_of(2), None);
    }

    #[test]
    fn events_before_the_first_turn_belong_to_no_turn() {
        // This build writes the prompt before spawning the loop, so the prompt precedes `turn/start`.
        // Attributing it forward would name a turn that had not started; backward has no answer.
        let events = vec![
            SessionEvent::user_text("m1", "hello"),
            SessionEvent::SessionTitle {
                title: "greeting".to_owned(),
                message_seqs: vec![],
                source: "human".to_owned(),
            },
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::TurnEnd {
                turn: 1,
                reason: girder_core::TurnEndReason::Completed,
            },
        ];

        let index = Index::build(&events);

        assert_eq!(index.turn_of(0), None);
        assert_eq!(index.turn_of(1), None);
        assert_eq!(index.turn_of(2), Some(1));
        assert_eq!(index.turn_span(1).map(|span| span.start), Some(2));
    }

    #[test]
    fn an_empty_stream_has_no_turns() {
        let index = Index::build(&[]);

        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
        assert!(index.turns().is_empty());
        assert_eq!(index.turn_of(0), None);
        assert_eq!(index.open_turn(), None);
        assert_eq!(index.turn_span(1), None);
    }

    #[test]
    fn a_stream_with_no_turn_events_is_empty() {
        let events = vec![SessionEvent::user_text("m1", "unanswered")];
        let index = Index::build(&events);

        assert!(index.is_empty());
        assert_eq!(index.turn_of(0), None);
    }

    #[test]
    fn an_absent_turn_has_no_span() {
        let mut events = Vec::new();
        events.extend(closed_turn(1));
        events.extend(closed_turn(2));

        let index = Index::build(&events);

        assert_eq!(index.turn_span(0), None);
        assert_eq!(index.turn_span(3), None);
        assert_eq!(index.turn_span(u64::MAX), None);
    }

    #[test]
    fn consecutive_turns_partition_the_stream() {
        let events: Vec<SessionEvent> = (1..=4)
            .flat_map(|turn| {
                [
                    SessionEvent::TurnStart { turn },
                    SessionEvent::user_text(&format!("m{turn}"), "go"),
                    SessionEvent::TurnEnd {
                        turn,
                        reason: girder_core::TurnEndReason::Completed,
                    },
                ]
            })
            .collect();

        let index = Index::build(&events);

        assert_eq!(index.len(), 4);
        // No gap and no overlap: each span starts where the previous ended, and every position is
        // attributed to exactly one turn.
        let mut expected_start = 0;
        for (position, span) in index.turns().iter().enumerate() {
            let turn = u64::try_from(position).map_or(0, |value| value + 1);
            assert_eq!(span.turn, turn);
            assert_eq!(span.start, expected_start);
            expected_start = span.end;
        }
        assert_eq!(expected_start, events.len());
        for position in 0..events.len() {
            assert!(index.turn_of(position).is_some(), "position {position}");
        }
        assert_eq!(index.turn_of(events.len()), None);
    }

    #[test]
    fn a_repeated_start_does_not_split_a_turn() {
        let events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::user_text("m1", "go"),
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::TurnEnd {
                turn: 1,
                reason: girder_core::TurnEndReason::Completed,
            },
        ];

        let index = Index::build(&events);

        assert_eq!(index.len(), 1);
        assert_eq!(
            index.turn_span(1),
            Some(TurnSpan {
                turn: 1,
                start: 0,
                end: 4,
            })
        );
    }

    #[test]
    fn a_mismatched_end_does_not_close_the_open_turn() {
        // A `turn/end` naming another number is a malformed log. Closing whatever is open would invent
        // a boundary, so the open turn keeps running and the record is not attributed to it either.
        let events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::TurnEnd {
                turn: 9,
                reason: girder_core::TurnEndReason::Completed,
            },
        ];

        let index = Index::build(&events);

        assert_eq!(index.turn_span(1).map(|span| span.end), Some(2));
        assert_eq!(index.open_turn(), Some(1));
        assert_eq!(index.turn_of(1), Some(1));
    }

    #[test]
    fn turn_of_is_consistent_with_every_span() {
        let mut events = vec![SessionEvent::user_text("m0", "orphan")];
        for turn in [1_u64, 2, 5] {
            events.push(SessionEvent::TurnStart { turn });
            events.push(SessionEvent::user_text(&format!("m{turn}"), "go"));
            events.push(SessionEvent::TurnEnd {
                turn,
                reason: girder_core::TurnEndReason::Completed,
            });
        }
        // A title in the gap before the final, unclosed turn.
        events.push(SessionEvent::TurnStart { turn: 6 });
        events.push(SessionEvent::SessionTitle {
            title: "tail".to_owned(),
            message_seqs: vec![],
            source: "model".to_owned(),
        });

        let index = Index::build(&events);

        for span in index.turns() {
            for position in span.start..span.end {
                assert_eq!(index.turn_of(position), Some(span.turn));
            }
            assert_eq!(index.turn_span(span.turn), Some(*span));
        }
        assert_eq!(index.turn_of(0), None, "the orphan prompt");
        assert_eq!(index.len(), 4);
        assert_eq!(index.open_turn(), Some(6));
    }
}
