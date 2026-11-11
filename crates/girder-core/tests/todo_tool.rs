//! Tests for the `todo_write` tool.
//!
//! The property that matters is that what a caller reads back is what the model wrote, and that a
//! list the tool refused never replaced a list the model had already got right: the file is the last
//! known good plan, and a rejected update that overwrote it would lose the record of finished work.

use std::path::{Path, PathBuf};

use girder_core::{ToolResult, ToolRunContext, read_todos, todo_tool};

/// A fresh directory per test.
///
/// The pid separates concurrent `cargo test` processes and the counter separates tests within one
/// process, which run on separate threads and would otherwise share a directory.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-todo-{name}-{}-{n}", std::process::id()));
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

async fn write(list: &Path, args: serde_json::Value) -> ToolResult {
    let tool = todo_tool(list.to_path_buf());
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a list write is a result")
}

/// A five-step list with two done and one active, used where the shape of the list is not the point.
fn five_steps() -> serde_json::Value {
    serde_json::json!({
        "todos": [
            { "content": "read the failing test", "status": "completed" },
            { "content": "find the off-by-one", "status": "completed" },
            { "content": "fix the bounds check", "status": "in_progress" },
            { "content": "add a regression test", "status": "pending" },
            { "content": "run the suite", "status": "pending" }
        ]
    })
}

#[tokio::test]
async fn a_written_list_reads_back_identical() {
    let dir = scratch("round-trip");
    let path = dir.join("todos.json");

    let result = write(&path, five_steps()).await;
    assert!(!result.is_error, "{}", text_of(&result));

    let todos = read_todos(&path);
    assert_eq!(todos.len(), 5, "every todo is persisted");
    assert_eq!(todos[0].content, "read the failing test");
    assert_eq!(todos[2].content, "fix the bounds check");
    // Compared through the wire encoding rather than the Rust variant names, so this keeps checking
    // the on-disk format even if the type is re-exported under a different name.
    let statuses: Vec<serde_json::Value> = todos
        .iter()
        .map(|todo| serde_json::to_value(todo.status).expect("a status has a JSON form"))
        .collect();
    let expected = [
        "completed",
        "completed",
        "in_progress",
        "pending",
        "pending",
    ]
    .map(serde_json::Value::from);
    assert_eq!(statuses, expected);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_file_is_created_even_when_its_directory_does_not_exist() {
    // The path comes from the session, not the model, and the first call of a run arrives before
    // anything has created that directory.
    let dir = scratch("nested");
    let path = dir.join("state").join("todos.json");

    let result = write(&path, five_steps()).await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(read_todos(&path).len(), 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_active_steps_are_refused() {
    let dir = scratch("two-active");
    let path = dir.join("todos.json");

    let result = write(
        &path,
        serde_json::json!({
            "todos": [
                { "content": "a", "status": "in_progress" },
                { "content": "b", "status": "in_progress" }
            ]
        }),
    )
    .await;

    assert!(result.is_error, "two active steps are refused");
    assert!(
        text_of(&result).contains('2'),
        "the message says how many were active: {}",
        text_of(&result)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_refused_list_leaves_the_previous_list_intact() {
    let dir = scratch("kept");
    let path = dir.join("todos.json");
    assert!(!write(&path, five_steps()).await.is_error);

    let result = write(
        &path,
        serde_json::json!({
            "todos": [
                { "content": "a", "status": "in_progress" },
                { "content": "b", "status": "in_progress" }
            ]
        }),
    )
    .await;

    assert!(result.is_error);
    let kept = read_todos(&path);
    assert_eq!(kept.len(), 5, "the last good list survived the refusal");
    assert_eq!(kept[2].content, "fix the bounds check");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn empty_content_is_refused() {
    let dir = scratch("empty");
    let path = dir.join("todos.json");

    for content in ["", "   "] {
        let result = write(
            &path,
            serde_json::json!({ "todos": [{ "content": content, "status": "pending" }] }),
        )
        .await;

        assert!(
            result.is_error,
            "`{content:?}` is not a concrete step, and the refusal is a result the model can act on"
        );
        assert!(read_todos(&path).is_empty(), "nothing was written");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_status_is_refused() {
    let dir = scratch("status");
    let path = dir.join("todos.json");

    let result = write(
        &path,
        serde_json::json!({ "todos": [{ "content": "a", "status": "done" }] }),
    )
    .await;

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("done"),
        "the message names the value the model sent: {}",
        text_of(&result)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_summary_counts_completed_steps_and_marks_each_one() {
    let dir = scratch("summary");
    let path = dir.join("todos.json");

    let result = write(&path, five_steps()).await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        text_of(&result),
        "2/5 completed\n\
         - [x] read the failing test\n\
         - [x] find the off-by-one\n\
         - [>] fix the bounds check\n\
         - [ ] add a regression test\n\
         - [ ] run the suite"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_summary_keeps_the_order_the_model_sent() {
    // A checklist that reorders itself between calls reads as if the plan had changed.
    let dir = scratch("order");
    let path = dir.join("todos.json");

    let result = write(
        &path,
        serde_json::json!({
            "todos": [
                { "content": "later", "status": "pending" },
                { "content": "earlier", "status": "completed" }
            ]
        }),
    )
    .await;

    assert_eq!(
        text_of(&result),
        "1/2 completed\n- [ ] later\n- [x] earlier"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_list_is_written_and_summarized_as_zero() {
    let dir = scratch("zero");
    let path = dir.join("todos.json");

    let result = write(&path, serde_json::json!({ "todos": [] })).await;

    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(text_of(&result), "0/0 completed");
    assert!(read_todos(&path).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_file_reads_as_an_empty_list() {
    let dir = scratch("missing");

    let todos = read_todos(&dir.join("never-written.json"));

    assert!(todos.is_empty(), "scratch state that is absent is nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_content_reads_as_an_empty_list() {
    // A truncated document is what a non-atomic writer leaves behind; this reader must not panic on
    // one, because it also runs against files written by an older version.
    let dir = scratch("malformed");
    let path = dir.join("todos.json");
    std::fs::write(&path, "{ not json").expect("written");

    assert!(read_todos(&path).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
