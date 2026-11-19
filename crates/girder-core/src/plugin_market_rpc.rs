//! `pluginMarket.*`: the market's four endpoints over the index this build already parses.
//!
//! # What upstream has
//!
//! `packages/boot/plugin-market/src/index.ts` (520 lines) marks four methods:
//!
//! | Endpoint | Answers |
//! | --- | --- |
//! | `listIndexes` | one summary per configured index: url, title, description, or the parse error |
//! | `setIndexes` | replaces the configured index list |
//! | `search` | one page of listings from one index |
//! | `readEntryDetail` | one entry's full record |
//!
//! # What this port has
//!
//! [`crate::plugin_market_gap`] (5,073 lines) holds the whole decision layer: index parsing, the
//! NFKC fold, the BM25 scoring, and the query validation. What it never had was an endpoint.
//!
//! The `url` parameter is the index's address, and this module does not fetch: a network read is a
//! deployment decision (which mirrors are reachable, whether to use a proxy), so the caller supplies
//! the already-parsed document through `setIndexes`, and every other endpoint reads it back. That is
//! the same split the settings loader uses: the pure decision here, the I/O above.
//!
//! # Why `search` validates before it searches
//!
//! Upstream throws `plugin-market/invalid-query` for a negative offset or an out-of-range limit
//! *before* touching the index. Doing it in the other order would make a bad query cost a full
//! scoring pass.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::plugin_market_gap::{
    MarketIndex, MarketQuery, MarketSort, QueryError, SearchIndex, validate_page,
};
use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::scope::BoxFut;

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "pluginMarket";

/// The largest page a caller may ask for.
///
/// Upstream's `MAX_LIMIT`. A market listing is read by a person, and a page larger than this is one
/// nobody scrolls.
pub const MAX_LIMIT: usize = 100;

/// The indices a deployment configured, keyed by url.
///
/// A `Mutex` rather than a channel because every operation is a short read or a whole-map replace:
/// `setIndexes` swaps the map, and a search holds it only while selecting.
#[derive(Default)]
pub struct MarketCatalog {
    documents: Mutex<BTreeMap<String, MarketIndex>>,
    errors: Mutex<BTreeMap<String, String>>,
}

impl MarketCatalog {
    /// An empty catalogue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the configured indices.
    ///
    /// Parse failures are kept beside the successes rather than dropping the whole map: a
    /// deployment with one bad index should still be able to search the others, and `listIndexes`
    /// reports the failure so a person can fix it. That is upstream's behaviour -- its summary row
    /// carries an `error` alongside the title.
    pub fn set_indices(&self, entries: &[(String, Value)]) {
        let mut documents = BTreeMap::new();
        let mut errors = BTreeMap::new();
        for (url, value) in entries {
            match crate::plugin_market_gap::parse_market_index(value, url) {
                Ok(index) => {
                    documents.insert(url.clone(), index);
                }
                Err(error) => {
                    errors.insert(url.clone(), error.to_string());
                }
            }
        }
        if let Ok(mut held) = self.documents.lock() {
            *held = documents;
        }
        if let Ok(mut held) = self.errors.lock() {
            *held = errors;
        }
    }

    /// One parsed index.
    fn document(&self, url: &str) -> Option<MarketIndex> {
        self.documents
            .lock()
            .ok()
            .and_then(|held| held.get(url).cloned())
    }

    /// Every configured url, sorted, so two runs list them the same way.
    fn urls(&self) -> Vec<String> {
        let documents = self
            .documents
            .lock()
            .map(|held| held.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut errors = self
            .errors
            .lock()
            .map(|held| held.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut all = documents;
        all.append(&mut errors);
        all.sort();
        all.dedup();
        all
    }

    /// The parse error for one url, when it failed.
    fn error(&self, url: &str) -> Option<String> {
        self.errors
            .lock()
            .ok()
            .and_then(|held| held.get(url).cloned())
    }
}

/// Translates a query error into the wire error upstream uses.
fn query_error(error: &QueryError) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: "search".to_owned(),
        message: format!("plugin-market/invalid-query: {error}"),
    }
}

/// Reads a `MarketQuery` from the wire.
///
/// Validated here rather than in `select` because upstream validates in the endpoint: a caller
/// learns about a bad limit without the index being touched.
fn market_query(args: &Value) -> Result<MarketQuery, RemoteError> {
    let offset = args.get("offset").and_then(Value::as_i64).unwrap_or(0);
    let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(20);
    let (offset, limit) = validate_page(offset, limit).map_err(|error| query_error(&error))?;
    if limit > MAX_LIMIT {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: "search".to_owned(),
            message: format!("plugin-market/invalid-query: limit must be at most {MAX_LIMIT}"),
        });
    }
    Ok(MarketQuery {
        text: args
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|text| !text.trim().is_empty()),
        category: args
            .get("category")
            .and_then(Value::as_str)
            .map(str::to_owned),
        labels: args
            .get("labels")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        // Upstream's default is the publisher's own order, not relevance: an untargeted listing
        // shows the index as its author arranged it.
        sort: match args.get("sort").and_then(Value::as_str) {
            Some("relevance") => MarketSort::Relevance,
            Some("published") => MarketSort::Published {
                direction: crate::plugin_market_gap::SortDirection::Desc,
            },
            _ => MarketSort::Index,
        },
        offset,
        limit,
    })
}

/// Registers the four `pluginMarket` endpoints.
///
/// One function because the four share the catalogue and nothing else, and a caller registering
/// three of them would have a market that lists an index it cannot search.
#[allow(
    clippy::too_many_lines,
    reason = "four handlers over one catalogue; splitting them would put each in a function that needs the same three lines of argument extraction, and the four are read together"
)]
pub fn register_market_methods(registry: &mut RemoteRegistry, catalog: &Arc<MarketCatalog>) {
    let listing = Arc::clone(catalog);
    registry.register(NAMESPACE, "listIndexes", move |_call: RemoteCall| {
        let catalog = Arc::clone(&listing);
        Box::pin(async move {
            let rows: Vec<Value> = catalog
                .urls()
                .into_iter()
                .map(|url| {
                    // A failed index reports its error and says nothing else: a title would be a
                    // claim about a document that did not parse.
                    if let Some(error) = catalog.error(&url) {
                        return json!({ "url": url, "error": error });
                    }
                    let document = catalog.document(&url);
                    // `LocalizedText` is one string or a map per language; the summary carries the
                    // first variant, which is what a one-line list shows. A client that needs every
                    // language reads the detail endpoint.
                    json!({
                        "url": url,
                        "title": document
                            .as_ref()
                            .and_then(|index| index.title.variants().first().copied()),
                    })
                })
                .collect();
            Ok(json!(rows))
        }) as BoxFut<_>
    });

    let setting = Arc::clone(catalog);
    registry.register(NAMESPACE, "setIndexes", move |call: RemoteCall| {
        let catalog = Arc::clone(&setting);
        Box::pin(async move {
            let entries = call
                .args
                .get("indexes")
                .and_then(Value::as_array)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "setIndexes".to_owned(),
                    message: "`indexes` must be an array of `{url, document}`".to_owned(),
                })?;
            let mut parsed = Vec::new();
            for entry in entries {
                let url = entry.get("url").and_then(Value::as_str).ok_or_else(|| {
                    RemoteError::InvalidArguments {
                        namespace: NAMESPACE.to_owned(),
                        method: "setIndexes".to_owned(),
                        message: "each index needs a `url`".to_owned(),
                    }
                })?;
                let document = entry.get("document").cloned().unwrap_or(Value::Null);
                parsed.push((url.to_owned(), document));
            }
            catalog.set_indices(&parsed);
            Ok(json!({ "indexes": parsed.len() }))
        }) as BoxFut<_>
    });

    let searching = Arc::clone(catalog);
    registry.register(NAMESPACE, "search", move |call: RemoteCall| {
        let catalog = Arc::clone(&searching);
        Box::pin(async move {
            let url = call
                .args
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "search".to_owned(),
                    message: "`url` is required".to_owned(),
                })?
                .to_owned();
            let query = market_query(&call.args)?;
            let Some(document) = catalog.document(&url) else {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "search".to_owned(),
                    message: format!("no index at `{url}`; call `setIndexes` first"),
                });
            };
            let page = SearchIndex::new(&document)
                .select(&query)
                .map_err(|error| query_error(&error))?;
            Ok(json!({
                "positions": page.positions,
                "total": page.total,
                "nextOffset": page.next_offset,
            }))
        }) as BoxFut<_>
    });

    let reading = Arc::clone(catalog);
    registry.register(NAMESPACE, "readEntryDetail", move |call: RemoteCall| {
        let catalog = Arc::clone(&reading);
        Box::pin(async move {
            let url = call
                .args
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "readEntryDetail".to_owned(),
                    message: "`url` is required".to_owned(),
                })?
                .to_owned();
            let Some(document) = catalog.document(&url) else {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "readEntryDetail".to_owned(),
                    message: format!("no index at `{url}`; call `setIndexes` first"),
                });
            };
            Ok(json!({
                "plugins": document.plugins.len(),
                "title": document.title.variants().first().copied(),
                "titles": document.title.variants(),
            }))
        }) as BoxFut<_>
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_negative_offset_is_refused_before_the_index_is_touched() {
        // Upstream validates in the endpoint for this reason: a bad query must not cost a scoring
        // pass over every listing.
        let error = market_query(&json!({ "offset": -1, "limit": 20 })).expect_err("negative");
        assert!(error.to_string().contains("invalid-query"), "{error}");
    }

    #[test]
    fn a_limit_past_the_cap_is_refused() {
        let error = market_query(&json!({ "offset": 0, "limit": 500 })).expect_err("too large");
        assert!(error.to_string().contains("invalid-query"), "{error}");
    }

    #[test]
    fn omitted_paging_uses_the_defaults() {
        let query = market_query(&json!({})).expect("defaults");
        assert_eq!(query.offset, 0);
        assert_eq!(query.limit, 20);
        assert_eq!(query.sort, MarketSort::Index);
    }

    #[test]
    fn a_blank_search_text_is_treated_as_no_filter() {
        let query = market_query(&json!({ "text": "   " })).expect("blank");
        assert_eq!(query.text, None);
    }

    #[test]
    fn a_failed_index_is_kept_and_reported() {
        let catalog = MarketCatalog::new();
        catalog.set_indices(&[("https://x/index.json".to_owned(), json!({"nope": true}))]);
        // The url is still listed, with its error, so a person can see what to fix.
        assert_eq!(catalog.urls(), vec!["https://x/index.json".to_owned()]);
        assert!(catalog.error("https://x/index.json").is_some());
        assert!(catalog.document("https://x/index.json").is_none());
    }
}
