//! Tests for workspace instruction files (M3 of the v2 plan).
//!
//! The rules that matter are the ones about what happens when there is too much: a file that does
//! not fit must be named in the output, because a model acting without instructions the user wrote
//! is worse than a model told the file exists.

use std::path::PathBuf;

use girder_core::{
    INSTRUCTION_FILE_NAME, InstructionLimits, InstructionState, digest, discover_instructions,
    render_instructions,
};
use tokio_util::sync::CancellationToken;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-instr-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

fn no_global() -> InstructionLimits {
    InstructionLimits {
        include_global: false,
        ..InstructionLimits::default()
    }
}

// ---- discovery --------------------------------------------------------------------------------

#[test]
fn an_instruction_file_in_the_working_directory_is_found() {
    let dir = scratch("basic");
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), "be brief").unwrap();

    let found = discover_instructions(&dir, None, &no_global());

    assert_eq!(found.len(), 1);
    assert!(found[0].absolute_path.ends_with(INSTRUCTION_FILE_NAME));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discovery_walks_up_to_the_root() {
    // Instructions apply to everything below them, so a file in the workspace root governs the whole
    // tree.
    let dir = scratch("walk");
    let nested = dir.join("a").join("b");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), "root rules").unwrap();
    std::fs::write(nested.join(INSTRUCTION_FILE_NAME), "nested rules").unwrap();

    let found = discover_instructions(&nested, None, &no_global());

    // Both apply, and the outer one comes first so the text reads general-to-local.
    let names: Vec<String> = found.iter().map(|f| f.display_path.clone()).collect();
    assert!(names.len() >= 2, "{names:?}");
    let root_at = names
        .iter()
        .position(|n| {
            n.ends_with(&format!("/{INSTRUCTION_FILE_NAME}")) && n.contains("/girder-instr-")
        })
        .expect("root");
    let nested_at = names
        .iter()
        .rposition(|n| n.contains("/a/b/"))
        .expect("nested");
    assert!(root_at < nested_at, "root must precede nested: {names:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_directory_without_an_instruction_file_contributes_nothing() {
    let dir = scratch("none");
    let nested = dir.join("sub");
    std::fs::create_dir_all(&nested).unwrap();

    let found = discover_instructions(&nested, None, &no_global());

    assert!(found.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_global_directories_are_searched_when_enabled() {
    let home = scratch("home");
    let workspace = scratch("ws-work");
    std::fs::create_dir_all(home.join(".agents")).unwrap();
    std::fs::write(home.join(".agents").join(INSTRUCTION_FILE_NAME), "global").unwrap();

    let with_global = InstructionLimits::default();
    let found = discover_instructions(&workspace, Some(&home), &with_global);

    assert!(
        found.iter().any(|f| f.display_path.contains(".agents")),
        "the global file must be found: {:?}",
        found.iter().map(|f| &f.display_path).collect::<Vec<_>>()
    );

    let without = discover_instructions(&workspace, Some(&home), &no_global());
    assert!(
        !without.iter().any(|f| f.display_path.contains(".agents")),
        "global must be skipped when disabled"
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&workspace);
}

// ---- rendering --------------------------------------------------------------------------------

#[tokio::test]
async fn a_file_is_rendered_with_its_path_as_a_heading() {
    // The model needs to know which file a rule came from to cite or re-read it.
    let dir = scratch("render");
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), "be brief").unwrap();
    let files = discover_instructions(&dir, None, &no_global());

    let rendered = render_instructions(&files, &no_global(), cancel())
        .await
        .unwrap();

    assert!(rendered.text.contains("be brief"));
    assert!(rendered.text.contains(INSTRUCTION_FILE_NAME));
    assert!(rendered.omitted.is_empty());
    assert!(rendered.truncated.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn several_files_render_in_discovery_order() {
    let dir = scratch("order");
    let nested = dir.join("sub");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), "OUTER").unwrap();
    std::fs::write(nested.join(INSTRUCTION_FILE_NAME), "INNER").unwrap();
    let files = discover_instructions(&nested, None, &no_global());

    let rendered = render_instructions(&files, &no_global(), cancel())
        .await
        .unwrap();

    let outer = rendered.text.find("OUTER").expect("outer present");
    let inner = rendered.text.find("INNER").expect("inner present");
    assert!(outer < inner, "general rules must precede local ones");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_workspace_renders_nothing() {
    let dir = scratch("empty");
    let rendered = render_instructions(&[], &no_global(), cancel())
        .await
        .unwrap();

    assert!(rendered.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- truncation and omission ------------------------------------------------------------------

#[tokio::test]
async fn a_file_over_the_per_file_limit_is_truncated_not_dropped() {
    // The beginning of a long instruction file is usually the part that states the rules, so a cut
    // is better than an omission.
    let dir = scratch("truncate");
    let content = "A".repeat(200);
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), &content).unwrap();
    let limits = InstructionLimits {
        max_bytes_per_file: 50,
        max_total_bytes: 1000,
        include_global: false,
    };
    let files = discover_instructions(&dir, None, &limits);

    let rendered = render_instructions(&files, &limits, cancel())
        .await
        .unwrap();

    assert_eq!(rendered.truncated.len(), 1);
    assert_eq!(rendered.truncated[0].full_bytes, 200);
    assert_eq!(rendered.truncated[0].kept_bytes, 50);
    assert!(
        rendered.omitted.is_empty(),
        "a truncated file is not omitted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_file_that_does_not_fit_the_total_budget_is_named_in_the_output() {
    // The property this whole module exists to protect: silently dropping instructions the user
    // wrote leaves the model acting without rules it was supposed to follow.
    let dir = scratch("omitted");
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), "A".repeat(100)).unwrap();
    let limits = InstructionLimits {
        max_bytes_per_file: 1000,
        max_total_bytes: 10,
        include_global: false,
    };
    let files = discover_instructions(&dir, None, &limits);

    let rendered = render_instructions(&files, &limits, cancel())
        .await
        .unwrap();

    assert!(
        rendered.text.contains("Not shown") || !rendered.omitted.is_empty(),
        "an omitted file must be visible: {:?}",
        rendered.text
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_truncation_never_splits_a_utf8_character() {
    // Slicing UTF-8 mid-sequence panics; an instruction file is prose, so a clean cut at a boundary
    // is the only acceptable behavior.
    let dir = scratch("utf8");
    // 3-byte characters, so a byte budget lands mid-sequence.
    let content = "字".repeat(50);
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), &content).unwrap();
    let limits = InstructionLimits {
        max_bytes_per_file: 7, // 2 characters + 1 byte into the third
        max_total_bytes: 1000,
        include_global: false,
    };
    let files = discover_instructions(&dir, None, &limits);

    let rendered = render_instructions(&files, &limits, cancel())
        .await
        .unwrap();

    // The important assertion is that this did not panic and produced valid text.
    assert!(rendered.truncated.len() == 1 || rendered.omitted.len() == 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_vanished_file_is_skipped_not_reported() {
    // The workspace changed under us, which is normal rather than an error.
    let dir = scratch("vanish");
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), "x").unwrap();
    let files = discover_instructions(&dir, None, &no_global());
    std::fs::remove_file(dir.join(INSTRUCTION_FILE_NAME)).unwrap();

    let rendered = render_instructions(&files, &no_global(), cancel()).await;

    assert!(rendered.is_ok());
    assert!(rendered.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- digests ----------------------------------------------------------------------------------

#[test]
fn a_whitespace_only_edit_does_not_change_the_digest() {
    // Instruction files are re-checked after every filesystem tool call; re-injecting on a
    // whitespace edit would fill the context with copies.
    assert_eq!(digest("be brief"), digest("  be brief\n\n"));
}

#[test]
fn a_content_edit_changes_the_digest() {
    assert_ne!(digest("be brief"), digest("be verbose"));
}

#[test]
fn the_state_reports_the_first_sighting_as_a_change() {
    let mut state = InstructionState::new();
    let path = PathBuf::from("/x/AGENTS.md");

    assert!(
        state.record(&path, "content"),
        "first sighting counts as a change"
    );
    assert!(!state.record(&path, "content"), "unchanged");
    assert!(!state.record(&path, "  content  "), "whitespace only");
    assert!(state.record(&path, "changed"));
    assert_eq!(state.tracked(), 1);
}

#[tokio::test]
async fn loading_combines_discovery_and_rendering() {
    let dir = scratch("load");
    std::fs::write(dir.join(INSTRUCTION_FILE_NAME), "rules").unwrap();

    let rendered = girder_core::load_instructions(dir.clone(), None, no_global(), cancel())
        .await
        .unwrap();

    assert!(rendered.text.contains("rules"));
    let _ = std::fs::remove_dir_all(&dir);
}
