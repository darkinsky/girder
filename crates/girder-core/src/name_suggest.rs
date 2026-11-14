//! Nearest-name lookup for a call that named a tool that does not exist.
//!
//! [`crate::dispatch`] answers an unknown tool with exactly "no tool named `x`" and nothing else.
//! That is an accurate sentence and a useless one: the model typed `terminal_attach` when the tools
//! are `terminal_start`, `terminal_send`, and `terminal_read`, and the error names neither the
//! mistake nor a candidate. A model reading it has two bad moves -- re-send the same name, or guess
//! again in the dark -- and the third, reading the tool list, costs a whole turn. This module turns
//! the dead end into one usable sentence: "no tool named `x`; did you mean `y`?".
//!
//! Nothing calls this yet; wiring it into `crate::dispatch` in `agent_loop` is a separate change.
//!
//! # Why edit distance and not a prefix test
//!
//! The typos that actually happen are not prefixes: `read_fil` for `read_file`, `termnal_read` for
//! `terminal_read`, `reed` for `read`. Each shares a prefix with its target, so a prefix test would
//! get those right and then fail on the ones that matter just as much -- a swapped pair
//! (`raed`), or a name assembled from the right words in the wrong order. Levenshtein covers both
//! shapes with one rule, and the score it produces is also what lets a caller refuse to answer when
//! nothing is close.
//!
//! # Why the computation is capped
//!
//! The unknown name is model output, and model output is not a tool name: it can be a path, a
//! sentence, or a 10,000-character parameter value that landed in the wrong JSON field. The plain
//! dynamic program is `O(n*m)` in those lengths, so one malformed call could stall the loop for
//! minutes. Two things bound it here. A length gap larger than the tolerance is rejected before any
//! table is built, and the table itself is a *band* of width `2 * max_distance + 1` around the
//! diagonal, because any cell outside that band is already over budget and cannot lead back under
//! it. Work is therefore linear in the longer input and never quadratic, and a row whose best cell
//! has exceeded the budget stops the whole computation -- no cell past that row can come back down,
//! since every later edit costs at least zero.
//!
//! # Why characters and not bytes
//!
//! Tool names may be Chinese (`读文件`), and every CJK character is three bytes of UTF-8. Counting
//! bytes would score one wrong character as three edits -- enough to push a real typo past any
//! tolerance -- and it would also invite slicing a `&str` at a non-boundary, which panics. The
//! distance below iterates `char`, so `写文件` is one edit away from `读文件`, which is what a
//! reader would say.
//!
//! # Why no candidate means no suggestion
//!
//! The same rule `error_recovery` states for failures: a wrong suggestion is worse than none,
//! because it sends the model down a path the evidence does not support and the model cannot tell
//! an inferred hint from an observed fact. When every candidate is too far away,
//! [`suggest_tool`] returns `None` and the caller keeps the original, truthful sentence.

/// How far a candidate may be from the unknown name, as a fraction of its length.
///
/// A third is the widest ratio that still rejects a different tool at the same nesting depth:
/// `read_file` and `write_file` are four edits apart over nine characters, which is more than a
/// third. Integer division keeps the threshold a whole number of edits, which is the only unit
/// [`distance`] reports.
const TOLERANCE_DIVISOR: usize = 3;

/// Absolute ceiling on the tolerance, whatever the input length.
///
/// A genuinely mistyped tool name is one or two edits wrong; the ratio alone would let a 3,000
/// character value match something 1,000 edits away, and a "did you mean" that far off is a
/// fabrication. The ceiling also keeps the band narrow, which is what makes a long input cheap.
const MAX_TOLERANCE: usize = 8;

/// One candidate name with its distance from the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The candidate name, exactly as the caller supplied it.
    pub name: String,
    /// Edit distance from the target; smaller is more similar. Never greater than the tolerance
    /// the search used, so every entry here was considered close.
    pub distance: usize,
}

/// Finds the `limit` closest names to `target`.
///
/// Only candidates within the tolerance for `target`'s length are returned, so a short list does
/// not mean a short search. The result is ordered by distance and then by name, so two names at the
/// same distance always come back in the same order -- a suggestion that shuffles between runs
/// reads as randomness to a model comparing two attempts.
///
/// `limit == 0` returns an empty vector without scoring anything.
#[must_use]
pub fn closest(target: &str, candidates: &[String], limit: usize) -> Vec<Candidate> {
    if limit == 0 {
        return Vec::new();
    }
    let tolerance = tolerance(target);
    let mut scored: Vec<Candidate> = candidates
        .iter()
        // Cloned rather than borrowed because `Candidate` owns its name: the caller keeps the
        // registry's list, and a suggestion outlives the borrow it was computed under.
        .map(|name| Candidate {
            name: name.clone(),
            distance: distance(target, name, tolerance),
        })
        // `distance` reports `tolerance + 1` for "further than that", so this one comparison also
        // drops the candidates it rejected early.
        .filter(|candidate| candidate.distance <= tolerance)
        .collect();
    scored.sort_by(|left, right| {
        left.distance
            .cmp(&right.distance)
            .then_with(|| left.name.cmp(&right.name))
    });
    scored.truncate(limit);
    scored
}

/// Builds the sentence a model reads when it named a tool that does not exist.
///
/// The text is ready to return as a tool error: it repeats the name that failed, so the model can
/// see its own typo, and names the single nearest tool. Only the nearest one is offered -- a list
/// invites the model to try each in turn, and the cost of a wrong choice here is a wasted call.
///
/// Returns `None` when no candidate is close enough, and also when the unknown name exactly matches
/// one: an exact name that did not resolve means the lookup failed for some other reason, and
/// advising the model to use the name it already used would be a loop rather than a hint.
#[must_use]
pub fn suggest_tool(unknown: &str, available: &[String]) -> Option<String> {
    let best = closest(unknown, available, 1).into_iter().next()?;
    if best.distance == 0 {
        return None;
    }
    Some(format!(
        "no tool named `{unknown}`; did you mean `{}`?",
        best.name
    ))
}

/// Edit distance between `a` and `b`, computed only as far as `max_distance`.
///
/// Returns `max_distance + 1` as soon as the result is known to exceed `max_distance`. That
/// sentinel is why callers can compare against `max_distance` instead of matching a variant: every
/// value the function returns is either the true distance or strictly greater than the cap.
///
/// The cost is `O(min(len(a), len(b)) * max_distance)` and never `O(len(a) * len(b))` -- see the
/// module documentation for why the difference matters when the inputs come from model output. With
/// `max_distance == 0` the answer is `0` for equal strings and `1` for everything else.
#[must_use]
pub fn distance(a: &str, b: &str, max_distance: usize) -> usize {
    // Saturating rather than adding: a caller may pass `usize::MAX` to mean "no cap", and `+ 1`
    // would wrap to zero and turn every answer into a false match.
    let over = max_distance.saturating_add(1);
    if a == b {
        return 0;
    }
    // Counted before the characters are collected: counting allocates nothing, and the gap check
    // rejects the pathological input (a 10,000-character value against a five-letter tool name)
    // without building a vector of 10,000 `char`s.
    let length_gap = a.chars().count().abs_diff(b.chars().count());
    if length_gap > max_distance {
        return over;
    }

    let left: Vec<char> = a.chars().collect();
    let right: Vec<char> = b.chars().collect();

    // Shared ends cost nothing to align, so removing them shrinks the table without changing the
    // distance. This is what makes `read_file` versus `read_files` a one-row problem instead of a
    // nine-column one.
    let mut start = 0;
    while start < left.len() && start < right.len() && left[start] == right[start] {
        start += 1;
    }
    let mut left_end = left.len();
    let mut right_end = right.len();
    while left_end > start && right_end > start && left[left_end - 1] == right[right_end - 1] {
        left_end -= 1;
        right_end -= 1;
    }

    let left = &left[start..left_end];
    let right = &right[start..right_end];
    if left.is_empty() || right.is_empty() {
        // One side is a prefix of the other; the remaining length is the distance, and the gap
        // check above already proved it fits under the cap.
        return left.len().max(right.len()).min(over);
    }

    banded(left, right, max_distance, over)
}

/// The banded dynamic program behind [`distance`], on already-trimmed characters.
///
/// Rows are indexed by `left`, columns by `right`. Only the diagonal band `|i - j| <= band` is
/// filled: a cell outside it has spent more than `band` edits on deletions and insertions alone, so
/// no alignment through it can finish under the cap, and skipping it is what bounds the work.
fn banded(left: &[char], right: &[char], band: usize, over: usize) -> usize {
    let rows = left.len();
    let columns = right.len();
    let mut previous = vec![over; columns + 1];
    let mut current = vec![over; columns + 1];

    // Row zero is "delete the whole left prefix", so its value is the column index -- but only out
    // to the band; past that a cell is already over the cap. Column zero is "the left prefix is
    // empty", which costs nothing.
    previous[0] = 0;
    for (column, cell) in previous
        .iter_mut()
        .enumerate()
        .take(columns.min(band) + 1)
        .skip(1)
    {
        *cell = column;
    }

    for row in 1..=rows {
        // Columns reachable from this row within the band. An empty band means the row starts past
        // the last column, so the alignment would need more than `band` deletions.
        let first = row.saturating_sub(band).max(1);
        let last = row.saturating_add(band).min(columns);
        if first > last {
            return over;
        }
        // The cell left of the band is read by the insertion candidate. Column zero is a real DP
        // value -- a full deletion of the left prefix -- so it is written as `row`, clamped; any
        // other cell left of the band is off it, already over budget by construction.
        if first == 1 {
            current[0] = row.min(over);
        } else {
            current[first - 1] = over;
        }
        let mut row_best = over;
        for column in first..=last {
            let substitution = previous[column - 1]
                .saturating_add(usize::from(left[row - 1] != right[column - 1]));
            let deletion = previous[column].saturating_add(1);
            let insertion = current[column - 1].saturating_add(1);
            // Clamped to `over` so a row of saturated values cannot drift above the sentinel and
            // lose its meaning as "worse than the cap".
            let value = substitution.min(deletion).min(insertion).min(over);
            current[column] = value;
            row_best = row_best.min(value);
        }
        if last < columns {
            current[last + 1] = over;
        }
        // Every cell of this row is over the cap, and edits only cost more from here, so no
        // alignment through this row can come back under it.
        if row_best > band {
            return over;
        }
        std::mem::swap(&mut previous, &mut current);
    }

    if previous[columns] <= band {
        previous[columns]
    } else {
        over
    }
}

/// The tolerance [`closest`] uses for a target of this length.
///
/// Split out so the ratio and the ceiling are stated once and can be tested directly: a caller
/// asking "would this ever match?" should not have to reproduce the arithmetic.
fn tolerance(target: &str) -> usize {
    (target.chars().count() / TOLERANCE_DIVISOR).clamp(1, MAX_TOLERANCE)
}
