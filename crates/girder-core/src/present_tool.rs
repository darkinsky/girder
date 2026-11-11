//! The `present` tool: announcing a file a person should open.
//!
//! A tool can write a file; that is not the same as telling anyone it exists. A run that produces a report and says
//! nothing about it leaves the person to find it, and the announcement is the difference between output and a
//! deliverable.
//!
//! # Why the announcement is recorded rather than printed
//!
//! A model that printed "I made report.md" would be describing a file it has not checked exists. The tool verifies
//! the path and records the announcement in a set a host renders, so the file card the person clicks is the file the
//! tool looked at.
//!
//! # Why the kind is declared by the caller
//!
//! A `.json` is sometimes a document and sometimes a data file. Guessing from the extension gets one of those wrong,
//! and the cost of the wrong choice is that a person opens it in the wrong application. [`DeliverableKind`] therefore
//! takes what the caller said and only falls back to the extension when it said nothing.
//!
//! # Why a missing file is a result and not an error
//!
//! A model that announced a path it had not written needs to know that, and telling it in a result lets it correct the
//! mistake in the same turn. An error would end the turn and leave the person with nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::deliverable::{Deliverable, Kind as DeliverableKind, Set as DeliverableSet};
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// Builds the `present` tool.
///
/// The set is shared with the caller so a host can render what a run announced: a tool that kept it privately would
/// announce to nobody.
#[must_use]
pub fn present_tool(
    workspace: PathBuf,
    set: Arc<std::sync::Mutex<DeliverableSet>>,
) -> ToolDefinition {
    ToolDefinition::new(
        "present",
        "Announces a file as something the person should open",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file to present" },
                "title": { "type": "string", "description": "What to call it in a list" },
                "kind": {
                    "type": "string",
                    "enum": ["document", "spreadsheet", "deck", "image", "artefact", "other"],
                    "description": "What sort of file it is; inferred from the extension when omitted"
                },
                "description": { "type": "string", "description": "One line about what it contains" }
            },
            "required": ["path", "title"]
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let workspace = workspace.clone();
            let set = Arc::clone(&set);
            Box::pin(async move { Ok(present(&args, &workspace, &set)) })
        }),
    )
}

fn present(
    args: &serde_json::Value,
    workspace: &Path,
    set: &Arc<std::sync::Mutex<DeliverableSet>>,
) -> ToolResult {
    let path = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if path.trim().is_empty() {
        return ToolResult::error("`path` is required");
    }
    let title = args
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if title.trim().is_empty() {
        return ToolResult::error("`title` is required");
    }

    let resolved = crate::workspace_path::resolve(Some(workspace), path);

    // The check runs before the announcement: a card a person clicks that leads nowhere is worse than a model told to
    // write the file first.
    let (bytes, exists) = match std::fs::metadata(&resolved) {
        Ok(metadata) if metadata.is_file() => (Some(metadata.len()), true),
        _ => (None, false),
    };
    if !exists {
        return ToolResult::error(format!(
            "`{path}` does not exist; write it before presenting it"
        ));
    }

    // The declared kind wins over the inferred one: a `.json` is sometimes a document, and guessing wrong opens it in
    // the wrong application.
    let kind = match args.get("kind").and_then(serde_json::Value::as_str) {
        Some("document") => DeliverableKind::Document,
        Some("spreadsheet") => DeliverableKind::Spreadsheet,
        Some("deck") => DeliverableKind::Deck,
        Some("image") => DeliverableKind::Image,
        Some("artefact") => DeliverableKind::Artefact,
        Some("other") => DeliverableKind::Other,
        _ => DeliverableKind::from_path(&resolved),
    };

    let Some(mut deliverable) = Deliverable::new(&resolved, title, kind) else {
        return ToolResult::error("`title` must not be blank");
    };
    if let Some(description) = args.get("description").and_then(serde_json::Value::as_str) {
        deliverable = deliverable.described(description);
    }
    if let Some(bytes) = bytes {
        deliverable = deliverable.with_size(bytes);
    }

    let shown = crate::workspace_path::display(Some(workspace), &resolved);
    if let Ok(mut set) = set.lock() {
        // Announcing one path twice keeps the first: a list that showed it twice would look like two outputs.
        let before = set.len();
        set.add(deliverable);
        let added = set.len() > before;
        return ToolResult::text(if added {
            format!("presented `{shown}` as {}", kind.as_str())
        } else {
            format!("`{shown}` was already presented; the first announcement stands")
        });
    }
    ToolResult::error("the deliverable set is unavailable")
}
