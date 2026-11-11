//! Tests for the compaction policy.
//!
//! The property under test is a legality property, not a size one: whatever a plan decides, the
//! surviving history must be one a provider will accept. That means a tool call and its result are
//! either both kept or both dropped, and it is checked on every plan here rather than only on the
//! cases written to exercise it.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the policy as written even before the crate root wires it in. It resolves `girder_core::…`
// through the alias the library declares for itself, which is why the same source builds here.
#[path = "../src/compaction_hook.rs"]
mod compaction_hook;

use compaction_hook::{CompactionPolicy, DEFAULT_KEEP_RECENT, DEFAULT_TRIGGER_MESSAGES, Plan};
use girder_core::{ChatMessage, ChatRole, ContentBlock, MessageId, MessageSource, ToolCallId};

fn id(value: &str) -> MessageId {
    MessageId::new(value).expect("valid message id")
}

fn call_id(value: &str) -> ToolCallId {
    ToolCallId::new(value).expect("valid call id")
}

fn user(index: usize, text: &str) -> ChatMessage {
    ChatMessage::user(id(&format!("u{index}")), text)
}

fn system(text: &str) -> ChatMessage {
    ChatMessage::system(id("sys"), text)
}

fn assistant(index: usize, text: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("a{index}")),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "test".to_owned(),
            model: "test".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    )
}

/// An assistant message asking for one tool call.
fn call(index: usize, call: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("a{index}")),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "test".to_owned(),
            model: "test".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::ToolCall {
            id: call_id(call),
            name: "bash".to_owned(),
            arguments: "{}".to_owned(),
        }],
    )
}

/// A tool-role message answering `call`.
fn result(index: usize, call: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("r{index}")),
        ChatRole::Tool,
        MessageSource::Tool {
            call_id: call_id(call),
        },
        vec![ContentBlock::Text {
            text: "ok".to_owned(),
        }],
    )
}

/// An assistant message asking for two tools at once, which is one message and two calls.
fn two_calls(index: usize, first: &str, second: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("a{index}")),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "test".to_owned(),
            model: "test".to_owned(),
            replay_state: None,
        },
        vec![
            ContentBlock::ToolCall {
                id: call_id(first),
                name: "bash".to_owned(),
                arguments: "{}".to_owned(),
            },
            ContentBlock::ToolCall {
                id: call_id(second),
                name: "bash".to_owned(),
                arguments: "{}".to_owned(),
            },
        ],
    )
}

/// The call ids a message asks for.
fn opens(message: &ChatMessage) -> Vec<ToolCallId> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// Asserts the invariant every plan must hold: no plan splits a tool pair.
///
/// Written as a check over the produced plan rather than over one hand-built case, because the
/// interesting failures come from cuts the test author did not picture.
fn assert_pairs_whole(messages: &[ChatMessage], plan: &Plan) {
    let Plan::Compact { keep, drop, .. } = plan else {
        return;
    };

    // The plan must be a partition: every index appears exactly once, in order.
    let mut combined: Vec<usize> = keep.iter().chain(drop.iter()).copied().collect();
    combined.sort_unstable();
    assert_eq!(
        combined,
        (0..messages.len()).collect::<Vec<_>>(),
        "the plan must partition the history"
    );
    assert!(keep.windows(2).all(|w| w[0] < w[1]), "keep must ascend");
    assert!(drop.windows(2).all(|w| w[0] < w[1]), "drop must ascend");

    for (index, message) in messages.iter().enumerate() {
        let kept = keep.contains(&index);
        for call in opens(message) {
            let answered_at = messages.iter().position(|candidate| {
                candidate
                    .source
                    .call_id()
                    .is_some_and(|answered| *answered == call)
            });
            let Some(answered_at) = answered_at else {
                continue;
            };
            assert_eq!(
                kept,
                keep.contains(&answered_at),
                "message {index} asks for `{}`, answered at {answered_at}, but the plan keeps only \
                 one of them: {plan:?}",
                call.as_str()
            );
        }
    }
}

/// Runs a policy and checks the invariant before returning the plan.
fn plan_of(policy: &CompactionPolicy, messages: &[ChatMessage]) -> Plan {
    let plan = policy.plan(messages);
    assert_pairs_whole(messages, &plan);
    plan
}

// ---- the decision to compact ------------------------------------------------------------------

#[test]
fn a_short_history_is_kept_whole() {
    // Under the trigger there is nothing to trade off, and summarizing a conversation that fits
    // costs a model call and loses detail for no gain.
    let messages: Vec<ChatMessage> = (0..5).map(|i| user(i, "x")).collect();

    let plan = plan_of(&CompactionPolicy::default(), &messages);

    assert_eq!(plan, Plan::KeepAll);
    assert!(!plan.is_compaction());
    assert_eq!(plan.reason(), None, "there is no decision to explain");
}

#[test]
fn an_empty_history_is_kept_whole() {
    // The empty case reaches the same answer as a short one, without indexing into nothing.
    assert_eq!(CompactionPolicy::default().plan(&[]), Plan::KeepAll);
    assert_eq!(
        CompactionPolicy::new(0, 4).plan(&[]),
        Plan::KeepAll,
        "even a policy that always triggers has nothing to compact"
    );
}

#[test]
fn a_long_history_compacts_and_keeps_the_most_recent() {
    let messages: Vec<ChatMessage> = (0..20).map(|i| user(i, "x")).collect();
    let policy = CompactionPolicy::new(10, 5);

    let plan = plan_of(&policy, &messages);

    let Plan::Compact { keep, drop, .. } = plan else {
        panic!("expected a compaction, got {plan:?}");
    };
    assert_eq!(keep, vec![15, 16, 17, 18, 19], "the last five survive");
    assert_eq!(drop, (0..15).collect::<Vec<_>>(), "the rest is summarized");
}

#[test]
fn the_plan_reason_names_the_trigger_and_the_span() {
    // A reason that cannot be read in a log line is not a reason: it has to say what fired and what
    // was cut, because that is what an operator debugging a lost turn needs.
    let messages: Vec<ChatMessage> = (0..20).map(|i| user(i, "x")).collect();

    let plan = plan_of(&CompactionPolicy::new(10, 5), &messages);

    let reason = plan.reason().expect("a compaction explains itself");
    assert!(reason.contains("20 messages"), "{reason}");
    assert!(reason.contains("10 message trigger"), "{reason}");
    assert!(reason.contains("0..15"), "{reason}");
    assert!(reason.contains("last 5"), "{reason}");
}

#[test]
fn a_byte_trigger_compacts_a_few_huge_messages() {
    // A handful of enormous tool results fills a window while the message count stays small, which
    // is why the size trigger exists alongside the count trigger.
    let messages = vec![
        user(0, &"x".repeat(4_000)),
        user(1, &"x".repeat(4_000)),
        user(2, "recent"),
        user(3, "recent"),
    ];
    let policy = CompactionPolicy {
        trigger_messages: 100,
        trigger_bytes: Some(8_000),
        keep_recent: 2,
        require_pairing: true,
    };

    let plan = plan_of(&policy, &messages);

    let Plan::Compact { keep, reason, .. } = plan else {
        panic!("expected a compaction, got {plan:?}");
    };
    assert_eq!(keep, vec![2, 3]);
    assert!(reason.contains("bytes"), "{reason}");
}

#[test]
fn the_system_prefix_is_pinned_rather_than_summarized() {
    // Replacing the instructions the model follows with a description of them is strictly worse
    // than leaving them alone; the summary stands in for conversation, not for the prompt.
    let mut messages = vec![system("be terse")];
    messages.extend((0..12).map(|i| user(i, "x")));
    let policy = CompactionPolicy::new(5, 3);

    let plan = plan_of(&policy, &messages);

    let Plan::Compact { keep, drop, .. } = plan else {
        panic!("expected a compaction, got {plan:?}");
    };
    assert!(keep.contains(&0), "the system prompt survives");
    assert!(!drop.contains(&0));
    assert_eq!(keep, vec![0, 10, 11, 12], "pinned prefix, then the floor");
}

#[test]
fn keeping_everything_is_legal_even_when_the_trigger_fires() {
    // The trigger asks whether compaction is worth considering; a keep floor that covers the whole
    // history answers no, and that is not a failure.
    let messages: Vec<ChatMessage> = (0..10).map(|i| user(i, "x")).collect();

    let plan = plan_of(&CompactionPolicy::new(5, 10), &messages);

    assert_eq!(plan, Plan::KeepAll);
}

// ---- tool pairing -----------------------------------------------------------------------------

#[test]
fn a_tool_pair_straddling_the_keep_floor_is_kept_whole() {
    // The failure this prevents: keeping the result while summarizing away the call, which is a
    // history no provider accepts.
    // The floor wants only the final result; the cut that would honour it falls between the call
    // and its answer, so the pair is dragged into the kept suffix instead.
    let mut messages: Vec<ChatMessage> = (0..8).map(|i| user(i, "x")).collect();
    messages.push(call(8, "c1"));
    messages.push(result(9, "c1"));
    let policy = CompactionPolicy::new(5, 1);

    let plan = plan_of(&policy, &messages);

    let Plan::Compact { keep, reason, .. } = plan else {
        panic!("expected a compaction, got {plan:?}");
    };
    assert_eq!(
        keep,
        vec![8, 9],
        "the pair moves together and the suffix grows past the floor"
    );
    assert!(
        reason.contains("tool pair"),
        "the shifted cut must say why: {reason}"
    );
}

#[test]
fn a_tool_pair_can_be_dropped_whole_when_the_floor_is_a_ceiling() {
    // `require_pairing: false` is the other side of the same choice: the floor is a ceiling and the
    // pair is summarized, never split.
    let mut messages: Vec<ChatMessage> = (0..8).map(|i| user(i, "x")).collect();
    messages.push(call(8, "c1"));
    messages.push(result(9, "c1"));
    messages.push(assistant(10, "done"));
    let policy = CompactionPolicy {
        trigger_messages: 5,
        trigger_bytes: None,
        keep_recent: 1,
        require_pairing: false,
    };

    let plan = plan_of(&policy, &messages);

    let Plan::Compact { keep, drop, .. } = plan else {
        panic!("expected a compaction, got {plan:?}");
    };
    assert!(!keep.contains(&8) && !keep.contains(&9), "dropped whole");
    assert!(drop.contains(&8) && drop.contains(&9));
    assert_eq!(keep, vec![10], "only the trailing assistant message");
}

#[test]
fn one_message_carrying_two_calls_keeps_both_results_together() {
    // The message/entry mapping is not one to one: a single assistant message may ask for several
    // tools, so pairing has to be counted per call, not per message.
    let mut messages: Vec<ChatMessage> = (0..6).map(|i| user(i, "x")).collect();
    messages.push(two_calls(6, "c1", "c2"));
    messages.push(result(7, "c1"));
    messages.push(result(8, "c2"));
    messages.push(assistant(9, "done"));
    let policy = CompactionPolicy::new(4, 2);

    let plan = plan_of(&policy, &messages);

    let Plan::Compact { keep, .. } = plan else {
        panic!("expected a compaction, got {plan:?}");
    };
    let keeps_call = keep.contains(&6);
    assert_eq!(
        keeps_call,
        keep.contains(&7) && keep.contains(&8),
        "both answers travel with their call, got keep {keep:?}"
    );
}

#[test]
fn an_open_call_at_the_end_pulls_the_cut_earlier() {
    // A call with no answer yet is the normal state mid-turn; cutting inside it would summarize a
    // question whose answer is about to arrive.
    let mut messages: Vec<ChatMessage> = (0..10).map(|i| user(i, "x")).collect();
    messages.push(call(10, "c1"));
    messages.push(assistant(11, "thinking"));
    messages.push(assistant(12, "more"));

    let plan = plan_of(&CompactionPolicy::new(5, 2), &messages);

    let Plan::Compact { keep, .. } = plan else {
        panic!("expected a compaction, got {plan:?}");
    };
    assert!(keep.contains(&10), "the unanswered call is kept, {keep:?}");
    assert_eq!(keep, (10..13).collect::<Vec<_>>());
}

#[test]
fn a_broken_history_is_refused_rather_than_half_repaired() {
    // A result whose call never appeared is already a request the provider rejects. Dropping it
    // would be a plausible repair and is not this layer's decision to make.
    let mut messages: Vec<ChatMessage> = (0..10).map(|i| user(i, "x")).collect();
    messages.push(result(10, "ghost"));
    messages.push(assistant(11, "done"));

    let plan = plan_of(&CompactionPolicy::new(5, 2), &messages);

    let Plan::Refuse { reason } = plan else {
        panic!("expected a refusal, got {plan:?}");
    };
    assert!(reason.contains("message 10"), "{reason}");
}

#[test]
fn an_orphaned_result_within_the_dropped_span_is_tolerated() {
    // A result whose call was summarized by an earlier compaction is already inside the span being
    // replaced, so the summary heals it: refusing here would make a long session uncompactable
    // forever.
    let mut messages: Vec<ChatMessage> = (0..12).map(|i| user(i, "x")).collect();
    messages.push(result(12, "ghost"));
    messages.push(assistant(13, "done"));

    // The orphan at index 12 falls inside the summarized span, so the surviving history is clean.
    let plan = plan_of(&CompactionPolicy::new(5, 1), &messages);

    let Plan::Compact { drop, .. } = plan else {
        panic!("expected a compaction, got {plan:?}");
    };
    assert!(drop.contains(&12), "the orphan is summarized away");
}

// ---- refusal ----------------------------------------------------------------------------------

#[test]
fn a_cut_that_would_split_the_only_pair_is_refused() {
    // Everything between the pinned prefix and the floor is one open exchange: keeping the floor
    // would split it, and keeping more would leave nothing to summarize.
    let messages = vec![system("prompt"), call(0, "c1"), result(1, "c1")];
    let policy = CompactionPolicy::new(1, 1);

    let plan = plan_of(&policy, &messages);

    let Plan::Refuse { reason } = plan else {
        panic!("expected a refusal, got {plan:?}");
    };
    assert!(reason.contains("split a tool call"), "{reason}");
    assert!(reason.contains("`c1`"), "the open call is named: {reason}");
}

#[test]
fn a_policy_that_keeps_nothing_is_refused() {
    // Keeping zero recent messages would summarize the entire history and leave the model with no
    // conversation at all.
    let messages: Vec<ChatMessage> = (0..10).map(|i| user(i, "x")).collect();

    let plan = plan_of(&CompactionPolicy::new(1, 0), &messages);

    let Plan::Refuse { reason } = plan else {
        panic!("expected a refusal, got {plan:?}");
    };
    assert!(reason.contains("entire history"), "{reason}");
}

#[test]
fn a_history_of_nothing_but_system_messages_is_refused() {
    let messages = vec![system("one"), system("two"), system("three")];

    let plan = plan_of(&CompactionPolicy::new(1, 1), &messages);

    let Plan::Refuse { reason } = plan else {
        panic!("expected a refusal, got {plan:?}");
    };
    assert!(reason.contains("system prefix"), "{reason}");
}

#[test]
fn a_pinned_prefix_and_floor_with_no_gap_between_them_is_refused() {
    // Two system messages and a floor of five leave indices 2..2 to summarize: nothing.
    let mut messages = vec![system("one"), system("two")];
    messages.extend((0..3).map(|i| user(i, "x")));

    let plan = plan_of(&CompactionPolicy::new(1, 5), &messages);

    assert_eq!(
        plan,
        Plan::KeepAll,
        "a keep floor covering the history is not a refusal"
    );

    let policy = CompactionPolicy {
        trigger_messages: 1,
        trigger_bytes: None,
        keep_recent: 3,
        require_pairing: true,
    };
    let Plan::Refuse { reason } = plan_of(&policy, &messages) else {
        panic!("expected a refusal, got {:?}", plan_of(&policy, &messages));
    };
    assert!(reason.contains("no messages between them"), "{reason}");
}

// ---- invariants over generated histories ------------------------------------------------------

#[test]
fn every_policy_keeps_tool_pairs_whole() {
    // The invariant is checked across a grid of shapes and settings rather than on one case,
    // because the interesting cut positions are the ones a hand-written test forgets.
    let shapes: Vec<Vec<ChatMessage>> = vec![
        // alternating pairs, plus a tail
        (0..12)
            .flat_map(|i| {
                vec![
                    call(i * 2, &format!("c{i}")),
                    result(i * 2 + 1, &format!("c{i}")),
                ]
            })
            .chain((0..3).map(|i| user(100 + i, "tail")))
            .collect(),
        // one message asking for two calls, answered adjacently
        vec![
            user(0, "go"),
            two_calls(1, "a", "b"),
            result(2, "a"),
            result(3, "b"),
            user(4, "thanks"),
            assistant(5, "done"),
            user(6, "again"),
        ],
    ];

    for shape in &shapes {
        for keep_recent in 0..=shape.len() {
            for require_pairing in [true, false] {
                let policy = CompactionPolicy {
                    trigger_messages: 1,
                    trigger_bytes: None,
                    keep_recent,
                    require_pairing,
                };
                // `plan_of` asserts the invariant; a panic here is the failure.
                let _ = plan_of(&policy, shape);
            }
        }
    }
}

#[test]
fn planning_is_pure_and_repeatable() {
    // The same history must always yield the same plan: a decision that drifts between calls makes
    // a session's behaviour impossible to reproduce from its log.
    let messages: Vec<ChatMessage> = (0..20).map(|i| user(i, "x")).collect();
    let policy = CompactionPolicy::default();

    assert_eq!(policy.plan(&messages), policy.plan(&messages));
}

#[test]
fn the_defaults_are_the_documented_ones() {
    let policy = CompactionPolicy::default();

    assert_eq!(policy.trigger_messages, DEFAULT_TRIGGER_MESSAGES);
    assert_eq!(policy.keep_recent, DEFAULT_KEEP_RECENT);
    assert!(policy.require_pairing);
    assert_eq!(policy.trigger_bytes, None);
}
