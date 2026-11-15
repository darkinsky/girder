//! Tests for compacting a session log.
//!
//! The properties pinned here are the ones that make the feature safe rather than the ones that make
//! it useful. A log is the source of truth for every derived view, so the interesting question is not
//! "did it get smaller" but "did anything change" — and the answers that must be *no* are the tool
//! exchanges, the turn boundaries, and the prompts. Those are tested first and from both directions:
//! the events themselves survive, and nothing surrounding them moves.
//!
//! Playback equivalence is the test that actually justifies the module. Three rules removing events is
//! only defensible if the projections a reader actually consumes are unchanged, so the same log is
//! folded twice — once as written and once compacted — through the transcript export, the digest, the
//! timeline, and the final-output selector, and the two fold results are compared.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/log_compact.rs"]
mod log_compact;

use girder_core::{
    ChatMessage, ChatRole, ContentBlock, MessageSource, SessionEvent, SessionStats, ToolCallId,
    TranscriptFormat, TurnEndReason, digest_session, export_transcript, final_text,
    session_outline,
};
use log_compact::{compact, is_compactable};

/// A message id that is valid by construction.
fn id(value: &str) -> girder_core::MessageId {
    girder_core::MessageId::new(value).unwrap_or_else(|_| {
        girder_core::MessageId::new("msg-fallback").expect("the fallback literal is a valid id")
    })
}

/// A tool call id that is valid by construction.
fn call_id(value: &str) -> ToolCallId {
    ToolCallId::new(value).unwrap_or_else(|_| {
        ToolCallId::new("call-fallback").expect("the fallback literal is a valid id")
    })
}

/// A prompt.
fn user(text: &str) -> SessionEvent {
    SessionEvent::user_text("msg-u", text)
}

/// The model's answer.
fn assistant(text: &str) -> SessionEvent {
    SessionEvent::assistant_text("msg-a", text, "test")
}

/// An assistant message with exactly the blocks given, bypassing the text constructor.
fn assistant_blocks(blocks: Vec<ContentBlock>) -> SessionEvent {
    SessionEvent::AssistantMessage {
        message: ChatMessage::new(
            id("msg-a"),
            ChatRole::Assistant,
            MessageSource::Model {
                provider: "test".to_owned(),
                model: "test".to_owned(),
                replay_state: None,
            },
            blocks,
        ),
    }
}

/// An assistant message consisting of one space-only text block.
fn assistant_blank() -> SessionEvent {
    assistant_blocks(vec![ContentBlock::Text {
        text: "   ".to_owned(),
    }])
}

/// An assistant message with no content blocks at all.
fn assistant_no_blocks() -> SessionEvent {
    assistant_blocks(Vec::new())
}

/// A usage record.
fn usage(model: &str, input: u64, output: u64) -> SessionEvent {
    SessionEvent::ModelUsage {
        model: model.to_owned(),
        input_tokens: input,
        output_tokens: output,
    }
}

/// A tool invocation.
fn tool_call(call: &str, name: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: call_id(call),
        name: name.to_owned(),
        arguments: "{\"path\":\"a.txt\"}".to_owned(),
    }
}

/// A tool result.
fn tool_result(call: &str, text: &str) -> SessionEvent {
    SessionEvent::ToolResult {
        id: call_id(call),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        is_error: false,
    }
}

/// A title record.
fn title(text: &str, source: &str) -> SessionEvent {
    SessionEvent::SessionTitle {
        title: text.to_owned(),
        message_seqs: vec![1],
        source: source.to_owned(),
    }
}

/// A turn boundary pair.
fn turn(number: u64) -> [SessionEvent; 2] {
    [
        SessionEvent::TurnStart { turn: number },
        SessionEvent::TurnEnd {
            turn: number,
            reason: TurnEndReason::Completed,
        },
    ]
}

/// A step boundary pair.
fn step(number: u64, step: u64) -> [SessionEvent; 2] {
    [
        SessionEvent::StepStart { turn: number, step },
        SessionEvent::StepEnd { turn: number, step },
    ]
}

/// A realistic turn: prompt, boundaries, a tool exchange, and an answer.
///
/// Used by the equivalence tests as the shape that *must not* be touched, so a change to any of the
/// rules is measured against a turn that contains one of everything.
fn one_tool_turn(number: u64) -> Vec<SessionEvent> {
    let mut events = vec![user("read a.txt")];
    events.extend(turn(number));
    events.extend(step(number, 1));
    events.push(assistant_blocks(vec![ContentBlock::ToolCall {
        id: call_id(&format!("call-{number}")),
        name: "read_file".to_owned(),
        arguments: "{\"path\":\"a.txt\"}".to_owned(),
    }]));
    events.push(tool_call(&format!("call-{number}"), "read_file"));
    events.push(tool_result(&format!("call-{number}"), "contents"));
    events.push(assistant("the file says contents"));
    events.extend(step(number, 2));
    events
}

// ---- what must never be compressed ------------------------------------------------------------

#[test]
fn a_tool_exchange_is_never_compacted() {
    // The core safety property. A call and its result are the only record of what the tool did, and a
    // result cannot be reconstructed from anything else in the log, so this rule has no exceptions —
    // not even for a call whose arguments are identical to the one before it.
    let events = vec![
        user("go"),
        SessionEvent::TurnStart { turn: 1 },
        tool_call("call-1", "bash"),
        tool_result("call-1", "one"),
        tool_call("call-1", "bash"),
        tool_result("call-1", "one"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
    assert!(compacted.reasons.is_empty());
    assert!(!is_compactable(&events));
}

#[test]
fn an_unanswered_tool_call_is_kept() {
    // The log records that the call happened even though no result arrived. Dropping it would turn a
    // session that crashed mid-call into one that never called anything, which is the opposite of
    // what an investigation needs.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        tool_call("call-1", "bash"),
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn turn_boundaries_are_never_compacted() {
    // Boundaries are what make a log resumable and what every fold uses to attribute an event to a
    // turn. A duplicated boundary is already deduplicated by the folds that care, so removing one
    // would save a line and risk orphaning everything between the pair.
    let mut events = Vec::new();
    events.extend(turn(1));
    events.extend(turn(1));
    events.extend(turn(2));
    events.push(user("again"));
    events.extend(turn(2));

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn step_boundaries_are_never_compacted() {
    // In particular an *unpaired* `step/start` stays. It is how a cancelled or crashed step is
    // visible: `agent_loop` returns before writing the matching `step/end` on an error path, and
    // `session_health` reports the result. It looks like garbage and is evidence.
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::StepStart { turn: 1, step: 2 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Blocked,
        },
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn user_messages_are_never_compacted() {
    // Including a prompt repeated verbatim: asking the same thing twice is two requests, and merging
    // them would misreport how many times the person asked.
    let events = vec![
        user("hello"),
        user("hello"),
        SessionEvent::TurnStart { turn: 1 },
        user("   "),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn a_user_role_message_inside_an_assistant_record_is_kept() {
    // A hand-edited or migrated log can put a user-role message in an `assistant/message` record. The
    // empty-message rule keys on an empty *content vector*, and this one is non-empty, but the role is
    // also checked so that a future loosening of the emptiness test cannot start deleting prompts.
    let events = vec![SessionEvent::AssistantMessage {
        message: ChatMessage::new(
            id("msg-mixed"),
            ChatRole::User,
            MessageSource::User { form: None },
            vec![ContentBlock::Text {
                text: "do not lose me".to_owned(),
            }],
        ),
    }];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

// ---- what may be compressed -------------------------------------------------------------------

#[test]
fn consecutive_identical_usage_records_merge_into_the_last_one() {
    // Adjacent and identical is the signature of one call recorded twice. The last of the run is kept,
    // because it is the write that was affirmed most recently.
    let events = vec![
        usage("deepseek-chat", 10, 20),
        usage("deepseek-chat", 10, 20),
        usage("deepseek-chat", 10, 20),
        user("next"),
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.removed, 2);
    assert_eq!(compacted.events.len(), 2);
    assert_eq!(compacted.events[0], usage("deepseek-chat", 10, 20));
    assert_eq!(compacted.events[1], user("next"));
    assert_eq!(compacted.reasons.len(), 1);
    assert!(compacted.reasons[0].contains("model/usage"));
    assert!(compacted.reasons[0].contains('2'));
}

#[test]
fn usage_records_that_differ_are_two_calls_and_both_stay() {
    // A different model or a different token count is a different call. Only an exact neighbour can be
    // a rewrite, so nothing here is removable.
    let events = vec![
        usage("deepseek-chat", 10, 20),
        usage("deepseek-chat", 11, 20),
        usage("deepseek-reasoner", 10, 20),
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn identical_usage_records_separated_by_an_event_both_stay() {
    // Same numbers, but a real event happened in between, so these are two calls that cost the same.
    // Consecutiveness is what distinguishes a duplicate write from a coincidence.
    let events = vec![
        usage("deepseek-chat", 10, 20),
        assistant("two calls cost the same"),
        usage("deepseek-chat", 10, 20),
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn an_empty_assistant_message_is_removed() {
    // The message exists so a usage record has an owner; it contributes nothing to the conversation.
    // It is removed here because a later assistant message still carries content.
    let events = vec![
        assistant_no_blocks(),
        assistant("the real answer"),
        assistant_blank(),
        assistant("and another"),
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.removed, 2);
    assert_eq!(
        compacted.events,
        vec![assistant("the real answer"), assistant("and another")]
    );
    assert_eq!(compacted.reasons.len(), 1);
    assert!(compacted.reasons[0].contains("assistant/message"));
}

#[test]
fn a_trailing_empty_assistant_message_is_kept() {
    // `final_output` selects the last assistant message with a non-empty content vector and reads its
    // text. Removing this one would promote an earlier answer into "what the model said last", which
    // changes the conversation rather than shortening the log.
    let events = vec![assistant("the earlier answer"), assistant_blank()];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn a_trailing_assistant_message_with_no_blocks_is_removed() {
    // The other half of the distinction above, and the reason the rule asks two questions rather than
    // one. A message with *no blocks* is invisible to `final_output`, which requires a non-empty
    // content vector, so it can never be the "last word" and removing it is safe wherever it sits. A
    // message whose blocks are whitespace text is visible to that selector even though it reads as
    // nothing, which is why it is only removed once something later supersedes it.
    let events = vec![assistant("the earlier answer"), assistant_no_blocks()];

    let compacted = compact(&events);

    assert_eq!(compacted.removed, 1);
    assert_eq!(compacted.events, vec![assistant("the earlier answer")]);
    assert_eq!(playback(&compacted.events), playback(&events));
}

#[test]
fn an_assistant_message_with_only_a_tool_call_is_not_empty() {
    // The call is the request half of an exchange. Treating it as an empty message would delete the
    // request while leaving the result, which no reader can reconstruct.
    let events = vec![assistant_blocks(vec![ContentBlock::ToolCall {
        id: call_id("call-1"),
        name: "bash".to_owned(),
        arguments: "{}".to_owned(),
    }])];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn an_assistant_message_with_only_reasoning_is_not_empty() {
    // Reasoning is what the model actually produced. It is not rendered as conversation, but it is
    // content, and this module does not get to decide it never happened.
    let events = vec![assistant_blocks(vec![ContentBlock::Reasoning {
        text: "thinking it over".to_owned(),
    }])];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn an_earlier_title_duplicating_a_later_one_is_removed() {
    // Every title reader takes the last title in the log, so an earlier record stating the same text
    // is invisible to all of them.
    let events = vec![
        title("release triage", "model"),
        title("something else", "human"),
        title("release triage", "human"),
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.removed, 1);
    assert_eq!(
        compacted.events,
        vec![
            title("something else", "human"),
            title("release triage", "human")
        ]
    );
    assert_eq!(compacted.reasons.len(), 1);
    assert!(compacted.reasons[0].contains("session/title"));
}

#[test]
fn a_rename_chain_is_preserved() {
    // Different texts are different titles, and the sequence of what a session was called is history
    // rather than redundancy.
    let events = vec![
        title("first name", "model"),
        title("second name", "human"),
        title("third name", "human"),
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
}

#[test]
fn the_same_title_announced_twice_in_a_row_keeps_one() {
    // A repeated announce is the simplest form of the rule and the one a resumed write produces.
    let events = vec![title("only name", "model"), title("only name", "model")];

    let compacted = compact(&events);

    assert_eq!(compacted.removed, 1);
    assert_eq!(compacted.events, vec![title("only name", "model")]);
}

// ---- playback equivalence ----------------------------------------------------------------------

/// Every projection a reader consumes, folded once.
#[derive(Debug, PartialEq, Eq)]
struct Playback {
    transcript: Vec<(String, String)>,
    digest: String,
    outline: Vec<(u64, String, u32)>,
    final_text: Option<String>,
    turns: u64,
    steps: u64,
    tool_calls: u64,
}

/// Folds a log into every derived view that defines what replaying it means.
fn playback(events: &[SessionEvent]) -> Playback {
    let transcript = export_transcript(events, TranscriptFormat::OpenAi)
        .messages
        .into_iter()
        .map(|message| (message.role, message.content))
        .collect();
    let digest = digest_session(events);
    let outline = session_outline(events)
        .into_iter()
        .map(|segment| (segment.turn, segment.summary, segment.steps))
        .collect();
    let stats = SessionStats::of(events);
    Playback {
        transcript,
        digest: format!("{digest:?}"),
        outline,
        final_text: final_text(events),
        turns: stats.turns,
        steps: stats.steps,
        tool_calls: stats.tool_calls,
    }
}

/// A log containing one removable record of every kind.
fn mixed_log() -> Vec<SessionEvent> {
    let mut events = Vec::new();
    events.push(title("triage", "model"));
    events.push(title("triage", "model"));
    events.extend(turn(1));
    events.extend(step(1, 1));
    events.push(assistant_blocks(vec![ContentBlock::ToolCall {
        id: call_id("call-1"),
        name: "read_file".to_owned(),
        arguments: "{\"path\":\"a.txt\"}".to_owned(),
    }]));
    events.push(tool_call("call-1", "read_file"));
    events.push(tool_result("call-1", "contents"));
    events.push(usage("deepseek-chat", 12, 34));
    events.push(usage("deepseek-chat", 12, 34));
    events.push(assistant_blank());
    events.push(assistant("the answer"));
    events.extend(step(1, 2));
    events.extend(turn(2));
    events.extend(step(2, 1));
    events.push(user("follow up"));
    events.push(assistant_no_blocks());
    events.push(assistant("the second answer"));
    events.extend(step(2, 2));
    events
}

#[test]
fn playback_is_unchanged_by_compaction() {
    // The property the whole module rests on. If this fails, no count of removed events makes the
    // change acceptable.
    let events = mixed_log();

    let compacted = compact(&events);

    assert!(
        compacted.removed > 0,
        "the fixture must actually exercise every rule"
    );
    assert_eq!(playback(&compacted.events), playback(&events));
}

#[test]
fn playback_is_unchanged_on_a_log_with_a_broken_turn() {
    // A cancelled turn leaves an unpaired step. Compaction must not "repair" it into something that
    // replays as a clean turn, because the difference is the whole point of recording it.
    let mut events = Vec::new();
    events.extend(turn(1));
    events.extend(step(1, 1));
    events.push(assistant("working"));
    events.push(usage("deepseek-chat", 5, 6));
    events.push(usage("deepseek-chat", 5, 6));
    events.extend(step(1, 2));
    events.push(assistant_blank());
    events.push(assistant("still working"));

    let compacted = compact(&events);

    assert!(compacted.removed > 0);
    assert_eq!(playback(&compacted.events), playback(&events));
}

#[test]
fn playback_is_unchanged_for_a_turn_full_of_tool_traffic() {
    // A turn that carries one of everything the rules must leave alone: prompt, boundaries, an
    // assistant tool-call request, the call, the result, and an answer. Nothing here is removable, so
    // this also pins that the rules do not start eating a well-formed turn.
    let events = one_tool_turn(1);

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
    assert_eq!(playback(&compacted.events), playback(&events));
}

#[test]
fn every_compacted_event_appears_in_the_input_unchanged_and_in_order() {
    // Removal, never rewriting. A subsequence check is enough because the rules only ever drop, but it
    // is stated as "in order" so that a future rule which reorders fails here.
    let events = mixed_log();

    let compacted = compact(&events);

    let mut cursor = 0usize;
    for kept in &compacted.events {
        let found = events
            .iter()
            .skip(cursor)
            .position(|candidate| candidate == kept);
        let offset = found.unwrap_or_else(|| {
            panic!("compacted event {kept:?} is not a later member of the original log")
        });
        cursor = cursor.saturating_add(offset).saturating_add(1);
    }
}

#[test]
fn compaction_is_idempotent() {
    // Compacting twice must not find anything new: the rules are stated over the log, and a second
    // pass that removed more would mean the first pass left a removable shape behind.
    let events = mixed_log();

    let once = compact(&events);
    let twice = compact(&once.events);

    assert_eq!(twice.events, once.events);
    assert_eq!(twice.removed, 0);
    assert!(!is_compactable(&once.events));
}

// ---- the reported numbers ---------------------------------------------------------------------

#[test]
fn an_empty_log_has_nothing_to_compact() {
    let compacted = compact(&[]);

    assert!(compacted.events.is_empty());
    assert_eq!(compacted.removed, 0);
    assert!(compacted.reasons.is_empty());
    assert!(!is_compactable(&[]));
}

#[test]
fn a_clean_log_is_returned_unchanged() {
    // The common answer. The events that carry a log's bytes are the ones that carry its meaning, so a
    // well-formed log is already compact.
    let mut events = Vec::new();
    events.extend(turn(1));
    events.extend(step(1, 1));
    events.push(user("hello"));
    events.push(assistant("hi"));
    events.push(usage("deepseek-chat", 1, 2));
    events.extend(step(1, 2));
    events.extend(turn(2));
    events.extend(step(2, 1));
    events.push(assistant_blocks(vec![ContentBlock::ToolCall {
        id: call_id("call-1"),
        name: "bash".to_owned(),
        arguments: "{}".to_owned(),
    }]));
    events.push(tool_call("call-1", "bash"));
    events.push(tool_result("call-1", "ok"));

    let compacted = compact(&events);

    assert_eq!(compacted.events, events);
    assert_eq!(compacted.removed, 0);
    assert!(compacted.reasons.is_empty());
    assert!(!is_compactable(&events));
}

#[test]
fn removed_equals_the_difference_in_length() {
    // A caller reports a size change from this number, so it must be the length change rather than a
    // tally that a second rule could double-count.
    let events = mixed_log();

    let compacted = compact(&events);

    assert_eq!(compacted.removed, events.len() - compacted.events.len());
    assert_eq!(
        is_compactable(&events),
        compacted.removed > 0,
        "the predicate and the transformation must agree"
    );
}

#[test]
fn each_reason_counts_only_its_own_rule() {
    // Two duplicates, one empty message, one superseded title: the reasons must add up to the total,
    // because an audit of a compaction is only as good as its arithmetic.
    let events = vec![
        usage("deepseek-chat", 1, 2),
        usage("deepseek-chat", 1, 2),
        usage("deepseek-chat", 1, 2),
        title("same", "model"),
        title("same", "model"),
        assistant_blank(),
        assistant("answer"),
    ];

    let compacted = compact(&events);

    assert_eq!(compacted.removed, 4);
    assert_eq!(compacted.reasons.len(), 3);
    assert!(compacted.reasons[0].contains("2 duplicate"));
    assert!(compacted.reasons[1].contains("1 empty"));
    assert!(compacted.reasons[2].contains("1 superseded"));
}

#[test]
fn a_large_log_is_compacted_only_where_it_is_redundant() {
    // The question this module was written to answer: can a ten-thousand-event log safely get
    // smaller? Only by the redundancy actually in it. This fixture is a thousand turns of real work
    // plus a handful of rewritten records, and the count stays a handful.
    let mut events = Vec::new();
    for number in 1..=1000u64 {
        events.extend(turn(number));
        events.extend(step(number, 1));
        events.push(user("read a.txt"));
        events.push(assistant("reading"));
        // A rewritten usage record on every hundredth turn.
        if number % 100 == 0 {
            events.push(usage("deepseek-chat", 7, 9));
            events.push(usage("deepseek-chat", 7, 9));
        } else {
            events.push(usage("deepseek-chat", 7, 9));
        }
        events.push(tool_call("call-1", "read_file"));
        events.push(tool_result("call-1", "contents"));
        events.extend(step(number, 2));
    }
    let original_len = events.len();
    assert!(original_len >= 10_000, "{original_len} events");

    let compacted = compact(&events);

    assert_eq!(compacted.removed, 10, "one duplicate per hundredth turn");
    assert_eq!(compacted.events.len(), original_len - 10);
    assert_eq!(playback(&compacted.events), playback(&events));
}
