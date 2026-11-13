//! Tests for the pre-flight context budget.
//!
//! The property that matters is stated once and then exercised from every direction: a request may
//! only be admitted if it fits what is left after the output reservation **and** the margin. Most of
//! these cases exist to pin down the failures that a naive `total <= window` comparison would let
//! through, since those are the ones that cost a truncated generation rather than an error.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the budget as written even before the crate root wires it in.

#[path = "../src/context_budget.rs"]
mod context_budget;

use context_budget::{Budget, Decision, Usage, check};

/// A 100k window with 8k reserved for output and 2k of margin: 90k for input.
fn budget() -> Budget {
    Budget::new(100_000, 8_000, 2_000)
}

#[test]
fn the_allowance_is_the_window_minus_output_and_margin() {
    assert_eq!(budget().input_allowance(), 90_000);
}

#[test]
fn a_request_that_fits_is_admitted() {
    let usage = Usage::new(5_000, 60_000, 10_000);
    assert_eq!(usage.total(), 75_000);
    assert_eq!(check(&budget(), &usage), Decision::Fits);
}

#[test]
fn a_request_exactly_on_the_allowance_fits() {
    // Exactly 90k: no slack, but nothing to shed either. The comparison must be inclusive, or a
    // request that is already maximal would be reported as over by zero tokens.
    let usage = Usage::new(10_000, 70_000, 10_000);
    assert_eq!(usage.total(), 90_000);
    assert_eq!(check(&budget(), &usage), Decision::Fits);
}

#[test]
fn one_token_over_the_allowance_is_rejected() {
    let usage = Usage::new(10_000, 70_001, 10_000);
    assert_eq!(
        check(&budget(), &usage),
        Decision::Over { excess: 1 },
        "the boundary is inclusive on the allowance and exclusive one token past it"
    );
}

#[test]
fn an_oversized_request_reports_exactly_what_must_be_shed() {
    let usage = Usage::new(20_000, 100_000, 15_000);
    assert_eq!(usage.total(), 135_000);
    assert_eq!(check(&budget(), &usage), Decision::Over { excess: 45_000 });
}

#[test]
fn the_output_reservation_is_subtracted_before_the_input_is_measured() {
    // 95k of input against a 100k window looks fine if the window is compared directly -- and it is
    // not: 8k is reserved for the model to write with. Admitting this request would accept it and
    // then truncate the generation, so the reservation has to be paid first.
    let usage = Usage::new(5_000, 80_000, 10_000);
    assert_eq!(usage.total(), 95_000);
    assert!(
        usage.total() < budget().window,
        "the whole point of this case is that it is under the raw window"
    );
    assert_eq!(
        check(&budget(), &usage),
        Decision::Over { excess: 5_000 },
        "90k allowance against a 95k request"
    );
}

#[test]
fn a_request_that_only_the_margin_rejects_is_rejected() {
    // 92k fits the window minus output (92k > 92k is false, so this is the allowance boundary plus
    // the margin) and does not fit once the 2k error allowance is charged.
    let usage = Usage::new(2_000, 80_000, 10_000);
    assert_eq!(usage.total(), 92_000);
    assert_eq!(
        check(&Budget::new(100_000, 8_000, 0), &usage),
        Decision::Fits,
        "without a margin this request would be admitted"
    );
    assert_eq!(
        check(&budget(), &usage),
        Decision::Over { excess: 2_000 },
        "the margin is what turns an estimate that only just fits into a refusal"
    );
}

#[test]
fn the_margin_reduces_the_allowance_by_its_whole_amount() {
    let usage = Usage::new(0, 50_000, 0);
    for margin in [0, 1, 1_000, 10_000] {
        let budget = Budget::new(60_000, 5_000, margin);
        assert_eq!(budget.input_allowance(), 55_000 - margin);
    }
    // The same 50k request, admitted and refused purely by where the margin lands the allowance:
    // 55k of allowance covers it, 45k does not. That is the subtraction reaching the comparison.
    assert_eq!(
        check(&Budget::new(60_000, 5_000, 0), &usage),
        Decision::Fits
    );
    assert_eq!(
        check(&Budget::new(60_000, 5_000, 10_000), &usage),
        Decision::Over { excess: 5_000 }
    );
}

#[test]
fn a_window_smaller_than_the_output_reservation_yields_no_allowance() {
    // The arithmetic that would underflow: a 4k window with 8k reserved. Reported as zero, not as a
    // wrapped 18-quintillion-token allowance that would admit every request ever built.
    let broken = Budget::new(4_000, 8_000, 0);
    assert_eq!(broken.input_allowance(), 0);
    assert_eq!(
        check(&broken, &Usage::new(0, 1, 0)),
        Decision::Over { excess: 1 }
    );
}

#[test]
fn a_window_smaller_than_output_plus_margin_also_yields_zero() {
    // Both subtractions saturate together: the reservation alone is already over the window.
    let broken = Budget::new(10_000, 9_500, 1_000);
    assert_eq!(broken.input_allowance(), 0);
}

#[test]
fn a_zero_window_yields_no_allowance() {
    let none = Budget::new(0, 0, 0);
    assert_eq!(none.input_allowance(), 0);
    assert_eq!(check(&none, &Usage::default()), Decision::Fits);
    assert_eq!(
        check(&none, &Usage::new(1, 0, 0)),
        Decision::Over { excess: 1 }
    );
}

#[test]
fn an_empty_request_fits() {
    // Nothing to send is nothing to shed. This must not be treated as an error: a caller probing a
    // budget before it has assembled a request is doing something legitimate.
    let usage = Usage::default();
    assert_eq!(usage.total(), 0);
    assert_eq!(check(&budget(), &usage), Decision::Fits);
}

#[test]
fn the_total_sums_every_component() {
    assert_eq!(Usage::new(1, 2, 3).total(), 6);
    assert_eq!(Usage::new(0, 0, 3).total(), 3);
    assert_eq!(Usage::new(7, 0, 0).total(), 7);
}

#[test]
fn the_total_saturates_instead_of_wrapping() {
    // A wrapping sum would report a small total for an absurd request and wave it through. Saturation
    // reports the largest representable cost, which no allowance can cover.
    let huge = Usage::new(u64::MAX, u64::MAX, u64::MAX);
    assert_eq!(huge.total(), u64::MAX);
    assert_eq!(
        check(&budget(), &huge),
        Decision::Over {
            excess: u64::MAX - 90_000
        }
    );
}

#[test]
fn a_zero_allowance_boundary_with_an_exactly_zero_request_still_fits() {
    // The tightest possible boundary: an allowance of zero and a request costing exactly zero.
    let zero = Budget::new(1_000, 1_000, 0);
    assert_eq!(zero.input_allowance(), 0);
    assert_eq!(check(&zero, &Usage::default()), Decision::Fits);
}

#[test]
fn the_constructor_keeps_every_field_verbatim() {
    // `Budget` does not "fix up" incoherent parts: a caller that wants to report a misconfiguration
    // needs to see what it actually configured.
    let budget = Budget::new(1, 2, 3);
    assert_eq!(budget.window, 1);
    assert_eq!(budget.reserve_output, 2);
    assert_eq!(budget.margin, 3);
    assert_eq!(budget.input_allowance(), 0);
}
