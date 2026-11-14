//! Tests for the deterministic compaction summary.
//!
//! The property under test is preservation, not brevity: whatever a segment records, the user's own
//! words and the complete paths must survive the trip through `summarize` and `render` unchanged.
//! Every other field may be lossy without harm; those two may not, so they are checked as text rather
//! than as structure — a value that is right in the struct and wrong after rendering is still wrong.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/compaction_summary.rs"]
mod compaction_summary;

use compaction_summary::{render, summarize};
use girder_core::{
    ChatMessage, ChatRole, ContentBlock, MessageId, MessageSource, SUMMARY_CLOSE_TAG,
    SUMMARY_OPEN_TAG, ToolCallId,
};

fn id(value: &str) -> MessageId {
    MessageId::new(value).expect("valid message id")
}

fn call_id(value: &str) -> ToolCallId {
    ToolCallId::new(value).expect("valid call id")
}

fn user(index: usize, text: &str) -> ChatMessage {
    ChatMessage::user(id(&format!("u{index}")), text)
}

fn assistant(index: usize, text: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("a{index}")),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "test".to_owned(),
            model: "test".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    )
}

/// An assistant message asking for one tool call with the given raw arguments.
fn call(index: usize, name: &str, arguments: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("a{index}")),
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "test".to_owned(),
            model: "test".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::ToolCall {
            id: call_id(&format!("c{index}")),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }],
    )
}

/// A tool result answering `index`, with the given text.
fn result(index: usize, text: &str) -> ChatMessage {
    ChatMessage::new(
        id(&format!("r{index}")),
        ChatRole::Tool,
        MessageSource::Tool {
            call_id: call_id(&format!("c{index}")),
        },
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    )
}

fn checkpoint(index: usize, body: &str) -> ChatMessage {
    ChatMessage::user(
        id(&format!("k{index}")),
        format!(
            "This is an automatically generated checkpoint.\n\n{SUMMARY_OPEN_TAG}\n{body}\n\
             {SUMMARY_CLOSE_TAG}"
        ),
    )
}

// ---- single turn ------------------------------------------------------------------------------

#[test]
fn a_single_turn_records_its_request_and_step_count() {
    let messages = vec![
        user(1, "add a flag to the CLI"),
        call(2, "read", r#"{"path":"crates/girder-cli/src/main.rs"}"#),
        result(3, "fn main() {}"),
        assistant(4, "done"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.turns, 1..2, "one request is one turn");
    assert_eq!(segment.requests, vec!["add a flag to the CLI"]);
    assert_eq!(segment.tools, vec!["read"]);
    assert_eq!(segment.files, vec!["crates/girder-cli/src/main.rs"]);
    assert_eq!(
        segment.steps, 2,
        "two assistant messages, one of them a tool call"
    );
}

#[test]
fn a_single_turn_with_no_tools_has_empty_tools_and_files() {
    let messages = vec![
        user(1, "explain the registry"),
        assistant(2, "it is a typed table"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.turns, 1..2);
    assert!(segment.tools.is_empty());
    assert!(segment.files.is_empty());
    assert_eq!(segment.steps, 1);
}

// ---- multiple turns ---------------------------------------------------------------------------

#[test]
fn several_turns_are_numbered_across_the_span() {
    let messages = vec![
        user(1, "first"),
        assistant(2, "ok"),
        user(3, "second"),
        assistant(4, "ok"),
        user(5, "third"),
        assistant(6, "ok"),
    ];

    let segment = summarize(&messages);

    assert_eq!(
        segment.turns,
        1..4,
        "three requests, three turns, half-open"
    );
    assert_eq!(segment.requests, vec!["first", "second", "third"]);
    assert_eq!(segment.steps, 3);
}

#[test]
fn a_multi_turn_span_keeps_tool_and_file_order_of_first_use() {
    let messages = vec![
        user(1, "look at two files"),
        call(2, "read", r#"{"path":"a.rs"}"#),
        result(3, "a"),
        call(4, "edit", r#"{"path":"b.rs"}"#),
        result(5, "b"),
        user(6, "now go back"),
        call(7, "read", r#"{"path":"a.rs"}"#),
        result(8, "a again"),
    ];

    let segment = summarize(&messages);

    assert_eq!(
        segment.tools,
        vec!["read", "edit"],
        "deduplicated, first use first"
    );
    assert_eq!(
        segment.files,
        vec!["a.rs", "b.rs"],
        "deduplicated, first use first"
    );
    assert_eq!(segment.turns, 1..3);
}

// ---- verbatim requests (the core property) ----------------------------------------------------

#[test]
fn a_user_request_survives_summarize_verbatim() {
    // Deliberately awkward: a conditional, a negation, and an exact identifier. Any of these is what
    // a paraphrase would quietly drop, and each one changes what the right implementation is.
    let request = "Rename `--jobs` to `--parallel`, but keep `--jobs` working as a deprecated alias \
                   for ONE release, and do NOT change any test that already passes.";

    let segment = summarize(&[user(1, request), assistant(2, "understood")]);

    assert_eq!(
        segment.requests,
        vec![request],
        "no trimming, no paraphrase"
    );
}

#[test]
fn a_verbatim_request_survives_rendering() {
    // The struct is right in the test above; this one checks the text a model would actually read.
    let request = "Do NOT touch crates/girder-core/src/lib.rs; the wiring is done by hand.";

    let rendered = render(&summarize(&[user(1, request)]));

    assert!(
        rendered.contains(request),
        "the request must appear whole in the rendered text, got:\n{rendered}"
    );
}

#[test]
fn every_request_survives_when_several_are_present() {
    let requests = [
        "add the flag",
        "and rename the subcommand",
        "actually revert the rename, keep only the flag",
    ];
    let mut messages = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        messages.push(user(index * 2 + 1, request));
        messages.push(assistant(index * 2 + 2, "ok"));
    }

    let rendered = render(&summarize(&messages));

    for request in requests {
        assert!(
            rendered.contains(request),
            "missing {request:?} in:\n{rendered}"
        );
    }
}

#[test]
fn a_request_is_not_trimmed_of_internal_newlines() {
    let request = "do two things:\n1. bump the version\n2. tag the release";

    let segment = summarize(&[user(1, request)]);

    assert_eq!(segment.requests, vec![request]);
    assert!(
        render(&segment).contains("1. bump the version\n2. tag the release"),
        "a multi-line request must not be collapsed"
    );
}

// ---- file paths -------------------------------------------------------------------------------

#[test]
fn identical_paths_from_different_tools_are_kept_once() {
    let messages = vec![
        user(1, "fix it"),
        call(2, "read", r#"{"path":"crates/girder-core/src/agent.rs"}"#),
        result(3, "…"),
        call(4, "edit", r#"{"path":"crates/girder-core/src/agent.rs"}"#),
        result(5, "ok"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.files, vec!["crates/girder-core/src/agent.rs"]);
}

#[test]
fn a_path_survives_rendering_whole() {
    let path = "crates/girder-core/src/compaction_summary.rs";

    let rendered = render(&summarize(&[
        user(1, "read it"),
        call(2, "read", &format!(r#"{{"path":"{path}"}}"#)),
        result(3, "…"),
    ]));

    assert!(
        rendered.contains(path),
        "full path must survive rendering:\n{rendered}"
    );
}

#[test]
fn a_path_is_not_canonicalised_or_shortened() {
    // Relative and absolute spellings of the same file are different strings, and this layer has no
    // workspace root to compare them against. Recording both is honest; picking one would be a guess.
    let messages = vec![
        user(1, "both spellings"),
        call(2, "read", r#"{"path":"./src/main.rs"}"#),
        result(3, "…"),
        call(4, "read", r#"{"path":"/abs/src/main.rs"}"#),
        result(5, "…"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.files, vec!["./src/main.rs", "/abs/src/main.rs"]);
}

#[test]
fn copy_and_move_contribute_both_of_their_paths() {
    let messages = vec![
        user(1, "move it"),
        call(
            2,
            "move_path",
            r#"{"source":"old/name.rs","destination":"new/name.rs"}"#,
        ),
        result(3, "ok"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.files, vec!["old/name.rs", "new/name.rs"]);
}

#[test]
fn a_patch_contributes_every_path_it_names() {
    let patch = "*** Begin Patch\n*** Update File: a.rs\n@@ fn x\n*** Add File: b.rs\n\
                 *** Delete File: c.rs\n*** End Patch";
    let arguments = serde_json::json!({ "patch": patch }).to_string();

    let segment = summarize(&[
        user(1, "apply it"),
        call(2, "apply_patch", &arguments),
        result(3, "ok"),
    ]);

    assert_eq!(
        segment.files,
        vec!["a.rs", "b.rs", "c.rs"],
        "patch paths must not be invisible to the summary"
    );
}

#[test]
fn non_path_string_arguments_are_not_mistaken_for_paths() {
    // `write` takes a `content` field full of source text; treating every string as a path would
    // bury the real paths under code fragments.
    let messages = vec![
        user(1, "write it"),
        call(
            2,
            "write",
            r#"{"path":"src/lib.rs","content":"fn main() { println!(\"hi\"); }"}"#,
        ),
        result(3, "ok"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.files, vec!["src/lib.rs"]);
}

#[test]
fn malformed_tool_arguments_contribute_nothing_and_do_not_panic() {
    let messages = vec![
        user(1, "try it"),
        call(2, "read", "{not json"),
        result(3, "error"),
        call(4, "read", r#"{"path":"still/works.rs"}"#),
        result(5, "ok"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.tools, vec!["read"], "the tool still ran");
    assert_eq!(
        segment.files,
        vec!["still/works.rs"],
        "one bad call must not hide the good one"
    );
}

// ---- tool deduplication -----------------------------------------------------------------------

#[test]
fn repeated_tool_use_is_recorded_once() {
    let messages = vec![
        user(1, "run it a few times"),
        call(2, "bash", r#"{"command":"ls"}"#),
        result(3, "a"),
        call(4, "bash", r#"{"command":"pwd"}"#),
        result(5, "/"),
        call(6, "bash", r#"{"command":"git status"}"#),
        result(7, "clean"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.tools, vec!["bash"]);
}

#[test]
fn several_tools_are_listed_once_each_in_use_order() {
    let messages = vec![
        user(1, "do the work"),
        call(2, "read", r#"{"path":"a.rs"}"#),
        result(3, "a"),
        call(4, "bash", "{}"),
        result(5, "b"),
        call(6, "read", r#"{"path":"b.rs"}"#),
        result(7, "c"),
        call(8, "todo_write", "{}"),
        result(9, "d"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.tools, vec!["read", "bash", "todo_write"]);
}

// ---- rendering --------------------------------------------------------------------------------

#[test]
fn render_labels_each_section_so_a_reader_can_skip_it() {
    let segment = summarize(&[
        user(1, "ship it"),
        call(2, "bash", r#"{"command":"cargo test"}"#),
        result(3, "ok"),
        assistant(4, "shipped"),
    ]);

    let rendered = render(&segment);

    assert!(rendered.contains("turns 1..2"), "turn range:\n{rendered}");
    assert!(
        rendered.contains("requests"),
        "requests header:\n{rendered}"
    );
    assert!(rendered.contains("tools: bash"), "tools line:\n{rendered}");
    assert!(rendered.contains("steps: 2"), "step count:\n{rendered}");
}

#[test]
fn render_produces_exactly_the_expected_block() {
    // Pinned as text rather than checked field by field: the shape is the deliverable, and a change
    // here changes what every future compaction puts in front of the model.
    let segment = summarize(&[
        user(1, "add the flag"),
        call(2, "read", r#"{"path":"a.rs"}"#),
        result(3, "…"),
        assistant(4, "done"),
    ]);

    assert_eq!(
        render(&segment),
        "compacted span: turns 1..2\n\
         requests (verbatim, in order):\n\
         - add the flag\n\
         tools: read\n\
         files touched (full paths):\n\
         - a.rs\n\
         steps: 2"
    );
}

#[test]
fn render_names_the_absent_sections_explicitly() {
    // `(none)` rather than an omitted line: an absent line is indistinguishable from a field the
    // renderer forgot, which is exactly the bug a checkpoint reader cannot see.
    let rendered = render(&summarize(&[assistant(1, "I did something on my own")]));

    assert!(
        rendered.contains("requests: (none)"),
        "an absent request list must be visible:\n{rendered}"
    );
    assert!(
        rendered.contains("no turn boundary"),
        "a span with no request must not invent turns:\n{rendered}"
    );
}

#[test]
fn render_is_much_shorter_than_the_messages_it_replaces() {
    // The summary sits in the context and costs tokens, so the contract is "compact but complete".
    // This is a loose bound on purpose: it catches an accidental transcript dump, not a small
    // regression in wording.
    let mut messages = vec![user(1, "refactor the module")];
    for index in 0..10 {
        messages.push(call(
            index * 3 + 2,
            "read",
            r#"{"path":"crates/girder-core/src/agent.rs"}"#,
        ));
        messages.push(result(
            index * 3 + 3,
            &"fn long_function() { /* lots of source */ }\n".repeat(20),
        ));
        messages.push(assistant(index * 3 + 4, "reading on"));
    }

    let segment = summarize(&messages);
    let rendered = render(&segment);
    let source_bytes: usize = messages.iter().map(|message| message.text().len()).sum();

    assert!(
        rendered.len() * 10 < source_bytes.max(1),
        "rendered {} bytes against {} bytes of message text",
        rendered.len(),
        source_bytes
    );
    assert!(
        rendered.lines().count() <= 12,
        "render must stay a short block"
    );
}

#[test]
fn render_of_a_single_turn_fits_on_a_few_lines() {
    let rendered = render(&summarize(&[user(1, "hello"), assistant(2, "hi")]));

    assert!(
        rendered.lines().count() <= 5,
        "a trivial turn must not produce a long block:\n{rendered}"
    );
    assert!(
        !rendered.ends_with('\n'),
        "no trailing newline, so it can be embedded"
    );
}

// ---- empty and degenerate input ---------------------------------------------------------------

#[test]
fn an_empty_message_list_summarizes_to_an_empty_segment() {
    let segment = summarize(&[]);

    assert_eq!(segment.turns, 0..0);
    assert!(segment.requests.is_empty());
    assert!(segment.tools.is_empty());
    assert!(segment.files.is_empty());
    assert_eq!(segment.steps, 0);
    assert_eq!(render(&segment), "", "nothing to say means saying nothing");
}

#[test]
fn a_segment_with_no_user_message_reports_no_turns_but_keeps_its_work() {
    // This happens after a compaction whose span began mid-turn: the assistant and tool messages
    // survive, the request that opened the turn does not.
    let messages = vec![
        assistant(1, "continuing"),
        call(2, "read", r#"{"path":"src/lib.rs"}"#),
        result(3, "…"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.turns, 0..0, "no request, so no attributable turn");
    assert!(segment.requests.is_empty());
    assert_eq!(segment.tools, vec!["read"]);
    assert_eq!(segment.files, vec!["src/lib.rs"]);
    assert_eq!(
        segment.steps, 2,
        "the work still happened and is still reported"
    );

    let rendered = render(&segment);
    assert!(
        rendered.contains("no turn boundary"),
        "state the absence:\n{rendered}"
    );
    assert!(
        rendered.contains("src/lib.rs"),
        "the work is still worth reporting:\n{rendered}"
    );
}

#[test]
fn a_blank_user_message_is_not_recorded_as_a_request() {
    let messages = vec![
        user(1, "   \n\t "),
        user(2, "real request"),
        assistant(3, "ok"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.requests, vec!["real request"]);
    assert_eq!(segment.turns, 1..2, "a blank message opens no turn");
}

#[test]
fn a_prior_checkpoint_is_not_replayed_as_a_request() {
    // Copying it forward would duplicate the previous summary and pad this one with its preamble.
    let messages = vec![
        checkpoint(1, "## Primary Request and Intent\n- earlier work"),
        user(2, "now do the next thing"),
        assistant(3, "ok"),
    ];

    let segment = summarize(&messages);

    assert_eq!(segment.requests, vec!["now do the next thing"]);
    assert_eq!(segment.turns, 1..2);
}

#[test]
fn a_user_message_that_merely_mentions_one_tag_is_still_a_request() {
    // The checkpoint shape needs both tags; a person talking about the format is not a checkpoint.
    let text = "why does the code look for <compacted-summary> here?";
    assert!(
        !text.contains(SUMMARY_CLOSE_TAG),
        "the fixture must not hold both tags"
    );

    let segment = summarize(&[user(1, text)]);

    assert_eq!(segment.requests, vec![text]);
}
