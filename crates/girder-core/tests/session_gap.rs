//! Tests for session telemetry capture.
//!
//! Corresponds to upstream `packages/session/session-telemetry/tests/telemetry.spec.ts` and
//! `redact.spec.ts`, restricted to the capture mapping and policy chain this port covers (the
//! fiber lifecycle cases have no local counterpart, because this crate has no cordis context).
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/session_gap.rs"]
mod session_gap;

use girder_core::{SessionEvent, TurnEndReason};

use session_gap::{
    Attribute, Channel, Record, SessionFacts, Severity, SharingStatus, Verdict, agent_error,
    apply_policies, body_texts, capture, escalate, is_exported, plan_export_batches,
    redact_literal, resume_after, severity_of, shutdown, starts_history,
};

/// Facts for an ordinary, unseeded session.
fn facts() -> SessionFacts {
    SessionFacts {
        id: "s1".to_owned(),
        format_version: None,
        cwd: None,
        parent_id: None,
        seed_length: None,
    }
}

/// Attribute lookup by key, so a test names the fact it means rather than an index.
fn attr<'a>(record: &'a Record, key: &str) -> Option<&'a Attribute> {
    record
        .attributes
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

// ---- severity mapping -------------------------------------------------------------------------

#[test]
fn a_failed_tool_result_is_an_error() {
    let event = SessionEvent::ToolResult {
        id: girder_core::ToolCallId::new("call-1").unwrap_or_else(|_| unreachable!()),
        content: Vec::new(),
        is_error: true,
    };
    assert_eq!(severity_of(&event), Severity::Error);
}

#[test]
fn a_successful_tool_result_is_info() {
    let event = SessionEvent::ToolResult {
        id: girder_core::ToolCallId::new("call-1").unwrap_or_else(|_| unreachable!()),
        content: Vec::new(),
        is_error: false,
    };
    assert_eq!(severity_of(&event), Severity::Info);
}

#[test]
fn a_failed_turn_is_an_error_but_a_cancelled_one_is_not() {
    let failed = SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Failed {
            message: "boom".to_owned(),
            code: "UNKNOWN".to_owned(),
        },
    };
    assert_eq!(severity_of(&failed), Severity::Error);

    // Cancellation stops a turn on purpose; paging on it would alert on every Ctrl-C.
    let aborted = SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Aborted {
            cause: "user".to_owned(),
        },
    };
    assert_eq!(severity_of(&aborted), Severity::Info);

    let completed = SessionEvent::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    };
    assert_eq!(severity_of(&completed), Severity::Info);
}

#[test]
fn an_ordinary_event_falls_through_to_info() {
    // The fall-through is the merge-extensible rule: a type this build does not model must not
    // acquire an invented outcome.
    assert_eq!(
        severity_of(&SessionEvent::user_text("m1", "hello")),
        Severity::Info
    );
    assert_eq!(
        severity_of(&SessionEvent::StepStart { turn: 1, step: 1 }),
        Severity::Info
    );
}

// ---- ledger capture ---------------------------------------------------------------------------

#[test]
fn a_ledger_record_carries_envelope_identity_and_the_body_without_its_tag() {
    let record = capture(
        &facts(),
        7,
        1_700_000_000_000,
        &SessionEvent::user_text("m1", "hi"),
    );

    assert_eq!(record.channel, Channel::Ledger);
    assert_eq!(record.time, 1_700_000_000_000);
    assert_eq!(record.severity, Severity::Info);

    assert_eq!(
        attr(&record, "session.id"),
        Some(&Attribute::Text("s1".to_owned()))
    );
    assert_eq!(
        attr(&record, "event.type"),
        Some(&Attribute::Text("user/message".to_owned()))
    );
    assert_eq!(attr(&record, "event.seq"), Some(&Attribute::Number(7)));

    // The tag belongs in attributes; leaving it in the body shows the discriminator twice.
    assert!(record.body.get("type").is_none());
    assert_eq!(record.body["message"]["id"], "m1");
}

#[test]
fn header_facts_are_stamped_only_when_present() {
    let bare = capture(&facts(), 1, 0, &SessionEvent::user_text("m1", "hi"));
    assert!(attr(&bare, "session.cwd").is_none());
    assert!(attr(&bare, "session.parent_id").is_none());
    assert!(attr(&bare, "session.seed_length").is_none());
    assert!(attr(&bare, "session.format_version").is_none());

    let rich = SessionFacts {
        id: "child".to_owned(),
        format_version: Some(girder_core::FormatVersion::V4),
        cwd: Some("/work".to_owned()),
        parent_id: Some("parent".to_owned()),
        seed_length: Some(3),
    };
    let record = capture(&rich, 1, 0, &SessionEvent::user_text("m1", "hi"));
    assert_eq!(
        attr(&record, "session.format_version"),
        Some(&Attribute::Number(4))
    );
    assert_eq!(
        attr(&record, "session.cwd"),
        Some(&Attribute::Text("/work".to_owned()))
    );
    assert_eq!(
        attr(&record, "session.parent_id"),
        Some(&Attribute::Text("parent".to_owned()))
    );
    // The seed length is the exact inherited cut, which the parent id alone cannot express.
    assert_eq!(
        attr(&record, "session.seed_length"),
        Some(&Attribute::Number(3))
    );
}

#[test]
fn every_capturable_event_maps_to_a_record() {
    // A capture path that silently drops a variant loses a ledger row, so the count would stop
    // matching the log. All variants must therefore produce a body.
    let events = [
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::ModelUsage {
            model: "m".to_owned(),
            input_tokens: 1,
            output_tokens: 1,
        },
        SessionEvent::ToolCall {
            id: girder_core::ToolCallId::new("c").unwrap_or_else(|_| unreachable!()),
            name: "read".to_owned(),
            arguments: "{}".to_owned(),
        },
        SessionEvent::PlanMode { active: true },
        SessionEvent::SessionTitle {
            title: "t".to_owned(),
            message_seqs: vec![1],
            source: "derived".to_owned(),
        },
    ];
    for event in &events {
        let record = capture(&facts(), 1, 0, event);
        assert!(
            !record.body.is_null(),
            "{} produced a null body",
            event.type_name()
        );
    }
}

// ---- operational records ----------------------------------------------------------------------

#[test]
fn an_agent_error_is_an_ops_record_with_the_operational_attributes() {
    let record = agent_error(
        "s1",
        "agent-9",
        3,
        2,
        Some("TypeError"),
        "x is not a function",
        42,
    );

    assert_eq!(record.channel, Channel::Ops);
    assert_eq!(record.severity, Severity::Error);
    assert_eq!(record.time, 42);
    assert_eq!(
        attr(&record, "telemetry.op"),
        Some(&Attribute::Text("agent-error".to_owned()))
    );
    assert_eq!(
        attr(&record, "session.id"),
        Some(&Attribute::Text("s1".to_owned()))
    );
    assert_eq!(
        attr(&record, "agent.id"),
        Some(&Attribute::Text("agent-9".to_owned()))
    );
    assert_eq!(
        attr(&record, "error.name"),
        Some(&Attribute::Text("TypeError".to_owned()))
    );
    assert_eq!(attr(&record, "turn"), Some(&Attribute::Number(3)));
    assert_eq!(attr(&record, "step"), Some(&Attribute::Number(2)));
    assert_eq!(record.body["message"], "x is not a function");
}

#[test]
fn a_non_error_thrown_value_normalizes_to_the_error_name() {
    // The live bus can carry a bare string; upstream wraps it in `Error`, so the name is stable.
    let record = agent_error("s1", "a", 1, 1, None, "just a string", 0);
    assert_eq!(
        attr(&record, "error.name"),
        Some(&Attribute::Text("Error".to_owned()))
    );
    assert_eq!(record.body["name"], "Error");
}

#[test]
fn a_shutdown_marker_is_an_informational_ops_record() {
    let record = shutdown("s1", 99);
    assert_eq!(record.channel, Channel::Ops);
    // A clean exit is not a failure: this distinction is the whole point of the record.
    assert_eq!(record.severity, Severity::Info);
    assert_eq!(
        attr(&record, "telemetry.op"),
        Some(&Attribute::Text("shutdown".to_owned()))
    );
    assert_eq!(record.body["op"], "shutdown");
}

// ---- policy chain -----------------------------------------------------------------------------

#[test]
fn no_policy_passes_a_record_through_unchanged() {
    let record = capture(
        &facts(),
        1,
        0,
        &SessionEvent::user_text("m1", "key sk-secret"),
    );
    let applied = apply_policies(record.clone(), &[]);
    assert_eq!(applied, Some(record));
}

#[test]
fn policies_stack_outermost_first() {
    let record = capture(&facts(), 1, 0, &SessionEvent::user_text("m1", "hi"));
    let outer = |record: &Record| {
        let mut next = record.clone();
        next.attributes
            .push(("outer".to_owned(), Attribute::Number(1)));
        Some(Verdict::Pass(next))
    };
    let inner = |record: &Record| {
        let mut next = record.clone();
        next.attributes
            .push(("inner".to_owned(), Attribute::Number(1)));
        Some(Verdict::Pass(next))
    };

    let applied = apply_policies(record, &[&outer, &inner]).unwrap_or_else(|| unreachable!());
    // Both ran, and because the chain is outermost-first the outer rule's push lands first.
    assert_eq!(
        attr(&applied, "outer"),
        Some(&Attribute::Number(1)),
        "outer rule must run"
    );
    assert_eq!(attr(&applied, "inner"), Some(&Attribute::Number(1)));
    let outer_index = applied
        .attributes
        .iter()
        .position(|(name, _)| name == "outer")
        .unwrap_or_else(|| unreachable!());
    let inner_index = applied
        .attributes
        .iter()
        .position(|(name, _)| name == "inner")
        .unwrap_or_else(|| unreachable!());
    assert!(outer_index < inner_index);
}

#[test]
fn a_replacing_policy_stops_the_chain() {
    let record = capture(&facts(), 1, 0, &SessionEvent::user_text("m1", "hi"));
    let replacer = |_: &Record| {
        Some(Verdict::Replace(Record {
            channel: Channel::Ops,
            time: 0,
            severity: Severity::Info,
            attributes: Vec::new(),
            body: serde_json::json!("replaced"),
        }))
    };
    let below = |record: &Record| {
        let mut next = record.clone();
        next.attributes
            .push(("below".to_owned(), Attribute::Number(1)));
        Some(Verdict::Pass(next))
    };

    let applied = apply_policies(record, &[&replacer, &below]).unwrap_or_else(|| unreachable!());
    assert_eq!(applied.body, serde_json::json!("replaced"));
    // The rule beneath a replacement never runs, matching upstream's skipped `next()`.
    assert!(attr(&applied, "below").is_none());
}

#[test]
fn a_withholding_policy_drops_that_record_only() {
    // Upstream's fail-closed containment for a throwing rule: the record is withheld, nothing else.
    let record = capture(&facts(), 1, 0, &SessionEvent::user_text("m1", "hi"));
    let withhold = |_: &Record| None;
    assert_eq!(apply_policies(record, &[&withhold]), None);
}

// ---- redaction --------------------------------------------------------------------------------

#[test]
fn a_literal_secret_is_replaced_in_the_body_and_the_attributes() {
    let mut record = capture(
        &SessionFacts {
            id: "s1".to_owned(),
            cwd: Some("/tmp/sk-secret".to_owned()),
            ..facts()
        },
        1,
        0,
        &SessionEvent::user_text("m1", "key sk-secret here"),
    );
    record
        .attributes
        .push(("n".to_owned(), Attribute::Number(1)));

    let redacted = redact_literal(&record, "sk-secret", "[redacted]");
    let texts = body_texts(&redacted.body);
    assert!(
        texts.iter().all(|text| !text.contains("sk-secret")),
        "body still leaks the secret: {texts:?}"
    );
    assert_eq!(
        attr(&redacted, "session.cwd"),
        Some(&Attribute::Text("/tmp/[redacted]".to_owned()))
    );
    // The number attribute is untouched, and the original record is not mutated.
    assert_eq!(attr(&redacted, "n"), Some(&Attribute::Number(1)));
    assert!(
        body_texts(&record.body)
            .iter()
            .any(|text| text.contains("sk-secret"))
    );
}

#[test]
fn an_empty_secret_is_a_no_op_rather_than_a_match_everything() {
    let record = capture(&facts(), 1, 0, &SessionEvent::user_text("m1", "hi"));
    let redacted = redact_literal(&record, "", "[redacted]");
    assert_eq!(redacted, record);
}

// ---- sharing gate -----------------------------------------------------------------------------

#[test]
fn the_sharing_mode_decides_what_leaves_the_process() {
    let ledger = capture(&facts(), 1, 0, &SessionEvent::user_text("m1", "hi"));
    let ops = shutdown("s1", 0);

    assert!(is_exported(SharingStatus::Full, &ledger));
    assert!(is_exported(SharingStatus::Full, &ops));

    // Feedback-only keeps the fact that something happened, not what was said.
    assert!(!is_exported(SharingStatus::FeedbackOnly, &ledger));
    assert!(is_exported(SharingStatus::FeedbackOnly, &ops));

    assert!(!is_exported(SharingStatus::Disabled, &ledger));
    assert!(!is_exported(SharingStatus::Disabled, &ops));
}

// ---- batching ---------------------------------------------------------------------------------

/// A record with a body of predictable size.
fn sized(text: &str) -> Record {
    let mut record = capture(&facts(), 1, 0, &SessionEvent::user_text("m1", text));
    record.body = serde_json::json!({ "text": text });
    record
}

#[test]
fn a_batch_never_exceeds_its_byte_bound() {
    let records = vec![
        sized(&"a".repeat(100)),
        sized(&"b".repeat(100)),
        sized(&"c".repeat(100)),
    ];
    let (batches, oversized) = plan_export_batches(records, 400);

    assert!(oversized.is_empty());
    assert!(
        batches.len() > 1,
        "three 100-byte bodies cannot fit one 400-byte batch"
    );
    for batch in &batches {
        assert!(batch.bytes <= 400, "batch was {} bytes", batch.bytes);
        assert!(!batch.records.is_empty());
    }
    assert_eq!(
        batches
            .iter()
            .map(|batch| batch.records.len())
            .sum::<usize>(),
        3,
        "every record is placed exactly once"
    );
}

#[test]
fn batching_preserves_capture_order() {
    let texts = ["first", "second", "third", "fourth"];
    let records: Vec<Record> = texts.iter().map(|text| sized(text)).collect();
    let (batches, _) = plan_export_batches(records, 200);

    let ordered: Vec<String> = batches
        .iter()
        .flat_map(|batch| batch.records.iter())
        .map(|record| record.body["text"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(ordered, texts);
}

#[test]
fn a_record_larger_than_the_bound_is_reported_rather_than_dropped() {
    let records = vec![sized("small"), sized(&"x".repeat(5000)), sized("tail")];
    let (batches, oversized) = plan_export_batches(records, 200);

    assert_eq!(oversized.len(), 1);
    assert_eq!(oversized[0].index, 1);
    assert!(oversized[0].bytes > 200);
    // The two ordinary records still ship.
    assert_eq!(
        batches
            .iter()
            .map(|batch| batch.records.len())
            .sum::<usize>(),
        2
    );
}

#[test]
fn a_zero_bound_means_no_limit() {
    let records = vec![sized(&"a".repeat(100)), sized(&"b".repeat(100))];
    let (batches, oversized) = plan_export_batches(records, 0);
    assert!(oversized.is_empty());
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].records.len(), 2);
}

#[test]
fn no_records_yield_no_batches() {
    let (batches, oversized) = plan_export_batches(Vec::new(), 100);
    assert!(batches.is_empty());
    assert!(oversized.is_empty());
}

// ---- capture windows --------------------------------------------------------------------------

#[test]
fn history_is_included_only_when_asked_for_or_when_the_log_is_not_seeded() {
    // An unseeded log has no inherited prefix, so both modes see everything.
    assert!(starts_history(false, None));
    assert!(starts_history(true, None));
    // A seeded log's inherited prefix is history; excluding it must skip those events.
    assert!(!starts_history(false, Some(5)));
    assert!(starts_history(true, Some(5)));
}

#[test]
fn a_missing_handoff_cursor_re_hands_from_the_start() {
    // Re-handing duplicate history is recoverable; silently skipping it is not.
    assert_eq!(resume_after(None), 0);
    assert_eq!(resume_after(Some(12)), 12);
}

#[test]
fn a_policy_can_escalate_a_record_but_never_downgrade_one() {
    let benign = capture(&facts(), 1, 0, &SessionEvent::user_text("m1", "hi"));
    assert_eq!(escalate(&benign).severity, Severity::Warn);

    // An error stays an error: a rule noticing something odd about a failure must not bury it.
    let failing = capture(
        &facts(),
        1,
        0,
        &SessionEvent::ToolResult {
            id: girder_core::ToolCallId::new("c").expect("test ids are valid"),
            content: Vec::new(),
            is_error: true,
        },
    );
    assert_eq!(escalate(&failing).severity, Severity::Error);
}
