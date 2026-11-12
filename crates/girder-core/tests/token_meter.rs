//! Tests for the approximate token meter.
//!
//! The estimator is a density heuristic, not a tokenizer, so these tests pin the *arithmetic* and
//! the *known error directions* rather than claiming accuracy. A test asserting that a Chinese
//! prompt is priced correctly would fail; a test asserting it is underpriced is the honest check.
//!
//! The module is included by path because `girder-core`'s crate root does not yet declare it —
//! wiring `pub mod token_meter;` belongs to the integrator. Once that lands, this `#[path]` line
//! should be replaced with `use girder_core::token_meter::*;`.

// The module is included by path, so its `crate::llm` import would resolve against this test
// crate's root rather than `girder-core`'s. This shim supplies the same two types there. It exists
// only because `girder-core`'s crate root does not yet declare the module — wiring
// `pub mod token_meter;` belongs to the integrator — and should be deleted together with the
// `#[path]` attribute once that lands.
mod llm {
    pub use girder_core::{ContentBlock, Message};
}

#[path = "../src/token_meter.rs"]
mod token_meter;

use girder_core::{ContentBlock, Message, Role, ToolCallId};
use token_meter::{
    BLOCK_OVERHEAD, CHARS_PER_TOKEN, Price, ROLE_OVERHEAD, cost, estimate_blocks, estimate_message,
    estimate_text, price_for,
};

/// A text block.
fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

/// A user message carrying `blocks`.
fn message(blocks: Vec<ContentBlock>) -> Message {
    Message::new(Role::User, blocks)
}

/// A tool call with a valid id.
fn tool_call(name: &str, arguments: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        id: ToolCallId::new("call-1").expect("a fixed non-empty id is valid"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

#[test]
fn empty_text_costs_nothing() {
    assert_eq!(estimate_text(""), 0);
    assert_eq!(estimate_blocks(&[]), 0);
}

#[test]
fn ascii_text_rounds_up_per_four_characters() {
    assert_eq!(estimate_text("a"), 1);
    assert_eq!(estimate_text("abcd"), 1);
    assert_eq!(estimate_text("abcde"), 2);
    assert_eq!(estimate_text("abcdefgh"), 2);
    // A clean multiple must not gain a token from rounding.
    assert_eq!(estimate_text(&"x".repeat(400)), 100);
    // One past a multiple does.
    assert_eq!(estimate_text(&"x".repeat(401)), 101);
}

#[test]
fn chinese_is_counted_by_character_not_byte() {
    let han = "这是一段中文文本";
    assert_eq!(han.chars().count(), 8);
    // Counting bytes would give 24; dividing that by four gives 6. Counting characters gives 2.
    assert_eq!(estimate_text(han), 2);
    // Same character count as ASCII, so the same price: the heuristic is density-based.
    assert_eq!(estimate_text(han), estimate_text("abcdefgh"));
    // And the documented error direction: eight Han characters really cost about eight tokens.
    assert!(estimate_text(han) < 8);
}

#[test]
fn code_is_underpriced_because_punctuation_is_not_free() {
    let code = "fn main() { println!(\"hi\"); }";
    assert_eq!(code.chars().count(), 29);
    assert_eq!(estimate_text(code), 8);
    // The assertion records current behaviour and the known error: the heuristic reads punctuation
    // as ordinary characters, while a tokenizer splits it into separate tokens.
    assert!(estimate_text(code) < u64::try_from(code.chars().count()).unwrap_or(u64::MAX) / 2);
}

#[test]
fn long_text_scales_roughly_linearly() {
    let unit = "the quick brown fox jumps over the lazy dog ";
    let one = estimate_text(unit);
    let ten = estimate_text(&unit.repeat(10));
    // Each copy is an exact multiple of four characters here, so scaling is exact.
    assert_eq!(ten, one * 10);

    // On incommensurate input, rounding does not accumulate: ten copies of a ten-character string
    // is a hundred characters, hence 25 tokens, not ten times the single copy's rounded 3.
    let ragged = "abcdefghij";
    let ragged_ten = estimate_text(&ragged.repeat(10));
    assert_eq!(ragged_ten, 25);
    // Repeating can only ever save tokens relative to pricing each copy separately, because the
    // ceiling is paid once on the whole string rather than once per copy.
    assert!(ragged_ten <= estimate_text(ragged) * 10);

    // A very long string still finishes and stays proportional.
    let huge = estimate_text(&"y".repeat(4_000_000));
    assert_eq!(huge, 1_000_000);
}

#[test]
fn estimate_blocks_charges_overhead_per_block() {
    assert_eq!(estimate_blocks(&[text_block("abcd")]), 1 + BLOCK_OVERHEAD);
    assert_eq!(
        estimate_blocks(&[text_block("abcd"), text_block("abcd")]),
        2 * (1 + BLOCK_OVERHEAD)
    );
    // An empty block still carries its framing.
    assert_eq!(estimate_blocks(&[text_block("")]), BLOCK_OVERHEAD);
}

#[test]
fn estimate_blocks_prices_by_block_type() {
    let reasoning = ContentBlock::Reasoning {
        text: "abcd".to_owned(),
    };
    assert_eq!(estimate_blocks(&[reasoning]), 1 + BLOCK_OVERHEAD);

    // Name and arguments round up separately: two tokens each, plus framing.
    assert_eq!(
        estimate_blocks(&[tool_call("abcdefgh", "abcdefgh")]),
        2 + 2 + BLOCK_OVERHEAD
    );
    // Separate rounding is observable: five characters each is two tokens each (4), whereas
    // summing first (10 characters) would give three.
    assert_eq!(
        estimate_blocks(&[tool_call("abcde", "abcde")]),
        2 + 2 + BLOCK_OVERHEAD
    );

    let image = ContentBlock::Image {
        attachment: "att_0123456789".to_owned(),
    };
    let file = ContentBlock::File {
        attachment: "att_0123456789".to_owned(),
    };
    // Sixteen characters of reference: four tokens plus framing, for both attachment kinds.
    assert_eq!(
        estimate_blocks(std::slice::from_ref(&image)),
        4 + BLOCK_OVERHEAD
    );
    assert_eq!(
        estimate_blocks(std::slice::from_ref(&image)),
        estimate_blocks(std::slice::from_ref(&file))
    );
}

#[test]
fn estimate_message_adds_role_overhead() {
    let msg = message(vec![text_block("abcd")]);
    assert_eq!(estimate_blocks(&msg.content), 1 + BLOCK_OVERHEAD);
    assert_eq!(estimate_message(&msg), 1 + BLOCK_OVERHEAD + ROLE_OVERHEAD);

    // An empty message is not free: its role wrapper still serializes.
    assert_eq!(estimate_message(&message(vec![])), ROLE_OVERHEAD);

    // The delta from role overhead is exactly the constant, for every role.
    for role in [Role::User, Role::Assistant, Role::Tool] {
        let msg = Message::new(role, vec![text_block("abcd")]);
        assert_eq!(
            estimate_message(&msg),
            estimate_blocks(&msg.content) + ROLE_OVERHEAD
        );
    }
}

#[test]
fn price_for_returns_none_without_a_table() {
    // Both a real model and a nonsense one are unpriced. Asserting only the unknown case would
    // pass even if a partial table existed, so both are checked.
    assert_eq!(price_for("deepseek-chat"), None);
    assert_eq!(price_for("gpt-unknown"), None);
    assert_eq!(price_for(""), None);
}

#[test]
fn cost_scales_each_direction_independently() {
    let price = Price {
        input_per_mtok: 0.27,
        output_per_mtok: 1.10,
    };
    // One million tokens in each direction is exactly the two rates summed.
    let both = cost(price, 1_000_000, 1_000_000);
    assert!((both - 1.37).abs() < 1e-12, "expected 1.37, got {both}");

    let input_only = cost(price, 1_000_000, 0);
    assert!((input_only - 0.27).abs() < 1e-12, "got {input_only}");
    let output_only = cost(price, 0, 1_000_000);
    assert!((output_only - 1.10).abs() < 1e-12, "got {output_only}");
}

#[test]
// Exact comparison is the assertion: zero tokens multiplied by a price is exactly 0.0 in IEEE 754,
// and a tolerance would let a regression to a tiny non-zero cost pass.
#[allow(
    clippy::float_cmp,
    reason = "zero tokens times a finite price is exactly 0.0; a tolerance would hide a regression"
)]
fn cost_of_zero_tokens_is_exactly_zero() {
    let price = Price {
        input_per_mtok: 3.0,
        output_per_mtok: 15.0,
    };
    assert_eq!(cost(price, 0, 0), 0.0);
    // A finite price and zero tokens must never produce a negative zero or NaN.
    assert!(cost(price, 0, 0).is_sign_positive());
    assert!(!cost(price, 0, 0).is_nan());
}

#[test]
#[allow(
    clippy::float_cmp,
    reason = "a zero price yields exactly 0.0; a tolerance would hide a regression"
)]
fn cost_handles_partial_millions_and_free_routes() {
    let price = Price {
        input_per_mtok: 2.0,
        output_per_mtok: 8.0,
    };
    // Half a million input is 1.0; a quarter million output is 2.0.
    let partial = cost(price, 500_000, 250_000);
    assert!((partial - 3.0).abs() < 1e-12, "expected 3.0, got {partial}");

    // A single token is a millionth of the rate, not zero.
    let one = cost(price, 1, 0);
    assert!((one - 2e-6).abs() < 1e-15, "got {one}");

    // A zero price is a real answer (a local or negotiated-free model), not an error.
    let free = Price {
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
    };
    assert_eq!(cost(free, 1_000_000, 1_000_000), 0.0);
}

#[test]
fn constants_are_the_upstream_densities() {
    // Pinned so a future edit to the heuristic is a deliberate, visible change.
    assert_eq!(CHARS_PER_TOKEN, 4);
    assert_eq!(BLOCK_OVERHEAD, 4);
    assert_eq!(ROLE_OVERHEAD, 4);
}
