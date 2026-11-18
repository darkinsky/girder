//! Tests for `working_dir_gap`: the three upstream `packages/session` modules that had no Rust
//! counterpart — `working-directory` (290 lines), `tool-working-directory` (43), `message-quotes`
//! (198, all browser UI).
//!
//! # What each suite is evidence for
//!
//! The module ports three different kinds of thing, so the evidence is three different kinds:
//!
//! - **Path rules and notices** are compared against upstream's *behaviour*, written out case by
//!   case from `index.ts` and `error.ts`. The interesting cases are the ones where a plausible
//!   implementation differs: `..` at the root (stays `/`), a relative path that runs out of segments
//!   (keeps the `..`), a request that is already the current directory (no notice).
//! - **The blockquote grammar** is compared against the real TypeScript, not against a reading of it.
//!   [`quote_corpus_matches_upstream_typescript`] replays 60 messages through
//!   `messageQuoteRanges`/`quoteTextOf` from `packages/session/message-quotes/src/client/quote-text.ts`
//!   and compares every span and every decoded excerpt. The offsets in the table are **byte** offsets
//!   converted from the UTF-16 indices upstream reports, which is the one thing the port changes and
//!   the one thing a table cannot leave implicit.
//! - **The tool** is run, not described: [`execute`] is awaited against a real temporary tree, because
//!   the branches that matter (`..`, a symlink, a file instead of a directory, a directory deleted
//!   under the session) are exactly the ones a pure function cannot answer.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written before the crate root wires it in.

#[path = "../src/working_dir_gap.rs"]
mod working_dir_gap;

use std::path::PathBuf;
use std::sync::Arc;

use girder_core::{ContextForm, MessageId, MessageSource, Projection, SessionEvent};
use serde_json::json;
use working_dir_gap::{
    CHANGE_EVENT_TYPE, Inspection, NOTICE_MARKER, Notice, PROJECTION_KEY, QuoteRange,
    TOOL_DESCRIPTION, TOOL_NAME, ToolArgs, WorkingDirectory, WorkingDirectoryErrorCode,
    WorkingDirectoryProjection, change, classify, context_text, decode_change, encode_change,
    ensure, execute, fold, fold_envelopes, is_absolute, is_blank, is_directory, is_js_whitespace,
    is_single_quote, json_quote, lexical, normalize_newlines, quote_excerpts, quote_ranges,
    quote_text, quotes, resolve, tool_parameters, validate_change,
};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// A temporary directory that removes itself, so a failed assertion does not leak a tree.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "girder-wdir-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).expect("create tree");
        std::fs::create_dir_all(root.join("a/b")).expect("create tree");
        std::fs::create_dir_all(root.join("empty")).expect("create tree");
        std::fs::write(root.join("file.txt"), "x").expect("write file");
        Self { root }
    }

    fn path(&self, suffix: &str) -> String {
        self.root.join(suffix).to_string_lossy().into_owned()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn directory(tree: &Tree) -> WorkingDirectory {
    WorkingDirectory::new(tree.path(""))
        .expect("absolute default")
        .with_original(Some(tree.path("a")))
        .expect("absolute original")
}

fn shared(state: WorkingDirectory) -> Arc<tokio::sync::Mutex<WorkingDirectory>> {
    Arc::new(tokio::sync::Mutex::new(state))
}

// ---------------------------------------------------------------------------------------------
// Capability 1: the working-directory service's path rules
// ---------------------------------------------------------------------------------------------

#[test]
fn absolute_detection_answers_for_both_platform_families() {
    for absolute in [
        "/",
        "/x",
        "/x/y",
        "\\\\server\\share",
        "C:\\x",
        "c:/x",
        "Z:\\",
    ] {
        assert!(is_absolute(absolute), "{absolute} should be absolute");
    }
    for relative in ["", "x", "./x", "../x", "C:", "C", "x/y", " x", "~/x"] {
        assert!(!is_absolute(relative), "{relative} should be relative");
    }
}

#[test]
fn lexical_folding_rules() {
    let cases = [
        ("/", "/"),
        ("/a", "/a"),
        ("/a/", "/a"),
        ("/a//b", "/a/b"),
        ("/a/./b", "/a/b"),
        ("/a/b/..", "/a"),
        ("/a/b/../c", "/a/c"),
        ("/a/b/../../..", "/"),
        ("/..", "/"),
        ("/.", "/"),
        ("a", "a"),
        ("a/", "a"),
        ("a/..", "."),
        ("a/../..", ".."),
        ("../a", "../a"),
        (".", "."),
        ("", "."),
        // A directory whose name starts with a space is not trimmed: `normalize_cache_path` trims,
        // and reusing it here would answer for a different directory.
        (" /a", " /a"),
        ("/a /b", "/a /b"),
    ];
    for (input, expected) in cases {
        assert_eq!(lexical(input), expected, "lexical({input:?})");
    }
}

#[test]
fn resolution_against_a_current_directory() {
    let cases = [
        ("/work", "sub", "/work/sub"),
        ("/work", "..", "/"),
        ("/work", "../other", "/other"),
        ("/work/", "sub", "/work/sub"),
        ("/work/a/b", "../../c", "/work/c"),
        ("/work", "/abs", "/abs"),
        ("/work", "/abs/./x", "/abs/x"),
        ("/work", "", "/work"),
        ("/", "a", "/a"),
    ];
    for (cwd, request, expected) in cases {
        assert_eq!(
            resolve(cwd, request),
            expected,
            "resolve({cwd:?}, {request:?})"
        );
    }
}

#[test]
fn an_empty_change_is_rejected_before_resolution() {
    let error = validate_change("/work", "").expect_err("empty is invalid");
    assert_eq!(error.code, WorkingDirectoryErrorCode::Invalid);
    assert!(error.message.contains("must not be empty"));
    assert_eq!(validate_change("/work", "a").expect("valid"), "/work/a");
}

#[test]
fn classification_only_accepts_a_directory() {
    assert_eq!(
        classify("/x", Inspection::Directory).expect("directory"),
        "/x"
    );
    for inspection in [Inspection::Missing, Inspection::NotADirectory] {
        let error = classify("/x", inspection).expect_err("not a directory");
        assert_eq!(error.code, WorkingDirectoryErrorCode::NotFound);
        // The message names where it looked, not what was asked for: a relative request is only
        // actionable once the candidate is spelled out.
        assert!(error.message.contains("\"/x\""), "{}", error.message);
    }
}

#[test]
fn error_codes_are_the_upstream_strings() {
    use working_dir_gap::WorkingDirectoryErrorCode as Code;
    assert_eq!(Code::Invalid.as_str(), "WORKING_DIRECTORY_INVALID");
    assert_eq!(Code::NotFound.as_str(), "WORKING_DIRECTORY_NOT_FOUND");
    assert_eq!(Code::Unavailable.as_str(), "WORKING_DIRECTORY_UNAVAILABLE");
    assert_eq!(Code::NotFound.to_string(), "WORKING_DIRECTORY_NOT_FOUND");
}

#[test]
fn a_relative_fallback_is_refused_with_the_code_upstream_uses() {
    let error = WorkingDirectory::new("relative/path").expect_err("relative default");
    assert_eq!(error.code, WorkingDirectoryErrorCode::Invalid);
    assert!(error.message.contains("must be absolute"));
    assert!(error.message.contains("relative/path"));
}

#[test]
fn a_relative_header_or_seed_is_refused_by_the_projection_schema() {
    let base = WorkingDirectory::new("/work").expect("absolute");
    assert_eq!(
        base.clone()
            .with_original(Some("rel".to_owned()))
            .expect_err("relative header")
            .code,
        WorkingDirectoryErrorCode::Invalid
    );
    assert_eq!(
        base.clone()
            .with_recorded(Some("rel".to_owned()))
            .expect_err("relative seed")
            .code,
        WorkingDirectoryErrorCode::Invalid
    );
    // `null` is a legal state, and is what an empty log means.
    assert!(
        !base
            .clone()
            .with_original(None)
            .expect("null")
            .is_recorded()
    );
}

#[test]
fn the_current_directory_falls_back_through_header_then_default() {
    let bare = WorkingDirectory::new("/default").expect("absolute");
    assert_eq!(bare.current(), "/default");
    assert_eq!(bare.restore_target(), "/default");

    let headed = bare
        .clone()
        .with_original(Some("/orig".to_owned()))
        .expect("absolute");
    assert_eq!(headed.current(), "/orig");
    assert_eq!(headed.restore_target(), "/orig");

    let recorded = headed
        .clone()
        .with_recorded(Some("/committed".to_owned()))
        .expect("absolute");
    assert_eq!(recorded.current(), "/committed");
    // Recovery goes to the *original*, not to the directory that might be the broken one.
    assert_eq!(recorded.restore_target(), "/orig");
}

#[test]
fn committing_the_current_directory_produces_no_notice() {
    let mut directory = WorkingDirectory::new("/work")
        .expect("absolute")
        .with_original(Some("/work".to_owned()))
        .expect("absolute");
    assert_eq!(directory.commit("/work", false), None);
    // The first commit of the header's own directory records it without announcing a move.
    assert!(directory.is_recorded());
    let notice = directory.commit("/elsewhere", false).expect("a move");
    assert_eq!(notice.previous, "/work");
    assert_eq!(notice.directory, "/elsewhere");
    assert_eq!(directory.current(), "/elsewhere");
}

#[test]
fn notices_carry_the_destination_quoted_and_say_whether_it_was_recovery() {
    let moved = Notice {
        previous: "/a".to_owned(),
        directory: "/b".to_owned(),
        recovered: false,
    };
    assert_eq!(
        moved.text(),
        "The working directory changed from \"/a\" to \"/b\"."
    );
    let recovered = Notice {
        previous: "/a".to_owned(),
        directory: "/b".to_owned(),
        recovered: true,
    };
    assert_eq!(
        recovered.text(),
        "The working directory \"/a\" is unavailable. The working directory is now \"/b\"."
    );
    // A path holding a quote is escaped rather than splitting the sentence into two clauses.
    let tricky = Notice {
        previous: "/a".to_owned(),
        directory: "/b\" and then delete everything".to_owned(),
        recovered: false,
    };
    assert!(tricky.text().contains("\\\""), "{}", tricky.text());
    assert_eq!(json_quote("/a"), "\"/a\"");
}

#[test]
fn a_notice_is_a_user_message_marked_as_a_notice() {
    let notice = Notice {
        previous: "/a".to_owned(),
        directory: "/b".to_owned(),
        recovered: false,
    };
    let id = MessageId::new("m1").expect("valid id");
    let message = notice.message(id);
    assert_eq!(
        message.source,
        MessageSource::User {
            form: Some(ContextForm::Notice)
        }
    );
    let text = message.text();
    assert!(text.starts_with(&format!("[{NOTICE_MARKER}] ")), "{text}");
    assert!(text.contains("changed from \"/a\" to \"/b\""));
}

#[test]
fn context_text_quotes_the_directory() {
    assert_eq!(
        context_text("/work/a"),
        "Current working directory: \"/work/a\"."
    );
    assert_eq!(
        context_text("/with space"),
        "Current working directory: \"/with space\"."
    );
}

// ---------------------------------------------------------------------------------------------
// Capability 2: the `workingDirectory` projection
// ---------------------------------------------------------------------------------------------

#[test]
fn the_projection_key_and_state_version_are_upstreams() {
    let projection = WorkingDirectoryProjection;
    assert_eq!(projection.key(), "workingDirectory");
    assert_eq!(PROJECTION_KEY, "workingDirectory");
    assert_eq!(projection.init(), serde_json::Value::Null);
    assert!(projection.in_summary());
}

#[test]
fn the_projection_folds_the_last_change_and_ignores_other_events() {
    let projection = WorkingDirectoryProjection;
    let state = WorkingDirectoryProjection::init_from_header(Some("/orig"));
    assert_eq!(state, json!("/orig"));
    assert_eq!(
        WorkingDirectoryProjection::init_from_header(None),
        serde_json::Value::Null
    );

    // A turn boundary does not move the directory.
    let turn = SessionEvent::TurnStart { turn: 1 };
    assert_eq!(projection.apply(state.clone(), &turn, 1), state);

    // A notice with no recoverable directory does not move it either.
    let notice = SessionEvent::UserMessage {
        message: girder_core::ChatMessage::user(MessageId::new("m1").expect("id"), "hello"),
    };
    assert_eq!(projection.apply(state.clone(), &notice, 2), state);

    // An empty log has no directory at all: `null`, not the launch directory.
    assert_eq!(
        fold(None, &[]),
        None,
        "no header and no event has no answer to guess"
    );
}

#[test]
fn fold_prefers_the_last_change_over_the_header() {
    use working_dir_gap::ChangeEvent;
    let changes = vec![
        ChangeEvent {
            cwd: "/one".to_owned(),
        },
        ChangeEvent {
            cwd: "/two".to_owned(),
        },
    ];
    assert_eq!(fold(Some("/orig"), &changes), Some("/two".to_owned()));
    assert_eq!(fold(Some("/orig"), &[]), Some("/orig".to_owned()));
    assert_eq!(fold(None, &changes), Some("/two".to_owned()));
}

#[test]
fn change_events_decode_from_both_envelopes_and_reject_corrupt_payloads() {
    let nested = json!({"type": CHANGE_EVENT_TYPE, "data": {"cwd": "/nested"}});
    let flat = json!({"type": CHANGE_EVENT_TYPE, "cwd": "/flat"});
    assert_eq!(
        decode_change(&nested).expect("nested"),
        Some(working_dir_gap::ChangeEvent {
            cwd: "/nested".to_owned()
        })
    );
    assert_eq!(
        decode_change(&flat).expect("flat"),
        Some(working_dir_gap::ChangeEvent {
            cwd: "/flat".to_owned()
        })
    );

    // Some other event: not an error, just not ours.
    assert_eq!(
        decode_change(&json!({"type": "turn/start", "turn": 1})),
        Ok(None)
    );
    assert_eq!(decode_change(&json!("not an object")), Ok(None));

    // Ours but unusable: each failure has its own answer rather than "invalid".
    assert!(decode_change(&json!({"type": CHANGE_EVENT_TYPE})).is_err());
    assert!(decode_change(&json!({"type": CHANGE_EVENT_TYPE, "data": {"cwd": ""}})).is_err());
    let relative =
        decode_change(&json!({"type": CHANGE_EVENT_TYPE, "cwd": "rel"})).expect_err("relative");
    assert!(relative.to_string().contains("relative"));
}

#[test]
fn encoding_a_change_refuses_what_the_reader_would_refuse() {
    let encoded = encode_change("/work").expect("absolute");
    assert_eq!(encoded, json!({"type": CHANGE_EVENT_TYPE, "cwd": "/work"}));
    assert!(encode_change("").is_err());
    assert!(encode_change("relative").is_err());
    // Round trip: a written event is a readable one.
    assert_eq!(
        decode_change(&encoded)
            .expect("decodes")
            .expect("is a change")
            .cwd,
        "/work"
    );
}

#[test]
fn folding_envelopes_skips_corrupt_events_rather_than_losing_the_state() {
    let events = vec![
        json!({"type": CHANGE_EVENT_TYPE, "data": {"cwd": "/one"}}),
        json!({"type": "turn/start", "turn": 1}),
        json!({"type": CHANGE_EVENT_TYPE, "data": {"cwd": "relative"}}),
        json!({"type": CHANGE_EVENT_TYPE, "data": {"cwd": "/two"}}),
    ];
    assert_eq!(
        fold_envelopes(Some("/orig"), &events),
        Some("/two".to_owned())
    );
    assert_eq!(fold_envelopes(Some("/orig"), &[]), Some("/orig".to_owned()));
    assert_eq!(fold_envelopes(None, &[]), None);
}

// ---------------------------------------------------------------------------------------------
// Capability 3: the `working_directory` tool
// ---------------------------------------------------------------------------------------------

#[test]
fn the_tool_schema_is_the_one_the_model_sees() {
    assert_eq!(TOOL_NAME, "working_directory");
    assert!(
        TOOL_DESCRIPTION
            .contains("Existing shells and running processes keep their own directories")
    );
    let parameters = tool_parameters();
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["properties"]["cd"]["type"], "string");
    // Nothing is required: omitting `cd` is how the directory is read.
    assert!(parameters.get("required").is_none());
    let output = working_dir_gap::tool_output_schema();
    assert_eq!(output["additionalProperties"], false);
    assert_eq!(output["properties"]["cwd"]["required"], true);
}

#[test]
fn arguments_distinguish_read_from_change_and_reject_an_empty_cd() {
    assert_eq!(
        ToolArgs::parse(&json!({})).expect("omitted"),
        ToolArgs { cd: None }
    );
    // An explicit null is an omission: some providers serialise an absent optional field that way.
    assert_eq!(
        ToolArgs::parse(&json!({"cd": null})).expect("null"),
        ToolArgs { cd: None }
    );
    assert_eq!(
        ToolArgs::parse(&json!({"cd": ".."})).expect("relative"),
        ToolArgs {
            cd: Some("..".to_owned())
        }
    );
    let empty = ToolArgs::parse(&json!({"cd": ""})).expect_err("empty");
    assert_eq!(empty.code, WorkingDirectoryErrorCode::Invalid);
    assert!(
        empty.message.contains("omit it to read"),
        "{}",
        empty.message
    );
    let wrong = ToolArgs::parse(&json!({"cd": 3})).expect_err("not a string");
    assert_eq!(wrong.code, WorkingDirectoryErrorCode::Invalid);
}

#[tokio::test]
async fn reading_the_directory_returns_the_absolute_path() {
    let tree = Tree::new("read");
    let state = shared(directory(&tree));
    let value = execute(&json!({}), &state).await.expect("read");
    assert_eq!(value, json!({"cwd": tree.path("a")}));
    // Reading records the header's directory, because a session that has a directory has moved.
    assert!(state.lock().await.is_recorded());
}

#[tokio::test]
async fn changing_into_a_relative_child_resolves_against_the_current_directory() {
    let tree = Tree::new("child");
    let state = shared(directory(&tree));
    let value = execute(&json!({"cd": "b"}), &state).await.expect("cd b");
    assert_eq!(value["cwd"], json!(tree.path("a/b")));

    // `..` climbs back, and a second call resolves against the new directory rather than the old one.
    let value = execute(&json!({"cd": ".."}), &state).await.expect("cd ..");
    assert_eq!(value["cwd"], json!(tree.path("a")));

    // An absolute request ignores the current directory.
    let value = execute(&json!({"cd": tree.path("empty")}), &state)
        .await
        .expect("cd absolute");
    assert_eq!(value["cwd"], json!(tree.path("empty")));
}

#[tokio::test]
async fn entering_a_file_or_a_missing_path_fails_with_not_found() {
    let tree = Tree::new("missing");
    let state = shared(directory(&tree));

    let file = execute(&json!({"cd": "file.txt"}), &state)
        .await
        .expect_err("a file");
    assert_eq!(file.code, WorkingDirectoryErrorCode::NotFound);
    assert!(
        file.message.contains("directory does not exist"),
        "{}",
        file.message
    );

    let missing = execute(&json!({"cd": "nope"}), &state)
        .await
        .expect_err("missing");
    assert_eq!(missing.code, WorkingDirectoryErrorCode::NotFound);

    // A trailing slash does not make a file a directory.
    let slashed = execute(&json!({"cd": "file.txt/"}), &state)
        .await
        .expect_err("slashed file");
    assert_eq!(slashed.code, WorkingDirectoryErrorCode::NotFound);

    // The failed attempts left the directory alone.
    assert_eq!(state.lock().await.current(), tree.path("a"));
}

#[tokio::test]
async fn a_deleted_directory_is_restored_to_the_original_project() {
    let tree = Tree::new("restore");
    let mut state = directory(&tree);
    // A directory that no longer exists: the session moved into it and it was removed.
    let gone = tree.path("gone");
    std::fs::create_dir_all(&gone).expect("create");
    let _ = change(&mut state, &gone).await.expect("enter");
    std::fs::remove_dir_all(&gone).expect("remove");

    let restored = ensure(&mut state).await.expect("recovery");
    assert_eq!(restored, tree.path("a"));
    assert_eq!(state.current(), tree.path("a"));
}

#[tokio::test]
async fn recovery_fails_when_the_original_project_is_gone_too() {
    let tree = Tree::new("unavailable");
    let mut state = directory(&tree);
    std::fs::remove_dir_all(tree.path("a")).expect("remove original");
    let error = ensure(&mut state).await.expect_err("nothing to restore");
    assert_eq!(error.code, WorkingDirectoryErrorCode::Unavailable);
    // Both paths are named: the operator needs to know what was tried.
    assert!(error.message.contains("\"/"), "{}", error.message);
    assert!(
        error.message.contains("also unavailable"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn recovery_of_a_session_with_no_header_uses_the_deployment_fallback() {
    let tree = Tree::new("fallback");
    let gone = tree.path("empty/gone");
    std::fs::create_dir_all(&gone).expect("create");
    // No header at all — a session with no original project — whose recorded directory then vanished
    // while the deployment fallback still exists: recovery goes to the fallback.
    let mut state = WorkingDirectory::new(tree.path("a"))
        .expect("absolute")
        .with_recorded(Some(gone.clone()))
        .expect("absolute");
    std::fs::remove_dir_all(&gone).expect("remove");
    let restored = ensure(&mut state).await.expect("recovery");
    assert_eq!(restored, tree.path("a"));
    assert_eq!(state.current(), tree.path("a"));
}

#[tokio::test]
async fn a_symlinked_directory_is_recorded_as_the_path_a_later_read_will_see() {
    let tree = Tree::new("symlink");
    let link = tree.path("link");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(tree.path("a"), &link).expect("symlink");
        let mut state = WorkingDirectory::new(tree.path("")).expect("absolute");
        let entered = change(&mut state, "link").await.expect("enter link");
        // The canonical path, not the link: a `read` inside it resolves the same way.
        assert_eq!(
            entered,
            std::fs::canonicalize(tree.path("a")).expect("canonical")
        );
        assert_eq!(state.current(), entered);
    }
    #[cfg(not(unix))]
    {
        let _ = link;
    }
}

#[tokio::test]
async fn the_tool_definition_runs_the_same_behaviour_through_the_registry_shape() {
    let tree = Tree::new("tool");
    let tool = working_dir_gap::working_directory_tool(shared(directory(&tree)));
    assert_eq!(tool.name, "working_directory");
    let schema = tool.schema();
    assert_eq!(schema.name, "working_directory");
    assert!(schema.parameters["properties"]["cd"].is_object());

    // The body is callable, and reports a failure as `ToolError::Failed` rather than panicking.
    let error = (tool.execute)(
        json!({"cd": "file.txt"}),
        girder_core::ToolRunContext::default(),
    )
    .await
    .expect_err("a file is not a directory");
    assert!(error.to_string().contains("working_directory"), "{error}");
}

#[test]
fn tool_results_render_the_bare_path() {
    let result = working_dir_gap::tool_result("/work/a");
    assert!(!result.is_error);
    let text = match &result.content[0] {
        girder_core::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(text, "/work/a");
    assert_eq!(
        result
            .value
            .as_ref()
            .map(girder_core::ToolValue::to_content),
        Some(vec![girder_core::ContentBlock::Text {
            text: "{\"cwd\":\"/work/a\"}".to_owned()
        }])
    );
}

#[test]
fn is_directory_separates_files_from_directories() {
    let tree = Tree::new("isdir");
    assert!(is_directory(tree.path("a")));
    assert!(!is_directory(tree.path("file.txt")));
    assert!(!is_directory(tree.path("nope")));
}

// ---------------------------------------------------------------------------------------------
// Capability 4: the blockquote grammar, differentially against the TypeScript
// ---------------------------------------------------------------------------------------------

/// `messageQuoteRanges` + `quoteTextOf` from the real
/// `packages/session/message-quotes/src/client/quote-text.ts`, replayed.
///
/// The table was produced by running the TypeScript under Node and converting each UTF-16 index to a
/// byte offset; the expected excerpts are that run's `quoteTextOf` output verbatim. Reading the
/// source would have caught the ` {0,3}` bound; only running it catches that `U+2028` is a line start
/// for `^` but not a line ending for `[^\r\n]*`, and that `"> \u{FEFF}"` is blank for JavaScript and
/// not for Rust.
#[test]
fn quote_corpus_matches_upstream_typescript() {
    // (message, spans as byte offsets, decoded excerpts)
    type Case = (
        &'static str,
        &'static [(usize, usize)],
        &'static [&'static str],
    );
    let corpus: &[Case] = &[
        ("", &[], &[]),
        ("> a", &[(0, 3)], &["a"]),
        ("> a\n", &[(0, 3)], &["a"]),
        (">a", &[(0, 2)], &["a"]),
        (">  a", &[(0, 4)], &[" a"]),
        (">\ta", &[(0, 3)], &["a"]),
        (" > a", &[(0, 4)], &["a"]),
        ("  > a", &[(0, 5)], &["a"]),
        ("   > a", &[(0, 6)], &["a"]),
        // Four spaces is indented code, not a quote.
        ("    > a", &[], &[]),
        ("> a\n> b", &[(0, 7)], &["a\nb"]),
        ("> a\n> b\n", &[(0, 7)], &["a\nb"]),
        ("> a\n\n> b", &[(0, 3), (5, 8)], &["a", "b"]),
        ("> a\n \n> b", &[(0, 3), (6, 9)], &["a", "b"]),
        // A continuation line that is only a marker is part of the span.
        ("> a\n>", &[(0, 5)], &["a\n"]),
        ("> a\n> \n> b", &[(0, 10)], &["a\n\nb"]),
        ("text before\n> quote", &[(12, 19)], &["quote"]),
        ("> quote\ntext after", &[(0, 7)], &["quote"]),
        // An inline `>` is ordinary text.
        ("a > b\nc", &[], &[]),
        (">> a", &[(0, 4)], &["> a"]),
        ("> > a", &[(0, 5)], &["> a"]),
        ("> a\r\n> b", &[(0, 8)], &["a\nb"]),
        // A lone `\r` ends the span: `[^\r\n]*` stops there, and `\r?\n` needs the `\n`.
        ("> a\r> b", &[(0, 3), (4, 7)], &["a", "b"]),
        ("> a\n>b\n>c", &[(0, 9)], &["a\nb\nc"]),
        ("> 中文引用", &[(0, 14)], &["中文引用"]),
        ("> emoji 🎉\n> next", &[(0, 19)], &["emoji 🎉\nnext"]),
        // A tab after the marker is consumed; a tab *before* it is not a quote.
        ("  > sp\n\t> tab", &[(0, 6)], &["sp"]),
        ("> \n> x", &[(0, 6)], &["\nx"]),
        (">\n>\n> x", &[(0, 7)], &["\n\nx"]),
        ("   \n> y", &[(4, 7)], &["y"]),
        ("> a\n\n\n> b", &[(0, 3), (6, 9)], &["a", "b"]),
        (
            "> a\n\n> b\n\n> c",
            &[(0, 3), (5, 8), (10, 13)],
            &["a", "b", "c"],
        ),
        ("> # heading", &[(0, 11)], &["# heading"]),
        ("> ```\n> code\n> ```", &[(0, 18)], &["```\ncode\n```"]),
        (">>", &[(0, 2)], &[">"]),
        (">>>", &[(0, 3)], &[">>"]),
        ("> a\n>> b\n> c", &[(0, 12)], &["a\n> b\nc"]),
        ("no quotes at all", &[], &[]),
        (
            "\n\n> after blank lines",
            &[(2, 21)],
            &["after blank lines"],
        ),
        ("> a\n>b\n\n>c", &[(0, 6), (8, 10)], &["a\nb", "c"]),
        ("   > a\n   > b\n   > c", &[(0, 20)], &["a\nb\nc"]),
        ("    > a\n> b", &[(8, 11)], &["b"]),
        (">a\n> b", &[(0, 6)], &["a\nb"]),
        (" > a", &[(0, 4)], &["a"]),
        ("  \t> a", &[], &[]),
        ("\t> a", &[], &[]),
        ("é> a\né> b", &[], &[]),
        ("😀> a", &[], &[]),
        // `U+2028` starts a line for `^m` but is swallowed by `[^\r\n]*`: one span, not two.
        ("> \u{2028}> x", &[(0, 8)], &["\u{2028}x"]),
        ("p\n\u{2028}\n> z", &[(6, 9)], &["z"]),
        ("> \u{FEFF}", &[], &[]),
        ("> \u{85}", &[(0, 4)], &["\u{85}"]),
        ("> \u{FEFF}x", &[(0, 6)], &["\u{FEFF}x"]),
        ("> \u{85}x", &[(0, 5)], &["\u{85}x"]),
        ("> \u{A0}", &[], &[]),
        ("> \u{2003}\n> y", &[(0, 9)], &["\u{2003}\ny"]),
        // 7 UTF-16 units, 9 bytes: the conversion the port makes, spelled out.
        ("> a\u{2028}> b", &[(0, 9)], &["a\u{2028}b"]),
    ];

    for (text, expected_ranges, expected_excerpts) in corpus {
        let ranges = quote_ranges(text);
        let actual: Vec<(usize, usize)> = ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect();
        assert_eq!(
            actual, *expected_ranges,
            "spans for {text:?} (Rust {text:?})"
        );
        let excerpts: Vec<String> = ranges
            .iter()
            .map(|range| quote_text(range.slice(text)))
            .collect();
        assert_eq!(excerpts, *expected_excerpts, "excerpts for {text:?}");
        // The offsets must be usable as byte slices, which is the whole point of converting them.
        for range in &ranges {
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
        }
    }
}

#[test]
fn quote_text_normalizes_line_endings_before_removing_markers() {
    assert_eq!(normalize_newlines("a\r\nb\rc"), "a\nb\nc");
    assert_eq!(quote_text("> a\r\n> b"), "a\nb");
    // Every line ending is normalized, so a lone `\r` also becomes a marker boundary here.
    assert_eq!(quote_text("> a\r> b"), "a\nb");
}

#[test]
fn a_second_marker_is_kept_because_a_quotation_of_a_quotation_is_literal() {
    assert_eq!(quote_text("> > a"), "> a");
    assert_eq!(quote_excerpts("> > a\n> b"), vec!["> a\nb".to_owned()]);
}

#[test]
fn quotes_carry_both_the_offsets_and_the_excerpt() {
    let parsed = quotes("intro\n> one\n> two\n\n> three");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].range, QuoteRange { start: 6, end: 17 });
    assert_eq!(parsed[0].text, "one\ntwo");
    assert_eq!(parsed[1].text, "three");
}

#[test]
fn whitespace_and_blankness_follow_javascript_not_rust() {
    // The one disagreement that matters: `U+FEFF` is blank for JavaScript's `trim`, and a quote that
    // decodes to nothing is not a quote.
    assert!(is_js_whitespace('\u{FEFF}'));
    assert!(!is_js_whitespace('a'));
    assert!(!is_js_whitespace('\u{200B}'));
    assert!(is_blank(" \t\r\n"));
    assert!(is_blank("\u{FEFF}"));
    assert!(!is_blank("x"));
}

#[test]
fn a_quoted_selection_is_one_span_that_spans_the_message() {
    assert!(is_single_quote("> just a quote"));
    assert!(is_single_quote("> a\n> b\n\n"));
    assert!(!is_single_quote("> a\n\ntyped question"));
    assert!(!is_single_quote("> a\n\n> b"));
    assert!(!is_single_quote("no quote"));
    assert!(!is_single_quote(""));
}

#[test]
fn a_range_slices_the_original_text() {
    let text = "> 中文引用";
    let range = quote_ranges(text)[0];
    assert_eq!(range.slice(text), "> 中文引用");
    assert_eq!(quote_text(range.slice(text)), "中文引用");
}
