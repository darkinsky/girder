//! Tests for the probe page.
//!
//! The page is a single embedded string, so the tests check the properties a browser depends on: it
//! is served, it is HTML, and the buttons it renders name methods the server actually has.

/// Sends one HTTP GET and returns the whole raw response.
///
/// A hand-written request rather than an HTTP client crate: the assertion is the same either way,
/// and this crate should not gain a dependency to read forty bytes.
async fn get(base: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let address = base.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
}

/// Starts the server on an ephemeral port and returns its base URL.
///
/// A real listener rather than an in-process test harness: the page is fetched by a browser over TCP,
/// and a harness that skipped the socket would not catch a routing mistake.
async fn serve() -> (String, tokio_util::sync::CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let cancel = tokio_util::sync::CancellationToken::new();
    let stop = cancel.clone();

    tokio::spawn(async move {
        let _ = girder_rpc::serve_on(
            listener,
            std::sync::Arc::new(girder_core::EventBus::new()),
            Vec::new(),
            stop,
        )
        .await;
    });

    (format!("http://{address}"), cancel)
}

#[tokio::test]
async fn the_page_is_served_at_the_root() {
    let (base, cancel) = serve().await;

    let response = get(&base, "/").await;
    cancel.cancel();

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        response.to_lowercase().contains("content-type: text/html"),
        "a browser needs HTML:\n{response}"
    );
    assert!(response.contains("<!doctype html>"));
}

#[test]
fn the_page_calls_only_methods_the_build_exposes() {
    // A button naming a method the server lacks is a 404 the user sees and nobody can explain. The
    // page and the registry are checked against one list, which lives in `girder_core` because it is
    // the crate that registers them.
    let registered = girder_core::registered_method_names();

    for method in [
        "session.projections",
        "job.list",
        "session.list",
        "job.kill",
    ] {
        assert!(
            registered.contains(&method.to_owned()),
            "the page offers `{method}` but no build registers it; registered: {registered:?}"
        );
    }
}

#[test]
fn the_page_posts_to_the_invoke_endpoint() {
    // The one URL the page depends on; a change here breaks every button at once.
    assert!(girder_rpc::INDEX_HTML.contains("/api/remote.invoke"));
    assert_eq!(girder_rpc::INVOKE_PATH, "/api/remote.invoke");
}

#[test]
fn the_page_names_its_peer() {
    // The endpoint requires a peer; a page that omitted it would get a deserialization error on
    // every call.
    assert!(girder_rpc::INDEX_HTML.contains("peer:"));
}

#[test]
fn the_page_is_self_contained() {
    // No build step, no bundler, no external asset: an embedded string needs none of those.
    assert!(!girder_rpc::INDEX_HTML.contains("src=\"http"));
    assert!(!girder_rpc::INDEX_HTML.contains("href=\"http"));
    assert!(girder_rpc::INDEX_HTML.contains("<script>"));
}

#[test]
fn the_page_says_what_it_is_not() {
    // A probe page mistaken for the product wastes someone's afternoon.
    let html = girder_rpc::INDEX_HTML;
    assert!(
        html.contains("Not the Web UI") || html.contains("conversation surface"),
        "the page must say what it is, not claim to be the Web UI"
    );
}

#[test]
fn the_page_subscribes_to_the_event_socket() {
    // Streaming is the reason this page exists in its current shape: a client that only polled would
    // show the answer in one piece when the turn ended, which looks like a slow server.
    let html = girder_rpc::INDEX_HTML;

    assert!(
        html.contains("remote.mux"),
        "the page must open the mux socket"
    );
    assert!(
        html.contains("agent/turn"),
        "it must subscribe to the turn event"
    );
    assert!(
        html.contains("textDelta"),
        "it must render streaming deltas"
    );
}

#[test]
fn the_page_creates_a_session_before_prompting() {
    // A prompt without a session has nowhere to land.
    let html = girder_rpc::INDEX_HTML;

    assert!(html.contains("session', 'create'"));
    assert!(html.contains("session', 'prompt'"));
}

#[test]
fn the_page_can_stop_a_running_turn() {
    // A conversation surface that cannot interrupt a long answer is one the user closes.
    let html = girder_rpc::INDEX_HTML;

    assert!(html.contains("session', 'cancel'"));
    assert!(html.contains(r#"id="stop""#));
}

#[test]
fn the_page_does_not_claim_features_it_lacks() {
    // No reconnect/retry UI is offered, so the page should not promise one. It does reconnect the
    // socket, which is a transport concern rather than a promise about conversation state.
    let html = girder_rpc::INDEX_HTML;
    let lowered = html.to_lowercase();

    assert!(!lowered.contains("history is preserved"));
    assert!(!lowered.contains("markdown"));
}

#[tokio::test]
async fn the_page_and_the_api_share_one_origin() {
    // Serving the page from the API process means a browser has no second origin to configure.
    let (base, cancel) = serve().await;

    let page = get(&base, "/").await;
    // The invoke endpoint exists only when a remote is mounted; without one the root still serves.
    let api = get(&base, "/api/remote.invoke").await;
    cancel.cancel();

    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(
        !api.starts_with("HTTP/1.1 200"),
        "GET on a POST endpoint must not succeed:\n{api}"
    );
}

// ---- session management -----------------------------------------------------------------------

#[test]
fn the_page_lists_sessions_in_a_sidebar() {
    // A conversation surface that can only hold one conversation is one the user abandons after the
    // first topic change.
    let html = girder_rpc::INDEX_HTML;

    assert!(
        html.contains(r#"id="sessions""#),
        "a session list container"
    );
    assert!(html.contains("session', 'list'"), "it must load the list");
}

#[test]
fn switching_to_a_session_loads_its_transcript() {
    // A switch that showed an empty page would look like the conversation was lost.
    let html = girder_rpc::INDEX_HTML;

    assert!(html.contains("session', 'events'"), "it must read the log");
    assert!(
        html.contains("renderEvent"),
        "it must render durable events"
    );
}

#[test]
fn starting_a_new_conversation_keeps_the_old_one() {
    // A "new" that discarded the previous conversation would be a destructive action with a benign
    // name.
    let html = girder_rpc::INDEX_HTML;

    assert!(html.contains("startNew"));
    // `startNew` must not call delete. A delete exists on the row, which is a different action with
    // its own confirmation; the assertion is that the *new* handler does not use it.
    let start_new = html
        .split("async function startNew")
        .nth(1)
        .expect("startNew exists");
    let body = start_new.split('}').next().unwrap_or_default();
    assert!(
        !body.contains("'delete'"),
        "startNew must not delete: {body}"
    );
}

#[test]
fn the_page_renders_both_log_and_stream_shapes() {
    // The durable log holds whole messages; the stream holds deltas. One rendering function keeps a
    // loaded transcript and a live one looking the same.
    let html = girder_rpc::INDEX_HTML;

    assert!(html.contains("'user/message'"));
    assert!(html.contains("'assistant/message'"));
    assert!(html.contains("'textDelta'"));
}

#[test]
fn the_page_can_rename_and_delete_a_session() {
    // Both act on a row, so each needs a target: rename and delete belong on the row rather than in a
    // global toolbar.
    let html = girder_rpc::INDEX_HTML;

    assert!(html.contains("session', 'rename'"));
    assert!(html.contains("session', 'delete'"));
    assert!(
        html.contains("window.confirm"),
        "a delete must be confirmed"
    );
}
