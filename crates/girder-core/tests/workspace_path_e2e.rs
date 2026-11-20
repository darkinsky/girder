//! End-to-end proof that every tool taking a workspace path refuses to leave the workspace.
//!
//! # Why this is an end-to-end test rather than a unit test
//!
//! `workspace::path::contains` and `sandbox::path::Sandbox` are already unit-tested for the answer
//! they compute, and that answer is correct. The question this file asks is different and narrower:
//! does the tool a model actually calls *consult* that answer? A correct containment helper that no
//! tool body reaches leaves a workspace with no boundary at all, and every unit test of the helper
//! keeps passing while the escape happens. So each case here builds a real directory, a real file
//! outside it, and — where relevant — a real symlink, then drives the public `ToolDefinition` the
//! way the agent loop drives it.
//!
//! # Why "did not read the bytes" is asserted separately from "returned an error"
//!
//! An error is not evidence of confinement. A tool that joins `../outside` onto the workspace and
//! then hits the filesystem produces `No such file or directory` for a target that was never there,
//! and produces the file's contents for one that is. Both are errors-or-not for the wrong reason, so
//! every refusal case here also asserts that the sentinel text living outside the workspace appears
//! nowhere in the result, and that a write left no file behind. That is the property a user cares
//! about; the error message is only the explanation.
//!
//! # Why the outside directory is a sibling
//!
//! A file placed inside the workspace proves nothing. Each fixture therefore creates `workspace/`
//! and `outside/` under one temporary root, so `../outside` is a genuine escape by one level, and
//! the fixture is self-contained enough that its own symlinks point somewhere real.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    ToolDefinition, ToolResult, ToolRunContext, glob_tool, grep_tool, read_tool, write_tool,
};

/// Text that exists only outside the workspace.
///
/// Unique enough that its presence in a result cannot be a coincidence. A test asserts this string
/// is absent, which is how "the tool returned an error" is distinguished from "the tool read the
/// file and then reported an error about something else".
const OUTSIDE_MARKER: &str = "OUTSIDE-SECRET-2f4b9c-do-not-leak";

/// A disposable workspace plus a sibling directory outside it.
struct Fixture {
    /// The temporary root both subdirectories live under, so one `remove_dir_all` cleans up.
    root: PathBuf,
    /// The workspace the tool is constructed with.
    workspace: PathBuf,
    /// A directory next to the workspace, reachable only by leaving it.
    outside: PathBuf,
}

impl Fixture {
    /// Builds a fresh root, workspace, and outside directory.
    ///
    /// The process id and a counter are both in the name: cargo runs the tests of one binary in
    /// parallel threads, and a shared directory would let one test's cleanup delete another's
    /// sentinel, turning a real pass into a flake.
    fn new(name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("girder-wspath-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).expect("created workspace");
        std::fs::create_dir_all(&outside).expect("created outside");
        Self {
            root,
            workspace,
            outside,
        }
    }

    /// Writes a file that a leaking tool would return.
    fn plant_outside_secret(&self, name: &str) -> PathBuf {
        let path = self.outside.join(name);
        std::fs::write(&path, format!("{OUTSIDE_MARKER}\n")).expect("planted outside file");
        path
    }

    /// A context whose `workspace` field agrees with the root the tool was built with.
    ///
    /// Both are set because a tool may read either: the `PathBuf` captured at construction and the
    /// one the turn carries. A disagreement between them is itself a bug, and setting both keeps
    /// this test about the boundary rather than about which of the two a tool happens to trust.
    fn context(&self) -> ToolRunContext {
        ToolRunContext::default().in_workspace(self.workspace.clone())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The text of a result, concatenated.
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

/// Runs a tool the way the agent loop runs it.
async fn run(tool: &ToolDefinition, args: serde_json::Value, ctx: ToolRunContext) -> ToolResult {
    (tool.execute)(args, ctx)
        .await
        .expect("a tool body returned")
}

/// Whether a refusal explains the boundary instead of reporting a missing file.
///
/// A refusal that only says "cannot read" is indistinguishable from a path that was simply wrong,
/// and it tells a model nothing about why asking again differently will not help. The boundary has
/// to be named for the refusal to be actionable.
fn names_the_boundary(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "outside the workspace",
        "outside workspace",
        "escapes the workspace",
        "resolves outside",
        "outside the sandbox",
        "boundary",
        "not inside the workspace",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Asserts a refusal is a real refusal: an error, no leaked bytes, and a reason.
fn assert_refused(result: &ToolResult, what: &str) {
    let text = text_of(result);
    assert!(
        result.is_error,
        "{what} must be refused, but the tool reported success: {text}"
    );
    assert!(
        !text.contains(OUTSIDE_MARKER),
        "{what} returned content from outside the workspace, so the escape happened even though \
         a message was produced: {text}"
    );
    assert!(
        names_the_boundary(&text),
        "{what} was refused without naming the workspace boundary, which is the difference \
         between a confined path and a path that merely happened not to exist: {text}"
    );
}

// ---- 1. read rejects `..` ----------------------------------------------------------------------

#[tokio::test]
async fn read_of_a_parent_relative_path_is_refused_and_names_the_boundary() {
    // `..` is the cheapest escape and the one a model writes by accident. The file it reaches is
    // real, so a tool that joins the path onto the workspace and reads will actually succeed; this
    // is what makes the case worth asserting rather than assuming.
    let fixture = Fixture::new("read-dotdot");
    fixture.plant_outside_secret("passwd");
    let tool = read_tool(fixture.workspace.clone());

    let result = run(
        &tool,
        serde_json::json!({"path": "../outside/passwd"}),
        fixture.context(),
    )
    .await;

    assert_refused(&result, "reading `../outside/passwd`");
}

// ---- 2. write rejects an absolute path outside ------------------------------------------------

#[tokio::test]
async fn write_to_an_absolute_path_outside_the_workspace_is_refused() {
    // An absolute path is the escape `..` normalisation cannot catch, because there is nothing to
    // normalise: the path already names a place. The assertion that the file was not created is
    // stronger than any error message, because a tool can report an error after writing.
    let fixture = Fixture::new("write-absolute");
    let target = fixture.outside.join("written-by-the-tool.txt");
    let tool = write_tool(fixture.workspace.clone());

    let result = run(
        &tool,
        serde_json::json!({
            "path": target.display().to_string(),
            "content": "this must never reach the disk outside the workspace",
        }),
        fixture.context(),
    )
    .await;

    assert_refused(&result, "writing an absolute path outside the workspace");
    assert!(
        !target.exists(),
        "the write escaped: `{}` exists outside the workspace after the tool refused it",
        target.display()
    );
}

// ---- 3. symlink escape is refused (the core case) ---------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn read_through_a_symlink_that_leaves_the_workspace_is_refused() {
    // The core case, and the one no lexical check can see. `workspace/escape/secret.txt` is inside
    // the workspace as a string and outside it as a directory entry, so a containment check written
    // with `starts_with` on the raw spelling passes while the read returns the outside file. Only a
    // check that lets the filesystem resolve the link — which is what `sandbox_path` exists for —
    // catches it.
    let fixture = Fixture::new("read-symlink");
    fixture.plant_outside_secret("secret.txt");
    let link = fixture.workspace.join("escape");
    std::os::unix::fs::symlink(&fixture.outside, &link)
        .expect("created a symlink out of the workspace");

    let tool = read_tool(fixture.workspace.clone());
    let result = run(
        &tool,
        serde_json::json!({"path": "escape/secret.txt"}),
        fixture.context(),
    )
    .await;

    assert_refused(&result, "reading through a symlink out of the workspace");

    // Spelled absolutely, through the same link: an absolute path that starts inside the workspace
    // must be judged by where it lands, not by how it begins.
    let absolute = link.join("secret.txt");
    let result = run(
        &tool,
        serde_json::json!({"path": absolute.display().to_string()}),
        fixture.context(),
    )
    .await;

    assert_refused(
        &result,
        "reading an absolute path that starts inside the workspace and lands outside it",
    );

    // A symlink naming one outside file, rather than a directory, is the same escape with one hop
    // fewer, and a check that handled only directory links would miss it.
    let file_link = fixture.workspace.join("secret-link.txt");
    std::os::unix::fs::symlink(fixture.outside.join("secret.txt"), &file_link)
        .expect("created a file symlink out of the workspace");
    let result = run(
        &tool,
        serde_json::json!({"path": "secret-link.txt"}),
        fixture.context(),
    )
    .await;

    assert_refused(
        &result,
        "reading a file symlink that points out of the workspace",
    );
}

// ---- 4. glob does not return files outside ----------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn glob_does_not_return_files_outside_the_workspace() {
    // A glob walks directories. A symlinked directory inside the workspace is a directory to the
    // walker, so a walk that does not check each entry lists outside files as if they were part of
    // the project. The outside file is given a name no pattern used here would otherwise match, so
    // its appearance in the output can only come from crossing the boundary.
    let fixture = Fixture::new("glob-escape");
    fixture.plant_outside_secret("leaked_glob_target.rs");
    let link = fixture.workspace.join("escape");
    std::os::unix::fs::symlink(&fixture.outside, &link)
        .expect("created a symlink out of the workspace");
    std::fs::write(fixture.workspace.join("inside.rs"), "fn inside() {}\n").expect("written");

    let tool = glob_tool(fixture.workspace.clone());
    for pattern in ["*.rs", "**/*.rs", "escape/*.rs"] {
        let result = run(
            &tool,
            serde_json::json!({ "pattern": pattern }),
            fixture.context(),
        )
        .await;
        let text = text_of(&result);
        assert!(
            !text.contains("leaked_glob_target"),
            "glob `{pattern}` listed `leaked_glob_target.rs`, which is outside the workspace: {text}"
        );
    }
}

#[tokio::test]
async fn glob_does_not_return_a_sibling_directory_that_the_pattern_names() {
    // The plain form of the same question, with no symlink involved: a pattern that spells its way
    // out with `..` must not be honoured, because the pattern is as much a path as an argument is.
    let fixture = Fixture::new("glob-dotdot");
    fixture.plant_outside_secret("sibling_target.rs");
    std::fs::write(fixture.workspace.join("inside.rs"), "fn inside() {}\n").expect("written");

    let tool = glob_tool(fixture.workspace.clone());
    let result = run(
        &tool,
        serde_json::json!({ "pattern": "../outside/sibling_target.rs" }),
        fixture.context(),
    )
    .await;

    let text = text_of(&result);
    // The pattern is echoed in the "no files match" message, so a check for the bare name would fail on a correct answer.
    // What must not appear is a *result line* naming the sibling, which would mean the walk listed it.
    // `no files match` echoes the pattern, so the name is present in a correct answer too. The distinguishing fact is the
    // shape: a match is a path line, and the miss message is the only output that starts with `no files match`.
    let listed = !text.starts_with("no files match") && text.contains("sibling_target");
    assert!(
        !listed,
        "glob followed a `..` pattern out of the workspace and listed a sibling file: {text}"
    );
}

// ---- 5. grep does not search outside ----------------------------------------------------------

#[tokio::test]
async fn grep_does_not_search_outside_the_workspace() {
    // `grep` takes a `path` argument that defaults to the workspace, which makes an explicit path
    // the obvious way out. The marker is planted outside and the pattern is the marker itself, so a
    // result containing it is proof the file was read rather than merely named.
    let fixture = Fixture::new("grep-dotdot");
    fixture.plant_outside_secret("secret.txt");
    std::fs::write(
        fixture.workspace.join("inside.txt"),
        "inside the workspace\n",
    )
    .expect("written");

    let tool = grep_tool(fixture.workspace.clone());
    let result = run(
        &tool,
        serde_json::json!({ "pattern": OUTSIDE_MARKER, "path": "../outside" }),
        fixture.context(),
    )
    .await;

    assert_refused(&result, "searching `../outside` with grep");

    // The absolute spelling of the same directory, which is the form a model produces when it read
    // the path from somewhere else rather than composing it.
    let absolute = fixture.outside.display().to_string();
    let result = run(
        &tool,
        serde_json::json!({ "pattern": OUTSIDE_MARKER, "path": absolute }),
        fixture.context(),
    )
    .await;

    assert_refused(
        &result,
        "searching an absolute path outside the workspace with grep",
    );
}

// ---- 6. a normal path inside still works ------------------------------------------------------

#[tokio::test]
async fn read_inside_the_workspace_succeeds() {
    // The control. A boundary that refuses everything is not a boundary, and without this case a
    // confinement that broke all reads would still pass the tests above.
    let fixture = Fixture::new("read-inside");
    std::fs::write(fixture.workspace.join("a.txt"), "first\nsecond\n").expect("written");
    let tool = read_tool(fixture.workspace.clone());

    let result = run(
        &tool,
        serde_json::json!({"path": "a.txt"}),
        fixture.context(),
    )
    .await;

    assert!(
        !result.is_error,
        "a read inside the workspace must succeed: {}",
        text_of(&result)
    );
    let text = text_of(&result);
    assert!(text.contains("first"), "{text}");
    assert!(text.contains("second"), "{text}");
}

#[tokio::test]
async fn write_inside_the_workspace_succeeds() {
    // The write control, asserted against the file rather than the message: the point is that a
    // legitimate write still reaches the disk once the boundary has an opinion.
    let fixture = Fixture::new("write-inside");
    let tool = write_tool(fixture.workspace.clone());

    let result = run(
        &tool,
        serde_json::json!({"path": "made.txt", "content": "inside\n"}),
        fixture.context(),
    )
    .await;

    assert!(
        !result.is_error,
        "a write inside the workspace must succeed: {}",
        text_of(&result)
    );
    let written = std::fs::read_to_string(fixture.workspace.join("made.txt")).expect("read back");
    assert_eq!(written, "inside\n");
}

// ---- 7. path normalisation --------------------------------------------------------------------

#[tokio::test]
async fn dot_slash_and_dotdot_segments_reach_the_same_file() {
    // Two spellings of one path must reach one file. `./a/../b` is what a model writes when it
    // tracked a directory it then left, and a confinement check that normalised differently from
    // the filesystem would refuse the spelling that is, in fact, inside.
    let fixture = Fixture::new("normalise");
    std::fs::create_dir_all(fixture.workspace.join("a")).expect("created");
    std::fs::write(fixture.workspace.join("b"), "the same file\n").expect("written");
    let tool = read_tool(fixture.workspace.clone());

    let plain = run(&tool, serde_json::json!({"path": "b"}), fixture.context()).await;
    let winding = run(
        &tool,
        serde_json::json!({"path": "./a/../b"}),
        fixture.context(),
    )
    .await;

    assert!(
        !plain.is_error,
        "reading `b` must succeed: {}",
        text_of(&plain)
    );
    assert!(
        !winding.is_error,
        "reading `./a/../b` must succeed, because it names the same file: {}",
        text_of(&winding)
    );
    assert_eq!(
        text_of(&plain),
        text_of(&winding),
        "the two spellings must produce one result"
    );
    assert!(
        text_of(&winding).contains("the same file"),
        "{}",
        text_of(&winding)
    );

    // The `..` that climbs exactly to the workspace root and no further is still inside, and it is
    // the boundary case a check that counts segments instead of resolving them gets wrong.
    let at_root = run(
        &tool,
        serde_json::json!({"path": "a/../b"}),
        fixture.context(),
    )
    .await;
    assert!(
        !at_root.is_error,
        "`a/../b` stays inside the workspace and must read: {}",
        text_of(&at_root)
    );
}

#[tokio::test]
async fn a_parent_relative_path_that_climbs_above_the_workspace_is_refused() {
    // The other side of normalisation: enough `..` to leave must be refused, while one that only
    // returns to the root (the case above) must not. Asserting both keeps the check from being
    // "any `..` is an escape", which would refuse legitimate paths.
    let fixture = Fixture::new("climb");
    fixture.plant_outside_secret("passwd");
    let tool = read_tool(fixture.workspace.clone());

    for path in [
        "../outside/passwd",
        "../../etc/passwd",
        "a/b/../../../outside/passwd",
    ] {
        let result = run(
            &tool,
            serde_json::json!({ "path": path }),
            fixture.context(),
        )
        .await;
        assert_refused(&result, &format!("reading `{path}`"));
    }
}
