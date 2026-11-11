//! The `session.search` Remote method: conversation history search over RPC.
//!
//! [`crate::session_query`] already answers "where did this happen" for a model through the
//! `session_search` tool. This exposes the same search to a remote client, and the two share the
//! matching rules on purpose: a client and a model that disagreed about whether an event matched
//! would be answering different questions about the same logs.
//!
//! # Why the root is a registration argument
//!
//! The directory of session logs belongs to the deployment, not to the query. Passing it in when the
//! registry is built means a client cannot name a directory to search — a `root` argument on the
//! method would let any caller read any log the process can open.
//!
//! # Why the response is a projection
//!
//! A hit's event is not returned as the event. The response carries the session, the position, the
//! type tag, and a bounded snippet: what a caller needs to decide whether to open the session. Sending
//! whole events would make a search a way to read a transcript in one call, which is a different
//! method with a different cost.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::session_query::{self, Kind, Query, Results};

/// The namespace `session.search` lives under.
const NAMESPACE: &str = "session";

/// The method name within [`NAMESPACE`].
const METHOD: &str = "search";

/// How many hits a call returns when the client does not say.
///
/// The same bound the `session_search` tool uses, so a client and a model that ask one question get
/// answers of the same size.
pub const DEFAULT_LIMIT: usize = 50;

/// How much of a hit's text the response carries.
///
/// A snippet rather than the whole event: a tool result can be an entire file, and a search whose
/// payload was the log itself would be a worse way to read a session than reading the session.
pub const MAX_TEXT_CHARS: usize = 200;

/// The arguments of `session.search`.
///
/// Every field is optional because the useful queries are bare: no text lists what happened, and no
/// session searches everything. Typed and `serde`-parsed rather than hand-read with `Value::get`, so
/// a wrong type names the field instead of silently defaulting.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SearchRequest {
    /// Case-insensitive text to look for; empty matches everything.
    ///
    /// Optional-and-nullable because a client that serialises every field sends `null` for the ones
    /// it is not using, and rejecting that would make the method unusable from a generated client.
    text: Option<String>,
    /// Restrict the search to one session id.
    session: Option<String>,
    /// One of `messages`, `tools`, or `boundaries`.
    kind: Option<String>,
    /// Maximum hits to return.
    limit: Option<u64>,
}

/// Maps the wire spelling of a kind onto the filter.
///
/// An unknown spelling is an error rather than "no filter": silently widening a request the caller
/// meant to narrow returns hits they did not ask for, and a client that sent the wrong word would
/// never learn it.
fn parse_kind(kind: Option<&str>) -> Result<Option<Kind>, RemoteError> {
    let Some(value) = kind else {
        return Ok(None);
    };
    // The spellings live in `session_query` so this layer and the tool layer cannot disagree about what a client is
    // allowed to send. This layer still refuses an unknown one, which is the difference between the two callers.
    Kind::from_wire(value)
        .map(Some)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: METHOD.to_owned(),
            message: format!("`kind` must be one of messages, tools, boundaries; got `{value}`"),
        })
}

/// A hit's text, bounded and cut on a character boundary.
///
/// The cut position is found by walking `char_indices` to the limit, so it always lands between
/// characters. Slicing at a fixed byte offset would split a multi-byte character and put U+FFFD in
/// the response, which a reader sees as corrupt log data rather than as a truncated snippet — and
/// this search exists to be pointed at non-ASCII conversations.
fn snippet(text: &str) -> String {
    // A client renders each hit as a block, so a newline is retained: it is the message's shape, not a row separator.
    session_query::excerpt(text, MAX_TEXT_CHARS, false)
}

/// Registers `session.search` over the session logs under `root`.
///
/// A free function taking the registry, like the other `register_*` functions: the host decides
/// which capabilities exist, and a module that registered itself would make that decision for it.
pub fn register_session_query_methods(registry: &mut RemoteRegistry, root: &Path) {
    let root: PathBuf = root.to_path_buf();
    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let root = root.clone();
        Box::pin(async move { search(&call.args, &root) })
    });
}

/// Runs one `session.search` call.
fn search(args: &Value, root: &Path) -> Result<Value, RemoteError> {
    let request: SearchRequest =
        serde_json::from_value(args.clone()).map_err(|error| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: METHOD.to_owned(),
            message: error.to_string(),
        })?;

    let limit = request
        .limit
        .map_or(DEFAULT_LIMIT, |value| {
            // Saturating rather than truncating: a limit too large for this target means "every hit
            // that exists", and a wrapped value would silently bound a request that asked for none.
            usize::try_from(value).unwrap_or(usize::MAX)
        })
        // Zero hits is not a useful answer to any question a caller asks, and `truncated` would be
        // true for it, which reads as "there were matches but I stopped" — a claim the search never
        // established.
        .max(1);

    let query = Query {
        text: request.text.unwrap_or_default(),
        kind: parse_kind(request.kind.as_deref())?,
        session: request.session,
        limit: Some(limit),
    };

    // `search_files` is synchronous — it lists a directory and decompresses every session log it
    // finds — and blocking file work does not become non-blocking by being awaited. It is called
    // inline because it runs at a client's request pace rather than on the agent's step path, and
    // because moving it to `spawn_blocking` would need the root and the query moved across a thread
    // boundary only to hand back the same value. This is the known blocking point: if session
    // directories grow large enough that one call starves the executor, this is where to offload it.
    let results: Results = session_query::search_files(root, &query);

    let hits: Vec<Value> = results
        .hits
        .iter()
        .map(|hit| {
            json!({
                "session": hit.session,
                "seq": hit.seq,
                // The event's own tag rather than the requested filter: a caller that asked for
                // everything needs to know what it got, and one that filtered gets confirmation.
                "kind": hit.kind,
                "text": snippet(&session_query::searchable_text(&hit.event)),
            })
        })
        .collect();

    // No matches is a result, not a failure: an empty array with the scan count tells a caller
    // whether nothing exists or nothing was examined, and an error would say neither.
    Ok(json!({
        "hits": hits,
        "sessionsScanned": results.sessions_scanned,
        // Reported, never inferred from `hits.len() == limit`: a caller that received exactly the
        // limit cannot tell a complete answer from a cut-off one, and a tool that guessed would draw
        // a conclusion the evidence does not support.
        "truncated": results.truncated,
    }))
}
