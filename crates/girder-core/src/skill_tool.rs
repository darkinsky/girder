//! The `skill` tool: the catalog a model reads to choose a skill, and one skill's content.
//!
//! # Why one tool lists and loads
//!
//! The catalog is what tells a model a skill exists; the body is what it reads after choosing. Two tools
//! would split that into two calls with a turn boundary between them — the model lists, the turn ends, the
//! model loads — and the listing exists only to inform the load. One tool with an optional name makes the
//! listing the default answer and the body the specified one.
//!
//! # Why the body is inlined and the files are only named
//!
//! The body is what the model follows, so it has to arrive with the answer. A skill's files are reference
//! material that the procedure points at, and a skill may ship enough of them that inlining everything
//! would let one load dominate the window. Listing each file with its size is what tells a model whether
//! the material it needs even exists; the content stays with whoever can read the skill's directory.
//!
//! # Why an unknown name lists what does exist
//!
//! A model that asked for a skill by the wrong name has one correction available to it: the real names. A
//! bare "no such skill" leaves it guessing, and guessing at a name it has not seen is what produced the
//! wrong one.

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::json;

use crate::skill::{Definition, Registry, SkillError};
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// Builds the `skill` tool over a registry.
///
/// The registry is captured rather than resolved from the context: a tool that looked one up per call
/// would have to decide what a missing registry means, and the answer — "no skills" — is a property of
/// this deployment that the caller already knows when it builds the tool.
#[must_use]
pub fn skill_tool(registry: Arc<Registry>) -> ToolDefinition {
    ToolDefinition::new(
        "skill",
        "Lists the skills available in this session, or loads one by name and returns its instructions. \
         Call it with no name first to see what exists; the description is what tells you whether a skill \
         applies. Call it with a name to read the procedure before following it.",
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill to load; omit it to list every skill and its description"
                }
            }
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let registry = Arc::clone(&registry);
            Box::pin(async move { Ok(run(&args, &registry).await) })
        }),
    )
}

/// Lists when no name is given, loads when one is.
///
/// A blank name is treated as absent rather than as a lookup for `""`: a model that emitted an empty
/// string was asking to see the list, and refusing it would spend a turn on a punctuation difference.
async fn run(args: &serde_json::Value, registry: &Registry) -> ToolResult {
    let name = args
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());
    match name {
        Some(name) => load(name, registry).await,
        None => catalog(registry).await,
    }
}

/// Renders the merged catalog, one line per skill.
async fn catalog(registry: &Registry) -> ToolResult {
    let entries = match registry.catalog().await {
        Ok(entries) => entries,
        Err(error) => return ToolResult::error(format!("cannot list the skills: {error}")),
    };
    if entries.is_empty() {
        // Text rather than an error: an empty catalog is a fact about the deployment, not a failed call,
        // and a model that received an error here would retry the same listing.
        return ToolResult::text("no skills are available in this session");
    }

    let mut out = format!(
        "{} skill{} available",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" }
    );
    for entry in &entries {
        // The provider is shown because a shadowed skill is otherwise invisible: a user who expected their
        // project's `review` and got the built-in one needs to see which source answered.
        let _ = write!(
            out,
            "\n- {}: {} (from `{}`)",
            entry.name, entry.description, entry.provider
        );
    }
    ToolResult::text(out)
}

/// Loads one skill and renders it.
async fn load(name: &str, registry: &Registry) -> ToolResult {
    match registry.load(name).await {
        Ok(definition) => ToolResult::text(render(&definition)),
        Err(SkillError::Unknown { .. }) => {
            // A second lookup is worth its cost here: the correction is the list, and a failed load is not
            // on a hot path.
            let known = registry.catalog().await.map_or_else(
                |_| String::new(),
                |entries| {
                    entries
                        .into_iter()
                        .map(|entry| entry.name)
                        .collect::<Vec<String>>()
                        .join(", ")
                },
            );
            if known.is_empty() {
                ToolResult::error(format!(
                    "no skill is named `{name}`, and no skills are available"
                ))
            } else {
                ToolResult::error(format!(
                    "no skill is named `{name}`; the available skills are: {known}"
                ))
            }
        }
        Err(error) => ToolResult::error(format!("cannot load the skill `{name}`: {error}")),
    }
}

/// Renders a skill for the model.
///
/// The body comes last so that a reader who stops early has already seen what the file list contains,
/// and the body itself is not annotated: it is instructions to follow, not text to comment on.
#[must_use]
fn render(definition: &Definition) -> String {
    let mut out = format!(
        "# {}\n{}\n\n",
        definition.summary.name, definition.summary.description
    );
    if definition.files.is_empty() {
        out.push_str("This skill ships no files.\n\n");
    } else {
        let _ = writeln!(out, "Files ({}):", definition.files.len());
        for (path, contents) in &definition.files {
            // Bytes rather than a line count: the number exists to tell a model whether a file is a template
            // or a one-line note, and no reader counts the lines of a file it has not seen.
            let _ = writeln!(out, "- {path} ({} bytes)", contents.len());
        }
        out.push('\n');
    }
    out.push_str(&definition.body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}
