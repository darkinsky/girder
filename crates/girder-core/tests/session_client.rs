//! Tests for the client stream transport of upstream `packages/api/session-controller`.
//!
//! Corresponds to `packages/api/session-controller/tests/transport.client.spec.ts` (725 lines, 14
//! cases) and the envelope-acceptance cases its "preserves current envelopes and payloads without
//! normalization across every journal path" case drives.
//!
//! # What each property protects
//!
//! - **A revision gap is recoverable, a missing baseline is not.** The first means "we lost transient
//!   frames, re-open"; the second means "the peer ignored the request". Collapsing them either loops
//!   forever on a broken Host or gives up on a healthy one.
//! - **A second opening in one generation is a violation.** Every later revision comparison would be
//!   against the wrong base.
//! - **An entry at or behind the cursor is stale, not an append.** Accepting it duplicates a row or
//!   silently rewinds the window.
//! - **`-0` is not `0`.** `serde_json`'s `as_i64` reports `-0` as `Some(0)`, so a numeric check accepts
//!   a producer bug the wire specifically forbids.
//! - **A surface replacement may only shadow earlier events.** A range reaching the event's own seq
//!   lets an event shadow itself, and the fold stops terminating.
//! - **A page is accepted whole.** Accepting up to the first bad record publishes a window that ends
//!   early while still reporting the peer's `has_more`, stranding everything between.
//! - **Unknown fields are refused; unknown event types are not.** An extra field means a peer speaking
//!   a different protocol; an unknown type is merge-extensible and may be ignorable.
//! - **A replacement page is read to its cursor, not to its bound.** Keeping `beforeSeq` across a
//!   repair would make the repair unreachable.
//! - **A generation end is classified by whether its baseline arrived.** That is the difference
//!   between reconnecting quietly and surfacing a broken Host.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than a
//! copy. Inside a `#[path]` module the `crate::` prefix names *this* test crate, not `girder-core`, so
//! every reference to the library goes through `girder_core::{...}` — including the ones the module
//! under test makes, which is why the module itself spells them that way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]
#![allow(
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "a test that indexes its own fixture is clearer than one that re-derives bounds"
)]

#[path = "../src/session/client.rs"]
pub mod session_client_gap;

use std::collections::BTreeMap;

use serde_json::{Value, json};

use girder_core::SurfaceOp;
use girder_core::session::controller::TurnWindow;
use girder_core::session::snapshot::{ControlBaseline, ControlFrame, ProjectionUpdate};

use session_client_gap::{
    AssistantFrame, AssistantRevision, ClientPageRequest, ControlAction, ControlError,
    ControlStream, EndKind, FollowChange, FollowFrame, FollowOptions, JournalStream, RevisionStep,
    StreamError, WireError, WireEvent, WirePage, WireRecord, assert_session_wire_event,
};

// ---- fixtures -------------------------------------------------------------------------------

/// A surface-eligible `user/message` envelope with an append marker, at `seq`.
fn surface_event(seq: u64) -> Value {
    json!({
        "type": "user/message",
        "seq": seq,
        "time": 1_700_000_000_000_i64 + i64::try_from(seq).unwrap(),
        "data": { "message": { "role": "user", "content": [] } },
        "surfaceOp": "append",
    })
}

/// A log-only event envelope, valid by construction.
fn log_event(type_name: &str, seq: u64) -> Value {
    json!({
        "type": type_name,
        "seq": seq,
        "time": 1_700_000_000_000_i64,
        "data": {},
    })
}

/// A surface-eligible `developer/message` envelope with an append marker, at `seq`.
fn developer(seq: u64) -> Value {
    json!({
        "type": "developer/message",
        "seq": seq,
        "time": 1_700_000_000_000_i64,
        "data": {},
        "surfaceOp": "append",
    })
}

fn record(seq: u64) -> WireRecord {
    WireRecord::event(WireEvent::accept(&surface_event(seq)).unwrap())
}

fn page(seqs: impl IntoIterator<Item = u64>, has_more: bool) -> WirePage {
    WirePage {
        records: seqs.into_iter().map(record).collect(),
        has_more,
    }
}

fn snapshot(cursor: u64, page: WirePage, assistant_revision: Option<u64>) -> FollowFrame {
    FollowFrame::Snapshot {
        cursor: Some(cursor),
        page,
        assistant_revision,
    }
}

fn empty_snapshot(page: WirePage, assistant_revision: Option<u64>) -> FollowFrame {
    FollowFrame::Snapshot {
        cursor: None,
        page,
        assistant_revision,
    }
}

fn opened_stream() -> JournalStream {
    JournalStream::new("session event stream", FollowOptions::upstream_default())
}

// ---- envelope acceptance --------------------------------------------------------------------

#[test]
fn accepts_a_current_surface_envelope_without_normalizing_it() {
    let raw = json!({
        "type": "user/message",
        "seq": 2,
        "time": 1_700_000_000_000_i64,
        "data": { "message": { "role": "user", "content": [] }, "extra": { "retained": true } },
        "surfaceOp": { "op": "replace", "startSeq": 0, "endSeq": 1 },
        "sourceEventSeqs": [0, 1],
    });
    let before = raw.clone();
    let event = WireEvent::accept(&raw).unwrap();

    assert_eq!(event.type_name, "user/message");
    assert_eq!(event.seq, 2);
    assert_eq!(event.time, 1_700_000_000_000);
    assert!(!event.ignorable);
    assert_eq!(
        event.surface_op,
        Some(SurfaceOp::Replace { start: 0, end: 1 })
    );
    assert_eq!(event.source_event_seqs, Some(json!([0, 1])));
    // The payload keeps fields this build does not otherwise interpret.
    assert_eq!(event.data["extra"], json!({ "retained": true }));
    assert_eq!(
        raw, before,
        "acceptance must not mutate the value it validated"
    );
}

#[test]
fn accepts_an_unknown_event_type_only_when_it_is_ignorable() {
    let event = WireEvent::accept(&json!({
        "type": "extension/event",
        "seq": 10,
        "time": 10,
        "data": { "nested": [null, true] },
        "ignorable": true,
    }))
    .unwrap();
    assert!(event.ignorable);
    assert_eq!(event.surface_op, None);

    // An unknown ignorable row keeps *opaque* metadata without touching the surface: this build cannot
    // decode the type, so it cannot rule on what the metadata means, and the row never changes the
    // surface anyway.
    let opaque = WireEvent::accept(&json!({
        "type": "extension/event",
        "seq": 10,
        "time": 10,
        "data": { "nested": [null, true] },
        "ignorable": true,
        "surfaceOp": { "opaque": ["retained"] },
        "sourceEventSeqs": { "opaque": [null] },
    }))
    .unwrap();
    assert_eq!(opaque.surface_op, None, "opaque metadata is not decoded");
    assert_eq!(opaque.source_event_seqs, Some(json!({ "opaque": [null] })));

    // A *known* non-surface type gets no such latitude: `ignorable` excuses an undecodable event, not
    // a misplaced marker on one this build understands.
    let mut known_ignorable = log_event("turn/start", 10);
    known_ignorable["ignorable"] = json!(true);
    known_ignorable["surfaceOp"] = json!("append");
    assert_eq!(
        WireEvent::accept(&known_ignorable).unwrap_err(),
        WireError::InvalidSurfaceOp {
            detail: "\"turn/start\" is not surface-eligible".to_owned()
        }
    );

    // Without `ignorable` an unknown type is refused outright, because a reader that skipped it would
    // be dropping an event it could not interpret.
    let mut unmarked = json!({
        "type": "extension/event",
        "seq": 10,
        "time": 10,
        "data": { "nested": [null, true] },
        "surfaceOp": { "opaque": ["retained"] },
    });
    // Without `ignorable` the same unknown type carrying placement metadata is refused, because this
    // build would have to interpret a marker on a type it cannot decode.
    assert_eq!(
        WireEvent::accept(&unmarked).unwrap_err(),
        WireError::InvalidSurfaceOp {
            detail: "\"extension/event\" is not surface-eligible".to_owned()
        }
    );
    // Once the metadata is gone, the row is an ordinary opaque event and is accepted: event names are
    // merge-extensible, so an unknown *type* alone is not a refusal.
    unmarked.as_object_mut().unwrap().remove("surfaceOp");
    assert!(WireEvent::accept(&unmarked).is_ok());
}

#[test]
fn rejects_an_unknown_field() {
    let mut raw = log_event("turn/start", 1);
    raw["surprise"] = json!(true);
    assert_eq!(
        WireEvent::accept(&raw).unwrap_err(),
        WireError::UnexpectedField {
            field: "surprise".to_owned()
        }
    );
}

#[test]
fn rejects_a_bad_envelope() {
    assert_eq!(
        WireEvent::accept(&json!([])).unwrap_err(),
        WireError::NotAnObject
    );
    assert_eq!(
        WireEvent::accept(&json!({ "seq": 0, "time": 0, "data": {} })).unwrap_err(),
        WireError::MissingType
    );
    assert_eq!(
        WireEvent::accept(&json!({ "type": "turn/start", "time": 0, "data": {} })).unwrap_err(),
        WireError::InvalidSeq
    );
    assert_eq!(
        WireEvent::accept(&json!({ "type": "turn/start", "seq": 0, "data": {} })).unwrap_err(),
        WireError::InvalidTime
    );
    // `data: null` is upstream's "no data" case, distinct from a missing `data` key.
    assert_eq!(
        WireEvent::accept(&json!({ "type": "turn/start", "seq": 0, "time": 0, "data": null }))
            .unwrap_err(),
        WireError::MissingData
    );
    // `ignorable: false` is not `ignorable: true`; upstream only ever writes the latter.
    assert_eq!(
        WireEvent::accept(&json!({
            "type": "turn/start", "seq": 0, "time": 0, "data": {}, "ignorable": false,
        }))
        .unwrap_err(),
        WireError::InvalidIgnorable
    );
}

#[test]
fn rejects_negative_zero_seq_and_accepts_plain_zero() {
    // Read from text so the token survives; `json!(-0)` would lose the sign at the macro level.
    let negative: Value =
        serde_json::from_str(r#"{"type":"turn/start","seq":-0,"time":0,"data":{}}"#).unwrap();
    assert_eq!(
        WireEvent::accept(&negative).unwrap_err(),
        WireError::InvalidSeq,
        "-0 is a distinct JavaScript value from 0 and upstream rejects it"
    );

    let zero: Value =
        serde_json::from_str(r#"{"type":"turn/start","seq":0,"time":0,"data":{}}"#).unwrap();
    assert_eq!(WireEvent::accept(&zero).unwrap().seq, 0);
}

#[test]
fn rejects_a_non_integer_or_unsafe_seq() {
    for raw in [
        r#"{"type":"turn/start","seq":1.5,"time":0,"data":{}}"#,
        r#"{"type":"turn/start","seq":-3,"time":0,"data":{}}"#,
        r#"{"type":"turn/start","seq":9007199254740992,"time":0,"data":{}}"#,
    ] {
        let value: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            WireEvent::accept(&value).unwrap_err(),
            WireError::InvalidSeq,
            "rejected: {raw}"
        );
    }
    // The largest safe integer is still a safe integer.
    let boundary: Value =
        serde_json::from_str(r#"{"type":"turn/start","seq":9007199254740991,"time":0,"data":{}}"#)
            .unwrap();
    assert_eq!(
        WireEvent::accept(&boundary).unwrap().seq,
        9_007_199_254_740_991
    );
}

#[test]
fn requires_a_surface_marker_on_a_surface_eligible_event() {
    let mut raw = surface_event(1);
    raw.as_object_mut().unwrap().remove("surfaceOp");
    assert_eq!(
        WireEvent::accept(&raw).unwrap_err(),
        WireError::MissingSurfaceOp {
            type_name: "user/message".to_owned()
        }
    );
}

#[test]
fn refuses_surface_metadata_on_a_non_surface_event() {
    let mut with_op = log_event("turn/start", 3);
    with_op["surfaceOp"] = json!("append");
    assert_eq!(
        WireEvent::accept(&with_op).unwrap_err(),
        WireError::InvalidSurfaceOp {
            detail: "\"turn/start\" is not surface-eligible".to_owned()
        }
    );

    let mut with_sources = log_event("turn/start", 3);
    with_sources["sourceEventSeqs"] = json!([0]);
    assert_eq!(
        WireEvent::accept(&with_sources).unwrap_err(),
        WireError::UnexpectedSources {
            type_name: "turn/start".to_owned()
        }
    );
}

#[test]
fn refuses_a_replacement_that_does_not_reference_only_earlier_events() {
    let mut self_shadowing = surface_event(5);
    self_shadowing["surfaceOp"] = json!({ "op": "replace", "startSeq": 0, "endSeq": 5 });
    assert_eq!(
        WireEvent::accept(&self_shadowing).unwrap_err(),
        WireError::InvalidSurfaceOp {
            detail: "surface replace at seq 5: startSeq and endSeq must reference earlier events"
                .to_owned()
        }
    );

    let mut ahead = surface_event(5);
    ahead["surfaceOp"] = json!({ "op": "replace", "startSeq": 0, "endSeq": 6 });
    assert!(matches!(
        WireEvent::accept(&ahead).unwrap_err(),
        WireError::InvalidSurfaceOp { .. }
    ));
}

#[test]
fn refuses_a_replace_marker_that_is_not_exactly_three_keys() {
    let mut extra = surface_event(5);
    extra["surfaceOp"] = json!({ "op": "replace", "startSeq": 0, "endSeq": 1, "extra": 1 });
    assert_eq!(
        WireEvent::accept(&extra).unwrap_err(),
        WireError::InvalidSurfaceOp {
            detail: "a replace marker must have exactly op, startSeq and endSeq".to_owned()
        }
    );

    let mut wrong_op = surface_event(5);
    wrong_op["surfaceOp"] = json!({ "op": "splice", "startSeq": 0, "endSeq": 1 });
    assert!(matches!(
        WireEvent::accept(&wrong_op).unwrap_err(),
        WireError::InvalidSurfaceOp { .. }
    ));
}

#[test]
fn enforces_the_source_sequence_rules() {
    // Empty arrays are refused: an empty provenance list says nothing.
    let mut empty = surface_event(5);
    empty["sourceEventSeqs"] = json!([]);
    assert_eq!(
        WireEvent::accept(&empty).unwrap_err(),
        WireError::InvalidSources {
            detail: "sourceEventSeqs must not be empty".to_owned()
        }
    );

    // Duplicates are refused, because provenance is a set written as an array.
    let mut duplicated = surface_event(5);
    duplicated["sourceEventSeqs"] = json!([1, 1]);
    assert_eq!(
        WireEvent::accept(&duplicated).unwrap_err(),
        WireError::InvalidSources {
            detail: "sourceEventSeqs must not contain duplicates".to_owned()
        }
    );

    // A source at or past this event cannot have existed when the event was written.
    let mut future = surface_event(5);
    future["sourceEventSeqs"] = json!([1, 5]);
    assert_eq!(
        WireEvent::accept(&future).unwrap_err(),
        WireError::InvalidSources {
            detail: "sourceEventSeqs must reference earlier events: 5 >= current seq 5".to_owned()
        }
    );

    // A non-integer entry is not a sequence.
    let mut fractional = surface_event(5);
    fractional["sourceEventSeqs"] = json!([1.5]);
    assert!(matches!(
        WireEvent::accept(&fractional).unwrap_err(),
        WireError::InvalidSources { .. }
    ));
}

#[test]
fn refuses_sources_on_an_assistant_message() {
    // An assistant message embeds the stream it was assembled from, so extra provenance would be a
    // second, contradicting record of the same derivation.
    let mut raw = json!({
        "type": "assistant/message",
        "seq": 5,
        "time": 10,
        "data": { "message": { "role": "assistant", "content": [] } },
        "surfaceOp": "append",
        "sourceEventSeqs": [1],
    });
    assert_eq!(
        WireEvent::accept(&raw).unwrap_err(),
        WireError::InvalidSources {
            detail: "assistant/message embeds its source stream and cannot carry sourceEventSeqs"
                .to_owned()
        }
    );
    raw.as_object_mut().unwrap().remove("sourceEventSeqs");
    assert!(WireEvent::accept(&raw).is_ok());
}

#[test]
fn enforces_tool_change_blocks_belonging_to_a_developer_message() {
    let addition = json!([{ "type": "tool-addition", "toolName": "grep" }]);
    // A tool-change block on a non-developer message is refused: the block is only meaningful with
    // the developer role that pairs it with a header.
    // A message-carrying surface type puts the role on `data.message`; a `user/message` puts it on
    // `data` itself, which is upstream's `type === 'user/message' ? data : data.message`.
    let mut change_on_assistant = log_event("system/message", 5);
    change_on_assistant["surfaceOp"] = json!("append");
    change_on_assistant["data"] = json!({
        "message": { "role": "assistant", "content": addition },
    });
    assert_eq!(
        WireEvent::accept(&change_on_assistant).unwrap_err(),
        WireError::InvalidPayload {
            detail: "system/message tool-change blocks require the developer role".to_owned()
        }
    );

    // The developer role on a user message is the other half of the same rule: the type and the role
    // occur together or not at all.
    let mut wrong_role = surface_event(5);
    wrong_role["data"] = json!({ "role": "developer", "content": [] });
    assert_eq!(
        WireEvent::accept(&wrong_role).unwrap_err(),
        WireError::InvalidPayload {
            detail: "session wire event developer/message and developer role must occur together"
                .to_owned()
        }
    );
    // ...and a developer/message without the developer role is the mirror image.
    let mut missing_role = surface_event(5);
    missing_role["type"] = json!("assistant/message");
    missing_role["data"] = json!({ "message": { "role": "assistant", "content": [] } });
    let mut developer_marker = log_event("developer/message", 5);
    developer_marker["surfaceOp"] = json!("append");
    developer_marker["data"] = json!({ "message": { "role": "assistant", "content": [] } });
    assert_eq!(
        WireEvent::accept(&developer_marker).unwrap_err(),
        WireError::InvalidPayload {
            detail: "developer/message requires the developer role".to_owned()
        }
    );
    assert!(
        WireEvent::accept(&missing_role).is_ok(),
        "the same role on the right type is fine"
    );

    let mut developer_with_untitled_tool = developer(5);
    developer_with_untitled_tool["data"] = json!({
        "message": { "role": "developer", "content": [{ "type": "tool-addition", "toolName": "" }] },
    });
    assert_eq!(
        WireEvent::accept(&developer_with_untitled_tool).unwrap_err(),
        WireError::InvalidPayload {
            detail: "developer/message tool change requires a nonempty toolName".to_owned()
        }
    );

    let mut inline_definition = developer(5);
    inline_definition["data"] = json!({
        "message": { "role": "developer", "content": [
            { "type": "tool-addition", "toolName": "grep", "tool": { "name": "grep" } },
        ] },
        "headerSeq": 2,
    });
    assert_eq!(
        WireEvent::accept(&inline_definition).unwrap_err(),
        WireError::InvalidPayload {
            detail: "developer/message tool-addition must omit inline tool definitions".to_owned()
        }
    );

    // `headerSeq` appears exactly when there is an addition to resolve against it.
    let mut missing_header = developer(5);
    missing_header["data"] = json!({
        "message": { "role": "developer", "content": addition },
    });
    assert!(matches!(
        WireEvent::accept(&missing_header).unwrap_err(),
        WireError::InvalidPayload { .. }
    ));

    let mut stray_header = developer(5);
    stray_header["data"] = json!({
        "message": { "role": "developer", "content": [] },
        "headerSeq": 2,
    });
    assert!(matches!(
        WireEvent::accept(&stray_header).unwrap_err(),
        WireError::InvalidPayload { .. }
    ));

    // The complete, correct shape is accepted.
    let mut valid = developer(5);
    valid["data"] = json!({
        "message": { "role": "developer", "content": addition },
        "headerSeq": 2,
    });
    assert!(WireEvent::accept(&valid).is_ok());
}

#[test]
fn enforces_request_header_shape() {
    let mut empty_tools = log_event("request/header", 3);
    empty_tools["data"] = json!({
        "reason": "initial",
        "header": { "config": { "provider": "mock", "model": "mock" }, "tools": [] },
    });
    assert_eq!(
        WireEvent::accept(&empty_tools).unwrap_err(),
        WireError::InvalidPayload {
            detail: "request/header must omit empty tools".to_owned()
        }
    );

    let mut with_system = log_event("request/header", 3);
    with_system["data"] = json!({
        "header": { "config": {}, "system": "prompt" },
    });
    assert_eq!(
        WireEvent::accept(&with_system).unwrap_err(),
        WireError::InvalidPayload {
            detail: "request/header must omit header.system; use system/message".to_owned()
        }
    );

    let mut empty_defaults = log_event("request/header", 3);
    empty_defaults["data"] = json!({
        "header": { "config": {}, "adapterDefaults": {} },
    });
    assert_eq!(
        WireEvent::accept(&empty_defaults).unwrap_err(),
        WireError::InvalidPayload {
            detail: "request/header must omit empty adapterDefaults".to_owned()
        }
    );

    let mut valid = log_event("request/header", 3);
    valid["data"] = json!({
        "reason": "initial",
        "header": { "config": { "provider": "mock", "model": "mock" }, "tools": [{ "name": "fixture" }] },
    });
    assert!(WireEvent::accept(&valid).is_ok());
}

#[test]
fn enforces_tool_result_error_shape() {
    let mut unmarked = json!({
        "type": "tool/result",
        "seq": 4,
        "time": 10,
        "data": { "message": { "role": "tool", "content": [], "isError": false }, "error": { "code": "X" } },
        "surfaceOp": "append",
    });
    assert_eq!(
        WireEvent::accept(&unmarked).unwrap_err(),
        WireError::InvalidPayload {
            detail: "tool/result error requires message.isError === true".to_owned()
        }
    );

    unmarked["data"]["message"]["isError"] = json!(true);
    assert!(WireEvent::accept(&unmarked).is_ok());

    // No error at all needs no marking.
    let mut no_error = unmarked.clone();
    no_error["data"].as_object_mut().unwrap().remove("error");
    assert!(WireEvent::accept(&no_error).is_ok());
}

#[test]
fn the_free_function_is_the_constructor() {
    let value = surface_event(1);
    assert_eq!(
        assert_session_wire_event(&value).unwrap(),
        WireEvent::accept(&value).unwrap()
    );
}

// ---- records --------------------------------------------------------------------------------

#[test]
fn accepts_a_record_and_reads_its_range() {
    let raw = json!({ "type": "event", "event": surface_event(7) });
    let record = WireRecord::accept(&raw).unwrap();
    assert_eq!(record.first_seq(), 7);
    assert_eq!(record.last_seq(), 7);

    let round_trip = WireRecord::event(record.event.clone());
    assert_eq!(round_trip, record);
}

#[test]
fn refuses_a_record_wrapper_that_is_not_the_event_discriminator() {
    assert_eq!(
        WireRecord::accept(&json!({ "type": "transient", "event": surface_event(1) })).unwrap_err(),
        WireError::MissingType
    );
    assert_eq!(
        WireRecord::accept(&json!({ "event": surface_event(1) })).unwrap_err(),
        WireError::MissingType
    );
    let mut extra = json!({ "type": "event", "event": surface_event(1) });
    extra["note"] = json!("extra");
    assert_eq!(
        WireRecord::accept(&extra).unwrap_err(),
        WireError::UnexpectedField {
            field: "note".to_owned()
        }
    );
}

// ---- pages ----------------------------------------------------------------------------------

#[test]
fn accepts_a_page_whole_and_exposes_its_entries() {
    let raw = json!({
        "records": [
            { "type": "event", "event": surface_event(2) },
            { "type": "event", "event": surface_event(3) },
        ],
        "hasMore": true,
    });
    let accepted = WirePage::accept(&raw).unwrap();
    assert_eq!(accepted.tail_cursor(), Some(3));
    assert!(accepted.has_more);
    assert_eq!(accepted.entries().len(), 2);
    assert_eq!(accepted.index_from(3), 1);
    assert_eq!(accepted.index_from(9), 2);
}

#[test]
fn refuses_a_page_that_contains_one_bad_record() {
    // Accepting up to the bad record would publish a window that ends early while still reporting the
    // peer's `hasMore`, stranding every row in between.
    let raw = json!({
        "records": [
            { "type": "event", "event": surface_event(2) },
            { "type": "event", "event": { "type": "user/message", "seq": 3, "time": 0, "data": {} } },
        ],
        "hasMore": true,
    });
    assert!(matches!(
        WirePage::accept(&raw).unwrap_err(),
        WireError::MissingSurfaceOp { .. }
    ));
}

#[test]
fn an_empty_page_has_no_cursor() {
    let accepted = WirePage::accept(&json!({ "records": [], "hasMore": false })).unwrap();
    assert_eq!(accepted.tail_cursor(), None);
    assert!(!accepted.has_more);
}

// ---- page requests ---------------------------------------------------------------------------

#[test]
fn a_repair_request_drops_the_bound_but_keeps_the_window() {
    let request = ClientPageRequest {
        before_seq: Some(40),
        max_messages: Some(500),
        turn_window: Some(TurnWindow {
            min_messages: 50,
            min_turns: 2,
        }),
    };
    let repair = request.repair_request();
    assert_eq!(repair.before_seq, None, "a repair must reach its cursor");
    assert_eq!(repair.max_messages, Some(500));
    assert_eq!(repair.turn_window, request.turn_window);

    // An unbounded request repairs to itself.
    assert_eq!(
        ClientPageRequest::unbounded().repair_request(),
        ClientPageRequest::unbounded()
    );
}

#[test]
fn rejects_a_request_whose_window_cannot_be_honoured() {
    let zero_budget = ClientPageRequest {
        max_messages: Some(0),
        ..ClientPageRequest::unbounded()
    };
    assert_eq!(
        zero_budget.validate().unwrap_err().code(),
        "gateway/bad-request"
    );

    let over_budget = ClientPageRequest {
        max_messages: Some(50),
        turn_window: Some(TurnWindow {
            min_messages: 51,
            min_turns: 1,
        }),
        ..ClientPageRequest::unbounded()
    };
    assert_eq!(
        over_budget.validate().unwrap_err().code(),
        "gateway/bad-request"
    );

    let zero_min_messages = ClientPageRequest {
        turn_window: Some(TurnWindow {
            min_messages: 0,
            min_turns: 1,
        }),
        ..ClientPageRequest::unbounded()
    };
    assert!(zero_min_messages.validate().is_err());

    assert_eq!(
        ClientPageRequest {
            max_messages: Some(500),
            turn_window: Some(TurnWindow {
                min_messages: 50,
                min_turns: 2,
            }),
            ..ClientPageRequest::unbounded()
        }
        .validate()
        .unwrap()
        .max_messages,
        Some(500)
    );
}

// ---- assistant revision continuity ----------------------------------------------------------

#[test]
fn revisions_are_dense_and_a_skip_asks_for_a_rebaseline() {
    let mut revision = AssistantRevision::opened_at(0);
    assert!(revision.is_opened());
    assert_eq!(revision.expected(), 1);

    assert_eq!(revision.observe(1), RevisionStep::Continue);
    assert_eq!(revision.revision(), 1);
    assert_eq!(revision.expected(), 2);

    // A skip names both the revision that was due and the one that arrived.
    assert_eq!(
        revision.observe(3),
        RevisionStep::Rebaseline {
            expected: 2,
            received: 3
        }
    );
    // A refused observation must not advance the counter, or the next real frame would also be wrong.
    assert_eq!(revision.revision(), 1);
}

#[test]
fn an_unopened_revision_counter_is_not_a_base() {
    let revision = AssistantRevision::unopened();
    assert!(!revision.is_opened());
    assert_eq!(revision.revision(), 0);
}

#[test]
fn reads_an_assistant_frame_shape() {
    for kind in ["start", "chunk", "end"] {
        let frame = AssistantFrame::accept(&json!({ "type": kind, "revision": 7 })).unwrap();
        assert_eq!(frame.revision, 7);
    }
    assert!(matches!(
        AssistantFrame::accept(&json!({ "type": "paused", "revision": 1 })).unwrap_err(),
        StreamError::MalformedAssistant { .. }
    ));
    assert!(matches!(
        AssistantFrame::accept(&json!({ "type": "start" })).unwrap_err(),
        StreamError::MalformedAssistant { .. }
    ));
    assert!(matches!(
        AssistantFrame::accept(&json!([])).unwrap_err(),
        StreamError::MalformedAssistant { .. }
    ));
}

// ---- the journal stream ---------------------------------------------------------------------

#[test]
fn publishes_replace_append_and_assistant_changes_in_order() {
    let mut stream = opened_stream();
    let changes = vec![
        stream
            .accept(snapshot(3, page([2, 3], true), Some(1)))
            .unwrap(),
        stream.accept(FollowFrame::Entry(record(4))).unwrap(),
        stream
            .accept(FollowFrame::Assistant(AssistantFrame { revision: 2 }))
            .unwrap(),
    ];

    assert_eq!(
        changes,
        vec![
            FollowChange::Replace {
                page: page([2, 3], true)
            },
            FollowChange::Append { record: record(4) },
            FollowChange::AssistantStream {
                frame: AssistantFrame { revision: 2 }
            },
        ]
    );
    assert_eq!(stream.cursor(), Some(4));
    assert_eq!(stream.assistant_revision().revision(), 2);
}

#[test]
fn refuses_an_entry_before_its_opening_cursor() {
    let mut stream = opened_stream();
    assert_eq!(
        stream.accept(FollowFrame::Entry(record(0))).unwrap_err(),
        StreamError::NotOpen {
            name: "session event stream".to_owned()
        }
    );
    assert!(!stream.is_opened());
}

#[test]
fn refuses_a_second_opening_in_one_generation() {
    let mut stream = opened_stream();
    stream
        .accept(snapshot(0, page([0], false), Some(1)))
        .unwrap();
    assert_eq!(
        stream
            .accept(snapshot(0, page([0], false), Some(1)))
            .unwrap_err(),
        StreamError::RepeatedOpening {
            name: "session event stream".to_owned()
        }
    );
}

#[test]
fn refuses_an_opted_in_opening_that_omitted_its_assistant_baseline() {
    // The baseline is not "no active attempt": it means the peer ignored `assistantStream: true`,
    // and assuming an empty baseline would silently drop a live answer.
    let mut stream = opened_stream();
    assert_eq!(
        stream
            .accept(snapshot(0, page([0], false), None))
            .unwrap_err(),
        StreamError::MissingAssistantBaseline {
            name: "session event stream".to_owned()
        }
    );
    assert!(
        !stream.is_opened(),
        "a refused opening must not become the base"
    );
}

#[test]
fn a_stream_that_did_not_opt_in_accepts_an_opening_without_a_baseline() {
    let mut stream = JournalStream::new("session event stream", FollowOptions::default());
    stream.accept(snapshot(0, page([0], false), None)).unwrap();
    // ...and then refuses an Assistant frame, because there is still no base to count revisions from.
    assert_eq!(
        stream
            .accept(FollowFrame::Assistant(AssistantFrame { revision: 1 }))
            .unwrap_err(),
        StreamError::NotOpen {
            name: "session event stream".to_owned()
        }
    );
}

#[test]
fn refuses_a_snapshot_whose_records_do_not_end_at_its_cursor() {
    let mut stream = opened_stream();
    assert_eq!(
        stream
            .accept(snapshot(9, page([2, 3], true), Some(1)))
            .unwrap_err(),
        StreamError::CursorMismatch {
            name: "session event stream".to_owned(),
            announced: Some(9),
            actual: Some(3),
        }
    );
    assert_eq!(
        stream
            .accept(snapshot(2, page([], false), Some(1)))
            .unwrap_err(),
        StreamError::CursorMismatch {
            name: "session event stream".to_owned(),
            announced: Some(2),
            actual: None,
        }
    );
    // An empty journal announces no cursor and holds no records: the one consistent empty opening.
    let mut empty = opened_stream();
    empty
        .accept(empty_snapshot(page([], false), Some(0)))
        .unwrap();
    assert_eq!(empty.cursor(), None);
    // ...and a cursor with no records is not that opening.
    let mut inconsistent = opened_stream();
    assert_eq!(
        inconsistent
            .accept(empty_snapshot(page([0], false), Some(0)))
            .unwrap_err(),
        StreamError::CursorMismatch {
            name: "session event stream".to_owned(),
            announced: None,
            actual: Some(0),
        }
    );
}

#[test]
fn refuses_a_stale_entry_instead_of_duplicating_it() {
    let mut stream = opened_stream();
    stream
        .accept(snapshot(3, page([3], false), Some(1)))
        .unwrap();
    assert_eq!(
        stream.accept(FollowFrame::Entry(record(3))).unwrap_err(),
        StreamError::StaleEntry {
            name: "session event stream".to_owned(),
            cursor: 3,
            seq: 3,
        }
    );
    assert_eq!(
        stream.cursor(),
        Some(3),
        "a refused entry must not move the cursor"
    );
}

#[test]
fn a_revision_gap_is_recoverable_and_reports_the_upstream_message() {
    let mut stream = opened_stream();
    stream
        .accept(snapshot(0, page([0], false), Some(1)))
        .unwrap();
    stream
        .accept(FollowFrame::Assistant(AssistantFrame { revision: 2 }))
        .unwrap();

    let error = stream
        .accept(FollowFrame::Assistant(AssistantFrame { revision: 4 }))
        .unwrap_err();
    assert_eq!(
        error,
        StreamError::RevisionGap {
            name: "session event stream".to_owned(),
            expected: 3,
            received: 4,
        }
    );
    assert!(error.is_recoverable());
    assert_eq!(
        error.carrier_message().as_deref(),
        Some("session assistant stream skipped revision 3")
    );
    // The durable cursor is untouched: nothing durable was lost.
    assert_eq!(stream.cursor(), Some(0));

    // Terminal errors say so, and have no carrier message of their own.
    let terminal = StreamError::RepeatedOpening {
        name: "session event stream".to_owned(),
    };
    assert!(!terminal.is_recoverable());
    assert_eq!(terminal.carrier_message(), None);
}

#[test]
fn a_reconnect_keeps_the_cursor_and_expects_a_new_opening() {
    let mut stream = opened_stream();
    stream
        .accept(snapshot(1, page([0, 1], false), Some(1)))
        .unwrap();
    stream.accept(FollowFrame::Entry(record(2))).unwrap();

    stream.reconnect();
    assert!(!stream.is_opened());
    assert_eq!(stream.cursor(), Some(2), "the cursor survives a reconnect");

    // A live entry after the reconnection but before the new opening is still a violation.
    assert!(matches!(
        stream.accept(FollowFrame::Entry(record(3))).unwrap_err(),
        StreamError::NotOpen { .. }
    ));

    // The replacement opening replaces the published window from the new cut.
    let change = stream
        .accept(snapshot(4, page([0, 1, 2, 3, 4], false), Some(1)))
        .unwrap();
    assert_eq!(
        change,
        FollowChange::Replace {
            page: page([0, 1, 2, 3, 4], false)
        }
    );
    assert_eq!(stream.cursor(), Some(4));
}

#[test]
fn the_stream_exposes_its_diagnostic_identity() {
    let stream = JournalStream::new("session event stream", FollowOptions::upstream_default());
    assert_eq!(stream.name(), "session event stream");
    assert_eq!(stream.options(), FollowOptions::upstream_default());
    assert!(stream.options().assistant_stream);
    assert!(stream.options().activate);
    // A stream that did not opt in to both defaults reports exactly that.
    assert_eq!(
        FollowOptions::default(),
        FollowOptions {
            assistant_stream: false,
            activate: false
        }
    );
}

#[test]
fn error_messages_name_the_stream() {
    let cases = [
        (
            StreamError::NotOpen {
                name: "session event stream".to_owned(),
            },
            "session event stream emitted an entry before its opening cursor",
        ),
        (
            StreamError::RepeatedOpening {
                name: "session event stream".to_owned(),
            },
            "session event stream emitted more than one opening cursor",
        ),
        (
            StreamError::MissingAssistantBaseline {
                name: "session event stream".to_owned(),
            },
            "session event stream omitted its opted-in opening Assistant baseline",
        ),
        (
            StreamError::CursorMismatch {
                name: "session event stream".to_owned(),
                announced: Some(9),
                actual: None,
            },
            "session event stream announced cursor 9 but its page is empty",
        ),
        (
            StreamError::CursorMismatch {
                name: "session event stream".to_owned(),
                announced: Some(9),
                actual: Some(3),
            },
            "session event stream announced cursor 9 but its page ends at 3",
        ),
        (
            StreamError::CursorMismatch {
                name: "session event stream".to_owned(),
                announced: None,
                actual: Some(3),
            },
            "session event stream announced an empty journal but its page ends at 3",
        ),
        (
            StreamError::StaleEntry {
                name: "session event stream".to_owned(),
                cursor: 3,
                seq: 2,
            },
            "session event stream emitted entry 2 at or behind its cursor 3",
        ),
        (
            StreamError::RevisionGap {
                name: "session event stream".to_owned(),
                expected: 2,
                received: 3,
            },
            "session assistant stream skipped revision 2",
        ),
        (
            StreamError::MalformedAssistant {
                detail: "frame has no type".to_owned(),
            },
            "session assistant frame is malformed: frame has no type",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}

// ---- the control stream ---------------------------------------------------------------------

fn baseline() -> ControlFrame {
    ControlFrame::Baseline(ControlBaseline {
        projections: BTreeMap::new(),
    })
}

fn projection(seq: u64) -> ControlFrame {
    ControlFrame::Projection(ProjectionUpdate {
        session_id: "session-1".to_owned(),
        seq,
        changes: vec![("title".to_owned(), json!("updated"))],
    })
}

#[test]
fn maps_a_control_baseline_and_its_deltas() {
    let mut stream = ControlStream::new("session control stream");
    assert_eq!(stream.accept(&baseline()).unwrap(), ControlAction::Replace);
    assert_eq!(
        stream.accept(&projection(1)).unwrap(),
        ControlAction::Update
    );
    assert_eq!(
        stream.accept(&projection(2)).unwrap(),
        ControlAction::Update
    );
    assert_eq!(stream.replaced(), 1);
    assert_eq!(stream.updated(), 2);
    assert!(stream.baseline_seen());
}

#[test]
fn refuses_a_control_delta_before_its_baseline() {
    let mut stream = ControlStream::new("session control stream");
    assert_eq!(
        stream.accept(&projection(1)).unwrap_err(),
        ControlError::DeltaBeforeBaseline {
            name: "session control stream".to_owned()
        }
    );
    assert_eq!(stream.updated(), 0);
}

#[test]
fn refuses_a_second_baseline_in_one_generation_but_accepts_one_after_advancing() {
    let mut stream = ControlStream::new("session control stream");
    stream.accept(&baseline()).unwrap();
    assert_eq!(
        stream.accept(&baseline()).unwrap_err(),
        ControlError::RepeatedBaseline {
            name: "session control stream".to_owned()
        }
    );

    stream.advance();
    assert_eq!(stream.generation(), 1);
    assert!(!stream.baseline_seen());
    assert_eq!(stream.accept(&baseline()).unwrap(), ControlAction::Replace);
    assert_eq!(stream.replaced(), 2);
}

#[test]
fn classifies_a_generation_end_by_whether_its_baseline_arrived() {
    assert_eq!(EndKind::classify(true), EndKind::CarrierLost);
    assert_eq!(EndKind::classify(false), EndKind::ProtocolViolation);
    assert_eq!(
        EndKind::CarrierLost.message("session control stream"),
        "session control stream ended without a terminal result"
    );
    assert_eq!(
        EndKind::ProtocolViolation.message("session control stream"),
        "session control stream ended before its opening snapshot"
    );
}

#[test]
fn control_error_messages_match_upstream() {
    assert_eq!(
        ControlError::DeltaBeforeBaseline {
            name: "session control stream".to_owned()
        }
        .to_string(),
        "session control stream emitted an update before its opening snapshot"
    );
    assert_eq!(
        ControlError::RepeatedBaseline {
            name: "session control stream".to_owned()
        }
        .to_string(),
        "session control stream emitted more than one opening snapshot"
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_client_gap as client;
}
