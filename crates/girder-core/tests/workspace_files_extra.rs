//! Tests for the per-entry workspace file methods.
//!
//! These exist because a tree is the wrong shape for a browser: it re-walks a subtree to answer "what is in this
//! folder" and cannot report one entry's metadata at all.

use std::path::PathBuf;
use std::sync::Arc;

use girder_core::api::config::WorkspaceRegistry;
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
            "workspaceFiles",
            "stat",
            call(json!({"workspaceId": id_of(&registry), "path": "a.txt"})),
        )
        .await
        .expect("stat");

    assert_eq!(
        value["absolutePath"],
        dir.join("a.txt").to_string_lossy().to_string()
    );
    assert!(
        value["version"]
            .as_str()
            .is_some_and(|version| !version.is_empty())
    );
    assert_eq!(value["bytes"], 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stat_on_a_missing_path_is_a_typed_failure() {
    // The client treats not-found as a failure code and removes the stale resource.
    let (dir, registry) = fixture("stat-missing");

    let error = registry
        .dispatch(
            "workspaceFiles",
            "stat",
            call(json!({"workspaceId": id_of(&registry), "path": "absent.txt"})),
        )
        .await
        .expect_err("missing");
    assert!(matches!(
        error,
        RemoteError::Failed(message) if message.starts_with("workspace-file/not-found:")
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn list_names_each_childs_type_in_name_order() {
    let (dir, registry) = fixture("list-order");
    std::fs::write(dir.join("b.txt"), "hello").expect("written");
    std::fs::create_dir(dir.join("a-dir")).expect("created");
    std::os::unix::fs::symlink("missing", dir.join("dangling")).expect("link");

    let value = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceId": id_of(&registry), "path": "", "maxEntries": 2})),
        )
        .await
        .expect("list");

    assert_eq!(value["path"], "");
    assert_eq!(value["truncated"], true);
    assert_eq!(
        value["entries"],
        json!([
            { "name": "a-dir", "type": "directory" },
            { "name": "b.txt", "type": "file", "size": 5 }
        ])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_returns_text_and_reports_the_total() {
    let (dir, registry) = fixture("read");
    std::fs::write(dir.join("a.txt"), "hello world").expect("written");

    let value = registry
        .dispatch(
            "workspaceFiles",
            "read",
            call(json!({"workspaceId": id_of(&registry), "path": "a.txt"})),
        )
        .await
        .expect("read");

    assert_eq!(value["text"], "hello world");
    assert_eq!(value["offset"], 1);
    assert_eq!(value["lines"], 1);
    assert_eq!(value["eof"], true);
    assert_eq!(value["bytes"], 11);
    assert!(
        value["absolutePath"]
            .as_str()
            .is_some_and(|path| path.ends_with("a.txt"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_returns_the_requested_lines_and_says_when_more_follow() {
    let (dir, registry) = fixture("read-limit");
    std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").expect("written");

    let value = registry
        .dispatch(
            "workspaceFiles",
            "read",
            call(
                json!({"workspaceId": id_of(&registry), "path": "a.txt", "offset": 2, "limit": 1}),
            ),
        )
        .await
        .expect("read");

    assert_eq!(value["text"], "two");
    assert_eq!(value["offset"], 2);
    assert_eq!(value["lines"], 1);
    assert_eq!(value["eof"], false);
    assert_eq!(value["bytes"], 14);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_refuses_a_file_that_contains_a_nul() {
    let (dir, registry) = fixture("read-nul");
    std::fs::write(dir.join("a.txt"), b"hello\0world").expect("written");

    let error = registry
        .dispatch(
            "workspaceFiles",
            "read",
            call(json!({"workspaceId": id_of(&registry), "path": "a.txt"})),
        )
        .await
        .expect_err("binary");

    assert!(matches!(
        error,
        RemoteError::Failed(message) if message.starts_with("workspace-file/not-text:")
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused() {
    let (dir, registry) = fixture("escape");

    let error = registry
        .dispatch(
            "workspaceFiles",
            "stat",
            call(json!({"workspaceId": id_of(&registry), "path": "../../etc/passwd"})),
        )
        .await
        .expect_err("refused");

    assert!(matches!(error, RemoteError::Refused { .. }), "{error:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn list_reports_the_codes_the_file_tree_reads() {
    let (dir, registry) = fixture("list-codes");
    std::fs::write(dir.join("a.txt"), "hello").expect("written");
    let id = id_of(&registry);

    let missing = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceId": id, "path": "gone"})),
        )
        .await
        .expect_err("missing");
    assert!(
        missing.to_string().starts_with("workspace-file/not-found:"),
        "{missing}"
    );

    let file = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceId": id, "path": "a.txt"})),
        )
        .await
        .expect_err("file");
    assert!(
        file.to_string()
            .starts_with("workspace-file/not-directory:"),
        "{file}"
    );

    let outside = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceId": id, "path": "../outside"})),
        )
        .await
        .expect_err("outside");
    assert!(
        outside
            .to_string()
            .starts_with("workspace-file/outside-workspace:"),
        "{outside}"
    );

    let listed = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceId": id, "path": dir.display().to_string()})),
        )
        .await
        .expect("absolute root");
    assert_eq!(listed["entries"][0]["name"], "a.txt");
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
            "workspaceFiles",
            "search",
            call(json!({"workspaceId": id_of(&registry), "path": "", "query": "rs"})),
        )
        .await
        .expect("search");

    let matches = value["matches"].as_array().expect("matches");
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert_eq!(matches[0]["path"], "main.rs");
    assert_eq!(matches[0]["type"], "file");
    assert_eq!(value["truncated"], false);
    assert_eq!(value["incomplete"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn search_reports_matching_ancestors_and_skips_hidden_files() {
    let (dir, registry) = fixture("search-shape");
    std::fs::create_dir_all(dir.join("src/deep")).expect("dir");
    std::fs::create_dir_all(dir.join("src/button-group")).expect("dir");
    std::fs::write(dir.join("src/deep/Button.tsx"), "x").expect("written");
    std::fs::write(dir.join("src/button-group/a"), "x").expect("written");
    std::fs::write(dir.join("BUTTON.md"), "x").expect("written");
    std::fs::write(dir.join("env.md"), "x").expect("written");
    std::fs::write(dir.join(".env"), "secret").expect("written");

    let value = registry
        .dispatch(
            "workspaceFiles",
            "search",
            call(json!({
                "workspaceId": id_of(&registry),
                "path": "",
                "query": "  button ",
                "respectGitignore": false
            })),
        )
        .await
        .expect("search");

    let paths: Vec<&str> = value["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        paths,
        ["BUTTON.md", "src/button-group", "src/deep/Button.tsx"]
    );

    let hidden = registry
        .dispatch(
            "workspaceFiles",
            "search",
            call(json!({
                "workspaceId": id_of(&registry),
                "path": "",
                "query": "env",
                "respectGitignore": false
            })),
        )
        .await
        .expect("hidden");
    let hidden_paths: Vec<&str> = hidden["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(hidden_paths, ["env.md"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn search_without_a_query_is_refused() {
    let (dir, registry) = fixture("search-empty");

    let error = registry
        .dispatch(
            "workspaceFiles",
            "search",
            call(json!({"workspaceId": id_of(&registry), "path": ""})),
        )
        .await
        .expect_err("no query");

    assert_eq!(
        error.to_string(),
        "gateway/bad-request: `query` is required"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_workspace_is_refused() {
    let (dir, registry) = fixture("unknown");

    let error = registry
        .dispatch(
            "workspaceFiles",
            "stat",
            call(json!({"workspaceId": "no-such", "path": "a.txt"})),
        )
        .await
        .expect_err("unknown workspace");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_session_scope_reads_that_sessions_workspace() {
    let dir = scratch("session-scope");
    std::fs::write(dir.join("notes.txt"), "hello\nworld\n").expect("written");
    let sessions = scratch("session-scope-catalog");
    std::fs::write(
        sessions.join("session-1.header"),
        serde_json::json!({
            "createdAt": 1,
            "cwd": dir.display().to_string(),
        })
        .to_string(),
    )
    .expect("header");
    let catalog = std::sync::Arc::new(girder_core::DirectoryCatalog::new(&sessions));
    let workspaces = Arc::new(WorkspaceRegistry::new());
    let fs: Arc<dyn girder_core::FileSystem> = Arc::new(LocalFileSystem);
    let mut registry = RemoteRegistry::new();
    girder_core::register_workspace_file_extras_with_sessions(
        &mut registry,
        &fs,
        &workspaces,
        Some(catalog),
        None,
    );

    let listed = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceFileScopeId": "session-1", "path": ""})),
        )
        .await
        .expect("list");
    assert_eq!(listed["entries"][0]["name"], "notes.txt");

    let page = registry
        .dispatch(
            "workspaceFiles",
            "read",
            call(json!({
                "workspaceFileScopeId": "session-1",
                "path": "notes.txt",
                "range": { "offset": 2, "limit": 1 }
            })),
        )
        .await
        .expect("read");
    assert_eq!(page["text"], "world");
    assert_eq!(page["eof"], true);

    let found = registry
        .dispatch(
            "workspaceFiles",
            "search",
            call(json!({
                "workspaceFileScopeId": "session-1",
                "request": { "query": "notes" }
            })),
        )
        .await
        .expect("search");
    assert_eq!(found["matches"][0]["path"], "notes.txt");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&sessions);
}

#[tokio::test]
async fn a_session_without_a_directory_uses_the_deployment_root() {
    let fallback = scratch("deployment-root");
    std::fs::write(fallback.join("marker.txt"), "fallback").expect("written");
    let own = scratch("session-own");
    std::fs::write(own.join("own.txt"), "own").expect("written");
    let sessions = scratch("session-fallback-catalog");
    std::fs::write(
        sessions.join("plain.header"),
        serde_json::json!({"createdAt": 1}).to_string(),
    )
    .expect("plain header");
    std::fs::write(
        sessions.join("placed.header"),
        serde_json::json!({"createdAt": 1, "cwd": own.display().to_string()}).to_string(),
    )
    .expect("placed header");
    let catalog = std::sync::Arc::new(girder_core::DirectoryCatalog::new(&sessions));
    let workspaces = Arc::new(WorkspaceRegistry::new());
    let fs: Arc<dyn girder_core::FileSystem> = Arc::new(LocalFileSystem);
    let mut registry = RemoteRegistry::new();
    girder_core::register_workspace_file_extras_with_sessions(
        &mut registry,
        &fs,
        &workspaces,
        Some(catalog),
        Some(fallback.clone()),
    );

    let listed = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceFileScopeId": "plain", "path": ""})),
        )
        .await
        .expect("fallback list");
    assert_eq!(listed["entries"][0]["name"], "marker.txt");

    let placed = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceFileScopeId": "placed", "path": ""})),
        )
        .await
        .expect("placed list");
    assert_eq!(placed["entries"][0]["name"], "own.txt");

    let missing = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceFileScopeId": "missing", "path": ""})),
        )
        .await
        .expect_err("missing session");
    assert!(
        missing.to_string().starts_with("gateway/lookup-not-found:"),
        "{missing}"
    );

    let moved = scratch("session-moved");
    std::fs::write(moved.join("moved.txt"), "moved").expect("written");
    girder_core::SessionStore::new(sessions.join("placed.jsonl.zst"))
        .touch()
        .expect("log");
    girder_core::SessionStore::new(sessions.join("placed.jsonl.zst"))
        .append(&girder_core::SessionEvent::WorkspaceRootChange {
            cwd: moved.display().to_string(),
        })
        .expect("move");
    let relocated = registry
        .dispatch(
            "workspaceFiles",
            "list",
            call(json!({"workspaceFileScopeId": "placed", "path": ""})),
        )
        .await
        .expect("relocated list");
    assert_eq!(relocated["entries"][0]["name"], "moved.txt");
    let _ = std::fs::remove_dir_all(&moved);
    let _ = std::fs::remove_dir_all(&fallback);
    let _ = std::fs::remove_dir_all(&own);
    let _ = std::fs::remove_dir_all(&sessions);
}
