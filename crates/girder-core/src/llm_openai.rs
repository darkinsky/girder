//! An `OpenAI` Chat Completions provider.
//!
//! [`crate::llm_deepseek`] speaks the Anthropic Messages format, and almost nothing in it is true
//! here. Messages there carry typed content blocks, a tool call is a started block whose `input`
//! streams as `partial_json`, and the stream is typed by `event:` lines with block indices on the
//! wire. Chat Completions is flatter: `content` is a plain string, tool calls arrive as fragments
//! discriminated by an `index` the protocol never puts on a block, and every frame is an untyped
//! `data:` payload ended by the literal `[DONE]`. Deriving this module from that one by editing
//! would have carried those assumptions along, so it is written against the protocol instead.
//!
//! # The three translations this module owns
//!
//! - **Request.** The harness message vocabulary is richer than the wire: `tool` results ride on a
//!   `tool` message here (not on a user turn), reasoning has no wire slot, and a tool call's
//!   arguments are a JSON *string* rather than a parsed object.
//! - **Stream.** The wire has no block indices at all. This module assigns one per content channel
//!   as that channel first speaks, which is what lets the consumer receive a reasoning block and a
//!   text block as two blocks even though the wire never numbers them.
//! - **Errors.** Chat Completions reports failures as `{"error": {"message": ...}}`, both as a
//!   non-2xx body and as an in-stream payload. Both are reduced to one message so the caller sees
//!   the provider's own words rather than a status line.
//!
//! # Tool calls are accumulative, by wire index
//!
//! A call arrives as several deltas that share an `index` and each carry part of the argument JSON.
//! The id and the function name are *identity*, not accumulation: they arrive once, and a
//! continuation delta that repeats them empty means "unchanged". Treating those repeats as content
//! is the classic bug in this protocol — it produces `bashbash` and an id that no result can be
//! matched to.
//!
//! # What is verified, and what is not
//!
//! The tests drive this against a local mock server and against the parser directly. That verifies
//! the body shape, the SSE framing, and the chunk translation. It does **not** verify the module
//! against a live endpoint: no credential is available in this environment. The code is written so
//! that claim is one HTTP call away, not one refactor away.
//!
//! # Why the SSE framing is hand-written
//!
//! A full SSE crate brings reconnection, retry policy, and event-id bookkeeping that this protocol
//! does not use — the harness owns retry, and the stream is one-shot. The framing the protocol
//! actually uses is a buffer, a `data:` prefix, and a blank-line terminator.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use girder_core::{
    BoxFut, ChunkStream, ContentBlock, FinishReason, LlmError, LlmProvider, LlmRequest, Message,
    Role, StreamChunk, TokenUsage, ToolCallId,
};

/// Endpoint root of the public `OpenAI` API.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The payload that terminates a Chat Completions stream.
pub const DONE: &str = "[DONE]";

/// The route appended to a configured base URL.
const CHAT_COMPLETIONS: &str = "chat/completions";

/// Configuration for one `OpenAI`-compatible endpoint.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    /// Endpoint root; `/chat/completions` is appended unless it is already present.
    pub base_url: String,
    /// Bearer token sent in the `authorization` header.
    pub api_key: String,
    /// Model used when a request names none.
    pub default_model: String,
    /// Output limit used when a request names none.
    pub max_tokens: u32,
}

impl OpenAiConfig {
    /// Builds a configuration for `base_url`.
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_model: default_model.into(),
            max_tokens: 8192,
        }
    }

    /// The full completion URL.
    ///
    /// A base that already ends in `/chat/completions` is used verbatim. Dashboards hand out the
    /// complete URL as often as the root, and appending the route again would post to
    /// `/chat/completions/chat/completions` — a 404 that names nothing useful.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with(CHAT_COMPLETIONS) {
            base.to_owned()
        } else {
            format!("{base}/{CHAT_COMPLETIONS}")
        }
    }
}

/// A provider backed by an `OpenAI`-compatible Chat Completions endpoint.
pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// Builds a provider.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Provider`] when the HTTP client cannot be constructed.
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        // Both timeouts are transport bounds rather than call policy: a client with neither waits
        // forever, which turns an unresponsive provider into a turn that never ends.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|error| LlmError::Provider(format!("http client: {error}")))?;
        Ok(Self { config, client })
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> &OpenAiConfig {
        &self.config
    }

    /// Builds the JSON body for one request.
    ///
    /// Exposed so a test can assert the wire shape without a server, which is the part most likely
    /// to drift from the API.
    #[must_use]
    pub fn body(&self, request: &LlmRequest) -> Value {
        let mut messages: Vec<Value> = Vec::with_capacity(request.messages.len() + 1);
        // The system prompt is a message here, not a top-level field as the Messages API has it.
        // First position because the wire format is order-sensitive: a system turn after a user
        // turn is either rejected or silently treated as conversation.
        if let Some(system) = request
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.is_empty())
        {
            messages.push(json!({ "role": "system", "content": system }));
        }
        messages.extend(request.messages.iter().map(message_to_wire));

        let model = if request.model.is_empty() {
            self.config.default_model.as_str()
        } else {
            request.model.as_str()
        };
        let mut body = json!({
            "model": model,
            "stream": true,
            // Without this the endpoint sends no usage at all on a streamed response, and token
            // accounting silently reports zero for every turn.
            "stream_options": { "include_usage": true },
            "max_tokens": request.max_tokens.unwrap_or(self.config.max_tokens),
            "messages": messages,
        });

        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters,
                            },
                        })
                    })
                    .collect(),
            );
        }
        body
    }
}

/// Builds the wire form of one conversation message.
fn message_to_wire(message: &Message) -> Value {
    match message.role {
        Role::Tool => {
            // A tool result is its own `tool` turn on this protocol, correlated by `tool_call_id`.
            // The Messages API puts the same information in a `tool_result` block on a user turn;
            // reusing that shape here would produce a request no Chat Completions endpoint accepts.
            let content = flatten(&message.content);
            json!({
                "role": "tool",
                "tool_call_id": message.tool_call_id.as_ref().map_or("", |id| id.as_str()),
                // Some gateways reject a tool message whose content is the empty string. The model
                // still needs to be told the call produced nothing, and a placeholder says that
                // more clearly than an error would.
                "content": if content.is_empty() { "(no output)" } else { content.as_str() },
            })
        }
        Role::Assistant => {
            let mut wire = json!({
                "role": "assistant",
                // The empty string rather than null, even for a tool-call-only turn: upstream
                // records that some gateways reject a null here outright, and the turn is durable,
                // so one rejected message would break every later request that replays it.
                "content": flatten(&message.content),
            });
            let tool_calls: Vec<Value> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some(json!({
                        "id": id.as_str(),
                        "type": "function",
                        "function": {
                            "name": name,
                            // The raw string, not a parsed object: unlike the Messages API, this
                            // protocol wants the JSON text the model produced. Parsing and
                            // re-serializing would also hide a malformed accumulator from the
                            // model, which is the one thing that can repair it.
                            "arguments": arguments,
                        },
                    })),
                    _ => None,
                })
                .collect();
            if !tool_calls.is_empty() {
                wire["tool_calls"] = Value::Array(tool_calls);
            }
            wire
        }
        Role::User => json!({ "role": "user", "content": flatten(&message.content) }),
    }
}

/// Flattens a message's blocks into the single string this protocol carries.
///
/// Reasoning and tool calls are dropped rather than rendered: neither belongs in the `content`
/// string of a user or assistant turn, and the tool calls of an assistant turn are sent separately
/// as `tool_calls`. Images and files have no attachment channel here, so they survive as the same
/// deterministic handle text the tool layer already shows the model.
fn flatten(blocks: &[ContentBlock]) -> String {
    blocks.iter().map(block_text).collect()
}

/// One block's contribution to a flattened message.
fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        // Reasoning is not part of the Chat Completions input vocabulary. Sending it as invisible
        // text would make the model read its own thinking as content; sending it in a vendor field
        // would be rejected by a strict endpoint.
        ContentBlock::Reasoning { .. } | ContentBlock::ToolCall { .. } => String::new(),
        ContentBlock::Image { attachment } => format!("[image {attachment}]"),
        ContentBlock::File { attachment } => format!("[file {attachment}]"),
    }
}

impl LlmProvider for OpenAiProvider {
    fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        let body = self.body(&request);
        let url = self.config.endpoint();
        let key = self.config.api_key.clone();
        let client = self.client.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }

            let response = client
                .post(&url)
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
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
                // The body is the only place the provider explains *why*: a 400 for a bad schema
                // and a 401 for a bad key look identical from the status line alone, and they need
                // different responses from the operator.
                let text = response.text().await.unwrap_or_default();
                return Err(LlmError::Provider(format!(
                    "HTTP {status}: {}",
                    body_error_message(&text)
                )));
            }

            // Parsed incrementally so a chunk reaches the caller as it arrives rather than after
            // the whole response.
            let stream = response
                .bytes_stream()
                .map(move |chunk| chunk.map_err(|error| error.to_string()));

            let chunks = async_stream::stream! {
                let mut parser = SseParser::default();
                let mut translator = Translator::default();
                let mut stream = Box::pin(stream);

                while let Some(chunk) = stream.next().await {
                    if cancel.is_cancelled() {
                        yield Err(LlmError::Cancelled);
                        return;
                    }
                    match chunk {
                        Ok(bytes) => {
                            for payload in parser.feed(&bytes) {
                                for event in translator.translate(&payload) {
                                    yield Ok(event);
                                }
                                if translator.finished {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            yield Err(LlmError::Provider(format!("stream error: {error}")));
                            return;
                        }
                    }
                }

                // A server that closes without the final blank line still sent a complete last
                // payload; flushing it is what keeps `[DONE]` from being lost to framing.
                if let Some(payload) = parser.flush() {
                    for event in translator.translate(&payload) {
                        yield Ok(event);
                    }
                }
                // A stream that ends before `[DONE]` is truncated. Accepting it would let a network
                // cut look like a complete answer, which is the failure this check exists for.
                if !translator.finished {
                    // A partial event left in the parser says the connection was cut mid-frame; none says the endpoint
                    // stopped after a complete one. The two are different stalls and a caller reading the error should
                    // be able to tell them apart.
                    let detail = if parser.has_pending() {
                        "stream ended before [DONE], with a partial event buffered"
                    } else {
                        "stream ended before [DONE]"
                    };
                    yield Err(LlmError::Provider(detail.to_owned()));
                }
            };

            // The provider contract has no error channel inside the stream, so a failure becomes a
            // terminal chunk the consumer can always recognize. A cancellation is reported as
            // `Aborted` rather than as a provider fault: a user cancel and a broken endpoint demand
            // different handling above this seam.
            let mapped = chunks.map(|item| match item {
                Ok(chunk) => chunk,
                Err(LlmError::Cancelled) => StreamChunk::Finish {
                    reason: FinishReason::Aborted,
                },
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

/// Incremental parser for the `data:`-only SSE framing this protocol uses.
///
/// The buffer holds bytes rather than text: a multi-byte character can be split across network
/// chunks, and decoding each chunk independently would replace half of it with U+FFFD.
#[derive(Default)]
pub struct SseParser {
    /// Bytes received but not yet terminated by a newline.
    buffer: Vec<u8>,
    /// `data:` fields accumulated for the event being framed.
    data: String,
}

impl SseParser {
    /// Feeds bytes, returning every payload they completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();

        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\r', '\n']);

            if line.is_empty() {
                self.dispatch(&mut out);
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                // Per the SSE specification, multiple `data:` fields in one event are joined by a
                // newline. A payload that is itself multi-line JSON (some proxies re-indent it)
                // therefore still parses.
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                // Exactly one leading space is framing, not content; anything after it is the
                // payload's own whitespace and is preserved.
                self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // `event:`, `id:`, `retry:`, and `:` comments carry nothing this protocol needs. The
            // `event:` name is deliberately *not* used for dispatch: the payload's own `type` field
            // is what the protocol means, and a proxy is free to add names.
        }

        out
    }

    /// Flushes a final payload the server closed without a trailing blank line.
    #[must_use]
    pub fn flush(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let buffered = std::mem::take(&mut self.buffer);
            let line = String::from_utf8_lossy(&buffered);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(rest) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if self.data.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.data))
        }
    }

    /// Whether a partial frame is buffered.
    ///
    /// A diagnostic rather than a step: the parser does not need the question, and a caller watching an idle connection
    /// does -- "nothing arrived" and "half a frame arrived" are different stalls.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.buffer.is_empty() || !self.data.is_empty()
    }

    /// Emits the accumulated payload, if any, and resets the event.
    fn dispatch(&mut self, out: &mut Vec<String>) {
        if !self.data.is_empty() {
            out.push(std::mem::take(&mut self.data));
        }
    }
}

/// One block this translator has opened, in the vocabulary the consumer sees.
enum OpenBlock {
    /// Visible text, accumulated because the closing chunk carries the whole block.
    Text {
        /// Assigned block index.
        index: usize,
        /// Text seen so far.
        text: String,
    },
    /// Reasoning text.
    Reasoning {
        /// Assigned block index.
        index: usize,
        /// Text seen so far.
        text: String,
    },
    /// A tool call whose arguments are still arriving.
    ToolCall {
        /// Assigned block index.
        index: usize,
        /// Call id, or a synthesized one for a stream that never sent a usable id.
        id: ToolCallId,
        /// Function name, accumulated from the delta that carries it.
        name: String,
        /// Raw argument JSON, concatenated verbatim.
        arguments: String,
        /// Whether the name has already been reported in a delta.
        name_sent: bool,
    },
}

impl OpenBlock {
    /// The block index this block was assigned.
    fn index(&self) -> usize {
        match self {
            Self::Text { index, .. }
            | Self::Reasoning { index, .. }
            | Self::ToolCall { index, .. } => *index,
        }
    }

    /// The completed block, for the closing chunk.
    fn into_content(self) -> ContentBlock {
        match self {
            Self::Text { text, .. } => ContentBlock::Text { text },
            Self::Reasoning { text, .. } => ContentBlock::Reasoning { text },
            Self::ToolCall {
                id,
                name,
                arguments,
                ..
            } => ContentBlock::ToolCall {
                id,
                name,
                arguments,
            },
        }
    }
}

/// Accumulated state for one wire tool-call index.
#[derive(Default)]
struct ToolState {
    /// The block index this call opened, once it had something to emit.
    block: Option<usize>,
    /// Identity seen so far, including from a delta that arrived before the block opened.
    id: Option<ToolCallId>,
}

/// Translates Chat Completions payloads into [`StreamChunk`]s.
///
/// Stateful because the protocol is: neither the block indices nor the assembled content exist on
/// the wire, and the finish reason and usage are deferred to `[DONE]` so that nothing follows the
/// terminal `finish` chunk.
#[derive(Default)]
pub struct Translator {
    /// Whether a terminal payload has been seen or the stream failed.
    pub finished: bool,
    /// Blocks opened so far, by assigned index.
    blocks: BTreeMap<usize, OpenBlock>,
    /// Index of the visible-text block, once one opened.
    text: Option<usize>,
    /// Index of the reasoning block, once one opened.
    reasoning: Option<usize>,
    /// Tool call state, keyed by the wire's `index`.
    tools: BTreeMap<u64, ToolState>,
    /// Index the next opened block receives.
    next_index: usize,
    /// Finish reason seen on a choice, deferred until `[DONE]`.
    pending_finish: Option<FinishReason>,
    /// Latest usage seen, deferred until `[DONE]`.
    pending_usage: Option<TokenUsage>,
}

impl Translator {
    /// Translates one `data:` payload.
    pub fn translate(&mut self, payload: &str) -> Vec<StreamChunk> {
        if self.finished {
            return Vec::new();
        }
        let data = payload.trim();
        if data.is_empty() {
            return Vec::new();
        }
        if data == DONE {
            return self.finish();
        }

        let Ok(json) = serde_json::from_str::<Value>(data) else {
            // A payload this module cannot read means the stream is no longer interpretable.
            // Continuing would guess at content the model produced, so the stream stops here with
            // the offending prefix in the message: the 120-character cap is enough to identify the
            // frame without pasting a whole response into a log line.
            self.finished = true;
            return vec![StreamChunk::Finish {
                reason: FinishReason::Error {
                    message: format!("malformed SSE payload: {}", truncate(data)),
                },
            }];
        };

        // An error payload can arrive *inside* a 200 response — some gateways open the stream and
        // then report the failure in-band — so it is checked before any content is read.
        if let Some(error) = json.get("error").filter(|value| !value.is_null()) {
            self.finished = true;
            return vec![StreamChunk::Finish {
                reason: FinishReason::Error {
                    message: error_message(error),
                },
            }];
        }

        let mut out = Vec::new();
        // Usage may be attached to the finish chunk or sent as a trailing usage-only chunk; keeping
        // the latest covers both shapes.
        if let Some(usage) = json.get("usage").filter(|value| !value.is_null()) {
            self.pending_usage = Some(usage_from(usage));
        }
        // Requests ask for a single choice, so the choice index is not carried into the block
        // index: two choices would otherwise interleave into one another's slots.
        if let Some(choices) = json.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    self.apply_delta(delta, &mut out);
                }
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    self.pending_finish = Some(finish_reason(reason));
                }
            }
        }
        out
    }

    /// Applies one `delta` object.
    fn apply_delta(&mut self, delta: &Value, out: &mut Vec<StreamChunk>) {
        // Reasoning first: a thinking model emits it before the answer. An *empty* string is what
        // the opening frame carries, and it must not open a block — an empty reasoning block would
        // be assembled into an empty message.
        if let Some(text) = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            let index = self.open_reasoning(out);
            if let Some(OpenBlock::Reasoning {
                text: accumulated, ..
            }) = self.blocks.get_mut(&index)
            {
                accumulated.push_str(text);
            }
            out.push(StreamChunk::ReasoningDelta {
                index,
                text: text.to_owned(),
            });
        }

        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            let index = self.open_text(out);
            if let Some(OpenBlock::Text {
                text: accumulated, ..
            }) = self.blocks.get_mut(&index)
            {
                accumulated.push_str(text);
            }
            out.push(StreamChunk::TextDelta {
                index,
                text: text.to_owned(),
            });
        }

        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                self.apply_tool_call(call, position, out);
            }
        }
    }

    /// Opens the visible-text block if none is open, and returns its index.
    fn open_text(&mut self, out: &mut Vec<StreamChunk>) -> usize {
        if let Some(index) = self.text {
            return index;
        }
        let index = self.take_index();
        self.text = Some(index);
        self.blocks.insert(
            index,
            OpenBlock::Text {
                index,
                text: String::new(),
            },
        );
        out.push(StreamChunk::BlockStart {
            index,
            block_type: "text".to_owned(),
        });
        index
    }

    /// Opens the reasoning block if none is open, and returns its index.
    fn open_reasoning(&mut self, out: &mut Vec<StreamChunk>) -> usize {
        if let Some(index) = self.reasoning {
            return index;
        }
        let index = self.take_index();
        self.reasoning = Some(index);
        self.blocks.insert(
            index,
            OpenBlock::Reasoning {
                index,
                text: String::new(),
            },
        );
        out.push(StreamChunk::BlockStart {
            index,
            block_type: "reasoning".to_owned(),
        });
        index
    }

    /// Applies one fragment of one tool call.
    fn apply_tool_call(&mut self, call: &Value, position: usize, out: &mut Vec<StreamChunk>) {
        // `index` is the wire's discriminator between parallel calls. The array position is a
        // fallback for a gateway that omits it: using it keeps such a stream readable instead of
        // collapsing every call onto slot zero.
        let wire_index = call
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| u64::try_from(position).unwrap_or(u64::MAX));
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty());
        let fragment = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let (known_id, opened) = {
            let state = self.tools.entry(wire_index).or_default();
            // The id is identity, not accumulation: a continuation delta repeats it as `""` or
            // `null` to mean "unchanged", and taking that literally would leave a call no result
            // can answer.
            if let Some(raw) = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                && let Ok(id) = ToolCallId::new(raw)
            {
                state.id = Some(id);
            }
            (state.id.clone(), state.block)
        };
        let index = if let Some(index) = opened {
            index
        } else {
            let Some(index) = self.open_tool_call(wire_index, known_id, name, fragment, out) else {
                // A delta that carries only identity has nothing to emit yet; the call opens on the
                // delta that says something.
                return;
            };
            index
        };

        let Some(OpenBlock::ToolCall {
            id,
            name: accumulated_name,
            arguments,
            name_sent,
            ..
        }) = self.blocks.get_mut(&index)
        else {
            return;
        };

        let delta_name = name.filter(|name| {
            if *name_sent {
                // Both assemblers *append* a name they are given, so re-emitting a repeat would
                // produce `bashbash`. The name is identity: the first one wins.
                return false;
            }
            accumulated_name.push_str(name);
            *name_sent = true;
            true
        });
        arguments.push_str(fragment);

        // A fragment with neither a name nor arguments says nothing a consumer can use.
        if delta_name.is_none() && fragment.is_empty() {
            return;
        }
        out.push(StreamChunk::ToolCallDelta {
            index,
            id: id.clone(),
            name: delta_name.map(str::to_owned),
            arguments_delta: fragment.to_owned(),
        });
    }

    /// Opens a tool-call block for `wire_index`, returning its new block index.
    ///
    /// Returns `None` when the delta carries neither a name nor an argument: a block opened there
    /// would be an unnamed, argument-less call, which no consumer can dispatch. The id, when one is
    /// already known, is carried into the block so the call stays correlatable with its result.
    fn open_tool_call(
        &mut self,
        wire_index: u64,
        known_id: Option<ToolCallId>,
        name: Option<&str>,
        fragment: &str,
        out: &mut Vec<StreamChunk>,
    ) -> Option<usize> {
        if name.is_none() && fragment.is_empty() {
            return None;
        }
        // A stream that never sends a usable id still gets a call the model asked for. The
        // synthesized id is derived from the wire index rather than random, so a replayed stream
        // assembles to the same message.
        let id = known_id.or_else(|| ToolCallId::new(format!("call-{wire_index}")).ok())?;
        let index = self.take_index();
        self.blocks.insert(
            index,
            OpenBlock::ToolCall {
                index,
                id,
                name: String::new(),
                arguments: String::new(),
                name_sent: false,
            },
        );
        if let Some(state) = self.tools.get_mut(&wire_index) {
            state.block = Some(index);
        }
        out.push(StreamChunk::BlockStart {
            index,
            block_type: "tool-call".to_owned(),
        });
        Some(index)
    }

    /// Allocates the next block index.
    fn take_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    /// Closes every open block and emits the deferred usage and finish reason.
    fn finish(&mut self) -> Vec<StreamChunk> {
        self.finished = true;
        let mut out = Vec::new();
        let blocks = std::mem::take(&mut self.blocks);
        let opened = blocks.len();
        for block in blocks.into_values() {
            let index = block.index();
            out.push(StreamChunk::BlockEnd {
                index,
                block: block.into_content(),
            });
        }
        if let Some(usage) = self.pending_usage.take() {
            out.push(StreamChunk::Usage(usage));
        }
        let reason = self.pending_finish.take().unwrap_or(FinishReason::Stop);
        // A `stop` with nothing opened is not an empty answer, it is a provider that completed
        // without producing one. Reporting success would hand the loop a blank assistant message
        // that looks like a model decision; reporting the failure lets the retry policy act on it.
        let reason = if opened == 0 && matches!(reason, FinishReason::Stop) {
            FinishReason::Error {
                message: "model returned a completed response with no content".to_owned(),
            }
        } else {
            reason
        };
        out.push(StreamChunk::Finish { reason });
        out
    }
}

/// Extracts the message from a provider error object.
fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.get("code").and_then(Value::as_str))
        .or_else(|| error.get("type").and_then(Value::as_str))
        .map_or_else(|| error.to_string(), str::to_owned)
}

/// Extracts a diagnostic from a non-2xx response body.
///
/// The body is the only carrier of the reason, but it is also attacker- and proxy-controlled: a
/// gateway answering with a full HTML page would otherwise put that page in every log line. So the
/// extraction is best-effort and the fallback is capped.
fn body_error_message(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(json) = serde_json::from_str::<Value>(trimmed) {
        if let Some(error) = json.get("error").filter(|value| !value.is_null()) {
            return error_message(error);
        }
        // Some gateways put the message at the top level instead of nesting it.
        if let Some(message) = json.get("message").and_then(Value::as_str) {
            return message.to_owned();
        }
    }
    if trimmed.is_empty() {
        "empty response body".to_owned()
    } else {
        truncate(trimmed)
    }
}

/// Caps a diagnostic so one bad response cannot flood a log.
fn truncate(text: &str) -> String {
    const LIMIT: usize = 200;
    if text.chars().count() <= LIMIT {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(LIMIT).collect();
    out.push('…');
    out
}

/// Maps the wire usage counters onto the harness's disjoint ones.
///
/// `prompt_tokens` *includes* the cached prefix on this protocol, while [`TokenUsage`] records the
/// cache separately and counts it as not billed. Leaving the cache in `input` would double-count
/// every cached turn.
fn usage_from(usage: &Value) -> TokenUsage {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        // `prompt_cache_hit_tokens` is the vendor spelling of the same count; some endpoints send
        // only that one.
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    TokenUsage {
        input: prompt.saturating_sub(cached),
        output: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached,
    }
}

/// Maps the wire `finish_reason` onto a [`FinishReason`].
///
/// An unrecognized value becomes an error rather than a silent `Stop`: `content_filter` and a
/// future reason both mean the answer is not what the model intended to say, and reporting success
/// would hide that.
fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        other => FinishReason::Error {
            message: format!("model stopped: {other}"),
        },
    }
}

/// Builds whichever provider the environment describes.
///
/// Two variables decide: `GIRDER_PROTOCOL` names the wire format (`anthropic` by default, or `openai`), and the key
/// variables differ per protocol. One entry point rather than two because a caller that has to know the protocol before
/// calling is a caller that has to duplicate this decision.
///
/// # Errors
///
/// Returns [`LlmError::Provider`] when the chosen protocol's key is unset or the client cannot be built.
pub fn provider_from_env() -> Result<Arc<dyn girder_core::LlmProvider>, LlmError> {
    let protocol = std::env::var("GIRDER_PROTOCOL").unwrap_or_else(|_| "anthropic".to_owned());
    match protocol.as_str() {
        "openai" => {
            // `OPENAI_API_KEY` first because that is what the ecosystem sets, then the harness's own prefixed name for a
            // deployment that talks to a compatible endpoint under a different product.
            let api_key = std::env::var("OPENAI_API_KEY")
                .or_else(|_| std::env::var("GIRDER_OPENAI_API_KEY"))
                .map_err(|_| {
                    LlmError::Provider(
                        "GIRDER_PROTOCOL is `openai` but OPENAI_API_KEY is not set".to_owned(),
                    )
                })?;
            let base_url =
                std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
            let default_model = std::env::var("GIRDER_MODEL")
                .or_else(|_| std::env::var("OPENAI_MODEL"))
                .unwrap_or_else(|_| "gpt-4o-mini".to_owned());
            let config = OpenAiConfig {
                base_url,
                api_key,
                default_model,
                max_tokens: 8192,
            };
            Ok(Arc::new(OpenAiProvider::new(config)?))
        }
        // Anything else is the Anthropic format, including an unknown name: this build's default protocol, and refusing
        // to start on a typo would be a worse failure than using the format every other variable already describes.
        _ => girder_core::provider_from_env(),
    }
}
