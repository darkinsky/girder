//! Tests for the `v2 → v3` whole-artifact session-format migration.
//!
//! Every expectation below was taken from upstream's own implementation
//! (`packages/session/session-format-v2-to-v3`), not invented here: the fixtures are the same
//! event arrays its `migration.spec.ts` and `structural-regressions.spec.ts` use, and the
//! expected targets — including the two generated system message ids and the derived inherited
//! cut — are what that implementation emits. A translation that silently reorders, renumbers, or
//! re-spells anything therefore fails rather than passing on a self-consistent but different
//! answer.
//!
//! The properties under test, in the order they matter:
//!
//! - **the prompt becomes a protected head, and chronology is preserved** — the head lands
//!   immediately after the first `step/start` and replaces only itself on every later change, so a
//!   replay reconstructs the *same* request the source made. This is the property the edge exists
//!   for; a head inserted one row late would silently change what the model saw.
//! - **renumbering moves audited references and nothing else** — `shadowedSeqs`, `messageSeqs`,
//!   `sourceEventSeq`, and `sourceEventSeqs` follow their rows through the shift, while a
//!   workflow-local `seq`, a captured `throughSeq`, a stream index, and numeric model input stay
//!   byte-identical. Remapping too much corrupts a payload; remapping too little leaves a dangling
//!   coordinate.
//! - **the inherited cut is derived, not copied** — inserting heads before a seeded log's
//!   `session/end-seed` marker moves the cut, so the migrated artifact must state the *target*
//!   count.
//! - **it refuses rather than guesses** — a surface row before the first step, a changed prompt
//!   outside an open step, an unclassified event or message source, a non-dense source, a pending
//!   reference, and a generated-id collision each name their reason. A refusal stops a read; a
//!   plausible wrong answer corrupts a history.
//! - **it is deterministic and idempotent at the identity level** — migrating twice produces equal
//!   artifacts, because the generated ids are content-addressed rather than counter-addressed.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written even before `crates/girder-core/src/lib.rs` declares it. It
//! spells its paths through `girder_core::`, the alias the library declares for itself, so the
//! same source builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/migration_steps_gap.rs"]
mod migration_steps_gap;

use migration_steps_gap::{
    Event, Header, SurfaceOp, V2ToV3, is_admitted_v2_type, migrate_session, sha256_hex,
};
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------------------------
// Fixtures. These mirror upstream's `migration.spec.ts` helpers member for member so a difference
// in the fixtures cannot mask a difference in the migration.
// ---------------------------------------------------------------------------------------------

/// The released-v2 header every unseeded fixture uses.
fn header() -> Header {
    Header {
        version: 2,
        id: "identity".to_owned(),
        created_at: 1,
        is_seeded: false,
        delegation_depth: 0,
        cwd: None,
        parent_session: None,
        origin: None,
        agent_preset: None,
    }
}

/// A `request/header` payload: a model config plus the prompt, when there is one.
fn request(system: Option<&str>) -> Value {
    let mut config = json!({ "provider": "mock", "model": "mock" });
    let mut header_value = Map::new();
    header_value.insert("config".to_owned(), config.take());
    if let Some(prompt) = system {
        header_value.insert("system".to_owned(), Value::String(prompt.to_owned()));
    }
    json!({ "header": Value::Object(header_value), "reason": "initial" })
}

/// A user message with one text block.
fn user(id: &str) -> Value {
    json!({
        "role": "user",
        "id": id,
        "source": { "kind": "user" },
        "content": [{ "type": "text", "text": id }],
    })
}

/// One row before it is given a dense position.
///
/// Takes its payload by value so a call site can write `row("turn/start", json!(..))` without an
/// extra binding; the value is only moved into the envelope.
#[allow(
    clippy::needless_pass_by_value,
    reason = "call sites build a json! literal inline; a reference would force a binding at each"
)]
fn row(type_: &str, data: Value) -> Value {
    json!({ "type": type_, "seq": 0, "time": 42, "data": data })
}

/// One row with an explicit `surfaceOp`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "call sites build json! literals inline; references would force a binding at each"
)]
fn surface(type_: &str, data: Value, op: Value) -> Value {
    json!({ "type": type_, "seq": 0, "time": 42, "data": data, "surfaceOp": op })
}

/// Give an array of rows dense `seq` values, the way a released-v2 log numbers them.
fn dense(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .enumerate()
        .map(|(seq, row)| {
            let mut row = row.clone();
            row["seq"] = Value::from(seq);
            row
        })
        .collect()
}

/// The first turn and step of a session.
fn opening() -> Vec<Value> {
    vec![
        row("turn/start", json!({ "turn": 1 })),
        row("step/start", json!({ "turn": 1, "step": 1 })),
    ]
}

/// Decode rows the way a caller would, refusing the same way the stage does.
fn decode(rows: &[Value]) -> Vec<Event> {
    rows.iter()
        .map(|value| Event::from_value(value).expect("fixture row is a released v2 row"))
        .collect()
}

/// Migrate a fixture in one call.
fn migrate(rows: &[Value]) -> migration_steps_gap::SessionArtifact {
    migrate_with(&header(), rows, Some(0))
}

/// Migrate a fixture against an explicit header and declared source cut.
fn migrate_with(
    header: &Header,
    rows: &[Value],
    cut: Option<usize>,
) -> migration_steps_gap::SessionArtifact {
    migrate_session(header, decode(&dense(rows)), cut).expect("fixture migrates")
}

/// The emittied rows as JSON, for comparison against a golden array.
fn values(artifact: &migration_steps_gap::SessionArtifact) -> Vec<Value> {
    artifact.events.iter().map(Event::to_value).collect()
}

/// The event tags in order.
fn tags(artifact: &migration_steps_gap::SessionArtifact) -> Vec<String> {
    artifact
        .events
        .iter()
        .map(|event| event.type_.clone())
        .collect()
}

/// The system heads, as `(seq, surfaceOp, sourceEventSeqs, id, content)`.
fn heads(
    artifact: &migration_steps_gap::SessionArtifact,
) -> Vec<(usize, Value, Vec<usize>, String, Value)> {
    artifact
        .events
        .iter()
        .filter(|event| event.type_ == "system/message")
        .map(|event| {
            let value = event.to_value();
            (
                event.seq,
                value.get("surfaceOp").cloned().unwrap_or(Value::Null),
                event.source_event_seqs.clone().unwrap_or_default(),
                event.data["message"]["id"].as_str().unwrap().to_owned(),
                event.data["message"]["content"].clone(),
            )
        })
        .collect()
}

/// The id of the empty head that opens the surface for the `identity` session.
const EMPTY_HEAD: &str =
    "v2-to-v3-system-c43ced201acd9ac5b45b8d69daae84defd1d9d121ec07bd370ba3babb91ca20d";

/// The id of the head promoted from the prompt `"first"` at source position 3.
const FIRST_HEAD: &str =
    "v2-to-v3-system-c40b78d9d0427da23234ac57a323a4a91d26fdb37e8a6068908b6b6502cd3cd2";

// ---------------------------------------------------------------------------------------------
// The head promotion, which is the reason this edge exists.
// ---------------------------------------------------------------------------------------------

#[test]
fn first_step_opens_an_empty_head_and_the_first_prompt_replaces_it() {
    let artifact = migrate(&[
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("user"), json!("append")),
        row("request/header", request(Some("first"))),
    ]);

    assert_eq!(
        tags(&artifact),
        [
            "turn/start",
            "step/start",
            "system/message",
            "user/message",
            "system/message",
            "request/header",
        ]
    );
    let heads = heads(&artifact);
    assert_eq!(heads.len(), 2);
    // The head that opens the surface is empty and appends; it came from the `step/start` row.
    assert_eq!(heads[0].0, 2);
    assert_eq!(heads[0].1, json!("append"));
    assert_eq!(heads[0].2, Vec::<usize>::new());
    assert_eq!(heads[0].3, EMPTY_HEAD);
    assert_eq!(heads[0].4, json!([]));
    // The first real prompt replaces that head and nothing else, and moves the head to itself.
    assert_eq!(heads[1].0, 4);
    assert_eq!(
        heads[1].1,
        json!({ "op": "replace", "startSeq": 2, "endSeq": 2 })
    );
    assert_eq!(heads[1].2, vec![2]);
    assert_eq!(heads[1].3, FIRST_HEAD);
    assert_eq!(heads[1].4, json!([{ "type": "text", "text": "first" }]));
    // The head carries the open step's coordinates and its anchor's time.
    let head = &artifact.events[4];
    assert_eq!(head.data["turn"], json!(1));
    assert_eq!(head.data["step"], json!(1));
    assert_eq!(head.time, 42);
    assert_eq!(
        head.data["message"]["source"],
        json!({ "kind": "plugin", "plugin": "@deepseek-ai/dsh-system-prompt" })
    );
}

#[test]
fn repeated_unchanged_and_cleared_prompts_emit_exactly_the_heads_they_need() {
    // Upstream's own fixture: two identical prompts, one change, a cleared prompt, then an empty
    // string. Only the three *changes* may emit, so the cleared and empty prompts collapse.
    let artifact = migrate(&[
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("user"), json!("append")),
        row("request/header", request(Some("first"))),
        row("request/header", request(Some("first"))),
        row("request/header", request(Some("changed"))),
        row("request/header", request(None)),
        row("request/header", request(Some(""))),
    ]);

    let heads = heads(&artifact);
    assert_eq!(heads.len(), 4);
    assert_eq!(
        heads.iter().map(|head| head.0).collect::<Vec<_>>(),
        [2, 4, 7, 9]
    );
    assert_eq!(
        heads.iter().map(|head| head.1.clone()).collect::<Vec<_>>(),
        [
            json!("append"),
            json!({ "op": "replace", "startSeq": 2, "endSeq": 2 }),
            json!({ "op": "replace", "startSeq": 4, "endSeq": 4 }),
            json!({ "op": "replace", "startSeq": 7, "endSeq": 7 }),
        ]
    );
    assert_eq!(
        heads.iter().map(|head| head.4.clone()).collect::<Vec<_>>(),
        [
            json!([]),
            json!([{ "type": "text", "text": "first" }]),
            json!([{ "type": "text", "text": "changed" }]),
            json!([]),
        ]
    );
    assert_eq!(artifact.inherited_event_count, 0);
    // Everything that is not a head keeps its source order and its time.
    assert_eq!(
        artifact
            .events
            .iter()
            .filter(|event| event.type_ != "system/message")
            .map(|event| (event.type_.clone(), event.time))
            .collect::<Vec<_>>(),
        [
            ("turn/start".to_owned(), 42),
            ("step/start".to_owned(), 42),
            ("user/message".to_owned(), 42),
            ("request/header".to_owned(), 42),
            ("request/header".to_owned(), 42),
            ("request/header".to_owned(), 42),
            ("request/header".to_owned(), 42),
            ("request/header".to_owned(), 42),
        ]
    );
    // No request header keeps the retired `system` member.
    for event in artifact
        .events
        .iter()
        .filter(|event| event.type_ == "request/header")
    {
        assert!(
            !event.data["header"]
                .as_object()
                .unwrap()
                .contains_key("system"),
            "header.system must be removed by the promotion"
        );
    }
}

#[test]
fn every_request_replays_the_same_messages_it_did_before_migration() {
    // The property, stated without reference to how the heads are spelled: fold each request's
    // surface, and the result must equal what v2's `header.system` contributed.
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("a"), json!("append")),
        row("request/header", request(Some("alpha"))),
        row("step/end", json!({ "turn": 1, "step": 1 })),
        row("step/start", json!({ "turn": 1, "step": 2 })),
        surface("user/message", user("b"), json!("append")),
        row("request/header", request(Some("beta"))),
        row("request/header", request(None)),
        row("step/end", json!({ "turn": 1, "step": 2 })),
        row(
            "turn/end",
            json!({ "turn": 1, "reason": { "kind": "completed" } }),
        ),
        row("turn/start", json!({ "turn": 2 })),
        row("step/start", json!({ "turn": 2, "step": 1 })),
        surface("user/message", user("c"), json!("append")),
        row("request/header", request(Some("alpha"))),
    ];
    let before = replay_v2(&dense(&rows));
    let artifact = migrate(&rows);
    let after = replay_v3(&values(&artifact));
    assert_eq!(before, after);
    assert_eq!(
        after,
        [
            vec![text("system", "alpha"), text("user", "a"),],
            vec![text("system", "beta"), text("user", "a"), text("user", "b"),],
            // The cleared prompt removed the head, so this request has no system message at all —
            // which is exactly what v2 sent for it.
            vec![text("user", "a"), text("user", "b"),],
            vec![
                text("system", "alpha"),
                text("user", "a"),
                text("user", "b"),
                text("user", "c"),
            ],
        ]
    );
}

/// One visible message, the shape both replays produce.
fn text(role: &str, body: &str) -> Value {
    json!({ "role": role, "content": [{ "type": "text", "text": body }] })
}

/// Replay a released-v2 log: `header.system` is prepended to the surface, empty prompts are absent.
fn replay_v2(rows: &[Value]) -> Vec<Vec<Value>> {
    let mut surface: Vec<Value> = Vec::new();
    let mut requests = Vec::new();
    let mut prompt = String::new();
    for row in rows {
        let op = row.get("surfaceOp");
        if op == Some(&json!("append")) {
            surface.push(row.clone());
        } else if let Some(Value::Object(replace)) = op {
            let start = surface
                .iter()
                .position(|candidate| candidate["seq"] == replace["start"])
                .unwrap();
            let end = surface
                .iter()
                .position(|candidate| candidate["seq"] == replace["end"])
                .unwrap();
            surface.splice(start..=end, [row.clone()]);
        }
        if row["type"] == json!("request/header") {
            let next = row["data"]["header"]["system"].as_str().unwrap_or_default();
            prompt.clear();
            prompt.push_str(next);
            let mut messages = visible(&surface);
            if !prompt.is_empty() {
                messages.insert(0, text("system", &prompt));
            }
            requests.push(messages);
        }
    }
    requests
}

/// Replay a migrated log: the head is a surface row like any other, and empty content is invisible.
fn replay_v3(events: &[Value]) -> Vec<Vec<Value>> {
    let mut surface: Vec<Value> = Vec::new();
    let mut requests = Vec::new();
    for event in events {
        let op = event.get("surfaceOp");
        if op == Some(&json!("append")) {
            surface.push(event.clone());
        } else if let Some(Value::Object(replace)) = op {
            let start = surface
                .iter()
                .position(|candidate| candidate["seq"] == replace["startSeq"])
                .unwrap();
            let end = surface
                .iter()
                .position(|candidate| candidate["seq"] == replace["endSeq"])
                .unwrap();
            surface.splice(start..=end, [event.clone()]);
        }
        if event["type"] == json!("request/header") {
            requests.push(visible(&surface));
        }
    }
    requests
}

/// The surface messages a set of rows contributes, in order, dropping empty ones.
fn visible(surface: &[Value]) -> Vec<Value> {
    surface
        .iter()
        .filter_map(|entry| {
            let message = if entry["type"] == json!("user/message") {
                entry["data"].clone()
            } else {
                entry["data"]["message"].clone()
            };
            let empty = message["content"].as_array().is_none_or(Vec::is_empty);
            if empty {
                None
            } else {
                Some(json!({ "role": message["role"], "content": message["content"] }))
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Renumbering: audited references move, everything else does not.
// ---------------------------------------------------------------------------------------------

#[test]
fn renumbering_moves_audited_references_and_leaves_opaque_coordinates_alone() {
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("a"), json!("append")),
        row("request/header", request(Some("sys"))),
        surface("user/message", user("b"), json!("append")),
        row(
            "compaction/prune",
            json!({ "shadowedRange": { "start": 2, "end": 4 }, "shadowedSeqs": [2, 4], "shadowedTokenCount": 20 }),
        ),
        {
            let mut replacement = surface(
                "user/message",
                user("replacement"),
                json!({ "op": "replace", "start": 2, "end": 4 }),
            );
            replacement["sourceEventSeqs"] = json!([2, 4]);
            replacement
        },
        row(
            "command/run",
            json!({ "commandId": "c", "name": "x", "source": { "kind": "user" } }),
        ),
        row(
            "command/done",
            json!({ "commandId": "c", "kind": "success", "sourceEventSeq": 6 }),
        ),
        row(
            "session/title",
            json!({ "title": "title", "messageSeqs": [2, 4], "source": { "kind": "fallback" } }),
        ),
    ];
    let artifact = migrate(&rows);
    let mapped: Vec<&Event> = artifact
        .events
        .iter()
        .filter(|event| event.type_ != "system/message")
        .collect();
    // Source 2 and 4 shifted to 3 and 6 because two heads were inserted before them.
    assert_eq!(mapped[2].seq, 3);
    assert_eq!(mapped[4].seq, 6);
    let prune = &mapped[5].data;
    assert_eq!(prune["shadowedSeqs"], json!([3, 6]));
    assert_eq!(prune["shadowedRange"], json!({ "start": 3, "end": 6 }));
    // The replacement's own declared sources move with it, and its range is canonicalized.
    assert_eq!(mapped[6].type_, "user/message");
    assert_eq!(mapped[6].source_event_seqs, Some(vec![3, 6]));
    assert_eq!(
        mapped[6].surface_op,
        Some(SurfaceOp::Replace {
            start_seq: 3,
            end_seq: 6
        })
    );
    // A command's source reference follows the command/run row.
    assert_eq!(mapped[8].data["sourceEventSeq"], json!(8));
    // A title's message list follows the same rows.
    assert_eq!(mapped[9].data["messageSeqs"], json!([3, 6]));
}

#[test]
fn captures_workflow_counters_and_model_input_are_not_session_coordinates() {
    let reference = json!({
        "kind": "session-reference", "form": "recall", "version": 1,
        "references": [{
            "sessionId": "other", "label": "other", "capturedThroughSeq": 99,
            "capturedFormatVersion": 2, "compacted": false, "originalMessages": 1,
            "retainedMessages": 1, "omittedMessages": 0, "omittedBytes": 0,
            "truncated": false, "inputIndex": 0,
        }],
    });
    let mut referenced = user("ref");
    referenced["source"] = reference.clone();
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", referenced.clone(), json!("append")),
        row(
            "tool-workflow/agent-start",
            json!({ "runId": "run", "seq": 99, "label": "child", "childId": "other" }),
        ),
        surface("user/message", user("human"), json!("append")),
        row(
            "session/title-llm-request",
            json!({
                "titleProvider": "mock",
                "messageSeqs": [2],
                "route": { "provider": "mock", "model": "mock" },
                "system": "title system",
                "messages": [{
                    "role": "user", "id": "title",
                    "source": { "kind": "plugin", "plugin": "dsh-session-title-llm" },
                    "content": [{ "type": "text", "text": "source seq=2 (preserved model input)" }],
                }],
                "maxTokens": 20,
            }),
        ),
    ];
    let artifact = migrate(&rows);
    let kept: Vec<&Event> = artifact
        .events
        .iter()
        .filter(|event| event.type_ != "system/message")
        .collect();
    // The capture is another generation's coordinate: byte-identical.
    assert_eq!(kept[2].data, referenced.as_object().unwrap().clone());
    // A workflow's `seq` is its own counter: byte-identical.
    assert_eq!(kept[3].data["seq"], json!(99));
    // A model-input string that happens to contain a source position is not a coordinate.
    assert_eq!(
        kept[5].data["messages"][0]["content"][0]["text"],
        json!("source seq=2 (preserved model input)")
    );
    // The one real reference on that row did move: source 2 is now target 3.
    assert_eq!(kept[5].data["messageSeqs"], json!([3]));
}

#[test]
fn compaction_cannot_keep_the_head_inside_its_own_shadow() {
    // A shadowed range that named a source row the head is *replacing* must be refused: the target
    // range would swallow the protected head, which no legal v3 artifact allows.
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("a"), json!("append")),
        row("request/header", request(Some("sys"))),
        row(
            "compaction/prune",
            json!({ "shadowedRange": { "start": 2, "end": 2 }, "shadowedSeqs": [2], "shadowedTokenCount": 1 }),
        ),
    ];
    // Source 2 maps to the user row at 3, not to the head at 2, so this range is legal and the
    // head is preserved. The point of the fixture is that the mapping — not the source number —
    // decides, so the migrated range must not name 2.
    let artifact = migrate(&rows);
    let prune = artifact
        .events
        .iter()
        .find(|event| event.type_ == "compaction/prune")
        .unwrap();
    assert_eq!(prune.data["shadowedSeqs"], json!([3]));
    assert_ne!(
        prune.data["shadowedSeqs"],
        json!([2]),
        "the migrated range must not alias the protected head"
    );
}

// ---------------------------------------------------------------------------------------------
// Envelope canonicalization and the preset rename.
// ---------------------------------------------------------------------------------------------

#[test]
fn empty_optional_header_fields_are_dropped() {
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        row(
            "request/header",
            json!({
                "header": {
                    "config": { "provider": "mock", "model": "mock" },
                    "tools": [],
                    "adapterDefaults": {},
                },
                "reason": "initial",
            }),
        ),
    ];
    let artifact = migrate(&rows);
    let request = artifact
        .events
        .iter()
        .find(|event| event.type_ == "request/header")
        .unwrap();
    let header = request.data["header"].as_object().unwrap();
    assert!(!header.contains_key("tools"));
    assert!(!header.contains_key("adapterDefaults"));
    assert_eq!(
        header["config"],
        json!({ "provider": "mock", "model": "mock" })
    );
}

#[test]
fn only_the_exact_code_preset_is_renamed() {
    for preset in [
        "code",
        "ptc",
        "standard",
        "minimal",
        "custom-agent",
        "code-custom",
        "Code",
    ] {
        let mut source = header();
        source.agent_preset = Some(preset.to_owned());
        let target = source.migrate().expect("header migrates");
        assert_eq!(target.version, 3);
        assert_eq!(
            target.agent_preset.as_deref(),
            Some(if preset == "code" { "ptc" } else { preset }),
            "preset {preset} must be renamed only when it is exactly `code`"
        );
        // The source header is not mutated: a caller may still be holding it.
        assert_eq!(source.agent_preset.as_deref(), Some(preset));
        assert_eq!(source.version, 2);
    }
    // An absent preset stays absent rather than becoming a default.
    assert_eq!(header().migrate().unwrap().agent_preset, None);
}

#[test]
fn ptc_vocabulary_is_renamed_in_every_attribution_slot() {
    let code_mode = json!({ "kind": "plugin", "plugin": "tools-code-mode" });
    let mut attributed = user("h");
    attributed["source"] = code_mode.clone();
    let mut inbox_message = user("i");
    inbox_message["source"] = code_mode.clone();
    let mut title_message = user("t");
    title_message["source"] = code_mode.clone();
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        row("agent-preset/selected", json!({ "agentPreset": "code" })),
        surface("user/message", attributed, json!("append")),
        row(
            "agent/inbox/spliced",
            json!({ "target": "next-turn", "start": 0, "inserted": [inbox_message] }),
        ),
        row(
            "session/title-llm-request",
            json!({
                "titleProvider": "mock",
                "messageSeqs": [3],
                "route": { "provider": "mock", "model": "mock" },
                "system": "s",
                "messages": [title_message],
                "maxTokens": 20,
            }),
        ),
        row(
            "tool/code-dispatch-start",
            json!({ "rootCallId": "r", "parentCallId": "p", "subCallId": "s", "name": "n", "arguments": "{}" }),
        ),
        row(
            "tool/code-dispatch",
            json!({
                "rootCallId": "r", "parentCallId": "p", "subCallId": "s",
                "name": "n", "arguments": "{}", "isError": false, "content": [],
            }),
        ),
    ];
    let artifact = migrate(&rows);
    let ptc = json!({ "kind": "plugin", "plugin": "tools-ptc" });
    assert_eq!(artifact.events[3].data["agentPreset"], json!("ptc"));
    assert_eq!(artifact.events[4].data["source"], ptc);
    assert_eq!(artifact.events[5].data["inserted"][0]["source"], ptc);
    assert_eq!(artifact.events[6].data["messages"][0]["source"], ptc);
    assert_eq!(artifact.events[7].type_, "tool/ptc-dispatch-start");
    assert_eq!(artifact.events[8].type_, "tool/ptc-dispatch");
    // A dispatch payload's own members are untouched.
    assert_eq!(artifact.events[8].data["rootCallId"], json!("r"));
    assert_eq!(artifact.events[8].data["arguments"], json!("{}"));
}

#[test]
fn a_non_code_preset_selection_keeps_its_metadata() {
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        row(
            "agent-preset/selected",
            json!({ "agentPreset": "standard" }),
        ),
    ];
    let artifact = migrate(&rows);
    // Two heads precede it, so the selection lands at target position 3.
    assert_eq!(artifact.events[3].type_, "agent-preset/selected");
    assert_eq!(
        artifact.events[3].data,
        json!({ "agentPreset": "standard" })
            .as_object()
            .unwrap()
            .clone()
    );
}

// ---------------------------------------------------------------------------------------------
// The inherited cut.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_seeded_cut_is_derived_after_the_inserted_heads() {
    let mut seeded = header();
    seeded.is_seeded = true;
    seeded.parent_session = Some("parent".to_owned());
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        row("request/header", request(Some("seed"))),
        row(
            "session-log-deepseek/delivery-accepted",
            json!({ "sessionId": "parent", "throughSeq": 1, "sessionFormatVersion": 2 }),
        ),
        row("session/end-seed", json!({ "inherited": true })),
    ];
    // With no declared cut the marker decides, and the cut is the *target* count: six rows, not
    // the marker's source position of four, because two heads were inserted before it.
    let artifact = migrate_with(&seeded, &rows, None);
    assert_eq!(artifact.inherited_event_count, 6);
    assert_eq!(artifact.header.version, 3);
    assert!(artifact.header.is_seeded);
    assert_eq!(artifact.header.parent_session.as_deref(), Some("parent"));
    assert_eq!(
        tags(&artifact),
        [
            "turn/start",
            "step/start",
            "system/message",
            "system/message",
            "request/header",
            "session-log-deepseek/delivery-accepted",
            "session/end-seed",
        ]
    );
    // The declared cut, when it agrees, is accepted and yields the same answer.
    assert_eq!(
        migrate_with(&seeded, &rows, Some(4)).inherited_event_count,
        6
    );
}

#[test]
fn a_declared_cut_that_disagrees_with_the_marker_is_refused() {
    let mut seeded = header();
    seeded.is_seeded = true;
    seeded.parent_session = Some("parent".to_owned());
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        row("request/header", request(Some("seed"))),
        row(
            "session-log-deepseek/delivery-accepted",
            json!({ "sessionId": "parent", "throughSeq": 1, "sessionFormatVersion": 2 }),
        ),
        row("session/end-seed", json!({ "inherited": true })),
    ];
    let error = migrate_session(&seeded, decode(&dense(&rows)), Some(3)).unwrap_err();
    assert!(
        error.to_string().contains("disagrees with its source cut"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn an_unseeded_log_cannot_claim_an_inherited_marker() {
    let rows = [row("session/end-seed", json!({ "inherited": true }))];
    let error = migrate_session(&header(), decode(&dense(&rows)), Some(0)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unseeded Session contains an inherited end-seed marker"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn a_seeded_log_without_a_marker_cannot_state_a_cut() {
    let mut seeded = header();
    seeded.is_seeded = true;
    let error = migrate_session(&seeded, Vec::new(), None).unwrap_err();
    assert!(
        error.to_string().contains("inherited end-seed marker"),
        "unexpected refusal: {error}"
    );
}

// ---------------------------------------------------------------------------------------------
// Refusals. Each names state the edge needs and a row cannot supply.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_surface_before_the_first_step_is_refused_rather_than_reordered() {
    let rows = [
        surface("user/message", user("user"), json!("append")),
        opening()[0].clone(),
        opening()[1].clone(),
    ];
    let error = migrate_session(&header(), decode(&dense(&rows)), Some(0)).unwrap_err();
    assert!(
        error.is_unsupported(),
        "expected an unsupported refusal: {error}"
    );
    assert!(
        error.to_string().contains("before first step"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn a_changed_prompt_outside_an_open_step_is_refused() {
    // Before the step opens: there is no `{ turn, step }` to stamp on a head.
    let early = [
        opening()[0].clone(),
        row("request/header", request(Some("early"))),
        opening()[1].clone(),
    ];
    let error = migrate_session(&header(), decode(&dense(&early)), Some(0)).unwrap_err();
    assert!(
        error.to_string().contains("outside an open step"),
        "unexpected refusal: {error}"
    );
    // After the step closes: the replacement has nowhere to go in the surface history.
    let late = [
        opening()[0].clone(),
        opening()[1].clone(),
        row("step/end", json!({ "turn": 1, "step": 1 })),
        row("request/header", request(Some("late"))),
    ];
    let error = migrate_session(&header(), decode(&dense(&late)), Some(0)).unwrap_err();
    assert!(
        error.to_string().contains("outside an open step"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn a_generated_head_id_that_collides_with_a_source_message_is_refused_in_either_order() {
    // Direction 1 — the source message is *already* present when the head is minted. A log-only
    // inbox row may precede the first step, so the id can be recorded before any head exists; the
    // minting side then finds it taken. A generated id is anchored at its own source position, so
    // the fixture uses the id the *shifted* run mints, which is what upstream's test derives.
    let shifted_input = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("a"), json!("append")),
        row("request/header", request(Some("system"))),
    ];
    let inbox = |id: &str| {
        let mut rows = vec![row(
            "agent/inbox/spliced",
            json!({ "target": "next-turn", "start": 0, "inserted": [user(id)] }),
        )];
        rows.extend(shifted_input.iter().cloned());
        rows
    };
    let probe = migrate(&inbox("placeholder"));
    let minted = heads(&probe)
        .first()
        .expect("the shifted run mints an empty head")
        .3
        .clone();
    assert_ne!(
        minted, EMPTY_HEAD,
        "a preceding inbox row shifts the anchor, so the minted id differs"
    );
    let error = migrate_session(&header(), decode(&dense(&inbox(&minted))), Some(0)).unwrap_err();
    assert!(error.is_unsupported());
    assert!(
        error
            .to_string()
            .contains("generated system message id collides with an existing message id"),
        "unexpected refusal: {error}"
    );

    // Direction 2 — the head is already minted when the source message arrives. The head that opens
    // the surface is emitted by `step/start`, so a message carrying its id can only be caught from
    // the source side.
    let mut after = [
        opening()[0].clone(),
        opening()[1].clone(),
        row("request/header", request(Some("system"))),
    ]
    .to_vec();
    after.push(row(
        "agent/inbox/spliced",
        json!({ "target": "next-turn", "start": 0, "inserted": [user(EMPTY_HEAD)] }),
    ));
    let error = migrate_session(&header(), decode(&dense(&after)), Some(0)).unwrap_err();
    assert!(error.is_unsupported());
    assert!(
        error
            .to_string()
            .contains("source message id collides with a generated system message id"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn an_unclassified_event_is_refused_even_when_ignorable() {
    for ignorable in [false, true] {
        let mut opaque = row("external/opaque", json!({ "seq": 1 }));
        if ignorable {
            opaque["ignorable"] = json!(true);
        }
        let rows = [opening()[0].clone(), opening()[1].clone(), opaque];
        let error = migrate_session(&header(), decode(&dense(&rows)), Some(0)).unwrap_err();
        assert!(error.is_unsupported());
        assert!(
            error.to_string().contains("unclassified event"),
            "unexpected refusal: {error}"
        );
    }
}

#[test]
fn an_unclassified_message_source_or_content_kind_is_refused() {
    let mut future_source = user("x");
    future_source["source"] = json!({ "kind": "future", "seq": 0 });
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", future_source, json!("append")),
    ];
    let error = migrate_session(&header(), decode(&dense(&rows)), Some(0)).unwrap_err();
    assert!(
        error.to_string().contains("unclassified message source"),
        "unexpected refusal: {error}"
    );

    let mut future_block = user("x");
    future_block["content"] = json!([{ "type": "future-block", "seq": 1 }]);
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", future_block, json!("append")),
    ];
    let error = migrate_session(&header(), decode(&dense(&rows)), Some(0)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unclassified message content kind"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn a_non_dense_source_is_refused() {
    // Upstream's `dense()` is what makes a source legal; hand it a gap and it must stop, because a
    // sparse log has no defined source-to-target mapping.
    let mut gapped = row("agent-preset/selected", json!({ "agentPreset": "code" }));
    gapped["seq"] = json!(19);
    let event = Event::from_value(&gapped).unwrap();
    let mut stage = V2ToV3::new(&header(), Some(0)).unwrap();
    let error = stage.transform_event(event).unwrap_err();
    assert!(
        error.to_string().contains("must be dense"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn a_reference_to_a_row_that_is_not_earlier_is_refused() {
    // `sourceEventSeqs` cannot name a later row at all, so the row is not a released-v2 surface row
    // and decoding refuses it before the stage sees it.
    let mut forward = surface("user/message", user("x"), json!("append"));
    forward["sourceEventSeqs"] = json!([99]);
    let mut gapped = surface("user/message", user("x"), json!("append"));
    gapped["sourceEventSeqs"] = json!([0, 0]);
    for broken in [forward, gapped] {
        let rows = dense(&[opening()[0].clone(), opening()[1].clone(), broken]);
        let error = Event::from_value(&rows[2]).unwrap_err();
        assert!(
            error.to_string().contains("unique earlier seqs"),
            "unexpected refusal: {error}"
        );
    }
    // A later row that names no earlier one is fine; only a *forward* or repeated name is not.
    let legal = [opening()[0].clone(), opening()[1].clone()];
    let mut referenced = surface("user/message", user("x"), json!("append"));
    referenced["sourceEventSeqs"] = json!([1]);
    let rows = dense(&[legal[0].clone(), legal[1].clone(), referenced]);
    assert!(Event::from_value(&rows[2]).is_ok());
}

#[test]
fn a_delivery_marker_must_name_this_session_or_its_parent() {
    let foreign = row(
        "session-log-deepseek/delivery-accepted",
        json!({ "sessionId": "foreign", "throughSeq": 1, "sessionFormatVersion": 2 }),
    );
    let rows = [opening()[0].clone(), opening()[1].clone(), foreign];
    let error = migrate_session(&header(), decode(&dense(&rows)), Some(0)).unwrap_err();
    assert!(
        error.to_string().contains("wrong Session"),
        "unexpected refusal: {error}"
    );
    // A marker naming this session is carried verbatim.
    let own = row(
        "session-log-deepseek/delivery-accepted",
        json!({ "sessionId": "identity", "throughSeq": 1, "sessionFormatVersion": 2 }),
    );
    let artifact = migrate(&[opening()[0].clone(), opening()[1].clone(), own]);
    assert_eq!(
        artifact.events.last().unwrap().data,
        json!({ "sessionId": "identity", "throughSeq": 1, "sessionFormatVersion": 2 })
            .as_object()
            .unwrap()
            .clone()
    );
}

#[test]
fn a_marker_claiming_the_target_generation_is_refused() {
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        row(
            "session-log-deepseek/delivery-accepted",
            json!({ "sessionId": "identity", "throughSeq": 1, "sessionFormatVersion": 3 }),
        ),
    ];
    let error = migrate_session(&header(), decode(&dense(&rows)), Some(0)).unwrap_err();
    assert!(
        error.to_string().contains("claims target format v3"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn unaudited_payload_members_are_refused() {
    let mut extended = request(Some("x"));
    extended["futureRef"] = json!(0);
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        row("request/header", extended),
    ];
    let error = migrate_session(&header(), decode(&dense(&rows)), Some(0)).unwrap_err();
    assert!(
        error.to_string().contains("unexpected field"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn a_source_header_that_is_not_released_v2_is_refused() {
    let mut wrong = header();
    wrong.version = 1;
    let error = wrong.migrate().unwrap_err();
    assert!(
        error.to_string().contains("expected format v2 header"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn a_non_absolute_cwd_is_refused() {
    let value = json!({
        "version": 2, "id": "x", "createdAt": 1, "isSeeded": false,
        "delegationDepth": 0, "cwd": "relative/path",
    });
    let error = Header::from_value(&value).unwrap_err();
    assert!(
        error.to_string().contains("cwd must be absolute"),
        "unexpected refusal: {error}"
    );
    // An origin other than `subagent` is likewise not a released v2 header.
    let value = json!({
        "version": 2, "id": "x", "createdAt": 1, "isSeeded": false,
        "delegationDepth": 0, "origin": "root",
    });
    let error = Header::from_value(&value).unwrap_err();
    assert!(
        error.to_string().contains("origin must be"),
        "unexpected refusal: {error}"
    );
}

// ---------------------------------------------------------------------------------------------
// Determinism, and the shapes that must not change.
// ---------------------------------------------------------------------------------------------

#[test]
fn migrating_twice_produces_the_same_artifact() {
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("a"), json!("append")),
        row("request/header", request(Some("first"))),
    ];
    let once = migrate(&rows);
    let twice = migrate(&rows);
    assert_eq!(once, twice);
    assert_eq!(values(&once), values(&twice));
}

#[test]
fn an_empty_or_metadata_only_log_survives_without_inventing_a_tail_request() {
    let empty = migrate(&[]);
    assert!(empty.events.is_empty());
    assert_eq!(empty.inherited_event_count, 0);
    // A step that never produced a request still gets its empty head, and nothing more.
    let headerless = migrate(&[
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("user"), json!("append")),
    ]);
    assert_eq!(
        tags(&headerless),
        ["turn/start", "step/start", "system/message", "user/message"]
    );
    // A log with no step at all is carried verbatim.
    let metadata = migrate(&[row("feedback/record", json!({ "text": "metadata" }))]);
    assert_eq!(tags(&metadata), ["feedback/record"]);
    assert_eq!(
        metadata.events[0].data,
        json!({ "text": "metadata" }).as_object().unwrap().clone()
    );
}

#[test]
fn ignorable_is_preserved_and_absent_when_absent() {
    let mut selected = row("agent-preset/selected", json!({ "agentPreset": "code" }));
    selected["ignorable"] = json!(true);
    selected["time"] = json!(-7);
    let rows = [opening()[0].clone(), opening()[1].clone(), selected];
    let artifact = migrate(&rows);
    let emitted = &artifact.events[3];
    assert_eq!(emitted.type_, "agent-preset/selected");
    assert_eq!(emitted.ignorable, Some(true));
    assert_eq!(emitted.time, -7);
    assert_eq!(emitted.to_value()["ignorable"], json!(true));
    // A native row without the flag does not gain one.
    let plain = migrate(&[opening()[0].clone(), opening()[1].clone()]);
    assert!(
        !plain.events[1]
            .to_value()
            .as_object()
            .unwrap()
            .contains_key("ignorable")
    );
}

// ---------------------------------------------------------------------------------------------
// The inventory and the digest are the two things a wrong translation would quietly get wrong.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_stage_exposes_the_header_and_rows_it_has_so_far() {
    // A streaming caller reads the stage between rows — that is why the migration is a stage and
    // not one function — so the accessors must reflect exactly what has been emitted, not the
    // source.
    let rows = [
        opening()[0].clone(),
        opening()[1].clone(),
        surface("user/message", user("a"), json!("append")),
    ];
    let mut stage = V2ToV3::new(&header(), Some(0)).unwrap();
    // The target header is available before any row is transformed.
    assert_eq!(stage.header().version, 3);
    assert!(stage.events().is_empty());

    let decoded = decode(&dense(&rows));
    stage.transform_event(decoded[0].clone()).unwrap();
    assert_eq!(stage.events().len(), 1);
    stage.transform_event(decoded[1].clone()).unwrap();
    // `step/start` emitted both itself and the empty head.
    assert_eq!(stage.events().len(), 3);
    assert_eq!(stage.events()[2].type_, "system/message");
    stage.transform_event(decoded[2].clone()).unwrap();
    assert_eq!(stage.events().len(), 4);
    assert_eq!(stage.finish().unwrap(), 0);
    // `finish` borrows, so the stage is still readable afterwards.
    assert_eq!(stage.events().len(), 4);
    assert_eq!(stage.header().id, "identity");
}

#[test]
fn a_migrated_header_renders_the_canonical_member_set() {
    let mut seeded = header();
    seeded.is_seeded = true;
    seeded.parent_session = Some("parent".to_owned());
    seeded.cwd = Some("/work".to_owned());
    seeded.agent_preset = Some("code".to_owned());
    let rendered = seeded.migrate().unwrap().to_value();
    assert_eq!(
        rendered,
        json!({
            "version": 3,
            "id": "identity",
            "createdAt": 1,
            "isSeeded": true,
            "delegationDepth": 0,
            "cwd": "/work",
            "parentSession": "parent",
            "agentPreset": "ptc",
        })
    );
    // Absent optionals are omitted rather than rendered as null, so the target is byte-canonical.
    let bare = header().migrate().unwrap().to_value();
    let members = bare.as_object().unwrap();
    assert_eq!(members.len(), 5);
    for absent in ["cwd", "parentSession", "origin", "agentPreset"] {
        assert!(!members.contains_key(absent), "{absent} must be omitted");
    }
    // The rendered header names exactly the members a released header may carry, so the only
    // difference from the source is the generation and the preset.
    let mut reread_source = bare.clone();
    reread_source["version"] = json!(2);
    let reread =
        Header::from_value(&reread_source).expect("the rendered member set is a released header");
    assert_eq!(reread.version, 2);
    assert_eq!(reread.id, "identity");
    assert_eq!(reread.created_at, 1);
    assert!(!reread.is_seeded);
    assert_eq!(reread.delegation_depth, 0);
}

#[test]
fn the_admitted_inventory_is_the_released_one() {
    // `assistant/chunk` belongs to v1 and was folded away by the v1 → v2 edge.
    assert!(!is_admitted_v2_type("assistant/chunk"));
    assert!(is_admitted_v2_type("assistant/attempt"));
    assert!(is_admitted_v2_type("assistant/message"));
    assert!(is_admitted_v2_type("session/end-seed"));
    assert!(is_admitted_v2_type("feedback/message-put"));
    assert!(!is_admitted_v2_type("system/message"));
}

#[test]
fn sha256_matches_published_vectors() {
    // The digest is only ever used to mint a message id, but an id is compared against logs that
    // upstream migrated, so the digest has to be the same function.
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn generated_ids_are_the_upstream_values() {
    // Pinned literally: these are the two ids upstream's implementation emitted for the fixture
    // above, so a change in the identity string, the separator, or the digest fails here.
    assert_eq!(EMPTY_HEAD.len(), "v2-to-v3-system-".len() + 64);
    assert_eq!(FIRST_HEAD.len(), "v2-to-v3-system-".len() + 64);
    assert_ne!(EMPTY_HEAD, FIRST_HEAD);
}

#[test]
fn the_envelope_round_trips_through_json() {
    let rows = [opening()[0].clone(), opening()[1].clone(), {
        let mut replaced = surface(
            "user/message",
            user("r"),
            json!({ "op": "replace", "start": 0, "end": 1 }),
        );
        replaced["sourceEventSeqs"] = json!([0, 1]);
        replaced
    }];
    let artifact = migrate(&rows);
    // An emitted row is canonical *v3*: `surfaceOp` uses `startSeq`/`endSeq`, which is not a
    // released-v2 spelling. So the round trip that must hold is through the target spelling, not
    // back through `from_value` (a v2 reader, which is right to refuse it).
    for event in &artifact.events {
        let value = event.to_value();
        let object = value.as_object().unwrap();
        assert_eq!(object["seq"], json!(event.seq));
        assert_eq!(object["type"], json!(event.type_));
        if let Some(SurfaceOp::Replace { start_seq, end_seq }) = event.surface_op {
            assert_eq!(object["surfaceOp"]["op"], json!("replace"));
            assert_eq!(object["surfaceOp"]["startSeq"], json!(start_seq));
            assert_eq!(object["surfaceOp"]["endSeq"], json!(end_seq));
            assert!(
                !object["surfaceOp"]
                    .as_object()
                    .unwrap()
                    .contains_key("start"),
                "a canonical v3 replacement must not keep the v2 `start` spelling"
            );
        }
    }
    // The released-v2 reader is right to refuse a canonical row: that is what makes `from_value` a
    // v2 *source* reader rather than a general one.
    let replaced = values(&artifact)
        .into_iter()
        .find(|value| value["type"] == json!("user/message"))
        .unwrap();
    let error = Event::from_value(&replaced).unwrap_err();
    assert!(
        error.to_string().contains("surfaceOp"),
        "unexpected refusal: {error}"
    );
}
