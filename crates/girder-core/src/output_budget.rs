//! Splitting one round's tool-output budget across the results in that round.
//!
//! Two mechanisms already bound tool output, and neither looks at the round as a whole.
//! [`tool_result_pruning`](crate::tool_result_pruning) bounds a *single* result against a
//! per-result limit; [`spill`](crate::spill) replaces one *single* oversized result with a locator.
//! A model that calls five tools and receives 100 KiB from each is inside both limits and still
//! 500 KiB heavier, which is the failure this module addresses: the round, not the result, is the
//! unit that overflows the window.
//!
//! # Why every result keeps a share
//!
//! One invariant outranks efficiency here: **no result is cut to zero**. From the model's side, a
//! result truncated to nothing is indistinguishable from a tool that returned nothing at all, and
//! the next step is composed on the assumption that the tool it called had no output — so it either
//! calls the tool again or reasons as though the call silently failed. A few bytes of the real
//! prefix are worth more than the bytes saved by eliminating the result, because they say *what
//! kind of thing* came back.
//!
//! # Why the large results are cut first
//!
//! Shrinking a result that already fits removes information and buys nothing: the round still had
//! room. The allocation therefore settles the small results at their full size and reduces only
//! those large enough to be part of the overflow. A 200-byte `grep` hit and a 2 MiB directory
//! listing are not equally compressible, and the small one is the one most likely to be needed
//! verbatim.
//!
//! # Why the reduction is proportional rather than greedy
//!
//! Filling the budget in arrival order until it runs out would spend everything on the first
//! results and leave the last ones at their one-byte floor. The rounds where this matters are the
//! ones with several similar-sized results — five compilations, four fetches — and starving the
//! later ones would make the model believe the later tools failed. Over-budget results are
//! therefore scaled by a common factor, so a round degrades evenly.
//!
//! # What is not here
//!
//! This module computes shares; it does not truncate text. Cutting a result is
//! [`tool_result_pruning`](crate::tool_result_pruning)'s job, because its strategies know that a
//! compiler's error is at the tail and a directory listing's meaning is at the head. Producing a
//! byte count keeps that judgement in one place instead of splitting it across two modules.

/// One round's allowance for tool output.
///
/// The fields are plain byte counts rather than ratios or token estimates: a caller that has
/// already measured its results should not have to convert units to use them, and the window
/// arithmetic that produces these numbers belongs to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// The ceiling on the sum of all allocations.
    ///
    /// This bounds what one round may carry; it is not the size of the context window. A caller
    /// normally sets it to what the window has left after the conversation already in it.
    pub total_bytes: usize,
    /// The most any single result may claim.
    ///
    /// A result beyond this is already [`tool_result_pruning`](crate::tool_result_pruning)'s
    /// concern. Capping shares here also keeps one very large result from consuming the round on
    /// its own when the smaller results are the ones a second call cannot recover.
    pub per_result_bytes: usize,
    /// Bytes held back for the most recent results before anything else is scaled.
    ///
    /// Recency is evidence of relevance: the results that arrived last are the ones the model has
    /// not yet acted on and is about to act on. Reserving for them keeps the common shape of a
    /// round — four cheap setup calls followed by the one call that answers the question — from
    /// spending the budget on the setup and scaling the answer down. Allocation walks the results
    /// newest-first and gives each one up to its full (per-result capped) size from this reserve.
    pub reserve_recent: usize,
}

/// One result's share of the round budget.
///
/// The share is a byte count, not a range, so the result keeps its own head: the caller decides
/// which end of the text survives, which is the judgement this module deliberately does not make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allocation {
    /// Position in the input, so a caller can pair the share with the result it came from.
    pub index: usize,
    /// How many bytes this result may occupy.
    ///
    /// Zero only for an empty result or a round whose budget cannot cover one byte per result;
    /// both are documented on [`allocate`].
    pub bytes: usize,
}

/// The smallest share a non-empty result receives.
///
/// One byte rather than zero is the point of the module: a zero-length result reads as a tool
/// that produced nothing.
const MIN_SHARE: usize = 1;

/// Distributes one round's budget across the sizes of its results.
///
/// Returns one entry per input size, in input order. The result is guaranteed to hold for a
/// non-degenerate budget (one where `total_bytes` covers a minimum share of every non-empty
/// result):
///
/// - no non-empty result receives zero bytes,
/// - no result receives more than [`Budget::per_result_bytes`],
/// - the shares sum to at most [`Budget::total_bytes`],
/// - a result receives its full size whenever that size fits inside an even share of what the
///   round may spend, so results that are not causing the overflow are not touched.
///
/// # The order of operations
///
/// 1. Each size is capped at [`Budget::per_result_bytes`]; the excess is pruning's business.
/// 2. [`Budget::reserve_recent`] is handed to the newest results first, up to their capped sizes.
///    Those bytes are a floor: scaling in step 4 cannot take them away.
/// 3. Every remaining result gets a floor of one byte, so no result can be scaled out of existence.
/// 4. Results whose remaining demand fits inside an even share of the round are settled at their
///    full size. They are not part of the overflow, so cutting them would cost information for no
///    gain.
/// 5. Everything still over budget is scaled by a common factor. Because the factor is shared,
///    the byte loss falls where the bytes are — on the large results — while the small ones lose
///    only in proportion to their size.
///
/// # Why the reserve does not simply shrink the budget
///
/// Treating `reserve_recent` as "bytes this round may not spend at all" would hold back capacity
/// even in rounds where nothing is scarce. Handing it to the newest results as a floor spends it
/// only when there is something to protect, and leaves it untouched when every result already fits.
///
/// # Degenerate budgets
///
/// When `total_bytes` is smaller than the floors require, the ceiling wins over the floor: the
/// newest results are served first, at their floor, until the budget is exhausted, and the oldest
/// ones receive zero. Keeping every result non-empty by exceeding the ceiling would trade a limit
/// the caller relies on for one the model merely prefers. A caller in this state has a budget too
/// small to be useful and should send fewer results, not a smaller floor.
#[must_use]
pub fn allocate(budget: &Budget, sizes: &[usize]) -> Vec<Allocation> {
    let count = sizes.len();
    if count == 0 {
        return Vec::new();
    }

    // Step 1: what each result would take if the round were unconstrained.
    let wanted: Vec<usize> = sizes
        .iter()
        .map(|&size| size.min(budget.per_result_bytes))
        .collect();

    // Step 2 and 3: the bytes no scaling may take away.
    let reserved = recency_floors(budget.reserve_recent.min(budget.total_bytes), &wanted);
    let floors: Vec<usize> = wanted
        .iter()
        .zip(&reserved)
        .map(|(&want, &held)| held.max(want.min(MIN_SHARE)))
        .collect();
    let floor_total = floors
        .iter()
        .fold(0usize, |sum, &bytes| sum.saturating_add(bytes));

    if floor_total > budget.total_bytes {
        return starved(&floors, budget.total_bytes);
    }

    let demand: Vec<usize> = wanted
        .iter()
        .zip(&floors)
        .map(|(&want, &held)| want.saturating_sub(held))
        .collect();
    let mut remaining = budget.total_bytes.saturating_sub(floor_total);

    // Step 4: settle the results that fit an even share of what is left.
    //
    // Settling cannot overspend: each settled result takes at most `even_share`, and there are
    // `count` of them, so the total is at most `count * (remaining / count) <= remaining`. The sum
    // is still built with `checked_add` and a fallback to scaling, so that a future change to the
    // threshold cannot turn an arithmetic assumption into an oversized round.
    let even_share = remaining / count;
    let mut settled = 0usize;
    let mut funded = true;
    let mut pending: Vec<usize> = Vec::with_capacity(count);
    for (index, &need) in demand.iter().enumerate() {
        if need <= even_share {
            match settled.checked_add(need) {
                Some(total) => settled = total,
                None => funded = false,
            }
        } else {
            pending.push(index);
        }
    }
    remaining = remaining.saturating_sub(settled);

    // Step 5: scale what is left across the results that are causing the overflow.
    let pending_demand: u128 = pending
        .iter()
        .map(|&index| demand[index] as u128)
        .sum::<u128>();
    let scale = if funded && pending_demand <= remaining as u128 {
        // The overflow resolved itself by settling the small results; the large ones fit whole.
        None
    } else {
        Some((remaining as u128, pending_demand))
    };

    let mut scaling = vec![false; count];
    for &index in &pending {
        scaling[index] = true;
    }

    let mut shares = floors;
    for (index, &need) in demand.iter().enumerate() {
        let addition = match scale {
            Some((pool, total)) if scaling[index] => proportional(need, pool, total),
            _ => need,
        };
        shares[index] = shares[index].saturating_add(addition);
    }

    allocations(shares)
}

/// The bytes the newest results keep regardless of scaling.
///
/// Walks the results backwards so the last one is served first: the last result is the one the
/// model is waiting on, and it should not lose its bytes to an older result that merely happens to
/// be large.
fn recency_floors(reserve: usize, wanted: &[usize]) -> Vec<usize> {
    let mut floors = vec![0usize; wanted.len()];
    let mut left = reserve;
    for (index, &want) in wanted.iter().enumerate().rev() {
        if left == 0 {
            break;
        }
        let held = want.min(left);
        floors[index] = held;
        left -= held;
    }
    floors
}

/// `item`'s share of `pool`, in proportion to `total`, rounded down.
///
/// Computed in `u128` because the product of two `usize` values overflows on a large round, and a
/// wrapping multiply here would hand a result a share smaller than a scaling step should.
fn proportional(item: usize, pool: u128, total: u128) -> usize {
    if total == 0 {
        return 0;
    }
    let scaled = (item as u128).saturating_mul(pool) / total;
    scaled.min(usize::MAX as u128) as usize
}

/// Serves the floors newest-first until the budget runs out.
///
/// Reached only when the round cannot afford one byte per non-empty result. Newest-first is the
/// same preference [`Budget::reserve_recent`] encodes, applied at the point where something has to
/// be dropped entirely.
fn starved(floors: &[usize], total: usize) -> Vec<Allocation> {
    let mut shares = vec![0usize; floors.len()];
    let mut left = total;
    for (index, &floor) in floors.iter().enumerate().rev() {
        let served = floor.min(left);
        shares[index] = served;
        left -= served;
    }
    allocations(shares)
}

/// Builds the result vector from finished shares.
fn allocations(shares: Vec<usize>) -> Vec<Allocation> {
    shares
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| Allocation { index, bytes })
        .collect()
}
