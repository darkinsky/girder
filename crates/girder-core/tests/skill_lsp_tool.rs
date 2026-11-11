//! Tests for the `skill` and `lsp` tools.
//!
//! # Why these two are tested together
//!
//! Both sit on a seam that converts a convention: a set of providers becomes one catalog, and a one-based
//! character position becomes a zero-based UTF-16 column. Both conversions are invisible on the easy input —
//! one provider, an ASCII line — and both fail in a way that still looks right, which is why every case below
//! names the input that would have caught it.
//!
//! # Why the LSP position tests answer only at the converted column
//!
//! [`StaticLsp`] answers at an exact `(line, character)` and reports `Nothing` anywhere else. A test that
//! expected a text answer at column *n* therefore fails when the tool sends any other column, which makes the
//! assertion a statement about the conversion rather than about the tool's formatting. Nothing here compares
//! the converter against itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use girder_core::{
    LspAnswer, LspLocation, LspOperation, LspPosition, LspRange, LspRegistry, StaticLsp,
    StaticSkills, ToolResult, ToolRunContext, lsp_tool, skill_tool,
};
use girder_core::{SkillDefinition, SkillRegistry, SkillSummary};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated directory, so a test that writes a file cannot see another test's.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-skill-lsp-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// The concatenated text of a result, which is all these tools ever return.
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

fn summary(name: &str, provider: &str, layer: u32) -> SkillSummary {
    SkillSummary {
        name: name.to_owned(),
        description: format!("{name} from {provider}"),
        provider: provider.to_owned(),
        layer,
    }
}

fn skill(name: &str, provider: &str, layer: u32) -> SkillDefinition {
    SkillDefinition::new(summary(name, provider, layer), format!("body of {name}")).expect("valid")
}

fn skills_registry(provider: StaticSkills) -> Arc<SkillRegistry> {
    let registry = SkillRegistry::new();
    registry.register(Arc::new(provider)).expect("registered");
    Arc::new(registry)
}

async fn run_skill(registry: &Arc<SkillRegistry>, args: serde_json::Value) -> ToolResult {
    let tool = skill_tool(Arc::clone(registry));
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a lookup is a result")
}

/// A registry with one Rust server that answers a single position.
fn rust_registry(line: u32, character: u32, answer: LspAnswer) -> Arc<LspRegistry> {
    let mut server = StaticLsp::new("rust", vec!["rs".to_owned()]);
    server.answer(line, character, answer);
    let registry = LspRegistry::new();
    registry.register(Arc::new(server)).expect("registered");
    Arc::new(registry)
}

/// Hovers `character` on the first line of a file containing `source`, and returns what the tool said.
async fn hover_at(dir: &Path, source: &str, character: u32) -> ToolResult {
    let file = dir.join("a.rs");
    std::fs::write(&file, format!("{source}\n")).expect("written");
    let registry = rust_registry(0, 0, LspAnswer::Nothing);
    let tool = lsp_tool(registry);
    (tool.execute)(
        serde_json::json!({
            "operation": "hover",
            "path": file.to_str().expect("utf-8"),
            "line": 1,
            "character": character,
            "workspace_root": dir.to_str().expect("utf-8")
        }),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result")
}

// ---- the skill tool: listing ----------------------------------------------------------------

#[tokio::test]
async fn no_name_lists_every_skill_with_its_description() {
    // The description is the whole basis for choosing a skill, so a listing without one is a list of names
    // the model cannot act on.
    let mut provider = StaticSkills::new("builtin", 0);
    provider.add(skill("review", "builtin", 0));
    provider.add(skill("deploy", "builtin", 0));
    let registry = skills_registry(provider);

    let result = run_skill(&registry, serde_json::json!({})).await;
    let out = text_of(&result);

    assert!(!result.is_error, "{out}");
    assert!(out.contains("- review: review from builtin"), "{out}");
    assert!(out.contains("- deploy: deploy from builtin"), "{out}");
}

#[tokio::test]
async fn a_catalog_reports_the_provider_that_answered() {
    // A user who expected their project's `review` and got the built-in one needs to see why.
    let mut builtin = StaticSkills::new("builtin", 0);
    builtin.add(skill("review", "builtin", 0));
    let mut project = StaticSkills::new("project", 10);
    project.add(skill("review", "project", 10));

    let registry = SkillRegistry::new();
    registry.register(Arc::new(builtin)).expect("registered");
    registry.register(Arc::new(project)).expect("registered");
    let registry = Arc::new(registry);

    let out = text_of(&run_skill(&registry, serde_json::json!({})).await);

    // Counted as entries rather than as occurrences of the word: the description repeats the name.
    assert_eq!(
        out.matches("- review:").count(),
        1,
        "one entry per name: {out}"
    );
    assert!(out.contains("from `project`"), "the winner is named: {out}");
    assert!(
        !out.contains("from `builtin`"),
        "the shadowed one is gone: {out}"
    );
}

#[tokio::test]
async fn an_empty_catalog_is_an_answer_and_not_a_failure() {
    // A deployment with no skills has not failed; a model told it failed retries the same listing.
    let registry = skills_registry(StaticSkills::new("builtin", 0));

    let result = run_skill(&registry, serde_json::json!({})).await;

    assert!(!result.is_error);
    assert!(
        text_of(&result).contains("no skills"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn a_blank_name_lists_rather_than_looking_up_the_empty_string() {
    // A model that emitted `""` was asking to see the list; refusing it spends a turn on punctuation.
    let mut provider = StaticSkills::new("builtin", 0);
    provider.add(skill("review", "builtin", 0));
    let registry = skills_registry(provider);

    let out = text_of(&run_skill(&registry, serde_json::json!({"name": "  "})).await);

    assert!(out.contains("- review:"), "{out}");
}

// ---- the skill tool: loading ----------------------------------------------------------------

#[tokio::test]
async fn a_name_loads_the_body() {
    let mut provider = StaticSkills::new("builtin", 0);
    provider.add(skill("review", "builtin", 0));
    let registry = skills_registry(provider);

    let result = run_skill(&registry, serde_json::json!({"name": "review"})).await;
    let out = text_of(&result);

    assert!(!result.is_error, "{out}");
    assert!(out.contains("body of review"), "{out}");
}

#[tokio::test]
async fn a_loaded_skill_names_its_files_without_inlining_them() {
    // The body is what the model follows; files are reference material that may be large enough that
    // inlining them would let one load dominate the window.
    let mut provider = StaticSkills::new("builtin", 0);
    provider.add(
        skill("review", "builtin", 0)
            .with_file("template.md", "# heading\nbody\n")
            .with_file("checklist.txt", "one\n"),
    );
    let registry = skills_registry(provider);

    let out = text_of(&run_skill(&registry, serde_json::json!({"name": "review"})).await);

    assert!(out.contains("template.md"), "{out}");
    assert!(out.contains("checklist.txt"), "{out}");
    assert!(
        !out.contains("# heading"),
        "the file's contents must not be inlined: {out}"
    );
}

#[tokio::test]
async fn a_skill_with_no_files_says_so() {
    let mut provider = StaticSkills::new("builtin", 0);
    provider.add(skill("review", "builtin", 0));
    let registry = skills_registry(provider);

    let out = text_of(&run_skill(&registry, serde_json::json!({"name": "review"})).await);

    assert!(out.contains("no files"), "{out}");
}

#[tokio::test]
async fn an_unknown_name_lists_what_does_exist() {
    // The real names are the one correction a model that guessed wrong can act on.
    let mut provider = StaticSkills::new("builtin", 0);
    provider.add(skill("review", "builtin", 0));
    provider.add(skill("deploy", "builtin", 0));
    let registry = skills_registry(provider);

    let result = run_skill(&registry, serde_json::json!({"name": "reviw"})).await;
    let out = text_of(&result);

    assert!(result.is_error, "{out}");
    assert!(out.contains("reviw"), "the mistake is named: {out}");
    assert!(out.contains("review"), "the alternatives are named: {out}");
    assert!(out.contains("deploy"), "{out}");
}

#[tokio::test]
async fn an_unknown_name_in_an_empty_registry_says_no_skills_exist() {
    let registry = skills_registry(StaticSkills::new("builtin", 0));

    let result = run_skill(&registry, serde_json::json!({"name": "review"})).await;

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("no skills"),
        "{}",
        text_of(&result)
    );
}

#[test]
fn the_skill_schema_declares_name_as_optional() {
    // A required `name` would make the schema lie about the listing call, and a model that trusted it would
    // never discover a skill.
    let registry = skills_registry(StaticSkills::new("builtin", 0));

    let tool = skill_tool(registry);
    let required = tool.parameters["required"].as_array();
    assert!(
        required.is_none_or(|fields| !fields.iter().any(|field| field == "name")),
        "{:?}",
        tool.parameters
    );
    assert!(tool.parameters["properties"]["name"].is_object());
}

// ---- UTF-16 conversion, proven through the tool ---------------------------------------------

/// Asserts that a one-based character position reaches the server as the given zero-based UTF-16 column.
///
/// The server answers only at that column, so a conversion that produced any other number yields `Nothing`.
async fn assert_column(name: &str, source: &str, character: u32, expected: u32) {
    let dir = scratch(name);
    let file = dir.join("a.rs");
    std::fs::write(&file, format!("{source}\n")).expect("written");
    let registry = rust_registry(0, expected, LspAnswer::Text("reached".to_owned()));
    let tool = lsp_tool(registry);

    let result = (tool.execute)(
        serde_json::json!({
            "operation": "hover",
            "path": file.to_str().expect("utf-8"),
            "line": 1,
            "character": character,
            "workspace_root": dir.to_str().expect("utf-8")
        }),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result");

    assert_eq!(
        text_of(&result),
        "reached",
        "{source:?} character {character} should be UTF-16 column {expected}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_ascii_character_reaches_its_index() {
    // The only case where subtracting one would have been right, which is why the mistake survives an
    // all-ASCII test suite.
    assert_column("utf16-ascii", "hello", 1, 0).await;
    assert_column("utf16-ascii", "hello", 2, 1).await;
    assert_column("utf16-ascii", "hello", 5, 4).await;
}

#[tokio::test]
async fn the_second_character_of_a_chinese_line_reaches_column_one() {
    // `中` is three UTF-8 bytes but one UTF-16 unit, so a byte offset would answer 3 and a one-based
    // pass-through would answer 2. The protocol wants 1.
    assert_column("utf16-chinese", "中文", 1, 0).await;
    assert_column("utf16-chinese", "中文", 2, 1).await;
    assert_column("utf16-chinese", "中文", 3, 2).await;
}

#[tokio::test]
async fn an_emoji_before_the_cursor_shifts_the_column_by_two() {
    // The case that separates UTF-16 from characters: `😀` is one character and two code units, so the `x`
    // after it is column 2 while a converter that assumed one unit per character would say 1.
    assert_column("utf16-emoji", "😀x", 1, 0).await;
    assert_column("utf16-emoji", "😀x", 2, 2).await;
    assert_column("utf16-emoji", "😀x", 3, 3).await;
}

#[tokio::test]
async fn a_mixed_line_accumulates_each_character_width() {
    // "a中b😀c": a=1, 中=1, b=1, 😀=2 units, so `c` is column 5. A converter that counted bytes lands on 8
    // and one that counted characters lands on 4; neither is the protocol's answer.
    let line = "a中b😀c";
    assert_column("utf16-mixed", line, 1, 0).await;
    assert_column("utf16-mixed", line, 2, 1).await;
    assert_column("utf16-mixed", line, 3, 2).await;
    assert_column("utf16-mixed", line, 4, 3).await;
    assert_column("utf16-mixed", line, 5, 5).await;
    assert_column("utf16-mixed", line, 6, 6).await;
}

#[tokio::test]
async fn two_emoji_in_a_row_shift_by_two_each_time() {
    assert_column("utf16-two-emoji", "😀😀x", 2, 2).await;
    assert_column("utf16-two-emoji", "😀😀x", 3, 4).await;
}

#[tokio::test]
async fn one_past_the_last_character_is_the_end_of_the_line() {
    // A cursor at the end is a real position, and it is where a query for a name that closes a line lands.
    assert_column("utf16-end", "abc", 4, 3).await;
    assert_column("utf16-end", "😀", 2, 2).await;
    assert_column("utf16-end", "a中b😀c", 6, 6).await;
}

#[tokio::test]
async fn an_empty_line_addresses_only_its_first_position() {
    assert_column("utf16-empty", "", 1, 0).await;
}

#[tokio::test]
async fn a_character_past_the_end_is_refused() {
    // Reporting a column for a place that does not exist would send the server a query about nothing.
    let dir = scratch("utf16-past-end");

    let past_end = hover_at(&dir, "abc", 5).await;
    assert!(past_end.is_error, "{}", text_of(&past_end));
    assert!(text_of(&past_end).contains('5'), "{}", text_of(&past_end));

    let past_empty = hover_at(&dir, "", 2).await;
    assert!(past_empty.is_error, "{}", text_of(&past_empty));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_zero_position_is_refused_because_the_input_is_one_based() {
    // Passing zero through would mean "before the first character", which is not a position the protocol has.
    let dir = scratch("utf16-zero");
    let registry = rust_registry(0, 0, LspAnswer::Text("reached".to_owned()));
    let file = dir.join("a.rs");
    std::fs::write(&file, "abc\n").expect("written");

    let result = (lsp_tool(registry).execute)(
        serde_json::json!({
            "operation": "hover",
            "path": file.to_str().expect("utf-8"),
            "line": 1,
            "character": 0
        }),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result");

    assert!(result.is_error, "{}", text_of(&result));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the lsp tool ---------------------------------------------------------------------------

#[tokio::test]
async fn a_definition_query_reaches_the_server_with_a_converted_position() {
    // The server answers only at the converted position, so a tool that passed the one-based numbers
    // through unchanged would get `Nothing` and this assertion would fail.
    let dir = scratch("lsp-definition");
    let file = dir.join("a.rs");
    std::fs::write(&file, "fn main() {}\n").expect("written");
    let registry = rust_registry(
        0,
        3,
        LspAnswer::Locations(vec![LspLocation {
            path: file.clone(),
            range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 7)),
        }]),
    );

    let out = text_of(
        &(lsp_tool(registry).execute)(
            serde_json::json!({
                "operation": "definition",
                "path": file.to_str().expect("utf-8"),
                "line": 1,
                "character": 4,
                "workspace_root": dir.to_str().expect("utf-8")
            }),
            ToolRunContext::default(),
        )
        .await
        .expect("a lookup is a result"),
    );

    assert!(out.contains("a.rs:1:4"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_reference_answer_reports_every_location_one_based() {
    let dir = scratch("lsp-references");
    let file = dir.join("a.rs");
    std::fs::write(&file, "fn a() {}\nfn b() {}\n").expect("written");
    let registry = rust_registry(
        0,
        0,
        LspAnswer::Locations(vec![
            LspLocation {
                path: file.clone(),
                range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 2)),
            },
            LspLocation {
                path: file.clone(),
                range: LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 2)),
            },
        ]),
    );

    let out = text_of(
        &(lsp_tool(registry).execute)(
            serde_json::json!({
                "operation": "references",
                "path": file.to_str().expect("utf-8"),
                "line": 1,
                "character": 1,
                "workspace_root": dir.to_str().expect("utf-8")
            }),
            ToolRunContext::default(),
        )
        .await
        .expect("a lookup is a result"),
    );

    assert!(out.contains("2 location(s)"), "{out}");
    assert!(out.contains("a.rs:1:1"), "{out}");
    // The protocol's line is zero-based and the tool's is not; an off-by-one here sends the model to the
    // wrong line.
    assert!(out.contains("a.rs:2:1"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_answered_location_on_a_chinese_line_is_reported_as_a_character() {
    // The answer comes back in the unit the tool accepts, or the model would be sent to the wrong place on
    // exactly the lines this module exists to get right.
    let dir = scratch("lsp-location-chinese");
    let file = dir.join("a.rs");
    std::fs::write(&file, "中文 a\n").expect("written");
    // UTF-16 column 3 is the `a`, which is character 4.
    let registry = rust_registry(
        0,
        0,
        LspAnswer::Locations(vec![LspLocation {
            path: file.clone(),
            range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 4)),
        }]),
    );

    let out = text_of(
        &(lsp_tool(registry).execute)(
            serde_json::json!({
                "operation": "definition",
                "path": file.to_str().expect("utf-8"),
                "line": 1,
                "character": 1,
                "workspace_root": dir.to_str().expect("utf-8")
            }),
            ToolRunContext::default(),
        )
        .await
        .expect("a lookup is a result"),
    );

    assert!(
        out.contains("a.rs:1:4"),
        "column 3 is the fourth character: {out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_location_inside_a_surrogate_pair_reports_only_the_line() {
    // The column names no character, so a character position would be a guess; the line alone is honest.
    let dir = scratch("lsp-location-surrogate");
    let file = dir.join("a.rs");
    // "a😀x": `a` occupies unit 0, `😀` occupies units 1 and 2, so only columns 0, 1, 3 and 4 are character
    // boundaries. Column 2 is the low half of the surrogate pair, which is inside a character.
    std::fs::write(&file, "a😀x\n").expect("written");
    let registry = rust_registry(
        0,
        0,
        LspAnswer::Locations(vec![LspLocation {
            path: file.clone(),
            range: LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 3)),
        }]),
    );

    let out = text_of(
        &(lsp_tool(registry).execute)(
            serde_json::json!({
                "operation": "definition",
                "path": file.to_str().expect("utf-8"),
                "line": 1,
                "character": 1,
                "workspace_root": dir.to_str().expect("utf-8")
            }),
            ToolRunContext::default(),
        )
        .await
        .expect("a lookup is a result"),
    );

    assert!(out.contains("a.rs:1"), "{out}");
    assert!(!out.contains("a.rs:1:"), "no column is claimed: {out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nothing_is_an_answer_rather_than_a_failure() {
    // "No definition exists" is a fact about the code. Reporting it as a failure makes the model retry a
    // query that already succeeded.
    let dir = scratch("lsp-nothing");
    let file = dir.join("a.rs");
    std::fs::write(&file, "fn a() {}\n").expect("written");
    let registry = rust_registry(0, 0, LspAnswer::Nothing);

    let result = (lsp_tool(registry).execute)(
        serde_json::json!({
            "operation": "definition",
            "path": file.to_str().expect("utf-8"),
            "line": 1,
            "character": 1,
            "workspace_root": dir.to_str().expect("utf-8")
        }),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result");

    assert!(!result.is_error, "{}", text_of(&result));
    assert!(text_of(&result).contains("nothing"), "{}", text_of(&result));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_location_list_is_distinct_from_nothing() {
    let dir = scratch("lsp-empty-list");
    let file = dir.join("a.rs");
    std::fs::write(&file, "fn a() {}\n").expect("written");
    let registry = rust_registry(0, 0, LspAnswer::Locations(Vec::new()));

    let out = text_of(
        &(lsp_tool(registry).execute)(
            serde_json::json!({
                "operation": "implementation",
                "path": file.to_str().expect("utf-8"),
                "line": 1,
                "character": 1,
                "workspace_root": dir.to_str().expect("utf-8")
            }),
            ToolRunContext::default(),
        )
        .await
        .expect("a lookup is a result"),
    );

    assert!(out.contains("no locations"), "{out}");
    assert!(!out.contains("could not determine"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unserved_extension_names_the_ones_that_are_served() {
    // "No server" alone is indistinguishable from "no definition"; the served list is the correction.
    let dir = scratch("lsp-unserved");
    let file = dir.join("a.txt");
    std::fs::write(&file, "hello\n").expect("written");
    let registry = rust_registry(0, 0, LspAnswer::Nothing);

    let result = (lsp_tool(registry).execute)(
        serde_json::json!({
            "operation": "definition",
            "path": file.to_str().expect("utf-8"),
            "line": 1,
            "character": 1
        }),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result");

    assert!(result.is_error);
    assert!(text_of(&result).contains(".rs"), "{}", text_of(&result));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_line_past_the_end_is_an_error_that_names_the_line() {
    let dir = scratch("lsp-bad-line");
    let file = dir.join("a.rs");
    std::fs::write(&file, "fn a() {}\n").expect("written");
    let registry = rust_registry(0, 0, LspAnswer::Nothing);

    let result = (lsp_tool(registry).execute)(
        serde_json::json!({
            "operation": "definition",
            "path": file.to_str().expect("utf-8"),
            "line": 99,
            "character": 1
        }),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result");

    assert!(result.is_error);
    assert!(text_of(&result).contains("99"), "{}", text_of(&result));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_operation_is_refused_by_name() {
    let dir = scratch("lsp-bad-operation");
    let file = dir.join("a.rs");
    std::fs::write(&file, "fn a() {}\n").expect("written");
    let registry = rust_registry(0, 0, LspAnswer::Nothing);

    let result = (lsp_tool(registry).execute)(
        serde_json::json!({
            "operation": "symbols",
            "path": file.to_str().expect("utf-8"),
            "line": 1,
            "character": 1
        }),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result");

    assert!(result.is_error);
    assert!(text_of(&result).contains("symbols"), "{}", text_of(&result));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_path_is_refused() {
    let registry = rust_registry(0, 0, LspAnswer::Nothing);

    let result = (lsp_tool(registry).execute)(
        serde_json::json!({"operation": "definition", "line": 1, "character": 1}),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result");

    assert!(result.is_error);
    assert!(text_of(&result).contains("path"), "{}", text_of(&result));
}

#[tokio::test]
async fn an_unreadable_file_is_refused_with_its_path() {
    // The conversion needs the line, so a file that cannot be read cannot be queried at all.
    let dir = scratch("lsp-missing-file");
    let registry = rust_registry(0, 0, LspAnswer::Nothing);

    let result = (lsp_tool(registry).execute)(
        serde_json::json!({
            "operation": "definition",
            "path": dir.join("absent.rs").to_str().expect("utf-8"),
            "line": 1,
            "character": 1
        }),
        ToolRunContext::default(),
    )
    .await
    .expect("a lookup is a result");

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("absent.rs"),
        "{}",
        text_of(&result)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_relative_path_resolves_against_the_named_root() {
    let dir = scratch("lsp-relative");
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").expect("written");
    let registry = rust_registry(0, 0, LspAnswer::Text("hovered".to_owned()));

    let out = text_of(
        &(lsp_tool(registry).execute)(
            serde_json::json!({
                "operation": "hover",
                "path": "a.rs",
                "line": 1,
                "character": 1,
                "workspace_root": dir.to_str().expect("utf-8")
            }),
            ToolRunContext::default(),
        )
        .await
        .expect("a lookup is a result"),
    );

    assert_eq!(out, "hovered");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_session_workspace_is_the_default_root() {
    // Without either root the file resolves against the process's directory, which is not where it lives.
    let dir = scratch("lsp-context-root");
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").expect("written");
    let registry = rust_registry(0, 0, LspAnswer::Text("hovered".to_owned()));

    let result = (lsp_tool(registry).execute)(
        serde_json::json!({
            "operation": "hover",
            "path": "a.rs",
            "line": 1,
            "character": 1
        }),
        ToolRunContext::default().in_workspace(dir.as_path()),
    )
    .await
    .expect("a lookup is a result");

    assert_eq!(text_of(&result), "hovered");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn each_operation_name_reaches_its_own_protocol_method() {
    // The four names are the tool's whole vocabulary; a mapping slip sends `implementation` to
    // `textDocument/definition`, which answers plausibly and wrongly.
    for (name, operation) in [
        ("definition", LspOperation::Definition),
        ("references", LspOperation::References),
        ("implementation", LspOperation::Implementation),
        ("hover", LspOperation::Hover),
    ] {
        let dir = scratch("lsp-operations");
        let file = dir.join("a.rs");
        std::fs::write(&file, "fn a() {}\n").expect("written");
        let registry = rust_registry(0, 0, LspAnswer::Text(operation.method().to_owned()));

        let out = text_of(
            &(lsp_tool(registry).execute)(
                serde_json::json!({
                    "operation": name,
                    "path": file.to_str().expect("utf-8"),
                    "line": 1,
                    "character": 1,
                    "workspace_root": dir.to_str().expect("utf-8")
                }),
                ToolRunContext::default(),
            )
            .await
            .expect("a lookup is a result"),
        );

        assert_eq!(out, operation.method(), "`{name}` sent the wrong method");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn a_crlf_line_does_not_shift_the_column() {
    // A CRLF file's line ends in a character a reader does not see; counting it would shift a position at
    // the end of the line.
    let dir = scratch("lsp-crlf");
    let file = dir.join("a.rs");
    std::fs::write(&file, "ab\r\ncd\r\n").expect("written");
    let registry = rust_registry(0, 2, LspAnswer::Text("end of line".to_owned()));

    let out = text_of(
        &(lsp_tool(registry).execute)(
            serde_json::json!({
                "operation": "hover",
                "path": file.to_str().expect("utf-8"),
                "line": 1,
                "character": 3,
                "workspace_root": dir.to_str().expect("utf-8")
            }),
            ToolRunContext::default(),
        )
        .await
        .expect("a lookup is a result"),
    );

    assert_eq!(out, "end of line", "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_last_line_of_a_file_that_ends_in_a_newline_is_addressable() {
    // A reader sees the place after the final newline as a line; a line number that named it must not be
    // rejected as past the end.
    let dir = scratch("lsp-final-line");
    let file = dir.join("a.rs");
    std::fs::write(&file, "fn a() {}\n").expect("written");
    let registry = rust_registry(1, 0, LspAnswer::Text("blank last line".to_owned()));

    let out = text_of(
        &(lsp_tool(registry).execute)(
            serde_json::json!({
                "operation": "hover",
                "path": file.to_str().expect("utf-8"),
                "line": 2,
                "character": 1,
                "workspace_root": dir.to_str().expect("utf-8")
            }),
            ToolRunContext::default(),
        )
        .await
        .expect("a lookup is a result"),
    );

    assert_eq!(out, "blank last line", "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
