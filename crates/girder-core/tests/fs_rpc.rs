//! Tests for the `fs.*` RPC methods.
//!
//! These answer "what is there" without reading it. The property that matters most is that absence is a fact rather
//! than an error: every caller would otherwise put its existence check in a failure branch.

use std::sync::Arc;

use girder_core::{LocalFileSystem, RemoteCall, RemoteRegistry};
use serde_json::json;

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-fsrpc-{name}-{}-{n}", std::process::id()));
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

fn registry() -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    let fs: Arc<dyn girder_core::FileSystem> = Arc::new(LocalFileSystem);
    girder_core::register_fs_methods(&mut registry, &fs);
    registry
}

// ---- stat -------------------------------------------------------------------------------------

#[tokio::test]
async fn stat_reports_a_file_with_its_size() {
    let dir = scratch("stat");
    std::fs::write(dir.join("a.txt"), "hello").expect("written");

    let value = registry()
        .dispatch(
            "fs",
            "stat",
            call(json!({"path": dir.join("a.txt").display().to_string()})),
        )
        .await
        .expect("stat");

    assert_eq!(value["exists"], true);
    assert_eq!(value["kind"], "file");
    assert_eq!(value["size"], 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stat_of_a_missing_path_is_a_fact_not_an_error() {
    // Every caller would otherwise put its existence check in a failure branch, where it reads like a bug.
    let dir = scratch("absent");

    let value = registry()
        .dispatch(
            "fs",
            "stat",
            call(json!({"path": dir.join("nope.txt").display().to_string()})),
        )
        .await
        .expect("stat");

    assert_eq!(value["exists"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stat_reports_a_directory() {
    let dir = scratch("statdir");

    let value = registry()
        .dispatch(
            "fs",
            "stat",
            call(json!({"path": dir.display().to_string()})),
        )
        .await
        .expect("stat");

    assert_eq!(value["kind"], "directory");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stat_without_a_path_is_refused() {
    let error = registry()
        .dispatch("fs", "stat", call(json!({})))
        .await
        .expect_err("missing path");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
}

// ---- list -------------------------------------------------------------------------------------

#[tokio::test]
async fn list_names_every_entry() {
    let dir = scratch("list");
    std::fs::write(dir.join("a.txt"), "x").expect("written");
    std::fs::create_dir_all(dir.join("sub")).expect("created");

    let value = registry()
        .dispatch(
            "fs",
            "list",
            call(json!({"path": dir.display().to_string()})),
        )
        .await
        .expect("list");

    let entries = value["entries"].as_array().expect("an array");
    assert_eq!(entries.len(), 2);
    assert_eq!(value["truncated"], false);
    assert_eq!(value["total"], 2);
    // A name, not a path: joining belongs to the caller, which knows its own root.
    let names: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"sub"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_listing_over_the_limit_says_it_stopped() {
    // A caller that received exactly the limit cannot otherwise tell whether more existed.
    let dir = scratch("limit");
    for index in 0..10 {
        std::fs::write(dir.join(format!("f{index}.txt")), "x").expect("written");
    }

    let value = registry()
        .dispatch(
            "fs",
            "list",
            call(json!({"path": dir.display().to_string(), "limit": 3})),
        )
        .await
        .expect("list");

    assert_eq!(value["entries"].as_array().map(Vec::len), Some(3));
    assert_eq!(value["truncated"], true);
    assert_eq!(
        value["total"], 10,
        "the total is reported even when the list is short"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_zero_limit_is_treated_as_one_rather_than_as_nothing() {
    // A caller that asked for a page asked for at least one entry; returning none would look like an empty directory.
    let dir = scratch("zero");
    std::fs::write(dir.join("a.txt"), "x").expect("written");

    let value = registry()
        .dispatch(
            "fs",
            "list",
            call(json!({"path": dir.display().to_string(), "limit": 0})),
        )
        .await
        .expect("list");

    assert_eq!(value["entries"].as_array().map(Vec::len), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn listing_a_missing_directory_is_an_error() {
    // Unlike `stat`, there is no useful "no" here: the caller asked for contents that cannot exist.
    let dir = scratch("nolist");

    let error = registry()
        .dispatch(
            "fs",
            "list",
            call(json!({"path": dir.join("absent").display().to_string()})),
        )
        .await
        .expect_err("missing");

    assert!(matches!(error, girder_core::RemoteError::Failed(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn list_without_a_path_is_refused() {
    let error = registry()
        .dispatch("fs", "list", call(json!({})))
        .await
        .expect_err("missing path");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
}
