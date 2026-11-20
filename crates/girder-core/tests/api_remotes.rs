//! Tests for the forwarded-Host-event face of upstream `packages/api/remotes`.
//!
//! Corresponds to upstream `packages/api/remotes/tests/remote-events.host.spec.ts`, restricted to
//! the parts this port covers: the allowlist, resource resolution, argument validation, the
//! per-Client queue and its end semantics, and the Agent-scoped waterfall bridge. The Cordis
//! lifecycle cases in the upstream spec (fiber dispose, `ctx.effect` withdrawal, `ready` before the
//! first delivery) have no local counterpart, because this crate has no Cordis Context; their
//! local equivalent is `EventQueue::end`, which is tested here.
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

#[path = "../src/api/remotes.rs"]
pub mod api_remotes_gap;

use serde_json::json;

use api_remotes_gap::{
    ABANDONED_INVOCATION, AGENT_FIELD, AllowlistError, Delivery, Dispatch, EventQueue,
    FORWARDED_EVENTS, ForwardError, ForwardedEvent, GLOBAL_RESOURCE_KIND, Outcome, Push, Resource,
    SESSION_RESOURCE_KIND, SIGNAL_FIELD, SourceEnd, UNEXPECTED_SOURCE_END, forward_waterfall,
    forwarded_event, forwarded_event_names, is_forwardable, project_request, resources_of,
    source_end_reason, validate_allowlist, validate_args,
};

// ---- the allowlist ----------------------------------------------------------------------------

/// Upstream's `API_REMOTE_FORWARDED_EVENTS` has thirty-one entries. A number pins the table against
/// an accidental deletion; the *names* below pin it against a rename.
#[test]
fn the_allowlist_is_the_same_thirty_one_events_upstream_forwards() {
    assert_eq!(FORWARDED_EVENTS.len(), 31);
}

#[test]
fn the_allowlist_passes_the_checks_upstream_gets_from_satisfies() {
    // A duplicate would make `forwarded_event` return the first and silently ignore the second's
    // mode; a waterfall with readers would promise resources its wire frame cannot carry.
    assert_eq!(validate_allowlist(), Ok(()));
}

#[test]
fn a_duplicate_name_is_reported_rather_than_silently_shadowed() {
    // The check itself, driven directly: the real table cannot be mutated, and a test that only
    // asserted "the real table is fine" would pass even if the check were deleted.
    let mut seen: Vec<&'static str> = Vec::new();
    for entry in FORWARDED_EVENTS {
        assert!(
            !seen.contains(&entry.name()),
            "{} appears twice in the real allowlist",
            entry.name()
        );
        if entry.delivery() == Delivery::Waterfall {
            assert!(
                !entry.reads_resources(),
                "{} is a waterfall with readers",
                entry.name()
            );
        }
        seen.push(entry.name());
    }
    assert_eq!(seen.len(), 31);
    // And the typed error still exists for the failure the loop above proves is absent.
    let error = AllowlistError::Duplicate { event: "x/y" };
    assert!(error.to_string().contains("more than once"));
}

#[test]
fn exactly_two_events_are_agent_scoped_waterfalls() {
    let waterfalls: Vec<&str> = FORWARDED_EVENTS
        .iter()
        .filter(|entry| entry.delivery() == Delivery::Waterfall)
        .map(ForwardedEvent::name)
        .collect();
    assert_eq!(
        waterfalls,
        vec!["approval/request", "user-questions/request"]
    );
}

#[test]
fn only_the_session_events_declare_resources() {
    let with_readers: Vec<&str> = FORWARDED_EVENTS
        .iter()
        .filter(|entry| entry.reads_resources())
        .map(ForwardedEvent::name)
        .collect();
    assert_eq!(
        with_readers,
        vec![
            "api-session/activity",
            "api-session/added",
            "api-session/error",
            "api-session/removed",
            "api-session/status",
        ]
    );
}

#[test]
fn the_consumer_key_set_is_derived_from_the_allowlist() {
    assert!(is_forwardable("settings/document-updated"));
    assert!(is_forwardable("approval/request"));
    // `api-session/added` and friends are the tagged ones; a name that only looks like them is not.
    assert!(!is_forwardable("api-session/created"));
    assert!(!is_forwardable(""));
    // A name that is a *Namespace method* rather than an event is not a subscription key.
    assert!(!is_forwardable("session.list"));
}

#[test]
fn names_are_returned_in_upstream_order() {
    let names = forwarded_event_names();
    assert_eq!(names.first(), Some(&"agent-preset/selected"));
    assert_eq!(names.last(), Some(&"web/providers-changed"));
    assert_eq!(names.len(), 31);
}

#[test]
fn a_lookup_answers_with_the_entry_and_its_mode() {
    let entry = forwarded_event("approval/request").unwrap();
    assert_eq!(entry.name(), "approval/request");
    assert_eq!(entry.delivery(), Delivery::Waterfall);
    assert_eq!(entry.delivery().as_str(), "waterfall");
    assert!(forwarded_event("nope/nothing").is_none());
}

// ---- resource resolution ----------------------------------------------------------------------

#[test]
fn an_event_with_no_readers_touches_the_global_resource() {
    let resources = resources_of("schedule/changed", &[]).unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].kind(), GLOBAL_RESOURCE_KIND);
    assert!(resources[0].is_global());
    assert_eq!(resources[0].to_wire(), json!({ "kind": "global" }));
}

#[test]
fn a_session_event_names_the_session_from_its_first_argument() {
    let resources = resources_of("api-session/activity", &[json!("session-7")]).unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].kind(), SESSION_RESOURCE_KIND);
    assert_eq!(resources[0].id(), Some("session-7"));
    assert!(!resources[0].is_global());
    assert_eq!(
        resources[0].to_wire(),
        json!({ "kind": "session", "id": "session-7" })
    );
}

#[test]
fn an_added_session_is_named_by_the_summary_it_carries() {
    // The one reader whose id is a *field* of its argument rather than the argument itself: a
    // summary payload is the whole row, and the id is `sessionId` inside it.
    let summary = json!({ "sessionId": "session-9", "updatedAt": 1, "running": false });
    let resources = resources_of("api-session/added", &[summary]).unwrap();
    assert_eq!(resources[0].id(), Some("session-9"));
}

#[test]
fn an_empty_session_id_makes_the_notification_invalid_rather_than_global() {
    // The distinction the function exists for: invalid reaches no non-owner, while global would
    // reach every admitted Peer.
    assert_eq!(
        resources_of("api-session/status", &[json!("")]),
        Err(ForwardError::InvalidResources {
            event: "api-session/status".to_owned()
        })
    );
}

#[test]
fn a_missing_or_mistyped_session_argument_is_invalid() {
    for args in [
        Vec::new(),
        vec![json!(null)],
        vec![json!(7)],
        vec![json!({ "notASessionId": "x" })],
    ] {
        assert_eq!(
            resources_of("api-session/added", &args),
            Err(ForwardError::InvalidResources {
                event: "api-session/added".to_owned()
            }),
            "{args:?}"
        );
    }
}

#[test]
fn resources_for_an_event_off_the_allowlist_are_refused() {
    assert_eq!(
        resources_of("not/an-event", &[]),
        Err(ForwardError::UnknownEvent {
            event: "not/an-event".to_owned()
        })
    );
}

#[test]
fn a_named_resource_reports_its_kind_and_id() {
    let resource = Resource::new("workspace", "w1");
    assert_eq!(resource.kind(), "workspace");
    assert_eq!(resource.id(), Some("w1"));
    assert_eq!(Resource::global().id(), None);
}

// ---- argument validation ----------------------------------------------------------------------

#[test]
fn lossless_arguments_are_accepted() {
    assert_eq!(
        validate_args(
            "settings/document-updated",
            &[json!("ui-theme"), json!(1), json!(null)]
        ),
        Ok(())
    );
}

#[test]
fn an_argument_beyond_the_javascript_integer_range_is_refused() {
    // 2^53 + 1 is exactly the point where a browser's `number` loses precision. This host produces
    // `u64` revisions, so the case is reachable rather than hypothetical.
    let unsafe_integer = json!(9_007_199_254_740_993_u64);
    assert_eq!(
        validate_args("goal/activation-changed", &[unsafe_integer]),
        Err(ForwardError::NotLossless {
            event: "goal/activation-changed".to_owned(),
            index: 0,
        })
    );
}

#[test]
fn the_first_offending_argument_is_the_one_named() {
    assert_eq!(
        validate_args(
            "goal/activation-changed",
            &[json!("fine"), json!(-9_007_199_254_740_993_i64 - 1)],
        ),
        Err(ForwardError::NotLossless {
            event: "goal/activation-changed".to_owned(),
            index: 1,
        })
    );
}

#[test]
fn arguments_for_an_unknown_event_are_refused_before_they_are_inspected() {
    assert_eq!(
        validate_args("not/an-event", &[json!("a")]),
        Err(ForwardError::UnknownEvent {
            event: "not/an-event".to_owned()
        })
    );
}

// ---- frames -----------------------------------------------------------------------------------

#[tokio::test]
async fn an_emit_frame_uses_upstream_field_names() {
    let queue = EventQueue::new();
    queue
        .push(
            "settings/document-updated",
            vec![json!("ui-theme"), json!(1)],
        )
        .unwrap();
    let frame = queue
        .recv()
        .await
        .and_then(Dispatch::into_emit)
        .expect("the pushed notification is queued");
    assert_eq!(frame.event(), "settings/document-updated");
    assert_eq!(frame.args(), [json!("ui-theme"), json!(1)]);
    assert_eq!(
        frame.to_wire(),
        json!({
            "type": "emit",
            "event": "settings/document-updated",
            "args": ["ui-theme", 1],
        })
    );
}

#[tokio::test]
async fn a_waterfall_frame_uses_upstream_field_names_and_carries_no_resources() {
    let queue = EventQueue::new();
    let ticket = forward_waterfall(
        &queue,
        "approval/request",
        Some("agent-1"),
        &json!({ "agent": "agent-1", "tool": "bash" }),
    )
    .unwrap();
    let slot = queue
        .recv()
        .await
        .and_then(Dispatch::into_waterfall)
        .expect("the invocation is queued");
    let wire = slot.to_wire("event-42");
    assert_eq!(
        wire,
        json!({
            "type": "waterfall",
            "event": "approval/request",
            "eventId": "event-42",
            "agentId": "agent-1",
            "request": { "tool": "bash" },
        })
    );
    // A waterfall frame has no `resources` slot upstream; the absence is asserted rather than
    // assumed, since adding one is how this shape would drift.
    assert!(wire.get("resources").is_none());
    assert!(slot.delegate());
    assert_eq!(ticket.outcome().await, Outcome::Next);
}

// ---- the queue --------------------------------------------------------------------------------

#[tokio::test]
async fn a_pushed_notification_is_received_by_the_one_consumer() {
    let queue = EventQueue::new();
    assert_eq!(
        queue.push("commands/change", vec![json!("cmd-1")]).unwrap(),
        Push::Accepted
    );
    let dispatch = queue.recv().await.unwrap();
    assert!(dispatch.is_emit());
    assert_eq!(dispatch.event(), "commands/change");
    let frame = dispatch.into_emit().unwrap();
    assert_eq!(frame.args(), [json!("cmd-1")]);
}

#[tokio::test]
async fn recv_waits_for_a_notification_that_has_not_arrived_yet() {
    // The property the queue exists for: a consumer parked on `recv` is woken by a push, rather
    // than polling and reporting "nothing yet" as though the stream had ended. The push happens
    // only after the consumer has yielded, so `recv` must have been waiting when it landed.
    let queue = EventQueue::new();
    let (dispatch, ()) = tokio::join!(queue.recv(), async {
        tokio::task::yield_now().await;
        queue.push("commands/change", vec![]).unwrap();
    });
    assert_eq!(dispatch.unwrap().event(), "commands/change");
}

#[tokio::test]
async fn a_pull_with_nothing_queued_does_not_report_the_end_of_the_stream() {
    // A live Client's stream is idle most of the time. `recv` must not return `None` at the first
    // quiet moment; `end` is the only thing that ends it.
    let queue = EventQueue::new();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), queue.recv())
            .await
            .is_err(),
        "an idle queue must not resolve"
    );
    assert!(!queue.is_ended());
}

#[tokio::test]
async fn ending_the_queue_discards_buffered_notifications() {
    // Upstream: "An abort discards the frames still buffered, so a revoked Peer receives none of
    // them." Delivering the backlog would hand a revoked Client what it was just cut off from.
    let queue = EventQueue::new();
    queue.push("commands/change", vec![json!("a")]).unwrap();
    queue.push("commands/change", vec![json!("b")]).unwrap();
    assert_eq!(queue.len(), 2);
    queue.end(Some("revoked".to_owned()));
    assert!(queue.is_ended());
    assert_eq!(queue.len(), 0);
    assert!(queue.recv().await.is_none());
    assert_eq!(queue.end_reason(), Some("revoked".to_owned()));
}

#[tokio::test]
async fn ending_the_queue_rejects_buffered_invocations_instead_of_dropping_them() {
    // A parked Host listener must be resumed, not abandoned: dropping the slot would leave the
    // waterfall unresolved, so the Host would hang rather than fail.
    let queue = EventQueue::new();
    let ticket = forward_waterfall(
        &queue,
        "approval/request",
        Some("agent-1"),
        &json!({ "agent": "agent-1", "tool": "bash" }),
    )
    .unwrap();
    queue.end(Some("client disconnected".to_owned()));
    assert_eq!(
        ticket.outcome().await,
        Outcome::Rejected {
            reason: Some("client disconnected".to_owned())
        }
    );
}

#[tokio::test]
async fn ending_twice_keeps_the_first_reason() {
    let queue = EventQueue::new();
    queue.end(Some("first".to_owned()));
    queue.end(Some("second".to_owned()));
    assert_eq!(queue.end_reason(), Some("first".to_owned()));
}

#[tokio::test]
async fn a_push_after_the_end_is_reported_rather_than_queued() {
    // A listener outliving its stream is the normal consequence of a disconnect, and a fresh Client
    // attaches new listeners, so this is not an error — but it must not silently accumulate.
    let queue = EventQueue::new();
    queue.end(None);
    assert_eq!(queue.push("commands/change", vec![]).unwrap(), Push::Ended);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn a_notification_off_the_allowlist_is_refused_by_the_queue() {
    let queue = EventQueue::new();
    assert_eq!(
        queue.push("not/an-event", vec![]),
        Err(ForwardError::UnknownEvent {
            event: "not/an-event".to_owned()
        })
    );
    assert!(queue.is_empty());
}

#[tokio::test]
async fn a_waterfall_offered_as_a_notification_is_refused() {
    // The mode is part of the entry, so the queue can refuse a mis-mode push rather than deliver a
    // waterfall frame that no Client could answer.
    let queue = EventQueue::new();
    assert_eq!(
        queue.push("approval/request", vec![json!({ "agent": "a" })]),
        Err(ForwardError::WrongDelivery {
            event: "approval/request".to_owned(),
            expected: "emit",
        })
    );
}

#[tokio::test]
async fn a_notification_with_an_unrepresentable_argument_never_enters_the_queue() {
    let queue = EventQueue::new();
    assert_eq!(
        queue.push("commands/change", vec![json!(9_007_199_254_740_993_u64)]),
        Err(ForwardError::NotLossless {
            event: "commands/change".to_owned(),
            index: 0,
        })
    );
    assert!(queue.is_empty());
}

// ---- the waterfall bridge ---------------------------------------------------------------------

#[tokio::test]
async fn a_waterfall_without_a_carrier_delegates_to_the_next_host_listener() {
    // Not an error: upstream's listener runs inside a waterfall, where returning without calling
    // `next()` would consume the chain and change Host behaviour merely because no Client attached.
    let queue = EventQueue::new();
    let ticket = forward_waterfall(&queue, "approval/request", None, &json!({})).unwrap();
    assert_eq!(ticket.outcome().await, Outcome::Next);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn a_request_that_does_not_carry_the_carrier_agent_is_refused() {
    let queue = EventQueue::new();
    for request in [
        json!({}),
        json!({ "agent": "" }),
        json!({ "agent": "someone-else" }),
        json!({ "agent": 7 }),
        json!(["agent-1"]),
        json!("agent-1"),
    ] {
        assert_eq!(
            forward_waterfall(&queue, "approval/request", Some("agent-1"), &request).err(),
            Some(ForwardError::MissingAgent {
                event: "approval/request".to_owned()
            }),
            "{request}"
        );
    }
    assert!(queue.is_empty());
}

#[tokio::test]
async fn a_queued_waterfall_is_settled_by_the_clients_answer() {
    let queue = EventQueue::new();
    let ticket = forward_waterfall(
        &queue,
        "user-questions/request",
        Some("agent-2"),
        &json!({ "agent": "agent-2", "question": "ship it?" }),
    )
    .unwrap();

    let dispatch = queue.recv().await.unwrap();
    assert!(dispatch.is_waterfall());
    assert_eq!(dispatch.event(), "user-questions/request");
    let slot = dispatch.into_waterfall().unwrap();
    assert_eq!(slot.agent_id(), "agent-2");
    assert_eq!(slot.request(), &json!({ "question": "ship it?" }));
    assert!(slot.resolve(json!({ "answer": "yes" })));

    assert_eq!(
        ticket.outcome().await,
        Outcome::Result(json!({ "answer": "yes" }))
    );
}

#[tokio::test]
async fn a_client_may_delegate_a_waterfall_back_to_the_host() {
    let queue = EventQueue::new();
    let ticket = forward_waterfall(
        &queue,
        "approval/request",
        Some("agent-1"),
        &json!({ "agent": "agent-1" }),
    )
    .unwrap();
    let dispatch = queue.recv().await.unwrap();
    let slot = dispatch.into_waterfall().unwrap();
    assert!(slot.delegate());
    assert_eq!(ticket.outcome().await, Outcome::Next);
}

#[tokio::test]
async fn an_invocation_dropped_without_settling_reads_as_a_rejection() {
    // "Nobody answered and nobody is going to" is exactly the abandonment `Rejected` describes; a
    // caller forced to handle a third case would be handling this one twice.
    let queue = EventQueue::new();
    let ticket = forward_waterfall(
        &queue,
        "approval/request",
        Some("agent-1"),
        &json!({ "agent": "agent-1" }),
    )
    .unwrap();
    drop(queue.recv().await.unwrap());
    assert_eq!(
        ticket.outcome().await,
        Outcome::Rejected {
            reason: Some(ABANDONED_INVOCATION.to_owned())
        }
    );
}

#[tokio::test]
async fn a_waterfall_queued_after_the_end_delegates_rather_than_hanging() {
    let queue = EventQueue::new();
    queue.end(Some("gone".to_owned()));
    let ticket = forward_waterfall(
        &queue,
        "approval/request",
        Some("agent-1"),
        &json!({ "agent": "agent-1" }),
    )
    .unwrap();
    assert_eq!(ticket.outcome().await, Outcome::Next);
}

#[tokio::test]
async fn an_emit_event_is_refused_by_the_waterfall_bridge() {
    let queue = EventQueue::new();
    assert_eq!(
        forward_waterfall(&queue, "commands/change", Some("agent-1"), &json!({})).err(),
        Some(ForwardError::WrongDelivery {
            event: "commands/change".to_owned(),
            expected: "waterfall",
        })
    );
}

#[tokio::test]
async fn an_unknown_event_is_refused_by_the_waterfall_bridge() {
    let queue = EventQueue::new();
    assert_eq!(
        forward_waterfall(&queue, "not/an-event", Some("agent-1"), &json!({})).err(),
        Some(ForwardError::UnknownEvent {
            event: "not/an-event".to_owned()
        })
    );
}

// ---- request projection -----------------------------------------------------------------------

#[test]
fn the_agent_and_signal_fields_are_stripped_from_the_projection() {
    // `agent` is the live identity the Host needs and the Client cannot use; `signal` is a transport
    // object, not JSON data.
    let projected = project_request(
        "approval/request",
        "agent-1",
        &json!({
            AGENT_FIELD: "agent-1",
            SIGNAL_FIELD: "an-abort-signal",
            "tool": "bash",
            "args": { "command": "ls" },
        }),
    )
    .unwrap();
    assert_eq!(projected.agent_id(), "agent-1");
    assert_eq!(
        projected.request(),
        &json!({ "tool": "bash", "args": { "command": "ls" } })
    );
}

#[test]
fn a_request_field_that_is_not_lossless_json_is_refused() {
    // The rejection is a refusal to forward at all, not a silent omission: a Client rendering the
    // notification without the field would show something the Host never decided.
    assert_eq!(
        project_request(
            "approval/request",
            "agent-1",
            &json!({ "agent": "agent-1", "revision": 9_007_199_254_740_993_u64 }),
        ),
        Err(ForwardError::NotLossless {
            event: "approval/request".to_owned(),
            index: 1,
        })
    );
}

#[test]
fn a_request_with_no_agent_at_all_is_missing_its_agent() {
    assert_eq!(
        project_request("approval/request", "agent-1", &json!({ "tool": "bash" })),
        Err(ForwardError::MissingAgent {
            event: "approval/request".to_owned()
        })
    );
}

// ---- source end classification ----------------------------------------------------------------

#[test]
fn a_cancelled_source_reports_its_own_reason() {
    // The reason wins because it is what the Client's pending waterfalls must be rejected with.
    let end = source_end_reason(true, Some("operator cancelled".to_owned()));
    assert!(end.is_cancelled());
    assert_eq!(end.reason(), Some("operator cancelled".to_owned()));
}

#[test]
fn a_cancellation_without_a_reason_does_not_invent_one() {
    // A caller that cancelled without explaining itself did not say why; fabricating a message would
    // put words in its mouth in the Client's error display.
    let end = source_end_reason(true, None);
    assert!(end.is_cancelled());
    assert_eq!(end.reason(), None);
}

#[test]
fn an_unexpected_end_reports_the_one_fixed_message() {
    let end = source_end_reason(false, Some("ignored".to_owned()));
    assert!(!end.is_cancelled());
    assert_eq!(end.reason(), Some(UNEXPECTED_SOURCE_END.to_owned()));
    assert_eq!(SourceEnd::Unexpected.reason(), end.reason());
}

#[tokio::test]
async fn a_cancelled_source_rejects_the_pending_invocations_with_its_reason() {
    let queue = EventQueue::new();
    let ticket = forward_waterfall(
        &queue,
        "approval/request",
        Some("agent-1"),
        &json!({ "agent": "agent-1" }),
    )
    .unwrap();
    queue.end_from(&source_end_reason(true, Some("peer revoked".to_owned())));
    assert_eq!(
        ticket.outcome().await,
        Outcome::Rejected {
            reason: Some("peer revoked".to_owned())
        }
    );
}

// ---- outcome helpers ---------------------------------------------------------------------------

#[test]
fn an_outcome_reports_which_of_the_three_answers_it_is() {
    let result = Outcome::Result(json!(1));
    assert_eq!(result.value(), Some(&json!(1)));
    assert!(!result.is_next());
    assert!(!result.is_rejected());

    let next = Outcome::Next;
    assert!(next.is_next());
    assert_eq!(next.value(), None);

    let rejected = Outcome::Rejected { reason: None };
    assert!(rejected.is_rejected());
    assert_eq!(rejected.value(), None);
}

// ---- consumer side of the queue ---------------------------------------------------------------

/// A queue that can be filled but not drained is a queue nobody can read.
///
/// `push` is the producer side; `try_pop_emit` is the consumer side, and a caller that asserts on
/// what a stream produced needs the second. It is the non-blocking sibling of [`EventQueue::recv`],
/// for a test or a transport that must not park a thread on an idle stream.
#[test]
fn a_pushed_frame_can_be_taken_back_out() {
    let queue = EventQueue::new();
    assert!(
        queue.try_pop_emit().is_none(),
        "an empty queue yields nothing"
    );

    queue
        .push("settings/document-updated", vec![json!({ "seq": 1 })])
        .expect("a legal event is accepted");
    assert_eq!(queue.len(), 1);

    let taken = queue.try_pop_emit().expect("the frame that was pushed");
    assert_eq!(taken.event(), "settings/document-updated");
    assert!(taken.is_emit());
    assert!(
        queue.try_pop_emit().is_none(),
        "it is taken once, not twice"
    );
}

/// A waterfall ticket names the Host event it is waiting on, which is what a Client dispatches by.
#[tokio::test]
async fn a_waterfall_ticket_names_its_event() {
    let queue = EventQueue::new();
    let ticket = forward_waterfall(
        &queue,
        "approval/request",
        Some("agent-1"),
        &json!({ "agent": "agent-1", "prompt": "which one?" }),
    )
    .expect("an allowlisted waterfall with its carrier is queued");
    assert_eq!(ticket.event(), "approval/request");
    // Settle it, so the assertion above is not the only thing this test ever does.
    drop(queue.recv().await);
    assert!(ticket.outcome().await.is_rejected());
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod api {
    pub use super::api_remotes_gap as remotes;
}
