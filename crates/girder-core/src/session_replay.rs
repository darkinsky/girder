//! Comparing two session logs: where they agree, where they part, and how to replay from there.
//!
//! A session log is append-only and totally ordered — see [`crate::session`] — so two logs built
//! from the same history share a *prefix*, not a scattering of interleaved lines. That single fact
//! is what makes comparison cheap: the first differing position is the whole answer, and everything
//! from there on is a tail.
//!
//! # Why a common prefix instead of a minimal edit distance
//!
//! A minimal edit distance (LCS, Myers, patience) answers "how do I turn A into B with the fewest
//! insertions and deletions". That question is only interesting when the two sequences are *not*
//! ordered by construction: two drafts of one file, where a paragraph moved and every later line
//! shifted. Session events are never like that. [`crate::SessionStore::append`] only writes at the
//! end, and a fork copies a prefix into a fresh file, so any two logs that share history agree on
//! an exact prefix of events and can only differ afterwards.
//!
//! Running an alignment algorithm on data that has this shape does not just cost more, it reports
//! the wrong thing. Given the shared history `[a, b]` and the fork `[a, b, c]`, an LCS-based diff
//! is entitled to say "b was removed and re-added"; the truth a user needs is "they agree on a and
//! b, and one of them continued with c". Worse, the quadratic alignment is a denial of service on
//! long sessions, which are exactly the logs a user compares.
//!
//! # What "the divergence point" means here
//!
//! The last event two logs share is not always the end of a turn: a rewind cuts on a turn boundary,
//! but a running session and its fork can diverge in the middle of a turn — the fork was taken at
//! the end of turn 2, then both went on into turn 3 and wrote different prompts. So
//! [`divergence_turn`] reports the turn the last shared event belongs to, and the caller reads it
//! with the position in hand:
//!
//! - the last shared event is a `turn/end` — the logs agree through that turn and differ afterwards;
//! - the last shared event is anything else — the logs differ *inside* that turn.
//!
//! Both are the same rule, and the alternative (report only `turn/end` events) would answer `None`
//! for the mid-turn case, which is the case a `/compare` command actually hits.
//!
//! # Why a snapshot exists at all
//!
//! Listing sessions to compare them should not decode every log. A [`Snapshot`] is the three cheap
//! facts that answer "did this change since I last looked": how long the log is, which turn it last
//! named, and its last sequence number. A caller compares snapshots to skip the expensive step, and
//! only reads the events when two snapshots actually disagree.

use girder_core::SessionEvent;

/// The difference between two event streams.
///
/// Three numbers rather than a list of changed events: the point of the comparison is the shape of
/// the fork — how much the logs agree on, how much each added — and a caller that wants the events
/// themselves can slice the tails with [`Diff::common_prefix`]. Materialising them here would make
/// every comparison clone a suffix, which is the opposite of what a cheap answer needs.
///
/// `only_a + only_b` is the number of events that differ. It is deliberately *not* a minimal edit
/// distance: the same event in a different order counts twice, because an append-only log cannot
/// reorder anything, so such a pair means two histories that genuinely diverge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diff {
    /// How many leading events the two logs share, in order.
    ///
    /// Also the index at which each log's tail begins, so `a[..common_prefix]` and
    /// `b[..common_prefix]` are the same events.
    pub common_prefix: usize,

    /// How many events `a` has after the common prefix.
    ///
    /// Zero means `a` is a prefix of `b` — the exact shape a fork has, since a fork copies a prefix
    /// and adds nothing.
    pub only_a: usize,

    /// How many events `b` has after the common prefix.
    ///
    /// Zero means `b` is a prefix of `a`.
    pub only_b: usize,
}

/// Compares two event streams.
///
/// **This does not compute a minimal edit distance.** Session logs are append-only, so "common
/// prefix plus two tails" is the correct and cheap description of a fork — see the module docs for
/// why alignment would be both slower and wrong here.
///
/// The cost is one comparison per shared event and nothing else: no allocation, no table, and no
/// work proportional to the differing tails. Comparing a million-event log against a fork that
/// diverged in its last turn is free up to the divergence.
///
/// Two identical streams report `common_prefix == len`, `only_a == only_b == 0`.
#[must_use]
pub fn diff(a: &[SessionEvent], b: &[SessionEvent]) -> Diff {
    let common = common_prefix(a, b);
    Diff {
        common_prefix: common,
        only_a: a.len() - common,
        only_b: b.len() - common,
    }
}

/// The number of leading events two streams share, in order.
///
/// `zip` stops at the shorter stream, which is the right bound: a prefix cannot be longer than the
/// log it is a prefix of.
fn common_prefix(a: &[SessionEvent], b: &[SessionEvent]) -> usize {
    a.iter()
        .zip(b)
        .take_while(|(left, right)| left == right)
        .count()
}

/// The turn in which two logs part ways.
///
/// Returns the turn of the last event the two streams share, which answers "did they differ after
/// turn N?" directly: when that event is the turn's `turn/end`, the logs agree through turn N and
/// differ afterwards; when it is any other event of turn N, they differ inside turn N. Both readings
/// come from one rule, and the alternative — requiring the shared event to be a `turn/end` — would
/// answer `None` for the mid-turn divergence a running session and its fork produce.
///
/// Returns `None` when the shared prefix names no turn at all: two empty logs, two logs that differ
/// at their very first event, or a shared prefix of events that carry no turn (a `session/title` or
/// a `model/selection` before the first `turn/start`). There is no turn to report in each case, and
/// inventing turn 0 would make "differ after turn 0" indistinguishable from "no shared history".
///
/// Identical streams report their final turn — the last point at which they agree — because the
/// rule is about where the shared content ends, not about whether a difference exists. A caller that
/// needs to tell "identical" from "diverged" reads [`Diff`] instead, where
/// `only_a == only_b == 0` is that answer.
#[must_use]
pub fn divergence_turn(a: &[SessionEvent], b: &[SessionEvent]) -> Option<u64> {
    let common = common_prefix(a, b);
    let shared: &[SessionEvent] = a.get(..common)?;
    // Backwards, because the answer is the *last* shared turn: a log can name the same turn several
    // times (a turn/start, its steps, its turn/end), and walking forward would need a final
    // assignment that just repeats this search.
    shared.iter().rev().find_map(SessionEvent::turn)
}

/// Replays events from `from` until a condition says stop.
///
/// `from` is a zero-based index into `events`, so it comes from [`Diff::common_prefix`] without
/// conversion: replaying a forked tail is `replay_until(forked, diff.common_prefix, …)`. A `from`
/// at or past the end yields an empty vector — a session that has written nothing past the fork
/// point has nothing to replay, which is the normal state of a fork that was just created rather
/// than an error.
///
/// `stop_at` is evaluated on each event *after* it is included, so the returned vector is inclusive
/// of the stopping event. That is the useful half: the caller stops at a `turn/end` because it wants
/// a prefix a replay can resume from, and a vector that excluded it would be the mid-turn cut
/// [`crate::session_rewind`] exists to avoid. When `stop_at` never returns true the whole remainder
/// is returned.
///
/// The closure takes a reference and the result is an owned vector, so a replay can look at an event
/// to decide whether to keep going without the decision and the copy fighting over the same value.
#[must_use]
pub fn replay_until(
    events: &[SessionEvent],
    from: usize,
    stop_at: impl Fn(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    let mut replayed = Vec::new();
    // `skip` past the end is empty rather than a panic: a stale cursor is a caller's ordinary
    // situation — the log was compared, then rewound — and this signature has no error channel.
    for event in events.iter().skip(from) {
        replayed.push(event.clone());
        if stop_at(event) {
            break;
        }
    }
    replayed
}

/// What a log looks like without reading its events.
///
/// Cheap enough to compute for every session in a list, which is what lets a caller decide *whether*
/// to compare two logs instead of comparing them to find out. Two snapshots that differ prove the
/// logs differ; two that match do not prove they are equal, so this is a filter, never the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// How many events the log holds.
    pub events: usize,

    /// The turn the log last named, if it has named one.
    ///
    /// This is the last event that carries a turn, which for a log that finished a turn is that
    /// turn's `turn/end` and for a log running mid-turn is the turn in flight. Reporting the
    /// in-flight turn rather than the last *completed* one is deliberate: a caller watching for
    /// progress wants to see a new turn start, not wait for it to close.
    pub last_turn: Option<u64>,

    /// The sequence number of the last event, or zero when the log is empty.
    ///
    /// Equal to [`Snapshot::events`] for every log this crate writes, because sequence numbers are
    /// one-based, positional, and gapless — the invariant [`crate::SessionStore::read_after`]
    /// documents. Both are kept for the same reason [`crate::RewindPoint`] keeps both of its
    /// positions: one is what a cursor-taking reader wants and the other is what a length test
    /// wants, and converting at every call site is how an off-by-one starts.
    ///
    /// A log that ever gains gaps or a sequence offset is the case that breaks the equality, and
    /// this function is the only place that would have to change.
    pub last_seq: u64,
}

/// Summarises a log without copying its events.
///
/// One backward pass for the turn and an O(1) length: no cloning, no allocation, so this is safe to
/// call on every session in a listing.
#[must_use]
pub fn snapshot(events: &[SessionEvent]) -> Snapshot {
    Snapshot {
        events: events.len(),
        last_turn: events.iter().rev().find_map(SessionEvent::turn),
        last_seq: u64::try_from(events.len()).unwrap_or(u64::MAX),
    }
}
