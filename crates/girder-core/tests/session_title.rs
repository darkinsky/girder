//! Tests for deriving a session title from the conversation.
//!
//! Two properties carry the weight here. A title is read by a person, so it must be one line of
//! readable text — no newlines, no runs of spacing — and it must survive being cut: a bound that
//! splits a Chinese character or an emoji in half produces a title that is worse than no title.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written even before the crate root wires it in. It resolves `girder_core::…`
//! through the alias the library declares for itself, which is why the same source builds here.

#[path = "../src/session_title.rs"]
mod session_title;

use girder_core::SessionEvent;

use session_title::{Source, derive};

/// A user message whose text is the whole point of the case.
fn user(id: &str, text: &str) -> SessionEvent {
    SessionEvent::user_text(id, text)
}

// ---- which messages are eligible ---------------------------------------------------------------

#[test]
fn the_first_prompt_source_reads_only_the_first_user_message() {
    let events = vec![
        user("m1", "explain the retry policy"),
        user("m2", "and the backoff"),
    ];

    assert_eq!(
        derive(&events, Source::FirstPrompt, 40).as_deref(),
        Some("explain the retry policy")
    );
}

#[test]
fn the_all_prompts_source_joins_every_user_message_in_log_order() {
    let events = vec![user("m1", "why does"), user("m2", "the job page us")];

    assert_eq!(
        derive(&events, Source::AllPrompts, 40).as_deref(),
        Some("why does the job page us")
    );
}

#[test]
fn a_conversation_without_a_user_message_has_no_title() {
    let events = vec![SessionEvent::assistant_text("a1", "an answer", "test")];

    assert_eq!(derive(&events, Source::FirstPrompt, 40), None);
    assert_eq!(derive(&events, Source::AllPrompts, 40), None);
}

#[test]
fn assistant_and_tool_events_do_not_become_the_title() {
    // The model's reply describes the answer, not the request; titling after it would name the
    // conversation after something the person never asked.
    let events = vec![
        SessionEvent::assistant_text("a1", "Leading answer text", "test"),
        user("m1", "the actual question"),
    ];

    assert_eq!(
        derive(&events, Source::FirstPrompt, 40).as_deref(),
        Some("the actual question")
    );
}

#[test]
fn blank_user_messages_are_skipped() {
    // A synthetic injection can arrive before the person says anything, and it must not become
    // the title.
    let events = vec![user("m1", "   \n\t "), user("m2", "real question")];

    assert_eq!(
        derive(&events, Source::FirstPrompt, 40).as_deref(),
        Some("real question")
    );
    assert_eq!(
        derive(&events, Source::AllPrompts, 40).as_deref(),
        Some("real question")
    );
}

#[test]
fn a_conversation_of_only_blank_messages_has_no_title() {
    let events = vec![user("m1", " \n "), user("m2", "\t")];

    assert_eq!(derive(&events, Source::FirstPrompt, 40), None);
    assert_eq!(derive(&events, Source::AllPrompts, 40), None);
}

// ---- shaping the text --------------------------------------------------------------------------

#[test]
fn newlines_are_removed() {
    let events = vec![user("m1", "fix the\nbuild\nplease")];

    assert_eq!(
        derive(&events, Source::FirstPrompt, 40).as_deref(),
        Some("fix the build please")
    );
}

#[test]
fn runs_of_whitespace_are_collapsed() {
    let events = vec![user("m1", "  lots   of\t\tspace  ")];

    assert_eq!(
        derive(&events, Source::FirstPrompt, 40).as_deref(),
        Some("lots of space")
    );
}

#[test]
fn a_title_within_the_limit_is_returned_whole() {
    let events = vec![user("m1", "short title")];

    // No ellipsis, and no trailing space left by the trim.
    assert_eq!(
        derive(&events, Source::FirstPrompt, 40).as_deref(),
        Some("short title")
    );
}

// ---- the bound counts characters, not bytes ----------------------------------------------------

#[test]
fn chinese_text_is_cut_on_character_boundaries() {
    // Eight characters, but twenty-four UTF-8 bytes: a byte-based cut would land mid-character.
    let events = vec![user("m1", "你好世界你好世界")];

    let title = derive(&events, Source::FirstPrompt, 5).expect("text yields a title");

    assert_eq!(title, "你好世界…");
    assert_eq!(title.chars().count(), 5, "the bound is characters");
}

#[test]
fn emoji_are_cut_on_character_boundaries() {
    let events = vec![user("m1", "🚀🚀🚀🚀🚀")];

    let title = derive(&events, Source::FirstPrompt, 3).expect("text yields a title");

    assert_eq!(title, "🚀🚀…");
    assert_eq!(title.chars().count(), 3);
}

#[test]
fn the_character_limit_is_enforced() {
    let events = vec![user("m1", "abcdefghijklmnopqrstuvwxyz")];

    let title = derive(&events, Source::FirstPrompt, 10).expect("text yields a title");

    assert_eq!(title, "abcdefghi…");
    assert_eq!(title.chars().count(), 10);
}

#[test]
fn a_zero_limit_yields_no_title() {
    let events = vec![user("m1", "anything")];

    assert_eq!(derive(&events, Source::FirstPrompt, 0), None);
}

#[test]
fn a_limit_of_one_leaves_only_the_ellipsis() {
    // The ellipsis is budgeted, so a one-character limit cannot produce two characters.
    let events = vec![user("m1", "anything")];

    assert_eq!(
        derive(&events, Source::FirstPrompt, 1).as_deref(),
        Some("…")
    );
}
