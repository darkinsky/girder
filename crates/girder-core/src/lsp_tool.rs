//! The `lsp` tool: the four semantic queries, asked in the convention a model reads code in.
//!
//! # Why the conversion is here and not in the LSP seam
//!
//! The protocol counts lines from zero and columns in UTF-16 code units. A person, and therefore a model
//! reading the same file through a line-numbered view, counts lines from one and positions in *characters*.
//! Both conventions are correct; the question is only where the translation happens.
//!
//! It cannot happen below this tool, because the seam never sees the line. [`crate::lsp::Request`] carries
//! a [`crate::lsp::Position`] and a path, and by the time a provider holds the request the file's text has
//! been left behind — a provider would have to read the file again to convert, and every provider would
//! then convert back for its own display. [`crate::lsp`] says as much in its module documentation: the
//! model-facing tool owns the one-based convention, and the conversion happens once, at that boundary. It
//! does happen here, and it needs the file, which is also why this tool insists the file be readable.
//!
//! # Why "the third character" is not "column three"
//!
//! Three different numbers are all called a column:
//!
//! - a byte offset, which Rust's `&str` slicing uses and which grows by three for `中`;
//! - a UTF-16 code-unit offset, which the protocol uses and which grows by two for `😀`;
//! - and a character index, which is the one a reader means.
//!
//! For ASCII all three agree, which is why the mistake survives testing. On `中x` the second character is
//! UTF-16 column 1, not 2; on `😀x` the second character is UTF-16 column 2, not 1. Subtracting one from the
//! one-based input would be right only on the all-ASCII lines where the check never fails. So this tool
//! reads the line and counts, rather than adjusting the number.
//!
//! # Why an answer's location is converted back
//!
//! Whatever a caller sends in, it must be able to read back out. A location shown as a UTF-16 column while
//! the input is a character index would send the model to the wrong place on exactly the lines this module
//! exists to get right, so the same line is read and the column is translated back into a one-based
//! character. When that line cannot be read, only the line number is reported: a column whose unit is
//! unknown is worse than no column at all.
//!
//! # Why a missing line is an error and an empty answer is not
//!
//! A line past the end of the file is a mistake in the call, and the model can fix it with the line count
//! in the message. An answer of "nothing" is a fact about the code — no definition exists — and reporting
//! it as a failure would make the model retry a query that succeeded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::lsp::{Answer, Location, LspError, Operation, Position, Registry, Request};
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};
use crate::workspace_path::{display, resolve};

/// Builds the `lsp` tool over a registry.
///
/// The registry is captured rather than resolved per call: a tool that looked one up would have to decide
/// what a missing registry means, and "no language servers" is a property of the deployment the caller
/// already knows when it builds the tool.
#[must_use]
pub fn lsp_tool(registry: Arc<Registry>) -> ToolDefinition {
    ToolDefinition::new(
        "lsp",
        "Asks a language server where a name is defined, where it is used, what implements it, or what it \
         means. `line` and `character` are one-based, and `character` counts characters as a reader does — \
         not bytes — so it stays right on lines containing non-ASCII text.",
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["definition", "references", "implementation", "hover"],
                    "description": "`definition` finds where the name under the cursor is declared, \
                                    `references` finds every use of it, `implementation` finds what \
                                    implements a trait or interface member, and `hover` returns its \
                                    documentation and type"
                },
                "path": { "type": "string", "description": "File to query; relative paths resolve against the workspace" },
                "line": { "type": "number", "description": "One-based line number" },
                "character": { "type": "number", "description": "One-based character position within the line" },
                "workspace_root": {
                    "type": "string",
                    "description": "Root of the project the file belongs to; defaults to the session workspace"
                }
            },
            "required": ["operation", "path", "line", "character"]
        }),
        Arc::new(move |args: serde_json::Value, ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move { Ok(run(&args, &ctx, &registry).await) })
        }),
    )
}

/// Parses the arguments, converts the position, and asks the registry.
async fn run(args: &serde_json::Value, ctx: &ToolRunContext, registry: &Registry) -> ToolResult {
    let Some(operation) = args
        .get("operation")
        .and_then(serde_json::Value::as_str)
        .and_then(operation_from_name)
    else {
        let given = args
            .get("operation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        return ToolResult::error(format!(
            "`operation` is `{given}`, which is not `definition`, `references`, `implementation`, or `hover`"
        ));
    };

    let Some(raw_path) = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.trim().is_empty())
    else {
        return ToolResult::error("`path` is required");
    };

    let Some(line) = one_based(args, "line") else {
        return ToolResult::error("`line` is required and starts at 1");
    };
    let Some(character) = one_based(args, "character") else {
        return ToolResult::error("`character` is required and starts at 1");
    };

    // A root named in the arguments wins over the session's: the caller asked about a file, and a file may
    // belong to a workspace other than the one the session is sitting in.
    let root = args
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .filter(|root| !root.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| ctx.workspace.clone());
    let path = resolve(root.as_deref(), raw_path);

    let Ok(text) = std::fs::read_to_string(&path) else {
        return ToolResult::error(format!(
            "cannot read `{}`, so the position cannot be converted",
            path.display()
        ));
    };
    let Some(source_line) = line_at(&text, line) else {
        let total = text.split('\n').count();
        return ToolResult::error(format!(
            "`{}` has {total} line{}, so line {line} does not exist",
            path.display(),
            if total == 1 { "" } else { "s" }
        ));
    };
    let Some(column) = character_to_utf16_column(source_line, character) else {
        let length = source_line.chars().count();
        return ToolResult::error(format!(
            "line {line} has {length} character{}, so character {character} does not exist",
            if length == 1 { "" } else { "s" }
        ));
    };

    // A server started at the wrong root answers about a different project, and a file outside every
    // workspace has no better candidate than the directory it sits in.
    let workspace_root = root.unwrap_or_else(|| {
        path.parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    });
    let request = Request::new(
        operation,
        &workspace_root,
        &path,
        Position::new(line - 1, column),
    );

    match registry.query(request).await {
        Ok(answer) => ToolResult::text(render(&answer, &workspace_root, operation)),
        Err(LspError::NoProvider { .. }) => {
            let extensions = registry.extensions();
            if extensions.is_empty() {
                ToolResult::error(format!(
                    "no language server serves `{}`, and none are registered in this session",
                    path.display()
                ))
            } else {
                // The served extensions are the correction: a caller that sent a `.txt` learns here that the
                // deployment speaks Rust and Python, rather than reading "no server" as "no definition".
                ToolResult::error(format!(
                    "no language server serves `{}`; this session serves {}",
                    path.display(),
                    extensions
                        .into_iter()
                        .map(|extension| format!(".{extension}"))
                        .collect::<Vec<String>>()
                        .join(", ")
                ))
            }
        }
        Err(error) => ToolResult::error(format!("`{operation:?}` failed: {error}")),
    }
}

/// Reads a one-based argument, rejecting zero and anything that is not a non-negative integer.
fn one_based(args: &serde_json::Value, key: &str) -> Option<u32> {
    let value = args.get(key).and_then(serde_json::Value::as_u64)?;
    let value = u32::try_from(value).ok()?;
    (value > 0).then_some(value)
}

/// The line at a one-based number.
///
/// Split on `\n` rather than [`str::lines`] so a file ending in a newline still has the empty final line a
/// reader sees as the place after the last newline. A trailing `\r` is stripped, because a CRLF file's line
/// does not end in a visible character and counting it would shift every column on it.
#[must_use]
fn line_at(text: &str, line: u32) -> Option<&str> {
    let index = usize::try_from(line.checked_sub(1)?).ok()?;
    text.split('\n')
        .nth(index)
        .map(|line| line.strip_suffix('\r').map_or(line, |stripped| stripped))
}

/// Converts a one-based character position to a zero-based UTF-16 column.
///
/// `character` is the position *of* a character, so the column is the total UTF-16 width of everything
/// before it. One past the last character means the end of the line, which is a valid cursor position; any
/// further is `None`, because it names a place that does not exist.
///
/// On `abc` the second character is column 1. On `中x` it is also column 1: `中` is one UTF-16 unit even
/// though it is three UTF-8 bytes. On `😀x` it is column 2, because `😀` is two UTF-16 units. That spread is
/// why this counts instead of subtracting one.
#[must_use]
pub fn character_to_utf16_column(line: &str, character: u32) -> Option<u32> {
    let target = character.checked_sub(1)?;
    let mut index = 0u32;
    let mut units = 0u32;
    for source in line.chars() {
        if index == target {
            return Some(units);
        }
        units += u32::try_from(source.len_utf16()).ok()?;
        index += 1;
    }
    (index == target).then_some(units)
}

/// Converts a zero-based UTF-16 column back to a one-based character position.
///
/// The inverse of [`character_to_utf16_column`], used so a reported location is in the same convention as
/// the position that was sent. A column inside a surrogate pair maps to no character and yields `None`.
#[must_use]
pub fn utf16_column_to_character(line: &str, column: u32) -> Option<u32> {
    let mut units = 0u32;
    let mut index = 0u32;
    for source in line.chars() {
        if units == column {
            return index.checked_add(1);
        }
        units += u32::try_from(source.len_utf16()).ok()?;
        if units > column {
            // The column lands inside a surrogate pair, which no character position names.
            return None;
        }
        index += 1;
    }
    if units == column {
        return index.checked_add(1);
    }
    None
}

/// Parses an operation name.
///
/// Spelled out rather than derived from serde so the error can name the value in the model's terms; serde's
/// own message is about a variant index.
fn operation_from_name(name: &str) -> Option<Operation> {
    match name {
        "definition" => Some(Operation::Definition),
        "references" => Some(Operation::References),
        "implementation" => Some(Operation::Implementation),
        "hover" => Some(Operation::Hover),
        _ => None,
    }
}

/// Renders an answer for the model.
fn render(answer: &Answer, root: &Path, operation: Operation) -> String {
    match answer {
        Answer::Text(text) if text.trim().is_empty() => {
            format!("`{}` returned no text", operation.as_str())
        }
        Answer::Text(text) => text.clone(),
        Answer::Nothing => format!(
            "`{}` returned nothing; the server could not determine an answer",
            operation.as_str()
        ),
        Answer::Locations(locations) if locations.is_empty() => {
            format!("`{}` found no locations", operation.as_str())
        }
        Answer::Locations(locations) => {
            // A `references` answer routinely names many ranges in one file. Reading it once per location would
            // turn a large result into a large number of identical reads, so each file is read at most once and
            // a failure is cached too — retrying an unreadable file per location would fail the same way.
            let mut sources: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
            let mut out = format!("{} location(s):", locations.len());
            for location in locations {
                out.push('\n');
                out.push_str(&render_location(location, root, &mut sources));
            }
            out
        }
    }
}

/// Renders one location as `path:line:character`, in the same one-based convention the tool accepts.
fn render_location(
    location: &Location,
    root: &Path,
    sources: &mut BTreeMap<PathBuf, Option<String>>,
) -> String {
    let path = display(Some(root), &location.path);
    // The protocol's line is zero-based; every number this tool shows is one-based. Saturating because the
    // protocol's field is a `u32` the server chooses, and a position one past its maximum is not worth a panic.
    let line = location.range.start.line.saturating_add(1);
    let Some(text) = sources
        .entry(location.path.clone())
        .or_insert_with(|| std::fs::read_to_string(&location.path).ok())
        .as_deref()
    else {
        return format!("{path}:{line}");
    };
    match line_at(text, line)
        .and_then(|source| utf16_column_to_character(source, location.range.start.character))
    {
        Some(character) => format!("{path}:{line}:{character}"),
        // The line could not be read or its column falls inside a surrogate pair, so the unit is unknown and
        // the line alone is the honest report.
        None => format!("{path}:{line}"),
    }
}
