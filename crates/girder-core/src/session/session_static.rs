//! A whole session as one static, self-contained HTML page.
//!
//! # Why a page, when there is already an export
//!
//! `session.export` answers Markdown or JSON. Markdown is a transcript for a document or a ticket,
//! and JSON is the log for a program that replays it; neither is a page a person can open from a
//! link and read. This module fills that gap: one string, one file, no build step, no server-side
//! rendering, no template engine — and therefore nothing that can be missing when the link is
//! opened.
//!
//! # Escaping is the security boundary
//!
//! A session carries text the harness did not write: the person's prompts, the model's answers, tool
//! arguments and tool output. All of it is attacker-controlled in the threat model that matters
//! here, because a shared link is handed to someone who did not author the session. A transcript
//! containing `<script>alert(1)</script>` must therefore render as those visible characters.
//!
//! Two rules follow, and both are load-bearing:
//!
//! - Every user-derived string goes through [`escape`] before it is concatenated into the page.
//!   Markup tags are emitted from constants in this file, never from session content.
//! - No text from the session is ever placed where a browser does not treat it as escaped text:
//!   there is no `<script>`, no event-handler attribute, no `javascript:` URL, and no `style`
//!   attribute. `class` and `aria-label` are the only attributes written, and `aria-label` holds
//!   escaped text.
//!
//! The escaping covers all five characters that are special in HTML — `<`, `>`, `&`, `"`, `'` — so
//! the same function is safe in element content *and* inside a quoted attribute value. An escaper
//! that only handled `<` and `>` would be correct in the first position and broken in the second:
//! an attribute value ends at the unescaped quote, and everything after it becomes markup.
//!
//! A Content-Security-Policy of `default-src 'none'` is emitted as a second layer. It does not
//! replace escaping — a policy is a browser feature, and the file may be read by something that
//! ignores it — it is what turns a hypothetical escaping bug into a blank page instead of a stolen
//! session.
//!
//! # Why nothing is loaded from elsewhere
//!
//! A shared page is opened later, by someone else, possibly with no network. A stylesheet behind a
//! URL would leave that reader with unstyled text, a font behind a URL with a different page, and
//! an image behind a URL with a missing box. So the page carries its own `<style>` block and
//! references no external resource at all: no `src`, no `href`, no `@import`, no `url(...)`.
//!
//! The page also runs no JavaScript, and that is a property rather than an omission: a static page
//! is one whose behaviour is entirely its markup. Script would mean the file's meaning depends on
//! an execution environment that may be a different browser, a sandboxed viewer, or a text editor,
//! and it would reopen exactly the injection surface that escaping closes.
//!
//! # Why the page is cut instead of clipped
//!
//! A session log has no natural size: one tool result may hold an entire file. Returning it whole
//! would make the page as large as the session, so content is cut at [`MAX_PAGE_BYTES`].
//!
//! The cut happens on the *content* while the shell is still being assembled, not on the finished
//! HTML string. Truncating the finished string would cut a tag, an attribute, or an escaped entity
//! in half, and every browser repairs those differently — the one outcome worse than a short page
//! is a page whose structure is a guess. Cutting content keeps every tag balanced, and the notice
//! that says the page was cut is appended after it, so a reader is told what happened instead of
//! wondering why the transcript stops mid-sentence.
//!
//! The cut is also taken on a character boundary, never at a raw byte offset: half a UTF-8 sequence
//! in the middle of a word reads as corrupt data rather than as a truncation.
//!
//! # What the page leaves out
//!
//! System prompts, model selection, usage, retries, and sandbox settings describe how a turn ran,
//! not what was said, and reasoning blocks are the model's private working. None of them are shown,
//! matching the readable export: a page exists to be read as a conversation, and per-step
//! bookkeeping would drown it. Tool calls and results are paired by call id so a result can be
//! headed with the tool that produced it.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use girder_core::{ChatMessage, ContentBlock, SessionEvent};

/// The largest page this module produces, in bytes.
///
/// One mebibyte is a long conversation wrapped in a small shell: large enough that ordinary
/// sessions render whole, small enough to mail, paste, or open from a link without the reader
/// waiting on a download. It bounds the *page*, not the session: anything longer is cut and said to
/// be cut.
pub const MAX_PAGE_BYTES: usize = 1024 * 1024;

/// The document shell before the transcript: doctype, metadata, inline style, and the heading.
///
/// Written as a function rather than a constant because the title is interpolated, and it is
/// interpolated escaped — the title is as untrusted as the messages.
fn head(title: &str) -> String {
    let title = escape(title);
    let mut out = String::with_capacity(head_len_hint(&title));
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    out.push_str("<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    // Second layer only; see the module docs. `style-src 'unsafe-inline'` allows the inline block
    // below, and `default-src 'none'` denies everything the page does not need.
    let _ = writeln!(
        out,
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">"
    );
    let _ = writeln!(out, "<title>{title}</title>");
    let _ = writeln!(out, "<style>{STYLE}</style>");
    out.push_str("</head>\n<body>\n<main>\n");
    let _ = writeln!(out, "<h1>{title}</h1>");
    out
}

/// A capacity guess for [`head`], so assembling a page does not reallocate per line.
///
/// An over-estimate is harmless: `String` grows as needed and the guess is never a bound anything
/// relies on.
fn head_len_hint(escaped_title: &str) -> usize {
    STYLE.len() + CSP.len() + escaped_title.len() * 2 + 512
}

/// The document shell after the transcript.
const FOOT: &str = "</main>\n</body>\n</html>\n";

/// The policy emitted in the document metadata.
///
/// `default-src 'none'` denies scripts, frames, and every network fetch; `style-src 'unsafe-inline'`
/// is what permits the inline style block, and inline styles are the only thing this page uses.
/// `base-uri 'none'` and `form-action 'none'` close the two remaining ways markup could be made to
/// navigate somewhere, even though this page emits neither a `<base>` nor a form.
const CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'";

/// The inline stylesheet: the page must render the same with no network and no build step.
///
/// No `url(...)`, no `@import`, and no external font: a page that failed to style itself offline
/// would be exactly the broken link this module exists to avoid.
const STYLE: &str = concat!(
    ":root{color-scheme:light dark}",
    "body{margin:0;font:16px/1.5 -apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;",
    "background:#f6f6f7;color:#1c1c1e}",
    "main{max-width:52rem;margin:0 auto;padding:2rem 1rem 4rem}",
    "h1{font-size:1.35rem;line-height:1.3;margin:0 0 1.5rem;word-break:break-word}",
    "section{background:#fff;border:1px solid #e2e2e6;border-radius:8px;",
    "padding:.75rem 1rem;margin:0 0 1rem}",
    "section.turn{background:transparent;border:0;border-radius:0;padding:1.5rem 0 .25rem}",
    "section.turn h2{font-size:.75rem;text-transform:uppercase;letter-spacing:.08em;color:#6b6b70}",
    "section.user{border-left:4px solid #3b6cf6}",
    "section.assistant{border-left:4px solid #2f9e6b}",
    "section.developer{border-left:4px solid #b07d1a}",
    "section.tool{border-left:4px solid #8a8a90}",
    "section.tool.error{border-left:4px solid #c0392b}",
    "h2{font-size:.8rem;margin:0 0 .4rem;color:#4a4a50;font-weight:600}",
    "pre{margin:0;white-space:pre-wrap;overflow-wrap:anywhere;",
    "font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}",
    "p.empty{color:#6b6b70}",
    "p.truncated{margin:1rem 0 0;padding:.6rem .8rem;border:1px dashed #c0392b;",
    "border-radius:6px;color:#8a2b20}",
    "@media (prefers-color-scheme:dark){",
    "body{background:#141416;color:#e8e8ea}",
    "section{background:#1d1d20;border-color:#2f2f34}",
    "h2{color:#b6b6bc}p.empty{color:#9a9aa2}p.truncated{color:#f0a094;border-color:#8a2b20}",
    "}"
);

/// The sentence shown when a session holds nothing to render.
///
/// A page for an empty session is still a page: reporting an error would claim the session was
/// missing, and a blank document would look like a failed render.
const EMPTY_NOTICE: &str = "<p class=\"empty\">This session has no messages.</p>\n";

/// The sentence shown when the page was cut, and the exact number of bytes reserved for it.
///
/// Fixed text, so its size is known and can be subtracted from the budget before any content is
/// written. That is what keeps the final page inside [`MAX_PAGE_BYTES`] even after the notice is
/// added to a page that already used its whole budget.
const TRUNCATION_NOTICE: &str = concat!(
    "<p class=\"truncated\">This page was cut at its size limit. ",
    "The rest of the session was omitted.</p>\n"
);

/// A page that is ready to write to a file and serve as `text/html`.
///
/// The string is the deliverable; `bytes` and `truncated` restate facts a caller would otherwise
/// recompute or infer, and inferring truncation from a length is not possible in general — a page
/// that ended exactly at the limit is indistinguishable from one that was cut at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The complete document, doctype included.
    pub html: String,
    /// The length of [`Page::html`] in bytes.
    pub bytes: usize,
    /// Whether any of the session was left out to fit [`MAX_PAGE_BYTES`].
    pub truncated: bool,
}

/// Renders a session as one self-contained HTML page.
///
/// `title` is shown in the browser tab and as the page heading; it is escaped like any other
/// untrusted text, because a session title is model- or user-generated and may contain markup.
///
/// The result is pure: no I/O, no clock, no randomness, so the same events and title always produce
/// the same bytes and a test can compare them.
pub fn render(events: &[SessionEvent], title: &str) -> Page {
    let head = head(title);
    // Reserve the notice unconditionally. A page that ends up needing it then costs nothing extra,
    // and a page that does not simply leaves the reserved bytes unused.
    let budget = MAX_PAGE_BYTES.saturating_sub(head.len() + FOOT.len() + TRUNCATION_NOTICE.len());
    let sections = sections(events);
    let (transcript, truncated) = fill(&sections, budget);

    let mut html = head;
    if sections.is_empty() {
        html.push_str(EMPTY_NOTICE);
    }
    html.push_str(&transcript);
    if truncated {
        html.push_str(TRUNCATION_NOTICE);
    }
    html.push_str(FOOT);

    Page {
        bytes: html.len(),
        html,
        truncated,
    }
}

/// Escapes the five characters that are special in HTML.
///
/// Returns text that is safe both as element content and as a quoted attribute value, which is why
/// both quote characters are escaped and not only the one this module happens to use. Escaping
/// `'` costs two bytes in the common case of an apostrophe; an escaper whose safety depends on the
/// caller's choice of quote character is one refactor away from an injection.
///
/// `&` is replaced first by construction — the replacement strings are appended, never rescanned —
/// so an input `&lt;` becomes `&amp;lt;` and displays as the literal characters it is, which is what
/// a reader of a transcript expects.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        push_escaped(&mut out, ch);
    }
    out
}

/// Appends one escaped character, so a caller can escape into an existing buffer.
///
/// A `char` at a time rather than `str::replace` chains: five passes would reallocate and rescan the
/// whole string five times, and each pass would have to avoid escaping the entities the previous
/// one produced.
fn push_escaped(out: &mut String, ch: char) {
    match ch {
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '&' => out.push_str("&amp;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&#x27;"),
        other => out.push(other),
    }
}

/// How many bytes [`push_escaped`] appends for `ch`.
///
/// Needed by the truncation path, which has to know the escaped size of a character *before* writing
/// it: a budget measured in characters would overshoot for every `&` and quote in the transcript.
const fn escaped_len(ch: char) -> usize {
    match ch {
        '<' | '>' => 4,
        '&' => 5,
        '"' | '\'' => 6,
        other => other.len_utf8(),
    }
}

/// Escapes as much of `text` as fits in `room` bytes, cutting on a character boundary.
///
/// Used only to fill the last, partial section: the output is a prefix of `escape(text)`, so it is
/// safe in exactly the same positions.
fn escaped_prefix(text: &str, room: usize) -> String {
    let mut out = String::with_capacity(room.min(text.len()));
    let mut used = 0;
    for ch in text.chars() {
        let len = escaped_len(ch);
        if used + len > room {
            break;
        }
        used += len;
        push_escaped(&mut out, ch);
    }
    out
}

/// One block of the transcript: a heading and the text under it.
///
/// `class` is a CSS class chosen here, never taken from the session, which is why it can be written
/// into the `class` attribute unescaped. `label` and `text` are raw session text and are escaped at
/// render time.
struct Section {
    /// The CSS class selecting this section's appearance.
    class: &'static str,
    /// The heading, unescaped.
    label: String,
    /// The body text, unescaped. Rendered inside a `<pre>` so line breaks survive.
    text: String,
}

/// Renders every section, stopping when `budget` bytes of markup would be exceeded.
///
/// Returns the markup and whether anything was left out. The last section is filled with as much of
/// its text as fits rather than dropped whole: a section cut mid-sentence is honest about the limit,
/// while dropping it silently would look like the session ended there.
fn fill(sections: &[Section], budget: usize) -> (String, bool) {
    let mut out = String::new();
    for section in sections {
        let piece = section_html(section);
        if out.len() + piece.len() <= budget {
            out.push_str(&piece);
            continue;
        }
        let remaining = budget.saturating_sub(out.len());
        out.push_str(&section_html_cut(section, remaining));
        return (out, true);
    }
    (out, false)
}

/// Renders one section in full.
fn section_html(section: &Section) -> String {
    section_html_escaped(section, &escape(&section.text))
}

/// Renders one section around text that is already escaped.
///
/// The split exists for the truncation path, which builds an escaped prefix itself and must not
/// escape it a second time.
fn section_html_escaped(section: &Section, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + section.label.len() + 128);
    // `class` comes from `Section::class`, a constant in this file. `aria-label` repeats the heading
    // for a screen reader, and it is the one attribute carrying session text — escaped, so a quote
    // in a tool name cannot end the value early.
    let _ = write!(
        out,
        "<section class=\"{}\" aria-label=\"{}\">\n<h2>{}</h2>\n",
        section.class,
        escape(&section.label),
        escape(&section.label),
    );
    if !body.is_empty() {
        let _ = writeln!(out, "<pre>{body}</pre>");
    }
    out.push_str("</section>\n");
    out
}

/// Renders one section with at most `budget` bytes of markup.
///
/// The markup is measured with a one-byte body so the `<pre>` wrapper is included in the count;
/// subtracting that byte leaves the room the escaped text may occupy. If even the wrapper does not
/// fit, nothing is emitted — the caller reports truncation, and a half-written tag would corrupt the
/// document instead.
fn section_html_cut(section: &Section, budget: usize) -> String {
    let overhead = section_html_escaped(section, "x").len().saturating_sub(1);
    if budget <= overhead {
        return String::new();
    }
    let body = escaped_prefix(&section.text, budget - overhead);
    section_html_escaped(section, &body)
}

/// Turns events into the sections a reader sees.
///
/// Boundary events become their own heading; `step/start`, `step/end`, `model/usage`, and the
/// settings records produce nothing, because a page shows the conversation rather than the
/// scheduler's bookkeeping (see the module docs). A message with no visible text is skipped for the
/// same reason: an assistant step that only requested tools would otherwise render an empty box.
fn sections(events: &[SessionEvent]) -> Vec<Section> {
    let mut out = Vec::new();
    // Call id to tool name, so a result can be headed with the tool that produced it. A result on
    // its own does not say which tool answered, and a heading named after a call id is unreadable.
    let mut tool_names: BTreeMap<String, String> = BTreeMap::new();

    for event in events {
        match event {
            SessionEvent::TurnStart { turn } => out.push(Section {
                class: "turn",
                label: format!("Turn {turn}"),
                text: String::new(),
            }),
            SessionEvent::UserMessage { message } => {
                message_section(&mut out, "user", "User", message);
            }
            SessionEvent::AssistantMessage { message } => {
                message_section(&mut out, "assistant", "Assistant", message);
            }
            SessionEvent::DeveloperMessage { message, .. } => {
                message_section(&mut out, "developer", "Developer", message);
            }
            SessionEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                tool_names.insert(id.to_string(), name.clone());
                out.push(Section {
                    class: "tool",
                    label: format!("Tool call: {name}"),
                    text: arguments.clone(),
                });
            }
            SessionEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                let call = id.to_string();
                let name = tool_names
                    .get(&call)
                    .cloned()
                    .unwrap_or_else(|| call.clone());
                let (class, label) = if *is_error {
                    ("tool error", format!("Tool result: {name} (failed)"))
                } else {
                    ("tool", format!("Tool result: {name}"))
                };
                out.push(Section {
                    class,
                    label,
                    text: blocks_text(content),
                });
            }
            // Everything else is bookkeeping a reader does not need; see the module docs.
            _ => {}
        }
    }

    out
}

/// Pushes a message section, unless the message has no visible text.
fn message_section(
    out: &mut Vec<Section>,
    class: &'static str,
    label: &str,
    message: &ChatMessage,
) {
    let text = message.text();
    if text.trim().is_empty() {
        return;
    }
    out.push(Section {
        class,
        label: label.to_owned(),
        text,
    });
}

/// Flattens content blocks into the text a reader sees.
///
/// Images and files are rendered as labels rather than bytes: the bytes live in the attachment
/// service, and a page that inlined them would neither be portable nor stay small. Reasoning and
/// embedded tool calls are skipped because neither was said to the reader, and the tool call is
/// already its own event.
fn blocks_text(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::Image { attachment } => {
                let _ = write!(out, "[image attachment: {attachment}]");
            }
            ContentBlock::File { attachment } => {
                let _ = write!(out, "[file attachment: {attachment}]");
            }
            ContentBlock::Reasoning { .. } | ContentBlock::ToolCall { .. } => {}
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "(no text output)".to_owned()
    } else {
        trimmed.to_owned()
    }
}
