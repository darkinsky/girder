//! The `session.export` Remote method: one session rendered as portable text.
//!
//! Two formats, because the two consumers differ. Markdown is what a person pastes into a document
//! or a ticket; JSON is what another program reads back and replays. A caller that wanted the second
//! should not have to undo the first, so the format is a request field rather than a second method.
//!
//! # Why the response is bounded
//!
//! A session log has no natural size: one tool result may hold an entire file. A response carrying
//! it whole would be a memory problem for the server and a hang for the client, so the export stops
//! at [`MAX_EXPORT_BYTES`] and says that it did. The flag is reported rather than inferred from the
//! length: a client that received exactly the limit cannot otherwise tell a complete transcript from
//! a cut-off one.
//!
//! The cut lands on a character boundary, never at a raw byte offset. A transcript exists to be
//! read, and half a UTF-8 sequence reads as corrupt log data rather than as a truncation — the one
//! reading a caller is least likely to suspect.
//!
//! # Why an unknown format is an error
//!
//! Falling back to the default would answer a request the caller did not make, and a client that
//! sent a misspelled format would never learn it. The same holds for an unknown session: an empty
//! transcript would claim a real session-held nothing, while the truth is that no such session
//! exists here.
//!
//! # What Markdown leaves out
//!
//! System prompts, model selection, usage, retries, and sandbox settings describe how a turn ran,
//! not what was said. They are omitted because several are re-recorded once per step and would
//! drown the conversation; the JSON form keeps them for a caller that wants them. Reasoning blocks
//! are omitted from both readable bodies for the same reason: they are the model's private working,
//! not something it said.
//!
//! Tool calls and results are paired by call id. A result on its own does not say which tool
//! produced it, and a heading named after a call id is unreadable, so the call's name is kept until
//! its result arrives.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use girder_core::{
    ChatMessage, ContentBlock, Declaration, RemoteCall, RemoteError, RemoteRegistry,
    SessionCatalog, SessionEvent,
};

/// The namespace `session.export` lives under.
pub const NAMESPACE: &str = "session";

/// The method name within [`NAMESPACE`], spelled as upstream spells it.
pub const METHOD: &str = "export";

/// The format used when the client does not name one.
///
/// Upstream's copy-to-clipboard action exports Markdown, so that is the useful default: a caller
/// that only wants a transcript should not have to choose.
pub const DEFAULT_FORMAT: &str = "markdown";

/// The largest response this method produces, in bytes.
///
/// One mebibyte is a long conversation and a small tool result at once: large enough that ordinary
/// sessions export whole, small enough that the response fits a single frame a client holds in
/// memory next to everything else it is rendering.
pub const MAX_EXPORT_BYTES: usize = 1024 * 1024;

/// A response format this method knows how to produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// A readable transcript.
    Markdown,
    /// The events themselves, one JSON array.
    Json,
}

impl Format {
    /// Maps the wire spelling onto a format, or `None` when it is not one this build produces.
    fn from_wire(name: &str) -> Option<Self> {
        match name {
            DEFAULT_FORMAT => Some(Self::Markdown),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    /// The spelling this format reports back, so a client sees which one it actually got.
    fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => DEFAULT_FORMAT,
            Self::Json => "json",
        }
    }
}

/// The arguments of `session.export`.
///
/// Extra fields are deliberately not refused: upstream's download route carries routing options
/// such as `includeDescendants`, and a client written against it would send them. A field this
/// build has no descendants to apply is ignored rather than made fatal.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportRequest {
    /// Which session to export.
    session_id: String,
    /// `markdown` or `json`; absent or `null` means [`DEFAULT_FORMAT`].
    #[serde(default)]
    format: Option<String>,
}

/// Registers `session.export`.
///
/// The catalog is a registration argument, like every other `register_*` function: the deployment
/// owns its session storage, and a module that reached for global state would answer for a
/// different set of sessions than the rest of the methods.
///
/// The method declares a read of `session`: exporting changes nothing, and a read-only mirror must
/// stay able to serve it.
pub fn register_session_export_method(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
) {
    registry.declare(NAMESPACE, METHOD, Declaration::read(NAMESPACE));
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        Box::pin(async move { export(&call.args, catalog.as_ref()).await })
    });
}

/// Runs one `session.export` call.
async fn export(args: &Value, catalog: &dyn SessionCatalog) -> Result<Value, RemoteError> {
    let request: ExportRequest =
        serde_json::from_value(args.clone()).map_err(|error| invalid(error.to_string()))?;

    let session_id = request.session_id.trim();
    if session_id.is_empty() {
        return Err(invalid("`sessionId` is required".to_owned()));
    }

    // The default is resolved through the same parser as a client-supplied name, so the spelled
    // default cannot drift from the spellings the parser accepts.
    let requested = request.format.as_deref().unwrap_or(DEFAULT_FORMAT).trim();
    let format = Format::from_wire(requested).ok_or_else(|| {
        invalid(format!(
            "`format` must be `{DEFAULT_FORMAT}` or `json`; got `{requested}`"
        ))
    })?;

    let Some(record) = catalog.read(session_id).await else {
        // `RemoteError` has no not-found variant, so this is `Failed` with the id in the message: a
        // client can still show which session was missing, and reusing `UnknownMethod` would send a
        // reader looking for a registry bug instead of a deleted session.
        return Err(RemoteError::Failed(format!(
            "no session `{session_id}` is known to this deployment"
        )));
    };

    let body = match format {
        Format::Markdown => render_markdown(&record.id, &record.events),
        Format::Json => render_json(&record.events)?,
    };

    Ok(capped_response(format, body))
}

/// Builds an `INVALID_ARGUMENTS` error for this method.
fn invalid(message: String) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: METHOD.to_owned(),
        message,
    }
}

/// Renders the events as a Markdown transcript.
///
/// Boundary events become `## Turn N` headings and nothing else; `step/start`, `step/end`,
/// `model/usage`, and the settings records produce no output, because a reader wants the
/// conversation rather than the scheduler's bookkeeping.
fn render_markdown(id: &str, events: &[SessionEvent]) -> String {
    let mut out = format!("# Session {id}\n");
    // Call id to tool name, so a result can be headed with the tool that produced it.
    let mut tool_names: BTreeMap<String, String> = BTreeMap::new();
    // Calls that never produced a result, with their raw arguments. Kept in log order so a
    // crashed turn still shows what the model asked for last.
    let mut unanswered: Vec<(String, String)> = Vec::new();

    for event in events {
        match event {
            SessionEvent::TurnStart { turn } => {
                section(&mut out, &format!("## Turn {turn}"), "");
            }
            SessionEvent::UserMessage { message } => message_section(&mut out, "User", message),
            SessionEvent::AssistantMessage { message } => {
                message_section(&mut out, "Assistant", message);
            }
            SessionEvent::DeveloperMessage { message, .. } => {
                message_section(&mut out, "Developer", message);
            }
            SessionEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                let call = id.to_string();
                tool_names.insert(call.clone(), name.clone());
                unanswered.push((call, arguments.clone()));
            }
            SessionEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                let call = id.to_string();
                unanswered.retain(|(pending, _)| pending != &call);
                let name = tool_names.get(&call).map_or(call.as_str(), String::as_str);
                let output = blocks_text(content);
                let body = if *is_error {
                    format!("_(failed)_\n\n{output}")
                } else {
                    output
                };
                section(&mut out, &format!("### Tool: {name}"), &body);
            }
            // Everything else is bookkeeping a reader does not need; see the module docs.
            _ => {}
        }
    }

    for (call, arguments) in unanswered {
        let name = tool_names.get(&call).map_or(call.as_str(), String::as_str);
        let body = if arguments.trim().is_empty() {
            "_(no result recorded)_".to_owned()
        } else {
            format!(
                "_(no result recorded)_\n\n```json\n{}\n```",
                arguments.trim()
            )
        };
        section(&mut out, &format!("### Tool: {name}"), &body);
    }

    out
}

/// Writes a message section, unless the message has no visible text.
///
/// An assistant step that only requested tools carries no text; its calls are recorded as their own
/// events, so an empty `### Assistant` heading would be noise rather than information.
fn message_section(out: &mut String, title: &str, message: &ChatMessage) {
    let text = message.text();
    if text.trim().is_empty() {
        return;
    }
    section(out, &format!("### {title}"), &text);
}

/// Appends one heading and its body, separated from what came before by a blank line.
fn section(out: &mut String, title: &str, body: &str) {
    let _ = write!(out, "\n{title}\n");
    let body = body.trim_end();
    if !body.trim().is_empty() {
        out.push_str(body);
        out.push('\n');
    }
}

/// Flattens content blocks into the text a reader sees.
///
/// Images and files are rendered as labels rather than bytes: the bytes live in the attachment
/// service, and a transcript that inlined them would be neither portable nor readable.
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
            // Reasoning is the model's private working, and an embedded tool call is already its
            // own event; neither belongs in the visible output twice.
            ContentBlock::Reasoning { .. } | ContentBlock::ToolCall { .. } => {}
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "_(no text output)_".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Renders the events as a JSON array.
///
/// The array is the log's own serialization, not a projection: a caller that round-trips it gets
/// back values equal to the events, which is what makes an exported session replayable.
fn render_json(events: &[SessionEvent]) -> Result<String, RemoteError> {
    serde_json::to_string(events).map_err(|error| {
        RemoteError::Failed(format!("the session could not be encoded as JSON: {error}"))
    })
}

/// Shapes the response, applying the size budget.
fn capped_response(format: Format, body: String) -> Value {
    let (content, truncated) = truncate(body);
    let bytes = content.len();
    json!({
        "format": format.as_str(),
        "content": content,
        // The length of what is actually returned, so a client sizing a buffer is not told the
        // size of the body that was cut.
        "bytes": bytes,
        "truncated": truncated,
    })
}

/// Cuts `content` to [`MAX_EXPORT_BYTES`] on a character boundary.
///
/// Returns whether it cut anything. The walk back from the limit costs at most three bytes, since
/// no UTF-8 sequence is longer than four.
fn truncate(mut content: String) -> (String, bool) {
    if content.len() <= MAX_EXPORT_BYTES {
        return (content, false);
    }
    let mut cut = MAX_EXPORT_BYTES;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    content.truncate(cut);
    (content, true)
}
