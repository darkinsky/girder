//! Tests for the session format migration framework.
//!
//! The property that matters is that an old log line can be recognized and upgraded *without
//! guessing*: a row whose correct result depends on state the migrator cannot see must be refused
//! with a reason, because a plausible-looking wrong migration corrupts a history silently while a
//! refusal only stops a read.
//!
//! The second property is idempotence. A migration that ran halfway — a crash between rows, a
//! re-run over an already-converted file — must leave the converted rows alone, or a second pass
//! would namespace a tag twice or re-lift a message that is already first-class.

// The module is compiled into this test target by path so the suite runs before
// `crates/girder-core/src/lib.rs` declares `pub mod session_migration;`. Once that wiring
// exists this include can be dropped and the import below changed to
// `girder_core::session_migration::{..}`; keep both in step with the library module.
use girder_core::{
    FormatVersion, Migration, MigrationError, Migrator, detect_format as detect,
    upgrade_event as upgrade, upgrade_in_session,
};

/// A v0 row as the released lane wrote it: flat payload, `steering/message` name.
const V0_STEERING: &str = r#"{"type":"steering/message","seq":2,"time":100,"data":{"turn":1,"content":[{"type":"text","text":"hi"}],"source":{"kind":"user"}}}"#;

/// A v0 turn start carrying the since-removed `trigger`.
const V0_TURN_START: &str =
    r#"{"type":"turn/start","seq":1,"time":100,"data":{"turn":1,"trigger":{"kind":"prompt"}}}"#;

/// A v0 turn end whose reason uses the retired `disposed` tag.
const V0_TURN_END_DISPOSED: &str =
    r#"{"type":"turn/end","seq":3,"time":100,"data":{"turn":1,"reason":{"kind":"disposed"}}}"#;

/// A v1 row: identity conversion already ran, so this is what v0→v1 must produce.
const V1_USER: &str = r#"{"type":"user/message","seq":2,"time":100,"data":{"id":"m2","role":"user","content":[{"type":"text","text":"hi"}],"source":{"kind":"user"}}}"#;

/// A v2 row: an assistant message carrying its embedded stream.
const V2_ASSISTANT: &str = r#"{"type":"assistant/message","seq":4,"time":120,"data":{"message":{"id":"m4","role":"assistant","content":[],"source":{"kind":"model"}},"stream":[{"type":"chunk","chunk":{"type":"block-start","blockType":"text"}}]}}"#;

/// A v3 tool result: a `user`-role message holding one `tool-result` wrapper.
const V3_TOOL_RESULT: &str = r#"{"type":"tool/result","seq":7,"time":200,"data":{"message":{"id":"m7","role":"user","source":{"kind":"tool","callId":"call-1"},"content":[{"type":"tool-result","toolCallId":"call-1","content":[{"type":"text","text":"42"}],"isError":false}]}}}"#;

/// A v3 user message whose source is still a plugin wrapper.
const V3_PLUGIN_SOURCE: &str = r#"{"type":"user/message","seq":9,"time":300,"data":{"id":"m9","role":"user","content":[{"type":"text","text":"summarized"}],"source":{"kind":"plugin","plugin":"compact","compactionId":"c1"}}}"#;

/// A v3 PTC dispatch whose content uses an extension tag. `tool/ptc-dispatch` is a v3-only event
/// name, so the row is unambiguously v3 even though the extension tag itself is generation-neutral.
const V3_EXTENSION_BLOCK: &str = r#"{"type":"tool/ptc-dispatch","seq":11,"time":400,"data":{"rootCallId":"c1","parentCallId":"c1","subCallId":"c2","name":"t","arguments":"{}","isError":false,"content":[{"type":"hologram","payload":1}]}}"#;

#[test]
fn a_header_reports_the_generation_its_version_field_names() {
    for version in 0..=4u64 {
        let line = format!(r#"{{"type":"session","version":{version},"id":"s1"}}"#);
        assert_eq!(
            detect(&line),
            FormatVersion::from_number(version),
            "version {version} should round-trip"
        );
    }
}

#[test]
fn a_header_from_a_newer_harness_is_not_silently_treated_as_current() {
    let line = r#"{"type":"session","version":5,"id":"s1"}"#;
    assert_eq!(detect(line), None, "a future generation must be refused");
}

#[test]
fn each_generations_own_marker_is_recognized() {
    let cases = [
        (V0_STEERING, FormatVersion::V0),
        (V0_TURN_START, FormatVersion::V0),
        (V0_TURN_END_DISPOSED, FormatVersion::V0),
        (V1_USER, FormatVersion::V1),
        (V2_ASSISTANT, FormatVersion::V2),
        (V3_TOOL_RESULT, FormatVersion::V3),
        (V3_EXTENSION_BLOCK, FormatVersion::V3),
    ];
    for (line, expected) in cases {
        assert_eq!(detect(line), Some(expected), "row was {line}");
    }
}

#[test]
fn the_newest_marker_wins_when_a_row_carries_two() {
    // A canonical replacement endpoint is a v3 marker, but the message it belongs to is already
    // first-class, which is v4 output. Detection must prefer v4: reporting v3 would re-run an edge
    // that already ran.
    let line = r#"{"type":"system/message","seq":2,"time":3,"data":{"message":{"id":"m2","role":"system","content":[],"source":{"kind":"system-prompt"}},"surfaceOp":{"op":"replace","startSeq":0,"endSeq":0}}}"#;
    assert_eq!(detect(line), Some(FormatVersion::V4));
}

#[test]
fn a_generation_neutral_row_is_attributed_to_the_oldest_generation_that_wrote_it() {
    // `step/start` is byte-identical in v1, v2, and v3. Detection cannot invent a distinction the
    // row does not carry, so it reports the oldest generation that could have written it.
    let line = r#"{"type":"step/start","seq":0,"time":1,"data":{"turn":1,"step":1}}"#;
    assert_eq!(detect(line), Some(FormatVersion::V1));
}

#[test]
fn a_packed_chunk_run_is_recognized_as_the_older_lane() {
    // Packed runs frame several events with `seq0`/`time0` instead of one with `seq`/`time`, so they
    // must be checked before the envelope test or they would look like a truncated current row.
    let line = r#"{"type":"text-chunks","seq0":4,"time0":10,"data":{"turn":1,"step":1,"index":0,"dt":[1,1],"texts":["a","b","c"]}}"#;
    assert_eq!(detect(line), Some(FormatVersion::V1));
}

#[test]
fn a_truncated_envelope_is_not_called_current() {
    // A row with some envelope members but not all is a partial write, not a reduced row.
    let line = r#"{"type":"tool/result","seq":"x","time":1,"data":{}}"#;
    assert_eq!(detect(line), None);
}

#[test]
fn the_installed_readers_own_row_shape_is_current() {
    let line = r#"{"type":"turn/start","turn":1}"#;
    assert_eq!(detect(line), Some(FormatVersion::V4));
}

#[test]
fn an_object_outside_every_vocabulary_is_unrecognized_rather_than_current() {
    let line = r#"{"type":"something/else","payload":1}"#;
    assert_eq!(detect(line), None);
}

#[test]
fn a_malformed_line_is_not_placed_and_does_not_panic() {
    for line in [
        "",
        "   ",
        "not json",
        "{",
        "[]",
        "null",
        "42",
        r#"{"no":"type"}"#,
        r#"{"type":9}"#,
        r#"{"type":"session"}"#,
        r#"{"type":"session","version":"4"}"#,
        r#"{"type":"tool/result","seq":"x","time":1,"data":{}}"#,
        "\u{0}\u{1}\u{2}",
    ] {
        assert_eq!(detect(line), None, "line was {line:?}");
    }
}

#[test]
fn a_v0_header_advances_to_v1_and_keeps_its_framing() {
    let line = r#"{"type":"session","version":0,"id":"s1","createdAt":1,"delegationDepth":0,"seedLength":3}"#;
    let upgraded = upgrade(line, FormatVersion::V0).expect("header should migrate");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["version"], 1);
    assert_eq!(parsed["id"], "s1");
    assert_eq!(
        parsed["seedLength"], 3,
        "seedLength is physical framing, not the logical isSeeded field"
    );
    assert_eq!(detect(&upgraded), Some(FormatVersion::V1));
}

#[test]
fn v0_turn_start_loses_the_retired_trigger() {
    let upgraded = upgrade(V0_TURN_START, FormatVersion::V0).expect("turn start should migrate");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["data"], serde_json::json!({ "turn": 1 }));
    assert_eq!(parsed["type"], "turn/start");
    assert_eq!(parsed["seq"], 1);
    assert_eq!(parsed["time"], 100);
}

#[test]
fn v0_turn_end_translates_a_disposed_reason_into_a_cancellation() {
    let upgraded =
        upgrade(V0_TURN_END_DISPOSED, FormatVersion::V0).expect("turn end should migrate");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(
        parsed["data"],
        serde_json::json!({ "turn": 1, "reason": { "kind": "aborted", "reason": { "kind": "disposed" } } })
    );
}

#[test]
fn v0_turn_end_folds_a_flattened_error_into_the_nested_object() {
    let line = r#"{"type":"turn/end","seq":3,"time":1,"data":{"turn":1,"reason":{"kind":"error","step":2,"message":"boom","code":"E1"}}}"#;
    let upgraded = upgrade(line, FormatVersion::V0).expect("error reason should migrate");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(
        parsed["data"]["reason"],
        serde_json::json!({ "kind": "error", "error": { "message": "boom", "code": "E1" } })
    );
}

#[test]
fn v0_turn_end_defaults_a_missing_error_code_to_unknown() {
    let line = r#"{"type":"turn/end","seq":3,"time":1,"data":{"turn":1,"reason":{"kind":"error","step":2,"message":"boom"}}}"#;
    let upgraded = upgrade(line, FormatVersion::V0).expect("error reason should migrate");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["data"]["reason"]["error"]["code"], "UNKNOWN");
}

#[test]
fn v0_steering_message_becomes_a_wrapped_user_message() {
    let upgraded = upgrade_in(V0_STEERING);
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["type"], "user/message");
    assert_eq!(parsed["data"]["id"], "legacy-message:s1:2");
    assert_eq!(parsed["data"]["role"], "user");
    assert_eq!(
        parsed["data"]["content"],
        serde_json::json!([{ "type": "text", "text": "hi" }])
    );
    assert_eq!(
        parsed["data"].get("turn"),
        None,
        "the steering turn is implied by position in v1"
    );
    assert_eq!(detect(&upgraded), Some(FormatVersion::V1));
}

#[test]
fn v0_tool_result_becomes_a_wrapper_message() {
    let line = r#"{"type":"tool/result","seq":5,"time":9,"data":{"callId":"call-1","content":[{"type":"text","text":"42"}],"isError":false}}"#;
    let upgraded = upgrade_in(line);
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["data"]["message"]["id"], "legacy-message:s1:5");
    assert_eq!(parsed["data"]["message"]["role"], "user");
    assert_eq!(
        parsed["data"]["message"]["source"],
        serde_json::json!({ "kind": "tool", "callId": "call-1" })
    );
    assert_eq!(
        parsed["data"]["message"]["content"],
        serde_json::json!([{
            "type": "tool-result",
            "toolCallId": "call-1",
            "content": [{ "type": "text", "text": "42" }],
            "isError": false,
        }])
    );
    assert_eq!(parsed["data"].get("callId"), None);
    assert_eq!(parsed["data"].get("isError"), None);
}

#[test]
fn a_v0_retry_gains_a_deterministic_id() {
    let line = r#"{"type":"llm/retry","seq":4,"time":1,"data":{"turn":1,"step":1,"provider":"deepseek","mode":"auto","policyKey":"p","retry":2,"delayMs":10,"failure":{"message":"x","code":"y"}}}"#;
    let first = upgrade_in(line);
    let parsed: serde_json::Value = serde_json::from_str(&first).expect("valid json");
    assert_eq!(parsed["data"]["retryId"], "legacy-retry:s1:4");
    // Deterministic: a second run must produce the same id, or references to it would dangle.
    assert_eq!(upgrade_in(line), first);
}

#[test]
fn a_v0_compaction_start_mints_the_groups_id() {
    let line = r#"{"type":"compact/start","seq":6,"time":1,"data":{"turn":1}}"#;
    let upgraded = upgrade_in(line);
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["type"], "compaction/start");
    assert_eq!(parsed["data"]["compactionId"], "legacy-compaction:s1:6");
}

#[test]
fn a_v0_request_header_drops_the_duplicated_message_prefix() {
    let line = r#"{"type":"request/header","seq":2,"time":1,"data":{"header":{"provider":"deepseek","model":"m","messagePrefix":[]},"reason":"initial"}}"#;
    let upgraded = upgrade(line, FormatVersion::V0).expect("header row should migrate");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(
        parsed["data"]["header"],
        serde_json::json!({ "provider": "deepseek", "model": "m" })
    );
}

#[test]
fn upgrading_an_already_upgraded_row_returns_it_unchanged() {
    // Every supported direction must be idempotent: a second pass over a half-migrated log is the
    // normal case after a crash, not an exotic one.
    let v0_rows = [V0_STEERING, V0_TURN_START, V0_TURN_END_DISPOSED];
    for line in v0_rows {
        let once = upgrade_in(line);
        let twice = Migrator::in_session("s1")
            .upgrade(&once, FormatVersion::V0)
            .expect("a v1 row under the v0 edge should pass through");
        assert_eq!(once, twice, "v0→v1 was not idempotent for {line}");
        assert_eq!(
            detect(&once),
            Some(FormatVersion::V1),
            "the converted row should be recognized as v1"
        );
    }

    for line in [V3_TOOL_RESULT, V3_PLUGIN_SOURCE, V3_EXTENSION_BLOCK] {
        let once = upgrade(line, FormatVersion::V3).expect("v3 row should migrate");
        let twice = upgrade(&once, FormatVersion::V3)
            .expect("an already-converted row should pass through");
        assert_eq!(once, twice, "v3→v4 was not idempotent for {line}");
        assert_eq!(
            detect(&twice),
            Some(FormatVersion::V4),
            "the converted row should be recognized as current"
        );
    }
}

#[test]
fn a_current_row_is_never_rewritten() {
    let line = r#"{"type":"turn/start","turn":1}"#;
    assert_eq!(
        upgrade(line, FormatVersion::V4).expect("a current row passes"),
        line
    );
}

#[test]
fn a_v3_tool_result_becomes_a_first_class_tool_message() {
    let upgraded = upgrade(V3_TOOL_RESULT, FormatVersion::V3).expect("tool result should lift");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    let message = &parsed["data"]["message"];
    assert_eq!(message["role"], "tool");
    assert_eq!(message["toolCallId"], "call-1");
    assert_eq!(message["id"], "m7");
    assert_eq!(
        message["source"],
        serde_json::json!({ "kind": "tool", "callId": "call-1" })
    );
    assert_eq!(
        message["content"],
        serde_json::json!([{ "type": "text", "text": "42" }]),
        "the wrapper is unwrapped, so the content is directly the block list"
    );
    assert_eq!(message["isError"], false);
    // The envelope keeps its coordinates: lifting a result must not renumber history.
    assert_eq!(parsed["seq"], 7);
    assert_eq!(parsed["time"], 200);
}

#[test]
fn a_v3_tool_result_keeps_metadata_from_both_owners_separately() {
    let line = r#"{"type":"tool/result","seq":7,"time":2,"data":{"message":{"id":"m7","role":"user","source":{"kind":"tool","callId":"c1"},"content":[{"type":"tool-result","toolCallId":"c1","content":[],"isError":true,"elapsedMs":5}],"traceId":"t"},"error":"boom"}}"#;
    let upgraded = upgrade(line, FormatVersion::V3).expect("tool result should lift");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    let message = &parsed["data"]["message"];
    // Owner-qualified, so a wrapper key cannot overwrite a message key of the same name.
    assert_eq!(message["plugin:message:traceId"], "t");
    assert_eq!(message["plugin:result:elapsedMs"], 5);
    assert_eq!(
        parsed["data"]["error"], "boom",
        "outer fields are untouched"
    );
}

#[test]
fn a_v3_plugin_source_becomes_its_producer_kind() {
    let upgraded = upgrade(V3_PLUGIN_SOURCE, FormatVersion::V3).expect("source should convert");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(
        parsed["data"]["source"],
        serde_json::json!({ "kind": "compact-checkpoint", "compactionId": "c1" }),
        "the plugin field is gone and every other field survives"
    );
}

#[test]
fn a_v3_unknown_plugin_source_keeps_its_complete_name() {
    let line = r#"{"type":"user/message","seq":1,"time":1,"data":{"id":"m1","role":"user","content":[],"source":{"kind":"plugin","plugin":"acme-widget"}}}"#;
    let upgraded = upgrade(line, FormatVersion::V3).expect("source should convert");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["data"]["source"]["kind"], "plugin:acme-widget");
}

#[test]
fn a_v3_system_prompt_source_depends_on_the_message_role() {
    let system = r#"{"type":"system/message","seq":1,"time":1,"data":{"message":{"id":"m1","role":"system","content":[],"source":{"kind":"plugin","plugin":"@deepseek-ai/dsh-system-prompt"}}}}"#;
    let upgraded = upgrade(system, FormatVersion::V3).expect("source should convert");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["data"]["message"]["source"]["kind"], "system-prompt");

    let user = r#"{"type":"user/message","seq":1,"time":1,"data":{"id":"m1","role":"user","content":[],"source":{"kind":"plugin","plugin":"@deepseek-ai/dsh-system-prompt"}}}"#;
    let upgraded = upgrade(user, FormatVersion::V3).expect("source should convert");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["data"]["source"]["kind"], "runtime-context");
}

#[test]
fn a_v3_extension_content_tag_is_namespaced() {
    let upgraded = upgrade(V3_EXTENSION_BLOCK, FormatVersion::V3).expect("content should convert");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["data"]["content"][0]["type"], "plugin:hologram");
    assert_eq!(parsed["data"]["content"][0]["payload"], 1);
}

#[test]
fn a_v3_known_content_tag_is_left_alone() {
    let line = r#"{"type":"user/message","seq":1,"time":1,"data":{"id":"m1","role":"user","content":[{"type":"text","text":"a"},{"type":"image","attachment":"x"}],"source":{"kind":"user"}}}"#;
    let upgraded = upgrade(line, FormatVersion::V3).expect("known tags need no conversion");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["data"]["content"][0]["type"], "text");
    assert_eq!(parsed["data"]["content"][1]["type"], "image");
}

#[test]
fn a_v3_stream_namespaces_both_of_its_block_shapes() {
    let line = r#"{"type":"assistant/message","seq":1,"time":1,"data":{"message":{"id":"m1","role":"assistant","content":[],"source":{"kind":"model"}},"stream":[{"type":"chunk","chunk":{"type":"block-start","blockType":"hologram"}},{"type":"chunk","chunk":{"type":"block-end","block":{"type":"hologram","x":1}}}]}}"#;
    let upgraded = upgrade(line, FormatVersion::V3).expect("stream should convert");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(
        parsed["data"]["stream"][0]["chunk"]["blockType"],
        "plugin:hologram"
    );
    assert_eq!(
        parsed["data"]["stream"][1]["chunk"]["block"]["type"],
        "plugin:hologram"
    );
}

#[test]
fn a_v3_ignorable_unknown_event_is_namespaced_with_its_payload_untouched() {
    let line =
        r#"{"type":"future/thing","seq":1,"time":1,"ignorable":true,"data":{"anything":[1,2,3]}}"#;
    let upgraded = upgrade(line, FormatVersion::V3).expect("ignorable event should be namespaced");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["type"], "plugin:future/thing");
    assert_eq!(parsed["ignorable"], true);
    assert_eq!(parsed["data"], serde_json::json!({ "anything": [1, 2, 3] }));
}

#[test]
fn a_v3_v4_header_advances_and_validates_its_required_fields() {
    let line = r#"{"type":"session","version":3,"id":"s1","createdAt":1,"isSeeded":false,"delegationDepth":0,"cwd":"/tmp","agentPreset":"ptc"}"#;
    let upgraded = upgrade(line, FormatVersion::V3).expect("header should migrate");
    let parsed: serde_json::Value = serde_json::from_str(&upgraded).expect("valid json");
    assert_eq!(parsed["version"], 4);
    assert_eq!(
        parsed["agentPreset"], "ptc",
        "a v3 preset is already current"
    );

    let missing = r#"{"type":"session","version":3,"id":"s1","createdAt":1}"#;
    let error = upgrade(missing, FormatVersion::V3).expect_err("a truncated header must refuse");
    assert!(
        matches!(error, MigrationError::Invalid { .. }),
        "got {error:?}"
    );
}

#[test]
fn an_unsupported_edge_names_what_it_would_need() {
    let v1_to_v2 = upgrade(V1_USER, FormatVersion::V1).expect_err("v1→v2 is not implemented");
    let MigrationError::Unsupported { from, to, reason } = &v1_to_v2 else {
        panic!("expected Unsupported, got {v1_to_v2:?}");
    };
    assert_eq!((*from, *to), (FormatVersion::V1, FormatVersion::V2));
    assert!(reason.contains("whole artifact"), "reason was {reason}");

    let v2_to_v3 = upgrade(V2_ASSISTANT, FormatVersion::V2).expect_err("v2→v3 is not implemented");
    let MigrationError::Unsupported { reason, .. } = &v2_to_v3 else {
        panic!("expected Unsupported, got {v2_to_v3:?}");
    };
    assert!(reason.contains("system-prompt head"), "reason was {reason}");
}

#[test]
fn every_generation_is_listed_oldest_first() {
    assert_eq!(FormatVersion::ALL.len(), 5);
    assert_eq!(FormatVersion::ALL[0], FormatVersion::V0);
    assert_eq!(FormatVersion::ALL[4], FormatVersion::CURRENT);
    for pair in FormatVersion::ALL.windows(2) {
        assert!(pair[0] < pair[1], "ALL must be ordered");
        assert_eq!(pair[0].next(), Some(pair[1]));
    }
}

#[test]
fn the_framework_reports_which_edges_it_can_perform() {
    let supported: Vec<_> = Migration::edges()
        .into_iter()
        .filter(|edge| edge.is_supported())
        .map(|edge| (edge.from, edge.to))
        .collect();
    assert_eq!(
        supported,
        vec![
            (FormatVersion::V0, FormatVersion::V1),
            (FormatVersion::V3, FormatVersion::V4),
        ]
    );
    for edge in Migration::edges() {
        assert_eq!(
            edge.is_supported(),
            edge.unsupported_reason().is_none(),
            "support and reason disagree for {edge:?}"
        );
    }
}

#[test]
fn a_row_that_needs_the_session_id_is_refused_with_the_alternative_named() {
    let error = upgrade(V0_STEERING, FormatVersion::V0)
        .expect_err("a bare row cannot mint a session-scoped id");
    let MigrationError::Unsupported { reason, .. } = &error else {
        panic!("expected Unsupported, got {error:?}");
    };
    assert!(reason.contains("upgrade_in_session"), "reason was {reason}");
}

#[test]
fn a_compaction_member_without_its_group_id_is_refused() {
    let line = r#"{"type":"compact/summary","seq":6,"time":1,"data":{"summary":[],"shadowedSeqs":[],"shadowedTokenCount":0,"provider":"p","model":"m","shadowedRange":{"start":0,"end":0}}}"#;
    let error = Migrator::in_session("s1")
        .upgrade(line, FormatVersion::V0)
        .expect_err("a compaction member cannot find its group id alone");
    let MigrationError::Unsupported { reason, .. } = &error else {
        panic!("expected Unsupported, got {error:?}");
    };
    assert!(reason.contains("compaction/start"), "reason was {reason}");
}

#[test]
fn retired_v0_events_are_refused_rather_than_dropped() {
    for line in [
        r#"{"type":"mode/set","seq":1,"time":1,"data":{"mode":"plan"}}"#,
        r#"{"type":"request/header-delta","seq":1,"time":1,"data":{}}"#,
    ] {
        let error = upgrade(line, FormatVersion::V0).expect_err("a retired event must refuse");
        assert!(
            matches!(error, MigrationError::Unsupported { .. }),
            "got {error:?} for {line}"
        );
    }
}

#[test]
fn a_request_header_with_the_retired_fallback_reason_is_refused() {
    let line =
        r#"{"type":"request/header","seq":1,"time":1,"data":{"header":{},"reason":"fallback"}}"#;
    let error = upgrade(line, FormatVersion::V0).expect_err("fallback has no current equivalent");
    assert!(
        matches!(error, MigrationError::Unsupported { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_v3_row_that_skipped_the_previous_edge_is_refused_as_a_skipped_edge() {
    // The pre-v3 replacement spelling means v2→v3 never ran; handing it to v3→v4 would convert a
    // row against the wrong baseline.
    let line = r#"{"type":"user/message","seq":1,"time":1,"data":{"id":"m1","role":"user","content":[],"source":{"kind":"user"},"surfaceOp":{"op":"replace","start":0,"end":0}}}"#;
    let error =
        upgrade(line, FormatVersion::V3).expect_err("a v2 row under the v3 edge must refuse");
    assert!(
        matches!(
            error,
            MigrationError::VersionMismatch {
                detected: FormatVersion::V2,
                ..
            }
        ),
        "got {error:?}"
    );
}

#[test]
fn a_v0_row_under_a_later_edge_is_refused_as_a_skipped_edge() {
    let error = upgrade(V0_STEERING, FormatVersion::V3).expect_err("v0 does not belong to v3");
    assert!(
        matches!(
            error,
            MigrationError::VersionMismatch {
                detected: FormatVersion::V0,
                ..
            }
        ),
        "got {error:?}"
    );
}

#[test]
fn a_v3_event_type_v3_never_defined_is_refused() {
    let line = r#"{"type":"nonsense/thing","seq":1,"time":1,"data":{}}"#;
    let error =
        upgrade(line, FormatVersion::V3).expect_err("an unknown required event must refuse");
    assert!(
        matches!(error, MigrationError::Unsupported { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_malformed_v3_tool_result_keeps_its_diagnostics() {
    let line = r#"{"type":"tool/result","seq":7,"time":2,"data":{"message":{"id":"m7","role":"user","source":{"kind":"tool","callId":"c1"},"content":[]}}}"#;
    let error = upgrade(line, FormatVersion::V3).expect_err("a wrapper-less result must refuse");
    assert!(
        matches!(error, MigrationError::Invalid { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_nested_tool_result_is_refused_rather_than_flattened() {
    let line = r#"{"type":"tool/result","seq":7,"time":2,"data":{"message":{"id":"m7","role":"user","source":{"kind":"tool","callId":"c1"},"content":[{"type":"tool-result","toolCallId":"c1","content":[{"type":"tool-result","toolCallId":"c1","content":[]}]}]}}}"#;
    let error = upgrade(line, FormatVersion::V3).expect_err("a nested result is unsupported");
    let MigrationError::Unsupported { reason, .. } = &error else {
        panic!("expected Unsupported, got {error:?}");
    };
    assert!(
        reason.contains("nests another tool result"),
        "reason was {reason}"
    );
}

#[test]
fn a_v3_request_header_with_defer_loading_is_refused() {
    let line = r#"{"type":"request/header","seq":1,"time":1,"data":{"header":{"provider":"p","model":"m","tools":[{"name":"t","deferLoading":true}]},"reason":"initial"}}"#;
    let error = upgrade(line, FormatVersion::V3).expect_err("deferLoading is v4-only");
    assert!(
        matches!(error, MigrationError::Unsupported { .. }),
        "got {error:?}"
    );
}

#[test]
fn an_unrecognized_row_cannot_be_upgraded_from_a_named_generation() {
    let error = upgrade("not json at all", FormatVersion::V0)
        .expect_err("a non-JSON line cannot be placed");
    assert!(
        matches!(error, MigrationError::Unrecognized { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_structurally_wrong_line_does_not_panic() {
    for line in [
        r#"{"type":"user/message","seq":1,"time":1}"#,
        r#"{"type":"user/message","seq":1,"time":1,"data":[]}"#,
        r#"{"type":"user/message","seq":"one","time":1,"data":{}}"#,
        r#"{"type":"assistant/message","seq":1,"time":1,"data":{"provenance":9}}"#,
        r#"{"type":"tool/result","seq":1,"time":1,"data":{"message":"no"}}"#,
        r#"{"type":"agent/inbox/spliced","seq":1,"time":1,"data":{"inserted":"no"}}"#,
    ] {
        for edge in [FormatVersion::V0, FormatVersion::V3] {
            // The property is the absence of a panic; the verdict may be either.
            let _ = upgrade(line, edge);
        }
    }
}

/// Upgrades a v0 row as a member of the session every fixture belongs to.
fn upgrade_in(line: &str) -> String {
    upgrade_in_session(line, FormatVersion::V0, "s1").expect("v0 row should migrate")
}
