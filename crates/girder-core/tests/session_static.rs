//! Tests for the static HTML session page.
//!
//! The properties under test are the ones a shared link depends on, in the order they matter:
//!
//! - **session content cannot execute** — this is the reason the module exists. A transcript holding
//!   `<script>alert(1)</script>` must render those characters, not run them, in message text, in a
//!   tool name, in tool arguments, and in the title. Markdown is readable by a person and JSON is
//!   readable by a program; HTML is the one format a browser executes, so it is the one where an
//!   escaping mistake is a vulnerability rather than a rendering flaw.
//! - **quotes are safe in attributes** — escaping only `<` and `>` is correct in element content and
//!   broken in an attribute value, where the unescaped quote ends the value and makes everything
//!   after it markup. The page writes one attribute carrying session text, so the test drives a
//!   quote into a tool name and asserts the attribute cannot be closed early.
//! - **the page is self-contained** — a share is opened later, by someone else, possibly offline. No
//!   `src`, `href`, `@import`, or `url(...)` may appear, or the link is broken where it matters.
//! - **the page runs no script** — static means static; a `<script>` tag in the output would make
//!   the file's meaning depend on an execution environment.
//! - **truncation is bounded and labelled** — a session has no natural size, so the page is cut at a
//!   limit and says so. Cuts happen on content, so no tag is ever left half-written.
//! - **an empty session still renders** — a session that was created and never used is a normal
//!   state, and a page that reported an error would claim the session was missing.
//! - **the page is deterministic** — same events, same bytes, so a test can compare them at all.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written before the crate root wires it in. It spells its paths through
//! `girder_core::`, the alias the library declares for itself, so the same source builds here and
//! inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/session_static.rs"]
mod session_static;

use girder_core::{ContentBlock, MessageId, SessionEvent, ToolCallId};
use session_static::{MAX_PAGE_BYTES, Page, escape, render};

// ---- fixtures ---------------------------------------------------------------------------------

/// A tool call event with a caller-chosen name and arguments.
fn tool_call(id: &str, name: &str, arguments: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        id: ToolCallId::new(id).expect("valid call id"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

/// A tool result event answering `id`.
fn tool_result(id: &str, text: &str, is_error: bool) -> SessionEvent {
    SessionEvent::ToolResult {
        id: ToolCallId::new(id).expect("valid call id"),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        is_error,
    }
}

/// One ordinary turn: a prompt, an answer, and a tool call with its result.
fn conversation() -> Vec<SessionEvent> {
    vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::user_text("m1", "How many files are here?"),
        SessionEvent::assistant_text("m2", "Let me look.", "deepseek"),
        tool_call("call-1", "list_files", "{\"path\":\".\"}"),
        tool_result("call-1", "a.txt\nb.txt", false),
        SessionEvent::assistant_text("m3", "There are two files.", "deepseek"),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: girder_core::TurnEndReason::Completed,
        },
    ]
}

// ---- the document -----------------------------------------------------------------------------

#[test]
fn renders_a_complete_document() {
    let page = render(&conversation(), "Two files");
    let html = &page.html;

    assert!(
        html.starts_with("<!DOCTYPE html>\n"),
        "the page declares its doctype: {html}"
    );
    assert!(html.contains("<html lang=\"en\">"), "the document element");
    assert!(html.contains("<meta charset=\"utf-8\">"), "the charset");
    assert!(
        html.ends_with("</main>\n</body>\n</html>\n"),
        "the document is closed: {}",
        html.ends_with("</html>\n")
    );
    assert_eq!(page.bytes, page.html.len(), "`bytes` counts the string");
    assert!(!page.truncated, "an ordinary session fits the limit");
}

#[test]
fn renders_the_conversation_it_was_given() {
    let page = render(&conversation(), "Two files");
    let html = &page.html;

    assert!(
        html.contains("<h1>Two files</h1>"),
        "the title is the page heading"
    );
    assert!(
        html.contains("<section class=\"turn\" aria-label=\"Turn 1\">\n<h2>Turn 1</h2>"),
        "a turn boundary becomes its own heading"
    );
    assert!(
        html.contains("<h2>User</h2>"),
        "the prompt is attributed to a speaker"
    );
    assert!(
        html.contains("<pre>How many files are here?</pre>"),
        "the prompt text is in the body"
    );
    assert!(
        html.contains("<h2>Assistant</h2>"),
        "the answer is attributed to a speaker"
    );
    assert!(
        html.contains("<pre>There are two files.</pre>"),
        "the answer text is in the body"
    );
    assert!(
        html.contains("<h2>Tool call: list_files</h2>"),
        "a tool call is headed by the tool name, not the call id"
    );
    assert!(
        html.contains("<h2>Tool result: list_files</h2>"),
        "the result is paired with the call that produced it"
    );
    assert!(
        html.contains("<pre>a.txt\nb.txt</pre>"),
        "tool output keeps its line breaks"
    );
}

#[test]
fn marks_a_failed_tool_result() {
    let events = vec![
        tool_call("call-1", "read_file", "{}"),
        tool_result("call-1", "permission denied", true),
    ];
    let html = render(&events, "Failure").html;

    assert!(
        html.contains("<h2>Tool result: read_file (failed)</h2>"),
        "a failed result is labelled as one: {html}"
    );
    assert!(
        html.contains("class=\"tool error\""),
        "and carries the error class, so it is visible without reading the label"
    );
}

#[test]
fn an_empty_session_still_renders_a_page() {
    let page = render(&[], "Empty");

    assert!(
        !page.truncated,
        "nothing was cut, because nothing was there"
    );
    assert_eq!(page.bytes, page.html.len());
    assert!(
        page.html.contains("<!DOCTYPE html>"),
        "an empty session is a page, not an error"
    );
    assert!(
        page.html.contains("This session has no messages."),
        "the reader is told the session was empty: {}",
        page.html
    );
}

#[test]
fn rendering_is_deterministic() {
    let first = render(&conversation(), "Same");
    let second = render(&conversation(), "Same");
    assert_eq!(first, second, "the same events must produce the same page");
}

// ---- escaping: the security boundary ----------------------------------------------------------

#[test]
fn escape_covers_all_five_html_characters() {
    assert_eq!(
        escape("<>&\"'"),
        "&lt;&gt;&amp;&quot;&#x27;",
        "every character that is special in HTML is replaced"
    );
    assert_eq!(escape("plain ASCII"), "plain ASCII");
    assert_eq!(escape(""), "");
    assert_eq!(
        escape("café — 日本語"),
        "café — 日本語",
        "non-special text passes through unchanged"
    );
}

#[test]
fn an_ampersand_is_not_double_escaped() {
    // A transcript routinely contains `&lt;` as literal text — a model quoting HTML. Rescanned
    // replacement passes turn it into a tag again, so the entity in the input must survive as
    // visible characters.
    assert_eq!(escape("&lt;"), "&amp;lt;");
    assert_eq!(escape("a & b"), "a &amp; b");
}

#[test]
fn a_script_tag_in_a_message_does_not_become_markup() {
    let events = vec![SessionEvent::user_text("m1", "<script>alert(1)</script>")];
    let page = render(&events, "Injection");
    let html = &page.html;

    assert!(
        !html.contains("<script>"),
        "no script element may reach the page: {html}"
    );
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "the tag is visible as text, which is what the reader is owed: {html}"
    );
}

#[test]
fn a_script_tag_in_a_title_is_escaped_too() {
    let page = render(&[], "<script>alert(1)</script>");
    let html = &page.html;

    assert!(
        !html.contains("<script>"),
        "the title is session-derived and equally untrusted: {html}"
    );
    assert!(
        html.contains("<title>&lt;script&gt;alert(1)&lt;/script&gt;</title>"),
        "the title is escaped in the document metadata: {html}"
    );
    assert!(
        html.contains("<h1>&lt;script&gt;alert(1)&lt;/script&gt;</h1>"),
        "and in the heading"
    );
}

#[test]
fn a_script_tag_in_tool_output_and_arguments_is_escaped() {
    let events = vec![
        tool_call("call-1", "run", "{\"code\":\"<script>alert(1)</script>\"}"),
        tool_result("call-1", "<img src=x onerror=alert(1)>", false),
    ];
    let html = render(&events, "Tools").html;

    assert!(!html.contains("<script>"), "tool arguments are escaped");
    assert!(!html.contains("<img"), "tool output is escaped");
    assert!(
        html.contains("&lt;img src=x onerror=alert(1)&gt;"),
        "the attempted tag is shown as text: {html}"
    );
}

#[test]
fn attribute_values_cannot_be_closed_early() {
    // The label is the one attribute holding session text. A tool name that tries to break out of
    // it must not gain a new attribute, so the quote has to arrive as `&quot;` and the rest of the
    // attempted handler as inert text.
    let name = "x\" onmouseover=\"alert(1)\"";
    let events = vec![tool_call("call-1", name, "{}")];
    let html = render(&events, "Attributes").html;

    assert!(
        !html.contains("onmouseover=\"alert(1)\""),
        "the quote cannot end the attribute value: {html}"
    );
    assert!(
        html.contains("onmouseover=&quot;alert(1)&quot;"),
        "the attempted handler is inert text inside the value: {html}"
    );
    assert!(
        html.contains("aria-label=\"Tool call: x&quot; onmouseover=&quot;alert(1)&quot;\""),
        "the attribute is properly delimited: {html}"
    );
    // The same text appears in the `<h2>`, where it must also be escaped rather than parsed.
    assert!(
        html.contains("<h2>Tool call: x&quot; onmouseover=&quot;alert(1)&quot;</h2>"),
        "the heading escapes independently of the attribute"
    );
}

#[test]
fn a_single_quote_cannot_break_a_style_attribute() {
    // The page writes no style attribute, and this pins that: a `'` reaching an attribute would be
    // an injection if the template ever moved to single-quoted attributes.
    let events = vec![SessionEvent::user_text("m1", "' onload='alert(1)")];
    let html = render(&events, "Quotes").html;

    assert!(
        !html.contains("onload='alert(1)'"),
        "a single quote is escaped, so it cannot delimit anything"
    );
    assert!(
        html.contains("&#x27; onload=&#x27;alert(1)"),
        "the text is inert: {html}"
    );
}

// ---- static: no external resources, no script -------------------------------------------------

#[test]
fn the_page_references_no_external_resource() {
    let html = render(&conversation(), "Self contained").html;

    for forbidden in [
        "src=", "href=", "@import", "url(", "http://", "https://", "<script", "<iframe", "<object",
        "<link", "<img",
    ] {
        assert!(
            !html.contains(forbidden),
            "a self-contained page must not contain `{forbidden}`: {html}"
        );
    }
}

#[test]
fn the_page_carries_its_own_stylesheet() {
    let html = render(&conversation(), "Styled").html;

    assert!(html.contains("<style>"), "the CSS is inline");
    assert!(
        html.contains("max-width:52rem"),
        "the stylesheet is the page's own, not a placeholder"
    );
    assert!(
        html.contains("prefers-color-scheme:dark"),
        "the stylesheet adapts to a dark reader without loading anything"
    );
}

#[test]
fn the_page_emits_no_script_language() {
    let html = render(&conversation(), "No script").html;

    for forbidden in ["<script", "javascript:", "onerror=", "onload=", "onclick="] {
        assert!(
            !html.contains(forbidden),
            "a static page runs nothing: found `{forbidden}`"
        );
    }
}

#[test]
fn the_page_declares_a_restrictive_content_security_policy() {
    let html = render(&conversation(), "Policy").html;

    assert!(
        html.contains("default-src 'none'"),
        "the policy denies everything the page does not need: {html}"
    );
    assert!(
        html.contains("style-src 'unsafe-inline'"),
        "while still permitting its own inline stylesheet"
    );
}

// ---- truncation -------------------------------------------------------------------------------

#[test]
fn a_large_session_is_cut_and_says_so() {
    // Two mebibytes of text against a one-mebibyte page: the session cannot fit by construction.
    let bulk = "lorem ipsum ".repeat(2 * 1024 * 1024 / 12 + 1);
    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::user_text("m1", bulk),
        SessionEvent::assistant_text("m2", "done", "deepseek"),
    ];
    let page: Page = render(&events, "Long");

    assert!(page.truncated, "the page reports that it was cut");
    assert_eq!(
        page.bytes,
        page.html.len(),
        "`bytes` counts what was returned"
    );
    assert!(
        page.html.len() <= MAX_PAGE_BYTES,
        "the page never exceeds its budget: {} > {MAX_PAGE_BYTES}",
        page.html.len()
    );
    assert!(
        page.html.contains("This page was cut at its size limit."),
        "the reader is told the rest is missing"
    );
    assert!(
        page.html.ends_with("</main>\n</body>\n</html>\n"),
        "the document is still closed, so the cut did not land inside a tag"
    );
    assert!(
        page.html.contains("class=\"truncated\""),
        "the notice is styled like a notice"
    );
}

#[test]
fn a_cut_never_splits_a_character() {
    // Multi-byte characters throughout: a byte-offset cut would leave an invalid sequence, and the
    // page would read as corrupt data rather than as a truncation.
    let bulk = "日本語のテキスト".repeat(200_000);
    let events = vec![SessionEvent::user_text("m1", bulk)];
    let page = render(&events, "Multi byte");

    assert!(page.truncated, "the session is long enough to be cut");
    assert!(
        page.html.len() <= MAX_PAGE_BYTES,
        "still within the budget: {}",
        page.html.len()
    );
    // The page is a `String`, so it is valid UTF-8 by construction; what the test has to rule out is
    // the repair path — a cut inside a sequence would have left a replacement character behind.
    assert!(
        page.html.contains("日本語"),
        "whole characters survive at the cut"
    );
    assert!(
        !page.html.contains('\u{fffd}'),
        "no replacement character: the cut was on a boundary"
    );
}

#[test]
fn a_page_just_under_the_limit_is_not_truncated() {
    // A small session must not claim to be cut: the flag says what happened, and a false positive
    // would teach readers to ignore it.
    let page = render(&conversation(), "Small");
    assert!(!page.truncated);
    assert!(page.html.len() < MAX_PAGE_BYTES);
}

// ---- message shapes ---------------------------------------------------------------------------

#[test]
fn a_message_with_only_a_tool_call_renders_no_empty_box() {
    let message = girder_core::ChatMessage::new(
        MessageId::new("m1").expect("valid id"),
        girder_core::ChatRole::Assistant,
        girder_core::MessageSource::Model {
            provider: "deepseek".to_owned(),
            model: "deepseek-chat".to_owned(),
            replay_state: None,
        },
        vec![ContentBlock::ToolCall {
            id: ToolCallId::new("call-1").expect("valid id"),
            name: "list_files".to_owned(),
            arguments: "{}".to_owned(),
        }],
    );
    let events = vec![SessionEvent::AssistantMessage { message }];
    let html = render(&events, "Silent step").html;

    assert!(
        !html.contains("<h2>Assistant</h2>"),
        "a step that only requested a tool said nothing, so it gets no heading: {html}"
    );
}

#[test]
fn reasoning_is_not_shown() {
    let message = girder_core::ChatMessage::new(
        MessageId::new("m1").expect("valid id"),
        girder_core::ChatRole::Assistant,
        girder_core::MessageSource::Model {
            provider: "deepseek".to_owned(),
            model: "deepseek-chat".to_owned(),
            replay_state: None,
        },
        vec![
            ContentBlock::Reasoning {
                text: "private working".to_owned(),
            },
            ContentBlock::Text {
                text: "visible answer".to_owned(),
            },
        ],
    );
    let events = vec![SessionEvent::AssistantMessage { message }];
    let html = render(&events, "Reasoning").html;

    assert!(
        !html.contains("private working"),
        "reasoning is the model's private working, not something it said: {html}"
    );
    assert!(
        html.contains("visible answer"),
        "the answer itself is shown"
    );
}

#[test]
fn settings_events_are_not_rendered() {
    let events = vec![
        SessionEvent::ModelSelection {
            selection: girder_core::ModelSelection {
                provider: "deepseek".to_owned(),
                model: "deepseek-chat".to_owned(),
                reasoning_effort: None,
            },
        },
        SessionEvent::SandboxMode {
            mode: "workspace-write".to_owned(),
            source: None,
        },
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 10,
            output_tokens: 20,
        },
    ];
    let html = render(&events, "Bookkeeping").html;

    assert!(
        !html.contains("workspace-write"),
        "sandbox settings describe how a turn ran, not what was said: {html}"
    );
    assert!(!html.contains("deepseek-chat"), "so do usage and model");
    assert!(
        html.contains("This session has no messages."),
        "with nothing left, the page is the empty one"
    );
}
