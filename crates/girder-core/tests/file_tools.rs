//! Tests for the file tools.
//!
//! Each tool has one failure mode worth pinning: a read that silently truncates, a write that clobbers, an edit that
//! changes too much, and a patch that applies by halves.

use std::sync::Arc;

use girder_core::{
    ObservationSession, PathPolicy, ToolRunContext, bash_tool, edit_tool, patch_tool, read_tool,
    write_tool,
};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-ft-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// A context with an observation session, so a write's conflict check has something to consult.
fn context(observations: Option<Arc<ObservationSession>>) -> ToolRunContext {
    ToolRunContext {
        observations,
        ..Default::default()
    }
}

fn text_of(result: &girder_core::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---- read -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_read_numbers_its_lines_from_one() {
    // A model that quotes a line to edit needs a way to say which line.
    let dir = scratch("read");
    std::fs::write(dir.join("a.txt"), "first\nsecond\n").expect("written");
    let tool = read_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({"path": "a.txt"}), context(None))
        .await
        .expect("read");

    assert!(!result.is_error);
    let text = text_of(&result);
    assert!(text.contains("1\tfirst"), "{text}");
    assert!(text.contains("2\tsecond"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_read_range_returns_only_those_lines() {
    let dir = scratch("read-range");
    std::fs::write(dir.join("a.txt"), "a\nb\nc\nd\n").expect("written");
    let tool = read_tool(dir.clone());

    let result = (tool.execute)(
        serde_json::json!({"path": "a.txt", "offset": 2, "limit": 2}),
        context(None),
    )
    .await
    .expect("read");

    let text = text_of(&result);
    assert!(text.contains("2\tb"), "{text}");
    assert!(text.contains("3\tc"), "{text}");
    assert!(!text.contains("\td"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_read_of_a_missing_file_is_a_result_not_an_error() {
    let dir = scratch("read-missing");
    let tool = read_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({"path": "nope.txt"}), context(None))
        .await
        .expect("a missing file is a result");

    assert!(result.is_error);
    assert!(text_of(&result).contains("cannot read"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_read_records_an_observation() {
    // What the model saw is what a later write must not contradict.
    let dir = scratch("read-observe");
    std::fs::write(dir.join("a.txt"), "content\n").expect("written");
    let observations = Arc::new(ObservationSession::new());
    let tool = read_tool(dir.clone());

    (tool.execute)(
        serde_json::json!({"path": "a.txt"}),
        context(Some(Arc::clone(&observations))),
    )
    .await
    .expect("read");

    assert!(observations.observed(&dir.join("a.txt")).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- write ------------------------------------------------------------------------------------

#[tokio::test]
async fn a_write_creates_the_file() {
    let dir = scratch("write");
    let tool = write_tool(dir.clone());

    let result = (tool.execute)(
        serde_json::json!({"path": "new.txt", "content": "hello\n"}),
        context(None),
    )
    .await
    .expect("wrote");

    assert!(!result.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).expect("read"),
        "hello\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_creates_missing_directories() {
    let dir = scratch("write-dirs");
    let tool = write_tool(dir.clone());

    (tool.execute)(
        serde_json::json!({"path": "a/b/c.txt", "content": "x"}),
        context(None),
    )
    .await
    .expect("wrote");

    assert!(dir.join("a/b/c.txt").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_is_recorded_as_the_new_expectation() {
    // A session that wrote and then wrote again must not fail its own second write.
    let dir = scratch("write-observe");
    let observations = Arc::new(ObservationSession::new());
    let tool = write_tool(dir.clone());
    let ctx = context(Some(Arc::clone(&observations)));

    (tool.execute)(
        serde_json::json!({"path": "a.txt", "content": "one"}),
        ctx.clone(),
    )
    .await
    .expect("first write");
    let second = (tool.execute)(serde_json::json!({"path": "a.txt", "content": "two"}), ctx)
        .await
        .expect("second write");

    assert!(!second.is_error, "{}", text_of(&second));
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "two"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_refuses_to_clobber_a_change_the_session_did_not_see() {
    // The failure this exists to prevent, and discovering it after the write is too late.
    let dir = scratch("write-conflict");
    std::fs::write(dir.join("a.txt"), "original").expect("written");
    let observations = Arc::new(ObservationSession::new());
    let tool = read_tool(dir.clone());
    let ctx = context(Some(Arc::clone(&observations)));

    (tool.execute)(serde_json::json!({"path": "a.txt"}), ctx.clone())
        .await
        .expect("read");

    // Somebody else changes the file.
    std::fs::write(dir.join("a.txt"), "changed by someone else").expect("written");

    let write = write_tool(dir.clone());
    let result = (write.execute)(serde_json::json!({"path": "a.txt", "content": "mine"}), ctx)
        .await
        .expect("refused");

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("refusing to write"),
        "{}",
        text_of(&result)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "changed by someone else",
        "the other change survives"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- edit -------------------------------------------------------------------------------------

#[tokio::test]
async fn an_edit_replaces_one_occurrence() {
    let dir = scratch("edit");
    std::fs::write(dir.join("a.txt"), "one two three\n").expect("written");
    let tool = edit_tool(dir.clone());

    let result = (tool.execute)(
        serde_json::json!({"path": "a.txt", "old": "two", "new": "TWO"}),
        context(None),
    )
    .await
    .expect("edited");

    assert!(!result.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "one TWO three\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_ambiguous_edit_is_refused() {
    // Replacing every occurrence would change code the caller never looked at.
    let dir = scratch("edit-ambiguous");
    std::fs::write(dir.join("a.txt"), "a b a\n").expect("written");
    let tool = edit_tool(dir.clone());

    let result = (tool.execute)(
        serde_json::json!({"path": "a.txt", "old": "a", "new": "X"}),
        context(None),
    )
    .await
    .expect("refused");

    assert!(result.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "a b a\n",
        "nothing changed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- patch ------------------------------------------------------------------------------------

#[tokio::test]
async fn a_patch_updates_a_file() {
    let dir = scratch("patch");
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").expect("written");
    let tool = patch_tool(dir.clone());
    let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+TWO\n*** End Patch\n";

    let result = (tool.execute)(serde_json::json!({"patch": patch}), context(None))
        .await
        .expect("applied");

    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "one\nTWO\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_patch_adds_a_file() {
    let dir = scratch("patch-add");
    let tool = patch_tool(dir.clone());
    let patch = "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** End Patch\n";

    (tool.execute)(serde_json::json!({"patch": patch}), context(None))
        .await
        .expect("applied");

    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).expect("read"),
        "hello\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_patch_deletes_a_file() {
    let dir = scratch("patch-delete");
    std::fs::write(dir.join("gone.txt"), "x").expect("written");
    let tool = patch_tool(dir.clone());
    let patch = "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n";

    (tool.execute)(serde_json::json!({"patch": patch}), context(None))
        .await
        .expect("applied");

    assert!(!dir.join("gone.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_patch_that_does_not_apply_leaves_every_file_untouched() {
    // The caller asked for a change, not for a prefix of one.
    let dir = scratch("patch-fail");
    std::fs::write(dir.join("a.txt"), "one\n").expect("written");
    std::fs::write(dir.join("b.txt"), "different\n").expect("written");
    let tool = patch_tool(dir.clone());
    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: a.txt\n@@\n-one\n+ONE\n",
        "*** Update File: b.txt\n@@\n-absent\n+x\n",
        "*** End Patch\n"
    );

    let result = (tool.execute)(serde_json::json!({"patch": patch}), context(None))
        .await
        .expect("refused");

    assert!(result.is_error);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "one\n",
        "the first file was not changed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unparseable_patch_is_reported() {
    let dir = scratch("patch-bad");
    let tool = patch_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({"patch": "not a patch"}), context(None))
        .await
        .expect("refused");

    assert!(result.is_error);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- bash closes the observation bypass ---------------------------------------------------------

/// A context that carries an observation session and can drive a shell command.
fn bash_context(
    observations: Arc<ObservationSession>,
    workspace: &std::path::Path,
) -> ToolRunContext {
    // The workspace is part of the context because the shell tool runs the command in it. Without it the command
    // executes in whatever directory the test binary was started from -- this repository -- and a test that writes
    // `a.txt` overwrites a tracked file. That happened once and the file had to be restored from git.
    ToolRunContext {
        observations: Some(observations),
        workspace: Some(workspace.to_path_buf()),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_write_after_a_shell_command_is_not_checked_against_a_stale_read() {
    // The bypass this closes: `read`, then `bash "echo > f"`, then `write`. Before the change the write compared
    // against the version recorded by the read, which the shell command had already replaced — so it either
    // refused a write that was safe or, worse, its own comparison was meaningless. After the command the session
    // holds no expectation, and the tool says so.
    let dir = scratch("bash-bypass");
    std::fs::write(dir.join("a.txt"), "original").expect("written");
    let observations = Arc::new(ObservationSession::new());

    let read = read_tool(dir.clone());
    (read.execute)(
        serde_json::json!({"path": "a.txt"}),
        context(Some(Arc::clone(&observations))),
    )
    .await
    .expect("read");
    assert!(observations.observed(&dir.join("a.txt")).is_some());

    let bash = bash_tool(PathPolicy::new(&dir));
    let result = (bash.execute)(
        serde_json::json!({"command": "echo rewritten-by-shell > a.txt"}),
        bash_context(Arc::clone(&observations), &dir),
    )
    .await
    .expect("ran");

    assert!(!result.is_error, "{:?}", result.content);
    assert!(
        observations.observed(&dir.join("a.txt")).is_none(),
        "the read predates the command, so it cannot be the expectation a later write is checked against"
    );

    let write = write_tool(dir.clone());
    let result = (write.execute)(
        serde_json::json!({"path": "a.txt", "content": "mine"}),
        context(Some(Arc::clone(&observations))),
    )
    .await
    .expect("wrote");

    assert!(
        !result.is_error,
        "the stale observation must not refuse this write: {}",
        text_of(&result)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).expect("read"),
        "mine"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_after_a_shell_command_warns_about_the_invalidated_read() {
    // The model has to learn that its reading is no longer current, or it will edit a file whose contents it no
    // longer knows.
    let dir = scratch("bash-warns");
    std::fs::write(dir.join("a.txt"), "original").expect("written");
    let observations = Arc::new(ObservationSession::new());
    let read = read_tool(dir.clone());
    (read.execute)(
        serde_json::json!({"path": "a.txt"}),
        context(Some(Arc::clone(&observations))),
    )
    .await
    .expect("read");

    let bash = bash_tool(PathPolicy::new(&dir));
    let result = (bash.execute)(
        serde_json::json!({"command": "echo touched > a.txt"}),
        bash_context(Arc::clone(&observations), &dir),
    )
    .await
    .expect("ran");

    assert!(
        text_of(&result).contains("[observation]"),
        "{:?}",
        result.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_read_after_a_shell_command_is_observed_again() {
    // Invalidation is not a permanent opt-out: the model can re-read, and the new read is the expectation again.
    let dir = scratch("bash-reread");
    std::fs::write(dir.join("a.txt"), "one").expect("written");
    let observations = Arc::new(ObservationSession::new());

    let bash = bash_tool(PathPolicy::new(&dir));
    (bash.execute)(
        serde_json::json!({"command": "echo two > a.txt"}),
        bash_context(Arc::clone(&observations), &dir),
    )
    .await
    .expect("ran");

    let read = read_tool(dir.clone());
    (read.execute)(
        serde_json::json!({"path": "a.txt"}),
        context(Some(Arc::clone(&observations))),
    )
    .await
    .expect("re-read");

    assert!(observations.observed(&dir.join("a.txt")).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the language hint ------------------------------------------------------------------------

#[tokio::test]
async fn a_read_names_the_language_so_a_renderer_can_highlight() {
    // The alternative is every consumer re-deriving it with its own table, and one of them disagreeing about what a
    // `.m` file is.
    let dir = scratch("language");
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").expect("written");
    let tool = read_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({"path": "main.rs"}), context(None))
        .await
        .expect("read");

    let text = text_of(&result);
    assert!(
        text.starts_with("rust\n"),
        "the hint leads the result: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_read_of_an_unknown_extension_has_no_hint() {
    // A guess would highlight a language the file is not.
    let dir = scratch("no-language");
    std::fs::write(dir.join("notes.xyz"), "plain\n").expect("written");
    let tool = read_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({"path": "notes.xyz"}), context(None))
        .await
        .expect("read");

    let text = text_of(&result);
    assert!(text.starts_with("1\t"), "no hint line: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- documents --------------------------------------------------------------------------------

#[tokio::test]
async fn a_docx_is_refused_with_an_explanation_instead_of_noise() {
    // A .docx is a zip. Decoding its bytes as text gives a model a screenful of noise it will try to reason about, which
    // is worse than being told the file needs a different tool.
    let dir = scratch("docx");
    // A zip local-file header: `PK\x03\x04`, which is what detect reads.
    let mut bytes = vec![0x50, 0x4b, 0x03, 0x04];
    bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(dir.join("report.docx"), &bytes).expect("written");
    let tool = read_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({"path": "report.docx"}), context(None))
        .await
        .expect("read");

    assert!(
        result.is_error,
        "a document is refused: {:?}",
        text_of(&result)
    );
    assert!(
        text_of(&result).contains("docx") || text_of(&result).to_lowercase().contains("document"),
        "the refusal names the format: {}",
        text_of(&result)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_plain_text_file_is_still_read() {
    // The check must not refuse everything: a file with no NUL byte and no known magic is text.
    let dir = scratch("still-text");
    std::fs::write(dir.join("notes.txt"), "plain words\n").expect("written");
    let tool = read_tool(dir.clone());

    let result = (tool.execute)(serde_json::json!({"path": "notes.txt"}), context(None))
        .await
        .expect("read");

    assert!(!result.is_error);
    assert!(text_of(&result).contains("plain words"));
    let _ = std::fs::remove_dir_all(&dir);
}
