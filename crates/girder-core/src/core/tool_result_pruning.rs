//! Trimming an oversized tool result down to a shape a model can still use.
//!
//! Spilling (see [`crate::spill`]) answers "this result is too large to carry" by saving it and
//! handing the model a locator. That is the right answer when the model can go and read the text
//! itself. It is the wrong answer for a result the model has already asked for and is waiting on:
//! paying a second turn to re-read what arrived in the first is a round trip spent on bookkeeping.
//! This module is the other half — the result stays inline, but only the parts that carry meaning.
//!
//! # Why the tail is kept and not only the head
//!
//! A tool result's informative part is at its *end* more often than at its beginning. A compiler
//! reports `error:` after the file listing, a failing command prints its exit trace last, a test
//! runner's summary is the final line, and a stack trace bottoms out in the root cause. A strategy
//! that keeps only the opening lines turns a failed build into a reassuring header followed by
//! silence — the model sees the command that ran, not the reason it failed, and reports success.
//!
//! The head is kept for the complementary reason: it names *what* the output is about (the file
//! being edited, the command being run, the shape of the response), so the model can tell one
//! result from another. Keeping both ends is the whole judgement of this module; everything else
//! here is bookkeeping around it.
//!
//! # Why the model is told the middle is missing
//!
//! Silence is what makes truncation dangerous. A model that receives a shortened result without a
//! marker has no way to distinguish "the command printed these five lines" from "the command
//! printed these five lines and two thousand more", so it answers with confidence about data it
//! never saw. Every presentation produced here therefore carries a notice that says how much was
//! removed, in the unit the model can reason about: lines.
//!
//! # Why the budget is counted in characters
//!
//! A context window is filled by *characters* — the tokenizer sees text, not UTF-8 bytes — and a
//! byte budget charges a page of Chinese or emoji two to four times what the model actually reads.
//! Counting bytes would prune text that fits, which is a real loss: pruning is not free, it throws
//! away the middle of a result. Slicing is by `char` for the same reason, so a cut never lands
//! inside a UTF-8 sequence and produces replacement characters.
//!
//! # Why lines and not characters are the presentation unit
//!
//! A tool result is line-structured: a build log, a diff, a listing, a trace. Cutting mid-line
//! produces a fragment that reads as corrupt — the model cannot tell whether the line ended there
//! or was severed. Keeping whole lines costs a little precision in the budget and buys a result the
//! model can reason about line by line.

/// What a tool result should look like to the model.
///
/// The variants differ in which part of the text survives, not in how it is measured: every
/// presentation is rendered from the original text by [`apply`], so a caller can store the decision
/// instead of a copy of the result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presentation {
    /// Keep the text exactly as it is.
    ///
    /// The answer for a result inside its budget, and for one so short that pruning could only make
    /// it longer (the notice costs more than it saves).
    Full,
    /// Keep the leading and the trailing lines, and replace the middle with a notice.
    ///
    /// This is the presentation [`decide`] prefers, because it preserves both what the result is
    /// about and the part that usually explains it.
    HeadTail {
        /// How many leading lines survive.
        head_lines: usize,
        /// How many trailing lines survive.
        tail_lines: usize,
    },
    /// Keep only the leading lines, and replace the rest with a notice.
    ///
    /// Available for callers that know their own output is front-loaded — a specification listing,
    /// a set of search matches — where the tail is genuinely less informative than the head.
    /// [`decide`] does not choose this: a function that only sees text cannot know which end
    /// carries the answer, and guessing wrong here hides the error the model was looking for.
    Head {
        /// How many leading lines survive.
        lines: usize,
    },
    /// Replace the entire result with one already-written line.
    ///
    /// The text is model-facing and is returned verbatim, notice included: a summary is prose about
    /// the result, so the code that wrote it is the only code that knows what it says. [`decide`]
    /// builds this variant for output that line-based pruning cannot help — a single enormous line,
    /// or lines so wide that keeping whole ones would blow the budget anyway.
    Summary(String),
}

/// Share of the character budget given to the head of a pruned result.
///
/// Half the budget goes to the head because the head is where the result identifies itself and
/// where a diff or listing starts. The remaining half is not the tail's to take: some of the budget
/// is deliberately left unspent so that whole lines can be kept at both ends instead of every byte
/// being contested by the width of one long line.
const HEAD_DIVISOR: usize = 2;

/// Share of the character budget given to the tail of a pruned result.
///
/// A third, not a sliver. The tail holds the diagnostic — a build's `error[...]` line, a trace's
/// root cause, a test runner's failure summary — and that diagnostic is several lines, not one. A
/// smaller share can afford only the final line, which is the least informative part of the block:
/// the explanation of *why* it failed sits above it.
const TAIL_DIVISOR: usize = 3;

/// Fewest trailing lines a pruned result keeps, when the text has that many to give.
///
/// A proportion is the wrong unit for a diagnostic. `error[E0308]` names the error, the next line
/// locates it, and the third states the mismatch; a stack trace's root cause is followed by the
/// frames above it. Keeping a fixed minimum count is what makes the tail's guarantee structural
/// rather than a side effect of how wide the last line happened to be. [`decide`] still refuses the
/// cut if these lines do not fit the budget, so the floor cannot cause overflow.
const MIN_TAIL_LINES: usize = 3;

/// Share of the budget a [`Presentation::Summary`] body may use, in quarters.
///
/// The rest is left for the notice, so a summary does not overflow the budget that caused it to be
/// written.
const SUMMARY_BUDGET_NUMERATOR: usize = 3;

/// Decides how a tool result should be presented to the model.
///
/// `limit_chars` is the number of characters the result may keep: it is a decision threshold, not a
/// hard cap on the rendered text, because the notice that reports the cut is added on top. Keeping
/// the notice outside the budget is deliberate — a result that fits the budget by being silently
/// shortened is worse than one that is a few dozen characters over and says so.
///
/// The returned presentation is guaranteed to render to fewer characters than `content`; when it
/// would not, [`Presentation::Full`] is returned. Pruning that makes a result bigger is not pruning.
///
/// # Choosing the presentation
///
/// - Text within `limit_chars` is kept whole.
/// - Multi-line text outside it gets [`Presentation::HeadTail`], with whole lines fitted into the
///   head and tail shares of the budget.
/// - Text that is a single line, or whose lines are too wide to fit whole, gets a
///   [`Presentation::Summary`]: there is no line boundary to cut on, so the only honest presentation
///   is a flattened excerpt plus the notice.
///
/// [`Presentation::Head`] is never chosen here; see its documentation for why.
#[must_use]
pub fn decide(content: &str, limit_chars: usize) -> Presentation {
    let total_chars = content.chars().count();
    if total_chars <= limit_chars {
        return Presentation::Full;
    }

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    if total_lines > 1 {
        let head_lines = fit_head(&lines, limit_chars / HEAD_DIVISOR);
        let tail_lines = fit_tail(&lines, limit_chars / TAIL_DIVISOR);
        // Both ends must fit inside the text with something left between them. When they meet in
        // the middle there is no middle to prune, and the overlap is what tells us the lines are too
        // wide for a line-based cut to help.
        if head_lines.saturating_add(tail_lines) < total_lines {
            let retained = retained_chars(&lines, head_lines, tail_lines);
            let notice = middle_notice(total_lines - head_lines - tail_lines, total_lines);
            let rendered = retained.saturating_add(notice.chars().count());
            if retained <= limit_chars && rendered < total_chars {
                return Presentation::HeadTail {
                    head_lines,
                    tail_lines,
                };
            }
        }
    }

    let summary = summarize(content, total_chars, total_lines, limit_chars);
    if summary.chars().count() < total_chars {
        Presentation::Summary(summary)
    } else {
        Presentation::Full
    }
}

/// Renders a presentation against the text it was decided for.
///
/// The text is passed again rather than carried in the presentation so a caller can log or store a
/// decision — "this result was pruned to three head and two tail lines" — without holding the
/// result itself.
///
/// A [`Presentation::HeadTail`] or [`Presentation::Head`] that would remove nothing returns the
/// text unchanged instead of a notice claiming zero lines were dropped: a marker that reports a cut
/// which did not happen teaches the model to distrust the markers that report real ones.
#[must_use]
pub fn apply(content: &str, presentation: &Presentation) -> String {
    match presentation {
        Presentation::Full => content.to_owned(),
        Presentation::Summary(summary) => summary.clone(),
        Presentation::Head { lines } => render(content, *lines, 0, head_notice),
        Presentation::HeadTail {
            head_lines,
            tail_lines,
        } => render(content, *head_lines, *tail_lines, middle_notice),
    }
}

/// Renders a head/tail cut, inserting the notice produced by `notice`.
///
/// The notice is a function rather than a string so each presentation can word the cut the way it
/// actually happened: "from the middle" and "only the beginning" describe different losses, and the
/// model should not have to infer which one it is looking at.
fn render(
    content: &str,
    head_lines: usize,
    tail_lines: usize,
    notice: fn(usize, usize) -> String,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let head = head_lines.min(total);
    let tail = tail_lines.min(total.saturating_sub(head));
    if head.saturating_add(tail) >= total {
        return content.to_owned();
    }

    let pruned = total - head - tail;
    let mut out = String::new();
    if head > 0 {
        out.push_str(&lines[..head].join("\n"));
        out.push('\n');
    }
    out.push_str(&notice(pruned, total));
    if tail > 0 {
        out.push('\n');
        out.push_str(&lines[total - tail..].join("\n"));
    }
    out
}

/// How many leading lines fit inside `budget` characters.
///
/// The first line is always kept, even when it alone exceeds the budget: an empty head would drop
/// the only clue to what the output is, and the caller still gets a smaller result than it started
/// with. [`decide`] checks the rendered size afterwards and falls back to a summary in that case.
fn fit_head(lines: &[&str], budget: usize) -> usize {
    let mut chars = 0usize;
    let mut kept = 0usize;
    for line in lines {
        let width = line.chars().count();
        if kept > 0 && chars.saturating_add(width) > budget {
            break;
        }
        chars = chars.saturating_add(width);
        kept += 1;
    }
    kept
}

/// How many trailing lines fit inside `budget` characters.
///
/// Mirrors [`fit_head`] from the other end for the same reason: the tail is the part that explains
/// the result, so it is worth one line even when that line is wide. It also honours
/// [`MIN_TAIL_LINES`] when the tail has that many lines to give — the caller checks the rendered
/// size afterwards, so the floor can only make the cut more expensive, never silently invalid.
fn fit_tail(lines: &[&str], budget: usize) -> usize {
    let mut chars = 0usize;
    let mut kept = 0usize;
    for line in lines.iter().rev() {
        let width = line.chars().count();
        if kept >= MIN_TAIL_LINES && chars.saturating_add(width) > budget {
            break;
        }
        chars = chars.saturating_add(width);
        kept += 1;
    }
    kept
}

/// Characters retained by a head/tail cut, excluding the notice.
fn retained_chars(lines: &[&str], head_lines: usize, tail_lines: usize) -> usize {
    let head = head_lines.min(lines.len());
    let tail = tail_lines.min(lines.len().saturating_sub(head));
    let mut chars = 0usize;
    for line in &lines[..head] {
        chars = chars.saturating_add(line.chars().count());
    }
    for line in &lines[lines.len() - tail..] {
        chars = chars.saturating_add(line.chars().count());
    }
    chars
}

/// Builds the one-line summary used when line-based cutting cannot shrink the result.
///
/// Newlines are flattened and runs of whitespace collapsed so the result really is one line: the
/// point of a summary is to be scannable, and a summary that wrapped would just be a shorter copy
/// of the text it replaced. The cut is by `char`, so a multi-byte character is never split.
fn summarize(content: &str, total_chars: usize, total_lines: usize, limit_chars: usize) -> String {
    let budget = limit_chars.saturating_mul(SUMMARY_BUDGET_NUMERATOR) / 4;
    let excerpt: String = content.chars().take(budget).collect();
    let body = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
    let kept = body.chars().count();
    let pruned_chars = total_chars.saturating_sub(kept);
    // At most one line survives the flattening, so every other line was dropped.
    let pruned_lines = total_lines.saturating_sub(1);
    let notice = summary_notice(pruned_lines, total_lines, pruned_chars, total_chars);
    if body.is_empty() {
        return notice;
    }
    format!("{body} {notice}")
}

/// The notice standing in for a middle that was cut out.
///
/// It names both numbers — how much was dropped and how much there was — because "34 lines pruned"
/// alone does not tell the model whether it is missing a third of the result or all of it.
fn middle_notice(pruned_lines: usize, total_lines: usize) -> String {
    format!(
        "[... {pruned_lines} of {total_lines} lines pruned from the middle; the beginning and the end are kept ...]"
    )
}

/// The notice standing in for everything after the head.
fn head_notice(pruned_lines: usize, total_lines: usize) -> String {
    format!("[... {pruned_lines} of {total_lines} lines pruned; only the beginning is shown ...]")
}

/// The notice appended to a one-line summary.
///
/// Both counts are reported: a single line of a million characters has pruned no lines at all, and
/// a reader told only about lines would conclude nothing was removed.
fn summary_notice(
    pruned_lines: usize,
    total_lines: usize,
    pruned_chars: usize,
    total_chars: usize,
) -> String {
    format!(
        "[... {pruned_lines} of {total_lines} lines and {pruned_chars} of {total_chars} characters pruned ...]"
    )
}
