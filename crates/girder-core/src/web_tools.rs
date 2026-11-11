//! The web tools: `web_fetch` and `web_search`.
//!
//! A model that can only read the local filesystem cannot answer a question about a library's current API. These two
//! tools let it look something up, and both are built around one rule: **say what you actually retrieved**.
//!
//! # Why a non-2xx response is a result and not an error
//!
//! A 404 is a successful retrieval of a "not found" page. Reporting it as a tool failure would make the model retry
//! the same URL, and the model cannot learn from an error the way it learns from a page that says "not found".
//!
//! # Why the truncation flag is passed through rather than summarised
//!
//! A model told "this is the page" when it has the first half draws conclusions the evidence does not support. The
//! flag comes from the provider, which is the only layer that knows whether it stopped reading.
//!
//! # Why the body is presented as text
//!
//! A model reads text. Returning HTML would spend its context on markup, and returning bytes would be unreadable
//! entirely. The provider already extracts text, and this presents what it extracted.

use std::sync::Arc;

use std::fmt::Write as _;

use serde_json::json;

use crate::tools::{ToolDefinition, ToolResult, ToolRunContext};
use crate::web::{FetchBody, FetchRequest, SearchRequest, Web, WebError};

/// How many sources a search returns when the caller names no bound.
///
/// 8: enough to see a range of answers and few enough that the model can read all of them.
pub const DEFAULT_SOURCES: usize = 8;

/// Builds the `web_fetch` tool.
#[must_use]
pub fn fetch_tool(web: Arc<Web>) -> ToolDefinition {
    ToolDefinition::new(
        "web_fetch",
        "Retrieves a URL and returns its text",
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The absolute URL to retrieve" }
            },
            "required": ["url"]
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let web = Arc::clone(&web);
            Box::pin(async move { fetch(&args, &web).await })
        }),
    )
}

async fn fetch(args: &serde_json::Value, web: &Web) -> Result<ToolResult, crate::tools::ToolError> {
    let url = args
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if url.trim().is_empty() {
        return Ok(ToolResult::error("`url` is required"));
    }

    match web.fetch(FetchRequest::new(url)).await {
        Ok(result) => {
            let mut out = String::new();
            let _ = write!(out, "{} (status {})\n\n", result.url, result.status_code);
            match &result.body {
                FetchBody::Text { text } => out.push_str(text),
                // JSON is text a model can read, and pretty-printed rather than compact so a nested structure is
                // followable at a glance.
                FetchBody::Json { value } => {
                    out.push_str(&serde_json::to_string_pretty(value).unwrap_or_default());
                }
                // A body the provider could not decode as text is described rather than dumped: a model given bytes
                // would spend its context on something it cannot read.
                FetchBody::Binary { media_type, bytes } => {
                    let _ = write!(
                        out,
                        "(a {media_type} body of {bytes} bytes, not shown as text)"
                    );
                }
            }
            if result.truncated {
                // Said explicitly: a model told "this is the page" when it has the first half draws conclusions the
                // evidence does not support.
                out.push_str("\n\n(truncated: this is the beginning of the response)");
            }
            Ok(ToolResult::text(out))
        }
        Err(error) => Ok(ToolResult::error(describe(&error))),
    }
}

/// Builds the `web_search` tool.
#[must_use]
pub fn search_tool(web: Arc<Web>) -> ToolDefinition {
    ToolDefinition::new(
        "web_search",
        "Searches the web and returns sources",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for" },
                "limit": { "type": "number", "description": "Maximum sources to return" }
            },
            "required": ["query"]
        }),
        Arc::new(move |args: serde_json::Value, _ctx: ToolRunContext| {
            let web = Arc::clone(&web);
            Box::pin(async move { search(&args, &web).await })
        }),
    )
}

async fn search(
    args: &serde_json::Value,
    web: &Web,
) -> Result<ToolResult, crate::tools::ToolError> {
    let query = args
        .get("query")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if query.trim().is_empty() {
        return Ok(ToolResult::error("`query` is required"));
    }
    let limit = usize::try_from(
        args.get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_SOURCES as u64),
    )
    .unwrap_or(DEFAULT_SOURCES)
    .max(1);

    let mut request = SearchRequest::new(query);
    request.max_results = Some(limit);

    match web.search(request).await {
        Ok(result) => {
            let mut out = String::new();
            if let Some(content) = &result.content {
                out.push_str(content);
                out.push_str("\n\n");
            }
            if result.sources.is_empty() {
                out.push_str("(no sources)");
            } else {
                for source in &result.sources {
                    // The URL is what makes a source checkable, so it is always shown; the title is the provider's
                    // and falls back to the URL when absent.
                    let _ = writeln!(
                        out,
                        "- {} — {}",
                        source.title.as_deref().unwrap_or(&source.url),
                        source.url
                    );
                    if let Some(snippet) = &source.snippet {
                        let _ = writeln!(out, "  {snippet}");
                    }
                }
            }
            if result.truncated {
                // The bound was applied by the seam, so a model that received exactly `limit` would otherwise not
                // know more existed.
                let _ = write!(out, "\n(stopped after {limit} sources)");
            }
            Ok(ToolResult::text(out))
        }
        Err(error) => Ok(ToolResult::error(describe(&error))),
    }
}

/// A one-line explanation of a web failure.
///
/// The variants matter to a model deciding what to do next: no provider means the deployment cannot search, a
/// provider failure means it tried and something broke, and the two call for different answers.
fn describe(error: &WebError) -> String {
    error.to_string()
}
