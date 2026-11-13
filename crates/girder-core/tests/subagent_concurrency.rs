//! Tests for the concurrency bound on delegation.
//!
//! Before this existed, a registry answered "how many are running" truthfully while letting a caller start a hundred:
//! a hundred provider streams, each getting a hundredth of the process-wide rate limit. The bound is the difference
//! between reporting a fan-out and permitting one.

use girder_core::{DEFAULT_CONCURRENCY, SubagentRegistry};

fn registry(limit: usize) -> SubagentRegistry {
    SubagentRegistry::with_concurrency(limit)
}

#[test]
fn an_empty_registry_has_room() {
    let registry = registry(4);

    assert_eq!(registry.active(), 0);
    assert!(registry.has_room());
    assert_eq!(registry.concurrency(), 4);
}

#[test]
fn the_default_is_upstreams_eight() {
    // Matching upstream rather than inventing a number: an operator who has tuned one has tuned the other.
    assert_eq!(SubagentRegistry::new().concurrency(), DEFAULT_CONCURRENCY);
    assert_eq!(DEFAULT_CONCURRENCY, 8);
}

#[test]
fn starting_one_makes_it_active() {
    let registry = registry(4);

    let id = registry.record_start("look at the tests", girder_core::Depth::root());

    assert!(!id.is_empty());
    assert_eq!(registry.active(), 1);
}

#[test]
fn the_bound_is_reached_after_that_many_starts() {
    let registry = registry(3);

    for index in 0..3 {
        registry.record_start(&format!("task {index}"), girder_core::Depth::root());
    }

    assert_eq!(registry.active(), 3);
    assert!(!registry.has_room(), "three of three is full");
}

#[test]
fn finishing_one_makes_room_again() {
    // The bound has to be released by completion, or a long session would stop delegating after eight ever.
    let registry = registry(2);
    let first = registry.record_start("first", girder_core::Depth::root());
    registry.record_start("second", girder_core::Depth::root());
    assert!(!registry.has_room());

    let _ = registry.record_finished(
        &first,
        &girder_core::DelegationResult {
            text: "done".to_owned(),
            turns: 1,
            child_id: girder_core::AgentId::new("child").expect("valid"),
        },
    );

    assert!(registry.has_room());
    assert_eq!(registry.active(), 1);
}

#[test]
fn a_zero_limit_is_raised_to_one() {
    // A registry that can never admit anything is a configuration mistake whose only symptom is every delegation
    // failing several layers from the cause.
    let registry = registry(0);

    assert_eq!(registry.concurrency(), 1);
    assert!(registry.has_room());
}

#[test]
fn two_registries_do_not_share_a_bound() {
    // Each session has its own; a shared limit would make one conversation's fan-out throttle another's.
    let first = registry(1);
    let second = registry(1);
    first.record_start("busy", girder_core::Depth::root());

    assert!(!first.has_room());
    assert!(second.has_room());
}

#[test]
fn an_admitted_start_is_refused_at_the_bound() {
    // `try_record_start` is the entry point new code should use: it checks and records under one lock, so two callers
    // racing cannot both see room and both take it.
    let registry = registry(2);
    registry
        .try_record_start("first", girder_core::Depth::root())
        .expect("room for one");
    registry
        .try_record_start("second", girder_core::Depth::root())
        .expect("room for two");

    let refusal = registry
        .try_record_start("third", girder_core::Depth::root())
        .expect_err("full");

    let girder_core::StartRefusal::AtCapacity { limit, active } = refusal;
    assert_eq!(limit, 2);
    assert_eq!(active, 2);
}

#[test]
fn a_refusal_explains_what_to_do() {
    // "at capacity" alone leaves the caller to work out whether to retry, wait, or give up.
    let registry = registry(1);
    registry
        .try_record_start("only", girder_core::Depth::root())
        .expect("room");

    let refusal = registry
        .try_record_start("next", girder_core::Depth::root())
        .expect_err("full");

    let message = refusal.to_string();
    assert!(message.contains("wait"), "{message}");
    assert!(message.contains('1'), "it names the limit: {message}");
}

#[test]
fn a_refused_start_records_nothing() {
    // A refusal that left an entry behind would occupy a slot for a subagent that never ran.
    let registry = registry(1);
    registry
        .try_record_start("only", girder_core::Depth::root())
        .expect("room");
    let before = registry.entries().len();

    let _ = registry.try_record_start("next", girder_core::Depth::root());

    assert_eq!(registry.entries().len(), before);
}

#[test]
fn the_accessors_agree_with_each_other() {
    // `has_room` is derived from `active` and `concurrency`; a caller may use any of the three, so they have to agree
    // or two screens would show different answers about the same registry.
    let registry = registry(2);
    assert_eq!(registry.active(), 0);
    assert!(registry.has_room());

    registry
        .try_record_start("one", girder_core::Depth::root())
        .expect("room");
    assert_eq!(registry.active(), 1);
    assert!(registry.has_room());

    let second = registry
        .try_record_start("two", girder_core::Depth::root())
        .expect("room");
    assert_eq!(registry.active(), 2);
    assert!(!registry.has_room());
    assert_eq!(registry.active(), registry.concurrency());

    registry.record_failed(&second, "boom");
    assert_eq!(registry.active(), 1);
    assert!(registry.has_room());
}
