//! A `DeepSeek` provider speaking the Anthropic Messages wire format.
//!
//! Upstream's `packages/llm/llm-deepseek` posts to `{base}/messages` with `anthropic-version` and
//! consumes a server-sent event stream. This module implements that protocol and nothing else about
//! the provider — no image offload, no file ids, no model discovery, no pricing. Those are admin
//! surfaces (see `docs/phase4-interfaces.md`), and the vertical slice never reaches them.
//!
//! # What is verified, and what is not
//!
//! The tests drive this against a **local mock server** that replays recorded-shaped SSE. That
//! verifies the request body the provider builds, the SSE parser, and the chunk translation. It does
//! **not** verify the provider against the live API: no credential is available in this environment,
//! so "works against api.deepseek.com" is an unverified claim. The module is written so that claim
//! is one HTTP call away, not one refactor away.
//!
//! # Why SSE parsing is hand-written
//!
//! The event stream is line-oriented: `event: <type>` then `data: <json>`, blank-line separated.
//! A full SSE crate would bring reconnection, retry policy, and event-id bookkeeping that this
//! protocol does not use — the harness owns retry (`llm-retry` upstream) and the stream is one-shot.
//! Parsing three fields by hand is smaller than configuring the alternative.

use std::sync::Arc;

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::llm::{
    ChunkStream, ContentBlock, FinishReason, LlmError, LlmProvider, LlmRequest, Role, StreamChunk,
    TokenUsage,
};
use crate::scope::BoxFut;

/// Default endpoint. Upstream's `PUBLIC_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/anthropic";

/// Default model id.
pub const DEFAULT_MODEL: &str = "deepseek-chat";

/// Configuration for one provider instance.
#[derive(Clone, Debug)]
pub struct DeepSeekConfig {
    /// Endpoint root; `/messages` is appended.
    pub base_url: String,
    /// API key, sent as `x-api-key`.
    pub api_key: String,
    /// `anthropic-version` header value.
    pub anthropic_version: String,
    /// Default output limit when a request does not set one.
    pub max_tokens: u32,
    /// Default model when a request does not name one.
    pub default_model: String,
}

impl DeepSeekConfig {
    /// Builds a config for `base_url` with `api_key`.
    #[must_use]
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            anthropic_version: "2023-06-01".to_owned(),
            max_tokens: 8192,
            default_model: DEFAULT_MODEL.to_owned(),
        }
    }

    /// Reads configuration from the environment.
    ///
    /// Returns `None` when no key is present, so a caller can fall back to another provider rather
    /// than fail at the first request.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("DEEPSEEK_API_KEY").ok()?;
        let base_url =
            std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        let default_model =
            std::env::var("GIRDER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
        Some(Self {
            base_url,
            api_key,
            anthropic_version: "2023-06-01".to_owned(),
            max_tokens: 8192,
            default_model,
        })
    }

    /// The full endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/messages")
    }
}

/// A provider backed by the Anthropic Messages API.
pub struct DeepSeekProvider {
    config: DeepSeekConfig,
    client: reqwest::Client,
}

impl DeepSeekProvider {
    /// Builds a provider.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Provider`] when the HTTP client cannot be constructed.
    pub fn new(config: DeepSeekConfig) -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| LlmError::Provider(format!("http client: {error}")))?;
        Ok(Self { config, client })
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> &DeepSeekConfig {
        &self.config
    }

    /// Builds the JSON body for one request.
    ///
    /// Exposed so a test can assert the wire shape without a server, which is the part most likely
    /// to drift from the API.
    #[must_use]
    pub fn body(&self, request: &LlmRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|message| {
                // The Messages API has no `tool` role: a tool result is a `user` turn holding a
                // `tool_result` block. That block is not optional — every `tool_use` must be
                // answered by one in the immediately following message, and the API rejects the
                // request otherwise.
                let role = match message.role {
                    Role::Tool | Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let content = if message.role == Role::Tool {
                    let call_id = message.tool_call_id.as_ref().map_or("", |id| id.as_str());
                    let text: String = message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": [{ "type": "text", "text": text }],
                    })]
                } else {
                    message
                        .content
                        .iter()
                        .map(block_to_wire)
                        .collect::<Vec<_>>()
                };
                serde_json::json!({ "role": role, "content": content })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": if request.model.is_empty() { &self.config.default_model } else { &request.model },
            "stream": true,
            "max_tokens": request.max_tokens.unwrap_or(self.config.max_tokens),
            "messages": messages,
        });

        if let Some(system) = &request.system_prompt {
            body["system"] = serde_json::json!(system);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(
                request
                    .tools
                    .iter()
                    .map(|tool| serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        body
    }
}

/// One content block in the wire vocabulary.
fn block_to_wire(block: &ContentBlock) -> serde_json::Value {
    match block {
        ContentBlock::Text { text } => serde_json::json!({ "type": "text", "text": text }),
        // Reasoning is not sent back: the API does not accept it as input.
        ContentBlock::Reasoning { .. } => serde_json::json!({ "type": "text", "text": "" }),
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            // The API wants a parsed object, not a string. A malformed accumulator becomes an empty
            // object rather than failing the whole request: the tool executor will report the
            // invalid arguments to the model, which is more useful than dropping the turn.
            "input": serde_json::from_str::<serde_json::Value>(arguments)
                .unwrap_or_else(|_| serde_json::json!({})),
        }),
        ContentBlock::Image { attachment } => serde_json::json!({
            "type": "text",
            "text": format!("[image {attachment}]"),
        }),
        ContentBlock::File { attachment } => serde_json::json!({
            "type": "text",
            "text": format!("[file {attachment}]"),
        }),
    }
}

impl LlmProvider for DeepSeekProvider {
    fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        let body = self.body(&request);
        let url = self.config.endpoint();
        let key = self.config.api_key.clone();
        let version = self.config.anthropic_version.clone();
        let client = self.client.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }

            let response = client
                .post(&url)
                .header("x-api-key", key)
                .header("anthropic-version", version)
                .header("accept", "text/event-stream")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|error| {
                    if cancel.is_cancelled() {
                        LlmError::Cancelled
                    } else {
                        LlmError::Provider(format!("request failed: {error}"))
                    }
                })?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(LlmError::Provider(format!(
                    "HTTP {status}: {}",
                    text.trim()
                )));
            }

            // Parse the SSE stream incrementally so a chunk reaches the caller as it arrives rather
            // than after the whole response.
            let stream = response
                .bytes_stream()
                .map(move |chunk| chunk.map_err(|error| error.to_string()));

            let chunks = async_stream::stream! {
                let mut parser = SseParser::default();
                let mut stream = Box::pin(stream);
                let mut translator = Translator::default();

                while let Some(chunk) = stream.next().await {
                    if cancel.is_cancelled() {
                        yield Err(LlmError::Cancelled);
                        return;
                    }
                    match chunk {
                        Ok(bytes) => {
                            for event in parser.feed(&bytes) {
                                for outcome in translator.translate(&event) {
                                    yield outcome;
                                }
                            }
                        }
                        Err(error) => {
                            yield Err(LlmError::Provider(format!("stream error: {error}")));
                            return;
                        }
                    }
                }
                // A stream that ends without `message_stop` is truncated.
                if !translator.finished {
                    yield Err(LlmError::Provider(
                        "stream ended before message_stop".to_owned(),
                    ));
                }
            };

            // Map `Result<StreamChunk, LlmError>` into the provider contract: a stream item is a
            // chunk, and a failure inside the stream is a terminal `Error` finish so the consumer
            // always sees a well-formed ending.
            let mapped = chunks.map(|item| match item {
                Ok(chunk) => chunk,
                Err(error) => StreamChunk::Finish {
                    reason: FinishReason::Error {
                        message: error.to_string(),
                    },
                },
            });

            Ok(Box::pin(mapped) as ChunkStream)
        })
    }
}

/// One parsed server-sent event.
#[derive(Clone, Debug, PartialEq)]
pub struct SseEvent {
    /// The `event:` field.
    pub event: String,
    /// The `data:` payload.
    pub data: String,
}

/// Incremental SSE parser.
///
/// Buffers bytes until a blank line, then emits the accumulated fields. Bytes rather than `String`
/// because a multi-byte character can be split across network chunks, and decoding each chunk
/// independently would corrupt it.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: String,
    data: String,
}

impl SseParser {
    /// Feeds bytes, returning every complete event.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();

        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\r', '\n']);

            if line.is_empty() {
                if !self.event.is_empty() || !self.data.is_empty() {
                    out.push(SseEvent {
                        event: std::mem::take(&mut self.event),
                        data: std::mem::take(&mut self.data),
                    });
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                rest.trim().clone_into(&mut self.event);
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest.trim_start());
            }
            // `id:` and `retry:` are unused by this protocol and ignored.
        }

        out
    }

    /// Whether a partial event is buffered.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.buffer.is_empty() || !self.data.is_empty()
    }
}

/// Translates SSE events into [`StreamChunk`]s.
#[derive(Default)]
pub struct Translator {
    /// Whether a terminal event has been seen.
    pub finished: bool,
    /// Blocks started so far, by index, so `content_block_stop` can be paired.
    started: std::collections::BTreeMap<usize, String>,
}

impl Translator {
    /// Translates one event, returning the chunks it produces.
    pub fn translate(&mut self, event: &SseEvent) -> Vec<Result<StreamChunk, LlmError>> {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            // `ping` and unknown event types often carry no JSON, or carry a shape this version does
            // not know. Ignoring them keeps a newer server compatible.
            return Vec::new();
        };

        match event.event.as_str() {
            "message_start" => Self::message_start(&json),
            "content_block_start" => self.block_start(&json),
            "content_block_delta" => Self::block_delta(&json),
            "content_block_stop" => self.block_stop(&json),
            "message_delta" => self.message_delta(&json),
            "message_stop" => {
                self.finished = true;
                Vec::new()
            }
            "error" => Self::error(&json),
            _ => Vec::new(),
        }
    }

    fn message_start(json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        json.pointer("/message/usage")
            .map(|usage| vec![Ok(StreamChunk::Usage(usage_from(usage)))])
            .unwrap_or_default()
    }

    fn block_start(&mut self, json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let Ok(index) = index_of(json) else {
            return Vec::new();
        };
        let kind = json["content_block"]["type"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.started.insert(index, kind.clone());

        match kind.as_str() {
            "text" => vec![Ok(StreamChunk::BlockStart {
                index,
                block_type: "text".to_owned(),
            })],
            "thinking" => vec![Ok(StreamChunk::BlockStart {
                index,
                block_type: "reasoning".to_owned(),
            })],
            "tool_use" => vec![
                Ok(StreamChunk::BlockStart {
                    index,
                    block_type: "tool-call".to_owned(),
                }),
                // The id and name arrive complete on the start event; only arguments stream.
                Ok(StreamChunk::ToolCallDelta {
                    index,
                    // A provider id that fails validation becomes a placeholder rather than failing
                    // the stream: the call is still usable, and dropping the response over a
                    // malformed id would be worse than an untraceable call.
                    id: crate::brand::ToolCallId::new(
                        json["content_block"]["id"].as_str().unwrap_or_default(),
                    )
                    .unwrap_or_else(|_| {
                        crate::brand::ToolCallId::new("invalid").expect("constant is valid")
                    }),
                    name: Some(
                        json["content_block"]["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    arguments_delta: String::new(),
                }),
            ],
            _ => Vec::new(),
        }
    }

    fn block_delta(json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let Ok(index) = index_of(json) else {
            return Vec::new();
        };
        let delta = &json["delta"];
        match delta["type"].as_str().unwrap_or_default() {
            "text_delta" => vec![Ok(StreamChunk::TextDelta {
                index,
                text: delta["text"].as_str().unwrap_or_default().to_owned(),
            })],
            "thinking_delta" => vec![Ok(StreamChunk::ReasoningDelta {
                index,
                text: delta["thinking"].as_str().unwrap_or_default().to_owned(),
            })],
            "input_json_delta" => vec![Ok(StreamChunk::ToolCallDelta {
                index,
                // Only the start event carries the id; a delta leaves it empty.
                id: crate::brand::ToolCallId::new("").unwrap_or_else(|_| {
                    crate::brand::ToolCallId::new("invalid").expect("constant is valid")
                }),
                name: None,
                arguments_delta: delta["partial_json"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            })],
            _ => Vec::new(),
        }
    }

    fn block_stop(&mut self, json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let Ok(index) = index_of(json) else {
            return Vec::new();
        };
        let Some(kind) = self.started.remove(&index) else {
            return Vec::new();
        };
        // The block is materialized by the assembler from its deltas; emitting the accumulated text
        // here would duplicate it.
        let block = match kind.as_str() {
            "text" => ContentBlock::Text {
                text: String::new(),
            },
            "thinking" => ContentBlock::Reasoning {
                text: String::new(),
            },
            _ => return Vec::new(),
        };
        vec![Ok(StreamChunk::BlockEnd { index, block })]
    }

    fn message_delta(&mut self, json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let mut out = Vec::new();
        if let Some(usage) = json.get("usage") {
            out.push(Ok(StreamChunk::Usage(usage_from(usage))));
        }
        if let Some(reason) = json.pointer("/delta/stop_reason").and_then(|r| r.as_str()) {
            out.push(Ok(StreamChunk::Finish {
                reason: finish_reason(reason),
            }));
            self.finished = true;
        }
        out
    }

    fn error(json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let message = json
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("provider error");
        vec![Ok(StreamChunk::Finish {
            reason: FinishReason::Error {
                message: message.to_owned(),
            },
        })]
    }
}

/// Reads the block index, refusing a value that cannot be a position.
fn index_of(json: &serde_json::Value) -> Result<usize, ()> {
    json["index"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(())
}

fn usage_from(value: &serde_json::Value) -> TokenUsage {
    TokenUsage {
        input: value
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        output: value
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cached: value
            .get("cache_read_input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    }
}

fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::Error {
            message: "the model refused".to_owned(),
        },
        other => FinishReason::Error {
            message: format!("unrecognized stop reason `{other}`"),
        },
    }
}

/// Builds a provider from the environment, or reports why it cannot.
///
/// # Errors
///
/// Returns [`LlmError::Provider`] when `DEEPSEEK_API_KEY` is unset or the client cannot be built.
pub fn provider_from_env() -> Result<Arc<dyn LlmProvider>, LlmError> {
    let config = DeepSeekConfig::from_env().ok_or_else(|| {
        LlmError::Provider(
            "DEEPSEEK_API_KEY is not set; set it to use the live provider".to_owned(),
        )
    })?;
    Ok(Arc::new(DeepSeekProvider::new(config)?))
}
