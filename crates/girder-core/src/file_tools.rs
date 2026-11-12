//! The file tools: read, write, and edit.
//!
//! These are the three operations a coding agent spends its time on, and each has a failure mode worth naming:
//!
//! - **read** that returns a whole file wastes a context window on lines the model did not need
//! - **write** that replaces a file silently discards whatever someone else changed
//! - **edit** that replaces every occurrence changes code the caller never looked at
//!
//! # Why read numbers its lines
//!
//! A model that quotes a line to edit needs a way to say which line. A numbered read makes the number part of the
//! observation, so the edit that follows is grounded in what was actually seen rather than in a guess about offsets.
//!
//! # Why write records an observation
//!
//! The write is the new expectation. A session that read a file, wrote it, and then wrote it again would otherwise
//! fail its own second write against a change it made itself.
//!
//! # Why edit refuses an ambiguous match
//!
//! Replacing every occurrence would change code the caller never looked at, and the caller would only find out on the
//! next read — which may be never.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::observation::{self, Observation};
use crate::patch;
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// How many bytes a read returns before it refuses rather than truncates.
///
/// Truncating a file mid-line would give the model a partial statement that looks complete. A refusal names the size
/// and lets the model ask for a range.
pub const MAX_READ_BYTES: usize = 512 * 1024;

/// Builds the `read` tool.
#[must_use]
pub fn read_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "read",
        "Reads a file, returning its lines with one-based line numbers",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to read, absolute or relative to the workspace" },
                "offset": { "type": "number", "description": "First line to return, one-based" },
                "limit": { "type": "number", "description": "How many lines to return" }
            },
            "required": ["path"]
        }),
        std::sync::Arc::new(move |args: serde_json::Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { read(&args, &workspace, &ctx).await })
        }),
    )
}

async fn read(
    args: &serde_json::Value,
    workspace: &Path,
    ctx: &ToolRunContext,
) -> Result<ToolResult, crate::tools::ToolError> {
    let Ok(path) = resolve(args, workspace) else {
        return Ok(ToolResult::error("`path` is required"));
    };
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) => {
            return Ok(ToolResult::error(format!(
                "cannot read `{}`: {error}",
                path.display()
            )));
        }
    };

    if text.len() > MAX_READ_BYTES {
        return Ok(ToolResult::error(format!(
            "`{}` is {} bytes, over the {MAX_READ_BYTES}-byte limit; read a range with `offset` and `limit`",
            path.display(),
            text.len()
        )));
    }

    let offset = args
        .get("offset")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let offset = usize::try_from(offset.max(1)).unwrap_or(1);
    let limit = args.get("limit").and_then(serde_json::Value::as_u64);

    let lines: Vec<&str> = text.lines().collect();
    let start = (offset - 1).min(lines.len());
    let end = match limit {
        Some(limit) => (start + usize::try_from(limit).unwrap_or(usize::MAX)).min(lines.len()),
        None => lines.len(),
    };

    let mut out = String::new();
    for (index, line) in lines[start..end].iter().enumerate() {
        // `write!` into a `String` cannot fail, and the result is ignored rather than unwrapped so the lint that
        // flags `format!` appended to a `String` is satisfied without a panic path.
        let _ = writeln!(out, "{}\t{line}", start + index + 1);
    }

    // The observation is recorded even for a partial read: what the model saw is what a later write must not
    // contradict, and a range still establishes that this session has looked at the file.
    ctx.observe(&path, Observation::Present(observation::version_of(&text)));

    // The language is named in the result because a client that renders the lines wants to highlight them, and the
    // alternative is every consumer re-deriving it from the extension with its own table. One shared table is what
    // keeps a preview and a diff agreeing about whether `.m` is Objective-C or MATLAB.
    Ok(ToolResult::text(
        match crate::code_language::read_hint(&path.display().to_string()) {
            Some(language) => format!("{language}\n{out}"),
            None => out,
        },
    ))
}

/// Builds the `write` tool.
#[must_use]
pub fn write_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "write",
        "Writes a file, replacing its contents",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to write" },
                "content": { "type": "string", "description": "The complete new contents" }
            },
            "required": ["path", "content"]
        }),
        std::sync::Arc::new(move |args: serde_json::Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { write(&args, &workspace, &ctx).await })
        }),
    )
}

async fn write(
    args: &serde_json::Value,
    workspace: &Path,
    ctx: &ToolRunContext,
) -> Result<ToolResult, crate::tools::ToolError> {
    let Ok(path) = resolve(args, workspace) else {
        return Ok(ToolResult::error("`path` is required"));
    };
    let content = args
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if content.is_empty() {
        return Ok(ToolResult::error("`content` is required"));
    }

    // The conflict check runs before anything is written: a write that clobbered a change the session never saw is
    // the failure this exists to prevent, and discovering it after the write is too late.
    if let Err(conflict) = ctx.check_write(&path) {
        return Ok(ToolResult::error(format!(
            "refusing to write `{}`: {}",
            path.display(),
            conflict.describe()
        )));
    }

    if let Some(parent) = path.parent()
        && !parent.is_dir()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        return Ok(ToolResult::error(format!(
            "cannot create `{}`: {error}",
            parent.display()
        )));
    }

    if let Err(error) = tokio::fs::write(&path, &content).await {
        return Ok(ToolResult::error(format!(
            "cannot write `{}`: {error}",
            path.display()
        )));
    }

    // The write is the new expectation: a session that wrote and then wrote again must not fail its own second write.
    ctx.observe(
        &path,
        Observation::Present(observation::version_of(&content)),
    );

    Ok(ToolResult::text(format!(
        "wrote {} bytes to {}",
        content.len(),
        path.display()
    )))
}

/// Builds the `edit` tool.
#[must_use]
pub fn edit_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "edit",
        "Replaces one exact occurrence of a text in a file",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to edit" },
                "old": { "type": "string", "description": "The exact text to replace; must occur once" },
                "new": { "type": "string", "description": "The replacement" }
            },
            "required": ["path", "old", "new"]
        }),
        std::sync::Arc::new(move |args: serde_json::Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { edit(&args, &workspace, &ctx).await })
        }),
    )
}

async fn edit(
    args: &serde_json::Value,
    workspace: &Path,
    ctx: &ToolRunContext,
) -> Result<ToolResult, crate::tools::ToolError> {
    let Ok(path) = resolve(args, workspace) else {
        return Ok(ToolResult::error("`path` is required"));
    };
    let old = args
        .get("old")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if old.is_empty() {
        return Ok(ToolResult::error("`old` is required"));
    }
    let new = args
        .get("new")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) => {
            return Ok(ToolResult::error(format!(
                "cannot read `{}`: {error}",
                path.display()
            )));
        }
    };

    if let Err(conflict) = ctx.check_write(&path) {
        return Ok(ToolResult::error(format!(
            "refusing to edit `{}`: {}",
            path.display(),
            conflict.describe()
        )));
    }

    let edited = match observation::replace_exactly_once(&text, old, new) {
        Ok(edited) => edited,
        Err(error) => return Ok(ToolResult::error(format!("`{}`: {error}", path.display()))),
    };

    if let Err(error) = tokio::fs::write(&path, &edited).await {
        return Ok(ToolResult::error(format!(
            "cannot write `{}`: {error}",
            path.display()
        )));
    }
    ctx.observe(
        &path,
        Observation::Present(observation::version_of(&edited)),
    );

    Ok(ToolResult::text(format!("edited {}", path.display())))
}

/// Builds the `apply_patch` tool.
///
/// The patch parser and the hunk matcher were written before a caller existed, so this is where they earn their
/// place: a multi-file change is one call rather than a sequence of edits that can fail halfway through.
#[must_use]
pub fn patch_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "apply_patch",
        "Applies a multi-file patch",
        json!({
            "type": "object",
            "properties": {
                "patch": { "type": "string", "description": "The patch, beginning with `*** Begin Patch`" }
            },
            "required": ["patch"]
        }),
        std::sync::Arc::new(move |args: serde_json::Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { apply(&args, &workspace, &ctx).await })
        }),
    )
}

async fn apply(
    args: &serde_json::Value,
    workspace: &Path,
    ctx: &ToolRunContext,
) -> Result<ToolResult, crate::tools::ToolError> {
    let text = args
        .get("patch")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if text.trim().is_empty() {
        return Ok(ToolResult::error("`patch` is required"));
    }

    let operations = match patch::parse(text.as_str()) {
        Ok(operations) => operations,
        Err(error) => {
            return Ok(ToolResult::error(format!(
                "cannot parse the patch: {error}"
            )));
        }
    };

    // Every operation is computed before any is applied. A patch that fails on its third file must not leave the
    // first two changed: the caller asked for a change, not for a prefix of one.
    let mut planned: Vec<(PathBuf, Option<String>)> = Vec::new();
    for operation in &operations {
        let path = workspace.join(operation.path());
        match operation {
            patch::Operation::Add { content, .. } => planned.push((path, Some(content.clone()))),
            patch::Operation::Delete { .. } => planned.push((path, None)),
            patch::Operation::Update { hunks, move_to, .. } => {
                let existing = match tokio::fs::read_to_string(&path).await {
                    Ok(existing) => existing,
                    Err(error) => {
                        return Ok(ToolResult::error(format!(
                            "cannot read `{}`: {error}",
                            path.display()
                        )));
                    }
                };
                let applied = patch::apply(&existing, hunks);
                if !applied.is_complete() {
                    let reasons: Vec<String> = applied
                        .failures
                        .iter()
                        .map(|failure| {
                            format!(
                                "hunk {} at line {}: {:?}",
                                failure.hunk, failure.expected, failure.reason
                            )
                        })
                        .collect();
                    return Ok(ToolResult::error(format!(
                        "the patch does not apply to `{}`: {}",
                        path.display(),
                        reasons.join("; ")
                    )));
                }
                planned.push((path, Some(applied.text)));
                if let Some(destination) = move_to {
                    planned.push((workspace.join(destination), None));
                }
            }
        }
    }

    for (path, content) in &planned {
        if let Some(content) = content {
            if let Some(parent) = path.parent()
                && !parent.is_dir()
            {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Err(error) = tokio::fs::write(path, content).await {
                return Ok(ToolResult::error(format!(
                    "cannot write `{}`: {error}",
                    path.display()
                )));
            }
            ctx.observe(path, Observation::Present(observation::version_of(content)));
        } else {
            let _ = tokio::fs::remove_file(path).await;
            ctx.forget(path);
        }
    }

    Ok(ToolResult::text(format!(
        "applied {} file operation(s)",
        operations.len()
    )))
}

/// Resolves a path against the workspace.
fn resolve(args: &serde_json::Value, workspace: &Path) -> Result<PathBuf, String> {
    let path = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "`path` is required".to_owned())?;
    Ok(crate::workspace_path::resolve(Some(workspace), path))
}

/// Builds the `ask_user` tool.
///
/// A model that needs a decision it cannot make — which of two designs, whether to overwrite — should ask rather than
/// guess. The alternative is a model inventing an answer and building on it, which costs more to undo than a question
/// costs to answer.
///
/// # Why the answer is a result rather than an error
///
/// A person declining to answer is information: the model can proceed differently. An error would make the turn look
/// broken and the model would retry the same question.
#[must_use]
pub fn ask_tool(responder: std::sync::Arc<dyn crate::questions::Responder>) -> ToolDefinition {
    ToolDefinition::new(
        "ask_user",
        "Asks the person a question and waits for the answer",
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "What to ask" },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "The choices to offer; omit for a free-form answer"
                },
                "context": { "type": "string", "description": "Why you are asking, to help them decide" },
                "multiple": {
                    "type": "boolean",
                    "description": "Whether more than one option may be chosen"
                }
            },
            "required": ["question"]
        }),
        std::sync::Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let responder = std::sync::Arc::clone(&responder);
            Box::pin(async move { ask_inner(&args, responder).await })
        }),
    )
}

async fn ask_inner(
    args: &serde_json::Value,
    responder: std::sync::Arc<dyn crate::questions::Responder>,
) -> Result<ToolResult, crate::tools::ToolError> {
    let question = args
        .get("question")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if question.trim().is_empty() {
        return Ok(ToolResult::error("`question` is required"));
    }

    let options: Vec<String> = args
        .get("options")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    // A question with one option is a confirmation, and a model that offered one choice meant to ask a yes/no
    // question without saying so. Treating it as a confirmation gives the person a usable control.
    let multiple = args
        .get("multiple")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let choices = || {
        options
            .iter()
            .map(|option| crate::questions::Option_::new(option.clone()))
            .collect::<Vec<_>>()
    };

    let shape = match (options.len(), multiple) {
        (0, _) => crate::questions::Shape::Text {
            label: question.to_owned(),
            optional: false,
        },
        (1, false) => crate::questions::Shape::Confirm {
            label: question.to_owned(),
        },
        (_, true) => crate::questions::Shape::Multiple {
            label: question.to_owned(),
            options: choices(),
            // At least one: a model that offered choices and received none has learned nothing, and would have to ask
            // again.
            min: 1,
            max: options.len(),
        },
        (_, false) => crate::questions::Shape::Choice {
            label: question.to_owned(),
            options: choices(),
        },
    };

    let mut ask = crate::questions::Question::new(shape);
    if let Some(context) = args.get("context").and_then(serde_json::Value::as_str) {
        ask = ask.with_context(context.to_owned());
    }

    match responder.ask(ask).await {
        Ok(reply) => {
            let answer = match &reply.answer {
                crate::questions::Answer::Text(text) => text.clone(),
                crate::questions::Answer::Chosen(value) => value.clone(),
                crate::questions::Answer::ChosenMany(values) => values.join(", "),
                crate::questions::Answer::Confirmed(confirmed) => {
                    if *confirmed { "yes" } else { "no" }.to_owned()
                }
            };
            // The default case is recorded in the text: a model that treated a timeout's default as a person's answer
            // would build on consent it never received.
            Ok(ToolResult::text(if reply.is_human() {
                answer
            } else {
                format!("{answer} (default; nobody answered)")
            }))
        }
        Err(error) => Ok(ToolResult::error(format!("cannot ask: {error}"))),
    }
}
