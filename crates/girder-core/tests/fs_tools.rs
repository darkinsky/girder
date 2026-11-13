//! Tests for the filesystem mutation tools.
//!
//! Four properties matter here, and each has tests that would fail if the property were dropped:
//!
//! - the workspace boundary holds for **both** paths of `move`/`copy`, not only the source;
//! - a delete is refused without the explicit confirmation the caller has to supply;
//! - a directory copy is recursive and cannot be aimed inside its own source;
//! - a refused or failed operation leaves the filesystem exactly as it was.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against the tool as written even before the crate root wires it in. `lib.rs` aliases the
//! crate to itself, so the module's `girder_core::…` imports resolve here too.

#[path = "../src/fs_tools.rs"]
mod fs_tools;

use std::path::{Path, PathBuf};

use girder_core::{ToolDefinition, ToolRegistry, ToolResult, ToolRunContext};

use fs_tools::{copy_tool, delete_tool, make_dir_tool, move_tool, register_fs_tools};

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-fs-tools-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

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

async fn run(tool: ToolDefinition, args: serde_json::Value) -> ToolResult {
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("the tools answer with a result rather than an error")
}

async fn call(workspace: &Path, which: ToolKind, args: serde_json::Value) -> ToolResult {
    let tool = match which {
        ToolKind::Move => move_tool(workspace.to_path_buf()),
        ToolKind::Copy => copy_tool(workspace.to_path_buf()),
        ToolKind::Delete => delete_tool(workspace.to_path_buf()),
        ToolKind::MakeDir => make_dir_tool(workspace.to_path_buf()),
    };
    run(tool, args).await
}

#[derive(Clone, Copy)]
enum ToolKind {
    Move,
    Copy,
    Delete,
    MakeDir,
}

// ---- move_path --------------------------------------------------------------------------------

#[tokio::test]
async fn a_file_is_renamed() {
    let dir = scratch("move-rename");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": "a.txt", "destination": "b.txt"}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert!(!dir.join("a.txt").exists(), "the source must be gone");
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).expect("read"),
        "body\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_directory_is_moved_with_its_contents() {
    let dir = scratch("move-dir");
    std::fs::create_dir_all(dir.join("tree/nested")).expect("created");
    std::fs::write(dir.join("tree/nested/deep.txt"), "deep\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": "tree", "destination": "moved"}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert!(!dir.join("tree").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("moved/nested/deep.txt")).expect("read"),
        "deep\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_move_onto_an_existing_file_is_refused_without_overwrite() {
    // Silently replacing the destination would destroy content the caller may not have read.
    let dir = scratch("move-exists");
    std::fs::write(dir.join("a.txt"), "from a\n").expect("written");
    std::fs::write(dir.join("b.txt"), "from b\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": "a.txt", "destination": "b.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("overwrite"),
        "{}",
        text_of(&result)
    );
    // Nothing moved: a refusal must not be a half-done operation.
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "from a\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).expect("read"),
        "from b\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_move_overwrites_when_asked_to() {
    let dir = scratch("move-overwrite");
    std::fs::write(dir.join("a.txt"), "from a\n").expect("written");
    std::fs::write(dir.join("b.txt"), "from b\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": "a.txt", "destination": "b.txt", "overwrite": true}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).expect("read"),
        "from a\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_move_onto_an_existing_directory_is_refused() {
    // Replacing a directory with a file is never what a caller meant, whatever `overwrite` says.
    let dir = scratch("move-onto-dir");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");
    std::fs::create_dir_all(dir.join("target")).expect("created");

    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": "a.txt", "destination": "target", "overwrite": true}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(dir.join("a.txt").exists());
    assert!(dir.join("target").is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_move_with_a_missing_source_is_refused() {
    let dir = scratch("move-missing");

    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": "nope.txt", "destination": "b.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("does not exist"),
        "{}",
        text_of(&result)
    );
    assert!(!dir.join("b.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_move_into_a_missing_parent_is_refused_and_creates_nothing() {
    let dir = scratch("move-no-parent");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": "a.txt", "destination": "missing/b.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(dir.join("a.txt").exists(), "the source must survive");
    assert!(
        !dir.join("missing").exists(),
        "a refused move must not leave a directory behind"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_move_out_of_the_workspace_is_refused_at_the_source() {
    let dir = scratch("move-escape-source");
    let outside = scratch("move-escape-source-outside");
    std::fs::write(outside.join("secret.txt"), "secret\n").expect("written");

    let escaped = outside.join("secret.txt");
    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": escaped.display().to_string(), "destination": "stolen.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("outside the workspace"),
        "{}",
        text_of(&result)
    );
    assert!(outside.join("secret.txt").exists());
    assert!(!dir.join("stolen.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn a_move_out_of_the_workspace_is_refused_at_the_destination() {
    // Checking only the source would make this the way to write outside the workspace.
    let dir = scratch("move-escape-destination");
    let outside = scratch("move-escape-destination-outside");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let escaped = outside.join("planted.txt");
    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"source": "a.txt", "destination": escaped.display().to_string()}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("outside the workspace"),
        "{}",
        text_of(&result)
    );
    assert!(dir.join("a.txt").exists(), "the source must not move");
    assert!(!outside.join("planted.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn a_parent_segment_escape_is_refused_for_both_paths() {
    let dir = scratch("move-dotdot");
    let workspace = dir.join("work");
    std::fs::create_dir_all(&workspace).expect("created");
    std::fs::write(dir.join("outside.txt"), "outside\n").expect("written");

    let result = call(
        &workspace,
        ToolKind::Move,
        serde_json::json!({"source": "../outside.txt", "destination": "inside.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(dir.join("outside.txt").exists());

    let result = call(
        &workspace,
        ToolKind::Move,
        serde_json::json!({"source": "inside.txt", "destination": "../planted.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(!dir.join("planted.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- copy_path --------------------------------------------------------------------------------

#[tokio::test]
async fn a_file_is_copied_and_the_source_remains() {
    let dir = scratch("copy-file");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "a.txt", "destination": "b.txt"}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "body\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).expect("read"),
        "body\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_directory_copy_is_recursive() {
    let dir = scratch("copy-dir");
    std::fs::create_dir_all(dir.join("tree/nested/deeper")).expect("created");
    std::fs::write(dir.join("tree/top.txt"), "top\n").expect("written");
    std::fs::write(dir.join("tree/nested/mid.txt"), "mid\n").expect("written");
    std::fs::write(dir.join("tree/nested/deeper/low.txt"), "low\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "tree", "destination": "clone"}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        std::fs::read_to_string(dir.join("clone/top.txt")).expect("read"),
        "top\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("clone/nested/mid.txt")).expect("read"),
        "mid\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("clone/nested/deeper/low.txt")).expect("read"),
        "low\n"
    );
    // Recursion must not have written back into the source.
    assert_eq!(
        std::fs::read_dir(dir.join("tree")).expect("listed").count(),
        2,
        "the source tree must keep exactly its original two entries"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_copy_into_its_own_source_is_refused() {
    // Without this check the copy is a directory that contains a copy of itself, forever.
    let dir = scratch("copy-self");
    std::fs::create_dir_all(dir.join("tree")).expect("created");
    std::fs::write(dir.join("tree/a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "tree", "destination": "tree/inside"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("inside itself"),
        "{}",
        text_of(&result)
    );
    assert!(!dir.join("tree/inside").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_copy_onto_an_existing_directory_is_refused_rather_than_merged() {
    // A merge has no single publish step, so it cannot be undone when it fails halfway.
    let dir = scratch("copy-onto-dir");
    std::fs::create_dir_all(dir.join("tree")).expect("created");
    std::fs::write(dir.join("tree/a.txt"), "from tree\n").expect("written");
    std::fs::create_dir_all(dir.join("existing")).expect("created");
    std::fs::write(dir.join("existing/keep.txt"), "keep\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "tree", "destination": "existing", "overwrite": true}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(text_of(&result).contains("merge"), "{}", text_of(&result));
    assert!(dir.join("existing/keep.txt").exists(), "nothing merged in");
    assert!(!dir.join("existing/a.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_copy_onto_an_existing_file_is_refused_without_overwrite() {
    let dir = scratch("copy-exists");
    std::fs::write(dir.join("a.txt"), "from a\n").expect("written");
    std::fs::write(dir.join("b.txt"), "from b\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "a.txt", "destination": "b.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).expect("read"),
        "from b\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_copy_overwrites_when_asked_to_and_leaves_no_staging_file() {
    let dir = scratch("copy-overwrite");
    std::fs::write(dir.join("a.txt"), "from a\n").expect("written");
    std::fs::write(dir.join("b.txt"), "from b\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "a.txt", "destination": "b.txt", "overwrite": true}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).expect("read"),
        "from a\n"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("listed")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("partial"))
        .collect();
    assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_copy_with_a_missing_source_is_refused_and_publishes_nothing() {
    let dir = scratch("copy-missing");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "nope.txt", "destination": "b.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("does not exist"),
        "{}",
        text_of(&result)
    );
    assert_eq!(
        std::fs::read_dir(&dir).expect("listed").count(),
        0,
        "a failed copy must leave the directory empty"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_copy_out_of_the_workspace_is_refused_at_the_source() {
    let dir = scratch("copy-escape-source");
    let outside = scratch("copy-escape-source-outside");
    std::fs::write(outside.join("secret.txt"), "secret\n").expect("written");

    let escaped = outside.join("secret.txt");
    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": escaped.display().to_string(), "destination": "stolen.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("outside the workspace"),
        "{}",
        text_of(&result)
    );
    assert!(!dir.join("stolen.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn a_copy_out_of_the_workspace_is_refused_at_the_destination() {
    let dir = scratch("copy-escape-destination");
    let outside = scratch("copy-escape-destination-outside");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let escaped = outside.join("planted.txt");
    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "a.txt", "destination": escaped.display().to_string()}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("outside the workspace"),
        "{}",
        text_of(&result)
    );
    assert!(!outside.join("planted.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

// ---- delete_path ------------------------------------------------------------------------------

#[tokio::test]
async fn a_delete_without_confirmation_is_refused() {
    // The confirmation is the decision itself: a delete is the one operation with no inverse.
    let dir = scratch("delete-unconfirmed");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(&dir, ToolKind::Delete, serde_json::json!({"path": "a.txt"})).await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(text_of(&result).contains("confirm"), "{}", text_of(&result));
    assert!(
        dir.join("a.txt").exists(),
        "the file must survive a refusal"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_delete_with_confirm_false_is_refused() {
    let dir = scratch("delete-false");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Delete,
        serde_json::json!({"path": "a.txt", "confirm": false}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(dir.join("a.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_file_is_deleted_with_confirmation() {
    let dir = scratch("delete-file");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Delete,
        serde_json::json!({"path": "a.txt", "confirm": true}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert!(!dir.join("a.txt").exists());
    assert!(dir.is_dir(), "the workspace itself must remain");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_directory_needs_recursive_and_is_then_deleted_with_its_contents() {
    let dir = scratch("delete-dir");
    std::fs::create_dir_all(dir.join("tree/nested")).expect("created");
    std::fs::write(dir.join("tree/nested/deep.txt"), "deep\n").expect("written");

    let refused = call(
        &dir,
        ToolKind::Delete,
        serde_json::json!({"path": "tree", "confirm": true}),
    )
    .await;
    assert!(refused.is_error, "{}", text_of(&refused));
    assert!(
        text_of(&refused).contains("recursive"),
        "{}",
        text_of(&refused)
    );
    assert!(dir.join("tree/nested/deep.txt").exists());

    let deleted = call(
        &dir,
        ToolKind::Delete,
        serde_json::json!({"path": "tree", "confirm": true, "recursive": true}),
    )
    .await;
    assert!(!deleted.is_error, "{}", text_of(&deleted));
    assert!(!dir.join("tree").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_workspace_root_is_never_deleted() {
    let dir = scratch("delete-root");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Delete,
        serde_json::json!({"path": ".", "confirm": true, "recursive": true}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("workspace root"),
        "{}",
        text_of(&result)
    );
    assert!(dir.join("a.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_delete_outside_the_workspace_is_refused() {
    let dir = scratch("delete-escape");
    let outside = scratch("delete-escape-outside");
    std::fs::write(outside.join("keep.txt"), "keep\n").expect("written");

    let escaped = outside.join("keep.txt");
    let result = call(
        &dir,
        ToolKind::Delete,
        serde_json::json!({"path": escaped.display().to_string(), "confirm": true}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("outside the workspace"),
        "{}",
        text_of(&result)
    );
    assert!(outside.join("keep.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn a_delete_of_a_missing_path_is_refused() {
    let dir = scratch("delete-missing");

    let result = call(
        &dir,
        ToolKind::Delete,
        serde_json::json!({"path": "nope.txt", "confirm": true}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("does not exist"),
        "{}",
        text_of(&result)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- make_dir ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_directory_is_created_with_its_parents() {
    let dir = scratch("mkdir-parents");

    let result = call(
        &dir,
        ToolKind::MakeDir,
        serde_json::json!({"path": "a/b/c"}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert!(dir.join("a/b/c").is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn without_parents_a_missing_intermediate_is_an_error() {
    let dir = scratch("mkdir-noparents");

    let result = call(
        &dir,
        ToolKind::MakeDir,
        serde_json::json!({"path": "a/b", "parents": false}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(!dir.join("a").exists(), "nothing may be created");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_existing_directory_is_reported_rather_than_refused() {
    // A caller that only needs the directory to exist has got what it asked for.
    let dir = scratch("mkdir-existing");
    std::fs::create_dir_all(dir.join("a")).expect("created");

    let result = call(&dir, ToolKind::MakeDir, serde_json::json!({"path": "a"})).await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("already exists"),
        "{}",
        text_of(&result)
    );
    assert!(dir.join("a").is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_path_occupied_by_a_file_is_refused() {
    let dir = scratch("mkdir-file");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::MakeDir,
        serde_json::json!({"path": "a.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("not a directory"),
        "{}",
        text_of(&result)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "body\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_make_dir_outside_the_workspace_is_refused() {
    let dir = scratch("mkdir-escape");
    let outside = scratch("mkdir-escape-outside");

    let escaped = outside.join("planted");
    let result = call(
        &dir,
        ToolKind::MakeDir,
        serde_json::json!({"path": escaped.display().to_string()}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("outside the workspace"),
        "{}",
        text_of(&result)
    );
    assert!(!outside.join("planted").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

// ---- registration and arguments ---------------------------------------------------------------

#[tokio::test]
async fn every_tool_is_registered_with_a_schema_that_names_its_arguments() {
    let registry = ToolRegistry::new();
    register_fs_tools(&registry, PathBuf::from("/tmp"));

    let mut names = registry.names();
    names.sort();
    assert_eq!(
        names,
        vec!["copy_path", "delete_path", "make_dir", "move_path"]
    );

    for name in ["copy_path", "delete_path", "make_dir", "move_path"] {
        let tool = registry.get(name).expect("registered");
        let required = tool
            .parameters
            .get("required")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(
            !required.is_empty(),
            "`{name}` must declare at least one required argument"
        );
    }

    // The confirmation must be part of the schema, not only of the implementation: a model reads
    // the schema to compose the call.
    let delete = registry.get("delete_path").expect("registered");
    let required: Vec<&str> = delete
        .parameters
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect()
        })
        .unwrap_or_default();
    assert!(required.contains(&"confirm"), "{required:?}");
}

#[tokio::test]
async fn a_missing_source_argument_is_refused_by_name() {
    let dir = scratch("args");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");

    let result = call(
        &dir,
        ToolKind::Move,
        serde_json::json!({"destination": "b.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("`source`"),
        "{}",
        text_of(&result)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_path_is_refused() {
    let dir = scratch("args-empty");

    let result = call(&dir, ToolKind::MakeDir, serde_json::json!({"path": "  "})).await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("non-empty"),
        "{}",
        text_of(&result)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- symlinks ---------------------------------------------------------------------------------

// A symlink is the one way a lexically-inside path leads outside, so the canonical half of the
// containment check has to be exercised separately from the lexical one.
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_directory_that_leads_outside_is_refused() {
    let dir = scratch("symlink-escape");
    let outside = scratch("symlink-escape-outside");
    std::fs::write(outside.join("secret.txt"), "secret\n").expect("written");
    std::os::unix::fs::symlink(&outside, dir.join("link")).expect("linked");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "link/secret.txt", "destination": "stolen.txt"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("resolves outside the workspace"),
        "{}",
        text_of(&result)
    );
    assert!(!dir.join("stolen.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_source_inside_a_copied_tree_is_named_rather_than_followed() {
    // Following a link while copying can recurse forever or copy content from outside the
    // workspace; naming the entry lets the caller decide which it wanted.
    let dir = scratch("symlink-in-tree");
    std::fs::create_dir_all(dir.join("tree")).expect("created");
    std::fs::write(dir.join("tree/real.txt"), "real\n").expect("written");
    std::os::unix::fs::symlink(dir.join("tree"), dir.join("tree/loop")).expect("linked");

    let result = call(
        &dir,
        ToolKind::Copy,
        serde_json::json!({"source": "tree", "destination": "clone"}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("symbolic link"),
        "{}",
        text_of(&result)
    );
    // The failure happened mid-tree; the staging tree must not have survived as a destination.
    assert!(!dir.join("clone").exists(), "no half-written destination");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn the_root_guard_holds_when_the_root_is_reached_through_a_link() {
    let dir = scratch("root-via-link");
    let outside = scratch("root-via-link-outside");
    std::fs::write(dir.join("a.txt"), "body\n").expect("written");
    std::os::unix::fs::symlink(&dir, outside.join("link")).expect("linked");

    // The workspace is spelled through the link, so the target's canonical form must be compared
    // against the root's canonical form rather than against the raw root path.
    let result = call(
        &outside.join("link"),
        ToolKind::Delete,
        serde_json::json!({"path": ".", "confirm": true, "recursive": true}),
    )
    .await;

    assert!(result.is_error, "{}", text_of(&result));
    assert!(
        text_of(&result).contains("workspace root"),
        "{}",
        text_of(&result)
    );
    assert!(dir.join("a.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}
