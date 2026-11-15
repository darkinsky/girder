//! Importing a chat transcript from another product: the inverse of [`crate::session_transcript`].
//!
//! # The gap this fills
//!
//! The crate could already *write* another product's vocabulary — [`crate::session_transcript::export`]
//! and [`crate::session_transcript::to_json`] project a session into the `OpenAI` and `Anthropic`
//! `messages` shapes — and it could *read* a session back, but only from its own transport format:
//! [`crate::session_portable::import`] requires a bundle that names [`crate::session_portable::VERSION`]
//! and carries this harness's own event vocabulary. Neither answers the question a person actually
//! asks: *I have a `ChatGPT` conversation here, can I turn it into a session?* Before this module the
//! answer was no, and the only way to bring a foreign conversation in was to retype it.
//!
//! # Why this is not export run backwards
//!
//! The exporter is lossy on purpose — see its module docs — and the losses are precisely the parts an
//! importer would have to invent back:
//!
//! - a `tool/call` and its `tool/result` are flattened into `[tool call] …` / `[tool result] …`
//!   *text*. The call id, the tool name, and the raw argument `JSON` survive only as prose. Turning
//!   that prose back into a `tool/call` would mean guessing a provider-issued id, which this harness
//!   then hands to a provider that never issued it, and a `tool/result` with no call to answer is
//!   the shape [`crate::session_health`] calls an orphan and a replay cannot use.
//! - reasoning blocks are private working that was never sent to either side; they are not in the
//!   export at all, so nothing can bring them back.
//! - images and files are `[image attachment: …]` labels pointing at this harness's attachment
//!   service. The handles in a foreign transcript point at the *other* product's storage and mean
//!   nothing here.
//!
//! So the rule of this module is **import only what can be known**: the roles both protocols share,
//! the text they carry, and the position of each message in the conversation. Everything else is
//! dropped, and — because a silent drop is the worst failure a migration can have — it is counted
//! and described. [`Imported::skipped`] is the number a caller can show; [`Imported::warnings`]
//! says what was lost and why.
//!
//! # What a transcript becomes
//!
//! A session log is not a message list: it is turn and step frames around messages (see
//! [`crate::session`]). An imported conversation therefore has to be given a frame, and the frame
//! must be one the rest of the crate already accepts — [`crate::session_health::diagnose`] must
//! report nothing, [`crate::resume_point::find`] must land on a turn boundary, and a UI expanding
//! turns must not find one that never closed. The frame this module writes is:
//!
//! ```text
//! turn/start N
//!   [step/start N 1 · developer/message · step/end N 1]   ← the source's system prompt, turn 1 only
//!   user/message                                          ← each person turn, as it sits in the source
//!   step/start N k · assistant/message · step/end N k      ← one step per model reply
//! turn/end N (completed)
//! ```
//!
//! Three decisions are worth stating:
//!
//! - **a turn begins at a user message.** That is where one exchange ends and the next begins in
//!   every chat transcript. Messages before the first user message — a source that opens with an
//!   assistant greeting is common — belong to turn 1, because a turn with no prompt is still a turn
//!   and inventing an empty user turn in front of it would add a message nobody wrote.
//! - **each model reply is its own step.** A step is "one model call plus the tools it requested"
//!   ([`crate::session::SessionEvent::StepStart`]); a transcript is exactly a list of model calls,
//!   so one reply per step is the faithful reading, and it never claims a call that produced no
//!   reply.
//! - **a person's message rides the turn, not a step.** A transcript gives no evidence of *which*
//!   step a prompt entered at, and `step/start` would claim a model call that may never have
//!   happened.
//!
//! Every turn is closed with [`crate::session::TurnEndReason::Completed`]. Nothing in an import
//! failed, was blocked, or ran out of budget; the source transcript simply ends. Leaving the last
//! turn open would make an imported session look like one that is still running.
//!
//! # The system prompt becomes an advisory developer message
//!
//! Both protocols carry a system prompt and neither agrees on where: `OpenAI` writes it as one more
//! message, `Anthropic` as a top-level `system` field that is not a message at all. This module
//! reads both and puts the text in **one [`crate::session::SessionEvent::DeveloperMessage`] at the
//! start of turn 1**.
//!
//! The obvious alternative — [`crate::session::SessionEvent::SystemMessage`] — is wrong, and the
//! reason is provenance rather than shape. That variant is documented as *the rendered system
//! prompt on the model-visible surface*, recorded by the loop once per step because the prompt it
//! assembles can change. A foreign product's prompt is not that: this harness never assembled it,
//! never rendered it against a workspace, and its next turn will append its own record beside it. A
//! replay that read the imported text as this harness's prompt would explain the wrong setup, and a
//! [`crate::session_transcript::export`] of the resumed session would carry two prompts of different
//! origins with nothing to tell them apart.
//!
//! `developer/message` is the honest slot. It is a message the model sees that is neither the prompt
//! nor the person speaking — exactly what foreign instructions are once they ride into this
//! harness — and every reader that renders a log (Markdown export, the static HTML view, the digest)
//! already shows it. The cost is stated, not hidden: a re-export does not turn it back into a
//! `system` message, because [`crate::session_transcript::export`] deliberately drops developer
//! messages, and the warning this module emits says so.
//!
//! Repeated system records are joined in log order with a blank line, which is the same rule the
//! `Anthropic` renderer applies when it has only one slot. When the transcript holds no messages at
//! all there is no turn to attach the prompt to; it is dropped, and the warning says that rather
//! than leaving the caller to notice.
//!
//! # Detecting the format
//!
//! [`detect`] answers from structure, never from a file name: `OpenAI` bodies hold a top-level
//! `messages` array, `Anthropic` bodies hold `system` *plus* `messages`. The two shapes overlap —
//! an `Anthropic` body without a system prompt is, field for field, an `OpenAI` body — so detection
//! is layered and each layer is decisive on its own:
//!
//! 1. a top-level `system`, `anthropic_version`, `top_k`, or `stop_sequences` is `Anthropic`: the
//!    chat-completions schema has none of them;
//! 2. a top-level `functions`, `function_call`, `logit_bias`, `response_format`, or `seed` is
//!    `OpenAI`;
//! 3. a message carrying `tool_calls`, `tool_call_id`, or `function_call`, a `tool`/`function`/
//!    `system`/`developer` role, a `tool_use`/`tool_result`/`thinking`/`document` block, or an
//!    `image_url`/`input_audio`/`refusal` block marks the protocol that writes it;
//! 4. with no marker at all the value is reported as `OpenAI`, because that is the shape
//!    [`crate::session_transcript::to_json`] writes and the one an unlabelled paste most likely is.
//!
//! Only the first three layers are *decisive*. [`import`] refuses a value whose decisive marker
//! names the other format, and accepts the marker-less overlap for either — otherwise exporting an
//! `Anthropic` body for a session with no prompt would produce something this module refused to read
//! back, which would make the round trip depend on the session having a prompt.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

// Named through `girder_core` rather than `crate::` for the same reason `session_transcript` does:
// these are re-exports of the crate root, and spelling them this way keeps the file compilable both
// as a module of the library and, by `tests/transcript_import.rs`, as a module of the test crate.
use girder_core::{
    ChatMessage, ChatRole, ContentBlock, MessageId, MessageSource, SessionEvent, TurnEndReason,
};

// `crate::` rather than `girder_core::`: this is the one path the test crate has to include by
// `#[path]`, and it resolves to the same module either way.
use crate::session_transcript::{Format, ROLE_ASSISTANT, ROLE_SYSTEM, ROLE_USER};

/// The role name `OpenAI` uses for the system prompt on its `o1`-era models.
///
/// Spelled here rather than in `session_transcript` because the *exporter* never writes it: it is
/// an input-only spelling, and a transcript this harness produces must not contain it.
const ROLE_DEVELOPER: &str = "developer";

/// The provider and model recorded on an imported assistant message.
///
/// A transcript carries no model id: [`crate::session_transcript::to_json`] emits only `role` and
/// `content`, and inventing `deepseek-chat` or `gpt-4o` would attribute the reply to a model the
/// source may never have used. The message model still requires a provider — an assistant message
/// whose source is not a model is inconsistent by [`crate::message::Message::is_consistent`] — so
/// this constant is the placeholder. It is a word rather than an empty string because an empty
/// provider reads as a decoding failure, and this value is the truth: the reply was imported.
pub const IMPORTED_PROVIDER: &str = "imported";

/// The prefix of every message id this module allocates.
///
/// Ids are positional (`imported-0`, `imported-1`, …) and derived from a counter rather than from
/// the source's ids: a transcript has none, and hashing the text would make two identical messages
/// collide in a log where they are two events.
const MESSAGE_ID_PREFIX: &str = "imported-";

/// Top-level fields only the `Anthropic` Messages API writes.
const ANTHROPIC_TOP_LEVEL: &[&str] = &["system", "anthropic_version", "top_k", "stop_sequences"];

/// Top-level fields only the `OpenAI` chat-completions API writes.
const OPENAI_TOP_LEVEL: &[&str] = &[
    "functions",
    "function_call",
    "logit_bias",
    "response_format",
    "seed",
];

/// Message roles only `OpenAI` writes.
///
/// `system` is in this list because the `Anthropic` API has no such role *inside* `messages`: its
/// prompt lives in the top-level field, which layer 1 already caught.
const OPENAI_ONLY_ROLES: &[&str] = &["tool", "function", "system", "developer"];

/// Content-block types only the `Anthropic` Messages API writes.
const ANTHROPIC_BLOCK_KINDS: &[&str] = &[
    "tool_use",
    "tool_result",
    "thinking",
    "redacted_thinking",
    "document",
    "server_tool_use",
];

/// Content-part types only `OpenAI` writes.
const OPENAI_BLOCK_KINDS: &[&str] = &["image_url", "input_audio", "refusal"];

/// Message-level fields that carry `OpenAI` tool traffic.
const OPENAI_CALL_FIELDS: &[&str] = &["tool_calls", "function_call"];

/// The result of importing one transcript.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Imported {
    /// The events, in conversation order, framed as turns and steps.
    ///
    /// Ready to append to a session log: every turn is closed and every step is paired, so
    /// [`crate::session_health::diagnose`] reports nothing and [`crate::resume_point::find`] lands
    /// on the final boundary.
    pub events: Vec<SessionEvent>,

    /// How many messages were **not** imported.
    ///
    /// Two things land here: a role this importer does not handle (a `tool` or `function` message,
    /// which is `OpenAI`'s native tool channel and has no event to become), and a message whose
    /// content held no text at all (an image-only turn, or one whose only block was a tool call).
    /// Both are counted rather than silently dropped, because a caller migrating a conversation
    /// needs to know whether what arrived is the whole of what was sent.
    pub skipped: usize,

    /// What was lost or translated, in a form a caller can show a person.
    ///
    /// Non-empty does not mean the import failed — a transcript with a system prompt always
    /// produces one warning, because that prompt is translated into an advisory message rather than
    /// into this harness's prompt, and a re-export will not turn it back into one.
    pub warnings: Vec<String>,
}

/// Why a transcript could not be imported.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImportError {
    /// The value carries the *other* format's decisive marker.
    ///
    /// Separate from [`Self::Malformed`] because the two ask for different fixes: this one is a
    /// caller that named the wrong format, and the value is fine.
    #[error("the value is not in this transcript format: {detail}")]
    NotThisFormat {
        /// Which shape was seen, and which was asked for.
        detail: String,
    },

    /// The value is not a chat payload, or one message in it is not shaped like a message.
    #[error("the transcript is not structured as a chat payload: {detail}")]
    Malformed {
        /// The field or message position that could not be read.
        detail: String,
    },
}

/// Detects which transcript format a JSON value is.
///
/// Returns `None` when the value is not a chat payload at all: not an object, no `messages` field,
/// or a `messages` field that is not an array. Those are structural facts, and a format guess about
/// a value that has no conversation in it would only be used to produce a confusing error later.
///
/// The overlap between the two shapes is documented in the module docs. A value with no decisive
/// marker is reported as [`Format::OpenAi`], which is the one case where this function's answer is
/// a default rather than a deduction; a caller that knows where its transcript came from should
/// pass the format to [`import`] instead of asking.
#[must_use]
pub fn detect(value: &Value) -> Option<Format> {
    let object = value.as_object()?;
    let messages = object.get("messages")?.as_array()?;
    Some(decisive_format(object, messages).unwrap_or(Format::OpenAi))
}

/// Imports a transcript in `format` into session events.
///
/// Only user and assistant messages are imported, because those are the two roles both protocols
/// share. The source's system prompt becomes one advisory `developer/message` at the start of turn
/// 1; see the module docs for why it is not a [`crate::session::SessionEvent::SystemMessage`].
///
/// A message-level `role` other than `user`, `assistant`, `system`, or `developer` is skipped and
/// counted in [`Imported::skipped`], and a content block that is not text is dropped and named in
/// [`Imported::warnings`]. Nothing is silently reconstructed: a `tool_calls` array is counted as
/// dropped because turning it back into a [`crate::session::SessionEvent::ToolCall`] would require
/// inventing the provider-issued id that a replay and a provider both depend on.
///
/// # Errors
///
/// - [`ImportError::NotThisFormat`] when the value carries a decisive marker of the other format —
///   a top-level `system` field offered as `OpenAI`, or a `tool`-role message offered as `Anthropic`.
/// - [`ImportError::Malformed`] when there is no `messages` array, or a message is not an object
///   with a string `role`, or a `content` value is neither a string, an array, nor null.
pub fn import(value: &Value, format: Format) -> Result<Imported, ImportError> {
    let object = value
        .as_object()
        .ok_or_else(|| ImportError::Malformed {
            detail: "a transcript is a JSON object holding a `messages` array".to_owned(),
        })?;
    let messages = object
        .get("messages")
        .ok_or_else(|| ImportError::Malformed {
            detail: "the value has no `messages` field".to_owned(),
        })?
        .as_array()
        .ok_or_else(|| ImportError::Malformed {
            detail: "`messages` is not an array".to_owned(),
        })?;

    if let Some(found) = decisive_format(object, messages) {
        if found != format {
            return Err(ImportError::NotThisFormat {
                detail: format!(
                    "the value is shaped as the {} format, which was not the {} format requested",
                    found.as_str(),
                    format.as_str()
                ),
            });
        }
    }

    let mut importer = Importer::default();

    // The `Anthropic` placement. `OpenAI` cannot reach here with one — a top-level `system` is
    // layer 1 of detection, so it was either refused above or is the format asked for.
    if let Some(system) = object.get("system") {
        let content = read_content(system)?;
        importer.absorb(content.dropped);
        importer.advisory(content.text);
    }

    for (index, message) in messages.iter().enumerate() {
        importer.adopt(index, message)?;
    }

    importer.finish()
}

/// Which protocol the value's decisive markers name, if any.
///
/// Layers are tried in a fixed order and the first hit wins; see the module docs for why that order
/// is `Anthropic`'s top-level fields first.
fn decisive_format(object: &Map<String, Value>, messages: &[Value]) -> Option<Format> {
    if ANTHROPIC_TOP_LEVEL
        .iter()
        .any(|key| object.contains_key(*key))
    {
        return Some(Format::Anthropic);
    }
    if OPENAI_TOP_LEVEL
        .iter()
        .any(|key| object.contains_key(*key))
    {
        return Some(Format::OpenAi);
    }

    for message in messages {
        // A non-object entry proves nothing about the format; `import` reports it as malformed with
        // its position, which is a better error than "I cannot tell what this is".
        let Some(message) = message.as_object() else {
            continue;
        };
        if OPENAI_CALL_FIELDS
            .iter()
            .any(|field| message.contains_key(*field))
        {
            return Some(Format::OpenAi);
        }
        if message
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|role| OPENAI_ONLY_ROLES.contains(&role))
        {
            return Some(Format::OpenAi);
        }
        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(kind) = block.get("type").and_then(Value::as_str) else {
                continue;
            };
            if ANTHROPIC_BLOCK_KINDS.contains(&kind) {
                return Some(Format::Anthropic);
            }
            if OPENAI_BLOCK_KINDS.contains(&kind) {
                return Some(Format::OpenAi);
            }
        }
    }

    None
}

/// One message that survived role filtering, in source order.
struct Inbound {
    /// Who said it. Only the two roles both protocols share can appear here.
    role: Speaker,
    /// Its text, already joined from content blocks and trimmed.
    text: String,
}

/// The two speakers a foreign transcript can contribute.
///
/// An enum rather than [`ChatRole`] because the frame builder must not have an unreachable arm: a
/// message with any other role never becomes an `Inbound`, so the match that turns one into an
/// event is exhaustive without a catch-all that would silently drop data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Speaker {
    /// The person.
    User,
    /// The model.
    Assistant,
}

/// The text of one `content` value, plus an inventory of what could not be kept.
struct Content {
    /// The joined text. Empty when every block was dropped.
    text: String,
    /// Dropped block kinds and how many of each, keyed by the type name for the warning.
    dropped: BTreeMap<String, usize>,
}

/// Reads a `content` value that is a string, an array of blocks, or null.
///
/// A string is a whole message; an array is a list of typed blocks, of which only the text-typed
/// ones (or, for exporters that omit the tag, the ones carrying a `text` field) become text. `null`
/// is accepted because `OpenAI` writes it on an assistant message whose whole output was a tool
/// call — the call is what is dropped there, not the message.
fn read_content(value: &Value) -> Result<Content, ImportError> {
    let mut content = Content {
        text: String::new(),
        dropped: BTreeMap::new(),
    };

    match value {
        Value::String(text) => content.text = text.trim().to_owned(),
        Value::Array(blocks) => {
            for block in blocks {
                match block {
                    // A bare string inside the array is legal in no protocol, but exporters that
                    // wrap a single string do exist. Reading it as text keeps a real message.
                    Value::String(text) => push_text(&mut content.text, text),
                    Value::Object(object) => {
                        let kind = object.get("type").and_then(Value::as_str);
                        match (kind, object.get("text").and_then(Value::as_str)) {
                            (None | Some("text"), Some(text)) => push_text(&mut content.text, text),
                            _ => {
                                let name = kind.unwrap_or("untyped").to_owned();
                                *content.dropped.entry(name).or_insert(0) += 1;
                            }
                        }
                    }
                    _ => *content.dropped.entry("untyped".to_owned()).or_insert(0) += 1,
                }
            }
            content.text = content.text.trim().to_owned();
        }
        // No text of its own. What it carried is counted by the caller from the message fields.
        Value::Null => {}
        _ => {
            return Err(ImportError::Malformed {
                detail: "a `content` value must be a string, an array of blocks, or null".to_owned(),
            });
        }
    }

    Ok(content)
}

/// Appends one text block, separating blocks with a newline.
///
/// A session message is one text block, so several source blocks have to become one string. A
/// newline is the cheapest separator that never welds two paragraphs into one word; it is not a
/// claim that the source had a line break there.
fn push_text(target: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(text);
}

/// What one import has collected so far.
///
/// A struct rather than a bag of locals in [`import`] because five separate facts accumulate across
/// messages — the inbound texts, the prompt, and three kinds of loss — and threading them through
/// each call site is how one of them ends up forgotten.
#[derive(Default)]
struct Importer {
    /// The user and assistant messages, in source order.
    inbound: Vec<Inbound>,
    /// The source's system prompt texts, in source order.
    advisory: Vec<String>,
    /// Messages that were not imported.
    skipped: usize,
    /// Skipped roles and how many of each, for the warning.
    skipped_roles: BTreeMap<String, usize>,
    /// Dropped non-text block kinds and how many of each.
    dropped_blocks: BTreeMap<String, usize>,
    /// Messages that had no text to import.
    empty: usize,
}

impl Importer {
    /// Records one system-prompt text unless it says nothing.
    fn advisory(&mut self, text: String) {
        if !text.is_empty() {
            self.advisory.push(text);
        }
    }

    /// Merges one block inventory into the running one.
    fn absorb(&mut self, dropped: BTreeMap<String, usize>) {
        for (kind, count) in dropped {
            *self.dropped_blocks.entry(kind).or_insert(0) += count;
        }
    }

    /// Counts the `OpenAI` tool traffic a message carries.
    ///
    /// Counted rather than imported: see the module docs. A `tool_calls` entry is one dropped item
    /// because each call is one thing that did not survive, and reporting the array as a single
    /// loss would understate a turn that called three tools.
    fn count_calls(&mut self, object: &Map<String, Value>) {
        for field in OPENAI_CALL_FIELDS {
            match object.get(*field) {
                Some(Value::Array(calls)) if !calls.is_empty() => {
                    *self.dropped_blocks.entry((*field).to_owned()).or_insert(0) += calls.len();
                }
                Some(Value::Null) | None => {}
                // A malformed single object is still one thing that was there and is not imported.
                Some(_) => *self.dropped_blocks.entry((*field).to_owned()).or_insert(0) += 1,
            }
        }
    }

    /// Reads message `index` and files it under its role.
    fn adopt(&mut self, index: usize, value: &Value) -> Result<(), ImportError> {
        let object = value.as_object().ok_or_else(|| ImportError::Malformed {
            detail: format!("message {index} is not a JSON object"),
        })?;
        let role = object
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| ImportError::Malformed {
                detail: format!("message {index} has no string `role`"),
            })?
            .to_owned();

        self.count_calls(object);

        let content = match object.get("content") {
            Some(value) => read_content(value)?,
            // Absent content is the same loss of information as `null`: the message says something
            // with fields other than text, and whatever it said is counted from those fields.
            None => Content {
                text: String::new(),
                dropped: BTreeMap::new(),
            },
        };
        self.absorb(content.dropped);
        let text = content.text;

        match role.as_str() {
            ROLE_USER => self.push(Speaker::User, text),
            ROLE_ASSISTANT => self.push(Speaker::Assistant, text),
            // Both spellings of "instructions that are not the person and not the model". Neither
            // is this harness's prompt, so both become the one advisory message.
            ROLE_SYSTEM | ROLE_DEVELOPER => self.advisory(text),
            _ => {
                self.skipped += 1;
                *self.skipped_roles.entry(role).or_insert(0) += 1;
            }
        }

        Ok(())
    }

    /// Files one conversational message, or counts it as unimportable when it says nothing.
    fn push(&mut self, role: Speaker, text: String) {
        if text.is_empty() {
            // A message whose only blocks were dropped lands here as well as in `dropped_blocks`.
            // That is deliberate: the caller sees one lost block *and* one lost message, which is
            // what happened, rather than a message silently vanishing from the count.
            self.empty += 1;
            return;
        }
        self.inbound.push(Inbound { role, text });
    }

    /// Builds the events and the warning list.
    fn finish(self) -> Result<Imported, ImportError> {
        // The warning about the prompt depends on whether a turn exists for it, and a turn exists
        // exactly when at least one message survived the role and text filters.
        let any_turn = !self.inbound.is_empty();
        let events = assemble(&self.inbound, &self.advisory)?;
        let warnings = self.warnings(any_turn);

        Ok(Imported {
            events,
            skipped: self.skipped,
            warnings,
        })
    }

    /// Describes every loss this import recorded.
    ///
    /// Fixed order rather than push order, because the list is read by a person and a stable order
    /// is what makes two imports of the same transcript comparable in a diff.
    fn warnings(&self, any_turn: bool) -> Vec<String> {
        let mut warnings = Vec::new();

        if !self.advisory.is_empty() {
            if any_turn {
                warnings.push(
                    "the source's system prompt was imported as an advisory developer message at \
                     the start of turn 1; it is not this harness's system prompt, and a re-export \
                     does not turn it back into one"
                        .to_owned(),
                );
            } else {
                warnings.push(
                    "the source's system prompt was dropped: the transcript holds no messages, so \
                     there is no turn to attach it to"
                        .to_owned(),
                );
            }
        }

        if let Some(total) = sum(&self.dropped_blocks) {
            warnings.push(format!(
                "{total} content item(s) that were not text were not imported: {}",
                describe(&self.dropped_blocks)
            ));
        }

        if self.skipped > 0 {
            warnings.push(format!(
                "{} message(s) with a role this importer does not handle were skipped: {}",
                self.skipped,
                describe(&self.skipped_roles)
            ));
        }

        if self.empty > 0 {
            warnings.push(format!(
                "{} message(s) carried no text and were skipped",
                self.empty
            ));
        }

        warnings
    }
}

/// The total of an inventory, or `None` when it is empty.
///
/// `Option` rather than `0` so the caller's condition is "anything was dropped" instead of "the sum
/// is non-zero", which would be the same test written twice.
fn sum(counts: &BTreeMap<String, usize>) -> Option<usize> {
    if counts.is_empty() {
        return None;
    }
    Some(counts.values().copied().sum())
}

/// Renders an inventory as `name ×count, …`.
fn describe(counts: &BTreeMap<String, usize>) -> String {
    counts
        .iter()
        .map(|(name, count)| format!("{name} ×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Turns the collected messages into turn and step frames.
///
/// Returns `Ok` with no events for an empty conversation: a transcript with no messages is a valid
/// empty transcript, not an error, and the same is true of an empty session this harness exports.
fn assemble(inbound: &[Inbound], advisory: &[String]) -> Result<Vec<SessionEvent>, ImportError> {
    let prompt = if advisory.is_empty() {
        None
    } else {
        // Joined in source order with a blank line, matching the `Anthropic` renderer's rule for
        // the one slot it has. A prompt that changed mid-conversation cannot be placed positionally
        // in a single advisory message, and the log order is the honest reading of it.
        Some(advisory.join("\n\n"))
    };

    // Grouped before framing rather than while framing, because a turn boundary depends on what the
    // turn already holds: a user message opens a turn only when something precedes it.
    let mut groups: Vec<Vec<&Inbound>> = Vec::new();
    for item in inbound {
        let opens = groups.is_empty()
            || (item.role == Speaker::User && groups.last().is_some_and(|group| !group.is_empty()));
        if opens {
            groups.push(Vec::new());
        }
        if let Some(group) = groups.last_mut() {
            group.push(item);
        }
    }

    let mut events = Vec::with_capacity(groups.len().saturating_mul(4));
    let mut ids: u64 = 0;
    let mut turn: u64 = 0;

    for group in &groups {
        turn += 1;
        events.push(SessionEvent::TurnStart { turn });

        let mut step: u64 = 0;
        if turn == 1 {
            if let Some(text) = prompt.as_deref() {
                step += 1;
                events.push(SessionEvent::StepStart { turn, step });
                events.push(SessionEvent::DeveloperMessage {
                    turn,
                    step,
                    message: build_message(
                        &mut ids,
                        ChatRole::Developer,
                        MessageSource::User { form: None },
                        text,
                    )?,
                });
                events.push(SessionEvent::StepEnd { turn, step });
            }
        }

        for item in group {
            match item.role {
                Speaker::User => events.push(SessionEvent::UserMessage {
                    message: build_message(
                        &mut ids,
                        ChatRole::User,
                        MessageSource::User { form: None },
                        &item.text,
                    )?,
                }),
                Speaker::Assistant => {
                    step += 1;
                    events.push(SessionEvent::StepStart { turn, step });
                    events.push(SessionEvent::AssistantMessage {
                        message: build_message(
                            &mut ids,
                            ChatRole::Assistant,
                            MessageSource::Model {
                                provider: IMPORTED_PROVIDER.to_owned(),
                                model: IMPORTED_PROVIDER.to_owned(),
                                replay_state: None,
                            },
                            &item.text,
                        )?,
                    });
                    events.push(SessionEvent::StepEnd { turn, step });
                }
            }
        }

        events.push(SessionEvent::TurnEnd {
            turn,
            reason: TurnEndReason::Completed,
        });
    }

    Ok(events)
}

/// Builds one message with the next positional id.
///
/// The id is generated here rather than passed in so that no caller can reuse one: the sequence is
/// the counter, and a duplicated id would make two events indistinguishable to a replay.
fn build_message(
    ids: &mut u64,
    role: ChatRole,
    source: MessageSource,
    text: &str,
) -> Result<ChatMessage, ImportError> {
    let id = MessageId::new(format!("{MESSAGE_ID_PREFIX}{ids}")).map_err(|error| {
        // Unreachable for the ids this module builds — digits and a hyphen are all any brand
        // accepts — but reported rather than unwrapped: a panic in an importer loses the whole
        // transcript, and this detail tells a reader exactly which invariant broke.
        ImportError::Malformed {
            detail: format!("could not allocate an id for an imported message: {error}"),
        }
    })?;
    *ids += 1;

    Ok(ChatMessage::new(
        id,
        role,
        source,
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    ))
}
