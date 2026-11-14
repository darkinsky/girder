//! Cost estimation against a price table the deployment supplies.
//!
//! This module exists because the two obvious answers are both wrong. The token meter's
//! `price_for` returns `None` for every model on purpose: a rate table frozen into the source
//! would be stale within weeks and — worse — would look authoritative while being wrong, so
//! callers would gate budgets and show spend from a number nobody had verified. But "this build
//! ships no table" is not "no prices exist": a deployment knows what it pays, and it can hand that
//! knowledge in at runtime. This module is the shape that hand-in takes, which is why every rate
//! here is injected and none is hard-coded.
//!
//! That split also explains `limits::Session::cost`: the budget field exists because a money
//! ceiling is a real requirement, but nothing produces a spend for it, because producing one needs
//! exactly the rates this build refuses to invent. With [`Table`] a deployment can produce one
//! itself without this crate claiming to know a price.
//!
//! Three decisions carry the design:
//!
//! - **No price is `None`, never `0`.** A zero asserts that the call was free. For an unpriced
//!   model that assertion is false in the direction that matters: it tells a budget there is room
//!   when there may be none, and it tells a reader that spend was measured when it was not.
//!   [`estimate`] therefore returns `None`, which forces the caller to decide what unknown cost
//!   means for it — that is the caller's judgement, not this module's.
//! - **Coverage is reported, not implied.** [`Estimate::unpriced_tokens`] and
//!   [`Batch::unpriced_models`] say which part of the usage the figure does *not* cover, because a
//!   total that silently drops the unpriced half is an underestimate that looks exact.
//! - **Integer arithmetic, multiply before divide.** Amounts are in the smallest currency unit, so
//!   they are `u64` and there is no floating point to round. A per-million rate is applied as
//!   `tokens * rate / 1_000_000` in `u128` for two reasons: dividing first discards the sub-unit
//!   remainder before it is scaled up (999_999 tokens at 3 per million is 2 units, not 0), and
//!   `u64 * u64` can exceed `u64` even when the final amount does not.
//!
//! # What this module does not do
//!
//! It does not know, fetch, or validate prices — every rate comes from [`Table`], which the
//! deployment fills. It does not convert currencies: an amount is only comparable to another
//! amount with the same [`Price::currency`], and summing across currencies produces a number with
//! no meaning. It does not round to a display precision either; that belongs to the layer that
//! knows what a reader expects to see, not to the layer that knows what was spent.

use std::collections::BTreeMap;

/// How many tokens a rate is stated per.
///
/// The divisor is fixed at one million because that is the unit providers quote in, and keeping it
/// named means the scale is visible at every use instead of being an unexplained constant.
pub const TOKENS_PER_MILLION: u64 = 1_000_000;

/// One model's rate, per million tokens.
///
/// The fields are integers in the smallest unit the deployment charges in (cents, for a
/// cents-based currency) rather than floating point. A float would invite a rounded total to be
/// displayed as if it were exact, and the smallest unit is the finest granularity a bill can
/// actually distinguish — finer precision would be invented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Price {
    /// Charged per million input (prompt) tokens.
    pub input_per_million: u64,
    /// Charged per million output (completion) tokens.
    pub output_per_million: u64,
    /// The unit both rates are stated in, for example `"USD-cents"`.
    ///
    /// Carried per price rather than per table because that is where the contract lives; a
    /// caller that mixes currencies in one table is doing something the type cannot forbid, so
    /// the string travels with every amount instead.
    pub currency: String,
}

/// A price table, keyed by exact model identifier.
///
/// Lookup is exact: no prefix matching, no case folding, no aliasing. A deployment that bills
/// `deepseek-chat` and `DeepSeek-Chat` as one model must register both names itself, because a
/// silent fuzzy match would price a model the deployment never agreed on — the failure mode this
/// module exists to avoid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    prices: BTreeMap<String, Price>,
}

impl Table {
    /// An empty table, which prices nothing.
    ///
    /// Starting empty rather than pre-filled is the point: an absent price is visible as
    /// [`None`], while a placeholder rate would be indistinguishable from a real one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces one model's price.
    ///
    /// Replacing is deliberate: a deployment that restates a price is correcting it, and keeping
    /// the first value would keep a stale rate that the operator already knows is wrong.
    pub fn set(&mut self, model: &str, price: Price) {
        self.prices.insert(model.to_owned(), price);
    }

    /// Looks up one model's price.
    ///
    /// `None` means the deployment has not priced this model. It does not mean free, and callers
    /// must not substitute zero for it.
    #[must_use]
    pub fn get(&self, model: &str) -> Option<&Price> {
        self.prices.get(model)
    }

    /// How many models are priced.
    #[must_use]
    pub fn len(&self) -> usize {
        self.prices.len()
    }

    /// Whether the table prices nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }
}

/// One model's token usage for one call or one session.
///
/// The model identifier is carried alongside the counts because a rate means nothing without it:
/// a token count alone could be priced several ways, and guessing the model would silently pick a
/// rate the caller never chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    /// The model these tokens were spent on, matched exactly against [`Table`].
    pub model: String,
    /// Input (prompt) tokens.
    pub input_tokens: u64,
    /// Output (completion) tokens.
    pub output_tokens: u64,
}

/// What one usage cost, and how much of it the figure covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// The amount, in [`Price::currency`]'s smallest unit.
    ///
    /// Saturates at `u64::MAX` rather than wrapping. Wrapping would turn an absurd amount into a
    /// small one, which is the direction that hides a problem; saturating keeps it visibly absurd.
    pub amount: u64,
    /// The rate that produced [`Estimate::amount`].
    ///
    /// Returned rather than only used, so a caller reporting a figure can also report the currency
    /// and rates behind it — a bare number in the wrong currency is worse than no number.
    pub price: Price,
    /// Tokens in the usage that the rate did not cover, so the amount excludes them.
    ///
    /// A [`Table`] entry states a rate for both directions of a usage, so a successful estimate
    /// reports `0` here: every token was covered. The field is not decoration. It makes the
    /// "how much did this cover" question answerable at the point of success, it is where a token
    /// class a flat per-direction rate cannot express (cached reads, reasoning tokens) would be
    /// reported without changing this signature, and it is a reminder to a caller summing
    /// estimates that amounts and uncovered tokens must be totalled separately.
    pub unpriced_tokens: u64,
}

/// Prices one usage.
///
/// Returns `None` when the model is not priced, and never `Some(0)` for it: zero is the assertion
/// that the call was free, and an unpriced call may have been expensive. Callers that need a
/// number anyway must choose it themselves — a budget, for instance, can treat unknown spend as
/// its whole ceiling, which is pessimistic but safe, while a display can say "unknown".
///
/// A zero *rate* is different and returns `Some`: a local or negotiated-free model is genuinely
/// priced at zero, and this function only refuses when it does not know.
#[must_use]
pub fn estimate(table: &Table, usage: &Usage) -> Option<Estimate> {
    let price = table.get(&usage.model)?.clone();
    let amount = charge(usage.input_tokens, price.input_per_million)
        .saturating_add(charge(usage.output_tokens, price.output_per_million));
    Some(Estimate {
        amount,
        price,
        unpriced_tokens: 0,
    })
}

/// Applies one per-million rate to one token count.
///
/// Multiplication happens in `u128` and before the division. `u128` because the product of two
/// `u64` values can exceed `u64` even when the resulting per-million amount fits — a large session
/// on a high rate is enough — and an intermediate wrap would report a cheap call. Multiply first
/// because `(tokens / 1_000_000) * rate` truncates every fraction of a million to zero: 999_999
/// tokens at 3 per million costs 2 units, and dividing first would report 0.
///
/// Truncation at the end is intentional and unavoidable: the smallest currency unit cannot be
/// split, so the last partial unit is dropped. The amount is therefore a floor, never a rounded or
/// inflated figure.
fn charge(tokens: u64, rate_per_million: u64) -> u64 {
    let product = u128::from(tokens) * u128::from(rate_per_million);
    let amount = product / u128::from(TOKENS_PER_MILLION);
    u64::try_from(amount).unwrap_or(u64::MAX)
}

/// A total over several usages, with the part it could not price kept separate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    /// The sum of the amounts that could be estimated, in their own currencies' smallest units.
    ///
    /// Saturates at `u64::MAX` instead of wrapping, like [`Estimate::amount`]. The batch does not
    /// check that every priced model shares one currency; a deployment mixing currencies must
    /// group by [`Price::currency`] first, because this sum would otherwise add unlike units.
    pub priced: u64,
    /// The models that had no price, in first-seen order, without duplicates.
    ///
    /// Models rather than token counts because that is what an operator can act on: an unpriced
    /// model name is a missing table entry to add, while an unpriced token count is only a symptom.
    /// Deduplicated so the same gap appears once however many calls hit it.
    pub unpriced_models: Vec<String>,
}

/// Prices a set of usages, keeping the priced and unpriced parts apart.
///
/// The two halves are reported separately rather than folded into one number because they answer
/// different questions. [`Batch::priced`] is what can be asserted; [`Batch::unpriced_models`] is
/// what must be resolved before the assertion covers the whole batch. Reporting only the former
/// would understate spend with no sign that anything was missing.
///
/// An empty slice yields an empty batch: zero priced and no unpriced models, which is the honest
/// statement that nothing was measured.
#[must_use]
pub fn estimate_batch(table: &Table, usages: &[Usage]) -> Batch {
    let mut batch = Batch::default();
    for usage in usages {
        match estimate(table, usage) {
            Some(estimate) => batch.priced = batch.priced.saturating_add(estimate.amount),
            None => {
                if !batch.unpriced_models.contains(&usage.model) {
                    batch.unpriced_models.push(usage.model.clone());
                }
            }
        }
    }
    batch
}
