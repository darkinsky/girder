//! Deriving a session title from the conversation itself.
//!
//! A session log has a [`SessionEvent::SessionTitle`] variant, but until now nothing produced one:
//! a title existed only when a person typed one with `rename`. That leaves every session unnamed in
//! the list a user actually navigates, because nobody renames a session they abandoned after one
//! message. This module is the deterministic producer that fills the gap.
//!
//! # What this is, and what it is not
//!
//! Upstream (`packages/session/session-title`) has two layers. The deterministic one —
//! `fallbackSessionTitle` in `normalize.ts` — is what this module ports: clean the text, join the
//! words, cut it to a budget. The other layer is an LLM provider
//! (`session-title-first-prompt-llm`), which sends the first prompt to a model and asks for a
//! title. **That layer is deliberately not implemented here.** Generating a title through a model is
//! one request, with a route, a timeout, a retry policy, and a failure mode; it belongs to whatever
//! owns model calls, not to a pure function over a log. Keeping it out also keeps this module
//! testable without a provider.
//!
//! The difference is worth stating plainly, because it is the whole reason upstream pays for the
//! extra layer. A leading-text extraction **cannot tell you what a conversation is about**. "Can you
//! fix this" and "Why does the release job fail on the second retry, and can you make it stop
//! paging us" produce titles of very different usefulness, and the first one is common: people open
//! with context the model already has. A model reading the first prompt can compress it to a
//! subject. This module can only quote the opening words, so its titles are honest but shallow —
//! they are a readable reminder of where the conversation started, not a summary of it.
//!
//! # Why characters rather than bytes
//!
//! Upstream budgets bytes, because it stores titles in a byte-counted field and a split code point
//! would produce mojibake. The same concern exists here, but the signature upstream's callers care
//! about — "how long may this title be" — is a count a person reads, and a Chinese title that got
//! one third of the Latin budget would be a surprise. Counting characters and truncating on
//! character boundaries gives both properties: the cap means what a reader would guess, and no
//! multi-byte character is ever cut in half.

// `girder_core::…` rather than `crate::…` so this file also builds when a test includes it by path;
// the library declares the alias for itself, which is what makes both spellings the same crate.
use girder_core::{ChatMessage, ChatRole, ContentBlock, SessionEvent};

/// Where the title text comes from.
///
/// The two variants are the two policies upstream exposes through a provider's `automatic` mode,
/// and the choice is a real trade-off rather than a detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The first user message.
    ///
    /// Cheap and stable: the title does not change as the conversation grows, so a UI does not
    /// reorder its list under the reader. The cost is that a short or context-dependent opener
    /// yields a short or context-dependent title.
    FirstPrompt,
    /// Every user message, concatenated in log order.
    ///
    /// Worth more text for a conversation whose opening was a fragment, at the price of needing a
    /// re-derivation whenever a new message arrives — the title can change mid-conversation.
    AllPrompts,
}

/// The character a truncated title ends with.
///
/// A single character rather than `"..."`: three dots spend three characters of a small budget, and
/// the ellipsis is one column wide in every font that has it.
const ELLIPSIS: char = '…';

/// Derives a title from a session's events.
///
/// Text is taken from user-role messages only. An assistant reply describes the model's answer,
/// not the request, and a tool result is machine output; titling a conversation after either would
/// name it after something the user never asked for. The first and last words of every candidate
/// are kept — see [`clean`] — so a title is always one line.
///
/// Returns `None` when there is nothing to derive from: no user message, or only messages that are
/// blank after cleaning. An empty string is never returned, because an empty `SessionTitle` event
/// is a title a UI would render as an unnamed session, which is the state this module exists to
/// avoid.
///
/// `max_chars` is an upper bound, not an exact length. A title longer than it is cut on a character
/// boundary and ends with an ellipsis, which counts toward the bound. A `max_chars` of zero
/// therefore yields `None` rather than an ellipsis with nothing behind it.
#[must_use]
pub fn derive(events: &[SessionEvent], source: Source, max_chars: usize) -> Option<String> {
    let joined = match source {
        Source::FirstPrompt => first_prompt(events),
        Source::AllPrompts => all_prompts(events),
    }?;
    let cleaned = clean(&joined);
    let title = truncate(&cleaned, max_chars);
    // A zero budget leaves nothing to put behind the ellipsis. Returning the empty string instead of
    // `None` would hand the caller a title it would render as an unnamed session, which is the state
    // this module exists to avoid, so the emptiness check is repeated here after the cut.
    if title.is_empty() {
        return None;
    }
    Some(title)
}

/// Joins the text blocks of one message.
///
/// The blocks are joined with a newline rather than concatenated directly, matching how upstream
/// reads a message: two text blocks are two paragraphs, and gluing them would weld the last word of
/// one to the first word of the next. The newline is later collapsed by [`clean`], so it only ever
/// separates words.
///
/// Only [`ContentBlock::Text`] counts. A tool call is not something the person said.
fn message_text(message: &ChatMessage) -> String {
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

/// Whether an event is a user-role message whose text survives cleaning.
///
/// An empty message is skipped rather than joined, so it cannot contribute a stray separator or
/// shadow the first real prompt. Upstream applies the same rule when it decides which messages
/// feed the title, and for the same reason: a blank injection arriving before the person's first
/// words must not become the title.
fn usable_user_text(event: &SessionEvent) -> Option<String> {
    let SessionEvent::UserMessage { message } = event else {
        return None;
    };
    if message.role != ChatRole::User {
        return None;
    }
    let text = message_text(message);
    if clean(&text).is_empty() {
        return None;
    }
    Some(text)
}

/// The first usable user message.
fn first_prompt(events: &[SessionEvent]) -> Option<String> {
    events.iter().find_map(usable_user_text)
}

/// Every usable user message, joined in log order.
fn all_prompts(events: &[SessionEvent]) -> Option<String> {
    let joined = events
        .iter()
        .filter_map(usable_user_text)
        .collect::<Vec<_>>()
        .join(" ");
    if joined.is_empty() {
        return None;
    }
    Some(joined)
}

/// Reduces raw text to one trimmed line with single spaces between words.
///
/// Whitespace collapsing is not cosmetic. A prompt pasted from a terminal or a source file arrives
/// with indentation and blank lines, and a title rendered with those would push the rest of the list
/// out of alignment. Collapsing first also makes the character budget mean something: without it, a
/// title could spend its whole allowance on spaces.
fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cuts `text` to at most `max_chars` characters, marking the cut with an ellipsis.
///
/// Cutting on `char` boundaries rather than on byte offsets is what keeps Chinese and emoji whole;
/// a byte slice would panic or, worse, produce a replacement character where a multi-byte sequence
/// was split. The ellipsis takes one of the budgeted characters, so the result is at most
/// `max_chars` characters — never one over.
fn truncate(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut bounded: String = text.chars().take(max_chars - 1).collect();
    bounded.push(ELLIPSIS);
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_limit_yields_no_title() {
        let events = vec![SessionEvent::user_text("m", "hello")];

        assert_eq!(derive(&events, Source::FirstPrompt, 0), None);
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // "你好世界" is four characters but twelve UTF-8 bytes. A byte-based cut at the same number
        // would panic; a character-based one keeps the units whole.
        let cut = truncate("你好世界", 3);

        assert_eq!(cut, "你好…");
        assert_eq!(cut.chars().count(), 3);
    }
}
