//! Tests for response replay state: the envelope, its two provider dialects, and assembly's fold.
//!
//! The reference vectors are transcriptions, not interpretations. Each group names the upstream
//! suite it came from, because the point of a port is that an implementation that disagrees with
//! these is wrong regardless of how defensible the disagreement looks:
//!
//! - `packages/llm/llm/tests/assembler.spec.ts` — the five replay-metadata cases of
//!   `describe('BlockAssembler replay metadata')`, reproduced one for one, including the
//!   misalignment case that discards the envelope and the no-per-block-half case that survives a
//!   drop.
//! - `packages/llm/llm-deepseek/tests/serialize.spec.ts` — `readReplay`/`replayState`, the ten
//!   unusable states, the signature rules, and the cross-model decline.
//! - `packages/llm/llm-pi-ai/tests/convert.spec.ts` — `toPiReplayState`/`toPiAssistant`'s envelope
//!   validation: the twenty-two `degrades malformed replay state` rows, the two source-mismatch
//!   rows, and the two block-mismatch rows.
//! - `packages/llm/llm/tests/service.spec.ts` — `forAdapter`'s replay ownership rule.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `llm_stream_gap`; adding `pub mod llm_stream_gap;`
//! and its `pub use` belongs to the integrator, along with the `replay_state` field on
//! `StreamChunk::Finish` that will carry [`ReplayEnvelope`] into the stream. Compiling the module
//! into this test binary directly is what makes the tests independent of that wiring, and it also
//! keeps the crate root untouched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "tests assert by panicking; a failed unwrap is a failed test, and the per-block tables \
              are clearer as one long case list than as dozens of near-identical functions"
)]

#[path = "../src/llm_stream_gap.rs"]
mod llm_stream_gap;

use girder_core::{ChatMessage, ChatRole, ContentBlock, MessageId, MessageSource, ToolCallId};
use serde_json::{Value, json};

use llm_stream_gap::{
    DEEPSEEK_REPLAY_KIND, DEEPSEEK_REPLAY_VERSION, DeepSeekReplayBlock, PI_REPLAY_KIND,
    PI_REPLAY_VERSION, PiReplayBlock, PiReplayResponse, PiStopReason, ReplayBlockKind,
    ReplayEnvelope, ReplayOutcome, align_replay, assemble_replay, deepseek_replay_state,
    pi_replay_state, read_deepseek_replay, read_pi_replay, retain_blocks, strip_foreign_replay,
    truncation_mask,
};

// =============================================================================================
// Fixtures
// =============================================================================================

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.to_owned(),
    }
}

fn reasoning(value: &str) -> ContentBlock {
    ContentBlock::Reasoning {
        text: value.to_owned(),
    }
}

fn tool_call(id: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        id: ToolCallId::new(id).unwrap(),
        name: "echo".to_owned(),
        arguments: "{}".to_owned(),
    }
}

fn message(role: ChatRole, source: MessageSource, content: Vec<ContentBlock>) -> ChatMessage {
    ChatMessage::new(MessageId::new("m-test").unwrap(), role, source, content)
}

/// The response half the assembler cases use, verbatim from `assembler.spec.ts`.
fn response() -> Value {
    json!({ "responseId": "resp-1" })
}

// =============================================================================================
// The envelope
// =============================================================================================

#[test]
fn a_block_less_envelope_reports_no_block_count() {
    let envelope = ReplayEnvelope::new(response(), None);
    assert_eq!(envelope.block_count(), None);
}

#[test]
fn an_envelope_with_an_empty_block_list_reports_zero_rather_than_none() {
    // `Some(0)` and `None` are different facts: one claims the stream produced no blocks, the other
    // makes no claim at all, and assembly treats them differently.
    let envelope = ReplayEnvelope::new(response(), Some(Vec::new()));
    assert_eq!(envelope.block_count(), Some(0));
}

#[test]
fn the_envelope_serializes_without_an_absent_block_half() {
    let envelope = ReplayEnvelope::new(response(), None);
    let encoded = serde_json::to_value(&envelope).unwrap();
    assert_eq!(encoded, json!({ "response": { "responseId": "resp-1" } }));
}

#[test]
fn block_kinds_pair_with_the_three_describable_content_types() {
    assert_eq!(ReplayBlockKind::of(&text("x")), Some(ReplayBlockKind::Text));
    assert_eq!(
        ReplayBlockKind::of(&reasoning("x")),
        Some(ReplayBlockKind::Reasoning)
    );
    assert_eq!(
        ReplayBlockKind::of(&tool_call("c1")),
        Some(ReplayBlockKind::ToolCall)
    );
    // Attachments have no replay form: a reader that called them text would claim fidelity it
    // does not have.
    assert_eq!(
        ReplayBlockKind::of(&ContentBlock::Image {
            attachment: "att-1".to_owned()
        }),
        None
    );
    assert_eq!(
        ReplayBlockKind::of(&ContentBlock::File {
            attachment: "att-1".to_owned()
        }),
        None
    );
}

#[test]
fn block_kind_tags_are_the_wire_tags() {
    assert_eq!(ReplayBlockKind::Text.tag(), "text");
    assert_eq!(ReplayBlockKind::Reasoning.tag(), "reasoning");
    assert_eq!(ReplayBlockKind::ToolCall.tag(), "tool-call");
}

// =============================================================================================
// Assembly: the five assembler.spec.ts cases
// =============================================================================================

#[test]
fn prunes_per_block_entries_with_the_tool_calls_a_max_tokens_finish_drops() {
    let blocks = vec![text("lead"), tool_call("c1"), reasoning("tail")];
    let envelope = ReplayEnvelope::new(
        response(),
        Some(vec![json!("meta-0"), json!("meta-1"), json!("meta-2")]),
    );

    let (kept, replay) = assemble_replay(blocks, true, Some(&envelope));

    assert_eq!(kept, vec![text("lead"), reasoning("tail")]);
    assert_eq!(
        replay,
        Some(ReplayEnvelope::new(
            response(),
            Some(vec![json!("meta-0"), json!("meta-2")])
        ))
    );
}

#[test]
fn omits_replay_metadata_whose_per_block_entries_misalign_with_the_emitted_blocks() {
    let blocks = vec![text("one"), text("two")];
    let envelope = ReplayEnvelope::new(response(), Some(vec![json!("meta-0")]));

    let (kept, replay) = assemble_replay(blocks, false, Some(&envelope));

    assert_eq!(kept.len(), 2);
    assert_eq!(replay, None);
}

#[test]
fn passes_replay_metadata_through_unchanged_when_assembly_drops_nothing() {
    let envelope = ReplayEnvelope::new(response(), Some(vec![json!("meta-0"), json!("meta-1")]));
    let blocks = vec![text("partial"), tool_call("c1")];

    let (kept, replay) = assemble_replay(blocks, false, Some(&envelope));

    assert_eq!(kept.len(), 2);
    // Unchanged, not merely equal: the identity is what a caller compares to decide whether a
    // request rebuilt from it is the same request.
    assert_eq!(replay.as_ref(), Some(&envelope));
}

#[test]
fn keeps_a_max_tokens_replay_state_with_no_per_block_entries_across_a_tool_call_drop() {
    let envelope = ReplayEnvelope::new(response(), None);
    let blocks = vec![text("partial"), tool_call("c1")];

    let (kept, replay) = assemble_replay(blocks, true, Some(&envelope));

    assert_eq!(kept, vec![text("partial")]);
    assert_eq!(replay.as_ref(), Some(&envelope));
}

#[test]
fn keeps_a_text_only_max_tokens_response_and_its_replay_metadata_intact() {
    let envelope = ReplayEnvelope::new(response(), Some(vec![json!("meta-0")]));
    let blocks = vec![text("partial")];

    let (kept, replay) = assemble_replay(blocks, true, Some(&envelope));

    assert_eq!(kept, vec![text("partial")]);
    assert_eq!(replay.as_ref(), Some(&envelope));
}

#[test]
fn a_stream_with_no_envelope_assembles_to_none() {
    let (kept, replay) = assemble_replay(vec![text("only")], true, None);
    assert_eq!(kept, vec![text("only")]);
    assert_eq!(replay, None);
}

#[test]
fn the_truncation_mask_drops_only_tool_calls() {
    let blocks = vec![text("a"), tool_call("c1"), reasoning("b")];
    assert_eq!(truncation_mask(&blocks, false), vec![true, true, true]);
    assert_eq!(truncation_mask(&blocks, true), vec![true, false, true]);
}

#[test]
fn retaining_blocks_applies_the_mask_positionally() {
    let blocks = vec![text("a"), tool_call("c1"), reasoning("b")];
    let kept = retain_blocks(blocks, &[true, false, true]);
    assert_eq!(kept, vec![text("a"), reasoning("b")]);
}

#[test]
fn a_short_mask_retains_the_blocks_it_does_not_cover() {
    // A mask is derived from the same block list, so a short one is a caller bug rather than a
    // stream shape; retaining rather than dropping keeps the bug from silently losing content.
    let blocks = vec![text("a"), text("b")];
    assert_eq!(retain_blocks(blocks, &[false]), vec![text("b")]);
}

#[test]
fn aligning_an_envelope_with_a_mask_that_keeps_everything_copies_it_verbatim() {
    let envelope = ReplayEnvelope::new(response(), Some(vec![json!("m0"), json!("m1")]));
    let aligned = align_replay(Some(&envelope), &[true, true]);
    assert_eq!(aligned.as_ref(), Some(&envelope));
}

#[test]
fn aligning_a_non_array_block_half_discards_the_envelope() {
    // Upstream reads `envelope.blocks.length` after testing only `=== undefined`, so a durable
    // `blocks: null` throws out of a fold over saved data. Metadata that cannot be read is metadata
    // the caller cannot use, so it is discarded instead.
    let envelope = ReplayEnvelope::new(response(), None);
    let damaged = Value::Object(
        json!({ "response": { "responseId": "resp-1" }, "blocks": Value::Null })
            .as_object()
            .unwrap()
            .clone(),
    );
    let decoded: ReplayEnvelope = serde_json::from_value(damaged).unwrap();
    assert_eq!(decoded.blocks, None);
    // The decoded value still round-trips through the aligner as "no per-block half".
    assert_eq!(align_replay(Some(&envelope), &[true]), Some(envelope));
}

// =============================================================================================
// The DeepSeek Messages dialect — serialize.spec.ts
// =============================================================================================

const MODEL: &str = "deepseek-v4-flash";

#[test]
fn writes_the_versioned_deepseek_envelope() {
    let envelope = deepseek_replay_state(
        MODEL,
        &[
            DeepSeekReplayBlock::text(),
            DeepSeekReplayBlock::reasoning("signed"),
            DeepSeekReplayBlock::tool_call(),
        ],
    );
    assert_eq!(
        envelope.response,
        json!({ "kind": DEEPSEEK_REPLAY_KIND, "version": DEEPSEEK_REPLAY_VERSION, "model": MODEL })
    );
    assert_eq!(
        envelope.blocks,
        Some(vec![
            json!({ "type": "text" }),
            json!({ "type": "reasoning", "signature": "signed" }),
            json!({ "type": "tool-call" }),
        ])
    );
}

#[test]
fn an_absent_signature_is_omitted_rather_than_written_as_null() {
    // The reader distinguishes "absent" from "present but not a string", and a writer that emitted
    // null for every unsigned entry would make its own output unreadable.
    let envelope = deepseek_replay_state(MODEL, &[DeepSeekReplayBlock::text()]);
    let entries = envelope.blocks.unwrap();
    assert_eq!(entries[0].get("signature"), None);
}

#[test]
fn the_response_half_alone_is_not_a_replay_state() {
    // The reader takes the whole envelope; handing it only the response half must fail rather than
    // be reinterpreted, or a caller could accidentally accept unvalidated blocks.
    let content = vec![reasoning("think")];
    let envelope = deepseek_replay_state(MODEL, &[DeepSeekReplayBlock::reasoning("signed")]);
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, Some(&envelope.response)),
        ReplayOutcome::Degraded(
            "DeepSeek Messages replay: DeepSeek Messages expected a JSON object".to_owned()
        ),
    );
}

#[test]
fn reads_a_whole_envelope_and_returns_its_blocks() {
    let content = vec![reasoning("think"), text("answer")];
    let envelope = deepseek_replay_state(
        MODEL,
        &[
            DeepSeekReplayBlock::reasoning("signed"),
            DeepSeekReplayBlock::text(),
        ],
    );
    let state = serde_json::to_value(&envelope).unwrap();
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, Some(&state)),
        ReplayOutcome::Usable(vec![
            DeepSeekReplayBlock::reasoning("signed"),
            DeepSeekReplayBlock::text(),
        ])
    );
}

#[test]
fn the_usable_accessor_answers_both_failure_kinds_with_none() {
    let content = vec![text("answer")];
    let state = json!({
        "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL },
        "blocks": [{ "type": "text" }],
    });
    let usable = read_deepseek_replay(&content, MODEL, MODEL, Some(&state)).usable();
    assert_eq!(usable, Some(vec![DeepSeekReplayBlock::text()]));
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, None).usable(),
        None
    );
    let damaged = read_deepseek_replay(&content, MODEL, MODEL, Some(&Value::Null));
    assert!(damaged.diagnostic().is_some());
}

#[test]
fn a_message_with_no_replay_state_is_absent_without_a_diagnostic() {
    let content = vec![text("answer")];
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, None),
        ReplayOutcome::Absent
    );
}

#[test]
fn an_empty_signature_string_is_a_valid_signature() {
    // Upstream's check is `typeof block.signature !== 'string'`, not emptiness: a provider that
    // returns an empty signature returned a signature, and the round trip must preserve it.
    let content = vec![reasoning("think")];
    let state = json!({
        "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL },
        "blocks": [{ "type": "reasoning", "signature": "" }],
    });
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, Some(&state)),
        ReplayOutcome::Usable(vec![DeepSeekReplayBlock::reasoning("")])
    );
}

#[test]
fn a_matching_envelope_for_another_target_model_is_absent_not_degraded() {
    // Replaying a signature into a request for a different model is a normal thing to decline; it
    // is not a damaged log, so it must not produce a diagnostic.
    let content = vec![reasoning("think")];
    let state = json!({
        "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL },
        "blocks": [{ "type": "reasoning", "signature": "signed" }],
    });
    let outcome = read_deepseek_replay(&content, MODEL, "different-model", Some(&state));
    assert_eq!(outcome, ReplayOutcome::Absent);
    assert_eq!(outcome.diagnostic(), None);
}

/// The ten unusable `DeepSeek` states `serialize.spec.ts` enumerates, with the diagnostic each gets.
fn unusable_deepseek_states() -> Vec<(Value, &'static str)> {
    vec![
        (
            Value::Null,
            "DeepSeek Messages replay: DeepSeek Messages expected a JSON object",
        ),
        (
            json!([]),
            "DeepSeek Messages replay: DeepSeek Messages expected a JSON object",
        ),
        (
            json!({ "response": null, "blocks": [] }),
            "DeepSeek Messages replay: DeepSeek Messages expected a JSON object",
        ),
        (
            json!({ "response": { "kind": "other", "version": 1 }, "blocks": [] }),
            "DeepSeek Messages replay: unsupported kind or version",
        ),
        (
            json!({ "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 2 }, "blocks": [] }),
            "DeepSeek Messages replay: unsupported kind or version",
        ),
        (
            json!({ "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": "wrong" }, "blocks": [] }),
            "DeepSeek Messages replay: model does not match assistant source model",
        ),
        (
            json!({ "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL }, "blocks": [] }),
            "DeepSeek Messages replay: block count mismatch",
        ),
        (
            json!({ "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL }, "blocks": null }),
            "DeepSeek Messages replay: block count mismatch",
        ),
        (
            json!({ "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL }, "blocks": [null] }),
            "DeepSeek Messages replay: block type mismatch",
        ),
        (
            json!({ "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL }, "blocks": [{ "type": "tool-call" }] }),
            "DeepSeek Messages replay: block type mismatch",
        ),
    ]
}

#[test]
fn every_unusable_deepseek_state_degrades_with_its_diagnostic() {
    // The content is one reasoning block, which is what `serialize.spec.ts` uses; the last row uses
    // a tool-call entry against it precisely because a mismatched tag must be caught before the
    // signature rule is reached.
    let content = vec![reasoning("think")];
    for (state, expected) in &unusable_deepseek_states() {
        let outcome = read_deepseek_replay(&content, MODEL, MODEL, Some(state));
        assert_eq!(
            outcome,
            ReplayOutcome::Degraded((*expected).to_owned()),
            "state {state} produced {outcome:?}"
        );
    }
}

#[test]
fn a_signature_outside_a_reasoning_entry_degrades_the_envelope() {
    let content = vec![text("answer")];
    let state = json!({
        "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL },
        "blocks": [{ "type": "text", "signature": "invalid-for-text" }],
    });
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, Some(&state)),
        ReplayOutcome::Degraded("DeepSeek Messages replay: invalid signature".to_owned())
    );
}

#[test]
fn a_non_string_signature_degrades_the_envelope() {
    let content = vec![reasoning("think")];
    let state = json!({
        "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL },
        "blocks": [{ "type": "reasoning", "signature": 3 }],
    });
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, Some(&state)),
        ReplayOutcome::Degraded("DeepSeek Messages replay: invalid signature".to_owned())
    );
}

#[test]
fn a_foreign_entry_kind_degrades_the_envelope() {
    let content = vec![text("answer")];
    let state = json!({
        "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": 1, "model": MODEL },
        "blocks": [{ "type": "audio" }],
    });
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, Some(&state)),
        ReplayOutcome::Degraded("DeepSeek Messages replay: block type mismatch".to_owned())
    );
}

#[test]
fn a_pi_ai_envelope_is_not_readable_as_a_deepseek_one() {
    // The dialects are not interchangeable, and a reader that accepted a foreign `kind` would hand
    // its own renderer another adapter's metadata.
    let content = vec![text("answer")];
    let state = json!({
        "response": { "kind": PI_REPLAY_KIND, "version": PI_REPLAY_VERSION, "api": "x", "provider": "p", "model": MODEL, "stopReason": "stop" },
        "blocks": [{ "type": "text" }],
    });
    assert_eq!(
        read_deepseek_replay(&content, MODEL, MODEL, Some(&state)),
        ReplayOutcome::Degraded("DeepSeek Messages replay: unsupported kind or version".to_owned())
    );
}

// =============================================================================================
// The pi-ai dialect — convert.spec.ts
// =============================================================================================

fn pi_response() -> PiReplayResponse {
    PiReplayResponse::new("openai-completions", "deepseek", MODEL, PiStopReason::Stop)
}

#[test]
fn writes_the_versioned_pi_envelope() {
    let envelope = pi_replay_state(
        &pi_response(),
        &[
            PiReplayBlock::Text {
                text_signature: None,
            },
            PiReplayBlock::Reasoning {
                thinking_signature: Some("thinking-sig".to_owned()),
                redacted: Some(true),
            },
            PiReplayBlock::ToolCall {
                thought_signature: Some("thought-sig".to_owned()),
                namespace: Some("ns".to_owned()),
            },
        ],
    );
    assert_eq!(
        envelope.response,
        json!({
            "kind": PI_REPLAY_KIND,
            "version": PI_REPLAY_VERSION,
            "api": "openai-completions",
            "provider": "deepseek",
            "model": MODEL,
            "stopReason": "stop",
        })
    );
    assert_eq!(
        envelope.blocks,
        Some(vec![
            json!({ "type": "text" }),
            json!({ "type": "reasoning", "thinkingSignature": "thinking-sig", "redacted": true }),
            json!({ "type": "tool-call", "thoughtSignature": "thought-sig", "namespace": "ns" }),
        ])
    );
}

#[test]
fn the_pi_envelope_records_the_requested_model_and_keeps_the_reported_one_beside_it() {
    // A provider may report a dated or redirected model while the signature stays valid only for the
    // requested identity, so the requested model is what `model` records and the reported one is
    // carried separately.
    let mut response = pi_response();
    response.model = "claude-haiku-4-5".to_owned();
    response.response_model = Some("claude-haiku-4-5-20251001".to_owned());
    let envelope = pi_replay_state(&response, &[]);
    assert_eq!(
        envelope.response,
        json!({
            "kind": PI_REPLAY_KIND,
            "version": PI_REPLAY_VERSION,
            "api": "openai-completions",
            "provider": "deepseek",
            "model": "claude-haiku-4-5",
            "responseModel": "claude-haiku-4-5-20251001",
            "stopReason": "stop",
        })
    );
}

#[test]
fn an_absent_pi_optional_field_is_omitted_rather_than_written_as_null() {
    let envelope = pi_replay_state(&pi_response(), &[]);
    for key in [
        "responseModel",
        "responseId",
        "providerThinkingLevel",
        "prefixDigest",
    ] {
        assert_eq!(envelope.response.get(key), None, "{key} must be absent");
    }
}

#[test]
fn reads_back_its_own_pi_envelope() {
    let content = vec![reasoning("think"), tool_call("c1")];
    let response = {
        let mut response = pi_response();
        response.response_id = Some("resp-1".to_owned());
        response.provider_thinking_level = Some("high".to_owned());
        response.prefix_digest = Some("a".repeat(64));
        response.stop_reason = PiStopReason::ToolUse;
        response
    };
    let blocks = vec![
        PiReplayBlock::Reasoning {
            thinking_signature: Some("sig".to_owned()),
            redacted: None,
        },
        PiReplayBlock::ToolCall {
            thought_signature: Some("thought".to_owned()),
            namespace: None,
        },
    ];
    let envelope = pi_replay_state(&response, &blocks);
    let state = serde_json::to_value(&envelope).unwrap();

    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Usable(llm_stream_gap::PiReplay { response, blocks })
    );
}

#[test]
fn every_pi_stop_reason_round_trips_through_the_wire_tag() {
    for reason in [
        PiStopReason::Stop,
        PiStopReason::Length,
        PiStopReason::ToolUse,
        PiStopReason::Error,
        PiStopReason::Aborted,
    ] {
        assert_eq!(PiStopReason::parse(reason.tag()), Some(reason));
    }
}

#[test]
fn pi_ai_terminal_states_outside_the_replay_domain_are_refused() {
    // `pending` and `deferred` are pi-ai states with no reconstructable message behind them, which
    // is why pi-ai's own replay validator refuses them.
    assert_eq!(PiStopReason::parse("pending"), None);
    assert_eq!(PiStopReason::parse("deferred"), None);
    assert_eq!(PiStopReason::parse("tool-use"), None);
}

#[test]
fn a_pi_replay_payload_reports_the_same_diagnostic_prefix_upstream_uses() {
    let content = vec![text("done")];
    let state = json!(1);
    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(
            "invalid pi-ai replay state: expected a replay envelope".to_owned()
        )
    );
}

/// The twenty-two `degrades malformed replay state` rows of `convert.spec.ts`, in order.
///
/// The valid response and envelope halves are reproduced verbatim so each row differs from a known
/// good state in exactly one field — which is what makes the row a test of *that* field.
const MALFORMED_PI_STATES: [(&str, &str); 22] = [
    ("number state", "expected a replay envelope"),
    ("null state", "expected a replay envelope"),
    ("array state", "expected a replay envelope"),
    ("missing response", "expected a response object"),
    ("array response", "expected a response object"),
    ("unknown kind", "unknown state kind"),
    ("non-string api", "api must be a non-empty string"),
    ("empty provider", "provider must be a non-empty string"),
    ("missing model", "model must be a non-empty string"),
    ("unknown stop reason", "unknown stopReason"),
    (
        "non-string response model",
        "responseModel must be a string",
    ),
    ("non-string response id", "responseId must be a string"),
    (
        "non-string provider thinking level",
        "providerThinkingLevel must be a string",
    ),
    (
        "null provider thinking level",
        "providerThinkingLevel must be a string",
    ),
    ("non-string prefix digest", "prefixDigest must be a string"),
    ("missing blocks", "blocks must be an array"),
    ("non-array blocks", "blocks must be an array"),
    ("number block", "block 0 must be an object"),
    ("null block", "block 0 must be an object"),
    ("array block", "block 0 must be an object"),
    ("unknown block type", "block 0 has an unknown type"),
    ("non-string namespace", "block 0 namespace must be a string"),
];

/// The malformed state each `MALFORMED_PI_STATES` row names, in the same order.
fn malformed_pi_state(name: &str) -> Value {
    let response = json!({
        "kind": PI_REPLAY_KIND,
        "version": PI_REPLAY_VERSION,
        "api": "openai-completions",
        "provider": "deepseek",
        "model": MODEL,
        "stopReason": "stop",
    });
    let valid = json!({ "response": response, "blocks": [{ "type": "text" }] });
    match name {
        "number state" => json!(1),
        "null state" => Value::Null,
        "array state" => json!([]),
        "missing response" => json!({ "blocks": [] }),
        "array response" => json!({ "response": [], "blocks": [] }),
        "unknown kind" => {
            let mut state = valid.clone();
            state["response"]["kind"] = json!("other");
            state
        }
        "non-string api" => {
            let mut state = valid.clone();
            state["response"]["api"] = json!(1);
            state
        }
        "empty provider" => {
            let mut state = valid.clone();
            state["response"]["provider"] = json!("");
            state
        }
        "missing model" => {
            let mut state = valid.clone();
            state["response"].as_object_mut().unwrap().remove("model");
            state
        }
        "unknown stop reason" => {
            let mut state = valid.clone();
            state["response"]["stopReason"] = json!("pause");
            state
        }
        "non-string response model" => {
            let mut state = valid.clone();
            state["response"]["responseModel"] = json!(1);
            state
        }
        "non-string response id" => {
            let mut state = valid.clone();
            state["response"]["responseId"] = json!(1);
            state
        }
        "non-string provider thinking level" => {
            let mut state = valid.clone();
            state["response"]["providerThinkingLevel"] = json!(1);
            state
        }
        "null provider thinking level" => {
            let mut state = valid.clone();
            state["response"]["providerThinkingLevel"] = Value::Null;
            state
        }
        "non-string prefix digest" => {
            let mut state = valid.clone();
            state["response"]["prefixDigest"] = json!(1);
            state
        }
        "missing blocks" => json!({ "response": response }),
        "non-array blocks" => {
            let mut state = valid.clone();
            state["blocks"] = json!("text");
            state
        }
        "number block" => {
            let mut state = valid.clone();
            state["blocks"] = json!([1]);
            state
        }
        "null block" => {
            let mut state = valid.clone();
            state["blocks"] = json!([null]);
            state
        }
        "array block" => {
            let mut state = valid.clone();
            state["blocks"] = json!([[]]);
            state
        }
        "unknown block type" => {
            let mut state = valid.clone();
            state["blocks"] = json!([{ "type": "audio" }]);
            state
        }
        "non-string namespace" => {
            let mut state = valid.clone();
            state["blocks"] = json!([{ "type": "tool-call", "namespace": 1 }]);
            state
        }
        other => panic!("no malformed state named {other}"),
    }
}

#[test]
fn every_malformed_pi_state_degrades_with_the_diagnostic_upstream_names() {
    let content = vec![text("done")];
    for (name, detail) in MALFORMED_PI_STATES {
        let state = malformed_pi_state(name);
        let outcome = read_pi_replay(Some(&state), &content, "deepseek", MODEL);
        assert_eq!(
            outcome,
            ReplayOutcome::Degraded(format!("invalid pi-ai replay state: {detail}")),
            "row {name} produced {outcome:?}"
        );
    }
}

#[test]
fn a_non_string_signature_and_a_non_boolean_redaction_are_refused() {
    // The two remaining malformed rows of the same table, kept separate because their messages name
    // a field deep inside a block rather than the block itself.
    let content = vec![text("done")];
    let signature = json!({
        "response": { "kind": PI_REPLAY_KIND, "version": PI_REPLAY_VERSION, "api": "a", "provider": "deepseek", "model": MODEL, "stopReason": "stop" },
        "blocks": [{ "type": "text", "textSignature": 1 }],
    });
    assert_eq!(
        read_pi_replay(Some(&signature), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(
            "invalid pi-ai replay state: block 0 textSignature must be a string".to_owned()
        )
    );

    let redaction = json!({
        "response": { "kind": PI_REPLAY_KIND, "version": PI_REPLAY_VERSION, "api": "a", "provider": "deepseek", "model": MODEL, "stopReason": "stop" },
        "blocks": [{ "type": "reasoning", "redacted": "yes" }],
    });
    assert_eq!(
        read_pi_replay(Some(&redaction), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(
            "invalid pi-ai replay state: block 0 redacted must be boolean".to_owned()
        )
    );
}

#[test]
fn a_flat_pre_envelope_state_degrades_with_the_pre_rename_diagnostic() {
    // A legacy session log stores the response half at the top level. It must degrade rather than be
    // reinterpreted, because the blocks it carries were never validated.
    let content = vec![text("done")];
    let state = json!({
        "kind": PI_REPLAY_KIND,
        "version": 1,
        "api": "openai-completions",
        "provider": "deepseek",
        "model": MODEL,
        "stopReason": "stop",
        "blocks": [{ "type": "text" }],
    });
    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(
            "invalid pi-ai replay state: expected a response object".to_owned()
        )
    );
}

#[test]
fn an_unsupported_version_names_the_version_it_found() {
    let content = vec![text("done")];
    let state = json!({ "response": { "kind": PI_REPLAY_KIND, "version": 3 }, "blocks": [] });
    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded("invalid pi-ai replay state: unsupported version 3".to_owned())
    );
}

#[test]
fn the_reported_version_is_rendered_the_way_javascript_renders_it() {
    let content = vec![text("done")];
    for (version, rendered) in [
        (json!("two"), "two"),
        (json!(null), "null"),
        (json!(2.5), "2.5"),
        (json!({}), "[object Object]"),
    ] {
        let state =
            json!({ "response": { "kind": PI_REPLAY_KIND, "version": version }, "blocks": [] });
        assert_eq!(
            read_pi_replay(Some(&state), &content, "deepseek", MODEL),
            ReplayOutcome::Degraded(format!(
                "invalid pi-ai replay state: unsupported version {rendered}"
            ))
        );
    }
}

#[test]
fn a_pi_envelope_from_another_provider_or_model_is_refused() {
    let content = vec![text("done")];
    let state = json!({
        "response": { "kind": PI_REPLAY_KIND, "version": PI_REPLAY_VERSION, "api": "a", "provider": "openai", "model": MODEL, "stopReason": "stop" },
        "blocks": [{ "type": "text" }],
    });
    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(
            "invalid pi-ai replay state: provider does not match assistant source".to_owned()
        )
    );

    let state = json!({
        "response": { "kind": PI_REPLAY_KIND, "version": PI_REPLAY_VERSION, "api": "a", "provider": "deepseek", "model": "deepseek-v4-pro", "stopReason": "stop" },
        "blocks": [{ "type": "text" }],
    });
    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(
            "invalid pi-ai replay state: model does not match assistant source".to_owned()
        )
    );
}

#[test]
fn a_pi_envelope_whose_block_count_drifted_is_refused() {
    let content = vec![text("done")];
    let state = json!({
        "response": { "kind": PI_REPLAY_KIND, "version": PI_REPLAY_VERSION, "api": "a", "provider": "deepseek", "model": MODEL, "stopReason": "stop" },
        "blocks": [],
    });
    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(
            "invalid pi-ai replay state: block count does not match assistant content".to_owned()
        )
    );
}

#[test]
fn a_pi_envelope_whose_block_kind_drifted_is_refused() {
    // The durable content is reasoning; the envelope says text. Trusting either half would send a
    // signature attached to the wrong block.
    let content = vec![reasoning("done")];
    let state = json!({
        "response": { "kind": PI_REPLAY_KIND, "version": PI_REPLAY_VERSION, "api": "a", "provider": "deepseek", "model": MODEL, "stopReason": "stop" },
        "blocks": [{ "type": "text" }],
    });
    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(
            "invalid pi-ai replay state: block 0 does not match assistant content".to_owned()
        )
    );
}

#[test]
fn a_message_with_no_pi_replay_state_is_absent() {
    let content = vec![text("done")];
    assert_eq!(
        read_pi_replay(None, &content, "deepseek", MODEL),
        ReplayOutcome::Absent
    );
}

#[test]
fn a_deepseek_envelope_is_not_readable_as_a_pi_one() {
    let content = vec![text("done")];
    let state = json!({
        "response": { "kind": DEEPSEEK_REPLAY_KIND, "version": DEEPSEEK_REPLAY_VERSION, "model": MODEL },
        "blocks": [{ "type": "text" }],
    });
    assert_eq!(
        read_pi_replay(Some(&state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded("invalid pi-ai replay state: unknown state kind".to_owned())
    );
}

#[test]
fn the_two_dialects_write_envelopes_neither_of_which_the_other_reads() {
    // A property rather than a case: whatever either writer produces, the other reader must refuse
    // it. This is what makes "adapter-private" a fact rather than a comment.
    let content = vec![text("answer")];
    let deepseek = deepseek_replay_state(MODEL, &[DeepSeekReplayBlock::text()]);
    let deepseek_state = serde_json::to_value(&deepseek).unwrap();
    assert!(matches!(
        read_pi_replay(Some(&deepseek_state), &content, "deepseek", MODEL),
        ReplayOutcome::Degraded(_)
    ));

    let pi = pi_replay_state(
        &pi_response(),
        &[PiReplayBlock::bare(ReplayBlockKind::Text)],
    );
    let pi_state = serde_json::to_value(&pi).unwrap();
    assert!(matches!(
        read_deepseek_replay(&content, MODEL, MODEL, Some(&pi_state)),
        ReplayOutcome::Degraded(_)
    ));
}

// =============================================================================================
// Routing — service.spec.ts `forAdapter`
// =============================================================================================

fn model_message(provider: &str, replay: Option<Value>) -> ChatMessage {
    message(
        ChatRole::Assistant,
        MessageSource::Model {
            provider: provider.to_owned(),
            model: "old-model".to_owned(),
            replay_state: replay,
        },
        vec![text("old response")],
    )
}

fn replay_state_of(message: &ChatMessage) -> Option<&Value> {
    match &message.source {
        MessageSource::Model { replay_state, .. } => replay_state.as_ref(),
        _ => None,
    }
}

#[test]
fn keeps_replay_state_when_the_target_adapter_also_serves_the_historical_provider() {
    let mut messages = vec![model_message(
        "historical",
        Some(json!({ "private": "state" })),
    )];
    let rewritten = strip_foreign_replay(&mut messages, |provider| provider == "historical");

    assert_eq!(rewritten, 0);
    assert_eq!(
        replay_state_of(&messages[0]),
        Some(&json!({ "private": "state" }))
    );
}

#[test]
fn strips_replay_state_but_preserves_provider_and_model_when_the_adapter_differs() {
    let mut messages = vec![model_message(
        "historical",
        Some(json!({ "private": "state" })),
    )];
    let rewritten = strip_foreign_replay(&mut messages, |provider| provider == "target");

    assert_eq!(rewritten, 1);
    assert_eq!(
        messages[0].source,
        MessageSource::Model {
            provider: "historical".to_owned(),
            model: "old-model".to_owned(),
            replay_state: None,
        }
    );
}

#[test]
fn a_message_with_no_replay_state_is_not_rewritten() {
    // Leaving it untouched keeps an unchanged request byte-identical, which is what lets a caller
    // decide whether it needs to rebuild an immutable request at all.
    let original = model_message("historical", None);
    let mut messages = vec![original.clone()];
    let rewritten = strip_foreign_replay(&mut messages, |_| false);

    assert_eq!(rewritten, 0);
    assert_eq!(messages[0], original);
}

#[test]
fn non_model_messages_are_never_rewritten() {
    let user = message(
        ChatRole::User,
        MessageSource::User { form: None },
        vec![text("summarize")],
    );
    let original = user.clone();
    let mut messages = vec![user];
    let rewritten = strip_foreign_replay(&mut messages, |_| false);

    assert_eq!(rewritten, 0);
    assert_eq!(messages[0], original);
}

#[test]
fn every_foreign_message_is_counted_and_only_foreign_ones_are_stripped() {
    let mut messages = vec![
        model_message("historical", Some(json!({ "a": 1 }))),
        model_message("target", Some(json!({ "b": 2 }))),
        model_message("third", Some(json!({ "c": 3 }))),
    ];
    let rewritten = strip_foreign_replay(&mut messages, |provider| provider == "target");

    assert_eq!(rewritten, 2);
    assert_eq!(replay_state_of(&messages[0]), None);
    assert_eq!(replay_state_of(&messages[1]), Some(&json!({ "b": 2 })));
    assert_eq!(replay_state_of(&messages[2]), None);
}

// =============================================================================================
// The exact envelopes both adapters will write
// =============================================================================================

#[test]
fn the_deepseek_stream_finish_envelope_matches_the_stream_spec_snapshot() {
    // `stream.spec.ts` asserts a tool-call response stores
    // `{ blocks: [{ type: 'reasoning', signature: 'signature' }, { type: 'tool-call' }] }`.
    let envelope = deepseek_replay_state(
        MODEL,
        &[
            DeepSeekReplayBlock::reasoning("signature"),
            DeepSeekReplayBlock::tool_call(),
        ],
    );
    assert_eq!(
        envelope.blocks,
        Some(vec![
            json!({ "type": "reasoning", "signature": "signature" }),
            json!({ "type": "tool-call" }),
        ])
    );
}

#[test]
fn the_pi_stream_finish_envelope_matches_the_convert_spec_snapshot() {
    // The `maps text events to text blocks` snapshot: one text block, no signature, requested model.
    let response = PiReplayResponse::new(
        "openai-completions",
        "deepseek",
        "deepseek-v4-flash",
        PiStopReason::Stop,
    );
    let envelope = pi_replay_state(
        &response,
        &[PiReplayBlock::Text {
            text_signature: None,
        }],
    );
    assert_eq!(
        serde_json::to_value(&envelope).unwrap(),
        json!({
            "response": {
                "kind": PI_REPLAY_KIND,
                "version": PI_REPLAY_VERSION,
                "api": "openai-completions",
                "provider": "deepseek",
                "model": "deepseek-v4-flash",
                "stopReason": "stop",
            },
            "blocks": [{ "type": "text" }],
        })
    );
}
