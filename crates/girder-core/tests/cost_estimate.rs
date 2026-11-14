//! Tests for cost estimation against an injected price table.
//!
//! Two properties matter more than any particular number here, and both are about what happens
//! when the module does *not* know something. An unpriced model must produce `None` rather than
//! `0`, because a zero is the assertion that the call was free and a caller gates on it. And the
//! arithmetic must stay exact in both directions — no precision lost to dividing before
//! multiplying, no silent wrap in the intermediate product — because a cost figure that is quietly
//! wrong is worse than one that is absent.
//!
//! The module is included by path because `girder-core`'s crate root does not yet declare it;
//! wiring `pub mod cost_estimate;` belongs to the integrator. `cost_estimate` imports nothing from
//! the crate, so it needs no shim here — unlike `tests/token_meter.rs`, which does.

#[path = "../src/cost_estimate.rs"]
mod cost_estimate;

use cost_estimate::{
    Batch, Estimate, Price, TOKENS_PER_MILLION, Table, Usage, estimate, estimate_batch,
};

/// A price in cents, the unit every test in this file uses.
fn cents(input_per_million: u64, output_per_million: u64) -> Price {
    Price {
        input_per_million,
        output_per_million,
        currency: "USD-cents".to_owned(),
    }
}

/// A usage of `model`.
fn usage(model: &str, input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        model: model.to_owned(),
        input_tokens,
        output_tokens,
    }
}

/// A table with one priced model, `deepseek-chat`, at 3 cents in and 6 cents out per million.
fn priced_table() -> Table {
    let mut table = Table::new();
    table.set("deepseek-chat", cents(3, 6));
    table
}

#[test]
fn estimate_prices_both_directions_from_injected_rates() {
    let table = priced_table();
    // One full million each way is exactly the rate in each direction: 3 + 6.
    let estimate = estimate(&table, &usage("deepseek-chat", 1_000_000, 1_000_000));
    let Some(estimate) = estimate else {
        panic!("a priced model must estimate");
    };
    assert_eq!(estimate.amount, 9);
    assert_eq!(estimate.price.currency, "USD-cents");
}

#[test]
fn estimate_returns_none_for_an_unpriced_model() {
    // The core requirement. `None`, not `Some(0)`: zero would tell a caller the call was free.
    let table = priced_table();
    assert!(estimate(&table, &usage("not-in-the-table", 1_000_000, 1_000_000)).is_none());

    // A model the table knows nothing about must not borrow another model's rate.
    assert!(estimate(&table, &usage("deepseek-reasoner", 10, 10)).is_none());

    // Nor may an empty model name match a table entry by accident.
    assert!(estimate(&table, &usage("", 10, 10)).is_none());
}

#[test]
fn estimate_returns_none_against_an_empty_table() {
    let table = Table::new();
    assert!(table.is_empty());
    assert_eq!(table.len(), 0);
    assert!(table.get("deepseek-chat").is_none());
    assert!(estimate(&table, &usage("deepseek-chat", 1_000_000, 1_000_000)).is_none());
}

#[test]
fn an_empty_table_yields_an_empty_batch() {
    let batch = estimate_batch(&Table::new(), &[usage("a", 1, 1), usage("b", 2, 2)]);
    // Nothing priced, and both models named as gaps rather than silently dropped.
    assert_eq!(batch.priced, 0);
    assert_eq!(batch.unpriced_models, vec!["a".to_owned(), "b".to_owned()]);
}

#[test]
fn estimate_batch_of_nothing_is_a_zero_batch() {
    let batch = estimate_batch(&priced_table(), &[]);
    assert_eq!(batch, Batch::default());
    assert_eq!(batch.priced, 0);
    assert!(batch.unpriced_models.is_empty());
}

#[test]
fn a_zero_price_is_a_price_not_a_missing_one() {
    // A local or negotiated-free model is genuinely priced at zero. The module must distinguish
    // "known to be free" from "not known", which is the whole reason `None` exists.
    let mut table = Table::new();
    table.set("local-model", cents(0, 0));
    let estimate = estimate(&table, &usage("local-model", 5_000_000, 5_000_000));
    let Some(estimate) = estimate else {
        panic!("a zero rate is a known price, not an unknown one");
    };
    assert_eq!(estimate.amount, 0);
}

#[test]
fn zero_tokens_on_a_priced_model_cost_zero_but_still_estimate() {
    let table = priced_table();
    let priced = estimate(&table, &usage("deepseek-chat", 0, 0));
    let Some(priced) = priced else {
        panic!("priced model with no tokens is still a priced model");
    };
    assert_eq!(priced.amount, 0);
    // Distinct from the unpriced case: this zero means "nothing was spent", that one means
    // "unknown what was spent".
    assert!(estimate(&table, &usage("unpriced", 0, 0)).is_none());
}

#[test]
fn one_direction_only_is_charged_in_that_direction() {
    let table = priced_table();
    let Some(input_only) = estimate(&table, &usage("deepseek-chat", 2_000_000, 0)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(input_only.amount, 6);
    let Some(output_only) = estimate(&table, &usage("deepseek-chat", 0, 2_000_000)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(output_only.amount, 12);
}

#[test]
fn multiplication_precedes_division_so_a_partial_million_survives() {
    // 999_999 tokens at 3 per million is 2.999997 units, which truncates to 2.
    // Dividing first would compute (999_999 / 1_000_000) * 3 and report 0 — a whole unit lost to
    // the order of operations, on nearly a million tokens.
    let mut table = Table::new();
    table.set("m", cents(3, 3));
    let Some(estimate) = estimate(&table, &usage("m", 999_999, 0)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(estimate.amount, 2);
}

#[test]
fn a_single_token_is_not_rounded_away_by_the_rate_search() {
    // One token at a rate that makes it worth a whole unit: 1 token * 1_000_000 / 1_000_000 = 1.
    let mut table = Table::new();
    table.set("expensive", cents(1_000_000, 0));
    let Some(estimate) = estimate(&table, &usage("expensive", 1, 0)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(estimate.amount, 1);
}

#[test]
fn a_sub_unit_amount_truncates_down_never_up() {
    // 1 token at 3 per million is 0.000003 units — below the smallest currency unit, so 0.
    // Truncation is the documented behaviour: the unit cannot be split, and rounding up would
    // overstate spend on every small call.
    let mut table = Table::new();
    table.set("cheap", cents(3, 3));
    let Some(estimate) = estimate(&table, &usage("cheap", 1, 1)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(estimate.amount, 0);
    assert_eq!(estimate.unpriced_tokens, 0, "the tokens were priced, at ~0");
}

#[test]
fn the_intermediate_product_uses_u128_so_it_does_not_wrap() {
    // 3e9 tokens * 1e10 per million = 3e19, which exceeds u64::MAX (about 1.8e19). The final
    // amount, 3e13, fits comfortably in u64 — so a u64 intermediate would wrap into a small,
    // believable, wrong number while a u128 intermediate gives the right one.
    let mut table = Table::new();
    table.set("huge-rate", cents(10_000_000_000, 0));
    let Some(estimate) = estimate(&table, &usage("huge-rate", 3_000_000_000, 0)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(estimate.amount, 30_000_000_000_000);
    assert!(
        u64::try_from(
            u128::from(3_000_000_000_u64) * u128::from(10_000_000_000_u64)
        )
        .is_err(),
        "the product really does exceed u64, so this test does exercise the u128 path"
    );
}

#[test]
fn an_amount_beyond_u64_saturates_instead_of_wrapping() {
    // The product of two u64::MAX values still fits in u128, but the per-million amount does not
    // fit in u64. Wrapping would turn an absurd total into a small one; saturating keeps it
    // visibly absurd, which is the safe direction for a number a budget reads.
    let mut table = Table::new();
    table.set("absurd", cents(u64::MAX, u64::MAX));
    let Some(estimate) = estimate(&table, &usage("absurd", u64::MAX, 0)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(estimate.amount, u64::MAX);
}

#[test]
fn saturating_addition_keeps_a_both_directions_total_absurd_rather_than_small() {
    // Both directions saturate individually, and their sum must not wrap back to a small number.
    let mut table = Table::new();
    table.set("absurd", cents(u64::MAX, u64::MAX));
    let Some(estimate) = estimate(&table, &usage("absurd", u64::MAX, u64::MAX)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(estimate.amount, u64::MAX);
}

#[test]
fn unpriced_tokens_is_zero_when_the_rate_covers_the_whole_usage() {
    // A table entry states a rate for both directions, so nothing is left uncovered. The field is
    // reported at the point of success so a caller summing estimates knows the figure is complete.
    let table = priced_table();
    let Some(estimate) = estimate(&table, &usage("deepseek-chat", 12_345, 6_789)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(estimate.unpriced_tokens, 0);
}

#[test]
fn the_estimate_carries_the_price_it_used() {
    // A bare amount is unauditable. The rate and currency travel with it so a report can state
    // both, and so a caller can tell two amounts in different currencies apart.
    let table = priced_table();
    let Some(estimate) = estimate(&table, &usage("deepseek-chat", 1_000_000, 0)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(
        estimate.price,
        Price {
            input_per_million: 3,
            output_per_million: 6,
            currency: "USD-cents".to_owned(),
        }
    );
}

#[test]
fn estimate_batch_separates_priced_from_unpriced() {
    let table = priced_table();
    let batch = estimate_batch(
        &table,
        &[
            usage("deepseek-chat", 1_000_000, 0), // 3
            usage("unknown-a", 500_000, 500_000),
            usage("deepseek-chat", 0, 1_000_000), // 6
            usage("unknown-b", 1, 1),
        ],
    );
    // Only the priced half is summed: 3 + 6.
    assert_eq!(batch.priced, 9);
    // Both gaps are named, rather than the total silently omitting them.
    assert_eq!(
        batch.unpriced_models,
        vec!["unknown-a".to_owned(), "unknown-b".to_owned()]
    );
}

#[test]
fn estimate_batch_names_a_repeated_unpriced_model_once() {
    let table = priced_table();
    let batch = estimate_batch(
        &table,
        &[
            usage("unknown", 1, 1),
            usage("unknown", 2, 2),
            usage("deepseek-chat", 1_000_000, 0),
            usage("unknown", 3, 3),
        ],
    );
    assert_eq!(batch.priced, 3);
    // A gap is a missing table entry, so it is reported once however many calls hit it.
    assert_eq!(batch.unpriced_models, vec!["unknown".to_owned()]);
}

#[test]
fn estimate_batch_saturates_its_total() {
    let mut table = Table::new();
    table.set("absurd", cents(u64::MAX, u64::MAX));
    let batch = estimate_batch(
        &table,
        &[
            usage("absurd", u64::MAX, 0),
            usage("absurd", u64::MAX, 0),
        ],
    );
    assert_eq!(batch.priced, u64::MAX);
    assert!(batch.unpriced_models.is_empty());
}

#[test]
fn a_batch_of_only_priced_usages_has_no_gaps() {
    let table = priced_table();
    let batch = estimate_batch(
        &table,
        &[
            usage("deepseek-chat", 1_000_000, 0),
            usage("deepseek-chat", 0, 1_000_000),
        ],
    );
    assert_eq!(batch.priced, 9);
    assert!(batch.unpriced_models.is_empty());
}

#[test]
fn a_batch_of_only_unpriced_usages_prices_nothing_and_says_so() {
    let table = priced_table();
    let batch = estimate_batch(&table, &[usage("a", 1_000_000, 1_000_000)]);
    // Not zero because it was free — zero because nothing could be priced. The model name is what
    // distinguishes the two cases for the caller.
    assert_eq!(batch.priced, 0);
    assert_eq!(batch.unpriced_models, vec!["a".to_owned()]);
}

#[test]
fn lookup_is_exact_and_respects_case() {
    // A fuzzy or case-folding match would price a model the deployment never agreed on, which is
    // exactly the "authoritative but wrong" failure this module is built to avoid.
    let table = priced_table();
    assert!(table.get("deepseek-chat").is_some());
    assert!(table.get("DeepSeek-Chat").is_none());
    assert!(table.get("deepseek").is_none());
    assert!(table.get("deepseek-chat-v2").is_none());
    assert!(estimate(&table, &usage("DeepSeek-Chat", 1_000_000, 0)).is_none());
}

#[test]
fn setting_a_model_twice_replaces_the_stale_rate() {
    // A restated price is a correction. Keeping the first value would keep a rate the operator
    // already knows is wrong.
    let mut table = Table::new();
    table.set("m", cents(3, 6));
    table.set("m", cents(5, 10));
    assert_eq!(table.len(), 1);
    let Some(estimate) = estimate(&table, &usage("m", 1_000_000, 1_000_000)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(estimate.amount, 15);
}

#[test]
fn distinct_models_hold_distinct_rates() {
    let mut table = Table::new();
    table.set("cheap", cents(1, 2));
    table.set("dear", cents(10, 60));
    assert_eq!(table.len(), 2);

    let Some(cheap) = estimate(&table, &usage("cheap", 1_000_000, 1_000_000)) else {
        panic!("priced model must estimate");
    };
    let Some(dear) = estimate(&table, &usage("dear", 1_000_000, 1_000_000)) else {
        panic!("priced model must estimate");
    };
    assert_eq!(cheap.amount, 3);
    assert_eq!(dear.amount, 70);
}

#[test]
fn the_per_million_scale_is_one_million() {
    // Pinned because every amount in this module depends on it, and a changed scale would make
    // every stored rate silently mean something else.
    assert_eq!(TOKENS_PER_MILLION, 1_000_000);
}

#[test]
fn estimate_is_reusable_and_does_not_mutate_the_table() {
    let table = priced_table();
    let before = table.clone();
    let first: Option<Estimate> = estimate(&table, &usage("deepseek-chat", 1_000_000, 0));
    let second: Option<Estimate> = estimate(&table, &usage("deepseek-chat", 1_000_000, 0));
    assert_eq!(first, second);
    assert_eq!(table, before);
}
