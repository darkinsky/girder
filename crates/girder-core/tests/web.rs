//! Tests for the web capability.
//!
//! Two properties carry the design: provider selection never depends on registration order, and the
//! result bound is enforced by the seam rather than trusted to a provider.

use std::sync::Arc;

use girder_core::{
    FetchBody, FetchProvider, FetchRequest, FetchResult, SearchProvider, SearchRequest,
    SearchResult, SearchSource, Web, WebError,
};
use tokio_util::sync::CancellationToken;

// ---- test providers ---------------------------------------------------------------------------

struct FixedSearch {
    name: String,
    sources: Vec<SearchSource>,
}

impl SearchProvider for FixedSearch {
    fn name(&self) -> &str {
        &self.name
    }

    fn search(
        &self,
        _request: SearchRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<SearchResult, WebError>> {
        let sources = self.sources.clone();
        Box::pin(async move {
            Ok(SearchResult {
                content: None,
                sources,
                truncated: false,
            })
        })
    }
}

struct FixedFetch {
    name: String,
    status: u16,
}

impl FetchProvider for FixedFetch {
    fn name(&self) -> &str {
        &self.name
    }

    fn fetch(
        &self,
        request: FetchRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<FetchResult, WebError>> {
        let status = self.status;
        Box::pin(async move {
            Ok(FetchResult {
                url: request.url,
                status_code: status,
                body: FetchBody::Text {
                    text: "body".to_owned(),
                },
                truncated: false,
            })
        })
    }
}

fn search_provider(name: &str, count: usize) -> Arc<dyn SearchProvider> {
    Arc::new(FixedSearch {
        name: name.to_owned(),
        sources: (0..count)
            .map(|index| SearchSource::url(format!("https://example.com/{index}")))
            .collect(),
    })
}

fn fetch_provider(name: &str, status: u16) -> Arc<dyn FetchProvider> {
    Arc::new(FixedFetch {
        name: name.to_owned(),
        status,
    })
}

// ---- sources ----------------------------------------------------------------------------------

#[test]
fn a_url_only_source_displays_its_host() {
    // Some providers return URL-only citations; inventing a title would make the seam report
    // something the provider never said.
    let source = SearchSource::url("https://example.com/a/b?q=1");

    assert_eq!(source.display_title(), "example.com");
}

#[test]
fn a_titled_source_displays_its_title() {
    let mut source = SearchSource::url("https://example.com/a");
    source.title = Some("A page".to_owned());

    assert_eq!(source.display_title(), "A page");
}

#[test]
fn a_host_strips_userinfo_and_port() {
    // Neither belongs in a display label.
    let source = SearchSource::url("https://user:pass@example.com:8443/path");

    assert_eq!(source.display_title(), "example.com");
}

#[test]
fn a_url_without_a_scheme_still_yields_a_host() {
    let source = SearchSource::url("example.com/path");

    assert_eq!(source.display_title(), "example.com");
}

// ---- the result bound -------------------------------------------------------------------------

#[test]
fn a_bound_truncates_and_records_it() {
    // A model told "these are all the sources" when they are not draws a conclusion the evidence
    // does not support.
    let result = SearchResult {
        content: None,
        sources: (0..5)
            .map(|index| SearchSource::url(format!("https://e.com/{index}")))
            .collect(),
        truncated: false,
    };

    let bounded = result.bounded(Some(3));

    assert_eq!(bounded.sources.len(), 3);
    assert!(bounded.truncated);
}

#[test]
fn a_bound_larger_than_the_result_changes_nothing() {
    let result = SearchResult {
        content: None,
        sources: vec![SearchSource::url("https://e.com")],
        truncated: false,
    };

    let bounded = result.bounded(Some(10));

    assert_eq!(bounded.sources.len(), 1);
    assert!(!bounded.truncated);
}

#[test]
fn no_bound_leaves_the_result_alone() {
    let result = SearchResult {
        content: None,
        sources: (0..50)
            .map(|index| SearchSource::url(format!("https://e.com/{index}")))
            .collect(),
        truncated: false,
    };

    assert_eq!(result.bounded(None).sources.len(), 50);
}

#[tokio::test]
async fn the_seam_enforces_the_bound_even_when_the_provider_ignores_it() {
    // A provider that ignored `max_results` would otherwise hand a model an unbounded list.
    let web = Web::new();
    web.register_search(search_provider("p", 100))
        .expect("registered");

    let result = web
        .search(SearchRequest::new("q").with_max_results(4))
        .await
        .expect("searched");

    assert_eq!(result.sources.len(), 4);
    assert!(result.truncated, "the seam must say it dropped sources");
}

// ---- selection --------------------------------------------------------------------------------

#[tokio::test]
async fn no_provider_is_an_error() {
    let web = Web::new();
    let error = web
        .search(SearchRequest::new("q"))
        .await
        .expect_err("no provider");

    assert_eq!(
        error,
        WebError::NoProvider {
            operation: "search"
        }
    );
}

#[tokio::test]
async fn exactly_one_provider_is_used_without_configuration() {
    let web = Web::new();
    web.register_search(search_provider("only", 1))
        .expect("registered");

    let result = web.search(SearchRequest::new("q")).await.expect("searched");

    assert_eq!(result.sources.len(), 1);
}

#[tokio::test]
async fn two_providers_without_a_configured_one_is_ambiguous() {
    // Resolving by registration order would make which provider answered depend on load order, and a
    // deployment would change behaviour without changing configuration.
    let web = Web::new();
    web.register_search(search_provider("a", 1))
        .expect("registered");
    web.register_search(search_provider("b", 1))
        .expect("registered");

    let error = web
        .search(SearchRequest::new("q"))
        .await
        .expect_err("ambiguous");

    let WebError::Ambiguous { operation, count } = error else {
        panic!("expected Ambiguous, got {error:?}");
    };
    assert_eq!((operation, count), ("search", 2));
}

#[tokio::test]
async fn configuring_one_of_two_resolves_the_ambiguity() {
    let web = Web::new();
    web.register_search(search_provider("a", 1))
        .expect("registered");
    web.register_search(search_provider("b", 3))
        .expect("registered");
    web.configure_search("b");

    let result = web.search(SearchRequest::new("q")).await.expect("searched");

    assert_eq!(result.sources.len(), 3, "the configured provider answered");
}

#[tokio::test]
async fn configuring_an_absent_provider_is_an_error() {
    // A configured name that does not exist is a configuration mistake, not a reason to fall back:
    // falling back would silently answer with a provider the deployment did not pick.
    let web = Web::new();
    web.register_search(search_provider("a", 1))
        .expect("registered");
    web.configure_search("nope");

    let error = web
        .search(SearchRequest::new("q"))
        .await
        .expect_err("absent");

    assert_eq!(
        error,
        WebError::NoProvider {
            operation: "search"
        }
    );
}

#[test]
fn a_duplicate_provider_name_is_refused() {
    let web = Web::new();
    web.register_search(search_provider("a", 1))
        .expect("registered");

    let error = web
        .register_search(search_provider("a", 2))
        .expect_err("duplicate");

    assert_eq!(error, "a");
}

#[test]
fn provider_names_are_sorted() {
    let web = Web::new();
    for name in ["zeta", "alpha"] {
        web.register_search(search_provider(name, 0))
            .expect("registered");
    }

    assert_eq!(web.search_names(), vec!["alpha", "zeta"]);
}

// ---- fetch ------------------------------------------------------------------------------------

#[tokio::test]
async fn a_fetch_reports_its_status() {
    let web = Web::new();
    web.register_fetch(fetch_provider("p", 200))
        .expect("registered");

    let result = web
        .fetch(FetchRequest::new("https://example.com"))
        .await
        .expect("fetched");

    assert_eq!(result.status_code, 200);
    assert!(result.is_success());
}

#[tokio::test]
async fn a_not_found_is_a_result_not_an_error() {
    // A model told "error" cannot tell "the page says not found" from "I could not reach the server",
    // and the two call for different next steps.
    let web = Web::new();
    web.register_fetch(fetch_provider("p", 404))
        .expect("registered");

    let result = web
        .fetch(FetchRequest::new("https://example.com/missing"))
        .await
        .expect("retrieved");

    assert_eq!(result.status_code, 404);
    assert!(
        !result.is_success(),
        "the caller can see it was not a success"
    );
}

#[tokio::test]
async fn the_fetch_and_search_providers_are_independent() {
    // A deployment may have a search provider and no fetch provider; requiring both would make a
    // partial capability impossible.
    let web = Web::new();
    web.register_search(search_provider("s", 1))
        .expect("registered");

    assert!(web.search(SearchRequest::new("q")).await.is_ok());
    assert!(web.fetch(FetchRequest::new("https://e.com")).await.is_err());
}

// ---- the body union ---------------------------------------------------------------------------

#[test]
fn a_body_reports_its_kind() {
    assert_eq!(
        FetchBody::Text {
            text: String::new()
        }
        .kind(),
        "text"
    );
    assert_eq!(
        FetchBody::Json {
            value: serde_json::Value::Null
        }
        .kind(),
        "json"
    );
    assert_eq!(
        FetchBody::Binary {
            media_type: "image/png".to_owned(),
            bytes: 10
        }
        .kind(),
        "binary"
    );
}

#[test]
fn a_binary_body_carries_no_bytes() {
    // A model cannot read them, and carrying them would fill a context window with noise.
    let body = FetchBody::Binary {
        media_type: "image/png".to_owned(),
        bytes: 1234,
    };

    let FetchBody::Binary { bytes, .. } = body else {
        panic!("expected binary");
    };
    assert_eq!(bytes, 1234);
}

#[test]
fn the_debug_form_lists_provider_names_not_handles() {
    let web = Web::new();
    web.register_search(search_provider("alpha", 0))
        .expect("registered");

    let text = format!("{web:?}");
    assert!(text.contains("alpha"));
    assert!(text.contains("Web"));
}
