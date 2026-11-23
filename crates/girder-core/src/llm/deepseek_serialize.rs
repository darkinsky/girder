//! `DeepSeek Messages` request serialization: the pure half of upstream's `serialize.ts`.
//!
//! # What upstream has, measured
//!
//! `packages/llm/llm-deepseek/src/serialize.ts` is 169 lines with exactly one export,
//! `serialize(options, connection, history, images, access, onReplayDegrade?, fileIds?)`. It is not
//! a formatter: it is the function that decides what a durable conversation *means* on the wire,
//! and every rule in it exists because the Messages protocol and the harness's log model disagree
//! about something. The rules, in the order the source applies them:
//!
//! | # | Rule | Upstream symbol |
//! | --- | --- | --- |
//! | 1 | Assistant `text` becomes `text`; an **empty** one is dropped, and dropping it must not disturb the replay indices of its neighbours | `assistant` |
//! | 2 | Assistant `reasoning` becomes `thinking`, carrying the native `signature` from the replay envelope when one validates at that index | `assistant` |
//! | 3 | Assistant `tool-call` becomes `tool_use`; its raw argument string is parsed to an object, and unparseable, non-object, or array arguments become `{}` rather than failing the request | `toolInput` |
//! | 4 | Any other assistant tag is `UNSUPPORTED_CONTENT` | `assistant` |
//! | 5 | User and tool-result content keeps `text` (empty dropped) and `image`; `reasoning` and `tool-call` are silently **omitted**, not refused | `input` |
//! | 6 | Any other user/tool-result tag is `UNSUPPORTED_CONTENT` | `input` |
//! | 7 | An image needs its prepared request version, or `INVALID_REQUEST`; it emits model-facing handle text **then** the image | `input` |
//! | 8 | When file ids are in play, an image without one is `INVALID_REQUEST`; otherwise it becomes a `file` source instead of inline base64 | `input` |
//! | 9 | Developer messages become `system` turns of `tool_addition`/`tool_removal` references plus text; an empty text block is dropped, and an all-empty developer message emits nothing | main loop |
//! | 10 | A `tool-addition`/`tool-removal` outside a developer message is `UNSUPPORTED_CONTENT` in *every* role | main loop |
//! | 11 | A system message must be all text, joined with `''`; the latest snapshot replaces earlier ones; an empty later snapshot **clears** the prompt | main loop |
//! | 12 | On an `in-history` route, a system message after the first turn becomes a `system` turn instead of the top-level prompt | main loop |
//! | 13 | Deferred system updates flush *after* the preceding user/tool-result turn and *before* the next assistant; a flush with no preceding user turn is `UNSUPPORTED_CONTENT`; a trailing flush is allowed | `flushSystemUpdates` |
//! | 14 | Tool results are `user` turns holding `tool_result`; `is_error` appears only when the message declares it | main loop |
//! | 15 | Empty `user` and `assistant` turns are dropped; empty tool results keep their call id and become `content: []` | main loop |
//! | 16 | Adjacent turns of the same wire role merge by appending blocks | main loop |
//! | 17 | The fold runs on a copy: durable messages are never mutated | main loop |
//! | 18 | `tool_use` ids must be unique within one assistant turn (`INVALID_REQUEST`) | validation |
//! | 19 | Every `tool_result` must match a pending call, and every pending call must be answered by the *next* user turn | validation |
//! | 20 | A history ending with unresolved calls is `INVALID_REQUEST` | validation |
//! | 21 | Within a merged user turn, `tool_result` blocks move in front of ordinary content | validation |
//! | 22 | Effort resolves request → route default → (`off` when thinking is disabled, else `high`); `off`/`low`/`high`/`max` are the only accepted values | effort block |
//! | 23 | `thinking` is always present; `output_config.effort` appears only when the effort is not `off` | envelope |
//! | 24 | `max_tokens` resolves request → model entry → connection | envelope |
//! | 25 | `system` is the request prompt and the collected history snapshot joined with `\n\n`, omitted entirely when empty | envelope |
//! | 26 | `temperature` and `stop_sequences` are omitted when unset; tools map `parameters` → `input_schema`; `defer_loading: true` only on deferred tools; absent and empty tool lists stay absent and empty | envelope |
//!
//! # What this crate already covers, and what the gap is
//!
//! `girder_core::llm::deepseek` (657 lines) is the *transport* half: an SSE parser, a `Translator`
//! turning Anthropic events into [`girder_core::StreamChunk`]s, `finish_reason` mapping, and a
//! `body` method that walks [`girder_core::LlmRequest`] to JSON. Measured against the table above,
//! `body` implements rule 2 (without signatures), 3, 14 (without `is_error`), 24, 25, and 26
//! (without `defer_loading`, `thinking`, `output_config`, or `stop_sequences`). It implements none
//! of 1, 5, 9–13, 15–23: it emits one wire turn per durable message, so an empty assistant text
//! stays `{"type":"text","text":""}`, a `reasoning` block becomes an empty text block, and two
//! adjacent user turns stay two turns.
//!
//! Two of those absences are *protocol errors* rather than cosmetic:
//!
//! - `body` never emits `thinking`, and the Messages API requires it whenever an assistant turn
//!   carries a `thinking` block — which is exactly the history this harness records. A multi-turn
//!   conversation with reasoning is therefore not merely lossy; it is ill-formed.
//! - `body` never merges or orders tool results, so a parallel tool call followed by two results
//!   sends `user, user` where the API requires the results to answer the call in the turn
//!   immediately after it.
//!
//! # What this module is
//!
//! The pure part of `serialize.ts`, ported rule for rule, taking this crate's durable
//! [`girder_core::ChatMessage`] and producing wire JSON. No network, no clock, no filesystem. Every
//! decision is one of drop, omit, merge, reorder, or refuse — which is why it is worth porting
//! before the provider is rewired to use it.
//!
//! Three deliberate differences from upstream, each forced by a local fact:
//!
//! 1. **Refusals are a typed enum, not an exception.** Upstream throws `LlmError` with a code
//!    string; [`girder_core::LlmError`] has two variants and both are transport failures.
//!    [`SerializeError`] is the closed set of refusal reasons, so a caller matches on *why* a
//!    history is unrepresentable instead of parsing a message.
//! 2. **The image *pipeline* is not ported, the image *wire shape* is.** Upstream's image branch
//!    needs `prepareImages` and a request-version map, and this crate's durable model has no
//!    prepare step — `crate::llm::deepseek::body` replaces an image with `[image <id>]` text today.
//!    [`SerializeImage`] is the value the caller supplies (handle text, media type, bytes, optional
//!    file id), and the two failures upstream raises for a missing version become
//!    [`SerializeError::MissingImage`] and [`SerializeError::MissingFileId`]. The order of blocks —
//!    handle text, then inline base64 or a file reference — is upstream's.
//! 3. **`defer_loading` and `stop_sequences` are request fields.** [`girder_core::ToolSchema`] has
//!    no `defer_loading`, so [`SerializeTool`] wraps it; the durable list is still accepted whole
//!    through [`SerializeRequest::from_llm`].
//!
//! # What is deliberately not here
//!
//! - **No `body` replacement.** `girder_core::llm::deepseek` is the integrator's file; this module is
//!   a sibling a test binary can include by path without touching the crate root.
//! - **Rule 10 cannot fire.** This crate's [`girder_core::ContentBlock`] has no
//!   `tool-addition`/`tool-removal` tag, so no durable block can be a tool change outside a
//!   developer message. The wire shape the rule protects is ported
//!   ([`developer_update_from_blocks`]); the guard itself has nothing to guard against yet, and is
//!   documented rather than faked with a check that can never be true.
//! - **Rule 14's `is_error` is absent, not `false`.** [`girder_core::MessageSource::Tool`] carries
//!   no error flag. Emitting `is_error: false` would claim a fact the log does not hold, which is
//!   the one thing this module must never do.
//! - **No `systemPromptUpdate` model lookup.** Whether a system message goes in history depends on
//!   the route's capability record; [`SerializeRoute::system_prompt_update_in_history`] is that
//!   input, passed in rather than discovered.
//!
//! # Why the fold runs on a copy
//!
//! Upstream's suite asserts, repeatedly, that serializing a history leaves it byte-identical
//! (`expect(JSON.stringify(history)).toBe(saved)`). The rules here build new turns rather than
//! mutate durable ones, and [`serialize`] takes `&[ChatMessage]`: a refused signature, a tool
//! result that moves to the front of its turn, and a superseded system snapshot all leave the log
//! untouched.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use girder_core::llm::stream::{
    DeepSeekReplayBlock, ReplayBlockKind, deepseek_replay_state, read_deepseek_replay,
};
use girder_core::{
    ChatMessage, ChatRole, ContentBlock, LlmRequest, MessageId, MessageSource, Role, ToolSchema,
};

// =============================================================================================
// Refusals
// =============================================================================================

/// Why a history cannot be represented as a `DeepSeek Messages` request.
///
/// Upstream carries these as `LlmError` codes plus a message; each is its own variant here because
/// the caller's response differs. A tool-pairing failure is a *bug in the history*, and the loop
/// that produced it should be fixed; `UnsupportedContent` is a capability question, and the answer
/// is to compress or drop the block. Collapsing both into one string would make them
/// indistinguishable exactly where the distinction matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SerializeError {
    /// A block has no representation in this dialect.
    ///
    /// Carries the upstream-shaped description (`assistant content plugin:text`, `system update
    /// without a preceding user or tool-result turn`, …) so a log line still reads the way the
    /// operator's documentation says it does.
    UnsupportedContent {
        /// What could not be represented.
        detail: String,
    },

    /// A `tool_use` id repeats within one assistant turn.
    DuplicateToolCallId {
        /// The repeated id.
        id: String,
    },

    /// A `tool_result` names a call that is not pending.
    UnmatchedToolResult {
        /// The offending call id.
        id: String,
    },

    /// An assistant turn's calls were not all answered by the turn immediately after it.
    PendingToolCalls {
        /// The call ids still unanswered when the next turn closed.
        ids: Vec<String>,
    },

    /// The history ends with calls no turn answers.
    UnresolvedTrailingToolCalls {
        /// The call ids left open at the end.
        ids: Vec<String>,
    },

    /// The route disables thinking but the request asks for an effort other than `off`.
    UnsupportedReasoningEffort {
        /// The effort as written.
        effort: String,
    },

    /// An image block was serialized without a prepared request version.
    MissingImage {
        /// The attachment id.
        attachment: String,
    },

    /// The request demands file references but this image has none.
    MissingFileId {
        /// The attachment id.
        attachment: String,
    },
}

impl std::fmt::Display for SerializeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedContent { detail } => {
                write!(formatter, "DeepSeek Messages cannot represent {detail}")
            }
            Self::DuplicateToolCallId { id } => {
                write!(formatter, "DeepSeek Messages duplicate tool call id `{id}`")
            }
            Self::UnmatchedToolResult { id } => write!(
                formatter,
                "DeepSeek Messages tool result `{id}` has no matching call"
            ),
            Self::PendingToolCalls { ids } => write!(
                formatter,
                "DeepSeek Messages tool calls need immediate results: {}",
                ids.join(", ")
            ),
            Self::UnresolvedTrailingToolCalls { ids } => write!(
                formatter,
                "DeepSeek Messages history ends with unresolved tools: {}",
                ids.join(", ")
            ),
            Self::UnsupportedReasoningEffort { effort } => write!(
                formatter,
                "DeepSeek Messages does not support reasoning effort {effort}"
            ),
            Self::MissingImage { attachment } => {
                write!(
                    formatter,
                    "DeepSeek Messages request image {attachment} is missing"
                )
            }
            Self::MissingFileId { attachment } => write!(
                formatter,
                "DeepSeek Messages request file id for {attachment} is missing"
            ),
        }
    }
}

impl std::error::Error for SerializeError {}

/// Refuses a block with the upstream-shaped description.
fn unsupported(detail: impl Into<String>) -> SerializeError {
    SerializeError::UnsupportedContent {
        detail: detail.into(),
    }
}

// =============================================================================================
// Inputs
// =============================================================================================

/// A reasoning effort this dialect accepts.
///
/// A closed enum rather than a string: upstream validates `['off','low','high','max']` at request
/// time and throws on anything else, which is the behaviour of an enum that was never written down.
/// Here an unknown effort cannot reach [`serialize`] at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningEffort {
    /// No thinking, and no `output_config`.
    Off,
    /// The lowest thinking budget.
    Low,
    /// The default budget.
    High,
    /// The largest budget.
    Max,
}

impl ReasoningEffort {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    /// Parses a wire spelling, or `None` when this dialect has no such effort.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// Whether thinking is disabled at this effort.
    #[must_use]
    pub fn is_off(self) -> bool {
        self == Self::Off
    }
}

/// Capability and default record for the route being serialized.
///
/// Upstream reads these off `connection` and the resolved model entry. They are inputs here because
/// they are provider configuration, not conversation state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerializeRoute {
    /// Model id the request is for, and the target the replay reader validates against.
    pub model: String,
    /// Whether this route keeps later system snapshots in the message history.
    ///
    /// Upstream's `systemPromptUpdate: 'in-history'`. When false, the last system snapshot becomes
    /// the top-level prompt and never appears as a turn.
    pub system_prompt_update_in_history: bool,
    /// Route-level thinking policy: when false, only [`ReasoningEffort::Off`] is accepted.
    pub thinking_enabled: bool,
    /// Route default effort, used when the request names none.
    pub default_effort: Option<ReasoningEffort>,
    /// Route default `max_tokens`, the last resort after the request and the model entry.
    pub max_tokens: u32,
    /// The model entry's own `max_tokens`, which outranks the route default.
    pub model_max_tokens: Option<u32>,
}

impl SerializeRoute {
    /// A route with upstream's defaults: thinking on, no `in-history` updates, no model limit.
    #[must_use]
    pub fn new(model: impl Into<String>, max_tokens: u32) -> Self {
        Self {
            model: model.into(),
            system_prompt_update_in_history: false,
            thinking_enabled: true,
            default_effort: None,
            max_tokens,
            model_max_tokens: None,
        }
    }

    /// Keeps later system snapshots in the history.
    #[must_use]
    pub fn with_system_prompt_update_in_history(mut self, in_history: bool) -> Self {
        self.system_prompt_update_in_history = in_history;
        self
    }

    /// Sets the route's thinking policy.
    #[must_use]
    pub fn with_thinking_enabled(mut self, enabled: bool) -> Self {
        self.thinking_enabled = enabled;
        self
    }

    /// Sets the route default effort.
    #[must_use]
    pub fn with_default_effort(mut self, effort: ReasoningEffort) -> Self {
        self.default_effort = Some(effort);
        self
    }

    /// Sets the model entry's own output limit.
    #[must_use]
    pub fn with_model_max_tokens(mut self, max_tokens: u32) -> Self {
        self.model_max_tokens = Some(max_tokens);
        self
    }

    /// The effort in force: the request's, then the route default, then `off` when thinking is
    /// disabled and `high` otherwise.
    ///
    /// Rule 22 in one expression. The two fallbacks are written as one `match` rather than two call
    /// sites so the policy cannot drift between "the request named an effort" and "it did not".
    #[must_use]
    pub fn resolve_effort(&self, requested: Option<ReasoningEffort>) -> ReasoningEffort {
        match requested.or(self.default_effort) {
            Some(effort) => effort,
            None if self.thinking_enabled => ReasoningEffort::High,
            None => ReasoningEffort::Off,
        }
    }
}

/// One image as the serializer sees it.
///
/// Upstream passes prepared request versions and file ids as two maps and raises
/// `INVALID_REQUEST` when either lookup misses. Here the two lookups fold into one value the caller
/// assembles, which turns "no request version" and "no file id" into states of the input rather
/// than of a map — and keeps the two refusals distinguishable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerializeImage {
    /// The durable attachment id this block names.
    pub attachment: String,
    /// The model-facing handle text that precedes the image.
    ///
    /// Upstream builds this with `requestImageHandleText`; see [`image_handle_text`], which ports
    /// the same sentence for callers that have not ported the attachment pipeline.
    pub handle_text: String,
    /// The request version's media type, for an inline source.
    pub media_type: String,
    /// The request version's bytes, for an inline source.
    pub data: Vec<u8>,
    /// A resolved Files id, which switches the source from inline to `file`.
    pub file_id: Option<String>,
}

impl SerializeImage {
    /// An inline image described by its parts.
    #[must_use]
    pub fn inline(
        attachment: impl Into<String>,
        handle_text: impl Into<String>,
        media_type: impl Into<String>,
        data: Vec<u8>,
    ) -> Self {
        Self {
            attachment: attachment.into(),
            handle_text: handle_text.into(),
            media_type: media_type.into(),
            data,
            file_id: None,
        }
    }

    /// The same image referenced by a Files id instead of inline bytes.
    #[must_use]
    pub fn with_file_id(mut self, file_id: impl Into<String>) -> Self {
        self.file_id = Some(file_id.into());
        self
    }
}

/// Looks up the prepared request version for one attachment id.
///
/// [`serialize`] takes this as a lookup, exactly as upstream takes `images`. `None` means "no
/// version for this attachment", which becomes [`SerializeError::MissingImage`].
pub type ImageLookup<'a> = &'a dyn Fn(&str) -> Option<SerializeImage>;

/// One tool as declared to the model.
///
/// [`ToolSchema`] is the durable shape and has no `defer_loading`; this wraps it so a deferred tool
/// can be declared without changing the durable type.
#[derive(Clone, Debug, PartialEq)]
pub struct SerializeTool {
    /// The tool's declaration.
    pub schema: ToolSchema,
    /// Whether the tool stays unavailable until a `tool_addition` activates it.
    pub defer_loading: bool,
}

impl SerializeTool {
    /// A tool that is available from the first turn.
    #[must_use]
    pub fn new(schema: ToolSchema) -> Self {
        Self {
            schema,
            defer_loading: false,
        }
    }

    /// A tool activated later by a `tool_addition` block.
    #[must_use]
    pub fn deferred(schema: ToolSchema) -> Self {
        Self {
            schema,
            defer_loading: true,
        }
    }
}

/// Everything [`serialize`] needs that is not the history.
#[derive(Clone, Debug, PartialEq)]
pub struct SerializeRequest {
    /// The route being serialized for.
    pub route: SerializeRoute,
    /// One-shot system prompt, prepended to the collected history snapshot.
    pub system: Option<String>,
    /// Output limit for this call, above the model and route limits.
    pub max_tokens: Option<u32>,
    /// The effort this call asks for.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Stop sequences.
    pub stop_sequences: Option<Vec<String>>,
    /// Whether every image must be a `file` reference.
    ///
    /// Upstream's `fileIds !== undefined`. A deployment that has ported Files sets this; one that
    /// has not leaves it false and gets inline base64, which is what this crate does today.
    pub require_file_ids: bool,
    /// Declared tools, or `None` for a tool-less request.
    ///
    /// `None` and `Some(vec![])` are kept apart because upstream keeps them apart: `tools:
    /// undefined` is absent from the body and `tools: []` is present and empty. A caller that
    /// collapses them sends a different request than it meant to.
    pub tools: Option<Vec<SerializeTool>>,
}

impl SerializeRequest {
    /// A request for `route` with everything optional unset.
    #[must_use]
    pub fn new(route: SerializeRoute) -> Self {
        Self {
            route,
            system: None,
            max_tokens: None,
            reasoning_effort: None,
            temperature: None,
            stop_sequences: None,
            require_file_ids: false,
            tools: None,
        }
    }

    /// Builds the request fields from a durable [`LlmRequest`].
    ///
    /// Walks the local type rather than requiring callers to restate it: `system_prompt` becomes
    /// [`Self::system`], `max_tokens` stays, and the tool list becomes [`SerializeTool::new`]
    /// entries — this crate has no deferred tools yet, so nothing is deferred.
    #[must_use]
    pub fn from_llm(route: SerializeRoute, request: &LlmRequest) -> Self {
        Self {
            route,
            system: request.system_prompt.clone(),
            max_tokens: request.max_tokens,
            reasoning_effort: None,
            temperature: None,
            stop_sequences: None,
            require_file_ids: false,
            tools: Some(
                request
                    .tools
                    .iter()
                    .cloned()
                    .map(SerializeTool::new)
                    .collect(),
            ),
        }
    }

    /// Sets the one-shot system prompt.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets the output limit.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the reasoning effort.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    /// Sets the temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Sets the stop sequences.
    #[must_use]
    pub fn with_stop_sequences(mut self, stop: Vec<String>) -> Self {
        self.stop_sequences = Some(stop);
        self
    }

    /// Demands `file` sources for every image.
    #[must_use]
    pub fn with_require_file_ids(mut self, required: bool) -> Self {
        self.require_file_ids = required;
        self
    }

    /// Declares the tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<SerializeTool>) -> Self {
        self.tools = Some(tools);
        self
    }
}

/// Where a degraded replay envelope is reported.
///
/// Upstream's `onReplayDegrade` is an optional callback receiving no message content and no
/// signature — only the reason. [`ReplayNotice`] is that call; the reason strings are the ones the
/// replay reader already produces.
pub trait ReplayNotice {
    /// Reports that an envelope was present but unusable.
    fn degrade(&mut self, reason: &str);
}

/// The notice that discards diagnostics.
///
/// Upstream's argument omitted. A caller that only wants a request does not have to write a
/// closure that throws its argument away.
#[derive(Clone, Copy, Debug, Default)]
pub struct SilentReplay;

impl ReplayNotice for SilentReplay {
    fn degrade(&mut self, _reason: &str) {}
}

impl<F: FnMut(&str)> ReplayNotice for F {
    fn degrade(&mut self, reason: &str) {
        self(reason);
    }
}

// =============================================================================================
// Small pure helpers
// =============================================================================================

/// Upstream's `toolInput`: a raw argument string as the object the API expects.
///
/// Three ways to get `{}`: invalid JSON, a JSON value that is not an object, and a JSON array.
/// `null` also lands in `{}`. The API wants an object, and a history that cannot supply one is
/// still a history worth sending — the tool executor reports the invalid arguments to the model,
/// which is more useful than failing the turn.
#[must_use]
pub fn tool_input(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) if value.is_object() => value,
        Ok(_) | Err(_) => json!({}),
    }
}

/// The file extension upstream maps a media type to, or `None` for one it does not know.
fn extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/webp" => Some(".webp"),
        "image/gif" => Some(".gif"),
        _ => None,
    }
}

/// The suffix of the model-facing image handle when a normalized copy is readable.
///
/// Mirrors upstream's `normalizedAccessText`. Upstream's `extension` uses `assertNever` and throws
/// on an unknown media type; this returns no extension and drops the "copy to a writable path"
/// sentence, because refusing a whole request over a media type the model can still see is worse
/// than sending it without the hint.
fn normalized_access_text(
    readonly_path: &str,
    width: u32,
    height: u32,
    media_type: &str,
) -> String {
    let mut text = format!(
        " Normalized copy (read-only; may be resized or re-encoded): \"{readonly_path}\" \
         ({width}x{height}px, {media_type}). Source dimensions, format, and byte size may differ."
    );
    if let Some(extension) = extension(media_type) {
        let _ = write!(
            text,
            " Copy to a writable path ending in {extension} before editing."
        );
    }
    text
}

/// Builds the model-facing handle text upstream's `requestImageHandleText` produces.
///
/// Ported because the wire shape of an image block depends on it: the text comes *before* the image
/// in the block list, so a port that emitted only the image would be a different request. The
/// identity rule is upstream's — a named attachment shows as `"name" (id)`, an unnamed one as the
/// bare id.
#[must_use]
pub fn image_handle_text(
    attachment: &str,
    name: Option<&str>,
    width: u32,
    height: u32,
    media_type: &str,
    access: Option<&str>,
) -> String {
    let identity = match name {
        None => attachment.to_owned(),
        Some(name) => format!("{} ({attachment})", json!(name)),
    };
    let preview = format!("Image {identity}; request preview {width}x{height}px.");
    match access {
        None => format!(
            "{preview} It may be resized or re-encoded; source dimensions, format, and byte size \
             may differ."
        ),
        Some(path) => format!(
            "{preview}{}",
            normalized_access_text(path, width, height, media_type)
        ),
    }
}

// =============================================================================================
// Block conversion
// =============================================================================================

/// The wire form of one image: handle text, then the source.
fn image_to_wire(image: &SerializeImage) -> Vec<Value> {
    let source = match &image.file_id {
        Some(file_id) => json!({ "type": "file", "file_id": file_id }),
        None => json!({
            "type": "base64",
            "media_type": image.media_type,
            "data": girder_core::encode_base64(&image.data),
        }),
    };
    vec![
        json!({ "type": "text", "text": image.handle_text }),
        json!({ "type": "image", "source": source }),
    ]
}

/// User and tool-result content: text and images, with assistant-only blocks omitted.
///
/// Rules 5 and 6. `reasoning` and `tool-call` are *dropped*, not refused: a user turn carrying them
/// is a subagent transcript relayed into this one, and the assistant-only half is not this model's
/// to see. Anything else is a capability question and is refused.
fn input_blocks(
    blocks: &[ContentBlock],
    images: ImageLookup<'_>,
    require_file_ids: bool,
) -> Result<Vec<Value>, SerializeError> {
    let mut wire = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    wire.push(json!({ "type": "text", "text": text }));
                }
            }
            ContentBlock::Reasoning { .. } | ContentBlock::ToolCall { .. } => {}
            ContentBlock::Image { attachment } => {
                let image = images(attachment).ok_or_else(|| SerializeError::MissingImage {
                    attachment: attachment.clone(),
                })?;
                if require_file_ids && image.file_id.is_none() {
                    return Err(SerializeError::MissingFileId {
                        attachment: attachment.clone(),
                    });
                }
                wire.extend(image_to_wire(&image));
            }
            other @ ContentBlock::File { .. } => {
                return Err(unsupported(format!(
                    "user/tool-result content {}",
                    other.type_name()
                )));
            }
        }
    }
    Ok(wire)
}

/// Assistant content: text, thinking with its replayed signature, and tool calls.
///
/// Rule 1's subtlety lives here: the replay envelope is index-aligned with the *durable* content, so
/// an empty text block that is dropped must still consume its envelope entry. That is why the loop
/// indexes durable positions rather than counting emitted blocks — a reader that counted emitted
/// blocks would attach the second entry's signature to the first surviving block.
fn assistant_blocks(
    message: &ChatMessage,
    replay: &[DeepSeekReplayBlock],
) -> Result<Vec<Value>, SerializeError> {
    let mut wire = Vec::new();
    for (index, block) in message.content.iter().enumerate() {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    wire.push(json!({ "type": "text", "text": text }));
                }
            }
            ContentBlock::Reasoning { text } => {
                let signature = replay
                    .get(index)
                    .filter(|entry| entry.kind == ReplayBlockKind::Reasoning)
                    .and_then(|entry| entry.signature.clone());
                let mut value = Map::new();
                value.insert("type".to_owned(), Value::from("thinking"));
                value.insert("thinking".to_owned(), Value::from(text.clone()));
                if let Some(signature) = signature {
                    value.insert("signature".to_owned(), Value::from(signature));
                }
                wire.push(Value::Object(value));
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => wire.push(json!({
                "type": "tool_use",
                "id": id.as_str(),
                "name": name,
                "input": tool_input(arguments),
            })),
            other => {
                // Rule 4. An image in an assistant turn is refused by this arm for the same
                // reason: the model cannot have produced one, so a history claiming it is damaged.
                return Err(unsupported(format!(
                    "assistant content {}",
                    other.type_name()
                )));
            }
        }
    }
    Ok(wire)
}

/// Reads a developer message's blocks into a `system` update.
///
/// Rule 9. This crate's durable model records a developer turn as text and has no
/// `tool-addition` block, so a developer turn contributes text; an empty text block is dropped and
/// an all-empty message produces no update at all. The tool-reference wire shape is available
/// through [`developer_update_from_blocks`].
fn developer_update(message: &ChatMessage) -> Result<Vec<Value>, SerializeError> {
    let mut blocks = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            other => {
                return Err(unsupported(format!(
                    "developer content {}",
                    other.type_name()
                )));
            }
        }
    }
    Ok(blocks)
}

/// The `system` update a set of tool changes and text blocks produces.
///
/// The exact wire shape upstream emits for a developer message, exposed separately so a caller
/// whose block model *can* express tool changes (this crate's cannot yet) produces identical JSON.
/// Additions precede removals and text follows both, which is upstream's block order for the
/// canonical a-then-r developer message; a caller with a mixed order should emit the blocks itself
/// rather than reorder them through this.
#[must_use]
pub fn developer_update_from_blocks(
    additions: &[&str],
    removals: &[&str],
    text_blocks: &[&str],
) -> Vec<Value> {
    let mut blocks = Vec::new();
    for name in additions {
        blocks.push(json!({
            "type": "tool_addition",
            "tool": { "type": "tool_reference", "name": name },
        }));
    }
    for name in removals {
        blocks.push(json!({
            "type": "tool_removal",
            "tool": { "type": "tool_reference", "name": name },
        }));
    }
    for text in text_blocks {
        if !text.is_empty() {
            blocks.push(json!({ "type": "text", "text": text }));
        }
    }
    blocks
}

// =============================================================================================
// The fold
// =============================================================================================

/// A wire turn's role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WireRole {
    User,
    Assistant,
    System,
}

impl WireRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }
}

/// One wire turn under construction.
///
/// Kept apart from the emitted JSON so merging can append blocks and the pairing pass can inspect
/// roles and tags without re-parsing.
#[derive(Clone, Debug)]
struct WireTurn {
    role: WireRole,
    blocks: Vec<Value>,
}

impl WireTurn {
    fn new(role: WireRole, blocks: Vec<Value>) -> Self {
        Self { role, blocks }
    }

    fn is_tool_result(block: &Value) -> bool {
        block.get("type").and_then(Value::as_str) == Some("tool_result")
    }
}

/// Maps the request `Role` onto the log `Role`.
fn role_of(role: Role) -> ChatRole {
    match role {
        Role::Assistant => ChatRole::Assistant,
        Role::Tool => ChatRole::Tool,
        Role::User => ChatRole::User,
    }
}

/// Reads the replay envelope for one assistant message.
///
/// Absence and cross-model declines are silent; a damaged envelope is reported to `notice` and the
/// content is serialized without signatures, which is upstream's behaviour.
fn read_replay(
    message: &ChatMessage,
    target_model: &str,
    notice: &mut dyn ReplayNotice,
) -> Vec<DeepSeekReplayBlock> {
    let MessageSource::Model {
        model: source_model,
        replay_state,
        ..
    } = &message.source
    else {
        return Vec::new();
    };
    let outcome = read_deepseek_replay(
        &message.content,
        source_model,
        target_model,
        replay_state.as_ref(),
    );
    // Collect the diagnostic before consuming the outcome: `diagnostic` borrows it.
    let diagnostic = outcome.diagnostic().map(std::borrow::ToOwned::to_owned);
    if let Some(reason) = diagnostic {
        notice.degrade(&reason);
    }
    outcome.usable().unwrap_or_default()
}

/// Rule 13: emit queued system updates, which must follow a user turn.
fn flush_system_updates(
    messages: &mut Vec<WireTurn>,
    system_updates: &mut Vec<WireTurn>,
) -> Result<(), SerializeError> {
    if system_updates.is_empty() {
        return Ok(());
    }
    if messages.last().map(|turn| turn.role) != Some(WireRole::User) {
        return Err(unsupported(
            "system update without a preceding user or tool-result turn",
        ));
    }
    messages.append(system_updates);
    Ok(())
}

/// Rule 16: append to a same-role turn, otherwise start a new one.
fn push_turn(messages: &mut Vec<WireTurn>, role: WireRole, blocks: Vec<Value>) {
    match messages.last_mut() {
        Some(previous) if previous.role == role => previous.blocks.extend(blocks),
        _ => messages.push(WireTurn::new(role, blocks)),
    }
}

/// The wire role a durable role becomes.
fn wire_role(role: ChatRole) -> WireRole {
    match role {
        ChatRole::Assistant => WireRole::Assistant,
        ChatRole::System => WireRole::System,
        // The Messages API has no `tool` and no `developer` role: a tool result is a `user` turn.
        ChatRole::Tool | ChatRole::User | ChatRole::Developer => WireRole::User,
    }
}

/// Rules 18–21: validate pairing and move results ahead of ordinary content.
fn validate_tool_pairing(messages: &mut [WireTurn]) -> Result<(), SerializeError> {
    let mut pending: BTreeSet<String> = BTreeSet::new();
    for turn in messages.iter_mut() {
        match turn.role {
            WireRole::Assistant => {
                // Rule 18: a repeated id inside one assistant turn is a history bug. Upstream
                // replaces `pending` with this turn's calls, and so does this.
                pending.clear();
                let mut seen: HashSet<String> = HashSet::new();
                for block in &turn.blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if !seen.insert(id.clone()) {
                        return Err(SerializeError::DuplicateToolCallId { id });
                    }
                    pending.insert(id);
                }
            }
            WireRole::User => {
                let results: Vec<String> = turn
                    .blocks
                    .iter()
                    .filter(|block| WireTurn::is_tool_result(block))
                    .filter_map(|block| {
                        block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect();
                for id in &results {
                    // Rule 19: a result with no pending call, or one answering a call from an
                    // older turn, is refused rather than sent.
                    if !pending.remove(id) {
                        return Err(SerializeError::UnmatchedToolResult { id: id.clone() });
                    }
                }
                if !pending.is_empty() {
                    return Err(SerializeError::PendingToolCalls {
                        ids: pending.iter().cloned().collect(),
                    });
                }
                // Rule 21: results first, then ordinary content. `sort_by_key` is stable, so the
                // order within each group is the order the history gave.
                turn.blocks
                    .sort_by_key(|block| i32::from(!WireTurn::is_tool_result(block)));
            }
            WireRole::System => {}
        }
    }
    if !pending.is_empty() {
        return Err(SerializeError::UnresolvedTrailingToolCalls {
            ids: pending.into_iter().collect(),
        });
    }
    Ok(())
}

/// Rules 23–26: the envelope.
fn build_body(
    request: &SerializeRequest,
    effort: ReasoningEffort,
    history_system: Option<&str>,
    messages: &[WireTurn],
) -> Value {
    let route = &request.route;
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::from(route.model.clone()));
    body.insert("stream".to_owned(), Value::from(true));
    body.insert(
        "messages".to_owned(),
        Value::Array(
            messages
                .iter()
                .map(|turn| {
                    json!({
                        "role": turn.role.as_str(),
                        "content": turn.blocks,
                    })
                })
                .collect(),
        ),
    );
    // Rule 24: the request wins, then the model entry, then the route default.
    body.insert(
        "max_tokens".to_owned(),
        Value::from(
            request
                .max_tokens
                .or(route.model_max_tokens)
                .unwrap_or(route.max_tokens),
        ),
    );
    // Rule 23: `thinking` is unconditional; `output_config` only when thinking is on.
    body.insert(
        "thinking".to_owned(),
        json!({ "type": if effort.is_off() { "disabled" } else { "enabled" } }),
    );
    if !effort.is_off() {
        body.insert(
            "output_config".to_owned(),
            json!({ "effort": effort.as_str() }),
        );
    }
    // Rule 25: request prompt and history snapshot, blank-line separated, omitted when the join is
    // empty. An empty history snapshot contributes nothing rather than an empty paragraph.
    let system = [request.system.as_deref(), history_system]
        .into_iter()
        .flatten()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::from(system));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(stop) = &request.stop_sequences {
        body.insert("stop_sequences".to_owned(), json!(stop));
    }
    if let Some(tools) = &request.tools {
        body.insert(
            "tools".to_owned(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        let mut entry = Map::new();
                        entry.insert("name".to_owned(), Value::from(tool.schema.name.clone()));
                        entry.insert(
                            "description".to_owned(),
                            Value::from(tool.schema.description.clone()),
                        );
                        entry.insert("input_schema".to_owned(), tool.schema.parameters.clone());
                        // Rule 26: only a deferred tool carries the flag. `false` is absent, not
                        // written, because the API treats their presence as significant.
                        if tool.defer_loading {
                            entry.insert("defer_loading".to_owned(), Value::from(true));
                        }
                        Value::Object(entry)
                    })
                    .collect(),
            ),
        );
    }
    Value::Object(body)
}

/// Serializes a history and request into a `DeepSeek Messages` JSON body.
///
/// The rules are the table in the module documentation, in upstream's order. The one structural
/// choice worth naming: the system-update queue flushes *only* when an assistant turn is about to
/// be emitted (rule 13), because the Messages API admits a `system` turn only between a user turn
/// and the next assistant turn. A pending update with no such slot is refused rather than dropped —
/// silently dropping it would send a request the operator did not ask for.
///
/// `notice` receives one call per assistant message whose replay envelope was present but unusable.
///
/// # Errors
///
/// Returns the [`SerializeError`] matching the first rule the history violates. The history is not
/// modified on any path, error or success.
pub fn serialize(
    request: &SerializeRequest,
    history: &[ChatMessage],
    images: ImageLookup<'_>,
    notice: &mut dyn ReplayNotice,
) -> Result<Value, SerializeError> {
    let route = &request.route;
    // Rules 22–23: resolve the effort first, because a request this route cannot honour should fail
    // without walking a history it is going to discard.
    let effort = route.resolve_effort(request.reasoning_effort);
    if !route.thinking_enabled && !effort.is_off() {
        return Err(SerializeError::UnsupportedReasoningEffort {
            effort: effort.as_str().to_owned(),
        });
    }

    let mut messages: Vec<WireTurn> = Vec::new();
    let mut history_system: Option<String> = None;
    let mut system_updates: Vec<WireTurn> = Vec::new();
    let mut saw_turn = false;

    for message in history {
        match message.role {
            ChatRole::Developer => {
                let blocks = developer_update(message)?;
                if !blocks.is_empty() {
                    system_updates.push(WireTurn::new(WireRole::System, blocks));
                }
                continue;
            }
            ChatRole::System => {
                if message
                    .content
                    .iter()
                    .any(|block| !matches!(block, ContentBlock::Text { .. }))
                {
                    return Err(unsupported("non-text system message"));
                }
                // Rule 11: blocks join with no separator, and the latest snapshot replaces the
                // previous one — including an empty one, which clears the prompt rather than
                // leaving the earlier text in force.
                let text: String = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if route.system_prompt_update_in_history && saw_turn {
                    // Rule 12: an `in-history` route keeps it as a turn, which the flush below
                    // positions after the preceding user turn.
                    if text.is_empty() {
                        return Err(unsupported("empty in-history system update"));
                    }
                    system_updates.push(WireTurn::new(
                        WireRole::System,
                        vec![json!({ "type": "text", "text": text })],
                    ));
                } else {
                    history_system = Some(text);
                }
                continue;
            }
            _ => {}
        }

        let blocks = match message.role {
            ChatRole::Assistant => {
                let replay = read_replay(message, &route.model, notice);
                assistant_blocks(message, &replay)?
            }
            ChatRole::Tool => {
                let call_id = message
                    .source
                    .call_id()
                    .map_or_else(String::new, |id| id.as_str().to_owned());
                let content = input_blocks(&message.content, images, request.require_file_ids)?;
                // Rule 14: `is_error` appears only when the message declares it. A `tool` role
                // guarantees a call id, so an absent one would be a producer bug; the empty string
                // is upstream's fallback and is kept rather than replaced with a guess.
                let mut value = Map::new();
                value.insert("type".to_owned(), Value::from("tool_result"));
                value.insert("tool_use_id".to_owned(), Value::from(call_id));
                value.insert("content".to_owned(), Value::Array(content));
                vec![Value::Object(value)]
            }
            _ => input_blocks(&message.content, images, request.require_file_ids)?,
        };

        // Rule 15: an empty user or assistant turn disappears; an empty tool result does not,
        // because its call id is the answer the API is waiting for.
        if matches!(message.role, ChatRole::User | ChatRole::Assistant) && blocks.is_empty() {
            continue;
        }
        // Rule 13: an assistant turn is the only position a system update may precede. Note the
        // ordering: an omitted empty assistant turn returns above and therefore flushes nothing,
        // which is the behaviour upstream's suite pins down explicitly.
        if message.role == ChatRole::Assistant {
            flush_system_updates(&mut messages, &mut system_updates)?;
        }
        push_turn(&mut messages, wire_role(message.role), blocks);
        saw_turn = true;
    }
    flush_system_updates(&mut messages, &mut system_updates)?;

    validate_tool_pairing(&mut messages)?;

    Ok(build_body(
        request,
        effort,
        history_system.as_deref(),
        &messages,
    ))
}

/// Serializes a durable [`LlmRequest`] with no images and no replay diagnostics.
///
/// The shorthand for a caller that already holds the local request type: [`SerializeRequest::from_llm`]
/// plus [`serialize`]. Kept here rather than in the provider so the provider keeps its own `body`
/// until the integrator switches it over.
///
/// # Errors
///
/// Returns whatever [`serialize`] refuses.
pub fn serialize_llm_request(
    route: SerializeRoute,
    request: &LlmRequest,
) -> Result<Value, SerializeError> {
    serialize_llm_request_with_images(route, request, &|_| None)
}

/// Serializes a durable [`LlmRequest`], resolving each image through `images`.
///
/// # Errors
///
/// Returns whatever [`serialize`] refuses, including [`SerializeError::MissingImage`] when `images`
/// has no bytes for an attachment the request names.
pub fn serialize_llm_request_with_images(
    route: SerializeRoute,
    request: &LlmRequest,
    images: ImageLookup<'_>,
) -> Result<Value, SerializeError> {
    let prepared = SerializeRequest::from_llm(route, request);
    // `request.messages` goes through, not `&[]` -- the shim passed an empty history, so every
    // conversation serialized to `"messages": []` while its `thinking` declaration and tools were
    // correct. A request about nothing is exactly what the API rejects.
    // `LlmRequest` carries `crate::llm::Message`, which is a leaner type than `ChatMessage`: it has
    // no `id` and no `source`, only a `tool_call_id`. `serialize` reads exactly three of those
    // fields -- role, content, and the call id -- so the bridge is small and total.
    // `serialize` reads role, content and the call id; everything else on `ChatMessage` is log
    // bookkeeping the wire never sees. A `tool` role is the one case where the call id lives in
    // `source` rather than beside it, so it is placed there.
    let history: Vec<ChatMessage> = request
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let source = match &message.tool_call_id {
                Some(call_id) => MessageSource::Tool {
                    call_id: call_id.clone(),
                },
                None => match message.role {
                    Role::Assistant => MessageSource::Model {
                        provider: String::new(),
                        model: String::new(),
                        replay_state: None,
                    },
                    _ => MessageSource::User { form: None },
                },
            };
            ChatMessage::new(
                // A synthetic id: the wire has no message ids, and `new` requires one. Failure is
                // unreachable for this format, so the fallback is a constant rather than a panic.
                MessageId::new(format!("wire-{index}"))
                    .unwrap_or_else(|_| MessageId::new("wire").expect("a legal id")),
                // Two `Role` enums exist -- one on the lean request type, one on the log type --
                // and they are not convertible because the log's has variants the request's cannot
                // express (system, relay, notice). The three that overlap map directly.
                role_of(message.role),
                source,
                message.content.clone(),
            )
        })
        .collect();
    serialize(&prepared, &history, images, &mut SilentReplay)
}

/// Builds the envelope upstream's `replayState` writes, for callers and tests assembling history.
///
/// The durable shape is `{ "response": { "kind": "deepseek-messages", "version": 1, "model": … },
/// "blocks": [ … ] }`, which is what
/// [`girder_core::read_deepseek_replay`] validates against.
#[must_use]
pub fn replay_state(model: &str, blocks: &[DeepSeekReplayBlock]) -> Value {
    serde_json::to_value(deepseek_replay_state(model, blocks)).unwrap_or(Value::Null)
}
