//! A one-glance digest of a whole session: what was asked, what came back, whether it ended.
//!
//! Three modules already read a session log, and none of them answers "what is the conclusion of this
//! conversation":
//!
//! - [`crate::session_title`] derives *one line about where the session started* — the first prompt,
//!   cleaned and clipped. It is stable by design, so it cannot change when the session's subject
//!   changes halfway through, and it never reads an answer.
//! - [`crate::session_timeline`] derives *one row per turn* — the turn's first prompt plus the tools it
//!   used. It describes each turn in isolation and deliberately avoids assistant text, so it never
//!   assembles a conclusion for the session as a whole. It returns `Vec<Segment>`: a row per turn means
//!   reading fifty rows to understand a fifty-turn session.
//! - [`crate::compaction_summary`] derives *the facts a compacted span may not lose* — requests
//!   verbatim, tools, paths — for the model to consume, not for a person to read. It folds
//!   [`ChatMessage`](crate::ChatMessage) values, not events, so it cannot see `turn/end` and cannot say
//!   whether the session finished.
//!
//! The gap is the one a returning user hits: the log holds every request, every tool call, and every
//! reply, and the three modules above each preserve one slice of it — a start, a list of per-turn rows,
//! or a verbatim fact sheet. None produces a single small value that says *where the conversation
//! ended up*. Fifty turns of history therefore cost fifty turns of reading to answer "did this get
//! done, and how did it end".
//!
//! [`digest`] is that value: one [`Digest`] per session, with the request budgeted rather than
//! exhaustive and the final answer promoted to a first-class field.
//!
//! # Why `last_answer` is the centre of this design
//!
//! "What is the conclusion" has no exact answer in a log — a conclusion is a judgement, and this module
//! makes no model call. The closest thing the log actually contains is the last thing the assistant
//! said: it is where the model states what it found, what it changed, and what is left. So it is
//! stored as its own field rather than being left to a caller to hunt for by walking backwards through
//! `assistant/message` records.
//!
//! Two consequences are deliberate:
//!
//! - An assistant message with no text — a pure tool-call step, which is the common shape mid-turn —
//!   does **not** clear the field. The last *spoken* answer is what a reader wants, and a trailing
//!   tool call is not a newer conclusion.
//! - The text is bounded ([`LAST_ANSWER_MAX_CHARS`]). A reply can be a pasted file or a forty-line
//!   report; an unbounded field would make every `Digest` as expensive to hold as the transcript it
//!   summarises, and [`one_line`] would clip it anyway. The bound is a pointer to the answer, not the
//!   answer: the log is still there for a caller that needs the rest. This is the opposite of the
//!   choice [`crate::compaction_summary`] makes for *requests*, and for the reason that module gives —
//!   a requirement must never be paraphrased, while a narration is safe to cut.
//!
//! # Why `requests` is the first few rather than all of them
//!
//! Because a digest that contains everything is a transcript with a shorter name. A long session's
//! requests, concatenated, are its whole story, which is exactly what the caller already has in the
//! log. The first [`MAX_REQUESTS`] are kept: they are what the session was opened for, and reading
//! them orients a reader faster than any summary of the middle. Steering messages later in the session
//! still count toward [`Digest::turns`]; they are simply not stored, because a list of five requests
//! with no indication of which one is current is worse than three that clearly are the opening.
//!
//! They are also not deduplicated. Two identical prompts are two requests — the second one almost
//! always means "again", which is information — and collapsing them would hide a retry loop, which is
//! the shape of session a returning user most needs to recognise.
//!
//! # Why `finished` distinguishes "wrapped up" from "stopped midway"
//!
//! The two demand different decisions from the reader. A session whose last record is `turn/end`
//! reached a boundary: the model stopped, and the work it describes is complete as far as the log can
//! tell. A session whose last record is `turn/start` (or a step inside one) was interrupted — the
//! process died, the user killed it, or it is running right now. Reading the first as the second
//! wastes a session; reading the second as the first invites believing a half-finished change is done.
//!
//! The field answers the structural question only: *did the log stop at a boundary*. It deliberately
//! does not fold in [`TurnEndReason`](crate::TurnEndReason), because "aborted" and "failed" are still
//! *closed* — the caller that needs to know why reads the event, and the caller that only plots a list
//! should not have to match on five reasons to draw one icon. That split is stated here rather than
//! left to be discovered, because a "finished" session that actually failed is a real surprise if the
//! documentation does not warn about it.
//!
//! # Why characters rather than bytes
//!
//! [`one_line`] takes a character budget, for the reason [`crate::session_title`] documents: the number
//! is a width a person judges a row by, and a Chinese row cut to a third of the Latin allowance would
//! be a surprise. Slicing on `char` boundaries is also what keeps the output valid UTF-8 — a byte cut
//! through a multi-byte sequence yields mojibake at best.

use std::collections::BTreeSet;
// `write!` into a `String`, which is infallible; importing the trait is what makes the macro resolve.
use std::fmt::Write as _;

// `girder_core::…` rather than `crate::…` so this file also builds when a test includes it by path;
// the library declares the alias for itself, which is what makes both spellings the same crate.
use girder_core::{ChatRole, ContentBlock, MessageSource, SessionEvent};

/// How many user requests a digest keeps.
///
/// Three is the size of an opening: a session usually begins with one statement of intent, sometimes
/// with a correction, occasionally with a third try. Beyond that the list stops being an orientation
/// and starts being a summary of the middle, which is the thing this module refuses to become.
const MAX_REQUESTS: usize = 3;

/// Longest assistant text a digest keeps, in characters.
///
/// Generous rather than tight — a conclusion is often a couple of sentences plus a list — while still
/// small enough that a thousand digests fit in memory. The cut is on a character boundary and marked
/// with an ellipsis, so a clipped answer never looks like a complete one.
const LAST_ANSWER_MAX_CHARS: usize = 280;

/// Maximum number of tool names [`one_line`] spells out before counting the rest.
///
/// The names are in [`Digest::tools`] in full; this is the row budget, and a row that lists nine MCP
/// tool names is a row nobody reads.
const ONE_LINE_TOOL_NAMES: usize = 3;

/// Longest single tool name [`one_line`] prints, in characters.
///
/// Tool names come from extensions and MCP servers, so their length is not this crate's to bound. One
/// unclipped name would spend the whole row.
const TOOL_NAME_MAX_CHARS: usize = 24;

/// The tool-argument keys that name a file in this workspace.
///
/// Sourced from the tool schemas rather than guessed: `read`/`write`/`edit`/`glob`/`grep`/`lsp`/
/// `present`/`delete_path`/`make_dir` all take `path`, and `copy_path`/`move_path` take `source` and
/// `destination`. Any other key is ignored on purpose — `write` also carries `content`, and treating
/// every string value as a path would fill a digest with source code.
///
/// Duplicated from [`crate::compaction_summary`] rather than shared: that list is a private constant
/// there, and widening it would mean changing the module's public surface to serve a second consumer.
/// Three key names are cheaper to repeat than a second exported vocabulary to keep in step.
const PATH_ARGUMENTS: [&str; 3] = ["path", "source", "destination"];

/// Markers that carry a path inside an `apply_patch` document.
///
/// The patch tool takes one `patch` string rather than a path argument, so its targets are only
/// visible by reading the document. These are the parser's own markers, so a change there is a change
/// here.
const PATCH_MARKERS: [&str; 4] = [
    "*** Add File: ",
    "*** Delete File: ",
    "*** Update File: ",
    "*** Move to: ",
];

/// The character a clipped string ends with.
///
/// One character rather than `"..."`: the dots spend three of a small budget, and the ellipsis is a
/// single column wide in every font that has it.
const ELLIPSIS: char = '…';

/// What separates the fields of a one-line row.
const SEPARATOR: &str = " · ";

/// What introduces the last answer in a one-line row.
///
/// An arrow rather than a word: "then" and "answer:" both spend characters on a row that is mostly
/// content, and the direction is the whole meaning.
const ANSWER_LEAD: &str = " → ";

/// A session's conclusion, compressed to the few things a returning reader needs.
///
/// Produced by [`digest`]. Every field is a value the log already contains — nothing here is inferred,
/// scored, or written by a model — so two calls on the same events always agree, and a digest can be
/// computed in a test, a replay, or a page render with no provider configured.
///
/// The struct is the whole answer rather than a set of accessors because a caller either wants the
/// digest or does not; a session list renders [`one_line`], and a detail view can read the fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Digest {
    /// How many turns the session had.
    ///
    /// Counted from distinct turn numbers in `turn/start`, because a turn is identified by its number
    /// everywhere else in this crate ([`crate::session_timeline`] pairs the two boundary records the
    /// same way) and a resumed or duplicated record must not inflate the count. A log with no boundary
    /// records at all falls back to the number of human requests: the runtime writes the prompt before
    /// the boundary, so a session killed in between has requests and no `turn/start`, and reporting
    /// zero turns for it would be a lie about a session that plainly happened.
    pub turns: u64,

    /// The first [`MAX_REQUESTS`] things the person asked for, cleaned and in log order.
    ///
    /// Only messages actually written by the person: this build also writes synthetic injections on
    /// the user role — goal continuations, compaction recalls, relayed peer messages — and a request
    /// list opening with the harness's own bookkeeping would misdescribe the session. The `source`
    /// field is what distinguishes them, which is the same test
    /// [`crate::session_timeline`] applies for the same reason.
    ///
    /// Kept verbatim apart from whitespace collapsing; see the module docs for why requests are not
    /// clipped while the answer is.
    pub requests: Vec<String>,

    /// The last thing the assistant said, cleaned and bounded to [`LAST_ANSWER_MAX_CHARS`].
    ///
    /// `None` when the session produced no assistant text at all — a session that only ran tools, or
    /// one that was interrupted before the model replied. Absent rather than an empty string, because
    /// a caller rendering the field needs to tell "it said nothing" from "nothing was recorded", and
    /// only one of those is worth a placeholder.
    pub last_answer: Option<String>,

    /// Which tools ran, deduplicated in first-use order.
    ///
    /// Distinct because a turn that reads six files used one tool: a list of `read_file` six times
    /// says nothing about capability. First-use order rather than sorted because the order is the
    /// story — search then edit reads differently from edit then search.
    pub tools: Vec<String>,

    /// Which files were touched, deduplicated in first-use order, paths kept whole.
    ///
    /// Taken from the tool arguments on `tool/call` events, which is the only place a path appears as
    /// data rather than as prose. Paths are never shortened: a truncated path reads as a fact and
    /// resolves to nothing, which is worse than no path at all — the same rule
    /// [`crate::compaction_summary`] applies, and for the same reason.
    pub files: Vec<String>,

    /// Whether the log stopped at a `turn/end` rather than in the middle of a turn.
    ///
    /// See the module docs for what this does and does not claim: it says the log reached a boundary,
    /// not that the turn succeeded.
    pub finished: bool,
}

/// Builds a session's digest from its event log.
///
/// Total: every log, including an empty one, produces a [`Digest`]. There is no error case because
/// there is no failure — unparseable tool arguments contribute no paths, which is the correct reading
/// of "nothing was recorded", and a log missing its boundaries still yields the requests it holds.
///
/// One pass over the events. Nothing here depends on event order except the two fields for which
/// order *is* the content: the request list (the first few, in the order they were asked) and the
/// answer (the last one spoken).
#[must_use]
pub fn digest(events: &[SessionEvent]) -> Digest {
    // Distinct turn numbers, so a duplicated or resumed `turn/start` counts once.
    let mut opened: BTreeSet<u64> = BTreeSet::new();
    let mut requests: Vec<String> = Vec::new();
    // Counted past the cap: the turn fallback below needs the full count even though the list is cut.
    let mut request_count: u64 = 0;
    let mut last_answer: Option<String> = None;
    let mut tools: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut ended = false;

    for event in events {
        match event {
            SessionEvent::TurnStart { turn } => {
                opened.insert(*turn);
                // The last boundary so far is a start. Deliberately not gated on the turn number: an
                // unmatched start is still evidence that the log stopped mid-turn.
                ended = false;
            }
            SessionEvent::TurnEnd { .. } => ended = true,
            SessionEvent::UserMessage { message } => {
                let Some(text) = human_prompt_text(message) else {
                    continue;
                };
                request_count = request_count.saturating_add(1);
                if requests.len() < MAX_REQUESTS {
                    requests.push(text);
                }
            }
            SessionEvent::AssistantMessage { message } => {
                // Role rather than source: an assistant-role message is the model speaking whoever
                // routed it. A textless message — the tool-call step that is the common shape
                // mid-turn — is skipped rather than stored as an empty answer, so it cannot erase the
                // last thing the model actually said.
                if message.role != ChatRole::Assistant {
                    continue;
                }
                let text = clean(&visible_text(message));
                if text.is_empty() {
                    continue;
                }
                last_answer = Some(clip(&text, LAST_ANSWER_MAX_CHARS));
            }
            SessionEvent::ToolCall {
                name, arguments, ..
            } => {
                push_unique(&mut tools, name);
                collect_paths(arguments, &mut files);
            }
            // Every other event is metadata — usage, selection, preset, retry, step boundaries, tool
            // results — and none of it changes what the session concluded. Matched explicitly rather
            // than with a catch-all so a new variant that *should* count becomes a compile error here
            // instead of a silent omission.
            SessionEvent::StepStart { .. }
            | SessionEvent::StepEnd { .. }
            | SessionEvent::DeveloperMessage { .. }
            | SessionEvent::ModelUsage { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::SystemMessage { .. }
            | SessionEvent::ModelSelection { .. }
            | SessionEvent::PermissionPreset { .. }
            | SessionEvent::PlanMode { .. }
            | SessionEvent::SandboxMode { .. }
            | SessionEvent::RequestContext { .. }
            | SessionEvent::SessionTitle { .. }
            | SessionEvent::LlmRetry { .. } => {}
        }
    }

    // Boundaries are authoritative when present; requests are the fallback for a log written before
    // (or without) a boundary, see [`Digest::turns`].
    let turns = if opened.is_empty() {
        request_count
    } else {
        u64::try_from(opened.len()).unwrap_or(u64::MAX)
    };

    Digest {
        turns,
        requests,
        last_answer,
        tools,
        files,
        finished: ended,
    }
}

/// Renders a digest as one list row of at most `max_chars` characters.
///
/// The order is by what a reader needs first, because the budget cuts the tail: the turn count and
/// whether the session finished survive any budget, the first request survives a small one, the answer
/// needs a little more, and the tool and file counts are the first to go.
///
/// Tools and files are counted rather than listed. The row's job is to say what kind of session this
/// was at a glance — a read-only investigation, or one that touched files — and the names are in the
/// [`Digest`] for a view with room for them.
///
/// Returns the empty string for an empty digest and for a zero budget. A row that says nothing about a
/// session that did nothing is worse than no row: a caller can skip an empty string, but a caller
/// given `"0 turns · unfinished"` has to decide whether to hide it.
///
/// The result is always at most `max_chars` characters, cut on a character boundary and marked with an
/// ellipsis that counts toward the budget. A Chinese row therefore stays whole — see the module docs.
#[must_use]
pub fn one_line(digest: &Digest, max_chars: usize) -> String {
    if max_chars == 0 || is_empty(digest) {
        return String::new();
    }

    let mut line = String::new();
    let noun = if digest.turns == 1 { "turn" } else { "turns" };
    let _ = write!(line, "{} {noun}", digest.turns);
    line.push_str(SEPARATOR);
    line.push_str(if digest.finished {
        "finished"
    } else {
        "unfinished"
    });

    if let Some(request) = digest.requests.first() {
        line.push_str(SEPARATOR);
        line.push_str(request);
    }

    if let Some(answer) = &digest.last_answer {
        line.push_str(ANSWER_LEAD);
        line.push_str(answer);
    }

    if !digest.tools.is_empty() {
        line.push_str(SEPARATOR);
        let _ = write!(
            line,
            "tools: {}",
            tool_list(&digest.tools, TOOL_NAME_MAX_CHARS, ONE_LINE_TOOL_NAMES)
        );
    }

    if !digest.files.is_empty() {
        line.push_str(SEPARATOR);
        let noun = if digest.files.len() == 1 {
            "file"
        } else {
            "files"
        };
        let _ = write!(line, "{} {noun}", digest.files.len());
    }

    clip(&line, max_chars)
}

/// Whether a digest records nothing at all.
///
/// Not public: [`one_line`] is the only caller that needs it, and a public `is_empty` would suggest an
/// empty digest is a state worth branching on rather than one worth rendering as nothing.
fn is_empty(digest: &Digest) -> bool {
    digest.turns == 0
        && digest.requests.is_empty()
        && digest.last_answer.is_none()
        && digest.tools.is_empty()
        && digest.files.is_empty()
}

/// The visible text blocks of a message, joined by newlines.
///
/// Blocks are separated rather than concatenated: two text blocks are two paragraphs, and gluing them
/// would weld the last word of one to the first word of the next. [`clean`] collapses the newlines
/// afterwards, so they only ever separate words.
///
/// Only [`ContentBlock::Text`] counts. Reasoning is the model talking to itself, and an image or file
/// reference has no text to quote.
fn visible_text(message: &girder_core::ChatMessage) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text: block_text } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(block_text);
        }
    }
    text
}

/// The text of a request written by the person, cleaned, if the message is one.
///
/// "By the person" is narrower than "role is user", because this build also writes synthetic
/// injections on that role and their `source` is what distinguishes them: a goal continuation or a
/// compaction recall described as a user request would present the harness's bookkeeping as something
/// the person asked for. Only `User { form: None }` counts, which is the rule
/// [`crate::session_timeline`] applies for the same reason and the same field.
///
/// A blank message is skipped rather than stored: it would render as an empty bullet and would spend
/// one of the three request slots on nothing.
fn human_prompt_text(message: &girder_core::ChatMessage) -> Option<String> {
    if message.role != ChatRole::User {
        return None;
    }
    if !matches!(message.source, MessageSource::User { form: None }) {
        return None;
    }
    let cleaned = clean(&visible_text(message));
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned)
}

/// Reduces text to one trimmed line with single spaces between words.
///
/// Collapsing is not cosmetic: prompts and replies pasted from a terminal arrive with indentation and
/// blank lines, and a one-line row rendered with those pushes the rest of the list out of alignment.
/// Collapsing first also makes a character budget mean what it says, since text could otherwise spend
/// its allowance on whitespace.
fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cuts `text` to at most `max_chars` characters, marking the cut with an ellipsis.
///
/// On `char` boundaries rather than byte offsets: a byte slice through a multi-byte sequence is
/// invalid UTF-8, which is not a rendering artefact a caller can recover from. The ellipsis takes one
/// of the budgeted characters, so the result is at most `max_chars` characters and never one over.
fn clip(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    // `max_chars` is at least 1 here, so the subtraction cannot underflow.
    let mut bounded: String = text.chars().take(max_chars - 1).collect();
    bounded.push(ELLIPSIS);
    bounded
}

/// Names as many tools as `budget` characters allow, then counts the rest.
///
/// Built by adding whole names while they fit rather than by clipping a joined string: a clip applied
/// to `read_file, mcp__rainbow__get_gro` leaves half a tool name, which reads as a tool that does not
/// exist. Every name appears whole, and the list ends either on a name or on the count of what was
/// left out — never mid-word.
fn tool_list(tools: &[String], name_budget: usize, max_names: usize) -> String {
    let mut list = String::new();
    let mut written = 0_usize;
    for (index, name) in tools.iter().enumerate() {
        if written == max_names {
            break;
        }
        let name = clip(name, name_budget);
        let separator = if written == 0 { 0 } else { 2 };
        // Reserve room for " and N more" so a name is only taken when the rest can still be counted
        // inside the same budget. Without it, a list that filled the budget would drop the trailing
        // names silently.
        let hidden_after = tools.len().saturating_sub(index).saturating_sub(1);
        let tail = if hidden_after > 0 {
            " and ".len() + digit_count(hidden_after) + " more".len()
        } else {
            0
        };
        let needed = separator + name.chars().count() + tail;
        if list.chars().count() + needed > name_budget {
            break;
        }
        if written > 0 {
            list.push_str(", ");
        }
        list.push_str(&name);
        written += 1;
    }
    if written == 0 {
        // Not even one name fits: a bare count is still true, and it is all the budget allows.
        return clip(&tools.len().to_string(), name_budget);
    }
    let hidden = tools.len() - written;
    if hidden > 0 {
        let _ = write!(list, " and {hidden} more");
    }
    list
}

/// The number of decimal digits in `value`, for budgeting a count without formatting it first.
fn digit_count(value: usize) -> usize {
    let mut digits = 1;
    let mut remaining = value;
    while remaining >= 10 {
        remaining /= 10;
        digits += 1;
    }
    digits
}

/// Adds `candidate` to `values` unless it is blank or already present.
///
/// A linear scan rather than a set: a session holds a handful of tools and a few dozen paths, so the
/// quadratic worst case is smaller than the allocation a hash set would cost, and preserving first-use
/// order needs an ordered structure anyway.
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
/// Arguments that do not parse are skipped rather than surfaced as an error: a model can emit malformed
/// JSON, and a digest must still describe the rest of the session. A malformed patch document is
/// treated the same way, line by line, so one bad hunk does not hide the paths above it.
fn collect_paths(arguments: &str, files: &mut Vec<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return;
    };

    for key in PATH_ARGUMENTS {
        if let Some(path) = value.get(key).and_then(serde_json::Value::as_str) {
            push_unique(files, path);
        }
    }

    // `apply_patch` carries its targets inside the document rather than in a path argument, so a
    // session that edited twelve files through patches would otherwise report none.
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
