//! Tests for the `instructions` tool.
//!
//! The property that matters is that the answer distinguishes "there is no instruction file" from
//! "the search did not look where the file is": a model told nothing concludes the workspace imposes
//! no rules, and acts on that conclusion.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the tool as written even before the crate root wires it in. It resolves `girder_core::…`
// through the alias the library declares for itself, which is why the same source builds here.
#[path = "../src/instructions_tool.rs"]
mod instructions_tool;

use std::path::PathBuf;

use girder_core::{ToolResult, ToolRunContext};

use instructions_tool::{MAX_OUTPUT_BYTES, instructions_tool};

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-instructions-tool-{name}-{}-{n}",
        std::process::id()
    ));
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

/// Runs the tool against `workspace` with a synthetic home, never the real one.
async fn run(
    workspace: &std::path::Path,
    home: Option<PathBuf>,
    args: serde_json::Value,
) -> ToolResult {
    let tool = instructions_tool(workspace.to_path_buf(), home);
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a listing is a result")
}

#[tokio::test]
async fn nothing_found_names_the_paths_that_were_searched() {
    // "No file" and "wrong directory" are the same silence unless the answer says where it looked.
    let dir = scratch("none");
    let workspace = dir.join("work");
    let home = dir.join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");

    let result = run(&workspace, Some(home.clone()), serde_json::json!({})).await;

    assert!(!result.is_error, "finding nothing is not an error");
    let text = text_of(&result);
    assert!(text.contains("no instruction files found"), "{text}");
    assert!(
        text.contains(&workspace.join("AGENTS.md").display().to_string()),
        "the workspace candidate must be named: {text}"
    );
    assert!(
        text.contains(&home.join(".agents").join("AGENTS.md").display().to_string()),
        "the user-global candidate must be named: {text}"
    );
    assert!(
        text.contains(&home.join(".girder").join("AGENTS.md").display().to_string()),
        "the harness-global candidate must be named: {text}"
    );
    assert!(text.contains("AGENTS.md"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn without_a_home_the_answer_says_the_globals_were_not_searched() {
    // Claiming a directory was searched when there was no home to search it under is the exact
    // confusion this tool exists to prevent.
    let dir = scratch("nohome");
    let workspace = dir.join("work");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let result = run(&workspace, None, serde_json::json!({})).await;

    let text = text_of(&result);
    assert!(text.contains("no instruction files found"), "{text}");
    assert!(text.contains("No home directory was given"), "{text}");
    assert!(!text.contains(".agents/AGENTS.md"), "{text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn one_file_is_listed_with_its_path_and_size() {
    let dir = scratch("one");
    let workspace = dir.join("work");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let body = "# Rules\n\nBe brief.\n";
    std::fs::write(workspace.join("AGENTS.md"), body).expect("written");

    let result = run(&workspace, None, serde_json::json!({})).await;

    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains("found 1 instruction file(s)"), "{text}");
    assert!(
        text.contains(&workspace.join("AGENTS.md").display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&format!("{} bytes", body.len())),
        "the size on disk must be reported: {text}"
    );
    assert!(text.contains("digest "), "{text}");
    assert!(
        !text.contains("Be brief"),
        "content must not be included unless it was asked for: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn full_includes_the_content_of_each_file() {
    let dir = scratch("full");
    let workspace = dir.join("work");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "# Rules\n\nBe brief.\n").expect("written");

    let result = run(&workspace, None, serde_json::json!({"full": true})).await;

    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains("Be brief."), "{text}");
    assert!(!text.contains("[truncated:"), "nothing was cut: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_large_file_is_cut_and_the_cut_is_stated() {
    // Silently returning a prefix would tell the model it had read rules it had not.
    let dir = scratch("large");
    let workspace = dir.join("work");
    std::fs::create_dir_all(&workspace).expect("workspace");
    // Comfortably past the whole-answer cap, and every line distinct so a cut can be located.
    let body: String = (0..(MAX_OUTPUT_BYTES / 16 + 200)).fold(String::new(), |mut text, index| {
        use std::fmt::Write as _;
        let _ = writeln!(text, "rule {index:06} applies here");
        text
    });
    assert!(
        body.len() > MAX_OUTPUT_BYTES,
        "the fixture must exceed the cap"
    );
    std::fs::write(workspace.join("AGENTS.md"), &body).expect("written");

    let result = run(&workspace, None, serde_json::json!({"full": true})).await;

    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(
        text.contains("[truncated:"),
        "the cut must be stated: {text}"
    );
    assert!(
        text.contains("of this file"),
        "the cut must say how much: {text}"
    );
    assert!(
        text.contains("rule 000000"),
        "the beginning is kept: {text}"
    );
    assert!(
        !text.contains("rule 999999"),
        "the end must not fit inside the cap"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn multiple_files_are_listed_outermost_first() {
    // Instructions apply to everything below them, so the general rule must read before the local one
    // that refines it.
    let dir = scratch("order");
    // Discovery walks up from the workspace, so the outer file sits above it and the local one is in
    // the workspace itself.
    let outer = dir.join("outer");
    let workspace = outer.join("a").join("b");
    std::fs::create_dir_all(&workspace).expect("nested");
    std::fs::write(outer.join("AGENTS.md"), "root rules").expect("root");
    std::fs::write(workspace.join("AGENTS.md"), "nested rules").expect("nested");

    let result = run(&workspace, None, serde_json::json!({"full": true})).await;

    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains("found 2 instruction file(s)"), "{text}");

    let root = text.find("root rules").expect("root rules listed");
    let nested_rules = text.find("nested rules").expect("nested rules listed");
    assert!(
        root < nested_rules,
        "the outer file must come first: {text}"
    );

    let root_path = outer.join("AGENTS.md").display().to_string();
    let nested_path = workspace.join("AGENTS.md").display().to_string();
    let root_at = text.find(&root_path).expect("root path listed");
    let nested_at = text.find(&nested_path).expect("nested path listed");
    assert!(root_at < nested_at, "paths follow discovery order: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}
