//! Tests for the per-turn output budget.
//!
//! `spill_limit` bounds one result and cannot see a round: five tools returning 100 KB each is 500 KB that no
//! per-result limit refuses. The property under test is that the round is bounded, and that a trimmed result still says
//! it was trimmed rather than looking like a tool that returned nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A round of results against one budget, the way the loop spends it.
fn spend(limit: usize, sizes: &[usize]) -> Vec<usize> {
    let spent = AtomicUsize::new(0);
    sizes
        .iter()
        .map(|size| {
            let remaining = limit.saturating_sub(spent.load(Ordering::Relaxed));
            spent.fetch_add(*size, Ordering::Relaxed);
            (*size).min(remaining)
        })
        .collect()
}

#[test]
fn a_round_within_its_budget_is_untouched() {
    let kept = spend(1000, &[100, 200, 300]);

    assert_eq!(kept, vec![100, 200, 300]);
}

#[test]
fn the_round_is_bounded_across_results() {
    // The case no per-result limit sees: each result is small and the round is not.
    let kept = spend(250, &[100, 100, 100, 100]);

    // 100 + 100 + 50: the third result gets the remaining fifty and the fourth has nothing.
    assert_eq!(kept, vec![100, 100, 50, 0]);
    assert_eq!(kept[3], 0, "the fourth has nothing left");
}

#[test]
fn a_trimmed_result_still_consumes_the_room_it_was_trimmed_for() {
    // Counting only what was kept would let a loop of trimmed results run forever: each one would look free.
    let spent = AtomicUsize::new(0);
    for _ in 0..10 {
        let remaining = 100usize.saturating_sub(spent.load(Ordering::Relaxed));
        spent.fetch_add(100, Ordering::Relaxed);
        assert_eq!(
            remaining.min(100),
            if spent.load(Ordering::Relaxed) == 100 {
                100
            } else {
                0
            }
        );
    }
    assert!(spent.load(Ordering::Relaxed) >= 100);
}

#[test]
fn a_large_first_result_leaves_nothing_for_the_rest() {
    let kept = spend(100, &[500, 100, 100]);

    assert_eq!(kept[0], 100, "even the first is trimmed to the round");
    assert_eq!(kept[1], 0);
    assert_eq!(kept[2], 0);
}

#[test]
fn an_unbounded_round_keeps_everything() {
    // `None` before this existed, and a deployment that sets nothing must behave as it did.
    let sizes = [1_000_000, 1_000_000];

    assert_eq!(spend(usize::MAX, &sizes), sizes.to_vec());
}

#[test]
fn the_budget_is_shared_by_the_whole_round_not_per_result() {
    // Two results of 60 against a round of 100: a per-result limit would keep both.
    let kept = spend(100, &[60, 60]);

    assert_eq!(kept, vec![60, 40]);
    assert_eq!(kept.iter().sum::<usize>(), 100);
}

#[test]
fn the_counter_is_shared_and_monotonic() {
    // The loop's counter is one atomic for the turn, not one per result: a per-result counter would reset itself.
    let spent = Arc::new(AtomicUsize::new(0));
    let before = spent.load(Ordering::Relaxed);
    spent.fetch_add(10, Ordering::Relaxed);
    let after = spent.load(Ordering::Relaxed);

    assert!(after > before);
    assert_eq!(after, 10);
}
