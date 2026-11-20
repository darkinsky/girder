//! End-to-end round-trip verification for every session format this crate writes.
//!
//! Four formats leave a session and two of them can come back, so "does it round-trip" is not one
//! question but five, each with a different *contract*:
//!
//! | format | what a round trip is allowed to preserve |
//! |---|---|
//! | [`girder_core::session::export_bundle`] (own transport) | every event, field for field, in order |
//! | [`girder_core::export_transcript`] (`OpenAI`) | the conversation text of the user and assistant turns |
//! | [`girder_core::export_transcript`] (`Anthropic`) | the same text, with the prompt lifted to a top-level field |
//! | [`girder_core::compact_log`] | every derived view (transcript, digest) of the same log |
//! | [`girder_core::parse_diff_patch`] | the exact text a patch was made against |
//! | [`girder_core::export_manifest`] | a total that agrees with the entries it declares |
//!
//! The point of testing them together is that the lossy ones must be *knowably* lossy. The portable
//! bundle is a transport format and is asserted to be lossless; the two chat vocabularies are
//! deliberate projections and are asserted to preserve exactly the part their documentation claims,
//! so a regression that started dropping conversation text would fail here rather than pass silently
//! under a weaker "the import returned Ok".
//!
//! Every assertion compares concrete content. "It parsed" is not a round trip: an importer that
//! returned an empty transcript would satisfy that and lose the entire session.
//!
//! # Why these are reached through the crate root
//!
//! Unlike the per-module tests beside this file, these go through the public re-exports
//! (`girder_core::session::export_bundle`, `girder_core::import_bundle`, …) on purpose. That is the surface a
//! consumer actually calls, and it is where a mis-wired re-export would hide. Including the modules
//! by path would test the implementations while bypassing the wiring that makes them reachable.
//!
//! # Finds reported, not repaired
//!
//! Three lossy round trips were found while writing this file and are pinned by the tests named
//! `*_loses_*`, with a comment stating what is lost. They are properties of the design as
//! documented, not defects introduced here, and the production code was deliberately left alone:
//! a test that silently "fixed" them would be claiming a contract the modules do not offer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

use std::fmt::Write as _;

use girder_core::{
    ChatMessage, ChatRole, ContentBlock, DEFAULT_CONTEXT, LineDiff, MessageId, MessageSource,
    ModelSelection, SessionEvent, SessionTranscript, ToolCallId, TranscriptFormat, TurnEndReason,
    apply_diff_patch, compact_log, detect_transcript_format, diff_text, digest_session,
    export_bundle, export_manifest, export_transcript, import_bundle, import_transcript,
    is_compactable, parse_diff_patch, transcript_to_json, validate_export_bundle,
};
use serde_json::{Value, json};

// ---- fixtures ---------------------------------------------------------------------------------

/// A message id that is valid by construction.
///
/// `MessageId::new` is fallible because ids are decoded from disk, but every literal in this file is
/// known good; the fallback keeps the fixture total without an `unwrap` at each of a dozen call sites.
fn message_id(value: &str) -> MessageId {
    MessageId::new(value).unwrap_or_else(|_| {
        MessageId::new("msg-fallback").expect("the fallback literal is a valid id")
    })
}

/// A tool-call id that is valid by construction.
fn call_id(value: &str) -> ToolCallId {
    ToolCallId::new(value).unwrap_or_else(|_| {
        ToolCallId::new("call-fallback").expect("the fallback literal is a valid id")
    })
}

/// Builds one message from a role, a source, and text.
fn message(role: ChatRole, source: MessageSource, id: &str, text: &str) -> ChatMessage {
    ChatMessage::new(
        message_id(id),
        role,
        source,
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    )
}

/// A user-authored message.
fn user_message(id: &str, text: &str) -> ChatMessage {
    message(ChatRole::User, MessageSource::User { form: None }, id, text)
}

/// A model-authored message.
///
/// The provider and model are carried through the portable bundle, so they are non-empty invented
/// values rather than placeholders: a round trip that dropped them would still equal a fixture whose
/// fields were all empty strings, which would make the comparison weaker than it looks.
fn assistant_message(id: &str, text: &str) -> ChatMessage {
    message(
        ChatRole::Assistant,
        MessageSource::Model {
            provider: "deepseek".to_owned(),
            model: "deepseek-chat".to_owned(),
            replay_state: Some(json!({ "finish_reason": "stop" })),
        },
        id,
        text,
    )
}

/// The system prompt as it is recorded on the model-visible surface.
fn system_message(id: &str, text: &str) -> ChatMessage {
    message(ChatRole::System, MessageSource::SystemPrompt, id, text)
}

/// A session log holding every event variant once, in a realistic order.
///
/// All seventeen variants are present deliberately. The portable bundle's whole promise is that it
/// carries the vocabulary losslessly, and a fixture that only exercised the conversational events
/// would leave the accounting, configuration, and frame variants — the ones most likely to be
/// dropped by a `match` that forgot an arm — untested.
#[allow(
    clippy::too_many_lines,
    reason = "one fixture holding every event variant is long by construction; splitting it would \
              hide which variant is missing"
)]
fn all_event_types() -> Vec<SessionEvent> {
    let call = call_id("call-1");
    vec![
        // ---- turn 1: prompt, one tool exchange, an answer ----
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::ModelSelection {
            selection: ModelSelection::new("deepseek", "deepseek-chat"),
        },
        SessionEvent::PermissionPreset {
            preset: "workspace-write".to_owned(),
        },
        SessionEvent::PlanMode { active: false },
        SessionEvent::SandboxMode {
            mode: "workspace-write".to_owned(),
            source: Some("setting".to_owned()),
        },
        SessionEvent::RequestContext {
            provider: "deepseek".to_owned(),
            model: "deepseek-chat".to_owned(),
            context_window: Some(64_000),
        },
        SessionEvent::SessionTitle {
            title: "Round-trip fixture".to_owned(),
            message_seqs: vec![1, 2],
            source: "human".to_owned(),
        },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::SystemMessage {
            turn: 1,
            step: 1,
            message: system_message("msg-sys-1", "you are a careful agent"),
        },
        SessionEvent::UserMessage {
            message: user_message("msg-user-1", "read the config"),
        },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::StepStart { turn: 1, step: 2 },
        SessionEvent::AssistantMessage {
            message: assistant_message("msg-asst-1", "I will read it."),
        },
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 120,
            output_tokens: 8,
        },
        SessionEvent::ToolCall {
            id: call.clone(),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"config.toml"}"#.to_owned(),
        },
        SessionEvent::ToolResult {
            id: call,
            content: vec![ContentBlock::Text {
                text: "port = 8080".to_owned(),
            }],
            is_error: false,
        },
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 140,
            output_tokens: 42,
        },
        SessionEvent::AssistantMessage {
            message: assistant_message("msg-asst-2", "The port is 8080."),
        },
        SessionEvent::StepEnd { turn: 1, step: 2 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
        // ---- turn 2: a prompt that changes the system prompt, then a failed turn ----
        SessionEvent::TurnStart { turn: 2 },
        SessionEvent::StepStart { turn: 2, step: 1 },
        SessionEvent::SystemMessage {
            turn: 2,
            step: 1,
            message: system_message("msg-sys-2", "you are a careful agent, now with tools"),
        },
        SessionEvent::LlmRetry {
            attempt: 2,
            reason: Some("rate limited".to_owned()),
        },
        SessionEvent::UserMessage {
            message: user_message("msg-user-2", "now fix it"),
        },
        SessionEvent::DeveloperMessage {
            turn: 2,
            step: 1,
            message: message(
                ChatRole::Developer,
                MessageSource::User { form: None },
                "msg-dev-1",
                "a new tool was added",
            ),
        },
        SessionEvent::AssistantMessage {
            message: assistant_message("msg-asst-3", "I need permission."),
        },
        SessionEvent::StepEnd { turn: 2, step: 1 },
        SessionEvent::TurnEnd {
            turn: 2,
            reason: TurnEndReason::Failed {
                message: "the provider refused the request".to_owned(),
                code: "REFUSED".to_owned(),
            },
        },
    ]
}

/// The user and assistant turns a log carries, as `(role, text)` in the transcript's spelling.
///
/// A tool call and its result are their own events, so they are not messages here. The exported
/// transcript turns them into labelled `user` text instead, which is a separate claim asserted on
/// its own — conflating the two is how a test ends up comparing a projection to a log.
fn log_turns(events: &[SessionEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::UserMessage { message } | SessionEvent::AssistantMessage { message } => {
                Some(message)
            }
            _ => None,
        })
        .map(|message| {
            let role = match message.role {
                ChatRole::Assistant => "assistant",
                _ => "user",
            };
            (role.to_owned(), message.text())
        })
        .collect()
}

/// The turns a transcript carries, in the same `(role, text)` shape as [`log_turns`].
///
/// The `system` message is excluded on purpose: the transcript holds the prompt as a `system`
/// message and the importer turns it into a `developer/message`, so it is outside the conversation
/// by construction. Its placement is asserted separately, where it belongs.
fn transcript_turns(transcript: &SessionTranscript) -> Vec<(String, String)> {
    transcript
        .messages
        .iter()
        .filter(|message| message.role != "system")
        .map(|message| (message.role.clone(), message.content.clone()))
        .collect()
}

/// The turns of a log with the tool annotations removed.
///
/// The transcript rides tool calls and results on the `user` channel as `[tool call] …` /
/// `[tool result] …` text. They are real transcript content, but they are not something a
/// participant said, so the conversation proper is what remains once they are filtered out.
fn turns_without_tool_labels(turns: &[(String, String)]) -> Vec<(String, String)> {
    turns
        .iter()
        .filter(|(_, text)| !text.starts_with("[tool "))
        .cloned()
        .collect()
}

/// The `content` of every message object in a transcript body, as `(role, text)`.
fn json_messages(body: &Value) -> Vec<(String, String)> {
    body["messages"]
        .as_array()
        .expect("a transcript body always holds a `messages` array")
        .iter()
        .map(|message| {
            (
                message["role"]
                    .as_str()
                    .expect("every transcript message declares a string role")
                    .to_owned(),
                message["content"]
                    .as_str()
                    .expect("this module writes every content as a string")
                    .to_owned(),
            )
        })
        .collect()
}

// ---- 1. own transport format ---------------------------------------------------------------

#[test]
fn portable_bundle_round_trips_every_event_field_for_field() {
    // The transport format's contract is that a session restored from it is the same session, so the
    // assertion is full structural equality against the input — not a count, and not "the import
    // returned events". A comparison of type tags would pass while every token count and permission
    // preset came back empty; equality of the whole vector cannot.
    let events = all_event_types();

    let encoded = export_bundle(&events, Some("Round-trip fixture"), 1_700_000_000_000)
        .expect("a log of plain data is representable as JSON");
    let bundle = import_bundle(&encoded).expect("a bundle this build wrote is one it reads");
    let restored = bundle
        .events()
        .expect("every event is one this build wrote");

    assert_eq!(
        restored, events,
        "importing a bundle must reconstruct the original log event for event"
    );
    assert_eq!(
        restored.len(),
        events.len(),
        "no event may be added or dropped in transit"
    );
    assert_eq!(
        bundle.title.as_deref(),
        Some("Round-trip fixture"),
        "the envelope's title is carried separately from the events and must survive too"
    );
    assert_eq!(
        bundle.exported_at, 1_700_000_000_000,
        "the export instant is provenance, but import must not discard it"
    );
}

#[test]
fn portable_bundle_rejects_an_event_it_cannot_reconstruct() {
    // The inverse property, and the reason the bundle exists as a *transport* format: a bundle with a
    // damaged event must be refused whole rather than partially applied. An importer that skipped
    // the bad entry would restore a session with a hole and report success, and the caller would
    // then delete the source it exported from.
    let encoded = export_bundle(&all_event_types(), None, 1).expect("the fixture is representable");
    let mut damaged: Value = serde_json::from_str(&encoded).expect("the bundle is JSON");

    // The user message loses its `message` field: valid JSON, invalid `user/message` event.
    damaged["events"][9] = json!({ "type": "user/message" });

    // The two checks are deliberately separate: `import` asks whether the envelope is one this build
    // understands, and `Bundle::events` asks whether each event can be reconstructed. This bundle's
    // envelope is fine, so the refusal has to come from the second step.
    let bundle = import_bundle(&damaged.to_string())
        .expect("the envelope is still valid: only an event was damaged");
    let error = bundle
        .events()
        .expect_err("a bundle holding an unreconstructable event must be refused whole");
    let rendered = error.to_string();
    assert!(
        rendered.contains('9'),
        "the error must name the offending index so a reader can find it, got: {rendered}"
    );
}

// ---- 2. OpenAI shape -------------------------------------------------------------------------

#[test]
fn openai_round_trip_preserves_user_and_assistant_text() {
    // The lossy formats project a log onto two roles and text. Tool calls become labelled text (so
    // they deliberately do *not* come back as tool calls), but what the person and the model said is
    // the transcript, and losing any of it would make the export a different conversation.
    let events = all_event_types();
    let original = log_turns(&events);
    assert!(
        original.len() >= 5,
        "the fixture must contain several conversational turns, found {}",
        original.len()
    );

    let transcript = export_transcript(&events, TranscriptFormat::OpenAi);
    let body = transcript_to_json(&transcript, TranscriptFormat::OpenAi);
    let imported = import_transcript(&body, TranscriptFormat::OpenAi)
        .expect("a body this build wrote is one it reads");

    // The exact contract: what the transcript says must be what the import reconstructs, turn for
    // turn, in order. Comparing against the transcript rather than against the original log is the
    // difference between testing the round trip and testing the projection twice.
    let restored = log_turns(&imported.events);
    assert_eq!(
        restored,
        transcript_turns(&transcript),
        "the imported conversation must equal the transcript it was read from"
    );
    assert_eq!(
        turns_without_tool_labels(&restored),
        original,
        "every user and assistant message must survive verbatim, in order"
    );
    assert_eq!(
        imported.skipped, 0,
        "no message may be skipped: every role this format writes is one the importer handles"
    );
}

#[test]
fn openai_round_trip_keeps_the_system_prompt_as_a_message() {
    // `OpenAI` has a `system` role *inside* `messages`, which is the one structural difference from
    // `Anthropic`. The prompt records are re-announced per step and unchanged ones are collapsed, so
    // a session that changed its prompt mid-way must still show both texts.
    let events = all_event_types();
    let transcript = export_transcript(&events, TranscriptFormat::OpenAi);
    let body = transcript_to_json(&transcript, TranscriptFormat::OpenAi);

    let roles: Vec<String> = json_messages(&body)
        .into_iter()
        .map(|(role, _)| role)
        .collect();
    assert!(
        roles.contains(&"system".to_owned()),
        "OpenAI keeps the prompt in `messages`, got roles: {roles:?}"
    );
    assert!(
        body.get("system").is_none(),
        "OpenAI has no top-level `system` field; writing one would be the Anthropic shape"
    );

    // The importer translates a foreign prompt into an advisory developer message rather than this
    // harness's own prompt — documented, and asserted here so the choice cannot silently change.
    let imported =
        import_transcript(&body, TranscriptFormat::OpenAi).expect("the body is OpenAI shaped");
    let developer: Vec<String> = imported
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::DeveloperMessage { message, .. } => Some(message.text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        developer,
        // One advisory message, holding both prompts joined with a blank line: the importer has a
        // single slot for a source's instructions, and it fills it in log order rather than
        // choosing one text and dropping the other.
        vec!["you are a careful agent\n\nyou are a careful agent, now with tools".to_owned()],
        "both prompt texts must arrive, joined in log order"
    );
}

// ---- 3. Anthropic shape ----------------------------------------------------------------------

#[test]
fn anthropic_round_trip_preserves_text_and_lifts_the_prompt_out_of_messages() {
    // Anthropic's Messages API has no `system` role at all: the prompt is a top-level field. Asserting
    // the placement structurally — rather than by substring search over the rendered JSON — is what
    // makes this test able to fail when the prompt moves back into `messages`.
    let events = all_event_types();
    let original = log_turns(&events);

    let transcript = export_transcript(&events, TranscriptFormat::Anthropic);
    let body = transcript_to_json(&transcript, TranscriptFormat::Anthropic);

    // Placement: the prompt is top-level, and no message claims the `system` role.
    let system = body
        .get("system")
        .and_then(Value::as_str)
        .expect("Anthropic lifts the prompt to a top-level `system` string");
    assert_eq!(
        system, "you are a careful agent\n\nyou are a careful agent, now with tools",
        "the changed prompts are joined in log order into the single slot the format has"
    );
    let roles: Vec<String> = json_messages(&body)
        .into_iter()
        .map(|(role, _)| role)
        .collect();
    assert!(
        !roles.contains(&"system".to_owned()),
        "Anthropic has no `system` role inside `messages`, got roles: {roles:?}"
    );

    // Content: the conversation text is the same projection the OpenAI body carries.
    let imported = import_transcript(&body, TranscriptFormat::Anthropic)
        .expect("a body this build wrote is one it reads");
    let restored = log_turns(&imported.events);
    assert_eq!(
        restored,
        transcript_turns(&transcript),
        "the top-level `system` field must not swallow or reorder the conversation"
    );
    assert_eq!(
        turns_without_tool_labels(&restored),
        original,
        "every user and assistant message must survive verbatim, in order"
    );
}

#[test]
fn transcript_format_detection_agrees_with_what_export_wrote() {
    // Detection is layered and the two shapes overlap, so a round trip that passed only because the
    // caller hard-coded the format would hide a body that could not be recognised. Both bodies here
    // are round-tripped through `detect` as well.
    let events = all_event_types();

    for format in [TranscriptFormat::OpenAi, TranscriptFormat::Anthropic] {
        let body = transcript_to_json(&export_transcript(&events, format), format);
        assert_eq!(
            detect_transcript_format(&body),
            Some(format),
            "a body this build wrote for {format:?} must be detected as {format:?}"
        );
        // And the detected format is the one the importer accepts.
        let detected = detect_transcript_format(&body).expect("detection succeeded above");
        assert!(
            import_transcript(&body, detected).is_ok(),
            "the detected format must be one import accepts for {format:?}"
        );
    }
}

// ---- 4. compaction playback equivalence -------------------------------------------------------

/// A log with something for each of compaction's three rules to remove, plus content they must keep.
///
/// Built by hand rather than reused from the fixture because the point here is the *removal* side:
/// consecutive identical usage records, a superseded title, and a duplicate-and-empty message pair.
fn compactable_log() -> Vec<SessionEvent> {
    vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::SessionTitle {
            title: "first name".to_owned(),
            message_seqs: vec![1],
            source: "human".to_owned(),
        },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::UserMessage {
            message: user_message("msg-user-1", "what does the config say"),
        },
        // Two byte-identical neighbours: one call's accounting written twice.
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 100,
            output_tokens: 20,
        },
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 100,
            output_tokens: 20,
        },
        SessionEvent::AssistantMessage {
            message: assistant_message("msg-asst-1", "It sets port 8080."),
        },
        // The same title announced again.
        SessionEvent::SessionTitle {
            title: "first name".to_owned(),
            message_seqs: vec![2],
            source: "model".to_owned(),
        },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ]
}

#[test]
fn compaction_preserves_the_digest_of_the_log() {
    // A digest is the compressed answer a session list shows: turns, requests, last answer, tools,
    // files, and whether the log finished. Compaction is only defensible if this projection is
    // identical before and after — a removal that changed the answer would be rewriting history to
    // save bytes.
    let events = compactable_log();
    let compacted = compact_log(&events);

    assert!(
        is_compactable(&events),
        "the fixture must actually be compactable, or this test asserts nothing"
    );
    assert!(
        compacted.removed > 0,
        "the fixture must exercise at least one removal rule"
    );
    assert_eq!(
        digest_session(&compacted.events),
        digest_session(&events),
        "playback must reconstruct the same digest from the compacted log"
    );
}

#[test]
fn compaction_preserves_the_transcript_of_the_log() {
    // The transcript is the other projection every reader consumes, and it is built by a different
    // module with different rules (it collapses repeated prompts, drops usage, and flattens tools).
    // Comparing it after compaction catches a removal that the digest happens not to observe — an
    // empty assistant message, which the digest does not count but a transcript would have shown.
    let events = compactable_log();
    let compacted = compact_log(&events);

    for format in [TranscriptFormat::OpenAi, TranscriptFormat::Anthropic] {
        assert_eq!(
            export_transcript(&compacted.events, format),
            export_transcript(&events, format),
            "the compacted log must play back as the same conversation for {format:?}"
        );
    }
}

#[test]
fn compaction_removes_only_bookkeeping_and_says_why() {
    // `removed` is a claim about the log, so it is checked against the actual length difference and
    // against the reasons that justify it. A rule that removed an event without reporting it would
    // make the compaction unauditable, which the module promises never to do.
    let events = compactable_log();
    let compacted = compact_log(&events);

    assert_eq!(
        compacted.removed,
        events.len() - compacted.events.len(),
        "the reported count must match the events actually gone"
    );
    assert!(
        !compacted.reasons.is_empty(),
        "a non-zero removal must carry one reason per rule that fired"
    );
    assert_eq!(
        compacted.reasons.len(),
        2,
        "the fixture triggers the duplicate-usage and superseded-title rules, got: {:?}",
        compacted.reasons
    );

    // The conversation itself is never compacted away.
    for kept in [
        "user/message",
        "assistant/message",
        "turn/start",
        "step/start",
    ] {
        assert!(
            compacted
                .events
                .iter()
                .any(|event| event.type_name() == kept),
            "`{kept}` carries meaning and must survive compaction"
        );
    }
}

// ---- 5. patch round trip ---------------------------------------------------------------------

#[test]
fn patch_round_trip_reproduces_the_edited_text() {
    // A patch is only useful if applying it reproduces the text it was made against. The patch here is
    // *rendered from* a real line diff rather than hand-written, so the test covers the two halves
    // actually used together: the diff that says what changed, and the parser/applier that redoes it.
    let before = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    let after = "alpha\nbeta\ngamma\nDELTA\nepsilon\nzeta\n";

    let change = diff_text(before, after, DEFAULT_CONTEXT);
    assert!(
        change.added > 0 && change.removed > 0,
        "the texts must differ"
    );

    // Render the single hunk this edit needs into unified-diff text.
    let old_start = 1; // the context window reaches the start of the file
    let (old_lines, new_lines) = (
        count_side(&change.lines, DiffSide::Old),
        count_side(&change.lines, DiffSide::New),
    );
    let mut patch = format!("@@ -{old_start},{old_lines} +{old_start},{new_lines} @@\n");
    for line in &change.lines {
        // `write!` rather than `push_str(&format!(..))`: the write target is already a `String`, so
        // formatting into a temporary and copying it is a second allocation per line.
        let _ = match line {
            LineDiff::Context { line } => writeln!(patch, " {line}"),
            LineDiff::Removed { line } => writeln!(patch, "-{line}"),
            LineDiff::Added { line } => writeln!(patch, "+{line}"),
        };
    }

    let parsed = parse_diff_patch(&patch).expect("a well-formed hunk parses");
    assert_eq!(parsed.hunks.len(), 1, "one edit is one hunk");
    assert_eq!(
        parsed.hunks[0].old_lines,
        u32::try_from(old_lines).expect("the fixture is far below u32::MAX lines"),
        "the declared old-side count must be the one the renderer wrote"
    );
    assert_eq!(
        parsed.hunks[0].new_lines,
        u32::try_from(new_lines).expect("the fixture is far below u32::MAX lines"),
        "the declared new-side count must be the one the renderer wrote"
    );

    let applied =
        apply_diff_patch(before, &parsed).expect("the patch matches the text it was made from");
    assert_eq!(
        applied, after,
        "applying the diff must reproduce the edited text exactly, line for line"
    );

    // Parsing is stable: the same patch parsed twice is the same patch. A parser that depended on
    // traversal order would make a stored patch non-reproducible.
    assert_eq!(
        parse_diff_patch(&patch).expect("parsing is deterministic"),
        parsed,
        "parsing the same patch twice must yield the same value"
    );
}

/// Which side of a diff a line counts toward.
#[derive(Clone, Copy)]
enum DiffSide {
    /// Lines present before the edit.
    Old,
    /// Lines present after it.
    New,
}

/// How many lines a rendered diff contributes to one side of its header.
fn count_side(lines: &[LineDiff], side: DiffSide) -> usize {
    lines
        .iter()
        .filter(|line| {
            matches!(
                (line, side),
                (LineDiff::Context { .. }, _)
                    | (LineDiff::Removed { .. }, DiffSide::Old)
                    | (LineDiff::Added { .. }, DiffSide::New)
            )
        })
        .count()
}

#[test]
fn patch_application_is_refused_when_the_text_moved() {
    // The safety property that makes a patch worth having instead of a full rewrite: if the text is not
    // the one the patch was made against, applying it would edit lines the patch never saw. Silence
    // here would be corruption, so the refusal is asserted.
    let patch = "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";
    let parsed = parse_diff_patch(patch).expect("the patch is well formed");

    let applied = apply_diff_patch("alpha\nnot-beta\ngamma\n", &parsed);
    assert!(
        applied.is_err(),
        "a patch whose context no longer matches must be refused, got: {applied:?}"
    );
}

// ---- 6. export-package manifest --------------------------------------------------------------

#[test]
fn manifest_validates_and_a_broken_total_is_rejected() {
    // The manifest's only job is to be trustworthy before anything expensive is read, and the total is
    // the checksum that makes that possible. So both directions are asserted: the honest one passes,
    // and a deliberately wrong total is refused *with the numbers that disagree* — a rejection that
    // said only "invalid" would leave the caller unable to tell damage from a format mismatch.
    let manifest = export_manifest(
        vec![
            ("session-a".to_owned(), Some("First".to_owned()), 12),
            ("session-b".to_owned(), None, 7),
            ("session-c".to_owned(), Some("Third".to_owned()), 0),
        ],
        1_700_000_000_000,
    );

    assert_eq!(
        manifest.total_events, 19,
        "the builder must sum the entries"
    );
    assert_eq!(manifest.entries.len(), 3);
    assert!(
        validate_export_bundle(&manifest).is_ok(),
        "a manifest this build wrote must validate"
    );

    let mut damaged = manifest.clone();
    damaged.total_events = 20;
    let error = validate_export_bundle(&damaged)
        .expect_err("a total that disagrees with the entries must be refused");
    let rendered = error.to_string();
    assert!(
        rendered.contains("20") && rendered.contains("19"),
        "the refusal must name both numbers so the caller can see the disagreement, got: {rendered}"
    );

    // An empty package is the one manifest that is always a mistake: it is what an export pointed at
    // the wrong directory produces, and a caller reading that as success deletes its source.
    let empty = export_manifest(Vec::new(), 1);
    assert!(
        validate_export_bundle(&empty).is_err(),
        "an empty package must be refused rather than reported as a complete backup"
    );
}

#[test]
fn manifest_survives_being_written_as_json() {
    // A manifest travels between instances as bytes, not as a Rust value, so the round trip that
    // matters is through JSON. Equality of the whole struct proves the field names and the optional
    // title encoding survive, which a "it deserialised" check would not.
    let manifest = export_manifest(
        vec![("session-a".to_owned(), Some("First".to_owned()), 12)],
        42,
    );
    let encoded = serde_json::to_string(&manifest).expect("the manifest is plain data");
    let restored: girder_core::ExportBundle =
        serde_json::from_str(&encoded).expect("the manifest is JSON this build reads");

    assert_eq!(
        restored, manifest,
        "a manifest must come back unchanged through its wire form"
    );
    assert!(
        validate_export_bundle(&restored).is_ok(),
        "a manifest that survived transit must still validate"
    );
}

// ---- findings: round trips that lose data ----------------------------------------------------

#[test]
fn transcript_round_trip_loses_tool_calls_by_design() {
    // FINDING, pinned rather than repaired. A tool call and its result are flattened into labelled
    // `user` text on export (see `session_transcript`), and the importer deliberately does not turn
    // that text back into a `tool/call` — it cannot invent a provider-issued call id. So a session
    // that called a tool comes back with the *prose* of the call and none of its structure.
    //
    // This is documented behaviour, not a defect, and the test exists so that a future change which
    // silently started dropping the text as well would be caught: the call must remain *visible*.
    let events = all_event_types();
    let body = transcript_to_json(
        &export_transcript(&events, TranscriptFormat::OpenAi),
        TranscriptFormat::OpenAi,
    );
    let imported =
        import_transcript(&body, TranscriptFormat::OpenAi).expect("the body is OpenAI shaped");

    let restored_calls = imported
        .events
        .iter()
        .filter(|event| matches!(event, SessionEvent::ToolCall { .. }))
        .count();
    assert_eq!(
        restored_calls, 0,
        "the importer must not fabricate tool calls: the id a replay depends on is not in the export"
    );

    let rendered = json_messages(&body)
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("[tool call] read_file"),
        "the call must still be visible as labelled text, or the exchange is lost entirely"
    );
    assert!(
        rendered.contains("[tool result] read_file"),
        "the result must still be visible as labelled text"
    );
}

#[test]
fn transcript_round_trip_loses_whitespace_at_the_edges_of_a_message() {
    // FINDING, pinned rather than repaired. `session_transcript` trims every message's text on export
    // (`blocks_text` ends in `.trim()`), so a message whose text began or ended with spaces comes back
    // without them. For prose this is invisible; for a pasted code block or an aligned table it is a
    // real change to what the person or the model said.
    //
    // The production behaviour is left alone — trimming is what keeps the transcript free of blank
    // bubbles around tool labels — but the loss is asserted so it cannot widen unnoticed.
    let events = vec![
        SessionEvent::UserMessage {
            message: user_message("msg-user-1", "  indented prompt  "),
        },
        SessionEvent::AssistantMessage {
            message: assistant_message("msg-asst-1", "\ttabbed reply\n"),
        },
    ];
    let body = transcript_to_json(
        &export_transcript(&events, TranscriptFormat::OpenAi),
        TranscriptFormat::OpenAi,
    );
    let imported =
        import_transcript(&body, TranscriptFormat::OpenAi).expect("the body is OpenAI shaped");

    assert_eq!(
        log_turns(&imported.events),
        vec![
            ("user".to_owned(), "indented prompt".to_owned()),
            ("assistant".to_owned(), "tabbed reply".to_owned()),
        ],
        "edge whitespace is trimmed on export and does not come back"
    );
    // The same surface, stated as the difference: the round trip is *not* lossless here.
    assert_ne!(
        log_turns(&imported.events),
        log_turns(&events),
        "this round trip is lossy: the texts differ by their surrounding whitespace"
    );
}

#[test]
fn transcript_round_trip_does_not_restore_the_system_role() {
    // FINDING, pinned rather than repaired. Both formats carry the prompt, and both come back as a
    // `developer/message` rather than a `system/message` — `transcript_import` documents this as a
    // provenance decision, since the harness never assembled that prompt. The consequence for a round
    // trip is that the *role* is not preserved even though the text is, and a second export therefore
    // has no prompt to write at all.
    let events = all_event_types();
    let original_prompts = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::SystemMessage { .. }))
        .count();
    assert!(original_prompts > 0, "the fixture must carry prompts");

    for format in [TranscriptFormat::OpenAi, TranscriptFormat::Anthropic] {
        let body = transcript_to_json(&export_transcript(&events, format), format);
        let imported = import_transcript(&body, format).expect("a body this build wrote");

        let restored_system = imported
            .events
            .iter()
            .filter(|event| matches!(event, SessionEvent::SystemMessage { .. }))
            .count();
        assert_eq!(
            restored_system, 0,
            "an imported prompt must not be passed off as this harness's own for {format:?}"
        );

        // The text survives, in the advisory slot, and the import says so rather than dropping it
        // silently.
        assert!(
            imported
                .events
                .iter()
                .any(|event| matches!(event, SessionEvent::DeveloperMessage { .. })),
            "the prompt's text must arrive as an advisory developer message for {format:?}"
        );
        assert!(
            imported
                .warnings
                .iter()
                .any(|warning| warning.contains("system prompt")),
            "the translation must be reported to the caller for {format:?}, got: {:?}",
            imported.warnings
        );
    }
}

#[test]
fn openai_round_trip_moves_a_mid_session_prompt_to_the_front() {
    // FINDING, pinned rather than repaired. `OpenAI` can hold a `system` message at any position, and
    // the exporter puts a *changed* prompt where it changed. The importer has one advisory slot at the
    // start of turn 1, so on the way back the mid-conversation prompt is hoisted to the front and
    // joined with the first one.
    //
    // The information is not lost — both texts arrive, in log order — but the *position* is. A caller
    // that cares about when the prompt changed cannot recover it from the import.
    let events = all_event_types();
    let body = transcript_to_json(
        &export_transcript(&events, TranscriptFormat::OpenAi),
        TranscriptFormat::OpenAi,
    );

    let roles: Vec<String> = json_messages(&body)
        .into_iter()
        .map(|(role, _)| role)
        .collect();
    assert!(
        roles.iter().skip(1).any(|role| role == "system"),
        "the export places the changed prompt where it changed, got roles: {roles:?}"
    );

    let imported =
        import_transcript(&body, TranscriptFormat::OpenAi).expect("the body is OpenAI shaped");
    let developer_positions: Vec<usize> = imported
        .events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(event, SessionEvent::DeveloperMessage { .. }).then_some(index)
        })
        .collect();
    assert_eq!(
        developer_positions.len(),
        1,
        "both prompt texts collapse into one advisory message"
    );
    assert!(
        matches!(
            imported.events.first(),
            Some(SessionEvent::TurnStart { turn: 1 })
        ) && developer_positions[0] < 4,
        "the advisory message must sit at the start of turn 1, not where the prompt changed"
    );
}

#[test]
fn portable_bundle_round_trip_is_lossless_where_the_transcripts_are_not() {
    // The reason the transport format exists beside the reader-facing ones. Same log, two round trips:
    // the portable bundle restores it exactly, while a transcript restores only the conversation. This
    // is the comparison that makes "lossy projection" a measured claim rather than an assertion.
    let events = all_event_types();

    let encoded = export_bundle(&events, None, 7).expect("the fixture is representable");
    let restored = import_bundle(&encoded)
        .expect("the bundle is one this build reads")
        .events()
        .expect("every event parses");
    assert_eq!(restored, events, "the transport format is lossless");

    let body = transcript_to_json(
        &export_transcript(&events, TranscriptFormat::OpenAi),
        TranscriptFormat::OpenAi,
    );
    let projected = import_transcript(&body, TranscriptFormat::OpenAi)
        .expect("the body is OpenAI shaped")
        .events;
    assert_ne!(
        projected, events,
        "the chat vocabulary must not be lossless: it is a projection, not a transport"
    );
    // What it does keep is the conversation, which is the contract asserted in the tests above —
    // once the tool annotations it adds on the user channel are set aside.
    assert_eq!(
        turns_without_tool_labels(&log_turns(&projected)),
        log_turns(&events),
        "the projection keeps the conversation even though it drops the frame"
    );
}

// ---- a small guard on the fixture itself ------------------------------------------------------

#[test]
fn fixture_covers_every_event_variant() {
    // Every round-trip test above is only as broad as this fixture. If a variant is added to
    // `SessionEvent` and not added here, the portable-bundle test would keep passing while silently
    // not covering the new event — so the count is pinned to the vocabulary as it stands.
    let events = all_event_types();
    let mut kinds: Vec<&str> = events.iter().map(SessionEvent::type_name).collect();
    kinds.sort_unstable();
    kinds.dedup();

    assert_eq!(
        kinds,
        vec![
            "assistant/message",
            "developer/message",
            "llm/retry",
            "model/selection",
            "model/usage",
            "permission/preset",
            "plan/mode",
            "request/context",
            "sandbox/mode",
            "session/title",
            "step/end",
            "step/start",
            "system/message",
            "tool/call",
            "tool/result",
            "turn/end",
            "turn/start",
            "user/message",
        ],
        "the fixture must hold one of every event variant, or the round trips below are partial"
    );
}

#[test]
fn fixture_roles_match_their_sources() {
    // A log that violated the role/source agreement would be refused by a replay, so a fixture that did
    // would be testing round trips of a history that could never exist.
    for event in all_event_types() {
        match event {
            SessionEvent::UserMessage { message }
            | SessionEvent::AssistantMessage { message }
            | SessionEvent::SystemMessage { message, .. }
            | SessionEvent::DeveloperMessage { message, .. } => {
                assert!(
                    message.is_consistent(),
                    "every message event must agree with its source, and this one does not"
                );
            }
            _ => {}
        }
    }
    // The fixture must use the message-level role, not the llm-layer one, or the round trips above
    // would be asserting against messages no replay would accept.
    assert_eq!(
        user_message("msg-role-check", "x").role,
        ChatRole::User,
        "a user message must carry the user role"
    );
}
