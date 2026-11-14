//! Which model served which turn of a session.
//!
//! A session log records every model selection it made, but a selection is only meaningful next to
//! the turns it governed. This module turns the ordered selection events into that answer: *at turn
//! 3, which model was in force?*
//!
//! # Why the latest selection is not enough
//!
//! [`crate::projection::ModelSelectionProjection`] keeps `lastUsed` — one value, overwritten on every
//! selection. That answers "what would the next step use", which is the right question for a picker
//! and the wrong one for everything else. A conversation that ran turns 1 to 3 on one model and 4 to
//! 6 on another cannot be explained by either single value: a reader asking "why was turn 3 slow"
//! needs the model of turn 3, and a fork resumed from turn 2 must know which model it is resuming
//! under. Keeping the whole sequence costs a few entries and answers both.
//!
//! # Why selections, not messages
//!
//! Messages already carry their producer — [`crate::MessageSource::Model`] names the provider and
//! model, and a `ModelUsage` event names the model that answered. But provenance is per *message*,
//! and a turn is not a message. A turn that was cancelled before the first response wrote no
//! assistant message and no usage, yet a model was chosen for it and may have spent tokens; a turn
//! whose steps ran several calls has one selection and many messages. The selection stream is the
//! only record of the decision itself, so it is what this module reads.
//!
//! # Why repeated selections are collapsed
//!
//! A selection is written at each turn boundary, so the same model name appears many times in a row.
//! Counting those events would report a session that never changed models as one that changed
//! constantly, and [`Memory::switches`] exists precisely to avoid that inflation: it counts
//! transitions between contiguous runs of one model, not events. Collapsing at insertion time also
//! keeps [`Memory::model_at`] a binary search over a short vector instead of a scan over the log.
//!
//! # Why `build` infers the turn
//!
//! [`SessionEvent::ModelSelection`] carries no turn number — `SessionEvent::turn` answers `None` for
//! it — because the writer emits it at a boundary rather than inside a step. Its position is
//! therefore the only evidence, and [`Memory::build`] reads it as follows: while a turn is open (a
//! `turn/start` with no matching `turn/end` yet) the selection applies to that turn, and otherwise
//! it applies to the turn after the last one the log names — the turn it was written ahead of. A log
//! that has named no turn at all starts at turn 1.
//!
//! # Why the tail after the last selection is not counted
//!
//! [`Memory::turns_per_model`] covers turns from the first selection's `from_turn` through the last
//! selection's. Turns after the final selection are deliberately not extrapolated: the log would have
//! written a selection at the start of each of them, so their absence is evidence the session stopped
//! rather than evidence that the last model ran and nobody recorded it. Guessing would let a resumed
//! session credit turns it never ran to whichever model happened to be selected last.

use std::collections::BTreeMap;

// Named through `::girder_core` rather than `crate::` so this file compiles both as a module of the
// library and, by `tests/model_memory.rs`, as a module of the test crate; `extern crate self as
// girder_core` in the crate root is what makes the path resolve from inside the library too.
use ::girder_core::SessionEvent;

/// One model selection.
///
/// A selection is a point in the log plus the turn it governs. The two are kept apart rather than
/// derived from one another because they answer different questions: `at` locates the event a reader
/// can jump to, and `from_turn` orders the selection against turns, which is what
/// [`Memory::model_at`] searches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Event index.
    ///
    /// The index of the `model/selection` event in the stream the selection came from, or whatever
    /// position the caller records. Ties on `from_turn` are ordered by this, so it only has to be
    /// monotonic with the log, not contiguous.
    pub at: usize,

    /// Model name.
    ///
    /// The provider-owned model id, as [`crate::ModelSelection`] names it. The provider is not kept:
    /// turns are attributed to a model, and two routes serving the same model id are the same answer
    /// to "which model ran this turn".
    pub model: String,

    /// Turn from which this selection takes effect.
    ///
    /// Turn numbering starts at 1, as [`SessionEvent::TurnStart`] numbers it. The selection covers
    /// every turn at or after this one until the next selection supersedes it.
    pub from_turn: u64,
}

impl Selection {
    /// Builds a selection.
    #[must_use]
    pub fn new(at: usize, model: impl Into<String>, from_turn: u64) -> Self {
        Self {
            at,
            model: model.into(),
            from_turn,
        }
    }
}

/// The history of model selections.
///
/// Holds one entry per contiguous run of a single model, ordered by `(from_turn, at)`. Two
/// consecutive selections of the same model collapse, because nothing observable differs between
/// "one turn on A" and "three turns on A" as far as the model in force is concerned — see the module
/// documentation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// Runs of one model, oldest first.
    segments: Vec<Selection>,

    /// The highest `from_turn` ever recorded.
    ///
    /// Kept beside the segments rather than read off the last one, because collapsing replaces a
    /// duplicate selection with the earlier entry and would otherwise discard the only evidence of
    /// how far the history reaches. [`Memory::turns_per_model`] needs that bound to give the last
    /// model its turns instead of a single one.
    last_turn: u64,
}

impl Memory {
    /// Creates an empty history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds the history from an event stream.
    ///
    /// The turn each selection governs is inferred from its position; see the module documentation
    /// for the rule. Rebuilding a log and recording its selections one by one must agree, which is
    /// why [`Memory::record`] applies the same collapse and keeps the same `last_turn` bound.
    #[must_use]
    pub fn build(events: &[SessionEvent]) -> Self {
        let mut memory = Self::new();
        // The turn whose `turn/start` has no matching `turn/end` yet. A selection written here was
        // written between the two, so it governs this turn rather than the next one.
        let mut open: Option<u64> = None;
        // The last turn the log named, from which a selection written at a boundary is the next one.
        let mut named: Option<u64> = None;

        for (at, event) in events.iter().enumerate() {
            match event {
                SessionEvent::TurnStart { turn } => {
                    open = Some(*turn);
                    named = Some(named.map_or(*turn, |seen| seen.max(*turn)));
                }
                SessionEvent::TurnEnd { turn, .. } => {
                    // Only the turn actually open is closed here: a `turn/end` naming another turn is
                    // a malformed log, and treating it as a close would move later selections to a
                    // turn they never governed.
                    if open == Some(*turn) {
                        open = None;
                    }
                    named = Some(named.map_or(*turn, |seen| seen.max(*turn)));
                }
                SessionEvent::ModelSelection { selection } => {
                    let after_last = named.map_or(1, |seen| seen.saturating_add(1));
                    memory.record(Selection {
                        at,
                        model: selection.model.clone(),
                        from_turn: open.unwrap_or(after_last),
                    });
                }
                // Steps and messages also name their turn, and a malformed log may hold one before
                // any `turn/start`; reading them keeps the "turn after the last one named" answer
                // from pointing at a turn the log has already moved past.
                other => {
                    if let Some(turn) = other.turn() {
                        named = Some(named.map_or(turn, |seen| seen.max(turn)));
                    }
                }
            }
        }

        memory
    }

    /// Records one selection.
    ///
    /// Selections are accepted out of order and land in `(from_turn, at)` order, so a caller that
    /// replays a merged or resumed log does not have to sort first. A selection naming the same model
    /// as its neighbour is folded into that neighbour: it changes nothing about which model was in
    /// force, and counting it as a switch would be wrong.
    pub fn record(&mut self, selection: Selection) {
        // Recorded before the collapse: the bound is a fact about the history, not about the runs
        // that survived it.
        self.last_turn = self.last_turn.max(selection.from_turn);

        let key = (selection.from_turn, selection.at);
        let position = self
            .segments
            .partition_point(|held| (held.from_turn, held.at) <= key);
        self.segments.insert(position, selection);
        // Keeps the first of each run, which is the earliest `(from_turn, at)` because the vector is
        // ordered by that key. The later duplicates carry no information: same model, later entry.
        self.segments
            .dedup_by(|later, earlier| later.model == earlier.model);
    }

    /// The model in force for `turn`.
    ///
    /// The selection in force *at that turn*, not the newest one: a session that changed models at
    /// turn 10 answered turn 3 with the model selected before it. `None` when no selection covers the
    /// turn — an empty history, a turn before the first selection, or a turn past the last one the
    /// history reaches.
    ///
    /// The far bound is the same one [`Memory::turns_per_model`] uses, for the same reason: a turn
    /// after the final selection is a turn the log never recorded a choice for. The model in force
    /// last probably still served it, but "probably" is not what this answers, and a caller that
    /// cannot tell a recorded turn from an extrapolated one reports the wrong model for a resumed
    /// session that switched between runs.
    #[must_use]
    pub fn model_at(&self, turn: u64) -> Option<&str> {
        if turn > self.last_turn {
            return None;
        }
        let after = self.segments.partition_point(|held| held.from_turn <= turn);
        self.segments
            .get(after.checked_sub(1)?)
            .map(|segment| segment.model.as_str())
    }

    /// The current (latest) model.
    ///
    /// `None` for a history with no selection. This is the answer a picker shows, and it says
    /// nothing about any earlier turn — which is exactly why [`Memory::model_at`] exists.
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        self.segments.last().map(|segment| segment.model.as_str())
    }

    /// How many times the model changed.
    ///
    /// The number of transitions between contiguous runs of one model: `A A B B C` is two switches,
    /// and a session that always used one model is zero. Counting selection events instead would
    /// report the writer's per-turn records as changes the user never made.
    #[must_use]
    pub fn switches(&self) -> usize {
        // Runs, not events, so the answer is one less than the number of runs. `saturating_sub`
        // rather than `len() - 1`: an empty or single-run history has no transition to count.
        self.segments.len().saturating_sub(1)
    }

    /// How many turns each model covered, most-covered first.
    ///
    /// A model that was selected, replaced, and selected again appears once, with the turns of both
    /// runs summed: the question is how much of the conversation ran on it, not how it was
    /// interleaved. Coverage runs from the first selection's `from_turn` through the last one's;
    /// turns beyond the final selection are not extrapolated, per the module documentation.
    ///
    /// Ties are ordered by model name so two callers with the same history print the same table.
    #[must_use]
    pub fn turns_per_model(&self) -> Vec<(String, u64)> {
        let mut totals: BTreeMap<&str, u64> = BTreeMap::new();

        for (index, segment) in self.segments.iter().enumerate() {
            // Exclusive end: the next run's first turn belongs to that run, not to this one. The last
            // run is bounded by the furthest turn any selection named, which is what gives the
            // current model every turn up to the end of the recorded history rather than one.
            let until = self
                .segments
                .get(index + 1)
                .map_or(self.last_turn.saturating_add(1), |next| next.from_turn);
            let covered = until.saturating_sub(segment.from_turn);
            let total = totals.entry(segment.model.as_str()).or_insert(0);
            *total = total.saturating_add(covered);
        }

        let mut rows: Vec<(String, u64)> = totals
            .into_iter()
            .map(|(model, turns)| (model.to_owned(), turns))
            .collect();
        rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        rows
    }
}
