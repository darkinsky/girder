//! Tests for the Remote stream carrier: upstream `packages/api/gateway`'s `stream-protocol.ts`,
//! the logical-stream half of `stream-server.ts`, and `rejectRemoteStreamUpgrade`.
//!
//! Corresponds to upstream `packages/api/gateway/tests/stream-protocol.host.spec.ts`,
//! `stream-server.host.spec.ts` (the inbox and routing cases; the `ws` carrier cases — Ping frames,
//! `terminate`, `bufferedAmount` — have no local counterpart because this crate's socket is
//! `girder-rpc`'s), and the access-control half of `gateway.host.spec.ts`.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than
//! a copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}` — including
//! the ones the module under test makes, which is why the module itself spells them that way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/api/extra.rs"]
pub mod api_extra_gap;

use serde_json::{Map, Value, json};

use api_extra_gap::{
    AbortReason, ClientMessage, Disposition, Heartbeat, MAX_MISSED_HEARTBEATS, ParseError,
    Reception, ServerMessage, StreamFailure, StreamTable, Tick, UpgradeRejection, UplinkError,
    UplinkInbox, UplinkPoll, UplinkViolation, UplinkWindow, bytes_of, disposition,
    frame_overhead_bytes, parse_client_message, parse_server_message,
};

// ---- client messages --------------------------------------------------------------------------

/// Upstream's `accepts every client message variant`, verbatim.
#[test]
fn accepts_every_client_message_variant() {
    assert_eq!(
        parse_client_message(
            &json!({
                "type": "open", "streamId": "stream-1", "endpoint": "feed/follow",
                "payload": { "cursor": 1 },
            })
            .to_string()
        )
        .unwrap(),
        ClientMessage::Open {
            stream_id: "stream-1".to_owned(),
            endpoint: "feed/follow".to_owned(),
            payload: json!({ "cursor": 1 }),
        }
    );
    assert_eq!(
        parse_client_message(r#"{"type":"cancel","streamId":"stream-1"}"#).unwrap(),
        ClientMessage::Cancel {
            stream_id: "stream-1".to_owned(),
        }
    );
    assert_eq!(
        parse_client_message(r#"{"type":"item","streamId":"stream-1","value":{"line":"ls\n"}}"#)
            .unwrap(),
        ClientMessage::Item {
            stream_id: "stream-1".to_owned(),
            value: Some(json!({ "line": "ls\n" })),
        }
    );
    // A present-but-absent `value` is a legal item and stays distinguishable from `null`.
    assert_eq!(
        parse_client_message(r#"{"type":"item","streamId":"stream-1"}"#).unwrap(),
        ClientMessage::Item {
            stream_id: "stream-1".to_owned(),
            value: None,
        }
    );
    assert_eq!(
        parse_client_message(r#"{"type":"item","streamId":"stream-1","value":null}"#).unwrap(),
        ClientMessage::Item {
            stream_id: "stream-1".to_owned(),
            value: Some(Value::Null),
        }
    );
    assert_eq!(
        parse_client_message(r#"{"type":"end","streamId":"stream-1"}"#).unwrap(),
        ClientMessage::End {
            stream_id: "stream-1".to_owned(),
        }
    );
}

/// The `it.each` table from upstream's spec, case for case, plus the exact-key rule.
#[test]
fn rejects_every_invalid_client_message_upstream_rejects() {
    let cases = [
        json!({ "type": "open", "streamId": "", "endpoint": "feed/follow", "payload": {} }),
        json!({ "type": "open", "streamId": "stream-1", "endpoint": "", "payload": {} }),
        json!({ "type": "open", "streamId": "stream-1", "endpoint": "feed/follow" }),
        json!({ "type": "cancel", "streamId": "stream-1", "extra": true }),
        json!({ "type": "item", "streamId": "", "value": "line" }),
        json!({ "type": "item", "streamId": "stream-1", "value": "line", "extra": true }),
        json!({ "type": "item", "streamId": "stream-1", "endpoint": "feed/follow" }),
        json!({ "type": "end", "streamId": "" }),
        json!({ "type": "end", "streamId": "stream-1", "value": "line" }),
        json!({ "type": "unknown", "streamId": "stream-1" }),
    ];
    for case in cases {
        assert_eq!(
            parse_client_message(&case.to_string()),
            Err(ParseError::InvalidClientMessage),
            "should have rejected {case}"
        );
    }
}

/// Upstream: "rejects an uplink item whose value is not lossless JSON". The check runs on the
/// *decoded* value, not the text, so the case that proves it is one that parses and only then
/// turns out to be untransportable.
///
/// `-0` is the one upstream case that diverges, and the divergence is documented in
/// [`girder_core::is_lossless`]: JavaScript cannot distinguish `-0` from `0` and upstream rejects it,
/// while `serde_json` preserves it, so it is lossless *here*. That is asserted rather than skipped,
/// because a silent difference between the two faces is exactly what a port must not have.
#[test]
fn rejects_an_uplink_item_whose_value_is_not_lossless_json() {
    // `serde_json` here carries `arbitrary_precision`, so it keeps the literal text of `-0` rather
    // than folding it to `0`; that is what makes it lossless on this face.
    let negative_zero = parse_client_message(r#"{"type":"item","streamId":"stream-1","value":-0}"#)
        .expect("`-0` is preserved textually, so it is not a hazard on this face");
    assert_eq!(negative_zero.stream_id(), "stream-1");
    match negative_zero {
        ClientMessage::Item {
            value: Some(value), ..
        } => assert_eq!(value.to_string(), "-0"),
        other => panic!("expected an item carrying -0, got {other:?}"),
    }
    // An out-of-range integer is the hazard this crate's `is_lossless` exists for.
    assert_eq!(
        parse_client_message(r#"{"type":"item","streamId":"stream-1","value":9007199254740993}"#),
        Err(ParseError::InvalidClientMessage)
    );
    // …and one inside the safe range passes.
    assert!(
        parse_client_message(r#"{"type":"item","streamId":"stream-1","value":9007199254740991}"#)
            .is_ok()
    );
}

/// `stream_id` and `is_open` are what a router needs before it has matched a variant, so both are
/// exercised on every variant rather than on the one that happens to be convenient.
#[test]
fn every_client_frame_names_its_stream_and_the_opener_alone_opens() {
    let frames = [
        ClientMessage::Open {
            stream_id: "s".to_owned(),
            endpoint: "feed/follow".to_owned(),
            payload: json!({}),
        },
        ClientMessage::Item {
            stream_id: "s".to_owned(),
            value: Some(json!(1)),
        },
        ClientMessage::End {
            stream_id: "s".to_owned(),
        },
        ClientMessage::Cancel {
            stream_id: "s".to_owned(),
        },
    ];
    for frame in &frames {
        assert_eq!(frame.stream_id(), "s", "{frame:?}");
    }
    assert_eq!(
        frames.iter().filter(|frame| frame.is_open()).count(),
        1,
        "only `open` creates a stream"
    );
    assert!(frames[0].is_open());
}

/// The same for a Host frame: a pump routes its downlink by this id and never re-reads the variant.
#[test]
fn every_server_frame_names_its_stream() {
    let frames = [
        ServerMessage::Item {
            stream_id: "s".to_owned(),
            value: None,
        },
        ServerMessage::End {
            stream_id: "s".to_owned(),
        },
        ServerMessage::Error {
            stream_id: "s".to_owned(),
            error: StreamFailure::new("offline", "gone", Map::new()),
        },
    ];
    for frame in &frames {
        assert_eq!(frame.stream_id(), "s", "{frame:?}");
    }
}

#[test]
fn rejects_a_non_message_payload_with_the_upstream_diagnostic() {
    for text in ["not json", "null", "[]", "1"] {
        let expected = if text == "not json" {
            ParseError::NotJson
        } else {
            ParseError::NotObject
        };
        assert_eq!(parse_client_message(text), Err(expected), "for {text}");
        assert_eq!(parse_server_message(text), Err(expected), "for {text}");
    }
    assert_eq!(
        ParseError::NotJson.to_string(),
        "api gateway: Remote stream message is not JSON"
    );
    assert_eq!(
        ParseError::NotObject.to_string(),
        "api gateway: Remote stream message must be an object"
    );
    assert_eq!(
        ParseError::InvalidClientMessage.to_string(),
        "api gateway: invalid Remote stream client message"
    );
    assert_eq!(
        ParseError::InvalidServerMessage.to_string(),
        "api gateway: invalid Remote stream server message"
    );
}

// ---- server messages --------------------------------------------------------------------------

#[test]
fn accepts_every_server_message_variant() {
    assert_eq!(
        parse_server_message(r#"{"type":"item","streamId":"stream-1","value":null}"#).unwrap(),
        ServerMessage::Item {
            stream_id: "stream-1".to_owned(),
            value: Some(Value::Null),
        }
    );
    assert_eq!(
        parse_server_message(r#"{"type":"item","streamId":"stream-1"}"#).unwrap(),
        ServerMessage::Item {
            stream_id: "stream-1".to_owned(),
            value: None,
        }
    );
    assert_eq!(
        parse_server_message(
            r#"{"type":"error","streamId":"stream-1","error":{"code":"offline","message":"connection lost","details":{}}}"#
        )
        .unwrap(),
        ServerMessage::Error {
            stream_id: "stream-1".to_owned(),
            error: StreamFailure::new("offline", "connection lost", Map::new()),
        }
    );
    assert_eq!(
        parse_server_message(r#"{"type":"end","streamId":"stream-1"}"#).unwrap(),
        ServerMessage::End {
            stream_id: "stream-1".to_owned(),
        }
    );
}

#[test]
fn rejects_every_invalid_server_message_upstream_rejects() {
    let cases = [
        r#"{"type":"item","streamId":"","value":"item"}"#,
        r#"{"type":"item","streamId":"stream-1","extra":true}"#,
        r#"{"type":"end","streamId":"stream-1","extra":true}"#,
        r#"{"type":"error","streamId":"stream-1","error":[]}"#,
        r#"{"type":"error","streamId":"stream-1","error":{"code":1,"message":"failure","details":{}}}"#,
        r#"{"type":"error","streamId":"stream-1","error":{"code":"failed","message":1,"details":{}}}"#,
        r#"{"type":"error","streamId":"stream-1","error":{"code":"failed","message":"failure","details":[]}}"#,
        r#"{"type":"unknown","streamId":"stream-1"}"#,
    ];
    for case in cases {
        assert_eq!(
            parse_server_message(case),
            Err(ParseError::InvalidServerMessage),
            "should have rejected {case}"
        );
    }
}

/// The encoder must round-trip, including the present-but-absent `value`, because a Host and a
/// Client that disagree on `value: null` vs. no `value` disagree on what a stream item *is*.
#[test]
fn every_server_frame_round_trips_through_its_own_encoder() {
    let frames = [
        ServerMessage::Item {
            stream_id: "s".to_owned(),
            value: None,
        },
        ServerMessage::Item {
            stream_id: "s".to_owned(),
            value: Some(json!({ "line": "ls\n", "n": 1 })),
        },
        ServerMessage::End {
            stream_id: "s".to_owned(),
        },
        ServerMessage::Error {
            stream_id: "s".to_owned(),
            error: UplinkViolation::Overflow {
                endpoint: "terminal/attach".to_owned(),
                max_bytes: 512,
            }
            .to_failure(),
        },
    ];
    for frame in frames {
        let text = frame.encode();
        assert_eq!(parse_server_message(&text).unwrap(), frame);
    }
    // An absent `value` must not appear as `null` in the encoded text.
    let absent = ServerMessage::Item {
        stream_id: "s".to_owned(),
        value: None,
    }
    .encode();
    assert!(!absent.contains("value"), "{absent}");
}

// ---- the uplink window ------------------------------------------------------------------------

/// The overhead must be *measured*, because the stream id is part of the frame it measures.
#[test]
fn the_frame_overhead_tracks_the_stream_id_it_measures() {
    let short = frame_overhead_bytes("s");
    let long = frame_overhead_bytes("stream-with-a-much-longer-id");
    assert!(
        long > short,
        "a longer id must leave less room for the item: {short} vs {long}"
    );
    // The envelope costs exactly one byte per id character, no escaping needed.
    assert_eq!(
        long - short,
        bytes_of("stream-with-a-much-longer-id") - bytes_of("s")
    );
    // An id needing escapes costs more than its character count.
    assert!(frame_overhead_bytes("a\"b") > frame_overhead_bytes("aab"));
    // The probe frame minus its `null` is the envelope: `{"type":"item","streamId":"s","value":null}`.
    assert_eq!(
        short,
        bytes_of(r#"{"type":"item","streamId":"s","value":null}"#) - 4
    );
}

#[test]
fn a_windows_frame_bytes_are_the_item_plus_its_envelope() {
    let window = UplinkWindow::for_stream("s", 512);
    assert_eq!(window.max_bytes, 512);
    assert_eq!(
        window.frame_bytes(&json!({ "line": "ls\n" })),
        bytes_of(r#"{"line":"ls\n"}"#) + window.frame_overhead_bytes
    );
    // The same item in a longer-id stream costs strictly more.
    let wider = UplinkWindow::for_stream("stream-1", 512);
    assert!(
        wider.frame_bytes(&json!({ "line": "ls\n" }))
            > window.frame_bytes(&json!({ "line": "ls\n" }))
    );
}

// ---- the inbox --------------------------------------------------------------------------------

#[test]
fn an_empty_inbox_is_pending_rather_than_ended() {
    let inbox = UplinkInbox::new(64, "feed/follow");
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Pending);
    assert!(inbox.is_empty());
    assert!(!inbox.is_ended());
    assert!(!inbox.is_finished());
    inbox.end();
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Ended);
    assert!(inbox.is_ended());
}

#[test]
fn items_drain_in_push_order_and_release_their_bytes() {
    let inbox = UplinkInbox::new(1024, "feed/follow");
    inbox.push(json!(1), 10).unwrap();
    inbox.push(json!(2), 20).unwrap();
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox.buffered_bytes(), 30);
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Item(json!(1)));
    assert_eq!(inbox.buffered_bytes(), 20);
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Item(json!(2)));
    assert_eq!(inbox.buffered_bytes(), 0);
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Pending);
}

/// The budget is on **buffered bytes**, so a stream that drains never overflows no matter how much
/// travelled through it.
#[test]
fn draining_lets_more_bytes_through_than_the_budget() {
    let inbox = UplinkInbox::new(64, "feed/follow");
    for _ in 0..100 {
        inbox.push(json!("x"), 64).unwrap();
        assert!(matches!(inbox.poll().unwrap(), UplinkPoll::Item(_)));
    }
    assert_eq!(inbox.buffered_bytes(), 0);
}

/// The one case the byte budget exists for: an undrained stream must stop accepting.
#[test]
fn an_overflow_fails_the_stream_and_drops_what_was_buffered() {
    let inbox = UplinkInbox::new(64, "terminal/attach");
    inbox.push(json!("a"), 32).unwrap();
    let violation = inbox.push(json!("b"), 64).unwrap_err();
    assert_eq!(
        violation,
        UplinkViolation::Overflow {
            endpoint: "terminal/attach".to_owned(),
            max_bytes: 64,
        }
    );
    assert_eq!(violation.code(), "gateway/uplink-overflow");
    assert_eq!(
        violation.to_failure().details.get("endpoint"),
        Some(&json!("terminal/attach"))
    );
    // The already-queued item is gone: a consumer must not read a prefix of a broken stream.
    assert!(inbox.is_empty());
    assert_eq!(inbox.buffered_bytes(), 0);
    assert_eq!(inbox.poll(), Err(UplinkError::Violation(violation.clone())));
    // …and the failure is sticky rather than transient.
    assert_eq!(inbox.poll(), Err(UplinkError::Violation(violation)));
}

#[test]
fn an_item_after_end_is_a_protocol_violation() {
    let inbox = UplinkInbox::new(1024, "feed/follow");
    inbox.end();
    let violation = inbox.push(json!(1), 4).unwrap_err();
    assert_eq!(
        violation,
        UplinkViolation::ItemAfterEnd {
            endpoint: "feed/follow".to_owned(),
        }
    );
    assert_eq!(violation.code(), "gateway/protocol");
    assert_eq!(
        violation.to_string(),
        "api gateway: Remote stream uplink item after end"
    );
}

/// `end` is a half-close, not a failure: a consumer that already buffered items must still get them.
#[test]
fn end_lets_a_consumer_drain_what_was_buffered() {
    let inbox = UplinkInbox::new(1024, "feed/follow");
    inbox.push(json!(1), 4).unwrap();
    inbox.push(json!(2), 4).unwrap();
    inbox.end();
    inbox.end(); // idempotent
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Item(json!(1)));
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Item(json!(2)));
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Ended);
}

/// `fail` is *not* `end`: the buffer is dropped, so the tail of a discarded stream is unreachable.
#[test]
fn fail_drops_the_buffer_and_reports_closure_not_a_violation() {
    let inbox = UplinkInbox::new(1024, "feed/follow");
    inbox.push(json!(1), 4).unwrap();
    inbox.fail("Remote stream cancelled");
    assert!(inbox.is_finished());
    assert_eq!(
        inbox.poll(),
        Err(UplinkError::Closed {
            reason: "Remote stream cancelled".to_owned(),
        })
    );
    assert_eq!(
        inbox.poll().unwrap_err().to_string(),
        "Remote stream cancelled"
    );
}

/// A push after the stream died is dropped silently: it is not a *new* violation, and re-raising
/// would make a socket close look like a protocol error.
#[test]
fn a_push_after_failure_is_dropped_without_a_second_violation() {
    let inbox = UplinkInbox::new(64, "feed/follow");
    inbox.fail("socket closed");
    assert!(inbox.push(json!(1), 1).is_ok());
    assert!(inbox.is_empty());
    assert_eq!(
        inbox.poll(),
        Err(UplinkError::Closed {
            reason: "socket closed".to_owned(),
        })
    );
}

/// The first failure wins. An overflow followed by a cancel must still report the overflow, because
/// that is the one the Client has to act on.
#[test]
fn the_first_failure_is_the_one_reported() {
    let inbox = UplinkInbox::new(8, "terminal/attach");
    inbox.push(json!(1), 8).unwrap();
    let violation = inbox.push(json!(2), 8).unwrap_err();
    inbox.fail("Remote stream cancelled");
    assert_eq!(inbox.poll(), Err(UplinkError::Violation(violation)));
}

#[test]
fn closing_for_the_consumer_ends_the_stream_and_frees_bytes() {
    let inbox = UplinkInbox::new(1024, "feed/follow");
    inbox.push(json!(1), 4).unwrap();
    inbox.close();
    assert_eq!(inbox.buffered_bytes(), 0);
    assert!(inbox.is_finished());
    assert_eq!(inbox.poll().unwrap(), UplinkPoll::Ended);
    // A cancelled-then-closed stream is still not a violation.
    assert!(inbox.push(json!(2), 4).is_ok());
}

// ---- the stream table -------------------------------------------------------------------------

#[test]
fn an_open_creates_a_stream_with_its_own_budget_and_endpoint() {
    let mut table = StreamTable::new(512);
    assert!(table.is_empty());
    assert_eq!(
        table.receive(
            &ClientMessage::Open {
                stream_id: "stream-1".to_owned(),
                endpoint: "feed/follow".to_owned(),
                payload: json!({}),
            },
            48,
        ),
        Reception::Opened
    );
    assert_eq!(table.len(), 1);
    let inbox = table.inbox("stream-1").unwrap();
    assert_eq!(inbox.endpoint(), "feed/follow");
    assert_eq!(inbox.max_bytes(), 512);
    assert_eq!(
        table.abort_reason("stream-1"),
        Some(&AbortReason::NotAborted)
    );
    assert!(table.inbox("absent").is_none());
}

/// The one protocol violation that fails the *socket*: two meanings for one id.
#[test]
fn a_duplicate_open_is_reported_rather_than_merged() {
    let mut table = StreamTable::new(512);
    let open = ClientMessage::Open {
        stream_id: "stream-1".to_owned(),
        endpoint: "feed/follow".to_owned(),
        payload: json!({}),
    };
    assert_eq!(table.receive(&open, 48), Reception::Opened);
    assert_eq!(table.receive(&open, 48), Reception::Duplicate);
    assert_eq!(table.len(), 1, "the second open must not replace the first");
}

/// A frame for a finished stream is expected, not an error: the tail arrives after the stream left
/// the table.
#[test]
fn frames_for_an_unknown_stream_are_dropped() {
    let mut table = StreamTable::new(512);
    assert_eq!(
        table.receive(
            &ClientMessage::Item {
                stream_id: "gone".to_owned(),
                value: Some(json!(1)),
            },
            32,
        ),
        Reception::Dropped
    );
    assert_eq!(
        table.receive(
            &ClientMessage::End {
                stream_id: "gone".to_owned(),
            },
            32,
        ),
        Reception::Dropped
    );
    assert_eq!(
        table.receive(
            &ClientMessage::Cancel {
                stream_id: "gone".to_owned(),
            },
            32,
        ),
        Reception::Dropped
    );
}

#[test]
fn an_item_and_an_end_reach_their_stream() {
    let mut table = StreamTable::new(512);
    table.receive(
        &ClientMessage::Open {
            stream_id: "s".to_owned(),
            endpoint: "terminal/attach".to_owned(),
            payload: json!({}),
        },
        40,
    );
    assert_eq!(
        table.receive(
            &ClientMessage::Item {
                stream_id: "s".to_owned(),
                value: Some(json!({ "keys": "ls\n" })),
            },
            40,
        ),
        Reception::Routed
    );
    let inbox = table.inbox("s").unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(
        inbox.poll().unwrap(),
        UplinkPoll::Item(json!({ "keys": "ls\n" }))
    );
    assert_eq!(
        table.receive(
            &ClientMessage::End {
                stream_id: "s".to_owned(),
            },
            32,
        ),
        Reception::Routed
    );
    assert!(inbox.is_ended());
}

/// An item frame *without* a `value` is not an item: it carries nothing for the Host to read.
#[test]
fn an_item_without_a_value_is_routed_but_queues_nothing() {
    let mut table = StreamTable::new(512);
    table.receive(
        &ClientMessage::Open {
            stream_id: "s".to_owned(),
            endpoint: "terminal/attach".to_owned(),
            payload: json!({}),
        },
        40,
    );
    assert_eq!(
        table.receive(
            &ClientMessage::Item {
                stream_id: "s".to_owned(),
                value: None,
            },
            32,
        ),
        Reception::Routed
    );
    assert!(table.inbox("s").unwrap().is_empty());
}

/// An overflow discovered while routing must be recorded on the stream, so the pump that finishes it
/// writes the `error` frame the Client is waiting for.
#[test]
fn an_overflow_is_recorded_as_the_streams_abort_reason() {
    let mut table = StreamTable::new(8);
    table.receive(
        &ClientMessage::Open {
            stream_id: "s".to_owned(),
            endpoint: "terminal/attach".to_owned(),
            payload: json!({}),
        },
        40,
    );
    assert_eq!(
        table.receive(
            &ClientMessage::Item {
                stream_id: "s".to_owned(),
                value: Some(json!(1)),
            },
            8,
        ),
        Reception::Routed
    );
    assert_eq!(
        table.receive(
            &ClientMessage::Item {
                stream_id: "s".to_owned(),
                value: Some(json!(2)),
            },
            8,
        ),
        Reception::Routed
    );
    assert!(matches!(
        table.abort_reason("s"),
        Some(AbortReason::Remote(UplinkViolation::Overflow { .. }))
    ));
    let completion = table.finish("s", false).unwrap();
    assert_eq!(completion.disposition, Disposition::Failure);
    assert!(matches!(
        completion.reason,
        AbortReason::Remote(UplinkViolation::Overflow { .. })
    ));
    assert!(table.is_empty());
}

#[test]
fn a_cancel_marks_the_stream_local_and_silences_its_terminal_frame() {
    let mut table = StreamTable::new(512);
    table.receive(
        &ClientMessage::Open {
            stream_id: "s".to_owned(),
            endpoint: "feed/follow".to_owned(),
            payload: json!({}),
        },
        40,
    );
    let inbox = table.inbox("s").unwrap();
    assert_eq!(
        table.receive(
            &ClientMessage::Cancel {
                stream_id: "s".to_owned(),
            },
            32,
        ),
        Reception::Routed
    );
    assert_eq!(table.abort_reason("s"), Some(&AbortReason::Local));
    assert!(inbox.is_finished());
    let completion = table.finish("s", true).unwrap();
    assert_eq!(completion.disposition, Disposition::Silent);
}

/// `finish` fails the inbox so a Host reader parked on it is woken instead of hanging.
#[test]
fn finish_fails_the_inbox_and_removes_the_stream() {
    let mut table = StreamTable::new(512);
    table.receive(
        &ClientMessage::Open {
            stream_id: "s".to_owned(),
            endpoint: "feed/follow".to_owned(),
            payload: json!({}),
        },
        40,
    );
    let inbox = table.inbox("s").unwrap();
    let completion = table.finish("s", false).unwrap();
    assert_eq!(completion.disposition, Disposition::End);
    assert_eq!(completion.reason, AbortReason::NotAborted);
    assert!(
        matches!(inbox.poll(), Err(UplinkError::Closed { reason }) if reason == "Remote stream ended")
    );
    // The completion hands the pump the same inbox, already failed: a caller that reads it there
    // sees the same closure rather than a second, differently-worded one.
    assert!(completion.inbox.is_finished());
    assert_eq!(completion.inbox.endpoint(), "feed/follow");
    assert!(table.is_empty());
    assert!(table.finish("s", false).is_none());
}

/// A socket close is not a Gateway failure: every live stream is silenced, and every inbox is woken.
#[test]
fn shutdown_silences_every_stream_and_wakes_every_reader() {
    let mut table = StreamTable::new(512);
    for id in ["a", "b", "c"] {
        table.receive(
            &ClientMessage::Open {
                stream_id: id.to_owned(),
                endpoint: "feed/follow".to_owned(),
                payload: json!({}),
            },
            40,
        );
    }
    let inboxes: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|id| table.inbox(id).unwrap())
        .collect();
    let stopped = table.shutdown();
    assert_eq!(stopped.len(), 3);
    assert!(table.is_empty());
    for inbox in inboxes {
        assert!(inbox.is_finished());
        assert!(matches!(
            inbox.poll(),
            Err(UplinkError::Closed { reason }) if reason == "Remote stream socket closed"
        ));
    }
}

// ---- dispositions and heartbeat ---------------------------------------------------------------

/// The ordering upstream gets right: a cancelled stream never reports the error it also threw.
#[test]
fn the_abort_reason_wins_over_the_iterators_outcome() {
    let remote = AbortReason::Remote(UplinkViolation::ItemAfterEnd {
        endpoint: "feed/follow".to_owned(),
    });
    assert_eq!(disposition(&remote, false), Disposition::Failure);
    assert_eq!(disposition(&remote, true), Disposition::Failure);
    assert_eq!(disposition(&AbortReason::Local, true), Disposition::Silent);
    assert_eq!(disposition(&AbortReason::Local, false), Disposition::Silent);
    assert_eq!(
        disposition(&AbortReason::NotAborted, true),
        Disposition::Failure
    );
    assert_eq!(
        disposition(&AbortReason::NotAborted, false),
        Disposition::End
    );
}

#[test]
fn the_heartbeat_pings_twice_then_asks_for_verification() {
    let mut heartbeat = Heartbeat::new();
    assert_eq!(heartbeat.missed(), 0);
    assert_eq!(heartbeat.tick(), Tick::Ping);
    assert_eq!(heartbeat.missed(), 1);
    assert_eq!(heartbeat.tick(), Tick::Ping);
    assert_eq!(heartbeat.missed(), MAX_MISSED_HEARTBEATS);
    assert_eq!(heartbeat.tick(), Tick::Verify);
    assert!(heartbeat.verify());
    // Verification without activity keeps asking; nothing terminates on the first check alone.
    assert_eq!(heartbeat.tick(), Tick::Verify);
}

/// A Pong that arrives between arming and verifying must save the socket — this is the case the
/// two-phase tick exists for.
#[test]
fn a_pong_between_the_tick_and_the_verification_saves_the_socket() {
    let mut heartbeat = Heartbeat::new();
    heartbeat.tick();
    heartbeat.tick();
    assert_eq!(heartbeat.tick(), Tick::Verify);
    heartbeat.alive();
    assert!(!heartbeat.verify());
    assert_eq!(heartbeat.missed(), 0);
    assert_eq!(heartbeat.tick(), Tick::Ping);
}

// ---- upgrade rejection ------------------------------------------------------------------------

#[test]
fn a_rejected_upgrade_is_a_plain_http_response_with_a_body_length() {
    for (rejection, status, reason) in [
        (UpgradeRejection::Unauthorized, 401, "Unauthorized"),
        (UpgradeRejection::Forbidden, 403, "Forbidden"),
        (UpgradeRejection::Conflict, 409, "Conflict"),
    ] {
        assert_eq!(rejection.status(), status);
        assert_eq!(rejection.reason(), reason);
        let response = rejection.response();
        assert!(response.starts_with(&format!("HTTP/1.1 {status} {reason}\r\n")));
        assert!(response.contains("Connection: close\r\n"));
        assert!(response.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        let body = reason.to_lowercase();
        assert!(response.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(response.ends_with(&format!("\r\n\r\n{body}")));
    }
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod api {
    pub use super::api_extra_gap as extra;
}
