//! The discovery tools: `glob` and `grep`.
//!
//! A coding agent spends much of its time looking for things: which file has this name, where is this symbol used.
//! These two tools answer those questions, and both have a property that matters more than speed — a result that says
//! it is complete must be complete.
//!
//! # Why a missing match list is different from a truncated one
//!
//! "No file matches" and "I stopped looking" are different facts. A model told the first stops; a model told the
//! second asks a narrower question. Both tools therefore bound their output and say so, rather than silently capping.
//!
//! # Why `grep` prefers ripgrep and falls back
//!
//! Ripgrep is fast, honours `.gitignore`, and is what a person expects. It is not always installed, so the fallback
//! walks the tree itself: slower, but a tool that only works where one binary happens to exist is worse than a slow
//! one.
//!
//! # Why patterns are matched case-insensitively by default
//!
//! A person searching for `SessionStore` usually means the concept, not the exact spelling. Both tools take a flag to
//! make it exact, so the default serves the common case and the flag serves the careful one.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// How many results one call returns before it stops and says so.
///
/// 200: enough to see a pattern across a project and far below the point where a model can read the list.
pub const DEFAULT_LIMIT: usize = 200;

/// How deep a walk descends before it gives up.
///
/// A guard against a symlink loop, which would otherwise run until the limit happened to stop it.
const MAX_DEPTH: usize = 32;

/// Whether a directory should not be walked.
///
/// The set every project has and no one wants searched. Ripgrep applies the same exclusions through `.gitignore`,
/// and the fallback needs them spelled out.
fn is_ignored(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".venv"
            | "__pycache__"
            | ".mypy_cache"
    )
}

/// Builds the `glob` tool.
#[must_use]
pub fn glob_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "glob",
        "Finds files whose path matches a glob pattern",
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, such as `**/*.rs` or `src/**/*.ts`"
                },
                "limit": { "type": "number", "description": "Maximum files to return" }
            },
            "required": ["pattern"]
        }),
        std::sync::Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { Ok(glob(&args, &workspace)) })
        }),
    )
}

fn glob(args: &serde_json::Value, workspace: &Path) -> ToolResult {
    let pattern = args
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if pattern.trim().is_empty() {
        return ToolResult::error("`pattern` is required");
    }
    let limit = usize::try_from(
        args.get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_LIMIT as u64),
    )
    .unwrap_or(DEFAULT_LIMIT)
    .max(1);

    // The suffix after the last `/` is what each name is matched against, and the prefix is where to start walking:
    // a pattern that began with a literal directory would otherwise be matched against every path in the tree.
    let (start, matcher) = split_pattern(workspace, pattern);

    let mut found = Vec::new();
    let mut truncated = false;
    walk(&start, &matcher, 0, &mut |path| {
        if found.len() >= limit {
            truncated = true;
            return;
        }
        found.push(path.to_path_buf());
    });

    if found.is_empty() {
        return ToolResult::text(format!("no files match `{pattern}`"));
    }

    let mut out = String::new();
    for path in &found {
        let shown = crate::workspace_path::display(Some(workspace), path);
        out.push_str(&shown);
        out.push('\n');
    }
    if truncated {
        // Said explicitly: a model that believed this was the whole list would draw a conclusion it has not earned.
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "\n(stopped after {limit} files; narrow the pattern to see the rest)\n"
        );
    }
    ToolResult::text(out)
}

/// Splits a glob into a starting directory and a matcher for the final path component.
fn split_pattern(workspace: &Path, pattern: &str) -> (PathBuf, GlobMatcher) {
    let normalized = pattern.replace('\\', "/");
    let (directory, name) = match normalized.rsplit_once('/') {
        // A pattern like `**/*.rs` starts at the workspace: the `**` means "any depth" and a literal join would ask
        // for a directory named `**`.
        Some((prefix, name)) if !prefix.is_empty() && prefix != "**" => {
            let prefix = prefix.trim_end_matches("/**").trim_end_matches("**");
            let prefix = prefix.trim_end_matches('/');
            if prefix.is_empty() {
                (workspace.to_path_buf(), name)
            } else {
                (workspace.join(prefix), name)
            }
        }
        // A bare pattern has no directory, so the walk covers the workspace and the whole name is the matcher.
        _ => (workspace.to_path_buf(), normalized.as_str()),
    };
    (directory, GlobMatcher::new(name))
}

/// A glob matcher for one path component.
#[derive(Debug)]
struct GlobMatcher {
    pattern: String,
}

impl GlobMatcher {
    fn new(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_lowercase(),
        }
    }

    /// Whether a name matches.
    ///
    /// Supports `*`, `?`, and a literal prefix or suffix, which is what a file search actually uses. A full glob
    /// grammar with character classes would be more correct and is not worth the parser for the patterns people
    /// write.
    fn matches(&self, name: &str) -> bool {
        if self.pattern == "*" || self.pattern.is_empty() {
            return true;
        }
        let name = name.to_lowercase();
        if let Some(suffix) = self.pattern.strip_prefix('*') {
            return if suffix.contains('*') {
                // A middle wildcard falls back to a substring test rather than a wrong answer.
                let parts: Vec<&str> = suffix.split('*').filter(|p| !p.is_empty()).collect();
                parts.iter().all(|part| name.contains(part))
            } else {
                name.ends_with(suffix)
            };
        }
        if let Some(prefix) = self.pattern.strip_suffix('*') {
            return name.starts_with(prefix);
        }
        if self.pattern.contains('*') {
            let parts: Vec<&str> = self.pattern.split('*').collect();
            let mut rest = name.as_str();
            for (index, part) in parts.iter().enumerate() {
                if part.is_empty() {
                    continue;
                }
                match rest.find(part) {
                    Some(at) => {
                        if index == 0 && at != 0 {
                            return false;
                        }
                        rest = &rest[at + part.len()..];
                    }
                    None => return false,
                }
            }
            return parts
                .last()
                .is_none_or(|last| last.is_empty() || rest.is_empty() || name.ends_with(last));
        }
        self.pattern == name
    }
}

/// Walks a directory, calling `visit` for each matching file.
fn walk(directory: &Path, matcher: &GlobMatcher, depth: usize, visit: &mut dyn FnMut(&Path)) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_ignored(&name) {
            continue;
        }
        if path.is_dir() {
            walk(&path, matcher, depth + 1, visit);
        } else if matcher.matches(&name) {
            visit(&path);
        }
    }
}

/// Builds the `grep` tool.
#[must_use]
pub fn grep_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "grep",
        "Searches file contents for a pattern",
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Text to search for" },
                "path": { "type": "string", "description": "File or directory to search; defaults to the workspace" },
                "case_sensitive": { "type": "boolean", "description": "Match exactly; off by default" },
                "limit": { "type": "number", "description": "Maximum matching lines to return" }
            },
            "required": ["pattern"]
        }),
        std::sync::Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { Ok(grep(&args, &workspace)) })
        }),
    )
}

fn grep(args: &serde_json::Value, workspace: &Path) -> ToolResult {
    let pattern = args
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if pattern.trim().is_empty() {
        return ToolResult::error("`pattern` is required");
    }
    let case_sensitive = args
        .get("case_sensitive")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let limit = usize::try_from(
        args.get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_LIMIT as u64),
    )
    .unwrap_or(DEFAULT_LIMIT)
    .max(1);
    let root = match args.get("path").and_then(serde_json::Value::as_str) {
        Some(path) => crate::workspace_path::resolve(Some(workspace), path),
        None => workspace.to_path_buf(),
    };

    // Ripgrep when it is installed: it honours `.gitignore`, which is the difference between searching a project and
    // searching its build output.
    if let Some(result) = grep_with_ripgrep(&root, pattern, case_sensitive, limit) {
        return result;
    }
    grep_in_process(&root, workspace, pattern, case_sensitive, limit)
}

/// Searches with ripgrep, returning `None` when the binary is not available.
fn grep_with_ripgrep(
    root: &Path,
    pattern: &str,
    case_sensitive: bool,
    limit: usize,
) -> Option<ToolResult> {
    let mut command = std::process::Command::new("rg");
    command
        .arg("--line-number")
        .arg("--no-heading")
        .arg("--color")
        .arg("never")
        // One file over the limit is enough to know the result is partial, so no file is read past it.
        .arg("--max-count")
        .arg(limit.to_string())
        .arg("--")
        .arg(pattern)
        .arg(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if !case_sensitive {
        command.arg("-i");
    }

    let output = command.output().ok()?;
    // Exit code 2 means ripgrep could not run (a bad pattern, a missing path); falling back is better than
    // reporting that as "no matches".
    if output.status.code() == Some(2) {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    let truncated = lines.len() > limit;
    lines.truncate(limit);

    if lines.is_empty() {
        return Some(ToolResult::text(format!("no matches for `{pattern}`")));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    if truncated {
        use std::fmt::Write as _;
        let _ = write!(out, "\n(stopped after {limit} matches)\n");
    }
    Some(ToolResult::text(out))
}

/// Searches by walking, for a host with no ripgrep.
fn grep_in_process(
    root: &Path,
    workspace: &Path,
    pattern: &str,
    case_sensitive: bool,
    limit: usize,
) -> ToolResult {
    let needle = if case_sensitive {
        pattern.to_owned()
    } else {
        pattern.to_lowercase()
    };

    let mut matches: Vec<String> = Vec::new();
    let mut truncated = false;

    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
    } else {
        walk(root, &GlobMatcher::new("*"), 0, &mut |path| {
            files.push(path.to_path_buf());
        });
    }

    for file in files {
        if matches.len() >= limit {
            truncated = true;
            break;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            // A binary or unreadable file is skipped rather than reported: the caller asked where a pattern appears,
            // not which files could not be decoded.
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            if matches.len() >= limit {
                truncated = true;
                break;
            }
            let haystack = if case_sensitive {
                line.to_owned()
            } else {
                line.to_lowercase()
            };
            if haystack.contains(&needle) {
                matches.push(format!(
                    "{}:{}:{}",
                    crate::workspace_path::display(Some(workspace), &file),
                    index + 1,
                    line
                ));
            }
        }
    }

    if matches.is_empty() {
        return ToolResult::text(format!("no matches for `{pattern}`"));
    }
    let mut out = matches.join("\n");
    out.push('\n');
    if truncated {
        use std::fmt::Write as _;
        let _ = write!(out, "\n(stopped after {limit} matches)\n");
    }
    ToolResult::text(out)
}
