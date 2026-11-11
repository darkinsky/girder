//! The `todo_write` tool: a task list the model maintains and a person can read.
//!
//! A model working on a multi-step task forgets steps between tool calls. The list is the memory: it
//! says what is done, what is being done, and what is left, so the next turn does not have to
//! reconstruct that from the transcript.
//!
//! # Why the list is a file
//!
//! The list is held in memory upstream, inside one process. Here it is a file under the paths the
//! caller passes in, because the two readers are different processes: the model, which needs it back
//! verbatim after a restart, and a person, who needs to see progress without reading a transcript. A
//! file is the smallest thing both can read.
//!
//! # Why the write is atomic
//!
//! The file has a reader that did not ask for it: a UI polls it to draw a checklist. A direct write
//! would let that reader observe a truncated JSON document and draw nothing, or worse, a half list.
//! [`write_text_atomic`] replaces the file by rename, so a reader sees the old list or the new one.
//! It also creates the parent directory, which is why this module does not: the tool receives a path
//! it did not choose, and the directory may not exist on the first call.
//!
//! # Why a bad list is refused before the file is touched
//!
//! The file on disk is the last known good state. A list that fails validation must leave that state
//! alone: a model that sends two active steps and is told so can fix the list and send it again, but
//! a model whose rejected list already replaced the good one has lost the record of what it finished.
//!
//! # Why at most one step is `in_progress`
//!
//! Two active steps means either the model is genuinely working in parallel — which this tool cannot
//! verify and which makes "what is happening now" unanswerable — or the list was not updated when a
//! step started, which is the failure the list exists to prevent. Refusing is the cheaper mistake:
//! the model gets a corrective message in the same turn.
//!
//! # Why a missing file reads as an empty list
//!
//! The list is session scratch state, not a contract. A turn that dies because its scratch file was
//! never written, or was written by an older version, is a worse outcome than a turn that starts
//! from an empty list. The cost is that "no file" and "empty list" are indistinguishable to a caller;
//! both mean "nothing to show", which is what every caller here does with the answer.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::atomic_write::{Mode, write_text_atomic};
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// Where one todo stands.
///
/// The three states are the whole vocabulary on purpose: a finer set (`blocked`, `cancelled`) invites
/// a model to park work there, and a list whose items are mostly parked is not a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Not started.
    Pending,
    /// Being worked on now. At most one item may hold this.
    InProgress,
    /// Done.
    Completed,
}

impl Status {
    /// The character between the brackets of a rendered line.
    ///
    /// Chosen to match the markers people already know from task lists, so the rendering needs no
    /// legend.
    #[must_use]
    pub fn marker(self) -> char {
        match self {
            Self::Pending => ' ',
            Self::InProgress => '>',
            Self::Completed => 'x',
        }
    }
}

/// One item in the list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    /// The step, as the model worded it.
    pub content: String,
    /// Where it stands.
    pub status: Status,
}

/// The document written to disk.
///
/// A struct wrapping the array rather than a bare array, so a later version can add fields (a
/// revision, a timestamp) without making today's readers fail on them. `default` on the field keeps a
/// document that predates those fields readable.
#[derive(Debug, Default, Deserialize, Serialize)]
struct Document {
    #[serde(default)]
    todos: Vec<Todo>,
}

/// Reads the list at `path`.
///
/// A missing file, a permission error, and malformed content all yield an empty list: see the module
/// documentation for why this reader does not report failures.
#[must_use]
pub fn read_todos(path: &Path) -> Vec<Todo> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Document>(&text).map_or_else(|_| Vec::new(), |document| document.todos)
}

/// Renders the list as the one line a person scans and the model reads back.
///
/// Input order is preserved rather than sorted by status: the plan is a sequence, and a checklist
/// that reorders itself between calls reads as if the steps had changed.
#[must_use]
pub fn summarize(todos: &[Todo]) -> String {
    let completed = todos
        .iter()
        .filter(|todo| todo.status == Status::Completed)
        .count();
    let mut out = format!("{completed}/{} completed", todos.len());
    for todo in todos {
        // `write!` into a `String` cannot fail; ignoring the result keeps the lint that flags a
        // discarded `write!` satisfied without an unwrap path that could never be taken.
        let _ = write!(out, "\n- [{}] {}", todo.status.marker(), todo.content);
    }
    out
}

/// Builds the `todo_write` tool, storing the list at `path`.
///
/// The path is captured rather than taken as an argument: the model must not choose where the
/// session keeps its own state, or a prompt-injected list could be written over a file that matters.
#[must_use]
pub fn todo_tool(path: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "todo_write",
        "Records and updates a task list to plan multi-step work and show progress; skip it for \
         trivial single-step tasks. Add one todo per concrete step before you start. While work \
         remains, keep exactly one todo `in_progress` — the step you are on now — and mark each todo \
         `completed` as soon as it is done.",
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let path = path.clone();
            // The write is one small file. A `spawn_blocking` hop would cost a thread move for a few
            // hundred bytes, and the tool already runs inside a turn that is waiting on it.
            Box::pin(async move { Ok(record(&args, &path)) })
        }),
    )
}

/// Validates the arguments, writes the list, and returns the summary.
///
/// Returns a [`ToolResult`] rather than a [`crate::tools::ToolError`]: a list the model can fix is a
/// result it should see and act on in the same turn, and an error would end the turn instead.
fn record(args: &serde_json::Value, path: &Path) -> ToolResult {
    let Some(raw) = args.get("todos") else {
        return ToolResult::error("`todos` is required");
    };
    let Some(items) = raw.as_array() else {
        return ToolResult::error("`todos` must be an array");
    };

    let mut todos = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let position = index + 1;

        let Some(content) = item.get("content").and_then(serde_json::Value::as_str) else {
            return ToolResult::error(format!("todo {position} has no `content` string"));
        };
        // Blank rather than only empty: a step of spaces renders as a bullet with nothing after it,
        // which is exactly the "I have a list" gesture without the plan.
        if content.trim().is_empty() {
            return ToolResult::error(format!(
                "todo {position} has empty `content`; every todo needs a concrete step"
            ));
        }

        let Some(name) = item.get("status").and_then(serde_json::Value::as_str) else {
            return ToolResult::error(format!("todo {position} has no `status` string"));
        };
        let Some(status) = status_from_name(name) else {
            return ToolResult::error(format!(
                "todo {position} has status `{name}`, which is not `pending`, `in_progress`, or `completed`"
            ));
        };

        todos.push(Todo {
            content: content.to_owned(),
            status,
        });
    }

    let active = todos
        .iter()
        .filter(|todo| todo.status == Status::InProgress)
        .count();
    if active > 1 {
        return ToolResult::error(format!(
            "the list has {active} todos `in_progress`; keep at most one, because a list with two \
             active steps is usually a list that was not kept up to date"
        ));
    }

    // Validation is complete, so the file may now be replaced: see the module documentation on why
    // the order matters.
    let document = Document { todos };
    let text = match serde_json::to_string_pretty(&document) {
        Ok(text) => text,
        Err(error) => return ToolResult::error(format!("cannot encode the list: {error}")),
    };
    // `PRIVATE`: the list says what this session is doing, which is scratch state and not a
    // deliverable another account should read.
    if let Err(error) = write_text_atomic(path, &text, Mode::PRIVATE) {
        return ToolResult::error(format!("cannot write `{}`: {error}", path.display()));
    }

    ToolResult::text(summarize(&document.todos))
}

/// Parses a status name.
///
/// Spelled out rather than derived from serde so the error can name the offending value in the
/// model's terms; serde's own message is about a field's variant index.
fn status_from_name(name: &str) -> Option<Status> {
    match name {
        "pending" => Some(Status::Pending),
        "in_progress" => Some(Status::InProgress),
        "completed" => Some(Status::Completed),
        _ => None,
    }
}
