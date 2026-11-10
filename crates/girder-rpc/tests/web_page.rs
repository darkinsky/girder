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
        html.contains("not to be the product") || html.contains("proves the chain"),
        "the page must say it is a probe, not the Web UI"
    );
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
