//! Tests for streaming subscriptions.
//!
//! A client that polls for a value either misses what happened between polls or spends a round trip
//! per interval. A subscription sends when the host has something, so the tests pin the properties a
//! client depends on: values arrive in order, a close is visible, and a closed stream stays closed.

use girder_rpc::{
    EndReason, SubscribeError, SubscriptionFrame, SubscriptionId, Subscriptions, is_subscribable,
};

fn id(value: &str) -> SubscriptionId {
    SubscriptionId::new(value).expect("valid")
}

// ---- identifiers ------------------------------------------------------------------------------

#[test]
fn an_empty_subscription_id_is_refused() {
    // It would name every subscription at once.
    assert!(SubscriptionId::new("").is_none());
}

#[test]
fn a_subscription_id_displays_itself() {
    assert_eq!(id("s1").to_string(), "s1");
    assert_eq!(id("s1").as_str(), "s1");
}

// ---- opening ----------------------------------------------------------------------------------

#[test]
fn opening_a_subscription_makes_it_visible() {
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");

    assert_eq!(subscriptions.ids(), vec!["s1"]);
    assert_eq!(subscriptions.len(), 1);
}

#[test]
fn a_duplicate_id_is_refused() {
    // Two subscriptions under one id would make every frame ambiguous.
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");

    let error = subscriptions
        .open(id("s1"), "job.follow")
        .expect_err("duplicate");

    let SubscribeError::Duplicate { id } = error else {
        panic!("expected Duplicate, got {error:?}");
    };
    assert_eq!(id, "s1");
}

#[test]
fn the_limit_is_enforced() {
    // An unbounded set is one client exhausting the server: each open subscription holds a slot the
    // host writes to.
    let subscriptions = Subscriptions::new(2);
    subscriptions.open(id("a"), "job.follow").expect("opened");
    subscriptions.open(id("b"), "job.follow").expect("opened");

    let error = subscriptions.open(id("c"), "job.follow").expect_err("full");

    assert!(matches!(error, SubscribeError::TooMany { limit: 2 }));
}

#[test]
fn a_zero_limit_still_allows_one() {
    // A limit of zero would make the feature unusable rather than conservative.
    let subscriptions = Subscriptions::new(0);
    assert!(subscriptions.open(id("a"), "job.follow").is_ok());
}

// ---- values -----------------------------------------------------------------------------------

#[test]
fn a_value_carries_its_subscription_and_method() {
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");

    let frame = subscriptions
        .push("s1", serde_json::json!({"text": "hello"}))
        .expect("value");

    let SubscriptionFrame::Value {
        subscription_id,
        method,
        value,
        seq,
    } = frame
    else {
        panic!("expected a value");
    };
    assert_eq!(subscription_id.as_str(), "s1");
    assert_eq!(method, "job.follow");
    assert_eq!(value["text"], "hello");
    assert_eq!(seq, 1);
}

#[test]
fn values_arrive_in_sequence() {
    // A client that could not tell frame 3 from frame 5 would render a missing update as continuity.
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");

    let mut sequences = Vec::new();
    for index in 0..3 {
        let frame = subscriptions
            .push("s1", serde_json::json!(index))
            .expect("value");
        if let SubscriptionFrame::Value { seq, .. } = frame {
            sequences.push(seq);
        }
    }

    assert_eq!(sequences, vec![1, 2, 3]);
}

#[test]
fn pushing_to_an_unknown_subscription_produces_nothing() {
    // A stray write must not create a subscription that no client asked for.
    let subscriptions = Subscriptions::new(4);
    assert!(subscriptions.push("nope", serde_json::json!(1)).is_none());
    assert!(subscriptions.is_empty());
}

#[test]
fn two_subscriptions_do_not_share_a_sequence() {
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("a"), "job.follow").expect("opened");
    subscriptions
        .open(id("b"), "session.follow")
        .expect("opened");

    for _ in 0..3 {
        subscriptions.push("a", serde_json::json!(1));
    }
    let second = subscriptions
        .push("b", serde_json::json!(1))
        .expect("value");

    let SubscriptionFrame::Value { seq, .. } = second else {
        panic!("expected a value");
    };
    assert_eq!(seq, 1, "each subscription counts its own values");
}

// ---- closing ----------------------------------------------------------------------------------

#[test]
fn closing_produces_one_end_frame() {
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");

    let frame = subscriptions
        .close("s1", EndReason::Completed)
        .expect("closed");

    let SubscriptionFrame::End {
        subscription_id,
        reason,
    } = frame
    else {
        panic!("expected an end");
    };
    assert_eq!(subscription_id.as_str(), "s1");
    assert_eq!(reason, EndReason::Completed);
}

#[test]
fn a_second_close_produces_nothing() {
    // A client that received one close and then another would not know which to believe.
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");
    subscriptions
        .close("s1", EndReason::Completed)
        .expect("closed");

    assert!(
        subscriptions
            .close(
                "s1",
                EndReason::Failed {
                    reason: "late".to_owned()
                }
            )
            .is_none()
    );
}

#[test]
fn a_closed_subscription_produces_no_more_values() {
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");
    subscriptions.close("s1", EndReason::Unsubscribed);

    assert!(subscriptions.push("s1", serde_json::json!(1)).is_none());
}

#[test]
fn a_failed_end_carries_the_reason() {
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");

    let frame = subscriptions
        .close(
            "s1",
            EndReason::Failed {
                reason: "the job was deleted".to_owned(),
            },
        )
        .expect("closed");

    let SubscriptionFrame::End { reason, .. } = frame else {
        panic!("expected an end");
    };
    let EndReason::Failed { reason } = reason else {
        panic!("expected a failure");
    };
    assert_eq!(reason, "the job was deleted");
}

// ---- the subscribable list --------------------------------------------------------------------

#[test]
fn the_streaming_methods_are_subscribable() {
    for (namespace, method) in [
        ("session", "observeEvent"),
        ("session", "observeProjections"),
        ("session", "observeState"),
        ("session", "follow"),
        ("job", "follow"),
    ] {
        assert!(
            is_subscribable(namespace, method),
            "{namespace}.{method} must be subscribable"
        );
    }
}

#[test]
fn a_unary_method_is_not_subscribable() {
    // Allowing a subscription to any method would let a client open thousands of slots that never
    // produce anything.
    assert!(!is_subscribable("session", "page"));
    assert!(!is_subscribable("job", "list"));
    assert!(!is_subscribable("workspace", "create"));
}

#[test]
fn an_unknown_method_is_not_subscribable() {
    assert!(!is_subscribable("nope", "nothing"));
}

// ---- wire format ------------------------------------------------------------------------------

#[test]
fn a_value_frame_serializes_with_its_tag() {
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");
    let frame = subscriptions
        .push("s1", serde_json::json!(1))
        .expect("value");

    let text = serde_json::to_string(&frame).expect("serializes");
    assert!(text.contains(r#""type":"value""#), "{text}");

    let restored: SubscriptionFrame = serde_json::from_str(&text).expect("parses");
    assert_eq!(restored, frame);
}

#[test]
fn an_end_frame_serializes_with_its_tag_and_reason() {
    let subscriptions = Subscriptions::new(4);
    subscriptions.open(id("s1"), "job.follow").expect("opened");
    let frame = subscriptions
        .close("s1", EndReason::Completed)
        .expect("closed");

    let text = serde_json::to_string(&frame).expect("serializes");
    assert!(text.contains(r#""type":"end""#), "{text}");
    assert!(text.contains(r#""kind":"completed""#), "{text}");
}

#[test]
fn the_debug_form_lists_ids_not_handles() {
    let subscriptions = Subscriptions::new(4);
    subscriptions
        .open(id("alpha"), "job.follow")
        .expect("opened");

    let text = format!("{subscriptions:?}");
    assert!(text.contains("alpha"));
    assert!(text.contains("Subscriptions"));
}
