//! Tests for `glob` and `grep`.
//!
//! Both tools must distinguish "nothing matched" from "I stopped looking": a model told the first stops, and one told
//! the second asks a narrower question.

use girder_core::{ToolResult, ToolRunContext, glob_tool, grep_tool};

fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-search-{name}-{}-{n}", std::process::id()));
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

async fn run_glob(dir: &std::path::Path, args: serde_json::Value) -> String {
    let tool = glob_tool(dir.to_path_buf());
    let result = (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a lookup is a result");
    text_of(&result)
}

async fn run_grep(dir: &std::path::Path, args: serde_json::Value) -> String {
    let tool = grep_tool(dir.to_path_buf());
    let result = (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a lookup is a result");
    text_of(&result)
}

/// A small tree with a nested directory and an ignored one.
fn tree(name: &str) -> std::path::PathBuf {
    let dir = scratch(name);
    std::fs::create_dir_all(dir.join("src")).expect("created");
    std::fs::create_dir_all(dir.join("target")).expect("created");
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").expect("written");
    std::fs::write(dir.join("src/lib.rs"), "pub fn helper() {}\n").expect("written");
    std::fs::write(dir.join("src/app.ts"), "export const a = 1;\n").expect("written");
    std::fs::write(dir.join("target/ignored.rs"), "fn ignored() {}\n").expect("written");
    dir
}

// ---- glob -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_suffix_pattern_finds_files_at_any_depth() {
    let dir = tree("glob-suffix");

    let out = run_glob(&dir, serde_json::json!({"pattern": "*.rs"})).await;

    assert!(out.contains("main.rs"), "{out}");
    assert!(out.contains("lib.rs"), "{out}");
    assert!(!out.contains("app.ts"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_ignored_directory_is_not_walked() {
    // Searching a project should not mean searching its build output.
    let dir = tree("glob-ignored");

    let out = run_glob(&dir, serde_json::json!({"pattern": "*.rs"})).await;

    assert!(!out.contains("ignored.rs"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_prefix_pattern_is_honoured() {
    let dir = tree("glob-prefix");

    let out = run_glob(&dir, serde_json::json!({"pattern": "lib*"})).await;

    assert!(out.contains("lib.rs"), "{out}");
    assert!(!out.contains("main.rs"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_directory_prefix_narrows_the_walk() {
    let dir = tree("glob-dir");

    let out = run_glob(&dir, serde_json::json!({"pattern": "src/*.rs"})).await;

    assert!(out.contains("lib.rs"), "{out}");
    assert!(!out.contains("main.rs"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn no_match_says_so() {
    let dir = tree("glob-none");

    let out = run_glob(&dir, serde_json::json!({"pattern": "*.xyz"})).await;

    assert!(out.contains("no files match"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_limit_is_reported_rather_than_silently_applied() {
    // A model that believed a capped list was complete would draw a conclusion it has not earned.
    let dir = scratch("glob-limit");
    for index in 0..10 {
        std::fs::write(dir.join(format!("f{index}.rs")), "").expect("written");
    }

    let out = run_glob(&dir, serde_json::json!({"pattern": "*.rs", "limit": 3})).await;

    assert!(out.contains("stopped after 3"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_pattern_is_refused() {
    let dir = scratch("glob-missing");
    let tool = glob_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({}), ToolRunContext::default())
        .await
        .expect("a result");

    assert!(result.is_error);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_star_matches_every_name() {
    // The walk uses `*` as its matcher, so it must accept everything.
    let dir = scratch("glob-star");
    std::fs::write(dir.join("a.anything"), "x").expect("written");

    let tool = glob_tool(dir.clone());
    let result = futures::executor::block_on((tool.execute)(
        serde_json::json!({"pattern": "*"}),
        ToolRunContext::default(),
    ))
    .expect("a result");

    assert!(text_of(&result).contains("a.anything"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- grep -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_pattern_is_found_with_its_file_and_line() {
    let dir = scratch("grep-find");
    std::fs::write(dir.join("a.txt"), "first\nneedle here\n").expect("written");

    let out = run_grep(&dir, serde_json::json!({"pattern": "needle"})).await;

    assert!(out.contains("a.txt"), "{out}");
    assert!(out.contains(":2:"), "the line number is one-based: {out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_search_is_case_insensitive_by_default() {
    let dir = scratch("grep-case");
    std::fs::write(dir.join("a.txt"), "Needle\n").expect("written");

    let out = run_grep(&dir, serde_json::json!({"pattern": "needle"})).await;

    assert!(out.contains("Needle"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_case_sensitive_search_respects_the_flag() {
    let dir = scratch("grep-exact");
    std::fs::write(dir.join("a.txt"), "Needle\nlowercase needle\n").expect("written");

    let out = run_grep(
        &dir,
        serde_json::json!({"pattern": "Needle", "case_sensitive": true}),
    )
    .await;

    assert!(out.contains("Needle"), "{out}");
    assert!(!out.contains("lowercase"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn grep_of_no_match_says_so() {
    let dir = scratch("grep-none");
    std::fs::write(dir.join("a.txt"), "nothing\n").expect("written");

    let out = run_grep(&dir, serde_json::json!({"pattern": "absent"})).await;

    assert!(out.contains("no matches"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_path_argument_narrows_the_search() {
    let dir = tree("grep-path");
    std::fs::write(dir.join("main.rs"), "fn needle() {}\n").expect("written");
    std::fs::write(dir.join("src/lib.rs"), "fn needle() {}\n").expect("written");

    let out = run_grep(
        &dir,
        serde_json::json!({"pattern": "needle", "path": "src"}),
    )
    .await;

    assert!(out.contains("lib.rs"), "{out}");
    assert!(!out.contains("main.rs"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_binary_file_is_skipped_rather_than_reported() {
    // The caller asked where a pattern appears, not which files could not be decoded.
    let dir = scratch("grep-binary");
    std::fs::write(dir.join("a.bin"), [0xff, 0xfe, 0x00, 0x01]).expect("written");
    std::fs::write(dir.join("b.txt"), "needle\n").expect("written");

    let out = run_grep(&dir, serde_json::json!({"pattern": "needle"})).await;

    assert!(out.contains("b.txt"), "{out}");
    assert!(!out.contains("a.bin"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn grep_without_a_pattern_is_refused() {
    let dir = scratch("grep-missing");
    let tool = grep_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({}), ToolRunContext::default())
        .await
        .expect("a result");

    assert!(result.is_error);
    let _ = std::fs::remove_dir_all(&dir);
}
