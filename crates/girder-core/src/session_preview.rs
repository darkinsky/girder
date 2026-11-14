//! One row of a session list: what the conversation is called, and what was said last.
//!
//! A `session.list` row carries identity and a timestamp and nothing else. `SessionSummary` is
//! deliberately made of fields that cost nothing to produce — the id, the log file's mtime, and one
//! `blank` bit read without decoding the log — because `SessionCatalog::list` runs over every session
//! on every refresh, and decoding every event of every session to build a list is O(sessions x
//! events) of zstd work for a view a person scans in a second. The result is a list that names its
//! sessions only when a title was written to the log, and never says what the conversation was about.
//!
//! This module fills the second half of that row: a title, and the last thing that was said.
//!
//! # Why this is not `session_digest`
//!
//! [`crate::session_digest`] answers *what did this session conclude*: the last answer, the first
//! three requests, the tools, the files, and whether the log stopped at a boundary. It is a summary of
//! a session a reader has already chosen, and it earns its cost by reading the whole log.
//!
//! A preview answers a different question, asked before the choice: *is this the session I want*. Two
//! consequences follow, and they are the reason this module exists rather than a flag on `digest`:
//!
//! - **The cost budget is different.** A digest is read once per opened session; a preview is read
//!   once per listed session. So [`preview_from_tail`] takes a slice the caller may have truncated to
//!   the last handful of events, and nothing here needs the middle or the beginning of a log. At
//!   about five events per turn, the last few events are exactly where the last message lives.
//! - **The interesting message is the last one, not the best one.** A digest promotes the last
//!   *answer*, because a conclusion is what the model said. A preview quotes the last message either
//!   party wrote, because the question it answers is "what were we just talking about", and a session
//!   interrupted while the person was still typing has no answer at all — but it does have a last
//!   message.
//!
//! # Why the snippet is single-line
//!
//! A list item is one row. A reply pasted from a terminal arrives with newlines, indentation, and
//! code blocks, and rendering it whole pushes every following row off the screen. Folding whitespace
//! first also makes the character budget mean what it says: text would otherwise spend its allowance
//! on runs of spaces that a reader never sees.
//!
//! # Why characters rather than bytes
//!
//! The budget is a width a person judges a row by, so counting bytes would give a Chinese row a third
//! of a Latin row's allowance. Cutting on `char` boundaries is also what keeps the result valid
//! UTF-8: a byte cut through a multi-byte sequence produces mojibake, and a reader cannot tell that
//! from a corrupt log.
//!
//! # What this module does not do
//!
//! It makes no model call and derives no title of its own. When the log records no title,
//! [`Preview::title`] is `None` rather than the first prompt: [`crate::session_title::derive`] is the
//! module that decides what a derived title may say, and a caller that wants one asks for it
//! explicitly. Duplicating that policy here would give the list and the title layer two ideas of what
//! a session is called.

// `girder_core::…` rather than `crate::…` so this file also builds when a test includes it by path;
// the library declares the alias for itself, which is what makes both spellings the same crate.
use std::collections::BTreeSet;

use girder_core::{ChatMessage, ContentBlock, MessageSource, SessionEvent};

/// The character a clipped snippet or title ends with.
///
/// One character rather than `"..."`: three dots spend three characters of a small row budget, and the
/// ellipsis is one column wide in every font that has it. The same choice, for the same reason, as
/// [`crate::session_digest`] and [`crate::session_title`].
const ELLIPSIS: char = '…';

/// One session's list row.
///
/// Every field is derived from the event slice and nothing else: no clocks, no provider, no model.
/// That is what lets a preview be computed during a page render, in a test, or by a host that has only
/// the log file.
///
/// The struct is plain data rather than a rendering, because the surfaces differ: a sidebar wants the
/// title bold and the snippet dim, a search result wants the snippet only, and an accessibility
/// description wants all four fields in a sentence. A single string would force one of those on
/// every caller.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Preview {
    /// Session id.
    ///
    /// Copied through rather than looked up: a preview's job is to be one value, and a row whose id
    /// had to be fetched separately would be one more thing to keep in step with the list order.
    pub id: String,

    /// Title, when the log records one.
    ///
    /// `None` when no `session/title` event was written — the session was never renamed and no title
    /// provider ran. Absent rather than empty, because a caller needs to tell "untitled" from
    /// "titled with nothing", and only one of those wants a placeholder.
    pub title: Option<String>,

    /// A snippet of the last message that had text, folded to one line and clipped.
    ///
    /// The empty string when the session has no message with text — an empty log, or one holding only
    /// boundary and accounting records. Empty rather than `None` because the common rendering is "the
    /// row has a subtitle or it does not", which is a length test, and a missing subtitle and an
    /// empty one look the same on screen.
    pub snippet: String,

    /// How many turns are visible in the events this preview was built from.
    ///
    /// **Exact only when given the whole log; a lower bound when given a tail.** The count is the
    /// number of distinct turn numbers named by the slice, so a caller that passes the last five
    /// events of a fifty-turn session gets `1`, not `50`. The field is not called `turns_seen`
    /// because the full-log case is the common one and the honest reading is what the documentation
    /// states: the number is never an overcount, and it is exact for a complete slice.
    ///
    /// Distinct turn numbers rather than boundary records, so a resumed or duplicated log — which can
    /// hold two `turn/start` records for one turn — does not report one turn as two. A slice with no
    /// turn number at all falls back to the number of prompts written by the person, which is what
    /// [`crate::session_digest`] does and for the same reason: the runtime writes the prompt before
    /// the boundary, so a session killed in between has prompts and no boundaries, and reporting zero
    /// turns for it would misdescribe a session that plainly happened.
    pub turns: u64,

    /// When the session was last active, in milliseconds since the epoch; `0` when unknown.
    ///
    /// **The event log carries no wall-clock time**, so this cannot come from `events`. Ordering is
    /// recorded; time is not. [`preview`] therefore reports `0`, and a caller that has a catalog row
    /// assigns the row's `updated_at` (which `DirectoryCatalog` takes from the log file's mtime)
    /// before rendering. The field is present rather than absent because a row that has to carry its
    /// timestamp separately is a row every call site forgets to complete, and `0` is unambiguous here:
    /// no real session was last active in 1970.
    pub at_ms: u64,
}

/// Builds a session's list preview from its events.
///
/// Pass the whole log when the caller already has it; otherwise use [`preview_from_tail`], which makes
/// the truncation explicit rather than accidental.
///
/// `max_chars` bounds [`Preview::snippet`] and [`Preview::title`] — both are drawn in the same row, so
/// both live inside one width. The bound counts characters, includes the ellipsis, and a `max_chars`
/// of zero yields an empty snippet and no title. [`Preview::at_ms`] is `0`; see its documentation for
/// why the log cannot supply it.
#[must_use]
pub fn preview(id: &str, events: &[SessionEvent], max_chars: usize) -> Preview {
    scan(id, events, max_chars)
}

/// Builds a preview from a slice that is known to be the *end* of a log.
///
/// This is the function that makes a session list affordable: a caller can read the last few events of
/// each log — `SessionStore::read_after` with a sequence near the end, or a tail read of the file —
/// and hand the result here, instead of decoding 200 complete logs to render 200 rows. The snippet is
/// the last message with text, so a tail that reaches back a few events finds it; the extra events
/// between it and the end are the tool calls and usage records of the step that wrote it.
///
/// Three consequences of the truncation, all of them stated rather than papered over:
///
/// - [`Preview::turns`] is a lower bound, not a total. The turns before the slice are not counted,
///   because they are not present.
/// - [`Preview::title`] is `None` when the `session/title` event falls before the slice. A title is
///   written once, near the start of a session, so a short tail usually misses it; a caller that
///   always needs the title should take it from the catalog's title store or derive it from the
///   header of the log.
/// - A slice is not verified to *be* a tail — that is the caller's claim, and this function cannot
///   tell the difference. Passing the beginning of a log yields the beginning's last message, which
///   is a wrong answer rather than an error.
#[must_use]
pub fn preview_from_tail(id: &str, tail: &[SessionEvent], max_chars: usize) -> Preview {
    scan(id, tail, max_chars)
}

/// The one pass both entry points share.
///
/// Two loops rather than one: the snippet is the *last* qualifying message, so it is found by walking
/// backwards and stopping at the first hit, while the turn count needs every event. The backward walk
/// therefore costs at most a handful of iterations when the events are passed as a tail, which is the
/// case this module is built for.
fn scan(id: &str, events: &[SessionEvent], max_chars: usize) -> Preview {
    Preview {
        id: id.to_owned(),
        title: last_title(events, max_chars),
        snippet: last_message_text(events).map_or_else(String::new, |text| clip(&text, max_chars)),
        turns: count_turns(events),
        // The log records order, not time. A caller with a catalog row fills this in; see the field.
        at_ms: 0,
    }
}

/// The last message with visible text, folded to one line.
///
/// Only [`SessionEvent::UserMessage`] and [`SessionEvent::AssistantMessage`] count. The other
/// message-carrying variants are not conversation: `system/message` is the prompt the harness
/// assembles — often thousands of characters of instructions — `developer/message` is tool
/// bookkeeping, and a `tool/result` is machine output the person never read. Quoting any of them
/// would put the harness's own text in the row where the conversation should be.
///
/// Walking backwards rather than remembering the latest match is what makes a decoy harmless: a
/// trailing tool-call step, an empty message written so a usage record has an owner, and a run of
/// accounting events are all skipped without any special case, because they simply do not qualify.
/// A message whose blocks are all non-text (a pure tool call) is skipped for the same reason — it is
/// not a newer thing said.
fn last_message_text(events: &[SessionEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        SessionEvent::UserMessage { message } | SessionEvent::AssistantMessage { message } => {
            let text = visible_text(message);
            let folded = one_line(&text);
            (!folded.is_empty()).then_some(folded)
        }
        _ => None,
    })
}

/// The visible text of a message, with blocks separated by a newline.
///
/// Blocks are separated rather than concatenated: two text blocks are two paragraphs, and gluing them
/// would weld the last word of one to the first word of the next. [`one_line`] collapses the newlines
/// afterwards, so they only ever separate words.
///
/// Only [`ContentBlock::Text`] is quoted. Reasoning is the model talking to itself rather than to the
/// reader, and an image or file reference has no text to show.
fn visible_text(message: &ChatMessage) -> String {
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

/// The last title the log records, folded to one line and clipped, when it has one.
///
/// The *last* `session/title` event wins: a rename appends a new title rather than rewriting the old
/// one, so the newest record is the current name. A blank title is skipped, because a row labelled
/// with whitespace is unreadable and is exactly the "no title" state this field distinguishes from
/// "titled".
///
/// The title is bounded here rather than trusted: [`SessionCatalog::rename`] stores whatever the
/// client sent, and a title longer than the row is not a title. The bound is the same `max_chars` the
/// snippet gets, since both are drawn in one row.
///
/// [`SessionCatalog::rename`]: crate::SessionCatalog::rename
fn last_title(events: &[SessionEvent], max_chars: usize) -> Option<String> {
    // A zero budget leaves nothing to put behind the ellipsis, and the empty string would render as
    // "titled with nothing" — a state this field exists to distinguish from "untitled". Checked before
    // the scan so a zero budget costs nothing and cannot produce a title at all.
    if max_chars == 0 {
        return None;
    }
    let title = events.iter().rev().find_map(|event| match event {
        SessionEvent::SessionTitle { title, .. } => Some(title),
        _ => None,
    })?;
    let folded = one_line(title);
    if folded.is_empty() {
        return None;
    }
    Some(clip(&folded, max_chars))
}

/// How many turns the slice can see.
///
/// Distinct turn numbers named by any event, which is at least as good as counting `turn/start`
/// records: a tail can begin in the middle of a turn, where no boundary record is present but the
/// steps and messages still name their turn. For a complete log the two are the same count, except
/// for a log that duplicated a boundary, and this form is immune to that duplication.
///
/// The fallback exists because the runtime writes the prompt before `turn/start` (see
/// [`crate::session_digest`]), so a session killed in between holds prompts and no turn numbers at
/// all. Counting those prompts is a lower bound on the turns that happened, which is the same
/// guarantee the field documents.
///
/// Only prompts written by the person count toward the fallback: this build also writes synthetic
/// messages on the user role — goal continuations, compaction recalls, relayed peer messages — and
/// counting the harness's own bookkeeping as a turn would inflate a session that never moved.
fn count_turns(events: &[SessionEvent]) -> u64 {
    let mut turns: BTreeSet<u64> = BTreeSet::new();
    let mut prompts: u64 = 0;
    for event in events {
        if let Some(turn) = event.turn() {
            turns.insert(turn);
        }
        if let SessionEvent::UserMessage { message } = event
            && message.role == girder_core::ChatRole::User
            && matches!(message.source, MessageSource::User { form: None })
        {
            prompts = prompts.saturating_add(1);
        }
    }
    if turns.is_empty() {
        prompts
    } else {
        // `usize` to `u64` is lossless on the platforms this runs on, and `try_from` keeps the
        // conversion honest on one where it is not: a saturated count beats a panic in a list view.
        u64::try_from(turns.len()).unwrap_or(u64::MAX)
    }
}

/// Reduces text to one trimmed line with single spaces between words.
///
/// This is the single-line guarantee. Splitting on whitespace rather than replacing `'\n'` with a
/// space is what keeps a row clean: a wrapped reply has newlines *and* indentation, and a
/// newline-only fold leaves runs of spaces that shift the row's text off its baseline. Collapsing
/// first also makes the character budget mean what it says, since no allowance is spent on
/// whitespace a reader cannot see.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cuts `text` to at most `max_chars` characters, marking the cut with an ellipsis.
///
/// On `char` boundaries rather than byte offsets, so a Chinese or emoji row is never split into
/// invalid UTF-8. The ellipsis spends one of the budgeted characters, so the result is at most
/// `max_chars` characters — a caller that reserved a row width gets a row no wider than it asked
/// for. A cut that would leave only the ellipsis is still made: "…" says "there is more" where an
/// empty string would say "there is nothing".
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
