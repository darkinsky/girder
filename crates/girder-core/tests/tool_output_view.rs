//! Tests for the budgeted tool-output view.
//!
//! Two properties carry this module, and both have a test that fails loudly if the accounting is
//! wrong:
//!
//! - [`an_image_consumes_budget`] — the whole reason the module exists. A view that prices only
//!   text passes every other test here and still hands the model a result it cannot afford, so
//!   the image charge is asserted before anything else.
//! - [`a_cut_never_splits_a_character`] — the corruption case. A byte-based cut leaves a
//!   replacement character in a Chinese sentence, which a model reads as a broken tool rather than
//!   as a shortened result.
//!
//! The remaining tests cover the reporting contract: a cut and an omission must both be visible in
//! the rendered text, because a shortened result that looks complete is worse than a large one.
//!
//! # Why the module is compiled by path
//!
//! `tool_output_view` is not declared in the crate root yet; the wiring is a separate change to
//! `lib.rs`, which this task must not touch. Including the exact source file keeps these tests
//! runnable now and independent of that wiring, while still exercising the real file rather than a
//! copy of it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/tool_output_view.rs"]
mod tool_output_view;

use girder_core::{ContentBlock, ToolCallId};
use tool_output_view::{ATTACHMENT_COST_CHARS, MIN_KEPT_CHARS, Outcome, View, block_cost};

const IMAGE_REF: &str = "att_0123456789";

fn text(body: &str) -> ContentBlock {
    ContentBlock::Text {
        text: body.to_owned(),
    }
}

fn image() -> ContentBlock {
    ContentBlock::Image {
        attachment: IMAGE_REF.to_owned(),
    }
}

fn file() -> ContentBlock {
    ContentBlock::File {
        attachment: "att_file".to_owned(),
    }
}

fn call(name: &str, arguments: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        id: ToolCallId::new("call-1").expect("a literal call id is valid"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

#[test]
fn a_fitting_text_is_kept_whole() {
    let mut view = View::new(100);
    assert_eq!(view.add_block(&text("hello")), Outcome::Kept);
    assert_eq!(view.render(), "hello");
    assert_eq!(view.omitted(), 0);
    assert_eq!(view.remaining(), 95);
}

#[test]
fn an_image_consumes_budget() {
    // The load-bearing test. The reference is 14 characters; if the view charged its length the
    // image would fit a 20-character budget, and the model would receive a full picture that this
    // budget claimed it could not afford.
    let mut view = View::new(20);
    assert_eq!(block_cost(&image()), ATTACHMENT_COST_CHARS);
    assert_eq!(view.add_block(&image()), Outcome::Omitted);
    assert_eq!(view.omitted(), 1);
    assert!(view.render().contains("1 of 1 blocks omitted"));
}

#[test]
fn a_file_costs_the_same_as_an_image() {
    // A file handle's text is short, but whether a route inlines the file is not this layer's
    // decision, so pricing it by its name would underprice the case the budget exists to catch.
    assert_eq!(block_cost(&file()), ATTACHMENT_COST_CHARS);
    assert_eq!(block_cost(&image()), block_cost(&file()));
}

#[test]
fn a_budget_holds_several_images_but_not_many() {
    // With a charge per attachment, three fit and the fourth is refused. A text-only budget would
    // have accepted every one of them.
    let mut view = View::new(3 * ATTACHMENT_COST_CHARS);
    assert_eq!(view.add_block(&image()), Outcome::Kept);
    assert_eq!(view.add_block(&image()), Outcome::Kept);
    assert_eq!(view.add_block(&image()), Outcome::Kept);
    assert_eq!(view.add_block(&image()), Outcome::Omitted);
    assert_eq!(view.omitted(), 1);
    assert_eq!(view.remaining(), 0);
}

#[test]
fn an_image_is_never_truncated() {
    // Half an image is not a smaller image: a partial attachment is either unreachable or
    // misleading, so the only two outcomes for one are "whole" and "absent".
    let mut view = View::new(ATTACHMENT_COST_CHARS + 500);
    assert_eq!(view.add_block(&image()), Outcome::Kept);
    assert_eq!(view.add_block(&image()), Outcome::Omitted);
}

#[test]
fn an_over_budget_text_is_truncated() {
    let body = "x".repeat(200);
    let mut view = View::new(100);
    assert_eq!(
        view.add_block(&text(&body)),
        Outcome::Truncated {
            kept: 100,
            total: 200
        }
    );
    assert_eq!(view.omitted(), 0);
    assert!(view.render().starts_with(&"x".repeat(100)));
    assert!(view.render().contains("kept 100 of 200 characters"));
}

#[test]
fn a_cut_never_splits_a_character() {
    // Chinese prose, three bytes per character. A byte-based cut at 10 would land inside the
    // fourth code point and produce a replacement character.
    let body = "工具输出视图的预算核算";
    let mut view = View::new(3);
    // Below the floor a fragment identifies nothing, so nothing is kept.
    assert_eq!(view.add_block(&text(body)), Outcome::Omitted);

    let mut wide = View::new(MIN_KEPT_CHARS + 2);
    let long = format!("{body}{}", "张".repeat(200));
    assert_eq!(
        wide.add_block(&text(&long)),
        Outcome::Truncated {
            kept: MIN_KEPT_CHARS + 2,
            total: long.chars().count()
        }
    );
    let rendered = wide.render();
    // The replacement character is what a byte-based cut leaves behind.
    assert!(!rendered.contains('\u{FFFD}'), "rendered: {rendered}");
    // The retained prefix is a real prefix of the source, character for character.
    let kept = kept_prefix(&rendered);
    assert!(long.starts_with(kept), "kept: {kept}");
    assert_eq!(kept.chars().count(), MIN_KEPT_CHARS + 2);
}

/// The retained prefix of a rendered view: everything before its truncation marker.
fn kept_prefix(rendered: &str) -> &str {
    rendered
        .split("\n[truncated: ")
        .next()
        .unwrap_or_default()
        .trim_end_matches('\n')
}

#[test]
fn an_emoji_is_not_split() {
    // Four-byte code points: a byte cut inside one is the worst version of the same bug.
    let body = "🎉".repeat(200);
    let budget = MIN_KEPT_CHARS + 10;
    let mut view = View::new(budget);
    assert_eq!(
        view.add_block(&text(&body)),
        Outcome::Truncated {
            kept: budget,
            total: 200
        }
    );
    let rendered = view.render();
    assert!(!rendered.contains('\u{FFFD}'), "rendered: {rendered}");
    assert_eq!(rendered.matches('🎉').count(), budget);
}

#[test]
fn a_fragment_too_short_to_identify_anything_is_omitted() {
    // Twenty characters of a build log say neither what failed nor that anything was cut, so the
    // block is dropped and reported instead of being kept as a misleading scrap.
    let mut view = View::new(MIN_KEPT_CHARS - 1);
    assert_eq!(view.add_block(&text(&"y".repeat(500))), Outcome::Omitted);
    assert_eq!(view.omitted(), 1);
    assert!(view.render().starts_with("[view:"));
}

#[test]
fn text_and_image_together_share_one_budget() {
    let mut view = View::new(ATTACHMENT_COST_CHARS + 20);
    assert_eq!(view.add_block(&text("summary: build ok")), Outcome::Kept);
    assert_eq!(view.add_block(&image()), Outcome::Kept);
    // The text spent its characters, so the second image no longer fits.
    assert_eq!(view.add_block(&image()), Outcome::Omitted);
    let rendered = view.render();
    assert!(rendered.contains("summary: build ok"));
    assert!(rendered.contains(IMAGE_REF));
}

#[test]
fn omitted_counts_every_dropped_block() {
    let mut view = View::new(ATTACHMENT_COST_CHARS);
    assert_eq!(view.add_block(&image()), Outcome::Kept);
    assert_eq!(view.add_block(&image()), Outcome::Omitted);
    assert_eq!(view.add_block(&file()), Outcome::Omitted);
    assert_eq!(view.add_block(&text("tail")), Outcome::Omitted);
    assert_eq!(view.omitted(), 3);
}

#[test]
fn the_render_says_what_was_omitted() {
    // A model that is not told will answer about output it never saw.
    let mut view = View::new(ATTACHMENT_COST_CHARS);
    assert_eq!(view.add_block(&text("first block")), Outcome::Kept);
    assert_eq!(view.add_block(&image()), Outcome::Omitted);
    assert_eq!(view.add_block(&file()), Outcome::Omitted);
    let rendered = view.render();
    assert!(
        rendered.contains("2 of 3 blocks omitted"),
        "rendered: {rendered}"
    );
    assert!(rendered.contains("budget 1000 characters"));
}

#[test]
fn an_untruncated_view_carries_no_notice() {
    // A marker that reports a cut which did not happen teaches the model to distrust the markers
    // that report real ones.
    let mut view = View::new(1_000);
    assert_eq!(view.add_block(&text("a")), Outcome::Kept);
    assert_eq!(view.render(), "a");
}

#[test]
fn an_empty_view_renders_empty() {
    let view = View::new(0);
    assert_eq!(view.render(), "");
    assert_eq!(view.omitted(), 0);
    assert_eq!(view.remaining(), 0);
}

#[test]
fn a_tool_call_is_charged_its_name_and_arguments() {
    assert_eq!(block_cost(&call("bash", "{\"cmd\":\"ls\"}")), 4 + 12);
    let mut view = View::new(4 + 12);
    assert_eq!(
        view.add_block(&call("bash", "{\"cmd\":\"ls\"}")),
        Outcome::Kept
    );
    assert!(view.render().contains("[tool call bash]"));
}

#[test]
fn a_zero_budget_omits_a_block_without_a_fragment() {
    let mut view = View::new(0);
    assert_eq!(view.add_block(&text("anything")), Outcome::Omitted);
    assert_eq!(view.omitted(), 1);
    // Nothing of the block's content survived, so the only text is the notice.
    assert_eq!(
        view.render(),
        "[view: 1 of 1 blocks omitted; budget 0 characters]"
    );
}

#[test]
fn truncation_and_omission_are_both_reported() {
    let mut view = View::new(ATTACHMENT_COST_CHARS + MIN_KEPT_CHARS);
    assert_eq!(
        view.add_block(&text(&"z".repeat(2_000))),
        Outcome::Truncated {
            kept: ATTACHMENT_COST_CHARS + MIN_KEPT_CHARS,
            total: 2_000
        }
    );
    assert_eq!(view.add_block(&image()), Outcome::Omitted);
    let rendered = view.render();
    assert!(rendered.contains("1 truncated"), "rendered: {rendered}");
    assert!(
        rendered.contains("1 of 2 blocks omitted"),
        "rendered: {rendered}"
    );
}

#[test]
fn a_reasoning_block_is_priced_as_text() {
    // Reasoning is not sent back to the provider, but it is still characters the caller asked to
    // carry, so pricing it at zero would let a view hide its real size.
    let reasoning = ContentBlock::Reasoning {
        text: "thinking".to_owned(),
    };
    assert_eq!(block_cost(&reasoning), 8);
    let mut view = View::new(8);
    assert_eq!(view.add_block(&reasoning), Outcome::Kept);
}
