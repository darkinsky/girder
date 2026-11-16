//! Tests for the Client half of upstream `packages/api/gateway`.
//!
//! Upstream reference: `packages/api/gateway/src/client/{index,remote-stream,snapshot-stream,
//! stream-client}.ts` (1,726 lines; `journal-stream.ts` is the remaining 580 and is recorded as the
//! open gap). The properties pinned here are the ones that fail *silently* if they regress: a
//! namespace that leaks a half-installed group, a withdrawn method that still reports a value, a
//! scoped call that sends the wrong wire field, a retry counter that never resets, a snapshot delta
//! applied against a baseline from another generation. Each is a behaviour a caller cannot detect
//! from the outside.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `gateway_client_gap`; adding
//! `pub mod gateway_client_gap;` belongs to the integrator. The module is compiled into this test
//! binary directly, so it must not reference `crate::` — inside a `#[path]` module that would name
//! *this* test crate. Every library reference goes through `girder_core::{...}` instead.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/gateway_client_gap.rs"]
mod gateway_client_gap;

use girder_core::typert_gap::{InvocationDescriptor, InvocationParameter, Receiver, Scope};
use serde_json::{Map, Value, json};

use gateway_client_gap::{
    CANCELLED_FAILURE_CODE, CARRIER_FAILURE_CODE, CarrierOutcome, ClientError, ClientRemote,
    ContextIdentity, DispatchError, InvocationOutcome, NAMESPACE_RESERVED_METHODS, NamespaceTable,
    RemoteFailure, RemoteStream, RetryPlan, Selection, SnapshotAction, SnapshotFrame,
    SnapshotStream, SnapshotViolation, StreamHandle, StreamRefusal, UplinkQueue, UplinkQueueError,
    UplinkQueuePoll, Variant, cancelled_failure, carrier_failure, fold_outcome, prepare_invocation,
    rebuilt_failure, remote_service_key, retry_plan, validate_client_descriptor, withdrawn_failure,
};

// ---- fixtures ------------------------------------------------------------------------------------

fn direct(id: &str, namespace: &str, method: &str) -> InvocationDescriptor {
    InvocationDescriptor::new(id, "svc", namespace, method)
}

/// A scoped descriptor whose only lookup parameter is the identity the Client substitutes.
fn scoped(
    id: &str,
    namespace: &str,
    method: &str,
    context: &str,
    wire: &str,
) -> InvocationDescriptor {
    let base = InvocationDescriptor::new(id, "svc", namespace, method)
        .with_parameter(InvocationParameter::lookup("identity", wire, context))
        .with_scope(Scope::new(context, wire));
    InvocationDescriptor {
        receiver: Receiver::Context {
            context: context.to_owned(),
            wire: wire.to_owned(),
        },
        ..base
    }
}

fn table_with(descriptors: &[InvocationDescriptor]) -> NamespaceTable {
    let table = NamespaceTable::new();
    table
        .mount_contribution(descriptors)
        .unwrap_or_else(|error| panic!("mount, got {error}"));
    table
}

fn selection(table: &NamespaceTable, namespace: &str, method: &str) -> Selection {
    table
        .select(namespace, method, None)
        .unwrap_or_else(|error| panic!("select, got {error}"))
}

// ---- the service key -----------------------------------------------------------------------------

#[test]
fn a_namespace_service_is_published_under_the_remote_prefix() {
    assert_eq!(remote_service_key("session"), "remote.session");
    assert_eq!(NamespaceTable::service_key("workspace"), "remote.workspace");
    // The key is what upstream's `remoteServiceKey` produces, so a nested namespace keeps its dots.
    assert_eq!(remote_service_key("a.b"), "remote.a.b");
}

// ---- contribution mounting -----------------------------------------------------------------------

#[test]
fn a_contribution_installs_every_method_and_lists_its_namespaces() {
    let table = table_with(&[
        direct("a", "session", "list"),
        direct("b", "session", "create"),
        direct("c", "workspace", "open"),
    ]);
    assert_eq!(table.namespaces(), vec!["session", "workspace"]);
    assert!(table.contains("session"));
    assert_eq!(
        table.methods("session"),
        vec![
            gateway_client_gap::MethodView {
                method: "create".to_owned(),
                direct: true,
                scoped: false,
            },
            gateway_client_gap::MethodView {
                method: "list".to_owned(),
                direct: true,
                scoped: false,
            },
        ]
    );
    // A namespace that was never mounted lists nothing rather than panicking.
    assert!(table.methods("nope").is_empty());
    assert!(!table.contains("nope"));
}

#[test]
fn a_repeated_method_inside_one_contribution_mounts_nothing() {
    let table = NamespaceTable::new();
    let error = table
        .mount_contribution(&[
            direct("a", "session", "list"),
            direct("b", "session", "list"),
        ])
        .expect_err("a repeated direct method is refused");
    assert_eq!(
        error,
        ClientError::DuplicateMethod {
            endpoint: "session/list".to_owned(),
            kind: "direct",
        }
    );
    // Validation runs before anything is stored, so the rejection left no trace. Upstream asserts
    // exactly this with `expect(ctx.typert.remotes.list()).toEqual([])`.
    assert!(table.namespaces().is_empty());
}

#[test]
fn a_method_already_mounted_by_another_contribution_is_refused() {
    let table = table_with(&[direct("a", "session", "list")]);
    let error = table
        .mount_contribution(&[direct("b", "session", "list")])
        .expect_err("a second mount of one endpoint is refused");
    assert_eq!(
        error,
        ClientError::DuplicateMethod {
            endpoint: "session/list".to_owned(),
            kind: "direct",
        }
    );
    assert_eq!(table.methods("session").len(), 1);
}

#[test]
fn direct_and_scoped_variants_of_one_method_coexist() {
    // The two variants are independent slots on one method record. A contribution may fill the one
    // the other left empty, and removal of one must not disturb the other — that is what upstream's
    // `remove(kind, method, token)` guard exists for.
    let table = table_with(&[direct("a", "session", "get")]);
    table
        .mount_contribution(&[scoped("b", "session", "get", "session", "sessionId")])
        .expect("the scoped variant of a direct method is not a duplicate");
    assert!(table.has("session", "get", Variant::Direct));
    assert!(table.has("session", "get", Variant::Scoped));
    assert_eq!(table.methods("session").len(), 1);
}

#[test]
fn one_contribution_may_install_both_variants_of_one_method() {
    // Two descriptors naming the same namespace *and* the same method, in different variants, in
    // one contribution: upstream's per-kind tables make this legal only across contributions, and
    // making it legal here too would let one contribution install two dispatch targets for one name.
    let table = NamespaceTable::new();
    table
        .mount_contribution(&[
            direct("a", "session", "get"),
            scoped("b", "session", "get", "session", "sessionId"),
        ])
        .expect("direct and scoped are distinct namespaces of the record");
    assert!(table.has("session", "get", Variant::Direct));
    assert!(table.has("session", "get", Variant::Scoped));
}

#[test]
fn a_method_may_not_shadow_the_namespace_service() {
    for reserved in NAMESPACE_RESERVED_METHODS {
        let table = NamespaceTable::new();
        let error = table
            .mount_contribution(&[direct("a", "session", reserved)])
            .expect_err("a reserved method name is refused");
        assert_eq!(
            error,
            ClientError::ReservedMethod {
                endpoint: format!("session/{reserved}"),
            },
            "method {reserved} must be reserved"
        );
        assert!(table.namespaces().is_empty());
    }
}

#[test]
fn a_namespace_may_not_take_the_remote_service_name() {
    let table = NamespaceTable::new();
    let error = table
        .mount_contribution(&[direct("a", "remote", "invoke")])
        .expect_err("the Remote service's own name is refused");
    assert_eq!(
        error,
        ClientError::NamespaceConflict {
            namespace: "remote".to_owned(),
        }
    );
}

#[test]
fn a_descriptor_without_a_wire_field_is_refused_before_mounting() {
    let table = NamespaceTable::new();
    let mut descriptor = direct("a", "session", "list");
    descriptor
        .parameters
        .push(InvocationParameter::json("query").on_wire(""));
    let error = table
        .mount_contribution(&[descriptor])
        .expect_err("a parameter with no wire field cannot be invoked");
    assert_eq!(error.code(), "client/declaration");
    assert!(table.namespaces().is_empty());
}

// ---- unmounting ----------------------------------------------------------------------------------

#[test]
fn unmounting_retires_every_method_and_drops_the_namespace() {
    let table = NamespaceTable::new();
    let mount = table
        .mount_contribution(&[
            direct("a", "session", "list"),
            direct("b", "session", "create"),
        ])
        .expect("mount");
    assert_eq!(mount.endpoints(), vec!["session/list", "session/create"]);
    assert_eq!(mount.unmount(), 2);
    assert!(table.namespaces().is_empty());
    assert!(!table.contains("session"));
}

#[test]
fn unmounting_twice_is_inert() {
    let table = NamespaceTable::new();
    let mount = table
        .mount_contribution(&[direct("a", "session", "list")])
        .expect("mount");
    assert_eq!(mount.unmount(), 1);
    assert!(mount.is_unmounted());
    assert_eq!(mount.unmount(), 0);
}

#[test]
fn unmounting_one_variant_keeps_the_other() {
    let table = NamespaceTable::new();
    let direct_mount = table
        .mount_contribution(&[direct("a", "session", "get")])
        .expect("mount direct");
    table
        .mount_contribution(&[scoped("b", "session", "get", "session", "sessionId")])
        .expect("mount scoped");
    assert_eq!(direct_mount.unmount(), 1);
    assert!(!table.has("session", "get", Variant::Direct));
    assert!(table.has("session", "get", Variant::Scoped));
    assert!(table.contains("session"));
}

#[test]
fn a_stale_mount_cannot_remove_a_newer_installation_of_the_same_endpoint() {
    // The token guard is the whole reason `remove` takes one: a disposer that runs late must not
    // retire the method a later installation put under the same name.
    let table = NamespaceTable::new();
    let stale = table
        .mount_contribution(&[direct("a", "session", "list")])
        .expect("first mount");
    let fresh = table.replace(&direct("b", "session", "list"));
    assert_eq!(stale.unmount(), 1, "the stale token is still retired");
    assert!(
        table.has("session", "list", Variant::Direct),
        "but the newer installation survives"
    );
    let selected = table.select("session", "list", None).expect("select");
    assert_eq!(selected.token.sequence(), fresh.sequence());
}

#[test]
fn a_withdrawn_selection_reports_withdrawal_but_still_names_its_descriptor() {
    let table = NamespaceTable::new();
    let mount = table
        .mount_contribution(&[direct("a", "session", "list")])
        .expect("mount");
    let selected = table.select("session", "list", None).expect("select");
    assert!(selected.still_mounted().is_ok());
    mount.unmount();
    assert_eq!(
        selected.still_mounted(),
        Err(DispatchError::Withdrawn {
            endpoint: "session/list".to_owned(),
        })
    );
    // The selection keeps the descriptor it was made with, so a failure can still name the method
    // that was called rather than the one that replaced it.
    assert_eq!(selected.descriptor.id, "a");
    // A selection of a method nobody mounted cannot be built at all.
    assert_eq!(
        table.select("session", "list", None),
        Err(DispatchError::Withdrawn {
            endpoint: "session/list".to_owned(),
        })
    );
}

// ---- selection ----------------------------------------------------------------------------------

#[test]
fn a_scoped_variant_wins_only_when_the_caller_has_an_identity() {
    let table = table_with(&[
        direct("a", "session", "get"),
        scoped("b", "session", "get", "session", "sessionId"),
    ]);
    // No identity: the direct variant is selected, because the Context projection cannot be filled.
    assert_eq!(selection(&table, "session", "get").variant, Variant::Direct);
    // With an identity the scoped variant wins, even though the direct one is installed.
    let scoped_selection = table
        .select("session", "get", Some(&json!("sess-1")))
        .expect("select");
    assert_eq!(scoped_selection.variant, Variant::Scoped);
    assert_eq!(scoped_selection.descriptor.id, "b");
}

#[test]
fn a_scoped_only_method_is_still_selectable_without_an_identity() {
    // Upstream's third branch: no identity, no direct variant, so the scoped descriptor is used
    // unbound. Refusing here instead would make a method uncallable that the Host mounted.
    let table = table_with(&[scoped("a", "session", "get", "session", "sessionId")]);
    let selected = selection(&table, "session", "get");
    assert_eq!(selected.variant, Variant::Scoped);
    assert_eq!(selected.descriptor.id, "a");
}

#[test]
fn selecting_a_method_nobody_mounted_is_a_withdrawal() {
    let table = NamespaceTable::new();
    assert_eq!(
        table.select("session", "list", None),
        Err(DispatchError::Withdrawn {
            endpoint: "session/list".to_owned(),
        })
    );
}

// ---- invocation arguments -----------------------------------------------------------------------

#[test]
fn arguments_are_placed_by_wire_field_in_declaration_order() {
    let mut descriptor = direct("a", "session", "prompt");
    descriptor
        .parameters
        .push(InvocationParameter::json("session").on_wire("sessionId"));
    descriptor
        .parameters
        .push(InvocationParameter::json("text").on_wire("prompt"));
    let table = table_with(&[descriptor]);
    let selected = selection(&table, "session", "prompt");
    let prepared =
        prepare_invocation(&selected, None, &[json!("s1"), json!("hello")]).expect("prepare");
    assert_eq!(prepared.endpoint, "session/prompt");
    assert_eq!(
        prepared.payload(),
        json!({ "args": { "sessionId": "s1", "prompt": "hello" } })
    );
    assert!(!prepared.cancellable);
}

#[test]
fn a_scoped_parameter_is_filled_from_the_identity_not_from_an_argument() {
    let table = table_with(&[scoped("a", "session", "get", "session", "sessionId")]);
    let selected = table
        .select("session", "get", Some(&json!("sess-1")))
        .expect("select");
    let identity = ContextIdentity::new(json!("sess-1"));
    // Zero business arguments: the identity is not one.
    let prepared = prepare_invocation(&selected, Some(&identity), &[]).expect("prepare");
    assert_eq!(
        prepared.payload(),
        json!({ "args": { "sessionId": "sess-1" } })
    );

    // One argument for a method that takes none is the arity error, not a silent override.
    assert_eq!(
        prepare_invocation(&selected, Some(&identity), &[json!("other")]),
        Err(ClientError::Arity {
            endpoint: "session/get".to_owned(),
            expected: 0,
            got: 1,
            cancellable: false,
        })
    );
}

#[test]
fn a_scoped_call_without_an_identity_names_the_missing_context() {
    let table = table_with(&[scoped("a", "session", "get", "session", "sessionId")]);
    let selected = selection(&table, "session", "get");
    assert_eq!(selected.variant, Variant::Scoped);
    assert_eq!(
        prepare_invocation(&selected, None, &[]),
        Err(ClientError::ScopeUnavailable {
            endpoint: "session/get".to_owned(),
            context: "session".to_owned(),
        })
    );
}

#[test]
fn an_undefined_argument_is_omitted_rather_than_sent_as_null() {
    // `null` and an absent field decode differently when a parameter accepts `undefined`, so the
    // two must not be collapsed on the way out.
    let mut descriptor = direct("a", "session", "list");
    descriptor
        .parameters
        .push(InvocationParameter::json("limit").accepting_undefined());
    let table = table_with(&[descriptor]);
    let selected = selection(&table, "session", "list");
    let prepared = prepare_invocation(&selected, None, &[Value::Null]).expect("prepare");
    assert!(prepared.args.is_empty());
    let prepared = prepare_invocation(&selected, None, &[json!(10)]).expect("prepare with a value");
    assert_eq!(prepared.args.get("limit"), Some(&json!(10)));
}

#[test]
fn arity_is_exact_and_a_cancellation_is_allowed_only_as_the_last_argument() {
    let mut descriptor = direct("a", "session", "prompt");
    descriptor
        .parameters
        .push(InvocationParameter::json("text"));
    descriptor = descriptor.with_cancellation();
    let table = table_with(&[descriptor]);
    let selected = selection(&table, "session", "prompt");

    // The business argument alone is the normal call.
    let prepared = prepare_invocation(&selected, None, &[json!("hi")]).expect("prepare");
    assert!(!prepared.cancellable);

    // Business argument plus cancellation: accepted, and reported as cancellable.
    let prepared = prepare_invocation(&selected, None, &[json!("hi"), json!({})]).expect("prepare");
    assert!(prepared.cancellable);
    assert_eq!(prepared.payload(), json!({ "args": { "text": "hi" } }));

    // Neither zero nor two business arguments.
    for (got, arguments) in [
        (0usize, vec![]),
        (3usize, vec![json!("a"), json!("b"), json!("c")]),
    ] {
        assert_eq!(
            prepare_invocation(&selected, None, &arguments),
            Err(ClientError::Arity {
                endpoint: "session/prompt".to_owned(),
                expected: 1,
                got,
                cancellable: true,
            })
        );
    }
}

#[test]
fn a_caller_cannot_smuggle_a_field_the_descriptor_did_not_declare() {
    // The output is built field by field, so a value for an undeclared field has nowhere to go.
    let mut descriptor = direct("a", "session", "list");
    descriptor
        .parameters
        .push(InvocationParameter::json("limit"));
    let table = table_with(&[descriptor]);
    let selected = selection(&table, "session", "list");
    let prepared = prepare_invocation(&selected, None, &[json!({ "limit": 5, "peer": "x" })])
        .expect("prepare");
    assert_eq!(prepared.args.len(), 1);
    assert!(prepared.args.get("peer").is_none());
}

#[test]
fn a_scope_that_does_not_select_its_only_lookup_parameter_is_refused() {
    // Two lookup parameters make the substitution ambiguous, which upstream refuses at mount time.
    let base = InvocationDescriptor::new("a", "svc", "session", "get")
        .with_parameter(InvocationParameter::lookup("one", "one", "session"))
        .with_parameter(InvocationParameter::lookup("two", "two", "session"))
        .with_scope(Scope::new("session", "one"));
    let descriptor = InvocationDescriptor {
        receiver: Receiver::Context {
            context: "session".to_owned(),
            wire: "one".to_owned(),
        },
        ..base
    };
    let error = validate_client_descriptor(&descriptor).expect_err("ambiguous scope");
    assert_eq!(error.code(), "client/declaration");
    assert!(error.to_string().contains("session/get"));
}

#[test]
fn a_context_receiver_with_no_wire_identity_is_refused() {
    let base = InvocationDescriptor::new("a", "svc", "session", "get");
    let descriptor = InvocationDescriptor {
        receiver: Receiver::Context {
            context: "session".to_owned(),
            wire: String::new(),
        },
        ..base
    };
    let error = validate_client_descriptor(&descriptor).expect_err("no wire identity");
    assert_eq!(error.code(), "client/declaration");
}

// ---- failure folding ----------------------------------------------------------------------------

#[test]
fn a_host_answer_is_passed_through_and_a_host_failure_keeps_its_code_verbatim() {
    let table = table_with(&[direct("a", "session", "list")]);
    let selected = selection(&table, "session", "list");
    assert_eq!(
        fold_outcome(
            &selected,
            CarrierOutcome::Answered {
                ok: true,
                payload: json!([1, 2]),
            }
        ),
        Ok(InvocationOutcome::Ok(json!([1, 2])))
    );
    let failure = fold_outcome(
        &selected,
        CarrierOutcome::Answered {
            ok: false,
            payload: json!({
                "code": "gateway/newer-code-this-build-does-not-declare",
                "message": "boom",
                "details": { "endpoint": "session/list" },
            }),
        },
    )
    .expect("fold")
    .failure()
    .cloned()
    .expect("a failure");
    // Verbatim: a code outside this Client's vocabulary still surfaces, so a newer Host stays
    // readable rather than turning into "unknown".
    assert_eq!(
        failure.code,
        "gateway/newer-code-this-build-does-not-declare"
    );
    assert_eq!(
        failure.details.get("endpoint"),
        Some(&json!("session/list"))
    );
}

#[test]
fn a_carrier_throw_folds_to_internal_and_a_caller_abort_to_cancelled() {
    let table = table_with(&[direct("a", "session", "list")]);
    let selected = selection(&table, "session", "list");

    let thrown = fold_outcome(&selected, CarrierOutcome::Threw("socket closed".to_owned()))
        .expect("fold")
        .failure()
        .cloned()
        .expect("a failure");
    assert_eq!(thrown.code, CARRIER_FAILURE_CODE);
    assert!(thrown.message.contains("session/list"));
    assert!(thrown.message.contains("socket closed"));

    let aborted = fold_outcome(&selected, CarrierOutcome::Aborted)
        .expect("fold")
        .failure()
        .cloned()
        .expect("a failure");
    assert_eq!(aborted.code, CANCELLED_FAILURE_CODE);
}

#[test]
fn an_abort_after_a_withdrawal_still_reports_the_withdrawal() {
    // The mount check runs before the fold: a value or failure from a method the Client no longer
    // has cannot be attributed, so the call reports that instead of a misleading carrier outcome.
    let table = NamespaceTable::new();
    let mount = table
        .mount_contribution(&[direct("a", "session", "list")])
        .expect("mount");
    let selected = table.select("session", "list", None).expect("select");
    mount.unmount();
    assert_eq!(
        fold_outcome(&selected, CarrierOutcome::Aborted),
        Err(ClientError::Withdrawn {
            endpoint: "session/list".to_owned(),
        })
    );
}

#[test]
fn a_malformed_host_failure_is_reported_as_internal_with_the_payload_kept() {
    let non_object = rebuilt_failure(json!("nope"));
    assert_eq!(non_object.code, CARRIER_FAILURE_CODE);
    assert_eq!(non_object.details.get("error"), Some(&json!("nope")));

    let no_code = rebuilt_failure(json!({ "message": "no code here" }));
    assert_eq!(no_code.code, CARRIER_FAILURE_CODE);
    assert!(no_code.details.contains_key("error"));

    // A code without a message is still usable: a missing diagnostic is not a missing failure.
    let bare = rebuilt_failure(json!({ "code": "gateway/bad-request" }));
    assert_eq!(bare.code, "gateway/bad-request");
    assert!(bare.message.is_empty());
    assert!(bare.details.is_empty());
}

#[test]
fn the_three_folders_produce_the_codes_a_caller_branches_on() {
    assert_eq!(carrier_failure("a/b", "x").code, CARRIER_FAILURE_CODE);
    assert_eq!(cancelled_failure("a/b").code, CANCELLED_FAILURE_CODE);
    assert_eq!(withdrawn_failure("a/b").code, CARRIER_FAILURE_CODE);
    assert!(
        withdrawn_failure("a/b")
            .message
            .contains("no longer mounted")
    );
    assert_eq!(
        RemoteFailure::new("c", "m", Map::new()).to_value(),
        json!({ "code": "c", "message": "m", "details": {} })
    );
    assert!(
        RemoteFailure::new("c", "m", Map::new())
            .to_string()
            .starts_with("c: m")
    );
}

// ---- the uplink queue ---------------------------------------------------------------------------

#[test]
fn the_uplink_hands_items_over_in_order_then_ends_on_half_close() {
    let queue = UplinkQueue::new("session/prompt");
    assert_eq!(queue.endpoint(), "session/prompt");
    assert_eq!(queue.poll(), UplinkQueuePoll::Pending);
    queue.push(json!(1)).expect("push");
    queue.push(json!(2)).expect("push");
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.poll(), UplinkQueuePoll::Item(json!(1)));
    assert_eq!(queue.poll(), UplinkQueuePoll::Item(json!(2)));
    // Half-close drains what was buffered and only then reports the end.
    queue.end();
    assert!(queue.is_ended());
    assert_eq!(queue.poll(), UplinkQueuePoll::Done);
}

#[test]
fn a_terminated_uplink_discards_its_buffer_and_refuses_further_items() {
    let queue = UplinkQueue::new("session/prompt");
    queue.push(json!(1)).expect("push");
    queue.close();
    assert!(queue.is_closed());
    assert!(queue.is_empty(), "the buffer is discarded, not drained");
    assert_eq!(queue.poll(), UplinkQueuePoll::Done);
    assert_eq!(queue.push(json!(2)), Err(UplinkQueueError::Terminated));
    // A half-close after termination changes nothing, so `is_ended` stays false.
    queue.end();
    assert!(!queue.is_ended());
}

#[test]
fn pushing_after_a_half_close_is_refused_with_the_ended_diagnostic() {
    let queue = UplinkQueue::new("session/prompt");
    queue.end();
    queue.end(); // idempotent
    assert_eq!(queue.push(json!(1)), Err(UplinkQueueError::Ended));
    assert_eq!(
        UplinkQueueError::Ended.message("session/prompt"),
        "client api: session/prompt uplink was ended"
    );
    assert_eq!(
        UplinkQueueError::Terminated.message("session/prompt"),
        "client api: session/prompt stream has terminated"
    );
}

#[test]
fn a_value_javascript_could_not_represent_exactly_is_refused() {
    // A number beyond `Number.MAX_SAFE_INTEGER` would arrive at the Host as a different value, so it
    // never reaches the queue at all.
    let queue = UplinkQueue::new("a/b");
    assert_eq!(
        queue.push(json!(9_007_199_254_740_993_i64)),
        Err(UplinkQueueError::Terminated)
    );
    assert!(queue.is_empty());
    assert_eq!(queue.push(json!(9_007_199_254_740_991_i64)), Ok(()));
}

// ---- the stream handle --------------------------------------------------------------------------

#[test]
fn a_handle_queues_before_its_first_read_and_disposes_once() {
    let handle = StreamHandle::new("session/stream");
    assert_eq!(handle.endpoint(), "session/stream");
    // `send` before the downlink is consumed must queue behind the opening frame, not be lost.
    handle.send(json!("first")).expect("send");
    assert_eq!(handle.uplink().len(), 1);
    handle.consume().expect("the first consumer");
    assert_eq!(handle.consume(), Err(StreamRefusal::Taken));

    assert!(handle.dispose());
    assert!(!handle.dispose(), "disposal is idempotent");
    assert!(handle.is_disposed());
    // A disposed handle's buffer was discarded, so `send` reports termination rather than accepting
    // an item nobody will deliver.
    assert_eq!(
        handle.send(json!("late")),
        Err(UplinkQueueError::Terminated)
    );
}

#[test]
fn half_closing_a_handle_keeps_the_queued_items_readable() {
    let handle = StreamHandle::new("session/stream");
    handle.send(json!(1)).expect("send");
    handle.end();
    assert!(handle.uplink().is_ended());
    assert_eq!(handle.uplink().poll(), UplinkQueuePoll::Item(json!(1)));
    assert_eq!(handle.uplink().poll(), UplinkQueuePoll::Done);
}

// ---- retry pacing --------------------------------------------------------------------------------

#[test]
fn retry_pacing_waits_only_when_there_is_no_carrier() {
    // A live carrier: retry immediately on the first attempt and give up afterwards, because a
    // carrier that keeps coming back while the stream keeps failing is not the carrier's fault.
    assert_eq!(
        retry_plan(true, 1),
        RetryPlan {
            wait: false,
            give_up: false
        }
    );
    assert_eq!(
        retry_plan(true, 2),
        RetryPlan {
            wait: false,
            give_up: true
        }
    );
    // No carrier: wait, at any attempt, because reconnecting into a socket that is not there is a
    // busy loop.
    for attempt in [1u32, 2, 9] {
        assert_eq!(
            retry_plan(false, attempt),
            RetryPlan {
                wait: true,
                give_up: false
            }
        );
    }
}

// ---- logical stream supervision ------------------------------------------------------------------

#[test]
fn generations_are_monotone_and_acceptance_resets_the_attempt_counter() {
    let stream = RemoteStream::new("session.stream");
    assert_eq!(stream.name(), "session.stream");
    assert_eq!(stream.generation(), 0);
    stream.take().expect("the single consumer");

    let first = stream.advance().expect("first generation");
    assert_eq!(first, 1);
    // Two carrier losses with a live carrier: the second one gives up.
    assert_eq!(
        stream.carrier_lost(true),
        RetryPlan {
            wait: false,
            give_up: false
        }
    );
    assert_eq!(stream.attempt(), 1);
    assert_eq!(
        stream.carrier_lost(true),
        RetryPlan {
            wait: false,
            give_up: true
        }
    );
    assert_eq!(stream.attempt(), 2);

    // Acceptance is progress: the attempt counter goes back to zero.
    assert!(stream.accept(first));
    assert_eq!(stream.attempt(), 0);
    assert!(!stream.accept(first), "a second acceptance changes nothing");
    // An acceptance for a generation that is not current is ignored rather than resetting pacing.
    let second = stream.advance().expect("second generation");
    assert_eq!(second, 2);
    assert!(!stream.accept(first));
    assert_eq!(stream.attempt(), 0);
    // A new generation starts unaccepted.
    assert!(!stream.accepted());
    let _ = stream.accept(second);
    assert!(stream.accepted());
}

#[test]
fn a_restart_resets_pacing_and_a_taken_stream_has_one_consumer() {
    let stream = RemoteStream::new("s");
    stream.take().expect("first consumer");
    assert_eq!(stream.take(), Err(StreamRefusal::Taken));

    stream.advance().expect("advance");
    stream.carrier_lost(false);
    stream.carrier_lost(false);
    assert_eq!(stream.attempt(), 2);
    assert_eq!(stream.restart(), 1);
    assert_eq!(stream.attempt(), 0, "a restart is a fresh start");
    assert_eq!(stream.revision(), 1);
}

#[test]
fn a_terminal_failure_is_published_once_and_stops_the_stream() {
    let stream = RemoteStream::new("s");
    stream.take().expect("take");
    stream.advance().expect("advance");
    assert!(stream.fail("protocol violation"));
    assert!(!stream.fail("a second failure is ignored"));
    assert_eq!(
        stream.terminal_failure().as_deref(),
        Some("protocol violation")
    );
    assert_eq!(stream.advance(), Err(StreamRefusal::Failed));
    // Disposal is still possible, and idempotent.
    assert!(stream.dispose());
    assert!(!stream.dispose());
    assert!(stream.is_disposed());
    assert_eq!(stream.take(), Err(StreamRefusal::Disposed));
}

#[test]
fn a_disposed_stream_cannot_advance_and_accepts_nothing() {
    let stream = RemoteStream::new("s");
    stream.take().expect("take");
    let generation = stream.advance().expect("advance");
    stream.dispose();
    assert_eq!(stream.advance(), Err(StreamRefusal::Disposed));
    assert!(!stream.accept(generation));
    // A restart on a disposed stream is inert rather than a resurrection.
    assert_eq!(stream.restart(), 0);
}

// ---- snapshot streams ---------------------------------------------------------------------------

#[test]
fn a_snapshot_stream_requires_a_baseline_before_its_first_delta() {
    let stream = RemoteStream::new("board.snapshot");
    let snapshot = SnapshotStream::new("board.snapshot", stream.clone());
    stream.take().expect("take");
    stream.advance().expect("advance");

    assert_eq!(
        snapshot.accept(&SnapshotFrame::Delta::<Value, Value>(json!({ "x": 1 }))),
        Err(SnapshotViolation::DeltaBeforeSnapshot {
            name: "board.snapshot".to_owned(),
        })
    );
    assert_eq!(
        snapshot.accept(&SnapshotFrame::Snapshot::<Value, Value>(
            json!({ "items": [] })
        )),
        Ok(SnapshotAction::Replace)
    );
    assert!(snapshot.snapshot_seen());
    // Acceptance for pacing follows the domain application, not the protocol check.
    assert!(!stream.accepted());
    snapshot.applied();
    assert!(stream.accepted());
    assert_eq!(
        snapshot.accept(&SnapshotFrame::Delta::<Value, Value>(json!({ "x": 1 }))),
        Ok(SnapshotAction::Update)
    );
    assert_eq!(snapshot.replaced(), 1);
    assert_eq!(snapshot.updated(), 1);
}

#[test]
fn a_second_baseline_in_one_generation_is_a_protocol_violation() {
    let stream = RemoteStream::new("s");
    let snapshot = SnapshotStream::new("s", stream.clone());
    stream.take().expect("take");
    stream.advance().expect("advance");
    snapshot
        .accept(&SnapshotFrame::Snapshot::<Value, Value>(json!(1)))
        .expect("first baseline");
    assert_eq!(
        snapshot.accept(&SnapshotFrame::Snapshot::<Value, Value>(json!(2))),
        Err(SnapshotViolation::RepeatedSnapshot {
            name: "s".to_owned(),
        })
    );
    assert_eq!(
        snapshot.replaced(),
        1,
        "the refused baseline was not counted"
    );
}

#[test]
fn a_new_generation_accepts_a_replacement_baseline_and_resets_the_expectation() {
    // The published model survives the reconnect; the *generation* is what allows a new baseline.
    let stream = RemoteStream::new("s");
    let snapshot = SnapshotStream::new("s", stream.clone());
    stream.take().expect("take");
    stream.advance().expect("advance");
    snapshot
        .accept(&SnapshotFrame::Snapshot::<Value, Value>(json!("gen-1")))
        .expect("baseline one");
    snapshot.applied();

    stream.advance().expect("advance");
    assert!(
        !snapshot.snapshot_seen(),
        "a new generation expects its own baseline"
    );
    assert_eq!(
        snapshot.accept(&SnapshotFrame::Delta::<Value, Value>(json!("too early"))),
        Err(SnapshotViolation::DeltaBeforeSnapshot {
            name: "s".to_owned(),
        })
    );
    assert_eq!(
        snapshot.accept(&SnapshotFrame::Snapshot::<Value, Value>(json!("gen-2"))),
        Ok(SnapshotAction::Replace)
    );
    assert_eq!(snapshot.replaced(), 2);
    assert_eq!(snapshot.restart(), 1);
    assert_eq!(snapshot.name(), "s");
    assert_eq!(snapshot.stream().name(), "s");
    assert!(snapshot.dispose());
    assert!(!snapshot.dispose());
}

#[test]
fn a_snapshot_violation_names_the_stream() {
    let violation = SnapshotViolation::DeltaBeforeSnapshot {
        name: "board".to_owned(),
    };
    assert!(violation.to_string().contains("board"));
    assert!(
        violation
            .to_string()
            .contains("before its opening snapshot")
    );
    let repeated = SnapshotViolation::RepeatedSnapshot {
        name: "board".to_owned(),
    };
    assert!(
        repeated
            .to_string()
            .contains("more than one opening snapshot")
    );
}

// ---- the client service ---------------------------------------------------------------------------

#[test]
fn the_service_owns_namespaces_and_streams_across_a_teardown() {
    let client = ClientRemote::new();
    let mount = client
        .mount(&[
            direct("a", "session", "list"),
            direct("b", "workspace", "open"),
        ])
        .expect("mount");
    assert_eq!(
        client.namespaces().namespaces(),
        vec!["session", "workspace"]
    );

    let stream = client.open_stream("session.stream").expect("open stream");
    assert_eq!(client.streams(), vec!["session.stream".to_owned()]);
    // A duplicate stream name would be disposed by the other's teardown.
    assert_eq!(
        client.open_stream("session.stream").unwrap_err(),
        ClientError::DuplicateMethod {
            endpoint: "session.stream".to_owned(),
            kind: "stream",
        }
    );

    // Teardown retires both, which is the sequence upstream gets from Cordis effects.
    assert_eq!(mount.unmount(), 2);
    assert!(client.streams().contains(&"session.stream".to_owned()));
    assert!(client.close_stream("session.stream"));
    assert!(stream.is_disposed());
    assert!(client.streams().is_empty());
    assert!(!client.close_stream("session.stream"));
    assert_eq!(client.dispose_streams(), 0);
}

#[test]
fn disposing_the_service_stops_every_running_stream() {
    let client = ClientRemote::new();
    let _first = client.open_stream("a").expect("a");
    let _second = client.open_stream("b").expect("b");
    assert_eq!(client.dispose_streams(), 2);
    assert!(client.streams().is_empty());
}

// ---- failures render ----------------------------------------------------------------------------

#[test]
fn every_client_error_names_the_endpoint_it_refused() {
    let cases: Vec<(ClientError, &str)> = vec![
        (
            ClientError::DuplicateMethod {
                endpoint: "a/b".to_owned(),
                kind: "direct",
            },
            "a/b",
        ),
        (
            ClientError::ReservedMethod {
                endpoint: "a/empty".to_owned(),
            },
            "a/empty",
        ),
        (
            ClientError::NamespaceConflict {
                namespace: "remote".to_owned(),
            },
            "remote",
        ),
        (
            ClientError::Arity {
                endpoint: "a/b".to_owned(),
                expected: 2,
                got: 1,
                cancellable: true,
            },
            "a/b",
        ),
        (
            ClientError::ScopeUnavailable {
                endpoint: "a/b".to_owned(),
                context: "session".to_owned(),
            },
            "a/b",
        ),
        (
            ClientError::Withdrawn {
                endpoint: "a/b".to_owned(),
            },
            "a/b",
        ),
        (
            ClientError::NamespaceNotStarted {
                namespace: "a".to_owned(),
            },
            "a",
        ),
        (
            ClientError::Declaration {
                endpoint: "a/b".to_owned(),
                reason: "no wire field".to_owned(),
            },
            "a/b",
        ),
    ];
    for (error, expected) in cases {
        assert!(
            error.to_string().contains(expected),
            "{error} must name {expected}"
        );
        assert!(error.code().starts_with("client/"), "{}", error.code());
    }
}

#[test]
fn a_withdrawal_from_a_selection_folds_into_the_client_error() {
    let table = NamespaceTable::new();
    let mount = table
        .mount_contribution(&[direct("a", "session", "list")])
        .expect("mount");
    let selected = table.select("session", "list", None).expect("select");
    mount.unmount();
    let error: ClientError = DispatchError::Withdrawn {
        endpoint: selected.endpoint(),
    }
    .into();
    assert_eq!(
        error,
        ClientError::Withdrawn {
            endpoint: "session/list".to_owned(),
        }
    );
    assert!(error.to_string().contains("no longer mounted"));
}
