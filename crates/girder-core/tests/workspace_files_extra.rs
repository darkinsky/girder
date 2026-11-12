//! Tests for the per-entry workspace file methods.
//!
//! These exist because a tree is the wrong shape for a browser: it re-walks a subtree to answer "what is in this
//! folder" and cannot report one entry's metadata at all.

use std::path::PathBuf;
use std::sync::Arc;

use girder_core::api_config::WorkspaceRegistry;
use girder_core::{LocalFileSystem, RemoteCall, RemoteError, RemoteRegistry};
use serde_json::json;

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-wf-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A registry with one workspace rooted at a fresh directory.
fn fixture(name: &str) -> (PathBuf, RemoteRegistry) {
    let dir = scratch(name);
    let workspaces = Arc::new(WorkspaceRegistry::new());
    let workspace = workspaces
        .register("ws-1", &dir, Some("test".to_owned()), 0)
        .expect("created");
    let fs: Arc<dyn girder_core::FileSystem> = Arc::new(LocalFileSystem);
    let _ = workspace;
    let mut registry = RemoteRegistry::new();
    girder_core::register_workspace_file_extras(&mut registry, &fs, &workspaces);
    (dir, registry)
}

/// The id the fixture registered.
const WORKSPACE_ID: &str = "ws-1";

fn id_of(_registry: &RemoteRegistry) -> &'static str {
    WORKSPACE_ID
}

#[tokio::test]
async fn stat_reports_a_file_size() {
    let (dir, registry) = fixture("stat");
    std::fs::write(dir.join("a.txt"), "hello").expect("written");

    let value = registry
        .dispatch(
            "workspace",
            "stat",
            call(json!({"workspaceId": id_of(&registry), "path": "a.txt"})),
        )
        .await
        .expect("stat");

    assert_eq!(value["exists"], true);
    assert_eq!(value["isFile"], true);
    assert_eq!(value["size"], 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stat_on_a_missing_path_says_it_is_missing() {
    // A browser refreshing a directory whose file was deleted needs "gone", not a transport error.
    let (dir, registry) = fixture("stat-missing");

    let value = registry
        .dispatch(
            "workspace",
            "stat",
            call(json!({"workspaceId": id_of(&registry), "path": "absent.txt"})),
        )
        .await
        .expect("stat");

    assert_eq!(value["exists"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn list_puts_directories_before_files() {
    // Mixing files into folders is the one thing every file manager agrees not to do.
    let (dir, registry) = fixture("list-order");
    std::fs::write(dir.join("b.txt"), "x").expect("written");
    std::fs::create_dir(dir.join("a-dir")).expect("created");

    let value = registry
        .dispatch(
            "workspace",
            "list",
            call(json!({"workspaceId": id_of(&registry), "path": ""})),
        )
        .await
        .expect("list");

    let entries = value["entries"].as_array().expect("entries");
    assert_eq!(entries[0]["name"], "a-dir", "the directory comes first");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_returns_text_and_reports_the_total() {
    let (dir, registry) = fixture("read");
    std::fs::write(dir.join("a.txt"), "hello world").expect("written");

    let value = registry
        .dispatch(
            "workspace",
            "read",
            call(json!({"workspaceId": id_of(&registry), "path": "a.txt"})),
        )
        .await
        .expect("read");

    assert_eq!(value["text"], "hello world");
    assert_eq!(value["totalBytes"], 11);
    assert_eq!(value["truncated"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_says_when_it_cut_the_content() {
    // A client that received the first bytes of a large file with no indication would render half a file as whole.
    let (dir, registry) = fixture("read-limit");
    std::fs::write(dir.join("a.txt"), "0123456789").expect("written");

    let value = registry
        .dispatch(
            "workspace",
            "read",
            call(json!({"workspaceId": id_of(&registry), "path": "a.txt", "limit": 4})),
        )
        .await
        .expect("read");

    assert_eq!(value["text"], "0123");
    assert_eq!(value["truncated"], true);
    assert_eq!(value["totalBytes"], 10);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_cuts_on_a_character_boundary() {
    // A cut inside a multi-byte character would hand the client a replacement character it displays as corruption.
    let (dir, registry) = fixture("read-boundary");
    std::fs::write(dir.join("a.txt"), "日本語").expect("written");

    let value = registry
        .dispatch(
            "workspace",
            "read",
            call(json!({"workspaceId": id_of(&registry), "path": "a.txt", "limit": 4})),
        )
        .await
        .expect("read");

    let text = value["text"].as_str().expect("text");
    assert_eq!(
        text, "日",
        "three bytes of a three-byte character is still one character"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused() {
    let (dir, registry) = fixture("escape");

    let error = registry
        .dispatch(
            "workspace",
            "stat",
            call(json!({"workspaceId": id_of(&registry), "path": "../../etc/passwd"})),
        )
        .await
        .expect_err("refused");

    assert!(matches!(error, RemoteError::Refused { .. }), "{error:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn search_finds_a_name_anywhere_in_the_name() {
    // A prefix-only match would miss `main.rs` for `rs`, which is not what a search box does.
    let (dir, registry) = fixture("search");
    std::fs::write(dir.join("main.rs"), "x").expect("written");
    std::fs::write(dir.join("other.txt"), "x").expect("written");

    let value = registry
        .dispatch(
            "workspace",
            "search",
            call(json!({"workspaceId": id_of(&registry), "path": "", "query": "rs"})),
        )
        .await
        .expect("search");

    let hits = value["hits"].as_array().expect("hits");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["name"], "main.rs");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn search_without_a_query_is_refused() {
    let (dir, registry) = fixture("search-empty");

    let error = registry
        .dispatch(
            "workspace",
            "search",
            call(json!({"workspaceId": id_of(&registry), "path": ""})),
        )
        .await
        .expect_err("no query");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_workspace_is_refused() {
    let (dir, registry) = fixture("unknown");

    let error = registry
        .dispatch(
            "workspace",
            "stat",
            call(json!({"workspaceId": "no-such", "path": "a.txt"})),
        )
        .await
        .expect_err("unknown workspace");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}
