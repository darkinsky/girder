//! Tests for scope admission on scoped event dispatch.
//!
//! The properties here are the ones the module exists for, and each is tested from the side that
//! can fail:
//!
//! - **A sibling does not receive another sibling's dispatch.** The whole point: two agents under
//!   one root, and `agent_a`'s event must not reach `agent_b`'s listener.
//! - **Admission extends up the chain, never down.** A harness-level listener observes every
//!   agent; an agent's listener never observes the harness's own dispatch.
//! - **An untagged dispatch reaches everyone.** This is upstream's `tag === undefined` rule and it
//!   is the opposite of what `EventBus::emit` does locally, so it is asserted rather than assumed.
//! - **The declared relation beats the numeric prefix.** The one property the shift encoding cannot
//!   express: a key keeping its identity while adopting a new parent.
//! - **Order survives admission.** `serial`, `bail`, and `waterfall` are order-dependent, so a
//!   filter that sorted would silently change semantics.
//!
//! # Why the module is compiled by path
//!
//! `scoped_events_gap` is not exported from the crate root yet — the wiring in `lib.rs` is a
//! separate change. Compiling the exact source file keeps these tests runnable and independent of
//! that wiring, while still exercising the same code a consumer will reach through the crate root.
//! Nothing is duplicated: this is the real file. It names the crate through `::girder_core`, which
//! the `extern crate self as girder_core` alias lets resolve from either side.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/scoped_events_gap.rs"]
mod scoped_events_gap;

use girder_core::{Event, EventBus, ListenerScope, Order, ScopeGraph, ScopeKey, child_scope};

use scoped_events_gap::{
    Admission, Ancestry, DispatchShape, SCOPED_EVENTS, Support, admit_outside_bus_events,
    encoded_is_within, is_scoped_waterfall, mint_child, native_upstream_events, scoped_event,
    support, unsupported_upstream_events,
};

/// A notify event the tests dispatch through the real bus.
struct Ping;

impl Event for Ping {
    const NAME: &'static str = "test/ping";
    type Args = u32;
}

/// A root plus two sibling agents, the tree a multi-agent harness actually has.
fn tree() -> (ScopeKey, ScopeKey, ScopeKey) {
    let root = child_scope(0, 1);
    (root, child_scope(root, 1), child_scope(root, 2))
}

// ---- the admission rule on its own -------------------------------------------------------------

#[test]
fn a_sibling_is_not_admitted() {
    let (_root, agent_a, agent_b) = tree();

    for ancestry in [Ancestry::Encoded, Ancestry::Declared(&ScopeGraph::new())] {
        let dispatch = Admission::scoped(agent_a, ancestry);
        assert!(
            dispatch.admits(ListenerScope::Scoped(agent_a)),
            "{ancestry:?}"
        );
        assert!(
            !dispatch.admits(ListenerScope::Scoped(agent_b)),
            "a sibling must never receive it ({ancestry:?})"
        );
    }
}

#[test]
fn a_parent_listener_sees_a_descendants_dispatch() {
    let (root, agent_a, _agent_b) = tree();
    let dispatch = Admission::scoped(agent_a, Ancestry::Encoded);
    assert!(dispatch.admits(ListenerScope::Scoped(root)));
}

#[test]
fn a_child_listener_does_not_see_a_parents_dispatch() {
    let (root, agent_a, _agent_b) = tree();
    let dispatch = Admission::scoped(root, Ancestry::Encoded);
    assert!(!dispatch.admits(ListenerScope::Scoped(agent_a)));
}

#[test]
fn a_global_listener_always_receives_it() {
    let (root, agent_a, _agent_b) = tree();
    for tag in [root, agent_a] {
        assert!(Admission::scoped(tag, Ancestry::Encoded).admits(ListenerScope::Global));
    }
    assert!(Admission::unscoped(Ancestry::Encoded).admits(ListenerScope::Global));
}

#[test]
fn an_untagged_dispatch_reaches_scoped_listeners() {
    // Upstream: `if (tag === undefined) return true`. The local `EventBus::emit` admits only
    // global listeners instead, which is a different rule; this type expresses upstream's.
    let (_root, agent_a, agent_b) = tree();
    let dispatch = Admission::unscoped(Ancestry::Encoded);

    assert!(dispatch.admits(ListenerScope::Scoped(agent_a)));
    assert!(dispatch.admits(ListenerScope::Scoped(agent_b)));
    assert_eq!(dispatch.tag(), None);
}

#[test]
fn the_declared_relation_wins_over_the_numeric_prefix() {
    // The gap the shift encoding cannot close: `agent_a` keeps its identity and adopts a parent
    // outside `root`. Its digits still say "below root", its declared parent says otherwise.
    let (root, agent_a, _agent_b) = tree();
    let outer = child_scope(0, 9);

    let mut graph = ScopeGraph::new();
    graph.bind(agent_a, outer).expect("first bind");

    let declared = Admission::scoped(agent_a, Ancestry::Declared(&graph));
    assert!(
        declared.admits(ListenerScope::Scoped(outer)),
        "the new parent admits it"
    );
    assert!(
        !declared.admits(ListenerScope::Scoped(root)),
        "the old parent no longer does, despite the matching digits"
    );

    // The encoded view of the same pair disagrees — that is exactly why the two exist.
    let encoded = Admission::scoped(agent_a, Ancestry::Encoded);
    assert!(encoded.admits(ListenerScope::Scoped(root)));
    assert!(!encoded.admits(ListenerScope::Scoped(outer)));
}

#[test]
fn an_unbound_key_is_not_narrowed_by_supplying_a_graph() {
    let (root, agent_a, _agent_b) = tree();
    let graph = ScopeGraph::new();
    assert!(graph.is_empty());

    let declared = Admission::scoped(agent_a, Ancestry::Declared(&graph));
    let encoded = Admission::scoped(agent_a, Ancestry::Encoded);
    assert_eq!(
        declared.admits(ListenerScope::Scoped(root)),
        encoded.admits(ListenerScope::Scoped(root))
    );
}

#[test]
fn admit_preserves_registration_order() {
    let (_root, agent_a, agent_b) = tree();
    let listeners = [
        ListenerScope::Scoped(agent_b),
        ListenerScope::Scoped(agent_a),
        ListenerScope::Global,
    ];

    let admitted = Admission::scoped(agent_a, Ancestry::Encoded).admit(&listeners);
    assert_eq!(
        admitted,
        vec![ListenerScope::Scoped(agent_a), ListenerScope::Global],
        "the admitted pair keeps the order it registered in"
    );
}

#[test]
fn an_admission_reports_its_own_tag_and_ancestry() {
    // A caller that hands an `Admission` to a dispatcher needs to read back what it built; the
    // accessors are `const` so a static default is expressible.
    let (_root, agent_a, _agent_b) = tree();
    let encoded = Admission::scoped(agent_a, Ancestry::Encoded);
    assert_eq!(encoded.tag(), Some(agent_a));
    assert_eq!(encoded.ancestry(), Ancestry::Encoded);

    let graph = ScopeGraph::new();
    let declared = Admission::unscoped(Ancestry::Declared(&graph));
    assert_eq!(declared.tag(), None);
    assert_eq!(declared.ancestry(), Ancestry::Declared(&graph));
}

#[test]
fn excludes_is_the_negation_of_admits() {
    let (root, agent_a, agent_b) = tree();
    let dispatch = Admission::scoped(agent_a, Ancestry::Encoded);
    for listener in [
        ListenerScope::Global,
        ListenerScope::Scoped(root),
        ListenerScope::Scoped(agent_a),
        ListenerScope::Scoped(agent_b),
    ] {
        assert_eq!(dispatch.excludes(listener), !dispatch.admits(listener));
    }
}

#[test]
fn a_root_tagged_dispatch_reaches_only_root_and_global_listeners() {
    // Symmetry with the ancestor case: a dispatch tagged `0` (the root) has no ancestry below it
    // that was declared, so an agent-scoped listener is not admitted. A listener *at* the root and
    // any global listener are.
    let (_root, agent_a, agent_b) = tree();
    let dispatch = Admission::scoped(0, Ancestry::Encoded);

    assert!(dispatch.admits(ListenerScope::Scoped(0)));
    assert!(dispatch.admits(ListenerScope::Global));
    assert!(
        !dispatch.admits(ListenerScope::Scoped(agent_a)),
        "the root's own dispatch does not descend into an agent"
    );
    assert!(!dispatch.admits(ListenerScope::Scoped(agent_b)));
}

// ---- agreement with the bus's own encoding -----------------------------------------------------

#[test]
fn encoded_is_within_matches_the_bus_walk() {
    // `child_scope` is the bus's own parent encoding; the module must not drift from it.
    let root = child_scope(0, 2);
    let child = child_scope(root, 5);
    let grandchild = child_scope(child, 7);

    assert!(encoded_is_within(root, 0), "the root owns everything");
    assert!(encoded_is_within(child, root));
    assert!(
        encoded_is_within(grandchild, root),
        "a grandparent admits it"
    );
    assert!(encoded_is_within(grandchild, child));
    assert!(!encoded_is_within(root, child), "never downward");
    assert!(
        encoded_is_within(grandchild, 0),
        "every key is within the root key"
    );
    assert!(!encoded_is_within(
        child_scope(root, 1),
        child_scope(root, 2)
    ));
    assert_eq!(mint_child(root, 5), child);
}

#[test]
fn mint_child_is_the_bus_encoding() {
    assert_eq!(mint_child(0, 1), child_scope(0, 1));
    let root = mint_child(0, 3);
    assert_eq!(mint_child(root, 4), child_scope(root, 4));
}

// ---- end to end through the real bus -----------------------------------------------------------

#[test]
fn through_the_bus_a_sibling_never_receives_the_dispatch() {
    // The property end to end, with the bus's own `on_scoped`/`emit_scoped` and this module's
    // admission checked against the same keys. The two must agree, or the wiring will disagree.
    let bus = EventBus::new();
    let (_root, agent_a, agent_b) = tree();

    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let seen = std::sync::Arc::clone(&seen);
        bus.on_scoped::<Ping, _>("a", Order::Append, agent_a, move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
    }

    bus.emit_scoped::<Ping>(agent_b, 2);
    assert_eq!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the bus and the admission rule agree that a sibling is excluded"
    );

    bus.emit_scoped::<Ping>(agent_a, 1);
    assert_eq!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "and agree that its own dispatch arrives"
    );

    let dispatch = Admission::scoped(agent_b, Ancestry::Encoded);
    assert!(!dispatch.admits(ListenerScope::Scoped(agent_a)));
    let dispatch = Admission::scoped(agent_a, Ancestry::Encoded);
    assert!(dispatch.admits(ListenerScope::Scoped(agent_a)));
}

// ---- the upstream catalogue --------------------------------------------------------------------

#[test]
fn the_upstream_catalogue_has_twenty_seven_entries() {
    // The migration plan says 20; the declarations in `packages/**/src` say 27. The difference is
    // not cosmetic — it is five events nobody would have wired.
    assert_eq!(SCOPED_EVENTS.len(), 27);
}

#[test]
fn the_catalogue_is_unique_and_sorted_by_name() {
    let mut names: Vec<&str> = SCOPED_EVENTS.iter().map(|event| event.name).collect();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), before, "no duplicate declarations");

    // A stable order makes the constant reviewable as a diff against upstream.
    let listed: Vec<&str> = SCOPED_EVENTS.iter().map(|event| event.name).collect();
    assert_eq!(listed, names, "the table is kept in sorted order");
}

#[test]
fn the_three_shapes_partition_the_catalogue() {
    let native = native_upstream_events();
    let awaited = admit_outside_bus_events();
    let unsupported = unsupported_upstream_events();

    assert_eq!(native.len(), 14, "notify-only");
    assert_eq!(awaited.len(), 3, "awaited");
    assert_eq!(unsupported.len(), 10, "waterfall");
    assert_eq!(
        native.len() + awaited.len() + unsupported.len(),
        SCOPED_EVENTS.len()
    );
}

#[test]
fn an_awaited_event_is_admissible_outside_the_bus() {
    // `session/flush` returns `Promise<void> | void`: every listener settles, so a caller can
    // filter with `Admission` and await the admitted ones. Upstream's own `flush` does exactly
    // that, which is why this group is not in the unsupported set.
    assert_eq!(
        admit_outside_bus_events(),
        vec!["agent/created", "agent/turn-stopping", "session/flush"]
    );
    assert_eq!(support("session/flush"), Some(Support::AdmitOutsideBus));
    assert!(!is_scoped_waterfall("session/flush"));
}

#[test]
fn a_waterfall_is_still_a_waterfall_whatever_it_returns() {
    // The awaited group is decided by the *absence* of a `next`, not by returning a promise:
    // `tools/execute` also returns `Promise<...>`, and it is a waterfall because it takes `next`.
    assert!(is_scoped_waterfall("tools/execute"));
    assert!(!is_scoped_waterfall("agent/created"));
    assert_eq!(support("tools/execute"), Some(Support::Unsupported));
}

#[test]
fn the_unwirable_set_is_exactly_the_scoped_waterfalls() {
    // These ten are the ones where a globally registered layer would run on a sibling agent's
    // event: `tools/execute` and `approval/request` in particular run inside a specific agent.
    assert_eq!(
        unsupported_upstream_events(),
        vec![
            "agent/pre-step",
            "agent/request",
            "agent/request-error",
            "approval/request",
            "system-prompt/assemble",
            "tools/execute",
            "tools/post-execute",
            "tools/pre-execute",
            "tools/ptc-dispatch-log",
            "user-questions/request",
        ]
    );

    for event in SCOPED_EVENTS {
        assert_eq!(
            event.shape == DispatchShape::Waterfall,
            unsupported_upstream_events().contains(&event.name),
            "{} must be classified by its shape and nothing else",
            event.name
        );
        assert_eq!(
            event.shape == DispatchShape::Awaited,
            admit_outside_bus_events().contains(&event.name),
            "{} must be in the awaited group iff its shape is awaited",
            event.name
        );
    }
}

#[test]
fn a_known_event_answers_its_shape() {
    assert!(is_scoped_waterfall("approval/request"));
    assert!(is_scoped_waterfall("tools/execute"));
    assert!(!is_scoped_waterfall("agent/created"));
    assert!(!is_scoped_waterfall("session/event"));
}

#[test]
fn an_event_upstream_does_not_scope_makes_no_claim() {
    // `agent-preset/selected` is forwarded with mode `emit` and carries no `Scoped<T>`, so asking
    // about it must say "unknown" rather than guess a shape.
    assert_eq!(scoped_event("agent-preset/selected"), None);
    assert_eq!(support("agent-preset/selected"), None);
    assert_eq!(scoped_event(""), None);
    assert_eq!(support("not-an-event"), None);
}

#[test]
fn the_support_verdicts_match_the_shapes() {
    assert_eq!(support("session/created"), Some(Support::Native));
    assert_eq!(support("goal/changed"), Some(Support::Native));
    assert_eq!(support("agent/created"), Some(Support::AdmitOutsideBus));
    assert_eq!(support("tools/execute"), Some(Support::Unsupported));
    assert_eq!(
        support("user-questions/request"),
        Some(Support::Unsupported)
    );
}

#[test]
fn the_catalogue_covers_every_subject_upstream_uses() {
    // Four subject types across the 27 declarations; a typo'd one would show up here.
    let mut subjects: Vec<&str> = SCOPED_EVENTS.iter().map(|event| event.subject).collect();
    subjects.sort_unstable();
    subjects.dedup();
    assert_eq!(
        subjects,
        vec![
            "Agent",
            "Session",
            "SubagentRuntime",
            "SystemPrompt",
            "ToolsService"
        ]
    );
}
