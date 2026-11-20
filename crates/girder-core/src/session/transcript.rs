//! A session as another product's chat history: the `OpenAI` and `Anthropic` message shapes.
//!
//! # Why this is not `session.export`
//!
//! `session.export` answers Markdown, JSON, or HTML for a *reader*: a person pasting a transcript
//! into a ticket, a program replaying the log, a browser opening a shared link. All three describe
//! this harness's own vocabulary, and a session restored from any of them comes back to *this*
//! harness.
//!
//! A person who asks to "export my session as `ChatGPT` format" wants the opposite: something another
//! product can read. That is not a rendering problem but a *vocabulary* problem. The two target
//! shapes carry roles, text, and nothing else; a session log carries five roles, tool calls with
//! provider-issued ids and raw argument JSON, tool results, reasoning blocks, model selections, and
//! per-step bookkeeping. This module is therefore a deliberate projection, and its value is in what
//! it decides to drop, in what order, and why.
//!
//! # The two shapes, side by side
//!
//! | | `OpenAI` Chat Completions | `Anthropic` Messages |
//! |---|---|---|
//! | the conversation | `messages: [{role, content}, …]` | `messages: [{role, content}, …]` |
//! | the person | `"user"` | `"user"` |
//! | the model | `"assistant"` | `"assistant"` |
//! | the system prompt | one more message, `{"role": "system"}` | **top-level `system` field**, never a message |
//! | `content` | a string or typed blocks | a string or typed blocks |
//!
//! So the answer to "are the role names different" is **no for the two conversational roles** and
//! **yes for the system prompt** — not its name but its *position*. `OpenAI` will accept a
//! `system` message anywhere in the array; the `Anthropic` API has no `system` role at all and
//! raises the prompt out of the conversation into a field of its own. That single difference is why
//! [`export`] produces a neutral transcript that still holds the prompt as a `system` message, and
//! [`to_json`] is the step that moves it.
//!
//! # Tool calls have no counterpart, so they become labelled text
//!
//! Both protocols *do* have a native tool protocol — `tool_calls` plus a `tool` role in `OpenAI`,
//! `tool_use`/`tool_result` blocks in `Anthropic` — and neither is usable here:
//!
//! - The native protocols are *request* structures, validated pairwise: every `tool_call` must be
//!   answered by a result carrying the same id, in the very next message, or the API rejects the
//!   whole request. A session log that was interrupted, that has a call whose tool was refused
//!   before it ran, or that was compacted mid-turn would export as an invalid request.
//! - They carry *structure* the log does not: the target product's own block ids, its own content
//!   typing, and in `Anthropic`'s case a strict user/assistant alternation that a tool result
//!   breaks. Reconstructing that faithfully is inventing data, not exporting it.
//! - The person asked for a transcript to read or to paste, not for a replayable tool session.
//!   Their tool definitions do not exist on the other side, so a native block would name a tool
//!   that the receiving product cannot call anyway.
//!
//! A call and its result are therefore converted to **labelled text**, and each appears in the
//! transcript at the position the event occupies in the log:
//!
//! ```text
//! [tool call] read_file {"path":"a.txt"}
//! [tool result] read_file
//! the contents of a.txt
//! ```
//!
//! A failed result carries ` (failed)` after the name, and a call that never produced a result is
//! left as the `[tool call]` line alone — the log is not repaired.
//!
//! **The labels ride the `user` channel, deliberately.** A tool result is input to the model — the
//! same direction as a person's turn — and the two target protocols have exactly two participants,
//! so environment text has only one possible channel. That is the same compromise a person makes
//! when they paste a command's output into a chat window, with one difference: the `[tool …]` label
//! is what stops a reader, and a receiving model, from mistaking it for the person's own words.
//! Nothing from the harness is ever attributed to the assistant, and an assistant message contains
//! only text the model actually produced. A step that called tools but said nothing produces no
//! assistant message — the call is its own line, so the exchange is not lost, and an empty bubble is
//! not invented.
//!
//! # What is not exported, and why
//!
//! | event | why it is dropped |
//! |---|---|
//! | `turn/start`, `turn/end`, `step/start`, `step/end` | the scheduler's frame around the conversation; a reader wants the conversation |
//! | `model/usage` | accounting, not text; it is summed elsewhere, and no target format has a slot for it |
//! | `model/selection`, `permission/preset`, `plan/mode`, `sandbox/mode`, `request/context` | describe how this harness is configured, which the receiving product neither knows nor honours |
//! | `session/title`, `llm/retry` | metadata about the log, not something either party said |
//! | `developer/message` | this harness's own tool-catalogue changes riding a turn — bookkeeping, and exporting it would smuggle a tool description into the other product's system prompt |
//! | `reasoning` blocks | the model's private working; it was not said to anyone, and both protocols have a separate channel for it that a transcript must not forge |
//! | `tool-call` blocks inside a message | the same call is recorded as its own `tool/call` event, so reading both would print every call twice |
//!
//! Images and files become `[image attachment: …]` / `[file attachment: …]` labels: the bytes live
//! in this harness's attachment service and the handle means nothing to another product, so the
//! label is the honest maximum.
//!
//! # Why tool text is bounded
//!
//! One tool result can hold an entire file, and a transcript exists to be pasted into a window or
//! read by a person. Tool-derived text is therefore cut at [`MAX_TOOL_TEXT_CHARS`] characters and
//! says so with a visible marker. The cut is taken on a character boundary rather than a byte
//! offset, because half a UTF-8 sequence reads as corrupt data rather than as a truncation.
//! Conversational text is *not* bounded: what the person and the model said is the transcript, and
//! silently shortening it would be a different failure from the one this limits.
//!
//! # Why a re-recorded system prompt appears once
//!
//! The prompt is re-recorded every step (`SessionEvent::SystemMessage`), because it can change
//! between steps. Copying each record would fill the transcript with identical prompts, so a
//! record whose text equals the previous record is skipped and only a genuine *change* is exported.
//! `OpenAI` keeps a change where it happened, as a `system` message at that point in the array —
//! the receiving product accepts a system message mid-conversation, and the position is real
//! information. `Anthropic` cannot express it positionally, so the changed prompts are joined in log
//! order into the one top-level `system` string.
//!
//! # What this module does not do
//!
//! It does not *import*. Recognising an older version of this harness's own log is
//! `session_migration`'s job, and a `Cordis`-era session is the same log one generation older, not
//! a foreign format. Reading a `ChatGPT` or `Claude` export back in would need the reverse of every
//! decision above — and the tool calls, which this projection has already flattened — so nothing
//! here should be used as the basis of an importer without those losses being weighed again.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use girder_core::{ContentBlock, SessionEvent};

/// The role name both protocols give the person.
pub const ROLE_USER: &str = "user";

/// The role name both protocols give the model.
pub const ROLE_ASSISTANT: &str = "assistant";

/// The role name the neutral transcript gives the system prompt.
///
/// `OpenAI` spells it the same way; `Anthropic` has no such role at all, which is why [`to_json`]
/// lifts it out of `messages` for that format.
pub const ROLE_SYSTEM: &str = "system";

/// The most characters one tool-derived annotation may carry.
///
/// Four thousand characters is a screen or two of output: enough that a compiler error or a short
/// file reads whole, small enough that a result holding a whole repository still leaves a transcript
/// that can be pasted and read.
pub const MAX_TOOL_TEXT_CHARS: usize = 4000;

/// Appended to tool-derived text cut at [`MAX_TOOL_TEXT_CHARS`].
const TRUNCATION_MARKER: &str = "\n… [truncated]";

/// What a tool result says when it carried no text at all.
const NO_TOOL_TEXT: &str = "(no text output)";

/// A transcription format this module can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `OpenAI`'s chat `messages` shape: the system prompt is one more message.
    OpenAi,
    /// `Anthropic`'s Messages shape: the system prompt is a top-level field.
    Anthropic,
}

impl Format {
    /// The format's spelling, for a diagnostic or a field naming what it is.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

/// One message in a transcript.
///
/// `role` is a `String` rather than an enum because the transcript is a *neutral* intermediate: it
/// carries `system` for both formats and lets [`to_json`] decide what that means, and a caller that
/// builds one by hand for a role this module never emits is not refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// `"system"`, `"user"`, or `"assistant"` as written by [`export`].
    pub role: String,
    /// The message text, already flattened from content blocks and trimmed.
    pub content: String,
}

impl Message {
    /// Builds one message from a role and its text.
    #[must_use]
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

/// A whole session projected into another product's vocabulary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    /// The messages, in the order they were said.
    pub messages: Vec<Message>,
}

impl Transcript {
    /// How many messages the transcript holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the session produced no exportable message.
    ///
    /// A session that was created and never used is a normal state, and an empty transcript is its
    /// correct projection rather than an error.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Exports a session's events into `format`'s vocabulary.
///
/// The result is format-neutral in one respect worth stating: the system prompt is a `system`
/// message in both cases, because that is where the two formats agree and where a caller reading
/// the transcript expects to find it. Only [`to_json`] applies each protocol's placement.
///
/// `format` is not decorative. `OpenAI` can hold a `system` message at any position, so a prompt
/// that changed mid-session stays where it changed; `Anthropic` has no positional system channel,
/// so the changed prompts are joined into one leading message here instead. See the module docs.
#[must_use]
pub fn export(events: &[SessionEvent], format: Format) -> Transcript {
    let mut messages: Vec<Message> = Vec::new();
    // Call id to tool name, so a result can be labelled with the tool that produced it. A result on
    // its own does not say which tool ran, and a heading named after a call id is unreadable.
    let mut tool_names: BTreeMap<String, String> = BTreeMap::new();
    // The last prompt exported, so a per-step re-record of an unchanged prompt is skipped instead
    // of repeated once per step.
    let mut last_system: Option<String> = None;
    // `Anthropic`'s one top-level `system` string, assembled in log order.
    let mut system_head: Option<String> = None;

    for event in events {
        match event {
            SessionEvent::SystemMessage { message, .. } => {
                let text = blocks_text(&message.content);
                if text.is_empty() || last_system.as_deref() == Some(text.as_str()) {
                    continue;
                }
                match format {
                    Format::OpenAi => messages.push(Message::new(ROLE_SYSTEM, text.clone())),
                    Format::Anthropic => match &mut system_head {
                        Some(head) => {
                            head.push_str("\n\n");
                            head.push_str(&text);
                        }
                        None => system_head = Some(text.clone()),
                    },
                }
                last_system = Some(text);
            }
            SessionEvent::UserMessage { message } => {
                push(&mut messages, ROLE_USER, blocks_text(&message.content));
            }
            SessionEvent::AssistantMessage { message } => {
                push(&mut messages, ROLE_ASSISTANT, blocks_text(&message.content));
            }
            SessionEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                tool_names.insert(id.to_string(), name.clone());
                push(&mut messages, ROLE_USER, call_annotation(name, arguments));
            }
            SessionEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                let call = id.to_string();
                let name = tool_names.get(&call).map_or(call.as_str(), String::as_str);
                let body = blocks_text(content);
                push(
                    &mut messages,
                    ROLE_USER,
                    result_annotation(name, &body, *is_error),
                );
            }
            // Everything else is frame, bookkeeping, or configuration; see the module docs.
            _ => {}
        }
    }

    if let Some(head) = system_head {
        // First, because a protocol that has no positional system channel has exactly one, and it
        // describes the conversation rather than belonging to a point inside it.
        messages.insert(0, Message::new(ROLE_SYSTEM, head));
    }

    Transcript { messages }
}

/// Renders a transcript as the JSON body each protocol expects.
///
/// The shape is the `messages` portion of a request body, not a complete request: the fields a
/// request also needs — `model`, `max_tokens`, and the rest — are not determined by a session log,
/// and inventing a model id would be a claim about the export rather than about the session.
///
/// `OpenAI` gets `{"messages": [{"role": …, "content": …}, …]}` with the system prompt in place as
/// one of those messages. `Anthropic` gets `{"system": "…", "messages": […]}` with every `system`
/// message lifted out and joined in log order, and `system` absent entirely when the session had
/// no prompt — an empty string would be a prompt, and an empty prompt is not what happened.
///
/// Roles are copied through verbatim, so a transcript built by hand keeps whatever role it declared;
/// `content` is always a string, because every block this module keeps is text.
#[must_use]
pub fn to_json(transcript: &Transcript, format: Format) -> Value {
    match format {
        Format::OpenAi => {
            let messages: Vec<Value> = transcript.messages.iter().map(message_json).collect();
            json!({ "messages": messages })
        }
        Format::Anthropic => {
            let mut system: Vec<&str> = Vec::new();
            let mut messages: Vec<Value> = Vec::new();
            for message in &transcript.messages {
                if message.role == ROLE_SYSTEM {
                    if !message.content.is_empty() {
                        system.push(message.content.as_str());
                    }
                } else {
                    messages.push(message_json(message));
                }
            }
            let mut body = Map::new();
            if !system.is_empty() {
                body.insert("system".to_owned(), Value::String(system.join("\n\n")));
            }
            body.insert("messages".to_owned(), Value::Array(messages));
            Value::Object(body)
        }
    }
}

/// One message as the `{role, content}` object both protocols use.
fn message_json(message: &Message) -> Value {
    json!({ "role": message.role, "content": message.content })
}

/// Appends a message unless its text says nothing.
///
/// An assistant step that only requested a tool carries no text, and its calls are already their own
/// lines; pushing an empty message would add a blank bubble that a receiving product has to render
/// and a reader has to skip.
fn push(messages: &mut Vec<Message>, role: &str, text: String) {
    if text.is_empty() {
        return;
    }
    messages.push(Message::new(role, text));
}

/// Flattens content blocks into the text a reader sees.
///
/// Reasoning is the model's private working and an embedded tool call is already its own event:
/// neither is text either party read, so neither is included here.
fn blocks_text(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::Image { attachment } => {
                let _ = write!(out, "[image attachment: {attachment}]");
            }
            ContentBlock::File { attachment } => {
                let _ = write!(out, "[file attachment: {attachment}]");
            }
            ContentBlock::Reasoning { .. } | ContentBlock::ToolCall { .. } => {}
        }
    }
    out.trim().to_owned()
}

/// The labelled text one tool call becomes.
fn call_annotation(name: &str, arguments: &str) -> String {
    let arguments = bound(arguments.trim());
    if arguments.is_empty() {
        format!("[tool call] {name}")
    } else {
        format!("[tool call] {name} {arguments}")
    }
}

/// The labelled text one tool result becomes.
///
/// `(failed)` is appended to the name rather than put on a line of its own so that a reader scanning
/// for failures finds them by name, and so a result whose text is empty still says it failed.
fn result_annotation(name: &str, text: &str, is_error: bool) -> String {
    let status = if is_error { " (failed)" } else { "" };
    let body = if text.is_empty() {
        NO_TOOL_TEXT.to_owned()
    } else {
        bound(text)
    };
    format!("[tool result] {name}{status}\n{body}")
}

/// Cuts tool-derived text at [`MAX_TOOL_TEXT_CHARS`], on a character boundary.
///
/// The cut counts characters rather than bytes so that a multi-byte script is not split into an
/// invalid sequence, and so the limit means the same thing whatever the tool returned.
fn bound(text: &str) -> String {
    if text.chars().count() <= MAX_TOOL_TEXT_CHARS {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(MAX_TOOL_TEXT_CHARS).collect();
    cut.push_str(TRUNCATION_MARKER);
    cut
}
