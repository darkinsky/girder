//! A deterministic timeline of what a session did, one segment per turn.
//!
//! A client that offers a sidebar of "what did this session do" needs a list, not a transcript. This
//! module folds a session's event log into that list without a model call, so it can run in a test, a
//! replay, or a page render where no provider is configured.
//!
//! # Where this sits
//!
//! Upstream (`packages/session/session-turn-outline`) registers a `turnOutline` projection: for every
//! started turn it keeps the `turn/start` sequence number, a bounded preview of the first human prompt,
//! and a bounded preview of the final assistant reply committed at `turn/end`. That is the rail a chat
//! client renders for turns outside its paged event window; the entry exists so a click can page
//! history back to an exact sequence number.
//!
//! This build has no timeline projection. A client that wants the list today must fold the events it
//! got from `session.page` itself — [`crate::ProjectionRegistry`] carries `turnBoundary`, `title`, and
//! `modelSelection`, none of which answer "which things were done". [`outline`] is that fold as a pure
//! function over `&[SessionEvent]`, so the RPC layer, a resumed replay, and a test all agree.
//!
//! # Why a segment is a turn
//!
//! The vocabulary offers two boundaries: a turn ([`SessionEvent::TurnStart`] /
//! [`SessionEvent::TurnEnd`]) and a step ([`SessionEvent::StepStart`] / [`SessionEvent::StepEnd`]).
//! A step is one model call plus the tools it requested, so it is the right unit for "what is happening
//! now" and the wrong unit for a sidebar: a turn that reads three files and runs the tests is four
//! steps and one thing the user asked for. The segment is the turn, and [`Segment::steps`] keeps the
//! step count available for a caller that wants to show depth without splitting the row.
//!
//! # How the summary is produced, and its ceiling
//!
//! Without a model, only two signals in the log describe *what* a turn was about: what the person
//! asked and which tools ran. Assistant text is present too, and is deliberately not used — it is the
//! model's own narration, and quoting its first line produces something that reads like a truncated
//! transcript rather than a description of the work. So a summary is assembled from the turn's first
//! human prompt plus the names of the tools it used:
//!
//! - prompt and tools — the prompt, then `(tools: …)`;
//! - tools only — `Used …`;
//! - neither, but steps ran — `N steps, no tool calls`;
//! - nothing at all — the empty string, which is what a just-opened turn looks like.
//!
//! This is a genuine ceiling, and it is worth naming: "can you fix this" and "the release job fails on
//! the second retry, make it stop paging us" are both prompts, and only the second one summarises
//! itself. A model reading the turn could compress either into a subject; this module can only quote,
//! so its summaries are honest reminders of where a turn started, not a compression of what it
//! achieved. That is the same trade-off [`crate::session_title`] documents for titles, for the same
//! reason, and the difference from upstream is stated rather than left for a reader to discover:
//! upstream previews the *response*, this module previews the *request* and adds the tool names
//! upstream does not record. Neither includes the other's half.
//!
//! # Why an unfinished turn is still a segment
//!
//! A turn that has `turn/start` and no `turn/end` is not a broken log — it is a turn that is running
//! right now, or one whose process died mid-step. Folding only on `turn/end` would make the live turn
//! vanish from the sidebar at exactly the moment the user is watching it, and would make a crashed
//! session look like it did nothing. So the open turn is emitted, with a partial step count, and
//! [`open_turn`] answers which turn (if any) is the one still in flight so a client can render a
//! spinner on it instead of guessing from the last row.
//!
//! # Why attribution cannot use a turn field everywhere
//!
//! [`SessionEvent::TurnStart`], [`SessionEvent::TurnEnd`], and [`SessionEvent::StepStart`] name their
//! turn. [`SessionEvent::UserMessage`] and [`SessionEvent::ToolCall`] do not — the vocabulary models
//! them as belonging to whatever is open. The fold therefore follows log position: a user message
//! outside an open turn is the prompt of the *next* turn, which is the order this build actually
//! writes (the runtime records the prompt before spawning the loop, so `user/message` precedes
//! `turn/start`; upstream opens the turn first — the deviation is recorded in `docs/migration-v2.md`).
//! Both orders attribute the same way, which is why the fold is not simply "events after the last
//! boundary".
//!
//! # Example
//!
//! ```text
//! turn/start 1, user/message "fix the retry", step/start 1, tool/call bash, step/end 1, turn/end 1
//!   -> Segment { turn: 1, summary: "fix the retry (tools: bash)", tools: ["bash"], steps: 1 }
//! ```

use std::collections::BTreeSet;
// `write!` into a `String`, which is infallible; importing the trait is what makes the macro resolve.
use std::fmt::Write as _;

// `girder_core::…` rather than `crate::…` so this file also builds when a test includes it by path;
// the library declares the alias for itself, which is what makes both spellings the same crate.
use girder_core::{ChatMessage, ChatRole, ContentBlock, MessageSource, SessionEvent};

/// Longest summary this module produces, in characters.
///
/// Characters rather than bytes, for the reason [`crate::session_title`] gives: the budget is a number
/// a person would guess at judging a one-line row, and cutting a Chinese summary to a third of the
/// Latin allowance would be a surprise. The number is a sidebar row, not a paragraph: wide enough for
/// a short sentence and the tool list behind it, narrow enough that a client never has to re-clip.
const SUMMARY_MAX_CHARS: usize = 96;

/// Shortest prompt excerpt left after the tool suffix has taken its share.
///
/// Three tool names can be long (`mcp__rainbow__get_group_config` is 30 characters), and a suffix that
/// consumed the whole budget would leave a row that names tools but not the request that caused them.
/// The floor keeps the request readable; the final [`clip`] still bounds the total.
const MIN_PROMPT_CHARS: usize = 24;

/// How many tool names a summary spells out before it counts the rest.
///
/// Not a data limit — [`Segment::tools`] holds every distinct name. This is the row budget: a turn
/// that called nine tools is described by its first three and "+6 more", which is what a reader can
/// take in at a glance.
const SUMMARY_TOOL_NAMES: usize = 3;

/// Longest single tool name a summary prints, in characters.
///
/// Tool names come from extensions and MCP servers, so their length is not this crate's to bound. One
/// unclipped name would spend the whole row.
const TOOL_NAME_MAX_CHARS: usize = 24;

/// The lead-in of the tool tail on a summary that also quotes a prompt.
const TOOL_PREFIX: &str = " (tools: ";

/// The lead-in of a summary that has no prompt, so the tools are the whole description.
const USED_PREFIX: &str = "Used ";

/// The character that closes the tool tail.
///
/// Reserved before the names are laid out rather than appended afterwards, so the budget arithmetic
/// and the produced string agree about how much room the tail actually takes.
const TOOL_TAIL_CLOSE: char = ')';

/// The character a clipped summary ends with.
///
/// One character rather than `"..."`: the dots spend three of a small budget, and the ellipsis is a
/// single column in every font that has it.
const ELLIPSIS: char = '…';

/// One stretch of a session's timeline.
///
/// A segment is one turn: everything between a `turn/start` and the `turn/end` that closes it, or the
/// still-open turn at the end of a log. See the module docs for why the step, not the turn, was
/// rejected as the unit.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    /// Which turn this is.
    ///
    /// The number `turn/start` carried, not a position in this vector: a resumed or forked log can
    /// repeat a number, and renumbering here would make a segment impossible to match against the
    /// events it came from.
    pub turn: u64,

    /// What this segment did, in one line.
    ///
    /// Empty when the log records nothing to derive it from — a turn that opened and has not produced
    /// a prompt or a tool call yet. Empty rather than a placeholder like `"(in progress)"` because the
    /// caller knows whether the turn is still open ([`open_turn`]) and can word it for its own surface;
    /// upstream uses the same empty-string convention for a preview that has not landed.
    pub summary: String,

    /// The distinct tools this turn used, in first-use order.
    ///
    /// Distinct because a turn that reads six files used one tool, and a row that listed `read_file`
    /// six times would say nothing. First-use order rather than sorted because the order is the story:
    /// a turn that searched and then edited reads differently from one that did the reverse.
    ///
    /// Empty for a turn that called no tools, which includes a turn that has not reached its first
    /// step.
    pub tools: Vec<String>,

    /// How many steps ran in this turn.
    ///
    /// Counted from `step/start` and deduplicated by step number, so a log that recorded one step
    /// twice (a retried write, a duplicated record) still counts it once. Partial for a turn that is
    /// still open: it is what has run so far, not a final count.
    pub steps: u32,
}

/// Folds an event log into its timeline.
///
/// One segment per turn boundary, in log order. Turns are emitted even when they have not closed, so a
/// live turn appears as the last segment rather than being dropped; [`open_turn`] tells a caller which
/// one that is.
///
/// Events that carry no turn and sit outside any boundary — a tool call logged after a `turn/end` — are
/// not attributed to anything. Guessing "the nearest turn" would put the first tool call of turn N+1 on
/// the row for turn N, which is exactly the mistake a timeline exists to avoid.
///
/// An empty log yields an empty vector, not a placeholder segment: a session with no events did
/// nothing, and a row claiming otherwise would be the first thing a user sees on a new session.
///
/// # Example
///
/// ```text
/// let events = vec![
///     SessionEvent::TurnStart { turn: 1 },
///     SessionEvent::user_text("m1", "fix the retry"),
///     SessionEvent::TurnEnd { turn: 1, reason: TurnEndReason::Completed },
/// ];
/// assert_eq!(outline(&events).len(), 1);
/// ```
#[must_use]
pub fn outline(events: &[SessionEvent]) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut open: Option<Building> = None;
    // A prompt that arrived before its `turn/start`. This build writes the prompt first so it survives
    // a crash before the model answers, which means the boundary that owns it comes later in the log.
    let mut pending_prompt: Option<String> = None;

    for event in events {
        match event {
            SessionEvent::TurnStart { turn } => {
                // A repeated `turn/start` for the turn already open keeps the segment: the log writer
                // has produced two records for one boundary before (see `docs/migration-v2.md`), and
                // splitting here would report one turn as two rows with half the tools each.
                if open.as_ref().is_some_and(|building| building.turn == *turn) {
                    continue;
                }
                // A turn that never ended is closed by the next one starting: the boundary is the only
                // evidence there is that the previous turn stopped, and dropping it would lose the row.
                if let Some(building) = open.take() {
                    segments.push(building.finish());
                }
                open = Some(Building {
                    turn: *turn,
                    prompt: pending_prompt.take(),
                    tools: Vec::new(),
                    steps: BTreeSet::new(),
                });
            }
            SessionEvent::TurnEnd { turn, .. } => {
                // Only the turn that is actually open can be closed by this event. A `turn/end` naming
                // another turn means a malformed log; inventing the missing boundary would be worse
                // than ignoring the record.
                if open.as_ref().is_some_and(|building| building.turn == *turn) {
                    if let Some(building) = open.take() {
                        segments.push(building.finish());
                    }
                }
            }
            SessionEvent::StepStart { turn, step } => {
                // The step names its turn, so it is attributed by that number rather than by position.
                // A mismatch is a log where the two disagree, and counting the step for whichever turn
                // happens to be open would silently inflate that row.
                if let Some(building) = open.as_mut() {
                    if building.turn == *turn {
                        building.steps.insert(*step);
                    }
                }
            }
            SessionEvent::UserMessage { message } => {
                let Some(text) = human_prompt_text(message) else {
                    continue;
                };
                match open.as_mut() {
                    // The first human message of a turn is the request; a later one is steering the
                    // work already under way, and replacing the summary with it would rename the row
                    // mid-turn. Upstream keeps the first prompt for the same reason.
                    Some(building) if building.prompt.is_none() => building.prompt = Some(text),
                    Some(_) => {}
                    // Before the boundary: this is the prompt of the turn about to open. Only the
                    // first is kept — a queued burst is one request, and the first message is the one
                    // that named it.
                    None => {
                        pending_prompt.get_or_insert(text);
                    }
                }
            }
            SessionEvent::ToolCall { name, .. } => {
                // A tool call carries no turn, so it belongs to the open one. Outside a boundary it is
                // unattributable and dropped; see the function docs.
                if let Some(building) = open.as_mut() {
                    building.note_tool(name);
                }
            }
            // Every other event is metadata — usage, selection, preset, retry — and none of it changes
            // what the turn did. Matched explicitly rather than with a catch-all so a new variant that
            // *should* count becomes a compile error here instead of a silent omission.
            SessionEvent::StepEnd { .. }
            | SessionEvent::ModelUsage { .. }
            | SessionEvent::DeveloperMessage { .. }
            | SessionEvent::AssistantMessage { .. }
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

    if let Some(building) = open.take() {
        segments.push(building.finish());
    }

    segments
}

/// The turn that started and has not ended, when there is one.
///
/// The companion to [`outline`], because a [`Segment`] is a value about the past and an in-flight turn
/// is not. A client asking "is something happening?" needs the answer without walking the log itself,
/// and without inferring it from `summary.is_empty()` — a turn can be open with a full summary already.
///
/// A log whose last boundary is a `turn/start` with a matching `turn/end` later is not open. A turn that
/// was superseded by the next `turn/start` is not open either: [`outline`] closed it at that boundary.
///
/// Compared by turn number, which is how the log pairs the two records. A forked log can repeat a
/// number, so the answer is the *last* boundary's verdict rather than every unmatched start.
#[must_use]
pub fn open_turn(events: &[SessionEvent]) -> Option<u64> {
    let mut open = None;
    for event in events {
        match event {
            SessionEvent::TurnStart { turn } => open = Some(*turn),
            // Compared rather than unconditionally cleared: a `turn/end` naming a different turn is a
            // malformed log, and clearing here would report a turn as finished that never closed.
            SessionEvent::TurnEnd { turn, .. } if open == Some(*turn) => open = None,
            _ => {}
        }
    }
    open
}

/// A turn still being accumulated.
///
/// Separate from [`Segment`] because the fields arrive out of order and independently: the prompt may
/// precede or follow the boundary, tools arrive one call at a time with no turn number, and steps are a
/// set rather than a count until the turn closes. Building the public value incrementally would mean
/// exposing a half-filled [`Segment`] and asking every caller to know which fields are meaningful yet.
struct Building {
    /// The turn number from `turn/start`.
    turn: u64,

    /// The first human prompt, when one was seen.
    prompt: Option<String>,

    /// Distinct tool names in first-use order.
    tools: Vec<String>,

    /// Distinct step numbers that started.
    steps: BTreeSet<u64>,
}

impl Building {
    /// Records a tool name unless the turn already used it.
    ///
    /// A linear scan is right at this size: a turn uses a handful of tools, and the vector is what the
    /// caller wants as output, so a set beside it would be a second allocation to keep in step.
    fn note_tool(&mut self, name: &str) {
        if !self.tools.iter().any(|seen| seen == name) {
            self.tools.push(name.to_owned());
        }
    }

    /// Produces the public value, deriving the summary.
    fn finish(self) -> Segment {
        let steps = u32::try_from(self.steps.len()).unwrap_or(u32::MAX);
        let summary = compose(self.prompt.as_deref(), &self.tools, steps);
        Segment {
            turn: self.turn,
            summary,
            tools: self.tools,
            steps,
        }
    }
}

/// Builds the one-line summary from the only two signals available without a model.
///
/// See the module docs for the four cases and why they are worded this way. The composition is total:
/// every input yields a string, and that string is always at most [`SUMMARY_MAX_CHARS`] characters.
fn compose(prompt: Option<&str>, tools: &[String], steps: u32) -> String {
    if let Some(prompt) = prompt {
        // The suffix is built first and bounded on its own, then the prompt is clipped into exactly
        // the room that is left. Building it the other way round would let a long tool list push the
        // request out of the row, and clipping the joined string would cut the suffix mid-name — the
        // two halves are budgeted separately so neither can truncate the other.
        let suffix = if tools.is_empty() {
            String::new()
        } else {
            // The prefix and the closing character are subtracted here rather than inside
            // [`tool_list`], so the helper budgets only the names it lays out and its comment about
            // "and N more" stays the only reservation it has to reason about. Saturating because these
            // are four independent constants: an edit that shrinks the row must yield a cramped tail
            // on every turn, not a panic in the one place a summary is built.
            let names_room = SUMMARY_MAX_CHARS
                .saturating_sub(MIN_PROMPT_CHARS)
                .saturating_sub(TOOL_PREFIX.len())
                .saturating_sub(TOOL_TAIL_CLOSE.len_utf8());
            format!("{TOOL_PREFIX}{})", tool_list(tools, names_room))
        };
        // The suffix is at most `SUMMARY_MAX_CHARS - MIN_PROMPT_CHARS`, so the room left is at least
        // `MIN_PROMPT_CHARS` and the joined result is at most `SUMMARY_MAX_CHARS` with no second cut.
        let room = SUMMARY_MAX_CHARS.saturating_sub(suffix.chars().count());
        let mut summary = excerpt(prompt, room);
        summary.push_str(&suffix);
        return summary;
    }
    if !tools.is_empty() {
        return format!(
            "{USED_PREFIX}{}",
            tool_list(tools, SUMMARY_MAX_CHARS.saturating_sub(USED_PREFIX.len()))
        );
    }
    if steps > 0 {
        // No prompt and no tools still leaves evidence that the model ran: it answered without asking
        // for anything. Saying so is more useful than an empty row, and it does not claim what it said
        // — the text is deliberately not read (module docs).
        let noun = if steps == 1 { "step" } else { "steps" };
        return format!("{steps} {noun}, no tool calls");
    }
    String::new()
}

/// Names as many tools as `budget` characters allow, then counts the rest.
///
/// Built by adding whole names while they fit rather than by clipping a joined string: a clip applied
/// to `read_file, mcp__rainbow__get_gro` leaves half a tool name, which reads as a tool that does not
/// exist. Every name therefore appears whole, and the list ends either on a name or on the count of
/// what was left out — never mid-word.
///
/// [`Segment::tools`] still holds the complete list. This budget is the row, not the data.
fn tool_list(tools: &[String], budget: usize) -> String {
    let mut list = String::new();
    let mut written = 0_usize;
    for (index, name) in tools.iter().enumerate() {
        if written == SUMMARY_TOOL_NAMES {
            break;
        }
        let name = clip(name, TOOL_NAME_MAX_CHARS);
        let separator = if written == 0 { 0 } else { 2 };
        // Reserved room for " and N more", so a name is only taken when the rest of the list can
        // still be counted inside the same budget. Without the reservation, a list that filled the
        // budget exactly would drop the trailing names silently.
        // `saturating_sub`: `index` is always below `len` here, but the reservation must not be the
        // one expression in this function that could panic if the loop bound ever changes.
        let hidden_after = tools.len().saturating_sub(index).saturating_sub(1);
        let tail = if hidden_after > 0 {
            " and ".len() + digit_count(hidden_after) + " more".len()
        } else {
            0
        };
        let needed = separator + name.chars().count() + tail;
        if list.chars().count() + needed > budget {
            break;
        }
        if written > 0 {
            list.push_str(", ");
        }
        list.push_str(&name);
        written += 1;
    }
    if written == 0 {
        // Not even one name fits: a bare count is still true, and it is the only thing the budget
        // allows. Reached only by a budget smaller than a single clipped tool name.
        return clip(&tools.len().to_string(), budget);
    }
    let hidden = tools.len() - written;
    if hidden > 0 {
        // `write!` rather than `push_str(&format!(..))`: the same bytes without a second allocation,
        // and the string is bounded, so the infallible path is the honest one.
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

/// The text of a message written by the person, cleaned, if it is one.
///
/// "By the person" is narrower than "role is user". This build writes synthetic injections with the
/// user role as well — a goal continuation, a compaction recall, a relayed peer message — and their
/// `source` distinguishes them ([`MessageSource::User`] with a [`ContextForm`](crate::ContextForm) for
/// the machine-written ones, [`MessageSource::Relay`] for a peer). A summary built from a goal
/// continuation would describe the harness's own bookkeeping as if the user had asked for it, so only
/// `User { form: None }` counts. Upstream applies the same rule by filtering on `source.kind`.
///
/// Text blocks are joined with a newline before cleaning: two blocks are two paragraphs, and gluing
/// them would weld the last word of one to the first of the next.
fn human_prompt_text(message: &ChatMessage) -> Option<String> {
    if message.role != ChatRole::User {
        return None;
    }
    let is_person = matches!(message.source, MessageSource::User { form: None });
    if !is_person {
        return None;
    }
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text: block_text } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(block_text);
        }
    }
    let cleaned = clean(&text);
    // A blank message is skipped rather than kept: it would produce a row whose summary is a space,
    // and it would shadow the first real prompt behind it if it landed first.
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned)
}

/// Reduces text to one trimmed line with single spaces between words.
///
/// Collapsing is not cosmetic: a prompt pasted from a terminal arrives with indentation and blank
/// lines, and a row rendered with those would push the rest of the sidebar out of alignment. Collapsing
/// first also makes the character budget mean something, since a summary could otherwise spend its
/// whole allowance on whitespace.
fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cleans and then clips, in that order, so the budget counts words rather than layout.
fn excerpt(text: &str, budget: usize) -> String {
    clip(&clean(text), budget)
}

/// Cuts text to at most `budget` characters, ending with an ellipsis when anything was cut.
///
/// Character boundaries, never bytes: cutting a multi-byte character in half produces a string that is
/// not the text it came from, and a summary is displayed verbatim. A budget of zero yields the empty
/// string rather than an ellipsis with nothing behind it.
fn clip(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_owned();
    }
    if budget == 0 {
        return String::new();
    }
    // `budget - 1` leaves room for the ellipsis, which is why it is added after the cut rather than
    // counted before it.
    let mut clipped: String = text.chars().take(budget - 1).collect();
    clipped.push(ELLIPSIS);
    clipped
}
