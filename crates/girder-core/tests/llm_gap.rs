//! Tests for provider-side vision-token pricing.
//!
//! The reference values here are copied from the provider's published image-token calculator
//! (api-docs.deepseek.com, "Token & Token Usage") in its `v41` configuration, by way of upstream
//! `packages/llm/llm-deepseek/tests/image-tokens.spec.ts`. They are the reason this port can claim
//! to be *exact* rather than approximate: every one of them is an answer the provider itself gives,
//! so an implementation that disagrees is wrong regardless of how defensible its arithmetic looks.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `llm_gap`; adding `pub mod llm_gap;` and its `pub use`
//! belongs to the integrator. The module is therefore compiled into this test binary directly, which
//! also means it has no dependency on the rest of the crate today. Once the wiring lands, the
//! `#[path]` attribute should be replaced with `use girder_core::llm_gap::*;`.

#[path = "../src/llm_gap.rs"]
mod llm_gap;

use llm_gap::{
    Dimensions, MAX_IMAGE_TOKENS, MIN_PIXELS, image_tokens, price_request_image, request_dimensions,
};

/// The provider's published reference prices: `(width, height, tokens)`.
///
/// Kept as one table rather than as individual assertions because the cases exist to pin the whole
/// curve — small-image floor, ordinary photographs, the cap, and the degenerate strips — and a table
/// makes a missing region obvious.
const PUBLISHED_PRICES: [(u32, u32, u64); 13] = [
    (100, 100, 184),
    (544, 544, 184),
    (640, 480, 206),
    (800, 800, 422),
    (1024, 768, 496),
    (1066, 600, 407),
    (1300, 1300, 994),
    (1920, 1080, 968),
    (2000, 2000, 994),
    (5000, 5000, 994),
    (300, 50, 200),
    (8192, 100, 593),
    (16, 8192, 590),
];

#[test]
fn prices_published_reference_geometries() {
    for (width, height, expected) in PUBLISHED_PRICES {
        assert_eq!(
            image_tokens(width, height),
            expected,
            "{width}x{height} should price at {expected} tokens"
        );
    }
}

#[test]
fn caps_every_geometry_at_the_provider_limit() {
    // The cap is the property a budget depends on: a caller reserving space for one image must be
    // able to assume a bounded cost, whatever the user uploaded.
    let geometries = [
        (2000_u32, 2000_u32),
        (5000, 5000),
        (8192, 8192),
        (16, 8192),
        (8192, 16),
        (u32::MAX, u32::MAX),
        (u32::MAX, 1),
        (1, u32::MAX),
    ];
    for (width, height) in geometries {
        let tokens = image_tokens(width, height);
        assert!(
            tokens <= MAX_IMAGE_TOKENS,
            "{width}x{height} priced at {tokens}, above the {MAX_IMAGE_TOKENS} cap"
        );
    }
}

#[test]
fn prices_small_images_at_the_scale_up_floor() {
    // Below the documented floor the provider scales the image *up*, so a tiny icon is not cheap.
    // This is the single most counter-intuitive case in the calculator and the one a hand-written
    // heuristic gets wrong by two orders of magnitude.
    let floor = image_tokens(544, 544);
    for side in [1_u32, 2, 10, 100, 300, 543] {
        assert_eq!(
            image_tokens(side, side),
            floor,
            "{side}x{side} should price at the {MIN_PIXELS}-pixel floor"
        );
    }
}

#[test]
fn solves_a_single_row_grid_for_an_extremely_wide_image() {
    // An aspect ratio near zero drives the width root below one cell, so the solver must fall back
    // to a one-row grid. A naive `sqrt` result of zero cells would divide by zero downstream.
    assert_eq!(image_tokens(9000, 1), MAX_IMAGE_TOKENS);
}

#[test]
fn solves_a_single_column_grid_for_an_extremely_tall_image() {
    assert_eq!(image_tokens(1, 9000), MAX_IMAGE_TOKENS);
}

#[test]
fn converges_through_repeated_projection_passes() {
    // These geometries are not at a fixpoint after one pass: the first grid's pixels re-price to a
    // different grid. They exist to prove the iteration runs, not just the closed form.
    assert_eq!(image_tokens(12, 1123), 380);
    assert_eq!(image_tokens(89, 2076), 254);
}

#[test]
fn is_asymmetric_in_width_and_height() {
    // A transposed geometry is a different price, which is why the geometry has named fields rather
    // than a tuple: transposing the arguments at a call site must be a type-level impossibility, and
    // this pins that the two orders genuinely differ.
    assert_ne!(image_tokens(8192, 100), image_tokens(100, 8192));
}

#[test]
fn answers_an_absent_image_with_no_price() {
    // A zero axis is the absence of an image, not a very thin one. Charging the two framing tokens
    // upstream's arithmetic would produce would report a cost for something never sent.
    for (width, height) in [(0_u32, 0_u32), (0, 100), (100, 0)] {
        assert_eq!(image_tokens(width, height), 0, "{width}x{height}");
    }
}

#[test]
fn pads_both_axes_to_whole_patches_before_counting_cells() {
    // 544 is the scale-up floor and 546 is its patch-padded width (39 patches), which is 13 token
    // cells and therefore 13 * 14 + 2 = 184 tokens. One pixel past that block pads to 40 patches,
    // which is 14 cells: the price steps to 212. This is the padding step being observable at all —
    // an implementation that downsampled before padding would charge a cell for a partial patch and
    // would not step here.
    assert_eq!(image_tokens(546, 546), 184);
    assert_eq!(image_tokens(547, 547), 212);
    // Within one patch block the price must not move, which is what "pad, then downsample" means.
    let within_block = image_tokens(544, 544);
    for side in 545..=546 {
        assert_eq!(
            image_tokens(side, side),
            within_block,
            "{side}x{side} is in the same patch block as 544x544"
        );
    }
}

#[test]
fn counts_patches_of_at_least_one_cell_on_the_smallest_axis() {
    // A single patch is narrower than one cell after 3:1 downsampling, so it must count as one cell
    // rather than zero — a zero would price the geometry at the two framing tokens alone.
    // One patch edge (14 px, pinned by `PATCH_SIZE`) on each axis.
    let tiny = image_tokens(14, 14);
    assert_eq!(
        tiny,
        image_tokens(544, 544),
        "below the floor, always the floor"
    );
    assert!(tiny > 2, "one cell must cost more than the framing alone");
}

#[test]
fn sends_a_geometry_unchanged_when_its_grid_fits() {
    let cases = [(800_u32, 800_u32), (1302, 1302), (8192, 78), (1, 9000)];
    for (width, height) in cases {
        assert_eq!(
            request_dimensions(width, height),
            Dimensions::new(width, height),
            "{width}x{height} already fits and must be sent as-is"
        );
    }
}

#[test]
fn downscales_a_geometry_to_the_solved_long_edge() {
    let cases = [
        (1303_u32, 1303_u32, 1302_u32, 1302_u32),
        (2048, 2048, 1302, 1302),
        (2048, 1024, 1848, 924),
        (3840, 2160, 1708, 961),
        (1080, 2400, 838, 1862),
    ];
    for (width, height, expected_width, expected_height) in cases {
        let sent = request_dimensions(width, height);
        assert_eq!(
            sent,
            Dimensions::new(expected_width, expected_height),
            "{width}x{height} should be sent as {expected_width}x{expected_height}"
        );
        // The resize exists to fit the cap, so the sent geometry and the source must price the
        // same. If they diverge, the harness is sending an image whose cost it did not predict.
        assert_eq!(
            image_tokens(sent.width, sent.height),
            image_tokens(width, height),
            "{width}x{height} repriced after resizing"
        );
    }
}

#[test]
fn prices_the_sent_geometry_rather_than_the_source() {
    // The case that motivates `price_request_image` existing at all: the source prices at 959 but
    // the request prices at 992, because rounding the aspect-preserving short edge crosses a
    // token-cell boundary. A caller that priced the source would under-reserve by 33 tokens.
    let price = price_request_image(1224, 1429);
    assert_eq!(price.dimensions, Dimensions::new(1187, 1386));
    assert_eq!(price.tokens, 992);
    assert_eq!(image_tokens(1224, 1429), 959);
}

#[test]
fn prices_an_absent_image_as_absent() {
    let price = price_request_image(0, 0);
    assert_eq!(price.dimensions, Dimensions::new(0, 0));
    assert_eq!(price.tokens, 0);
    assert!(price.dimensions.is_empty());
}

#[test]
fn reports_the_dimensions_it_priced() {
    // The price and the geometry travel together precisely so the number can be explained. For a
    // geometry sent unchanged they must be the source's own.
    let price = price_request_image(640, 480);
    assert_eq!(price.dimensions, Dimensions::new(640, 480));
    assert_eq!(price.tokens, 206);
    assert!(!price.dimensions.is_empty());
}
