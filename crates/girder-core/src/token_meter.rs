//! Approximate token estimation, and why this build carries no pricing table.
//!
//! Upstream `packages/llm/token-meter` (1625 lines) does three things: it *estimates* the token
//! count of a request surface without calling a model, it *records* what providers actually
//! reported, and it prices that usage against a route's billing table. This module ports only the
//! first of those. Girder already has the second — [`crate::TokenUsage`] and
//! [`crate::SessionEvent::ModelUsage`] — and has no source for the third.
//!
//! # The estimate is approximate, and the error is not small
//!
//! Upstream's estimator (`packages/llm/token-meter/src/estimate.ts`) is a fixed character-density
//! heuristic: it divides a string's **character** count by four and adds a small constant for
//! structural JSON framing. It is not a tokenizer. The error is one-directional on the inputs that
//! matter most:
//!
//! - **CJK** text is far denser per character — a tokenizer spends roughly one token per Han
//!   character, not one per four — so a Chinese prompt is **underestimated by close to 4x**.
//! - **Code and JSON** are punctuation-heavy, and tokenizers split punctuation into its own
//!   tokens, so they are also underestimated, though by less.
//! - **Plain English prose** is the one case where four characters per token is roughly right,
//!   which is exactly why the constant exists.
//!
//! Upstream says the same in its README: the fixed heuristic "underprices CJK text and JSON
//! schemas". Treat every number here as an order-of-magnitude figure for display, compaction, and
//! pressure checks — never as a billing record. When a provider reports usage, that exact figure
//! wins; [`crate::SessionEvent::ModelUsage`] carries it.
//!
//! # Why the estimate counts characters rather than bytes
//!
//! [`str::len`] is a byte count, so dividing *it* by four would charge a Han character three tokens
//! instead of one — a different error from upstream's, and not one upstream makes. Counting
//! `chars()` keeps this port faithful to the reference and keeps the documented CJK underprice,
//! rather than accidentally landing near the right answer for the wrong reason.
//!
//! # Pricing is deliberately absent
//!
//! [`price_for`] returns [`None`] for every model. That is not an unfinished stub; see its
//! documentation for why a frozen table would be worse than no table.

use crate::llm::{ContentBlock, Message};

/// Characters per token in the fixed-density heuristic.
///
/// Four is the long-standing rule of thumb for English prose, and is the constant upstream's
/// `estimate.ts` uses. It is a *density*, not a measurement: see the module documentation for how
/// badly it misses on CJK and structured text.
pub const CHARS_PER_TOKEN: u64 = 4;

/// Structural overhead charged per content block.
///
/// A block is not free to serialize: it carries a `type` tag and object punctuation in the JSON
/// request body. Upstream prices that at a flat four tokens rather than measuring it, because the
/// exact framing is the adapter's business and varies by provider.
pub const BLOCK_OVERHEAD: u64 = 4;

/// Role framing overhead charged per message.
///
/// Every message in a request body is wrapped in an object carrying its `role` and a content array.
/// Upstream charges a flat four tokens for that wrapper, for the same reason as
/// [`BLOCK_OVERHEAD`]: the real framing belongs to the adapter.
pub const ROLE_OVERHEAD: u64 = 4;

/// Estimates the tokens in a string.
///
/// Counts characters, divides by [`CHARS_PER_TOKEN`], and rounds up, so any non-empty text costs at
/// least one token. Empty text costs zero.
///
/// This is the heuristic from upstream's `estimate.ts` and is **approximate**; see the module
/// documentation, in particular the CJK underprice. It does not include [`BLOCK_OVERHEAD`] — use
/// [`estimate_blocks`] when pricing blocks, which adds it per block.
#[must_use]
pub fn estimate_text(text: &str) -> u64 {
    let characters = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    // Ceiling division: a partial token is still a token, and a one-character string must not
    // price at zero merely because it is shorter than the divisor.
    characters.div_ceil(CHARS_PER_TOKEN)
}

/// Estimates the tokens in a sequence of content blocks.
///
/// Each block is priced by kind, mirroring upstream's `estimateContent`:
///
/// - text and reasoning blocks price their text plus [`BLOCK_OVERHEAD`];
/// - tool calls price their name and their raw JSON arguments separately, each rounded up, plus
///   [`BLOCK_OVERHEAD`] — separate rounding because the two are distinct JSON fields, and summing
///   before dividing would underprice a call whose parts each have a remainder;
/// - image and file blocks price the length of their opaque attachment reference plus
///   [`BLOCK_OVERHEAD`].
///
/// The attachment arms are where this port necessarily diverges from upstream. Upstream serializes
/// an image block's reference object and divides that JSON length by four; here an attachment is an
/// opaque string, so its own length is the only model-visible proxy available. The bytes never
/// reach the request, so a real image's visual token cost is unknowable at this layer — a documented
/// limitation of upstream too, which delegates it to per-route `LlmImageRequestPricing`. Treat those
/// two arms as a lower bound on an attachment's true cost.
///
/// The sum saturates rather than wrapping: a caller assembling an enormous history should see a
/// huge number, not a small one.
#[must_use]
pub fn estimate_blocks(blocks: &[ContentBlock]) -> u64 {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                estimate_text(text).saturating_add(BLOCK_OVERHEAD)
            }
            ContentBlock::ToolCall {
                name, arguments, ..
            } => estimate_text(name)
                .saturating_add(estimate_text(arguments))
                .saturating_add(BLOCK_OVERHEAD),
            ContentBlock::Image { attachment } | ContentBlock::File { attachment } => {
                estimate_text(attachment).saturating_add(BLOCK_OVERHEAD)
            }
        })
        .fold(0u64, u64::saturating_add)
}

/// Estimates the tokens in one message, including its role framing.
///
/// This is [`estimate_blocks`] over the message's content plus [`ROLE_OVERHEAD`].
///
/// Upstream special-cases the system role: a system prompt is serialized as a plain string rather
/// than as typed content blocks, so upstream charges text density plus role framing and *no*
/// per-block overhead. This port deliberately does not reproduce that arm. Girder's
/// [`LlmRequest`](crate::LlmRequest) carries the system prompt in a separate
/// `system_prompt: Option<String>` field, and its [`Message`] has no system role at all, so a
/// system-role message cannot arrive here; conversation content always has real per-block framing.
#[must_use]
pub fn estimate_message(message: &Message) -> u64 {
    estimate_blocks(&message.content).saturating_add(ROLE_OVERHEAD)
}

/// A route's price, in currency units per million tokens.
///
/// Both fields are per **million** tokens because that is how providers publish rates; a per-token
/// figure would be a sub-nanocent value that f64 can hold but no human wants to read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    /// Currency units charged per million input (prompt) tokens.
    pub input_per_mtok: f64,
    /// Currency units charged per million output (completion) tokens.
    pub output_per_mtok: f64,
}

/// Looks up a model's price.
///
/// # This build has no pricing table, so this returns `None` for every model
///
/// Upstream `packages/llm/token-meter` has **no** monetary pricing table. Its 1625 lines price
/// *tokens*, and its only "pricing" is attachment accounting (`route-pricing.ts`,
/// `LlmImageRequestPricing`), which converts an image into a token count — a different question
/// from what a token costs. Searching the whole upstream tree for per-million, per-route, or
/// currency rates turns up nothing, because pricing is not modeled upstream at all. There was
/// therefore nothing to port.
///
/// A hard-coded table was rejected on purpose. Provider prices change without notice, vary by
/// contract, and differ by region and cache tier; a number frozen into this source would be wrong
/// within weeks and — worse — would look authoritative while being wrong. A wrong cost figure does
/// more harm than an absent one, because callers gate budgets on it and show spend to users.
///
/// The intended path is for the component that owns provider configuration to supply rates from a
/// maintained source and call [`cost`] with them. Until then callers must treat cost as unknown,
/// not as zero.
#[must_use]
pub fn price_for(_model: &str) -> Option<Price> {
    None
}

/// Computes the cost of a call.
///
/// Scales each token count by its per-million rate and sums them:
/// `input / 1e6 * input_per_mtok + output / 1e6 * output_per_mtok`.
///
/// Zero tokens in one direction cost zero in that direction, so an all-zero call costs exactly
/// `0.0`. A zero price is a legitimate answer — a local or negotiated-free model — and is not
/// reported as an error.
///
/// The result is not rounded or currency-quantized. Rounding belongs to the display layer, which
/// knows the currency and the precision a reader expects.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "token counts are far below 2^53 in any real call, so f64 represents them exactly"
)]
pub fn cost(price: Price, input_tokens: u64, output_tokens: u64) -> f64 {
    const TOKENS_PER_MILLION: f64 = 1_000_000.0;
    let input = input_tokens as f64 / TOKENS_PER_MILLION * price.input_per_mtok;
    let output = output_tokens as f64 / TOKENS_PER_MILLION * price.output_per_mtok;
    input + output
}
