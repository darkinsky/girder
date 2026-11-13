//! Tests for folding a session log into a per-turn timeline.
//!
//! Three properties carry the weight here. A segment must be one turn and must name the turn the log
//! named, because a client matches a row back to events by that number. The summary must come only
//! from the prompt and the tools, because there is no model in this path and pretending otherwise
//! would make the output untestable. And an unfinished turn must still be a segment: the live turn is
//! exactly the row a sidebar is watched for, so dropping it would fail at the one moment it matters.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It resolves `girder_core::…` through
//! the alias the library declares for itself, which is why the same source builds here.

#[path = "../src/session_timeline.rs"]
mod session_timeline;

use girder_core::{
    ChatMessage, ChatRole, ContextForm, MessageId, MessageSource, SessionEvent, ToolCallId,
    TurnEndReason,
};

use session_timeline::{Segment, open_turn, outline};

// ---- fixtures ----------------------------------------------------------------------------------

/// A prompt the person typed.
fn user(id: &str, text: &str) -> SessionEvent {
    SessionEvent::user_text(id, text)
}

/// A user-role message the harness wrote, which must never become a summary.
fn injected(id: &str, text: &str, form: ContextForm) -> SessionEvent {
    SessionEvent::UserMessage {
        message: ChatMessage::new(
            MessageId::new(id).expect("valid id"),
            ChatRole::User,
            MessageSource::User { form: Some(form) },
            vec![girder_core::ContentBlock::Text {
                text: text.to_owned(),
            }],
        ),
    }
}

/// A tool invocation.
fn tool(call: &str, name: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(call).expect("valid id"),
        name: name.to_owned(),
        arguments: "{}".to_owned(),
    }
}

/// One complete turn: boundary, prompt, steps, tools, end.
fn complete_turn(turn: u64, prompt: &str, tool_names: &[&str], steps: u64) -> Vec<SessionEvent> {
    let mut events = vec![
        SessionEvent::TurnStart { turn },
        user(&format!("m{turn}"), prompt),
    ];
    for step in 1..=steps {
        events.push(SessionEvent::StepStart { turn, step });
        events.push(SessionEvent::StepEnd { turn, step });
    }
    for (index, name) in tool_names.iter().enumerate() {
        events.push(tool(&format!("c{turn}-{index}"), name));
    }
    events.push(SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    });
    events
}

/// The segment for one turn, or a panic naming what was found instead.
fn segment(segments: &[Segment], turn: u64) -> &Segment {
    segments
        .iter()
        .find(|segment| segment.turn == turn)
        .unwrap_or_else(|| panic!("no segment for turn {turn}: {segments:?}"))
}

// ---- one turn ----------------------------------------------------------------------------------

#[test]
fn a_single_turn_becomes_one_segment() {
    let events = complete_turn(1, "fix the retry", &["bash"], 1);
    let segments = outline(&events);
    assert_eq!(segments.len(), 1);
    let only = &segments[0];
    assert_eq!(only.turn, 1);
    assert_eq!(only.steps, 1);
    assert_eq!(only.tools, vec!["bash".to_owned()]);
    assert_eq!(only.summary, "fix the retry (tools: bash)");
}

#[test]
fn a_turn_names_the_number_the_log_named_and_survives_an_unnumbered_format() {
    // Turn numbers are not positions: a resumed log can start at 7, and renumbering would make the
    // row impossible to match against the events behind it.
    let events = complete_turn(7, "and the backoff", &[], 2);
    let segments = outline(&events);
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].turn, 7);
    // No tools, so the summary is the request alone rather than "(tools: )".
    assert_eq!(segments[0].summary, "and the backoff");
}

// ---- several turns -----------------------------------------------------------------------------

#[test]
fn every_turn_becomes_its_own_segment_in_log_order() {
    let mut events = complete_turn(1, "read the parser", &["read_file"], 1);
    events.extend(complete_turn(
        2,
        "now add a test",
        &["edit_file", "bash"],
        2,
    ));

    let segments = outline(&events);

    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].turn, 1);
    assert_eq!(segments[0].summary, "read the parser (tools: read_file)");
    assert_eq!(segments[1].turn, 2);
    assert_eq!(
        segments[1].summary,
        "now add a test (tools: edit_file, bash)"
    );
    assert_eq!(segments[1].steps, 2);
}

#[test]
fn a_new_boundary_closes_a_turn_that_never_ended() {
    // The only evidence that the first turn stopped is that the second one started. Dropping it
    // would lose a whole row because the log is missing one record.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "start something"),
        SessionEvent::TurnStart { turn: 2 },
        user("m2", "and something else"),
    ];

    let segments = outline(&events);

    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].summary, "start something");
    assert_eq!(segments[1].summary, "and something else");
    // Only the second is still in flight; the first was closed by the boundary.
    assert_eq!(open_turn(&events), Some(2));
}

#[test]
fn a_repeated_start_for_the_open_turn_does_not_split_the_row() {
    // The loop has written two `turn/start` records for one turn before (documented in
    // `docs/migration-v2.md`); splitting would report one turn as two rows with half the tools each.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "one turn"),
        tool("c1", "bash"),
    ];

    let segments = outline(&events);

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].tools, vec!["bash".to_owned()]);
}

// ---- tools -------------------------------------------------------------------------------------

#[test]
fn a_tool_used_twice_is_listed_once() {
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "read both files"),
        tool("c1", "read_file"),
        tool("c2", "read_file"),
        tool("c3", "bash"),
        tool("c4", "read_file"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(
        segments[0].tools,
        vec!["read_file".to_owned(), "bash".to_owned()]
    );
    // Deduplicated in the row too: naming one tool twice would spend the row's budget on a repeat.
    assert_eq!(
        segments[0].summary,
        "read both files (tools: read_file, bash)"
    );
}

#[test]
fn tools_keep_first_use_order_because_the_order_is_the_story() {
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "find it then fix it"),
        tool("c1", "grep"),
        tool("c2", "read_file"),
        tool("c3", "edit_file"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(
        segments[0].tools,
        vec![
            "grep".to_owned(),
            "read_file".to_owned(),
            "edit_file".to_owned()
        ]
    );
}

#[test]
fn a_long_tool_list_is_summarised_but_kept_whole() {
    let names = [
        "read_file",
        "grep",
        "bash",
        "edit_file",
        "write_file",
        "glob",
        "web_fetch",
    ];
    let events = complete_turn(1, "do a lot", &names, 4);

    let segments = outline(&events);

    // The row names three and counts the rest, so a busy turn stays one readable line.
    assert_eq!(
        segments[0].summary,
        "do a lot (tools: read_file, grep, bash and 4 more)"
    );
    // The data is not lost: a caller that wants the full list has it.
    assert_eq!(segments[0].tools.len(), names.len());
}

// ---- steps -------------------------------------------------------------------------------------

#[test]
fn steps_counts_distinct_started_steps() {
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "three steps"),
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::StepStart { turn: 1, step: 2 },
        SessionEvent::StepEnd { turn: 1, step: 2 },
        SessionEvent::StepStart { turn: 1, step: 3 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(segments[0].steps, 3);
}

#[test]
fn a_duplicated_step_start_counts_once() {
    // A retried write can record one step twice. Counting records rather than step numbers would
    // report work that did not happen.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "one step, written twice"),
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
    ];

    let segments = outline(&events);

    assert_eq!(segments[0].steps, 1);
}

#[test]
fn a_step_numbered_for_another_turn_is_not_counted_here() {
    // The step names its own turn, so it is attributed by that number rather than by whichever turn
    // happens to be open at that position in the log.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "mine"),
        SessionEvent::StepStart { turn: 2, step: 1 },
        SessionEvent::StepEnd { turn: 2, step: 1 },
    ];

    let segments = outline(&events);

    assert_eq!(segments[0].steps, 0);
}

// ---- the unfinished turn -----------------------------------------------------------------------

#[test]
fn an_unfinished_turn_is_still_a_segment() {
    // The core case. A turn that has a start and no end is the turn running right now, or one whose
    // process died mid-step; either way the user is looking at it, and folding only on `turn/end`
    // would make it vanish from the sidebar at exactly that moment.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "refactor the parser"),
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::StepStart { turn: 1, step: 2 },
        tool("c1", "edit_file"),
        // No `turn/end`: the model is still working.
    ];

    let segments = outline(&events);

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].turn, 1);
    // Partial, not final: one step finished and a second started.
    assert_eq!(segments[0].steps, 2);
    assert_eq!(segments[0].tools, vec!["edit_file".to_owned()]);
    assert_eq!(
        segments[0].summary,
        "refactor the parser (tools: edit_file)"
    );
    // And a client can tell this row is the live one rather than inferring it from the summary.
    assert_eq!(open_turn(&events), Some(1));
}

#[test]
fn a_just_opened_turn_has_an_empty_summary_rather_than_a_placeholder() {
    // Nothing has happened yet: no prompt, no tool, no step. An empty string is the honest answer,
    // and the caller words it for its own surface instead of parsing a placeholder back out.
    let events = vec![SessionEvent::TurnStart { turn: 1 }];

    let segments = outline(&events);

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].summary, "");
    assert_eq!(segments[0].steps, 0);
    assert!(segments[0].tools.is_empty());
    assert_eq!(open_turn(&events), Some(1));
}

#[test]
fn open_turn_is_none_once_the_turn_ended() {
    let events = complete_turn(1, "finished", &[], 1);
    assert_eq!(open_turn(&events), None);
    assert_eq!(open_turn(&[]), None);
}

// ---- turns without a prompt --------------------------------------------------------------------

#[test]
fn a_turn_with_no_user_message_says_which_tools_it_used() {
    // A continuation turn carries no prompt of its own: the model kept working on what was already
    // asked. The tools are the only evidence of what it did, so they become the summary.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        tool("c1", "bash"),
        tool("c2", "read_file"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].summary, "Used bash, read_file");
}

#[test]
fn a_turn_with_no_prompt_and_no_tools_reports_its_steps() {
    // The model answered without calling anything, which is real work even though nothing can be
    // quoted. Saying "3 steps, no tool calls" does not claim what it said — the text is not read.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepStart { turn: 1, step: 2 },
        SessionEvent::StepStart { turn: 1, step: 3 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Blocked,
        },
    ];

    let segments = outline(&events);

    assert_eq!(segments[0].summary, "3 steps, no tool calls");
}

// ---- which prompts count -----------------------------------------------------------------------

#[test]
fn a_prompt_written_before_its_turn_start_still_belongs_to_the_turn() {
    // This build records the prompt before spawning the loop, so `user/message` precedes
    // `turn/start` — the reverse of upstream. Attribution follows log position only in the sense of
    // "the boundary that owns it"; both orders must produce the same row.
    let events = vec![
        user("m1", "the prompt lands first"),
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].summary, "the prompt lands first");
}

#[test]
fn a_harness_written_user_message_is_not_treated_as_a_request() {
    // Goal continuations, compaction recalls, and relayed peer messages all carry the user role. A
    // summary built from one would describe the harness's bookkeeping as if the person had asked for
    // it, so only `User { form: None }` counts.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        injected(
            "m1",
            "Continue working toward the goal. You have used 2/8 rounds.",
            ContextForm::Notice,
        ),
        tool("c1", "bash"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(segments[0].summary, "Used bash");
    assert!(!segments[0].summary.contains("Continue working"));
}

#[test]
fn only_the_first_prompt_of_a_turn_names_it() {
    // Later human messages in the same turn are steering the work already under way. Replacing the
    // summary with the newest one would rename the row mid-turn, and a reader watching the sidebar
    // would see the row they clicked change into a different one.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "fix the retry loop"),
        SessionEvent::StepStart { turn: 1, step: 1 },
        user("m2", "actually do it in the client too"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(segments[0].summary, "fix the retry loop");
}

#[test]
fn a_blank_prompt_does_not_shadow_the_real_one_behind_it() {
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "   \n\t "),
        user("m2", "the real request"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(segments[0].summary, "the real request");
}

// ---- the summary is one line -------------------------------------------------------------------

#[test]
fn a_multi_line_prompt_becomes_one_line() {
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "first line\n\n   second line\twith tabs"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    assert_eq!(segments[0].summary, "first line second line with tabs");
}

#[test]
fn a_long_prompt_is_clipped_and_still_names_its_tools() {
    let long = "a".repeat(400);
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", &long),
        tool("c1", "bash"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    let summary = &segments[0].summary;
    assert!(
        summary.chars().count() <= 96,
        "summary must fit one row, got {} chars",
        summary.chars().count()
    );
    assert!(summary.ends_with('…') || summary.contains("… (tools: bash)"));
    // The tool suffix sits outside the clipped prompt, so the row still says what ran.
    assert!(summary.contains("bash"), "got {summary}");
}

#[test]
fn clipping_never_splits_a_character() {
    // A summary is displayed verbatim, so a budget that cut a multi-byte character in half would
    // produce something that is not the text it came from.
    let chinese = "修复重试逻辑".repeat(40);
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", &chinese),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let segments = outline(&events);

    let summary = &segments[0].summary;
    assert!(summary.chars().count() <= 96);
    assert!(summary.ends_with('…'));
    // Reading every character proves the string is valid and was cut on boundaries.
    assert!(summary.chars().all(|character| character != '\u{FFFD}'));
}

// ---- empty and unattributable input ------------------------------------------------------------

#[test]
fn an_empty_log_has_no_timeline() {
    // A session that never ran did nothing. A placeholder row would be the first thing a user sees on
    // a brand-new session, claiming activity that never happened.
    assert!(outline(&[]).is_empty());
}

#[test]
fn a_tool_call_outside_any_turn_is_not_attributed_to_one() {
    // Guessing "the nearest turn" would put the next turn's first tool call on the previous turn's
    // row, which is the mistake a timeline exists to prevent.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "one"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
        tool("c1", "bash"),
    ];

    let segments = outline(&events);

    assert_eq!(segments.len(), 1);
    assert!(segments[0].tools.is_empty());
    assert_eq!(segments[0].summary, "one");
}

#[test]
fn metadata_events_do_not_disturb_the_timeline() {
    // Usage, selection, presets, and retries describe how a turn ran, not what it did. Folding them in
    // would make the summary depend on accounting rather than on the work.
    let mut events = vec![
        SessionEvent::TurnStart { turn: 1 },
        user("m1", "just run it"),
        SessionEvent::ModelSelection {
            selection: girder_core::ModelSelection::new("deepseek", "v4"),
        },
        SessionEvent::LlmRetry {
            attempt: 2,
            reason: Some("timeout".to_owned()),
        },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::assistant_text("a1", "Running it now.", "deepseek"),
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];
    events.push(SessionEvent::PlanMode { active: true });
    events.push(SessionEvent::SandboxMode {
        mode: "workspace-write".to_owned(),
        source: None,
    });

    let segments = outline(&events);

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].summary, "just run it");
    assert_eq!(segments[0].steps, 1);
    let _ = segment(&segments, 1);
}
