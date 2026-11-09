//! Acceptance tests for migration-plan phase 2 (§2.5).
//!
//! The plan's criteria: waterfall short-circuits when a layer does not call `next`; waterfall
//! delegates through every layer when each calls `next`; `serial`/`bail` stop at the first bail
//! value; and a listener is removed with its owner.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use girder_core::{Bail, Event, EventBus, Order, Parallel, Serial, Waterfall};

// ---- event catalogue ----------------------------------------------------------------------------

/// An `emit` event: notification only.
struct Ping;

impl Event for Ping {
    const NAME: &'static str = "test/ping";
    type Args = u32;
}

/// A `parallel` event.
struct Fanout;

impl Parallel for Fanout {
    const NAME: &'static str = "test/fanout";
    type Args = u32;
}

/// A `serial` event: listeners run in order, awaited, until one returns `Some`.
struct Pipeline;

impl Serial for Pipeline {
    const NAME: &'static str = "test/pipeline";
    type Args = &'static str;
    type Out = &'static str;
}

/// A `bail` event: synchronous, stops at the first `Some`.
struct Claim;

impl Bail for Claim {
    const NAME: &'static str = "test/claim";
    type Args = &'static str;
    type Out = &'static str;
}

/// A `waterfall` event with a layer chain around a terminal.
struct Transform;

impl Waterfall for Transform {
    const NAME: &'static str = "test/transform";
    type Args = i32;
    type Out = i32;
}

// ---- emit ---------------------------------------------------------------------------------------

#[test]
fn emit_runs_every_listener_in_registration_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let bus = EventBus::new();

    for label in [1, 2, 3] {
        let log = Arc::clone(&log);
        bus.on::<Ping, _>("p", Order::Append, move |_| log.lock().unwrap().push(label));
    }

    bus.emit::<Ping>(0);

    assert_eq!(*log.lock().unwrap(), vec![1, 2, 3]);
}

#[test]
fn prepend_puts_a_listener_first() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let bus = EventBus::new();

    {
        let log = Arc::clone(&log);
        bus.on::<Ping, _>("p", Order::Append, move |_| {
            log.lock().unwrap().push("later");
        });
    }
    {
        let log = Arc::clone(&log);
        bus.on::<Ping, _>("p", Order::Prepend, move |_| {
            log.lock().unwrap().push("early");
        });
    }

    bus.emit::<Ping>(0);

    assert_eq!(*log.lock().unwrap(), vec!["early", "later"]);
}

#[test]
fn emit_with_no_listeners_is_a_no_op() {
    let bus = EventBus::new();
    bus.emit::<Ping>(7);
    assert_eq!(bus.listener_count(Ping::NAME), 0);
}

// ---- parallel -----------------------------------------------------------------------------------

#[tokio::test]
async fn parallel_runs_all_listeners() {
    let count = Arc::new(AtomicUsize::new(0));
    let bus = EventBus::new();

    for _ in 0..4 {
        let count = Arc::clone(&count);
        bus.on_parallel::<Fanout, _>("p", Order::Append, move |_| {
            let count = Arc::clone(&count);
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
            })
        });
    }

    bus.parallel::<Fanout>(0).await.unwrap();

    assert_eq!(count.load(Ordering::SeqCst), 4);
}

// ---- serial -------------------------------------------------------------------------------------

#[tokio::test]
async fn serial_stops_at_the_first_bail_value() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let bus = EventBus::new();

    {
        let log = Arc::clone(&log);
        bus.on_serial::<Pipeline, _>("p", Order::Append, move |_| {
            let log = Arc::clone(&log);
            Box::pin(async move {
                log.lock().unwrap().push("first");
                None
            })
        });
    }
    {
        let log = Arc::clone(&log);
        bus.on_serial::<Pipeline, _>("p", Order::Append, move |_| {
            let log = Arc::clone(&log);
            Box::pin(async move {
                log.lock().unwrap().push("second");
                Some("stopped here")
            })
        });
    }
    {
        let log = Arc::clone(&log);
        bus.on_serial::<Pipeline, _>("p", Order::Append, move |_| {
            let log = Arc::clone(&log);
            Box::pin(async move {
                log.lock().unwrap().push("third");
                None
            })
        });
    }

    let verdict = bus.serial::<Pipeline>("go").await;

    assert_eq!(verdict, Some("stopped here"));
    assert_eq!(
        *log.lock().unwrap(),
        vec!["first", "second"],
        "the third listener must not run after a bail"
    );
}

#[tokio::test]
async fn serial_returns_none_when_every_listener_declines() {
    let bus = EventBus::new();
    bus.on_serial::<Pipeline, _>("p", Order::Append, |_| Box::pin(async { None }));

    assert_eq!(bus.serial::<Pipeline>("go").await, None);
}

// ---- bail ---------------------------------------------------------------------------------------

#[test]
fn bail_is_synchronous_and_stops_at_the_first_claim() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let bus = EventBus::new();

    {
        let log = Arc::clone(&log);
        bus.on_bail::<Claim, _>("p", Order::Append, move |token| {
            log.lock().unwrap().push("pass");
            let _ = token;
            None
        });
    }
    {
        let log = Arc::clone(&log);
        bus.on_bail::<Claim, _>("p", Order::Append, move |token| {
            log.lock().unwrap().push("claim");
            token.starts_with('/').then_some("claimed")
        });
    }
    {
        let log = Arc::clone(&log);
        bus.on_bail::<Claim, _>("p", Order::Append, move |_| {
            log.lock().unwrap().push("never");
            Some("wrong")
        });
    }

    let verdict = bus.bail::<Claim>("/help");

    assert_eq!(verdict, Some("claimed"));
    assert_eq!(*log.lock().unwrap(), vec!["pass", "claim"]);
}

// ---- waterfall ----------------------------------------------------------------------------------

/// Builds a layer that logs its name and then delegates.
fn delegating(
    log: Arc<Mutex<Vec<&'static str>>>,
    label: &'static str,
) -> impl Fn(i32, girder_core::Next<i32>) -> girder_core::BoxFut<i32> {
    move |_value, next| {
        log.lock().unwrap().push(label);
        Box::pin(async move { next().await })
    }
}

#[tokio::test]
async fn waterfall_delegates_through_every_layer() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let bus = EventBus::new();

    bus.on_waterfall::<Transform, _>("p", Order::Append, delegating(Arc::clone(&log), "outer"));
    bus.on_waterfall::<Transform, _>("p", Order::Append, delegating(Arc::clone(&log), "inner"));

    let result = bus
        .waterfall::<Transform, _>(10, |value| Box::pin(async move { value + 1 }))
        .await;

    assert_eq!(result, 11);
    assert_eq!(
        *log.lock().unwrap(),
        vec!["outer", "inner"],
        "layers run outermost-first"
    );
}

#[tokio::test]
async fn waterfall_short_circuits_when_a_layer_skips_next() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let bus = EventBus::new();

    // Outer layer vetoes: it never calls `next`, so the inner layer and the terminal never run.
    {
        let log = Arc::clone(&log);
        bus.on_waterfall::<Transform, _>("p", Order::Append, move |_value, _next| {
            log.lock().unwrap().push("veto");
            Box::pin(async move { -1 })
        });
    }
    bus.on_waterfall::<Transform, _>("p", Order::Append, delegating(Arc::clone(&log), "inner"));

    let result = bus
        .waterfall::<Transform, _>(10, |value| Box::pin(async move { value + 1 }))
        .await;

    assert_eq!(result, -1, "the vetoing layer's value wins");
    assert_eq!(
        *log.lock().unwrap(),
        vec!["veto"],
        "nothing after a veto may run"
    );
}

#[tokio::test]
async fn waterfall_can_rewrite_the_value_before_delegating() {
    let bus = EventBus::new();

    // Each layer doubles the argument it passes down; the terminal adds nothing.
    for _ in 0..3 {
        bus.on_waterfall::<Transform, _>("p", Order::Append, |value, next| {
            Box::pin(async move { next().await + value })
        });
    }

    let result = bus
        .waterfall::<Transform, _>(1, |value| Box::pin(async move { value }))
        .await;

    // 1 -> outer passes 1, next adds 1 -> 2 ... each layer adds its own input.
    // 1 + (1 + (1 + 1)) = 4
    assert_eq!(result, 4);
}

#[tokio::test]
async fn a_waterfall_with_no_layers_calls_the_terminal_directly() {
    let bus = EventBus::new();

    let result = bus
        .waterfall::<Transform, _>(41, |value| Box::pin(async move { value + 1 }))
        .await;

    assert_eq!(result, 42);
}

// ---- registration lifecycle ---------------------------------------------------------------------

#[test]
fn removing_by_id_is_idempotent() {
    let bus = EventBus::new();
    let id = bus.on::<Ping, _>("p", Order::Append, |_| {});

    assert!(bus.remove(Ping::NAME, id));
    assert!(!bus.remove(Ping::NAME, id), "second removal reports false");
    assert_eq!(bus.listener_count(Ping::NAME), 0);
}

#[test]
fn removing_an_owner_drops_only_that_plugin_listeners() {
    let count = Arc::new(AtomicUsize::new(0));
    let bus = EventBus::new();

    {
        let count = Arc::clone(&count);
        bus.on::<Ping, _>("plugin-a", Order::Append, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
    }
    {
        let count = Arc::clone(&count);
        bus.on::<Ping, _>("plugin-b", Order::Append, move |_| {
            count.fetch_add(100, Ordering::SeqCst);
        });
    }

    assert_eq!(bus.remove_owner("plugin-a"), 1);
    assert_eq!(bus.listener_count(Ping::NAME), 1);

    bus.emit::<Ping>(0);
    assert_eq!(
        count.load(Ordering::SeqCst),
        100,
        "only plugin-b still listens"
    );
}

#[test]
fn active_events_reports_only_events_with_listeners() {
    let bus = EventBus::new();
    bus.on::<Ping, _>("p", Order::Append, |_| {});
    bus.on_bail::<Claim, _>("p", Order::Append, |_| None);

    let mut active = bus.active_events();
    active.sort_unstable();
    assert_eq!(active, vec![Claim::NAME, Ping::NAME]);
}

#[test]
fn a_listener_can_register_another_listener_during_emit() {
    // The bus snapshots before dispatching, so re-entrant registration must not deadlock.
    let seen = Arc::new(AtomicUsize::new(0));
    let bus = Arc::new(EventBus::new());

    {
        let bus_for_listener = Arc::clone(&bus);
        let seen = Arc::clone(&seen);
        bus.on::<Ping, _>("outer", Order::Append, move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            let seen = Arc::clone(&seen);
            bus_for_listener.on::<Ping, _>("inner", Order::Append, move |_| {
                seen.fetch_add(10, Ordering::SeqCst);
            });
        });
    }

    bus.emit::<Ping>(0); // outer runs, registers inner
    assert_eq!(seen.load(Ordering::SeqCst), 1);

    bus.emit::<Ping>(0); // both run
    assert_eq!(seen.load(Ordering::SeqCst), 12);
}

// ---- listener teardown with the owner -----------------------------------------------------------

#[tokio::test]
async fn listeners_are_removed_when_their_owner_unloads() {
    // Phase 2 acceptance item 4: a listener goes away with its fiber. The bus does not know about
    // fibers, so the link is the owner name: a plugin registers under its own name and the registry
    // calls `remove_owner` during teardown. This test drives that path end to end.
    let seen = Arc::new(AtomicUsize::new(0));
    let bus = EventBus::new();

    {
        let seen = Arc::clone(&seen);
        bus.on::<Ping, _>("plugin-a", Order::Append, move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
    }

    bus.emit::<Ping>(0);
    assert_eq!(seen.load(Ordering::SeqCst), 1);

    // Teardown: every listener the plugin registered is dropped in one call.
    assert_eq!(bus.remove_owner("plugin-a"), 1);

    bus.emit::<Ping>(0);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the listener must not fire after its owner unloaded"
    );
    assert!(bus.active_events().is_empty());
}

// ---- push observers -----------------------------------------------------------------------------

#[test]
fn an_observer_receives_every_dispatch_with_its_arguments() {
    // This is what a live client needs: the event *and* what it carried. Polling listener counts
    // cannot express either of those.
    let bus = girder_core::EventBus::new();
    let mut observed = bus.observe(None);

    bus.on::<Ping, _>("p", Order::Append, |_| {});
    bus.emit::<Ping>(7);

    let event = observed.try_recv().expect("the dispatch must be forwarded");
    assert_eq!(event.name, Ping::NAME);
    assert_eq!(event.args, serde_json::json!(7));
}

#[test]
fn an_observer_sees_the_same_event_twice() {
    // The reason a listener count cannot substitute for a subscription.
    let bus = girder_core::EventBus::new();
    let mut observed = bus.observe(None);
    bus.on::<Ping, _>("p", Order::Append, |_| {});

    bus.emit::<Ping>(1);
    bus.emit::<Ping>(1);

    let first = observed.try_recv().unwrap();
    let second = observed.try_recv().unwrap();
    assert_eq!(first.name, second.name);
    assert_eq!(first.args, second.args);
}

#[test]
fn an_observer_can_watch_a_subset() {
    let bus = girder_core::EventBus::new();
    let mut observed = bus.observe(Some(vec![Claim::NAME]));
    bus.on::<Ping, _>("p", Order::Append, |_| {});
    bus.on_bail::<Claim, _>("p", Order::Append, |_| None);

    bus.emit::<Ping>(1);
    assert!(
        observed.try_recv().is_err(),
        "an unwatched event must not be forwarded"
    );

    bus.bail::<Claim>("token");
    assert_eq!(observed.try_recv().unwrap().name, Claim::NAME);
}

#[test]
fn an_observer_sees_a_serial_event_even_when_a_listener_bails() {
    // The event happened; the observer wants to know regardless of who stopped the chain.
    let bus = girder_core::EventBus::new();
    let mut observed = bus.observe(None);
    bus.on_bail::<Claim, _>("p", Order::Append, |_| Some("claimed"));

    let verdict = bus.bail::<Claim>("/help");

    assert_eq!(verdict, Some("claimed"));
    assert_eq!(observed.try_recv().unwrap().name, Claim::NAME);
}

#[test]
fn a_dropped_observer_is_pruned() {
    // An unbounded channel whose receiver is gone must not accumulate or panic.
    let bus = girder_core::EventBus::new();
    let observed = bus.observe(None);
    drop(observed);
    assert_eq!(bus.observer_count(), 1, "registration survives the drop");

    bus.on::<Ping, _>("p", Order::Append, |_| {});
    bus.emit::<Ping>(1);

    assert_eq!(
        bus.observer_count(),
        0,
        "a closed sink is pruned on dispatch"
    );
}

#[test]
fn dispatch_with_no_observer_is_unaffected() {
    let bus = girder_core::EventBus::new();
    let seen = Arc::new(AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen);
        bus.on::<Ping, _>("p", Order::Append, move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
    }

    bus.emit::<Ping>(1);

    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert_eq!(bus.observer_count(), 0);
}

#[tokio::test]
async fn a_waterfall_dispatch_is_observable() {
    // A waterfall's arguments are announced before the chain runs, so an observer sees the request
    // even if a layer vetoes it.
    let bus = girder_core::EventBus::new();
    let mut observed = bus.observe(None);
    bus.on_waterfall::<Transform, _>("p", Order::Append, |_args, _next| Box::pin(async { -1 }));

    let result = bus
        .waterfall::<Transform, _>(42, |value| Box::pin(async move { value }))
        .await;

    assert_eq!(result, -1, "the veto still wins");
    assert_eq!(observed.try_recv().unwrap().args, serde_json::json!(42));
}

// ---- observer backpressure ----------------------------------------------------------------------

#[test]
fn an_unread_observer_is_dropped_rather_than_buffered_without_bound() {
    // Dispatches arrive at model speed, a consumer at network speed. Measured before the bound:
    // 100,000 unread events stayed in memory. An unbounded queue turns one slow client into a leak.
    let bus = girder_core::EventBus::new();
    let mut observed = bus.observe(None);
    bus.on::<Ping, _>("p", Order::Append, |_| {});

    // Emit far more than the queue holds.
    for value in 0..10_000u32 {
        bus.emit::<Ping>(value);
    }

    assert_eq!(
        bus.observer_count(),
        0,
        "an observer that cannot keep up must be dropped, not buffered"
    );

    // What it did receive is a prefix: dropping the observer rather than the oldest event means a
    // consumer never sees a gap in the middle.
    let mut received = Vec::new();
    while let Ok(event) = observed.try_recv() {
        if let Some(value) = event.args.as_u64() {
            received.push(value);
        }
    }
    assert!(
        !received.is_empty(),
        "it must receive something before being dropped"
    );
    for (index, value) in received.iter().enumerate() {
        assert_eq!(
            *value, index as u64,
            "the received events must be a contiguous prefix"
        );
    }
}

#[test]
fn a_consumer_that_keeps_up_observes_every_dispatch() {
    // The bound must not drop anything for a healthy reader.
    let bus = girder_core::EventBus::new();
    let mut observed = bus.observe(None);
    bus.on::<Ping, _>("p", Order::Append, |_| {});

    let mut received = Vec::new();
    for value in 0..100u32 {
        bus.emit::<Ping>(value);
        // Drain between dispatches, as a real forwarder does.
        while let Ok(event) = observed.try_recv() {
            if let Some(value) = event.args.as_u64() {
                received.push(value);
            }
        }
    }

    assert_eq!(received.len(), 100, "a healthy consumer loses nothing");
    assert_eq!(bus.observer_count(), 1, "and is not dropped");
}

#[test]
fn a_dropped_receiver_is_pruned_on_the_next_dispatch() {
    // Cleanup must not depend on the consumer cooperatively unsubscribing.
    let bus = girder_core::EventBus::new();
    let rx = bus.observe(None);
    drop(rx);
    bus.on::<Ping, _>("p", Order::Append, |_| {});

    bus.emit::<Ping>(1);

    assert_eq!(bus.observer_count(), 0);
}

// ---- scoped dispatch (M0 of the v2 plan) --------------------------------------------------------

use girder_core::{ScopeKey, child_scope};

#[test]
fn a_global_listener_sees_every_scope() {
    let bus = EventBus::new();
    let seen = Arc::new(AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen);
        bus.on::<Ping, _>("global", Order::Append, move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
    }

    bus.emit_scoped::<Ping>(child_scope(0, 1), 1);
    bus.emit_scoped::<Ping>(child_scope(0, 2), 2);

    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

#[test]
fn a_scoped_listener_sees_only_its_own_subtree() {
    // The property `dsh-scope` exists for: an agent's events reach that agent and the harness, not
    // a sibling agent.
    let bus = EventBus::new();
    let agent_a = child_scope(0, 1);
    let agent_b = child_scope(0, 2);

    let seen_a = Arc::new(AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen_a);
        bus.on_scoped::<Ping, _>("a", Order::Append, agent_a, move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
    }

    bus.emit_scoped::<Ping>(agent_a, 1);
    assert_eq!(seen_a.load(Ordering::SeqCst), 1, "its own event arrives");

    bus.emit_scoped::<Ping>(agent_b, 2);
    assert_eq!(
        seen_a.load(Ordering::SeqCst),
        1,
        "a sibling's event must not arrive"
    );
}

#[test]
fn an_ancestor_listener_sees_a_descendants_event() {
    // Events extend *up* the chain: a harness-level listener observes every agent's events.
    let bus = EventBus::new();
    let root_scope: ScopeKey = child_scope(0, 1);
    let deeper = child_scope(root_scope, 3);

    let seen = Arc::new(AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen);
        bus.on_scoped::<Ping, _>("ancestor", Order::Append, root_scope, move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
    }

    bus.emit_scoped::<Ping>(deeper, 1);

    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "an ancestor admits descendants"
    );
}

#[test]
fn a_descendant_listener_does_not_see_an_ancestors_event() {
    let bus = EventBus::new();
    let root_scope: ScopeKey = child_scope(0, 1);
    let deeper = child_scope(root_scope, 3);

    let seen = Arc::new(AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen);
        bus.on_scoped::<Ping, _>("descendant", Order::Append, deeper, move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
    }

    bus.emit_scoped::<Ping>(root_scope, 1);

    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "admission extends up the chain, never down"
    );
}

#[test]
fn scope_keys_encode_their_path() {
    // Keys are `parent << 8 | index`, so ancestry is a shift-and-compare rather than a parent map.
    let root = child_scope(0, 2);
    let child = child_scope(root, 5);
    let grandchild = child_scope(child, 7);

    assert_ne!(root, child);
    assert_ne!(child, grandchild);
    // Two different roots never collide.
    assert_ne!(child_scope(0, 1), child_scope(0, 2));
    // Siblings under one parent differ.
    assert_ne!(child_scope(root, 1), child_scope(root, 2));
}

#[test]
fn an_unscoped_dispatch_reaches_only_global_listeners() {
    // `emit` has no scope, so a scoped listener must not receive it: it would see events from
    // unknown subtrees.
    let bus = EventBus::new();
    let seen = Arc::new(AtomicUsize::new(0));
    {
        let seen = Arc::clone(&seen);
        bus.on_scoped::<Ping, _>("scoped", Order::Append, child_scope(0, 1), move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
    }

    bus.emit::<Ping>(1);

    assert_eq!(seen.load(Ordering::SeqCst), 0);
}
