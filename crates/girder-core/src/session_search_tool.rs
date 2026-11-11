//! The `session_search` tool: finding something that was said in an earlier session.
//!
//! A session's context is finite and a long conversation is compacted, so a model that needs a detail from earlier has
//! nowhere to look. This gives it one: the session logs on disk, searched by content.
//!
//! # Why the result carries positions
//!
//! A hit that named only a session would leave the model to re-read the whole log to find the line. The sequence number
//! is what makes the answer usable, and it is also what a caller passes to a paged read.
//!
//! # Why the truncation is reported
//!
//! "No match" and "I stopped looking" are different facts. A model told the first stops; a model told the second asks a
//! narrower question. The bound is applied while scanning rather than after, so a broad query costs a bounded amount of
//! work.
//!
//! # Why this is separate from `grep`
//!
//! `grep` searches files on disk as files. This searches conversations as conversations: it knows what a message is,
//! filters by role, and reports a session sequence number rather than a byte offset. Folding them together would give
//! `grep` a scheme for reading sessions and this tool a grammar for matching bytes.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use crate::session_query::{self, Kind, Query, Results};
use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};

/// How many hits one call returns before it stops and says so.
pub const DEFAULT_HITS: usize = 50;

/// Builds the `session_search` tool.
///
/// The root is the directory holding session logs, which the tool reads directly: a search that had to go through the
/// session catalog would need that catalog mounted, and a deployment without one would lose the tool entirely.
#[must_use]
pub fn session_search_tool(root: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "session_search",
        "Searches earlier conversations for something that was said",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to look for; omit to match everything" },
                "session": { "type": "string", "description": "Restrict to one session id" },
                "kind": {
                    "type": "string",
                    "enum": ["messages", "tools", "boundaries"],
                    "description": "Restrict to one sort of event"
                },
                "limit": { "type": "number", "description": "Maximum hits to return" }
            }
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let root = root.clone();
            Box::pin(async move { Ok(search(&args, &root)) })
        }),
    )
}

fn search(args: &serde_json::Value, root: &std::path::Path) -> ToolResult {
    let limit = usize::try_from(
        args.get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_HITS as u64),
    )
    .unwrap_or(DEFAULT_HITS)
    .max(1);

    let mut query = Query {
        text: args
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: match args.get("kind").and_then(serde_json::Value::as_str) {
            Some("messages") => Some(Kind::Messages),
            Some("tools") => Some(Kind::Tools),
            Some("boundaries") => Some(Kind::Boundaries),
            _ => None,
        },
        session: args
            .get("session")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        limit: Some(limit),
    };
    // A query with no text matches everything, which is what a caller asking "show me this session" means. Anything
    // else would need a second tool for the same read.
    query.limit = Some(limit);

    let results = session_query::search_files(root, &query);
    ToolResult::text(render(&results, limit))
}

/// Renders a search result for a model.
fn render(results: &Results, limit: usize) -> String {
    if results.hits.is_empty() {
        // The scan count is included: "no match in 0 sessions" and "no match in 40" are different answers, and a
        // model deciding whether to broaden the query needs the difference.
        return format!(
            "no matches ({} session(s) searched)",
            results.sessions_scanned
        );
    }

    let mut out = String::new();
    for hit in &results.hits {
        // The session and sequence are what make a hit locatable; the text is what makes it recognisable.
        let _ = writeln!(
            out,
            "{}({}) {}: {}",
            hit.session,
            hit.seq,
            hit.kind,
            snippet(&session_query::searchable_text(&hit.event))
        );
    }
    if results.truncated {
        let _ = write!(
            out,
            "\n(stopped after {limit} hits across {} session(s))\n",
            results.sessions_scanned
        );
    }
    out
}

/// One line of an event's text, bounded.
///
/// Truncated on a character boundary rather than a byte offset: a cut inside a multi-byte character would produce a
/// replacement character, and the snippet would look like the session was corrupt.
fn snippet(text: &str) -> String {
    const MAX: usize = 160;
    let single_line: String = text
        .chars()
        .map(|character| if character == '\n' { ' ' } else { character })
        .collect();
    let trimmed = single_line.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_owned();
    }
    let cut: String = trimmed.chars().take(MAX).collect();
    format!("{cut}…")
}
