//! Streaming response replay state: the envelope, its two provider dialects, and the fold that
//! keeps it aligned with the blocks it describes.
//!
//! Upstream paths:
//!
//! - `packages/llm/llm/src/types.ts` (`ReplayEnvelope`, and the `replayState?` field on the
//!   `finish` chunk),
//! - `packages/llm/llm/src/assembler.ts` (`BlockAssembler`'s replay pass-through and pruning),
//! - `packages/llm/llm-deepseek/src/replay.ts` (the `DeepSeek Messages` envelope),
//! - `packages/llm/llm-pi-ai/src/replay.ts` (the pi-ai envelope and its reconstruction rules).
//!
//! # The gap this module closes
//!
//! [`StreamChunk::Finish`](crate::StreamChunk) in this build carries a
//! [`FinishReason`](crate::FinishReason) and nothing else. Upstream's `finish` chunk carries a
//! second field, `replayState`, and that field is the *only* way a provider's native response
//! metadata — Anthropic thinking signatures, pi-ai response ids, the reported model, the recorded
//! request-prefix digest — survives a turn boundary. Without it, a continuation of a conversation
//! whose earlier turns were produced by an adapter that signs its reasoning loses every signature,
//! and `deepseek`'s reasoning is dropped from the request rather than replayed.
//!
//! So this is not "a field is missing"; it is a capability. `/data/workspace/github/girder`'s
//! [`crate::MessageSource::Model`] already has a `replay_state` slot — the *durable* half of the
//! same fact — and nothing ever fills it. `llm_attempt_gap` documents the absence twice and
//! deliberately declines to model it, because the compact stream format it owns is about *content*
//! and the envelope is about *provider-native metadata*, which never belongs in a chunk log.
//!
//! # Why this module is pure data and pure functions
//!
//! Every question the envelope raises is answered by a fold over the values already in hand: what
//! shape does a provider write, what does it accept when reading back, and what happens to the
//! per-block half when assembly drops a block. None of it touches the network, the clock, or the
//! filesystem, which is exactly why it is the part of `packages/llm` worth porting first — the
//! adapters that would produce these values are already here ([`crate::llm::deepseek`],
//! [`crate::llm::openai`]), and they are the only thing standing between this module and a working
//! replay.
//!
//! # The three-valued answer
//!
//! Upstream's readers cannot return "no metadata" and "unusable metadata" as the same value,
//! because the caller does different things with each: the first is silent, the second calls
//! `onDegrade` with a diagnostic so an operator can see a log that has drifted. Rust's `Option`
//! collapses them, and a `Result` makes absence an error, so the answer is
//! [`ReplayOutcome`] — [`ReplayOutcome::Absent`], [`ReplayOutcome::Usable`], or
//! [`ReplayOutcome::Degraded`] carrying the diagnostic upstream would have passed to `onDegrade`.
//! A caller that ignores the distinction gets upstream's behaviour for free by treating `Absent`
//! and `Degraded` alike, which is what both adapters do with the result.
//!
//! # What is deliberately not here
//!
//! - **No `finish` chunk field.** Adding `replay_state` to [`crate::StreamChunk::Finish`] means
//!   editing `llm.rs`, which is the integrator's call. [`ReplayEnvelope`] is shaped so that the
//!   field can be added as `Option<ReplayEnvelope>` without any other change.
//! - **No prefix digest chain.** `packages/llm/llm-pi-ai/src/prefix-binding.ts` hashes pi-ai's own
//!   context objects with SHA-256; the digest binds to a serialization this port does not share, so
//!   computing one here would produce values nothing could ever compare against. What *is* here is
//!   the field the digest travels in ([`PiReplayResponse::prefix_digest`]) and its validation, which
//!   is the half a reader needs.
//! - **No stop-reason normalization.** `mapStopReason` (pi-ai) folds a provider's terminal event
//!   into a [`FinishReason`](crate::FinishReason) and needs `isContextOverflow`, a pi-ai-internal
//!   judgement this crate has no source for; the error taxonomy that part leans on is already in
//!   [`crate::llm::extra`].
//!
//! # One divergence, stated
//!
//! Upstream's assembler reads `envelope.blocks.length` after testing only `=== undefined`, so a
//! durable `blocks: null` throws a `TypeError` out of a *fold over saved data*. [`align_replay`]
//! treats a non-array `blocks` as misaligned and discards the envelope instead: metadata that
//! cannot be read is metadata the caller cannot use, and a reader that throws on a log it was
//! supposed to tolerate is worse than one that drops a field.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use girder_core::{ChatMessage, ContentBlock, MessageSource};

// =============================================================================================
// The envelope
// =============================================================================================

/// Adapter-private metadata attached to one model response, in two halves.
///
/// The split is the whole design: `response` is per-response and `blocks` is index-aligned with the
/// emitted content. Keeping them separate is what lets assembly *prune* the per-block half when it
/// drops a block while leaving the response half untouched — a single flat object could not express
/// "the ids are still valid but block 1 is gone".
///
/// Both halves stay opaque at this layer. The harness never reads `response`; only the adapter
/// that wrote it does, through one of the dialect readers below.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayEnvelope {
    /// Response-level adapter-private metadata (ids, native model, stop reason).
    pub response: Value,

    /// Per-block adapter-private metadata, one entry per emitted block in first-seen stream order.
    ///
    /// Absent means the metadata is independent of block structure, and the envelope passes through
    /// assembly unchanged. Present but a different length from the emitted block count means the two
    /// halves have drifted apart, and the whole envelope is discarded — see [`align_replay`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<Value>>,
}

impl ReplayEnvelope {
    /// Builds an envelope from its two halves.
    #[must_use]
    pub fn new(response: Value, blocks: Option<Vec<Value>>) -> Self {
        Self { response, blocks }
    }

    /// The number of per-block entries, or `None` when the envelope has no per-block half.
    ///
    /// `None` and `Some(0)` are different facts and the difference is load-bearing: an envelope
    /// with no per-block half survives an assembly that dropped blocks, while one that declared
    /// zero entries for a stream that produced blocks is misaligned and is discarded.
    #[must_use]
    pub fn block_count(&self) -> Option<usize> {
        self.blocks.as_ref().map(Vec::len)
    }
}

/// Which kind of content block a replay entry describes.
///
/// Mirrors the three tags both provider dialects share. It is a closed enum rather than a string
/// because every branch that consumes it is exhaustive over exactly these three, and a durable
/// fourth tag must be a compile error rather than a silently dropped entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayBlockKind {
    /// Visible text.
    #[default]
    Text,
    /// Model reasoning, the only kind that may carry a signature.
    Reasoning,
    /// A tool invocation.
    ToolCall,
}

impl ReplayBlockKind {
    /// The wire tag, which is also the content-block tag this kind pairs with.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Reasoning => "reasoning",
            Self::ToolCall => "tool-call",
        }
    }

    /// The kind a content block is, when it is one a replay envelope can describe.
    ///
    /// `image` and `file` return `None`. That is not an oversight: neither dialect can describe an
    /// attachment, so an assistant message carrying one cannot be replayed by signature at all, and
    /// a reader that mapped it to `text` would claim fidelity it does not have.
    #[must_use]
    pub fn of(block: &ContentBlock) -> Option<Self> {
        match block {
            ContentBlock::Text { .. } => Some(Self::Text),
            ContentBlock::Reasoning { .. } => Some(Self::Reasoning),
            ContentBlock::ToolCall { .. } => Some(Self::ToolCall),
            ContentBlock::Image { .. } | ContentBlock::File { .. } => None,
        }
    }
}

/// What a reader learned about one message's replay metadata.
///
/// Three values rather than two, for the reason in the module docs: absence is normal and silent,
/// while an *unusable* envelope is a diagnostic the operator needs and the request does not.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplayOutcome<T> {
    /// The message carries no envelope, or one written for a different target model.
    ///
    /// Both are ordinary history: a message from another adapter, or one this adapter can no longer
    /// speak for. The caller degrades to provider-neutral content without a diagnostic.
    Absent,
    /// The envelope is valid and index-aligned with the content.
    Usable(T),
    /// The envelope exists but cannot be used, with the diagnostic upstream passes to `onDegrade`.
    Degraded(String),
}

impl<T> ReplayOutcome<T> {
    /// The usable value, discarding both kinds of failure.
    #[must_use]
    pub fn usable(self) -> Option<T> {
        match self {
            Self::Usable(value) => Some(value),
            Self::Absent | Self::Degraded(_) => None,
        }
    }

    /// The diagnostic, when the envelope was present but unusable.
    #[must_use]
    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::Degraded(reason) => Some(reason),
            Self::Absent | Self::Usable(_) => None,
        }
    }
}

// =============================================================================================
// Assembly: keeping the per-block half in step with the blocks
// =============================================================================================

/// The keep/drop mask assembly applies to a block list, given the terminal reason.
///
/// The one drop rule upstream has is truncation: when the model hit its output limit, a tool call
/// in the stream is incomplete — its JSON may be cut mid-string — so executing it would run a
/// tool the model never finished asking for. Every other reason keeps every block.
///
/// A mask rather than a filtered list because the *positions* are what the replay envelope is
/// indexed by: filtering first and pruning second would require the caller to recompute which
/// entries belonged to which survivors.
#[must_use]
pub fn truncation_mask(blocks: &[ContentBlock], truncated: bool) -> Vec<bool> {
    blocks
        .iter()
        .map(|block| !(truncated && matches!(block, ContentBlock::ToolCall { .. })))
        .collect()
}

/// Applies a keep/drop mask to a block list.
#[must_use]
pub fn retain_blocks(blocks: Vec<ContentBlock>, kept: &[bool]) -> Vec<ContentBlock> {
    blocks
        .into_iter()
        .enumerate()
        .filter_map(|(index, block)| kept.get(index).copied().unwrap_or(true).then_some(block))
        .collect()
}

/// Prunes an envelope's per-block half in step with a keep/drop mask.
///
/// This is upstream `BlockAssembler.assembled`'s replay rule, stated once:
///
/// | envelope | entries vs. kept | result |
/// | --- | --- | --- |
/// | absent | — | absent |
/// | no `blocks` half | — | passes through unchanged, even when blocks were dropped |
/// | `blocks` | different length | **discarded entirely** — the halves have drifted |
/// | `blocks` | same length | pruned with the mask |
///
/// The third row is the surprising one and it is deliberate. An envelope whose entries do not
/// correspond one-to-one with the emitted blocks cannot be repaired by guessing: entry `i` might
/// describe block `i` or might describe the block that was dropped before it. Since a *wrong*
/// signature is worse than no signature — the provider rejects the request — the whole envelope is
/// dropped. The second row is the mirror image: an envelope with no per-block half makes no claim
/// about block structure, so dropping a block cannot invalidate it.
#[must_use]
pub fn align_replay(envelope: Option<&ReplayEnvelope>, kept: &[bool]) -> Option<ReplayEnvelope> {
    let envelope = envelope?;
    let Some(entries) = envelope.blocks.as_ref() else {
        return Some(envelope.clone());
    };
    if entries.len() != kept.len() {
        return None;
    }
    let pruned: Vec<Value> = entries
        .iter()
        .enumerate()
        .filter(|(index, _)| kept.get(*index).copied().unwrap_or(true))
        .map(|(_, entry)| entry.clone())
        .collect();
    Some(ReplayEnvelope {
        response: envelope.response.clone(),
        blocks: Some(pruned),
    })
}

/// Folds a finished stream the way the assembler does: drops truncated tool calls, then prunes the
/// envelope to match.
///
/// One function rather than two calls at the call site because the two steps are only correct
/// together: dropping blocks without pruning leaves an envelope whose entries now describe the
/// wrong blocks, which is precisely the misalignment [`align_replay`] discards.
#[must_use]
pub fn assemble_replay(
    blocks: Vec<ContentBlock>,
    truncated: bool,
    envelope: Option<&ReplayEnvelope>,
) -> (Vec<ContentBlock>, Option<ReplayEnvelope>) {
    let kept = truncation_mask(&blocks, truncated);
    let replay = align_replay(envelope, &kept);
    (retain_blocks(blocks, &kept), replay)
}

// =============================================================================================
// The DeepSeek Messages dialect
// =============================================================================================

/// One index-aligned entry of a `DeepSeek Messages` envelope.
///
/// Native thinking metadata only: the durable content blocks own all response text, so this carries
/// a tag and — for reasoning alone — the provider's opaque signature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeepSeekReplayBlock {
    /// Which kind of block this entry describes.
    pub kind: ReplayBlockKind,
    /// The provider's signature, for reasoning blocks.
    pub signature: Option<String>,
}

impl DeepSeekReplayBlock {
    /// A text entry.
    #[must_use]
    pub fn text() -> Self {
        Self {
            kind: ReplayBlockKind::Text,
            signature: None,
        }
    }

    /// A tool-call entry.
    #[must_use]
    pub fn tool_call() -> Self {
        Self {
            kind: ReplayBlockKind::ToolCall,
            signature: None,
        }
    }

    /// A reasoning entry carrying `signature`.
    #[must_use]
    pub fn reasoning(signature: impl Into<String>) -> Self {
        Self {
            kind: ReplayBlockKind::Reasoning,
            signature: Some(signature.into()),
        }
    }
}

/// The wire tag of the `DeepSeek Messages` dialect.
pub const DEEPSEEK_REPLAY_KIND: &str = "deepseek-messages";

/// The only `DeepSeek Messages` envelope version this build writes or accepts.
pub const DEEPSEEK_REPLAY_VERSION: u64 = 1;

/// Builds the envelope a completed `DeepSeek Messages` response stores.
///
/// The shape is the durable contract, so it is written out literally:
///
/// ```json
/// { "response": { "kind": "deepseek-messages", "version": 1, "model": "<requested>" },
///   "blocks": [ { "type": "text" }, { "type": "reasoning", "signature": "…" } ] }
/// ```
///
/// An entry omits `signature` rather than writing `null`, because the reader distinguishes
/// "absent" (no signature) from "present but not a string" (invalid), and `null` is the second.
#[must_use]
pub fn deepseek_replay_state(model: &str, blocks: &[DeepSeekReplayBlock]) -> ReplayEnvelope {
    let mut response = Map::new();
    response.insert(
        "kind".to_owned(),
        Value::String(DEEPSEEK_REPLAY_KIND.to_owned()),
    );
    response.insert("version".to_owned(), Value::from(DEEPSEEK_REPLAY_VERSION));
    response.insert("model".to_owned(), Value::String(model.to_owned()));

    let entries = blocks
        .iter()
        .map(|block| {
            let mut entry = Map::new();
            entry.insert(
                "type".to_owned(),
                Value::String(block.kind.tag().to_owned()),
            );
            if let Some(signature) = &block.signature {
                entry.insert("signature".to_owned(), Value::String(signature.clone()));
            }
            Value::Object(entry)
        })
        .collect();

    ReplayEnvelope::new(Value::Object(response), Some(entries))
}

/// Reads and validates a `DeepSeek Messages` envelope against the durable content it describes.
///
/// The checks are upstream's, in upstream's order, and the order matters because each one assumes
/// the previous one held:
///
/// 1. the state is a JSON object,
/// 2. so is its `response` half,
/// 3. `kind` and `version` are the ones this build writes,
/// 4. the recorded model is the *source* message's model — a signature from another model's
///    response is not portable,
/// 5. the entry count matches the content count,
/// 6. each entry's tag matches its block, and is one of the three known tags,
/// 7. a signature appears only on a reasoning entry and only as a string.
///
/// Step 8 is not a validation but a decision: unless the recorded model is also the **target**
/// model, the answer is [`ReplayOutcome::Absent`] rather than a diagnostic. Replaying a signature
/// into a request for a different model is a normal thing to decline, not a damaged log.
#[must_use]
pub fn read_deepseek_replay(
    content: &[ContentBlock],
    source_model: &str,
    target_model: &str,
    state: Option<&Value>,
) -> ReplayOutcome<Vec<DeepSeekReplayBlock>> {
    let Some(state) = state else {
        return ReplayOutcome::Absent;
    };
    match validate_deepseek_replay(content, source_model, state) {
        Ok(blocks) if source_model == target_model => ReplayOutcome::Usable(blocks),
        // The envelope is intact but was written for another model. Upstream declines it, because
        // replaying a signature into a different model's request is a normal thing to refuse rather
        // than a damaged log — so this is silence, not a diagnostic.
        Ok(_) => ReplayOutcome::Absent,
        Err(fault) => ReplayOutcome::Degraded(format!("DeepSeek Messages replay: {fault}")),
    }
}

/// A reason a `DeepSeek Messages` envelope cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeepSeekReplayFault {
    /// The envelope is not a JSON object.
    NotAnObject,
    /// The `response` half is missing or is not an object.
    NotAResponseObject,
    /// `kind`/`version` name a dialect or generation this build does not write.
    UnsupportedKindOrVersion,
    /// The envelope was written by another model.
    ForeignModel,
    /// The entry count and the content count disagree.
    BlockCount,
    /// An entry describes a block of a different kind, or of a kind with no replay form.
    BlockType,
    /// A signature appears where it is not allowed, or is not a string.
    Signature,
}

impl std::fmt::Display for DeepSeekReplayFault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::NotAnObject | Self::NotAResponseObject => {
                "DeepSeek Messages expected a JSON object"
            }
            Self::UnsupportedKindOrVersion => "unsupported kind or version",
            Self::ForeignModel => "model does not match assistant source model",
            Self::BlockCount => "block count mismatch",
            Self::BlockType => "block type mismatch",
            Self::Signature => "invalid signature",
        };
        formatter.write_str(text)
    }
}

/// The validation half of [`read_deepseek_replay`].
fn validate_deepseek_replay(
    content: &[ContentBlock],
    source_model: &str,
    state: &Value,
) -> Result<Vec<DeepSeekReplayBlock>, DeepSeekReplayFault> {
    use DeepSeekReplayFault as Fault;

    let envelope = state.as_object().ok_or(Fault::NotAnObject)?;
    let response = envelope
        .get("response")
        .and_then(Value::as_object)
        .ok_or(Fault::NotAResponseObject)?;
    if response.get("kind").and_then(Value::as_str) != Some(DEEPSEEK_REPLAY_KIND)
        || response.get("version").and_then(Value::as_u64) != Some(DEEPSEEK_REPLAY_VERSION)
    {
        return Err(Fault::UnsupportedKindOrVersion);
    }
    if response.get("model").and_then(Value::as_str) != Some(source_model) {
        return Err(Fault::ForeignModel);
    }
    let entries = envelope
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or(Fault::BlockCount)?;
    if entries.len() != content.len() {
        return Err(Fault::BlockCount);
    }
    let mut blocks = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let entry = entry.as_object().ok_or(Fault::BlockType)?;
        let tag = entry.get("type").and_then(Value::as_str);
        let expected = content.get(index).and_then(ReplayBlockKind::of);
        let kind = match tag {
            Some("text") => ReplayBlockKind::Text,
            Some("reasoning") => ReplayBlockKind::Reasoning,
            Some("tool-call") => ReplayBlockKind::ToolCall,
            _ => return Err(Fault::BlockType),
        };
        if Some(kind) != expected {
            return Err(Fault::BlockType);
        }
        let signature = match entry.get("signature") {
            None => None,
            Some(Value::String(signature)) if kind == ReplayBlockKind::Reasoning => {
                Some(signature.clone())
            }
            Some(_) => return Err(Fault::Signature),
        };
        blocks.push(DeepSeekReplayBlock { kind, signature });
    }
    Ok(blocks)
}

/// Reads the `blocks` array of a `pi-ai` envelope, one entry at a time.
fn read_pi_blocks(entries: &[Value]) -> Result<Vec<PiReplayBlock>, String> {
    let mut blocks = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let entry = entry
            .as_object()
            .ok_or_else(|| format!("block {index} must be an object"))?;
        let tag = entry.get("type").and_then(Value::as_str);
        let kind = match tag {
            Some("text") => ReplayBlockKind::Text,
            Some("reasoning") => ReplayBlockKind::Reasoning,
            Some("tool-call") => ReplayBlockKind::ToolCall,
            _ => return Err(format!("block {index} has an unknown type")),
        };
        for key in [
            "textSignature",
            "thinkingSignature",
            "thoughtSignature",
            "namespace",
        ] {
            if let Some(value) = entry.get(key) {
                if !value.is_string() {
                    return Err(format!("block {index} {key} must be a string"));
                }
            }
        }
        if let Some(value) = entry.get("redacted") {
            if !value.is_boolean() {
                return Err(format!("block {index} redacted must be boolean"));
            }
        }
        blocks.push(match kind {
            ReplayBlockKind::Text => PiReplayBlock::Text {
                text_signature: entry
                    .get("textSignature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            ReplayBlockKind::Reasoning => PiReplayBlock::Reasoning {
                thinking_signature: entry
                    .get("thinkingSignature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                redacted: entry.get("redacted").and_then(Value::as_bool),
            },
            ReplayBlockKind::ToolCall => PiReplayBlock::ToolCall {
                thought_signature: entry
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                namespace: entry
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        });
    }
    Ok(blocks)
}

// =============================================================================================
// The pi-ai dialect
// =============================================================================================

/// The terminal state pi-ai reports for an assistant message.
///
/// The five values are pi-ai's `stopReason` domain, which is *not* the harness's
/// [`FinishReason`](crate::FinishReason): `pending` and `deferred` are pi-ai states with no harness
/// equivalent, and `error`/`aborted` carry an `errorMessage` here rather than a failure code. Replay
/// stores the native value so a reconstructed message reports what the provider actually said.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PiStopReason {
    /// The model finished its turn.
    #[serde(rename = "stop")]
    Stop,
    /// The model hit its output limit.
    #[serde(rename = "length")]
    Length,
    /// The model asked for tools.
    #[serde(rename = "toolUse")]
    ToolUse,
    /// The provider reported a failure.
    #[serde(rename = "error")]
    Error,
    /// The caller cancelled.
    #[serde(rename = "aborted")]
    Aborted,
}

impl PiStopReason {
    /// The wire tag, which is also the value the validator accepts.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolUse => "toolUse",
            Self::Error => "error",
            Self::Aborted => "aborted",
        }
    }

    /// Parses a wire tag, refusing anything outside the domain.
    ///
    /// `pending` and `deferred` are deliberately not accepted: pi-ai's own replay validator rejects
    /// them, because a stored message that ended pending is not a message any later request can
    /// reconstruct.
    #[must_use]
    pub fn parse(tag: &str) -> Option<Self> {
        match tag {
            "stop" => Some(Self::Stop),
            "length" => Some(Self::Length),
            "toolUse" => Some(Self::ToolUse),
            "error" => Some(Self::Error),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// One index-aligned entry of a pi-ai envelope.
///
/// The field each variant carries is the provider's opaque signature for that block, and the names
/// differ per kind because pi-ai names them differently per kind. That asymmetry is preserved
/// rather than flattened into `signature` because the two are not interchangeable: an Anthropic
/// thinking signature is a different artifact from a text signature and from a tool call's thought
/// signature, and a replay that swapped them would be rejected by the provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PiReplayBlock {
    /// Visible text, with the provider's text signature when it issued one.
    Text {
        /// Opaque signature for the text block.
        text_signature: Option<String>,
    },
    /// Reasoning, with its thinking signature and redaction flag.
    Reasoning {
        /// Opaque signature for the thinking block.
        thinking_signature: Option<String>,
        /// Whether the provider redacted the thinking content.
        redacted: Option<bool>,
    },
    /// A tool call, with its thought signature and namespace.
    ToolCall {
        /// Opaque thought signature for the tool call.
        thought_signature: Option<String>,
        /// Provider-side namespace the tool belongs to.
        namespace: Option<String>,
    },
}

impl PiReplayBlock {
    /// The kind this entry describes.
    #[must_use]
    pub fn kind(&self) -> ReplayBlockKind {
        match self {
            Self::Text { .. } => ReplayBlockKind::Text,
            Self::Reasoning { .. } => ReplayBlockKind::Reasoning,
            Self::ToolCall { .. } => ReplayBlockKind::ToolCall,
        }
    }

    /// An entry for `block`, with no signature of any kind.
    #[must_use]
    pub fn bare(kind: ReplayBlockKind) -> Self {
        match kind {
            ReplayBlockKind::Text => Self::Text {
                text_signature: None,
            },
            ReplayBlockKind::Reasoning => Self::Reasoning {
                thinking_signature: None,
                redacted: None,
            },
            ReplayBlockKind::ToolCall => Self::ToolCall {
                thought_signature: None,
                namespace: None,
            },
        }
    }
}

/// The response half of a pi-ai envelope.
///
/// Every field but the five required ones is optional *and* meaningful when absent: `responseModel`
/// records what the provider said the model was when that differs from the requested identity, and
/// `prefix_digest` records the request prefix the signature was bound to. The reader distinguishes
/// "absent" from "present but empty", so the writer omits rather than defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiReplayResponse {
    /// Wire protocol the response came from, for example `anthropic-messages`.
    pub api: String,
    /// Provider route that produced it.
    pub provider: String,
    /// Requested model identity, matching the durable assistant source.
    pub model: String,
    /// Provider-reported model, when it differs from the requested one.
    pub response_model: Option<String>,
    /// Provider-issued response id.
    pub response_id: Option<String>,
    /// Provider-native thinking effort.
    pub provider_thinking_level: Option<String>,
    /// The terminal state.
    pub stop_reason: PiStopReason,
    /// Digest of the request context the response was generated from.
    pub prefix_digest: Option<String>,
}

impl PiReplayResponse {
    /// A response description with every optional field unset.
    #[must_use]
    pub fn new(
        api: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        stop_reason: PiStopReason,
    ) -> Self {
        Self {
            api: api.into(),
            provider: provider.into(),
            model: model.into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            stop_reason,
            prefix_digest: None,
        }
    }
}

/// The wire tag of the pi-ai dialect.
pub const PI_REPLAY_KIND: &str = "pi-ai";

/// The only pi-ai envelope version this build writes or accepts.
pub const PI_REPLAY_VERSION: u64 = 2;

/// A validated pi-ai envelope: both halves, ready for message reconstruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiReplay {
    /// The per-response half.
    pub response: PiReplayResponse,
    /// The per-block half, index-aligned with the durable content.
    pub blocks: Vec<PiReplayBlock>,
}

/// Builds the envelope a completed pi-ai response stores.
///
/// The per-block half is index-aligned with the streamed blocks, so assembly prunes an entry
/// together with its block whenever it drops one — see [`align_replay`].
#[must_use]
pub fn pi_replay_state(response: &PiReplayResponse, blocks: &[PiReplayBlock]) -> ReplayEnvelope {
    let mut head = Map::new();
    head.insert("kind".to_owned(), Value::String(PI_REPLAY_KIND.to_owned()));
    head.insert("version".to_owned(), Value::from(PI_REPLAY_VERSION));
    head.insert("api".to_owned(), Value::String(response.api.clone()));
    head.insert(
        "provider".to_owned(),
        Value::String(response.provider.clone()),
    );
    head.insert("model".to_owned(), Value::String(response.model.clone()));
    let optional = [
        ("responseModel", response.response_model.as_ref()),
        ("responseId", response.response_id.as_ref()),
        (
            "providerThinkingLevel",
            response.provider_thinking_level.as_ref(),
        ),
        ("prefixDigest", response.prefix_digest.as_ref()),
    ];
    for (key, value) in optional {
        if let Some(value) = value {
            head.insert(key.to_owned(), Value::String(value.clone()));
        }
    }
    head.insert(
        "stopReason".to_owned(),
        Value::String(response.stop_reason.tag().to_owned()),
    );

    let entries = blocks
        .iter()
        .map(|block| {
            let mut entry = Map::new();
            match block {
                PiReplayBlock::Text { text_signature } => {
                    entry.insert("type".to_owned(), Value::String("text".to_owned()));
                    if let Some(signature) = text_signature {
                        entry.insert("textSignature".to_owned(), Value::String(signature.clone()));
                    }
                }
                PiReplayBlock::Reasoning {
                    thinking_signature,
                    redacted,
                } => {
                    entry.insert("type".to_owned(), Value::String("reasoning".to_owned()));
                    if let Some(signature) = thinking_signature {
                        entry.insert(
                            "thinkingSignature".to_owned(),
                            Value::String(signature.clone()),
                        );
                    }
                    if let Some(redacted) = redacted {
                        entry.insert("redacted".to_owned(), Value::Bool(*redacted));
                    }
                }
                PiReplayBlock::ToolCall {
                    thought_signature,
                    namespace,
                } => {
                    entry.insert("type".to_owned(), Value::String("tool-call".to_owned()));
                    if let Some(signature) = thought_signature {
                        entry.insert(
                            "thoughtSignature".to_owned(),
                            Value::String(signature.clone()),
                        );
                    }
                    if let Some(namespace) = namespace {
                        entry.insert("namespace".to_owned(), Value::String(namespace.clone()));
                    }
                }
            }
            Value::Object(entry)
        })
        .collect();

    ReplayEnvelope::new(Value::Object(head), Some(entries))
}

/// Reads and validates a pi-ai envelope against the durable content it describes.
///
/// Two layers of checks, in upstream's order, and the split between them is visible in the
/// diagnostics: the first layer answers "is this a pi-ai envelope at all", the second "does it still
/// describe *this* message".
///
/// The reader is strict on purpose. Upstream's comment on the equivalent code says durable content
/// is authoritative and replay metadata only restores native fidelity, so *any* disagreement — a
/// foreign kind, an older version, a malformed value, a block count that drifted — must degrade the
/// one message rather than fail the request. That is why every fault becomes
/// [`ReplayOutcome::Degraded`] with a message naming the field, and why nothing here returns an
/// error the caller is expected to propagate.
#[must_use]
pub fn read_pi_replay(
    state: Option<&Value>,
    content: &[ContentBlock],
    source_provider: &str,
    source_model: &str,
) -> ReplayOutcome<PiReplay> {
    let Some(state) = state else {
        return ReplayOutcome::Absent;
    };
    match validate_pi_replay(state, content, source_provider, source_model) {
        Ok(replay) => ReplayOutcome::Usable(replay),
        Err(detail) => ReplayOutcome::Degraded(format!("invalid pi-ai replay state: {detail}")),
    }
}

/// Renders a JSON value the way JavaScript's `String()` would.
///
/// Upstream interpolates the raw `version` value into `unsupported version ${String(...)}`, so a
/// durable `version: "two"` produces `unsupported version two` and `version: undefined` produces
/// `unsupported version undefined`. Reproducing that keeps a diagnostic from a drifted log readable
/// rather than turning it into "unsupported version".
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Value::Null) => "null".to_owned(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.clone(),
        // `String([])` and `String({})` are `""` and `"[object Object]"`; `String([1,2])` is `"1,2"`.
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| js_string(Some(item)))
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_owned(),
    }
}

/// The validation half of [`read_pi_replay`], returning the diagnostic detail on failure.
fn validate_pi_replay(
    state: &Value,
    content: &[ContentBlock],
    source_provider: &str,
    source_model: &str,
) -> Result<PiReplay, String> {
    let replay = read_pi_shape(state)?;
    assert_pi_describes(&replay, content, source_provider, source_model)?;
    Ok(replay)
}

/// The first layer: is this a `pi-ai` v2 replay envelope at all?
///
/// Separate from the second layer because the two failures mean different things -- a shape failure
/// is a state this build cannot read, while a mismatch is a state that describes some other message.
/// A single function returning one `String` makes a caller unable to tell them apart.
fn read_pi_shape(state: &Value) -> Result<PiReplay, String> {
    let envelope = state
        .as_object()
        .ok_or_else(|| "expected a replay envelope".to_owned())?;
    let response = envelope
        .get("response")
        .and_then(Value::as_object)
        .ok_or_else(|| "expected a response object".to_owned())?;

    if response.get("kind").and_then(Value::as_str) != Some(PI_REPLAY_KIND) {
        return Err("unknown state kind".to_owned());
    }
    if response.get("version").and_then(Value::as_u64) != Some(PI_REPLAY_VERSION) {
        return Err(format!(
            "unsupported version {}",
            js_string(response.get("version"))
        ));
    }
    for key in ["api", "provider", "model"] {
        match response.get(key).and_then(Value::as_str) {
            Some(value) if !value.is_empty() => {}
            _ => return Err(format!("{key} must be a non-empty string")),
        }
    }
    // The stop reason is validated here and read into the struct later: a state naming a reason this
    // build does not know is unreadable, not merely incomplete.
    if response
        .get("stopReason")
        .and_then(Value::as_str)
        .and_then(PiStopReason::parse)
        .is_none()
    {
        return Err("unknown stopReason".to_owned());
    }
    for key in [
        "responseModel",
        "responseId",
        "providerThinkingLevel",
        "prefixDigest",
    ] {
        // `null` counts as present, exactly as upstream's `!== undefined` does: a writer that meant
        // "absent" omits the key, so a null is a value the reader did not expect.
        if let Some(value) = response.get(key) {
            if !value.is_string() {
                return Err(format!("{key} must be a string"));
            }
        }
    }

    let entries = envelope
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or_else(|| "blocks must be an array".to_owned())?;
    Ok(PiReplay {
        response: read_pi_response(response)?,
        blocks: read_pi_blocks(entries)?,
    })
}

/// Reads the `response` half of a `pi-ai` envelope.
fn read_pi_response(response: &Map<String, Value>) -> Result<PiReplayResponse, String> {
    let required = |key: &str| {
        response
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let stop_reason = response
        .get("stopReason")
        .and_then(Value::as_str)
        .and_then(PiStopReason::parse)
        .ok_or_else(|| "unknown stopReason".to_owned())?;
    Ok(PiReplayResponse {
        api: required("api"),
        provider: required("provider"),
        model: required("model"),
        response_model: response
            .get("responseModel")
            .and_then(Value::as_str)
            .map(str::to_owned),
        response_id: response
            .get("responseId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        provider_thinking_level: response
            .get("providerThinkingLevel")
            .and_then(Value::as_str)
            .map(str::to_owned),
        stop_reason,
        prefix_digest: response
            .get("prefixDigest")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn assert_pi_describes(
    replay: &PiReplay,
    content: &[ContentBlock],
    source_provider: &str,
    source_model: &str,
) -> Result<(), String> {
    if replay.response.provider != source_provider {
        return Err("provider does not match assistant source".to_owned());
    }
    if replay.response.model != source_model {
        return Err("model does not match assistant source".to_owned());
    }
    if replay.blocks.len() != content.len() {
        return Err("block count does not match assistant content".to_owned());
    }
    for (index, block) in replay.blocks.iter().enumerate() {
        let matched = content
            .get(index)
            .and_then(ReplayBlockKind::of)
            .is_some_and(|kind| kind == block.kind());
        if !matched {
            return Err(format!("block {index} does not match assistant content"));
        }
    }
    Ok(())
}

// =============================================================================================
// Routing: whose replay state may a request carry?
// =============================================================================================

/// Strips replay state from assistant messages addressed to an adapter that does not own them.
///
/// This is `LlmRuntime.forAdapter`, and it exists because replay state is *adapter-private*: only
/// the adapter instance that wrote it knows the dialect. A request routed to a different adapter
/// that carried the previous adapter's envelope would hand it metadata it cannot parse — at best it
/// degrades, at worst it reinterprets a foreign `{kind: "pi-ai"}` as its own.
///
/// The rule upstream applies is one message at a time and is *instance*, not provider, identity:
/// a historical provider is kept when the current adapter also serves that provider. So the
/// predicate takes a provider name and answers whether this adapter owns it, which is why the
/// caller supplies a closure rather than a list — the registry that knows is the caller's.
///
/// Only assistant messages from a model are touched; a user or tool message has no envelope, and a
/// message with no envelope is left alone (not rewritten into an equal copy) so an unchanged request
/// stays byte-identical. The return value is the number of messages rewritten, which is what lets a
/// caller decide whether it needs to rebuild an immutable request at all.
pub fn strip_foreign_replay(messages: &mut [ChatMessage], owns: impl Fn(&str) -> bool) -> usize {
    let mut rewritten = 0;
    for message in messages.iter_mut() {
        let MessageSource::Model {
            provider,
            model,
            replay_state,
        } = &message.source
        else {
            continue;
        };
        if replay_state.is_none() || owns(provider) {
            continue;
        }
        message.source = MessageSource::Model {
            provider: provider.clone(),
            model: model.clone(),
            replay_state: None,
        };
        rewritten += 1;
    }
    rewritten
}
