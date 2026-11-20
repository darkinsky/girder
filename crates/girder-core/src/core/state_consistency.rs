//! Cross-checks between the derived fields of one `session.state` answer.
//!
//! A `session.state` answer carries many numbers that describe the same log from different angles:
//! how many events it holds, the last sequence number, how many turns ran, how many timeline rows
//! there are, how many tool calls happened. Each comes from its own fold — [`crate::SessionStats`],
//! [`crate::session_outline`], [`crate::digest_session`], [`crate::core::tool_usage`] — and nothing has
//! ever compared the results.
//!
//! That is the gap this module fills. A fold that miscounts is invisible from the inside: its output
//! is a plausible number, and a client rendering "3 turns" beside a digest that says "4 turns" has no
//! way to know which one is wrong. Only a *cross*-check catches it, because the fields are supposed
//! to be redundant — two independent folds over one log answering the same question must agree, and
//! when they do not, at least one of them is broken.
//!
//! # Why the check takes numbers rather than an event log
//!
//! Because the question is not "is the log well formed" but "does this answer contradict itself",
//! and an answer is what a client receives. Taking [`Values`] rather than `&[SessionEvent]` keeps the
//! check a pure function of the payload, lets it run against a snapshot captured in a bug report
//! without the events, and means it can be asserted directly in a test rather than through a
//! registered Remote method.
//!
//! # Why sequence numbers are expected to equal the count
//!
//! Sequence numbers are one-based: the event at index `i` has sequence `i + 1`. So a log of `n`
//! events ends at sequence `n`, and an empty log reports `0` for both. `lastSeq` is therefore not an
//! independent measurement that might drift — it is the same number, and any difference is a bug in
//! whichever of the two was computed by walking the log.
//!
//! # Why two severities instead of one
//!
//! Some differences mean one field is simply wrong ([`Severity::Contradiction`]) and some mean two
//! fields are counting different sets, each correctly ([`Severity::Cosmetic`]). Collapsing them would
//! force a caller to choose between reporting noise and hiding a real bug; with the distinction, a
//! CI check can fail on contradictions alone while a diagnostic listing still shows the rest.

/// The wire name of the event count, used in [`Inconsistency::between`].
///
/// The names are the ones the payload carries rather than the Rust field names, because a report is
/// read beside the JSON it describes: telling a reader about `event_count` when the document says
/// `events` sends them looking for a field that is not there.
const EVENTS: &str = "events";

/// The wire name of the last sequence number.
const LAST_SEQ: &str = "lastSeq";

/// The wire name of the stats fold's turn count.
const TURNS: &str = "counts.turns";

/// The wire name of the digest's turn count.
const DIGEST_TURNS: &str = "digest.turns";

/// The wire name of the stats fold's answered tool-call count.
const TOOL_CALLS: &str = "counts.toolCalls";

/// The wire name of the per-tool fold.
const TOOL_USAGE: &str = "toolUsage";

/// The wire name of the timeline.
const TIMELINE: &str = "timeline";

/// The wire name of the open turn marker.
///
/// Named here so the messages that mention it name the field a client reads, rather than describing it in prose the client
/// has to translate back.
const OPEN_TURN: &str = "openTurn";

/// How badly two fields disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The two fields are different views of the same fact and both are right.
    ///
    /// An unfinished turn, for instance, has a timeline row in some snapshots and not in others
    /// depending on how far the turn got, so the timeline may legitimately carry one row more than
    /// the count of closed turns. Worth showing to someone reading a diagnostic, not worth failing.
    Cosmetic,
    /// One field says what the other denies.
    ///
    /// Both cannot be true, so at least one fold is wrong — or the two were read at different
    /// moments, which for a snapshot built from a single read should not happen either.
    Contradiction,
}

/// One place where two derived fields disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inconsistency {
    /// The two fields that disagree, as the payload names them.
    ///
    /// A pair rather than one name because a disagreement has two sides, and reporting only one
    /// would leave the reader guessing which field to compare it against.
    pub between: (String, String),
    /// What the two fields hold, and why they were expected to agree.
    ///
    /// Carries the numbers rather than only the field names: a report that says "these disagree"
    /// still makes the reader open the payload to find out by how much.
    pub detail: String,
    /// Whether this is a contradiction or a difference of definition.
    pub severity: Severity,
}

/// The derived values of one snapshot, reduced to the numbers that must agree.
///
/// Named after the fields they are taken from so that a caller filling this in reads the payload
/// once and is done. [`Values::default`] is the consistent empty log — every counter zero — which is
/// what a test starts from when it wants to change one number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Values {
    /// How many durable events the log holds.
    pub event_count: usize,
    /// The sequence number of the last event; `0` when there are none.
    pub last_seq: u64,
    /// Turns [`crate::SessionStats`] counted, which counts a turn that closed a step.
    pub turns: u64,
    /// Rows in the timeline.
    pub timeline_segments: usize,
    /// The turn that started and has not ended, when the log ends inside one.
    pub open_turn: Option<u64>,
    /// Turns [`crate::digest_session`] counted, which counts `turn/start` numbers.
    pub digest_turns: u64,
    /// Tool calls [`crate::SessionStats`] counted, which counts calls that got a result.
    pub tool_calls: u64,
    /// Every call counted per tool by [`crate::core::tool_usage`], summed over the tools.
    pub tool_usage_total: u32,
}

/// Reports every way the values in `values` disagree with each other.
///
/// Returns an empty vector when the snapshot is internally consistent, which is the ordinary case:
/// the checks are for a fold that broke, not for a state a healthy server reaches. Entries come back
/// in a fixed order — sequence, turns, tools, timeline, emptiness — so two runs on one snapshot
/// produce the same list and a caller can compare them.
///
/// # The rules
///
/// 1. `events` equals `lastSeq`: sequence numbers are one-based, so the last of `n` events is `n`.
/// 2. `counts.turns` equals `digest.turns`: both count turns in the same log.
/// 3. `counts.toolCalls` equals the sum of `toolUsage`, both counting tool calls.
/// 4. `timeline` has one row per turn, with one row of slack while a turn is open.
/// 5. A log with no events counts nothing.
///
/// Rule 2 is the one whose expectation the two folds do not quite share, and the difference is worth
/// stating because it is what the check reports first on a log that is otherwise fine.
/// [`crate::SessionStats`] counts a turn that *closed a step*, so a turn rejected before it ran is not
/// counted; [`crate::digest_session`] counts distinct `turn/start` numbers, so that same turn is. The
/// two therefore disagree by the number of turns that opened and never stepped — and, because the
/// digest falls back to the request count when a log has no boundaries at all, by the requests of a
/// log that has prompts and no `turn/start`. The check still compares them for equality: a difference
/// of one caused by an abandoned turn and a difference of one caused by a fold dropping a turn are
/// the same number, and silently accepting either would accept both. A caller that knows the log's
/// shape can discount the first case, which is why the pair is named in [`Inconsistency::between`]
/// rather than folded into one aggregate verdict.
///
/// Rule 3 has a caveat the check cannot resolve on its own: `counts.toolCalls` counts calls that
/// received a result, while `toolUsage` counts every call, so a log that ends mid-call makes them
/// differ by the number of unanswered calls. The difference is still reported, because from these
/// two numbers alone a reader cannot tell that case from one of the folds miscounting, and a check
/// that silently excused it would excuse real bugs of exactly the same size. A caller holding
/// [`crate::SessionStats::unanswered_tool_calls`] can discount a difference equal to it.
///
/// Rule 4 is stated with slack because the two folds count different things: a timeline row is
/// created by a `turn/start`, and a counted turn is one that closed a step. A turn that has started
/// and not finished may or may not have produced a step yet, so the timeline can be one row ahead of
/// the turn count or one behind. That is reported as [`Severity::Cosmetic`]; a wider gap is not
/// explained by the open turn and is a [`Severity::Contradiction`].
#[must_use]
pub fn check(values: &Values) -> Vec<Inconsistency> {
    let mut found = Vec::new();
    check_sequence(values, &mut found);
    check_turns(values, &mut found);
    check_tools(values, &mut found);
    check_timeline(values, &mut found);
    check_empty_log(values, &mut found);
    found
}

/// Appends one report, converting the field-name pair to the owned form the struct stores.
fn report(
    found: &mut Vec<Inconsistency>,
    between: (&str, &str),
    severity: Severity,
    detail: String,
) {
    found.push(Inconsistency {
        between: (between.0.to_owned(), between.1.to_owned()),
        detail,
        severity,
    });
}

/// Compares the event count with the last sequence number.
fn check_sequence(values: &Values, found: &mut Vec<Inconsistency>) {
    // Saturating rather than truncating: an `usize` wider than a `u64` cannot occur on a 64-bit
    // target and a saturating count is still the honest answer if it ever did.
    let count = u64::try_from(values.event_count).unwrap_or(u64::MAX);
    if count == values.last_seq {
        return;
    }
    report(
        found,
        (EVENTS, LAST_SEQ),
        Severity::Contradiction,
        format!(
            "`events` is {count} but `lastSeq` is {}, and sequence numbers are one-based, so the \
             last of {count} events is {count} itself (0 for an empty log)",
            values.last_seq
        ),
    );
}

/// Compares the two turn counts.
fn check_turns(values: &Values, found: &mut Vec<Inconsistency>) {
    if values.turns == values.digest_turns {
        return;
    }
    report(
        found,
        (TURNS, DIGEST_TURNS),
        Severity::Contradiction,
        format!(
            "`counts.turns` is {} while `digest.turns` is {}, and both count the turns of the same \
             log; one of the two folds is wrong",
            values.turns, values.digest_turns
        ),
    );
}
/// Compares the two tool-call counts.
fn check_tools(values: &Values, found: &mut Vec<Inconsistency>) {
    let usage_total = u64::from(values.tool_usage_total);
    if values.tool_calls == usage_total {
        return;
    }
    report(
        found,
        (TOOL_CALLS, TOOL_USAGE),
        Severity::Contradiction,
        format!(
            "`counts.toolCalls` is {} while `toolUsage` sums to {usage_total} calls; the two folds \
             count the same events, so a difference means one of them missed or double-counted a \
             call (a log ending mid-call explains a difference of exactly its unanswered calls)",
            values.tool_calls
        ),
    );
}

/// Compares the timeline with the turn count, allowing for one row of slack around an open turn.
fn check_timeline(values: &Values, found: &mut Vec<Inconsistency>) {
    let segments = u64::try_from(values.timeline_segments).unwrap_or(u64::MAX);
    let turns = values.turns;
    if segments == turns {
        return;
    }
    let Some(turn) = values.open_turn else {
        debug_assert!(!OPEN_TURN.is_empty());
        // No turn is open, so every row belongs to a turn that started and ended; the two counts
        // are over the same set and have no reason to differ.
        report(
            found,
            (TIMELINE, TURNS),
            Severity::Contradiction,
            format!(
                "`timeline` has {segments} rows for {turns} turns and no turn is open, so each row \
                 is one turn and the counts should match"
            ),
        );
        return;
    };
    if segments.abs_diff(turns) != 1 {
        // The open turn can move the two counts apart by one. Two or more is something else: a
        // dropped row, or a turn counted twice, and the open turn does not explain it.
        report(
            found,
            (TIMELINE, TURNS),
            Severity::Contradiction,
            format!(
                "`timeline` has {segments} rows for {turns} turns: turn {turn} is open, which \
                 allows a difference of one, not {}",
                segments.abs_diff(turns)
            ),
        );
        return;
    }
    let detail = if segments > turns {
        format!(
            "turn {turn} is open and already has a `timeline` row, so the timeline has {segments} \
             rows for {turns} finished turns; both counts are right about what they cover"
        )
    } else {
        format!(
            "turn {turn} is open and counted by `counts.turns` but has no `timeline` row yet, so \
             there are {segments} rows for {turns} turns; both counts are right about what they \
             cover"
        )
    };
    report(found, (TIMELINE, TURNS), Severity::Cosmetic, detail);
}

/// Rejects counters that are positive on a log with no events at all.
fn check_empty_log(values: &Values, found: &mut Vec<Inconsistency>) {
    if values.event_count > 0 {
        return;
    }
    // Zero events is a normal state, and a stat fold that reports work in it has counted something
    // that cannot be in the log. This is not a tolerance question: there is no event to count.
    let counters: [(&str, u64); 3] = [
        (TURNS, values.turns),
        (DIGEST_TURNS, values.digest_turns),
        (TOOL_CALLS, values.tool_calls),
    ];
    for (name, value) in counters {
        if value == 0 {
            continue;
        }
        report(
            found,
            (EVENTS, name),
            Severity::Contradiction,
            format!(
                "`events` is 0 but `{name}` is {value}: a log with no events holds nothing for any \
                 counter to count"
            ),
        );
    }
    if values.tool_usage_total > 0 {
        report(
            found,
            (EVENTS, TOOL_USAGE),
            Severity::Contradiction,
            format!(
                "`events` is 0 but `toolUsage` sums to {} calls, and a log with no events records \
                 no call to count",
                values.tool_usage_total
            ),
        );
    }
    if values.timeline_segments > 0 {
        report(
            found,
            (EVENTS, TIMELINE),
            Severity::Contradiction,
            format!(
                "`events` is 0 but `timeline` has {} rows; every row comes from a turn boundary in \
                 a log that has none",
                values.timeline_segments
            ),
        );
    }
}
