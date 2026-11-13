//! Tests for delegation-depth accounting.
//!
//! The property that matters is monotonicity. Every other rule here is arithmetic that a reviewer can
//! check by eye; monotonicity is the one that keeps a *resumed* child from reappearing at the top of
//! the tree with a fresh budget, so it gets the most cases.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the source as written even before the crate root wires it in.

#[path = "../src/subagent_depth.rs"]
mod subagent_depth;

use subagent_depth::{Depth, DepthError, Limit};

// ---- the top of the tree ----------------------------------------------------------------------

#[test]
fn top_is_depth_zero() {
    assert_eq!(Depth::TOP.get(), 0);
}

#[test]
fn decoding_a_raw_value_round_trips() {
    for value in [0, 1, 7, u32::MAX] {
        assert_eq!(Depth::from_raw(value).get(), value);
    }
}

#[test]
fn top_delegates_to_a_child_at_depth_one() {
    let child = Depth::TOP.child().expect("the top may delegate");
    assert_eq!(child.get(), 1);
}

// ---- the counter only goes up -----------------------------------------------------------------

#[test]
fn child_increments_by_one() {
    let mut depth = Depth::TOP;
    for expected in 1..=5 {
        depth = depth.child().expect("still under the overflow bound");
        assert_eq!(depth.get(), expected);
    }
}

#[test]
fn child_wraps_order_too() {
    // `Ord` is derived from the counter, so a deeper agent is strictly greater; the depth limit
    // comparisons would be confusing if a "larger" depth were not actually deeper.
    let top = Depth::TOP;
    let one = top.child().expect("top may delegate");
    let two = one.child().expect("depth one may delegate");
    assert!(top < one && one < two, "{top:?} < {one:?} < {two:?}");
}

#[test]
fn child_refuses_to_overflow() {
    let deepest = Depth::from_raw(u32::MAX);
    assert_eq!(
        deepest.child(),
        Err(DepthError::Overflow { depth: u32::MAX })
    );
}

// ---- depth can never shrink (the property that matters) ---------------------------------------

#[test]
fn deepest_keeps_the_larger_observation() {
    let shallow = Depth::from_raw(2);
    let deep = Depth::from_raw(5);
    assert_eq!(shallow.deepest(deep), deep);
    assert_eq!(deep.deepest(shallow), deep, "order must not matter");
}

#[test]
fn deepest_is_idempotent() {
    let depth = Depth::from_raw(3);
    assert_eq!(depth.deepest(depth), depth);
}

#[test]
fn resolve_prefers_the_persisted_header_over_fresh_runtime_options() {
    // The resume case: a child at depth 4 comes back with runtime options that were built fresh and
    // therefore say zero. Counting from the runtime alone would let it delegate four more levels.
    let resolved = Depth::resolve(Some(Depth::from_raw(4)), Some(Depth::TOP));
    assert_eq!(resolved.get(), 4, "a resume must not lower the depth");
}

#[test]
fn resolve_takes_the_runtime_value_when_it_is_deeper() {
    // The persisted header can lag a runtime that was set by hand or by a newer parent; either way
    // the deeper observation wins, because depth may only be confirmed upward.
    let resolved = Depth::resolve(Some(Depth::from_raw(1)), Some(Depth::from_raw(3)));
    assert_eq!(resolved.get(), 3);
}

#[test]
fn resolve_defaults_a_missing_source_to_top() {
    assert_eq!(Depth::resolve(None, None).get(), 0);
    assert_eq!(Depth::resolve(Some(Depth::from_raw(2)), None).get(), 2);
    assert_eq!(Depth::resolve(None, Some(Depth::from_raw(2))).get(), 2);
    // Both missing is the legacy session that predates the header field: top level, depth zero.
    assert_eq!(Depth::resolve(None, Some(Depth::TOP)).get(), 0);
}

#[test]
fn resolving_repeatedly_never_lowers_the_depth() {
    // A child that is resumed several times keeps the deepest value it has ever been seen at, which
    // is what makes the count a budget rather than a hint.
    let mut depth = Depth::from_raw(3);
    for runtime in [0, 2, 0, 1, 0] {
        let next = Depth::resolve(Some(depth), Some(Depth::from_raw(runtime)));
        assert!(
            next >= depth,
            "resume with runtime {runtime} lowered {depth:?} to {next:?}"
        );
        depth = next;
    }
    assert_eq!(depth.get(), 3);
}

// ---- the ceiling -------------------------------------------------------------------------------

#[test]
fn allows_is_false_at_the_limit() {
    let limit = Limit::new(3);
    assert!(
        !limit.allows(Depth::from_raw(3)),
        "the limit itself is terminal"
    );
}

#[test]
fn allows_is_true_below_the_limit() {
    let limit = Limit::new(3);
    for depth in 0..3 {
        assert!(
            limit.allows(Depth::from_raw(depth)),
            "depth {depth} is under 3"
        );
    }
}

#[test]
fn a_limit_of_three_admits_three_nested_levels() {
    // The worked example the documentation promises: children at 1, 2 and 3, and no fourth level.
    let limit = Limit::new(3);
    let one = limit.child(Depth::TOP).expect("depth 0 may delegate");
    let two = limit.child(one).expect("depth 1 may delegate");
    let three = limit.child(two).expect("depth 2 may delegate");
    assert_eq!((one.get(), two.get(), three.get()), (1, 2, 3));
    assert_eq!(
        limit.child(three),
        Err(DepthError::Exceeded { depth: 3, max: 3 })
    );
}

#[test]
fn a_limit_of_zero_forbids_all_delegation() {
    let limit = Limit::new(0);
    assert!(!limit.allows(Depth::TOP));
    assert_eq!(
        limit.child(Depth::TOP),
        Err(DepthError::Exceeded { depth: 0, max: 0 })
    );
}

#[test]
fn the_limit_child_reports_the_parent_depth_in_the_error() {
    let limit = Limit::new(2);
    let parent = Depth::from_raw(2);
    assert_eq!(
        limit.child(parent),
        Err(DepthError::Exceeded { depth: 2, max: 2 })
    );
}

#[test]
fn the_error_message_names_both_numbers() {
    let error = Limit::new(2)
        .child(Depth::from_raw(2))
        .expect_err("depth 2 cannot delegate under a limit of 2");
    let text = error.to_string();
    assert!(
        text.contains('2'),
        "message should carry depth and max: {text}"
    );
}

#[test]
fn a_limit_above_the_counter_bound_still_refuses_by_ceiling_first() {
    // `Limit::child` checks the ceiling before incrementing, so a limit of `u32::MAX` refuses a
    // parent at `u32::MAX` with `Exceeded` rather than `Overflow`.
    let limit = Limit::new(u32::MAX);
    assert_eq!(
        limit.child(Depth::from_raw(u32::MAX)),
        Err(DepthError::Exceeded {
            depth: u32::MAX,
            max: u32::MAX,
        })
    );
}
