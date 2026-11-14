//! Tests for the round output budget.
//!
//! The allocation is a pure function over sizes, so the tests pin the invariants a caller relies
//! on rather than a particular arrangement of bytes: the ceiling, the one-byte floor, and the
//! promise that a result which already fits is not touched.

use girder_core::{OutputAllocation as Allocation, OutputBudget, allocate_output};

/// Builds a budget with the three fields spelled out, so each test reads as a scenario.
fn budget(total_bytes: usize, per_result_bytes: usize, reserve_recent: usize) -> OutputBudget {
    OutputBudget {
        total_bytes,
        per_result_bytes,
        reserve_recent,
    }
}

/// The sum of every share.
fn spent(allocations: &[Allocation]) -> usize {
    allocations.iter().map(|allocation| allocation.bytes).sum()
}

/// The shares, in order, for a compact assertion.
fn shares(allocations: &[Allocation]) -> Vec<usize> {
    allocations
        .iter()
        .map(|allocation| allocation.bytes)
        .collect()
}

#[test]
fn a_round_that_fits_is_left_alone() {
    // Nothing is scarce, so every result keeps its full size and the reserve is not spent.
    let sizes = [100, 200, 300];
    let allocations = allocate_output(&budget(10_000, 4_096, 1_000), &sizes);

    assert_eq!(shares(&allocations), sizes.to_vec());
    assert!(spent(&allocations) <= 10_000);
}

#[test]
fn allocations_are_indexed_in_input_order() {
    // A caller pairs each share with the result it came from, so the order has to be stable even
    // when the sizes are not sorted.
    let allocations = allocate_output(&budget(1_000, 10_000, 0), &[50, 900, 20]);

    let indices: Vec<usize> = allocations
        .iter()
        .map(|allocation| allocation.index)
        .collect();
    assert_eq!(indices, vec![0, 1, 2]);
}

#[test]
fn an_over_budget_round_is_reduced_to_the_ceiling() {
    let sizes = [100_000; 5];
    let allocations = allocate_output(&budget(10_000, 1_000_000, 0), &sizes);

    assert!(
        spent(&allocations) <= 10_000,
        "spent {} of 10000",
        spent(&allocations)
    );
    // Evenly split, because the five results are the same size: cutting by arrival order would
    // leave the last one starved for no reason.
    for share in shares(&allocations) {
        assert!((1_900..=2_100).contains(&share), "unexpected share {share}");
    }
}

#[test]
fn every_result_keeps_at_least_one_byte() {
    // The core promise: a result cut to zero is indistinguishable from a tool that returned
    // nothing, and the model reasons about it as a failed call.
    let sizes = [900_000, 700_000, 500_000, 300_000, 100_000];
    let allocations = allocate_output(&budget(1_000, 1_000_000, 0), &sizes);

    for allocation in &allocations {
        assert!(
            allocation.bytes >= 1,
            "result {} was cut to zero bytes",
            allocation.index
        );
    }
    // The ceiling still holds, so the floor is met inside the budget rather than by exceeding it.
    assert!(spent(&allocations) <= 1_000);
}

#[test]
fn the_total_never_exceeds_the_budget() {
    // A spread of shapes: one dominating result, one tiny one, equal ones, and a zero-length one.
    for sizes in [
        vec![1_000_000, 1],
        vec![5, 5, 5, 5],
        vec![999_999, 999_999, 1],
        vec![0, 100_000, 0],
        vec![100_000; 17],
    ] {
        let allocations = allocate_output(&budget(4_096, 100_000, 512), &sizes);
        assert!(
            spent(&allocations) <= 4_096,
            "sizes {sizes:?} spent {}",
            spent(&allocations)
        );
    }
}

#[test]
fn the_largest_result_is_cut_hardest() {
    // Proportional reduction, not arrival order: the result that is causing the overflow gives up
    // the most bytes.
    let sizes = [1_000, 100_000];
    let allocations = allocate_output(&budget(5_000, 1_000_000, 0), &sizes);

    let small_loss = sizes[0].saturating_sub(allocations[0].bytes);
    let large_loss = sizes[1].saturating_sub(allocations[1].bytes);
    assert!(
        large_loss > small_loss,
        "small lost {small_loss}, large lost {large_loss}"
    );
    // The result that fits is untouched, because cutting it would cost information for no gain.
    assert_eq!(allocations[0].bytes, 1_000);
}

#[test]
fn a_single_oversized_result_is_capped_by_the_ceiling() {
    let allocations = allocate_output(&budget(8_000, 1_000_000, 0), &[10_000_000]);

    assert_eq!(allocations.len(), 1);
    assert_eq!(allocations[0].bytes, 8_000);
}

#[test]
fn the_per_result_limit_caps_a_share_below_the_round_budget() {
    // A round with room to spare still will not let one result claim more than the per-result
    // limit: anything beyond it belongs to pruning, which can name what it removed.
    let allocations = allocate_output(&budget(1_000_000, 4_096, 0), &[1_000_000]);

    assert_eq!(allocations[0].bytes, 4_096);
    assert!(spent(&allocations) <= 1_000_000);
}

#[test]
fn an_empty_round_allocates_nothing() {
    assert!(allocate_output(&budget(10_000, 1_000, 100), &[]).is_empty());
}

#[test]
fn empty_results_stay_empty() {
    // A tool that returns nothing is not a result that was cut; it has no bytes to keep.
    let allocations = allocate_output(&budget(10_000, 1_000, 100), &[0, 0, 0]);

    assert_eq!(shares(&allocations), vec![0, 0, 0]);
}

#[test]
fn the_recent_reserve_protects_the_last_result() {
    // The shape the reserve exists for: a huge earlier result and the answer that arrives last.
    // Without the reserve the answer is scaled to its floor; with it, the answer survives whole.
    let sizes = [1_000_000, 800];
    let unprotected = allocate_output(&budget(1_000, 1_000_000, 0), &sizes);
    let protected = allocate_output(&budget(1_000, 1_000_000, 900), &sizes);

    assert_eq!(
        unprotected[1].bytes, 1,
        "without a reserve the answer should be scaled to its floor"
    );
    assert_eq!(
        protected[1].bytes, 800,
        "the reserve should have kept the answer whole, got {}",
        protected[1].bytes
    );
    assert!(protected[1].bytes > unprotected[1].bytes);

    // The reserve is a floor for the newest results, not a slice taken out of the round: the
    // earlier result still receives what is left rather than being erased.
    assert!(protected[0].bytes >= 1);
    for allocations in [&unprotected, &protected] {
        assert!(spent(allocations) <= 1_000);
    }
}

#[test]
fn a_reserve_larger_than_the_round_still_respects_the_ceiling() {
    // A misconfiguration, but representable: the ceiling wins over the reservation, and every
    // result still receives at least its floor where the budget allows.
    let sizes = [50_000, 40_000, 30_000];
    let allocations = allocate_output(&budget(1_000, 1_000_000, 500_000), &sizes);

    assert!(spent(&allocations) <= 1_000);
    assert_eq!(allocations.len(), 3);
}

#[test]
fn a_round_too_small_for_its_floors_serves_the_newest_first() {
    // Three results cannot share one byte. The ceiling is the harder constraint, so it wins and
    // the newest result keeps the byte the budget can afford.
    let allocations = allocate_output(&budget(1, 1_000, 0), &[1_000, 1_000, 1_000]);

    assert!(spent(&allocations) <= 1);
    assert_eq!(allocations[2].bytes, 1);
}

#[test]
fn a_zero_budget_allocates_zero_bytes() {
    let allocations = allocate_output(&budget(0, 1_000, 0), &[100, 200]);

    assert_eq!(shares(&allocations), vec![0, 0]);
}

#[test]
fn a_huge_size_does_not_overflow_the_arithmetic() {
    // Sizes near `usize::MAX` would wrap a naive `size * pool` product and hand the result a tiny
    // share; the proportional step computes in a wider type so the share stays sound.
    let allocations = allocate_output(
        &budget(10_000, usize::MAX, 0),
        &[usize::MAX, usize::MAX / 2, 1],
    );

    assert!(spent(&allocations) <= 10_000);
    for allocation in &allocations {
        assert!(allocation.bytes >= 1);
    }
}
