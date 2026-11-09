//! The language-model seam: request shapes, stream chunks, and the provider trait.
//!
//! Upstream `packages/llm/llm` is 4,145 lines of source across a dozen files. This module carries
//! only what the vertical slice needs: the message and content vocabulary, the chunk vocabulary a
//! stream emits, and one trait a provider implements.
//!
//! # What was dropped, and why
//!
//! Upstream's `LlmConfigurableProvider`, model discovery, image pricing, badges, and replay
//! envelopes are all about *administering* providers from the GUI. None of them is on the path from
//! a prompt to a streamed answer, so none of them is here. They belong with phase 6's RPC surface,
//! where the client asks for them.
//!
//! # Cancellation is a parameter
//!
//! Every fallible async seam takes a [`CancellationToken`] explicitly. Upstream passes an
//! `AbortSignal` held in `exec.signal` and relies on holders registering listeners; Rust has no
//! equivalent ambient propagation (`task_local!` does not cross `tokio::spawn` — measured in phase
//! 1), so the token travels as an argument. An explicitly-passed token cannot be forgotten silently
//! the way an ambient one can be ignored.

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::brand::ToolCallId;
use crate::scope::BoxFut;

/// A boxed chunk stream.
///
/// `Send` because the loop spawns and the CLI drives it from another task; see migration-plan
/// §1.3 constraint three for what that forces on providers.
pub type ChunkStream = Pin<Box<dyn Stream<Item = StreamChunk> + Send>>;

/// One block of message or tool content.
///
/// Mirrors upstream's `ContentBlockMap` minus the merge-extensibility: a plugin cannot add a block
/// type, because Rust enums are closed. Upstream's `declare module` extension point is the same
/// mechanism the plan abandons (§1.2, third layer), and the known block types are enough for the
/// slice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentBlock {
    /// Text visible to the user.
    Text {
        /// The text.
        text: String,
    },

    /// Reasoning content, distinct from visible text.
    Reasoning {
        /// The reasoning text.
        text: String,
    },

    /// A tool invocation requested by the model.
    ///
    /// `arguments` is the raw JSON string the model produced, not a parsed value: a provider may
    /// stream it in pieces and the session log must record exactly what arrived.
    ToolCall {
        /// Provider-issued call id, correlating with the matching result.
        id: ToolCallId,
        /// Tool name as the model wrote it.
        name: String,
        /// Raw JSON string of the arguments.
        arguments: String,
    },

    /// An image reference. The bytes live in the attachment service, not in the log.
    Image {
        /// Opaque attachment reference.
        attachment: String,
    },

    /// A file reference. Providers see deterministic handle text, never the bytes.
    File {
        /// Opaque attachment reference.
        attachment: String,
    },
}

impl ContentBlock {
    /// The block's `type` tag.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Reasoning { .. } => "reasoning",
            Self::ToolCall { .. } => "tool-call",
            Self::Image { .. } => "image",
            Self::File { .. } => "file",
        }
    }
}

/// Who authored a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The end user.
    User,
    /// The model.
    Assistant,
    /// The harness, carrying tool results and injected context.
    Tool,
}

/// One conversation message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Who authored it.
    pub role: Role,
    /// Its blocks.
    pub content: Vec<ContentBlock>,
}

/// A JSON-schema description of a tool, as sent to the model.
///
/// `parameters` stays a raw JSON value rather than a typed schema: every provider wants the JSON
/// Schema object verbatim, and parsing it here would be work with no consumer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Tool name the model calls.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema object for the arguments.
    pub parameters: serde_json::Value,
}

/// Why a response stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    /// The model finished its turn.
    Stop,
    /// The model hit its output limit.
    Length,
    /// The model asked for one or more tools.
    ToolCalls,
    /// The caller cancelled.
    Aborted,
    /// The provider failed.
    Error {
        /// Human-readable failure.
        message: String,
    },
}

/// Token accounting for one request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt tokens billed.
    pub input: u64,
    /// Completion tokens billed.
    pub output: u64,
    /// Cached prompt tokens, when the provider reports them.
    pub cached: u64,
}

/// One delta in a streamed response.
///
/// The discriminant list mirrors upstream's `StreamChunk` exactly, including that tool arguments
/// stay raw JSON strings and that `usage` arrives before the terminal `finish`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum StreamChunk {
    /// A block is starting at `index`.
    BlockStart {
        /// Position this block occupies in the assembled message.
        index: usize,
        /// What kind of block it is.
        block_type: String,
    },

    /// More visible text for the block at `index`.
    TextDelta {
        /// Block position.
        index: usize,
        /// Appended text.
        text: String,
    },

    /// More reasoning text for the block at `index`.
    ReasoningDelta {
        /// Block position.
        index: usize,
        /// Appended text.
        text: String,
    },

    /// More of a tool call's name or arguments.
    ToolCallDelta {
        /// Block position.
        index: usize,
        /// Call id, stable across the call's deltas.
        id: ToolCallId,
        /// Name, when this delta carries it.
        name: Option<String>,
        /// Appended raw JSON.
        arguments_delta: String,
    },

    /// The block at `index` is complete.
    BlockEnd {
        /// Block position.
        index: usize,
        /// The assembled block.
        block: ContentBlock,
    },

    /// Token accounting. Always precedes `finish`.
    Usage(TokenUsage),

    /// The response is complete.
    Finish {
        /// Why it stopped.
        reason: FinishReason,
    },
}

/// A fully assembled response, built by folding the chunk stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssistantMessage {
    /// The assembled blocks, in index order.
    pub content: Vec<ContentBlock>,
    /// Token accounting, when the provider reported it.
    pub usage: Option<TokenUsage>,
    /// Why the response stopped.
    pub finish: Option<FinishReason>,
}

impl AssistantMessage {
    /// Folds a chunk stream into one message.
    ///
    /// Kept separate from the provider so every provider is assembled the same way — the one place
    /// where "accumulate tool arguments as a string" is decided.
    ///
    /// # Errors
    ///
    /// Returns [`AssembleError::Unfinished`] when the stream ends without a terminal `finish`. A
    /// stream that just stops is a provider bug, and silently accepting a truncated answer would
    /// make it invisible.
    pub fn from_chunks(
        chunks: impl IntoIterator<Item = StreamChunk>,
    ) -> Result<Self, AssembleError> {
        let mut slots: Vec<Option<ContentBlock>> = Vec::new();
        // The block kind announced by `block-start`, used only when no `block-end` arrived.
        let mut kinds: Vec<String> = Vec::new();
        let mut text: Vec<TextAccumulator> = Vec::new();
        let mut usage = None;
        let mut finish = None;

        for chunk in chunks {
            match chunk {
                StreamChunk::BlockStart { index, block_type } => {
                    // Record only the *kind* of block being streamed; the content is accumulated
                    // from the deltas and materialized below. Seeding an empty block here would
                    // shadow the accumulated text at materialization time.
                    ensure_len(&mut slots, index);
                    ensure_len(&mut kinds, index);
                    kinds[index] = block_type;
                }
                StreamChunk::TextDelta { index, text: delta } => {
                    ensure_len(&mut text, index);
                    text[index].visible.push_str(&delta);
                }
                StreamChunk::ReasoningDelta { index, text: delta } => {
                    ensure_len(&mut text, index);
                    text[index].reasoning.push_str(&delta);
                }
                StreamChunk::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
                    ensure_len(&mut slots, index);
                    match &mut slots[index] {
                        Some(ContentBlock::ToolCall {
                            arguments,
                            name: existing,
                            ..
                        }) => {
                            arguments.push_str(&arguments_delta);
                            if let Some(name) = name {
                                existing.push_str(&name);
                            }
                        }
                        other => {
                            *other = Some(ContentBlock::ToolCall {
                                id,
                                name: name.unwrap_or_default(),
                                arguments: arguments_delta,
                            });
                        }
                    }
                }
                StreamChunk::BlockEnd { index, block } => {
                    ensure_len(&mut slots, index);
                    slots[index] = Some(block);
                }
                StreamChunk::Usage(accounting) => usage = Some(accounting),
                StreamChunk::Finish { reason } => finish = Some(reason),
            }
        }

        let finish = finish.ok_or(AssembleError::Unfinished)?;
        let content = slots
            .into_iter()
            .enumerate()
            .filter_map(|(index, block)| {
                block.or_else(|| {
                    let kind = kinds.get(index).map(String::as_str).unwrap_or_default();
                    if kind == "tool-call" {
                        // A tool call whose deltas never produced a name is not a block.
                        return None;
                    }
                    let accumulated = text.get(index);
                    let visible = accumulated.map(|t| t.visible.as_str()).unwrap_or_default();
                    let reasoning = accumulated
                        .map(|t| t.reasoning.as_str())
                        .unwrap_or_default();
                    if !visible.is_empty() {
                        Some(ContentBlock::Text {
                            text: visible.to_owned(),
                        })
                    } else if !reasoning.is_empty() {
                        Some(ContentBlock::Reasoning {
                            text: reasoning.to_owned(),
                        })
                    } else {
                        None
                    }
                })
            })
            .collect();

        Ok(Self {
            content,
            usage,
            finish: Some(finish),
        })
    }

    /// The concatenated visible text.
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The tool calls the model requested, in order.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<(ToolCallId, &str, &str)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.as_str(), arguments.as_str())),
                _ => None,
            })
            .collect()
    }
}

/// Text accumulated for one block position, used when no `block-end` arrived.
#[derive(Default)]
struct TextAccumulator {
    reasoning: String,
    visible: String,
}

fn ensure_len<T: Default>(slots: &mut Vec<T>, index: usize) {
    if slots.len() <= index {
        slots.resize_with(index + 1, T::default);
    }
}

/// A failure while assembling a stream.
#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    /// The stream ended without a terminal `finish` chunk.
    #[error("stream ended without a `finish` chunk")]
    Unfinished,
}

/// One LLM request.
#[derive(Clone, Debug)]
pub struct LlmRequest {
    /// Provider-facing model id.
    pub model: String,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolSchema>,
    /// System prompt, when the caller supplies one.
    pub system_prompt: Option<String>,
    /// Output token limit.
    pub max_tokens: Option<u32>,
}

impl LlmRequest {
    /// Builds a request with everything optional unset.
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            system_prompt: None,
            max_tokens: None,
        }
    }

    /// Sets the tools offered to the model.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = tools;
        self
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Sets the output limit.
    #[must_use]
    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = Some(max);
        self
    }
}

/// A failure a provider may report.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// The provider returned an error status.
    #[error("provider error: {0}")]
    Provider(String),

    /// The caller cancelled before the response completed.
    #[error("request cancelled")]
    Cancelled,
}

/// A source of model responses.
///
/// Implementors are the only place that knows a provider's wire format. Everything above — the
/// loop, the session log, the tool executor — sees [`StreamChunk`] and nothing else, which is what
/// keeps a provider swappable.
pub trait LlmProvider: Send + Sync {
    /// Streams one response.
    ///
    /// The returned stream must observe `cancel`: a cancelled call is dropped, not left running.
    /// Providers normalize their own failures into a terminal `finish` chunk where they can; a
    /// failure that prevents the stream from starting at all is returned here.
    fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>>;
}

/// The registry key for the active provider.
pub struct LlmKey;

impl crate::ServiceKey for LlmKey {
    type Service = dyn LlmProvider;
    const ID: &'static str = "llm";
}
