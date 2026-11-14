//! Tests for a session list preview.
//!
//! Four properties carry the weight. The snippet must be the *last* message with text — the whole
//! point of the field is "what were we just talking about", and a test that records one message proves
//! nothing. It must be one line, because a list item is one row and a pasted reply would push every
//! following row off the screen. It must be cut on `char` boundaries, or a Chinese row becomes
//! mojibake. And `preview_from_tail` must be honest about what a truncated slice cannot know: the
//! turn count is a lower bound and a title written before the slice is absent.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It resolves `girder_core::…` through
//! the alias the library declares for itself, which is why the same source builds here.

#[path = "../src/session_preview.rs"]
mod session_preview;

use girder_core::{
    ChatMessage, ChatRole, ContentBlock, MessageId, MessageSource, SessionEvent, ToolCallId,
    TurnEndReason,
};

use session_preview::{Preview, preview, preview_from_tail};

// ---- fixtures ----------------------------------------------------------------------------------

/// A prompt written by the person.
fn user(id: &str, text: &str) -> SessionEvent {
    SessionEvent::user_text(id, text)
}

/// An assistant reply.
fn answer(id: &str, text: &str) -> SessionEvent {
    SessionEvent::assistant_text(id, text, "deepseek")
}

/// An assistant message carrying a tool call and no text: the common shape mid-turn.
fn tool_call_block(id: &str, name: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        message: ChatMessage::new(
            MessageId::new(id).expect("valid id"),
            ChatRole::Assistant,
            MessageSource::Model {
                provider: "deepseek".to_owned(),
                model: "deepseek".to_owned(),
                replay_state: None,
            },
            vec![ContentBlock::ToolCall {
                id: ToolCallId::new(format!("c{id}")).expect("valid id"),
                name: name.to_owned(),
                arguments: "{}".to_owned(),
            }],
        ),
    }
}

/// A tool result, which is machine output rather than conversation.
fn tool_result(call: &str, text: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(call).expect("valid id"),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        is_error: false,
    }
}

/// A `session/title` record.
fn title(text: &str) -> SessionEvent {
    SessionEvent::SessionTitle {
        title: text.to_owned(),
        message_seqs: vec![1],
        source: "human".to_owned(),
    }
}

/// A `turn/start` boundary.
fn start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

/// A `turn/end` boundary.
fn end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    }
}

/// One complete turn: boundary, prompt, answer, close.
fn complete_turn(turn: u64, prompt: &str, reply: &str) -> Vec<SessionEvent> {
    vec![
        start(turn),
        user(&format!("m{turn}"), prompt),
        SessionEvent::StepStart { turn, step: 1 },
        answer(&format!("a{turn}"), reply),
        SessionEvent::StepEnd { turn, step: 1 },
        end(turn),
    ]
}

// ---- the basic preview -------------------------------------------------------------------------

#[test]
fn a_preview_carries_the_id_the_title_the_snippet_and_the_turn_count() {
    let events = vec![
        title("retry policy"),
        start(1),
        user("m1", "why does the release job fail"),
        answer("a1", "The second retry reuses a stale token."),
        end(1),
    ];

    let row = preview("s-1", &events, 120);

    assert_eq!(row.id, "s-1");
    assert_eq!(row.title.as_deref(), Some("retry policy"));
    assert_eq!(row.snippet, "The second retry reuses a stale token.");
    assert_eq!(row.turns, 1);
}

#[test]
fn an_untitled_session_has_no_title_rather_than_an_empty_one() {
    // "Untitled" and "titled with nothing" are different states, and only one of them wants a
    // placeholder. Absent is how a caller tells them apart.
    let events = vec![start(1), user("m1", "hello"), end(1)];

    assert_eq!(preview("s-1", &events, 80).title, None);
}

#[test]
fn the_last_title_wins_because_a_rename_appends() {
    // A rename appends a record rather than rewriting the old one, so the newest record is the name.
    let events = vec![title("first name"), title("second name"), user("m1", "hi")];

    assert_eq!(
        preview("s-1", &events, 80).title.as_deref(),
        Some("second name")
    );
}

#[test]
fn a_blank_title_is_not_a_title() {
    // A row labelled with whitespace is unreadable, which is the state the field exists to avoid.
    let events = vec![title("   "), user("m1", "hi")];

    assert_eq!(preview("s-1", &events, 80).title, None);
}

// ---- the snippet is the last message: the core property ----------------------------------------

#[test]
fn the_snippet_is_the_last_message_not_the_first() {
    // The entire value of the field depends on this. A preview that reported the opening prompt would
    // tell a returning reader what they already knew from the title, and nothing about what was said
    // afterwards.
    let events = vec![
        start(1),
        user("m1", "why does the release job fail"),
        answer("a1", "Looking at the logs now."),
        answer("a2", "The second retry reuses a stale token."),
        end(1),
    ];

    assert_eq!(
        preview("s-1", &events, 120).snippet,
        "The second retry reuses a stale token."
    );
}

#[test]
fn the_last_message_may_be_the_persons_prompt() {
    // A session interrupted while the person was still talking has no answer after their prompt. The
    // question the row answers is "what were we just talking about", so the prompt is the right quote.
    let events = vec![
        start(1),
        user("m1", "fix the parser"),
        answer("a1", "Done."),
        start(2),
        user("m2", "now the tokenizer please"),
    ];

    assert_eq!(
        preview("s-1", &events, 120).snippet,
        "now the tokenizer please"
    );
}

#[test]
fn a_trailing_tool_call_does_not_replace_the_snippet() {
    // A step that only calls a tool said nothing. Quoting it would blank the row at the moment the
    // session is closest to an answer.
    let events = vec![
        start(1),
        user("m1", "read the parser"),
        answer("a1", "The parser drops the last token."),
        tool_call_block("a2", "read_file"),
        end(1),
    ];

    assert_eq!(
        preview("s-1", &events, 120).snippet,
        "The parser drops the last token."
    );
}

#[test]
fn a_tool_result_is_not_conversation() {
    // Machine output the person never read: quoting it would put a file's contents in the row.
    let events = vec![
        start(1),
        user("m1", "read the parser"),
        answer("a1", "It drops the last token."),
        tool_result("c1", "fn parse(input: &str) { /* two hundred lines */ }"),
        end(1),
    ];

    assert_eq!(
        preview("s-1", &events, 120).snippet,
        "It drops the last token."
    );
}

#[test]
fn the_system_prompt_is_never_quoted() {
    // `system/message` is the prompt the harness assembles — thousands of characters of instructions.
    // It is the newest message in the log, and quoting it would make every row look identical.
    let events = vec![
        start(1),
        user("m1", "why is the build slow"),
        answer("a1", "The linker is the cost."),
        SessionEvent::SystemMessage {
            turn: 1,
            step: 1,
            message: ChatMessage::new(
                MessageId::new("s1").expect("valid id"),
                ChatRole::System,
                MessageSource::SystemPrompt,
                vec![ContentBlock::Text {
                    text: "You are a coding agent. Follow these rules…".to_owned(),
                }],
            ),
        },
    ];

    assert_eq!(
        preview("s-1", &events, 120).snippet,
        "The linker is the cost."
    );
}

// ---- truncation --------------------------------------------------------------------------------

#[test]
fn a_long_snippet_is_clipped_and_marked_with_an_ellipsis() {
    // An unbounded snippet would make every row as expensive to hold as the transcript it summarises,
    // and the cut must be visible or a clipped reply reads as a short one.
    let long = "word ".repeat(60);
    let events = vec![
        start(1),
        user("m1", "summarise"),
        answer("a1", &long),
        end(1),
    ];

    let row = preview("s-1", &events, 40);

    assert_eq!(row.snippet.chars().count(), 40);
    assert!(row.snippet.ends_with('…'));
}

#[test]
fn a_snippet_that_fits_is_not_clipped() {
    // The ellipsis means "there is more"; adding it to text that ended naturally is a lie.
    let events = vec![answer("a1", "Exactly forty characters, give or take a few")];

    let row = preview("s-1", &events, 60);

    assert!(!row.snippet.ends_with('…'));
    assert_eq!(row.snippet, "Exactly forty characters, give or take a few");
}

#[test]
fn a_title_is_bounded_by_the_same_budget_as_the_snippet() {
    // Both are drawn in one row. A stored title is user text with no length bound of its own, so the
    // clipping has to happen here rather than being trusted upstream.
    let events = vec![title(&"very long title ".repeat(20)), user("m1", "hi")];

    let row = preview("s-1", &events, 20);

    let stored = row.title.expect("a title was recorded");
    assert_eq!(stored.chars().count(), 20);
    assert!(stored.ends_with('…'));
}

#[test]
fn a_zero_budget_yields_an_empty_snippet_and_no_title() {
    // A caller that reserved no width gets no text, rather than an ellipsis with nothing behind it.
    let events = vec![title("a title"), user("m1", "some text")];

    let row = preview("s-1", &events, 0);

    assert_eq!(row.snippet, "");
    assert_eq!(row.title, None);
}

#[test]
fn chinese_text_is_cut_on_character_boundaries() {
    // A budget is a width a person judges a row by. Cutting bytes would give a Chinese row a third of a
    // Latin row's allowance, and slicing mid-character would put U+FFFD in the row.
    let events = vec![answer(
        "a1",
        "第二个重试复用了过期的令牌，所以发布任务在第二次尝试时失败了，需要刷新凭证。",
    )];

    let row = preview("s-1", &events, 10);

    assert_eq!(row.snippet.chars().count(), 10);
    assert!(row.snippet.ends_with('…'));
    // Every character survives intact: no replacement character, no partial sequence.
    assert!(!row.snippet.contains('\u{FFFD}'));
    assert!(row.snippet.starts_with("第二个重试复用"));
}

#[test]
fn a_chinese_snippet_that_fits_is_returned_whole() {
    // The counter must count characters, not bytes: this text is well over its length in bytes and
    // well under its length in characters.
    let text = "重试策略已修复";
    let events = vec![answer("a1", text)];

    assert_eq!(preview("s-1", &events, 20).snippet, text);
}

// ---- the single-line guarantee -----------------------------------------------------------------

#[test]
fn newlines_in_the_last_message_are_folded_to_spaces() {
    // A list item is one row. A wrapped reply rendered with its own newlines pushes every following
    // row out of alignment.
    let events = vec![answer(
        "a1",
        "Done: I capped the backoff.\nThe retry now waits 30s.\n\nSee the diff.",
    )];

    let row = preview("s-1", &events, 200);

    assert!(!row.snippet.contains('\n'));
    assert_eq!(
        row.snippet,
        "Done: I capped the backoff. The retry now waits 30s. See the diff."
    );
}

#[test]
fn indentation_and_tabs_collapse_rather_than_becoming_runs_of_spaces() {
    // A prompt pasted from a terminal arrives with indentation. Folding only the newline would leave
    // runs of spaces that shift the row's text off its baseline.
    let events = vec![user(
        "m1",
        "fn main() {\n\t    let x = 1;\n\t    println!(\"{x}\");\n}",
    )];

    let row = preview("s-1", &events, 200);

    assert!(!row.snippet.contains('\n'));
    assert!(!row.snippet.contains('\t'));
    assert!(!row.snippet.contains("  "));
    assert_eq!(row.snippet, "fn main() { let x = 1; println!(\"{x}\"); }");
}

#[test]
fn a_multiline_message_is_folded_before_it_is_clipped() {
    // The order matters. Clipping first would spend the budget on the first line and then fold the
    // newline; folding first makes the budget mean "characters a reader sees".
    let events = vec![answer("a1", "one\ntwo\nthree\nfour\nfive\nsix")];

    let row = preview("s-1", &events, 12);

    assert!(!row.snippet.contains('\n'));
    // Twelve characters: eleven of text plus the ellipsis, counted after the fold.
    assert_eq!(row.snippet, "one two thr…");
}

#[test]
fn two_text_blocks_in_one_message_are_separated_by_a_space() {
    // Blocks are two paragraphs; gluing them would weld the last word of one to the first of the next.
    let message = ChatMessage::new(
        MessageId::new("a1").expect("valid id"),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "deepseek".to_owned(),
            model: "deepseek".to_owned(),
            replay_state: None,
        },
        vec![
            ContentBlock::Text {
                text: "First paragraph.".to_owned(),
            },
            ContentBlock::Text {
                text: "Second paragraph.".to_owned(),
            },
        ],
    );

    let row = preview("s-1", &[SessionEvent::AssistantMessage { message }], 100);

    assert_eq!(row.snippet, "First paragraph. Second paragraph.");
}

// ---- empty and textless logs -------------------------------------------------------------------

#[test]
fn an_empty_log_yields_an_empty_preview() {
    // A session created but never used still gets a row. Every field must be a value, not a panic.
    let row = preview("s-1", &[], 80);

    assert_eq!(
        row,
        Preview {
            id: "s-1".to_owned(),
            title: None,
            snippet: String::new(),
            turns: 0,
            at_ms: 0,
        }
    );
}

#[test]
fn a_log_with_no_text_has_no_snippet() {
    // Only boundaries and accounting: there is nothing to quote, and an empty string is the honest
    // answer for a row that has no subtitle.
    let events = vec![
        start(1),
        SessionEvent::ModelUsage {
            model: "deepseek".to_owned(),
            input_tokens: 10,
            output_tokens: 4,
        },
        end(1),
    ];

    let row = preview("s-1", &events, 80);

    assert_eq!(row.snippet, "");
    assert_eq!(row.turns, 1);
}

#[test]
fn a_message_whose_text_is_only_whitespace_is_skipped() {
    // A blank message would render as an empty subtitle. Falling back to the previous message is what
    // the backward walk gives for free.
    let events = vec![answer("a1", "an actual reply"), answer("a2", "   \n\t  ")];

    assert_eq!(preview("s-1", &events, 80).snippet, "an actual reply");
}

// ---- turns: what the count claims --------------------------------------------------------------

#[test]
fn turns_are_counted_from_distinct_turn_numbers() {
    // A resumed or duplicated log repeats a boundary; counting records rather than turn numbers would
    // report one turn as two.
    let mut events = complete_turn(1, "one", "first");
    events.extend(complete_turn(2, "two", "second"));
    events.push(start(2));

    assert_eq!(preview("s-1", &events, 80).turns, 2);
}

#[test]
fn a_turn_is_counted_from_its_steps_even_when_its_boundary_is_absent() {
    // A tail can begin mid-turn. The steps and messages still name their turn, so the count does not
    // need the `turn/start` record — which is what makes a tail-only preview worth anything.
    let events = vec![
        SessionEvent::StepStart { turn: 7, step: 2 },
        answer("a7", "now the tokenizer"),
        SessionEvent::StepEnd { turn: 7, step: 2 },
        SessionEvent::TurnEnd {
            turn: 7,
            reason: TurnEndReason::Completed,
        },
    ];

    assert_eq!(preview("s-1", &events, 80).turns, 1);
}

#[test]
fn a_log_with_prompts_and_no_boundaries_still_counts_its_turns() {
    // The runtime writes the prompt before `turn/start` so a prompt survives a crash before the model
    // answers. A session killed in between holds prompts and no boundaries, and reporting zero turns
    // for it would misdescribe a session that plainly happened.
    let events = vec![user("m1", "first"), user("m2", "second")];

    assert_eq!(preview("s-1", &events, 80).turns, 2);
}

#[test]
fn synthetic_user_messages_do_not_inflate_the_fallback_count() {
    // This build writes goal continuations and compaction recalls on the user role. Counting them as
    // turns would report a session that never moved as a long one.
    let events = vec![
        user("m1", "a real question"),
        SessionEvent::UserMessage {
            message: ChatMessage::new(
                MessageId::new("m2").expect("valid id"),
                ChatRole::User,
                MessageSource::User {
                    form: Some(girder_core::ContextForm::Recall),
                },
                vec![ContentBlock::Text {
                    text: "recalled context".to_owned(),
                }],
            ),
        },
    ];

    assert_eq!(preview("s-1", &events, 80).turns, 1);
}

#[test]
fn turns_still_count_boundaries_when_a_prompt_names_no_turn() {
    // The fallback is only a fallback: once any event names a turn, the boundary count is the answer
    // and prompts are not counted on top of it.
    let events = vec![start(1), user("m1", "one"), user("m2", "two"), end(1)];

    assert_eq!(preview("s-1", &events, 80).turns, 1);
}

// ---- preview_from_tail: the cost property ------------------------------------------------------

#[test]
fn a_tail_of_the_same_log_yields_the_same_snippet() {
    // This is the whole reason the function exists: a caller that read only the last few events must
    // get the row a full read would have produced. Five events is about one turn.
    let full = complete_turn(
        1,
        "why does the release job fail",
        "the second retry reuses a stale token",
    );
    let tail = &full[full.len() - 4..];

    assert_eq!(
        preview("s-1", &full, 120).snippet,
        preview_from_tail("s-1", tail, 120).snippet
    );
}

#[test]
fn a_tail_reports_a_lower_bound_on_turns_not_a_total() {
    // The turns before the slice are absent, so the count cannot see them. The field documents this;
    // the test pins the documented behaviour so it cannot drift into an implied total.
    let mut full = complete_turn(1, "one", "first");
    full.extend(complete_turn(2, "two", "second"));
    full.extend(complete_turn(3, "three", "third"));

    let whole = preview("s-1", &full, 120);
    let tail = preview_from_tail("s-1", &full[full.len() - 6..], 120);

    assert_eq!(whole.turns, 3);
    assert_eq!(tail.turns, 1);
    // And the snippet is still right, because the last turn is what the tail holds.
    assert_eq!(tail.snippet, "third");
}

#[test]
fn a_tail_that_misses_the_title_reports_no_title() {
    // A title is written once, near the start. A short tail usually misses it, and inventing one would
    // duplicate the policy `session_title` owns.
    let mut full = vec![title("retry policy")];
    full.extend(complete_turn(1, "one", "first"));

    let tail = preview_from_tail("s-1", &full[full.len() - 3..], 120);

    assert_eq!(tail.title, None);
    assert_eq!(tail.snippet, "first");
}

#[test]
fn an_empty_tail_is_an_empty_preview() {
    // Reading the tail of an empty log is not an error; it is a row for a session with nothing in it.
    assert_eq!(preview_from_tail("s-1", &[], 80), preview("s-1", &[], 80));
}

// ---- at_ms: what the log can and cannot say -----------------------------------------------------

#[test]
fn at_ms_is_zero_because_the_log_records_no_wall_clock_time() {
    // Ordering is recorded; time is not. A caller with a catalog row assigns the row's `updated_at`.
    // Inventing a timestamp here would make two hosts disagree about the same session.
    let events = complete_turn(1, "one", "first");

    assert_eq!(preview("s-1", &events, 80).at_ms, 0);
    assert_eq!(preview_from_tail("s-1", &events, 80).at_ms, 0);
}
