//! Tests for `web_fetch` and `web_search`.
//!
//! The rule both are built around is that a result says what it actually retrieved: a 404 is a page, and a truncated
//! body must say so or a model draws conclusions the evidence does not support.

use std::sync::Arc;

use girder_core::{
    FetchBody, FetchProvider, FetchRequest, FetchResult, SearchProvider, SearchRequest,
    SearchResult, SearchSource, ToolResult, ToolRunContext, Web, WebError, fetch_tool, search_tool,
};

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A provider that answers from a fixed body.
struct Fixed {
    body: FetchBody,
    status: u16,
    truncated: bool,
}

impl FetchProvider for Fixed {
    fn name(&self) -> &'static str {
        "fixed"
    }

    fn fetch(
        &self,
        request: FetchRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> girder_core::BoxFut<Result<FetchResult, WebError>> {
        let url = request.url;
        let body = self.body.clone();
        let status = self.status;
        let truncated = self.truncated;
        Box::pin(async move {
            Ok(FetchResult {
                url,
                status_code: status,
                body,
                truncated,
            })
        })
    }
}

/// A search provider that answers from a fixed list.
struct Searchy {
    sources: Vec<SearchSource>,
    content: Option<String>,
}

impl SearchProvider for Searchy {
    fn name(&self) -> &'static str {
        "searchy"
    }

    fn search(
        &self,
        _request: SearchRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> girder_core::BoxFut<Result<SearchResult, WebError>> {
        let sources = self.sources.clone();
        let content = self.content.clone();
        Box::pin(async move {
            Ok(SearchResult {
                content,
                sources,
                truncated: false,
            })
        })
    }
}

fn web_with_fetch(provider: Fixed) -> Arc<Web> {
    let web = Arc::new(Web::new());
    web.register_fetch(Arc::new(provider)).expect("registered");
    web
}

async fn run_fetch(web: Arc<Web>, args: serde_json::Value) -> ToolResult {
    let tool = fetch_tool(web);
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a fetch is a result")
}

// ---- web_fetch --------------------------------------------------------------------------------

#[tokio::test]
async fn a_text_body_is_returned() {
    let web = web_with_fetch(Fixed {
        body: FetchBody::Text {
            text: "the page".to_owned(),
        },
        status: 200,
        truncated: false,
    });

    let result = run_fetch(web, serde_json::json!({"url": "https://example.com"})).await;

    assert!(!result.is_error);
    let text = text_of(&result);
    assert!(text.contains("the page"), "{text}");
    assert!(text.contains("200"), "the status is shown: {text}");
}

#[tokio::test]
async fn a_non_2xx_response_is_a_result_not_an_error() {
    // A 404 is a successful retrieval of a "not found" page, and a model cannot learn from an error the way it
    // learns from a page that says not found.
    let web = web_with_fetch(Fixed {
        body: FetchBody::Text {
            text: "not found".to_owned(),
        },
        status: 404,
        truncated: false,
    });

    let result = run_fetch(web, serde_json::json!({"url": "https://example.com/x"})).await;

    assert!(!result.is_error, "a 404 must not be a tool failure");
    let text = text_of(&result);
    assert!(text.contains("404"), "{text}");
    assert!(text.contains("not found"), "{text}");
}

#[tokio::test]
async fn a_truncated_body_says_so() {
    // A model told "this is the page" when it has the first half draws conclusions the evidence does not support.
    let web = web_with_fetch(Fixed {
        body: FetchBody::Text {
            text: "the beginning".to_owned(),
        },
        status: 200,
        truncated: true,
    });

    let text = text_of(&run_fetch(web, serde_json::json!({"url": "https://example.com"})).await);

    assert!(text.contains("truncated"), "{text}");
}

#[tokio::test]
async fn a_json_body_is_pretty_printed() {
    let web = web_with_fetch(Fixed {
        body: FetchBody::Json {
            value: serde_json::json!({"a": 1, "b": [2, 3]}),
        },
        status: 200,
        truncated: false,
    });

    let text =
        text_of(&run_fetch(web, serde_json::json!({"url": "https://example.com/api"})).await);

    assert!(text.contains("\"a\""), "{text}");
    assert!(
        text.contains('\n'),
        "printed across lines rather than compact: {text}"
    );
}

#[tokio::test]
async fn a_binary_body_is_described_rather_than_dumped() {
    // A model given bytes would spend its context on something it cannot read.
    let web = web_with_fetch(Fixed {
        body: FetchBody::Binary {
            media_type: "image/png".to_owned(),
            bytes: 1234,
        },
        status: 200,
        truncated: false,
    });

    let text =
        text_of(&run_fetch(web, serde_json::json!({"url": "https://example.com/a.png"})).await);

    assert!(text.contains("image/png"), "{text}");
    assert!(text.contains("1234"), "{text}");
    assert!(text.contains("not shown"), "{text}");
}

#[tokio::test]
async fn a_missing_url_is_refused() {
    let web = web_with_fetch(Fixed {
        body: FetchBody::Text {
            text: "x".to_owned(),
        },
        status: 200,
        truncated: false,
    });

    let result = run_fetch(web, serde_json::json!({})).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("required"));
}

#[tokio::test]
async fn no_fetch_provider_is_reported_as_a_result() {
    // A deployment with no fetch provider: the model needs to know it cannot look things up.
    let web = Arc::new(Web::new());

    let result = run_fetch(web, serde_json::json!({"url": "https://example.com"})).await;

    assert!(result.is_error);
}

// ---- web_search -------------------------------------------------------------------------------

fn web_with_search(provider: Searchy) -> Arc<Web> {
    let web = Arc::new(Web::new());
    web.register_search(Arc::new(provider)).expect("registered");
    web
}

async fn run_search(web: Arc<Web>, args: serde_json::Value) -> ToolResult {
    let tool = search_tool(web);
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a search is a result")
}

#[tokio::test]
async fn a_search_lists_its_sources_with_urls() {
    // The URL is what makes a source checkable.
    let web = web_with_search(Searchy {
        sources: vec![SearchSource {
            url: "https://a.example".to_owned(),
            title: Some("A".to_owned()),
            snippet: Some("about a".to_owned()),
            summary: None,
            published_at: None,
        }],
        content: None,
    });

    let text = text_of(&run_search(web, serde_json::json!({"query": "x"})).await);

    assert!(text.contains("https://a.example"), "{text}");
    assert!(text.contains('A'), "{text}");
    assert!(text.contains("about a"), "{text}");
}

#[tokio::test]
async fn a_source_without_a_title_shows_its_url() {
    let web = web_with_search(Searchy {
        sources: vec![SearchSource {
            url: "https://b.example".to_owned(),
            title: None,
            snippet: None,
            summary: None,
            published_at: None,
        }],
        content: None,
    });

    let text = text_of(&run_search(web, serde_json::json!({"query": "x"})).await);

    assert!(text.contains("https://b.example"), "{text}");
}

#[tokio::test]
async fn a_summary_is_shown_before_the_sources() {
    let web = web_with_search(Searchy {
        sources: Vec::new(),
        content: Some("a summary of the answer".to_owned()),
    });

    let text = text_of(&run_search(web, serde_json::json!({"query": "x"})).await);

    assert!(text.contains("a summary"), "{text}");
    assert!(text.contains("no sources"), "{text}");
}

#[tokio::test]
async fn a_search_with_no_sources_says_so() {
    let web = web_with_search(Searchy {
        sources: Vec::new(),
        content: None,
    });

    let text = text_of(&run_search(web, serde_json::json!({"query": "x"})).await);

    assert!(text.contains("no sources"), "{text}");
}

#[tokio::test]
async fn a_missing_query_is_refused() {
    let web = web_with_search(Searchy {
        sources: Vec::new(),
        content: None,
    });

    let result = run_search(web, serde_json::json!({})).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("required"));
}

#[tokio::test]
async fn no_search_provider_is_reported_as_a_result() {
    let web = Arc::new(Web::new());

    let result = run_search(web, serde_json::json!({"query": "x"})).await;

    assert!(result.is_error);
}
