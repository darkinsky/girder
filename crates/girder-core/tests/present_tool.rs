//! Tests for the `present` tool.
//!
//! The property that matters is that the announcement is about a file that exists: a card a person clicks that leads
//! nowhere is worse than a model told to write the file first.

use std::sync::{Arc, Mutex};

use girder_core::{DeliverableKind, DeliverableSet, ToolResult, ToolRunContext, present_tool};

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-present-{name}-{}-{n}", std::process::id()));
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

async fn run(
    dir: &std::path::Path,
    set: &Arc<Mutex<DeliverableSet>>,
    args: serde_json::Value,
) -> ToolResult {
    let tool = present_tool(dir.to_path_buf(), Arc::clone(set));
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a presentation is a result")
}

#[tokio::test]
async fn an_existing_file_is_announced() {
    let dir = scratch("ok");
    std::fs::write(dir.join("report.md"), "# report").expect("written");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    let result = run(
        &dir,
        &set,
        serde_json::json!({"path": "report.md", "title": "The report"}),
    )
    .await;

    assert!(!result.is_error, "{}", text_of(&result));
    let set = set.lock().expect("unpoisoned");
    assert_eq!(set.len(), 1);
    assert_eq!(set.entries()[0].title, "The report");
    assert_eq!(set.entries()[0].bytes, Some(8), "the size is recorded");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_file_is_refused_before_the_announcement() {
    // A card a person clicks that leads nowhere is worse than a model told to write the file first.
    let dir = scratch("missing");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    let result = run(
        &dir,
        &set,
        serde_json::json!({"path": "absent.md", "title": "Nope"}),
    )
    .await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("does not exist"));
    assert_eq!(
        set.lock().expect("unpoisoned").len(),
        0,
        "nothing announced"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_declared_kind_wins_over_the_extension() {
    // A `.json` is sometimes a document and sometimes a data file, and guessing wrong opens it in the wrong
    // application.
    let dir = scratch("kind");
    std::fs::write(dir.join("data.json"), "{}").expect("written");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    run(
        &dir,
        &set,
        serde_json::json!({"path": "data.json", "title": "Data", "kind": "document"}),
    )
    .await;

    let set = set.lock().expect("unpoisoned");
    assert_eq!(set.entries()[0].kind, DeliverableKind::Document);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_undeclared_kind_falls_back_to_the_extension() {
    let dir = scratch("infer");
    std::fs::write(dir.join("sheet.csv"), "a,b").expect("written");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    run(
        &dir,
        &set,
        serde_json::json!({"path": "sheet.csv", "title": "Sheet"}),
    )
    .await;

    let set = set.lock().expect("unpoisoned");
    assert_eq!(set.entries()[0].kind, DeliverableKind::Spreadsheet);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn announcing_one_file_twice_keeps_the_first() {
    // A list that showed it twice would look like two outputs.
    let dir = scratch("twice");
    std::fs::write(dir.join("a.md"), "x").expect("written");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    run(
        &dir,
        &set,
        serde_json::json!({"path": "a.md", "title": "First"}),
    )
    .await;
    let second = run(
        &dir,
        &set,
        serde_json::json!({"path": "a.md", "title": "Second"}),
    )
    .await;

    assert!(!second.is_error);
    assert!(text_of(&second).contains("already presented"));
    let set = set.lock().expect("unpoisoned");
    assert_eq!(set.len(), 1);
    assert_eq!(set.entries()[0].title, "First");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_description_is_carried() {
    let dir = scratch("describe");
    std::fs::write(dir.join("a.md"), "x").expect("written");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    run(
        &dir,
        &set,
        serde_json::json!({"path": "a.md", "title": "T", "description": "about a"}),
    )
    .await;

    let set = set.lock().expect("unpoisoned");
    assert_eq!(set.entries()[0].description.as_deref(), Some("about a"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_title_is_refused() {
    let dir = scratch("title");
    std::fs::write(dir.join("a.md"), "x").expect("written");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    let result = run(&dir, &set, serde_json::json!({"path": "a.md"})).await;

    assert!(result.is_error);
    assert_eq!(set.lock().expect("unpoisoned").len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_path_is_refused() {
    let dir = scratch("path");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    let result = run(&dir, &set, serde_json::json!({"title": "T"})).await;

    assert!(result.is_error);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_directory_is_not_presented() {
    // A directory is not a file a person opens.
    let dir = scratch("dir");
    std::fs::create_dir_all(dir.join("sub")).expect("created");
    let set = Arc::new(Mutex::new(DeliverableSet::new()));

    let result = run(
        &dir,
        &set,
        serde_json::json!({"path": "sub", "title": "Sub"}),
    )
    .await;

    assert!(result.is_error);
    let _ = std::fs::remove_dir_all(&dir);
}
