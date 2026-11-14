//! What a compacted span must never lose: a deterministic summary of the span being dropped.
//!
//! [`compaction`](crate::compaction) decides *which* span may be cut and
//! [`compaction_hook`](crate::compaction_hook) decides *whether* to cut. Neither says what the model
//! should see in place of the dropped messages. The model-facing checkpoint is assembled
//! there from a model-written paragraph ([`COMPACTION_INSTRUCTION`](crate::COMPACTION_INSTRUCTION),
//! [`checkpoint_message`](crate::checkpoint_message)), and that paragraph is the one part of the
//! pipeline whose output cannot be predicted, tested, or replayed.
//!
//! This module is the deterministic floor underneath it: a summary the harness can produce with no
//! provider, no prompt, and no tokenizer. It always succeeds, it costs nothing, and — crucially — it
//! is the only layer that decides what is *not allowed* to be summarised away.
//!
//! # Why the user's requests are carried verbatim
//!
//! A summary is a lossy function, and every lossy function applied to a requirement is a chance to
//! drop the part that mattered. "Rename the flag, but keep the old spelling working for one release
//! and do not touch the tests" summarises cleanly to "rename the flag", and the model then does the
//! wrong, smaller job with full confidence. The user's own words are the highest-priority information
//! in the context, so they are copied, not paraphrased. This is also why there is no length cap here:
//! a cap would be a silent paraphrase at the boundary, which is the failure mode this module exists
//! to prevent.
//!
//! # Why paths are kept whole
//!
//! A truncated path is worse than no path: it reads as a fact, resolves to nothing, and sends the
//! model to a file that does not exist. Paths are therefore stored exactly as the tool received them
//! — not canonicalised, not shortened, not made relative — since this layer has no workspace root to
//! canonicalise against and no right to guess which spelling the caller will need.
//!
//! # Why no model is called
//!
//! Two reasons. The practical one: this runs on the failure path too, where the provider that would
//! write a paragraph is exactly the thing that is unavailable. The structural one: everything else in
//! the compaction pipeline is deterministic and therefore testable, and adding a model call here would
//! put the one non-deterministic step inside the module whose job is to be the fallback for it.
//!
//! What this module deliberately does *not* extract: what the assistant concluded, why an attempt
//! failed, or what is left to do. Those need judgement, so they belong to the model-written
//! checkpoint. The split is "facts nobody may lose" here, "understanding" there.

use std::fmt::Write as _;
use std::ops::Range;

use girder_core::{ChatMessage, ChatRole, ContentBlock, SUMMARY_CLOSE_TAG, SUMMARY_OPEN_TAG};

/// The tool-argument keys that name a path in this workspace.
///
/// Taken from the tool schemas rather than guessed: `read`/`write`/`edit`/`glob`/`grep`/`lsp`/
/// `present`/`delete_path`/`make_dir` all use `path`, and `copy_path`/`move_path` use `source` and
/// `destination`. A key outside this list is ignored on purpose — `write` takes a `content` field
/// that routinely holds source code, and treating any string value as a path would fill the summary
/// with garbage.
const PATH_ARGUMENTS: [&str; 3] = ["path", "source", "destination"];

/// Markers that carry a path inside an `apply_patch` document.
///
/// The patch tool takes one `patch` string rather than a path argument, so the paths are only
/// visible by reading the document. Matching the parser's own markers keeps the two in step: if a
/// marker changes there, it changes here.
const PATCH_MARKERS: [&str; 4] = [
    "*** Add File: ",
    "*** Delete File: ",
    "*** Update File: ",
    "*** Move to: ",
];

/// A span of history that was compacted.
///
/// The fields are exactly the facts a reader cannot reconstruct from what survives: which turns are
/// gone, what was asked, which tools ran, which files were touched, and how much work it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Which turns the span covers, half-open: `1..4` is turns 1, 2, and 3.
    ///
    /// Turn numbers are derived here rather than read from the log, because a [`ChatMessage`] carries
    /// no turn of its own. The rule is the one the loop already follows: a user message opens a turn,
    /// so a span of `n` requests reports `1..n + 1`. The numbering is therefore **relative to the
    /// span**, not absolute: a span lifted from the middle of a long conversation numbers its first
    /// turn 1. That is the honest choice — an absolute number cannot be recovered from messages, and
    /// a guessed one would tell the reader the conversation is shorter than it is.
    ///
    /// A span with no user message has no attributable turn and reports `0..0` rather than inventing
    /// a number; the span still may hold steps, and [`render`] says so.
    pub turns: Range<u64>,
    /// What the user asked for, verbatim and in order.
    ///
    /// Kept as they were written, never summarised and never truncated; see the module documentation
    /// for why this is the one field whose fidelity outranks the token budget.
    pub requests: Vec<String>,
    /// Which tools ran, deduplicated in order of first use.
    pub tools: Vec<String>,
    /// Which files were touched, deduplicated in order of first use, paths kept whole.
    pub files: Vec<String>,
    /// How many model responses the span contained.
    ///
    /// One response is one step, because a step is what a single model call produces. This is a
    /// count of assistant messages rather than of `StepStart` events: the input here is the message
    /// list, not the event log, and the message list is what the caller actually holds.
    pub steps: u32,
}

/// Builds the summary of a span from the messages it is replacing.
///
/// Total: every input, including an empty one, produces a [`Segment`]. There is no error variant
/// because there is no failure to report — unparseable tool arguments and absent fields simply
/// contribute nothing, which is the correct reading of "nothing was recorded".
///
/// A prior checkpoint is not treated as a request. It is a machine-written paragraph that re-states
/// the span before it, so copying it verbatim would both duplicate the previous summary and pad the
/// new one with its own preamble. Its *content* is not lost: checkpoints are merged by the caller,
/// which is the only layer that can tell a stale fact from a current one.
#[must_use]
pub fn summarize(messages: &[ChatMessage]) -> Segment {
    let mut requests: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut turns = 0_u64;
    let mut steps = 0_u32;

    for message in messages {
        if message.role == ChatRole::Assistant {
            steps = steps.saturating_add(1);
        }

        if let Some(request) = request_text(message) {
            turns = turns.saturating_add(1);
            push_unique(&mut requests, &request);
        }

        for block in &message.content {
            let ContentBlock::ToolCall {
                name, arguments, ..
            } = block
            else {
                continue;
            };
            push_unique(&mut tools, name);
            collect_paths(arguments, &mut files);
        }
    }

    Segment {
        turns: if turns == 0 { 0..0 } else { 1..turns + 1 },
        requests,
        tools,
        files,
        steps,
    }
}

/// Renders a segment as the text that stands in for the compacted span.
///
/// The shape is a compact block rather than prose: every line is a label plus content, so a reader
/// can find one fact without reading the rest, and no sentence is spent on transitions. Requests and
/// files keep their own lines because those are the two things that must never be silently merged:
/// joining requests into a paragraph invites the reader to treat them as one, and joining paths with
/// a separator makes them impossible to copy whole.
///
/// An empty segment renders as the empty string, so a caller can skip inserting a block that says
/// nothing. Every other case renders at least the header, because a span that was compacted but held
/// no request is itself information: it says the model was working unattended.
#[must_use]
pub fn render(segment: &Segment) -> String {
    if is_empty(segment) {
        return String::new();
    }

    let mut out = String::new();
    if segment.turns.start < segment.turns.end {
        // The half-open form is printed as it is stored. Restating it as "1-3" would add an
        // off-by-one for a reader who already knows Rust ranges, and a wrong turn number in a
        // checkpoint misleads the model about how much history is behind it.
        let _ = writeln!(
            out,
            "compacted span: turns {}..{}",
            segment.turns.start, segment.turns.end
        );
    } else {
        out.push_str("compacted span: no turn boundary\n");
    }

    if segment.requests.is_empty() {
        out.push_str("requests: (none)\n");
    } else {
        out.push_str("requests (verbatim, in order):\n");
        for request in &segment.requests {
            let _ = writeln!(out, "- {request}");
        }
    }

    if !segment.tools.is_empty() {
        let _ = writeln!(out, "tools: {}", segment.tools.join(", "));
    }

    if !segment.files.is_empty() {
        out.push_str("files touched (full paths):\n");
        for file in &segment.files {
            let _ = writeln!(out, "- {file}");
        }
    }

    let _ = writeln!(out, "steps: {}", segment.steps);
    out.trim_end().to_owned()
}

/// Whether a segment records nothing at all.
///
/// Not part of the public surface: a caller that needs the check can compare [`render`] against the
/// empty string, and adding an `is_empty` would suggest an empty segment is a state worth branching
/// on rather than one worth rendering as nothing.
fn is_empty(segment: &Segment) -> bool {
    segment.turns.start == segment.turns.end
        && segment.requests.is_empty()
        && segment.tools.is_empty()
        && segment.files.is_empty()
        && segment.steps == 0
}

/// The text a message contributes as a user request, if it contributes one.
///
/// Role is not sufficient on its own: a checkpoint also rides the user role (that is what makes the
/// model read it as standing context rather than as its own words), and it must not be re-summarised
/// as a requirement. Blank text is dropped because it would render as an empty bullet.
fn request_text(message: &ChatMessage) -> Option<String> {
    if message.role != ChatRole::User {
        return None;
    }

    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    if text.trim().is_empty() || is_checkpoint_text(&text) {
        return None;
    }

    Some(text)
}

/// Whether text is a machine-written checkpoint rather than something the person said.
///
/// Both tags are required, for the same reason [`is_checkpoint`](crate::is_checkpoint) requires
/// both: a message that merely mentions one tag is not a checkpoint, and treating it as one would
/// silently drop a real request. That function takes a message and this one takes text, so the
/// decision is duplicated here rather than routed through a conversion the summary does not need.
fn is_checkpoint_text(text: &str) -> bool {
    text.contains(SUMMARY_OPEN_TAG) && text.contains(SUMMARY_CLOSE_TAG)
}

/// Adds `candidate` to `values` unless it is blank or already present.
///
/// A linear scan rather than a set: a span holds a handful of requests and paths, so the quadratic
/// worst case is smaller than the allocation a hash set would cost, and preserving first-use order
/// requires an ordered structure anyway.
fn push_unique(values: &mut Vec<String>, candidate: &str) {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return;
    }
    if values.iter().any(|existing| existing == trimmed) {
        return;
    }
    values.push(trimmed.to_owned());
}

/// Extracts every path named by a tool call's raw JSON arguments.
///
/// Arguments that do not parse are skipped rather than surfaced as an error: a model can emit
/// malformed JSON, and a compaction must still describe the rest of the span. A malformed patch
/// document is treated the same way, line by line, so one bad hunk does not hide the paths above it.
fn collect_paths(arguments: &str, files: &mut Vec<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return;
    };

    for key in PATH_ARGUMENTS {
        if let Some(path) = value.get(key).and_then(serde_json::Value::as_str) {
            push_unique(files, path);
        }
    }

    // `apply_patch` carries its paths inside the document instead of in an argument, so a span that
    // edited twelve files through patches would otherwise report zero files touched.
    if let Some(patch) = value.get("patch").and_then(serde_json::Value::as_str) {
        for line in patch.lines() {
            for marker in PATCH_MARKERS {
                if let Some(path) = line.trim_end().strip_prefix(marker) {
                    push_unique(files, path);
                }
            }
        }
    }
}
