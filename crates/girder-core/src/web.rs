//! The web capability: search and fetch, behind one provider seam.
//!
//! This is not a browser UI. It is the model's ability to look something up and read a page, which is
//! why it belongs in the backend scope rather than the frontend.
//!
//! # Why search and fetch share one seam
//!
//! They are different operations — one asks a search service, one retrieves a URL — but they share the
//! things that are easy to get wrong when duplicated: provider selection, cancellation, error shape,
//! and "which provider is configured". Two seams would mean two answers to each of those, and the
//! answers would drift.
//!
//! # Why a non-2xx fetch is a result rather than an error
//!
//! A 404 is a fact about the resource, not a failure to retrieve it. A model told "error" cannot
//! distinguish "the page says not found" from "I could not reach the server", and the two call for
//! different next steps. [`WebError`] is reserved for failures to retrieve or represent at all.
//!
//! # Why the body is a closed union
//!
//! A provider decodes the kind and a consumer renders it. If the union were open, a provider could
//! introduce a kind no consumer handles and the failure would appear at render time in front of a
//! user. Closed means adding one breaks compilation at every consumer until each is handled.

use std::sync::Arc;

use crate::scope::BoxFut;

/// One citeable source.
///
/// `url` is required and everything else is optional because not every provider returns them.
/// Inventing a title to fill the gap would make the seam report something the provider never said;
/// a consumer renders `title`, or the URL's host when absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchSource {
    /// The source's URL.
    pub url: String,
    /// Its title, when the provider supplied one.
    pub title: Option<String>,
    /// A short excerpt suited to display.
    pub snippet: Option<String>,
    /// A longer excerpt suited to a model reading it; the consumer bounds its length.
    pub summary: Option<String>,
    /// A provider-supplied publication or crawl timestamp.
    pub published_at: Option<String>,
}

impl SearchSource {
    /// Builds a URL-only source, which some providers return exclusively.
    #[must_use]
    pub fn url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: None,
            snippet: None,
            summary: None,
            published_at: None,
        }
    }

    /// The label a consumer displays: the title, or the URL's host.
    ///
    /// A host rather than the whole URL because a citation list with full URLs is unreadable, and
    /// because the host is what a reader recognises.
    #[must_use]
    pub fn display_title(&self) -> &str {
        self.title.as_deref().unwrap_or_else(|| host_of(&self.url))
    }
}

/// Extracts a host from a URL without a URL parser.
///
/// Deliberately simple: this is for display only, so a wrong answer produces a slightly wrong label
/// rather than a failed request. A real parser would need a dependency for a case that cannot matter.
fn host_of(url: &str) -> &str {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    // Strip userinfo and port: neither belongs in a display label.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host)
}

/// What one search provider is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRequest {
    /// The query. One per request; a consumer that wants several issues several.
    pub query: String,
    /// An upper bound on returned sources.
    ///
    /// Enforced by the seam rather than only passed to the provider: a provider that ignores it would
    /// otherwise return an unbounded list to a model with a bounded window.
    pub max_results: Option<usize>,
}

impl SearchRequest {
    /// Builds a request for one query.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            max_results: None,
        }
    }

    /// Bounds the result count.
    #[must_use]
    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = Some(max);
        self
    }
}

/// A normalized search outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    /// Provider-generated answer text or summary, when it produced one.
    pub content: Option<String>,
    /// Citeable sources, already truncated to the request's bound.
    pub sources: Vec<SearchSource>,
    /// Whether the seam dropped sources to honour the bound.
    ///
    /// Recorded rather than inferred: a consumer that got exactly the bound cannot tell whether it
    /// was truncated, and a model told "these are all the sources" when they are not draws a
    /// conclusion the evidence does not support.
    pub truncated: bool,
}

impl SearchResult {
    /// Truncates to `bound`, recording whether anything was dropped.
    ///
    /// The single place the bound is enforced, so a provider that ignored `max_results` cannot make
    /// the seam lie about it.
    #[must_use]
    pub fn bounded(mut self, bound: Option<usize>) -> Self {
        if let Some(bound) = bound
            && self.sources.len() > bound
        {
            self.sources.truncate(bound);
            self.truncated = true;
        }
        self
    }
}

/// What one fetch provider is asked.
///
/// No timeout, format, or extraction fields: cancellation is an execution argument, and presentation
/// belongs to the consumer. A request that carried them would give every provider a second contract
/// to honour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchRequest {
    /// The URL to retrieve.
    pub url: String,
}

impl FetchRequest {
    /// Builds a request for one URL.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

/// The decoded body of a fetched resource.
///
/// A closed union: the provider decodes the kind and the consumer renders it, so a new kind is a
/// coordinated change rather than a plugin extension. A consumer's `match` must handle every arm, and
/// adding one breaks compilation until it does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchBody {
    /// Plain text or HTML reduced to text.
    Text {
        /// The text.
        text: String,
    },
    /// A JSON document, already parsed.
    Json {
        /// The value.
        value: serde_json::Value,
    },
    /// Binary content, with its media type and length but not its bytes.
    ///
    /// The bytes are absent rather than truncated: a model cannot read them, and carrying them would
    /// fill a context window with noise.
    Binary {
        /// The media type the provider detected.
        media_type: String,
        /// How many bytes the resource holds.
        bytes: u64,
    },
}

impl FetchBody {
    /// The kind name, for a log or a client.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Json { .. } => "json",
            Self::Binary { .. } => "binary",
        }
    }
}

/// A normalized fetch outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchResult {
    /// The final URL after allowed redirects.
    pub url: String,
    /// The HTTP status, including a non-2xx one.
    pub status_code: u16,
    /// The decoded body.
    pub body: FetchBody,
    /// Whether the provider capped the body.
    pub truncated: bool,
}

impl FetchResult {
    /// Whether the resource was retrieved successfully.
    ///
    /// Separate from "the request succeeded": a 404 arrived, which is a successful retrieval of a
    /// page that says it does not exist.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status_code)
    }
}

/// Why a web operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebError {
    /// No provider can serve the request.
    #[error("no web provider for {operation}")]
    NoProvider {
        /// Which operation: `search` or `fetch`.
        operation: &'static str,
    },

    /// More than one provider is usable and none was configured.
    ///
    /// Refused rather than resolved by registration order: which provider answered would then depend
    /// on load order, and a deployment would change behaviour without changing configuration.
    #[error("{count} providers can serve {operation}; configure one")]
    Ambiguous {
        /// Which operation.
        operation: &'static str,
        /// How many were usable.
        count: usize,
    },

    /// The provider failed.
    #[error("{provider} failed: {reason}")]
    Provider {
        /// Which provider.
        provider: String,
        /// Why.
        reason: String,
    },

    /// The URL is not one this seam will retrieve.
    #[error("`{url}` is not a fetchable URL")]
    InvalidUrl {
        /// The URL.
        url: String,
    },

    /// The caller cancelled.
    #[error("cancelled")]
    Cancelled,
}

/// A search-capable backend.
pub trait SearchProvider: Send + Sync {
    /// Its registered name.
    fn name(&self) -> &str;

    /// Runs one search.
    fn search(
        &self,
        request: SearchRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFut<Result<SearchResult, WebError>>;
}

/// A fetch-capable backend.
pub trait FetchProvider: Send + Sync {
    /// Its registered name.
    fn name(&self) -> &str;

    /// Retrieves one URL.
    fn fetch(
        &self,
        request: FetchRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFut<Result<FetchResult, WebError>>;
}

/// The web capability: registered providers and the selection rule.
///
/// A `Mutex` rather than lock-free: registration happens once at boot and selection reads the whole
/// list, so the contention is nil and the invariant — "at most one configured provider" — is easier
/// to hold with a lock.
#[derive(Default)]
pub struct Web {
    search: std::sync::Mutex<Vec<Arc<dyn SearchProvider>>>,
    fetch: std::sync::Mutex<Vec<Arc<dyn FetchProvider>>>,
    configured_search: std::sync::Mutex<Option<String>>,
    configured_fetch: std::sync::Mutex<Option<String>>,
}

impl std::fmt::Debug for Web {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Web")
            .field("search", &self.search_names())
            .field("fetch", &self.fetch_names())
            .finish()
    }
}

impl Web {
    /// Creates an empty capability.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a search provider.
    ///
    /// # Errors
    ///
    /// Returns the name when it is taken. Replacing silently would route a query to a provider the
    /// deployment did not choose.
    pub fn register_search(&self, provider: Arc<dyn SearchProvider>) -> Result<(), String> {
        let name = provider.name().to_owned();
        let mut providers = self
            .search
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if providers.iter().any(|existing| existing.name() == name) {
            return Err(name);
        }
        providers.push(provider);
        Ok(())
    }

    /// Registers a fetch provider.
    ///
    /// # Errors
    ///
    /// Returns the name when it is taken.
    pub fn register_fetch(&self, provider: Arc<dyn FetchProvider>) -> Result<(), String> {
        let name = provider.name().to_owned();
        let mut providers = self
            .fetch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if providers.iter().any(|existing| existing.name() == name) {
            return Err(name);
        }
        providers.push(provider);
        Ok(())
    }

    /// Names one provider as the configured one for searches.
    pub fn configure_search(&self, name: impl Into<String>) {
        *self
            .configured_search
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name.into());
    }

    /// Names one provider as the configured one for fetches.
    pub fn configure_fetch(&self, name: impl Into<String>) {
        *self
            .configured_fetch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name.into());
    }

    /// The registered search provider names, sorted.
    #[must_use]
    pub fn search_names(&self) -> Vec<String> {
        let providers = self
            .search
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut names: Vec<String> = providers.iter().map(|p| p.name().to_owned()).collect();
        names.sort();
        names
    }

    /// The registered fetch provider names, sorted.
    #[must_use]
    pub fn fetch_names(&self) -> Vec<String> {
        let providers = self
            .fetch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut names: Vec<String> = providers.iter().map(|p| p.name().to_owned()).collect();
        names.sort();
        names
    }

    /// Runs one search through the selected provider.
    ///
    /// # Errors
    ///
    /// Returns [`WebError::NoProvider`], [`WebError::Ambiguous`], or whatever the provider reports.
    pub async fn search(&self, request: SearchRequest) -> Result<SearchResult, WebError> {
        let provider = self.select_search()?;
        let bound = request.max_results;
        let result = provider
            .search(request, tokio_util::sync::CancellationToken::new())
            .await?;
        // The bound is enforced here, not only inside the provider: a provider that ignored it would
        // otherwise hand a model an unbounded list.
        Ok(result.bounded(bound))
    }

    /// Runs one fetch through the selected provider.
    ///
    /// # Errors
    ///
    /// Returns [`WebError::NoProvider`], [`WebError::Ambiguous`], or whatever the provider reports.
    pub async fn fetch(&self, request: FetchRequest) -> Result<FetchResult, WebError> {
        let provider = self.select_fetch()?;
        provider
            .fetch(request, tokio_util::sync::CancellationToken::new())
            .await
    }

    /// Chooses the search provider.
    fn select_search(&self) -> Result<Arc<dyn SearchProvider>, WebError> {
        let providers = self
            .search
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let configured = self
            .configured_search
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(name) = configured {
            return providers
                .iter()
                .find(|provider| provider.name() == name)
                .cloned()
                .ok_or(WebError::NoProvider {
                    operation: "search",
                });
        }
        match providers.len() {
            0 => Err(WebError::NoProvider {
                operation: "search",
            }),
            1 => Ok(Arc::clone(&providers[0])),
            count => Err(WebError::Ambiguous {
                operation: "search",
                count,
            }),
        }
    }

    /// Chooses the fetch provider.
    fn select_fetch(&self) -> Result<Arc<dyn FetchProvider>, WebError> {
        let providers = self
            .fetch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let configured = self
            .configured_fetch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(name) = configured {
            return providers
                .iter()
                .find(|provider| provider.name() == name)
                .cloned()
                .ok_or(WebError::NoProvider { operation: "fetch" });
        }
        match providers.len() {
            0 => Err(WebError::NoProvider { operation: "fetch" }),
            1 => Ok(Arc::clone(&providers[0])),
            count => Err(WebError::Ambiguous {
                operation: "fetch",
                count,
            }),
        }
    }
}

impl Web {
    /// Whether no provider is registered at all.
    ///
    /// A caller uses this to decide whether to offer the web tools: a tool that always fails teaches the model that
    /// the web is broken rather than that this deployment has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.search
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            && self
                .fetch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
    }
}

/// Builds a web capability from the environment.
///
/// The providers are chosen by what the environment configures: a search endpoint and a fetch endpoint, each optional.
/// An unset one means this deployment does not offer that capability, which is the honest reading — it is not a
/// failure to configure, and a caller that treated it as one would refuse to start.
#[must_use]
pub fn build_web_from_env() -> Web {
    let web = Web::new();
    if let Ok(endpoint) = std::env::var("GIRDER_FETCH_ENDPOINT")
        && !endpoint.trim().is_empty()
    {
        // A single provider serves both when one endpoint is configured: the common shape is one service that
        // searches and fetches.
        let provider = Arc::new(EndpointProvider {
            name: "env".to_owned(),
            endpoint: endpoint.clone(),
        });
        let _ = web.register_fetch(Arc::clone(&provider) as Arc<dyn FetchProvider>);
        let _ = web.register_search(provider);
    }
    web
}

/// A provider that talks to one HTTP endpoint.
///
/// Deliberately minimal: it posts a JSON request and reads a JSON answer. A deployment with a real search service
/// implements [`SearchProvider`] and [`FetchProvider`] itself, and this exists so the tools are usable without one.
pub struct EndpointProvider {
    /// Its name, for an error message.
    pub name: String,
    /// The base URL.
    pub endpoint: String,
}

impl FetchProvider for EndpointProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn fetch(
        &self,
        request: FetchRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::scope::BoxFut<Result<FetchResult, WebError>> {
        let endpoint = self.endpoint.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let response = reqwest::Client::new()
                .get(&endpoint)
                .query(&[("url", request.url.as_str())])
                .send()
                .await
                .map_err(|error| WebError::Provider {
                    provider: name.clone(),
                    reason: error.to_string(),
                })?;
            let status = response.status().as_u16();
            let body = response.text().await.map_err(|error| WebError::Provider {
                provider: name,
                reason: error.to_string(),
            })?;
            Ok(FetchResult {
                url: request.url,
                status_code: status,
                body: FetchBody::Text { text: body },
                truncated: false,
            })
        })
    }
}

impl SearchProvider for EndpointProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn search(
        &self,
        request: SearchRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> crate::scope::BoxFut<Result<SearchResult, WebError>> {
        let endpoint = self.endpoint.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let response = reqwest::Client::new()
                .get(&endpoint)
                .query(&[("q", request.query.as_str())])
                .send()
                .await
                .map_err(|error| WebError::Provider {
                    provider: name.clone(),
                    reason: error.to_string(),
                })?;
            let value: serde_json::Value =
                response.json().await.map_err(|error| WebError::Provider {
                    provider: name,
                    reason: error.to_string(),
                })?;
            let sources = value
                .get("results")
                .and_then(serde_json::Value::as_array)
                .map(|results| {
                    results
                        .iter()
                        .filter_map(|result| {
                            let url = result.get("url")?.as_str()?.to_owned();
                            Some(SearchSource {
                                url,
                                title: result
                                    .get("title")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_owned),
                                snippet: result
                                    .get("snippet")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_owned),
                                summary: None,
                                published_at: None,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(SearchResult {
                content: None,
                sources,
                truncated: false,
            })
        })
    }
}
