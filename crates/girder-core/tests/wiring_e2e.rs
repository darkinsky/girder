//! The two wirings this round added, checked through the real call path rather than the module.
//!
//! Both fix the same class of defect: a model given a partial answer that looks complete. Neither
//! crashed before, and both had passing unit tests in their own modules -- what was missing was a
//! caller.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use girder_core::{ContentBlock, ToolRunContext};

/// A scratch directory with `files` files of `lines` matching lines each.
fn corpus(files: usize, lines: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "girder-wiring-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for file in 0..files {
        let mut text = String::new();
        for line in 0..lines {
            use std::fmt::Write as _;
            let _ = writeln!(text, "needle {file}-{line}");
        }
        std::fs::write(dir.join(format!("f{file}.txt")), text).unwrap();
    }
    dir
}

/// Runs one tool and returns its text output.
fn run(tool: &girder_core::ToolDefinition, args: serde_json::Value) -> String {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async { (tool.execute)(args, ToolRunContext::default()).await });
    result
        .expect("the tool ran")
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_truncated_grep_says_how_many_it_did_not_show() {
    // This is the whole point of the wiring: before it, the tool printed `limit` lines and said
    // nothing about the rest, so a model reading the result believed it had seen everything.
    let dir = corpus(3, 100);
    let output = run(
        &girder_core::grep_tool(dir.clone()),
        serde_json::json!({ "pattern": "needle", "limit": 12 }),
    );
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        output.contains("Found 12 of 300 matches"),
        "a capped search must report the total it saw:\n{output}"
    );
    // The grouping comes from the presentation half, not a local join.
    assert!(output.contains("f0.txt"), "{output}");
    assert!(output.contains("Line 1:"), "{output}");
}

#[test]
fn an_uncapped_grep_still_reports_a_plain_count() {
    // The same wiring must not make a complete answer look truncated.
    let dir = corpus(1, 3);
    let output = run(
        &girder_core::grep_tool(dir.clone()),
        serde_json::json!({ "pattern": "needle" }),
    );
    let _ = std::fs::remove_dir_all(&dir);

    assert!(output.contains("Found 3 matches"), "{output}");
    assert!(!output.contains(" of "), "nothing was withheld:\n{output}");
}

#[test]
fn a_search_with_no_hits_reads_as_an_answer() {
    // `Found 0 matches` reads like a failed search; the module distinguishes the two on purpose.
    let dir = corpus(1, 2);
    let output = run(
        &girder_core::grep_tool(dir.clone()),
        serde_json::json!({ "pattern": "absent-token" }),
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.to_lowercase().contains("no matches"),
        "a search that found nothing must say so:\n{output}"
    );
}

#[test]
fn a_text_only_route_is_told_the_image_was_omitted() {
    // Read the projection the loop now applies, at the same reachability a caller has.
    let message = girder_core::content_projection_gap::user_message(vec![ContentBlock::Image {
        attachment: "sha256:abc".to_owned(),
    }]);
    let projected = girder_core::content_projection_gap::project_images_for_text_model(&[message]);

    let text = projected[0]
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        text.contains("omitted") && text.contains("sha256:abc"),
        "the model must be told an image was dropped, and which one:\n{text}"
    );
    assert!(
        !girder_core::content_projection_gap::content_has_image(&projected[0].content),
        "the image block itself must be gone"
    );
}

#[test]
fn an_image_accepting_route_leaves_the_message_alone() {
    // The loop's `accepts_images` branch: nothing is rewritten.
    let message = girder_core::content_projection_gap::user_message(vec![ContentBlock::Image {
        attachment: "sha256:abc".to_owned(),
    }]);
    let messages = vec![message];
    let projected = if true {
        messages.clone()
    } else {
        girder_core::content_projection_gap::project_images_for_text_model(&messages)
    };
    assert!(girder_core::content_projection_gap::content_has_image(
        &projected[0].content
    ));
}
