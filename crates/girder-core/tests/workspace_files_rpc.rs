//! Tests for `workspace.tree` and `workspace.readFile`, the workspace file-browser surface.
//!
//! The properties under test, in the order they matter:
//!
//! - **the workspace boundary holds** — `../`, an absolute path elsewhere, and a symlink that points
//!   out are all refused, because a preview that leaks one file outside the project makes every path
//!   it reports untrustworthy
//! - **a cut page says it was cut** — a caller cannot otherwise tell a truncated preview from a
//!   complete one, and would read half a file as if it were all of it
//! - **binary content is refused, not decoded** — bytes that are not UTF-8, or that carry a NUL, come
//!   back as an error naming the reason rather than as mojibake presented as text
//! - a listing is bounded and a bounded listing reports it
//! - an unknown workspace id is a refusal, not an empty listing
//!
//! Isolation: every test builds its own registry and workspace over a scratch directory named from
//! the process id and a counter, so two test binaries and two tests in one binary cannot collide.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the methods as written even before the crate root wires them in. It spells its own paths
//! through `girder_core::`, which is the alias the library declares for itself, so the same source
//! builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/workspace_files_rpc.rs"]
mod workspace_files_rpc;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::api_config::WorkspaceRegistry;
use girder_core::{LocalFileSystem, RemoteCall, RemoteError, RemoteRegistry};

use workspace_files_rpc::{
    DEFAULT_READ_MAX_BYTES, DEFAULT_TREE_ENTRY_LIMIT, MAX_TREE_DEPTH, MAX_TREE_ENTRY_LIMIT,
    register_workspace_file_methods,
};

/// The workspace id every test registers under.
const WORKSPACE: &str = "ws-test";

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-wsfiles-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created scratch directory");
    dir
}

/// A workspace root with a small tree inside it, and the outside file the escapes aim at.
///
/// ```text
/// <scratch>/
///   outside.txt          <- never reachable from the workspace
///   root/                <- the registered workspace
///     README.md
///     src/
///       main.rs
///     empty/
/// ```
fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let base = scratch(name);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("src")).expect("created src");
    std::fs::create_dir_all(root.join("empty")).expect("created empty");
    std::fs::write(base.join("outside.txt"), "outside secret").expect("wrote outside");
    std::fs::write(root.join("README.md"), "# readme\n").expect("wrote readme");
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").expect("wrote main");
    (base, root)
}

/// A registry with the fixture's root registered under [`WORKSPACE`].
fn registry(root: &Path) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    let fs: Arc<dyn girder_core::FileSystem> = Arc::new(LocalFileSystem);
    let workspaces = Arc::new(WorkspaceRegistry::new());
    workspaces
        .register(WORKSPACE, root, None, 0)
        .expect("registered workspace");
    register_workspace_file_methods(&mut registry, &fs, &workspaces);
    registry
}

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

async fn tree(
    registry: &RemoteRegistry,
    args: serde_json::Value,
) -> Result<serde_json::Value, RemoteError> {
    registry.dispatch("workspace", "tree", call(args)).await
}

async fn read_file(
    registry: &RemoteRegistry,
    args: serde_json::Value,
) -> Result<serde_json::Value, RemoteError> {
    registry.dispatch("workspace", "readFile", call(args)).await
}

/// The `path` values of a tree's entries, in the order they were returned.
fn paths(value: &serde_json::Value) -> Vec<String> {
    value["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|entry| entry["path"].as_str().expect("path string").to_owned())
        .collect()
}

/// The message of a refusal, which is where this build keeps its error codes.
fn refusal(error: &RemoteError) -> String {
    error.to_string()
}

// ---- tree -------------------------------------------------------------------------------------

#[tokio::test]
async fn tree_lists_the_direct_children_of_the_root() {
    let (base, root) = fixture("tree-root");
    let registry = registry(&root);

    let value = tree(&registry, serde_json::json!({ "workspaceId": WORKSPACE }))
        .await
        .expect("tree");

    // Names, not paths, on each entry: a tree view joins them itself, and a `./README.md` prefix
    // would be a second spelling of the same file.
    assert_eq!(paths(&value), vec!["README.md", "empty", "src"]);
    assert_eq!(value["path"], "");
    assert_eq!(value["truncated"], false);
    assert_eq!(value["depth"], 1);
    assert_eq!(value["entries"][0]["kind"], "file");
    assert_eq!(value["entries"][0]["size"], 9);
    assert_eq!(value["entries"][1]["kind"], "directory");
    // A directory has no byte size; `null` says "not applicable" where `0` would read as "empty".
    assert_eq!(value["entries"][1]["size"], serde_json::Value::Null);
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn tree_returns_a_subdirectory_with_workspace_relative_paths() {
    let (base, root) = fixture("tree-sub");
    let registry = registry(&root);

    let value = tree(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "src" }),
    )
    .await
    .expect("tree");

    assert_eq!(value["path"], "src");
    assert_eq!(paths(&value), vec!["src/main.rs"]);
    assert_eq!(value["entries"][0]["depth"], 1);
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn tree_descends_to_the_requested_depth() {
    let (base, root) = fixture("tree-depth");
    let registry = registry(&root);

    let shallow = tree(&registry, serde_json::json!({ "workspaceId": WORKSPACE }))
        .await
        .expect("tree");
    let deep = tree(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "depth": 2 }),
    )
    .await
    .expect("tree");

    // One level is the default because that is what a collapsed tree shows; asking for two adds the
    // grandchildren and nothing else.
    assert_eq!(paths(&shallow), vec!["README.md", "empty", "src"]);
    assert_eq!(
        paths(&deep),
        vec!["README.md", "empty", "src", "src/main.rs"]
    );
    assert_eq!(deep["entries"][3]["depth"], 2);
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn tree_reports_a_cut_listing_rather_than_returning_a_full_page() {
    let (base, root) = fixture("tree-limit");
    let registry = registry(&root);

    let value = tree(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "limit": 2 }),
    )
    .await
    .expect("tree");

    // Three children existed, two were returned: a caller that only compared `entries.len()` to its
    // own limit could not tell this from a directory that holds exactly two files.
    assert_eq!(paths(&value).len(), 2);
    assert_eq!(value["truncated"], true);
    assert_eq!(value["limit"], 2);
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn tree_defaults_are_the_documented_bounds() {
    assert_eq!(DEFAULT_TREE_ENTRY_LIMIT, 500);
    assert_eq!(MAX_TREE_ENTRY_LIMIT, 5_000);
    assert_eq!(MAX_TREE_DEPTH, 8);
    assert_eq!(DEFAULT_READ_MAX_BYTES, 256 * 1024);
}

#[tokio::test]
async fn tree_refuses_a_depth_outside_the_bound_instead_of_clamping() {
    let (base, root) = fixture("tree-depth-bound");
    let registry = registry(&root);

    // A clamp would answer a different question than the one asked, silently.
    let error = tree(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "depth": MAX_TREE_DEPTH + 1 }),
    )
    .await
    .expect_err("depth above the bound");
    assert!(refusal(&error).contains("depth"));

    let zero = tree(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "depth": 0 }),
    )
    .await
    .expect_err("zero depth");
    assert!(refusal(&zero).contains("depth"));
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn tree_refuses_a_path_that_names_a_file() {
    let (base, root) = fixture("tree-notdir");
    let registry = registry(&root);

    let error = tree(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "README.md" }),
    )
    .await
    .expect_err("a file is not a directory");
    assert!(refusal(&error).contains("not a directory"));
    let _ = std::fs::remove_dir_all(&base);
}

// ---- readFile ---------------------------------------------------------------------------------

#[tokio::test]
async fn read_file_returns_the_text_and_its_size() {
    let (base, root) = fixture("read-ok");
    let registry = registry(&root);

    let value = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "src/main.rs" }),
    )
    .await
    .expect("readFile");

    assert_eq!(value["text"], "fn main() {}\n");
    assert_eq!(value["path"], "src/main.rs");
    assert_eq!(value["size"], 13);
    assert_eq!(value["returnedBytes"], 13);
    assert_eq!(value["truncated"], false);
    assert_eq!(value["eof"], true);
    assert_eq!(value["notice"], serde_json::Value::Null);
    assert!(value["version"].as_str().is_some_and(|v| !v.is_empty()));
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn read_file_accepts_a_leading_dot_slash() {
    let (base, root) = fixture("read-dot");
    let registry = registry(&root);

    // Two spellings of one file must not resolve to two different places.
    let value = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "./README.md" }),
    )
    .await
    .expect("readFile");

    assert_eq!(value["text"], "# readme\n");
    assert_eq!(value["path"], "README.md");
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn read_file_cuts_a_large_file_and_says_so() {
    let (base, root) = fixture("read-cut");
    // Well below the preview ceiling, so it is read and cut rather than refused: the point here is
    // the truncation marker, not the ceiling.
    let mut body = String::new();
    for n in 0..200 {
        let _ = writeln!(body, "line {n}");
    }
    std::fs::write(root.join("big.txt"), &body).expect("wrote big file");
    let registry = registry(&root);

    let value = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "big.txt", "maxBytes": 40 }),
    )
    .await
    .expect("readFile");

    assert_eq!(value["truncated"], true);
    assert_eq!(value["eof"], false);
    assert_eq!(value["returnedBytes"], 40);
    assert_eq!(value["size"], body.len());
    assert_eq!(value["maxBytes"], 40);
    // The flag is for a client to branch on; the notice is what a person reads when the client does
    // not, and it is present so a silently-cut page cannot happen.
    let notice = value["notice"].as_str().expect("a notice on a cut page");
    assert!(notice.starts_with("truncated:"), "notice was {notice:?}");
    assert_eq!(value["text"].as_str(), Some(&body[..40]));
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn read_file_cuts_on_a_character_boundary() {
    let (base, root) = fixture("read-utf8");
    // Three-byte characters: a cut at byte 4 would land inside one.
    let body = "日本語のテキスト";
    std::fs::write(root.join("jp.txt"), body).expect("wrote utf8 file");
    let registry = registry(&root);

    let value = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "jp.txt", "maxBytes": 4 }),
    )
    .await
    .expect("readFile");

    // Cutting at an arbitrary byte would either panic on the slice or put U+FFFD at the end of a
    // page that is otherwise the file; 3 is the last boundary at or below 4.
    assert_eq!(value["returnedBytes"], 3);
    assert_eq!(value["text"], "日");
    assert_eq!(value["truncated"], true);
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn read_file_refuses_a_file_above_the_preview_ceiling() {
    let (base, root) = fixture("read-ceiling");
    // A sparse file would still report its length without costing the disk; a real one keeps the test
    // honest about what the seam would have to read.
    let sparse = root.join("huge.bin");
    let file = std::fs::File::create(&sparse).expect("created");
    file.set_len(workspace_files_rpc::MAX_PREVIEW_BYTES + 1)
        .expect("set length");
    drop(file);
    let registry = registry(&root);

    let error = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "huge.bin" }),
    )
    .await
    .expect_err("above the ceiling");

    // Refused with its size named, not read whole to be cut: reading eight gigabytes to show the
    // first paragraph is not a preview.
    let message = refusal(&error);
    assert!(message.contains("preview ceiling"), "message was {message}");
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn read_file_refuses_a_directory() {
    let (base, root) = fixture("read-dir");
    let registry = registry(&root);

    let error = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "src" }),
    )
    .await
    .expect_err("a directory is not a file");
    assert!(refusal(&error).contains("directory"));
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn read_file_refuses_a_missing_path() {
    let (base, root) = fixture("read-missing");
    let registry = registry(&root);

    let error = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "nope.txt" }),
    )
    .await
    .expect_err("missing file");
    assert!(refusal(&error).contains("does not exist"));
    let _ = std::fs::remove_dir_all(&base);
}

// ---- binary content ---------------------------------------------------------------------------

#[tokio::test]
async fn read_file_refuses_bytes_carrying_a_nul() {
    let (base, root) = fixture("read-nul");
    // Valid UTF-8, but a NUL: the marker a binary file usually leads with. Returning this as text
    // would hand a client something it will render as an empty-looking line.
    std::fs::write(root.join("blob.bin"), b"ELF\0\0\0rest").expect("wrote binary");
    let registry = registry(&root);

    let error = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "blob.bin" }),
    )
    .await
    .expect_err("binary file");
    let message = refusal(&error);
    assert!(
        message.contains("workspace-file/not-text"),
        "message was {message}"
    );
    assert!(message.contains("NUL"), "message was {message}");
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn read_file_refuses_text_that_is_not_utf8() {
    let (base, root) = fixture("read-invalid-utf8");
    // 0xFF is never a valid UTF-8 lead byte, and there is no NUL, so only the UTF-8 check can catch
    // this one.
    std::fs::write(root.join("latin1.txt"), [0x63, 0x61, 0x66, 0xe9, 0x0a]).expect("wrote latin1");
    let registry = registry(&root);

    let error = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "latin1.txt" }),
    )
    .await
    .expect_err("not UTF-8");
    let message = refusal(&error);
    assert!(
        message.contains("workspace-file/not-text"),
        "message was {message}"
    );
    assert!(message.contains("UTF-8"), "message was {message}");
    let _ = std::fs::remove_dir_all(&base);
}

// ---- the workspace boundary -------------------------------------------------------------------

#[tokio::test]
async fn read_file_refuses_a_relative_escape() {
    let (base, root) = fixture("escape-relative");
    let registry = registry(&root);

    for path in [
        "../outside.txt",
        "src/../../outside.txt",
        "./../outside.txt",
    ] {
        // The fixture's outside file exists and the process can read it, so the containment check is
        // the only thing between the caller and its contents.
        let error = read_file(
            &registry,
            serde_json::json!({ "workspaceId": WORKSPACE, "path": path }),
        )
        .await
        .expect_err("a relative escape must be refused");
        let message = refusal(&error);
        assert!(
            message.contains("escapes the workspace"),
            "`{path}` produced {message}"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn read_file_refuses_an_absolute_path_outside_the_workspace() {
    let (base, root) = fixture("escape-absolute");
    let registry = registry(&root);
    let outside = base.join("outside.txt").display().to_string();

    let error = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": outside }),
    )
    .await
    .expect_err("absolute escape");
    let message = refusal(&error);
    assert!(
        message.contains("escapes the workspace"),
        "message was {message}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn tree_refuses_a_relative_escape() {
    let (base, root) = fixture("escape-tree");
    let registry = registry(&root);

    let error = tree(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": ".." }),
    )
    .await
    .expect_err("relative escape");
    assert!(refusal(&error).contains("escapes the workspace"));
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_refuses_a_symlink_that_leaves_the_workspace() {
    let (base, root) = fixture("escape-symlink");
    // The lexical check sees an ordinary name inside the workspace; only the canonical check can
    // catch where it actually points. This is why containment is checked twice.
    std::os::unix::fs::symlink(base.join("outside.txt"), root.join("link.txt"))
        .expect("created symlink");
    let registry = registry(&root);

    let error = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "link.txt" }),
    )
    .await
    .expect_err("symlink escape");
    let message = refusal(&error);
    assert!(
        message.contains("resolves outside the workspace"),
        "message was {message}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_allows_a_symlink_that_stays_inside_the_workspace() {
    let (base, root) = fixture("symlink-inside");
    // Refusing every symlink would be simpler and wrong: a project that links its own files together
    // is ordinary, and only the target's location matters.
    std::os::unix::fs::symlink(root.join("README.md"), root.join("alias.md"))
        .expect("created symlink");
    let registry = registry(&root);

    let value = read_file(
        &registry,
        serde_json::json!({ "workspaceId": WORKSPACE, "path": "alias.md" }),
    )
    .await
    .expect("readFile");
    assert_eq!(value["text"], "# readme\n");
    let _ = std::fs::remove_dir_all(&base);
}

// ---- unknown workspace ------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_workspace_is_refused_by_both_methods() {
    let (base, root) = fixture("unknown-ws");
    let registry = registry(&root);

    let tree_error = tree(&registry, serde_json::json!({ "workspaceId": "nope" }))
        .await
        .expect_err("unknown workspace");
    let read_error = read_file(
        &registry,
        serde_json::json!({ "workspaceId": "nope", "path": "README.md" }),
    )
    .await
    .expect_err("unknown workspace");

    // An empty listing for an unknown id would be indistinguishable from an empty directory, which
    // is exactly the wrong conclusion for a browser to draw.
    for message in [refusal(&tree_error), refusal(&read_error)] {
        assert!(
            message.contains("unknown workspace"),
            "message was {message}"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn a_missing_workspace_id_is_an_argument_error() {
    let (base, root) = fixture("no-ws-id");
    let registry = registry(&root);

    let error = tree(&registry, serde_json::json!({}))
        .await
        .expect_err("no id");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    let _ = std::fs::remove_dir_all(&base);
}
